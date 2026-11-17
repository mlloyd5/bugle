//! Raster renderer for [`Annotation`] documents.
//!
//! Shapes are drawn with signed-distance coverage so strokes are anti-aliased
//! without pulling in a full vector library. Text goes through `ab_glyph`.

use crate::color::Color;
use crate::model::*;
use ab_glyph::{FontVec, PxScale};
use anyhow::{anyhow, Context, Result};
use bugle_core::{Point, Rect};
use image::{imageops, GenericImageView, Rgba, RgbaImage};
use imageproc::drawing::{draw_text_mut, text_size, Blend};
use std::path::Path;

const FONT_CANDIDATES: &[(&str, u32)] = &[
    ("/System/Library/Fonts/Supplemental/Arial Bold.ttf", 0),
    ("/System/Library/Fonts/Supplemental/Arial.ttf", 0),
    ("/System/Library/Fonts/Helvetica.ttc", 1),
    ("/System/Library/Fonts/Helvetica.ttc", 0),
    ("/System/Library/Fonts/Geneva.ttf", 0),
    ("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 0),
];

/// Rendering defaults (filled from `MarkupConfig`).
pub struct Renderer {
    pub font: FontVec,
    pub defaults: Style,
}

impl Renderer {
    pub fn new(font_path: Option<&Path>, defaults: Style) -> Result<Self> {
        let font = load_font(font_path)?;
        Ok(Self { font, defaults })
    }

    pub fn with_defaults() -> Result<Self> {
        Self::new(
            None,
            Style { color: Some(Color::RED), stroke_width: Some(4.0), font_size: Some(28.0), fill: None, opacity: None },
        )
    }

    /// Render `doc` over `src`. The document is checked first with
    /// [`Annotation::validate`] and [`Annotation::output_size`]; violations
    /// are [`bugle_core::BugleError::Invalid`] errors naming the field.
    pub fn render(&self, src: &RgbaImage, doc: &Annotation) -> Result<RgbaImage> {
        doc.validate()?;
        let mut img = match doc.crop {
            Some(r) => crop(src, r)?,
            None => src.clone(),
        };
        doc.output_size(img.width(), img.height())?;
        let base = doc.style.merged(&self.defaults);
        let mut counter = 0u32;
        for shape in &doc.shapes {
            self.draw_shape(&mut img, shape, &base, &mut counter)?;
        }
        if let Some(canvas) = &doc.canvas {
            img = apply_canvas(&img, canvas)?;
        }
        if let Some(s) = doc.scale {
            if s > 0.0 && (s - 1.0).abs() > 1e-3 {
                let (w, h) =
                    scaled_size(img.width(), img.height(), s).ok_or_else(|| invalid("scale", "output image is too large"))?;
                check_pixels("scale", w, h)?;
                img = imageops::resize(&img, w, h, imageops::FilterType::Lanczos3);
            }
        }
        Ok(img)
    }

    fn draw_shape(&self, img: &mut RgbaImage, shape: &Shape, base: &Style, counter: &mut u32) -> Result<()> {
        match shape {
            Shape::Rectangle { rect, radius, style } => {
                let st = style.merged(base);
                if let Some(fill) = st.fill {
                    fill_coverage(img, expand(rect, 1.0), fill.with_alpha(apply_opacity(fill, &st)), |x, y| {
                        coverage(sdf_round_rect(x, y, rect, *radius))
                    });
                }
                let w = st.stroke_width.unwrap_or(4.0) as f64;
                let c = color_of(&st);
                fill_coverage(img, expand(rect, w), c, |x, y| coverage(sdf_round_rect(x, y, rect, *radius).abs() - w / 2.0));
            }
            Shape::Ellipse { rect, style } => {
                let st = style.merged(base);
                if let Some(fill) = st.fill {
                    fill_coverage(img, expand(rect, 1.0), fill.with_alpha(apply_opacity(fill, &st)), |x, y| {
                        coverage(sdf_ellipse(x, y, rect))
                    });
                }
                let w = st.stroke_width.unwrap_or(4.0) as f64;
                fill_coverage(img, expand(rect, w), color_of(&st), |x, y| coverage(sdf_ellipse(x, y, rect).abs() - w / 2.0));
            }
            Shape::Line { from, to, style } => {
                let st = style.merged(base);
                stroke_segment(img, *from, *to, st.stroke_width.unwrap_or(4.0), color_of(&st));
            }
            Shape::Arrow { from, to, arrow, style } => {
                let st = style.merged(base);
                draw_arrow(img, *from, *to, *arrow, st.stroke_width.unwrap_or(4.0), color_of(&st));
            }
            Shape::Freehand { points, style } => {
                let st = style.merged(base);
                let w = st.stroke_width.unwrap_or(4.0);
                let c = color_of(&st);
                for pair in points.windows(2) {
                    stroke_segment(img, pair[0], pair[1], w, c);
                }
                let w = w as f64;
                if points.len() == 1 {
                    fill_coverage(img, Rect::new(points[0].x - w, points[0].y - w, 2.0 * w, 2.0 * w), c, |x, y| {
                        coverage(((x - points[0].x).powi(2) + (y - points[0].y).powi(2)).sqrt() - w / 2.0)
                    });
                }
            }
            Shape::Text { at, text, background, style } => {
                let st = style.merged(base);
                let size = st.font_size.unwrap_or(28.0);
                let c = color_of(&st);
                self.draw_text(img, *at, text, size, c, *background);
            }
            Shape::Highlight { rect, style } => {
                let st = style.merged(base);
                let c = st.color.unwrap_or(Color::YELLOW);
                multiply_rect(img, rect, c, apply_opacity(c, &st));
            }
            Shape::Counter { at, number, style } => {
                let st = style.merged(base);
                let n = match number {
                    Some(n) => {
                        *counter = *n;
                        *n
                    }
                    None => {
                        *counter = counter.saturating_add(1);
                        *counter
                    }
                };
                let size = st.font_size.unwrap_or(28.0);
                let r = (size * 0.75) as f64;
                let c = color_of(&st);
                fill_coverage(img, Rect::new(at.x - r - 1.0, at.y - r - 1.0, 2.0 * r + 2.0, 2.0 * r + 2.0), c, |x, y| {
                    coverage(((x - at.x).powi(2) + (y - at.y).powi(2)).sqrt() - r)
                });
                let label = n.to_string();
                let text_c = if c.luminance() > 0.6 { Color::BLACK } else { Color::WHITE };
                let (tw, th) = text_size(PxScale::from(size), &self.font, &label);
                let mut canvas = Blend(std::mem::take(img));
                draw_text_mut(
                    &mut canvas,
                    text_c.rgba(),
                    (at.x - tw as f64 / 2.0).round() as i32,
                    (at.y - th as f64 / 2.0).round() as i32,
                    PxScale::from(size),
                    &self.font,
                    &label,
                );
                *img = canvas.0;
            }
            Shape::Redact { rect, mode, strength } => redact(img, rect, *mode, *strength),
            Shape::Spotlight { rect, dim } => spotlight(img, rect, *dim),
            Shape::Image { at, path, scale } => {
                // Check the header before decoding so a huge file is never
                // fully loaded.
                let (iw, ih) = image::image_dimensions(path).with_context(|| format!("loading {path}"))?;
                check_pixels("image.path", iw, ih)?;
                let overlay = image::open(path).with_context(|| format!("loading {path}"))?.to_rgba8();
                let overlay = if (*scale - 1.0).abs() > 1e-3 && *scale > 0.0 {
                    let w = ((overlay.width() as f64) * *scale as f64).max(1.0);
                    let h = ((overlay.height() as f64) * *scale as f64).max(1.0);
                    if !(w * h).is_finite() || w * h > limits::MAX_PIXELS as f64 {
                        return Err(invalid("image.scale", "scaled image is too large"));
                    }
                    imageops::resize(&overlay, w as u32, h as u32, imageops::FilterType::Lanczos3)
                } else {
                    overlay
                };
                imageops::overlay(img, &overlay, at.x.round() as i64, at.y.round() as i64);
            }
        }
        Ok(())
    }

    fn draw_text(&self, img: &mut RgbaImage, at: Point, text: &str, size: f32, color: Color, background: Option<Color>) {
        let scale = PxScale::from(size);
        let line_h = (size * 1.25).round() as f64;
        let lines = text_lines(text);
        let widths: Vec<u32> = lines.iter().map(|l| text_size(scale, &self.font, l).0).collect();
        let max_w = widths.iter().copied().max().unwrap_or(0) as f64;
        let total_h = line_h * lines.len().max(1) as f64;
        if let Some(bg) = background {
            let pad = (size * 0.35) as f64;
            let r = Rect::new(at.x - pad, at.y - pad * 0.5, max_w + 2.0 * pad, total_h + pad);
            let radius = (size * 0.3).max(2.0);
            fill_coverage(img, expand(&r, 1.0), bg, |x, y| coverage(sdf_round_rect(x, y, &r, radius)));
        }
        let mut canvas = Blend(std::mem::take(img));
        for (i, line) in lines.iter().enumerate() {
            draw_text_mut(
                &mut canvas,
                color.rgba(),
                at.x.round() as i32,
                (at.y + i as f64 * line_h).round() as i32,
                scale,
                &self.font,
                line,
            );
        }
        *img = canvas.0;
    }
}

/// Convenience: render with built-in defaults.
pub fn render(src: &RgbaImage, doc: &Annotation) -> Result<RgbaImage> {
    Renderer::with_defaults()?.render(src, doc)
}

pub fn load_font(path: Option<&Path>) -> Result<FontVec> {
    if let Some(p) = path {
        let bytes = std::fs::read(p).with_context(|| format!("reading font {}", p.display()))?;
        return FontVec::try_from_vec(bytes).map_err(|e| anyhow!("bad font {}: {e}", p.display()));
    }
    for (p, idx) in FONT_CANDIDATES {
        if let Ok(bytes) = std::fs::read(p) {
            if let Ok(f) = FontVec::try_from_vec_and_index(bytes, *idx) {
                return Ok(f);
            }
        }
    }
    Err(anyhow!("no usable font found; set font_path in the [markup] section of the config"))
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

fn expand(r: &Rect, by: f64) -> Rect {
    Rect::new(r.x - by - 1.0, r.y - by - 1.0, r.w + 2.0 * by + 2.0, r.h + 2.0 * by + 2.0)
}

fn color_of(st: &Style) -> Color {
    let c = st.color.unwrap_or(Color::RED);
    c.with_alpha(apply_opacity(c, st))
}

fn apply_opacity(c: Color, st: &Style) -> u8 {
    match st.opacity {
        Some(o) => ((c.0[3] as u32 * o as u32) / 255) as u8,
        None => c.0[3],
    }
}

/// Signed distance to a rounded rectangle (negative inside).
fn sdf_round_rect(px: f64, py: f64, r: &Rect, radius: f32) -> f64 {
    let rad = (radius as f64).min(r.w / 2.0).min(r.h / 2.0).max(0.0);
    let cx = r.x + r.w / 2.0;
    let cy = r.y + r.h / 2.0;
    let hx = r.w / 2.0 - rad;
    let hy = r.h / 2.0 - rad;
    let dx = (px - cx).abs() - hx;
    let dy = (py - cy).abs() - hy;
    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    let inside = dx.max(dy).min(0.0);
    outside + inside - rad
}

/// Signed distance to the ellipse inscribed in `r` (negative inside).
///
/// Finds the closest point on the ellipse with a few Newton-like steps on
/// the curve's evolute, working in the first quadrant. Converges to well under
/// a hundredth of a pixel for any aspect ratio, and is negative everywhere in
/// the interior, including the exact centre.
fn sdf_ellipse(px: f64, py: f64, r: &Rect) -> f64 {
    let a = (r.w / 2.0).max(0.5);
    let b = (r.h / 2.0).max(0.5);
    let qx = (px - (r.x + a)).abs();
    let qy = (py - (r.y + b)).abs();
    let (mut tx, mut ty) = (std::f64::consts::FRAC_1_SQRT_2, std::f64::consts::FRAC_1_SQRT_2);
    for _ in 0..4 {
        let (x, y) = (a * tx, b * ty);
        // Centre of curvature of the ellipse at (x, y).
        let ex = (a * a - b * b) * tx.powi(3) / a;
        let ey = (b * b - a * a) * ty.powi(3) / b;
        let (rx, ry) = (x - ex, y - ey);
        let (dx, dy) = (qx - ex, qy - ey);
        let rlen = rx.hypot(ry);
        let dlen = dx.hypot(dy);
        if dlen < 1e-9 {
            // The point sits on the centre of curvature (the centre of a
            // circle): every direction is equally close, keep the current one.
            break;
        }
        let nx = ((dx * rlen / dlen + ex) / a).clamp(0.0, 1.0);
        let ny = ((dy * rlen / dlen + ey) / b).clamp(0.0, 1.0);
        let t = nx.hypot(ny);
        if t < 1e-12 {
            break;
        }
        tx = nx / t;
        ty = ny / t;
    }
    let dist = (qx - a * tx).hypot(qy - b * ty);
    if (qx / a).powi(2) + (qy / b).powi(2) < 1.0 {
        -dist
    } else {
        dist
    }
}

fn sdf_segment(px: f64, py: f64, a: Point, b: Point) -> f64 {
    let abx = b.x - a.x;
    let aby = b.y - a.y;
    let apx = px - a.x;
    let apy = py - a.y;
    let len2 = abx * abx + aby * aby;
    let t = if len2 <= 0.0 { 0.0 } else { ((apx * abx + apy * aby) / len2).clamp(0.0, 1.0) };
    let qx = a.x + t * abx - px;
    let qy = a.y + t * aby - py;
    (qx * qx + qy * qy).sqrt()
}

fn sdf_triangle(px: f64, py: f64, p: [Point; 3]) -> f64 {
    let mut d = f64::MAX;
    let mut sign = 1.0;
    let mut inside = true;
    for i in 0..3 {
        let a = p[i];
        let b = p[(i + 1) % 3];
        d = d.min(sdf_segment(px, py, a, b));
        let cross = (b.x - a.x) * (py - a.y) - (b.y - a.y) * (px - a.x);
        if cross < 0.0 {
            inside = false;
        }
    }
    // Handle either winding.
    if !inside {
        let mut inside_rev = true;
        for i in 0..3 {
            let a = p[i];
            let b = p[(i + 1) % 3];
            let cross = (b.x - a.x) * (py - a.y) - (b.y - a.y) * (px - a.x);
            if cross > 0.0 {
                inside_rev = false;
            }
        }
        if inside_rev {
            inside = true;
        }
    }
    if inside {
        sign = -1.0;
    }
    sign * d
}

fn coverage(d: f64) -> f32 {
    (0.5 - d).clamp(0.0, 1.0) as f32
}

fn blend(dst: &mut Rgba<u8>, c: Color, cov: f32) {
    let a = (c.0[3] as f32 / 255.0) * cov;
    if a <= 0.0 {
        return;
    }
    for i in 0..3 {
        let d = dst.0[i] as f32;
        let s = c.0[i] as f32;
        dst.0[i] = (d + (s - d) * a).round().clamp(0.0, 255.0) as u8;
    }
    let da = dst.0[3] as f32 / 255.0;
    dst.0[3] = ((a + da * (1.0 - a)) * 255.0).round().clamp(0.0, 255.0) as u8;
}

/// Blend `color` into every pixel of `bbox` with the coverage returned by `f`.
fn fill_coverage(img: &mut RgbaImage, bbox: Rect, color: Color, f: impl Fn(f64, f64) -> f32) {
    let x0 = bbox.x.floor().max(0.0) as u32;
    let y0 = bbox.y.floor().max(0.0) as u32;
    let x1 = (bbox.right().ceil().max(0.0) as u32).min(img.width());
    let y1 = (bbox.bottom().ceil().max(0.0) as u32).min(img.height());
    for y in y0..y1 {
        for x in x0..x1 {
            let cov = f(x as f64 + 0.5, y as f64 + 0.5);
            if cov > 0.0 {
                blend(img.get_pixel_mut(x, y), color, cov);
            }
        }
    }
}

fn stroke_segment(img: &mut RgbaImage, a: Point, b: Point, width: f32, color: Color) {
    let w = width as f64;
    let bbox = Rect::new(a.x.min(b.x) - w, a.y.min(b.y) - w, (a.x - b.x).abs() + 2.0 * w, (a.y - b.y).abs() + 2.0 * w);
    fill_coverage(img, bbox, color, |x, y| coverage(sdf_segment(x, y, a, b) - w / 2.0));
}

fn draw_arrow(img: &mut RgbaImage, from: Point, to: Point, style: ArrowStyle, width: f32, color: Color) {
    let w = width as f64;
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let (nx, ny) = (-uy, ux);
    let head_len = (w * 4.0).clamp(10.0, len * 0.5);
    let head_w = match style {
        ArrowStyle::Slim => w * 2.5,
        ArrowStyle::Fancy => w * 4.0,
        _ => w * 3.0,
    };
    let shaft_end = Point { x: to.x - ux * head_len * 0.8, y: to.y - uy * head_len * 0.8 };
    let shaft_start = if style == ArrowStyle::Double {
        Point { x: from.x + ux * head_len * 0.8, y: from.y + uy * head_len * 0.8 }
    } else {
        from
    };
    let bbox = Rect::new(
        from.x.min(to.x) - head_w - w,
        from.y.min(to.y) - head_w - w,
        (dx).abs() + 2.0 * (head_w + w),
        (dy).abs() + 2.0 * (head_w + w),
    );
    let head = |tip: Point, dir: (f64, f64)| -> [Point; 3] {
        let base = Point { x: tip.x - dir.0 * head_len, y: tip.y - dir.1 * head_len };
        [
            tip,
            Point { x: base.x + nx * head_w / 2.0, y: base.y + ny * head_w / 2.0 },
            Point { x: base.x - nx * head_w / 2.0, y: base.y - ny * head_w / 2.0 },
        ]
    };
    let head_a = head(to, (ux, uy));
    let head_b = head(from, (-ux, -uy));
    let fancy = style == ArrowStyle::Fancy;
    let slim_w = if style == ArrowStyle::Slim { w * 0.6 } else { w };
    fill_coverage(img, bbox, color, |x, y| {
        // Shaft: optionally tapered (fancy) from w/4 at the tail to w at the head.
        let shaft_w = if fancy {
            let t = (((x - from.x) * ux + (y - from.y) * uy) / len).clamp(0.0, 1.0);
            slim_w * (0.25 + 0.75 * t)
        } else {
            slim_w
        };
        let mut d = sdf_segment(x, y, shaft_start, shaft_end) - shaft_w / 2.0;
        d = d.min(sdf_triangle(x, y, head_a));
        if style == ArrowStyle::Double {
            d = d.min(sdf_triangle(x, y, head_b));
        }
        coverage(d)
    });
}

fn clamp_rect(img: &RgbaImage, r: &Rect) -> Option<(u32, u32, u32, u32)> {
    let x0 = r.x.floor().max(0.0) as u32;
    let y0 = r.y.floor().max(0.0) as u32;
    let x1 = (r.right().ceil().max(0.0) as u32).min(img.width());
    let y1 = (r.bottom().ceil().max(0.0) as u32).min(img.height());
    // Lazily: a rect starting past the edge has x0 > x1.
    (x1 > x0 && y1 > y0).then(|| (x0, y0, x1 - x0, y1 - y0))
}

/// Multiply the pixels under `r` by `c`, mixed in with strength `alpha`
/// (0 leaves the pixels alone, 255 is a full multiply).
fn multiply_rect(img: &mut RgbaImage, r: &Rect, c: Color, alpha: u8) {
    let Some((x0, y0, w, h)) = clamp_rect(img, r) else { return };
    let a = alpha as u32;
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            let p = img.get_pixel_mut(x, y);
            for i in 0..3 {
                let d = p.0[i] as u32;
                let m = (d * c.0[i] as u32) / 255;
                p.0[i] = ((d * (255 - a) + m * a + 127) / 255) as u8;
            }
        }
    }
}

