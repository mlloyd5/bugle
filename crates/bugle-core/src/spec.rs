//! Request/response specs that travel between the CLI, HTTP API, MCP server
//! and the engine. Everything is serde so agents can post them as JSON.

use crate::geometry::Rect;
use serde::{Deserialize, Serialize};

/// A display as seen by the window server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub frame: Rect,
    /// Backing scale factor (2.0 on Retina).
    pub scale: f64,
    pub is_main: bool,
}

/// A window as seen by the window server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u32,
    pub pid: i32,
    pub owner: String,
    pub title: String,
    pub frame: Rect,
    pub layer: i32,
    pub on_screen: bool,
}

/// What to capture.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureTarget {
    /// A whole display. `display` defaults to the main display.
    Screen {
        #[serde(default)]
        display: Option<u32>,
    },
    /// A single window, by id or by fuzzy title/owner match.
    Window {
        #[serde(default)]
        id: Option<u32>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        owner: Option<String>,
    },
    /// A rectangle in global screen points.
    Area { rect: Rect },
    /// Let the user drag a selection with the system crosshair.
    Interactive,
    /// All displays stitched side by side.
    AllScreens,
}

impl Default for CaptureTarget {
    fn default() -> Self {
        CaptureTarget::Screen { display: None }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
    Webp,
    Tiff,
}

impl ImageFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Webp => "webp",
            ImageFormat::Tiff => "tiff",
        }
    }
    pub fn mime(&self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Webp => "image/webp",
            ImageFormat::Tiff => "image/tiff",
        }
    }
}

impl std::str::FromStr for ImageFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "png" => Ok(Self::Png),
            "jpg" | "jpeg" => Ok(Self::Jpeg),
            "webp" => Ok(Self::Webp),
            "tif" | "tiff" => Ok(Self::Tiff),
            other => Err(format!("unknown image format: {other}")),
        }
    }
}

/// Options that apply to any screenshot. All fields are optional so JSON
/// requests only need to say what differs from the configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CaptureOptions {
    /// Keep the window drop shadow when capturing a window.
    #[serde(default)]
    pub shadow: Option<bool>,
    /// Seconds to wait before capturing (self-timer).
    #[serde(default)]
    pub delay: Option<f64>,
    #[serde(default)]
    pub format: Option<ImageFormat>,
    /// JPEG/WebP quality 1-100.
    #[serde(default)]
    pub quality: Option<u8>,
    /// Also place the image on the clipboard.
    #[serde(default)]
    pub copy: Option<bool>,
    /// Override the destination file (otherwise the naming template is used).
    #[serde(default)]
    pub output: Option<String>,
    /// Free-form label stored in history.
    #[serde(default)]
    pub label: Option<String>,
    /// Hide the mouse cursor in the capture.
    #[serde(default)]
    pub hide_cursor: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CaptureRequest {
    #[serde(default)]
    pub target: CaptureTarget,
    #[serde(default, flatten)]
    pub options: CaptureOptions,
}

/// OCR request. `source` is a media id or a file path.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OcrOptions {
    #[serde(default)]
    pub languages: Option<Vec<String>>,
    /// `fast` or `accurate` (accurate by default).
    #[serde(default)]
    pub fast: Option<bool>,
    /// Which engine: `vision` (Apple), or an AI provider name.
    #[serde(default)]
    pub engine: Option<String>,
    /// Restrict recognition to this sub-rectangle of the image (pixels).
    #[serde(default)]
    pub region: Option<Rect>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrLine {
    pub text: String,
    /// Bounding box in image pixels, top-left origin.
    pub bbox: Rect,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OcrResult {
    pub text: String,
    pub lines: Vec<OcrLine>,
    pub engine: String,
}

/// Scrolling capture request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollCaptureRequest {
    /// Area of the screen that scrolls (global points).
    pub rect: Rect,
    /// Maximum number of frames to grab.
    #[serde(default = "default_max_frames")]
    pub max_frames: u32,
    /// Scroll wheel lines per step (negative scrolls up).
    #[serde(default = "default_scroll_step")]
    pub step_lines: i32,
    /// Delay between scroll and grab, in milliseconds.
    #[serde(default = "default_settle_ms")]
    pub settle_ms: u64,
    /// Scroll horizontally instead of vertically.
    #[serde(default)]
    pub horizontal: bool,
    #[serde(default, flatten)]
    pub options: CaptureOptions,
}

