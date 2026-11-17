//! Video editor logic: the in and out points, the playhead and playback,
//! the zoom, pointer and preset choices, and the [`EditRequest`] they make.
//!
//! Nothing here touches AppKit or decodes a frame; `video_editor_window.rs`
//! is the glue. [`VideoEditor::handle`] turns the window's events into
//! changes and answers with [`Effects`]: whether to refresh the controls,
//! which frame to show next (and not before when, during playback), and an
//! export to run. The export itself is `Engine::edit`, the same pipeline as
//! `bugle edit` and the `edit_video` action.

use bugle_engine::edit::MIN_CLIP_SECONDS;
use bugle_engine::{EditRequest, ExportPreset};
use bugle_platform::{VideoEditorCommand, VideoEditorEvent, VideoEditorState};
use std::time::{Duration, Instant};

/// The zoom menu: label and factor (`None`: no zoom).
pub const ZOOMS: [(&str, Option<f64>); 5] = [("Off", None), ("1.5×", Some(1.5)), ("2×", Some(2.0)), ("2.5×", Some(2.5)), ("3×", Some(3.0))];

/// The preset menu, in [`ExportPreset::ALL`] order.
pub const PRESETS: [ExportPreset; 9] = ExportPreset::ALL;

/// Frames per second the preview plays at, at most (fewer when decoding
/// cannot keep up: playback stays in real time and skips frames).
pub const PLAY_FPS: f64 = 30.0;

/// A menu label for a preset.
pub fn preset_label(p: ExportPreset) -> &'static str {
    match p {
        ExportPreset::Original => "Original",
        ExportPreset::Lossless => "Lossless (copy)",
        ExportPreset::High => "H.264",
        ExportPreset::Hevc => "HEVC",
        ExportPreset::P1080 => "1080p",
        ExportPreset::P720 => "720p",
        ExportPreset::P540 => "540p",
        ExportPreset::Gif => "GIF",
        ExportPreset::GifSmall => "GIF (small)",
    }
}

/// What the recording's event track allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Caps {
    /// Zoom on clicks (there is a usable event track).
    pub zoom: bool,
    /// Smooth the pointer (and the frames lack it: `hide_cursor`).
    pub smooth: bool,
}

/// A frame to show: the one at `t`, on screen no earlier than `at`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameRequest {
    pub t: f64,
    pub at: Instant,
}

/// What the window should do after an event.
#[derive(Debug, Default)]
pub struct Effects {
    /// Send [`VideoEditor::state`] to the window.
    pub state: bool,
    pub frame: Option<FrameRequest>,
    /// Export with this request.
    pub export: Option<EditRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Play {
    /// Timeline time shown at `origin_at`.
    origin_t: f64,
    origin_at: Instant,
}

/// One recording being edited.
#[derive(Debug)]
pub struct VideoEditor {
    /// Engine `source` naming the recording.
    source: String,
    duration: f64,
    /// One frame, for ← and →.
    step: f64,
    caps: Caps,
    playhead: f64,
    in_t: f64,
    out_t: f64,
    zoom: usize,
    smooth: bool,
    preset: usize,
    playing: Option<Play>,
    busy: bool,
}

impl VideoEditor {
    /// `fps` is the recording's nominal frame rate (for frame steps).
    /// Smoothing starts on when it is possible: a recording made with the
    /// pointer hidden shows no pointer at all without it (or zoom).
    pub fn new(source: impl Into<String>, duration: f64, fps: f64, caps: Caps) -> Self {
        let duration = if duration.is_finite() { duration.max(MIN_CLIP_SECONDS) } else { MIN_CLIP_SECONDS };
        let fps = if fps.is_finite() && fps >= 1.0 { fps.min(240.0) } else { 30.0 };
        Self {
            source: source.into(),
            duration,
            step: 1.0 / fps,
            caps,
            playhead: 0.0,
            in_t: 0.0,
            out_t: duration,
            zoom: 0,
            smooth: caps.smooth,
            preset: 0,
            playing: None,
            busy: false,
        }
    }

