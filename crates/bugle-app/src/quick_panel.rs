//! Windowing glue for the quick-access panels: shows an AppKit panel for each
//! finished capture (stacking them when captures come in a row), turns their
//! clicks into [`QuickEvent`]s on the main loop, runs the chosen action
//! against the engine and closes each panel when it times out. The decisions
//! live in `quick_access.rs`.

use crate::quick_access::{self, Backend, Feedback, PanelAction, QuickEvent, SyncState, Target};
use crate::{file_name, Action, App, AppEvent, Notice, Proxy};
use anyhow::Result;
use bugle_engine::Engine;
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Long edge of the sharper thumbnail rendered after the panel is up
/// (the panel shows it at 128 pt, so 2× on Retina).
const PANEL_THUMB_EDGE: u32 = 256;

/// The engine and the platform layer behind the panel buttons.
struct EngineBackend<'a> {
    engine: &'a Engine,
}

impl Backend for EngineBackend<'_> {
    fn copy(&self, source: &str) -> Result<()> {
        self.engine.copy_media(source).map(|_| ())
    }
    /// *Annotate* with an external app (`annotate_app` is not `builtin`).
    fn annotate(&self, path: &Path) -> Result<()> {
        let app = self.engine.config.quick_access.annotate_app.trim();
        let opened = bugle_platform::open_with(path, (!app.is_empty()).then_some(app))?;
        if opened == bugle_platform::Opened::FellBack {
            tracing::warn!(
                "[quick_access] annotate_app {app:?} is not an installed application's bundle id \
                 (e.g. \"com.apple.Preview\"); opened {} with the default app",
                file_name(path)
            );
        }
        Ok(())
    }
    fn pin(&self, source: &str) -> Result<()> {
        self.engine.pin(source, None, 1.0, false).map(|_| ())
    }
    fn sync(&self, source: &str) -> Result<(String, String)> {
        self.engine.sync_media(source).map(|(_, r)| (r.action, r.destination))
    }
    fn reveal(&self, path: &Path) -> Result<()> {
        bugle_platform::reveal_path(path)
    }
    fn open(&self, path: &Path) -> Result<()> {
        bugle_platform::open_with(path, None).map(|_| ())
    }
}

impl App {
    /// Before an action that grabs the screen goes to the worker: take the
    /// panels (and any still fading out) off screen at once, so the capture
    /// or recording cannot contain them. They stay up otherwise and come back
    /// when the action is done ([`App::quick_after_action`]), with the new
    /// capture's panel in front. Actions that only act on an existing item
    /// leave them alone. The window server drops the windows before the grab:
    /// screen and window captures wait 150 ms first, area captures and
    /// recordings wait for the selection overlay.
    pub(crate) fn quick_before_action(&mut self, action: Action) {
        if matches!(action, Action::OcrLatest | Action::PinLatest | Action::SyncLatest) {
            return;
        }
        if self.quick.is_open() {
            tracing::debug!("quick access: hiding {} panel(s) during {action:?}", self.quick.count());
        }
        self.quick.hide(Instant::now());
        bugle_platform::panel_hide_all();
    }

    /// An action finished: bring back the panels hidden for it, unless a
    /// recording is still running (it would record them) or another capture
    /// is in flight. Their timeouts pick up where they stopped.
    pub(crate) fn quick_after_action(&mut self) {
        if self.is_recording() || self.busy {
            return;
        }
        self.quick.show(Instant::now());
        // A no-op when nothing is hidden.
        bugle_platform::panel_show_all();
    }

    /// Show the panel for a finished action. `false` means the caller should
    /// fall back to the plain toast (panel disabled, nothing saved, or an
    /// action that only reports on an existing item).
    pub(crate) fn quick_show(&mut self, action: Action, n: &Notice) -> bool {
        if matches!(action, Action::OcrLatest | Action::PinLatest | Action::SyncLatest) {
            return false;
        }
        self.quick_show_notice(n)
    }

    /// Show the panel for a saved file, whatever produced it. `false` when
    /// the panel is disabled or there is nothing to act on.
    pub(crate) fn quick_show_notice(&mut self, n: &Notice) -> bool {
        if !self.quick.wants_panel(n.thumb.is_some(), n.open.as_deref()) {
            return false;
        }
        let Some(path) = n.open.as_deref() else { return false };
        let recent = self.engine.history.recent(20).unwrap_or_default();
        let target = quick_access::resolve_target(&recent, path);
        // The kind's own destination (`[sync.destinations.<kind>]`), as `sync` uses.
        let dest = self.engine.config.sync.destination_for(target.kind).ok().flatten();
        let sync = SyncState { destination: dest.is_some(), synced: quick_access::already_synced(&target, dest.as_ref()) };
        let actions = quick_access::actions_for(target.kind, sync);
        let labels: Vec<&str> = actions.iter().map(|a| a.label()).collect();
        let (generation, evicted) = self.quick.open(target.clone(), actions, Instant::now());
        for g in evicted {
            // Pushed out of a full stack.
            bugle_platform::panel_dismiss(g, true);
        }
        let proxy = self.proxy.clone();
        let shown = bugle_platform::panel_show(
            generation,
            n.thumb.as_deref(),
            &n.title,
            &n.subtitle,
            &labels,
            quick_access::corner(self.quick.config.position),
            move |choice| proxy.send(AppEvent::QuickAccess(QuickEvent::Choice { generation, choice })),
        );
        if let Err(e) = shown {
            // The other panels stay up, each with its own timeout.
            tracing::warn!("quick access: {e:#}");
            self.quick.forget(generation);
            return false;
        }
        // While the stack is hidden the new panel joins it off screen; it
        // comes back with the others once nothing is grabbing the screen.
        self.quick_after_action();
        tracing::info!("quick access: panel {generation} for {} with {labels:?}", file_name(&target.path));
        spawn_sharper_thumb(self.proxy.clone(), generation, target);
        true
    }

