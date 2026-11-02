//! The mouse's charge: HID report 0x90 through hidraw, and the kernel's power_supply.
//! Python reference: `BATTERY_REPORT_ID`, `hidraw_for`, `parse_battery_report`, `query_battery`,
//! `kernel_battery` (v1.4.1, ~l.538-605) and `bin/magic-mouse-battery-query`.
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::devices::hidiocginput;
use crate::iface::log;

/// The Magic Mouse reports its charge in HID *input* report 0x90.
pub const BATTERY_REPORT_ID: u8 = 0x90;

/// Sorted entries of `dir` whose name starts with `prefix` (Python `sorted(glob(dir/prefix*))`).
fn entries(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
            .map(|e| e.path())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// The hidraw node of this mouse: the one whose uevent names its address (two of a model
/// share the ID), else the first of the model. `sysfs` is /sys/class/hidraw.
pub fn hidraw_for_in(sysfs: &Path, uniq: &str, vendor: u16, product: u16) -> Option<PathBuf> {
    let want_uniq = (!uniq.is_empty()).then(|| format!("HID_UNIQ={uniq}").to_lowercase());
    let want_id = format!("HID_ID=0005:{vendor:08X}:{product:08X}");
    let mut by_id = None;
    for p in entries(sysfs, "hidraw") {
        let Ok(ue) = std::fs::read_to_string(p.join("device/uevent")) else {
            continue;
        };
        let node = Path::new("/dev").join(p.file_name().unwrap());
        if let Some(w) = &want_uniq {
            if ue.to_lowercase().contains(w.as_str()) {
                return Some(node);
            }
        }
        if by_id.is_none() && ue.contains(&want_id) {
            by_id = Some(node);
        }
    }
    by_id
}

pub fn hidraw_for(uniq: &str, vendor: u16, product: u16) -> Option<PathBuf> {
    hidraw_for_in(Path::new("/sys/class/hidraw"), uniq, vendor, product)
}

/// report 0x90: [id, flags, percent, ...]; flags bit2 = charging (usage 0x85/0x44).
pub fn parse_battery_report(buf: &[u8]) -> Option<(u8, bool)> {
    if buf.len() < 3 || buf[0] != BATTERY_REPORT_ID {
        return None;
    }
    let percent = buf[2];
    if percent > 100 {
        return None;
    }
    Some((percent, buf[1] & 0x04 != 0))
}

/// The raw answer to GET_REPORT(0x90) (HIDIOCGINPUT, 4 bytes).
pub fn get_report(hidraw: &Path) -> std::io::Result<[u8; 4]> {
    let mut buf = [BATTERY_REPORT_ID, 0, 0, 0];
    let f = OpenOptions::new().read(true).write(true).open(hidraw)?;
    // SAFETY: HIDIOCGINPUT(4) reads and writes the 4-byte buffer.
    let r = unsafe {
        libc::ioctl(
            f.as_raw_fd(),
            hidiocginput(buf.len()) as _,
            buf.as_mut_ptr(),
        )
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(buf)
}

/// Ask the mouse for its charge right now: (percent, charging).
pub fn query_battery(hidraw: &Path) -> Option<(u8, bool)> {
    match get_report(hidraw) {
        Ok(buf) => parse_battery_report(&buf),
        Err(exc) => {
            log(&format!("battery: {}: {exc}", hidraw.display()));
            None
        }
    }
}

/// The charge the kernel publishes (power_supply hid-<address>-battery), if it knows one.
/// `sysfs` is /sys/class/power_supply.
pub fn kernel_battery_in(sysfs: &Path, uniq: &str) -> Option<u8> {
    if uniq.is_empty() {
        return None;
    }
    let prefix = format!("hid-{}-battery", uniq.to_lowercase());
    for p in entries(sysfs, &prefix) {
        let Ok(s) = std::fs::read_to_string(p.join("capacity")) else {
            continue;
        };
        let Ok(value) = s.trim().parse::<i64>() else {
            continue;
        };
        if 0 < value && value <= 100 {
            return Some(value as u8); // 0 = the mouse has not reported yet
        }
    }
    None
}

pub fn kernel_battery(uniq: &str) -> Option<u8> {
    kernel_battery_in(Path::new("/sys/class/power_supply"), uniq)
}

/// Any Magic Mouse's hidraw node, as `bin/magic-mouse-battery-query`.
pub fn hidraw_for_any_magic_mouse_in(sysfs: &Path) -> Option<PathBuf> {
    const IDS: [&str; 3] = [
        "HID_ID=0005:0000004C:00000269",
        "HID_ID=0005:0000004C:00000323",
        "HID_ID=0005:000005AC:0000030D",
    ];
    for p in entries(sysfs, "hidraw") {
        let ue = std::fs::read_to_string(p.join("device/uevent")).unwrap_or_default();
        if IDS.iter().any(|i| ue.contains(i)) {
            return Some(Path::new("/dev").join(p.file_name().unwrap()));
        }
    }
    None
}

/// `bin/magic-mouse-battery-query`: Ok(line to print) or Err(message to exit with).
pub fn battery_query_cli() -> Result<String, String> {
    let dev = hidraw_for_any_magic_mouse_in(Path::new("/sys/class/hidraw"))
        .ok_or_else(|| "no Magic Mouse hidraw node found".to_string())?;
    let buf = get_report(&dev)
        .map_err(|exc| format!("{}: GET_REPORT(0x90) refused: {exc}", dev.display()))?;
    Ok(format_query(&dev, &buf))
}

pub fn format_query(dev: &Path, buf: &[u8; 4]) -> String {
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}: report 0x90 = {hex}  -> flags={:#04x} battery={}%",
        dev.display(),
        buf[1],
        buf[2]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mm-battery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn hidraw(root: &Path, n: &str, uevent: &str) {
        let d = root.join(n).join("device");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("uevent"), uevent).unwrap();
    }

    #[test]
    fn parse() {
        assert_eq!(
            parse_battery_report(&[0x90, 0x00, 57, 0]),
            Some((57, false))
        );
        assert_eq!(
            parse_battery_report(&[0x90, 0x04, 100, 0]),
            Some((100, true))
        );
        assert_eq!(parse_battery_report(&[0x90, 0x0b, 0]), Some((0, false)));
        assert_eq!(parse_battery_report(&[0x90, 0x06, 101, 0]), None);
        assert_eq!(parse_battery_report(&[0x91, 0x00, 50, 0]), None);
        assert_eq!(parse_battery_report(&[0x90, 0x00]), None);
        assert_eq!(parse_battery_report(&[]), None);
    }

    #[test]
    fn hidraw_lookup() {
        let root = tmp("hidraw");
        hidraw(
            &root,
            "hidraw0",
            "HID_ID=0003:0000046D:0000C52B\nHID_UNIQ=\n",
        );
        hidraw(
            &root,
            "hidraw1",
            "HID_ID=0005:0000004C:00000269\nHID_UNIQ=aa:bb:cc:dd:ee:01\n",
        );
        hidraw(
            &root,
            "hidraw2",
            "HID_ID=0005:0000004C:00000269\nHID_UNIQ=aa:bb:cc:dd:ee:02\n",
        );
        std::fs::create_dir_all(root.join("hidraw3")).unwrap(); // no uevent: skipped
                                                                // by address (case-insensitive), not the first of the model
        assert_eq!(
            hidraw_for_in(&root, "AA:BB:CC:DD:EE:02", 0x004C, 0x0269),
            Some(PathBuf::from("/dev/hidraw2"))
        );
        // unknown address: the first of the model
        assert_eq!(
            hidraw_for_in(&root, "11:22:33:44:55:66", 0x004C, 0x0269),
            Some(PathBuf::from("/dev/hidraw1"))
        );
        assert_eq!(
            hidraw_for_in(&root, "", 0x004C, 0x0269),
            Some(PathBuf::from("/dev/hidraw1"))
        );
        assert_eq!(hidraw_for_in(&root, "", 0x05AC, 0x0323), None);
        assert_eq!(
            hidraw_for_any_magic_mouse_in(&root),
            Some(PathBuf::from("/dev/hidraw1"))
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn kernel_capacity() {
        let root = tmp("psu");
        let d = root.join("hid-aa:bb:cc:dd:ee:01-battery");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("capacity"), "0\n").unwrap();
        assert_eq!(kernel_battery_in(&root, "AA:BB:CC:DD:EE:01"), None); // 0 = not reported yet
        std::fs::write(d.join("capacity"), "73\n").unwrap();
        assert_eq!(kernel_battery_in(&root, "AA:BB:CC:DD:EE:01"), Some(73));
        assert_eq!(kernel_battery_in(&root, "AA:BB:CC:DD:EE:02"), None);
        assert_eq!(kernel_battery_in(&root, ""), None);
        std::fs::write(d.join("capacity"), "junk").unwrap();
        assert_eq!(kernel_battery_in(&root, "aa:bb:cc:dd:ee:01"), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn query_line() {
        assert_eq!(
            format_query(Path::new("/dev/hidraw3"), &[0x90, 0x04, 0x39, 0x00]),
            "/dev/hidraw3: report 0x90 = 90043900  -> flags=0x04 battery=57%"
        );
    }

    #[test]
    #[ignore = "hardware: opens /dev/hidraw"]
    fn query_live() {
        println!("{:?}", battery_query_cli());
    }
}
