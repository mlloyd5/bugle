//! The live audio level meter while recording: one glyph per source after
//! the `● m:ss` in the status item, and a line per source under *Stop
//! Recording* in the menu. Fed from `Engine::record_status`, so it shows
//! whatever recording is running, however it was started.

use anyhow::Result;
use bugle_core::audio_level::{meter_label, AudioLevels};
use std::time::Duration;
use tray_icon::menu::{Menu, MenuItem};

/// How often the meter is redrawn while recording.
pub const INTERVAL: Duration = Duration::from_millis(100);

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
        if let Some(l) = mic {
            self.mic.set_text(meter_label("Microphone", &l));
        }
        if let Some(l) = system {
            self.system.set_text(meter_label("System Audio", &l));
        }
        Ok(())
    }
}
