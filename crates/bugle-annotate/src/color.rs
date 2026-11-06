use serde::{Deserialize, Serialize};

/// RGBA colour, serialised as `#RRGGBB`, `#RRGGBBAA` or a CSS-ish name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub [u8; 4]);

impl Color {
    pub const RED: Color = Color([0xFF, 0x3B, 0x30, 0xFF]);
    pub const YELLOW: Color = Color([0xFF, 0xE6, 0x00, 0xFF]);
    pub const BLACK: Color = Color([0, 0, 0, 0xFF]);
    pub const WHITE: Color = Color([0xFF, 0xFF, 0xFF, 0xFF]);

    pub fn rgba(&self) -> image::Rgba<u8> {
        image::Rgba(self.0)
    }

    pub fn with_alpha(mut self, a: u8) -> Self {
        self.0[3] = a;
        self
    }

    pub fn parse(s: &str) -> Option<Color> {
        let s = s.trim();
        let named = match s.to_ascii_lowercase().as_str() {
            "red" => Some([0xFF, 0x3B, 0x30, 0xFF]),
            "orange" => Some([0xFF, 0x95, 0x00, 0xFF]),
            "yellow" => Some([0xFF, 0xE6, 0x00, 0xFF]),
            "green" => Some([0x34, 0xC7, 0x59, 0xFF]),
            "blue" => Some([0x00, 0x7A, 0xFF, 0xFF]),
            "purple" => Some([0xAF, 0x52, 0xDE, 0xFF]),
            "pink" => Some([0xFF, 0x2D, 0x55, 0xFF]),
            "white" => Some([0xFF, 0xFF, 0xFF, 0xFF]),
            "black" => Some([0, 0, 0, 0xFF]),
            "gray" | "grey" => Some([0x8E, 0x8E, 0x93, 0xFF]),
            "transparent" | "none" => Some([0, 0, 0, 0]),
            _ => None,
        };
        if let Some(c) = named {
            return Some(Color(c));
        }
        let hex = s.strip_prefix('#').unwrap_or(s);
        let v = u32::from_str_radix(hex, 16).ok()?;
        match hex.len() {
            6 => Some(Color([(v >> 16) as u8, (v >> 8) as u8, v as u8, 0xFF])),
            8 => Some(Color([(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8])),
            3 => {
                let r = ((v >> 8) & 0xF) as u8;
                let g = ((v >> 4) & 0xF) as u8;
                let b = (v & 0xF) as u8;
                Some(Color([r * 17, g * 17, b * 17, 0xFF]))
            }
            _ => None,
        }
    }

    pub fn to_hex(&self) -> String {
        let [r, g, b, a] = self.0;
        if a == 0xFF {
            format!("#{r:02X}{g:02X}{b:02X}")
        } else {
            format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
        }
    }

    /// Perceived luminance 0..1, for choosing contrasting text.
    pub fn luminance(&self) -> f32 {
        let [r, g, b, _] = self.0;
        (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
    }
}

impl Serialize for Color {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Color::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("bad colour: {s}")))
    }
}

impl std::str::FromStr for Color {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Color::parse(s).ok_or_else(|| format!("bad colour: {s}"))
    }
}
