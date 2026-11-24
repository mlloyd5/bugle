//! `cargo xtask smoke-vm`: the app's `--smoke-test` inside a throwaway macOS
//! VM ([Tart](https://tart.run), Virtualization.framework on Apple silicon),
//! so its overlays, editor windows, synthetic events, recording and sounds
//! stay off the host's screen, clipboard and speakers. `bundle --smoke-vm`
//! and `verify --smoke-vm` run the same step on the bundle they just built
//! or checked ([`VmSmoke`]).
//!
//! * `--prepare` makes the golden image (`bugle-smoke-base` by default) once:
//!   a local clone of an already pulled Cirrus Labs `macos-<name>-base`
//!   image, booted to grant Bugle Screen Recording, Microphone,
//!   Accessibility and Post Event in the guest's TCC databases. Those images
//!   ship with SIP disabled *inside the guest* (their build disables it from
//!   guest recovery), which is what lets `sqlite3` write TCC.db; the host's
//!   SIP is never involved. Before writing anything it reads `csrutil
//!   status` and `sw_vers` from the guest and stops with the cause and the
//!   fix when SIP still protects TCC.db or the guest's macOS is one whose
//!   TCC layout this tool does not know ([`grant_plan`]).
//! * A run builds the bundle into its own directory, clones the golden image
//!   to `bugle-smoke-run-<millis>-<pid>` (an APFS copy-on-write clone, so
//!   runs are cheap, clean and can overlap), boots it headless with two
//!   shares (the bundle read-only, a run directory read-write), runs the
//!   smoke test through the Tart guest agent (`tart exec`) with the
//!   throwaway config from [`crate::smoke_config`], streams its output,
//!   copies the guest's run directory back and deletes the VM.
//! * SIGINT, SIGTERM and SIGHUP in any phase stop the child processes, stop
//!   and delete the clone and exit with 128 + the signal ([`crate::interrupt`]).
//!   A run killed outright (SIGKILL) cannot clean up; the next run, or
//!   `--cleanup`, removes clones whose owning process is gone ([`stale_runs`]).
//!
//! Nothing is downloaded implicitly: a missing `tart`, source image or golden
//! image is an error naming the command that fixes it.

use crate::{bundle, interrupt, smoke_config, smoke_timeout_from_env, BundleArgs, Workspace, APP_EXECUTABLE, APP_NAME};
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
/// macOS majors and the Cirrus Labs image names for them.
const MACOS_NAMES: [(u32, &str); 4] = [(14, "sonoma"), (15, "sequoia"), (26, "tahoe"), (27, "golden-gate")];
/// The guest macOS majors whose TCC layout `--prepare` knows. Only 26 has
/// been run; 27 follows what is known of its layout (see [`UserDb`]).
const KNOWN_GUESTS: std::ops::RangeInclusive<u32> = 14..=27;
/// Set to try the newest known layout on a newer guest anyway.
const UNTESTED_GUEST_VAR: &str = "BUGLE_SMOKE_VM_UNTESTED_GUEST";
/// Exit codes of the `--prepare` script.
const EXIT_NO_USER_DB: i32 = 4;
const EXIT_MISSING_DB: i32 = 5;
const EXIT_SQLITE: i32 = 6;

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
    /// Only remove what earlier runs that were killed left behind (their
    /// `bugle-smoke-run-…` VMs whose process is gone), then exit. Every run
    /// does this first anyway.
    #[arg(long, conflicts_with_all = ["prepare", "app"])]
    cleanup: bool,
}

pub fn smoke_vm(args: &SmokeVmArgs) -> Result<()> {
    interrupt::install();
    let ws = Workspace::load()?;
    if args.cleanup {
        host_checks()?;
        return cleanup_stale(&ws);
    }
    if args.prepare {
        host_checks()?;
        return prepare(args, &ws);
    }
    VmSmoke::preflight(&args.image, &ws)?.run(&ws, args.app.as_deref())
}

/// The golden image `bundle --smoke-vm` and `verify --smoke-vm` clone:
/// `BUGLE_SMOKE_VM_IMAGE`, else [`GOLDEN_IMAGE`].
pub fn image_from_env() -> String {
    std::env::var("BUGLE_SMOKE_VM_IMAGE").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| GOLDEN_IMAGE.into())
}

/// Apple silicon, and a `tart` with `exec`.
fn host_checks() -> Result<()> {
    if std::env::consts::ARCH != "aarch64" {
        bail!("Tart runs macOS guests only on Apple silicon; this Mac is {}", std::env::consts::ARCH);
    }
    check_tart()
}

// --- the run ----------------------------------------------------------------

/// A smoke run in a VM whose preconditions have been checked. Made before
/// anything is built, so a missing `tart` or image, or a bad time limit,
/// fails `bundle --smoke-vm` up front.
pub struct VmSmoke {
    image: String,
    limit: Duration,
    boot: Duration,
}

impl VmSmoke {
    /// Installs the signal handling, checks `tart`, the time limits and the
    /// golden image, and removes VMs that killed runs left behind.
    pub fn preflight(image: &str, ws: &Workspace) -> Result<Self> {
        interrupt::install();
        host_checks()?;
        let limit = smoke_timeout_from_env()? + COPY_MARGIN;
        let boot = boot_timeout(std::env::var("BUGLE_SMOKE_VM_BOOT_TIMEOUT"))?;
        if !image_listed(&tart_list()?, image)? {
            bail!(
                "the Tart image {image:?} does not exist; make it once with `cargo xtask smoke-vm --prepare` \
                 (see README, Testing in a VM)"
            );
        }
        cleanup_stale(ws)?;
        Ok(Self { image: image.into(), limit, boot })
    }

