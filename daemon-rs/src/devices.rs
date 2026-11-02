//! Finding and grabbing Magic Mice, and the shared virtual mouse.
//! Python reference: `is_magic_mouse`, `find_mice`, `make_uinput`, `EVIOCGMTSLOTS` (v1.4.1).
use std::collections::HashSet;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use evdev::raw_stream::RawDevice;
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{
    AttributeSet, BusType, EventType, InputEvent, InputId, Key, MiscType, RelativeAxisType,
};

use crate::iface::{log, DeviceInfo, Event, Output};

pub const VIRT_NAME: &str = "magic-mouse-omarchy";
pub const VPAD_NAME: &str = "Magic Mouse virtual touchpad";
pub const APPLE_VENDORS: [u16; 2] = [0x004C, 0x05AC];
/// Magic Mouse 1, Magic Mouse 2, Magic Mouse USB-C.
pub const MAGIC_MOUSE_PRODUCTS: [u16; 3] = [0x030D, 0x0269, 0x0323];

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;
pub const SYN_REPORT: u16 = 0;
pub const MSC_RAW: u16 = 0x03;
pub const REL_X: u16 = 0x00;
pub const BTN_LEFT: u16 = 0x110;
pub const ABS_MT_SLOT: u16 = 0x2f;

// --------------------------------------------------------------------------- ioctl numbers
const IOC_READ: u64 = 2;
const IOC_WRITE: u64 = 1;

const fn ioc(dir: u64, typ: u8, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((typ as u64) << 8) | nr
}

/// EVIOCGMTSLOTS(len): read one ABS_MT_* value for every slot (Python l.105).
pub const fn eviocgmtslots(len: usize) -> u64 {
    ioc(IOC_READ, b'E', 0x0A, len)
}

/// EVIOCGABS(abs): struct input_absinfo (6 x i32) of one axis.
pub const fn eviocgabs(code: u16) -> u64 {
    ioc(
        IOC_READ,
        b'E',
        0x40 + code as u64,
        std::mem::size_of::<libc::input_absinfo>(),
    )
}

/// EVIOCGKEY(len): the key state bitmap.
pub const fn eviocgkey(len: usize) -> u64 {
    ioc(IOC_READ, b'E', 0x18, len)
}

/// HIDIOCGINPUT(len): GET_REPORT for an input report on a hidraw node.
pub const fn hidiocginput(len: usize) -> u64 {
    ioc(IOC_READ | IOC_WRITE, b'H', 0x0A, len)
}

// --------------------------------------------------------------------------- matching
/// What `is_magic_mouse` looks at, so the rule is testable without a device.
#[derive(Clone, Debug, Default)]
pub struct Caps {
    pub vendor: u16,
    pub product: u16,
    pub name: String,
    pub rel_x: bool,
    pub btn_left: bool,
    pub ev_abs: bool,
}

pub fn is_magic_mouse(c: &Caps) -> bool {
    let by_id = APPLE_VENDORS.contains(&c.vendor) && MAGIC_MOUSE_PRODUCTS.contains(&c.product);
    let by_name = c.name.to_lowercase().contains("magic mouse");
    if !(by_id || by_name) {
        return false;
    }
    c.rel_x && c.btn_left && c.ev_abs
}

/// `find_mice` skips our own virtual devices by name.
pub fn is_own_device(name: &str) -> bool {
    name == VIRT_NAME || name == VPAD_NAME
}

fn caps_of(dev: &RawDevice) -> Caps {
    let id = dev.input_id();
    Caps {
        vendor: id.vendor(),
        product: id.product(),
        name: dev.name().unwrap_or("").to_string(),
        rel_x: dev
            .supported_relative_axes()
            .is_some_and(|r| r.contains(RelativeAxisType::REL_X)),
        btn_left: dev
            .supported_keys()
            .is_some_and(|k| k.contains(Key::BTN_LEFT)),
        ev_abs: dev.supported_events().contains(EventType::ABSOLUTE),
    }
}

