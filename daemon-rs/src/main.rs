//! MagicMouse daemon 2.0: Rust port of bin/magic-mouse-daemon v1.4.1. See PORT.md.
//! main.rs (owner Bob): Session, the poll loop and the command line (Python: Session, publish_status, run, main).
#![allow(dead_code)]

mod battery;
mod bluez;
mod config;
mod config_cli;
mod curve;
mod devices;
mod engine;
mod hypr;
mod iface;
mod probe;
mod replay;
mod status;
mod touch;
mod vpad;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use bluez::BluezBattery;
use devices::{MagicMouse, VirtualOut};
use engine::Engine;
use hypr::{HyprActions, HyprEvent, HyprEvents};
use iface::{log, Config, Output, ProbeSink};
use probe::Probe;

const RESCAN_SEC: f64 = 2.0;
const TICK: f64 = 1.0 / 125.0;
const TOUCH_UNITS_PER_MM: f64 = 38.0;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// time.time()
fn wall() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// time.monotonic()
fn mono(start: Instant) -> f64 {
    start.elapsed().as_secs_f64()
}

/// The one virtual mouse, shared by every Session's Engine.
#[derive(Clone)]
struct SharedOut(Rc<RefCell<VirtualOut>>);

impl Output for SharedOut {
    fn write(&mut self, kind: u16, code: u16, value: i32) {
        self.0.borrow_mut().write(kind, code, value)
    }
    fn syn(&mut self) {
        self.0.borrow_mut().syn()
    }
}

/// One grabbed Magic Mouse: its own Engine, battery and Hyprland settings for its touchpad.
struct Session {
    dev: MagicMouse,
    bluez: Rc<BluezBattery>,
    eng: Engine,
    cfg: Config,
    hidraw: Option<PathBuf>,
    next_battery: f64,
    next_kernel_battery: f64,
    queried: Option<(u8, bool)>,
    battery: Option<(u8, bool)>,
    vpad_cfg: Option<String>,
    vpad_retry: f64,
    has_vpad: bool,
}

impl Session {
    fn new(mut dev: MagicMouse, ui: &SharedOut, cfg: &Config, verbose: bool, bluez: Rc<BluezBattery>, now: f64) -> std::io::Result<Self> {
        let eng = Engine::new(cfg.clone(), verbose, Box::new(ui.clone()), Box::new(HyprActions));
        let hidraw = battery::hidraw_for(dev.uniq(), dev.vendor(), dev.product());
        log(&format!("grab {} ({} {:04x}:{:04x})", dev.path(), dev.name(), dev.vendor(), dev.product()));
        dev.grab()?;
        let info = dev.info()?;
        let mut s = Session {
            dev,
            bluez,
            eng,
            cfg: cfg.clone(),
            hidraw,
            next_battery: 0.0,
            next_kernel_battery: 0.0,
            queried: None,
            battery: None,
            vpad_cfg: None,
            vpad_retry: 0.0,
            has_vpad: false,
        };
        s.eng.attach(Box::new(info));
        s.sync_vpad(now);
        Ok(s)
    }

    /// The probe's hello for this mouse: name and touch surface.
    fn geometry(&self) -> Value {
        let mut geo = serde_json::Map::new();
        geo.insert("model".into(), json!(self.dev.name()));
        geo.insert("product".into(), json!(format!("{:04x}", self.dev.product())));
        geo.insert("res".into(), json!(TOUCH_UNITS_PER_MM));
        for (key, code) in [("x", ABS_MT_POSITION_X), ("y", ABS_MT_POSITION_Y), ("major", ABS_MT_TOUCH_MAJOR)] {
            if let Some((lo, hi)) = self.dev.abs_range(code) {
                geo.insert(key.into(), json!([lo, hi]));
            }
        }
        Value::Object(geo)
    }

    fn set_config(&mut self, cfg: &Config) {
        self.cfg = cfg.clone();
        self.eng.set_config(cfg.clone());
    }

