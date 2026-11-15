//! Quick-access panel logic: which buttons a capture gets, which panel is
//! current, what a click means, when the panel times out, and how each button
//! maps onto the engine.
//!
//! Nothing here touches AppKit; `quick_panel.rs` is the windowing glue. The
//! engine side is behind [`Backend`] so the dispatch can be tested with a
//! recorder instead of the clipboard and Finder.

use anyhow::Result;
use bugle_core::config::{PanelCorner, QuickAccessConfig};
use bugle_core::{MediaItem, MediaKind};
use bugle_platform::{Corner, PanelChoice};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long the panel stays up after the pointer leaves it, at most.
const HOVER_GRACE: Duration = Duration::from_secs(2);

/// What a panel button (or the thumbnail / close button) does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelAction {
    /// Put the capture on the clipboard (`Engine::copy_media`).
    Copy,
    /// Open the file in the configured markup app (`[quick_access] annotate_app`).
    Annotate,
    /// Float it above all windows (`Engine::pin`).
    Pin,
    /// Send it to the `[sync]` destination (`Engine::sync_media`).
    Sync,
    /// Select it in Finder.
    Reveal,
    /// Open it in the default app (thumbnail click).
    Open,
    /// Close the panel and do nothing (close button).
    Dismiss,
}

impl PanelAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::Annotate => "Annotate",
            Self::Pin => "Pin",
            Self::Sync => "Sync",
            Self::Reveal => "Reveal",
            Self::Open => "Open",
            Self::Dismiss => "Dismiss",
        }
    }

    /// Whether the action may block (clipboard encoding, network, the pin
    /// request that round-trips through the main thread) and so must run off
    /// the main thread.
    pub fn runs_off_main(self) -> bool {
        matches!(self, Self::Copy | Self::Pin | Self::Sync)
    }
}

/// Where the sync button stands for a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncState {
    /// A `[sync]` destination is configured.
    pub destination: bool,
    /// The automatic sync after save already handled this kind.
    pub automatic: bool,
}

/// The buttons a capture gets, in order. Sync only shows up when a
/// destination exists and the automatic sync did not already send the file.
pub fn actions_for(kind: MediaKind, sync: SyncState) -> Vec<PanelAction> {
    let mut v = vec![PanelAction::Copy];
    let image = matches!(kind, MediaKind::Screenshot | MediaKind::ScrollCapture | MediaKind::Annotated);
    if image {
        v.push(PanelAction::Annotate);
        v.push(PanelAction::Pin);
    }
    if sync.destination && !sync.automatic {
        v.push(PanelAction::Sync);
    }
    v.push(PanelAction::Reveal);
    v
}

/// The capture a panel acts on.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// Engine `source`: the history id when the file is in history, else the path.
    pub source: String,
    pub path: PathBuf,
    pub kind: MediaKind,
}

/// Find the history record for a just-saved file so engine actions update
/// that record (sync moves, URLs) instead of treating it as a loose file.
pub fn resolve_target(recent: &[MediaItem], path: &Path) -> Target {
    match recent.iter().find(|m| m.path == path) {
        Some(m) => Target { source: m.id.clone(), path: m.path.clone(), kind: m.kind },
        None => Target {
            source: path.display().to_string(),
            path: path.to_path_buf(),
            kind: bugle_engine::kind_from_path(path),
        },
    }
}

pub fn corner(c: PanelCorner) -> Corner {
    match c {
        PanelCorner::TopRight => Corner::TopRight,
        PanelCorner::TopLeft => Corner::TopLeft,
        PanelCorner::BottomRight => Corner::BottomRight,
        PanelCorner::BottomLeft => Corner::BottomLeft,
    }
}

/// Messages from the panel and its helper threads to the main loop.
#[derive(Debug)]
pub enum QuickEvent {
    /// The user clicked something on panel `generation`.
    Choice { generation: u64, choice: PanelChoice },
    /// A sharper thumbnail for panel `generation`.
    Thumb { generation: u64, png: Vec<u8> },
    /// An off-main-thread action finished.
    Finished { action: PanelAction, result: Result<Option<Feedback>, String> },
}

/// Something to toast after an action.
#[derive(Debug, Clone, PartialEq)]
pub struct Feedback {
    pub title: String,
    pub subtitle: String,
}

