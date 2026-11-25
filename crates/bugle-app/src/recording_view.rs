//! What the status item and the recording entries show: this app's own
//! recording, or failing that one another Bugle process on this Mac is
//! making (`bugle record`, `bugle serve`, `bugle mcp`), read from the
//! engine's recording-state files (`bugle_engine::record::marker`), and
//! the save of a recording this app's engine finalises after it stopped by
//! itself, which a new recording from the menu waits for.

use crate::Action;
use bugle_core::{RecordingState, RecordingStatus};
use bugle_engine::record::marker::LiveRecording;
use chrono::{DateTime, Utc};

/// The label of the *Stop Recording* entry while nothing else is recording.
pub const STOP_LABEL: &str = "Stop Recording";

/// The status item's title while this app's engine saves a recording.
pub const SAVING_TITLE: &str = "Saving…";

/// Whether this app's engine is saving a recording that stopped by itself
/// (`Engine::recording_saving`: the reap took it out of the slot, so its
/// status reads idle until the file is written).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Saving {
    #[default]
    No,
    /// Saving, and nothing waits for it.
    Previous,
    /// Saving, and a Record from the menu or a hotkey waits for the save
    /// before it opens the picker and starts.
    RecordWaits,
}

impl Saving {
    /// From the engine's `recording_saving` and whether a Record action is
    /// with the worker (its `Done` not handled yet). A Record that is no
    /// longer waiting (the save is over; the picker is up) is not shown.
    pub fn of(saving: bool, record_running: bool) -> Saving {
        match (saving, record_running) {
            (false, _) => Saving::No,
            (true, false) => Saving::Previous,
            (true, true) => Saving::RecordWaits,
        }
    }
}

/// One state of the recording UI; the app redraws only when it changes.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingView {
    /// The status item's title (`● m:ss`, plus the level meter for this
    /// app's own recording, or [`SAVING_TITLE`]); `None` shows the icon
    /// alone.
    pub title: Option<String>,
    /// This app's engine is recording: Stop works, Record turns into Stop.
    pub own: bool,
    /// Another process is recording (and this app is not): its pid.
    pub other: Option<u32>,
    /// This app's engine is saving a recording (and not recording).
    pub saving: Saving,
}

impl RecordingView {
    /// The *Stop Recording* entry: its label, and whether it is enabled. A
    /// recording of another process cannot be stopped from here (`bugle
    /// serve` and `bugle mcp` would quit on a signal), so the entry says
    /// where it runs instead of offering a Stop that does nothing; a save
    /// in progress says so the same way.
    pub fn stop_item(&self) -> (String, bool) {
        if let Some(note) = self.saving_note() {
            return (note.to_string(), false);
        }
        match (self.own, self.other) {
            (false, Some(pid)) => (other_process_label(pid), false),
            (own, _) => (STOP_LABEL.to_string(), own),
        }
    }

    /// Whether the Record entries are enabled: not while this app records
    /// (Stop is the way out) nor while a Record already waits for a save, so
    /// a second one cannot be queued behind it.
    pub fn record_enabled(&self) -> bool {
        !self.own && self.saving != Saving::RecordWaits
    }

    /// What a save in progress means for the user, for the Stop entry and
    /// for the toast a Record or Stop hotkey gets meanwhile.
    pub fn saving_note(&self) -> Option<&'static str> {
        match self.saving {
            Saving::No => None,
            Saving::Previous => Some("Saving the previous recording…"),
            Saving::RecordWaits => Some("Saving the previous recording; the new one starts once it is saved"),
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

/// The view for this app's recorder status `own`, its save in progress
/// `saving`, and the recordings other processes make (`others`, oldest
/// first, as `marker::scan` returns them), at `now`. The app's own
/// recording wins, then its own save (another process's recording is not
/// one the app waits for); otherwise the oldest other one shows, without a
/// level meter (its levels stay in that process).
pub fn view(own: &RecordingStatus, saving: Saving, others: &[LiveRecording], now: DateTime<Utc>) -> RecordingView {
    if own.state != RecordingState::Idle {
        let meter = own.audio_levels.as_ref().and_then(bugle_core::audio_level::meter_title);
        let meter = meter.map(|m| format!(" {m}")).unwrap_or_default();
        let title = Some(format!("{}{meter}", clock(own.elapsed_seconds)));
        return RecordingView { title, own: true, other: None, saving: Saving::No };
    }
    if saving != Saving::No {
        return RecordingView { title: Some(SAVING_TITLE.to_string()), own: false, other: None, saving };
    }
    match others.first() {
        Some(r) => RecordingView { title: Some(clock(r.elapsed_seconds(now))), own: false, other: Some(r.pid), saving },
        None => RecordingView { title: None, own: false, other: None, saving },
    }
}

/// What the app does with a command from the menu or a hotkey, before its
/// busy check ([`command`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Go on with this action (a Record turns into Stop while recording);
    /// `record`: it is a Record, which may wait for a save
    /// (the app's `record_running` while it is with the worker).
    Send { action: Action, record: bool },
    /// Do nothing but say this in a toast.
    Toast(String),
    /// Do nothing, without a word (Stop with nothing to stop).
    Ignore,
}

/// What a Record or Stop command does given what the app shows (`shown`,
/// fresh); any other action goes on unchanged. While this app records,
/// Record means Stop. Stop with nothing of this app's to stop explains a
/// save in progress or another process's recording. A Record while one
/// already waits for a save is not queued behind it; one during a plain
/// save goes on and becomes the waiting Record.
pub fn command(action: Action, shown: &RecordingView) -> Step {
    let record = matches!(action, Action::RecordVideo | Action::RecordGif | Action::RecordWindow);
    if record && shown.own {
        return Step::Send { action: Action::StopRecording, record: false };
    }
    if action == Action::StopRecording && !shown.own {
        return match (shown.saving_note(), shown.other) {
            (Some(note), _) => Step::Toast(note.to_string()),
            (None, Some(pid)) => Step::Toast(format!(
                "{}: stop it where it was started (bugle record, or record_stop through bugle serve / bugle mcp)",
                other_process_label(pid)
            )),
            (None, None) => Step::Ignore,
        };
    }
    if record && shown.saving == Saving::RecordWaits {
        return Step::Toast(shown.saving_note().unwrap_or_default().to_string());
    }
    Step::Send { action, record }
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
        let v = view(&idle(), Saving::No, &[], Utc::now());
        assert_eq!(v, RecordingView { title: None, own: false, other: None, saving: Saving::No });
        assert_eq!(v.stop_item(), (STOP_LABEL.to_string(), false));
    }