    pub(crate) fn on_quick(&mut self, e: QuickEvent) {
        match e {
            QuickEvent::Choice { generation, choice } => {
                let Some((action, target)) = self.quick.on_choice(generation, choice) else { return };
                // A real click already closed the panel; this covers choices
                // that did not come from the panel itself.
                bugle_platform::panel_dismiss(generation, true);
                tracing::info!(
                    "quick access: {} {} (panel {generation}; {} still up)",
                    action.label(),
                    file_name(&target.path),
                    self.quick.count()
                );
                self.quick_run(action, target);
            }
            QuickEvent::Thumb { generation, png } => {
                if self.quick.is_current(generation) {
                    bugle_platform::panel_set_image(generation, &png);
                }
            }
            QuickEvent::Finished { action, result } => match result {
                Ok(Some(Feedback { title, subtitle })) => bugle_platform::toast_show(None, &title, &subtitle, None, 2.5),
                Ok(None) => {}
                Err(e) => self.toast_error(&format!("{}: {e}", action.label())),
            },
            QuickEvent::Count(tx) => {
                let _ = tx.send((self.quick.count(), bugle_platform::panel_count()));
            }
        }
    }

    fn quick_run(&mut self, action: PanelAction, target: Target) {
        if action == PanelAction::Annotate && self.engine.config.quick_access.builtin_editor() {
            // The editor is a window of this app: open it here, on the main thread.
            return self.open_editor(&target.source);
        }
        if action == PanelAction::Edit {
            // The video editor is a window of this app too.
            return self.open_video_editor(&target.source);
        }
        if !action.runs_off_main() {
            let result = quick_access::perform(&EngineBackend { engine: &self.engine }, action, &target);
            self.on_quick(QuickEvent::Finished { action, result: result.map_err(|e| format!("{e:#}")) });
            return;
        }
        let engine: Arc<Engine> = self.engine.clone();
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new().name("bugle-quick-access".into()).spawn(move || {
            let result = quick_access::perform(&EngineBackend { engine: &engine }, action, &target);
            proxy.send(AppEvent::QuickAccess(QuickEvent::Finished { action, result: result.map_err(|e| format!("{e:#}")) }));
        });
        if let Err(e) = spawned {
            self.toast_error(&format!("{}: {e}", action.label()));
        }
    }

    /// Close the panels that timed out. Called from the main loop's tick.
    pub(crate) fn quick_tick(&mut self) {
        if !self.quick.is_open() {
            return;
        }
        for g in self.quick.tick(Instant::now(), bugle_platform::panel_hovered()) {
            bugle_platform::panel_dismiss(g, true);
        }
    }
}

/// Render a sharper thumbnail than the toast's off the main thread and hand
/// it to the panel if it is still up.
fn spawn_sharper_thumb(proxy: Proxy, generation: u64, target: Target) {
    let _ = std::thread::Builder::new().name("bugle-thumb".into()).spawn(move || {
        let img = if target.kind == bugle_core::MediaKind::Video {
            bugle_platform::video_frame(&target.path, 0.0).ok()
        } else {
            image::open(&target.path).ok().map(|i| i.to_rgba8())
        };
        if let Some(png) = img.and_then(|i| bugle_engine::encode_png_bounded(&i, PANEL_THUMB_EDGE).ok()) {
            proxy.send(AppEvent::QuickAccess(QuickEvent::Thumb { generation, png }));
        }
    });
}

/// `--smoke-test` step: show panels for two synthetic captures in a row (a
/// gradient PNG in the temp directory, not added to history), check that
/// they stack, then close them by injecting the `Choice` events the panels'
/// click callback would send (no real click: the shim's click path is not
/// exercised here). The first panel of the process is generation 1. The PNG
/// is removed afterwards.
pub(crate) fn smoke(proxy: &Proxy) {
    let img = image::RgbaImage::from_fn(640, 400, |x, y| image::Rgba([(x / 3) as u8, (y / 2) as u8, 180, 255]));
    let path = std::env::temp_dir().join("bugle-smoke-quick-access.png");
    if let Err(e) = img.save(&path) {
        eprintln!("smoke test: quick access skipped ({e})");
        return;
    }
    let thumb = bugle_engine::encode_png_bounded(&img, 112).ok();
    for title in ["Quick access smoke test", "Quick access smoke test (stacked)"] {
        proxy.send(AppEvent::Done(
            Action::CaptureScreen,
            Ok(Some(Notice {
                title: title.into(),
                subtitle: "bugle-smoke-quick-access.png".into(),
                thumb: thumb.clone(),
                open: Some(path.clone()),
            })),
        ));
        std::thread::sleep(Duration::from_millis(400));
    }
    let count = || {
        let (tx, rx) = mpsc::channel();
        proxy.send(AppEvent::QuickAccess(QuickEvent::Count(tx)));
        rx.recv_timeout(Duration::from_secs(2)).ok()
    };
    std::thread::sleep(Duration::from_millis(1100));
    eprintln!("smoke test: quick-access panels (state, on screen) {:?} (want Some((2, 2)))", count());
    for generation in [1, 2] {
        proxy.send(AppEvent::QuickAccess(QuickEvent::Choice { generation, choice: bugle_platform::PanelChoice::Close }));
        std::thread::sleep(Duration::from_millis(300));
        eprintln!("smoke test: quick-access panels after closing {generation}: {:?}", count());
    }
    if let Err(e) = std::fs::remove_file(&path) {
        eprintln!("smoke test: could not remove {} ({e})", path.display());
    }
}
