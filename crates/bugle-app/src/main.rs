//! `bugle-app` — the menu-bar front end.
//!
//! An accessory (no Dock icon) AppKit process: a status-bar item with a menu,
//! global hotkeys, and the same [`Engine`] the CLI and servers use. The main
//! thread runs the `tao` event loop; engine work happens on one worker thread
//! so the menu stays responsive, and the worker asks the main thread to show
//! the selection overlay or pin windows through [`UiHost`].

mod audio_menu;
mod commands;
mod editor_window;
mod icon;
mod level_meter;
mod login;
mod markup_editor;
mod notifications;
mod picker;
mod quick_access;
mod quick_panel;
mod smoke_input;
mod video_editor;
mod video_editor_window;

use anyhow::{anyhow, Context, Result};
use bugle_core::{
    CaptureOptions, CaptureRequest, CaptureTarget, Config, MediaKind, OcrOptions, RecordRequest, RecordingFormat,
    RecordingState, Rect, ScrollCaptureRequest, WindowInfo,
};
use commands::{Command, Slot};
use bugle_engine::{Engine, UiHost};
use clap::Parser;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::{accelerator::Accelerator, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder};

#[derive(Parser, Debug)]
#[command(name = "bugle-app", version, about = "Bugle menu-bar app: status item, global hotkeys, in-app overlays.")]
struct Cli {
    /// Config file (default ~/.config/bugle/config.toml or $BUGLE_CONFIG).
    #[arg(long, env = "BUGLE_CONFIG")]
    config: Option<PathBuf>,
    /// Start the agent API (HTTP) at launch.
    #[arg(long)]
    serve: bool,
    /// Debug logging.
    #[arg(short, long)]
    verbose: bool,
    /// Show the overlay, a toast, a pin, the quick-access panel, the
    /// markup editor and the video editor, then exit. Exercises the in-app UI plumbing without
    /// needing a keyboard.
    #[arg(long, hide = true)]
    smoke_test: bool,
    /// Open the markup editor at launch for a media id, `latest` or an
    /// image path.
    #[arg(long, hide = true, value_name = "SOURCE")]
    annotate: Option<String>,
    /// Open the video editor at launch for a media id, `latest` or a video
    /// path.
    #[arg(long, hide = true, value_name = "SOURCE")]
    edit_video: Option<String>,
}

// ---------------------------------------------------------------------------
// Events and actions
// ---------------------------------------------------------------------------

/// Engine work the user can ask for; [`commands::MENU`] maps menu items and
/// hotkeys to these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    CaptureArea,
    CaptureWindow,
    CaptureScreen,
    ScrollCapture,
    RecordVideo,
    RecordGif,
    /// MP4 recording of a window picked with the window picker.
    RecordWindow,
    StopRecording,
    OcrLatest,
    PinLatest,
    SyncLatest,
}

/// Something to tell the user once an action finished.
struct Notice {
    title: String,
    subtitle: String,
    thumb: Option<Vec<u8>>,
    open: Option<PathBuf>,
}

/// Requests from worker threads that must run on the main thread.
enum UiRequest {
    Select(Sender<Option<Rect>>),
    Pin { png: Vec<u8>, frame: Option<Rect>, opacity: f64, reply: Sender<Result<(), String>> },
    /// Window picker / area selection session (Space switches); see
    /// [`picker::begin`].
    PickWindow(picker::Request),
    /// Space or Esc for the open overlay, without a keyboard (`--smoke-test`).
    PickControl(picker::Control),
}

enum AppEvent {
    Menu(MenuId),
    HotKey(u32),
    Ui(UiRequest),
    Done(Action, Result<Option<Notice>, String>),
    ApiStopped(Option<String>),
    QuickAccess(quick_access::QuickEvent),
    Editor(editor_window::EditorMsg),
    VideoEditor(video_editor_window::VideoEditorMsg),
    /// The answer to an *Audio Levels* prompt (`None`: cancelled).
    AudioPrompt(audio_menu::Field, Option<String>),
    Quit,
}

/// `EventLoopProxy` is `Send` but not `Sync`; the hotkey and menu handlers
/// need a `Sync` handle.
#[derive(Clone)]
struct Proxy(Arc<Mutex<EventLoopProxy<AppEvent>>>);

impl Proxy {
    fn send(&self, e: AppEvent) {
        if let Ok(p) = self.0.lock() {
            let _ = p.send_event(e);
        }
    }
}

// ---------------------------------------------------------------------------
// UiHost: worker threads → main thread
// ---------------------------------------------------------------------------

struct AppUiHost {
    proxy: Proxy,
}

impl UiHost for AppUiHost {
    fn select_area(&self) -> Result<Option<Rect>> {
        // One overlay request at a time (shared with pick sessions); a
        // second selection waits for the first.
        let _gate = picker::wait_gate();
        let (tx, rx) = mpsc::channel();
        self.proxy.send(AppEvent::Ui(UiRequest::Select(tx)));
        rx.recv().context("selection overlay closed without a result")
    }

