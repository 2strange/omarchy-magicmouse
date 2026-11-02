//! Live touch stream for the preview window (Python: `PROBE_PATH`, `PROBE_MAX_CLIENTS`, `Probe`).
//!
//! A Unix stream socket in the owner-only status directory (`$XDG_RUNTIME_DIR/magic-mouse/probe.sock`,
//! 0600). Each client gets one JSON line per input frame, `json.dumps(obj, separators=(",", ":"))`
//! plus a newline, a hello line first. Clients only listen; what they send is discarded. A client that
//! cannot keep up loses frames instead of stalling the daemon (dropped after more than
//! `MAX_STALLS` frames in a row it could not take, or on a cut line), at most `MAX_CLIENTS`.
//!
//! API for the main poll loop (same shape as the Python select loop):
//!
//! * `Probe::new()` (default path) / `Probe::with_path(p)`, then `open()`: creates/validates the
//!   directory, replaces a stale socket, refuses a live one or a non-socket; on failure it logs
//!   `probe disabled: ...` and stays closed (`is_open() == false`), the daemon runs on.
//! * `fds()`: listening fd + every client fd; poll all of them for POLLIN (Python `probe.fds()`).
//!   The set changes after `handle` and `send`, so rebuild the poll list each round.
//! * `handle(&ready)`: pass the fds that polled readable (POLLIN/POLLHUP/POLLERR); accepts on the
//!   listener (sending the hello) and drains / drops clients.
//! * `set_hello(geometry)`: Python `probe.hello = s.geometry()`; `send_hello()` broadcasts it with
//!   `"type": "hello"` (what `make_active` does); `send_obj(&json!({"type": "disconnected"}))`.
//! * `ProbeSink` is implemented for `Probe` and for `Rc<RefCell<Probe>>`, so the main loop can
//!   keep one handle and give the Engine a clone (`Box::new(rc.clone())`).
//! * `close()` (also on drop): drops clients, closes and unlinks the socket.
use std::cell::RefCell;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Map, Value};

use crate::iface::{log, ProbeSink};
use crate::status::{open_private_dir, py_json, status_dir, OwnError};

pub const MAX_CLIENTS: usize = 4;
/// A client is dropped once it could not take more than this many frames in a row.
pub const MAX_STALLS: u32 = 200;

/// Python `PROBE_PATH`.
pub fn probe_path() -> PathBuf {
    status_dir().join("probe.sock")
}

struct Client {
    sock: UnixStream,
    stalls: u32,
}

pub struct Probe {
    path: PathBuf,
    sock: Option<UnixListener>,
    clients: Vec<Client>,
    /// Python `probe.hello`: the active mouse's geometry, sent (plus `"type": "hello"`) on connect.
    pub hello: Map<String, Value>,
}

impl Default for Probe {
    fn default() -> Self {
        Self::new()
    }
}

impl Probe {
    pub fn new() -> Self {
        Self::with_path(probe_path())
    }

