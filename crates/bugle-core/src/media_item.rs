use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Screenshot,
    ScrollCapture,
    Annotated,
    Video,
    Gif,
    Audio,
    Subtitle,
    Text,
}

/// One item of local history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    pub id: String,
    pub kind: MediaKind,
    pub path: PathBuf,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    #[serde(default)]
    pub label: Option<String>,
    /// Free-form description of how it was produced (target, window title…).
    #[serde(default)]
    pub source: Option<String>,
    /// Id of the media this one was derived from (annotation, OCR, …).
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub bytes: Option<u64>,
}

impl MediaItem {
    pub fn new(kind: MediaKind, path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let bytes = std::fs::metadata(&path).ok().map(|m| m.len());
        Self {
            id: short_id(),
            kind,
            path,
            created_at: Utc::now(),
            width: None,
            height: None,
            duration_seconds: None,
            label: None,
            source: None,
            parent: None,
            bytes,
        }
    }

    pub fn with_size(mut self, w: u32, h: u32) -> Self {
        self.width = Some(w);
        self.height = Some(h);
        self
    }

    pub fn is_image(&self) -> bool {
        matches!(self.kind, MediaKind::Screenshot | MediaKind::ScrollCapture | MediaKind::Annotated)
    }

    pub fn is_video(&self) -> bool {
        matches!(self.kind, MediaKind::Video | MediaKind::Gif)
    }
}

/// Short, URL-safe ids (12 hex chars from a v4 uuid) — long enough to never
/// collide locally, short enough to type.
pub fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}
