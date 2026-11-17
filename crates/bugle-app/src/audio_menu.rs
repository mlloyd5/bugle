//! The *Audio Levels* submenu: microphone and system-audio levels and
//! ducking for the next recordings, without editing the config file by hand.
//!
//! Each level is a preset or a custom value typed into a prompt (*Custom…*),
//! and so is the ducking amount; ducking itself is a toggle.
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
use bugle_core::audio_mix::{AudioMixConfig, MAX_DUCK_DB, MAX_GAIN};
use bugle_core::config::{RecordingBackend, RecordingConfig};
use std::io::Write;
use std::path::Path;
use tray_icon::menu::{CheckMenuItem, MenuItem, PredefinedMenuItem, Submenu};

/// Menu item ids start with this.
const PREFIX: &str = "audio:";
const DUCK_ID: &str = "audio:duck";
/// Tag of the ducking-amount items (`audio:amount:<i>`, `audio:amount:custom`).
const AMOUNT_TAG: &str = "amount";
/// Last id segment of a *Custom…* item.
const CUSTOM: &str = "custom";

/// Level presets offered for each source: linear gains (1.0 = as recorded,
/// or the default share of a mix), shown as percentages.
pub const PRESETS: &[f32] = &[0.0, 0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 4.0];

/// Ducking amounts offered, in dB (12 is the config default).
pub const AMOUNT_PRESETS: &[f32] = &[6.0, 12.0, 18.0, 24.0, 30.0, 40.0, 60.0];

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
    /// How far system audio is lowered while ducking, in dB.
    DuckingAmount(f32),
}

impl Change {
    /// `levels` with this change applied.
    pub fn apply(self, mut levels: AudioMixConfig) -> AudioMixConfig {
        match self {
            Change::Gain(Source::Microphone, g) => levels.microphone_gain = g,
            Change::Gain(Source::SystemAudio, g) => levels.system_audio_gain = g,
            Change::Ducking(d) => levels.ducking = d,
            Change::DuckingAmount(db) => levels.ducking_amount_db = db,
        }
        levels
    }
}

/// A value the submenu can ask for in a prompt (*Custom…*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Gain(Source),
    DuckingAmount,
}

/// What a prompt for a [`Field`] shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub title: String,
    pub message: String,
    /// The current value, as it would be typed.
    pub initial: String,
}

impl Field {
    /// The prompt for this field, starting from `levels`.
    pub fn prompt(self, levels: &AudioMixConfig) -> Prompt {
        match self {
            Field::Gain(source) => Prompt {
                title: format!("{} Level", source.title()),
                message: format!(
                    "Percent of the recorded level, 0 (muted) to {:.0}. Applies to the next recordings.",
                    MAX_GAIN * 100.0
                ),
                initial: number_text(source.gain(levels) * 100.0),
            },
            Field::DuckingAmount => Prompt {
                title: "Ducking Amount".into(),
                message: format!(
                    "How far system audio is lowered while the microphone is active, in dB, 0 to {MAX_DUCK_DB:.0}."
                ),
                initial: number_text(levels.ducking_amount_db),
            },
        }
    }

    /// The change typed `text` asks for, or why it is not a valid value.
    /// A level is a percentage (`60`, `60%`, `muted`), kept to 0.1%; an
    /// amount is in dB (`18`, `18 dB`), kept to 0.1 dB.
    pub fn parse(self, text: &str) -> Result<Change, String> {
        let t = text.trim();
        match self {
            Field::Gain(source) => {
                let percent = if t.eq_ignore_ascii_case("muted") || t.eq_ignore_ascii_case("mute") {
                    0.0
                } else {
                    parse_number(t.strip_suffix('%').unwrap_or(t), 0.0, MAX_GAIN * 100.0, "%")?
                };
                Ok(Change::Gain(source, round_tenth(percent) / 100.0))
            }
            Field::DuckingAmount => {
                let lower = t.to_ascii_lowercase();
                let n = lower.strip_suffix("db").unwrap_or(&lower);
                Ok(Change::DuckingAmount(round_tenth(parse_number(n, 0.0, MAX_DUCK_DB, " dB")?)))
            }
        }
    }
}

