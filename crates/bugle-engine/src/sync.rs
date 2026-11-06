//! Sync step: after a file is saved, copy or move it into a folder that
//! Dropbox / iCloud Drive mirrors, or upload it to an S3-compatible bucket
//! with a SigV4-signed `PUT`. Nothing here talks to a hosted sharing service;
//! credentials are read from the environment and never stored.
//!
//! The SigV4 signer is hand-rolled on top of `ring` (already in the
//! dependency graph through `rustls`) so no S3 SDK is pulled in.

use crate::Engine;
use anyhow::{anyhow, bail, Context, Result};
use bugle_core::config::{S3Config, SyncDestination, SyncMode};
use bugle_core::{BugleError, Config, MediaItem};
use ring::{digest, hmac};
use std::path::{Path, PathBuf};

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
}

impl Engine {
    /// The configured destination, or an error when `[sync]` is inconsistent.
    pub fn sync_destination(&self) -> Result<Option<SyncDestination>> {
        self.config.sync.destination()
    }

    /// Short label for the destination (`iCloud Drive/Bugle`, `~/Dropbox/x`,
    /// `s3://bucket/prefix`), for toasts and status lines.
    pub fn sync_destination_label(&self) -> Option<String> {
        match self.config.sync.destination().ok()? {
            Some(SyncDestination::Folder(dir)) => Some(folder_label(&dir)),
            Some(SyncDestination::S3(s3)) => {
                let prefix = normalise_prefix(&s3.prefix);
                Some(format!("s3://{}/{}", s3.bucket, prefix))
            }
            None => None,
        }
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

    /// Sync one item to the configured destination, updating `item.path`
    /// (after a move) and `item.url` (after an upload). `Ok(None)` when no
    /// destination is configured. Does not touch history.
    pub fn sync_item(&self, item: &mut MediaItem) -> Result<Option<SyncReport>> {
        let Some(dest) = self.config.sync.destination()? else { return Ok(None) };
        if !item.path.is_file() {
            return Err(BugleError::MediaNotFound(item.path.display().to_string()).into());
        }
        let report = match dest {
            SyncDestination::Folder(dir) => sync_to_folder(&dir, self.config.sync.mode, item)?,
            SyncDestination::S3(s3) => {
                let url = upload_to_s3(&s3, &item.path)?;
                item.url = Some(url.clone());
                if !self.config.general.copy_to_clipboard {
                    if let Err(e) = bugle_platform::clipboard_write_text(&url) {
                        tracing::warn!("could not copy {url} to the clipboard: {e}");
                    }
                }
                SyncReport { action: "uploaded".into(), destination: url.clone(), path: item.path.clone(), url: Some(url) }
            }
        };
        Ok(Some(report))
    }

    /// `bugle sync <source>`: sync an existing capture on demand, whatever
    /// `enabled` and `kinds` say, and record the new path / URL in history.
    pub fn sync_media(&self, source: &str) -> Result<(MediaItem, SyncReport)> {
        let mut item = self.resolve_media(source)?;
        let report = self.sync_item(&mut item)?.ok_or_else(|| {
            anyhow!("no sync destination configured: set [sync] folder or [sync.s3] in {}", Config::default_path().display())
        })?;
        if let Some(stored) = self.history.get(&item.id)? {
            if stored.path != item.path || stored.url != item.url {
                self.history.update(&item)?;
            }
        }
        Ok((item, report))
    }
}

// ---------------------------------------------------------------------------
// Folder destination
// ---------------------------------------------------------------------------

fn sync_to_folder(dir: &Path, mode: SyncMode, item: &mut MediaItem) -> Result<SyncReport> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating sync folder {}", dir.display()))?;
    let name = item.path.file_name().and_then(|n| n.to_str()).ok_or_else(|| anyhow!("{} has no file name", item.path.display()))?;
    let already_there = match (item.path.parent().and_then(|p| p.canonicalize().ok()), dir.canonicalize()) {
        (Some(parent), Ok(dir)) => parent == dir,
        _ => false,
    };
    let label = folder_label(dir);
    if already_there {
        return Ok(SyncReport { action: "already there".into(), destination: label, path: item.path.clone(), url: None });
    }
    let dest = unique_in_dir(dir, name);
    let action = match mode {
        SyncMode::Copy => {
            std::fs::copy(&item.path, &dest).with_context(|| format!("copying to {}", dest.display()))?;
            "copied"
        }
        SyncMode::Move => {
            move_file(&item.path, &dest)?;
            item.path = dest;
            "moved"
        }
    };
    Ok(SyncReport { action: action.into(), destination: label, path: item.path.clone(), url: None })
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

/// `PUT` the file as `{prefix}/{file name}` and return its public URL.
pub fn upload_to_s3(cfg: &S3Config, path: &Path) -> Result<String> {
    let access_key = env_var(&cfg.access_key_env)?;
    let secret_key = env_var(&cfg.secret_key_env)?;
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
    let key = format!("{}{name}", normalise_prefix(&cfg.prefix));
    let body = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let endpoint = cfg.endpoint.trim().trim_end_matches('/');
    let host = host_of(endpoint)?;
    let canonical_uri = format!("/{}/{}", uri_encode(cfg.bucket.trim(), false), uri_encode(&key, false));
    let url = format!("{endpoint}{canonical_uri}");
    let now = chrono::Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let payload_hash = sha256_hex(&body);
    let content_type = content_type_for(path);
    let headers = vec![
        ("content-type".to_string(), content_type.to_string()),
        ("host".to_string(), host.clone()),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];
    let authorization = sigv4_authorization(&SigV4Request {
        method: "PUT",
        canonical_uri: &canonical_uri,
        canonical_query: "",
        headers: &headers,
        payload_hash: &payload_hash,
        amz_date: &amz_date,
        region: cfg.region.trim(),
        service: "s3",
        access_key: &access_key,
        secret_key: &secret_key,
    });

    let request_url = url.clone();
    let (status, text) = block_on(async move {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(20))
            .timeout(std::time::Duration::from_secs(600))
            .build()?;
        let resp = client
            .put(&request_url)
            .header("content-type", content_type)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", &amz_date)
            .header("authorization", authorization)
            .body(body)
            .send()
            .await
            .with_context(|| format!("PUT {request_url}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Ok::<_, anyhow::Error>((status, text))
    })?;
    if !status.is_success() {
        let detail = text.trim();
        bail!("PUT {url} failed: {status}{}", if detail.is_empty() { String::new() } else { format!(": {detail}") });
    }
    let public = match cfg.public_url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(base) => format!("{}/{}", base.trim_end_matches('/'), uri_encode(&key, false)),
        None => url,
    };
    Ok(public)
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

fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
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
}
