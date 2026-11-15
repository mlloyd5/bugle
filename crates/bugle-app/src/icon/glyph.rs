//! The camera glyph as a resolution-independent signed distance field.
//!
//! Dependency-free on purpose: the menu-bar icon (`icon.rs`) rasterises it at
//! status-item size, and the packaging task (`xtask`) includes this file by
//! path to draw the application icon at every `.icns` size.

/// The size the glyph was designed at; stroke widths scale from it.
pub const DESIGN_SIZE: f32 = 36.0;

/// Alpha coverage (0–1) of the glyph at `(x, y)` inside a `size`×`size`
/// square, with roughly one pixel of anti-aliasing at that size.
pub fn coverage(x: f32, y: f32, size: f32) -> f32 {
    let s = size;
    let k = size / DESIGN_SIZE;
    // Camera body: rounded rectangle outline.
    let body = sd_round_rect(x - s * 0.5, y - s * 0.56, s * 0.40, s * 0.27, 4.0 * k);
    let body_stroke = body.abs() - 2.4 * k;
    // Viewfinder bump on top of the body.
    let bump = sd_round_rect(x - s * 0.5, y - s * 0.29, s * 0.14, s * 0.07, 2.0 * k);
    let bump_fill = bump.max(-body); // only the part outside the body
    // Lens: ring inside the body.
    let lens = ((x - s * 0.5).powi(2) + (y - s * 0.58).powi(2)).sqrt() - s * 0.14;
    let lens_stroke = lens.abs() - 2.2 * k;
    let d = body_stroke.min(bump_fill).min(lens_stroke);
    (0.5 - d).clamp(0.0, 1.0)
}

/// Signed distance to a rounded rectangle centred at the origin with
/// half-extents `hw`/`hh` and corner radius `r`.
pub fn sd_round_rect(x: f32, y: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = x.abs() - hw + r;
    let qy = y.abs() - hh + r;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}
