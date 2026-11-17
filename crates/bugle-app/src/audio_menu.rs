//! The *Audio Levels* submenu: microphone and system-audio levels and
//! ducking for the next recordings, without editing the config file by hand.
//!
//! A change takes effect from the next recording (it travels in the record
//! request as overrides of the levels the engine loaded) and is written back
//! to `[recordings]` in the config file, touching only the key that changed
//! so comments and the rest of the file stay as they were.

use anyhow::{Context, Result};
use bugle_core::audio_mix::{AudioMixConfig, AudioMixOverrides};
use std::path::Path;
use std::sync::Mutex;
use tray_icon::menu::{CheckMenuItem, PredefinedMenuItem, Submenu};

/// Menu item ids start with this.
const PREFIX: &str = "audio:";
const DUCK_ID: &str = "audio:duck";

/// Level presets offered for each source: linear gains (1.0 = as recorded,
/// or the default share of a mix), shown as percentages.
pub const PRESETS: &[f32] = &[0.0, 0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 4.0];

/// One of the two leveled sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Microphone,
    SystemAudio,
}

impl Source {
    fn tag(self) -> &'static str {
        match self {
            Source::Microphone => "mic",
            Source::SystemAudio => "sys",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Source::Microphone => "Microphone",
            Source::SystemAudio => "System Audio",
        }
    }

    /// The `[recordings]` key that holds this source's level.
    pub fn config_key(self) -> &'static str {
        match self {
            Source::Microphone => "microphone_gain",
            Source::SystemAudio => "system_audio_gain",
        }
    }

    fn gain(self, levels: &AudioMixConfig) -> f32 {
        match self {
            Source::Microphone => levels.microphone_gain,
            Source::SystemAudio => levels.system_audio_gain,
        }
    }
}

/// What a click in the submenu asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Change {
    Gain(Source, f32),
    Ducking(bool),
}

impl Change {
    /// `levels` with this change applied.
    pub fn apply(self, mut levels: AudioMixConfig) -> AudioMixConfig {
        match self {
            Change::Gain(Source::Microphone, g) => levels.microphone_gain = g,
            Change::Gain(Source::SystemAudio, g) => levels.system_audio_gain = g,
            Change::Ducking(d) => levels.ducking = d,
        }
        levels
    }
}

fn preset_id(source: Source, i: usize) -> String {
    format!("{PREFIX}{}:{i}", source.tag())
}

/// The change a menu item id stands for, or `None` for an id that is not
/// one of this submenu's. `ducking` is the state before the click.
pub fn parse_id(id: &str, ducking: bool) -> Option<Change> {
    if id == DUCK_ID {
        return Some(Change::Ducking(!ducking));
    }
    let rest = id.strip_prefix(PREFIX)?;
    let (tag, i) = rest.split_once(':')?;
    let source = [Source::Microphone, Source::SystemAudio].into_iter().find(|s| s.tag() == tag)?;
    let gain = *PRESETS.get(i.parse::<usize>().ok()?)?;
    Some(Change::Gain(source, gain))
}

/// "Muted", or the gain as a percentage.
pub fn gain_label(g: f32) -> String {
    if g == 0.0 {
        "Muted".into()
    } else {
        format!("{:.0}%", g * 100.0)
    }
}

/// The levels the app's recordings use, and those the engine loaded from
/// the config: the worker builds record requests from these.
struct Shared {
    current: AudioMixConfig,
    loaded: AudioMixConfig,
}

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);

/// The record-request overrides that turn the engine's loaded levels into
/// the ones picked in the menu (empty until the menu changes something).
pub fn overrides() -> AudioMixOverrides {
    let guard = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(s) => overrides_between(&s.loaded, &s.current),
        None => AudioMixOverrides::default(),
    }
}

/// Overrides for the fields the menu sets that differ between `loaded` and
/// `current`.
pub fn overrides_between(loaded: &AudioMixConfig, current: &AudioMixConfig) -> AudioMixOverrides {
    AudioMixOverrides {
        microphone_gain: (current.microphone_gain != loaded.microphone_gain).then_some(current.microphone_gain),
        system_audio_gain: (current.system_audio_gain != loaded.system_audio_gain).then_some(current.system_audio_gain),
        ducking: (current.ducking != loaded.ducking).then_some(current.ducking),
        ducking_amount_db: None,
    }
}

