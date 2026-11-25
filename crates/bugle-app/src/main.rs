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
mod meter_smoke;
mod notifications;
mod picker;
mod quick_access;
mod quick_panel;
mod recent_probe;
mod recording_view;
mod smoke_input;
mod smoke_shots;
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
    /// markup editor and the video editor, make a 2 s recording with the level meter (status
    /// menu open), then exit. Exercises the in-app UI plumbing without needing a keyboard.
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
    /// Close every pin window; answers how many were open (`--smoke-test`).
    ClosePins(Sender<usize>),
}

enum AppEvent {
    Menu(MenuId),
    HotKey(u32),
    Ui(UiRequest),
    Done(Action, Result<Option<Notice>, String>),
    ApiStopped(Option<String>),
    /// The engine started or stopped a recording, from whichever thread
    /// (the app's own Agent API included): redraw the recording state now.
    RecordingChanged,
    QuickAccess(quick_access::QuickEvent),
    Editor(editor_window::EditorMsg),
    VideoEditor(video_editor_window::VideoEditorMsg),
    /// The answer to an *Audio Levels* prompt (`None`: cancelled).
    AudioPrompt(audio_menu::Field, Option<String>),
    /// Open the status menu and sample it (`--smoke-test`).
    MeterProbe(meter_smoke::ProbeRequest),
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

