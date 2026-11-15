//! Configuration (`~/.config/bugle/config.toml`).
//!
//! Capture, recording, OCR and annotation settings, plus the AI provider
//! registry, the agent API settings and the sync destination.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::media_item::MediaKind;
use crate::spec::ImageFormat;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub screenshots: ScreenshotConfig,
    pub recordings: RecordingConfig,
    pub ocr: OcrConfig,
    pub markup: MarkupConfig,
    pub api: ApiConfig,
    pub ai: AiConfig,
    pub hotkeys: HotkeyConfig,
    pub sync: SyncConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Where captures land. `~` is expanded.
    pub save_dir: String,
    /// Filename template, see `naming`.
    pub filename_template: String,
    /// Play the shutter sound.
    pub shutter_sound: bool,
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
    /// Scale captures down to 1x (points) instead of keeping Retina pixels.
    pub downscale_retina: bool,
    /// `bugle-app` *Capture Window*: hover to highlight a window and click it.
    /// `false` captures the front-most window at once (the CLI and API always
    /// do, unless given an id, title or owner).
    pub window_picker: bool,
}

/// Which recorder produces video (mp4/mov) recordings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecordingBackend {
    /// ScreenCaptureKit when system audio is requested (and available),
    /// otherwise `screencapture -v`.
    #[default]
    Auto,
    /// Always the in-process ScreenCaptureKit + AVAssetWriter recorder.
    #[serde(rename = "stream")]
    StreamCapture,
    /// Always the system `screencapture -v` child process (no system audio).
    #[serde(rename = "system")]
    SystemRecorder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingConfig {
    pub gif_frame_rate: u32,
    /// Max long edge for GIF frames.
    pub gif_max_size: u32,
    pub video_fps: u32,
    pub microphone: bool,
    /// Record system audio; needs the ScreenCaptureKit backend.
    pub system_audio: bool,
    pub highlight_clicks: bool,
    pub retina: bool,
    pub backend: RecordingBackend,
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
pub struct MarkupConfig {
    pub stroke_color: String,
    pub stroke_width: f32,
    pub font_path: Option<String>,
    pub font_size: f32,
    /// Default background preset for `canvas.background` when `"auto"`.
    pub background: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub bind: String,
    /// Bearer token. When set every HTTP request must carry it. Recommended;
    /// required when `bind` is not a loopback address.
    pub token: Option<String>,
    /// Allow CORS from browsers (useful for local web agents). Only takes
    /// effect when `token` is set, so web pages cannot drive the API.
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
            recordings: RecordingConfig::default(),
            ocr: OcrConfig::default(),
            markup: MarkupConfig::default(),
            api: ApiConfig::default(),
            ai: AiConfig::default_registry(),
            hotkeys: HotkeyConfig::default(),
            sync: SyncConfig::default(),
        }
    }
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            save_dir: "~/Desktop/Bugle".into(),
            filename_template: crate::naming::DEFAULT_TEMPLATE.into(),
            shutter_sound: false,
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
            downscale_retina: false,
            window_picker: true,
        }
    }
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            gif_frame_rate: 12,
            gif_max_size: 1200,
            video_fps: 30,
            microphone: false,
            system_audio: false,
            highlight_clicks: false,
            retina: true,
            backend: RecordingBackend::Auto,
        }
    }
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self { languages: vec![], accurate: true, language_correction: true, engine: "vision".into() }
    }
}

