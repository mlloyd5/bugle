use crate::color::Color;
use bugle_core::{Point, Rect};
use serde::{Deserialize, Serialize};

/// Per-shape style. Every field is optional; unset fields fall back to the
/// document [`Style`] and then to built-in defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Style {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Color>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Color>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// 0-255 opacity multiplier applied to colour and fill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<u8>,
}

impl Style {
    pub fn merged(&self, base: &Style) -> Style {
        Style {
            color: self.color.or(base.color),
            fill: self.fill.or(base.fill),
            stroke_width: self.stroke_width.or(base.stroke_width),
            font_size: self.font_size.or(base.font_size),
            opacity: self.opacity.or(base.opacity),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ArrowStyle {
    #[default]
    Standard,
    Double,
    /// Thin shaft, open head.
    Slim,
    /// Tapered shaft.
    Fancy,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RedactStyle {
    #[default]
    Pixelate,
    Blur,
    BlackOut,
    /// Pixelate → blur → pixelate; cannot be reversed by sharpening.
    SecureBlur,
}

/// One drawing object. Coordinates are image pixels, top-left origin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Shape {
    Rectangle {
        rect: Rect,
        #[serde(default)]
        radius: f32,
        #[serde(default, flatten)]
        style: Style,
    },
    Ellipse {
        rect: Rect,
        #[serde(default, flatten)]
        style: Style,
    },
    Line {
        from: Point,
        to: Point,
        #[serde(default, flatten)]
        style: Style,
    },
    Arrow {
        from: Point,
        to: Point,
        #[serde(default)]
        arrow: ArrowStyle,
        #[serde(default, flatten)]
        style: Style,
    },
    Freehand {
        points: Vec<Point>,
        #[serde(default, flatten)]
        style: Style,
    },
    Text {
        at: Point,
        text: String,
        /// Optional pill background behind the text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        background: Option<Color>,
        #[serde(default, flatten)]
        style: Style,
    },
    /// Translucent marker over a rect.
    Highlight {
        rect: Rect,
        #[serde(default, flatten)]
        style: Style,
    },
    /// Numbered step badge. `number` auto-increments when omitted.
    Counter {
        at: Point,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        number: Option<u32>,
        #[serde(default, flatten)]
        style: Style,
    },
    /// Hide sensitive content.
    Redact {
        rect: Rect,
        #[serde(default)]
        mode: RedactStyle,
        /// Block size for pixelate / sigma for blur. 0 = auto.
        #[serde(default)]
        strength: f32,
    },
    /// Darken everything outside `rect`.
    Spotlight {
        rect: Rect,
        /// 0..1 — how dark the surroundings get.
        #[serde(default = "default_dim")]
        dim: f32,
    },
    /// Paste another image, e.g. a logo or a cursor.
    Image {
        at: Point,
        path: String,
        #[serde(default = "default_one")]
        scale: f32,
    },
}

fn default_dim() -> f32 {
    0.6
}
fn default_one() -> f32 {
    1.0
}

/// Background behind the screenshot when `padding > 0`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Background {
    /// `"none"`, a colour, a preset name (`sunset`, `ocean`, `forest`,
    /// `candy`, `mono`, `midnight`) or `gradient:#a,#b`.
    Named(String),
    Gradient {
        from: Color,
        to: Color,
        #[serde(default)]
        angle: f32,
    },
}

impl Default for Background {
    fn default() -> Self {
        Background::Named("none".into())
    }
}

impl Background {
    pub fn preset(name: &str) -> Option<(Color, Color, f32)> {
        let (a, b) = match name {
            "sunset" => ("#FF9A8B", "#FF6A88"),
            "ocean" => ("#2E3192", "#1BFFFF"),
            "forest" => ("#134E5E", "#71B280"),
            "candy" => ("#A18CD1", "#FBC2EB"),
            "mono" => ("#2C2C2E", "#1C1C1E"),
            "midnight" => ("#0F2027", "#2C5364"),
            "peach" => ("#FFECD2", "#FCB69F"),
            "mint" => ("#D4FC79", "#96E6A1"),
            _ => return None,
        };
        Some((Color::parse(a)?, Color::parse(b)?, 135.0))
    }

    pub fn resolve(&self) -> Option<(Color, Color, f32)> {
        match self {
            Background::Gradient { from, to, angle } => Some((*from, *to, *angle)),
            Background::Named(n) => {
                let n = n.trim();
                if n.eq_ignore_ascii_case("none") || n.is_empty() {
                    return None;
                }
                if let Some(rest) = n.strip_prefix("gradient:") {
                    let mut it = rest.split(',');
                    let a = Color::parse(it.next()?)?;
                    let b = Color::parse(it.next()?)?;
                    let angle = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(135.0);
                    return Some((a, b, angle));
                }
                if let Some(p) = Self::preset(&n.to_ascii_lowercase()) {
                    return Some(p);
                }
                let c = Color::parse(n)?;
                Some((c, c, 0.0))
            }
        }
    }
}

/// Canvas treatment applied after the shapes: padding, background,
/// rounded corners and drop shadow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Canvas {
    pub padding: u32,
    pub background: Background,
    pub corner_radius: u32,
    pub shadow: bool,
    /// Extra solid colour inset around the screenshot (a "frame").
    pub border: Option<Color>,
    pub border_width: u32,
}

impl Default for Canvas {
    fn default() -> Self {
        Self { padding: 48, background: Background::default(), corner_radius: 12, shadow: true, border: None, border_width: 0 }
    }
}

/// A complete annotation document.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Annotation {
    /// Crop applied before anything is drawn (shape coordinates are relative
    /// to the cropped image).
    pub crop: Option<Rect>,
    /// Document-wide style defaults.
    pub style: Style,
    pub shapes: Vec<Shape>,
    pub canvas: Option<Canvas>,
    /// Scale factor applied to the final image (0.5 to downscale Retina).
    pub scale: Option<f32>,
}

