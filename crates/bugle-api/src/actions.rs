//! The action catalog: every agent-callable operation with its JSON schema.

use anyhow::{anyhow, Result};
use bugle_core::{CaptureRequest, DescribeRequest, OcrOptions, RecordRequest, ScrollCaptureRequest, SubtitleFormat};
use bugle_engine::Engine;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct ActionOutput {
    pub json: Value,
    /// PNG bytes to attach (MCP image content / HTTP inline base64).
    pub image: Option<Vec<u8>>,
}

impl ActionOutput {
    fn json(v: Value) -> Self {
        Self { json: v, image: None }
    }
}

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

fn rect_schema() -> Value {
    json!({"type":"object","properties":{"x":{"type":"number"},"y":{"type":"number"},"w":{"type":"number"},"h":{"type":"number"}},"required":["x","y","w","h"]})
}

fn target_schema() -> Value {
    json!({
        "type":"object",
        "description":"What to capture. kind=screen (optional display id), window (id, or title/owner substring; omit all for the front-most window), area (rect in screen points), interactive (user drags a selection), all_screens.",
        "properties":{
            "kind":{"type":"string","enum":["screen","window","area","interactive","all_screens"]},
            "display":{"type":"integer"},
            "id":{"type":"integer"},
            "title":{"type":"string"},
            "owner":{"type":"string"},
            "rect":rect_schema()
        },
        "required":["kind"]
    })
}

