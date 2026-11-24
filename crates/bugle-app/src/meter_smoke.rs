//! `--smoke-test` step for the live level meter: a short real
//! ScreenCaptureKit recording with system audio, checking while it runs that
//! the record status reports levels, that the status item title and the
//! menu's meter lines are set from them, and that the meter keeps updating
//! while the status menu is open; then that the recording stops cleanly into
//! a valid file.
//!
//! The recording covers a small corner of the main display and goes to the
//! temporary folder; it is deleted (file and history entry) afterwards
//! unless `BUGLE_SMOKE_KEEP` is set, also after a failure or a panic. The
//! step is skipped when `[sync]` would copy or upload a video. The
//! microphone is recorded only with `BUGLE_SMOKE_MICROPHONE=1` and access
//! already granted. A short system sound is played with `afplay` (another
//! process: the recorder leaves this one's audio out) so the meter has
//! something to show.
//!
//! Whether the meter updates while the menu is open is read from the app's
//! own redraw log ([`level_meter::watch`]), not from the probe: the probe
//! reads the open menu once and closes it, so it wakes the run loop only
//! twice and cannot account for the redraws in between.

use crate::level_meter::{self, Update};
use crate::{AppEvent, Proxy};
use bugle_core::audio_level::{meter_glyph, TrackLevel, FLOOR_DB, METER_RANGE_DB};
use bugle_core::{CaptureTarget, MediaKind, RecordRequest, RecordingFormat, RecordingState, Rect};
use bugle_engine::Engine;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use tray_icon::menu::{ContextMenu, Menu};
use tray_icon::TrayIcon;

/// The shortest recording, from its start.
const RECORD_FOR: Duration = Duration::from_millis(2200);
/// How long the status menu stays open, and when (after it opens) the probe
/// reads it.
const MENU_OPEN_S: f64 = 1.5;
const SAMPLE_AT_S: f64 = 1.2;
/// How long the app may take to show the meter of a recording started
/// behind its back: the levels come with the first audio buffers.
const SHOW_TIMEOUT: Duration = Duration::from_secs(3);
/// How soon after the call that starts (or stops) a recording behind the
/// app's back returns, as its Agent API does, the engine's `RecordingChanged`
/// must have reached the app and the status item switched: well inside the
/// one-second tick that used to be the only way the app found out. Timed from
/// the return, so the recorder's own startup (ScreenCaptureKit can take a
/// while) does not count.
const SHOW_PROMPTLY: Duration = Duration::from_millis(500);