#[derive(Debug)]
struct Shown {
    generation: u64,
    target: Target,
    actions: Vec<PanelAction>,
    deadline: Option<Instant>,
}

/// The panel state machine. At most one panel is current; every click,
/// timeout or replacement ends it.
#[derive(Debug)]
pub struct QuickAccess {
    pub config: QuickAccessConfig,
    generation: u64,
    shown: Option<Shown>,
}

impl QuickAccess {
    pub fn new(config: QuickAccessConfig) -> Self {
        Self { config, generation: 0, shown: None }
    }

    fn timeout(&self) -> Option<Duration> {
        let s = self.config.timeout_seconds;
        (s.is_finite() && s > 0.0).then(|| Duration::from_secs_f64(s))
    }

    /// Whether a finished capture should get the panel rather than a toast.
    pub fn wants_panel(&self, has_thumbnail: bool, path: Option<&Path>) -> bool {
        self.config.enabled && has_thumbnail && path.is_some()
    }

    /// Start a new panel (replacing any current one); returns its generation.
    pub fn open(&mut self, target: Target, actions: Vec<PanelAction>, now: Instant) -> u64 {
        self.generation += 1;
        let deadline = self.timeout().map(|t| now + t);
        self.shown = Some(Shown { generation: self.generation, target, actions, deadline });
        self.generation
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.shown.as_ref().is_some_and(|s| s.generation == generation)
    }

    pub fn is_open(&self) -> bool {
        self.shown.is_some()
    }

    /// Map a click to the action and its target and end the panel. Clicks on
    /// a panel that is no longer current are ignored.
    pub fn on_choice(&mut self, generation: u64, choice: PanelChoice) -> Option<(PanelAction, Target)> {
        if !self.is_current(generation) {
            return None;
        }
        let shown = self.shown.take()?;
        let action = match choice {
            PanelChoice::Button(i) => *shown.actions.get(i)?,
            PanelChoice::Close => PanelAction::Dismiss,
            PanelChoice::Thumbnail => PanelAction::Open,
        };
        Some((action, shown.target))
    }

    /// Advance the clock. Hovering keeps the panel up for a short grace
    /// period after the pointer leaves. Returns `true` when the panel timed
    /// out and should be closed.
    pub fn tick(&mut self, now: Instant, hovered: bool) -> bool {
        let grace = self.timeout().map(|t| t.min(HOVER_GRACE));
        let Some(shown) = self.shown.as_mut() else { return false };
        let Some(deadline) = shown.deadline else { return false };
        if hovered {
            if let Some(g) = grace {
                shown.deadline = Some(deadline.max(now + g));
            }
            return false;
        }
        if now >= deadline {
            self.shown = None;
            return true;
        }
        false
    }

    /// The panel went away without a click we saw (e.g. the app replaced it).
    pub fn forget(&mut self) {
        self.shown = None;
    }
}

/// The engine-facing side of the panel's buttons.
pub trait Backend {
    fn copy(&self, source: &str) -> Result<()>;
    fn annotate(&self, path: &Path) -> Result<()>;
    fn pin(&self, source: &str) -> Result<()>;
    /// Returns the sync verb (`copied`, `uploaded`, …) and the destination.
    fn sync(&self, source: &str) -> Result<(String, String)>;
    fn reveal(&self, path: &Path);
    fn open(&self, path: &Path);
}

