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
//! * `check` runs clippy (warnings are errors) and the workspace tests.
//! * `smoke-vm` runs the bundle's `--smoke-test` in a throwaway Tart macOS
//!   VM instead of on the host; `smoke-vm --prepare` makes its golden image
//!   (see `smoke_vm.rs`).

mod icns;
mod plist;
mod smoke_vm;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::cell::OnceCell;
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
        /// Also launch the bundled app with `--smoke-test` and a throwaway
        /// config (killed after 180 s, or BUGLE_SMOKE_TIMEOUT seconds).
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
    /// Run `cargo clippy --workspace --all-targets -- -D warnings` and
    /// `cargo test --workspace`; fails if either does.
    Check,
    /// Build the bundle and run its `--smoke-test` in a throwaway Tart macOS
    /// VM (cloned from a golden image, deleted afterwards), so nothing shows
    /// on this Mac. `--prepare` makes the golden image once.
    SmokeVm(smoke_vm::SmokeVmArgs),
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
            let smoke_limit = smoke_test.then(smoke_timeout_from_env).transpose()?;
            verify(&app, &ws, None)?;
            if let Some(limit) = smoke_limit {
                smoke(&app, limit)?;
            }
            Ok(())
        }
        Cmd::Icon { out } => {
            let out = out.map(std::path::absolute).transpose()?.unwrap_or_else(default_dist);
            std::fs::create_dir_all(&out)?;
            std::fs::write(out.join(format!("{ICON_NAME}.icns")), icns::icns()?)?;
            std::fs::write(out.join(format!("{ICON_NAME}-1024.png")), icns::png(&icns::render(1024)?)?)?;
            println!("wrote {}/{ICON_NAME}.icns and {ICON_NAME}-1024.png", out.display());
            Ok(())
        }
        Cmd::Check => check(),
        Cmd::SmokeVm(args) => smoke_vm::smoke_vm(&args),
    }
}

