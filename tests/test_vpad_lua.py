"""The Python daemon's Hyprland rule covers every mouse's virtual touchpad, like the Rust daemon's.
Needs python-evdev, like the daemon itself."""
import importlib.machinery
import importlib.util
import os
import unittest

DAEMON = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "bin", "magic-mouse-daemon")

try:
    loader = importlib.machinery.SourceFileLoader("magic_mouse_daemon_vpad", DAEMON)
    spec = importlib.util.spec_from_loader("magic_mouse_daemon_vpad", loader)
    daemon = importlib.util.module_from_spec(spec)
    loader.exec_module(daemon)
except ImportError:  # python-evdev missing
    daemon = None


@unittest.skipIf(daemon is None, "cannot import the daemon (python-evdev missing?)")
class VpadLua(unittest.TestCase):
    def test_every_touchpad_name_gets_the_same_rule(self):
        cfg = {"touchpad": {"scroll_speed": 3}, "scroll": {"natural": 1}}
        rules = [f'hl.device({{ name = "magic-mouse-virtual-touchpad{sfx}", scroll_factor = 1.5000, '
                 f'natural_scroll = true, tap_to_click = false }})'
                 for sfx in ["", "-1", "-2", "-3"]]
        self.assertEqual(daemon.vpad_lua(cfg), " ".join(rules))


if __name__ == "__main__":
    unittest.main()
