//! Edits driven by a recording's event track (`bugle_core::events`):
//! auto-zoom on clicks and a smoothed, redrawn pointer.
//!
//! * **Auto-zoom.** Clicks become zoom segments: the view eases in to the
//!   zoom factor over [`ZOOM_IN`] seconds so it is fully zoomed at the
//!   click, holds for [`ZOOM_HOLD`] seconds after the last click, then eases
//!   out over [`ZOOM_OUT`]. A click that comes before the previous segment
//!   has zoomed out joins it, and the view pans to it over at most [`PAN`]
//!   seconds, arriving as it happens. The visible rect keeps the frame's
//!   aspect ratio, is centred on the click and is clamped to the frame.
//! * **Cursor smoothing.** The pointer path is resampled at [`GRID_HZ`] and
//!   convolved with a Gaussian ([`SMOOTH_SIGMA`] seconds), which removes
//!   jitter and rounds off hand movements without lag (the filter is
//!   centred). Around each click it blends back to the raw path
//!   ([`PIN_SIGMA`]), so the pointer is exactly where the click landed.
//!   Smoothing needs frames without the pointer (`hide_cursor` recordings);
//!   the pointer is then drawn by [`draw_cursor`] at the smoothed position.
//!   Such recordings get the pointer drawn back (at the raw position)
//!   whenever an edit uses the track, even without smoothing.
//!
//! Event positions are points relative to the recorded area; a frame's
//! pixels per point are its size over the area's size, so this works at any
//! export size (Retina, scaled presets, GIF).

use bugle_core::events::EventTrack;
use image::RgbaImage;

/// Zoom factors accepted by an edit.
pub const MIN_ZOOM: f64 = 1.0;
pub const MAX_ZOOM: f64 = 4.0;
/// Seconds to ease in before a click.
pub const ZOOM_IN: f64 = 0.5;
/// Seconds to stay zoomed after the last click of a segment.
pub const ZOOM_HOLD: f64 = 1.5;
/// Seconds to ease back out.
pub const ZOOM_OUT: f64 = 0.6;
/// Longest pan between two clicks of a segment.
pub const PAN: f64 = 0.5;
/// Gaussian width of the pointer smoothing, in seconds.
pub const SMOOTH_SIGMA: f64 = 0.1;
/// Around a click the smoothed path blends back to the raw one over this.
pub const PIN_SIGMA: f64 = 0.06;
/// Rate the pointer path is resampled at for smoothing.
pub const GRID_HZ: f64 = 120.0;

/// Cubic ease, 0 at `x <= 0`, 1 at `x >= 1`.
fn smoothstep(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn lerp2(a: (f64, f64), b: (f64, f64), k: f64) -> (f64, f64) {
    (a.0 + (b.0 - a.0) * k, a.1 + (b.1 - a.1) * k)
}

/// One run of clicks the view stays zoomed through.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoomSegment {
    /// `(t, x, y)` in track time and area points, in time order.
    pub clicks: Vec<(f64, f64, f64)>,
}

impl ZoomSegment {
    fn first(&self) -> f64 {
        self.clicks[0].0
    }
    fn last(&self) -> f64 {
        self.clicks[self.clicks.len() - 1].0
    }
    /// When easing in starts.
    pub fn start(&self) -> f64 {
        self.first() - ZOOM_IN
    }
    /// When easing out starts.
    pub fn hold_end(&self) -> f64 {
        self.last() + ZOOM_HOLD
    }
    /// When the view is back at 1x.
    pub fn end(&self) -> f64 {
        self.hold_end() + ZOOM_OUT
    }

    /// 0..1: how far zoomed in at `t`.
    fn amount(&self, t: f64) -> f64 {
        if t <= self.start() || t >= self.end() {
            0.0
        } else if t < self.first() {
            smoothstep((t - self.start()) / ZOOM_IN)
        } else if t <= self.hold_end() {
            1.0
        } else {
            1.0 - smoothstep((t - self.hold_end()) / ZOOM_OUT)
        }
    }

    /// The point the view centres on at `t`.
    fn focus(&self, t: f64) -> (f64, f64) {
        let c = &self.clicks;
        let i = c.partition_point(|k| k.0 <= t);
        if i == 0 {
            return (c[0].1, c[0].2);
        }
        let a = c[i - 1];
        let Some(&b) = c.get(i) else { return (a.1, a.2) };
        let pan_start = a.0.max(b.0 - PAN);
        if t <= pan_start || b.0 <= pan_start {
            return (a.1, a.2);
        }
        lerp2((a.1, a.2), (b.1, b.2), smoothstep((t - pan_start) / (b.0 - pan_start)))
    }
}

