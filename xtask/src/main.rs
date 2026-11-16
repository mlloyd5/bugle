//! `cargo xtask` — Bugle's build tasks.
//!
//! * `bundle` builds the release binaries and assembles `dist/Bugle.app`:
//!   Info.plist, icon, the `bugle-app` executable and the `bugle` CLI, signed
//!   with the hardened runtime (ad-hoc unless an identity is given), verified,
//!   and optionally notarized and zipped. Everything happens in a staging
//!   directory; the finished bundle replaces `dist/Bugle.app` only once it
//!   has passed verification (and notarization, when asked for).
//!   `--universal` builds arm64 and x86_64 and merges them with `lipo`;
//!   `--dmg` also writes a compressed disk image.
//! * `dmg` is `bundle --dmg`.
//! * `verify` re-runs the checks on an existing bundle.
//! * `icon` writes the `.icns` and a 1024 px PNG preview.

mod icns;
mod plist;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const APP_NAME: &str = "Bugle";
const APP_EXECUTABLE: &str = "bugle-app";
const CLI_EXECUTABLE: &str = "bugle";
const ICON_NAME: &str = "Bugle";
const ENTITLEMENT: &str = "com.apple.security.device.audio-input";
/// Matches `-mmacosx-version-min` in `crates/bugle-platform/build.rs`.
const MIN_MACOS: &str = "13.0";
/// The workspace version (`[workspace.package] version`).
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "cargo xtask", about = "Bugle build tasks")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build release binaries and assemble, sign and verify Bugle.app.
    Bundle(BundleArgs),
    /// `bundle --dmg`: build the bundle, then Bugle-<version>.dmg (the app
    /// and an /Applications link, compressed), signed and verified.
    Dmg(BundleArgs),
    /// Check an existing bundle (signatures, entitlements, Info.plist, icon, binaries).
    Verify {
        /// The bundle to check (default dist/Bugle.app).
        app: Option<PathBuf>,
        /// Also launch the bundled app with `--smoke-test` (shows UI for a few seconds).
        #[arg(long)]
        smoke_test: bool,
    },
    /// Write Bugle.icns and a 1024 px PNG of the app icon.
    Icon {
        /// Output directory (default dist/; relative paths are relative to
        /// the current directory).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Args)]
struct BundleArgs {
    /// Code-signing identity: "-" for ad-hoc, or a Developer ID Application
    /// identity (name or SHA-1) from the keychain.
    #[arg(long, env = "BUGLE_SIGN_IDENTITY", default_value = "-")]
    sign: String,
    /// CFBundleIdentifier (default: `[workspace.metadata.bugle] bundle_id`
    /// in the root Cargo.toml).
    #[arg(long, env = "BUGLE_BUNDLE_ID")]
    bundle_id: Option<String>,
    /// CFBundleVersion (default: the workspace version).
    #[arg(long, env = "BUGLE_BUILD_NUMBER")]
    build_number: Option<String>,
    /// Output directory (default dist/; relative paths are relative to the
    /// current directory). Only `Bugle.app`, `Bugle-<version>.zip`,
    /// `Bugle-<version>.dmg` and the `.staging` directory inside it are ever
    /// replaced.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Also write Bugle-<version>.zip.
    #[arg(long)]
    zip: bool,
    /// Also write Bugle-<version>.dmg: the app and an /Applications link on
    /// a compressed (UDZO) image. Signed with a Developer ID identity and
    /// notarized with --notarize; checked with `hdiutil verify` and by
    /// mounting it.
    #[arg(long)]
    dmg: bool,
    /// Build for arm64 and x86_64 and merge each executable into a universal
    /// binary with `lipo`. Both Rust targets must be installed
    /// (`rustup target add aarch64-apple-darwin x86_64-apple-darwin`).
    #[arg(long)]
    universal: bool,
    /// Submit to Apple's notary service, staple the ticket and check it with
    /// Gatekeeper. Needs a Developer ID identity plus either
    /// BUGLE_NOTARY_PROFILE (a `notarytool store-credentials` keychain
    /// profile) or an App Store Connect API key: BUGLE_NOTARY_KEY (path to
    /// the .p8 file), BUGLE_NOTARY_KEY_ID and BUGLE_NOTARY_ISSUER. Fails if
    /// any of that is missing.
    #[arg(long)]
    notarize: bool,
    /// Launch the bundled app with `--smoke-test` after verifying.
    #[arg(long)]
    smoke_test: bool,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Bundle(args) => bundle(&args),
        Cmd::Dmg(args) => bundle(&BundleArgs { dmg: true, ..args }),
        Cmd::Verify { app, smoke_test } => {
            let ws = Workspace::load()?;
            let app = app.unwrap_or_else(|| default_dist().join(format!("{APP_NAME}.app")));
            verify(&app, &ws, None)?;
            if smoke_test {
                smoke(&app)?;
            }
            Ok(())
        }
        Cmd::Icon { out } => {
            let out = out.map(std::path::absolute).transpose()?.unwrap_or_else(default_dist);
            std::fs::create_dir_all(&out)?;
            std::fs::write(out.join(format!("{ICON_NAME}.icns")), icns::icns()?)?;
            std::fs::write(out.join(format!("{ICON_NAME}-1024.png")), icns::png(&icns::render(1024))?)?;
            println!("wrote {}/{ICON_NAME}.icns and {ICON_NAME}-1024.png", out.display());
            Ok(())
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask lives in the workspace").to_path_buf()
}

fn default_dist() -> PathBuf {
    workspace_root().join("dist")
}

fn entitlements_file() -> PathBuf {
    workspace_root().join("packaging").join(format!("{APP_NAME}.entitlements"))
}

fn cargo() -> Command {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(workspace_root());
    cmd
}

/// What `cargo metadata` says about the workspace: the real target directory
/// (honouring `CARGO_TARGET_DIR` and `build.target-dir`) and the bundle id
/// from `[workspace.metadata.bugle]`.
struct Workspace {
    target_dir: PathBuf,
    bundle_id: String,
}

impl Workspace {
    fn load() -> Result<Self> {
        let out = cargo()
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .stderr(Stdio::inherit())
            .output()
            .context("running cargo metadata")?;
        if !out.status.success() {
            bail!("cargo metadata failed ({})", out.status);
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("parsing cargo metadata")?;
        let target_dir = v["target_directory"].as_str().ok_or_else(|| anyhow!("cargo metadata: no target_directory"))?;
        let bundle_id = v["metadata"]["bugle"]["bundle_id"]
            .as_str()
            .ok_or_else(|| anyhow!("[workspace.metadata.bugle] bundle_id is missing from Cargo.toml"))?;
        Ok(Self { target_dir: target_dir.into(), bundle_id: bundle_id.into() })
    }

    /// Scratch space for verification output.
    fn scratch(&self, name: &str) -> Result<PathBuf> {
        let dir = self.target_dir.join("xtask").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

/// A reverse-DNS identifier: at least two dot-separated components of ASCII
/// letters, digits and hyphens.
fn validate_bundle_id(id: &str) -> Result<()> {
    let parts: Vec<&str> = id.split('.').collect();
    let ok = parts.len() >= 2
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    if !ok {
        bail!("invalid bundle id {id:?}: use dot-separated components of letters, digits and hyphens (e.g. com.example.bugle)");
    }
    Ok(())
}

fn bundle(args: &BundleArgs) -> Result<()> {
    let ws = Workspace::load()?;
    let bundle_id = args.bundle_id.clone().unwrap_or_else(|| ws.bundle_id.clone());
    validate_bundle_id(&bundle_id)?;
    let adhoc = args.sign == "-";
    // An explicit --notarize that cannot run is an error, not a silent skip.
    let notary = if args.notarize {
        if adhoc {
            bail!("--notarize needs a Developer ID identity (--sign / BUGLE_SIGN_IDENTITY)");
        }
        Some(notary_credentials()?)
    } else {
        None
    };

    let archs = Archs::new(args.universal);
    archs.check_installed(target_installed)?;
    let exes = build(&archs, &ws)?;

    // Assemble in a staging directory next to the final bundle.
    let out = args.out.clone().map(std::path::absolute).transpose()?.unwrap_or_else(default_dist);
    let staging = out.join(".staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging).with_context(|| format!("removing {}", staging.display()))?;
    }
    let app = staging.join(format!("{APP_NAME}.app"));
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    std::fs::create_dir_all(&macos)?;
    std::fs::create_dir_all(&resources)?;
    for exe in [APP_EXECUTABLE, CLI_EXECUTABLE] {
        std::fs::copy(&exes[exe], macos.join(exe)).with_context(|| format!("copying {}", exes[exe].display()))?;
    }
    let build_number = args.build_number.as_deref().unwrap_or(VERSION);
    let info = plist::info_plist(&plist::BundleInfo {
        bundle_id: &bundle_id,
        version: VERSION,
        build_number,
        executable: APP_EXECUTABLE,
        icon_file: ICON_NAME,
        min_macos: MIN_MACOS,
    });
    std::fs::write(contents.join("Info.plist"), info)?;
    std::fs::write(contents.join("PkgInfo"), "APPL????")?;
    std::fs::write(resources.join(format!("{ICON_NAME}.icns")), icns::icns()?)?;
    println!("assembled {}", app.display());

    // Sign inside-out: the nested CLI first, then the bundle (which seals the
    // main executable, Info.plist and resources).
    codesign(&macos.join(CLI_EXECUTABLE), &args.sign, Some(&format!("{bundle_id}.cli")))?;
    codesign(&app, &args.sign, None)?;
    verify(&app, &ws, archs.expected())?;

    if let Some(creds) = &notary {
        let upload = staging.join(format!("{APP_NAME}-upload.zip"));
        make_zip(&app, &upload)?;
        notarize(&upload, creds)?;
        run(Command::new("xcrun").args(["stapler", "staple"]).arg(&app))?;
        run(Command::new("xcrun").args(["stapler", "validate"]).arg(&app))?;
        run(Command::new("spctl").args(["--assess", "--type", "execute", "-vv"]).arg(&app))
            .context("Gatekeeper rejected the notarized bundle")?;
    } else if !adhoc {
        println!("not notarized; Gatekeeper will reject this bundle on other Macs (see --notarize)");
    }

    // Only now replace the previous bundle.
    let final_app = out.join(format!("{APP_NAME}.app"));
    if final_app.exists() {
        std::fs::remove_dir_all(&final_app).with_context(|| format!("removing old {}", final_app.display()))?;
    }
    std::fs::rename(&app, &final_app).with_context(|| format!("moving the bundle to {}", final_app.display()))?;

    let zip = out.join(format!("{APP_NAME}-{VERSION}.zip"));
    let _ = std::fs::remove_file(&zip);
    if args.zip || args.notarize {
        make_zip(&final_app, &zip)?;
    }

    // The disk image is made from the finished (stapled) bundle, signed and
    // notarized itself, verified, and only then moved next to it.
    let dmg = out.join(dmg_name());
    let _ = std::fs::remove_file(&dmg);
    if args.dmg {
        let staged = staging.join(dmg_name());
        make_dmg(&final_app, &staged, &ws)?;
        if !adhoc {
            codesign_dmg(&staged, &args.sign)?;
        }
        if let Some(creds) = &notary {
            notarize(&staged, creds)?;
            run(Command::new("xcrun").args(["stapler", "staple"]).arg(&staged))?;
            run(Command::new("spctl")
                .args(["--assess", "--type", "open", "--context", "context:primary-signature", "-vv"])
                .arg(&staged))
            .context("Gatekeeper rejected the notarized disk image")?;
        }
        verify_dmg(&staged, &ws, archs.expected(), !adhoc)?;
        std::fs::rename(&staged, &dmg).with_context(|| format!("moving the disk image to {}", dmg.display()))?;
    }
    std::fs::remove_dir_all(&staging)?;
    if args.smoke_test {
        smoke(&final_app)?;
    }

    let signature = match (adhoc, notary.is_some()) {
        (true, _) => "ad-hoc signature",
        (false, false) => "Developer ID signature, not notarized",
        (false, true) => "Developer ID signature, notarized and stapled",
    };
    println!("\n{APP_NAME}.app {VERSION} ({build_number}), {bundle_id}, {}, {signature}", archs.describe());
    println!("  app: {}", final_app.display());
    if zip.exists() {
        println!("  zip: {}", zip.display());
    }
    if dmg.exists() {
        println!("  dmg: {}", dmg.display());
    }
    Ok(())
}

/// The architectures a bundle is built for: the host's (plain `cargo build`,
/// reusing `target/release`) or arm64 + x86_64 merged with `lipo`.
#[derive(Debug, PartialEq)]
enum Archs {
    Host,
    Universal,
}

/// Rust target triple and the matching `lipo` architecture name.
const UNIVERSAL_TARGETS: [(&str, &str); 2] = [("aarch64-apple-darwin", "arm64"), ("x86_64-apple-darwin", "x86_64")];

impl Archs {
    fn new(universal: bool) -> Self {
        if universal {
            Self::Universal
        } else {
            Self::Host
        }
    }

    /// One cargo build per entry; `None` builds for the host.
    fn targets(&self) -> Vec<Option<&'static str>> {
        match self {
            Self::Host => vec![None],
            Self::Universal => UNIVERSAL_TARGETS.iter().map(|(t, _)| Some(*t)).collect(),
        }
    }

    /// What `lipo -archs` must report for every executable, when it matters.
    fn expected(&self) -> Option<&'static [&'static str]> {
        const UNIVERSAL: [&str; 2] = [UNIVERSAL_TARGETS[0].1, UNIVERSAL_TARGETS[1].1];
        match self {
            Self::Host => None,
            Self::Universal => Some(&UNIVERSAL),
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Self::Host => std::env::consts::ARCH,
            Self::Universal => "universal (arm64 + x86_64)",
        }
    }

    /// Fails up front, naming the command to run, when a Rust target is not
    /// installed, rather than letting cargo fail halfway through.
    fn check_installed(&self, installed: impl Fn(&str) -> bool) -> Result<()> {
        let missing: Vec<&str> = self.targets().into_iter().flatten().filter(|t| !installed(t)).collect();
        if missing.is_empty() {
            return Ok(());
        }
        bail!(
            "--universal needs the Rust standard library for {list}, which this toolchain does not have.\n\
             Install it with:  rustup target add {list}\n\
             (a toolchain not managed by rustup, such as Homebrew's `rust` formula, cannot add targets; use rustup)",
            list = missing.join(" ")
        )
    }
}

/// The rustc cargo will use (`RUSTC`, else `rustc` on PATH, which honours a
/// rustup override in the workspace).
fn rustc() -> Command {
    let mut cmd = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()));
    cmd.current_dir(workspace_root());
    cmd
}

/// Whether the toolchain has the standard library for `target`.
fn target_installed(target: &str) -> bool {
    let Ok(out) = rustc().args(["--print", "target-libdir", "--target", target]).output() else { return false };
    if !out.status.success() {
        return false;
    }
    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().any(|e| e.file_name().to_string_lossy().starts_with("libstd-")))
        .unwrap_or(false)
}

