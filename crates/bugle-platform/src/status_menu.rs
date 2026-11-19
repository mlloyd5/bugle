//! Status-menu probe (`shim/status_menu.m`) for `bugle-app --smoke-test`:
//! opens the status item's menu as a click does, reads the status item
//! title and the menu's item titles once while it is open, and closes it.

use objc2::rc::Retained;
use objc2_foundation::NSAttributedString;
use std::ffi::{CStr, CString};
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

extern "C" {
    fn bugle_menu_title_mono_tail(head: *const c_char, tail: *const c_char) -> *mut c_void;
    fn bugle_attributed_width(s: *mut c_void) -> f64;
}

/// A menu-item title of `head` in the menu font followed by `tail` in Menlo
/// at the same size (the monospaced system font lacks the meter's `▮`/`▯`;
/// it is used only without Menlo), for `muda`'s
/// `MenuItem::set_attributed_title`: a tail of a constant character count
/// draws at a constant width. No colour is set, so AppKit colours it for
/// the item's state. `None` if either part holds a NUL.
pub fn menu_title_mono_tail(head: &str, tail: &str) -> Option<Retained<NSAttributedString>> {
    let (head, tail) = (CString::new(head).ok()?, CString::new(tail).ok()?);
    let raw = unsafe { bugle_menu_title_mono_tail(head.as_ptr(), tail.as_ptr()) };
    // SAFETY: the shim returns a +1 NSAttributedString, or null.
    unsafe { Retained::from_raw(raw as *mut NSAttributedString) }
}

/// The width in points `title` draws at.
pub fn attributed_width(title: &NSAttributedString) -> f64 {
    // SAFETY: a live NSAttributedString; the shim only reads it.
    unsafe { bugle_attributed_width(title as *const NSAttributedString as *mut c_void) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::audio_level::{meter_label_parts, TrackLevel, FLOOR_DB};

    #[test]
    fn meter_lines_draw_at_one_width() {
        let mut dbs = vec![f64::NEG_INFINITY, FLOOR_DB, -60.0, -59.6, -45.0, -11.0, -9.6, -1.0, -0.4, 0.0, 20.0];
        dbs.extend((0..=120).map(|i| -60.0 + i as f64 * 0.5));
        for name in ["Microphone", "System Audio"] {
            let widths: Vec<f64> = dbs
                .iter()
                .map(|&db| {
                    let (head, gauge) = meter_label_parts(name, &TrackLevel { peak_db: db, rms_db: db });
                    attributed_width(&menu_title_mono_tail(&head, &gauge).unwrap())
                })
                .collect();
            let (lo, hi) = widths.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &w| (lo.min(w), hi.max(w)));
            assert!(lo > 50.0, "{name}: {lo}");
            assert!(hi - lo < 0.01, "{name}: drawn widths {lo:.2}..{hi:.2} pt");
        }
        // In the menu font the same parts would not hold still.
        let plain = |t: &str| attributed_width(&menu_title_mono_tail(t, "").unwrap());
        assert!((plain("▯▯▯▯▯▯▯▯▯▯▯▯  silent") - plain("▮▮▮▮▮▮▮▮▮▮▯▯  -11 dB")).abs() > 0.0);
        assert!(menu_title_mono_tail("a\0b", "").is_none());
    }
}
