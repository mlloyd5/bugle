use crate::color::Color;
use bugle_core::{BugleError, Point, Rect};
use serde::{Deserialize, Serialize};
use std::fmt::Display;

/// Limits enforced by [`Annotation::validate`] and
/// [`Annotation::output_size`]. Documents usually come from API callers, so
/// every number is bounded before anything is allocated or drawn.
pub mod limits {
    /// Largest `canvas.padding` and `canvas.border_width`, in pixels.
    pub const MAX_CANVAS_INSET: u32 = 2048;
    /// Largest image the renderer will produce (after crop, canvas and scale)
    /// or load for an `image` shape, in pixels.
    pub const MAX_PIXELS: u64 = 100_000_000;
    /// Allowed range for the document `scale` and for `image.scale`.
    pub const MIN_SCALE: f32 = 0.05;
    pub const MAX_SCALE: f32 = 8.0;
    /// Largest blur sigma for `redact` in `blur` mode.
    pub const MAX_BLUR_SIGMA: f32 = 100.0;
    /// Largest block size for `redact` in the other modes.
    pub const MAX_PIXELATE_BLOCK: f32 = 4096.0;
    /// Allowed `font_size` range.
    pub const MIN_FONT_SIZE: f32 = 4.0;
    pub const MAX_FONT_SIZE: f32 = 512.0;
    /// Largest `stroke_width` (0 is allowed).
    pub const MAX_STROKE_WIDTH: f32 = 256.0;
    /// Largest absolute value of any coordinate, size or corner radius.
    pub const MAX_COORD: f64 = 10_000_000.0;
    /// Most shapes in one document.
    pub const MAX_SHAPES: usize = 10_000;
    /// Most freehand points across the whole document.
    pub const MAX_FREEHAND_POINTS: usize = 100_000;
    /// Longest `text`, in characters.
    pub const MAX_TEXT_CHARS: usize = 10_000;
}

/// A validation error naming the offending field. It is a
/// [`BugleError::Invalid`], which the API maps to HTTP 400.
pub(crate) fn invalid(field: impl Display, msg: impl Display) -> anyhow::Error {
    BugleError::Invalid(format!("annotation {field}: {msg}")).into()
}

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
    /// Outlined rectangle, optionally filled, with optional rounded corners.
    Rectangle {
        rect: Rect,
        #[serde(default)]
        radius: f32,
        #[serde(default, flatten)]
        style: Style,
    },
    /// Ellipse inscribed in `rect`, outlined and optionally filled.
    Ellipse {
        rect: Rect,
        #[serde(default, flatten)]
        style: Style,
    },
    /// Straight line segment.
    Line {
        from: Point,
        to: Point,
        #[serde(default, flatten)]
        style: Style,
    },
    /// Line with an arrow head at `to` (and at `from` for the double style).
    Arrow {
        from: Point,
        to: Point,
        #[serde(default)]
        arrow: ArrowStyle,
        #[serde(default, flatten)]
        style: Style,
    },
    /// Pen stroke through a list of points.
    Freehand {
        points: Vec<Point>,
        #[serde(default, flatten)]
        style: Style,
    },
    /// One or more lines of text, top-left anchored at `at`.
    Text {
        at: Point,
        text: String,
        /// Optional pill background behind the text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        background: Option<Color>,
        #[serde(default, flatten)]
        style: Style,
    },
    /// Marker-pen highlight: multiplies the pixels under `rect` by the colour,
    /// so text stays legible. The colour's alpha and `opacity` set the strength.
    Highlight {
        rect: Rect,
        #[serde(default, flatten)]
        style: Style,
    },
    /// Numbered step badge: a filled circle with a number in it. `number`
    /// auto-increments from the previous badge when omitted.
    Counter {
        at: Point,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        number: Option<u32>,
        #[serde(default, flatten)]
        style: Style,
    },
    /// Hide sensitive content under `rect` by pixelating, blurring or painting
    /// over it.
    Redact {
        rect: Rect,
        #[serde(default)]
        mode: RedactStyle,
        /// Block size for pixelate / sigma for blur (at most 100). 0 = auto.
        #[serde(default)]
        strength: f32,
    },
    /// Darken everything outside `rect` to draw attention to it.
    Spotlight {
        rect: Rect,
        /// 0..1 — how dark the surroundings get.
        #[serde(default = "default_dim")]
        dim: f32,
    },
    /// Paste another image file (a logo, a cursor) with its top-left corner at
    /// `at`, resized by `scale`.
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
/// rounded corners, an optional solid frame and a drop shadow.
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

