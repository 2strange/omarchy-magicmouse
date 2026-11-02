#!/usr/bin/env bash
# omarchy-magic-mouse installer. Safe to re-run.
#
# Privilege model: no file from this checkout is ever executed as root. The
# only privileged step is one fixed Python helper embedded below (see the
# comment above it), run by the distro's python3; it carries the exact bytes
# of the three /etc files and installs them atomically with fail-closed checks.
# Access to the mouse comes from the device-specific uaccess udev rule alone:
# the installer never changes group membership, and stops if the rule did not
# take effect.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m==> error:\033[0m %s\n' "$*" >&2; exit 1; }
ASSUME_YES=0
MODE=auto  # auto: release binary, else local cargo build, else the Python daemon
usage() {
  cat <<'USAGE'
Usage: ./install.sh [--yes] [--build | --python]
  --yes      do not ask before appending the Hyprland snippet
  --build    build the daemon from daemon-rs/ with cargo instead of downloading it
  --python   install the Python daemon (bin/magic-mouse-daemon, needs python-evdev)
USAGE
}
for arg in "$@"; do case "$arg" in
  --yes|-y) ASSUME_YES=1 ;;
  --build) MODE=build ;;
  --python) MODE=python ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; die "unknown option: $arg" ;;
esac; done
ask() {  # ask "question" -> 0 for yes
  (( ASSUME_YES )) && return 0
  if [ -t 0 ]; then read -r -p "$1 [Y/n] " a; [[ -z $a || $a =~ ^[Yy] ]]; else return 0; fi
}

[ "$(id -u)" -ne 0 ] || die "run install.sh as your user, not as root (it asks for sudo itself, once)"

# ---------------------------------------------------------------------------
# The daemon. 2.0 is one Rust binary; users do not need Rust. In order:
#   1. the prebuilt binary attached to the GitHub release of this plugin's version
#      (manifest.json), over HTTPS, checked against the release's SHA-256 before it is
#      made executable or installed; it is never run here, and never as root;
#   2. a local build with cargo, when cargo is installed (daemon-rs/);
#   3. the Python daemon (bin/magic-mouse-daemon) with python-evdev, as up to 1.4.
# ---------------------------------------------------------------------------
REPO="2strange/omarchy-magicmouse"
ASSET="magic-mouse-daemon-x86_64-linux"
dest="$HOME/.local/bin/magic-mouse-daemon"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

put_daemon() {  # put_daemon file: atomic replace, also while the old daemon runs (no "text file busy")
  mkdir -p "$(dirname "$dest")"
  install -m755 "$1" "$dest.new.$$" && mv -f "$dest.new.$$" "$dest" || { rm -f "$dest.new.$$"; return 1; }
}

pkg_add() {  # pkg_add package
  if command -v omarchy >/dev/null 2>&1; then omarchy pkg add "$1"; else sudo pacman -S --needed --noconfirm "$1"; fi
}

daemon_download() {
  [ "$(uname -m)" = x86_64 ] || { say "no prebuilt daemon for $(uname -m)"; return 1; }
  command -v curl >/dev/null 2>&1 || { say "curl is missing: cannot download the daemon"; return 1; }
  local version url
  version="$(/usr/bin/python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))["version"])' "$here/manifest.json")" \
    || { say "cannot read the version from manifest.json"; return 1; }
  [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([.+-][0-9A-Za-z.-]+)?$ ]] || { say "odd version in manifest.json: $version"; return 1; }
  url="https://github.com/$REPO/releases/download/v$version/$ASSET"
  say "downloading the daemon $version: $url"
  local get=(curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location --retry 2 --max-time 120)
  "${get[@]}" -o "$tmp/$ASSET" "$url" && "${get[@]}" -o "$tmp/$ASSET.sha256" "$url.sha256" \
    || { say "download failed (no release asset for v$version, or offline)"; return 1; }
  local want got
  want="$(head -c 64 "$tmp/$ASSET.sha256")"
  # A download that arrived but does not check out is not "offline": stop, do not fall back quietly.
  [[ $want =~ ^[0-9a-f]{64}$ ]] || die "the downloaded .sha256 is not a SHA-256 sum: not installing anything"
  got="$(sha256sum "$tmp/$ASSET" | cut -d' ' -f1)"
  [ "$got" = "$want" ] || die "checksum mismatch for $ASSET (got $got, want $want): not installing it"
  [ "$(head -c 4 "$tmp/$ASSET" | od -An -c | tr -d ' ')" = '177ELF' ] || die "$ASSET is not an ELF binary: not installing it"
  put_daemon "$tmp/$ASSET" || return 1
  say "daemon $version (SHA-256 $got) -> ~/.local/bin/magic-mouse-daemon"
}

