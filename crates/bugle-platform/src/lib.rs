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
    let len = (w as usize) * (h as usize) * 4;
    let buf = unsafe { std::slice::from_raw_parts(img.data, len) }.to_vec();
    unsafe { ffi::bugle_image_free(&mut img) };
    RgbaImage::from_raw(w, h, buf).ok_or_else(|| anyhow!("bad image buffer"))
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
    let mut buf: Vec<ffi::bugle_window_t> = Vec::with_capacity(1024);
    let n = unsafe { ffi::bugle_list_windows(buf.as_mut_ptr(), 1024, on_screen_only as i32) };
    if n < 0 {
        bail!("could not enumerate windows");
    }
    unsafe { buf.set_len(n as usize) };
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

pub fn capture_rect(rect: Rect, show_cursor: bool) -> Result<RgbaImage> {
    if rect.is_empty() {
        return Err(BugleError::Invalid("empty capture rect".into()).into());
    }
    take_image(unsafe { ffi::bugle_capture_rect(rect.x, rect.y, rect.w, rect.h, show_cursor as i32) })
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
    let langs = cstr(&params.languages.join(","))?;
    let mut lines: *mut ffi::bugle_ocr_line_t = std::ptr::null_mut();
    let mut count: i32 = 0;
    let rc = unsafe {
        ffi::bugle_ocr(
            img.as_raw().as_ptr(),
            img.width() as i32,
            img.height() as i32,
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
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let l = unsafe { &*lines.add(i) };
        let text = unsafe { CStr::from_ptr(l.text) }.to_string_lossy().into_owned();
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
    let (mut dx, mut dy) = (0.0, 0.0);
    let rc = unsafe {
        ffi::bugle_register_translation(
            reference.as_raw().as_ptr(),
            reference.width() as i32,
            reference.height() as i32,
            floating.as_raw().as_ptr(),
            floating.width() as i32,
            floating.height() as i32,
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

// ---------------------------------------------------------------------------
// Media
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

    #[test]
    fn lists_displays() {
        let d = displays().unwrap();
        assert!(!d.is_empty());
        assert!(d.iter().any(|d| d.is_main));
    }
}
