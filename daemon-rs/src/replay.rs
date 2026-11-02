//! Golden-trace replay: `--replay <events> [--config <toml>] [--probe]` (PORT.md, "Replay contract").
//! Owner: Sonde. The Python reference is `tools/golden.py`; its docstring lists the choices both
//! sides honour (fake device, touchpad mode, ticks, rounding). `tools/compare.py` diffs the traces.
//!
//! Input lines: `A <json>` attach, `M <json>` device state from now on, `E <ts> <type> <code> <value>`
//! feed, `T <now>` tick, `C <now>` control click (Hyprland's event); blank lines and `#` comments skipped.
//! Output: one trace line per call the Engine makes on the outside world (W, S, P, X, K, B).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::rc::Rc;

use crate::iface::{Actions, Config, DeviceInfo, Event, Output, ProbeSink, TouchpadOut};

const SLOTS: usize = 16; // ABS_MT_SLOT 0..15, like hid-magicmouse
const ABS_MT_SLOT: u16 = 47;
const ABS_MT_TOUCH_MAJOR: u16 = 48;
const ABS_MT_TOUCH_MINOR: u16 = 49;
const ABS_MT_POSITION_X: u16 = 53;
const ABS_MT_POSITION_Y: u16 = 54;
const ABS_MT_TRACKING_ID: u16 = 57;

/// What the replay needs from the Engine (PORT.md "Engine API", plus `control_click`).
/// The real Engine gets it by delegation below; tests use a fake.
pub trait ReplayEngine {
    fn attach(&mut self, dev: Box<dyn DeviceInfo>);
    fn set_vpad(&mut self, vpad: Option<Box<dyn TouchpadOut>>);
    fn set_probe(&mut self, probe: Option<Box<dyn ProbeSink>>);
    fn feed(&mut self, ev: Event);
    fn tick(&mut self, now: f64);
    fn control_click(&mut self, now: f64);
}

// Bind to the real Engine and config. Henry's engine.rs is not merged yet: until then this part is
// compiled out. Once Engine has the API of PORT.md (and control_click), drop both cfg lines.
#[cfg(feature = "replay")]
impl ReplayEngine for crate::engine::Engine {
    fn attach(&mut self, dev: Box<dyn DeviceInfo>) {
        crate::engine::Engine::attach(self, dev)
    }
    fn set_vpad(&mut self, vpad: Option<Box<dyn TouchpadOut>>) {
        crate::engine::Engine::set_vpad(self, vpad)
    }
    fn set_probe(&mut self, probe: Option<Box<dyn ProbeSink>>) {
        crate::engine::Engine::set_probe(self, probe)
    }
    fn feed(&mut self, ev: Event) {
        crate::engine::Engine::feed(self, ev)
    }
    fn tick(&mut self, now: f64) {
        crate::engine::Engine::tick(self, now)
    }
    fn control_click(&mut self, now: f64) {
        crate::engine::Engine::control_click(self, now)
    }
}

/// Replay one events file through the real Engine and write the trace to `out`.
#[cfg(feature = "replay")]
pub fn run(events: &Path, config: Option<&Path>, probe: bool, out: &mut dyn Write) -> io::Result<()> {
    let cfg = match config {
        Some(p) => crate::config::load_config(p),
        None => crate::config::defaults(),
    };
    let input = BufReader::new(File::open(events)?);
    run_with(
        |cfg, ui, actions| crate::engine::Engine::new(cfg, false, ui, actions),
        cfg,
        input,
        probe,
        out,
    )
}

/// Open an events file (for callers that bind their own engine).
pub fn open(events: &Path) -> io::Result<BufReader<File>> {
    Ok(BufReader::new(File::open(events)?))
}

// ------------------------------------------------------------------ recorders

type Sink = Rc<RefCell<String>>;

fn emit(sink: &Sink, line: &str) {
    let mut s = sink.borrow_mut();
    s.push_str(line);
    s.push('\n');
}

/// Python f"{v:.6f}" (both round the exact binary value to nearest, ties to even).
pub fn f6(v: f64) -> String {
    format!("{v:.6}")
}

struct MouseRec(Sink);
impl Output for MouseRec {
    fn write(&mut self, kind: u16, code: u16, value: i32) {
        emit(&self.0, &format!("W {kind} {code} {value}"));
    }
    fn syn(&mut self) {
        emit(&self.0, "S");
    }
}

struct PadRec(Sink);
impl TouchpadOut for PadRec {
    fn update(&mut self, fingers: &[(i32, f64, f64)], gain: f64, mirror: bool) {
        emit(&self.0, &p_line(fingers, gain, mirror));
    }
}

