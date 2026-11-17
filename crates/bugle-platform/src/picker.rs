//! Window picker overlay (`shim/picker.m`): a see-through overlay that reports
//! pointer moves and ends on a click, Esc, right click or timeout. Which
//! window is under the pointer is the caller's decision; the overlay only
//! draws the rect it is given.

use bugle_core::{Point, Rect};
use std::ffi::CString;
use std::time::Duration;

#[allow(non_camel_case_types)]
mod ffi {
    use libc::{c_char, c_void};

    pub type bugle_pick_callback_t = unsafe extern "C" fn(ctx: *mut c_void, kind: i32, x: f64, y: f64);

    extern "C" {
        pub fn bugle_pick_begin(cb: bugle_pick_callback_t, ctx: *mut c_void, timeout_seconds: f64, allow_switch: i32) -> i32;
        pub fn bugle_pick_highlight(x: f64, y: f64, w: f64, h: f64, label: *const c_char);
        pub fn bugle_pick_active() -> i32;
        pub fn bugle_pick_cancel();
        pub fn bugle_pick_switch();
    }
}

/// What the picker overlay reports. `Clicked` and `Cancelled` are final.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PickEvent {
    /// The pointer is at this global point (top-left origin).
    Moved(Point),
    /// The user clicked here; the overlay is already gone.
    Clicked(Point),
    /// Esc, right click, timeout, switching to another app, or
    /// [`window_pick_cancel`].
    Cancelled,
    /// Space, when the picker was opened with `allow_switch`: the user wants
    /// the area selection instead. Bugle is left active so the selection
    /// overlay can open at once.
    Switch,
}

type PickCb = Box<dyn FnMut(PickEvent)>;

unsafe extern "C" fn pick_trampoline(ctx: *mut libc::c_void, kind: i32, x: f64, y: f64) {
    let p = Point { x, y };
    if kind == 0 {
        // SAFETY: ctx is the Box<PickCb> leaked in `window_pick_begin`; it
        // is freed only by the final call below, which the shim delivers
        // after every move and never re-entrantly.
        let cb = unsafe { &mut *(ctx as *mut PickCb) };
        cb(PickEvent::Moved(p));
    } else {
        let mut cb: Box<PickCb> = unsafe { Box::from_raw(ctx as *mut PickCb) };
        cb(match kind {
            1 => PickEvent::Clicked(p),
            3 => PickEvent::Switch,
            _ => PickEvent::Cancelled,
        });
    }
}

/// Show the picker overlay on every screen and return at once. `handler`
/// runs on the main thread for every pointer move and then exactly once with
/// a final event. The overlay closes itself after `timeout` so it can never
/// strand the desktop. With `allow_switch`, Space ends it with
/// [`PickEvent::Switch`]. Returns `false` (and drops `handler` without calling
/// it) when a picker or area selection is already on screen. Main thread
/// only, needs a running AppKit loop.
pub fn window_pick_begin(timeout: Duration, allow_switch: bool, handler: impl FnMut(PickEvent) + 'static) -> bool {
    let boxed: Box<PickCb> = Box::new(Box::new(handler));
    let ctx = Box::into_raw(boxed);
    let secs = timeout.as_secs_f64().max(0.5);
    let started = unsafe { ffi::bugle_pick_begin(pick_trampoline, ctx as *mut libc::c_void, secs, allow_switch as i32) } == 1;
    if !started {
        drop(unsafe { Box::from_raw(ctx) });
    }
    started
}

/// Highlight `rect` (global points) with a label, or clear the highlight.
/// No-op when no picker is active. Main thread only.
pub fn window_pick_highlight(rect: Option<Rect>, label: &str) {
    let r = rect.unwrap_or_default();
    let label = CString::new(label.replace('\0', "")).unwrap_or_default();
    unsafe { ffi::bugle_pick_highlight(r.x, r.y, r.w, r.h, label.as_ptr()) }
}

/// Whether the picker overlay is on screen.
pub fn window_pick_active() -> bool {
    unsafe { ffi::bugle_pick_active() == 1 }
}

/// Dismiss the picker; the handler gets [`PickEvent::Cancelled`]. Main thread only.
pub fn window_pick_cancel() {
    unsafe { ffi::bugle_pick_cancel() }
}

/// Do what Space does: end with [`PickEvent::Switch`] when the picker was
/// opened with `allow_switch`, otherwise nothing. Main thread only.
pub fn window_pick_switch() {
    unsafe { ffi::bugle_pick_switch() }
}