/// Write one change into the `[recordings]` table of the config file at
/// `path` (created if missing), keeping everything else in the file as it
/// is, including comments next to the value being replaced.
pub fn persist(path: &Path, change: Change) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: toml_edit::Document = text.parse().with_context(|| format!("parsing {}", path.display()))?;
    let (key, new): (&str, toml_edit::Value) = match change {
        Change::Gain(source, g) => (source.config_key(), toml_edit::Value::from(g as f64)),
        Change::Ducking(d) => ("ducking", toml_edit::Value::from(d)),
    };
    let recordings = doc
        .entry("recordings")
        .or_insert(toml_edit::table())
        .as_table_like_mut()
        .with_context(|| format!("{}: `recordings` is not a table", path.display()))?;
    match recordings.get_mut(key).and_then(|i| i.as_value_mut()) {
        Some(old) => {
            let decor = old.decor().clone();
            *old = new;
            *old.decor_mut() = decor;
        }
        None => {
            recordings.insert(key, toml_edit::Item::Value(new));
        }
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))
}

/// The submenu and its items.
pub struct AudioMenu {
    pub submenu: Submenu,
    sources: Vec<(Source, Submenu, Vec<CheckMenuItem>)>,
    duck: CheckMenuItem,
    levels: AudioMixConfig,
}

