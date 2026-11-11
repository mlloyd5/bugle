//! Scrolling capture: drive the scroll wheel, grab frames, stitch.

use crate::stitch::{choose_shift, estimate_shift_profile, frame_difference, StripCanvas};
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

/// Accepted range of `max_frames` (values outside are clamped).
pub const MAX_FRAMES_RANGE: (u32, u32) = (2, 500);
/// Accepted range of `settle_ms` (values outside are clamped).
pub const SETTLE_MS_RANGE: (u64, u64) = (50, 5000);
/// Largest accepted `|step_lines|`; zero and larger values are rejected.
pub const MAX_STEP_LINES: u32 = 100;
/// Largest stitched canvas, in pixels (200 megapixels).
pub const CANVAS_PIXEL_LIMIT: u64 = 200_000_000;

/// Validated, clamped parameters of a scroll capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollPlan {
    pub max_frames: u32,
    pub step_lines: i32,
    pub settle: Duration,
}

/// Validate a request: reject an empty rect and a zero or out-of-range
/// `step_lines`; clamp `max_frames` and `settle_ms` into their ranges.
pub(crate) fn plan_scroll(req: &ScrollCaptureRequest) -> Result<ScrollPlan, BugleError> {
    if req.rect.is_empty() {
        return Err(BugleError::Invalid("empty scroll rect".into()));
    }
    if req.step_lines == 0 {
        return Err(BugleError::Invalid("step_lines must be non-zero".into()));
    }
    if req.step_lines.unsigned_abs() > MAX_STEP_LINES {
        return Err(BugleError::Invalid(format!(
            "step_lines {} is out of range (-{MAX_STEP_LINES}..={MAX_STEP_LINES})",
            req.step_lines
        )));
    }
    Ok(ScrollPlan {
        max_frames: req.max_frames.clamp(MAX_FRAMES_RANGE.0, MAX_FRAMES_RANGE.1),
        step_lines: req.step_lines,
        settle: Duration::from_millis(req.settle_ms.clamp(SETTLE_MS_RANGE.0, SETTLE_MS_RANGE.1)),
    })
}

/// What to do with a freshly grabbed frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameCheck {
    /// The frame does not have the size of the first frame.
    SizeChanged,
    /// Nothing moved since the previous frame.
    Stalled,
    /// The frame differs and should be measured.
    Moved,
}

pub(crate) fn check_frame(first: (u32, u32), prev: &RgbaImage, next: &RgbaImage) -> FrameCheck {
    if next.dimensions() != first {
        FrameCheck::SizeChanged
    } else if frame_difference(prev, next) < 0.5 {
        FrameCheck::Stalled
    } else {
        FrameCheck::Moved
    }
}

/// Whether appending `shift` pixels of `next` would take the canvas past
/// `limit` pixels.
pub(crate) fn exceeds_canvas_limit(canvas: &StripCanvas, next: &RgbaImage, shift: i64, limit: u64) -> bool {
    canvas.pixels_after(next, shift) > limit
}

/// Result of the scroll loop.
struct Scrolled {
    canvas: StripCanvas,
    frames: u32,
    total_shift: i64,
    reason: String,
}

