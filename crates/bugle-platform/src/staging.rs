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
//! in this process or any other Bugle process on this Mac, cannot be locked
//! here. A folder is removed only when all of these hold:
//!
//! - its name is exactly the pattern (`.bugle-export.` and six ASCII letters
//!   or digits) and it sits directly in the folder being swept;
//! - it is a directory itself, not a symlink to one (it is opened with
//!   `O_NOFOLLOW`, and removal never follows a symlink inside it);
//! - it is at least [`PROBE_AFTER`] old by its own modification time, checked
//!   before its lock is even tried, so a sweep can never take the lock of a
//!   folder whose writer has run `mkdtemp` but not yet `flock`;
//! - its lock can be taken (and is held while it is removed);
//! - the path still names the folder that was opened and locked (device and
//!   inode), checked before its contents are read and again just before it
//!   is removed;
//! - neither it nor anything directly in it was modified for the idle floor:
//!   [`STALE_AFTER`] on a local volume with working locks, which also covers
//!   a writer from an older build that does not lock (for such a writer the
//!   idle rule is the only protection).
//!
//! A flock only reaches processes on the same Mac, so on a volume that is
//! not local (`MNT_LOCAL` unset: a file share, which another Mac may also
//! write) or that does not support locks, nothing can be proved live, and
//! the floor is [`REMOTE_STALE_AFTER`] instead. A lock call that fails any
//! other way (`ENOLCK`, ...) keeps the folder, as does any other error.
//! Sweeping never fails: what it could not do is logged at debug level.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// Start of a staging folder's name; `mkdtemp` adds six letters or digits.
pub const PREFIX: &str = ".bugle-export.";
const RANDOM_LEN: usize = 6;

/// How old (by its own modification time) a staging folder must be before
/// its lock is even tried.
pub const PROBE_AFTER: Duration = Duration::from_secs(60);

/// How long an unlocked staging folder on a local volume must have been
/// untouched before it is removed.
pub const STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// The same, on a volume that is not local or does not support locks.
pub const REMOTE_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// The age floors a sweep applies (the constants above; tests lower them).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Floors {
    pub probe_after: Duration,
    pub stale_after: Duration,
    pub remote_stale_after: Duration,
}

const FLOORS: Floors = Floors { probe_after: PROBE_AFTER, stale_after: STALE_AFTER, remote_stale_after: REMOTE_STALE_AFTER };

#[cfg(test)]
impl Floors {
    pub(crate) const ZERO: Floors = Floors { probe_after: Duration::ZERO, stale_after: Duration::ZERO, remote_stale_after: Duration::ZERO };
}

/// What the lock probe found on a folder that is not live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lock {
    /// Taken: no process on this Mac holds it.
    Taken,
    /// The volume does not support locks.
    Unsupported,
}

/// How long a folder must be untouched, given its lock and its volume: only
/// a lock taken on a local volume says no writer anywhere holds it.
fn idle_floor(lock: Lock, local: bool, floors: &Floors) -> Duration {
    match (lock, local) {
        (Lock::Taken, true) => floors.stale_after,
        _ => floors.remote_stale_after,
    }
}

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
    sweep_with(dir, &FLOORS)
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

pub(crate) fn sweep_with(dir: &Path, floors: &Floors) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !is_staging_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        match remove_if_stale(&path, floors) {
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
/// is live, recent, not a directory, or no longer the folder first seen.
fn remove_if_stale(path: &Path, floors: &Floors) -> std::io::Result<bool> {
    let seen = std::fs::symlink_metadata(path)?;
    // A symlink (or a file) with the name is not ours.
    if !seen.file_type().is_dir() {
        return Ok(false);
    }
    // A new folder's writer may not have locked it yet: taking the lock now
    // would leave that export unlocked for good. Not even tried.
    if !older_than(seen.modified()?, floors.probe_after) {
        return Ok(false);
    }
    // O_NOFOLLOW: should it have become a symlink since, the open fails.
    let folder = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(path)?;
    if !same_inode(&folder.metadata()?, &seen) {
        return Ok(false);
    }
    let local = on_local_volume(&folder)?;
    let lock = match folder.try_lock() {
        Ok(()) => Lock::Taken,
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(e)) if matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::EOPNOTSUPP)) => Lock::Unsupported,
        // ENOLCK and the like: liveness unknown, so kept.
        Err(TryLockError::Error(e)) => return Err(e),
    };
    if !untouched_for(&folder, path, idle_floor(lock, local, floors))? {
        return Ok(false);
    }
    // Removal goes by path: make sure it still names the locked folder.
    if !still_at(&folder, path)? {
        return Ok(false);
    }
    // Does not follow symlinks, at the top or inside. The lock is held
    // (`folder` is open) until the folder is gone.
    std::fs::remove_dir_all(path)?;
    Ok(true)
}

/// Whether `t` is at least `age` ago (a time in the future is not).
fn older_than(t: SystemTime, age: Duration) -> bool {
    SystemTime::now().duration_since(t).is_ok_and(|a| a >= age)
}