    #[cfg(test)]
    fn is_playing(&self) -> bool {
        self.playing.is_some()
    }

    #[cfg(test)]
    fn playhead(&self) -> f64 {
        self.playhead
    }

    #[cfg(test)]
    fn range(&self) -> (f64, f64) {
        (self.in_t, self.out_t)
    }

    /// While an export runs every event is ignored.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
        if busy {
            self.playing = None;
        }
    }

    /// The zoom factor and smoothing the preview renders with (the same
    /// as the export's).
    pub fn options(&self) -> (Option<f64>, bool) {
        (ZOOMS.get(self.zoom).and_then(|z| z.1), self.smooth && self.caps.smooth)
    }

    /// The first picture: the frame at the playhead.
    pub fn start(&self, now: Instant) -> Effects {
        Effects { state: true, frame: Some(self.frame_now(now)), export: None }
    }

    /// The decodable time for the playhead `t`: the very end of the track
    /// has no frame of its own, so it shows the last one.
    fn frame_time(&self, t: f64) -> f64 {
        t.clamp(0.0, (self.duration - self.step / 2.0).max(0.0))
    }

    fn frame_now(&self, now: Instant) -> FrameRequest {
        FrameRequest { t: self.frame_time(self.playhead), at: now }
    }

    fn seek(&mut self, t: f64, now: Instant) -> Effects {
        self.playing = None;
        self.playhead = t.clamp(0.0, self.duration);
        Effects { state: true, frame: Some(self.frame_now(now)), export: None }
    }

    /// Set the in point; an out point it would pass goes back to the end.
    fn mark_in(&mut self, t: f64) {
        self.in_t = t.clamp(0.0, self.duration - MIN_CLIP_SECONDS);
        if self.out_t - self.in_t < MIN_CLIP_SECONDS {
            self.out_t = self.duration;
        }
    }

    /// Set the out point; an in point it would pass goes back to the start.
    fn mark_out(&mut self, t: f64) {
        self.out_t = t.clamp(MIN_CLIP_SECONDS, self.duration);
        if self.out_t - self.in_t < MIN_CLIP_SECONDS {
            self.in_t = 0.0;
        }
    }

    pub fn handle(&mut self, event: VideoEditorEvent, now: Instant) -> Effects {
        if self.busy {
            return Effects::default();
        }
        let state = Effects { state: true, ..Default::default() };
        match event {
            VideoEditorEvent::Seek(t) => self.seek(t, now),
            // Dragging a handle shows the frame under it, and never pushes
            // the other handle: it stops a minimum clip away.
            VideoEditorEvent::SetIn(t) => {
                self.in_t = t.clamp(0.0, self.out_t - MIN_CLIP_SECONDS).max(0.0);
                self.seek(self.in_t, now)
            }
            VideoEditorEvent::SetOut(t) => {
                self.out_t = t.clamp(self.in_t + MIN_CLIP_SECONDS, self.duration).min(self.duration);
                self.seek(self.out_t, now)
            }
            VideoEditorEvent::Command(c) => match c {
                VideoEditorCommand::PlayPause if self.playing.is_some() => {
                    self.playing = None;
                    state
                }
                VideoEditorCommand::PlayPause => {
                    // From the in point when the playhead is outside the
                    // clip or at its end.
                    if self.playhead < self.in_t || self.playhead >= self.out_t - self.step / 2.0 {
                        self.playhead = self.in_t;
                    }
                    self.playing = Some(Play { origin_t: self.playhead, origin_at: now });
                    Effects { state: true, frame: Some(self.frame_now(now)), export: None }
                }
                VideoEditorCommand::MarkIn => {
                    self.mark_in(self.playhead);
                    state
                }
                VideoEditorCommand::MarkOut => {
                    self.mark_out(self.playhead);
                    state
                }
                VideoEditorCommand::Export => {
                    self.playing = None;
                    Effects { state: true, frame: None, export: Some(self.request()) }
                }
                VideoEditorCommand::StepBack => self.seek(self.playhead - self.step, now),
                VideoEditorCommand::StepForward => self.seek(self.playhead + self.step, now),
                VideoEditorCommand::GoIn => self.seek(self.in_t, now),
                VideoEditorCommand::GoOut => self.seek(self.out_t, now),
                VideoEditorCommand::Escape if self.playing.is_some() => {
                    self.playing = None;
                    state
                }
                VideoEditorCommand::Escape => Effects::default(),
            },
            VideoEditorEvent::Zoom(i) if i < ZOOMS.len() && (self.caps.zoom || i == 0) => {
                self.zoom = i;
                self.rerender(now)
            }
            VideoEditorEvent::Smooth(on) if self.caps.smooth => {
                self.smooth = on;
                self.rerender(now)
            }
            VideoEditorEvent::Preset(i) if i < PRESETS.len() => {
                self.preset = i;
                state
            }
            // A choice the window should not have offered: put it back.
            VideoEditorEvent::Zoom(_) | VideoEditorEvent::Smooth(_) | VideoEditorEvent::Preset(_) => state,
        }
    }

