//! Post-capture editing: trim a recording and export it with a preset.
//!
//! * Video → video: one `AVAssetExportSession` over the trimmed time range;
//!   audio tracks are kept. `original` copies the samples without
//!   re-encoding when the cut starts on a keyframe, and re-encodes otherwise:
//!   a copied cut between keyframes would keep the frames back to the
//!   previous keyframe in the file (hidden by an edit list), and trimmed-off
//!   footage must really be gone. `lossless` always copies and accepts that.
//!   Every re-encode renders through a video composition, which stores only
//!   frames inside the range and is how `fps` resamples.
//! * Video → GIF: frames decoded exactly (no keyframe snapping) at the GIF
//!   rate, scaled, and streamed into the GIF encoder.
//! * GIF → GIF: a frame-exact cut (the delays of the frames at the cut points
//!   are shortened), or a resample at a new rate.
//! * GIF → video: frames resampled to a constant rate and encoded to H.264
//!   (HEVC with `hevc`).
//!
//! * Event-track effects (`zoom`, `smooth_cursor`; see [`crate::cursor_fx`]):
//!   frames are rendered one by one. Video → video decodes the range in
//!   order (`AVAssetReader`), applies the effects, encodes with
//!   `AVAssetWriter` and copies the source's audio over the same range back
//!   in; the other pipelines apply them to the frames they already decode.
//!   A plain edit of a recording that has a track writes the trimmed track
//!   next to its output, so it can be zoomed later.
//!
//! GIF sources are streamed (one pass for the timeline, one for the frames),
//! never held in memory whole. GIF delays are written in centiseconds with
//! the rounding error carried forward, so the output keeps its length.
//!
//! The result is written under a temporary name next to the output and
//! renamed into place, so a failed export never leaves a partial file, and
//! it is added to history (with the source as `parent` when the source is a
//! history item).

use crate::cursor_fx::{Effects, MAX_ZOOM};
use crate::Engine;
use anyhow::{anyhow, Context, Result};
use bugle_core::config::RecordingConfig;
use bugle_core::{BugleError, EventTrack, MediaItem, MediaKind};
use bugle_platform::{SequentialFrameReader, VideoExportPreset, VideoFrameReader, VideoFrameWriter};
use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::{AnimationDecoder, Delay, Frame, ImageDecoder, RgbaImage};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Shortest clip an edit may produce, in seconds.
pub const MIN_CLIP_SECONDS: f64 = 0.05;
/// GIF frame rates are capped here (delays are stored in 10 ms units).
const GIF_MAX_FPS: u32 = 50;
const VIDEO_MAX_FPS: u32 = 120;
/// `gif-small` defaults.
const GIF_SMALL_FPS: u32 = 8;
const GIF_SMALL_MAX_SIZE: u32 = 480;
/// Shortest GIF delay players honour (shorter ones are shown as 100 ms).
const GIF_MIN_DELAY_CS: u32 = 2;
/// Decoder limits for GIF sources (any path is accepted over the API).
const GIF_MAX_SIDE: u32 = 16_384;
const GIF_MAX_ALLOC: u64 = 512 << 20;
/// Edits one engine runs at once; more are refused.
pub const MAX_CONCURRENT_EDITS: usize = 2;

/// What an edit exports to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ExportPreset {
    /// Keep the source format. Video cuts on a keyframe are copied without
    /// re-encoding; other cuts are re-encoded so nothing before them stays
    /// in the file.
    #[default]
    #[serde(rename = "original")]
    Original,
    /// Always copy video samples (fast, no quality loss). A cut between
    /// keyframes keeps up to one keyframe interval before the start in the
    /// file, hidden from playback.
    #[serde(rename = "lossless")]
    Lossless,
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
    /// Animated GIF; from a video at `recordings.gif_frame_rate` and
    /// `gif_max_size`, from a GIF keeping its timing, scaled to `gif_max_size`.
    #[serde(rename = "gif")]
    Gif,
    /// Animated GIF at 8 fps, long edge 480 px (chat-sized).
    #[serde(rename = "gif-small")]
    GifSmall,
}