/// Check that by [`SHOW_PROMPTLY`] after `until` (when the call made at
/// `since` returned) the app handled the engine's event for `recording`
/// (`what`) and the status item showed it; either may already have happened
/// during the call (0 ms). The event is required, not just the switch: the
/// 100 ms meter tick, or the one-second one, could flip the item without it.
fn check_prompt(what: &str, recording: bool, since: Instant, until: Instant, failures: &mut Vec<String>) {
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let (event, shown) = loop {
        let (event, shown) = (level_meter::event_at(recording), level_meter::shown_at(recording));
        if (event.is_some() && shown.is_some()) || until.elapsed() > SHOW_PROMPTLY {
            break (event.map(|at| at.saturating_duration_since(until)), shown.map(|at| at.saturating_duration_since(until)));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let call = ms(until.saturating_duration_since(since));
    match (event, shown) {
        (Some(e), Some(s)) if e <= SHOW_PROMPTLY && s <= SHOW_PROMPTLY => say(format!(
            "the status item showed the {what} {:.0} ms and the engine's event was handled {:.0} ms after the call returned (the call took {call:.0} ms)",
            ms(s),
            ms(e)
        )),
        (None, _) => failures.push(format!(
            "the app handled no RecordingChanged event for the {what} within {} ms of the call returning (it took {call:.0} ms)",
            SHOW_PROMPTLY.as_millis()
        )),
        (_, None) => failures.push(format!("the status item did not show the {what} within {} ms of the call returning", SHOW_PROMPTLY.as_millis())),
        (Some(e), Some(s)) => failures.push(format!(
            "the status item showed the {what} {:.0} ms and the event came {:.0} ms after the call returned, not within {} ms (the call took {call:.0} ms)",
            ms(s),
            ms(e),
            SHOW_PROMPTLY.as_millis()
        )),
    }
}

/// Ask the main thread to open the status menu and read it
/// ([`bugle_platform::status_menu_probe`]); the report (JSON) or why it
/// could not run comes back on `reply`.
pub struct ProbeRequest {
    reply: Sender<Result<String, String>>,
}

/// Main thread: start the probe for `req` on the app's status item. The
/// caller redraws the recording UI first, so the menu opens on the meter.
pub fn open_probe(tray: &TrayIcon, menu: &Menu, req: ProbeRequest) {
    let Some(item) = tray.ns_status_item() else {
        let _ = req.reply.send(Err("the status item has no NSStatusItem".into()));
        return;
    };
    let item_ptr = &*item as *const _ as *mut std::ffi::c_void;
    let reply = req.reply.clone();
    // SAFETY: both objects are alive now; the shim retains them until the
    // probe is done.
    let started = unsafe {
        bugle_platform::status_menu_probe(item_ptr, menu.ns_menu(), MENU_OPEN_S, SAMPLE_AT_S, move |json| {
            let _ = reply.send(Ok(json));
        })
    };
    if !started {
        let _ = req.reply.send(Err("a probe is already running".into()));
    }
}

fn say(msg: impl AsRef<str>) {
    eprintln!("smoke test: level meter: {}", msg.as_ref());
}

/// Play a short, quiet system sound from another process.
fn chirp() -> Option<Child> {
    Command::new("/usr/bin/afplay").args(["-v", "0.3", "/System/Library/Sounds/Tink.aiff"]).spawn().ok()
}

fn recording_path() -> std::path::PathBuf {
    std::env::temp_dir().join("bugle-smoke-meter.mp4")
}

/// Run the step. `Err` carries its failures (each also printed as a detail
/// line above it); a skip is `Ok`. Whatever happens, a recording it left
/// running is stopped and its file removed.
pub fn smoke(engine: &Engine, proxy: &Proxy) -> Result<(), String> {
    let out = recording_path();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(engine, proxy, &out)));
    cleanup(engine, &out);
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(failures)) => Err(failures.join("; ")),
        Err(_) => Err("the step panicked".into()),
    }
}

/// Stop a recording the step left running and remove what it wrote (file,
/// writer leftovers, history entry) unless `BUGLE_SMOKE_KEEP` is set.
fn cleanup(engine: &Engine, out: &Path) {
    level_meter::watch(false);
    if engine.record_status().state != RecordingState::Idle && engine.record_status().output.as_deref() == out.to_str() {
        match engine.record_stop() {
            Ok(_) => say("stopped the recording left running"),
            Err(e) => say(format!("stopping the recording left running failed: {e:#}")),
        }
    }
    if std::env::var_os("BUGLE_SMOKE_KEEP").is_some() {
        return;
    }
    for m in engine.history.all().unwrap_or_default().into_iter().filter(|m| m.path == out) {
        let _ = engine.history.delete(&m.id, false);
    }
    let _ = std::fs::remove_file(out);
    for l in leftovers_of(out) {
        let _ = std::fs::remove_file(out.with_file_name(l));
    }
}