fn spotlight(img: &mut RgbaImage, r: &Rect, dim: f32) {
    let keep = 1.0 - dim.clamp(0.0, 1.0);
    let (w, h) = (img.width(), img.height());
    for y in 0..h {
        for x in 0..w {
            if r.contains(Point { x: x as f64 + 0.5, y: y as f64 + 0.5 }) {
                continue;
            }
            let p = img.get_pixel_mut(x, y);
            for i in 0..3 {
                p.0[i] = (p.0[i] as f32 * keep) as u8;
            }
        }
    }
}

fn pixelate_region(img: &mut RgbaImage, x0: u32, y0: u32, w: u32, h: u32, block: u32) {
    let block = block.max(2);
    let mut by = y0;
    while by < y0 + h {
        let bh = block.min(y0 + h - by);
        let mut bx = x0;
        while bx < x0 + w {
            let bw = block.min(x0 + w - bx);
            let mut sum = [0u64; 4];
            for y in by..by + bh {
                for x in bx..bx + bw {
                    let p = img.get_pixel(x, y);
                    for (s, v) in sum.iter_mut().zip(p.0) {
                        *s += v as u64;
                    }
                }
            }
            let n = bw as u64 * bh as u64;
            let avg = Rgba([(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, (sum[3] / n) as u8]);
            for y in by..by + bh {
                for x in bx..bx + bw {
                    img.put_pixel(x, y, avg);
                }
            }
            bx += bw;
        }
        by += bh;
    }
}

fn blur_region(img: &mut RgbaImage, x0: u32, y0: u32, w: u32, h: u32, sigma: f32) {
    // Blur a padded crop so edges do not darken, then paste the centre back.
    // Saturating arithmetic: callers clamp the region to the image, but the
    // pad comes from a caller-supplied sigma.
    let sigma = if sigma.is_finite() { sigma.clamp(0.1, limits::MAX_BLUR_SIGMA) } else { 1.0 };
    let pad = (sigma * 3.0).ceil() as u32;
    let px0 = x0.saturating_sub(pad);
    let py0 = y0.saturating_sub(pad);
    let px1 = x0.saturating_add(w).saturating_add(pad).min(img.width());
    let py1 = y0.saturating_add(h).saturating_add(pad).min(img.height());
    let sub = img.view(px0, py0, px1 - px0, py1 - py0).to_image();
    let blurred = imageops::blur(&sub, sigma);
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            img.put_pixel(x, y, *blurred.get_pixel(x - px0, y - py0));
        }
    }
}

