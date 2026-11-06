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

    pub fn render(&self, src: &RgbaImage, doc: &Annotation) -> Result<RgbaImage> {
        let mut img = match doc.crop {
            Some(r) => crop(src, r)?,
            None => src.clone(),
        };
        let base = doc.style.merged(&self.defaults);
        let mut counter = 0u32;
        for shape in &doc.shapes {
            self.draw_shape(&mut img, shape, &base, &mut counter)?;
        }
        if let Some(canvas) = &doc.canvas {
            img = apply_canvas(&img, canvas);
        }
        if let Some(s) = doc.scale {
            if s > 0.0 && (s - 1.0).abs() > 1e-3 {
                let w = ((img.width() as f32) * s).round().max(1.0) as u32;
                let h = ((img.height() as f32) * s).round().max(1.0) as u32;
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
                multiply_rect(img, rect, c);
            }
            Shape::Counter { at, number, style } => {
                let st = style.merged(base);
                let n = match number {
                    Some(n) => {
                        *counter = *n;
                        *n
                    }
                    None => {
                        *counter += 1;
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
                let overlay = image::open(path).with_context(|| format!("loading {path}"))?.to_rgba8();
                let overlay = if (*scale - 1.0).abs() > 1e-3 && *scale > 0.0 {
                    imageops::resize(
                        &overlay,
                        ((overlay.width() as f32) * scale).max(1.0) as u32,
                        ((overlay.height() as f32) * scale).max(1.0) as u32,
                        imageops::FilterType::Lanczos3,
                    )
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
        let lines: Vec<&str> = text.lines().collect();
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
    Err(anyhow!("no usable font found; set annotate.font_path in the config"))
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

/// Approximate signed distance to an ellipse inscribed in `r`.
fn sdf_ellipse(px: f64, py: f64, r: &Rect) -> f64 {
    let rx = (r.w / 2.0).max(0.5);
    let ry = (r.h / 2.0).max(0.5);
    let cx = r.x + rx;
    let cy = r.y + ry;
    let nx = (px - cx) / rx;
    let ny = (py - cy) / ry;
    let k = (nx * nx + ny * ny).sqrt();
    // Gradient-normalised approximation: good to a fraction of a pixel for
    // the aspect ratios screenshots use.
    let gx = nx / rx;
    let gy = ny / ry;
    let g = (gx * gx + gy * gy).sqrt().max(1e-6);
    (k - 1.0) * k / g
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
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

fn multiply_rect(img: &mut RgbaImage, r: &Rect, c: Color) {
    let Some((x0, y0, w, h)) = clamp_rect(img, r) else { return };
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            let p = img.get_pixel_mut(x, y);
            for i in 0..3 {
                p.0[i] = ((p.0[i] as u32 * c.0[i] as u32) / 255) as u8;
            }
        }
    }
}

fn spotlight(img: &mut RgbaImage, r: &Rect, dim: f32) {
    let keep = (1.0 - dim.clamp(0.0, 1.0)) as f32;
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
                    for i in 0..4 {
                        sum[i] += p.0[i] as u64;
                    }
                }
            }
            let n = (bw * bh) as u64;
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
    let pad = (sigma * 3.0).ceil() as u32;
    let px0 = x0.saturating_sub(pad);
    let py0 = y0.saturating_sub(pad);
    let px1 = (x0 + w + pad).min(img.width());
    let py1 = (y0 + h + pad).min(img.height());
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

fn apply_canvas(img: &RgbaImage, canvas: &Canvas) -> RgbaImage {
    let pad = canvas.padding;
    let bw = canvas.border_width;
    let (iw, ih) = (img.width(), img.height());
    let (ow, oh) = (iw + 2 * (pad + bw), ih + 2 * (pad + bw));
    let mut out = RgbaImage::from_pixel(ow, oh, Rgba([0, 0, 0, 0]));

    if let Some((from, to, angle)) = canvas.background.resolve() {
        let a = angle.to_radians() as f64;
        let (dx, dy) = (a.sin(), -a.cos());
        let span = (ow as f64 * dx.abs()) + (oh as f64 * dy.abs());
        for y in 0..oh {
            for x in 0..ow {
                let proj = (x as f64 - ow as f64 / 2.0) * dx + (y as f64 - oh as f64 / 2.0) * dy;
                let t = ((proj / span) + 0.5).clamp(0.0, 1.0) as f32;
                let mut p = [0u8; 4];
                for i in 0..4 {
                    p[i] = (from.0[i] as f32 + (to.0[i] as f32 - from.0[i] as f32) * t).round() as u8;
                }
                out.put_pixel(x, y, Rgba(p));
            }
        }
    }

    let inner = Rect::new((pad + bw) as f64, (pad + bw) as f64, iw as f64, ih as f64);
    let radius = canvas.corner_radius as f32;

    if canvas.shadow && pad > 0 {
        let sigma = (pad as f32 / 3.0).clamp(4.0, 28.0);
        let offset_y = (pad as f64 / 4.0).min(24.0);
        let shadow_rect = Rect::new(inner.x, inner.y + offset_y, inner.w, inner.h);
        let mut mask = RgbaImage::from_pixel(ow, oh, Rgba([0, 0, 0, 0]));
        fill_coverage(&mut mask, expand(&shadow_rect, 1.0), Color([0, 0, 0, 140]), |x, y| {
            coverage(sdf_round_rect(x, y, &shadow_rect, radius))
        });
        let mask = imageops::blur(&mask, sigma);
        for y in 0..oh {
            for x in 0..ow {
                let m = mask.get_pixel(x, y);
                if m.0[3] > 0 {
                    blend(out.get_pixel_mut(x, y), Color([0, 0, 0, 255]), m.0[3] as f32 / 255.0);
                }
            }
        }
    }

    if let (Some(bc), true) = (canvas.border, bw > 0) {
        let br = Rect::new(pad as f64, pad as f64, (iw + 2 * bw) as f64, (ih + 2 * bw) as f64);
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
    out
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
