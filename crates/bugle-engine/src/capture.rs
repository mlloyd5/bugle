//! Screenshot pipeline: resolve target → grab pixels → post-process → save.

use crate::Engine;
use anyhow::Result;
use bugle_core::{BugleError, CaptureRequest, CaptureTarget, DisplayInfo, MediaItem, MediaKind, Rect, WindowInfo};
use image::RgbaImage;
use std::collections::HashMap;

/// Largest coordinate or size (in points) accepted for a capture rect.
/// Generous enough for any real display arrangement.
pub const MAX_RECT_POINTS: f64 = 100_000.0;
/// Largest side and pixel count of the stitched `all_screens` canvas.
pub const MAX_CANVAS_SIDE: u32 = 32_768;
pub const MAX_CANVAS_PIXELS: u64 = 256 * 1024 * 1024;

/// Reject rects the platform layer cannot sensibly capture: non-finite
/// values, empty or negative sizes, and absurd coordinates.
pub fn validate_rect(r: &Rect) -> Result<()> {
    if ![r.x, r.y, r.w, r.h].iter().all(|v| v.is_finite()) {
        return Err(BugleError::Invalid(format!("rect {r} has non-finite values")).into());
    }
    if r.w < 1.0 || r.h < 1.0 {
        return Err(BugleError::Invalid(format!("rect {r} is empty (w and h must be at least 1)")).into());
    }
    if r.w > MAX_RECT_POINTS || r.h > MAX_RECT_POINTS || r.x.abs() > MAX_RECT_POINTS || r.y.abs() > MAX_RECT_POINTS {
        return Err(BugleError::Invalid(format!("rect {r} is out of range (limit {MAX_RECT_POINTS} points)")).into());
    }
    Ok(())
}

/// Validate the rect of an area target (other targets carry none).
pub fn validate_target(t: &CaptureTarget) -> Result<()> {
    match t {
        CaptureTarget::Area { rect } => validate_rect(rect),
        _ => Ok(()),
    }
}

/// Geometry of the `all_screens` canvas.
#[derive(Debug, Clone, PartialEq)]
pub struct AllScreensCanvas {
    /// Union of all display frames (points).
    pub bounds: Rect,
    /// Highest display scale; the canvas is rendered at this scale.
    pub scale: f64,
    pub width: u32,
    pub height: u32,
}

/// Compute (and bound) the canvas that holds every display.
pub fn all_screens_canvas(displays: &[DisplayInfo]) -> Result<AllScreensCanvas> {
    if displays.is_empty() {
        return Err(BugleError::Invalid("no displays to capture".into()).into());
    }
    let min_x = displays.iter().map(|d| d.frame.x).fold(f64::MAX, f64::min);
    let min_y = displays.iter().map(|d| d.frame.y).fold(f64::MAX, f64::min);
    let max_x = displays.iter().map(|d| d.frame.right()).fold(f64::MIN, f64::max);
    let max_y = displays.iter().map(|d| d.frame.bottom()).fold(f64::MIN, f64::max);
    let scale = displays.iter().map(|d| d.scale).filter(|s| s.is_finite()).fold(1.0, f64::max);
    let bounds = Rect::new(min_x, min_y, max_x - min_x, max_y - min_y);
    validate_rect(&bounds)?;
    let w = (bounds.w * scale).round();
    let h = (bounds.h * scale).round();
    if w > MAX_CANVAS_SIDE as f64 || h > MAX_CANVAS_SIDE as f64 || w * h > MAX_CANVAS_PIXELS as f64 {
        return Err(BugleError::Invalid(format!(
            "all-screens canvas would be {w}x{h} pixels (limit {MAX_CANVAS_SIDE} per side, {MAX_CANVAS_PIXELS} pixels); capture displays individually"
        ))
        .into());
    }
    Ok(AllScreensCanvas { bounds, scale, width: w as u32, height: h as u32 })
}

#[derive(Debug)]
pub struct CaptureOutcome {
    pub item: MediaItem,
    pub image: RgbaImage,
    /// Screen rect that was captured, when known (points).
    pub rect: Option<Rect>,
    pub window: Option<WindowInfo>,
}

impl Engine {
    pub fn displays(&self) -> Result<Vec<DisplayInfo>> {
        bugle_platform::displays()
    }

    pub fn windows(&self, all: bool) -> Result<Vec<WindowInfo>> {
        if all {
            bugle_platform::windows(false)
        } else {
            bugle_platform::app_windows()
        }
    }

