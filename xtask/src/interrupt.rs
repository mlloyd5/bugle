//! SIGINT, SIGTERM and SIGHUP for the commands that own a Tart VM
//! (`smoke-vm`, and `bundle` / `verify` with `--smoke-vm`).
//!
//! Without this, Ctrl-C kills xtask at once and its guards never run, so the
//! VM clone, its `tart run` and any `tart exec` outlive it. [`install`]
//! replaces the default action with a handler that only records the signal.
//! Children started through [`isolate`] get their own process group, so a
//! Ctrl-C in the terminal reaches xtask alone; the polling waits here notice
//! the signal, stop the child's whole group (TERM, then KILL) and return
//! [`Interrupted`], and the callers' guards then stop and delete the VM in
//! order. Teardown commands are waited for without that check, so a second
//! Ctrl-C does not cut the cleanup short. `main` exits with 128 + the signal.
//!
//! When nothing installed the handler, every helper here behaves like the
//! plain `std::process` call it replaces.

use anyhow::{Context, Result};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

static SIGNAL: AtomicI32 = AtomicI32::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// How long a stopped child's group gets after SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

/// The error a wait returns once a signal has arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupted(pub i32);

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "interrupted by {}", signal_name(self.0))
    }
}

impl std::error::Error for Interrupted {}

fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGINT => "SIGINT".into(),
        libc::SIGTERM => "SIGTERM".into(),
        libc::SIGHUP => "SIGHUP".into(),
        n => format!("signal {n}"),
    }
}

extern "C" fn record(sig: libc::c_int) {
    // The first signal is the one reported; the handler does nothing else.
    let _ = SIGNAL.compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst);
}

/// Records SIGINT, SIGTERM and SIGHUP instead of dying on them. Idempotent.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: a zeroed sigaction with an async-signal-safe handler (one
        // atomic store) and an empty mask is a valid argument.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }
}

pub fn installed() -> bool {
    INSTALLED.load(Ordering::SeqCst)
}

/// The signal received, if any.
pub fn pending() -> Option<i32> {
    match SIGNAL.load(Ordering::SeqCst) {
        0 => None,
        s => Some(s),
    }
}

/// Fails with [`Interrupted`] once a signal has arrived.
pub fn check() -> Result<()> {
    match pending() {
        Some(sig) => Err(Interrupted(sig).into()),
        None => Ok(()),
    }
}

/// Sleeps for `d`, failing with [`Interrupted`] as soon as a signal arrives.
pub fn sleep(d: Duration) -> Result<()> {
    let t0 = Instant::now();
    while let Some(left) = d.checked_sub(t0.elapsed()) {
        check()?;
        std::thread::sleep(left.min(POLL));
    }
    check()
}

/// The exit code for an error caused by a signal: 128 + its number.
pub fn exit_code(err: &anyhow::Error) -> Option<u8> {
    err.chain().find_map(|e| e.downcast_ref::<Interrupted>()).map(|i| (128 + i.0) as u8)
}

/// Gives the command its own process group once the handler is installed,
/// so a terminal's Ctrl-C does not reach it and [`stop`] can end it with
/// everything it started.
pub fn isolate(cmd: &mut Command) -> &mut Command {
    if installed() {
        cmd.process_group(0);
    }
    cmd
}

/// Waits for `child`. With `interruptible`, a signal stops the child's group
/// and returns [`Interrupted`]; without it (teardown) the wait goes on.
pub fn wait(child: &mut Child, interruptible: bool) -> Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if interruptible {
            if let Some(sig) = pending() {
                stop(child);
                return Err(Interrupted(sig).into());
            }
        }
        std::thread::sleep(POLL);
    }
}

/// Ends `child` and its process group: SIGTERM, then SIGKILL after
/// [`TERM_GRACE`], and reaps it.
pub fn stop(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let pid = child.id() as libc::pid_t;
    let signal = |sig| {
        // SAFETY: plain kill(2) calls. The negative pid names the child's
        // own group (see `isolate`); if it has none, the child alone.
        unsafe {
            if libc::kill(-pid, sig) != 0 {
                libc::kill(pid, sig);
            }
        }
    };
    signal(libc::SIGTERM);
    let t0 = Instant::now();
    while t0.elapsed() < TERM_GRACE {
        if matches!(child.try_wait(), Ok(Some(_))) {
            // The leader is gone; make sure nothing it started lingers.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            return;
        }
        std::thread::sleep(POLL);
    }
    signal(libc::SIGKILL);
    let _ = child.wait();
}

/// `Command::status`, isolated and interruptible once the handler is installed.
pub fn status(cmd: &mut Command) -> Result<ExitStatus> {
    if !installed() {
        return Ok(cmd.status()?);
    }
    let mut child = isolate(cmd).spawn()?;
    wait(&mut child, true)
}

/// `Command::output` (stdout always captured; stderr captured too, or
/// inherited when `capture_stderr` is false), isolated and, with
/// `interruptible`, cut short by a signal.
pub fn output(cmd: &mut Command, capture_stderr: bool, interruptible: bool) -> Result<Output> {
    cmd.stdout(Stdio::piped());
    if capture_stderr {
        cmd.stderr(Stdio::piped());
    }
    let mut child = isolate(cmd).spawn()?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let status = wait(&mut child, interruptible)?;
    let stdout = out.join().ok().context("reading the child's output")?;
    let stderr = err.join().ok().context("reading the child's errors")?;
    Ok(Output { status, stdout, stderr })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_signal() {
        let e = anyhow::Error::from(Interrupted(libc::SIGINT)).context("waiting for the VM");
        assert_eq!(exit_code(&e), Some(130));
        assert_eq!(exit_code(&Interrupted(libc::SIGTERM).into()), Some(143));
        assert_eq!(exit_code(&anyhow::anyhow!("other")), None);
        assert_eq!(Interrupted(libc::SIGHUP).to_string(), "interrupted by SIGHUP");
    }

    /// Without the handler (tests never install it), the helpers are the
    /// plain calls.
    #[test]
    fn helpers_without_the_handler() {
        assert!(status(&mut Command::new("/usr/bin/true")).unwrap().success());
        let out = output(Command::new("/bin/sh").args(["-c", "echo out; echo err >&2; exit 3"]), true, true).unwrap();
        assert_eq!((out.status.code(), &out.stdout[..], &out.stderr[..]), (Some(3), &b"out\n"[..], &b"err\n"[..]));
    }

    /// `stop` ends a process group: the shell and the sleep it started.
    #[test]
    fn stop_ends_the_group() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        use std::io::BufRead;
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let sleeper: libc::pid_t = line.trim().parse().unwrap();
        let t0 = Instant::now();
        stop(&mut child);
        assert!(t0.elapsed() < TERM_GRACE, "SIGTERM was enough");
        // The orphaned grandchild is reaped by launchd shortly after.
        // SAFETY: kill with signal 0 only checks for existence.
        let gone = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            (unsafe { libc::kill(sleeper, 0) }) != 0
        });
        assert!(gone, "the grandchild {sleeper} survived");
    }
}