fn run(engine: &Engine, proxy: &Proxy, out: &Path) -> Result<(), Vec<String>> {
    let skip = |why: &str| {
        say(format!("skipped ({why})"));
        Ok(())
    };
    if !bugle_platform::sck_record_available() {
        return skip("the ScreenCaptureKit recorder needs macOS 14");
    }
    if !bugle_platform::screen_access_preflight() {
        return skip("Screen Recording access is not granted");
    }
    if engine.config.recordings.backend == bugle_core::config::RecordingBackend::SystemRecorder {
        return skip("recordings.backend = \"system\" has no level meter");
    }
    if engine.config.sync.applies_to(MediaKind::Video) {
        return skip("[sync] would copy, move or upload the recording");
    }
    if engine.record_status().state != RecordingState::Idle {
        return skip("a recording is already running");
    }
    let mic_access = bugle_platform::microphone_access();
    let microphone = std::env::var_os("BUGLE_SMOKE_MICROPHONE").is_some_and(|v| v == "1");
    if microphone && mic_access != "authorized" {
        return skip(&format!("BUGLE_SMOKE_MICROPHONE=1 but microphone access is {mic_access} (not requested)"));
    }
    if !microphone && engine.config.recordings.microphone {
        // The engine would add the microphone anyway.
        return skip("[recordings] microphone = true; set BUGLE_SMOKE_MICROPHONE=1 to record it");
    }
    if microphone {
        say("recording the microphone");
    } else {
        say("microphone not recorded (set BUGLE_SMOKE_MICROPHONE=1 to include it)");
    }
    let display = match bugle_platform::main_display() {
        Ok(d) => d,
        Err(e) => return skip(&format!("no display: {e:#}")),
    };
    let _ = std::fs::remove_file(out);
    let req = RecordRequest {
        target: CaptureTarget::Area { rect: Rect::new(display.frame.x, display.frame.y, 320.0, 200.0) },
        format: RecordingFormat::Mp4,
        fps: None,
        microphone,
        system_audio: true,
        mix_audio: false,
        mix: Default::default(),
        // A backstop: the step stops it well before this.
        max_seconds: Some(15.0),
        highlight_clicks: false,
        cursor_events: false,
        hide_cursor: false,
        output: Some(out.display().to_string()),
        label: None,
    };
    level_meter::watch(true);
    let t0 = Instant::now();
    if let Err(e) = engine.record_start(&req) {
        return Err(vec![format!("could not start the recording: {e:#}")]);
    }
    let mut failures = Vec::new();
    check_prompt("recording", true, t0, Instant::now(), &mut failures);
    let mut sounds: Vec<Option<Child>> = vec![chirp()];
    let mut polled: Vec<Option<bugle_core::audio_level::AudioLevels>> = Vec::new();
    let mut inactive = 0;
    let poll = |polled: &mut Vec<_>, inactive: &mut usize| {
        let st = engine.record_status();
        if st.state == RecordingState::Active {
            polled.push(st.audio_levels);
        } else {
            *inactive += 1;
        }
    };
    // The step started the recording behind the app's back (no `Done`); the
    // meter follows once the levels come in: wait for it, not a fixed time.
    while level_meter::updates().is_empty() && t0.elapsed() < SHOW_TIMEOUT {
        poll(&mut polled, &mut inactive);
        std::thread::sleep(Duration::from_millis(50));
    }
    let shown_after = t0.elapsed();
    let mut probe: Option<Receiver<Result<String, String>>> = None;
    let ask = || {
        let (reply, rx) = mpsc::channel();
        proxy.send(AppEvent::MeterProbe(ProbeRequest { reply }));
        rx
    };
    if level_meter::updates().is_empty() {
        failures.push(format!("the app did not show the recording's meter within {:.0} s", SHOW_TIMEOUT.as_secs_f64()));
    } else {
        say(format!("the app showed the recording after {:.2}s", shown_after.as_secs_f64()));
        probe = Some(ask());
    }
    let mut menu_asked = Instant::now();
    let mut attempts = 1;
    let mut report = None;
    while t0.elapsed() < RECORD_FOR || (probe.is_some() && report.is_none() && menu_asked.elapsed() < Duration::from_secs(6)) {
        // A sound while the menu is open, so the meter has to move.
        if sounds.len() == attempts && probe.is_some() && menu_asked.elapsed() >= Duration::from_millis(400) {
            sounds.push(chirp());
        }
        if let (Some(rx), None) = (&probe, &report) {
            if let Ok(r) = rx.try_recv() {
                // Closed before it was read, not by the probe (a click, say):
                // open it once more.
                match r.as_deref().ok().and_then(closed_early) {
                    Some(after) if attempts == 1 => {
                        say(format!("the status menu was closed from outside after {after:.2}s, before it was read; opening it again"));
                        attempts += 1;
                        probe = Some(ask());
                        menu_asked = Instant::now();
                    }
                    _ => report = Some(r),
                }
            }
        }
        poll(&mut polled, &mut inactive);
        std::thread::sleep(Duration::from_millis(100));
    }
    let sound_ok = sounds.len() > attempts && sounds.into_iter().all(|s| s.and_then(|mut c| c.wait().ok()).is_some_and(|st| st.success()));
    if !sound_ok {
        say("the test sound could not be played (afplay failed or did not run); meter movement is not checked");
    }

    // What the engine reported.
    let with_system: Vec<_> = polled.iter().flatten().filter_map(|l| l.system_audio).collect();
    let with_mic: Vec<_> = polled.iter().flatten().filter_map(|l| l.microphone).collect();
    say(format!("record status polled {} times, system audio levels in {} (want all)", polled.len(), with_system.len()));
    if inactive > 0 {
        failures.push(format!("the recording was not active in {inactive} polls"));
    }
    if with_system.is_empty() || with_system.len() < polled.len().saturating_sub(2) {
        failures.push("system audio levels missing from the record status".to_string());
    }
    if microphone {
        say(format!("microphone levels in {} of {}", with_mic.len(), polled.len()));
    } else if !with_mic.is_empty() {
        failures.push("microphone levels reported for a recording without the microphone".to_string());
    }
    let system_peak = range("system audio", &with_system, &mut failures);
    range("microphone", &with_mic, &mut failures);
    if sound_ok && system_peak.is_some_and(|p| p <= METER_RANGE_DB) {
        failures.push(format!("the test sound played but system audio never rose above the meter's {METER_RANGE_DB} dB floor"));
    }

    // What the app showed, and what the open menu showed.
    let updates = level_meter::updates();
    let mut moved_while_open = None;
    match report {
        None if probe.is_some() => failures.push("no report from the status menu probe".into()),
        None => {}
        Some(Err(e)) => failures.push(format!("status menu probe: {e}")),
        Some(Ok(json)) => moved_while_open = check_probe(&json, &updates, &mut failures),
    }
    if sound_ok {
        match moved_while_open {
            Some((lines, glyphs)) if lines == 0 || glyphs == 0 => failures.push(format!(
                "with the test sound playing, the meter barely moved while the menu was open (lines changed {lines} times, title glyphs {glyphs} times)"
            )),
            _ => {}
        }
    }

    // Stop and check the file.
    let mut mic_skipped = false;
    let stop_at = Instant::now();
    let stopped = engine.record_stop();
    check_prompt("stop", false, stop_at, Instant::now(), &mut failures);
    match stopped {
        Ok(media) => {
            let info = bugle_platform::video_info(&media.path);
            let tracks = bugle_platform::audio_track_count(&media.path).unwrap_or(0);
            match &info {
                Ok(i) => say(format!(
                    "stopped: {} {}×{} {:.1}s, {tracks} audio track(s)",
                    media.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    i.width,
                    i.height,
                    i.duration,
                )),
                Err(e) => failures.push(format!("the recording is not a readable movie: {e:#}")),
            }
            if !matches!(info, Ok(i) if i.width > 0 && i.height > 0 && i.duration > 1.0) {
                failures.push("the recording has no video or is shorter than 1 s".into());
            }
            if tracks == 0 {
                failures.push("the recording has no audio track".into());
            }
            if microphone && with_mic.is_empty() {
                if tracks >= 2 {
                    failures.push("the microphone was recorded but the record status had no microphone levels".into());
                } else {
                    mic_skipped = true;
                    say("microphone skipped: access is granted but the recorder captured no microphone (see its warning above)");
                }
            }
            // The writer's temporary file (`<name>.sb-…`) must be gone once
            // the recording is stopped, even before `max_seconds`.
            std::thread::sleep(Duration::from_millis(300));
            let leftovers = leftovers_of(&media.path);
            if !leftovers.is_empty() {
                failures.push(format!("files left next to the recording: {leftovers:?}"));
            }
            if std::env::var_os("BUGLE_SMOKE_KEEP").is_some() {
                say(format!("kept {}", media.path.display()));
            } else if let Err(e) = engine.history.delete(&media.id, true) {
                failures.push(format!("could not delete the recording: {e:#}"));
            }
        }
        Err(e) => failures.push(format!("stopping the recording failed: {e:#}")),
    }
    if !failures.is_empty() {
        for f in &failures {
            say(format!("FAIL detail: {f}"));
        }
        return Err(failures);
    }
    let sources = if microphone && !mic_skipped { "system audio and microphone" } else { "system audio" };
    if sound_ok {
        say(format!("ok ({sources})"));
    } else {
        say(format!("ok ({sources}; silent: movement not checked)"));
    }
    Ok(())
}

