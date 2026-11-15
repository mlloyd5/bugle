use anyhow::{anyhow, bail, Context, Result};
use bugle_markup::{Annotation, Shape, Style};
use bugle_core::{
    BugleError, CaptureOptions, CaptureRequest, CaptureTarget, Config, DescribeRequest, ImageFormat, OcrOptions, Point, RecordRequest,
    RecordingFormat, Rect, ScrollCaptureRequest, SubtitleFormat,
};
use bugle_engine::Engine;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "bugle", version, about = "Screenshots, recordings, OCR and AI actions for macOS.", long_about = None)]
#[command(after_help = "Aliases: `bugle jameson` = `bugle serve`, `bugle parker` = `bugle mcp`.")]
pub struct Cli {
    /// Print JSON instead of human output where applicable.
    #[arg(long, global = true)]
    pub json: bool,
    /// Debug logging.
    #[arg(short, long, global = true)]
    pub verbose: bool,
    /// Config file (default ~/.config/bugle/config.toml or $BUGLE_CONFIG).
    #[arg(long, global = true, env = "BUGLE_CONFIG")]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Take a screenshot.
    Capture(CaptureArgs),
    /// Capture a long scrolling page and stitch it.
    Scroll(ScrollArgs),
    /// Record the screen (video or GIF).
    Record {
        #[command(subcommand)]
        cmd: RecordCmd,
    },
    /// Recognise text in an image (Apple Vision, or an AI provider).
    Ocr(OcrArgs),
    /// Describe an image or recording with a vision model.
    Describe(DescribeArgs),
    /// Speech-to-text for a recording.
    Transcribe(TranscribeArgs),
    /// Transcribe a recording and write SRT/VTT subtitles next to it.
    Subtitles(SubtitlesArgs),
    /// Draw annotations onto an image.
    #[command(name = "annotate")]
    Markup(MarkupArgs),
    /// Float an image above all windows until Esc / double-click.
    Pin(PinArgs),
    /// Let the user drag a rectangle; prints it.
    Select,
    /// List displays.
    Displays,
    /// List windows.
    Windows {
        /// Include off-screen and system windows.
        #[arg(long)]
        all: bool,
    },
    /// Show recent captures.
    History {
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// Remove an item from history.
    Delete {
        id: String,
        /// Also delete the file on disk.
        #[arg(long)]
        file: bool,
    },
    /// Put a capture on the clipboard.
    Copy {
        #[arg(default_value = "latest")]
        source: String,
    },
    /// Open a capture in the default app.
    Open {
        #[arg(default_value = "latest")]
        source: String,
    },
    /// Copy, move or upload a capture to the `[sync]` destination (folder or S3 bucket).
    Sync {
        #[arg(default_value = "latest")]
        source: String,
    },
    /// Plain text completion with the configured text provider.
    Ask {
        prompt: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        system: Option<String>,
    },
    /// List configured AI providers.
    Providers,
    /// Show / request permissions.
    Permissions {
        /// Prompt macOS for Screen Recording and Accessibility access.
        #[arg(long)]
        request: bool,
    },
    /// Configuration helpers.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Run the HTTP API for agents (alias: jameson).
    #[command(alias = "jameson")]
    Serve {
        #[arg(long)]
        bind: Option<String>,
        /// Bearer token required on every request.
        #[arg(long, env = "BUGLE_TOKEN")]
        token: Option<String>,
    },
    /// Run the MCP stdio server for agents (alias: parker).
    #[command(alias = "parker")]
    Mcp,
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum TargetKind {
    Screen,
    Window,
    Area,
    Interactive,
    All,
}

#[derive(Args, Debug, Clone)]
pub struct TargetArgs {
    /// What to capture (default: interactive when a TTY, screen otherwise).
    #[arg(value_enum)]
    pub kind: Option<TargetKind>,
    /// Display id (screen).
    #[arg(long)]
    pub display: Option<u32>,
    /// Window id (window).
    #[arg(long = "window-id", short = 'w')]
    pub window_id: Option<u32>,
    /// Window title substring (window).
    #[arg(long, short = 't')]
    pub title: Option<String>,
    /// Owning application substring (window).
    #[arg(long, short = 'a')]
    pub app: Option<String>,
    /// Rect `x,y,w,h` in screen points (area).
    #[arg(long, short = 'r')]
    pub rect: Option<String>,
}

impl TargetArgs {
    fn resolve(&self, default_interactive: bool) -> Result<CaptureTarget> {
        let kind = self.kind.unwrap_or({
            if self.rect.is_some() {
                TargetKind::Area
            } else if self.window_id.is_some() || self.title.is_some() || self.app.is_some() {
                TargetKind::Window
            } else if self.display.is_some() {
                TargetKind::Screen
            } else if default_interactive {
                TargetKind::Interactive
            } else {
                TargetKind::Screen
            }
        });
        Ok(match kind {
            TargetKind::Screen => CaptureTarget::Screen { display: self.display },
            TargetKind::Window => CaptureTarget::Window { id: self.window_id, title: self.title.clone(), owner: self.app.clone() },
            TargetKind::Area => CaptureTarget::Area {
                rect: Rect::parse(self.rect.as_deref().ok_or_else(|| anyhow!("--rect x,y,w,h is required for area"))?)
                    .ok_or_else(|| anyhow!("bad --rect; expected x,y,w,h"))?,
            },
            TargetKind::Interactive => CaptureTarget::Interactive,
            TargetKind::All => CaptureTarget::AllScreens,
        })
    }
}

#[derive(Args, Debug)]
pub struct CaptureArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Seconds to wait before capturing (self-timer).
    #[arg(long, short = 'd')]
    pub delay: Option<f64>,
    #[arg(long, short = 'f')]
    pub format: Option<ImageFormat>,
    #[arg(long, short = 'q')]
    pub quality: Option<u8>,
    /// Copy to the clipboard too.
    #[arg(long, short = 'c')]
    pub copy: bool,
    /// Output file or directory.
    #[arg(long, short = 'o')]
    pub output: Option<String>,
    #[arg(long, short = 'l')]
    pub label: Option<String>,
    /// Drop the window shadow.
    #[arg(long)]
    pub no_shadow: bool,
    /// Include the mouse cursor.
    #[arg(long)]
    pub cursor: bool,
    /// Run OCR afterwards and print the text.
    #[arg(long)]
    pub ocr: bool,
    /// Describe the capture with the default vision provider afterwards.
    #[arg(long)]
    pub describe: bool,
    /// Pin the result to the screen afterwards.
    #[arg(long)]
    pub pin: bool,
    /// Open the result afterwards.
    #[arg(long)]
    pub open: bool,
}

#[derive(Args, Debug)]
pub struct ScrollArgs {
    /// Rect `x,y,w,h` to scroll; omitted = drag to select.
    #[arg(long, short = 'r')]
    pub rect: Option<String>,
    #[arg(long, default_value_t = 40)]
    pub max_frames: u32,
    /// Scroll wheel lines per step (negative = down/forward).
    #[arg(long, default_value_t = -8, allow_negative_numbers = true)]
    pub step: i32,
    #[arg(long, default_value_t = 350)]
    pub settle_ms: u64,
    #[arg(long)]
    pub horizontal: bool,
    #[arg(long, short = 'o')]
    pub output: Option<String>,
    #[arg(long, short = 'c')]
    pub copy: bool,
    /// Seconds to wait before starting (time to hover the scrollable view).
    #[arg(long, short = 'd', default_value_t = 2.0)]
    pub delay: f64,
}

#[derive(Subcommand, Debug)]
pub enum RecordCmd {
    /// Start a recording; blocks until Ctrl-C (or --seconds).
    Start(RecordArgs),
    /// Explains how to stop a recording: Ctrl-C in the `record start`
    /// terminal, or the `record_stop` action of a running API/MCP server.
    Stop,
    /// Explains where recording status lives: a recording belongs to the
    /// process that started it (use the server's `record_status` action).
    Status,
}

#[derive(Args, Debug)]
pub struct RecordArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long, short = 'f', value_enum, default_value = "mp4")]
    pub format: RecordFormatArg,
    /// GIF frames per second.
    #[arg(long)]
    pub fps: Option<u32>,
    /// Record the microphone.
    #[arg(long, short = 'm')]
    pub mic: bool,
    /// Record system audio (uses the ScreenCaptureKit recorder).
    #[arg(long)]
    pub system_audio: bool,
    /// Stop automatically after N seconds.
    #[arg(long, short = 's')]
    pub seconds: Option<f64>,
    /// Highlight mouse clicks.
    #[arg(long)]
    pub clicks: bool,
    #[arg(long, short = 'o')]
    pub output: Option<String>,
    #[arg(long, short = 'l')]
    pub label: Option<String>,
    /// Generate subtitles with the default transcriber when done.
    #[arg(long)]
    pub subtitles: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum RecordFormatArg {
    Mp4,
    Mov,
    Gif,
}

