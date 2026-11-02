//! config.toml over the built-in defaults (Python: `DEFAULTS`, `load_config`, `config_mtime`).
//!
//! `DEFAULTS` keeps Python's key order, so `to_json` writes what `json.dumps(load_config(..))`
//! writes (`magic-mouse-config get` / `defaults`) byte for byte, although `Config` itself is a
//! BTreeMap. The file is read through `status::read_own_file` (no symlinks, no FIFOs, ours, 1 MiB).
use std::collections::BTreeMap;
use std::path::Path;

use crate::iface::{log, Config};
use crate::status::{py_float_repr, py_json, py_repr_str, read_own_file, MAX_OWN_FILE_BYTES};

/// Python `TOUCH_UNITS_PER_MM` / `SURFACE_LENGTH_MM` (for the legacy `palm_zone` key).
pub const TOUCH_UNITS_PER_MM: f64 = 38.0;
pub const SURFACE_LENGTH_MM: f64 = (2047.0 + 1589.0) / TOUCH_UNITS_PER_MM;

/// One default value; its variant is the type a user value must have (Python `type(want)`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Def {
    B(bool),
    I(i64),
    F(f64),
    S(&'static str),
}

use Def::{B, F, I, S};

/// Python `DEFAULTS`, in its order. See bin/magic-mouse-daemon for what each key does.
pub const DEFAULTS: &[(&str, &[(&str, Def)])] = &[
    (
        "pointer",
        &[
            ("tracking_speed", F(0.6875)),
            ("cpi", I(1300)),
            ("speed", F(1.0)),
            ("smoothing_ms", F(30.0)),
            ("click_hold_ms", F(80.0)),
            ("click_drag_units", F(12.0)),
        ],
    ),
    (
        "scroll",
        &[
            ("natural", B(true)),
            ("invert_y", B(false)),
            ("invert_x", B(false)),
            ("speed", F(1.0)),
            ("start_mm", F(0.8)),
            ("settle_ms", F(150.0)),
            ("settle_counts", F(4.0)),
            ("accel", F(0.6)),
            ("horizontal", B(true)),
            ("touch_start_size", F(300.0)),
            ("touch_hold_size", F(250.0)),
            ("touch_pair_size", F(250.0)),
            ("touch_rate", F(42.0)),
            ("touch_front_size", F(510.0)),
            ("touch_front_mm", F(13.0)),
            ("touch_straight", F(0.5)),
            ("touch_reverse_mm", F(0.5)),
            ("touch_firmware", B(true)),
            ("free_axis", B(false)),
            ("edge_left_mm", F(3.0)),
            ("edge_right_mm", F(3.0)),
            ("edge_front_mm", F(2.0)),
            ("edge_release_mm", F(3.0)),
            ("edge_back_mm", F(18.0)),
        ],
    ),
    ("buttons", &[("middle", S("left")), ("right_click", B(true)), ("control_click", B(false))]),
    (
        "gestures",
        &[
            ("swipe_pages", B(true)),
            ("swipe_mm", F(8.0)),
            ("invert_swipe", B(false)),
            ("swipe_end", S("wrap")),
            ("include_empty", B(true)),
            ("two_finger_swipe_left", S("dispatch hl.dsp.focus({ workspace = \"e+1\" })")),
            ("two_finger_swipe_right", S("dispatch hl.dsp.focus({ workspace = \"e-1\" })")),
            ("two_finger_double_tap", S("")),
            ("double_tap", S("")),
        ],
    ),
    (
        "touchpad",
        &[
            ("enabled", B(true)),
            ("scroll_speed", F(0.35)),
            ("swipe_gain", F(2.7)),
            ("swipe", B(true)),
            ("swipe_reverse", B(false)),
        ],
    ),
    ("bar", &[("show_percent", B(true))]),
    ("battery", &[("enabled", B(true)), ("poll_minutes", F(15.0)), ("bluez", B(true))]),
    (
        "momentum",
        &[
            ("enabled", B(true)),
            ("flick", F(900.0)),
            ("decay", F(0.97)),
            ("stop", F(60.0)),
            ("max_seconds", F(2.5)),
        ],
    ),
];