/// Report a source's levels and fail readings outside the dBFS range; the
/// highest peak, if any.
fn range(name: &str, ls: &[TrackLevel], failures: &mut Vec<String>) -> Option<f64> {
    if ls.is_empty() {
        return None;
    }
    let max_peak = ls.iter().map(|l| l.peak_db).fold(f64::MIN, f64::max);
    let min_peak = ls.iter().map(|l| l.peak_db).fold(f64::MAX, f64::min);
    let max_rms = ls.iter().map(|l| l.rms_db).fold(f64::MIN, f64::max);
    say(format!("{name} peak {min_peak:.1}..{max_peak:.1} dBFS, rms up to {max_rms:.1} dBFS"));
    let bad = ls.iter().filter(|l| !(FLOOR_DB..=0.0).contains(&l.peak_db) || !(FLOOR_DB..=0.0).contains(&l.rms_db)).count();
    if bad > 0 {
        failures.push(format!("{bad} {name} readings outside {FLOOR_DB}..0 dBFS"));
    }
    if max_peak <= METER_RANGE_DB {
        say(format!("{name} stayed below the meter's {METER_RANGE_DB} dB floor"));
    }
    Some(max_peak)
}

/// Names in `path`'s folder that start with its file name and a dot.
fn leftovers_of(path: &Path) -> Vec<String> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().map(|n| format!("{}.", n.to_string_lossy()))) else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(&name)).collect()
}

