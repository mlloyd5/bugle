//! Screenshot pipeline: resolve target → grab pixels → post-process → save.

use crate::Engine;
use anyhow::{anyhow, Result};
use bugle_core::{BugleError, CaptureRequest, CaptureTarget, DisplayInfo, Media, MediaKind, Rect, WindowInfo};
use image::RgbaImage;
use std::collections::HashMap;

#[derive(Debug)]
pub struct CaptureOutcome {
    pub media: Media,
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
                let title_ok = t.as_ref().map_or(true, |t| w.title.to_ascii_lowercase().contains(t.as_str()));
                let owner_ok = o.as_ref().map_or(true, |o| w.owner.to_ascii_lowercase().contains(o.as_str()));
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
                let img = bugle_platform::capture_rect(*rect, show_cursor)?;
                let scale = img.width() as f64 / rect.w.max(1.0);
                Ok((self.maybe_downscale(img, scale), Some(*rect), None))
            }
            CaptureTarget::Window { id, title, owner } => {
                let w = self.resolve_window(*id, title.as_deref(), owner.as_deref())?;
                let img = bugle_platform::capture_window(w.id, shadow, show_cursor)?;
                let scale = if shadow { 1.0 } else { img.width() as f64 / w.frame.w.max(1.0) };
                Ok((self.maybe_downscale(img, scale), Some(w.frame), Some(w)))
            }
            CaptureTarget::Interactive => {
                let rect = self.select_area()?.ok_or_else(|| anyhow!("selection cancelled"))?;
                let img = bugle_platform::capture_rect(rect, show_cursor)?;
                let scale = img.width() as f64 / rect.w.max(1.0);
                Ok((self.maybe_downscale(img, scale), Some(rect), None))
            }
            CaptureTarget::AllScreens => {
                let displays = bugle_platform::displays()?;
                let min_x = displays.iter().map(|d| d.frame.x).fold(f64::MAX, f64::min);
                let min_y = displays.iter().map(|d| d.frame.y).fold(f64::MAX, f64::min);
                let max_x = displays.iter().map(|d| d.frame.right()).fold(f64::MIN, f64::max);
                let max_y = displays.iter().map(|d| d.frame.bottom()).fold(f64::MIN, f64::max);
                let scale = displays.iter().map(|d| d.scale).fold(1.0, f64::max);
                let w = ((max_x - min_x) * scale).round() as u32;
                let h = ((max_y - min_y) * scale).round() as u32;
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
                Ok((self.maybe_downscale(canvas, scale), Some(Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)), None))
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
        if !bugle_platform::screen_access_preflight() {
            bugle_platform::screen_access_request();
            return Err(BugleError::PermissionDenied.into());
        }
        let o = &req.options;
        if let Some(d) = o.delay {
            if d > 0.0 {
                std::thread::sleep(std::time::Duration::from_secs_f64(d));
            }
        }
        let shadow = o.shadow.unwrap_or(self.config.screenshots.window_shadow);
        let show_cursor = !o.hide_cursor.unwrap_or(true);
        let (image, rect, window) = self.grab(&req.target, shadow, show_cursor)?;
        if self.config.general.sound {
            bugle_platform::play_sound("Tink");
        }
        let format = o.format.unwrap_or(self.config.screenshots.format);
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
        let source = match &req.target {
            CaptureTarget::Window { .. } => window
                .as_ref()
                .map(|w| format!("window {} — {} ({})", w.id, w.owner, w.title))
                .unwrap_or_else(|| "window".into()),
            CaptureTarget::Screen { display } => format!("screen {}", display.map(|d| d.to_string()).unwrap_or("main".into())),
            CaptureTarget::Area { rect } => format!("area {rect}"),
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
        Ok(CaptureOutcome { media, image, rect, window })
    }
}
