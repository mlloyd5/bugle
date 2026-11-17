//! Window pick sessions: the hover-highlight window picker, the crosshair
//! area selection, and the Space toggle between them.
//!
//! The window picker is a see-through overlay that highlights the window
//! under the pointer; a click picks it, Esc / right click / timeout cancels.
//! The overlay lives in the platform shim and only reports pointer
//! positions; [`window_at`] decides which window that is.
//!
//! A pick *session* may switch overlays: Space in the picker closes it and
//! opens the area selection, and Space in the selection (while not
//! dragging) goes back. [`Session`] is that state machine, free of AppKit so
//! it can be tested; [`begin`] drives it on the main thread of a process
//! whose AppKit loop is already running (the menu-bar app), and
//! [`run_blocking`] pumps a loop of its own around it (the CLI).

use anyhow::{anyhow, Result};
use bugle_core::{BugleError, Point, Rect, WindowInfo};
use bugle_platform::{PickEvent, SelectOutcome};
use std::cell::Cell;
use std::sync::mpsc::{self, Sender, TryRecvError};
use std::time::{Duration, Instant};

/// How long the window picker waits for a choice before closing itself.
pub const PICK_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a session that may switch overlays stays up in total. Each
/// picker phase is still capped at [`PICK_TIMEOUT`]; an area phase gets
/// whatever is left, so toggling cannot keep the overlay up forever.
pub const SESSION_LIMIT: Duration = Duration::from_secs(120);

/// How long after the worker's request the main thread may still open the
/// overlay. Later than this, the worker could give up before the overlay
/// closes, so the click would go nowhere; the request is refused instead.
pub const START_WINDOW: Duration = Duration::from_secs(3);

/// After a window is picked: the overlay activated Bugle, so give the
/// target app a moment to become active again and not be captured dimmed.
pub const SETTLE: Duration = Duration::from_millis(150);

/// Smallest side, in points, of a window worth picking. Matches what
/// `bugle_platform::app_windows` (and so window capture by id) accepts.
pub const MIN_SIDE: f64 = 50.0;

/// Which overlay a session shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Hover-highlight window picker.
    Window,
    /// Crosshair area selection.
    Area,
}

impl Mode {
    pub fn other(self) -> Self {
        match self {
            Mode::Window => Mode::Area,
            Mode::Area => Mode::Window,
        }
    }
}

/// What the user chose.
#[derive(Debug, Clone)]
pub enum Picked {
    Window(WindowInfo),
    Area(Rect),
}

/// How one overlay phase ended.
#[derive(Debug, Clone)]
pub enum PhaseEnd {
    /// A click in the picker: the window under it, `None` outside every window.
    Clicked(Option<WindowInfo>),
    /// A dragged area.
    Selected(Rect),
    /// Esc, right click, timeout, Bugle losing active status.
    Cancelled,
    /// Space: show the other overlay.
    Switch,
}

/// What the session does next.
#[derive(Debug, Clone)]
pub enum Next {
    /// Open this overlay, closing itself after `timeout`.
    Open { mode: Mode, timeout: Duration },
    /// Answer the caller (`None` = cancelled).
    Done(Option<Picked>),
}

/// Total time a session may stay up; see [`SESSION_LIMIT`].
pub fn session_limit(allow_switch: bool, pick_timeout: Duration) -> Duration {
    if allow_switch {
        SESSION_LIMIT.max(pick_timeout)
    } else {
        pick_timeout
    }
}

/// The Space-toggle state machine of one pick session.
#[derive(Debug, Clone)]
pub struct Session {
    mode: Mode,
    allow_switch: bool,
    pick_timeout: Duration,
    deadline: Instant,
    switches: u32,
}

