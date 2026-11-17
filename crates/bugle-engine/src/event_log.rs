//! Records a recording's pointer/click event track (`bugle_core::events`).
//!
//! A thread polls the pointer position and the mouse-button state
//! ([`POLL_HZ`]; no permission needed, no events posted or intercepted) and
//! logs a `move` when the pointer moved and a `down`/`up` when a button
//! changed. A press shorter than one poll interval can be missed. Events are
//! stamped on the host clock (the clock ScreenCaptureKit stamps frames with)
//! and placed on the video's timeline when the recording is finalised, by a
//! [`VideoClock`] that says where the video's time 0 is:
//!
//! * the stream recorder reports the host time its movie starts at;
//! * the GIF recorder stamps every frame it grabs, so frame `k` (shown from
//!   `k / fps`) maps to the moment it was grabbed, even when grabs fall
//!   behind the frame rate;
//! * the system recorder (`screencapture -v`, a child process, only used
//!   for event tracks when the stream recorder is unavailable or
//!   `backend = "system"`) reports nothing. Its movie ends about when it is
//!   interrupted, or when it exits by itself at its deadline (the log thread
//!   watches for that; a child its owner already reaped counts as exited),
//!   so the start is taken as that end minus the video track's duration.
//!   That is an estimate: a self-stopped child's exit also includes its own
//!   flush.
//!
//! The log stops before the file is finalised, and none of this depends on
//! how long finalising takes. Events before
//! the video starts collapse into the starting position; events after it
//! ends are dropped.
//!
//! For a window recording the pointer is logged relative to the window's
//! current origin (polled with it), since the recorders follow the window;
//! a resize sets `area_changed`, after which positions no longer map.

use bugle_core::events::{CursorEvent, EventKind, EventTrack, MouseButton};
use bugle_core::Rect;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const POLL_HZ: f64 = 60.0;
/// Pointer moves shorter than this (points) are not logged.
const MIN_MOVE: f64 = 0.25;
/// A window counts as resized when a side changes by more than this (points).
const RESIZE_TOLERANCE: f64 = 1.0;

const BUTTONS: [(u32, MouseButton); 3] = [(1, MouseButton::Left), (2, MouseButton::Right), (4, MouseButton::Middle)];

/// Turns polled samples into events.
#[derive(Default)]
pub struct Sampler {
    last: Option<(f64, f64)>,
    buttons: u32,
    pub events: Vec<CursorEvent>,
}

impl Sampler {
    /// Feed one poll: time (s), position (area points), button mask.
    pub fn feed(&mut self, t: f64, x: f64, y: f64, buttons: u32) {
        let moved = match self.last {
            None => true,
            Some((lx, ly)) => (x - lx).abs() >= MIN_MOVE || (y - ly).abs() >= MIN_MOVE,
        };
        if moved {
            self.events.push(CursorEvent::moved(t, x, y));
            self.last = Some((x, y));
        }
        for (bit, button) in BUTTONS {
            let (was, is) = (self.buttons & bit != 0, buttons & bit != 0);
            if was != is {
                let kind = if is { EventKind::Down } else { EventKind::Up };
                self.events.push(CursorEvent { t, kind, button: Some(button), x, y });
            }
        }
        self.buttons = buttons;
    }
}

/// Where the video's time 0 is on the host clock.
#[derive(Clone)]
pub enum VideoClock {
    /// Time 0 is this host time (stream recorder).
    StartsAt(f64),
    /// Frame `k`, shown from `k / fps`, was grabbed at host time `times[k]`
    /// (GIF recorder).
    Frames { times: Arc<Mutex<Vec<f64>>>, fps: f64 },
    /// The video ends at the stop time given to [`EventLog::finish`], or
    /// when the watched child exited by itself (system recorder).
    EndAligned,
}

/// Video time of host time `t` for frames grabbed at `times` (sorted),
/// shown at `fps`: piecewise linear between grabs, negative before the
/// first one.
pub fn frames_time(t: f64, times: &[f64], fps: f64) -> f64 {
    let Some(&first) = times.first() else { return t };
    if t < first {
        return t - first;
    }
    let k = times.partition_point(|&g| g <= t) - 1;
    match times.get(k + 1) {
        Some(&next) if next > times[k] => (k as f64 + ((t - times[k]) / (next - times[k])).clamp(0.0, 1.0)) / fps,
        _ => k as f64 / fps + (t - times[k]),
    }
}

