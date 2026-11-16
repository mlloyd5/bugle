//! Markup editor window (`shim/editor.m`): a toolbar above a canvas showing
//! an image scaled to fit.
//!
//! The window is a view and nothing more. It reports input as
//! [`EditorEvent`]s in image pixel coordinates and draws what it is given:
//! the image ([`editor_set_image`]), an overlay of simple primitives on top
//! ([`editor_set_overlay`]) and the toolbar state ([`editor_set_state`]).
//! What a click means is the caller's business.
//!
//! Every function here is main-thread only and needs a running AppKit loop.
//! One editor at a time.

use super::cstr;
use anyhow::{bail, Result};
use bugle_core::{Point, Rect};
use image::RgbaImage;
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

type RawCallback = unsafe extern "C" fn(ctx: *mut c_void, kind: i32, x: f64, y: f64, arg: i32, text: *const c_char);
type RawFree = unsafe extern "C" fn(ctx: *mut c_void);

extern "C" {
    fn bugle_editor_open(
        title: *const c_char,
        img_w: u32,
        img_h: u32,
        tool_labels: *const *const c_char,
        tool_symbols: *const *const c_char,
        n_tools: i32,
        colors: *const u32,
        n_colors: i32,
        width_labels: *const *const c_char,
        n_widths: i32,
        cb: RawCallback,
        ctx: *mut c_void,
        free_ctx: RawFree,
    ) -> i32;
    fn bugle_editor_set_image(rgba: *const u8, w: u32, h: u32);
    fn bugle_editor_set_overlay(values: *const f64, n: usize);
    fn bugle_editor_set_state(tool: i32, color: i32, width: i32, can_undo: i32, can_redo: i32, dirty: i32);
    fn bugle_editor_begin_text(x: f64, y: f64, font_px: f64, initial: *const c_char, rgba: u32);
    fn bugle_editor_close();
    fn bugle_editor_active() -> i32;
    fn bugle_editor_focus();
    fn bugle_editor_inject(kind: i32, x: f64, y: f64, arg: i32, text: *const c_char);
}

/// A toolbar or keyboard command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorCommand {
    Undo,
    Redo,
    Delete,
    Save,
    Copy,
    Escape,
}

/// Input from the editor window. Points are image pixels (top-left origin)
/// and may lie outside the image.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorEvent {
    MouseDown { at: Point, shift: bool, clicks: u32 },
    MouseDrag { at: Point, shift: bool },
    MouseUp { at: Point, shift: bool },
    /// A tool button, by index into the `tools` given to [`editor_open`].
    Tool(usize),
    /// A colour swatch, by index.
    Color(usize),
    /// A stroke size, by index.
    Width(usize),
    Command(EditorCommand),
    /// The label started with [`editor_begin_text`] was finished; empty when
    /// the user cancelled.
    Text(String),
    /// An unmodified printable key, lower-cased.
    Key(char),
    /// The zoom changed: this many image pixels per screen point.
    Scale(f64),
}

impl EditorEvent {
    fn from_raw(kind: i32, x: f64, y: f64, arg: i32, text: Option<String>) -> Option<Self> {
        let at = Point { x, y };
        let shift = arg & 1 == 1;
        let index = usize::try_from(arg).ok();
        Some(match kind {
            0 => Self::MouseDown { at, shift, clicks: ((arg >> 8) & 0xFF).max(1) as u32 },
            1 => Self::MouseDrag { at, shift },
            2 => Self::MouseUp { at, shift },
            3 => Self::Tool(index?),
            4 => Self::Color(index?),
            5 => Self::Width(index?),
            6 => Self::Command(match arg {
                0 => EditorCommand::Undo,
                1 => EditorCommand::Redo,
                2 => EditorCommand::Delete,
                3 => EditorCommand::Save,
                4 => EditorCommand::Copy,
                5 => EditorCommand::Escape,
                _ => return None,
            }),
            7 => Self::Text(text.unwrap_or_default()),
            8 => Self::Key(char::from_u32(u32::try_from(arg).ok()?)?),
            9 if x.is_finite() && x > 0.0 => Self::Scale(x),
            _ => return None,
        })
    }

