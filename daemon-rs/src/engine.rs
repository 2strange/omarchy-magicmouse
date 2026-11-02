//! The touch and pointer engine (Python: `class Engine`, v1.4.1), ported 1:1: same arithmetic in
//! the same order, same state transitions, same calls on the outside world. Owner: Henry.
use std::collections::{BTreeSet, VecDeque};
use std::io::Write as _;

use serde_json::json;

use crate::curve::{AppleCurve, MAC_CURSOR_PPI};
use crate::iface::{log, Actions, Config, DeviceInfo, Event, Output, ProbeSink, TouchpadOut};
use crate::touch::{fsum, hypot, pmax, pmin, ppow, round_int, round_to, OMap, Touch, MOVE_WINDOW};

// ---- evdev codes
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;
pub const MSC_RAW: u16 = 0x03;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const REL_WHEEL_HI_RES: u16 = 0x0b;
pub const REL_HWHEEL_HI_RES: u16 = 0x0c;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
pub const ABS_MT_TOUCH_MINOR: u16 = 0x31;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;

// ---- constants (Python l.53-104, 1063-1071)
/// REL_WHEEL_HI_RES units per wheel notch
pub const HIRES: i64 = 120;
/// base: 1 mm of finger = ~0.11 notch
pub const HIRES_PER_MM: f64 = 13.0;
/// the touch surface is really ~38 units/mm on both axes (the driver's advertised values are wrong)
pub const TOUCH_UNITS_PER_MM: f64 = 38.0;
/// sideways travel must beat lengthwise travel by this much to scroll horizontally
pub const HORIZONTAL_RATIO: f64 = 2.0;
pub const TICK_HZ: f64 = 125.0;
pub const TICK: f64 = 1.0 / TICK_HZ;
/// Magic Mouse reports at ~90 Hz
pub const NOMINAL_DT: f64 = 0.011;
/// smoothing window for the finger speed that drives the scroll ramp
pub const SCROLL_SPEED_MS: f64 = 40.0;
/// smoothing window for the wheel velocity momentum starts from
pub const SCROLL_VEL_MS: f64 = 60.0;
pub const PAIR_MOVING_MM: f64 = 0.25;
pub const PAIR_RATIO: f64 = 0.3;
pub const MOVING_MM: f64 = 0.5;
pub const MIN_SLIDE_MM: f64 = 1.0;
pub const POINTER_RATIO: f64 = 3.0;
/// seconds around a press or release in which no scroll starts
pub const CLICK_QUIET: f64 = 0.25;
pub const BRIDGE_MS: f64 = 0.06;
pub const STALE_SEC: f64 = 0.3;
pub const FIRM_REPORTS: i64 = 3;
/// the firmware's contact states that touch (3 making touch, 4 touching)
pub const FW_TOUCHING: [i64; 2] = [3, 4];
pub const FW_PAIR_GAP_MM: f64 = 30.0;
pub const UNDECIPHERED: &str = "/sys/module/hid_magicmouse/parameters/report_undeciphered";
pub const EDGE_STRAIGHT: f64 = 0.6;
pub const JUMP_MM: f64 = 5.0;
pub const SURFACE_LENGTH_MM: f64 = (2047.0 + 1589.0) / TOUCH_UNITS_PER_MM;
pub const FREE_MINOR: f64 = 0.35;
pub const FREE_DIR_MS: f64 = 80.0;
pub const TP_PAIR_FRAMES: i64 = 3;
pub const TP_PAIR_YOUNG: f64 = 0.15;
pub const TP_SWIPE_RATIO: f64 = 1.5;
pub const TP_HOLD_SEC: f64 = 0.1;
pub const TP_CATCH_UP: f64 = 0.5;
pub const WORKSPACE_NEXT: &str = "workspace = \"e+1\"";
pub const WORKSPACE_PREV: &str = "workspace = \"e-1\"";

// ---- config access with Python's dict semantics
fn raw<'a>(cfg: &'a Config, sec: &str, key: &str) -> Option<&'a toml::Value> {
    cfg.0.get(sec).and_then(|s| s.get(key))
}

/// `float(cfg[sec].get(key, default))`
fn get_f(cfg: &Config, sec: &str, key: &str, default: f64) -> f64 {
    if raw(cfg, sec, key).is_some() {
        cfg.f(sec, key)
    } else {
        default
    }
}

/// `bool(cfg[sec].get(key, default))` (truthiness)
fn get_b(cfg: &Config, sec: &str, key: &str, default: bool) -> bool {
    if raw(cfg, sec, key).is_some() {
        cfg.b(sec, key)
    } else {
        default
    }
}

/// `cfg[sec].get(key, default) is True` (only the boolean true)
fn is_true(cfg: &Config, sec: &str, key: &str, default: bool) -> bool {
    match raw(cfg, sec, key) {
        Some(toml::Value::Boolean(b)) => *b,
        Some(_) => false,
        None => default,
    }
}

/// Python's repr of a tuple of ints, for log lines: (1,) / (1, 2) / None
fn tup(k: &Option<Vec<i64>>) -> String {
    match k {
        None => "None".into(),
        Some(v) if v.len() == 1 => format!("({},)", v[0]),
        Some(v) => format!("({})", v.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", ")),
    }
}

fn s_if(n: usize) -> &'static str {
    if n > 1 {
        "s"
    } else {
        ""
    }
}

fn ax(axis: &str) -> usize {
    if axis == "x" {
        0
    } else {
        1
    }
}

pub type Key = Vec<i64>;

pub struct Engine {
    pub cfg: Config,
    pub ui: Box<dyn Output>,
    pub actions: Box<dyn Actions>,
    pub verbose: bool,
    pub probe: Option<Box<dyn ProbeSink>>,
    /// --record: raw input events, one "ts type code value" line each
    pub record: Option<Box<dyn std::io::Write>>,
    pub vpad: Option<Box<dyn TouchpadOut>>,
    pub tp_key: Option<Key>,
    pub tp_pair_n: i64,
    pub tp_t0: f64,
    pub tp_from: (f64, f64),
    pub tp_light: OMap<i64, f64>,
    pub tp_swiped: bool,
    pub tp_owed: OMap<i64, [f64; 2]>,
    pub tp_out: OMap<i64, [f64; 2]>,
    pub why: String,
    pub why_locked: bool,
    pub curve: AppleCurve,
    pub res_x: f64,
    pub res_y: f64,
    pub dev: Option<Box<dyn DeviceInfo>>,
    pub dropped: bool,
    pub x_min: i64,
    pub x_max: i64,
    pub y_min: i64,
    pub y_max: i64,
    // frame accumulators
    pub f_dx: i64,
    pub f_dy: i64,
    pub f_btn: Vec<(u16, i32)>,
    // buttons
    pub pressed: OMap<u16, u16>,
    pub left_press_t: f64,
    pub last_button_t: f64,
    pub right_latched: bool,
    // pointer
    pub v: f64,
    pub last_motion_t: f64,
    pub rem_x: f64,
    pub rem_y: f64,
    pub hold_until: f64,
    pub hold_acc: [f64; 2],
    pub recent: VecDeque<(f64, f64)>,
    pub ptr_total: f64,
    pub lost: Option<(f64, i64, i64)>,
    // touch
    pub slots: OMap<i64, [i64; 5]>,
    pub cur_slot: i64,
    pub mt_frame: bool,
    pub mt_seen: BTreeSet<i64>,
    pub f_reported: BTreeSet<i64>,
    pub slot_seen: OMap<i64, f64>,
    pub parked: OMap<i64, [i64; 5]>,
    pub slot_fw: OMap<i64, i64>,
    pub fw_seen: bool,
    pub fw_slot_ok: bool,
    pub fw_cache: (f64, String),
    /// report_undeciphered in sysfs (UNDECIPHERED); None = counts as missing (replay)
    pub undeciphered: Option<std::path::PathBuf>,
    pub parked_touch: OMap<i64, Touch>,
    pub pending_lift: Option<f64>,
    pub touches: OMap<i64, Touch>,
    pub primary: Option<Key>,
    pub t_ref: Option<(f64, f64)>,
    pub scrolling: bool,
    pub axis: &'static str,
    pub start_axis: &'static str,
    pub s_travel_x: f64,
    pub s_rem: [f64; 2],
    pub s_notch: [i64; 2],
    pub s_vel: f64,
    pub s_vel_xy: [f64; 2],
    pub s_dir: [f64; 2],
    pub s_speed: f64,
    pub s_last_t: f64,
    pub s_travel: f64,
    pub scroll_t0: f64,
    // touch session and gestures
    pub session_open: bool,
    pub session_t0: f64,
    pub session_travel: f64,
    pub session_clicked: bool,
    pub max_fingers: usize,
    pub gesture_fired: bool,
    pub session_scrolled: bool,
    pub two_key: Option<Key>,
    pub two_ref: Option<(f64, f64)>,
    pub last_tap_t: f64,
    pub last_tap_fingers: usize,
    // momentum
    pub coasting: bool,
    pub c_vel: f64,
    pub c_dir: (f64, f64),
    pub c_t0: f64,
    pub c_last_t: f64,
}

impl Engine {
    pub fn new(cfg: Config, verbose: bool, ui: Box<dyn Output>, actions: Box<dyn Actions>) -> Self {
        let curve = AppleCurve::new(cfg.f("pointer", "tracking_speed"));
        Engine {
            cfg,
            ui,
            actions,
            verbose,
            probe: None,
            record: None,
            vpad: None,
            tp_key: None,
            tp_pair_n: 0,
            tp_t0: 0.0,
            tp_from: (0.0, 0.0),
            tp_light: OMap::new(),
            tp_swiped: false,
            tp_owed: OMap::new(),
            tp_out: OMap::new(),
            why: String::new(),
            why_locked: false,
            curve,
            res_x: TOUCH_UNITS_PER_MM,
            res_y: TOUCH_UNITS_PER_MM,
            dev: None,
            dropped: false,
            x_min: -1100,
            x_max: 1258,
            y_min: -1589,
            y_max: 2047,
            f_dx: 0,
            f_dy: 0,
            f_btn: Vec::new(),
            pressed: OMap::new(),
            left_press_t: -1.0,
            last_button_t: -1.0,
            right_latched: false,
            v: 0.0,
            last_motion_t: -1.0,
            rem_x: 0.0,
            rem_y: 0.0,
            hold_until: 0.0,
            hold_acc: [0.0, 0.0],
            recent: VecDeque::new(),
            ptr_total: 0.0,
            lost: None,
            slots: OMap::new(),
            cur_slot: 0,
            mt_frame: false,
            mt_seen: BTreeSet::new(),
            f_reported: BTreeSet::new(),
            slot_seen: OMap::new(),
            parked: OMap::new(),
            slot_fw: OMap::new(),
            fw_seen: false,
            fw_slot_ok: false,
            fw_cache: (-1.0, String::new()),
            undeciphered: Some(UNDECIPHERED.into()),
            parked_touch: OMap::new(),
            pending_lift: None,
            touches: OMap::new(),
            primary: None,
            t_ref: None,
            scrolling: false,
            axis: "y",
            start_axis: "y",
            s_travel_x: 0.0,
            s_rem: [0.0, 0.0],
            s_notch: [0, 0],
            s_vel: 0.0,
            s_vel_xy: [0.0, 0.0],
            s_dir: [0.0, 0.0],
            s_speed: 0.0,
            s_last_t: -1.0,
            s_travel: 0.0,
            scroll_t0: 0.0,
            session_open: false,
            session_t0: 0.0,
            session_travel: 0.0,
            session_clicked: false,
            max_fingers: 0,
            gesture_fired: false,
            session_scrolled: false,
            two_key: None,
            two_ref: None,
            last_tap_t: -1.0,
            last_tap_fingers: 0,
            coasting: false,
            c_vel: 0.0,
            c_dir: (0.0, 1.0),
            c_t0: 0.0,
            c_last_t: 0.0,
        }
    }

    /// Live reload (Python `update_config`).
    pub fn set_config(&mut self, cfg: Config) {
        self.curve = AppleCurve::new(cfg.f("pointer", "tracking_speed"));
        self.cfg = cfg;
    }

    /// Touchpad mode on or off (Python `Session.sync_vpad`): on stops a glide and a scroll, both
    /// reset the touchpad state (`Session.reset_tp`). Call it only when the mode changes.
    pub fn set_vpad(&mut self, vpad: Option<Box<dyn TouchpadOut>>) {
        let on = vpad.is_some();
        self.vpad = vpad;
        if on {
            self.stop_coast("touchpad mode");
            self.scrolling = false;
        }
        self.reset_tp();
    }

    /// Python `Session.reset_tp`.
    pub fn reset_tp(&mut self) {
        self.tp_key = None;
        self.tp_light.clear();
        self.tp_owed.clear();
        self.tp_out.clear();
        self.tp_swiped = false;
        self.tp_pair_n = 0;
    }

    pub fn set_probe(&mut self, probe: Option<Box<dyn ProbeSink>>) {
        self.probe = probe;
    }

    /// --record (Python `eng.record`): every input event is written there before it is processed.
    pub fn set_record(&mut self, w: Option<Box<dyn std::io::Write>>) {
        self.record = w;
    }

    /// ((x_min, x_max), (y_min, y_max), res_x): the touch surface, for VPad::new.
    pub fn surface(&self) -> ((i32, i32), (i32, i32), f64) {
        ((self.x_min as i32, self.x_max as i32), (self.y_min as i32, self.y_max as i32), self.res_x)
    }

