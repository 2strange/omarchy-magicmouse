//! Hyprland: command socket, event socket, actions, workspace stepping, Lua strings. Owner: Luke.
//! Python reference (v1.4.1): `hypr_socket`, `hypr_request`, `CONTROL_CLICK_EVENT`, `_LUA_UNBIND`,
//! `_LUA_BIND`, `HyprEvents`, `run_action`, `WORKSPACE_NEXT/PREV`, `workspace_step`, `vpad_lua`.
use std::collections::BTreeSet;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::iface::{log, Actions, Config};

/// The virtual mouse's name (Python `VIRT_NAME`); the Control-click binding is limited to it.
pub const VIRT_NAME: &str = "magic-mouse-omarchy";
/// The virtual touchpad as Hyprland calls it (Python `VPAD_HYPR`).
pub const VPAD_HYPR: &str = "magic-mouse-virtual-touchpad";
/// Python `TP_SCROLL_GAIN`: the virtual touchpad's scroll_factor is scroll_speed divided by it.
pub const TP_SCROLL_GAIN: f64 = 2.0;

pub const CONTROL_CLICK_EVENT: &str = "custom>>magic-mouse>>control-click";
pub const LUA_UNBIND: &str = "if _G.magic_mouse_control_click then pcall(function() _G.magic_mouse_control_click:unbind() end) _G.magic_mouse_control_click = nil end";
pub const LUA_BIND: &str = concat!(
    "if _G.magic_mouse_control_click then pcall(function() _G.magic_mouse_control_click:unbind() end) _G.magic_mouse_control_click = nil end",
    r#" _G.magic_mouse_control_click = hl.bind("CTRL + mouse:272", "#,
    r#"hl.dsp.event("magic-mouse>>control-click"), "#,
    r#"{ device = { inclusive = true, list = { "magic-mouse-omarchy" } }, "#,
    r#"description = "Magic Mouse: Control-click is a right click" })"#
);

pub const WORKSPACE_NEXT: &str = r#"workspace = "e+1""#;
pub const WORKSPACE_PREV: &str = r#"workspace = "e-1""#;

// --------------------------------------------------------------------------- sockets

fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        // SAFETY: getuid has no failure mode.
        format!("/run/user/{}", unsafe { libc::getuid() })
    })
}

/// Python `hypr_socket()`: the command socket of this session's Hyprland, else the newest one.
pub fn hypr_socket() -> Option<PathBuf> {
    let rt = runtime_dir();
    if let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        if !sig.is_empty() {
            let p = PathBuf::from(format!("{rt}/hypr/{sig}/.socket.sock"));
            if p.exists() {
                return Some(p);
            }
        }
    }
    newest_socket(Path::new(&format!("{rt}/hypr")))
}