impl Default for MarkupConfig {
    fn default() -> Self {
        Self {
            stroke_color: "#FF3B30".into(),
            stroke_width: 4.0,
            font_path: None,
            font_size: 28.0,
            background: "none".into(),
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { bind: "127.0.0.1:4848".into(), token: None, cors: false }
    }
}

/// Global hotkeys for the menu-bar app (`bugle-app`). Each value is a
/// `modifier+modifier+key` string: modifiers `cmd`, `shift`, `alt`/`option`,
/// `ctrl`; keys are letters, digits, `F1`..`F24`, `space`, `enter`, … (the
/// W3C `KeyboardEvent.code` names also work). An empty string or a missing
/// key leaves that action without a hotkey.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    /// Drag an area with the crosshair overlay.
    pub capture_area: Option<String>,
    /// Capture the main display.
    pub capture_screen: Option<String>,
    /// Capture the front-most window.
    pub capture_window: Option<String>,
    /// Drag an area, then scroll-and-stitch it (needs Accessibility).
    pub scroll_capture: Option<String>,
    /// Start an MP4 recording of a dragged area, or stop the active recording.
    pub record: Option<String>,
    /// Start a GIF recording of a dragged area, or stop the active recording.
    pub record_gif: Option<String>,
    /// OCR the latest capture and copy the text.
    pub ocr_latest: Option<String>,
    /// Pin the latest capture to the screen.
    pub pin_latest: Option<String>,
    /// Bindings for the other menu items (`stop_recording`, `sync_latest`,
    /// `open_folder`, `toggle_api`, `preferences`, `quit`). `bugle-app` owns
    /// the list of valid names and warns about unknown keys and non-string
    /// values instead of failing to load.
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            capture_area: Some("cmd+shift+4".into()),
            capture_screen: Some("cmd+shift+3".into()),
            capture_window: None,
            scroll_capture: None,
            record: Some("cmd+shift+5".into()),
            record_gif: None,
            ocr_latest: Some("cmd+shift+2".into()),
            pin_latest: Some("cmd+shift+p".into()),
            extra: BTreeMap::new(),
        }
    }
}

impl HotkeyConfig {
    /// `(action, binding)` pairs for every configured, non-empty hotkey.
    /// Action names match the `[hotkeys]` keys.
    pub fn bindings(&self) -> Vec<(&'static str, String)> {
        let all = [
            ("capture_area", &self.capture_area),
            ("capture_screen", &self.capture_screen),
            ("capture_window", &self.capture_window),
            ("scroll_capture", &self.scroll_capture),
            ("record", &self.record),
            ("record_gif", &self.record_gif),
            ("ocr_latest", &self.ocr_latest),
            ("pin_latest", &self.pin_latest),
        ];
        all.into_iter()
            .filter_map(|(name, v)| v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| (name, s.to_string())))
            .collect()
    }
}

/// Sync every new capture to storage you own: a folder that Dropbox or
/// iCloud Drive mirrors, or an S3-compatible bucket. No hosted service.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// Run the sync step after every save. `bugle sync <source>` works
    /// whenever a destination is configured, enabled or not.
    pub enabled: bool,
    /// `copy` leaves the original in `save_dir`; `move` relocates it (folder
    /// destinations only — an upload always keeps the local file).
    pub mode: SyncMode,
    /// Which kinds of media are synced automatically.
    pub kinds: Vec<MediaKind>,
    /// Destination folder (`~` is expanded). Mutually exclusive with `s3`.
    pub folder: Option<String>,
    /// S3-compatible destination. Mutually exclusive with `folder`.
    pub s3: Option<S3Config>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    #[default]
    Copy,
    Move,
}

/// An S3-compatible bucket reached with path-style requests
/// (`{endpoint}/{bucket}/{key}`) and SigV4 signing: AWS S3, MinIO, Garage,
/// Cloudflare R2, Backblaze B2, … Credentials come from the environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct S3Config {
    /// Base URL of the service, e.g. `https://s3.eu-west-1.amazonaws.com`,
    /// `http://127.0.0.1:9000` (MinIO) or `https://<account>.r2.cloudflarestorage.com`.
    pub endpoint: String,
    pub bucket: String,
    /// Signing region. `auto` for R2, `us-east-1` for most self-hosted servers.
    pub region: String,
    /// Key prefix ("folder") inside the bucket; a trailing `/` is implied.
    pub prefix: String,
    /// Environment variables holding the credentials.
    pub access_key_env: String,
    pub secret_key_env: String,
    /// Base URL used for the link that is recorded and copied, when objects
    /// are served from somewhere else than the API endpoint (a CDN, a custom
    /// domain). Defaults to `{endpoint}/{bucket}`.
    pub public_url: Option<String>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: SyncMode::Copy,
            kinds: vec![MediaKind::Screenshot, MediaKind::ScrollCapture, MediaKind::Annotated, MediaKind::Video, MediaKind::Gif],
            folder: None,
            s3: None,
        }
    }
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            endpoint: "https://s3.amazonaws.com".into(),
            bucket: String::new(),
            region: "us-east-1".into(),
            prefix: String::new(),
            access_key_env: "AWS_ACCESS_KEY_ID".into(),
            secret_key_env: "AWS_SECRET_ACCESS_KEY".into(),
            public_url: None,
        }
    }
}

