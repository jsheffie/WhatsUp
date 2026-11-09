#!/bin/zsh
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

cargo build --release

app="$root/dist/WhatsUp.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/target/release/whatsup" "$app/Contents/MacOS/whatsup"
cp "$root/packaging/Info.plist" "$app/Contents/Info.plist"
chmod +x "$app/Contents/MacOS/whatsup"

if [[ -f "$root/packaging/AppIcon.icns" ]]; then
  cp "$root/packaging/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
fi

codesign --force --sign - "$app"
echo "$app"
