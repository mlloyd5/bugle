//! Screen recording.
//!
//! * GIF: an in-process frame-grab loop encoded with the `image` crate.
//! * Video: the system `screencapture -v` recorder (H.264 .mov, optional
//!   microphone), remuxed to MP4 on request. System-audio capture needs an
//!   SCStream audio tap and is not implemented yet.

use crate::Engine;
use anyhow::{anyhow, bail, Context, Result};
use bugle_core::{CaptureTarget, Media, MediaKind, RecordRequest, RecordingFormat, RecordingState, RecordingStatus};
use chrono::Utc;
use image::codecs::gif::{GifEncoder, Repeat};
use image::{Delay, Frame, RgbaImage};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Recorder {
    inner: Mutex<Option<Active>>,
}

struct Active {
    format: RecordingFormat,
    started: Instant,
    started_at: chrono::DateTime<chrono::Utc>,
    output: PathBuf,
    label: Option<String>,
    source: String,
    backend: Backend,
    frames: Arc<AtomicU64>,
    auto_stop: bool,
}

enum Backend {
    Gif { stop: Arc<AtomicBool>, handle: std::thread::JoinHandle<Result<u64>> },
    Video { child: std::process::Child, wants_mp4: bool },
}

impl Engine {
    pub fn record_status(&self) -> RecordingStatus {
        let g = self.recorder.inner.lock().unwrap();
        match g.as_ref() {
            None => RecordingStatus { state: RecordingState::Idle, format: None, started_at: None, elapsed_seconds: 0.0, frames: 0, output: None },
            Some(a) => RecordingStatus {
                state: RecordingState::Recording,
                format: Some(a.format),
                started_at: Some(a.started_at),
                elapsed_seconds: a.started.elapsed().as_secs_f64(),
                frames: a.frames.load(Ordering::Relaxed),
                output: Some(a.output.display().to_string()),
            },
        }
    }

    pub fn record_start(&self, req: &RecordRequest) -> Result<RecordingStatus> {
        if !bugle_platform::screen_access_preflight() {
            return Err(bugle_core::BugleError::PermissionDenied.into());
        }
        let mut g = self.recorder.inner.lock().unwrap();
        if g.is_some() {
            bail!("a recording is already in progress");
        }
        let (rect, window) = match &req.target {
            CaptureTarget::Screen { display } => {
                let d = match display {
                    Some(id) => bugle_platform::displays()?.into_iter().find(|d| d.id == *id).ok_or(bugle_core::BugleError::NoSuchDisplay(*id))?,
                    None => bugle_platform::main_display()?,
                };
                (d.frame, None)
            }
            CaptureTarget::Area { rect } => (*rect, None),
            CaptureTarget::Window { id, title, owner } => {
                let w = self.resolve_window(*id, title.as_deref(), owner.as_deref())?;
                (w.frame, Some(w))
            }
            CaptureTarget::Interactive => {
                drop(g);
                let r = self.select_area()?.ok_or_else(|| anyhow!("selection cancelled"))?;
                g = self.recorder.inner.lock().unwrap();
                (r, None)
            }
            CaptureTarget::AllScreens => (bugle_platform::main_display()?.frame, None),
        };
        let mut vars = HashMap::new();
        vars.insert("kind", "recording".to_string());
        if let Some(w) = &window {
            vars.insert("app", w.owner.clone());
            vars.insert("title", w.title.clone());
        }
        let frames = Arc::new(AtomicU64::new(0));
        let (output, backend) = match req.format {
            RecordingFormat::Gif => {
                let output = self.output_path(req.output.as_deref(), "gif", &vars);
                let stop = Arc::new(AtomicBool::new(false));
                let fps = req.fps.unwrap_or(self.config.recording.gif_fps).clamp(1, 30);
                let max_size = self.config.recording.gif_max_size;
                let max_secs = req.max_seconds;
                let show_cursor = true;
                let out = output.clone();
                let stop2 = stop.clone();
                let frames2 = frames.clone();
                let window_id = window.as_ref().map(|w| w.id);
                let handle = std::thread::Builder::new().name("bugle-gif".into()).spawn(move || {
                    gif_loop(rect, window_id, fps, max_size, max_secs, show_cursor, stop2, frames2, out)
                })?;
                (output, Backend::Gif { stop, handle })
            }
            RecordingFormat::Mov | RecordingFormat::Mp4 => {
                let wants_mp4 = req.format == RecordingFormat::Mp4;
                let output = self.output_path(req.output.as_deref(), if wants_mp4 { "mp4" } else { "mov" }, &vars);
                let tmp_mov = if wants_mp4 { output.with_extension("rec.mov") } else { output.clone() };
                let mut cmd = std::process::Command::new("/usr/sbin/screencapture");
                cmd.arg("-v").arg("-x");
                match &window {
                    Some(w) => {
                        cmd.arg("-l").arg(w.id.to_string());
                    }
                    None => {
                        cmd.arg("-R").arg(format!("{},{},{},{}", rect.x, rect.y, rect.w, rect.h));
                    }
                }
                if req.microphone || self.config.recording.microphone {
                    cmd.arg("-g");
                }
                if req.highlight_clicks || self.config.recording.highlight_clicks {
                    cmd.arg("-k");
                }
                if let Some(s) = req.max_seconds {
                    cmd.arg("-V").arg(format!("{}", s.max(1.0) as u64));
                }
                cmd.arg(&tmp_mov);
                cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
                let child = cmd.spawn().context("starting screencapture")?;
                if req.system_audio {
                    tracing::warn!("system audio capture is not supported by the screencapture backend; recording without it");
                }
                (output, Backend::Video { child, wants_mp4 })
            }
        };
        let source = match &window {
            Some(w) => format!("window {} — {} ({})", w.id, w.owner, w.title),
            None => format!("area {rect}"),
        };
        *g = Some(Active {
            format: req.format,
            started: Instant::now(),
            started_at: Utc::now(),
            output,
            label: req.label.clone(),
            source,
            backend,
            frames,
            auto_stop: req.max_seconds.is_some(),
        });
        drop(g);
        Ok(self.record_status())
    }

