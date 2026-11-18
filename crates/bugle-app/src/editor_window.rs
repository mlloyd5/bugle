//! Windowing glue for the markup editor: opens the AppKit window for an
//! image, feeds its events to [`Editor`], re-renders the preview with the
//! engine's renderer on a preview thread, and saves or copies the result off
//! the main thread. The decisions live in `markup_editor.rs`.

use crate::markup_editor::{Editor, Effects, Request, Tool, CANVAS_BACKGROUND_ITEM, MENUS, MENU_CANVAS, MENU_STYLE, PALETTE, SIZES};
use crate::{file_name, saved_subtitle, App, AppEvent, Notice, Proxy};
use bugle_core::Point;
use bugle_engine::Engine;
use bugle_markup::Annotation;
use bugle_platform::{EditorCommand, EditorEvent, EditorId, EditorMenu, EditorTool, EditorZoom};
use image::RgbaImage;
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

/// Editor traffic on the main loop.
pub(crate) enum EditorMsg {
    /// Open the editor for a media id, `latest` or a file path.
    Open(String),
    /// A save finished. On success the window `generation` closes.
    Saved { generation: u64, result: Result<Notice, String> },
    Copied(Result<(), String>),
    /// A preview finished rendering on the preview thread: the image and
    /// where the edited image's pixel (0, 0) is in it.
    Preview { generation: u64, seq: u64, result: Result<(RgbaImage, Point), String> },
    /// Smoke test: deliver an event to the newest editor as if its window
    /// had reported it.
    Inject(EditorEvent),
    /// Smoke test: zoom the newest editor as the keyboard shortcuts do.
    Zoom(EditorZoom),
    /// Smoke test: close the newest editor without saving.
    Close,
    /// Smoke test: how many editor windows are open.
    Count(mpsc::Sender<usize>),
}

/// The open editors by generation, oldest first. Each window's handler owns
/// its session; the registry only points at it, so a closed window's session
/// drops out by itself.
struct Registry<T> {
    entries: Vec<(u64, EditorId, Weak<T>)>,
}

impl<T> Registry<T> {
    const fn new() -> Self {
        Self { entries: Vec::new() }
    }

    fn insert(&mut self, generation: u64, window: EditorId, session: &Rc<T>) {
        self.entries.retain(|(_, _, s)| s.strong_count() > 0);
        self.entries.push((generation, window, Rc::downgrade(session)));
    }

    /// The window and session of editor `generation`, while it lives.
    fn get(&self, generation: u64) -> Option<(EditorId, Rc<T>)> {
        let (_, window, s) = self.entries.iter().find(|(g, _, _)| *g == generation)?;
        Some((*window, s.upgrade()?))
    }

    /// The live editors, newest first.
    fn newest_first(&self) -> impl Iterator<Item = (EditorId, Rc<T>)> + '_ {
        self.entries.iter().rev().filter_map(|(_, w, s)| Some((*w, s.upgrade()?)))
    }
}

thread_local! {
    /// The open windows' sessions, for results arriving on the loop.
    static SESSIONS: RefCell<Registry<RefCell<Session>>> = const { RefCell::new(Registry::new()) };
}

/// The window and session of editor `generation`, while it is alive.
fn session(generation: u64) -> Option<(EditorId, Rc<RefCell<Session>>)> {
    SESSIONS.with(|r| r.borrow().get(generation))
}

/// The newest editor window that is still open.
fn newest_window() -> Option<EditorId> {
    SESSIONS.with(|r| r.borrow().newest_first().map(|(w, _)| w).find(|w| bugle_platform::editor_is_open(*w)))
}

/// The open window already editing `source`, if any.
fn open_window_for(source: &str) -> Option<EditorId> {
    SESSIONS.with(|r| {
        r.borrow()
            .newest_first()
            .filter(|(w, _)| bugle_platform::editor_is_open(*w))
            .find(|(_, s)| s.try_borrow().is_ok_and(|s| s.source == source))
            .map(|(w, _)| w)
    })
}

