"""Checks for how the daemon creates and trusts its status directory,
$XDG_RUNTIME_DIR/magic-mouse. Needs python-evdev, like the daemon itself.

Run from the repo root:  /usr/bin/python3 -m unittest discover -s tests -v
"""
import importlib.machinery
import importlib.util
import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DAEMON = os.path.join(ROOT, "bin", "magic-mouse-daemon")
READER = os.path.join(ROOT, "status-reader.py")

try:
    loader = importlib.machinery.SourceFileLoader("magic_mouse_daemon", DAEMON)
    spec = importlib.util.spec_from_loader("magic_mouse_daemon", loader)
    daemon = importlib.util.module_from_spec(spec)
    loader.exec_module(daemon)
except ImportError as exc:  # python-evdev missing
    daemon = None
    SKIP = f"cannot import the daemon: {exc}"


@unittest.skipIf(daemon is None, "cannot import the daemon (python-evdev missing?)")
class StatusDirCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.dir = os.path.join(self._tmp.name, "magic-mouse")
        self.file = os.path.join(self.dir, "battery.json")
        self.logged = []
        for name, value in (("STATUS_DIR", self.dir), ("STATUS_FILE", self.file)):
            p = mock.patch.object(daemon, name, value)
            p.start()
            self.addCleanup(p.stop)
        p = mock.patch.object(daemon, "log", self.logged.append)
        p.start()
        self.addCleanup(p.stop)

    def mode(self, path):
        return stat.S_IMODE(os.stat(path).st_mode)

    def test_missing_directory_is_created_owner_only(self):
        old = os.umask(0)  # even a permissive umask must not widen it
        self.addCleanup(os.umask, old)
        daemon.write_status({"connected": True, "percent": 50})
        self.assertEqual(self.mode(self.dir), 0o700)
        self.assertEqual(self.mode(self.file), 0o600)
        with open(self.file) as f:
            self.assertEqual(json.load(f)["percent"], 50)
        self.assertEqual(self.logged, [])

    def test_owner_only_and_group_readable_directories_are_accepted(self):
        for mode in (0o700, 0o750, 0o755, 0o500):
            with self.subTest(mode=oct(mode)):
                os.makedirs(self.dir, exist_ok=True)
                os.chmod(self.dir, mode)
                os.close(daemon.open_private_dir(self.dir))
                os.chmod(self.dir, 0o700)

    def test_group_or_other_writable_directory_is_refused(self):
        os.mkdir(self.dir, 0o700)
        for mode in (0o770, 0o707, 0o775, 0o757, 0o777, 0o1777):
            with self.subTest(mode=oct(mode)):
                os.chmod(self.dir, mode)
                with self.assertRaises(OSError) as cm:
                    daemon.open_private_dir(self.dir)
                self.assertIn("writable by group or others", str(cm.exception))
                self.logged.clear()
                daemon.write_status({"connected": True})
                self.assertEqual(os.listdir(self.dir), [], "nothing may be written there")
                self.assertEqual(len(self.logged), 1)
                self.assertIn("not publishing battery status", self.logged[0])
                self.assertIn(self.dir, self.logged[0])
        self.assertEqual(self.mode(self.dir), 0o1777, "an unsafe directory is refused, not chmod-ed")

    def test_directory_owned_by_someone_else_is_refused(self):
        os.mkdir(self.dir, 0o700)
        with mock.patch("os.getuid", return_value=os.getuid() + 1):
            with self.assertRaises(OSError) as cm:
                daemon.open_private_dir(self.dir)
            self.assertIn("not owned", str(cm.exception))
            daemon.write_status({"connected": True})
        self.assertEqual(os.listdir(self.dir), [])
        self.assertIn("not publishing battery status", self.logged[-1])

    def test_symlinked_directory_is_refused(self):
        real = os.path.join(self._tmp.name, "elsewhere")
        os.mkdir(real, 0o700)
        os.symlink(real, self.dir)
        daemon.write_status({"connected": True})
        self.assertEqual(os.listdir(real), [])
        self.assertIn("not publishing battery status", self.logged[-1])

    def test_a_fifo_where_the_file_belongs_is_refused_without_blocking(self):
        os.mkdir(self.dir, 0o700)
        os.mkfifo(self.file, 0o600)
        daemon.write_status({"connected": True})
        self.assertTrue(stat.S_ISFIFO(os.lstat(self.file).st_mode))
        self.assertIn("not a regular file", self.logged[-1])

    def test_daemon_output_is_accepted_by_the_widget_reader(self):
        daemon.write_status({"connected": True, "model": "Magic Mouse 2", "percent": 64, "charging": True})
        out = subprocess.run([sys.executable, "-I", READER, self.file], capture_output=True, text=True, timeout=5)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(json.loads(out.stdout)["percent"], 64)

    def test_other_files_keep_the_plain_owner_check(self):
        # config.toml's directory is the user's own ~/.config/magic-mouse: group-writable is their call.
        cfgdir = os.path.join(self._tmp.name, "cfg")
        os.mkdir(cfgdir)
        os.chmod(cfgdir, 0o775)
        target = os.path.join(cfgdir, "config.toml")
        daemon.write_own_file(target, b"x = 1\n")
        with open(target, "rb") as f:
            self.assertEqual(f.read(), b"x = 1\n")


if __name__ == "__main__":
    unittest.main()