#[derive(Args, Debug)]
pub struct OcrArgs {
    #[arg(default_value = "latest")]
    pub source: String,
    /// Recognition languages, e.g. en-US,de-DE.
    #[arg(long)]
    pub lang: Option<String>,
    #[arg(long)]
    pub fast: bool,
    /// `vision` or an AI provider name.
    #[arg(long)]
    pub engine: Option<String>,
    /// Restrict to a sub-rect of the image `x,y,w,h` (pixels).
    #[arg(long)]
    pub region: Option<String>,
    /// Copy the text to the clipboard.
    #[arg(long, short = 'c')]
    pub copy: bool,
}

#[derive(Args, Debug)]
pub struct DescribeArgs {
    #[arg(default_value = "latest")]
    pub source: String,
    #[arg(long, short = 'p')]
    pub prompt: Option<String>,
    #[arg(long)]
    pub provider: Option<String>,
    /// Add OCR text to the prompt.
    #[arg(long)]
    pub with_ocr: bool,
}

#[derive(Args, Debug)]
pub struct TranscribeArgs {
    #[arg(default_value = "latest")]
    pub source: String,
    #[arg(long)]
    pub provider: Option<String>,
    #[arg(long)]
    pub language: Option<String>,
}

#[derive(Args, Debug)]
pub struct SubtitlesArgs {
    #[arg(default_value = "latest")]
    pub source: String,
    #[arg(long, short = 'f', default_value = "srt")]
    pub format: String,
    #[arg(long)]
    pub provider: Option<String>,
    #[arg(long)]
    pub language: Option<String>,
    #[arg(long, short = 'o')]
    pub output: Option<String>,
}

