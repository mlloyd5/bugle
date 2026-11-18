//! Synthetic mouse and key input for `bugle-app --smoke-test`
//! (`shim/smoke_input.m`).
//!
//! The events are built inside the process and handed to one of Bugle's own
//! windows, so they go through AppKit's real path from there: the window's
//! hit-testing, the view that took the mouse-down getting the drags, key
//! equivalents, the responder chain, a text field's field editor, a
//! button's highlight and action. Nothing reaches the window server: the real
//! pointer does not move, no Accessibility permission is needed, and other
//! apps see none of these events ([`paste`] does write the general
//! pasteboard, which they can read). It is not the same as a person's hardware input:
//! the window server's part (which window is under the pointer, activation
//! by a click, the keyboard layout) is left out.
//!
//! Points are window coordinates: points from the bottom-left corner of the
//! window's frame. Every function waits for the main thread, which must be
//! running its loop; call them from another thread (from the main thread
//! they run at once).

use std::ffi::CString;
use std::os::raw::{c_char, c_void};

extern "C" {
    fn bugle_smoke_on_main(f: unsafe extern "C" fn(*mut c_void), ctx: *mut c_void);
    fn bugle_smoke_mouse(target: i32, phase: i32, x: f64, y: f64, clicks: i32, mods: u64) -> i32;
    fn bugle_smoke_click(target: i32, x: f64, y: f64) -> i32;
    fn bugle_smoke_key(target: i32, key_code: u16, chars: *const c_char, chars_ignoring: *const c_char, mods: u64) -> i32;
    fn bugle_smoke_pick_popup(target: i32, x: f64, y: f64, index: i32) -> i32;
    fn bugle_smoke_paste(target: i32, text: *const c_char) -> i32;
    fn bugle_video_editor_smoke_rect(which: i32, out: *mut f64) -> i32;
    fn bugle_editor_smoke_point(x: f64, y: f64, out: *mut f64) -> i32;
    fn bugle_prompt_smoke_text(buf: *mut c_char, cap: usize) -> i32;
}

/// The window the input goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    VideoEditor = 0,
    /// The newest markup editor.
    MarkupEditor = 1,
    /// The open text prompt.
    Prompt = 2,
}

/// A rectangle in window coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WindowRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl WindowRect {
    pub fn mid(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// Modifier keys (`NSEventModifierFlags` bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods(pub u64);

impl Mods {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1 << 17);
    pub const CONTROL: Self = Self(1 << 18);
    pub const OPTION: Self = Self(1 << 19);
    pub const COMMAND: Self = Self(1 << 20);
    const NUMERIC_PAD: u64 = 1 << 21;
    const FUNCTION: u64 = 1 << 23;

    pub fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// A key on the keyboard (US ANSI layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A letter, digit, space or punctuation key, by the character it types
    /// unshifted.
    Char(char),
    Return,
    Escape,
    /// The Delete (backspace) key.
    Delete,
    /// The forward-delete key (fn-Delete).
    ForwardDelete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

impl Key {
    /// Virtual key code, the character typed, and the flags the hardware
    /// adds by itself (arrows are on the numeric pad and function keys).
    fn raw(self) -> Option<(u16, char, u64)> {
        let fn_key = Mods::FUNCTION;
        Some(match self {
            Key::Return => (36, '\r', 0),
            Key::Escape => (53, '\u{1b}', 0),
            Key::Delete => (51, '\u{7f}', 0),
            Key::ForwardDelete => (117, '\u{f728}', fn_key),
            Key::Left => (123, '\u{f702}', fn_key | Mods::NUMERIC_PAD),
            Key::Right => (124, '\u{f703}', fn_key | Mods::NUMERIC_PAD),
            Key::Down => (125, '\u{f701}', fn_key | Mods::NUMERIC_PAD),
            Key::Up => (126, '\u{f700}', fn_key | Mods::NUMERIC_PAD),
            Key::Home => (115, '\u{f729}', fn_key),
            Key::End => (119, '\u{f72b}', fn_key),
            Key::Char(c) => (char_key_code(c)?, c.to_ascii_lowercase(), 0),
        })
    }
}

/// The ANSI virtual key code of the key that types `c` (shifted or not).
fn char_key_code(c: char) -> Option<u16> {
    const LETTERS: [u16; 26] = [0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16, 6];
    const DIGITS: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    let c = c.to_ascii_lowercase();
    Some(match c {
        'a'..='z' => LETTERS[(c as u8 - b'a') as usize],
        '0'..='9' => DIGITS[(c as u8 - b'0') as usize],
        ' ' => 49,
        '.' => 47,
        ',' => 43,
        '-' => 27,
        '=' => 24,
        '/' => 44,
        _ => return None,
    })
}

/// How a key press reached the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRoute {
    /// Through `NSApplication`: the window was Bugle's key window and Bugle
    /// the active app, as for a real key press.
    App,
    /// Bugle was not active or the window not key: the same steps
    /// (key equivalents, then the window) were taken on the window by hand.
    Window,
}