daemon_build() {
  [ -f "$here/daemon-rs/Cargo.toml" ] || { say "no daemon-rs/ in this checkout"; return 1; }
  command -v cargo >/dev/null 2>&1 || { say "cargo is missing: cannot build the daemon"; return 1; }
  say "building the daemon with cargo (a few minutes)"
  cargo build --release --locked --manifest-path "$here/daemon-rs/Cargo.toml" || { say "cargo build failed"; return 1; }
  put_daemon "$here/daemon-rs/target/release/magic-mouse-daemon" || return 1
  say "daemon (built here) -> ~/.local/bin/magic-mouse-daemon"
}

daemon_python() {
  if ! /usr/bin/python3 -c 'import evdev' 2>/dev/null; then
    say "installing python-evdev"
    pkg_add python-evdev || die "could not install python-evdev"
  fi
  if ! /usr/bin/python3 -c 'import gi' 2>/dev/null; then
    say "installing python-gobject (battery in Bluetooth settings)"
    pkg_add python-gobject || say "python-gobject not installed: Bluetooth settings will not show the charge (the bar still does)"
  fi
  put_daemon "$here/bin/magic-mouse-daemon" || die "could not install the daemon to $dest"
  say "Python daemon -> ~/.local/bin/magic-mouse-daemon"
}

case "$MODE" in
  auto) daemon_download || daemon_build || { say "falling back to the Python daemon"; daemon_python; } ;;
  build) daemon_build || die "could not build the daemon" ;;
  python) daemon_python ;;
esac
# The helpers run whichever daemon is installed next to them (the .py ones for the Python daemon).
for f in magic-mouse-config magic-mouse-config.py magic-mouse-battery-query magic-mouse-battery-query.py; do
  install -Dm755 "$here/bin/$f" "$HOME/.local/bin/$f"
done
install -Dm644 "$here/config/magic-mouse.service" "$HOME/.config/systemd/user/magic-mouse.service"
if [ ! -f "$HOME/.config/magic-mouse/config.toml" ]; then
  install -Dm644 "$here/config/config.toml" "$HOME/.config/magic-mouse/config.toml"
  say "config -> ~/.config/magic-mouse/config.toml"
fi

