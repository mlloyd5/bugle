//! Screenshots of the menu-bar UI for a person to look at, taken at chosen
//! `--smoke-test` steps when `BUGLE_SMOKE_SCREENSHOTS` names a folder
//! (`cargo xtask smoke-vm` sets it in the guest and copies the folder back).
//! The steps check the same states by reading them; these pictures show
//! what they look like: the recording timer, the menu and the toast while
//! another process records, the live level meter.
//!
//! Each shot is the main display (`NN-<name>.png`), the rightmost
//! [`BAR_WIDTH`] points of the menu bar (`NN-<name>-bar.png`, the same strip
//! of the display every time, so the timer's width can be compared between
//! shots) and, while a menu or toast of this app is up, a crop around it and
//! the status item (`NN-<name>-crop.png`). They are captured through
//! ScreenCaptureKit from a worker thread, so the main thread keeps running
//! (an open menu keeps tracking). Toasts are kept out of screen captures by
//! design; while shots are on they are let in ([`init`]). A shot that fails
//! is reported on a `smoke test: screenshot` line and does not fail the run:
//! the pictures are for review, the checks are elsewhere.

use bugle_core::{Rect, WindowInfo};
use image::RgbaImage;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The folder named by `BUGLE_SMOKE_SCREENSHOTS`, if any.
fn folder() -> Option<PathBuf> {
    std::env::var_os("BUGLE_SMOKE_SCREENSHOTS").filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Whether shots are taken at all.
pub fn enabled() -> bool {
    folder().is_some()
}

/// Numbers the shots in the order they were taken.
static NEXT: AtomicU32 = AtomicU32::new(1);
/// The app's `NSStatusItem`, set once on the main thread ([`set_status_item`]).
static STATUS_ITEM: AtomicUsize = AtomicUsize::new(0);

/// The window level of an open menu (`NSPopUpMenuWindowLevel`) and of the
/// status bar (`NSStatusWindowLevel`), at which this app shows its toasts.
const MENU_LEVEL: i32 = 101;
const STATUS_LEVEL: i32 = 25;
/// Points kept around the status item, menu and toast in the crop.
const MARGIN: f64 = 24.0;
/// How much of the menu bar, from the display's right edge, the bar strip
/// shows.
const BAR_WIDTH: f64 = 400.0;

fn say(msg: impl AsRef<str>) {
    eprintln!("smoke test: screenshot {}", msg.as_ref());
}

/// When shots are on: let toasts into screen captures.
pub fn init() {
    if enabled() {
        bugle_platform::toast_set_capturable(true);
    }
}

/// Main thread, once: remember the status item, so a shot can find it on
/// screen (on recent macOS its window is not this process's in the window
/// list).
pub fn set_status_item(tray: &tray_icon::TrayIcon) {
    if let Some(item) = tray.ns_status_item() {
        // The tray icon keeps the item alive as long as the app runs.
        STATUS_ITEM.store(&*item as *const _ as usize, Ordering::SeqCst);
    }
}

/// The status item's frame found by the last shot that could ask for it.
static LAST_FRAME: std::sync::Mutex<Option<Rect>> = std::sync::Mutex::new(None);

/// The status item's frame on screen (global points, top-left origin), and
/// whether it is the one the last shot found. While a menu of this app is
/// open it is not asked: the frame comes from the main thread, which then
/// runs the menu's tracking loop and does not serve the main queue (the
/// request would wait out its second), and a request posted to it must not
/// add a wake-up of the main run loop to the level-meter step's count.
fn status_item_frame(menu_open: bool) -> Option<(Rect, bool)> {
    let item = STATUS_ITEM.load(Ordering::SeqCst);
    let mut last = LAST_FRAME.lock().unwrap_or_else(|e| e.into_inner());
    let ask = item != 0 && !menu_open;
    // SAFETY: set from the tray icon, which lives as long as the app.
    match ask.then(|| unsafe { bugle_platform::status_item_frame(item as *mut std::ffi::c_void) }).flatten() {
        Some(r) => {
            *last = Some(r);
            Some((r, false))
        }
        None => last.map(|r| (r, true)),
    }
}

/// This process's on-screen menus and toasts.
fn menus_and_toasts() -> Vec<WindowInfo> {
    let pid = std::process::id() as i32;
    bugle_platform::windows(true)
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.pid == pid && w.alpha > 0.0 && w.frame.w > 0.0 && w.frame.h > 0.0)
        .filter(|w| w.layer == MENU_LEVEL || is_toast(w))
        .collect()
}

/// A toast: a panel at the status level of a toast's size (this app keeps
/// other, larger and transparent windows at that level; its status item's
/// window is not its own in the window list).
fn is_toast(w: &WindowInfo) -> bool {
    w.layer == STATUS_LEVEL && w.frame.w <= 400.0 && w.frame.h <= 240.0
}

/// The window ids of this app's toasts on screen. A new toast is a new
/// window (one shown over another replaces it), so a toast appeared when an
/// id not seen before is listed.
pub fn toast_ids() -> std::collections::HashSet<u32> {
    menus_and_toasts().iter().filter(|w| w.layer == STATUS_LEVEL).map(|w| w.id).collect()
}

/// Whether one of this process's menus is open on screen.
pub fn menu_open() -> bool {
    menus_and_toasts().iter().any(|w| w.layer == MENU_LEVEL)
}

/// Take shot `name` now. Returns at once when shots are off.
pub fn shoot(name: &str) {
    let Some(dir) = folder() else { return };
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let stem = format!("{n:02}-{name}");
    if let Err(e) = take(&dir, &stem) {
        say(format!("{stem} FAILED ({e:#}); not a smoke-test failure"));
    }
}

