//! `cargo xtask smoke-vm`: the app's `--smoke-test` inside a throwaway macOS
//! VM ([Tart](https://tart.run), Virtualization.framework on Apple silicon),
//! so its overlays, editor windows, synthetic events, recording and sounds
//! stay off the host's screen, clipboard and speakers.
//!
//! * `--prepare` makes the golden image (`bugle-smoke-base` by default) once:
//!   a local clone of an already pulled Cirrus Labs `macos-<name>-base`
//!   image, booted to grant Bugle Screen Recording, Microphone,
//!   Accessibility and Post Event in the guest's TCC databases. Those images
//!   ship with SIP disabled *inside the guest* (their build disables it from
//!   guest recovery), which is what lets `sqlite3` write TCC.db; the host's
//!   SIP is never involved.
//! * A run builds the bundle into its own directory, clones the golden image
//!   to `bugle-smoke-run-<millis>-<pid>` (an APFS copy-on-write clone, so
//!   runs are cheap, clean and can overlap), boots it headless with two
//!   shares (the bundle read-only, a run directory read-write), runs the
//!   smoke test through the Tart guest agent (`tart exec`) with the
//!   throwaway config from [`crate::smoke_config`], streams its output,
//!   copies the guest's run directory back and deletes the VM.
//!
//! Nothing is downloaded implicitly: a missing `tart`, source image or golden
//! image is an error naming the command that fixes it.

use crate::{bundle, smoke_config, smoke_timeout_from_env, BundleArgs, Workspace, APP_EXECUTABLE, APP_NAME};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The golden image runs clone unless `--image` / `BUGLE_SMOKE_VM_IMAGE`
/// names another.
pub const GOLDEN_IMAGE: &str = "bugle-smoke-base";
/// Every per-run clone's name starts with this.
const RUN_PREFIX: &str = "bugle-smoke-run-";
/// Where Tart's default (automount) tag puts `--dir` shares in a macOS guest.
const GUEST_SHARES: &str = "/Volumes/My Shared Files";
/// The guest copy of the bundle and the run directory live here, at a fixed
/// path so the path-based TCC grants written by `--prepare` match it.
const GUEST_DIR: &str = "/Users/admin/bugle-smoke";
/// How long a booting VM gets before its guest agent and shares must answer,
/// unless `BUGLE_SMOKE_VM_BOOT_TIMEOUT` (whole seconds) says otherwise.
const BOOT_TIMEOUT_DEFAULT: Duration = Duration::from_secs(300);
/// Added to the smoke-test limit for copying the bundle in and the results out.
const COPY_MARGIN: Duration = Duration::from_secs(60);
/// The services `--prepare` grants Bugle in the guest.
const TCC_SERVICES: [&str; 4] =
    ["kTCCServiceScreenCapture", "kTCCServiceMicrophone", "kTCCServiceAccessibility", "kTCCServicePostEvent"];
/// The `--prepare` script exits with this when SIP is enabled in the guest.
const EXIT_SIP_ENABLED: i32 = 3;

#[derive(Args)]
pub struct SmokeVmArgs {
    /// Make the golden image instead of running the smoke test: clone
    /// --from (already pulled) to --image, boot it and grant Bugle Screen
    /// Recording, Microphone and Accessibility in the guest.
    #[arg(long)]
    prepare: bool,
    /// With --prepare: the image to start from (default
    /// ghcr.io/cirruslabs/macos-<host macOS>-base:latest). It must already be
    /// local (`tart pull`); nothing is downloaded implicitly.
    #[arg(long, requires = "prepare")]
    from: Option<String>,
    /// With --prepare: replace an existing golden image.
    #[arg(long, requires = "prepare")]
    force: bool,
    /// The golden image runs clone (and --prepare writes).
    #[arg(long, env = "BUGLE_SMOKE_VM_IMAGE", default_value = GOLDEN_IMAGE)]
    image: String,
    /// Run this existing bundle instead of building one.
    #[arg(long, conflicts_with = "prepare")]
    app: Option<PathBuf>,
}

