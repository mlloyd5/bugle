//! Windowing glue for the video editor: opens the AppKit window for a
//! recording, feeds its events to [`VideoEditor`], decodes preview frames
//! (with the zoom and pointer effects the export would apply) on a preview
//! thread, reads the audio waveform for the timeline on a thread of its own,
//! and runs the export (`Engine::edit`) off the main thread. The decisions
//! live in `video_editor.rs`.

use crate::editor_window::guarded;
use crate::video_editor::{
    preset_label, waveform_levels, Caps, Effects, FrameRequest, VideoEditor, CLICK_ZOOMS, PRESETS, WAVEFORM_BUCKETS, ZOOMS,
};
use bugle_core::MediaKind;
use crate::{file_name, saved_subtitle, App, AppEvent, Notice, Proxy};
use bugle_engine::{ClickZoom, EditRequest, EditSource, Engine};
use bugle_platform::{VideoEditorCommand, VideoEditorEvent};
use image::RgbaImage;
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Largest preview frame decoded, in pixels (the window shows it scaled to
/// fit; 2x a large window on Retina).
const PREVIEW_MAX: (u32, u32) = (1920, 1200);

/// Video editor traffic on the main loop.
pub(crate) enum VideoEditorMsg {
    /// Open the video editor for a media id, `latest` or a file path.
    Open(String),
    /// A preview frame finished decoding on the preview thread.
    Preview { generation: u64, seq: u64, t: f64, result: Result<RgbaImage, String> },
    /// The recording's audio peaks for the timeline (none without audio).
    Waveform { generation: u64, peaks: Vec<f32> },
    /// An export finished. On success the window `generation` closes.
    Exported { generation: u64, result: Result<Notice, String> },
    /// Smoke test: deliver an event as if the window had reported it.
    Inject(VideoEditorEvent),
    /// Smoke test: close without exporting.
    Close,
}

thread_local! {
    /// The open window's session, for results arriving on the loop. The
    /// window's handler owns the session; this never keeps it alive.
    static CURRENT: RefCell<Weak<RefCell<Session>>> = const { RefCell::new(Weak::new()) };
}

/// One frame to decode: its sequence number, time, when it may be shown
/// (playback) and the effect options to render it with.
struct PreviewJob {
    seq: u64,
    frame: FrameRequest,
    zoom: Option<f64>,
    click_zooms: Vec<ClickZoom>,
    smooth: bool,
}

/// The preview options effects were built for: zoom factor, click zooms,
/// smoothing.
type EffectsKey = (Option<f64>, Vec<ClickZoom>, bool);

fn toast_problem(msg: &str) {
    tracing::warn!("video editor: {msg}");
    bugle_platform::toast_show(None, "Edit Video", msg, None, 4.0);
}

/// Decode preview frames on a thread of their own. Only the newest waiting
/// job is decoded; a job due later (playback) waits for its time unless a
/// newer one replaces it. The effects are built once per option change,
/// with the same code the export uses. The thread ends when the session
/// (holding the sender) is dropped.
fn spawn_preview(src: Arc<EditSource>, proxy: Proxy, generation: u64) -> Option<mpsc::Sender<PreviewJob>> {
    let (tx, rx) = mpsc::channel::<PreviewJob>();
    let spawned = std::thread::Builder::new().name("bugle-video-preview".into()).spawn(move || {
        let mut frames = src.preview_frames(PREVIEW_MAX).map_err(|e| format!("{e:#}"));
        // The effects for the options they were built for.
        let mut effects: Option<(EffectsKey, Option<bugle_engine::cursor_fx::Effects>)> = None;
        'jobs: while let Ok(mut job) = rx.recv() {
            loop {
                while let Ok(newer) = rx.try_recv() {
                    job = newer;
                }
                let wait = job.frame.at.saturating_duration_since(Instant::now());
                if wait.is_zero() {
                    break;
                }
                match rx.recv_timeout(wait) {
                    Ok(newer) => job = newer,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break 'jobs,
                }
            }
            let key = (job.zoom, job.click_zooms, job.smooth);
            if effects.as_ref().map(|e| &e.0) != Some(&key) {
                let req = EditRequest { zoom: key.0, click_zooms: key.1.clone(), smooth_cursor: key.2, ..Default::default() };
                effects = Some((key, src.effects(&req)));
            }
            let fx = effects.as_ref().and_then(|e| e.1.as_ref());
            // Every job answers, error or not: playback waits for it.
            let result = match frames.as_mut() {
                Ok(f) => guarded("preview", || f.frame(job.frame.t, fx)),
                Err(e) => Err(e.clone()),
            };
            proxy.send(AppEvent::VideoEditor(VideoEditorMsg::Preview { generation, seq: job.seq, t: job.frame.t, result }));
        }
    });
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            tracing::warn!("video editor: no preview thread ({e})");
            None
        }
    }
}

