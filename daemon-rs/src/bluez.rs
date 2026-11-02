//! The mouse's charge in every Bluetooth UI, through BlueZ's BatteryProvider API
//! (org.bluez.BatteryProviderManager1). Python reference: `class BluezBattery` (v1.4.1).
//!
//! Python runs a GLib main loop in a thread and hands set()/clear() over with idle_add; here a
//! worker thread owns all BlueZ work and takes commands from a channel in the order they were
//! sent (the NameOwnerChanged watcher sends its `forget` into the same channel). zbus serves
//! GetManagedObjects and the provider objects from its own executor thread.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use zbus::blocking::Connection;
use zbus::names::BusName;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

use crate::iface::log;

pub const ROOT: &str = "/io/github/twostrange/magicmouse/battery";
pub const PROVIDER_IFACE: &str = "org.bluez.BatteryProvider1";
pub const SOURCE: &str = "magic-mouse-daemon";

/// Our object for the mouse with this address: ROOT/dev_AA_BB_...
pub fn provider_path(address: &str) -> String {
    format!("{ROOT}/dev_{}", address.to_uppercase().replace(':', "_"))
}

/// /org/bluez/hci0/dev_XX -> /org/bluez/hci0
pub fn adapter_of(device: &str) -> &str {
    device.rsplit_once('/').map_or("", |(a, _)| a)
}

/// our path -> (BlueZ device path, percent): what is exported right now.
pub type Objects = Arc<Mutex<HashMap<String, (String, u8)>>>;

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Set(String, u8),
    Clear(String),
    Forget,
}

// --------------------------------------------------------------------------- handle (main thread)
/// What the main loop holds. Without a system bus it logs once and does nothing.
pub struct BluezBattery {
    tx: Option<Sender<Cmd>>,
    objects: Objects,
}

impl BluezBattery {
    /// Connects to the system bus and starts the worker; registers nothing with BlueZ yet.
    pub fn new() -> Self {
        let objects: Objects = Arc::default();
        match ZbusOps::connect() {
            Ok(ops) => {
                let (tx, rx) = channel();
                watch_bluez(&ops.conn, tx.clone());
                let objs = objects.clone();
                thread::Builder::new()
                    .name("bluez-battery".into())
                    .spawn(move || Worker::new(ops, objs).run(rx))
                    .expect("spawn bluez thread");
                Self {
                    tx: Some(tx),
                    objects,
                }
            }
            Err(exc) => {
                log(&format!(
                    "battery: no system bus for Bluetooth settings: {exc}"
                ));
                Self { tx: None, objects }
            }
        }
    }

    /// Inert (Python: self.bus is None).
    pub fn disabled() -> Self {
        Self {
            tx: None,
            objects: Arc::default(),
        }
    }

    fn with_channel(tx: Sender<Cmd>, objects: Objects) -> Self {
        Self {
            tx: Some(tx),
            objects,
        }
    }

    pub fn set(&self, address: &str, percent: u8) {
        if let Some(tx) = &self.tx {
            if !address.is_empty() {
                let _ = tx.send(Cmd::Set(address.to_string(), percent));
            }
        }
    }

    /// poll() calls this often: only when registered. force (mouse gone) also catches a set still queued.
    pub fn clear(&self, address: &str, force: bool) {
        if let Some(tx) = &self.tx {
            if !address.is_empty()
                && (force
                    || self
                        .objects
                        .lock()
                        .unwrap()
                        .contains_key(&provider_path(address)))
            {
                let _ = tx.send(Cmd::Clear(address.to_string()));
            }
        }
    }
}

impl Default for BluezBattery {
    fn default() -> Self {
        Self::new()
    }
}

