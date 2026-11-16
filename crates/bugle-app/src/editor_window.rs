//! Windowing glue for the markup editor: opens the AppKit window for an
//! image, feeds its events to [`Editor`], re-renders the preview with the
//! engine's renderer, and saves or copies the result off the main thread.
//! The decisions live in `markup_editor.rs`.

use crate::markup_editor::{Editor, Effects, Request, Tool, PALETTE, SIZES};
use crate::{file_name, saved_subtitle, App, AppEvent, Notice, Proxy};
use bugle_engine::Engine;
use bugle_platform::{EditorCommand, EditorEvent, EditorTool};
use image::RgbaImage;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Editor traffic on the main loop.
pub(crate) enum EditorMsg {
    /// Open the editor for a media id, `latest` or a file path.
    Open(String),
    /// A save finished. On success the window `generation` closes.
    Saved { generation: u64, result: Result<Notice, String> },
    Copied(Result<(), String>),
    /// Smoke test: deliver an event as if the window had reported it.
    Inject(EditorEvent),
    /// Smoke test: close without saving.
    Close,
}

/// One open editor window. Lives in the window's event handler and is
/// dropped when the window closes.
struct Session {
    editor: Editor,
    base: RgbaImage,
    engine: Arc<Engine>,
    proxy: Proxy,
    /// What `Engine::apply_annotations` saves from.
    source: String,
    generation: u64,
    /// A save is running; input is ignored until it reports back.
    saving: Arc<AtomicBool>,
}

impl Session {
    fn on_event(&mut self, event: EditorEvent) {
        if self.saving.load(Ordering::SeqCst) {
            return;
        }
        let fx = self.editor.handle(event);
        self.apply(fx);
    }

