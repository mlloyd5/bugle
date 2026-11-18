//! Safe Rust API over the macOS shim.
//!
//! Every function here is synchronous and may block for a few hundred
//! milliseconds (ScreenCaptureKit round-trips). Call from a blocking thread
//! when inside an async runtime. The two `*_blocking` UI functions must run
//! on the process main thread; the engine spawns a helper process for them.

#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
mod picker;
#[cfg(target_os = "macos")]
pub use picker::{window_pick_active, window_pick_begin, window_pick_cancel, window_pick_highlight, window_pick_switch, PickEvent};
#[cfg(target_os = "macos")]
mod panel;
#[cfg(target_os = "macos")]
pub use panel::{
    open_with, panel_count, panel_dismiss, panel_dismiss_all, panel_hide_all, panel_hovered, panel_set_image, panel_show,
    panel_show_all, reveal_path, Corner, Opened, PanelChoice,
};
#[cfg(target_os = "macos")]
pub mod login_item;
#[cfg(target_os = "macos")]
pub mod notify;
#[cfg(target_os = "macos")]
mod prompt;
#[cfg(target_os = "macos")]
pub use prompt::{prompt_active, prompt_answer, prompt_text};
#[cfg(target_os = "macos")]
mod editor;
#[cfg(target_os = "macos")]
pub use editor::{
    editor_begin_text, editor_close, editor_count, editor_focus, editor_inject, editor_is_open, editor_open, editor_set_busy,
    editor_set_image, editor_set_menu, editor_set_overlay, editor_set_state, editor_zoom, EditorCommand, EditorEvent, EditorId,
    EditorMenu, EditorMenuItem, EditorPrim, EditorState, EditorTool, EditorZoom,
};
#[cfg(target_os = "macos")]
mod video_editor;
#[cfg(target_os = "macos")]
pub use video_editor::{
    video_editor_active, video_editor_close, video_editor_focus, video_editor_inject, video_editor_open, video_editor_set_frame,
    video_editor_set_state, video_editor_set_waveform, video_editor_waveform, ClickMarker, VideoEditorCommand, VideoEditorEvent,
    VideoEditorState,
};

use anyhow::{anyhow, bail, Context, Result};
use bugle_core::audio_level::{AudioLevels, TrackLevel};
use bugle_core::audio_mix::{AudioMixConfig, Leveler, Mixer};
use bugle_core::{BugleError, DisplayInfo, OcrLine, Point, Rect, WindowInfo};
use image::RgbaImage;
use std::ffi::{CStr, CString};
use std::path::Path;
use std::sync::atomic::AtomicBool;

#[cfg(not(target_os = "macos"))]
compile_error!("bugle-platform currently supports macOS only");

/// Takes the shim's error for the current thread. The shim keeps one error
/// per thread, so this must run on the thread that made the failing call,
/// before any other shim call on it.
fn last_error() -> String {
    unsafe {
        let p = ffi::bugle_last_error();
        if p.is_null() {
            return "unknown platform error".into();
        }
        let s = CStr::from_ptr(p).to_string_lossy().into_owned();
        ffi::bugle_free(p as *mut _);
        s
    }
}

fn take_image(mut img: ffi::bugle_image_t) -> Result<RgbaImage> {
    if img.data.is_null() || img.width <= 0 || img.height <= 0 {
        let e = last_error();
        tracing::debug!("platform returned empty image: data_null={} {}x{}: {e}", img.data.is_null(), img.width, img.height);
        if !img.data.is_null() {
            unsafe { ffi::bugle_image_free(&mut img) };
        }
        if !screen_access_preflight() {
            return Err(BugleError::PermissionDenied.into());
        }
        bail!("{e}");
    }
    let (w, h) = (img.width as u32, img.height as u32);
    let Some(len) = rgba_len(w, h) else {
        unsafe { ffi::bugle_image_free(&mut img) };
        bail!("platform returned an image of impossible size {w}x{h}");
    };
    let buf = unsafe { std::slice::from_raw_parts(img.data, len) }.to_vec();
    unsafe { ffi::bugle_image_free(&mut img) };
    RgbaImage::from_raw(w, h, buf).ok_or_else(|| anyhow!("bad image buffer"))
}

/// Largest capture side the shim accepts, in points (and in pixels for the
/// ScreenCaptureKit configuration). Mirrors `BUGLE_MAX_PX` in shim.m.
const MAX_CAPTURE_SIDE: f64 = 16384.0;

/// Byte length of a `w` x `h` RGBA8 image, or `None` when it overflows.
fn rgba_len(w: u32, h: u32) -> Option<usize> {
    (w as usize).checked_mul(h as usize)?.checked_mul(4)
}

/// Validates an RGBA8 image for the FFI: both sides positive and within
/// `i32`, and the buffer at least `w * h * 4` bytes long.
fn ffi_dims(w: u32, h: u32, buf_len: usize) -> Result<(i32, i32)> {
    if w == 0 || h == 0 {
        return Err(BugleError::Invalid(format!("empty image ({w}x{h})")).into());
    }
    let (Ok(wi), Ok(hi)) = (i32::try_from(w), i32::try_from(h)) else {
        return Err(BugleError::Invalid(format!("image too large ({w}x{h})")).into());
    };
    match rgba_len(w, h) {
        Some(need) if buf_len >= need => Ok((wi, hi)),
        Some(need) => {
            Err(BugleError::Invalid(format!("image buffer is {buf_len} bytes, {w}x{h} needs {need}")).into())
        }
        None => Err(BugleError::Invalid(format!("image too large ({w}x{h})")).into()),
    }
}

fn cstr(s: &str) -> Result<CString> {
    CString::new(s).context("string contains NUL")
}

