//! Cursor and click event tracks: the sidecar a recording made with
//! `cursor_events` gets next to its video, named after the whole file name
//! (`clip.mp4` → `clip.mp4.events.json`, so `clip.mov` beside it has its own).
//!
//! The file is one JSON object:
//!
//! ```json
//! {
//!   "format": "bugle-events",
//!   "version": 1,
//!   "area": {"x": 0, "y": 30, "w": 1280, "h": 720},
//!   "cursor_in_video": true,
//!   "duration": 12.4,
//!   "fps": 30,
//!   "events": [
//!     {"t": 0.0,  "type": "move", "x": 640.0, "y": 360.0},
//!     {"t": 1.52, "type": "down", "button": "left", "x": 702.5, "y": 118.0},
//!     {"t": 1.61, "type": "up",   "button": "left", "x": 702.5, "y": 118.0}
//!   ]
//! }
//! ```
//!
//! * `area` is the recorded area in global screen points (top-left origin);
//!   for a window recording, the window's frame when the recording started.
//! * `x`/`y` are points relative to the area's top-left corner (for a window
//!   recording, to the window's top-left corner at that moment, so a window
//!   that moves keeps the mapping). They can lie outside `0..w` / `0..h` when
//!   the pointer leaves the area.
//! * `t` is seconds on the video's timeline (0 = first frame).
//! * `type` is `move`, `down` or `up`; `button` (`left`, `right`, `middle`)
//!   is given for `down`/`up`. Readers ignore unknown event types and fields.
//! * `cursor_in_video` says whether the frames show the pointer (false for
//!   recordings made with `hide_cursor`, whose edits draw it back).
//! * `fps` (optional) is the frame rate the recording was made at.
//! * `area_changed` (optional, default false) is true when a recorded window
//!   was resized, after which positions no longer map onto the frames.
//!
//! A reader accepts any `version` 1 file; a later version bumps the number
//! only for changes an older reader would misread.

use crate::{BugleError, Rect};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const EVENTS_FORMAT: &str = "bugle-events";
pub const EVENTS_VERSION: u32 = 1;
/// Longest gap between two samples that is interpolated as one motion. The
/// recorder logs only changes, so a longer gap means the pointer rested and
/// then moved: it is held and moves during the last such interval.
pub const MAX_INTERPOLATED_GAP: f64 = 1.0 / 30.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Move,
    Down,
    Up,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CursorEvent {
    pub t: f64,
    #[serde(rename = "type")]
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<MouseButton>,
    pub x: f64,
    pub y: f64,
}

