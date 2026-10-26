#!/bin/sh
# FluxDeck installer: build, grant DDC/CI i2c access, install desktop entries.
set -eu

cd "$(dirname "$0")"

echo "==> Building (cargo build --release)..."
if ! command -v cargo >/dev/null 2>&1; then
    echo "ERROR: cargo not found. Install Rust via rustup (https://rustup.rs) and re-run." >&2
    exit 1
fi
cargo build --release
BIN="$HOME/.local/bin/fluxdeck"

echo "==> i2c group + udev rule for DDC/CI brightness access..."
if command -v sudo >/dev/null 2>&1; then
    getent group i2c >/dev/null 2>&1 || sudo groupadd i2c
    sudo usermod -aG i2c "$USER" || echo "WARN: could not add $USER to i2c group" >&2
    printf 'SUBSYSTEM=="i2c-dev", KERNEL=="i2c-[0-9]*", MODE="0660", GROUP="i2c"\n' \
        | sudo tee /etc/udev/rules.d/99-fluxdeck-ddc.rules >/dev/null
    sudo udevadm control --reload-rules 2>/dev/null || true
else
    echo "WARN: sudo not available; skipping i2c group + udev rule." >&2
    echo "      DDC/CI (external monitors) needs: sudo groupadd i2c; sudo usermod -aG i2c $USER." >&2
fi

echo "==> Installing desktop entries..."
mkdir -p ~/.local/share/applications ~/.local/bin ~/.config/autostart
cp target/release/fluxdeck "$BIN"
sed "s|^Exec=.*|Exec=\"$BIN\"|" fluxdeck.desktop \
    > ~/.local/share/applications/fluxdeck.desktop
sed "s|^Exec=.*|Exec=\"$BIN\"|" fluxdeck.desktop \
    > ~/.config/autostart/fluxdeck-autostart.desktop
if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate ~/.local/share/applications/fluxdeck.desktop || true
fi

cat <<EOF

Done. Post-install steps:
  1. Log out and back in (or run: newgrp i2c) so the i2c group applies —
     external monitor brightness via DDC/CI needs it.
  2. If using autostart, verify ~/.config/autostart/fluxdeck-autostart.desktop.
  3. Binary: $BIN
EOF