    fn pin(&self, png: Vec<u8>, frame: Option<Rect>, opacity: f64) -> Result<()> {
        let (reply, rx) = mpsc::channel();
        self.proxy.send(AppEvent::Ui(UiRequest::Pin { png, frame, opacity, reply }));
        rx.recv().context("pin request dropped")?.map_err(|e| anyhow!(e))
    }

    fn pick_window(&self) -> Result<Option<WindowInfo>> {
        // Refused (BugleError::Busy), not queued, while another pick or
        // selection is open or just after the user dismissed one.
        picker::pick_window(&self.proxy, picker::PICK_TIMEOUT)
    }
}

// ---------------------------------------------------------------------------
// Worker thread: runs engine actions one at a time
// ---------------------------------------------------------------------------

fn worker(engine: Arc<Engine>, rx: Receiver<Action>, proxy: Proxy) {
    for action in rx {
        let result = run_action(&engine, action, &proxy).map_err(|e| format!("{e:#}"));
        proxy.send(AppEvent::Done(action, result));
    }
}

fn run_action(engine: &Engine, action: Action, proxy: &Proxy) -> Result<Option<Notice>> {
    match action {
        // With the window picker on, Space switches between it and the area
        // selection; the session answers with whichever the user finished in.
        Action::CaptureArea | Action::CaptureWindow if engine.config.screenshots.window_picker => {
            let start = if action == Action::CaptureArea { picker::Mode::Area } else { picker::Mode::Window };
            let Some(picked) = picker::pick(proxy, start, true, picker::PICK_TIMEOUT)? else { return Ok(None) };
            capture(engine, picked_target(picked))
        }
        Action::CaptureArea => capture(engine, CaptureTarget::Interactive),
        Action::CaptureWindow => {
            // Let the status menu fade before grabbing.
            std::thread::sleep(Duration::from_millis(150));
            capture(engine, CaptureTarget::Window { id: None, title: None, owner: None })
        }
        Action::CaptureScreen => {
            std::thread::sleep(Duration::from_millis(150));
            capture(engine, CaptureTarget::Screen { display: None })
        }
        Action::ScrollCapture => {
            let rect = if engine.config.screenshots.window_picker {
                // Space switches to the window picker; a picked window
                // scrolls its on-screen part, unless another window covers it.
                let Some(picked) = picker::pick(proxy, picker::Mode::Area, true, picker::PICK_TIMEOUT)? else { return Ok(None) };
                picker::scroll_rect(&picked)?
            } else {
                let Some(rect) = engine.select_area()? else { return Ok(None) };
                rect
            };
            let req = ScrollCaptureRequest {
                rect,
                max_frames: 40,
                step_lines: -8,
                settle_ms: 350,
                horizontal: false,
                options: CaptureOptions::default(),
            };
            let (out, stats) = engine.scroll_capture(&req)?;
            if engine.config.general.shutter_sound {
                bugle_platform::play_sound("Tink");
            }
            Ok(Some(Notice {
                title: format!("Scrolling capture saved ({} frames)", stats.frames),
                subtitle: saved_subtitle(engine, &out.item),
                thumb: bugle_engine::encode_png_bounded(&out.image, 112).ok(),
                open: Some(out.item.path.clone()),
            }))
        }
        Action::RecordVideo | Action::RecordGif | Action::RecordWindow => {
            let format = if action == Action::RecordGif { RecordingFormat::Gif } else { RecordingFormat::Mp4 };
            let target = if engine.config.screenshots.window_picker {
                let start = if action == Action::RecordWindow { picker::Mode::Window } else { picker::Mode::Area };
                let Some(picked) = picker::pick(proxy, start, true, picker::PICK_TIMEOUT)? else { return Ok(None) };
                picked_target(picked)
            } else if action == Action::RecordWindow {
                // Front-most window, like Capture Window without the picker.
                std::thread::sleep(Duration::from_millis(150));
                CaptureTarget::Window { id: None, title: None, owner: None }
            } else {
                CaptureTarget::Interactive
            };
            let req = RecordRequest {
                target,
                format,
                fps: None,
                microphone: false,
                system_audio: engine.config.recordings.system_audio,
                mix_audio: false,
                // The engine's levels (the Audio Levels menu's) apply.
                mix: Default::default(),
                max_seconds: None,
                highlight_clicks: false,
                cursor_events: false,
                hide_cursor: false,
                output: None,
                label: None,
            };
            let status = match engine.record_start(&req) {
                Ok(s) => s,
                Err(e) if is_cancelled(&e) => return Ok(None),
                Err(e) => return Err(e),
            };
            Ok(Some(Notice {
                title: format!("Recording {}", if format == RecordingFormat::Gif { "GIF" } else { "video" }),
                subtitle: status.output.as_deref().map(|p| file_name(std::path::Path::new(p))).unwrap_or_default(),
                thumb: None,
                open: None,
            }))
        }
        Action::StopRecording => {
            let media = engine.record_stop()?;
            if engine.config.general.shutter_sound {
                bugle_platform::play_sound("Tink");
            }
            Ok(Some(Notice {
                title: format!("Recording saved ({:.0}s)", media.duration_seconds.unwrap_or(0.0)),
                subtitle: saved_subtitle(engine, &media),
                thumb: notice_thumb(&media),
                open: Some(media.path.clone()),
            }))
        }
        Action::OcrLatest => {
            let (media, r) = engine.ocr("latest", &OcrOptions::default())?;
            let text = r.text.trim().to_string();
            if text.is_empty() {
                return Ok(Some(Notice {
                    title: "No text found".into(),
                    subtitle: file_name(&media.path),
                    thumb: None,
                    open: Some(media.path.clone()),
                }));
            }
            bugle_platform::clipboard_write_text(&text)?;
            let first = text.lines().next().unwrap_or("").to_string();
            Ok(Some(Notice {
                title: format!("OCR copied ({} lines)", r.lines.len().max(text.lines().count())),
                subtitle: first,
                thumb: None,
                open: Some(media.path.clone()),
            }))
        }
        Action::PinLatest => {
            engine.pin("latest", None, 1.0, false)?;
            Ok(None)
        }
        Action::SyncLatest => {
            let (media, report) = engine.sync_media("latest")?;
            let thumb = if media.is_image() {
                image::open(&media.path).ok().and_then(|i| bugle_engine::encode_png_bounded(&i.to_rgba8(), 112).ok())
            } else {
                None
            };
            let mut action = report.action.clone();
            if let Some(c) = action.get_mut(..1) {
                c.make_ascii_uppercase();
            }
            Ok(Some(Notice {
                title: format!("{action}: {}", file_name(&media.path)),
                subtitle: report.destination,
                thumb,
                open: Some(media.path.clone()),
            }))
        }
    }
}