#[derive(Args, Debug)]
pub struct MarkupArgs {
    #[arg(default_value = "latest")]
    pub source: String,
    /// Annotation JSON document (file path, or `-` for stdin).
    #[arg(long, short = 's')]
    pub spec: Option<String>,
    /// Quick arrow `x1,y1,x2,y2` (repeatable).
    #[arg(long)]
    pub arrow: Vec<String>,
    /// Quick rectangle `x,y,w,h` (repeatable).
    #[arg(long = "box")]
    pub rect: Vec<String>,
    /// Quick text `x,y:Your text` (repeatable).
    #[arg(long)]
    pub text: Vec<String>,
    /// Quick redaction `x,y,w,h` (repeatable; pixelate).
    #[arg(long)]
    pub redact: Vec<String>,
    /// Quick highlight `x,y,w,h` (repeatable).
    #[arg(long)]
    pub highlight: Vec<String>,
    /// Quick numbered counter `x,y` (repeatable).
    #[arg(long)]
    pub counter: Vec<String>,
    /// Crop `x,y,w,h` first.
    #[arg(long)]
    pub crop: Option<String>,
    /// Background canvas preset (sunset, ocean, forest, candy, mono, midnight, #hex, gradient:#a,#b).
    #[arg(long)]
    pub canvas: Option<String>,
    #[arg(long)]
    pub padding: Option<u32>,
    #[arg(long)]
    pub color: Option<String>,
    #[arg(long)]
    pub stroke: Option<f32>,
    #[arg(long, short = 'o')]
    pub output: Option<String>,
    #[arg(long, short = 'c')]
    pub copy: bool,
    #[arg(long)]
    pub open: bool,
    /// Print the example schema and exit.
    #[arg(long)]
    pub schema: bool,
}

