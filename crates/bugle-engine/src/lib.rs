//! The Bugle engine. One [`Engine`] per process; every front end (CLI, HTTP,
//! MCP) calls the same methods so behaviour cannot drift between them.
//!
//! Blocking methods talk to the window server and may take a few hundred
//! milliseconds; async front ends wrap them in `spawn_blocking`.

pub mod capture;
pub mod edit;
pub mod record;
pub mod scroll;
pub mod stitch;
pub mod sync;

use anyhow::{anyhow, bail, Context, Result};
use bugle_ai::{ImageInput, Registry, Role, TextModel, Transcriber, VisionModel};
use bugle_markup::{Annotation, Renderer, Style};
use bugle_core::naming;
use bugle_core::{
    BugleError, Config, DescribeRequest, DescribeResult, History, ImageFormat, MediaItem, MediaKind, OcrOptions, OcrResult,
    Rect, SubtitleFormat, Transcript,
};
use image::{DynamicImage, RgbaImage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use capture::CaptureOutcome;
pub use edit::{EditRequest, ExportPreset};
pub use record::RecorderState;
pub use sync::SyncReport;

/// Hook for front ends that own the process main thread and an AppKit run
/// loop (the menu-bar app). When installed, interactive selection and pin
/// windows go through it instead of spawning helper processes. Implementations
/// marshal to the main thread; the engine calls them from worker threads.
pub trait UiHost: Send + Sync {
    /// Show the crosshair overlay; `None` when the user cancels.
    fn select_area(&self) -> Result<Option<Rect>>;
    /// Float a PNG above all windows and return without waiting.
    fn pin(&self, png: Vec<u8>, frame: Option<Rect>, opacity: f64) -> Result<()>;
}

pub struct Engine {
    pub config: Config,
    pub history: History,
    pub ai: Registry,
    pub recorder_state: RecorderState,
    /// Whether the caller runs on the process main thread, so AppKit overlays
    /// (area selection, pin) can be shown in-process instead of via a helper.
    pub main_thread_ui: bool,
    /// In-process overlay host, see [`UiHost`]. Takes precedence over the
    /// helper-process fallback when `main_thread_ui` is false.
    pub ui_host: Option<Arc<dyn UiHost>>,
    renderer: Mutex<Option<Renderer>>,
    edit_slots: edit::EditSlots,
}

impl Engine {
    pub fn new(config: Config, main_thread_ui: bool) -> Result<Self> {
        let history = History::open(config.history_path())?;
        let ai = Registry::from_config(&config.ai);
        std::fs::create_dir_all(config.save_dir()).ok();
        if let Err(e) = config.sync.validate() {
            tracing::warn!("{e:#}; media that would sync there is not synced until the config is fixed");
        }
        for w in config.sync.warnings() {
            tracing::warn!("{w}");
        }
        Ok(Self { config, history, ai, recorder_state: RecorderState::default(), main_thread_ui, ui_host: None, renderer: Mutex::new(None), edit_slots: Default::default() })
    }

    pub fn from_default_config(main_thread_ui: bool) -> Result<Self> {
        Self::new(Config::load()?, main_thread_ui)
    }

    // -----------------------------------------------------------------------
    // MediaItem resolution
    // -----------------------------------------------------------------------

    /// Resolve `source` to a history item or a file on disk. Accepts a media
    /// id (or prefix), `latest`/`last`, or a path.
    ///
    /// A bare name is looked up in history first, so a file in the working
    /// directory cannot shadow a media id. Anything that is unambiguously a
    /// path (absolute, `~/…`, or containing `/`) only ever means a file.
    pub fn resolve_media(&self, source: &str) -> Result<MediaItem> {
        if source.is_empty() || source == "latest" || source == "last" {
            return self.history.latest()?.ok_or_else(|| BugleError::MediaNotFound("history is empty".into()).into());
        }
        let p = bugle_core::config::expand_tilde(source);
        if !is_explicit_path(source) {
            match self.history.get(source) {
                Ok(Some(m)) => return Ok(m),
                Ok(None) => {}
                // Too short or ambiguous as an id: still fine as a file name.
                Err(e) if !p.exists() => return Err(e),
                Err(_) => {}
            }
        }
        if p.exists() {
            return Ok(media_from_file(&p));
        }
        Err(BugleError::MediaNotFound(source.to_string()).into())
    }

    pub fn load_image(&self, source: &str) -> Result<(MediaItem, RgbaImage)> {
        let m = self.resolve_media(source)?;
        if m.is_video() {
            // First frame of a video is a fine stand-in for image operations.
            let img = bugle_platform::video_frame(&m.path, 0.0)?;
            return Ok((m, img));
        }
        let img = image::open(&m.path).with_context(|| format!("opening {}", m.path.display()))?.to_rgba8();
        Ok((m, img))
    }

    /// Where a new file should go, honouring an explicit output path.
    pub fn output_path(&self, output: Option<&str>, ext: &str, vars: &HashMap<&str, String>) -> PathBuf {
        if let Some(o) = output {
            let p = bugle_core::config::expand_tilde(o);
            if p.is_dir() {
                let stem = naming::render(&self.config.general.filename_template, vars);
                return naming::unique_path(&p, &stem, ext);
            }
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            return p;
        }
        let dir = self.config.save_dir();
        std::fs::create_dir_all(&dir).ok();
        let stem = naming::render(&self.config.general.filename_template, vars);
        naming::unique_path(&dir, &stem, ext)
    }

    /// Encode and save an image, sync it (see `[sync]`), record it in history.
    ///
    /// `format` is the format the request asked for (`None` = not given).
    /// Without one, a recognised image extension on `output` decides, then
    /// the configured default. An explicit format that disagrees with the
    /// extension wins and the extension is corrected.
    pub fn save_image(
        &self,
        img: &RgbaImage,
        kind: MediaKind,
        format: impl Into<Option<ImageFormat>>,
        quality: u8,
        output: Option<&str>,
        vars: &HashMap<&str, String>,
        label: Option<String>,
        source: Option<String>,
        parent: Option<String>,
    ) -> Result<MediaItem> {
        let format = choose_image_format(format.into(), self.config.screenshots.format, output);
        let path = match_extension(self.output_path(output, format.extension(), vars), format);
        encode_to_path(img, format, quality, &path)?;
        let mut m = MediaItem::new(kind, &path).with_size(img.width(), img.height());
        m.label = label;
        m.source = source;
        m.parent = parent;
        self.history.push(&m)?;
        self.sync_after_save(&mut m);
        Ok(m)
    }

    pub fn copy_to_clipboard(&self, path: &Path, img: Option<&RgbaImage>) -> Result<()> {
        match img {
            Some(img) => {
                let mut buf = std::io::Cursor::new(Vec::new());
                DynamicImage::ImageRgba8(img.clone()).write_to(&mut buf, image::ImageFormat::Png)?;
                bugle_platform::clipboard_write_png(buf.get_ref())
            }
            None => bugle_platform::clipboard_write_file(path),
        }
    }

    /// Put a media item on the clipboard: image data for images, a file
    /// reference for everything else.
    pub fn copy_media(&self, source: &str) -> Result<MediaItem> {
        let m = self.resolve_media(source)?;
        if m.is_image() {
            let (_, img) = self.load_image(source)?;
            self.copy_to_clipboard(&m.path, Some(&img))?;
        } else {
            self.copy_to_clipboard(&m.path, None)?;
        }
        Ok(m)
    }

    // -----------------------------------------------------------------------
    // OCR
    // -----------------------------------------------------------------------

    /// The OCR engine for a request: its `engine`, else `[ocr] engine`.
    pub fn ocr_engine(&self, opts: &OcrOptions) -> OcrEngine {
        select_ocr_engine(opts.engine.as_deref(), &self.config.ocr.engine)
    }

    /// OCR with whichever engine [`Engine::ocr_engine`] picks. Every front
    /// end goes through this (or [`Engine::run_ocr`]).
    pub async fn ocr_image_any(self: &Arc<Self>, img: RgbaImage, opts: &OcrOptions) -> Result<OcrResult> {
        match self.ocr_engine(opts) {
            OcrEngine::Vision => {
                let e = self.clone();
                let opts = opts.clone();
                tokio::task::spawn_blocking(move || e.ocr_image(&img, &opts)).await?
            }
            OcrEngine::Ai(provider) => self.ocr_image_ai(&img, opts, &provider).await,
        }
    }

    /// Load `source` and OCR it with the selected engine.
    pub async fn run_ocr(self: &Arc<Self>, source: &str, opts: &OcrOptions) -> Result<(MediaItem, OcrResult)> {
        let e = self.clone();
        let s = source.to_string();
        let (m, img) = tokio::task::spawn_blocking(move || e.load_image(&s)).await??;
        let r = self.ocr_image_any(img, opts).await?;
        Ok((m, r))
    }

    /// Apple Vision OCR (blocking). Fails when the selected engine is an AI
    /// provider; [`Engine::ocr_image_any`] routes automatically.
    pub fn ocr_image(&self, img: &RgbaImage, opts: &OcrOptions) -> Result<OcrResult> {
        if let OcrEngine::Ai(engine) = self.ocr_engine(opts) {
            bail!("OCR engine '{engine}' is an AI provider; use the async `ocr_image_any`/`run_ocr` path");
        }
        let (img, offset) = crop_region(img, opts.region)?;
        let langs = opts.languages.clone().unwrap_or_else(|| self.config.ocr.languages.clone());
        let params = bugle_platform::OcrParams {
            languages: &langs,
            accurate: !opts.fast.unwrap_or(!self.config.ocr.accurate),
            language_correction: self.config.ocr.language_correction,
        };
        let mut lines = bugle_platform::ocr(&img, &params)?;
        for l in &mut lines {
            l.bbox.x += offset.0;
            l.bbox.y += offset.1;
        }
        let text = join_lines(&lines);
        Ok(OcrResult { text, lines, engine: "vision".into() })
    }

    /// Blocking OCR of `source` with the selected engine, for callers outside
    /// an async context (the menu-bar app). An AI engine runs on a private
    /// runtime on a scoped thread, so this is safe from any thread.
    pub fn ocr(&self, source: &str, opts: &OcrOptions) -> Result<(MediaItem, OcrResult)> {
        let (m, img) = self.load_image(source)?;
        let r = match self.ocr_engine(opts) {
            OcrEngine::Vision => self.ocr_image(&img, opts)?,
            OcrEngine::Ai(provider) => std::thread::scope(|s| {
                s.spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?
                        .block_on(self.ocr_image_ai(&img, opts, &provider))
                })
                .join()
                .map_err(|_| anyhow!("OCR thread panicked"))?
            })?,
        };
        Ok((m, r))
    }

    /// OCR through a vision model instead of Apple Vision (no boxes).
    async fn ocr_image_ai(&self, img: &RgbaImage, opts: &OcrOptions, provider: &str) -> Result<OcrResult> {
        let (img, _) = crop_region(img, opts.region)?;
        let p = self.ai.resolve(Some(provider), Role::Vision)?;
        let png = encode_png_bounded(&img, 2000)?;
        let text = p
            .describe(
                &[ImageInput::png(png)],
                "Transcribe every piece of visible text in this image exactly, preserving line breaks and reading order. Output only the text.",
                Some("You are an OCR engine."),
            )
            .await?;
        Ok(OcrResult { text, lines: vec![], engine: provider.to_string() })
    }

    // -----------------------------------------------------------------------
    // Annotation
    // -----------------------------------------------------------------------

    fn with_renderer<T>(&self, f: impl FnOnce(&Renderer) -> Result<T>) -> Result<T> {
        let mut guard = self.renderer.lock().unwrap();
        if guard.is_none() {
            let a = &self.config.markup;
            let defaults = Style {
                color: Some(a.stroke_color.parse().map_err(|e: String| anyhow!(e))?),
                stroke_width: Some(a.stroke_width),
                font_size: Some(a.font_size),
                fill: None,
                opacity: None,
            };
            let font = a.font_path.as_ref().map(|p| bugle_core::config::expand_tilde(p));
            *guard = Some(Renderer::new(font.as_deref(), defaults)?);
        }
        f(guard.as_ref().unwrap())
    }

    pub fn apply_annotations(&self, source: &str, mut doc: Annotation, output: Option<&str>, copy: bool) -> Result<(MediaItem, RgbaImage)> {
        let (parent, img) = self.load_image(source)?;
        if let Some(c) = doc.canvas.as_mut() {
            if matches!(&c.background, bugle_markup::Background::Named(n) if n == "auto") {
                c.background = bugle_markup::Background::Named(self.config.markup.background.clone());
            }
        }
        let out = self.with_renderer(|r| r.render(&img, &doc))?;
        let mut vars = HashMap::new();
        vars.insert("kind", "annotated".to_string());
        let m = self.save_image(
            &out,
            MediaKind::Annotated,
            None,
            self.config.screenshots.jpeg_quality,
            output,
            &vars,
            None,
            Some(format!("annotate: {}", doc.summary())),
            Some(parent.id.clone()),
        )?;
        if copy || self.config.general.copy_to_clipboard {
            self.copy_to_clipboard(&m.path, Some(&out))?;
        }
        Ok((m, out))
    }

    // -----------------------------------------------------------------------
    // AI actions
    // -----------------------------------------------------------------------

    pub async fn describe(&self, req: &DescribeRequest) -> Result<DescribeResult> {
        let (media, img) = self.load_image(&req.source)?;
        let provider = self.ai.resolve(req.provider.as_deref(), Role::Vision)?;
        let mut prompt = req.prompt.clone().unwrap_or_else(|| {
            "Describe this screenshot: which application is shown, what the user is doing, and any notable text or state. Be specific and concise.".into()
        });
        let mut ocr = None;
        if req.with_ocr && media.is_image() {
            // Grounding text always comes from Apple Vision: the vision model
            // already sees the image, so a second AI pass adds nothing.
            let vision = OcrOptions { engine: Some("vision".into()), ..Default::default() };
            if let Ok(r) = self.ocr_image(&img, &vision) {
                if !r.text.trim().is_empty() {
                    prompt.push_str("\n\nText recognised by OCR (may contain errors):\n");
                    prompt.push_str(&r.text);
                }
                ocr = Some(r);
            }
        }
        let images = if media.is_video() {
            sample_video_frames(&media.path, 4)?
        } else {
            vec![ImageInput::png(encode_png_bounded(&img, 1600)?)]
        };
        let description = provider.describe(&images, &prompt, None).await?;
        Ok(DescribeResult { description, provider: provider.name().to_string(), model: provider.model(), ocr })
    }

    pub async fn complete_text(&self, prompt: &str, provider: Option<&str>, system: Option<&str>) -> Result<String> {
        let p = self.ai.resolve(provider, Role::Text)?;
        p.complete(prompt, system).await
    }

    /// Transcribe a recording (or audio file). Video sources get their audio
    /// track extracted to a temporary AAC file first.
    pub async fn transcribe(&self, source: &str, provider: Option<&str>, language: Option<&str>) -> Result<(MediaItem, Transcript)> {
        let media = self.resolve_media(source)?;
        let p = self.ai.resolve(provider, Role::Transcribe)?;
        let ext = media.path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let is_audio = matches!(ext.as_str(), "m4a" | "mp3" | "wav" | "aac" | "flac" | "ogg" | "caf");
        let tmp;
        let audio_path: PathBuf = if is_audio {
            media.path.clone()
        } else {
            tmp = tempfile::Builder::new().prefix("bugle-audio-").suffix(".m4a").tempfile()?;
            let input = media.path.clone();
            let out = tmp.path().to_path_buf();
            tokio::task::spawn_blocking(move || bugle_platform::export_audio_m4a(&input, &out)).await??;
            tmp.path().to_path_buf()
        };
        let mut t = p.transcribe(&audio_path, language).await?;
        if t.provider.is_empty() {
            t.provider = p.name().to_string();
        }
        Ok((media, t))
    }

    /// Transcribe and write a sidecar subtitle file next to the recording.
    pub async fn subtitles(
        &self,
        source: &str,
        format: SubtitleFormat,
        provider: Option<&str>,
        language: Option<&str>,
        output: Option<&str>,
    ) -> Result<(MediaItem, Transcript, PathBuf)> {
        let (media, transcript) = self.transcribe(source, provider, language).await?;
        let ext = match format {
            SubtitleFormat::Srt => "srt",
            SubtitleFormat::Vtt => "vtt",
        };
        let path = match output {
            Some(o) => {
                let p = bugle_core::config::expand_tilde(o);
                if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
                }
                p
            }
            None => media.path.with_extension(ext),
        };
        std::fs::write(&path, transcript.render(format)).with_context(|| format!("writing {}", path.display()))?;
        let source = format!("subtitles via {}", transcript.provider);
        // The sync may move the file; report where it is now.
        let m = self.save_subtitle_item(&media, &path, source, output.is_some())?;
        Ok((media, transcript, m.path))
    }

    // -----------------------------------------------------------------------
    // Pin / helpers
    // -----------------------------------------------------------------------

    /// Float an image above everything. Runs in-process on the main thread,
    /// otherwise spawns a detached helper process.
    pub fn pin(&self, source: &str, frame: Option<Rect>, opacity: f64, blocking: bool) -> Result<MediaItem> {
        let (media, img) = self.load_image(source)?;
        if self.main_thread_ui && blocking {
            let mut buf = std::io::Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(img).write_to(&mut buf, image::ImageFormat::Png)?;
            bugle_platform::pin_image_blocking(buf.get_ref(), frame, opacity);
            return Ok(media);
        }
        if let Some(host) = &self.ui_host {
            let mut buf = std::io::Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(img).write_to(&mut buf, image::ImageFormat::Png)?;
            host.pin(buf.into_inner(), frame, opacity)?;
            return Ok(media);
        }
        let exe = std::env::current_exe()?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("pin").arg(&media.path).arg("--opacity").arg(opacity.to_string());
        if let Some(f) = frame {
            cmd.arg("--at").arg(f.to_string());
        }
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().context("spawning pin helper")?;
        if blocking {
            child.wait()?;
        } else {
            // Reap the helper when it exits so a long-running server does not
            // accumulate zombie processes.
            std::thread::Builder::new()
                .name("bugle-pin-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                })
                .context("spawning pin reaper thread")?;
        }
        Ok(media)
    }

    /// Interactive rectangle selection.
    pub fn select_area(&self) -> Result<Option<Rect>> {
        if self.main_thread_ui {
            return Ok(bugle_platform::select_rect_blocking());
        }
        if let Some(host) = &self.ui_host {
            return host.select_area();
        }
        let exe = std::env::current_exe()?;
        let out = std::process::Command::new(exe).arg("select").arg("--json").output().context("spawning select helper")?;
        if !out.status.success() {
            return Ok(None);
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(v)?))
    }

    pub fn permissions(&self) -> serde_json::Value {
        serde_json::json!({
            "screen_recording": bugle_platform::screen_access_preflight(),
            "accessibility": bugle_platform::accessibility_trusted(false),
        })
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Which OCR implementation handles a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcrEngine {
    /// Apple Vision (text plus per-line boxes).
    Vision,
    /// A vision-capable AI provider by name (text only, no boxes).
    Ai(String),
}

/// The one rule for choosing an OCR engine: the request's value, else the
/// configured default. `vision` (or nothing) means Apple Vision; any other
/// name is an AI provider.
pub fn select_ocr_engine(requested: Option<&str>, config_default: &str) -> OcrEngine {
    let name = requested.map(str::trim).filter(|s| !s.is_empty()).unwrap_or_else(|| config_default.trim());
    if name.is_empty() || name.eq_ignore_ascii_case("vision") {
        OcrEngine::Vision
    } else {
        OcrEngine::Ai(name.to_string())
    }
}

/// Crop to an OCR region (pixels); returns the image and the crop offset.
fn crop_region(img: &RgbaImage, region: Option<Rect>) -> Result<(RgbaImage, (f64, f64))> {
    let Some(r) = region else {
        return Ok((img.clone(), (0.0, 0.0)));
    };
    if ![r.x, r.y, r.w, r.h].iter().all(|v| v.is_finite()) {
        return Err(BugleError::Invalid("OCR region must be finite numbers".into()).into());
    }
    let x0 = r.x.max(0.0) as u32;
    let y0 = r.y.max(0.0) as u32;
    let w = (r.w.max(0.0) as u32).min(img.width().saturating_sub(x0));
    let h = (r.h.max(0.0) as u32).min(img.height().saturating_sub(y0));
    if w == 0 || h == 0 {
        return Err(BugleError::Invalid("OCR region is outside the image".into()).into());
    }
    Ok((image::imageops::crop_imm(img, x0, y0, w, h).to_image(), (x0 as f64, y0 as f64)))
}

/// True when `source` can only mean a file: absolute, `~`-relative, or
/// containing a path separator.
fn is_explicit_path(source: &str) -> bool {
    source.starts_with('/') || source.starts_with('~') || source.contains('/')
}

fn media_from_file(p: &Path) -> MediaItem {
    let mut m = MediaItem::new(kind_from_path(p), p);
    if m.is_image() {
        if let Ok(dim) = image::image_dimensions(p) {
            m.width = Some(dim.0);
            m.height = Some(dim.1);
        }
    }
    m
}

/// Image format implied by a path's extension, if it is one we can write.
pub fn format_from_extension(p: &Path) -> Option<ImageFormat> {
    p.extension().and_then(|e| e.to_str()).and_then(|e| e.parse().ok())
}

/// Decide the encode format: an explicit request wins, then a recognised
/// image extension on the output path, then the configured default.
pub fn choose_image_format(explicit: Option<ImageFormat>, default: ImageFormat, output: Option<&str>) -> ImageFormat {
    if let Some(f) = explicit {
        return f;
    }
    output.and_then(|o| format_from_extension(Path::new(o))).unwrap_or(default)
}

/// Make a recognised image extension agree with `format` (an explicit
/// `--format png` with `-o shot.jpg` writes `shot.png`). Unrecognised or
/// missing extensions are left alone.
pub fn match_extension(path: PathBuf, format: ImageFormat) -> PathBuf {
    match format_from_extension(&path) {
        Some(f) if f != format => {
            let fixed = path.with_extension(format.extension());
            tracing::warn!("output {} does not match format {}; writing {}", path.display(), format.extension(), fixed.display());
            fixed
        }
        _ => path,
    }
}

/// Smallest and largest long edge [`encode_png_bounded`] accepts; requests
/// outside the range are clamped.
pub const MIN_MAX_EDGE: u32 = 16;
pub const MAX_MAX_EDGE: u32 = 8192;

/// Size that brings `(w, h)` down to a long edge of at most `max_edge`
/// (clamped to `MIN_MAX_EDGE..=MAX_MAX_EDGE`), keeping the aspect ratio and
/// never producing a zero side. `None` when the image already fits.
pub fn bounded_size(w: u32, h: u32, max_edge: u32) -> Option<(u32, u32)> {
    let max_edge = max_edge.clamp(MIN_MAX_EDGE, MAX_MAX_EDGE);
    if w.max(h) <= max_edge {
        return None;
    }
    let s = max_edge as f64 / w.max(h) as f64;
    let side = |v: u32| (((v as f64) * s).round() as u32).clamp(1, max_edge);
    Some((side(w), side(h)))
}

pub fn kind_from_path(p: &Path) -> MediaKind {
    match p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("mov") | Some("mp4") | Some("m4v") => MediaKind::Video,
        Some("gif") => MediaKind::Gif,
        Some("m4a") | Some("mp3") | Some("wav") | Some("aac") => MediaKind::Audio,
        Some("srt") | Some("vtt") => MediaKind::Subtitle,
        Some("txt") | Some("md") => MediaKind::Text,
        _ => MediaKind::Screenshot,
    }
}