/// A small thumbnail of a recording (its first frame) or a GIF/image, for
/// the quick-access panel or a toast.
pub(crate) fn notice_thumb(media: &bugle_core::MediaItem) -> Option<Vec<u8>> {
    let img = match media.kind {
        MediaKind::Video => bugle_platform::video_frame(&media.path, 0.0).ok(),
        _ => image::open(&media.path).ok().map(|i| i.to_rgba8()),
    };
    img.and_then(|i| bugle_engine::encode_png_bounded(&i, 112).ok())
}

/// Whether an error means the user cancelled the area selection. Matches the
/// `BugleError::Cancelled` variant; the message check covers paths that still
/// return a plain `anyhow!("selection cancelled")`.
fn is_cancelled(e: &anyhow::Error) -> bool {
    e.chain().any(|c| matches!(c.downcast_ref::<bugle_core::BugleError>(), Some(bugle_core::BugleError::Cancelled)))
        || format!("{e:#}").contains("selection cancelled")
}

/// The capture target for what a pick session returned.
fn picked_target(picked: picker::Picked) -> CaptureTarget {
    match picked {
        picker::Picked::Window(w) => CaptureTarget::Window { id: Some(w.id), title: None, owner: None },
        picker::Picked::Area(rect) => CaptureTarget::Area { rect },
    }
}

