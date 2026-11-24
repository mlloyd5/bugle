//! What the status item and the recording entries show: this app's own
//! recording, or failing that one another Bugle process on this Mac is
//! making (`bugle record`, `bugle serve`, `bugle mcp`), read from the
//! engine's recording-state files (`bugle_engine::record::marker`).

use bugle_core::{RecordingState, RecordingStatus};
use bugle_engine::record::marker::LiveRecording;
use chrono::{DateTime, Utc};

/// The label of the *Stop Recording* entry while nothing else is recording.
pub const STOP_LABEL: &str = "Stop Recording";

/// One state of the recording UI; the app redraws only when it changes.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingView {
    /// The status item's title (`● m:ss`, plus the level meter for this
    /// app's own recording); `None` shows the icon alone.
    pub title: Option<String>,
    /// This app's engine is recording: Stop works, Record turns into Stop.
    pub own: bool,
    /// Another process is recording (and this app is not): its pid.
    pub other: Option<u32>,
}

impl RecordingView {
    /// The *Stop Recording* entry: its label, and whether it is enabled. A
    /// recording of another process cannot be stopped from here (`bugle
    /// serve` and `bugle mcp` would quit on a signal), so the entry says
    /// where it runs instead of offering a Stop that does nothing.
    pub fn stop_item(&self) -> (String, bool) {
        match (self.own, self.other) {
            (false, Some(pid)) => (other_process_label(pid), false),
            (own, _) => (STOP_LABEL.to_string(), own),
        }
    }
}

/// How the menu names a recording another process makes.
pub fn other_process_label(pid: u32) -> String {
    format!("Recording in another Bugle process (pid {pid})")
}

fn clock(secs: f64) -> String {
    let secs = secs.max(0.0) as u64;
    format!("● {}:{:02}", secs / 60, secs % 60)
}

/// The view for this app's recorder status `own` and the recordings other
/// processes make (`others`, oldest first, as `marker::scan` returns them),
/// at `now`. The app's own recording wins; otherwise the oldest other one
/// shows, without a level meter (its levels stay in that process).
pub fn view(own: &RecordingStatus, others: &[LiveRecording], now: DateTime<Utc>) -> RecordingView {
    if own.state != RecordingState::Idle {
        let meter = own.audio_levels.as_ref().and_then(bugle_core::audio_level::meter_title);
        let meter = meter.map(|m| format!(" {m}")).unwrap_or_default();
        return RecordingView { title: Some(format!("{}{meter}", clock(own.elapsed_seconds))), own: true, other: None };
    }
    match others.first() {
        Some(r) => RecordingView { title: Some(clock(r.elapsed_seconds(now))), own: false, other: Some(r.pid) },
        None => RecordingView { title: None, own: false, other: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::RecordingFormat;
    use std::path::PathBuf;

    fn idle() -> RecordingStatus {
        RecordingStatus {
            state: RecordingState::Idle,
            format: None,
            started_at: None,
            elapsed_seconds: 0.0,
            frames: 0,
            output: None,
            audio_levels: None,
        }
    }

    fn other(pid: u32, started_at: DateTime<Utc>) -> LiveRecording {
        LiveRecording { pid, process_started_us: 0, started_at, format: RecordingFormat::Mp4, output: PathBuf::from("/tmp/r.mp4"), max_seconds: None }
    }

    #[test]
    fn stop_label_matches_the_menu_table() {
        assert_eq!(crate::commands::find("stop_recording").map(|e| e.label), Some(STOP_LABEL));
    }

    #[test]
    fn nothing_recording() {
        let v = view(&idle(), &[], Utc::now());
        assert_eq!(v, RecordingView { title: None, own: false, other: None });
        assert_eq!(v.stop_item(), (STOP_LABEL.to_string(), false));
    }

    #[test]
    fn own_recording() {
        let st = RecordingStatus { state: RecordingState::Active, elapsed_seconds: 65.9, ..idle() };
        let v = view(&st, &[], Utc::now());
        assert_eq!(v, RecordingView { title: Some("● 1:05".into()), own: true, other: None });
        assert_eq!(v.stop_item(), (STOP_LABEL.to_string(), true));
        // Finishing (stopped by itself, not finalised yet) still counts.
        let st = RecordingStatus { state: RecordingState::Finishing, ..st };
        assert!(view(&st, &[], Utc::now()).own);
    }

    #[test]
    fn another_process_recording() {
        let now = Utc::now();
        let others = [other(4242, now - chrono::Duration::seconds(125)), other(77, now - chrono::Duration::seconds(3))];
        let v = view(&idle(), &others, now);
        assert_eq!(v, RecordingView { title: Some("● 2:05".into()), own: false, other: Some(4242) });
        let (label, enabled) = v.stop_item();
        assert!(!enabled, "a Stop that cannot stop it is not offered");
        assert_eq!(label, "Recording in another Bugle process (pid 4242)");
    }

    #[test]
    fn another_process_recording_clamps_its_time() {
        let now = Utc::now();
        let mut r = other(9, now - chrono::Duration::seconds(600));
        r.max_seconds = Some(30.0);
        assert_eq!(view(&idle(), &[r.clone()], now).title.as_deref(), Some("● 0:30"));
        r.started_at = now + chrono::Duration::seconds(10);
        assert_eq!(view(&idle(), &[r], now).title.as_deref(), Some("● 0:00"));
    }

    #[test]
    fn own_recording_wins_over_another_process() {
        let now = Utc::now();
        let st = RecordingStatus { state: RecordingState::Active, elapsed_seconds: 2.0, ..idle() };
        let v = view(&st, &[other(4242, now - chrono::Duration::seconds(500))], now);
        assert_eq!(v, RecordingView { title: Some("● 0:02".into()), own: true, other: None });
        assert_eq!(v.stop_item(), (STOP_LABEL.to_string(), true));
    }
}
