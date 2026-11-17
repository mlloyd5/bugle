//! The menu-bar app's side of window picking: the worker thread asks the
//! main thread for a pick session ([`pick`]) and waits for the answer. The
//! session itself (the window picker, the area selection and the Space
//! toggle between them) is [`bugle_engine::pick`], re-exported here; the CLI
//! drives the same sessions with a run loop of its own.

pub use bugle_engine::pick::*;

use crate::{AppEvent, Proxy, UiRequest};
use anyhow::{anyhow, Result};
use bugle_core::{BugleError, WindowInfo};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

/// After the user dismisses a picker, how long API requests for another one
/// are refused, so a client cannot raise the overlay again straight away.
pub const COOLDOWN: Duration = Duration::from_secs(3);

/// Held for a whole pick session (and by `UiHost::select_area`), so at most
/// one interactive overlay request is in flight. A second pick is refused
/// rather than queued.
static OVERLAY_GATE: Mutex<()> = Mutex::new(());

/// When the user last dismissed a pick session (see [`COOLDOWN`]).
static LAST_CANCEL: Mutex<Option<Instant>> = Mutex::new(None);

/// Take the overlay gate, waiting for it (area selection requests).
pub fn wait_gate() -> MutexGuard<'static, ()> {
    OVERLAY_GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Take the overlay gate, or refuse with [`BugleError::Busy`] when another
/// pick or selection holds it.
pub fn try_gate() -> Result<MutexGuard<'static, ()>> {
    match OVERLAY_GATE.try_lock() {
        Ok(g) => Ok(g),
        Err(TryLockError::Poisoned(e)) => Ok(e.into_inner()),
        Err(TryLockError::WouldBlock) => {
            Err(BugleError::Busy("another window pick or area selection is already open".into()).into())
        }
    }
}

/// How much of the cool-down is left at `now` after a cancel at `last`.
pub fn cooldown_left(last: Option<Instant>, now: Instant) -> Option<Duration> {
    let left = (last? + COOLDOWN).saturating_duration_since(now);
    (!left.is_zero()).then_some(left)
}

fn note_cancel(at: Instant) {
    *LAST_CANCEL.lock().unwrap_or_else(|e| e.into_inner()) = Some(at);
}

/// Refuse with [`BugleError::Busy`] while the cool-down runs.
fn check_cooldown(now: Instant) -> Result<()> {
    let last = *LAST_CANCEL.lock().unwrap_or_else(|e| e.into_inner());
    match cooldown_left(last, now) {
        Some(left) => Err(BugleError::Busy(format!(
            "the window picker was just dismissed; try again in {:.0} s",
            left.as_secs_f64().ceil()
        ))
        .into()),
        None => Ok(()),
    }
}


/// Worker thread: ask the main thread for a pick session and wait for the
/// answer. `Ok(None)` means the user cancelled. A picked window comes back
/// after [`SETTLE`]. Refused with [`BugleError::Busy`] (not queued) while
/// another pick or selection is in flight; the gate is held for the whole
/// session, so nothing can open in the gap between two of its overlays.
pub fn pick(proxy: &Proxy, start: Mode, allow_switch: bool, timeout: Duration) -> Result<Option<Picked>> {
    let _gate = try_gate()?;
    let (tx, rx) = mpsc::channel();
    let deadline = Instant::now() + START_WINDOW;
    proxy.send(AppEvent::Ui(UiRequest::PickWindow(Request { start, allow_switch, timeout, deadline, reply: tx })));
    // The overlay opens by `deadline` and the session ends by itself
    // `session_limit` later; the slack covers the hop back.
    let picked = match rx.recv_timeout(START_WINDOW + session_limit(allow_switch, timeout) + Duration::from_secs(2)) {
        Ok(Ok(p)) => p,
        Ok(Err(why)) => return Err(why.into_error()),
        Err(RecvTimeoutError::Timeout) => return Err(anyhow!("window picker did not answer")),
        Err(RecvTimeoutError::Disconnected) => return Err(anyhow!("window picker request dropped")),
    };
    match picked {
        Some(Picked::Window(_)) => std::thread::sleep(SETTLE),
        None => note_cancel(Instant::now()),
        Some(Picked::Area(_)) => {}
    }
    Ok(picked)
}

/// [`pick`] for a window only (no Space toggle), as API callers get it.
/// Also refused for [`COOLDOWN`] after the user dismissed a picker.
pub fn pick_window(proxy: &Proxy, timeout: Duration) -> Result<Option<WindowInfo>> {
    check_cooldown(Instant::now())?;
    Ok(match pick(proxy, Mode::Window, false, timeout)? {
        Some(Picked::Window(w)) => Some(w),
        // Without `allow_switch` the session never shows the area overlay.
        Some(Picked::Area(_)) | None => None,
    })
}

/// `--smoke-test`: run a session that switches window → area → window and
/// is then cancelled, all without a keyboard.
pub(crate) fn smoke(proxy: &Proxy) {
    let p = proxy.clone();
    let driver = std::thread::spawn(move || {
        for c in [Control::Switch, Control::Switch, Control::Cancel] {
            std::thread::sleep(Duration::from_millis(900));
            p.send(AppEvent::Ui(UiRequest::PickControl(c)));
        }
    });
    let t = Instant::now();
    let r = pick(proxy, Mode::Window, true, Duration::from_secs(10));
    eprintln!(
        "smoke test: pick session window → area → window → cancel → {:?} after {:.1}s",
        r.map(|p| p.is_some()),
        t.elapsed().as_secs_f64()
    );
    let _ = driver.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_pick_is_refused_not_queued() {
        let held = try_gate().expect("gate free");
        let e = try_gate().unwrap_err();
        assert!(matches!(e.downcast_ref::<BugleError>(), Some(BugleError::Busy(_))), "{e:#}");
        drop(held);
        drop(try_gate().expect("free again"));
    }

    #[test]
    fn cooldown_after_a_cancel() {
        let t0 = Instant::now();
        assert_eq!(cooldown_left(None, t0), None);
        assert_eq!(cooldown_left(Some(t0), t0 + Duration::from_secs(1)), Some(COOLDOWN - Duration::from_secs(1)));
        assert_eq!(cooldown_left(Some(t0), t0 + COOLDOWN), None);
        note_cancel(t0);
        let e = check_cooldown(t0 + Duration::from_millis(500)).unwrap_err();
        assert!(matches!(e.downcast_ref::<BugleError>(), Some(BugleError::Busy(m)) if m.contains("try again in 3 s")), "{e:#}");
        check_cooldown(t0 + COOLDOWN).unwrap();
    }
}
