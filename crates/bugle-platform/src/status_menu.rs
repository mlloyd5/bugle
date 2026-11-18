//! Status-menu probe (`shim/status_menu.m`) for `bugle-app --smoke-test`:
//! opens the status item's menu as a click does, reads the status item
//! title and the menu's item titles once while it is open, and closes it.

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};

type RawCallback = unsafe extern "C" fn(ctx: *mut c_void, json: *const c_char);

extern "C" {
    fn bugle_status_menu_probe(status_item: *mut c_void, menu: *mut c_void, seconds: f64, sample_at: f64, cb: RawCallback, ctx: *mut c_void)
        -> i32;
}

type Handler = Box<dyn FnOnce(String)>;

unsafe extern "C" fn call(ctx: *mut c_void, json: *const c_char) {
    // SAFETY: ctx is the Box<Handler> leaked in `status_menu_probe`; the shim
    // calls this once.
    let handler = unsafe { Box::from_raw(ctx as *mut Handler) };
    let json = if json.is_null() { "{}".to_string() } else { unsafe { CStr::from_ptr(json) }.to_string_lossy().into_owned() };
    handler(json);
}

/// Open `menu` (an `NSMenu`) from `status_item` (an `NSStatusItem`) on a
/// later main-loop turn, read it once `sample_at` seconds after it opens,
/// close it after `seconds`, and pass the report (JSON: `opened`,
/// `opened_at` and `closed_at` in Unix seconds, `firings` of the probe's own
/// timers, `sample` with `t`, `mode`, `title`, `items`, `width`) to `done`
/// on the main thread. Returns `false` without taking `done` when a probe is
/// running, a pointer is null, or off the main thread.
///
/// # Safety
/// `status_item` and `menu` must point to live objects of those classes.
pub unsafe fn status_menu_probe(
    status_item: *mut c_void,
    menu: *mut c_void,
    seconds: f64,
    sample_at: f64,
    done: impl FnOnce(String) + 'static,
) -> bool {
    let boxed: Box<Handler> = Box::new(Box::new(done));
    let ctx = Box::into_raw(boxed) as *mut c_void;
    let rc = unsafe { bugle_status_menu_probe(status_item, menu, seconds, sample_at, call, ctx) };
    if rc != 1 {
        drop(unsafe { Box::from_raw(ctx as *mut Handler) });
        return false;
    }
    true
}