# ---------------------------------------------------------------------------
# Root step. One fixed helper, embedded here so the exact bytes are in the
# reviewed file, handed to the distro's python3 on the command line: it is in
# memory before sudo runs, so nothing on disk can be swapped in between the
# password prompt and execution. It walks to each target directory one
# component at a time with O_NOFOLLOW and holds the directory descriptor,
# insists on root-owned, non-world-writable directories, writes to an
# O_EXCL temp file in that directory, re-reads and verifies through the same
# descriptor, renames atomically, verifies again, and refuses if the existing
# entry is a symlink or anything but a regular file.
# ---------------------------------------------------------------------------
udev_path=/etc/udev/rules.d/70-magic-mouse.rules
modprobe_path=/etc/modprobe.d/hid_magicmouse.conf
modules_path=/etc/modules-load.d/magic-mouse.conf
root_helper=$(cat <<'PY'
import hashlib, os, stat, subprocess, sys

UDEV = b"""# omarchy-magic-mouse: grant the logged-in user access to the Magic Mouse and to
# uinput (for the virtual mouse) via logind's per-seat uaccess ACL. No group
# changes, no world-readable nodes; only these device IDs.
KERNEL=="uinput", SUBSYSTEM=="misc", OPTIONS+="static_node=uinput", TAG+="uaccess"
SUBSYSTEM=="input", KERNEL=="event*", ATTRS{id/vendor}=="004c", ATTRS{id/product}=="0269", TAG+="uaccess"
SUBSYSTEM=="input", KERNEL=="event*", ATTRS{id/vendor}=="004c", ATTRS{id/product}=="0323", TAG+="uaccess"
SUBSYSTEM=="input", KERNEL=="event*", ATTRS{id/vendor}=="004c", ATTRS{id/product}=="030d", TAG+="uaccess"
SUBSYSTEM=="input", KERNEL=="event*", ATTRS{id/vendor}=="05ac", ATTRS{id/product}=="030d", TAG+="uaccess"
# Raw HID node too, so the daemon can ask the mouse for its battery level.
SUBSYSTEM=="hidraw", KERNELS=="0005:004C:0269.*", TAG+="uaccess"
SUBSYSTEM=="hidraw", KERNELS=="0005:004C:0323.*", TAG+="uaccess"
SUBSYSTEM=="hidraw", KERNELS=="0005:05AC:030D.*", TAG+="uaccess"
"""
MODPROBE = b"""# omarchy-magic-mouse: let the mouse's own firmware decide left vs right click
# (like macOS) instead of the driver's three fixed touch zones, which also
# invent a middle button that macOS never had. report_undeciphered passes on the
# mouse's own contact state (hovering, touching, lifting), the way macOS tells a
# touch from a finger just above the surface.
options hid_magicmouse emulate_3button=0 report_undeciphered=1
"""
MODULES = b"""# omarchy-magic-mouse: load uinput at boot so the udev rule can grant access to
# /dev/uinput. Until the module is loaded that node is a root-only placeholder,
# and opening it as the user can't trigger the autoload.
uinput
"""
TARGETS = (
    (("etc", "udev", "rules.d"), "70-magic-mouse.rules", UDEV),
    (("etc", "modprobe.d"), "hid_magicmouse.conf", MODPROBE),
    (("etc", "modules-load.d"), "magic-mouse.conf", MODULES),
)
DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC


def die(msg):
    sys.stderr.write("root helper: " + msg + "\n")
    sys.exit(1)


def open_dir(parts):
    """Walk from / one component at a time, never following symlinks, and
    return a descriptor for the final directory. Every component must be a
    root-owned directory that only root can write to."""
    fd = os.open("/", DIR_FLAGS)
    path = ""
    for part in parts:
        path += "/" + part
        try:
            nfd = os.open(part, DIR_FLAGS, dir_fd=fd)
        except OSError as e:
            die("cannot open directory %s: %s" % (path, e.strerror))
        os.close(fd)
        fd = nfd
        st = os.fstat(fd)
        if not stat.S_ISDIR(st.st_mode):
            die("%s is not a directory" % path)
        if st.st_uid != 0:
            die("%s is not owned by root" % path)
        if st.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
            die("%s is writable by non-root users" % path)
    return fd


def install(parts, name, data):
    path = "/" + "/".join(parts) + "/" + name
    dfd = open_dir(parts)
    try:
        st = os.stat(name, dir_fd=dfd, follow_symlinks=False)
    except FileNotFoundError:
        pass
    else:
        if not stat.S_ISREG(st.st_mode):
            die("%s exists and is not a regular file; refusing to replace it" % path)
    tmp = ".%s.tmp.%d" % (name, os.getpid())
    try:
        tfd = os.open(tmp, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o644, dir_fd=dfd)
    except OSError as e:
        die("cannot create %s/%s: %s" % (os.path.dirname(path), tmp, e.strerror))
    try:
        view = memoryview(data)
        while view:
            n = os.write(tfd, view)
            view = view[n:]
        os.fsync(tfd)
        st = os.fstat(tfd)
        if not stat.S_ISREG(st.st_mode) or st.st_nlink != 1 or st.st_size != len(data):
            die("%s: temporary file changed under us" % path)
        if os.pread(tfd, len(data) + 1, 0) != data:
            die("%s: temporary file content does not match" % path)
        os.fchown(tfd, 0, 0)
        os.fchmod(tfd, 0o644)
        os.rename(tmp, name, src_dir_fd=dfd, dst_dir_fd=dfd)
    except BaseException:
        try:
            os.unlink(tmp, dir_fd=dfd)
        except OSError:
            pass
        raise
    finally:
        os.close(tfd)
    os.fsync(dfd)
    ffd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=dfd)
    try:
        st = os.fstat(ffd)
        if not stat.S_ISREG(st.st_mode) or st.st_uid != 0 or st.st_nlink != 1:
            die("%s: not a root-owned regular file after install" % path)
        if os.pread(ffd, len(data) + 1, 0) != data:
            die("%s: content mismatch after install" % path)
    finally:
        os.close(ffd)
        os.close(dfd)
    return hashlib.sha256(data).hexdigest()