    #[test]
    fn own_recording() {
        let st = RecordingStatus { state: RecordingState::Active, elapsed_seconds: 65.9, ..idle() };
        let v = view(&st, Saving::No, &[], Utc::now());
        assert_eq!(v, RecordingView { title: Some("● 1:05".into()), own: true, other: None, saving: Saving::No });
        assert_eq!(v.stop_item(), (STOP_LABEL.to_string(), true));
        // Finishing (stopped by itself, not finalised yet) still counts.
        let st = RecordingStatus { state: RecordingState::Finishing, ..st };
        assert!(view(&st, Saving::No, &[], Utc::now()).own);
    }

    #[test]
    fn another_process_recording() {
        let now = Utc::now();
        let others = [other(4242, now - chrono::Duration::seconds(125)), other(77, now - chrono::Duration::seconds(3))];
        let v = view(&idle(), Saving::No, &others, now);
        assert_eq!(v, RecordingView { title: Some("● 2:05".into()), own: false, other: Some(4242), saving: Saving::No });
        let (label, enabled) = v.stop_item();
        assert!(!enabled, "a Stop that cannot stop it is not offered");
        assert_eq!(label, "Recording in another Bugle process (pid 4242)");
    }

    #[test]
    fn another_process_recording_clamps_its_time() {
        let now = Utc::now();
        let mut r = other(9, now - chrono::Duration::seconds(600));
        r.max_seconds = Some(30.0);
        assert_eq!(view(&idle(), Saving::No, &[r.clone()], now).title.as_deref(), Some("● 0:30"));
        r.started_at = now + chrono::Duration::seconds(10);
        assert_eq!(view(&idle(), Saving::No, &[r], now).title.as_deref(), Some("● 0:00"));
    }

    #[test]
    fn own_recording_wins_over_another_process() {
        let now = Utc::now();
        let st = RecordingStatus { state: RecordingState::Active, elapsed_seconds: 2.0, ..idle() };
        let v = view(&st, Saving::No, &[other(4242, now - chrono::Duration::seconds(500))], now);
        assert_eq!(v, RecordingView { title: Some("● 0:02".into()), own: true, other: None, saving: Saving::No });
        assert_eq!(v.stop_item(), (STOP_LABEL.to_string(), true));
    }

    #[test]
    fn saving_follows_the_engine_and_a_waiting_record() {
        assert_eq!(Saving::of(false, false), Saving::No);
        // A Record past its wait (picker up, starting) is not a save.
        assert_eq!(Saving::of(false, true), Saving::No);
        assert_eq!(Saving::of(true, false), Saving::Previous);
        assert_eq!(Saving::of(true, true), Saving::RecordWaits);
    }

    #[test]
    fn saving_the_previous_recording() {
        let v = view(&idle(), Saving::Previous, &[], Utc::now());
        assert_eq!(v, RecordingView { title: Some(SAVING_TITLE.into()), own: false, other: None, saving: Saving::Previous });
        let (label, enabled) = v.stop_item();
        assert!(!enabled, "nothing to stop while the file is written");
        assert_eq!(label, "Saving the previous recording…");
        // A Record meanwhile waits for the save; it may still be asked for.
        assert!(v.record_enabled());
    }