/// Run one panel action; the result says what, if anything, to toast.
pub fn perform(backend: &dyn Backend, action: PanelAction, target: &Target) -> Result<Option<Feedback>> {
    let name = file_name(&target.path);
    match action {
        PanelAction::Copy => {
            backend.copy(&target.source)?;
            Ok(Some(Feedback { title: "Copied to clipboard".into(), subtitle: name }))
        }
        PanelAction::Annotate => backend.annotate(&target.path).map(|_| None),
        PanelAction::Pin => backend.pin(&target.source).map(|_| None),
        PanelAction::Sync => {
            let (verb, destination) = backend.sync(&target.source)?;
            Ok(Some(Feedback { title: format!("{}: {name}", capitalise(&verb)), subtitle: destination }))
        }
        PanelAction::Reveal => {
            backend.reveal(&target.path);
            Ok(None)
        }
        PanelAction::Open => {
            backend.open(&target.path);
            Ok(None)
        }
        PanelAction::Dismiss => Ok(None),
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn target() -> Target {
        Target { source: "abc123".into(), path: PathBuf::from("/tmp/Bugle shot.png"), kind: MediaKind::Screenshot }
    }

    fn cfg(timeout: f64) -> QuickAccessConfig {
        QuickAccessConfig { timeout_seconds: timeout, ..QuickAccessConfig::default() }
    }

    const NO_SYNC: SyncState = SyncState { destination: false, automatic: false };

    #[test]
    fn buttons_depend_on_kind_and_sync() {
        use PanelAction::*;
        assert_eq!(actions_for(MediaKind::Screenshot, NO_SYNC), vec![Copy, Annotate, Pin, Reveal]);
        assert_eq!(actions_for(MediaKind::ScrollCapture, NO_SYNC), vec![Copy, Annotate, Pin, Reveal]);
        assert_eq!(actions_for(MediaKind::Video, NO_SYNC), vec![Copy, Reveal]);
        let manual = SyncState { destination: true, automatic: false };
        assert_eq!(actions_for(MediaKind::Screenshot, manual), vec![Copy, Annotate, Pin, Sync, Reveal]);
        assert_eq!(actions_for(MediaKind::Gif, manual), vec![Copy, Sync, Reveal]);
        let auto = SyncState { destination: true, automatic: true };
        assert!(!actions_for(MediaKind::Screenshot, auto).contains(&Sync), "already synced on save");
    }

    #[test]
    fn target_prefers_history_record() {
        let mut m = MediaItem::new(MediaKind::ScrollCapture, "/tmp/Bugle shot.png");
        m.id = "deadbeef0001".into();
        let other = MediaItem::new(MediaKind::Screenshot, "/tmp/other.png");
        let t = resolve_target(&[other, m], Path::new("/tmp/Bugle shot.png"));
        assert_eq!(t.source, "deadbeef0001");
        assert_eq!(t.kind, MediaKind::ScrollCapture);
        let loose = resolve_target(&[], Path::new("/tmp/clip.mp4"));
        assert_eq!(loose.source, "/tmp/clip.mp4");
        assert_eq!(loose.kind, MediaKind::Video);
    }

    #[test]
    fn choices_map_to_actions_once() {
        let mut q = QuickAccess::new(cfg(8.0));
        let now = Instant::now();
        let g = q.open(target(), actions_for(MediaKind::Screenshot, NO_SYNC), now);
        assert!(q.is_open());
        assert_eq!(q.on_choice(g + 1, PanelChoice::Button(0)), None, "stale generation");
        let (a, t) = q.on_choice(g, PanelChoice::Button(2)).unwrap();
        assert_eq!(a, PanelAction::Pin);
        assert_eq!(t, target());
        assert!(!q.is_open());
        assert_eq!(q.on_choice(g, PanelChoice::Button(0)), None, "panel already ended");

        let g = q.open(target(), vec![PanelAction::Copy], now);
        assert_eq!(q.on_choice(g, PanelChoice::Button(5)), None, "out of range");
        let g = q.open(target(), vec![PanelAction::Copy], now);
        assert_eq!(q.on_choice(g, PanelChoice::Close).unwrap().0, PanelAction::Dismiss);
        let g = q.open(target(), vec![PanelAction::Copy], now);
        assert_eq!(q.on_choice(g, PanelChoice::Thumbnail).unwrap().0, PanelAction::Open);
    }

    #[test]
    fn new_panel_replaces_old() {
        let mut q = QuickAccess::new(cfg(8.0));
        let now = Instant::now();
        let g1 = q.open(target(), vec![PanelAction::Copy], now);
        let g2 = q.open(target(), vec![PanelAction::Reveal], now);
        assert!(g2 > g1);
        assert!(!q.is_current(g1) && q.is_current(g2));
        assert_eq!(q.on_choice(g1, PanelChoice::Button(0)), None);
        assert_eq!(q.on_choice(g2, PanelChoice::Button(0)).unwrap().0, PanelAction::Reveal);
    }

    #[test]
    fn times_out_and_hover_pauses() {
        let mut q = QuickAccess::new(cfg(5.0));
        let t0 = Instant::now();
        q.open(target(), vec![PanelAction::Copy], t0);
        assert!(!q.tick(t0 + Duration::from_secs(4), false));
        // Hovered past the deadline: stays up.
        assert!(!q.tick(t0 + Duration::from_secs(6), true));
        assert!(!q.tick(t0 + Duration::from_secs(30), true));
        // Pointer leaves: a short grace, then it closes.
        assert!(!q.tick(t0 + Duration::from_millis(31_500), false));
        assert!(q.tick(t0 + Duration::from_secs(33), false));
        assert!(!q.is_open());
        assert!(!q.tick(t0 + Duration::from_secs(40), false), "nothing left to close");
    }

    #[test]
    fn zero_timeout_stays_open() {
        let mut q = QuickAccess::new(cfg(0.0));
        let t0 = Instant::now();
        q.open(target(), vec![PanelAction::Copy], t0);
        assert!(!q.tick(t0 + Duration::from_secs(3600), false));
        assert!(q.is_open());
    }

    #[test]
    fn disabled_config_keeps_toast() {
        let mut c = cfg(8.0);
        let p = Path::new("/tmp/x.png");
        assert!(QuickAccess::new(c.clone()).wants_panel(true, Some(p)));
        assert!(!QuickAccess::new(c.clone()).wants_panel(false, Some(p)), "no thumbnail");
        assert!(!QuickAccess::new(c.clone()).wants_panel(true, None), "nothing saved");
        c.enabled = false;
        assert!(!QuickAccess::new(c).wants_panel(true, Some(p)));
    }

    #[derive(Default)]
    struct Recorder(RefCell<Vec<String>>);

    impl Backend for Recorder {
        fn copy(&self, source: &str) -> Result<()> {
            self.0.borrow_mut().push(format!("copy {source}"));
            Ok(())
        }
        fn annotate(&self, path: &Path) -> Result<()> {
            self.0.borrow_mut().push(format!("annotate {}", path.display()));
            Ok(())
        }
        fn pin(&self, source: &str) -> Result<()> {
            self.0.borrow_mut().push(format!("pin {source}"));
            anyhow::bail!("no image")
        }
        fn sync(&self, source: &str) -> Result<(String, String)> {
            self.0.borrow_mut().push(format!("sync {source}"));
            Ok(("uploaded".into(), "https://b.example/x.png".into()))
        }
        fn reveal(&self, path: &Path) {
            self.0.borrow_mut().push(format!("reveal {}", path.display()));
        }
        fn open(&self, path: &Path) {
            self.0.borrow_mut().push(format!("open {}", path.display()));
        }
    }

    #[test]
    fn perform_dispatches_to_backend() {
        let b = Recorder::default();
        let t = target();
        let fb = perform(&b, PanelAction::Copy, &t).unwrap().unwrap();
        assert_eq!(fb.title, "Copied to clipboard");
        assert_eq!(fb.subtitle, "Bugle shot.png");
        assert_eq!(perform(&b, PanelAction::Annotate, &t).unwrap(), None);
        assert!(perform(&b, PanelAction::Pin, &t).is_err(), "errors propagate");
        let fb = perform(&b, PanelAction::Sync, &t).unwrap().unwrap();
        assert_eq!(fb.title, "Uploaded: Bugle shot.png");
        assert_eq!(fb.subtitle, "https://b.example/x.png");
        assert_eq!(perform(&b, PanelAction::Reveal, &t).unwrap(), None);
        assert_eq!(perform(&b, PanelAction::Open, &t).unwrap(), None);
        assert_eq!(perform(&b, PanelAction::Dismiss, &t).unwrap(), None);
        assert_eq!(
            b.0.into_inner(),
            vec![
                "copy abc123",
                "annotate /tmp/Bugle shot.png",
                "pin abc123",
                "sync abc123",
                "reveal /tmp/Bugle shot.png",
                "open /tmp/Bugle shot.png",
            ]
        );
    }

    #[test]
    fn blocking_actions_leave_the_main_thread() {
        assert!(PanelAction::Pin.runs_off_main(), "pin round-trips through the main thread");
        assert!(PanelAction::Copy.runs_off_main() && PanelAction::Sync.runs_off_main());
        assert!(!PanelAction::Reveal.runs_off_main() && !PanelAction::Dismiss.runs_off_main());
    }

    #[test]
    fn corners_map() {
        assert_eq!(corner(PanelCorner::BottomLeft), Corner::BottomLeft);
        assert_eq!(corner(PanelCorner::default()), Corner::TopRight);
    }
}