    fn to_raw(&self) -> (i32, f64, f64, i32, Option<&str>) {
        match self {
            Self::MouseDown { at, shift, clicks } => (0, at.x, at.y, *shift as i32 | ((*clicks as i32) << 8), None),
            Self::MouseDrag { at, shift } => (1, at.x, at.y, *shift as i32, None),
            Self::MouseUp { at, shift } => (2, at.x, at.y, *shift as i32, None),
            Self::Tool(i) => (3, 0.0, 0.0, *i as i32, None),
            Self::Color(i) => (4, 0.0, 0.0, *i as i32, None),
            Self::Width(i) => (5, 0.0, 0.0, *i as i32, None),
            Self::Command(c) => (6, 0.0, 0.0, *c as i32, None),
            Self::Text(t) => (7, 0.0, 0.0, 0, Some(t)),
            Self::Key(c) => (8, 0.0, 0.0, *c as i32, None),
            Self::Scale(s) => (9, *s, 0.0, 0, None),
        }
    }
}

/// Something drawn above the image, in image pixels. Used for the shape
/// being dragged out and for selection marks; finished shapes are part of
/// the image itself.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorPrim {
    /// A stroked path through the points (two points: a straight line).
    Polyline { points: Vec<Point>, color: [u8; 4], width: f64 },
    Arrow { from: Point, to: Point, color: [u8; 4], width: f64 },
    Rect { rect: Rect, color: [u8; 4], width: f64 },
    Ellipse { rect: Rect, color: [u8; 4], width: f64 },
    Fill { rect: Rect, color: [u8; 4] },
    /// Dashed selection outline around `rect`, a fixed size on screen.
    Selection { rect: Rect },
    /// Round drag handle, a fixed size on screen.
    Handle { at: Point },
}

impl EditorPrim {
    /// Append the wire form: `kind, r, g, b, a, width, point_count`, then
    /// the points (rect-like kinds: origin, then size).
    fn encode(&self, out: &mut Vec<f64>) {
        let mut head = |kind: f64, c: [u8; 4], width: f64, n: usize| {
            out.push(kind);
            out.extend(c.iter().map(|v| *v as f64 / 255.0));
            out.push(width);
            out.push(n as f64);
        };
        const NONE: [u8; 4] = [0; 4];
        match self {
            Self::Polyline { points, color, width } => {
                head(0.0, *color, *width, points.len());
                out.extend(points.iter().flat_map(|p| [p.x, p.y]));
            }
            Self::Arrow { from, to, color, width } => {
                head(1.0, *color, *width, 2);
                out.extend([from.x, from.y, to.x, to.y]);
            }
            Self::Rect { rect, color, width } => {
                head(2.0, *color, *width, 2);
                out.extend([rect.x, rect.y, rect.w, rect.h]);
            }
            Self::Ellipse { rect, color, width } => {
                head(3.0, *color, *width, 2);
                out.extend([rect.x, rect.y, rect.w, rect.h]);
            }
            Self::Fill { rect, color } => {
                head(4.0, *color, 0.0, 2);
                out.extend([rect.x, rect.y, rect.w, rect.h]);
            }
            Self::Selection { rect } => {
                head(5.0, NONE, 0.0, 2);
                out.extend([rect.x, rect.y, rect.w, rect.h]);
            }
            Self::Handle { at } => {
                head(6.0, NONE, 0.0, 1);
                out.extend([at.x, at.y]);
            }
        }
    }
}

/// A tool button: its tooltip and SF Symbol name.
#[derive(Debug, Clone, Copy)]
pub struct EditorTool {
    pub label: &'static str,
    pub symbol: &'static str,
}

/// What [`editor_set_state`] shows in the toolbar and title bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EditorState {
    pub tool: usize,
    pub color: usize,
    pub width: usize,
    pub can_undo: bool,
    pub can_redo: bool,
    /// Unsaved changes: closing the window asks first.
    pub dirty: bool,
}

