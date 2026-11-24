//! Notices, off the main thread, that the Recent list on show went stale
//! while the history file stayed the same: a file was deleted (in Finder,
//! say), or a file appeared or disappeared so its row's enabled state is
//! wrong (an uploaded item deleted locally, a drive unplugged or plugged
//! back in). The menu is then rebuilt on the next tick.
//!
//! A `stat` on an external or network volume can block for a long time (a
//! drive spinning up, an unreachable share), so the files are never checked
//! on the main thread. A background thread is given the rows on show and
//! checks each file every [`INTERVAL`] on a short-lived thread of its own,
//! never starting a second check of a path while one is still running: a
//! hung volume delays only the checks of its own files, and the others keep
//! being noticed. The result is a flag the main thread reads for free.
//!
//! A row is stale when its file [`looks_deleted`] (the test `History::recent`
//! hides entries by, so a rebuild drops it) or when whether it exists differs
//! from the enabled state it was shown with. A drive that was unplugged
//! together with its mount point leaves its files' folders missing, which
//! `looks_deleted` does not count, so they stay listed, disabled.

use bugle_core::history::looks_deleted;
use bugle_core::MediaItem;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

/// How often the files on show are checked.
pub const INTERVAL: Duration = Duration::from_secs(2);

/// A row on show: the item, and whether its file existed (the row's
/// enabled state) when the menu was built.
pub type Row = (MediaItem, bool);

/// Whether `row` no longer matches its file. Stats the file.
type Check = Arc<dyn Fn(&Row) -> bool + Send + Sync>;

fn stale(row: &Row) -> bool {
    looks_deleted(&row.0, chrono::Utc::now()) || row.0.path.exists() != row.1
}

enum Msg {
    Watch(u64, Vec<Row>),
    Checked { generation: u64, path: PathBuf, stale: bool },
    Stop,
}

pub struct RecentProbe {
    tx: Sender<Msg>,
    /// The generation of the rows in which a check found a stale one
    /// (0: none yet).
    stale: Arc<AtomicU64>,
    /// The generation of the rows on show.
    generation: u64,
}

impl RecentProbe {
    /// Start the checking thread. Without one (the spawn failed) nothing is
    /// ever reported, so the list changes only with the history file.
    pub fn start(interval: Duration) -> Self {
        Self::start_with(interval, Arc::new(stale))
    }