/// Wait (at most `limit`) for one of this process's menus to open, give it
/// `settle` to draw, take shot `name` and say whether the menu was still
/// open afterwards (a menu that closed during the capture may be missing
/// from it).
pub fn shoot_open_menu(name: &str, settle: Duration, limit: Duration) {
    if !enabled() {
        return;
    }
    let t = Instant::now();
    while !menu_open() {
        if t.elapsed() > limit {
            say(format!("{name}: no menu of this app opened within {:.1}s; shot taken anyway", limit.as_secs_f64()));
            return shoot(name);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    std::thread::sleep(settle);
    shoot(name);
    if !menu_open() {
        say(format!("{name}: the menu closed before the capture finished, so it may be missing from the picture"));
    }
}

/// [`shoot_open_menu`] on a thread of its own, so the caller can open the
/// menu (and wait for it to close) meanwhile. `None` when shots are off.
pub fn shoot_open_menu_soon(name: &'static str) -> Option<std::thread::JoinHandle<()>> {
    enabled().then(|| std::thread::spawn(move || shoot_open_menu(name, Duration::from_millis(350), Duration::from_secs(4))))
}

fn take(dir: &std::path::Path, stem: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let display = bugle_platform::main_display()?;
    let ws = menus_and_toasts();
    let menu_open = ws.iter().any(|w| w.layer == MENU_LEVEL);
    let img = bugle_platform::capture_display(Some(display.id), false)?;
    img.save(dir.join(format!("{stem}.png")))?;
    let scale = img.width() as f64 / display.frame.w;
    let mut notes = vec![format!("{}×{} px", img.width(), img.height())];
    let item = match status_item_frame(menu_open) {
        Some((r, stale)) => {
            let when = if stale { ", as the last shot found it" } else { "" };
            notes.push(format!("status item x {:.0}..{:.0} ({:.0}×{:.0} pt{when})", r.x, r.x + r.w, r.w, r.h));
            let right = display.frame.x + display.frame.w;
            let bar = Rect::new(right - BAR_WIDTH, display.frame.y, BAR_WIDTH, r.y + r.h - display.frame.y);
            if let Some(bar) = clamp(&bar, &display.frame) {
                crop(&img, &bar, &display.frame, scale).save(dir.join(format!("{stem}-bar.png")))?;
            }
            Some(r)
        }
        None => {
            notes.push("status item not found".into());
            None
        }
    };
    if !ws.is_empty() {
        let frames: Vec<Rect> = ws.iter().map(|w| w.frame).chain(item).collect();
        if let Some(r) = union(&frames).and_then(|u| clamp(&grow(&u, MARGIN), &display.frame)) {
            crop(&img, &r, &display.frame, scale).save(dir.join(format!("{stem}-crop.png")))?;
        }
        let menus = ws.iter().filter(|w| w.layer == MENU_LEVEL).count();
        notes.push(format!("{menus} menu(s), {} toast(s) of this app", ws.len() - menus));
    }
    say(format!("{stem}: {}", notes.join(", ")));
    Ok(())
}

/// The smallest rect holding all of `rs`.
fn union(rs: &[Rect]) -> Option<Rect> {
    let first = rs.first()?;
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x + first.w, first.y + first.h);
    for r in &rs[1..] {
        x0 = x0.min(r.x);
        y0 = y0.min(r.y);
        x1 = x1.max(r.x + r.w);
        y1 = y1.max(r.y + r.h);
    }
    Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
}

fn grow(r: &Rect, by: f64) -> Rect {
    Rect::new(r.x - by, r.y - by, r.w + 2.0 * by, r.h + 2.0 * by)
}

/// `r` cut to `within`; `None` if nothing is left.
fn clamp(r: &Rect, within: &Rect) -> Option<Rect> {
    let x0 = r.x.max(within.x);
    let y0 = r.y.max(within.y);
    let x1 = (r.x + r.w).min(within.x + within.w);
    let y1 = (r.y + r.h).min(within.y + within.h);
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// `r` (global points) cut out of `img`, a capture of `display` at `scale`
/// pixels per point.
fn crop(img: &RgbaImage, r: &Rect, display: &Rect, scale: f64) -> RgbaImage {
    let px = |v: f64, max: u32| ((v * scale).round().max(0.0) as u32).min(max);
    let (x, y) = (px(r.x - display.x, img.width()), px(r.y - display.y, img.height()));
    let (w, h) = (px(r.w, img.width() - x), px(r.h, img.height() - y));
    image::imageops::crop_imm(img, x, y, w.max(1), h.max(1)).to_image()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops_stay_within_the_display() {
        let display = Rect::new(0.0, 0.0, 1024.0, 768.0);
        let item = Rect::new(900.0, 0.0, 60.0, 24.0);
        let menu = Rect::new(880.0, 24.0, 300.0, 200.0);
        let u = union(&[item, menu]).unwrap();
        assert_eq!(u, Rect::new(880.0, 0.0, 300.0, 224.0));
        assert_eq!(clamp(&grow(&u, MARGIN), &display), Some(Rect::new(856.0, 0.0, 168.0, 248.0)));
        assert_eq!(union(&[]), None);
        assert_eq!(clamp(&Rect::new(2000.0, 0.0, 10.0, 10.0), &display), None);

        let img = RgbaImage::new(2048, 1536);
        let c = crop(&img, &Rect::new(856.0, 0.0, 168.0, 248.0), &display, 2.0);
        assert_eq!((c.width(), c.height()), (336, 496));
        // Never past the image, whatever the rect says.
        let c = crop(&img, &Rect::new(1000.0, 700.0, 500.0, 500.0), &display, 2.0);
        assert_eq!((c.width(), c.height()), (48, 136));
    }
}
