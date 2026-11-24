//! Sync step: after a file is saved, copy or move it into a folder that
//! Dropbox / iCloud Drive mirrors, or upload it to an S3-compatible bucket
//! with SigV4-signed requests. Nothing here talks to a hosted sharing
//! service; credentials are read from the environment and never stored.
//!
//! Each media kind can have its own destination (`[sync.destinations.*]`).
//! Subtitle sidecars (`clip.srt` / `clip.vtt`) follow their video to the
//! video's destination under the video's synced name. Large files go up as
//! an S3 multipart upload streamed from disk one part at a time.
//!
//! The SigV4 signer is hand-rolled on top of `ring` (already in the
//! dependency graph through `rustls`) so no S3 SDK is pulled in.

use crate::Engine;
use anyhow::{anyhow, bail, Context, Result};
use bugle_core::config::{S3Config, SyncDestination, SyncMode};
use bugle_core::history::SyncRecord;
use bugle_core::{BugleError, Config, MediaItem, MediaKind};
use ring::{digest, hmac};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Sidecar subtitle extensions synced along with a video.
const SIDECAR_EXTENSIONS: [&str; 2] = ["srt", "vtt"];

/// What one sync did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncReport {
    /// `copied`, `moved`, `uploaded` or `already there`.
    pub action: String,
    /// Where the file went: a folder path or the object URL.
    pub destination: String,
    /// Where the local file lives now (changes after a move).
    pub path: PathBuf,
    /// Object URL for bucket uploads.
    pub url: Option<String>,
    /// Sidecar subtitles synced along with a video.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sidecars: Vec<SyncReport>,
}

impl Engine {
    /// The default destination, or an error when `[sync]` is inconsistent.
    pub fn sync_destination(&self) -> Result<Option<SyncDestination>> {
        self.config.sync.destination()
    }

    /// Short label for the default destination (`iCloud Drive/Bugle`,
    /// `~/Dropbox/x`, `s3://bucket/prefix`), for toasts and status lines.
    pub fn sync_destination_label(&self) -> Option<String> {
        destination_label(&self.config.sync.destination().ok()??)
    }

    /// Like [`Engine::sync_destination_label`], for the destination `kind`
    /// is synced to.
    pub fn sync_destination_label_for(&self, kind: MediaKind) -> Option<String> {
        destination_label(&self.config.sync.destination_for(kind).ok()??)
    }

