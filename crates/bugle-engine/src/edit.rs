//! Post-capture editing: trim a recording and export it with a preset.
//!
//! * Video → video: one `AVAssetExportSession` over the trimmed time range.
//!   With nothing to re-encode (`original`, no frame rate) the samples are
//!   copied, so a plain trim is lossless; audio tracks are always kept.
//! * Video → GIF: frames decoded at the GIF rate with one reusable image
//!   generator, scaled, and streamed into the GIF encoder.
//! * GIF → GIF: a frame-exact cut of the decoded frames (the delays of the
//!   frames at the cut points are shortened), or a resample at a new rate.
//! * GIF → video: frames resampled to a constant rate and encoded to H.264.
//!
//! The result is written under a temporary name next to the output and
//! renamed into place, so a failed export never leaves a partial file, and it
//! is added to history with the source as its parent.

use crate::Engine;
use anyhow::{Context, Result};
use bugle_core::config::RecordingConfig;
use bugle_core::{BugleError, MediaItem, MediaKind};
use bugle_platform::{VideoExportPreset, VideoFrameReader, VideoFrameWriter};
use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::{AnimationDecoder, Delay, Frame, RgbaImage};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Shortest clip an edit may produce, in seconds.
pub const MIN_CLIP_SECONDS: f64 = 0.05;
/// GIF frame rates are capped here (delays are stored in 10 ms units).
const GIF_MAX_FPS: u32 = 50;
const VIDEO_MAX_FPS: u32 = 120;
/// `gif-small` defaults.
const GIF_SMALL_FPS: u32 = 8;
const GIF_SMALL_MAX_SIZE: u32 = 480;

/// What an edit exports to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ExportPreset {
    /// Keep the source format; a video trim is copied without re-encoding.
    #[default]
    #[serde(rename = "original")]
    Original,
    /// H.264 at the source size.
    #[serde(rename = "high")]
    High,
    /// HEVC (H.265) at the source size: smaller files, newer players.
    #[serde(rename = "hevc")]
    Hevc,
    /// H.264 scaled to fit 1920x1080.
    #[serde(rename = "1080p")]
    P1080,
    /// H.264 scaled to fit 1280x720.
    #[serde(rename = "720p")]
    P720,
    /// H.264 scaled to fit 960x540.
    #[serde(rename = "540p")]
    P540,
    /// Animated GIF at `recordings.gif_frame_rate` / `gif_max_size`.
    #[serde(rename = "gif")]
    Gif,
    /// Animated GIF at 8 fps, long edge 480 px (chat-sized).
    #[serde(rename = "gif-small")]
    GifSmall,
}

impl ExportPreset {
    pub const ALL: [ExportPreset; 8] = [
        ExportPreset::Original,
        ExportPreset::High,
        ExportPreset::Hevc,
        ExportPreset::P1080,
        ExportPreset::P720,
        ExportPreset::P540,
        ExportPreset::Gif,
        ExportPreset::GifSmall,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ExportPreset::Original => "original",
            ExportPreset::High => "high",
            ExportPreset::Hevc => "hevc",
            ExportPreset::P1080 => "1080p",
            ExportPreset::P720 => "720p",
            ExportPreset::P540 => "540p",
            ExportPreset::Gif => "gif",
            ExportPreset::GifSmall => "gif-small",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            ExportPreset::Original => "same format as the source; video trims are copied without re-encoding",
            ExportPreset::High => "H.264 at the source size",
            ExportPreset::Hevc => "HEVC (H.265) at the source size",
            ExportPreset::P1080 => "H.264 scaled down to fit 1920x1080",
            ExportPreset::P720 => "H.264 scaled down to fit 1280x720",
            ExportPreset::P540 => "H.264 scaled down to fit 960x540",
            ExportPreset::Gif => "animated GIF at recordings.gif_frame_rate / gif_max_size",
            ExportPreset::GifSmall => "animated GIF, 8 fps, long edge 480 px",
        }
    }

    pub fn is_gif(self) -> bool {
        matches!(self, ExportPreset::Gif | ExportPreset::GifSmall)
    }

    /// The box a scaling video preset fits frames into.
    fn video_box(self) -> Option<(u32, u32)> {
        match self {
            ExportPreset::P1080 => Some((1920, 1080)),
            ExportPreset::P720 => Some((1280, 720)),
            ExportPreset::P540 => Some((960, 540)),
            _ => None,
        }
    }
}

impl std::fmt::Display for ExportPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for ExportPreset {
    type Err = BugleError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        ExportPreset::ALL.into_iter().find(|p| p.name() == s).ok_or_else(|| {
            let names: Vec<_> = ExportPreset::ALL.iter().map(|p| p.name()).collect();
            BugleError::Invalid(format!("unknown export preset '{s}' (expected one of: {})", names.join(", ")))
        })
    }
}