    fn sync_vpad(&mut self, now: f64) {
        let want = self.cfg.b("touchpad", "enabled");
        if want && !self.has_vpad {
            let (xr, yr, res) = self.eng.surface();
            match vpad::VPad::new(xr, yr, res) {
                Ok(pad) => {
                    log("touchpad mode on");
                    self.eng.set_vpad(Some(Box::new(pad))); // stop_coast, scrolling off, reset_tp
                    self.has_vpad = true;
                    self.vpad_cfg = None;
                }
                Err(exc) => {
                    log(&format!("touchpad mode: cannot create the virtual touchpad, staying off: {exc}"));
                    return;
                }
            }
        } else if !want && self.has_vpad {
            self.eng.set_vpad(None); // dropping the VPad lifts its fingers (Python vpad.close)
            self.has_vpad = false;
            log("touchpad mode off");
        }
        if self.has_vpad {
            let lua = hypr::vpad_lua(&self.cfg);
            if self.vpad_cfg.as_deref() != Some(lua.as_str()) {
                let reply = hypr::hypr_request(&format!("eval {lua}"));
                self.vpad_cfg = hypr::reply_ok(reply.as_deref()).then_some(lua);
                if self.vpad_cfg.is_none() {
                    self.vpad_retry = now + HyprEvents::RETRY_SEC;
                }
            }
        }
    }

    /// Battery and a pending touchpad settings retry. True if the charge changed (status to write).
    fn poll(&mut self, now: f64) -> bool {
        let mut changed = false;
        let enabled = self.cfg.b("battery", "enabled");
        let to_bluez = self.cfg.b("battery", "bluez");
        let uniq = self.dev.uniq().to_string();
        if enabled && now >= self.next_battery {
            self.next_battery = now + self.cfg.f("battery", "poll_minutes").max(1.0) * 60.0;
            self.next_kernel_battery = now; // settle the charge below right away
            let got = self.hidraw.as_deref().and_then(battery::query_battery);
            if let Some((pct, charging)) = got {
                self.queried = got;
                log(&format!("battery {pct}%{}", if charging { " charging" } else { "" }));
            } else if self.hidraw.is_none() {
                log("battery: no hidraw node found for the mouse");
            }
        }
        if enabled && now >= self.next_kernel_battery {
            self.next_kernel_battery = now + 60.0;
            // One source, so the bar does not flip between two readings: the kernel's, which follows
            // the mouse, and our query's until the kernel knows (some mice never answer the query).
            let pct = battery::kernel_battery(&uniq).or(self.queried.map(|q| q.0));
            let now_bat = pct.map(|p| (p, self.queried.is_some_and(|q| q.1)));
            if now_bat != self.battery {
                self.battery = now_bat;
                changed = true;
            }
            if to_bluez {
                if let Some(p) = pct {
                    self.bluez.set(&uniq, p);
                }
            }
        }
        if !(enabled && to_bluez) {
            self.bluez.clear(&uniq, false); // switched off live: no stale charge in Bluetooth settings
        }
        if self.has_vpad && self.vpad_cfg.is_none() && now >= self.vpad_retry {
            self.sync_vpad(now);
        }
        changed
    }

    fn status(&self) -> Value {
        let mut d = serde_json::Map::new();
        d.insert("model".into(), json!(self.dev.name()));
        d.insert("address".into(), json!(self.dev.uniq()));
        if let Some((pct, charging)) = self.battery {
            d.insert("percent".into(), json!(pct));
            d.insert("charging".into(), json!(charging));
        }
        Value::Object(d)
    }

    /// Feed what the mouse sent. False when it is gone.
    fn read(&mut self) -> bool {
        match self.dev.read() {
            Ok(events) => {
                for ev in events {
                    self.eng.feed(ev);
                }
                true
            }
            Err(exc) => {
                log(&format!("device gone: {exc}"));
                false
            }
        }
    }

