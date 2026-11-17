//! The status menu and the `[hotkeys]` names come from one table, [`MENU`],
//! so every menu item can be bound and the two cannot drift apart. The menu
//! is built by walking it; hotkeys are resolved against it by name.

use crate::Action;
use bugle_core::config::HotkeyConfig;
use global_hotkey::hotkey::HotKey;
use std::collections::HashMap;

/// What a menu item or hotkey does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// An engine action, run on the worker thread.
    Run(Action),
    /// Open the markup editor for the newest image (main thread, not the worker).
    AnnotateLatest,
    OpenFolder,
    ToggleApi,
    Preferences,
    Quit,
}

/// One bindable menu item.
#[derive(Debug)]
pub struct Entry {
    /// The `[hotkeys]` key, also used as the menu item id.
    pub name: &'static str,
    pub label: &'static str,
    pub command: Command,
    /// Shown in the menu (but not registered globally) when the entry has
    /// no hotkey of its own.
    pub menu_key: Option<&'static str>,
}

/// A row of the status menu.
#[derive(Debug)]
pub enum Slot {
    Item(Entry),
    Separator,
    /// The *Recent Captures* submenu, filled at run time; not bindable.
    Recent,
    /// The *Open at Login* check item (`login::LoginMenu`): present only
    /// when running from Bugle.app, with its state read from macOS; not
    /// bindable.
    OpenAtLogin,
    /// The *Audio Levels* submenu (`audio_menu::AudioMenu`); not bindable.
    AudioLevels,
}

const fn item(name: &'static str, label: &'static str, command: Command) -> Slot {
    Slot::Item(Entry { name, label, command, menu_key: None })
}

/// The status menu, top to bottom.
pub const MENU: &[Slot] = &[
    item("capture_area", "Capture Area", Command::Run(Action::CaptureArea)),
    item("capture_window", "Capture Window", Command::Run(Action::CaptureWindow)),
    item("capture_screen", "Capture Screen", Command::Run(Action::CaptureScreen)),
    item("scroll_capture", "Scrolling Capture", Command::Run(Action::ScrollCapture)),
    Slot::Separator,
    item("record", "Record Video", Command::Run(Action::RecordVideo)),
    item("record_gif", "Record GIF", Command::Run(Action::RecordGif)),
    item("record_window", "Record Window", Command::Run(Action::RecordWindow)),
    item("stop_recording", "Stop Recording", Command::Run(Action::StopRecording)),
    Slot::AudioLevels,
    Slot::Separator,
    item("annotate_latest", "Annotate Latest", Command::AnnotateLatest),
    item("ocr_latest", "OCR Latest", Command::Run(Action::OcrLatest)),
    item("pin_latest", "Pin Latest", Command::Run(Action::PinLatest)),
    item("sync_latest", "Sync Latest", Command::Run(Action::SyncLatest)),
    Slot::Recent,
    item("open_folder", "Open History Folder", Command::OpenFolder),
    Slot::Separator,
    item("toggle_api", "Start Agent API", Command::ToggleApi),
    item("preferences", "Preferences…", Command::Preferences),
    Slot::OpenAtLogin,
    Slot::Separator,
    Slot::Item(Entry { name: "quit", label: "Quit Bugle", command: Command::Quit, menu_key: Some("cmd+q") }),
];

/// Every bindable entry, in menu order.
pub fn entries() -> impl Iterator<Item = &'static Entry> {
    MENU.iter().filter_map(|s| match s {
        Slot::Item(e) => Some(e),
        _ => None,
    })
}

pub fn find(name: &str) -> Option<&'static Entry> {
    entries().find(|e| e.name == name)
}

/// `(name, binding)` pairs from `[hotkeys]`: the named fields first, then any
/// other keys. Non-string values are reported and skipped.
pub fn config_pairs(h: &HotkeyConfig) -> (Vec<(String, String)>, Vec<String>) {
    let mut pairs: Vec<(String, String)> = h.bindings().into_iter().map(|(n, b)| (n.to_string(), b)).collect();
    let mut warnings = Vec::new();
    for (name, v) in &h.extra {
        match v.as_str().map(str::trim) {
            Some("") => {}
            Some(s) => pairs.push((name.clone(), s.to_string())),
            None => warnings.push(format!("[hotkeys] {name}: expected a string like \"cmd+shift+k\", got {v}")),
        }
    }
    (pairs, warnings)
}

