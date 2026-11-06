//! Frame stitching for scrolling capture.
//!
//! Primary alignment comes from Vision's translational registration. A
//! pure-Rust row-profile
//! search backs it up when Vision locks onto a sticky header or a blank area.

use image::{GenericImageView, RgbaImage};

/// Result of comparing two consecutive frames.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shift {
    /// Pixels the content moved along the scroll axis. Positive means new
    /// content arrived at the far end (page scrolled forward).
    pub pixels: i64,
    pub confidence: f32,
}

/// Mean absolute difference of two equally sized frames (0..255), sampled.
pub fn frame_difference(a: &RgbaImage, b: &RgbaImage) -> f64 {
    if a.dimensions() != b.dimensions() {
        return 255.0;
    }
    let step = ((a.width() * a.height()) / 20_000).max(1);
    let (mut sum, mut n) = (0u64, 0u64);
    let pa = a.as_raw();
    let pb = b.as_raw();
    let mut i = 0;
    while i + 3 < pa.len() {
        sum += (pa[i] as i64 - pb[i] as i64).unsigned_abs() + (pa[i + 1] as i64 - pb[i + 1] as i64).unsigned_abs();
        n += 2;
        i += 4 * step as usize;
    }
    if n == 0 {
        0.0
    } else {
        sum as f64 / n as f64
    }
}

/// Per-row (or per-column) grayscale signature.
fn profile(img: &RgbaImage, horizontal: bool) -> Vec<f32> {
    let (w, h) = img.dimensions();
    let raw = img.as_raw();
    if horizontal {
        let mut out = vec![0f32; w as usize];
        for x in 0..w as usize {
            let mut s = 0u32;
            for y in (0..h as usize).step_by(2) {
                let i = (y * w as usize + x) * 4;
                s += raw[i] as u32 + raw[i + 1] as u32 + raw[i + 2] as u32;
            }
            out[x] = s as f32;
        }
        out
    } else {
        let mut out = vec![0f32; h as usize];
        for y in 0..h as usize {
            let mut s = 0u32;
            for x in (0..w as usize).step_by(2) {
                let i = (y * w as usize + x) * 4;
                s += raw[i] as u32 + raw[i + 1] as u32 + raw[i + 2] as u32;
            }
            out[y] = s as f32;
        }
        out
    }
}

/// Estimate how far `next` scrolled relative to `prev` using 1-D profile
/// correlation. Only forward shifts are searched.
pub fn estimate_shift_profile(prev: &RgbaImage, next: &RgbaImage, horizontal: bool) -> Shift {
    let a = profile(prev, horizontal);
    let b = profile(next, horizontal);
    let n = a.len().min(b.len());
    if n < 8 {
        return Shift { pixels: 0, confidence: 0.0 };
    }
    // Use gradients so flat backgrounds do not dominate.
    let ga: Vec<f32> = a.windows(2).map(|w| w[1] - w[0]).collect();
    let gb: Vec<f32> = b.windows(2).map(|w| w[1] - w[0]).collect();
    let n = ga.len().min(gb.len());
    let min_overlap = (n / 4).max(8);
    let mut best = (0i64, f32::MAX);
    let mut second = f32::MAX;
    for s in 0..(n - min_overlap) {
        // prev[s..] should match next[..n-s]
        let mut err = 0f32;
        let mut cnt = 0;
        for i in (0..n - s).step_by(1) {
            err += (ga[i + s] - gb[i]).abs();
            cnt += 1;
        }
        let e = err / cnt as f32;
        if e < best.1 {
            second = best.1;
            best = (s as i64, e);
        } else if e < second {
            second = e;
        }
    }
    let confidence = if second.is_finite() && second > 0.0 { (1.0 - best.1 / second).clamp(0.0, 1.0) } else { 0.0 };
    Shift { pixels: best.0, confidence }
}

/// Append the new strip of `next` to `canvas` given the shift.
pub fn append_strip(canvas: &mut RgbaImage, next: &RgbaImage, shift: i64, horizontal: bool) {
    let (fw, fh) = next.dimensions();
    if shift <= 0 {
        return;
    }
    if horizontal {
        let strip_w = (shift as u32).min(fw);
        let strip = next.view(fw - strip_w, 0, strip_w, fh).to_image();
        let mut out = RgbaImage::new(canvas.width() + strip_w, canvas.height().max(fh));
        image::imageops::overlay(&mut out, canvas, 0, 0);
        image::imageops::overlay(&mut out, &strip, canvas.width() as i64, 0);
        *canvas = out;
    } else {
        let strip_h = (shift as u32).min(fh);
        let strip = next.view(0, fh - strip_h, fw, strip_h).to_image();
        let mut out = RgbaImage::new(canvas.width().max(fw), canvas.height() + strip_h);
        image::imageops::overlay(&mut out, canvas, 0, 0);
        image::imageops::overlay(&mut out, &strip, 0, canvas.height() as i64);
        *canvas = out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn textured(w: u32, h: u32, offset: u32) -> RgbaImage {
        let mut img = RgbaImage::new(w, h);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let yy = y + offset;
            let v = (((x / 7) * 31 + (yy / 5) * 17) % 199) as u8;
            let u = ((yy * 13) % 251) as u8;
            *p = image::Rgba([v, u, v ^ u, 255]);
        }
        img
    }

    #[test]
    fn profile_shift_is_recovered() {
        let a = textured(300, 400, 0);
        let b = textured(300, 400, 123);
        let s = estimate_shift_profile(&a, &b, false);
        assert_eq!(s.pixels, 123);
        assert!(s.confidence > 0.3, "{s:?}");
    }

    #[test]
    fn strips_append() {
        let a = textured(300, 400, 0);
        let b = textured(300, 400, 123);
        let mut canvas = a.clone();
        append_strip(&mut canvas, &b, 123, false);
        assert_eq!(canvas.height(), 523);
        // Row 500 of the canvas should equal row 500 of the virtual page.
        let page = textured(300, 600, 0);
        assert_eq!(canvas.get_pixel(10, 500), page.get_pixel(10, 500));
    }
}