/// A trim/export request. Times are seconds from the start of the source; a
/// negative value counts back from the end (`end: -2` drops the last two
/// seconds).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EditRequest {
    /// Media id (or prefix), `latest`, or a path to a .mp4/.mov/.m4v/.gif.
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub start: Option<f64>,
    #[serde(default)]
    pub end: Option<f64>,
    #[serde(default)]
    pub preset: ExportPreset,
    /// Output frame rate (re-encodes a video; resamples a GIF).
    #[serde(default)]
    pub fps: Option<u32>,
    /// Long edge of GIF output, in pixels.
    #[serde(default)]
    pub max_size: Option<u32>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// Parse a time given as seconds (`90`, `1.5`, `-2`, `3s`) or as
/// `m:ss` / `h:mm:ss` (with optional fractions, optional leading `-`).
pub fn parse_time(s: &str) -> std::result::Result<f64, BugleError> {
    let bad = || BugleError::Invalid(format!("invalid time '{s}' (use seconds like 12.5, or m:ss)"));
    let t = s.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let body = body.strip_suffix('s').unwrap_or(body);
    if body.is_empty() {
        return Err(bad());
    }
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() > 3 {
        return Err(bad());
    }
    let mut total = 0.0;
    for (i, p) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let v: f64 = p.parse().map_err(|_| bad())?;
        if !v.is_finite() || v < 0.0 || (!last && p.contains('.')) || (i > 0 && v >= 60.0) {
            return Err(bad());
        }
        total = total * 60.0 + v;
    }
    Ok(if neg { -total } else { total })
}

/// Resolve a request's `start`/`end` against the source duration. Negative
/// values count back from the end; the result is clamped to the source and
/// must be at least [`MIN_CLIP_SECONDS`] long.
pub fn resolve_range(start: Option<f64>, end: Option<f64>, duration: f64) -> std::result::Result<(f64, f64), BugleError> {
    if !duration.is_finite() || duration <= 0.0 {
        return Err(BugleError::Invalid("the source has no duration".into()));
    }
    let abs = |v: f64| if v < 0.0 { duration + v } else { v };
    let s = start.unwrap_or(0.0);
    let e = end.unwrap_or(duration);
    if !s.is_finite() || !e.is_finite() {
        return Err(BugleError::Invalid("start and end must be finite numbers".into()));
    }
    if abs(s) >= duration {
        return Err(BugleError::Invalid(format!("start {:.2}s is at or past the end of the {duration:.2}s source", abs(s))));
    }
    let (s, e) = (abs(s).clamp(0.0, duration), abs(e).clamp(0.0, duration));
    if e - s < MIN_CLIP_SECONDS {
        return Err(BugleError::Invalid(format!(
            "the range {s:.2}s..{e:.2}s of a {duration:.2}s source is empty (start must be before end)"
        )));
    }
    Ok((s, e))
}

/// The container an edit writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Gif,
    Video { mp4: bool },
}

impl Target {
    fn extension(self) -> &'static str {
        match self {
            Target::Gif => "gif",
            Target::Video { mp4: true } => "mp4",
            Target::Video { mp4: false } => "mov",
        }
    }
}

fn extension_of(p: &str) -> String {
    Path::new(p).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// Pick the output container from the preset, an explicit output extension,
/// and the source format (`original` keeps it).
fn choose_target(preset: ExportPreset, output: Option<&str>, source_ext: &str) -> std::result::Result<Target, BugleError> {
    match output.map(extension_of).as_deref() {
        Some("gif") if preset.is_gif() || preset == ExportPreset::Original => return Ok(Target::Gif),
        Some("gif") => {
            return Err(BugleError::Invalid(format!("preset {preset} writes video; use a .mp4 or .mov output")));
        }
        Some(ext @ ("mp4" | "m4v" | "mov")) => {
            if preset.is_gif() {
                return Err(BugleError::Invalid(format!("preset {preset} writes a GIF; use a .gif output")));
            }
            return Ok(Target::Video { mp4: ext != "mov" });
        }
        _ => {}
    }
    Ok(if preset.is_gif() || (preset == ExportPreset::Original && source_ext == "gif") {
        Target::Gif
    } else {
        Target::Video { mp4: source_ext != "mov" }
    })
}

/// The export-session preset for a video → video edit.
fn video_plan(preset: ExportPreset, fps: Option<u32>) -> VideoExportPreset {
    match preset {
        // A new frame rate needs a re-encode.
        ExportPreset::Original if fps.is_some() => VideoExportPreset::Highest,
        ExportPreset::Original => VideoExportPreset::Passthrough,
        ExportPreset::Hevc => VideoExportPreset::HevcHighest,
        ExportPreset::P1080 => VideoExportPreset::Fit1920x1080,
        ExportPreset::P720 => VideoExportPreset::Fit1280x720,
        ExportPreset::P540 => VideoExportPreset::Fit960x540,
        ExportPreset::High | ExportPreset::Gif | ExportPreset::GifSmall => VideoExportPreset::Highest,
    }
}

/// GIF output settings: `fps` is `None` when a GIF source keeps its own
/// timing; `max_size` is `None` when frames keep their size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GifSettings {
    fps: Option<u32>,
    max_size: Option<u32>,
}