/// A binding that survived validation.
#[derive(Debug)]
pub struct Binding {
    pub entry: &'static Entry,
    /// The binding as written in the config.
    pub combo: String,
    pub hotkey: HotKey,
}

/// Resolve `(name, binding)` pairs against [`MENU`]. Unknown names, bindings
/// that do not parse, and a key combination already taken by an earlier item
/// are skipped with a warning; the first one wins. (A name cannot appear
/// twice: TOML rejects duplicate keys.)
pub fn plan(pairs: &[(String, String)]) -> (Vec<Binding>, Vec<String>) {
    let mut out: Vec<Binding> = Vec::new();
    let mut warnings = Vec::new();
    let mut by_combo: HashMap<u32, &'static str> = HashMap::new();
    for (name, combo) in pairs {
        let Some(entry) = find(name) else {
            let known: Vec<&str> = entries().map(|e| e.name).collect();
            warnings.push(format!("[hotkeys] unknown key `{name}` (known: {})", known.join(", ")));
            continue;
        };
        let hotkey = match combo.parse::<HotKey>() {
            Ok(h) => h,
            Err(e) => {
                warnings.push(format!("[hotkeys] {name} = {combo:?}: {e}"));
                continue;
            }
        };
        if let Some(other) = by_combo.get(&hotkey.id()) {
            warnings.push(format!("[hotkeys] {name} = {combo:?} is already bound to {other}; skipped"));
            continue;
        }
        by_combo.insert(hotkey.id(), entry.name);
        out.push(Binding { entry, combo: combo.clone(), hotkey });
    }
    (out, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bugle_core::Config;

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn table_names_are_unique_and_cover_every_action() {
        let names: Vec<&str> = entries().map(|e| e.name).collect();
        let unique: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "duplicate menu names: {names:?}");
        for a in [
            Action::CaptureArea,
            Action::CaptureWindow,
            Action::CaptureScreen,
            Action::ScrollCapture,
            Action::RecordVideo,
            Action::RecordGif,
            Action::RecordWindow,
            Action::StopRecording,
            Action::OcrLatest,
            Action::PinLatest,
            Action::SyncLatest,
        ] {
            assert!(entries().any(|e| e.command == Command::Run(a)), "{a:?} has no menu entry");
        }
        for c in [Command::OpenFolder, Command::ToggleApi, Command::Preferences, Command::Quit] {
            assert!(entries().any(|e| e.command == c), "{c:?} has no menu entry");
        }
        assert_eq!(MENU.iter().filter(|s| matches!(s, Slot::Recent)).count(), 1);
        assert_eq!(MENU.iter().filter(|s| matches!(s, Slot::AudioLevels)).count(), 1);
    }

    #[test]
    fn every_named_config_key_is_in_the_table() {
        // `bindings()` lists every named `[hotkeys]` field when all are set.
        let mut h = HotkeyConfig::default();
        for f in [&mut h.capture_window, &mut h.scroll_capture, &mut h.record_gif] {
            *f = Some("F13".into());
        }
        for (name, _) in h.bindings() {
            assert!(find(name).is_some(), "[hotkeys] {name} is not a menu entry");
        }
    }

    #[test]
    fn menu_keys_parse() {
        for e in entries() {
            if let Some(k) = e.menu_key {
                k.parse::<tray_icon::menu::accelerator::Accelerator>().unwrap_or_else(|err| panic!("{}: {k}: {err}", e.name));
            }
        }
    }

    #[test]
    fn defaults_plan_without_warnings() {
        let (pairs, w) = config_pairs(&Config::default().hotkeys);
        assert!(w.is_empty(), "{w:?}");
        let (b, w) = plan(&pairs);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(b.len(), pairs.len());
        assert!(b.iter().any(|b| b.entry.name == "capture_area" && b.combo == "cmd+shift+4"));
    }

    #[test]
    fn unknown_bad_and_duplicate_bindings_warn_and_are_skipped() {
        let (b, w) = plan(&pairs(&[
            ("capture_area", "cmd+shift+4"),
            ("capture_everything", "cmd+shift+9"), // unknown name
            ("ocr_latest", "cmd+shift+nope"),      // does not parse
            ("pin_latest", "shift+cmd+4"),         // same combo as capture_area
            ("quit", "ctrl+alt+q"),
        ]));
        let names: Vec<&str> = b.iter().map(|b| b.entry.name).collect();
        assert_eq!(names, ["capture_area", "quit"]);
        assert_eq!(b[0].combo, "cmd+shift+4");
        assert_eq!(w.len(), 3, "{w:#?}");
        assert!(w[0].contains("unknown key `capture_everything`"));
        assert!(w[1].contains("ocr_latest"));
        assert!(w[2].contains("already bound to capture_area"));
    }

    #[test]
    fn non_string_named_keys_warn_instead_of_failing_the_load() {
        let toml = "[hotkeys]\ncapture_area = 5\nrecord = [\"cmd+shift+5\"]\nocr_latest = \"ctrl+alt+o\"\n";
        let cfg: Config = toml::from_str(toml).expect("config still loads");
        let h = &cfg.hotkeys;
        assert_eq!(h.capture_area, None, "a bad value unbinds rather than keeping the default");
        assert_eq!(h.record, None);
        assert_eq!(h.ocr_latest.as_deref(), Some("ctrl+alt+o"));
        assert_eq!(h.capture_screen.as_deref(), Some("cmd+shift+3"), "missing keys keep defaults");
        let (pairs, w) = config_pairs(h);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w.iter().any(|w| w.contains("capture_area")) && w.iter().any(|w| w.contains("record")));
        assert!(!pairs.iter().any(|(n, _)| n == "capture_area" || n == "record"));
        // Saving and reloading keeps the bad value where the user put it.
        let back: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.hotkeys.extra.get("capture_area").and_then(|v| v.as_integer()), Some(5));
        assert_eq!(back.hotkeys.capture_area, None);
    }

    #[test]
    fn extra_keys_come_from_toml_and_non_strings_warn() {
        let toml = "[hotkeys]\ncapture_area = \"\"\nstop_recording = \"cmd+shift+.\"\nquit = 5\nsync_latest = \"\"\nbogus = \"F14\"\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        let (pairs, w) = config_pairs(&cfg.hotkeys);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("quit"));
        assert!(!pairs.iter().any(|(n, _)| n == "capture_area" || n == "sync_latest"), "empty strings unbind");
        assert!(pairs.iter().any(|(n, b)| n == "stop_recording" && b == "cmd+shift+."));
        let (b, w) = plan(&pairs);
        assert!(b.iter().any(|b| b.entry.command == Command::Run(Action::StopRecording)));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("bogus"));
    }

    #[test]
    fn example_config_hotkeys_are_valid_even_when_uncommented() {
        let text = include_str!("../../../config.example.toml");
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(cfg.screenshots.window_picker);
        // Uncomment every `# name = "combo"` line in [hotkeys].
        let mut in_hotkeys = false;
        let mut lines = vec!["[hotkeys]".to_string()];
        for l in text.lines() {
            if l.starts_with('[') {
                in_hotkeys = l.trim() == "[hotkeys]";
                continue;
            }
            let l = l.trim_start_matches("# ");
            if in_hotkeys && l.contains(" = \"") {
                lines.push(l.split(" #").next().unwrap_or(l).to_string());
            }
        }
        let cfg: Config = toml::from_str(&lines.join("\n")).unwrap();
        let (pairs, w) = config_pairs(&cfg.hotkeys);
        assert!(w.is_empty(), "{w:?}");
        let (b, w) = plan(&pairs);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(b.len(), entries().count(), "the example should show a binding for every menu item");
    }

    #[test]
    fn extra_keys_survive_a_save() {
        let mut cfg = Config::default();
        cfg.hotkeys.extra.insert("toggle_api".into(), toml::Value::String("ctrl+alt+a".into()));
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.hotkeys.extra.get("toggle_api").and_then(|v| v.as_str()), Some("ctrl+alt+a"));
    }
}
