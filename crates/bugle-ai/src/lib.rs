//! AI provider abstraction.
//!
//! Three roles, each a trait:
//! * [`VisionModel`] — describe one or more images (screenshot → text).
//! * [`TextModel`] — plain completion (summarise OCR output, name a file…).
//! * [`Transcriber`] — audio → timed transcript (recording → subtitles).
//!
//! A [`Provider`] is one configured endpoint and implements whichever roles
//! its `kind` supports. The [`Registry`] resolves names and defaults.
//!
//! Supported kinds: `openai` (any OpenAI-compatible server: llama.cpp,
//! LM Studio, vLLM, Ollama's `/v1`, OpenAI, OpenRouter…), `anthropic`,
//! `ollama` (native API), `whisper_cpp` (whisper.cpp server), `command`
//! (a local executable).

pub mod providers;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bugle_core::config::{AiConfig, ProviderConfig};
use bugle_core::{BugleError, Transcript};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

pub use providers::Provider;

/// An image handed to a vision model.
#[derive(Clone)]
pub struct ImageInput {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

impl ImageInput {
    pub fn png(bytes: Vec<u8>) -> Self {
        Self { bytes, mime: "image/png" }
    }
    pub fn jpeg(bytes: Vec<u8>) -> Self {
        Self { bytes, mime: "image/jpeg" }
    }
}

#[async_trait]
pub trait VisionModel: Send + Sync {
    async fn describe(&self, images: &[ImageInput], prompt: &str, system: Option<&str>) -> Result<String>;
}

#[async_trait]
pub trait TextModel: Send + Sync {
    async fn complete(&self, prompt: &str, system: Option<&str>) -> Result<String>;
}

#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn transcribe(&self, audio: &Path, language: Option<&str>) -> Result<Transcript>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Vision,
    Text,
    Transcribe,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Role::Vision => "vision",
            Role::Text => "text",
            Role::Transcribe => "transcriber",
        })
    }
}

/// Named providers plus per-role defaults.
pub struct Registry {
    providers: BTreeMap<String, Arc<Provider>>,
    default_vision: Option<String>,
    default_text: Option<String>,
    default_transcriber: Option<String>,
}

impl Registry {
    pub fn from_config(cfg: &AiConfig) -> Self {
        let providers = cfg
            .providers
            .iter()
            .map(|(name, pc)| (name.clone(), Arc::new(Provider::new(name.clone(), pc.clone()))))
            .collect();
        Self {
            providers,
            default_vision: cfg.default_vision.clone(),
            default_text: cfg.default_text.clone(),
            default_transcriber: cfg.default_transcriber.clone(),
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<Provider>> {
        self.providers.get(name).cloned()
    }

    /// Resolve `name` (or the role default) to a provider that supports `role`.
    pub fn resolve(&self, name: Option<&str>, role: Role) -> Result<Arc<Provider>> {
        let default = match role {
            Role::Vision => self.default_vision.as_deref(),
            Role::Text => self.default_text.as_deref(),
            Role::Transcribe => self.default_transcriber.as_deref(),
        };
        let name = name.or(default).ok_or_else(|| {
            BugleError::ProviderNotConfigured(format!("no default {role} provider; set ai.default_{} in the config", role_key(role)))
        })?;
        let p = self
            .providers
            .get(name)
            .cloned()
            .ok_or_else(|| BugleError::ProviderNotConfigured(format!("unknown provider '{name}'")))?;
        if !p.capable(role) {
            return Err(anyhow!("provider '{name}' (kind {}) does not support {role}", p.kind()));
        }
        if let Some(reason) = p.not_ready_reason() {
            return Err(BugleError::ProviderNotConfigured(format!("provider '{name}' is not ready: {reason}")).into());
        }
        Ok(p)
    }

    /// One JSON object per provider. `roles` lists what the provider's kind
    /// offers; `ready` is false (with a `reason`) while required settings
    /// such as `base_url` or `model` are missing.
    pub fn describe_all(&self) -> Vec<serde_json::Value> {
        let roles = [Role::Vision, Role::Text, Role::Transcribe];
        self.providers
            .values()
            .map(|p| {
                let supported: Vec<String> = roles.iter().filter(|r| p.capable(**r)).map(|r| r.to_string()).collect();
                let reason = p.not_ready_reason();
                let defaults: Vec<String> = [
                    (Role::Vision, self.default_vision.as_deref()),
                    (Role::Text, self.default_text.as_deref()),
                    (Role::Transcribe, self.default_transcriber.as_deref()),
                ]
                .iter()
                .filter(|(_, d)| *d == Some(p.name()))
                .map(|(r, _)| r.to_string())
                .collect();
                serde_json::json!({
                    "name": p.name(),
                    "kind": p.kind(),
                    "model": p.model(),
                    "base_url": p.base_url(),
                    "roles": supported,
                    "default_for": defaults,
                    "ready": reason.is_none(),
                    "reason": reason,
                })
            })
            .collect()
    }
}

fn role_key(r: Role) -> &'static str {
    match r {
        Role::Vision => "vision",
        Role::Text => "text",
        Role::Transcribe => "transcriber",
    }
}

/// Build a one-off provider without a registry (used by tests and the CLI's
/// `--provider-url` escape hatch).
pub fn adhoc(kind: &str, base_url: &str, model: Option<&str>) -> Provider {
    Provider::new(
        "adhoc".into(),
        ProviderConfig {
            kind: kind.into(),
            base_url: Some(base_url.into()),
            model: model.map(|s| s.to_string()),
            ..Default::default()
        },
    )
}
