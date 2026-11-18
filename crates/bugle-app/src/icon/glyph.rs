//! The horn glyph as a resolution-independent signed distance field: the
//! one-colour version of the application icon's mark (a mouthpiece, a straight
//! tube, one pill-shaped loop of tubing below it and a flared bell).
//!
//! Dependency-free on purpose: the menu-bar icon (`icon.rs`) rasterises it at
//! status-item size, and the packaging task (`xtask`) includes this file by
//! path for its rounded-rect distance field, which masks the application icon.

/// The size the glyph was designed at; every coordinate below is in these
/// units and scales with the requested size.
pub const DESIGN_SIZE: f32 = 36.0;

/// Half the stroke width of the tube and the loop.
const STROKE: f32 = 1.75;
/// The tube's centre line.
const TUBE_Y: f32 = 17.0;
const TUBE_X0: f32 = 4.4;
/// Where the bell starts flaring out of the tube.
const THROAT: (f32, f32) = (17.5, TUBE_Y);
/// Centre of the bell's mouth and its half-width.
const MOUTH: (f32, f32) = (29.0, 11.5);
const MOUTH_HALF: f32 = 8.0;
/// How sharply the bell flares (1 = a straight cone).
const FLARE: f32 = 2.6;
/// Radius rounding the tips of the bell's mouth.
const BELL_ROUND: f32 = 0.9;
/// The loop: a stadium hanging from the tube.
const LOOP_C: (f32, f32) = (15.2, 22.6);
const LOOP_HW: f32 = 7.6;
const LOOP_HH: f32 = 5.6;

/// Alpha coverage (0–1) of the glyph at `(x, y)` inside a `size`×`size`
/// square, with roughly one pixel of anti-aliasing at that size.
pub fn coverage(x: f32, y: f32, size: f32) -> f32 {
    let k = size / DESIGN_SIZE;
    // Work in design units; distances come back in pixels.
    let (u, v) = (x / k, y / k);

    // Mouthpiece: a short upright bar closing the left end of the tube.
    let mouthpiece = sd_segment(u, v, (TUBE_X0, TUBE_Y - 2.6), (TUBE_X0, TUBE_Y + 2.6)) - 1.45;
    // Tube from the mouthpiece into the bell.
    let tube = sd_segment(u, v, (TUBE_X0, TUBE_Y), THROAT) - STROKE;
    // The loop, drawn as an outline whose top runs along the tube.
    let lp = sd_round_rect(u - LOOP_C.0, v - LOOP_C.1, LOOP_HW, LOOP_HH, LOOP_HH).abs() - STROKE;
    let bell = sd_bell(u, v);

    let d = mouthpiece.min(tube).min(lp).min(bell) * k;
    (0.5 - d).clamp(0.0, 1.0)
}

/// Approximate signed distance to the bell: a solid that widens from the tube
/// at the throat to `MOUTH_HALF` at the mouth along `w(t) = w0 + (w1 - w0)·t^FLARE`.
/// The perpendicular excess is divided by the edge's slope so the field stays
/// close to a true distance (good enough for one pixel of anti-aliasing).
fn sd_bell(u: f32, v: f32) -> f32 {
    let (ax, ay) = (MOUTH.0 - THROAT.0, MOUTH.1 - THROAT.1);
    let len = (ax * ax + ay * ay).sqrt();
    let (dx, dy) = (ax / len, ay / len);
    let (px, py) = (u - THROAT.0, v - THROAT.1);
    let along = px * dx + py * dy;
    let across = (px * -dy + py * dx).abs();
    // Build the bell `BELL_ROUND` smaller all round, then grow it back by
    // that much, which rounds the two tips of the mouth.
    let (w0, w1, end) = (STROKE - BELL_ROUND, MOUTH_HALF - BELL_ROUND, len - BELL_ROUND);
    let t = (along / end).clamp(0.0, 1.0);
    let w = w0 + (w1 - w0) * t.powf(FLARE);
    let slope = (w1 - w0) * FLARE * t.powf(FLARE - 1.0) / end;
    let side = (across - w) / (1.0 + slope * slope).sqrt();
    // Flat mouth at the far end and a flat cut at the throat (hidden in the
    // tube), combined like the sides and ends of a rounded rectangle.
    let ends = (along - end).max(-along);
    let outside = (side.max(0.0).powi(2) + ends.max(0.0).powi(2)).sqrt();
    outside + side.max(ends).min(0.0) - BELL_ROUND
}

/// Distance from `(x, y)` to the segment `a`–`b`.
fn sd_segment(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (px, py) = (x - a.0, y - a.1);
    let (bx, by) = (b.0 - a.0, b.1 - a.1);
    let h = ((px * bx + py * by) / (bx * bx + by * by)).clamp(0.0, 1.0);
    ((px - bx * h).powi(2) + (py - by * h).powi(2)).sqrt()
}

/// Signed distance to a rounded rectangle centred at the origin with
/// half-extents `hw`/`hh` and corner radius `r`.
pub fn sd_round_rect(x: f32, y: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = x.abs() - hw + r;
    let qy = y.abs() - hh + r;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}
