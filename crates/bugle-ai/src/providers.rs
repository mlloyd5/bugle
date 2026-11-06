//! Concrete provider implementations.

use crate::{ImageInput, Role, TextModel, Transcriber, VisionModel};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use bugle_core::config::ProviderConfig;
use bugle_core::{Transcript, TranscriptSegment};
use reqwest::Client;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    OpenAi,
    Anthropic,
    Ollama,
    WhisperCpp,
    Command,
}

impl Kind {
    fn parse(s: &str) -> Result<Kind> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "openai" | "openai_compatible" | "openai-compatible" | "llama" | "llamacpp" | "llama.cpp" | "lmstudio" | "vllm" => Kind::OpenAi,
            "anthropic" | "claude" => Kind::Anthropic,
            "ollama" => Kind::Ollama,
            "whisper_cpp" | "whisper.cpp" | "whispercpp" | "whisper" => Kind::WhisperCpp,
            "command" | "cli" | "exec" => Kind::Command,
            other => bail!("unknown provider kind '{other}'"),
        })
    }

    fn label(&self) -> &'static str {
        match self {
            Kind::OpenAi => "openai",
            Kind::Anthropic => "anthropic",
            Kind::Ollama => "ollama",
            Kind::WhisperCpp => "whisper_cpp",
            Kind::Command => "command",
        }
    }
}

pub struct Provider {
    name: String,
    kind: Result<Kind, String>,
    cfg: ProviderConfig,
    client: Client,
}