/// The `cargo build` arguments for one target (`None`: the host).
fn cargo_build_args(target: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> =
        ["build", "--release", "--locked", "--message-format=json-render-diagnostics", "-p", "bugle-app", "-p", "bugle-cli"]
            .map(String::from)
            .into();
    if let Some(t) = target {
        args.extend(["--target".into(), t.into()]);
    }
    args
}

/// Builds the executables for every architecture and returns one path per
/// executable: cargo's own output for a host build, a `lipo`-merged binary
/// (under `<target dir>/xtask/universal`) for a universal one.
fn build(archs: &Archs, ws: &Workspace) -> Result<HashMap<&'static str, PathBuf>> {
    let mut per_target = Vec::new();
    for target in archs.targets() {
        per_target.push(build_release(target)?);
    }
    let Some(expected) = archs.expected() else {
        return Ok(per_target.swap_remove(0));
    };
    let dir = ws.scratch("universal")?;
    let mut merged = HashMap::new();
    for exe in [APP_EXECUTABLE, CLI_EXECUTABLE] {
        let inputs: Vec<&Path> = per_target.iter().map(|m| m[exe].as_path()).collect();
        let output = dir.join(exe);
        lipo_create(&inputs, &output)?;
        check_archs(&output, expected)?;
        merged.insert(exe, output);
    }
    Ok(merged)
}

