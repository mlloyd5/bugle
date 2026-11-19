//! Append-only JSON-lines history store: a flat file is enough and is
//! trivially inspectable by agents. A record appended again with an id that
//! is already present replaces the earlier one in place (used when a file is
//! moved or gains a sync URL); `DELETE <id>` lines are tombstones that remove
//! the item recorded before them (a record for the same id appended after a
//! tombstone adds it again, at the end).
//!
//! Several processes (CLI, menu-bar app, API server) may append to the same
//! file. Every writer takes an exclusive advisory lock on a sidecar
//! `<file>.lock` and writes each record with a single `write_all`, so lines
//! never interleave. When the file holds more dead lines (superseded records,
//! tombstones and the items they removed) than live items, `open` compacts it
//! under the same lock.
//!
//! Entries whose file was deleted outside Bugle are hidden by
//! [`History::recent`] (every listing) but stay in the file, so a file put
//! back from the Trash or renamed back shows up again with its metadata.
//! Only [`History::prune_missing`] (`bugle history prune`, the
//! `history_prune` action) writes tombstones for them, and it never touches a
//! file. See [`looks_deleted`] and [`History::prune_missing`] for what counts.

use crate::media_item::MediaItem;
use crate::BugleError;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Shortest id prefix `get`/`delete` accept when the id is not an exact match.
pub const MIN_PREFIX_LEN: usize = 4;

/// Entries younger than this are never hidden or pruned: a save or a sync
/// move may be between writing the file and recording its new path.
pub const PRUNE_GRACE: chrono::Duration = chrono::Duration::seconds(60);

/// Whether `m` points at a local file that looks deleted: its path is
/// absolute, the file is `NotFound` (not a permission or I/O error; a broken
/// symlink still exists), its parent directory exists (so an unmounted
/// volume does not count), it has no synced copy (`url`), and it is older
/// than [`PRUNE_GRACE`]. [`History::recent`] hides such entries.
pub fn looks_deleted(m: &MediaItem, now: chrono::DateTime<chrono::Utc>) -> bool {
    if m.url.is_some() || !m.path.is_absolute() || now - m.created_at < PRUNE_GRACE {
        return false;
    }
    file_not_found(&m.path) && m.path.parent().is_some_and(|d| d.is_dir())
}

