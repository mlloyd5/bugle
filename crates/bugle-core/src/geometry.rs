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

    /// Parse `x,y,w,h` or `x,y,wxh`-style strings used on the CLI.
    pub fn parse(s: &str) -> Option<Rect> {
        let parts: Vec<f64> = s
            .split(|c| c == ',' || c == 'x' || c == ' ')
            .filter(|p| !p.is_empty())
            .map(|p| p.trim().parse::<f64>())
            .collect::<Result<_, _>>()
            .ok()?;
        if parts.len() == 4 {
            Some(Rect::new(parts[0], parts[1], parts[2], parts[3]))
        } else {
            None
        }
    }
}

impl std::fmt::Display for Rect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{},{},{},{}", self.x, self.y, self.w, self.h)
    }
}
