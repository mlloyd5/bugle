//! Audio levels for recordings: per-source gain, optional ducking of system
//! audio under the microphone, and a peak limiter that keeps the result
//! below full scale. [`Mixer`] folds the sources into one track;
//! [`Leveler`] applies the same levels to each source's own track when they
//! are kept separate.
//!
//! The math is plain sample processing on interleaved `f32` buffers, so it is
//! tested here on synthetic signals; the platform crate feeds it the decoded
//! tracks of a finished recording and encodes what it returns.

use serde::{Deserialize, Serialize};

/// Highest per-source gain accepted (+18 dB).
pub const MAX_GAIN: f32 = 8.0;
/// Most ducking accepted, in dB.
pub const MAX_DUCK_DB: f32 = 60.0;
/// Limiter ceiling: -0.3 dBFS, a little headroom for the AAC encoder
/// (decoded AAC can still overshoot the ceiling slightly).
pub const LIMIT_CEILING: f32 = 0.966;
/// Limiter release time.
const LIMIT_RELEASE_MS: f32 = 100.0;
/// Release of the ducking detector: holds the "mic is active" state through
/// zero crossings and short gaps between syllables.
const DETECT_RELEASE_MS: f32 = 60.0;

/// Mix settings: `[recordings]` keys in the config file.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioMixConfig {
    /// Microphone level in the mixed track: a linear factor on its default
    /// share of the mix (`1/n` of `n` sources), so 1.0 = the default share.
    pub microphone_gain: f32,
    /// System-audio level in the mixed track: a linear factor on its default
    /// share of the mix (`1/n` of `n` sources), so 1.0 = the default share.
    pub system_audio_gain: f32,
    /// Lower system audio while the microphone is active.
    pub ducking: bool,
    /// How far system audio is lowered while ducked, in dB.
    pub ducking_amount_db: f32,
    /// Microphone level (dBFS peak, as recorded: before `microphone_gain`)
    /// above which it counts as active.
    pub ducking_threshold_db: f32,
    /// Time for system audio to fade down once the microphone is active.
    pub ducking_attack_ms: f32,
    /// Time for system audio to come back once the microphone goes quiet.
    pub ducking_release_ms: f32,
}

impl Default for AudioMixConfig {
    fn default() -> Self {
        Self {
            microphone_gain: 1.0,
            system_audio_gain: 1.0,
            ducking: false,
            ducking_amount_db: 12.0,
            ducking_threshold_db: -40.0,
            ducking_attack_ms: 10.0,
            ducking_release_ms: 500.0,
        }
    }
}

/// Per-recording overrides of [`AudioMixConfig`] (CLI flags, API arguments).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioMixOverrides {
    /// Microphone level in the mixed track: a linear factor on its default
    /// share of the mix (`1/n` of `n` sources), so 1.0 = the default share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone_gain: Option<f32>,
    /// System-audio level in the mixed track: a linear factor on its default
    /// share of the mix (`1/n` of `n` sources), so 1.0 = the default share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_audio_gain: Option<f32>,
    /// Duck system audio under the microphone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ducking: Option<bool>,
    /// How far system audio is lowered while ducked, in dB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ducking_amount_db: Option<f32>,
}

impl AudioMixOverrides {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl AudioMixConfig {
    /// Whether these levels change the sources as recorded: a gain other
    /// than 1, or ducking with an audible microphone. Separate tracks are
    /// re-encoded only then.
    pub fn changes_tracks(&self) -> bool {
        self.microphone_gain != 1.0 || self.system_audio_gain != 1.0 || (self.ducking && self.microphone_gain > 0.0)
    }

    /// These settings with `o` applied on top.
    pub fn with(mut self, o: &AudioMixOverrides) -> Self {
        if let Some(g) = o.microphone_gain {
            self.microphone_gain = g;
        }
        if let Some(g) = o.system_audio_gain {
            self.system_audio_gain = g;
        }
        if let Some(d) = o.ducking {
            self.ducking = d;
        }
        if let Some(a) = o.ducking_amount_db {
            self.ducking_amount_db = a;
        }
        self
    }