if os.geteuid() != 0:
    die("must run as root (install.sh runs it via sudo)")
os.umask(0o022)
digests = [install(*t) for t in TARGETS]
for param, value in (("emulate_3button", b"0\n"), ("report_undeciphered", b"1\n")):
    try:
        pfd = os.open(f"/sys/module/hid_magicmouse/parameters/{param}", os.O_WRONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    except FileNotFoundError:
        continue  # driver not loaded yet; the modprobe option applies when it loads
    os.write(pfd, value)
    os.close(pfd)
# Always load it: /dev/uinput exists as a static placeholder even when the module
# is not loaded, and then there is no device for the rule to apply to.
subprocess.run(["/usr/bin/modprobe", "uinput"], check=True)
subprocess.run(["/usr/bin/udevadm", "control", "--reload"], check=True)
subprocess.run(["/usr/bin/udevadm", "trigger", "--subsystem-match=misc", "--subsystem-match=input",
                "--subsystem-match=hidraw", "--action=add"], check=False)
# The ACL checks in install.sh run right after this; let udev finish first.
subprocess.run(["/usr/bin/udevadm", "settle", "--timeout=10"], check=False)
print(" ".join(digests))
PY
)

[ -t 0 ] || die "the root step uses sudo and needs a terminal: run ./install.sh from an interactive shell inside your desktop session"
say "root step (needs your password): the helper embedded in install.sh writes $udev_path, $modprobe_path and $modules_path atomically, loads uinput, then reloads udev"
digests="$(sudo /usr/bin/python3 -I -c "$root_helper")"
read -r udev_sha modprobe_sha modules_sha <<<"$digests"
[ "$(sha256sum "$udev_path" | cut -d' ' -f1)" = "$udev_sha" ]         || die "$udev_path does not match what the helper wrote"
[ "$(sha256sum "$modprobe_path" | cut -d' ' -f1)" = "$modprobe_sha" ] || die "$modprobe_path does not match what the helper wrote"
[ "$(sha256sum "$modules_path" | cut -d' ' -f1)" = "$modules_sha" ]   || die "$modules_path does not match what the helper wrote"

# Fail closed: the uaccess ACL must actually have reached the device nodes.
acl_help="uaccess ACLs are granted by systemd-logind to the active seat session. Run install.sh from a terminal inside your Hyprland session (not over SSH), and check: getfacl /dev/uinput"
[ -r /dev/uinput ] && [ -w /dev/uinput ] || die "your user cannot open /dev/uinput after installing the udev rule. $acl_help"
for dev in /sys/class/input/event*; do
  [ -e "$dev/device/id/vendor" ] || continue
  vid=$(<"$dev/device/id/vendor"); pid=$(<"$dev/device/id/product")
  case "$vid:$pid" in
    004c:0269|004c:0323|004c:030d|05ac:030d)
      node="/dev/input/${dev##*/}"
      [ -r "$node" ] || die "your user cannot read $node (the Magic Mouse) after installing the udev rule. $acl_help"
      ;;
  esac
done
say "device access OK (uaccess ACL on /dev/uinput and the mouse)"

