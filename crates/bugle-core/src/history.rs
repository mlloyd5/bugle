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

    pub fn recent(&self, n: usize) -> Result<Vec<MediaItem>> {
        let mut all = self.all()?;
        all.reverse();
        all.truncate(n);
        Ok(all)
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
}
