#!/usr/bin/python3
"""Golden traces for the Rust port (daemon-rs/PORT.md, "Replay contract").

usage: tools/golden.py build [--out DIR] [--recordings DIR] [--only NAME ...]
           write every case: DIR/<name>.events, DIR/<name>.trace, DIR/<name>.toml (when the case has
           a config) and DIR/manifest.json (default DIR: daemon-rs/tests/golden)
       tools/golden.py run <events> [--config TOML] [--probe]
           the Python reference: replay one events file, the trace goes to stdout
           (what `magic-mouse-daemon --replay <events> [--config TOML] [--probe]` must print)
       tools/golden.py events <recording.jsonl> [--kind KIND]
           convert one recording to an events file on stdout (KIND: probe, hidraw, evdev; default: guessed)

Runs with /usr/bin/python3 -I and needs python-evdev (the daemon imports it). The daemon is loaded
like tests/test_touch_tracking.py does it (side-effect free: main() is not called), and everything
it would do outside itself is replaced by a recorder:

  ui.write / ui.syn        -> W <type> <code> <value> / S
  vpad.update              -> P <gain> <mirror 0|1> <id:x:y,...>   (6 decimals; "-" = no fingers)
  run_action(cmd)          -> X <cmd>
  workspace_step(..)       -> K <forward 0|1> <include_empty 0|1> <end> <fallback>
  probe.send(obj)          -> B <json>  (only with --probe; sorted keys, compact separators)

Choices both sides honour (Rust: daemon-rs/src/replay.rs):
  * Attach (A line): the fake device has the A line's ABS_MT_POSITION_X/Y ranges, ABS_MT_SLOT 0..15,
    MSC_RAW in its capabilities when msc_raw is true. Its kernel state (current slot, EVIOCGMTSLOTS,
    keys held) comes from the optional "slot", "slots" ([slot, tid, x, y, major, minor] per active
    slot) and "keys" fields; missing = slot 0, all 16 slots empty (tracking id -1, the rest 0), no key.
    Every recording is attached empty (they all start with a landing or a frame with no contact).
  * "M <json>" (an extension of the contract): the same state fields, replacing the device's state
    from then on; read by the resync after SYN_DROPPED (sync_mt, active_keys).
  * Touchpad mode: right after an attach, when touchpad.enabled is true and no virtual touchpad is set
    yet, the recorder is set as the virtual touchpad (Python: Session.sync_vpad -> eng.vpad = VPad,
    stop_coast, scrolling = False, reset_tp; Rust: Engine::set_vpad(Some(..)) must do the same).
  * The firmware sysfs parameter (report_undeciphered) reads as missing, so the probe's "fw" field is
    "active", "ready", "disabled" or "off", never "reconnect" (Python: UNDECIPHERED patched).
  * Config: <name>.toml through load_config, defaults when the case has none.
  * Ticks: after every SYN_REPORT a "T <ts>" line, then one every TICK (8 ms, computed as ts + k*TICK,
    rounded to 6 decimals) while it is earlier than the next event, at most TICK_CAP seconds after it.
    The last frame gets the full TICK_CAP. The consumer just calls tick() for every T line.
  * "C <now>" (an extension of the contract): Hyprland reported the Control + click binding, the
    daemon calls Engine.control_click(now).
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.machinery
import importlib.util
import json
import math
import os
import shutil
import sys
import types

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DAEMON = os.path.join(ROOT, "bin", "magic-mouse-daemon")
OUT = os.path.join(ROOT, "daemon-rs", "tests", "golden")
RECORDINGS = os.path.expanduser("~/Work/omarchy_bob/docs/magic-mouse")

TICK = 0.008
TICK_CAP = 3.0  # covers momentum.max_seconds (2.5 s) after the last lift
SLOTS = 16  # ABS_MT_SLOT 0..15, like hid-magicmouse

EV_SYN, EV_KEY, EV_REL, EV_ABS, EV_MSC = 0, 1, 2, 3, 4
SYN_REPORT, SYN_DROPPED = 0, 3
REL_X, REL_Y = 0, 1
MSC_RAW = 3
BTN_LEFT, BTN_RIGHT, BTN_MIDDLE = 272, 273, 274
SLOT, MAJOR, MINOR, ORIENT, X, Y, TID = 47, 48, 49, 52, 53, 54, 57
X_RANGE, Y_RANGE = (-1100, 1258), (-1589, 2047)


# --------------------------------------------------------------------------- the Python reference
_daemon = None


def daemon():
    """The daemon as a module, loaded once, with its outside world cut off."""
    global _daemon
    if _daemon is None:
        loader = importlib.machinery.SourceFileLoader("magic_mouse_daemon_golden", DAEMON)
        spec = importlib.util.spec_from_loader("magic_mouse_daemon_golden", loader)
        d = importlib.util.module_from_spec(spec)
        loader.exec_module(d)
        d.log = lambda msg: None
        d.UNDECIPHERED = "/nonexistent/report_undeciphered"
        d.fcntl = types.SimpleNamespace(ioctl=_ioctl)  # only sync_mt's EVIOCGMTSLOTS goes through here
        _daemon = d
    return _daemon


_device = None  # the attached FakeDevice


def _ioctl(fd, request, buf, mutate=True):
    """EVIOCGMTSLOTS on the fake device: its current state (see FakeDevice)."""
    code = buf[0]
    col = {TID: 0, X: 1, Y: 2, MAJOR: 3, MINOR: 4}.get(code)
    for i in range(1, len(buf)):
        v = _device.slots.get(i - 1) if _device else None
        buf[i] = v[col] if v is not None and col is not None else (-1 if code == TID else 0)
    return 0


def f6(v) -> str:
    return f"{float(v):.6f}"


class FakeDevice:
    def __init__(self, info: dict) -> None:
        self.fd = -1
        self.ranges = {X: tuple(info.get("x", X_RANGE)), Y: tuple(info.get("y", Y_RANGE)), SLOT: (0, SLOTS - 1)}
        self.msc_raw = bool(info.get("msc_raw", False))
        self.slot = 0
        self.slots: dict[int, list] = {}
        self.keys: list[int] = []
        self.set_state(info)

    def set_state(self, info: dict) -> None:
        """What the kernel holds right now (A and M lines): current slot, the active slots
        [slot, tracking id, x, y, major, minor] and the keys held. Missing = empty."""
        self.slot = int(info.get("slot", 0))
        self.slots = {int(s[0]): [int(v) for v in s[1:6]] for s in info.get("slots", [])}
        self.keys = [int(k) for k in info.get("keys", [])]

    def absinfo(self, code):
        if code not in self.ranges:
            raise OSError(22, "no such axis")
        lo, hi = self.ranges[code]
        return types.SimpleNamespace(value=self.slot if code == SLOT else 0, min=lo, max=hi, fuzz=0, flat=0, resolution=0)

    def capabilities(self):
        return {EV_MSC: [MSC_RAW]} if self.msc_raw else {}

    def active_keys(self):
        return list(self.keys)


class Recorder:
    """ui, vpad and probe in one: every call becomes a trace line."""

    def __init__(self, out: list) -> None:
        self.out = out
        self.clients = [object()]

    # virtual mouse
    def write(self, typ, code, value):
        self.out.append(f"W {typ} {code} {int(value)}")

    def syn(self):
        self.out.append("S")

    # virtual touchpad
    def update(self, fingers, gain, mirror=False):
        fs = ",".join(f"{int(t)}:{f6(x)}:{f6(y)}" for t, x, y in fingers) or "-"
        self.out.append(f"P {f6(gain)} {1 if mirror else 0} {fs}")

    # probe
    def send(self, obj, targets=None):
        self.out.append("B " + json.dumps(obj, sort_keys=True, separators=(",", ":")))


def load_cfg(path: str | None) -> dict:
    d = daemon()
    if path is None:
        return {k: dict(v) for k, v in d.DEFAULTS.items()}
    if not os.path.exists(path):
        raise SystemExit(f"config {path} not found")
    return d.load_config(path)


def replay(lines, cfg_path: str | None, probe: bool, attribution: list | None = None) -> list[str]:
    """Run an events file (as lines) through the Python Engine; returns the trace lines.
    attribution, if given, gets the 1-based input line number of every trace line."""
    global _device
    _device = None
    d = daemon()
    out: list[str] = []
    d.run_action = lambda cmd: out.append(f"X {cmd}")
    d.workspace_step = lambda forward, end, fallback, include_empty=True: out.append(
        f"K {1 if forward else 0} {1 if include_empty else 0} {end} {fallback}")
    cfg = load_cfg(cfg_path)
    rec = Recorder(out)
    eng = d.Engine(cfg, rec, False)
    if probe:
        eng.probe = rec
    for no, line in enumerate(lines, 1):
        line = line.rstrip("\n")
        if not line or line.startswith("#"):
            continue
        tag, _, rest = line.partition(" ")
        if tag == "E":
            ts, typ, code, value = rest.split()
            ts = float(ts)
            eng.feed(types.SimpleNamespace(type=int(typ), code=int(code), value=int(value), timestamp=lambda ts=ts: ts))
        elif tag == "T":
            eng.tick(float(rest))
        elif tag == "C":
            eng.control_click(float(rest))
        elif tag == "M":
            if _device is None:
                raise SystemExit(f"line {no}: M before A")
            _device.set_state(json.loads(rest))
        elif tag == "A":
            _device = FakeDevice(json.loads(rest))
            eng.attach(_device)
            if bool(eng.cfg["touchpad"]["enabled"]) and eng.vpad is None:  # Session.sync_vpad
                eng.vpad = rec
                eng.stop_coast("touchpad mode")
                eng.scrolling = False
                eng.tp_key = None
                eng.tp_light.clear()
                eng.tp_owed.clear()
                eng.tp_out.clear()
                eng.tp_swiped = False
                eng.tp_pair_n = 0
        else:
            raise SystemExit(f"line {no}: unknown line {line[:40]!r}")
        if attribution is not None:
            attribution.extend([no] * (len(out) - len(attribution)))
    return out


# --------------------------------------------------------------------------- events files
def num(ts: float) -> str:
    return repr(float(ts))


class Events:
    """Builds an events file: A, E and C lines; T lines are added by lines()."""

    def __init__(self) -> None:
        self.items: list[tuple] = []  # ("A", json) | ("E", ts, type, code, value) | ("C", ts)

    def attach(self, x=X_RANGE, y=Y_RANGE, msc_raw=False) -> None:
        self.items.append(("A", json.dumps({"x": list(x), "y": list(y), "msc_raw": bool(msc_raw)}, sort_keys=True)))

    def ev(self, ts, typ, code, value) -> None:
        self.items.append(("E", float(ts), int(typ), int(code), int(value)))

    def device_state(self, slot=0, slots=(), keys=()) -> None:
        """M line: what EVIOCGMTSLOTS, ABS_MT_SLOT and EVIOCGKEY report from now on."""
        self.items.append(("M", json.dumps({"slot": slot, "slots": [list(s) for s in slots], "keys": list(keys)},
                                           sort_keys=True, separators=(",", ":"))))

    def control_click(self, ts) -> None:
        self.items.append(("C", float(ts)))

    def lines(self) -> list[str]:
        out = []
        times = [it[1] for it in self.items if it[0] in ("E", "C")]
        k = 0  # index into times of the current timed item
        for it in self.items:
            if it[0] in ("A", "M"):
                out.append(f"{it[0]} {it[1]}")
                continue
            if it[0] == "C":
                out.append(f"C {num(it[1])}")
            else:
                out.append(f"E {num(it[1])} {it[2]} {it[3]} {it[4]}")
            k += 1
            if it[0] == "E" and it[2] == EV_SYN and it[3] == SYN_REPORT:
                ts = it[1]
                nxt = times[k] if k < len(times) else math.inf
                out.append(f"T {num(ts)}")
                i = 1
                while i * TICK <= TICK_CAP + 1e-9:
                    t = round(ts + i * TICK, 6)
                    if t >= nxt:
                        break
                    out.append(f"T {num(t)}")
                    i += 1
        return out


class Frames(Events):
    """Synthetic input: whole evdev frames with contacts, motion and buttons."""

    def __init__(self, t0=1000.0, msc_raw=False) -> None:
        super().__init__()
        self.t = t0
        self.msc_raw = msc_raw
        self.slots: dict[int, int] = {}  # tracking id -> slot
        self.last: dict[int, tuple] = {}  # tracking id -> its last (x, y)
        self.next_tid = 100
        self.attach(msc_raw=msc_raw)

    def tid(self) -> int:
        self.next_tid += 1
        return self.next_tid

    def frame(self, contacts=(), lift=(), rel=(0, 0), keys=(), dt=0.011, syn=SYN_REPORT) -> None:
        """contacts: (tid, x, y[, major, minor[, state]]) every finger down; lift: tids lifting."""
        ts = self.t
        for tid in lift:
            self.ev(ts, EV_ABS, SLOT, self.slots.pop(tid))
            self.ev(ts, EV_ABS, TID, -1)
            self.last.pop(tid, None)
        for c in contacts:
            tid, x, y = c[:3]
            self.last[tid] = (x, y)
            major = c[3] if len(c) > 3 else 600
            minor = c[4] if len(c) > 4 else 400
            state = c[5] if len(c) > 5 else 4
            if tid not in self.slots:
                self.slots[tid] = next(i for i in range(SLOTS) if i not in self.slots.values())
                self.ev(ts, EV_ABS, SLOT, self.slots[tid])
                self.ev(ts, EV_ABS, TID, tid)
            else:
                self.ev(ts, EV_ABS, SLOT, self.slots[tid])
            for code, v in ((X, x), (Y, y), (MAJOR, major), (MINOR, minor)):
                self.ev(ts, EV_ABS, code, round(v))
            if self.msc_raw:
                self.ev(ts, EV_MSC, MSC_RAW, state << 4)
        for code, v in keys:
            self.ev(ts, EV_KEY, code, v)
        if rel[0]:
            self.ev(ts, EV_REL, REL_X, rel[0])
        if rel[1]:
            self.ev(ts, EV_REL, REL_Y, rel[1])
        self.ev(ts, EV_SYN, syn, 0)
        self.t = round(ts + dt, 6)

    def idle(self, seconds) -> None:
        """Nothing happens for this long (the ticks fill it)."""
        self.t = round(self.t + seconds, 6)

    def slide(self, tids, start, step, n, **kw) -> list:
        """n frames of fingers moving together by step per frame from start (one (x, y) per finger)."""
        pos = [list(p) for p in start]
        for i in range(n):
            self.frame([(t, *p, *kw.get("extra", ())) for t, p in zip(tids, pos)], dt=kw.get("dt", 0.011))
            for p, s in zip(pos, step if isinstance(step[0], (tuple, list)) else [step] * len(tids)):
                p[0] += s[0]
                p[1] += s[1]
        return pos

    def touch(self, tids, pos, n, **kw) -> None:
        """Fingers lying still for n frames."""
        for _ in range(n):
            self.frame([(t, *p, *kw.get("extra", ())) for t, p in zip(tids, pos)], dt=kw.get("dt", 0.011))

    def lift(self, tids, linger=8) -> None:
        """Fingers leave the way the mouse reports it: a few small reports (firmware: break touch,
        linger, out of range), then the slot is freed. Others still down keep reporting."""
        others = [t for t in self.slots if t not in tids]
        for i in range(linger):
            st = 5 if i == 0 else 7 if i == linger - 1 else 6
            self.frame([(t, *self.last[t], 160, 80, st) for t in tids]
                       + [(t, *self.last[t]) for t in others])
        self.frame([(t, *self.last[t]) for t in others], lift=tids)


# ---- recordings
def from_probe(path: str) -> Events:
    """flicker-rec.py / swipe-rec.py: the daemon's probe frames. Converted like flicker-replay.py and
    swipe-replay.py: per frame, lifts first (slot, tracking id -1) for contacts gone since the last
    frame, then every contact (slot, tracking id when new, x, y, major, minor), MSC_RAW state << 4 when
    the frame carries a firmware state (element 7, >= 0), then SYN_REPORT. Slots: lowest free one;
    tracking id = the recorded id. Motion and buttons are not in these recordings. All frames are
    fed, inside and between phases, in one run."""
    ev = Events()
    hello, frames, msc = {}, [], False
    for ln in open(path):
        o = json.loads(ln)
        if o.get("type") == "hello":
            hello = o
        elif "c" in o:
            frames.append(o)
            msc = msc or any(len(c) > 7 and c[7] >= 0 for c in o["c"])
    ev.attach(hello.get("x", X_RANGE), hello.get("y", Y_RANGE), msc)
    slots: dict[int, int] = {}
    for f in frames:
        t = f["t"]
        now = {c[0] for c in f["c"]}
        for cid in [c for c in slots if c not in now]:
            ev.ev(t, EV_ABS, SLOT, slots.pop(cid))
            ev.ev(t, EV_ABS, TID, -1)
        for c in f["c"]:
            cid, x, y, major, minor = c[:5]
            if cid not in slots:
                slots[cid] = next(i for i in range(SLOTS) if i not in slots.values())
                ev.ev(t, EV_ABS, SLOT, slots[cid])
                ev.ev(t, EV_ABS, TID, cid)
            else:
                ev.ev(t, EV_ABS, SLOT, slots[cid])
            for code, v in ((X, x), (Y, y), (MAJOR, major), (MINOR, minor)):
                ev.ev(t, EV_ABS, code, v)
            if len(c) > 7 and c[7] >= 0:
                ev.ev(t, EV_MSC, MSC_RAW, c[7] << 4)
        ev.ev(t, EV_SYN, SYN_REPORT, 0)
    return ev


class KernelMT:
    """What hid-magicmouse plus the input core make of the HID reports (MM1, report 0x29):
    per contact slot = id, input_mt_report_slot_state (new tracking id when a slot goes active,
    -1 when its state is TOUCH_STATE_NONE), major << 2, minor << 2, -orientation, x, y, MSC_RAW
    tdata[7] (report_undeciphered=1); then buttons (clicks & 3, no three-button emulation: the
    recording has no click), REL_X, REL_Y, SYN_REPORT. The input core drops unchanged values per
    slot (and with them the slot switch), zero REL values and frames with nothing in them."""

    def __init__(self, ev: Events) -> None:
        self.ev = ev
        self.slot = 0  # the slot the kernel last sent (ABS_MT_SLOT's value)
        self.vals: dict[int, dict[int, int]] = {}
        self.keys: dict[int, int] = {}
        self.trkid = 0
        self.pending: list[tuple[int, int, int]] = []

    def mt(self, slot, code, value) -> None:
        cur = self.vals.setdefault(slot, {TID: -1})
        if cur.get(code, 0) == value:
            return
        cur[code] = value
        if slot != self.slot:
            self.pending.append((EV_ABS, SLOT, slot))
            self.slot = slot
        self.pending.append((EV_ABS, code, value))

    def report(self, ts, head: bytes, contacts: list) -> None:
        for c in contacts:
            raw = bytes.fromhex(c["raw"])
            slot, down = c["id"], (raw[7] & 0xF0) != 0
            if down:
                tid = self.vals.get(slot, {}).get(TID, -1)
                if tid < 0:
                    tid = self.trkid
                    self.trkid = (self.trkid + 1) & 0xFFFF
                self.mt(slot, TID, tid)
                self.mt(slot, MAJOR, c["major"] << 2)
                self.mt(slot, MINOR, c["minor"] << 2)
                self.mt(slot, ORIENT, -c["orient"])
                self.mt(slot, X, c["x"])
                self.mt(slot, Y, c["y"])
                self.pending.append((EV_MSC, MSC_RAW, raw[7]))
            else:
                self.mt(slot, TID, -1)
        s10 = lambda v: v - 0x400 if v & 0x200 else v
        dx = s10(((head[3] & 0x0C) << 6) | head[1])
        dy = s10(((head[3] & 0x30) << 4) | head[2])
        clicks = head[3] & 3
        for code, bit in ((BTN_LEFT, 1), (BTN_RIGHT, 2)):
            v = 1 if clicks & bit else 0
            if self.keys.get(code, 0) != v:
                self.keys[code] = v
                self.pending.append((EV_KEY, code, v))
        if dx:
            self.pending.append((EV_REL, REL_X, dx))
        if dy:
            self.pending.append((EV_REL, REL_Y, dy))
        if self.pending:
            for typ, code, v in self.pending:
                self.ev.ev(ts, typ, code, v)
            self.ev.ev(ts, EV_SYN, SYN_REPORT, 0)
            self.pending = []


def from_hidraw(path: str) -> Events:
    """raw-rec.py: hidraw reports of a Magic Mouse 1, decoded per contact. See KernelMT."""
    ev = Events()
    meta = {}
    reports = []
    for ln in open(path):
        o = json.loads(ln)
        if o.get("type") == "meta":
            meta = o
        elif "head" in o:
            reports.append(o)
    if meta.get("product", 0x030D) != 0x030D:
        raise SystemExit(f"{path}: only Magic Mouse 1 reports are converted")
    ev.attach(X_RANGE, Y_RANGE, True)
    k = KernelMT(ev)
    for r in reports:
        k.report(r["t"], bytes.fromhex(r["head"]), r["c"])
    return ev


def from_evdev(path: str) -> Events:
    """mm-record.py: evdev events [ts, type, code, value] as read (EV_ABS, EV_KEY, EV_REL, EV_SYN;
    no EV_MSC). Fed as they are; phase markers are skipped."""
    ev = Events()
    ev.attach(X_RANGE, Y_RANGE, False)
    for ln in open(path):
        o = json.loads(ln)
        if isinstance(o, list):
            ev.ev(*o)
    return ev


def guess_kind(path: str) -> str:
    with open(path) as f:
        for ln in f:
            o = json.loads(ln)
            if isinstance(o, list):
                return "evdev"
            if o.get("type") == "hello" or ("c" in o and "head" not in o):
                return "probe"
            if "head" in o or o.get("type") == "meta":
                return "hidraw"
    raise SystemExit(f"{path}: unknown recording format")


CONVERT = {"probe": from_probe, "hidraw": from_hidraw, "evdev": from_evdev}


# --------------------------------------------------------------------------- synthetic cases
LEGACY = "[touchpad]\nenabled = false\n"


def s_legacy_scroll():
    """One finger: a slow scroll, a fast flick that coasts (ticks), a scroll the next finger stops,
    a sideways flick (page back), a horizontal scroll."""
    f = Frames()
    a = f.tid()
    f.touch([a], [(0, -300)], 4)
    end = f.slide([a], [(0, -300)], (0, 20), 25)  # ~ 0.5 mm per frame
    f.lift([a])
    f.idle(0.4)
    b = f.tid()
    f.touch([b], [(100, -400)], 3)
    f.slide([b], [(100, -400)], (0, 30), 3)
    f.slide([b], [(100, -310)], (0, 120), 10)  # fast: a flick
    f.lift([b])
    f.idle(0.5)  # coasting
    c = f.tid()
    f.touch([c], [(0, 200)], 5)  # stops the glide
    f.lift([c])
    f.idle(0.6)
    d = f.tid()
    f.touch([d], [(-200, 0)], 3)
    f.slide([d], [(-200, 0)], (110, 4), 8)  # sideways flick: back/forward
    f.lift([d])
    f.idle(0.8)
    e2 = f.tid()
    f.touch([e2], [(-400, 300)], 4)
    f.slide([e2], [(-400, 300)], (25, 3), 30)  # horizontal scroll
    f.slide([e2], [(350, 390)], (0, -25), 20)  # and back to vertical in the same scroll
    f.lift([e2])
    f.idle(3.0)
    return f


def s_long_coast():
    """A hard flick with a long glide: max_seconds ends it, not the decay."""
    f = Frames()
    a = f.tid()
    f.touch([a], [(0, -800)], 4)
    f.slide([a], [(0, -800)], (0, 180), 8)
    f.lift([a])
    f.idle(3.0)
    b = f.tid()  # and a flick the mouse's own motion swallows
    f.touch([b], [(0, -800)], 4)
    for i in range(10):
        f.frame([(b, 0, -800 + 150 * i)], rel=(6, -4))
    f.lift([b])
    return f


def s_two_finger():
    """Two fingers: taps, double taps, two-finger swipes left and right, a two-finger scroll."""
    f = Frames()
    for n in (1, 1, 2, 2):  # a double tap with one finger, then with two
        ids = [f.tid() for _ in range(n)]
        f.touch(ids, [(-150 + 300 * i, 0) for i in range(n)], 6)
        f.lift(ids)
        f.idle(0.15)
    f.idle(0.6)
    for dx in (-60, 60):  # swipe left, then right
        a, b = f.tid(), f.tid()
        f.touch([a, b], [(-150, 100), (150, 100)], 4)
        f.slide([a, b], [(-150, 100), (150, 100)], (dx, 2), 14)
        f.lift([a, b])
        f.idle(0.6)
    a, b = f.tid(), f.tid()
    f.touch([a, b], [(-150, -200), (150, -200)], 4)
    f.slide([a, b], [(-150, -200), (150, -200)], (1, 45), 20)  # two-finger scroll
    f.lift([a, b])
    f.idle(3.0)
    return f


def s_edges():
    """Fingers on the edges and the palm zone: holding the mouse, an edge finger that slides
    (released), a palm, a tracking-id flicker of a scrolling finger, a slot reused with a jump."""
    f = Frames()
    g = f.tid()
    p = f.tid()
    f.touch([g, p], [(-1080, 0), (0, 1900)], 6)  # left edge + palm
    for i in range(30):  # the edge finger slides straight on
        f.frame([(g, -1080, 25 * i), (p, 0, 1900)])
    f.lift([g, p])
    f.idle(0.5)
    a = f.tid()
    f.touch([a], [(200, -500)], 4)
    pos = f.slide([a], [(200, -500)], (0, 40), 10)
    f.lift([a], linger=0)  # a flicker: back within BRIDGE_MS with a new id
    b = f.tid()
    f.slide([b], [pos[0]], (0, 40), 10)
    f.frame([(b, pos[0][0] + 600, pos[0][1] + 400)])  # same slot, same id, a jump: a new finger
    f.touch([b], [(pos[0][0] + 600, pos[0][1] + 400)], 3)
    f.lift([b])
    f.idle(0.5)
    h = f.tid()  # a hovering finger: small and slow reports, never a touch
    for i in range(20):
        f.frame([(h, 0, -100 + 20 * i, 200, 120)], dt=0.03)
    f.frame([(h, 0, 300, 600, 0)])  # no width: a hover over the edge
    f.lift([h])
    f.idle(2.0)
    return f


def s_click():
    """Pointer motion at several speeds, left, right and middle clicks, a drag, click creep."""
    f = Frames()
    for i in range(40):
        f.frame(rel=(1 + i % 3, -(i % 2)), dt=0.011)
    for i in range(30):
        f.frame(rel=(12 + i, 8), dt=0.011)
    f.idle(0.3)
    for i in range(10):
        f.frame(rel=(-3, 2), dt=0.004 if i % 2 else 0.011)  # burst frames
    f.idle(0.5)
    for btn in (BTN_LEFT, BTN_RIGHT, BTN_MIDDLE):
        f.frame(keys=[(btn, 1)], rel=(1, 0))
        f.frame(rel=(2, 1))  # creep, swallowed
        f.frame(rel=(1, 1))
        f.frame(keys=[(btn, 0)])
        f.idle(0.4)
    f.frame(keys=[(BTN_LEFT, 1)])  # a drag
    for i in range(20):
        f.frame(rel=(5, 3))
    f.frame(keys=[(BTN_LEFT, 0)])
    f.idle(0.4)
    a = f.tid()  # a click with a finger on: no scroll from the rolling finger
    f.touch([a], [(0, -300)], 4)
    f.frame([(a, 0, -300)], keys=[(BTN_LEFT, 1)])
    f.slide([a], [(0, -300)], (0, 30), 8)
    f.frame([(a, 0, -60)], keys=[(BTN_LEFT, 0)])
    f.lift([a])
    f.idle(1.0)
    return f


def s_control_click():
    """Control + click: Hyprland's event while the button is held (latched), after a quick click,
    a stale one, and one with no press of ours."""
    f = Frames()
    f.frame(keys=[(BTN_LEFT, 1)])
    f.control_click(f.t + 0.02)
    f.t = round(f.t + 0.03, 6)
    f.frame()
    f.frame(keys=[(BTN_LEFT, 0)])
    f.idle(0.5)
    f.frame(keys=[(BTN_LEFT, 1)])
    f.frame(keys=[(BTN_LEFT, 0)])
    f.control_click(f.t + 0.01)  # after the release
    f.t = round(f.t + 0.02, 6)
    f.idle(0.5)
    f.frame(keys=[(BTN_LEFT, 1)])
    f.frame(keys=[(BTN_LEFT, 0)])
    f.idle(1.5)
    f.control_click(f.t)  # stale
    f.t = round(f.t + 0.01, 6)
    f.frame()
    f.control_click(f.t + 0.5)  # no press at all
    f.t = round(f.t + 0.6, 6)
    f.frame(keys=[(BTN_RIGHT, 1)])
    f.frame(keys=[(BTN_RIGHT, 0)])
    f.idle(0.5)
    return f


def s_syn_dropped():
    """SYN_DROPPED: skip to SYN_REPORT, resync from the device (M line: what the kernel holds then).
    First with the finger still down and the button still held, then with both gone in the gap."""
    f = Frames()
    a = f.tid()
    f.touch([a], [(0, -300)], 4)
    f.slide([a], [(0, -300)], (0, 30), 6)
    f.frame([(a, 0, -120)], keys=[(BTN_LEFT, 1)])
    f.device_state(0, [(0, a, 0, -90, 600, 400)], [BTN_LEFT])
    f.frame([(a, 0, -100)], rel=(3, 3), syn=SYN_DROPPED, dt=0.0)
    f.frame([(a, 0, -90)], rel=(3, 3))  # dropped events, then the SYN_REPORT that resyncs
    f.frame([(a, 0, -80)])
    f.slide([a], [(0, -80)], (0, 30), 8)
    f.frame([(a, 0, 160)], keys=[(BTN_LEFT, 0)])
    f.idle(0.3)
    f.frame([(a, 0, 170)], keys=[(BTN_LEFT, 1)])
    f.device_state(0, [], [])
    f.frame([(a, 0, 180)], syn=SYN_DROPPED, dt=0.0)
    f.slots.clear()
    f.last.clear()
    f.frame(rel=(1, 0))  # finger lifted and button released while events were lost
    f.frame(rel=(1, 0))
    b = f.tid()
    f.touch([b], [(0, -300)], 4)
    f.slide([b], [(0, -300)], (0, 30), 8)
    f.lift([b])
    f.idle(1.0)
    return f


def s_touchpad():
    """Touchpad mode: one-finger scrolls (catch-up, play, a held light report), a two-finger swipe,
    two fingers scrolling up and down, a scroll a second finger cannot join, a click pause."""
    f = Frames()
    a = f.tid()
    f.touch([a], [(0, -300)], 4)
    pos = f.slide([a], [(0, -300)], (0, 30), 15)
    f.slide([a], pos, (0, -10), 6)  # turn: the play eats it first
    f.frame([(a, 0, pos[0][1] - 60, 200, 100)])  # too light for a moment
    f.slide([a], [(0, pos[0][1] - 70)], (0, 25), 8)
    f.lift([a])
    f.idle(0.5)
    a, b = f.tid(), f.tid()
    f.touch([a, b], [(-200, 100), (150, 120)], 3)
    f.slide([a, b], [(-200, 100), (150, 120)], (55, 3), 15)  # swipe
    f.lift([a])  # one lifts: the swipe is over
    f.touch([b], [(975, 165)], 5)
    f.lift([b])
    f.idle(0.5)
    a, b = f.tid(), f.tid()
    f.touch([a, b], [(-200, -300), (150, -300)], 3)
    f.slide([a, b], [(-200, -300), (150, -300)], (2, 40), 15)  # two fingers up and down: one scrolls
    f.lift([a, b])
    f.idle(0.5)
    a = f.tid()
    f.touch([a], [(-100, -400)], 3)
    f.slide([a], [(-100, -400)], (0, 35), 12)
    b = f.tid()
    f.slide([a, b], [(-100, 20), (300, 0)], (0, 35), 10)  # late second finger: ignored
    f.lift([a, b])
    f.idle(0.3)
    f.frame(keys=[(BTN_LEFT, 1)])
    f.frame(keys=[(BTN_LEFT, 0)])
    a = f.tid()
    f.touch([a], [(0, 0)], 2)
    f.slide([a], [(0, 0)], (0, 30), 20)  # starts only after the click pause
    f.lift([a])
    f.idle(0.5)
    a = f.tid()  # scrolling while the mouse moves
    f.touch([a], [(0, -300)], 3)
    for i in range(15):
        f.frame([(a, 0, -300 + 15 * i)], rel=(4, 2))
    for i in range(15):
        f.frame([(a, 0, -75 + 60 * i)], rel=(2, 1))
    f.lift([a])
    f.idle(1.0)
    return f


def s_touchpad_fw():
    """Touchpad mode with the mouse's own touch state: hover states, the firmware swipe pair,
    a pair that comes in from the edge, a lift through break/linger states."""
    f = Frames(msc_raw=True)
    h = f.tid()
    for i in range(8):  # hovering (state 2), big: still not a touch
        f.frame([(h, 0, -200 + 15 * i, 700, 400, 2)])
    for st in (3, 4, 4, 4):
        f.frame([(h, 0, -80, 700, 400, st)])
    pos = f.slide([h], [(0, -80)], (0, 35), 12, extra=(700, 400, 4))
    for st in (5, 6, 6, 7):
        f.frame([(h, *pos[0], 300, 200, st)])
    f.lift([h])
    f.idle(0.5)
    a, b = f.tid(), f.tid()
    for st in (2, 3):
        f.frame([(a, -250, 100, 600, 400, st), (b, 120, 130, 600, 400, st)])
    f.slide([a, b], [(-250, 100), (120, 130)], (-45, 2), 12, extra=(600, 400, 4))  # light swipe left
    for st in (5, 6):
        f.frame([(a, -790, 124, 300, 200, st), (b, -420, 154, 300, 200, st)])
    f.lift([a, b])
    f.idle(0.5)
    a, b = f.tid(), f.tid()  # one of them from the left edge
    f.frame([(a, -1090, 300, 600, 400, 3), (b, -700, 320, 600, 400, 3)])
    f.slide([a, b], [(-1090, 300), (-700, 320)], (50, -1), 12, extra=(600, 400, 4))
    f.lift([a, b])
    f.idle(1.0)
    return f


SYNTHETIC = {
    # name: (builder, config toml or None, probe)
    "legacy_scroll": (s_legacy_scroll, LEGACY, False),
    "legacy_scroll_wheel": (s_legacy_scroll, LEGACY + "[scroll]\nnatural = false\ninvert_x = true\nspeed = 1.7\naccel = 0.0\nstart_mm = 0.2\n[gestures]\nswipe_pages = false\n", False),
    "legacy_free_axis": (s_legacy_scroll, LEGACY + "[scroll]\nfree_axis = true\nhorizontal = true\n[momentum]\ndecay = 0.985\n", False),
    "legacy_no_momentum": (s_legacy_scroll, LEGACY + "[momentum]\nenabled = false\n[scroll]\nhorizontal = false\n", False),
    "legacy_long_coast": (s_long_coast, LEGACY + "[momentum]\ndecay = 0.995\nmax_seconds = 2.5\nflick = 500\n", False),
    "legacy_two_finger": (s_two_finger, LEGACY + "[gestures]\ndouble_tap = \"dispatch zoom\"\ntwo_finger_double_tap = \"notify-send overview\"\n", False),
    "legacy_two_finger_custom": (s_two_finger, LEGACY + "[gestures]\ninvert_swipe = true\nswipe_mm = 6.0\ntwo_finger_swipe_left = \"notify-send left\"\nswipe_end = \"stop\"\n", False),
    "legacy_edges": (s_edges, LEGACY, True),
    "edges_touchpad": (s_edges, None, True),
    "click": (s_click, None, False),
    "click_mapped": (s_click, "[buttons]\nright_click = false\nmiddle = \"right\"\n[pointer]\ntracking_speed = 1.5\nsmoothing_ms = 10\nclick_hold_ms = 0\n", False),
    "control_click": (s_control_click, "[buttons]\ncontrol_click = true\n", False),
    "control_click_off": (s_control_click, None, False),
    "syn_dropped": (s_syn_dropped, LEGACY, False),
    "touchpad": (s_touchpad, None, True),
    "touchpad_tuned": (s_touchpad, "[touchpad]\nswipe_gain = 4.0\nswipe_reverse = true\n[scroll]\ntouch_reverse_mm = 0.0\ntouch_straight = 0.0\ntouch_start_size = 0\n", False),
    "touchpad_noswipe": (s_touchpad, "[touchpad]\nswipe = false\n", False),
    "touchpad_fw": (s_touchpad_fw, None, True),
    "touchpad_fw_off": (s_touchpad_fw, "[scroll]\ntouch_firmware = false\n", True),
    "legacy_fw": (s_touchpad_fw, LEGACY, False),
}

RECORDED = {
    # name: (file, kind)
    "rec_mm1_evdev": ("2026-10-30-mm1-raw.jsonl", "evdev"),
    "rec_flicker": ("flicker-20261101-1350.jsonl", "probe"),
    "rec_hidraw": ("raw-20261101-2055.jsonl", "hidraw"),
    "rec_swipe": ("swipe-20261101-2204.jsonl", "probe"),
}
LIVE = """# the config the recordings were made with (2026-11-01), without comments
[pointer]
tracking_speed = 0.875
[scroll]
touch_front_mm = 13
touch_front_size = 510
edge_release_mm = 5
edge_front_mm = 2
touch_pair_size = 280
touch_rate = 36
touch_hold_size = 250
touch_start_size = 280
free_axis = true
edge_back_mm = 30
edge_right_mm = 23
edge_left_mm = 5
natural = false
speed = 1.7
start_mm = 0.2
[gestures]
invert_swipe = true
swipe_pages = false
swipe_mm = 6.0
[momentum]
decay = 0.975
[touchpad]
scroll_speed = 0.2
"""
RECORDED_CONFIGS = {
    # suffix: (config, probe)
    "": (None, True),
    "_live": (LIVE, False),
    "_live_legacy": (LIVE + "enabled = false\n", False),
}


def cases(recordings: str):
    """(name, events file name, events lines, config text or None, probe) for every case.
    The configs of one recording share its events file."""
    for name, (build, cfg, probe) in SYNTHETIC.items():
        yield name, name + ".events", build().lines(), cfg, probe
    for base, (fname, kind) in RECORDED.items():
        path = os.path.join(recordings, fname)
        if not os.path.exists(path):
            print(f"skip {base}: {path} missing", file=sys.stderr)
            continue
        lines = CONVERT[kind](path).lines()
        for suffix, (cfg, probe) in RECORDED_CONFIGS.items():
            yield base + suffix, base + ".events", lines, cfg, probe


def write_text(path: str, lines: list[str]) -> None:
    with open(path, "w") as f:
        f.write("".join(l + "\n" for l in lines))


def build(out: str, recordings: str, only: list[str] | None) -> None:
    os.makedirs(out, exist_ok=True)
    manifest = []
    old = os.path.join(out, "manifest.json")
    if only and os.path.exists(old):
        manifest = [m for m in json.load(open(old)) if m["name"] not in only]
    for name, ev_name, lines, cfg, probe in cases(recordings):
        if only and name not in only:
            continue
        write_text(os.path.join(out, ev_name), lines)
        cfg_name = None
        if cfg is not None:
            cfg_name = name + ".toml"
            with open(os.path.join(out, cfg_name), "w") as f:
                f.write(cfg)
        trace = replay(lines, os.path.join(out, cfg_name) if cfg_name else None, probe)
        write_text(os.path.join(out, name + ".trace"), trace)
        kinds = {}
        for l in trace:
            kinds[l[0]] = kinds.get(l[0], 0) + 1
        manifest.append({"name": name, "events": ev_name, "trace": name + ".trace",
                         "config": cfg_name, "probe": probe,
                         "sha256": hashlib.sha256("".join(l + "\n" for l in trace).encode()).hexdigest()})
        print(f"{name:<32} {len(lines):>7} input lines  {len(trace):>7} trace lines  "
              + " ".join(f"{k}={v}" for k, v in sorted(kinds.items())), file=sys.stderr)
    manifest.sort(key=lambda m: m["name"])
    with open(old, "w") as f:
        json.dump(manifest, f, indent=1)
        f.write("\n")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build")
    b.add_argument("--out", default=OUT)
    b.add_argument("--recordings", default=RECORDINGS)
    b.add_argument("--only", nargs="*")
    r = sub.add_parser("run")
    r.add_argument("events")
    r.add_argument("--config")
    r.add_argument("--probe", action="store_true")
    c = sub.add_parser("events")
    c.add_argument("recording")
    c.add_argument("--kind", choices=sorted(CONVERT))
    a = ap.parse_args()
    if a.cmd == "build":
        build(a.out, a.recordings, a.only)
    elif a.cmd == "run":
        sys.stdout.write("".join(l + "\n" for l in replay(open(a.events), a.config, a.probe)))
    else:
        ev = CONVERT[a.kind or guess_kind(a.recording)](a.recording)
        sys.stdout.write("".join(l + "\n" for l in ev.lines()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