/// Where synced files go, resolved from [`SyncConfig`].
#[derive(Debug, Clone)]
pub enum SyncDestination {
    Folder(PathBuf),
    S3(S3Config),
}

impl SyncConfig {
    /// The configured destination; `None` when neither `folder` nor `s3` is
    /// set, an error when both are.
    pub fn destination(&self) -> Result<Option<SyncDestination>> {
        let folder = self.folder.as_deref().map(str::trim).filter(|f| !f.is_empty());
        match (folder, &self.s3) {
            (Some(_), Some(_)) => anyhow::bail!("[sync] sets both `folder` and `s3`; pick one"),
            (Some(f), None) => Ok(Some(SyncDestination::Folder(expand_tilde(f)))),
            (None, Some(s3)) => {
                if s3.bucket.trim().is_empty() {
                    anyhow::bail!("[sync.s3] needs a `bucket`");
                }
                if s3.endpoint.trim().is_empty() {
                    anyhow::bail!("[sync.s3] needs an `endpoint`");
                }
                Ok(Some(SyncDestination::S3(s3.clone())))
            }
            (None, None) => Ok(None),
        }
    }

    /// Whether the automatic step applies to `kind`.
    pub fn applies_to(&self, kind: MediaKind) -> bool {
        self.enabled && self.kinds.contains(&kind)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_defaults_and_folder() {
        let cfg: Config = toml::from_str("[sync]\nenabled = true\nfolder = \"~/Sync/Bugle\"\n").unwrap();
        assert!(cfg.sync.enabled);
        assert_eq!(cfg.sync.mode, SyncMode::Copy);
        assert!(cfg.sync.applies_to(MediaKind::Screenshot));
        assert!(!cfg.sync.applies_to(MediaKind::Subtitle));
        match cfg.sync.destination().unwrap() {
            Some(SyncDestination::Folder(p)) => assert!(p.ends_with("Sync/Bugle") && !p.starts_with("~")),
            other => panic!("unexpected {other:?}"),
        }
        assert!(Config::default().sync.destination().unwrap().is_none());
    }

    #[test]
    fn sync_rejects_two_destinations() {
        let cfg: Config = toml::from_str(
            "[sync]\nfolder = \"~/x\"\nmode = \"move\"\nkinds = [\"screenshot\"]\n[sync.s3]\nbucket = \"b\"\nendpoint = \"http://127.0.0.1:9000\"\n",
        )
        .unwrap();
        assert_eq!(cfg.sync.mode, SyncMode::Move);
        assert_eq!(cfg.sync.kinds, vec![MediaKind::Screenshot]);
        assert!(cfg.sync.destination().is_err());
    }

    #[test]
    fn removed_keys_in_old_configs_still_parse() {
        let cfg: Config = toml::from_str(
            "[screenshots]\ndesktop_icons_hidden = true\nself_timer_seconds = 3.0\njpeg_quality = 70\n\
             [markup]\npadding = 10\ncorner_radius = 4\nshadow = false\nfont_size = 20.0\n",
        )
        .unwrap();
        assert_eq!(cfg.screenshots.jpeg_quality, 70);
        assert_eq!(cfg.markup.font_size, 20.0);
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../config.example.toml");
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.general.filename_template, crate::naming::DEFAULT_TEMPLATE);
        for key in ["desktop_icons_hidden", "self_timer_seconds", "corner_radius", "{counter}"] {
            assert!(!text.contains(key), "config.example.toml still documents {key}");
        }
    }
}
