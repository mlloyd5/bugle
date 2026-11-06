//! The Bugle engine. One [`Engine`] per process; every front end (CLI, HTTP,
//! MCP) calls the same methods so behaviour cannot drift between them.
//!
//! Blocking methods talk to the window server and may take a few hundred
//! milliseconds; async front ends wrap them in `spawn_blocking`.

pub mod capture;
pub mod record;
pub mod scroll;
pub mod stitch;

use anyhow::{anyhow, bail, Context, Result};
use bugle_ai::{ImageInput, Registry, Role, TextModel, Transcriber, VisionModel};
use bugle_annotate::{Annotation, Renderer, Style};
use bugle_core::naming;
use bugle_core::{
    BugleError, Config, DescribeRequest, DescribeResult, History, ImageFormat, Media, MediaKind, OcrOptions, OcrResult,
    Rect, SubtitleFormat, Transcript,
};
use image::{DynamicImage, RgbaImage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub use capture::CaptureOutcome;
pub use record::Recorder;

pub struct Engine {
    pub config: Config,
    pub history: History,
    pub ai: Registry,
    pub recorder: Recorder,
    /// Whether the caller runs on the process main thread, so AppKit overlays
    /// (area selection, pin) can be shown in-process instead of via a helper.
    pub main_thread_ui: bool,
    renderer: Mutex<Option<Renderer>>,
}

impl Engine {
    pub fn new(config: Config, main_thread_ui: bool) -> Result<Self> {
        let history = History::open(config.history_path())?;
        let ai = Registry::from_config(&config.ai);
        std::fs::create_dir_all(config.save_dir()).ok();
        Ok(Self { config, history, ai, recorder: Recorder::default(), main_thread_ui, renderer: Mutex::new(None) })
    }

    pub fn from_default_config(main_thread_ui: bool) -> Result<Self> {
        Self::new(Config::load()?, main_thread_ui)
    }

    // -----------------------------------------------------------------------
    // Media resolution
    // -----------------------------------------------------------------------

    /// Resolve `source` to a history item or a file on disk. Accepts a media
    /// id (or prefix), `latest`/`last`, or a path.
    pub fn resolve_media(&self, source: &str) -> Result<Media> {
        if source.is_empty() || source == "latest" || source == "last" {
            return self.history.latest()?.ok_or_else(|| BugleError::MediaNotFound("history is empty".into()).into());
        }
        let p = bugle_core::config::expand_tilde(source);
        if p.exists() {
            let kind = kind_from_path(&p);
            let mut m = Media::new(kind, &p);
            if m.is_image() {
                if let Ok(dim) = image::image_dimensions(&p) {
                    m.width = Some(dim.0);
                    m.height = Some(dim.1);
                }
            }
            return Ok(m);
        }
        self.history.get(source)?.ok_or_else(|| BugleError::MediaNotFound(source.to_string()).into())
    }

    pub fn load_image(&self, source: &str) -> Result<(Media, RgbaImage)> {
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

    /// Encode and save an image, record it in history.
    pub fn save_image(
        &self,
        img: &RgbaImage,
        kind: MediaKind,
        format: ImageFormat,
        quality: u8,
        output: Option<&str>,
        vars: &HashMap<&str, String>,
        label: Option<String>,
        source: Option<String>,
        parent: Option<String>,
    ) -> Result<Media> {
        let path = self.output_path(output, format.extension(), vars);
        encode_to_path(img, format, quality, &path)?;
        let mut m = Media::new(kind, &path).with_size(img.width(), img.height());
        m.label = label;
        m.source = source;
        m.parent = parent;
        self.history.push(&m)?;
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

    // -----------------------------------------------------------------------
    // OCR
    // -----------------------------------------------------------------------

    pub fn ocr_image(&self, img: &RgbaImage, opts: &OcrOptions) -> Result<OcrResult> {
        let engine = opts.engine.clone().unwrap_or_else(|| self.config.ocr.engine.clone());
        if engine != "vision" {
            bail!("OCR engine '{engine}' is an AI provider; use `describe`/the async `ocr_with_ai` path");
        }
        let (img, offset) = match opts.region {
            Some(r) => {
                let x0 = r.x.max(0.0) as u32;
                let y0 = r.y.max(0.0) as u32;
                let w = (r.w as u32).min(img.width().saturating_sub(x0));
                let h = (r.h as u32).min(img.height().saturating_sub(y0));
                if w == 0 || h == 0 {
                    return Err(BugleError::Invalid("OCR region is outside the image".into()).into());
                }
                (image::imageops::crop_imm(img, x0, y0, w, h).to_image(), (x0 as f64, y0 as f64))
            }
            None => (img.clone(), (0.0, 0.0)),
        };
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

    pub fn ocr(&self, source: &str, opts: &OcrOptions) -> Result<(Media, OcrResult)> {
        let (m, img) = self.load_image(source)?;
        Ok((m, self.ocr_image(&img, opts)?))
    }

    /// OCR through a vision model instead of Apple Vision (no boxes).
    pub async fn ocr_with_ai(&self, source: &str, provider: &str) -> Result<OcrResult> {
        let (_, img) = self.load_image(source)?;
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
            let a = &self.config.annotate;
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

    pub fn annotate(&self, source: &str, mut doc: Annotation, output: Option<&str>, copy: bool) -> Result<(Media, RgbaImage)> {
        let (parent, img) = self.load_image(source)?;
        if let Some(c) = doc.canvas.as_mut() {
            if matches!(&c.background, bugle_annotate::Background::Named(n) if n == "auto") {
                c.background = bugle_annotate::Background::Named(self.config.annotate.background.clone());
            }
        }
        let out = self.with_renderer(|r| r.render(&img, &doc))?;
        let mut vars = HashMap::new();
        vars.insert("kind", "annotated".to_string());
        let m = self.save_image(
            &out,
            MediaKind::Annotated,
            self.config.screenshots.format,
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
            if let Ok(r) = self.ocr_image(&img, &OcrOptions::default()) {
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
    pub async fn transcribe(&self, source: &str, provider: Option<&str>, language: Option<&str>) -> Result<(Media, Transcript)> {
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
    ) -> Result<(Media, Transcript, PathBuf)> {
        let (media, transcript) = self.transcribe(source, provider, language).await?;
        let ext = match format {
            SubtitleFormat::Srt => "srt",
            SubtitleFormat::Vtt => "vtt",
        };
        let path = match output {
            Some(o) => bugle_core::config::expand_tilde(o),
            None => media.path.with_extension(ext),
        };
        std::fs::write(&path, transcript.render(format))?;
        let mut m = Media::new(MediaKind::Subtitle, &path);
        m.parent = Some(media.id.clone());
        m.source = Some(format!("subtitles via {}", transcript.provider));
        self.history.push(&m)?;
        Ok((media, transcript, path))
    }

    // -----------------------------------------------------------------------
    // Pin / helpers
    // -----------------------------------------------------------------------

    /// Float an image above everything. Runs in-process on the main thread,
    /// otherwise spawns a detached helper process.
    pub fn pin(&self, source: &str, frame: Option<Rect>, opacity: f64, blocking: bool) -> Result<Media> {
        let (media, img) = self.load_image(source)?;
        if self.main_thread_ui && blocking {
            let mut buf = std::io::Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(img).write_to(&mut buf, image::ImageFormat::Png)?;
            bugle_platform::pin_image_blocking(buf.get_ref(), frame, opacity);
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
        }
        Ok(media)
    }

    /// Interactive rectangle selection.
    pub fn select_area(&self) -> Result<Option<Rect>> {
        if self.main_thread_ui {
            return Ok(bugle_platform::select_rect_blocking());
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
pub fn encode_png_bounded(img: &RgbaImage, max_edge: u32) -> Result<Vec<u8>> {
    let long = img.width().max(img.height());
    let scaled;
    let src = if long > max_edge {
        let s = max_edge as f64 / long as f64;
        scaled = image::imageops::resize(
            img,
            ((img.width() as f64) * s).round() as u32,
            ((img.height() as f64) * s).round() as u32,
            image::imageops::FilterType::Triangle,
        );
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