/// Zoom level and focus over time, planned from a track's clicks.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoomPlan {
    pub factor: f64,
    pub segments: Vec<ZoomSegment>,
}

impl ZoomPlan {
    /// `clicks` must be in time order.
    pub fn new(clicks: impl IntoIterator<Item = (f64, f64, f64)>, factor: f64) -> Self {
        let mut segments: Vec<ZoomSegment> = Vec::new();
        for c in clicks {
            match segments.last_mut() {
                // Still zoomed (or zooming out) when this one would start
                // zooming in: stay in and pan.
                Some(s) if c.0 - ZOOM_IN <= s.end() => s.clicks.push(c),
                _ => segments.push(ZoomSegment { clicks: vec![c] }),
            }
        }
        Self { factor: factor.clamp(MIN_ZOOM, MAX_ZOOM), segments }
    }

    /// Zoom level (1 = whole frame) and the focus point (area points) at `t`.
    pub fn at(&self, t: f64) -> (f64, Option<(f64, f64)>) {
        let i = self.segments.partition_point(|s| s.end() <= t);
        match self.segments.get(i) {
            Some(s) if t > s.start() => (1.0 + (self.factor - 1.0) * s.amount(t), Some(s.focus(t))),
            _ => (1.0, None),
        }
    }
}

/// The pointer path, raw or smoothed.
pub enum CursorPath<'a> {
    Raw(&'a EventTrack),
    Smoothed { t0: f64, points: Vec<(f64, f64)> },
}

impl<'a> CursorPath<'a> {
    /// A smoothed copy of the track's path (see the module docs).
    pub fn smoothed(track: &'a EventTrack) -> Self {
        let Some(first) = track.events.first() else { return CursorPath::Raw(track) };
        let t0 = first.t.min(0.0);
        let t_end = track.duration.max(track.events.last().map(|e| e.t).unwrap_or(0.0)) + 0.5;
        let n = ((t_end - t0) * GRID_HZ).ceil().max(1.0) as usize + 1;
        let at = |i: usize| t0 + i as f64 / GRID_HZ;
        let raw: Vec<(f64, f64)> = (0..n).map(|i| track.position_at(at(i)).unwrap_or((first.x, first.y))).collect();
        let radius = (3.0 * SMOOTH_SIGMA * GRID_HZ).ceil() as isize;
        let kernel: Vec<f64> = (-radius..=radius).map(|k| (-0.5 * (k as f64 / (SMOOTH_SIGMA * GRID_HZ)).powi(2)).exp()).collect();
        let clicks: Vec<f64> = track.clicks().map(|c| c.0).collect();
        let points = (0..n)
            .map(|i| {
                let (mut sx, mut sy, mut sw) = (0.0, 0.0, 0.0);
                for (j, w) in kernel.iter().enumerate() {
                    // Clamp at the ends: the path holds its first/last point.
                    let idx = (i as isize + j as isize - radius).clamp(0, n as isize - 1) as usize;
                    sx += raw[idx].0 * w;
                    sy += raw[idx].1 * w;
                    sw += w;
                }
                let t = at(i);
                let k = clicks.iter().map(|c| (-0.5 * ((t - c) / PIN_SIGMA).powi(2)).exp()).fold(0.0, f64::max);
                lerp2((sx / sw, sy / sw), raw[i], k)
            })
            .collect();
        CursorPath::Smoothed { t0, points }
    }

    /// Position (area points) at `t`, `None` before a raw track's first event.
    pub fn at(&self, t: f64) -> Option<(f64, f64)> {
        match self {
            CursorPath::Raw(track) => track.position_at(t),
            CursorPath::Smoothed { t0, points } => {
                let f = ((t - t0) * GRID_HZ).clamp(0.0, (points.len() - 1) as f64);
                let i = f.floor() as usize;
                let j = (i + 1).min(points.len() - 1);
                Some(lerp2(points[i], points[j], f - i as f64))
            }
        }
    }
}