pub fn smoke_vm(args: &SmokeVmArgs) -> Result<()> {
    if std::env::consts::ARCH != "aarch64" {
        bail!("Tart runs macOS guests only on Apple silicon; this Mac is {}", std::env::consts::ARCH);
    }
    check_tart()?;
    let ws = Workspace::load()?;
    if args.prepare {
        prepare(args, &ws)
    } else {
        run_smoke(args, &ws)
    }
}

// --- the run ----------------------------------------------------------------

fn run_smoke(args: &SmokeVmArgs, ws: &Workspace) -> Result<()> {
    // Both limits are checked before anything is built or cloned.
    let limit = smoke_timeout_from_env()? + COPY_MARGIN;
    let boot = boot_timeout(std::env::var("BUGLE_SMOKE_VM_BOOT_TIMEOUT"))?;
    if !image_listed(&tart_list()?, &args.image)? {
        bail!(
            "the Tart image {:?} does not exist; make it once with `cargo xtask smoke-vm --prepare` \
             (see README, Testing in a VM)",
            args.image
        );
    }
    let since_epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let name = run_vm_name(std::process::id(), since_epoch);
    let dir = ws.target_dir.join("xtask").join("smoke-vm").join(&name);
    let share = dir.join("share");
    std::fs::create_dir_all(&share).with_context(|| format!("creating {}", share.display()))?;

    // Each run assembles its own bundle, so overlapping runs never replace
    // the bundle another VM is reading (cargo serialises the builds).
    let app = match &args.app {
        Some(app) => std::path::absolute(app)?,
        None => {
            bundle(&BundleArgs {
                sign: "-".into(),
                bundle_id: None,
                build_number: None,
                out: Some(dir.join("dist")),
                zip: false,
                dmg: false,
                universal: false,
                notarize: false,
                smoke_test: false,
            })?;
            dir.join("dist").join(format!("{APP_NAME}.app"))
        }
    };
    if !app.join("Contents").join("MacOS").join(APP_EXECUTABLE).is_file() {
        bail!("{} is not a Bugle bundle (no Contents/MacOS/{APP_EXECUTABLE})", app.display());
    }
    let app_dir = app.parent().context("the bundle has no parent directory")?;
    let guest_run = Path::new(GUEST_DIR).join("run");
    std::fs::write(share.join("config.toml"), smoke_config(&guest_run)?)?;
    let shared_name = app.file_name().and_then(|n| n.to_str()).context("the bundle path has no UTF-8 name")?;
    let script = guest_run_script(shared_name);
    std::fs::write(share.join("run.sh"), &script)?;

    run_quiet(Command::new("tart").args(["clone", &args.image, &name]))
        .with_context(|| format!("cloning {} to {name}", args.image))?;
    let mut vm = Vm { name: name.clone(), run: None, delete: true, stop_timeout: 5 };
    println!("VM {name} (cloned from {}); if this is interrupted, remove it with `tart delete {name}`", args.image);
    let shares =
        [Share { name: "app", path: app_dir, read_only: true }, Share { name: "run", path: &share, read_only: false }];
    vm.boot(&tart_run_args(&name, &shares)?, &dir.join("tart-run.log"))?;
    vm.wait_ready(&format!("{GUEST_SHARES}/run/run.sh"), boot)?;

    println!("$ tart exec {name} /bin/bash -c <{}>  (limit {}s)", share.join("run.sh").display(), limit.as_secs());
    let mut exec = Command::new("tart")
        .args(["exec", &name, "/bin/bash", "-c", &script])
        .stdin(Stdio::null())
        .spawn()
        .context("running tart exec")?;
    let t0 = Instant::now();
    let status = loop {
        if let Some(status) = exec.try_wait()? {
            break status;
        }
        if t0.elapsed() > limit {
            let _ = exec.kill();
            let _ = exec.wait();
            drop(vm);
            bail!(
                "the smoke test in the VM did not finish within {}s (BUGLE_SMOKE_TIMEOUT sets the app's limit); \
                 output so far in {}",
                limit.as_secs(),
                share.display()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    drop(vm);

    let log = std::fs::read_to_string(share.join("smoke.log")).unwrap_or_default();
    let artifacts = share.join("artifacts");
    if !status.success() {
        bail!("smoke test failed in the VM ({status}); log and artefacts in {}", share.display());
    }
    if let Some(problem) = missing_grant(&log) {
        bail!(
            "smoke test passed in the VM but {problem}; the image {:?} lacks the grant, \
             rebuild it with `cargo xtask smoke-vm --prepare --force`",
            args.image
        );
    }
    println!("smoke test passed in the VM in {:.1}s; artefacts in {}", t0.elapsed().as_secs_f32(), artifacts.display());
    Ok(())
}

/// A line of the smoke log showing that the VM's grants did not apply (the
/// recording step is otherwise only skipped, which passes).
fn missing_grant(log: &str) -> Option<&str> {
    log.lines().map(str::trim).find(|l| l.contains("Screen Recording access is not granted"))
}

/// The shell script run in the guest: copy the bundle `shared_name` out of
/// the read-only share to `Bugle.app` in [`GUEST_DIR`] (the path the TCC
/// grants name), run the smoke test
/// with the throwaway config, tee its output to the run share and copy the
/// guest run directory (captures, history, kept temp files) back.
///
/// The guest's clipboard is its own (`--no-clipboard`), so the ⌘V step runs,
/// and saving is on, since nothing it writes leaves the VM.
fn guest_run_script(shared_name: &str) -> String {
    let run = format!("{GUEST_DIR}/run");
    let app = format!("{GUEST_DIR}/{APP_NAME}.app");
    let shared_run = format!("{GUEST_SHARES}/run");
    let q = |s: &str| sh_quote(s);
    format!(
        r#"#!/bin/bash
# Written by `cargo xtask smoke-vm`; runs inside the VM.
set -uo pipefail
echo "guest: macOS $(sw_vers -productVersion) ($(uname -m))"
rm -rf {dir} && mkdir -p {captures} {tmp} || exit 90
ditto {shared_app} {app} || exit 91
cp {shared_config} {config} || exit 92
cd {run} || exit 93
env -u BUGLE_CONFIG TMPDIR={tmp} BUGLE_SMOKE_KEEP=1 BUGLE_SMOKE_CLIPBOARD=1 BUGLE_SMOKE_SAVE=1 \
  {exe} --smoke-test --config {config} 2>&1 | tee {log}
status=${{PIPESTATUS[0]}}
ditto {run} {artifacts} || echo "copying the artefacts back failed" >&2
exit "$status"
"#,
        dir = q(GUEST_DIR),
        captures = q(&format!("{run}/captures")),
        tmp = q(&format!("{run}/tmp")),
        shared_app = q(&format!("{GUEST_SHARES}/app/{shared_name}")),
        app = q(&app),
        shared_config = q(&format!("{shared_run}/config.toml")),
        config = q(&format!("{run}/config.toml")),
        exe = q(&format!("{app}/Contents/MacOS/{APP_EXECUTABLE}")),
        log = q(&format!("{shared_run}/smoke.log")),
        artifacts = q(&format!("{shared_run}/artifacts")),
        run = q(&run),
    )
}

// --- the golden image -------------------------------------------------------

fn prepare(args: &SmokeVmArgs, ws: &Workspace) -> Result<()> {
    let boot = boot_timeout(std::env::var("BUGLE_SMOKE_VM_BOOT_TIMEOUT"))?;
    let from = match &args.from {
        Some(f) => f.clone(),
        None => {
            let version = command_stdout(Command::new("sw_vers").arg("-productVersion"))?;
            match default_base_image(&version) {
                Some(img) => img,
                None => bail!("no Cirrus Labs image is known for macOS {version}; name one with --from"),
            }
        }
    };
    let list = tart_list()?;
    if !image_listed(&list, &from)? {
        bail!(
            "the Tart image {from:?} is not on this Mac. Pull it first (tens of GB, once):\n  tart pull {from}\n\
             then run `cargo xtask smoke-vm --prepare` again"
        );
    }
    if image_listed(&list, &args.image)? {
        if !args.force {
            bail!("{:?} already exists; pass --force to replace it", args.image);
        }
        run_quiet(Command::new("tart").args(["delete", &args.image]))?;
    }
    run_quiet(Command::new("tart").args(["clone", &from, &args.image]))
        .with_context(|| format!("cloning {from} to {}", args.image))?;
    // Deleted again unless every step below succeeds.
    let mut vm = Vm { name: args.image.clone(), run: None, delete: true, stop_timeout: 120 };
    let log = ws.target_dir.join("xtask").join("smoke-vm").join(format!("prepare-{}.log", args.image));
    std::fs::create_dir_all(log.parent().expect("has a parent"))?;
    vm.boot(&tart_run_args(&args.image, &[])?, &log)?;
    vm.wait_ready("/", boot)?;

    let exe = Path::new(GUEST_DIR).join(format!("{APP_NAME}.app")).join("Contents").join("MacOS").join(APP_EXECUTABLE);
    let script = guest_prepare_script(&ws.bundle_id, &exe.display().to_string());
    println!("$ tart exec {} /bin/bash -c <grant script>", args.image);
    let status = Command::new("tart")
        .args(["exec", &args.image, "/bin/bash", "-c", &script])
        .stdin(Stdio::null())
        .status()
        .context("running tart exec")?;
    if status.code() == Some(EXIT_SIP_ENABLED) {
        bail!(
            "SIP is enabled inside {from}, so its TCC databases cannot be written. Use a Cirrus Labs \
             `-base` or `-xcode` image (they ship with SIP disabled in the guest), or disable it in the \
             guest by hand: `tart run --recovery <vm>`, Options → Utilities → Terminal, `csrutil disable`, \
             `halt`; then pass that VM with --from. Never on the host."
        );
    }
    if !status.success() {
        bail!("granting the permissions in the guest failed ({status})");
    }
    // A clean shutdown, so the TCC writes reach the disk image.
    vm.delete = false;
    drop(vm);
    println!("prepared {:?} from {from}; `cargo xtask smoke-vm` now clones it for each run", args.image);
    Ok(())
}

/// The `--prepare` script: refuse unless SIP is off in the guest, then
/// grant [`TCC_SERVICES`] to Bugle by bundle id and by the guest
/// executable's path, in the system and the user TCC database (the way the
/// Cirrus Labs images grant their guest agent), and mark the screen-capture
/// approval as fresh for macOS 15's periodic re-confirmation (best effort).
fn guest_prepare_script(bundle_id: &str, exe: &str) -> String {
    let sql = tcc_sql(bundle_id, exe);
    format!(
        r#"#!/bin/bash
# Written by `cargo xtask smoke-vm --prepare`; runs inside the VM.
set -euo pipefail
echo "guest: macOS $(sw_vers -productVersion), $(csrutil status)"
if ! csrutil status | grep -q disabled; then exit {sip}; fi
major=$(sw_vers -productVersion | cut -d. -f1)
if [ "$major" -lt 27 ]; then
  user_db="$HOME/Library/Application Support/com.apple.TCC/TCC.db"
else
  # macOS 27 keeps the user database in a ProtectedSystem container; ask tccd.
  user_db=$(sudo lsof -a -u "$(id -u)" -c tccd -Fn | sed -n 's|^n\(/private/var/containers/Data/ProtectedSystem/.*/com\.apple\.TCC/TCC\.db\)$|\1|p' | sort -u)
  if [ "$(printf '%s\n' "$user_db" | grep -c .)" != 1 ]; then echo "cannot find the user TCC database" >&2; exit 4; fi
fi
for db in "/Library/Application Support/com.apple.TCC/TCC.db" "$user_db"; do
  sudo test -f "$db" || {{ echo "missing $db" >&2; exit 5; }}
  sudo sqlite3 "$db" {sql}
  echo "granted in $db"
done
approvals="$HOME/Library/Group Containers/group.com.apple.replayd/ScreenCaptureApprovals"
defaults write "$approvals" {exe} -date "2100-01-01 00:00:00 +0000" 2>/dev/null || true
"#,
        sip = EXIT_SIP_ENABLED,
        sql = sh_quote(&sql),
        exe = sh_quote(exe),
    )
}

/// `INSERT OR REPLACE` rows granting (`auth_value` 2) each of
/// [`TCC_SERVICES`] to the bundle id (`client_type` 0) and the executable
/// path (`client_type` 1), without a code requirement, so ad-hoc builds whose
/// signature changes every time still match.
fn tcc_sql(bundle_id: &str, exe: &str) -> String {
    let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let rows: Vec<String> = TCC_SERVICES
        .iter()
        .flat_map(|svc| {
            [(bundle_id, 0), (exe, 1)]
                .map(|(client, ty)| format!("({}, {ty}, {}, 2, 0, 1, NULL, 'UNUSED')", lit(svc), lit(client)))
        })
        .collect();
    format!(
        "INSERT OR REPLACE INTO access (service, client_type, client, auth_value, auth_reason, auth_version, \
         indirect_object_identifier_type, indirect_object_identifier) VALUES\n{};",
        rows.join(",\n")
    )
}

/// `ghcr.io/cirruslabs/macos-<name>-base:latest` for the host's macOS
/// (`sw_vers -productVersion`), when Cirrus Labs publishes one.
fn default_base_image(product_version: &str) -> Option<String> {
    let major: u32 = product_version.trim().split('.').next()?.parse().ok()?;
    let name = match major {
        14 => "sonoma",
        15 => "sequoia",
        26 => "tahoe",
        27 => "golden-gate",
        _ => return None,
    };
    Some(format!("ghcr.io/cirruslabs/macos-{name}-base:latest"))
}

// --- Tart -------------------------------------------------------------------

/// `tart` is on PATH and has `exec` (Tart 2.22 and later).
fn check_tart() -> Result<()> {
    let out = match Command::new("tart").arg("--version").output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "`tart` is not installed. Install it with `brew install cirruslabs/cli/tart` (https://tart.run), \
             then pull a base image and run `cargo xtask smoke-vm --prepare` (see README, Testing in a VM)"
        ),
        Err(e) => return Err(e).context("running tart --version"),
    };
    let ok = Command::new("tart").args(["exec", "--help"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    if !ok.is_ok_and(|s| s.success()) {
        bail!(
            "tart {} has no `exec` command; update it with `brew upgrade cirruslabs/cli/tart`",
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    Ok(())
}

fn tart_list() -> Result<String> {
    command_stdout(Command::new("tart").args(["list", "--format", "json"]))
}

/// Whether `tart list --format json` output has a VM or image called `name`
/// (an OCI reference without a tag also matches its `:latest`).
fn image_listed(list_json: &str, name: &str) -> Result<bool> {
    let v: serde_json::Value = serde_json::from_str(list_json).context("parsing tart list --format json")?;
    let entries = v.as_array().context("tart list --format json: expected an array")?;
    let tagged = format!("{name}:latest");
    let untagged_oci = name.contains('/') && !name.rsplit('/').next().unwrap_or("").contains([':', '@']);
    Ok(entries.iter().filter_map(|e| e["Name"].as_str()).any(|n| n == name || (untagged_oci && n == tagged)))
}

/// A per-run VM name: unique per process and millisecond, and only letters,
/// digits and hyphens.
fn run_vm_name(pid: u32, since_epoch: Duration) -> String {
    format!("{RUN_PREFIX}{}-{pid}", since_epoch.as_millis())
}

/// A `tart run --dir` share: `/Volumes/My Shared Files/<name>` in the guest.
struct Share<'a> {
    name: &'a str,
    path: &'a Path,
    read_only: bool,
}

/// `tart run` arguments: no window (the guest still has its display, which
/// ScreenCaptureKit records), no host audio (the guest gets a null speaker,
/// so the test's sounds and recorded system audio stay in the VM and the
/// host microphone is not attached), no shared clipboard, plus the shares.
fn tart_run_args(name: &str, shares: &[Share]) -> Result<Vec<OsString>> {
    let mut args: Vec<OsString> = ["run", "--no-graphics", "--no-audio", "--no-clipboard"].map(OsString::from).into();
    for s in shares {
        let path = s.path.to_str().with_context(|| format!("{} is not UTF-8", s.path.display()))?;
        if !s.path.is_absolute() || path.contains(':') || s.name.contains(':') {
            bail!("cannot share {path:?} as {:?}: tart --dir needs an absolute path without ':'", s.name);
        }
        args.push(format!("--dir={}:{path}{}", s.name, if s.read_only { ":ro" } else { "" }).into());
    }
    args.push(name.into());
    Ok(args)
}

/// A Tart VM this run created: on drop it is stopped (if this run booted
/// it) and, unless `delete` was cleared, deleted.
struct Vm {
    name: String,
    run: Option<Child>,
    delete: bool,
    /// Seconds `tart stop` waits for a clean shutdown before forcing it.
    stop_timeout: u32,
}

impl Vm {
    fn boot(&mut self, args: &[OsString], log: &Path) -> Result<()> {
        let out = std::fs::File::create(log).with_context(|| format!("creating {}", log.display()))?;
        println!(
            "$ tart {}  (output in {})",
            args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "),
            log.display()
        );
        let child = Command::new("tart")
            .args(args)
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .spawn()
            .context("running tart run")?;
        self.run = Some(child);
        Ok(())
    }

    /// Waits until the guest agent answers and `path` exists in the guest
    /// (the shares appear once the user session has logged in).
    fn wait_ready(&mut self, path: &str, limit: Duration) -> Result<()> {
        let t0 = Instant::now();
        loop {
            if let Some(status) = self.run.as_mut().and_then(|c| c.try_wait().transpose()).transpose()? {
                bail!("tart run exited ({status}) before the VM was ready; see its log");
            }
            let ok = Command::new("tart")
                .args(["exec", &self.name, "/bin/test", "-e", path])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                println!("VM {} ready after {:.0}s", self.name, t0.elapsed().as_secs_f32());
                return Ok(());
            }
            if t0.elapsed() > limit {
                bail!(
                    "VM {} not ready after {}s (no guest agent, or {path:?} missing; \
                     BUGLE_SMOKE_VM_BOOT_TIMEOUT sets the limit)",
                    self.name,
                    limit.as_secs()
                );
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if let Some(mut child) = self.run.take() {
            let _ =
                run_quiet(Command::new("tart").args(["stop", "--timeout", &self.stop_timeout.to_string(), &self.name]));
            let _ = child.kill();
            let _ = child.wait();
        }
        if self.delete {
            if let Err(e) = run_quiet(Command::new("tart").args(["delete", &self.name])) {
                eprintln!("could not delete VM {}: {e:#}; remove it with `tart delete {}`", self.name, self.name);
            }
        }
    }
}

// --- helpers ----------------------------------------------------------------

/// The VM boot limit from the value of `BUGLE_SMOKE_VM_BOOT_TIMEOUT`.
fn boot_timeout(var: Result<String, std::env::VarError>) -> Result<Duration> {
    let v = match var {
        Err(std::env::VarError::NotPresent) => return Ok(BOOT_TIMEOUT_DEFAULT),
        Err(std::env::VarError::NotUnicode(v)) => {
            bail!("BUGLE_SMOKE_VM_BOOT_TIMEOUT={v:?}: expected a whole number of seconds greater than 0")
        }
        Ok(v) => v,
    };
    let v = v.trim();
    if v.is_empty() {
        return Ok(BOOT_TIMEOUT_DEFAULT);
    }
    match v.parse::<u64>() {
        Ok(s) if s > 0 => Ok(Duration::from_secs(s)),
        _ => bail!("BUGLE_SMOKE_VM_BOOT_TIMEOUT={v:?}: expected a whole number of seconds greater than 0"),
    }
}

/// `s` as one single-quoted POSIX shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Runs a command, echoing it, with its output captured and shown only if
/// it fails.
fn run_quiet(cmd: &mut Command) -> Result<()> {
    let shown: Vec<String> =
        std::iter::once(cmd.get_program()).chain(cmd.get_args()).map(|s| s.to_string_lossy().into_owned()).collect();
    println!("$ {}", shown.join(" "));
    let out = cmd.stdin(Stdio::null()).output().with_context(|| format!("running {}", shown[0]))?;
    if !out.status.success() {
        bail!("{} failed ({}): {}", shown.join(" "), out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn command_stdout(cmd: &mut Command) -> Result<String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let out =
        cmd.stdin(Stdio::null()).stderr(Stdio::inherit()).output().with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!("{program} failed ({})", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_names_are_unique_and_valid() {
        let a = run_vm_name(42, Duration::from_millis(1_700_000_000_123));
        assert_eq!(a, "bugle-smoke-run-1700000000123-42");
        assert_ne!(a, run_vm_name(43, Duration::from_millis(1_700_000_000_123)));
        assert_ne!(a, run_vm_name(42, Duration::from_millis(1_700_000_000_124)));
        assert!(a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        assert!(!a.starts_with(GOLDEN_IMAGE), "a run clone must never look like the golden image");
    }

    #[test]
    fn boot_timeouts() {
        use std::env::VarError;
        use std::os::unix::ffi::OsStringExt;
        let ok = |s: &str| boot_timeout(Ok(s.to_string()));
        assert_eq!(boot_timeout(Err(VarError::NotPresent)).unwrap(), BOOT_TIMEOUT_DEFAULT);
        assert_eq!(ok(" ").unwrap(), BOOT_TIMEOUT_DEFAULT);
        assert_eq!(ok("600").unwrap(), Duration::from_secs(600));
        let not_unicode = boot_timeout(Err(VarError::NotUnicode(std::ffi::OsString::from_vec(vec![b'6', 0xff]))));
        assert!(not_unicode.unwrap_err().to_string().contains("BUGLE_SMOKE_VM_BOOT_TIMEOUT"));
        for bad in ["0", "-1", "1.5", "5m", "x"] {
            assert!(ok(bad).unwrap_err().to_string().contains("BUGLE_SMOKE_VM_BOOT_TIMEOUT"), "{bad}");
        }
    }

    #[test]
    fn run_args() {
        let app = Path::new("/tmp/run 1/dist");
        let run = Path::new("/tmp/run 1/share");
        let args = tart_run_args(
            "vm",
            &[Share { name: "app", path: app, read_only: true }, Share { name: "run", path: run, read_only: false }],
        )
        .unwrap();
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "run",
                "--no-graphics",
                "--no-audio",
                "--no-clipboard",
                "--dir=app:/tmp/run 1/dist:ro",
                "--dir=run:/tmp/run 1/share",
                "vm"
            ]
        );
        let bad = |p: &str| tart_run_args("vm", &[Share { name: "x", path: Path::new(p), read_only: false }]).is_err();
        assert!(bad("relative/dir") && bad("/a:b"));
        assert_eq!(tart_run_args("vm", &[]).unwrap().last().unwrap(), "vm");
    }

    #[test]
    fn listed_images() {
        let list = r#"[
            {"Source":"OCI","Name":"ghcr.io/cirruslabs/macos-tahoe-base:latest","Disk":50,"Size":25,"Running":false,"State":"stopped"},
            {"Source":"local","Name":"bugle-smoke-base","Disk":50,"Size":25,"Running":false,"State":"stopped"}
        ]"#;
        assert!(image_listed(list, "bugle-smoke-base").unwrap());
        assert!(image_listed(list, "ghcr.io/cirruslabs/macos-tahoe-base:latest").unwrap());
        assert!(image_listed(list, "ghcr.io/cirruslabs/macos-tahoe-base").unwrap());
        assert!(!image_listed(list, "ghcr.io/cirruslabs/macos-tahoe-base:2025").unwrap());
        assert!(!image_listed(list, "bugle-smoke").unwrap());
        assert!(!image_listed("[]", "bugle-smoke-base").unwrap());
        assert!(image_listed("not json", "x").is_err());
    }

    #[test]
    fn base_images_follow_the_host() {
        assert_eq!(default_base_image("26.5.1").as_deref(), Some("ghcr.io/cirruslabs/macos-tahoe-base:latest"));
        assert_eq!(default_base_image("15.7\n").as_deref(), Some("ghcr.io/cirruslabs/macos-sequoia-base:latest"));
        assert_eq!(default_base_image("14.0").as_deref(), Some("ghcr.io/cirruslabs/macos-sonoma-base:latest"));
        assert_eq!(default_base_image("27.0").as_deref(), Some("ghcr.io/cirruslabs/macos-golden-gate-base:latest"));
        for none in ["13.6", "99.0", "", "x"] {
            assert_eq!(default_base_image(none), None, "{none}");
        }
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn run_script_uses_fixed_guest_paths() {
        let other = guest_run_script("My Build.app");
        assert!(
            other.contains("ditto '/Volumes/My Shared Files/app/My Build.app' '/Users/admin/bugle-smoke/Bugle.app'")
        );
        let s = guest_run_script("Bugle.app");
        assert!(
            s.contains("ditto '/Volumes/My Shared Files/app/Bugle.app' '/Users/admin/bugle-smoke/Bugle.app'"),
            "{s}"
        );
        assert!(s.contains("'/Users/admin/bugle-smoke/Bugle.app/Contents/MacOS/bugle-app' --smoke-test --config '/Users/admin/bugle-smoke/run/config.toml'"), "{s}");
        assert!(s.contains("env -u BUGLE_CONFIG TMPDIR='/Users/admin/bugle-smoke/run/tmp' "), "{s}");
        assert!(s.contains("status=${PIPESTATUS[0]}") && s.trim_end().ends_with("exit \"$status\""), "{s}");
        // The config it copies in points at the same guest run directory.
        let cfg = smoke_config(&Path::new(GUEST_DIR).join("run")).unwrap();
        assert!(cfg.contains("\"/Users/admin/bugle-smoke/run/captures\""), "{cfg}");
    }

    #[test]
    fn grant_sql() {
        let sql = tcc_sql("io.example.bugle", "/Users/admin/x's/bugle-app");
        assert_eq!(sql.matches("('kTCCService").count(), TCC_SERVICES.len() * 2);
        assert!(sql.contains("('kTCCServiceScreenCapture', 0, 'io.example.bugle', 2, 0, 1, NULL, 'UNUSED')"), "{sql}");
        assert!(
            sql.contains("('kTCCServiceMicrophone', 1, '/Users/admin/x''s/bugle-app', 2, 0, 1, NULL, 'UNUSED')"),
            "{sql}"
        );
        let script = guest_prepare_script("io.example.bugle", "/p");
        assert!(script.contains(&format!("exit {EXIT_SIP_ENABLED}")));
        assert!(script.contains("/Library/Application Support/com.apple.TCC/TCC.db"));
    }

    #[test]
    fn flags_parse() {
        use clap::Parser;
        let parse =
            |args: &[&str]| crate::Cli::try_parse_from(["cargo xtask", "smoke-vm"].iter().chain(args)).map(|c| c.cmd);
        let Ok(crate::Cmd::SmokeVm(a)) = parse(&[]) else { panic!() };
        assert!(!a.prepare && a.app.is_none());
        // The env var may be set by whoever runs the tests; the flag wins.
        let Ok(crate::Cmd::SmokeVm(a)) = parse(&["--prepare", "--from", "img", "--force", "--image", "g"]) else {
            panic!()
        };
        assert!(a.prepare && a.force && a.from.as_deref() == Some("img") && a.image == "g");
        assert!(parse(&["--from", "img"]).is_err(), "--from needs --prepare");
        assert!(parse(&["--force"]).is_err(), "--force needs --prepare");
        assert!(parse(&["--prepare", "--app", "x"]).is_err());
    }

    #[test]
    fn scripts_parse() {
        for script in [guest_run_script("Bugle.app"), guest_prepare_script("io.example.bugle", "/a b/bugle-app")] {
            let ok = Command::new("/bin/bash").args(["-n", "-c", &script]).status().unwrap().success();
            assert!(ok, "bash -n rejected:\n{script}");
        }
        assert!(guest_run_script("Bugle.app").contains("BUGLE_SMOKE_SAVE=1 \\\n  '/Users/admin"));
    }

    #[test]
    fn missing_grants_are_found() {
        assert_eq!(missing_grant("smoke test: level meter: ok\n"), None);
        assert!(
            missing_grant("x\nsmoke test: level meter: skipped (Screen Recording access is not granted)\n").is_some()
        );
    }
}
