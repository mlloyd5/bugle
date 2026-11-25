//! Cross-process recording state: every engine that records says so in a
//! small file, so another Bugle process on the same Mac (the menu-bar app,
//! while `bugle record`, `bugle serve` or `bugle mcp` records) can show it.
//!
//! * One file per owning process, `<pid>.json`, in [`marker_dir`] (next to
//!   the history file, so processes that share a config share the folder).
//!   A process only ever writes or removes its own file, so two recording
//!   processes cannot be confused about whose recording a file describes.
//! * The owner writes the file to a temp name and renames it into place, so
//!   a reader sees the old file or the new one, never half of one. It is
//!   written when a recording starts and removed when its slot is freed
//!   (stop, auto-stop reap, or a new start finalising the previous one); see
//!   [`Engine::sync_recording_marker`]. A recording that stops by itself
//!   keeps its slot until someone stops or reaps it (`bugle serve` and
//!   `bugle mcp` wait for the client's `record_stop`), so a watchdog thread
//!   ([`WATCH_INTERVAL`]) removes the file as soon as the recorder is seen
//!   to have stopped capturing: its `max_seconds` passed, the GIF loop hit
//!   its length or memory cap, the system recorder exited, or the stream
//!   recorder's capture stream or file writer failed.
//! * The file names the owner by pid *and* the owner's process start time,
//!   so a pid the system reused for another process does not keep a dead
//!   owner's recording alive. A reader ([`scan`]) ignores and removes the
//!   file of a process that is gone (crashed, killed), and leaves alone a
//!   file it cannot parse while its pid is still running (its owner will
//!   replace or remove it). A running owner whose start time the reader may
//!   not read (another user's process, `sudo bugle record`) is trusted.

use crate::Engine;
use bugle_core::{Config, RecordingFormat};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the watchdog of a published recording checks whether its
/// recorder stopped capturing by itself.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// The folder holding the recording-state files for `config`.
pub fn marker_dir(config: &Config) -> PathBuf {
    let history = config.history_path();
    history.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).join("live-recordings")
}

/// A recording another process is making, as its state file describes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveRecording {
    /// The recording process.
    pub pid: u32,
    /// When that process started (microseconds since the Unix epoch, as the
    /// kernel reports it), so a reused pid is not mistaken for the owner; 0
    /// when it could not be read (only the pid is checked then).
    pub process_started_us: u64,
    pub started_at: DateTime<Utc>,
    pub format: RecordingFormat,
    pub output: PathBuf,
    /// The recording stops by itself after this long.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_seconds: Option<f64>,
}

impl LiveRecording {
    /// Seconds recorded at `now`: never negative (a clock that stepped back)
    /// and never more than `max_seconds`, like the owner's own status. Across
    /// processes only the wall clock is shared, so this is `now -
    /// started_at` (the owner's own status uses a monotonic clock); a clock
    /// change while recording shifts it, within those bounds.
    pub fn elapsed_seconds(&self, now: DateTime<Utc>) -> f64 {
        let e = ((now - self.started_at).num_milliseconds() as f64 / 1000.0).max(0.0);
        match self.max_seconds {
            Some(m) if m > 0.0 => e.min(m),
            _ => e,
        }
    }

    /// Whether the process that wrote this is still the one running under
    /// its pid. When its start time cannot be read, a running pid is
    /// trusted.
    pub fn owner_alive(&self) -> bool {
        match process_start(self.pid) {
            ProcessStart::Gone => false,
            ProcessStart::Unknown => true,
            ProcessStart::At(t) => self.process_started_us == 0 || t == self.process_started_us,
        }
    }
}

/// What a reader can tell about a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStart {
    /// No such process runs (or only its zombie).
    Gone,
    /// It runs, but its start time may not be read (another user's).
    Unknown,
    /// It runs and started at this time (microseconds since the Unix epoch).
    At(u64),
}