impl ExportPreset {
    pub const ALL: [ExportPreset; 9] = [
        ExportPreset::Original,
        ExportPreset::Lossless,
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
            ExportPreset::Lossless => "lossless",
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
            ExportPreset::Original => {
                "same format as the source; copied without re-encoding when the cut starts on a keyframe, \
                 otherwise re-encoded so nothing before the cut stays in the file"
            }
            ExportPreset::Lossless => {
                "always copies video samples (fast, no quality loss); WARNING: a cut between keyframes keeps \
                 up to one keyframe interval of footage before the start in the file, hidden but recoverable"
            }
            ExportPreset::High => "H.264 at the source size",
            ExportPreset::Hevc => "HEVC (H.265) at the source size",
            ExportPreset::P1080 => "H.264 scaled down to fit 1920x1080",
            ExportPreset::P720 => "H.264 scaled down to fit 1280x720",
            ExportPreset::P540 => "H.264 scaled down to fit 960x540",
            ExportPreset::Gif => {
                "animated GIF: from a video at recordings.gif_frame_rate, from a GIF keeping its timing; \
                 long edge recordings.gif_max_size"
            }
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
    /// Output frame rate. Re-encodes a video (not with `lossless`); resamples
    /// a GIF.
    #[serde(default)]
    pub fps: Option<u32>,
    /// Long edge of GIF output, in pixels.
    #[serde(default)]
    pub max_size: Option<u32>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    /// Zoom in on clicks to this factor (above 1, at most 4), using the
    /// recording's cursor event track.
    #[serde(default)]
    pub zoom: Option<f64>,
    /// Draw the pointer along a smoothed path (recordings made with
    /// `hide_cursor`, which have an event track and no pointer in the frames).
    #[serde(default)]
    pub smooth_cursor: bool,
}

impl EditRequest {
    /// Whether the edit needs the recording's event track.
    fn uses_events(&self) -> bool {
        self.zoom.is_some() || self.smooth_cursor
    }
}

/// The event track of a recording: the one history recorded for it, else
/// the sidecar next to the file. `None` when there is none.
fn event_track_of(item: &MediaItem) -> Option<Result<EventTrack>> {
    let path = item.events.clone().filter(|p| p.is_file()).unwrap_or_else(|| EventTrack::sidecar_path(&item.path));
    path.is_file().then(|| EventTrack::load(&path))
}

/// Check the event-track options against the track (if any).
fn check_effects(req: &EditRequest, track: Option<&EventTrack>, source: &Path) -> std::result::Result<(), BugleError> {
    if let Some(z) = req.zoom {
        if !(z.is_finite() && z > 1.0 && z <= MAX_ZOOM) {
            return Err(BugleError::Invalid(format!("zoom must be above 1 and at most {MAX_ZOOM} (got {z})")));
        }
    }
    if !req.uses_events() {
        return Ok(());
    }
    if req.preset == ExportPreset::Lossless {
        return Err(BugleError::Invalid("zoom and smooth_cursor re-render the frames; use another preset than lossless".into()));
    }
    let Some(track) = track else {
        return Err(BugleError::Invalid(format!(
            "{} has no cursor event track ({}); record with cursor_events (CLI --cursor-events) to use zoom or smooth_cursor",
            source.display(),
            EventTrack::sidecar_path(source).display()
        )));
    };
    if req.smooth_cursor && track.cursor_in_video {
        return Err(BugleError::Invalid(
            "smooth_cursor needs a recording made with hide_cursor: this one has the pointer in its frames".into(),
        ));
    }
    Ok(())
}

/// Render `start..end` of a video through `fx` at `fps` into a silent movie
/// of `size`: frames decoded in order, each output frame showing the source
/// frame on screen at the middle of its interval.
#[allow(clippy::too_many_arguments)]
fn render_effects(src: &Path, out: &Path, start: f64, end: f64, fps: u32, size: (u32, u32), mp4: bool, hevc: bool, fx: &Effects) -> Result<()> {
    let mut reader = SequentialFrameReader::open(src, start, end)?;
    let mut writer = VideoFrameWriter::create_with_codec(out, size.0, size.1, fps, mp4, hevc)?;
    let mut current: Option<RgbaImage> = None;
    let mut next = reader.next_frame()?;
    for (t, _) in sample_times(start, end, fps) {
        while next.as_ref().is_some_and(|(pts, _)| *pts <= t) {
            current = next.take().map(|(_, f)| f);
            next = reader.next_frame()?;
        }
        // Before the first decoded frame, show that one.
        let frame = current.as_ref().or(next.as_ref().map(|(_, f)| f)).ok_or_else(|| anyhow!("the source has no frames in the range"))?;
        let mut f = fx.apply(frame.clone(), t);
        if f.dimensions() != size {
            f = image::imageops::resize(&f, size.0, size.1, image::imageops::FilterType::Triangle);
        }
        writer.append(&f)?;
    }
    writer.finish()
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

    /// Whether `ext` (lowercase) is a correct extension for this container.
    fn accepts(self, ext: &str) -> bool {
        match self {
            Target::Video { mp4: true } => ext == "mp4" || ext == "m4v",
            _ => ext == self.extension(),
        }
    }
}

fn extension_of(p: &str) -> String {
    Path::new(p).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// Make an output path's extension agree with the container: a wrong one
/// is replaced (`clip.txt` → `clip.mp4`), a missing one appended.
fn fix_output_extension(path: PathBuf, target: Target) -> PathBuf {
    let ext = extension_of(&path.to_string_lossy());
    if target.accepts(&ext) {
        return path;
    }
    let fixed = if ext.is_empty() {
        let mut s = path.clone().into_os_string();
        s.push(".");
        s.push(target.extension());
        PathBuf::from(s)
    } else {
        path.with_extension(target.extension())
    };
    tracing::warn!("output {} does not match the {} output; writing {}", path.display(), target.extension(), fixed.display());
    fixed
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

/// The export-session preset for a video → video edit. `clean_cut` says
/// whether the start falls on a keyframe (so copying keeps nothing before it).
fn video_plan(preset: ExportPreset, fps: Option<u32>, clean_cut: bool) -> std::result::Result<VideoExportPreset, BugleError> {
    Ok(match preset {
        ExportPreset::Lossless if fps.is_some() => {
            return Err(BugleError::Invalid("preset lossless copies samples and cannot change the frame rate; use high".into()));
        }
        ExportPreset::Lossless => VideoExportPreset::Passthrough,
        ExportPreset::Original if fps.is_none() && clean_cut => VideoExportPreset::Passthrough,
        ExportPreset::Original => VideoExportPreset::Highest,
        ExportPreset::Hevc => VideoExportPreset::HevcHighest,
        ExportPreset::P1080 => VideoExportPreset::Fit1920x1080,
        ExportPreset::P720 => VideoExportPreset::Fit1280x720,
        ExportPreset::P540 => VideoExportPreset::Fit960x540,
        ExportPreset::High | ExportPreset::Gif | ExportPreset::GifSmall => VideoExportPreset::Highest,
    })
}

/// How long a video export may run before it is cancelled.
fn export_timeout(seconds: f64) -> f64 {
    120.0 + 10.0 * seconds.max(0.0)
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

/// Output frames for resampling `start..end` at `fps`: `(time, seconds)`,
/// where each frame covers `[start + k/fps, min(start + (k+1)/fps, end))`
/// and is sampled in the middle of that interval, so decoding never lands
/// on a frame boundary. A final sliver under 10 ms is folded into the frame
/// before it. The durations add up to `end - start`.
fn sample_times(start: f64, end: f64, fps: u32) -> Vec<(f64, f64)> {
    let step = 1.0 / fps.max(1) as f64;
    let n = (((end - start) / step) - 1e-6).ceil().max(1.0) as usize;
    let mut out: Vec<(f64, f64)> = (0..n)
        .map(|k| {
            let a = start + k as f64 * step;
            let b = (a + step).min(end);
            ((a + b) / 2.0, b - a)
        })
        .collect();
    if out.len() > 1 && out[out.len() - 1].1 < 0.01 {
        let last = out.pop().unwrap();
        out.last_mut().unwrap().1 += last.1;
    }
    out
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

/// A GIF decoder with size and allocation limits.
fn gif_decoder(path: &Path) -> Result<GifDecoder<std::io::BufReader<std::fs::File>>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut decoder = GifDecoder::new(std::io::BufReader::new(file)).with_context(|| format!("decoding {}", path.display()))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(GIF_MAX_SIDE);
    limits.max_image_height = Some(GIF_MAX_SIDE);
    limits.max_alloc = Some(GIF_MAX_ALLOC);
    decoder.set_limits(limits).with_context(|| format!("{} is too large to edit", path.display()))?;
    Ok(decoder)
}

/// Visit every frame of a GIF in order with its start time and delay
/// (seconds), one frame in memory at a time. `visit` returns `false` to
/// stop early.
fn for_each_gif_frame(path: &Path, mut visit: impl FnMut(RgbaImage, f64, f64) -> Result<bool>) -> Result<()> {
    let mut t = 0.0;
    for f in gif_decoder(path)?.into_frames() {
        let f = f.with_context(|| format!("decoding {}", path.display()))?;
        let d = gif_delay_seconds(f.delay());
        if !visit(f.into_buffer(), t, d)? {
            break;
        }
        t += d;
    }
    Ok(())
}

/// A GIF's timeline, from a streaming pass that keeps no frames.
#[derive(Debug, Clone, Copy)]
struct GifMeta {
    frames: usize,
    duration: f64,
    width: u32,
    height: u32,
}

impl GifMeta {
    fn read(path: &Path) -> Result<Self> {
        let (width, height) = gif_decoder(path)?.dimensions();
        let mut m = GifMeta { frames: 0, duration: 0.0, width, height };
        for_each_gif_frame(path, |_, _, d| {
            m.frames += 1;
            m.duration += d;
            Ok(true)
        })?;
        if m.frames == 0 {
            return Err(BugleError::Invalid(format!("{} has no frames", path.display())).into());
        }
        Ok(m)
    }

    /// Mean frame rate, for re-encoding to a constant-rate video.
    fn mean_fps(&self) -> u32 {
        ((self.frames as f64 / self.duration).round() as u32).clamp(1, 60)
    }
}

/// The part of a frame shown at `[s, s + d)` that falls inside
/// `start..end`, if it is long enough to keep (5 ms).
fn cut_overlap(s: f64, d: f64, start: f64, end: f64) -> Option<f64> {
    let shown = (s + d).min(end) - s.max(start);
    (shown >= 0.005).then_some(shown)
}

/// Stream the frames of a GIF source that `start..end` needs into `emit`
/// as `(frame, time, seconds)`: a frame-exact cut, or a resample at `fps`.
/// `time` is the source time in the middle of the output frame.
fn stream_gif_range(path: &Path, start: f64, end: f64, fps: Option<u32>, mut emit: impl FnMut(RgbaImage, f64, f64) -> Result<()>) -> Result<()> {
    match fps {
        None => for_each_gif_frame(path, |img, s, d| {
            if s >= end {
                return Ok(false);
            }
            if let Some(shown) = cut_overlap(s, d, start, end) {
                emit(img, s.max(start) + shown / 2.0, shown)?;
            }
            Ok(true)
        }),
        Some(fps) => {
            let samples = sample_times(start, end, fps);
            let mut k = 0;
            for_each_gif_frame(path, |img, s, d| {
                // Every sample time inside this frame's interval shows it.
                let mut taken = Vec::new();
                while k < samples.len() && samples[k].0 < s + d {
                    taken.push(samples[k]);
                    k += 1;
                }
                let n = taken.len();
                let mut img = Some(img);
                for (i, (t, secs)) in taken.into_iter().enumerate() {
                    let f = if i + 1 == n { img.take().expect("frame") } else { img.clone().expect("frame") };
                    emit(f, t, secs)?;
                }
                Ok(k < samples.len())
            })
        }
    }
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

/// Streams frames into a looping GIF. Delays are whole centiseconds; the
/// rounding error is carried forward (each delay ends the frame at the
/// rounded *ideal* end time), so many short frames do not drift, and no
/// delay is below [`GIF_MIN_DELAY_CS`].
struct GifSink<W: std::io::Write> {
    enc: GifEncoder<W>,
    ideal_end: f64,
    written_cs: u64,
    size: (u32, u32),
    frames: usize,
}

impl<W: std::io::Write> GifSink<W> {
    fn new(w: W) -> Result<Self> {
        let mut enc = GifEncoder::new_with_speed(w, 10);
        enc.set_repeat(Repeat::Infinite)?;
        Ok(Self { enc, ideal_end: 0.0, written_cs: 0, size: (0, 0), frames: 0 })
    }

    /// The delay (centiseconds) of the next frame lasting `secs`.
    fn next_delay_cs(&mut self, secs: f64) -> u32 {
        self.ideal_end += secs.max(0.0);
        let target = (self.ideal_end * 100.0).round() as i64;
        let cs = (target - self.written_cs as i64).max(GIF_MIN_DELAY_CS as i64) as u32;
        self.written_cs += cs as u64;
        cs
    }

    fn push(&mut self, img: RgbaImage, secs: f64) -> Result<()> {
        let cs = self.next_delay_cs(secs);
        self.size = img.dimensions();
        self.enc.encode_frame(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(cs * 10, 1)))?;
        self.frames += 1;
        Ok(())
    }

    /// Frame size and the duration actually written, in seconds.
    fn finish(self) -> Result<((u32, u32), f64)> {
        if self.frames == 0 {
            return Err(BugleError::Invalid("the edit produced no frames".into()).into());
        }
        Ok((self.size, self.written_cs as f64 / 100.0))
    }
}

fn create_gif_sink(path: &Path) -> Result<GifSink<std::io::BufWriter<std::fs::File>>> {
    let file = std::fs::File::create(path).context("creating the output file")?;
    GifSink::new(std::io::BufWriter::new(file))
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

/// Counts an engine's running edits (see [`MAX_CONCURRENT_EDITS`]).
#[derive(Default)]
pub struct EditSlots(AtomicUsize);

/// A running edit's slot; released on drop.
struct EditSlot<'a>(&'a AtomicUsize);

impl EditSlots {
    fn acquire(&self) -> Result<EditSlot<'_>> {
        let prev = self.0.fetch_add(1, Ordering::SeqCst);
        if prev >= MAX_CONCURRENT_EDITS {
            self.0.fetch_sub(1, Ordering::SeqCst);
            return Err(anyhow!("{MAX_CONCURRENT_EDITS} edits are already running; try again when one finishes"));
        }
        Ok(EditSlot(&self.0))
    }
}

impl Drop for EditSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Engine {
    /// Trim a recording and export it with a preset, as a new history item.
    /// See the module docs for the pipelines.
    pub fn edit(&self, req: &EditRequest) -> Result<MediaItem> {
        let src = self.resolve_media(&req.source)?;
        let Some(kind) = source_kind(&src.path) else {
            return Err(BugleError::Invalid(format!("{} is not a recording (.mp4, .mov, .m4v or .gif)", src.path.display())).into());
        };
        if req.fps == Some(0) || req.max_size == Some(0) {
            return Err(BugleError::Invalid("fps and max_size must be positive".into()).into());
        }
        if kind == MediaKind::Gif && req.preset == ExportPreset::Lossless {
            return Err(BugleError::Invalid("preset lossless applies to video sources; GIF cuts are always exact".into()).into());
        }
        let track = match event_track_of(&src) {
            Some(Ok(t)) => Some(t),
            Some(Err(e)) if req.uses_events() => return Err(e),
            Some(Err(e)) => {
                tracing::warn!("ignoring the cursor event track: {e:#}");
                None
            }
            None => None,
        };
        check_effects(req, track.as_ref(), &src.path)?;
        let fx = track.as_ref().filter(|_| req.uses_events()).map(|t| Effects::new(t, req.zoom, req.smooth_cursor));
        let apply = |f: RgbaImage, t: f64| match &fx {
            Some(fx) => fx.apply(f, t),
            None => f,
        };
        let src_ext = extension_of(&src.path.to_string_lossy());
        let target = choose_target(req.preset, req.output.as_deref(), &src_ext)?;
        let mut vars = HashMap::new();
        vars.insert("kind", "edit".to_string());
        if let Some(l) = &req.label {
            vars.insert("label", l.clone());
        }
        let out = fix_output_extension(self.output_path(req.output.as_deref(), target.extension(), &vars), target);
        if same_file(&src.path, &out) {
            return Err(BugleError::Invalid(format!("the output {} is the source; choose another path", out.display())).into());
        }
        let _slot = self.edit_slots.acquire()?;
        let dir = out.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        // Reserve a name beside the output, then let the writer create the
        // file itself so it gets normal permissions (tempfile's are 0600).
        let tmp = tempfile::Builder::new()
            .prefix(".bugle-edit-")
            .suffix(&format!(".{}", target.extension()))
            .tempfile_in(dir)
            .with_context(|| format!("creating a temporary file in {}", dir.display()))?
            .into_temp_path();
        std::fs::remove_file(&tmp).context("preparing the output file")?;

        let rec = &self.config.recordings;
        let no_video = || anyhow!("the export produced no video");
        let (w, h, duration, range) = match (kind, target) {
            (MediaKind::Video, Target::Video { mp4 }) if fx.is_some() => {
                let info = bugle_platform::video_info(&src.path)?;
                let (s, e) = resolve_range(req.start, req.end, info.end())?;
                let nominal = if info.nominal_fps.is_finite() && info.nominal_fps >= 1.0 { info.nominal_fps.round() as u32 } else { 30 };
                let fps = req.fps.unwrap_or(nominal).clamp(1, VIDEO_MAX_FPS);
                let (w, h) = video_frame_size(info.width.max(1) as u32, info.height.max(1) as u32, req.preset.video_box());
                let silent = tempfile::Builder::new()
                    .prefix(".bugle-edit-")
                    .suffix(&format!(".video.{}", target.extension()))
                    .tempfile_in(dir)
                    .context("creating a temporary file")?
                    .into_temp_path();
                let hevc = req.preset == ExportPreset::Hevc;
                render_effects(&src.path, &silent, s, e, fps, (w, h), mp4, hevc, fx.as_ref().expect("checked"))?;
                if !bugle_platform::mux_audio(&silent, &src.path, s, e, &tmp, mp4, Some(export_timeout(e - s)))? {
                    std::fs::rename(&silent, &tmp).context("moving the rendered video")?;
                }
                let d = bugle_platform::media_duration(&tmp).ok().filter(|d| *d > 0.0).unwrap_or(e - s);
                (w, h, d, (s, e))
            }
            (MediaKind::Video, Target::Video { mp4 }) => {
                let fps = req.fps.map(|f| f.min(VIDEO_MAX_FPS));
                // Ranges are measured on the video track: an audio track
                // that runs longer is cut at the end of the video.
                let info = bugle_platform::video_info(&src.path)?;
                let (s, e) = resolve_range(req.start, req.end, info.end())?;
                let clean_cut = s <= 1e-6 || bugle_platform::video_sync_at(&src.path, s)?;
                let plan = video_plan(req.preset, fps, clean_cut)?;
                bugle_platform::export_video(&src.path, &tmp, s, e, plan, mp4, fps, Some(export_timeout(e - s)))?;
                let (w, h) = bugle_platform::video_size(&tmp).map_err(|_| no_video())?;
                let d = bugle_platform::media_duration(&tmp).ok().filter(|d| *d > 0.0).unwrap_or(e - s);
                (w, h, d, (s, e))
            }
            (MediaKind::Video, Target::Gif) => {
                let g = gif_settings(req.preset, req.fps, req.max_size, true, rec);
                let fps = g.fps.unwrap_or(rec.gif_frame_rate).max(1);
                let info = bugle_platform::video_info(&src.path)?;
                let (s, e) = resolve_range(req.start, req.end, info.end())?;
                // Exact decoding: any tolerance lets the generator snap to a
                // nearby keyframe, which can lie outside the cut.
                let mut reader = VideoFrameReader::open(&src.path, 0.0)?;
                let mut sink = create_gif_sink(&tmp)?;
                let mut last: Option<RgbaImage> = None;
                for (t, secs) in sample_times(s, e, fps) {
                    let frame = match reader.frame_at(t) {
                        Ok(f) => fit_gif_frame(apply(f, t), g.max_size),
                        // Past the last decodable frame: hold the previous one.
                        Err(_) if last.is_some() => last.clone().unwrap(),
                        Err(e) => return Err(e),
                    };
                    last = Some(frame.clone());
                    sink.push(frame, secs)?;
                }
                let ((w, h), d) = sink.finish()?;
                (w, h, d, (s, e))
            }
            (_, Target::Gif) => {
                let meta = GifMeta::read(&src.path)?;
                let g = gif_settings(req.preset, req.fps, req.max_size, false, rec);
                let (s, e) = resolve_range(req.start, req.end, meta.duration)?;
                let mut sink = create_gif_sink(&tmp)?;
                stream_gif_range(&src.path, s, e, g.fps, |img, t, secs| sink.push(fit_gif_frame(apply(img, t), g.max_size), secs))?;
                let ((w, h), d) = sink.finish()?;
                (w, h, d, (s, e))
            }
            (_, Target::Video { mp4 }) => {
                let meta = GifMeta::read(&src.path)?;
                let (s, e) = resolve_range(req.start, req.end, meta.duration)?;
                let fps = req.fps.unwrap_or_else(|| meta.mean_fps()).clamp(1, VIDEO_MAX_FPS);
                let (w, h) = video_frame_size(meta.width, meta.height, req.preset.video_box());
                let hevc = req.preset == ExportPreset::Hevc;
                let mut writer = VideoFrameWriter::create_with_codec(&tmp, w, h, fps, mp4, hevc)?;
                stream_gif_range(&src.path, s, e, Some(fps), |f, t, _| {
                    let f = apply(f, t);
                    let f = if f.dimensions() == (w, h) { f } else { image::imageops::resize(&f, w, h, image::imageops::FilterType::Triangle) };
                    writer.append(&f)
                })?;
                writer.finish()?;
                let d = bugle_platform::media_duration(&tmp).ok().filter(|d| *d > 0.0).unwrap_or(e - s);
                (w, h, d, (s, e))
            }
        };
        tmp.persist(&out).with_context(|| format!("moving the export to {}", out.display()))?;

        let in_history = self.history.all().map(|all| all.iter().any(|m| m.id == src.id)).unwrap_or(false);
        let mut m = MediaItem::new(if target == Target::Gif { MediaKind::Gif } else { MediaKind::Video }, &out);
        m.width = Some(w);
        m.height = Some(h);
        m.duration_seconds = Some(duration);
        m.label = req.label.clone();
        let mut what = format!("{:.2}s–{:.2}s, preset {}", range.0, range.1, req.preset);
        if let Some(z) = req.zoom {
            what.push_str(&format!(", zoom {z}x on clicks"));
        }
        if req.smooth_cursor {
            what.push_str(", smoothed cursor");
        }
        // A plain edit keeps the track (trimmed) so the output can be zoomed
        // later; after effects the pointer and framing are in the pixels.
        if let (Some(t), None) = (&track, &fx) {
            let path = EventTrack::sidecar_path(&out);
            match t.trimmed(range.0, range.1).save(&path) {
                Ok(()) => m.events = Some(path),
                Err(e) => tracing::warn!("could not write the cursor event track {}: {e:#}", path.display()),
            }
        }
        if in_history {
            m.parent = Some(src.id.clone());
            m.source = Some(format!("edit: {what}"));
        } else {
            m.source = Some(format!("edit of {}: {what}", src.path.display()));
        }
        self.history.push(&m)?;
        self.sync_after_save(&mut m);
        Ok(m)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::Config;

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

    /// A primary-coloured frame with a textured border, so a re-encode
    /// cannot reproduce it bit for bit.
    fn textured(w: u32, h: u32, c: [u8; 3], seed: u32) -> RgbaImage {
        let mut img = solid(w, h, c);
        for (x, y, p) in img.enumerate_pixels_mut() {
            if x < 16 || y < 16 {
                let v = ((x * 31 + y * 17 + seed * 7) % 251) as u8;
                *p = image::Rgba([v, v.wrapping_mul(3), v ^ 0x5a, 255]);
            }
        }
        img
    }

    /// Which primary a decoded pixel is (H.264's colour conversion shifts
    /// pure primaries a little, e.g. green comes back as ~(68, 251, 0)).
    fn near(p: &image::Rgba<u8>, c: [u8; 3]) -> bool {
        (0..3).all(|i| if c[i] == 255 { p[i] > 180 } else { p[i] < 120 })
    }

    fn near_rgb(p: [u8; 3], c: [u8; 3]) -> bool {
        near(&image::Rgba([p[0], p[1], p[2], 255]), c)
    }

    /// 3 s, 160x120 at 10 fps, keyframe every second: one second each of
    /// red, green, blue.
    fn fixture_movie(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        let mp4 = extension_of(name) != "mov";
        let mut w = VideoFrameWriter::create(&p, 160, 120, 10, mp4).unwrap();
        for i in 0..30u32 {
            w.append(&textured(160, 120, [RED, GREEN, BLUE][i as usize / 10], i)).unwrap();
        }
        w.finish().unwrap();
        p
    }

    /// 6 frames of 100 ms, 40x30: red, red, green, green, blue, blue.
    fn fixture_gif(dir: &Path) -> PathBuf {
        let p = dir.join("fixture.gif");
        let mut sink = create_gif_sink(&p).unwrap();
        for i in 0..6 {
            sink.push(solid(40, 30, [RED, GREEN, BLUE][i / 2]), 0.1).unwrap();
        }
        sink.finish().unwrap();
        p
    }

    /// (delay in centiseconds, colour of the centre pixel) per frame.
    fn gif_timeline(p: &Path) -> Vec<(u32, [u8; 3])> {
        let mut out = vec![];
        for_each_gif_frame(p, |f, _, d| {
            let px = f.get_pixel(f.width() / 2, f.height() / 2);
            out.push(((d * 100.0).round() as u32, [px[0], px[1], px[2]]));
            Ok(true)
        })
        .unwrap();
        out
    }

    fn path_str(p: &Path) -> String {
        p.display().to_string()
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
        assert!(err.contains("720p") && err.contains("lossless"), "{err}");
        assert!(ExportPreset::Lossless.description().contains("WARNING"));
        let req: EditRequest = serde_json::from_value(serde_json::json!({"source":"x","end":-1,"preset":"720p"})).unwrap();
        assert_eq!((req.preset, req.end, req.start), (ExportPreset::P720, Some(-1.0), None));
        let req: EditRequest = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(req.preset, ExportPreset::Original);
    }

    #[test]
    fn targets_and_extensions() {
        use ExportPreset::*;
        assert_eq!(choose_target(Original, None, "mov").unwrap(), Target::Video { mp4: false });
        assert_eq!(choose_target(Lossless, None, "mp4").unwrap(), Target::Video { mp4: true });
        assert_eq!(choose_target(Original, None, "gif").unwrap(), Target::Gif);
        assert_eq!(choose_target(P720, None, "gif").unwrap(), Target::Video { mp4: true });
        assert_eq!(choose_target(Hevc, None, "mov").unwrap(), Target::Video { mp4: false });
        assert_eq!(choose_target(Gif, None, "mp4").unwrap(), Target::Gif);
        assert_eq!(choose_target(Original, Some("/x/clip.mov"), "mp4").unwrap(), Target::Video { mp4: false });
        assert_eq!(choose_target(High, Some("clip.M4V"), "mov").unwrap(), Target::Video { mp4: true });
        assert_eq!(choose_target(Original, Some("clip.gif"), "mp4").unwrap(), Target::Gif);
        assert_eq!(choose_target(GifSmall, Some("/tmp/out"), "mp4").unwrap(), Target::Gif);
        assert_eq!(choose_target(High, Some("notes.txt"), "mov").unwrap(), Target::Video { mp4: false });
        assert!(choose_target(P1080, Some("clip.gif"), "mp4").is_err());
        assert!(choose_target(Gif, Some("clip.mp4"), "mp4").is_err());

        let mp4 = Target::Video { mp4: true };
        assert_eq!(fix_output_extension(PathBuf::from("/t/notes.txt"), mp4), PathBuf::from("/t/notes.mp4"));
        assert_eq!(fix_output_extension(PathBuf::from("/t/clip"), mp4), PathBuf::from("/t/clip.mp4"));
        assert_eq!(fix_output_extension(PathBuf::from("/t/v1.2"), Target::Gif), PathBuf::from("/t/v1.gif"));
        assert_eq!(fix_output_extension(PathBuf::from("/t/clip.M4V"), mp4), PathBuf::from("/t/clip.M4V"));
        assert_eq!(fix_output_extension(PathBuf::from("/t/clip.MOV"), Target::Video { mp4: false }), PathBuf::from("/t/clip.MOV"));
    }

    #[test]
    fn video_plans() {
        use ExportPreset::*;
        assert_eq!(video_plan(Original, None, true).unwrap(), VideoExportPreset::Passthrough);
        assert_eq!(video_plan(Original, None, false).unwrap(), VideoExportPreset::Highest, "unclean cut re-encodes");
        assert_eq!(video_plan(Original, Some(15), true).unwrap(), VideoExportPreset::Highest);
        assert_eq!(video_plan(Lossless, None, false).unwrap(), VideoExportPreset::Passthrough);
        assert!(video_plan(Lossless, Some(15), true).is_err());
        assert_eq!(video_plan(High, None, true).unwrap(), VideoExportPreset::Highest);
        assert_eq!(video_plan(Hevc, Some(30), true).unwrap(), VideoExportPreset::HevcHighest);
        assert_eq!(video_plan(P1080, None, true).unwrap(), VideoExportPreset::Fit1920x1080);
        assert_eq!(video_plan(P720, None, true).unwrap(), VideoExportPreset::Fit1280x720);
        assert_eq!(video_plan(P540, None, true).unwrap(), VideoExportPreset::Fit960x540);
        assert!(export_timeout(3600.0) > 3600.0 && export_timeout(0.0) >= 60.0);
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
    fn sampling_covers_the_range_exactly() {
        assert_eq!(sample_times(0.0, 1.0, 4), vec![(0.125, 0.25), (0.375, 0.25), (0.625, 0.25), (0.875, 0.25)]);
        assert_eq!(sample_times(1.0, 1.5, 2), vec![(1.25, 0.5)]);
        // Shorter than one frame: one frame, inside the range.
        let t = sample_times(2.0, 2.05, 10);
        assert_eq!(t.len(), 1);
        assert!((t[0].0 - 2.025).abs() < 1e-9 && (t[0].1 - 0.05).abs() < 1e-9);
        // Float noise does not add a frame; a short last frame is shortened.
        assert_eq!(sample_times(0.1, 0.4, 10).len(), 3);
        let t = sample_times(0.0, 0.6, 8);
        assert_eq!(t.len(), 5);
        assert!((t[4].1 - 0.1).abs() < 1e-9);
        // A last sliver under 10 ms is folded into the previous frame.
        assert_eq!(sample_times(0.0, 1.005, 10).len(), 10);
        for (start, end, fps) in [(0.0, 1.0, 4), (0.3, 2.71, 12), (0.0, 0.6, 8), (5.0, 5.05, 30), (0.0, 1.005, 10)] {
            let total: f64 = sample_times(start, end, fps).iter().map(|s| s.1).sum();
            assert!((total - (end - start)).abs() < 1e-9, "{start}..{end}@{fps}");
        }

        assert_eq!(cut_overlap(0.1, 0.1, 0.15, 0.32).map(|v| (v * 1000.0).round()), Some(50.0));
        assert_eq!(cut_overlap(0.0, 0.1, 0.098, 0.2), None, "slivers are dropped");

        assert_eq!(video_frame_size(160, 120, None), (160, 120));
        assert_eq!(video_frame_size(41, 31, None), (40, 30));
        assert_eq!(video_frame_size(3840, 2160, Some((1280, 720))), (1280, 720));
        assert_eq!(video_frame_size(1000, 2000, Some((1280, 720))), (360, 720));
        assert_eq!(video_frame_size(320, 240, Some((1280, 720))), (320, 240), "never upscaled");
        assert_eq!(video_frame_size(1, 1, None), (2, 2));
    }

    #[test]
    fn gif_delays_carry_rounding_and_respect_the_minimum() {
        let mut sink = GifSink::new(Vec::new()).unwrap();
        let cs: Vec<u32> = sample_times(0.0, 0.6, 8).into_iter().map(|(_, d)| sink.next_delay_cs(d)).collect();
        assert_eq!(cs, vec![13, 12, 13, 12, 10]);
        assert_eq!(sink.written_cs, 60);
        // 30 fps for 1 s: 3.33 cs frames, exactly 1 s in total (no drift).
        let mut sink = GifSink::new(Vec::new()).unwrap();
        let total: u32 = (0..30).map(|_| sink.next_delay_cs(1.0 / 30.0)).sum();
        assert_eq!(total, 100);
        // 12 fps for 4 s ends at 4.00 s.
        let mut sink = GifSink::new(Vec::new()).unwrap();
        let total: u32 = (0..48).map(|_| sink.next_delay_cs(1.0 / 12.0)).sum();
        assert_eq!(total, 400);
        // Nothing below 2 cs, even for slivers.
        let mut sink = GifSink::new(Vec::new()).unwrap();
        assert_eq!(sink.next_delay_cs(0.005), GIF_MIN_DELAY_CS);
    }

    #[test]
    fn keyframe_cut_is_copied_bit_for_bit() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_movie(dir.path(), "src.mp4");
        let before = std::fs::read(&src).unwrap();
        let out = dir.path().join("cut.mp4");
        let req = EditRequest {
            source: path_str(&src),
            start: Some(1.0),
            end: Some(-1.0),
            output: Some(path_str(&out)),
            label: Some("demo".into()),
            ..Default::default()
        };
        let m = engine.edit(&req).unwrap();
        assert_eq!((m.path.as_path(), m.kind), (out.as_path(), MediaKind::Video));
        assert_eq!((m.width, m.height), (Some(160), Some(120)));
        assert!((m.duration_seconds.unwrap() - 1.0).abs() < 0.05, "{:?}", m.duration_seconds);
        // Lossless: decoded frames are identical to the source's.
        let (mut a, mut b) = (VideoFrameReader::open(&src, 0.0).unwrap(), VideoFrameReader::open(&out, 0.0).unwrap());
        for t in [0.05, 0.45, 0.95] {
            assert!(a.frame_at(1.0 + t).unwrap() == b.frame_at(t).unwrap(), "frame at {t} differs");
        }
        assert_eq!(bugle_platform::video_info(&out).unwrap().stored_samples, Some(10));
        // A path source is not in history: no dangling parent.
        assert_eq!(m.parent, None);
        assert!(m.source.as_deref().unwrap().contains("src.mp4"), "{:?}", m.source);
        assert_eq!(std::fs::read(&src).unwrap(), before, "source untouched");
        let latest = engine.history.latest().unwrap().unwrap();
        assert_eq!((latest.id.as_str(), latest.label.as_deref()), (m.id.as_str(), Some("demo")));
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".bugle-edit-"))
            .count();
        assert_eq!(leftovers, 0);

        // "latest" is a history item, so the next edit records it as parent.
        let req = EditRequest { source: "latest".into(), end: Some(0.5), output: Some(path_str(&dir.path().join("again.mp4"))), ..Default::default() };
        assert_eq!(engine.edit(&req).unwrap().parent.as_deref(), Some(m.id.as_str()));
    }

    #[test]
    fn cut_between_keyframes_leaves_nothing_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_movie(dir.path(), "src.mp4");
        let edit = |preset, start: f64, end: f64, name: &str| {
            let out = dir.path().join(name);
            let req = EditRequest { source: path_str(&src), start: Some(start), end: Some(end), preset, output: Some(path_str(&out)), ..Default::default() };
            engine.edit(&req).unwrap();
            let info = bugle_platform::video_info(&out).unwrap();
            (out, info)
        };
        // original, start at 0.5 (keyframes at 0, 1, 2): re-encoded, and the
        // file stores only the 10 frames of the range.
        let (safe, info) = edit(ExportPreset::Original, 0.5, 1.5, "safe.mp4");
        assert_eq!(info.stored_samples, Some(10), "{info:?}");
        let mut r = VideoFrameReader::open(&safe, 0.0).unwrap();
        assert!(near(r.frame_at(0.25).unwrap().get_pixel(80, 60), RED));
        assert!(near(r.frame_at(0.75).unwrap().get_pixel(80, 60), GREEN));
        // A clean start with an end cut is copied and stores only the range.
        let (_, info) = edit(ExportPreset::Original, 0.0, 1.5, "head.mp4");
        assert_eq!(info.stored_samples, Some(15), "{info:?}");
        // lossless copies anyway, keeping the hidden frames back to 0.0.
        let (_, info) = edit(ExportPreset::Lossless, 0.5, 1.5, "fast.mp4");
        assert!(info.stored_samples.unwrap() > 10, "{info:?}");
    }

    #[test]
    fn fps_resamples_video() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_movie(dir.path(), "src.mov");
        for (preset, name) in [(ExportPreset::High, "high.mov"), (ExportPreset::Original, "orig.mov"), (ExportPreset::P540, "small.mov")] {
            let out = dir.path().join(name);
            let req = EditRequest { source: path_str(&src), end: Some(2.0), preset, fps: Some(5), output: Some(path_str(&out)), ..Default::default() };
            engine.edit(&req).unwrap();
            let info = bugle_platform::video_info(&out).unwrap();
            assert_eq!(info.stored_samples, Some(10), "{preset}: 2 s at 5 fps, {info:?}");
            assert!((info.nominal_fps - 5.0).abs() < 0.5, "{preset}: {info:?}");
        }
        let req = EditRequest { source: path_str(&src), preset: ExportPreset::Lossless, fps: Some(5), ..Default::default() };
        assert!(invalid(&engine.edit(&req).unwrap_err()));
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
        let req = EditRequest { source: path_str(&big), preset: ExportPreset::P540, ..Default::default() };
        let m = engine.edit(&req).unwrap();
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
            source: path_str(&src),
            end: Some(-1.0),
            preset: ExportPreset::Gif,
            fps: Some(5),
            max_size: Some(80),
            output: Some(path_str(&out)),
            ..Default::default()
        };
        let m = engine.edit(&req).unwrap();
        assert_eq!(m.kind, MediaKind::Gif);
        assert_eq!((m.width, m.height), (Some(80), Some(60)));
        assert_eq!(m.duration_seconds, Some(2.0));
        let tl = gif_timeline(&out);
        assert_eq!(tl.len(), 10, "2 s at 5 fps");
        assert!(tl.iter().all(|(cs, _)| *cs == 20));
        assert!(near_rgb(tl[0].1, RED));
        assert!(near_rgb(tl[9].1, GREEN));
        // Normal permissions, like any newly created file here.
        use std::os::unix::fs::PermissionsExt;
        let probe = dir.path().join("probe");
        std::fs::write(&probe, b"").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&out), mode(&probe));
    }

    /// A movie whose audio outlasts its video (made with ffmpeg; skipped
    /// when ffmpeg is not installed).
    #[test]
    fn video_shorter_than_audio() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("long-audio.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-t", "1", "-i", "color=red:s=64x48:r=10"])
            .args(["-f", "lavfi", "-t", "2", "-i", "sine=frequency=440", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            eprintln!("ffmpeg (with libx264) not available: skipping");
            return;
        }
        let engine = engine_in(dir.path());
        assert!(bugle_platform::media_duration(&src).unwrap() > 1.5);
        let req = EditRequest { source: path_str(&src), preset: ExportPreset::GifSmall, ..Default::default() };
        let m = engine.edit(&req).unwrap();
        assert!((m.duration_seconds.unwrap() - 1.0).abs() < 0.06, "{:?}", m.duration_seconds);
        // Past the end of the video: a clear error that names no temp file.
        let req = EditRequest { source: path_str(&src), start: Some(1.5), ..Default::default() };
        let e = engine.edit(&req).unwrap_err();
        assert!(invalid(&e), "{e:#}");
        assert!(!format!("{e:#}").contains(".bugle-edit"), "{e:#}");
    }

    #[test]
    fn gif_cut_is_frame_exact_and_gif_resamples() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_gif(dir.path());
        let out = dir.path().join("cut.gif");
        let req = EditRequest { source: path_str(&src), start: Some(0.15), end: Some(0.45), output: Some(path_str(&out)), ..Default::default() };
        let m = engine.edit(&req).unwrap();
        assert_eq!(m.duration_seconds, Some(0.3));
        let tl = gif_timeline(&out);
        assert_eq!(tl.iter().map(|t| t.0).collect::<Vec<_>>(), vec![5, 10, 10, 5]);
        assert_eq!(tl.iter().map(|t| t.1).collect::<Vec<_>>(), vec![RED, GREEN, GREEN, BLUE]);

        // gif-small resamples to 8 fps and keeps the length exactly.
        let small = dir.path().join("small.gif");
        let req = EditRequest { source: path_str(&src), preset: ExportPreset::GifSmall, output: Some(path_str(&small)), ..Default::default() };
        let m = engine.edit(&req).unwrap();
        assert_eq!(m.duration_seconds, Some(0.6));
        let tl = gif_timeline(&small);
        assert_eq!(tl.iter().map(|t| t.0).collect::<Vec<_>>(), vec![13, 12, 13, 12, 10]);
        assert_eq!(tl.iter().map(|t| t.1).collect::<Vec<_>>(), vec![RED, RED, GREEN, BLUE, BLUE]);
    }

    #[test]
    fn gif_to_video_h264_and_hevc() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_gif(dir.path());
        let req = EditRequest { source: path_str(&src), start: Some(0.2), preset: ExportPreset::P720, ..Default::default() };
        let m = engine.edit(&req).unwrap();
        assert_eq!(m.kind, MediaKind::Video);
        assert_eq!(m.path.extension().unwrap(), "mp4");
        assert_eq!((m.width, m.height), (Some(40), Some(30)));
        assert!((m.duration_seconds.unwrap() - 0.4).abs() < 0.05, "{:?}", m.duration_seconds);
        let mut r = VideoFrameReader::open(&m.path, 0.0).unwrap();
        assert!(near(r.frame_at(0.05).unwrap().get_pixel(20, 15), GREEN));
        assert!(near(r.frame_at(0.35).unwrap().get_pixel(20, 15), BLUE));
        let has = |p: &Path, tag: &[u8]| std::fs::read(p).unwrap().windows(4).any(|w| w == tag);
        assert!(has(&m.path, b"avc1"));

        let req = EditRequest { source: path_str(&src), preset: ExportPreset::Hevc, ..Default::default() };
        let m = engine.edit(&req).unwrap();
        assert!(has(&m.path, b"hvc1") || has(&m.path, b"hev1"), "HEVC sample entry expected");
        assert!(!has(&m.path, b"avc1"));
    }

    #[test]
    fn gif_sources_are_size_limited() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        // A 1x1 image on a 20000x10 logical screen.
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&[0x20, 0x4E, 0x0A, 0x00, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0x80, 0, 0, 0, 255, 255, 255]);
        bytes.extend_from_slice(&[0x02, 0x02, 0x44, 0x01, 0x00, 0x3B]);
        let p = dir.path().join("wide.gif");
        std::fs::write(&p, &bytes).unwrap();
        assert!(engine.edit(&EditRequest { source: path_str(&p), ..Default::default() }).is_err());
        assert!(engine.history.latest().unwrap().is_none());
    }

    #[test]
    fn output_extension_is_corrected_and_unrelated_files_kept() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_gif(dir.path());
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, b"keep me").unwrap();
        let req = EditRequest { source: path_str(&src), preset: ExportPreset::High, output: Some(path_str(&notes)), ..Default::default() };
        let m = engine.edit(&req).unwrap();
        assert_eq!(m.path, dir.path().join("notes.mp4"));
        assert_eq!(std::fs::read(&notes).unwrap(), b"keep me");
        let req = EditRequest { source: path_str(&src), output: Some(path_str(&dir.path().join("clip"))), ..Default::default() };
        assert_eq!(engine.edit(&req).unwrap().path, dir.path().join("clip.gif"));
    }

    #[test]
    fn concurrent_edits_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_gif(dir.path());
        let held: Vec<_> = (0..MAX_CONCURRENT_EDITS).map(|_| engine.edit_slots.acquire().unwrap()).collect();
        let e = engine.edit(&EditRequest { source: path_str(&src), ..Default::default() }).unwrap_err();
        assert!(e.to_string().contains("already running"), "{e}");
        drop(held);
        engine.edit(&EditRequest { source: path_str(&src), ..Default::default() }).unwrap();
    }

    #[test]
    fn rejects_bad_requests() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = fixture_movie(dir.path(), "src.mp4");
        let gif = fixture_gif(dir.path());
        let png = dir.path().join("shot.png");
        solid(4, 4, RED).save(&png).unwrap();
        let s = path_str(&src);
        let cases = [
            EditRequest { source: path_str(&png), ..Default::default() },
            EditRequest { source: s.clone(), output: Some(s.clone()), ..Default::default() },
            EditRequest { source: s.clone(), start: Some(2.5), end: Some(1.0), ..Default::default() },
            EditRequest { source: s.clone(), start: Some(10.0), ..Default::default() },
            EditRequest { source: s.clone(), preset: ExportPreset::P720, output: Some(path_str(&dir.path().join("x.gif"))), ..Default::default() },
            EditRequest { source: s.clone(), fps: Some(0), ..Default::default() },
            EditRequest { source: path_str(&gif), preset: ExportPreset::Lossless, ..Default::default() },
        ];
        for req in &cases {
            let e = engine.edit(req).unwrap_err();
            assert!(invalid(&e), "{req:?}: {e:#}");
        }
        assert!(engine.history.latest().unwrap().is_none(), "nothing recorded");
        let missing = engine.edit(&EditRequest { source: "/nonexistent/clip.mp4".into(), ..Default::default() }).unwrap_err();
        assert!(matches!(missing.downcast_ref::<BugleError>(), Some(BugleError::MediaNotFound(_))));
    }

    use bugle_core::events::{CursorEvent, EventKind, MouseButton};

    fn click_at(t: f64, x: f64, y: f64) -> CursorEvent {
        CursorEvent { t, kind: EventKind::Down, button: Some(MouseButton::Left), x, y }
    }

    /// 2 s, 160x120 at 10 fps: left half red, right half blue.
    fn split_movie(dir: &Path) -> PathBuf {
        let p = dir.join("split.mp4");
        let mut frame = solid(160, 120, BLUE);
        for y in 0..120 {
            for x in 0..80 {
                frame.put_pixel(x, y, image::Rgba([255, 0, 0, 255]));
            }
        }
        let mut w = VideoFrameWriter::create(&p, 160, 120, 10, true).unwrap();
        for _ in 0..20 {
            w.append(&frame).unwrap();
        }
        w.finish().unwrap();
        p
    }

    #[test]
    fn event_options_need_a_matching_track() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = split_movie(dir.path());
        let s = path_str(&src);
        let no_track = engine.edit(&EditRequest { source: s.clone(), zoom: Some(2.0), ..Default::default() }).unwrap_err();
        assert!(invalid(&no_track) && format!("{no_track:#}").contains("cursor_events"), "{no_track:#}");
        // 80x60 pt area recorded at 2 px/pt, pointer in the frames.
        let track = EventTrack::new(bugle_core::Rect::new(0.0, 0.0, 80.0, 60.0), true, 2.0, vec![click_at(1.0, 60.0, 30.0)]);
        track.save(&EventTrack::sidecar_path(&src)).unwrap();
        for req in [
            EditRequest { source: s.clone(), zoom: Some(1.0), ..Default::default() },
            EditRequest { source: s.clone(), zoom: Some(f64::NAN), ..Default::default() },
            EditRequest { source: s.clone(), zoom: Some(2.0), preset: ExportPreset::Lossless, ..Default::default() },
            EditRequest { source: s.clone(), smooth_cursor: true, ..Default::default() },
        ] {
            let e = engine.edit(&req).unwrap_err();
            assert!(invalid(&e), "{req:?}: {e:#}");
        }
        // A broken track is an error only when it is needed.
        std::fs::write(EventTrack::sidecar_path(&src), b"{}").unwrap();
        assert!(engine.edit(&EditRequest { source: s.clone(), zoom: Some(2.0), ..Default::default() }).is_err());
        engine.edit(&EditRequest { source: s, end: Some(1.0), ..Default::default() }).unwrap();
    }

    #[test]
    fn zoom_follows_the_click_and_plain_edits_keep_the_track() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        let src = split_movie(dir.path());
        let track = EventTrack::new(
            bugle_core::Rect::new(100.0, 100.0, 80.0, 60.0),
            true,
            2.0,
            vec![CursorEvent::moved(0.0, 10.0, 10.0), click_at(1.0, 60.0, 30.0)],
        );
        track.save(&EventTrack::sidecar_path(&src)).unwrap();

        let out = dir.path().join("zoomed.mp4");
        let req = EditRequest { source: path_str(&src), zoom: Some(2.0), output: Some(path_str(&out)), ..Default::default() };
        let m = engine.edit(&req).unwrap();
        assert_eq!((m.width, m.height), (Some(160), Some(120)));
        assert!((m.duration_seconds.unwrap() - 2.0).abs() < 0.05, "{:?}", m.duration_seconds);
        assert!(m.source.as_deref().unwrap().contains("zoom 2x"), "{:?}", m.source);
        assert_eq!(m.events, None, "the framing is in the pixels now");
        assert!(!EventTrack::sidecar_path(&out).exists());
        let mut r = VideoFrameReader::open(&out, 0.0).unwrap();
        // Before the zoom: both halves. At the click: only the blue half.
        let f = r.frame_at(0.15).unwrap();
        assert!(near(f.get_pixel(20, 60), RED) && near(f.get_pixel(140, 60), BLUE));
        let f = r.frame_at(1.05).unwrap();
        assert!(near(f.get_pixel(5, 60), BLUE) && near(f.get_pixel(155, 60), BLUE));
        let leftovers = std::fs::read_dir(dir.path()).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with(".bugle-edit-")).count();
        assert_eq!(leftovers, 0);

        // To GIF, the same effects apply.
        let gif = dir.path().join("zoomed.gif");
        let req = EditRequest { source: path_str(&src), start: Some(0.9), end: Some(1.2), zoom: Some(2.0), preset: ExportPreset::Gif, output: Some(path_str(&gif)), ..Default::default() };
        engine.edit(&req).unwrap();
        for (_, c) in gif_timeline(&gif) {
            assert!(near_rgb(c, BLUE), "{c:?}");
        }

        // A plain trim writes the trimmed track next to its output.
        let cut = dir.path().join("cut.mp4");
        let req = EditRequest { source: path_str(&src), start: Some(0.5), output: Some(path_str(&cut)), ..Default::default() };
        let m = engine.edit(&req).unwrap();
        let side = EventTrack::sidecar_path(&cut);
        assert_eq!(m.events.as_deref(), Some(side.as_path()));
        let t = EventTrack::load(&side).unwrap();
        assert!((t.duration - 1.5).abs() < 1e-9);
        assert_eq!(t.clicks().map(|c| c.0).collect::<Vec<_>>(), vec![0.5]);
        // ... which the next edit picks up through history.
        let m2 = engine.edit(&EditRequest { source: m.id.clone(), zoom: Some(2.0), ..Default::default() }).unwrap();
        assert_eq!(m2.parent.as_deref(), Some(m.id.as_str()));
    }

    #[test]
    fn hidden_pointer_is_drawn_back_and_audio_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path());
        // 64x64 grey-ish video at 10 fps with one AAC track, 1 s.
        let src = dir.path().join("tone.mp4");
        bugle_platform::write_tone_fixture(&src, &bugle_platform::ToneFixture { tracks: 1, seconds: 1.0, video: true, ..Default::default() }).unwrap();
        let before = VideoFrameReader::open(&src, 0.0).unwrap().frame_at(0.5).unwrap();
        // 32x32 pt area (2 px/pt); the pointer rests at (4, 4) pt.
        let track = EventTrack::new(bugle_core::Rect::new(0.0, 0.0, 32.0, 32.0), false, 1.0, vec![CursorEvent::moved(0.0, 4.0, 4.0)]);
        track.save(&EventTrack::sidecar_path(&src)).unwrap();
        let out = dir.path().join("drawn.mp4");
        let req = EditRequest { source: path_str(&src), smooth_cursor: true, output: Some(path_str(&out)), ..Default::default() };
        engine.edit(&req).unwrap();
        assert_eq!(bugle_platform::audio_track_count(&out).unwrap(), 1, "audio copied back in");
        assert_eq!(bugle_platform::video_size(&out).unwrap(), (64, 64));
        let after = VideoFrameReader::open(&out, 0.0).unwrap().frame_at(0.5).unwrap();
        // Arrow body 3 pt right and 10 pt below the tip: dark now.
        let (x, y) = (8 + 6, 8 + 20);
        let lum = |p: &image::Rgba<u8>| p.0[..3].iter().map(|&v| v as u32).sum::<u32>();
        assert!(lum(after.get_pixel(x, y)) < 150, "{:?} (was {:?})", after.get_pixel(x, y), before.get_pixel(x, y));
        assert!(lum(after.get_pixel(50, 50)).abs_diff(lum(before.get_pixel(50, 50))) < 60, "far from the pointer: unchanged");
    }
}