    fn close(mut self, probe_is_ours: bool, probe: Option<&Rc<RefCell<Probe>>>) {
        self.eng.set_vpad(None);
        self.eng.release_buttons();
        let _ = self.dev.ungrab();
        self.eng.reset_touch();
        if probe_is_ours {
            self.eng.set_probe(None);
            if let Some(p) = probe {
                p.borrow_mut().send_obj(&json!({"type": "disconnected"}));
            }
        }
        self.bluez.clear(self.dev.uniq(), true);
        log(&format!("released grab ({})", self.dev.name()));
    }
}

/// battery.json for the bar: every mouse under "mice"; the top level is the emptiest one.
fn publish_status(sessions: &BTreeMap<String, Session>) {
    let mice: Vec<Value> = sessions.values().map(Session::status).collect();
    if mice.is_empty() {
        status::write_status(&json!({"connected": false, "mice": [], "ts": wall()}));
        return;
    }
    let top = mice
        .iter()
        .filter(|m| m.get("percent").is_some())
        .min_by_key(|m| m["percent"].as_u64().unwrap_or(0))
        .unwrap_or(&mice[0]);
    let mut d = top.as_object().cloned().unwrap_or_default();
    d.insert("connected".into(), json!(true));
    d.insert("mice".into(), Value::Array(mice.clone()));
    d.insert("ts".into(), json!(wall()));
    status::write_status(&Value::Object(d));
}

struct Loop {
    sessions: BTreeMap<String, Session>,
    active: Option<String>,
    probe: Option<Rc<RefCell<Probe>>>,
}

impl Loop {
    fn make_active(&mut self, path: Option<String>) {
        if path == self.active {
            return;
        }
        if let Some(old) = self.active.as_ref().and_then(|p| self.sessions.get_mut(p)) {
            old.eng.set_probe(None);
        }
        self.active = path.clone();
        let Some(probe) = &self.probe else { return };
        match path.and_then(|p| self.sessions.get_mut(&p)) {
            Some(s) => {
                s.eng.set_probe(Some(Box::new(probe.clone()) as Box<dyn ProbeSink>));
                let mut p = probe.borrow_mut();
                p.set_hello(s.geometry());
                p.send_hello();
            }
            None => probe.borrow_mut().send_obj(&json!({"type": "disconnected"})),
        }
    }

    fn drop_session(&mut self, path: &str) {
        let Some(s) = self.sessions.remove(path) else { return };
        let was_active = self.active.as_deref() == Some(path);
        if was_active {
            self.active = None; // close() sends "disconnected" itself
        }
        s.close(was_active, self.probe.as_ref());
        if was_active {
            let next = self.sessions.keys().next().cloned();
            if next.is_some() {
                self.make_active(next);
            }
        }
        publish_status(&self.sessions);
    }
}

