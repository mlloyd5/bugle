//! Filename templates.
//!
//! Supported tokens: `{date}`, `{time}`, `{datetime}`, `{year}`, `{month}`,
//! `{day}`, `{hour}`, `{minute}`, `{second}`, `{kind}`, `{label}`, `{id}`,
//! `{counter}`, `{app}` (window owner), `{title}` (window title).

use chrono::Local;
use std::collections::HashMap;

pub const DEFAULT_TEMPLATE: &str = "Bugle {date} at {time}";

pub fn render(template: &str, vars: &HashMap<&str, String>) -> String {
    let now = Local::now();
    let mut out = template.to_string();
    let builtins: [(&str, String); 10] = [
        ("date", now.format("%Y-%m-%d").to_string()),
        ("time", now.format("%H.%M.%S").to_string()),
        ("datetime", now.format("%Y-%m-%d_%H.%M.%S").to_string()),
        ("year", now.format("%Y").to_string()),
        ("month", now.format("%m").to_string()),
        ("day", now.format("%d").to_string()),
        ("hour", now.format("%H").to_string()),
        ("minute", now.format("%M").to_string()),
        ("second", now.format("%S").to_string()),
        ("id", crate::media::short_id()),
    ];
    for (k, v) in builtins {
        out = out.replace(&format!("{{{k}}}"), &v);
    }
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), &sanitize(v));
    }
    // Drop any unresolved tokens and tidy whitespace.
    let mut cleaned = String::with_capacity(out.len());
    let mut depth = 0;
    for c in out.chars() {
        match c {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            _ if depth == 0 => cleaned.push(c),
            _ => {}
        }
    }
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Make a string safe as a filename component.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '\0' | '*' | '?' | '"' | '<' | '>' | '|') { '-' } else { c })
        .collect::<String>()
        .trim()
        .chars()
        .take(120)
        .collect()
}

/// Pick a path that does not exist yet by appending ` 2`, ` 3`, …
pub fn unique_path(dir: &std::path::Path, stem: &str, ext: &str) -> std::path::PathBuf {
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    for n in 2..10_000 {
        let p = dir.join(format!("{stem} {n}.{ext}"));
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{stem} {}.{ext}", crate::media::short_id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_and_drops_unknown() {
        let mut vars = HashMap::new();
        vars.insert("app", "Safari".to_string());
        let s = render("{app} {nope} {year}", &vars);
        assert!(s.starts_with("Safari "));
        assert!(!s.contains("nope"));
    }
}