fn path_cstr(p: &Path) -> Result<CString> {
    cstr(p.to_str().ok_or_else(|| anyhow!("non-UTF8 path"))?)
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

/// Whether this process (or its responsible app) has Screen Recording access.
pub fn screen_access_preflight() -> bool {
    unsafe { ffi::bugle_screen_access_preflight() != 0 }
}

/// Ask macOS to prompt for Screen Recording access. Returns current state.
pub fn screen_access_request() -> bool {
    unsafe { ffi::bugle_screen_access_request() != 0 }
}

/// Accessibility trust (needed to post scroll events for scrolling capture).
pub fn accessibility_trusted(prompt: bool) -> bool {
    unsafe { ffi::bugle_accessibility_trusted(prompt as i32) != 0 }
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

pub fn displays() -> Result<Vec<DisplayInfo>> {
    let mut buf = vec![
        ffi::bugle_display_t { id: 0, x: 0.0, y: 0.0, w: 0.0, h: 0.0, scale: 1.0, is_main: 0 };
        32
    ];
    let n = unsafe { ffi::bugle_list_displays(buf.as_mut_ptr(), buf.len() as i32) };
    if n < 0 {
        bail!("could not enumerate displays");
    }
    Ok(buf[..n as usize]
        .iter()
        .map(|d| DisplayInfo {
            id: d.id,
            frame: Rect::new(d.x, d.y, d.w, d.h),
            scale: d.scale,
            is_main: d.is_main != 0,
        })
        .collect())
}

pub fn main_display() -> Result<DisplayInfo> {
    let ds = displays()?;
    ds.iter().find(|d| d.is_main).cloned().or_else(|| ds.first().cloned()).ok_or_else(|| anyhow!("no displays"))
}

fn c_field(buf: &[libc::c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

/// Windows known to the window server. Titles are only populated when the
/// process has Screen Recording access (macOS privacy rule).
pub fn windows(on_screen_only: bool) -> Result<Vec<WindowInfo>> {
    // SAFETY: an all-zero bugle_window_t is valid (integers, floats, char arrays).
    let zero: ffi::bugle_window_t = unsafe { std::mem::zeroed() };
    let mut cap = 512usize;
    let buf = loop {
        let mut buf = vec![zero; cap];
        let total = unsafe { ffi::bugle_list_windows(buf.as_mut_ptr(), cap as i32, on_screen_only as i32) };
        if total < 0 {
            bail!("could not enumerate windows");
        }
        let total = total as usize;
        if total <= cap {
            buf.truncate(total);
            break buf;
        }
        // More windows than room: grow (with slack, the list can change
        // between calls) and ask again.
        cap = total + total / 4 + 16;
    };
    Ok(buf
        .iter()
        .map(|w| WindowInfo {
            id: w.id,
            pid: w.pid,
            owner: c_field(&w.owner),
            title: c_field(&w.title),
            frame: Rect::new(w.x, w.y, w.w, w.h),
            layer: w.layer,
            on_screen: w.on_screen != 0,
            alpha: w.alpha,
        })
        .collect())
}

/// "Real" application windows: layer 0, reasonably sized, with an owner.
pub fn app_windows() -> Result<Vec<WindowInfo>> {
    Ok(windows(true)?
        .into_iter()
        .filter(|w| w.layer == 0 && w.frame.w >= 50.0 && w.frame.h >= 50.0 && !w.owner.is_empty())
        .collect())
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

pub fn capture_display(display_id: Option<u32>, show_cursor: bool) -> Result<RgbaImage> {
    take_image(unsafe { ffi::bugle_capture_display(display_id.unwrap_or(0), show_cursor as i32) })
}

pub fn capture_area(rect: Rect, show_cursor: bool) -> Result<RgbaImage> {
    if ![rect.x, rect.y, rect.w, rect.h].iter().all(|v| v.is_finite()) {
        return Err(BugleError::Invalid(format!("non-finite capture rect {rect}")).into());
    }
    if rect.is_empty() {
        return Err(BugleError::Invalid("empty capture rect".into()).into());
    }
    if rect.w > MAX_CAPTURE_SIDE || rect.h > MAX_CAPTURE_SIDE {
        return Err(BugleError::Invalid(format!("capture rect {rect} exceeds {MAX_CAPTURE_SIDE} points per side")).into());
    }
    take_image(unsafe { ffi::bugle_capture_area(rect.x, rect.y, rect.w, rect.h, show_cursor as i32) })
}

pub fn capture_window(window_id: u32, shadow: bool, show_cursor: bool) -> Result<RgbaImage> {
    take_image(unsafe { ffi::bugle_capture_window(window_id, shadow as i32, show_cursor as i32) })
}

// ---------------------------------------------------------------------------
// Vision
// ---------------------------------------------------------------------------

pub struct OcrParams<'a> {
    pub languages: &'a [String],
    pub accurate: bool,
    pub language_correction: bool,
}

/// Apple Vision text recognition. Bounding boxes are returned in image
/// pixels with a top-left origin, reading order top to bottom.
pub fn ocr(img: &RgbaImage, params: &OcrParams) -> Result<Vec<OcrLine>> {
    let (w, h) = ffi_dims(img.width(), img.height(), img.as_raw().len())?;
    let langs = cstr(&params.languages.join(","))?;
    let mut lines: *mut ffi::bugle_ocr_line_t = std::ptr::null_mut();
    let mut count: i32 = 0;
    let rc = unsafe {
        ffi::bugle_ocr(
            img.as_raw().as_ptr(),
            img.as_raw().len(),
            w,
            h,
            langs.as_ptr(),
            params.accurate as i32,
            params.language_correction as i32,
            &mut lines,
            &mut count,
        )
    };
    if rc != 0 {
        bail!("{}", last_error());
    }
    if lines.is_null() || count <= 0 {
        unsafe { ffi::bugle_ocr_free(lines, count.max(0)) };
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let l = unsafe { &*lines.add(i) };
        let text = if l.text.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(l.text) }.to_string_lossy().into_owned()
        };
        out.push(OcrLine { text, bbox: Rect::new(l.x, l.y, l.w, l.h), confidence: l.confidence });
    }
    unsafe { ffi::bugle_ocr_free(lines, count) };
    sort_reading_order(&mut out);
    Ok(out)
}

/// Order OCR lines top-to-bottom, then left-to-right within a row. Rows are
/// bucketed by the median line height so the comparator is a total order.
pub fn sort_reading_order(lines: &mut [OcrLine]) {
    if lines.is_empty() {
        return;
    }
    let mut heights: Vec<f64> = lines.iter().map(|l| l.bbox.h).collect();
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = heights[heights.len() / 2].max(1.0);
    let key = |l: &OcrLine| -> (i64, i64) {
        let row = ((l.bbox.y + l.bbox.h / 2.0) / (median * 0.9)).floor() as i64;
        (row, l.bbox.x.round() as i64)
    };
    lines.sort_by_key(key);
}

/// Translation (dx, dy) in pixels that aligns `floating` onto `reference`,
/// via `VNTranslationalImageRegistrationRequest`.
pub fn register_translation(reference: &RgbaImage, floating: &RgbaImage) -> Result<(f64, f64)> {
    let (rw, rh) = ffi_dims(reference.width(), reference.height(), reference.as_raw().len())?;
    let (fw, fh) = ffi_dims(floating.width(), floating.height(), floating.as_raw().len())?;
    let (mut dx, mut dy) = (0.0, 0.0);
    let rc = unsafe {
        ffi::bugle_register_translation(
            reference.as_raw().as_ptr(),
            reference.as_raw().len(),
            rw,
            rh,
            floating.as_raw().as_ptr(),
            floating.as_raw().len(),
            fw,
            fh,
            &mut dx,
            &mut dy,
        )
    };
    if rc != 0 {
        bail!("{}", last_error());
    }
    Ok((dx, dy))
}

// ---------------------------------------------------------------------------
// Input, clipboard, UI
// ---------------------------------------------------------------------------

pub fn post_scroll(at: Point, dx_lines: i32, dy_lines: i32) -> Result<()> {
    if unsafe { ffi::bugle_post_scroll(at.x, at.y, dx_lines, dy_lines) } != 0 {
        bail!("could not post scroll event");
    }
    Ok(())
}

pub fn cursor_position() -> Point {
    let (mut x, mut y) = (0.0, 0.0);
    unsafe { ffi::bugle_cursor_position(&mut x, &mut y) };
    Point { x, y }
}

/// Mouse buttons held down right now: bit 0 left, bit 1 right, bit 2 middle.
/// Reads the session state (no permission needed, no events posted).
pub fn mouse_buttons() -> u32 {
    unsafe { ffi::bugle_mouse_buttons() as u32 }
}

/// Now on the host clock, in seconds: the clock ScreenCaptureKit stamps
/// frames with (see [`SckRecorder::session_start`]).
pub fn host_time_now() -> f64 {
    unsafe { ffi::bugle_host_time_now() }
}

/// A window's current frame (global points), `None` once it is gone.
pub fn window_frame(window_id: u32) -> Option<Rect> {
    let (mut x, mut y, mut w, mut h) = (0.0, 0.0, 0.0, 0.0);
    (unsafe { ffi::bugle_window_frame(window_id, &mut x, &mut y, &mut w, &mut h) } == 0).then_some(Rect { x, y, w, h })
}

/// Whether child process `pid` is no longer running: it exited (not
/// reaped by this call, so its `std::process::Child` can still be waited
/// on), or it is no longer waitable at all because its owner already reaped
/// it. Only "still running" answers `false`.
pub fn child_exited(pid: u32) -> bool {
    unsafe { ffi::bugle_child_exited(pid as i32) != 0 }
}

pub fn clipboard_write_png(png: &[u8]) -> Result<()> {
    if unsafe { ffi::bugle_clipboard_write_image(png.as_ptr(), png.len()) } != 0 {
        bail!("clipboard write failed");
    }
    Ok(())
}

pub fn clipboard_write_text(text: &str) -> Result<()> {
    let c = cstr(text)?;
    if unsafe { ffi::bugle_clipboard_write_text(c.as_ptr()) } != 0 {
        bail!("clipboard write failed");
    }
    Ok(())
}

pub fn clipboard_write_file(path: &Path) -> Result<()> {
    let c = path_cstr(path)?;
    if unsafe { ffi::bugle_clipboard_write_file(c.as_ptr()) } != 0 {
        bail!("clipboard write failed");
    }
    Ok(())
}

/// Show the dimmed crosshair overlay and let the user drag a rectangle.
/// Main thread only. Returns `None` when cancelled with Esc.
pub fn select_rect_blocking() -> Option<Rect> {
    let (mut x, mut y, mut w, mut h) = (0.0, 0.0, 0.0, 0.0);
    let ok = unsafe { ffi::bugle_select_rect_blocking(&mut x, &mut y, &mut w, &mut h) };
    (ok == 1).then(|| Rect::new(x, y, w, h))
}

/// Float an image above all windows until closed. Main thread only.
/// Pass a negative origin to place it at the top-right of the main screen,
/// and a zero size to use the image's natural point size.
pub fn pin_image_blocking(png: &[u8], frame: Option<Rect>, opacity: f64) {
    let f = frame.unwrap_or(Rect::new(-1.0, -1.0, 0.0, 0.0));
    unsafe { ffi::bugle_pin_image_blocking(png.as_ptr(), png.len(), f.x, f.y, f.w, f.h, opacity) }
}

/// Non-blocking area selection for hosts whose AppKit run loop is already
/// running (the menu-bar app). `done` is called once on the main thread with
/// the rect, or `None` when cancelled. Returns `false` (and never calls
/// `done`) when a selection is already in progress. Main thread only.
pub fn select_rect_begin(done: impl FnOnce(Option<Rect>) + 'static) -> bool {
    type Cb = Box<dyn FnOnce(Option<Rect>)>;
    unsafe extern "C" fn trampoline(ctx: *mut libc::c_void, ok: i32, x: f64, y: f64, w: f64, h: f64) {
        let cb: Box<Cb> = unsafe { Box::from_raw(ctx as *mut Cb) };
        cb((ok == 1).then(|| Rect::new(x, y, w, h)));
    }
    let boxed: Box<Cb> = Box::new(Box::new(done));
    let ctx = Box::into_raw(boxed);
    let started = unsafe { ffi::bugle_select_rect_begin(trampoline, ctx as *mut libc::c_void) } == 1;
    if !started {
        drop(unsafe { Box::from_raw(ctx) });
    }
    started
}

/// How a switchable area selection ended, see [`select_rect_begin_ex`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SelectOutcome {
    Selected(Rect),
    /// Esc, the timeout or [`select_rect_cancel`].
    Cancelled,
    /// Space: the user wants the window picker instead. Bugle is left active
    /// so the picker can open at once.
    Switch,
}

/// [`select_rect_begin`] with two extras: when `allow_switch` is set, Space
/// (while not dragging) ends the selection with [`SelectOutcome::Switch`];
/// a `timeout` cancels the overlay after that long. `done` runs once on the
/// main thread. Returns `false` (and never calls `done`) when a selection or
/// window picker is already on screen. Main thread only.
pub fn select_rect_begin_ex(allow_switch: bool, timeout: Option<std::time::Duration>, done: impl FnOnce(SelectOutcome) + 'static) -> bool {
    type Cb = Box<dyn FnOnce(SelectOutcome)>;
    unsafe extern "C" fn trampoline(ctx: *mut libc::c_void, result: i32, x: f64, y: f64, w: f64, h: f64) {
        let cb: Box<Cb> = unsafe { Box::from_raw(ctx as *mut Cb) };
        cb(match result {
            1 => SelectOutcome::Selected(Rect::new(x, y, w, h)),
            2 => SelectOutcome::Switch,
            _ => SelectOutcome::Cancelled,
        });
    }
    let boxed: Box<Cb> = Box::new(Box::new(done));
    let ctx = Box::into_raw(boxed);
    let secs = timeout.map(|t| t.as_secs_f64().max(0.5)).unwrap_or(0.0);
    let started = unsafe { ffi::bugle_select_rect_begin_ex(trampoline, ctx as *mut libc::c_void, allow_switch as i32, secs) } == 1;
    if !started {
        drop(unsafe { Box::from_raw(ctx) });
    }
    started
}

/// Hand keyboard focus back to the app the user was in (`[NSApp deactivate]`).
/// Main thread only.
pub fn app_deactivate() {
    unsafe { ffi::bugle_app_deactivate() }
}

/// Prepare AppKit for a process that has no run loop of its own (the CLI):
/// an accessory app (no Dock icon) that has finished launching. Idempotent.
/// Main thread only; pair with [`run_loop_once`].
pub fn app_bootstrap() {
    unsafe { ffi::bugle_app_bootstrap() }
}

/// Handle AppKit events (and the main dispatch queue, and timers) for up to
/// `slice`, returning after the first event or when it runs out. Lets a
/// caller without `[NSApp run]` drive the non-blocking overlays such as
/// [`window_pick_begin`] and [`select_rect_begin_ex`] while checking its own
/// state in between. Main thread only, after [`app_bootstrap`].
pub fn run_loop_once(slice: std::time::Duration) {
    unsafe { ffi::bugle_run_loop_once(slice.as_secs_f64()) }
}

/// Whether the caller is on the process main thread, where AppKit overlays
/// must run.
pub fn is_main_thread() -> bool {
    unsafe { ffi::bugle_is_main_thread() == 1 }
}

/// The title of Bugle's key window, or `None` when Bugle is not the active
/// app or none of its windows is key.
/// For smoke tests. From a thread other than the main one it waits for the
/// main thread, which must be running its loop.
pub fn key_window_title() -> Option<String> {
    let mut buf = vec![0u8; 512];
    let found = unsafe { ffi::bugle_key_window_title(buf.as_mut_ptr().cast(), buf.len()) };
    if found == 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// Whether a selection overlay is currently on screen.
pub fn select_rect_active() -> bool {
    unsafe { ffi::bugle_select_rect_active() == 1 }
}

/// Dismiss the selection overlay as if Esc was pressed. Main thread only.
pub fn select_rect_cancel() {
    unsafe { ffi::bugle_select_rect_cancel() }
}

/// Do what Space does in a selection opened by [`select_rect_begin_ex`] with
/// `allow_switch`: end it with [`SelectOutcome::Switch`] unless a drag is in
/// progress. Otherwise nothing. Main thread only.
pub fn select_rect_switch() {
    unsafe { ffi::bugle_select_rect_switch() }
}

/// Show a pin window and return immediately; it stays until the user closes
/// it. Main thread only, needs a running AppKit loop. See `pin_image_blocking`
/// for the frame conventions.
pub fn pin_image_show(png: &[u8], frame: Option<Rect>, opacity: f64) -> Result<()> {
    let f = frame.unwrap_or(Rect::new(-1.0, -1.0, 0.0, 0.0));
    if unsafe { ffi::bugle_pin_image_show(png.as_ptr(), png.len(), f.x, f.y, f.w, f.h, opacity) } != 1 {
        bail!("could not decode image for pin window");
    }
    Ok(())
}

/// Show a transient corner notification with an optional PNG thumbnail.
/// Clicking it opens `open_path`. Main thread only, needs a running AppKit loop.
pub fn toast_show(png: Option<&[u8]>, title: &str, subtitle: &str, open_path: Option<&Path>, seconds: f64) {
    let (Ok(t), Ok(st)) = (cstr(title), cstr(subtitle)) else { return };
    let op = open_path.and_then(|p| path_cstr(p).ok());
    let (ptr, len) = png.map(|p| (p.as_ptr(), p.len())).unwrap_or((std::ptr::null(), 0));
    unsafe {
        ffi::bugle_toast_show(
            ptr,
            len,
            t.as_ptr(),
            st.as_ptr(),
            op.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null()),
            seconds,
        )
    }
}

// ---------------------------------------------------------------------------
// MediaItem
// ---------------------------------------------------------------------------

/// Extract the audio track of a movie to an AAC `.m4a` (what speech-to-text
/// providers want). Errors with a distinct message when there is no audio.
pub fn export_audio_m4a(input: &Path, output: &Path) -> Result<()> {
    let i = path_cstr(input)?;
    let o = path_cstr(output)?;
    match unsafe { ffi::bugle_export_audio_m4a(i.as_ptr(), o.as_ptr()) } {
        0 => Ok(()),
        -2 => bail!("{} has no audio track", input.display()),
        _ => bail!("{}", last_error()),
    }
}

/// Remux a QuickTime movie into an MP4 container without re-encoding.
pub fn export_mp4(input: &Path, output: &Path) -> Result<()> {
    let i = path_cstr(input)?;
    let o = path_cstr(output)?;
    if unsafe { ffi::bugle_export_mp4(i.as_ptr(), o.as_ptr()) } != 0 {
        bail!("{}", last_error());
    }
    Ok(())
}

/// Remux an MP4 into a QuickTime `.mov` container without re-encoding.
pub fn export_mov(input: &Path, output: &Path) -> Result<()> {
    let i = path_cstr(input)?;
    let o = path_cstr(output)?;
    if unsafe { ffi::bugle_export_mov(i.as_ptr(), o.as_ptr()) } != 0 {
        bail!("{}", last_error());
    }
    Ok(())
}

/// Number of audio tracks in a movie (0 for a file AVFoundation cannot read).
pub fn audio_track_count(path: &Path) -> Result<usize> {
    let p = path_cstr(path)?;
    Ok(unsafe { ffi::bugle_audio_track_count(p.as_ptr()) }.max(0) as usize)
}

/// Most buckets [`audio_peaks`] fills.
pub const AUDIO_PEAKS_MAX: usize = 1 << 16;

/// The loudest sample of a movie's audio (every enabled track mixed) in each
/// of `buckets` equal slices of its first `duration` seconds, full scale 1:
/// a timeline waveform. `None` when the movie has no audio, or when `cancel`
/// was set before the decode finished (it is checked once per sample buffer).
pub fn audio_peaks(path: &Path, duration: f64, buckets: usize, cancel: Option<&AtomicBool>) -> Result<Option<Vec<f32>>> {
    if !(duration.is_finite() && duration > 0.0) || buckets == 0 || buckets > AUDIO_PEAKS_MAX {
        bail!("audio peaks: bad duration {duration} or bucket count {buckets}");
    }
    let p = path_cstr(path)?;
    let mut out = vec![0f32; buckets];
    let cancel = cancel.map_or(std::ptr::null(), |c| c.as_ptr() as *const bool);
    match unsafe { ffi::bugle_audio_peaks(p.as_ptr(), duration, out.as_mut_ptr(), buckets as i32, cancel) } {
        1 => Ok(Some(out)),
        0 | 2 => Ok(None),
        _ => bail!("{}", last_error()),
    }
}

/// Number of video tracks in a movie (0 for a file AVFoundation cannot read).
pub fn video_track_count(path: &Path) -> Result<usize> {
    let p = path_cstr(path)?;
    Ok(unsafe { ffi::bugle_video_track_count(p.as_ptr()) }.max(0) as usize)
}

/// [`mix_audio_tracks_with`] at the default levels: each of the `n` sources
/// at `1/n`, no ducking.
pub fn mix_audio_tracks(input: &Path, output: &Path) -> Result<bool> {
    mix_audio_tracks_with(input, output, &AudioMixConfig::default())
}

/// Fold every enabled audio track of `input` into one AAC track in `output`
/// (a `.mov` extension writes QuickTime, anything else MP4) through
/// [`bugle_core::audio_mix::Mixer`]: each of the `n` sources gets a base
/// share of `1/n` (so at unity gains the sum cannot clip) scaled by its gain
/// in `levels`, system audio optionally ducked under the microphone, and a
/// peak limiter on the sum. Track 0 is system audio and track 1 the
/// microphone, the order the stream recorder writes them. The audio is
/// re-encoded; video is copied without re-encoding, frame times untouched.
/// An existing `output` is replaced. Returns `false`, writing nothing, when
/// the input has fewer than two audio tracks; `input` and `output` must be
/// different files.
pub fn mix_audio_tracks_with(input: &Path, output: &Path, levels: &AudioMixConfig) -> Result<bool> {
    run_audio_pass(input, output, levels, false)
}

/// Apply `levels` to each audio track of `input`, keeping them separate:
/// the same tracks, in the same order, re-encoded as AAC into `output`
/// (`.mov` writes QuickTime, anything else MP4) through
/// [`bugle_core::audio_mix::Leveler`]. Track 0 (system audio) is scaled by
/// `system_audio_gain` and optionally ducked under track 1 (the microphone),
/// which is scaled by `microphone_gain`; a gain of 1 leaves a track as
/// recorded, and a track above 1 is peak-limited. Video is copied without
/// re-encoding. Returns `false`, writing nothing, when the input has fewer
/// than two audio tracks (one track cannot say which source it is); `input`
/// and `output` must be different files.
pub fn level_audio_tracks(input: &Path, output: &Path, levels: &AudioMixConfig) -> Result<bool> {
    run_audio_pass(input, output, levels, true)
}

fn run_audio_pass(input: &Path, output: &Path, levels: &AudioMixConfig, separate: bool) -> Result<bool> {
    let what = if separate { "leveling" } else { "mixing" };
    if same_file(input, output) {
        return Err(BugleError::Invalid(format!("{what} {}: input and output are the same file", input.display())).into());
    }
    levels.validate().map_err(BugleError::Invalid)?;
    let i = path_cstr(input)?;
    let o = path_cstr(output)?;
    let mut state = MixState { levels: *levels, separate, processor: None };
    let ctx = &mut state as *mut MixState as *mut libc::c_void;
    match unsafe { ffi::bugle_mix_audio_tracks(i.as_ptr(), o.as_ptr(), separate as i32, mix_block, ctx) } {
        0 => Ok(true),
        1 => Ok(false),
        _ => bail!("{}", last_error()),
    }
}

/// What the shim's callback works on; the processor is built on the first
/// block, once the track count, channel count and sample rate (all set by
/// the shim) are known.
struct MixState {
    levels: AudioMixConfig,
    /// Keep the tracks apart ([`Leveler`]) instead of mixing them.
    separate: bool,
    processor: Option<((usize, usize, i32), Processor)>,
}

enum Processor {
    Mix(Mixer),
    Level(Leveler),
}

extern "C" fn mix_block(
    ctx: *mut libc::c_void,
    tracks: *const *const f32,
    n_tracks: i32,
    channels: i32,
    sample_rate: i32,
    out: *mut f32,
    n_out: i32,
    frames: i32,
) -> i32 {
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if ctx.is_null() || tracks.is_null() || out.is_null() || n_tracks <= 0 || channels <= 0 || sample_rate <= 0 || frames < 0 {
            return false;
        }
        let (n, ch, len) = (n_tracks as usize, channels as usize, frames as usize * channels as usize);
        // SAFETY: the shim calls this from one thread at a time while
        // `run_audio_pass` blocks with `state` alive; `tracks` holds `n`
        // pointers to `len` floats each and `out` has room for `n_out` runs
        // of `len`.
        let state = unsafe { &mut *(ctx as *mut MixState) };
        let want_out = if state.separate { n } else { 1 };
        if n_out as usize != want_out {
            return false;
        }
        let inputs: Vec<&[f32]> = unsafe { std::slice::from_raw_parts(tracks, n) }
            .iter()
            .map(|p| unsafe { std::slice::from_raw_parts(*p, len) })
            .collect();
        let out = unsafe { std::slice::from_raw_parts_mut(out, len * want_out) };
        let (levels, separate) = (state.levels, state.separate);
        let key = (n, ch, sample_rate);
        let (_, processor) = match &mut state.processor {
            Some((k, _)) if *k == key => state.processor.as_mut().unwrap(),
            _ => {
                let p = if separate {
                    Processor::Level(Leveler::new(&levels, n, ch, sample_rate as f32))
                } else {
                    Processor::Mix(Mixer::new(&levels, n, ch, sample_rate as f32))
                };
                state.processor.insert((key, p))
            }
        };
        match processor {
            Processor::Mix(m) => m.process(&inputs, out),
            Processor::Level(l) => l.process(&inputs, out),
        }
        true
    }));
    match run {
        Ok(true) => 0,
        _ => -1,
    }
}

