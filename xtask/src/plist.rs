//! `Contents/Info.plist` for Bugle.app, written as XML by hand (strings and
//! booleans are all it needs).

/// What varies between builds.
pub struct BundleInfo<'a> {
    pub bundle_id: &'a str,
    pub version: &'a str,
    pub build_number: &'a str,
    pub executable: &'a str,
    pub icon_file: &'a str,
    pub min_macos: &'a str,
}

enum Value<'a> {
    Str(&'a str),
    Bool(bool),
}

/// Usage strings for the privacy prompts the app (or the embedded CLI) can
/// trigger. Screen Recording's system prompt currently shows fixed text; the
/// key is set so the string is there wherever macOS does show it.
pub const USAGE_DESCRIPTIONS: &[(&str, &str)] = &[
    (
        "NSScreenCaptureUsageDescription",
        "Bugle captures the screen for screenshots, scrolling captures and recordings.",
    ),
    ("NSMicrophoneUsageDescription", "Bugle records your microphone when you include it in a screen recording."),
];

pub fn info_plist(info: &BundleInfo) -> String {
    let mut entries: Vec<(&str, Value)> = vec![
        ("CFBundleDevelopmentRegion", Value::Str("en")),
        ("CFBundleDisplayName", Value::Str("Bugle")),
        ("CFBundleExecutable", Value::Str(info.executable)),
        ("CFBundleIconFile", Value::Str(info.icon_file)),
        ("CFBundleIdentifier", Value::Str(info.bundle_id)),
        ("CFBundleInfoDictionaryVersion", Value::Str("6.0")),
        ("CFBundleName", Value::Str("Bugle")),
        ("CFBundlePackageType", Value::Str("APPL")),
        ("CFBundleShortVersionString", Value::Str(info.version)),
        ("CFBundleVersion", Value::Str(info.build_number)),
        ("LSApplicationCategoryType", Value::Str("public.app-category.productivity")),
        ("LSMinimumSystemVersion", Value::Str(info.min_macos)),
        ("LSMultipleInstancesProhibited", Value::Bool(true)),
        // Menu-bar app: no Dock icon, no main menu.
        ("LSUIElement", Value::Bool(true)),
        ("NSHighResolutionCapable", Value::Bool(true)),
        ("NSHumanReadableCopyright", Value::Str("MIT License")),
        ("NSPrincipalClass", Value::Str("NSApplication")),
        ("NSSupportsAutomaticGraphicsSwitching", Value::Bool(true)),
    ];
    entries.extend(USAGE_DESCRIPTIONS.iter().map(|(k, v)| (*k, Value::Str(v))));
    entries.sort_by_key(|(k, _)| *k);

    let mut out = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<plist version=\"1.0\">\n<dict>\n",
    ));
    for (key, value) in entries {
        out.push_str(&format!("\t<key>{}</key>\n", escape(key)));
        match value {
            Value::Str(s) => out.push_str(&format!("\t<string>{}</string>\n", escape(s))),
            Value::Bool(b) => out.push_str(if b { "\t<true/>\n" } else { "\t<false/>\n" }),
        }
    }
    out.push_str("</dict>\n</plist>\n");
    out
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        info_plist(&BundleInfo {
            bundle_id: "com.example.bugle",
            version: "1.2.3",
            build_number: "45",
            executable: "bugle-app",
            icon_file: "Bugle",
            min_macos: "13.0",
        })
    }

    #[test]
    fn has_bundle_keys() {
        let p = sample();
        assert!(p.contains("<key>CFBundleIdentifier</key>\n\t<string>com.example.bugle</string>"));
        assert!(p.contains("<key>CFBundleShortVersionString</key>\n\t<string>1.2.3</string>"));
        assert!(p.contains("<key>CFBundleVersion</key>\n\t<string>45</string>"));
        assert!(p.contains("<key>CFBundleExecutable</key>\n\t<string>bugle-app</string>"));
        assert!(p.contains("<key>LSUIElement</key>\n\t<true/>"));
        assert!(p.contains("<key>NSMicrophoneUsageDescription</key>"));
        assert!(p.contains("<key>NSScreenCaptureUsageDescription</key>"));
        assert!(p.ends_with("</dict>\n</plist>\n"));
    }

    #[test]
    fn keys_are_sorted_and_unique() {
        let p = sample();
        let keys: Vec<&str> =
            p.lines().filter_map(|l| l.trim().strip_prefix("<key>")?.strip_suffix("</key>")).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn escapes_markup() {
        assert_eq!(escape("a<b>&c"), "a&lt;b&gt;&amp;c");
    }
}