fn redact(img: &mut RgbaImage, r: &Rect, mode: RedactStyle, strength: f32) {
    let Some((x0, y0, w, h)) = clamp_rect(img, r) else { return };
    let auto_block = ((w.min(h) as f32) / 8.0).clamp(6.0, 24.0);
    match mode {
        RedactStyle::Pixelate => pixelate_region(img, x0, y0, w, h, if strength > 0.0 { strength as u32 } else { auto_block as u32 }),
        RedactStyle::Blur => blur_region(img, x0, y0, w, h, if strength > 0.0 { strength } else { auto_block / 1.5 }),
        RedactStyle::BlackOut => {
            for y in y0..y0 + h {
                for x in x0..x0 + w {
                    img.put_pixel(x, y, Rgba([0, 0, 0, 255]));
                }
            }
        }
        RedactStyle::SecureBlur => {
            // Destroy high-frequency detail twice over so de-pixelation attacks
            // have nothing to recover ("secure blur").
            pixelate_region(img, x0, y0, w, h, (auto_block * 1.5) as u32);
            blur_region(img, x0, y0, w, h, auto_block);
            pixelate_region(img, x0, y0, w, h, (auto_block * 1.2) as u32);
        }
    }
}

fn crop(src: &RgbaImage, r: Rect) -> Result<RgbaImage> {
    let (x0, y0, w, h) = clamp_rect(src, &r).ok_or_else(|| anyhow!("crop rect {r} is outside the image"))?;
    Ok(src.view(x0, y0, w, h).to_image())
}

