//! System notifications (`UNUserNotificationCenter`), backed by
//! `shim/notify.m`. They need an app bundle: outside `Bugle.app` nothing here
//! touches the framework and [`start`] reports [`NotifyState::Unavailable`].
//!
//! Authorization is asynchronous: [`start`] returns at once and the answer
//! shows up in [`state`] a moment later. Only `start(true)` can show the
//! system prompt (once per bundle id); `start(false)` and [`refresh`] only
//! read the current setting.

use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::path::Path;

extern "C" {
    fn bugle_notify_start(request: c_int) -> c_int;
    fn bugle_notify_state() -> c_int;
    fn bugle_notify_refresh();
    fn bugle_notify_post(title: *const c_char, body: *const c_char, open: *const c_char) -> c_int;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyState {
    /// [`start`] has not been called.
    NotStarted,
    /// The user has not been asked yet (only `start(true)` asks).
    NotDetermined,
    /// Waiting for the user's (or the system's) answer.
    Pending,
    Authorized,
    /// The user said no, or switched notifications off in System Settings.
    Denied,
    /// Not running from an app bundle.
    Unavailable,
}

impl NotifyState {
    fn from_raw(v: c_int) -> Self {
        match v {
            1 => Self::Pending,
            2 => Self::Authorized,
            3 => Self::Denied,
            4 => Self::Unavailable,
            5 => Self::NotDetermined,
            _ => Self::NotStarted,
        }
    }
}

/// Installs the notification delegate and asks for authorization
/// (`prompt`), or only reads the current setting (never shows a prompt).
pub fn start(prompt: bool) -> NotifyState {
    if !crate::login_item::in_app_bundle() {
        return NotifyState::Unavailable;
    }
    NotifyState::from_raw(unsafe { bugle_notify_start(c_int::from(prompt)) })
}

pub fn state() -> NotifyState {
    NotifyState::from_raw(unsafe { bugle_notify_state() })
}

/// Re-reads the setting (no prompt), e.g. after a denial, so switching
/// notifications on in System Settings takes effect without a restart.
pub fn refresh() {
    if crate::login_item::in_app_bundle() {
        unsafe { bugle_notify_refresh() }
    }
}

/// Posts a notification if authorized; clicking it opens `open`. Returns
/// whether it was handed to the notification center.
pub fn post(title: &str, body: &str, open: Option<&Path>) -> bool {
    if state() != NotifyState::Authorized {
        return false;
    }
    let cstr = |s: &str| CString::new(s.replace('\0', "")).unwrap_or_default();
    let title = cstr(title);
    let body = cstr(body);
    let open = open.map(|p| cstr(&p.to_string_lossy()));
    let rc = unsafe { bugle_notify_post(title.as_ptr(), body.as_ptr(), open.as_ref().map_or(std::ptr::null(), |o| o.as_ptr())) };
    rc == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_values_map() {
        assert_eq!(NotifyState::from_raw(0), NotifyState::NotStarted);
        assert_eq!(NotifyState::from_raw(2), NotifyState::Authorized);
        assert_eq!(NotifyState::from_raw(3), NotifyState::Denied);
        assert_eq!(NotifyState::from_raw(5), NotifyState::NotDetermined);
        assert_eq!(NotifyState::from_raw(99), NotifyState::NotStarted);
    }

    #[test]
    fn unbundled_process_never_touches_the_framework() {
        // The test binary is not an app bundle: no prompt, no delegate, and
        // posting is a no-op.
        assert_eq!(start(true), NotifyState::Unavailable);
        assert_ne!(state(), NotifyState::Authorized);
        assert!(!post("Bugle", "test", None));
        refresh();
    }
}
