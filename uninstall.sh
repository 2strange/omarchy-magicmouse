#!/usr/bin/env bash
set -euo pipefail
systemctl --user disable --now magic-mouse.service 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/magic-mouse.service" "$HOME/.local/bin/magic-mouse-daemon" "$HOME/.local/bin/magic-mouse-config" "$HOME/.local/bin/magic-mouse-battery-query" \
  "$HOME/.local/bin/magic-mouse-config.py" "$HOME/.local/bin/magic-mouse-battery-query.py"
systemctl --user daemon-reload
omarchy plugin disable io.github.2strange.magicmouse 2>/dev/null || true
# The plugin checkout itself is left for `omarchy plugin remove io.github.2strange.magicmouse`.
sudo rm -f /etc/udev/rules.d/70-magic-mouse.rules /etc/modprobe.d/hid_magicmouse.conf /etc/modules-load.d/magic-mouse.conf && sudo udevadm control --reload
echo "removed daemon, unit, udev rule, driver options and the bar widget."
echo "kept: ~/.config/magic-mouse/config.toml, the 'omarchy-magic-mouse' block in ~/.config/hypr/input.lua, and the plugin checkout (omarchy plugin remove io.github.2strange.magicmouse)."