/// Sorted like Python's `sorted(glob("/dev/input/event*"))` (plain string order).
pub fn event_nodes(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
            .map(|e| e.path().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

// --------------------------------------------------------------------------- the grabbed mouse
/// One Magic Mouse evdev node (Python: `InputDevice`), opened O_NONBLOCK like python-evdev.
pub struct MagicMouse {
    dev: RawDevice,
    path: String,
    name: String,
    uniq: String,
    vendor: u16,
    product: u16,
    msc_raw: bool,
}

impl MagicMouse {
    pub fn open(path: &str) -> io::Result<Self> {
        let dev = RawDevice::open(path)?;
        let fd = dev.as_raw_fd();
        // SAFETY: plain fcntl on an fd we own.
        unsafe {
            let fl = libc::fcntl(fd, libc::F_GETFL);
            if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let id = dev.input_id();
        Ok(Self {
            path: path.to_string(),
            name: dev.name().unwrap_or("").to_string(),
            uniq: dev.unique_name().unwrap_or("").to_string(),
            vendor: id.vendor(),
            product: id.product(),
            msc_raw: dev
                .misc_properties()
                .is_some_and(|m| m.contains(MiscType::MSC_RAW)),
            dev,
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The Bluetooth address (Python `dev.uniq`), "" when the kernel has none.
    pub fn uniq(&self) -> &str {
        &self.uniq
    }
    pub fn vendor(&self) -> u16 {
        self.vendor
    }
    pub fn product(&self) -> u16 {
        self.product
    }
    /// For the poll loop.
    pub fn fd(&self) -> RawFd {
        self.dev.as_raw_fd()
    }
    pub fn is_magic_mouse(&self) -> bool {
        is_magic_mouse(&caps_of(&self.dev))
    }
    pub fn grab(&mut self) -> io::Result<()> {
        self.dev.grab()
    }
    pub fn ungrab(&mut self) -> io::Result<()> {
        self.dev.ungrab()
    }
    /// (min, max) of an axis, read live like Python's `dev.absinfo(code)`.
    pub fn abs_range(&self, code: u16) -> Option<(i32, i32)> {
        abs_info(self.fd(), code)
            .ok()
            .map(|a| (a.minimum, a.maximum))
    }

    /// What one read(2) returns, at most 64 events (python-evdev `read()`).
    /// Ok(empty) when nothing is pending; Err when the device is gone.
    pub fn read(&mut self) -> io::Result<Vec<Event>> {
        let mut buf = [libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_: 0,
            code: 0,
            value: 0,
        }; 64];
        let size = std::mem::size_of_val(&buf);
        // SAFETY: buf is a writable array of `size` bytes.
        let n = unsafe { libc::read(self.fd(), buf.as_mut_ptr().cast(), size) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(Vec::new());
            }
            return Err(err);
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "end of file"));
        }
        let count = n as usize / std::mem::size_of::<libc::input_event>();
        Ok(buf[..count].iter().map(to_event).collect())
    }

    /// The Engine's view of the device: a dup of the fd, so the Engine can own it.
    pub fn info(&self) -> io::Result<MouseInfo> {
        // SAFETY: dup of a valid fd; the result is owned by OwnedFd.
        let fd = unsafe { libc::fcntl(self.fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(MouseInfo {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            msc_raw: self.msc_raw,
        })
    }
}

pub fn to_event(ev: &libc::input_event) -> Event {
    Event {
        ts: ev.time.tv_sec as f64 + ev.time.tv_usec as f64 / 1_000_000.0,
        kind: ev.type_,
        code: ev.code,
        value: ev.value,
    }
}

/// Every Magic Mouse not in `skip` (paths already grabbed). Opens /dev/input/event*.
pub fn find_mice(skip: &HashSet<String>) -> Vec<MagicMouse> {
    let mut found = Vec::new();
    for path in event_nodes(Path::new("/dev/input")) {
        if skip.contains(&path) {
            continue;
        }
        let Ok(dev) = MagicMouse::open(&path) else {
            continue;
        };
        if !is_own_device(dev.name()) && dev.is_magic_mouse() {
            found.push(dev);
        }
    }
    found
}

fn abs_info(fd: RawFd, code: u16) -> io::Result<libc::input_absinfo> {
    let mut info = libc::input_absinfo {
        value: 0,
        minimum: 0,
        maximum: 0,
        fuzz: 0,
        flat: 0,
        resolution: 0,
    };
    // SAFETY: EVIOCGABS writes one input_absinfo.
    let r = unsafe {
        libc::ioctl(
            fd,
            eviocgabs(code) as _,
            &mut info as *mut libc::input_absinfo,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

/// `iface::DeviceInfo` for the Engine (reads live state through its own dup of the fd).
pub struct MouseInfo {
    fd: OwnedFd,
    msc_raw: bool,
}

impl MouseInfo {
    /// The full absinfo (value, min, max, fuzz, flat, resolution); `value` of ABS_MT_SLOT is
    /// the current slot that Python's `sync_mt` needs.
    pub fn abs_info(&self, code: u16) -> Option<libc::input_absinfo> {
        abs_info(self.fd.as_raw_fd(), code).ok()
    }
}

impl DeviceInfo for MouseInfo {
    fn abs_range(&self, code: u16) -> Option<(i32, i32)> {
        self.abs_info(code).map(|a| (a.minimum, a.maximum))
    }

    fn abs_value(&self, code: u16) -> Option<i32> {
        self.abs_info(code).map(|a| a.value)
    }

    fn mt_slots(&self, code: u16) -> Option<Vec<i32>> {
        let n = (self.abs_info(ABS_MT_SLOT)?.maximum + 1).max(0) as usize;
        let mut buf = vec![0i32; n + 1];
        buf[0] = code as i32;
        let len = buf.len() * std::mem::size_of::<i32>();
        // SAFETY: the kernel writes at most `len` bytes into buf.
        let r = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                eviocgmtslots(len) as _,
                buf.as_mut_ptr(),
            )
        };
        if r < 0 {
            log(&format!("touch state: {}", io::Error::last_os_error()));
            return None;
        }
        Some(buf[1..].to_vec())
    }

    fn active_keys(&self) -> Vec<u16> {
        let mut bits = [0u8; (0x2ff / 8) + 1]; // KEY_MAX
                                               // SAFETY: the kernel writes at most bits.len() bytes.
        let r = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                eviocgkey(bits.len()) as _,
                bits.as_mut_ptr(),
            )
        };
        if r < 0 {
            return Vec::new();
        }
        keys_from_bits(&bits)
    }

    fn has_msc_raw(&self) -> bool {
        self.msc_raw
    }
}

pub fn keys_from_bits(bits: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    for (i, b) in bits.iter().enumerate() {
        for j in 0..8 {
            if b & (1 << j) != 0 {
                out.push((i * 8 + j) as u16);
            }
        }
    }
    out
}

// --------------------------------------------------------------------------- uinput
/// A uinput device that queues `write`s and sends them with the SYN_REPORT on `syn`
/// (one write(2) per frame; readers only see events at SYN_REPORT anyway).
pub struct VirtualOut {
    dev: VirtualDevice,
    queue: Vec<InputEvent>,
    failed: bool,
}

impl VirtualOut {
    pub fn new(dev: VirtualDevice) -> Self {
        Self {
            dev,
            queue: Vec::new(),
            failed: false,
        }
    }
    pub fn fd(&self) -> RawFd {
        self.dev.as_raw_fd()
    }
}

impl Output for VirtualOut {
    fn write(&mut self, kind: u16, code: u16, value: i32) {
        self.queue
            .push(InputEvent::new(EventType(kind), code, value));
    }

    fn syn(&mut self) {
        // emit() appends the SYN_REPORT
        let r = self.dev.emit(&self.queue);
        self.queue.clear();
        match r {
            Ok(()) => self.failed = false,
            Err(err) if !self.failed => {
                self.failed = true;
                log(&format!("virtual device: {err}"));
            }
            Err(_) => {}
        }
    }
}

/// The shared virtual mouse, as Python's `make_uinput()` (USB bus, 004c:0269, version 1).
pub fn make_uinput() -> io::Result<VirtualOut> {
    let mut keys = AttributeSet::<Key>::new();
    for k in [
        Key::BTN_LEFT,
        Key::BTN_RIGHT,
        Key::BTN_MIDDLE,
        Key::BTN_SIDE,
        Key::BTN_EXTRA,
    ] {
        keys.insert(k);
    }
    let mut rel = AttributeSet::<RelativeAxisType>::new();
    for r in [
        RelativeAxisType::REL_X,
        RelativeAxisType::REL_Y,
        RelativeAxisType::REL_WHEEL,
        RelativeAxisType::REL_HWHEEL,
        RelativeAxisType::REL_WHEEL_HI_RES,
        RelativeAxisType::REL_HWHEEL_HI_RES,
    ] {
        rel.insert(r);
    }
    let dev = VirtualDeviceBuilder::new()?
        .name(VIRT_NAME)
        .input_id(InputId::new(BusType::BUS_USB, 0x004C, 0x0269, 1))
        .with_keys(&keys)?
        .with_relative_axes(&rel)?
        .build()?;
    Ok(VirtualOut::new(dev))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mm() -> Caps {
        Caps {
            vendor: 0x004C,
            product: 0x0269,
            name: "Austin's Mouse".into(),
            rel_x: true,
            btn_left: true,
            ev_abs: true,
        }
    }

    #[test]
    fn magic_mouse_by_id() {
        assert!(is_magic_mouse(&mm()));
        for (v, p) in [(0x004C, 0x030D), (0x05AC, 0x0323), (0x05AC, 0x0269)] {
            assert!(is_magic_mouse(&Caps {
                vendor: v,
                product: p,
                ..mm()
            }));
        }
    }

    #[test]
    fn magic_mouse_by_name() {
        let c = Caps {
            vendor: 0x1234,
            product: 0x1,
            name: "Bob's MAGIC Mouse".into(),
            ..mm()
        };
        assert!(is_magic_mouse(&c));
        let c = Caps {
            vendor: 0x1234,
            product: 0x1,
            name: "Logitech".into(),
            ..mm()
        };
        assert!(!is_magic_mouse(&c));
        // Apple vendor, other product
        assert!(!is_magic_mouse(&Caps {
            product: 0x0265,
            ..mm()
        }));
    }

    #[test]
    fn magic_mouse_needs_caps() {
        assert!(!is_magic_mouse(&Caps {
            rel_x: false,
            ..mm()
        }));
        assert!(!is_magic_mouse(&Caps {
            btn_left: false,
            ..mm()
        }));
        assert!(!is_magic_mouse(&Caps {
            ev_abs: false,
            ..mm()
        }));
    }

    #[test]
    fn own_devices_skipped() {
        assert!(is_own_device("magic-mouse-omarchy"));
        assert!(is_own_device("Magic Mouse virtual touchpad"));
        assert!(!is_own_device("Magic Mouse"));
    }

    #[test]
    fn ioctl_numbers() {
        // Python: (2 << 30) | (length << 16) | (ord("E") << 8) | 0x0A
        assert_eq!(
            eviocgmtslots(44),
            (2 << 30) | (44 << 16) | (0x45 << 8) | 0x0A
        );
        assert_eq!(eviocgabs(0x35), 0x8018_4575);
        assert_eq!(eviocgkey(96), 0x8060_4518);
        assert_eq!(hidiocginput(4), (3 << 30) | (4 << 16) | (0x48 << 8) | 0x0A);
    }

    #[test]
    fn key_bits() {
        let mut bits = [0u8; 0x60];
        bits[0x110 / 8] |= 1 << (0x110 % 8);
        bits[0x111 / 8] |= 1 << (0x111 % 8);
        assert_eq!(keys_from_bits(&bits), vec![0x110, 0x111]);
    }

    #[test]
    fn event_timestamp() {
        let ev = libc::input_event {
            time: libc::timeval {
                tv_sec: 12,
                tv_usec: 345_678,
            },
            type_: EV_ABS,
            code: 0x35,
            value: -7,
        };
        let e = to_event(&ev);
        assert!((e.ts - 12.345678).abs() < 1e-9);
        assert_eq!((e.kind, e.code, e.value), (EV_ABS, 0x35, -7));
    }

    #[test]
    fn event_nodes_sorted_as_strings() {
        let dir = std::env::temp_dir().join(format!("mm-devices-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["event10", "event2", "mouse0", "event1"] {
            std::fs::write(dir.join(n), b"").unwrap();
        }
        let got: Vec<String> = event_nodes(&dir)
            .into_iter()
            .map(|p| p.rsplit('/').next().unwrap().to_string())
            .collect();
        assert_eq!(got, ["event1", "event10", "event2"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[ignore = "hardware: opens /dev/input (no grab)"]
    fn list_mice() {
        for m in find_mice(&HashSet::new()) {
            println!(
                "{}: {} {:04x}:{:04x}",
                m.path(),
                m.name(),
                m.vendor(),
                m.product()
            );
        }
    }

    #[test]
    #[ignore = "creates a uinput device"]
    fn create_virtual_mouse() {
        let mut ui = make_uinput().unwrap();
        ui.write(EV_REL, REL_X, 1);
        ui.syn();
    }
}
