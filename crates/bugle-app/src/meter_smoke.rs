//! `--smoke-test` step for the live level meter: a short real
//! ScreenCaptureKit recording with system audio (and the microphone, only
//! when access is already granted), checking while it runs that the record
//! status reports levels, that the status item title and the menu's meter
//! lines are set from them, and that they keep changing while the status
//! menu is open; then that the recording stops cleanly into a valid file.
//!
//! The recording covers a small corner of the main display and goes to the
//! temporary folder; it is deleted (file and history entry) afterwards
//! unless `BUGLE_SMOKE_KEEP` is set. A short system sound is played with
//! `afplay` (another process: the recorder leaves this one's audio out) so
//! the system-audio meter has something to show.

use crate::{AppEvent, Proxy};
use bugle_core::audio_level::{meter_glyph, AudioLevels, FLOOR_DB, METER_RANGE_DB};
use bugle_core::{CaptureTarget, RecordRequest, RecordingFormat, RecordingState, Rect};
use bugle_engine::Engine;
use std::process::{Child, Command};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};
use tray_icon::menu::{ContextMenu, Menu};
use tray_icon::TrayIcon;

/// How long the recording runs.
const RECORD_FOR: Duration = Duration::from_millis(2200);
/// When, after the start, the status menu opens, and for how long.
const MENU_AT: Duration = Duration::from_millis(400);
const MENU_OPEN_S: f64 = 1.5;

/// Ask the main thread to open the status menu and sample it
/// ([`bugle_platform::status_menu_probe`]); the report (JSON) or why it
/// could not run comes back on `reply`.
pub struct ProbeRequest {
    reply: Sender<Result<String, String>>,
}