/// `P <gain> <mirror 0|1> <id:x:y,...>`, "-" for no fingers.
pub fn p_line(fingers: &[(i32, f64, f64)], gain: f64, mirror: bool) -> String {
    let fs: Vec<String> = fingers.iter().map(|(t, x, y)| format!("{t}:{}:{}", f6(*x), f6(*y))).collect();
    let fs = if fs.is_empty() { "-".to_string() } else { fs.join(",") };
    format!("P {} {} {fs}", f6(gain), u8::from(mirror))
}

struct ProbeRec(Sink);
impl ProbeSink for ProbeRec {
    fn has_clients(&self) -> bool {
        true
    }
    fn send(&mut self, json: &serde_json::Value) {
        let mut s = String::from("B ");
        canonical(json, &mut s);
        emit(&self.0, &s);
    }
}

/// JSON with sorted keys and no spaces (Python: json.dumps(sort_keys=True, separators=(",", ":"))),
/// whatever map order serde_json was built with. Numbers in serde_json's spelling: compare.py
/// compares B lines as JSON.
pub fn canonical(v: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match v {
        Value::Object(m) => {
            let sorted: BTreeMap<&String, &Value> = m.iter().collect();
            out.push('{');
            for (i, (k, v)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                canonical(v, out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(v, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

struct ActRec(Sink);
impl Actions for ActRec {
    fn run_action(&mut self, cmd: &str) {
        emit(&self.0, &format!("X {cmd}"));
    }
    fn workspace_step(&mut self, forward: bool, end: &str, fallback: &str, include_empty: bool) {
        emit(&self.0, &format!("K {} {} {end} {fallback}", u8::from(forward), u8::from(include_empty)));
    }
}

// ------------------------------------------------------------------ the fake device

/// What the kernel holds (A and M lines): current slot, active slots, keys held.
#[derive(Default)]
struct DevState {
    slot: i32,
    slots: BTreeMap<usize, [i32; 5]>, // slot -> tracking id, x, y, major, minor
    keys: Vec<u16>,
}

impl DevState {
    fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        let mut st = DevState { slot: v.get("slot").and_then(|s| s.as_i64()).unwrap_or(0) as i32, ..Default::default() };
        if let Some(slots) = v.get("slots") {
            for s in slots.as_array().ok_or("slots: not a list")? {
                let n: Vec<i64> = s
                    .as_array()
                    .ok_or("slot: not a list")?
                    .iter()
                    .map(|x| x.as_i64().ok_or("slot: not an integer"))
                    .collect::<Result<_, _>>()?;
                if n.len() < 6 || n[0] < 0 || n[0] as usize >= SLOTS {
                    return Err("slot: want [slot, tid, x, y, major, minor]".into());
                }
                st.slots.insert(n[0] as usize, [n[1] as i32, n[2] as i32, n[3] as i32, n[4] as i32, n[5] as i32]);
            }
        }
        if let Some(keys) = v.get("keys") {
            for k in keys.as_array().ok_or("keys: not a list")? {
                st.keys.push(k.as_u64().ok_or("key: not an integer")? as u16);
            }
        }
        Ok(st)
    }
}

struct ReplayDevice {
    x: (i32, i32),
    y: (i32, i32),
    msc_raw: bool,
    state: Rc<RefCell<DevState>>,
}

impl DeviceInfo for ReplayDevice {
    fn abs_range(&self, code: u16) -> Option<(i32, i32)> {
        match code {
            ABS_MT_POSITION_X => Some(self.x),
            ABS_MT_POSITION_Y => Some(self.y),
            ABS_MT_SLOT => Some((0, SLOTS as i32 - 1)),
            _ => None,
        }
    }
    fn mt_slots(&self, code: u16) -> Option<Vec<i32>> {
        let col = match code {
            ABS_MT_TRACKING_ID => Some(0),
            ABS_MT_POSITION_X => Some(1),
            ABS_MT_POSITION_Y => Some(2),
            ABS_MT_TOUCH_MAJOR => Some(3),
            ABS_MT_TOUCH_MINOR => Some(4),
            _ => None,
        };
        let st = self.state.borrow();
        Some(
            (0..SLOTS)
                .map(|i| match (st.slots.get(&i), col) {
                    (Some(v), Some(c)) => v[c],
                    _ if code == ABS_MT_TRACKING_ID => -1,
                    _ => 0,
                })
                .collect(),
        )
    }
    fn active_keys(&self) -> Vec<u16> {
        self.state.borrow().keys.clone()
    }
    fn has_msc_raw(&self) -> bool {
        self.msc_raw
    }
}

fn range(v: &serde_json::Value, key: &str, default: (i32, i32)) -> Result<(i32, i32), String> {
    match v.get(key) {
        None => Ok(default),
        Some(r) => match r.as_array().map(|a| a.iter().map(|x| x.as_i64()).collect::<Vec<_>>()).as_deref() {
            Some([Some(lo), Some(hi)]) => Ok((*lo as i32, *hi as i32)),
            _ => Err(format!("{key}: want [min, max]")),
        },
    }
}

// ------------------------------------------------------------------ the loop

fn bad(no: usize, msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("events line {no}: {msg}"))
}

fn num<T: std::str::FromStr>(no: usize, s: Option<&str>) -> io::Result<T> {
    s.and_then(|s| s.parse().ok()).ok_or_else(|| bad(no, "bad or missing number"))
}

/// Replay `input` through the engine `make` builds from the config and the recorders.
/// Touchpad mode: after the first attach with touchpad.enabled, the recorder becomes the virtual
/// touchpad (Python: Session.sync_vpad; Engine::set_vpad does stop_coast, scrolling = false, reset_tp).
pub fn run_with<E: ReplayEngine>(
    make: impl FnOnce(Config, Box<dyn Output>, Box<dyn Actions>) -> E,
    cfg: Config,
    input: impl BufRead,
    probe: bool,
    out: &mut dyn Write,
) -> io::Result<()> {
    let sink: Sink = Rc::new(RefCell::new(String::new()));
    let touchpad = cfg.b("touchpad", "enabled");
    let mut eng = make(cfg, Box::new(MouseRec(sink.clone())), Box::new(ActRec(sink.clone())));
    if probe {
        eng.set_probe(Some(Box::new(ProbeRec(sink.clone()))));
    }
    let mut state: Option<Rc<RefCell<DevState>>> = None;
    let mut vpad_set = false;
    for (i, line) in input.lines().enumerate() {
        let no = i + 1;
        let line = line?;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (tag, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        match tag {
            "E" => {
                let mut p = rest.split_ascii_whitespace();
                let ev = Event {
                    ts: num(no, p.next())?,
                    kind: num(no, p.next())?,
                    code: num(no, p.next())?,
                    value: num(no, p.next())?,
                };
                eng.feed(ev);
            }
            "T" => eng.tick(num(no, Some(rest.trim()))?),
            "C" => eng.control_click(num(no, Some(rest.trim()))?),
            "A" | "M" => {
                let v: serde_json::Value = serde_json::from_str(rest).map_err(|e| bad(no, e))?;
                let st = DevState::from_json(&v).map_err(|e| bad(no, e))?;
                if tag == "M" {
                    let cur = state.as_ref().ok_or_else(|| bad(no, "M before A"))?;
                    *cur.borrow_mut() = st;
                } else {
                    let st = Rc::new(RefCell::new(st));
                    state = Some(st.clone());
                    let dev = ReplayDevice {
                        x: range(&v, "x", (-1100, 1258)).map_err(|e| bad(no, e))?,
                        y: range(&v, "y", (-1589, 2047)).map_err(|e| bad(no, e))?,
                        msc_raw: v.get("msc_raw").and_then(|m| m.as_bool()).unwrap_or(false),
                        state: st,
                    };
                    eng.attach(Box::new(dev));
                    if touchpad && !vpad_set {
                        eng.set_vpad(Some(Box::new(PadRec(sink.clone()))));
                        vpad_set = true;
                    }
                }
            }
            _ => return Err(bad(no, format!("unknown line {:?}", &line[..line.len().min(40)]))),
        }
        let mut s = sink.borrow_mut();
        if !s.is_empty() {
            out.write_all(s.as_bytes())?;
            s.clear();
        }
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Echoes its inputs through the recorders, so the plumbing shows in the trace.
    struct Fake {
        ui: Box<dyn Output>,
        act: Box<dyn Actions>,
        vpad: Option<Box<dyn TouchpadOut>>,
        probe: Option<Box<dyn ProbeSink>>,
        dev: Option<Box<dyn DeviceInfo>>,
    }

    impl ReplayEngine for Fake {
        fn attach(&mut self, dev: Box<dyn DeviceInfo>) {
            self.dev = Some(dev);
        }
        fn set_vpad(&mut self, vpad: Option<Box<dyn TouchpadOut>>) {
            self.vpad = vpad;
        }
        fn set_probe(&mut self, probe: Option<Box<dyn ProbeSink>>) {
            self.probe = probe;
        }
        fn feed(&mut self, ev: Event) {
            self.ui.write(ev.kind, ev.code, ev.value);
            if ev.kind == 0 {
                self.ui.syn();
                let dev = self.dev.as_ref().unwrap();
                let tids = dev.mt_slots(ABS_MT_TRACKING_ID).unwrap();
                let xs = dev.mt_slots(ABS_MT_POSITION_X).unwrap();
                if let Some(p) = self.probe.as_mut() {
                    p.send(&serde_json::json!({"t": ev.ts, "b": tids[0], "a": [xs[0], dev.active_keys()]}));
                }
                if let Some(v) = self.vpad.as_mut() {
                    v.update(&[(7, ev.ts, -0.0000001), (8, 1.0 / 3.0, 2.5)], 2.7, true);
                    v.update(&[], 2.0, false);
                }
            }
        }
        fn tick(&mut self, now: f64) {
            self.act.run_action(&format!("tick {now}"));
        }
        fn control_click(&mut self, now: f64) {
            self.act.workspace_step(now > 0.0, "wrap", "dispatch x y", true);
        }
    }

    fn trace(cfg: &str, input: &str, probe: bool) -> String {
        let mut c = crate::iface::Config::default();
        let tp: toml::Table = toml::from_str(cfg).unwrap();
        for (sec, vals) in tp {
            let m = vals.as_table().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            c.0.insert(sec, m);
        }
        let mut out = Vec::new();
        run_with(
            |_, ui, act| Fake { ui, act, vpad: None, probe: None, dev: None },
            c,
            input.as_bytes(),
            probe,
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn lines_and_formats() {
        let input = "# comment\n\nA {\"msc_raw\":false,\"x\":[-1100,1258],\"y\":[-1589,2047]}\n\
                     E 1000.25 3 53 -12\nE 1000.25 0 0 0\nT 1000.258\nC 1000.3\n\
                     M {\"keys\":[272],\"slot\":0,\"slots\":[[0,101,-5,6,600,400]]}\nE 1000.5 0 0 0\n";
        let t = trace("[touchpad]\nenabled = true\n", input, true);
        assert_eq!(
            t,
            "W 3 53 -12\nW 0 0 0\nS\nB {\"a\":[0,[]],\"b\":-1,\"t\":1000.25}\n\
             P 2.700000 1 7:1000.250000:-0.000000,8:0.333333:2.500000\nP 2.000000 0 -\n\
             X tick 1000.258\nK 1 1 wrap dispatch x y\n\
             W 0 0 0\nS\nB {\"a\":[-5,[272]],\"b\":101,\"t\":1000.5}\n\
             P 2.700000 1 7:1000.500000:-0.000000,8:0.333333:2.500000\nP 2.000000 0 -\n"
        );
    }

    #[test]
    fn legacy_mode_has_no_touchpad_and_no_probe() {
        let t = trace("[touchpad]\nenabled = false\n", "A {}\nE 1.5 0 0 0\n", false);
        assert_eq!(t, "W 0 0 0\nS\n");
    }

    #[test]
    fn errors_name_the_line() {
        let mut out = Vec::new();
        let mut c = crate::iface::Config::default();
        c.0.insert("touchpad".into(), [("enabled".to_string(), toml::Value::Boolean(false))].into());
        let e = run_with(|_, ui, act| Fake { ui, act, vpad: None, probe: None, dev: None }, c, "A {}\nE 1 x 0 0\n".as_bytes(), false, &mut out)
            .unwrap_err();
        assert!(e.to_string().contains("line 2"), "{e}");
    }

    #[test]
    fn every_golden_events_file_parses() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
        let manifest: serde_json::Value =
            serde_json::from_reader(File::open(dir.join("manifest.json")).unwrap()).unwrap();
        for case in manifest.as_array().unwrap() {
            let events = dir.join(case["events"].as_str().unwrap());
            let mut c = crate::iface::Config::default();
            c.0.insert("touchpad".into(), [("enabled".to_string(), toml::Value::Boolean(true))].into());
            run_with(
                |_, ui, act| Fake { ui, act, vpad: None, probe: None, dev: None },
                c,
                open(&events).unwrap(),
                true,
                &mut io::sink(),
            )
            .unwrap_or_else(|e| panic!("{}: {e}", events.display()));
        }
    }

    #[test]
    fn f6_matches_python() {
        // Python: f"{v:.6f}"
        assert_eq!(f6(0.0078125), "0.007812");
        assert_eq!(f6(0.0234375), "0.023438");
        assert_eq!(f6(-0.0), "-0.000000");
        assert_eq!(f6(-1e-9), "-0.000000");
        assert_eq!(f6(1793537443.2710001), "1793537443.271000");
    }
}