fn gif_settings(preset: ExportPreset, fps: Option<u32>, max_size: Option<u32>, from_video: bool, rec: &RecordingConfig) -> GifSettings {
    let fps = fps.or((preset == ExportPreset::GifSmall).then_some(GIF_SMALL_FPS)).or(from_video.then_some(rec.gif_frame_rate));
    let max_size = max_size.or(match preset {
        ExportPreset::GifSmall => Some(GIF_SMALL_MAX_SIZE),
        ExportPreset::Gif => Some(rec.gif_max_size),
        _ => from_video.then_some(rec.gif_max_size),
    });
    GifSettings {
        fps: fps.map(|f| f.clamp(1, GIF_MAX_FPS)),
        max_size: max_size.map(|m| m.clamp(crate::MIN_MAX_EDGE, crate::MAX_MAX_EDGE)),
    }
}

/// Frame times for resampling `start..end` at `fps`: the middle of each
/// output frame's interval, so decoding never lands on a frame boundary.
fn sample_times(start: f64, end: f64, fps: u32) -> Vec<f64> {
    let step = 1.0 / fps.max(1) as f64;
    let n = (((end - start) / step) - 1e-6).ceil().max(1.0) as usize;
    (0..n).map(|k| (start + (k as f64 + 0.5) * step).min(end)).collect()
}

/// A decoded GIF: full frames and when each starts, in seconds.
struct GifFrames {
    frames: Vec<RgbaImage>,
    starts: Vec<f64>,
    delays: Vec<f64>,
    duration: f64,
}

/// A GIF frame delay in seconds. Players show delays of 10 ms or less as
/// 100 ms, so the timeline does too.
fn gif_delay_seconds(d: Delay) -> f64 {
    let (n, den) = d.numer_denom_ms();
    let ms = n as f64 / den.max(1) as f64;
    if ms <= 10.0 {
        0.1
    } else {
        ms / 1000.0
    }
}

fn decode_gif(path: &Path) -> Result<GifFrames> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let decoder = GifDecoder::new(std::io::BufReader::new(file)).with_context(|| format!("decoding {}", path.display()))?;
    let mut out = GifFrames { frames: vec![], starts: vec![], delays: vec![], duration: 0.0 };
    for f in decoder.into_frames() {
        let f = f.with_context(|| format!("decoding {}", path.display()))?;
        let d = gif_delay_seconds(f.delay());
        out.starts.push(out.duration);
        out.delays.push(d);
        out.duration += d;
        out.frames.push(f.into_buffer());
    }
    if out.frames.is_empty() {
        return Err(BugleError::Invalid(format!("{} has no frames", path.display())).into());
    }
    Ok(out)
}

impl GifFrames {
    /// Index of the frame shown at `t`.
    fn index_at(&self, t: f64) -> usize {
        self.starts.partition_point(|s| *s <= t).saturating_sub(1)
    }

    /// Mean frame rate, for re-encoding to a constant-rate video.
    fn mean_fps(&self) -> u32 {
        ((self.frames.len() as f64 / self.duration).round() as u32).clamp(1, 60)
    }
}

/// Frame-exact cut: the frames overlapping `start..end` with their delays
/// shortened to the overlap. Slivers under 5 ms are dropped.
fn gif_cut(starts: &[f64], delays: &[f64], start: f64, end: f64) -> Vec<(usize, f64)> {
    starts
        .iter()
        .zip(delays)
        .enumerate()
        .filter_map(|(i, (s, d))| {
            let shown = (s + d).min(end) - s.max(start);
            (shown >= 0.005).then_some((i, shown))
        })
        .collect()
}

/// Scale a frame so its long edge is at most `max_size` (no upscaling).
fn fit_gif_frame(img: RgbaImage, max_size: Option<u32>) -> RgbaImage {
    match max_size.and_then(|m| crate::bounded_size(img.width(), img.height(), m)) {
        Some((w, h)) => image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle),
        None => img,
    }
}

/// Frame size for an H.264 encode: fitted into `fit_box` (never upscaled)
/// and rounded down to even sides.
fn video_frame_size(w: u32, h: u32, fit_box: Option<(u32, u32)>) -> (u32, u32) {
    let s = fit_box.map(|(bw, bh)| (bw as f64 / w.max(1) as f64).min(bh as f64 / h.max(1) as f64).min(1.0)).unwrap_or(1.0);
    let even = |v: u32| ((v as f64 * s).round() as u32 & !1).max(2);
    (even(w), even(h))
}