    /// Automatic step after a successful save, for an item that is already
    /// in history: honours `enabled` and `kinds`, records a new path / URL
    /// in history, and never fails the save (problems are logged). It runs
    /// on the caller's thread, so a long upload holds up the caller until
    /// it finishes, but the item is in history from the start.
    pub(crate) fn sync_after_save(&self, item: &mut MediaItem) {
        if !self.config.sync.applies_to(item.kind) {
            return;
        }
        let original = item.path.clone();
        match self.sync_item(item) {
            Ok(Some(r)) => {
                tracing::info!("{} {} → {}", r.action, original.display(), r.destination);
                self.record_sync(item, Some(&original));
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("sync of {} failed: {e:#}", original.display()),
        }
    }

    /// Store `item`'s new path / URL / synced name in history.
    /// `recorded_path` is the path its history entry had before the sync
    /// (`None` when it was not in history).
    ///
    /// If the entry has gone meanwhile (see [`History::record_synced`]): a
    /// prune that saw the old path missing mid-move is undone, so the moved
    /// file keeps its entry; a user's delete (`bugle delete`, `DELETE
    /// /v1/media/{id}`) stays, and when it asked for the file to go, the
    /// moved file and event track it could not see are removed here.
    ///
    /// [`History::record_synced`]: bugle_core::History::record_synced
    fn record_sync(&self, item: &MediaItem, recorded_path: Option<&Path>) {
        let Some(recorded_path) = recorded_path else { return };
        let moved = recorded_path != item.path;
        match self.history.record_synced(item, moved) {
            Ok(SyncRecord::Restored) => tracing::warn!("history entry {} vanished during its sync; recorded it again", item.id),
            Ok(SyncRecord::Deleted { remove_file }) => {
                tracing::info!("history entry {} was deleted during its sync; it stays deleted", item.id);
                if remove_file && moved {
                    for p in std::iter::once(&item.path).chain(item.events.as_ref()).filter(|p| p.is_file()) {
                        if let Err(e) = std::fs::remove_file(p) {
                            tracing::warn!("could not remove {} after its entry was deleted: {e}", p.display());
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("could not record the sync of {}: {e:#}", item.path.display()),
        }
    }

    /// Sync one item to its destination, updating `item.path` (after a
    /// move), `item.url` (after an upload) and `item.synced_as`. A video
    /// takes its sidecar subtitles along; a subtitle whose video is known
    /// follows that video. `Ok(None)` when no destination is configured.
    /// Does not record `item` in history (sidecars already there are).
    pub fn sync_item(&self, item: &mut MediaItem) -> Result<Option<SyncReport>> {
        if !item.path.is_file() {
            return Err(BugleError::MediaNotFound(item.path.display().to_string()).into());
        }
        if item.kind == MediaKind::Subtitle && self.config.sync.sidecars {
            if let Some(video) = self.video_of_subtitle(item)? {
                return self.sync_sidecar(&video, item, None);
            }
        }
        let Some(dest) = self.config.sync.destination_for(item.kind)? else { return Ok(None) };
        let mode = self.config.sync.mode_for(item.kind);
        let original = item.path.clone();
        let mut report = self.sync_file(&dest, mode, item, &Naming::Local, true)?;
        if self.config.sync.sidecars && item.is_video() {
            report.sidecars = self.sync_sidecars_of(&original, item);
            report.sidecars.extend(self.sync_event_track_of(&dest, mode, item));
        }
        Ok(Some(report))
    }

    /// Send a video's cursor event track along under the video's synced
    /// name (`clip 2.mp4` → `clip 2.mp4.events.json`); a move updates
    /// `item.events`. Failures are logged.
    fn sync_event_track_of(&self, dest: &SyncDestination, mode: SyncMode, item: &mut MediaItem) -> Option<SyncReport> {
        let path = item.events.clone().filter(|p| p.is_file())?;
        let name = format!("{}.events.json", item.synced_as.as_deref()?);
        let mut track = MediaItem::new(item.kind, &path);
        match self.sync_file(dest, mode, &mut track, &Naming::Sidecar { name, replace: true }, false) {
            Ok(r) => {
                item.events = Some(track.path);
                Some(r)
            }
            Err(e) => {
                tracing::warn!("sync of the event track {} failed: {e:#}", path.display());
                None
            }
        }
    }

    /// `bugle sync <source>`: sync an existing capture on demand, whatever
    /// `enabled` and `kinds` say, and record the new path / URL in history.
    pub fn sync_media(&self, source: &str) -> Result<(MediaItem, SyncReport)> {
        let mut item = self.resolve_media(source)?;
        let recorded = matches!(self.history.get(&item.id), Ok(Some(_))).then(|| item.path.clone());
        let report = self.sync_item(&mut item)?.ok_or_else(|| {
            anyhow!(
                "no sync destination configured for {}: set [sync] folder or [sync.s3] in {}",
                kind_name(item.kind),
                Config::default_path().display()
            )
        })?;
        self.record_sync(&item, recorded.as_deref());
        Ok((item, report))
    }

    /// Record a subtitle file `subtitles` just wrote for `video` in history,
    /// then sync it: as a sidecar when its video was synced or syncs
    /// automatically, or on its own when `subtitle` is in `kinds` / has an
    /// entry. An explicit `output` location is never emptied: the sync then
    /// copies even in move mode. Returns the item with its current path.
    pub(crate) fn save_subtitle_item(
        &self,
        video: &MediaItem,
        path: &Path,
        source: String,
        explicit_output: bool,
    ) -> Result<MediaItem> {
        let mut m = MediaItem::new(MediaKind::Subtitle, path);
        m.parent = Some(video.id.clone());
        m.source = Some(source);
        self.history.push(&m)?;
        let sync = &self.config.sync;
        let as_sidecar = sync.sidecars
            && video.is_video()
            && (video.synced_as.is_some() || sync.applies_to(video.kind) || sync.applies_to(MediaKind::Subtitle));
        let result = if as_sidecar {
            self.sync_sidecar(video, &mut m, explicit_output.then_some(SyncMode::Copy))
        } else if sync.applies_to(MediaKind::Subtitle) {
            let mode = if explicit_output { SyncMode::Copy } else { sync.mode_for(MediaKind::Subtitle) };
            match sync.destination_for(MediaKind::Subtitle) {
                Ok(Some(dest)) => self.sync_file(&dest, mode, &mut m, &Naming::Local, false).map(Some),
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            }
        } else {
            Ok(None)
        };
        match result {
            Ok(Some(r)) => {
                tracing::info!("{} {} → {}", r.action, path.display(), r.destination);
                self.record_sync(&m, Some(path));
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("sync of {} failed: {e:#}", path.display()),
        }
        Ok(m)
    }

    /// Where a video's subtitles go: an explicit `[sync.destinations.subtitle]`
    /// entry wins; otherwise they follow the video.
    fn sidecar_destination(&self, video: &MediaItem) -> Result<Option<(SyncDestination, SyncMode)>> {
        let sync = &self.config.sync;
        let kind = if sync.has_entry(MediaKind::Subtitle) { MediaKind::Subtitle } else { video.kind };
        Ok(sync.destination_for(kind)?.map(|d| (d, sync.mode_for(kind))))
    }

    /// Sync one subtitle of `video` (see [`Engine::sidecar_naming`]).
    /// `mode` overrides the configured mode.
    fn sync_sidecar(&self, video: &MediaItem, sub: &mut MediaItem, mode: Option<SyncMode>) -> Result<Option<SyncReport>> {
        let Some((dest, configured)) = self.sidecar_destination(video)? else { return Ok(None) };
        let naming = self.sidecar_naming(video, &sub.path, &dest)?;
        self.sync_file(&dest, mode.unwrap_or(configured), sub, &naming, false).map(Some)
    }

    /// The name a subtitle of `video` should get at `dest`:
    /// * a name an earlier sidecar of this video (same extension) was
    ///   synced as, which may be replaced;
    /// * else `{stem the video was synced as}.{ext}`, replacing a file of
    ///   that name only when it is identical (a unique variant otherwise);
    /// * else, when the synced name is unknown, the video's own stem only
    ///   where it is certain (the video lives in that folder, or a bucket key
    ///   is the local name), and the subtitle's local name made unique
    ///   anywhere else.
    fn sidecar_naming(&self, video: &MediaItem, sub: &Path, dest: &SyncDestination) -> Result<Naming> {
        let ext = sub.extension().and_then(|e| e.to_str()).unwrap_or("srt").to_string();
        let earlier = self.history.all()?.into_iter().rev().find_map(|m| {
            let same_video = m.kind == MediaKind::Subtitle && m.parent.as_deref() == Some(video.id.as_str());
            let name = m.synced_as.filter(|n| same_video && Path::new(n).extension().and_then(|e| e.to_str()) == Some(ext.as_str()))?;
            Some(name)
        });
        if let Some(name) = earlier {
            return Ok(Naming::Sidecar { name, replace: true });
        }
        let stem = match &video.synced_as {
            Some(synced) => Some(stem_of(Path::new(synced))?),
            None => match dest {
                SyncDestination::Folder(dir) if in_dir(&video.path, dir) => Some(stem_of(&video.path)?),
                SyncDestination::S3(_) => Some(stem_of(&video.path)?),
                SyncDestination::Folder(_) => None,
            },
        };
        Ok(match stem {
            Some(stem) => Naming::Sidecar { name: format!("{stem}.{ext}"), replace: false },
            None => Naming::Local,
        })
    }

    /// The video a subtitle belongs to: its `parent`, else a history video
    /// whose path differs only in the extension.
    fn video_of_subtitle(&self, sub: &MediaItem) -> Result<Option<MediaItem>> {
        if let Some(parent) = sub.parent.as_deref() {
            if let Ok(Some(p)) = self.history.get(parent) {
                if p.is_video() {
                    return Ok(Some(p));
                }
            }
        }
        Ok(self.history.all()?.into_iter().rev().find(|m| m.is_video() && same_stem(&m.path, &sub.path)))
    }

    /// Sync the `.srt` / `.vtt` files next to `original` (the video's path
    /// before a move). Failures are logged and left out of the report.
    fn sync_sidecars_of(&self, original: &Path, video: &MediaItem) -> Vec<SyncReport> {
        let candidates: Vec<PathBuf> =
            SIDECAR_EXTENSIONS.iter().map(|ext| original.with_extension(ext)).filter(|p| p.is_file()).collect();
        if candidates.is_empty() {
            return Vec::new();
        }
        let history = self.history.all().unwrap_or_default();
        let mut reports = Vec::new();
        for path in candidates {
            let known = history.iter().rev().find(|m| m.kind == MediaKind::Subtitle && m.path == path).cloned();
            let mut sub = known.clone().unwrap_or_else(|| {
                let mut m = MediaItem::new(MediaKind::Subtitle, &path);
                m.parent = Some(video.id.clone());
                m
            });
            match self.sync_sidecar(video, &mut sub, None) {
                Ok(Some(r)) => {
                    if known.is_some() {
                        self.record_sync(&sub, Some(&path));
                    } else if let Err(e) = self.history.push(&sub) {
                        // Recorded so a later re-sync knows the file is ours.
                        tracing::warn!("could not record the synced sidecar {}: {e:#}", sub.path.display());
                    }
                    reports.push(r);
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("sync of sidecar {} failed: {e:#}", path.display()),
            }
        }
        reports
    }

    /// Send `item` to `dest` under `naming`. `announce` copies an upload's
    /// URL to the clipboard.
    fn sync_file(
        &self,
        dest: &SyncDestination,
        mode: SyncMode,
        item: &mut MediaItem,
        naming: &Naming,
        announce: bool,
    ) -> Result<SyncReport> {
        match dest {
            SyncDestination::Folder(dir) => sync_to_folder(dir, mode, item, naming),
            SyncDestination::S3(s3) => {
                // Keys are written like any upload: the same key is replaced.
                let name = match naming {
                    Naming::Sidecar { name, .. } => name.clone(),
                    Naming::Local => file_name(&item.path)?.to_string(),
                };
                let url = upload_to_s3_as(s3, &item.path, &name)?;
                item.url = Some(url.clone());
                item.synced_as = Some(name);
                if announce && !self.config.general.copy_to_clipboard {
                    if let Err(e) = bugle_platform::clipboard_write_text(&url) {
                        tracing::warn!("could not copy {url} to the clipboard: {e}");
                    }
                }
                Ok(SyncReport::new("uploaded", url.clone(), item.path.clone(), Some(url)))
            }
        }
    }
}

/// How a synced file is named at its destination.
#[derive(Debug, Clone, PartialEq)]
enum Naming {
    /// The local file name, made unique in a folder.
    Local,
    /// A sidecar's name. An existing file of that name is replaced when
    /// `replace` (Bugle wrote it for this video) or when it is identical;
    /// otherwise a unique variant is used.
    Sidecar { name: String, replace: bool },
}

impl SyncReport {
    fn new(action: &str, destination: String, path: PathBuf, url: Option<String>) -> Self {
        Self { action: action.into(), destination, path, url, sidecars: Vec::new() }
    }
}

fn destination_label(dest: &SyncDestination) -> Option<String> {
    Some(match dest {
        SyncDestination::Folder(dir) => folder_label(dir),
        SyncDestination::S3(s3) => format!("s3://{}/{}", s3.bucket, normalise_prefix(&s3.prefix)),
    })
}

fn kind_name(kind: MediaKind) -> String {
    serde_json::to_value(kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn file_name(path: &Path) -> Result<&str> {
    path.file_name().and_then(|n| n.to_str()).ok_or_else(|| anyhow!("{} has no file name", path.display()))
}

fn stem_of(path: &Path) -> Result<String> {
    path.file_stem().and_then(|s| s.to_str()).map(str::to_string).ok_or_else(|| anyhow!("{} has no file name", path.display()))
}

fn same_stem(a: &Path, b: &Path) -> bool {
    a.parent() == b.parent() && a.file_stem().is_some() && a.file_stem() == b.file_stem()
}

/// Whether `path` sits directly in `dir` (both resolved).
fn in_dir(path: &Path, dir: &Path) -> bool {
    match (path.parent().and_then(|p| p.canonicalize().ok()), dir.canonicalize()) {
        (Some(parent), Ok(dir)) => parent == dir,
        _ => false,
    }
}

/// Byte-for-byte equality, without reading either file whole.
fn same_contents(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else { return false };
    if ma.len() != mb.len() {
        return false;
    }
    let (Ok(mut fa), Ok(mut fb)) = (std::fs::File::open(a), std::fs::File::open(b)) else { return false };
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let Ok(n) = fa.read(&mut ba) else { return false };
        if n == 0 {
            return true;
        }
        if fb.read_exact(&mut bb[..n]).is_err() || ba[..n] != bb[..n] {
            return false;
        }
    }
}

// ---------------------------------------------------------------------------
// Folder destination
// ---------------------------------------------------------------------------

fn sync_to_folder(dir: &Path, mode: SyncMode, item: &mut MediaItem, naming: &Naming) -> Result<SyncReport> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating sync folder {}", dir.display()))?;
    let label = folder_label(dir);
    let dest = match naming {
        Naming::Sidecar { name, replace } => {
            let exact = dir.join(name);
            let taken = exact.exists() && !is_same_file(&item.path, &exact);
            if taken && !replace && !same_contents(&item.path, &exact) {
                unique_in_dir(dir, name)
            } else {
                exact
            }
        }
        Naming::Local => {
            if in_dir(&item.path, dir) {
                item.synced_as = Some(file_name(&item.path)?.to_string());
                return Ok(SyncReport::new("already there", label, item.path.clone(), None));
            }
            unique_in_dir(dir, file_name(&item.path)?)
        }
    };
    if is_same_file(&item.path, &dest) {
        item.synced_as = Some(file_name(&dest)?.to_string());
        return Ok(SyncReport::new("already there", label, item.path.clone(), None));
    }
    let action = match mode {
        SyncMode::Copy => {
            std::fs::copy(&item.path, &dest).with_context(|| format!("copying to {}", dest.display()))?;
            "copied"
        }
        SyncMode::Move => {
            move_file(&item.path, &dest)?;
            item.path = dest.clone();
            "moved"
        }
    };
    item.synced_as = Some(file_name(&dest)?.to_string());
    Ok(SyncReport::new(action, label, item.path.clone(), None))
}

fn is_same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// `name`, or `name 2`, `name 3`, … when it is taken.
fn unique_in_dir(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, Some(e)),
        _ => (name, None),
    };
    for n in 2..10_000 {
        let candidate = match ext {
            Some(e) => dir.join(format!("{stem} {n}.{e}")),
            None => dir.join(format!("{stem} {n}")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{} {}", bugle_core::media_item::short_id(), name))
}

/// Rename, falling back to copy + delete across volumes.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).with_context(|| format!("copying to {}", to.display()))?;
    std::fs::remove_file(from).with_context(|| format!("removing {} after copy", from.display()))?;
    Ok(())
}

/// `~/Library/Mobile Documents/com~apple~CloudDocs/X` → `iCloud Drive/X`,
/// `$HOME/Dropbox/X` → `~/Dropbox/X`.
pub fn folder_label(dir: &Path) -> String {
    let s = dir.display().to_string();
    if let Some(home) = dirs::home_dir() {
        let home = home.display().to_string();
        let icloud = format!("{home}/Library/Mobile Documents/com~apple~CloudDocs");
        if let Some(rest) = s.strip_prefix(&icloud) {
            return format!("iCloud Drive{rest}");
        }
        if let Some(rest) = s.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    s
}

// ---------------------------------------------------------------------------
// S3-compatible destination
// ---------------------------------------------------------------------------

const MIB: u64 = 1024 * 1024;
/// S3's smallest part (except the last), largest part and largest part count.
const MIN_PART_SIZE: u64 = 5 * MIB;
const MAX_PART_SIZE: u64 = 5120 * MIB;
const MAX_PARTS: u64 = 10_000;
/// Read size when hashing or streaming a file: the most of it held in memory.
const CHUNK: usize = 256 * 1024;

/// Access key pair read from the environment variables `[sync.s3]` names.
#[derive(Clone)]
pub struct S3Credentials {
    pub access_key: String,
    pub secret_key: String,
}

impl S3Credentials {
    pub fn from_env(cfg: &S3Config) -> Result<Self> {
        Ok(Self { access_key: env_var(&cfg.access_key_env)?, secret_key: env_var(&cfg.secret_key_env)? })
    }
}

/// When to switch to multipart, how big the parts are and how hard to retry.
#[derive(Debug, Clone)]
pub struct UploadTuning {
    /// Files of at least this many bytes go up as multipart uploads.
    pub multipart_threshold: u64,
    /// Bytes per part (the last one is shorter); raised when the file would
    /// need more than 10 000 parts.
    pub part_size: u64,
    /// Attempts per request (create, each part, complete) before giving up.
    pub attempts: u32,
    /// Wait before the second attempt; doubled for each one after that, up
    /// to `max_backoff`, with jitter. A server's `Retry-After` is honoured
    /// up to two minutes.
    pub backoff: Duration,
    pub max_backoff: Duration,
}

impl UploadTuning {
    /// Clamped to the S3 limits whatever the config says (the config is also
    /// validated, so out-of-range values normally never get here).
    pub fn from_config(cfg: &S3Config) -> Self {
        Self {
            multipart_threshold: cfg.multipart_threshold_mb.clamp(5, 5120).saturating_mul(MIB),
            part_size: cfg.part_size_mb.saturating_mul(MIB).clamp(MIN_PART_SIZE, MAX_PART_SIZE),
            attempts: 6,
            backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }

    fn part_size_for(&self, size: u64) -> u64 {
        self.part_size.max(1).max(size.div_ceil(MAX_PARTS))
    }
}

/// Per-request deadline: a minute plus the body at a pessimistic 128 KiB/s,
/// so a large part on a slow link is not cut off while it is moving.
pub fn request_timeout(body_len: u64) -> Duration {
    Duration::from_secs(60u64.saturating_add(body_len / (128 * 1024)))
}

/// Upload the file as `{prefix}/{file name}` and return its public URL.
pub fn upload_to_s3(cfg: &S3Config, path: &Path) -> Result<String> {
    upload_to_s3_as(cfg, path, file_name(path)?)
}

/// Upload the file as `{prefix}/{name}` and return its public URL.
pub fn upload_to_s3_as(cfg: &S3Config, path: &Path, name: &str) -> Result<String> {
    let creds = S3Credentials::from_env(cfg)?;
    upload_with(cfg, &creds, path, name, &UploadTuning::from_config(cfg))
}

/// The upload itself: one streamed `PUT` below the multipart threshold,
/// otherwise create / upload-part × n / complete, aborting on failure.
pub fn upload_with(cfg: &S3Config, creds: &S3Credentials, path: &Path, name: &str, tuning: &UploadTuning) -> Result<String> {
    let key = format!("{}{name}", normalise_prefix(&cfg.prefix));
    let size = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?.len();
    let target = S3Target::new(cfg, creds.clone(), &key)?;
    let url = target.object_url();
    let content_type = content_type_for(name);
    let path = path.to_path_buf();
    let tuning = tuning.clone();
    block_on(async move {
        let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(20)).build()?;
        if size >= tuning.multipart_threshold && size > 0 {
            multipart_upload(&client, &target, &path, size, content_type, &tuning).await
        } else {
            let hash = hash_range(&path, 0, size)?;
            let what = format!("PUT {}", target.object_url());
            with_retry(&tuning, &what, |_| {
                let call = Call::new("PUT", Vec::new(), &hash).content_type(content_type).body(range_body(&path, 0, size), size);
                send(&client, &target, call)
            })
            .await?;
            ensure_unchanged(&path, size)
        }
    })?;
    let public = match cfg.public_url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(base) => format!("{}/{}", base.trim_end_matches('/'), uri_encode(&key, false)),
        None => url,
    };
    Ok(public)
}

/// Fail when the file is no longer `size` bytes: it grew (the extra bytes
/// were not sent) or shrank while it was being read.
fn ensure_unchanged(path: &Path, size: u64) -> Result<()> {
    let now = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?.len();
    if now != size {
        bail!("{} changed while it was being uploaded ({size} → {now} bytes); sync it again once it is complete", path.display());
    }
    Ok(())
}

/// One object in one bucket, and how to sign requests for it.
struct S3Target {
    endpoint: String,
    host: String,
    /// `/{bucket}/{key}`, URI-encoded.
    canonical_uri: String,
    region: String,
    creds: S3Credentials,
}

impl S3Target {
    fn new(cfg: &S3Config, creds: S3Credentials, key: &str) -> Result<Self> {
        let endpoint = cfg.endpoint.trim().trim_end_matches('/').to_string();
        let host = host_of(&endpoint)?;
        let canonical_uri = format!("/{}/{}", uri_encode(cfg.bucket.trim(), false), uri_encode(key, false));
        Ok(Self { endpoint, host, canonical_uri, region: cfg.region.trim().to_string(), creds })
    }

    fn object_url(&self) -> String {
        format!("{}{}", self.endpoint, self.canonical_uri)
    }

    /// Request URL plus the headers to send (`authorization` included) for
    /// a request signed now. `headers` are extra signed headers.
    fn sign(
        &self,
        method: &str,
        query: &[(&str, &str)],
        payload_hash: &str,
        mut headers: Vec<(String, String)>,
    ) -> (String, Vec<(String, String)>) {
        let canonical_query = canonical_query(query);
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        headers.push(("x-amz-content-sha256".into(), payload_hash.into()));
        headers.push(("x-amz-date".into(), amz_date.clone()));
        let mut signed = headers.clone();
        signed.push(("host".into(), self.host.clone()));
        let authorization = sigv4_authorization(&SigV4Request {
            method,
            canonical_uri: &self.canonical_uri,
            canonical_query: &canonical_query,
            headers: &signed,
            payload_hash,
            amz_date: &amz_date,
            region: &self.region,
            service: "s3",
            access_key: &self.creds.access_key,
            secret_key: &self.creds.secret_key,
        });
        headers.push(("authorization".into(), authorization));
        let url = if canonical_query.is_empty() {
            self.object_url()
        } else {
            format!("{}?{canonical_query}", self.object_url())
        };
        (url, headers)
    }
}

/// SigV4 canonical query string: names and values URI-encoded (slashes
/// too), sorted by name, `name=` for an empty value. Also used verbatim as
/// the request's query so the two never disagree.
pub fn canonical_query(query: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k, true), uri_encode(v, true))).collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// A failed attempt, whether trying again could help, and what the
/// server said about it.
struct Attempt {
    error: anyhow::Error,
    retry: bool,
    /// The S3 error `<Code>` (`NoSuchUpload`, `RequestTimeout`, …).
    code: Option<String>,
    retry_after: Option<Duration>,
}

impl Attempt {
    fn fatal(error: anyhow::Error) -> Self {
        Self { error, retry: false, code: None, retry_after: None }
    }
}

/// A finished request: headers we care about and the body text.
struct Reply {
    etag: Option<String>,
    content_length: Option<u64>,
    text: String,
}

/// One request to send: everything but the signature.
struct Call<'a> {
    method: &'a str,
    query: Vec<(&'a str, &'a str)>,
    payload_hash: &'a str,
    /// Extra signed headers (`content-type`).
    headers: Vec<(String, String)>,
    /// Streamed body and its exact length; `None` for an empty body.
    body: Option<(reqwest::Body, u64)>,
}

impl<'a> Call<'a> {
    fn new(method: &'a str, query: Vec<(&'a str, &'a str)>, payload_hash: &'a str) -> Self {
        Self { method, query, payload_hash, headers: Vec::new(), body: None }
    }

    fn content_type(mut self, ct: &str) -> Self {
        self.headers.push(("content-type".into(), ct.into()));
        self
    }

    fn body(mut self, body: reqwest::Body, len: u64) -> Self {
        self.body = Some((body, len));
        self
    }
}

/// Sign and send one request. Transport errors, 5xx, 429 and
/// `RequestTimeout` are worth retrying; other 4xx are not. The length is
/// always sent (and signed) as `content-length`, so a streamed body never
/// falls back to chunked transfer encoding, which S3 rejects.
async fn send(client: &reqwest::Client, target: &S3Target, call: Call<'_>) -> std::result::Result<Reply, Attempt> {
    let Call { method, query, payload_hash, mut headers, body } = call;
    let len = body.as_ref().map(|(_, n)| *n).unwrap_or(0);
    headers.push(("content-length".into(), len.to_string()));
    let (url, headers) = target.sign(method, &query, payload_hash, headers);
    // Errors name the object, not the query (upload ids are noise).
    let what = format!("{method} {}", target.object_url());
    let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| Attempt::fatal(e.into()))?;
    let mut req = client.request(m, &url).timeout(request_timeout(len));
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some((body, _)) = body {
        req = req.body(body);
    }
    let resp = req.send().await.map_err(|e| Attempt {
        error: anyhow::Error::new(e).context(what.clone()),
        retry: true,
        code: None,
        retry_after: None,
    })?;
    let status = resp.status();
    let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let etag = header("etag");
    let content_length = header("content-length").and_then(|v| v.trim().parse().ok());
    let retry_after = header("retry-after").and_then(|v| parse_retry_after(&v));
    let text = resp.text().await.unwrap_or_default();
    // CompleteMultipartUpload can answer 200 with an <Error> document.
    let error_body = text.contains("<Error>");
    if !status.is_success() || error_body {
        let code = xml_value(&text, "Code");
        let retry = status.is_server_error()
            || status.as_u16() == 429
            || (status.is_success() && error_body)
            || matches!(code.as_deref(), Some("RequestTimeout" | "SlowDown" | "InternalError"));
        return Err(Attempt {
            error: anyhow!("{what} failed: {status}{}", error_summary(&text)),
            retry,
            code,
            retry_after,
        });
    }
    Ok(Reply { etag, content_length, text })
}

/// `: Code: Message` from an S3 error document. Other elements (the access
/// key id, the string to sign) are left out so they never reach logs or
/// agent-visible results; a body that is not an error document is dropped.
fn error_summary(text: &str) -> String {
    let field = |tag| xml_value(text, tag).map(|v| xml_unescape(&v)).map(|v| v.chars().take(300).collect::<String>());
    match (field("Code"), field("Message")) {
        (Some(c), Some(m)) => format!(": {c}: {m}"),
        (Some(c), None) => format!(": {c}"),
        (None, Some(m)) => format!(": {m}"),
        (None, None) => String::new(),
    }
}

/// `Retry-After` in seconds (the HTTP-date form is not worth a parser here).
fn parse_retry_after(v: &str) -> Option<Duration> {
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Run `f` (given the attempt number, from 1) until it succeeds, fails for
/// good, or attempts run out. Waits grow exponentially to `max_backoff`
/// with jitter; a `Retry-After` from the server is waited out (up to two
/// minutes) when it is longer.
async fn with_retry<F, Fut, T>(tuning: &UploadTuning, what: &str, mut f: F) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, Attempt>>,
{
    let attempts = tuning.attempts.max(1);
    for n in 1..=attempts {
        match f(n).await {
            Ok(v) => return Ok(v),
            Err(a) if a.retry && n < attempts => {
                let wait = backoff(tuning, n).max(a.retry_after.unwrap_or_default().min(Duration::from_secs(120)));
                tracing::warn!("{what}: attempt {n}/{attempts} failed, retrying in {wait:?}: {:#}", a.error);
                tokio::time::sleep(wait).await;
            }
            Err(a) => return Err(a.error.context(format!("{what} (attempt {n}/{attempts})"))),
        }
    }
    unreachable!("the loop returns on the last attempt")
}

/// Wait after failed attempt `n`: `backoff · 2^(n-1)` capped at
/// `max_backoff`, then scaled by a random factor in [0.5, 1).
fn backoff(tuning: &UploadTuning, n: u32) -> Duration {
    let full = tuning.backoff.saturating_mul(1u32 << (n - 1).min(16)).min(tuning.max_backoff);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let jitter = 0.5 + 0.5 * f64::from(nanos.wrapping_mul(2_654_435_761) % 1000) / 1000.0;
    full.mul_f64(jitter)
}

async fn multipart_upload(
    client: &reqwest::Client,
    target: &S3Target,
    path: &Path,
    size: u64,
    content_type: &str,
    tuning: &UploadTuning,
) -> Result<()> {
    let empty = sha256_hex(b"");
    // A create whose response was lost and is then retried leaves the
    // first upload open and unknown to us; a bucket lifecycle rule for
    // incomplete multipart uploads cleans those up (see the README).
    let reply = with_retry(tuning, "create multipart upload", |_| {
        send(client, target, Call::new("POST", vec![("uploads", "")], &empty).content_type(content_type))
    })
    .await?;
    let upload_id = xml_value(&reply.text, "UploadId")
        .map(|id| xml_unescape(&id))
        .ok_or_else(|| anyhow!("create multipart upload: the response has no UploadId"))?;

    let result = upload_parts_and_complete(client, target, path, size, tuning, &upload_id).await;
    if let Err(e) = result {
        let abort = with_retry(tuning, "abort multipart upload", |_| {
            send(client, target, Call::new("DELETE", vec![("uploadId", upload_id.as_str())], &empty))
        })
        .await;
        if let Err(a) = abort {
            tracing::warn!("{a:#}; the incomplete upload may linger until a bucket lifecycle rule removes it");
        }
        return Err(e);
    }
    Ok(())
}

async fn upload_parts_and_complete(
    client: &reqwest::Client,
    target: &S3Target,
    path: &Path,
    size: u64,
    tuning: &UploadTuning,
    upload_id: &str,
) -> Result<()> {
    let part_size = tuning.part_size_for(size);
    let count = size.div_ceil(part_size);
    let mut etags = Vec::with_capacity(count as usize);
    for n in 1..=count {
        let offset = (n - 1) * part_size;
        let len = part_size.min(size - offset);
        let hash = hash_range(path, offset, len)?;
        let (hash, number) = (hash.as_str(), n.to_string());
        let number = number.as_str();
        let what = format!("upload part {n}/{count}");
        let reply = with_retry(tuning, &what, |_| async move {
            let call = Call::new("PUT", vec![("partNumber", number), ("uploadId", upload_id)], hash).body(range_body(path, offset, len), len);
            let reply = send(client, target, call).await?;
            match reply.etag.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
                Some(etag) => Ok(etag.to_string()),
                None => Err(Attempt { error: anyhow!("part {n}: the response has no ETag"), retry: true, code: None, retry_after: None }),
            }
        })
        .await?;
        etags.push(reply);
    }
    // Every byte that was sent came from the first `size` bytes; a file
    // that grew meanwhile would be stored truncated, so stop here (and abort).
    ensure_unchanged(path, size)?;
    let body = complete_body(&etags);
    let hash = sha256_hex(body.as_bytes());
    with_retry(tuning, "complete multipart upload", |attempt| {
        let call = Call::new("POST", vec![("uploadId", upload_id)], &hash)
            .content_type("application/xml")
            .body(body.clone().into(), body.len() as u64);
        async move {
            match send(client, target, call).await {
                // An earlier attempt may have completed the upload and lost
                // the response: the upload is gone, but the object is there.
                Err(a) if attempt > 1 && a.code.as_deref() == Some("NoSuchUpload") => {
                    match head_size(client, target).await {
                        Some(len) if len == size => Ok(()),
                        _ => Err(a),
                    }
                }
                other => other.map(|_| ()),
            }
        }
    })
    .await
}

/// The stored object's size, if it exists.
async fn head_size(client: &reqwest::Client, target: &S3Target) -> Option<u64> {
    let empty = sha256_hex(b"");
    send(client, target, Call::new("HEAD", Vec::new(), &empty)).await.ok()?.content_length
}

/// `CompleteMultipartUpload` document listing parts 1… in order.
pub fn complete_body(etags: &[String]) -> String {
    let mut s = String::from("<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    for (i, etag) in etags.iter().enumerate() {
        s.push_str(&format!("<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>", i + 1, xml_escape(etag)));
    }
    s.push_str("</CompleteMultipartUpload>");
    s
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Text of the first `<tag>…</tag>`, as written (still XML-escaped).
fn xml_value(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    let v = xml[start..end].trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// SHA-256 (hex) of `len` bytes at `offset`, read in bounded chunks.
fn hash_range(path: &Path, offset: u64, len: u64) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    f.seek(SeekFrom::Start(offset))?;
    let mut ctx = digest::Context::new(&digest::SHA256);
    let mut buf = vec![0u8; CHUNK];
    let mut left = len;
    while left > 0 {
        let want = (left as usize).min(CHUNK);
        let got = f.read(&mut buf[..want])?;
        if got == 0 {
            bail!("{} shrank while it was being uploaded", path.display());
        }
        ctx.update(&buf[..got]);
        left -= got as u64;
    }
    Ok(hex(ctx.finish().as_ref()))
}

/// A request body that streams `len` bytes at `offset` from disk.
fn range_body(path: &Path, offset: u64, len: u64) -> reqwest::Body {
    reqwest::Body::wrap_stream(range_stream(path, offset, len))
}

/// `len` bytes at `offset` as a stream of reads of at most `CHUNK` bytes;
/// the file is opened when the stream is first polled. A file that ends
/// early yields an error instead of a short body.
fn range_stream(path: &Path, offset: u64, len: u64) -> impl futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static {
    struct State {
        path: PathBuf,
        file: Option<std::fs::File>,
        offset: u64,
        left: u64,
    }
    let state = State { path: path.to_path_buf(), file: None, offset, left: len };
    futures::stream::unfold(state, |mut st| async move {
        if st.left == 0 {
            return None;
        }
        let chunk = (|| -> std::io::Result<Vec<u8>> {
            if st.file.is_none() {
                let mut f = std::fs::File::open(&st.path)?;
                f.seek(SeekFrom::Start(st.offset))?;
                st.file = Some(f);
            }
            let f = st.file.as_mut().expect("opened above");
            let mut buf = vec![0u8; (st.left as usize).min(CHUNK)];
            let got = f.read(&mut buf)?;
            if got == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "file shrank while it was being uploaded"));
            }
            buf.truncate(got);
            Ok(buf)
        })();
        match chunk {
            Ok(buf) => {
                st.left -= buf.len() as u64;
                Some((Ok(buf), st))
            }
            Err(e) => {
                st.left = 0;
                Some((Err(e), st))
            }
        }
    })
}

fn env_var(name: &str) -> Result<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => bail!("environment variable {name} is not set (named by [sync.s3])"),
    }
}