fn def_value(d: Def) -> toml::Value {
    match d {
        B(v) => toml::Value::Boolean(v),
        I(v) => toml::Value::Integer(v),
        F(v) => toml::Value::Float(v),
        S(v) => toml::Value::String(v.to_string()),
    }
}

fn want(sect: &str, key: &str) -> Option<Def> {
    DEFAULTS.iter().find(|(s, _)| *s == sect)?.1.iter().find(|(k, _)| *k == key).map(|(_, d)| *d)
}

/// The defaults alone.
pub fn defaults() -> Config {
    Config(
        DEFAULTS
            .iter()
            .map(|(s, keys)| (s.to_string(), keys.iter().map(|(k, d)| (k.to_string(), def_value(*d))).collect()))
            .collect(),
    )
}

/// Python `load_config(path)`: config.toml merged over DEFAULTS. A missing file is the defaults,
/// silently; an unreadable or broken one is the defaults with a log line. Unknown sections and
/// keys are ignored, wrong types (and non-finite numbers) keep the default, ints count for float
/// keys (stored as floats), bools never count as numbers. Legacy `scroll.edge_mm` and
/// `scroll.palm_zone` are mapped like in Python. No clamping (Python has none either).
pub fn load_config(path: &Path) -> Config {
    load_config_with(path, &mut |m| log(m))
}

pub fn load_config_with(path: &Path, log: &mut dyn FnMut(&str)) -> Config {
    let mut cfg = defaults();
    let raw = match read_own_file(path, MAX_OWN_FILE_BYTES) {
        Ok(b) => b,
        Err(e) if e.is_not_found() => return cfg,
        Err(e) => {
            log(&format!("config: {e} (using defaults)"));
            return cfg;
        }
    };
    let text = match std::str::from_utf8(&raw) {
        Ok(t) => t,
        Err(e) => {
            log(&format!("config: {} (using defaults)", utf8_error(&raw, &e)));
            return cfg;
        }
    };
    let user: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            log(&format!("config: {} (using defaults)", toml_error(text, &e)));
            return cfg;
        }
    };
    for (sect, vals) in user {
        let (Some(section), toml::Value::Table(mut vals)) = (cfg.0.get_mut(&sect), vals) else {
            log(&format!("config: unknown section [{sect}] ignored"));
            continue;
        };
        if sect == "scroll" {
            // earlier keys of this fork: one edge width for both sides, palm zone as a share
            if let Some(old) = vals.remove("edge_mm") {
                vals.entry("edge_left_mm").or_insert(old.clone());
                vals.entry("edge_right_mm").or_insert(old);
            }
            if let Some(pz) = vals.remove("palm_zone") {
                let share = match pz {
                    toml::Value::Integer(v) => Some(v as f64),
                    toml::Value::Float(v) => Some(v),
                    _ => None,
                };
                if let Some(share) = share {
                    vals.entry("edge_back_mm").or_insert(toml::Value::Float(share * SURFACE_LENGTH_MM));
                }
            }
        }
        for (k, v) in vals {
            let Some(want) = want(&sect, &k) else {
                log(&format!("config: unknown key {sect}.{k} ignored"));
                continue;
            };
            let (ok, v, tname) = match (want, v) {
                (B(_), v @ toml::Value::Boolean(_)) => (true, v, "bool"),
                (B(_), v) => (false, v, "bool"),
                (F(_), toml::Value::Integer(i)) => (true, toml::Value::Float(i as f64), "float"),
                (F(_), toml::Value::Float(f)) => (f.is_finite(), toml::Value::Float(f), "float"),
                (F(_), v) => (false, v, "float"),
                (I(_), v @ toml::Value::Integer(_)) => (true, v, "int"),
                (I(_), toml::Value::Float(f)) => (f.is_finite(), toml::Value::Float(f), "int"),
                (I(_), v) => (false, v, "int"),
                (S(_), v @ toml::Value::String(_)) => (true, v, "str"),
                (S(_), v) => (false, v, "str"),
            };
            if ok {
                section.insert(k, v);
            } else {
                // a wrong type would crash the daemon later; keep the default instead
                log(&format!("config: {sect}.{k} = {} is not a {tname}, using the default", py_repr(&v)));
            }
        }
    }
    cfg
}