/// Smoke test: the shapes of the newest editor's document. Main thread only.
pub(crate) fn smoke_shapes() -> Option<Vec<bugle_markup::Shape>> {
    SESSIONS.with(|r| r.borrow().newest_first().next().and_then(|(_, s)| s.try_borrow().ok().map(|s| s.editor.final_document().shapes)))
}

/// One preview to render: its sequence number and the document.
type PreviewJob = (u64, Annotation);

/// Run a render (or save) and turn a panic into an error, so a renderer bug
/// can neither take down a worker thread that the window waits on nor
/// unwind through the AppKit callback on the main thread.
pub(crate) fn guarded<T>(what: &str, f: impl FnOnce() -> anyhow::Result<T>) -> Result<T, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r.map_err(|e| format!("{e:#}")),
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            tracing::error!("editor: {what} panicked: {msg}");
            Err(format!("{what} failed unexpectedly ({msg})"))
        }
    }
}

/// Tell the user about a refused change or a failed preview.
fn toast_problem(msg: &str) {
    tracing::warn!("editor: {msg}");
    bugle_platform::toast_show(None, "Annotate", msg, None, 4.0);
}

/// Render previews on a thread of their own, so a very large capture never
/// stalls the main loop. Only the newest waiting job is rendered; the thread
/// ends when the session (holding the sender) is dropped.
fn spawn_preview(engine: Arc<Engine>, base: Arc<RgbaImage>, proxy: Proxy, generation: u64) -> Option<mpsc::Sender<PreviewJob>> {
    let (tx, rx) = mpsc::channel::<PreviewJob>();
    let spawned = std::thread::Builder::new().name("bugle-editor-preview".into()).spawn(move || {
        while let Ok(mut job) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                job = newer;
            }
            let (seq, doc) = job;
            let t = std::time::Instant::now();
            // Every job answers, error or not: the window waits for it.
            let result = guarded("preview", || engine.render_annotations(&base, &doc)).map(|img| (img, doc.source_origin()));
            tracing::debug!("editor: preview {seq} rendered in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
            proxy.send(AppEvent::Editor(EditorMsg::Preview { generation, seq, result }));
        }
    });
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            tracing::warn!("editor: no preview thread ({e}); rendering on the main thread");
            None
        }
    }
}

/// One open editor window. Lives in the window's event handler and is
/// dropped when the window closes.
struct Session {
    /// The window, once it is open.
    window: EditorId,
    editor: Editor,
    base: Arc<RgbaImage>,
    engine: Arc<Engine>,
    proxy: Proxy,
    /// What `Engine::apply_annotations` saves from.
    source: String,
    generation: u64,
    /// A save is running; input is ignored until it reports back.
    saving: Arc<AtomicBool>,
    preview: Option<mpsc::Sender<PreviewJob>>,
    /// The newest preview asked for; results for older ones are dropped.
    seq: u64,
    /// A preview is being rendered.
    pending: bool,
    /// The overlay waits for the pending preview: the edit that started it
    /// moved something from the overlay into the image (a shape released or
    /// finished, the crop tool left), so showing the new overlay over the old
    /// image would make it vanish for a moment. The next pointer input lets
    /// go, so a new drag is never held up.
    hold_overlay: bool,
    overlay_stale: bool,
    /// The last preview error toasted, so a failing render is reported once.
    last_error: Option<String>,
    /// The menus as last sent to the window.
    menus: Vec<EditorMenu>,
}

impl Session {
    fn on_event(&mut self, event: EditorEvent) {
        if self.saving.load(Ordering::SeqCst) {
            return;
        }
        let pointer = matches!(event, EditorEvent::MouseDown { .. } | EditorEvent::MouseDrag { .. });
        let fx = self.editor.handle(event);
        self.apply(fx, pointer);
    }

