# MagicMouse 2.0: Rust port of `bin/magic-mouse-daemon`

Reference is the Python daemon at tag **v1.4.1** (`bin/magic-mouse-daemon`). The port changes
**nothing visible**: same `config.toml` keys and defaults, same `battery.json`, same probe-socket
lines, same Hyprland Lua, same virtual devices (names, ids, capabilities). Panel/QML stay untouched.
Behaviour differences are bugs, unless this file lists them.

## Acceptance

1. `cargo test` green; the Python unit tests (`tests/test_touch_tracking.py`) ported 1:1 as Rust tests.
2. **Golden traces:** `tools/golden.py` drives the Python `Engine` with each recording in
   `~/Work/omarchy_bob/docs/magic-mouse/` (and synthetic cases) and writes a trace; the Rust binary's
   `--replay` writes the same trace. `tools/compare.py` diffs them. Must be identical (floats rounded to
   1e-6 in the trace).
3. Riker review, then live test by the human.

## Replay contract (golden traces)

Input file (`*.events`), one line per evdev event, plus tick lines:

    E <ts> <type> <code> <value>      # fed to Engine::feed
    T <now>                           # Engine::tick(now)
    A <json>                          # device attach: {"x":[min,max],"y":[min,max],"msc_raw":bool}

Ticks: the generator inserts `T` lines every 8 ms (TICK) between events while coasting would matter;
simplest rule: after every SYN_REPORT and then every 8 ms until the next event. Both sides only
*consume* the file, so the schedule is whatever the file says.

Config: `--replay <events> --config <toml>`; defaults when no config.

Output trace, one line per call the Engine makes on the outside world, in order:

    W <type> <code> <value>           # virtual mouse write
    S                                 # virtual mouse syn
    P <gain> <mirror 0|1> <id:x:y,...>  # virtual touchpad update (x, y rounded to 6 decimals)
    X <cmd>                           # run_action
    K <forward 0|1> <include_empty 0|1> <end> <fallback>   # workspace_step (fallback = rest of line)
    B <json>                          # probe frame (only with --probe; sorted keys)

## Crate layout and owners (exclusive write per file)

| File | Owner | Python reference |
|---|---|---|
| `Cargo.toml`, `src/main.rs`, `src/iface.rs` | Bob (integration) | `run`, `main`, `Session`, `publish_status` |
| `src/engine.rs`, `src/touch.rs`, `src/curve.rs` | Henry | `Touch`, `AppleCurve`, `Engine` (lines ~105-145, 460-536, 1207-2380) |
| `src/config.rs`, `src/status.rs`, `src/probe.rs` | Bill | `DEFAULTS`, `load_config`, `config_mtime`, own-file helpers, `write_status`, `Probe` |
| `src/hypr.rs` | Luke | `hypr_socket`, `hypr_request`, `HyprEvents`, `run_action`, `workspace_step`, `vpad_lua`, control-click Lua |
| `src/devices.rs`, `src/vpad.rs`, `src/battery.rs`, `src/bluez.rs` | Bender | `is_magic_mouse`, `find_mice`, `make_uinput`, `VPad`, `hidraw_for`, battery query, `kernel_battery`, `BluezBattery` |
| `tools/golden.py`, `tools/compare.py`, `src/replay.rs` | Sonde | replay contract above |

Everything talks through the traits in `src/iface.rs`. Changing `iface.rs` = ask Bob.

## Engine API (fixed, Henry implements, Sonde/Bob call it)

```rust
pub struct Engine { /* Python Engine fields; pub where Session/probe need them */ }
impl Engine {
    pub fn new(cfg: Config, verbose: bool, ui: Box<dyn Output>, actions: Box<dyn Actions>) -> Self;
    pub fn set_config(&mut self, cfg: Config);              // live reload (Python: eng.cfg = ...)
    pub fn set_vpad(&mut self, vpad: Option<Box<dyn TouchpadOut>>);
    pub fn set_probe(&mut self, probe: Option<Box<dyn ProbeSink>>);
    pub fn attach(&mut self, dev: Box<dyn DeviceInfo>);
    pub fn feed(&mut self, ev: Event);
    pub fn tick(&mut self, now: f64);
    pub fn coasting(&self) -> bool;
    pub fn release_buttons(&mut self);
    pub fn reset_touch(&mut self);
}
```

The shared virtual mouse is shared via `Rc<RefCell<..>>` behind the `Output` impl (Bob's business).

## Crates

`evdev` (devices + uinput), `serde`/`serde_json`, `toml`, `zbus` (blocking API, BlueZ), `libc`.
No async runtime of our own; one `poll(2)` loop like the Python `select` loop.

## Packaging (later, Bender)

GitHub Actions builds `x86_64-unknown-linux-musl` (static, `--release`), attaches
`magic-mouse-daemon-x86_64-linux` + `.sha256` to the release and records a build provenance
attestation (GitHub attestations).
`install.sh` downloads the binary pinned in `daemon.lock`, checks it against the digest pinned
there (not the release's `.sha256`), installs to `~/.local/bin/magic-mouse-daemon`. Python daemon stays in
the repo until 2.0 is accepted.

## Integration notes (Bob)

- `bin/magic-mouse-config` exec()s the Python daemon file for `load_config`, `DEFAULTS`, `write_own_file`
  (Bill). With a binary at that path it breaks → the binary gets `config get|defaults|set` subcommands
  (config::to_json, status::write_own_file) and magic-mouse-config becomes a thin wrapper, same CLI.
- serde_json `preserve_order` on: probe frames and battery.json keep Python's key order.