/// Builds both binaries for one target (`None`: the host) and returns their
/// paths as cargo reports them, so a custom target directory or
/// `build.target` is honoured and nothing stale is picked up.
fn build_release(target: Option<&str>) -> Result<HashMap<&'static str, PathBuf>> {
    let args = cargo_build_args(target);
    let mut cmd = cargo();
    cmd.args(&args);
    if target.is_some() {
        // A cross build would otherwise get rustc's per-target default
        // minimum (10.12 for x86_64), below the shim's.
        cmd.env("MACOSX_DEPLOYMENT_TARGET", MIN_MACOS);
    }
    let shown: Vec<&str> = args.iter().map(String::as_str).filter(|a| !a.starts_with("--message-format")).collect();
    println!("$ cargo {}", shown.join(" "));
    let out = cmd.stderr(Stdio::inherit()).output().context("running cargo build")?;
    if !out.status.success() {
        bail!("cargo build failed ({})", out.status);
    }
    let mut exes = HashMap::new();
    for line in out.stdout.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { continue };
        if v["reason"] != "compiler-artifact" {
            continue;
        }
        let (Some(name), Some(exe)) = (v["target"]["name"].as_str(), v["executable"].as_str()) else { continue };
        for want in [APP_EXECUTABLE, CLI_EXECUTABLE] {
            if name == want {
                exes.insert(want, PathBuf::from(exe));
            }
        }
    }
    for want in [APP_EXECUTABLE, CLI_EXECUTABLE] {
        if !exes.contains_key(want) {
            bail!("cargo build did not report the {want} executable");
        }
    }
    Ok(exes)
}