    /// Runs the smoke test of `app` in a fresh clone; without `app`, of a
    /// bundle built into the run directory first. An interrupted run removes
    /// its bundle copy and keeps its logs.
    pub fn run(&self, ws: &Workspace, app: Option<&Path>) -> Result<()> {
        let since_epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let name = run_vm_name(std::process::id(), since_epoch);
        let dir = run_dir(ws, &name);
        let share = dir.join("share");
        std::fs::create_dir_all(&share).with_context(|| format!("creating {}", share.display()))?;
        let result = self.build_and_run(&name, &dir, app);
        if interrupt::pending().is_some() {
            let _ = std::fs::remove_dir_all(dir.join("dist"));
            eprintln!("logs of the interrupted run are in {}", dir.display());
        }
        result
    }

    fn build_and_run(&self, name: &str, dir: &Path, app: Option<&Path>) -> Result<()> {
        // Each run assembles its own bundle, so overlapping runs never
        // replace the bundle another VM is reading (cargo serialises the
        // builds).
        let app = match app {
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
                    smoke_vm: false,
                })?;
                dir.join("dist").join(format!("{APP_NAME}.app"))
            }
        };
        interrupt::check()?;
        if !app.join("Contents").join("MacOS").join(APP_EXECUTABLE).is_file() {
            bail!("{} is not a Bugle bundle (no Contents/MacOS/{APP_EXECUTABLE})", app.display());
        }
        let share = dir.join("share");
        let app_dir = app.parent().context("the bundle has no parent directory")?;
        let guest_run = Path::new(GUEST_DIR).join("run");
        std::fs::write(share.join("config.toml"), smoke_config(&guest_run)?)?;
        let shared_name = app.file_name().and_then(|n| n.to_str()).context("the bundle path has no UTF-8 name")?;
        let script = guest_run_script(shared_name);
        std::fs::write(share.join("run.sh"), &script)?;

        let mut vm = Vm::clone_from(&self.image, name, 5)?;
        println!("VM {name} (cloned from {}); stopped and deleted when the run ends or is interrupted", self.image);
        let shares =
            [Share { name: "app", path: app_dir, read_only: true }, Share { name: "run", path: &share, read_only: false }];
        vm.boot(&tart_run_args(name, &shares)?, &dir.join("tart-run.log"))?;
        vm.wait_ready(&format!("{GUEST_SHARES}/run/run.sh"), self.boot)?;

        println!(
            "$ tart exec {name} /bin/bash -c <{}>  (limit {}s)",
            share.join("run.sh").display(),
            self.limit.as_secs()
        );
        let mut exec = interrupt::isolate(Command::new("tart").args(["exec", name, "/bin/bash", "-c", &script]))
            .stdin(Stdio::null())
            .spawn()
            .context("running tart exec")?;
        let t0 = Instant::now();
        let status = loop {
            if let Some(status) = exec.try_wait()? {
                break status;
            }
            if let Some(sig) = interrupt::pending() {
                interrupt::stop(&mut exec);
                return Err(interrupt::Interrupted(sig).into());
            }
            if t0.elapsed() > self.limit {
                interrupt::stop(&mut exec);
                drop(vm);
                bail!(
                    "the smoke test in the VM did not finish within {}s (BUGLE_SMOKE_TIMEOUT sets the app's limit); \
                     output so far in {}",
                    self.limit.as_secs(),
                    share.display()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        drop(vm);
        interrupt::check()?;

        let log = std::fs::read_to_string(share.join("smoke.log")).unwrap_or_default();
        let artifacts = share.join("artifacts");
        if !status.success() {
            bail!("smoke test failed in the VM ({status}); log and artefacts in {}", share.display());
        }
        if let Some(problem) = missing_grant(&log) {
            bail!(
                "smoke test passed in the VM but {problem}; the image {:?} lacks the grant, \
                 rebuild it with `cargo xtask smoke-vm --prepare --force`",
                self.image
            );
        }
        println!(
            "smoke test passed in the VM in {:.1}s; artefacts in {}",
            t0.elapsed().as_secs_f32(),
            artifacts.display()
        );
        Ok(())
    }
}

/// The run directory of the VM called `name` (the same name).
fn run_dir(ws: &Workspace, name: &str) -> PathBuf {
    ws.target_dir.join("xtask").join("smoke-vm").join(name)
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

// --- leftovers of killed runs ----------------------------------------------

/// The start time (milliseconds since the epoch) and owning pid in a run
/// clone's name, for exactly the names [`run_vm_name`] makes; `None` for
/// every other VM, which this tool never touches.
fn parse_run_name(name: &str) -> Option<(u64, u32)> {
    let (ms, pid) = name.strip_prefix(RUN_PREFIX)?.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(ms) || !digits(pid) {
        return None;
    }
    Some((ms.parse().ok()?, pid.parse().ok()?))
}

/// A run clone found by [`stale_runs`].
#[derive(Debug, PartialEq)]
struct RunVm {
    name: String,
    pid: u32,
    running: bool,
}

/// The run clones in `tart list --format json` output, split into those
/// whose owning run is gone (stale) and those still owned by a live run.
/// `owner_alive(pid, started_ms)` decides; nothing else is listed.
fn stale_runs(list_json: &str, owner_alive: impl Fn(u32, u64) -> bool) -> Result<(Vec<RunVm>, Vec<RunVm>)> {
    let v: serde_json::Value = serde_json::from_str(list_json).context("parsing tart list --format json")?;
    let entries = v.as_array().context("tart list --format json: expected an array")?;
    let (mut stale, mut live) = (Vec::new(), Vec::new());
    for e in entries {
        // Only local VMs: an OCI image is never a run clone.
        if e["Source"].as_str() != Some("local") {
            continue;
        }
        let Some(name) = e["Name"].as_str() else { continue };
        let Some((started_ms, pid)) = parse_run_name(name) else { continue };
        let vm = RunVm { name: name.into(), pid, running: e["Running"].as_bool().unwrap_or(true) };
        if owner_alive(pid, started_ms) {
            live.push(vm);
        } else {
            stale.push(vm);
        }
    }
    Ok((stale, live))
}

/// Whether the run that named its clone with `pid` and `started_ms` may
/// still be going: the pid exists, its process started no later than the
/// run (else the pid was reused) and it is an `xtask`. When the process
/// cannot be inspected, it is assumed alive, so a clone is removed only
/// when its owner is certainly gone.
fn owner_alive(pid: u32, started_ms: u64) -> bool {
    if pid == std::process::id() {
        return true;
    }
    let Ok(p) = libc::pid_t::try_from(pid) else { return false };
    if p <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(p, 0) } != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return false;
    }
    match process_info(p) {
        Some((start_ms, comm)) => owner_matches(start_ms, &comm, started_ms),
        None => true,
    }
}