    /// Reject values that are not finite or out of range.
    pub fn validate(&self) -> Result<(), String> {
        for (name, g) in [("microphone_gain", self.microphone_gain), ("system_audio_gain", self.system_audio_gain)] {
            if !g.is_finite() || !(0.0..=MAX_GAIN).contains(&g) {
                return Err(format!("{name} = {g} is out of range (0 to {MAX_GAIN})"));
            }
        }
        if !self.ducking_amount_db.is_finite() || !(0.0..=MAX_DUCK_DB).contains(&self.ducking_amount_db) {
            return Err(format!("ducking_amount_db = {} is out of range (0 to {MAX_DUCK_DB})", self.ducking_amount_db));
        }
        if !self.ducking_threshold_db.is_finite() || !(-96.0..=0.0).contains(&self.ducking_threshold_db) {
            return Err(format!("ducking_threshold_db = {} is out of range (-96 to 0)", self.ducking_threshold_db));
        }
        for (name, ms) in [("ducking_attack_ms", self.ducking_attack_ms), ("ducking_release_ms", self.ducking_release_ms)] {
            if !ms.is_finite() || !(0.0..=10_000.0).contains(&ms) {
                return Err(format!("{name} = {ms} is out of range (0 to 10000)"));
            }
        }
        Ok(())
    }
}

pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// One-pole smoothing coefficient for a time constant of `ms` (0 = instant).
fn coef(ms: f32, sample_rate: f32) -> f32 {
    if ms <= 0.0 {
        0.0
    } else {
        (-1.0 / (ms / 1000.0 * sample_rate)).exp()
    }
}

/// Gain for the ducked source, driven by the microphone's level: a peak
/// detector (instant rise, short release) decides whether the microphone is
/// active, and the gain follows towards the ducked level with the attack
/// time and back to unity with the release time.
#[derive(Debug, Clone)]
pub struct Ducker {
    floor: f32,
    threshold: f32,
    detect_release: f32,
    attack: f32,
    release: f32,
    level: f32,
    gain: f32,
}

impl Ducker {
    pub fn new(cfg: &AudioMixConfig, sample_rate: f32) -> Self {
        Self {
            floor: db_to_linear(-cfg.ducking_amount_db),
            threshold: db_to_linear(cfg.ducking_threshold_db),
            detect_release: coef(DETECT_RELEASE_MS, sample_rate),
            attack: coef(cfg.ducking_attack_ms, sample_rate),
            release: coef(cfg.ducking_release_ms, sample_rate),
            level: 0.0,
            gain: 1.0,
        }
    }

    /// Feed one frame's key level (peak of the microphone frame); returns
    /// the gain for the ducked source at that frame.
    pub fn next(&mut self, key: f32) -> f32 {
        self.level = if key > self.level { key } else { key + self.detect_release * (self.level - key) };
        let target = if self.level >= self.threshold { self.floor } else { 1.0 };
        let c = if target < self.gain { self.attack } else { self.release };
        self.gain = target + c * (self.gain - target);
        self.gain
    }
}

/// Stereo-linked peak limiter: instant attack (no frame ever leaves above
/// the ceiling) and a smooth release back to unity.
#[derive(Debug, Clone)]
pub struct Limiter {
    ceiling: f32,
    release: f32,
    gain: f32,
}

impl Limiter {
    pub fn new(ceiling: f32, sample_rate: f32) -> Self {
        Self { ceiling, release: coef(LIMIT_RELEASE_MS, sample_rate), gain: 1.0 }
    }

    /// Scale one interleaved frame in place.
    pub fn apply(&mut self, frame: &mut [f32]) {
        let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let need = if peak > self.ceiling { self.ceiling / peak } else { 1.0 };
        self.gain = if need < self.gain { need } else { need + self.release * (self.gain - need) };
        for s in frame {
            *s *= self.gain;
        }
    }

