//! Own files and the bar's status file (Python: `MAX_OWN_FILE_BYTES`, `read_own_file`,
//! `open_private_dir`, `write_own_file`, `STATUS_DIR`, `STATUS_FILE`, `write_status`), plus
//! `py_json`, a `json.dumps` work-alike the probe and `magic-mouse-config get` output rely on.
//!
//! Every file the daemon reads or writes on the user's behalf (config, status) goes through
//! these helpers: descriptor-first, never following symlinks, never blocking on a planted FIFO,
//! and never writing through a predictable temporary name. Errors read like Python's OSError
//! (`[Errno 1] directory not owned by this user: '/path'`), so the journal stays the same.
use std::ffi::CString;
use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::iface::log;

pub const MAX_OWN_FILE_BYTES: usize = 1 << 20;

/// Python `OSError(errno, msg, path)`.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnError {
    pub errno: i32,
    pub msg: String,
    pub path: String,
}

impl OwnError {
    pub fn new(errno: i32, msg: impl Into<String>, path: &Path) -> Self {
        OwnError { errno, msg: msg.into(), path: path.to_string_lossy().into_owned() }
    }
    /// From an errno, with the C library's text (what Python shows).
    pub fn os(errno: i32, path: &Path) -> Self {
        let text = std::io::Error::from_raw_os_error(errno).to_string();
        let text = match text.rfind(" (os error ") {
            Some(i) => text[..i].to_string(),
            None => text,
        };
        Self::new(errno, text, path)
    }
    pub fn last(path: &Path) -> Self {
        Self::os(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO), path)
    }
    /// Python FileNotFoundError.
    pub fn is_not_found(&self) -> bool {
        self.errno == libc::ENOENT
    }
}

impl fmt::Display for OwnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[Errno {}] {}: {}", self.errno, self.msg, py_repr_str(&self.path))
    }
}

impl std::error::Error for OwnError {}

fn cpath(path: &Path) -> Result<CString, OwnError> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| OwnError::new(libc::EINVAL, "embedded null byte", path))
}

pub(crate) fn fstat(fd: RawFd, path: &Path) -> Result<libc::stat, OwnError> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fd is open, st is a valid out pointer.
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } != 0 {
        return Err(OwnError::last(path));
    }
    // SAFETY: fstat succeeded and filled it.
    Ok(unsafe { st.assume_init() })
}