fn default_max_frames() -> u32 {
    40
}
fn default_scroll_step() -> i32 {
    -8
}
fn default_settle_ms() -> u64 {
    350
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RecordingFormat {
    #[default]
    Mp4,
    Mov,
    Gif,
}

/// Recording request. Video goes through the system recorder; GIF is an
/// in-process frame grab loop.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RecordRequest {
    #[serde(default)]
    pub target: CaptureTarget,
    #[serde(default)]
    pub format: RecordingFormat,
    /// Frames per second for GIF recordings.
    #[serde(default)]
    pub fps: Option<u32>,
    /// Record microphone audio.
    #[serde(default)]
    pub microphone: bool,
    /// Record system audio.
    #[serde(default)]
    pub system_audio: bool,
    /// Stop automatically after this many seconds.
    #[serde(default)]
    pub max_seconds: Option<f64>,
    /// Show mouse clicks.
    #[serde(default)]
    pub highlight_clicks: bool,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    Idle,
    Active,
    Finishing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingStatus {
    pub state: RecordingState,
    pub format: Option<RecordingFormat>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub elapsed_seconds: f64,
    pub frames: u64,
    pub output: Option<String>,
}

/// Transcript produced by a speech-to-text provider.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Transcript {
    pub language: Option<String>,
    pub text: String,
    pub segments: Vec<TranscriptSegment>,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SubtitleFormat {
    #[default]
    Srt,
    Vtt,
}

impl Transcript {
    pub fn to_srt(&self) -> String {
        let mut out = String::new();
        for (i, s) in self.segments.iter().enumerate() {
            out.push_str(&format!(
                "{}\n{} --> {}\n{}\n\n",
                i + 1,
                fmt_ts(s.start, ','),
                fmt_ts(s.end, ','),
                s.text.trim()
            ));
        }
        out
    }

    pub fn to_vtt(&self) -> String {
        let mut out = String::from("WEBVTT\n\n");
        for s in &self.segments {
            out.push_str(&format!(
                "{} --> {}\n{}\n\n",
                fmt_ts(s.start, '.'),
                fmt_ts(s.end, '.'),
                s.text.trim()
            ));
        }
        out
    }

    pub fn render(&self, f: SubtitleFormat) -> String {
        match f {
            SubtitleFormat::Srt => self.to_srt(),
            SubtitleFormat::Vtt => self.to_vtt(),
        }
    }
}

fn fmt_ts(t: f64, sep: char) -> String {
    let total_ms = (t.max(0.0) * 1000.0).round() as u64;
    let h = total_ms / 3_600_000;
    let m = (total_ms / 60_000) % 60;
    let s = (total_ms / 1000) % 60;
    let ms = total_ms % 1000;
    format!("{h:02}:{m:02}:{s:02}{sep}{ms:03}")
}

/// Image description request (vision model).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DescribeRequest {
    pub source: String,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    /// Include OCR text in the prompt so the model can ground its answer.
    #[serde(default)]
    pub with_ocr: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeResult {
    pub description: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub ocr: Option<OcrResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_timestamps() {
        assert_eq!(fmt_ts(3661.5, ','), "01:01:01,500");
        assert_eq!(fmt_ts(0.0, '.'), "00:00:00.000");
    }

    #[test]
    fn target_json_roundtrip() {
        let t = CaptureTarget::Area { rect: Rect::new(1.0, 2.0, 3.0, 4.0) };
        let j = serde_json::to_string(&t).unwrap();
        assert!(j.contains("\"kind\":\"area\""));
        let back: CaptureTarget = serde_json::from_str(&j).unwrap();
        assert_eq!(back, t);
    }
}