/// [`owner_alive`] for a process that exists, from its start time and name.
fn owner_matches(process_start_ms: u64, comm: &str, run_started_ms: u64) -> bool {
    process_start_ms <= run_started_ms && comm == "xtask"
}

/// A process's start time (milliseconds since the epoch) and short name.
fn process_info(pid: libc::pid_t) -> Option<(u64, String)> {
    // SAFETY: proc_pidinfo fills at most `size` bytes of the zeroed struct.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
    if n != size {
        return None;
    }
    let start_ms = info.pbi_start_tvsec * 1000 + info.pbi_start_tvusec / 1000;
    let comm: Vec<u8> = info.pbi_comm.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    Some((start_ms, String::from_utf8_lossy(&comm).into_owned()))
}

/// Stops and deletes the run clones of runs that are gone (killed before
/// they could clean up), and removes their bundle copies. Clones of live
/// runs, and every VM not named by [`run_vm_name`], are left alone.
fn cleanup_stale(ws: &Workspace) -> Result<()> {
    let (stale, live) = stale_runs(&tart_list()?, owner_alive)?;
    for vm in &live {
        if vm.pid != std::process::id() {
            println!("VM {} belongs to a run still going (pid {}); left alone", vm.name, vm.pid);
        }
    }
    for vm in &stale {
        println!("removing VM {}, left by a run that is gone (pid {})", vm.name, vm.pid);
        if vm.running {
            // Its `tart run` (and any `tart exec`) exit once the VM stops.
            let _ = run_quiet(Command::new("tart").args(["stop", "--timeout", "5", &vm.name]), false);
        }
        run_quiet(Command::new("tart").args(["delete", &vm.name]), false)
            .with_context(|| format!("removing the stale VM {}", vm.name))?;
        let _ = std::fs::remove_dir_all(run_dir(ws, &vm.name).join("dist"));
    }
    if stale.is_empty() {
        println!("no VMs left over by earlier runs");
    }
    Ok(())
}

// --- the golden image -------------------------------------------------------

fn prepare(args: &SmokeVmArgs, ws: &Workspace) -> Result<()> {
    let boot = boot_timeout(std::env::var("BUGLE_SMOKE_VM_BOOT_TIMEOUT"))?;
    let host_version = command_stdout(Command::new("sw_vers").arg("-productVersion"))?;
    let from = match &args.from {
        Some(f) => f.clone(),
        None => match default_base_image(&host_version) {
            Some(img) => img,
            None => bail!("no Cirrus Labs image is known for macOS {host_version}; name one with --from"),
        },
    };
    if let Some(guest) = image_macos_major(&from) {
        guest_fits_host(&from, guest, macos_major(&host_version)?)?;
    }
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
        run_quiet(Command::new("tart").args(["delete", &args.image]), false)?;
    }
    // Deleted again unless every step below succeeds.
    let mut vm = Vm::clone_from(&from, &args.image, 120)?;
    let log = ws.target_dir.join("xtask").join("smoke-vm").join(format!("prepare-{}.log", args.image));
    std::fs::create_dir_all(log.parent().expect("has a parent"))?;
    vm.boot(&tart_run_args(&args.image, &[])?, &log)?;
    vm.wait_ready("/", boot)?;

    // Read on the host, so the decision is unit-tested on captured output.
    let sip = vm.exec_stdout(&["/usr/bin/csrutil", "status"])?;
    let version = vm.exec_stdout(&["/usr/bin/sw_vers", "-productVersion"])?;
    println!("guest: macOS {version}; {}", sip.lines().next().unwrap_or("").trim());
    let major = macos_major(&version)?;
    let allow_untested = std::env::var_os(UNTESTED_GUEST_VAR).is_some_and(|v| v == "1");
    let user_db = grant_plan(&from, &parse_sip(&sip), &sip, major, allow_untested)?;

    let exe = Path::new(GUEST_DIR).join(format!("{APP_NAME}.app")).join("Contents").join("MacOS").join(APP_EXECUTABLE);
    let script = guest_prepare_script(&ws.bundle_id, &exe.display().to_string(), user_db);
    println!("$ tart exec {} /bin/bash -c <grant script>", args.image);
    let status = interrupt::status(Command::new("tart").args(["exec", &args.image, "/bin/bash", "-c", &script]).stdin(Stdio::null()))
        .context("running tart exec")?;
    if !status.success() {
        bail!("{}", prepare_failure(status.code(), &from, major));
    }
    // A clean shutdown, so the TCC writes reach the disk image.
    vm.keep = true;
    drop(vm);
    interrupt::check()?;
    println!("prepared {:?} from {from}; `cargo xtask smoke-vm` now clones it for each run", args.image);
    Ok(())
}