impl Annotation {
    /// Check every field against [`limits`]: numbers must be finite and in
    /// range, and the document must not be too large. Errors name the field
    /// (for example `shapes[2].stroke_width`) and are [`BugleError::Invalid`].
    ///
    /// The output size also depends on the input image; see
    /// [`Annotation::output_size`].
    pub fn validate(&self) -> anyhow::Result<()> {
        use limits::*;
        if let Some(r) = &self.crop {
            check_rect("crop", r)?;
        }
        check_style("style", &self.style)?;
        if let Some(s) = self.scale {
            check_range("scale", s, MIN_SCALE, MAX_SCALE)?;
        }
        if let Some(c) = &self.canvas {
            if c.padding > MAX_CANVAS_INSET {
                return Err(invalid("canvas.padding", format_args!("{} is larger than {MAX_CANVAS_INSET}", c.padding)));
            }
            if c.border_width > MAX_CANVAS_INSET {
                return Err(invalid(
                    "canvas.border_width",
                    format_args!("{} is larger than {MAX_CANVAS_INSET}", c.border_width),
                ));
            }
            if let Some((_, _, angle)) = c.background.resolve() {
                if !angle.is_finite() {
                    return Err(invalid("canvas.background", "gradient angle must be a finite number"));
                }
            }
        }
        if self.shapes.len() > MAX_SHAPES {
            return Err(invalid("shapes", format_args!("{} shapes, at most {MAX_SHAPES} allowed", self.shapes.len())));
        }
        let mut points = 0usize;
        for (i, shape) in self.shapes.iter().enumerate() {
            let field = |f: &str| format!("shapes[{i}].{f}");
            let prefix = format!("shapes[{i}]");
            match shape {
                Shape::Rectangle { rect, radius, style } => {
                    check_rect(&field("rect"), rect)?;
                    check_range(&field("radius"), *radius, 0.0, MAX_COORD as f32)?;
                    check_style(&prefix, style)?;
                }
                Shape::Ellipse { rect, style } | Shape::Highlight { rect, style } => {
                    check_rect(&field("rect"), rect)?;
                    check_style(&prefix, style)?;
                }
                Shape::Line { from, to, style } | Shape::Arrow { from, to, style, .. } => {
                    check_point(&field("from"), from)?;
                    check_point(&field("to"), to)?;
                    check_style(&prefix, style)?;
                }
                Shape::Freehand { points: pts, style } => {
                    points = points.saturating_add(pts.len());
                    if points > MAX_FREEHAND_POINTS {
                        return Err(invalid(
                            field("points"),
                            format_args!("more than {MAX_FREEHAND_POINTS} freehand points in the document"),
                        ));
                    }
                    for (j, p) in pts.iter().enumerate() {
                        check_point(&format!("shapes[{i}].points[{j}]"), p)?;
                    }
                    check_style(&prefix, style)?;
                }
                Shape::Text { at, text, style, .. } => {
                    check_point(&field("at"), at)?;
                    let n = text.chars().count();
                    if n > MAX_TEXT_CHARS {
                        return Err(invalid(field("text"), format_args!("{n} characters, at most {MAX_TEXT_CHARS} allowed")));
                    }
                    check_style(&prefix, style)?;
                }
                Shape::Counter { at, style, .. } => {
                    check_point(&field("at"), at)?;
                    check_style(&prefix, style)?;
                }
                Shape::Redact { rect, mode, strength } => {
                    check_rect(&field("rect"), rect)?;
                    let max = if *mode == RedactStyle::Blur { MAX_BLUR_SIGMA } else { MAX_PIXELATE_BLOCK };
                    check_range(&field("strength"), *strength, 0.0, max)?;
                }
                Shape::Spotlight { rect, dim } => {
                    check_rect(&field("rect"), rect)?;
                    check_range(&field("dim"), *dim, 0.0, 1.0)?;
                }
                Shape::Image { at, path, scale } => {
                    check_point(&field("at"), at)?;
                    if path.trim().is_empty() {
                        return Err(invalid(field("path"), "must not be empty"));
                    }
                    check_range(&field("scale"), *scale, MIN_SCALE, MAX_SCALE)?;
                }
            }
        }
        Ok(())
    }