    /// `pointer`: the effects answer a mouse-down or drag, whose feedback
    /// must show at once.
    fn apply(&mut self, fx: Effects, pointer: bool) {
        if fx.render {
            self.render();
            self.hold_overlay = self.pending && !pointer;
        }
        if pointer {
            self.hold_overlay = false;
        }
        if let Some(e) = &fx.error {
            toast_problem(e);
        }
        if fx.render || fx.overlay {
            self.overlay_stale = true;
            self.flush_overlay();
        }
        if fx.render || fx.state {
            bugle_platform::editor_set_state(self.window, self.editor.state());
            self.sync_menus();
        }
        if let Some(t) = fx.begin_text {
            bugle_platform::editor_begin_text(self.window, t.at, t.font_px, &t.initial, t.color);
        }
        match fx.request {
            Some(Request::Save) => self.save(),
            Some(Request::Copy) => self.copy(),
            None => {}
        }
    }

    fn flush_overlay(&mut self) {
        if self.overlay_stale && !self.hold_overlay {
            self.overlay_stale = false;
            bugle_platform::editor_set_overlay(self.window, &self.editor.overlay());
        }
    }

    /// Send the menus that changed.
    fn sync_menus(&mut self) {
        let menus = self.editor.menus();
        for (i, m) in menus.iter().enumerate() {
            if self.menus.get(i) != Some(m) {
                bugle_platform::editor_set_menu(self.window, i, m);
            }
        }
        self.menus = menus.to_vec();
    }

    /// Show the image with the finished shapes, crop and canvas: at once
    /// when there is nothing to draw, otherwise from the preview thread.
    fn render(&mut self) {
        let doc = self.editor.document();
        self.seq += 1;
        if doc == Annotation::default() {
            self.pending = false;
            return bugle_platform::editor_set_image(self.window, &self.base, Point::default());
        }
        if let Some(tx) = &self.preview {
            if tx.send((self.seq, doc.clone())).is_ok() {
                self.pending = true;
                return;
            }
            self.preview = None;
        }
        // No preview thread (or it is gone): render here.
        self.pending = false;
        self.hold_overlay = false;
        let result = guarded("preview", || self.engine.render_annotations(&self.base, &doc)).map(|img| (img, doc.source_origin()));
        self.show_preview(result);
    }

    fn on_preview(&mut self, seq: u64, result: Result<(RgbaImage, Point), String>) {
        if seq != self.seq {
            // A newer one is on its way.
            return;
        }
        self.pending = false;
        self.hold_overlay = false;
        self.show_preview(result);
        self.flush_overlay();
    }

    fn show_preview(&mut self, result: Result<(RgbaImage, Point), String>) {
        match result {
            Ok((img, origin)) => {
                self.last_error = None;
                bugle_platform::editor_set_image(self.window, &img, origin);
            }
            Err(e) => {
                if self.last_error.as_ref() != Some(&e) {
                    toast_problem(&format!("the preview could not be drawn: {e}"));
                }
                self.last_error = Some(e);
            }
        }
    }

    /// Save an annotated copy as a new history item. The window stays up
    /// (ignoring input) until the save succeeded, so a failure loses nothing.
    fn save(&mut self) {
        if self.editor.is_empty() {
            // Nothing drawn, cropped or framed: there is no copy worth writing.
            return bugle_platform::editor_close(self.window);
        }
        let doc = self.editor.final_document();
        let (engine, proxy, source, generation) = (self.engine.clone(), self.proxy.clone(), self.source.clone(), self.generation);
        let saving = self.saving.clone();
        saving.store(true, Ordering::SeqCst);
        // No closing half-way: the copy is being written either way.
        bugle_platform::editor_set_busy(self.window, true);
        let flag = saving.clone();
        let spawned = std::thread::Builder::new().name("bugle-editor-save".into()).spawn(move || {
            // Always answers, so the window never stays busy.
            let result = guarded("save", || engine.apply_annotations(&source, doc, None, false)).map(|(item, img)| Notice {
                title: "Annotated copy saved".into(),
                subtitle: saved_subtitle(&engine, &item),
                thumb: bugle_engine::encode_png_bounded(&img, 112).ok(),
                open: Some(item.path.clone()),
            });
            if result.is_err() {
                flag.store(false, Ordering::SeqCst);
            }
            proxy.send(AppEvent::Editor(EditorMsg::Saved { generation, result }));
        });
        if let Err(e) = spawned {
            saving.store(false, Ordering::SeqCst);
            bugle_platform::editor_set_busy(self.window, false);
            self.proxy.send(AppEvent::Editor(EditorMsg::Saved { generation, result: Err(e.to_string()) }));
        }
    }