fn capture(engine: &Engine, target: CaptureTarget) -> Result<Option<Notice>> {
    let req = CaptureRequest { target, options: CaptureOptions::default() };
    let out = match engine.capture(&req) {
        Ok(o) => o,
        Err(e) if is_cancelled(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    let what = match &out.window {
        Some(w) if !w.owner.is_empty() => format!("{} window captured", w.owner),
        _ => format!("Screenshot saved ({}×{})", out.image.width(), out.image.height()),
    };
    Ok(Some(Notice {
        title: what,
        subtitle: saved_subtitle(engine, &out.item),
        thumb: bugle_engine::encode_png_bounded(&out.image, 112).ok(),
        open: Some(out.item.path.clone()),
    }))
}

/// Toast subtitle for a saved item: the object URL after an upload, the
/// file name plus the sync folder after a copy/move, else the file name.
fn saved_subtitle(engine: &Engine, item: &bugle_core::MediaItem) -> String {
    if let Some(url) = &item.url {
        return url.clone();
    }
    let name = file_name(&item.path);
    if engine.config.sync.applies_to(item.kind) {
        if let Some(label) = engine.sync_destination_label_for(item.kind) {
            return format!("{name} → {label}");
        }
    }
    name
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

// ---------------------------------------------------------------------------
// Agent API on its own runtime thread
// ---------------------------------------------------------------------------

struct ApiHandle {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

fn start_api(engine: Arc<Engine>, proxy: Proxy) -> Result<ApiHandle> {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let bind = engine.config.api.bind.clone();
    let token = engine.config.api.token.clone();
    let cors = engine.config.api.cors;
    std::thread::Builder::new().name("bugle-api".into()).spawn(move || {
        let res = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt.block_on(bugle_api::http::serve_with_shutdown(engine, &bind, token, cors, async {
                let _ = rx.await;
            })),
            Err(e) => Err(e.into()),
        };
        proxy.send(AppEvent::ApiStopped(res.err().map(|e| format!("{e:#}"))));
    })?;
    Ok(ApiHandle { shutdown: Some(tx) })
}

impl ApiHandle {
    fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

// ---------------------------------------------------------------------------
// Menu
// ---------------------------------------------------------------------------

struct MenuItems {
    menu: Menu,
    record: MenuItem,
    record_gif: MenuItem,
    record_window: MenuItem,
    stop: MenuItem,
    recent: Submenu,
    api: MenuItem,
    login: Option<login::LoginMenu>,
    audio: audio_menu::AudioMenu,
    meter: level_meter::LevelMeter,
}

/// Build the status menu from [`commands::MENU`]. `keys` maps entry names to
/// the hotkeys that registered, shown as the items' key equivalents.
fn build_menu(config: &Config, keys: &HashMap<&'static str, String>) -> Result<MenuItems> {
    let menu = Menu::new();
    let recent = Submenu::with_id("recent", "Recent Captures", true);
    let login = login::LoginMenu::new();
    let audio = audio_menu::AudioMenu::new(
        config.recordings.mix,
        audio_menu::requirement(&config.recordings, bugle_platform::sck_record_available()),
    )?;
    let mut by_name: HashMap<&'static str, MenuItem> = HashMap::new();
    let mut meter_at = None;
    for slot in commands::MENU {
        match slot {
            Slot::Separator => menu.append(&PredefinedMenuItem::separator())?,
            Slot::Recent => menu.append(&recent)?,
            Slot::OpenAtLogin => {
                if let Some(l) = &login {
                    menu.append(&l.item)?;
                }
            }
            Slot::AudioLevels => menu.append(&audio.submenu)?,
            Slot::Item(e) => {
                let key = keys.get(e.name).map(String::as_str).or(e.menu_key);
                let item = MenuItem::with_id(e.name, e.label, true, key.and_then(|k| k.parse::<Accelerator>().ok()));
                menu.append(&item)?;
                by_name.insert(e.name, item);
                if e.name == "stop_recording" {
                    meter_at = Some(menu.items().len());
                }
            }
        }
    }
    let mut take = |name: &str| by_name.remove(name).ok_or_else(|| anyhow!("menu table has no {name}"));
    let items = MenuItems {
        record: take("record")?,
        record_gif: take("record_gif")?,
        record_window: take("record_window")?,
        stop: take("stop_recording")?,
        api: take("toggle_api")?,
        menu,
        recent,
        login,
        audio,
        meter: level_meter::LevelMeter::new(meter_at.ok_or_else(|| anyhow!("menu table has no stop_recording"))?),
    };
    take("sync_latest")?.set_enabled(config.sync.has_destination());
    items.stop.set_enabled(false);
    items.api.set_text(format!("Start Agent API ({})", config.api.bind));
    Ok(items)
}

// ---------------------------------------------------------------------------
// App state on the main thread
// ---------------------------------------------------------------------------

struct App {
    engine: Arc<Engine>,
    config_path: PathBuf,
    tray: TrayIcon,
    items: MenuItems,
    worker_tx: Sender<Action>,
    busy: bool,
    api: Option<ApiHandle>,
    hotkeys: HashMap<u32, Command>,
    _hotkey_manager: GlobalHotKeyManager,
    history_mtime: Option<SystemTime>,
    proxy: Proxy,
    last_tick: Instant,
    /// Whether the recording on show has a level meter (it then redraws every
    /// [`level_meter::INTERVAL`]), and when it last did.
    metering: bool,
    last_meter: Instant,
    /// What the status item and recording entries show now, so a redraw
    /// changes only what moved: (title, recording).
    recording_shown: Option<(Option<String>, bool)>,
    quick: quick_access::QuickAccess,
    /// Numbers editor windows, so a late save or preview reaches only its own.
    editor_generation: u64,
    /// The same for video editor windows and their exports.
    video_editor_generation: u64,
}

impl App {
    fn is_recording(&self) -> bool {
        self.engine.record_status().state != RecordingState::Idle
    }

    fn toast(&self, n: &Notice) {
        bugle_platform::toast_show(n.thumb.as_deref(), &n.title, &n.subtitle, n.open.as_deref(), 3.0);
        notifications::post(&n.title, &n.subtitle, n.open.as_deref(), n.thumb.as_deref());
    }

    fn toast_error(&self, msg: &str) {
        tracing::warn!("{msg}");
        bugle_platform::toast_show(None, "Bugle", msg, None, 5.0);
        notifications::post("Bugle", msg, None, None);
    }

    fn dispatch(&mut self, mut action: Action) {
        let recording = self.is_recording();
        if matches!(action, Action::RecordVideo | Action::RecordGif | Action::RecordWindow) && recording {
            action = Action::StopRecording;
        }
        if action == Action::StopRecording && !recording {
            return;
        }
        if self.busy || bugle_platform::select_rect_active() || bugle_platform::window_pick_active() {
            tracing::debug!("ignoring {action:?}: busy");
            return;
        }
        self.quick_before_action(action);
        self.busy = true;
        if self.worker_tx.send(action).is_err() {
            self.busy = false;
            self.toast_error("worker thread is gone");
        }
    }

    fn on_menu(&mut self, id: &MenuId, control_flow: &mut ControlFlow) {
        let id = id.0.as_str();
        if let Some(path) = id.strip_prefix("recent:") {
            bugle_platform::open_path(std::path::Path::new(path));
            return;
        }
        if id == login::MENU_ID {
            return self.toggle_login();
        }
        match self.items.audio.click(id) {
            Some(audio_menu::Step::Changed(change)) => return self.audio_changed(change),
            Some(audio_menu::Step::Ask(field, p)) => {
                // Not modal: the answer comes back as an `AudioPrompt` event,
                // and the event loop keeps running meanwhile. A prompt that is
                // already open is brought to the front instead.
                let proxy = self.proxy.clone();
                let done = move |text| proxy.send(AppEvent::AudioPrompt(field, text));
                bugle_platform::prompt_text(&p.title, &p.message, &p.initial, done);
                return;
            }
            None => {}
        }
        match commands::find(id) {
            Some(e) => self.run_command(e.command, control_flow),
            None => tracing::warn!("unknown menu item {id}"),
        }
    }

    /// *Open at Login* was clicked (not a [`Command`]: it has no hotkey).
    /// The answer to an *Audio Levels* prompt (`None`: cancelled).
    fn on_audio_prompt(&mut self, field: audio_menu::Field, text: Option<String>) {
        tracing::info!("audio prompt for {field:?} answered: {}", if text.is_some() { "OK" } else { "cancelled" });
        match self.items.audio.answer(field, text.as_deref()) {
            audio_menu::Outcome::Changed(change) => self.audio_changed(change),
            audio_menu::Outcome::Invalid(e) => self.toast_error(&format!("Audio Levels: {e}")),
            audio_menu::Outcome::Cancelled => {}
        }
    }

    /// Hand the menu's new levels to the engine and save `change`.
    fn audio_changed(&mut self, change: audio_menu::Change) {
        self.engine.set_recording_levels(self.items.audio.levels());
        if let Err(e) = audio_menu::persist(&self.config_path, change) {
            self.toast_error(&format!("Audio Levels: the change applies until Bugle quits but was not saved: {e:#}"));
        }
    }

    fn toggle_login(&mut self) {
        match self.items.login.as_mut().map(login::LoginMenu::toggle) {
            Some(Ok(Some(msg))) => self.toast_error(&msg),
            Some(Err(e)) => self.toast_error(&format!("Open at Login: {e:#}")),
            _ => {}
        }
    }

    /// The one entry point for menu items and hotkeys.
    fn run_command(&mut self, command: Command, control_flow: &mut ControlFlow) {
        // While an overlay is up (it covers the screen), only Quit acts;
        // anything else would happen invisibly behind it.
        let overlay = bugle_platform::window_pick_active() || bugle_platform::select_rect_active();
        if overlay && command != Command::Quit {
            tracing::debug!("ignoring {command:?}: an overlay is open");
            return;
        }
        match command {
            Command::Run(a) => self.dispatch(a),
            Command::AnnotateLatest => self.annotate_latest(),
            Command::EditLatest => self.edit_latest_recording(),
            Command::OpenFolder => bugle_platform::open_path(&self.engine.config.save_dir()),
            Command::ToggleApi => self.toggle_api(),
            Command::Preferences => self.open_preferences(),
            Command::Quit => self.quit(control_flow),
        }
    }

    fn on_ui(&mut self, req: UiRequest) {
        match req {
            UiRequest::Select(tx) => {
                let started = bugle_platform::select_rect_begin(move |r| {
                    let _ = tx.send(r);
                });
                if !started {
                    tracing::warn!("selection overlay already active");
                }
                // `tx` was moved into the callback; when `started` is false the
                // callback is dropped, which closes the channel and unblocks
                // the caller with an error.
            }
            UiRequest::Pin { png, frame, opacity, reply } => {
                let _ = reply.send(bugle_platform::pin_image_show(&png, frame, opacity).map_err(|e| e.to_string()));
            }
            UiRequest::PickWindow(req) => {
                // On failure `begin` answers the request with the reason itself.
                picker::begin(req);
            }
            UiRequest::PickControl(c) => picker::control(c),
        }
    }

    fn on_done(&mut self, action: Action, result: Result<Option<Notice>, String>) {
        self.busy = false;
        match result {
            Ok(Some(n)) if self.quick_show(action, &n) => {
                notifications::post(&n.title, &n.subtitle, n.open.as_deref(), n.thumb.as_deref())
            }
            Ok(Some(n)) => self.toast(&n),
            Ok(None) => {}
            Err(e) => self.toast_error(&format!("{action:?}: {e}")),
        }
        self.quick_after_action();
        self.refresh_recent();
        self.update_recording_ui();
    }

    fn update_recording_ui(&mut self) {
        let st = self.engine.record_status();
        let recording = st.state != RecordingState::Idle;
        let title = recording.then(|| {
            let secs = st.elapsed_seconds as u64;
            let meter = st.audio_levels.as_ref().and_then(bugle_core::audio_level::meter_title);
            let meter = meter.map(|m| format!(" {m}")).unwrap_or_default();
            format!("● {}:{:02}{meter}", secs / 60, secs % 60)
        });
        let shown = (title, recording);
        if self.recording_shown.as_ref() != Some(&shown) {
            if self.recording_shown.as_ref().map(|s| s.1) != Some(recording) {
                self.items.stop.set_enabled(recording);
                self.items.record.set_enabled(!recording);
                self.items.record_gif.set_enabled(!recording);
                self.items.record_window.set_enabled(!recording);
            }
            self.tray.set_title(shown.0.as_deref());
            self.recording_shown = Some(shown);
        }
        if let Err(e) = self.items.meter.update(&self.items.menu, st.audio_levels.as_ref()) {
            tracing::warn!("level meter: {e:#}");
        }
        self.metering = st.audio_levels.is_some();
        self.last_meter = Instant::now();
    }

    /// How long the event loop may sleep before the next [`App::tick`].
    fn wake_interval(&self) -> Duration {
        if self.metering {
            level_meter::INTERVAL
        } else {
            Duration::from_secs(1)
        }
    }

    fn refresh_recent(&mut self) {
        let mtime = std::fs::metadata(self.engine.history.path()).and_then(|m| m.modified()).ok();
        if mtime == self.history_mtime && !self.items.recent.items().is_empty() {
            return;
        }
        self.history_mtime = mtime;
        while self.items.recent.remove_at(0).is_some() {}
        let recent = self.engine.history.recent(5).unwrap_or_default();
        if recent.is_empty() {
            let _ = self.items.recent.append(&MenuItem::with_id("recent:none", "No captures yet", false, None));
            return;
        }
        for m in recent {
            let kind = match m.kind {
                MediaKind::Screenshot => "screenshot",
                MediaKind::ScrollCapture => "scrolling",
                MediaKind::Annotated => "annotated",
                MediaKind::Video => "video",
                MediaKind::Gif => "gif",
                MediaKind::Audio => "audio",
                MediaKind::Subtitle => "subtitles",
                MediaKind::Text => "text",
            };
            let text = format!("{}  ({kind}, {})", file_name(&m.path), m.created_at.with_timezone(&chrono_local()).format("%H:%M"));
            let id = format!("recent:{}", m.path.display());
            let _ = self.items.recent.append(&MenuItem::with_id(id, text, m.path.exists(), None));
        }
    }

    fn toggle_api(&mut self) {
        if let Some(api) = self.api.take() {
            api.stop();
            self.items.api.set_text(format!("Start Agent API ({})", self.engine.config.api.bind));
            return;
        }
        match start_api(self.engine.clone(), self.proxy.clone()) {
            Ok(h) => {
                self.api = Some(h);
                self.items.api.set_text("Stop Agent API");
                bugle_platform::toast_show(
                    None,
                    "Agent API running",
                    &format!("http://{}/v1", self.engine.config.api.bind),
                    None,
                    2.5,
                );
            }
            Err(e) => self.toast_error(&format!("could not start API: {e:#}")),
        }
    }

    fn on_api_stopped(&mut self, err: Option<String>) {
        self.api = None;
        self.items.api.set_text(format!("Start Agent API ({})", self.engine.config.api.bind));
        if let Some(e) = err {
            self.toast_error(&format!("Agent API stopped: {e}"));
        }
    }

    fn open_preferences(&self) {
        if !self.config_path.exists() {
            if let Err(e) = Config::default().save_to(&self.config_path) {
                self.toast_error(&format!("could not write {}: {e}", self.config_path.display()));
                return;
            }
        }
        bugle_platform::open_path(&self.config_path);
    }

    fn quit(&mut self, control_flow: &mut ControlFlow) {
        // Quitting mid-export would drop the clip and could leave a
        // temporary file in the save folder.
        if video_editor_window::export_running() {
            bugle_platform::video_editor_focus();
            return self.toast_error("Edit Video: an export is running; quit once it has finished");
        }
        if self.is_recording() {
            match self.engine.record_stop() {
                Ok(m) => tracing::info!("recording saved to {}", m.path.display()),
                Err(e) => tracing::warn!("stopping recording on quit: {e:#}"),
            }
        }
        if let Some(api) = self.api.take() {
            api.stop();
        }
        *control_flow = ControlFlow::Exit;
    }

    /// Periodic housekeeping: recording timer and level meter in the status
    /// bar, recent list.
    fn tick(&mut self) {
        self.quick_tick();
        let housekeeping = self.last_tick.elapsed() >= Duration::from_millis(900);
        if housekeeping || (self.metering && self.last_meter.elapsed() >= level_meter::INTERVAL) {
            self.update_recording_ui();
        }
        if !housekeeping {
            return;
        }
        self.last_tick = Instant::now();
        if !self.busy {
            // Panels hidden for a recording that ended without a Done
            // (stopped through the HTTP API, say) come back too.
            self.quick_after_action();
        }
        self.refresh_recent();
        if let Some(l) = self.items.login.as_mut() {
            l.refresh();
        }
        notifications::tick();
    }
}

/// `chrono::Local` without naming the type at every call site.
fn chrono_local() -> chrono::Local {
    chrono::Local
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()))
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let config_path = cli.config.clone().unwrap_or_else(Config::default_path);
    let config = Config::load_from(&config_path)?;

    // The event loop must exist before anything touches NSApplication.
    let mut event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.set_activate_ignoring_other_apps(false);
    let proxy = Proxy(Arc::new(Mutex::new(event_loop.create_proxy())));

    let mut engine = Engine::new(config, false)?;
    engine.ui_host = Some(Arc::new(AppUiHost { proxy: proxy.clone() }));
    let engine = Arc::new(engine);
    notifications::init(engine.config.general.system_notifications, cli.smoke_test);

    if !bugle_platform::screen_access_preflight() {
        tracing::warn!("Screen Recording permission not granted; asking");
        bugle_platform::screen_access_request();
    }

    // Global hotkeys, resolved against the menu table. Bad bindings are
    // logged and skipped, never fatal.
    let hotkey_manager = GlobalHotKeyManager::new().context("installing hotkey handler")?;
    let mut hotkeys: HashMap<u32, Command> = HashMap::new();
    let mut menu_keys: HashMap<&'static str, String> = HashMap::new();
    let (pairs, mut hotkey_warnings) = commands::config_pairs(&engine.config.hotkeys);
    let (plan, plan_warnings) = commands::plan(&pairs);
    hotkey_warnings.extend(plan_warnings);
    for b in plan {
        match hotkey_manager.register(b.hotkey) {
            Ok(()) => {
                tracing::info!("hotkey {} → {}", b.combo, b.entry.name);
                hotkeys.insert(b.hotkey.id(), b.entry.command);
                menu_keys.insert(b.entry.name, b.combo);
            }
            Err(e) => hotkey_warnings.push(format!("[hotkeys] could not register {} for {}: {e}", b.combo, b.entry.name)),
        }
    }
    for w in &hotkey_warnings {
        tracing::warn!("{w}");
    }
    {
        let p = proxy.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.state() == HotKeyState::Pressed {
                p.send(AppEvent::HotKey(e.id()));
            }
        }));
    }

    // Menu + status item.
    let items = build_menu(&engine.config, &menu_keys)?;
    let tray = TrayIconBuilder::new()
        .with_id("bugle")
        .with_menu(Box::new(items.menu.clone()))
        .with_icon_templated(icon::status_icon())
        .with_tooltip("Bugle")
        .build()
        .context("creating status item")?;
    {
        let p = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| p.send(AppEvent::Menu(e.id))));
    }

    // Worker.
    let (worker_tx, worker_rx) = mpsc::channel::<Action>();
    {
        let engine = engine.clone();
        let p = proxy.clone();
        std::thread::Builder::new().name("bugle-worker".into()).spawn(move || worker(engine, worker_rx, p))?;
    }

    let mut app = App {
        engine: engine.clone(),
        config_path,
        tray,
        items,
        worker_tx,
        busy: false,
        api: None,
        hotkeys,
        _hotkey_manager: hotkey_manager,
        history_mtime: None,
        proxy: proxy.clone(),
        last_tick: Instant::now() - Duration::from_secs(5),
        metering: false,
        last_meter: Instant::now(),
        recording_shown: None,
        quick: quick_access::QuickAccess::new(engine.config.quick_access.clone()),
        editor_generation: 0,
        video_editor_generation: 0,
    };
    app.refresh_recent();
    app.update_recording_ui();
    if cli.serve {
        app.toggle_api();
    }
    tracing::info!("Bugle is in the menu bar (saving to {})", engine.config.save_dir().display());
    if let Some(first) = hotkey_warnings.first() {
        let n = hotkey_warnings.len();
        app.toast_error(&format!("{n} hotkey binding{} skipped: {first}", if n == 1 { "" } else { "s" }));
    }

    if let Some(source) = cli.annotate.clone() {
        proxy.send(AppEvent::Editor(editor_window::EditorMsg::Open(source)));
    }
    if let Some(source) = cli.edit_video.clone() {
        proxy.send(AppEvent::VideoEditor(video_editor_window::VideoEditorMsg::Open(source)));
    }

    let smoke_started = cli.smoke_test.then(|| smoke_test(engine.clone(), proxy.clone()));

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + app.wake_interval());
        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) | Event::MainEventsCleared => {
                app.tick();
                if let Some(t0) = smoke_started {
                    if t0.elapsed() > Duration::from_millis(3500) && bugle_platform::select_rect_active() && !picker::in_session() {
                        tracing::info!("smoke test: cancelling overlay");
                        bugle_platform::select_rect_cancel();
                    }
                    if t0.elapsed() > Duration::from_secs(12) && bugle_platform::window_pick_active() {
                        tracing::info!("smoke test: cancelling window picker");
                        bugle_platform::window_pick_cancel();
                    }
                }
            }
            Event::UserEvent(e) => match e {
                AppEvent::Menu(id) => app.on_menu(&id, control_flow),
                AppEvent::HotKey(id) => {
                    if let Some(command) = app.hotkeys.get(&id).copied() {
                        app.run_command(command, control_flow);
                    }
                }
                AppEvent::Ui(req) => app.on_ui(req),
                AppEvent::Done(action, result) => app.on_done(action, result),
                AppEvent::ApiStopped(err) => app.on_api_stopped(err),
                AppEvent::QuickAccess(e) => app.on_quick(e),
                AppEvent::Editor(m) => app.on_editor(m),
                AppEvent::VideoEditor(m) => app.on_video_editor(m),
                AppEvent::AudioPrompt(field, text) => app.on_audio_prompt(field, text),
                AppEvent::Quit => app.quit(control_flow),
            },
            Event::LoopDestroyed => {
                if let Some(api) = app.api.take() {
                    api.stop();
                }
            }
            _ => {}
        }
    })
}

