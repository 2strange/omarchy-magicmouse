"""daemon.lock pins exactly one prebuilt daemon, and install.sh trusts only that digest."""
import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent


class DaemonLock(unittest.TestCase):
    def test_one_well_formed_pin(self):
        pins = [line.split() for line in (ROOT / "daemon.lock").read_text().splitlines()
                if line.strip() and not line.lstrip().startswith("#")]
        self.assertEqual(len(pins), 1)
        tag, asset, digest = pins[0]
        self.assertRegex(tag, r"^v\d+\.\d+\.\d+")
        self.assertEqual(asset, "magic-mouse-daemon-x86_64-linux")
        self.assertRegex(digest, r"^[0-9a-f]{64}$")

    def test_installer_does_not_fetch_the_release_checksum(self):
        fn = re.search(r"^daemon_download\(\) \{.*?^\}", (ROOT / "install.sh").read_text(), re.S | re.M)
        self.assertIsNotNone(fn)
        self.assertIn("daemon.lock", fn.group(0))
        self.assertNotIn(".sha256", fn.group(0))


if __name__ == "__main__":
    unittest.main()
