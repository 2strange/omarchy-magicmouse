#!/usr/bin/python3
"""magic-mouse-config: read or change ~/.config/magic-mouse/config.toml, keeping comments.

File access goes through the daemon's read_own_file / write_own_file (no symlink
following, no blocking on FIFOs, owner/type/size checks, random O_EXCL temp + rename).

  magic-mouse-config get                       # effective config as JSON (defaults merged)
  magic-mouse-config defaults                  # built-in defaults as JSON
  magic-mouse-config set pointer.tracking_speed=1.0 scroll.natural=true
"""
import errno, json, os, re, stat, sys

CONFIG = os.path.expanduser(os.environ.get("MAGIC_MOUSE_CONFIG", "~/.config/magic-mouse/config.toml"))
HERE = os.path.dirname(os.path.realpath(__file__))


def daemon_module():
    # Same discipline as the daemon's read_own_file (which is not available yet):
    # descriptor-first, no symlink following, no blocking, must be our regular file.
    fd = os.open(os.path.join(HERE, "magic-mouse-daemon"), os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or st.st_size > (1 << 20):
            raise OSError(errno.EINVAL, "magic-mouse-daemon is not a small regular file owned by this user")
        src = os.read(fd, st.st_size + 1).decode("utf-8")
    finally:
        os.close(fd)
    mod = {"__name__": "magic_mouse_daemon"}
    exec(compile(src, "magic-mouse-daemon", "exec"), mod)
    mod["log"] = lambda msg: print(msg, file=sys.stderr)  # keep stdout clean for JSON
    return mod


def parse_value(raw: str):
    low = raw.strip().lower()
    if low in ("true", "false"):
        return low == "true"
    try:
        return int(raw) if re.fullmatch(r"-?\d+", raw.strip()) else float(raw)
    except ValueError:
        return raw.strip().strip('"')


def fmt(v) -> str:
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, float):
        s = f"{v:.4f}".rstrip("0").rstrip(".")
        return s if "." in s else s + ".0"
    return json.dumps(str(v))


def set_values(pairs: list[tuple[str, str, object]], mod: dict) -> None:
    try:
        text = mod["read_own_file"](CONFIG).decode("utf-8")
    except FileNotFoundError:
        text = ""
    lines = text.split("\n") if text else []
    for section, key, value in pairs:
        start = next((i for i, l in enumerate(lines) if l.strip() == f"[{section}]"), None)
        if start is None:
            if lines and lines[-1] != "":
                lines.append("")
            lines += [f"[{section}]", f"{key} = {fmt(value)}"]
            continue
        end = next((i for i in range(start + 1, len(lines)) if lines[i].strip().startswith("[")), len(lines))
        pat = re.compile(rf"^(\s*{re.escape(key)}\s*=\s*)([^#]*?)(\s*#.*)?$")
        for i in range(start + 1, end):
            m = pat.match(lines[i])
            if m:
                lines[i] = f"{m.group(1)}{fmt(value)}{m.group(3) or ''}"
                break
        else:
            lines.insert(start + 1, f"{key} = {fmt(value)}")
    mod["write_own_file"](CONFIG, "\n".join(lines).encode("utf-8"))


def main(argv: list[str]) -> int:
    if len(argv) >= 1 and argv[0] == "get":
        mod = daemon_module()
        print(json.dumps(mod["load_config"](CONFIG)))
        return 0
    if len(argv) == 1 and argv[0] == "defaults":
        print(json.dumps(daemon_module()["DEFAULTS"]))
        return 0
    if len(argv) >= 2 and argv[0] == "set":
        pairs = []
        for item in argv[1:]:
            if "=" not in item or "." not in item.split("=", 1)[0]:
                print(f"bad assignment: {item} (want section.key=value)", file=sys.stderr)
                return 2
            path, raw = item.split("=", 1)
            section, key = path.split(".", 1)
            pairs.append((section, key, parse_value(raw)))
        try:
            set_values(pairs, daemon_module())
        except OSError as exc:
            print(f"config: {exc}", file=sys.stderr)
            return 1
        return 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