/// Python `config_mtime(path)`: stat (follows symlinks) mtime in seconds, -1.0 if it fails.
pub fn config_mtime(path: &Path) -> f64 {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(m) => m.mtime() as f64 + m.mtime_nsec() as f64 / 1e9,
        Err(_) => -1.0,
    }
}

/// `json.dumps(cfg)` as Python writes it for this config (DEFAULTS order, `", "` / `": "`).
/// Python's `magic-mouse-config get` prints `to_json(&load_config(..))`, `defaults` prints
/// `to_json(&defaults())`.
pub fn to_json(cfg: &Config) -> String {
    let mut out = String::from("{");
    let empty = BTreeMap::new();
    for (i, (sect, keys)) in DEFAULTS.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&py_json(&serde_json::Value::String(sect.to_string()), false));
        out.push_str(": {");
        let have = cfg.0.get(*sect).unwrap_or(&empty);
        let mut first = true;
        for (k, d) in keys.iter() {
            let v = have.get(*k).cloned().unwrap_or_else(|| def_value(*d));
            if !first {
                out.push_str(", ");
            }
            first = false;
            out.push_str(&py_json(&serde_json::Value::String(k.to_string()), false));
            out.push_str(": ");
            out.push_str(&match v {
                toml::Value::Float(f) => crate::status::py_json_float(f),
                toml::Value::Integer(n) => n.to_string(),
                toml::Value::Boolean(b) => b.to_string(),
                toml::Value::String(s) => py_json(&serde_json::Value::String(s), false),
                other => py_json(&serde_json::Value::String(other.to_string()), false),
            });
        }
        out.push('}');
    }
    out.push('}');
    out
}

/// Python `repr()` of a parsed TOML value, for the log lines.
fn py_repr(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => py_repr_str(s),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => py_float_repr(*f),
        toml::Value::Boolean(b) => if *b { "True" } else { "False" }.to_string(),
        toml::Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        toml::Value::Table(t) => format!(
            "{{{}}}",
            t.iter().map(|(k, v)| format!("{}: {}", py_repr_str(k), py_repr(v))).collect::<Vec<_>>().join(", ")
        ),
        toml::Value::Datetime(d) => d.to_string(),
    }
}

/// Python's UnicodeDecodeError text.
fn utf8_error(raw: &[u8], e: &std::str::Utf8Error) -> String {
    let pos = e.valid_up_to();
    let b = raw[pos];
    let why = match e.error_len() {
        None => "unexpected end of data",
        Some(_) if (0xc2..=0xf4).contains(&b) => "invalid continuation byte",
        Some(_) => "invalid start byte",
    };
    format!("'utf-8' codec can't decode byte 0x{b:02x} in position {pos}: {why}")
}