/// How a click's mouse-up reached the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickRoute {
    /// Something took it off the queue during the mouse-down: a tracking
    /// loop of the view's own (NSButton on macOS 26 runs none, so this has
    /// not been seen).
    Tracked,
    /// Sent to the window after the mouse-down (a view without one).
    Sent,
}

/// What [`pick_popup`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupPick {
    /// The click opened the menu (closed again unchosen) and the item was
    /// chosen through the menu.
    Opened,
    /// The item was chosen through the menu, but the click was not seen
    /// opening it.
    NotOpened,
    /// No enabled pop-up button under the point.
    NoPopup,
    /// No such item.
    NoItem,
}

/// Run `f` on the main thread and wait for its result. A panic in `f` is
/// carried back to the caller.
pub fn on_main<R: Send, F: FnOnce() -> R + Send>(f: F) -> R {
    type Slot<F, R> = (Option<F>, Option<std::thread::Result<R>>);
    unsafe extern "C" fn tramp<R, F: FnOnce() -> R>(ctx: *mut c_void) {
        // SAFETY: ctx is the Slot below, alive until bugle_smoke_on_main
        // returns, which waits for this call.
        let slot = unsafe { &mut *(ctx as *mut Slot<F, R>) };
        if let Some(f) = slot.0.take() {
            slot.1 = Some(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)));
        }
    }
    let mut slot: Slot<F, R> = (Some(f), None);
    unsafe { bugle_smoke_on_main(tramp::<R, F>, &mut slot as *mut Slot<F, R> as *mut c_void) };
    match slot.1 {
        Some(Ok(r)) => r,
        Some(Err(panic)) => std::panic::resume_unwind(panic),
        None => unreachable!("bugle_smoke_on_main returned without running the closure"),
    }
}

/// Left-button mouse-down at `at`, hit-tested by the window. `false` when
/// the target window is not on screen.
pub fn mouse_down(target: Target, at: (f64, f64)) -> bool {
    unsafe { bugle_smoke_mouse(target as i32, 0, at.0, at.1, 1, 0) == 1 }
}

/// A drag to `at`, sent to the view that took the mouse-down.
pub fn mouse_drag(target: Target, at: (f64, f64)) -> bool {
    unsafe { bugle_smoke_mouse(target as i32, 1, at.0, at.1, 1, 0) == 1 }
}

/// The mouse-up at `at`, sent to the view that took the mouse-down.
pub fn mouse_up(target: Target, at: (f64, f64)) -> bool {
    unsafe { bugle_smoke_mouse(target as i32, 2, at.0, at.1, 1, 0) == 1 }
}

/// Press at `from`, drag through `steps` points on the way to `to`, release
/// at `to`.
pub fn drag(target: Target, from: (f64, f64), to: (f64, f64), steps: u32) -> bool {
    if !mouse_down(target, from) {
        return false;
    }
    let steps = steps.max(1);
    for i in 1..=steps {
        let f = i as f64 / steps as f64;
        mouse_drag(target, (from.0 + (to.0 - from.0) * f, from.1 + (to.1 - from.1) * f));
    }
    mouse_up(target, to)
}

/// A click at `at` that also works on a control with a tracking loop of
/// its own (see [`ClickRoute`]). `None` when the target window is not on
/// screen.
pub fn click(target: Target, at: (f64, f64)) -> Option<ClickRoute> {
    match unsafe { bugle_smoke_click(target as i32, at.0, at.1) } {
        1 => Some(ClickRoute::Tracked),
        2 => Some(ClickRoute::Sent),
        _ => None,
    }
}

/// Press and release `key` with `mods`. `None` when the target window is
/// not on screen or the key is not on the keyboard.
pub fn key(target: Target, key: Key, mods: Mods) -> Option<KeyRoute> {
    let (code, c, extra) = key.raw()?;
    let shifted = if mods.0 & Mods::SHIFT.0 != 0 { c.to_ascii_uppercase() } else { c };
    // Control turns a letter into a control character; nothing here uses it.
    let chars = CString::new(shifted.to_string()).ok()?;
    let ignoring = CString::new(shifted.to_string()).ok()?;
    match unsafe { bugle_smoke_key(target as i32, code, chars.as_ptr(), ignoring.as_ptr(), mods.0 | extra) } {
        1 => Some(KeyRoute::App),
        2 => Some(KeyRoute::Window),
        _ => None,
    }
}