/// glob(`<dir>/*/.socket.sock`), the one with the newest mtime.
fn newest_socket(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for ent in std::fs::read_dir(dir).ok()?.flatten() {
        let name = ent.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue; // glob's * does not match hidden names
        }
        let p = ent.path().join(".socket.sock");
        if let Ok(m) = std::fs::metadata(&p).and_then(|m| m.modified()) {
            if best.as_ref().is_none_or(|(t, _)| m > *t) {
                best = Some((m, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// One synchronous request on a Unix socket: send, read to EOF. Errors are logged (Python: `hyprland: ...`).
fn request_on(path: &Path, cmd: &str, timeout: Duration) -> Option<String> {
    let res = (|| -> std::io::Result<Vec<u8>> {
        let mut c = UnixStream::connect(path)?;
        c.set_read_timeout(Some(timeout))?;
        c.set_write_timeout(Some(timeout))?;
        c.write_all(cmd.as_bytes())?;
        let mut out = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match c.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    })();
    match res {
        Ok(out) => Some(String::from_utf8_lossy(&out).into_owned()),
        Err(exc) => {
            log(&format!("hyprland: {exc}"));
            None
        }
    }
}

/// Python `hypr_request(cmd, timeout=0.5)`: the reply, None if Hyprland can't be reached.
pub fn hypr_request(cmd: &str) -> Option<String> {
    hypr_request_timeout(cmd, Duration::from_millis(500))
}

pub fn hypr_request_timeout(cmd: &str, timeout: Duration) -> Option<String> {
    let sock = hypr_socket()?;
    request_on(&sock, cmd, timeout)
}

// --------------------------------------------------------------------------- event socket

/// What the main loop does with a line from the event socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HyprEvent {
    /// Control + left press on our virtual mouse: answer with a right click (`Engine::control_click`).
    ControlClick,
    /// Hyprland reloaded its config: runtime bindings and device settings are gone
    /// (call `HyprEvents::resync`, clear every session's vpad_cfg and re-sync it).
    ConfigReloaded,
    Other,
}

/// Python `run`: `line == CONTROL_CLICK_EVENT` / `line.startswith("configreloaded>>")`.
pub fn parse_event(line: &str) -> HyprEvent {
    if line == CONTROL_CLICK_EVENT {
        HyprEvent::ControlClick
    } else if line.starts_with("configreloaded>>") {
        HyprEvent::ConfigReloaded
    } else {
        HyprEvent::Other
    }
}

/// Python `HyprEvents`: Hyprland's event socket (.socket2.sock) plus the Control-click binding it serves.
pub struct HyprEvents {
    sock: Option<UnixStream>,
    buf: Vec<u8>,
    next_try: f64,
    /// What Hyprland currently has, as far as we know.
    pub bound: Option<bool>,
    next_sync: f64,
    warned: bool,
    /// Successful connections so far (a change: re-apply runtime settings).
    pub connects: u64,
}

impl Default for HyprEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl HyprEvents {
    pub const RETRY_SEC: f64 = 5.0;

    pub fn new() -> Self {
        HyprEvents {
            sock: None,
            buf: Vec::new(),
            next_try: 0.0,
            bound: None,
            next_sync: 0.0,
            warned: false,
            connects: 0,
        }
    }

    /// The fd to poll for reading, None while not connected (Python: `[hypr] if hypr.sock`).
    pub fn fd(&self) -> Option<RawFd> {
        self.sock.as_ref().map(|s| s.as_raw_fd())
    }

    pub fn is_connected(&self) -> bool {
        self.sock.is_some()
    }

    /// Called every loop pass (monotonic `now`); tries at most every RETRY_SEC.
    pub fn connect(&mut self, now: f64) {
        self.connect_with(now, hypr_socket);
    }

    fn connect_with(&mut self, now: f64, find: impl FnOnce() -> Option<PathBuf>) {
        if self.sock.is_some() || now < self.next_try {
            return;
        }
        self.next_try = now + Self::RETRY_SEC;
        let Some(cmd) = find() else { return };
        let ev = cmd.parent().unwrap_or(Path::new("")).join(".socket2.sock");
        let Ok(s) = UnixStream::connect(ev) else {
            return;
        };
        if s.set_nonblocking(true).is_err() {
            return;
        }
        self.sock = Some(s);
        self.buf.clear();
        self.connects += 1;
        self.resync(); // a new connection may be a new Hyprland
    }

    pub fn resync(&mut self) {
        self.bound = None;
        self.next_sync = 0.0;
    }

    pub fn close(&mut self) {
        self.sock = None;
        self.bound = None;
    }

    /// Python `read_lines()`: call when the fd is readable. Complete lines; closes on EOF/error.
    pub fn read_lines(&mut self) -> Vec<String> {
        let Some(s) = self.sock.as_mut() else {
            return Vec::new();
        };
        let mut chunk = vec![0u8; 65536];
        let n = match s.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                return Vec::new()
            }
            Err(_) => 0,
        };
        if n == 0 {
            self.close();
            return Vec::new();
        }
        self.buf.extend_from_slice(&chunk[..n]);
        let mut lines = Vec::new();
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            lines.push(String::from_utf8_lossy(&line[..i]).into_owned());
        }
        lines
    }

    /// `read_lines` classified with `parse_event`.
    pub fn read_events(&mut self) -> Vec<HyprEvent> {
        self.read_lines().iter().map(|l| parse_event(l)).collect()
    }

    /// Python `sync_binding(want, now)`: called every loop pass; only talks to Hyprland on a change.
    pub fn sync_binding(&mut self, want: bool, now: f64) {
        self.sync_binding_with(want, now, &mut hypr_request);
    }

    fn sync_binding_with(
        &mut self,
        want: bool,
        now: f64,
        req: &mut dyn FnMut(&str) -> Option<String>,
    ) {
        if self.sock.is_none() || self.bound == Some(want) || now < self.next_sync {
            return;
        }
        let reply = req(&format!(
            "eval {}",
            if want { LUA_BIND } else { LUA_UNBIND }
        ));
        let ok = reply.as_deref().is_some_and(|r| py_strip(r) == "ok");
        if !ok {
            self.next_sync = now + Self::RETRY_SEC;
            if let Some(r) = &reply {
                if want && !self.warned {
                    log(&format!(
                        "control_click: Hyprland refused the binding (needs Lua-config Hyprland): {}",
                        py_strip(r)
                    ));
                    self.warned = true;
                }
            }
            return;
        }
        if want || self.bound == Some(true) {
            // at startup, clearing a leftover from an earlier run is not news
            log(&format!(
                "control_click binding {}",
                if want { "added" } else { "removed" }
            ));
        }
        self.bound = Some(want);
    }
}

/// Python `str.strip()` (whitespace).
fn py_strip(s: &str) -> &str {
    s.trim()
}

/// Session.sync_vpad's check: the reply to `eval <lua>` means it was applied.
pub fn reply_ok(reply: Option<&str>) -> bool {
    reply.is_some_and(|r| py_strip(r) == "ok")
}

// --------------------------------------------------------------------------- actions

/// Python `run_action(cmd)`: "dispatch ..." / "keyword ..." go straight to Hyprland's socket
/// (detached thread, 1 s timeout, one recv); anything else is `/bin/sh -c cmd` in a new session,
/// stdout/stderr to /dev/null, environment and stdin inherited. Never blocks the caller.
pub fn run_action(cmd: &str) {
    let cmd = cmd.trim();
    match classify_action(cmd) {
        ActionKind::Nothing => {}
        ActionKind::Hypr => {
            let cmd = cmd.to_owned();
            spawn_logged(move || send_action(&cmd));
        }
        ActionKind::Shell => {
            if let Err(exc) = spawn_shell(cmd) {
                log(&format!("action failed: {exc}"));
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ActionKind {
    Nothing,
    Hypr,
    Shell,
}

fn classify_action(cmd: &str) -> ActionKind {
    match cmd.split_whitespace().next() {
        None => ActionKind::Nothing,
        Some("dispatch" | "keyword") => ActionKind::Hypr,
        Some(_) => ActionKind::Shell,
    }
}

fn send_action(cmd: &str) {
    let Some(sock) = hypr_socket() else {
        log("action: no Hyprland socket");
        return;
    };
    let res = (|| -> std::io::Result<()> {
        let t = Some(Duration::from_secs(1));
        let mut c = UnixStream::connect(sock)?;
        c.set_read_timeout(t)?;
        c.set_write_timeout(t)?;
        c.write_all(cmd.as_bytes())?;
        let mut buf = [0u8; 4096];
        let _ = c.read(&mut buf)?;
        Ok(())
    })();
    if let Err(exc) = res {
        log(&format!("action failed: {exc}"));
    }
}

fn shell_command(cmd: &str) -> Command {
    let mut c = Command::new("/bin/sh");
    c.arg("-c")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe (Python: start_new_session=True).
    unsafe {
        c.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    c
}

fn spawn_shell(cmd: &str) -> std::io::Result<()> {
    let mut child = shell_command(cmd).spawn()?;
    // reap it, so no zombie stays behind (Python's Popen reaps on a later call)
    spawn_logged(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// A detached helper thread; when the system cannot start one (EAGAIN), log and go on.
fn spawn_logged(f: impl FnOnce() + Send + 'static) {
    if let Err(exc) = std::thread::Builder::new().spawn(f) {
        log(&format!("cannot start a helper thread: {exc}"));
    }
}

/// One step at a time, so a quick second swipe sees the first one's result.
static WS_LOCK: Mutex<()> = Mutex::new(());

/// Python `workspace_step(forward, end, fallback, include_empty)`: in a detached thread.
pub fn workspace_step(forward: bool, end: &str, fallback: &str, include_empty: bool) {
    let (end, fallback) = (end.to_owned(), fallback.to_owned());
    spawn_logged(move || {
        let _g = WS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        workspace_step_with(
            forward,
            &end,
            &fallback,
            include_empty,
            &mut hypr_request,
            &mut run_action,
        );
    });
}

/// Python Engine.two_fingers: `swipe_end` falls back to "wrap" when unknown.
pub fn swipe_end(cfg: &Config) -> String {
    let end = cfg_get(cfg, "gestures", "swipe_end").map_or_else(|| "wrap".to_owned(), py_str);
    if matches!(end.as_str(), "wrap" | "new" | "stop") {
        end
    } else {
        "wrap".to_owned()
    }
}

/// The synchronous body of `workspace_step`; `req` is hypr_request, `run` is run_action.
pub fn workspace_step_with(
    forward: bool,
    end: &str,
    fallback: &str,
    include_empty: bool,
    req: &mut dyn FnMut(&str) -> Option<String>,
    run: &mut dyn FnMut(&str),
) {
    let act = req("j/activeworkspace");
    let spaces = req("j/workspaces");
    let (cur, here, elsewhere) = match read_workspaces(act.as_deref(), spaces.as_deref()) {
        Ok(v) => v,
        Err(exc) => {
            log(&format!("workspace step: {exc}"));
            return;
        }
    };
    if cur <= 0 {
        run(fallback);
        return;
    }
    if let Some(target) = step_target(forward, end, include_empty, cur, &here, &elsewhere) {
        if target != cur {
            req(&focus_cmd(target));
        }
    }
}

/// `dispatch hl.dsp.focus({ workspace = "<id>" })`
pub fn focus_cmd(target: i64) -> String {
    format!(r#"dispatch hl.dsp.focus({{ workspace = "{target}" }})"#)
}

type Spaces = (i64, BTreeSet<i64>, BTreeSet<i64>);

/// The current workspace, ids on its monitor (with it), ids on other monitors (positive ids only).
fn read_workspaces(act: Option<&str>, spaces: Option<&str>) -> Result<Spaces, String> {
    let nonempty = |s: Option<&str>, d: &'static str| -> String {
        match s {
            Some(s) if !s.is_empty() => s.to_owned(),
            _ => d.to_owned(),
        }
    };
    let act: Value = serde_json::from_str(&nonempty(act, "{}")).map_err(|e| e.to_string())?;
    let spaces: Value = serde_json::from_str(&nonempty(spaces, "[]")).map_err(|e| e.to_string())?;
    let act = act.as_object().ok_or("activeworkspace is not an object")?;
    let cur = match act.get("id") {
        None => 0,
        Some(v) => py_int(v)?,
    };
    let mon = act.get("monitorID").unwrap_or(&Value::Null);
    let mut here = BTreeSet::from([cur]);
    let mut elsewhere = BTreeSet::new();
    let items: &[Value] = match &spaces {
        Value::Array(a) => a,
        Value::Object(_) | Value::String(_) => &[], // iterating gives strings: none is a dict
        _ => return Err("workspaces is not iterable".into()),
    };
    for w in items {
        let Some(w) = w.as_object() else { continue };
        let id = match w.get("id") {
            None => 0,
            Some(v) => py_int(v)?,
        };
        if id > 0 {
            if py_eq(w.get("monitorID").unwrap_or(&Value::Null), mon) {
                here.insert(id);
            } else {
                elsewhere.insert(id);
            }
        }
    }
    Ok((cur, here, elsewhere))
}

/// The workspace to focus, None for nothing (Python workspace_step after the reads, cur > 0).
fn step_target(
    forward: bool,
    end: &str,
    include_empty: bool,
    cur: i64,
    here: &BTreeSet<i64>,
    elsewhere: &BTreeSet<i64>,
) -> Option<i64> {
    let top = *here.iter().max()?;
    let ids: Vec<i64> = if include_empty {
        (1..=top.max(5))
            .filter(|i| !elsewhere.contains(i) || here.contains(i))
            .collect()
    } else {
        here.iter().copied().collect()
    };
    let pos = ids.iter().position(|&i| i == cur)?;
    if forward {
        if pos + 1 < ids.len() {
            Some(ids[pos + 1])
        } else if end == "wrap" {
            Some(ids[0])
        } else if end == "new" {
            let mut t = ids[ids.len() - 1] + 1;
            while elsewhere.contains(&t) {
                t += 1;
            }
            (t <= 10).then_some(t)
        } else {
            None
        }
    } else if pos > 0 {
        Some(ids[pos - 1])
    } else if end == "wrap" {
        Some(ids[ids.len() - 1])
    } else {
        None
    }
}

/// Python int() on a JSON value.
fn py_int(v: &Value) -> Result<i64, String> {
    match v {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => n
            .as_i64()
            .or_else(|| {
                n.as_f64()
                    .filter(|f| f.is_finite())
                    .map(|f| f.trunc() as i64)
            })
            .ok_or_else(|| format!("invalid int: {n}")),
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("invalid literal for int(): '{s}'")),
        _ => Err(format!(
            "int() argument must be a string or a number, not {v}"
        )),
    }
}

/// Python == on JSON values (1 == 1.0).
fn py_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(x), Some(y)) => x == y,
            _ => x.as_f64() == y.as_f64(),
        },
        _ => a == b,
    }
}

/// The real side effects: what the Engine starts outside itself.
#[derive(Debug, Default, Clone, Copy)]
pub struct HyprActions;

impl Actions for HyprActions {
    fn run_action(&mut self, cmd: &str) {
        run_action(cmd);
    }

    fn workspace_step(&mut self, forward: bool, end: &str, fallback: &str, include_empty: bool) {
        workspace_step(forward, end, fallback, include_empty);
    }
}

/// The swipe action that led to a step (Python passes it as `fallback`; the trait does not):
/// the first of two_finger_swipe_left/right that holds WORKSPACE_NEXT (forward) or only WORKSPACE_PREV.
pub fn step_fallback(forward: bool, cfg: &Config) -> String {
    let actions: Vec<String> = ["two_finger_swipe_left", "two_finger_swipe_right"]
        .iter()
        .filter_map(|k| cfg_get(cfg, "gestures", k).map(py_str))
        .collect();
    let fits = |a: &String| {
        let next = a.contains(WORKSPACE_NEXT);
        (next || a.contains(WORKSPACE_PREV)) && next == forward
    };
    actions
        .iter()
        .find(|a| fits(a))
        .cloned()
        .unwrap_or_else(|| {
            if forward {
                format!("dispatch hl.dsp.focus({{ {WORKSPACE_NEXT} }})")
            } else {
                format!("dispatch hl.dsp.focus({{ {WORKSPACE_PREV} }})")
            }
        })
}

// --------------------------------------------------------------------------- vpad

/// Python `vpad_lua(cfg)`: Hyprland settings for the virtual touchpad only.
pub fn vpad_lua(cfg: &Config) -> String {
    let natural = if cfg_get(cfg, "scroll", "natural").is_some_and(py_truthy) {
        "true"
    } else {
        "false"
    };
    let factor = py_fmt4(cfg.f("touchpad", "scroll_speed") / TP_SCROLL_GAIN);
    format!(
        r#"hl.device({{ name = "{VPAD_HYPR}", scroll_factor = {factor}, natural_scroll = {natural}, tap_to_click = false }})"#
    )
}

/// Python `f"{x:.4f}"`.
fn py_fmt4(x: f64) -> String {
    if x.is_nan() {
        "nan".into()
    } else if x.is_infinite() {
        if x > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        }
    } else {
        format!("{x:.4}")
    }
}

// --------------------------------------------------------------------------- config helpers (dict.get)

fn cfg_get<'a>(cfg: &'a Config, sec: &str, key: &str) -> Option<&'a toml::Value> {
    cfg.0.get(sec).and_then(|s| s.get(key))
}

