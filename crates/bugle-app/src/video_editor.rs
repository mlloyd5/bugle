//! Video editor logic: the in and out points, the playhead and playback,
//! the zoom (for all clicks and for single ones), pointer and preset
//! choices, and the [`EditRequest`] they make.
//!
//! Nothing here touches AppKit or decodes a frame; `video_editor_window.rs`
//! is the glue. [`VideoEditor::handle`] turns the window's events into
//! changes and answers with [`Effects`]: whether to refresh the controls,
//! which frame to show next (and not before when, during playback), and an
//! export to run. The export itself is `Engine::edit`, the same pipeline as
//! `bugle edit` and the `edit_video` action.

use bugle_engine::cursor_fx::{ZOOM_HOLD, ZOOM_IN, ZOOM_OUT};
use bugle_engine::edit::MIN_CLIP_SECONDS;
use bugle_engine::{ClickZoom, EditRequest, ExportPreset};
use bugle_platform::{ClickMarker, VideoEditorCommand, VideoEditorEvent, VideoEditorState};
use std::time::{Duration, Instant};

/// The zoom menu: label and factor (`None`: no zoom).
pub const ZOOMS: [(&str, Option<f64>); 5] = [("Off", None), ("1.5×", Some(1.5)), ("2×", Some(2.0)), ("2.5×", Some(2.5)), ("3×", Some(3.0))];

/// The click menu (the zoom of the click at the playhead): label and level
/// (`None`: as the zoom menu says; 1: this click is not zoomed).
pub const CLICK_ZOOMS: [(&str, Option<f64>); 6] =
    [("Default", None), ("Off", Some(1.0)), ("1.5×", Some(1.5)), ("2×", Some(2.0)), ("2.5×", Some(2.5)), ("3×", Some(3.0))];

/// Buckets of the timeline waveform (more than a timeline is wide in pixels).
pub const WAVEFORM_BUCKETS: usize = 4096;