    /// Python `bool(eng.touches)`: a finger is on the mouse (the probe follows that mouse).
    pub fn has_touches(&self) -> bool {
        !self.touches.is_empty()
    }

    /// Where report_undeciphered is read (fw_status, for the probe); None = it counts as missing.
    pub fn set_undeciphered_path(&mut self, p: Option<std::path::PathBuf>) {
        self.undeciphered = p;
    }

    pub fn coasting(&self) -> bool {
        self.coasting
    }

    fn t(&self, tid: i64) -> &Touch {
        &self.touches[tid]
    }

    fn t_mut(&mut self, tid: i64) -> &mut Touch {
        self.touches.get_mut(tid).unwrap_or_else(|| panic!("KeyError: {tid}"))
    }

    fn recent_of(&self, tid: i64) -> (f64, f64) {
        let (x, y) = self.t(tid).recent();
        (x as f64, y as f64)
    }

    // ---- device attach
    pub fn attach(&mut self, dev: Box<dyn DeviceInfo>) {
        self.dev = Some(dev);
        self.fw_seen = false;
        self.fw_cache = (-1.0, String::new());
        self.reset_touch();
        self.v = 0.0;
        self.last_motion_t = -1.0;
        let dev = self.dev.as_ref().expect("dev");
        if let Some((lo, hi)) = dev.abs_range(ABS_MT_POSITION_X) {
            self.x_min = lo as i64;
            self.x_max = hi as i64;
            if let Some((lo, hi)) = dev.abs_range(ABS_MT_POSITION_Y) {
                self.y_min = lo as i64;
                self.y_max = hi as i64;
            }
        }
        self.sync_mt();
    }

    pub fn reset_touch(&mut self) {
        self.slots.clear();
        self.slot_fw.clear();
        self.parked.clear();
        self.parked_touch.clear();
        self.slot_seen.clear();
        self.touches.clear();
        self.cur_slot = 0;
        self.mt_frame = false;
        self.mt_seen.clear();
        self.pending_lift = None;
        self.primary = None;
        self.t_ref = None;
        self.scrolling = false;
        self.session_open = false;
        self.two_key = None;
        self.two_ref = None;
        self.stop_coast("reset");
    }

    /// Load the kernel's current multitouch state (EVIOCGMTSLOTS through DeviceInfo).
    pub fn sync_mt(&mut self) {
        let Some(dev) = self.dev.as_ref() else {
            return;
        };
        let (Some((_, slot_max)), Some(cur)) = (dev.abs_range(ABS_MT_SLOT), dev.abs_value(ABS_MT_SLOT)) else {
            log("touch state: no ABS_MT_SLOT info");
            return;
        };
        let n = (slot_max as i64 + 1).max(0) as usize;
        let mut cols = Vec::new();
        for code in [
            ABS_MT_TRACKING_ID,
            ABS_MT_POSITION_X,
            ABS_MT_POSITION_Y,
            ABS_MT_TOUCH_MAJOR,
            ABS_MT_TOUCH_MINOR,
        ] {
            match dev.mt_slots(code) {
                Some(mut v) => {
                    v.resize(n, 0);
                    cols.push(v);
                }
                None => {
                    log(&format!("touch state: EVIOCGMTSLOTS {code:#x} failed"));
                    return;
                }
            }
        }
        self.parked.clear();
        self.parked_touch.clear();
        self.slot_seen.clear();
        self.slot_fw.clear(); // not readable by ioctl: known again with the next report
        self.slots.clear();
        #[allow(clippy::needless_range_loop)]
        for i in 0..n {
            if cols[0][i] >= 0 {
                self.slots.insert(
                    i as i64,
                    [
                        cols[0][i] as i64,
                        cols[1][i] as i64,
                        cols[2][i] as i64,
                        cols[3][i] as i64,
                        cols[4][i] as i64,
                    ],
                );
            }
        }
        self.cur_slot = cur as i64;
    }

    // ---- helpers
    /// Pixels per raw count at smoothed speed v (raw counts/ms), macOS style.
    pub fn pointer_gain(&self, v: f64) -> f64 {
        let cpi = pmax(self.cfg.f("pointer", "cpi"), 1.0);
        let x = v * 1000.0 / cpi;
        self.curve.gain(x) * MAC_CURSOR_PPI / cpi * self.cfg.f("pointer", "speed")
    }

    fn emit_rel(&mut self, code: u16, value: i64) {
        if value != 0 {
            self.ui.write(EV_REL, code, value as i32);
        }
    }

    /// Accumulate fractional hi-res units, emit hi-res + notch events.
    fn emit_scroll(&mut self, hires: f64, axis: &str) {
        let a = ax(axis);
        self.s_rem[a] += hires;
        let n = self.s_rem[a] as i64; // int(): toward zero
        if n == 0 {
            return;
        }
        self.s_rem[a] -= n as f64;
        if axis == "y" {
            self.emit_rel(REL_WHEEL_HI_RES, n);
        } else {
            self.emit_rel(REL_HWHEEL_HI_RES, n);
        }
        self.s_notch[a] += n;
        while self.s_notch[a].abs() >= HIRES {
            let step = if self.s_notch[a] > 0 { 1 } else { -1 };
            self.emit_rel(if axis == "y" { REL_WHEEL } else { REL_HWHEEL }, step);
            self.s_notch[a] -= step * HIRES;
        }
    }