impl Session {
    /// A session that opens `start` first. Without `allow_switch`, Space is
    /// not offered and a stray switch ends the session as cancelled.
    pub fn new(start: Mode, allow_switch: bool, pick_timeout: Duration, now: Instant) -> Self {
        Self { mode: start, allow_switch, pick_timeout, deadline: now + session_limit(allow_switch, pick_timeout), switches: 0 }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn allow_switch(&self) -> bool {
        self.allow_switch
    }

    /// How many times the user switched overlays so far.
    pub fn switches(&self) -> u32 {
        self.switches
    }

    /// The overlay to open first.
    pub fn start(&self, now: Instant) -> Next {
        self.open(now)
    }

    fn open(&self, now: Instant) -> Next {
        let left = self.deadline.saturating_duration_since(now);
        if left.is_zero() {
            return Next::Done(None);
        }
        let timeout = match self.mode {
            Mode::Window => left.min(self.pick_timeout),
            Mode::Area => left,
        };
        Next::Open { mode: self.mode, timeout }
    }

    /// The current overlay closed with `end`.
    pub fn on_end(&mut self, end: PhaseEnd, now: Instant) -> Next {
        match end {
            PhaseEnd::Clicked(w) => Next::Done(w.map(Picked::Window)),
            PhaseEnd::Selected(r) => Next::Done(Some(Picked::Area(r))),
            PhaseEnd::Cancelled => Next::Done(None),
            PhaseEnd::Switch if !self.allow_switch => Next::Done(None),
            PhaseEnd::Switch => {
                self.mode = self.mode.other();
                self.switches += 1;
                self.open(now)
            }
        }
    }
}

/// Why a session could not open or continue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Another overlay or session is up, or the request came too late:
    /// trying again later can work (HTTP 409).
    Busy(String),
    /// Something broke (the window list could not be read).
    Failed(String),
}

impl Refusal {
    pub fn into_error(self) -> anyhow::Error {
        match self {
            Refusal::Busy(why) => BugleError::Busy(format!("window picker unavailable: {why}")).into(),
            Refusal::Failed(why) => anyhow!("window picker unavailable: {why}"),
        }
    }
}

/// The main thread's answer: what was picked, `None` when cancelled, or why
/// the overlay could not open.
pub type PickReply = Sender<Result<Option<Picked>, Refusal>>;

/// A pick request from a worker thread.
pub struct Request {
    pub start: Mode,
    pub allow_switch: bool,
    /// Window picker timeout (each picker phase).
    pub timeout: Duration,
    /// Not opened after this, when the worker may have stopped waiting.
    pub deadline: Instant,
    pub reply: PickReply,
}

/// Programmatic stand-ins for keys, for `--smoke-test`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// What Space does in the open overlay.
    Switch,
    /// What Esc does.
    Cancel,
}

thread_local! {
    /// Whether a pick session is between its first overlay and its answer
    /// (main thread only).
    static IN_SESSION: Cell<bool> = const { Cell::new(false) };
}

/// Whether a pick session is running (main thread).
pub fn in_session() -> bool {
    IN_SESSION.with(Cell::get)
}

/// What the hit test ignores.
#[derive(Debug, Clone, Default)]
pub struct PickFilter {
    /// Bugle's own process: the overlay, toasts and pins are never picked.
    pub own_pid: i32,
    /// Display frames; windows that touch none of them are off screen.
    /// Empty means "don't check".
    pub displays: Vec<Rect>,
}

impl PickFilter {
    pub fn current() -> Self {
        let displays = bugle_platform::displays().map(|d| d.into_iter().map(|d| d.frame).collect()).unwrap_or_default();
        Self { own_pid: std::process::id() as i32, displays }
    }

    /// An ordinary application window that is on screen, visible (alpha
    /// above zero), big enough to matter and not Bugle's own.
    pub fn accepts(&self, w: &WindowInfo) -> bool {
        w.layer == 0
            && w.on_screen
            && w.alpha > 0.0
            && !w.owner.is_empty()
            && w.pid != self.own_pid
            && w.frame.is_valid()
            && w.frame.w >= MIN_SIDE
            && w.frame.h >= MIN_SIDE
            && (self.displays.is_empty() || self.displays.iter().any(|d| d.intersection(&w.frame).is_some()))
    }
}

/// The window under `p` (global points, top-left origin). `windows` is in
/// window-server z-order, front-most first, as `bugle_platform::windows`
/// returns it; the first window the filter accepts that contains `p` wins.
pub fn window_at<'a>(windows: &'a [WindowInfo], p: Point, filter: &PickFilter) -> Option<&'a WindowInfo> {
    windows.iter().find(|w| w.frame.contains(p) && filter.accepts(w))
}

/// Text drawn on the highlighted window.
pub fn label(w: &WindowInfo) -> String {
    let title = w.title.trim();
    let name = if title.is_empty() || title == w.owner { w.owner.clone() } else { format!("{} — {title}", w.owner) };
    format!("{name}  {:.0} × {:.0}", w.frame.w, w.frame.h)
}

