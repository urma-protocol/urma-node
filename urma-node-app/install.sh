#!/usr/bin/env bash
set -euo pipefail

prefix=${PREFIX:-$HOME/.local}
app=$(cd "$(dirname "$0")" && pwd)
export CARGO_TARGET_DIR=$(realpath -m "${CARGO_TARGET_DIR:-$app/../target}")

cd "$app"
pnpm install --frozen-lockfile
pnpm tauri build --no-bundle

install -Dm755 "$CARGO_TARGET_DIR"/release/urma-node-app "$prefix/bin/urma-node-app"
install -Dm644 src-tauri/icons/128x128.png "$prefix/share/icons/hicolor/128x128/apps/urma-node.png"
install -Dm644 src/assets/urma-node.svg "$prefix/share/icons/hicolor/scalable/apps/urma-node.svg"
install -Dm644 /dev/stdin "$prefix/share/applications/urma-node.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=URMA Node
Comment=Local URMA Node status
Exec=$prefix/bin/urma-node-app
Icon=urma-node
Categories=Network;
Terminal=false
EOF

echo "installed $prefix/bin/urma-node-app"