/// What `csrutil status` says about SIP in the guest.
#[derive(Debug, PartialEq)]
enum Sip {
    Disabled,
    Enabled,
    /// "unknown (Custom Configuration)": only Filesystem Protections matter
    /// for writing TCC.db.
    Custom { filesystem_protected: bool },
    /// Output this tool does not recognise.
    Unknown,
}

fn parse_sip(out: &str) -> Sip {
    let value = |key: &str| {
        out.lines().find_map(|l| l.trim().strip_prefix(key)).map(|v| v.trim().trim_end_matches('.').to_ascii_lowercase())
    };
    let Some(status) = value("System Integrity Protection status:") else { return Sip::Unknown };
    if status.starts_with("disabled") {
        Sip::Disabled
    } else if status.starts_with("enabled") {
        Sip::Enabled
    } else if status.contains("custom configuration") {
        match value("Filesystem Protections:").as_deref() {
            Some("disabled") => Sip::Custom { filesystem_protected: false },
            Some("enabled") => Sip::Custom { filesystem_protected: true },
            _ => Sip::Unknown,
        }
    } else {
        Sip::Unknown
    }
}

/// Where the guest keeps the user TCC database.
#[derive(Debug, Clone, Copy, PartialEq)]
enum UserDb {
    /// `~/Library/Application Support/com.apple.TCC/TCC.db` (macOS 14–26).
    Home,
    /// A ProtectedSystem container on macOS 27, found through `tccd`'s
    /// open files.
    Tccd,
}

/// Whether `--prepare` can grant the permissions in a guest with this SIP
/// state and macOS major, and where its user database is; otherwise the
/// cause and the fix. `from` names the image in the messages.
fn grant_plan(from: &str, sip: &Sip, sip_raw: &str, major: u32, allow_untested: bool) -> Result<UserDb> {
    let recovery = "disable it in the guest by hand: `tart run --recovery <vm>`, Options → Utilities → Terminal, \
                    `csrutil disable`, `halt`; then pass that VM with --from. Never on the host";
    match sip {
        Sip::Disabled | Sip::Custom { filesystem_protected: false } => {}
        Sip::Enabled => bail!(
            "SIP is enabled inside {from}, so its TCC databases cannot be written. Use a Cirrus Labs \
             `-base` or `-xcode` image (they ship with SIP disabled in the guest), or {recovery}."
        ),
        Sip::Custom { filesystem_protected: true } => bail!(
            "SIP inside {from} is a custom configuration that keeps Filesystem Protections on, so its TCC \
             databases cannot be written. Turn SIP off completely: {recovery}."
        ),
        Sip::Unknown => bail!(
            "`csrutil status` in {from} printed something this tool does not recognise, so it cannot tell \
             whether TCC.db is writable:\n{}",
            sip_raw.trim()
        ),
    }
    let (lo, hi) = (*KNOWN_GUESTS.start(), *KNOWN_GUESTS.end());
    if major < lo {
        bail!("{from} runs macOS {major}; `--prepare` supports guests from macOS {lo} (use a newer image with --from)");
    }
    if major > hi && !allow_untested {
        bail!(
            "{from} runs macOS {major}, newer than any guest this tool knows (macOS {lo}–{hi}); where its user \
             TCC database lives is unknown, so the grants may not apply. Use an image of macOS {lo}–{hi} with \
             --from, or set {UNTESTED_GUEST_VAR}=1 to try the macOS {hi} layout and check that the run's \
             recording step is not skipped."
        );
    }
    Ok(if major < 27 { UserDb::Home } else { UserDb::Tccd })
}

/// The error for a failed `--prepare` script, from its exit code.
fn prepare_failure(code: Option<i32>, from: &str, major: u32) -> String {
    match code {
        Some(EXIT_NO_USER_DB) => format!(
            "could not find the user TCC database in {from} (macOS {major}) among tccd's open files; \
             its layout may have changed (see grant_plan in xtask/src/smoke_vm.rs)"
        ),
        Some(EXIT_MISSING_DB) => format!("a TCC database is missing in {from} (macOS {major}); see the output above"),
        Some(EXIT_SQLITE) => format!(
            "sqlite3 could not write a TCC database in {from} (macOS {major}) although SIP reports it \
             writable; see the error above"
        ),
        Some(c) => format!("granting the permissions in the guest failed (exit status {c})"),
        None => "granting the permissions in the guest was killed by a signal".into(),
    }
}