/// One line like tomllib's TOMLDecodeError: `<message> (at line L, column C)`.
fn toml_error(text: &str, e: &toml::de::Error) -> String {
    let msg = e.message().trim().replace('\n', " ");
    match e.span() {
        Some(span) => {
            let before = &text[..span.start.min(text.len())];
            let line = before.matches('\n').count() + 1;
            let col = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
            format!("{msg} (at line {line}, column {col})")
        }
        None => msg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::tests::TempDir;

    fn load(text: &str) -> (Config, Vec<String>) {
        let t = TempDir::new("cfg");
        let p = t.0.join("config.toml");
        std::fs::write(&p, text).unwrap();
        let mut logged = vec![];
        let cfg = load_config_with(&p, &mut |m| logged.push(m.to_string()));
        (cfg, logged)
    }

    #[test]
    fn missing_file_is_the_defaults_quietly() {
        let t = TempDir::new("cfg");
        let mut logged = vec![];
        let cfg = load_config_with(&t.0.join("none.toml"), &mut |m| logged.push(m.to_string()));
        assert_eq!(cfg, defaults());
        assert!(logged.is_empty());
        assert_eq!(config_mtime(&t.0.join("none.toml")), -1.0);
    }

    #[test]
    fn defaults_cover_every_key_with_its_type() {
        let d = defaults();
        assert_eq!(d.f("pointer", "tracking_speed"), 0.6875);
        assert_eq!(d.0["pointer"]["cpi"], toml::Value::Integer(1300));
        assert!(d.b("scroll", "natural"));
        assert_eq!(d.s("buttons", "middle"), "left");
        assert_eq!(d.s("gestures", "two_finger_swipe_left"), "dispatch hl.dsp.focus({ workspace = \"e+1\" })");
        assert_eq!(d.0.values().map(|s| s.len()).sum::<usize>(), 6 + 24 + 3 + 9 + 5 + 1 + 3 + 5);
    }

    #[test]
    fn user_values_merge_over_defaults() {
        let (cfg, logged) = load(
            "[pointer]\ntracking_speed = 1\ncpi = 1600.5\n[scroll]\nnatural = false\n[buttons]\nmiddle = \"middle\"\n",
        );
        assert!(logged.is_empty(), "{logged:?}");
        assert_eq!(cfg.0["pointer"]["tracking_speed"], toml::Value::Float(1.0), "int for a float key becomes a float");
        assert_eq!(cfg.0["pointer"]["cpi"], toml::Value::Float(1600.5), "an int key keeps the float as given");
        assert!(!cfg.b("scroll", "natural"));
        assert_eq!(cfg.s("buttons", "middle"), "middle");
        assert_eq!(cfg.f("pointer", "speed"), 1.0);
    }

    #[test]
    fn wrong_types_keep_the_default() {
        let (cfg, logged) = load(
            "[pointer]\nspeed = true\ncpi = \"x\"\nsmoothing_ms = inf\nclick_hold_ms = nan\n[scroll]\nnatural = 1\n[buttons]\nmiddle = 3\n[gestures]\nswipe_mm = [1, 2]\n",
        );
        assert_eq!(cfg, defaults());
        let want = [
            "config: pointer.cpi = 'x' is not a int, using the default",
            "config: pointer.click_hold_ms = nan is not a float, using the default",
            "config: pointer.smoothing_ms = inf is not a float, using the default",
            "config: pointer.speed = True is not a float, using the default",
            "config: scroll.natural = 1 is not a bool, using the default",
            "config: buttons.middle = 3 is not a str, using the default",
            "config: gestures.swipe_mm = [1, 2] is not a float, using the default",
        ];
        let mut got = logged.clone();
        let mut w: Vec<String> = want.iter().map(|s| s.to_string()).collect();
        got.sort();
        w.sort();
        assert_eq!(got, w);
    }

    #[test]
    fn unknown_sections_and_keys_are_ignored() {
        let (cfg, logged) = load("top = 1\n[nope]\na = 1\n[pointer]\nwhat = 2\n");
        assert_eq!(cfg, defaults());
        let mut got = logged;
        got.sort();
        assert_eq!(
            got,
            vec![
                "config: unknown key pointer.what ignored",
                "config: unknown section [nope] ignored",
                "config: unknown section [top] ignored",
            ]
        );
    }

    #[test]
    fn legacy_scroll_keys() {
        let (cfg, logged) = load("[scroll]\nedge_mm = 5\npalm_zone = 0.25\n");
        assert!(logged.is_empty(), "{logged:?}");
        assert_eq!(cfg.f("scroll", "edge_left_mm"), 5.0);
        assert_eq!(cfg.f("scroll", "edge_right_mm"), 5.0);
        assert!((cfg.f("scroll", "edge_back_mm") - 0.25 * SURFACE_LENGTH_MM).abs() < 1e-12);
        // the new keys win; a non-number palm_zone is dropped silently
        let (cfg, logged) = load("[scroll]\nedge_mm = 5\nedge_left_mm = 1.5\npalm_zone = true\n");
        assert!(logged.is_empty(), "{logged:?}");
        assert_eq!(cfg.f("scroll", "edge_left_mm"), 1.5);
        assert_eq!(cfg.f("scroll", "edge_right_mm"), 5.0);
        assert_eq!(cfg.f("scroll", "edge_back_mm"), 18.0);
    }

    #[test]
    fn broken_files_are_the_defaults_with_a_line() {
        let (cfg, logged) = load("[pointer\n");
        assert_eq!(cfg, defaults());
        assert_eq!(logged.len(), 1);
        assert!(logged[0].starts_with("config: ") && logged[0].ends_with("(using defaults)"), "{logged:?}");
        assert!(logged[0].contains("(at line 1, column"), "{logged:?}");

        let t = TempDir::new("cfg");
        let p = t.0.join("config.toml");
        std::fs::write(&p, b"[bar]\nshow_percent = \"\xff\"\n").unwrap();
        let mut logged = vec![];
        assert_eq!(load_config_with(&p, &mut |m| logged.push(m.to_string())), defaults());
        assert_eq!(
            logged,
            vec!["config: 'utf-8' codec can't decode byte 0xff in position 22: invalid start byte (using defaults)"]
        );
    }

    #[test]
    fn symlinked_config_is_refused() {
        let t = TempDir::new("cfg");
        let real = t.0.join("real.toml");
        std::fs::write(&real, "[bar]\nshow_percent = false\n").unwrap();
        let link = t.0.join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut logged = vec![];
        assert_eq!(load_config_with(&link, &mut |m| logged.push(m.to_string())), defaults());
        assert_eq!(logged.len(), 1);
        assert!(logged[0].contains("[Errno 40]"), "{logged:?}");
        assert!(config_mtime(&link) > 0.0, "mtime follows the link like os.stat");
    }

    #[test]
    fn json_matches_python_dumps() {
        // python3 -c 'json.dumps(DEFAULTS)', first and last part
        let j = to_json(&defaults());
        assert!(j.starts_with(
            "{\"pointer\": {\"tracking_speed\": 0.6875, \"cpi\": 1300, \"speed\": 1.0, \"smoothing_ms\": 30.0, "
        ));
        assert!(j.contains("\"two_finger_swipe_left\": \"dispatch hl.dsp.focus({ workspace = \\\"e+1\\\" })\""));
        assert!(j.ends_with("\"momentum\": {\"enabled\": true, \"flick\": 900.0, \"decay\": 0.97, \"stop\": 60.0, \"max_seconds\": 2.5}}"));
    }

    #[test]
    fn json_matches_the_python_daemon() {
        // Compare with the reference itself when python3 + tomllib are around (no evdev needed:
        // DEFAULTS and load_config are cut out of the script).
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../bin/magic-mouse-daemon");
        let t = TempDir::new("cfg");
        let p = t.0.join("config.toml");
        std::fs::write(&p, "[pointer]\ntracking_speed = 2\ncpi = 1600.5\n[scroll]\nedge_mm = 4\npalm_zone = 0.3\nnatural = 0\n[x]\n").unwrap();
        let prog = r#"
import sys, re, json, math, os, stat, errno, tomllib
src = open(sys.argv[1]).read()
ns = {"os": os, "stat": stat, "errno": errno, "math": math, "tomllib": tomllib, "log": lambda m: None}
ns["SURFACE_LENGTH_MM"] = (2047 + 1589) / 38.0
for name in ("DEFAULTS = {", "MAX_OWN_FILE_BYTES =", "def read_own_file", "def load_config"):
    i = src.index("\n" + name) + 1
    j = re.compile(r"\n(?=\S)(?!\})").search(src, i + 1).start()
    exec(src[i:j], ns)
print(json.dumps(ns["DEFAULTS"]))
print(json.dumps(ns["load_config"](sys.argv[2])))
"#;
        let Ok(out) = std::process::Command::new("python3").arg("-I").arg("-c").arg(prog).arg(&script).arg(&p).output() else {
            return;
        };
        if !out.status.success() {
            panic!("python reference failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        let text = String::from_utf8(out.stdout).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next().unwrap(), to_json(&defaults()));
        let mut sink = |_: &str| {};
        assert_eq!(lines.next().unwrap(), to_json(&load_config_with(&p, &mut sink)));
    }
}