#[derive(Args, Debug)]
pub struct PinArgs {
    #[arg(default_value = "latest")]
    pub source: String,
    /// Frame `x,y,w,h` in screen points.
    #[arg(long)]
    pub at: Option<String>,
    #[arg(long, default_value_t = 1.0)]
    pub opacity: f64,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Write a default config file (does not overwrite).
    Init,
    /// Print the effective configuration (secrets shown as "***").
    Show {
        /// Print tokens, API keys and auth headers in the clear.
        #[arg(long)]
        reveal: bool,
    },
    /// Print the config file path.
    Path,
}

fn load_config(path: Option<&PathBuf>) -> Result<Config> {
    match path {
        Some(p) => Config::load_from(p),
        None => Config::load(),
    }
}

fn parse_rect(s: &str) -> Result<Rect> {
    Rect::parse(s).ok_or_else(|| anyhow!("bad rect '{s}'; expected x,y,w,h"))
}

/// Pin frames follow the platform convention: a negative origin means "top
/// right of the main screen" and a zero size means "the image's own size",
/// so unlike capture rects they may be empty.
fn parse_pin_frame(s: &str) -> Result<Rect> {
    let v: Vec<f64> = s
        .split(',')
        .map(|p| p.trim().parse::<f64>().ok().filter(|n| n.is_finite()))
        .collect::<Option<_>>()
        .ok_or_else(|| anyhow!("bad frame '{s}'; expected x,y,w,h"))?;
    match v.as_slice() {
        [x, y, w, h] if *w >= 0.0 && *h >= 0.0 => Ok(Rect::new(*x, *y, *w, *h)),
        _ => bail!("bad frame '{s}'; expected x,y,w,h"),
    }
}

fn parse_point(s: &str) -> Result<Point> {
    let parts: Vec<f64> = s.split(',').map(|p| p.trim().parse()).collect::<Result<_, _>>().map_err(|_| anyhow!("bad point '{s}'"))?;
    if parts.len() != 2 {
        bail!("bad point '{s}'; expected x,y");
    }
    Ok(Point { x: parts[0], y: parts[1] })
}

