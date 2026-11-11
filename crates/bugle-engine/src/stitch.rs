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

/// Mean absolute difference of two equally sized frames (0..255) over the
/// R, G and B channels of every pixel. Frames of different sizes count as
/// completely different.
pub fn frame_difference(a: &RgbaImage, b: &RgbaImage) -> f64 {
    if a.dimensions() != b.dimensions() {
        return 255.0;
    }
    let (mut sum, mut n) = (0u64, 0u64);
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        sum += (0..3).map(|c| pa[c].abs_diff(pb[c]) as u64).sum::<u64>();
        n += 3;
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
        for (x, o) in out.iter_mut().enumerate() {
            let mut s = 0u32;
            for y in (0..h as usize).step_by(2) {
                let i = (y * w as usize + x) * 4;
                s += raw[i] as u32 + raw[i + 1] as u32 + raw[i + 2] as u32;
            }
            *o = s as f32;
        }
        out
    } else {
        let mut out = vec![0f32; h as usize];
        for (y, o) in out.iter_mut().enumerate() {
            let mut s = 0u32;
            for x in (0..w as usize).step_by(2) {
                let i = (y * w as usize + x) * 4;
                s += raw[i] as u32 + raw[i + 1] as u32 + raw[i + 2] as u32;
            }
            *o = s as f32;
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
    // Too short to compare with the required overlap.
    let Some(search) = n.checked_sub(min_overlap).filter(|&k| k > 0) else {
        return Shift { pixels: 0, confidence: 0.0 };
    };
    let mut best = (0i64, f32::MAX);
    let mut second = f32::MAX;
    for s in 0..search {
        // prev[s..] should match next[..n-s]
        let err: f32 = ga[s..n].iter().zip(&gb[..n - s]).map(|(x, y)| (x - y).abs()).sum();
        let e = err / (n - s) as f32;
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

/// Profile confidence at or above which the profile estimate overrides a
/// disagreeing Vision estimate.
pub const PROFILE_TRUSTED_CONFIDENCE: f32 = 0.5;
/// Disagreement (pixels) tolerated before the estimates count as different.
pub const SHIFT_TOLERANCE: i64 = 2;

/// Combine the Vision registration (`vision`, forward pixels, `None` when it
/// failed) with the profile estimate.
///
/// Vision wins when it reports movement and either agrees with the profile
/// (within [`SHIFT_TOLERANCE`]) or the profile is not confident. A confident
/// profile that disagrees wins: that is the case where Vision locked onto a
/// sticky header or a blank area. Without usable Vision movement the profile
/// estimate is used.
pub fn choose_shift(vision: Option<i64>, profile: Shift) -> i64 {
    match vision {
        Some(v) if v > 1 => {
            if profile.confidence >= PROFILE_TRUSTED_CONFIDENCE
                && profile.pixels > 1
                && (profile.pixels - v).abs() > SHIFT_TOLERANCE
            {
                profile.pixels
            } else {
                v
            }
        }
        _ => profile.pixels,
    }
}

/// Append the new strip of `next` to `canvas` given the shift.
///
/// This copies the whole canvas, so calling it once per frame is quadratic;
/// [`StripCanvas`] collects strips and assembles them once instead.
pub fn append_strip(canvas: &mut RgbaImage, next: &RgbaImage, shift: i64, horizontal: bool) {
    let Some(strip) = strip_of(next, shift, horizontal) else {
        return;
    };
    *canvas = assemble(canvas, std::slice::from_ref(&strip), horizontal);
}

/// The trailing `shift` pixels of `next` along the scroll axis (clamped to
/// the frame), or `None` when there is nothing to append.
fn strip_of(next: &RgbaImage, shift: i64, horizontal: bool) -> Option<RgbaImage> {
    let (fw, fh) = next.dimensions();
    if shift <= 0 || fw == 0 || fh == 0 {
        return None;
    }
    let len = u32::try_from(shift).unwrap_or(u32::MAX);
    Some(if horizontal {
        let strip_w = len.min(fw);
        next.view(fw - strip_w, 0, strip_w, fh).to_image()
    } else {
        let strip_h = len.min(fh);
        next.view(0, fh - strip_h, fw, strip_h).to_image()
    })
}

/// Lay `first` and `strips` end to end along the scroll axis in one pass.
fn assemble(first: &RgbaImage, strips: &[RgbaImage], horizontal: bool) -> RgbaImage {
    let along = |img: &RgbaImage| if horizontal { img.width() } else { img.height() };
    let across = |img: &RgbaImage| if horizontal { img.height() } else { img.width() };
    let total: u32 = strips.iter().map(along).fold(along(first), u32::saturating_add);
    let cross = strips.iter().map(across).fold(across(first), u32::max);
    let mut out = if horizontal { RgbaImage::new(total, cross) } else { RgbaImage::new(cross, total) };
    let mut pos = 0i64;
    for img in std::iter::once(first).chain(strips) {
        let (x, y) = if horizontal { (pos, 0) } else { (0, pos) };
        image::imageops::replace(&mut out, img, x, y);
        pos += along(img) as i64;
    }
    out
}

/// Accumulates the first frame and the new strip of every later frame, and
/// builds the stitched image once at the end (linear in the output size).
#[derive(Debug, Clone)]
pub struct StripCanvas {
    first: RgbaImage,
    strips: Vec<RgbaImage>,
    horizontal: bool,
    /// Length along the scroll axis, in pixels.
    len: u64,
    /// Extent across the scroll axis, in pixels.
    cross: u64,
}

impl StripCanvas {
    pub fn new(first: RgbaImage, horizontal: bool) -> Self {
        let (w, h) = first.dimensions();
        let (len, cross) = if horizontal { (w, h) } else { (h, w) };
        Self { first, strips: Vec::new(), horizontal, len: len as u64, cross: cross as u64 }
    }

    /// Pixel count of the canvas after appending a strip of `shift` pixels
    /// taken from a frame like `next` (clamped to the frame length).
    pub fn pixels_after(&self, next: &RgbaImage, shift: i64) -> u64 {
        let (fw, fh) = next.dimensions();
        let (flen, fcross) = if self.horizontal { (fw, fh) } else { (fh, fw) };
        let add = shift.clamp(0, flen as i64) as u64;
        (self.len + add) * self.cross.max(fcross as u64)
    }

    /// Current pixel count of the canvas.
    pub fn pixels(&self) -> u64 {
        self.len * self.cross
    }

    /// Queue the trailing `shift` pixels of `next`. Returns the number of
    /// pixels actually appended along the scroll axis.
    pub fn push(&mut self, next: &RgbaImage, shift: i64) -> i64 {
        let Some(strip) = strip_of(next, shift, self.horizontal) else {
            return 0;
        };
        let (sw, sh) = strip.dimensions();
        let (add, cross) = if self.horizontal { (sw, sh) } else { (sh, sw) };
        self.len += add as u64;
        self.cross = self.cross.max(cross as u64);
        self.strips.push(strip);
        add as i64
    }

    /// Build the stitched image.
    pub fn finish(self) -> RgbaImage {
        if self.strips.is_empty() {
            return self.first;
        }
        assemble(&self.first, &self.strips, self.horizontal)
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

    #[test]
    fn short_profiles_do_not_underflow() {
        // 8 rows -> 7 gradients, fewer than the minimum overlap of 8.
        for h in 0..=40 {
            let a = textured(16, h, 0);
            let b = textured(16, h, 3);
            let s = estimate_shift_profile(&a, &b, false);
            let t = estimate_shift_profile(&a, &b, true);
            if h <= 9 {
                assert_eq!(s, Shift { pixels: 0, confidence: 0.0 }, "h={h}");
            }
            assert!(s.pixels >= 0 && t.pixels >= 0);
        }
        let a = textured(16, 8, 0);
        assert_eq!(estimate_shift_profile(&a, &a, false), Shift { pixels: 0, confidence: 0.0 });
    }

    #[test]
    fn confident_profile_overrides_disagreeing_vision() {
        let confident = Shift { pixels: 120, confidence: 0.9 };
        let unsure = Shift { pixels: 120, confidence: 0.2 };
        // Vision locked onto a sticky header: far off, profile confident.
        assert_eq!(choose_shift(Some(40), confident), 120);
        // Small disagreement: Vision kept.
        assert_eq!(choose_shift(Some(118), confident), 118);
        assert_eq!(choose_shift(Some(122), confident), 122);
        // Profile not confident: Vision kept.
        assert_eq!(choose_shift(Some(40), unsure), 40);
        // Vision failed or saw no movement: profile used.
        assert_eq!(choose_shift(None, unsure), 120);
        assert_eq!(choose_shift(Some(0), confident), 120);
        // Confident profile that saw no movement does not override Vision.
        assert_eq!(choose_shift(Some(40), Shift { pixels: 0, confidence: 0.9 }), 40);
    }

    #[test]
    fn strip_canvas_assembles_once() {
        let page = textured(300, 1000, 0);
        let shifts = [123i64, 80, 200, 0, 50];
        let mut canvas = StripCanvas::new(textured(300, 400, 0), false);
        let mut reference = textured(300, 400, 0);
        let mut offset = 0u32;
        for &s in &shifts {
            offset += s as u32;
            let frame = textured(300, 400, offset);
            let before = canvas.pixels_after(&frame, s);
            assert_eq!(canvas.push(&frame, s), s);
            assert_eq!(canvas.pixels(), before);
            append_strip(&mut reference, &frame, s, false);
        }
        assert_eq!(canvas.pixels(), 300 * (400 + 453));
        let out = canvas.finish();
        assert_eq!(out.dimensions(), (300, 853));
        assert_eq!(out, reference);
        for y in [0, 399, 400, 522, 523, 852] {
            assert_eq!(out.get_pixel(17, y), page.get_pixel(17, y), "row {y}");
        }
    }

    #[test]
    fn strip_canvas_horizontal_and_oversized_shift() {
        let mut canvas = StripCanvas::new(textured(100, 50, 0), true);
        // A shift larger than the frame appends the whole frame.
        assert_eq!(canvas.push(&textured(100, 50, 0), 500), 100);
        assert_eq!(canvas.push(&textured(100, 50, 0), -3), 0);
        let out = canvas.finish();
        assert_eq!(out.dimensions(), (200, 50));
    }

    #[test]
    fn localized_change_is_not_a_stall() {
        let a = RgbaImage::from_pixel(400, 400, image::Rgba([255, 255, 255, 255]));
        let mut b = a.clone();
        // A 20x20 block change on otherwise identical frames, blue channel only
        // and at odd coordinates so a strided R/G sample would miss it.
        for y in 101..121 {
            for x in 101..121 {
                b.put_pixel(x, y, image::Rgba([255, 255, 0, 255]));
            }
        }
        let d = frame_difference(&a, &b);
        assert!(d > 0.0, "{d}");
        assert_eq!(frame_difference(&a, &a), 0.0);
        assert_eq!(frame_difference(&a, &RgbaImage::new(10, 10)), 255.0);
    }
}