    fn recording_changed(&self) {
        self.proxy.send(AppEvent::RecordingChanged);
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
            // A recording that stopped at an API request's max_seconds may
            // still be saving (the menu reads idle): let it finish before
            // the user picks, and start with `record_start_after_save`, so
            // a selection is never thrown away on a busy start.
            engine.wait_for_recording_save();
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
            let status = match engine.record_start_after_save(&req) {
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
    /// The action with the worker is a Record (its `Done` not handled
    /// yet), which first waits for a save in progress: shown as
    /// [`recording_view::Saving::RecordWaits`].
    record_running: bool,
    api: Option<ApiHandle>,
    hotkeys: HashMap<u32, Command>,
    _hotkey_manager: GlobalHotKeyManager,
    history_mtime: Option<SystemTime>,
    /// Checks the files of the Recent rows on show off the main thread, so
    /// one deleted in Finder drops out (and a row's enabled state follows
    /// its file) without a history change, on an external or network volume
    /// too, without a `stat` there blocking the tick.
    recent_probe: recent_probe::RecentProbe,
    proxy: Proxy,
    last_tick: Instant,
    /// Whether the recording on show has a level meter (it then redraws every
    /// [`level_meter::INTERVAL`]), and when it last did.
    metering: bool,
    last_meter: Instant,
    /// What the status item and recording entries show now, so a redraw
    /// changes only what moved.
    recording_shown: Option<recording_view::RecordingView>,
    /// Where Bugle processes sharing this config say they are recording,
    /// and the recordings of other processes found there on the last
    /// housekeeping tick ([`App::scan_other_recordings`]).
    marker_dir: PathBuf,
    other_recordings: Vec<bugle_engine::record::marker::LiveRecording>,
    /// A thread is finalising a recording that stopped by itself
    /// ([`App::reap_auto_stopped`]).
    reaping: Arc<std::sync::atomic::AtomicBool>,
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

    fn dispatch(&mut self, action: Action) {
        // Fresh, so a Record that waits for a save shows so straight away.
        self.update_recording_ui();
        let Some(shown) = self.recording_shown.as_ref() else { return };
        let (action, record) = match recording_view::command(action, shown) {
            recording_view::Step::Send { action, record } => (action, record),
            recording_view::Step::Toast(msg) => {
                tracing::info!("{action:?}: {msg}");
                bugle_platform::toast_show(None, "Bugle", &msg, None, 4.0);
                return;
            }
            recording_view::Step::Ignore => return,
        };
        if self.busy || bugle_platform::select_rect_active() || bugle_platform::window_pick_active() {
            tracing::debug!("ignoring {action:?}: busy");
            return;
        }
        self.quick_before_action(action);
        self.busy = true;
        self.record_running = record;
        if self.worker_tx.send(action).is_err() {
            self.busy = false;
            self.record_running = false;
            self.toast_error("worker thread is gone");
        }
        self.update_recording_ui();
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
            UiRequest::ClosePins(reply) => {
                let _ = reply.send(bugle_platform::pin_close_all());
            }
        }
    }

    fn on_done(&mut self, action: Action, result: Result<Option<Notice>, String>) {
        self.busy = false;
        // Started, failed or cancelled: no Record waits any more.
        self.record_running = false;
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
        let saving = recording_view::Saving::of(self.engine.recording_saving(), self.record_running);
        let shown = recording_view::view(&st, saving, &self.other_recordings, chrono::Utc::now());
        if self.recording_shown.as_ref() != Some(&shown) {
            let before = self.recording_shown.as_ref();
            if before.map(|s| s.own) != Some(shown.own) {
                level_meter::note_shown(shown.own);
            }
            if before.map(|s| s.record_enabled()) != Some(shown.record_enabled()) {
                let enabled = shown.record_enabled();
                self.items.record.set_enabled(enabled);
                self.items.record_gif.set_enabled(enabled);
                self.items.record_window.set_enabled(enabled);
            }
            if before.map(|s| (s.own, s.other, s.saving)) != Some((shown.own, shown.other, shown.saving)) {
                let (label, enabled) = shown.stop_item();
                self.items.stop.set_text(label);
                self.items.stop.set_enabled(enabled);
                OTHER_SHOWN.store(shown.other.unwrap_or(0), std::sync::atomic::Ordering::SeqCst);
            }
            // Monospaced digits while there is a title (a count of this
            // app's recording or another process's, or *Saving…*, where it
            // changes nothing), so the item (and its neighbours) do not
            // shift every second; set before the title, and the button's own
            // font is back before the idle title is set.
            let titled = shown.title.is_some();
            if before.map(|s| s.title.is_some()) != Some(titled) {
                if let Some(item) = self.tray.ns_status_item() {
                    // SAFETY: the status item is alive; this is the main thread.
                    unsafe { bugle_platform::status_item_mono_digits(&*item as *const _ as *mut std::ffi::c_void, titled) };
                }
            }
            self.tray.set_title(shown.title.as_deref());
            self.recording_shown = Some(shown);
        }
        if let Err(e) = self.items.meter.update(&self.items.menu, st.audio_levels.as_ref()) {
            tracing::warn!("level meter: {e:#}");
        }
        self.metering = st.audio_levels.is_some();
        self.last_meter = Instant::now();
    }

    /// A recording started or stopped, possibly behind the app's back (the
    /// app's own Agent API): show it now instead of on the next tick, which
    /// remains a fallback.
    fn on_recording_changed(&mut self) {
        self.update_recording_ui();
        level_meter::note_event(self.recording_shown.as_ref().is_some_and(|s| s.own));
        // A stop the app did not ask for: its panels come back and the
        // recording joins the recent list (a no-op while busy or recording).
        self.quick_after_action();
        self.refresh_recent();
    }

    /// A recording that stopped by itself (an Agent API request's
    /// `max_seconds`, a GIF cap, or its capture stream failing) stays in the
    /// engine until something finalises it: do that off the main thread (an
    /// MP4 export can take a while). The thread then sends
    /// `RecordingChanged` itself (the engine's own come while it still
    /// saves), on a panic too, and a later `record_stop` from the API still
    /// gets the media record (with its `failure`, if capture stopped early).
    fn reap_auto_stopped(&self) {
        use std::sync::atomic::Ordering;
        if self.engine.record_status().state != RecordingState::Finishing || self.reaping.swap(true, Ordering::SeqCst) {
            return;
        }
        /// Clears `reaping` however the reap thread ends (a panic too), and
        /// redraws then: the engine's own announcements come while it is
        /// still saving (`recording_saving`), so *Saving…* clears at once,
        /// written, failed or panicked.
        struct Reaping(Arc<std::sync::atomic::AtomicBool>, Option<Proxy>);
        impl Drop for Reaping {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
                if let Some(proxy) = self.1.take() {
                    proxy.send(AppEvent::RecordingChanged);
                }
            }
        }
        let (engine, reaping) = (self.engine.clone(), Reaping(self.reaping.clone(), Some(self.proxy.clone())));
        let spawned = std::thread::Builder::new().name("bugle-reap".into()).spawn(move || {
            let _reaping = reaping;
            match engine.record_reap() {
                Some(Ok(m)) => match &m.failure {
                    Some(why) => tracing::warn!("recording stopped early ({why}), saved what was captured to {}", m.path.display()),
                    None => tracing::info!("recording stopped by itself, saved to {}", m.path.display()),
                },
                Some(Err(e)) => tracing::warn!("finalising a recording that stopped by itself: {e:#}"),
                None => {}
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("could not start the thread finalising a recording: {e}");
            self.reaping.store(false, Ordering::SeqCst);
        }
    }

    /// Read which recordings other Bugle processes sharing this config are
    /// making (`bugle record`, `bugle serve`, `bugle mcp`: each runs its own
    /// engine, which writes a state file while it records). One directory
    /// listing per housekeeping tick; files of processes that are gone are
    /// removed as they are found.
    fn scan_other_recordings(&mut self) {
        self.other_recordings = bugle_engine::record::marker::scan(&self.marker_dir, std::process::id());
        *OTHER_FOUND.lock().unwrap_or_else(|e| e.into_inner()) = self.other_recordings.iter().map(|r| r.pid).collect();
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
        if mtime == self.history_mtime && !self.items.recent.items().is_empty() && !self.recent_probe.changed() {
            return;
        }
        self.history_mtime = mtime;
        while self.items.recent.remove_at(0).is_some() {}
        let recent: Vec<recent_probe::Row> = self.engine.history.recent(5).unwrap_or_default().into_iter().map(|m| {
            let exists = m.path.exists();
            (m, exists)
        }).collect();
        self.recent_probe.watch(recent.clone());
        if recent.is_empty() {
            let _ = self.items.recent.append(&MenuItem::with_id("recent:none", "No captures yet", false, None));
            return;
        }
        for (m, exists) in recent {
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
            let _ = self.items.recent.append(&MenuItem::with_id(id, text, exists, None));
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
        // A reap in progress has already freed the slot (the status reads
        // idle) but is still writing the file: `record_stop` waits for it
        // rather than exiting mid-export.
        if self.is_recording() || self.reaping.load(std::sync::atomic::Ordering::SeqCst) {
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
        if housekeeping {
            self.scan_other_recordings();
        }
        if housekeeping || (self.metering && self.last_meter.elapsed() >= level_meter::INTERVAL) {
            self.update_recording_ui();
        }
        if !housekeeping {
            return;
        }
        self.last_tick = Instant::now();
        self.reap_auto_stopped();
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
    let mut config = Config::load_from(&config_path)?;
    // The smoke test never reads the user's history (a stale entry there hid
    // a failing step): unless the config names one, it gets an empty file in
    // a folder of its own, which also holds its recording-state folder (so
    // no other run's or process's recordings show), removed at exit.
    let smoke_dir = (cli.smoke_test && config.general.history_path.is_none())
        .then(|| std::env::temp_dir().join(format!("bugle-smoke-{}", std::process::id())));
    if let Some(d) = &smoke_dir {
        let _ = std::fs::remove_dir_all(d);
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        config.general.history_path = Some(d.join("history.jsonl").display().to_string());
    }

    // The event loop must exist before anything touches NSApplication.
    let mut event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.set_activate_ignoring_other_apps(false);
    let proxy = Proxy(Arc::new(Mutex::new(event_loop.create_proxy())));

    let mut engine = Engine::new(config, false)?;
    engine.ui_host = Some(Arc::new(AppUiHost { proxy: proxy.clone() }));
    let engine = Arc::new(engine);
    // Clear what killed exports left in the save folder (exports elsewhere
    // sweep their own folder as they start).
    bugle_platform::staging::sweep_in_background(vec![engine.config.save_dir()]);
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
    if cli.smoke_test {
        smoke_shots::set_status_item(&tray);
    }
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
        record_running: false,
        api: None,
        hotkeys,
        _hotkey_manager: hotkey_manager,
        history_mtime: None,
        recent_probe: recent_probe::RecentProbe::start(recent_probe::INTERVAL),
        proxy: proxy.clone(),
        last_tick: Instant::now() - Duration::from_secs(5),
        metering: false,
        last_meter: Instant::now(),
        recording_shown: None,
        marker_dir: bugle_engine::record::marker::marker_dir(&engine.config),
        other_recordings: Vec::new(),
        reaping: Default::default(),
        quick: quick_access::QuickAccess::new(engine.config.quick_access.clone()),
        editor_generation: 0,
        video_editor_generation: 0,
    };
    app.refresh_recent();
    app.scan_other_recordings();
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
                AppEvent::RecordingChanged => app.on_recording_changed(),
                AppEvent::QuickAccess(e) => app.on_quick(e),
                AppEvent::Editor(m) => app.on_editor(m),
                AppEvent::VideoEditor(m) => app.on_video_editor(m),
                AppEvent::AudioPrompt(field, text) => app.on_audio_prompt(field, text),
                AppEvent::MeterProbe(req) => {
                    app.update_recording_ui();
                    meter_smoke::open_probe(&app.tray, &app.items.menu, req)
                }
                AppEvent::Quit => app.quit(control_flow),
            },
            Event::LoopDestroyed => {
                if let Some(api) = app.api.take() {
                    api.stop();
                }
                if let Some(d) = &smoke_dir {
                    let _ = std::fs::remove_dir_all(d);
                }
                if SMOKE_FAILED.load(std::sync::atomic::Ordering::SeqCst) {
                    eprintln!("smoke test: FAILED");
                    std::process::exit(1);
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
    smoke_shots::init();
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
        smoke_pin(&engine, &proxy);
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
        if !smoke_input::smoke(&proxy) {
            smoke_fail("input", "one or more checks failed, see the lines above");
        }
        smoke_other_recording(&engine, &proxy);
        // Last: it records.
        if let Err(e) = meter_smoke::smoke(&engine, &proxy) {
            smoke_fail("level meter", e);
        }
        std::thread::sleep(Duration::from_millis(1500));
        proxy.send(AppEvent::Quit);
    });
    t0
}

/// For the `--smoke-test` step, written on the main thread: the pid of the
/// other process whose recording the menu shows (0: none), and the pids of
/// all the other recordings the last scan found.
static OTHER_SHOWN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static OTHER_FOUND: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Set by [`smoke_fail`]; the run then exits 1.
static SMOKE_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Report a `--smoke-test` step that failed outright (an unexpected error or
/// a wrong result, not a capability this machine lacks): prints the
/// `FAILED` line and makes the run exit 1 once it has finished.
pub(crate) fn smoke_fail(step: &str, err: impl std::fmt::Display) {
    eprintln!("smoke test: {step} FAILED ({err})");
    SMOKE_FAILED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// `--smoke-test` step: pin a synthetic PNG (a temp file, not added to
/// history, so the step does not depend on what the history holds) through
/// `Engine::pin` and the UI host, check with the window server that one
/// window of the image's size appeared, close it, and check it is gone. Any
/// error fails the run.
fn smoke_pin(engine: &Engine, proxy: &Proxy) {
    const SIZE: (u32, u32) = (237, 151);
    let pins = || -> Result<usize> {
        Ok(bugle_platform::windows(true)?
            .iter()
            .filter(|w| w.pid == std::process::id() as i32 && w.frame.w == SIZE.0 as f64 && w.frame.h == SIZE.1 as f64)
            .count())
    };
    // Poll until `want` such windows are on screen, for at most 2 s.
    let wait_for = |want: usize| -> Result<usize> {
        let t = Instant::now();
        loop {
            let n = pins()?;
            if n == want || t.elapsed() > Duration::from_secs(2) {
                return Ok(n);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let run = || -> Result<()> {
        let png = tempfile::Builder::new().prefix("bugle-smoke-pin-").suffix(".png").tempfile()?;
        image::RgbaImage::from_pixel(SIZE.0, SIZE.1, image::Rgba([230, 140, 30, 255])).save(png.path())?;
        let before = pins()?;
        engine.pin(&png.path().display().to_string(), None, 1.0, false)?;
        let shown = wait_for(1)?;
        let (tx, rx) = mpsc::channel();
        proxy.send(AppEvent::Ui(UiRequest::ClosePins(tx)));
        let closed = rx.recv_timeout(Duration::from_secs(2)).context("closing the pins timed out")?;
        let after = wait_for(0)?;
        eprintln!(
            "smoke test: pin {}×{} windows before {before}, shown {shown} (want 1), closed {closed} (want 1), after {after} (want 0)",
            SIZE.0, SIZE.1
        );
        if (before, shown, closed, after) != (0, 1, 1, 0) {
            anyhow::bail!("unexpected pin windows");
        }
        Ok(())
    };
    if let Err(e) = run() {
        smoke_fail("pin", format!("{e:#}"));
    }
}

/// `--smoke-test` step: a recording another Bugle process makes shows in
/// the menu bar. A stand-in process (`sleep`) gets a recording-state file as
/// an engine recording in it would write; within 3 s the app must find it
/// and show another process's recording (the oldest one: with a configured
/// `history_path` a real one may be older). While it shows, the open status
/// menu must name the process on a disabled Stop entry, and *Stop
/// Recording* (as its hotkey would) must show a toast saying where to stop
/// it ([`smoke_other_recording_ui`]). Once the process is killed without
/// removing its file, the app must drop it within 3 s, with the stale file
/// removed (and the folder, if the step created it).
fn smoke_other_recording(engine: &Engine, proxy: &Proxy) {
    use bugle_engine::record::marker;
    let wait_for = |pid: u32, found: bool| -> Option<Duration> {
        let t = Instant::now();
        loop {
            let is_found = OTHER_FOUND.lock().unwrap_or_else(|e| e.into_inner()).contains(&pid);
            let shown = OTHER_SHOWN.load(std::sync::atomic::Ordering::SeqCst) != 0;
            if is_found == found && (!found || shown) {
                return Some(t.elapsed());
            }
            if t.elapsed() > Duration::from_secs(3) {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let run = || -> Result<()> {
        let dir = marker::marker_dir(&engine.config);
        let dir_existed = dir.exists();
        let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn()?;
        let pid = child.id();
        let rec = marker::LiveRecording {
            pid,
            process_started_us: marker::process_start(pid).micros().unwrap_or(0),
            started_at: chrono::Utc::now() - chrono::Duration::seconds(61),
            format: RecordingFormat::Mp4,
            output: std::env::temp_dir().join("bugle-smoke-other-process.mp4"),
            max_seconds: None,
        };
        let published = marker::publish(&dir, &rec);
        let shown = published.is_ok().then(|| wait_for(pid, true)).flatten();
        let ui = shown.map(|_| smoke_other_recording_ui(proxy, OTHER_SHOWN.load(std::sync::atomic::Ordering::SeqCst)));
        let _ = child.kill();
        let _ = child.wait();
        let cleared = wait_for(pid, false);
        let left = dir.join(format!("{pid}.json")).exists();
        let _ = marker::clear(&dir, pid);
        if !dir_existed {
            // Only if empty: a Bugle process may have started recording.
            let _ = std::fs::remove_dir(&dir);
        }
        published?;
        eprintln!(
            "smoke test: another process's recording shown after {:?} (want some), cleared after its process died after {:?} (want some), stale file left: {left} (want false)",
            shown, cleared
        );
        if shown.is_none() || cleared.is_none() || left {
            anyhow::bail!("the menu did not follow the other process's recording");
        }
        ui.unwrap_or(Ok(()))
    };
    if let Err(e) = run() {
        smoke_fail("other process recording", format!("{e:#}"));
    }
}

/// [`smoke_other_recording`] while the recording of process `pid` shows:
/// open the status menu and read its Stop entry (label and enabled state),
/// then choose *Stop Recording* and check that a toast appears. With
/// screenshots on ([`smoke_shots`]), a picture of each.
fn smoke_other_recording_ui(proxy: &Proxy, pid: u32) -> Result<()> {
    if smoke_shots::enabled() {
        // The title was set just now; let the menu bar draw it.
        std::thread::sleep(Duration::from_millis(400));
        smoke_shots::shoot("other-recording-timer");
    }
    // Opened again once if something closed it before it was read, as the
    // level-meter step does.
    let mut json = String::new();
    for attempt in 1..=2 {
        let shot = smoke_shots::shoot_open_menu_soon("other-recording-menu");
        let report = meter_smoke::ask_probe(proxy).recv_timeout(Duration::from_secs(6));
        if let Some(s) = shot {
            let _ = s.join();
        }
        json = match report {
            Ok(Ok(json)) => json,
            Ok(Err(e)) => anyhow::bail!("status menu probe: {e}"),
            Err(_) => anyhow::bail!("no report from the status menu probe"),
        };
        match meter_smoke::closed_early(&json) {
            Some(after) if attempt == 1 => eprintln!(
                "smoke test: other process recording: the status menu was closed from outside after {after:.2}s, before it was read; opening it again"
            ),
            _ => break,
        }
    }
    let v: serde_json::Value = serde_json::from_str(&json).context("status menu probe report")?;
    if v["opened"] != serde_json::Value::Bool(true) {
        anyhow::bail!("the status menu did not open");
    }
    let s = &v["sample"];
    if s.is_null() {
        anyhow::bail!("the status menu closed before it was read, twice");
    }
    let want = recording_view::other_process_label(pid);
    let items = s["items"].as_array().cloned().unwrap_or_default();
    let at = items.iter().position(|i| i.as_str() == Some(want.as_str()));
    let enabled = at.and_then(|i| s["enabled"][i].as_bool());
    eprintln!(
        "smoke test: other process recording: status item title {:?}, menu entry {want:?} at {at:?} (want some), enabled {enabled:?} (want Some(false))",
        s["title"].as_str().unwrap_or("")
    );
    if at.is_none() || enabled != Some(false) {
        anyhow::bail!("the open menu does not show {want:?} as a disabled entry");
    }

    let before = smoke_shots::toast_ids();
    proxy.send(AppEvent::Menu(MenuId("stop_recording".into())));
    let t = Instant::now();
    let mut appeared = None;
    while t.elapsed() < Duration::from_secs(2) {
        if let Some(&id) = smoke_shots::toast_ids().difference(&before).next() {
            appeared = Some((t.elapsed(), id));
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!(
        "smoke test: other process recording: Stop Recording showed a toast after {:?} (want some)",
        appeared.map(|(after, _)| after)
    );
    let Some((_, toast)) = appeared else {
        anyhow::bail!("Stop Recording while another process records showed no toast");
    };
    if smoke_shots::enabled() {
        // Let the window server draw it.
        std::thread::sleep(Duration::from_millis(500));
        smoke_shots::shoot("other-recording-stop-toast");
        // Gone (4 s) before the next step, so it is not in that step's shots.
        let t = Instant::now();
        while smoke_shots::toast_ids().contains(&toast) && t.elapsed() < Duration::from_secs(6) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(())
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