/// Main thread: start the probe for `req` on the app's status item.
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
        bugle_platform::status_menu_probe(item_ptr, menu.ns_menu(), MENU_OPEN_S, 0.1, move |json| {
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

pub fn smoke(engine: &Engine, proxy: &Proxy) {
    let skip = |why: &str| say(format!("skipped ({why})"));
    if !bugle_platform::sck_record_available() {
        return skip("the ScreenCaptureKit recorder needs macOS 14");
    }
    if !bugle_platform::screen_access_preflight() {
        return skip("Screen Recording access is not granted");
    }
    if engine.config.recordings.backend == bugle_core::config::RecordingBackend::SystemRecorder {
        return skip("recordings.backend = \"system\" has no level meter");
    }
    if engine.record_status().state != RecordingState::Idle {
        return skip("a recording is already running");
    }
    let mic_access = bugle_platform::microphone_access();
    let microphone = mic_access == "authorized";
    if !microphone {
        if engine.config.recordings.microphone {
            // The engine would add the microphone anyway and could prompt.
            return skip(&format!("[recordings] microphone = true but microphone access is {mic_access}"));
        }
        say(format!("microphone not recorded: access is {mic_access} (not requested, so no prompt); checking system audio only"));
    }
    let display = match bugle_platform::main_display() {
        Ok(d) => d,
        Err(e) => return skip(&format!("no display: {e:#}")),
    };
    let out = std::env::temp_dir().join("bugle-smoke-meter.mp4");
    let _ = std::fs::remove_file(&out);
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
    if let Err(e) = engine.record_start(&req) {
        say(format!("FAIL: could not start the recording: {e:#}"));
        return;
    }
    let t0 = Instant::now();
    let mut failures = Vec::new();
    let mut sounds = Vec::new();
    let mut probe = None;
    let mut polled: Vec<Option<AudioLevels>> = Vec::new();
    let mut inactive = 0;
    while t0.elapsed() < RECORD_FOR {
        if sounds.is_empty() && t0.elapsed() >= Duration::from_millis(200) {
            sounds.extend(chirp());
        }
        if sounds.len() == 1 && t0.elapsed() >= Duration::from_millis(900) {
            sounds.extend(chirp());
        }
        if probe.is_none() && t0.elapsed() >= MENU_AT {
            let (reply, rx) = mpsc::channel();
            proxy.send(AppEvent::MeterProbe(ProbeRequest { reply }));
            probe = Some(rx);
        }
        let st = engine.record_status();
        if st.state == RecordingState::Active {
            polled.push(st.audio_levels);
        } else {
            inactive += 1;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let report = probe.map(|rx| rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "no report from the main thread".to_string()).and_then(|r| r));
    for mut s in sounds {
        let _ = s.wait();
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
    // Access granted does not mean the recorder got a microphone (none
    // connected, say): the file's second audio track tells. Levels missing
    // for a microphone that was recorded are a failure.
    let mut mic_skipped = false;
    if microphone {
        say(format!("microphone levels in {} of {}", with_mic.len(), polled.len()));
    } else if !with_mic.is_empty() {
        failures.push("microphone levels reported for a recording without the microphone".to_string());
    }
    let range = |name: &str, ls: &[bugle_core::audio_level::TrackLevel], failures: &mut Vec<String>| {
        if ls.is_empty() {
            return;
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
            say(format!("{name} stayed below the meter's {METER_RANGE_DB} dB floor (nothing audible reached the recorder)"));
        }
    };
    range("system audio", &with_system, &mut failures);
    range("microphone", &with_mic, &mut failures);

    // What the status item and the open menu showed.
    match report {
        None => failures.push("the status menu probe never ran".into()),
        Some(Err(e)) => failures.push(format!("status menu probe: {e}")),
        Some(Ok(json)) => check_probe(&json, !with_mic.is_empty(), &mut failures),
    }

    // Stop and check the file.
    match engine.record_stop() {
        Ok(media) => {
            match bugle_platform::video_info(&media.path) {
                Ok(i) => say(format!(
                    "stopped: {} {}×{} {:.1}s, {} audio track(s)",
                    media.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    i.width,
                    i.height,
                    i.duration,
                    bugle_platform::audio_track_count(&media.path).unwrap_or(0)
                )),
                Err(e) => failures.push(format!("the recording is not a readable movie: {e:#}")),
            }
            if !matches!(bugle_platform::video_info(&media.path), Ok(i) if i.width > 0 && i.height > 0 && i.duration > 1.0) {
                failures.push("the recording has no video or is shorter than 1 s".into());
            }
            let tracks = bugle_platform::audio_track_count(&media.path).unwrap_or(0);
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
                for l in &leftovers {
                    let _ = std::fs::remove_file(media.path.with_file_name(l));
                }
            }
            if std::env::var_os("BUGLE_SMOKE_KEEP").is_some() {
                say(format!("kept {}", media.path.display()));
            } else if let Err(e) = engine.history.delete(&media.id, true) {
                failures.push(format!("could not delete the recording: {e:#}"));
            }
        }
        Err(e) => failures.push(format!("stopping the recording failed: {e:#}")),
    }
    if failures.is_empty() {
        say(if microphone && !mic_skipped { "ok (system audio and microphone)" } else { "ok (system audio)" });
    } else {
        for f in &failures {
            say(format!("FAIL: {f}"));
        }
    }
}

/// Names in `path`'s folder that start with its file name and a dot.
fn leftovers_of(path: &std::path::Path) -> Vec<String> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().map(|n| format!("{}.", n.to_string_lossy()))) else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(&name)).collect()
}