pub fn encode_to_path(img: &RgbaImage, format: ImageFormat, quality: u8, path: &Path) -> Result<()> {
    let dynimg = DynamicImage::ImageRgba8(img.clone());
    match format {
        ImageFormat::Png => dynimg.save_with_format(path, image::ImageFormat::Png)?,
        ImageFormat::Jpeg => {
            let f = std::fs::File::create(path)?;
            let mut w = std::io::BufWriter::new(f);
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut w, quality.clamp(1, 100));
            dynimg.to_rgb8().write_with_encoder(enc)?;
        }
        ImageFormat::Webp => dynimg.save_with_format(path, image::ImageFormat::WebP)?,
        ImageFormat::Tiff => dynimg.save_with_format(path, image::ImageFormat::Tiff)?,
    }
    Ok(())
}

/// PNG-encode, downscaling so the long edge is at most `max_edge` pixels
/// (vision models neither need nor want 5K screenshots).
/// `max_edge` is clamped to [`MIN_MAX_EDGE`]`..=`[`MAX_MAX_EDGE`].
pub fn encode_png_bounded(img: &RgbaImage, max_edge: u32) -> Result<Vec<u8>> {
    let scaled;
    let src = if let Some((w, h)) = bounded_size(img.width(), img.height(), max_edge) {
        scaled = image::imageops::resize(img, w, h, image::imageops::FilterType::Triangle);
        &scaled
    } else {
        img
    };
    let mut buf = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(src.clone()).write_to(&mut buf, image::ImageFormat::Png)?;
    Ok(buf.into_inner())
}