// --------------------------------------------------------------------------- the bus, abstracted
/// Everything the worker does on D-Bus (a fake in the tests).
pub trait BusOps {
    /// BlueZ's object for the device with this address (/org/bluez/hciN/dev_XX_...).
    fn device_path(&mut self, address: &str) -> Result<Option<String>, String>;
    fn export(&mut self, path: &str, device: &str, percent: u8) -> Result<(), String>;
    fn unexport(&mut self, path: &str) -> Result<(), String>;
    fn register(&mut self, adapter: &str) -> Result<(), String>;
    fn interfaces_added(&mut self, path: &str, device: &str, percent: u8) -> Result<(), String>;
    fn interfaces_removed(&mut self, path: &str) -> Result<(), String>;
    fn percentage_changed(&mut self, path: &str, percent: u8) -> Result<(), String>;
}

/// Python's `_set`, `_clear` and `forget`, run one at a time on the worker thread.
pub struct Worker<B: BusOps> {
    pub bus: B,
    objects: Objects,
    /// adapters we registered with
    adapters: HashSet<String>,
}

impl<B: BusOps> Worker<B> {
    pub fn new(bus: B, objects: Objects) -> Self {
        Self {
            bus,
            objects,
            adapters: HashSet::new(),
        }
    }

    pub fn run(mut self, rx: Receiver<Cmd>) {
        for cmd in rx {
            self.handle(cmd);
        }
    }

    pub fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Set(a, p) => self.set(&a, p),
            Cmd::Clear(a) => self.clear(&a),
            Cmd::Forget => self.forget(),
        }
    }

    fn set(&mut self, address: &str, percent: u8) {
        let path = provider_path(address);
        let mut exported = false;
        if let Err(exc) = self.try_set(&path, address, percent, &mut exported) {
            log(&format!("battery: Bluetooth settings: {exc}"));
            if exported {
                // or every later try fails with "already exported"
                let _ = self.bus.unexport(&path);
            }
            self.objects.lock().unwrap().remove(&path);
        }
    }

    fn try_set(
        &mut self,
        path: &str,
        address: &str,
        percent: u8,
        exported: &mut bool,
    ) -> Result<(), String> {
        let known = self.objects.lock().unwrap().get(path).cloned();
        if let Some((dev, old)) = known {
            *exported = true; // Python: reg is set, so a failed emit unregisters it too
            if old != percent {
                self.objects
                    .lock()
                    .unwrap()
                    .insert(path.to_string(), (dev, percent));
                self.bus.percentage_changed(path, percent)?;
            }
            return Ok(());
        }
        let Some(dev) = self.bus.device_path(address)? else {
            return Ok(()); // BlueZ does not know the mouse (yet); the next poll tries again
        };
        let adapter = adapter_of(&dev).to_string();
        self.bus.export(path, &dev, percent)?;
        *exported = true;
        self.objects
            .lock()
            .unwrap()
            .insert(path.to_string(), (dev.clone(), percent));
        if !self.adapters.contains(&adapter) {
            self.bus.register(&adapter)?;
            self.adapters.insert(adapter); // BlueZ now asks GetManagedObjects and picks the battery up
        } else {
            self.bus.interfaces_added(path, &dev, percent)?;
        }
        log(&format!("battery: {percent}% handed to Bluetooth settings"));
        Ok(())
    }

    /// BlueZ went away: drop every battery; the next set() registers anew.
    fn forget(&mut self) {
        let paths: Vec<String> = self
            .objects
            .lock()
            .unwrap()
            .drain()
            .map(|(p, _)| p)
            .collect();
        for p in paths {
            let _ = self.bus.unexport(&p);
        }
        self.adapters.clear();
    }

    fn clear(&mut self, address: &str) {
        let path = provider_path(address);
        let had = self.objects.lock().unwrap().remove(&path).is_some();
        if had {
            let _ = self.bus.unexport(&path);
            if let Err(exc) = self.bus.interfaces_removed(&path) {
                log(&format!("battery: Bluetooth settings: {exc}"));
            }
        }
    }
}

// --------------------------------------------------------------------------- zbus
struct Provider {
    device: OwnedObjectPath,
    percentage: u8,
}