/// `text` as a number within `lo..=hi`, or a message naming the range.
fn parse_number(text: &str, lo: f32, hi: f32, unit: &str) -> Result<f32, String> {
    let range = || format!("{}{unit} to {}{unit}", number_text(lo), number_text(hi));
    match text.trim().parse::<f32>() {
        Ok(v) if v.is_finite() && (lo..=hi).contains(&v) => Ok(v),
        Ok(_) => Err(format!("{} is out of range ({})", text.trim(), range())),
        Err(_) => Err(format!("\"{}\" is not a number ({})", text.trim(), range())),
    }
}

/// `v` rounded to one decimal place.
fn round_tenth(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

/// `v` with at most one decimal and no trailing `.0`.
fn number_text(v: f32) -> String {
    let t = format!("{:.1}", round_tenth(v));
    t.strip_suffix(".0").map(str::to_string).unwrap_or(t)
}

/// What a click asks for: a change, or a prompt for a custom value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Click {
    Set(Change),
    Custom(Field),
}

fn preset_id(source: Source, i: usize) -> String {
    format!("{PREFIX}{}:{i}", source.tag())
}

fn custom_id(source: Source) -> String {
    format!("{PREFIX}{}:{CUSTOM}", source.tag())
}

fn amount_id(i: usize) -> String {
    format!("{PREFIX}{AMOUNT_TAG}:{i}")
}

fn amount_custom_id() -> String {
    format!("{PREFIX}{AMOUNT_TAG}:{CUSTOM}")
}

/// What a menu item id stands for, or `None` for an id that is not one of
/// this submenu's. `ducking` is the state before the click.
pub fn parse_id(id: &str, ducking: bool) -> Option<Click> {
    if id == DUCK_ID {
        return Some(Click::Set(Change::Ducking(!ducking)));
    }
    let rest = id.strip_prefix(PREFIX)?;
    let (tag, i) = rest.split_once(':')?;
    if tag == AMOUNT_TAG {
        if i == CUSTOM {
            return Some(Click::Custom(Field::DuckingAmount));
        }
        let db = *AMOUNT_PRESETS.get(i.parse::<usize>().ok()?)?;
        return Some(Click::Set(Change::DuckingAmount(db)));
    }
    let source = [Source::Microphone, Source::SystemAudio].into_iter().find(|s| s.tag() == tag)?;
    if i == CUSTOM {
        return Some(Click::Custom(Field::Gain(source)));
    }
    let gain = *PRESETS.get(i.parse::<usize>().ok()?)?;
    Some(Click::Set(Change::Gain(source, gain)))
}

/// "Muted", or the gain as a percentage.
pub fn gain_label(g: f32) -> String {
    if g == 0.0 {
        "Muted".into()
    } else {
        format!("{}%", number_text(g * 100.0))
    }
}

/// A ducking amount, such as "12 dB".
pub fn amount_label(db: f32) -> String {
    format!("{} dB", number_text(db))
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

/// What one choice submenu shows: its title (with the value), which preset
/// is checked (`None`: the value is custom, so *Custom…* is checked), and
/// the *Custom…* item's text.
#[derive(Debug, Clone, PartialEq)]
pub struct Choices {
    pub title: String,
    pub checked: Option<usize>,
    pub custom: String,
}

impl Choices {
    fn new(title: &str, label: String, checked: Option<usize>) -> Self {
        let custom = if checked.is_some() { "Custom…".to_string() } else { format!("Custom ({label})…") };
        Self { title: format!("{title}: {label}"), checked, custom }
    }
}

/// What the submenu shows for some levels: each source's choices, whether
/// ducking is checked, and the ducking-amount choices.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub sources: Vec<Choices>,
    pub ducking: bool,
    pub amount: Choices,
}