/// `"bugle/"` for `""`, `"bugle"`, `"/bugle/"`; `""` stays `""`.
fn normalise_prefix(prefix: &str) -> String {
    let p = prefix.trim().trim_matches('/');
    if p.is_empty() {
        String::new()
    } else {
        format!("{p}/")
    }
}

fn host_of(endpoint: &str) -> Result<String> {
    let rest = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .ok_or_else(|| anyhow!("[sync.s3] endpoint must start with http:// or https://: {endpoint}"))?;
    let host = rest.split('/').next().unwrap_or("").trim();
    if host.is_empty() {
        bail!("[sync.s3] endpoint has no host: {endpoint}");
    }
    Ok(host.to_string())
}

fn content_type_for(name: &str) -> &'static str {
    match Path::new(name).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("tif") | Some("tiff") => "image/tiff",
        Some("gif") => "image/gif",
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("m4a") => "audio/mp4",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("vtt") => "text/vtt",
        Some("srt") | Some("txt") | Some("md") => "text/plain",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

/// Run a future to completion on a throw-away current-thread runtime. The
/// engine's blocking methods are called both from inside a tokio runtime
/// (CLI, servers via `spawn_blocking`) and from plain threads (menu-bar
/// app), so neither `Handle::block_on` nor a global runtime fits.
fn block_on<T: Send + 'static>(fut: impl std::future::Future<Output = Result<T>> + Send + 'static) -> Result<T> {
    std::thread::Builder::new()
        .name("bugle-sync".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            rt.block_on(fut)
        })
        .context("spawning sync thread")?
        .join()
        .map_err(|_| anyhow!("sync thread panicked"))?
}