/// The `--prepare` script: grant [`TCC_SERVICES`] to Bugle by bundle id and
/// by the guest executable's path, in the system and the user TCC database
/// (the way the Cirrus Labs images grant their guest agent), and mark the
/// screen-capture approval as fresh for macOS 15's periodic re-confirmation
/// (best effort). [`grant_plan`] has checked SIP and the macOS version.
fn guest_prepare_script(bundle_id: &str, exe: &str, user_db: UserDb) -> String {
    let sql = tcc_sql(bundle_id, exe);
    let find_user_db = match user_db {
        UserDb::Home => r#"user_db="$HOME/Library/Application Support/com.apple.TCC/TCC.db""#.to_string(),
        UserDb::Tccd => format!(
            r#"# macOS 27 keeps the user database in a ProtectedSystem container; ask tccd.
user_db=$(sudo lsof -a -u "$(id -u)" -c tccd -Fn | sed -n 's|^n\(/private/var/containers/Data/ProtectedSystem/.*/com\.apple\.TCC/TCC\.db\)$|\1|p' | sort -u)
if [ "$(printf '%s\n' "$user_db" | grep -c .)" != 1 ]; then echo "cannot find the user TCC database" >&2; exit {EXIT_NO_USER_DB}; fi"#
        ),
    };
    format!(
        r#"#!/bin/bash
# Written by `cargo xtask smoke-vm --prepare`; runs inside the VM.
set -euo pipefail
{find_user_db}
for db in "/Library/Application Support/com.apple.TCC/TCC.db" "$user_db"; do
  sudo test -f "$db" || {{ echo "missing $db" >&2; exit {EXIT_MISSING_DB}; }}
  if ! err=$(sudo sqlite3 "$db" {sql} 2>&1); then echo "sqlite3 $db: $err" >&2; exit {EXIT_SQLITE}; fi
  echo "granted in $db"
done
approvals="$HOME/Library/Group Containers/group.com.apple.replayd/ScreenCaptureApprovals"
defaults write "$approvals" {exe} -date "2100-01-01 00:00:00 +0000" 2>/dev/null || true
"#,
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

/// The major version in `sw_vers -productVersion` output.
fn macos_major(product_version: &str) -> Result<u32> {
    let v = product_version.trim();
    v.split('.').next().and_then(|m| m.parse().ok()).with_context(|| format!("not a macOS version: {v:?}"))
}

/// `ghcr.io/cirruslabs/macos-<name>-base:latest` for the host's macOS
/// (`sw_vers -productVersion`), when Cirrus Labs publishes one.
fn default_base_image(product_version: &str) -> Option<String> {
    let major = macos_major(product_version).ok()?;
    let (_, name) = MACOS_NAMES.iter().find(|(m, _)| *m == major)?;
    Some(format!("ghcr.io/cirruslabs/macos-{name}-base:latest"))
}

/// The macOS major of a Cirrus Labs image, from its name
/// (`…/macos-<name>-<flavour>[:tag]`); `None` for any other name.
fn image_macos_major(image: &str) -> Option<u32> {
    let last = image.rsplit('/').next()?;
    let repo = last.split([':', '@']).next()?;
    let rest = repo.strip_prefix("macos-")?;
    MACOS_NAMES
        .iter()
        .find(|(_, name)| rest.strip_prefix(name).is_some_and(|r| r.is_empty() || r.starts_with('-')))
        .map(|(m, _)| *m)
}

/// Virtualization.framework runs a macOS guest only up to the host's own
/// major version; a newer one would fail to boot with a framework error.
fn guest_fits_host(from: &str, guest: u32, host: u32) -> Result<()> {
    if guest > host {
        let pull = MACOS_NAMES
            .iter()
            .find(|(m, _)| *m == host)
            .map(|(_, n)| format!("pull ghcr.io/cirruslabs/macos-{n}-base:latest and pass it with --from, or "))
            .unwrap_or_default();
        bail!(
            "{from} is macOS {guest}, newer than this Mac (macOS {host}); Virtualization.framework runs macOS \
             guests only up to the host's own version. {pull}update this Mac first"
        );
    }
    Ok(())
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

/// [`tart_list`] that is not cut short by a signal, for the teardown.
fn tart_list_teardown() -> Result<String> {
    let out = interrupt::output(Command::new("tart").args(["list", "--format", "json"]).stdin(Stdio::null()), true, false)?;
    if !out.status.success() {
        bail!("tart list failed ({})", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
/// digits and hyphens. [`parse_run_name`] reads it back.
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

/// A Tart VM this process created: on drop it is stopped (if this process
/// booted it) and, unless `keep` was set, deleted. Both are waited for even
/// after a signal, so a second Ctrl-C does not leave the clone behind.
struct Vm {
    name: String,
    run: Option<Child>,
    /// The `tart run` output, quoted when it exits early.
    log: Option<PathBuf>,
    keep: bool,
    /// Seconds `tart stop` waits for a clean shutdown before forcing it.
    stop_timeout: u32,
}

impl Vm {
    /// Clones `source` to `name`. The guard exists before the clone starts,
    /// so a clone that fails halfway is removed too; the clone itself (about
    /// a second, copy-on-write) always finishes, and a signal that arrived
    /// meanwhile is reported after it.
    fn clone_from(source: &str, name: &str, stop_timeout: u32) -> Result<Self> {
        let vm = Self { name: name.into(), run: None, log: None, keep: false, stop_timeout };
        run_quiet(Command::new("tart").args(["clone", source, name]), false)
            .with_context(|| format!("cloning {source} to {name}"))?;
        interrupt::check()?;
        Ok(vm)
    }

    fn boot(&mut self, args: &[OsString], log: &Path) -> Result<()> {
        let out = std::fs::File::create(log).with_context(|| format!("creating {}", log.display()))?;
        println!(
            "$ tart {}  (output in {})",
            args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "),
            log.display()
        );
        let child = interrupt::isolate(Command::new("tart").args(args))
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .spawn()
            .context("running tart run")?;
        self.run = Some(child);
        self.log = Some(log.to_path_buf());
        Ok(())
    }

    /// Waits until the guest agent answers and `path` exists in the guest
    /// (the shares appear once the user session has logged in).
    fn wait_ready(&mut self, path: &str, limit: Duration) -> Result<()> {
        let t0 = Instant::now();
        loop {
            interrupt::check()?;
            if let Some(status) = self.run.as_mut().and_then(|c| c.try_wait().transpose()).transpose()? {
                let tail = self.log.as_deref().map(log_tail).unwrap_or_default();
                bail!(
                    "tart run exited ({status}) before the VM was ready. If the image's macOS is newer than \
                     this Mac's, Virtualization.framework cannot run it. Its log ends:\n{tail}"
                );
            }
            let probe = interrupt::output(
                Command::new("tart").args(["exec", &self.name, "/bin/test", "-e", path]).stdin(Stdio::null()),
                true,
                true,
            )?;
            if probe.status.success() {
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
            interrupt::sleep(Duration::from_secs(2))?;
        }
    }

    /// The trimmed standard output of a command run in the guest.
    fn exec_stdout(&self, argv: &[&str]) -> Result<String> {
        let out = interrupt::output(Command::new("tart").arg("exec").arg(&self.name).args(argv).stdin(Stdio::null()), true, true)?;
        if !out.status.success() {
            bail!("`{}` in the guest failed ({}): {}", argv.join(" "), out.status, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if let Some(sig) = interrupt::pending() {
            eprintln!("{}: stopping and deleting VM {}", interrupt::Interrupted(sig), self.name);
        }
        if let Some(mut child) = self.run.take() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = run_quiet(
                    Command::new("tart").args(["stop", "--timeout", &self.stop_timeout.to_string(), &self.name]),
                    false,
                );
            }
            interrupt::stop(&mut child);
        }
        if self.keep {
            return;
        }
        // Not when the clone never appeared (it failed before creating it).
        let exists = tart_list_teardown().and_then(|l| image_listed(&l, &self.name)).unwrap_or(true);
        if exists {
            if let Err(e) = run_quiet(Command::new("tart").args(["delete", &self.name]), false) {
                eprintln!("could not delete VM {}: {e:#}; remove it with `tart delete {}`", self.name, self.name);
            }
        }
    }
}

// --- helpers ----------------------------------------------------------------

/// The last lines of a log file, indented.
fn log_tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(15)..].iter().map(|l| format!("  {l}\n")).collect()
}

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
/// it fails. Only an `interruptible` one is cut short by a signal; changes
/// to Tart's VMs (clone, stop, delete) always run to the end.
fn run_quiet(cmd: &mut Command, interruptible: bool) -> Result<()> {
    let shown: Vec<String> =
        std::iter::once(cmd.get_program()).chain(cmd.get_args()).map(|s| s.to_string_lossy().into_owned()).collect();
    println!("$ {}", shown.join(" "));
    let out = interrupt::output(cmd.stdin(Stdio::null()), true, interruptible)
        .with_context(|| format!("running {}", shown[0]))?;
    if !out.status.success() {
        bail!("{} failed ({}): {}", shown.join(" "), out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn command_stdout(cmd: &mut Command) -> Result<String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let out = interrupt::output(cmd.stdin(Stdio::null()).stderr(Stdio::inherit()), false, true)
        .with_context(|| format!("running {program}"))?;
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
        assert_eq!(parse_run_name(&a), Some((1_700_000_000_123, 42)));
    }

    #[test]
    fn only_run_names_parse() {
        for other in [
            GOLDEN_IMAGE,
            "bugle-smoke-run-",
            "bugle-smoke-run-123",
            "bugle-smoke-run-123-",
            "bugle-smoke-run--42",
            "bugle-smoke-run-123-42-x",
            "bugle-smoke-run-12a-42",
            "bugle-smoke-run-123-+42",
            "my-bugle-smoke-run-123-42",
            "bugle-smoke-run-99999999999999999999999-1",
            "ghcr.io/cirruslabs/macos-tahoe-base:latest",
        ] {
            assert_eq!(parse_run_name(other), None, "{other}");
        }
    }

    /// `tart list --format json` as Tart 2.42 prints it (captured), plus run
    /// clones: one of a live run, one of a killed run still running, one
    /// stopped, and look-alikes that must never be touched.
    const LIST: &str = r#"[
  {"Source" : "local", "Disk" : 50, "Size" : 32, "State" : "stopped", "Accessed" : "2026-11-19T03:25:10Z", "Running" : false, "Name" : "bugle-smoke-base"},
  {"Source" : "OCI", "Disk" : 50, "Size" : 32, "State" : "stopped", "Accessed" : "2026-11-19T03:23:43Z", "Running" : false, "Name" : "ghcr.io\/cirruslabs\/macos-tahoe-base:latest"},
  {"Source" : "local", "Disk" : 50, "Size" : 32, "State" : "running", "Running" : true, "Name" : "bugle-smoke-run-1000-11"},
  {"Source" : "local", "Disk" : 50, "Size" : 32, "State" : "running", "Running" : true, "Name" : "bugle-smoke-run-2000-22"},
  {"Source" : "local", "Disk" : 50, "Size" : 32, "State" : "stopped", "Running" : false, "Name" : "bugle-smoke-run-3000-33"},
  {"Source" : "local", "Disk" : 50, "Size" : 32, "State" : "stopped", "Running" : false, "Name" : "bugle-smoke-run-3000-33-mine"},
  {"Source" : "local", "Disk" : 50, "Size" : 32, "State" : "stopped", "Running" : false, "Name" : "someone-elses-vm"},
  {"Source" : "OCI", "Disk" : 50, "Size" : 32, "State" : "stopped", "Running" : false, "Name" : "example.com\/bugle-smoke-run-4000-44"}
]"#;

    #[test]
    fn stale_runs_are_the_dead_owners_only() {
        let mut asked = Vec::new();
        let asked_cell = std::cell::RefCell::new(&mut asked);
        let (stale, live) = stale_runs(LIST, |pid, ms| {
            asked_cell.borrow_mut().push((pid, ms));
            pid == 11
        })
        .unwrap();
        let names = |v: &[RunVm]| v.iter().map(|r| (r.name.clone(), r.pid, r.running)).collect::<Vec<_>>();
        assert_eq!(names(&live), [("bugle-smoke-run-1000-11".to_string(), 11, true)]);
        assert_eq!(
            names(&stale),
            [("bugle-smoke-run-2000-22".to_string(), 22, true), ("bugle-smoke-run-3000-33".to_string(), 33, false)]
        );
        assert_eq!(asked, [(11, 1000), (22, 2000), (33, 3000)], "only run clones are looked at");
        assert!(stale_runs("{}", |_, _| false).is_err());
    }

    #[test]
    fn owners_are_checked_by_pid_start_time_and_name() {
        // This test process: started before now, named after the test binary.
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let (start, comm) = process_info(std::process::id() as libc::pid_t).unwrap();
        assert!(start <= now_ms && start > now_ms - 3_600_000, "{start} vs {now_ms}");
        assert!(comm.starts_with("xtask"), "{comm}");
        assert!(owner_alive(std::process::id(), 0), "this process is always alive");
        // A process that has exited is not.
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!owner_alive(pid, now_ms + 60_000));
        // A live process that is not an xtask (a reused pid) is not either.
        let mut sleeper = Command::new("/bin/sleep").arg("5").spawn().unwrap();
        assert!(!owner_alive(sleeper.id(), now_ms + 60_000));
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        assert!(owner_matches(1_000, "xtask", 1_000));
        assert!(!owner_matches(1_001, "xtask", 1_000), "started after the run: the pid was reused");
        assert!(!owner_matches(1_000, "zsh", 2_000));
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
        assert!(image_listed(LIST, "bugle-smoke-base").unwrap());
        assert!(image_listed(LIST, "ghcr.io/cirruslabs/macos-tahoe-base:latest").unwrap());
        assert!(image_listed(LIST, "ghcr.io/cirruslabs/macos-tahoe-base").unwrap());
        assert!(!image_listed(LIST, "ghcr.io/cirruslabs/macos-tahoe-base:2025").unwrap());
        assert!(!image_listed(LIST, "bugle-smoke").unwrap());
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
    fn image_names_give_their_macos() {
        assert_eq!(image_macos_major("ghcr.io/cirruslabs/macos-tahoe-base:latest"), Some(26));
        assert_eq!(image_macos_major("ghcr.io/cirruslabs/macos-golden-gate-xcode:27.0"), Some(27));
        assert_eq!(image_macos_major("ghcr.io/cirruslabs/macos-sequoia-base@sha256:abc"), Some(15));
        assert_eq!(image_macos_major("ghcr.io/cirruslabs/macos-sonoma"), Some(14));
        for none in ["bugle-smoke-base", "my-vm", "ghcr.io/cirruslabs/macos-tahoeish-base", "macos-golden-base"] {
            assert_eq!(image_macos_major(none), None, "{none}");
        }
    }

    #[test]
    fn guests_newer_than_the_host_are_refused_up_front() {
        guest_fits_host("img", 26, 26).unwrap();
        guest_fits_host("img", 15, 26).unwrap();
        let e = guest_fits_host("ghcr.io/cirruslabs/macos-golden-gate-base:latest", 27, 26).unwrap_err().to_string();
        assert!(e.contains("macOS 27, newer than this Mac (macOS 26)"), "{e}");
        assert!(e.contains("Virtualization.framework"), "{e}");
        assert!(e.contains("macos-tahoe-base:latest"), "names the image for this Mac: {e}");
        let e = guest_fits_host("img", 27, 25).unwrap_err().to_string();
        assert!(!e.contains("pull"), "no image is known for macOS 25: {e}");
    }

    /// `sw_vers -productVersion` and `csrutil status` as captured: the
    /// disabled one from the `bugle-smoke-base` guest (Cirrus Labs
    /// macos-tahoe-base), the enabled one from a stock macOS 26 host.
    const SW_VERS_GUEST: &str = "26.6.2\n";
    const SIP_DISABLED: &str = "System Integrity Protection status: disabled.\n";
    const SIP_ENABLED: &str = "System Integrity Protection status: enabled.\n";
    /// The layout `csrutil` prints for a partial `csrutil enable --without …`
    /// (not captured: no such image here).
    const SIP_CUSTOM_FS_OFF: &str = "System Integrity Protection status: unknown (Custom Configuration).\n\n\
        Configuration:\n\tApple Internal: disabled\n\tKext Signing: enabled\n\tFilesystem Protections: disabled\n\
        \tDebugging Restrictions: enabled\n\tDTrace Restrictions: enabled\n\tNVRAM Protections: enabled\n\
        \tBaseSystem Verification: enabled\n\nThis is an unsupported configuration, likely to break in the future \
        and leave your machine in an unknown state.\n";

    #[test]
    fn sip_states_parse() {
        assert_eq!(parse_sip(SIP_DISABLED), Sip::Disabled);
        assert_eq!(parse_sip(SIP_ENABLED), Sip::Enabled);
        assert_eq!(parse_sip(SIP_CUSTOM_FS_OFF), Sip::Custom { filesystem_protected: false });
        let fs_on = SIP_CUSTOM_FS_OFF.replace("Filesystem Protections: disabled", "Filesystem Protections: enabled");
        assert_eq!(parse_sip(&fs_on), Sip::Custom { filesystem_protected: true });
        // The old in-guest check (`grep -q disabled`) took this for "off".
        assert!(fs_on.contains("disabled"));
        let no_fs = SIP_CUSTOM_FS_OFF.replace("\tFilesystem Protections: disabled\n", "");
        assert_eq!(parse_sip(&no_fs), Sip::Unknown);
        for odd in ["", "csrutil: command not found", "System Integrity Protection status: maybe."] {
            assert_eq!(parse_sip(odd), Sip::Unknown, "{odd:?}");
        }
        assert_eq!(macos_major(SW_VERS_GUEST).unwrap(), 26);
        assert!(macos_major("").is_err() && macos_major("x.1").is_err());
    }

    #[test]
    fn grant_plans() {
        let plan = |raw: &str, major, untested| grant_plan("img", &parse_sip(raw), raw, major, untested);
        assert_eq!(plan(SIP_DISABLED, 26, false).unwrap(), UserDb::Home);
        assert_eq!(plan(SIP_DISABLED, 14, false).unwrap(), UserDb::Home);
        assert_eq!(plan(SIP_DISABLED, 27, false).unwrap(), UserDb::Tccd);
        assert_eq!(plan(SIP_CUSTOM_FS_OFF, 15, false).unwrap(), UserDb::Home);

        let err = |raw: &str, major, untested| plan(raw, major, untested).unwrap_err().to_string();
        let e = err(SIP_ENABLED, 26, false);
        assert!(e.contains("SIP is enabled inside img") && e.contains("csrutil disable") && e.contains("--recovery"), "{e}");
        let fs_on = SIP_CUSTOM_FS_OFF.replace("Filesystem Protections: disabled", "Filesystem Protections: enabled");
        let e = err(&fs_on, 26, false);
        assert!(e.contains("Filesystem Protections on") && e.contains("csrutil disable"), "{e}");
        let e = err("csrutil: command not found", 26, false);
        assert!(e.contains("does not recognise") && e.contains("csrutil: command not found"), "{e}");
        // SIP is reported before the version.
        assert!(err(SIP_ENABLED, 28, false).contains("SIP is enabled"));

        let e = err(SIP_DISABLED, 28, false);
        assert!(e.contains("macOS 28, newer than any guest this tool knows (macOS 14–27)"), "{e}");
        assert!(e.contains("BUGLE_SMOKE_VM_UNTESTED_GUEST=1"), "{e}");
        assert_eq!(plan(SIP_DISABLED, 28, true).unwrap(), UserDb::Tccd, "the override tries the newest layout");
        assert!(err(SIP_DISABLED, 13, true).contains("from macOS 14"));
    }

    #[test]
    fn prepare_failures_name_the_cause() {
        assert!(prepare_failure(Some(EXIT_NO_USER_DB), "img", 27).contains("user TCC database"));
        assert!(prepare_failure(Some(EXIT_MISSING_DB), "img", 26).contains("missing"));
        assert!(prepare_failure(Some(EXIT_SQLITE), "img", 26).contains("sqlite3 could not write"));
        assert!(prepare_failure(Some(1), "img", 26).contains("exit status 1"));
        assert!(prepare_failure(None, "img", 26).contains("signal"));
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
        let home = guest_prepare_script("io.example.bugle", "/p", UserDb::Home);
        assert!(home.contains("/Library/Application Support/com.apple.TCC/TCC.db"));
        assert!(home.contains(r#"user_db="$HOME/Library/Application Support/com.apple.TCC/TCC.db""#), "{home}");
        assert!(!home.contains("lsof") && !home.contains("csrutil"), "{home}");
        assert!(home.contains(&format!("exit {EXIT_SQLITE}")) && home.contains(&format!("exit {EXIT_MISSING_DB}")));
        let tccd = guest_prepare_script("io.example.bugle", "/p", UserDb::Tccd);
        assert!(tccd.contains("-c tccd") && tccd.contains(&format!("exit {EXIT_NO_USER_DB}")), "{tccd}");
    }

    #[test]
    fn flags_parse() {
        use clap::Parser;
        let parse =
            |args: &[&str]| crate::Cli::try_parse_from(["cargo xtask", "smoke-vm"].iter().chain(args)).map(|c| c.cmd);
        let Ok(crate::Cmd::SmokeVm(a)) = parse(&[]) else { panic!() };
        assert!(!a.prepare && !a.cleanup && a.app.is_none());
        // The env var may be set by whoever runs the tests; the flag wins.
        let Ok(crate::Cmd::SmokeVm(a)) = parse(&["--prepare", "--from", "img", "--force", "--image", "g"]) else {
            panic!()
        };
        assert!(a.prepare && a.force && a.from.as_deref() == Some("img") && a.image == "g");
        assert!(parse(&["--from", "img"]).is_err(), "--from needs --prepare");
        assert!(parse(&["--force"]).is_err(), "--force needs --prepare");
        assert!(parse(&["--prepare", "--app", "x"]).is_err());
        let Ok(crate::Cmd::SmokeVm(a)) = parse(&["--cleanup"]) else { panic!() };
        assert!(a.cleanup);
        assert!(parse(&["--cleanup", "--prepare"]).is_err() && parse(&["--cleanup", "--app", "x"]).is_err());
    }

    #[test]
    fn scripts_parse() {
        for script in [
            guest_run_script("Bugle.app"),
            guest_prepare_script("io.example.bugle", "/a b/bugle-app", UserDb::Home),
            guest_prepare_script("io.example.bugle", "/a b/bugle-app", UserDb::Tccd),
        ] {
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