fn run(ui: &SharedOut, cfg_path: &Path, verbose: bool, probe: Option<Rc<RefCell<Probe>>>, hypr: &mut HyprEvents, bluez: Rc<BluezBattery>, record: Option<PathBuf>) {
    let start = Instant::now();
    let mut lp = Loop { sessions: BTreeMap::new(), active: None, probe };
    let mut cfg = config::load_config(cfg_path);
    let mut mtime = config::config_mtime(cfg_path);
    let (mut next_cfg_check, mut next_scan) = (0.0, 0.0);
    let mut hypr_conn = hypr.connects;
    publish_status(&lp.sessions);

    while !STOP.load(Ordering::SeqCst) {
        let now = mono(start);
        if now >= next_scan {
            next_scan = now + RESCAN_SEC;
            let known: HashSet<String> = lp.sessions.keys().cloned().collect();
            for dev in devices::find_mice(&known) {
                let path = dev.path().to_string();
                match Session::new(dev, ui, &cfg, verbose, bluez.clone(), now) {
                    Ok(mut s) => {
                        if let Some(r) = &record {
                            match std::fs::OpenOptions::new().append(true).create(true).open(r) {
                                Ok(f) => s.eng.set_record(Some(Box::new(f))),
                                Err(exc) => log(&format!("record: {exc}")),
                            }
                        }
                        lp.sessions.insert(path.clone(), s);
                        if lp.active.is_none() {
                            lp.make_active(Some(path));
                        }
                        publish_status(&lp.sessions);
                    }
                    Err(exc) => log(&format!("cannot grab {path}: {exc}")),
                }
            }
        }
        let mut any = false;
        for s in lp.sessions.values_mut() {
            any |= s.poll(now);
        }
        if any {
            publish_status(&lp.sessions);
        }
        hypr.connect(now);
        if hypr.connects != hypr_conn {
            // a new Hyprland (or a reconnect): settings are gone
            hypr_conn = hypr.connects;
            for s in lp.sessions.values_mut() {
                s.vpad_cfg = None;
            }
        }
        hypr.sync_binding(cfg.b("buttons", "control_click") && !lp.sessions.is_empty(), now);
        let coasting = lp.sessions.values().any(|s| s.eng.coasting());
        let timeout = if coasting { TICK } else { 0.25 };

        let mut fds: Vec<libc::pollfd> = vec![];
        let dev_fds: Vec<(i32, String)> = lp.sessions.iter().map(|(p, s)| (s.dev.fd(), p.clone())).collect();
        fds.extend(dev_fds.iter().map(|(fd, _)| libc::pollfd { fd: *fd, events: libc::POLLIN, revents: 0 }));
        let hypr_fd = hypr.fd();
        if let Some(fd) = hypr_fd {
            fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
        }
        if let Some(p) = &lp.probe {
            fds.extend(p.borrow().fds().into_iter().map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }));
        }
        // SAFETY: fds is a valid array of pollfd for the duration of the call.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, (timeout * 1000.0).ceil() as i32) };
        let ready: Vec<i32> = if n > 0 {
            fds.iter().filter(|f| f.revents != 0).map(|f| f.fd).collect()
        } else {
            vec![]
        };
        if let Some(p) = &lp.probe {
            p.borrow_mut().handle(&ready);
        }
        for (fd, path) in &dev_fds {
            if !ready.contains(fd) {
                continue;
            }
            let Some(s) = lp.sessions.get_mut(path) else { continue };
            if !s.read() {
                lp.drop_session(path);
            } else if lp.active.as_deref() != Some(path.as_str()) && s.eng.has_touches() {
                lp.make_active(Some(path.clone())); // the preview shows the mouse being touched
            }
        }
        if hypr_fd.is_some_and(|fd| ready.contains(&fd)) {
            for ev in hypr.read_events() {
                match ev {
                    HyprEvent::ControlClick => {
                        if let Some(s) = lp.active.as_ref().and_then(|p| lp.sessions.get_mut(p)) {
                            s.eng.control_click(wall());
                        }
                    }
                    HyprEvent::ConfigReloaded => {
                        hypr.resync(); // a reload drops bindings added at runtime
                        let now = mono(start);
                        for s in lp.sessions.values_mut() {
                            s.vpad_cfg = None; // ...and the virtual touchpads' settings
                            s.sync_vpad(now);
                        }
                    }
                    HyprEvent::Other => {}
                }
            }
        }
        for s in lp.sessions.values_mut() {
            if s.eng.coasting() {
                s.eng.tick(wall());
            }
        }
        let now = mono(start);
        if now >= next_cfg_check {
            next_cfg_check = now + 1.0;
            let m = config::config_mtime(cfg_path);
            if m != mtime {
                mtime = m;
                cfg = config::load_config(cfg_path);
                for s in lp.sessions.values_mut() {
                    s.set_config(&cfg);
                    s.sync_vpad(now);
                }
                log("config reloaded");
            }
        }
    }
    let paths: Vec<String> = lp.sessions.keys().cloned().collect();
    for p in paths {
        lp.drop_session(&p);
    }
}