/// Whether both paths name the same existing file (hard and symbolic links
/// included).
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Test fixture parameters for [`write_tone_fixture`].
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct ToneFixture {
    /// Number of AAC tracks (1-8).
    pub tracks: u32,
    pub seconds: f64,
    /// Sine amplitude, full scale = 1.0.
    pub amplitude: f64,
    /// All tracks play the same 440 Hz tone throughout; otherwise track `i`
    /// plays `440*(i+1)` Hz during the `i`-th equal slice only.
    pub overlap: bool,
    /// Tracks after the first start this many seconds in.
    pub delay: f64,
    /// Add a 64x64 H.264 video track at 10 fps.
    pub video: bool,
}

impl Default for ToneFixture {
    fn default() -> Self {
        Self { tracks: 2, seconds: 1.0, amplitude: 0.5, overlap: false, delay: 0.0, video: false }
    }
}

/// Test fixture: write an `.mp4` described by `f`. Lives in its own object
/// file, which shipped binaries do not reference and so do not contain.
#[doc(hidden)]
pub fn write_tone_fixture(output: &Path, f: &ToneFixture) -> Result<()> {
    let o = path_cstr(output)?;
    let e = unsafe {
        ffi::bugle_fixture_write_tones(
            o.as_ptr(),
            f.tracks.min(i32::MAX as u32) as i32,
            f.seconds,
            f.amplitude,
            f.overlap as i32,
            f.delay,
            f.video as i32,
        )
    };
    if !e.is_null() {
        let msg = unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned();
        unsafe { ffi::bugle_free(e as *mut _) };
        bail!("{msg}");
    }
    Ok(())
}

