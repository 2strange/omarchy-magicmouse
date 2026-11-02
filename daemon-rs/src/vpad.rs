//! The virtual touchpad of touchpad mode. Python reference: `VPad` and the VPAD_* / TP_* constants
//! (v1.4.1, ~l.1061-1197). `VPadCore` is the pure logic over any `iface::Output` (tests use a fake
//! sink); `VPad` is the real uinput device.
use std::collections::HashMap;
use std::io;

use evdev::uinput::VirtualDeviceBuilder;
use evdev::{
    AbsInfo, AbsoluteAxisType, AttributeSet, BusType, InputId, Key, PropType, UinputAbsSetup,
};

use crate::devices::{VirtualOut, EV_ABS, EV_KEY, VPAD_NAME};
use crate::iface::{Output, TouchpadOut};

pub const VPAD_HYPR: &str = "magic-mouse-virtual-touchpad";
/// The virtual surface is this many times the mouse, so a swipe has room (max swipe_gain).
pub const VPAD_GROW: f64 = 5.0;
pub const TP_PAIR_FRAMES: u32 = 3;
pub const TP_PAIR_YOUNG: f64 = 0.15;
pub const TP_SWIPE_RATIO: f64 = 1.5;
pub const TP_HOLD_SEC: f64 = 0.1;
/// One finger moves this much farther on the virtual touchpad (scroll_factor divided by it).
pub const TP_SCROLL_GAIN: f64 = 2.0;
pub const TP_CATCH_UP: f64 = 0.5;
/// The added finger sits this far beside the first one.
pub const VPAD_EXTRA_MM: f64 = 12.0;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_TOUCH: u16 = 0x14a;
pub const BTN_TOOL_FINGER: u16 = 0x145;
pub const BTN_TOOL_DOUBLETAP: u16 = 0x14d;
pub const BTN_TOOL_TRIPLETAP: u16 = 0x14e;
pub const BTN_TOOL_QUADTAP: u16 = 0x14f;
pub const BTN_TOOL_QUINTTAP: u16 = 0x148;
pub const VPAD_TOOLS: [u16; 5] = [
    BTN_TOOL_FINGER,
    BTN_TOOL_DOUBLETAP,
    BTN_TOOL_TRIPLETAP,
    BTN_TOOL_QUADTAP,
    BTN_TOOL_QUINTTAP,
];

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;

/// Python `grow(r)`: centre +- half span * VPAD_GROW, truncated by int().
pub fn grow(r: (i32, i32)) -> (i32, i32) {
    let c = (r.0 as f64 + r.1 as f64) / 2.0;
    let h = (r.1 as f64 - r.0 as f64) / 2.0;
    ((c - h * VPAD_GROW) as i32, (c + h * VPAD_GROW) as i32)
}

/// Python `int(round(res))` (round half to even).
pub fn abs_resolution(res: f64) -> i32 {
    res.round_ties_even() as i32
}

/// Python min(max(v, lo), hi).
fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    let v = if lo > v { lo } else { v };
    if hi < v {
        hi
    } else {
        v
    }
}

fn next_id(n: &mut i32) -> i32 {
    *n = *n % 65000 + 1;
    *n
}

/// The finger bookkeeping and event stream of Python's `VPad`, writing to any `Output`.
pub struct VPadCore<O: Output> {
    pub out: O,
    pub xr: (i32, i32),
    pub yr: (i32, i32),
    pub res: f64,
    /// virtual tracking id -> slot, in insertion order (Python dict order decides the lift order)
    slots: Vec<(i32, i32)>,
    /// real tracking id -> virtual id
    ids: HashMap<i32, i32>,
    next_id: i32,
    extra: Option<i32>,
    extra_pos: Option<(f64, f64)>,
    count: usize,
    gain: f64,
    /// the gain sideways, negative while a swipe is mirrored
    gx: f64,
    /// real tid -> real x, y, virtual x, y
    anchor: HashMap<i32, (f64, f64, f64, f64)>,
    last: HashMap<i32, (f64, f64)>,
}