    /// Put the annotated image on the clipboard; the window stays open.
    fn copy(&self) {
        let doc = self.editor.final_document();
        let (engine, proxy, base) = (self.engine.clone(), self.proxy.clone(), self.base.clone());
        let spawned = std::thread::Builder::new().name("bugle-editor-copy".into()).spawn(move || {
            let result = guarded("copy", || {
                let rendered;
                let img = if doc == Annotation::default() {
                    &*base
                } else {
                    rendered = engine.render_annotations(&base, &doc)?;
                    &rendered
                };
                let mut png = Vec::new();
                img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
                bugle_platform::clipboard_write_png(&png)
            });
            proxy.send(AppEvent::Editor(EditorMsg::Copied(result)));
        });
        if let Err(e) = spawned {
            self.proxy.send(AppEvent::Editor(EditorMsg::Copied(Err(e.to_string()))));
        }
    }
}

impl App {
    /// The most recent image in the history, for *Annotate Latest*.
    pub(crate) fn annotate_latest(&mut self) {
        let recent = self.engine.history.recent(50).unwrap_or_default();
        match recent.iter().find(|m| m.is_image() && m.path.exists()) {
            Some(m) => self.open_editor(&m.id.clone()),
            None => self.toast_error("Annotate: no screenshot in the history yet"),
        }
    }

    /// Open the markup editor for a media id, `latest` or an image path, in
    /// a window of its own. Several editors can be open at once; one already
    /// open on the same capture comes forward instead of a second one.
    pub(crate) fn open_editor(&mut self, source: &str) {
        let item = match self.engine.resolve_media(source) {
            Ok(v) => v,
            Err(e) => return self.toast_error(&format!("Annotate: {e:#}")),
        };
        // Pin down what is on screen: `latest` may be something else by the
        // time the copy is saved.
        let source = match self.engine.history.get(&item.id) {
            Ok(Some(_)) => item.id.clone(),
            _ => item.path.display().to_string(),
        };
        // Check before decoding: bringing an open editor forward needs no pixels.
        if let Some(window) = open_window_for(&source) {
            tracing::info!("editor: {} is already open", file_name(&item.path));
            return bugle_platform::editor_focus(window);
        }
        let (item, base) = match self.engine.load_image(&source) {
            Ok(v) => v,
            Err(e) => return self.toast_error(&format!("Annotate: {e:#}")),
        };
        self.editor_generation += 1;
        let generation = self.editor_generation;
        let size = base.dimensions();
        let base = Arc::new(base);
        let session = Rc::new(RefCell::new(Session {
            window: EditorId::NONE,
            editor: Editor::new(size),
            preview: spawn_preview(self.engine.clone(), base.clone(), self.proxy.clone(), generation),
            base,
            engine: self.engine.clone(),
            proxy: self.proxy.clone(),
            source,
            generation,
            saving: Arc::new(AtomicBool::new(false)),
            seq: 0,
            pending: false,
            hold_overlay: false,
            overlay_stale: false,
            last_error: None,
            menus: Vec::new(),
        }));
        let tools: Vec<EditorTool> = Tool::ALL.iter().map(|t| t.button()).collect();
        let handler = session.clone();
        let opened = bugle_platform::editor_open(
            &format!("Annotate — {}", file_name(&item.path)),
            size,
            &tools,
            &PALETTE,
            &SIZES,
            MENUS,
            move |event| match handler.try_borrow_mut() {
                Ok(mut s) => s.on_event(event),
                Err(_) => tracing::warn!("editor: dropped re-entrant event {event:?}"),
            },
        );
        let window = match opened {
            Ok(w) => w,
            Err(e) => return self.toast_error(&format!("Annotate: {e:#}")),
        };
        // The window reports nothing before this turn ends (its first zoom
        // report is queued), so the session knows its id before any event.
        session.borrow_mut().window = window;
        SESSIONS.with(|r| r.borrow_mut().insert(generation, window, &session));
        tracing::info!("editor: opened {} ({}×{}) as {window:?}", file_name(&item.path), size.0, size.1);
        // First paint: the image, an empty overlay, the toolbar and menus.
        session.borrow_mut().apply(Effects { render: true, ..Default::default() }, false);
    }