    /// The options changed: show the current frame with them, unless
    /// playback is about to.
    fn rerender(&self, now: Instant) -> Effects {
        Effects { state: true, frame: self.playing.is_none().then(|| self.frame_now(now)), export: None }
    }

    /// A frame asked for at `t` is on screen at `now`. During playback,
    /// the next one: where real time has got to, but at most [`PLAY_FPS`]
    /// frames a second; at the out point playback stops on its last frame.
    pub fn shown(&mut self, t: f64, now: Instant) -> Option<FrameRequest> {
        let play = self.playing?;
        let real = play.origin_t + now.saturating_duration_since(play.origin_at).as_secs_f64();
        let next = real.max(t + 1.0 / PLAY_FPS);
        if next >= self.out_t {
            self.playing = None;
            self.playhead = self.out_t;
            return Some(self.frame_now(now));
        }
        self.playhead = next;
        let at = play.origin_at + Duration::from_secs_f64((next - play.origin_t).max(0.0));
        Some(FrameRequest { t: self.frame_time(next), at })
    }

    /// The export as an edit request: only the trimmed ends are given.
    pub fn request(&self) -> EditRequest {
        let (zoom, smooth) = self.options();
        EditRequest {
            source: self.source.clone(),
            start: (self.in_t > 1e-3).then_some(self.in_t),
            end: (self.out_t < self.duration - 1e-3).then_some(self.out_t),
            preset: PRESETS.get(self.preset).copied().unwrap_or_default(),
            zoom,
            smooth_cursor: smooth,
            ..Default::default()
        }
    }

    pub fn state(&self) -> VideoEditorState {
        VideoEditorState {
            playhead: self.playhead,
            in_point: self.in_t,
            out_point: self.out_t,
            playing: self.playing.is_some(),
            zoom: self.zoom,
            preset: self.preset,
            smooth: self.smooth && self.caps.smooth,
            can_zoom: self.caps.zoom,
            can_smooth: self.caps.smooth,
            busy: self.busy,
            // The range itself is on the timeline; its length goes here.
            time_text: format!("{} / {} · clip {}", clock(self.playhead), clock(self.duration), clock(self.out_t - self.in_t)),
        }
    }
}