/// Answer `reply` with a refusal. `switched`: an earlier overlay of this
/// session left Bugle active for a successor that is not coming, so hand
/// focus back. Does not touch [`IN_SESSION`]; the caller owns that.
fn fail(reply: &PickReply, why: Refusal, switched: bool) -> bool {
    tracing::warn!("window picker not started: {why:?}");
    if switched {
        bugle_platform::app_deactivate();
    }
    let _ = reply.send(Err(why));
    false
}

/// Main thread: start a pick session and answer `req.reply` when it ends.
/// When the first overlay cannot open (another session is running,
/// `deadline` has passed, the window list cannot be read, or another
/// overlay is up) `reply` gets an `Err` naming the reason and this returns
/// `false`.
pub fn begin(req: Request) -> bool {
    let Request { start, allow_switch, timeout, deadline, reply } = req;
    let now = Instant::now();
    if in_session() {
        // Leave IN_SESSION alone: it belongs to the running session.
        return fail(&reply, Refusal::Busy("another window pick is in progress".into()), false);
    }
    if now > deadline {
        return fail(&reply, Refusal::Busy("the request expired before the overlay could open".into()), false);
    }
    let session = Session::new(start, allow_switch, timeout, now);
    let next = session.start(now);
    advance(session, next, reply)
}

/// Act on `next`: open the next overlay, or deliver the answer.
fn advance(session: Session, next: Next, reply: PickReply) -> bool {
    match next {
        Next::Done(picked) => {
            IN_SESSION.with(|s| s.set(false));
            if session.switches() > 0 {
                // A switch skipped the deactivation; the session may have
                // ended without another overlay (time limit).
                bugle_platform::app_deactivate();
            }
            let _ = reply.send(Ok(picked));
            true
        }
        Next::Open { mode, timeout } => {
            if session.switches() > 0 {
                tracing::info!("pick session: switched to {:?} (switch {})", session.mode(), session.switches());
            }
            IN_SESSION.with(|s| s.set(true));
            // Kept for the refusal case: the handler (and the `reply` it
            // owns) is dropped unused when the overlay does not start.
            let refused = reply.clone();
            let switched = session.switches() > 0;
            let opened = match mode {
                Mode::Window => open_window(session, timeout, reply),
                Mode::Area => open_area(session, timeout, reply),
            };
            match opened {
                Ok(()) => true,
                Err(why) => {
                    // Set just above, so this session owns the flag.
                    IN_SESSION.with(|s| s.set(false));
                    fail(&refused, why, switched)
                }
            }
        }
    }
}

fn open_window(session: Session, timeout: Duration, reply: PickReply) -> Result<(), Refusal> {
    let filter = PickFilter::current();
    // Snapshot before the overlay exists (and again after every switch
    // back); the filter drops Bugle's own windows anyway.
    let windows: Vec<WindowInfo> = match bugle_platform::windows(true) {
        Ok(w) => w.into_iter().filter(|w| filter.accepts(w)).collect(),
        Err(e) => return Err(Refusal::Failed(format!("could not list windows: {e:#}"))),
    };
    let allow_switch = session.allow_switch();
    // Outer `None` until the first move, so the first position is always drawn.
    let mut current: Option<Option<u32>> = None;
    let mut state = Some((session, reply));
    let started = bugle_platform::window_pick_begin(timeout, allow_switch, move |ev| {
        let end = match ev {
            PickEvent::Moved(p) => {
                let hit = window_at(&windows, p, &filter);
                let id = hit.map(|w| w.id);
                if current != Some(id) {
                    current = Some(id);
                    tracing::debug!("window picker: {:?} at {},{}", hit.map(label), p.x, p.y);
                    bugle_platform::window_pick_highlight(hit.map(|w| w.frame), &hit.map(label).unwrap_or_default());
                }
                return;
            }
            PickEvent::Clicked(p) => PhaseEnd::Clicked(window_at(&windows, p, &filter).cloned()),
            PickEvent::Cancelled => PhaseEnd::Cancelled,
            PickEvent::Switch => PhaseEnd::Switch,
        };
        if let Some((mut session, reply)) = state.take() {
            let next = session.on_end(end, Instant::now());
            advance(session, next, reply);
        }
    });
    if started {
        Ok(())
    } else {
        Err(Refusal::Busy("another overlay is open".into()))
    }
}

