use serde::{Deserialize, Serialize};

/// A point in global screen coordinates (points, not pixels; origin top-left).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

/// Axis-aligned rectangle. Screen rects are in points with a top-left origin
/// (Cocoa's flipped coordinate system is handled inside the platform shim).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn origin(&self) -> Point {
        Point { x: self.x, y: self.y }
    }

    pub fn size(&self) -> Size {
        Size { w: self.w, h: self.h }
    }

    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }

    pub fn center(&self) -> Point {
        Point { x: self.x + self.w / 2.0, y: self.y + self.h / 2.0 }
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.y >= self.y && p.x < self.right() && p.y < self.bottom()
    }

    pub fn intersection(&self, o: &Rect) -> Option<Rect> {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        if r > x && b > y {
            Some(Rect::new(x, y, r - x, b - y))
        } else {
            None
        }
    }

    pub fn scaled(&self, s: f64) -> Rect {
        Rect::new(self.x * s, self.y * s, self.w * s, self.h * s)
    }

    /// All four values are finite and the size is positive.
    pub fn is_valid(&self) -> bool {
        [self.x, self.y, self.w, self.h].iter().all(|v| v.is_finite()) && !self.is_empty()
    }

    /// Parse the `x,y,w,h` or `x,y,WxH` strings used on the CLI (commas or
    /// whitespace separate the fields). Only finite decimal numbers are
    /// accepted and the result must satisfy [`Rect::is_valid`].
    pub fn parse(s: &str) -> Option<Rect> {
        let num = |p: &str| p.trim().parse::<f64>().ok().filter(|v| v.is_finite());
        let fields: Vec<&str> = s.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty()).collect();
        let r = match fields.as_slice() {
            [x, y, w, h] => Rect::new(num(x)?, num(y)?, num(w)?, num(h)?),
            [x, y, size] => {
                let (w, h) = size.split_once(['x', 'X'])?;
                Rect::new(num(x)?, num(y)?, num(w)?, num(h)?)
            }
            _ => return None,
        };
        r.is_valid().then_some(r)
    }
}

impl std::fmt::Display for Rect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{},{},{},{}", self.x, self.y, self.w, self.h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_both_forms() {
        let want = Some(Rect::new(10.0, -20.0, 300.0, 200.5));
        assert_eq!(Rect::parse("10,-20,300,200.5"), want);
        assert_eq!(Rect::parse(" 10, -20, 300, 200.5 "), want);
        assert_eq!(Rect::parse("10 -20 300 200.5"), want);
        assert_eq!(Rect::parse("10,-20,300x200.5"), want);
        assert_eq!(Rect::parse("10,-20,300X200.5"), want);
        assert_eq!(Rect::parse("0,0,1e3,1e2"), Some(Rect::new(0.0, 0.0, 1000.0, 100.0)));
    }

    #[test]
    fn parse_rejects_non_finite_and_malformed() {
        for s in [
            "nan,0,10,10",
            "0,0,inf,10",
            "0,0,10,-inf",
            "0,0,infinity,10",
            "0x10,0,10,10",
            "0,0,0x10,10",
            "0,0,10x10x10",
            "0,0,10",
            "1,2,3,4,5",
            "",
            "0,0,0,10",
            "0,0,10,-5",
            "a,b,c,d",
        ] {
            assert_eq!(Rect::parse(s), None, "{s:?}");
        }
    }

    #[test]
    fn is_valid() {
        assert!(Rect::new(-5.0, 0.0, 1.0, 1.0).is_valid());
        assert!(!Rect::new(0.0, 0.0, 0.0, 1.0).is_valid());
        assert!(!Rect::new(f64::NAN, 0.0, 1.0, 1.0).is_valid());
        assert!(!Rect::new(0.0, 0.0, f64::INFINITY, 1.0).is_valid());
        assert!(!Rect::new(0.0, 0.0, 1.0, f64::NAN).is_valid());
    }
}