// ---------------------------------------------------------------------------
// Canvas: padding, gradient background, rounded corners, drop shadow
// ---------------------------------------------------------------------------

fn apply_canvas(img: &RgbaImage, canvas: &Canvas) -> Result<RgbaImage> {
    let pad = canvas.padding;
    let bw = canvas.border_width;
    let (iw, ih) = (img.width(), img.height());
    // Checked even though `validate` bounds padding and border: this is the
    // allocation size.
    let (ow, oh) = canvas_size(iw, ih, canvas)?;
    check_pixels("canvas", ow, oh)?;
    let mut out = RgbaImage::from_pixel(ow, oh, Rgba([0, 0, 0, 0]));

    if let Some((from, to, angle)) = canvas.background.resolve() {
        let a = angle.to_radians() as f64;
        let (dx, dy) = (a.sin(), -a.cos());
        let span = (ow as f64 * dx.abs()) + (oh as f64 * dy.abs());
        for y in 0..oh {
            for x in 0..ow {
                let proj = (x as f64 - ow as f64 / 2.0) * dx + (y as f64 - oh as f64 / 2.0) * dy;
                let t = ((proj / span) + 0.5).clamp(0.0, 1.0) as f32;
                let p = std::array::from_fn(|i| (from.0[i] as f32 + (to.0[i] as f32 - from.0[i] as f32) * t).round() as u8);
                out.put_pixel(x, y, Rgba(p));
            }
        }
    }

    // `canvas_size` succeeded, so `pad + bw` and `iw + 2 * bw` fit in u32;
    // the geometry below is in f64 anyway.
    let inset = pad as f64 + bw as f64;
    let inner = Rect::new(inset, inset, iw as f64, ih as f64);
    let radius = canvas.corner_radius as f32;

    if canvas.shadow && pad > 0 {
        let sigma = (pad as f32 / 3.0).clamp(4.0, 28.0);
        let offset_y = (pad as f64 / 4.0).min(24.0);
        let shadow_rect = Rect::new(inner.x, inner.y + offset_y, inner.w, inner.h);
        // A one-channel mask, blurred in time independent of sigma: this is
        // the costliest step on a large capture.
        let (w, h) = (ow as usize, oh as usize);
        let mut mask = vec![0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                mask[y * w + x] = coverage(sdf_round_rect(x as f64 + 0.5, y as f64 + 0.5, &shadow_rect, radius)) * (140.0 / 255.0);
            }
        }
        blur_mask(&mut mask, w, h, sigma);
        for (i, m) in mask.iter().enumerate() {
            if *m > 0.5 / 255.0 {
                blend(out.get_pixel_mut((i % w) as u32, (i / w) as u32), Color([0, 0, 0, 255]), *m);
            }
        }
    }

    if let (Some(bc), true) = (canvas.border, bw > 0) {
        let br = Rect::new(pad as f64, pad as f64, iw as f64 + 2.0 * bw as f64, ih as f64 + 2.0 * bw as f64);
        fill_coverage(&mut out, expand(&br, 1.0), bc, |x, y| coverage(sdf_round_rect(x, y, &br, radius + bw as f32)));
    }

    // Paste the screenshot with rounded-corner alpha.
    for y in 0..ih {
        for x in 0..iw {
            let px = inner.x + x as f64 + 0.5;
            let py = inner.y + y as f64 + 0.5;
            let cov = coverage(sdf_round_rect(px, py, &inner, radius));
            if cov <= 0.0 {
                continue;
            }
            let s = img.get_pixel(x, y);
            blend(out.get_pixel_mut(inner.x as u32 + x, inner.y as u32 + y), Color(s.0), cov);
        }
    }
    Ok(out)
}