impl CursorEvent {
    pub fn moved(t: f64, x: f64, y: f64) -> Self {
        Self { t, kind: EventKind::Move, button: None, x, y }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventTrack {
    pub format: String,
    pub version: u32,
    pub area: Rect,
    #[serde(default = "yes")]
    pub cursor_in_video: bool,
    #[serde(default)]
    pub duration: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub area_changed: bool,
    #[serde(default)]
    pub events: Vec<CursorEvent>,
}

fn yes() -> bool {
    true
}

fn invalid(msg: impl Into<String>) -> BugleError {
    BugleError::Invalid(msg.into())
}

impl EventTrack {
    pub fn new(area: Rect, cursor_in_video: bool, duration: f64, events: Vec<CursorEvent>) -> Self {
        Self {
            format: EVENTS_FORMAT.into(),
            version: EVENTS_VERSION,
            area,
            cursor_in_video,
            duration,
            fps: None,
            area_changed: false,
            events,
        }
    }

    /// The sidecar next to a video: `clip.mp4` → `clip.mp4.events.json`.
    pub fn sidecar_path(video: &Path) -> PathBuf {
        let mut s = video.as_os_str().to_owned();
        s.push(".events.json");
        PathBuf::from(s)
    }

    /// Parse and validate a track. Events are put in time order; unknown
    /// event types are dropped.
    pub fn parse(text: &str) -> Result<Self, BugleError> {
        let mut t: EventTrack = serde_json::from_str(text).map_err(|e| invalid(format!("not a Bugle event track: {e}")))?;
        if t.format != EVENTS_FORMAT {
            return Err(invalid(format!("not a Bugle event track (format is '{}')", t.format)));
        }
        if t.version == 0 || t.version > EVENTS_VERSION {
            return Err(invalid(format!("event track version {} is not supported (this Bugle reads {EVENTS_VERSION})", t.version)));
        }
        let a = t.area;
        if ![a.x, a.y, a.w, a.h].iter().all(|v| v.is_finite()) || a.w <= 0.0 || a.h <= 0.0 {
            return Err(invalid("event track area must be a finite, non-empty rect"));
        }
        if !t.duration.is_finite() || t.duration < 0.0 {
            return Err(invalid("event track duration must be a finite, non-negative number"));
        }
        if let Some(e) = t.events.iter().find(|e| !(e.t.is_finite() && e.x.is_finite() && e.y.is_finite())) {
            return Err(invalid(format!("event track has a non-finite event at t={}", e.t)));
        }
        t.events.retain(|e| e.kind != EventKind::Other);
        t.events.sort_by(|a, b| a.t.total_cmp(&b.t));
        t.fps = t.fps.filter(|f| f.is_finite() && *f > 0.0);
        Ok(t)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }

    /// Write the track (via a temp file renamed into place).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    /// Times and positions of button presses.
    pub fn clicks(&self) -> impl Iterator<Item = (f64, f64, f64)> + '_ {
        self.events.iter().filter(|e| e.kind == EventKind::Down).map(|e| (e.t, e.x, e.y))
    }

    /// The pointer position (area points) at `t`, or `None` before the first
    /// event. Samples closer than [`MAX_INTERPOLATED_GAP`] are interpolated;
    /// across a longer gap the pointer rests and then moves during the last
    /// such interval before the next sample.
    pub fn position_at(&self, t: f64) -> Option<(f64, f64)> {
        let ev = &self.events;
        let i = ev.partition_point(|e| e.t <= t);
        if i == 0 {
            return None;
        }
        let a = &ev[i - 1];
        let Some(b) = ev.get(i) else { return Some((a.x, a.y)) };
        let start = a.t.max(b.t - MAX_INTERPOLATED_GAP);
        if t <= start || b.t <= start {
            return Some((a.x, a.y));
        }
        let k = (t - start) / (b.t - start);
        Some((a.x + (b.x - a.x) * k, a.y + (b.y - a.y) * k))
    }

    /// The part of the track inside `start..end`, shifted so `start` is 0. A
    /// `move` at 0 carries the position at `start`.
    pub fn trimmed(&self, start: f64, end: f64) -> Self {
        let mut events = Vec::new();
        if let Some((x, y)) = self.position_at(start) {
            events.push(CursorEvent::moved(0.0, x, y));
        }
        events.extend(self.events.iter().filter(|e| e.t > start && e.t <= end).map(|e| CursorEvent { t: e.t - start, ..*e }));
        Self { events, duration: (end - start).max(0.0), ..self.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "format": "bugle-events", "version": 1,
        "area": {"x": 0, "y": 30, "w": 1280, "h": 720},
        "cursor_in_video": false, "duration": 3.0,
        "future_field": {"ignored": true},
        "events": [
            {"t": 1.52, "type": "down", "button": "left", "x": 700, "y": 100},
            {"t": 0.0, "type": "move", "x": 10, "y": 20},
            {"t": 1.0, "type": "move", "x": 700, "y": 100},
            {"t": 1.6, "type": "scroll", "x": 700, "y": 100, "dy": 3},
            {"t": 1.61, "type": "up", "button": "left", "x": 700, "y": 100},
            {"t": 2.0, "type": "down", "button": "back", "x": 5, "y": 6}
        ]
    }"#;

    #[test]
    fn parses_sorts_and_drops_unknown_types() {
        let t = EventTrack::parse(SAMPLE).unwrap();
        assert_eq!(t.area, Rect::new(0.0, 30.0, 1280.0, 720.0));
        assert!(!t.cursor_in_video);
        assert_eq!(t.events.len(), 5, "the scroll event is dropped");
        assert!(t.events.windows(2).all(|w| w[0].t <= w[1].t));
        assert_eq!(t.events[2].button, Some(MouseButton::Left));
        assert_eq!(t.events[4].button, Some(MouseButton::Other));
        assert_eq!(t.clicks().collect::<Vec<_>>(), vec![(1.52, 700.0, 100.0), (2.0, 5.0, 6.0)]);
        // Older writers without the optional fields.
        let t = EventTrack::parse(r#"{"format":"bugle-events","version":1,"area":{"x":0,"y":0,"w":10,"h":10}}"#).unwrap();
        assert!(t.cursor_in_video && t.events.is_empty() && t.duration == 0.0);
        assert!(t.fps.is_none() && !t.area_changed);
        let t = EventTrack::parse(r#"{"format":"bugle-events","version":1,"area":{"x":0,"y":0,"w":10,"h":10},"fps":-3,"area_changed":true}"#).unwrap();
        assert!(t.fps.is_none() && t.area_changed);
    }

    #[test]
    fn rejects_bad_tracks() {
        let area = r#""area":{"x":0,"y":0,"w":10,"h":10}"#;
        for bad in [
            "not json".to_string(),
            r#"{"format":"other","version":1,"area":{"x":0,"y":0,"w":10,"h":10}}"#.to_string(),
            format!(r#"{{"format":"bugle-events","version":2,{area}}}"#),
            format!(r#"{{"format":"bugle-events","version":0,{area}}}"#),
            r#"{"format":"bugle-events","version":1,"area":{"x":0,"y":0,"w":0,"h":10}}"#.to_string(),
            format!(r#"{{"format":"bugle-events","version":1,{area},"duration":-1}}"#),
            format!(r#"{{"format":"bugle-events","version":1,{area},"events":[{{"t":0,"type":"move","x":1}}]}}"#),
        ] {
            assert!(matches!(EventTrack::parse(&bad), Err(BugleError::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("clip.v2.mp4");
        let side = EventTrack::sidecar_path(&video);
        assert_eq!(side, dir.path().join("clip.v2.mp4.events.json"));
        assert_ne!(EventTrack::sidecar_path(&dir.path().join("clip.v2.mov")), side, "one sidecar per file");
        let t = EventTrack::parse(SAMPLE).unwrap();
        t.save(&side).unwrap();
        assert_eq!(EventTrack::load(&side).unwrap(), t);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temp file left");
        let text = std::fs::read_to_string(&side).unwrap();
        assert!(text.contains(r#""type":"move""#) && !text.contains(r#""button":null"#), "{text}");
    }

    #[test]
    fn positions_interpolate_short_gaps_and_hold_long_ones() {
        let mut t = EventTrack::new(Rect::new(0.0, 0.0, 100.0, 100.0), true, 3.0, vec![]);
        assert_eq!(t.position_at(1.0), None);
        t.events = vec![
            CursorEvent::moved(0.0, 0.0, 0.0),
            // Rests at 0 until 1.0 - 1/30, then moves to 10.
            CursorEvent::moved(1.0, 10.0, 0.0),
            CursorEvent::moved(1.02, 12.0, 4.0),
        ];
        assert_eq!(t.position_at(0.5), Some((0.0, 0.0)));
        assert_eq!(t.position_at(1.0 - MAX_INTERPOLATED_GAP), Some((0.0, 0.0)));
        let (x, _) = t.position_at(1.0 - MAX_INTERPOLATED_GAP / 2.0).unwrap();
        assert!((x - 5.0).abs() < 1e-9, "{x}");
        let (x, y) = t.position_at(1.01).unwrap();
        assert!((x - 11.0).abs() < 1e-9 && (y - 2.0).abs() < 1e-9);
        assert_eq!(t.position_at(9.0), Some((12.0, 4.0)));
    }

    #[test]
    fn trimming_shifts_and_seeds_the_position() {
        let t = EventTrack::parse(SAMPLE).unwrap();
        let cut = t.trimmed(1.5, 1.7);
        assert!((cut.duration - 0.2).abs() < 1e-9);
        assert_eq!(cut.events[0], CursorEvent::moved(0.0, 700.0, 100.0));
        let times: Vec<f64> = cut.events.iter().map(|e| (e.t * 100.0).round() / 100.0).collect();
        assert_eq!(times, vec![0.0, 0.02, 0.11]);
        assert_eq!(cut.area, t.area);
        assert!(!cut.cursor_in_video);
    }
}
