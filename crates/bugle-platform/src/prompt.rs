//! A one-line text prompt (a small panel with a text field and OK/Cancel),
//! backed by `shim/prompt.m`, for values the menu-bar app asks for that a
//! menu of presets cannot hold.
//!
//! The prompt is not modal: [`prompt_text`] returns at once and the answer
//! reaches the callback later, so the app's event loop keeps running while
//! it is open. One prompt at a time. Main thread only.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

type RawCallback = unsafe extern "C" fn(ctx: *mut c_void, ok: i32, text: *const c_char);
type RawFree = unsafe extern "C" fn(ctx: *mut c_void);

extern "C" {
    fn bugle_prompt_open(
        title: *const c_char,
        message: *const c_char,
        initial: *const c_char,
        cb: RawCallback,
        ctx: *mut c_void,
        free_ctx: RawFree,
    ) -> i32;
    fn bugle_prompt_active() -> i32;
    fn bugle_prompt_answer(ok: i32, text: *const c_char);
}

type Handler = RefCell<Option<Box<dyn FnOnce(Option<String>)>>>;

unsafe extern "C" fn call(ctx: *mut c_void, ok: i32, text: *const c_char) {
    // SAFETY: ctx is the Box<Handler> leaked in `prompt_text`; the shim
    // frees it through `free` only after this call, on a later main-loop
    // turn.
    let handler = unsafe { &*(ctx as *const Handler) };
    let answer = (ok == 1 && !text.is_null()).then(|| unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned());
    if let Some(f) = handler.borrow_mut().take() {
        f(answer);
    }
}

unsafe extern "C" fn free(ctx: *mut c_void) {
    drop(unsafe { Box::from_raw(ctx as *mut Handler) });
}

/// Ask for a line of text in a prompt titled `title`, with `message` below
/// it and `initial` (selected) in the field, and return at once. `done`
/// runs later on the main thread with the text for OK, or `None` for
/// Cancel, Esc or the close button. Returns `false` without taking `done`
/// when a prompt is already open (it is brought to the front instead) or
/// when called off the main thread.
pub fn prompt_text(title: &str, message: &str, initial: &str, done: impl FnOnce(Option<String>) + 'static) -> bool {
    let c = |s: &str| CString::new(s.replace('\0', "")).unwrap_or_default();
    let (title, message, initial) = (c(title), c(message), c(initial));
    let boxed: Box<Handler> = Box::new(RefCell::new(Some(Box::new(done))));
    let ctx = Box::into_raw(boxed) as *mut c_void;
    let rc = unsafe { bugle_prompt_open(title.as_ptr(), message.as_ptr(), initial.as_ptr(), call, ctx, free) };
    if rc != 1 {
        // The shim did not take the context.
        drop(unsafe { Box::from_raw(ctx as *mut Handler) });
        return false;
    }
    true
}

/// Whether a prompt is open.
pub fn prompt_active() -> bool {
    unsafe { bugle_prompt_active() == 1 }
}

/// Answer the open prompt as if OK (with `text`) or Cancel (`None`) were
/// pressed. Any thread; for smoke tests, which cannot click. No-op when no
/// prompt is open.
pub fn prompt_answer(text: Option<&str>) {
    let c = text.map(|t| CString::new(t.replace('\0', "")).unwrap_or_default());
    unsafe { bugle_prompt_answer(c.is_some() as i32, c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr())) }
}