impl Provider {
    pub fn new(name: String, cfg: ProviderConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_seconds.max(5)))
            .build()
            .unwrap_or_else(|_| Client::new());
        let kind = Kind::parse(&cfg.kind).map_err(|e| e.to_string());
        Self { name, kind, cfg, client }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> &str {
        match &self.kind {
            Ok(k) => k.label(),
            Err(_) => "invalid",
        }
    }

    pub fn model(&self) -> String {
        self.cfg.model.clone().unwrap_or_default()
    }

    pub fn base_url(&self) -> String {
        self.cfg.base_url.clone().unwrap_or_default()
    }

    pub fn supports(&self, role: Role) -> bool {
        match (&self.kind, role) {
            (Ok(Kind::OpenAi), _) => true,
            (Ok(Kind::Anthropic), Role::Vision | Role::Text) => true,
            (Ok(Kind::Ollama), Role::Vision | Role::Text) => true,
            (Ok(Kind::WhisperCpp), Role::Transcribe) => true,
            (Ok(Kind::Command), _) => !self.cfg.command.is_empty(),
            _ => false,
        }
    }

    fn kind_or_err(&self) -> Result<Kind> {
        self.kind.clone().map_err(|e| anyhow!("provider '{}': {e}", self.name))
    }

    fn url(&self, path: &str) -> Result<String> {
        let base = self.cfg.base_url.as_deref().ok_or_else(|| anyhow!("provider '{}' has no base_url", self.name))?;
        Ok(format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/')))
    }

    fn model_or_err(&self) -> Result<&str> {
        self.cfg.model.as_deref().ok_or_else(|| anyhow!("provider '{}' has no model", self.name))
    }

    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        let mut rb = self.client.request(method, url);
        for (k, v) in &self.cfg.headers {
            rb = rb.header(k, v);
        }
        rb
    }

    fn system_prompt<'a>(&'a self, explicit: Option<&'a str>) -> Option<&'a str> {
        explicit.or(self.cfg.system_prompt.as_deref())
    }

    // ---- OpenAI-compatible -------------------------------------------------

    async fn openai_chat(&self, images: &[ImageInput], prompt: &str, system: Option<&str>) -> Result<String> {
        let mut content = vec![json!({"type": "text", "text": prompt})];
        for img in images {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&img.bytes);
            content.push(json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{b64}", img.mime)}}));
        }
        let mut messages = vec![];
        if let Some(s) = self.system_prompt(system) {
            messages.push(json!({"role": "system", "content": s}));
        }
        messages.push(json!({"role": "user", "content": content}));
        let body = json!({
            "model": self.model_or_err()?,
            "messages": messages,
            "max_tokens": self.cfg.max_tokens,
            "stream": false,
        });
        let mut rb = self.request(reqwest::Method::POST, self.url("chat/completions")?).json(&body);
        if let Some(key) = self.cfg.resolve_api_key() {
            rb = rb.bearer_auth(key);
        }
        let v: Value = send_json(rb).await?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.trim().to_string())
            .ok_or_else(|| anyhow!("unexpected chat response: {}", truncate(&v.to_string())))
    }

    async fn openai_transcribe(&self, audio: &Path, language: Option<&str>) -> Result<Transcript> {
        let bytes = tokio::fs::read(audio).await.with_context(|| format!("reading {}", audio.display()))?;
        let filename = audio.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or("audio.m4a".into());
        let mut form = reqwest::multipart::Form::new()
            .part("file", reqwest::multipart::Part::bytes(bytes).file_name(filename))
            .text("response_format", "verbose_json")
            .text("timestamp_granularities[]", "segment");
        if let Some(m) = &self.cfg.model {
            form = form.text("model", m.clone());
        }
        if let Some(l) = language {
            form = form.text("language", l.to_string());
        }
        let mut rb = self.request(reqwest::Method::POST, self.url("audio/transcriptions")?).multipart(form);
        if let Some(key) = self.cfg.resolve_api_key() {
            rb = rb.bearer_auth(key);
        }
        let v: Value = send_json(rb).await?;
        Ok(parse_verbose_json(&v, &self.name))
    }

    // ---- Anthropic ----------------------------------------------------------

    async fn anthropic_messages(&self, images: &[ImageInput], prompt: &str, system: Option<&str>) -> Result<String> {
        let mut content = vec![];
        for img in images {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&img.bytes);
            content.push(json!({"type": "image", "source": {"type": "base64", "media_type": img.mime, "data": b64}}));
        }
        content.push(json!({"type": "text", "text": prompt}));
        let mut body = json!({
            "model": self.model_or_err()?,
            "max_tokens": self.cfg.max_tokens,
            "messages": [{"role": "user", "content": content}],
        });
        if let Some(s) = self.system_prompt(system) {
            body["system"] = json!(s);
        }
        let key = self
            .cfg
            .resolve_api_key()
            .ok_or_else(|| anyhow!("provider '{}' needs an API key (api_key_env = \"ANTHROPIC_API_KEY\")", self.name))?;
        let rb = self
            .request(reqwest::Method::POST, self.url("v1/messages")?)
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        let v: Value = send_json(rb).await?;
        let text = v["content"]
            .as_array()
            .map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        if text.is_empty() {
            bail!("unexpected Anthropic response: {}", truncate(&v.to_string()));
        }
        Ok(text.trim().to_string())
    }

    // ---- Ollama native ------------------------------------------------------

    async fn ollama_chat(&self, images: &[ImageInput], prompt: &str, system: Option<&str>) -> Result<String> {
        let mut messages = vec![];
        if let Some(s) = self.system_prompt(system) {
            messages.push(json!({"role": "system", "content": s}));
        }
        let imgs: Vec<String> = images.iter().map(|i| base64::engine::general_purpose::STANDARD.encode(&i.bytes)).collect();
        messages.push(json!({"role": "user", "content": prompt, "images": imgs}));
        let body = json!({"model": self.model_or_err()?, "messages": messages, "stream": false});
        let v: Value = send_json(self.request(reqwest::Method::POST, self.url("api/chat")?).json(&body)).await?;
        v["message"]["content"]
            .as_str()
            .map(|s| s.trim().to_string())
            .ok_or_else(|| anyhow!("unexpected Ollama response: {}", truncate(&v.to_string())))
    }

    // ---- whisper.cpp server -------------------------------------------------

    async fn whisper_cpp_transcribe(&self, audio: &Path, language: Option<&str>) -> Result<Transcript> {
        let bytes = tokio::fs::read(audio).await.with_context(|| format!("reading {}", audio.display()))?;
        let filename = audio.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or("audio.m4a".into());
        let mut form = reqwest::multipart::Form::new()
            .part("file", reqwest::multipart::Part::bytes(bytes).file_name(filename))
            .text("response_format", "verbose_json")
            .text("temperature", "0.0");
        if let Some(l) = language {
            form = form.text("language", l.to_string());
        }
        let rb = self.request(reqwest::Method::POST, self.url("inference")?).multipart(form);
        let v: Value = send_json(rb).await?;
        Ok(parse_verbose_json(&v, &self.name))
    }

    // ---- local command ------------------------------------------------------

    async fn run_command(&self, input: Option<&Path>, prompt: &str, language: Option<&str>, want_srt: bool) -> Result<String> {
        let tmp = tempfile::Builder::new().prefix("bugle-cmd-").tempdir()?;
        let output = tmp.path().join(if want_srt { "out.srt" } else { "out.txt" });
        let subst = |s: &str| {
            s.replace("{input}", &input.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default())
                .replace("{output}", &output.to_string_lossy())
                .replace("{prompt}", prompt)
                .replace("{language}", language.unwrap_or("auto"))
                .replace("{model}", self.cfg.model.as_deref().unwrap_or(""))
        };
        let argv: Vec<String> = self.cfg.command.iter().map(|a| subst(a)).collect();
        let (prog, args) = argv.split_first().ok_or_else(|| anyhow!("provider '{}' has an empty command", self.name))?;
        tracing::debug!("running {prog} {args:?}");
        let out = tokio::process::Command::new(prog)
            .args(args)
            .output()
            .await
            .with_context(|| format!("running {prog}"))?;
        if !out.status.success() {
            bail!("{prog} exited with {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
        }
        if output.exists() {
            return Ok(tokio::fs::read_to_string(&output).await?);
        }
        // whisper-cli style tools write `<input>.srt` next to the input.
        if want_srt {
            if let Some(i) = input {
                let sibling = std::path::PathBuf::from(format!("{}.srt", i.display()));
                if sibling.exists() {
                    return Ok(tokio::fs::read_to_string(sibling).await?);
                }
            }
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[async_trait]
impl VisionModel for Provider {
    async fn describe(&self, images: &[ImageInput], prompt: &str, system: Option<&str>) -> Result<String> {
        match self.kind_or_err()? {
            Kind::OpenAi => self.openai_chat(images, prompt, system).await,
            Kind::Anthropic => self.anthropic_messages(images, prompt, system).await,
            Kind::Ollama => self.ollama_chat(images, prompt, system).await,
            Kind::Command => {
                let tmp = tempfile::Builder::new().prefix("bugle-img-").suffix(".png").tempfile()?;
                let first = images.first().ok_or_else(|| anyhow!("no image"))?;
                tokio::fs::write(tmp.path(), &first.bytes).await?;
                let s = self.run_command(Some(tmp.path()), prompt, None, false).await?;
                Ok(s.trim().to_string())
            }
            Kind::WhisperCpp => bail!("whisper.cpp cannot describe images"),
        }
    }
}

#[async_trait]
impl TextModel for Provider {
    async fn complete(&self, prompt: &str, system: Option<&str>) -> Result<String> {
        match self.kind_or_err()? {
            Kind::OpenAi => self.openai_chat(&[], prompt, system).await,
            Kind::Anthropic => self.anthropic_messages(&[], prompt, system).await,
            Kind::Ollama => self.ollama_chat(&[], prompt, system).await,
            Kind::Command => Ok(self.run_command(None, prompt, None, false).await?.trim().to_string()),
            Kind::WhisperCpp => bail!("whisper.cpp cannot complete text"),
        }
    }
}

#[async_trait]
impl Transcriber for Provider {
    async fn transcribe(&self, audio: &Path, language: Option<&str>) -> Result<Transcript> {
        match self.kind_or_err()? {
            Kind::OpenAi => self.openai_transcribe(audio, language).await,
            Kind::WhisperCpp => self.whisper_cpp_transcribe(audio, language).await,
            Kind::Command => {
                let srt = self.run_command(Some(audio), "", language, true).await?;
                let mut t = parse_srt(&srt);
                t.provider = self.name.clone();
                t.language = language.map(|s| s.to_string());
                Ok(t)
            }
            Kind::Anthropic | Kind::Ollama => bail!("provider '{}' does not do speech-to-text", self.name),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn send_json(rb: reqwest::RequestBuilder) -> Result<Value> {
    let resp = rb.send().await.context("request failed (is the server running?)")?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("HTTP {status}: {}", truncate(&text));
    }
    serde_json::from_str(&text).with_context(|| format!("non-JSON response: {}", truncate(&text)))
}

fn truncate(s: &str) -> String {
    if s.len() > 600 {
        format!("{}…", &s[..600])
    } else {
        s.to_string()
    }
}

/// Parse OpenAI/whisper.cpp `verbose_json`.
pub fn parse_verbose_json(v: &Value, provider: &str) -> Transcript {
    let segments = v["segments"]
        .as_array()
        .map(|segs| {
            segs.iter()
                .filter_map(|s| {
                    Some(TranscriptSegment {
                        start: num(&s["start"])?,
                        end: num(&s["end"])?,
                        text: s["text"].as_str()?.trim().to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let text = v["text"]
        .as_str()
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" "));
    Transcript { language: v["language"].as_str().map(|s| s.to_string()), text, segments, provider: provider.to_string() }
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Parse SRT text into a transcript.
pub fn parse_srt(srt: &str) -> Transcript {
    let mut segments = vec![];
    for block in srt.replace("\r\n", "\n").split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        let Some(ts_idx) = lines.iter().position(|l| l.contains("-->")) else { continue };
        let (a, b) = lines[ts_idx].split_once("-->").unwrap();
        let (Some(start), Some(end)) = (parse_ts(a), parse_ts(b)) else { continue };
        let text = lines[ts_idx + 1..].join("\n").trim().to_string();
        if !text.is_empty() {
            segments.push(TranscriptSegment { start, end, text });
        }
    }
    let text = segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ");
    Transcript { language: None, text, segments, provider: String::new() }
}

fn parse_ts(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    let mut parts = s.split(':').collect::<Vec<_>>();
    parts.reverse();
    let mut total = 0.0;
    let mut mult = 1.0;
    for p in parts {
        total += p.trim().parse::<f64>().ok()? * mult;
        mult *= 60.0;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_roundtrip() {
        let srt = "1\n00:00:01,000 --> 00:00:02,500\nHello there\n\n2\n00:00:03,000 --> 00:00:04,000\nBye\n";
        let t = parse_srt(srt);
        assert_eq!(t.segments.len(), 2);
        assert_eq!(t.segments[0].start, 1.0);
        assert_eq!(t.segments[0].end, 2.5);
        assert!(t.to_srt().contains("Hello there"));
        assert!(t.to_vtt().starts_with("WEBVTT"));
    }

    #[test]
    fn verbose_json_parses() {
        let v = json!({"text":"hi there","language":"en","segments":[{"start":0.0,"end":1.2,"text":" hi there "}]});
        let t = parse_verbose_json(&v, "x");
        assert_eq!(t.segments[0].text, "hi there");
        assert_eq!(t.language.as_deref(), Some("en"));
    }

    #[test]
    fn roles() {
        let p = crate::adhoc("anthropic", "https://api.anthropic.com", Some("m"));
        assert!(p.supports(Role::Vision));
        assert!(!p.supports(Role::Transcribe));
        let w = crate::adhoc("whisper_cpp", "http://localhost:8178", None);
        assert!(w.supports(Role::Transcribe));
    }
}