/// Read the recording's audio peaks on a thread and send them to the loop
/// (nothing for a recording without audio; a failure is only logged: the
/// timeline is usable without a waveform). Setting `cancel` (the window
/// closed) stops the decode early and sends nothing.
fn spawn_waveform(src: Arc<EditSource>, proxy: Proxy, generation: u64, cancel: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new().name("bugle-video-waveform".into()).spawn(move || {
        match guarded("waveform", || src.audio_peaks(WAVEFORM_BUCKETS, Some(&cancel))) {
            Ok(_) if cancel.load(Ordering::Relaxed) => {}
            Ok(Some(peaks)) => proxy.send(AppEvent::VideoEditor(VideoEditorMsg::Waveform { generation, peaks })),
            Ok(None) => {}
            Err(e) => tracing::warn!("video editor: no waveform ({e})"),
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("video editor: no waveform thread ({e})");
    }
}

/// One open video editor. Lives in the window's event handler and is
/// dropped when the window closes.
struct Session {
    editor: VideoEditor,
    src: Arc<EditSource>,
    engine: Arc<Engine>,
    proxy: Proxy,
    generation: u64,
    preview: Option<mpsc::Sender<PreviewJob>>,
    /// The newest frame asked for; results for older ones are dropped.
    seq: u64,
    /// The last preview error toasted, so a failing decode is reported once.
    last_error: Option<String>,
    /// Set when the window closes: stops the waveform thread's decode.
    waveform_cancel: Arc<AtomicBool>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.waveform_cancel.store(true, Ordering::Relaxed);
    }
}

impl Session {
    fn on_event(&mut self, event: VideoEditorEvent) {
        let fx = self.editor.handle(event, Instant::now());
        self.apply(fx);
    }

    fn apply(&mut self, fx: Effects) {
        if let Some(f) = fx.frame {
            self.request_frame(f);
        }
        if let Some(req) = fx.export {
            self.export(req);
        }
        if fx.state {
            bugle_platform::video_editor_set_state(&self.editor.state());
        }
    }

    fn request_frame(&mut self, frame: FrameRequest) {
        self.seq += 1;
        let (zoom, smooth) = self.editor.options();
        let job = PreviewJob { seq: self.seq, frame, zoom, click_zooms: self.editor.click_zooms(), smooth };
        if let Some(tx) = &self.preview {
            if tx.send(job).is_ok() {
                return;
            }
            self.preview = None;
        }
        toast_problem("the preview is not available");
    }

    fn on_preview(&mut self, seq: u64, t: f64, result: Result<RgbaImage, String>) {
        if seq != self.seq {
            // A newer frame is on its way.
            return;
        }
        match result {
            Ok(img) => {
                self.last_error = None;
                bugle_platform::video_editor_set_frame(&img);
            }
            Err(e) => {
                if self.last_error.as_ref() != Some(&e) {
                    toast_problem(&format!("the frame at {} could not be shown: {e}", crate::video_editor::clock(t)));
                }
                self.last_error = Some(e);
            }
        }
        // During playback: the next frame, and the playhead moves on.
        if let Some(f) = self.editor.shown(t, Instant::now()) {
            self.request_frame(f);
            bugle_platform::video_editor_set_state(&self.editor.state());
        }
    }

    /// Export the clip as a new history item. The window stays up (its
    /// controls disabled) until the export succeeded, so a failure loses
    /// nothing.
    fn export(&mut self, req: EditRequest) {
        if let Err(e) = self.src.check(&req) {
            return toast_problem(&e.to_string());
        }
        self.editor.set_busy(true);
        let (engine, proxy, generation) = (self.engine.clone(), self.proxy.clone(), self.generation);
        tracing::info!("video editor: exporting {req:?}");
        let spawned = std::thread::Builder::new().name("bugle-video-export".into()).spawn(move || {
            // Always answers, so the window never stays busy.
            let result = guarded("export", || engine.edit(&req)).map(|item| Notice {
                title: format!("Edited recording saved ({:.1}s)", item.duration_seconds.unwrap_or(0.0)),
                subtitle: saved_subtitle(&engine, &item),
                thumb: crate::notice_thumb(&item),
                open: Some(item.path.clone()),
            });
            proxy.send(AppEvent::VideoEditor(VideoEditorMsg::Exported { generation, result }));
        });
        if let Err(e) = spawned {
            self.editor.set_busy(false);
            toast_problem(&format!("could not start the export: {e}"));
        }
    }

    fn export_failed(&mut self) {
        self.editor.set_busy(false);
        bugle_platform::video_editor_set_state(&self.editor.state());
    }
}

/// Smoke test: the open video editor's state as its window was last given
/// it. Main thread only.
pub(crate) fn smoke_state() -> Option<bugle_platform::VideoEditorState> {
    CURRENT.with(|c| c.borrow().upgrade()).and_then(|s| s.try_borrow().ok().map(|s| s.editor.state()))
}

/// Smoke test: write the synthetic recording and its event track (see
/// `write_smoke_recording`) for the input steps in `smoke_input.rs`.
pub(crate) fn write_input_smoke_recording(path: &std::path::Path, side: &std::path::Path) -> anyhow::Result<()> {
    write_smoke_recording(path, side)
}

/// Whether the open video editor (if any) is exporting.
pub(crate) fn export_running() -> bool {
    CURRENT.with(|c| c.borrow().upgrade()).is_some_and(|s| s.try_borrow().map_or(true, |s| s.editor.busy()))
}

impl App {
    /// The most recent recording (video or GIF) in the history, for *Edit
    /// Latest Recording*.
    pub(crate) fn edit_latest_recording(&mut self) {
        let all = self.engine.history.all().unwrap_or_default();
        match all.iter().rev().find(|m| matches!(m.kind, MediaKind::Video | MediaKind::Gif) && m.path.exists()) {
            Some(m) => self.open_video_editor(&m.id.clone()),
            None => self.toast_error("Edit Video: no recording in the history yet"),
        }
    }

    /// Open the video editor for a media id, `latest` or a video or GIF
    /// path. One at a time: with one already open, that one comes forward.
    pub(crate) fn open_video_editor(&mut self, source: &str) {
        if bugle_platform::video_editor_active() {
            bugle_platform::video_editor_focus();
            return self.toast_error("Edit Video: finish or close the open video editor first");
        }
        let src = match self.engine.edit_source(source) {
            Ok(s) => Arc::new(s),
            Err(e) => return self.toast_error(&format!("Edit Video: {e:#}")),
        };
        if let Some(e) = &src.track_error {
            tracing::warn!("video editor: ignoring the cursor event track: {e}");
        }
        self.video_editor_generation += 1;
        let generation = self.video_editor_generation;
        let caps = Caps { zoom: src.can_zoom(), smooth: src.can_smooth(), gif: src.gif };
        let editor = VideoEditor::new(src.source.clone(), src.duration(), src.info.nominal_fps, caps, src.zoom_clicks());
        let waveform_cancel = Arc::new(AtomicBool::new(false));
        let session = Rc::new(RefCell::new(Session {
            editor,
            preview: spawn_preview(src.clone(), self.proxy.clone(), generation),
            src: src.clone(),
            engine: self.engine.clone(),
            proxy: self.proxy.clone(),
            generation,
            seq: 0,
            last_error: None,
            waveform_cancel: waveform_cancel.clone(),
        }));
        let zoom_labels: Vec<&str> = ZOOMS.iter().map(|z| z.0).collect();
        let click_labels: Vec<&str> = CLICK_ZOOMS.iter().map(|z| z.0).collect();
        let preset_labels: Vec<&str> = session.borrow().editor.presets().iter().map(|p| preset_label(*p)).collect();
        let handler = session.clone();
        let opened = bugle_platform::video_editor_open(
            &format!("Edit Video — {}", file_name(&src.item.path)),
            src.duration(),
            (src.info.width, src.info.height),
            &zoom_labels,
            &click_labels,
            &preset_labels,
            move |event| match handler.try_borrow_mut() {
                Ok(mut s) => s.on_event(event),
                Err(_) => tracing::warn!("video editor: dropped re-entrant event {event:?}"),
            },
        );
        if let Err(e) = opened {
            return self.toast_error(&format!("Edit Video: {e:#}"));
        }
        CURRENT.with(|c| *c.borrow_mut() = Rc::downgrade(&session));
        // A GIF has no audio track to draw.
        if !src.gif {
            spawn_waveform(src.clone(), self.proxy.clone(), generation, waveform_cancel);
        }
        tracing::info!(
            "video editor: opened {} ({:.2}s, {}×{}, zoom {}, smoothing {})",
            file_name(&src.item.path),
            src.duration(),
            src.info.width,
            src.info.height,
            caps.zoom,
            caps.smooth
        );
        let fx = session.borrow().editor.start(Instant::now());
        session.borrow_mut().apply(fx);
    }

    pub(crate) fn on_video_editor(&mut self, msg: VideoEditorMsg) {
        let current = || CURRENT.with(|c| c.borrow().upgrade());
        match msg {
            VideoEditorMsg::Open(source) => self.open_video_editor(&source),
            VideoEditorMsg::Preview { generation, seq, t, result } => {
                if let Some(session) = current() {
                    match session.try_borrow_mut() {
                        Ok(mut s) if s.generation == generation => s.on_preview(seq, t, result),
                        // A window that has closed since.
                        Ok(_) => {}
                        // Never dropped (playback waits for it): try again
                        // on the next turn.
                        Err(_) => self.proxy.send(AppEvent::VideoEditor(VideoEditorMsg::Preview { generation, seq, t, result })),
                    }
                }
            }
            VideoEditorMsg::Waveform { generation, peaks } => {
                // Only for the window it was read for.
                if current().is_some_and(|s| s.try_borrow().is_ok_and(|s| s.generation == generation)) {
                    bugle_platform::video_editor_set_waveform(&waveform_levels(&peaks));
                }
            }
            VideoEditorMsg::Exported { generation, result: Ok(n) } => {
                if generation == self.video_editor_generation {
                    bugle_platform::video_editor_close();
                }
                tracing::info!("video editor: exported {}", n.subtitle);
                if !self.quick_show_notice(&n) {
                    self.toast(&n);
                }
                self.refresh_recent();
            }
            VideoEditorMsg::Exported { generation, result: Err(e) } => {
                // The window is still up with the work in it; let it go on.
                if let Some(session) = current() {
                    if let Ok(mut s) = session.try_borrow_mut() {
                        if s.generation == generation {
                            s.export_failed();
                        }
                    }
                }
                self.toast_error(&format!("Edit Video: could not export: {e}"));
            }
            VideoEditorMsg::Inject(event) => bugle_platform::video_editor_inject(event),
            VideoEditorMsg::Close => bugle_platform::video_editor_close(),
        }
    }
}

/// `--smoke-test` step: write a synthetic 3-second recording (a moving
/// gradient, 640x400 at 30 fps, two tone bursts as its audio) with an event
/// track (pointer hidden, two clicks) to the temp directory — not added to
/// history — open it in the video editor, check its waveform reached the
/// timeline, and drive it with injected events (nothing can click):
/// seek, set the in and out points, zoom, smoothing, a preset, playback.
/// The window is captured along the way (kept only with
/// `BUGLE_SMOKE_KEEP`) and closed without exporting; with
/// `BUGLE_SMOKE_SAVE` it exports instead, which writes to the configured
/// `save_dir` and history: use a throwaway `--config`.
pub(crate) fn smoke(proxy: &Proxy) {
    let path = std::env::temp_dir().join("bugle-smoke-video-editor.mp4");
    let side = bugle_core::EventTrack::sidecar_path(&path);
    if let Err(e) = write_smoke_recording(&path, &side) {
        eprintln!("smoke test: video editor skipped ({e:#})");
        return;
    }
    let send = |e: VideoEditorEvent| proxy.send(AppEvent::VideoEditor(VideoEditorMsg::Inject(e)));
    let command = |c: VideoEditorCommand| send(VideoEditorEvent::Command(c));
    let snap = |name: &str| {
        std::thread::sleep(Duration::from_millis(900));
        let shot = bugle_platform::app_windows()
            .ok()
            .and_then(|ws| ws.into_iter().find(|w| w.title.starts_with("Edit Video")))
            .and_then(|w| bugle_platform::capture_window(w.id, false, false).ok());
        match shot {
            Some(shot) if std::env::var_os("BUGLE_SMOKE_KEEP").is_some() => {
                let out = std::env::temp_dir().join(format!("bugle-smoke-{name}.png"));
                match shot.save(&out) {
                    Ok(()) => eprintln!("smoke test: {name} → {}", out.display()),
                    Err(e) => eprintln!("smoke test: {name} not kept ({e})"),
                }
            }
            Some(shot) => eprintln!("smoke test: {name} {}×{}", shot.width(), shot.height()),
            None => eprintln!("smoke test: {name} not found"),
        }
    };
    proxy.send(AppEvent::VideoEditor(VideoEditorMsg::Open(path.display().to_string())));
    std::thread::sleep(Duration::from_millis(700));
    if !smoke_waveform(&path) {
        crate::smoke_fail("video editor waveform", "a check in the line above did not hold");
    }
    snap("video-editor-waveform");
    send(VideoEditorEvent::Seek(0.5));
    command(VideoEditorCommand::MarkIn);
    send(VideoEditorEvent::SetOut(2.6));
    command(VideoEditorCommand::StepForward);
    // Zoom 2x, at the first click.
    send(VideoEditorEvent::Zoom(2));
    send(VideoEditorEvent::Seek(1.0));
    snap("video-editor-zoomed");
    // The second click at 3x of its own.
    send(VideoEditorEvent::Seek(2.0));
    send(VideoEditorEvent::ClickZoom(CLICK_ZOOMS.iter().position(|z| z.1 == Some(3.0)).unwrap_or(0)));
    snap("video-editor-click-zoom");
    send(VideoEditorEvent::Smooth(false));
    send(VideoEditorEvent::Preset(PRESETS.iter().position(|p| *p == bugle_engine::ExportPreset::P720).unwrap_or(0)));
    // Cut 1.4–1.8 out of the clip; playback jumps over it.
    send(VideoEditorEvent::Seek(1.4));
    command(VideoEditorCommand::Cut);
    send(VideoEditorEvent::Seek(1.8));
    command(VideoEditorCommand::Cut);
    send(VideoEditorEvent::Seek(1.2));
    command(VideoEditorCommand::PlayPause);
    snap("video-editor-playing");
    // A markup editor opened over the video editor and closed again hands
    // the keyboard focus back to the video editor, not to another app.
    let png = std::env::temp_dir().join("bugle-smoke-focus.png");
    match RgbaImage::from_pixel(240, 160, image::Rgba([60, 120, 200, 255])).save(&png) {
        Ok(()) => {
            proxy.send(AppEvent::Editor(crate::editor_window::EditorMsg::Open(png.display().to_string())));
            std::thread::sleep(Duration::from_millis(700));
            let during = bugle_platform::key_window_title();
            proxy.send(AppEvent::Editor(crate::editor_window::EditorMsg::Close));
            std::thread::sleep(Duration::from_millis(400));
            let after = bugle_platform::key_window_title();
            let ok = after.as_deref().is_some_and(|t| t.starts_with("Edit Video"));
            eprintln!("smoke test: key window with a markup editor over the video editor: {during:?}, after closing it: {after:?} (want Edit Video…: {ok})");
            let _ = std::fs::remove_file(&png);
        }
        Err(e) => eprintln!("smoke test: editor focus step skipped ({e})"),
    }
    command(VideoEditorCommand::Escape);
    if std::env::var_os("BUGLE_SMOKE_SAVE").is_some() {
        // The window closes itself once the export is written.
        command(VideoEditorCommand::Export);
        std::thread::sleep(Duration::from_millis(5000));
    }
    proxy.send(AppEvent::VideoEditor(VideoEditorMsg::Close));
    std::thread::sleep(Duration::from_millis(300));
    for p in [&path, &side] {
        if let Err(e) = std::fs::remove_file(p) {
            eprintln!("smoke test: could not remove {} ({e})", p.display());
        }
    }
}

/// The smoke test's recording and its event track: 320x200 pt recorded at
/// 2 px/pt with the pointer hidden, moving across, clicking twice, with the
/// audio of [`SMOKE_TONES`].
fn write_smoke_recording(path: &std::path::Path, side: &std::path::Path) -> anyhow::Result<()> {
    use bugle_core::events::{CursorEvent, EventKind, MouseButton};
    let (w, h, fps) = (640u32, 400u32, 30u32);
    // The frames go to a video-only file first; the audio is added after.
    let video = path.with_file_name("bugle-smoke-video-editor-frames.mp4");
    let _frames = RemoveOnDrop(vec![video.clone()]);
    // The recording and its track, unless both were written.
    let mut written = RemoveOnDrop(vec![path.to_owned(), side.to_owned()]);
    let mut writer = bugle_platform::VideoFrameWriter::create(&video, w, h, fps, true)?;
    for i in 0..3 * fps {
        let shift = i * 4;
        writer.append(&RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([((x + shift) / 3 % 256) as u8, (y / 2) as u8, 140 + ((x / 40 + y / 40) % 2 * 60) as u8, 255])
        }))?;
    }
    writer.finish()?;
    add_smoke_audio(&video, path)?;
    let click = |t: f64, kind: EventKind, x: f64, y: f64| CursorEvent { t, kind, button: Some(MouseButton::Left), x, y };
    let mut events: Vec<CursorEvent> =
        (0..30).map(|i| CursorEvent::moved(i as f64 * 0.1, 20.0 + i as f64 * 9.0, 40.0 + i as f64 * 4.0)).collect();
    events.push(click(1.0, EventKind::Down, 110.0, 80.0));
    events.push(click(1.05, EventKind::Up, 110.0, 80.0));
    events.push(click(2.0, EventKind::Down, 200.0, 120.0));
    events.push(click(2.05, EventKind::Up, 200.0, 120.0));
    events.sort_by(|a, b| a.t.total_cmp(&b.t));
    let mut track = bugle_core::EventTrack::new(bugle_core::Rect::new(0.0, 0.0, 320.0, 200.0), false, 3.0, events);
    track.fps = Some(fps as f64);
    track.save(side)?;
    written.0.clear();
    Ok(())
}