fn open_at(dirfd: RawFd, path: &Path, flags: i32, mode: libc::mode_t) -> Result<OwnedFd, OwnError> {
    let c = cpath(path)?;
    loop {
        // SAFETY: c is a valid C string; the returned fd is owned below.
        let fd = unsafe { libc::openat(dirfd, c.as_ptr(), flags, mode as libc::c_uint) };
        if fd >= 0 {
            // SAFETY: fresh descriptor, nobody else owns it.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let e = OwnError::last(path);
        if e.errno != libc::EINTR {
            return Err(e);
        }
    }
}

fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

fn is_reg(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
}

/// Read a small regular file owned by this user. O_NOFOLLOW so a planted symlink is not
/// followed, O_NONBLOCK so a planted FIFO cannot block us, fstat (type, owner, size) before any
/// read, and a bounded read.
pub fn read_own_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>, OwnError> {
    read_own_file_as(path, max_bytes, uid())
}

pub(crate) fn read_own_file_as(path: &Path, max_bytes: usize, uid: u32) -> Result<Vec<u8>, OwnError> {
    let fd = open_at(libc::AT_FDCWD, path, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC, 0)?;
    let st = fstat(fd.as_raw_fd(), path)?;
    if !is_reg(&st) {
        return Err(OwnError::new(libc::EINVAL, "not a regular file", path));
    }
    if st.st_uid != uid {
        return Err(OwnError::new(libc::EPERM, "not owned by this user", path));
    }
    let too_big = || OwnError::new(libc::EFBIG, format!("larger than {max_bytes} bytes"), path);
    if st.st_size as u64 > max_bytes as u64 {
        return Err(too_big());
    }
    let mut out = Vec::new();
    let mut buf = vec![0u8; 65536];
    loop {
        // SAFETY: buf is valid for buf.len() bytes.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = OwnError::last(path);
            if e.errno == libc::EINTR {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
        if out.len() > max_bytes {
            return Err(too_big());
        }
    }
    Ok(out)
}

/// Create `directory` owner-only (0700) if it is missing, and return a descriptor for it. An
/// existing directory is accepted only if it is a real directory (not a symlink) owned by this
/// user that group and others cannot write to. Anything else is refused, not repaired.
pub fn open_private_dir(directory: &Path) -> Result<OwnedFd, OwnError> {
    open_private_dir_as(directory, uid())
}

pub(crate) fn open_private_dir_as(directory: &Path, uid: u32) -> Result<OwnedFd, OwnError> {
    let c = cpath(directory)?;
    // SAFETY: valid C string.
    if unsafe { libc::mkdir(c.as_ptr(), 0o700) } != 0 {
        let e = OwnError::last(directory);
        if e.errno != libc::EEXIST {
            return Err(e);
        }
    }
    let dfd = open_at(libc::AT_FDCWD, directory, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0)?;
    let st = fstat(dfd.as_raw_fd(), directory)?;
    if st.st_uid != uid {
        return Err(OwnError::new(libc::EPERM, "directory not owned by this user", directory));
    }
    if st.st_mode & (libc::S_IWGRP | libc::S_IWOTH) != 0 {
        return Err(OwnError::new(
            libc::EPERM,
            format!("directory is writable by group or others (mode {:04o})", st.st_mode & 0o777),
            directory,
        ));
    }
    Ok(dfd)
}

fn random_hex8() -> String {
    let mut b = [0u8; 8];
    let mut got = 0;
    while got < b.len() {
        // SAFETY: the buffer tail is valid for the requested length.
        let n = unsafe { libc::getrandom(b[got..].as_mut_ptr().cast(), b.len() - got, 0) };
        if n > 0 {
            got += n as usize;
        } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            // no entropy source at all: time and pid still give a fresh O_EXCL name
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            b = ((t as u64) ^ ((std::process::id() as u64) << 32)).to_le_bytes();
            break;
        }
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Atomically replace `path` with `data`. The directory must be owned by this user and is
/// opened O_NOFOLLOW and held (with `private_dir`, it is also created 0700 and refused if group
/// or others can write to it); the existing target may only be a regular file; the data goes to
/// a random same-directory O_CREAT|O_EXCL|O_NOFOLLOW temp file, is fsynced through that
/// descriptor, then renamed into place; the temp file is removed on any failure.
pub fn write_own_file(path: &Path, data: &[u8], mode: u32, private_dir: bool) -> Result<(), OwnError> {
    write_own_file_as(path, data, mode, private_dir, uid())
}

pub(crate) fn write_own_file_as(path: &Path, data: &[u8], mode: u32, private_dir: bool, uid: u32) -> Result<(), OwnError> {
    let directory = path.parent().unwrap_or(Path::new(""));
    let name = Path::new(path.file_name().ok_or_else(|| OwnError::new(libc::EISDIR, "Is a directory", path))?);
    let dfd = if private_dir {
        open_private_dir_as(directory, uid)?
    } else {
        if !directory.as_os_str().is_empty() {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)
                .map_err(|e| OwnError::os(e.raw_os_error().unwrap_or(libc::EIO), directory))?;
        }
        let d = if directory.as_os_str().is_empty() { Path::new(".") } else { directory };
        open_at(libc::AT_FDCWD, d, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0)?
    };
    if fstat(dfd.as_raw_fd(), directory)?.st_uid != uid {
        return Err(OwnError::new(libc::EPERM, "directory not owned by this user", directory));
    }
    let cname = cpath(name)?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid fd, C string and out pointer.
    if unsafe { libc::fstatat(dfd.as_raw_fd(), cname.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        // SAFETY: fstatat succeeded.
        if !is_reg(&unsafe { st.assume_init() }) {
            return Err(OwnError::new(libc::EINVAL, "existing target is not a regular file", path));
        }
    } else {
        let e = OwnError::last(path);
        if !e.is_not_found() {
            return Err(e);
        }
    }
    let tmp = PathBuf::from(format!(".{}.{}.tmp", name.to_string_lossy(), random_hex8()));
    let ctmp = cpath(&tmp)?;
    let tfd = open_at(
        dfd.as_raw_fd(),
        &tmp,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        mode as libc::mode_t,
    )?;
    let result = (|| {
        let mut rest = data;
        while !rest.is_empty() {
            // SAFETY: rest is valid for rest.len() bytes.
            let n = unsafe { libc::write(tfd.as_raw_fd(), rest.as_ptr().cast(), rest.len()) };
            if n < 0 {
                let e = OwnError::last(&tmp);
                if e.errno == libc::EINTR {
                    continue;
                }
                return Err(e);
            }
            rest = &rest[n as usize..];
        }
        // SAFETY: valid fd.
        if unsafe { libc::fsync(tfd.as_raw_fd()) } != 0 {
            return Err(OwnError::last(&tmp));
        }
        // SAFETY: valid fds and C strings.
        if unsafe { libc::renameat(dfd.as_raw_fd(), ctmp.as_ptr(), dfd.as_raw_fd(), cname.as_ptr()) } != 0 {
            return Err(OwnError::last(&tmp));
        }
        Ok(())
    })();
    if result.is_err() {
        // SAFETY: valid fd and C string.
        unsafe { libc::unlinkat(dfd.as_raw_fd(), ctmp.as_ptr(), 0) };
    }
    drop(tfd);
    result?;
    // SAFETY: valid fd.
    if unsafe { libc::fsync(dfd.as_raw_fd()) } != 0 {
        return Err(OwnError::last(directory));
    }
    Ok(())
}

// --------------------------------------------------------------------------- status

/// Python `STATUS_DIR`: `$XDG_RUNTIME_DIR/magic-mouse` (`/run/user/<uid>/magic-mouse` without it).
pub fn status_dir() -> PathBuf {
    let rt = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", uid())));
    rt.join("magic-mouse")
}

/// Python `STATUS_FILE`: `<status_dir>/battery.json`.
pub fn status_file() -> PathBuf {
    status_dir().join("battery.json")
}

/// Publish `data` for the bar widget (battery.json, 0600 in a 0700 directory). If the status
/// directory is not safe (not ours, or group/other-writable) nothing is written and the daemon
/// keeps running; the reason goes to the journal and the widget falls back to UPower.
pub fn write_status(data: &Value) {
    write_status_to(&status_file(), data, &mut |m| log(m));
}

/// `write_status` with an explicit file and logger (tests, or a cached path).
pub fn write_status_to(file: &Path, data: &Value, log: &mut dyn FnMut(&str)) {
    write_status_as(file, data, log, uid());
}

pub(crate) fn write_status_as(file: &Path, data: &Value, log: &mut dyn FnMut(&str), uid: u32) {
    if let Err(exc) = write_own_file_as(file, py_json(data, false).as_bytes(), 0o600, true, uid) {
        let dir = file.parent().unwrap_or(Path::new(""));
        log(&format!(
            "status: not publishing battery status to {}: {exc} (fix or remove that directory; the next status write retries)",
            dir.display()
        ));
    }
}

// --------------------------------------------------------------------------- Python-compatible output

/// Python `json.dumps(v)` (`compact` = `separators=(",", ":")`): ensure_ascii escapes, Python
/// float repr (`1.0`, `1e-05`, `1e+16`), map keys in the Map's iteration order (insertion order
/// with serde_json's `preserve_order` feature, sorted without it).
pub fn py_json(v: &Value, compact: bool) -> String {
    let mut out = String::new();
    let (item, key) = if compact { (",", ":") } else { (", ", ": ") };
    write_json(&mut out, v, item, key);
    out
}

fn write_json(out: &mut String, v: &Value, item: &str, key: &str) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else {
                out.push_str(&py_json_float(n.as_f64().unwrap_or(f64::NAN)));
            }
        }
        Value::String(s) => py_json_str(out, s),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(item);
                }
                write_json(out, x, item, key);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(item);
                }
                py_json_str(out, k);
                out.push_str(key);
                write_json(out, x, item, key);
            }
            out.push('}');
        }
    }
}