impl Annotation {
    pub fn from_json(s: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(s)?)
    }

    /// Short description of the document for history / logs.
    pub fn summary(&self) -> String {
        let mut counts = std::collections::BTreeMap::<&str, usize>::new();
        for s in &self.shapes {
            *counts.entry(s.kind()).or_default() += 1;
        }
        let mut parts: Vec<String> = counts.into_iter().map(|(k, v)| format!("{v} {k}")).collect();
        if self.crop.is_some() {
            parts.push("crop".into());
        }
        if self.canvas.is_some() {
            parts.push("canvas".into());
        }
        parts.join(", ")
    }
}

impl Shape {
    pub fn kind(&self) -> &'static str {
        match self {
            Shape::Rectangle { .. } => "rectangle",
            Shape::Ellipse { .. } => "ellipse",
            Shape::Line { .. } => "line",
            Shape::Arrow { .. } => "arrow",
            Shape::Freehand { .. } => "freehand",
            Shape::Text { .. } => "text",
            Shape::Highlight { .. } => "highlight",
            Shape::Counter { .. } => "counter",
            Shape::Redact { .. } => "redact",
            Shape::Spotlight { .. } => "spotlight",
            Shape::Image { .. } => "image",
        }
    }
}

/// JSON Schema-ish description of the document, served to agents so they can
/// author annotations without reading Rust.
pub fn schema_description() -> serde_json::Value {
    serde_json::json!({
        "crop": {"x":0,"y":0,"w":0,"h":0, "_doc": "optional, pixels; applied first"},
        "style": {"color":"#FF3B30","fill":null,"stroke_width":4,"font_size":28,"opacity":255},
        "scale": 1.0,
        "canvas": {"padding":48,"background":"sunset | #hex | gradient:#a,#b | none","corner_radius":12,"shadow":true},
        "shapes": [
            {"type":"rectangle","rect":{"x":0,"y":0,"w":100,"h":50},"radius":6,"color":"red","fill":null},
            {"type":"ellipse","rect":{"x":0,"y":0,"w":100,"h":50}},
            {"type":"line","from":{"x":0,"y":0},"to":{"x":100,"y":100}},
            {"type":"arrow","from":{"x":0,"y":0},"to":{"x":100,"y":100},"arrow":"standard | double | slim | fancy"},
            {"type":"freehand","points":[{"x":0,"y":0}]},
            {"type":"text","at":{"x":0,"y":0},"text":"Hello","background":"#000000AA","font_size":28,"color":"white"},
            {"type":"highlight","rect":{"x":0,"y":0,"w":100,"h":20},"color":"yellow"},
            {"type":"counter","at":{"x":0,"y":0},"number":1},
            {"type":"redact","rect":{"x":0,"y":0,"w":100,"h":20},"mode":"pixelate | blur | black_out | secure_blur","strength":0},
            {"type":"spotlight","rect":{"x":0,"y":0,"w":100,"h":20},"dim":0.6},
            {"type":"image","at":{"x":0,"y":0},"path":"/path/logo.png","scale":1.0}
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_style() {
        let j = r##"{"shapes":[{"type":"arrow","from":{"x":1,"y":2},"to":{"x":3,"y":4},"color":"#00FF00","stroke_width":6}]}"##;
        let a = Annotation::from_json(j).unwrap();
        match &a.shapes[0] {
            Shape::Arrow { style, .. } => {
                assert_eq!(style.color, Some(Color([0, 255, 0, 255])));
                assert_eq!(style.stroke_width, Some(6.0));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn background_presets() {
        assert!(Background::Named("sunset".into()).resolve().is_some());
        assert!(Background::Named("none".into()).resolve().is_none());
        assert!(Background::Named("gradient:#fff,#000,90".into()).resolve().is_some());
    }
}
