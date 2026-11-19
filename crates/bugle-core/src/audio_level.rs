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

/// Characters in every [`meter_gauge`]: 12 bar cells, the clip mark, a
/// space and the 6-character value.
pub const METER_GAUGE_CHARS: usize = 20;

/// The level part of a source's menu line: bar, a clip mark (`!`, or a
/// space when not clipping) and the peak in dB (or "silent" at the bottom
/// of the meter, also for -inf or NaN), right-aligned with plain spaces to
/// [`METER_GAUGE_CHARS`] characters whatever the level. The app draws it in
/// a monospaced font, where a constant character count is a constant width,
/// so the open menu does not resize as the level moves (the menu font's
/// digits, minus sign and "silent" all differ in width).
pub fn meter_gauge(level: &TrackLevel) -> String {
    let value = if level.peak_db > METER_RANGE_DB { format!("{:.0} dB", level.peak_db.min(20.0).round() + 0.0) } else { "silent".to_string() };
    let clip = if level.clipping() { '!' } else { ' ' };
    format!("{}{clip} {value:>6}", meter_bar(level.peak_db, 12))
}

/// A source's menu line in two parts: the name (drawn in the menu font,
/// with the two spaces after it) and its [`meter_gauge`] (drawn
/// monospaced). The name is constant per line, so the line's width is too.
pub fn meter_label_parts(name: &str, level: &TrackLevel) -> (String, String) {
    (format!("{name}  "), meter_gauge(level))
}

/// A source's whole menu line as plain text: [`meter_label_parts`] joined.
pub fn meter_label(name: &str, level: &TrackLevel) -> String {
    let (head, gauge) = meter_label_parts(name, level);
    head + &gauge
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
        assert_eq!(meter_label("Microphone", &quiet), "Microphone  ▯▯▯▯▯▯▯▯▯▯▯▯  silent");
        let loud = TrackLevel { peak_db: -0.2, rms_db: -6.0 };
        assert!(loud.clipping());
        assert_eq!(meter_label("System Audio", &loud), "System Audio  ▮▮▮▮▮▮▮▮▮▮▮▮!   0 dB");
        let mid = TrackLevel::from_linear(0.1, 0.05);
        assert_eq!(meter_label("Microphone", &mid), "Microphone  ▮▮▮▮▮▮▮▮▯▯▯▯  -20 dB");
        assert_eq!(meter_label_parts("Microphone", &mid), ("Microphone  ".to_string(), "▮▮▮▮▮▮▮▮▯▯▯▯  -20 dB".to_string()));
    }

    /// Levels across the whole range: silence, the floor, -inf and NaN
    /// (never reported, but harmless), the rounding boundaries, clipping and
    /// overshoot above full scale.
    fn every_level() -> Vec<TrackLevel> {
        let mut dbs = vec![f64::NEG_INFINITY, f64::NAN, FLOOR_DB, METER_RANGE_DB, -59.6, -59.4, -9.6, -9.4, -0.6, -0.5, -0.4, -0.04, 0.0];
        dbs.extend((0..=1200).map(|i| -100.0 + i as f64 * 0.1));
        dbs.extend([0.4, 9.6, 20.0, f64::INFINITY]);
        dbs.into_iter().map(|db| TrackLevel { peak_db: db, rms_db: db }).collect()
    }

    #[test]
    fn gauge_width_is_constant() {
        // Only characters that are one cell wide in a monospaced font: the
        // bar cells, digits, minus, plain spaces and the letters of "dB" and
        // "silent" (no figure spaces: a monospaced font may not have them).
        let allowed = |c: char| matches!(c, '▮' | '▯' | '!' | ' ' | '-' | '0'..='9' | 'd' | 'B' | 's' | 'i' | 'l' | 'e' | 'n' | 't');
        for l in every_level() {
            let g = meter_gauge(&l);
            assert_eq!(g.chars().count(), METER_GAUGE_CHARS, "{}: {g:?}", l.peak_db);
            assert!(g.chars().all(allowed), "{}: {g:?}", l.peak_db);
            assert!(g.ends_with(" dB") || g.ends_with(" silent"), "{g:?}");
        }
        // Both sources' lines keep one length at every level.
        for name in ["Microphone", "System Audio"] {
            let lens: std::collections::BTreeSet<_> = every_level().iter().map(|l| meter_label(name, l).chars().count()).collect();
            assert_eq!(lens.len(), 1, "{name}: {lens:?}");
        }
        let at = |db: f64| meter_gauge(&TrackLevel { peak_db: db, rms_db: db });
        assert_eq!(at(f64::NEG_INFINITY), "▯▯▯▯▯▯▯▯▯▯▯▯  silent");
        assert_eq!(at(f64::NAN), "▯▯▯▯▯▯▯▯▯▯▯▯  silent");
        assert_eq!(at(-59.6), "▯▯▯▯▯▯▯▯▯▯▯▯  -60 dB");
        assert_eq!(at(-0.04), "▮▮▮▮▮▮▮▮▮▮▮▮!   0 dB");
        assert_eq!(at(20.0), "▮▮▮▮▮▮▮▮▮▮▮▮!  20 dB");
        // Never reported (levels are clamped to +20), but kept in width.
        assert!(at(f64::INFINITY).ends_with("!  20 dB"));
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