/// Stream frames (image, delay in seconds) into a looping GIF. Returns the
/// frame size and count.
fn write_gif(path: &Path, frames: impl Iterator<Item = Result<(RgbaImage, f64)>>) -> Result<(u32, u32, usize)> {
    let file = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut enc = GifEncoder::new_with_speed(std::io::BufWriter::new(file), 10);
    enc.set_repeat(Repeat::Infinite)?;
    let (mut w, mut h, mut n) = (0, 0, 0);
    for f in frames {
        let (img, secs) = f?;
        (w, h) = img.dimensions();
        let delay = Delay::from_numer_denom_ms(((secs * 1000.0).round() as u32).max(10), 1);
        enc.encode_frame(Frame::from_parts(img, 0, 0, delay))?;
        n += 1;
    }
    if n == 0 {
        return Err(BugleError::Invalid("the edit produced no frames".into()).into());
    }
    Ok((w, h, n))
}

/// Whether two paths name the same existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn source_kind(p: &Path) -> Option<MediaKind> {
    Some(crate::kind_from_path(p)).filter(|k| matches!(k, MediaKind::Video | MediaKind::Gif))
}

impl Engine {
    /// Trim a recording and export it with a preset, as a new history item
    /// whose parent is the source. See the module docs for the pipelines.
    pub fn edit(&self, req: &EditRequest) -> Result<MediaItem> {
        let src = self.resolve_media(&req.source)?;
        let Some(kind) = source_kind(&src.path) else {
            return Err(BugleError::Invalid(format!("{} is not a recording (.mp4, .mov, .m4v or .gif)", src.path.display())).into());
        };
        if req.fps == Some(0) || req.max_size == Some(0) {
            return Err(BugleError::Invalid("fps and max_size must be positive".into()).into());
        }
        let src_ext = extension_of(&src.path.to_string_lossy());
        let target = choose_target(req.preset, req.output.as_deref(), &src_ext)?;
        let mut vars = HashMap::new();
        vars.insert("kind", "edit".to_string());
        if let Some(l) = &req.label {
            vars.insert("label", l.clone());
        }
        let out = self.output_path(req.output.as_deref(), target.extension(), &vars);
        if same_file(&src.path, &out) {
            return Err(BugleError::Invalid(format!("the output {} is the source; choose another path", out.display())).into());
        }
        let dir = out.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let tmp = tempfile::Builder::new()
            .prefix(".bugle-edit-")
            .suffix(&format!(".{}", target.extension()))
            .tempfile_in(dir)
            .with_context(|| format!("creating a temporary file in {}", dir.display()))?
            .into_temp_path();

        let rec = &self.config.recordings;
        let (w, h, duration, range) = match (kind, target) {
            (MediaKind::Video, Target::Video { mp4 }) => {
                let fps = req.fps.map(|f| f.min(VIDEO_MAX_FPS));
                let (s, e) = resolve_range(req.start, req.end, bugle_platform::media_duration(&src.path)?)?;
                bugle_platform::export_video(&src.path, &tmp, s, e, video_plan(req.preset, fps), mp4, fps)?;
                let (w, h) = bugle_platform::video_size(&tmp)?;
                let d = bugle_platform::media_duration(&tmp).ok().filter(|d| *d > 0.0).unwrap_or(e - s);
                (w, h, d, (s, e))
            }
            (MediaKind::Video, Target::Gif) => {
                let g = gif_settings(req.preset, req.fps, req.max_size, true, rec);
                let fps = g.fps.unwrap_or(rec.gif_frame_rate).max(1);
                let (s, e) = resolve_range(req.start, req.end, bugle_platform::media_duration(&src.path)?)?;
                // Exact decoding: any tolerance lets the generator snap to a
                // nearby keyframe, which can lie outside the cut.
                let mut reader = VideoFrameReader::open(&src.path, 0.0)?;
                let times = sample_times(s, e, fps);
                let frames = times.into_iter().map(|t| Ok((fit_gif_frame(reader.frame_at(t)?, g.max_size), 1.0 / fps as f64)));
                let (w, h, n) = write_gif(&tmp, frames)?;
                (w, h, n as f64 / fps as f64, (s, e))
            }
            (_, Target::Gif) => {
                let gif = decode_gif(&src.path)?;
                let g = gif_settings(req.preset, req.fps, req.max_size, false, rec);
                let (s, e) = resolve_range(req.start, req.end, gif.duration)?;
                let (w, h, _) = match g.fps {
                    Some(fps) => {
                        let frames = sample_times(s, e, fps)
                            .into_iter()
                            .map(|t| Ok((fit_gif_frame(gif.frames[gif.index_at(t)].clone(), g.max_size), 1.0 / fps as f64)));
                        write_gif(&tmp, frames)?
                    }
                    None => {
                        let frames = gif_cut(&gif.starts, &gif.delays, s, e)
                            .into_iter()
                            .map(|(i, d)| Ok((fit_gif_frame(gif.frames[i].clone(), g.max_size), d)));
                        write_gif(&tmp, frames)?
                    }
                };
                (w, h, e - s, (s, e))
            }
            (_, Target::Video { mp4 }) => {
                let gif = decode_gif(&src.path)?;
                let (s, e) = resolve_range(req.start, req.end, gif.duration)?;
                let fps = req.fps.unwrap_or_else(|| gif.mean_fps()).clamp(1, VIDEO_MAX_FPS);
                let first = &gif.frames[0];
                let (w, h) = video_frame_size(first.width(), first.height(), req.preset.video_box());
                let mut writer = VideoFrameWriter::create(&tmp, w, h, fps, mp4)?;
                for t in sample_times(s, e, fps) {
                    let f = &gif.frames[gif.index_at(t)];
                    let f = if f.dimensions() == (w, h) {
                        f.clone()
                    } else {
                        image::imageops::resize(f, w, h, image::imageops::FilterType::Triangle)
                    };
                    writer.append(&f)?;
                }
                writer.finish()?;
                let d = bugle_platform::media_duration(&tmp).ok().filter(|d| *d > 0.0).unwrap_or(e - s);
                (w, h, d, (s, e))
            }
        };
        tmp.persist(&out).with_context(|| format!("moving the export to {}", out.display()))?;

        let mut m = MediaItem::new(if target == Target::Gif { MediaKind::Gif } else { MediaKind::Video }, &out);
        m.width = Some(w);
        m.height = Some(h);
        m.duration_seconds = Some(duration);
        m.label = req.label.clone();
        m.parent = Some(src.id.clone());
        m.source = Some(format!("edit: {:.2}s–{:.2}s, preset {}", range.0, range.1, req.preset));
        self.sync_after_save(&mut m);
        self.history.push(&m)?;
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::Config;
    use std::path::PathBuf;

    fn engine_in(tmp: &Path) -> Engine {
        let mut cfg = Config::default();
        cfg.general.save_dir = tmp.join("save").display().to_string();
        cfg.general.history_path = Some(tmp.join("history.jsonl").display().to_string());
        Engine::new(cfg, false).unwrap()
    }

    fn invalid(e: &anyhow::Error) -> bool {
        matches!(e.chain().find_map(|e| e.downcast_ref::<BugleError>()), Some(BugleError::Invalid(_)))
    }

    const RED: [u8; 3] = [255, 0, 0];
    const GREEN: [u8; 3] = [0, 255, 0];
    const BLUE: [u8; 3] = [0, 0, 255];

    fn solid(w: u32, h: u32, c: [u8; 3]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba([c[0], c[1], c[2], 255]))
    }

