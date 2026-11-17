//! Video editor window (`shim/video_editor.m`): a frame preview above a
//! timeline (the recording, the kept range between the in and out handles,
//! the playhead) and a control bar (play/pause, Set In / Set Out, zoom and
//! pointer options, export preset, Export).
//!
//! Like the markup editor window it is a view and nothing more: it reports
//! input as [`VideoEditorEvent`]s, times in seconds on the video's
//! timeline, and draws the frame ([`video_editor_set_frame`]) and control
//! state ([`video_editor_set_state`]) it is given. What the input means,
//! and which frame to show, is the caller's business.
//!
//! Every function here is main-thread only and needs a running AppKit loop.
//! One video editor at a time.

use super::cstr;
use anyhow::{bail, Result};
use image::RgbaImage;
use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::{c_char, c_void};

type RawCallback = unsafe extern "C" fn(ctx: *mut c_void, kind: i32, value: f64, arg: i32);
type RawFree = unsafe extern "C" fn(ctx: *mut c_void);

extern "C" {
    fn bugle_video_editor_open(
        title: *const c_char,
        duration: f64,
        w: u32,
        h: u32,
        zoom_labels: *const *const c_char,
        n_zoom: i32,
        preset_labels: *const *const c_char,
        n_presets: i32,
        cb: RawCallback,
        ctx: *mut c_void,
        free_ctx: RawFree,
    ) -> i32;
    fn bugle_video_editor_set_frame(rgba: *const u8, w: u32, h: u32);
    #[allow(clippy::too_many_arguments)]
    fn bugle_video_editor_set_state(
        playhead: f64,
        in_point: f64,
        out_point: f64,
        playing: i32,
        zoom: i32,
        smooth: i32,
        preset: i32,
        can_zoom: i32,
        can_smooth: i32,
        busy: i32,
        time_text: *const c_char,
    );
    fn bugle_video_editor_close();
    fn bugle_video_editor_active() -> i32;
    fn bugle_video_editor_focus();
    fn bugle_video_editor_inject(kind: i32, value: f64, arg: i32);
}

/// A button or keyboard command of the video editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEditorCommand {
    /// Play button or Space.
    PlayPause = 0,
    /// Set In or I: start the clip at the playhead.
    MarkIn = 1,
    /// Set Out or O: end the clip at the playhead.
    MarkOut = 2,
    /// Export or ⌘E.
    Export = 3,
    /// ←: one frame back.
    StepBack = 4,
    /// →: one frame forward.
    StepForward = 5,
    /// Esc.
    Escape = 6,
    /// ↑ or Home: to the in point.
    GoIn = 7,
    /// ↓ or End: to the out point.
    GoOut = 8,
}

impl VideoEditorCommand {
    const ALL: [Self; 9] = [
        Self::PlayPause,
        Self::MarkIn,
        Self::MarkOut,
        Self::Export,
        Self::StepBack,
        Self::StepForward,
        Self::Escape,
        Self::GoIn,
        Self::GoOut,
    ];

    fn from_raw(arg: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|c| *c as i32 == arg)
    }
}

/// Input from the video editor window. Times are seconds on the video's
/// timeline, between 0 and the duration given to [`video_editor_open`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VideoEditorEvent {
    /// The playhead was clicked or dragged to this time.
    Seek(f64),
    /// The in handle was dragged to this time.
    SetIn(f64),
    /// The out handle was dragged to this time.
    SetOut(f64),
    Command(VideoEditorCommand),
    /// A zoom choice, by index into the `zoom_labels` given at open.
    Zoom(usize),
    /// The *Smooth pointer* check box.
    Smooth(bool),
    /// An export preset, by index into the `preset_labels` given at open.
    Preset(usize),
}

impl VideoEditorEvent {
    fn from_raw(kind: i32, value: f64, arg: i32) -> Option<Self> {
        let time = value.is_finite().then_some(value);
        Some(match kind {
            0 => Self::Seek(time?),
            1 => Self::SetIn(time?),
            2 => Self::SetOut(time?),
            3 => Self::Command(VideoEditorCommand::from_raw(arg)?),
            4 => Self::Zoom(usize::try_from(arg).ok()?),
            5 => Self::Smooth(arg != 0),
            6 => Self::Preset(usize::try_from(arg).ok()?),
            _ => return None,
        })
    }

    fn to_raw(self) -> (i32, f64, i32) {
        let index = |i: usize| i32::try_from(i).unwrap_or(-1);
        match self {
            Self::Seek(t) => (0, t, 0),
            Self::SetIn(t) => (1, t, 0),
            Self::SetOut(t) => (2, t, 0),
            Self::Command(c) => (3, 0.0, c as i32),
            Self::Zoom(i) => (4, 0.0, index(i)),
            Self::Smooth(on) => (5, 0.0, on as i32),
            Self::Preset(i) => (6, 0.0, index(i)),
        }
    }
}

/// What [`video_editor_set_state`] shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VideoEditorState {
    pub playhead: f64,
    pub in_point: f64,
    pub out_point: f64,
    pub playing: bool,
    /// Selected zoom choice and preset, by index.
    pub zoom: usize,
    pub preset: usize,
    pub smooth: bool,
    /// Whether the zoom menu and the smoothing check box can be used (the
    /// recording has a usable event track).
    pub can_zoom: bool,
    pub can_smooth: bool,
    /// An export is running: controls are disabled and the window cannot
    /// be closed.
    pub busy: bool,
    /// The line next to the play button (times, clip length).
    pub time_text: String,
}

type Handler = RefCell<Box<dyn FnMut(VideoEditorEvent)>>;

