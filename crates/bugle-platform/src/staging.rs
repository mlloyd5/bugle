//! Leftover staging folders.
//!
//! Every shim writer writes into a new hidden folder next to its output,
//! `.bugle-export.XXXXXX` (`mkdtemp`), and renames the finished file out of
//! it (see "Staged output" in `shim/shim.m`). A process killed mid-write
//! leaves that folder behind; [`sweep`] removes such leftovers.
//!
//! The shim holds an exclusive `flock(2)` on each folder, on a descriptor of
//! its own, from just after creating it until it is removed. The kernel drops
//! the lock when the process exits, however it exits, and a flock belongs to
//! the open file description, so a folder being written by another export,
//! in this process or any other Bugle process, cannot be locked here. A
//! folder is removed only when all of these hold:
//!
//! - its name is exactly the pattern (`.bugle-export.` and six ASCII letters
//!   or digits) and it sits directly in the folder being swept;
//! - it is a directory itself, not a symlink to one (it is opened with
//!   `O_NOFOLLOW`, and removal never follows a symlink inside it);
//! - its lock can be taken (and is held while it is removed);
//! - neither it nor anything directly in it was modified for [`STALE_AFTER`],
//!   which covers the moment between `mkdtemp` and the lock, and a writer
//!   from a build that did not lock.
//!
//! On a volume without flock support nothing can be proved live, so a
//! folder there must be untouched for [`NO_LOCK_STALE_AFTER`] instead. Any
//! other failure keeps the folder. Sweeping never fails: what it could not
//! do is logged at debug level.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// Start of a staging folder's name; `mkdtemp` adds six letters or digits.
pub const PREFIX: &str = ".bugle-export.";
const RANDOM_LEN: usize = 6;

/// How long an unlocked staging folder must have been untouched before it
/// is removed.
pub const STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// The same, on a volume that does not support locks.
pub const NO_LOCK_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// [`sweep_beside`] looks at a folder at most this often per process: a
/// folder only becomes stale after [`STALE_AFTER`] anyway.
const RESWEEP_AFTER: Duration = Duration::from_secs(10 * 60);

/// Whether `name` is a staging folder's name, exactly.
pub fn is_staging_name(name: &OsStr) -> bool {
    let b = name.as_bytes();
    b.len() == PREFIX.len() + RANDOM_LEN && b.starts_with(PREFIX.as_bytes()) && b[PREFIX.len()..].iter().all(u8::is_ascii_alphanumeric)
}

/// Remove the stale staging folders directly in `dir` (see the module
/// docs). Returns how many were removed.
pub fn sweep(dir: &Path) -> usize {
    sweep_with(dir, STALE_AFTER, NO_LOCK_STALE_AFTER)
}

/// [`sweep`] the folder `output` would be staged in, unless this process
/// swept it in the last few minutes. Called before every staged write.
pub fn sweep_beside(output: &Path) {
    let dir = match output.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    sweep_throttled(dir);
}

/// [`sweep`] each of `dirs` on a thread of its own (for a long-lived
/// process at start-up), so start-up never waits on a slow volume.
pub fn sweep_in_background(dirs: Vec<PathBuf>) {
    let spawned = std::thread::Builder::new().name("bugle-staging-sweep".into()).spawn(move || {
        for d in &dirs {
            sweep_throttled(d);
        }
    });
    if let Err(e) = spawned {
        tracing::debug!("not sweeping staging folders: {e}");
    }
}

fn sweep_throttled(dir: &Path) {
    static LAST: Mutex<Option<HashMap<PathBuf, Instant>>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let last = last.get_or_insert_with(HashMap::new);
        let now = Instant::now();
        if last.get(dir).is_some_and(|t| now.duration_since(*t) < RESWEEP_AFTER) {
            return;
        }
        last.insert(dir.to_path_buf(), now);
    }
    sweep(dir);
}

pub(crate) fn sweep_with(dir: &Path, stale_after: Duration, no_lock_stale_after: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !is_staging_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        match remove_if_stale(&path, stale_after, no_lock_stale_after) {
            Ok(true) => {
                tracing::info!("removed the leftover staging folder {}", path.display());
                removed += 1;
            }
            Ok(false) => {}
            Err(e) => tracing::debug!("keeping the staging folder {}: {e}", path.display()),
        }
    }
    removed
}

/// Remove the staging folder at `path` if it is stale. `Ok(false)` when it
/// is live, recent, or not a directory.
fn remove_if_stale(path: &Path, stale_after: Duration, no_lock_stale_after: Duration) -> std::io::Result<bool> {
    // A symlink (or a file) with the name is not ours.
    if !std::fs::symlink_metadata(path)?.file_type().is_dir() {
        return Ok(false);
    }
    // O_NOFOLLOW: should it have become a symlink since, the open fails.
    let folder = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(path)?;
    let idle = match folder.try_lock() {
        Ok(()) => stale_after,
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(e)) if matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::EOPNOTSUPP)) => no_lock_stale_after,
        Err(TryLockError::Error(e)) => return Err(e),
    };
    if !untouched_for(&folder, path, idle)? {
        return Ok(false);
    }
    // Does not follow symlinks, at the top or inside. The lock is held
    // (`folder` is open) until the folder is gone.
    std::fs::remove_dir_all(path)?;
    Ok(true)
}