/// Put events (already on the video's timeline) into `0..=duration`: the
/// position they leave the pointer at before 0 becomes a `move` at 0, the
/// first `move` at the start is placed at exactly 0, and events after the
/// end are dropped. A button released after the start but pressed before it
/// keeps its `up` only.
pub fn place_events(events: Vec<CursorEvent>, duration: f64) -> Vec<CursorEvent> {
    let mut out = Vec::with_capacity(events.len());
    let mut before: Option<(f64, f64)> = None;
    for e in events {
        if e.t < 0.0 {
            before = Some((e.x, e.y));
            continue;
        }
        if e.t > duration {
            break;
        }
        if let Some((x, y)) = before.take() {
            if !(e.t == 0.0 && e.kind == EventKind::Move) {
                out.push(CursorEvent::moved(0.0, x, y));
            }
        }
        out.push(e);
    }
    if let Some((x, y)) = before {
        out.push(CursorEvent::moved(0.0, x, y));
    }
    // The first sample is where the pointer was when the video started.
    if let Some(first) = out.first_mut().filter(|e| e.kind == EventKind::Move && e.t < 0.1) {
        first.t = 0.0;
    }
    out
}

/// What the log thread hands back.
#[derive(Default)]
struct Logged {
    started: f64,
    /// When the log stopped polling.
    stopped: f64,
    events: Vec<CursorEvent>,
    child_exited_at: Option<f64>,
    area_changed: bool,
}

/// A running event log. Dropping it stops the thread.
pub struct EventLog {
    area: Rect,
    cursor_in_video: bool,
    fps: Option<f64>,
    clock: VideoClock,
    stop: Arc<AtomicBool>,
    child: Arc<AtomicU32>,
    handle: Option<std::thread::JoinHandle<Logged>>,
    /// What the thread logged, once stopped.
    logged: Option<Logged>,
}

impl EventLog {
    /// Start logging the pointer relative to `area` (global points), or to
    /// window `window`'s current origin. Runs until finished or dropped.
    pub fn start(area: Rect, window: Option<u32>) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(AtomicU32::new(0));
        let (stop2, child2) = (stop.clone(), child.clone());
        let handle = std::thread::Builder::new().name("bugle-events".into()).spawn(move || {
            let interval = Duration::from_secs_f64(1.0 / POLL_HZ);
            let mut next = Instant::now();
            let mut log = Logged { started: bugle_platform::host_time_now(), ..Default::default() };
            let mut s = Sampler::default();
            let mut origin = (area.x, area.y);
            let mut polls: u64 = 0;
            while !stop2.load(Ordering::Relaxed) {
                if let Some(id) = window.filter(|_| polls.is_multiple_of(2)) {
                    if let Some(f) = bugle_platform::window_frame(id) {
                        origin = (f.x, f.y);
                        if (f.w - area.w).abs() > RESIZE_TOLERANCE || (f.h - area.h).abs() > RESIZE_TOLERANCE {
                            log.area_changed = true;
                        }
                    }
                }
                polls += 1;
                let t = bugle_platform::host_time_now();
                let p = bugle_platform::cursor_position();
                s.feed(t, p.x - origin.0, p.y - origin.1, bugle_platform::mouse_buttons());
                let pid = child2.load(Ordering::Relaxed);
                if pid != 0 && log.child_exited_at.is_none() && bugle_platform::child_exited(pid) {
                    log.child_exited_at = Some(t);
                }
                next += interval;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    next = now;
                }
            }
            log.stopped = bugle_platform::host_time_now();
            log.events = s.events;
            log
        })?;
        Ok(Self { area, cursor_in_video: true, fps: None, clock: VideoClock::EndAligned, stop, child, handle: Some(handle), logged: None })
    }

    /// Say how the video is timed, whether its frames show the pointer and
    /// its frame rate (known once the recorder runs).
    pub fn attach(&mut self, clock: VideoClock, cursor_in_video: bool, fps: Option<f64>) {
        self.clock = clock;
        self.cursor_in_video = cursor_in_video;
        self.fps = fps;
    }

    /// Note when child `pid` (the system recorder) exits by itself.
    pub fn watch_child(&self, pid: u32) {
        self.child.store(pid, Ordering::Relaxed);
    }

    /// Stop polling (before the recording is finalised, which can take a
    /// while). Idempotent.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            self.logged = Some(h.join().unwrap_or_default());
        }
    }

    /// Stop logging (if not yet) and build the track for a video whose track
    /// lasts `video_seconds`. `stopped_at` is when the system recorder was
    /// interrupted, or seen to exit after the log stopped (only used for
    /// [`VideoClock::EndAligned`]).
    pub fn finish(mut self, video_seconds: f64, stopped_at: Option<f64>) -> EventTrack {
        self.stop();
        let log = self.logged.take().unwrap_or_default();
        let (to_video, duration): (Box<dyn Fn(f64) -> f64>, f64) = match &self.clock {
            VideoClock::StartsAt(s) => {
                let s = *s;
                (Box::new(move |t| t - s), video_seconds)
            }
            VideoClock::Frames { times, fps } => {
                let times = times.lock().map(|v| v.clone()).unwrap_or_default();
                tracing::trace!("event track: GIF frames grabbed at host times {times:?}");
                let (fps, n) = (*fps, times.len());
                (Box::new(move |t| frames_time(t, &times, fps)), n as f64 / fps)
            }
            VideoClock::EndAligned => {
                // Interrupted: the video ends then; otherwise it ended when
                // the child exited by itself, as the log saw (to within a
                // poll, plus the child's own flush). Neither: it exited
                // between the log's last poll and the stop.
                let end = stopped_at.or(log.child_exited_at).unwrap_or(log.stopped);
                let start = end - video_seconds;
                (Box::new(move |t| t - start), video_seconds)
            }
        };
        tracing::debug!("event track: host time {:.4} is video time {:.4} (log started then)", log.started, to_video(log.started));
        let events = log.events.into_iter().map(|e| CursorEvent { t: to_video(e.t), ..e }).collect();
        let mut track = EventTrack::new(self.area, self.cursor_in_video, duration, place_events(events, duration));
        track.fps = self.fps;
        track.area_changed = log.area_changed;
        track
    }
}

