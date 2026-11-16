//! Core types shared by every Bugle crate: geometry, capture specs, media
//! records, configuration and the local history store.
//!
//! Nothing in here touches the OS; the platform crate does that.

pub mod audio_mix;
pub mod config;
pub mod geometry;
pub mod history;
pub mod media_item;
pub mod naming;
pub mod spec;

pub use config::Config;
pub use geometry::{Point, Rect, Size};
pub use history::History;
pub use media_item::{MediaItem, MediaKind};
pub use spec::*;

/// Crate-wide error type. Most engine errors are `anyhow`, but a few
/// well-known failure modes get their own variant so callers (HTTP/MCP)
/// can map them to sensible status codes.
#[derive(Debug, thiserror::Error)]
pub enum BugleError {
    #[error("screen capture permission has not been granted (System Settings → Privacy & Security → Screen Recording)")]
    PermissionDenied,
    #[error("no such display: {0}")]
    NoSuchDisplay(u32),
    #[error("no such window: {0}")]
    NoSuchWindow(String),
    #[error("media not found: {0}")]
    MediaNotFound(String),
    #[error("provider not configured: {0}")]
    ProviderNotConfigured(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("not supported on this platform: {0}")]
    Unsupported(String),
    #[error("selection cancelled")]
    Cancelled,
}