    /// Stop the active recording and finalise the file.
    pub fn record_stop(&self) -> Result<Media> {
        let active = self.recorder.inner.lock().unwrap().take().ok_or_else(|| anyhow!("no recording in progress"))?;
        let duration = active.started.elapsed().as_secs_f64();
        let kind = match active.backend {
            Backend::Gif { stop, handle } => {
                stop.store(true, Ordering::Relaxed);
                let n = handle.join().map_err(|_| anyhow!("gif thread panicked"))??;
                if n == 0 {
                    bail!("no frames were captured");
                }
                MediaKind::Gif
            }
            Backend::Video { mut child, wants_mp4 } => {
                // With a deadline, screencapture stops by itself; give it a
                // few seconds to finalise before interrupting it.
                let mut exited = child.try_wait()?.is_some();
                if !exited && active.auto_stop {
                    for _ in 0..40 {
                        std::thread::sleep(Duration::from_millis(100));
                        if child.try_wait()?.is_some() {
                            exited = true;
                            break;
                        }
                    }
                }
                if !exited {
                    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
                }
                let status = child.wait()?;
                // screencapture exits 0 on a clean stop; a non-zero status with
                // no file means it never started (permission etc.).
                let tmp_mov = if wants_mp4 { active.output.with_extension("rec.mov") } else { active.output.clone() };
                // Give the muxer a moment to flush the moov atom.
                for _ in 0..40 {
                    if tmp_mov.exists() && std::fs::metadata(&tmp_mov).map(|m| m.len() > 0).unwrap_or(false) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if !tmp_mov.exists() {
                    let mut err = String::new();
                    if let Some(mut e) = child.stderr.take() {
                        use std::io::Read;
                        let _ = e.read_to_string(&mut err);
                    }
                    bail!("screencapture produced no file (status {status}): {}", err.trim());
                }
                if wants_mp4 {
                    bugle_platform::export_mp4(&tmp_mov, &active.output)?;
                    let _ = std::fs::remove_file(&tmp_mov);
                }
                MediaKind::Video
            }
        };
        let mut m = Media::new(kind, &active.output);
        m.duration_seconds = Some(if kind == MediaKind::Gif {
            duration
        } else {
            bugle_platform::media_duration(&active.output).ok().filter(|d| d.is_finite() && *d > 0.0).unwrap_or(duration)
        });
        m.label = active.label;
        m.source = Some(active.source);
        if kind == MediaKind::Gif {
            if let Ok(d) = image::image_dimensions(&active.output) {
                m.width = Some(d.0);
                m.height = Some(d.1);
            }
        }
        self.history.push(&m)?;
        Ok(m)
    }
}

#[allow(clippy::too_many_arguments)]
fn gif_loop(
    rect: bugle_core::Rect,
    window_id: Option<u32>,
    fps: u32,
    max_size: u32,
    max_secs: Option<f64>,
    show_cursor: bool,
    stop: Arc<AtomicBool>,
    frames_counter: Arc<AtomicU64>,
    output: PathBuf,
) -> Result<u64> {
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let start = Instant::now();
    let mut frames: Vec<RgbaImage> = Vec::new();
    let mut next_tick = start;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some(m) = max_secs {
            if start.elapsed().as_secs_f64() >= m {
                break;
            }
        }
        let now = Instant::now();
        if now < next_tick {
            std::thread::sleep(next_tick - now);
        }
        next_tick += interval;
        let grab = match window_id {
            Some(id) => bugle_platform::capture_window(id, false, show_cursor),
            None => bugle_platform::capture_rect(rect, show_cursor),
        };
        match grab {
            Ok(img) => {
                let long = img.width().max(img.height());
                let img = if long > max_size {
                    let s = max_size as f64 / long as f64;
                    image::imageops::resize(
                        &img,
                        ((img.width() as f64) * s) as u32,
                        ((img.height() as f64) * s) as u32,
                        image::imageops::FilterType::Triangle,
                    )
                } else {
                    img
                };
                frames.push(img);
                frames_counter.store(frames.len() as u64, Ordering::Relaxed);
            }
            Err(e) => tracing::warn!("gif frame grab failed: {e}"),
        }
    }
    if frames.is_empty() {
        return Ok(0);
    }
    let file = std::fs::File::create(&output)?;
    let mut enc = GifEncoder::new_with_speed(std::io::BufWriter::new(file), 10);
    enc.set_repeat(Repeat::Infinite)?;
    let delay = Delay::from_numer_denom_ms(1000, fps);
    let n = frames.len() as u64;
    enc.encode_frames(frames.into_iter().map(move |f| Frame::from_parts(f, 0, 0, delay)))?;
    Ok(n)
}