impl Drop for EventLog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampler_logs_changes_only() {
        let mut s = Sampler::default();
        s.feed(0.0, 10.0, 10.0, 0);
        s.feed(0.1, 10.1, 10.0, 0); // under MIN_MOVE: nothing
        s.feed(0.2, 12.0, 10.0, 0);
        s.feed(0.3, 12.0, 10.0, 1); // left down
        s.feed(0.4, 12.0, 10.0, 1);
        s.feed(0.5, 12.0, 10.0, 2); // left up, right down at once
        s.feed(0.6, 12.0, 10.0, 0);
        let kinds: Vec<(f64, EventKind, Option<MouseButton>)> = s.events.iter().map(|e| (e.t, e.kind, e.button)).collect();
        assert_eq!(
            kinds,
            vec![
                (0.0, EventKind::Move, None),
                (0.2, EventKind::Move, None),
                (0.3, EventKind::Down, Some(MouseButton::Left)),
                (0.5, EventKind::Up, Some(MouseButton::Left)),
                (0.5, EventKind::Down, Some(MouseButton::Right)),
                (0.6, EventKind::Up, Some(MouseButton::Right)),
            ]
        );
        assert_eq!((s.events[2].x, s.events[2].y), (12.0, 10.0));
    }

    #[test]
    fn frames_map_grab_times_onto_the_frame_timeline() {
        // 10 fps, but grabs fell behind: 0.0, 0.1, 0.3, 0.6.
        let times = [100.0, 100.1, 100.3, 100.6];
        let v = |t| frames_time(t, &times, 10.0);
        assert!((v(100.0) - 0.0).abs() < 1e-9);
        assert!((v(100.2) - 0.15).abs() < 1e-9, "halfway between grabs 1 and 2");
        assert!((v(100.6) - 0.3).abs() < 1e-9);
        assert!((v(99.5) + 0.5).abs() < 1e-9, "before the first grab: negative");
        assert!((v(100.7) - 0.4).abs() < 1e-9, "after the last: real time");
        assert_eq!(frames_time(5.0, &[], 10.0), 5.0);
    }

    #[test]
    fn placing_keeps_the_starting_position_and_cuts_the_end() {
        let ev = vec![
            CursorEvent::moved(-0.5, 1.0, 1.0),
            CursorEvent::moved(-0.1, 2.0, 2.0),
            CursorEvent::moved(0.2, 3.0, 3.0),
            CursorEvent::moved(1.5, 4.0, 4.0),
        ];
        assert_eq!(place_events(ev.clone(), 1.0), vec![CursorEvent::moved(0.0, 2.0, 2.0), CursorEvent::moved(0.2, 3.0, 3.0)]);
        // Everything before 0: the last position remains.
        assert_eq!(place_events(ev[..2].to_vec(), 1.0), vec![CursorEvent::moved(0.0, 2.0, 2.0)]);
        // A first sample just after 0 is moved onto 0; one on 0 needs no seed.
        assert_eq!(place_events(vec![CursorEvent::moved(0.02, 5.0, 5.0)], 1.0), vec![CursorEvent::moved(0.0, 5.0, 5.0)]);
        let ev = vec![CursorEvent::moved(-0.2, 1.0, 1.0), CursorEvent::moved(0.0, 2.0, 2.0)];
        assert_eq!(place_events(ev, 1.0), vec![CursorEvent::moved(0.0, 2.0, 2.0)]);
    }

    fn fake_log(events: Vec<CursorEvent>, clock: VideoClock, exited: Option<f64>) -> EventLog {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = std::thread::spawn(move || Logged { started: 1000.0, stopped: 1010.0, events, child_exited_at: exited, area_changed: false });
        EventLog {
            area: Rect::new(0.0, 0.0, 10.0, 10.0),
            cursor_in_video: true,
            fps: Some(30.0),
            clock,
            stop,
            child: Default::default(),
            handle: Some(handle),
            logged: None,
        }
    }

    #[test]
    fn clocks_place_events_independently_of_finalising_time() {
        let down = |t| CursorEvent { t, kind: EventKind::Down, button: Some(MouseButton::Left), x: 1.0, y: 1.0 };
        let ev = || vec![CursorEvent::moved(1000.0, 0.0, 0.0), down(1001.5)];
        // Stream recorder: the movie starts 0.25 s after the log.
        let t = fake_log(ev(), VideoClock::StartsAt(1000.25), None).finish(3.0, Some(1003.25));
        assert_eq!(t.clicks().map(|c| c.0).collect::<Vec<_>>(), vec![1.25]);
        assert_eq!(t.fps, Some(30.0));
        // System recorder stopped at 1003.4 with a 3.2 s video: starts at 1000.2.
        let t = fake_log(ev(), VideoClock::EndAligned, None).finish(3.2, Some(1003.4));
        assert!((t.clicks().next().unwrap().0 - 1.3).abs() < 1e-9);
        // ... or it exited by itself at its deadline (no stop time).
        let t = fake_log(ev(), VideoClock::EndAligned, Some(1002.2)).finish(2.0, None);
        assert!((t.clicks().next().unwrap().0 - 1.3).abs() < 1e-9);
        // GIF: frames grabbed every 0.1 s from 1000.5 at 10 fps.
        let times = Arc::new(Mutex::new((0..20).map(|k| 1000.5 + k as f64 * 0.1).collect::<Vec<_>>()));
        let t = fake_log(ev(), VideoClock::Frames { times, fps: 10.0 }, None).finish(99.0, None);
        assert!((t.duration - 2.0).abs() < 1e-9, "20 frames at 10 fps");
        assert!((t.clicks().next().unwrap().0 - 1.0).abs() < 1e-9);
        assert_eq!(t.events[0], CursorEvent::moved(0.0, 0.0, 0.0));
    }

    #[test]
    fn a_child_reaped_before_the_log_polls_still_counts_as_exited() {
        let mut log = EventLog::start(Rect::new(0.0, 0.0, 10.0, 10.0), None).unwrap();
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        // The owner reaps it (as `record_status` does) before the log sees it.
        child.wait().unwrap();
        let reaped = bugle_platform::host_time_now();
        log.watch_child(child.id());
        std::thread::sleep(Duration::from_millis(100));
        log.stop();
        let at = log.logged.as_ref().unwrap().child_exited_at.expect("exit noticed");
        // The poll that notices the exit stamps it with the time it sampled
        // the pointer, which can be a moment before `reaped` was read here.
        assert!((at - reaped).abs() < 0.1, "{}", at - reaped);
        // So the video ends there, not at the start of the log. Keep the
        // events in time order: the pointer may have moved while this ran.
        let down = CursorEvent { t: at - 0.5, kind: EventKind::Down, button: Some(MouseButton::Left), x: 1.0, y: 1.0 };
        let events = &mut log.logged.as_mut().unwrap().events;
        events.retain(|e| e.t < down.t);
        events.push(down);
        let t = log.finish(2.0, None);
        assert!((t.clicks().next().unwrap().0 - 1.5).abs() < 1e-9);
    }

    #[test]
    fn log_runs_and_stops() {
        let log = EventLog::start(Rect::new(0.0, 0.0, 100.0, 100.0), None).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        let now = bugle_platform::host_time_now();
        let t = log.finish(0.05, Some(now));
        assert_eq!(t.duration, 0.05);
        assert_eq!(t.events.first().map(|e| e.t), Some(0.0), "starts with the pointer position");
        assert!(t.events.iter().all(|e| e.t <= 0.05));
    }
}