    pub fn gain(&self) -> f32 {
        self.gain
    }
}

/// Mixes the decoded audio tracks of a recording into one. Track 0 is
/// system audio and track 1 the microphone (the order the stream recorder
/// writes them); further tracks are mixed at unity. Each of the `n` tracks
/// gets a base share of `1/n`, so at gains of 1 or below the sum cannot
/// exceed full scale (ducking only lowers it) and is passed through as is;
/// with any gain above 1 the limiter catches the peaks past the ceiling.
/// Ducking is keyed on the microphone as recorded, and is off while
/// `microphone_gain` is 0 (a muted mic does not duck anything).
#[derive(Debug, Clone)]
pub struct Mixer {
    channels: usize,
    shares: Vec<f32>,
    ducker: Option<Ducker>,
    limiter: Option<Limiter>,
}

/// Index of the system-audio track.
pub const SYSTEM_TRACK: usize = 0;
/// Index of the microphone track.
pub const MIC_TRACK: usize = 1;

impl Mixer {
    pub fn new(cfg: &AudioMixConfig, tracks: usize, channels: usize, sample_rate: f32) -> Self {
        let base = 1.0 / tracks.max(1) as f32;
        let shares = (0..tracks)
            .map(|i| match i {
                SYSTEM_TRACK => base * cfg.system_audio_gain,
                MIC_TRACK => base * cfg.microphone_gain,
                _ => base,
            })
            .collect();
        let ducker = (cfg.ducking && tracks > MIC_TRACK && cfg.microphone_gain > 0.0).then(|| Ducker::new(cfg, sample_rate));
        let boosted = cfg.microphone_gain > 1.0 || cfg.system_audio_gain > 1.0;
        let limiter = boosted.then(|| Limiter::new(LIMIT_CEILING, sample_rate));
        Self { channels: channels.max(1), shares, ducker, limiter }
    }

    /// Mix `inputs` (one interleaved buffer per track, each at least as long
    /// as `out`) into `out`. State carries over between calls, so a track
    /// may be fed in blocks of any size.
    pub fn process(&mut self, inputs: &[&[f32]], out: &mut [f32]) {
        let ch = self.channels;
        for (f, frame) in out.chunks_exact_mut(ch).enumerate() {
            let at = f * ch;
            let duck = match (&mut self.ducker, inputs.get(MIC_TRACK)) {
                (Some(d), Some(mic)) => {
                    let key = mic[at..at + ch].iter().fold(0.0f32, |m, s| m.max(clean(*s).abs()));
                    d.next(key)
                }
                _ => 1.0,
            };
            for (c, o) in frame.iter_mut().enumerate() {
                let mut sum = 0.0f32;
                for (t, input) in inputs.iter().enumerate() {
                    let mut g = self.shares.get(t).copied().unwrap_or(0.0);
                    if t == SYSTEM_TRACK {
                        g *= duck;
                    }
                    sum += g * clean(input[at + c]);
                }
                *o = sum;
            }
            if let Some(l) = &mut self.limiter {
                l.apply(frame);
            }
        }
    }
}

/// Applies the levels to each source's own track, for recordings whose
/// audio stays in separate tracks. Same track order as [`Mixer`] (track 0
/// system audio, track 1 the microphone, further tracks at unity), but
/// without the `1/n` share: a gain of 1 leaves a track as recorded. System
/// audio is ducked under the microphone as in the mix (keyed on the mic as
/// recorded, off while `microphone_gain` is 0), and a track whose gain is
/// above 1 gets its own peak limiter.
#[derive(Debug, Clone)]
pub struct Leveler {
    channels: usize,
    gains: Vec<f32>,
    ducker: Option<Ducker>,
    limiters: Vec<Option<Limiter>>,
}

impl Leveler {
    pub fn new(cfg: &AudioMixConfig, tracks: usize, channels: usize, sample_rate: f32) -> Self {
        let gains: Vec<f32> = (0..tracks)
            .map(|i| match i {
                SYSTEM_TRACK => cfg.system_audio_gain,
                MIC_TRACK => cfg.microphone_gain,
                _ => 1.0,
            })
            .collect();
        let ducker = (cfg.ducking && tracks > MIC_TRACK && cfg.microphone_gain > 0.0).then(|| Ducker::new(cfg, sample_rate));
        let limiters = gains.iter().map(|g| (*g > 1.0).then(|| Limiter::new(LIMIT_CEILING, sample_rate))).collect();
        Self { channels: channels.max(1), gains, ducker, limiters }
    }