/// Whether neither the folder nor anything directly in it was modified in
/// the last `idle` (a writer appending to its file updates that file only).
/// A time in the future counts as recent.
fn untouched_for(folder: &File, path: &Path, idle: Duration) -> std::io::Result<bool> {
    let mut newest = folder.metadata()?.modified()?;
    for entry in std::fs::read_dir(path)? {
        // Not followed: a symlink counts by its own time.
        if let Ok(t) = entry?.metadata().and_then(|m| m.modified()) {
            newest = newest.max(t);
        }
    }
    Ok(SystemTime::now().duration_since(newest).is_ok_and(|age| age >= idle))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT_ONCE: Duration = Duration::ZERO;

    fn staging(dir: &Path, random: &str) -> PathBuf {
        let p = dir.join(format!("{PREFIX}{random}"));
        std::fs::create_dir(&p).unwrap();
        std::fs::write(p.join("clip.mp4"), b"partial").unwrap();
        p
    }

    #[test]
    fn names() {
        assert!(is_staging_name(OsStr::new(".bugle-export.aZ09xY")));
        for n in [".bugle-export.aZ09x", ".bugle-export.aZ09xYz", ".bugle-export.aZ-9xY", "bugle-export.aZ09xY", ".bugle-export.", ".bugle-edit-aZ09xY", ".Bugle-export.aZ09xY"] {
            assert!(!is_staging_name(OsStr::new(n)), "{n}");
        }
    }

    #[test]
    fn a_stale_folder_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let p = staging(tmp.path(), "AbC123");
        std::fs::create_dir(p.join("nested")).unwrap();
        assert_eq!(sweep_with(tmp.path(), AT_ONCE, AT_ONCE), 1);
        assert!(!p.exists());
    }

    #[test]
    fn a_recent_folder_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let p = staging(tmp.path(), "AbC123");
        assert_eq!(sweep(tmp.path()), 0);
        assert!(p.join("clip.mp4").exists());
    }

    #[test]
    fn a_locked_folder_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let p = staging(tmp.path(), "AbC123");
        // What the shim does: its own descriptor, an exclusive flock. Ours
        // conflicts with it even within one process.
        let held = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY).open(&p).unwrap();
        held.try_lock().unwrap();
        assert_eq!(sweep_with(tmp.path(), AT_ONCE, AT_ONCE), 0);
        assert!(p.join("clip.mp4").exists());
        // Released (as by the process exiting): now it goes.
        drop(held);
        assert_eq!(sweep_with(tmp.path(), AT_ONCE, AT_ONCE), 1);
        assert!(!p.exists());
    }

    #[test]
    fn look_alikes_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let mut kept = Vec::new();
        for name in [".bugle-export.AbC12", ".bugle-export.AbC1234", ".bugle-export.AbC_12", "bugle-export.AbC123", ".bugle-edit-AbC123"] {
            let p = tmp.path().join(name);
            std::fs::create_dir(&p).unwrap();
            std::fs::write(p.join("mine.txt"), b"user").unwrap();
            kept.push(p);
        }
        // The right name on a plain file is not a folder of ours either.
        let file = tmp.path().join(".bugle-export.File99");
        std::fs::write(&file, b"user").unwrap();
        kept.push(file);
        // Nor is a folder of that name one level down.
        let deeper = tmp.path().join("photos");
        std::fs::create_dir(&deeper).unwrap();
        kept.push(staging(&deeper, "Deep01"));
        assert_eq!(sweep_with(tmp.path(), AT_ONCE, AT_ONCE), 0);
        for p in &kept {
            assert!(p.exists(), "{}", p.display());
        }
    }

    #[test]
    fn symlinks_are_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let swept = tmp.path().join("save");
        std::fs::create_dir(&swept).unwrap();
        let user = tmp.path().join("user");
        std::fs::create_dir(&user).unwrap();
        std::fs::write(user.join("keep.txt"), b"user").unwrap();
        // A symlink named like a staging folder, pointing at user content.
        let link = swept.join(".bugle-export.Link01");
        std::os::unix::fs::symlink(&user, &link).unwrap();
        // A real (stale) staging folder holding a symlink to user content.
        let real = staging(&swept, "Real01");
        std::os::unix::fs::symlink(&user, real.join("inside")).unwrap();
        assert_eq!(sweep_with(&swept, AT_ONCE, AT_ONCE), 1);
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link itself is kept");
        assert!(!real.exists());
        assert!(user.join("keep.txt").exists(), "nothing behind a link is removed");
    }

    #[test]
    fn sweeping_a_missing_folder_does_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(sweep_with(&tmp.path().join("missing"), AT_ONCE, AT_ONCE), 0);
        sweep_beside(&tmp.path().join("missing").join("out.mp4"));
    }
}
