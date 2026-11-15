//! `cargo xtask` — Bugle's build tasks.
//!
//! * `bundle` builds the release binaries and assembles `dist/Bugle.app`:
//!   Info.plist, icon, the `bugle-app` executable and the `bugle` CLI, signed
//!   with the hardened runtime (ad-hoc unless an identity is given), verified,
//!   and optionally zipped and notarized.
//! * `verify` re-runs the checks on an existing bundle.
//! * `icon` writes the `.icns` and a 1024 px PNG preview.

mod icns;
mod plist;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const APP_NAME: &str = "Bugle";
const APP_EXECUTABLE: &str = "bugle-app";
const CLI_EXECUTABLE: &str = "bugle";
const ICON_NAME: &str = "Bugle";
const DEFAULT_BUNDLE_ID: &str = "io.github.mlloyd.bugle";
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
    /// Check an existing bundle (signature, Info.plist, icon, binaries).
    Verify {
        /// The bundle to check (default dist/Bugle.app).
        app: Option<PathBuf>,
        /// Also launch the bundled app with `--smoke-test` (shows UI for a few seconds).
        #[arg(long)]
        smoke_test: bool,
    },
    /// Write Bugle.icns and a 1024 px PNG of the app icon.
    Icon {
        /// Output directory (default dist/).
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
    /// CFBundleIdentifier.
    #[arg(long, env = "BUGLE_BUNDLE_ID", default_value = DEFAULT_BUNDLE_ID)]
    bundle_id: String,
    /// CFBundleVersion (default: the workspace version).
    #[arg(long, env = "BUGLE_BUILD_NUMBER")]
    build_number: Option<String>,
    /// Output directory (default dist/).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Reuse the existing release binaries instead of running `cargo build`.
    #[arg(long)]
    no_build: bool,
    /// Also write dist/Bugle-<version>.zip.
    #[arg(long)]
    zip: bool,
    /// Submit to Apple's notary service and staple the ticket. Needs a
    /// Developer ID identity plus BUGLE_NOTARY_PROFILE (a `notarytool
    /// store-credentials` profile) or BUGLE_NOTARY_APPLE_ID,
    /// BUGLE_NOTARY_TEAM_ID and BUGLE_NOTARY_PASSWORD; skipped otherwise.
    #[arg(long)]
    notarize: bool,
    /// Launch the bundled app with `--smoke-test` after verifying.
    #[arg(long)]
    smoke_test: bool,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Bundle(args) => bundle(&args),
        Cmd::Verify { app, smoke_test } => {
            let app = app.unwrap_or_else(|| dist_dir(None).join(format!("{APP_NAME}.app")));
            verify(&app)?;
            if smoke_test {
                smoke(&app)?;
            }
            Ok(())
        }
        Cmd::Icon { out } => {
            let out = dist_dir(out);
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

fn dist_dir(out: Option<PathBuf>) -> PathBuf {
    out.unwrap_or_else(|| workspace_root().join("dist"))
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| workspace_root().join("target"))
}

fn entitlements() -> PathBuf {
    workspace_root().join("packaging").join(format!("{APP_NAME}.entitlements"))
}

fn bundle(args: &BundleArgs) -> Result<()> {
    let root = workspace_root();
    if !args.no_build {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        run(Command::new(cargo)
            .current_dir(&root)
            .args(["build", "--release", "--locked", "-p", "bugle-app", "-p", "bugle-cli"]))?;
    }
    let release = target_dir().join("release");
    for exe in [APP_EXECUTABLE, CLI_EXECUTABLE] {
        if !release.join(exe).is_file() {
            bail!("{} is missing; build without --no-build", release.join(exe).display());
        }
    }

    // Assemble.
    let out = dist_dir(args.out.clone());
    let app = out.join(format!("{APP_NAME}.app"));
    if app.exists() {
        std::fs::remove_dir_all(&app).with_context(|| format!("removing old {}", app.display()))?;
    }
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    std::fs::create_dir_all(&macos)?;
    std::fs::create_dir_all(&resources)?;
    for exe in [APP_EXECUTABLE, CLI_EXECUTABLE] {
        std::fs::copy(release.join(exe), macos.join(exe)).with_context(|| format!("copying {exe}"))?;
    }
    let build_number = args.build_number.as_deref().unwrap_or(VERSION);
    let info = plist::info_plist(&plist::BundleInfo {
        bundle_id: &args.bundle_id,
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
    let adhoc = args.sign == "-";
    codesign(&macos.join(CLI_EXECUTABLE), &args.sign, Some(&format!("{}.cli", args.bundle_id)))?;
    codesign(&app, &args.sign, None)?;

    verify(&app)?;

    let zip = out.join(format!("{APP_NAME}-{VERSION}.zip"));
    if args.zip || args.notarize {
        make_zip(&app, &zip)?;
    }
    if args.notarize {
        match notary_credentials() {
            _ if adhoc => println!("skipping notarization: ad-hoc signature (set --sign / BUGLE_SIGN_IDENTITY)"),
            None => println!("skipping notarization: no BUGLE_NOTARY_PROFILE or BUGLE_NOTARY_APPLE_ID/_TEAM_ID/_PASSWORD"),
            Some(creds) => {
                notarize(&zip, &creds)?;
                run(Command::new("xcrun").args(["stapler", "staple"]).arg(&app))?;
                run(Command::new("xcrun").args(["stapler", "validate"]).arg(&app))?;
                // Re-zip so the archive carries the stapled ticket.
                make_zip(&app, &zip)?;
            }
        }
    }
    if !adhoc {
        // Gatekeeper's view; rejects until the bundle is notarized.
        let _ = run(Command::new("spctl").args(["--assess", "--type", "execute", "-vv"]).arg(&app));
    }
    if args.smoke_test {
        smoke(&app)?;
    }

    println!("\n{APP_NAME}.app {VERSION} ({build_number}), {} signature", if adhoc { "ad-hoc" } else { "Developer ID" });
    println!("  app: {}", app.display());
    if zip.exists() && (args.zip || args.notarize) {
        println!("  zip: {}", zip.display());
    }
    println!("  CLI: ln -s \"<install dir>/{APP_NAME}.app/Contents/MacOS/{CLI_EXECUTABLE}\" /usr/local/bin/{CLI_EXECUTABLE}");
    Ok(())
}

fn codesign(path: &Path, identity: &str, identifier: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("codesign");
    cmd.args(["--force", "--sign", identity, "--options", "runtime", "--entitlements"]).arg(entitlements());
    // Secure timestamps need Apple's server and a real identity.
    cmd.arg(if identity == "-" { "--timestamp=none" } else { "--timestamp" });
    if let Some(id) = identifier {
        cmd.args(["--identifier", id]);
    }
    run(cmd.arg(path))
}

/// The checks `bundle` runs after signing.
fn verify(app: &Path) -> Result<()> {
    if !app.is_dir() {
        bail!("{} not found; run `cargo xtask bundle` first", app.display());
    }
    let contents = app.join("Contents");
    run(Command::new("plutil").arg("-lint").arg(contents.join("Info.plist")).arg(entitlements()))?;
    run(Command::new("codesign").args(["--verify", "--deep", "--strict", "--verbose=2"]).arg(app))?;
    for target in [app.to_path_buf(), contents.join("MacOS").join(CLI_EXECUTABLE)] {
        let out = Command::new("codesign").args(["--display", "--verbose=2"]).arg(&target).output()?;
        let text = String::from_utf8_lossy(&out.stderr);
        let flags = text.lines().find(|l| l.contains("flags=")).unwrap_or("").trim();
        let ident = text.lines().find(|l| l.starts_with("Identifier=")).unwrap_or("").trim();
        println!("  {}: {ident}, {flags}", target.file_name().unwrap_or_default().to_string_lossy());
        if !flags.contains("runtime") {
            bail!("{} is not signed with the hardened runtime", target.display());
        }
    }
    let ents = Command::new("codesign").args(["--display", "--entitlements", "-", "--xml"]).arg(app).output()?;
    if !String::from_utf8_lossy(&ents.stdout).contains("com.apple.security.device.audio-input") {
        bail!("{} lacks the audio-input entitlement", app.display());
    }

    // The icon must round-trip through iconutil.
    let check = target_dir().join("xtask").join("icon-check.iconset");
    let _ = std::fs::remove_dir_all(&check);
    std::fs::create_dir_all(check.parent().unwrap())?;
    run(Command::new("iconutil")
        .args(["--convert", "iconset", "--output"])
        .arg(&check)
        .arg(contents.join("Resources").join(format!("{ICON_NAME}.icns"))))?;
    let n = std::fs::read_dir(&check)?.count();
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

enum NotaryCredentials {
    Profile(String),
    AppleId { apple_id: String, team_id: String, password: String },
}

fn notary_credentials() -> Option<NotaryCredentials> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(p) = var("BUGLE_NOTARY_PROFILE") {
        return Some(NotaryCredentials::Profile(p));
    }
    Some(NotaryCredentials::AppleId {
        apple_id: var("BUGLE_NOTARY_APPLE_ID")?,
        team_id: var("BUGLE_NOTARY_TEAM_ID")?,
        password: var("BUGLE_NOTARY_PASSWORD")?,
    })
}

fn notarize(zip: &Path, creds: &NotaryCredentials) -> Result<()> {
    let mut cmd = Command::new("xcrun");
    cmd.args(["notarytool", "submit"]).arg(zip).arg("--wait");
    match creds {
        NotaryCredentials::Profile(p) => {
            println!("$ xcrun notarytool submit {} --wait --keychain-profile {p}", zip.display());
            cmd.args(["--keychain-profile", p]);
        }
        NotaryCredentials::AppleId { apple_id, team_id, password } => {
            println!("$ xcrun notarytool submit {} --wait --apple-id {apple_id} --team-id {team_id} --password ***", zip.display());
            cmd.args(["--apple-id", apple_id, "--team-id", team_id, "--password", password]);
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
