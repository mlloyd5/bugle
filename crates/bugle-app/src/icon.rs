//! The status-bar glyph, rasterised at start-up so the binary needs no asset
//! files: a camera body with a lens, drawn from signed distance functions.
//! It is a template image (black + alpha), so macOS tints it for light and
//! dark menu bars and for the pressed state.

use tray_icon::Icon;

/// Size in pixels; the menu bar shows it at half that in points (Retina).
const SIZE: u32 = 36;

pub fn status_icon() -> Icon {
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let a = coverage(px, py);
            let i = ((y * SIZE + x) * 4) as usize;
            rgba[i] = 0;
            rgba[i + 1] = 0;
            rgba[i + 2] = 0;
            rgba[i + 3] = (a * 255.0).round() as u8;
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("icon buffer")
}

/// Alpha coverage of the glyph at a pixel centre.
fn coverage(x: f32, y: f32) -> f32 {
    let s = SIZE as f32;
    // Camera body: rounded rectangle outline.
    let body = sd_round_rect(x - s * 0.5, y - s * 0.56, s * 0.40, s * 0.27, 4.0);
    let body_stroke = body.abs() - 2.4;
    // Viewfinder bump on top of the body.
    let bump = sd_round_rect(x - s * 0.5, y - s * 0.29, s * 0.14, s * 0.07, 2.0);
    let bump_fill = bump.max(-body); // only the part outside the body
    // Lens: ring inside the body.
    let lens = ((x - s * 0.5).powi(2) + (y - s * 0.58).powi(2)).sqrt() - s * 0.14;
    let lens_stroke = lens.abs() - 2.2;
    let d = body_stroke.min(bump_fill).min(lens_stroke);
    smooth(d)
}

fn smooth(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

/// Signed distance to a rounded rectangle centred at the origin with
/// half-extents `hw`/`hh` and corner radius `r`.
fn sd_round_rect(x: f32, y: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = x.abs() - hw + r;
    let qy = y.abs() - hh + r;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_has_ink_and_transparency() {
        let mut ink = 0;
        let mut clear = 0;
        for y in 0..SIZE {
            for x in 0..SIZE {
                let a = coverage(x as f32 + 0.5, y as f32 + 0.5);
                if a > 0.9 {
                    ink += 1;
                } else if a < 0.1 {
                    clear += 1;
                }
            }
        }
        assert!(ink > 100, "ink={ink}");
        assert!(clear > 600, "clear={clear}");
        let _ = status_icon();
    }
}
