//! Configuration (`~/.config/bugle/config.toml`).
//!
//! Capture, recording, OCR and annotation settings, plus the AI provider
//! registry and the agent API settings.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::spec::ImageFormat;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub screenshots: ScreenshotConfig,
    pub recording: RecordingConfig,
    pub ocr: OcrConfig,
    pub annotate: AnnotateConfig,
    pub api: ApiConfig,
    pub ai: AiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Where captures land. `~` is expanded.
    pub save_dir: String,
    /// Filename template, see `naming`.
    pub filename_template: String,
    /// Play the shutter sound.
    pub sound: bool,
    /// Copy every capture to the clipboard as well as saving it.
    pub copy_to_clipboard: bool,
    /// Where the history file lives.
    pub history_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenshotConfig {
    pub format: ImageFormat,
    pub jpeg_quality: u8,
    /// Keep the window drop shadow in window captures.
    pub window_shadow: bool,
    /// Hide desktop icons during capture.
    pub hide_desktop_icons: bool,
    /// Self-timer default when `--delay` is given without a value.
    pub self_timer_seconds: f64,
    /// Scale captures down to 1x (points) instead of keeping Retina pixels.
    pub downscale_retina: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingConfig {
    pub gif_fps: u32,
    /// Max long edge for GIF frames.
    pub gif_max_size: u32,
    pub video_fps: u32,
    pub microphone: bool,
    pub system_audio: bool,
    pub highlight_clicks: bool,
    pub retina: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OcrConfig {
    /// BCP-47 codes passed to Vision (`en-US`, `de-DE`, …). Empty = automatic.
    pub languages: Vec<String>,
    pub accurate: bool,
    pub language_correction: bool,
    /// Default OCR engine: `vision` or an AI provider name.
    pub engine: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnotateConfig {
    pub stroke_color: String,
    pub stroke_width: f32,
    pub font_path: Option<String>,
    pub font_size: f32,
    /// Default background preset for `canvas.background` when `"auto"`.
    pub background: String,
    pub padding: u32,
    pub corner_radius: u32,
    pub shadow: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub bind: String,
    /// Optional bearer token. When set every HTTP request must carry it.
    pub token: Option<String>,
    /// Allow CORS from browsers (useful for local web agents).
    pub cors: bool,
}

/// AI provider registry. Providers are named so requests can pick one;
/// `default_*` choose the provider for each role.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AiConfig {
    pub default_vision: Option<String>,
    pub default_transcriber: Option<String>,
    pub default_text: Option<String>,
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// `openai` (any OpenAI-compatible server: llama.cpp, Ollama, LM Studio,
    /// vLLM, OpenAI, OpenRouter…), `anthropic`, `ollama` (native API),
    /// `whisper_cpp` (whisper.cpp server), `command` (run a local program).
    pub kind: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// Name of an environment variable holding the key (preferred over `api_key`).
    pub api_key_env: Option<String>,
    /// Request timeout.
    pub timeout_seconds: u64,
    /// Extra headers.
    pub headers: BTreeMap<String, String>,
    /// For `command`: program + args. `{input}`, `{output}`, `{prompt}`,
    /// `{language}` are substituted. The program must write its result to
    /// `{output}` (SRT for transcription, plain text otherwise) or stdout.
    pub command: Vec<String>,
    /// Default system prompt for vision/text use.
    pub system_prompt: Option<String>,
    pub max_tokens: u32,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: "openai".into(),
            base_url: None,
            model: None,
            api_key: None,
            api_key_env: None,
            timeout_seconds: 180,
            headers: BTreeMap::new(),
            command: vec![],
            system_prompt: None,
            max_tokens: 1024,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            screenshots: ScreenshotConfig::default(),
            recording: RecordingConfig::default(),
            ocr: OcrConfig::default(),
            annotate: AnnotateConfig::default(),
            api: ApiConfig::default(),
            ai: AiConfig::default_registry(),
        }
    }
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            save_dir: "~/Desktop/Bugle".into(),
            filename_template: crate::naming::DEFAULT_TEMPLATE.into(),
            sound: false,
            copy_to_clipboard: false,
            history_path: None,
        }
    }
}

impl Default for ScreenshotConfig {
    fn default() -> Self {
        Self {
            format: ImageFormat::Png,
            jpeg_quality: 90,
            window_shadow: true,
            hide_desktop_icons: false,
            self_timer_seconds: 5.0,
            downscale_retina: false,
        }
    }
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            gif_fps: 12,
            gif_max_size: 1200,
            video_fps: 30,
            microphone: false,
            system_audio: false,
            highlight_clicks: false,
            retina: true,
        }
    }
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self { languages: vec![], accurate: true, language_correction: true, engine: "vision".into() }
    }
}

impl Default for AnnotateConfig {
    fn default() -> Self {
        Self {
            stroke_color: "#FF3B30".into(),
            stroke_width: 4.0,
            font_path: None,
            font_size: 28.0,
            background: "none".into(),
            padding: 48,
            corner_radius: 12,
            shadow: true,
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { bind: "127.0.0.1:4848".into(), token: None, cors: true }
    }
}

impl AiConfig {
    /// A starter registry pointing at the usual local servers so `bugle
    /// config init` yields something that works once a model is running.
    pub fn default_registry() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "llama".into(),
            ProviderConfig {
                kind: "openai".into(),
                base_url: Some("http://127.0.0.1:8080/v1".into()),
                model: Some("default".into()),
                system_prompt: Some(
                    "You describe screenshots precisely for a sighted or non-sighted reader. Name the application, the key UI elements, and any visible text. Be concise.".into(),
                ),
                ..Default::default()
            },
        );
        providers.insert(
            "ollama".into(),
            ProviderConfig {
                kind: "ollama".into(),
                base_url: Some("http://127.0.0.1:11434".into()),
                model: Some("llava".into()),
                ..Default::default()
            },
        );
        providers.insert(
            "whisper".into(),
            ProviderConfig {
                kind: "whisper_cpp".into(),
                base_url: Some("http://127.0.0.1:8178".into()),
                ..Default::default()
            },
        );
        providers.insert(
            "openai".into(),
            ProviderConfig {
                kind: "openai".into(),
                base_url: Some("https://api.openai.com/v1".into()),
                model: Some("gpt-4o-mini".into()),
                api_key_env: Some("OPENAI_API_KEY".into()),
                ..Default::default()
            },
        );
        providers.insert(
            "anthropic".into(),
            ProviderConfig {
                kind: "anthropic".into(),
                base_url: Some("https://api.anthropic.com".into()),
                model: Some("claude-sonnet-5-5".into()),
                api_key_env: Some("ANTHROPIC_API_KEY".into()),
                ..Default::default()
            },
        );
        Self {
            default_vision: Some("llama".into()),
            default_transcriber: Some("whisper".into()),
            default_text: Some("llama".into()),
            providers,
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("BUGLE_CONFIG") {
            return PathBuf::from(p);
        }
        dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("bugle").join("config.toml")
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&Self::default_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn save_dir(&self) -> PathBuf {
        expand_tilde(&self.general.save_dir)
    }

    pub fn history_path(&self) -> PathBuf {
        match &self.general.history_path {
            Some(p) => expand_tilde(p),
            None => dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("bugle")
                .join("history.jsonl"),
        }
    }

    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.ai.providers.get(name)
    }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if p == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(p)
}

impl ProviderConfig {
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(env) = &self.api_key_env {
            if let Ok(v) = std::env::var(env) {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        self.api_key.clone()
    }
}
