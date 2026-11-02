//! `magic-mouse-daemon config get|defaults|set ...`: the Python `bin/magic-mouse-config`, same CLI and
//! output, so Panel.qml and the preview keep calling `magic-mouse-config` (now a wrapper around this).
//! Owner: Bob.
use std::path::{Path, PathBuf};

use crate::config::{defaults, load_config_with, to_json};
use crate::status::{py_json, read_own_file, write_own_file, MAX_OWN_FILE_BYTES};

const USAGE: &str = "magic-mouse-config: read or change ~/.config/magic-mouse/config.toml, keeping comments.
  magic-mouse-config get                       # effective config as JSON (defaults merged)
  magic-mouse-config defaults                  # built-in defaults as JSON
  magic-mouse-config set pointer.tracking_speed=1.0 scroll.natural=true";

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

fn config_path() -> PathBuf {
    let raw = std::env::var("MAGIC_MOUSE_CONFIG").unwrap_or_else(|_| "~/.config/magic-mouse/config.toml".into());
    match raw.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(raw),
    }
}

/// Python parse_value: true/false, an integer when it is all digits, else float(), else the stripped string.
pub fn parse_value(raw: &str) -> Val {
    let t = raw.trim();
    let low = t.to_lowercase();
    if low == "true" || low == "false" {
        return Val::Bool(low == "true");
    }
    let digits = t.strip_prefix('-').unwrap_or(t);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(v) = t.parse::<i64>() {
            return Val::Int(v);
        }
    }
    if let Ok(v) = t.parse::<f64>() {
        if v.is_finite() {
            return Val::Float(v); // nan/inf would write invalid TOML and reset the whole config
        }
    }
    Val::Str(t.trim_matches('"').to_string())
}

/// Python fmt(): floats with 4 decimals, trailing zeros cut, always a dot.
pub fn fmt(v: &Val) -> String {
    match v {
        Val::Bool(b) => if *b { "true" } else { "false" }.into(),
        Val::Int(i) => i.to_string(),
        Val::Float(f) => {
            let s = format!("{f:.4}");
            let s = s.trim_end_matches('0').trim_end_matches('.');
            if s.contains('.') { s.to_string() } else { format!("{s}.0") }
        }
        Val::Str(s) => py_json(&serde_json::Value::String(s.clone()), false),
    }
}

/// `^(\s*KEY\s*=\s*)([^#]*?)(\s*#.*)?$` → (prefix, comment) when the line sets KEY.
fn match_key<'a>(line: &'a str, key: &str) -> Option<(&'a str, &'a str)> {
    let rest = line.trim_start();
    let after_key = rest.strip_prefix(key)?;
    let after_ws = after_key.trim_start();
    let after_eq = after_ws.strip_prefix('=')?;
    let value_start = line.len() - after_eq.trim_start().len();
    let prefix = &line[..value_start];
    let tail = &line[value_start..];
    let comment = match tail.find('#') {
        Some(h) => {
            let start = tail[..h].trim_end().len();
            &tail[start..]
        }
        None => "",
    };
    Some((prefix, comment))
}

pub fn set_text(text: &str, pairs: &[(String, String, Val)]) -> String {
    let mut lines: Vec<String> = if text.is_empty() { vec![] } else { text.split('\n').map(String::from).collect() };
    for (section, key, value) in pairs {
        let header = format!("[{section}]");
        let Some(start) = lines.iter().position(|l| l.trim() == header) else {
            if lines.last().is_some_and(|l| !l.is_empty()) {
                lines.push(String::new());
            }
            lines.push(header);
            lines.push(format!("{key} = {}", fmt(value)));
            continue;
        };
        let end = (start + 1..lines.len()).find(|&i| lines[i].trim().starts_with('[')).unwrap_or(lines.len());
        let hit = (start + 1..end).find_map(|i| match_key(&lines[i], key).map(|(p, c)| (i, format!("{p}{}{c}", fmt(value)))));
        match hit {
            Some((i, line)) => lines[i] = line,
            None => lines.insert(start + 1, format!("{key} = {}", fmt(value))),
        }
    }
    lines.join("\n")
}

fn set_values(path: &Path, pairs: &[(String, String, Val)]) -> Result<(), String> {
    let text = match read_own_file(path, MAX_OWN_FILE_BYTES) {
        Ok(b) => String::from_utf8(b).map_err(|e| e.to_string())?,
        Err(e) if e.is_not_found() => String::new(),
        Err(e) => return Err(e.to_string()),
    };
    write_own_file(path, set_text(&text, pairs).as_bytes(), 0o600, false).map_err(|e| e.to_string())
}

/// argv without the program name and without "config". Returns the exit code.
pub fn main(argv: &[String]) -> i32 {
    let path = config_path();
    let mut log = |m: &str| eprintln!("{m}"); // keep stdout clean for JSON
    match argv {
        [cmd, ..] if cmd == "get" => {
            println!("{}", to_json(&load_config_with(&path, &mut log)));
            0
        }
        [cmd] if cmd == "defaults" => {
            println!("{}", to_json(&defaults()));
            0
        }
        [cmd, items @ ..] if cmd == "set" && !items.is_empty() => {
            let mut pairs = vec![];
            for item in items {
                let Some((p, raw)) = item.split_once('=').filter(|(p, _)| p.contains('.')) else {
                    eprintln!("bad assignment: {item} (want section.key=value)");
                    return 2;
                };
                let (section, key) = p.split_once('.').unwrap();
                pairs.push((section.to_string(), key.to_string(), parse_value(raw)));
            }
            match set_values(&path, &pairs) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("config: {e}");
                    1
                }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str, k: &str, v: &str) -> (String, String, Val) {
        (s.into(), k.into(), parse_value(v))
    }

    #[test]
    fn values() {
        assert_eq!(parse_value(" 3 "), Val::Int(3));
        assert_eq!(parse_value("-2"), Val::Int(-2));
        assert_eq!(parse_value("TRUE"), Val::Bool(true));
        assert_eq!(parse_value("0.35"), Val::Float(0.35));
        assert_eq!(parse_value("\"wrap\""), Val::Str("wrap".into()));
        assert_eq!(fmt(&Val::Float(0.35)), "0.35");
        assert_eq!(fmt(&Val::Float(2.0)), "2.0");
        assert_eq!(fmt(&Val::Float(0.123456)), "0.1235");
        assert_eq!(fmt(&Val::Str("a\"ü".into())), "\"a\\\"\\u00fc\"");
    }

    #[test]
    fn edits_keep_comments_and_order() {
        let text = "# mine\n[scroll]\nspeed = 0.3   # slow\nnatural = true\n\n[pointer]\ntracking_speed = 1.0\n";
        let out = set_text(text, &[p("scroll", "speed", "0.25"), p("pointer", "accel", "false"), p("gestures", "swipe_end", "stop")]);
        assert_eq!(
            out,
            "# mine\n[scroll]\nspeed = 0.25   # slow\nnatural = true\n\n[pointer]\naccel = false\ntracking_speed = 1.0\n\n[gestures]\nswipe_end = \"stop\""
        );
        assert_eq!(set_text("", &[p("a", "b", "1")]), "[a]\nb = 1");
        // a key that only starts like ours is not ours
        assert_eq!(set_text("[s]\nspeedy = 1", &[p("s", "speed", "2")]), "[s]\nspeed = 2\nspeedy = 1");
    }
}
