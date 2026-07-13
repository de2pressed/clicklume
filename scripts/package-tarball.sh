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
MACHINE="$(uname -m)"
OUT="$ROOT/dist"
NAME="clicklume-${VERSION}-linux-${MACHINE}"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

cd "$ROOT"
"$CARGO_BIN" build --release --locked
mkdir -p "$OUT" "$STAGE/$NAME/bin" "$STAGE/$NAME/systemd" "$STAGE/$NAME/udev"
install -m 0755 target/release/clicklume-gui "$STAGE/$NAME/bin/"
install -m 0755 target/release/clicklume-backend "$STAGE/$NAME/bin/"
install -m 0755 target/release/clicklume-cli "$STAGE/$NAME/bin/"
install -m 0755 install.sh uninstall.sh "$STAGE/$NAME/"
install -m 0644 README.md LICENSE clicklume.desktop "$STAGE/$NAME/"
install -m 0644 systemd/clicklume.service "$STAGE/$NAME/systemd/"
install -m 0644 udev/*.rules "$STAGE/$NAME/udev/"
tar -C "$STAGE" -czf "$OUT/$NAME.tar.gz" "$NAME"