    pub(crate) fn on_editor(&mut self, msg: EditorMsg) {
        match msg {
            EditorMsg::Open(source) => self.open_editor(&source),
            EditorMsg::Saved { generation, result: Ok(n) } => {
                if let Some((window, _)) = session(generation) {
                    bugle_platform::editor_close(window);
                }
                tracing::info!("editor: saved {}", n.subtitle);
                if !self.quick_show_notice(&n) {
                    self.toast(&n);
                }
                self.refresh_recent();
            }
            EditorMsg::Saved { generation, result: Err(e) } => {
                // The window is still up with the work in it; let it go on.
                if let Some((window, _)) = session(generation) {
                    bugle_platform::editor_set_busy(window, false);
                }
                self.toast_error(&format!("Annotate: could not save: {e}"));
            }
            EditorMsg::Copied(Ok(())) => bugle_platform::toast_show(None, "Copied to clipboard", "Annotated image", None, 2.5),
            EditorMsg::Copied(Err(e)) => self.toast_error(&format!("Annotate: could not copy: {e}")),
            EditorMsg::Preview { generation, seq, result } => {
                // Nothing to do for a window that has closed since.
                if let Some((_, session)) = session(generation) {
                    match session.try_borrow_mut() {
                        Ok(mut s) => s.on_preview(seq, result),
                        // Never dropped (the window would wait for it): try
                        // again on the next turn.
                        Err(_) => self.proxy.send(AppEvent::Editor(EditorMsg::Preview { generation, seq, result })),
                    }
                }
            }
            EditorMsg::Inject(event) => newest_window().into_iter().for_each(|w| bugle_platform::editor_inject(w, &event)),
            EditorMsg::Zoom(z) => newest_window().into_iter().for_each(|w| bugle_platform::editor_zoom(w, z)),
            EditorMsg::Close => newest_window().into_iter().for_each(bugle_platform::editor_close),
            EditorMsg::Count(tx) => {
                let _ = tx.send(bugle_platform::editor_count());
            }
        }
    }
}

