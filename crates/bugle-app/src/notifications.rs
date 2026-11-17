//! System notifications alongside the toast and the quick-access panel
//! (`[general] system_notifications`). A notification is posted as well only
//! when the app runs from Bugle.app, the switch is on and macOS has
//! authorized it. Anything else — unbundled, switched off, denied, not
//! answered yet — leaves the in-app feedback alone, silently.
//!
//! While not authorized, the setting is re-read (never prompting) every few
//! seconds from the app's tick, so allowing Bugle in System Settings takes
//! effect without a restart and without losing a message to the re-read.
//!
//! `--smoke-test` never asks for permission: with the switch on it only
//! reads the current setting. Tests never touch the framework, since a test
//! binary is never a bundle.

use bugle_platform::notify::{self, NotifyState};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);
static LAST_REFRESH: Mutex<Option<Instant>> = Mutex::new(None);
const REFRESH_EVERY: Duration = Duration::from_secs(5);

/// What [`init`] does for a given launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// In-app feedback only; the framework is never touched.
    Off,
    /// Read the current authorization without prompting (smoke test).
    QueryOnly,
    /// Ask for authorization (prompts once per bundle id) and post.
    Request,
}

fn plan(enabled: bool, bundled: bool, smoke_test: bool) -> Start {
    match (enabled, bundled, smoke_test) {
        (false, _, _) | (_, false, _) => Start::Off,
        (true, true, true) => Start::QueryOnly,
        (true, true, false) => Start::Request,
    }
}

/// Whether the tick should re-read the setting: any state but authorized,
/// once `start` has run (a pending request is answered by the shim itself,
/// but a dismissed prompt may never answer).
fn needs_refresh(state: NotifyState) -> bool {
    !matches!(state, NotifyState::Authorized | NotifyState::Unavailable | NotifyState::NotStarted)
}

/// Called once at launch, after the event loop exists.
pub fn init(enabled: bool, smoke_test: bool) {
    match plan(enabled, bugle_platform::login_item::in_app_bundle(), smoke_test) {
        Start::Off => {
            if enabled {
                tracing::info!("system notifications need Bugle.app; in-app feedback only");
            }
        }
        Start::QueryOnly => {
            notify::start(false);
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(1500));
                eprintln!("smoke test: system notifications {:?} (not prompted)", notify::state());
            });
        }
        Start::Request => {
            notify::start(true);
            ENABLED.store(true, Ordering::Relaxed);
        }
    }
}

/// From the app's periodic tick: re-read the setting while not authorized.
pub fn tick() {
    if !ENABLED.load(Ordering::Relaxed) || !needs_refresh(notify::state()) {
        return;
    }
    let mut last = LAST_REFRESH.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|t| t.elapsed() < REFRESH_EVERY) {
        return;
    }
    *last = Some(Instant::now());
    notify::refresh();
}

/// Posts `title`/`body` to Notification Center when enabled and authorized;
/// clicking it opens `open`.
pub fn post(title: &str, body: &str, open: Option<&Path>) {
    if ENABLED.load(Ordering::Relaxed) {
        notify::post(title, body, open);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bundled_normal_launch_with_the_switch_on_asks() {
        assert_eq!(plan(true, true, false), Start::Request);
        assert_eq!(plan(false, true, false), Start::Off);
        assert_eq!(plan(true, false, false), Start::Off);
        assert_eq!(plan(false, false, false), Start::Off);
        // The smoke test never prompts, and with the switch off it does not
        // touch the framework at all.
        assert_eq!(plan(true, true, true), Start::QueryOnly);
        assert_eq!(plan(false, true, true), Start::Off);
        assert_eq!(plan(true, false, true), Start::Off);
    }

    #[test]
    fn every_unauthorized_answer_is_re_read() {
        for s in [NotifyState::Denied, NotifyState::NotDetermined, NotifyState::Pending] {
            assert!(needs_refresh(s), "{s:?}");
        }
        for s in [NotifyState::Authorized, NotifyState::Unavailable, NotifyState::NotStarted] {
            assert!(!needs_refresh(s), "{s:?}");
        }
    }

    #[test]
    fn unbundled_is_a_no_op() {
        init(true, false);
        tick();
        post("Bugle", "test", None);
        assert!(!ENABLED.load(Ordering::Relaxed));
    }
}
