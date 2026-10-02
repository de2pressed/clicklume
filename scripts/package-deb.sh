#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_BIN="${CARGO:-$(command -v cargo 2>/dev/null || true)}"
if [ -z "$CARGO_BIN" ] && [ -x "$HOME/.cargo/bin/cargo" ]; then
    CARGO_BIN="$HOME/.cargo/bin/cargo"
fi
if [ -z "$CARGO_BIN" ]; then
    echo "cargo not found" >&2
    exit 1
fi
VERSION="$(awk -F'"' '/^version = / { print $2; exit }' "$ROOT/Cargo.toml")"
ARCH="$(dpkg --print-architecture)"
OUT="$ROOT/dist"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

cd "$ROOT"
"$CARGO_BIN" build --release --locked
mkdir -p "$OUT"
mkdir -p \
    "$STAGE/DEBIAN" \
    "$STAGE/usr/lib/clicklume" \
    "$STAGE/usr/bin" \
    "$STAGE/usr/share/applications" \
    "$STAGE/usr/lib/systemd/user" \
    "$STAGE/usr/lib/udev/rules.d" \
    "$STAGE/usr/share/doc/clicklume"

install -m 0755 target/release/clicklume-gui "$STAGE/usr/lib/clicklume/"
install -m 0755 target/release/clicklume-backend "$STAGE/usr/lib/clicklume/"
install -m 0755 target/release/clicklume-cli "$STAGE/usr/lib/clicklume/"
ln -s ../lib/clicklume/clicklume-gui "$STAGE/usr/bin/clicklume"
ln -s ../lib/clicklume/clicklume-cli "$STAGE/usr/bin/clicklume-cli"
sed 's|@GUI_BIN@|/usr/lib/clicklume/clicklume-gui|g' clicklume.desktop \
    > "$STAGE/usr/share/applications/clicklume.desktop"
sed 's|@GUI_BIN@|/usr/lib/clicklume/clicklume-gui|g' systemd/clicklume.service \
    > "$STAGE/usr/lib/systemd/user/clicklume.service"
install -m 0644 udev/*.rules "$STAGE/usr/lib/udev/rules.d/"
install -m 0644 README.md LICENSE "$STAGE/usr/share/doc/clicklume/"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: clicklume
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Maintainer: de2pressed <noreply@github.com>
Depends: libc6, libgcc-s1
Description: Native autoclicker for Ubuntu GNOME Wayland
 ClickLume uses Linux uinput for mouse clicks and passive evdev reads for
 configurable global hotkeys. Runtime is unprivileged after one-time setup.
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
udevadm control --reload-rules 2>/dev/null || true
udevadm trigger --subsystem-match=input 2>/dev/null || true
udevadm trigger --subsystem-match=misc --sysname-match=uinput 2>/dev/null || true
echo "ClickLume installed. Add your desktop user to the input group if needed:"
echo "  sudo usermod -aG input <username>"
echo "Then log out and back in. ClickLume never runs as root."
EOF
chmod 0755 "$STAGE/DEBIAN/postinst"

cat > "$STAGE/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
udevadm control --reload-rules 2>/dev/null || true
EOF
chmod 0755 "$STAGE/DEBIAN/postrm"

find "$STAGE" -type d -exec chmod 0755 {} +
dpkg-deb --root-owner-group --build "$STAGE" "$OUT/clicklume_${VERSION}_${ARCH}.deb"