/// The [`View`] of `levels`.
pub fn view(levels: &AudioMixConfig) -> View {
    let sources = [Source::Microphone, Source::SystemAudio]
        .into_iter()
        .map(|source| {
            let g = source.gain(levels);
            Choices::new(source.title(), gain_label(g), PRESETS.iter().position(|p| *p == g))
        })
        .collect();
    let db = levels.ducking_amount_db;
    let amount = Choices::new("Ducking Amount", amount_label(db), AMOUNT_PRESETS.iter().position(|p| *p == db));
    View { sources, ducking: levels.ducking, amount }
}

/// A float as TOML writes it, from its shortest `f32` form: 0.6 is saved
/// as `0.6`, not as the `f64` widening `0.6000000238418579`.
fn toml_float(v: f32) -> toml_edit::Value {
    toml_edit::Value::from(v.to_string().parse::<f64>().unwrap_or(v as f64))
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
        Change::Gain(source, g) => (source.config_key(), toml_float(g)),
        Change::Ducking(d) => ("ducking", toml_edit::Value::from(d)),
        Change::DuckingAmount(db) => ("ducking_amount_db", toml_float(db)),
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

/// What handling a click came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The levels changed (the caller hands [`AudioMenu::levels`] to the
    /// engine and persists the change).
    Changed(Change),
    /// A custom-value prompt was cancelled; nothing changed.
    Cancelled,
    /// The typed value was refused; nothing changed.
    Invalid(String),
}

/// One choice submenu and its items: the presets, then *Custom…*.
struct ChoiceMenu {
    submenu: Submenu,
    presets: Vec<CheckMenuItem>,
    custom: CheckMenuItem,
}

impl ChoiceMenu {
    fn new(id: String, title: &str, items: impl Iterator<Item = (String, String)>, custom_id: String) -> Result<Self> {
        let submenu = Submenu::with_id(id, title, true);
        let presets: Vec<CheckMenuItem> = items.map(|(id, label)| CheckMenuItem::with_id(id, label, true, false, None)).collect();
        for item in &presets {
            submenu.append(item)?;
        }
        submenu.append(&PredefinedMenuItem::separator())?;
        let custom = CheckMenuItem::with_id(custom_id, "Custom…", true, false, None);
        submenu.append(&custom)?;
        Ok(Self { submenu, presets, custom })
    }

    fn show(&self, c: &Choices) {
        self.submenu.set_text(&c.title);
        for (i, item) in self.presets.iter().enumerate() {
            item.set_checked(c.checked == Some(i));
        }
        self.custom.set_text(&c.custom);
        self.custom.set_checked(c.checked.is_none());
    }
}

