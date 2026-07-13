#!/usr/bin/env bash
set -uo pipefail

INSTALL_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/clicklume"
BIN_DIR="$HOME/.local/bin"
DESKTOP_FILE="${XDG_DATA_HOME:-$HOME/.local/share}/applications/clicklume.desktop"
SERVICE_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/clicklume.service"

systemctl --user disable --now clicklume.service >/dev/null 2>&1 || true
rm -f "$SERVICE_FILE" "$DESKTOP_FILE"
rm -f "$BIN_DIR/clicklume" "$BIN_DIR/clicklume-cli"
if [ -L "$BIN_DIR/autoclick-cli" ]; then
    rm -f "$BIN_DIR/autoclick-cli"
fi
rm -rf "$INSTALL_DIR"
systemctl --user daemon-reload

cat <<'EOF'
ClickLume user files were removed.

Preferences remain in ~/.config/clicklume so reinstalling keeps your settings.
To remove system permission rules too, run:
  sudo rm -f /etc/udev/rules.d/99-clicklume-uinput.rules
  sudo rm -f /etc/udev/rules.d/99-clicklume-keyboards.rules
  sudo udevadm control --reload-rules

Removing yourself from the input group can affect other software, so it is not
done automatically.
EOF