fn same_inode(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Whether `path` still names `folder` itself (not a symlink, not another
/// folder put in its place).
fn still_at(folder: &File, path: &Path) -> std::io::Result<bool> {
    let now = std::fs::symlink_metadata(path)?;
    Ok(now.file_type().is_dir() && same_inode(&folder.metadata()?, &now))
}

/// Whether `folder` is on a local volume (`MNT_LOCAL`).
#[cfg(target_os = "macos")]
fn on_local_volume(folder: &File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(folder.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let st = unsafe { st.assume_init() };
    Ok(st.f_flags & libc::MNT_LOCAL as u32 != 0)
}

/// Elsewhere every volume is treated as shared.
#[cfg(not(target_os = "macos"))]
fn on_local_volume(_folder: &File) -> std::io::Result<bool> {
    Ok(false)
}

/// Whether neither the folder nor anything directly in it was modified in
/// the last `idle` (a writer appending to its file updates that file only).
/// A time in the future counts as recent.
fn untouched_for(folder: &File, path: &Path, idle: Duration) -> std::io::Result<bool> {
    // The entries are read by path: only while it names the locked folder.
    if !still_at(folder, path)? {
        return Ok(false);
    }
    let mut newest = folder.metadata()?.modified()?;
    for entry in std::fs::read_dir(path)? {
        // Not followed: a symlink counts by its own time.
        if let Ok(t) = entry?.metadata().and_then(|m| m.modified()) {
            newest = newest.max(t);
        }
    }
    Ok(older_than(newest, idle))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT_ONCE: &Floors = &Floors::ZERO;

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
        assert_eq!(sweep_with(tmp.path(), AT_ONCE), 1);
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
        assert_eq!(sweep_with(tmp.path(), AT_ONCE), 0);
        assert!(p.join("clip.mp4").exists());
        // Released (as by the process exiting): now it goes.
        drop(held);
        assert_eq!(sweep_with(tmp.path(), AT_ONCE), 1);
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
        assert_eq!(sweep_with(tmp.path(), AT_ONCE), 0);
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
        assert_eq!(sweep_with(&swept, AT_ONCE), 1);
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link itself is kept");
        assert!(!real.exists());
        assert!(user.join("keep.txt").exists(), "nothing behind a link is removed");
    }

    /// Set the modification time of `p` (a file or a folder) `secs` back.
    fn backdate(p: &Path, secs: u64) {
        let t = SystemTime::now() - Duration::from_secs(secs);
        File::open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn idle_floors() {
        let f = FLOORS;
        assert_eq!(idle_floor(Lock::Taken, true, &f), STALE_AFTER);
        assert_eq!(idle_floor(Lock::Taken, false, &f), REMOTE_STALE_AFTER, "a share: another Mac's lock is invisible");
        assert_eq!(idle_floor(Lock::Unsupported, true, &f), REMOTE_STALE_AFTER);
        assert_eq!(idle_floor(Lock::Unsupported, false, &f), REMOTE_STALE_AFTER);
        assert!(PROBE_AFTER < STALE_AFTER && STALE_AFTER < REMOTE_STALE_AFTER);
    }

    #[test]
    fn a_new_folder_is_not_probed() {
        let tmp = tempfile::tempdir().unwrap();
        let p = staging(tmp.path(), "AbC123");
        // Every floor but the probe one at zero: a folder just made (its
        // writer may not have locked it yet) is not touched.
        let floors = Floors { probe_after: PROBE_AFTER, ..Floors::ZERO };
        assert_eq!(sweep_with(tmp.path(), &floors), 0);
        assert!(p.join("clip.mp4").exists());
        // Its writer can still lock it.
        let writer = File::open(&p).unwrap();
        writer.try_lock().unwrap();
        drop(writer);
        backdate(&p, 2 * 60);
        assert_eq!(sweep_with(tmp.path(), &floors), 1);
    }

    #[test]
    fn a_stale_folder_with_a_recent_child_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let p = staging(tmp.path(), "AbC123");
        // The folder itself is old; the file being written in it is not.
        backdate(&p, 60 * 60);
        assert_eq!(sweep(tmp.path()), 0);
        assert!(p.join("clip.mp4").exists());
        // Real floors, everything idle for an hour: removed.
        backdate(&p.join("clip.mp4"), 60 * 60);
        backdate(&p, 60 * 60);
        assert_eq!(sweep(tmp.path()), 1);
        assert!(!p.exists());
    }

    #[test]
    fn a_folder_put_in_place_of_the_locked_one_is_not_it() {
        let tmp = tempfile::tempdir().unwrap();
        let p = staging(tmp.path(), "AbC123");
        let folder = File::open(&p).unwrap();
        assert!(still_at(&folder, &p).unwrap());
        // Swapped for another folder of the same name, then for a symlink.
        std::fs::rename(&p, tmp.path().join("moved")).unwrap();
        std::fs::create_dir(&p).unwrap();
        assert!(!still_at(&folder, &p).unwrap());
        std::fs::remove_dir(&p).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("moved"), &p).unwrap();
        assert!(!still_at(&folder, &p).unwrap(), "a symlink to it is not it");
    }

    #[test]
    fn a_folder_is_swept_at_most_once_in_a_while() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out.mp4");
        sweep_beside(&out);
        // Stale by every floor, but the folder was just swept: left for now.
        let p = staging(tmp.path(), "AbC123");
        backdate(&p.join("clip.mp4"), 2 * 24 * 60 * 60);
        backdate(&p, 2 * 24 * 60 * 60);
        sweep_beside(&out);
        assert!(p.exists());
        // An unthrottled sweep takes it.
        assert_eq!(sweep(tmp.path()), 1);
    }

    #[test]
    fn sweeping_a_missing_folder_does_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(sweep_with(&tmp.path().join("missing"), AT_ONCE), 0);
        sweep_beside(&tmp.path().join("missing").join("out.mp4"));
    }
}
