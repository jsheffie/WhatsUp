#!/usr/bin/env bash
# Build WhatsUp and install it for the current user:
#   ~/.local/bin/whatsup                          the binary
#   ~/.local/share/applications/whatsup.desktop   app launcher entry
# Pass --uninstall to remove both. The config in ~/.config/whatsup is kept.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
apps_dir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
binary="$bin_dir/whatsup"
desktop="$apps_dir/whatsup.desktop"

if [[ "${1:-}" == "--uninstall" ]]; then
  rm -f "$binary" "$desktop"
  update-desktop-database "$apps_dir" 2>/dev/null || true
  echo "removed $binary and $desktop"
  exit 0
fi

cd "$root"
cargo build --release

mkdir -p "$bin_dir" "$apps_dir"
install -m 755 "$root/target/release/whatsup" "$binary"

cat >"$desktop" <<EOF
[Desktop Entry]
Type=Application
Name=WhatsUp
Comment=See which local dev services are listening
Exec=$binary
Icon=utilities-system-monitor
Terminal=false
Categories=Development;
StartupNotify=false
EOF

update-desktop-database "$apps_dir" 2>/dev/null || true
echo "$binary"
echo "$desktop"