fn open_area(mut session: Session, timeout: Duration, reply: PickReply) -> Result<(), Refusal> {
    let allow_switch = session.allow_switch();
    let started = bugle_platform::select_rect_begin_ex(allow_switch, Some(timeout), move |out| {
        let end = match out {
            SelectOutcome::Selected(r) => PhaseEnd::Selected(r),
            SelectOutcome::Cancelled => PhaseEnd::Cancelled,
            SelectOutcome::Switch => PhaseEnd::Switch,
        };
        let next = session.on_end(end, Instant::now());
        advance(session, next, reply);
    });
    if started {
        Ok(())
    } else {
        Err(Refusal::Busy("another overlay is open".into()))
    }
}

/// Main thread: act on the open overlay as Space or Esc would.
pub fn control(c: Control) {
    match c {
        Control::Switch if bugle_platform::window_pick_active() => bugle_platform::window_pick_switch(),
        Control::Switch => bugle_platform::select_rect_switch(),
        Control::Cancel if bugle_platform::window_pick_active() => bugle_platform::window_pick_cancel(),
        Control::Cancel => bugle_platform::select_rect_cancel(),
    }
}

/// How long [`run_blocking`] handles events before it looks for the answer.
const PUMP_SLICE: Duration = Duration::from_millis(50);

/// Main thread of a process without a running AppKit loop (the CLI): run a
/// pick session and handle events until it ends. `Ok(None)` means the user
/// cancelled. A picked window comes back after [`SETTLE`]. Off the main
/// thread this refuses with [`BugleError::NoUiHost`] before showing anything.
pub fn run_blocking(start: Mode, allow_switch: bool, timeout: Duration) -> Result<Option<Picked>> {
    if !bugle_platform::is_main_thread() {
        return Err(BugleError::NoUiHost("the window picker runs on the main thread only".into()).into());
    }
    bugle_platform::app_bootstrap();
    let (tx, rx) = mpsc::channel();
    let now = Instant::now();
    // Refusals are answered on `tx` before `begin` returns.
    begin(Request { start, allow_switch, timeout, deadline: now + START_WINDOW, reply: tx });
    // Every overlay closes itself; the slack covers the final callback.
    let give_up = now + session_limit(allow_switch, timeout) + Duration::from_secs(2);
    let picked = loop {
        match rx.try_recv() {
            Ok(Ok(p)) => break p,
            Ok(Err(why)) => return Err(why.into_error()),
            Err(TryRecvError::Disconnected) => return Err(anyhow!("window picker request dropped")),
            Err(TryRecvError::Empty) if Instant::now() > give_up => {
                control(Control::Cancel);
                return Err(anyhow!("window picker did not answer"));
            }
            Err(TryRecvError::Empty) => bugle_platform::run_loop_once(PUMP_SLICE),
        }
    };
    // Let the window server take the overlay down before anything is grabbed.
    bugle_platform::run_loop_once(Duration::from_millis(20));
    if let Some(Picked::Window(_)) = picked {
        std::thread::sleep(SETTLE);
    }
    Ok(picked)
}

/// The part of `frame` on the display it overlaps most, for callers that
/// need on-screen pixels of a picked window (scrolling capture). `None`
/// when it is on no display; an empty `displays` leaves `frame` as is.
pub fn visible_frame(frame: Rect, displays: &[Rect]) -> Option<Rect> {
    if displays.is_empty() {
        return Some(frame);
    }
    displays.iter().filter_map(|d| d.intersection(&frame)).max_by(|a, b| (a.w * a.h).total_cmp(&(b.w * b.h)))
}

