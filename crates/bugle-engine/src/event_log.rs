//! Records a recording's pointer/click event track (`bugle_core::events`).
//!
//! A thread polls the pointer position and the mouse-button state
//! ([`POLL_HZ`]; no permission needed, no events posted or intercepted) and
//! logs a `move` when the pointer moved and a `down`/`up` when a button
//! changed. A press shorter than one poll interval can be missed.
//!
//! The log starts just before the recorder does, and the recorder's own
//! startup delay is unknown, so the track is aligned on the *end*: the video
//! ends when the recording is stopped, which is when the log stops too (or
//! at `max_seconds`). The difference between the log's length and the
//! video's is how much later the video started; event times are shifted by
//! it (at most [`MAX_ALIGN_SHIFT`]).

use bugle_core::events::{CursorEvent, EventKind, EventTrack, MouseButton};
use bugle_core::Rect;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const POLL_HZ: f64 = 60.0;
/// Pointer moves shorter than this (points) are not logged.
const MIN_MOVE: f64 = 0.25;
/// Largest start offset the end alignment trusts.
pub const MAX_ALIGN_SHIFT: f64 = 2.0;

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

/// How far to shift a log of `log_seconds` onto a video lasting
/// `video_seconds` that ended when the log did.
pub fn align_shift(log_seconds: f64, video_seconds: f64) -> f64 {
    let s = log_seconds - video_seconds;
    if s.is_finite() && (0.0..=MAX_ALIGN_SHIFT).contains(&s) {
        s
    } else {
        0.0
    }
}

/// Move events `shift` seconds earlier. Events before 0 are dropped, but the
/// position they leave the pointer at is kept as a `move` at 0.
pub fn shift_events(events: Vec<CursorEvent>, shift: f64) -> Vec<CursorEvent> {
    let mut out = Vec::with_capacity(events.len());
    let mut before: Option<(f64, f64)> = None;
    for e in events {
        let t = e.t - shift;
        if t < 0.0 {
            before = Some((e.x, e.y));
        } else {
            if let Some((x, y)) = before.take() {
                if t > 0.0 {
                    out.push(CursorEvent::moved(0.0, x, y));
                }
            }
            out.push(CursorEvent { t, ..e });
        }
    }
    if let Some((x, y)) = before {
        out.push(CursorEvent::moved(0.0, x, y));
    }
    out
}

/// A running event log. Dropping it stops the thread.
pub struct EventLog {
    area: Rect,
    cursor_in_video: bool,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<(Vec<CursorEvent>, f64)>>,
}

impl EventLog {
    /// Start logging the pointer relative to `area` (global points). Stops
    /// by itself after `max_seconds`.
    pub fn start(area: Rect, cursor_in_video: bool, max_seconds: Option<f64>) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::Builder::new().name("bugle-events".into()).spawn(move || {
            let t0 = Instant::now();
            let interval = Duration::from_secs_f64(1.0 / POLL_HZ);
            let mut next = t0;
            let mut s = Sampler::default();
            loop {
                let t = t0.elapsed().as_secs_f64();
                if let Some(m) = max_seconds.filter(|m| *m > 0.0 && t >= *m) {
                    return (s.events, m);
                }
                if stop2.load(Ordering::Relaxed) {
                    return (s.events, t);
                }
                let p = bugle_platform::cursor_position();
                s.feed(t, p.x - area.x, p.y - area.y, bugle_platform::mouse_buttons());
                next += interval;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    next = now;
                }
            }
        })?;
        Ok(Self { area, cursor_in_video, stop, handle: Some(handle) })
    }

    /// Record whether the frames show the pointer (known once the recorder
    /// is chosen).
    pub fn set_cursor_in_video(&mut self, shown: bool) {
        self.cursor_in_video = shown;
    }

    /// Stop logging and build the track for a video lasting `video_seconds`.
    pub fn finish(mut self, video_seconds: f64) -> EventTrack {
        self.stop.store(true, Ordering::Relaxed);
        let (events, log_seconds) = self.handle.take().and_then(|h| h.join().ok()).unwrap_or_default();
        let shift = align_shift(log_seconds, video_seconds);
        let mut events = shift_events(events, shift);
        // The first sample is where the pointer was when the video started.
        if let Some(first) = events.first_mut().filter(|e| e.kind == EventKind::Move) {
            first.t = 0.0;
        }
        EventTrack::new(self.area, self.cursor_in_video, video_seconds, events)
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
    fn alignment_trusts_small_positive_offsets_only() {
        assert_eq!(align_shift(10.3, 10.0), 10.3 - 10.0);
        assert_eq!(align_shift(10.0, 10.2), 0.0, "video longer than the log");
        assert_eq!(align_shift(30.0, 10.0), 0.0, "implausible");
        assert_eq!(align_shift(f64::NAN, 1.0), 0.0);
    }

    #[test]
    fn shifting_keeps_the_starting_position() {
        let ev = vec![CursorEvent::moved(0.0, 1.0, 1.0), CursorEvent::moved(0.2, 2.0, 2.0), CursorEvent::moved(0.5, 3.0, 3.0)];
        let out = shift_events(ev.clone(), 0.3);
        assert_eq!(out, vec![CursorEvent::moved(0.0, 2.0, 2.0), CursorEvent::moved(0.2, 3.0, 3.0)]);
        assert_eq!(shift_events(ev.clone(), 0.0), ev);
        // Everything before 0: the last position remains.
        assert_eq!(shift_events(ev, 1.0), vec![CursorEvent::moved(0.0, 3.0, 3.0)]);
        // An event landing exactly on 0 needs no seed.
        let ev = vec![CursorEvent::moved(0.0, 1.0, 1.0), CursorEvent::moved(0.5, 2.0, 2.0)];
        assert_eq!(shift_events(ev, 0.5), vec![CursorEvent::moved(0.0, 2.0, 2.0)]);
    }

    #[test]
    fn log_runs_and_stops() {
        let log = EventLog::start(Rect::new(0.0, 0.0, 100.0, 100.0), false, None).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        let t = log.finish(0.05);
        assert!(!t.cursor_in_video);
        assert_eq!(t.duration, 0.05);
        assert_eq!(t.events.first().map(|e| e.t), Some(0.0), "starts with the pointer position");
        // Auto-stop at max_seconds.
        let log = EventLog::start(Rect::new(0.0, 0.0, 10.0, 10.0), true, Some(0.05)).unwrap();
        std::thread::sleep(Duration::from_millis(120));
        let t0 = Instant::now();
        let t = log.finish(0.05);
        assert!(t0.elapsed() < Duration::from_millis(50), "already stopped");
        assert!(t.events.iter().all(|e| e.t <= 0.05));
    }
}