/// Files removed (those still there) when this is dropped, so the smoke
/// recording's scratch files go on every path out.
struct RemoveOnDrop(Vec<std::path::PathBuf>);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// The smoke recording's audio: a 440 Hz tone over each (start, end,
/// amplitude) in seconds (full scale 1), silence elsewhere. The loud
/// burst fills the timeline's waveform, the second is half as tall.
const SMOKE_TONES: [(f64, f64, f64); 2] = [(0.2, 1.2, 0.8), (2.0, 2.8, 0.4)];

/// The amplitude [`SMOKE_TONES`] gives `t` seconds in (0: silent).
fn smoke_tone_at(t: f64) -> f64 {
    SMOKE_TONES.iter().find(|(a, b, _)| (*a..*b).contains(&t)).map_or(0.0, |s| s.2)
}

/// Write `path`: the video of `video` with the 3 s of [`SMOKE_TONES`] as its
/// audio track (16-bit mono PCM at 48 kHz, written as a WAV file next to it
/// and copied in without re-encoding).
fn add_smoke_audio(video: &std::path::Path, path: &std::path::Path) -> anyhow::Result<()> {
    const RATE: u32 = 48_000;
    let samples: Vec<i16> = (0..3 * RATE)
        .map(|i| {
            let t = i as f64 / RATE as f64;
            (smoke_tone_at(t) * (std::f64::consts::TAU * 440.0 * t).sin() * i16::MAX as f64) as i16
        })
        .collect();
    let bytes = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + bytes as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    // PCM, one channel, the rate, bytes a second, bytes a frame, bits.
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&bytes.to_le_bytes());
    samples.iter().for_each(|s| wav.extend_from_slice(&s.to_le_bytes()));
    let tone = path.with_file_name("bugle-smoke-video-editor-tone.wav");
    let _tone = RemoveOnDrop(vec![tone.clone()]);
    std::fs::write(&tone, wav)?;
    let muxed = bugle_platform::mux_audio(video, &tone, 0.0, 3.0, path, true, Some(30.0))?;
    anyhow::ensure!(muxed, "the tone was not added to the smoke recording");
    Ok(())
}