pub fn catalog() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "capture",
            description: "Take a screenshot of a screen, window or area. Saves it, records it in history and returns the media id (and the image unless inline_image=false).",
            input_schema: json!({
                "type":"object",
                "properties":{
                    "target": target_schema(),
                    "delay":{"type":"number","description":"seconds to wait first"},
                    "format":{"type":"string","enum":["png","jpeg","webp","tiff"]},
                    "quality":{"type":"integer"},
                    "copy":{"type":"boolean","description":"also put on the clipboard"},
                    "output":{"type":"string","description":"explicit file path"},
                    "label":{"type":"string"},
                    "shadow":{"type":"boolean","description":"keep window shadow"},
                    "hide_cursor":{"type":"boolean"},
                    "inline_image":{"type":"boolean","default":true},
                    "max_edge":{"type":"integer","description":"downscale the inline image to this long edge (default 1600)"}
                }
            }),
        },
        ToolSpec {
            name: "list_displays",
            description: "List connected displays with their frames (points) and scale factors.",
            input_schema: json!({"type":"object","properties":{}}),
        },
        ToolSpec {
            name: "list_windows",
            description: "List on-screen application windows (id, owner app, title, frame). Front-most first.",
            input_schema: json!({"type":"object","properties":{"all":{"type":"boolean","description":"include off-screen and system windows"}}}),
        },
        ToolSpec {
            name: "ocr",
            description: "Recognise text in a screenshot with Apple Vision. Returns plain text plus per-line bounding boxes (pixels). source = media id, 'latest', or a file path.",
            input_schema: json!({
                "type":"object",
                "properties":{
                    "source":{"type":"string","default":"latest"},
                    "languages":{"type":"array","items":{"type":"string"}},
                    "fast":{"type":"boolean"},
                    "region":rect_schema(),
                    "engine":{"type":"string","description":"'vision' (default) or an AI provider name"}
                }
            }),
        },
        ToolSpec {
            name: "describe",
            description: "Describe a screenshot (or sampled frames of a recording) with a vision model.",
            input_schema: json!({
                "type":"object",
                "properties":{
                    "source":{"type":"string","default":"latest"},
                    "prompt":{"type":"string"},
                    "provider":{"type":"string","description":"provider name from the config; default vision provider when omitted"},
                    "with_ocr":{"type":"boolean","description":"ground the model with OCR text"}
                }
            }),
        },
        ToolSpec {
            name: "annotate",
            description: "Draw annotations (arrows, boxes, text, counters, redactions, highlights, spotlight, crop, background canvas) onto an image and save the result as a new media item. See annotate_schema for the document format.",
            input_schema: json!({
                "type":"object",
                "properties":{
                    "source":{"type":"string","default":"latest"},
                    "annotation":{"type":"object","description":"Annotation document (see annotate_schema)"},
                    "output":{"type":"string"},
                    "copy":{"type":"boolean"},
                    "inline_image":{"type":"boolean","default":true}
                },
                "required":["annotation"]
            }),
        },
        ToolSpec {
            name: "annotate_schema",
            description: "Return an example annotation document showing every shape type and option.",
            input_schema: json!({"type":"object","properties":{}}),
        },
        ToolSpec {
            name: "scroll_capture",
            description: "Capture a long scrolling page: scrolls the given screen rect step by step and stitches the frames (Vision image registration). Needs Accessibility permission.",
            input_schema: json!({
                "type":"object",
                "properties":{
                    "rect":rect_schema(),
                    "max_frames":{"type":"integer","default":40},
                    "step_lines":{"type":"integer","default":-8},
                    "settle_ms":{"type":"integer","default":350},
                    "horizontal":{"type":"boolean"},
                    "output":{"type":"string"},
                    "inline_image":{"type":"boolean","default":false}
                },
                "required":["rect"]
            }),
        },
        ToolSpec {
            name: "record_start",
            description: "Start a screen recording (mp4/mov via screencapture or, with system_audio, the ScreenCaptureKit recorder; gif via a frame grab loop). Only one recording at a time.",
            input_schema: json!({
                "type":"object",
                "properties":{
                    "target": target_schema(),
                    "format":{"type":"string","enum":["mp4","mov","gif"],"default":"mp4"},
                    "fps":{"type":"integer","description":"gif only"},
                    "microphone":{"type":"boolean"},
                    "system_audio":{"type":"boolean","description":"capture system audio (ScreenCaptureKit backend)"},
                    "max_seconds":{"type":"number","description":"auto-stop"},
                    "highlight_clicks":{"type":"boolean"},
                    "output":{"type":"string"},
                    "label":{"type":"string"}
                }
            }),
        },
        ToolSpec {
            name: "record_stop",
            description: "Stop the active recording and return its media record.",
            input_schema: json!({"type":"object","properties":{}}),
        },
        ToolSpec {
            name: "record_status",
            description: "Report whether a recording is running, for how long, and how many frames were captured.",
            input_schema: json!({"type":"object","properties":{}}),
        },
        ToolSpec {
            name: "transcribe",
            description: "Speech-to-text for a recording or audio file using the configured transcription provider (whisper.cpp, OpenAI-compatible, or a local command). Returns timed segments.",
            input_schema: json!({
                "type":"object",
                "properties":{
                    "source":{"type":"string","default":"latest"},
                    "provider":{"type":"string"},
                    "language":{"type":"string","description":"BCP-47/ISO code hint"}
                }
            }),
        },
        ToolSpec {
            name: "subtitles",
            description: "Transcribe a recording and write an SRT or VTT sidecar next to it.",
            input_schema: json!({
                "type":"object",
                "properties":{
                    "source":{"type":"string","default":"latest"},
                    "format":{"type":"string","enum":["srt","vtt"],"default":"srt"},
                    "provider":{"type":"string"},
                    "language":{"type":"string"},
                    "output":{"type":"string"}
                }
            }),
        },
        ToolSpec {
            name: "complete",
            description: "Plain text completion with the configured text provider (e.g. summarise OCR output).",
            input_schema: json!({"type":"object","properties":{"prompt":{"type":"string"},"provider":{"type":"string"},"system":{"type":"string"}},"required":["prompt"]}),
        },
        ToolSpec {
            name: "history",
            description: "List recent captures (newest first).",
            input_schema: json!({"type":"object","properties":{"limit":{"type":"integer","default":20}}}),
        },
        ToolSpec {
            name: "get_media",
            description: "Return metadata for a media item, and the image itself for screenshots (inline_image).",
            input_schema: json!({"type":"object","properties":{"source":{"type":"string","default":"latest"},"inline_image":{"type":"boolean","default":true},"max_edge":{"type":"integer"}}}),
        },
        ToolSpec {
            name: "delete_media",
            description: "Remove a media item from history, optionally deleting the file.",
            input_schema: json!({"type":"object","properties":{"id":{"type":"string"},"delete_file":{"type":"boolean","default":false}},"required":["id"]}),
        },
        ToolSpec {
            name: "pin",
            description: "Float an image above all windows (pin to screen). Esc/double-click closes it.",
            input_schema: json!({"type":"object","properties":{"source":{"type":"string","default":"latest"},"frame":rect_schema(),"opacity":{"type":"number"}}}),
        },
        ToolSpec {
            name: "copy",
            description: "Put a media item on the clipboard (image data for screenshots, file reference otherwise).",
            input_schema: json!({"type":"object","properties":{"source":{"type":"string","default":"latest"}}}),
        },
        ToolSpec {
            name: "select_area",
            description: "Ask the user to drag a rectangle on screen; returns it in screen points. Use it before area captures or recordings when the region is unknown.",
            input_schema: json!({"type":"object","properties":{}}),
        },
        ToolSpec {
            name: "providers",
            description: "List configured AI providers and which roles (vision/text/transcriber) each supports.",
            input_schema: json!({"type":"object","properties":{}}),
        },
        ToolSpec {
            name: "permissions",
            description: "Report Screen Recording and Accessibility permission state.",
            input_schema: json!({"type":"object","properties":{}}),
        },
    ]
}