/// Sample layouts [`audio_level_fixture`] can build.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelFixtureFormat {
    F32 = 0,
    I16 = 1,
    F64 = 2,
    I32 = 3,
    /// Not readable by the meter.
    I16BigEndian = 4,
}

/// Test fixture: measure `samples` (channel-major, `channels` blocks of equal
/// length) the way the recorder's level meter does, packed as `format`,
/// interleaved or not. Returns (peak, RMS, seconds) at `rate`, or `None` when
/// the meter cannot read the format.
#[doc(hidden)]
pub fn audio_level_fixture(
    samples: &[f32],
    channels: usize,
    format: LevelFixtureFormat,
    interleaved: bool,
    rate: f64,
) -> Option<(f64, f64, f64)> {
    let frames = samples.len().checked_div(channels)?;
    let mut out = [0f64; 3];
    let rc = unsafe {
        ffi::bugle_fixture_audio_level(
            format as i32,
            channels as i32,
            interleaved as i32,
            samples.as_ptr(),
            frames as i32,
            rate,
            out.as_mut_ptr(),
        )
    };
    (rc == 0).then_some((out[0], out[1], out[2]))
}

pub fn media_duration(path: &Path) -> Result<f64> {
    let p = path_cstr(path)?;
    let d = unsafe { ffi::bugle_media_duration(p.as_ptr()) };
    if d.is_nan() || d < 0.0 {
        bail!("could not read duration of {}", path.display());
    }
    Ok(d)
}

pub fn video_frame(path: &Path, seconds: f64) -> Result<RgbaImage> {
    let p = path_cstr(path)?;
    take_image(unsafe { ffi::bugle_video_frame(p.as_ptr(), seconds) })
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

/// Display size (pixels, after the track's rotation) of a movie's first
/// video track.
pub fn video_size(path: &Path) -> Result<(u32, u32)> {
    let i = video_info(path)?;
    Ok((i.width, i.height))
}

/// Facts about a movie's first video track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoInfo {
    /// Display size in pixels (after the track's rotation).
    pub width: u32,
    pub height: u32,
    pub nominal_fps: f64,
    /// The video track's time range in seconds (an audio track may run
    /// longer than this).
    pub start: f64,
    pub duration: f64,
    /// Samples stored in the media, including any an edit list hides from
    /// playback; `None` when the file cannot say.
    pub stored_samples: Option<u64>,
}

impl VideoInfo {
    pub fn end(&self) -> f64 {
        self.start + self.duration
    }
}

pub fn video_info(path: &Path) -> Result<VideoInfo> {
    let p = path_cstr(path)?;
    let mut i = ffi::bugle_video_info_t::default();
    if unsafe { ffi::bugle_video_info(p.as_ptr(), &mut i) } != 0 {
        bail!("{}", last_error());
    }
    Ok(VideoInfo {
        width: i.width.max(0) as u32,
        height: i.height.max(0) as u32,
        nominal_fps: i.nominal_fps,
        start: i.start,
        duration: i.duration,
        stored_samples: u64::try_from(i.stored_samples).ok(),
    })
}

/// Whether the frame shown at `seconds` is a full sync (key) frame starting
/// exactly there, i.e. whether a passthrough cut at that time copies nothing
/// from before it.
pub fn video_sync_at(path: &Path, seconds: f64) -> Result<bool> {
    let p = path_cstr(path)?;
    match unsafe { ffi::bugle_video_sync_at(p.as_ptr(), seconds) } {
        1 => Ok(true),
        0 => Ok(false),
        _ => bail!("{}", last_error()),
    }
}

/// The `AVAssetExportSession` preset a video export runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoExportPreset {
    /// No re-encode: samples are copied (trim and container change only).
    Passthrough,
    /// H.264 at the source size.
    Highest,
    /// HEVC at the source size.
    HevcHighest,
    /// H.264 scaled to fit 1920x1080.
    Fit1920x1080,
    /// H.264 scaled to fit 1280x720.
    Fit1280x720,
    /// H.264 scaled to fit 960x540.
    Fit960x540,
}

impl VideoExportPreset {
    fn code(self) -> i32 {
        match self {
            Self::Passthrough => 0,
            Self::Highest => 1,
            Self::HevcHighest => 2,
            Self::Fit1920x1080 => 3,
            Self::Fit1280x720 => 4,
            Self::Fit960x540 => 5,
        }
    }
}

/// Export `start..end` seconds of `input` to `output` (MP4 when `mp4`,
/// QuickTime otherwise) with `preset`; audio tracks are kept. `fps`
/// resamples the video (an error with `Passthrough`). The export is cancelled
/// after `timeout` seconds (`None`: no limit). A failed export leaves no file
/// at `output`.
#[allow(clippy::too_many_arguments)]
pub fn export_video(
    input: &Path,
    output: &Path,
    start: f64,
    end: f64,
    preset: VideoExportPreset,
    mp4: bool,
    fps: Option<u32>,
    timeout: Option<f64>,
) -> Result<()> {
    export_video_ranges(input, output, &[(start, end)], preset, mp4, fps, timeout)
}

/// Flatten `[start, end)` ranges for the shim, which takes them as pairs.
fn flat_ranges(ranges: &[(f64, f64)]) -> Result<(Vec<f64>, i32)> {
    let n = i32::try_from(ranges.len()).map_err(|_| anyhow!("too many time ranges"))?;
    Ok((ranges.iter().flat_map(|&(s, e)| [s, e]).collect(), n))
}

/// [`export_video`] over several ranges of `input` (in order, not
/// overlapping), spliced back to back: the part between two ranges is cut
/// out. Audio follows the video.
pub fn export_video_ranges(
    input: &Path,
    output: &Path,
    ranges: &[(f64, f64)],
    preset: VideoExportPreset,
    mp4: bool,
    fps: Option<u32>,
    timeout: Option<f64>,
) -> Result<()> {
    let i = path_cstr(input)?;
    let o = path_cstr(output)?;
    let (r, n) = flat_ranges(ranges)?;
    let fps = fps.map(|f| f.min(240) as i32).unwrap_or(0);
    let timeout = timeout.filter(|t| t.is_finite() && *t > 0.0).unwrap_or(0.0);
    if unsafe { ffi::bugle_export_video(i.as_ptr(), o.as_ptr(), r.as_ptr(), n, preset.code(), mp4 as i32, fps, timeout) } != 0 {
        bail!("{}", last_error());
    }
    Ok(())
}

/// Decodes frames of one movie at arbitrary times, reusing one
/// `AVAssetImageGenerator` (much cheaper than [`video_frame`] per frame).
pub struct VideoFrameReader {
    handle: *mut libc::c_void,
}

// The generator is used from one thread at a time (`&mut self`).
unsafe impl Send for VideoFrameReader {}

impl VideoFrameReader {
    /// `tolerance` is how far (seconds, either side) a returned frame may be
    /// from the requested time; 0 decodes exactly.
    pub fn open(path: &Path, tolerance: f64) -> Result<Self> {
        let p = path_cstr(path)?;
        let handle = unsafe { ffi::bugle_frames_open(p.as_ptr(), tolerance) };
        if handle.is_null() {
            bail!("{}", last_error());
        }
        Ok(Self { handle })
    }

    pub fn frame_at(&mut self, seconds: f64) -> Result<RgbaImage> {
        let img = unsafe { ffi::bugle_frames_at(self.handle, seconds) };
        if img.data.is_null() {
            bail!("{}", last_error());
        }
        take_image(img)
    }

    /// Decode frames scaled down to fit `max_w` x `max_h` pixels, aspect
    /// kept (never scaled up); `(0, 0)` restores full size. For previews.
    pub fn set_max_size(&mut self, max_w: u32, max_h: u32) {
        unsafe { ffi::bugle_frames_set_max_size(self.handle, max_w, max_h) }
    }
}

impl Drop for VideoFrameReader {
    fn drop(&mut self) {
        unsafe { ffi::bugle_frames_close(self.handle) }
    }
}

/// Encodes RGBA frames of one fixed, even size to an H.264 (or HEVC) movie at
/// a constant frame rate. Dropping an unfinished writer discards the file.
pub struct VideoFrameWriter {
    handle: *mut libc::c_void,
    width: u32,
    height: u32,
}

unsafe impl Send for VideoFrameWriter {}

impl VideoFrameWriter {
    pub fn create(path: &Path, width: u32, height: u32, fps: u32, mp4: bool) -> Result<Self> {
        Self::create_with_codec(path, width, height, fps, mp4, false)
    }

    pub fn create_with_codec(path: &Path, width: u32, height: u32, fps: u32, mp4: bool, hevc: bool) -> Result<Self> {
        let p = path_cstr(path)?;
        let (Ok(w), Ok(h)) = (i32::try_from(width), i32::try_from(height)) else {
            return Err(BugleError::Invalid(format!("frame size {width}x{height} is too large")).into());
        };
        let fps = i32::try_from(fps).unwrap_or(i32::MAX);
        let handle = unsafe { ffi::bugle_writer_open(p.as_ptr(), w, h, fps, mp4 as i32, hevc as i32) };
        if handle.is_null() {
            bail!("{}", last_error());
        }
        Ok(Self { handle, width, height })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Append the next frame; it must be exactly [`Self::size`].
    pub fn append(&mut self, frame: &RgbaImage) -> Result<()> {
        if frame.dimensions() != (self.width, self.height) {
            return Err(BugleError::Invalid(format!(
                "frame is {}x{}, the writer expects {}x{}",
                frame.width(),
                frame.height(),
                self.width,
                self.height
            ))
            .into());
        }
        let (w, h) = ffi_dims(frame.width(), frame.height(), frame.as_raw().len())?;
        let raw = frame.as_raw();
        if unsafe { ffi::bugle_writer_append(self.handle, raw.as_ptr(), raw.len(), w, h) } != 0 {
            bail!("{}", last_error());
        }
        Ok(())
    }

    /// Finalise the movie (waits at most two minutes).
    pub fn finish(mut self) -> Result<()> {
        let h = std::mem::replace(&mut self.handle, std::ptr::null_mut());
        if unsafe { ffi::bugle_writer_finish(h, 120.0) } != 0 {
            bail!("{}", last_error());
        }
        Ok(())
    }
}

impl Drop for VideoFrameWriter {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { ffi::bugle_writer_cancel(self.handle) }
        }
    }
}

/// Decodes a movie's video track in presentation order over a time range,
/// one decode per frame (`AVAssetReader`). The first frame may start before
/// the range: it is the one shown at its start.
pub struct SequentialFrameReader {
    handle: *mut libc::c_void,
}

unsafe impl Send for SequentialFrameReader {}

impl SequentialFrameReader {
    /// Read `start..end` seconds; `end <= start` reads the whole track.
    pub fn open(path: &Path, start: f64, end: f64) -> Result<Self> {
        let p = path_cstr(path)?;
        let handle = unsafe { ffi::bugle_seq_open(p.as_ptr(), start, end) };
        if handle.is_null() {
            bail!("{}", last_error());
        }
        Ok(Self { handle })
    }