hypr_lua="$HOME/.config/hypr/input.lua"; hypr_conf="$HOME/.config/hypr/input.conf"
marker="omarchy-magic-mouse"
hypr_target=""; hypr_snippet=""
if [ -f "$hypr_lua" ] && grep -q 'require("hypr.input")' "$HOME/.config/hypr/hyprland.lua" 2>/dev/null; then
  hypr_target="$hypr_lua"; hypr_snippet="$here/config/hypr-input.lua"
elif [ -f "$hypr_conf" ]; then
  hypr_target="$hypr_conf"; hypr_snippet="$here/config/hypr-input.conf"
fi
if [ -z "$hypr_target" ]; then
  say "WARNING: no ~/.config/hypr/input.lua or input.conf found; add config/hypr-input.lua yourself"
elif grep -q "$marker" "$hypr_target"; then
  say "Hyprland snippet already present in $hypr_target"
elif ask "Append the 7-line 'magic-mouse-omarchy' device block to $hypr_target? (needed so Hyprland doesn't re-accelerate the pointer)"; then
  cp "$hypr_target" "$hypr_target.bak.magic-mouse.$(date +%s)"
  cat "$hypr_snippet" >> "$hypr_target"
  say "Hyprland snippet appended to $hypr_target (backup written beside it)"
else
  say "Skipped. Add the block from $hypr_snippet to your Hyprland input config by hand."
fi
if command -v hyprctl >/dev/null 2>&1 && [ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]; then
  hyprctl reload >/dev/null || true
  errs="$(hyprctl configerrors 2>/dev/null || true)"
  [ -z "$errs" ] || { echo "$errs"; say "WARNING: Hyprland reported config errors above"; }
fi

plugin_id="io.github.2strange.magicmouse"
old_id="io.github.maikunari.magic-mouse"  # the plugin this one grew out of: same daemon, unit and config
if grep -q "$old_id" "$HOME/.config/omarchy/shell.json" 2>/dev/null; then
  say "replacing the bar widget $old_id with $plugin_id"
  omarchy plugin disable "$old_id" >/dev/null 2>&1 || say "WARNING: could not disable $old_id; remove it with: omarchy plugin remove $old_id"
fi
plugins_dir="$HOME/.config/omarchy/plugins"
if command -v omarchy-shell >/dev/null 2>&1; then
  if [ "$here" = "$plugins_dir/$plugin_id" ]; then
    say "bar widget: running from the installed plugin checkout, nothing to copy"
  elif [ -d "$plugins_dir/$plugin_id/.git" ]; then
    say "bar widget: git-managed copy already installed (update it with: omarchy plugin update $plugin_id)"
  else
    say "bar widget -> $plugins_dir/$plugin_id"
    rm -rf "$plugins_dir/$plugin_id"
    mkdir -p "$plugins_dir/$plugin_id"
    cp -r "$here/manifest.json" "$here/BarWidget.qml" "$here/Panel.qml" "$here/status-reader.py" "$here/README.md" "$here/LICENSE" "$plugins_dir/$plugin_id/"
    [ -f "$here/preview-touch.png" ] && cp "$here/preview-touch.png" "$plugins_dir/$plugin_id/"
    cp -r "$here/preview" "$plugins_dir/$plugin_id/"  # the touch preview window
  fi
  omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true
  sleep 1
  if ! grep -q "$plugin_id" "$HOME/.config/omarchy/shell.json" 2>/dev/null; then
    omarchy plugin enable "$plugin_id" --section right >/dev/null 2>&1 || say "WARNING: could not enable the bar widget; try: omarchy plugin enable $plugin_id --section right"
  fi
fi

say "enabling magic-mouse.service (user)"
systemctl --user daemon-reload
systemctl --user enable magic-mouse.service
systemctl --user restart magic-mouse.service  # a running daemon (maybe the old Python one) picks up the new one
sleep 1
systemctl --user --no-pager --lines=5 status magic-mouse.service || true

say "done. Turn each Magic Mouse off and on once: the driver reads its new options when a mouse connects."
say "Tune ~/.config/magic-mouse/config.toml (applies live). Logs: journalctl --user -u magic-mouse -f"