pub async fn run(cli: Cli) -> Result<()> {
    let config = load_config(cli.config.as_ref())?;
    let json = cli.json;

    // Commands that need the AppKit overlay in-process are run with
    // main_thread_ui = true; servers spawn helpers instead.
    let main_thread = !matches!(cli.cmd, Cmd::Serve { .. } | Cmd::Mcp);
    let engine = Arc::new(Engine::new(config, main_thread)?);

    match cli.cmd {
        Cmd::Capture(a) => {
            let target = a.target.resolve(atty_stdout())?;
            let req = CaptureRequest {
                target,
                options: CaptureOptions {
                    shadow: a.no_shadow.then_some(false),
                    delay: a.delay,
                    format: a.format,
                    quality: a.quality,
                    copy: a.copy.then_some(true),
                    output: a.output,
                    label: a.label,
                    hide_cursor: Some(!a.cursor),
                },
            };
            let e = engine.clone();
            let out = e.capture(&req)?;
            let mut extra = json!({});
            if a.ocr {
                let r = engine.ocr_image_any(out.image.clone(), &OcrOptions::default()).await?;
                extra["ocr"] = json!(r);
                if !json {
                    println!("{}", r.text);
                }
            }
            if a.describe {
                let r = engine.describe(&DescribeRequest { source: out.item.id.clone(), ..Default::default() }).await?;
                extra["description"] = json!(r);
                if !json {
                    println!("{}", r.description);
                }
            }
            if json {
                println!("{}", json!({"media": out.item, "rect": out.rect, "window": out.window, "extra": extra}));
            } else {
                println!("{}  [{}]  {}x{}", out.item.path.display(), out.item.id, out.image.width(), out.image.height());
                if let Some(url) = &out.item.url {
                    println!("{url}");
                }
            }
            if a.open {
                bugle_platform::open_path(&out.item.path);
            }
            if a.pin {
                engine.pin(&out.item.id, None, 1.0, true)?;
            }
        }
        Cmd::Scroll(a) => {
            let rect = match a.rect {
                Some(r) => parse_rect(&r)?,
                None => engine.select_area()?.ok_or(BugleError::Cancelled)?,
            };
            bugle_engine::capture::validate_rect(&rect)?;
            if a.delay > 0.0 {
                eprintln!("starting in {:.0}s — keep the mouse over the scrollable area", a.delay);
                tokio::time::sleep(std::time::Duration::from_secs_f64(a.delay)).await;
            }
            let req = ScrollCaptureRequest {
                rect,
                max_frames: a.max_frames,
                step_lines: a.step,
                settle_ms: a.settle_ms,
                horizontal: a.horizontal,
                options: CaptureOptions { output: a.output, copy: a.copy.then_some(true), ..Default::default() },
            };
            let (out, stats) = engine.scroll_capture(&req)?;
            if json {
                println!("{}", json!({"media": out.item, "stats": stats}));
            } else {
                println!(
                    "{}  [{}]  {}x{}  ({} frames, {})",
                    out.item.path.display(),
                    out.item.id,
                    out.image.width(),
                    out.image.height(),
                    stats.frames,
                    stats.stopped_reason
                );
            }
        }
        Cmd::Record { cmd } => match cmd {
            RecordCmd::Start(a) => {
                let target = a.target.resolve(atty_stdout())?;
                let req = RecordRequest {
                    target,
                    format: match a.format {
                        RecordFormatArg::Mp4 => RecordingFormat::Mp4,
                        RecordFormatArg::Mov => RecordingFormat::Mov,
                        RecordFormatArg::Gif => RecordingFormat::Gif,
                    },
                    fps: a.fps,
                    microphone: a.mic,
                    system_audio: a.system_audio,
                    max_seconds: a.seconds,
                    highlight_clicks: a.clicks,
                    output: a.output,
                    label: a.label,
                };
                let status = engine.record_start(&req)?;
                eprintln!("recording → {}  (Ctrl-C to stop)", status.output.as_deref().unwrap_or("?"));
                match a.seconds {
                    Some(s) => {
                        let deadline = tokio::time::sleep(std::time::Duration::from_secs_f64(s + 0.2));
                        tokio::select! { _ = deadline => {}, _ = tokio::signal::ctrl_c() => {} }
                    }
                    None => {
                        tokio::signal::ctrl_c().await?;
                    }
                }
                let media = engine.record_stop()?;
                if a.subtitles {
                    match engine.subtitles(&media.id, SubtitleFormat::Srt, None, None, None).await {
                        Ok((_, t, p)) => eprintln!("subtitles → {} ({} segments)", p.display(), t.segments.len()),
                        Err(e) => eprintln!("subtitles failed: {e:#}"),
                    }
                }
                if json {
                    println!("{}", json!({"media": media}));
                } else {
                    println!("{}  [{}]  {:.1}s", media.path.display(), media.id, media.duration_seconds.unwrap_or(0.0));
                }
            }
            RecordCmd::Stop => {
                bail!("`record stop` only applies to recordings started through the API server; press Ctrl-C in the recording terminal instead")
            }
            RecordCmd::Status => {
                // A recording lives in the process that started it, so a fresh
                // CLI process never has one; say so instead of a bare "idle".
                const NOTE: &str = "recordings belong to the process that started them: `bugle record start` runs until Ctrl-C in its terminal, and recordings started through the API/MCP server are queried with its `record_status` action";
                if json {
                    println!("{}", json!({"status": engine.record_status(), "note": NOTE}));
                } else {
                    println!("no recording in this process; {NOTE}");
                }
            }
        },
        Cmd::Ocr(a) => {
            let opts = OcrOptions {
                languages: a.lang.map(|l| l.split(',').map(|s| s.trim().to_string()).collect()),
                fast: a.fast.then_some(true),
                engine: a.engine,
                region: a.region.as_deref().map(parse_rect).transpose()?,
            };
            let (media, r) = engine.run_ocr(&a.source, &opts).await?;
            if a.copy {
                bugle_platform::clipboard_write_text(&r.text)?;
            }
            if json {
                println!("{}", json!({"media_id": media.id, "text": r.text, "lines": r.lines, "engine": r.engine}));
            } else {
                println!("{}", r.text);
            }
        }
        Cmd::Describe(a) => {
            let r = engine
                .describe(&DescribeRequest { source: a.source, prompt: a.prompt, provider: a.provider, with_ocr: a.with_ocr })
                .await?;
            if json {
                println!("{}", json!(r));
            } else {
                println!("{}", r.description);
                eprintln!("— {} / {}", r.provider, r.model);
            }
        }
        Cmd::Transcribe(a) => {
            let (media, t) = engine.transcribe(&a.source, a.provider.as_deref(), a.language.as_deref()).await?;
            if json {
                println!("{}", json!({"media_id": media.id, "transcript": t}));
            } else {
                println!("{}", t.to_srt());
            }
        }
        Cmd::Subtitles(a) => {
            let format = match a.format.to_ascii_lowercase().as_str() {
                "srt" => SubtitleFormat::Srt,
                "vtt" => SubtitleFormat::Vtt,
                other => bail!("unknown subtitle format {other}"),
            };
            let (media, t, path) =
                engine.subtitles(&a.source, format, a.provider.as_deref(), a.language.as_deref(), a.output.as_deref()).await?;
            if json {
                println!("{}", json!({"media_id": media.id, "path": path, "segments": t.segments.len()}));
            } else {
                println!("{}  ({} segments via {})", path.display(), t.segments.len(), t.provider);
            }
        }
        Cmd::Markup(a) => {
            if a.schema {
                println!("{}", serde_json::to_string_pretty(&bugle_markup::schema_description())?);
                return Ok(());
            }
            let mut doc = match a.spec.as_deref() {
                Some("-") => {
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                    Annotation::from_json(&s)?
                }
                Some(p) => Annotation::from_json(&std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?)?,
                None => Annotation::default(),
            };
            let st = Style::default();
            for s in &a.arrow {
                let v: Vec<f64> = s.split(',').map(|p| p.trim().parse()).collect::<Result<_, _>>()?;
                if v.len() != 4 {
                    bail!("--arrow needs x1,y1,x2,y2");
                }
                doc.shapes.push(Shape::Arrow {
                    from: Point { x: v[0], y: v[1] },
                    to: Point { x: v[2], y: v[3] },
                    arrow: Default::default(),
                    style: st.clone(),
                });
            }
            for s in &a.rect {
                doc.shapes.push(Shape::Rectangle { rect: parse_rect(s)?, radius: 4.0, style: st.clone() });
            }
            for s in &a.text {
                let (pos, text) = s.split_once(':').ok_or_else(|| anyhow!("--text needs x,y:text"))?;
                doc.shapes.push(Shape::Text { at: parse_point(pos)?, text: text.to_string(), background: None, style: st.clone() });
            }
            for s in &a.redact {
                doc.shapes.push(Shape::Redact { rect: parse_rect(s)?, mode: Default::default(), strength: 0.0 });
            }
            for s in &a.highlight {
                doc.shapes.push(Shape::Highlight { rect: parse_rect(s)?, style: Style::default() });
            }
            for s in &a.counter {
                doc.shapes.push(Shape::Counter { at: parse_point(s)?, number: None, style: st.clone() });
            }
            if let Some(c) = &a.crop {
                doc.crop = Some(parse_rect(c)?);
            }
            if let Some(c) = &a.color {
                doc.style.color = Some(c.parse().map_err(|e: String| anyhow!(e))?);
            }
            if let Some(w) = a.stroke {
                doc.style.stroke_width = Some(w);
            }
            if a.canvas.is_some() || a.padding.is_some() {
                let mut canvas = doc.canvas.take().unwrap_or_default();
                if let Some(c) = &a.canvas {
                    canvas.background = bugle_markup::Background::Named(c.clone());
                }
                if let Some(p) = a.padding {
                    canvas.padding = p;
                }
                doc.canvas = Some(canvas);
            }
            if doc.shapes.is_empty() && doc.crop.is_none() && doc.canvas.is_none() {
                bail!("nothing to do: pass --spec, or quick flags like --arrow/--box/--text/--redact/--canvas");
            }
            let (media, img) = engine.apply_annotations(&a.source, doc, a.output.as_deref(), a.copy)?;
            if json {
                println!("{}", json!({"media": media}));
            } else {
                println!("{}  [{}]  {}x{}", media.path.display(), media.id, img.width(), img.height());
            }
            if a.open {
                bugle_platform::open_path(&media.path);
            }
        }
        Cmd::Pin(a) => {
            let frame = a.at.as_deref().map(parse_pin_frame).transpose()?;
            engine.pin(&a.source, frame, a.opacity, true)?;
        }
        Cmd::Select => {
            let r = bugle_platform::select_rect_blocking();
            if json {
                println!("{}", json!(r));
            } else {
                match r {
                    Some(r) => println!("{r}"),
                    None => std::process::exit(1),
                }
            }
        }
        Cmd::Displays => {
            let d = engine.displays()?;
            if json {
                println!("{}", json!(d));
            } else {
                for d in d {
                    println!("{:>4}  {:>5}x{:<5}  @{:.0},{:.0}  scale {:.1}{}", d.id, d.frame.w, d.frame.h, d.frame.x, d.frame.y, d.scale, if d.is_main { "  (main)" } else { "" });
                }
            }
        }
        Cmd::Windows { all } => {
            let w = engine.windows(all)?;
            if json {
                println!("{}", json!(w));
            } else {
                for w in w {
                    println!("{:>6}  {:<22}  {:<50}  {}", w.id, trunc(&w.owner, 22), trunc(&w.title, 50), w.frame);
                }
            }
        }
        Cmd::History { limit } => {
            let items = engine.history.recent(limit)?;
            if json {
                println!("{}", json!(items));
            } else {
                for m in items {
                    println!(
                        "{}  {}  {:<18}  {}",
                        m.id,
                        m.created_at.with_timezone(&chrono_local()).format("%Y-%m-%d %H:%M:%S"),
                        format!("{:?}", m.kind).to_lowercase(),
                        m.path.display()
                    );
                }
            }
        }
        Cmd::Delete { id, file } => {
            let ok = engine.history.delete(&id, file)?;
            println!("{}", if ok { "deleted" } else { "not found" });
        }
        Cmd::Copy { source } => {
            let m = engine.copy_media(&source)?;
            println!("copied {}", m.path.display());
        }
        Cmd::Open { source } => {
            let m = engine.resolve_media(&source)?;
            bugle_platform::open_path(&m.path);
        }
        Cmd::Sync { source } => {
            let e = engine.clone();
            let (media, report) = tokio::task::spawn_blocking(move || e.sync_media(&source)).await??;
            if json {
                println!("{}", json!({"media": media, "sync": report}));
            } else {
                println!("{} {}  →  {}", report.action, media.path.display(), report.destination);
            }
        }
        Cmd::Ask { prompt, provider, system } => {
            println!("{}", engine.complete_text(&prompt, provider.as_deref(), system.as_deref()).await?);
        }
        Cmd::Providers => {
            let v = engine.ai.describe_all();
            if json {
                println!("{}", json!(v));
            } else {
                for p in v {
                    println!(
                        "{:<12} {:<12} {:<28} {:<40} roles: {}{}",
                        p["name"].as_str().unwrap_or(""),
                        p["kind"].as_str().unwrap_or(""),
                        p["model"].as_str().unwrap_or(""),
                        p["base_url"].as_str().unwrap_or(""),
                        p["roles"].as_array().map(|r| r.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_default(),
                        p["default_for"].as_array().filter(|a| !a.is_empty()).map(|a| format!("  (default {})", a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","))).unwrap_or_default()
                    );
                }
            }
        }
        Cmd::Permissions { request } => {
            if request {
                bugle_platform::screen_access_request();
                bugle_platform::accessibility_trusted(true);
            }
            println!("{}", serde_json::to_string_pretty(&engine.permissions())?);
        }
        Cmd::Config { cmd } => {
            let path = cli.config.clone().unwrap_or_else(Config::default_path);
            match cmd {
                ConfigCmd::Init => {
                    if path.exists() {
                        println!("{} already exists", path.display());
                    } else {
                        Config::default().save_to(&path)?;
                        println!("wrote {}", path.display());
                    }
                }
                ConfigCmd::Show { reveal } => println!("{}", toml_pretty(&engine.config, reveal)?),
                ConfigCmd::Path => println!("{}", path.display()),
            }
        }
        Cmd::Serve { bind, token } => {
            let bind = bind.unwrap_or_else(|| engine.config.api.bind.clone());
            let token = token.or_else(|| engine.config.api.token.clone());
            let cors = engine.config.api.cors;
            bugle_api::http::serve(engine, &bind, token, cors).await?;
        }
        Cmd::Mcp => {
            bugle_api::mcp::serve(engine).await?;
        }
    }
    Ok(())
}

