//! Notices, off the main thread, that a file of the Recent list on show was
//! deleted (in Finder, say) while the history file stayed the same, so the
//! menu can drop it the next time it is rebuilt.
//!
//! A `stat` on an external or network volume can block for a long time (a
//! drive spinning up, an unreachable share), so the files are never checked
//! on the main thread: a background thread checks the items it was last
//! given every [`INTERVAL`] and raises a flag the main thread reads for free.
//! A check that hangs holds up only that thread, and the next list it is
//! given replaces the one it was stuck on.
//!
//! "Deleted" is [`looks_deleted`], the test `History::recent` uses to hide an
//! entry, so a rebuild after the flag really drops the item. In particular a
//! file whose parent directory is missing (a drive that was unplugged) does
//! not count, and neither does one younger than the prune grace period.

use bugle_core::history::looks_deleted;
use bugle_core::MediaItem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

/// How often the files on show are checked.
pub const INTERVAL: Duration = Duration::from_secs(2);

pub struct RecentProbe {
    tx: Sender<(u64, Vec<MediaItem>)>,
    /// The generation of the list in which the thread found a deleted file
    /// (0: none yet).
    gone: Arc<AtomicU64>,
    /// The generation of the list on show.
    generation: u64,
}

impl RecentProbe {
    /// Start the checking thread. Without one (the spawn failed) nothing is
    /// ever reported, so the list changes only with the history file.
    pub fn start(interval: Duration) -> Self {
        let (tx, rx) = channel::<(u64, Vec<MediaItem>)>();
        let gone = Arc::new(AtomicU64::new(0));
        let found = gone.clone();
        let spawned = std::thread::Builder::new().name("bugle-recent-probe".into()).spawn(move || {
            let mut watched: Option<(u64, Vec<MediaItem>)> = None;
            loop {
                match rx.recv_timeout(interval) {
                    Ok(next) => watched = Some(rx.try_iter().last().unwrap_or(next)),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                let Some((generation, items)) = &watched else { continue };
                let now = chrono::Utc::now();
                if items.iter().any(|m| looks_deleted(m, now)) {
                    found.store(*generation, Ordering::SeqCst);
                    // Reported once; the rebuild sends the next list.
                    watched = None;
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("could not start the thread checking the Recent files: {e}");
        }
        Self { tx, gone, generation: 0 }
    }

    /// The items now on show replace the ones checked so far.
    pub fn watch(&mut self, items: Vec<MediaItem>) {
        self.generation += 1;
        let _ = self.tx.send((self.generation, items));
    }

    /// Whether a file of the items on show was found deleted. Never touches
    /// the filesystem.
    pub fn file_gone(&self) -> bool {
        self.generation != 0 && self.gone.load(Ordering::SeqCst) == self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::MediaKind;
    use std::path::Path;
    use std::time::Instant;

    const FAST: Duration = Duration::from_millis(10);

    fn old_item(path: &Path) -> MediaItem {
        let mut m = MediaItem::new(MediaKind::Screenshot, path);
        m.created_at = chrono::Utc::now() - chrono::Duration::hours(1);
        m
    }

    /// Wait up to a few seconds (the machine may be busy) for `file_gone`.
    fn reports(probe: &RecentProbe) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if probe.file_gone() {
                return true;
            }
            std::thread::sleep(FAST);
        }
        false
    }

    #[test]
    fn reports_a_file_deleted_on_any_volume_once_per_list() {
        let tmp = tempfile::tempdir().unwrap();
        let shot = tmp.path().join("shot.png");
        let other = tmp.path().join("other.png");
        std::fs::write(&shot, b"x").unwrap();
        std::fs::write(&other, b"x").unwrap();
        let mut probe = RecentProbe::start(FAST);
        assert!(!probe.file_gone(), "nothing on show yet");
        probe.watch(vec![old_item(&shot), old_item(&other)]);
        std::thread::sleep(FAST * 10);
        assert!(!probe.file_gone());
        std::fs::remove_file(&shot).unwrap();
        assert!(reports(&probe));
        // The rebuilt list (without it) starts clean.
        probe.watch(vec![old_item(&other)]);
        assert!(!probe.file_gone());
        std::thread::sleep(FAST * 10);
        assert!(!probe.file_gone());
        std::fs::remove_file(&other).unwrap();
        assert!(reports(&probe));
    }

    #[test]
    fn an_unplugged_drive_is_not_a_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        // A file at the root of a volume and one in a folder on it: both
        // parents go away with the volume.
        let volume = tmp.path().join("Volumes/Backup");
        std::fs::create_dir_all(volume.join("Shots")).unwrap();
        let at_root = volume.join("shot.png");
        let in_folder = volume.join("Shots/clip.mp4");
        std::fs::write(&at_root, b"x").unwrap();
        std::fs::write(&in_folder, b"x").unwrap();
        // Just saved: a sync may be moving it.
        let fresh = tmp.path().join("fresh.png");
        let mut probe = RecentProbe::start(FAST);
        probe.watch(vec![old_item(&at_root), old_item(&in_folder), MediaItem::new(MediaKind::Screenshot, &fresh)]);
        std::fs::remove_dir_all(&volume).unwrap();
        std::thread::sleep(FAST * 30);
        assert!(!probe.file_gone());
    }
}