impl Engine {
    /// Capture a scrolling region. The mouse must not be moved while this
    /// runs; the scroll events are posted at the centre of the rect. The view
    /// is scrolled back by the number of steps taken when the capture ends,
    /// whether it succeeded or not.
    pub fn scroll_capture(&self, req: &ScrollCaptureRequest) -> Result<(CaptureOutcome, ScrollStats)> {
        let plan = plan_scroll(req)?;
        if !bugle_platform::screen_access_preflight() {
            return Err(BugleError::PermissionDenied.into());
        }
        if !bugle_platform::accessibility_trusted(true) {
            bail!("scrolling capture posts scroll events and needs Accessibility access (System Settings → Privacy & Security → Accessibility)");
        }
        let rect = req.rect;
        let at = rect.center();
        let horizontal = req.horizontal;

        let mut steps = 0u32;
        let scrolled = self.scroll_frames(req, &plan, &mut steps);
        self.scroll_back_along(at, -plan.step_lines, steps, horizontal);
        let Scrolled { canvas, frames, total_shift, reason } = scrolled?;
        let canvas = canvas.finish();

        let mut vars = HashMap::new();
        vars.insert("kind", "scroll".to_string());
        let format = req.options.format.unwrap_or(self.config.screenshots.format);
        let media = self.save_image(
            &canvas,
            MediaKind::ScrollCapture,
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
            CaptureOutcome { item: media, image: canvas, rect: Some(rect), window: None },
            ScrollStats { frames, stitched_pixels: total_shift, stopped_reason: reason },
        ))
    }

    /// Scroll, grab and queue strips. `steps` counts the scroll events
    /// posted, so the caller can undo them even when this fails.
    fn scroll_frames(&self, req: &ScrollCaptureRequest, plan: &ScrollPlan, steps: &mut u32) -> Result<Scrolled> {
        let rect = req.rect;
        let at = rect.center();
        let horizontal = req.horizontal;

        let first = bugle_platform::capture_area(rect, false)?;
        let first_size = first.dimensions();
        let mut prev = first.clone();
        let mut canvas = StripCanvas::new(first, horizontal);
        let mut frames = 1u32;
        let mut total_shift = 0i64;
        let mut reason = "max frames".to_string();
        let mut stalls = 0;

        while frames < plan.max_frames {
            if horizontal {
                bugle_platform::post_scroll(at, plan.step_lines, 0)?;
            } else {
                bugle_platform::post_scroll(at, 0, plan.step_lines)?;
            }
            *steps += 1;
            std::thread::sleep(plan.settle);
            let next = bugle_platform::capture_area(rect, false)?;
            frames += 1;
            match check_frame(first_size, &prev, &next) {
                FrameCheck::SizeChanged => {
                    reason = "frame size changed".into();
                    break;
                }
                FrameCheck::Stalled => {
                    stalls += 1;
                    if stalls >= 2 {
                        reason = "end of content".into();
                        break;
                    }
                    continue;
                }
                FrameCheck::Moved => {}
            }
            stalls = 0;
            let shift = self.measure_shift(&prev, &next, horizontal);
            tracing::debug!("frame {frames}: shift {shift}");
            if shift <= 1 {
                reason = "no measurable scroll".into();
                break;
            }
            // A shift of a frame or more lost the overlap; the strip is
            // clamped to the frame, so the whole frame is appended.
            if exceeds_canvas_limit(&canvas, &next, shift, CANVAS_PIXEL_LIMIT) {
                reason = "canvas limit".into();
                break;
            }
            total_shift += canvas.push(&next, shift);
            prev = next;
        }
        Ok(Scrolled { canvas, frames, total_shift, reason })
    }

    /// Vision registration first; the profile estimate when Vision fails,
    /// reports no movement, or disagrees with a confident profile estimate
    /// (sticky headers). See [`choose_shift`].
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
        let profile = estimate_shift_profile(prev, next, horizontal);
        let chosen = choose_shift(vision, profile);
        if let Some(v) = vision {
            if v != chosen {
                tracing::debug!("vision {v} vs profile {} (conf {:.2}): using {chosen}", profile.pixels, profile.confidence);
            }
        }
        chosen
    }

    /// Scroll back up by posting reverse events — courtesy for the user.
    pub fn scroll_back(&self, at: Point, lines: i32, times: u32) {
        self.scroll_back_along(at, lines, times, false);
    }

    /// Post `times` scroll events of `lines` along the given axis.
    fn scroll_back_along(&self, at: Point, lines: i32, times: u32, horizontal: bool) {
        for _ in 0..times {
            let _ = if horizontal {
                bugle_platform::post_scroll(at, lines, 0)
            } else {
                bugle_platform::post_scroll(at, 0, lines)
            };
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(max_frames: u32, step_lines: i32, settle_ms: u64) -> ScrollCaptureRequest {
        let mut r: ScrollCaptureRequest = serde_json::from_value(serde_json::json!({
            "rect": bugle_core::Rect { x: 0.0, y: 0.0, w: 200.0, h: 300.0 }
        }))
        .unwrap();
        r.max_frames = max_frames;
        r.step_lines = step_lines;
        r.settle_ms = settle_ms;
        r
    }

    #[test]
    fn requests_are_clamped() {
        let p = plan_scroll(&req(4_000_000_000, -8, 10_000_000)).unwrap();
        assert_eq!(p, ScrollPlan { max_frames: 500, step_lines: -8, settle: Duration::from_millis(5000) });
        let p = plan_scroll(&req(0, 100, 0)).unwrap();
        assert_eq!(p, ScrollPlan { max_frames: 2, step_lines: 100, settle: Duration::from_millis(50) });
        let p = plan_scroll(&req(40, -100, 350)).unwrap();
        assert_eq!(p, ScrollPlan { max_frames: 40, step_lines: -100, settle: Duration::from_millis(350) });
    }

    #[test]
    fn nonsense_requests_are_rejected() {
        for bad in [req(40, 0, 350), req(40, 101, 350), req(40, -101, 350), req(40, i32::MIN, 350)] {
            assert!(matches!(plan_scroll(&bad), Err(BugleError::Invalid(_))), "{}", bad.step_lines);
        }
        let mut empty = req(40, -8, 350);
        empty.rect.w = 0.0;
        assert!(matches!(plan_scroll(&empty), Err(BugleError::Invalid(_))));
    }

    #[test]
    fn frame_checks() {
        let a = RgbaImage::from_pixel(50, 40, image::Rgba([10, 20, 30, 255]));
        let b = RgbaImage::from_pixel(50, 40, image::Rgba([200, 20, 30, 255]));
        let resized = RgbaImage::from_pixel(60, 40, image::Rgba([10, 20, 30, 255]));
        assert_eq!(check_frame((50, 40), &a, &a), FrameCheck::Stalled);
        assert_eq!(check_frame((50, 40), &a, &b), FrameCheck::Moved);
        assert_eq!(check_frame((50, 40), &a, &resized), FrameCheck::SizeChanged);
    }

    #[test]
    fn canvas_limit() {
        let frame = RgbaImage::new(100, 50);
        let mut canvas = StripCanvas::new(frame.clone(), false);
        // 100 x 50 = 5_000 pixels so far; a limit of 7_000 allows 20 more rows.
        assert!(!exceeds_canvas_limit(&canvas, &frame, 20, 7_000));
        assert!(exceeds_canvas_limit(&canvas, &frame, 21, 7_000));
        // Oversized shifts are clamped to the frame length (50 rows).
        assert!(!exceeds_canvas_limit(&canvas, &frame, 1_000_000, 10_000));
        canvas.push(&frame, 50);
        assert!(exceeds_canvas_limit(&canvas, &frame, 2, 10_000));
        assert!(!exceeds_canvas_limit(&canvas, &frame, 2, CANVAS_PIXEL_LIMIT));
    }
}
