//! SIGINT, SIGTERM, SIGHUP and SIGQUIT for the commands that own a Tart VM
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
//! Ctrl-C does not cut the cleanup short, but every such wait has a hard
//! limit ([`Wait::teardown`]): a hung `tart` is stopped and [`TimedOut`]
//! returned rather than leaving SIGKILL as the only way out. `main` exits
//! with 128 + the signal. A signal the process inherited as ignored (`nohup`
//! ignores SIGHUP) stays ignored.
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

/// How a wait ends early: on a signal (`interruptible`) and/or after `limit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    pub interruptible: bool,
    pub limit: Option<Duration>,
}

impl Wait {
    /// Cut short by a signal, otherwise unbounded (builds, the notary).
    pub const INTERRUPTIBLE: Wait = Wait { interruptible: true, limit: None };

    /// Cut short by a signal or after `limit`.
    pub fn within(limit: Duration) -> Wait {
        Wait { interruptible: true, limit: Some(limit) }
    }

    /// Not cut short by a signal (cleanup), but never longer than `limit`.
    pub fn teardown(limit: Duration) -> Wait {
        Wait { interruptible: false, limit: Some(limit) }
    }
}

/// The error a wait returns when its child outlived the limit (the child's
/// group has been stopped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedOut(pub Duration);

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "did not finish within {}s and was stopped", self.0.as_secs())
    }
}

impl std::error::Error for TimedOut {}

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
        libc::SIGQUIT => "SIGQUIT".into(),
        n => format!("signal {n}"),
    }
}

extern "C" fn record(sig: libc::c_int) {
    // The first signal is the one reported; the handler does nothing else.
    let _ = SIGNAL.compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst);
}

/// The signals [`install`] records.
const SIGNALS: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// Records [`SIGNALS`] instead of dying on them, except those this process
/// inherited as ignored. Idempotent.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    for sig in SIGNALS {
        // SAFETY: sigaction with a null new action only reads the current
        // one; a zeroed sigaction with an async-signal-safe handler (one
        // atomic store) and an empty mask is a valid new action.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, std::ptr::null(), &mut old) == 0 && old.sa_sigaction == libc::SIG_IGN {
                continue;
            }
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
/// everything it started. Its stdin is then null: a background group that
/// read the terminal would be stopped by SIGTTIN.
pub fn isolate(cmd: &mut Command) -> &mut Command {
    if installed() {
        cmd.process_group(0).stdin(Stdio::null());
    }
    cmd
}

/// Waits for `child`, stopping its group and failing with [`Interrupted`]
/// or [`TimedOut`] as `how` says.
pub fn wait(child: &mut Child, how: Wait) -> Result<ExitStatus> {
    let t0 = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if how.interruptible {
            if let Some(sig) = pending() {
                stop(child);
                return Err(Interrupted(sig).into());
            }
        }
        if let Some(limit) = how.limit {
            if t0.elapsed() > limit {
                stop(child);
                return Err(TimedOut(limit).into());
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

/// `Command::status`, isolated and interruptible once the handler is
/// installed, and ended after `limit` if one is given.
pub fn status(cmd: &mut Command, limit: Option<Duration>) -> Result<ExitStatus> {
    if !installed() && limit.is_none() {
        return Ok(cmd.status()?);
    }
    let mut child = isolate(cmd).spawn()?;
    wait(&mut child, Wait { interruptible: true, limit })
}

/// `Command::output` (stdout always captured; stderr captured too, or
/// inherited when `capture_stderr` is false), isolated and ended early as
/// `how` says.
pub fn output(cmd: &mut Command, capture_stderr: bool, how: Wait) -> Result<Output> {
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
    let status = wait(&mut child, how)?;
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
        assert!(status(&mut Command::new("/usr/bin/true"), None).unwrap().success());
        let out = output(Command::new("/bin/sh").args(["-c", "echo out; echo err >&2; exit 3"]), true, Wait::INTERRUPTIBLE)
            .unwrap();
        assert_eq!((out.status.code(), &out.stdout[..], &out.stderr[..]), (Some(3), &b"out\n"[..], &b"err\n"[..]));
    }

    /// A limit stops a hung child (teardown waits too) and says so.
    #[test]
    fn limits_stop_hung_children() {
        let t0 = Instant::now();
        let e = output(Command::new("/bin/sleep").arg("30"), true, Wait::teardown(Duration::from_millis(300))).unwrap_err();
        assert_eq!(e.downcast_ref::<TimedOut>(), Some(&TimedOut(Duration::from_millis(300))));
        let e = status(Command::new("/bin/sleep").arg("30"), Some(Duration::from_millis(300))).unwrap_err();
        assert!(e.to_string().contains("did not finish within"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(5), "SIGTERM ends sleep at once");
        // Within the limit, the result is the child's.
        let out = output(Command::new("/bin/echo").arg("hi"), true, Wait::within(Duration::from_secs(30))).unwrap();
        assert_eq!(out.stdout, b"hi\n");
    }

    /// An ignored signal stays ignored (`nohup`). Run in a child process so
    /// the test binary's own dispositions are untouched.
    #[test]
    fn inherited_ignore_is_kept() {
        if std::env::var_os("BUGLE_XTASK_INSTALL_PROBE").is_some() {
            install();
            let disposition = |sig| unsafe {
                let mut old: libc::sigaction = std::mem::zeroed();
                libc::sigaction(sig, std::ptr::null(), &mut old);
                old.sa_sigaction
            };
            assert_eq!(disposition(libc::SIGHUP), libc::SIG_IGN, "SIGHUP was ignored before");
            assert_ne!(disposition(libc::SIGINT), libc::SIG_IGN);
            assert_ne!(disposition(libc::SIGINT), libc::SIG_DFL);
            assert_ne!(disposition(libc::SIGQUIT), libc::SIG_DFL);
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let ok = Command::new("/usr/bin/nohup")
            .arg(exe)
            .args(["--exact", "interrupt::tests::inherited_ignore_is_kept", "--nocapture"])
            .env("BUGLE_XTASK_INSTALL_PROBE", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&ok.stdout).into_owned() + &String::from_utf8_lossy(&ok.stderr);
        assert!(ok.status.success() && text.contains("1 passed"), "{text}");
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