    /// The next frame and its presentation time (seconds), or `None` at the
    /// end of the range.
    pub fn next_frame(&mut self) -> Result<Option<(f64, RgbaImage)>> {
        let mut pts = 0.0;
        let img = unsafe { ffi::bugle_seq_next(self.handle, &mut pts) };
        if img.data.is_null() {
            let e = unsafe { ffi::bugle_last_error() };
            if e.is_null() {
                return Ok(None);
            }
            let msg = unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned();
            unsafe { ffi::bugle_free(e as *mut _) };
            bail!("{msg}");
        }
        Ok(Some((pts, take_image(img)?)))
    }
}

impl Drop for SequentialFrameReader {
    fn drop(&mut self) {
        unsafe { ffi::bugle_seq_close(self.handle) }
    }
}

/// Copy the video track of `video` and the audio tracks of `audio_source`
/// over `start..end` (placed at 0) into `output`, without re-encoding.
/// Returns `false` (nothing written) when the source has no audio there.
pub fn mux_audio(video: &Path, audio_source: &Path, start: f64, end: f64, output: &Path, mp4: bool, timeout: Option<f64>) -> Result<bool> {
    mux_audio_ranges(video, audio_source, &[(start, end)], output, mp4, timeout)
}

/// [`mux_audio`] with the audio of several ranges of `audio_source` (in
/// order, not overlapping), spliced back to back as the video was.
pub fn mux_audio_ranges(video: &Path, audio_source: &Path, ranges: &[(f64, f64)], output: &Path, mp4: bool, timeout: Option<f64>) -> Result<bool> {
    let (v, a, o) = (path_cstr(video)?, path_cstr(audio_source)?, path_cstr(output)?);
    let (r, n) = flat_ranges(ranges)?;
    let timeout = timeout.filter(|t| t.is_finite() && *t > 0.0).unwrap_or(0.0);
    match unsafe { ffi::bugle_mux_audio(v.as_ptr(), a.as_ptr(), r.as_ptr(), n, o.as_ptr(), mp4 as i32, timeout) } {
        0 => Ok(true),
        1 => Ok(false),
        _ => bail!("{}", last_error()),
    }
}

// ---------------------------------------------------------------------------
// ScreenCaptureKit recording
// ---------------------------------------------------------------------------

/// Whether the in-process ScreenCaptureKit recorder (system audio) is
/// available on this macOS (14+).
pub fn sck_record_available() -> bool {
    unsafe { ffi::bugle_sck_record_available() != 0 }
}

#[derive(Debug, Clone)]
pub struct SckRecordOptions {
    /// Area to record in global points; ignored when `window_id` is set.
    pub rect: Rect,
    pub window_id: Option<u32>,
    pub fps: u32,
    /// Keep Retina pixels (otherwise record at 1x points).
    pub retina: bool,
    pub system_audio: bool,
    pub microphone: bool,
    pub show_cursor: bool,
    /// Draw a highlight around mouse clicks (macOS 15+; a warning otherwise).
    pub highlight_clicks: bool,
    /// Stop capturing by itself after this long; `stop` then only finalises.
    pub max_seconds: Option<f64>,
}

/// An in-progress `SCStream` → `AVAssetWriter` recording (H.264 video, AAC
/// system audio, AAC microphone as a second audio track). Dropping an
/// unstopped recorder stops and finalises it.
pub struct SckRecorder {
    handle: *mut libc::c_void,
}

// The handle is an Objective-C object whose state is guarded by its own lock.
unsafe impl Send for SckRecorder {}
unsafe impl Sync for SckRecorder {}

impl SckRecorder {
    pub fn start(opts: &SckRecordOptions, output: &Path) -> Result<Self> {
        if opts.window_id.is_none() && opts.rect.is_empty() {
            return Err(BugleError::Invalid("empty recording rect".into()).into());
        }
        let out = path_cstr(output)?;
        let handle = unsafe {
            ffi::bugle_sck_record_start(
                opts.rect.x,
                opts.rect.y,
                opts.rect.w,
                opts.rect.h,
                opts.window_id.unwrap_or(0),
                opts.fps.clamp(1, 120) as i32,
                opts.retina as i32,
                opts.system_audio as i32,
                opts.microphone as i32,
                opts.show_cursor as i32,
                opts.highlight_clicks as i32,
                opts.max_seconds.unwrap_or(0.0),
                out.as_ptr(),
            )
        };
        if handle.is_null() {
            let e = last_error();
            if !screen_access_preflight() {
                return Err(BugleError::PermissionDenied.into());
            }
            bail!("{e}");
        }
        Ok(Self { handle })
    }

    /// Host-clock time ([`host_time_now`]) of the movie's time 0.
    pub fn session_start(&self) -> Option<f64> {
        Some(unsafe { ffi::bugle_sck_record_session_start(self.handle) }).filter(|s| s.is_finite())
    }

    /// Video frames written so far.
    pub fn frames(&self) -> u64 {
        unsafe { ffi::bugle_sck_record_frames(self.handle) }.max(0) as u64
    }

    /// Current levels of the audio sources this recording captures (a source
    /// is `None` when it was asked for but could not be opened, or delivers a
    /// sample format the meter cannot read).
    pub fn audio_levels(&self) -> AudioLevels {
        let mut v = [0f32; 4];
        let present = unsafe { ffi::bugle_sck_record_levels(self.handle, v.as_mut_ptr()) };
        AudioLevels {
            system_audio: (present & 1 != 0).then(|| TrackLevel::from_linear(v[0], v[1])),
            microphone: (present & 2 != 0).then(|| TrackLevel::from_linear(v[2], v[3])),
        }
    }

    /// Non-fatal problems (microphone unavailable, stream stopped early).
    pub fn warnings(&self) -> Option<String> {
        unsafe {
            let p = ffi::bugle_sck_record_warnings(self.handle);
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            ffi::bugle_free(p as *mut _);
            Some(s)
        }
    }

    /// Stop capturing and finalise the file.
    pub fn stop(mut self) -> Result<()> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> Result<()> {
        if self.handle.is_null() {
            return Ok(());
        }
        let h = std::mem::replace(&mut self.handle, std::ptr::null_mut());
        if unsafe { ffi::bugle_sck_record_stop(h) } != 0 {
            bail!("{}", last_error());
        }
        Ok(())
    }
}

impl Drop for SckRecorder {
    fn drop(&mut self) {
        let _ = self.stop_inner();
    }
}

pub fn play_sound(name: &str) {
    if let Ok(c) = cstr(name) {
        unsafe { ffi::bugle_play_sound(c.as_ptr()) }
    }
}