impl AudioMenu {
    /// Build the submenu showing `levels` (the config's `[recordings]`).
    pub fn new(levels: AudioMixConfig) -> Result<Self> {
        *SHARED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Shared { current: levels, loaded: levels });
        let submenu = Submenu::with_id("audio_levels", "Audio Levels", true);
        let mut sources = Vec::new();
        for source in [Source::Microphone, Source::SystemAudio] {
            let sub = Submenu::with_id(format!("{PREFIX}{}", source.tag()), source.title(), true);
            let items: Vec<CheckMenuItem> = PRESETS
                .iter()
                .enumerate()
                .map(|(i, g)| CheckMenuItem::with_id(preset_id(source, i), gain_label(*g), true, false, None))
                .collect();
            for item in &items {
                sub.append(item)?;
            }
            submenu.append(&sub)?;
            sources.push((source, sub, items));
        }
        submenu.append(&PredefinedMenuItem::separator())?;
        let duck = CheckMenuItem::with_id(DUCK_ID, "Duck System Audio While Talking", true, false, None);
        submenu.append(&duck)?;
        let menu = Self { submenu, sources, duck, levels };
        menu.sync();
        Ok(menu)
    }

    /// Show `self.levels`: the check marks (none when a level from the
    /// config is not a preset) and the current level in each title.
    fn sync(&self) {
        for (source, sub, items) in &self.sources {
            let g = source.gain(&self.levels);
            sub.set_text(format!("{}: {}", source.title(), gain_label(g)));
            for (item, preset) in items.iter().zip(PRESETS) {
                item.set_checked(*preset == g);
            }
        }
        self.duck.set_checked(self.levels.ducking);
    }

    /// Act on a click if `id` is one of this submenu's items: the next
    /// recordings use the new levels. Returns the change (to persist), or
    /// `None` for an id that is not an audio item.
    pub fn handle(&mut self, id: &str) -> Option<Change> {
        let change = parse_id(id, self.levels.ducking)?;
        self.levels = change.apply(self.levels);
        if let Some(s) = SHARED.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            s.current = self.levels;
        }
        // The menu toggles a check item on click; show what is set.
        self.sync();
        Some(change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::Config;

    #[test]
    fn ids_round_trip_and_presets_are_valid() {
        for source in [Source::Microphone, Source::SystemAudio] {
            for (i, g) in PRESETS.iter().enumerate() {
                assert_eq!(parse_id(&preset_id(source, i), false), Some(Change::Gain(source, *g)));
                let levels = Change::Gain(source, *g).apply(AudioMixConfig::default());
                levels.validate().unwrap();
            }
        }
        assert!(PRESETS.contains(&1.0), "the default level is a preset");
        assert_eq!(parse_id(DUCK_ID, false), Some(Change::Ducking(true)));
        assert_eq!(parse_id(DUCK_ID, true), Some(Change::Ducking(false)));
        for other in ["audio:mic:99", "audio:mic", "audio:xyz:1", "audio:mic:x", "record", "recent:/tmp/a", "audio_levels"] {
            assert_eq!(parse_id(other, false), None, "{other}");
        }
        assert_eq!(gain_label(0.0), "Muted");
        assert_eq!(gain_label(0.25), "25%");
        assert_eq!(gain_label(1.5), "150%");
    }

    #[test]
    fn overrides_carry_only_what_the_menu_changed() {
        let loaded = AudioMixConfig { system_audio_gain: 0.5, ..Default::default() };
        assert!(overrides_between(&loaded, &loaded).is_empty());
        let current = Change::Gain(Source::Microphone, 2.0).apply(Change::Ducking(true).apply(loaded));
        let o = overrides_between(&loaded, &current);
        assert_eq!((o.microphone_gain, o.system_audio_gain, o.ducking, o.ducking_amount_db), (Some(2.0), None, Some(true), None));
        // The engine merges them back into the menu's levels.
        assert_eq!(loaded.with(&o), current);
    }

    #[test]
    fn persist_rewrites_one_key_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let text = "# my config\n[general]\nsave_dir = \"~/Shots\"  # where\n\n[recordings]\nvideo_fps = 24\nmicrophone_gain = 1.0  # mic level\n";
        std::fs::write(&path, text).unwrap();
        persist(&path, Change::Gain(Source::Microphone, 0.5)).unwrap();
        persist(&path, Change::Gain(Source::SystemAudio, 1.5)).unwrap();
        persist(&path, Change::Ducking(true)).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.starts_with("# my config\n[general]\nsave_dir = \"~/Shots\"  # where\n"), "{out}");
        assert!(out.contains("microphone_gain = 0.5  # mic level\n"), "{out}");
        assert!(out.contains("video_fps = 24\n"), "{out}");
        let cfg = Config::load_from(&path).unwrap();
        let r = &cfg.recordings;
        assert_eq!((r.mix.microphone_gain, r.mix.system_audio_gain, r.mix.ducking, r.video_fps), (0.5, 1.5, true, 24));
        assert_eq!(cfg.general.save_dir, "~/Shots");
        persist(&path, Change::Ducking(false)).unwrap();
        assert!(!Config::load_from(&path).unwrap().recordings.mix.ducking);
    }

    #[test]
    fn persist_creates_the_file_and_the_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bugle").join("config.toml");
        persist(&path, Change::Gain(Source::SystemAudio, 0.25)).unwrap();
        assert_eq!(Config::load_from(&path).unwrap().recordings.mix.system_audio_gain, 0.25);
        // A file without [recordings] gets one; a dotted key form is updated
        // where it is.
        let other = tmp.path().join("other.toml");
        std::fs::write(&other, "[general]\nshutter_sound = false\n").unwrap();
        persist(&other, Change::Ducking(true)).unwrap();
        let cfg = Config::load_from(&other).unwrap();
        assert!(cfg.recordings.mix.ducking && !cfg.general.shutter_sound);
        let dotted = tmp.path().join("dotted.toml");
        std::fs::write(&dotted, "recordings.microphone_gain = 2.0\n").unwrap();
        persist(&dotted, Change::Gain(Source::Microphone, 0.75)).unwrap();
        assert_eq!(Config::load_from(&dotted).unwrap().recordings.mix.microphone_gain, 0.75);
        // Not TOML: refused, file untouched.
        let bad = tmp.path().join("bad.toml");
        std::fs::write(&bad, "this is = = not toml").unwrap();
        assert!(persist(&bad, Change::Ducking(true)).is_err());
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "this is = = not toml");
    }
}