/// A picked window or area as the rect to scroll-capture: the window's
/// on-screen frame, or the area itself.
pub fn picked_rect(picked: &Picked, displays: &[Rect]) -> Option<Rect> {
    match picked {
        Picked::Window(w) => visible_frame(w.frame, displays),
        Picked::Area(r) => Some(*r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u32, pid: i32, x: f64, y: f64, w: f64, h: f64) -> WindowInfo {
        WindowInfo {
            id,
            pid,
            owner: format!("App{pid}"),
            title: format!("Window {id}"),
            frame: Rect::new(x, y, w, h),
            layer: 0,
            on_screen: true,
            alpha: 1.0,
        }
    }

    fn filter() -> PickFilter {
        PickFilter { own_pid: 1, displays: vec![Rect::new(0.0, 0.0, 1440.0, 900.0), Rect::new(1440.0, 0.0, 1920.0, 1080.0)] }
    }

    fn at(ws: &[WindowInfo], x: f64, y: f64) -> Option<u32> {
        window_at(ws, Point { x, y }, &filter()).map(|w| w.id)
    }

    #[test]
    fn front_most_window_containing_the_point_wins() {
        let ws = [win(10, 2, 100.0, 100.0, 400.0, 300.0), win(20, 3, 0.0, 0.0, 1000.0, 800.0)];
        assert_eq!(at(&ws, 150.0, 150.0), Some(10));
        assert_eq!(at(&ws, 50.0, 50.0), Some(20));
        assert_eq!(at(&ws, 1200.0, 50.0), None);
    }

    #[test]
    fn edges_are_half_open() {
        let ws = [win(10, 2, 100.0, 100.0, 400.0, 300.0)];
        assert_eq!(at(&ws, 100.0, 100.0), Some(10));
        assert_eq!(at(&ws, 499.9, 399.9), Some(10));
        assert_eq!(at(&ws, 500.0, 200.0), None);
        assert_eq!(at(&ws, 200.0, 400.0), None);
    }

    #[test]
    fn skips_own_overlay_and_non_application_windows() {
        let mut overlay = win(1, 1, 0.0, 0.0, 1440.0, 900.0); // Bugle's own pid
        overlay.layer = 1000;
        let own_toast = win(2, 1, 100.0, 100.0, 300.0, 80.0);
        let mut menu_bar = win(3, 4, 0.0, 0.0, 1440.0, 24.0);
        menu_bar.layer = 25;
        let mut hidden = win(4, 5, 0.0, 0.0, 800.0, 600.0);
        hidden.on_screen = false;
        let mut ownerless = win(5, 6, 0.0, 0.0, 800.0, 600.0);
        ownerless.owner.clear();
        let target = win(6, 7, 0.0, 0.0, 800.0, 600.0);
        let ws = [overlay, own_toast, menu_bar, hidden, ownerless, target];
        assert_eq!(at(&ws, 150.0, 120.0), Some(6));
        assert_eq!(at(&ws, 10.0, 10.0), Some(6));
    }

    #[test]
    fn skips_tiny_and_off_screen_windows() {
        let tiny = win(1, 2, 100.0, 100.0, 40.0, 400.0);
        let sliver = win(2, 2, 100.0, 100.0, 400.0, 10.0);
        let off = win(3, 2, -5000.0, -5000.0, 6000.0, 5100.0); // reaches on screen
        let far = win(4, 2, 9000.0, 9000.0, 400.0, 400.0);
        let bad = win(5, 2, f64::NAN, 0.0, 400.0, 400.0);
        let target = win(6, 2, 0.0, 0.0, 800.0, 600.0);
        let f = filter();
        assert!(!f.accepts(&tiny) && !f.accepts(&sliver) && !f.accepts(&far) && !f.accepts(&bad));
        assert!(f.accepts(&off), "a window partly on screen is pickable");
        let ws = [tiny, sliver, far.clone(), bad, target];
        assert_eq!(at(&ws, 110.0, 105.0), Some(6));
        assert_eq!(window_at(&[far], Point { x: 9100.0, y: 9100.0 }, &f).map(|w| w.id), None);
    }

    #[test]
    fn transparent_windows_are_not_picked() {
        let mut invisible = win(1, 2, 0.0, 0.0, 1440.0, 900.0); // full-screen, alpha 0, in front
        invisible.alpha = 0.0;
        let mut faint = win(2, 3, 100.0, 100.0, 400.0, 300.0);
        faint.alpha = 0.3;
        let target = win(3, 4, 0.0, 0.0, 800.0, 600.0);
        let ws = [invisible, faint, target];
        assert_eq!(at(&ws, 150.0, 150.0), Some(2), "translucent windows still count");
        assert_eq!(at(&ws, 50.0, 50.0), Some(3));
        assert_eq!(at(&ws, 1000.0, 800.0), None);
    }

    #[test]
    fn missing_alpha_deserializes_as_opaque() {
        let j = r#"{"id":1,"pid":2,"owner":"A","title":"","frame":{"x":0,"y":0,"w":100,"h":100},"layer":0,"on_screen":true}"#;
        let w: WindowInfo = serde_json::from_str(j).unwrap();
        assert_eq!(w.alpha, 1.0);
    }

    #[test]
    fn expired_requests_are_refused_with_a_reason() {
        for allow_switch in [false, true] {
            let (tx, rx) = mpsc::channel();
            let past = Instant::now() - Duration::from_secs(1);
            let req = Request { start: Mode::Window, allow_switch, timeout: Duration::from_secs(1), deadline: past, reply: tx };
            assert!(!begin(req));
            let got = rx.recv().unwrap();
            assert!(matches!(got, Err(Refusal::Busy(why)) if why.contains("expired")));
            assert!(!in_session());
        }
    }

    #[test]
    fn a_second_session_is_refused_and_keeps_the_running_flag() {
        IN_SESSION.with(|s| s.set(true));
        let (tx, rx) = mpsc::channel();
        let req = Request {
            start: Mode::Window,
            allow_switch: true,
            timeout: PICK_TIMEOUT,
            deadline: Instant::now() + START_WINDOW,
            reply: tx,
        };
        assert!(!begin(req));
        assert!(matches!(rx.recv().unwrap(), Err(Refusal::Busy(_))));
        assert!(in_session(), "the running session still owns the flag");
        IN_SESSION.with(|s| s.set(false));
    }

    #[test]
    fn refusals_map_to_typed_errors() {
        let busy = Refusal::Busy("another overlay is open".into()).into_error();
        assert!(matches!(busy.downcast_ref::<BugleError>(), Some(BugleError::Busy(_))), "{busy:#}");
        let failed = Refusal::Failed("could not list windows".into()).into_error();
        assert!(failed.downcast_ref::<BugleError>().is_none());
    }

    fn open(n: &Next) -> Option<(Mode, Duration)> {
        match n {
            Next::Open { mode, timeout } => Some((*mode, *timeout)),
            Next::Done(_) => None,
        }
    }

    fn done(n: Next) -> Option<Picked> {
        match n {
            Next::Done(p) => p,
            Next::Open { mode, .. } => panic!("still open in {mode:?}"),
        }
    }

    #[test]
    fn space_toggles_between_picker_and_area() {
        let t0 = Instant::now();
        let mut s = Session::new(Mode::Window, true, PICK_TIMEOUT, t0);
        assert_eq!(open(&s.start(t0)), Some((Mode::Window, PICK_TIMEOUT)));
        // Picker → area: the area phase gets the rest of the session.
        let n = s.on_end(PhaseEnd::Switch, t0 + Duration::from_secs(5));
        assert_eq!(open(&n), Some((Mode::Area, SESSION_LIMIT - Duration::from_secs(5))));
        assert_eq!(s.mode(), Mode::Area);
        // Area → picker: the picker is capped at its own timeout again.
        let n = s.on_end(PhaseEnd::Switch, t0 + Duration::from_secs(10));
        assert_eq!(open(&n), Some((Mode::Window, PICK_TIMEOUT)));
        assert_eq!(s.switches(), 2);
        // A click ends the session with that window.
        let picked = done(s.on_end(PhaseEnd::Clicked(Some(win(7, 2, 0.0, 0.0, 400.0, 300.0))), t0 + Duration::from_secs(11)));
        assert!(matches!(picked, Some(Picked::Window(w)) if w.id == 7));
    }

    #[test]
    fn area_selection_can_start_the_session() {
        let t0 = Instant::now();
        let mut s = Session::new(Mode::Area, true, PICK_TIMEOUT, t0);
        assert_eq!(open(&s.start(t0)), Some((Mode::Area, SESSION_LIMIT)));
        let picked = done(s.on_end(PhaseEnd::Selected(Rect::new(1.0, 2.0, 30.0, 40.0)), t0));
        assert!(matches!(picked, Some(Picked::Area(r)) if r == Rect::new(1.0, 2.0, 30.0, 40.0)));
        let mut s = Session::new(Mode::Area, true, PICK_TIMEOUT, t0);
        assert_eq!(open(&s.on_end(PhaseEnd::Switch, t0)).map(|o| o.0), Some(Mode::Window));
    }

    #[test]
    fn cancel_and_empty_clicks_end_the_session() {
        let t0 = Instant::now();
        for start in [Mode::Window, Mode::Area] {
            let mut s = Session::new(start, true, PICK_TIMEOUT, t0);
            assert!(done(s.on_end(PhaseEnd::Cancelled, t0)).is_none());
        }
        let mut s = Session::new(Mode::Window, true, PICK_TIMEOUT, t0);
        assert!(done(s.on_end(PhaseEnd::Clicked(None), t0)).is_none(), "a click outside every window cancels");
    }

    #[test]
    fn without_switching_space_is_a_cancel() {
        let t0 = Instant::now();
        let mut s = Session::new(Mode::Window, false, Duration::from_millis(1500), t0);
        assert_eq!(open(&s.start(t0)), Some((Mode::Window, Duration::from_millis(1500))));
        assert!(done(s.on_end(PhaseEnd::Switch, t0)).is_none());
        assert_eq!(s.switches(), 0);
        assert_eq!(session_limit(false, PICK_TIMEOUT), PICK_TIMEOUT);
        assert_eq!(session_limit(true, PICK_TIMEOUT), SESSION_LIMIT);
    }

    #[test]
    fn toggling_cannot_outlast_the_session() {
        let t0 = Instant::now();
        let mut s = Session::new(Mode::Window, true, PICK_TIMEOUT, t0);
        // Near the end, the picker only gets what is left.
        let n = s.on_end(PhaseEnd::Switch, t0 + SESSION_LIMIT - Duration::from_secs(20));
        assert_eq!(open(&n), Some((Mode::Area, Duration::from_secs(20))));
        let n = s.on_end(PhaseEnd::Switch, t0 + SESSION_LIMIT - Duration::from_secs(4));
        assert_eq!(open(&n), Some((Mode::Window, Duration::from_secs(4))));
        // Past the limit, a switch closes the session instead of reopening.
        assert!(done(s.on_end(PhaseEnd::Switch, t0 + SESSION_LIMIT)).is_none());
    }

    #[test]
    fn second_display_and_negative_coordinates() {
        let mut f = filter();
        f.displays.push(Rect::new(-1280.0, 0.0, 1280.0, 800.0));
        let left = win(1, 2, -1000.0, 100.0, 600.0, 400.0);
        let right = win(2, 2, 1600.0, 100.0, 600.0, 400.0);
        let ws = [left, right];
        assert_eq!(window_at(&ws, Point { x: -900.0, y: 200.0 }, &f).map(|w| w.id), Some(1));
        assert_eq!(window_at(&ws, Point { x: 1700.0, y: 200.0 }, &f).map(|w| w.id), Some(2));
    }

    #[test]
    fn empty_display_list_disables_the_off_screen_check() {
        let f = PickFilter { own_pid: 1, displays: vec![] };
        let far = win(4, 2, 9000.0, 9000.0, 400.0, 400.0);
        assert!(f.accepts(&far));
    }

    #[test]
    fn labels() {
        let mut w = win(1, 2, 0.0, 0.0, 800.0, 600.0);
        assert_eq!(label(&w), "App2 — Window 1  800 × 600");
        w.title.clear();
        assert_eq!(label(&w), "App2  800 × 600");
    }

    #[test]
    fn a_picked_window_scrolls_its_on_screen_part() {
        let displays = [Rect::new(0.0, 0.0, 1440.0, 900.0), Rect::new(1440.0, 0.0, 1920.0, 1080.0)];
        // Hanging off the bottom of the main display.
        let low = win(1, 2, 100.0, 600.0, 800.0, 600.0);
        assert_eq!(picked_rect(&Picked::Window(low), &displays), Some(Rect::new(100.0, 600.0, 800.0, 300.0)));
        // Across two displays: the larger part wins.
        let across = win(2, 2, 1340.0, 100.0, 600.0, 400.0);
        assert_eq!(visible_frame(across.frame, &displays), Some(Rect::new(1440.0, 100.0, 500.0, 400.0)));
        // Off every display.
        assert_eq!(visible_frame(Rect::new(9000.0, 9000.0, 400.0, 400.0), &displays), None);
        // No display list: the frame as is; an area is never touched.
        assert_eq!(visible_frame(across.frame, &[]), Some(across.frame));
        let area = Rect::new(-50.0, -50.0, 100.0, 100.0);
        assert_eq!(picked_rect(&Picked::Area(area), &displays), Some(area));
    }

    #[test]
    fn run_blocking_refuses_off_the_main_thread() {
        // Test threads are never the process main thread.
        let e = run_blocking(Mode::Window, false, Duration::from_secs(1)).unwrap_err();
        assert!(matches!(e.downcast_ref::<BugleError>(), Some(BugleError::NoUiHost(_))), "{e:#}");
        assert!(!in_session());
    }
}
