//! Open at Login (`SMAppService.mainAppService`), backed by
//! `shim/login_item.m`. Registration only works from `Bugle.app`; an
//! unbundled binary should not offer it (see [`in_app_bundle`]).

use anyhow::{anyhow, Result};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};

extern "C" {
    fn bugle_running_in_app_bundle() -> c_int;
    fn bugle_login_item_status() -> c_int;
    fn bugle_login_item_set(enabled: c_int, err: *mut *mut c_char) -> c_int;
    fn bugle_login_item_open_settings();
    fn bugle_free(p: *mut std::ffi::c_void);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginItemStatus {
    NotRegistered,
    Enabled,
    /// Registered, but the user has to allow it in System Settings →
    /// General → Login Items.
    RequiresApproval,
    NotFound,
    /// A status value this code does not know.
    Unknown,
}

impl LoginItemStatus {
    fn from_raw(v: c_int) -> Self {
        match v {
            0 => Self::NotRegistered,
            1 => Self::Enabled,
            2 => Self::RequiresApproval,
            3 => Self::NotFound,
            _ => Self::Unknown,
        }
    }

    /// Registered, whether or not the user has approved it yet.
    pub fn is_registered(self) -> bool {
        matches!(self, Self::Enabled | Self::RequiresApproval)
    }

    /// Will actually launch at login.
    pub fn is_enabled(self) -> bool {
        self == Self::Enabled
    }
}

/// Whether this process runs from an app bundle (`Bugle.app`).
pub fn in_app_bundle() -> bool {
    unsafe { bugle_running_in_app_bundle() != 0 }
}

pub fn status() -> LoginItemStatus {
    LoginItemStatus::from_raw(unsafe { bugle_login_item_status() })
}

/// Opens System Settings → General → Login Items, where the user approves
/// (or removes) Bugle.
pub fn open_settings() {
    unsafe { bugle_login_item_open_settings() }
}

/// Registers or unregisters the app as a login item; returns the new status.
pub fn set_enabled(enabled: bool) -> Result<LoginItemStatus> {
    let mut err: *mut c_char = std::ptr::null_mut();
    let rc = unsafe { bugle_login_item_set(c_int::from(enabled), &mut err) };
    if rc < 0 {
        let msg = if err.is_null() {
            "login item request failed".to_string()
        } else {
            let s = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
            unsafe { bugle_free(err.cast()) };
            s
        };
        return Err(anyhow!(msg));
    }
    Ok(LoginItemStatus::from_raw(rc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_values_map() {
        assert_eq!(LoginItemStatus::from_raw(1), LoginItemStatus::Enabled);
        assert!(LoginItemStatus::from_raw(2).is_registered());
        assert!(!LoginItemStatus::from_raw(2).is_enabled());
        assert!(!LoginItemStatus::from_raw(0).is_registered());
        assert_eq!(LoginItemStatus::from_raw(-1), LoginItemStatus::Unknown);
    }

    #[test]
    fn test_binary_is_not_a_bundle() {
        // Read-only queries; registering is never attempted in tests.
        assert!(!in_app_bundle());
        let _ = status();
    }
}