fn source_of(args: &Value) -> String {
    args.get("source").and_then(|s| s.as_str()).unwrap_or("latest").to_string()
}

fn inline_image(args: &Value, default: bool) -> bool {
    args.get("inline_image").and_then(|v| v.as_bool()).unwrap_or(default)
}

fn max_edge(args: &Value) -> u32 {
    args.get("max_edge").and_then(|v| v.as_u64()).unwrap_or(1600) as u32
}

fn image_bytes(img: &image::RgbaImage, max_edge: u32) -> Result<Vec<u8>> {
    bugle_engine::encode_png_bounded(img, max_edge)
}

/// Run one action. Blocking engine calls are moved to the blocking pool.
pub async fn run(engine: &Arc<Engine>, name: &str, args: Value) -> Result<ActionOutput> {
    let e = engine.clone();
    match name {
        "capture" => {
            let mut req: CaptureRequest = serde_json::from_value(args.clone())?;
            if req.target == Default::default() && args.get("target").is_none() {
                req.target = bugle_core::CaptureTarget::Screen { display: None };
            }
            let want = inline_image(&args, true);
            let me = max_edge(&args);
            let out = tokio::task::spawn_blocking(move || e.capture(&req)).await??;
            let image = if want { Some(image_bytes(&out.image, me)?) } else { None };
            Ok(ActionOutput { json: json!({"media": out.item, "rect": out.rect, "window": out.window}), image })
        }
        "list_displays" => Ok(ActionOutput::json(json!(tokio::task::spawn_blocking(move || e.displays()).await??))),
        "list_windows" => {
            let all = args.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
            Ok(ActionOutput::json(json!(tokio::task::spawn_blocking(move || e.windows(all)).await??)))
        }
        "ocr" => {
            let source = source_of(&args);
            let opts: OcrOptions = serde_json::from_value(args.clone())?;
            if let Some(engine_name) = opts.engine.as_deref().filter(|n| *n != "vision") {
                let r = e.ocr_with_ai(&source, engine_name).await?;
                return Ok(ActionOutput::json(json!(r)));
            }
            let (media, r) = tokio::task::spawn_blocking(move || e.ocr(&source, &opts)).await??;
            Ok(ActionOutput::json(json!({"media_id": media.id, "text": r.text, "lines": r.lines, "engine": r.engine})))
        }
        "describe" => {
            let mut req: DescribeRequest = serde_json::from_value(args.clone())?;
            if req.source.is_empty() {
                req.source = "latest".into();
            }
            let r = e.describe(&req).await?;
            Ok(ActionOutput::json(json!(r)))
        }
        "annotate" => {
            let source = source_of(&args);
            let doc: bugle_markup::Annotation =
                serde_json::from_value(args.get("annotation").cloned().ok_or_else(|| anyhow!("annotation is required"))?)?;
            let output = args.get("output").and_then(|v| v.as_str()).map(|s| s.to_string());
            let copy = args.get("copy").and_then(|v| v.as_bool()).unwrap_or(false);
            let want = inline_image(&args, true);
            let me = max_edge(&args);
            let (media, img) = tokio::task::spawn_blocking(move || e.apply_annotations(&source, doc, output.as_deref(), copy)).await??;
            let image = if want { Some(image_bytes(&img, me)?) } else { None };
            Ok(ActionOutput { json: json!({"media": media}), image })
        }
        "annotate_schema" => Ok(ActionOutput::json(bugle_markup::schema_description())),
        "scroll_capture" => {
            let req: ScrollCaptureRequest = serde_json::from_value(args.clone())?;
            let want = inline_image(&args, false);
            let me = max_edge(&args);
            let (out, stats) = tokio::task::spawn_blocking(move || e.scroll_capture(&req)).await??;
            let image = if want { Some(image_bytes(&out.image, me)?) } else { None };
            Ok(ActionOutput { json: json!({"media": out.item, "stats": stats}), image })
        }
        "record_start" => {
            let req: RecordRequest = serde_json::from_value(args)?;
            Ok(ActionOutput::json(json!(tokio::task::spawn_blocking(move || e.record_start(&req)).await??)))
        }
        "record_stop" => Ok(ActionOutput::json(json!({"media": tokio::task::spawn_blocking(move || e.record_stop()).await??}))),
        "record_status" => Ok(ActionOutput::json(json!(e.record_status()))),
        "transcribe" => {
            let source = source_of(&args);
            let provider = args.get("provider").and_then(|v| v.as_str()).map(|s| s.to_string());
            let language = args.get("language").and_then(|v| v.as_str()).map(|s| s.to_string());
            let (media, t) = e.transcribe(&source, provider.as_deref(), language.as_deref()).await?;
            Ok(ActionOutput::json(json!({"media_id": media.id, "transcript": t, "srt": t.to_srt()})))
        }
        "subtitles" => {
            let source = source_of(&args);
            let format: SubtitleFormat = args.get("format").cloned().map(serde_json::from_value).transpose()?.unwrap_or_default();
            let provider = args.get("provider").and_then(|v| v.as_str()).map(|s| s.to_string());
            let language = args.get("language").and_then(|v| v.as_str()).map(|s| s.to_string());
            let output = args.get("output").and_then(|v| v.as_str()).map(|s| s.to_string());
            let (media, t, path) = e.subtitles(&source, format, provider.as_deref(), language.as_deref(), output.as_deref()).await?;
            Ok(ActionOutput::json(json!({"media_id": media.id, "path": path, "segments": t.segments.len(), "text": t.text})))
        }
        "complete" => {
            let prompt = args.get("prompt").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("prompt is required"))?;
            let provider = args.get("provider").and_then(|v| v.as_str());
            let system = args.get("system").and_then(|v| v.as_str());
            let text = e.complete_text(prompt, provider, system).await?;
            Ok(ActionOutput::json(json!({"text": text})))
        }
        "history" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            Ok(ActionOutput::json(json!(e.history.recent(limit)?)))
        }
        "get_media" => {
            let source = source_of(&args);
            let want = inline_image(&args, true);
            let me = max_edge(&args);
            let media = e.resolve_media(&source)?;
            let image = if want && media.is_image() {
                let (_, img) = tokio::task::spawn_blocking(move || e.load_image(&source)).await??;
                Some(image_bytes(&img, me)?)
            } else {
                None
            };
            Ok(ActionOutput { json: json!(media), image })
        }
        "delete_media" => {
            let id = args.get("id").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("id is required"))?;
            let f = args.get("delete_file").and_then(|v| v.as_bool()).unwrap_or(false);
            Ok(ActionOutput::json(json!({"deleted": e.history.delete(id, f)?})))
        }
        "pin" => {
            let source = source_of(&args);
            let frame = args.get("frame").cloned().map(serde_json::from_value).transpose()?;
            let opacity = args.get("opacity").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let media = tokio::task::spawn_blocking(move || e.pin(&source, frame, opacity, false)).await??;
            Ok(ActionOutput::json(json!({"pinned": media})))
        }
        "copy" => {
            let source = source_of(&args);
            let media = tokio::task::spawn_blocking(move || {
                let m = e.resolve_media(&source)?;
                if m.is_image() {
                    let (_, img) = e.load_image(&source)?;
                    e.copy_to_clipboard(&m.path, Some(&img))?;
                } else {
                    e.copy_to_clipboard(&m.path, None)?;
                }
                Ok::<_, anyhow::Error>(m)
            })
            .await??;
            Ok(ActionOutput::json(json!({"copied": media.id})))
        }
        "select_area" => {
            let r = tokio::task::spawn_blocking(move || e.select_area()).await??;
            Ok(ActionOutput::json(json!({"rect": r})))
        }
        "providers" => Ok(ActionOutput::json(json!(e.ai.describe_all()))),
        "permissions" => Ok(ActionOutput::json(e.permissions())),
        other => Err(anyhow!("unknown action '{other}'")),
    }
}

/// HTTP status for an error.
pub fn status_for(err: &anyhow::Error) -> u16 {
    use bugle_core::BugleError as E;
    match err.downcast_ref::<E>() {
        Some(E::PermissionDenied) => 403,
        Some(E::NoSuchDisplay(_)) | Some(E::NoSuchWindow(_)) | Some(E::MediaNotFound(_)) => 404,
        Some(E::Invalid(_)) | Some(E::ProviderNotConfigured(_)) => 400,
        Some(E::Unsupported(_)) => 501,
        None => {
            if err.downcast_ref::<serde_json::Error>().is_some() {
                400
            } else {
                500
            }
        }
    }
}