/// `m:ss.cc`, or `h:mm:ss.cc` from an hour on.
pub fn clock(t: f64) -> String {
    let cs = (t.max(0.0) * 100.0).round() as u64;
    let (h, m, s, c) = (cs / 360_000, cs / 6000 % 60, cs / 100 % 60, cs % 100);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{c:02}")
    } else {
        format!("{m}:{s:02}.{c:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use VideoEditorCommand as C;
    use VideoEditorEvent as E;

    const ALL: Caps = Caps { zoom: true, smooth: true };

    fn editor(caps: Caps) -> VideoEditor {
        VideoEditor::new("abc123", 10.0, 25.0, caps)
    }

    fn nothing(fx: &Effects) -> bool {
        !fx.state && fx.frame.is_none() && fx.export.is_none()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn untouched_edit_exports_the_whole_recording() {
        let e = editor(Caps::default());
        let r = e.request();
        assert_eq!(r.source, "abc123");
        assert_eq!((r.start, r.end, r.zoom, r.smooth_cursor), (None, None, None, false));
        assert_eq!(r.preset, ExportPreset::Original);
        assert!(!r.uses_events(), "a plain trim keeps the frames");
    }

    #[test]
    fn marks_set_the_range_at_the_playhead() {
        let mut e = editor(Caps::default());
        let now = Instant::now();
        e.handle(E::Seek(2.5), now);
        e.handle(E::Command(C::MarkIn), now);
        e.handle(E::Seek(7.0), now);
        let fx = e.handle(E::Command(C::MarkOut), now);
        assert!(fx.state && fx.frame.is_none());
        assert_eq!(e.range(), (2.5, 7.0));
        let r = e.request();
        assert_eq!((r.start, r.end), (Some(2.5), Some(7.0)));
        // An in point past the out point resets the out point, and back.
        e.handle(E::Seek(8.0), now);
        e.handle(E::Command(C::MarkIn), now);
        assert_eq!(e.range(), (8.0, 10.0));
        e.handle(E::Seek(1.0), now);
        e.handle(E::Command(C::MarkOut), now);
        assert_eq!(e.range(), (0.0, 1.0));
        assert_eq!(e.request().start, None);
    }

    #[test]
    fn dragged_handles_stop_a_clip_apart_and_show_their_frame() {
        let mut e = editor(Caps::default());
        let now = Instant::now();
        e.handle(E::SetOut(4.0), now);
        let fx = e.handle(E::SetIn(9.0), now);
        let (i, o) = e.range();
        assert!(close(o, 4.0) && close(i, 4.0 - MIN_CLIP_SECONDS), "{i} {o}");
        assert!(close(fx.frame.unwrap().t, i), "shows the frame at the handle");
        e.handle(E::SetOut(-3.0), now);
        assert!(close(e.range().1, i + MIN_CLIP_SECONDS));
        e.handle(E::SetIn(-1.0), now);
        e.handle(E::SetOut(99.0), now);
        assert_eq!(e.range(), (0.0, 10.0));
        // The very end shows the last frame, not a time past it.
        assert!(e.handle(E::Command(C::GoOut), now).frame.unwrap().t < 10.0);
    }

    #[test]
    fn steps_and_seeks_stay_on_the_timeline() {
        let mut e = editor(Caps::default());
        let now = Instant::now();
        let fx = e.handle(E::Command(C::StepBack), now);
        assert_eq!(e.playhead(), 0.0);
        assert_eq!(fx.frame.unwrap().t, 0.0);
        e.handle(E::Command(C::StepForward), now);
        assert!(close(e.playhead(), 0.04));
        e.handle(E::Seek(42.0), now);
        assert_eq!(e.playhead(), 10.0);
    }

    #[test]
    fn playback_runs_in_real_time_and_stops_at_the_out_point() {
        let mut e = editor(Caps::default());
        let t0 = Instant::now();
        e.handle(E::Seek(3.0), t0);
        e.handle(E::Command(C::MarkIn), t0);
        e.handle(E::Seek(4.0), t0);
        e.handle(E::Command(C::MarkOut), t0);
        // At the out point, play starts over from the in point.
        let fx = e.handle(E::Command(C::PlayPause), t0);
        assert!(e.is_playing() && e.state().playing);
        assert_eq!(fx.frame.unwrap().t, 3.0);
        // A fast decoder: the next frame is a frame interval on, due then.
        let next = e.shown(3.0, t0 + Duration::from_millis(5)).unwrap();
        assert!(close(next.t, 3.0 + 1.0 / PLAY_FPS));
        let due = next.at.duration_since(t0).as_secs_f64();
        assert!((due - 1.0 / PLAY_FPS).abs() < 1e-6, "{due}");
        // A slow one: frames are skipped to stay in real time.
        let next = e.shown(next.t, t0 + Duration::from_millis(500)).unwrap();
        assert!(close(next.t, 3.5), "{}", next.t);
        // Past the out point: the last frame, and playback stops.
        let last = e.shown(next.t, t0 + Duration::from_millis(1200)).unwrap();
        assert!(!e.is_playing());
        assert_eq!(e.playhead(), 4.0);
        assert!(close(last.t, 4.0));
        assert_eq!(e.shown(4.0, t0 + Duration::from_secs(2)), None, "nothing more");
    }

    #[test]
    fn pausing_seeking_and_exporting_stop_playback() {
        let now = Instant::now();
        let mut e = editor(Caps::default());
        e.handle(E::Command(C::PlayPause), now);
        e.handle(E::Command(C::PlayPause), now);
        assert!(!e.is_playing(), "play toggles");
        e.handle(E::Command(C::PlayPause), now);
        assert!(e.handle(E::Command(C::Escape), now).state);
        assert!(!e.is_playing());
        assert!(nothing(&e.handle(E::Command(C::Escape), now)), "Esc never closes");
        e.handle(E::Command(C::PlayPause), now);
        e.handle(E::Seek(1.0), now);
        assert!(!e.is_playing());
        e.handle(E::Command(C::PlayPause), now);
        let fx = e.handle(E::Command(C::Export), now);
        assert!(!e.is_playing() && fx.export.is_some());
        assert_eq!(e.shown(1.0, now), None, "a late frame starts nothing");
    }

    #[test]
    fn options_need_the_track_and_rerender_the_frame() {
        let now = Instant::now();
        let mut plain = editor(Caps::default());
        plain.handle(E::Zoom(2), now);
        plain.handle(E::Smooth(true), now);
        assert_eq!(plain.options(), (None, false), "no track: nothing to apply");
        assert!(!plain.state().can_zoom && !plain.state().can_smooth);

        let pointer_in_video = Caps { zoom: true, smooth: false };
        let mut e = editor(pointer_in_video);
        assert_eq!(e.options(), (None, false), "smoothing starts off");
        let fx = e.handle(E::Zoom(2), now);
        assert!(fx.frame.is_some(), "the frame is drawn again with the zoom");
        assert_eq!(e.options(), (Some(2.0), false));
        e.handle(E::Zoom(99), now);
        assert_eq!(e.options().0, Some(2.0), "out of range ignored");

        let mut hidden = editor(ALL);
        assert_eq!(hidden.options(), (None, true), "a hidden pointer starts smoothed");
        hidden.handle(E::Smooth(false), now);
        hidden.handle(E::Zoom(4), now);
        hidden.handle(E::Preset(PRESETS.iter().position(|p| *p == ExportPreset::Gif).unwrap()), now);
        let r = hidden.request();
        assert_eq!((r.zoom, r.smooth_cursor, r.preset), (Some(3.0), false, ExportPreset::Gif));
        assert!(r.uses_events());
    }

    #[test]
    fn busy_ignores_everything() {
        let now = Instant::now();
        let mut e = editor(ALL);
        e.handle(E::Command(C::PlayPause), now);
        e.set_busy(true);
        assert!(!e.is_playing() && e.state().busy);
        for ev in [E::Seek(3.0), E::Command(C::MarkIn), E::Command(C::Export), E::Zoom(1), E::Preset(3)] {
            assert!(nothing(&e.handle(ev, now)), "{ev:?}");
        }
        e.set_busy(false);
        assert!(e.handle(E::Seek(3.0), now).frame.is_some());
    }

    #[test]
    fn clock_and_labels() {
        assert_eq!(clock(0.0), "0:00.00");
        assert_eq!(clock(65.456), "1:05.46");
        assert_eq!(clock(3725.0), "1:02:05.00");
        assert_eq!(clock(-1.0), "0:00.00");
        let e = editor(Caps::default());
        assert_eq!(e.state().time_text, "0:00.00 / 0:10.00 · clip 0:10.00");
        for p in PRESETS {
            assert!(!preset_label(p).is_empty());
        }
        assert_eq!(PRESETS[0], ExportPreset::default(), "the menu starts on the default preset");
    }
}
