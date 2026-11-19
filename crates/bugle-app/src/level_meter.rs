//! The live audio level meter while recording: one glyph per source after
//! the `● m:ss` in the status item, and a line per source under *Stop
//! Recording* in the menu. Fed from `Engine::record_status`, so it shows
//! whatever recording is running, however it was started.

use anyhow::Result;
use bugle_core::audio_level::{meter_label, meter_title, AudioLevels};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tray_icon::menu::{Menu, MenuItem};

/// How often the meter is redrawn while recording.
pub const INTERVAL: Duration = Duration::from_millis(100);

/// One redraw of the meter, as logged for `--smoke-test` ([`watch`]): when
/// (Unix seconds), the status-title glyphs, and the menu lines set.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub at: f64,
    pub glyphs: String,
    pub lines: Vec<String>,
}

static WATCHING: AtomicBool = AtomicBool::new(false);
static UPDATES: Mutex<Vec<Update>> = Mutex::new(Vec::new());
static SHOWN: Mutex<Vec<(bool, Instant)>> = Mutex::new(Vec::new());

/// Start (clearing the logs) or stop logging meter redraws and recording
/// state changes. Off unless a smoke test turns it on.
pub fn watch(on: bool) {
    UPDATES.lock().unwrap_or_else(|e| e.into_inner()).clear();
    SHOWN.lock().unwrap_or_else(|e| e.into_inner()).clear();
    WATCHING.store(on, Ordering::Relaxed);
}

/// The redraws logged since [`watch`] turned logging on.
pub fn updates() -> Vec<Update> {
    UPDATES.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Main thread: the status item (and menu) just switched to showing a
/// recording (`true`) or no recording; logged while [`watch`]ing.
pub fn note_shown(recording: bool) {
    if WATCHING.load(Ordering::Relaxed) {
        SHOWN.lock().unwrap_or_else(|e| e.into_inner()).push((recording, Instant::now()));
    }
}

/// When the app last switched to showing `recording`, since [`watch`].
pub fn shown_at(recording: bool) -> Option<Instant> {
    SHOWN.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().find(|s| s.0 == recording).map(|s| s.1)
}

pub struct LevelMeter {
    mic: MenuItem,
    system: MenuItem,
    /// Menu index of the first meter line (just under *Stop Recording*).
    at: usize,
    /// Which lines are in the menu now: (microphone, system audio).
    shown: (bool, bool),
}

impl LevelMeter {
    pub fn new(at: usize) -> Self {
        Self {
            mic: MenuItem::with_id("level:microphone", "Microphone", false, None),
            system: MenuItem::with_id("level:system_audio", "System Audio", false, None),
            at,
            shown: (false, false),
        }
    }

    /// Show `levels` in the menu (no lines without a recording or audio).
    pub fn update(&mut self, menu: &Menu, levels: Option<&AudioLevels>) -> Result<()> {
        let mic = levels.and_then(|l| l.microphone);
        let system = levels.and_then(|l| l.system_audio);
        let want = (mic.is_some(), system.is_some());
        if want != self.shown {
            if self.shown.0 {
                menu.remove(&self.mic)?;
            }
            if self.shown.1 {
                menu.remove(&self.system)?;
            }
            self.shown = (false, false);
            let mut at = self.at;
            if want.0 {
                menu.insert(&self.mic, at)?;
                at += 1;
            }
            if want.1 {
                menu.insert(&self.system, at)?;
            }
            self.shown = want;
        }
        let mut lines = Vec::new();
        for (item, name, level) in [(&self.mic, "Microphone", mic), (&self.system, "System Audio", system)] {
            if let Some(l) = level {
                let text = meter_label(name, &l);
                item.set_text(&text);
                lines.push(text);
            }
        }
        if WATCHING.load(Ordering::Relaxed) && !lines.is_empty() {
            let at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            let glyphs = levels.and_then(meter_title).unwrap_or_default();
            let mut log = UPDATES.lock().unwrap_or_else(|e| e.into_inner());
            // Bounded: a smoke step logs a few seconds at most.
            if log.len() < 10_000 {
                log.push(Update { at, glyphs, lines });
            }
        }
        Ok(())
    }
}