pub fn open_path(path: &Path) {
    if let Ok(c) = path_cstr(path) {
        unsafe { ffi::bugle_open_path(c.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_frames_keep_their_colour_through_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let c = [190u8, 60, 35];
        let close = |p: &image::Rgba<u8>, what: &str| {
            for i in 0..3 {
                assert!((p[i] as i32 - c[i] as i32).abs() <= 4, "{what}: {:?} vs {c:?}", p);
            }
        };
        let mut src = dir.path().join("0.mp4");
        let mut frame = RgbaImage::from_pixel(160, 120, image::Rgba([c[0], c[1], c[2], 255]));
        // Four generations, as repeated effects edits would make.
        for gen in 1..=4 {
            let mut w = VideoFrameWriter::create(&src, 160, 120, 10, true).unwrap();
            for _ in 0..5 {
                w.append(&frame).unwrap();
            }
            w.finish().unwrap();
            let mut r = SequentialFrameReader::open(&src, 0.0, 0.0).unwrap();
            let (pts, f) = r.next_frame().unwrap().unwrap();
            assert!(pts.abs() < 1e-6);
            close(f.get_pixel(80, 60), &format!("generation {gen}, sequential reader"));
            // The generator colour-matches into the bitmap's colour space: a
            // small fixed offset that does not grow across generations.
            let g = VideoFrameReader::open(&src, 0.0).unwrap().frame_at(0.25).unwrap();
            let g = g.get_pixel(80, 60);
            assert!((0..3).all(|i| (g[i] as i32 - c[i] as i32).abs() <= 12), "generator: {g:?}");
            let n = 1 + std::iter::from_fn(|| r.next_frame().unwrap()).count();
            assert_eq!(n, 5);
            frame = f;
            src = dir.path().join(format!("{gen}.mp4"));
        }
    }

    #[test]
    fn host_clock_and_child_watch() {
        let a = host_time_now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let d = host_time_now() - a;
        assert!(d > 0.015 && d < 0.5, "{d}");
        let mut child = std::process::Command::new("/bin/sleep").arg("0.1").spawn().unwrap();
        assert!(!child_exited(child.id()));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(child_exited(child.id()));
        // Not reaped: the owner still gets the status.
        assert!(child.try_wait().unwrap().unwrap().success());
        // Once the owner has reaped it, it still counts as exited.
        assert!(child_exited(child.id()));
        let mut quick = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        quick.wait().unwrap();
        assert!(child_exited(quick.id()), "reaped before the first check");
    }

    #[test]
    fn registration_recovers_vertical_shift() {
        // Build a textured image and a copy shifted down by 37px; Vision
        // should report the translation that maps the shifted copy back.
        let w = 320;
        let h = 240;
        let mut base = RgbaImage::new(w, h);
        for (x, y, p) in base.enumerate_pixels_mut() {
            let v = ((x * 7 + y * 13) % 251) as u8;
            let u = ((x * 3) ^ (y * 5)) as u8;
            *p = image::Rgba([v, u, v ^ u, 255]);
        }
        let shift = 37;
        let mut shifted = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let sy = (y as i64 - shift).rem_euclid(h as i64) as u32;
                shifted.put_pixel(x, y, *base.get_pixel(x, sy));
            }
        }
        let (dx, dy) = register_translation(&base, &shifted).expect("registration");
        assert!(dx.abs() < 1.5, "dx={dx}");
        assert!((dy.abs() - shift as f64).abs() < 1.5, "dy={dy}");
    }

    fn invalid_msg(e: &anyhow::Error) -> String {
        match e.downcast_ref::<BugleError>() {
            Some(BugleError::Invalid(m)) => m.clone(),
            other => panic!("expected BugleError::Invalid, got {other:?} / {e}"),
        }
    }

    #[test]
    fn ffi_dims_validation() {
        assert_eq!(ffi_dims(4, 3, 48).unwrap(), (4, 3));
        assert_eq!(ffi_dims(4, 3, 64).unwrap(), (4, 3));
        assert!(invalid_msg(&ffi_dims(0, 3, 0).unwrap_err()).contains("empty"));
        assert!(invalid_msg(&ffi_dims(4, 3, 47).unwrap_err()).contains("needs 48"));
        let big = i32::MAX as u32 + 1;
        assert!(invalid_msg(&ffi_dims(big, 1, usize::MAX).unwrap_err()).contains("too large"));
        assert!(invalid_msg(&ffi_dims(1, big, usize::MAX).unwrap_err()).contains("too large"));
        assert_eq!(rgba_len(u32::MAX, u32::MAX), None);
        assert_eq!(rgba_len(70_000, 70_000), Some(70_000 * 70_000 * 4));
    }

    #[test]
    fn ocr_and_registration_reject_empty_images() {
        let empty = RgbaImage::new(0, 0);
        let ok = RgbaImage::new(8, 8);
        let p = OcrParams { languages: &[], accurate: false, language_correction: false };
        invalid_msg(&ocr(&empty, &p).unwrap_err());
        invalid_msg(&register_translation(&empty, &ok).unwrap_err());
        invalid_msg(&register_translation(&ok, &empty).unwrap_err());
    }

    #[test]
    fn capture_area_rejects_bad_rects() {
        for r in [
            Rect::new(f64::NAN, 0.0, 10.0, 10.0),
            Rect::new(0.0, f64::INFINITY, 10.0, 10.0),
            Rect::new(0.0, 0.0, f64::NAN, 10.0),
            Rect::new(0.0, 0.0, 10.0, f64::NEG_INFINITY),
            Rect::new(0.0, 0.0, 0.0, 10.0),
            Rect::new(0.0, 0.0, 20000.0, 10.0),
            Rect::new(0.0, 0.0, 10.0, 1e300),
        ] {
            invalid_msg(&capture_area(r, false).unwrap_err());
        }
    }

    #[test]
    fn shim_rejects_bad_inputs() {
        // Bypass the Rust-side checks to exercise the shim's own validation.
        unsafe {
            for (x, y, w, h) in [(f64::NAN, 0.0, 10.0, 10.0), (0.0, 0.0, f64::INFINITY, 10.0), (0.0, 0.0, 1e9, 10.0)] {
                let img = ffi::bugle_capture_area(x, y, w, h, 0);
                assert!(img.data.is_null());
                assert!(last_error().contains("invalid capture area"));
            }
            let buf = [0u8; 16];
            let (mut lines, mut count) = (std::ptr::null_mut(), 0);
            let langs = CString::new("").unwrap();
            // 4x4 needs 64 bytes; 16 are given.
            let rc = ffi::bugle_ocr(buf.as_ptr(), buf.len(), 4, 4, langs.as_ptr(), 0, 0, &mut lines, &mut count);
            assert_eq!(rc, -1);
            assert!(lines.is_null());
            assert!(last_error().contains("OCR: invalid image"));
            let rc = ffi::bugle_ocr(buf.as_ptr(), buf.len(), i32::MAX, i32::MAX, langs.as_ptr(), 0, 0, &mut lines, &mut count);
            assert_eq!(rc, -1);
            assert!(last_error().contains("OCR: invalid image"));
            let (mut dx, mut dy) = (0.0, 0.0);
            let rc = ffi::bugle_register_translation(buf.as_ptr(), 16, 2, 2, buf.as_ptr(), 15, 2, 2, &mut dx, &mut dy);
            assert_eq!(rc, -1);
            assert!(last_error().contains("registration: invalid image"));
            // The error was handed over: nothing left for this thread.
            assert_eq!(last_error(), "unknown platform error");
        }
    }

    /// Hammers capture/OCR from many threads with valid and invalid input
    /// and checks every error is the one its own call produced.
    #[test]
    fn concurrent_calls_keep_errors_per_thread() {
        let screen = screen_access_preflight();
        if !screen {
            eprintln!("Screen Recording not granted: skipping the capture part of this test");
        }
        let threads: Vec<_> = (1..=8u32)
            .map(|t| {
                std::thread::spawn(move || {
                    let mut text = RgbaImage::from_pixel(96, 48, image::Rgba([255, 255, 255, 255]));
                    for (x, y, p) in text.enumerate_pixels_mut() {
                        if (x / 6 + y / 6 + t) % 3 == 0 {
                            *p = image::Rgba([0, 0, 0, 255]);
                        }
                    }
                    let langs = CString::new("").unwrap();
                    for i in 0..6u32 {
                        // Invalid OCR whose message names this thread's size.
                        let w = (1000 * t + i) as i32;
                        let buf = [0u8; 4];
                        let (mut lines, mut count) = (std::ptr::null_mut(), 0);
                        let rc = unsafe {
                            ffi::bugle_ocr(buf.as_ptr(), buf.len(), w, 7, langs.as_ptr(), 0, 0, &mut lines, &mut count)
                        };
                        assert_eq!(rc, -1);
                        let e = last_error();
                        assert!(e.contains(&format!("({w}x7, 4 bytes)")), "thread {t} got foreign error {e:?}");

                        // Valid OCR must succeed regardless of other threads' errors.
                        let p = OcrParams { languages: &[], accurate: false, language_correction: false };
                        ocr(&text, &p).expect("valid OCR");

                        // Invalid capture whose message names this thread's x.
                        let x = (t * 100 + i) as f64;
                        let img = unsafe { ffi::bugle_capture_area(x, 0.0, f64::NAN, 10.0, 0) };
                        assert!(img.data.is_null());
                        let e = last_error();
                        assert!(e.contains(&format!("invalid capture area {x},0 ")), "thread {t} got foreign error {e:?}");

                        if screen {
                            let img = capture_area(Rect::new(0.0, 0.0, 40.0 + t as f64, 30.0), false).expect("capture");
                            assert!(img.width() > 0 && img.height() > 0);
                            assert!(capture_window(u32::MAX - t, false, false).is_err());
                            windows(true).expect("windows");
                        }
                    }
                })
            })
            .collect();
        for th in threads {
            th.join().expect("worker thread panicked");
        }
    }

    /// The first audio track of `path` decoded by the system `afconvert` to
    /// 48 kHz stereo float samples (interleaved). Float output keeps values
    /// beyond full scale, so clipping shows up as |sample| > 1.
    fn decode(path: &Path) -> Vec<f32> {
        let wav = path.with_extension("wav");
        let st = std::process::Command::new("/usr/bin/afconvert")
            .args(["-f", "WAVE", "-d", "LEF32@48000"])
            .arg(path)
            .arg(&wav)
            .status()
            .expect("afconvert");
        assert!(st.success(), "afconvert failed on {}", path.display());
        let bytes = std::fs::read(&wav).unwrap();
        // Walk the RIFF chunks to "data".
        let mut i = 12;
        let data = loop {
            let id = &bytes[i..i + 4];
            let len = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
            if id == b"data" {
                break &bytes[i + 8..(i + 8 + len).min(bytes.len())];
            }
            i += 8 + len + (len & 1);
        };
        data.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
    }

    /// RMS of each of `parts` equal slices of `samples`.
    fn slice_rms(samples: &[f32], parts: usize) -> Vec<f64> {
        let n = samples.len() / parts;
        (0..parts)
            .map(|p| {
                // Skip the edges of each slice: AAC priming and the tone's
                // on/off transitions smear across them.
                let s = &samples[p * n + n / 5..(p + 1) * n - n / 5];
                (s.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
            })
            .collect()
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |m, v| m.max(v.abs()))
    }

    #[test]
    fn audio_peaks_cover_every_track_and_stop_at_the_duration() {
        let tmp = tempfile::tempdir().unwrap();
        // Track 0 sounds in the first second, track 1 in the second.
        let two = tmp.path().join("two.mp4");
        write_tone_fixture(&two, &ToneFixture { tracks: 2, seconds: 2.0, ..Default::default() }).unwrap();
        let p = audio_peaks(&two, 4.0, 8, None).unwrap().unwrap();
        assert_eq!(p.len(), 8);
        for (i, v) in p.iter().enumerate().take(4) {
            assert!((0.35..=0.8).contains(v), "bucket {i}: {p:?}");
        }
        assert!(p[5..].iter().all(|v| *v < 0.01), "nothing after the audio ends: {p:?}");
        // Only the first second asked for: all of it in the buckets.
        let p = audio_peaks(&two, 1.0, 4, None).unwrap().unwrap();
        assert!(p.iter().all(|v| *v > 0.35), "{p:?}");
        // A cancelled read stops with nothing.
        assert_eq!(audio_peaks(&two, 4.0, 8, Some(&AtomicBool::new(true))).unwrap(), None);
        assert!(audio_peaks(&two, 4.0, 8, Some(&AtomicBool::new(false))).unwrap().is_some());
        // Silence is flat.
        let quiet = tmp.path().join("quiet.mp4");
        write_tone_fixture(&quiet, &ToneFixture { tracks: 1, amplitude: 0.0, ..Default::default() }).unwrap();
        assert!(audio_peaks(&quiet, 1.0, 10, None).unwrap().unwrap().iter().all(|v| *v < 0.01));
        // A movie without audio has no waveform.
        let silent = tmp.path().join("silent.mp4");
        let mut w = VideoFrameWriter::create(&silent, 64, 64, 10, true).unwrap();
        for _ in 0..5 {
            w.append(&RgbaImage::new(64, 64)).unwrap();
        }
        w.finish().unwrap();
        assert_eq!(audio_peaks(&silent, 0.5, 10, None).unwrap(), None);
        assert!(audio_peaks(&silent, 0.0, 10, None).is_err() && audio_peaks(&silent, 1.0, 0, None).is_err());
    }

    #[test]
    fn level_meter_reads_every_sample_layout() {
        // Two channels of 480 frames: a 0.5 sine on the left, silence but
        // for one -0.8 sample on the right.
        let n = 480;
        let mut s: Vec<f32> = (0..n).map(|i| 0.5 * (i as f32 * std::f32::consts::TAU * 1000.0 / 48000.0).sin()).collect();
        s.extend(std::iter::repeat_n(0.0, n));
        s[n + 100] = -0.8;
        let want_rms = ((0.25 * 0.5 * n as f64 + 0.64) / (2 * n) as f64).sqrt();
        use LevelFixtureFormat::*;
        for format in [F32, I16, F64, I32] {
            for interleaved in [false, true] {
                let (peak, rms, secs) = audio_level_fixture(&s, 2, format, interleaved, 48000.0).expect("measured");
                let tol = if format == I16 { 1e-3 } else { 1e-5 };
                assert!((peak - 0.8).abs() < tol, "peak {peak} ({format:?}, interleaved {interleaved})");
                assert!((rms - want_rms).abs() < tol, "rms {rms} vs {want_rms} ({format:?}, interleaved {interleaved})");
                assert!((secs - 0.01).abs() < 1e-9, "seconds {secs}");
            }
        }
        assert_eq!(audio_level_fixture(&s, 2, I16BigEndian, true, 48000.0), None, "big-endian is not read");
        let silent = vec![0f32; 256];
        assert_eq!(audio_level_fixture(&silent, 1, F32, true, 48000.0).map(|r| (r.0, r.1)), Some((0.0, 0.0)));
    }

    /// The recorder's meter state after `buffers` of `seconds` each, arriving
    /// back to back from t = 0, each with `peak` and `mean_square`.
    fn steady_level(buffers: usize, seconds: f64, peak: f64, mean_square: f64) -> ffi::bugle_level_t {
        let mut l = ffi::bugle_level_t::default();
        for i in 0..buffers {
            unsafe { ffi::bugle_level_add(&mut l, (i + 1) as f64 * seconds, peak, mean_square, seconds) };
        }
        l
    }

    fn level_at(l: &ffi::bugle_level_t, now: f64) -> (f64, f64) {
        let r = unsafe { ffi::bugle_level_at(l, now) };
        (bugle_core::audio_level::db_from_linear(r.peak as f32), bugle_core::audio_level::db_from_linear(r.mean_square.sqrt() as f32))
    }

    #[test]
    fn meter_constants_match_core() {
        let mut c = [0f64; 2];
        unsafe { ffi::bugle_level_constants(c.as_mut_ptr()) };
        assert_eq!(c, [bugle_core::audio_level::PEAK_FALL_DB_PER_S, bugle_core::audio_level::RMS_TIME_CONSTANT_S]);
    }

    #[test]
    fn meter_rms_settles_on_a_steady_signal() {
        // A 0.5 sine: mean square 0.125, RMS -9.03 dBFS.
        let want = 10.0 * 0.125f64.log10();
        for seconds in [0.01, 0.02, 0.1] {
            let buffers = (3.0 / seconds) as usize;
            let l = steady_level(buffers, seconds, 0.5, 0.125);
            let end = buffers as f64 * seconds;
            // Read just after a buffer and just before the next one is due.
            for at in [end, end + seconds * 0.99] {
                let (peak, rms) = level_at(&l, at);
                assert!((rms - want).abs() < 0.2, "rms {rms} vs {want} at {seconds} s buffers, read at +{}", at - end);
                assert!(peak <= -6.0 && peak > -6.0 - 20.0 * seconds - 0.1, "peak {peak}");
            }
        }
    }

    #[test]
    fn meter_follows_level_changes_with_its_time_constant() {
        // From silence to a steady signal: after one time constant the mean
        // square is 1 - 1/e of the way there.
        let mut l = steady_level(100, 0.01, 0.0, 0.0);
        for i in 0..30 {
            unsafe { ffi::bugle_level_add(&mut l, 1.0 + (i + 1) as f64 * 0.01, 1.0, 1.0, 0.01) };
        }
        let ms = unsafe { ffi::bugle_level_at(&l, 1.3) }.mean_square;
        assert!((ms - (1.0 - (-1.0f64).exp())).abs() < 0.01, "mean square {ms}");
    }

    #[test]
    fn meter_peak_falls_and_reads_decay_without_changing_state() {
        let l = steady_level(10, 0.01, 1.0, 0.5);
        let before = l;
        // The peak falls 20 dB/s from the last buffer.
        assert_eq!(level_at(&l, 0.1).0, 0.0);
        assert_eq!(level_at(&l, 0.6).0, -10.0);
        assert_eq!(level_at(&l, 1.1).0, -20.0);
        // The average holds for the last buffer's duration, then decays with
        // the time constant once no buffers come.
        let ms = |t| unsafe { ffi::bugle_level_at(&l, t) }.mean_square;
        assert!((ms(0.11) - 0.5).abs() < 1e-12);
        assert!((ms(0.11 + 0.3) - 0.5 / std::f64::consts::E).abs() < 1e-9);
        // Reading composes and leaves the state alone.
        let mid = unsafe { ffi::bugle_level_at(&l, 0.3) };
        let a = unsafe { ffi::bugle_level_at(&mid, 0.7) };
        let b = unsafe { ffi::bugle_level_at(&l, 0.7) };
        assert!((a.peak - b.peak).abs() < 1e-12 && (a.mean_square - b.mean_square).abs() < 1e-12);
        assert_eq!((l.peak, l.mean_square, l.updated), (before.peak, before.mean_square, before.updated));
        // A louder buffer raises the held peak at once.
        let mut l = l;
        unsafe { ffi::bugle_level_add(&mut l, 0.5, 0.9, 0.5, 0.01) };
        assert!((l.peak - 0.9).abs() < 1e-12, "peak {}", l.peak);
    }

    #[test]
    fn unread_meter_reads_as_unset() {
        let l = ffi::bugle_level_t::default();
        let r = unsafe { ffi::bugle_level_at(&l, 5.0) };
        assert_eq!((r.seen, r.peak, r.mean_square), (0, 0.0, 0.0));
    }

    #[test]
    fn mixes_audio_tracks_into_one() {
        let tmp = tempfile::tempdir().unwrap();
        let two = tmp.path().join("two.mp4");
        write_tone_fixture(&two, &ToneFixture { tracks: 2, seconds: 2.0, ..Default::default() }).unwrap();
        assert_eq!(audio_track_count(&two).unwrap(), 2);
        // Decoding picks the first track only: tone in the first half.
        let rms = slice_rms(&decode(&two), 2);
        assert!(rms[0] > 0.1 && rms[1] < 0.01, "fixture track 0 levels {rms:?}");

        let mixed = tmp.path().join("mixed.mp4");
        assert!(mix_audio_tracks(&two, &mixed).unwrap());
        assert_eq!(audio_track_count(&mixed).unwrap(), 1);
        let d = media_duration(&mixed).unwrap();
        assert!((d - 2.0).abs() < 0.15, "mixed duration {d}");
        // Both tracks are in the one track (each at half level): tone in
        // both halves.
        let rms = slice_rms(&decode(&mixed), 2);
        assert!(rms[0] > 0.1 && rms[1] > 0.1, "mixed levels {rms:?}");

        // A .mov output is QuickTime.
        let mov = tmp.path().join("mixed.mov");
        assert!(mix_audio_tracks(&two, &mov).unwrap());
        let head = std::fs::read(&mov).unwrap();
        assert_eq!(&head[4..12], b"ftypqt  ");
        assert_eq!(audio_track_count(&mov).unwrap(), 1);
    }

    #[test]
    fn loud_sources_do_not_clip() {
        let tmp = tempfile::tempdir().unwrap();
        // Two in-phase tones at 0.9: an unscaled sum would peak at 1.8.
        let loud = tmp.path().join("loud.mp4");
        let f = ToneFixture { tracks: 2, seconds: 1.0, amplitude: 0.9, overlap: true, ..Default::default() };
        write_tone_fixture(&loud, &f).unwrap();
        let p = peak(&decode(&loud));
        assert!(p > 0.8, "fixture track peak {p}");
        let mixed = tmp.path().join("mixed.mp4");
        assert!(mix_audio_tracks(&loud, &mixed).unwrap());
        let p = peak(&decode(&mixed));
        assert!(p > 0.5 && p < 1.0, "mixed peak {p}");
    }

    #[test]
    fn mix_levels_scale_each_source() {
        let tmp = tempfile::tempdir().unwrap();
        // Track 0 (system audio) sounds in the first half, track 1 (mic) in
        // the second.
        let two = tmp.path().join("two.mp4");
        write_tone_fixture(&two, &ToneFixture { tracks: 2, seconds: 2.0, ..Default::default() }).unwrap();
        let plain = tmp.path().join("plain.mp4");
        assert!(mix_audio_tracks(&two, &plain).unwrap());
        let base = slice_rms(&decode(&plain), 2);
        let leveled = tmp.path().join("leveled.mp4");
        let levels = AudioMixConfig { system_audio_gain: 0.25, microphone_gain: 1.8, ..Default::default() };
        assert!(mix_audio_tracks_with(&two, &leveled, &levels).unwrap());
        let rms = slice_rms(&decode(&leveled), 2);
        let (sys, mic) = (rms[0] / base[0], rms[1] / base[1]);
        assert!((sys - 0.25).abs() < 0.03, "system ratio {sys} ({rms:?} vs {base:?})");
        assert!((mic - 1.8).abs() < 0.1, "mic ratio {mic} ({rms:?} vs {base:?})");
        // Out-of-range levels are refused before anything is written.
        let bad = tmp.path().join("bad.mp4");
        let e = mix_audio_tracks_with(&two, &bad, &AudioMixConfig { microphone_gain: -1.0, ..Default::default() }).unwrap_err();
        assert!(e.to_string().contains("microphone_gain"), "{e}");
        assert!(!bad.exists());
    }

    #[test]
    fn leveling_keeps_the_tracks_separate_and_scales_each() {
        let tmp = tempfile::tempdir().unwrap();
        // Track 0 (system audio) sounds in the first half, track 1 (mic) in
        // the second.
        let two = tmp.path().join("two.mp4");
        write_tone_fixture(&two, &ToneFixture { tracks: 2, seconds: 2.0, video: true, ..Default::default() }).unwrap();
        let levels = AudioMixConfig { system_audio_gain: 0.25, microphone_gain: 1.8, ..Default::default() };
        let leveled = tmp.path().join("leveled.mp4");
        assert!(level_audio_tracks(&two, &leveled, &levels).unwrap());
        assert_eq!(audio_track_count(&leveled).unwrap(), 2);
        assert_eq!(video_track_count(&leveled).unwrap(), 1);
        let (d0, d1) = (media_duration(&two).unwrap(), media_duration(&leveled).unwrap());
        assert!((d0 - d1).abs() < 0.05, "duration {d0} -> {d1}");
        // The first track decodes on its own: system audio, scaled, still
        // silent in the mic's half.
        let (src0, out0) = (slice_rms(&decode(&two), 2), slice_rms(&decode(&leveled), 2));
        assert!((out0[0] / src0[0] - 0.25).abs() < 0.03, "system ratio ({out0:?} vs {src0:?})");
        assert!(out0[1] < 0.01, "track 0 picked up the mic: {out0:?}");
        // Mixing both files at default levels shows the second track too.
        let (m_src, m_out) = (tmp.path().join("m_src.mp4"), tmp.path().join("m_out.mp4"));
        assert!(mix_audio_tracks(&two, &m_src).unwrap());
        assert!(mix_audio_tracks(&leveled, &m_out).unwrap());
        let (a, b) = (slice_rms(&decode(&m_src), 2), slice_rms(&decode(&m_out), 2));
        assert!((b[1] / a[1] - 1.8).abs() < 0.1, "mic ratio ({b:?} vs {a:?})");
        // The video samples are copied.
        assert_eq!(video_frame(&two, 0.5).unwrap(), video_frame(&leveled, 0.5).unwrap());
        // A .mov output is QuickTime.
        let mov = tmp.path().join("leveled.mov");
        assert!(level_audio_tracks(&two, &mov, &levels).unwrap());
        assert_eq!(&std::fs::read(&mov).unwrap()[4..12], b"ftypqt  ");
        assert_eq!(audio_track_count(&mov).unwrap(), 2);
        // One track: nothing to level, nothing written.
        let one = tmp.path().join("one.mp4");
        write_tone_fixture(&one, &ToneFixture { tracks: 1, ..Default::default() }).unwrap();
        let none = tmp.path().join("none.mp4");
        assert!(!level_audio_tracks(&one, &none, &levels).unwrap());
        assert!(!none.exists());
        // Out-of-range levels are refused.
        let bad = AudioMixConfig { system_audio_gain: 9.0, ..Default::default() };
        assert!(level_audio_tracks(&two, &tmp.path().join("bad.mp4"), &bad).is_err());
    }

    #[test]
    fn mix_ducks_system_audio_and_limits_loud_gains() {
        let tmp = tempfile::tempdir().unwrap();
        // Both tracks play the same 440 Hz tone at 0.5 throughout, so the
        // mic is always active.
        let src = tmp.path().join("src.mp4");
        let f = ToneFixture { tracks: 2, seconds: 1.5, overlap: true, ..Default::default() };
        write_tone_fixture(&src, &f).unwrap();
        let plain = tmp.path().join("plain.mp4");
        assert!(mix_audio_tracks(&src, &plain).unwrap());
        let ducked = tmp.path().join("ducked.mp4");
        let levels = AudioMixConfig { ducking: true, ducking_amount_db: 12.0, ..Default::default() };
        assert!(mix_audio_tracks_with(&src, &ducked, &levels).unwrap());
        // In phase: 0.5*(a + a) without ducking, 0.5*(a/4 + a) with 12 dB.
        let ratio = slice_rms(&decode(&ducked), 1)[0] / slice_rms(&decode(&plain), 1)[0];
        assert!((ratio - 0.625).abs() < 0.04, "ducked/plain {ratio}");

        // Two in-phase 0.9 tones at 4x: the unlimited sum would peak at 3.6.
        let loud = tmp.path().join("loud.mp4");
        write_tone_fixture(&loud, &ToneFixture { amplitude: 0.9, ..f }).unwrap();
        let limited = tmp.path().join("limited.mp4");
        let levels = AudioMixConfig { microphone_gain: 4.0, system_audio_gain: 4.0, ..Default::default() };
        assert!(mix_audio_tracks_with(&loud, &limited, &levels).unwrap());
        let p = peak(&decode(&limited));
        assert!(p > 0.85 && p < 1.0, "limited peak {p}");
    }

    #[test]
    fn mixing_keeps_video_and_track_offsets() {
        let tmp = tempfile::tempdir().unwrap();
        // Track 0 sounds in [0, 1.5); track 1 only starts at 1.5 s and
        // sounds in [1.5, 3). Losing the offset would move its tone to the
        // start and leave the second half silent.
        let src = tmp.path().join("src.mp4");
        let f = ToneFixture { tracks: 2, seconds: 3.0, delay: 1.5, video: true, ..Default::default() };
        write_tone_fixture(&src, &f).unwrap();
        assert_eq!(video_track_count(&src).unwrap(), 1);
        let mixed = tmp.path().join("mixed.mp4");
        assert!(mix_audio_tracks(&src, &mixed).unwrap());
        assert_eq!(audio_track_count(&mixed).unwrap(), 1);
        assert_eq!(video_track_count(&mixed).unwrap(), 1);
        let (d0, d1) = (media_duration(&src).unwrap(), media_duration(&mixed).unwrap());
        assert!((d0 - d1).abs() < 0.05, "duration {d0} -> {d1}");
        let rms = slice_rms(&decode(&mixed), 2);
        assert!(rms[0] > 0.1 && rms[1] > 0.1, "mixed levels {rms:?}");
        // The video samples are copied, so decoded frames are identical.
        for t in [0.25, 1.55, 2.75] {
            assert_eq!(video_frame(&src, t).unwrap(), video_frame(&mixed, t).unwrap(), "frame at {t}s");
        }
    }

    #[test]
    fn mixing_one_track_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let one = tmp.path().join("one.mp4");
        write_tone_fixture(&one, &ToneFixture { tracks: 1, seconds: 0.5, ..Default::default() }).unwrap();
        let out = tmp.path().join("out.mp4");
        assert!(!mix_audio_tracks(&one, &out).unwrap());
        assert!(!out.exists());
        // Not a movie: no audio tracks, nothing to do.
        let junk = tmp.path().join("junk.mp4");
        std::fs::write(&junk, b"not a movie").unwrap();
        assert_eq!(audio_track_count(&junk).unwrap(), 0);
        assert!(!mix_audio_tracks(&junk, &out).unwrap());
        assert!(!out.exists());
    }

    #[test]
    fn mixing_refuses_to_overwrite_its_input() {
        let tmp = tempfile::tempdir().unwrap();
        let two = tmp.path().join("two.mp4");
        write_tone_fixture(&two, &ToneFixture::default()).unwrap();
        let before = std::fs::read(&two).unwrap();
        let link = tmp.path().join("link.mp4");
        std::fs::hard_link(&two, &link).unwrap();
        let dotted = tmp.path().join(".").join("two.mp4");
        for out in [&two, &link, &dotted] {
            let e = mix_audio_tracks(&two, out).unwrap_err();
            assert!(e.to_string().contains("same file"), "{e}");
        }
        assert_eq!(std::fs::read(&two).unwrap(), before);
    }

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba([rgb[0], rgb[1], rgb[2], 255]))
    }

    fn close(a: &image::Rgba<u8>, rgb: [u8; 3]) -> bool {
        (0..3).all(|i| (a[i] as i32 - rgb[i] as i32).abs() < 40)
    }

    #[test]
    fn frame_writer_reader_and_export_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let movie = dir.path().join("fixture.mp4");
        // 2 s at 10 fps: red for the first second, blue for the second.
        let mut w = VideoFrameWriter::create(&movie, 64, 48, 10, true).unwrap();
        for i in 0..20 {
            w.append(&solid(64, 48, if i < 10 { [255, 0, 0] } else { [0, 0, 255] })).unwrap();
        }
        assert!(w.append(&solid(62, 48, [0, 0, 0])).is_err(), "size mismatch is refused");
        w.finish().unwrap();
        assert_eq!(video_size(&movie).unwrap(), (64, 48));
        assert!((media_duration(&movie).unwrap() - 2.0).abs() < 0.05);

        let mut r = VideoFrameReader::open(&movie, 0.0).unwrap();
        assert!(close(r.frame_at(0.45).unwrap().get_pixel(32, 24), [255, 0, 0]));
        assert!(close(r.frame_at(1.55).unwrap().get_pixel(32, 24), [0, 0, 255]));

        let cut = dir.path().join("cut.mov");
        export_video(&movie, &cut, 1.0, 2.0, VideoExportPreset::Highest, false, Some(5), Some(60.0)).unwrap();
        assert!((media_duration(&cut).unwrap() - 1.0).abs() < 0.1);
        let mut r = VideoFrameReader::open(&cut, 0.0).unwrap();
        assert!(close(r.frame_at(0.1).unwrap().get_pixel(32, 24), [0, 0, 255]));
        // The frame rate really changed: 1 s at 5 fps is 5 stored frames.
        let info = video_info(&cut).unwrap();
        assert_eq!(info.stored_samples, Some(5), "{info:?}");
        assert!((info.nominal_fps - 5.0).abs() < 0.5, "{info:?}");
        let src = video_info(&movie).unwrap();
        assert_eq!(src.stored_samples, Some(20));
        assert!((src.end() - 2.0).abs() < 0.01, "{src:?}");

        // Keyframes every second: a cut at 1.0 is clean, one at 0.5 is not.
        assert!(video_sync_at(&movie, 0.0).unwrap());
        assert!(video_sync_at(&movie, 1.0).unwrap());
        assert!(!video_sync_at(&movie, 0.5).unwrap());

        // Passthrough (no re-encode) trims, but a cut between keyframes keeps
        // the frames back to the previous keyframe in the file (hidden by an
        // edit list) ...
        let pass = dir.path().join("pass.mp4");
        export_video(&movie, &pass, 0.5, 1.5, VideoExportPreset::Passthrough, true, None, None).unwrap();
        assert!((media_duration(&pass).unwrap() - 1.0).abs() < 0.1);
        assert_eq!(video_size(&pass).unwrap(), (64, 48));
        assert!(video_info(&pass).unwrap().stored_samples.unwrap() > 10);
        // ... while a cut on a keyframe, or a re-encode, stores only the range.
        let clean = dir.path().join("clean.mp4");
        export_video(&movie, &clean, 1.0, 2.0, VideoExportPreset::Passthrough, true, None, None).unwrap();
        assert_eq!(video_info(&clean).unwrap().stored_samples, Some(10));
        export_video(&movie, &clean, 0.5, 1.5, VideoExportPreset::Highest, true, None, None).unwrap();
        assert_eq!(video_info(&clean).unwrap().stored_samples, Some(10));

        // Several ranges are spliced back to back, re-encoded or copied.
        let spliced = dir.path().join("spliced.mp4");
        let ranges = [(0.0, 0.5), (1.5, 2.0)];
        export_video_ranges(&movie, &spliced, &ranges, VideoExportPreset::Highest, true, None, None).unwrap();
        assert!((media_duration(&spliced).unwrap() - 1.0).abs() < 0.1);
        let mut r = VideoFrameReader::open(&spliced, 0.0).unwrap();
        assert!(close(r.frame_at(0.25).unwrap().get_pixel(32, 24), [255, 0, 0]));
        assert!(close(r.frame_at(0.75).unwrap().get_pixel(32, 24), [0, 0, 255]));
        assert_eq!(video_info(&spliced).unwrap().stored_samples, Some(10), "only the kept frames");
        export_video_ranges(&movie, &spliced, &[(0.0, 0.5), (1.0, 1.5)], VideoExportPreset::Passthrough, true, None, None).unwrap();
        assert!((media_duration(&spliced).unwrap() - 1.0).abs() < 0.1);
        let mut r = VideoFrameReader::open(&spliced, 0.0).unwrap();
        assert!(close(r.frame_at(0.75).unwrap().get_pixel(32, 24), [0, 0, 255]));
        let bad = dir.path().join("bad.mp4");
        assert!(export_video_ranges(&movie, &bad, &[(1.0, 1.5), (0.0, 0.5)], VideoExportPreset::Highest, true, None, None).is_err());
        assert!(export_video_ranges(&movie, &bad, &[], VideoExportPreset::Highest, true, None, None).is_err());
        assert!(!bad.exists());

        // A frame rate with passthrough is refused; a timeout cancels the export.
        let bad = dir.path().join("bad.mp4");
        assert!(export_video(&movie, &bad, 0.0, 1.0, VideoExportPreset::Passthrough, true, Some(5), None).is_err());
        let e = export_video(&movie, &bad, 0.0, 2.0, VideoExportPreset::Highest, true, None, Some(1e-6)).unwrap_err();
        assert!(e.to_string().contains("timed out"), "{e}");
        assert!(!bad.exists());

        // Bad ranges and inputs fail without leaving a file behind.
        let bad = dir.path().join("bad.mp4");
        assert!(export_video(&movie, &bad, 1.5, 1.0, VideoExportPreset::Passthrough, true, None, None).is_err());
        assert!(export_video(&dir.path().join("missing.mp4"), &bad, 0.0, 1.0, VideoExportPreset::Passthrough, true, None, None).is_err());
        assert!(!bad.exists());
        assert!(VideoFrameReader::open(&dir.path().join("missing.mp4"), 0.0).is_err());
    }

    #[test]
    fn frame_writer_rejects_bad_sizes_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.mp4");
        assert!(VideoFrameWriter::create(&p, 63, 48, 10, true).is_err(), "odd width");
        assert!(VideoFrameWriter::create(&p, 64, 48, 0, true).is_err(), "zero fps");
        // No frames: finishing fails and removes the file.
        let w = VideoFrameWriter::create(&p, 64, 48, 10, true).unwrap();
        assert!(w.finish().is_err());
        assert!(!p.exists());
        // Dropped unfinished: discarded.
        let mut w = VideoFrameWriter::create(&p, 64, 48, 10, true).unwrap();
        w.append(&solid(64, 48, [0, 255, 0])).unwrap();
        drop(w);
        assert!(!p.exists());
    }

    #[test]
    fn lists_displays() {
        let d = displays().unwrap();
        assert!(!d.is_empty());
        assert!(d.iter().any(|d| d.is_main));
    }
}
