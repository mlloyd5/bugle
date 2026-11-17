//! Live audio levels of a running recording: the types `record_status`
//! reports and the meter the menu-bar app draws from them.
//!
//! The recorder measures each source as it arrives (before any gain or
//! ducking, which happen after the recording stops): a peak that falls at
//! [`PEAK_FALL_DB_PER_S`] once the signal drops, and an RMS averaged over
//! [`RMS_TIME_CONSTANT_S`]. Both are read without disturbing each other, so
//! several readers (the menu bar, the agent API) see the same values.

use serde::{Deserialize, Serialize};

/// How fast a held peak falls once the signal drops below it.
pub const PEAK_FALL_DB_PER_S: f64 = 20.0;
/// Time constant of the RMS average.
pub const RMS_TIME_CONSTANT_S: f64 = 0.3;
/// Levels below this are reported as this (dBFS); silence included.
pub const FLOOR_DB: f64 = -100.0;
/// Bottom of the drawn meter (dBFS): anything quieter shows as empty.
pub const METER_RANGE_DB: f64 = -60.0;
/// A peak at or above this (dBFS) counts as clipping.
pub const CLIP_DB: f64 = -0.5;

/// One source's current level, in dBFS (0 = full scale), never below
/// [`FLOOR_DB`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TrackLevel {
    pub peak_db: f64,
    pub rms_db: f64,
}

impl TrackLevel {
    /// From linear sample magnitudes (1.0 = full scale).
    pub fn from_linear(peak: f32, rms: f32) -> Self {
        Self { peak_db: db_from_linear(peak), rms_db: db_from_linear(rms) }
    }

    pub fn clipping(&self) -> bool {
        self.peak_db >= CLIP_DB
    }
}

/// The current levels of a recording's sources; a source the recording does
/// not capture is `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioLevels {
    pub microphone: Option<TrackLevel>,
    pub system_audio: Option<TrackLevel>,
}

impl AudioLevels {
    pub fn is_empty(&self) -> bool {
        self.microphone.is_none() && self.system_audio.is_none()
    }
}

/// dBFS of a linear magnitude, rounded to 0.1 dB and clamped to
/// [`FLOOR_DB`]..=+20 (decoded or converted audio can overshoot full scale).
pub fn db_from_linear(x: f32) -> f64 {
    let x = x as f64;
    if !x.is_finite() || x <= 0.0 {
        return FLOOR_DB;
    }
    let db = (20.0 * x.log10()).clamp(FLOOR_DB, 20.0);
    (db * 10.0).round() / 10.0
}

/// How full the meter is for `db`, 0.0 (at or below [`METER_RANGE_DB`]) to
/// 1.0 (full scale or above), linear in dB.
pub fn meter_fraction(db: f64) -> f64 {
    if !db.is_finite() {
        return 0.0;
    }
    ((db - METER_RANGE_DB) / -METER_RANGE_DB).clamp(0.0, 1.0)
}

/// One block glyph (`▁`..`█`) for the status-item title; silence is the
/// lowest one, so the meter keeps its width.
pub fn meter_glyph(db: f64) -> char {
    const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let i = (meter_fraction(db) * GLYPHS.len() as f64).ceil() as usize;
    GLYPHS[i.saturating_sub(1).min(GLYPHS.len() - 1)]
}

/// A `width`-segment bar (`▮` lit, `▯` dark) for a menu item.
pub fn meter_bar(db: f64, width: usize) -> String {
    let lit = (meter_fraction(db) * width as f64).round() as usize;
    (0..width).map(|i| if i < lit { '▮' } else { '▯' }).collect()
}

/// A menu line for one source: name, bar, a clip mark (`!` in a slot kept
/// when not clipping) and the peak in dB (or "silent" at the bottom of the
/// meter), right-aligned in a fixed number of characters. The padding is
/// figure spaces (digit width), so the line keeps its width as the value
/// changes and an open menu does not resize ten times a second.
pub fn meter_label(name: &str, level: &TrackLevel) -> String {
    let value = if level.peak_db <= METER_RANGE_DB { "silent".to_string() } else { format!("{:.0} dB", level.peak_db.round() + 0.0) };
    let clip = if level.clipping() { '!' } else { '\u{2007}' };
    format!("{name}  {}{clip} {value:\u{2007}>6}", meter_bar(level.peak_db, 12))
}

