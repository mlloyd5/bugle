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
    /// The HTTP client, or why it could not be built (reported at call time
    /// and by [`Provider::not_ready_reason`] rather than silently replaced by
    /// a client without the configured timeout).
    client: Result<Client, String>,
}

impl Provider {
    pub fn new(name: String, cfg: ProviderConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_seconds.max(5)))
            .build()
            .map_err(|e| format!("could not build HTTP client: {e}"));
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

    /// Whether this provider can serve `role` right now: its kind offers the
    /// role and every setting the kind needs is present.
    pub fn supports(&self, role: Role) -> bool {
        self.capable(role) && self.not_ready_reason().is_none()
    }

    /// Whether the provider's kind offers `role` at all, regardless of
    /// whether the required settings are filled in.
    pub fn capable(&self, role: Role) -> bool {
        matches!(
            (&self.kind, role),
            (Ok(Kind::OpenAi), _)
                | (Ok(Kind::Anthropic), Role::Vision | Role::Text)
                | (Ok(Kind::Ollama), Role::Vision | Role::Text)
                | (Ok(Kind::WhisperCpp), Role::Transcribe)
                | (Ok(Kind::Command), _)
        )
    }

    /// Why the provider cannot be used yet (missing settings, invalid kind…),
    /// or `None` when it is ready.
    pub fn not_ready_reason(&self) -> Option<String> {
        let kind = match &self.kind {
            Ok(k) => *k,
            Err(e) => return Some(e.clone()),
        };
        let mut missing: Vec<&str> = vec![];
        let has = |o: &Option<String>| o.as_deref().is_some_and(|s| !s.trim().is_empty());
        if matches!(kind, Kind::OpenAi | Kind::Ollama | Kind::Anthropic | Kind::WhisperCpp) && !has(&self.cfg.base_url) {
            missing.push("base_url");
        }
        if matches!(kind, Kind::OpenAi | Kind::Ollama | Kind::Anthropic) && !has(&self.cfg.model) {
            missing.push("model");
        }
        if kind == Kind::Anthropic && self.cfg.resolve_api_key().is_none() {
            missing.push("an API key (api_key_env or api_key)");
        }
        if kind == Kind::Command && self.cfg.command.first().is_none_or(|p| p.trim().is_empty()) {
            missing.push("command");
        }
        if !missing.is_empty() {
            return Some(format!("missing {}", missing.join(", ")));
        }
        match &self.client {
            Err(e) if kind != Kind::Command => Some(e.clone()),
            _ => None,
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

    fn request(&self, method: reqwest::Method, url: String) -> Result<reqwest::RequestBuilder> {
        let client = self.client.as_ref().map_err(|e| anyhow!("provider '{}': {e}", self.name))?;
        let mut rb = client.request(method, url);
        for (k, v) in &self.cfg.headers {
            rb = rb.header(k, v);
        }
        Ok(rb)
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
        let mut rb = self.request(reqwest::Method::POST, self.url("chat/completions")?)?.json(&body);
        if let Some(key) = self.cfg.resolve_api_key() {
            rb = rb.bearer_auth(key);
        }
        let v: Value = send_json(rb).await?;
        openai_message_text(&v)
    }

    async fn openai_transcribe(&self, audio: &Path, language: Option<&str>) -> Result<Transcript> {
        let mut form = reqwest::multipart::Form::new()
            .part("file", file_part(audio).await?)
            .text("response_format", "verbose_json")
            .text("timestamp_granularities[]", "segment");
        if let Some(m) = &self.cfg.model {
            form = form.text("model", m.clone());
        }
        if let Some(l) = language {
            form = form.text("language", l.to_string());
        }
        let mut rb = self.request(reqwest::Method::POST, self.url("audio/transcriptions")?)?.multipart(form);
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
            .request(reqwest::Method::POST, self.url("v1/messages")?)?
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
        let v: Value = send_json(self.request(reqwest::Method::POST, self.url("api/chat")?)?.json(&body)).await?;
        v["message"]["content"]
            .as_str()
            .map(|s| s.trim().to_string())
            .ok_or_else(|| anyhow!("unexpected Ollama response: {}", truncate(&v.to_string())))
    }

    // ---- whisper.cpp server -------------------------------------------------

    async fn whisper_cpp_transcribe(&self, audio: &Path, language: Option<&str>) -> Result<Transcript> {
        let mut form = reqwest::multipart::Form::new()
            .part("file", file_part(audio).await?)
            .text("response_format", "verbose_json")
            .text("temperature", "0.0");
        if let Some(l) = language {
            form = form.text("language", l.to_string());
        }
        let rb = self.request(reqwest::Method::POST, self.url("inference")?)?.multipart(form);
        let v: Value = send_json(rb).await?;
        Ok(parse_verbose_json(&v, &self.name))
    }

    // ---- local command ------------------------------------------------------

    async fn run_command(&self, input: Option<&Path>, prompt: &str, language: Option<&str>, want_srt: bool) -> Result<String> {
        let tmp = tempfile::Builder::new().prefix("bugle-cmd-").tempdir()?;
        let output = tmp.path().join(if want_srt { "out.srt" } else { "out.txt" });
        let input_s = input.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let output_s = output.to_string_lossy().into_owned();
        let vars = [
            ("input", input_s.as_str()),
            ("output", output_s.as_str()),
            ("prompt", prompt),
            ("language", language.unwrap_or("auto")),
            ("model", self.cfg.model.as_deref().unwrap_or("")),
        ];
        let argv: Vec<String> = self.cfg.command.iter().map(|a| substitute(a, &vars)).collect();
        let (prog, args) = argv.split_first().ok_or_else(|| anyhow!("provider '{}' has an empty command", self.name))?;
        tracing::debug!("running {prog} {args:?}");
        let limit = Duration::from_secs(self.cfg.timeout_seconds.max(1));
        // `kill_on_drop` makes both the timeout below and a dropped request
        // (the caller's future going away) terminate the child.
        let child = tokio::process::Command::new(prog)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(limit, child).await {
            Ok(r) => r.with_context(|| format!("running {prog}"))?,
            Err(_) => bail!("{prog} timed out after {}s (timeout_seconds)", limit.as_secs()),
        };
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

/// Shorten `s` to at most 600 bytes for error messages, cutting on a
/// character boundary.
fn truncate(s: &str) -> String {
    const MAX: usize = 600;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Replace `{name}` placeholders in one left-to-right pass, so text that a
/// value brings in (a prompt containing `{output}`, say) is never expanded.
/// Unknown placeholders are left as they are.
fn substitute(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let hit = after
            .find('}')
            .and_then(|close| vars.iter().find(|(k, _)| *k == &after[..close]).map(|(_, v)| (close, *v)));
        match hit {
            Some((close, value)) => {
                out.push_str(value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A multipart file part that streams `path` from disk instead of reading it
/// into memory.
async fn file_part(path: &Path) -> Result<reqwest::multipart::Part> {
    let file = tokio::fs::File::open(path).await.with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata().await.with_context(|| format!("reading {}", path.display()))?.len();
    let filename = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or("audio.m4a".into());
    Ok(reqwest::multipart::Part::stream_with_length(reqwest::Body::from(file), len).file_name(filename))
}

/// The assistant text of an OpenAI-compatible chat completion. `content` may
/// be a string, an array of content parts, or null alongside a `refusal`.
fn openai_message_text(v: &Value) -> Result<String> {
    let msg = &v["choices"][0]["message"];
    match &msg["content"] {
        Value::String(s) => return Ok(s.trim().to_string()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter(|p| p["type"].as_str().is_none_or(|t| t == "text" || t == "output_text"))
                .filter_map(|p| p["text"].as_str().or_else(|| p["text"]["value"].as_str()).or_else(|| p.as_str()))
                .collect::<Vec<_>>()
                .join("");
            if !text.trim().is_empty() {
                return Ok(text.trim().to_string());
            }
        }
        _ => {}
    }
    if let Some(refusal) = msg["refusal"].as_str().filter(|r| !r.is_empty()) {
        bail!("the model refused: {}", truncate(refusal.trim()));
    }
    Err(anyhow!("unexpected chat response: {}", truncate(&v.to_string())))
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
///
/// Cues are separated by blank lines (which may contain whitespace); cue
/// settings after the end timestamp (`align:start` …) are ignored.
pub fn parse_srt(srt: &str) -> Transcript {
    let mut segments = vec![];
    let srt = srt.trim_start_matches('\u{feff}');
    let mut blocks: Vec<Vec<&str>> = vec![vec![]];
    for line in srt.lines() {
        if line.trim().is_empty() {
            if !blocks.last().is_some_and(|b| b.is_empty()) {
                blocks.push(vec![]);
            }
        } else if let Some(b) = blocks.last_mut() {
            b.push(line.trim_end());
        }
    }
    for lines in blocks {
        let Some(ts_idx) = lines.iter().position(|l| l.contains("-->")) else { continue };
        let Some((a, b)) = lines[ts_idx].split_once("-->") else { continue };
        let a = a.split_whitespace().last().unwrap_or("");
        let b = b.split_whitespace().next().unwrap_or("");
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
        let p = Provider::new(
            "a".into(),
            ProviderConfig {
                kind: "anthropic".into(),
                base_url: Some("https://api.anthropic.com".into()),
                model: Some("m".into()),
                api_key: Some("k".into()),
                ..Default::default()
            },
        );
        assert!(p.supports(Role::Vision));
        assert!(!p.supports(Role::Transcribe));
        let w = crate::adhoc("whisper_cpp", "http://localhost:8178", None);
        assert!(w.supports(Role::Transcribe));
    }

    fn provider(cfg: ProviderConfig) -> Provider {
        Provider::new("p".into(), cfg)
    }

    #[test]
    fn readiness_reports_missing_settings() {
        // openai without a model: capable of transcription, but not ready.
        let p = crate::adhoc("openai", "http://127.0.0.1:1", None);
        assert!(p.capable(Role::Transcribe));
        assert!(!p.supports(Role::Transcribe));
        assert!(p.not_ready_reason().unwrap().contains("model"));

        let o = provider(ProviderConfig { kind: "ollama".into(), ..Default::default() });
        let r = o.not_ready_reason().unwrap();
        assert!(r.contains("base_url") && r.contains("model"), "{r}");
        assert!(!o.supports(Role::Vision));

        let a = provider(ProviderConfig {
            kind: "anthropic".into(),
            base_url: Some("https://x".into()),
            model: Some("m".into()),
            api_key_env: Some("BUGLE_TEST_SURELY_UNSET_KEY".into()),
            ..Default::default()
        });
        assert!(a.not_ready_reason().unwrap().contains("API key"));

        let c = provider(ProviderConfig { kind: "command".into(), ..Default::default() });
        assert!(!c.supports(Role::Text));
        assert!(provider(ProviderConfig { kind: "bogus".into(), ..Default::default() }).not_ready_reason().is_some());

        let mut cfg = bugle_core::config::AiConfig::default();
        cfg.providers.insert("o".into(), ProviderConfig { kind: "ollama".into(), ..Default::default() });
        cfg.providers.insert("w".into(), ProviderConfig { kind: "whisper".into(), base_url: Some("http://x".into()), ..Default::default() });
        cfg.default_vision = Some("o".into());
        let reg = crate::Registry::from_config(&cfg);
        let all = reg.describe_all();
        assert_eq!(all[0]["name"], "o");
        assert_eq!(all[0]["ready"], false);
        assert!(all[0]["reason"].as_str().unwrap().contains("model"));
        assert_eq!(all[0]["roles"], json!(["vision", "text"]));
        assert_eq!(all[0]["default_for"], json!(["vision"]));
        assert_eq!(all[1]["ready"], true);
        assert!(all[1]["reason"].is_null());
        let err = reg.resolve(None, Role::Vision).err().unwrap().to_string();
        assert!(err.contains("not ready"), "{err}");
        assert!(reg.resolve(Some("w"), Role::Transcribe).is_ok());
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = format!("{}é{}", "a".repeat(599), "b".repeat(50));
        let t = truncate(&s);
        assert!(t.ends_with('…'));
        assert_eq!(t.trim_end_matches('…'), "a".repeat(599));
        let emoji = "🙂".repeat(300);
        assert!(truncate(&emoji).len() <= 600 + '…'.len_utf8());
        assert_eq!(truncate("short"), "short");
    }

    #[test]
    fn substitute_is_single_pass() {
        let vars = [("prompt", "say {output} and {model}"), ("output", "/tmp/o"), ("model", "m")];
        assert_eq!(substitute("{prompt}", &vars), "say {output} and {model}");
        assert_eq!(substitute("-o={output} -m {model}", &vars), "-o=/tmp/o -m m");
        assert_eq!(substitute("{unknown} {{output}} {", &vars), "{unknown} {/tmp/o} {");
    }

    #[tokio::test]
    async fn command_prompt_placeholders_are_not_expanded() {
        let p = provider(ProviderConfig {
            kind: "command".into(),
            model: Some("MODEL".into()),
            command: vec!["printf".into(), "%s".into(), "{prompt}|{model}".into()],
            ..Default::default()
        });
        let out = p.complete("x {output} {language} {model}", None).await.unwrap();
        assert_eq!(out, "x {output} {language} {model}|MODEL");
    }

    #[tokio::test]
    async fn command_times_out_and_is_killed() {
        let p = provider(ProviderConfig {
            kind: "command".into(),
            timeout_seconds: 1,
            command: vec!["sleep".into(), "30".into()],
            ..Default::default()
        });
        let started = std::time::Instant::now();
        let err = p.complete("hi", None).await.unwrap_err().to_string();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn srt_whitespace_separators_and_cue_settings() {
        let srt = "\u{feff}1\r\n00:01.000 --> 00:02.000 align:start position:10%\r\nOne\r\n  \r\n2\n00:00:03,000 --> 00:00:04,500\nTwo\nlines\n\t\n\n3\n00:05.000-->00:06.000\nThree\n";
        let t = parse_srt(srt);
        assert_eq!(t.segments.len(), 3, "{:?}", t.segments.iter().map(|s| &s.text).collect::<Vec<_>>());
        assert_eq!((t.segments[0].start, t.segments[0].end), (1.0, 2.0));
        assert_eq!(t.segments[0].text, "One");
        assert_eq!((t.segments[1].start, t.segments[1].end), (3.0, 4.5));
        assert_eq!(t.segments[1].text, "Two\nlines");
        assert_eq!((t.segments[2].start, t.segments[2].end), (5.0, 6.0));
    }

    #[test]
    fn openai_content_shapes() {
        let s = json!({"choices":[{"message":{"content":" hi "}}]});
        assert_eq!(openai_message_text(&s).unwrap(), "hi");
        let parts = json!({"choices":[{"message":{"content":[{"type":"text","text":"a "},{"type":"image_url","image_url":{}},{"type":"text","text":"b"}]}}]});
        assert_eq!(openai_message_text(&parts).unwrap(), "a b");
        let refusal = json!({"choices":[{"message":{"content":null,"refusal":"I can't help with that."}}]});
        let e = openai_message_text(&refusal).unwrap_err().to_string();
        assert!(e.contains("refused") && e.contains("can't help"), "{e}");
        assert!(openai_message_text(&json!({"x":1})).unwrap_err().to_string().contains("unexpected chat response"));
    }

    // ---- local HTTP server tests -------------------------------------------

    use axum::extract::{Multipart, State};
    use axum::routing::post;
    use std::sync::{Arc, Mutex};

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn openai_chat_over_http() {
        let app = axum::Router::new().route(
            "/chat/completions",
            post(|axum::Json(body): axum::Json<Value>| async move {
                let prompt = body["messages"][0]["content"][0]["text"].as_str().unwrap_or("").to_string();
                match prompt.as_str() {
                    "parts" => (axum::http::StatusCode::OK, axum::Json(json!({"choices":[{"message":{"content":[{"type":"text","text":"from parts"}]}}]}))),
                    "refuse" => (axum::http::StatusCode::OK, axum::Json(json!({"choices":[{"message":{"content":null,"refusal":"no"}}]}))),
                    // An error body whose byte 600 falls inside a multi-byte character.
                    _ => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": format!("{}{}", "x".repeat(587), "é".repeat(400))}))),
                }
            }),
        );
        let base = serve(app).await;
        let p = crate::adhoc("openai", &base, Some("m"));
        assert_eq!(p.complete("parts", None).await.unwrap(), "from parts");
        assert!(p.complete("refuse", None).await.unwrap_err().to_string().contains("refused"));
        let e = p.complete("boom", None).await.unwrap_err().to_string();
        assert!(e.contains("HTTP 400") && e.ends_with('…'), "{e}");
    }

    #[tokio::test]
    async fn transcription_upload_streams_file() {
        type Seen = Arc<Mutex<Option<(String, Vec<u8>)>>>;
        let seen: Seen = Arc::default();
        let app = axum::Router::new()
            .route(
                "/audio/transcriptions",
                post(|State(seen): State<Seen>, mut mp: Multipart| async move {
                    while let Some(field) = mp.next_field().await.unwrap() {
                        if field.name() == Some("file") {
                            let name = field.file_name().unwrap_or("").to_string();
                            let bytes = field.bytes().await.unwrap().to_vec();
                            *seen.lock().unwrap() = Some((name, bytes));
                        }
                    }
                    axum::Json(json!({"text":"hello","segments":[{"start":0.0,"end":1.0,"text":"hello"}]}))
                }),
            )
            .with_state(seen.clone());
        let base = serve(app).await;
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("clip.wav");
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&audio, &payload).unwrap();
        let p = crate::adhoc("openai", &base, Some("whisper-1"));
        let t = p.transcribe(&audio, None).await.unwrap();
        assert_eq!(t.text, "hello");
        let (name, bytes) = seen.lock().unwrap().take().unwrap();
        assert_eq!(name, "clip.wav");
        assert_eq!(bytes, payload);
    }
}
