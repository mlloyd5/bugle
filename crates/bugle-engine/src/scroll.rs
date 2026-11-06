//! Scrolling capture: drive the scroll wheel, grab frames, stitch.

use crate::stitch::{append_strip, estimate_shift_profile, frame_difference};
use crate::{CaptureOutcome, Engine};
use anyhow::{bail, Result};
use bugle_core::{BugleError, MediaKind, Point, ScrollCaptureRequest};
use image::RgbaImage;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScrollStats {
    pub frames: u32,
    pub stitched_pixels: i64,
    pub stopped_reason: String,
}

impl Engine {
    /// Capture a scrolling region. The mouse must not be moved while this
    /// runs; the scroll events are posted at the centre of the rect.
    pub fn scroll_capture(&self, req: &ScrollCaptureRequest) -> Result<(CaptureOutcome, ScrollStats)> {
        if !bugle_platform::screen_access_preflight() {
            return Err(BugleError::PermissionDenied.into());
        }
        if !bugle_platform::accessibility_trusted(true) {
            bail!("scrolling capture posts scroll events and needs Accessibility access (System Settings → Privacy & Security → Accessibility)");
        }
        let rect = req.rect;
        if rect.is_empty() {
            return Err(BugleError::Invalid("empty scroll rect".into()).into());
        }
        let at = rect.center();
        let horizontal = req.horizontal;
        let settle = Duration::from_millis(req.settle_ms.max(50));

        let first = bugle_platform::capture_rect(rect, false)?;
        let mut canvas = first.clone();
        let mut prev = first;
        let mut frames = 1u32;
        let mut total_shift = 0i64;
        let mut reason = "max frames".to_string();
        let mut stalls = 0;

        while frames < req.max_frames {
            if horizontal {
                bugle_platform::post_scroll(at, req.step_lines, 0)?;
            } else {
                bugle_platform::post_scroll(at, 0, req.step_lines)?;
            }
            std::thread::sleep(settle);
            let next = bugle_platform::capture_rect(rect, false)?;
            frames += 1;
            if frame_difference(&prev, &next) < 0.5 {
                stalls += 1;
                if stalls >= 2 {
                    reason = "end of content".into();
                    break;
                }
                continue;
            }
            stalls = 0;
            let shift = self.measure_shift(&prev, &next, horizontal);
            tracing::debug!("frame {frames}: shift {shift}");
            if shift <= 1 {
                reason = "no measurable scroll".into();
                break;
            }
            let frame_len = if horizontal { next.width() } else { next.height() } as i64;
            if shift >= frame_len {
                // Scrolled more than a frame: we lost the overlap. Append whole frame.
                append_strip(&mut canvas, &next, frame_len, horizontal);
            } else {
                append_strip(&mut canvas, &next, shift, horizontal);
            }
            total_shift += shift;
            prev = next;
        }

        let mut vars = HashMap::new();
        vars.insert("kind", "scroll".to_string());
        let format = req.options.format.unwrap_or(self.config.screenshots.format);
        let media = self.save_image(
            &canvas,
            MediaKind::ScrollingCapture,
            format,
            req.options.quality.unwrap_or(self.config.screenshots.jpeg_quality),
            req.options.output.as_deref(),
            &vars,
            req.options.label.clone(),
            Some(format!("scrolling capture {rect} ({frames} frames)")),
            None,
        )?;
        if req.options.copy.unwrap_or(self.config.general.copy_to_clipboard) {
            self.copy_to_clipboard(&media.path, Some(&canvas))?;
        }
        Ok((
            CaptureOutcome { media, image: canvas, rect: Some(rect), window: None },
            ScrollStats { frames, stitched_pixels: total_shift, stopped_reason: reason },
        ))
    }

    /// Vision registration first; profile correlation when Vision reports no
    /// movement for frames that clearly differ (sticky headers) or fails.
    fn measure_shift(&self, prev: &RgbaImage, next: &RgbaImage, horizontal: bool) -> i64 {
        let vision = bugle_platform::register_translation(prev, next).ok().map(|(dx, dy)| {
            // Registration gives where `next`'s content sits relative to
            // `prev`; forward scrolling moves content towards the origin.
            if horizontal {
                (-dx).round() as i64
            } else {
                (-dy).round() as i64
            }
        });
        let fallback = estimate_shift_profile(prev, next, horizontal);
        match vision {
            Some(v) if v > 1 => {
                if fallback.confidence > 0.5 && (fallback.pixels - v).abs() > 2 {
                    tracing::debug!("vision {v} vs profile {} (conf {:.2})", fallback.pixels, fallback.confidence);
                }
                v
            }
            _ => fallback.pixels,
        }
    }

    /// Scroll back up by posting reverse events — courtesy for the user.
    pub fn scroll_back(&self, at: Point, lines: i32, times: u32) {
        for _ in 0..times {
            let _ = bugle_platform::post_scroll(at, 0, lines);
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}