type Handler = RefCell<Box<dyn FnMut(EditorEvent)>>;

unsafe extern "C" fn call(ctx: *mut c_void, kind: i32, x: f64, y: f64, arg: i32, text: *const c_char) {
    // SAFETY: ctx is the Box<Handler> leaked in `editor_open`; the shim frees
    // it through `free` only after the window is gone, on a later turn.
    let handler = unsafe { &*(ctx as *const Handler) };
    let text = (!text.is_null()).then(|| unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned());
    let Some(event) = EditorEvent::from_raw(kind, x, y, arg, text) else { return };
    // The window never reports while a report is being handled; if it ever
    // did, dropping the event is better than re-entering the handler.
    match handler.try_borrow_mut() {
        Ok(mut h) => h(event),
        Err(_) => tracing::warn!("editor: dropped re-entrant event {event:?}"),
    };
}

unsafe extern "C" fn free(ctx: *mut c_void) {
    drop(unsafe { Box::from_raw(ctx as *mut Handler) });
}

/// Open the editor window for an image of `size` pixels and bring it to the
/// front. `colors` are RGBA swatches, `widths` the labels of the stroke
/// sizes. `handler` runs on the main thread for every event and is dropped
/// when the window closes, however it closes. Fails when an editor is
/// already open (see [`editor_active`]).
pub fn editor_open(
    title: &str,
    size: (u32, u32),
    tools: &[EditorTool],
    colors: &[[u8; 4]],
    widths: &[&str],
    handler: impl FnMut(EditorEvent) + 'static,
) -> Result<()> {
    let title = cstr(title)?;
    let strings = |v: Vec<&str>| v.into_iter().map(cstr).collect::<Result<Vec<CString>>>();
    let labels = strings(tools.iter().map(|t| t.label).collect())?;
    let symbols = strings(tools.iter().map(|t| t.symbol).collect())?;
    let width_labels = strings(widths.to_vec())?;
    let ptrs = |v: &[CString]| v.iter().map(|c| c.as_ptr()).collect::<Vec<*const c_char>>();
    let (label_ptrs, symbol_ptrs, width_ptrs) = (ptrs(&labels), ptrs(&symbols), ptrs(&width_labels));
    let packed: Vec<u32> = colors.iter().map(|c| u32::from_be_bytes(*c)).collect();
    let boxed: Box<Handler> = Box::new(RefCell::new(Box::new(handler)));
    let ctx = Box::into_raw(boxed) as *mut c_void;
    let ok = unsafe {
        bugle_editor_open(
            title.as_ptr(),
            size.0,
            size.1,
            label_ptrs.as_ptr(),
            symbol_ptrs.as_ptr(),
            label_ptrs.len() as i32,
            packed.as_ptr(),
            packed.len() as i32,
            width_ptrs.as_ptr(),
            width_ptrs.len() as i32,
            call,
            ctx,
            free,
        )
    };
    if ok != 1 {
        // The shim did not take the context.
        drop(unsafe { Box::from_raw(ctx as *mut Handler) });
        bail!("could not open the editor window (is one already open?)");
    }
    Ok(())
}

/// Show `img` on the canvas. No-op when no editor is open.
pub fn editor_set_image(img: &RgbaImage) {
    unsafe { bugle_editor_set_image(img.as_raw().as_ptr(), img.width(), img.height()) }
}

/// Replace what is drawn above the image; an empty slice clears it.
pub fn editor_set_overlay(prims: &[EditorPrim]) {
    let mut values = Vec::new();
    for p in prims {
        p.encode(&mut values);
    }
    unsafe { bugle_editor_set_overlay(values.as_ptr(), values.len()) }
}

pub fn editor_set_state(s: EditorState) {
    let i = |v: usize| i32::try_from(v).unwrap_or(-1);
    unsafe { bugle_editor_set_state(i(s.tool), i(s.color), i(s.width), s.can_undo as i32, s.can_redo as i32, s.dirty as i32) }
}