    /// Which primary a decoded pixel is (H.264's colour conversion shifts
    /// pure primaries a little, e.g. green comes back as ~(68, 251, 0)).
    fn near(p: &image::Rgba<u8>, c: [u8; 3]) -> bool {
        (0..3).all(|i| if c[i] == 255 { p[i] > 180 } else { p[i] < 120 })
    }

    /// 3 s, 160x120 at 10 fps: one second each of red, green, blue.
    fn fixture_movie(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        let mp4 = extension_of(name) != "mov";
        let mut w = VideoFrameWriter::create(&p, 160, 120, 10, mp4).unwrap();
        for i in 0..30 {
            w.append(&solid(160, 120, [RED, GREEN, BLUE][i / 10])).unwrap();
        }
        w.finish().unwrap();
        p
    }

    /// 6 frames of 100 ms, 40x30: red, red, green, green, blue, blue.
    fn fixture_gif(dir: &Path) -> PathBuf {
        let p = dir.join("fixture.gif");
        let frames = (0..6).map(|i| Ok((solid(40, 30, [RED, GREEN, BLUE][i / 2]), 0.1)));
        write_gif(&p, frames).unwrap();
        p
    }

    fn gif_timeline(p: &Path) -> Vec<(f64, [u8; 3])> {
        let g = decode_gif(p).unwrap();
        g.frames
            .iter()
            .zip(&g.delays)
            .map(|(f, d)| {
                let px = f.get_pixel(f.width() / 2, f.height() / 2);
                (*d, [px[0], px[1], px[2]])
            })
            .collect()
    }

