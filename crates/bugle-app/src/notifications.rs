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
//!
//! A notice with a thumbnail carries it as the notification's attachment.
//! The notification center moves an attachment's file into its own store,
//! so each post writes a throwaway copy of the (already downscaled) toast
//! thumbnail into [`thumb_dir`] and hands that over. Copies the center did
//! not take are removed: by the shim when attaching or delivery fails, here
//! when the post is skipped, and by a sweep of anything older than
//! [`THUMB_MAX_AGE`] on each post (everything at launch).

use bugle_platform::notify::{self, NotifyState};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

static ENABLED: AtomicBool = AtomicBool::new(false);
static LAST_REFRESH: Mutex<Option<Instant>> = Mutex::new(None);
const REFRESH_EVERY: Duration = Duration::from_secs(5);
/// Thumbnail copies older than this were never taken by the notification
/// center (it moves them away at once) and are swept.
const THUMB_MAX_AGE: Duration = Duration::from_secs(10 * 60);
static THUMB_SEQ: AtomicU64 = AtomicU64::new(0);

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
            // Nothing is in flight yet: leftovers from an earlier run go.
            sweep_thumbs(&thumb_dir(), Duration::ZERO);
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
/// clicking it opens `open`. `thumb` (PNG bytes, the notice's toast thumbnail) is
/// attached as a throwaway copy.
pub fn post(title: &str, body: &str, open: Option<&Path>, thumb: Option<&[u8]>) {
    if !ENABLED.load(Ordering::Relaxed) || notify::state() != NotifyState::Authorized {
        return;
    }
    let copy = thumb.and_then(|png| {
        let dir = thumb_dir();
        sweep_thumbs(&dir, THUMB_MAX_AGE);
        write_thumb(&dir, png)
            .inspect_err(|e| tracing::debug!("notification thumbnail copy: {e}"))
            .ok()
    });
    if !notify::post(title, body, open, copy.as_deref()) {
        if let Some(c) = copy {
            let _ = std::fs::remove_file(c);
        }
    }
}

/// Where the throwaway attachment copies go.
fn thumb_dir() -> PathBuf {
    std::env::temp_dir().join("bugle-notification-thumbnails")
}

/// Writes a fresh copy of `png` into `dir` under a name no other post uses.
fn write_thumb(dir: &Path, png: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let seq = THUMB_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{}-{nanos}-{seq}.png", std::process::id()));
    std::fs::write(&path, png)?;
    Ok(path)
}

/// Removes the `.png` copies in `dir` last modified at least `max_age` ago
/// (`Duration::ZERO`: all of them). Returns how many went.
fn sweep_thumbs(dir: &Path, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let now = SystemTime::now();
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "png"))
        .filter(|p| {
            let age = std::fs::metadata(p).and_then(|m| m.modified()).map(|t| now.duration_since(t).unwrap_or_default());
            age.is_ok_and(|a| a >= max_age)
        })
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count()
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
        post("Bugle", "test", None, Some(b"png"));
        assert!(!ENABLED.load(Ordering::Relaxed));
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bugle-notify-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn each_thumbnail_copy_is_a_new_file() {
        let dir = scratch_dir("copies");
        let a = write_thumb(&dir, b"first").unwrap();
        let b = write_thumb(&dir, b"second").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(dir.as_path()));
        assert_eq!(a.extension().and_then(|x| x.to_str()), Some("png"));
        assert_eq!(std::fs::read(&a).unwrap(), b"first");
        assert_eq!(std::fs::read(&b).unwrap(), b"second");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sweep_removes_only_old_png_copies() {
        let dir = scratch_dir("sweep");
        let fresh = write_thumb(&dir, b"png").unwrap();
        let other = dir.join("keep.txt");
        std::fs::write(&other, b"not ours").unwrap();
        // Fresh copies may still be on their way to the notification center.
        assert_eq!(sweep_thumbs(&dir, THUMB_MAX_AGE), 0);
        assert!(fresh.exists());
        // At launch everything goes, but only the PNG copies.
        assert_eq!(sweep_thumbs(&dir, Duration::ZERO), 1);
        assert!(!fresh.exists());
        assert!(other.exists());
        // A missing directory is fine.
        assert_eq!(sweep_thumbs(&dir.join("missing"), Duration::ZERO), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