/// The parsed probe report: what the open menu showed, sample by sample.
/// `microphone`: whether the record status had microphone levels.
fn check_probe(json: &str, microphone: bool, failures: &mut Vec<String>) {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => return failures.push(format!("status menu probe report: {e}")),
    };
    if v["opened"] != serde_json::Value::Bool(true) {
        return failures.push("the status menu did not open".into());
    }
    let samples = v["samples"].as_array().cloned().unwrap_or_default();
    let tracking: Vec<_> = samples.iter().filter(|s| s["mode"] == "NSEventTrackingRunLoopMode").collect();
    let modes: std::collections::BTreeSet<_> = samples.iter().filter_map(|s| s["mode"].as_str()).collect();
    say(format!(
        "status menu open {:.1}s: {} samples, {} in NSEventTrackingRunLoopMode (modes seen: {modes:?})",
        v["open_seconds"].as_f64().unwrap_or(0.0),
        samples.len(),
        tracking.len()
    ));
    if tracking.len() < 5 {
        return failures.push("too few samples while the menu was tracking".into());
    }
    let mut titles = Vec::new();
    let mut lines = Vec::new();
    let mut widths = std::collections::BTreeSet::new();
    let mut mismatched = 0;
    // Samples whose menu or title is not what a meter shows: (t, why).
    let mut bad: Vec<(f64, String)> = Vec::new();
    for s in &tracking {
        let t = s["t"].as_f64().unwrap_or(0.0);
        let title = s["title"].as_str().unwrap_or("").to_string();
        let items: Vec<&str> = s["items"].as_array().map(|a| a.iter().filter_map(|i| i.as_str()).collect()).unwrap_or_default();
        widths.insert(s["width"].as_f64().unwrap_or(0.0).round() as i64);
        let Some(stop) = items.iter().position(|i| *i == "Stop Recording") else {
            bad.push((t, "no Stop Recording item".into()));
            continue;
        };
        let want: &[&str] = if microphone { &["Microphone", "System Audio"] } else { &["System Audio"] };
        let meter: Vec<&str> = items.iter().skip(stop + 1).take(want.len()).copied().collect();
        if meter.len() != want.len() || !meter.iter().zip(want).all(|(l, n)| l.starts_with(&format!("{n}  "))) {
            bad.push((t, format!("lines under Stop Recording are {meter:?}, want {want:?}; title {title:?}")));
            continue;
        }
        // The title's glyphs come from the same status as the lines.
        let glyphs: Vec<char> = title.rsplit(' ').next().unwrap_or("").chars().collect();
        if !title.starts_with("● ") || glyphs.len() != want.len() {
            bad.push((t, format!("status item title {title:?} does not end in {} meter glyph(s)", want.len())));
            continue;
        }
        for (g, line) in glyphs.iter().zip(&meter) {
            if !glyph_matches(*g, line) {
                mismatched += 1;
            }
        }
        titles.push(title);
        lines.push(meter.join(" | "));
    }
    if let Some((t, why)) = bad.first() {
        failures.push(format!("{} of {} samples do not show the meter, first at {t:.2}s: {why}", bad.len(), tracking.len()));
    }
    let distinct = |v: &[String]| v.windows(2).filter(|w| w[0] != w[1]).count();
    say(format!(
        "while open: status title changed {} times ({:?} → {:?}), meter lines changed {} times, menu width {:?} pt",
        distinct(&titles),
        titles.first().map(String::as_str).unwrap_or(""),
        titles.last().map(String::as_str).unwrap_or(""),
        distinct(&lines),
        widths
    ));
    say(format!("meter lines while open, first and last: {:?} → {:?}", lines.first(), lines.last()));
    if distinct(&titles) == 0 && distinct(&lines) == 0 {
        failures.push("neither the status title nor the meter lines changed while the menu was open".into());
    }
    if mismatched > 0 {
        failures.push(format!("{mismatched} title glyphs do not match their meter line's dB"));
    }
    if widths.len() > 1 {
        say("note: the menu changed width while open");
    }
}

/// Whether the status-title glyph `g` fits the peak shown on `line`
/// (`… -23 dB` or `… silent`), allowing one step for the line's rounding.
fn glyph_matches(g: char, line: &str) -> bool {
    const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let db = if line.trim_end().ends_with("silent") {
        METER_RANGE_DB
    } else {
        match line.trim_end().strip_suffix(" dB").and_then(|s| s.rsplit(|c: char| c.is_whitespace() || c == '\u{2007}').next()) {
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
    use bugle_core::audio_level::{meter_label, TrackLevel};

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
}
