//! A modal text prompt (an `NSAlert` with a text field), backed by
//! `shim/prompt.m`, for values the menu-bar app asks for that a menu of
//! presets cannot hold.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

extern "C" {
    fn bugle_prompt_text(title: *const c_char, message: *const c_char, initial: *const c_char, out: *mut *mut c_char) -> c_int;
    fn bugle_free(p: *mut std::ffi::c_void);
}

/// Ask for a line of text in a modal alert titled `title`, with `message`
/// below it and `initial` in the field. `None` when the user cancels or
/// when called off the main thread (the alert needs the main run loop).
/// Blocks until the alert closes.
pub fn prompt_text(title: &str, message: &str, initial: &str) -> Option<String> {
    let c = |s: &str| CString::new(s.replace('\0', "")).unwrap_or_default();
    let (title, message, initial) = (c(title), c(message), c(initial));
    let mut out: *mut c_char = std::ptr::null_mut();
    let rc = unsafe { bugle_prompt_text(title.as_ptr(), message.as_ptr(), initial.as_ptr(), &mut out) };
    if out.is_null() {
        return None;
    }
    let text = unsafe { CStr::from_ptr(out) }.to_string_lossy().into_owned();
    unsafe { bugle_free(out.cast()) };
    (rc == 1).then_some(text)
}