    fn start_with(interval: Duration, check: Check) -> Self {
        let (tx, rx) = channel::<Msg>();
        let stale = Arc::new(AtomicU64::new(0));
        let (found, results) = (stale.clone(), tx.clone());
        let spawned = std::thread::Builder::new().name("bugle-recent-probe".into()).spawn(move || {
            let mut watched: Option<(u64, Vec<Row>)> = None;
            let mut in_flight: HashSet<PathBuf> = HashSet::new();
            loop {
                // Nothing to check: sleep until there is.
                let msg = if watched.as_ref().is_some_and(|(_, rows)| !rows.is_empty()) {
                    match rx.recv_timeout(interval) {
                        Ok(m) => Some(m),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                } else {
                    Some(rx.recv().unwrap_or(Msg::Stop))
                };
                match msg {
                    Some(Msg::Stop) => return,
                    Some(Msg::Watch(generation, rows)) => watched = Some((generation, rows)),
                    Some(Msg::Checked { generation, path, stale }) => {
                        in_flight.remove(&path);
                        if stale && watched.as_ref().is_some_and(|(g, _)| *g == generation) {
                            found.store(generation, Ordering::SeqCst);
                            // Reported once; the rebuild sends the next rows.
                            watched = None;
                        }
                        continue;
                    }
                    None => {}
                }
                let Some((generation, rows)) = &watched else { continue };
                for row in rows {
                    if !in_flight.insert(row.0.path.clone()) {
                        continue; // still checking it (a hung volume)
                    }
                    let (row, generation, check, results) = (row.clone(), *generation, check.clone(), results.clone());
                    let path = row.0.path.clone();
                    let spawned = std::thread::Builder::new().name("bugle-recent-stat".into()).spawn(move || {
                        let stale = check(&row);
                        let _ = results.send(Msg::Checked { generation, path: row.0.path, stale });
                    });
                    if spawned.is_err() {
                        in_flight.remove(&path);
                    }
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("could not start the thread checking the Recent files: {e}");
        }
        Self { tx, stale, generation: 0 }
    }

    /// The rows now on show replace the ones checked so far.
    pub fn watch(&mut self, rows: Vec<Row>) {
        self.generation += 1;
        let _ = self.tx.send(Msg::Watch(self.generation, rows));
    }

    /// Whether a row on show was found stale. Never touches the filesystem.
    pub fn changed(&self) -> bool {
        self.generation != 0 && self.stale.load(Ordering::SeqCst) == self.generation
    }
}

impl Drop for RecentProbe {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::MediaKind;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    const FAST: Duration = Duration::from_millis(10);

    fn old_item(path: &Path) -> MediaItem {
        let mut m = MediaItem::new(MediaKind::Screenshot, path);
        m.created_at = chrono::Utc::now() - chrono::Duration::hours(1);
        m
    }

    /// The row the menu builds for `m`.
    fn row(m: MediaItem) -> Row {
        let exists = m.path.exists();
        (m, exists)
    }

    /// Wait up to a few seconds (the machine may be busy) for `changed`.
    fn reports(probe: &RecentProbe) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if probe.changed() {
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
        assert!(!probe.changed(), "nothing on show yet");
        probe.watch(vec![row(old_item(&shot)), row(old_item(&other))]);
        std::thread::sleep(FAST * 10);
        assert!(!probe.changed());
        std::fs::remove_file(&shot).unwrap();
        assert!(reports(&probe));
        // The rebuilt list (without it) starts clean.
        probe.watch(vec![row(old_item(&other))]);
        assert!(!probe.changed());
        std::thread::sleep(FAST * 10);
        assert!(!probe.changed());
        std::fs::remove_file(&other).unwrap();
        assert!(reports(&probe));
    }

    #[test]
    fn an_unplugged_drive_disables_its_rows_without_hiding_them() {
        let tmp = tempfile::tempdir().unwrap();
        // A file at the root of a volume and one in a folder on it: both
        // parents go away with the volume's mount point.
        let volume = tmp.path().join("Volumes/Backup");
        std::fs::create_dir_all(volume.join("Shots")).unwrap();
        let at_root = old_item(&volume.join("shot.png"));
        let in_folder = old_item(&volume.join("Shots/clip.mp4"));
        std::fs::write(&at_root.path, b"x").unwrap();
        std::fs::write(&in_folder.path, b"x").unwrap();
        let mut probe = RecentProbe::start(FAST);
        probe.watch(vec![row(at_root.clone()), row(in_folder.clone())]);
        std::fs::remove_dir_all(&volume).unwrap();
        // Reported so the rows show disabled, but History::recent keeps them.
        assert!(reports(&probe));
        let now = chrono::Utc::now();
        assert!(!looks_deleted(&at_root, now) && !looks_deleted(&in_folder, now));
        // Shown disabled: nothing more to report until the drive is back.
        probe.watch(vec![row(at_root.clone()), row(in_folder)]);
        std::thread::sleep(FAST * 30);
        assert!(!probe.changed());
        std::fs::create_dir_all(&volume).unwrap();
        std::fs::write(&at_root.path, b"x").unwrap();
        assert!(reports(&probe));
    }

    #[test]
    fn an_uploaded_item_deleted_locally_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = old_item(&tmp.path().join("shot.png"));
        m.url = Some("https://example.invalid/shot.png".into());
        std::fs::write(&m.path, b"x").unwrap();
        let mut probe = RecentProbe::start(FAST);
        probe.watch(vec![row(m.clone())]);
        std::fs::remove_file(&m.path).unwrap();
        assert!(!looks_deleted(&m, chrono::Utc::now()), "still listed: it has a synced copy");
        assert!(reports(&probe));
        // Just saved and not there yet: shown disabled, nothing to report.
        probe.watch(vec![row(MediaItem::new(MediaKind::Screenshot, tmp.path().join("fresh.png")))]);
        std::thread::sleep(FAST * 30);
        assert!(!probe.changed());
    }

    #[test]
    fn a_hung_volume_delays_only_its_own_files() {
        static RELEASE: AtomicBool = AtomicBool::new(false);
        let tmp = tempfile::tempdir().unwrap();
        let local = old_item(&tmp.path().join("shot.png"));
        std::fs::write(&local.path, b"x").unwrap();
        let share = old_item(Path::new("/Volumes/Share/hung.png"));
        let check: Check = Arc::new(|r: &Row| {
            if r.0.path.starts_with("/Volumes/Share") {
                while !RELEASE.load(Ordering::SeqCst) {
                    std::thread::sleep(FAST);
                }
                return false;
            }
            stale(r)
        });
        let mut probe = RecentProbe::start_with(FAST, check);
        // The hung file first, as the old sequential loop would have met it.
        probe.watch(vec![(share, true), row(local.clone())]);
        std::thread::sleep(FAST * 10);
        std::fs::remove_file(&local.path).unwrap();
        let noticed = reports(&probe);
        RELEASE.store(true, Ordering::SeqCst);
        assert!(noticed);
    }
}