/// What is wrong with `peaks` (equal slices of `duration` seconds, as
/// [`EditSource::audio_peaks`] reads them, or scaled by
/// [`waveform_levels`]) for [`SMOKE_TONES`]: empty, or a slice well inside
/// a tone not near that tone's share of the loudest, or one well inside the
/// silence not near zero. Time within 50 ms of a tone's edge is not judged.
fn smoke_waveform_problems(peaks: &[f32], duration: f64) -> Vec<String> {
    if peaks.is_empty() {
        return vec!["no peaks".into()];
    }
    let loudest = SMOKE_TONES.iter().fold(0.0f64, |m, s| m.max(s.2));
    let top = peaks.iter().copied().fold(0.0f32, f32::max) as f64;
    // Judged over windows of at least 20 ms (the loudest slice in each), so
    // each holds whole cycles of the tone however fine the slices are.
    let slice = duration / peaks.len() as f64;
    let per = ((0.02 / slice).ceil() as usize).max(1);
    let mut problems = Vec::new();
    for (i, window) in peaks.chunks(per).enumerate() {
        let v = window.iter().copied().fold(0.0f32, f32::max);
        let (from, to) = ((i * per) as f64 * slice, (i * per + window.len()) as f64 * slice);
        let (a, b) = (from - 0.05, to + 0.05);
        let (amp_a, amp_b) = (smoke_tone_at(a.max(0.0)), smoke_tone_at(b.min(duration - 1e-9)));
        if amp_a != amp_b || SMOKE_TONES.iter().any(|(s, e, _)| (a < *s && *s < b) || (a < *e && *e < b)) {
            continue;
        }
        let want = amp_a / loudest * top;
        let ok = if amp_a == 0.0 { (v as f64) < 0.02 * top.max(0.1) } else { ((v as f64) - want).abs() < 0.15 * want };
        if !ok && problems.len() < 5 {
            problems.push(format!("{from:.2}–{to:.2}s at {v:.3}, want {want:.3}"));
        }
    }
    problems
}