/// How long the menu was open when it closed before the probe read it.
fn closed_early(json: &str) -> Option<f64> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let open = v["closed_at"].as_f64()? - v["opened_at"].as_f64()?;
    (v["opened"] == serde_json::Value::Bool(true) && v["sample"].is_null()).then_some(open)
}

/// Check the probe report against the app's redraw log: the app kept
/// redrawing while the menu was open (more often than the probe woke the run
/// loop), and what the open menu showed is what the app last set. Returns
/// how often the meter lines and the title glyphs changed while open.
fn check_probe(json: &str, updates: &[Update], failures: &mut Vec<String>) -> Option<(usize, usize)> {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            failures.push(format!("status menu probe report: {e}"));
            return None;
        }
    };
    if v["opened"] != serde_json::Value::Bool(true) {
        failures.push("the status menu did not open".into());
        return None;
    }
    let (opened, closed) = (v["opened_at"].as_f64().unwrap_or(0.0), v["closed_at"].as_f64().unwrap_or(0.0));
    let firings = v["firings"].as_u64().unwrap_or(0) as usize;
    let open: Vec<&Update> = updates.iter().filter(|u| u.at > opened && u.at < closed).collect();
    let gaps: Vec<f64> = open.windows(2).map(|w| w[1].at - w[0].at).collect();
    let max_gap = gaps.iter().copied().fold(0.0, f64::max);
    let line_changes = open.windows(2).filter(|w| w[0].lines != w[1].lines).count();
    let glyph_changes = open.windows(2).filter(|w| w[0].glyphs != w[1].glyphs).count();
    say(format!(
        "status menu open {:.2}s: the app redrew the meter {} times (longest gap {:.2}s); the probe's own timers fired {firings} times",
        closed - opened,
        open.len(),
        max_gap
    ));
    say(format!("while open: meter lines changed {line_changes} times, status-title glyphs {glyph_changes} times"));
    if open.len() <= firings {
        failures.push(format!("the app redrew the meter {} times while the menu was open, not more than the probe's {firings} wake-ups", open.len()));
    }

    // The one reading of the open menu.
    let s = &v["sample"];
    if s.is_null() {
        failures.push("the probe did not read the open menu".into());
        return Some((line_changes, glyph_changes));
    }
    let mode = s["mode"].as_str().unwrap_or("");
    let title = s["title"].as_str().unwrap_or("");
    let items: Vec<&str> = s["items"].as_array().map(|a| a.iter().filter_map(|i| i.as_str()).collect()).unwrap_or_default();
    say(format!(
        "open menu read at {:.2}s in {mode}: title {title:?}, menu width {:.0} pt",
        s["t"].as_f64().unwrap_or(-1.0),
        s["width"].as_f64().unwrap_or(0.0)
    ));
    if mode != "NSEventTrackingRunLoopMode" {
        failures.push(format!("the open menu was read in {mode:?}, not NSEventTrackingRunLoopMode"));
    }
    // What the app last set before the reading.
    let read_at = opened + s["t"].as_f64().unwrap_or(0.0);
    let Some(last) = updates.iter().rev().find(|u| u.at <= read_at) else {
        failures.push("no meter redraw before the open menu was read".into());
        return Some((line_changes, glyph_changes));
    };
    let Some(stop) = items.iter().position(|i| *i == "Stop Recording") else {
        failures.push("no Stop Recording item in the open menu".into());
        return Some((line_changes, glyph_changes));
    };
    let meter: Vec<&str> = items.iter().skip(stop + 1).take(last.lines.len()).copied().collect();
    if meter != last.lines {
        failures.push(format!("the open menu shows {meter:?} under Stop Recording, the app last set {:?}", last.lines));
    }
    let glyphs = title.rsplit(' ').next().unwrap_or("");
    if !title.starts_with("● ") || glyphs != last.glyphs {
        failures.push(format!("the status item title is {title:?}; want `● m:ss {}`", last.glyphs));
    }
    if s["digits_monospaced"].as_bool() != Some(true) {
        failures.push("the status item title's font does not draw its digits at one width".into());
    }
    for (g, line) in glyphs.chars().zip(&meter) {
        if !glyph_matches(g, line) {
            failures.push(format!("title glyph {g} does not match {line:?}"));
        }
    }
    Some((line_changes, glyph_changes))
}