    #[test]
    fn times_parse() {
        assert_eq!(parse_time("12.5").unwrap(), 12.5);
        assert_eq!(parse_time("3s").unwrap(), 3.0);
        assert_eq!(parse_time("-2").unwrap(), -2.0);
        assert_eq!(parse_time("1:30").unwrap(), 90.0);
        assert_eq!(parse_time("1:02:03.5").unwrap(), 3723.5);
        assert_eq!(parse_time("-0:05").unwrap(), -5.0);
        for bad in ["", "-", "abc", "1:60", "1.5:00", "1:2:3:4", "nan", "inf", "1:-5"] {
            assert!(parse_time(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ranges_resolve_against_duration() {
        assert_eq!(resolve_range(None, None, 10.0).unwrap(), (0.0, 10.0));
        assert_eq!(resolve_range(Some(2.0), Some(-3.0), 10.0).unwrap(), (2.0, 7.0));
        assert_eq!(resolve_range(Some(-4.0), None, 10.0).unwrap(), (6.0, 10.0));
        // Clamped to the source.
        assert_eq!(resolve_range(Some(-40.0), Some(99.0), 10.0).unwrap(), (0.0, 10.0));
        for (s, e) in [(Some(5.0), Some(5.0)), (Some(6.0), Some(4.0)), (Some(12.0), None), (Some(f64::NAN), None)] {
            assert!(matches!(resolve_range(s, e, 10.0), Err(BugleError::Invalid(_))), "{s:?}..{e:?}");
        }
        assert!(resolve_range(None, None, 0.0).is_err());
        assert!(resolve_range(None, None, f64::NAN).is_err());
    }

    #[test]
    fn presets_parse_and_serialise() {
        for p in ExportPreset::ALL {
            assert_eq!(p.name().parse::<ExportPreset>().unwrap(), p);
            assert_eq!(serde_json::to_value(p).unwrap(), serde_json::json!(p.name()));
        }
        assert_eq!("GIF-Small".parse::<ExportPreset>().unwrap(), ExportPreset::GifSmall);
        let err = "4k".parse::<ExportPreset>().unwrap_err().to_string();
        assert!(err.contains("720p") && err.contains("gif-small"), "{err}");
        let req: EditRequest = serde_json::from_value(serde_json::json!({"source":"x","end":-1,"preset":"720p"})).unwrap();
        assert_eq!((req.preset, req.end, req.start), (ExportPreset::P720, Some(-1.0), None));
        let req: EditRequest = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(req.preset, ExportPreset::Original);
    }

    #[test]
    fn targets() {
        use ExportPreset::*;
        assert_eq!(choose_target(Original, None, "mov").unwrap(), Target::Video { mp4: false });
        assert_eq!(choose_target(Original, None, "mp4").unwrap(), Target::Video { mp4: true });
        assert_eq!(choose_target(Original, None, "gif").unwrap(), Target::Gif);
        assert_eq!(choose_target(P720, None, "gif").unwrap(), Target::Video { mp4: true });
        assert_eq!(choose_target(Hevc, None, "mov").unwrap(), Target::Video { mp4: false });
        assert_eq!(choose_target(Gif, None, "mp4").unwrap(), Target::Gif);
        assert_eq!(choose_target(Original, Some("/x/clip.mov"), "mp4").unwrap(), Target::Video { mp4: false });
        assert_eq!(choose_target(High, Some("clip.M4V"), "mov").unwrap(), Target::Video { mp4: true });
        assert_eq!(choose_target(Original, Some("clip.gif"), "mp4").unwrap(), Target::Gif);
        // A directory or extension-less output follows the preset.
        assert_eq!(choose_target(GifSmall, Some("/tmp/out"), "mp4").unwrap(), Target::Gif);
        assert!(choose_target(P1080, Some("clip.gif"), "mp4").is_err());
        assert!(choose_target(Gif, Some("clip.mp4"), "mp4").is_err());
    }

    #[test]
    fn video_plans() {
        use ExportPreset::*;
        assert_eq!(video_plan(Original, None), VideoExportPreset::Passthrough);
        assert_eq!(video_plan(Original, Some(15)), VideoExportPreset::Highest);
        assert_eq!(video_plan(High, None), VideoExportPreset::Highest);
        assert_eq!(video_plan(Hevc, Some(30)), VideoExportPreset::HevcHighest);
        assert_eq!(video_plan(P1080, None), VideoExportPreset::Fit1920x1080);
        assert_eq!(video_plan(P720, None), VideoExportPreset::Fit1280x720);
        assert_eq!(video_plan(P540, None), VideoExportPreset::Fit960x540);
    }

    #[test]
    fn gif_settings_defaults_and_overrides() {
        use ExportPreset::*;
        let rec = RecordingConfig { gif_frame_rate: 12, gif_max_size: 1200, ..Config::default().recordings };
        let s = |p, fps, max, from_video| gif_settings(p, fps, max, from_video, &rec);
        assert_eq!(s(Gif, None, None, true), GifSettings { fps: Some(12), max_size: Some(1200) });
        assert_eq!(s(Gif, None, None, false), GifSettings { fps: None, max_size: Some(1200) });
        assert_eq!(s(GifSmall, None, None, false), GifSettings { fps: Some(8), max_size: Some(480) });
        assert_eq!(s(Original, None, None, false), GifSettings { fps: None, max_size: None });
        assert_eq!(s(Original, None, None, true), GifSettings { fps: Some(12), max_size: Some(1200) });
        assert_eq!(s(GifSmall, Some(200), Some(1), true), GifSettings { fps: Some(GIF_MAX_FPS), max_size: Some(crate::MIN_MAX_EDGE) });
    }

    #[test]
    fn sampling_and_cutting() {
        assert_eq!(sample_times(0.0, 1.0, 4), vec![0.125, 0.375, 0.625, 0.875]);
        assert_eq!(sample_times(1.0, 1.5, 2).len(), 1);
        // A clip shorter than one frame still gets a frame, inside the range.
        let t = sample_times(2.0, 2.05, 10);
        assert_eq!(t.len(), 1);
        assert!(t[0] <= 2.05);
        // Float noise does not add a frame.
        assert_eq!(sample_times(0.1, 0.4, 10).len(), 3);

        let starts = [0.0, 0.1, 0.2, 0.3];
        let delays = [0.1; 4];
        let cut = gif_cut(&starts, &delays, 0.15, 0.32);
        assert_eq!(cut.iter().map(|c| c.0).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!((cut[0].1 - 0.05).abs() < 1e-9 && (cut[1].1 - 0.1).abs() < 1e-9 && (cut[2].1 - 0.02).abs() < 1e-9);
        // Slivers are dropped.
        assert_eq!(gif_cut(&starts, &delays, 0.098, 0.2).len(), 1);

        assert_eq!(video_frame_size(160, 120, None), (160, 120));
        assert_eq!(video_frame_size(41, 31, None), (40, 30));
        assert_eq!(video_frame_size(3840, 2160, Some((1280, 720))), (1280, 720));
        assert_eq!(video_frame_size(1000, 2000, Some((1280, 720))), (360, 720));
        assert_eq!(video_frame_size(320, 240, Some((1280, 720))), (320, 240), "never upscaled");
        assert_eq!(video_frame_size(1, 1, None), (2, 2));
    }

    #[test]
    fn trims_video_losslessly_and_records_history() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_movie(dir.path(), "src.mp4");
        let before = std::fs::read(&src).unwrap();
        let out = dir.path().join("cut.mp4");
        let req = EditRequest {
            source: src.display().to_string(),
            start: Some(1.0),
            end: Some(-1.0),
            output: Some(out.display().to_string()),
            label: Some("demo".into()),
            ..Default::default()
        };
        let m = engine.edit(&req).unwrap();
        assert_eq!(m.path, out);
        assert_eq!(m.kind, MediaKind::Video);
        assert_eq!((m.width, m.height), (Some(160), Some(120)));
        assert!((m.duration_seconds.unwrap() - 1.0).abs() < 0.1, "{:?}", m.duration_seconds);
        assert!(m.source.as_deref().unwrap().contains("1.00s–2.00s, preset original"));
        let mut r = VideoFrameReader::open(&out, 0.0).unwrap();
        assert!(near(r.frame_at(0.5).unwrap().get_pixel(80, 60), GREEN));
        // The source is untouched, the edit is in history, and no temp file is left.
        assert_eq!(std::fs::read(&src).unwrap(), before);
        let latest = engine.history.latest().unwrap().unwrap();
        assert_eq!((latest.id, latest.label.as_deref()), (m.id.clone(), Some("demo")));
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".bugle-edit-"))
            .collect();
        assert!(leftovers.is_empty());

        // "latest" now names the edit; chain a re-encode into a .mov at 5 fps.
        let req = EditRequest { source: "latest".into(), preset: ExportPreset::High, fps: Some(5), output: Some(dir.path().join("re.mov").display().to_string()), ..Default::default() };
        let m2 = engine.edit(&req).unwrap();
        assert_eq!(m2.parent.as_deref(), Some(m.id.as_str()));
        assert_eq!(m2.path.extension().unwrap(), "mov");
        assert!((m2.duration_seconds.unwrap() - 1.0).abs() < 0.15);
    }

    #[test]
    fn scales_video_with_presets() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let big = dir.path().join("big.mov");
        let mut w = VideoFrameWriter::create(&big, 1600, 900, 10, false).unwrap();
        for _ in 0..10 {
            w.append(&solid(1600, 900, BLUE)).unwrap();
        }
        w.finish().unwrap();
        let req = EditRequest { source: big.display().to_string(), preset: ExportPreset::P540, ..Default::default() };
        let m = engine.edit(&req).unwrap();
        // Default location (save dir), container follows the .mov source.
        assert!(m.path.starts_with(dir.path().join("save")));
        assert_eq!(m.path.extension().unwrap(), "mov");
        assert_eq!((m.width, m.height), (Some(960), Some(540)));
    }

    #[test]
    fn video_to_gif() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_movie(dir.path(), "src.mov");
        let out = dir.path().join("clip.gif");
        let req = EditRequest {
            source: src.display().to_string(),
            end: Some(-1.0),
            preset: ExportPreset::Gif,
            fps: Some(5),
            max_size: Some(80),
            output: Some(out.display().to_string()),
            ..Default::default()
        };
        let m = engine.edit(&req).unwrap();
        assert_eq!(m.kind, MediaKind::Gif);
        assert_eq!((m.width, m.height), (Some(80), Some(60)));
        let tl = gif_timeline(&out);
        assert_eq!(tl.len(), 10, "2 s at 5 fps");
        assert!(tl.iter().all(|(d, _)| (d - 0.2).abs() < 1e-9));
        assert!(near(&image::Rgba([tl[0].1[0], tl[0].1[1], tl[0].1[2], 255]), RED));
        assert!(near(&image::Rgba([tl[9].1[0], tl[9].1[1], tl[9].1[2], 255]), GREEN));
    }

