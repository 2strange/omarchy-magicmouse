//! The seams between the modules. Owner: Bob. Change only after asking Bob (PORT.md).
use std::collections::BTreeMap;

/// One evdev event as the Engine sees it (Python: `ev.timestamp(), ev.type, ev.code, ev.value`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    pub ts: f64,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

/// config.toml merged over DEFAULTS. Python reads it as `cfg["section"]["key"]` with float()/bool()/str().
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config(pub BTreeMap<String, BTreeMap<String, toml::Value>>);

impl Config {
    fn get(&self, sec: &str, key: &str) -> &toml::Value {
        self.0
            .get(sec)
            .and_then(|s| s.get(key))
            .unwrap_or_else(|| panic!("config key {sec}.{key} missing (DEFAULTS must have it)"))
    }
    /// Python float(): ints and floats (and bools, which Python treats as 0/1).
    pub fn f(&self, sec: &str, key: &str) -> f64 {
        match self.get(sec, key) {
            toml::Value::Float(v) => *v,
            toml::Value::Integer(v) => *v as f64,
            toml::Value::Boolean(v) => f64::from(u8::from(*v)),
            v => panic!("config {sec}.{key} is not a number: {v}"),
        }
    }
    /// Python bool(): numbers are true when non-zero, strings when non-empty.
    pub fn b(&self, sec: &str, key: &str) -> bool {
        match self.get(sec, key) {
            toml::Value::Boolean(v) => *v,
            toml::Value::Integer(v) => *v != 0,
            toml::Value::Float(v) => *v != 0.0,
            toml::Value::String(v) => !v.is_empty(),
            _ => true,
        }
    }
    pub fn s(&self, sec: &str, key: &str) -> String {
        match self.get(sec, key) {
            toml::Value::String(v) => v.clone(),
            v => v.to_string(),
        }
    }
}

/// The virtual mouse (Python: `ui.write(type, code, value)`, `ui.syn()`).
pub trait Output {
    fn write(&mut self, kind: u16, code: u16, value: i32);
    fn syn(&mut self);
}

/// The virtual touchpad (Python: `VPad.update(fingers, gain, mirror)`); fingers = (id, x_mm, y_mm).
pub trait TouchpadOut {
    fn update(&mut self, fingers: &[(i32, f64, f64)], gain: f64, mirror: bool);
}

/// The live touch view (Python: `Probe`). `json` is one frame without the trailing newline.
pub trait ProbeSink {
    fn has_clients(&self) -> bool;
    fn send(&mut self, json: &serde_json::Value);
}

/// What the Engine starts outside itself (Python: `run_action(cmd)`, `workspace_step(forward, ...)`).
pub trait Actions {
    fn run_action(&mut self, cmd: &str);
    fn workspace_step(&mut self, forward: bool, cfg: &Config);
}

/// What the Engine reads from the grabbed device (Python: `dev.absinfo`, EVIOCGMTSLOTS, `active_keys`, capabilities).
pub trait DeviceInfo {
    /// (min, max) of an ABS axis, None if unknown.
    fn abs_range(&self, code: u16) -> Option<(i32, i32)>;
    /// EVIOCGMTSLOTS for one ABS_MT_* code: the value per slot.
    fn mt_slots(&self, code: u16) -> Option<Vec<i32>>;
    fn active_keys(&self) -> Vec<u16>;
    /// The driver passes the firmware state on (EV_MSC / MSC_RAW in the capabilities).
    fn has_msc_raw(&self) -> bool;
}

/// Python `log(msg)`: stderr with the daemon's prefix, picked up by journald.
pub fn log(msg: &str) {
    eprintln!("{msg}");
}