    /// Find a window by id, or by case-insensitive substring of title/owner.
    /// Prefers on-screen, front-most (first in z-order) matches.
    pub fn resolve_window(&self, id: Option<u32>, title: Option<&str>, owner: Option<&str>) -> Result<WindowInfo> {
        let wins = bugle_platform::app_windows()?;
        if let Some(id) = id {
            return wins
                .into_iter()
                .find(|w| w.id == id)
                .ok_or_else(|| BugleError::NoSuchWindow(format!("id {id}")).into());
        }
        let t = title.map(|s| s.to_ascii_lowercase());
        let o = owner.map(|s| s.to_ascii_lowercase());
        if t.is_none() && o.is_none() {
            // Front-most application window.
            return wins.into_iter().next().ok_or_else(|| BugleError::NoSuchWindow("no windows on screen".into()).into());
        }
        wins.into_iter()
            .find(|w| {
                let title_ok = t.as_ref().is_none_or(|t| w.title.to_ascii_lowercase().contains(t.as_str()));
                let owner_ok = o.as_ref().is_none_or(|o| w.owner.to_ascii_lowercase().contains(o.as_str()));
                title_ok && owner_ok
            })
            .ok_or_else(|| {
                BugleError::NoSuchWindow(format!(
                    "title~{} owner~{}",
                    title.unwrap_or("*"),
                    owner.unwrap_or("*")
                ))
                .into()
            })
    }