impl<O: Output> VPadCore<O> {
    /// xr, yr: the mouse's touch ranges (grown here); res: units per mm.
    pub fn new(out: O, xr: (i32, i32), yr: (i32, i32), res: f64) -> Self {
        Self {
            out,
            xr: grow(xr),
            yr: grow(yr),
            res,
            slots: Vec::new(),
            ids: HashMap::new(),
            next_id: 0,
            extra: None,
            extra_pos: None,
            count: 0,
            gain: 1.0,
            gx: 1.0,
            anchor: HashMap::new(),
            last: HashMap::new(),
        }
    }

    fn new_id(&mut self) -> i32 {
        next_id(&mut self.next_id)
    }

    fn slot_of(&self, vid: i32) -> Option<i32> {
        self.slots.iter().find(|(v, _)| *v == vid).map(|(_, s)| *s)
    }

    /// fingers: (real tracking id, x, y) to put down; [] lifts everything.
    /// No virtual finger ever jumps: a finger keeps its virtual place, a new one lands
    /// relative to a finger that was already down, and the added finger moves along with it.
    /// mirror: a swipe (two or more fingers) moves the other way sideways.
    pub fn update(&mut self, fingers: &[(i32, f64, f64)], gain: f64, mirror: bool) {
        let (x0r, x1r) = (self.xr.0 as f64, self.xr.1 as f64);
        let (y0r, y1r) = (self.yr.0 as f64, self.yr.1 as f64);
        let gain = if fingers.len() >= 2 {
            let g = if 1.0 > gain { 1.0 } else { gain };
            if VPAD_GROW < g {
                VPAD_GROW
            } else {
                g
            }
        } else {
            TP_SCROLL_GAIN
        };
        let gx = if mirror && fingers.len() >= 2 {
            -gain
        } else {
            gain
        };
        if (gain, gx) != (self.gain, self.gx) {
            // re-anchor, so no finger jumps on a change
            self.anchor.clear();
            self.gain = gain;
        }
        self.gx = gx;
        let prev = self.last.clone();
        let gone: Vec<i32> = self
            .last
            .keys()
            .copied()
            .filter(|t| !fingers.iter().any(|f| f.0 == *t))
            .collect();
        for tid in gone {
            self.last.remove(&tid);
            self.anchor.remove(&tid);
        }
        // down in the last frame too
        let lead = fingers
            .iter()
            .copied()
            .find(|(t, _, _)| prev.contains_key(t));
        let mut want: Vec<(i32, f64, f64)> = Vec::new();
        for &(tid, x, y) in fingers {
            let next = &mut self.next_id;
            self.ids.entry(tid).or_insert_with(|| next_id(next));
            self.anchor.entry(tid).or_insert_with(|| {
                let (vx, vy) = if let Some(&p) = prev.get(&tid) {
                    p
                } else if let Some((lt, lx, ly)) = lead {
                    let (lvx, lvy) = prev[&lt];
                    (lvx + (x - lx), lvy + (y - ly))
                } else {
                    (x, y) // a fresh landing: where it really is, in the middle of the bigger surface
                };
                (x, y, vx, vy)
            });
            let (ax, ay, avx, avy) = self.anchor[&tid];
            let vx = clamp(avx + (x - ax) * gx, x0r, x1r);
            let vy = clamp(avy + (y - ay) * gain, y0r, y1r);
            self.last.insert(tid, (vx, vy));
            want.push((self.ids[&tid], vx, vy));
        }
        if !want.is_empty() {
            match (self.extra, self.extra_pos, lead) {
                (Some(_), Some((ex, ey)), Some((lt, _, _))) => {
                    let (lvx, lvy) = prev[&lt];
                    let (nvx, nvy) = self.last[&lt];
                    self.extra_pos = Some((ex + nvx - lvx, ey + nvy - lvy)); // rides along with the lead finger
                }
                _ => {
                    self.extra = Some(self.new_id()); // a new touch: the added finger lands too
                    let (_, x0, y0) = want[0];
                    let dx = VPAD_EXTRA_MM * self.res;
                    self.extra_pos = Some((if x0 + dx <= x1r { x0 + dx } else { x0 - dx }, y0));
                }
            }
            let (ex, ey) = self.extra_pos.unwrap();
            want.push((
                self.extra.unwrap(),
                clamp(ex, x0r, x1r),
                clamp(ey, y0r, y1r),
            ));
        } else {
            self.extra = None;
            self.extra_pos = None;
        }
        let keep = |v: i32| want.iter().any(|w| w.0 == v);
        let mut i = 0;
        while i < self.slots.len() {
            // lifts
            let (vid, slot) = self.slots[i];
            if keep(vid) {
                i += 1;
                continue;
            }
            self.slots.remove(i);
            self.out.write(EV_ABS, ABS_MT_SLOT, slot);
            self.out.write(EV_ABS, ABS_MT_TRACKING_ID, -1);
        }
        self.ids.retain(|_, v| keep(*v));
        for &(vid, x, y) in &want {
            let slot = match self.slot_of(vid) {
                Some(s) => s,
                None => {
                    let Some(s) = (0..10).find(|i| !self.slots.iter().any(|(_, s)| s == i)) else {
                        continue; // more than ten fingers: Python would raise here
                    };
                    self.slots.push((vid, s));
                    s
                }
            };
            self.out.write(EV_ABS, ABS_MT_SLOT, slot);
            self.out.write(EV_ABS, ABS_MT_TRACKING_ID, vid);
            self.out.write(EV_ABS, ABS_MT_POSITION_X, x as i32);
            self.out.write(EV_ABS, ABS_MT_POSITION_Y, y as i32);
        }
        let n = want.len().min(VPAD_TOOLS.len());
        if n != self.count {
            if self.count > 0 {
                self.out.write(EV_KEY, VPAD_TOOLS[self.count - 1], 0);
            }
            if n > 0 {
                self.out.write(EV_KEY, VPAD_TOOLS[n - 1], 1);
            }
            self.out.write(EV_KEY, BTN_TOUCH, i32::from(n > 0));
            self.count = n;
        }
        if let Some(&(_, x, y)) = want.first() {
            self.out.write(EV_ABS, ABS_X, x as i32);
            self.out.write(EV_ABS, ABS_Y, y as i32);
        }
        self.out.syn();
    }
}