/// Whether the status-title glyph `g` fits the peak shown on `line`
/// (`… -23 dB` or `… silent`), allowing one step for the line's rounding.
fn glyph_matches(g: char, line: &str) -> bool {
    const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let db = if line.trim_end().ends_with("silent") {
        METER_RANGE_DB
    } else {
        match line.trim_end().strip_suffix(" dB").and_then(|s| s.rsplit(' ').next()) {
            Some(n) => match n.parse::<f64>() {
                Ok(d) => d,
                Err(_) => return false,
            },
            None => return false,
        }
    };
    let idx = |c: char| GLYPHS.iter().position(|x| *x == c);
    match (idx(g), idx(meter_glyph(db)), idx(meter_glyph(db - 0.5)), idx(meter_glyph(db + 0.5))) {
        (Some(a), Some(b), Some(lo), Some(hi)) => a == b || a == lo || a == hi,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::audio_level::meter_label;

    #[test]
    fn glyphs_match_their_lines() {
        for db in [-100.0, -61.0, -45.3, -30.0, -12.6, -6.0, -0.4, 0.0] {
            let l = TrackLevel { peak_db: db, rms_db: db };
            let line = meter_label("System Audio", &l);
            assert!(glyph_matches(meter_glyph(db), &line), "{db}: {line}");
        }
        let line = meter_label("System Audio", &TrackLevel { peak_db: -6.0, rms_db: -9.0 });
        assert!(!glyph_matches('▁', &line), "{line}");
        assert!(!glyph_matches('x', &line));
    }

    fn update(at: f64, glyphs: &str, line: &str) -> Update {
        Update { at, glyphs: glyphs.into(), lines: vec![line.into()] }
    }

    fn report(sample_title: &str, sample_line: &str) -> String {
        serde_json::json!({
            "opened": true, "opened_at": 100.0, "closed_at": 101.5, "firings": 2,
            "sample": {"t": 1.2, "mode": "NSEventTrackingRunLoopMode", "title": sample_title, "digits_monospaced": true,
                       "items": ["Record", "Stop Recording", sample_line, "", "Audio Levels"], "width": 320.0}
        })
        .to_string()
    }

    #[test]
    fn probe_counts_redraws_inside_the_open_window() {
        let a = meter_label("System Audio", &TrackLevel { peak_db: -20.0, rms_db: -30.0 });
        let b = meter_label("System Audio", &TrackLevel { peak_db: -40.0, rms_db: -50.0 });
        let (ga, gb) = (meter_glyph(-20.0).to_string(), meter_glyph(-40.0).to_string());
        let ups = vec![
            update(99.9, &ga, &a),
            update(100.2, &ga, &a),
            update(100.5, &gb, &b),
            update(100.8, &gb, &b),
            update(101.6, "▁", &b),
        ];
        let mut failures = Vec::new();
        let moved = check_probe(&report(&format!("● 0:01 {gb}"), &b), &ups, &mut failures);
        assert_eq!(moved, Some((1, 1)));
        assert!(failures.is_empty(), "{failures:?}");

        // Two redraws while open are not more than the probe's two wake-ups,
        // and a menu line that is not what the app set is caught.
        let mut failures = Vec::new();
        check_probe(&report(&format!("● 0:01 {ga}"), &b), &ups[..3], &mut failures);
        assert_eq!(failures.len(), 3, "{failures:?}");

        // A title in proportional digits is caught.
        let mut v: serde_json::Value = serde_json::from_str(&report(&format!("● 0:01 {gb}"), &b)).unwrap();
        v["sample"]["digits_monospaced"] = false.into();
        let mut failures = Vec::new();
        check_probe(&v.to_string(), &ups, &mut failures);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("digits"), "{failures:?}");
    }
}