/// Put a text field on the canvas at `at` for a label `font_px` image pixels
/// tall, pre-filled with `initial`. The handler gets one
/// [`EditorEvent::Text`] when the user is done. May be called from inside
/// the handler.
pub fn editor_begin_text(at: Point, font_px: f64, initial: &str, color: [u8; 4]) {
    let initial = CString::new(initial.replace('\0', "")).unwrap_or_default();
    unsafe { bugle_editor_begin_text(at.x, at.y, font_px, initial.as_ptr(), u32::from_be_bytes(color)) }
}

/// Close the editor without asking about unsaved changes. May be called
/// from inside the handler, which is dropped on a later main-loop turn.
pub fn editor_close() {
    unsafe { bugle_editor_close() }
}

/// Whether an editor window is open.
pub fn editor_active() -> bool {
    unsafe { bugle_editor_active() == 1 }
}

/// Bring the open editor to the front.
pub fn editor_focus() {
    unsafe { bugle_editor_focus() }
}

/// Deliver `event` to the handler as if the window had reported it. For
/// smoke tests, which cannot click. Not from inside the handler.
pub fn editor_inject(event: &EditorEvent) {
    let (kind, x, y, arg, text) = event.to_raw();
    let text = text.map(|t| CString::new(t.replace('\0', "")).unwrap_or_default());
    unsafe { bugle_editor_inject(kind, x, y, arg, text.as_ref().map_or(std::ptr::null(), |t| t.as_ptr())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip() {
        let at = Point { x: 12.5, y: -3.0 };
        let all = [
            EditorEvent::MouseDown { at, shift: true, clicks: 2 },
            EditorEvent::MouseDown { at, shift: false, clicks: 1 },
            EditorEvent::MouseDrag { at, shift: true },
            EditorEvent::MouseUp { at, shift: false },
            EditorEvent::Tool(10),
            EditorEvent::Color(0),
            EditorEvent::Width(2),
            EditorEvent::Command(EditorCommand::Undo),
            EditorEvent::Command(EditorCommand::Escape),
            EditorEvent::Text("héllo".into()),
            EditorEvent::Key('v'),
            EditorEvent::Scale(2.0),
        ];
        for e in all {
            let (kind, x, y, arg, text) = e.to_raw();
            assert_eq!(EditorEvent::from_raw(kind, x, y, arg, text.map(str::to_string)), Some(e.clone()), "{e:?}");
        }
    }

    #[test]
    fn bad_events_are_dropped() {
        assert_eq!(EditorEvent::from_raw(3, 0.0, 0.0, -1, None), None);
        assert_eq!(EditorEvent::from_raw(6, 0.0, 0.0, 9, None), None);
        assert_eq!(EditorEvent::from_raw(9, 0.0, 0.0, 0, None), None);
        assert_eq!(EditorEvent::from_raw(9, f64::NAN, 0.0, 0, None), None);
        assert_eq!(EditorEvent::from_raw(42, 0.0, 0.0, 0, None), None);
        assert_eq!(EditorEvent::from_raw(7, 0.0, 0.0, 0, None), Some(EditorEvent::Text(String::new())));
    }

    #[test]
    fn prims_encode_with_point_counts() {
        let mut v = Vec::new();
        EditorPrim::Polyline {
            points: vec![Point { x: 1.0, y: 2.0 }, Point { x: 3.0, y: 4.0 }, Point { x: 5.0, y: 6.0 }],
            color: [255, 0, 0, 255],
            width: 4.0,
        }
        .encode(&mut v);
        assert_eq!(v, vec![0.0, 1.0, 0.0, 0.0, 1.0, 4.0, 3.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        v.clear();
        EditorPrim::Selection { rect: Rect::new(1.0, 2.0, 3.0, 4.0) }.encode(&mut v);
        assert_eq!(v, vec![5.0, 0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 1.0, 2.0, 3.0, 4.0]);
        v.clear();
        EditorPrim::Handle { at: Point { x: 7.0, y: 8.0 } }.encode(&mut v);
        assert_eq!(v, vec![6.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 7.0, 8.0]);
    }
}
