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

    /// Automatic step after a successful save. Honours `enabled` and `kinds`
    /// and never fails the save: problems are logged.
    pub(crate) fn sync_after_save(&self, item: &mut MediaItem) {
        if !self.config.sync.applies_to(item.kind) {
            return;
        }
        let original = item.path.clone();
        match self.sync_item(item) {
            Ok(Some(r)) => tracing::info!("{} {} → {}", r.action, original.display(), r.destination),
            Ok(None) => {}
            Err(e) => tracing::warn!("sync of {} failed: {e:#}", original.display()),
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
                return self.sync_sidecar(&video, item);
            }
        }
        let Some(dest) = self.config.sync.destination_for(item.kind)? else { return Ok(None) };
        let mode = self.config.sync.mode_for(item.kind);
        let original = item.path.clone();
        let mut report = self.sync_file(&dest, mode, item, None, true)?;
        if self.config.sync.sidecars && item.is_video() {
            report.sidecars = self.sync_sidecars_of(&dest, mode, &original, item);
        }
        Ok(Some(report))
    }

    /// `bugle sync <source>`: sync an existing capture on demand, whatever
    /// `enabled` and `kinds` say, and record the new path / URL in history.
    pub fn sync_media(&self, source: &str) -> Result<(MediaItem, SyncReport)> {
        let mut item = self.resolve_media(source)?;
        let report = self.sync_item(&mut item)?.ok_or_else(|| {
            anyhow!(
                "no sync destination configured for {}: set [sync] folder or [sync.s3] in {}",
                kind_name(item.kind),
                Config::default_path().display()
            )
        })?;
        if let Some(stored) = self.history.get(&item.id)? {
            if stored.path != item.path || stored.url != item.url || stored.synced_as != item.synced_as {
                self.history.update(&item)?;
            }
        }
        Ok((item, report))
    }

    /// After `subtitles` wrote `sub` for `video`: sync it when the video was
    /// synced or would be synced automatically. Never fails the caller.
    pub(crate) fn sync_new_sidecar(&self, video: &MediaItem, sub: &mut MediaItem) {
        let sync = &self.config.sync;
        if !sync.sidecars || !video.is_video() || !(video.synced_as.is_some() || sync.applies_to(video.kind)) {
            return;
        }
        match self.sync_sidecar(video, sub) {
            Ok(Some(r)) => tracing::info!("{} {} → {}", r.action, sub.path.display(), r.destination),
            Ok(None) => {}
            Err(e) => tracing::warn!("sync of {} failed: {e:#}", sub.path.display()),
        }
    }

    /// Sync one subtitle to `video`'s destination as `{video stem}.{ext}`.
    fn sync_sidecar(&self, video: &MediaItem, sub: &mut MediaItem) -> Result<Option<SyncReport>> {
        let Some(dest) = self.config.sync.destination_for(video.kind)? else { return Ok(None) };
        let name = sidecar_name(video, &sub.path)?;
        let mode = self.config.sync.mode_for(video.kind);
        self.sync_file(&dest, mode, sub, Some(&name), false).map(Some)
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
    fn sync_sidecars_of(&self, dest: &SyncDestination, mode: SyncMode, original: &Path, video: &MediaItem) -> Vec<SyncReport> {
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
            let result = sidecar_name(video, &path).and_then(|name| self.sync_file(dest, mode, &mut sub, Some(&name), false));
            match result {
                Ok(r) => {
                    if let Some(old) = known {
                        if old.path != sub.path || old.url != sub.url || old.synced_as != sub.synced_as {
                            if let Err(e) = self.history.update(&sub) {
                                tracing::warn!("could not record the synced sidecar {}: {e:#}", sub.path.display());
                            }
                        }
                    }
                    reports.push(r);
                }
                Err(e) => tracing::warn!("sync of sidecar {} failed: {e:#}", path.display()),
            }
        }
        reports
    }

    /// Send `item` to `dest`. `name` is the file name at the destination:
    /// `None` keeps the local name (a unique one in a folder), `Some` is an
    /// exact name that replaces whatever is there (sidecars). `announce`
    /// copies an upload's URL to the clipboard.
    fn sync_file(
        &self,
        dest: &SyncDestination,
        mode: SyncMode,
        item: &mut MediaItem,
        name: Option<&str>,
        announce: bool,
    ) -> Result<SyncReport> {
        match dest {
            SyncDestination::Folder(dir) => sync_to_folder(dir, mode, item, name),
            SyncDestination::S3(s3) => {
                let name = match name {
                    Some(n) => n.to_string(),
                    None => file_name(&item.path)?.to_string(),
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

/// `{stem the video was synced as}.{subtitle extension}`.
fn sidecar_name(video: &MediaItem, sub: &Path) -> Result<String> {
    let synced = video.synced_as.clone().map(PathBuf::from).unwrap_or_else(|| video.path.clone());
    let stem = synced.file_stem().and_then(|s| s.to_str()).ok_or_else(|| anyhow!("{} has no file name", synced.display()))?;
    let ext = sub.extension().and_then(|e| e.to_str()).unwrap_or("srt");
    Ok(format!("{stem}.{ext}"))
}

fn same_stem(a: &Path, b: &Path) -> bool {
    a.parent() == b.parent() && a.file_stem().is_some() && a.file_stem() == b.file_stem()
}

// ---------------------------------------------------------------------------
// Folder destination
// ---------------------------------------------------------------------------

fn sync_to_folder(dir: &Path, mode: SyncMode, item: &mut MediaItem, name: Option<&str>) -> Result<SyncReport> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating sync folder {}", dir.display()))?;
    let label = folder_label(dir);
    let dest = match name {
        Some(n) => dir.join(n),
        None => {
            let already_there = match (item.path.parent().and_then(|p| p.canonicalize().ok()), dir.canonicalize()) {
                (Some(parent), Ok(dir)) => parent == dir,
                _ => false,
            };
            if already_there {
                item.synced_as = Some(file_name(&item.path)?.to_string());
                return Ok(SyncReport::new("already there", label, item.path.clone(), None));
            }
            unique_in_dir(dir, file_name(&item.path)?)
        }
    };
    if let (Ok(a), Ok(b)) = (item.path.canonicalize(), dest.canonicalize()) {
        if a == b {
            item.synced_as = Some(file_name(&dest)?.to_string());
            return Ok(SyncReport::new("already there", label, item.path.clone(), None));
        }
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
/// S3's smallest part (except the last) and largest part count.
const MIN_PART_SIZE: u64 = 5 * MIB;
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
    /// Wait before the second attempt; doubled for each one after that.
    pub backoff: Duration,
}

impl UploadTuning {
    pub fn from_config(cfg: &S3Config) -> Self {
        Self {
            multipart_threshold: cfg.multipart_threshold_mb.max(5) * MIB,
            part_size: (cfg.part_size_mb * MIB).max(MIN_PART_SIZE),
            attempts: 4,
            backoff: Duration::from_millis(500),
        }
    }

    fn part_size_for(&self, size: u64) -> u64 {
        self.part_size.max(1).max(size.div_ceil(MAX_PARTS))
    }
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
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(600))
            .build()?;
        if size >= tuning.multipart_threshold && size > 0 {
            multipart_upload(&client, &target, &path, size, content_type, &tuning).await
        } else {
            let hash = hash_range(&path, 0, size)?;
            let what = format!("PUT {}", target.object_url());
            with_retry(&tuning, &what, || {
                let call = Call::new("PUT", Vec::new(), &hash).content_type(content_type).body(range_body(&path, 0, size), size);
                send(&client, &target, call)
            })
            .await
            .map(|_| ())
        }
    })?;
    let public = match cfg.public_url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(base) => format!("{}/{}", base.trim_end_matches('/'), uri_encode(&key, false)),
        None => url,
    };
    Ok(public)
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

/// A failed attempt, and whether trying again could help.
struct Attempt {
    error: anyhow::Error,
    retry: bool,
}

impl Attempt {
    fn fatal(error: anyhow::Error) -> Self {
        Self { error, retry: false }
    }
}

/// A finished request: headers we care about and the body text.
struct Reply {
    etag: Option<String>,
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

/// Sign and send one request. Transport errors, 5xx and 429 are worth
/// retrying; other 4xx are not. The length is always sent (and signed) as
/// `content-length`, so a streamed body never falls back to chunked
/// transfer encoding, which S3 rejects.
async fn send(client: &reqwest::Client, target: &S3Target, call: Call<'_>) -> std::result::Result<Reply, Attempt> {
    let Call { method, query, payload_hash, mut headers, body } = call;
    let len = body.as_ref().map(|(_, n)| *n).unwrap_or(0);
    headers.push(("content-length".into(), len.to_string()));
    let (url, headers) = target.sign(method, &query, payload_hash, headers);
    let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| Attempt::fatal(e.into()))?;
    let mut req = client.request(m, &url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some((body, _)) = body {
        req = req.body(body);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| Attempt { error: anyhow::Error::new(e).context(format!("{method} {url}")), retry: true })?;
    let status = resp.status();
    let etag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
    let text = resp.text().await.unwrap_or_default();
    // CompleteMultipartUpload can answer 200 with an <Error> document.
    let error_body = text.contains("<Error>");
    if !status.is_success() || error_body {
        let detail = text.trim();
        let detail = if detail.is_empty() { String::new() } else { format!(": {detail}") };
        return Err(Attempt {
            error: anyhow!("{method} {url} failed: {status}{detail}"),
            retry: status.is_server_error() || status.as_u16() == 429 || (status.is_success() && error_body),
        });
    }
    Ok(Reply { etag, text })
}

/// Run `f` until it succeeds, it fails for good, or attempts run out.
async fn with_retry<F, Fut, T>(tuning: &UploadTuning, what: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, Attempt>>,
{
    let attempts = tuning.attempts.max(1);
    let mut wait = tuning.backoff;
    for n in 1..=attempts {
        match f().await {
            Ok(v) => return Ok(v),
            Err(a) if a.retry && n < attempts => {
                tracing::warn!("{what}: attempt {n}/{attempts} failed, retrying: {:#}", a.error);
                tokio::time::sleep(wait).await;
                wait = wait.saturating_mul(2);
            }
            Err(a) => return Err(a.error.context(format!("{what} (attempt {n}/{attempts})"))),
        }
    }
    unreachable!("the loop returns on the last attempt")
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
    let reply = with_retry(tuning, "create multipart upload", || {
        send(client, target, Call::new("POST", vec![("uploads", "")], &empty).content_type(content_type))
    })
    .await?;
    let upload_id = xml_value(&reply.text, "UploadId")
        .ok_or_else(|| anyhow!("create multipart upload: no UploadId in the response: {}", reply.text.trim()))?;

    let result = upload_parts_and_complete(client, target, path, size, tuning, &upload_id).await;
    if let Err(e) = result {
        let abort = with_retry(tuning, "abort multipart upload", || {
            send(client, target, Call::new("DELETE", vec![("uploadId", upload_id.as_str())], &empty))
        })
        .await;
        if let Err(a) = abort {
            tracing::warn!("{a:#}; the incomplete upload {upload_id} may linger until a bucket lifecycle rule removes it");
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
        let reply = with_retry(tuning, &what, || async move {
            let call = Call::new("PUT", vec![("partNumber", number), ("uploadId", upload_id)], hash).body(range_body(path, offset, len), len);
            let reply = send(client, target, call).await?;
            match reply.etag.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
                Some(etag) => Ok(etag.to_string()),
                None => Err(Attempt { error: anyhow!("part {n}: the response has no ETag"), retry: true }),
            }
        })
        .await?;
        etags.push(reply);
    }
    let body = complete_body(&etags);
    let hash = sha256_hex(body.as_bytes());
    with_retry(tuning, "complete multipart upload", || {
        let call = Call::new("POST", vec![("uploadId", upload_id)], &hash)
            .content_type("application/xml")
            .body(body.clone().into(), body.len() as u64);
        send(client, target, call)
    })
    .await?;
    Ok(())
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

/// Text of the first `<tag>…</tag>`.
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

/// A request body that streams `len` bytes at `offset` from disk in
/// `CHUNK`-sized pieces; the file is opened when the body is first polled.
fn range_body(path: &Path, offset: u64, len: u64) -> reqwest::Body {
    struct State {
        path: PathBuf,
        file: Option<std::fs::File>,
        offset: u64,
        left: u64,
    }
    let state = State { path: path.to_path_buf(), file: None, offset, left: len };
    let stream = futures::stream::unfold(state, |mut st| async move {
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
    });
    reqwest::Body::wrap_stream(stream)
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
    use bugle_core::MediaKind;

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

    #[test]
    fn each_kind_goes_to_its_own_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_in(tmp.path(), SyncMode::Copy);
        let videos = tmp.path().join("videos");
        engine.config.sync.destinations.insert(
            MediaKind::Video,
            bugle_core::config::SyncTarget { folder: Some(videos.display().to_string()), ..Default::default() },
        );
        engine.config.sync.destinations.insert(
            MediaKind::Gif,
            bugle_core::config::SyncTarget { prefix: Some("gifs".into()), mode: Some(SyncMode::Move), ..Default::default() },
        );
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

        let mut gif = MediaItem::new(MediaKind::Gif, write(&save.join("g.gif"), b"gif"));
        engine.sync_after_save(&mut gif);
        assert_eq!(gif.path, cloud.join("gifs").join("g.gif"), "the gif entry moves into its sub-folder");
        assert!(!save.join("g.gif").exists());

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
        let mut sub = MediaItem::new(MediaKind::Subtitle, &vtt);
        sub.parent = Some(video.id.clone());
        engine.sync_new_sidecar(&video, &mut sub);
        assert_eq!(std::fs::read(cloud.join("clip 2.vtt")).unwrap(), b"WEBVTT");
        assert_eq!(sub.synced_as.as_deref(), Some("clip 2.vtt"));

        // Regenerated subtitles replace the synced sidecar, and a sidecar
        // given by path finds its video through history.
        write(&vtt, b"WEBVTT\n\nnew");
        let (_, r) = engine.sync_media(vtt.to_str().unwrap()).unwrap();
        assert_eq!(r.action, "copied");
        assert_eq!(std::fs::read(cloud.join("clip 2.vtt")).unwrap(), b"WEBVTT\n\nnew");
        assert!(!cloud.join("clip 2 2.vtt").exists() && !cloud.join("clip.vtt").exists());
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
        assert_eq!(engine.history.get(&sub.id).unwrap().unwrap().path, cloud.join("rec.srt"));

        // A sidecar written next to the moved video is already in place.
        let vtt = write(&cloud.join("rec.vtt"), b"vtt");
        let mut item = MediaItem::new(MediaKind::Subtitle, &vtt);
        engine.sync_new_sidecar(&video, &mut item);
        assert_eq!(item.path, vtt);
        assert!(vtt.exists());

        // With `sidecars = false` a subtitle is synced like any other file.
        engine.config.sync.sidecars = false;
        engine.config.sync.mode = SyncMode::Copy;
        let lone = write(&save.join("rec.vtt"), b"lone");
        let (_, r) = engine.sync_media(lone.to_str().unwrap()).unwrap();
        assert_eq!(r.destination, folder_label(&cloud));
        assert_eq!(std::fs::read(cloud.join("rec 2.vtt")).unwrap(), b"lone");
    }

    #[test]
    fn new_sidecars_only_sync_when_their_video_does() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = engine_in(tmp.path(), SyncMode::Copy);
        engine.config.sync.enabled = false;
        let save = engine.config.save_dir();
        let video = MediaItem::new(MediaKind::Video, write(&save.join("x.mp4"), b"v"));
        let mut sub = MediaItem::new(MediaKind::Subtitle, write(&save.join("x.srt"), b"s"));
        engine.sync_new_sidecar(&video, &mut sub);
        assert!(!tmp.path().join("cloud").join("x.srt").exists(), "the video was never synced");
        let mut synced = video.clone();
        synced.synced_as = Some("x 7.mp4".into());
        engine.sync_new_sidecar(&synced, &mut sub);
        assert!(tmp.path().join("cloud").join("x 7.srt").exists(), "a manually synced video takes its subtitles along");
    }

    // -----------------------------------------------------------------------
    // S3: query canonicalisation, tuning, and a mock server
    // -----------------------------------------------------------------------

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
        //! An in-process S3 stand-in that verifies every request the way a
        //! real server would: SigV4 signature recomputed from what arrived,
        //! payload hash, content length, part order and the complete body.
        use super::super::*;
        use axum::body::Bytes;
        use axum::extract::State;
        use axum::http::{HeaderMap, Method, StatusCode, Uri};
        use axum::response::{IntoResponse, Response};
        use std::collections::{BTreeMap, HashMap};
        use std::sync::{Arc, Mutex};

        pub const ACCESS: &str = "AKIDMOCK";
        pub const SECRET: &str = "mock/secret/key";

        #[derive(Default)]
        pub struct Upload {
            key: String,
            content_type: String,
            parts: BTreeMap<u32, (String, Vec<u8>)>,
        }

        #[derive(Default)]
        pub struct S3State {
            /// `create`, `part 1`, `part 2 (500)`, `complete`, `abort`, `put` …
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
            /// Part number → how many more times it answers 500.
            pub fail_part: HashMap<u32, u32>,
            /// How many more times `complete` answers 200 with an `<Error>`.
            pub complete_errors: u32,
            pub largest_part: usize,
            next_id: u32,
        }

        pub type Shared = Arc<Mutex<S3State>>;

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

        /// Independent check of the signature: rebuild the canonical request
        /// from the raw request line and headers.
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
            if h("content-length").map(|l| l.parse::<usize>().ok()) != Some(Some(body.len())) {
                return Err(format!("content-length {:?} for a {}-byte body", h("content-length"), body.len()));
            }
            for required in ["host", "x-amz-content-sha256", "x-amz-date", "content-length"] {
                if !signed_names.split(';').any(|n| n == required) {
                    return Err(format!("{required} is not signed"));
                }
            }
            let signed: Vec<(String, String)> =
                signed_names.split(';').map(|n| (n.to_string(), h(n).unwrap_or_default())).collect();
            let pairs: Vec<(String, String)> = uri
                .query()
                .unwrap_or("")
                .split('&')
                .filter(|p| !p.is_empty())
                .map(|p| match p.split_once('=') {
                    Some((k, v)) => (decode(k), decode(v)),
                    None => (decode(p), String::new()),
                })
                .collect();
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

        fn query_map(uri: &Uri) -> HashMap<String, String> {
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

        async fn handle(State(st): State<Shared>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
            let mut s = st.lock().unwrap();
            if let Err(v) = verify(&method, &uri, &headers, &body) {
                s.violations.push(v.clone());
                return (StatusCode::FORBIDDEN, format!("<Error><Code>SignatureDoesNotMatch</Code><Message>{v}</Message></Error>"))
                    .into_response();
            }
            let key = decode(uri.path());
            let q = query_map(&uri);
            let ct = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            match (method.as_str(), q.get("uploads"), q.get("uploadId"), q.get("partNumber")) {
                ("POST", Some(_), None, None) => {
                    s.next_id += 1;
                    let id = format!("up/{}+x", s.next_id);
                    s.log.push("create".into());
                    s.uploads.insert(id.clone(), Upload { key: key.clone(), content_type: ct, parts: BTreeMap::new() });
                    let xml = format!(
                        "<?xml version=\"1.0\"?><InitiateMultipartUploadResult><Key>{key}</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"
                    );
                    (StatusCode::OK, xml).into_response()
                }
                ("PUT", None, Some(id), Some(n)) => {
                    let n: u32 = n.parse().unwrap();
                    if let Some(left) = s.fail_part.get_mut(&n).filter(|l| **l > 0) {
                        *left -= 1;
                        s.log.push(format!("part {n} (500)"));
                        return (StatusCode::INTERNAL_SERVER_ERROR, "<Error><Code>InternalError</Code></Error>").into_response();
                    }
                    s.log.push(format!("part {n}"));
                    s.largest_part = s.largest_part.max(body.len());
                    let etag = format!("\"{}\"", &sha256_hex(&body)[..32]);
                    let Some(up) = s.uploads.get_mut(id) else {
                        return (StatusCode::NOT_FOUND, "<Error><Code>NoSuchUpload</Code></Error>").into_response();
                    };
                    let last = up.parts.keys().next_back().copied().unwrap_or(0);
                    let violation = (n != last + 1 && n != last).then(|| format!("part {n} arrived after part {last}"));
                    if up.key != key {
                        return (StatusCode::BAD_REQUEST, "wrong key").into_response();
                    }
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
                    let Some(up) = s.uploads.remove(id) else {
                        return (StatusCode::NOT_FOUND, "<Error><Code>NoSuchUpload</Code></Error>").into_response();
                    };
                    let numbers: Vec<u32> = tags(&text, "PartNumber").iter().map(|n| n.parse().unwrap()).collect();
                    let etags: Vec<String> = tags(&text, "ETag").iter().map(|e| e.replace("&quot;", "\"")).collect();
                    let mut problems = Vec::new();
                    if numbers != (1..=up.parts.len() as u32).collect::<Vec<_>>() {
                        problems.push(format!("complete lists parts {numbers:?} for {} uploaded", up.parts.len()));
                    }
                    let mut object = Vec::new();
                    for (n, etag) in numbers.iter().zip(&etags) {
                        match up.parts.get(n) {
                            Some((e, bytes)) if e == etag => object.extend_from_slice(bytes),
                            _ => problems.push(format!("part {n}: ETag {etag} does not match")),
                        }
                    }
                    if !problems.is_empty() {
                        s.violations.extend(problems);
                        return (StatusCode::BAD_REQUEST, "<Error><Code>InvalidPart</Code></Error>").into_response();
                    }
                    s.objects.insert(up.key.clone(), (up.content_type, object));
                    (StatusCode::OK, format!("<CompleteMultipartUploadResult><Key>{}</Key></CompleteMultipartUploadResult>", up.key))
                        .into_response()
                }
                ("DELETE", None, Some(id), None) => {
                    s.log.push("abort".into());
                    s.uploads.remove(id);
                    s.aborted.push(id.clone());
                    StatusCode::NO_CONTENT.into_response()
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

    fn fast_tuning(threshold: u64, part_size: u64, attempts: u32) -> UploadTuning {
        UploadTuning { multipart_threshold: threshold, part_size, attempts, backoff: Duration::from_millis(1) }
    }

    /// Run a blocking upload while the mock server keeps serving.
    async fn upload(endpoint: &str, path: &Path, name: &str, tuning: UploadTuning) -> Result<String> {
        let (cfg, path, name) = (mock_cfg(endpoint), path.to_path_buf(), name.to_string());
        tokio::task::spawn_blocking(move || upload_with(&cfg, &mock_creds(), &path, &name, &tuning)).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multipart_upload_streams_parts_in_order_and_retries() {
        let state = mock::Shared::default();
        let endpoint = mock::start(state.clone()).await;
        {
            let mut s = state.lock().unwrap();
            s.fail_part.insert(2, 1);
            s.complete_errors = 1;
        }
        let tmp = tempfile::tempdir().unwrap();
        let part = 600 * 1024u64; // spans several read chunks
        let data = pseudo_random(2 * part as usize + 300 * 1024 + 7);
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
        let body = s.complete_bodies.last().unwrap();
        assert_eq!(body, &complete_body(&s.etags));
        assert_eq!(s.etags.len(), 3);
        assert!(s.aborted.is_empty() && s.uploads.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multipart_upload_aborts_when_a_part_keeps_failing() {
        let state = mock::Shared::default();
        let endpoint = mock::start(state.clone()).await;
        state.lock().unwrap().fail_part.insert(2, 99);
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp.path().join("rec.mov"), &pseudo_random(5000));

        let err = upload(&endpoint, &path, "rec.mov", fast_tuning(1000, 2000, 3)).await.unwrap_err();
        assert!(format!("{err:#}").contains("upload part 2/3 (attempt 3/3)"), "{err:#}");

        let s = state.lock().unwrap();
        assert!(s.violations.is_empty(), "{:?}", s.violations);
        assert_eq!(s.log, ["create", "part 1", "part 2 (500)", "part 2 (500)", "part 2 (500)", "abort"]);
        assert_eq!(s.aborted, ["up/1+x"], "the upload id round-trips through the query string");
        assert!(s.objects.is_empty() && s.uploads.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_errors_are_not_retried() {
        let state = mock::Shared::default();
        let endpoint = mock::start(state.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp.path().join("a.png"), b"png");
        let (cfg, p) = (mock_cfg(&endpoint), path.clone());
        let bad = S3Credentials { access_key: mock::ACCESS.into(), secret_key: "wrong".into() };
        let err = tokio::task::spawn_blocking(move || upload_with(&cfg, &bad, &p, "a.png", &fast_tuning(1000, 1000, 3)))
            .await
            .unwrap()
            .unwrap_err();
        assert!(format!("{err:#}").contains("403"), "{err:#}");
        let s = state.lock().unwrap();
        assert_eq!(s.violations.len(), 1, "one attempt only: {:?}", s.violations);
        assert!(s.log.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn small_files_are_one_streamed_put() {
        let state = mock::Shared::default();
        let endpoint = mock::start(state.clone()).await;
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
        let state = mock::Shared::default();
        let endpoint = mock::start(state.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        // Unique variable names: tests run in parallel in one process.
        std::env::set_var("BUGLE_TEST_SIDECAR_KEY", mock::ACCESS);
        std::env::set_var("BUGLE_TEST_SIDECAR_SECRET", mock::SECRET);
        let mut engine = engine_in(tmp.path(), SyncMode::Copy);
        engine.config.general.copy_to_clipboard = true; // keep the URL off the real clipboard
        engine.config.sync.folder = Some(tmp.path().join("cloud").display().to_string());
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
        let s = state.lock().unwrap();
        assert!(s.violations.is_empty(), "{:?}", s.violations);
        assert_eq!(s.objects["/captures/team/bugle/videos/demo.vtt"], ("text/vtt".to_string(), b"WEBVTT".to_vec()));
        assert!(!tmp.path().join("cloud").join("demo.mp4").exists(), "videos skip the default folder");
    }
}