unsafe extern "C" fn call(ctx: *mut c_void, kind: i32, value: f64, arg: i32) {
    // SAFETY: ctx is the Box<Handler> leaked in `video_editor_open`; the
    // shim frees it through `free` only after the window is gone, on a
    // later main-loop turn.
    let handler = unsafe { &*(ctx as *const Handler) };
    let Some(event) = VideoEditorEvent::from_raw(kind, value, arg) else {
        return;
    };
    match handler.try_borrow_mut() {
        Ok(mut h) => h(event),
        Err(_) => tracing::warn!("video editor: dropped re-entrant event {event:?}"),
    };
}

unsafe extern "C" fn free(ctx: *mut c_void) {
    drop(unsafe { Box::from_raw(ctx as *mut Handler) });
}

/// Open the video editor for a recording `duration` seconds long with
/// frames of `size` pixels (used to size the window) and bring it to the
/// front. `zoom_labels` and `presets` fill the two pop-up menus. `handler`
/// runs on the main thread for every event and is dropped when the window
/// closes, however it closes. Fails when a video editor is already open
/// (see [`video_editor_active`]).
pub fn video_editor_open(
    title: &str,
    duration: f64,
    size: (u32, u32),
    zoom_labels: &[&str],
    presets: &[&str],
    handler: impl FnMut(VideoEditorEvent) + 'static,
) -> Result<()> {
    if !(duration.is_finite() && duration > 0.0) {
        bail!("the recording has no duration");
    }
    let title = cstr(title)?;
    let strings = |v: &[&str]| v.iter().map(|s| cstr(s)).collect::<Result<Vec<CString>>>();
    let (zooms, preset_names) = (strings(zoom_labels)?, strings(presets)?);
    let ptrs = |v: &[CString]| v.iter().map(|c| c.as_ptr()).collect::<Vec<*const c_char>>();
    let (zoom_ptrs, preset_ptrs) = (ptrs(&zooms), ptrs(&preset_names));
    let boxed: Box<Handler> = Box::new(RefCell::new(Box::new(handler)));
    let ctx = Box::into_raw(boxed) as *mut c_void;
    let ok = unsafe {
        bugle_video_editor_open(
            title.as_ptr(),
            duration,
            size.0,
            size.1,
            zoom_ptrs.as_ptr(),
            zoom_ptrs.len() as i32,
            preset_ptrs.as_ptr(),
            preset_ptrs.len() as i32,
            call,
            ctx,
            free,
        )
    };
    if ok != 1 {
        // The shim did not take the context.
        drop(unsafe { Box::from_raw(ctx as *mut Handler) });
        bail!("could not open the video editor window (is one already open?)");
    }
    Ok(())
}

/// Show `img` in the preview, scaled to fit. No-op when no video editor is
/// open.
pub fn video_editor_set_frame(img: &RgbaImage) {
    unsafe { bugle_video_editor_set_frame(img.as_raw().as_ptr(), img.width(), img.height()) }
}

pub fn video_editor_set_state(s: &VideoEditorState) {
    let i = |v: usize| i32::try_from(v).unwrap_or(-1);
    let text = CString::new(s.time_text.replace('\0', "")).unwrap_or_default();
    unsafe {
        bugle_video_editor_set_state(
            s.playhead,
            s.in_point,
            s.out_point,
            s.playing as i32,
            i(s.zoom),
            s.smooth as i32,
            i(s.preset),
            s.can_zoom as i32,
            s.can_smooth as i32,
            s.busy as i32,
            text.as_ptr(),
        )
    }
}

/// Close the video editor. May be called from inside the handler, which is
/// dropped on a later main-loop turn.
pub fn video_editor_close() {
    unsafe { bugle_video_editor_close() }
}

/// Whether a video editor window is open.
pub fn video_editor_active() -> bool {
    unsafe { bugle_video_editor_active() == 1 }
}

/// Bring the open video editor to the front.
pub fn video_editor_focus() {
    unsafe { bugle_video_editor_focus() }
}

/// Deliver `event` to the handler as if the window had reported it. For
/// smoke tests, which cannot click. Not from inside the handler.
pub fn video_editor_inject(event: VideoEditorEvent) {
    let (kind, value, arg) = event.to_raw();
    unsafe { bugle_video_editor_inject(kind, value, arg) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip() {
        let mut all = vec![
            VideoEditorEvent::Seek(1.25),
            VideoEditorEvent::SetIn(0.0),
            VideoEditorEvent::SetOut(12.5),
            VideoEditorEvent::Zoom(3),
            VideoEditorEvent::Smooth(true),
            VideoEditorEvent::Smooth(false),
            VideoEditorEvent::Preset(8),
        ];
        all.extend(VideoEditorCommand::ALL.map(VideoEditorEvent::Command));
        for e in all {
            let (kind, value, arg) = e.to_raw();
            assert_eq!(VideoEditorEvent::from_raw(kind, value, arg), Some(e), "{e:?}");
        }
    }

    #[test]
    fn bad_events_are_dropped() {
        assert_eq!(VideoEditorEvent::from_raw(0, f64::NAN, 0), None);
        assert_eq!(VideoEditorEvent::from_raw(1, f64::INFINITY, 0), None);
        assert_eq!(VideoEditorEvent::from_raw(3, 0.0, 42), None);
        assert_eq!(VideoEditorEvent::from_raw(4, 0.0, -1), None);
        assert_eq!(VideoEditorEvent::from_raw(6, 0.0, -3), None);
        assert_eq!(VideoEditorEvent::from_raw(99, 0.0, 0), None);
    }
}