/// The status-item suffix: one glyph per captured source (microphone first,
/// then system audio), or `None` when the recording has no audio.
pub fn meter_title(levels: &AudioLevels) -> Option<String> {
    let glyphs: String = [levels.microphone, levels.system_audio].iter().flatten().map(|l| meter_glyph(l.peak_db)).collect();
    (!glyphs.is_empty()).then_some(glyphs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_to_db() {
        assert_eq!(db_from_linear(1.0), 0.0);
        assert_eq!(db_from_linear(0.5), -6.0);
        assert_eq!(db_from_linear(0.1), -20.0);
        assert_eq!(db_from_linear(0.0), FLOOR_DB);
        assert_eq!(db_from_linear(-1.0), FLOOR_DB);
        assert_eq!(db_from_linear(f32::NAN), FLOOR_DB);
        assert_eq!(db_from_linear(1e-9), FLOOR_DB);
        assert_eq!(db_from_linear(f32::INFINITY), FLOOR_DB);
        assert_eq!(db_from_linear(1e6), 20.0);
    }

    #[test]
    fn meter_scale() {
        assert_eq!(meter_fraction(0.0), 1.0);
        assert_eq!(meter_fraction(3.0), 1.0);
        assert_eq!(meter_fraction(-30.0), 0.5);
        assert_eq!(meter_fraction(-60.0), 0.0);
        assert_eq!(meter_fraction(FLOOR_DB), 0.0);
        assert_eq!(meter_fraction(f64::NAN), 0.0);
    }

    #[test]
    fn glyphs_cover_the_range() {
        assert_eq!(meter_glyph(FLOOR_DB), '▁');
        assert_eq!(meter_glyph(-59.0), '▁');
        assert_eq!(meter_glyph(-30.0), '▄');
        assert_eq!(meter_glyph(-29.0), '▅');
        assert_eq!(meter_glyph(0.0), '█');
        assert_eq!(meter_glyph(-0.1), '█');
        // Monotonic.
        let mut last = '▁';
        for i in 0..=70 {
            let g = meter_glyph(-(70 - i) as f64);
            assert!(g >= last);
            last = g;
        }
    }

    #[test]
    fn bars_and_labels() {
        assert_eq!(meter_bar(0.0, 4), "▮▮▮▮");
        assert_eq!(meter_bar(-30.0, 4), "▮▮▯▯");
        assert_eq!(meter_bar(FLOOR_DB, 4), "▯▯▯▯");
        let quiet = TrackLevel { peak_db: FLOOR_DB, rms_db: FLOOR_DB };
        assert_eq!(meter_label("Microphone", &quiet), "Microphone  ▯▯▯▯▯▯▯▯▯▯▯▯\u{2007} silent");
        let loud = TrackLevel { peak_db: -0.2, rms_db: -6.0 };
        assert!(loud.clipping());
        assert_eq!(meter_label("System Audio", &loud), "System Audio  ▮▮▮▮▮▮▮▮▮▮▮▮! \u{2007}\u{2007}0 dB");
        let mid = TrackLevel::from_linear(0.1, 0.05);
        assert_eq!(meter_label("Microphone", &mid), "Microphone  ▮▮▮▮▮▮▮▮▯▯▯▯\u{2007} -20 dB");
        // Every value takes the same number of characters.
        let width = |db: f64| meter_label("M", &TrackLevel { peak_db: db, rms_db: db }).chars().count();
        assert!([-100.0, -45.0, -6.0, -0.2, 0.0].iter().all(|&db| width(db) == width(-18.0)));
    }

    #[test]
    fn title_lists_present_sources_in_order() {
        assert_eq!(meter_title(&AudioLevels::default()), None);
        let mic = TrackLevel { peak_db: 0.0, rms_db: -3.0 };
        let sys = TrackLevel { peak_db: FLOOR_DB, rms_db: FLOOR_DB };
        assert_eq!(meter_title(&AudioLevels { microphone: Some(mic), system_audio: Some(sys) }).as_deref(), Some("█▁"));
        assert_eq!(meter_title(&AudioLevels { microphone: None, system_audio: Some(sys) }).as_deref(), Some("▁"));
    }

    #[test]
    fn serializes_with_nulls_for_missing_sources() {
        let l = AudioLevels { microphone: Some(TrackLevel { peak_db: -12.5, rms_db: -20.0 }), system_audio: None };
        assert_eq!(
            serde_json::to_value(l).unwrap(),
            serde_json::json!({"microphone": {"peak_db": -12.5, "rms_db": -20.0}, "system_audio": null})
        );
    }
}