    /// Level `inputs` (one interleaved buffer per track, all of the same
    /// length `len`) into `out`, which holds one run of `len` samples per
    /// track, in track order. State carries over between calls, so tracks
    /// may be fed in blocks of any size.
    pub fn process(&mut self, inputs: &[&[f32]], out: &mut [f32]) {
        let ch = self.channels;
        let len = inputs.first().map_or(0, |i| i.len());
        for f in 0..len / ch {
            let at = f * ch;
            let duck = match (&mut self.ducker, inputs.get(MIC_TRACK)) {
                (Some(d), Some(mic)) => {
                    let key = mic[at..at + ch].iter().fold(0.0f32, |m, s| m.max(clean(*s).abs()));
                    d.next(key)
                }
                _ => 1.0,
            };
            for (t, input) in inputs.iter().enumerate() {
                let Some(frame) = out.get_mut(t * len + at..t * len + at + ch) else { break };
                let mut g = self.gains.get(t).copied().unwrap_or(1.0);
                if t == SYSTEM_TRACK {
                    g *= duck;
                }
                for (c, o) in frame.iter_mut().enumerate() {
                    *o = g * clean(input[at + c]);
                }
                if let Some(Some(l)) = self.limiters.get_mut(t) {
                    l.apply(frame);
                }
            }
        }
    }
}

/// Treat NaN and infinities as silence.
fn clean(s: f32) -> f32 {
    if s.is_finite() {
        s
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// Interleaved stereo sine, `secs` long.
    fn sine(freq: f32, amp: f32, secs: f32) -> Vec<f32> {
        let n = (secs * SR) as usize;
        (0..n)
            .flat_map(|i| {
                let v = amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin();
                [v, v]
            })
            .collect()
    }

    fn peak(s: &[f32]) -> f32 {
        s.iter().fold(0.0f32, |m, v| m.max(v.abs()))
    }

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|v| v * v).sum::<f32>() / s.len().max(1) as f32).sqrt()
    }

    fn mix(cfg: &AudioMixConfig, inputs: &[&[f32]]) -> Vec<f32> {
        let mut out = vec![0.0; inputs[0].len()];
        Mixer::new(cfg, inputs.len(), 2, SR).process(inputs, &mut out);
        out
    }

    #[test]
    fn unity_gains_give_each_source_half() {
        let sys = sine(440.0, 0.5, 0.2);
        let mic = vec![0.0; sys.len()];
        let out = mix(&AudioMixConfig::default(), &[&sys, &mic]);
        for (o, s) in out.iter().zip(&sys) {
            assert!((o - s * 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn default_levels_are_the_plain_sum() {
        // Loud, in phase: the sum peaks at 0.99, between the limiter ceiling
        // and full scale. Without a gain above 1 nothing touches it.
        let a = sine(50.0, 0.99, 0.5);
        let b = sine(50.0, 0.99, 0.5);
        for cfg in [AudioMixConfig::default(), AudioMixConfig { microphone_gain: 0.7, system_audio_gain: 1.0, ..Default::default() }] {
            let out = mix(&cfg, &[&a, &b]);
            let plain: Vec<f32> = a.iter().zip(&b).map(|(x, y)| cfg.system_audio_gain * 0.5 * x + cfg.microphone_gain * 0.5 * y).collect();
            assert_eq!(out, plain, "{cfg:?}");
        }
        assert!(peak(&mix(&AudioMixConfig::default(), &[&a, &b])) > LIMIT_CEILING);
    }

    #[test]
    fn muted_mic_does_not_duck() {
        let sys = sine(220.0, 0.5, 0.5);
        let loud_mic = sine(1000.0, 0.5, 0.5);
        let cfg = AudioMixConfig { ducking: true, microphone_gain: 0.0, ..Default::default() };
        let out = mix(&cfg, &[&sys, &loud_mic]);
        let plain: Vec<f32> = sys.iter().map(|s| 0.5 * s).collect();
        assert_eq!(out, plain);
        // The same mic at any audible gain does duck.
        let ducked = mix(&AudioMixConfig { microphone_gain: 0.1, ..cfg }, &[&sys, &loud_mic]);
        let sys_part: Vec<f32> = ducked.iter().zip(&loud_mic).map(|(o, m)| o - 0.05 * m).collect();
        assert!(rms(&sys_part[sys.len() / 2..]) < rms(&plain) * 0.3);
    }

    #[test]
    fn gains_scale_each_source() {
        let sys = sine(440.0, 0.4, 0.2);
        let mic = sine(660.0, 0.4, 0.2);
        let zero = vec![0.0; sys.len()];
        let cfg = AudioMixConfig { microphone_gain: 1.5, system_audio_gain: 0.25, ..Default::default() };
        let only_sys = mix(&cfg, &[&sys, &zero]);
        let only_mic = mix(&cfg, &[&zero, &mic]);
        assert!((peak(&only_sys) - 0.4 * 0.5 * 0.25).abs() < 1e-3, "{}", peak(&only_sys));
        assert!((peak(&only_mic) - 0.4 * 0.5 * 1.5).abs() < 1e-3, "{}", peak(&only_mic));
        // Zero gain mutes the source.
        let muted = mix(&AudioMixConfig { system_audio_gain: 0.0, ..Default::default() }, &[&sys, &zero]);
        assert_eq!(peak(&muted), 0.0);
    }

    fn level(cfg: &AudioMixConfig, inputs: &[&[f32]]) -> Vec<Vec<f32>> {
        let len = inputs[0].len();
        let mut out = vec![0.0; len * inputs.len()];
        Leveler::new(cfg, inputs.len(), 2, SR).process(inputs, &mut out);
        out.chunks(len).map(<[f32]>::to_vec).collect()
    }

    #[test]
    fn changes_tracks_only_for_audible_changes() {
        assert!(!AudioMixConfig::default().changes_tracks());
        assert!(AudioMixConfig { microphone_gain: 0.5, ..Default::default() }.changes_tracks());
        assert!(AudioMixConfig { system_audio_gain: 2.0, ..Default::default() }.changes_tracks());
        assert!(AudioMixConfig { ducking: true, ..Default::default() }.changes_tracks());
        // Ducking under a muted mic does nothing, but the mute itself does.
        assert!(AudioMixConfig { ducking: true, microphone_gain: 0.0, ..Default::default() }.changes_tracks());
        assert!(!AudioMixConfig { ducking_amount_db: 30.0, ..Default::default() }.changes_tracks());
    }

    #[test]
    fn leveler_scales_each_track_on_its_own() {
        let sys = sine(440.0, 0.4, 0.2);
        let mic = sine(660.0, 0.4, 0.2);
        // Unity: the tracks come out as they went in.
        let out = level(&AudioMixConfig::default(), &[&sys, &mic]);
        assert_eq!(out, [sys.clone(), mic.clone()]);
        let cfg = AudioMixConfig { system_audio_gain: 0.25, microphone_gain: 2.0, ..Default::default() };
        let out = level(&cfg, &[&sys, &mic]);
        for (o, s) in out[0].iter().zip(&sys) {
            assert!((o - 0.25 * s).abs() < 1e-6);
        }
        for (o, m) in out[1].iter().zip(&mic) {
            assert!((o - 2.0 * m).abs() < 1e-6);
        }
        // A third track is passed through.
        let extra = sine(330.0, 0.3, 0.2);
        let out = level(&cfg, &[&sys, &mic, &extra]);
        assert_eq!(out[2], extra);
    }

    #[test]
    fn leveler_ducks_system_audio_and_limits_loud_tracks() {
        let sys = sine(220.0, 0.5, 0.5);
        let mic = sine(1000.0, 0.5, 0.5);
        let cfg = AudioMixConfig { ducking: true, ducking_amount_db: 12.0, ..Default::default() };
        let out = level(&cfg, &[&sys, &mic]);
        // Once the attack settles system audio sits 12 dB down; the mic is
        // untouched.
        let tail = sys.len() / 2;
        let ratio = rms(&out[0][tail..]) / rms(&sys[tail..]);
        assert!((ratio - db_to_linear(-12.0)).abs() < 0.01, "ducked ratio {ratio}");
        assert_eq!(out[1], mic);
        // A muted mic does not duck.
        let out = level(&AudioMixConfig { microphone_gain: 0.0, ..cfg }, &[&sys, &mic]);
        assert_eq!(out[0], sys);
        assert_eq!(peak(&out[1]), 0.0);
        // A full-scale track at 4x is limited on its own, not clipped.
        let loud = sine(440.0, 1.0, 0.5);
        let out = level(&AudioMixConfig { microphone_gain: 4.0, ..Default::default() }, &[&sys, &loud]);
        assert!(peak(&out[1]) <= LIMIT_CEILING + 1e-6 && peak(&out[1]) > 0.9 * LIMIT_CEILING);
        assert_eq!(out[0], sys, "the other track is not limited");
    }

    #[test]
    fn limiter_keeps_loud_gains_below_full_scale() {
        // Two in-phase full-scale tones at 4x: an unlimited sum peaks at 4.0.
        let a = sine(440.0, 1.0, 0.5);
        let cfg = AudioMixConfig { microphone_gain: 4.0, system_audio_gain: 4.0, ..Default::default() };
        let out = mix(&cfg, &[&a, &a]);
        assert!(peak(&out) <= LIMIT_CEILING + 1e-6, "peak {}", peak(&out));
        // Still loud: limited, not silenced.
        assert!(peak(&out) > 0.9 * LIMIT_CEILING);
    }

    #[test]
    fn limiter_leaves_quiet_signals_alone_and_releases() {
        let mut l = Limiter::new(LIMIT_CEILING, SR);
        let mut quiet = [0.3f32, -0.3];
        l.apply(&mut quiet);
        assert_eq!(quiet, [0.3, -0.3]);
        let mut loud = [2.0f32, 1.0];
        l.apply(&mut loud);
        assert!((loud[0] - LIMIT_CEILING).abs() < 1e-6 && (loud[1] - LIMIT_CEILING / 2.0).abs() < 1e-6);
        // After a peak the gain recovers smoothly towards unity.
        let g0 = l.gain();
        for _ in 0..(SR as usize / 10) {
            l.apply(&mut [0.0, 0.0]);
        }
        let g1 = l.gain();
        for _ in 0..(SR as usize) {
            l.apply(&mut [0.0, 0.0]);
        }
        assert!(g0 < g1 && g1 < 1.0 && l.gain() > 0.999, "{g0} {g1} {}", l.gain());
    }

    #[test]
    fn ducking_lowers_system_audio_while_mic_is_active() {
        // System audio throughout; the mic speaks for the middle second.
        let secs = 3.0;
        let sys = sine(220.0, 0.5, secs);
        let frames = sys.len() / 2;
        let mut mic = vec![0.0f32; sys.len()];
        let talk = sine(1000.0, 0.3, secs);
        let (on, off) = (frames / 3 * 2, frames * 2 / 3 * 2);
        mic[on..off].copy_from_slice(&talk[on..off]);
        let cfg = AudioMixConfig { ducking: true, ducking_amount_db: 12.0, ducking_release_ms: 150.0, ..Default::default() };
        let ducked = mix(&cfg, &[&sys, &mic]);
        // Measure system audio alone by subtracting the mic's share.
        let sys_part: Vec<f32> = ducked.iter().zip(&mic).map(|(o, m)| o - 0.5 * m).collect();
        let sec = |i: usize| &sys_part[(i * frames / 3 + frames / 12) * 2..((i + 1) * frames / 3 - frames / 12) * 2];
        let base = rms(&sine(220.0, 0.25, 0.5));
        let (before, during, after) = (rms(sec(0)), rms(sec(1)), rms(sec(2)));
        assert!((before - base).abs() / base < 0.02, "before {before} vs {base}");
        let want = base * db_to_linear(-12.0);
        assert!((during - want).abs() / want < 0.05, "during {during} vs {want}");
        // Release is 150 ms: in the last second the gain is well on its way
        // back, and by its end it has recovered.
        assert!(after > during * 2.0, "after {after}");
        let tail = &sys_part[(frames - frames / 12) * 2..];
        assert!((rms(tail) - base).abs() / base < 0.05, "tail {}", rms(tail));
    }

    #[test]
    fn ducking_ignores_mic_noise_below_threshold() {
        let sys = sine(220.0, 0.5, 1.0);
        // Noise floor around -50 dBFS, below the -40 dBFS default threshold.
        let mic: Vec<f32> = (0..sys.len()).map(|i| if i % 2 == 0 { 0.003 } else { -0.003 }).collect();
        let cfg = AudioMixConfig { ducking: true, ..Default::default() };
        let out = mix(&cfg, &[&sys, &mic]);
        let sys_part: Vec<f32> = out.iter().zip(&mic).map(|(o, m)| o - 0.5 * m).collect();
        assert!((rms(&sys_part) - rms(&sys) * 0.5).abs() < 1e-4);
    }

    #[test]
    fn ducker_attack_and_release_are_smooth() {
        let cfg = AudioMixConfig { ducking: true, ducking_attack_ms: 10.0, ducking_release_ms: 200.0, ..Default::default() };
        let mut d = Ducker::new(&cfg, SR);
        let floor = db_to_linear(-12.0);
        let mut prev = 1.0;
        // Attack: monotonic fall, about 63% of the way after one time constant.
        let tc = (0.010 * SR) as usize;
        for i in 0..tc * 10 {
            let g = d.next(0.5);
            assert!(g <= prev + 1e-7);
            if i + 1 == tc {
                let done = (1.0 - g) / (1.0 - floor);
                assert!((done - 0.632).abs() < 0.02, "attack progress {done}");
            }
            prev = g;
        }
        assert!((prev - floor).abs() < 1e-3);
        // Release: monotonic rise back to unity (after the detector's hold).
        for _ in 0..(SR as usize * 2) {
            let g = d.next(0.0);
            assert!(g >= prev - 1e-7);
            prev = g;
        }
        assert!(prev > 0.999);
    }

    #[test]
    fn ducking_off_or_without_a_mic_track_changes_nothing() {
        let sys = sine(220.0, 0.5, 0.2);
        let loud_mic = sine(1000.0, 0.5, 0.2);
        let plain = mix(&AudioMixConfig::default(), &[&sys, &loud_mic]);
        let off = mix(&AudioMixConfig { ducking: false, ducking_amount_db: 40.0, ..Default::default() }, &[&sys, &loud_mic]);
        assert_eq!(plain, off);
        let one = mix(&AudioMixConfig { ducking: true, ..Default::default() }, &[&sys]);
        assert_eq!(one, sys);
    }

    #[test]
    fn block_size_does_not_change_the_result() {
        let sys = sine(220.0, 0.9, 1.0);
        let mic = sine(500.0, 0.9, 1.0);
        let cfg = AudioMixConfig { ducking: true, microphone_gain: 3.0, system_audio_gain: 2.0, ..Default::default() };
        let whole = mix(&cfg, &[&sys, &mic]);
        let mut m = Mixer::new(&cfg, 2, 2, SR);
        let mut blocks = vec![0.0; sys.len()];
        let mut at = 0;
        for size in [2, 1000, 4096, 6, 99_998].iter().cycle() {
            if at >= sys.len() {
                break;
            }
            let end = (at + size).min(sys.len());
            m.process(&[&sys[at..end], &mic[at..end]], &mut blocks[at..end]);
            at = end;
        }
        assert_eq!(whole, blocks);
    }

    #[test]
    fn non_finite_samples_are_silence() {
        let sys = [f32::NAN, f32::INFINITY, 0.5, 0.5];
        let mic = [0.0; 4];
        let out = mix(&AudioMixConfig::default(), &[&sys, &mic]);
        assert_eq!(out, [0.0, 0.0, 0.25, 0.25]);
    }

    #[test]
    fn overrides_and_validation() {
        let base = AudioMixConfig::default();
        assert!(base.validate().is_ok());
        let o = AudioMixOverrides { microphone_gain: Some(2.0), ducking: Some(true), ..Default::default() };
        let c = base.with(&o);
        assert_eq!((c.microphone_gain, c.system_audio_gain, c.ducking), (2.0, 1.0, true));
        assert!(AudioMixOverrides::default().is_empty() && !o.is_empty());
        for bad in [
            AudioMixConfig { microphone_gain: -1.0, ..base },
            AudioMixConfig { system_audio_gain: f32::NAN, ..base },
            AudioMixConfig { microphone_gain: 9.0, ..base },
            AudioMixConfig { ducking_amount_db: -3.0, ..base },
            AudioMixConfig { ducking_threshold_db: 6.0, ..base },
            AudioMixConfig { ducking_release_ms: -1.0, ..base },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn config_keys_parse_flat() {
        #[derive(Deserialize)]
        struct S {
            #[serde(flatten)]
            mix: AudioMixConfig,
        }
        let s: S = toml::from_str("microphone_gain = 2\nducking = true\nducking_amount_db = 18.5\n").unwrap();
        assert_eq!(s.mix.microphone_gain, 2.0);
        assert!(s.mix.ducking);
        assert_eq!(s.mix.ducking_amount_db, 18.5);
        assert_eq!(s.mix.system_audio_gain, 1.0);
    }

    #[test]
    fn recordings_section_and_request_carry_the_mix_keys() {
        let cfg: crate::Config =
            toml::from_str("[recordings]\nmix_audio = true\nsystem_audio_gain = 0.5\nducking = true\nvideo_fps = 24\n").unwrap();
        let r = &cfg.recordings;
        assert!(r.mix_audio && r.mix.ducking && r.video_fps == 24);
        assert_eq!((r.mix.system_audio_gain, r.mix.microphone_gain), (0.5, 1.0));
        // The saved form reads back the same.
        let back: crate::Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back.recordings.mix, r.mix);

        let req: crate::RecordRequest =
            serde_json::from_str(r#"{"mix_audio":true,"microphone_gain":2.5,"ducking":false}"#).unwrap();
        assert_eq!(req.mix.microphone_gain, Some(2.5));
        assert_eq!(req.mix.ducking, Some(false));
        assert_eq!(req.mix.system_audio_gain, None);
        let plain: crate::RecordRequest = serde_json::from_str("{}").unwrap();
        assert!(plain.mix.is_empty());
    }
}