fn file_not_found(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Whether `dir` is known to hold at least one entry. An empty directory
/// (or one that cannot be read) may be the mount point of a share that is
/// offline, so nothing under it is pruned.
fn has_entries(dir: &Path) -> bool {
    std::fs::read_dir(dir).map(|mut d| d.next().is_some()).unwrap_or(false)
}

pub struct History {
    path: PathBuf,
    lock: Mutex<()>,
}

/// Result of parsing the history file.
struct Parsed {
    /// Live items, oldest first.
    items: Vec<MediaItem>,
    /// Non-empty lines in the file (records, tombstones and unparseable lines).
    lines: usize,
}

/// Exclusive cross-process lock on `<history>.lock`; released on drop.
struct FileLock(File);

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl History {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let h = Self { path, lock: Mutex::new(()) };
        if let Err(e) = h.compact_if_needed() {
            tracing::warn!("history compaction skipped: {e:#}");
        }
        Ok(h)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock_path(&self) -> PathBuf {
        let mut name = self.path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".lock");
        self.path.with_file_name(name)
    }

    fn lock_file(&self) -> Result<File> {
        let lp = self.lock_path();
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lp)
            .with_context(|| format!("opening {}", lp.display()))
    }

    /// Block until this process holds the cross-process writer lock.
    fn lock_exclusive(&self) -> Result<FileLock> {
        let f = self.lock_file()?;
        f.lock().context("locking history")?;
        Ok(FileLock(f))
    }

    /// Append one line (which must end in `\n`) with a single write while
    /// holding both the in-process and the cross-process lock.
    fn append(&self, line: &[u8]) -> Result<()> {
        let _g = self.lock.lock().unwrap();
        let _fl = self.lock_exclusive()?;
        let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
        f.write_all(line)?;
        Ok(())
    }

    pub fn push(&self, media: &MediaItem) -> Result<()> {
        let mut buf = serde_json::to_vec(media)?;
        buf.push(b'\n');
        self.append(&buf)
    }

    /// Re-record an item whose path or metadata changed. The newest line
    /// for an id wins while the item keeps its original position.
    pub fn update(&self, media: &MediaItem) -> Result<()> {
        self.push(media)
    }

    /// All items, newest last. Tombstones (deleted items) are filtered out.
    pub fn all(&self) -> Result<Vec<MediaItem>> {
        let _g = self.lock.lock().unwrap();
        Ok(self.read_unlocked()?.items)
    }

    /// Linear-time parse: an id → slot index makes replacements and
    /// tombstones O(1) each.
    fn read_unlocked(&self) -> Result<Parsed> {
        let f = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Parsed { items: vec![], lines: 0 }),
            Err(e) => return Err(e.into()),
        };
        let mut slots: Vec<Option<MediaItem>> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut lines = 0;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            lines += 1;
            if let Some(id) = line.strip_prefix("DELETE ") {
                if let Some(i) = index.remove(id.trim()) {
                    slots[i] = None;
                }
                continue;
            }
            match serde_json::from_str::<MediaItem>(&line) {
                Ok(m) => match index.get(&m.id) {
                    Some(&i) => slots[i] = Some(m),
                    None => {
                        index.insert(m.id.clone(), slots.len());
                        slots.push(Some(m));
                    }
                },
                Err(e) => tracing::warn!("skipping bad history line: {e}"),
            }
        }
        Ok(Parsed { items: slots.into_iter().flatten().collect(), lines })
    }

    /// Rewrite the file with only the live items when it has more dead lines
    /// than live ones.
    ///
    /// Safety against concurrent writers: every append in this module holds
    /// the exclusive lock on `<file>.lock` (a separate file, so it is never
    /// replaced by the rename below). Compaction takes the same lock with a
    /// non-blocking attempt, reads, writes a temp file in the same directory
    /// and renames it over the original before releasing the lock, so no
    /// append can land on the old file after it was read. If the lock is
    /// busy, compaction is skipped rather than risk losing a line. Writers
    /// that do not take the lock (older builds) are not protected.
    /// Unparseable lines are dropped; reads already ignore them.
    fn compact_if_needed(&self) -> Result<bool> {
        let _g = self.lock.lock().unwrap();
        // Cheap check first, without touching the lock file.
        let parsed = self.read_unlocked()?;
        if parsed.lines - parsed.items.len() <= parsed.items.len() {
            return Ok(false);
        }
        let f = self.lock_file()?;
        match f.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let _fl = FileLock(f);
        // Re-read under the lock: another process may have appended.
        let parsed = self.read_unlocked()?;
        if parsed.lines - parsed.items.len() <= parsed.items.len() {
            return Ok(false);
        }
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let tmp = dir.join(format!(
            ".{}.compact-{}",
            self.path.file_name().and_then(|n| n.to_str()).unwrap_or("history"),
            crate::media_item::short_id()
        ));
        let res = (|| -> Result<()> {
            let mut buf = Vec::new();
            for m in &parsed.items {
                serde_json::to_writer(&mut buf, m)?;
                buf.push(b'\n');
            }
            let mut out = File::create(&tmp)?;
            out.write_all(&buf)?;
            out.sync_all()?;
            std::fs::rename(&tmp, &self.path)?;
            Ok(())
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        res.map(|_| true)
    }

    /// The `n` newest items, newest first, leaving out entries whose file
    /// [`looks_deleted`]. Nothing is written: hidden entries come back when
    /// their file does. Only as many files are checked as it takes to find
    /// `n` shown ones.
    pub fn recent(&self, n: usize) -> Result<Vec<MediaItem>> {
        let now = chrono::Utc::now();
        Ok(self.all()?.into_iter().rev().filter(|m| !looks_deleted(m, now)).take(n).collect())
    }

    /// Tombstone the entries whose file was deleted, for `bugle history
    /// prune`; files are never touched. On top of [`looks_deleted`], an entry
    /// is kept when its parent directory is empty (possibly the mount point
    /// of an offline share) and when it is the `parent` of a subtitle entry
    /// that stays (a sidecar sync finds its video that way). With `dry_run`
    /// nothing is written. Returns the entries removed (or that would be).
    ///
    /// The files are checked without the writer lock; under it only the
    /// candidates are re-checked: each must still have the record that was
    /// checked (another process may have recorded a new path meanwhile) and
    /// still be missing.
    pub fn prune_missing(&self, dry_run: bool) -> Result<Vec<MediaItem>> {
        let candidates = self.prune_candidates()?;
        if candidates.is_empty() || dry_run {
            return Ok(candidates);
        }
        self.tombstone_unchanged(candidates)
    }

    /// The unlocked half of [`History::prune_missing`].
    fn prune_candidates(&self) -> Result<Vec<MediaItem>> {
        let now = chrono::Utc::now();
        let items = {
            let _g = self.lock.lock().unwrap();
            self.read_unlocked()?.items
        };
        let mut dir_ok: HashMap<PathBuf, bool> = HashMap::new();
        let mut candidates: Vec<MediaItem> = items
            .iter()
            .filter(|m| {
                looks_deleted(m, now)
                    && m.path.parent().is_some_and(|d| *dir_ok.entry(d.to_path_buf()).or_insert_with(|| has_entries(d)))
            })
            .cloned()
            .collect();
        let candidate_ids: std::collections::HashSet<&str> = candidates.iter().map(|m| m.id.as_str()).collect();
        let needed: std::collections::HashSet<String> = items
            .iter()
            .filter(|m| m.kind == crate::media_item::MediaKind::Subtitle && !candidate_ids.contains(m.id.as_str()))
            .filter_map(|m| m.parent.clone())
            .collect();
        candidates.retain(|m| !needed.contains(&m.id));
        Ok(candidates)
    }

    /// The locked half of [`History::prune_missing`]: tombstone those of
    /// `candidates` whose record is unchanged and whose file is still gone.
    fn tombstone_unchanged(&self, mut candidates: Vec<MediaItem>) -> Result<Vec<MediaItem>> {
        let _g = self.lock.lock().unwrap();
        let _fl = self.lock_exclusive()?;
        let current: HashMap<String, MediaItem> = self.read_unlocked()?.items.into_iter().map(|m| (m.id.clone(), m)).collect();
        candidates.retain(|m| {
            current.get(&m.id).is_some_and(|c| c.path == m.path && c.url == m.url && c.created_at == m.created_at)
                && file_not_found(&m.path)
        });
        if candidates.is_empty() {
            return Ok(candidates);
        }
        let buf: String = candidates.iter().map(|m| format!("DELETE {}\n", m.id)).collect();
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        f.write_all(buf.as_bytes())?;
        Ok(candidates)
    }

    /// Look up an item. An exact id wins; otherwise `id` must be a prefix of
    /// at least [`MIN_PREFIX_LEN`] characters matching exactly one live item.
    /// Empty or too-short ids and ambiguous prefixes are `BugleError::Invalid`.
    pub fn get(&self, id: &str) -> Result<Option<MediaItem>> {
        let id = id.trim();
        if id.is_empty() {
            return Err(BugleError::Invalid("empty media id".into()).into());
        }
        let all = self.all()?;
        if let Some(m) = all.iter().find(|m| m.id == id) {
            return Ok(Some(m.clone()));
        }
        if id.chars().count() < MIN_PREFIX_LEN {
            return Err(BugleError::Invalid(format!(
                "media id prefix '{id}' is too short; give at least {MIN_PREFIX_LEN} characters"
            ))
            .into());
        }
        let mut matches: Vec<MediaItem> = all.into_iter().filter(|m| m.id.starts_with(id)).collect();
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            _ => {
                let ids: Vec<&str> = matches.iter().map(|m| m.id.as_str()).collect();
                Err(BugleError::Invalid(format!("media id prefix '{id}' is ambiguous: {}", ids.join(", "))).into())
            }
        }
    }

    pub fn latest(&self) -> Result<Option<MediaItem>> {
        Ok(self.all()?.into_iter().last())
    }

    /// Remove from history and optionally delete the file on disk. The id is
    /// resolved like [`History::get`].
    pub fn delete(&self, id: &str, remove_file: bool) -> Result<bool> {
        let item = match self.get(id)? {
            Some(m) => m,
            None => return Ok(false),
        };
        self.append(format!("DELETE {}\n", item.id).as_bytes())?;
        if remove_file && item.path.exists() {
            std::fs::remove_file(&item.path).with_context(|| format!("removing {}", item.path.display()))?;
            // The cursor event track belongs to the file.
            if let Some(events) = item.events.as_ref().filter(|p| p.is_file()) {
                std::fs::remove_file(events).with_context(|| format!("removing {}", events.display()))?;
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_item::MediaKind;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("bugle-hist-{}", uuid::Uuid::new_v4()))
    }

    fn item(id: &str) -> MediaItem {
        let mut m = MediaItem::new(MediaKind::Screenshot, format!("/nonexistent/{id}.png"));
        m.id = id.to_string();
        m
    }

    /// An item recorded at `path` just now (inside the prune grace period).
    fn item_at(id: &str, path: &Path) -> MediaItem {
        let mut m = MediaItem::new(MediaKind::Screenshot, path);
        m.id = id.to_string();
        m
    }

    /// An item recorded at `path` well before the prune grace period.
    fn old_item(id: &str, path: &Path) -> MediaItem {
        let mut m = item_at(id, path);
        m.created_at = chrono::Utc::now() - chrono::Duration::hours(1);
        m
    }

    fn ids(items: &[MediaItem]) -> Vec<&str> {
        items.iter().map(|m| m.id.as_str()).collect()
    }

    fn is_invalid(e: &anyhow::Error) -> bool {
        matches!(e.downcast_ref::<BugleError>(), Some(BugleError::Invalid(_)))
    }

    #[test]
    fn roundtrip_and_delete() {
        let dir = temp_dir();
        let h = History::open(dir.join("history.jsonl")).unwrap();
        let m = MediaItem::new(MediaKind::Screenshot, "/tmp/nope.png");
        h.push(&m).unwrap();
        assert_eq!(h.all().unwrap().len(), 1);
        assert!(h.get(&m.id[..6]).unwrap().is_some());
        let mut moved = m.clone();
        moved.path = "/tmp/moved.png".into();
        moved.url = Some("https://example.invalid/moved.png".into());
        h.push(&MediaItem::new(MediaKind::Gif, "/tmp/later.gif")).unwrap();
        h.update(&moved).unwrap();
        let all = h.all().unwrap();
        assert_eq!(all.len(), 2, "update replaces instead of appending");
        assert_eq!(all[0].path, moved.path, "updated item keeps its position");
        assert_eq!(all[0].url.as_deref(), Some("https://example.invalid/moved.png"));
        assert!(h.delete(&m.id, false).unwrap());
        assert_eq!(h.all().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn get_requires_exact_or_unique_prefix() {
        let dir = temp_dir();
        let h = History::open(dir.join("history.jsonl")).unwrap();
        h.push(&item("abcd11112222")).unwrap();
        h.push(&item("abcd33334444")).unwrap();
        h.push(&item("abc")).unwrap();

        // Empty and too-short ids are rejected, never "newest match".
        assert!(is_invalid(&h.get("").unwrap_err()));
        assert!(is_invalid(&h.get("  ").unwrap_err()));
        assert!(is_invalid(&h.get("a").unwrap_err()));
        assert!(is_invalid(&h.get("ab").unwrap_err()));
        // An exact id wins even when shorter than the prefix minimum.
        assert_eq!(h.get("abc").unwrap().unwrap().id, "abc");
        // Ambiguous prefix lists the candidates.
        let e = h.get("abcd").unwrap_err();
        assert!(is_invalid(&e));
        let msg = e.to_string();
        assert!(msg.contains("abcd11112222") && msg.contains("abcd33334444"), "{msg}");
        // Unique prefix resolves; unknown prefix is None.
        assert_eq!(h.get("abcd1").unwrap().unwrap().id, "abcd11112222");
        assert!(h.get("ffff").unwrap().is_none());

        // delete follows the same rules and never removes the newest item by accident.
        assert!(is_invalid(&h.delete("", true).unwrap_err()));
        assert!(is_invalid(&h.delete("abcd", false).unwrap_err()));
        assert_eq!(h.all().unwrap().len(), 3);
        assert!(h.delete("abcd3", false).unwrap());
        let ids: Vec<String> = h.all().unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["abcd11112222", "abc"]);
        // With the other candidate gone the prefix is unique again.
        assert_eq!(h.get("abcd").unwrap().unwrap().id, "abcd11112222");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn push_writes_one_complete_line() {
        let dir = temp_dir();
        let h = History::open(dir.join("history.jsonl")).unwrap();
        h.push(&item("aaaa00000001")).unwrap();
        h.delete("aaaa00000001", false).unwrap();
        let text = std::fs::read_to_string(h.path()).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(text.lines().count(), 2);
        assert_eq!(text.lines().nth(1), Some("DELETE aaaa00000001"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_pushes_do_not_interleave() {
        let dir = temp_dir();
        let path = dir.join("history.jsonl");
        History::open(&path).unwrap();
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    // Separate instances, like separate processes: only the file lock serialises them.
                    let h = History::open(&path).unwrap();
                    for i in 0..50 {
                        let mut m = item(&format!("t{t:02}i{i:04}xxxx"));
                        m.source = Some("x".repeat(2000));
                        h.push(&m).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for line in text.lines() {
            serde_json::from_str::<MediaItem>(line).expect("every line is a whole record");
        }
        assert_eq!(History::open(&path).unwrap().all().unwrap().len(), 400);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_is_linear_and_order_preserving() {
        let dir = temp_dir();
        let path = dir.join("history.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        let n = 20_000;
        let mut buf = String::new();
        for i in 0..n {
            buf.push_str(&serde_json::to_string(&item(&format!("id{i:010}"))).unwrap());
            buf.push('\n');
        }
        for i in (0..n).step_by(2) {
            buf.push_str(&format!("DELETE id{i:010}\n"));
        }
        std::fs::write(&path, buf).unwrap();
        // Quadratic code takes seconds here; the indexed read is milliseconds.
        let h = History { path: path.clone(), lock: Mutex::new(()) };
        let start = std::time::Instant::now();
        let all = h.all().unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());
        assert_eq!(all.len(), n / 2);
        assert_eq!(all[0].id, "id0000000001");
        assert_eq!(all.last().unwrap().id, format!("id{:010}", n - 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn compacts_on_open_when_mostly_dead() {
        let dir = temp_dir();
        let path = dir.join("history.jsonl");
        let h = History::open(&path).unwrap();
        let keep = item("keep00000001");
        h.push(&keep).unwrap();
        for i in 0..5 {
            let mut m = keep.clone();
            m.label = Some(format!("v{i}"));
            h.update(&m).unwrap();
        }
        let gone = item("gone00000001");
        h.push(&gone).unwrap();
        h.delete(&gone.id, false).unwrap();
        h.push(&item("last00000001")).unwrap();
        // 9 lines, 2 live.
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 9);
        let before = h.all().unwrap();

        let h = History::open(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        let after = h.all().unwrap();
        assert_eq!(after.iter().map(|m| &m.id).collect::<Vec<_>>(), before.iter().map(|m| &m.id).collect::<Vec<_>>());
        assert_eq!(after[0].label.as_deref(), Some("v4"));
        // No temp files left behind; appends still work after the rename.
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(names.iter().all(|n| !n.to_string_lossy().contains("compact")), "{names:?}");
        h.push(&item("new000000001")).unwrap();
        assert_eq!(History::open(&path).unwrap().all().unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn compaction_skipped_while_another_writer_holds_the_lock() {
        let dir = temp_dir();
        let path = dir.join("history.jsonl");
        let h = History::open(&path).unwrap();
        let m = item("busy00000001");
        h.push(&m).unwrap();
        for _ in 0..4 {
            h.update(&m).unwrap();
        }
        let held = h.lock_exclusive().unwrap();
        History::open(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 5, "not compacted while locked");
        drop(held);
        History::open(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn healthy_file_is_left_alone() {
        let dir = temp_dir();
        let path = dir.join("history.jsonl");
        let h = History::open(&path).unwrap();
        h.push(&item("aaaa00000001")).unwrap();
        h.push(&item("bbbb00000001")).unwrap();
        h.update(&item("aaaa00000001")).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        History::open(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recent_hides_missing_files_without_writing() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        let h = History::open(&path).unwrap();
        for i in 0..3 {
            let p = dir.join(format!("live{i}.png"));
            std::fs::write(&p, b"x").unwrap();
            h.push(&old_item(&format!("live0000000{i}"), &p)).unwrap();
            h.push(&old_item(&format!("gone0000000{i}"), &dir.join(format!("gone{i}.png")))).unwrap();
        }
        let before = std::fs::read_to_string(&path).unwrap();
        assert_eq!(ids(&h.recent(2).unwrap()), ["live00000002", "live00000001"]);
        assert_eq!(h.recent(10).unwrap().len(), 3);
        // Opening does not prune either.
        History::open(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "listing and opening never write");
        assert_eq!(h.all().unwrap().len(), 6);
        // Put back from the Trash: the entry shows again, metadata intact.
        std::fs::write(dir.join("gone2.png"), b"x").unwrap();
        assert_eq!(ids(&h.recent(1).unwrap()), ["gone00000002"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_removes_deleted_files_and_keeps_the_rest() {
        let dir = temp_dir();
        let media = dir.join("media");
        std::fs::create_dir_all(&media).unwrap();
        let path = dir.join("history.jsonl");
        let h = History::open(&path).unwrap();
        let live = media.join("live.png");
        std::fs::write(&live, b"x").unwrap();
        h.push(&old_item("live00000001", &live)).unwrap();
        h.push(&old_item("gone00000001", &media.join("gone.png"))).unwrap();
        // Parent directory missing: an unmounted volume looks like this.
        h.push(&old_item("vol000000001", &dir.join("Volumes/Backup/shot.png"))).unwrap();
        // Parent directory empty: the mount point of an offline share.
        std::fs::create_dir_all(dir.join("nas")).unwrap();
        h.push(&old_item("nas000000001", &dir.join("nas/shot.png"))).unwrap();
        // Synced copy: the entry still leads somewhere.
        let mut synced = old_item("sync00000001", &media.join("uploaded.png"));
        synced.url = Some("https://example.invalid/uploaded.png".into());
        h.push(&synced).unwrap();
        // Relative path: nothing to check it against.
        h.push(&old_item("rel000000001", Path::new("relative.png"))).unwrap();
        // Just recorded: a save or a sync move may be in flight.
        h.push(&item_at("new000000001", &media.join("pending.png"))).unwrap();
        // A directory where the file was is not "gone".
        std::fs::create_dir_all(media.join("dir.png")).unwrap();
        h.push(&old_item("dir000000001", &media.join("dir.png"))).unwrap();
        // A broken symlink still exists.
        std::os::unix::fs::symlink(media.join("nowhere.png"), media.join("link.png")).unwrap();
        h.push(&old_item("link00000001", &media.join("link.png"))).unwrap();
        // A video whose subtitle stays is kept; one whose subtitle goes too is not.
        h.push(&old_item("vid000000001", &media.join("clip.mp4"))).unwrap();
        let vtt = media.join("clip.vtt");
        std::fs::write(&vtt, b"WEBVTT").unwrap();
        let mut sub = old_item("sub000000001", &vtt);
        sub.kind = MediaKind::Subtitle;
        sub.parent = Some("vid000000001".into());
        h.push(&sub).unwrap();
        h.push(&old_item("vid000000002", &media.join("other.mp4"))).unwrap();
        let mut sub = old_item("sub000000002", &media.join("other.vtt"));
        sub.kind = MediaKind::Subtitle;
        sub.parent = Some("vid000000002".into());
        h.push(&sub).unwrap();

        let before = std::fs::read_to_string(&path).unwrap();
        let expected = ["gone00000001", "vid000000002", "sub000000002"];
        assert_eq!(ids(&h.prune_missing(true).unwrap()), expected);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "dry run writes nothing");
        assert_eq!(ids(&h.prune_missing(false).unwrap()), expected);
        let kept: Vec<String> = h.all().unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(
            kept,
            [
                "live00000001", "vol000000001", "nas000000001", "sync00000001", "rel000000001", "new000000001",
                "dir000000001", "link00000001", "vid000000001", "sub000000001"
            ]
        );
        // Idempotent, and no file was touched.
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(h.prune_missing(false).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(live.is_file() && vtt.is_file() && media.join("dir.png").is_dir());
        assert!(media.join("link.png").symlink_metadata().is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_keeps_entries_it_cannot_check() {
        let dir = temp_dir();
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("other.png"), b"x").unwrap();
        let h = History::open(dir.join("history.jsonl")).unwrap();
        h.push(&old_item("perm00000001", &locked.join("shot.png"))).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = std::fs::symlink_metadata(locked.join("shot.png")).is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
        let pruned = h.prune_missing(false).unwrap();
        let shown = h.recent(5).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if denied {
            assert!(pruned.is_empty(), "PermissionDenied is not NotFound");
            assert_eq!(ids(&shown), ["perm00000001"]);
        } // else running as root: the directory is still readable.
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_rechecks_candidates_under_the_lock() {
        // Between the unlocked check and the tombstone write another process
        // recorded a new path (a sync move), and a file was put back.
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        let h = History::open(&path).unwrap();
        let moved = old_item("move00000001", &dir.join("before.png"));
        h.push(&moved).unwrap();
        h.push(&old_item("back00000001", &dir.join("back.png"))).unwrap();
        h.push(&old_item("gone00000001", &dir.join("gone.png"))).unwrap();
        std::fs::write(dir.join("unrelated.png"), b"x").unwrap();
        let candidates = h.prune_candidates().unwrap();
        assert_eq!(ids(&candidates), ["move00000001", "back00000001", "gone00000001"]);
        let mut after = moved.clone();
        after.path = dir.join("synced/after.png");
        History::open(&path).unwrap().update(&after).unwrap();
        std::fs::write(dir.join("back.png"), b"x").unwrap();
        assert_eq!(ids(&h.tombstone_unchanged(candidates).unwrap()), ["gone00000001"]);
        assert_eq!(ids(&h.all().unwrap()), ["move00000001", "back00000001"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_prunes_write_one_tombstone() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("keep.png"), b"x").unwrap();
        let path = dir.join("history.jsonl");
        let h = History::open(&path).unwrap();
        h.push(&old_item("gone00000001", &dir.join("gone.png"))).unwrap();
        // Opened up front, like separate running processes: an `open` after
        // the tombstone would compact it away.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| History::open(&path).unwrap())
            .map(|other| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    other.prune_missing(false).unwrap().len()
                })
            })
            .collect();
        let total: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(total, 1);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("DELETE gone00000001").count(), 1, "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