/// `--smoke-test`: drive the UI host from a worker thread the way an action
/// would, then quit. The main loop cancels the overlay after a few seconds.
fn smoke_test(engine: Arc<Engine>, proxy: Proxy) -> Instant {
    let t0 = Instant::now();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        proxy.send(AppEvent::Ui(UiRequest::Pin {
            png: Vec::new(),
            frame: None,
            opacity: 1.0,
            reply: mpsc::channel().0,
        }));
        proxy.send(AppEvent::Done(
            Action::CaptureScreen,
            Ok(Some(Notice { title: "Bugle smoke test".into(), subtitle: "toast, overlay, pin".into(), thumb: None, open: None })),
        ));
        std::thread::sleep(Duration::from_millis(400));
        let r = engine.select_area();
        eprintln!("smoke test: select_area → {r:?}");
        match engine.pin("latest", None, 1.0, false) {
            Ok(m) => eprintln!("smoke test: pinned {}", m.path.display()),
            Err(e) => eprintln!("smoke test: pin skipped ({e:#})"),
        }
        // The picker must close itself: give it a short timeout and check
        // that the answer comes back. A click during the test only reports
        // the window, nothing is captured.
        let t = Instant::now();
        let r = picker::pick_window(&proxy, Duration::from_millis(1500));
        eprintln!(
            "smoke test: window picker → {:?} after {:.1}s",
            r.map(|w| w.map(|w| picker::label(&w))),
            t.elapsed().as_secs_f64()
        );
        picker::smoke(&proxy);
        quick_panel::smoke(&proxy);
        editor_window::smoke(&proxy);
        video_editor_window::smoke(&proxy);
        smoke_audio_prompt(&proxy);
        smoke_input::smoke(&proxy);
        std::thread::sleep(Duration::from_millis(1500));
        proxy.send(AppEvent::Quit);
    });
    t0
}