fn lipo_create(inputs: &[&Path], output: &Path) -> Result<()> {
    let _ = std::fs::remove_file(output);
    run(Command::new("lipo").arg("-create").args(inputs).arg("-output").arg(output))
}

/// The architectures in a Mach-O file, from `lipo -archs`, sorted.
fn lipo_archs(path: &Path) -> Result<Vec<String>> {
    let out = Command::new("lipo").arg("-archs").arg(path).output().context("running lipo")?;
    if !out.status.success() {
        bail!("lipo -archs {} failed: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    let mut archs: Vec<String> = String::from_utf8_lossy(&out.stdout).split_whitespace().map(String::from).collect();
    archs.sort();
    Ok(archs)
}

/// The architectures of `path`; with `expected`, fails unless they are
/// exactly those.
fn check_archs(path: &Path, expected: &[&str]) -> Result<Vec<String>> {
    let archs = lipo_archs(path)?;
    let mut want = expected.to_vec();
    want.sort();
    if !want.is_empty() && archs != want {
        bail!("{} has architectures {archs:?}, expected {want:?}", path.display());
    }
    Ok(archs)
}

fn dmg_name() -> String {
    format!("{APP_NAME}-{VERSION}.dmg")
}

/// `hdiutil create` arguments for a compressed, read-only image of `folder`.
fn dmg_create_args(folder: &Path, volume: &str, dmg: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = ["create", "-volname", volume, "-fs", "HFS+", "-format", "UDZO", "-imagekey", "zlib-level=9", "-ov", "-srcfolder"]
        .map(Into::into)
        .into();
    args.push(folder.into());
    args.push(dmg.into());
    args
}

/// Writes a disk image holding a copy of `app` and an `Applications` link
/// to /Applications, so it can be dragged across.
fn make_dmg(app: &Path, dmg: &Path, ws: &Workspace) -> Result<()> {
    let root = ws.scratch("dmg-root")?;
    // ditto keeps the signature, extended attributes and the stapled ticket.
    run(Command::new("ditto").arg(app).arg(root.join(format!("{APP_NAME}.app"))))?;
    std::os::unix::fs::symlink("/Applications", root.join("Applications")).context("linking /Applications")?;
    let _ = std::fs::remove_file(dmg);
    run(Command::new("hdiutil").args(dmg_create_args(&root, &format!("{APP_NAME} {VERSION}"), dmg)))?;
    std::fs::remove_dir_all(&root)?;
    Ok(())
}

/// A disk image takes a plain signature (no runtime or entitlements).
fn codesign_dmg(dmg: &Path, identity: &str) -> Result<()> {
    run(Command::new("codesign").args(["--force", "--sign", identity, "--timestamp"]).arg(dmg))
}

/// Checks the image (`hdiutil verify`), then mounts it read-only and checks
/// what is on it: the bundle (signature and architectures) and the
/// /Applications link.
fn verify_dmg(dmg: &Path, ws: &Workspace, archs: Option<&[&str]>, signed: bool) -> Result<()> {
    run(Command::new("hdiutil").arg("verify").arg(dmg))?;
    if signed {
        run(Command::new("codesign").args(["--verify", "--strict", "--verbose=2"]).arg(dmg))?;
    }
    let mount = ws.scratch("dmg-mount")?;
    run(Command::new("hdiutil")
        .args(["attach", "-readonly", "-nobrowse", "-noautoopen", "-mountpoint"])
        .arg(&mount)
        .arg(dmg))?;
    let checked = check_dmg_contents(&mount, archs);
    let detached = run(Command::new("hdiutil").arg("detach").arg(&mount).stdout(Stdio::null()))
        .or_else(|_| run(Command::new("hdiutil").args(["detach", "-force"]).arg(&mount)));
    checked?;
    detached?;
    println!("verified {}", dmg.display());
    Ok(())
}

fn check_dmg_contents(mount: &Path, archs: Option<&[&str]>) -> Result<()> {
    let mut names: Vec<String> = std::fs::read_dir(mount)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    println!("  dmg contents: {}", names.join(", "));
    let app = mount.join(format!("{APP_NAME}.app"));
    if !app.is_dir() {
        bail!("the disk image has no {APP_NAME}.app");
    }
    let link = std::fs::read_link(mount.join("Applications")).context("the disk image has no Applications link")?;
    if link != Path::new("/Applications") {
        bail!("Applications on the disk image points to {}", link.display());
    }
    run(Command::new("codesign").args(["--verify", "--deep", "--strict", "--verbose=2"]).arg(&app))?;
    for exe in [APP_EXECUTABLE, CLI_EXECUTABLE] {
        let found = check_archs(&app.join("Contents").join("MacOS").join(exe), archs.unwrap_or(&[]))?;
        println!("  dmg {exe}: {}", found.join(" "));
    }
    Ok(())
}

fn codesign(path: &Path, identity: &str, identifier: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("codesign");
    cmd.args(["--force", "--sign", identity, "--options", "runtime", "--entitlements"]).arg(entitlements_file());
    // Secure timestamps need Apple's server and a real identity.
    cmd.arg(if identity == "-" { "--timestamp=none" } else { "--timestamp" });
    if let Some(id) = identifier {
        cmd.args(["--identifier", id]);
    }
    run(cmd.arg(path))
}

/// The checks `bundle` runs after signing. `archs`, when given, is what
/// every executable must contain (a universal build); otherwise the
/// architectures are only printed.
fn verify(app: &Path, ws: &Workspace, archs: Option<&[&str]>) -> Result<()> {
    if !app.is_dir() {
        bail!("{} not found; run `cargo xtask bundle` first", app.display());
    }
    let contents = app.join("Contents");
    run(Command::new("plutil").arg("-lint").arg(contents.join("Info.plist")))?;
    run(Command::new("codesign").args(["--verify", "--deep", "--strict", "--verbose=2"]).arg(app))?;

    // Each signed executable: hardened runtime, and the entitlements actually
    // embedded in its signature are valid and include audio-input.
    let scratch = ws.scratch("verify")?;
    for (name, target) in [(APP_EXECUTABLE, app.to_path_buf()), (CLI_EXECUTABLE, contents.join("MacOS").join(CLI_EXECUTABLE))] {
        let out = Command::new("codesign").args(["--display", "--verbose=2"]).arg(&target).output()?;
        let text = String::from_utf8_lossy(&out.stderr);
        let flags = text.lines().find(|l| l.contains("flags=")).unwrap_or("").trim();
        let ident = text.lines().find(|l| l.starts_with("Identifier=")).unwrap_or("").trim();
        let found = check_archs(&contents.join("MacOS").join(name), archs.unwrap_or(&[]))?;
        println!("  {name}: {ident}, {flags}, {}", found.join(" "));
        if !flags.contains("runtime") {
            bail!("{} is not signed with the hardened runtime", target.display());
        }
        let ents = Command::new("codesign").args(["--display", "--entitlements", "-", "--xml"]).arg(&target).output()?;
        let file = scratch.join(format!("{name}.entitlements"));
        std::fs::write(&file, &ents.stdout)?;
        run(Command::new("plutil").arg("-lint").arg(&file))?;
        if !String::from_utf8_lossy(&ents.stdout).contains(ENTITLEMENT) {
            bail!("{} lacks the {ENTITLEMENT} entitlement", target.display());
        }
    }

    // The icon must round-trip through iconutil.
    let iconset = scratch.join("icon.iconset");
    run(Command::new("iconutil")
        .args(["--convert", "iconset", "--output"])
        .arg(&iconset)
        .arg(contents.join("Resources").join(format!("{ICON_NAME}.icns"))))?;
    let n = std::fs::read_dir(&iconset)?.count();
    println!("  icon: {n} images");
    if n < icns::ICNS_ELEMENTS.len() {
        bail!("icon has {n} images, expected {}", icns::ICNS_ELEMENTS.len());
    }

    for exe in [APP_EXECUTABLE, CLI_EXECUTABLE] {
        let out = Command::new(contents.join("MacOS").join(exe)).arg("--version").output()?;
        let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
        println!("  {exe} --version: {line}");
        if !out.status.success() || !line.ends_with(VERSION) {
            bail!("{exe} --version printed {line:?}, expected version {VERSION}");
        }
    }
    println!("verified {}", app.display());
    Ok(())
}

/// Launches the bundled app with `--smoke-test` (overlay, toast and pin for a
/// few seconds, then it quits by itself) and fails if it errors or hangs.
fn smoke(app: &Path) -> Result<()> {
    let exe = app.join("Contents").join("MacOS").join(APP_EXECUTABLE);
    println!("$ {} --smoke-test", exe.display());
    let mut child = Command::new(&exe).arg("--smoke-test").spawn().context("launching the bundled app")?;
    let t0 = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("smoke test exited with {status}");
            }
            println!("smoke test passed in {:.1}s", t0.elapsed().as_secs_f32());
            return Ok(());
        }
        if t0.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("smoke test did not finish within 60s");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn make_zip(app: &Path, zip: &Path) -> Result<()> {
    let _ = std::fs::remove_file(zip);
    run(Command::new("ditto").args(["-c", "-k", "--sequesterRsrc", "--keepParent"]).arg(app).arg(zip))
}

/// Notary credentials that never put a secret on the command line: a keychain
/// profile, or an API key whose secret is a file.
#[derive(Debug, PartialEq)]
enum NotaryCredentials {
    Profile(String),
    ApiKey { key: PathBuf, key_id: String, issuer: String },
}

fn notary_credentials() -> Result<NotaryCredentials> {
    notary_credentials_from(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
}

fn notary_credentials_from(var: impl Fn(&str) -> Option<String>) -> Result<NotaryCredentials> {
    if let Some(p) = var("BUGLE_NOTARY_PROFILE") {
        return Ok(NotaryCredentials::Profile(p));
    }
    match (var("BUGLE_NOTARY_KEY"), var("BUGLE_NOTARY_KEY_ID"), var("BUGLE_NOTARY_ISSUER")) {
        (Some(key), Some(key_id), Some(issuer)) => Ok(NotaryCredentials::ApiKey { key: key.into(), key_id, issuer }),
        (None, None, None) => bail!(
            "--notarize needs BUGLE_NOTARY_PROFILE, or BUGLE_NOTARY_KEY, BUGLE_NOTARY_KEY_ID and BUGLE_NOTARY_ISSUER"
        ),
        _ => bail!("--notarize: set all of BUGLE_NOTARY_KEY, BUGLE_NOTARY_KEY_ID and BUGLE_NOTARY_ISSUER"),
    }
}

fn notarize(zip: &Path, creds: &NotaryCredentials) -> Result<()> {
    let mut cmd = Command::new("xcrun");
    cmd.args(["notarytool", "submit"]).arg(zip).arg("--wait");
    // Identifiers are masked so they stay out of CI logs.
    match creds {
        NotaryCredentials::Profile(p) => {
            println!("$ xcrun notarytool submit {} --wait --keychain-profile ***", zip.display());
            cmd.args(["--keychain-profile", p]);
        }
        NotaryCredentials::ApiKey { key, key_id, issuer } => {
            if !key.is_file() {
                bail!("BUGLE_NOTARY_KEY does not name a file");
            }
            println!("$ xcrun notarytool submit {} --wait --key *** --key-id *** --issuer ***", zip.display());
            cmd.arg("--key").arg(key).args(["--key-id", key_id, "--issuer", issuer]);
        }
    }
    let status = cmd.status().context("running xcrun notarytool")?;
    if !status.success() {
        bail!("notarization failed ({status}); `xcrun notarytool log <id>` shows why");
    }
    Ok(())
}

/// Runs a command, echoing it first, and fails on a non-zero exit.
fn run(cmd: &mut Command) -> Result<()> {
    let shown: Vec<String> = std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|s| {
            let s = s.to_string_lossy();
            if s.contains(' ') {
                format!("{s:?}")
            } else {
                s.into_owned()
            }
        })
        .collect();
    println!("$ {}", shown.join(" "));
    let status = cmd.status().with_context(|| format!("running {}", shown[0]))?;
    if !status.success() {
        bail!("{} failed ({status})", shown[0]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_ids() {
        for ok in ["com.example.bugle", "io.github.some-user.bugle", "a.b"] {
            validate_bundle_id(ok).unwrap();
        }
        for bad in ["", "bugle", "com..bugle", ".com.bugle", "com.bugle.", "com.my app", "com.bugle_app", "com.büg"] {
            assert!(validate_bundle_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn workspace_bundle_id_is_valid() {
        let ws = Workspace::load().unwrap();
        validate_bundle_id(&ws.bundle_id).unwrap();
        assert!(ws.target_dir.is_absolute());
    }

    fn parse(args: &[&str]) -> Cmd {
        Cli::try_parse_from(std::iter::once("cargo xtask").chain(args.iter().copied())).unwrap().cmd
    }

    #[test]
    fn bundle_flags_parse() {
        let Cmd::Bundle(a) = parse(&["bundle"]) else { panic!() };
        assert!(!a.universal && !a.dmg && !a.zip);
        let Cmd::Bundle(a) = parse(&["bundle", "--universal", "--dmg", "--out", "x"]) else { panic!() };
        assert!(a.universal && a.dmg);
        assert_eq!(a.out.as_deref(), Some(Path::new("x")));
        // `dmg` takes the bundle flags; main() forces dmg on.
        let Cmd::Dmg(a) = parse(&["dmg", "--universal"]) else { panic!() };
        assert!(a.universal);
        assert!(Cli::try_parse_from(["cargo xtask", "bundle", "--universal=yes"]).is_err());
    }

    #[test]
    fn build_plan() {
        assert_eq!(Archs::new(false).targets(), vec![None]);
        assert_eq!(Archs::new(false).expected(), None);
        assert_eq!(Archs::new(true).targets(), vec![Some("aarch64-apple-darwin"), Some("x86_64-apple-darwin")]);
        assert_eq!(Archs::new(true).expected(), Some(&["arm64", "x86_64"][..]));

        let host = cargo_build_args(None);
        assert!(!host.iter().any(|a| a == "--target"));
        assert!(host.iter().any(|a| a == "--locked") && host.iter().any(|a| a == "bugle-app") && host.iter().any(|a| a == "bugle-cli"));
        let x86 = cargo_build_args(Some("x86_64-apple-darwin"));
        assert_eq!(&x86[x86.len() - 2..], ["--target", "x86_64-apple-darwin"]);
    }

    #[test]
    fn missing_targets_name_the_rustup_command() {
        Archs::new(false).check_installed(|_| false).unwrap();
        Archs::new(true).check_installed(|_| true).unwrap();
        let e = Archs::new(true).check_installed(|t| t.starts_with("aarch64")).unwrap_err().to_string();
        assert!(e.contains("rustup target add x86_64-apple-darwin"), "{e}");
        assert!(!e.contains("add aarch64"), "{e}");
        let e = Archs::new(true).check_installed(|_| false).unwrap_err().to_string();
        assert!(e.contains("rustup target add aarch64-apple-darwin x86_64-apple-darwin"), "{e}");
    }

    #[test]
    fn host_target_is_installed() {
        let host = if cfg!(target_arch = "aarch64") { "aarch64-apple-darwin" } else { "x86_64-apple-darwin" };
        assert!(target_installed(host));
        assert!(!target_installed("no-such-target"));
    }

    #[test]
    fn dmg_plan() {
        assert_eq!(dmg_name(), format!("Bugle-{VERSION}.dmg"));
        let args = dmg_create_args(Path::new("/r"), "Bugle 1.0", Path::new("/o/Bugle.dmg"));
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args[0], "create");
        assert!(args.windows(2).any(|w| w == ["-format", "UDZO"]));
        assert!(args.windows(2).any(|w| w == ["-volname", "Bugle 1.0"]));
        assert_eq!(&args[args.len() - 3..], ["-srcfolder", "/r", "/o/Bugle.dmg"]);
    }

    /// `lipo` merging and the architecture check, on two tiny C programs
    /// (clang cross-compiles both slices without a Rust target).
    #[test]
    fn lipo_merges_and_checks_archs() {
        let ws = Workspace::load().unwrap();
        let dir = ws.scratch("test-lipo").unwrap();
        let src = dir.join("main.c");
        std::fs::write(&src, "int main(void) { return 0; }\n").unwrap();
        let mut slices = Vec::new();
        for arch in ["arm64", "x86_64"] {
            let out = dir.join(arch);
            let ok = Command::new("cc").args(["-arch", arch, "-o"]).arg(&out).arg(&src).status().is_ok_and(|s| s.success());
            if !ok {
                eprintln!("skipping: cc cannot build {arch}");
                return;
            }
            slices.push(out);
        }
        let fat = dir.join("fat");
        lipo_create(&slices.iter().map(PathBuf::as_path).collect::<Vec<_>>(), &fat).unwrap();
        assert_eq!(check_archs(&fat, &["x86_64", "arm64"]).unwrap(), ["arm64", "x86_64"]);
        assert!(check_archs(&slices[0], &["arm64", "x86_64"]).is_err());
        assert_eq!(check_archs(&slices[0], &[]).unwrap(), ["arm64"]);
    }

    #[test]
    fn notary_credentials_need_a_complete_set() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(
            notary_credentials_from(env(&[("BUGLE_NOTARY_PROFILE", "p")])).unwrap(),
            NotaryCredentials::Profile("p".into())
        );
        assert!(matches!(
            notary_credentials_from(env(&[
                ("BUGLE_NOTARY_KEY", "/k.p8"),
                ("BUGLE_NOTARY_KEY_ID", "id"),
                ("BUGLE_NOTARY_ISSUER", "iss")
            ]))
            .unwrap(),
            NotaryCredentials::ApiKey { .. }
        ));
        assert!(notary_credentials_from(env(&[])).is_err());
        assert!(notary_credentials_from(env(&[("BUGLE_NOTARY_KEY_ID", "id")])).is_err());
    }
}
