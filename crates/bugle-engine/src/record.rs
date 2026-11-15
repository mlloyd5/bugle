//! Screen recording.
//!
//! * GIF: an in-process frame-grab loop encoded with the `image` crate.
//! * Video: either the system `screencapture -v` recorder (H.264 .mov,
//!   optional microphone, click highlight), remuxed to MP4 on request, or the
//!   in-process ScreenCaptureKit recorder (`SCStream` + `AVAssetWriter`,
//!   H.264 + AAC straight to .mp4, click highlight on macOS 15+) which is the
//!   only one that can capture system audio. `recording.backend` picks between them; `auto` uses
//!   ScreenCaptureKit whenever system audio is requested.

use crate::Engine;
use anyhow::{anyhow, bail, Context, Result};
use bugle_core::config::RecordingBackend;
use bugle_platform::{SckRecordOptions, SckRecorder};
use bugle_core::{BugleError, CaptureTarget, MediaItem, MediaKind, Rect, RecordRequest, RecordingFormat, RecordingState, RecordingStatus};
use chrono::Utc;
use image::codecs::gif::{GifEncoder, Repeat};
use image::{Delay, Frame, RgbaImage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// GIF frames are kept decoded until the recording stops; stop capturing once
/// they take this much memory.
const GIF_MAX_BYTES: u64 = 1 << 30;
/// Longest GIF recording, whatever `max_seconds` says.
const GIF_MAX_SECONDS: f64 = 600.0;
/// How long `screencapture` must survive after spawning before the recording
/// is considered started.
const SCREENCAPTURE_STARTUP: Duration = Duration::from_millis(300);

#[derive(Default)]
pub struct RecorderState {
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
    max_seconds: Option<f64>,
    /// When the backend was first seen to have stopped by itself.
    finished_at: Option<Instant>,
}

/// A running recorder. Dropping it stops the recorder (the file may or may
/// not be finalised); `Engine::finalize` is the orderly path.
enum Backend {
    Gif {
        stop: Arc<AtomicBool>,
        /// Set by the frame loop once it stops capturing (before encoding).
        capture_done: Arc<AtomicBool>,
        /// `None` once joined.
        handle: Option<std::thread::JoinHandle<Result<u64>>>,
    },
    Video { child: std::process::Child, wants_mp4: bool },
    /// ScreenCaptureKit stream written by AVAssetWriter. Always produces an
    /// MP4; a `.mov` request is remuxed afterwards. `rec` is `None` once
    /// stopped.
    Sck { rec: Option<SckRecorder>, wants_mov: bool, mix_audio: bool },
}

impl Backend {
    /// Whether the recorder has stopped capturing without being asked to
    /// (deadline reached, process exited, frame cap hit).
    fn stopped_by_itself(&mut self, elapsed: f64, max_seconds: Option<f64>) -> bool {
        match self {
            Backend::Gif { capture_done, handle, .. } => {
                capture_done.load(Ordering::Relaxed) || handle.as_ref().is_none_or(|h| h.is_finished())
            }
            Backend::Video { child, .. } => !matches!(child.try_wait(), Ok(None)),
            // The stream stops itself at the deadline; the recorder exposes
            // no other "finished" signal.
            Backend::Sck { rec, .. } => rec.is_none() || deadline_passed(elapsed, max_seconds),
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        match self {
            Backend::Gif { stop, handle, .. } => {
                stop.store(true, Ordering::Relaxed);
                if let Some(h) = handle.take() {
                    let _ = h.join();
                }
            }
            Backend::Video { child, .. } => interrupt_child(child),
            // SckRecorder stops and finalises itself when dropped.
            Backend::Sck { rec, .. } => drop(rec.take()),
        }
    }
}

/// SIGINT a still-running child (screencapture finalises the file on
/// SIGINT), wait for it, and kill it if it does not exit within 5 s.
fn interrupt_child(child: &mut std::process::Child) {
    if !matches!(child.try_wait(), Ok(None)) {
        return;
    }
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    for _ in 0..50 {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    tracing::warn!("screencapture did not exit after SIGINT; killing it");
    let _ = child.kill();
    let _ = child.wait();
}

fn deadline_passed(elapsed: f64, max_seconds: Option<f64>) -> bool {
    max_seconds.is_some_and(|m| elapsed >= m)
}

impl Active {
    fn frames(&self) -> u64 {
        match &self.backend {
            Backend::Sck { rec: Some(rec), .. } => rec.frames(),
            _ => self.frames.load(Ordering::Relaxed),
        }
    }

    /// Check whether the backend stopped by itself and remember when.
    fn poll(&mut self) -> RecordingState {
        if self.finished_at.is_none() {
            let elapsed = self.started.elapsed().as_secs_f64();
            if self.backend.stopped_by_itself(elapsed, self.max_seconds) {
                self.finished_at = Some(Instant::now());
            }
        }
        if self.finished_at.is_some() {
            RecordingState::Finishing
        } else {
            RecordingState::Active
        }
    }

    /// Seconds recorded; frozen once the backend has stopped by itself and
    /// never more than the requested maximum.
    fn elapsed_seconds(&self) -> f64 {
        let end = self.finished_at.unwrap_or_else(Instant::now);
        let e = end.saturating_duration_since(self.started).as_secs_f64();
        match self.max_seconds {
            Some(m) if m > 0.0 => e.min(m),
            _ => e,
        }
    }

    fn status(&mut self) -> RecordingStatus {
        let state = self.poll();
        RecordingStatus {
            state,
            format: Some(self.format),
            started_at: Some(self.started_at),
            elapsed_seconds: self.elapsed_seconds(),
            frames: self.frames(),
            output: Some(self.output.display().to_string()),
        }
    }
}

/// Recording targets must be real, finite areas of at least 2x2 points.
fn validate_record_rect(r: &Rect) -> std::result::Result<(), BugleError> {
    if !(r.x.is_finite() && r.y.is_finite() && r.w.is_finite() && r.h.is_finite()) {
        return Err(BugleError::Invalid(format!("recording rect is not finite: {r}")));
    }
    if r.w < 2.0 || r.h < 2.0 {
        return Err(BugleError::Invalid(format!("recording rect must be at least 2x2 points: {r}")));
    }
    Ok(())
}

/// `screencapture -R` takes integer points. Rounds the edges so the result
/// covers the same pixels, never narrower than 1 point.
fn screencapture_rect_arg(r: &Rect) -> String {
    let x0 = r.x.round();
    let y0 = r.y.round();
    let w = ((r.x + r.w).round() - x0).max(1.0);
    let h = ((r.y + r.h).round() - y0).max(1.0);
    format!("{},{},{},{}", x0 as i64, y0 as i64, w as i64, h as i64)
}

/// Size of a GIF frame after fitting its long side into `max_size`; never 0.
fn gif_frame_dims(w: u32, h: u32, max_size: u32) -> (u32, u32) {
    let long = w.max(h);
    if long <= max_size {
        return (w.max(1), h.max(1));
    }
    let s = max_size as f64 / long as f64;
    ((((w as f64) * s) as u32).max(1), (((h as f64) * s) as u32).max(1))
}

/// Why the GIF loop must stop regardless of the caller's deadline.
fn gif_cap_reason(bytes: u64, elapsed: f64) -> Option<String> {
    if bytes >= GIF_MAX_BYTES {
        Some(format!("frames reached the {} MiB memory cap", GIF_MAX_BYTES >> 20))
    } else if elapsed >= GIF_MAX_SECONDS {
        Some(format!("reached the {} minute length cap", GIF_MAX_SECONDS / 60.0))
    } else {
        None
    }
}

/// The `foo.rec.mov` / `foo.rec.mp4` intermediate file next to `output`.
fn temp_path(output: &Path, ext: &str) -> PathBuf {
    output.with_extension(format!("rec.{ext}"))
}

fn file_nonempty(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
}

/// A conversion of `tmp` failed: drop any partial output and keep the raw
/// recording under its final name (`foo.rec.mov` -> `foo.mov`) so neither
/// the recording is lost nor a temp file left behind. Returns the error to
/// report.
fn keep_raw_after_failed_export(tmp: &Path, output: &Path, ext: &str, e: anyhow::Error) -> anyhow::Error {
    let _ = std::fs::remove_file(output);
    if !file_nonempty(tmp) {
        let _ = std::fs::remove_file(tmp);
        return e;
    }
    let keep = output.with_extension(ext);
    match std::fs::rename(tmp, &keep) {
        Ok(()) => e.context(format!("converting the recording failed; the original was kept at {}", keep.display())),
        Err(_) => {
            let _ = std::fs::remove_file(tmp);
            e.context("converting the recording failed")
        }
    }
}

/// The audio mixer: `bugle_platform::mix_audio_tracks` (a parameter so the
/// tests can make it fail).
type MixFn<'a> = &'a dyn Fn(&Path, &Path) -> Result<bool>;

/// Check that `mixed`, a mix of `src`, is complete before it replaces
/// anything: one audio track, as many video tracks as `src`, and the same
/// duration give or take half a second (or 2%).
fn check_mixed_copy(src: &Path, mixed: &Path) -> Result<()> {
    let audio = bugle_platform::audio_track_count(mixed)?;
    if audio != 1 {
        bail!("the mixed copy has {audio} audio tracks");
    }
    let (v0, v1) = (bugle_platform::video_track_count(src)?, bugle_platform::video_track_count(mixed)?);
    if v0 != v1 {
        bail!("the mixed copy has {v1} video tracks instead of {v0}");
    }
    let (d0, d1) = (bugle_platform::media_duration(src)?, bugle_platform::media_duration(mixed)?);
    if (d0 - d1).abs() > (d0 * 0.02).max(0.5) {
        bail!("the mixed copy lasts {d1:.2}s instead of {d0:.2}s");
    }
    Ok(())
}

/// Mix the audio tracks of `src` into `dst` and check the result. Returns
/// whether `dst` now holds a good mix; on failure `dst` is removed and the
/// reason logged (`src` is never touched). With fewer than two audio tracks
/// nothing is written.
fn mix_into(src: &Path, dst: &Path, mix: MixFn) -> bool {
    let result = mix(src, dst).and_then(|mixed| {
        if mixed {
            check_mixed_copy(src, dst)?;
        }
        Ok(mixed)
    });
    match result {
        Ok(mixed) => {
            if !mixed {
                tracing::info!("mix_audio: the recording has fewer than two audio tracks; nothing to mix");
            }
            mixed
        }
        Err(e) => {
            let _ = std::fs::remove_file(dst);
            tracing::warn!("mixing the audio tracks failed; keeping them separate: {e:#}");
            false
        }
    }
}

/// Create a new, uniquely named hidden file next to `path` for its mixed
/// copy. Creating it (rather than picking a name) guarantees no existing
/// file is ever overwritten or removed.
fn reserve_mix_temp(path: &Path) -> std::io::Result<PathBuf> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut last = None;
    for n in 0..100 {
        let candidate = dir.join(format!(".{stem}.{}-{n}.mix.mp4", std::process::id()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no free temp name")))
}

/// Fold the audio tracks of the finished recording at `path` into one,
/// replacing the file once the mixed copy has been checked. Needs free
/// space for a second copy while it runs. With fewer than two tracks there
/// is nothing to do; any failure is logged and leaves the recording as it
/// was. Returns whether the file was replaced.
fn mix_audio_in_place(path: &Path, mix: MixFn) -> bool {
    let tmp = match reserve_mix_temp(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("mixing the audio tracks failed (no temp file next to the recording: {e}); keeping them separate");
            return false;
        }
    };
    if mix_into(path, &tmp, mix) {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return true,
            Err(e) => tracing::warn!("replacing the recording with its mixed copy failed; keeping separate audio tracks: {e}"),
        }
    }
    let _ = std::fs::remove_file(&tmp);
    false
}

/// Turn the stream recorder's MP4 at `written` into the final `output`:
/// mix the audio tracks if asked, and for a `.mov` request write QuickTime
/// (straight from the mixer when it ran, otherwise by remuxing). `written`
/// is removed once it is no longer needed.
fn finish_stream_file(written: &Path, output: &Path, wants_mov: bool, mix_audio: bool, mix: MixFn) -> Result<()> {
    if !wants_mov {
        if mix_audio {
            mix_audio_in_place(written, mix);
        }
        return Ok(());
    }
    if !(mix_audio && mix_into(written, output, mix)) {
        if let Err(e) = bugle_platform::export_mov(written, output) {
            return Err(keep_raw_after_failed_export(written, output, "mp4", e));
        }
    }
    let _ = std::fs::remove_file(written);
    Ok(())
}

/// Read whatever the child wrote to its piped stderr.
fn child_stderr(child: &mut std::process::Child) -> String {
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read;
        let _ = e.read_to_string(&mut err);
    }
    err.trim().to_string()
}

impl Engine {
    pub fn record_status(&self) -> RecordingStatus {
        let mut g = self.recorder_state.inner.lock().unwrap();
        match g.as_mut() {
            None => RecordingStatus { state: RecordingState::Idle, format: None, started_at: None, elapsed_seconds: 0.0, frames: 0, output: None },
            Some(a) => a.status(),
        }
    }

    /// Lock the recorder state with no recording in it. A recording whose
    /// backend already stopped by itself is finalised first (as
    /// `record_stop` would); one that is still capturing is an error.
    fn idle_recorder(&self) -> Result<MutexGuard<'_, Option<Active>>> {
        loop {
            let mut g = self.recorder_state.inner.lock().unwrap();
            let state = match g.as_mut() {
                None => return Ok(g),
                Some(a) => a.poll(),
            };
            if state != RecordingState::Finishing {
                bail!("a recording is already in progress");
            }
            let finished = g.take().expect("checked above");
            drop(g);
            match self.finalize(finished) {
                Ok(m) => tracing::info!("finalised the previous recording: {}", m.path.display()),
                Err(e) => tracing::warn!("finalising the previous recording failed: {e:#}"),
            }
        }
    }

    pub fn record_start(&self, req: &RecordRequest) -> Result<RecordingStatus> {
        if !bugle_platform::screen_access_preflight() {
            return Err(bugle_core::BugleError::PermissionDenied.into());
        }
        // Fail before asking the user for a selection; checked again below
        // since the lock is not held while the target is resolved.
        drop(self.idle_recorder()?);
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
                let r = self.select_area()?.ok_or(BugleError::Cancelled)?;
                (r, None)
            }
            CaptureTarget::AllScreens => {
                return Err(BugleError::Invalid(
                    "recordings support screen (one display), area, window and interactive targets, not all screens".into(),
                )
                .into())
            }
        };
        validate_record_rect(&rect)?;
        let mut g = self.idle_recorder()?;
        let mut vars = HashMap::new();
        vars.insert("kind", "recording".to_string());
        if let Some(w) = &window {
            vars.insert("app", w.owner.clone());
            vars.insert("title", w.title.clone());
        }
        let frames = Arc::new(AtomicU64::new(0));
        let (output, backend) = match req.format {
            RecordingFormat::Gif => {
                if req.mix_audio {
                    tracing::warn!("mix_audio applies to video recordings; a GIF has no audio");
                }
                let output = self.output_path(req.output.as_deref(), "gif", &vars);
                let stop = Arc::new(AtomicBool::new(false));
                let capture_done = Arc::new(AtomicBool::new(false));
                let fps = req.fps.unwrap_or(self.config.recordings.gif_frame_rate).clamp(1, 30);
                let max_size = self.config.recordings.gif_max_size;
                let max_secs = req.max_seconds;
                let show_cursor = true;
                let out = output.clone();
                let stop2 = stop.clone();
                let done2 = capture_done.clone();
                let frames2 = frames.clone();
                let window_id = window.as_ref().map(|w| w.id);
                let handle = std::thread::Builder::new().name("bugle-gif".into()).spawn(move || {
                    gif_loop(rect, window_id, fps, max_size, max_secs, show_cursor, stop2, done2, frames2, out)
                })?;
                (output, Backend::Gif { stop, capture_done, handle: Some(handle) })
            }
            RecordingFormat::Mov | RecordingFormat::Mp4 => {
                let wants_mp4 = req.format == RecordingFormat::Mp4;
                let output = self.output_path(req.output.as_deref(), if wants_mp4 { "mp4" } else { "mov" }, &vars);
                let microphone = req.microphone || self.config.recordings.microphone;
                let system_audio = req.system_audio || self.config.recordings.system_audio;
                let mix_audio = req.mix_audio || self.config.recordings.mix_audio;
                let highlight_clicks = req.highlight_clicks || self.config.recordings.highlight_clicks;
                let sck_available = bugle_platform::sck_record_available();
                let use_sck = match self.config.recordings.backend {
                    RecordingBackend::StreamCapture => {
                        if !sck_available {
                            tracing::warn!("recordings.backend = \"stream\" but ScreenCaptureKit recording needs macOS 14; using the system recorder");
                        }
                        sck_available
                    }
                    RecordingBackend::SystemRecorder => false,
                    RecordingBackend::Auto => {
                        if system_audio && !sck_available {
                            tracing::warn!("system audio needs the ScreenCaptureKit recorder (macOS 14+); recording without it");
                        }
                        system_audio && sck_available
                    }
                };
                if mix_audio && !(use_sck && microphone && system_audio) {
                    tracing::warn!(
                        "mix_audio has nothing to mix: it needs both the microphone and system audio, recorded by the ScreenCaptureKit recorder"
                    );
                }
                if use_sck {
                    // AVAssetWriter writes the MP4 container directly; a .mov
                    // request is remuxed on stop.
                    let tmp = if wants_mp4 { output.clone() } else { temp_path(&output, "mp4") };
                    let opts = SckRecordOptions {
                        rect,
                        window_id: window.as_ref().map(|w| w.id),
                        fps: req.fps.unwrap_or(self.config.recordings.video_fps).clamp(1, 60),
                        retina: self.config.recordings.retina,
                        system_audio,
                        microphone,
                        show_cursor: true,
                        highlight_clicks,
                        max_seconds: req.max_seconds,
                    };
                    let rec = SckRecorder::start(&opts, &tmp).context("starting ScreenCaptureKit recording")?;
                    if let Some(w) = rec.warnings() {
                        tracing::warn!("{w}");
                    }
                    (output, Backend::Sck { rec: Some(rec), wants_mov: !wants_mp4, mix_audio })
                } else {
                    let tmp_mov = if wants_mp4 { temp_path(&output, "mov") } else { output.clone() };
                    let mut cmd = std::process::Command::new("/usr/sbin/screencapture");
                    cmd.arg("-v").arg("-x");
                    match &window {
                        Some(w) => {
                            cmd.arg("-l").arg(w.id.to_string());
                        }
                        None => {
                            cmd.arg("-R").arg(screencapture_rect_arg(&rect));
                        }
                    }
                    if microphone {
                        cmd.arg("-g");
                    }
                    if highlight_clicks {
                        cmd.arg("-k");
                    }
                    if let Some(s) = req.max_seconds {
                        cmd.arg("-V").arg(format!("{}", s.max(1.0) as u64));
                    }
                    cmd.arg(&tmp_mov);
                    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
                    let mut child = cmd.spawn().context("starting screencapture")?;
                    // A bad window id or missing permission makes it exit
                    // straight away; report that now rather than at stop.
                    let t0 = Instant::now();
                    while t0.elapsed() < SCREENCAPTURE_STARTUP {
                        std::thread::sleep(Duration::from_millis(50));
                        if let Some(status) = child.try_wait()? {
                            let err = child_stderr(&mut child);
                            if !file_nonempty(&tmp_mov) {
                                let _ = std::fs::remove_file(&tmp_mov);
                            }
                            bail!("screencapture exited immediately (status {status}): {err}");
                        }
                    }
                    if system_audio {
                        tracing::warn!("system audio capture is not supported by the screencapture backend; recording without it");
                    }
                    (output, Backend::Video { child, wants_mp4 })
                }
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
            max_seconds: req.max_seconds,
            finished_at: None,
        });
        drop(g);
        Ok(self.record_status())
    }

    /// Stop the active recording and finalise the file.
    pub fn record_stop(&self) -> Result<MediaItem> {
        let active = self.recorder_state.inner.lock().unwrap().take().ok_or_else(|| anyhow!("no recording in progress"))?;
        self.finalize(active)
    }

    /// Stop `active`'s backend (if still running), finish the file and add
    /// it to the history. Error paths remove the intermediate files.
    fn finalize(&self, mut active: Active) -> Result<MediaItem> {
        // Freezes the elapsed time if the backend already stopped by itself.
        active.poll();
        let duration = active.elapsed_seconds();
        let auto_stop = active.max_seconds.is_some();
        let kind = match &mut active.backend {
            Backend::Gif { stop, handle, .. } => {
                stop.store(true, Ordering::Relaxed);
                let h = handle.take().ok_or_else(|| anyhow!("gif recorder already stopped"))?;
                let n = match h.join().map_err(|_| anyhow!("gif thread panicked")).and_then(|r| r) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = std::fs::remove_file(&active.output);
                        return Err(e);
                    }
                };
                if n == 0 {
                    bail!("no frames were captured");
                }
                MediaKind::Gif
            }
            Backend::Video { child, wants_mp4 } => {
                let wants_mp4 = *wants_mp4;
                // With a deadline, screencapture stops by itself; give it a
                // few seconds to finalise before interrupting it.
                let mut exited = child.try_wait()?.is_some();
                if !exited && auto_stop {
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
                let tmp_mov = if wants_mp4 { temp_path(&active.output, "mov") } else { active.output.clone() };
                // Give the muxer a moment to flush the moov atom.
                for _ in 0..40 {
                    if file_nonempty(&tmp_mov) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if !file_nonempty(&tmp_mov) {
                    let _ = std::fs::remove_file(&tmp_mov);
                    let err = child_stderr(child);
                    bail!("screencapture produced no file (status {status}): {err}");
                }
                if wants_mp4 {
                    if let Err(e) = bugle_platform::export_mp4(&tmp_mov, &active.output) {
                        return Err(keep_raw_after_failed_export(&tmp_mov, &active.output, "mov", e));
                    }
                    let _ = std::fs::remove_file(&tmp_mov);
                }
                MediaKind::Video
            }
            Backend::Sck { rec, wants_mov, mix_audio } => {
                let (wants_mov, mix_audio) = (*wants_mov, *mix_audio);
                let rec = rec.take().ok_or_else(|| anyhow!("recorder already stopped"))?;
                // The file AVAssetWriter writes: the output itself, or a
                // temp MP4 when a .mov was asked for.
                let written = if wants_mov { temp_path(&active.output, "mp4") } else { active.output.clone() };
                let warnings = rec.warnings();
                let frames = rec.frames();
                if let Err(e) = rec.stop() {
                    let _ = std::fs::remove_file(&written);
                    return Err(e);
                }
                if let Some(w) = warnings {
                    tracing::warn!("{w}");
                }
                if frames == 0 {
                    let _ = std::fs::remove_file(&written);
                    bail!("no frames were captured");
                }
                finish_stream_file(&written, &active.output, wants_mov, mix_audio, &bugle_platform::mix_audio_tracks)?;
                MediaKind::Video
            }
        };
        let mut m = MediaItem::new(kind, &active.output);
        m.duration_seconds = Some(if kind == MediaKind::Gif {
            duration
        } else {
            bugle_platform::media_duration(&active.output).ok().filter(|d| d.is_finite() && *d > 0.0).unwrap_or(duration)
        });
        m.label = active.label.take();
        m.source = Some(std::mem::take(&mut active.source));
        if kind == MediaKind::Gif {
            if let Ok(d) = image::image_dimensions(&active.output) {
                m.width = Some(d.0);
                m.height = Some(d.1);
            }
        }
        self.history.push(&m)?;
        self.sync_after_save(&mut m);
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
    capture_done: Arc<AtomicBool>,
    frames_counter: Arc<AtomicU64>,
    output: PathBuf,
) -> Result<u64> {
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let start = Instant::now();
    let mut frames: Vec<RgbaImage> = Vec::new();
    let mut bytes: u64 = 0;
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
        if let Some(why) = gif_cap_reason(bytes, start.elapsed().as_secs_f64()) {
            tracing::warn!("GIF recording stopped: {why}");
            break;
        }
        let now = Instant::now();
        if now < next_tick {
            std::thread::sleep(next_tick - now);
        }
        next_tick += interval;
        let grab = match window_id {
            Some(id) => bugle_platform::capture_window(id, false, show_cursor),
            None => bugle_platform::capture_area(rect, show_cursor),
        };
        match grab {
            Ok(img) => {
                let (w, h) = gif_frame_dims(img.width(), img.height(), max_size);
                let img = if (w, h) != img.dimensions() {
                    image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle)
                } else {
                    img
                };
                bytes += img.as_raw().len() as u64;
                frames.push(img);
                frames_counter.store(frames.len() as u64, Ordering::Relaxed);
            }
            Err(e) => tracing::warn!("gif frame grab failed: {e}"),
        }
    }
    capture_done.store(true, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::Config;

    fn active(backend: Backend, output: PathBuf, max_seconds: Option<f64>) -> Active {
        Active {
            format: RecordingFormat::Gif,
            started: Instant::now(),
            started_at: Utc::now(),
            output,
            label: None,
            source: "test".into(),
            backend,
            frames: Arc::new(AtomicU64::new(0)),
            max_seconds,
            finished_at: None,
        }
    }

    fn video(program: &str, args: &[&str]) -> Backend {
        let child = std::process::Command::new(program).args(args).stderr(std::process::Stdio::piped()).spawn().unwrap();
        Backend::Video { child, wants_mp4: false }
    }

    /// A GIF backend whose "frame loop" waits for the stop flag (or for
    /// `release`) and then runs `finish`.
    fn gif<F>(release: Arc<AtomicBool>, finish: F) -> (Backend, Arc<AtomicBool>)
    where
        F: FnOnce() -> Result<u64> + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (stop2, done2) = (stop.clone(), done.clone());
        let handle = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) && !release.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            done2.store(true, Ordering::Relaxed);
            finish()
        });
        (Backend::Gif { stop: stop.clone(), capture_done: done, handle: Some(handle) }, stop)
    }

    fn wait_until(mut f: impl FnMut() -> bool) {
        let t0 = Instant::now();
        while !f() {
            assert!(t0.elapsed() < Duration::from_secs(5), "timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn engine_in(tmp: &Path) -> Engine {
        let mut cfg = Config::default();
        cfg.general.save_dir = tmp.join("save").display().to_string();
        cfg.general.history_path = Some(tmp.join("history.jsonl").display().to_string());
        Engine::new(cfg, false).unwrap()
    }

    #[test]
    fn rect_validation() {
        assert!(validate_record_rect(&Rect::new(-1440.0, 10.5, 2.0, 300.0)).is_ok());
        for r in [
            Rect::new(0.0, 0.0, 0.0, 100.0),
            Rect::new(0.0, 0.0, 100.0, -5.0),
            Rect::new(0.0, 0.0, 1.5, 100.0),
            Rect::new(f64::NAN, 0.0, 100.0, 100.0),
            Rect::new(0.0, 0.0, f64::INFINITY, 100.0),
            Rect::new(0.0, f64::NEG_INFINITY, 100.0, 100.0),
        ] {
            assert!(matches!(validate_record_rect(&r), Err(BugleError::Invalid(_))), "{r:?}");
        }
    }

    #[test]
    fn screencapture_rect_is_integral() {
        assert_eq!(screencapture_rect_arg(&Rect::new(10.5, 20.0, 300.25, 200.0)), "11,20,300,200");
        assert_eq!(screencapture_rect_arg(&Rect::new(0.0, 0.0, 1440.0, 900.0)), "0,0,1440,900");
        assert_eq!(screencapture_rect_arg(&Rect::new(-1440.4, -0.6, 0.3, 0.2)), "-1440,-1,1,1");
    }

    #[test]
    fn gif_dims_never_zero() {
        assert_eq!(gif_frame_dims(800, 600, 1200), (800, 600));
        assert_eq!(gif_frame_dims(2400, 1200, 1200), (1200, 600));
        assert_eq!(gif_frame_dims(5000, 2, 1200), (1200, 1));
        assert_eq!(gif_frame_dims(1, 100_000, 1200), (1, 1200));
        assert_eq!(gif_frame_dims(10, 10, 0), (1, 1));
    }

    #[test]
    fn gif_caps() {
        assert!(gif_cap_reason(0, 0.0).is_none());
        assert!(gif_cap_reason(GIF_MAX_BYTES - 1, GIF_MAX_SECONDS - 0.1).is_none());
        assert!(gif_cap_reason(GIF_MAX_BYTES, 1.0).unwrap().contains("memory"));
        assert!(gif_cap_reason(0, GIF_MAX_SECONDS).unwrap().contains("length"));
    }

    #[test]
    fn deadline() {
        assert!(!deadline_passed(100.0, None));
        assert!(!deadline_passed(1.9, Some(2.0)));
        assert!(deadline_passed(2.0, Some(2.0)));
    }

    #[test]
    fn exited_child_reports_finishing_with_frozen_elapsed() {
        let mut a = active(video("/usr/bin/true", &[]), PathBuf::from("/nonexistent/x.mov"), None);
        wait_until(|| a.poll() == RecordingState::Finishing);
        let e1 = a.status().elapsed_seconds;
        std::thread::sleep(Duration::from_millis(50));
        let s = a.status();
        assert_eq!(s.state, RecordingState::Finishing);
        assert_eq!(s.elapsed_seconds, e1);
    }

    #[test]
    fn running_child_is_active_and_interrupted_on_drop() {
        let mut a = active(video("/bin/sleep", &["30"]), PathBuf::from("/nonexistent/x.mov"), None);
        let pid = match &a.backend {
            Backend::Video { child, .. } => child.id() as i32,
            _ => unreachable!(),
        };
        assert_eq!(a.poll(), RecordingState::Active);
        let t0 = Instant::now();
        drop(a);
        assert!(t0.elapsed() < Duration::from_secs(5), "child should exit on SIGINT");
        // Reaped: the pid no longer exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }

    #[test]
    fn gif_backend_finishing_and_drop_joins() {
        let release = Arc::new(AtomicBool::new(false));
        let ran = Arc::new(AtomicBool::new(false));
        let ran2 = ran.clone();
        let (backend, stop) = gif(release.clone(), move || {
            ran2.store(true, Ordering::Relaxed);
            Ok(0)
        });
        let mut a = active(backend, PathBuf::from("/nonexistent/x.gif"), Some(5.0));
        assert_eq!(a.poll(), RecordingState::Active);
        drop(a);
        assert!(stop.load(Ordering::Relaxed));
        assert!(ran.load(Ordering::Relaxed), "drop must join the thread");

        // Stopping by itself (deadline / cap) shows as Finishing.
        let (backend, _) = gif(release.clone(), || Ok(0));
        let mut a = active(backend, PathBuf::from("/nonexistent/x.gif"), None);
        release.store(true, Ordering::Relaxed);
        wait_until(|| a.poll() == RecordingState::Finishing);
    }

    #[test]
    fn sck_deadline_counts_as_finished_and_elapsed_is_capped() {
        let mut a = active(Backend::Sck { rec: None, wants_mov: false, mix_audio: false }, PathBuf::from("/nonexistent/x.mp4"), Some(0.01));
        a.started -= Duration::from_secs(3);
        assert_eq!(a.poll(), RecordingState::Finishing);
        assert_eq!(a.elapsed_seconds(), 0.01);
    }

    #[test]
    fn start_refuses_running_but_reaps_finished() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_in(tmp.path());
        assert!(engine.idle_recorder().is_ok());

        // Still capturing: refused, and left alone.
        let release = Arc::new(AtomicBool::new(false));
        let (backend, stop) = gif(release.clone(), || Ok(0));
        *engine.recorder_state.inner.lock().unwrap() = Some(active(backend, tmp.path().join("a.gif"), None));
        let err = engine.idle_recorder().err().expect("should refuse");
        assert!(err.to_string().contains("already in progress"));
        assert_eq!(engine.record_status().state, RecordingState::Active);
        assert!(!stop.load(Ordering::Relaxed));

        // Stopped by itself: finalised (into the history) and the slot freed.
        let out = tmp.path().join("b.gif");
        let out2 = out.clone();
        let (backend, _) = gif(release.clone(), move || {
            let img = RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]));
            let file = std::fs::File::create(&out2)?;
            let mut enc = GifEncoder::new(file);
            enc.encode_frames(std::iter::once(Frame::new(img)))?;
            Ok(1)
        });
        // Replacing the slot drops (stops and joins) the first recording.
        *engine.recorder_state.inner.lock().unwrap() = Some(active(backend, out.clone(), None));
        assert!(stop.load(Ordering::Relaxed));
        release.store(true, Ordering::Relaxed);
        wait_until(|| engine.record_status().state == RecordingState::Finishing);
        let g = engine.idle_recorder().unwrap();
        assert!(g.is_none());
        drop(g);
        let items = engine.history.all().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, out);
        assert_eq!(items[0].width, Some(4));
        assert_eq!(engine.record_status().state, RecordingState::Idle);
    }

    #[test]
    fn failed_gif_finalise_removes_partial_file() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_in(tmp.path());
        let out = tmp.path().join("partial.gif");
        std::fs::write(&out, b"GIF89a partial").unwrap();
        let (backend, _) = gif(Arc::new(AtomicBool::new(false)), || bail!("encoder failed"));
        *engine.recorder_state.inner.lock().unwrap() = Some(active(backend, out.clone(), None));
        assert!(engine.record_stop().is_err());
        assert!(!out.exists());
        assert!(engine.record_stop().is_err(), "slot is free after a failed stop");
    }

    use bugle_platform::{audio_track_count, mix_audio_tracks, video_track_count, write_tone_fixture, ToneFixture};

    fn two_tracks_with_video(path: &Path) {
        let f = ToneFixture { tracks: 2, seconds: 1.0, video: true, ..Default::default() };
        write_tone_fixture(path, &f).unwrap();
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn mixing_in_place_replaces_the_file_and_spares_neighbours() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = tmp.path().join("clip.mp4");
        two_tracks_with_video(&rec);
        // Files that a predictable temp name could have clobbered.
        std::fs::write(tmp.path().join("clip.mix.mp4"), b"keep me").unwrap();
        std::fs::write(tmp.path().join("clip.rec.mix.mp4"), b"keep me too").unwrap();
        assert!(mix_audio_in_place(&rec, &mix_audio_tracks));
        assert_eq!(audio_track_count(&rec).unwrap(), 1);
        assert_eq!(video_track_count(&rec).unwrap(), 1);
        assert_eq!(std::fs::read(tmp.path().join("clip.mix.mp4")).unwrap(), b"keep me");
        assert_eq!(std::fs::read(tmp.path().join("clip.rec.mix.mp4")).unwrap(), b"keep me too");
        assert_eq!(dir_names(tmp.path()), ["clip.mix.mp4", "clip.mp4", "clip.rec.mix.mp4"]);

        // One track now: nothing to do, file and folder untouched.
        let before = std::fs::read(&rec).unwrap();
        assert!(!mix_audio_in_place(&rec, &mix_audio_tracks));
        assert_eq!(std::fs::read(&rec).unwrap(), before);
        assert_eq!(dir_names(tmp.path()).len(), 3);
    }

    #[test]
    fn failed_mix_keeps_the_recording() {
        let tmp = tempfile::tempdir().unwrap();
        // A real AVFoundation failure: the track headers are intact but the
        // sample data is cut off.
        let rec = tmp.path().join("cut.mp4");
        two_tracks_with_video(&rec);
        let full = std::fs::read(&rec).unwrap();
        std::fs::write(&rec, &full[..full.len() / 2]).unwrap();
        assert_eq!(audio_track_count(&rec).unwrap(), 2, "fixture must keep its track headers");
        let before = std::fs::read(&rec).unwrap();
        assert!(!mix_audio_in_place(&rec, &mix_audio_tracks));
        assert_eq!(std::fs::read(&rec).unwrap(), before);
        assert_eq!(dir_names(tmp.path()), ["cut.mp4"]);

        // The mixer reports an error after writing part of its output.
        let rec = tmp.path().join("clip.mp4");
        two_tracks_with_video(&rec);
        let before = std::fs::read(&rec).unwrap();
        let failing = |_: &Path, dst: &Path| -> Result<bool> {
            std::fs::write(dst, b"partial")?;
            bail!("encoder exploded")
        };
        assert!(!mix_audio_in_place(&rec, &failing));
        assert_eq!(std::fs::read(&rec).unwrap(), before);
        assert_eq!(dir_names(tmp.path()), ["clip.mp4", "cut.mp4"]);

        // The mixer claims success but its output is not a full copy (the
        // video track is missing): rejected before it replaces anything.
        let no_video = |_: &Path, dst: &Path| -> Result<bool> {
            write_tone_fixture(dst, &ToneFixture { tracks: 1, seconds: 1.0, ..Default::default() })?;
            Ok(true)
        };
        assert!(!mix_audio_in_place(&rec, &no_video));
        assert_eq!(std::fs::read(&rec).unwrap(), before);
        assert_eq!(dir_names(tmp.path()), ["clip.mp4", "cut.mp4"]);
    }

    #[test]
    fn failed_rename_keeps_the_recording() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("rec");
        std::fs::create_dir(&dir).unwrap();
        let rec = dir.join("clip.mp4");
        two_tracks_with_video(&rec);
        let before = std::fs::read(&rec).unwrap();
        // Mix for real, then make the folder read-only so the rename fails.
        let lock = dir.clone();
        let mix_then_lock = move |src: &Path, dst: &Path| -> Result<bool> {
            let r = mix_audio_tracks(src, dst)?;
            std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o555))?;
            Ok(r)
        };
        let replaced = mix_audio_in_place(&rec, &mix_then_lock);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!replaced);
        assert_eq!(std::fs::read(&rec).unwrap(), before);
        assert_eq!(audio_track_count(&rec).unwrap(), 2);
    }

    #[test]
    fn mov_requests_are_mixed_straight_to_quicktime() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("clip.mov");
        let written = temp_path(&out, "mp4");
        two_tracks_with_video(&written);
        finish_stream_file(&written, &out, true, true, &mix_audio_tracks).unwrap();
        assert!(!written.exists());
        assert_eq!(&std::fs::read(&out).unwrap()[4..12], b"ftypqt  ");
        assert_eq!(audio_track_count(&out).unwrap(), 1);
        assert_eq!(video_track_count(&out).unwrap(), 1);

        // A failed mix falls back to the plain remux with both tracks.
        two_tracks_with_video(&written);
        let failing = |_: &Path, _: &Path| -> Result<bool> { bail!("no") };
        finish_stream_file(&written, &out, true, true, &failing).unwrap();
        assert!(!written.exists());
        assert_eq!(&std::fs::read(&out).unwrap()[4..12], b"ftypqt  ");
        assert_eq!(audio_track_count(&out).unwrap(), 2);

        // MP4 without mixing: left exactly as written.
        let mp4 = tmp.path().join("plain.mp4");
        two_tracks_with_video(&mp4);
        let before = std::fs::read(&mp4).unwrap();
        finish_stream_file(&mp4, &mp4, false, false, &mix_audio_tracks).unwrap();
        assert_eq!(std::fs::read(&mp4).unwrap(), before);
    }

    #[test]
    fn failed_export_keeps_raw_recording_without_temp_name() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("clip.mp4");
        let raw = temp_path(&output, "mov");
        assert_eq!(raw, tmp.path().join("clip.rec.mov"));
        std::fs::write(&raw, b"movie").unwrap();
        std::fs::write(&output, b"partial").unwrap();
        let e = keep_raw_after_failed_export(&raw, &output, "mov", anyhow!("boom"));
        assert!(format!("{e:#}").contains("clip.mov"));
        assert!(!raw.exists());
        assert!(!output.exists());
        assert_eq!(std::fs::read(tmp.path().join("clip.mov")).unwrap(), b"movie");

        // An empty temp file is just removed.
        std::fs::remove_file(tmp.path().join("clip.mov")).unwrap();
        std::fs::write(&raw, b"").unwrap();
        keep_raw_after_failed_export(&raw, &output, "mov", anyhow!("boom"));
        assert!(!raw.exists());
        assert!(!tmp.path().join("clip.mov").exists());
    }
}