    /// Size of the rendered image for an input of `width` x `height` pixels
    /// (measured after `crop`, which the renderer applies first). Errors when
    /// the image after canvas padding, or after `scale`, would exceed
    /// [`limits::MAX_PIXELS`]. Uses checked arithmetic, so it is safe on a
    /// document that has not been validated.
    pub fn output_size(&self, width: u32, height: u32) -> anyhow::Result<(u32, u32)> {
        let (w, h) = match &self.canvas {
            Some(c) => canvas_size(width, height, c)?,
            None => (width, height),
        };
        check_pixels("canvas", w, h)?;
        match self.scale {
            Some(s) if s > 0.0 && (s - 1.0).abs() > 1e-3 => {
                let (w, h) = scaled_size(w, h, s).ok_or_else(|| invalid("scale", "output image is too large"))?;
                check_pixels("scale", w, h)?;
                Ok((w, h))
            }
            _ => Ok((w, h)),
        }
    }
}

/// Size of the canvas around a `width` x `height` image.
pub(crate) fn canvas_size(width: u32, height: u32, c: &Canvas) -> anyhow::Result<(u32, u32)> {
    let grow = c.padding.checked_add(c.border_width).and_then(|v| v.checked_mul(2));
    let size = grow.and_then(|g| Some((width.checked_add(g)?, height.checked_add(g)?)));
    size.ok_or_else(|| invalid("canvas", "padding and border make the image too large"))
}

/// `width` x `height` multiplied by `s`, rounded and at least 1x1; `None`
/// when the result does not fit in `u32` or `s` is not finite.
pub(crate) fn scaled_size(width: u32, height: u32, s: f32) -> Option<(u32, u32)> {
    let f = |v: u32| {
        let r = (v as f64 * s as f64).round().max(1.0);
        (r.is_finite() && r <= u32::MAX as f64).then_some(r as u32)
    };
    Some((f(width)?, f(height)?))
}

/// Error naming `field` when `w` x `h` exceeds [`limits::MAX_PIXELS`].
pub(crate) fn check_pixels(field: &str, w: u32, h: u32) -> anyhow::Result<()> {
    if w as u64 * h as u64 > limits::MAX_PIXELS {
        return Err(invalid(
            field,
            format_args!("image would be {w}x{h} pixels, more than {} megapixels", limits::MAX_PIXELS / 1_000_000),
        ));
    }
    Ok(())
}

fn check_range(field: &str, v: f32, min: f32, max: f32) -> anyhow::Result<()> {
    if !v.is_finite() {
        return Err(invalid(field, "must be a finite number"));
    }
    if v < min || v > max {
        return Err(invalid(field, format_args!("{v} is outside {min}..={max}")));
    }
    Ok(())
}

fn check_coord(field: &str, v: f64) -> anyhow::Result<()> {
    if !v.is_finite() {
        return Err(invalid(field, "must be a finite number"));
    }
    if v.abs() > limits::MAX_COORD {
        return Err(invalid(field, format_args!("{v} is larger than {}", limits::MAX_COORD)));
    }
    Ok(())
}

fn check_point(field: &str, p: &Point) -> anyhow::Result<()> {
    check_coord(&format!("{field}.x"), p.x)?;
    check_coord(&format!("{field}.y"), p.y)
}

fn check_rect(field: &str, r: &Rect) -> anyhow::Result<()> {
    check_coord(&format!("{field}.x"), r.x)?;
    check_coord(&format!("{field}.y"), r.y)?;
    check_coord(&format!("{field}.w"), r.w)?;
    check_coord(&format!("{field}.h"), r.h)
}

