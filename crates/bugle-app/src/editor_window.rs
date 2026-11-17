//! Windowing glue for the markup editor: opens the AppKit window for an
//! image, feeds its events to [`Editor`], re-renders the preview with the
//! engine's renderer on a preview thread, and saves or copies the result off
//! the main thread. The decisions live in `markup_editor.rs`.

use crate::markup_editor::{Editor, Effects, Request, Tool, CANVAS_BACKGROUND_ITEM, MENUS, MENU_CANVAS, MENU_STYLE, PALETTE, SIZES};
use crate::{file_name, saved_subtitle, App, AppEvent, Notice, Proxy};
use bugle_core::Point;
use bugle_engine::Engine;
use bugle_markup::Annotation;
use bugle_platform::{EditorCommand, EditorEvent, EditorMenu, EditorTool, EditorZoom};
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
    /// Smoke test: deliver an event as if the window had reported it.
    Inject(EditorEvent),
    /// Smoke test: zoom as the keyboard shortcuts do.
    Zoom(EditorZoom),
    /// Smoke test: close without saving.
    Close,
}

thread_local! {
    /// The open window's session, for preview results arriving on the loop.
    /// The window's handler owns the session; this never keeps it alive.
    static CURRENT: RefCell<Weak<RefCell<Session>>> = const { RefCell::new(Weak::new()) };
}

/// One preview to render: its sequence number and the document.
type PreviewJob = (u64, Annotation);

/// Run a render (or save) and turn a panic into an error, so a renderer bug
/// can neither take down a worker thread that the window waits on nor
/// unwind through the AppKit callback on the main thread.
fn guarded<T>(what: &str, f: impl FnOnce() -> anyhow::Result<T>) -> Result<T, String> {
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
            bugle_platform::editor_set_state(self.editor.state());
            self.sync_menus();
        }
        if let Some(t) = fx.begin_text {
            bugle_platform::editor_begin_text(t.at, t.font_px, &t.initial, t.color);
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
            bugle_platform::editor_set_overlay(&self.editor.overlay());
        }
    }

    /// Send the menus that changed.
    fn sync_menus(&mut self) {
        let menus = self.editor.menus();
        for (i, m) in menus.iter().enumerate() {
            if self.menus.get(i) != Some(m) {
                bugle_platform::editor_set_menu(i, m);
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
            return bugle_platform::editor_set_image(&self.base, Point::default());
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
                bugle_platform::editor_set_image(&img, origin);
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
            return bugle_platform::editor_close();
        }
        let doc = self.editor.final_document();
        let (engine, proxy, source, generation) = (self.engine.clone(), self.proxy.clone(), self.source.clone(), self.generation);
        let saving = self.saving.clone();
        saving.store(true, Ordering::SeqCst);
        // No closing half-way: the copy is being written either way.
        bugle_platform::editor_set_busy(true);
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
            bugle_platform::editor_set_busy(false);
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

    /// Open the markup editor for a media id, `latest` or an image path. One
    /// editor at a time: with one already open, that one comes forward.
    pub(crate) fn open_editor(&mut self, source: &str) {
        if bugle_platform::editor_active() {
            bugle_platform::editor_focus();
            return self.toast_error("Annotate: finish or close the open editor first");
        }
        let (item, base) = match self.engine.load_image(source) {
            Ok(v) => v,
            Err(e) => return self.toast_error(&format!("Annotate: {e:#}")),
        };
        self.editor_generation += 1;
        let generation = self.editor_generation;
        let size = base.dimensions();
        let base = Arc::new(base);
        let session = Rc::new(RefCell::new(Session {
            editor: Editor::new(size),
            preview: spawn_preview(self.engine.clone(), base.clone(), self.proxy.clone(), generation),
            base,
            engine: self.engine.clone(),
            proxy: self.proxy.clone(),
            // Pin down what is on screen: `latest` may be something else by
            // the time the copy is saved.
            source: match self.engine.history.get(&item.id) {
                Ok(Some(_)) => item.id.clone(),
                _ => item.path.display().to_string(),
            },
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
        if let Err(e) = opened {
            return self.toast_error(&format!("Annotate: {e:#}"));
        }
        CURRENT.with(|c| *c.borrow_mut() = Rc::downgrade(&session));
        tracing::info!("editor: opened {} ({}×{})", file_name(&item.path), size.0, size.1);
        // First paint: the image, an empty overlay, the toolbar and menus.
        session.borrow_mut().apply(Effects { render: true, ..Default::default() }, false);
    }

    pub(crate) fn on_editor(&mut self, msg: EditorMsg) {
        match msg {
            EditorMsg::Open(source) => self.open_editor(&source),
            EditorMsg::Saved { generation, result: Ok(n) } => {
                if generation == self.editor_generation {
                    bugle_platform::editor_close();
                }
                tracing::info!("editor: saved {}", n.subtitle);
                if !self.quick_show_notice(&n) {
                    self.toast(&n);
                }
                self.refresh_recent();
            }
            EditorMsg::Saved { generation, result: Err(e) } => {
                // The window is still up with the work in it; let it go on.
                if generation == self.editor_generation {
                    bugle_platform::editor_set_busy(false);
                }
                self.toast_error(&format!("Annotate: could not save: {e}"));
            }
            EditorMsg::Copied(Ok(())) => bugle_platform::toast_show(None, "Copied to clipboard", "Annotated image", None, 2.5),
            EditorMsg::Copied(Err(e)) => self.toast_error(&format!("Annotate: could not copy: {e}")),
            EditorMsg::Preview { generation, seq, result } => {
                let session = CURRENT.with(|c| c.borrow().upgrade());
                if let Some(session) = session {
                    match session.try_borrow_mut() {
                        Ok(mut s) if s.generation == generation => s.on_preview(seq, result),
                        // A window that has closed since.
                        Ok(_) => {}
                        // Never dropped (the window would wait for it): try
                        // again on the next turn.
                        Err(_) => self.proxy.send(AppEvent::Editor(EditorMsg::Preview { generation, seq, result })),
                    }
                }
            }
            EditorMsg::Inject(event) => bugle_platform::editor_inject(&event),
            EditorMsg::Zoom(z) => bugle_platform::editor_zoom(z),
            EditorMsg::Close => bugle_platform::editor_close(),
        }
    }
}

/// `--smoke-test` step: open the editor on a synthetic image (a gradient PNG
/// in the temp directory, not added to history), draw with every kind of
/// input the window reports — the events are injected, since nothing can
/// click — pick styles and a canvas from the menus, crop, retype a label
/// over two lines and zoom, capturing the window along the way, and close
/// without saving. The captures are kept only when `BUGLE_SMOKE_KEEP` is
/// set; the PNG is removed either way. With `BUGLE_SMOKE_SAVE` set it saves
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
    proxy.send(AppEvent::Editor(EditorMsg::Close));
    std::thread::sleep(Duration::from_millis(300));
    if let Err(e) = std::fs::remove_file(&path) {
        eprintln!("smoke test: could not remove {} ({e})", path.display());
    }
}
