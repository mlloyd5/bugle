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

/// Whether the open video editor (if any) is exporting.
pub(crate) fn export_running() -> bool {
    CURRENT.with(|c| c.borrow().upgrade()).is_some_and(|s| s.try_borrow().map_or(true, |s| s.editor.busy()))
}

impl App {
    /// The most recent video in the history, for *Edit Latest Recording*.
    pub(crate) fn edit_latest_recording(&mut self) {
        let all = self.engine.history.all().unwrap_or_default();
        match all.iter().rev().find(|m| m.kind == bugle_core::MediaKind::Video && m.path.exists()) {
            Some(m) => self.open_video_editor(&m.id.clone()),
            None => self.toast_error("Edit Video: no video recording in the history yet"),
        }
    }

    /// Open the video editor for a media id, `latest` or a video path. One
    /// at a time: with one already open, that one comes forward.
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
        let caps = Caps { zoom: src.can_zoom(), smooth: src.can_smooth() };
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
        let preset_labels: Vec<&str> = PRESETS.iter().map(|p| preset_label(*p)).collect();
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
        spawn_waveform(src.clone(), self.proxy.clone(), generation, waveform_cancel);
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
/// gradient, 640x400 at 30 fps) with an event track (pointer hidden, two
/// clicks) to the temp directory — not added to history — open it in the
/// video editor, and drive it with injected events (nothing can click):
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
/// 2 px/pt with the pointer hidden, moving across, clicking twice.
fn write_smoke_recording(path: &std::path::Path, side: &std::path::Path) -> anyhow::Result<()> {
    use bugle_core::events::{CursorEvent, EventKind, MouseButton};
    let (w, h, fps) = (640u32, 400u32, 30u32);
    let mut writer = bugle_platform::VideoFrameWriter::create(path, w, h, fps, true)?;
    for i in 0..3 * fps {
        let shift = i * 4;
        writer.append(&RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([((x + shift) / 3 % 256) as u8, (y / 2) as u8, 140 + ((x / 40 + y / 40) % 2 * 60) as u8, 255])
        }))?;
    }
    writer.finish()?;
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
    track.save(side)
}