    /// Grab pixels for a target without saving anything.
    pub fn grab(&self, target: &CaptureTarget, shadow: bool, show_cursor: bool) -> Result<(RgbaImage, Option<Rect>, Option<WindowInfo>)> {
        match target {
            CaptureTarget::Screen { display } => {
                let d = match display {
                    Some(id) => bugle_platform::displays()?
                        .into_iter()
                        .find(|d| d.id == *id)
                        .ok_or(BugleError::NoSuchDisplay(*id))?,
                    None => bugle_platform::main_display()?,
                };
                let img = bugle_platform::capture_display(Some(d.id), show_cursor)?;
                Ok((self.maybe_downscale(img, d.scale), Some(d.frame), None))
            }
            CaptureTarget::Area { rect } => {
                validate_rect(rect)?;
                let img = bugle_platform::capture_area(*rect, show_cursor)?;
                let scale = img.width() as f64 / rect.w.max(1.0);
                Ok((self.maybe_downscale(img, scale), Some(*rect), None))
            }
            CaptureTarget::Window { .. } | CaptureTarget::PickWindow => {
                let w = match target {
                    CaptureTarget::Window { id, title, owner } => self.resolve_window(*id, title.as_deref(), owner.as_deref())?,
                    _ => self.pick_window()?,
                };
                let img = bugle_platform::capture_window(w.id, shadow, show_cursor)?;
                let scale = if shadow { 1.0 } else { img.width() as f64 / w.frame.w.max(1.0) };
                Ok((self.maybe_downscale(img, scale), Some(w.frame), Some(w)))
            }
            CaptureTarget::Interactive => {
                // In a terminal with `[screenshots] window_picker`, Space in
                // the selection switches to the window picker.
                let rect = match self.select_area_or_window()?.ok_or(BugleError::Cancelled)? {
                    crate::pick::Picked::Area(rect) => rect,
                    crate::pick::Picked::Window(w) => {
                        let target = CaptureTarget::Window { id: Some(w.id), title: None, owner: None };
                        let (img, frame, _) = self.grab(&target, shadow, show_cursor)?;
                        return Ok((img, frame, Some(w)));
                    }
                };
                validate_rect(&rect)?;
                let img = bugle_platform::capture_area(rect, show_cursor)?;
                let scale = img.width() as f64 / rect.w.max(1.0);
                Ok((self.maybe_downscale(img, scale), Some(rect), None))
            }
            CaptureTarget::AllScreens => {
                let displays = bugle_platform::displays()?;
                let AllScreensCanvas { bounds, scale, width: w, height: h } = all_screens_canvas(&displays)?;
                let (min_x, min_y) = (bounds.x, bounds.y);
                let mut canvas = RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 255]));
                for d in &displays {
                    let img = bugle_platform::capture_display(Some(d.id), show_cursor)?;
                    let img = if (d.scale - scale).abs() > 0.01 {
                        image::imageops::resize(
                            &img,
                            (d.frame.w * scale) as u32,
                            (d.frame.h * scale) as u32,
                            image::imageops::FilterType::Triangle,
                        )
                    } else {
                        img
                    };
                    image::imageops::overlay(
                        &mut canvas,
                        &img,
                        ((d.frame.x - min_x) * scale) as i64,
                        ((d.frame.y - min_y) * scale) as i64,
                    );
                }
                Ok((self.maybe_downscale(canvas, scale), Some(bounds), None))
            }
        }
    }

    fn maybe_downscale(&self, img: RgbaImage, scale: f64) -> RgbaImage {
        if self.config.screenshots.downscale_retina && scale > 1.01 {
            image::imageops::resize(
                &img,
                (img.width() as f64 / scale).round() as u32,
                (img.height() as f64 / scale).round() as u32,
                image::imageops::FilterType::Lanczos3,
            )
        } else {
            img
        }
    }

    /// Full screenshot flow: delay, grab, save, clipboard, history.
    pub fn capture(&self, req: &CaptureRequest) -> Result<CaptureOutcome> {
        self.check_target_host(&req.target)?;
        if !bugle_platform::screen_access_preflight() {
            bugle_platform::screen_access_request();
            return Err(BugleError::PermissionDenied.into());
        }
        validate_target(&req.target)?;
        let o = &req.options;
        // A picked window is chosen first and the delay counts from the
        // click, so the user can open a menu in it before the grab.
        // (`interactive` keeps its delay before the selection.)
        let picked;
        let target = match &req.target {
            CaptureTarget::PickWindow => {
                let w = self.pick_window()?;
                let t = CaptureTarget::Window { id: Some(w.id), title: None, owner: None };
                picked = Some(w);
                t
            }
            t => {
                picked = None;
                t.clone()
            }
        };
        if let Some(d) = o.delay {
            if d > 0.0 {
                std::thread::sleep(std::time::Duration::from_secs_f64(d));
            }
        }
        let shadow = o.shadow.unwrap_or(self.config.screenshots.window_shadow);
        let show_cursor = !o.hide_cursor.unwrap_or(true);
        let (image, rect, window) = self.grab(&target, shadow, show_cursor)?;
        let window = window.or(picked);
        if self.config.general.shutter_sound {
            bugle_platform::play_sound("Tink");
        }
        // `None` lets save_image infer the format from the output extension.
        let format = o.format;
        let quality = o.quality.unwrap_or(self.config.screenshots.jpeg_quality);
        let mut vars = HashMap::new();
        vars.insert("kind", "screenshot".to_string());
        if let Some(w) = &window {
            vars.insert("app", w.owner.clone());
            vars.insert("title", w.title.clone());
        }
        if let Some(l) = &o.label {
            vars.insert("label", l.clone());
        }
        let source = match &target {
            CaptureTarget::Window { .. } | CaptureTarget::PickWindow => window
                .as_ref()
                .map(|w| format!("window {} — {} ({})", w.id, w.owner, w.title))
                .unwrap_or_else(|| "window".into()),
            CaptureTarget::Screen { display } => format!("screen {}", display.map(|d| d.to_string()).unwrap_or("main".into())),
            CaptureTarget::Area { rect } => format!("area {rect}"),
            // Space in the selection picked a window.
            CaptureTarget::Interactive if window.is_some() => {
                window.as_ref().map(|w| format!("window {} — {} ({})", w.id, w.owner, w.title)).unwrap_or_default()
            }
            CaptureTarget::Interactive => format!("area {}", rect.map(|r| r.to_string()).unwrap_or_default()),
            CaptureTarget::AllScreens => "all screens".into(),
        };
        let media = self.save_image(
            &image,
            MediaKind::Screenshot,
            format,
            quality,
            o.output.as_deref(),
            &vars,
            o.label.clone(),
            Some(source),
            None,
        )?;
        if o.copy.unwrap_or(self.config.general.copy_to_clipboard) {
            self.copy_to_clipboard(&media.path, Some(&image))?;
        }
        Ok(CaptureOutcome { item: media, image, rect, window })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: u32, x: f64, y: f64, w: f64, h: f64, scale: f64) -> DisplayInfo {
        DisplayInfo { id, frame: Rect::new(x, y, w, h), scale, is_main: id == 1 }
    }

    #[test]
    fn rect_validation() {
        assert!(validate_rect(&Rect::new(0.0, 0.0, 100.0, 50.0)).is_ok());
        assert!(validate_rect(&Rect::new(-1440.0, -200.0, 1.0, 1.0)).is_ok());
        for bad in [
            Rect::new(f64::NAN, 0.0, 10.0, 10.0),
            Rect::new(0.0, 0.0, f64::NAN, 10.0),
            Rect::new(0.0, 0.0, 10.0, f64::INFINITY),
            Rect::new(0.0, 0.0, 0.0, 10.0),
            Rect::new(0.0, 0.0, -5.0, 10.0),
            Rect::new(0.0, 0.0, 0.5, 10.0),
            Rect::new(0.0, 0.0, 1e9, 10.0),
            Rect::new(1e9, 0.0, 10.0, 10.0),
        ] {
            let err = validate_rect(&bad).unwrap_err();
            assert!(matches!(err.downcast_ref::<BugleError>(), Some(BugleError::Invalid(_))), "{bad:?}");
        }
        assert!(validate_target(&CaptureTarget::Area { rect: Rect::new(0.0, 0.0, f64::NAN, 1.0) }).is_err());
        assert!(validate_target(&CaptureTarget::AllScreens).is_ok());
    }

    #[test]
    fn all_screens_canvas_bounds() {
        let c = all_screens_canvas(&[display(1, 0.0, 0.0, 1512.0, 982.0, 2.0), display(2, -1920.0, -100.0, 1920.0, 1080.0, 1.0)])
            .unwrap();
        assert_eq!(c.bounds, Rect::new(-1920.0, -100.0, 3432.0, 1082.0));
        assert_eq!(c.scale, 2.0);
        assert_eq!((c.width, c.height), (6864, 2164));

        assert!(all_screens_canvas(&[]).is_err());
        let huge = all_screens_canvas(&[display(1, 0.0, 0.0, 20_000.0, 20_000.0, 2.0)]).unwrap_err();
        assert!(matches!(huge.downcast_ref::<BugleError>(), Some(BugleError::Invalid(_))));
        assert!(all_screens_canvas(&[display(1, 0.0, 0.0, f64::NAN, 100.0, 1.0)]).is_err());
    }
}