    // ---- event intake
    pub fn feed(&mut self, ev: Event) {
        if let Some(r) = self.record.as_mut() {
            let _ = writeln!(r, "{:.6} {} {} {}", ev.ts, ev.kind, ev.code, ev.value);
        }
        if ev.kind == EV_SYN {
            if ev.code == SYN_DROPPED {
                self.dropped = true;
                log("SYN_DROPPED");
            } else if ev.code == SYN_REPORT {
                self.fw_slot_ok = false;
                if self.dropped {
                    self.resync(ev.ts);
                } else {
                    self.process_frame(ev.ts);
                }
            }
            return;
        }
        if self.dropped {
            return;
        }
        let value = ev.value as i64;
        match ev.kind {
            EV_REL => {
                if ev.code == REL_X {
                    self.f_dx += value;
                } else if ev.code == REL_Y {
                    self.f_dy += value;
                }
            }
            EV_KEY => {
                if [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE].contains(&ev.code) {
                    self.f_btn.push((ev.code, ev.value));
                }
            }
            EV_MSC => {
                if ev.code == MSC_RAW {
                    if self.fw_slot_ok {
                        self.slot_fw.insert(self.cur_slot, value >> 4);
                    }
                    self.fw_slot_ok = false;
                    if !self.fw_seen {
                        self.fw_seen = true;
                        log("touch: the mouse's own touch state is available (report_undeciphered)");
                    }
                }
            }
            EV_ABS => {
                self.fw_slot_ok = true;
                self.mt_frame = true;
                if ev.code == ABS_MT_SLOT {
                    self.cur_slot = value;
                }
                self.mt_seen.insert(self.cur_slot);
                if ev.code != ABS_MT_SLOT {
                    let cs = self.cur_slot;
                    if !self.slots.contains_key(cs) && ev.code != ABS_MT_TRACKING_ID && self.parked.contains_key(cs) {
                        let p = self.parked.remove(cs).expect("parked");
                        self.slots.insert(cs, p); // still the same finger
                    }
                    if ev.code == ABS_MT_TRACKING_ID {
                        self.slot_fw.remove(cs);
                        if let Some(old) = self.parked.remove(cs) {
                            self.parked_touch.remove(old[0]);
                        }
                    }
                    let slot = self.slots.setdefault(cs, [-1, 0, 0, 0, 0]);
                    match ev.code {
                        ABS_MT_TRACKING_ID => slot[0] = value,
                        ABS_MT_POSITION_X => slot[1] = value,
                        ABS_MT_POSITION_Y => slot[2] = value,
                        ABS_MT_TOUCH_MAJOR => slot[3] = value,
                        ABS_MT_TOUCH_MINOR => slot[4] = value,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// After SYN_DROPPED: reload touches and release buttons whose release was lost.
    fn resync(&mut self, ts: f64) {
        self.dropped = false;
        self.mt_frame = false;
        self.mt_seen.clear();
        self.f_dx = 0;
        self.f_dy = 0;
        self.sync_mt();
        let down: Vec<u16> = match &self.dev {
            Some(d) => d.active_keys(),
            None => self.pressed.keys(),
        };
        self.f_btn = self
            .pressed
            .keys()
            .into_iter()
            .filter(|p| !down.contains(p))
            .map(|p| (p, 0))
            .collect();
        self.process_frame(ts);
    }

    // ---- per-frame processing
    fn process_frame(&mut self, ts: f64) {
        for &s in &self.mt_seen {
            self.slot_seen.insert(s, ts);
        }
        let lifted: Vec<i64> = self.slots.iter().filter(|(_, v)| v[0] < 0).map(|(s, _)| s).collect();
        let mut silent = Vec::new();
        for s in self.slots.keys() {
            if !self.mt_seen.contains(&s) && !lifted.contains(&s) && ts - *self.slot_seen.setdefault(s, ts) > STALE_SEC {
                silent.push(s);
            }
        }
        for &s in &silent {
            let v = self.slots[s];
            self.parked.insert(s, v);
        }
        let stale: Vec<i64> = lifted.iter().chain(silent.iter()).copied().collect();
        if !stale.is_empty() && self.verbose {
            log(&format!("touch: drop slots {stale:?}"));
        }
        for &s in &stale {
            self.slots.remove(s);
            self.slot_seen.remove(s);
            self.slot_fw.remove(s);
        }
        self.f_reported = self.mt_seen.clone();
        self.mt_frame = false;
        self.mt_seen.clear();

        let (dx, dy) = (self.f_dx, self.f_dy);
        let btns = std::mem::take(&mut self.f_btn);
        self.f_dx = 0;
        self.f_dy = 0;

        // buttons first, so motion in the press frame is covered by click tolerance
        let mut clicked = false;
        for (phys, value) in btns {
            self.last_button_t = ts;
            let code;
            if value == 1 {
                code = self.map_button(phys);
                self.pressed.insert(phys, code);
                clicked = true;
                self.stop_coast("button");
                self.hold_until = ts + self.cfg.f("pointer", "click_hold_ms") / 1000.0;
                self.hold_acc = [0.0, 0.0];
                if code == BTN_LEFT {
                    self.left_press_t = ts;
                }
            } else {
                // release what was pressed, even if buttons.middle changed in between
                code = match self.pressed.remove(phys) {
                    Some(c) => c,
                    None => self.map_button(phys),
                };
            }
            self.ui.write(EV_KEY, code, value);
            if value == 0 && code == BTN_LEFT && self.right_latched {
                self.ui.write(EV_KEY, BTN_RIGHT, 0);
                self.right_latched = false;
            }
        }

        if dx != 0 || dy != 0 {
            self.pointer(ts, dx, dy);
        }

        self.touch(ts);
        if clicked {
            self.session_clicked = true; // after touch(), so a same-frame landing can't reset it
        }
        self.ui.syn();
        if self.probe.as_ref().is_some_and(|p| p.has_clients()) {
            self.publish(ts);
        }
    }

    /// pointer_moving() without pruning self.recent, so watching never changes behaviour.
    pub fn moving_now(&self, ts: f64) -> bool {
        let cut = ts - self.cfg.f("scroll", "settle_ms") / 1000.0;
        fsum(self.recent.iter().filter(|(t, _)| *t >= cut).map(|(_, d)| *d)) > self.cfg.f("scroll", "settle_counts")
    }

    /// For the preview: does the mouse's own touch state reach the daemon?
    pub fn fw_status(&mut self, ts: f64) -> String {
        if ts - self.fw_cache.0 < 1.0 {
            return self.fw_cache.1.clone();
        }
        let status = if !get_b(&self.cfg, "scroll", "touch_firmware", true) {
            "disabled"
        } else if self.fw_seen {
            "active"
        } else {
            let ready = self.dev.as_ref().is_some_and(|d| d.has_msc_raw());
            let switched = self
                .undeciphered
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .is_some_and(|s| matches!(s.trim(), "Y" | "1"));
            if ready {
                "ready"
            } else if switched {
                "reconnect"
            } else {
                "off"
            }
        };
        self.fw_cache = (ts, status.to_string());
        status.to_string()
    }

    /// One probe frame: every contact with its size, role and firmware state, plus the decision.
    fn publish(&mut self, ts: f64) {
        let follow: Vec<i64> = self.primary.clone().unwrap_or_default();
        let mut contacts = Vec::new();
        for (s, v) in self.slots.iter() {
            if v[0] < 0 {
                continue;
            }
            let t = self.touches.get(v[0]);
            let role = match t {
                Some(t) if t.palm => "palm",
                Some(t) if t.grip => "grip",
                Some(t) if !t.ok => "hover",
                _ if follow.contains(&v[0]) => "follow",
                _ => "rest",
            };
            let rate = t.map_or(0, |t| round_int(t.rate));
            let fw = self.slot_fw.get(s).copied().unwrap_or(-1);
            contacts.push(json!([v[0], v[1], v[2], v[3], v[4], role, rate, fw]));
        }
        let moving = self.moving_now(ts);
        let fw = self.fw_status(ts);
        let zones: Vec<f64> = ["edge_left_mm", "edge_right_mm", "edge_front_mm", "edge_back_mm"]
            .iter()
            .map(|k| self.cfg.f("scroll", k))
            .collect();
        let frame = json!({
            "t": round_to(ts, 4),
            "c": contacts,
            "scroll": self.scrolling,
            "axis": self.axis,
            "coast": self.coasting,
            "moving": moving,
            "why": self.why,
            "fw": fw,
            "zones": zones,
        });
        if let Some(p) = self.probe.as_mut() {
            p.send(&frame);
        }
    }

    fn pointer(&mut self, ts: f64, dx: i64, dy: i64) {
        let mut dt = if self.last_motion_t >= 0.0 { ts - self.last_motion_t } else { NOMINAL_DT };
        if dt > 0.2 {
            self.v = 0.0;
            dt = NOMINAL_DT;
        }
        self.last_motion_t = ts;
        let dist = hypot(dx as f64, dy as f64);
        self.recent.push_back((ts, dist));
        self.ptr_total += dist;
        let keep = pmax(self.cfg.f("scroll", "settle_ms") / 1000.0, MOVE_WINDOW);
        while self.recent.front().is_some_and(|r| r.0 < ts - keep) {
            self.recent.pop_front();
        }
        if dt > 0.0 {
            let v_inst = dist / (dt * 1000.0);
            let a = 1.0 - (-(dt * 1000.0) / pmax(self.cfg.f("pointer", "smoothing_ms"), 1.0)).exp();
            self.v += a * (v_inst - self.v);
        }

        let (mut dx, mut dy) = (dx as f64, dy as f64);
        if ts < self.hold_until {
            self.hold_acc[0] += dx;
            self.hold_acc[1] += dy;
            if hypot(self.hold_acc[0], self.hold_acc[1]) < self.cfg.f("pointer", "click_drag_units") {
                return; // click tolerance: swallow the creep
            }
            self.hold_until = 0.0;
            dx = self.hold_acc[0];
            dy = self.hold_acc[1];
            self.hold_acc = [0.0, 0.0];
        }

        let gain = self.pointer_gain(self.v);
        let ox = dx * gain + self.rem_x;
        let oy = dy * gain + self.rem_y;
        let (ix, iy) = (ox as i64, oy as i64);
        self.rem_x = ox - ix as f64;
        self.rem_y = oy - iy as f64;
        self.emit_rel(REL_X, ix);
        self.emit_rel(REL_Y, iy);
    }

    fn map_button(&self, code: u16) -> u16 {
        if code == BTN_RIGHT && matches!(raw(&self.cfg, "buttons", "right_click"), Some(toml::Value::Boolean(false))) {
            return BTN_LEFT;
        }
        if code != BTN_MIDDLE {
            return code;
        }
        match self.cfg.s("buttons", "middle").to_lowercase().as_str() {
            "left" => BTN_LEFT,
            "right" => BTN_RIGHT,
            _ => BTN_MIDDLE,
        }
    }

    /// Hyprland swallowed a Control + left press on our device: answer with a right click.
    pub fn control_click(&mut self, now: f64) {
        if !self.cfg.b("buttons", "control_click") || self.right_latched {
            return;
        }
        if self.left_press_t < 0.0 || now - self.left_press_t > 1.0 {
            return;
        }
        self.left_press_t = -1.0;
        let held = self.pressed.values().any(|&c| c == BTN_LEFT);
        self.ui.write(EV_KEY, BTN_RIGHT, 1);
        if held {
            self.right_latched = true;
        } else {
            self.ui.write(EV_KEY, BTN_RIGHT, 0);
        }
        self.ui.syn();
    }

    /// Mouse gone: never leave a button held down on the virtual device.
    pub fn release_buttons(&mut self) {
        let mut codes: BTreeSet<u16> = self.pressed.values().copied().collect();
        if self.right_latched {
            codes.insert(BTN_RIGHT);
        }
        for code in codes {
            self.ui.write(EV_KEY, code, 0);
        }
        if !self.pressed.is_empty() || self.right_latched {
            self.ui.syn();
        }
        self.pressed.clear();
        self.right_latched = false;
    }

    fn nav(&mut self, back: bool) {
        let code = if back { BTN_SIDE } else { BTN_EXTRA };
        self.ui.write(EV_KEY, code, 1);
        self.ui.syn();
        self.ui.write(EV_KEY, code, 0);
        self.ui.syn();
        if self.verbose {
            log(&format!("swipe: {}", if back { "back" } else { "forward" }));
        }
    }

    /// Has the mouse itself really moved within the last settle_ms?
    fn pointer_moving(&mut self, ts: f64) -> bool {
        let cut = ts - self.cfg.f("scroll", "settle_ms") / 1000.0;
        while self.recent.front().is_some_and(|r| r.0 < cut) {
            self.recent.pop_front();
        }
        fsum(self.recent.iter().map(|(_, d)| *d)) > self.cfg.f("scroll", "settle_counts")
    }

    /// How far the mouse itself moved, in mm, within the last `window` seconds.
    fn pointer_mm(&self, ts: f64, window: f64) -> f64 {
        let counts = fsum(self.recent.iter().filter(|(t, _)| *t >= ts - window).map(|(_, d)| *d));
        counts * 25.4 / pmax(self.cfg.f("pointer", "cpi"), 1.0)
    }

    fn mm(&self, key: &str) -> f64 {
        pmax(self.cfg.f("scroll", key), 0.0)
    }

    fn touch(&mut self, ts: f64) {
        let mut live: OMap<i64, (i64, i64)> = OMap::new();
        let mut reports: OMap<i64, i64> = OMap::new();
        let mut fw: OMap<i64, i64> = OMap::new();
        for (_, v) in self.slots.iter() {
            if v[0] >= 0 {
                live.insert(v[0], (v[1], v[2]));
            }
        }
        // a report with no width (TOUCH_MINOR 0) is a finger hovering over the edge: size 0
        for (s, v) in self.slots.iter() {
            if v[0] >= 0 && self.f_reported.contains(&s) {
                reports.insert(v[0], if v[4] > 0 { v[3] } else { 0 });
            }
        }
        for (s, v) in self.slots.iter() {
            if v[0] >= 0 {
                if let Some(&f) = self.slot_fw.get(s) {
                    fw.insert(v[0], f);
                }
            }
        }
        let parked_ids: Vec<i64> = self.parked.values().map(|v| v[0]).collect();
        for tid in self.touches.keys().into_iter().filter(|t| !live.contains_key(*t)).collect::<Vec<_>>() {
            let gone = self.touches.remove(tid).expect("touch");
            if parked_ids.contains(&tid) {
                self.parked_touch.insert(tid, gone); // silent, not lifted: keep it for a restore
                continue;
            }
            if !gone.grip {
                self.lost = Some((ts, gone.x, gone.y));
            }
        }
        for tid in self.parked_touch.keys() {
            if !parked_ids.contains(&tid) && !live.contains_key(tid) {
                self.parked_touch.remove(tid);
            }
        }
        let left_to = self.x_min as f64 + self.mm("edge_left_mm") * self.res_x;
        let right_from = self.x_max as f64 - self.mm("edge_right_mm") * self.res_x;
        let front_to = self.y_min as f64 + self.mm("edge_front_mm") * self.res_y;
        let palm_from = self.y_max as f64 - self.mm("edge_back_mm") * self.res_y;
        for (tid, &(x, y)) in live.iter() {
            let mut have = self.touches.contains_key(tid);
            if !have && self.parked_touch.contains_key(tid) {
                let t = self.parked_touch.remove(tid).expect("parked touch");
                self.touches.insert(tid, t); // the same finger, speaking again
                have = true;
            } else if have {
                let t = self.t(tid);
                if hypot((x - t.x) as f64, (y - t.y) as f64) / self.res_x > JUMP_MM {
                    self.touches.remove(tid); // a new finger reusing the slot and the id: a new landing
                    if self.primary.as_ref().is_some_and(|p| p.contains(&tid)) {
                        self.t_ref = None;
                    }
                    have = false;
                }
            }
            if !have {
                let xf = x as f64;
                let yf = y as f64;
                let mut grip = (self.mm("edge_left_mm") > 0.0 && xf <= left_to)
                    || (self.mm("edge_right_mm") > 0.0 && xf >= right_from)
                    || (self.mm("edge_front_mm") > 0.0 && yf <= front_to);
                let mut palm = self.mm("edge_back_mm") > 0.0 && yf >= palm_from;
                if grip || palm {
                    if let Some(lost) = self.lost {
                        if ts - lost.0 < BRIDGE_MS && hypot((x - lost.1) as f64, (y - lost.2) as f64) / self.res_x < 5.0 {
                            grip = false; // a tracking-id flicker of a scrolling finger, not a new grip
                            palm = false;
                        }
                    }
                }
                let mut t = Touch::new(ts, x, y, grip, palm);
                t.ptr0 = self.ptr_total;
                self.touches.insert(tid, t);
            } else {
                let ptr_total = self.ptr_total;
                let res_x = self.res_x;
                let t = self.t_mut(tid);
                t.update(ts, x, y);
                let (rx, ry) = t.recent();
                if t.grip && !t.palm && hypot(rx as f64, ry as f64) / res_x < MOVING_MM / 2.0 {
                    t.x0 = x; // at rest: a later slide is judged from here
                    t.y0 = y;
                    t.path = 0.0;
                    t.ptr0 = ptr_total;
                }
                let t = self.t(tid);
                if t.grip && !t.palm && self.edge_finger_slides(ts, t) {
                    let t = self.t_mut(tid);
                    t.grip = false; // it came in from the edge but really slides: let it scroll
                    t.hist.clear();
                    t.hist.push_back((ts, x, y)); // judge its movement from here, not from the edge
                    if self.verbose {
                        log(&format!("edge finger {tid} released"));
                    }
                }
            }
        }

        self.confirm_touches(ts, &reports, &fw);
        self.why_locked = false;
        let mut fingers: Vec<i64> = self.touches.iter().filter(|(_, t)| !t.grip && t.ok).map(|(k, _)| k).collect();
        fingers.sort();
        if self.vpad.is_some() {
            self.touchpad_frame(ts, &fingers);
            return;
        }
        if fingers.is_empty() {
            self.why = self.idle_why();
            // A 1-2 report dropout looks like a lift. Wait BRIDGE_MS before ending the scroll.
            if self.scrolling {
                if self.pending_lift.is_none() {
                    self.pending_lift = Some(ts);
                }
                let pl = self.pending_lift.expect("pending");
                if ts - pl < BRIDGE_MS {
                    return;
                }
                self.end_session(pl);
                self.pending_lift = None;
            } else if self.session_open {
                self.end_session(ts);
            }
            return;
        }
        self.pending_lift = None;
        if !self.session_open {
            self.session_open = true;
            self.session_t0 = ts;
            self.session_travel = 0.0;
            self.session_clicked = false;
            self.session_scrolled = false;
            self.max_fingers = 0;
            self.gesture_fired = false;
            self.two_key = None;
            self.primary = None;
            self.stop_coast("touch");
        }
        self.max_fingers = self.max_fingers.max(fingers.len());
        let Some(key) = self.pick(&fingers, ts) else {
            if self.scrolling {
                self.why = format!("scroll end: {}", self.why);
                self.end_scroll(ts);
            }
            self.primary = None; // everything is resting: nothing to follow
            self.t_ref = None;
            return;
        };
        if key.len() == 2 {
            self.two_fingers(&key);
        }
        let n = key.len() as f64;
        let x = key.iter().map(|&t| self.t(t).x).sum::<i64>() as f64 / n;
        let y = key.iter().map(|&t| self.t(t).y).sum::<i64>() as f64 / n;
        self.follow(ts, key, x, y);
    }

    /// "not touching yet (size N)" / "only grip fingers" / "no touch"
    fn idle_why(&self) -> String {
        let light: Vec<i64> = self.touches.values().filter(|t| !t.grip).map(|t| t.size).collect();
        if let Some(m) = light.iter().max() {
            format!("not touching yet (size {m})")
        } else if !self.touches.is_empty() {
            "only grip fingers".into()
        } else {
            "no touch".into()
        }
    }

    /// Touchpad mode: hand the followed finger(s) to the virtual touchpad, plus one.
    fn touchpad_frame(&mut self, ts: f64, fingers: &[i64]) {
        let gain = self.cfg.f("touchpad", "swipe_gain");
        let swipe_on = is_true(&self.cfg, "touchpad", "swipe", true);
        let mut key = self.tp_key.clone();
        if let Some(k) = key.clone() {
            let mut keep = Vec::new();
            for &t in &k {
                if fingers.contains(&t) {
                    self.tp_light.remove(t);
                    keep.push(t);
                } else if self.touches.get(t).is_some_and(|o| !o.grip)
                    && ts - *self.tp_light.setdefault(t, ts) < TP_HOLD_SEC
                {
                    keep.push(t); // lighter for a moment while it slides: still the same touch
                }
            }
            if k.len() >= 2 && keep.len() < k.len() {
                keep.clear(); // one finger of a swipe lifted: the swipe is over
                self.tp_swiped = true;
                let ptr_total = self.ptr_total;
                for &t in &k {
                    if let Some(o) = self.touches.get_mut(t) {
                        if o.edge0 {
                            o.grip = true; // back to holding the mouse, as before the swipe
                            o.x0 = o.x;
                            o.y0 = o.y;
                            o.path = 0.0;
                            o.ptr0 = ptr_total;
                        }
                    }
                }
            }
            key = if keep.is_empty() { None } else { Some(keep) };
        }
        if fingers.is_empty() {
            self.tp_swiped = false;
        }
        let mut cand = if !fingers.is_empty() { self.pick(fingers, ts) } else { None };
        let pair = if swipe_on && !self.tp_swiped { self.fw_pair() } else { None };
        let pair_ok = match (&pair, &key) {
            (Some(_), None) => true,
            (Some(p), Some(k)) => k.len() == 1 && p.contains(&k[0]) && ts - self.tp_t0 < TP_PAIR_YOUNG,
            _ => false,
        };
        if pair_ok && self.tp_engage(ts, pair.as_ref().expect("pair")) {
            // The mouse says both touch and they slide sideways side by side: the swipe, at once
            let down: Vec<i64> = key.clone().unwrap_or_default();
            let p = pair.expect("pair");
            self.tp_pair_n = 0;
            self.tp_t0 = ts;
            for &t in &p {
                self.t_mut(t).grip = false;
                if !down.contains(&t) {
                    let (rx, ry) = self.recent_of(t);
                    self.tp_owed.insert(t, [-rx, -ry]);
                    self.tp_out.remove(t);
                }
            }
            key = Some(p);
        } else if let Some(k) = key.clone() {
            if k.len() == 1 {
                let t1 = self.t(k[0]);
                let dx = t1.x as f64 - self.tp_from.0;
                let dy = t1.y as f64 - self.tp_from.1;
                let sideways = dx.abs() >= TP_SWIPE_RATIO * dy.abs();
                cand = if swipe_on && (sideways || ts - self.tp_t0 < TP_PAIR_YOUNG) {
                    self.swipe_partner(k[0])
                } else {
                    None
                };
            }
            if k.len() == 1 && cand.is_some() {
                self.tp_pair_n += 1; // a second finger goes sideways along: a swipe after all
                if self.tp_pair_n >= TP_PAIR_FRAMES {
                    let c = cand.clone().expect("cand");
                    self.tp_pair_n = 0;
                    for &t in &c {
                        self.t_mut(t).grip = false;
                    }
                    self.tp_owed.clear();
                    key = Some(c);
                }
            } else {
                self.tp_pair_n = 0;
            }
        } else if self.tp_swiped {
            self.why = "swipe done: lift all fingers".into();
        } else if let Some(mut c) = cand.clone() {
            if c.len() == 2 && !(swipe_on && self.swipe_pair(&c)) {
                // up and down (or swipe off), the fingers scroll together: one is enough
                let mut best = c[0];
                let mut bm = hypot_recent(self.t(c[0]));
                for &t in &c[1..] {
                    let m = hypot_recent(self.t(t));
                    if m > bm {
                        bm = m;
                        best = t;
                    }
                }
                c = vec![best];
            }
            if c.len() == 1 && swipe_on {
                if let Some(p) = self.swipe_partner(c[0]) {
                    c = p; // sideways with a finger from the edge
                }
            }
            if self.tp_engage(ts, &c) {
                self.tp_pair_n = 0;
                self.tp_t0 = ts;
                for &t in &c {
                    self.t_mut(t).grip = false; // an edge finger in a swipe slides: it is a finger now
                }
                if c.len() == 1 {
                    // land where the slide began: a short flick counts from its start
                    let t = self.t(c[0]);
                    let (rx, ry) = t.recent();
                    self.tp_from = ((t.x - rx) as f64, (t.y - ry) as f64);
                    self.tp_owed = OMap::new();
                    self.tp_owed.insert(c[0], [-(rx as f64), -(ry as f64)]);
                }
                key = Some(c);
            }
        }
        self.tp_key = key.clone();
        self.primary = key.clone();
        let kk = key.clone().unwrap_or_default();
        self.tp_light.retain(|t| kk.contains(&t));
        self.tp_owed.retain(|t| kk.contains(&t));
        self.tp_out.retain(|t| kk.contains(&t));
        let Some(key) = key else {
            if fingers.is_empty() {
                self.why = self.idle_why();
            }
            if let Some(vp) = self.vpad.as_mut() {
                vp.update(&[], gain, false);
            }
            return;
        };
        let mut pos: Vec<(i32, f64, f64)> = Vec::new();
        for &t in &key {
            let [ox, oy] = self.tp_owed.get(t).copied().unwrap_or([0.0, 0.0]);
            let (tx, ty) = (self.t(t).x as f64, self.t(t).y as f64);
            let (px, py) = self.with_play(t, tx + ox, ty + oy);
            pos.push((t as i32, px, py));
            if self.tp_owed.contains_key(t) {
                // catch up over a few frames, not in one jump
                let (ox, oy) = (ox * TP_CATCH_UP, oy * TP_CATCH_UP);
                self.tp_owed.insert(t, [ox, oy]);
                if hypot(ox, oy) < 1.0 {
                    self.tp_owed.remove(t);
                }
            }
        }
        let mirror = is_true(&self.cfg, "touchpad", "swipe_reverse", false);
        if let Some(vp) = self.vpad.as_mut() {
            vp.update(&pos, gain, mirror);
        }
        let n = key.len();
        self.why = format!(
            "touchpad: {} finger{} -> {}{}{}",
            n,
            s_if(n),
            n + 1,
            if n >= 2 { " (swipe)" } else { " (scroll)" },
            if !self.tp_light.is_empty() { " held" } else { "" }
        );
    }

    /// Where finger t is on the virtual touchpad: it follows the real one with touch_reverse_mm
    /// of play on each axis (backlash).
    fn with_play(&mut self, t: i64, x: f64, y: f64) -> (f64, f64) {
        let play = pmax(get_f(&self.cfg, "scroll", "touch_reverse_mm", 0.0), 0.0) * self.res_x;
        if !self.tp_out.contains_key(t) {
            let (rx, ry) = self.recent_of(t);
            let ox = if rx != 0.0 { x - play.copysign(rx) } else { x };
            let oy = if ry != 0.0 { y - play.copysign(ry) } else { y };
            self.tp_out.insert(t, [ox, oy]);
        }
        let out = self.tp_out.get_mut(t).expect("tp_out");
        for (i, v) in [x, y].into_iter().enumerate() {
            if v > out[i] + play {
                out[i] = v - play;
            } else if v < out[i] - play {
                out[i] = v + play;
            }
        }
        (out[0], out[1])
    }

    /// t1 and another confirmed finger sliding sideways the same way, as a sorted pair.
    fn swipe_partner(&self, t1: i64) -> Option<Key> {
        let (ax, ay) = self.recent_of(t1);
        if ax.abs() < TP_SWIPE_RATIO * ay.abs() || ax.abs() / self.res_x < PAIR_MOVING_MM {
            return None;
        }
        let mut best: Option<(f64, i64)> = None;
        for (t, o) in self.touches.iter() {
            if t == t1 || o.palm || !o.ok || self.wobbling(o) {
                continue;
            }
            let (bx, by) = o.recent();
            let (bx, by) = (bx as f64, by as f64);
            let m = bx.abs() / self.res_x;
            if bx * ax > 0.0
                && m >= MOVING_MM
                && bx.abs() >= TP_SWIPE_RATIO * by.abs()
                && m >= PAIR_RATIO * ax.abs() / self.res_x
                && best.is_none_or(|b| m > b.0)
            {
                best = Some((m, t));
            }
        }
        best.map(|b| sorted2(t1, b.1))
    }

    /// Two fingers the mouse itself calls touching, side by side, sliding sideways together.
    pub fn fw_pair(&self) -> Option<Key> {
        let live: Vec<(i64, &Touch)> =
            self.touches.iter().filter(|(_, o)| FW_TOUCHING.contains(&o.fw) && !o.palm).collect();
        let mut best: Option<(f64, Key)> = None;
        for (i, &(ta, a)) in live.iter().enumerate() {
            let (ax, ay) = a.recent();
            for &(tb, b) in &live[i + 1..] {
                let (bx, by) = b.recent();
                let ma = ax.abs() as f64 / self.res_x;
                let mb = bx.abs() as f64 / self.res_x;
                if ax * bx <= 0
                    || pmin(ma, mb) < MOVING_MM
                    || pmax(ma, mb) < MIN_SLIDE_MM
                    || pmin(ma, mb) < PAIR_RATIO * pmax(ma, mb)
                {
                    continue; // not both moving the same way, enough
                }
                if ((ax + bx).abs() as f64) < TP_SWIPE_RATIO * (ay + by).abs() as f64 {
                    continue; // more up and down than sideways: a scroll
                }
                if hypot((a.x - b.x) as f64, (a.y - b.y) as f64) / self.res_x > FW_PAIR_GAP_MM {
                    continue;
                }
                if best.as_ref().is_none_or(|b| ma + mb > b.0) {
                    best = Some((ma + mb, sorted2(ta, tb)));
                }
            }
        }
        best.map(|b| b.1)
    }

    /// Two fingers sliding sideways together: the workspace swipe, not a scroll.
    fn swipe_pair(&self, pair: &[i64]) -> bool {
        let vx = pair.iter().map(|&t| self.t(t).recent().0).sum::<i64>() as f64 / self.res_x;
        let vy = pair.iter().map(|&t| self.t(t).recent().1).sum::<i64>() as f64 / self.res_y;
        vx.abs() > 0.0 && vx.abs() >= TP_SWIPE_RATIO * vy.abs()
    }

    /// May these fingers go down on the virtual touchpad?
    fn tp_engage(&mut self, ts: f64, key: &[i64]) -> bool {
        if !self.pressed.is_empty() || ts - self.last_button_t < CLICK_QUIET {
            self.why = "button pressed or click pause".into();
            return false;
        }
        let slide = self.key_recent_mm(key);
        if slide < (if key.len() >= 2 { MIN_SLIDE_MM } else { MOVING_MM }) {
            self.why = if slide < MOVING_MM { "fingers resting" } else { "two fingers creeping" }.into();
            return false;
        }
        if self.pointer_moving(ts) && (slide < MIN_SLIDE_MM || slide < POINTER_RATIO * self.pointer_mm(ts, MOVE_WINDOW)) {
            self.why = "mouse moving: finger slide too small".into();
            return false;
        }
        if key.iter().any(|&t| self.wobbling(self.t(t))) {
            self.why = "finger wobbling on the spot (hover?)".into();
            return false;
        }
        true
    }

    /// Moving, but back and forth rather than one way: a finger hovering just above.
    fn wobbling(&self, t: &Touch) -> bool {
        let straight = get_f(&self.cfg, "scroll", "touch_straight", 0.0);
        straight > 0.0 && t.straightness() < straight
    }

    /// Hysteresis on contact size, or the mouse's own touch state where it tells one.
    fn confirm_touches(&mut self, ts: f64, reports: &OMap<i64, i64>, fw: &OMap<i64, i64>) {
        let empty = OMap::new();
        let fw = if !fw.is_empty() && get_b(&self.cfg, "scroll", "touch_firmware", true) { fw } else { &empty };
        if fw.is_empty() {
            for t in self.touches.values_mut() {
                t.fw = -1;
            }
        }
        let start = pmax(get_f(&self.cfg, "scroll", "touch_start_size", 0.0), 0.0);
        let hold = pmin(pmax(get_f(&self.cfg, "scroll", "touch_hold_size", 0.0), 0.0), start);
        let pair = pmin(pmax(get_f(&self.cfg, "scroll", "touch_pair_size", 0.0), 0.0), start);
        let pair = if pair != 0.0 { pair } else { hold };
        let rate = get_f(&self.cfg, "scroll", "touch_rate", 0.0);
        let max_gap = if rate > 0.0 { 1.0 / rate } else { f64::INFINITY };
        let mut gaps: OMap<i64, f64> = OMap::new();
        for (tid, &size) in reports.iter() {
            let Some(t) = self.touches.get_mut(tid) else {
                continue;
            };
            let gap = match t.last_rep {
                Some(l) => ts - l,
                None => 0.0,
            };
            t.last_rep = Some(ts);
            t.size = size;
            if gap > 0.0 {
                t.rate = if t.rate == 0.0 { 1.0 / gap } else { 0.8 * t.rate + 0.2 / gap };
            }
            gaps.insert(tid, gap);
        }
        let present: Vec<&Touch> = self
            .touches
            .values()
            .filter(|t| !t.grip && t.last_rep.is_some_and(|l| ts - l <= STALE_SEC / 3.0))
            .collect();
        let two = present.iter().filter(|t| t.size as f64 >= pair).count() >= 2;
        let mut anchor = present.iter().any(|t| t.ok);
        for (tid, &gap) in gaps.iter() {
            if let Some(&f) = fw.get(tid) {
                let t = self.t_mut(tid);
                let was = t.ok;
                t.firm = 0;
                t.fw = f;
                t.ok = FW_TOUCHING.contains(&f);
                if t.ok && !was && !t.grip {
                    anchor = true;
                    self.stop_coast("touch");
                }
                continue;
            }
            let start_size = self.start_size(self.t(tid), start);
            let t = self.t_mut(tid);
            let size = t.size as f64;
            if t.ok {
                if size < hold {
                    t.ok = false;
                    t.firm = 0;
                }
                continue;
            }
            if start <= 0.0 || (!t.grip && (anchor || two) && size >= pmin(hold, pair)) {
                t.ok = true; // off, or one of two fingers
            } else if size >= start_size {
                t.firm = if gap <= max_gap { t.firm + 1 } else { 1 };
                t.ok = t.firm >= FIRM_REPORTS;
            } else {
                t.firm = 0;
            }
            if t.ok && !t.grip {
                anchor = true;
                self.stop_coast("touch");
            }
        }
    }

    /// The size a single finger needs to start where it is.
    pub fn start_size(&self, t: &Touch, start: f64) -> f64 {
        self.start_size_at((t.y - self.y_min) as f64, start)
    }

    /// start_size() for a finger `dy` touch units from the front (t.y - y_min).
    fn start_size_at(&self, dy: f64, start: f64) -> f64 {
        let front = get_f(&self.cfg, "scroll", "touch_front_size", 0.0);
        let depth = get_f(&self.cfg, "scroll", "touch_front_mm", 0.0);
        if front <= start || depth <= 0.0 {
            return start;
        }
        let mm_in = dy / self.res_y;
        let share = pmin(pmax((2.0 * depth - mm_in) / depth, 0.0), 1.0);
        start + (front - start) * share
    }

    /// A finger that landed in the edge zone slides on the surface (not just with the hand).
    fn edge_finger_slides(&self, ts: f64, t: &Touch) -> bool {
        let travel = hypot((t.x - t.x0) as f64, (t.y - t.y0) as f64) / self.res_x;
        let hand_mm = (self.ptr_total - t.ptr0) * 25.4 / pmax(self.cfg.f("pointer", "cpi"), 1.0);
        if travel < self.cfg.f("scroll", "edge_release_mm") + POINTER_RATIO * hand_mm {
            return false;
        }
        if travel < EDGE_STRAIGHT * t.path / self.res_x {
            return false;
        }
        let (rx, ry) = t.recent();
        let slide = hypot(rx as f64, ry as f64) / self.res_x;
        slide >= MIN_SLIDE_MM && slide > POINTER_RATIO * self.pointer_mm(ts, MOVE_WINDOW)
    }

    fn down(&self, key: &Option<Key>) -> bool {
        key.as_ref().is_some_and(|k| !k.is_empty() && k.iter().all(|t| self.touches.contains_key(*t)))
    }

    /// The finger(s) to follow.
    fn pick(&mut self, fingers: &[i64], ts: f64) -> Option<Key> {
        if self.scrolling && self.down(&self.primary) {
            let primary = self.primary.clone().expect("primary");
            if primary.len() == 1 && fingers.len() >= 2 {
                if let Some(pair) = self.partner(fingers, primary[0]) {
                    let sideways = if self.axis == "xy" { self.s_travel_x } else { self.s_travel };
                    if self.start_axis == "x"
                        && ts - self.scroll_t0 < 0.25
                        && sideways.abs() < self.cfg.f("gestures", "swipe_mm")
                    {
                        // a young sideways scroll that turns out to be two fingers is the workspace swipe
                        self.scrolling = false;
                        self.two_key = None;
                        self.why = "second finger joined: two-finger swipe".into();
                    }
                    return Some(pair);
                }
            }
            if primary.len() == 2 {
                let mut ms: Vec<(f64, i64)> =
                    primary.iter().map(|&t| (hypot_recent(self.t(t)) / self.res_x, t)).collect();
                sort_desc(&mut ms);
                let ((ma, a), (mb, _b)) = (ms[0], ms[1]);
                if mb < PAIR_MOVING_MM / 2.0 && ma >= MOVING_MM {
                    return Some(vec![a]); // the partner came to rest: follow the finger that still moves
                }
            }
            return Some(primary);
        }
        if self.scrolling && fingers.len() == 1 {
            return Some(vec![fingers[0]]);
        }
        if fingers.len() == 1 {
            return Some(vec![fingers[0]]);
        }
        let mut moves: Vec<(f64, i64)> = fingers.iter().map(|&t| (hypot_recent(self.t(t)) / self.res_x, t)).collect();
        sort_desc(&mut moves);
        let ((m1, t1), (m2, _t2)) = (moves[0], moves[1]);
        if m1 < MOVING_MM {
            self.why = "all fingers resting".into();
            return if self.down(&self.primary) { self.primary.clone() } else { None };
        }
        if let Some(pair) = self.partner(fingers, t1) {
            return Some(pair);
        }
        if m2 >= MOVING_MM && m2 >= 0.5 * m1 {
            self.why = "two fingers moving apart (pinch)".into();
            return None;
        }
        Some(vec![t1])
    }

    /// The finger moving along with t1, if any, as a sorted pair.
    fn partner(&self, fingers: &[i64], t1: i64) -> Option<Key> {
        let (ax, ay) = self.t(t1).recent();
        let m1 = hypot(ax as f64, ay as f64) / self.res_x;
        let mut best: Option<(f64, i64)> = None;
        for &t in fingers {
            if t == t1 {
                continue;
            }
            let (bx, by) = self.t(t).recent();
            let m = hypot(bx as f64, by as f64) / self.res_x;
            if m >= PAIR_MOVING_MM && m >= PAIR_RATIO * m1 && ax * bx + ay * by > 0 && best.is_none_or(|b| m > b.0) {
                best = Some((m, t));
            }
        }
        best.map(|b| sorted2(t1, b.1))
    }

    fn key_recent_mm(&self, key: &[i64]) -> f64 {
        let n = key.len() as f64;
        let vx = key.iter().map(|&t| self.t(t).recent().0).sum::<i64>() as f64 / n / self.res_x;
        let vy = key.iter().map(|&t| self.t(t).recent().1).sum::<i64>() as f64 / n / self.res_y;
        hypot(vx, vy)
    }

    fn two_fingers(&mut self, pair: &[i64]) {
        let cx = pair.iter().map(|&t| self.t(t).x).sum::<i64>() as f64 / 2.0;
        let cy = pair.iter().map(|&t| self.t(t).y).sum::<i64>() as f64 / 2.0;
        if self.two_key.as_deref() != Some(pair) {
            self.two_key = Some(pair.to_vec());
            self.two_ref = Some((cx, cy));
            return;
        }
        let (rx, ry) = self.two_ref.expect("two_ref");
        let dx_mm = (cx - rx) / self.res_x;
        let dy_mm = (cy - ry) / self.res_y;
        self.session_travel = pmax(self.session_travel, hypot(dx_mm, dy_mm));
        if self.gesture_fired || self.scrolling {
            return; // a two-finger scroll that drifts sideways is not a workspace swipe
        }
        if dx_mm.abs() >= self.cfg.f("gestures", "swipe_mm") && dx_mm.abs() > HORIZONTAL_RATIO * dy_mm.abs() {
            self.gesture_fired = true;
            let invert = get_b(&self.cfg, "gestures", "invert_swipe", false);
            let mut left = dx_mm < 0.0;
            if invert {
                left = !left;
            }
            let action = self.cfg.s("gestures", if left { "two_finger_swipe_left" } else { "two_finger_swipe_right" });
            let dir = if dx_mm < 0.0 { "left" } else { "right" };
            if self.verbose {
                log(&format!("two-finger swipe {dir}"));
            }
            self.why = format!("two-finger swipe {dir}{}", if invert { " (inverted)" } else { "" });
            self.why_locked = true;
            let mut end = if raw(&self.cfg, "gestures", "swipe_end").is_some() {
                self.cfg.s("gestures", "swipe_end")
            } else {
                "wrap".to_string()
            };
            if !["wrap", "new", "stop"].contains(&end.as_str()) {
                end = "wrap".into();
            }
            if action.contains(WORKSPACE_NEXT) || action.contains(WORKSPACE_PREV) {
                // the default actions: our own stepping
                let include_empty = get_b(&self.cfg, "gestures", "include_empty", true);
                self.actions.workspace_step(action.contains(WORKSPACE_NEXT), &end, &action, include_empty);
            } else {
                self.actions.run_action(&action);
            }
        }
    }

    fn follow(&mut self, ts: f64, key: Key, x: f64, y: f64) {
        if Some(&key) != self.primary.as_ref() || self.t_ref.is_none() {
            // a finger landed or started moving (or the pair changed): re-anchor
            if self.scrolling && self.verbose && Some(&key) != self.primary.as_ref() {
                log(&format!("scroll handoff {} -> {}", tup(&self.primary), tup(&Some(key.clone()))));
            }
            self.primary = Some(key);
            self.t_ref = Some((x, y));
            if !self.scrolling {
                self.s_vel = 0.0;
                self.why = "finger landed or changed: anchor".into();
            }
            self.s_last_t = ts;
            return;
        }

        let (rx, ry) = self.t_ref.expect("t_ref");
        let mut dx_mm = (x - rx) / self.res_x;
        let mut dy_mm = (y - ry) / self.res_y;
        self.session_travel += hypot(dx_mm, dy_mm);
        let n = key.len();

        if !self.scrolling {
            if !self.pressed.is_empty() || ts - self.last_button_t < CLICK_QUIET {
                self.why = "button pressed or click pause".into();
                self.t_ref = Some((x, y));
                return;
            }
            let start_mm = self.cfg.f("scroll", "start_mm");
            if self.pointer_moving(ts) {
                let nf = n as f64;
                let vx = key.iter().map(|&t| self.t(t).recent().0).sum::<i64>() as f64 / nf / self.res_x;
                let vy = key.iter().map(|&t| self.t(t).recent().1).sum::<i64>() as f64 / nf / self.res_y;
                let slide = hypot(vx, vy);
                if slide < pmax(start_mm, MIN_SLIDE_MM) || slide < POINTER_RATIO * self.pointer_mm(ts, MOVE_WINDOW) {
                    self.why = "mouse moving: finger slide too small".into();
                    self.t_ref = Some((x, y));
                    return;
                }
                dx_mm = vx;
                dy_mm = vy;
            } else if hypot(dx_mm, dy_mm) < start_mm {
                self.why = "below start distance".into();
                return;
            } else if self.key_recent_mm(&key) < MOVING_MM {
                self.why = "finger only creeping".into();
                return;
            }
            self.axis = if dx_mm.abs() > HORIZONTAL_RATIO * dy_mm.abs() { "x" } else { "y" };
            let horizontal = self.cfg.b("scroll", "horizontal");
            if self.axis == "x" && (!horizontal || n == 2) {
                if !self.why_locked {
                    self.why = if n == 2 { "sideways with two fingers (gesture)" } else { "horizontal scroll off" }.into();
                }
                self.t_ref = Some((x, y));
                return;
            }
            self.start_axis = self.axis;
            if self.cfg.b("scroll", "free_axis") && horizontal {
                self.axis = "xy";
            }
            self.scrolling = true;
            self.session_scrolled = true;
            self.scroll_t0 = ts;
            self.s_travel = 0.0;
            self.s_travel_x = 0.0;
            self.t_ref = Some((x, y)); // the start travel is swallowed, like macOS
            self.s_vel = 0.0;
            self.s_speed = 0.0;
            self.s_rem = [0.0, 0.0];
            self.s_notch = [0, 0];
            self.s_vel_xy = [0.0, 0.0];
            self.s_dir = [0.0, 0.0];
            self.s_last_t = ts;
            self.why = format!("scroll start ({n} finger{}, {})", s_if(n), self.axis);
            if self.verbose {
                log(&format!("scroll start axis={} fingers={n}", self.axis));
            }
            return;
        }

        // scrolling
        self.t_ref = Some((x, y));
        let pointer = self.pointer_mm(ts, MOVE_WINDOW);
        if pointer >= MIN_SLIDE_MM && pointer > self.key_recent_mm(&key) {
            // the mouse moves farther than the finger slides: mousing now. Stop, no glide.
            self.scrolling = false;
            self.why = "scroll stop: mouse moves more than finger".into();
            if self.verbose {
                log("scroll stop (pointer moving)");
            }
            return;
        }
        self.why = format!("scrolling ({n} finger{}, {})", s_if(n), self.axis);
        let d_mm;
        if self.axis == "xy" {
            let k_dir = 1.0 - (-(pmax(ts - self.s_last_t, 0.0) * 1000.0) / FREE_DIR_MS).exp();
            self.s_dir[0] += k_dir * (dx_mm - self.s_dir[0]);
            self.s_dir[1] += k_dir * (dy_mm - self.s_dir[1]);
            let big = pmax(self.s_dir[0].abs(), self.s_dir[1].abs());
            if self.s_dir[0].abs() < FREE_MINOR * big {
                dx_mm = 0.0;
            }
            if self.s_dir[1].abs() < FREE_MINOR * big {
                dy_mm = 0.0;
            }
            d_mm = hypot(dx_mm, dy_mm);
            self.s_travel_x += dx_mm;
        } else {
            d_mm = if self.axis == "y" { dy_mm } else { dx_mm };
        }
        self.s_travel += d_mm;
        let dt = pmax(ts - self.s_last_t, 0.0);
        self.s_last_t = ts;
        let k_speed = 1.0 - (-(dt * 1000.0) / SCROLL_SPEED_MS).exp();
        let k_vel = 1.0 - (-(dt * 1000.0) / SCROLL_VEL_MS).exp();
        self.s_speed += k_speed * ((if dt > 0.0 { d_mm.abs() / dt } else { 0.0 }) - self.s_speed);
        let ramp = 1.0 + self.cfg.f("scroll", "accel") * pmin(pmax((self.s_speed - 75.0) / 370.0, 0.0), 2.0);
        let gain = HIRES_PER_MM * self.cfg.f("scroll", "speed") * ramp;
        if self.axis == "xy" {
            let hx = self.scroll_sign("x") * dx_mm * gain;
            let hy = self.scroll_sign("y") * dy_mm * gain;
            self.emit_scroll(hx, "x");
            self.emit_scroll(hy, "y");
            for (i, h) in [hx, hy].into_iter().enumerate() {
                self.s_vel_xy[i] += k_vel * ((if dt > 0.0 { h / dt } else { 0.0 }) - self.s_vel_xy[i]);
            }
            self.s_vel = hypot(self.s_vel_xy[0], self.s_vel_xy[1]);
            return;
        }
        let hires = self.scroll_sign(self.axis) * d_mm * gain;
        self.emit_scroll(hires, self.axis);
        self.s_vel += k_vel * ((if dt > 0.0 { hires / dt } else { 0.0 }) - self.s_vel);
    }

    /// Map finger travel (touch units) to wheel sign.
    fn scroll_sign(&self, axis: &str) -> f64 {
        let mut sign;
        if axis == "y" {
            sign = -1.0;
            if self.cfg.b("scroll", "invert_y") {
                sign = -sign;
            }
        } else {
            sign = 1.0;
            if self.cfg.b("scroll", "invert_x") {
                sign = -sign;
            }
        }
        if self.cfg.b("scroll", "natural") {
            sign = -sign;
        }
        sign
    }

    fn end_scroll(&mut self, ts: f64) {
        // the lift report comes one report after the last move; only a longer gap means the finger had stopped
        let fade = (-pmax(ts - self.s_last_t - NOMINAL_DT, 0.0) * 1000.0 / SCROLL_VEL_MS).exp();
        self.s_vel *= fade;
        self.s_vel_xy = [self.s_vel_xy[0] * fade, self.s_vel_xy[1] * fade];
        let sideways = if self.axis == "xy" { self.s_travel_x } else { self.s_travel };
        let swipe = (self.axis == "x" || (self.axis == "xy" && self.start_axis == "x"))
            && self.cfg.b("gestures", "swipe_pages")
            && (ts - self.scroll_t0) < 0.4
            && sideways.abs() >= self.cfg.f("gestures", "swipe_mm");
        if swipe {
            self.nav(sideways > 0.0); // finger moving right = back, like macOS
        } else if self.cfg.b("momentum", "enabled")
            && self.s_vel != 0.0
            && self.s_vel.abs() >= self.cfg.f("momentum", "flick")
        {
            self.coasting = true;
            self.c_vel = self.s_vel;
            if self.axis == "xy" {
                let [vx, vy] = self.s_vel_xy;
                self.c_dir = (vx / self.s_vel, vy / self.s_vel);
            }
            self.c_t0 = ts;
            self.c_last_t = ts;
            self.s_rem = [0.0, 0.0];
            if self.verbose {
                log(&format!("coast start {:.0} hires/s axis={}", self.c_vel, self.axis));
            }
        }
        self.scrolling = false;
        self.primary = None;
        self.t_ref = None;
    }

    fn end_session(&mut self, ts: f64) {
        let dur = ts - self.session_t0;
        if self.scrolling {
            self.end_scroll(ts);
        } else if !self.session_clicked
            && !self.gesture_fired
            && !self.session_scrolled
            && self.session_travel < 2.0
            && dur < 0.3
        {
            // a tap (touch without click or travel); two in a row is a double tap
            if self.last_tap_t >= 0.0 && ts - self.last_tap_t < 0.4 && self.last_tap_fingers == self.max_fingers {
                self.last_tap_t = -1.0;
                let action =
                    self.cfg.s("gestures", if self.max_fingers >= 2 { "two_finger_double_tap" } else { "double_tap" });
                if self.verbose {
                    log(&format!("double tap ({} finger)", self.max_fingers));
                }
                self.actions.run_action(&action);
            } else {
                self.last_tap_t = ts;
                self.last_tap_fingers = self.max_fingers;
            }
        }
        self.scrolling = false;
        self.t_ref = None;
        self.primary = None;
        self.session_open = false;
    }

    // ---- momentum
    pub fn stop_coast(&mut self, why: &str) {
        if self.coasting && self.verbose {
            log(&format!("coast stop ({why})"));
        }
        self.coasting = false;
        self.c_vel = 0.0;
    }

    pub fn tick(&mut self, now: f64) {
        if !self.coasting {
            return;
        }
        if now - self.c_t0 > self.cfg.f("momentum", "max_seconds") || self.c_vel.abs() < self.cfg.f("momentum", "stop") {
            self.stop_coast("decay");
            return;
        }
        let dt = pmin(pmax(now - self.c_last_t, 0.0), 0.05);
        self.c_last_t = now;
        if dt == 0.0 {
            return;
        }
        if self.axis == "xy" {
            self.emit_scroll(self.c_vel * dt * self.c_dir.0, "x");
            self.emit_scroll(self.c_vel * dt * self.c_dir.1, "y");
        } else {
            self.emit_scroll(self.c_vel * dt, self.axis);
        }
        self.ui.syn();
        self.c_vel *= ppow(self.cfg.f("momentum", "decay"), dt / TICK);
    }
}

fn hypot_recent(t: &Touch) -> f64 {
    let (x, y) = t.recent();
    hypot(x as f64, y as f64)
}

fn sorted2(a: i64, b: i64) -> Key {
    if a <= b {
        vec![a, b]
    } else {
        vec![b, a]
    }
}

/// `sorted(..., reverse=True)` over (float, id) tuples.
fn sort_desc(v: &mut [(f64, i64)]) {
    v.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(b.1.cmp(&a.1)));
}

#[cfg(test)]
mod tests {
    //! Port of tests/test_touch_tracking.py (the Engine / Touch / VPad-call cases).
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    const SLOT: u16 = ABS_MT_SLOT;
    const TID: u16 = ABS_MT_TRACKING_ID;
    const X: u16 = ABS_MT_POSITION_X;
    const Y: u16 = ABS_MT_POSITION_Y;
    const MAJOR: u16 = ABS_MT_TOUCH_MAJOR;
    const MINOR: u16 = ABS_MT_TOUCH_MINOR;
    /// EV_MSC/MSC_RAW: the firmware state byte, state << 4
    const RAW: u16 = 0xffff;

    /// DEFAULTS (bin/magic-mouse-daemon l.147-250), for the tests.
    const DEFAULTS: &str = r#"
[pointer]
tracking_speed = 0.6875
cpi = 1300
speed = 1.0
smoothing_ms = 30.0
click_hold_ms = 80.0
click_drag_units = 12.0
[scroll]
natural = true
invert_y = false
invert_x = false
speed = 1.0
start_mm = 0.8
settle_ms = 150.0
settle_counts = 4.0
accel = 0.6
horizontal = true
touch_start_size = 300.0
touch_hold_size = 250.0
touch_pair_size = 250.0
touch_rate = 42.0
touch_front_size = 510.0
touch_front_mm = 13.0
touch_straight = 0.5
touch_reverse_mm = 0.5
touch_firmware = true
free_axis = false
edge_left_mm = 3.0
edge_right_mm = 3.0
edge_front_mm = 2.0
edge_release_mm = 3.0
edge_back_mm = 18.0
[buttons]
middle = "left"
right_click = true
control_click = false
[gestures]
swipe_pages = true
swipe_mm = 8.0
invert_swipe = false
swipe_end = "wrap"
include_empty = true
two_finger_swipe_left = 'dispatch hl.dsp.focus({ workspace = "e+1" })'
two_finger_swipe_right = 'dispatch hl.dsp.focus({ workspace = "e-1" })'
two_finger_double_tap = ""
double_tap = ""
[touchpad]
enabled = true
scroll_speed = 0.35
swipe_gain = 2.7
swipe = true
swipe_reverse = false
[bar]
show_percent = true
[battery]
enabled = true
poll_minutes = 15.0
bluez = true
[momentum]
enabled = true
flick = 900.0
decay = 0.97
stop = 60.0
max_seconds = 2.5
"#;

    fn defaults() -> Config {
        Config(toml::from_str(DEFAULTS).expect("defaults"))
    }

    #[derive(Default)]
    struct Wheel {
        x: i64,
        y: i64,
    }
    struct FakeUI(Rc<RefCell<Wheel>>);
    impl Output for FakeUI {
        fn write(&mut self, kind: u16, code: u16, value: i32) {
            let mut w = self.0.borrow_mut();
            if kind == EV_REL && code == REL_WHEEL_HI_RES {
                w.y += value as i64;
            } else if kind == EV_REL && code == REL_HWHEEL_HI_RES {
                w.x += value as i64;
            }
        }
        fn syn(&mut self) {}
    }
    struct FakeActions(Rc<RefCell<Vec<String>>>);
    impl Actions for FakeActions {
        fn run_action(&mut self, cmd: &str) {
            self.0.borrow_mut().push(cmd.to_string());
        }
        fn workspace_step(&mut self, forward: bool, end: &str, fallback: &str, include_empty: bool) {
            self.0.borrow_mut().push(format!("step {forward} {end} {fallback} {include_empty}"));
        }
    }
    #[derive(Default)]
    struct PadLog {
        calls: Vec<(Vec<i32>, f64)>,
        pos: Vec<Vec<(f64, f64)>>,
        mirror: bool,
    }
    struct FakePad(Rc<RefCell<PadLog>>);
    impl TouchpadOut for FakePad {
        fn update(&mut self, fingers: &[(i32, f64, f64)], gain: f64, mirror: bool) {
            let mut p = self.0.borrow_mut();
            p.calls.push((fingers.iter().map(|f| f.0).collect(), gain));
            p.mirror = mirror;
            p.pos.push(fingers.iter().map(|f| (f.1, f.2)).collect());
        }
    }

    struct Case {
        eng: Engine,
        t: f64,
        wheel: Rc<RefCell<Wheel>>,
        actions: Rc<RefCell<Vec<String>>>,
        pad: Rc<RefCell<PadLog>>,
    }

    impl Case {
        fn new() -> Self {
            let wheel = Rc::new(RefCell::new(Wheel::default()));
            let actions = Rc::new(RefCell::new(Vec::new()));
            let eng = Engine::new(defaults(), false, Box::new(FakeUI(wheel.clone())), Box::new(FakeActions(actions.clone())));
            Case { eng, t: 100.0, wheel, actions, pad: Rc::new(RefCell::new(PadLog::default())) }
        }
        fn touchpad() -> Self {
            let mut c = Case::new();
            c.eng.vpad = Some(Box::new(FakePad(c.pad.clone())));
            c
        }
        fn set(&mut self, sec: &str, key: &str, v: toml::Value) {
            self.eng.cfg.0.get_mut(sec).expect("sec").insert(key.into(), v);
        }
        fn ev(&mut self, kind: u16, code: u16, value: i32) {
            self.eng.feed(Event { ts: self.t, kind, code, value });
        }
        /// One input report: (code, value) ABS_MT events, then SYN_REPORT. A new finger gets a
        /// width (TOUCH_MINOR) unless the frame sets one.
        fn frame(&mut self, events: &[(u16, i32)]) {
            let mut evs = Vec::new();
            let has_minor = events.iter().any(|e| e.0 == MINOR);
            for &(code, value) in events {
                evs.push((code, value));
                if !has_minor && code == TID && value >= 0 {
                    evs.push((MINOR, 400));
                }
            }
            for (code, value) in evs {
                if code == RAW {
                    self.ev(EV_MSC, MSC_RAW, value);
                } else {
                    self.ev(EV_ABS, code, value);
                }
            }
            self.ev(EV_SYN, SYN_REPORT, 0);
            if self.eng.coasting {
                self.eng.tick(self.t);
            }
            self.t += 0.011;
        }
        fn idle(&mut self, n: i64) {
            for _ in 0..n {
                self.frame(&[]);
            }
        }
        fn touch(&self, tid: i64) -> &Touch {
            &self.eng.touches[tid]
        }
        fn calls(&self) -> Vec<Vec<i32>> {
            self.pad.borrow().calls.iter().map(|c| c.0.clone()).collect()
        }
        fn last(&self) -> Vec<i32> {
            self.calls().last().cloned().expect("a pad call")
        }
    }

    fn n(x: f64) -> i64 {
        x as i64 // Python int()
    }

    // ---------------------------------------------------------------- TouchTracking
    #[test]
    fn ghost_slot_from_sync_expires() {
        let mut c = Case::new();
        c.eng.slots.insert(5, [900, 0, 600, 400, 300]);
        c.idle(n(STALE_SEC / 0.011) + 5);
        assert!(!c.eng.slots.contains_key(5));
    }

    #[test]
    fn silent_resting_finger_is_kept_then_restored_as_same_touch() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 1), (TID, 7), (X, 400), (Y, 200), (MAJOR, 600)]);
        c.idle(5);
        assert!(c.eng.touches.contains_key(7));
        c.idle(n(STALE_SEC / 0.011) + 5);
        assert!(!c.eng.touches.contains_key(7));
        let before = c.eng.parked_touch[7].clone();
        c.frame(&[(SLOT, 1), (X, 404)]);
        let t = c.touch(7);
        // the same Touch (Python: assertIs): landing point kept (a new one would land at 404), then updated
        assert_eq!((t.x0, t.y0, t.ok), (before.x0, before.y0, before.ok));
        assert_eq!(t.x, 404);
    }