fn atty_stdout() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    } else {
        s.to_string()
    }
}

fn chrono_local() -> chrono::Local {
    chrono::Local
}

fn toml_pretty(c: &Config, reveal: bool) -> Result<String> {
    let mut v = toml::Value::try_from(c)?;
    if !reveal {
        redact_secrets(&mut v, false);
    }
    Ok(toml::to_string_pretty(&v)?)
}

/// Replace secret values with `"***"`: `token`, `api_key` and similar keys
/// anywhere, and credential-looking entries inside `headers` tables. Names of
/// environment variables (`*_env`) are not secrets and are kept.
fn redact_secrets(v: &mut toml::Value, in_headers: bool) {
    match v {
        toml::Value::Table(t) => {
            for (k, val) in t.iter_mut() {
                let key = k.to_ascii_lowercase();
                let secret = if in_headers {
                    ["auth", "key", "token", "secret", "cookie", "password"].iter().any(|s| key.contains(s))
                } else {
                    !key.ends_with("_env")
                        && (matches!(key.as_str(), "token" | "password" | "secret")
                            || key.ends_with("api_key")
                            || key.ends_with("_token")
                            || key.ends_with("_secret")
                            || key.ends_with("secret_key")
                            || key.ends_with("_password"))
                };
                if secret && val.is_str() {
                    *val = toml::Value::String("***".into());
                } else {
                    redact_secrets(val, key == "headers");
                }
            }
        }
        toml::Value::Array(a) => a.iter_mut().for_each(|x| redact_secrets(x, in_headers)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_scroll_step_parses() {
        let cli = Cli::try_parse_from(["bugle", "scroll", "--step", "-8", "--rect", "0,0,10,10"]).unwrap();
        match cli.cmd {
            Cmd::Scroll(a) => assert_eq!(a.step, -8),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn config_show_reveal_flag() {
        let cli = Cli::try_parse_from(["bugle", "config", "show", "--reveal"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Config { cmd: ConfigCmd::Show { reveal: true } }));
        let cli = Cli::try_parse_from(["bugle", "config", "show"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Config { cmd: ConfigCmd::Show { reveal: false } }));
    }

    #[test]
    fn redacts_secrets() {
        let src = r#"
            [api]
            token = "tok-123"
            bind = "127.0.0.1:7070"
            [ai.providers.remote]
            api_key = "sk-abc"
            api_key_env = "REMOTE_KEY"
            max_tokens = 1024
            [ai.providers.remote.headers]
            Authorization = "Bearer xyz"
            X-Title = "bugle"
            [sync]
            secret_key_env = "AWS_SECRET_ACCESS_KEY"
        "#;
        let mut v: toml::Value = toml::from_str(src).unwrap();
        redact_secrets(&mut v, false);
        assert_eq!(v["api"]["token"].as_str(), Some("***"));
        assert_eq!(v["api"]["bind"].as_str(), Some("127.0.0.1:7070"));
        let p = &v["ai"]["providers"]["remote"];
        assert_eq!(p["api_key"].as_str(), Some("***"));
        assert_eq!(p["api_key_env"].as_str(), Some("REMOTE_KEY"));
        assert_eq!(p["max_tokens"].as_integer(), Some(1024));
        assert_eq!(p["headers"]["Authorization"].as_str(), Some("***"));
        assert_eq!(p["headers"]["X-Title"].as_str(), Some("bugle"));
        assert_eq!(v["sync"]["secret_key_env"].as_str(), Some("AWS_SECRET_ACCESS_KEY"));

        // The real config round-trips through the redacting printer.
        let mut c = Config::default();
        c.api.token = Some("tok-123".into());
        let shown = toml_pretty(&c, false).unwrap();
        assert!(!shown.contains("tok-123") && shown.contains("***"));
        assert!(toml_pretty(&c, true).unwrap().contains("tok-123"));
    }
}
