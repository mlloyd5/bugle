//! The status-bar glyph, rasterised at start-up so the binary needs no asset
//! files: the horn from the application icon in one colour, drawn from signed
//! distance functions (`icon/glyph.rs`, whose rounded-rect field also masks
//! the `.app` icon built by `cargo xtask bundle`).
//! It is a template image (black + alpha), so macOS tints it for light and
//! dark menu bars and for the pressed state.

mod glyph;

use tray_icon::Icon;

/// Size in pixels; the menu bar shows it at half that in points (Retina).
const SIZE: u32 = 36;

pub fn status_icon() -> Icon {
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let a = coverage(x as f32 + 0.5, y as f32 + 0.5);
            let i = ((y * SIZE + x) * 4) as usize;
            rgba[i + 3] = (a * 255.0).round() as u8;
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("icon buffer")
}

/// Alpha coverage of the glyph at a pixel centre.
fn coverage(x: f32, y: f32) -> f32 {
    glyph::coverage(x, y, SIZE as f32)
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

    #[test]
    fn glyph_scales_with_size() {
        // The same relative points are ink or clear at any size.
        for size in [36.0f32, 128.0, 1024.0] {
            let at = |u: f32, v: f32| glyph::coverage(u / 36.0 * size, v / 36.0 * size, size);
            assert!(at(10.0, 17.0) > 0.9, "tube, size {size}");
            assert!(at(15.2, 28.2) > 0.9, "bottom of the loop, size {size}");
            assert!(at(28.0, 12.0) > 0.9, "bell, size {size}");
            assert!(at(15.2, 22.6) < 0.01, "inside the loop, size {size}");
            assert!(at(4.0, 30.0) < 0.01, "below the mouthpiece, size {size}");
            assert!(at(0.7, 0.7) < 0.01, "corner, size {size}");
        }
    }
}