/// Type `text` one key at a time (characters [`Key::Char`] knows, capitals
/// with Shift). The route of the last key, or `None` when a key could not
/// be pressed.
pub fn type_text(target: Target, text: &str) -> Option<KeyRoute> {
    let mut route = None;
    for c in text.chars() {
        let mods = if c.is_ascii_uppercase() { Mods::SHIFT } else { Mods::NONE };
        route = Some(key(target, Key::Char(c), mods)?);
    }
    route
}

/// Choose item `index` of the pop-up button at `at` (see [`PopupPick`]):
/// a mouse-down opens its menu, which is closed again unchosen, and the
/// item is chosen through the menu. An open menu runs a modal tracking
/// session of its own; steering it with more synthetic events is not
/// attempted. `None` when the target window is not on
/// screen.
pub fn pick_popup(target: Target, at: (f64, f64), index: usize) -> Option<PopupPick> {
    let index = i32::try_from(index).unwrap_or(-1);
    match unsafe { bugle_smoke_pick_popup(target as i32, at.0, at.1, index) } {
        1 => Some(PopupPick::Opened),
        2 => Some(PopupPick::NotOpened),
        -1 => Some(PopupPick::NoPopup),
        -2 => Some(PopupPick::NoItem),
        _ => None,
    }
}

/// What [`paste`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paste {
    /// ⌘V was pressed with the text on the pasteboard, and the previous
    /// contents were put back.
    Restored,
    /// ⌘V was pressed, but something else wrote the pasteboard meanwhile, so
    /// the previous contents were left out rather than overwrite it.
    NotRestored,
    /// The pasteboard held more than plain text (possibly data an app has
    /// only promised): nothing was touched.
    Declined,
}

/// Press ⌘V in the target window with `text` on the general pasteboard and
/// put back what was there, in one main-thread turn. Only when the
/// pasteboard is empty or holds plain text alone; see [`Paste`]. It goes
/// through the user's real pasteboard (its change count goes up twice and
/// clipboard tools see `text` go by), so callers make it opt-in. `None`
/// when the target window is not on screen.
pub fn paste(target: Target, text: &str) -> Option<Paste> {
    let c = CString::new(text.replace('\0', "")).unwrap_or_default();
    match unsafe { bugle_smoke_paste(target as i32, c.as_ptr()) } {
        1 => Some(Paste::Restored),
        2 => Some(Paste::NotRestored),
        -1 => Some(Paste::Declined),
        _ => None,
    }
}

/// A part of the video editor, for [`video_editor_rect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEditorPart {
    /// The timeline's track: time 0 at its left edge, the end at its right.
    Track = 0,
    Play = 1,
    SetIn = 2,
    SetOut = 3,
    Cut = 4,
    Restore = 5,
    ZoomMenu = 6,
    ClickMenu = 7,
    Smooth = 8,
    PresetMenu = 9,
    Export = 10,
}

/// Where `part` of the open video editor is, in window coordinates.
pub fn video_editor_rect(part: VideoEditorPart) -> Option<WindowRect> {
    on_main(move || {
        let mut out = [0.0f64; 4];
        (unsafe { bugle_video_editor_smoke_rect(part as i32, out.as_mut_ptr()) } == 1)
            .then_some(WindowRect { x: out[0], y: out[1], w: out[2], h: out[3] })
    })
}

/// Where edited-image pixel (`x`, `y`) of the newest markup editor is, in
/// window coordinates.
pub fn markup_editor_point(x: f64, y: f64) -> Option<(f64, f64)> {
    on_main(move || {
        let mut out = [0.0f64; 2];
        (unsafe { bugle_editor_smoke_point(x, y, out.as_mut_ptr()) } == 1).then_some((out[0], out[1]))
    })
}

/// The text in the open prompt's field (as being edited), or `None` with no
/// prompt open.
pub fn prompt_field_text() -> Option<String> {
    on_main(|| {
        let mut buf = vec![0u8; 1024];
        if unsafe { bugle_prompt_smoke_text(buf.as_mut_ptr().cast(), buf.len()) } != 1 {
            return None;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Some(String::from_utf8_lossy(&buf[..end]).into_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_have_ansi_codes() {
        assert_eq!(Key::Char('a').raw().map(|r| r.0), Some(0));
        assert_eq!(Key::Char('Z').raw().map(|r| (r.0, r.1)), Some((6, 'z')));
        assert_eq!(Key::Char('5').raw().map(|r| r.0), Some(23));
        assert_eq!(Key::Char(' ').raw().map(|r| r.0), Some(49));
        assert_eq!(Key::Char('é').raw(), None);
        assert_eq!(Key::Left.raw().map(|r| r.1), Some('\u{f702}'));
        assert_eq!(Key::ForwardDelete.raw().map(|r| r.0), Some(117));
    }
}
