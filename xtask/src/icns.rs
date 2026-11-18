//! The application icon: the full-bleed artwork in
//! `packaging/Bugle-icon-1024.png` (a horn inside capture corners on an amber
//! gradient), masked to the macOS rounded square with a soft drop shadow, then
//! scaled to every size and packed into an `.icns` file.
//!
//! The artwork is embedded at compile time, so the build does not depend on
//! the working directory, and every step is deterministic: the same artwork
//! gives a byte-for-byte identical `.icns`.

#[allow(dead_code)] // only the rounded-rect distance field is used here
#[path = "../../crates/bugle-app/src/icon/glyph.rs"]
mod glyph;

use anyhow::{Context, Result};
use image::imageops::{self, FilterType};
use image::{ImageEncoder, Rgba, Rgba32FImage, RgbaImage};

/// The master artwork: square, full bleed, no transparency.
const ARTWORK: &[u8] = include_bytes!("../../packaging/Bugle-icon-1024.png");

/// The design grid: a 1024-unit canvas with an 824-unit rounded square, the
/// usual proportions for macOS app icons.
const CANVAS: u32 = 1024;
const BODY_HALF: f32 = 412.0;
const BODY_RADIUS: f32 = 185.0;

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

/// The icon on the full 1024-unit canvas, premultiplied, linear in the
/// sRGB-encoded values (as the artwork is).
fn master() -> Result<Rgba32FImage> {
    let art = image::load_from_memory_with_format(ARTWORK, image::ImageFormat::Png)
        .context("decoding packaging/Bugle-icon-1024.png")?
        .to_rgba32f();
    let side = (2.0 * BODY_HALF) as u32;
    let art = imageops::resize(&art, side, side, FilterType::Lanczos3);
    let origin = CANVAS as f32 * 0.5 - BODY_HALF;

    let mut img = Rgba32FImage::new(CANVAS, CANVAS);
    for (x, y, p) in img.enumerate_pixels_mut() {
        let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
        let (cx, cy) = (fx - CANVAS as f32 * 0.5, fy - CANVAS as f32 * 0.5);

        // Soft drop shadow under the body.
        let ds = glyph::sd_round_rect(cx, cy - 10.0, BODY_HALF, BODY_HALF, BODY_RADIUS);
        let shadow = 0.28 * (1.0 - ((ds + 6.0) / 30.0).clamp(0.0, 1.0)).powi(2);

        // The artwork, clipped to the body with one unit of anti-aliasing.
        let db = glyph::sd_round_rect(cx, cy, BODY_HALF, BODY_HALF, BODY_RADIUS);
        let body_a = (0.5 - db).clamp(0.0, 1.0);
        let ax = ((fx - origin).floor() as i64).clamp(0, side as i64 - 1) as u32;
        let ay = ((fy - origin).floor() as i64).clamp(0, side as i64 - 1) as u32;
        let c = art.get_pixel(ax, ay).0;

        // Premultiplied "source over": body onto shadow (black).
        let a = body_a + shadow * (1.0 - body_a);
        *p = Rgba([c[0] * body_a, c[1] * body_a, c[2] * body_a, a]);
    }
    Ok(img)
}

/// Premultiplied float image → straight-alpha 8-bit.
fn to_rgba8(img: &Rgba32FImage) -> RgbaImage {
    let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    RgbaImage::from_fn(img.width(), img.height(), |x, y| {
        let [r, g, b, a] = img.get_pixel(x, y).0;
        let a = a.clamp(0.0, 1.0);
        let un = |v: f32| if a > 0.0 { v / a } else { 0.0 };
        Rgba([to8(un(r)), to8(un(g)), to8(un(b)), to8(a)])
    })
}

/// Renders the icon at `px`×`px` from an already composed master.
fn render_from(master: &Rgba32FImage, px: u32) -> RgbaImage {
    if px == CANVAS {
        to_rgba8(master)
    } else {
        // Resampling premultiplied values keeps the transparent corners from
        // bleeding dark fringes into the edge.
        to_rgba8(&imageops::resize(master, px, px, FilterType::Lanczos3))
    }
}

/// Renders the icon at `px`×`px`.
pub fn render(px: u32) -> Result<RgbaImage> {
    Ok(render_from(&master()?, px))
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
    let master = master()?;
    let mut body = Vec::new();
    for (kind, size) in ICNS_ELEMENTS {
        let data = png(&render_from(&master, *size))?;
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
    fn artwork_is_a_square_master() {
        let art = image::load_from_memory(ARTWORK).unwrap();
        assert_eq!((art.width(), art.height()), (CANVAS, CANVAS));
    }

    #[test]
    fn corners_are_clear_and_body_is_artwork() {
        let img = render(128).unwrap();
        assert_eq!(img.get_pixel(0, 0)[3], 0);
        assert_eq!(img.get_pixel(127, 0)[3], 0);
        // The top of the body is the warm gradient, fully opaque.
        let top = img.get_pixel(64, 20);
        assert_eq!(top[3], 255);
        assert!(top[0] > 200 && top[1] > 100 && top[2] < 120, "{top:?}");
        // Somewhere in the middle there is white ink (the horn).
        let ink = (32..96)
            .flat_map(|y| (32..96).map(move |x| (x, y)))
            .filter(|&(x, y)| img.get_pixel(x, y).0[..3].iter().all(|&v| v > 235))
            .count();
        assert!(ink > 100, "only {ink} white pixels");
    }

    #[test]
    fn shadow_sits_below_the_body() {
        let img = render(1024).unwrap();
        let below = img.get_pixel(512, 512 + BODY_HALF as u32 + 8);
        let above = img.get_pixel(512, 512 - BODY_HALF as u32 - 8);
        assert!(below[3] > above[3], "{below:?} vs {above:?}");
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
        assert_eq!(png(&render(64).unwrap()).unwrap(), png(&render(64).unwrap()).unwrap());
    }
}
