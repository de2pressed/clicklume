#!/usr/bin/env bash
# Install ClickLume for the current user. Root is used only for the optional
# udev/input-group setup; the application always runs unprivileged.
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
INSTALL_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/clicklume/bin"
BIN_DIR="$HOME/.local/bin"
DESKTOP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
SERVICE_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
NON_INTERACTIVE=0
SKIP_SYSTEM_SETUP=0

for arg in "$@"; do
    case "$arg" in
        --non-interactive) NON_INTERACTIVE=1 ;;
        --skip-system-setup) SKIP_SYSTEM_SETUP=1 ;;
        --help)
            echo "usage: ./install.sh [--non-interactive] [--skip-system-setup]"
            exit 0
            ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

if [ "$NON_INTERACTIVE" -eq 0 ] && [ ! -t 1 ]; then
    echo "ClickLume installer must run in a terminal (or use --non-interactive)." >&2
    exit 2
fi

step() { printf '\n==> %s\n' "$1"; }

run_privileged() {
    local description="$1"
    shift
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    elif sudo -n true 2>/dev/null; then
        sudo -n "$@"
    else
        printf 'Skipped %s (administrator access required).\n' "$description"
        printf 'Run manually: sudo'
        printf ' %q' "$@"
        printf '\n'
        return 1
    fi
}

SOURCE_BIN_DIR="$PROJECT_DIR/bin"
if [ ! -x "$SOURCE_BIN_DIR/clicklume-gui" ]; then
    step "Build optimized binaries"
    CARGO_BIN="$(command -v cargo 2>/dev/null || true)"
    if [ -z "$CARGO_BIN" ] && [ -x "$HOME/.cargo/bin/cargo" ]; then
        CARGO_BIN="$HOME/.cargo/bin/cargo"
    fi
    if [ -z "$CARGO_BIN" ]; then
        echo "cargo is required when installing from a source checkout." >&2
        echo "Use a prebuilt GitHub release archive instead." >&2
        exit 1
    fi
    (cd "$PROJECT_DIR" && "$CARGO_BIN" build --release --locked)
    SOURCE_BIN_DIR="$PROJECT_DIR/target/release"
fi

for binary in clicklume-gui clicklume-backend clicklume-cli; do
    if [ ! -x "$SOURCE_BIN_DIR/$binary" ]; then
        echo "missing executable: $SOURCE_BIN_DIR/$binary" >&2
        exit 1
    fi
done

step "Install application binaries"
mkdir -p "$INSTALL_DIR" "$BIN_DIR"
install -m 0755 "$SOURCE_BIN_DIR/clicklume-gui" "$INSTALL_DIR/clicklume-gui"
install -m 0755 "$SOURCE_BIN_DIR/clicklume-backend" "$INSTALL_DIR/clicklume-backend"
install -m 0755 "$SOURCE_BIN_DIR/clicklume-cli" "$INSTALL_DIR/clicklume-cli"
ln -sfn "$INSTALL_DIR/clicklume-gui" "$BIN_DIR/clicklume"
ln -sfn "$INSTALL_DIR/clicklume-cli" "$BIN_DIR/clicklume-cli"
# Compatibility with pre-rename installations and troubleshooting commands.
ln -sfn "$INSTALL_DIR/clicklume-cli" "$BIN_DIR/autoclick-cli"

step "Install desktop launcher and optional user service"
mkdir -p "$DESKTOP_DIR" "$SERVICE_DIR"
sed "s|@GUI_BIN@|$INSTALL_DIR/clicklume-gui|g" \
    "$PROJECT_DIR/clicklume.desktop" > "$DESKTOP_DIR/clicklume.desktop"
chmod 0644 "$DESKTOP_DIR/clicklume.desktop"
sed "s|@GUI_BIN@|$INSTALL_DIR/clicklume-gui|g" \
    "$PROJECT_DIR/systemd/clicklume.service" > "$SERVICE_DIR/clicklume.service"
chmod 0644 "$SERVICE_DIR/clicklume.service"
systemctl --user daemon-reload
systemctl --user disable clicklume.service >/dev/null 2>&1 || true
# Prevent the pre-rename service from launching a second copy after upgrade.
systemctl --user disable --now autoclick-gui.service >/dev/null 2>&1 || true

if [ "$SKIP_SYSTEM_SETUP" -eq 0 ]; then
    step "Configure Wayland input access"
    run_privileged "uinput udev rule" install -m 0644 \
        "$PROJECT_DIR/udev/99-clicklume-uinput.rules" \
        /etc/udev/rules.d/99-clicklume-uinput.rules || true
    run_privileged "keyboard udev rule" install -m 0644 \
        "$PROJECT_DIR/udev/99-clicklume-keyboards.rules" \
        /etc/udev/rules.d/99-clicklume-keyboards.rules || true
    run_privileged "udev reload" udevadm control --reload-rules || true
    run_privileged "udev input refresh" udevadm trigger --subsystem-match=input || true
    if ! id -nG "$USER" | tr ' ' '\n' | grep -qx input; then
        run_privileged "input group membership" usermod -aG input "$USER" || true
    fi
fi

cat <<EOF

ClickLume is installed.

Launch it from the app menu or run:
  $BIN_DIR/clicklume

Autostart remains disabled. Enable "Start on login" inside ClickLume if wanted.
If input-group membership was added, log out and back in before using hotkeys.
EOF
