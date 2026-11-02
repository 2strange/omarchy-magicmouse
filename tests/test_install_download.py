"""install.sh, run for real: daemon_download installs only the digest pinned in daemon.lock, and
the default mode stops instead of building something else. curl, uname and the install step are
stubbed, so nothing is downloaded or installed and no network is needed."""
import hashlib
import os
import pathlib
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
INSTALL = (ROOT / "install.sh").read_text()
ELF = b"\x7fELF" + b"\0" * 60


def function(name):
    m = re.search(rf"^{name}\(\) \{{.*?^\}}", INSTALL, re.S | re.M)
    assert m, name
    return m.group(0)


def mode_dispatch():
    m = re.search(r'^case "\$MODE" in.*?^esac', INSTALL, re.S | re.M)
    assert m
    return m.group(0)


STUBS = r"""
set -euo pipefail
say() { echo "say: $*"; }
die() { echo "DIE: $*"; exit 9; }
uname() { echo x86_64; }
put_daemon() { echo "INSTALLED $(sha256sum "$1" | cut -c1-64)"; }
REPO=owner/repo
tmp="$(mktemp -d)"
"""

FAKE_CURL = r"""#!/bin/sh
# fake curl: copy $FAKE_ASSET to the -o target, or fail like a 404 when FAKE_FAIL is set
echo "$*" >> "$FAKE_LOG"
out=""
while [ $# -gt 0 ]; do [ "$1" = -o ] && { out="$2"; shift; }; shift; done
[ -z "${FAKE_FAIL:-}" ] || exit 22
cp "$FAKE_ASSET" "$out"
"""


class Download(unittest.TestCase):
    def setUp(self):
        self.dir = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.dir)
        (self.dir / "bin").mkdir()
        curl = self.dir / "bin" / "curl"
        curl.write_text(FAKE_CURL)
        curl.chmod(0o755)
        self.asset = self.dir / "asset"
        self.asset.write_bytes(ELF)
        self.digest = hashlib.sha256(ELF).hexdigest()

    def lock(self, text):
        (self.dir / "daemon.lock").write_text(text)

    def run_download(self, **env):
        script = STUBS + f'trap \'rm -rf "$tmp"\' EXIT\nhere="{self.dir}"\n' + function("daemon_download") + "\nrc=0; daemon_download || rc=$?; echo rc=$rc\n"
        self.log = self.dir / "curl.log"
        e = dict(os.environ, PATH=f"{self.dir / 'bin'}:{os.environ['PATH']}", FAKE_ASSET=str(self.asset),
                 FAKE_LOG=str(self.log), **env)
        return subprocess.run(["bash", "-c", script], capture_output=True, text=True, env=e).stdout

    def test_pinned_digest_installs(self):
        self.lock(f"# comment\nv2.0.0 asset-name {self.digest}\n")
        out = self.run_download()
        self.assertIn(f"INSTALLED {self.digest}", out)
        self.assertIn("rc=0", out)
        calls = self.log.read_text().splitlines()
        self.assertEqual(len(calls), 1)  # the asset only: no .sha256 from the release
        self.assertIn("--proto =https", calls[0])
        self.assertIn("https://github.com/owner/repo/releases/download/v2.0.0/asset-name", calls[0])

    def test_other_bytes_stop_before_install(self):
        self.lock(f"v2.0.0 asset-name {'0' * 64}\n")
        out = self.run_download()
        self.assertIn("DIE: checksum mismatch", out)
        self.assertNotIn("INSTALLED", out)

    def test_matching_digest_but_not_elf_stops(self):
        self.asset.write_bytes(b"#!/bin/sh\n")
        self.lock(f"v2.0.0 asset-name {hashlib.sha256(b'#!/bin/sh' + bytes([10])).hexdigest()}\n")
        out = self.run_download()
        self.assertIn("DIE:", out)
        self.assertNotIn("INSTALLED", out)

    def test_bad_locks_stop(self):
        for text in ("", "# only a comment\n", f"v2.0.0 a {self.digest}\nv2.0.1 a {self.digest}\n",
                     "v2.0.0 a;rm notahash\n", f"v2.0.0 a {self.digest} extra\n"):
            with self.subTest(text=text):
                self.lock(text)
                out = self.run_download()
                self.assertIn("DIE:", out)
                self.assertNotIn("INSTALLED", out)

    def test_failed_transfer_installs_nothing(self):
        self.lock(f"v2.0.0 asset-name {self.digest}\n")
        out = self.run_download(FAKE_FAIL="1")
        self.assertIn("rc=1", out)
        self.assertNotIn("INSTALLED", out)


class DefaultMode(unittest.TestCase):
    def test_no_quiet_fallback_when_the_download_fails(self):
        script = STUBS + """
trap 'rm -rf "$tmp"' EXIT
daemon_download() { return 1; }
daemon_build() { echo BUILT; }
daemon_python() { echo PYTHON; }
MODE=auto
""" + mode_dispatch() + "\necho done\n"
        out = subprocess.run(["bash", "-c", script], capture_output=True, text=True).stdout
        self.assertIn("DIE:", out)
        self.assertNotIn("BUILT", out)
        self.assertNotIn("PYTHON", out)
        self.assertNotIn("done", out)


if __name__ == "__main__":
    unittest.main()