#[zbus::interface(name = "org.bluez.BatteryProvider1")]
impl Provider {
    #[zbus(property)]
    fn device(&self) -> OwnedObjectPath {
        self.device.clone()
    }
    #[zbus(property)]
    fn percentage(&self) -> u8 {
        self.percentage
    }
    #[zbus(property)]
    fn source(&self) -> String {
        SOURCE.to_string()
    }
}

pub struct ZbusOps {
    conn: Connection,
}

fn e2s(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl ZbusOps {
    /// System bus, with org.freedesktop.DBus.ObjectManager served at ROOT.
    pub fn connect() -> zbus::Result<Self> {
        let conn = Connection::system()?;
        conn.object_server().at(ROOT, zbus::fdo::ObjectManager)?;
        Ok(Self { conn })
    }
}

impl BusOps for ZbusOps {
    fn device_path(&mut self, address: &str) -> Result<Option<String>, String> {
        let proxy = zbus::blocking::fdo::ObjectManagerProxy::builder(&self.conn)
            .destination("org.bluez")
            .map_err(e2s)?
            .path("/")
            .map_err(e2s)?
            .build()
            .map_err(e2s)?;
        let objects = proxy.get_managed_objects().map_err(e2s)?;
        // sorted, so two adapters knowing the same mouse always give the same answer
        let sorted: BTreeMap<String, _> = objects
            .into_iter()
            .map(|(p, i)| (p.to_string(), i))
            .collect();
        for (path, ifaces) in sorted {
            let Some(d) = ifaces
                .iter()
                .find(|(n, _)| n.as_str() == "org.bluez.Device1")
                .map(|(_, d)| d)
            else {
                continue;
            };
            let addr = d
                .get("Address")
                .and_then(|v| <&str>::try_from(&**v).ok())
                .unwrap_or("");
            if addr.to_lowercase() == address.to_lowercase() {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    fn export(&mut self, path: &str, device: &str, percent: u8) -> Result<(), String> {
        let device = OwnedObjectPath::try_from(device.to_string()).map_err(e2s)?;
        // zbus emits InterfacesAdded on ROOT here (the ObjectManager there sees the new child)
        let added = self
            .conn
            .object_server()
            .at(
                path,
                Provider {
                    device,
                    percentage: percent,
                },
            )
            .map_err(e2s)?;
        if !added {
            return Err(format!("an object is already exported at {path}"));
        }
        Ok(())
    }

    fn unexport(&mut self, path: &str) -> Result<(), String> {
        // zbus emits InterfacesRemoved on ROOT here
        self.conn
            .object_server()
            .remove::<Provider, _>(path)
            .map(|_| ())
            .map_err(e2s)
    }

    fn register(&mut self, adapter: &str) -> Result<(), String> {
        let root = ObjectPath::try_from(ROOT).map_err(e2s)?;
        self.conn
            .call_method(
                Some("org.bluez"),
                adapter,
                Some("org.bluez.BatteryProviderManager1"),
                "RegisterBatteryProvider",
                &(root,),
            )
            .map(|_| ())
            .map_err(e2s)
    }

    fn interfaces_added(&mut self, _path: &str, _device: &str, _percent: u8) -> Result<(), String> {
        Ok(()) // already sent by the object server in export()
    }

    fn interfaces_removed(&mut self, _path: &str) -> Result<(), String> {
        Ok(()) // already sent by the object server in unexport()
    }

    fn percentage_changed(&mut self, path: &str, percent: u8) -> Result<(), String> {
        let iface = self
            .conn
            .object_server()
            .interface::<_, Provider>(path)
            .map_err(e2s)?;
        iface.get_mut().percentage = percent;
        let changed: HashMap<&str, Value> = HashMap::from([("Percentage", Value::U8(percent))]);
        self.conn
            .emit_signal(
                None::<BusName>,
                path,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(PROVIDER_IFACE, changed, Vec::<&str>::new()),
            )
            .map_err(e2s)
    }
}

/// bluetoothd restarted (or went away): our registration is gone with it -> Forget.
fn watch_bluez(conn: &Connection, tx: Sender<Cmd>) {
    let conn = conn.clone();
    let spawned = thread::Builder::new()
        .name("bluez-watch".into())
        .spawn(move || {
            let stream = zbus::blocking::fdo::DBusProxy::new(&conn)
                .and_then(|p| p.receive_name_owner_changed_with_args(&[(0, "org.bluez")]));
            let stream = match stream {
                Ok(s) => s,
                Err(exc) => {
                    log(&format!("battery: cannot watch bluetoothd: {exc}"));
                    return;
                }
            };
            for sig in stream {
                let Ok(args) = sig.args() else { continue };
                if args.old_owner().is_some() && tx.send(Cmd::Forget).is_err() {
                    return;
                }
            }
        });
    if let Err(exc) = spawned {
        log(&format!("battery: cannot watch bluetoothd: {exc}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        devices: HashMap<String, String>,
        fail_register: bool,
    }

    impl BusOps for Fake {
        fn device_path(&mut self, address: &str) -> Result<Option<String>, String> {
            self.calls.push(format!("device_path {address}"));
            Ok(self.devices.get(&address.to_lowercase()).cloned())
        }
        fn export(&mut self, path: &str, device: &str, percent: u8) -> Result<(), String> {
            self.calls.push(format!("export {path} {device} {percent}"));
            Ok(())
        }
        fn unexport(&mut self, path: &str) -> Result<(), String> {
            self.calls.push(format!("unexport {path}"));
            Ok(())
        }
        fn register(&mut self, adapter: &str) -> Result<(), String> {
            self.calls.push(format!("register {adapter}"));
            if self.fail_register {
                Err("org.bluez.Error.Failed".into())
            } else {
                Ok(())
            }
        }
        fn interfaces_added(
            &mut self,
            path: &str,
            device: &str,
            percent: u8,
        ) -> Result<(), String> {
            self.calls.push(format!("added {path} {device} {percent}"));
            Ok(())
        }
        fn interfaces_removed(&mut self, path: &str) -> Result<(), String> {
            self.calls.push(format!("removed {path}"));
            Ok(())
        }
        fn percentage_changed(&mut self, path: &str, percent: u8) -> Result<(), String> {
            self.calls.push(format!("changed {path} {percent}"));
            Ok(())
        }
    }

    const A1: &str = "aa:bb:cc:dd:ee:01";
    const A2: &str = "aa:bb:cc:dd:ee:02";
    const P1: &str = "/io/github/twostrange/magicmouse/battery/dev_AA_BB_CC_DD_EE_01";
    const P2: &str = "/io/github/twostrange/magicmouse/battery/dev_AA_BB_CC_DD_EE_02";
    const D1: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01";
    const D2: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_02";

    fn worker() -> (Worker<Fake>, Objects) {
        let mut f = Fake::default();
        f.devices.insert(A1.into(), D1.into());
        f.devices.insert(A2.into(), D2.into());
        let objects: Objects = Arc::default();
        (Worker::new(f, objects.clone()), objects)
    }

    fn take(w: &mut Worker<Fake>) -> Vec<String> {
        std::mem::take(&mut w.bus.calls)
    }

    #[test]
    fn paths() {
        assert_eq!(provider_path(A1), P1);
        assert_eq!(adapter_of(D1), "/org/bluez/hci0");
        assert_eq!(adapter_of("nothing"), "");
    }

    #[test]
    fn set_registers_once_per_adapter() {
        let (mut w, objects) = worker();
        w.handle(Cmd::Set(A1.into(), 80));
        assert_eq!(
            take(&mut w),
            [
                format!("device_path {A1}"),
                format!("export {P1} {D1} 80"),
                "register /org/bluez/hci0".into()
            ]
        );
        assert_eq!(objects.lock().unwrap().get(P1), Some(&(D1.to_string(), 80)));
        w.handle(Cmd::Set(A1.into(), 80)); // unchanged: nothing
        assert!(take(&mut w).is_empty());
        w.handle(Cmd::Set(A1.into(), 79));
        assert_eq!(take(&mut w), [format!("changed {P1} 79")]);
        // second mouse on the same adapter: InterfacesAdded, no second registration
        w.handle(Cmd::Set(A2.into(), 50));
        assert_eq!(
            take(&mut w),
            [
                format!("device_path {A2}"),
                format!("export {P2} {D2} 50"),
                format!("added {P2} {D2} 50")
            ]
        );
    }

    #[test]
    fn unknown_device_retries_later() {
        let (mut w, objects) = worker();
        w.handle(Cmd::Set("11:22:33:44:55:66".into(), 10));
        assert_eq!(take(&mut w), ["device_path 11:22:33:44:55:66"]);
        assert!(objects.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_registration_cleans_up() {
        let (mut w, objects) = worker();
        w.bus.fail_register = true;
        w.handle(Cmd::Set(A1.into(), 80));
        assert_eq!(
            take(&mut w),
            [
                format!("device_path {A1}"),
                format!("export {P1} {D1} 80"),
                "register /org/bluez/hci0".into(),
                format!("unexport {P1}")
            ]
        );
        assert!(objects.lock().unwrap().is_empty());
        w.bus.fail_register = false;
        w.handle(Cmd::Set(A1.into(), 80)); // tries again from scratch
        assert_eq!(
            take(&mut w),
            [
                format!("device_path {A1}"),
                format!("export {P1} {D1} 80"),
                "register /org/bluez/hci0".into()
            ]
        );
    }

    #[test]
    fn clear_and_forget() {
        let (mut w, objects) = worker();
        w.handle(Cmd::Set(A1.into(), 80));
        take(&mut w);
        w.handle(Cmd::Clear(A1.into()));
        assert_eq!(
            take(&mut w),
            [format!("unexport {P1}"), format!("removed {P1}")]
        );
        w.handle(Cmd::Clear(A1.into())); // not registered: nothing
        assert!(take(&mut w).is_empty());
        // the adapter stays registered: InterfacesAdded next time
        w.handle(Cmd::Set(A1.into(), 70));
        assert_eq!(take(&mut w).last().unwrap(), &format!("added {P1} {D1} 70"));
        // BlueZ restarted: everything dropped silently, next set registers anew
        w.handle(Cmd::Forget);
        assert_eq!(take(&mut w), [format!("unexport {P1}")]);
        assert!(objects.lock().unwrap().is_empty());
        w.handle(Cmd::Set(A1.into(), 70));
        assert_eq!(take(&mut w).last().unwrap(), "register /org/bluez/hci0");
    }

    #[test]
    fn handle_queues_in_order_and_clear_only_when_registered() {
        let (tx, rx) = channel();
        let objects: Objects = Arc::default();
        let h = BluezBattery::with_channel(tx, objects.clone());
        h.clear(A1, false); // not registered: not queued
        h.set(A1, 90);
        h.set("", 90); // no address: nothing
        h.clear(A1, true); // forced: queued behind the set, so it catches it
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            [Cmd::Set(A1.into(), 90), Cmd::Clear(A1.into())]
        );
        objects.lock().unwrap().insert(P1.into(), (D1.into(), 90));
        h.clear("AA:BB:CC:DD:EE:01", false);
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            [Cmd::Clear("AA:BB:CC:DD:EE:01".into())]
        );
        let off = BluezBattery::disabled();
        off.set(A1, 1);
        off.clear(A1, true);
    }

    #[test]
    #[ignore = "D-Bus: connects to the system bus and BlueZ"]
    fn live_device_lookup() {
        let mut ops = ZbusOps::connect().unwrap();
        println!("{:?}", ops.device_path(A1));
    }
}