/// The submenu and its items.
pub struct AudioMenu {
    pub submenu: Submenu,
    sources: Vec<ChoiceMenu>,
    duck: CheckMenuItem,
    amount: ChoiceMenu,
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
            let items = PRESETS.iter().enumerate().map(|(i, g)| (preset_id(source, i), gain_label(*g)));
            let sub = ChoiceMenu::new(format!("{PREFIX}{}", source.tag()), source.title(), items, custom_id(source))?;
            submenu.append(&sub.submenu)?;
            sources.push(sub);
        }
        submenu.append(&PredefinedMenuItem::separator())?;
        let duck = CheckMenuItem::with_id(DUCK_ID, "Duck System Audio While Talking", true, false, None);
        submenu.append(&duck)?;
        let items = AMOUNT_PRESETS.iter().enumerate().map(|(i, db)| (amount_id(i), amount_label(*db)));
        let amount = ChoiceMenu::new(format!("{PREFIX}{AMOUNT_TAG}"), "Ducking Amount", items, amount_custom_id())?;
        submenu.append(&amount.submenu)?;
        let menu = Self { submenu, sources, duck, amount, levels };
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
        for (menu, choices) in self.sources.iter().zip(&v.sources) {
            menu.show(choices);
        }
        self.duck.set_checked(v.ducking);
        self.amount.show(&v.amount);
    }

    /// Act on a click if `id` is one of this submenu's items, or return
    /// `None` for an id that is not an audio item. A *Custom…* item asks
    /// for the value through `ask` (given the [`Prompt`]; `None` = cancel).
    pub fn handle(&mut self, id: &str, ask: impl FnOnce(&Prompt) -> Option<String>) -> Option<Outcome> {
        let outcome = match parse_id(id, self.levels.ducking)? {
            Click::Set(change) => Outcome::Changed(change),
            Click::Custom(field) => match ask(&field.prompt(&self.levels)) {
                None => Outcome::Cancelled,
                Some(text) => match field.parse(&text) {
                    Ok(change) => Outcome::Changed(change),
                    Err(e) => Outcome::Invalid(e),
                },
            },
        };
        if let Outcome::Changed(change) = outcome {
            self.levels = change.apply(self.levels);
        }
        // The menu toggles a check item on click; show what is set.
        self.sync();
        Some(outcome)
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
                assert_eq!(parse_id(&preset_id(source, i), false), Some(Click::Set(Change::Gain(source, *g))));
                let levels = Change::Gain(source, *g).apply(AudioMixConfig::default());
                levels.validate().unwrap();
            }
            assert_eq!(parse_id(&custom_id(source), false), Some(Click::Custom(Field::Gain(source))));
        }
        for (i, db) in AMOUNT_PRESETS.iter().enumerate() {
            assert_eq!(parse_id(&amount_id(i), false), Some(Click::Set(Change::DuckingAmount(*db))));
            Change::DuckingAmount(*db).apply(AudioMixConfig::default()).validate().unwrap();
        }
        assert_eq!(parse_id(&amount_custom_id(), true), Some(Click::Custom(Field::DuckingAmount)));
        assert!(PRESETS.contains(&1.0), "the default level is a preset");
        assert!(AMOUNT_PRESETS.contains(&AudioMixConfig::default().ducking_amount_db), "the default amount is a preset");
        assert_eq!(parse_id(DUCK_ID, false), Some(Click::Set(Change::Ducking(true))));
        assert_eq!(parse_id(DUCK_ID, true), Some(Click::Set(Change::Ducking(false))));
        for other in [
            "audio:mic:99",
            "audio:mic",
            "audio:xyz:1",
            "audio:mic:x",
            "audio:amount:99",
            "audio:amount",
            "audio:xyz:custom",
            "record",
            "recent:/tmp/a",
            "audio_levels",
        ] {
            assert_eq!(parse_id(other, false), None, "{other}");
        }
        assert_eq!(gain_label(0.0), "Muted");
        assert_eq!(gain_label(0.25), "25%");
        assert_eq!(gain_label(1.5), "150%");
        assert_eq!(gain_label(0.335), "33.5%");
        assert_eq!(amount_label(12.0), "12 dB");
        assert_eq!(amount_label(7.5), "7.5 dB");
    }

    #[test]
    fn custom_values_parse_round_and_refuse_out_of_range() {
        let mic = Field::Gain(Source::Microphone);
        assert_eq!(mic.parse("60"), Ok(Change::Gain(Source::Microphone, 0.6)));
        assert_eq!(mic.parse(" 60 % "), Ok(Change::Gain(Source::Microphone, 0.6)));
        let Ok(Change::Gain(_, third)) = mic.parse("33.333") else { panic!() };
        assert!((third - 0.333).abs() < 1e-6, "{third}");
        assert_eq!(mic.parse("Muted"), Ok(Change::Gain(Source::Microphone, 0.0)));
        assert_eq!(mic.parse("800"), Ok(Change::Gain(Source::Microphone, 8.0)));
        for bad in ["801", "-1", "abc", "", "NaN", "inf"] {
            let e = mic.parse(bad).unwrap_err();
            assert!(e.contains("0% to 800%"), "{bad}: {e}");
        }
        let amt = Field::DuckingAmount;
        assert_eq!(amt.parse("18"), Ok(Change::DuckingAmount(18.0)));
        assert_eq!(amt.parse("7.5 dB"), Ok(Change::DuckingAmount(7.5)));
        assert_eq!(amt.parse("20db"), Ok(Change::DuckingAmount(20.0)));
        assert_eq!(amt.parse("0"), Ok(Change::DuckingAmount(0.0)));
        for bad in ["61", "-3", "loud"] {
            assert!(amt.parse(bad).unwrap_err().contains("0 dB to 60 dB"), "{bad}");
        }
        // Every accepted value is one the config accepts.
        for text in ["0", "0.04", "799.96", "800"] {
            let Ok(change) = mic.parse(text) else { panic!("{text}") };
            change.apply(AudioMixConfig::default()).validate().unwrap();
        }
        // The prompt starts from the current value, as typed.
        let levels = AudioMixConfig { system_audio_gain: 0.6, ducking_amount_db: 7.5, ..Default::default() };
        assert_eq!(Field::Gain(Source::SystemAudio).prompt(&levels).initial, "60");
        assert_eq!(Field::Gain(Source::Microphone).prompt(&levels).initial, "100");
        let p = Field::DuckingAmount.prompt(&levels);
        assert_eq!((p.title.as_str(), p.initial.as_str()), ("Ducking Amount", "7.5"));
    }

    #[test]
    fn view_checks_the_preset_and_titles_the_level() {
        let v = view(&AudioMixConfig::default());
        let one = PRESETS.iter().position(|p| *p == 1.0);
        let preset = |title: &str, checked| Choices { title: title.to_string(), checked, custom: "Custom…".to_string() };
        assert_eq!(v.sources, vec![preset("Microphone: 100%", one), preset("System Audio: 100%", one)]);
        assert!(!v.ducking);
        assert_eq!(v.amount, preset("Ducking Amount: 12 dB", Some(1)));
        // A click: the clicked preset is the only one checked in its source.
        let Some(Click::Set(change)) = parse_id(&preset_id(Source::SystemAudio, 0), false) else { panic!() };
        let levels = change.apply(AudioMixConfig::default());
        let v = view(&levels);
        assert_eq!(v.sources[0], preset("Microphone: 100%", one));
        assert_eq!(v.sources[1], preset("System Audio: Muted", Some(0)));
        // Ducking toggles from the state before the click, both ways.
        let toggle = |l: AudioMixConfig| match parse_id(DUCK_ID, l.ducking) {
            Some(Click::Set(c)) => c.apply(l),
            other => panic!("{other:?}"),
        };
        let on = toggle(levels);
        assert!(view(&on).ducking);
        assert!(!view(&toggle(on)).ducking);
        // A level or amount that is not a preset: Custom is checked and
        // names the value.
        let odd = AudioMixConfig { microphone_gain: 0.6, ducking_amount_db: 20.0, ..Default::default() };
        let v = view(&odd);
        assert_eq!(
            v.sources[0],
            Choices { title: "Microphone: 60%".into(), checked: None, custom: "Custom (60%)…".into() }
        );
        assert_eq!(v.amount, Choices { title: "Ducking Amount: 20 dB".into(), checked: None, custom: "Custom (20 dB)…".into() });
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
        // Custom values are written in their short form and read back as set.
        persist(&path, Change::Gain(Source::Microphone, 0.6)).unwrap();
        persist(&path, Change::DuckingAmount(7.5)).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("microphone_gain = 0.6  # mic level\n"), "{out}");
        assert!(out.contains("ducking_amount_db = 7.5\n"), "{out}");
        let mix = Config::load_from(&path).unwrap().recordings.mix;
        assert_eq!((mix.microphone_gain, mix.ducking_amount_db), (0.6, 7.5));
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