/// `--smoke-test` step: open the editor on a synthetic image (a gradient PNG
/// in the temp directory, not added to history), draw with every kind of
/// input the window reports — the events are injected, since nothing can
/// click — pick styles and a canvas from the menus, crop, retype a label
/// over two lines and zoom, capturing the window along the way, and close
/// without saving. Before closing it opens a second editor next to the
/// first (and the first image once more, which only brings its window
/// forward) and counts the windows. The captures are kept only when
/// `BUGLE_SMOKE_KEEP` is set; the PNGs are removed either way. With `BUGLE_SMOKE_SAVE` set it saves
/// instead, which writes an annotated copy to the configured `save_dir` and
/// history: use it with a throwaway `--config`.
pub(crate) fn smoke(proxy: &Proxy) {
    let img = RgbaImage::from_fn(900, 560, |x, y| image::Rgba([40 + (x / 6) as u8, 60 + (y / 5) as u8, 120, 255]));
    let path = std::env::temp_dir().join("bugle-smoke-editor.png");
    if let Err(e) = img.save(&path) {
        eprintln!("smoke test: editor skipped ({e})");
        return;
    }
    let send = |e: EditorEvent| proxy.send(AppEvent::Editor(EditorMsg::Inject(e)));
    let pt = |x: f64, y: f64| Point { x, y };
    let click = |at: Point, clicks: u32| {
        send(EditorEvent::MouseDown { at, shift: false, clicks });
        send(EditorEvent::MouseUp { at, shift: false });
    };
    let drag = |tool: Tool, from: Point, to: Point| {
        send(EditorEvent::Key(tool.key()));
        send(EditorEvent::MouseDown { at: from, shift: false, clicks: 1 });
        send(EditorEvent::MouseDrag { at: pt((from.x + to.x) / 2.0, (from.y + to.y) / 2.0 + 12.0), shift: false });
        send(EditorEvent::MouseUp { at: to, shift: false });
    };
    // Proof the window is really on screen with something in it.
    let snap = |name: &str| {
        std::thread::sleep(Duration::from_millis(900));
        let shot = bugle_platform::app_windows()
            .ok()
            .and_then(|ws| ws.into_iter().find(|w| w.title.starts_with("Annotate")))
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
    proxy.send(AppEvent::Editor(EditorMsg::Open(path.display().to_string())));
    std::thread::sleep(Duration::from_millis(700));
    // A double-headed arrow, picked from the style menu.
    send(EditorEvent::Key(Tool::Arrow.key()));
    send(EditorEvent::Menu { menu: MENU_STYLE, item: 1 });
    drag(Tool::Arrow, pt(120.0, 420.0), pt(330.0, 250.0));
    send(EditorEvent::Color(4));
    drag(Tool::Rectangle, pt(340.0, 120.0), pt(620.0, 260.0));
    send(EditorEvent::Color(3));
    drag(Tool::Ellipse, pt(660.0, 300.0), pt(840.0, 420.0));
    drag(Tool::Pen, pt(80.0, 80.0), pt(260.0, 90.0));
    drag(Tool::Highlight, pt(360.0, 320.0), pt(600.0, 360.0));
    // A blacked-out redaction.
    send(EditorEvent::Key(Tool::Redact.key()));
    send(EditorEvent::Menu { menu: MENU_STYLE, item: 2 });
    drag(Tool::Redact, pt(360.0, 400.0), pt(600.0, 470.0));
    send(EditorEvent::Color(0));
    send(EditorEvent::Key(Tool::Counter.key()));
    click(pt(700.0, 110.0), 1);
    send(EditorEvent::Command(EditorCommand::Undo));
    send(EditorEvent::Command(EditorCommand::Redo));
    // A label: the click opens the window's text field, which stays up
    // (nothing can type into it); the text arrives as if it had been typed.
    send(EditorEvent::Key(Tool::Text.key()));
    click(pt(80.0, 470.0), 1);
    send(EditorEvent::Text("Smoke test\r\nsecond line".into()));
    // Shapes the crop will cut away entirely (past its right and bottom
    // edges): rendered after the crop they start outside the image.
    drag(Tool::Redact, pt(875.0, 300.0), pt(898.0, 420.0));
    drag(Tool::Highlight, pt(100.0, 545.0), pt(400.0, 558.0));
    drag(Tool::Rectangle, pt(880.0, 470.0), pt(898.0, 556.0));
    // Crop off the edges, then a canvas (Ocean, which brings padding).
    drag(Tool::Crop, pt(50.0, 40.0), pt(870.0, 540.0));
    snap("editor-crop");
    send(EditorEvent::Key(Tool::Select.key()));
    send(EditorEvent::Menu { menu: MENU_CANVAS, item: CANVAS_BACKGROUND_ITEM + 3 });
    // Leave the rectangle selected so the capture shows the selection marks.
    click(pt(340.0, 190.0), 1);
    snap("editor-window");
    // Double-click the label: the field comes back with both lines in it.
    click(pt(90.0, 480.0), 2);
    snap("editor-label");
    send(EditorEvent::Text("Smoke test\nsecond line".into()));
    proxy.send(AppEvent::Editor(EditorMsg::Zoom(EditorZoom::In)));
    proxy.send(AppEvent::Editor(EditorMsg::Zoom(EditorZoom::In)));
    snap("editor-zoomed");
    proxy.send(AppEvent::Editor(EditorMsg::Zoom(EditorZoom::Fit)));
    if std::env::var_os("BUGLE_SMOKE_SAVE").is_some() {
        // The window closes itself once the copy is written.
        send(EditorEvent::Command(EditorCommand::Save));
        std::thread::sleep(Duration::from_millis(2500));
    }
    let count = || {
        let (tx, rx) = mpsc::channel();
        proxy.send(AppEvent::Editor(EditorMsg::Count(tx)));
        rx.recv_timeout(Duration::from_secs(2)).ok()
    };
    // A second editor beside the first; the first image again only focuses.
    let second = std::env::temp_dir().join("bugle-smoke-editor-2.png");
    let img2 = RgbaImage::from_fn(480, 320, |x, y| image::Rgba([200, 80 + (x / 4) as u8, 40 + (y / 3) as u8, 255]));
    match img2.save(&second) {
        Ok(()) => {
            proxy.send(AppEvent::Editor(EditorMsg::Open(second.display().to_string())));
            proxy.send(AppEvent::Editor(EditorMsg::Open(path.display().to_string())));
            std::thread::sleep(Duration::from_millis(700));
            eprintln!("smoke test: editors open with a second one: {:?} (want Some(2))", count());
            // The newest is the second editor: draw in it, then close it.
            drag(Tool::Rectangle, pt(40.0, 40.0), pt(200.0, 160.0));
            snap("editor-second");
            proxy.send(AppEvent::Editor(EditorMsg::Close));
            std::thread::sleep(Duration::from_millis(300));
            eprintln!("smoke test: editors open after closing it: {:?} (want Some(1))", count());
        }
        Err(e) => eprintln!("smoke test: second editor skipped ({e})"),
    }
    proxy.send(AppEvent::Editor(EditorMsg::Close));
    std::thread::sleep(Duration::from_millis(300));
    eprintln!("smoke test: editors open at the end: {:?} (want Some(0))", count());
    for p in [&path, &second] {
        match std::fs::remove_file(p) {
            Err(e) if p.exists() => eprintln!("smoke test: could not remove {} ({e})", p.display()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only the shim hands out window ids; the tests tell entries apart by
    // generation.
    const W: EditorId = EditorId::NONE;

    #[test]
    fn registry_finds_live_sessions_by_generation() {
        let mut r = Registry::new();
        let (a, b) = (Rc::new("a"), Rc::new("b"));
        r.insert(1, W, &a);
        r.insert(2, W, &b);
        assert_eq!(r.get(1).map(|(_, s)| *s), Some("a"));
        assert_eq!(r.get(2).map(|(_, s)| *s), Some("b"));
        assert!(r.get(3).is_none(), "never opened");
        let newest: Vec<&str> = r.newest_first().map(|(_, s)| *s).collect();
        assert_eq!(newest, vec!["b", "a"]);
    }

    #[test]
    fn registry_forgets_closed_windows() {
        let mut r = Registry::new();
        let a = Rc::new(1);
        {
            let b = Rc::new(2);
            r.insert(1, W, &a);
            r.insert(2, W, &b);
            assert_eq!(r.newest_first().count(), 2);
        }
        // The second window's handler (the session's only owner) was dropped.
        assert!(r.get(2).is_none());
        assert_eq!(r.newest_first().map(|(_, s)| *s).collect::<Vec<_>>(), vec![1]);
        let c = Rc::new(3);
        r.insert(3, W, &c);
        assert_eq!(r.entries.len(), 2, "dead entries go on the next insert");
        drop(a);
        drop(c);
        assert_eq!(r.newest_first().count(), 0);
    }
}
