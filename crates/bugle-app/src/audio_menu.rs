//! The *Audio Levels* submenu: microphone and system-audio levels and
//! ducking for the next recordings, without editing the config file by hand.
//!
//! A change takes effect from the next recording, whether the menu, a
//! hotkey or the in-app agent API starts it (the app hands the new levels to
//! [`bugle_engine::Engine::set_recording_levels`]), and is written back to
//! `[recordings]` in the config file, touching only the key that changed so
//! comments and the rest of the file stay as they were.
//!
//! Levels apply only to recordings with both a microphone and a
//! system-audio track; when the config cannot produce those the submenu
//! says so in a disabled first line ([`requirement`]).

use anyhow::{Context, Result};
use bugle_core::audio_mix::AudioMixConfig;
use bugle_core::config::{RecordingBackend, RecordingConfig};
use std::io::Write;
use std::path::Path;
use tray_icon::menu::{CheckMenuItem, MenuItem, PredefinedMenuItem, Submenu};

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

/// Why the levels would not change the app's recordings with this
/// `[recordings]` config, as the submenu's disabled first line, or `None`
/// when they apply. The app's own recordings ask for neither the
/// microphone nor system audio, so both come from the config; leveling
/// needs both tracks, which only the ScreenCaptureKit recorder writes
/// (`sck_available`: macOS 14 or later).
pub fn requirement(cfg: &RecordingConfig, sck_available: bool) -> Option<&'static str> {
    if cfg.backend == RecordingBackend::SystemRecorder {
        Some("Needs recordings.backend = \"auto\" or \"stream\"")
    } else if !sck_available {
        Some("Needs macOS 14 or later")
    } else if !(cfg.microphone && cfg.system_audio) {
        Some("Needs microphone and system_audio on in [recordings]")
    } else {
        None
    }
}

/// What the submenu shows for some levels: each source's title (with its
/// level) and which preset is checked (none when the level is not a
/// preset), and whether ducking is checked.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub sources: Vec<(String, Option<usize>)>,
    pub ducking: bool,
}

/// The [`View`] of `levels`.
pub fn view(levels: &AudioMixConfig) -> View {
    let sources = [Source::Microphone, Source::SystemAudio]
        .into_iter()
        .map(|source| {
            let g = source.gain(levels);
            (format!("{}: {}", source.title(), gain_label(g)), PRESETS.iter().position(|p| *p == g))
        })
        .collect();
    View { sources, ducking: levels.ducking }
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
    write_atomically(path, doc.to_string().as_bytes()).with_context(|| format!("writing {}", path.display()))
}

/// Replace the file at `path` with `bytes` through a temp file in the same
/// directory, so a crash or a full disk leaves the old file whole. A
/// symlinked config is written where the link points (the link stays), and
/// an existing file keeps its permissions.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    if let Ok(meta) = std::fs::metadata(&target) {
        std::fs::set_permissions(tmp.path(), meta.permissions())?;
    }
    tmp.persist(&target)?;
    Ok(())
}

/// The submenu and its items.
pub struct AudioMenu {
    pub submenu: Submenu,
    sources: Vec<(Submenu, Vec<CheckMenuItem>)>,
    duck: CheckMenuItem,
    levels: AudioMixConfig,
}

impl AudioMenu {
    /// Build the submenu showing `levels`, with `hint` (see
    /// [`requirement`]) as a disabled first line.
    pub fn new(levels: AudioMixConfig, hint: Option<&str>) -> Result<Self> {
        let submenu = Submenu::with_id("audio_levels", "Audio Levels", true);
        if let Some(hint) = hint {
            submenu.append(&MenuItem::with_id(format!("{PREFIX}hint"), hint, false, None))?;
            submenu.append(&PredefinedMenuItem::separator())?;
        }
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
            sources.push((sub, items));
        }
        submenu.append(&PredefinedMenuItem::separator())?;
        let duck = CheckMenuItem::with_id(DUCK_ID, "Duck System Audio While Talking", true, false, None);
        submenu.append(&duck)?;
        let menu = Self { submenu, sources, duck, levels };
        menu.sync();
        Ok(menu)
    }

    /// The levels the menu shows.
    pub fn levels(&self) -> AudioMixConfig {
        self.levels
    }

    /// Show `self.levels` (see [`view`]).
    fn sync(&self) {
        let v = view(&self.levels);
        for ((sub, items), (title, checked)) in self.sources.iter().zip(v.sources) {
            sub.set_text(title);
            for (i, item) in items.iter().enumerate() {
                item.set_checked(checked == Some(i));
            }
        }
        self.duck.set_checked(v.ducking);
    }

    /// Act on a click if `id` is one of this submenu's items. Returns the
    /// change (for the caller to hand [`AudioMenu::levels`] to the engine
    /// and persist), or `None` for an id that is not an audio item.
    pub fn handle(&mut self, id: &str) -> Option<Change> {
        let change = parse_id(id, self.levels.ducking)?;
        self.levels = change.apply(self.levels);
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
    fn view_checks_the_preset_and_titles_the_level() {
        let v = view(&AudioMixConfig::default());
        let one = PRESETS.iter().position(|p| *p == 1.0);
        assert_eq!(v.sources, vec![("Microphone: 100%".to_string(), one), ("System Audio: 100%".to_string(), one)]);
        assert!(!v.ducking);
        // A click: the clicked preset is the only one checked in its source.
        let levels = parse_id(&preset_id(Source::SystemAudio, 0), false).unwrap().apply(AudioMixConfig::default());
        let v = view(&levels);
        assert_eq!(v.sources[0], ("Microphone: 100%".to_string(), one));
        assert_eq!(v.sources[1], ("System Audio: Muted".to_string(), Some(0)));
        // Ducking toggles from the state before the click, both ways.
        let on = parse_id(DUCK_ID, levels.ducking).unwrap().apply(levels);
        assert!(view(&on).ducking);
        assert!(!view(&parse_id(DUCK_ID, on.ducking).unwrap().apply(on)).ducking);
        // A level from the config that is not a preset: nothing checked.
        let odd = AudioMixConfig { microphone_gain: 0.6, ..Default::default() };
        assert_eq!(view(&odd).sources[0], ("Microphone: 60%".to_string(), None));
    }

    #[test]
    fn requirement_names_what_the_config_lacks() {
        let both = RecordingConfig { microphone: true, system_audio: true, ..Default::default() };
        assert_eq!(requirement(&both, true), None);
        assert!(requirement(&RecordingConfig::default(), true).unwrap().contains("microphone and system_audio"));
        let sys_only = RecordingConfig { system_audio: true, ..Default::default() };
        assert!(requirement(&sys_only, true).is_some());
        assert!(requirement(&both, false).unwrap().contains("macOS 14"));
        let screencapture = RecordingConfig { backend: RecordingBackend::SystemRecorder, ..both.clone() };
        assert!(requirement(&screencapture, true).unwrap().contains("backend"));
        let stream = RecordingConfig { backend: RecordingBackend::StreamCapture, ..both };
        assert_eq!(requirement(&stream, true), None);
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

    #[test]
    fn persist_keeps_a_symlink_and_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("dotfiles").join("config.toml");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "[recordings]\nducking = false\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = tmp.path().join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        persist(&link, Change::Ducking(true)).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(Config::load_from(&real).unwrap().recordings.mix.ducking);
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o640);
        // No temp files left behind.
        assert_eq!(std::fs::read_dir(real.parent().unwrap()).unwrap().count(), 1);
    }
}