    pub fn with_path(path: PathBuf) -> Self {
        Probe { path, sock: None, clients: Vec::new(), hello: Map::new() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_open(&self) -> bool {
        self.sock.is_some()
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Python `Probe.open`: logs `probe: <path>` or `probe disabled: <reason>`.
    pub fn open(&mut self) {
        match self.try_open() {
            Ok(()) => log(&format!("probe: {}", self.path.display())),
            Err(exc) => log(&format!("probe disabled: {exc}")),
        }
    }

    pub fn try_open(&mut self) -> Result<(), OwnError> {
        let path = self.path.clone();
        let dir = path.parent().unwrap_or(Path::new(""));
        drop(open_private_dir(dir)?); // create 0700 / refuse an unsafe directory
        match std::fs::symlink_metadata(&path) {
            Ok(st) => {
                use std::os::unix::fs::FileTypeExt;
                if !st.file_type().is_socket() {
                    return Err(OwnError::new(libc::EEXIST, "not a socket, refusing to replace it", &path));
                }
                if UnixStream::connect(&path).is_ok() {
                    return Err(OwnError::new(libc::EADDRINUSE, "another daemon owns the probe socket", &path));
                }
                // stale socket from a previous run
                std::fs::remove_file(&path).map_err(|e| io_err(e, &path))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(e, &path)),
        }
        let listener = bind_listen(&path)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| io_err(e, &path))?;
        }
        self.sock = Some(listener);
        Ok(())
    }

    /// Python `probe.fds()`: poll these for readability.
    pub fn fds(&self) -> Vec<RawFd> {
        self.sock.iter().map(|s| s.as_raw_fd()).chain(self.clients.iter().map(|c| c.sock.as_raw_fd())).collect()
    }

    /// The listening socket alone, if open.
    pub fn listener_fd(&self) -> Option<RawFd> {
        self.sock.as_ref().map(|s| s.as_raw_fd())
    }

    /// Python `probe.handle(ready)`.
    pub fn handle(&mut self, ready: &[RawFd]) {
        if let Some(sock) = &self.sock {
            if ready.contains(&sock.as_raw_fd()) {
                if let Ok((conn, _)) = sock.accept() {
                    if self.clients.len() >= MAX_CLIENTS {
                        drop(conn);
                    } else if conn.set_nonblocking(true).is_ok() {
                        let fd = conn.as_raw_fd();
                        self.clients.push(Client { sock: conn, stalls: 0 });
                        let hello = self.hello_obj();
                        self.send_to(&hello, Some(fd));
                    }
                }
            }
        }
        let readable: Vec<RawFd> = self.clients.iter().map(|c| c.sock.as_raw_fd()).filter(|fd| ready.contains(fd)).collect();
        let mut buf = [0u8; 4096];
        for fd in readable {
            let n = loop {
                // SAFETY: fd is a client socket we own; buf is valid.
                let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
                if n < 0 && errno() == libc::EINTR {
                    continue;
                }
                break n;
            };
            if n < 0 && matches!(errno(), libc::EAGAIN) {
                continue;
            }
            if n <= 0 {
                self.drop_fd(fd);
            }
        }
    }

    fn hello_obj(&self) -> Value {
        let mut h = self.hello.clone();
        h.insert("type".into(), Value::String("hello".into()));
        Value::Object(h)
    }

    /// Python `probe.hello = geometry` (a JSON object; anything else counts as empty).
    pub fn set_hello(&mut self, geometry: Value) {
        self.hello = match geometry {
            Value::Object(m) => m,
            _ => Map::new(),
        };
    }

    /// Python `probe.send(dict(probe.hello, type="hello"))`: to every client.
    pub fn send_hello(&mut self) {
        let hello = self.hello_obj();
        self.send_to(&hello, None);
    }

    /// Python `probe.send(obj)`: to every client.
    pub fn send_obj(&mut self, obj: &Value) {
        self.send_to(obj, None);
    }

    fn send_to(&mut self, obj: &Value, only: Option<RawFd>) {
        let mut line = py_json(obj, true);
        line.push('\n');
        let line = line.as_bytes();
        let targets: Vec<RawFd> =
            self.clients.iter().map(|c| c.sock.as_raw_fd()).filter(|fd| only.is_none_or(|o| o == *fd)).collect();
        for fd in targets {
            let sent = loop {
                // SAFETY: fd is a client socket we own; line is valid.
                let n = unsafe { libc::send(fd, line.as_ptr().cast(), line.len(), libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
                if n < 0 && errno() == libc::EINTR {
                    continue;
                }
                break n;
            };
            if sent < 0 {
                if errno() == libc::EAGAIN {
                    // slow reader: skip this frame for it; one that never reads gives its slot back
                    let c = self.clients.iter_mut().find(|c| c.sock.as_raw_fd() == fd).expect("client");
                    c.stalls += 1;
                    if c.stalls > MAX_STALLS {
                        self.drop_fd(fd);
                    }
                } else {
                    self.drop_fd(fd);
                }
            } else if (sent as usize) < line.len() {
                self.drop_fd(fd); // a cut line would corrupt the stream
            } else if let Some(c) = self.clients.iter_mut().find(|c| c.sock.as_raw_fd() == fd) {
                c.stalls = 0;
            }
        }
    }

    fn drop_fd(&mut self, fd: RawFd) {
        self.clients.retain(|c| c.sock.as_raw_fd() != fd);
    }

    /// Python `probe.close()`.
    pub fn close(&mut self) {
        self.clients.clear();
        if self.sock.take().is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.close();
    }
}

impl ProbeSink for Probe {
    fn has_clients(&self) -> bool {
        !self.clients.is_empty()
    }
    fn send(&mut self, json: &Value) {
        self.send_obj(json);
    }
}

impl ProbeSink for Rc<RefCell<Probe>> {
    fn has_clients(&self) -> bool {
        self.borrow().has_clients()
    }
    fn send(&mut self, json: &Value) {
        self.borrow_mut().send_obj(json);
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn io_err(e: std::io::Error, path: &Path) -> OwnError {
    OwnError::os(e.raw_os_error().unwrap_or(libc::EIO), path)
}

/// socket(SOCK_NONBLOCK|SOCK_CLOEXEC) + bind + listen(MAX_CLIENTS), like Python (std's
/// UnixListener::bind would listen with a backlog of 128 and block).
fn bind_listen(path: &Path) -> Result<UnixListener, OwnError> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: zeroed sockaddr_un is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        return Err(OwnError::new(libc::ENAMETOOLONG, "AF_UNIX path too long", path));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    // SAFETY: plain socket call.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(OwnError::last(path));
    }
    // SAFETY: fresh descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: addr is a valid sockaddr_un of at least len bytes.
    if unsafe { libc::bind(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) } != 0 {
        return Err(OwnError::last(path));
    }
    // SAFETY: bound socket.
    if unsafe { libc::listen(fd.as_raw_fd(), MAX_CLIENTS as i32) } != 0 {
        return Err(OwnError::last(path));
    }
    Ok(UnixListener::from(fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::tests::TempDir;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read};
    use std::time::Duration;

    fn opened(t: &TempDir) -> Probe {
        let mut p = Probe::with_path(t.0.join("mm").join("probe.sock"));
        p.try_open().unwrap();
        p
    }

    fn connect(p: &mut Probe) -> BufReader<UnixStream> {
        let c = UnixStream::connect(p.path()).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let fd = p.listener_fd().unwrap();
        p.handle(&[fd]);
        BufReader::new(c)
    }

    fn line(r: &mut BufReader<UnixStream>) -> String {
        let mut s = String::new();
        r.read_line(&mut s).unwrap();
        s
    }

    #[test]
    fn socket_is_private_and_greets_with_hello() {
        let t = TempDir::new("pr");
        let mut p = opened(&t);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(p.path()).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(t.0.join("mm")).unwrap().permissions().mode() & 0o777, 0o700);
        p.set_hello(json!({"model": "Magic Mouse", "res": 38.0, "x": [-1100, 1258]}));
        assert!(!p.has_clients());
        let mut c = connect(&mut p);
        assert!(p.has_clients());
        let hello: Value = serde_json::from_str(&line(&mut c)).unwrap();
        assert_eq!(hello, json!({"model": "Magic Mouse", "res": 38.0, "x": [-1100, 1258], "type": "hello"}));
        ProbeSink::send(&mut p, &json!({"t": 1.5, "c": [[0, 1, 2]], "scroll": false}));
        let l = line(&mut c);
        assert!(l.ends_with("\n") && !l.contains(' '), "compact separators: {l:?}");
        assert_eq!(serde_json::from_str::<Value>(&l).unwrap()["t"], 1.5);
        p.close();
        assert!(!p.path().exists(), "close unlinks the socket");
    }

    #[test]
    fn at_most_four_clients() {
        let t = TempDir::new("pr");
        let mut p = opened(&t);
        let mut cs: Vec<_> = (0..MAX_CLIENTS).map(|_| connect(&mut p)).collect();
        assert_eq!(p.client_count(), MAX_CLIENTS);
        let mut fifth = connect(&mut p);
        assert_eq!(p.client_count(), MAX_CLIENTS);
        let mut buf = Vec::new();
        assert_eq!(fifth.read_to_end(&mut buf).unwrap(), 0, "the fifth is closed at once");
        for c in &mut cs {
            assert!(line(c).contains("\"type\":\"hello\""));
        }
    }

    #[test]
    fn a_client_that_hangs_up_is_dropped_and_input_is_discarded() {
        let t = TempDir::new("pr");
        let mut p = opened(&t);
        let c = connect(&mut p);
        let fd = p.fds()[1];
        use std::io::Write;
        c.get_ref().write_all(b"ignored").unwrap();
        p.handle(&[fd]);
        assert_eq!(p.client_count(), 1);
        drop(c);
        p.handle(&[fd]);
        assert_eq!(p.client_count(), 0);
        assert_eq!(p.fds().len(), 1);
    }

    #[test]
    fn a_client_that_never_reads_gives_its_slot_back() {
        let t = TempDir::new("pr");
        let mut p = opened(&t);
        let _c = connect(&mut p);
        let frame = json!({"pad": "x".repeat(1000)});
        let mut sent = 0;
        while p.has_clients() && sent < 100_000 {
            p.send_obj(&frame);
            sent += 1;
        }
        assert!(!p.has_clients(), "still connected after {sent} frames");
        // the stall counter alone: once the buffer is full, more than MAX_STALLS frames
        assert!(sent > MAX_STALLS as usize);
    }

    #[test]
    fn stale_socket_is_replaced_live_one_and_other_files_refused() {
        let t = TempDir::new("pr");
        let path = t.0.join("mm").join("probe.sock");
        {
            let mut p = opened(&t);
            // a second daemon must not take over a live socket
            let mut q = Probe::with_path(path.clone());
            let e = q.try_open().unwrap_err();
            assert_eq!(e.errno, libc::EADDRINUSE, "{e}");
            assert!(p.is_open());
            // leave the socket file behind, as a crash would
            p.sock = None;
        }
        assert!(path.exists());
        let mut p = Probe::with_path(path.clone());
        p.try_open().unwrap();
        p.close();
        std::fs::write(&path, b"x").unwrap();
        let e = Probe::with_path(path.clone()).try_open().unwrap_err();
        assert!(e.to_string().contains("not a socket"), "{e}");
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }

    #[test]
    fn unsafe_directory_disables_the_probe() {
        let t = TempDir::new("pr");
        let dir = t.0.join("mm");
        std::fs::create_dir(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let mut p = Probe::with_path(dir.join("probe.sock"));
        assert!(p.try_open().unwrap_err().to_string().contains("writable by group or others"));
        assert!(!p.is_open() && p.fds().is_empty());
    }

    #[test]
    fn shared_handle_is_a_sink() {
        let t = TempDir::new("pr");
        let rc = Rc::new(RefCell::new(opened(&t)));
        let mut sink: Box<dyn ProbeSink> = Box::new(rc.clone());
        assert!(!sink.has_clients());
        let mut c = connect(&mut rc.borrow_mut());
        line(&mut c);
        sink.send(&json!({"type": "disconnected"}));
        assert_eq!(line(&mut c), "{\"type\":\"disconnected\"}\n");
    }
}
