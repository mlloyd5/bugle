//! System notifications alongside the toast (`[general]
//! system_notifications`). The toast is always shown; a notification is
//! posted as well only when the app runs from Bugle.app, the switch is on
//! and macOS has authorized it. Anything else — unbundled, switched off,
//! denied, not answered yet — is toast-only, silently.
//!
//! `--smoke-test` and tests never ask for permission: the smoke test only
//! reads the current setting, and a test binary is never a bundle.

use bugle_platform::notify::{self, NotifyState};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// What [`init`] does for a given launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// Toast only; the framework is never touched.
    Off,
    /// Read the current authorization without prompting (smoke test).
    QueryOnly,
    /// Ask for authorization (prompts once per bundle id) and post.
    Request,
}

fn plan(enabled: bool, bundled: bool, smoke_test: bool) -> Start {
    match (bundled, smoke_test, enabled) {
        (false, _, _) => Start::Off,
        (true, true, _) => Start::QueryOnly,
        (true, false, true) => Start::Request,
        (true, false, false) => Start::Off,
    }
}

/// Called once at launch, after the event loop exists.
pub fn init(enabled: bool, smoke_test: bool) {
    match plan(enabled, bugle_platform::login_item::in_app_bundle(), smoke_test) {
        Start::Off => {
            if enabled {
                tracing::info!("system notifications need Bugle.app; showing toasts only");
            }
        }
        Start::QueryOnly => {
            notify::start(false);
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                eprintln!("smoke test: system notifications {:?} (not prompted)", notify::state());
            });
        }
        Start::Request => {
            notify::start(true);
            ENABLED.store(true, Ordering::Relaxed);
        }
    }
}

/// Posts `title`/`body` to Notification Center when enabled and authorized.
pub fn post(title: &str, body: &str, open: Option<&Path>) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    match notify::state() {
        NotifyState::Authorized => {
            notify::post(title, body, open);
        }
        // Pick up a change made in System Settings for the next one.
        NotifyState::Denied => notify::refresh(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bundled_normal_launch_asks() {
        assert_eq!(plan(true, true, false), Start::Request);
        assert_eq!(plan(false, true, false), Start::Off);
        assert_eq!(plan(true, false, false), Start::Off);
        assert_eq!(plan(true, false, true), Start::Off);
        // The smoke test reads the setting even when the switch is off, and
        // never prompts.
        assert_eq!(plan(true, true, true), Start::QueryOnly);
        assert_eq!(plan(false, true, true), Start::QueryOnly);
    }

    #[test]
    fn posting_unbundled_is_a_no_op() {
        init(true, false);
        post("Bugle", "test", None);
        assert!(!ENABLED.load(Ordering::Relaxed));
    }
}