pub fn sample_video_frames(path: &Path, n: usize) -> Result<Vec<ImageInput>> {
    let dur = bugle_platform::media_duration(path)?;
    let mut out = vec![];
    for i in 0..n.max(1) {
        let t = if n <= 1 { dur / 2.0 } else { dur * (i as f64 + 0.5) / n as f64 };
        if let Ok(f) = bugle_platform::video_frame(path, t) {
            out.push(ImageInput::png(encode_png_bounded(&f, 1280)?));
        }
    }
    if out.is_empty() {
        bail!("could not extract any frames from {}", path.display());
    }
    Ok(out)
}

/// Join OCR lines into text, inserting blank lines where the vertical gap is
/// clearly bigger than a line (paragraph breaks).
fn join_lines(lines: &[bugle_core::OcrLine]) -> String {
    let mut out = String::new();
    let mut prev: Option<&bugle_core::OcrLine> = None;
    for l in lines {
        if let Some(p) = prev {
            let same_row = (l.bbox.y - p.bbox.y).abs() < p.bbox.h * 0.5;
            if same_row {
                out.push_str("    ");
            } else {
                let gap = l.bbox.y - p.bbox.bottom();
                out.push('\n');
                if gap > p.bbox.h * 1.2 {
                    out.push('\n');
                }
            }
        }
        out.push_str(&l.text);
        prev = Some(l);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_engine_selection() {
        assert_eq!(select_ocr_engine(None, "vision"), OcrEngine::Vision);
        assert_eq!(select_ocr_engine(None, ""), OcrEngine::Vision);
        assert_eq!(select_ocr_engine(None, "openai"), OcrEngine::Ai("openai".into()));
        assert_eq!(select_ocr_engine(Some("vision"), "openai"), OcrEngine::Vision);
        assert_eq!(select_ocr_engine(Some("Vision"), "openai"), OcrEngine::Vision);
        assert_eq!(select_ocr_engine(Some("ollama"), "vision"), OcrEngine::Ai("ollama".into()));
        assert_eq!(select_ocr_engine(Some(""), "ollama"), OcrEngine::Ai("ollama".into()));
    }

    #[test]
    fn format_inference() {
        use ImageFormat::*;
        assert_eq!(choose_image_format(None, Png, Some("shot.jpg")), Jpeg);
        assert_eq!(choose_image_format(None, Png, Some("~/x/shot.JPEG")), Jpeg);
        assert_eq!(choose_image_format(None, Png, Some("shot.webp")), Webp);
        assert_eq!(choose_image_format(None, Jpeg, Some("shot.tif")), Tiff);
        assert_eq!(choose_image_format(None, Jpeg, Some("shot")), Jpeg);
        assert_eq!(choose_image_format(None, Jpeg, Some("shot.bmp")), Jpeg);
        assert_eq!(choose_image_format(None, Webp, None), Webp);
        assert_eq!(choose_image_format(Some(Png), Jpeg, Some("shot.jpg")), Png);

        assert_eq!(match_extension(PathBuf::from("/t/shot.jpg"), Png), PathBuf::from("/t/shot.png"));
        assert_eq!(match_extension(PathBuf::from("/t/shot.jpeg"), Jpeg), PathBuf::from("/t/shot.jpeg"));
        assert_eq!(match_extension(PathBuf::from("/t/shot.TIF"), Tiff), PathBuf::from("/t/shot.TIF"));
        assert_eq!(match_extension(PathBuf::from("/t/shot"), Png), PathBuf::from("/t/shot"));
        assert_eq!(match_extension(PathBuf::from("/t/shot.dat"), Png), PathBuf::from("/t/shot.dat"));
    }

    #[test]
    fn bounded_encode_never_zero() {
        assert_eq!(bounded_size(100, 50, 1600), None);
        assert_eq!(bounded_size(3200, 1600, 1600), Some((1600, 800)));
        // max_edge 0 is clamped up, extreme aspect ratios keep a 1px side.
        assert_eq!(bounded_size(4000, 2, 0), Some((MIN_MAX_EDGE, 1)));
        assert_eq!(bounded_size(1, 100_000, 1600), Some((1, 1600)));
        assert_eq!(bounded_size(20_000, 10_000, u32::MAX), Some((MAX_MAX_EDGE, 4096)));

        let img = RgbaImage::new(5000, 3);
        let png = encode_png_bounded(&img, 0).unwrap();
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (MIN_MAX_EDGE, 1));
    }

    #[test]
    fn crop_region_rejects_bad_regions() {
        let img = RgbaImage::new(100, 100);
        assert!(crop_region(&img, Some(Rect::new(f64::NAN, 0.0, 10.0, 10.0))).is_err());
        assert!(crop_region(&img, Some(Rect::new(200.0, 0.0, 10.0, 10.0))).is_err());
        let (c, off) = crop_region(&img, Some(Rect::new(90.0, 10.0, 50.0, 20.0))).unwrap();
        assert_eq!((c.width(), c.height(), off), (10, 20, (90.0, 10.0)));
    }

    #[test]
    fn history_id_beats_relative_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.general.save_dir = dir.path().join("out").display().to_string();
        config.general.history_path = Some(dir.path().join("history.jsonl").display().to_string());
        let engine = Engine::new(config, false).unwrap();

        // A file in the working directory whose name equals a media id.
        let local = tempfile::Builder::new().prefix("bugle-resolve-test-").tempfile_in(".").unwrap();
        let name = local.path().file_name().unwrap().to_str().unwrap().to_string();
        let stored = dir.path().join("stored.png");
        RgbaImage::new(2, 2).save(&stored).unwrap();
        let mut item = MediaItem::new(MediaKind::Screenshot, &stored);
        item.id = name.clone();
        engine.history.push(&item).unwrap();

        assert_eq!(engine.resolve_media(&name).unwrap().path, stored);
        // With a separator it is a path again.
        let explicit = format!("./{name}");
        assert_eq!(engine.resolve_media(&explicit).unwrap().path, PathBuf::from(&explicit));
        // Absolute paths resolve to files; unknown names are not found.
        assert_eq!(engine.resolve_media(stored.to_str().unwrap()).unwrap().path, stored);
        let missing = engine.resolve_media("no-such-id-or-file").unwrap_err();
        assert!(matches!(missing.downcast_ref::<BugleError>(), Some(BugleError::MediaNotFound(_))));
    }
}