/// `--smoke-test` step, with the video editor open on the smoke recording
/// at `path`: wait for the waveform to reach the timeline, then check the
/// levels it holds (all [`WAVEFORM_BUCKETS`] of them, scaled so the loudest
/// is 1, the shape of [`SMOKE_TONES`], what a second read of the file gives)
/// and how a capture of the window draws them ([`smoke_drawn_problems`]).
/// Prints what it found; returns whether every check passed.
fn smoke_waveform(path: &std::path::Path) -> bool {
    let t0 = Instant::now();
    let shown = loop {
        match bugle_platform::video_editor_waveform() {
            Some(w) if !w.is_empty() => break Some(w),
            _ if t0.elapsed() > Duration::from_secs(5) => break None,
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let Some(shown) = shown else {
        let waited = t0.elapsed().as_secs_f64();
        eprintln!("smoke test: video editor waveform not shown after {waited:.1}s (want {WAVEFORM_BUCKETS} levels: false)");
        return false;
    };
    let waited = t0.elapsed().as_secs_f64();
    let duration = bugle_platform::video_info(path).map(|i| i.start + i.duration).unwrap_or(f64::NAN);
    let read = bugle_platform::audio_peaks(path, duration, WAVEFORM_BUCKETS, None);
    // How far the window's levels are from those of a second read (reads
    // of one file differ a little: the decoder's resampling).
    let off = match &read {
        Ok(Some(p)) if p.len() == shown.len() => {
            Some(waveform_levels(p).iter().zip(&shown).fold(0.0f32, |m, (a, b)| m.max((a - b).abs())))
        }
        Ok(_) => None,
        Err(e) => {
            eprintln!("smoke test: video editor waveform could not be read again ({e:#})");
            None
        }
    };
    let top = shown.iter().copied().fold(0.0f32, f32::max);
    let problems = smoke_waveform_problems(&shown, duration);
    // The loudest level over the 20 ms from `t`.
    let at = |t: f64| {
        let i = |t: f64| ((t / duration * shown.len() as f64) as usize).min(shown.len());
        shown[i(t)..i(t + 0.02).max(i(t) + 1).min(shown.len())].iter().copied().fold(f32::NAN, f32::max)
    };
    // Let the timeline redraw before capturing it.
    std::thread::sleep(Duration::from_millis(500));
    let drawn = smoke_drawn_problems(duration);
    let ok = shown.len() == WAVEFORM_BUCKETS
        && (top - 1.0).abs() < 0.01
        && off.is_some_and(|d| d < 0.01)
        && problems.is_empty()
        && drawn.as_ref().is_ok_and(|d| d.1.is_empty());
    eprintln!(
        "smoke test: video editor waveform {} levels after {waited:.1}s, loudest {top:.2}, at 0.7s {:.2}, 1.6s {:.2}, 2.4s {:.2}; \
         a second read differs by {off:?}; problems: {problems:?}; drawn (rows at 0.7s, 1.6s, 2.4s; problems): {drawn:?} \
         (want {WAVEFORM_BUCKETS} levels, loudest 1.00, 1.00 / 0.00 / 0.50, under 0.01, no problems: {ok})",
        shown.len(),
        at(0.7),
        at(1.6),
        at(2.4),
    );
    ok
}

/// Capture the video editor's window and measure the waveform drawn in its
/// timeline track: in each pixel column, the rows that differ clearly from
/// the same rows of a column in the silence (so the colours of either
/// appearance do not matter), averaged over a few columns around 0.7 s
/// (the loud tone), 1.6 s (silence) and 2.4 s (the half-level tone). Returns
/// those three heights in pixels and what is wrong with them: the loud
/// tone should fill most of the track, the half-level one about half as
/// much, the silence nothing.
fn smoke_drawn_problems(duration: f64) -> Result<([f64; 3], Vec<String>), String> {
    let (track, (win_w, _)) = bugle_platform::video_editor_track_frame().ok_or("no video editor")?;
    let window = bugle_platform::app_windows()
        .map_err(|e| format!("{e:#}"))?
        .into_iter()
        .find(|w| w.title.starts_with("Edit Video"))
        .ok_or("window not found")?;
    let shot = bugle_platform::capture_window(window.id, false, false).map_err(|e| format!("capture: {e:#}"))?;
    let scale = shot.width() as f64 / win_w;
    // Inside the track, clear of its outline.
    let (top, bottom) = (((track.y + 2.0) * scale) as u32, ((track.y + track.h - 2.0) * scale) as u32);
    if bottom as f64 > shot.height() as f64 || top >= bottom {
        return Err(format!("track {track:?} is outside the {}x{} capture", shot.width(), shot.height()));
    }
    let col = |t: f64| ((track.x + track.w * t / duration) * scale) as u32;
    let base = col(1.6);
    let differs = |x: u32, y: u32| {
        let (a, b) = (shot.get_pixel(x, y).0, shot.get_pixel(base, y).0);
        (0..3).map(|c| (a[c] as i32 - b[c] as i32).abs()).sum::<i32>() > 40
    };
    // Rows differing from the silent column, averaged over five columns
    // spaced 3 px apart either side of `t` (the silent one compares columns
    // 60 ms either side of the base).
    let height = |t: f64| {
        let cols: Vec<u32> = if t == 1.6 {
            vec![col(1.54), col(1.66)]
        } else {
            (-2..=2).map(|k| (col(t) as i32 + 3 * k).max(0) as u32).collect()
        };
        let rows: usize = cols.iter().map(|&x| (top..bottom).filter(|&y| differs(x, y)).count()).sum();
        rows as f64 / cols.len() as f64
    };
    let h = [height(0.7), height(1.6), height(2.4)];
    let full = (bottom - top) as f64;
    let mut problems = Vec::new();
    if h[0] < 0.6 * full {
        problems.push(format!("the loud tone is {:.0} of {full:.0} rows tall", h[0]));
    }
    if h[1] > 0.1 * h[0].max(1.0) {
        problems.push(format!("the silence is {:.0} rows tall", h[1]));
    }
    if !(0.35..=0.65).contains(&(h[2] / h[0].max(1.0))) {
        problems.push(format!("the half-level tone is {:.0} rows to the loud one's {:.0}", h[2], h[0]));
    }
    Ok((h, problems))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_recording_has_the_tones_as_audio() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bugle-smoke-video-editor.mp4");
        let side = bugle_core::EventTrack::sidecar_path(&path);
        write_smoke_recording(&path, &side).unwrap();
        assert_eq!(bugle_platform::audio_track_count(&path).unwrap(), 1);
        let peaks = bugle_platform::audio_peaks(&path, 3.0, 30, None).unwrap().expect("audio");
        let problems = smoke_waveform_problems(&peaks, 3.0);
        assert!(problems.is_empty(), "{problems:?} in {peaks:?}");
        assert!(smoke_waveform_problems(&waveform_levels(&peaks), 3.0).is_empty(), "scaled: {:?}", waveform_levels(&peaks));
        assert!((peaks[7] - 0.8).abs() < 0.05 && peaks[16] < 0.01 && (peaks[24] - 0.4).abs() < 0.05, "{peaks:?}");
        // As finely as the window reads them, too.
        let fine = bugle_platform::audio_peaks(&path, 3.0, WAVEFORM_BUCKETS, None).unwrap().expect("audio");
        let problems = smoke_waveform_problems(&waveform_levels(&fine), 3.0);
        assert!(problems.is_empty(), "{problems:?}");
        let again = bugle_platform::audio_peaks(&path, 3.0, WAVEFORM_BUCKETS, None).unwrap().expect("audio");
        let off = fine.iter().zip(&again).fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        // Reads of the same file differ a little (the decoder's resampling),
        // never by much.
        assert!(off < 0.01, "a second read is {off} off");
        // The wrong shape is caught: backwards, flat, missing.
        let backwards: Vec<f32> = peaks.iter().rev().copied().collect();
        assert!(!smoke_waveform_problems(&backwards, 3.0).is_empty());
        assert!(!smoke_waveform_problems(&[0.5; 30], 3.0).is_empty());
        assert!(!smoke_waveform_problems(&[], 3.0).is_empty());
        // Only the files asked for are left behind.
        let mut left: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        left.sort();
        assert_eq!(left.len(), 2, "{left:?}");
    }

    #[test]
    fn a_failed_smoke_recording_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bugle-smoke-video-editor.mp4");
        // The event track cannot be saved: the recording is removed too.
        let side = tmp.path().join("missing").join("track.json");
        assert!(write_smoke_recording(&path, &side).is_err());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }
}
