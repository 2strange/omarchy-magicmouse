"""Checks for status-reader.py, the bounded reader the bar widget runs instead
of opening $XDG_RUNTIME_DIR/magic-mouse/battery.json from inside the shell.

Run from the repo root:  /usr/bin/python3 -m unittest discover -s tests -v
"""
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
READER = os.path.join(ROOT, "status-reader.py")

spec = importlib.util.spec_from_file_location("status_reader", READER)
status_reader = importlib.util.module_from_spec(spec)
spec.loader.exec_module(status_reader)

GOOD = {"connected": True, "model": "Magic Mouse 2", "percent": 87, "charging": False, "ts": 1.5}


class ReaderCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.dir = os.path.join(self._tmp.name, "magic-mouse")
        os.mkdir(self.dir, 0o700)
        os.chmod(self.dir, 0o700)
        self.path = os.path.join(self.dir, "battery.json")

    def put(self, data, mode=0o600):
        raw = data if isinstance(data, bytes) else json.dumps(data).encode()
        fd = os.open(self.path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, mode)
        os.write(fd, raw)
        os.close(fd)
        os.chmod(self.path, mode)

    def run_reader(self, path=None):
        """Run the real script as the widget does. A hang fails the test."""
        try:
            p = subprocess.run([sys.executable, "-I", READER, path or self.path],
                               capture_output=True, text=True, timeout=5)
        except subprocess.TimeoutExpired:
            self.fail("status-reader blocked")
        return p

    def assertRejected(self, path=None):
        p = self.run_reader(path)
        self.assertEqual((p.returncode, p.stdout), (1, ""), p.stderr)

    # ---- accepted
    def test_regular_file_is_passed_through(self):
        self.put(GOOD)
        p = self.run_reader()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(json.loads(p.stdout), GOOD)

    def test_directory_may_be_group_readable(self):
        os.chmod(self.dir, 0o755)
        self.put(GOOD)
        self.assertEqual(self.run_reader().returncode, 0)

    def test_file_of_exactly_the_ceiling_is_accepted(self):
        pad = status_reader.MAX_BYTES - len(json.dumps({"p": ""}))
        self.put({"p": "x" * pad})
        self.assertEqual(os.path.getsize(self.path), status_reader.MAX_BYTES)
        self.assertEqual(self.run_reader().returncode, 0)

    # ---- special files and size
    def test_fifo_does_not_block(self):
        os.mkfifo(self.path, 0o600)
        self.assertRejected()

    def test_fifo_with_a_writer_does_not_block_or_leak(self):
        os.mkfifo(self.path, 0o600)
        w = os.open(self.path, os.O_RDWR)  # keeps the pipe alive; a blocking read would hang
        self.addCleanup(os.close, w)
        os.write(w, b'{"percent": 1}')
        self.assertRejected()

    def test_oversized_file_is_rejected(self):
        self.put({"pad": "x" * (status_reader.MAX_BYTES + 1)})
        self.assertRejected()

    def test_huge_file_is_rejected_without_reading_it(self):
        fd = os.open(self.path, os.O_WRONLY | os.O_CREAT, 0o600)
        os.ftruncate(fd, 64 << 30)  # sparse: 64 GiB of zeros costs nothing to create
        os.close(fd)
        self.assertRejected()

    def test_read_is_bounded_even_when_fstat_reports_no_size(self):
        # /proc files are regular, ours and report st_size 0: only the bounded read can stop them.
        with mock.patch.dict(os.environ, {"PAD": "x" * 200}):
            with self.assertRaises(ValueError) as cm:
                status_reader.read_status(f"/proc/{os.getpid()}/environ", max_bytes=64)
        self.assertIn("larger than", str(cm.exception))

    def test_symlink_to_a_file_is_rejected(self):
        real = os.path.join(self._tmp.name, "real.json")
        with open(real, "w") as f:
            json.dump(GOOD, f)
        os.symlink(real, self.path)
        self.assertRejected()

    def test_directory_in_place_of_the_file_is_rejected(self):
        os.mkdir(self.path)
        self.assertRejected()

    def test_missing_file_is_quiet(self):
        p = self.run_reader()
        self.assertEqual((p.returncode, p.stdout, p.stderr), (1, "", ""))

    # ---- ownership and permissions
    def test_group_or_other_writable_directory_is_rejected(self):
        self.put(GOOD)
        for mode in (0o770, 0o707, 0o775, 0o757, 0o777, 0o1777):
            with self.subTest(mode=oct(mode)):
                os.chmod(self.dir, mode)
                self.assertRejected()

    def test_group_or_other_writable_file_is_rejected(self):
        for mode in (0o660, 0o606, 0o666):
            with self.subTest(mode=oct(mode)):
                self.put(GOOD, mode)
                self.assertRejected()

    def test_directory_not_owned_by_this_user_is_rejected(self):
        self.put(GOOD)
        with mock.patch("os.getuid", return_value=os.getuid() + 1):
            with self.assertRaises(ValueError) as cm:
                status_reader.read_status(self.path)
        self.assertIn("not owned", str(cm.exception))

    def test_symlinked_directory_is_rejected(self):
        self.put(GOOD)
        link = os.path.join(self._tmp.name, "link")
        os.symlink(self.dir, link)
        self.assertRejected(os.path.join(link, "battery.json"))

    # ---- content
    def test_bad_content_is_rejected(self):
        for label, raw in (("not json", b"hello"), ("array", b"[1,2]"), ("scalar", b"7"),
                           ("null", b"null"), ("empty", b""), ("nan", b'{"percent": NaN}'),
                           ("bad utf-8", b'{"model": "\xff"}'), ("deep", b"[" * 3000 + b"]" * 3000)):
            with self.subTest(label):
                self.put(raw)
                self.assertRejected()

    def test_bad_usage(self):
        p = subprocess.run([sys.executable, "-I", READER], capture_output=True, text=True, timeout=5)
        self.assertEqual(p.returncode, 2)

    def test_verbose_says_why(self):
        self.put(GOOD, 0o666)
        p = subprocess.run([sys.executable, "-I", READER, "-v", self.path], capture_output=True, text=True, timeout=5)
        self.assertEqual((p.returncode, p.stdout), (1, ""))
        self.assertIn("writable by group or others", p.stderr)


if __name__ == "__main__":
    unittest.main()