fn check_style(prefix: &str, st: &Style) -> anyhow::Result<()> {
    if let Some(w) = st.stroke_width {
        check_range(&format!("{prefix}.stroke_width"), w, 0.0, limits::MAX_STROKE_WIDTH)?;
    }
    if let Some(f) = st.font_size {
        check_range(&format!("{prefix}.font_size"), f, limits::MIN_FONT_SIZE, limits::MAX_FONT_SIZE)?;
    }
    Ok(())
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
    use limits::*;
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
            {"type":"highlight","rect":{"x":0,"y":0,"w":100,"h":20},"color":"yellow","_doc":"colour alpha and opacity set the strength"},
            {"type":"counter","at":{"x":0,"y":0},"number":1},
            {"type":"redact","rect":{"x":0,"y":0,"w":100,"h":20},"mode":"pixelate | blur | black_out | secure_blur","strength":0},
            {"type":"spotlight","rect":{"x":0,"y":0,"w":100,"h":20},"dim":0.6},
            {"type":"image","at":{"x":0,"y":0},"path":"/path/logo.png","scale":1.0}
        ],
        "_limits": {
            "scale": format!("{}..={} (document and image)", MIN_SCALE, MAX_SCALE),
            "canvas.padding, canvas.border_width": format!("<= {MAX_CANVAS_INSET}"),
            "output_and_image_pixels": format!("<= {MAX_PIXELS} after crop, canvas and scale"),
            "stroke_width": format!("0..={MAX_STROKE_WIDTH}"),
            "font_size": format!("{MIN_FONT_SIZE}..={MAX_FONT_SIZE}"),
            "redact.strength": format!("0..={MAX_BLUR_SIGMA} for blur, 0..={MAX_PIXELATE_BLOCK} otherwise"),
            "spotlight.dim": "0..=1",
            "coordinates": format!("finite, |v| <= {MAX_COORD}"),
            "shapes": format!("<= {MAX_SHAPES}"),
            "freehand_points": format!("<= {MAX_FREEHAND_POINTS} in total"),
            "text": format!("<= {MAX_TEXT_CHARS} characters")
        }
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

    /// Validate `doc` and return the error message, which must be a
    /// `BugleError::Invalid`.
    fn rejection(doc: serde_json::Value) -> String {
        let a: Annotation = serde_json::from_value(doc).unwrap();
        let err = a.validate().expect_err("document should be rejected");
        match err.downcast_ref::<BugleError>() {
            Some(BugleError::Invalid(m)) => m.clone(),
            other => panic!("expected BugleError::Invalid, got {other:?}"),
        }
    }

    fn rejects(doc: serde_json::Value, field: &str) {
        let msg = rejection(doc);
        assert!(msg.contains(field), "error {msg:?} should name {field}");
    }

    fn shape(s: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "shapes": [s] })
    }

    #[test]
    fn accepts_documented_examples() {
        // The README example.
        let readme: Annotation = serde_json::from_value(serde_json::json!({
          "crop": {"x": 0, "y": 0, "w": 800, "h": 500},
          "style": {"color": "blue", "stroke_width": 5},
          "shapes": [
            {"type": "ellipse",  "rect": {"x": 50, "y": 50, "w": 200, "h": 120}, "fill": "#00FF0040"},
            {"type": "arrow",    "from": {"x": 300, "y": 400}, "to": {"x": 150, "y": 180}, "arrow": "fancy", "color": "red"},
            {"type": "text",     "at": {"x": 320, "y": 420}, "text": "Fancy arrow", "background": "#000000B0", "color": "white"},
            {"type": "counter",  "at": {"x": 100, "y": 100}},
            {"type": "redact",   "rect": {"x": 500, "y": 300, "w": 200, "h": 80}, "mode": "secure_blur"},
            {"type": "spotlight","rect": {"x": 40, "y": 40, "w": 300, "h": 200}, "dim": 0.5}
          ],
          "canvas": {"padding": 40, "background": "gradient:#1e3c72,#2a5298", "corner_radius": 16, "shadow": true},
          "scale": 1.0
        }))
        .unwrap();
        readme.validate().unwrap();
        // Every shape in the schema example, with the placeholders filled in.
        let mut schema = schema_description();
        for s in schema["shapes"].as_array_mut().unwrap() {
            for (k, v) in [("arrow", "fancy"), ("mode", "blur")] {
                if s.get(k).is_some() {
                    s[k] = v.into();
                }
            }
        }
        schema["canvas"]["background"] = "sunset".into();
        let schema: Annotation = serde_json::from_value(schema).unwrap();
        assert_eq!(schema.shapes.len(), 11);
        schema.validate().unwrap();
        // The docs/AGENTS.md curl example.
        let agents: Annotation = serde_json::from_value(serde_json::json!({
            "shapes":[{"type":"arrow","from":{"x":10,"y":10},"to":{"x":300,"y":200}}],
            "canvas":{"padding":32,"background":"mono"}
        }))
        .unwrap();
        agents.validate().unwrap();
        Annotation::default().validate().unwrap();
        // Negative sizes were always accepted (they draw nothing).
        let neg: Annotation =
            serde_json::from_value(shape(serde_json::json!({"type":"rectangle","rect":{"x":50,"y":50,"w":-10,"h":-10}}))).unwrap();
        neg.validate().unwrap();
    }

    #[test]
    fn rejects_canvas_limits() {
        rejects(serde_json::json!({"canvas":{"padding":2049}}), "canvas.padding");
        rejects(serde_json::json!({"canvas":{"padding":u32::MAX}}), "canvas.padding");
        rejects(serde_json::json!({"canvas":{"padding":0,"border_width":4_000_000_000u32}}), "canvas.border_width");
        rejects(serde_json::json!({"canvas":{"background":"gradient:#fff,#000,NaN"}}), "canvas.background");
        let ok: Annotation = serde_json::from_value(serde_json::json!({"canvas":{"padding":2048,"border_width":2048}})).unwrap();
        ok.validate().unwrap();
    }

    #[test]
    fn rejects_scale() {
        rejects(serde_json::json!({"scale":0.0}), "scale");
        rejects(serde_json::json!({"scale":-1.0}), "scale");
        rejects(serde_json::json!({"scale":8.5}), "scale");
        rejects(serde_json::json!({"scale":1e30}), "scale");
        let a = Annotation { scale: Some(f32::NAN), ..Default::default() };
        assert!(a.validate().unwrap_err().to_string().contains("scale"));
        let a = Annotation { scale: Some(f32::INFINITY), ..Default::default() };
        assert!(a.validate().is_err());
    }

    #[test]
    fn rejects_style_numbers() {
        rejects(serde_json::json!({"style":{"font_size":2}}), "style.font_size");
        rejects(serde_json::json!({"style":{"font_size":513}}), "style.font_size");
        rejects(serde_json::json!({"style":{"stroke_width":-1}}), "style.stroke_width");
        rejects(serde_json::json!({"style":{"stroke_width":257}}), "style.stroke_width");
        rejects(
            shape(serde_json::json!({"type":"line","from":{"x":0,"y":0},"to":{"x":1,"y":1},"stroke_width":1e9})),
            "shapes[0].stroke_width",
        );
        rejects(
            shape(serde_json::json!({"type":"text","at":{"x":0,"y":0},"text":"x","font_size":100000})),
            "shapes[0].font_size",
        );
        let mut a = Annotation::default();
        a.style.font_size = Some(f32::NAN);
        assert!(a.validate().unwrap_err().to_string().contains("finite"));
    }

    #[test]
    fn rejects_shape_numbers() {
        rejects(
            shape(serde_json::json!({"type":"rectangle","rect":{"x":0,"y":0,"w":10,"h":10},"radius":-1})),
            "shapes[0].radius",
        );
        rejects(shape(serde_json::json!({"type":"ellipse","rect":{"x":0,"y":0,"w":1e12,"h":10}})), "shapes[0].rect.w");
        rejects(shape(serde_json::json!({"type":"spotlight","rect":{"x":0,"y":0,"w":10,"h":10},"dim":1.5})), "shapes[0].dim");
        rejects(shape(serde_json::json!({"type":"spotlight","rect":{"x":0,"y":0,"w":10,"h":10},"dim":-0.1})), "shapes[0].dim");
        rejects(
            shape(serde_json::json!({"type":"redact","rect":{"x":0,"y":0,"w":10,"h":10},"mode":"blur","strength":101})),
            "shapes[0].strength",
        );
        rejects(
            shape(serde_json::json!({"type":"redact","rect":{"x":0,"y":0,"w":10,"h":10},"mode":"pixelate","strength":-3})),
            "shapes[0].strength",
        );
        rejects(
            shape(serde_json::json!({"type":"redact","rect":{"x":0,"y":0,"w":10,"h":10},"strength":5000})),
            "shapes[0].strength",
        );
        rejects(shape(serde_json::json!({"type":"image","at":{"x":0,"y":0},"path":"/x.png","scale":9})), "shapes[0].scale");
        rejects(shape(serde_json::json!({"type":"image","at":{"x":0,"y":0},"path":"/x.png","scale":0})), "shapes[0].scale");
        rejects(shape(serde_json::json!({"type":"image","at":{"x":0,"y":0},"path":" "})), "shapes[0].path");
        rejects(serde_json::json!({"crop":{"x":0,"y":-2e7,"w":10,"h":10}}), "crop.y");
        let a = Annotation {
            shapes: vec![Shape::Counter { at: Point { x: f64::NAN, y: 0.0 }, number: None, style: Style::default() }],
            ..Default::default()
        };
        assert!(a.validate().unwrap_err().to_string().contains("shapes[0].at.x"));
        let a = Annotation {
            shapes: vec![Shape::Arrow {
                from: Point { x: 0.0, y: 0.0 },
                to: Point { x: 0.0, y: f64::INFINITY },
                arrow: ArrowStyle::Standard,
                style: Style::default(),
            }],
            ..Default::default()
        };
        assert!(a.validate().unwrap_err().to_string().contains("shapes[0].to.y"));
    }

    #[test]
    fn rejects_oversized_documents() {
        let line = Shape::Line { from: Point { x: 0.0, y: 0.0 }, to: Point { x: 1.0, y: 1.0 }, style: Style::default() };
        let mut a = Annotation { shapes: vec![line; limits::MAX_SHAPES], ..Default::default() };
        a.validate().unwrap();
        a.shapes.push(a.shapes[0].clone());
        assert!(a.validate().unwrap_err().to_string().contains("shapes"));

        // The point budget is shared across freehand shapes.
        let half = limits::MAX_FREEHAND_POINTS / 2;
        let pen = |n: usize| Shape::Freehand { points: vec![Point { x: 1.0, y: 1.0 }; n], style: Style::default() };
        let a = Annotation { shapes: vec![pen(half), pen(half)], ..Default::default() };
        a.validate().unwrap();
        let a = Annotation { shapes: vec![pen(half), pen(half + 1)], ..Default::default() };
        assert!(a.validate().unwrap_err().to_string().contains("shapes[1].points"));

        let text = "x".repeat(limits::MAX_TEXT_CHARS + 1);
        rejects(shape(serde_json::json!({"type":"text","at":{"x":0,"y":0},"text":text})), "shapes[0].text");
    }

    #[test]
    fn output_size_is_checked() {
        let a = Annotation { canvas: Some(Canvas { padding: 10, border_width: 2, ..Default::default() }), ..Default::default() };
        assert_eq!(a.output_size(100, 50).unwrap(), (124, 74));
        // u32 overflow in the canvas arithmetic is an error, not a panic,
        // even for documents that were never validated.
        let a = Annotation { canvas: Some(Canvas { padding: u32::MAX, border_width: 1, ..Default::default() }), ..Default::default() };
        assert!(a.output_size(1, 1).is_err());
        let a = Annotation { canvas: Some(Canvas { padding: 1 << 30, ..Default::default() }), ..Default::default() };
        assert!(a.output_size(1, 1).is_err());
        let a = Annotation { canvas: Some(Canvas { padding: 10, ..Default::default() }), ..Default::default() };
        assert!(a.output_size(u32::MAX - 5, 1).is_err());
        // Over 100 megapixels after canvas, or after scale.
        let err = Annotation::default().output_size(10_001, 10_000).unwrap_err();
        assert!(matches!(err.downcast_ref::<BugleError>(), Some(BugleError::Invalid(_))));
        let a = Annotation { scale: Some(8.0), ..Default::default() };
        assert_eq!(a.output_size(1000, 1000).unwrap(), (8000, 8000));
        assert!(a.output_size(2000, 1000).unwrap_err().to_string().contains("scale"));
        let a = Annotation { scale: Some(f32::MAX), ..Default::default() };
        assert!(a.output_size(1000, 1000).is_err());
    }

    #[test]
    fn background_presets() {
        assert!(Background::Named("sunset".into()).resolve().is_some());
        assert!(Background::Named("none".into()).resolve().is_none());
        assert!(Background::Named("gradient:#fff,#000,90".into()).resolve().is_some());
    }
}