impl<O: Output> TouchpadOut for VPadCore<O> {
    fn update(&mut self, fingers: &[(i32, f64, f64)], gain: f64, mirror: bool) {
        VPadCore::update(self, fingers, gain, mirror);
    }
}

/// The real virtual touchpad (uinput). Dropping it lifts every finger first (Python `close()`).
pub struct VPad {
    core: VPadCore<VirtualOut>,
}

impl VPad {
    /// xr, yr: the mouse's ABS_MT_POSITION_X/Y ranges; res: Engine.res_x. Creates the uinput device.
    pub fn new(xr: (i32, i32), yr: (i32, i32), res: f64) -> io::Result<Self> {
        let (gx, gy) = (grow(xr), grow(yr));
        let r = abs_resolution(res);
        let mut keys = AttributeSet::<Key>::new();
        for k in [BTN_LEFT, BTN_TOUCH].into_iter().chain(VPAD_TOOLS) {
            keys.insert(Key::new(k));
        }
        let mut props = AttributeSet::<PropType>::new();
        props.insert(PropType::POINTER);
        props.insert(PropType::BUTTONPAD);
        let abs = |code: u16, min: i32, max: i32, res: i32| {
            UinputAbsSetup::new(AbsoluteAxisType(code), AbsInfo::new(0, min, max, 0, 0, res))
        };
        // Bluetooth bus: libinput treats it as an external touchpad (not off with the lid, no disable-while-typing)
        let dev = VirtualDeviceBuilder::new()?
            .name(VPAD_NAME)
            .input_id(InputId::new(BusType::BUS_BLUETOOTH, 0x004C, 0x0269, 1))
            .with_keys(&keys)?
            .with_absolute_axis(&abs(ABS_X, gx.0, gx.1, r))?
            .with_absolute_axis(&abs(ABS_Y, gy.0, gy.1, r))?
            .with_absolute_axis(&abs(ABS_MT_SLOT, 0, 9, 0))?
            .with_absolute_axis(&abs(ABS_MT_TRACKING_ID, 0, 65535, 0))?
            .with_absolute_axis(&abs(ABS_MT_POSITION_X, gx.0, gx.1, r))?
            .with_absolute_axis(&abs(ABS_MT_POSITION_Y, gy.0, gy.1, r))?
            .with_properties(&props)?
            .build()?;
        Ok(Self {
            core: VPadCore::new(VirtualOut::new(dev), xr, yr, res),
        })
    }
}