    fn apply(&mut self, fx: Effects) {
        if fx.render {
            self.render();
        }
        if fx.render || fx.overlay {
            bugle_platform::editor_set_overlay(&self.editor.overlay());
        }
        if fx.render || fx.state {
            bugle_platform::editor_set_state(self.editor.state());
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

    /// Show the image with the finished shapes baked in.
    fn render(&self) {
        let doc = self.editor.document();
        if doc.shapes.is_empty() {
            return bugle_platform::editor_set_image(&self.base);
        }
        match self.engine.render_annotations(&self.base, &doc) {
            Ok(img) => bugle_platform::editor_set_image(&img),
            Err(e) => tracing::warn!("editor: preview failed: {e:#}"),
        }
    }

    /// Save an annotated copy as a new history item. The window stays up
    /// (ignoring input) until the save succeeded, so a failure loses nothing.
    fn save(&mut self) {
        if self.editor.is_empty() {
            // Nothing drawn: there is no copy worth writing.
            return bugle_platform::editor_close();
        }
        let doc = self.editor.final_document();
        let (engine, proxy, source, generation) = (self.engine.clone(), self.proxy.clone(), self.source.clone(), self.generation);
        let saving = self.saving.clone();
        saving.store(true, Ordering::SeqCst);
        let flag = saving.clone();
        let spawned = std::thread::Builder::new().name("bugle-editor-save".into()).spawn(move || {
            let result = engine
                .apply_annotations(&source, doc, None, false)
                .map(|(item, img)| Notice {
                    title: "Annotated copy saved".into(),
                    subtitle: saved_subtitle(&engine, &item),
                    thumb: bugle_engine::encode_png_bounded(&img, 112).ok(),
                    open: Some(item.path.clone()),
                })
                .map_err(|e| format!("{e:#}"));
            if result.is_err() {
                flag.store(false, Ordering::SeqCst);
            }
            proxy.send(AppEvent::Editor(EditorMsg::Saved { generation, result }));
        });
        if let Err(e) = spawned {
            saving.store(false, Ordering::SeqCst);
            self.proxy.send(AppEvent::Editor(EditorMsg::Saved { generation, result: Err(e.to_string()) }));
        }
    }

    /// Put the annotated image on the clipboard; the window stays open.
    fn copy(&self) {
        let doc = self.editor.final_document();
        let (engine, proxy, base) = (self.engine.clone(), self.proxy.clone(), self.base.clone());
        let spawned = std::thread::Builder::new().name("bugle-editor-copy".into()).spawn(move || {
            let result = (|| {
                let img = if doc.shapes.is_empty() { base } else { engine.render_annotations(&base, &doc)? };
                let mut png = Vec::new();
                img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
                bugle_platform::clipboard_write_png(&png)
            })();
            proxy.send(AppEvent::Editor(EditorMsg::Copied(result.map_err(|e| format!("{e:#}")))));
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
        let size = base.dimensions();
        let session = Rc::new(RefCell::new(Session {
            editor: Editor::new(size),
            base,
            engine: self.engine.clone(),
            proxy: self.proxy.clone(),
            source: source.to_string(),
            generation: self.editor_generation,
            saving: Arc::new(AtomicBool::new(false)),
        }));
        let tools: Vec<EditorTool> = Tool::ALL.iter().map(|t| t.button()).collect();
        let handler = session.clone();
        let opened = bugle_platform::editor_open(
            &format!("Annotate — {}", file_name(&item.path)),
            size,
            &tools,
            &PALETTE,
            &SIZES,
            move |event| handler.borrow_mut().on_event(event),
        );
        if let Err(e) = opened {
            return self.toast_error(&format!("Annotate: {e:#}"));
        }
        tracing::info!("editor: opened {} ({}×{})", file_name(&item.path), size.0, size.1);
        // First paint: the image, an empty overlay and the toolbar state.
        session.borrow_mut().apply(Effects { render: true, ..Default::default() });
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
            EditorMsg::Saved { result: Err(e), .. } => self.toast_error(&format!("Annotate: could not save: {e}")),
            EditorMsg::Copied(Ok(())) => bugle_platform::toast_show(None, "Copied to clipboard", "Annotated image", None, 2.5),
            EditorMsg::Copied(Err(e)) => self.toast_error(&format!("Annotate: could not copy: {e}")),
            EditorMsg::Inject(event) => bugle_platform::editor_inject(&event),
            EditorMsg::Close => bugle_platform::editor_close(),
        }
    }
}

/// `--smoke-test` step: open the editor on a synthetic image (a gradient PNG
/// in the temp directory, not added to history), draw with every kind of
/// input the window reports — the events are injected, since nothing can
/// click — capture the window, and close without saving. The capture is kept
/// only when `BUGLE_SMOKE_KEEP` is set; the PNG is removed either way. With
/// `BUGLE_SMOKE_SAVE` set it saves instead, which writes an annotated copy to
/// the configured `save_dir` and history: use it with a throwaway `--config`.
pub(crate) fn smoke(proxy: &Proxy) {
    use bugle_core::Point;
    let img = RgbaImage::from_fn(900, 560, |x, y| image::Rgba([40 + (x / 6) as u8, 60 + (y / 5) as u8, 120, 255]));
    let path = std::env::temp_dir().join("bugle-smoke-editor.png");
    if let Err(e) = img.save(&path) {
        eprintln!("smoke test: editor skipped ({e})");
        return;
    }
    let send = |e: EditorEvent| proxy.send(AppEvent::Editor(EditorMsg::Inject(e)));
    let pt = |x: f64, y: f64| Point { x, y };
    let drag = |tool: Tool, from: Point, to: Point| {
        send(EditorEvent::Key(tool.key()));
        send(EditorEvent::MouseDown { at: from, shift: false, clicks: 1 });
        send(EditorEvent::MouseDrag { at: pt((from.x + to.x) / 2.0, (from.y + to.y) / 2.0 + 12.0), shift: false });
        send(EditorEvent::MouseUp { at: to, shift: false });
    };
    proxy.send(AppEvent::Editor(EditorMsg::Open(path.display().to_string())));
    std::thread::sleep(Duration::from_millis(700));
    drag(Tool::Arrow, pt(120.0, 420.0), pt(330.0, 250.0));
    send(EditorEvent::Color(4));
    drag(Tool::Rectangle, pt(340.0, 120.0), pt(620.0, 260.0));
    send(EditorEvent::Color(3));
    drag(Tool::Ellipse, pt(660.0, 300.0), pt(840.0, 420.0));
    drag(Tool::Pen, pt(80.0, 80.0), pt(260.0, 90.0));
    drag(Tool::Highlight, pt(360.0, 320.0), pt(600.0, 360.0));
    drag(Tool::Redact, pt(360.0, 400.0), pt(600.0, 470.0));
    send(EditorEvent::Color(0));
    send(EditorEvent::Key(Tool::Counter.key()));
    send(EditorEvent::MouseDown { at: pt(700.0, 110.0), shift: false, clicks: 1 });
    send(EditorEvent::MouseUp { at: pt(700.0, 110.0), shift: false });
    send(EditorEvent::Command(EditorCommand::Undo));
    send(EditorEvent::Command(EditorCommand::Redo));
    // A label: the click opens the window's text field, which stays up
    // (nothing can type into it); the text arrives as if it had been typed.
    send(EditorEvent::Key(Tool::Text.key()));
    send(EditorEvent::MouseDown { at: pt(80.0, 480.0), shift: false, clicks: 1 });
    send(EditorEvent::MouseUp { at: pt(80.0, 480.0), shift: false });
    send(EditorEvent::Text("Smoke test".into()));
    // Leave the rectangle selected so the capture shows the selection marks.
    send(EditorEvent::Key(Tool::Select.key()));
    send(EditorEvent::MouseDown { at: pt(340.0, 190.0), shift: false, clicks: 1 });
    send(EditorEvent::MouseUp { at: pt(340.0, 190.0), shift: false });
    std::thread::sleep(Duration::from_millis(900));
    // Proof the window is really on screen with something in it.
    let shot = bugle_platform::app_windows()
        .ok()
        .and_then(|ws| ws.into_iter().find(|w| w.title.starts_with("Annotate")))
        .and_then(|w| bugle_platform::capture_window(w.id, false, false).ok());
    match shot {
        Some(shot) if std::env::var_os("BUGLE_SMOKE_KEEP").is_some() => {
            let out = std::env::temp_dir().join("bugle-smoke-editor-window.png");
            match shot.save(&out) {
                Ok(()) => eprintln!("smoke test: editor window → {}", out.display()),
                Err(e) => eprintln!("smoke test: editor window not kept ({e})"),
            }
        }
        Some(shot) => eprintln!("smoke test: editor window {}×{}", shot.width(), shot.height()),
        None => eprintln!("smoke test: editor window not found"),
    }
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