    #[test]
    fn record_waiting_for_the_save() {
        let v = view(&idle(), Saving::RecordWaits, &[], Utc::now());
        assert_eq!(v.title.as_deref(), Some(SAVING_TITLE));
        assert_eq!(v.stop_item(), ("Saving the previous recording; the new one starts once it is saved".to_string(), false));
        // A second Record is not queued behind the first: the entries are
        // off, and a hotkey gets this note in a toast instead.
        assert!(!v.record_enabled());
        assert_eq!(v.saving_note(), Some(v.stop_item().0.as_str()));
    }

    #[test]
    fn saving_clears_with_the_engine() {
        // The save over (written or failed) or the start done: nothing left.
        let v = view(&idle(), Saving::of(false, true), &[], Utc::now());
        assert_eq!(v, RecordingView { title: None, own: false, other: None, saving: Saving::No });
        assert_eq!(v.saving_note(), None);
        assert!(v.record_enabled());
        assert_eq!(v.stop_item(), (STOP_LABEL.to_string(), false));
        // The new recording then shows as any other.
        let st = RecordingStatus { state: RecordingState::Active, elapsed_seconds: 1.0, ..idle() };
        let v = view(&st, Saving::of(false, false), &[], Utc::now());
        assert!(v.own && !v.record_enabled());
        assert_eq!(v.stop_item(), (STOP_LABEL.to_string(), true));
    }

    #[test]
    fn own_save_wins_over_another_process_recording() {
        let now = Utc::now();
        let others = [other(4242, now - chrono::Duration::seconds(5))];
        let v = view(&idle(), Saving::RecordWaits, &others, now);
        assert_eq!(v.title.as_deref(), Some(SAVING_TITLE));
        assert_eq!(v.other, None, "the other recording is not what the title shows");
        assert!(!v.stop_item().1);
        // Once saved, the other process's recording shows again.
        let v = view(&idle(), Saving::No, &others, now);
        assert_eq!(v.other, Some(4242));
        assert!(v.record_enabled(), "another process's recording never blocks Record");
    }

    #[test]
    fn own_recording_shows_no_save() {
        // Saving is only ever set while the slot is empty; a recording in
        // it (the next one already started) wins regardless.
        let st = RecordingStatus { state: RecordingState::Active, elapsed_seconds: 3.0, ..idle() };
        let v = view(&st, Saving::RecordWaits, &[], Utc::now());
        assert_eq!(v, RecordingView { title: Some("● 0:03".into()), own: true, other: None, saving: Saving::No });
    }

    fn shown(saving: Saving) -> RecordingView {
        view(&idle(), saving, &[], Utc::now())
    }

    const RECORDS: [Action; 3] = [Action::RecordVideo, Action::RecordGif, Action::RecordWindow];

    #[test]
    fn record_without_a_save_goes_on() {
        for a in RECORDS {
            assert_eq!(command(a, &shown(Saving::No)), Step::Send { action: a, record: true });
        }
    }

    #[test]
    fn record_during_a_save_becomes_the_waiting_record() {
        for a in RECORDS {
            assert_eq!(command(a, &shown(Saving::Previous)), Step::Send { action: a, record: true });
        }
    }

    #[test]
    fn second_record_is_not_queued() {
        for a in RECORDS {
            assert_eq!(
                command(a, &shown(Saving::RecordWaits)),
                Step::Toast("Saving the previous recording; the new one starts once it is saved".into())
            );
        }
    }

    #[test]
    fn stop_hotkey_during_a_save_says_so() {
        assert_eq!(command(Action::StopRecording, &shown(Saving::Previous)), Step::Toast("Saving the previous recording…".into()));
        assert_eq!(
            command(Action::StopRecording, &shown(Saving::RecordWaits)),
            Step::Toast("Saving the previous recording; the new one starts once it is saved".into())
        );
        assert_eq!(command(Action::StopRecording, &shown(Saving::No)), Step::Ignore);
    }

    #[test]
    fn stop_hotkey_during_another_process_recording() {
        let now = Utc::now();
        let v = view(&idle(), Saving::No, &[other(4242, now)], now);
        let Step::Toast(msg) = command(Action::StopRecording, &v) else { panic!("no toast") };
        assert!(msg.starts_with("Recording in another Bugle process (pid 4242): stop it where it was started"), "{msg}");
    }

    #[test]
    fn record_while_recording_stops() {
        let st = RecordingStatus { state: RecordingState::Active, ..idle() };
        let v = view(&st, Saving::No, &[], Utc::now());
        for a in RECORDS.into_iter().chain([Action::StopRecording]) {
            assert_eq!(command(a, &v), Step::Send { action: Action::StopRecording, record: false });
        }
    }

    #[test]
    fn other_actions_never_wait_for_a_save() {
        for saving in [Saving::No, Saving::Previous, Saving::RecordWaits] {
            for a in [Action::CaptureArea, Action::CaptureScreen, Action::OcrLatest, Action::SyncLatest] {
                assert_eq!(command(a, &shown(saving)), Step::Send { action: a, record: false });
            }
        }
    }
}