impl TouchpadOut for VPad {
    fn update(&mut self, fingers: &[(i32, f64, f64)], gain: f64, mirror: bool) {
        self.core.update(fingers, gain, mirror);
    }
}

impl Drop for VPad {
    fn drop(&mut self) {
        self.core.update(&[], 1.0, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Sink {
        ev: Vec<(u16, u16, i32)>,
        frames: Vec<Vec<(u16, u16, i32)>>,
    }
    impl Output for Sink {
        fn write(&mut self, kind: u16, code: u16, value: i32) {
            self.ev.push((kind, code, value));
        }
        fn syn(&mut self) {
            self.frames.push(std::mem::take(&mut self.ev));
        }
    }

    // Magic Mouse 2 ranges (hid-magicmouse): X -1100..1258, Y -1589..2047
    fn pad() -> VPadCore<Sink> {
        VPadCore::new(Sink::default(), (-1100, 1258), (-1589, 2047), 38.0)
    }
    fn last(p: &VPadCore<Sink>) -> &Vec<(u16, u16, i32)> {
        p.out.frames.last().unwrap()
    }
    const A: u16 = EV_ABS;
    const K: u16 = EV_KEY;

    #[test]
    fn grow_and_resolution() {
        // c = 79, h = 1179 -> 79 -+ 5895
        assert_eq!(grow((-1100, 1258)), (-5816, 5974));
        assert_eq!(grow((-1589, 2047)), (-8861, 9319));
        assert_eq!(grow((0, 3)), (-6, 9)); // 1.5 -+ 7.5 -> int(-6.0), int(9.0)
        assert_eq!(grow((0, 1)), (-2, 3)); // 0.5 -+ 2.5 -> int(-2.0), int(3.0)
        assert_eq!(abs_resolution(38.0), 38);
        assert_eq!(abs_resolution(26.5), 26); // Python round(): half to even
        assert_eq!(abs_resolution(27.5), 28);
    }

    #[test]
    fn single_finger_lands_with_extra() {
        let mut p = pad();
        p.update(&[(7, 100.0, 200.0)], 3.0, false);
        // fresh landing at its real place; extra finger 12 mm * 38 to the right
        assert_eq!(
            last(&p),
            &vec![
                (A, ABS_MT_SLOT, 0),
                (A, ABS_MT_TRACKING_ID, 1),
                (A, ABS_MT_POSITION_X, 100),
                (A, ABS_MT_POSITION_Y, 200),
                (A, ABS_MT_SLOT, 1),
                (A, ABS_MT_TRACKING_ID, 2),
                (A, ABS_MT_POSITION_X, 556),
                (A, ABS_MT_POSITION_Y, 200),
                (K, BTN_TOOL_DOUBLETAP, 1),
                (K, BTN_TOUCH, 1),
                (A, ABS_X, 100),
                (A, ABS_Y, 200),
            ]
        );
    }

    #[test]
    fn one_finger_moves_with_scroll_gain_and_extra_rides_along() {
        let mut p = pad();
        p.update(&[(7, 100.0, 200.0)], 3.0, false);
        p.update(&[(7, 110.0, 195.0)], 3.0, false); // one finger: gain is TP_SCROLL_GAIN
        assert_eq!(
            last(&p),
            &vec![
                (A, ABS_MT_SLOT, 0),
                (A, ABS_MT_TRACKING_ID, 1),
                (A, ABS_MT_POSITION_X, 120),
                (A, ABS_MT_POSITION_Y, 190),
                (A, ABS_MT_SLOT, 1),
                (A, ABS_MT_TRACKING_ID, 2),
                (A, ABS_MT_POSITION_X, 576),
                (A, ABS_MT_POSITION_Y, 190),
                (A, ABS_X, 120),
                (A, ABS_Y, 190),
            ]
        );
    }

    #[test]
    fn extra_lands_left_near_the_edge() {
        let mut p = pad();
        p.update(&[(1, 5900.0, 0.0)], 1.0, false);
        let f = last(&p);
        assert_eq!(f[6], (A, ABS_MT_POSITION_X, 5900 - 456));
    }

    #[test]
    fn clamped_to_the_grown_surface() {
        let mut p = pad();
        p.update(&[(1, 0.0, 0.0)], 1.0, false);
        p.update(&[(1, 10000.0, -20000.0)], 1.0, false);
        let f = last(&p);
        assert_eq!(f[2], (A, ABS_MT_POSITION_X, 5974));
        assert_eq!(f[3], (A, ABS_MT_POSITION_Y, -8861));
    }

    #[test]
    fn second_finger_lands_relative_to_lead_and_regain_reanchors() {
        let mut p = pad();
        p.update(&[(1, 0.0, 0.0)], 3.0, false);
        p.update(&[(1, 10.0, 0.0)], 3.0, false); // virtual 20
                                                 // two fingers: gain 3, re-anchored at the virtual places; finger 2 lands 50 right of the lead
        p.update(&[(1, 10.0, 0.0), (2, 60.0, 0.0)], 3.0, false);
        let f = last(&p);
        assert_eq!(
            f,
            &vec![
                (A, ABS_MT_SLOT, 0),
                (A, ABS_MT_TRACKING_ID, 1),
                (A, ABS_MT_POSITION_X, 20),
                (A, ABS_MT_POSITION_Y, 0),
                (A, ABS_MT_SLOT, 2),
                (A, ABS_MT_TRACKING_ID, 3),
                (A, ABS_MT_POSITION_X, 70),
                (A, ABS_MT_POSITION_Y, 0),
                (A, ABS_MT_SLOT, 1),
                (A, ABS_MT_TRACKING_ID, 2),
                (A, ABS_MT_POSITION_X, 476),
                (A, ABS_MT_POSITION_Y, 0),
                (K, BTN_TOOL_DOUBLETAP, 0),
                (K, BTN_TOOL_TRIPLETAP, 1),
                (K, BTN_TOUCH, 1),
                (A, ABS_X, 20),
                (A, ABS_Y, 0),
            ]
        );
        // mirrored swipe: x moves the other way, times the gain
        p.update(&[(1, 10.0, 0.0), (2, 60.0, 0.0)], 3.0, true); // gx changes: re-anchor, no jump
        p.update(&[(1, 12.0, 1.0), (2, 62.0, 1.0)], 3.0, true);
        let f = last(&p);
        assert_eq!(f[2], (A, ABS_MT_POSITION_X, 14));
        assert_eq!(f[3], (A, ABS_MT_POSITION_Y, 3));
        assert_eq!(f[6], (A, ABS_MT_POSITION_X, 64));
        assert_eq!(f[10], (A, ABS_MT_POSITION_X, 476 - 6));
        // gain is capped at VPAD_GROW and floored at 1
        p.update(&[(1, 12.0, 1.0), (2, 62.0, 1.0)], 9.0, false);
        p.update(&[(1, 13.0, 1.0), (2, 63.0, 1.0)], 9.0, false);
        assert_eq!(last(&p)[2], (A, ABS_MT_POSITION_X, 14 + 5));
        p.update(&[(1, 13.0, 1.0), (2, 63.0, 1.0)], 0.2, false);
        p.update(&[(1, 14.0, 1.0), (2, 64.0, 1.0)], 0.2, false);
        assert_eq!(last(&p)[2], (A, ABS_MT_POSITION_X, 20));
    }

    #[test]
    fn lift_one_then_all() {
        let mut p = pad();
        p.update(&[(1, 0.0, 0.0), (2, 50.0, 0.0)], 1.0, false);
        // slots: vid1->0, vid2->1, extra vid3->2
        p.update(&[(2, 50.0, 0.0)], 1.0, false);
        assert_eq!(
            last(&p),
            &vec![
                (A, ABS_MT_SLOT, 0),
                (A, ABS_MT_TRACKING_ID, -1),
                (A, ABS_MT_SLOT, 1),
                (A, ABS_MT_TRACKING_ID, 2),
                (A, ABS_MT_POSITION_X, 50),
                (A, ABS_MT_POSITION_Y, 0),
                (A, ABS_MT_SLOT, 2),
                (A, ABS_MT_TRACKING_ID, 3),
                (A, ABS_MT_POSITION_X, 456),
                (A, ABS_MT_POSITION_Y, 0),
                (K, BTN_TOOL_TRIPLETAP, 0),
                (K, BTN_TOOL_DOUBLETAP, 1),
                (K, BTN_TOUCH, 1),
                (A, ABS_X, 50),
                (A, ABS_Y, 0),
            ]
        );
        p.update(&[], 1.0, false);
        assert_eq!(
            last(&p),
            &vec![
                (A, ABS_MT_SLOT, 1),
                (A, ABS_MT_TRACKING_ID, -1),
                (A, ABS_MT_SLOT, 2),
                (A, ABS_MT_TRACKING_ID, -1),
                (K, BTN_TOOL_DOUBLETAP, 0),
                (K, BTN_TOUCH, 0),
            ]
        );
        // a new touch gets new virtual ids and the lowest free slots
        p.update(&[(9, 0.0, 0.0)], 1.0, false);
        let f = last(&p);
        assert_eq!(f[0..2], [(A, ABS_MT_SLOT, 0), (A, ABS_MT_TRACKING_ID, 4)]);
        assert_eq!(f[4..6], [(A, ABS_MT_SLOT, 1), (A, ABS_MT_TRACKING_ID, 5)]);
    }

    #[test]
    fn replaced_finger_without_lead_gets_new_extra() {
        let mut p = pad();
        p.update(&[(1, 0.0, 0.0)], 1.0, false); // vid 1, extra 2
        p.update(&[(5, 30.0, 0.0)], 1.0, false); // no lead: fresh landing, new extra 4
        let f = last(&p);
        assert_eq!(
            f[..4],
            [
                (A, ABS_MT_SLOT, 0),
                (A, ABS_MT_TRACKING_ID, -1),
                (A, ABS_MT_SLOT, 1),
                (A, ABS_MT_TRACKING_ID, -1),
            ]
        );
        assert_eq!(
            f[4..8],
            [
                (A, ABS_MT_SLOT, 0),
                (A, ABS_MT_TRACKING_ID, 3),
                (A, ABS_MT_POSITION_X, 30),
                (A, ABS_MT_POSITION_Y, 0)
            ]
        );
        assert_eq!(f[8..10], [(A, ABS_MT_SLOT, 1), (A, ABS_MT_TRACKING_ID, 4)]);
    }

    #[test]
    fn tool_keys_cap_at_five() {
        let mut p = pad();
        let f: Vec<(i32, f64, f64)> = (0..6).map(|i| (i, i as f64 * 10.0, 0.0)).collect();
        p.update(&f, 1.0, false);
        let fr = last(&p);
        assert!(fr.contains(&(K, BTN_TOOL_QUINTTAP, 1)));
        assert_eq!(fr.iter().filter(|e| e.1 == ABS_MT_SLOT).count(), 7);
    }

    #[test]
    fn id_wraps() {
        let mut p = pad();
        p.next_id = 64999;
        assert_eq!(p.new_id(), 65000);
        assert_eq!(p.new_id(), 1);
    }

    #[test]
    #[ignore = "creates a uinput device"]
    fn create_virtual_touchpad() {
        let mut v = VPad::new((-1100, 1258), (-1589, 2047), 38.0).unwrap();
        v.update(&[(1, 0.0, 0.0)], 1.0, false);
    }
}