/// The part of a frame that is shown, in frame pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The rect of a `w` x `h` frame shown at `level`, centred on `focus`
/// (pixels) and kept inside the frame.
pub fn viewport(w: f64, h: f64, level: f64, focus: (f64, f64)) -> Viewport {
    let level = level.max(1.0);
    let (vw, vh) = (w / level, h / level);
    let x = (focus.0 - vw / 2.0).clamp(0.0, w - vw);
    let y = (focus.1 - vh / 2.0).clamp(0.0, h - vh);
    Viewport { x, y, w: vw, h: vh }
}

/// Scale the `vp` part of `src` up to `src`'s size (bilinear).
pub fn crop_scale(src: &RgbaImage, vp: &Viewport) -> RgbaImage {
    let (w, h) = src.dimensions();
    let (sx, sy) = (vp.w / w as f64, vp.h / h as f64);
    // Samples stay inside the viewport, so nothing outside it bleeds in at
    // the edges.
    let (x_lo, y_lo) = (vp.x.max(0.0), vp.y.max(0.0));
    let x_hi = (vp.x + vp.w - 1.0).min(w as f64 - 1.0).max(x_lo);
    let y_hi = (vp.y + vp.h - 1.0).min(h as f64 - 1.0).max(y_lo);
    let raw = src.as_raw();
    let stride = w as usize * 4;
    let mut out = RgbaImage::new(w, h);
    for (oy, row) in out.rows_mut().enumerate() {
        let fy = (vp.y + (oy as f64 + 0.5) * sy - 0.5).clamp(y_lo, y_hi);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(h as usize - 1);
        let ky = fy - y0 as f64;
        for (ox, px) in row.enumerate() {
            let fx = (vp.x + (ox as f64 + 0.5) * sx - 0.5).clamp(x_lo, x_hi);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min(w as usize - 1);
            let kx = fx - x0 as f64;
            for c in 0..4 {
                let p = |x: usize, y: usize| raw[y * stride + x * 4 + c] as f64;
                let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * kx;
                let bot = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * kx;
                px[c] = (top + (bot - top) * ky).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// The pointer: an arrow with its tip at (0, 0), in points.
const ARROW: [(f64, f64); 7] = [(0.0, 0.0), (0.0, 17.0), (4.2, 13.2), (7.0, 19.5), (9.6, 18.4), (6.9, 12.3), (12.0, 12.3)];
/// Width of the arrow's white outline, in points.
const ARROW_OUTLINE: f64 = 1.3;

/// Signed distance from `p` to the arrow polygon (negative inside).
fn arrow_distance(p: (f64, f64)) -> f64 {
    let mut d = f64::INFINITY;
    let mut inside = false;
    for i in 0..ARROW.len() {
        let a = ARROW[i];
        let b = ARROW[(i + 1) % ARROW.len()];
        let (ex, ey) = (b.0 - a.0, b.1 - a.1);
        let (wx, wy) = (p.0 - a.0, p.1 - a.1);
        let k = ((wx * ex + wy * ey) / (ex * ex + ey * ey)).clamp(0.0, 1.0);
        d = d.min(((wx - ex * k).powi(2) + (wy - ey * k).powi(2)).sqrt());
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < a.0 + (p.1 - a.1) / (b.1 - a.1) * ex {
            inside = !inside;
        }
    }
    if inside {
        -d
    } else {
        d
    }
}

/// Draw the pointer with its tip at `tip` (pixels), `scale` pixels per point:
/// a black arrow with a white outline, anti-aliased.
pub fn draw_cursor(img: &mut RgbaImage, tip: (f64, f64), scale: f64) {
    if !(tip.0.is_finite() && tip.1.is_finite() && scale.is_finite() && scale > 0.0) {
        return;
    }
    let (w, h) = (img.width() as f64, img.height() as f64);
    let x0 = (tip.0 - 2.0).floor().max(0.0);
    let y0 = (tip.1 - 2.0).floor().max(0.0);
    let x1 = (tip.0 + 14.0 * scale + 2.0).ceil().min(w);
    let y1 = (tip.1 + 21.0 * scale + 2.0).ceil().min(h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for y in y0 as u32..y1 as u32 {
        for x in x0 as u32..x1 as u32 {
            let p = ((x as f64 + 0.5 - tip.0) / scale, (y as f64 + 0.5 - tip.1) / scale);
            let d = arrow_distance(p) * scale;
            let alpha = (0.5 - d).clamp(0.0, 1.0);
            if alpha <= 0.0 {
                continue;
            }
            // 1 deep inside (black), 0 in the outline (white).
            let ink = (0.5 - (d + ARROW_OUTLINE * scale)).clamp(0.0, 1.0);
            let shade = 255.0 * (1.0 - ink);
            let px = img.get_pixel_mut(x, y);
            for c in 0..3 {
                px[c] = (px[c] as f64 * (1.0 - alpha) + shade * alpha).round() as u8;
            }
            px[3] = 255;
        }
    }
}

/// Everything an edit does with a track, applied frame by frame.
pub struct Effects<'a> {
    pub track: &'a EventTrack,
    pub zoom: Option<ZoomPlan>,
    /// Pointer to draw (`None` when the frames already show it).
    pub cursor: Option<CursorPath<'a>>,
}

impl<'a> Effects<'a> {
    /// `zoom` is the factor (> 1 to zoom); `smooth` draws a smoothed pointer.
    /// A track whose frames lack the pointer always gets it drawn.
    pub fn new(track: &'a EventTrack, zoom: Option<f64>, smooth: bool) -> Self {
        let zoom = zoom.filter(|z| *z > 1.0).map(|z| ZoomPlan::new(track.clicks(), z));
        let cursor = match (track.cursor_in_video, smooth) {
            (true, _) => None,
            (false, true) => Some(CursorPath::smoothed(track)),
            (false, false) => Some(CursorPath::Raw(track)),
        };
        Self { track, zoom, cursor }
    }

    /// Render the frame shown at track time `t`.
    pub fn apply(&self, frame: RgbaImage, t: f64) -> RgbaImage {
        let (w, h) = (frame.width() as f64, frame.height() as f64);
        let (ppx, ppy) = (w / self.track.area.w, h / self.track.area.h);
        let (level, focus) = self.zoom.as_ref().map(|z| z.at(t)).unwrap_or((1.0, None));
        let (mut out, vp) = match focus {
            Some(f) if level > 1.0 + 1e-6 => {
                let vp = viewport(w, h, level, (f.0 * ppx, f.1 * ppy));
                (crop_scale(&frame, &vp), vp)
            }
            _ => (frame, Viewport { x: 0.0, y: 0.0, w, h }),
        };
        if let Some((px, py)) = self.cursor.as_ref().and_then(|c| c.at(t)) {
            let k = w / vp.w;
            draw_cursor(&mut out, ((px * ppx - vp.x) * k, (py * ppy - vp.y) * k), ppx * k);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    use bugle_core::events::{CursorEvent, EventKind, MouseButton};
    use bugle_core::Rect;

    fn click(t: f64, x: f64, y: f64) -> (f64, f64, f64) {
        (t, x, y)
    }

    fn track(events: Vec<CursorEvent>, cursor_in_video: bool) -> EventTrack {
        EventTrack::new(Rect::new(0.0, 0.0, 100.0, 50.0), cursor_in_video, 10.0, events)
    }

    fn down(t: f64, x: f64, y: f64) -> CursorEvent {
        CursorEvent { t, kind: EventKind::Down, button: Some(MouseButton::Left), x, y }
    }

    #[test]
    fn no_clicks_no_zoom() {
        let p = ZoomPlan::new(vec![], 2.0);
        for t in [0.0, 1.0, 100.0] {
            assert_eq!(p.at(t), (1.0, None));
        }
    }

    #[test]
    fn one_click_eases_in_holds_and_eases_out() {
        let p = ZoomPlan::new(vec![click(2.0, 10.0, 20.0)], 2.0);
        assert_eq!(p.segments.len(), 1);
        assert_eq!(p.at(2.0 - ZOOM_IN - 0.01).0, 1.0);
        assert_eq!(p.at(2.0), (2.0, Some((10.0, 20.0))));
        assert_eq!(p.at(2.0 + ZOOM_HOLD).0, 2.0);
        assert_eq!(p.at(2.0 + ZOOM_HOLD + ZOOM_OUT + 0.01), (1.0, None));
        // Monotonic in, and halfway at the midpoint (smoothstep is symmetric).
        let mut last = 1.0;
        for k in 0..=20 {
            let z = p.at(2.0 - ZOOM_IN + ZOOM_IN * k as f64 / 20.0).0;
            assert!(z >= last - 1e-12, "{k}: {z} < {last}");
            last = z;
        }
        assert!((p.at(2.0 - ZOOM_IN / 2.0).0 - 1.5).abs() < 1e-9);
        let out_mid = p.at(2.0 + ZOOM_HOLD + ZOOM_OUT / 2.0).0;
        assert!((out_mid - 1.5).abs() < 1e-9, "{out_mid}");
        // The factor is clamped.
        assert_eq!(ZoomPlan::new(vec![click(1.0, 0.0, 0.0)], 9.0).factor, MAX_ZOOM);
    }

    #[test]
    fn close_clicks_merge_and_pan_far_ones_split() {
        let p = ZoomPlan::new(vec![click(1.0, 10.0, 10.0), click(3.0, 90.0, 40.0), click(20.0, 50.0, 25.0)], 2.0);
        assert_eq!(p.segments.len(), 2);
        assert_eq!(p.segments[0].clicks.len(), 2);
        // Zoomed all the way between the first two clicks.
        for t in [1.0, 2.0, 2.6, 3.0] {
            assert_eq!(p.at(t).0, 2.0, "{t}");
        }
        // Holds on the first click until the pan starts, then arrives on time.
        assert_eq!(p.at(3.0 - PAN).1, Some((10.0, 10.0)));
        let (x, _) = p.at(3.0 - PAN / 2.0).1.unwrap();
        assert!((x - 50.0).abs() < 1e-9, "{x}");
        assert_eq!(p.at(3.0).1, Some((90.0, 40.0)));
        assert_eq!(p.at(10.0), (1.0, None));
        assert_eq!(p.at(20.0), (2.0, Some((50.0, 25.0))));
        // A click just after the previous segment would have zoomed out
        // still joins it if it would start easing in before then.
        let end = 1.0 + ZOOM_HOLD + ZOOM_OUT;
        assert_eq!(ZoomPlan::new(vec![click(1.0, 0.0, 0.0), click(end + ZOOM_IN, 0.0, 0.0)], 2.0).segments.len(), 1);
        assert_eq!(ZoomPlan::new(vec![click(1.0, 0.0, 0.0), click(end + ZOOM_IN + 0.01, 0.0, 0.0)], 2.0).segments.len(), 2);
    }

    #[test]
    fn viewports_keep_aspect_and_stay_inside() {
        assert_eq!(viewport(200.0, 100.0, 1.0, (0.0, 0.0)), Viewport { x: 0.0, y: 0.0, w: 200.0, h: 100.0 });
        assert_eq!(viewport(200.0, 100.0, 2.0, (100.0, 50.0)), Viewport { x: 50.0, y: 25.0, w: 100.0, h: 50.0 });
        assert_eq!(viewport(200.0, 100.0, 2.0, (0.0, 0.0)), Viewport { x: 0.0, y: 0.0, w: 100.0, h: 50.0 });
        assert_eq!(viewport(200.0, 100.0, 4.0, (500.0, 500.0)), Viewport { x: 150.0, y: 75.0, w: 50.0, h: 25.0 });
        // Levels below 1 never show more than the frame.
        assert_eq!(viewport(200.0, 100.0, 0.5, (10.0, 10.0)).w, 200.0);
    }

    #[test]
    fn crop_scale_magnifies_a_quadrant() {
        let mut img = RgbaImage::from_pixel(40, 20, Rgba([0, 0, 255, 255]));
        for y in 0..10 {
            for x in 0..20 {
                img.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            }
        }
        let out = crop_scale(&img, &Viewport { x: 0.0, y: 0.0, w: 20.0, h: 10.0 });
        assert_eq!(out.dimensions(), (40, 20));
        assert!(out.pixels().all(|p| p[0] > 200 && p[2] < 60), "top-left quadrant fills the frame");
        // The identity viewport reproduces the frame.
        assert_eq!(crop_scale(&img, &Viewport { x: 0.0, y: 0.0, w: 40.0, h: 20.0 }), img);
    }

    #[test]
    fn smoothing_removes_jitter_and_keeps_clicks_exact() {
        // A pointer moving right at 100 pt/s with ±3 pt vertical jitter every
        // sample, and a click at 1.0 s.
        let mut ev: Vec<CursorEvent> =
            (0..=120).map(|i| CursorEvent::moved(i as f64 / 60.0, i as f64 * 100.0 / 60.0, 25.0 + if i % 2 == 0 { 3.0 } else { -3.0 })).collect();
        ev.push(down(1.0, 100.0, 28.0));
        ev.sort_by(|a, b| a.t.total_cmp(&b.t));
        let t = track(ev, false);
        let path = CursorPath::smoothed(&t);
        let raw = CursorPath::Raw(&t);
        let dev = |p: &CursorPath, a: f64, b: f64| {
            (0..50).map(|k| a + (b - a) * k as f64 / 50.0).map(|s| (p.at(s).unwrap().1 - 25.0).abs()).fold(0.0, f64::max)
        };
        assert!(dev(&raw, 0.3, 0.7) > 2.0);
        assert!(dev(&path, 0.3, 0.7) < 0.5, "{}", dev(&path, 0.3, 0.7));
        // The horizontal motion is kept (centred filter: no lag).
        let (x, _) = path.at(0.5).unwrap();
        assert!((x - 50.0).abs() < 1.0, "{x}");
        // Exactly on the click when it happens.
        let (x, y) = path.at(1.0).unwrap();
        assert!((x - 100.0).abs() < 1e-6 && (y - 28.0).abs() < 1e-6, "{x},{y}");
        // A resting pointer stays put.
        let still = track(vec![CursorEvent::moved(0.0, 40.0, 10.0)], false);
        let p = CursorPath::smoothed(&still);
        for s in [0.0, 1.0, 9.9, 50.0] {
            let (x, y) = p.at(s).unwrap();
            assert!((x - 40.0).abs() < 1e-9 && (y - 10.0).abs() < 1e-9, "{s}: {x},{y}");
        }
    }

    #[test]
    fn cursor_draws_black_with_white_outline() {
        let mut img = RgbaImage::from_pixel(60, 60, Rgba([128, 128, 128, 255]));
        draw_cursor(&mut img, (10.0, 10.0), 2.0);
        // Deep inside the arrow (3 pt right, 10 pt down of the tip): black.
        assert!(img.get_pixel(10 + 6, 10 + 20)[0] < 30);
        // Just inside the left edge: the white outline.
        assert!(img.get_pixel(10, 10 + 16)[0] > 220, "{:?}", img.get_pixel(10, 26));
        // Outside: untouched.
        assert_eq!(img.get_pixel(5, 5)[0], 128);
        assert_eq!(img.get_pixel(50, 20)[0], 128);
        // Off-frame or degenerate positions draw nothing and do not panic.
        let before = img.clone();
        draw_cursor(&mut img, (-500.0, -500.0), 2.0);
        draw_cursor(&mut img, (f64::NAN, 3.0), 2.0);
        draw_cursor(&mut img, (59.9, 59.9), 0.0);
        assert_eq!(img, before);
    }

    #[test]
    fn effects_zoom_on_the_click_and_draw_the_pointer() {
        // 100x50 pt area, frames at 2 px/pt. Left half red, right half blue;
        // a click on the blue half at 1.0 s.
        let mut frame = RgbaImage::from_pixel(200, 100, Rgba([0, 0, 255, 255]));
        for y in 0..100 {
            for x in 0..100 {
                frame.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            }
        }
        let t = track(vec![CursorEvent::moved(0.0, 75.0, 25.0), down(1.0, 75.0, 25.0)], true);
        let fx = Effects::new(&t, Some(2.0), false);
        assert!(fx.cursor.is_none(), "the frames show the pointer already");
        let out = fx.apply(frame.clone(), 1.0);
        assert!(out.pixels().all(|p| p[2] == 255 && p[0] == 0), "zoomed into the blue half");
        assert_eq!(fx.apply(frame.clone(), 9.0), frame, "zoomed out again");

        // Without the pointer in the frames it is drawn, scaled with the zoom.
        let t = track(vec![CursorEvent::moved(0.0, 75.0, 25.0), down(1.0, 75.0, 25.0)], false);
        let fx = Effects::new(&t, Some(2.0), true);
        let out = fx.apply(frame.clone(), 1.0);
        // The click is centred: tip at (100, 50); 4 px/pt now.
        assert!(out.get_pixel(100 + 12, 50 + 40)[2] < 30, "black arrow body");
        let plain = Effects::new(&t, None, false).apply(frame.clone(), 1.0);
        assert!(plain.get_pixel(150 + 6, 50 + 20)[2] < 30, "drawn at 2 px/pt without zoom");
    }
}