/// Clippy, then the tests. The tests run even if clippy failed, including
/// when clippy stopped on a compile error, so one pass shows both; each
/// failure's error (a non-zero exit, or cargo not starting) is printed
/// before the summary.
fn check() -> Result<()> {
    let clippy = run(cargo().args(["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]));
    let test = run(cargo().args(["test", "--workspace"]));
    let mut failed = Vec::new();
    for (name, result) in [("clippy", clippy), ("tests", test)] {
        if let Err(e) = result {
            eprintln!("{name}: {e:#}");
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        bail!("check failed: {}", failed.join(" and "));
    }
    println!("check: clippy and tests passed");
    Ok(())
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

/// A cargo command run from the workspace root. `cargo run` gives xtask its
/// own package variables (CARGO_MANIFEST_DIR, CARGO_PKG_*, ...); they are
/// removed so build scripts that watch them (ring's) do not see a change and
/// rebuild every time xtask and plain cargo alternate.
fn cargo() -> Command {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(workspace_root());
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(is_package_env_var) {
            cmd.env_remove(&key);
        }
    }
    cmd
}

/// The per-package variables cargo sets for a binary it runs.
fn is_package_env_var(key: &str) -> bool {
    key.starts_with("CARGO_PKG_")
        || matches!(
            key,
            "CARGO_MANIFEST_DIR"
                | "CARGO_MANIFEST_PATH"
                | "CARGO_MANIFEST_LINKS"
                | "CARGO_CRATE_NAME"
                | "CARGO_BIN_NAME"
                | "CARGO_PRIMARY_PACKAGE"
        )
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
    // Before anything is built, so a bad BUGLE_SMOKE_TIMEOUT fails early.
    let smoke_limit = args.smoke_test.then(smoke_timeout_from_env).transpose()?;
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

    let final_app = out.join(format!("{APP_NAME}.app"));
    let zip = out.join(format!("{APP_NAME}-{VERSION}.zip"));
    let dmg = out.join(dmg_name());
    let staged_dmg = staging.join(dmg_name());
    let steps = release_steps(&ReleaseOptions {
        notarize: notary.is_some(),
        signed: !adhoc,
        zip: args.zip || args.notarize,
        dmg: args.dmg,
        smoke_test: args.smoke_test,
    });
    for step in steps {
        match step {
            Step::NotarizeApp => {
                let creds = notary.as_ref().expect("planned only with credentials");
                let upload = staging.join(format!("{APP_NAME}-upload.zip"));
                make_zip(&app, &upload)?;
                notarize(&upload, creds)?;
                run(Command::new("xcrun").args(["stapler", "staple"]).arg(&app))?;
                run(Command::new("xcrun").args(["stapler", "validate"]).arg(&app))?;
                run(Command::new("spctl").args(["--assess", "--type", "execute", "-vv"]).arg(&app))
                    .context("Gatekeeper rejected the notarized bundle")?;
            }
            // The image is made from the staged (stapled) bundle, so a
            // failure here leaves the previous bundle and image untouched.
            Step::MakeDmg => make_dmg(&app, &staged_dmg, &ws)?,
            Step::SignDmg => codesign_dmg(&staged_dmg, &args.sign)?,
            Step::NotarizeDmg => {
                let creds = notary.as_ref().expect("planned only with credentials");
                notarize(&staged_dmg, creds)?;
                run(Command::new("xcrun").args(["stapler", "staple"]).arg(&staged_dmg))?;
                run(Command::new("spctl")
                    .args(["--assess", "--type", "open", "--context", "context:primary-signature", "-vv"])
                    .arg(&staged_dmg))
                .context("Gatekeeper rejected the notarized disk image")?;
            }
            Step::VerifyDmg => verify_dmg(&staged_dmg, &ws, archs.expected(), !adhoc)?,
            Step::ReplaceApp => {
                if final_app.exists() {
                    std::fs::remove_dir_all(&final_app).with_context(|| format!("removing old {}", final_app.display()))?;
                }
                std::fs::rename(&app, &final_app).with_context(|| format!("moving the bundle to {}", final_app.display()))?;
            }
            // A rename over the old image: replaced, never removed first.
            Step::ReplaceDmg => {
                std::fs::rename(&staged_dmg, &dmg).with_context(|| format!("moving the disk image to {}", dmg.display()))?
            }
            // Without --dmg / --zip, an older file of this version would no
            // longer match the new bundle.
            Step::RemoveStaleDmg => {
                let _ = std::fs::remove_file(&dmg);
            }
            Step::RemoveStaleZip => {
                let _ = std::fs::remove_file(&zip);
            }
            Step::Zip => make_zip(&final_app, &zip)?,
            Step::Smoke => smoke(&final_app, smoke_limit.expect("planned only with --smoke-test"))?,
        }
    }
    std::fs::remove_dir_all(&staging)?;
    if !adhoc && notary.is_none() {
        println!("not notarized; Gatekeeper will reject this bundle on other Macs (see --notarize)");
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

/// What happens after the staged bundle is signed and verified.
struct ReleaseOptions {
    notarize: bool,
    /// A Developer ID identity (the image gets its own signature).
    signed: bool,
    zip: bool,
    dmg: bool,
    smoke_test: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    NotarizeApp,
    MakeDmg,
    SignDmg,
    NotarizeDmg,
    VerifyDmg,
    ReplaceApp,
    ReplaceDmg,
    RemoveStaleDmg,
    RemoveStaleZip,
    Zip,
    Smoke,
}

/// The order of the release steps. Everything that can fail on the staged
/// bundle and image (notarization, the image itself) runs before anything
/// in the output directory is touched, so a failure leaves the previous
/// bundle and image as they were.
fn release_steps(o: &ReleaseOptions) -> Vec<Step> {
    let mut steps = Vec::new();
    if o.notarize {
        steps.push(Step::NotarizeApp);
    }
    if o.dmg {
        steps.push(Step::MakeDmg);
        if o.signed {
            steps.push(Step::SignDmg);
        }
        if o.notarize {
            steps.push(Step::NotarizeDmg);
        }
        steps.push(Step::VerifyDmg);
    }
    steps.push(Step::ReplaceApp);
    steps.push(if o.dmg { Step::ReplaceDmg } else { Step::RemoveStaleDmg });
    steps.push(if o.zip { Step::Zip } else { Step::RemoveStaleZip });
    if o.smoke_test {
        steps.push(Step::Smoke);
    }
    steps
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
    // Host and cross builds alike: rustc's per-target default minimum
    // (11.0 for arm64, 10.12 for x86_64) is below the shim's, and both
    // modes should produce the same LC_BUILD_VERSION. Every xtask build sets
    // it, so consecutive runs still reuse target/release.
    cmd.env("MACOSX_DEPLOYMENT_TARGET", MIN_MACOS);
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
    let mount = ws.target_dir.join("xtask").join("dmg-mount");
    // An interrupted earlier run may have left its image attached here,
    // which would make the scratch cleanup and the attach fail.
    if mount.exists() {
        detach_quietly(&mount);
    }
    let mount = ws.scratch("dmg-mount")?;
    run(Command::new("hdiutil")
        .args(["attach", "-readonly", "-nobrowse", "-noautoopen", "-mountpoint"])
        .arg(&mount)
        .arg(dmg))?;
    let mut guard = Attached { mount, detached: false };
    check_dmg_contents(&guard.mount, archs)?;
    guard.detach()?;
    println!("verified {}", dmg.display());
    Ok(())
}

/// A mounted image, detached (forcibly) when dropped without [`detach`]
/// having succeeded, so an error or panic while checking it does not leave
/// it attached.
///
/// [`detach`]: Attached::detach
struct Attached {
    mount: PathBuf,
    detached: bool,
}

impl Attached {
    fn detach(&mut self) -> Result<()> {
        run(Command::new("hdiutil").arg("detach").arg(&self.mount).stdout(Stdio::null()))
            .or_else(|_| run(Command::new("hdiutil").args(["detach", "-force"]).arg(&self.mount).stdout(Stdio::null())))?;
        self.detached = true;
        Ok(())
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        if !self.detached {
            detach_quietly(&self.mount);
        }
    }
}

/// Best-effort `hdiutil detach -force`; silent when nothing is mounted.
fn detach_quietly(mount: &Path) {
    let _ = Command::new("hdiutil").args(["detach", "-force"]).arg(mount).stdout(Stdio::null()).stderr(Stdio::null()).status();
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

    // Every slice runs `--version`: the native one directly, an x86_64 one
    // on Apple silicon through Rosetta when it is installed.
    let rosetta = OnceCell::new();
    for exe in [APP_EXECUTABLE, CLI_EXECUTABLE] {
        let path = contents.join("MacOS").join(exe);
        check_version(exe, None, Command::new(&path).arg("--version"))?;
        for slice in foreign_slices(&lipo_archs(&path)?, host_lipo_arch()) {
            if slice == "x86_64" && *rosetta.get_or_init(rosetta_available) {
                check_version(exe, Some(&slice), Command::new("arch").args(["-x86_64"]).arg(&path).arg("--version"))?;
            } else {
                println!("  {exe} ({slice}): not executed, this Mac cannot run that slice (no Rosetta)");
            }
        }
    }
    println!("verified {}", app.display());
    Ok(())
}

fn check_version(exe: &str, slice: Option<&str>, cmd: &mut Command) -> Result<()> {
    let out = cmd.output().with_context(|| format!("running {exe} --version"))?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let label = slice.map_or_else(|| exe.to_string(), |s| format!("{exe} ({s})"));
    println!("  {label} --version: {line}");
    if !out.status.success() || !line.ends_with(VERSION) {
        bail!("{label} --version printed {line:?}, expected version {VERSION}");
    }
    Ok(())
}

/// The host's architecture as `lipo` names it.
fn host_lipo_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    }
}

/// The slices of a binary that the host does not run natively.
fn foreign_slices(archs: &[String], host: &str) -> Vec<String> {
    archs.iter().filter(|a| a.as_str() != host).cloned().collect()
}

/// Whether x86_64 code runs here (natively or through Rosetta 2).
fn rosetta_available() -> bool {
    Command::new("arch")
        .args(["-x86_64", "/usr/bin/true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// How long [`smoke`] waits for the app before killing it, unless
/// `BUGLE_SMOKE_TIMEOUT` (whole seconds) says otherwise. A run from a
/// release bundle took about 32 s, most of it the editors and the 2 s
/// recording.
const SMOKE_TIMEOUT_DEFAULT: Duration = Duration::from_secs(180);

/// The smoke-test time limit from `BUGLE_SMOKE_TIMEOUT` as
/// `std::env::var` reads it: unset or empty is the default, anything else
/// must be a whole number of seconds above 0.
fn smoke_timeout(var: Result<String, std::env::VarError>) -> Result<Duration> {
    let v = match var {
        Ok(v) => v,
        Err(std::env::VarError::NotPresent) => return Ok(SMOKE_TIMEOUT_DEFAULT),
        Err(std::env::VarError::NotUnicode(v)) => bail!("BUGLE_SMOKE_TIMEOUT={v:?}: not valid Unicode"),
    };
    let v = v.trim();
    if v.is_empty() {
        return Ok(SMOKE_TIMEOUT_DEFAULT);
    }
    match v.parse::<u64>() {
        Ok(s) if s > 0 => Ok(Duration::from_secs(s)),
        _ => bail!("BUGLE_SMOKE_TIMEOUT={v:?}: expected a whole number of seconds greater than 0"),
    }
}

/// [`smoke_timeout`] for this process, read before any build step so a bad
/// value fails early.
fn smoke_timeout_from_env() -> Result<Duration> {
    smoke_timeout(std::env::var("BUGLE_SMOKE_TIMEOUT"))
}

/// A TOML basic string holding `s`.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The named `[hotkeys]` keys; [`smoke_config`] clears every one.
const SMOKE_HOTKEYS: [&str; 8] =
    ["capture_area", "capture_screen", "capture_window", "scroll_capture", "record", "record_gif", "ocr_latest", "pin_latest"];

/// The throwaway config the smoke test runs with: saves and history go to
/// `dir`, sync (folder or S3) is off, clipboard copy is off, and no global
/// hotkey is registered, so the run neither reads nor touches the user's
/// own config, captures, history or sync destination, and leaves a running
/// Bugle its hotkeys. Everything else is the default. Fails on a path that
/// is not UTF-8 (TOML strings must be).
fn smoke_config(dir: &Path) -> Result<String> {
    let path = |name: &str| -> Result<String> {
        let p = dir.join(name);
        let s = p.to_str().ok_or_else(|| anyhow!("smoke-test path is not UTF-8: {}", p.display()))?;
        Ok(toml_string(s))
    };
    let mut s = String::from("# Written by `cargo xtask` for one `bugle-app --smoke-test` run.\n\n[general]\n");
    s += &format!("save_dir = {}\n", path("captures")?);
    s += &format!("history_path = {}\n", path("history.jsonl")?);
    s += "copy_to_clipboard = false\nshutter_sound = false\nsystem_notifications = false\n\n";
    s += "[sync]\nenabled = false\n\n[hotkeys]\n";
    for h in SMOKE_HOTKEYS {
        s += &format!("{h} = \"\"\n");
    }
    Ok(s)
}

/// A fresh directory for one smoke run, removed on drop unless
/// `BUGLE_SMOKE_KEEP` is set (the app then keeps its recording too).
struct SmokeDir(PathBuf);

impl SmokeDir {
    fn create() -> Result<Self> {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!("bugle-xtask-smoke-{}-{nanos}", std::process::id()));
        // Exclusive: fails if the directory already exists.
        std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let me = Self(dir);
        for sub in ["captures", "tmp"] {
            let d = me.0.join(sub);
            std::fs::create_dir(&d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(me)
    }
}

impl Drop for SmokeDir {
    fn drop(&mut self) {
        if std::env::var_os("BUGLE_SMOKE_KEEP").is_some() {
            println!("smoke test files kept in {}", self.0.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Launches the bundled app with `--smoke-test` (overlay, toast, pin, the
/// panels and editors, a 2 s recording; then it quits by itself) and fails
/// if it errors or does not finish within `limit`. The app gets a throwaway
/// config ([`smoke_config`]) and its own `TMPDIR`, both in a temp directory
/// removed afterwards, so the run never touches the user's config or data.
/// The app's own opt-ins pass through from the environment on purpose:
/// `BUGLE_SMOKE_CLIPBOARD=1` pastes through the real clipboard,
/// `BUGLE_SMOKE_MICROPHONE=1` records the microphone, `BUGLE_SMOKE_SAVE`
/// saves and exports (into the throwaway `save_dir`), `BUGLE_SMOKE_KEEP`
/// keeps the files.
fn smoke(app: &Path, limit: Duration) -> Result<()> {
    let exe = app.join("Contents").join("MacOS").join(APP_EXECUTABLE);
    let dir = SmokeDir::create()?;
    let config = dir.0.join("config.toml");
    std::fs::write(&config, smoke_config(&dir.0)?).with_context(|| format!("writing {}", config.display()))?;
    println!("$ {} --smoke-test --config {}  (limit {}s)", exe.display(), config.display(), limit.as_secs());
    let mut child = Command::new(&exe)
        .arg("--smoke-test")
        .arg("--config")
        .arg(&config)
        .env_remove("BUGLE_CONFIG")
        .env("TMPDIR", dir.0.join("tmp"))
        .spawn()
        .context("launching the bundled app")?;
    let t0 = Instant::now();
    loop {
        let status = match child.try_wait() {
            Ok(s) => s,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e).context("waiting for the smoke test");
            }
        };
        if let Some(status) = status {
            if !status.success() {
                bail!("smoke test exited with {status}");
            }
            println!("smoke test passed in {:.1}s", t0.elapsed().as_secs_f32());
            return Ok(());
        }
        if t0.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("smoke test did not finish within {}s (BUGLE_SMOKE_TIMEOUT sets the limit)", limit.as_secs());
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

    #[test]
    fn release_steps_touch_the_output_last() {
        let opts = |notarize, signed, zip, dmg| ReleaseOptions { notarize, signed, zip, dmg, smoke_test: false };
        use Step::*;
        assert_eq!(release_steps(&opts(false, false, false, false)), [ReplaceApp, RemoveStaleDmg, RemoveStaleZip]);
        assert_eq!(
            release_steps(&opts(false, false, false, true)),
            [MakeDmg, VerifyDmg, ReplaceApp, ReplaceDmg, RemoveStaleZip]
        );
        assert_eq!(
            release_steps(&opts(true, true, true, true)),
            [NotarizeApp, MakeDmg, SignDmg, NotarizeDmg, VerifyDmg, ReplaceApp, ReplaceDmg, Zip]
        );
        assert_eq!(release_steps(&ReleaseOptions { smoke_test: true, ..opts(false, true, false, true) }).last(), Some(&Smoke));
        // Nothing in the output directory changes before every check on the
        // staged bundle and image has passed, and the old image is never
        // deleted when a new one is being made.
        for notarize in [false, true] {
            for dmg in [false, true] {
                let steps = release_steps(&opts(notarize, notarize, true, dmg));
                let first_output = steps.iter().position(|s| *s == ReplaceApp).unwrap();
                assert!(steps[first_output..].iter().all(|s| !matches!(s, NotarizeApp | MakeDmg | SignDmg | NotarizeDmg | VerifyDmg)));
                assert_eq!(steps.contains(&RemoveStaleDmg), !dmg);
            }
        }
    }

    #[test]
    fn foreign_slices_are_the_non_host_ones() {
        let archs = vec!["arm64".to_string(), "x86_64".to_string()];
        assert_eq!(foreign_slices(&archs, "arm64"), ["x86_64"]);
        assert_eq!(foreign_slices(&archs, "x86_64"), ["arm64"]);
        assert!(foreign_slices(&archs[..1], "arm64").is_empty());
        assert_eq!(host_lipo_arch(), if cfg!(target_arch = "aarch64") { "arm64" } else { "x86_64" });
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

    #[test]
    fn smoke_timeouts() {
        use std::env::VarError;
        use std::os::unix::ffi::OsStringExt;
        let ok = |s: &str| smoke_timeout(Ok(s.to_string()));
        assert_eq!(smoke_timeout(Err(VarError::NotPresent)).unwrap(), SMOKE_TIMEOUT_DEFAULT);
        assert_eq!(ok("").unwrap(), SMOKE_TIMEOUT_DEFAULT);
        assert_eq!(ok(" 300 ").unwrap(), Duration::from_secs(300));
        for bad in ["0", "-5", "1.5", "2m", "abc"] {
            let e = ok(bad).unwrap_err().to_string();
            assert!(e.contains("BUGLE_SMOKE_TIMEOUT"), "{bad}: {e}");
        }
        let e = smoke_timeout(Err(VarError::NotUnicode(std::ffi::OsString::from_vec(vec![b'6', 0xff]))))
            .unwrap_err()
            .to_string();
        assert!(e.contains("not valid Unicode"), "{e}");
    }

    #[test]
    fn toml_strings() {
        let parse = |s: &str| toml::from_str::<toml::Table>(&format!("v = {}", toml_string(s))).unwrap()["v"].clone();
        for s in ["/tmp/a b", "C:\\x\\y", "quote \" here", "tab\tnew\nline", "\u{1}ctl", "ünï/çødé"] {
            assert_eq!(parse(s).as_str(), Some(s), "{s:?}");
        }
    }

    #[test]
    fn smoke_config_stays_in_its_directory() {
        use bugle_core::config::Config;
        use bugle_core::MediaKind;
        let dir = Path::new("/private/tmp/bugle smoke \"dir\"");
        let cfg: Config = toml::from_str(&smoke_config(dir).unwrap()).unwrap();
        assert_eq!(cfg.save_dir(), dir.join("captures"));
        assert_eq!(cfg.history_path(), dir.join("history.jsonl"));
        assert!(!cfg.sync.enabled);
        for kind in [MediaKind::Screenshot, MediaKind::Video, MediaKind::Gif] {
            assert!(!cfg.sync.applies_to(kind), "{kind:?} would sync");
        }
        assert!(!cfg.general.copy_to_clipboard && !cfg.general.shutter_sound && !cfg.general.system_notifications);
        assert!(cfg.hotkeys.bindings().is_empty(), "{:?}", cfg.hotkeys.bindings());
        assert!(cfg.hotkeys.extra.is_empty(), "every key is a named one: {:?}", cfg.hotkeys.extra);
        // Every hotkey with a default is cleared.
        let defaults: Vec<_> = Config::default().hotkeys.bindings().into_iter().map(|(n, _)| n).collect();
        assert!(defaults.iter().all(|n| SMOKE_HOTKEYS.contains(n)), "{defaults:?}");
        // A path TOML cannot hold is an error, not a lossy rewrite.
        use std::os::unix::ffi::OsStrExt;
        let bad = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/bugle-\xff"));
        assert!(smoke_config(bad).unwrap_err().to_string().contains("not UTF-8"));
    }
}