/// Box widths for three box blurs that together approximate a Gaussian of
/// `sigma` (each odd, so the boxes are centred).
fn box_sizes(sigma: f32) -> [usize; 3] {
    let n = 3.0;
    let ideal = ((12.0 * sigma * sigma / n) + 1.0).sqrt();
    let mut lo = ideal.floor() as i64;
    if lo % 2 == 0 {
        lo -= 1;
    }
    let lo = lo.max(1);
    let hi = lo + 2;
    let l = lo as f32;
    let m = ((12.0 * sigma * sigma - n * l * l - 4.0 * n * l - 3.0 * n) / (-4.0 * l - 4.0)).round() as i64;
    std::array::from_fn(|i| if (i as i64) < m { lo as usize } else { hi as usize })
}

/// Blur a one-channel `w` x `h` mask in place with three box blurs per axis
/// (running sums: the cost does not grow with `sigma`). Outside the mask
/// counts as zero.
fn blur_mask(mask: &mut [f32], w: usize, h: usize, sigma: f32) {
    if w == 0 || h == 0 || sigma.is_nan() || sigma <= 0.0 {
        return;
    }
    let mut line = Vec::new();
    for size in box_sizes(sigma) {
        let r = size / 2;
        // Rows, then columns.
        for (len, count, stride, step) in [(w, h, w, 1), (h, w, 1, w)] {
            line.resize(len, 0.0);
            for k in 0..count {
                let at = |i: usize| k * stride + i * step;
                for (i, v) in line.iter_mut().enumerate() {
                    *v = mask[at(i)];
                }
                // Running sum over [i - r, i + r], computed in f64 so long
                // lines do not drift.
                let mut sum: f64 = line[..r.min(len)].iter().map(|v| *v as f64).sum();
                for i in 0..len {
                    if i + r < len {
                        sum += line[i + r] as f64;
                    }
                    if i > r {
                        sum -= line[i - r - 1] as f64;
                    }
                    mask[at(i)] = (sum / size as f64) as f32;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> RgbaImage {
        RgbaImage::from_pixel(200, 120, Rgba([240, 240, 240, 255]))
    }

    #[test]
    fn renders_every_shape() {
        let doc: Annotation = serde_json::from_value(serde_json::json!({
            "shapes": [
                {"type":"rectangle","rect":{"x":10,"y":10,"w":60,"h":40},"radius":6},
                {"type":"ellipse","rect":{"x":80,"y":10,"w":60,"h":40},"fill":"#00FF0080"},
                {"type":"line","from":{"x":0,"y":0},"to":{"x":199,"y":119}},
                {"type":"arrow","from":{"x":20,"y":100},"to":{"x":180,"y":60},"arrow":"fancy"},
                {"type":"arrow","from":{"x":20,"y":110},"to":{"x":180,"y":110},"arrow":"double"},
                {"type":"freehand","points":[{"x":5,"y":5},{"x":30,"y":40},{"x":60,"y":20}]},
                {"type":"text","at":{"x":100,"y":70},"text":"Hi\nthere","background":"#000000AA","color":"white","font_size":16},
                {"type":"highlight","rect":{"x":0,"y":50,"w":100,"h":20}},
                {"type":"counter","at":{"x":150,"y":30}},
                {"type":"counter","at":{"x":170,"y":30}},
                {"type":"redact","rect":{"x":120,"y":80,"w":60,"h":30},"mode":"secure_blur"},
                {"type":"spotlight","rect":{"x":50,"y":50,"w":100,"h":50},"dim":0.5}
            ],
            "canvas": {"padding": 24, "background": "sunset", "corner_radius": 10, "shadow": true}
        }))
        .unwrap();
        let out = render(&base(), &doc).unwrap();
        assert_eq!(out.width(), 200 + 48);
        assert_eq!(out.height(), 120 + 48);
        // Something was drawn on the padded background.
        let corner = out.get_pixel(2, 2);
        assert!(corner.0[3] == 255);
    }

    #[test]
    fn ellipse_fill_stays_inside() {
        let doc: Annotation = serde_json::from_value(serde_json::json!({
            "shapes": [{"type":"ellipse","rect":{"x":20,"y":20,"w":100,"h":60},"fill":"#0000FF","color":"#0000FF","stroke_width":1}]
        }))
        .unwrap();
        let out = render(&base(), &doc).unwrap();
        assert_eq!(*out.get_pixel(70, 50), Rgba([0, 0, 255, 255])); // centre
        assert_eq!(*out.get_pixel(22, 22), Rgba([240, 240, 240, 255])); // bbox corner untouched
    }

    #[test]
    fn odd_sized_ellipse_centre_pixel() {
        // 61x41 at (20,20): the centre of pixel (50,40) is the exact ellipse centre.
        let filled: Annotation = serde_json::from_value(serde_json::json!({
            "shapes": [{"type":"ellipse","rect":{"x":20,"y":20,"w":61,"h":41},"fill":"#0000FF","color":"#FF0000","stroke_width":2}]
        }))
        .unwrap();
        let out = render(&base(), &filled).unwrap();
        assert_eq!(*out.get_pixel(50, 40), Rgba([0, 0, 255, 255]), "filled ellipse centre");

        let stroked: Annotation = serde_json::from_value(serde_json::json!({
            "shapes": [{"type":"ellipse","rect":{"x":20,"y":20,"w":61,"h":41},"color":"#FF0000","stroke_width":2}]
        }))
        .unwrap();
        let out = render(&base(), &stroked).unwrap();
        assert_eq!(*out.get_pixel(50, 40), Rgba([240, 240, 240, 255]), "stroked ellipse centre");
    }

    #[test]
    fn ellipse_distance_is_negative_inside() {
        for &(w, h) in &[(61.0, 41.0), (200.0, 6.0), (7.0, 151.0), (50.0, 50.0), (1.0, 1.0)] {
            let r = Rect::new(0.0, 0.0, w, h);
            let (rx, ry) = (w / 2.0, h / 2.0);
            for i in 0..=40 {
                for j in 0..=40 {
                    let x = w * i as f64 / 40.0;
                    let y = h * j as f64 / 40.0;
                    let k = ((x - rx) / rx).powi(2) + ((y - ry) / ry).powi(2);
                    let d = sdf_ellipse(x, y, &r);
                    if k < 0.999 {
                        assert!(d < 0.0, "{w}x{h} at ({x},{y}): {d}");
                    } else if k > 1.001 {
                        assert!(d > 0.0, "{w}x{h} at ({x},{y}): {d}");
                    }
                }
            }
            // Centre is as deep as the short semi-axis.
            assert!((sdf_ellipse(rx, ry, &r) + rx.min(ry)).abs() < 1e-6);
        }
        // Exact distances on the axes.
        let r = Rect::new(0.0, 0.0, 100.0, 40.0);
        assert!((sdf_ellipse(103.0, 20.0, &r) - 3.0).abs() < 1e-6);
        assert!((sdf_ellipse(50.0, 15.0, &r) + 15.0).abs() < 1e-6);
        assert!((sdf_ellipse(50.0, -2.0, &r) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn ellipse_distance_matches_brute_force() {
        let r = Rect::new(0.0, 0.0, 300.0, 12.0);
        let (a, b) = (150.0, 6.0);
        let samples: Vec<(f64, f64)> = (0..200_000)
            .map(|i| {
                let t = i as f64 / 200_000.0 * std::f64::consts::TAU;
                (a + a * t.cos(), b + b * t.sin())
            })
            .collect();
        for &(x, y) in &[(150.0, 6.0), (10.0, 6.0), (149.0, 1.0), (290.0, 8.0), (300.5, 6.0), (20.0, -4.0), (151.0, 14.0)] {
            let brute = samples.iter().map(|&(sx, sy)| (sx - x).hypot(sy - y)).fold(f64::MAX, f64::min);
            let d = sdf_ellipse(x, y, &r);
            assert!((d.abs() - brute).abs() < 0.02, "({x},{y}): sdf {d}, brute force {brute}");
        }
    }

    fn rejected(doc: serde_json::Value) -> String {
        let doc: Annotation = serde_json::from_value(doc).unwrap();
        let err = render(&base(), &doc).expect_err("render should reject the document");
        match err.downcast_ref::<bugle_core::BugleError>() {
            Some(bugle_core::BugleError::Invalid(m)) => m.clone(),
            other => panic!("expected BugleError::Invalid, got {other:?}"),
        }
    }

    #[test]
    fn render_validates_first() {
        assert!(rejected(serde_json::json!({"canvas":{"padding":u32::MAX,"border_width":u32::MAX}})).contains("canvas.padding"));
        assert!(rejected(serde_json::json!({"scale":1000})).contains("scale"));
        assert!(rejected(serde_json::json!({"shapes":[
            {"type":"redact","rect":{"x":0,"y":0,"w":10,"h":10},"mode":"blur","strength":1e30}
        ]}))
        .contains("shapes[0].strength"));
        // Output size is checked against the actual (cropped) image size:
        // 200x120 at 8x is fine, 2000x1000 at 8x (128 megapixels) is not.
        let doc = Annotation { scale: Some(8.0), ..Default::default() };
        assert_eq!(render(&base(), &doc).unwrap().dimensions(), (1600, 960));
        let big = RgbaImage::new(2000, 1000);
        let doc = Annotation { scale: Some(8.0), canvas: Some(Canvas { padding: 0, shadow: false, ..Default::default() }), ..Default::default() };
        let err = Renderer::with_defaults().unwrap().render(&big, &doc).unwrap_err();
        assert!(err.to_string().contains("scale"), "{err}");
    }

    #[test]
    fn apply_canvas_checks_arithmetic() {
        // Bypasses `validate`: overflow must be an error, not a panic or a
        // giant allocation.
        let img = RgbaImage::new(4, 4);
        for (padding, border_width) in [(u32::MAX, 0), (u32::MAX / 2, 1), (0, u32::MAX), (1 << 20, 0)] {
            let c = Canvas { padding, border_width, ..Default::default() };
            assert!(apply_canvas(&img, &c).is_err(), "padding {padding}, border {border_width}");
        }
        let c = Canvas { padding: 3, border_width: 2, border: Some(Color::RED), ..Default::default() };
        assert_eq!(apply_canvas(&img, &c).unwrap().dimensions(), (14, 14));
    }

    #[test]
    fn blur_region_saturates() {
        let mut img = base();
        // Region at the image edge, absurd sigma: no overflow, sigma clamped.
        blur_region(&mut img, 190, 110, 10, 10, f32::MAX);
        blur_region(&mut img, 190, 110, 10, 10, f32::NAN);
        blur_region(&mut img, 0, 0, 200, 120, 1e9);
        pixelate_region(&mut img, 190, 110, 10, 10, u32::MAX);
    }

    #[test]
    fn highlight_honours_opacity_and_alpha() {
        let px = |doc: serde_json::Value| {
            let doc: Annotation = serde_json::from_value(doc).unwrap();
            *render(&base(), &doc).unwrap().get_pixel(10, 10)
        };
        let rect = serde_json::json!({"x":0,"y":0,"w":50,"h":50});
        // Full strength: plain multiply (240 * 0x80 / 255 = 120), as before.
        assert_eq!(px(serde_json::json!({"shapes":[{"type":"highlight","rect":rect,"color":"#FF8000"}]})), Rgba([240, 120, 0, 255]));
        // Opacity 0 and a transparent colour leave the image alone.
        assert_eq!(
            px(serde_json::json!({"shapes":[{"type":"highlight","rect":rect,"color":"#FF8000","opacity":0}]})),
            Rgba([240, 240, 240, 255])
        );
        assert_eq!(
            px(serde_json::json!({"shapes":[{"type":"highlight","rect":rect,"color":"#FF800000"}]})),
            Rgba([240, 240, 240, 255])
        );
        // Half alpha, or half opacity: half way between.
        let half = Rgba([240, 180, 120, 255]);
        assert_eq!(px(serde_json::json!({"shapes":[{"type":"highlight","rect":rect,"color":"#FF8000FF","opacity":128}]})), half);
        assert_eq!(px(serde_json::json!({"shapes":[{"type":"highlight","rect":rect,"color":"#FF800080"}]})), half);
    }

    #[test]
    fn counter_number_does_not_overflow() {
        let doc: Annotation = serde_json::from_value(serde_json::json!({"shapes":[
            {"type":"counter","at":{"x":30,"y":30},"number":u32::MAX},
            {"type":"counter","at":{"x":90,"y":30}}
        ]}))
        .unwrap();
        render(&base(), &doc).unwrap();
    }

    /// A PNG file that has only a header: enough for `image_dimensions`.
    fn png_header(w: u32, h: u32) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut c = !0u32;
            for &b in data {
                c ^= b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                }
            }
            !c
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            out.extend((data.len() as u32).to_be_bytes());
            let start = out.len();
            out.extend(kind);
            out.extend(data);
            let crc = crc32(&out[start..]);
            out.extend(crc.to_be_bytes());
        }
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend(w.to_be_bytes());
        ihdr.extend(h.to_be_bytes());
        ihdr.extend([8, 6, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &[]);
        chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn image_shape_limits_size() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/bugle-markup-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let huge = dir.join("huge.png");
        std::fs::write(&huge, png_header(20_000, 20_000)).unwrap();
        let msg = rejected(serde_json::json!({"shapes":[{"type":"image","at":{"x":0,"y":0},"path":huge}]}));
        assert!(msg.contains("image.path"), "{msg}");

        let small = dir.join("small.png");
        RgbaImage::from_pixel(1300, 1300, Rgba([1, 2, 3, 255])).save(&small).unwrap();
        let msg = rejected(serde_json::json!({"shapes":[{"type":"image","at":{"x":0,"y":0},"path":small,"scale":8}]}));
        assert!(msg.contains("image.scale"), "{msg}");
        let doc: Annotation =
            serde_json::from_value(serde_json::json!({"shapes":[{"type":"image","at":{"x":10,"y":10},"path":small,"scale":0.05}]})).unwrap();
        assert_eq!(*render(&base(), &doc).unwrap().get_pixel(20, 20), Rgba([1, 2, 3, 255]));
    }

    /// Pixels that differ from the plain background inside `r`.
    fn inked(img: &RgbaImage, r: (u32, u32, u32, u32)) -> usize {
        let bg = Rgba([240, 240, 240, 255]);
        (r.1..r.1 + r.3).flat_map(|y| (r.0..r.0 + r.2).map(move |x| (x, y))).filter(|&(x, y)| *img.get_pixel(x, y) != bg).count()
    }

    #[test]
    fn multi_line_labels_draw_every_line() {
        // 20 px font: lines start 25 px apart. A lone `\r` (as some text
        // fields send it) breaks the line too.
        for text in ["Top\nBottom", "Top\rBottom", "Top\r\nBottom", "Top\u{2028}Bottom"] {
            let doc: Annotation = serde_json::from_value(serde_json::json!({
                "shapes": [{"type":"text","at":{"x":10,"y":10},"text":text,"font_size":20,"color":"black"}]
            }))
            .unwrap();
            let out = render(&base(), &doc).unwrap();
            assert!(inked(&out, (10, 10, 120, 22)) > 20, "{text:?}: first line");
            assert!(inked(&out, (10, 36, 120, 22)) > 20, "{text:?}: second line");
            assert_eq!(inked(&out, (10, 62, 120, 40)), 0, "{text:?}: nothing below");
        }
        // A blank line in the middle keeps its height.
        let doc: Annotation = serde_json::from_value(serde_json::json!({
            "shapes": [{"type":"text","at":{"x":10,"y":10},"text":"a\n\nb","font_size":20,"color":"black"}]
        }))
        .unwrap();
        let out = render(&base(), &doc).unwrap();
        assert_eq!(inked(&out, (10, 36, 120, 22)), 0);
        assert!(inked(&out, (10, 62, 120, 22)) > 5);
    }

    #[test]
    fn arrow_styles_differ() {
        let draw = |style: &str| {
            let doc: Annotation = serde_json::from_value(serde_json::json!({
                "shapes": [{"type":"arrow","from":{"x":20,"y":60},"to":{"x":180,"y":60},"arrow":style,"stroke_width":6,"color":"black"}]
            }))
            .unwrap();
            render(&base(), &doc).unwrap()
        };
        let [standard, double, slim, fancy] = ["standard", "double", "slim", "fancy"].map(draw);
        // Only the double arrow has a head at the tail: ink beside the shaft.
        let beside_tail = (34, 52, 10, 4);
        assert!(inked(&double, beside_tail) > 20);
        assert_eq!(inked(&standard, beside_tail), 0);
        // Slim is thinner along the shaft, fancy tapers towards the tail.
        let shaft = (60, 50, 40, 20);
        assert!(inked(&slim, shaft) < inked(&standard, shaft));
        assert!(inked(&fancy, (25, 50, 20, 20)) < inked(&standard, (25, 50, 20, 20)));
        assert_ne!(standard, fancy);
    }

    #[test]
    fn redact_styles_differ() {
        let mut img = base();
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = Rgba([(x * 7) as u8, (y * 11) as u8, ((x * y) & 0xFF) as u8, 255]);
        }
        let draw = |mode: &str| {
            let doc: Annotation = serde_json::from_value(serde_json::json!({
                "shapes": [{"type":"redact","rect":{"x":20,"y":20,"w":120,"h":60},"mode":mode}]
            }))
            .unwrap();
            render(&img, &doc).unwrap()
        };
        let outs = ["pixelate", "blur", "black_out", "secure_blur"].map(draw);
        for (i, a) in outs.iter().enumerate() {
            assert_ne!(*a, img, "mode {i} changed nothing");
            assert_eq!(*a.get_pixel(5, 5), *img.get_pixel(5, 5), "mode {i} leaked outside");
            for b in &outs[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn source_origin_matches_the_render() {
        // A marker at source (60, 40); crop from (50, 30) and a 16 + 4 px
        // canvas: it lands at (60 - 50 + 20, 40 - 30 + 20).
        let mut img = base();
        img.put_pixel(60, 40, Rgba([255, 0, 0, 255]));
        let doc = Annotation {
            crop: Some(Rect::new(50.0, 30.0, 100.0, 60.0)),
            canvas: Some(Canvas {
                padding: 16,
                border_width: 4,
                border: Some(Color::WHITE),
                corner_radius: 0,
                shadow: false,
                background: Background::Named("mono".into()),
            }),
            ..Default::default()
        };
        let out = render(&img, &doc).unwrap();
        let o = doc.source_origin();
        assert_eq!((o.x, o.y), (-30.0, -10.0));
        assert_eq!(*out.get_pixel((60.0 + o.x) as u32, (40.0 + o.y) as u32), Rgba([255, 0, 0, 255]));
        assert_eq!(out.dimensions(), (140, 100));
    }

    #[test]
    fn box_blur_matches_a_gaussian() {
        for sigma in [4.0f32, 9.5, 28.0] {
            let sizes = box_sizes(sigma);
            assert!(sizes.iter().all(|s| s % 2 == 1), "{sizes:?}");
            // Three boxes of width s have variance sum((s^2 - 1) / 12).
            let var: f32 = sizes.iter().map(|s| (*s as f32 * *s as f32 - 1.0) / 12.0).sum();
            assert!((var.sqrt() - sigma).abs() < 0.6, "sigma {sigma}: boxes {sizes:?} give {}", var.sqrt());
        }
        // A filled square, blurred both ways: within a few percent anywhere.
        let (w, h) = (160usize, 120usize);
        let inside = |x: usize, y: usize| (50..110).contains(&x) && (40..80).contains(&y);
        let mut mask: Vec<f32> = (0..w * h).map(|i| if inside(i % w, i / w) { 1.0 } else { 0.0 }).collect();
        let total: f32 = mask.iter().sum();
        blur_mask(&mut mask, w, h, 8.0);
        let img = image::GrayImage::from_fn(w as u32, h as u32, |x, y| image::Luma([if inside(x as usize, y as usize) { 255 } else { 0 }]));
        let reference = imageops::blur(&img, 8.0);
        let worst = (0..w * h).map(|i| (mask[i] - reference.as_raw()[i] as f32 / 255.0).abs()).fold(0.0, f32::max);
        assert!(worst < 0.05, "largest difference {worst}");
        // Nothing reaches the edge, so nothing is lost.
        assert!((mask.iter().sum::<f32>() - total).abs() < 1.0);
        // Degenerate input is left alone.
        blur_mask(&mut [], 0, 0, 3.0);
        let mut one = [1.0f32];
        blur_mask(&mut one, 1, 1, f32::NAN);
        assert_eq!(one, [1.0]);
    }

    /// `cargo test -p bugle-markup --release -- --ignored --nocapture`:
    /// how long a canvas takes on a 5K capture.
    #[test]
    #[ignore]
    fn time_large_canvas() {
        let img = RgbaImage::from_pixel(5120, 2880, Rgba([90, 120, 200, 255]));
        let doc: Annotation = serde_json::from_value(serde_json::json!({
            "shapes": [{"type":"arrow","from":{"x":100,"y":100},"to":{"x":3000,"y":2000}},
                       {"type":"spotlight","rect":{"x":500,"y":500,"w":1000,"h":800}}],
            "canvas": {"padding": 192, "background": "ocean"}
        }))
        .unwrap();
        let r = Renderer::with_defaults().unwrap();
        let t = std::time::Instant::now();
        let out = r.render(&img, &doc).unwrap();
        eprintln!("5120x2880 with canvas: {:?} → {}x{}", t.elapsed(), out.width(), out.height());
    }

    #[test]
    fn shapes_past_the_cropped_edge_draw_nothing() {
        // An editor keeps shapes in original pixels and translates them by
        // the crop, so after cropping they routinely start past the right or
        // bottom edge (or before the top-left one).
        let img = RgbaImage::from_pixel(800, 600, Rgba([240, 240, 240, 255]));
        let rect = |x: f64, y: f64| serde_json::json!({"x":x,"y":y,"w":120,"h":80});
        for (x, y) in [(500.0, 100.0), (100.0, 450.0), (500.0, 450.0), (-300.0, -300.0)] {
            let doc: Annotation = serde_json::from_value(serde_json::json!({
                "crop": {"x":0,"y":0,"w":400,"h":300},
                "shapes": [
                    {"type":"highlight","rect":rect(x, y)},
                    {"type":"redact","rect":rect(x, y),"mode":"pixelate"},
                    {"type":"redact","rect":rect(x, y),"mode":"blur"},
                    {"type":"redact","rect":rect(x, y),"mode":"black_out"},
                    {"type":"redact","rect":rect(x, y),"mode":"secure_blur"},
                    {"type":"spotlight","rect":rect(x, y),"dim":0.5},
                    {"type":"rectangle","rect":rect(x, y),"fill":"#FF000080"},
                    {"type":"ellipse","rect":rect(x, y)},
                    {"type":"arrow","from":{"x":x,"y":y},"to":{"x":x + 100.0,"y":y + 50.0}},
                    {"type":"text","at":{"x":x,"y":y},"text":"off\nside","background":"#000000"},
                    {"type":"counter","at":{"x":x,"y":y}}
                ]
            }))
            .unwrap();
            let out = render(&img, &doc).unwrap();
            assert_eq!(out.dimensions(), (400, 300), "({x}, {y})");
        }
    }

    #[test]
    fn crop_then_scale() {
        let doc = Annotation { crop: Some(Rect::new(10.0, 10.0, 100.0, 50.0)), scale: Some(0.5), ..Default::default() };
        let out = render(&base(), &doc).unwrap();
        assert_eq!((out.width(), out.height()), (50, 25));
    }

    #[test]
    fn redact_changes_pixels() {
        let mut img = base();
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = Rgba([(x * 3) as u8, (y * 5) as u8, ((x ^ y) & 0xFF) as u8, 255]);
        }
        let before = *img.get_pixel(30, 30);
        let doc: Annotation = serde_json::from_value(serde_json::json!({
            "shapes": [{"type":"redact","rect":{"x":20,"y":20,"w":40,"h":40},"mode":"black_out"}]
        }))
        .unwrap();
        let out = render(&img, &doc).unwrap();
        assert_ne!(*out.get_pixel(30, 30), before);
        assert_eq!(*out.get_pixel(30, 30), Rgba([0, 0, 0, 255]));
        assert_eq!(*out.get_pixel(5, 5), *img.get_pixel(5, 5));
    }
}