fn default_config() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/magic-mouse/config.toml")
}

const HELP: &str = "usage: magic-mouse-daemon [--config PATH] [--verbose] [--list] [--record PATH] [--no-probe]
       magic-mouse-daemon config get|defaults|set section.key=value ...
       magic-mouse-daemon battery-query
       magic-mouse-daemon --replay EVENTS [--config TOML] [--probe]

Mac-like Magic Mouse daemon";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("config") => std::process::exit(config_cli::main(&args[1..])),
        Some("battery-query") => match battery::battery_query_cli() {
            Ok(out) => {
                println!("{out}");
                return;
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        _ => {}
    }
    let (mut cfg_path, mut verbose, mut list, mut record, mut no_probe) = (default_config(), false, false, None, false);
    let (mut replay, mut probe_trace) = (None::<PathBuf>, false);
    let mut cfg_given = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => {
                cfg_path = it.next().map(PathBuf::from).unwrap_or_else(|| usage_exit());
                cfg_given = true;
            }
            "--verbose" => verbose = true,
            "--list" => list = true,
            "--record" => record = Some(it.next().map(PathBuf::from).unwrap_or_else(|| usage_exit())),
            "--no-probe" => no_probe = true,
            "--replay" => replay = Some(it.next().map(PathBuf::from).unwrap_or_else(|| usage_exit())),
            "--probe" => probe_trace = true,
            "-h" | "--help" => {
                println!("{HELP}");
                return;
            }
            _ => usage_exit(),
        }
    }

    if let Some(events) = replay {
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        let cfg = cfg_given.then_some(cfg_path.as_path());
        if let Err(e) = replay::run(&events, cfg, probe_trace, &mut out) {
            eprintln!("replay: {e}");
            std::process::exit(1);
        }
        return;
    }

    if list {
        let mice = devices::find_mice(&HashSet::new());
        if mice.is_empty() {
            println!("no Magic Mouse found (is it connected? is hid_magicmouse bound?)");
            std::process::exit(1);
        }
        for dev in mice {
            println!("{}: {} {:04x}:{:04x}", dev.path(), dev.name(), dev.vendor(), dev.product());
        }
        return;
    }

    let _ = config::load_config(&cfg_path);
    log(&format!("magic-mouse-daemon pid={} config={}", std::process::id(), cfg_path.display()));
    let ui = match devices::make_uinput() {
        Ok(u) => SharedOut(Rc::new(RefCell::new(u))),
        Err(e) => {
            log(&format!("cannot create the virtual mouse: {e}"));
            std::process::exit(1);
        }
    };
    log(&format!("virtual device '{}' ready", devices::VIRT_NAME));
    if let Some(r) = &record {
        log(&format!("recording raw input to {}", r.display()));
    }
    let probe = (!no_probe).then(|| {
        let mut p = Probe::new();
        p.open();
        Rc::new(RefCell::new(p))
    });
    let mut hypr = HyprEvents::new();
    let bluez = Rc::new(BluezBattery::new()); // always: battery.bluez can be switched on and off live
    // systemctl stop sends SIGTERM: leave the loop so buttons are released and the
    // Control-click binding is taken back out of Hyprland.
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    run(&ui, &cfg_path, verbose, probe.clone(), &mut hypr, bluez, record);
    if STOP.load(Ordering::SeqCst) {
        log("interrupt");
    }
    if hypr.is_connected() && hypr.bound == Some(true) {
        hypr.sync_binding(false, f64::MAX); // past any retry wait: take the binding out now
    }
    hypr.close();
    if let Some(p) = probe {
        p.borrow_mut().close();
    }
}

fn usage_exit() -> ! {
    eprintln!("{HELP}");
    std::process::exit(2);
}