// ---------------------------------------------------------------------------
// AWS Signature Version 4
// ---------------------------------------------------------------------------

/// Inputs to the signer. `headers` are `(lowercase name, trimmed value)` and
/// must include `host`; `canonical_uri` is already URI-encoded.
pub struct SigV4Request<'a> {
    pub method: &'a str,
    pub canonical_uri: &'a str,
    pub canonical_query: &'a str,
    pub headers: &'a [(String, String)],
    pub payload_hash: &'a str,
    /// `YYYYMMDD'T'HHMMSS'Z'`.
    pub amz_date: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
}

/// The value of the `Authorization` header.
pub fn sigv4_authorization(req: &SigV4Request) -> String {
    let mut headers: Vec<(String, String)> =
        req.headers.iter().map(|(k, v)| (k.trim().to_ascii_lowercase(), collapse_spaces(v.trim()))).collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method, req.canonical_uri, req.canonical_query, canonical_headers, signed_headers, req.payload_hash
    );
    let date = &req.amz_date[..8];
    let scope = format!("{date}/{}/{}/aws4_request", req.region, req.service);
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{}\n{scope}\n{}", req.amz_date, sha256_hex(canonical_request.as_bytes()));
    let k_date = hmac_sha256(format!("AWS4{}", req.secret_key).as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, req.region.as_bytes());
    let k_service = hmac_sha256(&k_region, req.service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed_headers},Signature={signature}", req.access_key)
}