    #[test]
    fn gif_cut_is_frame_exact_and_gif_resamples() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_gif(dir.path());
        let out = dir.path().join("cut.gif");
        let req = EditRequest {
            source: src.display().to_string(),
            start: Some(0.15),
            end: Some(0.45),
            output: Some(out.display().to_string()),
            ..Default::default()
        };
        let m = engine.edit(&req).unwrap();
        assert!((m.duration_seconds.unwrap() - 0.3).abs() < 1e-9);
        let tl = gif_timeline(&out);
        let delays: Vec<u32> = tl.iter().map(|(d, _)| (d * 1000.0).round() as u32).collect();
        assert_eq!(delays, vec![50, 100, 100, 50]);
        assert_eq!(tl.iter().map(|t| t.1).collect::<Vec<_>>(), vec![RED, GREEN, GREEN, BLUE]);

        // gif-small resamples to 8 fps (frames are already under 480 px).
        let small = dir.path().join("small.gif");
        let req = EditRequest { source: src.display().to_string(), preset: ExportPreset::GifSmall, output: Some(small.display().to_string()), ..Default::default() };
        engine.edit(&req).unwrap();
        let tl = gif_timeline(&small);
        assert_eq!(tl.len(), 5, "0.6 s at 8 fps");
        assert!(tl.iter().all(|(d, _)| (d - 0.13).abs() < 0.011));
    }

    #[test]
    fn gif_to_video() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_gif(dir.path());
        let req = EditRequest { source: src.display().to_string(), start: Some(0.2), preset: ExportPreset::P720, ..Default::default() };
        let m = engine.edit(&req).unwrap();
        assert_eq!(m.kind, MediaKind::Video);
        assert_eq!(m.path.extension().unwrap(), "mp4");
        assert_eq!((m.width, m.height), (Some(40), Some(30)));
        assert!((m.duration_seconds.unwrap() - 0.4).abs() < 0.05, "{:?}", m.duration_seconds);
        let mut r = VideoFrameReader::open(&m.path, 0.0).unwrap();
        assert!(near(r.frame_at(0.05).unwrap().get_pixel(20, 15), GREEN));
        assert!(near(r.frame_at(0.35).unwrap().get_pixel(20, 15), BLUE));
    }

    #[test]
    fn rejects_bad_requests() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_movie(dir.path(), "src.mp4");
        let png = dir.path().join("shot.png");
        solid(4, 4, RED).save(&png).unwrap();
        let s = src.display().to_string();
        let cases = [
            EditRequest { source: png.display().to_string(), ..Default::default() },
            EditRequest { source: s.clone(), output: Some(s.clone()), ..Default::default() },
            EditRequest { source: s.clone(), start: Some(2.5), end: Some(1.0), ..Default::default() },
            EditRequest { source: s.clone(), start: Some(10.0), ..Default::default() },
            EditRequest { source: s.clone(), preset: ExportPreset::P720, output: Some(dir.path().join("x.gif").display().to_string()), ..Default::default() },
            EditRequest { source: s.clone(), fps: Some(0), ..Default::default() },
        ];
        for req in &cases {
            let e = engine.edit(req).unwrap_err();
            assert!(invalid(&e), "{req:?}: {e:#}");
        }
        assert!(engine.history.latest().unwrap().is_none(), "nothing recorded");
        let missing = engine.edit(&EditRequest { source: "/nonexistent/clip.mp4".into(), ..Default::default() }).unwrap_err();
        assert!(matches!(missing.downcast_ref::<BugleError>(), Some(BugleError::MediaNotFound(_))));
    }
}