/// Python bool() on a TOML value.
fn py_truthy(v: &toml::Value) -> bool {
    match v {
        toml::Value::Boolean(b) => *b,
        toml::Value::Integer(i) => *i != 0,
        toml::Value::Float(f) => *f != 0.0,
        toml::Value::String(s) => !s.is_empty(),
        toml::Value::Array(a) => !a.is_empty(),
        toml::Value::Table(t) => !t.is_empty(),
        toml::Value::Datetime(_) => true,
    }
}

/// Python str() on a TOML value (strings as they are).
fn py_str(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::net::UnixListener;

    fn cfg(entries: &[(&str, &str, toml::Value)]) -> Config {
        let mut m: BTreeMap<String, BTreeMap<String, toml::Value>> = BTreeMap::new();
        for (s, k, v) in entries {
            m.entry((*s).into())
                .or_default()
                .insert((*k).into(), v.clone());
        }
        Config(m)
    }

    // Expected strings: printed by the Python daemon (v1.4.1) itself via importlib.
    #[test]
    fn control_click_strings() {
        assert_eq!(CONTROL_CLICK_EVENT, "custom>>magic-mouse>>control-click");
        assert_eq!(
            LUA_UNBIND,
            "if _G.magic_mouse_control_click then pcall(function() _G.magic_mouse_control_click:unbind() end) _G.magic_mouse_control_click = nil end"
        );
        assert_eq!(
            LUA_BIND,
            r#"if _G.magic_mouse_control_click then pcall(function() _G.magic_mouse_control_click:unbind() end) _G.magic_mouse_control_click = nil end _G.magic_mouse_control_click = hl.bind("CTRL + mouse:272", hl.dsp.event("magic-mouse>>control-click"), { device = { inclusive = true, list = { "magic-mouse-omarchy" } }, description = "Magic Mouse: Control-click is a right click" })"#
        );
        assert!(LUA_BIND.contains(VIRT_NAME));
    }

    #[test]
    fn vpad_lua_strings() {
        let cases: &[(f64, Option<bool>, &str)] = &[
            (1.0, Some(true), "0.5000"),
            (0.0625, Some(false), "0.0312"),
            (2.5, None, "1.2500"),
            (1e-5, Some(true), "0.0000"),
            (-0.0, Some(false), "-0.0000"),
            (123.456789, Some(true), "61.7284"),
            (0.00015, Some(true), "0.0001"),
            (0.35, Some(true), "0.1750"),
        ];
        for (speed, natural, factor) in cases {
            let mut e = vec![("touchpad", "scroll_speed", toml::Value::Float(*speed))];
            if let Some(n) = natural {
                e.push(("scroll", "natural", toml::Value::Boolean(*n)));
            }
            let nat = if natural.unwrap_or(false) {
                "true"
            } else {
                "false"
            };
            assert_eq!(
                vpad_lua(&cfg(&e)),
                format!(
                    r#"hl.device({{ name = "magic-mouse-virtual-touchpad", scroll_factor = {factor}, natural_scroll = {nat}, tap_to_click = false }})"#
                )
            );
        }
        // an integer scroll_speed and a truthy integer natural, as Python float()/bool()
        let c = cfg(&[
            ("touchpad", "scroll_speed", toml::Value::Integer(3)),
            ("scroll", "natural", toml::Value::Integer(1)),
        ]);
        assert_eq!(
            vpad_lua(&c),
            r#"hl.device({ name = "magic-mouse-virtual-touchpad", scroll_factor = 1.5000, natural_scroll = true, tap_to_click = false })"#
        );
    }

    #[test]
    fn focus_and_workspace_strings() {
        assert_eq!(
            focus_cmd(3),
            r#"dispatch hl.dsp.focus({ workspace = "3" })"#
        );
        assert_eq!(WORKSPACE_NEXT, r#"workspace = "e+1""#);
        assert_eq!(WORKSPACE_PREV, r#"workspace = "e-1""#);
    }

    #[test]
    fn events() {
        assert_eq!(
            parse_event("custom>>magic-mouse>>control-click"),
            HyprEvent::ControlClick
        );
        assert_eq!(
            parse_event("custom>>magic-mouse>>control-click "),
            HyprEvent::Other
        );
        assert_eq!(parse_event("configreloaded>>"), HyprEvent::ConfigReloaded);
        assert_eq!(parse_event("configreloaded>>x"), HyprEvent::ConfigReloaded);
        assert_eq!(parse_event("workspace>>2"), HyprEvent::Other);
        assert_eq!(parse_event(""), HyprEvent::Other);
    }

    #[test]
    fn actions_classified() {
        assert_eq!(classify_action(""), ActionKind::Nothing);
        assert_eq!(
            classify_action("dispatch hl.dsp.focus({ workspace = \"e+1\" })"),
            ActionKind::Hypr
        );
        assert_eq!(
            classify_action("keyword general:gaps_in 3"),
            ActionKind::Hypr
        );
        assert_eq!(classify_action("dispatcher x"), ActionKind::Shell);
        assert_eq!(classify_action("notify-send hi"), ActionKind::Shell);
        let c = shell_command("echo 'a b'");
        assert_eq!(c.get_program(), "/bin/sh");
        let args: Vec<_> = c.get_args().collect();
        assert_eq!(args, ["-c", "echo 'a b'"]);
    }

    /// A fake Hyprland: (activeworkspace, workspaces) replies; returns (requests, fallback runs).
    fn step(
        forward: bool,
        end: &str,
        include_empty: bool,
        act: &str,
        spaces: &str,
    ) -> (Vec<String>, Vec<String>) {
        let mut reqs = Vec::new();
        let mut runs = Vec::new();
        let mut req = |c: &str| {
            reqs.push(c.to_owned());
            match c {
                "j/activeworkspace" => Some(act.to_owned()),
                "j/workspaces" => Some(spaces.to_owned()),
                _ => Some("ok".to_owned()),
            }
        };
        let mut run = |c: &str| runs.push(c.to_owned());
        workspace_step_with(forward, end, "FALLBACK", include_empty, &mut req, &mut run);
        (reqs[2..].to_vec(), runs)
    }

    fn target(
        forward: bool,
        end: &str,
        include_empty: bool,
        act: &str,
        spaces: &str,
    ) -> Option<i64> {
        let (reqs, runs) = step(forward, end, include_empty, act, spaces);
        assert!(runs.is_empty());
        assert!(reqs.len() <= 1);
        reqs.first().map(|r| {
            let id = r
                .trim_start_matches(r#"dispatch hl.dsp.focus({ workspace = ""#)
                .trim_end_matches(r#"" })"#);
            assert_eq!(focus_cmd(id.parse().unwrap()), *r);
            id.parse().unwrap()
        })
    }

    // Expected values: the Python workspace_step with hypr_request/run_action patched (see the report).
    #[test]
    fn workspace_step_cases() {
        let one = r#"[{"id":1,"monitorID":0},{"id":2,"monitorID":0},{"id":3,"monitorID":0}]"#;
        let two = r#"[{"id":1,"monitorID":0},{"id":2,"monitorID":1},{"id":4,"monitorID":0},{"id":6,"monitorID":1}]"#;
        let a = |id: i64, m: i64| format!(r#"{{"id":{id},"monitorID":{m}}}"#);
        type Case<'a> = (bool, &'a str, bool, String, &'a str, Option<i64>);
        let cases: &[Case] = &[
            (true, "wrap", true, a(3, 0), one, Some(4)),
            (true, "wrap", true, a(5, 0), one, Some(1)),
            (false, "wrap", true, a(1, 0), one, Some(5)),
            (false, "stop", true, a(1, 0), one, None),
            (true, "stop", true, a(5, 0), one, None),
            (true, "new", true, a(5, 0), one, Some(6)),
            (true, "wrap", false, a(3, 0), one, Some(1)),
            (true, "new", false, a(3, 0), one, Some(4)),
            (false, "wrap", false, a(1, 0), one, Some(3)),
            (true, "wrap", true, a(1, 0), two, Some(3)),
            (true, "wrap", true, a(5, 0), two, Some(1)),
            (true, "new", true, a(5, 0), two, Some(7)),
            (true, "new", false, a(4, 0), two, Some(5)),
            (false, "wrap", true, a(3, 0), two, Some(1)),
            (
                true,
                "new",
                true,
                a(10, 0),
                r#"[{"id":10,"monitorID":0}]"#,
                None,
            ),
            (
                true,
                "new",
                true,
                a(9, 0),
                r#"[{"id":9,"monitorID":0},{"id":10,"monitorID":1}]"#,
                None,
            ),
            (
                true,
                "wrap",
                true,
                a(1, 0),
                r#"[{"id":-98,"monitorID":0},{"id":1,"monitorID":0}]"#,
                Some(2),
            ),
            (
                true,
                "wrap",
                false,
                a(1, 0),
                r#"[{"id":1,"monitorID":0}]"#,
                None,
            ), // target == cur
            (true, "wrap", true, a(2, 0), "{}", Some(3)),
            (true, "wrap", true, a(2, 0), "", Some(3)),
            (
                true,
                "wrap",
                true,
                r#"{"id":"2","monitorID":0}"#.into(),
                one,
                Some(3),
            ),
            (
                true,
                "wrap",
                true,
                r#"{"id":2.0,"monitorID":0.0}"#.into(),
                one,
                Some(3),
            ),
        ];
        for (fw, end, inc, act, spaces, want) in cases {
            assert_eq!(
                target(*fw, end, *inc, act, spaces),
                *want,
                "{fw} {end} {inc} {act} {spaces}"
            );
        }
    }

    #[test]
    fn workspace_step_fallback_and_errors() {
        // named workspace / unknown id: the plain action
        let (reqs, runs) = step(true, "wrap", true, r#"{"id":-98,"monitorID":0}"#, "[]");
        assert!(reqs.is_empty());
        assert_eq!(runs, ["FALLBACK"]);
        let (reqs, runs) = step(true, "wrap", true, "", "[]");
        assert!(reqs.is_empty());
        assert_eq!(runs, ["FALLBACK"]);
        // unreadable replies: logged, nothing done
        for (act, spaces) in [
            ("not json", "[]"),
            ("[]", "[]"),
            (r#"{"id":null}"#, "[]"),
            ("{}", "null"),
            ("{}", "5"),
        ] {
            let (reqs, runs) = step(true, "wrap", true, act, spaces);
            assert!(reqs.is_empty() && runs.is_empty(), "{act} {spaces}");
        }
        // Hyprland gone: None replies count as {} / [] -> fallback
        let mut runs = Vec::new();
        workspace_step_with(false, "wrap", "FB", true, &mut |_| None, &mut |c| {
            runs.push(c.to_owned())
        });
        assert_eq!(runs, ["FB"]);
    }

    #[test]
    fn swipe_end_and_fallback_from_config() {
        let s = |v: &str| toml::Value::String(v.into());
        assert_eq!(swipe_end(&cfg(&[])), "wrap");
        assert_eq!(
            swipe_end(&cfg(&[("gestures", "swipe_end", s("new"))])),
            "new"
        );
        assert_eq!(
            swipe_end(&cfg(&[("gestures", "swipe_end", s("bogus"))])),
            "wrap"
        );
        let left = r#"dispatch hl.dsp.focus({ workspace = "e+1" })"#;
        let right = r#"dispatch hl.dsp.focus({ workspace = "e-1" })"#;
        let c = cfg(&[
            ("gestures", "two_finger_swipe_left", s(left)),
            ("gestures", "two_finger_swipe_right", s(right)),
        ]);
        assert_eq!(step_fallback(true, &c), left);
        assert_eq!(step_fallback(false, &c), right);
        let c = cfg(&[
            (
                "gestures",
                "two_finger_swipe_left",
                s("keyword x; dispatch hl.dsp.focus({ workspace = \"e-1\" })"),
            ),
            ("gestures", "two_finger_swipe_right", s("notify-send x")),
        ]);
        assert_eq!(
            step_fallback(false, &c),
            "keyword x; dispatch hl.dsp.focus({ workspace = \"e-1\" })"
        );
        assert_eq!(step_fallback(true, &c), left);
    }

    fn fake_hypr() -> (PathBuf, UnixListener) {
        let dir = std::env::temp_dir().join(format!(
            "mm-hypr-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let l = UnixListener::bind(dir.join(".socket2.sock")).unwrap();
        (dir.join(".socket.sock"), l)
    }

    #[test]
    fn event_socket_lines_and_reconnect() {
        let (cmd, listener) = fake_hypr();
        let mut h = HyprEvents::new();
        h.connect_with(0.0, || None);
        assert!(h.fd().is_none());
        // the retry waits RETRY_SEC
        h.connect_with(1.0, || Some(cmd.clone()));
        assert!(h.fd().is_none());
        h.connect_with(5.0, || Some(cmd.clone()));
        assert!(h.fd().is_some());
        assert_eq!(h.connects, 1);
        let (mut peer, _) = listener.accept().unwrap();
        assert!(h.read_lines().is_empty()); // nothing yet: WouldBlock
        assert!(h.is_connected());
        peer.write_all(b"workspace>>2\ncustom>>magic-mouse>>control-click\nconfigrel")
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(h.read_events(), [HyprEvent::Other, HyprEvent::ControlClick]);
        peer.write_all(b"oaded>>\n\xffx\n").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(h.read_lines(), ["configreloaded>>", "\u{fffd}x"]);
        h.bound = Some(true);
        drop(peer);
        std::thread::sleep(Duration::from_millis(20));
        assert!(h.read_lines().is_empty());
        assert!(h.fd().is_none());
        assert_eq!(h.bound, None);
        h.connect_with(9.0, || Some(cmd.clone()));
        assert!(h.fd().is_none());
        h.connect_with(10.0, || Some(cmd.clone()));
        assert_eq!(h.connects, 2);
        let _ = std::fs::remove_dir_all(cmd.parent().unwrap());
    }

    #[test]
    fn binding_sync() {
        let (cmd, _listener) = fake_hypr();
        let mut h = HyprEvents::new();
        let mut sent: Vec<String> = Vec::new();
        let mut reply = Some("ok".to_owned());
        // not connected: nothing
        h.sync_binding_with(true, 0.0, &mut |c| {
            sent.push(c.into());
            reply.clone()
        });
        assert!(sent.is_empty());
        h.connect_with(0.0, || Some(cmd.clone()));
        // startup: clear a leftover
        h.sync_binding_with(false, 0.0, &mut |c| {
            sent.push(c.into());
            reply.clone()
        });
        assert_eq!(sent, [format!("eval {LUA_UNBIND}")]);
        assert_eq!(h.bound, Some(false));
        h.sync_binding_with(false, 0.1, &mut |c| {
            sent.push(c.into());
            reply.clone()
        });
        assert_eq!(sent.len(), 1);
        // refused: retry after RETRY_SEC
        reply = Some("error: no hl\n".into());
        h.sync_binding_with(true, 1.0, &mut |c| {
            sent.push(c.into());
            reply.clone()
        });
        assert_eq!(sent[1], format!("eval {LUA_BIND}"));
        assert_eq!(h.bound, Some(false));
        assert!(h.warned);
        h.sync_binding_with(true, 5.9, &mut |c| {
            sent.push(c.into());
            reply.clone()
        });
        assert_eq!(sent.len(), 2);
        reply = Some(" ok\n".into());
        h.sync_binding_with(true, 6.0, &mut |c| {
            sent.push(c.into());
            reply.clone()
        });
        assert_eq!(h.bound, Some(true));
        // a config reload: resync sends it again
        h.resync();
        h.sync_binding_with(true, 6.1, &mut |c| {
            sent.push(c.into());
            reply.clone()
        });
        assert_eq!(sent.len(), 4);
        assert!(reply_ok(Some("ok\n")) && !reply_ok(Some("okay")) && !reply_ok(None));
        let _ = std::fs::remove_dir_all(cmd.parent().unwrap());
    }

    #[test]
    fn newest_socket_glob() {
        let dir = std::env::temp_dir().join(format!("mm-hypr-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["a", "b", ".hidden"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        assert_eq!(newest_socket(&dir), None);
        std::fs::write(dir.join("a/.socket.sock"), "").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.join("b/.socket.sock"), "").unwrap();
        std::fs::write(dir.join(".hidden/.socket.sock"), "").unwrap();
        assert_eq!(newest_socket(&dir), Some(dir.join("b/.socket.sock")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