fn collapse_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// RFC 3986 unreserved characters stay; everything else is `%XX`. `/` is
/// kept unless `encode_slash` (S3 keys keep their slashes in the URI).
pub fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&key, data).as_ref().to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, data).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::{History, MediaKind};

    const ACCESS: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// AWS "Signature Calculations for the Authorization Header", example 1
    /// (GET object with a Range header).
    #[test]
    fn sigv4_matches_aws_get_example() {
        let headers = h(&[
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("Range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let auth = sigv4_authorization(&SigV4Request {
            method: "GET",
            canonical_uri: "/test.txt",
            canonical_query: "",
            headers: &headers,
            payload_hash: EMPTY_SHA256,
            amz_date: "20130524T000000Z",
            region: "us-east-1",
            service: "s3",
            access_key: ACCESS,
            secret_key: SECRET,
        });
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    /// Example 2 from the same page: PUT of "Welcome to Amazon S3." to
    /// `test$file.text` with a storage-class header.
    #[test]
    fn sigv4_matches_aws_put_example() {
        let payload_hash = sha256_hex(b"Welcome to Amazon S3.");
        assert_eq!(payload_hash, "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072");
        let headers = h(&[
            ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", &payload_hash),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
        ]);
        let canonical_uri = format!("/{}", uri_encode("test$file.text", false));
        assert_eq!(canonical_uri, "/test%24file.text");
        let auth = sigv4_authorization(&SigV4Request {
            method: "PUT",
            canonical_uri: &canonical_uri,
            canonical_query: "",
            headers: &headers,
            payload_hash: &payload_hash,
            amz_date: "20130524T000000Z",
            region: "us-east-1",
            service: "s3",
            access_key: ACCESS,
            secret_key: SECRET,
        });
        assert!(auth.ends_with("Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"), "{auth}");
        assert!(auth.contains("SignedHeaders=date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class,"), "{auth}");
    }

    #[test]
    fn uri_encoding_and_prefix() {
        assert_eq!(uri_encode("Bugle 2026-11-06 at 10.00.00.png", false), "Bugle%202026-11-06%20at%2010.00.00.png");
        assert_eq!(uri_encode("a/b c", false), "a/b%20c");
        assert_eq!(uri_encode("a/b c", true), "a%2Fb%20c");
        assert_eq!(normalise_prefix(""), "");
        assert_eq!(normalise_prefix("/bugle/"), "bugle/");
        assert_eq!(normalise_prefix("team/bugle"), "team/bugle/");
        assert_eq!(host_of("https://s3.eu-west-1.amazonaws.com/").unwrap(), "s3.eu-west-1.amazonaws.com");
        assert_eq!(host_of("http://127.0.0.1:9000").unwrap(), "127.0.0.1:9000");
        assert!(host_of("s3.amazonaws.com").is_err());
    }

    fn engine_in(tmp: &Path, mode: SyncMode) -> Engine {
        let mut cfg = Config::default();
        cfg.general.save_dir = tmp.join("save").display().to_string();
        cfg.general.history_path = Some(tmp.join("history.jsonl").display().to_string());
        cfg.sync.enabled = true;
        cfg.sync.mode = mode;
        cfg.sync.folder = Some(tmp.join("cloud").display().to_string());
        Engine::new(cfg, false).unwrap()
    }

    /// `bugle history prune` in another process can tombstone an item while
    /// its file is between the old path and the recorded new one.
    #[test]
    fn sync_records_an_item_pruned_during_the_move_again() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_in(tmp.path(), SyncMode::Move);
        let src = engine.config.save_dir().join("shot.png");
        std::fs::write(&src, b"not really a png").unwrap();
        let mut item = MediaItem::new(MediaKind::Screenshot, &src);
        item.created_at = chrono::Utc::now() - chrono::Duration::hours(1);
        engine.history.push(&item).unwrap();
        std::fs::write(engine.config.save_dir().join("other.png"), b"x").unwrap();

        let mut moving = item.clone();
        engine.sync_item(&mut moving).unwrap().unwrap();
        assert!(!src.exists());
        // The window between the move and record_sync.
        assert_eq!(engine.history.prune_missing(false).unwrap().len(), 1);
        assert!(engine.history.get(&item.id).unwrap().is_none());
        engine.record_sync(&moving, Some(&src));
        let stored = engine.history.get(&item.id).unwrap().unwrap();
        assert_eq!(stored.path, moving.path);
        assert!(stored.path.is_file());

        // An item that was never in history is not added by a sync.
        let loose = MediaItem::new(MediaKind::Screenshot, tmp.path().join("loose.png"));
        engine.record_sync(&loose, None);
        assert!(engine.history.get(&loose.id).unwrap().is_none());

        // A copy (or upload) keeps the old path, so prune cannot have removed
        // the entry: one deleted during the sync stays deleted.
        let mut copied = item.clone();
        copied.id = "copy00000001".into();
        copied.synced_as = Some("shot.png".into());
        engine.record_sync(&copied, Some(&copied.path.clone()));
        assert!(engine.history.get(&copied.id).unwrap().is_none());
    }

    /// A `bugle delete` (or `DELETE /v1/media/{id}`) that lands while a sync
    /// is moving the file wins: the entry stays deleted, and the moved file
    /// stays or goes as the delete asked.
    #[test]
    fn a_delete_during_the_move_stays_deleted() {
        for remove_file in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let engine = engine_in(tmp.path(), SyncMode::Move);
            let src = engine.config.save_dir().join("clip.mp4");
            std::fs::write(&src, b"not really a video").unwrap();
            let events = engine.config.save_dir().join("clip.mp4.events.json");
            std::fs::write(&events, b"{}").unwrap();
            let mut item = MediaItem::new(MediaKind::Video, &src);
            item.events = Some(events.clone());
            engine.history.push(&item).unwrap();

            let mut moving = item.clone();
            engine.sync_item(&mut moving).unwrap().unwrap();
            assert!(!src.exists() && moving.path.is_file());
            let moved_events = moving.events.clone().unwrap();
            assert_ne!(moved_events, events);
            // The window between the move and record_sync, from another process.
            let other = History::open(engine.history.path()).unwrap();
            assert!(other.delete(&item.id, remove_file).unwrap());
            engine.record_sync(&moving, Some(&src));

            assert!(engine.history.get(&item.id).unwrap().is_none(), "remove_file={remove_file}");
            assert!(History::open(engine.history.path()).unwrap().all().unwrap().is_empty());
            assert_eq!(moving.path.is_file(), !remove_file, "the moved file follows the delete");
            assert_eq!(moved_events.is_file(), !remove_file, "so does its event track");
        }
    }

    /// The delete reads the entry after the sync recorded the new path: it
    /// removes the moved file, not the old path.
    #[test]
    fn a_delete_after_the_record_removes_the_moved_file() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_in(tmp.path(), SyncMode::Move);
        let src = engine.config.save_dir().join("shot.png");
        std::fs::write(&src, b"not really a png").unwrap();
        let item = MediaItem::new(MediaKind::Screenshot, &src);
        engine.history.push(&item).unwrap();
        let mut moving = item.clone();
        engine.sync_item(&mut moving).unwrap().unwrap();
        engine.record_sync(&moving, Some(&src));
        assert!(History::open(engine.history.path()).unwrap().delete(&item.id[..6], true).unwrap());
        assert!(!moving.path.exists());
        assert!(engine.history.get(&item.id).unwrap().is_none());
    }

    #[test]
    fn folder_copy_then_move_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_in(tmp.path(), SyncMode::Copy);
        let src = engine.config.save_dir().join("shot.png");
        std::fs::write(&src, b"not really a png").unwrap();
        let item = MediaItem::new(MediaKind::Screenshot, &src);
        engine.history.push(&item).unwrap();

        // Copy: original stays, a copy lands in the folder, history is unchanged.
        let (after, report) = engine.sync_media(&item.id).unwrap();
        assert_eq!(report.action, "copied");
        assert_eq!(after.path, src);
        assert!(src.exists());
        let cloud = tmp.path().join("cloud");
        assert_eq!(std::fs::read(cloud.join("shot.png")).unwrap(), b"not really a png");
        assert_eq!(engine.history.get(&item.id).unwrap().unwrap().path, src);

        // A second copy does not overwrite the first.
        engine.sync_media(&item.id).unwrap();
        assert!(cloud.join("shot 2.png").exists());

        // Move: the file leaves save_dir and history follows it.
        let engine = engine_in(tmp.path(), SyncMode::Move);
        let (after, report) = engine.sync_media(&item.id).unwrap();
        assert_eq!(report.action, "moved");
        assert!(!src.exists());
        assert_eq!(after.path, cloud.join("shot 3.png"));
        assert!(after.path.exists());
        assert_eq!(engine.history.get(&item.id).unwrap().unwrap().path, after.path);
        assert_eq!(engine.history.all().unwrap().len(), 1);

        // Already inside the folder: nothing to do.
        let (_, report) = engine.sync_media(&item.id).unwrap();
        assert_eq!(report.action, "already there");
        assert!(cloud.join("shot 3.png").exists() && !cloud.join("shot 4.png").exists());
    }

    #[test]
    fn automatic_step_respects_enabled_and_kinds() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_in(tmp.path(), SyncMode::Copy);
        let src = engine.config.save_dir().join("clip.srt");
        std::fs::write(&src, b"1\n").unwrap();
        let mut item = MediaItem::new(MediaKind::Subtitle, &src);
        engine.sync_after_save(&mut item);
        assert!(!tmp.path().join("cloud").join("clip.srt").exists(), "subtitles are not in the default kinds");

        let shot = engine.config.save_dir().join("a.png");
        std::fs::write(&shot, b"x").unwrap();
        let mut item = MediaItem::new(MediaKind::Screenshot, &shot);
        engine.sync_after_save(&mut item);
        assert!(tmp.path().join("cloud").join("a.png").exists());

        engine.config.sync.enabled = false;
        let shot2 = engine.config.save_dir().join("b.png");
        std::fs::write(&shot2, b"x").unwrap();
        let mut item = MediaItem::new(MediaKind::Screenshot, &shot2);
        engine.sync_after_save(&mut item);
        assert!(!tmp.path().join("cloud").join("b.png").exists(), "disabled sync does nothing");
        // The explicit command ignores `enabled`.
        let (_, r) = engine.sync_media(shot2.to_str().unwrap()).unwrap();
        assert_eq!(r.action, "copied");
    }

    // -----------------------------------------------------------------------
    // Per-kind destinations and sidecar subtitles (folder destination)
    // -----------------------------------------------------------------------

    fn write(path: &Path, bytes: &[u8]) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        path.to_path_buf()
    }

    fn target(folder: Option<&Path>, prefix: Option<&str>, mode: Option<SyncMode>) -> bugle_core::config::SyncTarget {
        bugle_core::config::SyncTarget {
            folder: folder.map(|f| f.display().to_string()),
            prefix: prefix.map(str::to_string),
            mode,
            ..Default::default()
        }
    }

    #[test]
    fn each_kind_goes_to_its_own_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_in(tmp.path(), SyncMode::Copy);
        let videos = tmp.path().join("videos");
        engine.config.sync.destinations.insert(MediaKind::Video, target(Some(&videos), None, None));
        engine.config.sync.destinations.insert(MediaKind::Gif, target(None, Some("gifs"), Some(SyncMode::Move)));
        let save = engine.config.save_dir();
        let cloud = tmp.path().join("cloud");

        let mut shot = MediaItem::new(MediaKind::Screenshot, write(&save.join("a.png"), b"png"));
        engine.sync_after_save(&mut shot);
        assert!(cloud.join("a.png").exists());
        assert_eq!(shot.synced_as.as_deref(), Some("a.png"));

        let mut video = MediaItem::new(MediaKind::Video, write(&save.join("v.mp4"), b"mp4"));
        engine.sync_after_save(&mut video);
        assert!(videos.join("v.mp4").exists() && !cloud.join("v.mp4").exists());
        assert!(save.join("v.mp4").exists(), "copy is the default mode");

        // Saves are in history before the sync runs; the sync then updates it.
        let mut gif = MediaItem::new(MediaKind::Gif, write(&save.join("g.gif"), b"gif"));
        engine.history.push(&gif).unwrap();
        engine.sync_after_save(&mut gif);
        assert_eq!(gif.path, cloud.join("gifs").join("g.gif"), "the gif entry moves into its sub-folder");
        assert!(!save.join("g.gif").exists());
        let stored = engine.history.get(&gif.id).unwrap().unwrap();
        assert_eq!(stored.path, gif.path);
        assert_eq!(stored.synced_as.as_deref(), Some("g.gif"));

        assert_eq!(engine.sync_destination_label_for(MediaKind::Video), Some(folder_label(&videos)));
        assert_eq!(engine.sync_destination_label_for(MediaKind::Screenshot), engine.sync_destination_label());
    }

    #[test]
    fn sidecars_follow_their_video_under_its_synced_name() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_in(tmp.path(), SyncMode::Copy);
        let save = engine.config.save_dir();
        let cloud = tmp.path().join("cloud");
        // The folder already has a clip.mp4 and a clip.srt from something else.
        write(&cloud.join("clip.mp4"), b"other video");
        write(&cloud.join("clip.srt"), b"other subtitles");

        let video_path = write(&save.join("clip.mp4"), b"video");
        write(&save.join("clip.srt"), b"1\nsrt");
        let video = MediaItem::new(MediaKind::Video, &video_path);
        engine.history.push(&video).unwrap();

        let (video, report) = engine.sync_media(&video.id).unwrap();
        assert_eq!(report.action, "copied");
        assert_eq!(video.synced_as.as_deref(), Some("clip 2.mp4"));
        assert_eq!(engine.history.get(&video.id).unwrap().unwrap().synced_as.as_deref(), Some("clip 2.mp4"));
        assert_eq!(report.sidecars.len(), 1);
        assert_eq!(std::fs::read(cloud.join("clip 2.srt")).unwrap(), b"1\nsrt");
        assert_eq!(std::fs::read(cloud.join("clip.srt")).unwrap(), b"other subtitles", "unrelated files are untouched");

        // A subtitle written later (what `subtitles` does) follows the video.
        let vtt = write(&save.join("clip.vtt"), b"WEBVTT");
        let sub = engine.save_subtitle_item(&video, &vtt, "test".into(), false).unwrap();
        assert_eq!(std::fs::read(cloud.join("clip 2.vtt")).unwrap(), b"WEBVTT");
        assert_eq!(sub.synced_as.as_deref(), Some("clip 2.vtt"));
        assert_eq!(engine.history.get(&sub.id).unwrap().unwrap().synced_as.as_deref(), Some("clip 2.vtt"));

        // Regenerated subtitles replace the sidecar Bugle wrote, and a
        // sidecar given by path finds its video through history.
        write(&vtt, b"WEBVTT\n\nnew");
        let (_, r) = engine.sync_media(vtt.to_str().unwrap()).unwrap();
        assert_eq!(r.action, "copied");
        assert_eq!(std::fs::read(cloud.join("clip 2.vtt")).unwrap(), b"WEBVTT\n\nnew");
        assert!(!cloud.join("clip 2 2.vtt").exists() && !cloud.join("clip.vtt").exists());
    }

    #[test]
    fn sidecars_never_replace_files_bugle_did_not_write() {
        for mode in [SyncMode::Copy, SyncMode::Move] {
            let tmp = tempfile::tempdir().unwrap();
            let engine = engine_in(tmp.path(), mode);
            let save = engine.config.save_dir();
            let cloud = tmp.path().join("cloud");
            // Someone else's subtitles, with no video next to them, and an
            // identical copy of ours.
            write(&cloud.join("talk.srt"), b"theirs");
            write(&cloud.join("talk.vtt"), b"WEBVTT same");
            let video = MediaItem::new(MediaKind::Video, write(&save.join("talk.mp4"), b"v"));
            engine.history.push(&video).unwrap();
            write(&save.join("talk.srt"), b"ours");
            write(&save.join("talk.vtt"), b"WEBVTT same");

            let (video, report) = engine.sync_media(&video.id).unwrap();
            assert_eq!(video.synced_as.as_deref(), Some("talk.mp4"));
            assert_eq!(report.sidecars.len(), 2, "{mode:?}");
            assert_eq!(std::fs::read(cloud.join("talk.srt")).unwrap(), b"theirs", "{mode:?}");
            assert_eq!(std::fs::read(cloud.join("talk 2.srt")).unwrap(), b"ours", "{mode:?}");
            assert_eq!(std::fs::read(cloud.join("talk.vtt")).unwrap(), b"WEBVTT same", "identical files may be replaced");
            assert!(!cloud.join("talk 2.vtt").exists(), "{mode:?}");

            // Next time the sidecar Bugle wrote is reused, not a third name.
            if mode == SyncMode::Copy {
                write(&save.join("talk.srt"), b"ours, regenerated");
                engine.sync_media(save.join("talk.srt").to_str().unwrap()).unwrap();
                assert_eq!(std::fs::read(cloud.join("talk 2.srt")).unwrap(), b"ours, regenerated");
                assert_eq!(std::fs::read(cloud.join("talk.srt")).unwrap(), b"theirs");
                assert!(!cloud.join("talk 3.srt").exists());
            }
        }
    }

    #[test]
    fn a_video_synced_before_synced_as_existed_does_not_guess_its_name() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_in(tmp.path(), SyncMode::Copy);
        let save = engine.config.save_dir();
        let cloud = tmp.path().join("cloud");
        // Earlier: clip.mp4 belonged to someone else, ours was copied as clip 2.mp4,
        // and history (from an older version) does not say so.
        write(&cloud.join("clip.mp4"), b"other video");
        write(&cloud.join("clip.srt"), b"other subtitles");
        write(&cloud.join("clip 2.mp4"), b"video");
        let video = MediaItem::new(MediaKind::Video, write(&save.join("clip.mp4"), b"video"));
        engine.history.push(&video).unwrap();
        assert!(video.synced_as.is_none());

        let srt = write(&save.join("clip.srt"), b"ours");
        let sub = engine.save_subtitle_item(&video, &srt, "test".into(), false).unwrap();
        assert_eq!(std::fs::read(cloud.join("clip.srt")).unwrap(), b"other subtitles");
        assert_eq!(sub.synced_as.as_deref(), Some("clip 2.srt"));
        assert_eq!(std::fs::read(cloud.join("clip 2.srt")).unwrap(), b"ours");
    }

    #[test]
    fn subtitles_report_where_the_file_is_and_keep_explicit_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_in(tmp.path(), SyncMode::Move);
        let save = engine.config.save_dir();
        let cloud = tmp.path().join("cloud");
        let video = MediaItem::new(MediaKind::Video, write(&save.join("v.mp4"), b"v"));
        engine.history.push(&video).unwrap();

        // Written next to the video in move mode: moved, and the returned
        // path is the new one.
        let srt = write(&save.join("v.srt"), b"srt");
        let m = engine.save_subtitle_item(&video, &srt, "test".into(), false).unwrap();
        assert_eq!(m.path, cloud.join("v.srt"));
        assert!(m.path.exists() && !srt.exists());
        assert_eq!(engine.history.get(&m.id).unwrap().unwrap().path, m.path);

        // An explicit output is copied, never emptied.
        let out = write(&tmp.path().join("explicit").join("mine.vtt"), b"WEBVTT");
        let m = engine.save_subtitle_item(&video, &out, "test".into(), true).unwrap();
        assert_eq!(m.path, out);
        assert!(out.exists());
        // The video was never synced, so its synced name is unknown: the local name is kept.
        assert_eq!(std::fs::read(cloud.join("mine.vtt")).unwrap(), b"WEBVTT");
        assert_eq!(engine.history.get(&m.id).unwrap().unwrap().path, out);
    }

    #[test]
    fn moving_a_video_moves_its_sidecars_and_their_history() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_in(tmp.path(), SyncMode::Move);
        let save = engine.config.save_dir();
        let cloud = tmp.path().join("cloud");
        let video = MediaItem::new(MediaKind::Video, write(&save.join("rec.mov"), b"mov"));
        engine.history.push(&video).unwrap();
        let mut sub = MediaItem::new(MediaKind::Subtitle, write(&save.join("rec.srt"), b"srt"));
        sub.parent = Some(video.id.clone());
        engine.history.push(&sub).unwrap();

        let (video, report) = engine.sync_media(&video.id).unwrap();
        assert_eq!(video.path, cloud.join("rec.mov"));
        assert_eq!(report.sidecars[0].action, "moved");
        assert!(cloud.join("rec.srt").exists() && !save.join("rec.srt").exists());
        let stored = engine.history.get(&sub.id).unwrap().unwrap();
        assert_eq!(stored.path, cloud.join("rec.srt"));
        assert_eq!(stored.synced_as.as_deref(), Some("rec.srt"));

        // A sidecar written next to the moved video is already in place:
        // the sync runs (and records its name) without moving anything.
        let vtt = write(&cloud.join("rec.vtt"), b"vtt");
        let item = engine.save_subtitle_item(&video, &vtt, "test".into(), false).unwrap();
        assert_eq!(item.path, vtt);
        assert_eq!(item.synced_as.as_deref(), Some("rec.vtt"), "the sync step ran");
        assert_eq!(std::fs::read_dir(&cloud).unwrap().count(), 3, "nothing was duplicated");

        // With `sidecars = false` a subtitle is synced like any other file.
        engine.config.sync.sidecars = false;
        engine.config.sync.mode = SyncMode::Copy;
        let lone = write(&save.join("rec.vtt"), b"lone");
        let (_, r) = engine.sync_media(lone.to_str().unwrap()).unwrap();
        assert_eq!(r.destination, folder_label(&cloud));
        assert_eq!(std::fs::read(cloud.join("rec 2.vtt")).unwrap(), b"lone");
    }

    #[test]
    fn event_tracks_follow_their_video_and_go_with_it_on_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_in(tmp.path(), SyncMode::Move);
        let save = engine.config.save_dir();
        let cloud = tmp.path().join("cloud");
        write(&cloud.join("rec.mp4"), b"someone else's");
        let mut video = MediaItem::new(MediaKind::Video, write(&save.join("rec.mp4"), b"mp4"));
        video.events = Some(write(&save.join("rec.mp4.events.json"), b"{}"));
        engine.history.push(&video).unwrap();
        let (video, report) = engine.sync_media(&video.id).unwrap();
        assert_eq!(video.path, cloud.join("rec 2.mp4"));
        let moved = cloud.join("rec 2.mp4.events.json");
        assert_eq!(video.events.as_deref(), Some(moved.as_path()));
        assert!(moved.exists() && !save.join("rec.mp4.events.json").exists());
        assert_eq!(report.sidecars.len(), 1);
        assert_eq!(engine.history.get(&video.id).unwrap().unwrap().events.as_deref(), Some(moved.as_path()));

        assert!(engine.history.delete(&video.id, true).unwrap());
        assert!(!moved.exists() && !video.path.exists());
        assert_eq!(std::fs::read(cloud.join("rec.mp4")).unwrap(), b"someone else's");
    }

    #[test]
    fn new_sidecars_only_sync_when_their_video_does() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_in(tmp.path(), SyncMode::Copy);
        engine.config.sync.enabled = false;
        let save = engine.config.save_dir();
        let cloud = tmp.path().join("cloud");
        let video = MediaItem::new(MediaKind::Video, write(&save.join("x.mp4"), b"v"));
        let srt = write(&save.join("x.srt"), b"s");
        let m = engine.save_subtitle_item(&video, &srt, "test".into(), false).unwrap();
        assert!(m.synced_as.is_none() && !cloud.join("x.srt").exists(), "the video was never synced");
        let mut synced = video.clone();
        synced.synced_as = Some("x 7.mp4".into());
        let m = engine.save_subtitle_item(&synced, &srt, "test".into(), false).unwrap();
        assert!(cloud.join("x 7.srt").exists(), "a manually synced video takes its subtitles along");
        assert_eq!(m.synced_as.as_deref(), Some("x 7.srt"));
    }

    #[test]
    fn a_subtitle_entry_wins_over_the_videos_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_in(tmp.path(), SyncMode::Copy);
        let subs = tmp.path().join("subs");
        engine.config.sync.destinations.insert(MediaKind::Subtitle, target(Some(&subs), None, None));
        let save = engine.config.save_dir();
        let video = MediaItem::new(MediaKind::Video, write(&save.join("d.mp4"), b"v"));
        engine.history.push(&video).unwrap();
        write(&save.join("d.srt"), b"s");
        let (_, report) = engine.sync_media(&video.id).unwrap();
        assert!(tmp.path().join("cloud").join("d.mp4").exists());
        assert_eq!(report.sidecars[0].destination, folder_label(&subs));
        assert!(subs.join("d.srt").exists() && !tmp.path().join("cloud").join("d.srt").exists());

        // Only a subtitle entry, no default: `bugle sync` on the subtitle works.
        engine.config.sync.folder = None;
        write(&save.join("d.vtt"), b"WEBVTT");
        let (_, r) = engine.sync_media(save.join("d.vtt").to_str().unwrap()).unwrap();
        assert_eq!(r.destination, folder_label(&subs));
        assert!(subs.join("d.vtt").exists());
    }

    // -----------------------------------------------------------------------
    // S3: signing vectors, query canonicalisation, tuning, and a mock server
    // -----------------------------------------------------------------------

    /// AWS "Signature Calculations for the Authorization Header", the two
    /// query-string examples (GET Bucket Lifecycle, GET Bucket / List
    /// Objects): checks the canonical query independently of the mock.
    #[test]
    fn sigv4_matches_aws_query_string_examples() {
        let headers = h(&[
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let sign = |query: &[(&str, &str)]| {
            sigv4_authorization(&SigV4Request {
                method: "GET",
                canonical_uri: "/",
                canonical_query: &canonical_query(query),
                headers: &headers,
                payload_hash: EMPTY_SHA256,
                amz_date: "20130524T000000Z",
                region: "us-east-1",
                service: "s3",
                access_key: ACCESS,
                secret_key: SECRET,
            })
        };
        assert!(
            sign(&[("lifecycle", "")]).ends_with("Signature=fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"),
            "{}",
            sign(&[("lifecycle", "")])
        );
        // Given out of order: the canonical query sorts it.
        let list = sign(&[("prefix", "J"), ("max-keys", "2")]);
        assert!(list.ends_with("Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"), "{list}");
    }

    #[test]
    fn canonical_query_and_complete_body() {
        assert_eq!(canonical_query(&[("uploads", "")]), "uploads=");
        assert_eq!(canonical_query(&[("uploadId", "a/b+c=="), ("partNumber", "2")]), "partNumber=2&uploadId=a%2Fb%2Bc%3D%3D");
        assert_eq!(
            complete_body(&["\"e1\"".into(), "\"e&2\"".into()]),
            "<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Part><PartNumber>1</PartNumber><ETag>&quot;e1&quot;</ETag></Part>\
             <Part><PartNumber>2</PartNumber><ETag>&quot;e&amp;2&quot;</ETag></Part></CompleteMultipartUpload>"
        );
        assert_eq!(xml_value("<a><UploadId> u-1 </UploadId></a>", "UploadId").as_deref(), Some("u-1"));
        assert_eq!(xml_value("<UploadId></UploadId>", "UploadId"), None);
        assert_eq!(xml_unescape("a&amp;b&lt;&quot;&amp;amp;"), "a&b<\"&amp;");
    }

    #[test]
    fn error_bodies_are_reduced_to_code_and_message() {
        let body = "<Error><Code>SignatureDoesNotMatch</Code><Message>The request signature we calculated does not match</Message>\
                    <AWSAccessKeyId>AKIDLEAK</AWSAccessKeyId><StringToSign>AWS4-HMAC-SHA256 leak</StringToSign></Error>";
        let s = error_summary(body);
        assert_eq!(s, ": SignatureDoesNotMatch: The request signature we calculated does not match");
        assert_eq!(error_summary("<html>proxy page with AKIDLEAK</html>"), "");
    }

    #[test]
    fn tuning_respects_s3_limits() {
        let mut cfg = S3Config { multipart_threshold_mb: 0, part_size_mb: 1, ..Default::default() };
        let t = UploadTuning::from_config(&cfg);
        assert_eq!(t.part_size, 5 * MIB, "parts are at least 5 MiB");
        assert_eq!(t.multipart_threshold, 5 * MIB);
        cfg.part_size_mb = 16;
        let t = UploadTuning::from_config(&cfg);
        assert_eq!(t.part_size_for(100 * MIB), 16 * MIB);
        let huge = 1024 * 1024 * MIB; // 1 TiB
        assert!(huge.div_ceil(t.part_size_for(huge)) <= MAX_PARTS, "never more than 10 000 parts");
        // Absurd values neither overflow nor exceed the S3 maximums.
        let t = UploadTuning::from_config(&S3Config { multipart_threshold_mb: u64::MAX, part_size_mb: u64::MAX, ..Default::default() });
        assert_eq!(t.part_size, 5120 * MIB);
        assert_eq!(t.multipart_threshold, 5120 * MIB);
        // The per-request deadline grows with the body.
        assert_eq!(request_timeout(0), Duration::from_secs(60));
        assert_eq!(request_timeout(16 * MIB), Duration::from_secs(60 + 128));
        assert!(request_timeout(5120 * MIB) > Duration::from_secs(10 * 3600));
        assert_eq!(request_timeout(u64::MAX).as_secs(), 60 + u64::MAX / (128 * 1024));
    }

    #[test]
    fn backoff_is_capped_and_jittered() {
        let t = UploadTuning::from_config(&S3Config::default());
        assert_eq!(t.attempts, 6);
        for n in 1..=20 {
            let full = Duration::from_secs(1u64 << (n - 1).min(5)).min(Duration::from_secs(30));
            let w = backoff(&t, n);
            assert!(w >= full / 2 && w <= full, "attempt {n}: {w:?} vs {full:?}");
        }
        assert_eq!(parse_retry_after(" 7 "), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }

    #[test]
    fn hash_range_reads_the_right_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let data = pseudo_random(3 * CHUNK + 17);
        let path = write(&tmp.path().join("f.bin"), &data);
        let (off, len) = (CHUNK as u64 - 5, 2 * CHUNK as u64 + 9);
        let slice = &data[off as usize..(off + len) as usize];
        assert_eq!(hash_range(&path, off, len).unwrap(), sha256_hex(slice));
        assert!(hash_range(&path, off, data.len() as u64).is_err(), "reading past the end fails");
    }

    #[tokio::test]
    async fn range_stream_reads_bounded_chunks() {
        use futures::StreamExt;
        let tmp = tempfile::tempdir().unwrap();
        let data = pseudo_random(5 * CHUNK + 3);
        let path = write(&tmp.path().join("f.bin"), &data);
        let (off, len) = (7u64, 4 * CHUNK as u64 + 100);
        let chunks: Vec<Vec<u8>> = range_stream(&path, off, len).map(|c| c.unwrap()).collect().await;
        assert!(chunks.iter().all(|c| !c.is_empty() && c.len() <= CHUNK), "at most {CHUNK} bytes are held per read");
        assert_eq!(chunks.concat(), &data[off as usize..(off + len) as usize]);
        // A file that ends early is an error, not a short body.
        let tail: Vec<std::io::Result<Vec<u8>>> = range_stream(&path, data.len() as u64 - 10, 20).collect().await;
        assert!(tail.last().unwrap().is_err());
    }

    fn pseudo_random(n: usize) -> Vec<u8> {
        let mut x: u32 = 0x9e37_79b9;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 24) as u8
            })
            .collect()
    }

    mod mock {
        //! An in-process S3 stand-in that checks each request the way a real
        //! server would: SigV4 signature recomputed from what arrived (with
        //! the shared signer; the published AWS vectors check the signer
        //! itself), payload hash, content length, part order, minimum part
        //! size and the complete body.
        use super::super::*;
        use axum::body::Bytes;
        use axum::extract::State;
        use axum::http::{HeaderMap, Method, StatusCode, Uri};
        use axum::response::{IntoResponse, Response};
        use std::collections::{BTreeMap, HashMap};
        use std::sync::{Arc, Mutex};

        pub const ACCESS: &str = "AKIDMOCK";
        pub const SECRET: &str = "mock/secret/key";

        pub struct Upload {
            key: String,
            content_type: String,
            parts: BTreeMap<u32, (String, Vec<u8>)>,
        }

        pub type Hook = Box<dyn FnOnce() + Send>;

        pub struct S3State {
            /// `create`, `part 1`, `part 2 (500)`, `complete`, `abort`, `put`, `head` …
            pub log: Vec<String>,
            /// Object key → (content type, bytes).
            pub objects: HashMap<String, (String, Vec<u8>)>,
            pub uploads: HashMap<String, Upload>,
            pub aborted: Vec<String>,
            /// ETags handed out per part number, in order.
            pub etags: Vec<String>,
            pub complete_bodies: Vec<String>,
            /// Protocol violations the server noticed; tests assert it stays empty.
            pub violations: Vec<String>,
            /// Part number → (how many more failures, status, error code).
            pub fail_part: HashMap<u32, (u32, u16, &'static str)>,
            /// How many more times `complete` answers 200 with an `<Error>`.
            pub complete_errors: u32,
            /// How many more times `complete` succeeds but answers 500 (a
            /// lost response).
            pub complete_then_500: u32,
            /// Smallest accepted non-final part (S3: 5 MiB).
            pub min_part: usize,
            /// Run once when this part number arrives.
            pub on_part: Option<(u32, Hook)>,
            pub largest_part: usize,
            next_id: u32,
        }

        pub type Shared = Arc<Mutex<S3State>>;

        pub fn shared() -> Shared {
            Arc::new(Mutex::new(S3State {
                log: Vec::new(),
                objects: HashMap::new(),
                uploads: HashMap::new(),
                aborted: Vec::new(),
                etags: Vec::new(),
                complete_bodies: Vec::new(),
                violations: Vec::new(),
                fail_part: HashMap::new(),
                complete_errors: 0,
                complete_then_500: 0,
                min_part: 5 * 1024 * 1024,
                on_part: None,
                largest_part: 0,
                next_id: 0,
            }))
        }

        pub async fn start(state: Shared) -> String {
            let app = axum::Router::new()
                .fallback(handle)
                .layer(axum::extract::DefaultBodyLimit::disable())
                .with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            format!("http://{addr}")
        }

        fn decode(s: &str) -> String {
            let b = s.as_bytes();
            let mut out = Vec::new();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' && i + 2 < b.len() {
                    out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            String::from_utf8(out).unwrap()
        }

        /// Check the signature: rebuild the canonical request from the raw
        /// request line and headers.
        fn verify(method: &Method, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> Result<(), String> {
            let h = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
            let auth = h("authorization").ok_or("no authorization header")?;
            let signed_names = auth
                .split("SignedHeaders=")
                .nth(1)
                .and_then(|s| s.split(',').next())
                .ok_or("no SignedHeaders")?
                .to_string();
            let date = h("x-amz-date").ok_or("no x-amz-date")?;
            let payload_hash = h("x-amz-content-sha256").ok_or("no x-amz-content-sha256")?;
            if payload_hash != sha256_hex(body) {
                return Err(format!("x-amz-content-sha256 {payload_hash} does not match the body"));
            }
            if headers.contains_key("transfer-encoding") {
                return Err("chunked transfer encoding".into());
            }
            if method != Method::HEAD && h("content-length").map(|l| l.parse::<usize>().ok()) != Some(Some(body.len())) {
                return Err(format!("content-length {:?} for a {}-byte body", h("content-length"), body.len()));
            }
            for required in ["host", "x-amz-content-sha256", "x-amz-date", "content-length"] {
                if !signed_names.split(';').any(|n| n == required) {
                    return Err(format!("{required} is not signed"));
                }
            }
            let signed: Vec<(String, String)> =
                signed_names.split(';').map(|n| (n.to_string(), h(n).unwrap_or_default())).collect();
            let pairs: Vec<(String, String)> = query_pairs(uri);
            let borrowed: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let expected = sigv4_authorization(&SigV4Request {
                method: method.as_str(),
                canonical_uri: uri.path(),
                canonical_query: &canonical_query(&borrowed),
                headers: &signed,
                payload_hash: &payload_hash,
                amz_date: &date,
                region: "us-east-1",
                service: "s3",
                access_key: ACCESS,
                secret_key: SECRET,
            });
            if expected != auth {
                return Err(format!("bad signature for {method} {uri}"));
            }
            Ok(())
        }

        fn query_pairs(uri: &Uri) -> Vec<(String, String)> {
            uri.query()
                .unwrap_or("")
                .split('&')
                .filter(|p| !p.is_empty())
                .map(|p| match p.split_once('=') {
                    Some((k, v)) => (decode(k), decode(v)),
                    None => (decode(p), String::new()),
                })
                .collect()
        }

        fn tags(xml: &str, tag: &str) -> Vec<String> {
            let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
            xml.split(&open).skip(1).filter_map(|s| s.split(&close).next()).map(str::to_string).collect()
        }

        fn error(status: StatusCode, code: &str) -> Response {
            (status, format!("<Error><Code>{code}</Code><Message>mock {code}</Message></Error>")).into_response()
        }

        async fn handle(State(st): State<Shared>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
            let mut s = st.lock().unwrap();
            if let Err(v) = verify(&method, &uri, &headers, &body) {
                s.violations.push(v.clone());
                // Like S3: the key id and string to sign come back in the body.
                return (
                    StatusCode::FORBIDDEN,
                    format!(
                        "<Error><Code>SignatureDoesNotMatch</Code><Message>The request signature we calculated does not match</Message>\
                         <AWSAccessKeyId>{ACCESS}</AWSAccessKeyId><StringToSign>AWS4-HMAC-SHA256 secret-ish</StringToSign></Error>"
                    ),
                )
                    .into_response();
            }
            let key = decode(uri.path());
            let q: HashMap<String, String> = query_pairs(&uri).into_iter().collect();
            let ct = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            match (method.as_str(), q.get("uploads"), q.get("uploadId"), q.get("partNumber")) {
                ("POST", Some(_), None, None) => {
                    s.next_id += 1;
                    // An id that needs XML escaping and URL encoding.
                    let id = format!("up/{}+x&y", s.next_id);
                    s.log.push("create".into());
                    s.uploads.insert(id.clone(), Upload { key: key.clone(), content_type: ct, parts: BTreeMap::new() });
                    let xml = format!(
                        "<?xml version=\"1.0\"?><InitiateMultipartUploadResult><Key>k</Key><UploadId>{}</UploadId></InitiateMultipartUploadResult>",
                        id.replace('&', "&amp;")
                    );
                    (StatusCode::OK, xml).into_response()
                }
                ("PUT", None, Some(id), Some(n)) => {
                    let n: u32 = n.parse().unwrap();
                    if let Some((left, status, code)) = s.fail_part.get_mut(&n).filter(|f| f.0 > 0) {
                        *left -= 1;
                        let (status, code) = (*status, *code);
                        s.log.push(format!("part {n} ({status})"));
                        return error(StatusCode::from_u16(status).unwrap(), code);
                    }
                    if s.on_part.as_ref().is_some_and(|(p, _)| *p == n) {
                        let (_, hook) = s.on_part.take().unwrap();
                        hook();
                    }
                    s.log.push(format!("part {n}"));
                    s.largest_part = s.largest_part.max(body.len());
                    let etag = format!("\"{}\"", &sha256_hex(&body)[..32]);
                    let Some(up) = s.uploads.get_mut(id) else { return error(StatusCode::NOT_FOUND, "NoSuchUpload") };
                    if up.key != key {
                        return error(StatusCode::BAD_REQUEST, "WrongKey");
                    }
                    let last = up.parts.keys().next_back().copied().unwrap_or(0);
                    let violation = (n != last + 1 && n != last).then(|| format!("part {n} arrived after part {last}"));
                    up.parts.insert(n, (etag.clone(), body.to_vec()));
                    if let Some(v) = violation {
                        s.violations.push(v);
                    }
                    if s.etags.len() < n as usize {
                        s.etags.push(etag.clone());
                    } else {
                        s.etags[n as usize - 1] = etag.clone();
                    }
                    ([("etag", etag)], "").into_response()
                }
                ("POST", None, Some(id), None) => {
                    let text = String::from_utf8_lossy(&body).to_string();
                    s.log.push("complete".into());
                    s.complete_bodies.push(text.clone());
                    if s.complete_errors > 0 {
                        s.complete_errors -= 1;
                        return (StatusCode::OK, "<Error><Code>InternalError</Code><Message>try again</Message></Error>").into_response();
                    }
                    let Some(up) = s.uploads.get(id) else { return error(StatusCode::NOT_FOUND, "NoSuchUpload") };
                    let numbers: Vec<u32> = tags(&text, "PartNumber").iter().map(|n| n.parse().unwrap()).collect();
                    let etags: Vec<String> = tags(&text, "ETag").iter().map(|e| e.replace("&quot;", "\"")).collect();
                    if numbers != (1..=up.parts.len() as u32).collect::<Vec<_>>() {
                        let v = format!("complete lists parts {numbers:?} for {} uploaded", up.parts.len());
                        s.violations.push(v);
                        return error(StatusCode::BAD_REQUEST, "InvalidPartOrder");
                    }
                    let mut object = Vec::new();
                    for (i, (n, etag)) in numbers.iter().zip(&etags).enumerate() {
                        match up.parts.get(n) {
                            Some((e, bytes)) if e == etag => {
                                if i + 1 < numbers.len() && bytes.len() < s.min_part {
                                    return error(StatusCode::BAD_REQUEST, "EntityTooSmall");
                                }
                                object.extend_from_slice(bytes);
                            }
                            _ => {
                                let v = format!("part {n}: ETag {etag} does not match");
                                s.violations.push(v);
                                return error(StatusCode::BAD_REQUEST, "InvalidPart");
                            }
                        }
                    }
                    let up = s.uploads.remove(id).unwrap();
                    s.objects.insert(up.key.clone(), (up.content_type, object));
                    if s.complete_then_500 > 0 {
                        s.complete_then_500 -= 1;
                        return error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError");
                    }
                    (StatusCode::OK, "<CompleteMultipartUploadResult><Key>k</Key></CompleteMultipartUploadResult>").into_response()
                }
                ("DELETE", None, Some(id), None) => {
                    s.log.push("abort".into());
                    if s.uploads.remove(id).is_none() {
                        return error(StatusCode::NOT_FOUND, "NoSuchUpload");
                    }
                    s.aborted.push(id.clone());
                    StatusCode::NO_CONTENT.into_response()
                }
                ("HEAD", None, None, None) => {
                    s.log.push("head".into());
                    match s.objects.get(&key) {
                        Some((_, bytes)) => ([("content-length", bytes.len().to_string())], "").into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                }
                ("PUT", None, None, None) => {
                    s.log.push("put".into());
                    s.objects.insert(key, (ct, body.to_vec()));
                    ([("etag", "\"single\"")], "").into_response()
                }
                _ => {
                    s.violations.push(format!("unexpected {method} {uri}"));
                    StatusCode::BAD_REQUEST.into_response()
                }
            }
        }
    }

    fn mock_cfg(endpoint: &str) -> S3Config {
        S3Config { endpoint: endpoint.into(), bucket: "captures".into(), prefix: "team/bugle".into(), ..Default::default() }
    }

    fn mock_creds() -> S3Credentials {
        S3Credentials { access_key: mock::ACCESS.into(), secret_key: mock::SECRET.into() }
    }

    /// Small parts for small fixtures (the mock's 5 MiB minimum is lowered
    /// to match where a test needs it); no real waiting between attempts.
    fn fast_tuning(threshold: u64, part_size: u64, attempts: u32) -> UploadTuning {
        UploadTuning {
            multipart_threshold: threshold,
            part_size,
            attempts,
            backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
        }
    }

    /// Run a blocking upload while the mock server keeps serving.
    async fn upload(endpoint: &str, path: &Path, name: &str, tuning: UploadTuning) -> Result<String> {
        let (cfg, path, name) = (mock_cfg(endpoint), path.to_path_buf(), name.to_string());
        tokio::task::spawn_blocking(move || upload_with(&cfg, &mock_creds(), &path, &name, &tuning)).await.unwrap()
    }

    async fn mock() -> (mock::Shared, String) {
        let state = mock::shared();
        let endpoint = mock::start(state.clone()).await;
        (state, endpoint)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multipart_upload_streams_parts_in_order_and_retries() {
        let (state, endpoint) = mock().await;
        {
            let mut s = state.lock().unwrap();
            s.fail_part.insert(2, (1, 500, "InternalError"));
            s.complete_errors = 1;
        }
        let tmp = tempfile::tempdir().unwrap();
        // Real S3 part sizes: two full 5 MiB parts and a short last one.
        let part = 5 * MIB;
        let data = pseudo_random(2 * part as usize + MIB as usize + 7);
        let path = write(&tmp.path().join("long.mp4"), &data);

        let url = upload(&endpoint, &path, "Long clip.mp4", fast_tuning(1024, part, 3)).await.unwrap();
        assert_eq!(url, format!("{endpoint}/captures/team/bugle/Long%20clip.mp4"));

        let s = state.lock().unwrap();
        assert!(s.violations.is_empty(), "{:?}", s.violations);
        assert_eq!(s.log, ["create", "part 1", "part 2 (500)", "part 2", "part 3", "complete", "complete"]);
        let (ct, object) = &s.objects["/captures/team/bugle/Long clip.mp4"];
        assert_eq!(ct, "video/mp4", "the content type is set when the upload is created");
        assert!(object == &data, "the parts reassemble into the file");
        assert_eq!(s.largest_part, part as usize);
        // The complete body lists every part once, in order, with its ETag.
        assert_eq!(s.etags.len(), 3);
        assert_eq!(s.complete_bodies.last().unwrap(), &complete_body(&s.etags));
        assert!(s.aborted.is_empty() && s.uploads.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multipart_upload_aborts_when_a_part_keeps_failing() {
        let (state, endpoint) = mock().await;
        state.lock().unwrap().fail_part.insert(2, (99, 503, "SlowDown"));
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp.path().join("rec.mov"), &pseudo_random(5000));

        let err = upload(&endpoint, &path, "rec.mov", fast_tuning(1000, 2000, 3)).await.unwrap_err();
        assert!(format!("{err:#}").contains("upload part 2/3 (attempt 3/3)"), "{err:#}");

        let s = state.lock().unwrap();
        assert!(s.violations.is_empty(), "{:?}", s.violations);
        assert_eq!(s.log, ["create", "part 1", "part 2 (503)", "part 2 (503)", "part 2 (503)", "abort"]);
        assert_eq!(s.aborted, ["up/1+x&y"], "the upload id is unescaped and round-trips through the query string");
        assert!(s.objects.is_empty() && s.uploads.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_timeout_from_the_server_is_retried() {
        let (state, endpoint) = mock().await;
        {
            let mut s = state.lock().unwrap();
            s.fail_part.insert(1, (1, 400, "RequestTimeout"));
            s.min_part = 0;
        }
        let tmp = tempfile::tempdir().unwrap();
        let data = pseudo_random(3000);
        let path = write(&tmp.path().join("a.mov"), &data);
        upload(&endpoint, &path, "a.mov", fast_tuning(1000, 2000, 3)).await.unwrap();
        let s = state.lock().unwrap();
        assert_eq!(s.log, ["create", "part 1 (400)", "part 1", "part 2", "complete"]);
        assert_eq!(s.objects["/captures/team/bugle/a.mov"].1, data);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rejected_complete_aborts_the_upload() {
        // Parts below the server's 5 MiB minimum: complete answers
        // EntityTooSmall (a 400, not retried) and the upload is aborted.
        let (state, endpoint) = mock().await;
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp.path().join("b.mov"), &pseudo_random(5000));
        let err = upload(&endpoint, &path, "b.mov", fast_tuning(1000, 2000, 3)).await.unwrap_err();
        assert!(format!("{err:#}").contains("EntityTooSmall"), "{err:#}");
        let s = state.lock().unwrap();
        assert_eq!(s.log, ["create", "part 1", "part 2", "part 3", "complete", "abort"]);
        assert_eq!(s.aborted.len(), 1);
        assert!(s.objects.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_complete_that_keeps_answering_an_error_document_aborts() {
        let (state, endpoint) = mock().await;
        {
            let mut s = state.lock().unwrap();
            s.complete_errors = 99;
            s.min_part = 0;
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp.path().join("c.mov"), &pseudo_random(3000));
        let err = upload(&endpoint, &path, "c.mov", fast_tuning(1000, 2000, 3)).await.unwrap_err();
        assert!(format!("{err:#}").contains("complete multipart upload (attempt 3/3)"), "{err:#}");
        let s = state.lock().unwrap();
        assert_eq!(s.log, ["create", "part 1", "part 2", "complete", "complete", "complete", "abort"]);
        assert!(s.objects.is_empty() && s.uploads.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lost_complete_response_is_recognised_by_the_stored_object() {
        let (state, endpoint) = mock().await;
        {
            let mut s = state.lock().unwrap();
            s.complete_then_500 = 1;
            s.min_part = 0;
        }
        let tmp = tempfile::tempdir().unwrap();
        let data = pseudo_random(3000);
        let path = write(&tmp.path().join("d.mov"), &data);
        let url = upload(&endpoint, &path, "d.mov", fast_tuning(1000, 2000, 3)).await.unwrap();
        assert!(url.ends_with("/d.mov"));
        let s = state.lock().unwrap();
        assert!(s.violations.is_empty(), "{:?}", s.violations);
        // First complete worked but "lost" its response; the retry finds no
        // upload, the HEAD finds the object of the right size.
        assert_eq!(s.log, ["create", "part 1", "part 2", "complete", "complete", "head"]);
        assert_eq!(s.objects["/captures/team/bugle/d.mov"].1, data);
        assert!(s.aborted.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_that_shrinks_or_grows_mid_upload_is_aborted() {
        let tmp = tempfile::tempdir().unwrap();
        for (grow, part) in [(false, 1), (true, 3)] {
            let (state, endpoint) = mock().await;
            let path = write(&tmp.path().join("e.mov"), &pseudo_random(5000));
            let p = path.clone();
            let hook: mock::Hook = Box::new(move || {
                let f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
                if grow {
                    use std::io::Write as _;
                    (&f).write_all(b"more frames").unwrap();
                } else {
                    f.set_len(1500).unwrap();
                }
            });
            {
                let mut s = state.lock().unwrap();
                s.min_part = 0;
                s.on_part = Some((part, hook));
            }
            let err = upload(&endpoint, &path, "e.mov", fast_tuning(1000, 2000, 3)).await.unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains(if grow { "changed while it was being uploaded" } else { "shrank" }), "{msg}");
            let s = state.lock().unwrap();
            assert_eq!(s.log.last().map(String::as_str), Some("abort"), "{:?}", s.log);
            assert!(!s.log.iter().any(|l| l == "complete"), "{:?}", s.log);
            assert!(s.objects.is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reserved_characters_in_keys_are_signed_correctly() {
        let (state, endpoint) = mock().await;
        state.lock().unwrap().min_part = 0;
        let tmp = tempfile::tempdir().unwrap();
        let name = "Ä + & = # ? % ' ( ) ! * ~ ; , @ $ [ ].png";
        let data = pseudo_random(3000);
        let path = write(&tmp.path().join("odd.png"), &data);
        for threshold in [10_000, 1000] {
            let url = upload(&endpoint, &path, name, fast_tuning(threshold, 2000, 3)).await.unwrap();
            assert!(url.ends_with(&uri_encode(name, false)), "{url}");
        }
        let s = state.lock().unwrap();
        assert!(s.violations.is_empty(), "{:?}", s.violations);
        assert_eq!(s.log, ["put", "create", "part 1", "part 2", "complete"]);
        assert_eq!(s.objects[&format!("/captures/team/bugle/{name}")].1, data);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_errors_are_not_retried_and_do_not_leak_details() {
        let (state, endpoint) = mock().await;
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp.path().join("a.png"), b"png");
        let (cfg, p) = (mock_cfg(&endpoint), path.clone());
        let bad = S3Credentials { access_key: mock::ACCESS.into(), secret_key: "wrong".into() };
        let err = tokio::task::spawn_blocking(move || upload_with(&cfg, &bad, &p, "a.png", &fast_tuning(1000, 1000, 3)))
            .await
            .unwrap()
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("403") && msg.contains("SignatureDoesNotMatch"), "{msg}");
        assert!(!msg.contains(mock::ACCESS) && !msg.contains("StringToSign"), "{msg}");
        let s = state.lock().unwrap();
        assert_eq!(s.violations.len(), 1, "one attempt only: {:?}", s.violations);
        assert!(s.log.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn small_files_are_one_streamed_put() {
        let (state, endpoint) = mock().await;
        let tmp = tempfile::tempdir().unwrap();
        let data = pseudo_random(CHUNK * 2 + 3);
        let path = write(&tmp.path().join("shot.png"), &data);
        let url = upload(&endpoint, &path, "shot.png", fast_tuning(10 * MIB, 5 * MIB, 3)).await.unwrap();
        assert!(url.ends_with("/captures/team/bugle/shot.png"));
        let s = state.lock().unwrap();
        assert!(s.violations.is_empty(), "{:?}", s.violations);
        assert_eq!(s.log, ["put"]);
        assert_eq!(s.objects["/captures/team/bugle/shot.png"], ("image/png".to_string(), data));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sidecar_is_uploaded_next_to_its_video() {
        let (state, endpoint) = mock().await;
        let tmp = tempfile::tempdir().unwrap();
        // Unique variable names: tests run in parallel in one process.
        std::env::set_var("BUGLE_TEST_SIDECAR_KEY", mock::ACCESS);
        std::env::set_var("BUGLE_TEST_SIDECAR_SECRET", mock::SECRET);
        let mut engine = engine_in(tmp.path(), SyncMode::Copy);
        engine.config.general.copy_to_clipboard = true; // keep the URL off the real clipboard
        let mut s3 = mock_cfg(&endpoint);
        s3.access_key_env = "BUGLE_TEST_SIDECAR_KEY".into();
        s3.secret_key_env = "BUGLE_TEST_SIDECAR_SECRET".into();
        engine.config.sync.destinations.insert(
            MediaKind::Video,
            bugle_core::config::SyncTarget { s3: Some(s3), prefix: Some("videos".into()), ..Default::default() },
        );
        let save = engine.config.save_dir();
        let video = MediaItem::new(MediaKind::Video, write(&save.join("demo.mp4"), b"video bytes"));
        engine.history.push(&video).unwrap();
        write(&save.join("demo.vtt"), b"WEBVTT");
        let id = video.id.clone();

        let (video, report) = tokio::task::spawn_blocking(move || engine.sync_media(&id)).await.unwrap().unwrap();
        assert_eq!(report.action, "uploaded");
        assert_eq!(video.url.as_deref(), Some(format!("{endpoint}/captures/team/bugle/videos/demo.mp4").as_str()));
        assert_eq!(report.sidecars.len(), 1);
        assert_eq!(report.sidecars[0].url.as_deref(), Some(format!("{endpoint}/captures/team/bugle/videos/demo.vtt").as_str()));
        let s = state.lock().unwrap();
        assert!(s.violations.is_empty(), "{:?}", s.violations);
        assert_eq!(s.log, ["put", "put"]);
        assert_eq!(s.objects["/captures/team/bugle/videos/demo.mp4"].1, b"video bytes");
        assert_eq!(s.objects["/captures/team/bugle/videos/demo.vtt"], ("text/vtt".to_string(), b"WEBVTT".to_vec()));
        assert!(!tmp.path().join("cloud").join("demo.mp4").exists(), "videos skip the default folder");
    }
}