impl ProcessStart {
    /// The start time when known.
    pub fn micros(self) -> Option<u64> {
        match self {
            ProcessStart::At(t) => Some(t),
            _ => None,
        }
    }
}

/// The state file of process `pid` in `dir`.
fn file_for(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.json"))
}

/// The pid a state file (`<pid>.json`) or an owner's temp file
/// (`.<pid>.json.tmp`) belongs to, and whether it is a temp file.
fn parse_name(name: &str) -> Option<(u32, bool)> {
    let (digits, tmp) = match name.strip_prefix('.').and_then(|n| n.strip_suffix(".json.tmp")) {
        Some(d) => (d, true),
        None => (name.strip_suffix(".json")?, false),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((digits.parse().ok()?, tmp))
}

/// Write `rec` as its owner's state file in `dir`, replacing the owner's
/// previous one in a single rename.
pub fn publish(dir: &Path, rec: &LiveRecording) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.json.tmp", rec.pid));
    std::fs::write(&tmp, serde_json::to_vec(rec)?)?;
    std::fs::rename(&tmp, file_for(dir, rec.pid)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Remove process `pid`'s state file from `dir`, if any.
pub fn clear(dir: &Path, pid: u32) -> std::io::Result<()> {
    match std::fs::remove_file(file_for(dir, pid)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The recordings other live processes are making, by start time, leaving
/// out `own_pid`'s. Files of processes that are gone are removed; a file
/// that does not parse, or names another pid than its file name, is skipped
/// (and removed once its pid is gone).
pub fn scan(dir: &Path, own_pid: u32) -> Vec<LiveRecording> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut live = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some((pid, tmp)) = name.to_str().and_then(parse_name) else {
            continue;
        };
        if pid == own_pid {
            continue;
        }
        let path = entry.path();
        let parsed = (!tmp)
            .then(|| std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<LiveRecording>(&b).ok()))
            .flatten()
            .filter(|r| r.pid == pid);
        let stale = match &parsed {
            Some(rec) => !rec.owner_alive(),
            None => process_start(pid) == ProcessStart::Gone,
        };
        if stale {
            tracing::debug!("removing the recording state of process {pid}, which is gone: {}", path.display());
            let _ = std::fs::remove_file(&path);
        } else if let Some(rec) = parsed {
            live.push(rec);
        }
    }
    live.sort_by_key(|r| r.started_at);
    live
}

/// Whether process `pid` runs and, when this process may read it, when it
/// started (a zombie counts as gone).
#[cfg(target_os = "macos")]
pub fn process_start(pid: u32) -> ProcessStart {
    let Some(pid) = i32::try_from(pid).ok().filter(|p| *p > 0) else {
        return ProcessStart::Gone;
    };
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
    if n == size {
        return match info.pbi_status {
            libc::SZOMB => ProcessStart::Gone,
            _ => ProcessStart::At(info.pbi_start_tvsec.saturating_mul(1_000_000).saturating_add(info.pbi_start_tvusec)),
        };
    }
    // Not readable (another user's process answers EPERM): ask whether it
    // runs at all; EPERM from `kill` means it does.
    let runs = unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if runs {
        ProcessStart::Unknown
    } else {
        ProcessStart::Gone
    }
}

/// Elsewhere only whether the process runs.
#[cfg(not(target_os = "macos"))]
pub fn process_start(pid: u32) -> ProcessStart {
    match i32::try_from(pid).ok().filter(|p| *p > 0) {
        Some(p) if unsafe { libc::kill(p, 0) } == 0 => ProcessStart::Unknown,
        _ => ProcessStart::Gone,
    }
}

/// This process's pid and start time (0 if unreadable), read once.
fn own_identity() -> (u32, u64) {
    static ID: std::sync::OnceLock<(u32, u64)> = std::sync::OnceLock::new();
    *ID.get_or_init(|| {
        let pid = std::process::id();
        (pid, process_start(pid).micros().unwrap_or(0))
    })
}

/// A check whether `active`'s recorder has stopped capturing by itself, that
/// a watchdog thread can run without the recorder lock: the same signals as
/// `Backend::stopped_by_itself`, minus the ones only the slot can see.
fn stopped_probe(active: &super::Active) -> Box<dyn Fn() -> bool + Send> {
    let (started, max_seconds) = (active.started, active.max_seconds);
    let deadline = move || super::deadline_passed(started.elapsed().as_secs_f64(), max_seconds);
    match &active.backend {
        super::Backend::Gif { capture_done, .. } => {
            let done = capture_done.clone();
            Box::new(move || done.load(Ordering::Relaxed) || deadline())
        }
        super::Backend::Video { child, .. } => {
            let pid = child.id();
            Box::new(move || bugle_platform::child_exited(pid) || deadline())
        }
        // The stream recorder stops at its deadline, or early when capture
        // or writing fails; its probe says so without the recorder.
        super::Backend::Sck { rec, .. } => {
            let probe = rec.as_ref().map(|r| r.probe());
            Box::new(move || probe.as_ref().is_none_or(|p| p.stopped()) || deadline())
        }
    }
}

/// Watch a published recording: once `stopped()` says its recorder stopped
/// capturing, remove `pid`'s file from `dir` (unless `cancel` was set first,
/// by the next sync: that one owns the file now). One check per
/// [`WATCH_INTERVAL`]; exits after removing the file or on `cancel`.
fn spawn_watchdog(dir: PathBuf, pid: u32, stopped: Box<dyn Fn() -> bool + Send>, cancel: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new().name("bugle-rec-watch".into()).spawn(move || loop {
        std::thread::sleep(WATCH_INTERVAL);
        if cancel.load(Ordering::SeqCst) {
            return;
        }
        if stopped() {
            let _serial = SYNC.lock().unwrap_or_else(|e| e.into_inner());
            if !cancel.load(Ordering::SeqCst) {
                tracing::debug!("the recording stopped by itself; removing its recording state");
                if let Err(e) = clear(&dir, pid) {
                    tracing::warn!("could not remove the recording state in {}: {e}", dir.display());
                }
            }
            return;
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("could not start the recording-state watchdog (a recording that stops by itself shows until it is stopped): {e}");
    }
}

/// Serialises [`Engine::sync_recording_marker`], so the file always ends up
/// matching the last state any thread saw.
static SYNC: Mutex<()> = Mutex::new(());

impl Engine {
    /// Make this process's state file match its recorder: written while a
    /// recording is in the slot and capturing, removed once it is free or
    /// has stopped capturing (a watchdog removes it then, see
    /// [`spawn_watchdog`]). Called at every start and stop (with
    /// `recording_changed`), never with the recorder locked. Failures are
    /// logged: other processes then just do not see the recording.
    pub(super) fn sync_recording_marker(&self) {
        let _serial = SYNC.lock().unwrap_or_else(|e| e.into_inner());
        let (pid, process_started_us) = own_identity();
        let mut slot = self.recorder_state.inner.lock().unwrap_or_else(|e| e.into_inner());
        let capturing = slot.as_mut().is_some_and(|a| a.poll() == bugle_core::RecordingState::Active);
        let live = slot.as_ref().filter(|_| capturing).map(|a| {
            let rec = LiveRecording {
                pid,
                process_started_us,
                started_at: a.started_at,
                format: a.format,
                output: a.output.clone(),
                max_seconds: a.max_seconds,
            };
            (rec, stopped_probe(a))
        });
        drop(slot);
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(old) = self.recorder_state.marker_watch.lock().unwrap_or_else(|e| e.into_inner()).replace(cancel.clone()) {
            old.store(true, Ordering::SeqCst);
        }
        let dir = marker_dir(&self.config);
        let r = match live {
            Some((rec, stopped)) => publish(&dir, &rec).map(|()| spawn_watchdog(dir.clone(), pid, stopped, cancel)),
            None => clear(&dir, pid),
        };
        if let Err(e) = r {
            tracing::warn!("could not update the recording state in {} (other Bugle processes will not see it): {e}", dir.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pid: u32) -> LiveRecording {
        LiveRecording {
            pid,
            process_started_us: process_start(pid).micros().unwrap_or(0),
            started_at: Utc::now(),
            format: RecordingFormat::Mp4,
            output: PathBuf::from("/tmp/x.mp4"),
            max_seconds: None,
        }
    }

    /// A pid that no longer runs: a child that exited and was reaped.
    fn dead_pid() -> u32 {
        let mut c = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    }

    /// A process that keeps running until killed (and is reaped on drop).
    struct Sleeper(std::process::Child);
    impl Sleeper {
        fn new() -> Self {
            Self(std::process::Command::new("/bin/sleep").arg("60").spawn().unwrap())
        }
        fn pid(&self) -> u32 {
            self.0.id()
        }
    }
    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn names() {
        assert_eq!(parse_name("123.json"), Some((123, false)));
        assert_eq!(parse_name(".123.json.tmp"), Some((123, true)));
        for bad in ["x.json", ".json", "12a.json", "123.json.tmp", "123", ".DS_Store", "-1.json"] {
            assert_eq!(parse_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn write_scan_and_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("live-recordings");
        let me = std::process::id();
        let r = rec(me);
        assert!(r.process_started_us > 0, "this process's start time is readable");
        publish(&dir, &r).unwrap();
        // The owner does not see its own file; anyone else does.
        assert!(scan(&dir, me).is_empty());
        assert_eq!(scan(&dir, 0), vec![r.clone()]);
        // Republishing replaces it in place, leaving no temp file.
        let r2 = LiveRecording { output: PathBuf::from("/tmp/y.mp4"), ..r };
        publish(&dir, &r2).unwrap();
        assert_eq!(scan(&dir, 0), vec![r2]);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        clear(&dir, me).unwrap();
        assert!(scan(&dir, 0).is_empty());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        // Clearing again, or scanning a folder that does not exist, is fine.
        clear(&dir, me).unwrap();
        assert!(scan(&tmp.path().join("missing"), 0).is_empty());
    }

    #[test]
    fn a_dead_owner_is_ignored_and_cleaned_up() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = dead_pid();
        assert_eq!(process_start(pid), ProcessStart::Gone);
        publish(tmp.path(), &LiveRecording { process_started_us: 1, ..rec(pid) }).unwrap();
        std::fs::write(tmp.path().join(format!(".{pid}.json.tmp")), b"{").unwrap();
        assert!(scan(tmp.path(), 0).is_empty());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "stale file and temp file removed");
    }

    #[test]
    fn a_reused_pid_is_not_the_owner() {
        let tmp = tempfile::tempdir().unwrap();
        // Our pid, but written by an earlier process that had it.
        let me = std::process::id();
        let mut r = rec(me);
        r.process_started_us -= 1;
        assert!(!r.owner_alive());
        publish(tmp.path(), &r).unwrap();
        assert!(scan(tmp.path(), 0).is_empty());
        assert!(!file_for(tmp.path(), me).exists());
    }

    #[test]
    fn a_corrupt_file_is_skipped_and_kept_while_its_pid_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let alive = Sleeper::new();
        let dead = dead_pid();
        std::fs::write(file_for(tmp.path(), alive.pid()), b"{\"pid\":").unwrap();
        std::fs::write(file_for(tmp.path(), dead), b"not json").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), b"unrelated").unwrap();
        assert!(scan(tmp.path(), 0).is_empty());
        assert!(file_for(tmp.path(), alive.pid()).exists(), "its owner may still rewrite it");
        assert!(!file_for(tmp.path(), dead).exists());
        assert!(tmp.path().join("notes.txt").exists(), "other files are left alone");
    }

    #[test]
    fn two_owners_keep_their_own_files() {
        let tmp = tempfile::tempdir().unwrap();
        let other = Sleeper::new();
        let me = std::process::id();
        let mine = rec(me);
        let theirs = LiveRecording { started_at: mine.started_at - chrono::Duration::seconds(5), ..rec(other.pid()) };
        publish(tmp.path(), &mine).unwrap();
        publish(tmp.path(), &theirs).unwrap();
        // A third process sees both, oldest first; each owner sees the other.
        assert_eq!(scan(tmp.path(), 0), vec![theirs.clone(), mine.clone()]);
        assert_eq!(scan(tmp.path(), me), vec![theirs.clone()]);
        assert_eq!(scan(tmp.path(), other.pid()), vec![mine.clone()]);
        // One stopping leaves the other's recording in place.
        clear(tmp.path(), me).unwrap();
        assert_eq!(scan(tmp.path(), 0), vec![theirs.clone()]);
        // A file claiming to be ours under the other's name is not trusted
        // and, while that process runs, not removed either.
        std::fs::write(file_for(tmp.path(), other.pid()), serde_json::to_vec(&mine).unwrap()).unwrap();
        assert!(scan(tmp.path(), 0).is_empty());
        assert!(file_for(tmp.path(), other.pid()).exists());
        // Once the other process is killed, its file goes.
        drop(other);
        assert!(scan(tmp.path(), 0).is_empty());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    /// A GIF recording whose frame loop waits for the stop flag, then
    /// writes one frame to `out` (or fails, without `ok`).
    fn gif_active(out: &Path, ok: bool) -> super::super::Active {
        gif_active_with(out, ok, Arc::new(AtomicBool::new(false)), Some(90.0))
    }

    /// The same, but the loop also stops capturing by itself once `release`
    /// is set (as at its length or memory cap).
    fn gif_active_with(out: &Path, ok: bool, release: Arc<AtomicBool>, max_seconds: Option<f64>) -> super::super::Active {
        use std::sync::atomic::AtomicU64;
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (stop2, done2, out2) = (stop.clone(), done.clone(), out.to_path_buf());
        let handle = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) && !release.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            done2.store(true, Ordering::Relaxed);
            anyhow::ensure!(ok, "encoder failed");
            let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]));
            let mut enc = image::codecs::gif::GifEncoder::new(std::fs::File::create(&out2)?);
            enc.encode_frames(std::iter::once(image::Frame::new(img)))?;
            Ok(1)
        });
        super::super::Active {
            format: RecordingFormat::Gif,
            started: std::time::Instant::now(),
            started_at: Utc::now(),
            output: out.to_path_buf(),
            label: None,
            source: "test".into(),
            backend: super::super::Backend::Gif { stop, capture_done: done, handle: Some(handle) },
            frames: Arc::new(AtomicU64::new(0)),
            max_seconds,
            finished_at: None,
            events: None,
        }
    }

    #[test]
    fn the_engine_publishes_its_recording_while_it_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.general.save_dir = tmp.path().join("save").display().to_string();
        cfg.general.history_path = Some(tmp.path().join("history.jsonl").display().to_string());
        let engine = Engine::new(cfg, false).unwrap();
        let dir = marker_dir(&engine.config);
        assert_eq!(dir, tmp.path().join("live-recordings"));
        let me = std::process::id();

        let out = tmp.path().join("a.gif");
        engine.install(engine.idle_recorder().unwrap(), gif_active(&out, true));
        let seen = scan(&dir, 0);
        assert_eq!(seen.len(), 1);
        let (st, r) = (engine.record_status(), &seen[0]);
        assert_eq!((r.pid, r.format, &r.output, r.max_seconds), (me, RecordingFormat::Gif, &out, Some(90.0)));
        assert_eq!(Some(r.started_at), st.started_at);
        assert!(r.owner_alive());
        assert!(scan(&dir, me).is_empty(), "the engine's own process does not count it");

        engine.record_stop().unwrap();
        assert!(scan(&dir, 0).is_empty());

        // A stop whose finalisation fails still frees the slot and the file.
        engine.install(engine.idle_recorder().unwrap(), gif_active(&tmp.path().join("b.gif"), false));
        assert_eq!(scan(&dir, 0).len(), 1);
        assert!(engine.record_stop().is_err());
        assert!(scan(&dir, 0).is_empty());
    }

    fn engine_in(tmp: &Path) -> Engine {
        let mut cfg = Config::default();
        cfg.general.save_dir = tmp.join("save").display().to_string();
        cfg.general.history_path = Some(tmp.join("history.jsonl").display().to_string());
        Engine::new(cfg, false).unwrap()
    }

    /// Wait (at most 3 watchdog intervals plus slack) for `f`.
    fn eventually(mut f: impl FnMut() -> bool) -> bool {
        let t0 = std::time::Instant::now();
        while t0.elapsed() < WATCH_INTERVAL * 3 + Duration::from_secs(2) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_recording_that_stops_capturing_by_itself_is_unpublished() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_in(tmp.path());
        let dir = marker_dir(&engine.config);

        // No max_seconds; the GIF loop stops by itself (a cap). Nobody
        // reaps it (as under `bugle serve`): the file still goes.
        let release = Arc::new(AtomicBool::new(false));
        let out = tmp.path().join("cap.gif");
        engine.install(engine.idle_recorder().unwrap(), gif_active_with(&out, true, release.clone(), None));
        assert_eq!(scan(&dir, 0).len(), 1);
        release.store(true, Ordering::Relaxed);
        assert!(eventually(|| scan(&dir, 0).is_empty()), "the file outlived the capture");
        assert_eq!(engine.record_status().state, bugle_core::RecordingState::Finishing, "the slot is kept for record_stop");
        engine.record_stop().unwrap();
        assert!(scan(&dir, 0).is_empty());

        // max_seconds passed and nobody calls record_stop.
        let out = tmp.path().join("deadline.gif");
        engine.install(engine.idle_recorder().unwrap(), gif_active_with(&out, true, Default::default(), Some(0.2)));
        assert!(eventually(|| scan(&dir, 0).is_empty()), "the file outlived max_seconds");
        engine.record_stop().unwrap();
    }

    /// A stream recording (a fixture recorder: no stream, no writer) into
    /// `out`, with no `max_seconds`.
    fn sck_active(out: &Path) -> super::super::Active {
        let mut a = gif_active(out, true);
        // The GIF backend stops on drop.
        a.backend = super::super::Backend::Sck { rec: Some(bugle_platform::SckRecorder::fixture()), wants_mov: false, audio: None };
        a.format = RecordingFormat::Mp4;
        a.max_seconds = None;
        a
    }

    /// Make the stream of the stream recording in `engine`'s slot fail.
    fn fail_stream(engine: &Engine, why: &str) {
        let g = engine.recorder_state.inner.lock().unwrap();
        match g.as_ref().map(|a| &a.backend) {
            Some(super::super::Backend::Sck { rec: Some(rec), .. }) => rec.fixture_stream_failed(why),
            _ => panic!("no stream recording in the slot"),
        }
    }

    #[test]
    fn a_stream_recording_that_fails_early_is_unpublished_and_reported() {
        use bugle_core::RecordingState;
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_in(tmp.path());
        let dir = marker_dir(&engine.config);

        // Nobody reaps it (as under `bugle serve` or `bugle record`).
        engine.install(engine.idle_recorder().unwrap(), sck_active(&tmp.path().join("a.mp4")));
        assert_eq!(scan(&dir, 0).len(), 1);
        let st = engine.record_status();
        assert_eq!((st.state, st.failure), (RecordingState::Active, None));
        fail_stream(&engine, "display removed");
        assert!(eventually(|| scan(&dir, 0).is_empty()), "the file outlived the stream");
        let st = engine.record_status();
        assert_eq!(st.state, RecordingState::Finishing, "the engine's own status is not a phantom recording");
        let why = st.failure.unwrap_or_default();
        assert!(why.contains("capture stream stopped") && why.contains("display removed"), "{why}");
        // The stop reports what came of it (here: nothing was captured).
        let e = engine.record_stop().unwrap_err().to_string();
        assert!(e.contains("no frames were captured"), "{e}");
        assert!(scan(&dir, 0).is_empty());
        assert_eq!(engine.record_status().state, RecordingState::Idle);

        // Where a reaper runs (the menu-bar app), it finalises it, and a new
        // recording can start meanwhile without an explicit stop.
        engine.install(engine.idle_recorder().unwrap(), sck_active(&tmp.path().join("b.mp4")));
        fail_stream(&engine, "permission revoked");
        assert!(engine.record_reap().is_some_and(|r| r.is_err()));
        assert!(scan(&dir, 0).is_empty());
        engine.install(engine.idle_recorder().unwrap(), sck_active(&tmp.path().join("c.mp4")));
        fail_stream(&engine, "permission revoked");
        drop(engine.free_recorder(false).unwrap());
        assert!(scan(&dir, 0).is_empty());

        // A stream that fails before the recording is published (between
        // its start and the first sync) is never published.
        let mut a = sck_active(&tmp.path().join("d.mp4"));
        if let super::super::Backend::Sck { rec: Some(rec), .. } = &a.backend {
            rec.fixture_stream_failed("stopped at once");
        }
        a.started = std::time::Instant::now();
        engine.install(engine.idle_recorder().unwrap(), a);
        assert!(scan(&dir, 0).is_empty());
        assert!(engine.record_stop().is_err());
    }

    #[test]
    fn a_previous_recordings_watchdog_leaves_the_next_one_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine_in(tmp.path());
        let dir = marker_dir(&engine.config);
        let release = Arc::new(AtomicBool::new(false));
        engine.install(engine.idle_recorder().unwrap(), gif_active_with(&tmp.path().join("a.gif"), true, release.clone(), None));
        engine.record_stop().unwrap();
        // The first one's capture flag is set now; its watchdog must not
        // take the second recording's file.
        engine.install(engine.idle_recorder().unwrap(), gif_active(&tmp.path().join("b.gif"), true));
        std::thread::sleep(WATCH_INTERVAL * 2 + Duration::from_millis(300));
        assert_eq!(scan(&dir, 0).len(), 1);
        engine.record_stop().unwrap();
        assert!(scan(&dir, 0).is_empty());
    }

    #[test]
    fn the_system_recorder_exiting_counts_as_stopped() {
        let child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let mut a = gif_active(Path::new("/nonexistent/x.mov"), true);
        // Swap in a system-recorder backend; the GIF one stops on drop.
        a.backend = super::super::Backend::Video { child, wants_mp4: false };
        let stopped = stopped_probe(&a);
        assert!(eventually(&stopped));
    }

    #[test]
    fn a_running_owner_that_cannot_be_inspected_is_trusted() {
        // launchd (pid 1) belongs to root: its start time is not readable
        // here, as with `sudo bugle record`.
        if process_start(1) != ProcessStart::Unknown {
            eprintln!("skipped: pid 1's start time is readable (running as root?)");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let r = LiveRecording { process_started_us: 1_234_567, ..rec(1) };
        assert!(r.owner_alive());
        publish(tmp.path(), &r).unwrap();
        assert_eq!(scan(tmp.path(), 0), vec![r]);
        assert!(file_for(tmp.path(), 1).exists());
    }

    #[test]
    fn elapsed_is_clamped() {
        let now = Utc::now();
        let mut r = rec(std::process::id());
        r.started_at = now - chrono::Duration::milliseconds(75_500);
        assert_eq!(r.elapsed_seconds(now), 75.5);
        r.max_seconds = Some(30.0);
        assert_eq!(r.elapsed_seconds(now), 30.0);
        r.started_at = now + chrono::Duration::seconds(3);
        assert_eq!(r.elapsed_seconds(now), 0.0);
    }
}