/// `--smoke-test` step: open the *Audio Levels* prompt from its menu item,
/// click the item again (the open prompt comes to the front, no second one
/// opens), and cancel it, so nothing is written to the config.
fn smoke_audio_prompt(proxy: &Proxy) {
    let click = || proxy.send(AppEvent::Menu(MenuId("audio:mic:custom".into())));
    let open = || {
        bugle_platform::app_windows().map(|ws| ws.iter().filter(|w| w.title == "Microphone Level").count()).unwrap_or(0)
    };
    click();
    std::thread::sleep(Duration::from_millis(600));
    click();
    std::thread::sleep(Duration::from_millis(600));
    eprintln!(
        "smoke test: audio prompt windows open: {} (want 1), key window {:?}",
        open(),
        bugle_platform::key_window_title()
    );
    bugle_platform::prompt_answer(None);
    std::thread::sleep(Duration::from_millis(600));
    // No editor is open, so the focus goes back to the app the user was in.
    eprintln!(
        "smoke test: audio prompt windows after cancel: {} (want 0), key window {:?} (want None)",
        open(),
        bugle_platform::key_window_title()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use global_hotkey::hotkey::HotKey;

    #[test]
    fn default_hotkeys_parse() {
        let cfg = Config::default();
        let b = cfg.hotkeys.bindings();
        assert!(b.iter().any(|(n, _)| *n == "capture_area"));
        for (name, s) in &b {
            let hk: HotKey = s.parse().unwrap_or_else(|e| panic!("{name}: {s}: {e}"));
            assert!(commands::find(name).is_some(), "{name} has no menu entry");
            let _ = hk.id();
        }
        let ids: std::collections::HashSet<u32> = b.iter().map(|(_, s)| s.parse::<HotKey>().unwrap().id()).collect();
        assert_eq!(ids.len(), b.len(), "duplicate default hotkeys");
    }

    #[test]
    fn hotkeys_round_trip_through_toml() {
        let toml = "[hotkeys]\ncapture_area = \"cmd+alt+4\"\ncapture_screen = \"\"\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        let b = cfg.hotkeys.bindings();
        assert_eq!(b.iter().find(|(n, _)| *n == "capture_area").map(|(_, s)| s.as_str()), Some("cmd+alt+4"));
        assert!(!b.iter().any(|(n, _)| *n == "capture_screen"), "empty string disables");
        assert!(b.iter().any(|(n, _)| *n == "record"), "unspecified keys keep defaults");
    }
}