/// The timeline waveform for `peaks` (0..1): scaled so the loudest bucket
/// fills the track, but a quiet recording (peaking under 0.1) is drawn no
/// louder than ten times as loud, so near-silence stays flat.
pub fn waveform_levels(peaks: &[f32]) -> Vec<f32> {
    let max = peaks.iter().copied().filter(|v| v.is_finite()).fold(0.0f32, f32::max);
    let k = 1.0 / max.max(0.1);
    peaks.iter().map(|v| if v.is_finite() { (v * k).clamp(0.0, 1.0) } else { 0.0 }).collect()
}

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
    /// The times of the clicks auto-zoom follows.
    clicks: Vec<f64>,
    /// Each click's choice in [`CLICK_ZOOMS`].
    click_zoom: Vec<usize>,
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
    /// `fps` is the recording's nominal frame rate (for frame steps);
    /// `clicks` the times of the clicks auto-zoom follows, in order (none
    /// without zoom). Smoothing starts on when it is possible: a recording
    /// made with the pointer hidden shows no pointer at all without it (or
    /// zoom).
    pub fn new(source: impl Into<String>, duration: f64, fps: f64, caps: Caps, clicks: Vec<f64>) -> Self {
        let duration = if duration.is_finite() { duration.max(MIN_CLIP_SECONDS) } else { MIN_CLIP_SECONDS };
        let fps = if fps.is_finite() && fps >= 1.0 { fps.min(240.0) } else { 30.0 };
        Self {
            source: source.into(),
            duration,
            step: 1.0 / fps,
            caps,
            click_zoom: vec![0; if caps.zoom { clicks.len() } else { 0 }],
            clicks: if caps.zoom { clicks } else { Vec::new() },
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

    /// Whether an export is running.
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// While an export runs every event is ignored.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
        if busy {
            self.playing = None;
        }
    }

    /// The zoom factor and smoothing the preview renders with (the same
    /// as the export's); see also [`VideoEditor::click_zooms`].
    pub fn options(&self) -> (Option<f64>, bool) {
        (ZOOMS.get(self.zoom).and_then(|z| z.1), self.smooth && self.caps.smooth)
    }

    /// The clicks whose zoom differs from the zoom menu's, with their level.
    pub fn click_zooms(&self) -> Vec<ClickZoom> {
        let default = self.options().0.unwrap_or(1.0);
        self.clicks
            .iter()
            .zip(&self.click_zoom)
            .filter_map(|(&t, &c)| CLICK_ZOOMS.get(c).and_then(|z| z.1).filter(|z| *z != default).map(|zoom| ClickZoom { t, zoom }))
            .collect()
    }

    /// The level each click zooms to (1: not at all).
    fn click_level(&self, i: usize) -> f64 {
        let choice = self.click_zoom.get(i).and_then(|c| CLICK_ZOOMS.get(*c)).and_then(|z| z.1);
        choice.or(self.options().0).unwrap_or(1.0)
    }

    /// The click the click menu is about: the one the view zooms for at the
    /// playhead (the last whose ease-in has begun, until its zoom would be
    /// over), whether or not it is zoomed.
    pub fn selected_click(&self) -> Option<usize> {
        let i = self.clicks.partition_point(|c| c - ZOOM_IN <= self.playhead).checked_sub(1)?;
        (self.playhead <= self.clicks[i] + ZOOM_HOLD + ZOOM_OUT).then_some(i)
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
            // A lossless copy keeps the frames as recorded, so zoom and
            // smoothing cannot go with it: turning either on leaves the
            // lossless preset for the default one, and choosing lossless
            // turns both off.
            VideoEditorEvent::Zoom(i) if i < ZOOMS.len() && (self.caps.zoom || i == 0) => {
                self.zoom = i;
                if i != 0 {
                    self.leave_lossless();
                }
                self.rerender(now)
            }
            VideoEditorEvent::Smooth(on) if self.caps.smooth => {
                self.smooth = on;
                if on {
                    self.leave_lossless();
                }
                self.rerender(now)
            }
            VideoEditorEvent::ClickZoom(c) if c < CLICK_ZOOMS.len() && self.caps.zoom => {
                let Some(i) = self.selected_click() else { return state };
                self.click_zoom[i] = c;
                if self.click_level(i) > 1.0 {
                    self.leave_lossless();
                }
                self.rerender(now)
            }
            VideoEditorEvent::Preset(i) if i < PRESETS.len() => {
                self.preset = i;
                if self.lossless() && (self.zoom != 0 || self.smooth || self.click_zoom.iter().any(|c| *c != 0)) {
                    self.zoom = 0;
                    self.smooth = false;
                    self.click_zoom.iter_mut().for_each(|c| *c = 0);
                    return self.rerender(now);
                }
                state
            }
            // A choice the window should not have offered: put it back.
            VideoEditorEvent::Zoom(_) | VideoEditorEvent::Smooth(_) | VideoEditorEvent::Preset(_) | VideoEditorEvent::ClickZoom(_) => state,
        }
    }

    fn lossless(&self) -> bool {
        PRESETS.get(self.preset) == Some(&ExportPreset::Lossless)
    }

    fn leave_lossless(&mut self) {
        if self.lossless() {
            self.preset = 0;
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
            click_zooms: self.click_zooms(),
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
            click_zoom: self.selected_click().map(|i| self.click_zoom[i]),
            clicks: (0..self.clicks.len())
                .map(|i| ClickMarker { t: self.clicks[i], level: self.click_level(i), selected: Some(i) == self.selected_click() })
                .collect(),
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
        VideoEditor::new("abc123", 10.0, 25.0, caps, vec![2.0, 3.0, 8.0])
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
        assert!(r.click_zooms.is_empty());
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
    fn lossless_and_rerendering_options_exclude_each_other() {
        let now = Instant::now();
        let lossless = PRESETS.iter().position(|p| *p == ExportPreset::Lossless).unwrap();
        let mut e = editor(ALL);
        e.handle(E::Zoom(1), now);
        let fx = e.handle(E::Preset(lossless), now);
        assert!(fx.state && fx.frame.is_some(), "the frame is drawn again without the effects");
        assert_eq!(e.options(), (None, false), "lossless turns zoom and smoothing off");
        let r = e.request();
        assert_eq!(r.preset, ExportPreset::Lossless);
        assert!(!r.uses_events(), "a lossless request can always be exported");
        assert!(!e.state().smooth && e.state().zoom == 0);

        e.handle(E::Smooth(true), now);
        assert_eq!(e.request().preset, ExportPreset::Original, "smoothing leaves lossless");
        e.handle(E::Smooth(false), now);
        e.handle(E::Preset(lossless), now);
        e.handle(E::Zoom(2), now);
        assert_eq!((e.request().preset, e.options().0), (ExportPreset::Original, Some(2.0)), "zoom leaves lossless");
        e.handle(E::Preset(lossless), now);
        e.handle(E::Zoom(0), now);
        assert_eq!(e.request().preset, ExportPreset::Lossless, "zoom off keeps lossless");
    }

    #[test]
    fn each_click_gets_its_own_zoom() {
        let now = Instant::now();
        let c = |name: &str| CLICK_ZOOMS.iter().position(|z| z.0 == name).unwrap();
        let mut e = editor(Caps { zoom: true, smooth: false });
        // Clicks at 2, 3 and 8 s. Before the first one eases in: none.
        assert_eq!(e.state().click_zoom, None);
        e.handle(E::ClickZoom(c("2×")), now);
        assert!(e.click_zooms().is_empty(), "no click at the playhead: nothing changes");
        // Easing in to the first click: it is the one the menu is about.
        e.handle(E::Seek(2.0 - ZOOM_IN / 2.0), now);
        let s = e.state();
        assert_eq!(s.click_zoom, Some(0), "Default");
        assert_eq!(s.clicks.iter().map(|m| m.selected).collect::<Vec<_>>(), vec![true, false, false]);
        assert!(s.clicks.iter().all(|m| m.level == 1.0), "zoom off: no click zooms");
        let fx = e.handle(E::ClickZoom(c("2.5×")), now);
        assert!(fx.frame.is_some(), "the frame is drawn again with it");
        assert_eq!(e.click_zooms(), vec![ClickZoom { t: 2.0, zoom: 2.5 }]);
        assert!(e.request().uses_events() && e.request().zoom.is_none());
        // Past its ease-in the second click is selected, and stays until its zoom is over.
        e.handle(E::Seek(2.9), now);
        assert_eq!(e.state().clicks.iter().position(|m| m.selected), Some(1));
        e.handle(E::Seek(3.0 + ZOOM_HOLD + ZOOM_OUT + 0.1), now);
        assert_eq!(e.selected_click(), None);
        // With the zoom menu on, the others follow it; one can be left out.
        e.handle(E::Zoom(2), now);
        e.handle(E::Seek(8.0), now);
        e.handle(E::ClickZoom(c("Off")), now);
        assert_eq!(e.click_zooms(), vec![ClickZoom { t: 2.0, zoom: 2.5 }, ClickZoom { t: 8.0, zoom: 1.0 }]);
        let levels: Vec<f64> = e.state().clicks.iter().map(|m| m.level).collect();
        assert_eq!(levels, vec![2.5, 2.0, 1.0]);
        // A click set to what the zoom menu says needs no override.
        e.handle(E::ClickZoom(c("2×")), now);
        assert_eq!(e.click_zooms().len(), 1);
        // Lossless resets every click; a zoomed click leaves lossless.
        let lossless = PRESETS.iter().position(|p| *p == ExportPreset::Lossless).unwrap();
        e.handle(E::Preset(lossless), now);
        assert!(e.click_zooms().is_empty() && !e.request().uses_events());
        e.handle(E::ClickZoom(c("Off")), now);
        assert_eq!(e.request().preset, ExportPreset::Lossless, "a click off keeps lossless");
        assert!(!e.request().uses_events());
        e.handle(E::ClickZoom(c("3×")), now);
        assert_eq!(e.request().preset, ExportPreset::Original);
        // Without zoom there are no clicks to choose.
        let mut plain = VideoEditor::new("x", 10.0, 25.0, Caps::default(), vec![2.0]);
        plain.handle(E::Seek(2.0), now);
        plain.handle(E::ClickZoom(c("2×")), now);
        assert!(plain.click_zooms().is_empty() && plain.state().clicks.is_empty() && plain.state().click_zoom.is_none());
    }

    #[test]
    fn waveform_is_scaled_to_its_loudest_bucket() {
        assert_eq!(waveform_levels(&[0.0, 0.25, 0.5]), vec![0.0, 0.5, 1.0]);
        let quiet = waveform_levels(&[0.01, 0.05]);
        assert!((quiet[0] - 0.1).abs() < 1e-6 && (quiet[1] - 0.5).abs() < 1e-6, "quiet stays quiet: {quiet:?}");
        assert_eq!(waveform_levels(&[f32::NAN, 2.0]), vec![0.0, 1.0]);
        assert!(waveform_levels(&[]).is_empty());
    }

    #[test]
    fn busy_ignores_everything() {
        let now = Instant::now();
        let mut e = editor(ALL);
        e.handle(E::Command(C::PlayPause), now);
        e.set_busy(true);
        assert!(!e.is_playing() && e.state().busy && e.busy());
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