    #[test]
    fn restore_does_not_stop_a_glide() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 1), (TID, 7), (X, 400), (Y, 1500), (MAJOR, 600)]);
        c.frame(&[(SLOT, 0), (TID, 8), (X, 0), (Y, 1200), (MAJOR, 600)]);
        for i in 0..25 {
            c.frame(&[(SLOT, 0), (Y, 1200 - 60 * i)]);
        }
        c.frame(&[(SLOT, 0), (TID, -1)]);
        c.idle(n(BRIDGE_MS / 0.011) + 2);
        assert!(c.eng.coasting);
        c.idle(n(STALE_SEC / 0.011) - 6);
        assert!(!c.eng.touches.contains_key(7));
        assert!(c.eng.coasting);
        c.frame(&[(SLOT, 1), (X, 402)]);
        assert!(c.eng.coasting);
    }

    #[test]
    fn lift_is_never_restored() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 1), (TID, 7), (X, 400), (Y, 200)]);
        c.idle(n(STALE_SEC / 0.011) + 5);
        c.frame(&[(SLOT, 1), (TID, -1)]);
        c.frame(&[(SLOT, 1), (X, 410)]);
        assert!(!c.eng.touches.contains_key(7));
    }

    #[test]
    fn jump_without_new_id_is_a_new_landing() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 0), (TID, 9), (X, 0), (Y, 0)]);
        assert_eq!(c.touch(9).x0, 0);
        let x = n(JUMP_MM * 1.5 * TOUCH_UNITS_PER_MM) as i32;
        c.frame(&[(SLOT, 0), (X, x)]);
        assert_eq!(c.touch(9).x0, x as i64); // a new Touch, landed here
    }

    #[test]
    fn two_fingers_moving_together_are_followed_as_a_pair() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 0), (TID, 1), (X, -300), (Y, 400), (MAJOR, 600), (SLOT, 1), (TID, 2), (X, 300), (Y, 450), (MAJOR, 600)]);
        for i in 1..30 {
            c.frame(&[(SLOT, 0), (Y, 400 + 30 * i), (SLOT, 1), (Y, 450 + 18 * i)]);
        }
        assert_eq!(c.eng.primary, Some(vec![1, 2]));
        assert!(c.eng.scrolling);
    }

    #[test]
    fn partner_that_stops_is_dropped() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 0), (TID, 1), (X, -300), (Y, 400), (MAJOR, 600), (SLOT, 1), (TID, 2), (X, 300), (Y, 450), (MAJOR, 600)]);
        for i in 1..20 {
            c.frame(&[(SLOT, 0), (Y, 400 + 30 * i), (SLOT, 1), (Y, 450 + 25 * i)]);
        }
        for i in 20..45 {
            c.frame(&[(SLOT, 0), (Y, 400 + 30 * i)]);
        }
        assert_eq!(c.eng.primary, Some(vec![1]));
    }

    // ---------------------------------------------------------------- TouchConfirm
    impl Case {
        fn slide(&mut self, slot: i32, n: i32, size: i32, gap: f64, y0: i32, step: i32) {
            for i in 0..n {
                self.frame(&[(SLOT, slot), (Y, y0 + step * i), (MAJOR, size)]);
                self.t += gap - 0.011;
            }
        }
    }

    #[test]
    fn hover_with_big_but_sparse_reports_does_not_scroll() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600)]);
        c.slide(0, 30, 600, 0.03, 0, 40);
        assert!(!c.touch(1).ok);
        assert_eq!(c.wheel.borrow().y, 0);
    }

    #[test]
    fn small_contact_does_not_scroll() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 200)]);
        c.slide(0, 30, 200, 0.011, 0, 40);
        assert_eq!(c.wheel.borrow().y, 0);
    }

    #[test]
    fn firm_touch_scrolls_and_keeps_scrolling_when_lighter() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600)]);
        c.slide(0, 3, 600, 0.011, 0, 40);
        assert!(c.touch(1).ok);
        c.slide(0, 25, 320, 0.011, 120, 40);
        assert!(c.eng.scrolling);
        assert_ne!(c.wheel.borrow().y, 0);
    }

    #[test]
    fn dropping_below_hold_stops_counting() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600)]);
        c.slide(0, 3, 600, 0.011, 0, 40);
        c.slide(0, 2, 200, 0.011, 120, 40);
        assert!(!c.touch(1).ok);
    }

    #[test]
    fn second_finger_needs_only_the_hold_size() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 0), (TID, 1), (X, -300), (Y, 0), (MAJOR, 600)]);
        c.slide(0, 3, 600, 0.011, 0, 40);
        c.frame(&[(SLOT, 1), (TID, 2), (X, 300), (Y, 0), (MAJOR, 350)]);
        assert!(c.touch(2).ok);
    }

    #[test]
    fn two_light_fingers_count_together() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 0), (TID, 1), (X, -300), (Y, 0), (MAJOR, 280), (SLOT, 1), (TID, 2), (X, 300), (Y, 0), (MAJOR, 280)]);
        assert!(c.touch(1).ok && c.touch(2).ok);
    }

    #[test]
    fn one_light_finger_alone_does_not_count() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 0), (TID, 1), (X, -300), (Y, 0), (MAJOR, 280)]);
        c.slide(0, 10, 280, 0.011, 0, 40);
        assert!(!c.touch(1).ok);
    }

    #[test]
    fn report_without_width_is_not_a_touch() {
        let mut c = Case::new();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 1030), (Y, -1250), (MAJOR, 680), (MINOR, 0)]);
        c.slide(0, 10, 680, 0.011, 0, 40);
        assert!(!c.touch(1).ok);
    }

    #[test]
    fn front_zone_needs_the_front_size() {
        let mut c = Case::new();
        let front = -1589 + 5 * 38;
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, front), (MAJOR, 480)]);
        c.slide(0, 5, 480, 0.011, front, 4);
        assert!(!c.touch(1).ok);
        c.frame(&[(SLOT, 1), (TID, 2), (X, 500), (Y, front), (MAJOR, 650)]);
        c.slide(1, 5, 650, 0.011, front, 4);
        assert!(c.touch(2).ok);
    }

    #[test]
    fn front_size_eases_back_to_the_start_size() {
        let mut c = Case::new();
        let start = 300.0;
        c.set("scroll", "touch_front_size", toml::Value::Integer(540));
        c.set("scroll", "touch_front_mm", toml::Value::Integer(15));
        let at = |e: &Engine, mm: f64| e.start_size_at(mm * e.res_y, start);
        assert_eq!(at(&c.eng, 10.0), 540.0);
        assert!((at(&c.eng, 22.5) - 420.0).abs() < 1e-7);
        assert_eq!(at(&c.eng, 30.0), start);
        assert_eq!(at(&c.eng, 60.0), start);
        c.set("scroll", "touch_front_size", toml::Value::Integer(0));
        assert_eq!(at(&c.eng, 10.0), start);
    }

    #[test]
    fn off_counts_every_touch() {
        let mut c = Case::new();
        c.set("scroll", "touch_start_size", toml::Value::Integer(0));
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 100)]);
        assert!(c.touch(1).ok);
    }

    // ---------------------------------------------------------------- TouchpadMode
    #[test]
    fn one_finger_goes_to_the_touchpad_without_wheel_events() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600)]);
        for i in 1..20 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 600)]);
        }
        assert_eq!(c.last(), vec![1]);
        assert_eq!((c.wheel.borrow().x, c.wheel.borrow().y), (0, 0));
    }

    #[test]
    fn tp_two_fingers_moving_together_go_as_a_pair() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, -300), (Y, 400), (MAJOR, 600), (SLOT, 1), (TID, 2), (X, 300), (Y, 450), (MAJOR, 600)]);
        for i in 1..20 {
            c.frame(&[(SLOT, 0), (X, -300 + 30 * i), (SLOT, 1), (X, 300 + 28 * i)]);
        }
        assert_eq!(c.last(), vec![1, 2]);
        assert!(c.actions.borrow().is_empty());
    }

    #[test]
    fn firmware_hover_never_touches_however_big() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 900), (RAW, 0x20)]);
        for i in 1..20 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 900), (RAW, 0x20)]);
        }
        assert_eq!(c.last(), Vec::<i32>::new());
    }

    #[test]
    fn firmware_touch_counts_however_small() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 100), (RAW, 0x30)]);
        for i in 1..20 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 100), (RAW, 0x40)]);
        }
        assert_eq!(c.last(), vec![1]);
    }

    #[test]
    fn firmware_break_and_linger_lift() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600), (RAW, 0x40)]);
        for i in 1..20 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 600), (RAW, 0x40)]);
        }
        c.frame(&[(SLOT, 0), (Y, 800), (MAJOR, 600), (RAW, 0x50)]);
        for _ in 0..n(TP_HOLD_SEC / 0.011) + 2 {
            c.frame(&[(SLOT, 0), (Y, 800), (MAJOR, 600), (RAW, 0x60)]);
        }
        assert_eq!(c.last(), Vec::<i32>::new());
    }

    #[test]
    fn firmware_state_off_falls_back_to_size() {
        let mut c = Case::touchpad();
        c.set("scroll", "touch_firmware", toml::Value::Boolean(false));
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600), (RAW, 0x20)]);
        for i in 1..20 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 600), (RAW, 0x20)]);
        }
        assert_eq!(c.last(), vec![1]);
    }

    impl Case {
        #[allow(clippy::too_many_arguments)]
        fn light_pair(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, dx: i32, dy: i32, n: i32, state: i32) {
            self.frame(&[(SLOT, 0), (TID, 1), (X, x1), (Y, y1), (MAJOR, 200), (RAW, state),
                (SLOT, 1), (TID, 2), (X, x2), (Y, y2), (MAJOR, 200), (RAW, state)]);
            for i in 1..=n {
                self.frame(&[(SLOT, 0), (X, x1 + dx * i), (Y, y1 + dy * i), (MAJOR, 200), (RAW, state),
                    (SLOT, 1), (X, x2 + dx * i), (Y, y2 + dy * i), (MAJOR, 200), (RAW, state)]);
            }
        }
    }

    #[test]
    fn firmware_light_quick_swipe_on_the_edge_is_a_swipe_at_once() {
        let mut c = Case::touchpad();
        c.light_pair(1200, -200, 1180, 400, -60, 0, 4, 0x40);
        assert_eq!(c.last(), vec![1, 2]);
    }

    #[test]
    fn firmware_pair_owes_the_way_from_landing() {
        let mut c = Case::touchpad();
        c.light_pair(300, -200, 280, 400, -60, 0, 3, 0x40);
        assert!(c.calls().iter().any(|k| k.len() == 2));
        assert!(c.eng.tp_owed.contains_key(1));
    }

    #[test]
    fn firmware_pair_up_and_down_is_no_swipe() {
        let mut c = Case::touchpad();
        c.light_pair(-100, -600, 300, -600, 0, 60, 8, 0x40);
        assert!(c.calls().iter().all(|k| k.len() < 2));
    }

    #[test]
    fn firmware_pair_far_apart_is_no_swipe() {
        let mut c = Case::touchpad();
        c.light_pair(-1080, 400, 1230, 400, 50, 0, 2, 0x40);
        assert_eq!(c.eng.fw_pair(), None);
        assert!(c.calls().iter().all(|k| k.len() < 2));
    }

    #[test]
    fn firmware_hovering_pair_is_no_swipe() {
        let mut c = Case::touchpad();
        c.light_pair(300, -200, 280, 400, -60, 0, 8, 0x20);
        assert_eq!(c.last(), Vec::<i32>::new());
    }

    #[test]
    fn firmware_state_of_a_still_finger_stays_with_it() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 200), (RAW, 0x20),
            (SLOT, 1), (TID, 2), (X, 600), (Y, 600), (MAJOR, 600), (RAW, 0x40)]);
        for i in 1..12 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 200), (RAW, 0x20), (RAW, 0x40)]);
        }
        assert_eq!(c.eng.slot_fw[0], 2);
        assert!(!c.last().contains(&1));
    }

    #[test]
    fn firmware_pair_keeps_the_finger_already_down_in_place() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 300), (Y, -200), (MAJOR, 200), (RAW, 0x40)]);
        for i in 1..4 {
            c.frame(&[(SLOT, 0), (X, 300 - 60 * i), (MAJOR, 200), (RAW, 0x40)]);
        }
        c.eng.tp_t0 = c.t;
        c.frame(&[(SLOT, 0), (X, 60), (MAJOR, 200), (RAW, 0x40), (SLOT, 1), (TID, 2), (X, 280), (Y, 400), (MAJOR, 200), (RAW, 0x40)]);
        for i in 1..4 {
            c.frame(&[(SLOT, 0), (X, 60 - 60 * i), (MAJOR, 200), (RAW, 0x40), (SLOT, 1), (X, 280 - 60 * i), (MAJOR, 200), (RAW, 0x40)]);
        }
        assert_eq!(c.eng.tp_key, Some(vec![1, 2]));
        assert!(!c.eng.tp_owed.contains_key(1));
    }

    #[test]
    fn lift_lifts_the_touchpad() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600)]);
        c.frame(&[(SLOT, 0), (Y, 40)]);
        c.frame(&[(SLOT, 0), (TID, -1)]);
        assert_eq!(c.last(), Vec::<i32>::new());
    }

    // ---------------------------------------------------------------- TouchpadSteadiness
    fn swipe_pair_start(c: &mut Case) {
        c.frame(&[(SLOT, 0), (TID, 1), (X, -300), (Y, 400), (MAJOR, 600), (SLOT, 1), (TID, 2), (X, 300), (Y, 450), (MAJOR, 600)]);
    }

    #[test]
    fn pair_does_not_flap_while_one_finger_slows() {
        let mut c = Case::touchpad();
        swipe_pair_start(&mut c);
        for i in 1..15 {
            c.frame(&[(SLOT, 0), (X, -300 + 40 * i), (SLOT, 1), (X, 300 + 40 * i)]);
        }
        for i in 15..30 {
            c.frame(&[(SLOT, 0), (X, -300 + 40 * i), (SLOT, 1), (X, 300 + 40 * 15 + i)]);
        }
        let counts: Vec<usize> = c.calls().iter().filter(|k| !k.is_empty()).map(|k| k.len()).collect();
        let changes = counts.windows(2).filter(|w| w[0] != w[1]).count();
        assert!(changes <= 1);
    }

    #[test]
    fn second_finger_during_a_scroll_is_ignored() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600)]);
        for i in 1..15 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 600)]);
        }
        for i in 15..40 {
            let mut ev = vec![(SLOT, 0), (Y, 40 * i), (MAJOR, 600)];
            if i == 15 {
                ev.extend([(SLOT, 1), (TID, 2), (X, 500), (Y, 600), (MAJOR, 600)]);
            } else {
                ev.extend([(SLOT, 1), (Y, 600 + 30 * (i - 15))]);
            }
            c.frame(&ev);
        }
        assert!(c.calls().iter().filter(|k| !k.is_empty()).all(|k| *k == vec![1]));
    }

    #[test]
    fn two_fingers_up_and_down_scroll_as_one() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, -300), (Y, 0), (MAJOR, 600), (SLOT, 1), (TID, 2), (X, 300), (Y, 50), (MAJOR, 600)]);
        for i in 1..20 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (SLOT, 1), (Y, 50 + 38 * i)]);
        }
        let down: Vec<Vec<i32>> = c.calls().into_iter().filter(|k| !k.is_empty()).collect();
        assert!(!down.is_empty() && down.iter().all(|k| k.len() == 1));
    }

    #[test]
    fn finger_reporting_light_for_a_moment_stays_down() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600)]);
        for i in 1..10 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 600)]);
        }
        for i in 10..14 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 150)]);
        }
        for i in 14..20 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 600)]);
        }
        let calls = c.calls();
        let first = calls.iter().position(|k| !k.is_empty()).expect("down");
        assert!(calls[first..].iter().all(|k| *k == vec![1]));
    }

    #[test]
    fn finger_left_after_a_swipe_does_not_scroll() {
        let mut c = Case::touchpad();
        swipe_pair_start(&mut c);
        for i in 1..15 {
            c.frame(&[(SLOT, 0), (X, -300 + 40 * i), (SLOT, 1), (X, 300 + 40 * i)]);
        }
        assert_eq!(c.last(), vec![1, 2]);
        c.frame(&[(SLOT, 0), (TID, -1)]);
        for i in 15..30 {
            c.frame(&[(SLOT, 1), (Y, 450 + 40 * (i - 15))]);
        }
        assert_eq!(c.last(), Vec::<i32>::new());
    }

    #[test]
    fn moving_the_mouse_while_scrolling_keeps_the_finger_down() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600)]);
        for i in 1..10 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 600)]);
        }
        for i in 10..25 {
            c.ev(EV_REL, REL_X, 30);
            c.frame(&[(SLOT, 0), (Y, 400 + 5 * (i - 10)), (MAJOR, 600)]);
        }
        let calls = c.calls();
        let first = calls.iter().position(|k| !k.is_empty()).expect("down");
        assert!(calls[first..].iter().all(|k| *k == vec![1]));
    }

    #[test]
    fn short_flick_lands_where_it_began() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600)]);
        for i in 1..16 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 600)]);
        }
        let calls = c.calls();
        let first = calls.iter().position(|k| !k.is_empty()).expect("down");
        let pos = c.pad.borrow().pos.clone();
        let y_down = pos[first][0].1;
        assert!(y_down < (40 * (first as i64 + 1) - 40) as f64);
        let play = 0.5 * TOUCH_UNITS_PER_MM;
        let want = c.touch(1).y as f64 - play;
        assert!((pos.last().expect("pos")[0].1 - want).abs() <= 2.0);
    }

    #[test]
    fn hovering_finger_wobbling_on_the_spot_stays_up() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 1200), (MAJOR, 600)]);
        for i in 1..40 {
            c.frame(&[(SLOT, 0), (Y, 1200 + if i % 2 == 1 { 38 } else { -38 }), (MAJOR, 600)]);
        }
        assert!(c.touch(1).ok);
        assert!(c.calls().iter().all(|k| k.is_empty()));
        assert!(c.eng.why.contains("wobbling"));
    }

    #[test]
    fn wobbling_second_finger_does_not_join_a_swipe() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, -300), (Y, 1200), (MAJOR, 600)]);
        for i in 1..30 {
            let mut ev = vec![(SLOT, 0), (X, -300 + 40 * i), (MAJOR, 600)];
            if i == 3 {
                ev.extend([(SLOT, 1), (TID, 2), (X, 300), (Y, 1100), (MAJOR, 600)]);
            } else if i > 3 {
                ev.extend([(SLOT, 1), (X, 300 + if i % 2 == 1 { 40 } else { -40 }), (MAJOR, 600)]);
            }
            c.frame(&ev);
        }
        assert!(c.calls().iter().filter(|k| !k.is_empty()).all(|k| *k == vec![1]));
    }

    #[test]
    fn wobble_of_a_finger_down_moves_nothing() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600)]);
        for i in 1..16 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 600)]);
        }
        for i in 0..10 {
            c.frame(&[(SLOT, 0), (Y, 600 + if i % 2 == 1 { 12 } else { -12 }), (MAJOR, 600)]);
        }
        let pos = c.pad.borrow().pos.clone();
        let ys: Vec<f64> = pos[pos.len() - 10..].iter().map(|p| p[0].1).collect();
        assert!(ys[1..].iter().all(|y| *y == ys[1]));
        assert!(ys.windows(2).all(|w| w[1] >= w[0]));
        let still = ys[9];
        for i in 1..6 {
            c.frame(&[(SLOT, 0), (Y, 600 + 40 * i), (MAJOR, 600)]);
        }
        assert!(c.pad.borrow().pos.last().expect("pos")[0].1 > still);
    }

    fn edge_swipe(c: &mut Case) {
        c.set("scroll", "edge_left_mm", toml::Value::Float(13.0));
        c.frame(&[(SLOT, 0), (TID, 1), (X, -700), (Y, -1000), (MAJOR, 600)]);
        c.frame(&[(SLOT, 1), (TID, 2), (X, 200), (Y, -1100), (MAJOR, 600)]);
        for i in 1..25 {
            c.frame(&[(SLOT, 0), (X, -700 + 45 * i), (SLOT, 1), (X, 200 + 45 * i)]);
        }
    }

    #[test]
    fn swipe_with_a_finger_from_the_edge_is_a_swipe() {
        let mut c = Case::touchpad();
        edge_swipe(&mut c);
        assert_eq!(c.last(), vec![1, 2]);
    }

    #[test]
    fn edge_finger_left_after_a_swipe_holds_again() {
        let mut c = Case::touchpad();
        edge_swipe(&mut c);
        assert_eq!(c.last(), vec![1, 2]);
        c.frame(&[(SLOT, 1), (TID, -1)]);
        c.frame(&[(SLOT, 2), (TID, 3), (X, 300), (Y, -500), (MAJOR, 600)]);
        for i in 1..25 {
            c.frame(&[(SLOT, 2), (Y, -500 + 40 * i)]);
        }
        assert!(c.touch(1).grip);
        assert_eq!(c.last(), vec![3]);
    }

    #[test]
    fn one_finger_sideways_then_a_second_joins_is_a_swipe() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 200), (Y, -1100), (MAJOR, 600)]);
        for i in 1..30 {
            let mut ev = vec![(SLOT, 0), (X, 200 + 30 * i)];
            if i == 20 {
                ev.extend([(SLOT, 1), (TID, 2), (X, -500), (Y, -1000), (MAJOR, 600)]);
            } else if i > 20 {
                ev.extend([(SLOT, 1), (X, -500 + 30 * (i - 20))]);
            }
            c.frame(&ev);
        }
        assert_eq!(c.last(), vec![1, 2]);
    }

    #[test]
    fn wobbling_edge_finger_is_not_released() {
        let mut c = Case::touchpad();
        c.set("scroll", "edge_right_mm", toml::Value::Float(13.0));
        c.set("scroll", "edge_release_mm", toml::Value::Float(4.0));
        c.frame(&[(SLOT, 0), (TID, 1), (X, 1030), (Y, -1290), (MAJOR, 760)]);
        for i in 1..60 {
            c.frame(&[(SLOT, 0), (X, 1030 - 3 * i - if i % 8 < 4 { 100 } else { 0 }), (Y, -1290 - if i % 6 < 3 { 30 } else { 0 })]);
        }
        assert!(c.touch(1).grip);
        assert!(c.calls().iter().all(|k| k.is_empty()));
    }

    #[test]
    fn swipe_off_two_fingers_sideways_scroll_as_one() {
        let mut c = Case::touchpad();
        c.set("touchpad", "swipe", toml::Value::Boolean(false));
        swipe_pair_start(&mut c);
        for i in 1..20 {
            c.frame(&[(SLOT, 0), (X, -300 + 40 * i), (SLOT, 1), (X, 300 + 40 * i)]);
        }
        let down: Vec<Vec<i32>> = c.calls().into_iter().filter(|k| !k.is_empty()).collect();
        assert!(!down.is_empty() && down.iter().all(|k| k.len() == 1));
    }

    #[test]
    fn reversed_swipe_is_sent_mirrored() {
        let mut c = Case::touchpad();
        c.set("touchpad", "swipe_reverse", toml::Value::Boolean(true));
        swipe_pair_start(&mut c);
        for i in 1..20 {
            c.frame(&[(SLOT, 0), (X, -300 + 40 * i), (SLOT, 1), (X, 300 + 40 * i)]);
        }
        assert_eq!(c.last(), vec![1, 2]);
        assert!(c.pad.borrow().mirror);
    }

    #[test]
    fn finger_staying_light_lifts() {
        let mut c = Case::touchpad();
        c.frame(&[(SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600)]);
        for i in 1..10 {
            c.frame(&[(SLOT, 0), (Y, 40 * i), (MAJOR, 600)]);
        }
        for _ in 10..25 {
            c.frame(&[(SLOT, 0), (Y, 400), (MAJOR, 150)]);
        }
        assert_eq!(c.last(), Vec::<i32>::new());
    }

    // ---------------------------------------------------------------- beyond the Python suite
    #[test]
    fn two_finger_sideways_fires_the_workspace_step() {
        let mut c = Case::new();
        swipe_pair_start(&mut c);
        for i in 1..20 {
            c.frame(&[(SLOT, 0), (X, -300 - 40 * i), (SLOT, 1), (X, 300 - 40 * i)]);
        }
        assert_eq!(
            *c.actions.borrow(),
            vec![r#"step true wrap dispatch hl.dsp.focus({ workspace = "e+1" }) true"#.to_string()]
        );
    }

    struct FakeDev;
    impl DeviceInfo for FakeDev {
        fn abs_range(&self, code: u16) -> Option<(i32, i32)> {
            match code {
                ABS_MT_SLOT => Some((0, 3)),
                ABS_MT_POSITION_X => Some((-1000, 1000)),
                ABS_MT_POSITION_Y => Some((-1500, 2000)),
                _ => None,
            }
        }
        fn mt_slots(&self, code: u16) -> Option<Vec<i32>> {
            Some(match code {
                ABS_MT_TRACKING_ID => vec![-1, 5, -1, -1],
                ABS_MT_POSITION_X => vec![0, 11, 0, 0],
                ABS_MT_POSITION_Y => vec![0, 22, 0, 0],
                ABS_MT_TOUCH_MAJOR => vec![0, 33, 0, 0],
                _ => vec![0, 44, 0, 0],
            })
        }
        fn abs_value(&self, code: u16) -> Option<i32> {
            (code == ABS_MT_SLOT).then_some(2)
        }
        fn active_keys(&self) -> Vec<u16> {
            vec![]
        }
        fn has_msc_raw(&self) -> bool {
            true
        }
    }

    #[test]
    fn attach_reads_the_ranges_and_the_slots() {
        let mut c = Case::new();
        c.eng.attach(Box::new(FakeDev));
        assert_eq!((c.eng.x_min, c.eng.x_max, c.eng.y_min, c.eng.y_max), (-1000, 1000, -1500, 2000));
        assert_eq!(c.eng.slots.keys(), vec![1]);
        assert_eq!(c.eng.slots[1], [5, 11, 22, 33, 44]);
        assert_eq!(c.eng.cur_slot, 2);
    }

    #[test]
    fn dropped_events_release_lost_buttons() {
        struct Out(Rc<RefCell<Vec<(u16, u16, i32)>>>);
        impl Output for Out {
            fn write(&mut self, kind: u16, code: u16, value: i32) {
                self.0.borrow_mut().push((kind, code, value));
            }
            fn syn(&mut self) {}
        }
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut eng = Engine::new(defaults(), false, Box::new(Out(log.clone())), Box::new(FakeActions(Rc::default())));
        eng.attach(Box::new(FakeDev));
        let ev = |kind, code, value| Event { ts: 1.0, kind, code, value };
        eng.feed(ev(EV_KEY, BTN_MIDDLE, 1));
        eng.feed(ev(EV_SYN, SYN_REPORT, 0));
        eng.feed(ev(EV_SYN, SYN_DROPPED, 0));
        eng.feed(ev(EV_SYN, SYN_REPORT, 0));
        // middle maps to left (buttons.middle = "left"), and the lost release comes as left too
        assert_eq!(*log.borrow(), vec![(EV_KEY, BTN_LEFT, 1), (EV_KEY, BTN_LEFT, 0)]);
    }
}
