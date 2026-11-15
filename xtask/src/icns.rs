//! The application icon: the menu-bar camera glyph (shared SDF in
//! `crates/bugle-app/src/icon/glyph.rs`) in white on a brass-coloured rounded
//! square, rendered directly at every size and packed into an `.icns` file.
//!
//! Every size is rasterised from the distance fields rather than downscaled,
//! so small sizes stay crisp, and the output is byte-for-byte reproducible.

#[path = "../../crates/bugle-app/src/icon/glyph.rs"]
mod glyph;

use anyhow::Result;
use image::{ImageEncoder, RgbaImage};

/// The design grid: a 1024-unit canvas with an 824-unit rounded square, the
/// usual proportions for macOS app icons.
const CANVAS: f32 = 1024.0;
const BODY_HALF: f32 = 412.0;
const BODY_RADIUS: f32 = 185.0;
/// Edge length of the glyph's square, in canvas units.
const GLYPH: f32 = 620.0;
/// Gradient top and bottom (sRGB, 0–1).
const TOP: [f32; 3] = [1.0, 0.749, 0.329]; // #FFBF54
const BOTTOM: [f32; 3] = [0.894, 0.408, 0.129]; // #E46821

/// `.icns` element types and the pixel size each holds.
pub const ICNS_ELEMENTS: &[(&[u8; 4], u32)] = &[
    (b"icp4", 16),
    (b"icp5", 32),
    (b"ic11", 32), // 16@2x
    (b"ic12", 64), // 32@2x
    (b"ic07", 128),
    (b"ic13", 256), // 128@2x
    (b"ic08", 256),
    (b"ic14", 512), // 256@2x
    (b"ic09", 512),
    (b"ic10", 1024), // 512@2x
];

/// Renders the icon at `px`×`px`.
pub fn render(px: u32) -> RgbaImage {
    let scale = CANVAS / px as f32; // canvas units per output pixel
    let glyph_px = GLYPH / scale;
    let glyph_x0 = (CANVAS * 0.5 - GLYPH * 0.5) / scale;
    // The glyph's ink spans roughly y = 0.22..0.83 of its square; centre that.
    let glyph_y0 = (CANVAS * 0.5 - GLYPH * 0.525) / scale;
    let mut img = RgbaImage::new(px, px);
    for (x, y, p) in img.enumerate_pixels_mut() {
        let fx = x as f32 + 0.5;
        let fy = y as f32 + 0.5;
        let (cx, cy) = (fx * scale - CANVAS * 0.5, fy * scale - CANVAS * 0.5);

        // Soft drop shadow under the body.
        let ds = glyph::sd_round_rect(cx, cy - 10.0, BODY_HALF, BODY_HALF, BODY_RADIUS);
        let shadow = 0.28 * (1.0 - ((ds + 6.0) / 30.0).clamp(0.0, 1.0)).powi(2);
        let mut c = [0.0f32; 3];
        let mut a = shadow;

        // Body with a vertical gradient.
        let db = glyph::sd_round_rect(cx, cy, BODY_HALF, BODY_HALF, BODY_RADIUS);
        let body_a = (0.5 - db / scale).clamp(0.0, 1.0);
        let t = ((cy + BODY_HALF) / (2.0 * BODY_HALF)).clamp(0.0, 1.0);
        let body: [f32; 3] = std::array::from_fn(|i| TOP[i] + (BOTTOM[i] - TOP[i]) * t);
        over(&mut c, &mut a, body, body_a);

        // White glyph, clipped to the body.
        let g = glyph::coverage(fx - glyph_x0, fy - glyph_y0, glyph_px) * body_a;
        over(&mut c, &mut a, [1.0; 3], g);

        let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        *p = image::Rgba([to8(c[0]), to8(c[1]), to8(c[2]), to8(a)]);
    }
    img
}

/// Straight-alpha "source over" of `src` with alpha `sa` onto `dst`/`da`.
fn over(dst: &mut [f32; 3], da: &mut f32, src: [f32; 3], sa: f32) {
    let out_a = sa + *da * (1.0 - sa);
    if out_a <= 0.0 {
        return;
    }
    for i in 0..3 {
        dst[i] = (src[i] * sa + dst[i] * *da * (1.0 - sa)) / out_a;
    }
    *da = out_a;
}

pub fn png(img: &RgbaImage) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buf).write_image(
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(buf)
}

/// The complete `.icns` file: `icns` + total length, then one PNG element per
/// entry of [`ICNS_ELEMENTS`] (type + element length + data, big-endian).
pub fn icns() -> Result<Vec<u8>> {
    let mut body = Vec::new();
    for (kind, size) in ICNS_ELEMENTS {
        let data = png(&render(*size))?;
        body.extend_from_slice(*kind);
        body.extend_from_slice(&(data.len() as u32 + 8).to_be_bytes());
        body.extend_from_slice(&data);
    }
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_clear_and_centre_is_opaque() {
        let img = render(128);
        assert_eq!(img.get_pixel(0, 0)[3], 0);
        assert_eq!(img.get_pixel(127, 0)[3], 0);
        let mid = img.get_pixel(64, 64);
        assert_eq!(mid[3], 255);
        // The left edge of the camera body is white ink.
        let ink = img.get_pixel((64.0 - 0.4 * GLYPH / 8.0) as u32, 64);
        assert!(ink[0] > 240 && ink[1] > 240 && ink[2] > 240, "{ink:?}");
    }

    #[test]
    fn icns_layout_is_consistent() {
        let data = icns().unwrap();
        assert_eq!(&data[..4], b"icns");
        assert_eq!(u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize, data.len());
        let mut off = 8;
        let mut kinds = Vec::new();
        while off < data.len() {
            let kind = &data[off..off + 4];
            let len = u32::from_be_bytes(data[off + 4..off + 8].try_into().unwrap()) as usize;
            assert_eq!(&data[off + 8..off + 16], b"\x89PNG\r\n\x1a\n");
            kinds.push(kind.to_vec());
            off += len;
        }
        assert_eq!(off, data.len());
        assert_eq!(kinds.len(), ICNS_ELEMENTS.len());
    }

    #[test]
    fn rendering_is_deterministic() {
        assert_eq!(png(&render(64)).unwrap(), png(&render(64)).unwrap());
    }
}
