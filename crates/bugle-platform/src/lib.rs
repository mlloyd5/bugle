//! Safe Rust API over the macOS shim.
//!
//! Every function here is synchronous and may block for a few hundred
//! milliseconds (ScreenCaptureKit round-trips). Call from a blocking thread
//! when inside an async runtime. The two `*_blocking` UI functions must run
//! on the process main thread; the engine spawns a helper process for them.

#[cfg(target_os = "macos")]
mod ffi;

use anyhow::{anyhow, bail, Context, Result};
use bugle_core::{BugleError, DisplayInfo, OcrLine, Point, Rect, WindowInfo};
use image::RgbaImage;
use std::ffi::{CStr, CString};
use std::path::Path;

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

/// Whether a selection overlay is currently on screen.
pub fn select_rect_active() -> bool {
    unsafe { ffi::bugle_select_rect_active() == 1 }
}

/// Dismiss the selection overlay as if Esc was pressed. Main thread only.
pub fn select_rect_cancel() {
    unsafe { ffi::bugle_select_rect_cancel() }
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
    let p = path_cstr(path)?;
    let (mut w, mut h) = (0i32, 0i32);
    if unsafe { ffi::bugle_video_size(p.as_ptr(), &mut w, &mut h) } != 0 {
        bail!("{}", last_error());
    }
    Ok((w.max(0) as u32, h.max(0) as u32))
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
/// resamples the video (ignored by `Passthrough`). A failed export leaves no
/// file at `output`.
pub fn export_video(
    input: &Path,
    output: &Path,
    start: f64,
    end: f64,
    preset: VideoExportPreset,
    mp4: bool,
    fps: Option<u32>,
) -> Result<()> {
    let i = path_cstr(input)?;
    let o = path_cstr(output)?;
    let fps = fps.map(|f| f.min(240) as i32).unwrap_or(0);
    if unsafe { ffi::bugle_export_video(i.as_ptr(), o.as_ptr(), start, end, preset.code(), mp4 as i32, fps) } != 0 {
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
}

impl Drop for VideoFrameReader {
    fn drop(&mut self) {
        unsafe { ffi::bugle_frames_close(self.handle) }
    }
}

/// Encodes RGBA frames of one fixed, even size to an H.264 movie at a
/// constant frame rate. Dropping an unfinished writer discards the file.
pub struct VideoFrameWriter {
    handle: *mut libc::c_void,
    width: u32,
    height: u32,
}

unsafe impl Send for VideoFrameWriter {}

impl VideoFrameWriter {
    pub fn create(path: &Path, width: u32, height: u32, fps: u32, mp4: bool) -> Result<Self> {
        let p = path_cstr(path)?;
        let (Ok(w), Ok(h)) = (i32::try_from(width), i32::try_from(height)) else {
            return Err(BugleError::Invalid(format!("frame size {width}x{height} is too large")).into());
        };
        let fps = i32::try_from(fps).unwrap_or(i32::MAX);
        let handle = unsafe { ffi::bugle_writer_open(p.as_ptr(), w, h, fps, mp4 as i32) };
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

    /// Finalise the movie.
    pub fn finish(mut self) -> Result<()> {
        let h = std::mem::replace(&mut self.handle, std::ptr::null_mut());
        if unsafe { ffi::bugle_writer_finish(h) } != 0 {
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

    /// Video frames written so far.
    pub fn frames(&self) -> u64 {
        unsafe { ffi::bugle_sck_record_frames(self.handle) }.max(0) as u64
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
        export_video(&movie, &cut, 1.0, 2.0, VideoExportPreset::Highest, false, Some(5)).unwrap();
        assert!((media_duration(&cut).unwrap() - 1.0).abs() < 0.1);
        let mut r = VideoFrameReader::open(&cut, 0.0).unwrap();
        assert!(close(r.frame_at(0.1).unwrap().get_pixel(32, 24), [0, 0, 255]));

        // Passthrough (no re-encode) trims too.
        let pass = dir.path().join("pass.mp4");
        export_video(&movie, &pass, 0.5, 1.5, VideoExportPreset::Passthrough, true, None).unwrap();
        assert!((media_duration(&pass).unwrap() - 1.0).abs() < 0.1);
        assert_eq!(video_size(&pass).unwrap(), (64, 48));

        // Bad ranges and inputs fail without leaving a file behind.
        let bad = dir.path().join("bad.mp4");
        assert!(export_video(&movie, &bad, 1.5, 1.0, VideoExportPreset::Passthrough, true, None).is_err());
        assert!(export_video(&dir.path().join("missing.mp4"), &bad, 0.0, 1.0, VideoExportPreset::Passthrough, true, None).is_err());
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
