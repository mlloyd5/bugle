//! Filename templates.
//!
//! Supported tokens: `{date}`, `{time}`, `{datetime}`, `{year}`, `{month}`,
//! `{day}`, `{hour}`, `{minute}`, `{second}`, `{id}`, plus whatever the
//! caller passes in `vars` (the engine supplies `{kind}`, `{label}`, `{app}`
//! (window owner) and `{title}` (window title) where they apply). Unknown
//! tokens are dropped. Name clashes are numbered by [`unique_path`], which
//! appends ` 2`, ` 3`, … to the stem.

use chrono::Local;
use std::collections::HashMap;

pub const DEFAULT_TEMPLATE: &str = "Bugle {date} at {time}";

pub fn render(template: &str, vars: &HashMap<&str, String>) -> String {
    let now = Local::now();
    let builtin = |k: &str| -> Option<String> {
        let fmt = match k {
            "date" => "%Y-%m-%d",
            "time" => "%H.%M.%S",
            "datetime" => "%Y-%m-%d_%H.%M.%S",
            "year" => "%Y",
            "month" => "%m",
            "day" => "%d",
            "hour" => "%H",
            "minute" => "%M",
            "second" => "%S",
            "id" => return Some(crate::media_item::short_id()),
            _ => return None,
        };
        Some(now.format(fmt).to_string())
    };
    // One pass over the template: tokens are resolved where they appear and
    // substituted values are never re-scanned, so braces inside a window
    // title or label survive. Unknown tokens and stray `{` are dropped.
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find(['{', '}']) {
            Some(j) if after.as_bytes()[j] == b'}' => {
                let name = &after[..j];
                if let Some(v) = builtin(name) {
                    out.push_str(&v);
                } else if let Some(v) = vars.get(name) {
                    out.push_str(&sanitize(v));
                }
                rest = &after[j + 1..];
            }
            _ => rest = after,
        }
    }
    out.push_str(rest);
    let tidy = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if tidy.is_empty() {
        format!("Bugle {}", now.format("%Y-%m-%d_%H.%M.%S"))
    } else {
        tidy
    }
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
    dir.join(format!("{stem} {}.{ext}", crate::media_item::short_id()))
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

    #[test]
    fn braces_in_values_survive() {
        let mut vars = HashMap::new();
        vars.insert("title", "fn main() { loop".to_string());
        vars.insert("label", "a/b {x} }".to_string());
        let s = render("{title} - {label} {year}", &vars);
        let year = Local::now().format("%Y").to_string();
        assert_eq!(s, format!("fn main() {{ loop - a-b {{x}} }} {year}"));
    }

    #[test]
    fn stray_brace_in_template_does_not_swallow_the_rest() {
        let s = render("shot { {year} end", &HashMap::new());
        let year = Local::now().format("%Y").to_string();
        assert_eq!(s, format!("shot {year} end"));
    }

    #[test]
    fn vars_are_sanitised() {
        let mut vars = HashMap::new();
        vars.insert("kind", "screenshot".to_string());
        vars.insert("app", "a:b/c".to_string());
        assert_eq!(render("{kind}-{app}", &vars), "screenshot-a-b-c");
    }

    #[test]
    fn empty_result_falls_back() {
        let mut vars = HashMap::new();
        vars.insert("title", "   ".to_string());
        for t in ["", "{title}", "{nope} {}", "{", "   "] {
            let s = render(t, &vars);
            assert!(s.starts_with("Bugle ") && s.len() > "Bugle ".len(), "{t:?} -> {s:?}");
        }
    }

    #[test]
    fn counter_is_not_a_token() {
        // Clashes are numbered by unique_path, not by a template token.
        assert_eq!(render("x {counter}", &HashMap::new()), "x");
    }
}
