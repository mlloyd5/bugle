//! Append-only JSON-lines history store: a flat file is enough and is
//! trivially inspectable by agents.

use crate::media_item::MediaItem;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct History {
    path: PathBuf,
    lock: Mutex<()>,
}

impl History {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self { path, lock: Mutex::new(()) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn push(&self, media: &MediaItem) -> Result<()> {
        let _g = self.lock.lock().unwrap();
        let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
        serde_json::to_writer(&mut f, media)?;
        f.write_all(b"\n")?;
        Ok(())
    }

    /// All items, newest last. Tombstones (deleted items) are filtered out.
    pub fn all(&self) -> Result<Vec<MediaItem>> {
        let _g = self.lock.lock().unwrap();
        self.read_unlocked()
    }

    fn read_unlocked(&self) -> Result<Vec<MediaItem>> {
        let f = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut items: Vec<MediaItem> = Vec::new();
        let mut deleted: Vec<String> = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(id) = line.strip_prefix("DELETE ") {
                deleted.push(id.trim().to_string());
                continue;
            }
            match serde_json::from_str::<MediaItem>(&line) {
                Ok(m) => items.push(m),
                Err(e) => tracing::warn!("skipping bad history line: {e}"),
            }
        }
        items.retain(|m| !deleted.contains(&m.id));
        Ok(items)
    }

    pub fn recent(&self, n: usize) -> Result<Vec<MediaItem>> {
        let mut all = self.all()?;
        all.reverse();
        all.truncate(n);
        Ok(all)
    }

    pub fn get(&self, id: &str) -> Result<Option<MediaItem>> {
        Ok(self.all()?.into_iter().rev().find(|m| m.id == id || m.id.starts_with(id)))
    }

    pub fn latest(&self) -> Result<Option<MediaItem>> {
        Ok(self.all()?.into_iter().last())
    }

    /// Remove from history and optionally delete the file on disk.
    pub fn delete(&self, id: &str, remove_file: bool) -> Result<bool> {
        let item = match self.get(id)? {
            Some(m) => m,
            None => return Ok(false),
        };
        {
            let _g = self.lock.lock().unwrap();
            let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
            writeln!(f, "DELETE {}", item.id)?;
        }
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

    #[test]
    fn roundtrip_and_delete() {
        let dir = std::env::temp_dir().join(format!("bugle-hist-{}", uuid::Uuid::new_v4()));
        let h = History::open(dir.join("history.jsonl")).unwrap();
        let m = MediaItem::new(MediaKind::Screenshot, "/tmp/nope.png");
        h.push(&m).unwrap();
        assert_eq!(h.all().unwrap().len(), 1);
        assert!(h.get(&m.id[..6]).unwrap().is_some());
        assert!(h.delete(&m.id, false).unwrap());
        assert!(h.all().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