/// A float as `json.dumps` writes it (NaN / Infinity like Python's allow_nan default).
pub fn py_json_float(x: f64) -> String {
    if x.is_nan() {
        "NaN".into()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        py_float_repr(x)
    }
}

/// Python `repr(float)`: shortest round-trip digits, fixed notation for 1e-4 <= |x| < 1e16,
/// otherwise `1.5e-07` / `1e+16`.
pub fn py_float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sci = format!("{x:e}"); // shortest digits: "-1.2345e3", "1e-5", "0e0"
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (neg, mant) = match mant.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mant),
    };
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    if !(-4..16).contains(&exp) {
        s.push_str(&digits[..1]);
        if digits.len() > 1 {
            s.push('.');
            s.push_str(&digits[1..]);
        }
        s.push_str(&format!("e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs()));
    } else if exp >= 0 {
        let point = exp as usize + 1;
        if digits.len() <= point {
            s.push_str(&digits);
            s.push_str(&"0".repeat(point - digits.len()));
            s.push_str(".0");
        } else {
            s.push_str(&digits[..point]);
            s.push('.');
            s.push_str(&digits[point..]);
        }
    } else {
        s.push_str("0.");
        s.push_str(&"0".repeat((-exp - 1) as usize));
        s.push_str(&digits);
    }
    s
}

fn py_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Python `repr(str)` (close enough for log lines).
pub fn py_repr_str(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::from(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    //! Port of tests/test_status_dir.py (+ the reader handshake from test_status_reader.py).
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::sync::atomic::{AtomicU32, Ordering};

    pub(crate) struct TempDir(pub PathBuf);
    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!("mm-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = chmod_all(&self.0);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn chmod_all(p: &Path) -> std::io::Result<()> {
        if std::fs::symlink_metadata(p)?.is_dir() {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
            for e in std::fs::read_dir(p)? {
                chmod_all(&e?.path())?;
            }
        }
        Ok(())
    }

    struct Case {
        tmp: TempDir,
        dir: PathBuf,
        file: PathBuf,
        logged: Vec<String>,
    }
    impl Case {
        fn new() -> Self {
            let tmp = TempDir::new("st");
            let dir = tmp.0.join("magic-mouse");
            let file = dir.join("battery.json");
            Case { tmp, dir, file, logged: vec![] }
        }
        fn write_status(&mut self, data: Value) {
            let logged = &mut self.logged;
            write_status_to(&self.file, &data, &mut |m| logged.push(m.to_string()));
        }
        fn listdir(&self, p: &Path) -> Vec<String> {
            std::fs::read_dir(p).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
        }
    }
    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }
    fn chmod(p: &Path, m: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    }
    fn mkfifo(p: &Path) {
        let c = cpath(p).unwrap();
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn missing_directory_is_created_owner_only() {
        let mut c = Case::new();
        // even a permissive umask must not widen it (umask is process-wide: restore at once)
        // SAFETY: umask cannot fail.
        let old = unsafe { libc::umask(0) };
        c.write_status(json!({"connected": true, "percent": 50}));
        // SAFETY: as above.
        unsafe { libc::umask(old) };
        assert_eq!(mode(&c.dir), 0o700);
        assert_eq!(mode(&c.file), 0o600);
        let v: Value = serde_json::from_slice(&std::fs::read(&c.file).unwrap()).unwrap();
        assert_eq!(v["percent"], 50);
        assert!(c.logged.is_empty(), "{:?}", c.logged);
    }

    #[test]
    fn owner_only_and_group_readable_directories_are_accepted() {
        let c = Case::new();
        for m in [0o700, 0o750, 0o755, 0o500] {
            let _ = std::fs::create_dir(&c.dir);
            chmod(&c.dir, m);
            open_private_dir(&c.dir).unwrap_or_else(|e| panic!("mode {m:o}: {e}"));
            chmod(&c.dir, 0o700);
        }
    }

    #[test]
    fn group_or_other_writable_directory_is_refused() {
        let mut c = Case::new();
        std::fs::DirBuilder::new().mode(0o700).create(&c.dir).unwrap();
        for m in [0o770, 0o707, 0o775, 0o757, 0o777, 0o1777] {
            chmod(&c.dir, m);
            let e = open_private_dir(&c.dir).unwrap_err();
            assert!(e.to_string().contains("writable by group or others"), "{e}");
            c.logged.clear();
            c.write_status(json!({"connected": true}));
            assert!(c.listdir(&c.dir).is_empty(), "nothing may be written there");
            assert_eq!(c.logged.len(), 1);
            assert!(c.logged[0].contains("not publishing battery status"));
            assert!(c.logged[0].contains(&*c.dir.to_string_lossy()));
        }
        assert_eq!(mode(&c.dir), 0o1777, "an unsafe directory is refused, not chmod-ed");
    }

    #[test]
    fn directory_owned_by_someone_else_is_refused() {
        let mut c = Case::new();
        std::fs::DirBuilder::new().mode(0o700).create(&c.dir).unwrap();
        let other = uid() + 1;
        let e = open_private_dir_as(&c.dir, other).unwrap_err();
        assert!(e.to_string().contains("not owned"), "{e}");
        let logged = &mut c.logged;
        write_status_as(&c.file, &json!({"connected": true}), &mut |m| logged.push(m.to_string()), other);
        assert!(c.listdir(&c.dir).is_empty());
        assert!(c.logged.last().unwrap().contains("not publishing battery status"));
    }

    #[test]
    fn symlinked_directory_is_refused() {
        let mut c = Case::new();
        let real = c.tmp.0.join("elsewhere");
        std::fs::DirBuilder::new().mode(0o700).create(&real).unwrap();
        std::os::unix::fs::symlink(&real, &c.dir).unwrap();
        c.write_status(json!({"connected": true}));
        assert!(c.listdir(&real).is_empty());
        assert!(c.logged.last().unwrap().contains("not publishing battery status"));
    }

    #[test]
    fn a_fifo_where_the_file_belongs_is_refused_without_blocking() {
        let mut c = Case::new();
        std::fs::DirBuilder::new().mode(0o700).create(&c.dir).unwrap();
        mkfifo(&c.file);
        c.write_status(json!({"connected": true}));
        use std::os::unix::fs::FileTypeExt;
        assert!(std::fs::symlink_metadata(&c.file).unwrap().file_type().is_fifo());
        assert!(c.logged.last().unwrap().contains("not a regular file"));
    }

    #[test]
    fn daemon_output_is_accepted_by_the_widget_reader() {
        let mut c = Case::new();
        c.write_status(json!({"connected": true, "model": "Magic Mouse 2", "percent": 64, "charging": true}));
        let reader = Path::new(env!("CARGO_MANIFEST_DIR")).join("../status-reader.py");
        let out = match std::process::Command::new("python3").arg("-I").arg(&reader).arg(&c.file).output() {
            Ok(o) => o,
            Err(_) => return, // no python3 here: the reader cannot be run
        };
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["percent"], 64);
    }

    #[test]
    fn other_files_keep_the_plain_owner_check() {
        // config.toml's directory is the user's own ~/.config/magic-mouse: group-writable is their call.
        let c = Case::new();
        let cfgdir = c.tmp.0.join("cfg");
        std::fs::create_dir(&cfgdir).unwrap();
        chmod(&cfgdir, 0o775);
        let target = cfgdir.join("config.toml");
        write_own_file(&target, b"x = 1\n", 0o600, false).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"x = 1\n");
        assert!(c.listdir(&cfgdir) == vec!["config.toml".to_string()], "temp file left behind");
    }

    #[test]
    fn rewrite_replaces_and_keeps_mode() {
        let mut c = Case::new();
        c.write_status(json!({"percent": 1}));
        c.write_status(json!({"percent": 2}));
        assert_eq!(std::fs::read_to_string(&c.file).unwrap(), "{\"percent\": 2}");
        assert_eq!(mode(&c.file), 0o600);
        assert_eq!(c.listdir(&c.dir), vec!["battery.json".to_string()]);
    }

    // ---- read_own_file (the config side; same rules the widget reader applies)
    #[test]
    fn read_own_file_refuses_symlink_fifo_dir_and_size() {
        let c = Case::new();
        let real = c.tmp.0.join("real");
        std::fs::write(&real, b"abc").unwrap();
        assert_eq!(read_own_file(&real, 3).unwrap(), b"abc");
        assert_eq!(read_own_file(&real, 2).unwrap_err().errno, libc::EFBIG);
        let link = c.tmp.0.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(read_own_file(&link, 100).unwrap_err().errno, libc::ELOOP);
        let fifo = c.tmp.0.join("fifo");
        mkfifo(&fifo);
        assert!(read_own_file(&fifo, 100).unwrap_err().to_string().contains("not a regular file"));
        assert!(read_own_file(&c.tmp.0, 100).unwrap_err().to_string().contains("not a regular file"));
        assert!(read_own_file_as(&real, 100, uid() + 1).unwrap_err().to_string().contains("not owned"));
        assert!(read_own_file(&c.tmp.0.join("nope"), 100).unwrap_err().is_not_found());
    }

    #[test]
    fn read_is_bounded_even_when_fstat_reports_no_size() {
        // /proc files are regular, ours and report st_size 0: only the bounded read stops them.
        let p = PathBuf::from(format!("/proc/{}/environ", std::process::id()));
        if let Ok(all) = std::fs::read(&p) {
            if all.len() > 64 {
                assert_eq!(read_own_file(&p, 64).unwrap_err().errno, libc::EFBIG);
            }
        }
    }

    #[test]
    fn error_text_reads_like_python() {
        let e = OwnError::new(libc::EPERM, "directory not owned by this user", Path::new("/x/y"));
        assert_eq!(e.to_string(), "[Errno 1] directory not owned by this user: '/x/y'");
        assert_eq!(OwnError::os(libc::ENOENT, Path::new("/a")).to_string(), "[Errno 2] No such file or directory: '/a'");
    }

    #[test]
    fn json_like_python() {
        let v = json!({"a": 1, "b": [1.0, 0.5, -0.0, 1e-5, 1e16, 123456789.125, 0.0001], "c": "ä\"\n\u{1f600}\u{7f}", "d": null, "e": true});
        assert_eq!(
            py_json(&v, false),
            r#"{"a": 1, "b": [1.0, 0.5, -0.0, 1e-05, 1e+16, 123456789.125, 0.0001], "c": "\u00e4\"\n\ud83d\ude00\u007f", "d": null, "e": true}"#
        );
        assert_eq!(py_json(&json!({"x": [1, {}], "y": []}), true), r#"{"x":[1,{}],"y":[]}"#);
        for (x, s) in [(1.5e-7, "1.5e-07"), (2.5e20, "2.5e+20"), (1e15, "1000000000000000.0"), (0.6875, "0.6875"), (100.0, "100.0"), (-3.25, "-3.25")] {
            assert_eq!(py_float_repr(x), s);
        }
        assert_eq!(py_json_float(f64::INFINITY), "Infinity");
        assert_eq!(py_repr_str("it's"), "\"it's\"");
    }
}
