#!/bin/bash
# Build a drag-and-drop disk image: OpenRDP.app on the left, Applications on the right.
# Usage: scripts/make-dmg.sh <openrdp-binary> <version> <output.dmg>

set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <openrdp-binary> <version> <output.dmg>" >&2
  exit 2
fi

binary=$1
version=$2
output=$3
root=$(cd "$(dirname "$0")/.." && pwd)
stage=$(mktemp -d)
rw="$stage/rw.dmg"
vol=OpenRDP
mount="/Volumes/$vol"
attached=0

cleanup() {
  if [[ $attached -eq 1 ]]; then
    hdiutil detach "$mount" -quiet || hdiutil detach "$mount" -force -quiet || true
  fi
  rm -rf "$stage"
}
trap cleanup EXIT

if [[ -d "$mount" ]]; then
  echo "$mount is already in use" >&2
  exit 1
fi

app="$stage/OpenRDP.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/openrdp"
chmod 755 "$app/Contents/MacOS/openrdp"
cp "$root/assets/icon/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>com.openrdp.server</string>
    <key>CFBundleName</key><string>OpenRDP</string>
    <key>CFBundleExecutable</key><string>openrdp</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${version}</string>
    <key>CFBundleVersion</key><string>${version}</string>
    <key>LSUIElement</key><true/>
</dict>
</plist>
EOF
codesign --force --sign - --identifier com.openrdp.server "$app"

mibs=$(du -sm "$app" | awk '{print $1}')
hdiutil create -size "$((mibs + 32))m" -fs HFS+ -volname "$vol" "$rw" >/dev/null
hdiutil attach "$rw" >/dev/null
attached=1
cp -R "$app" "$mount/OpenRDP.app"
ln -s /Applications "$mount/Applications"
mkdir -p "$mount/.background"
python3 - "$mount/.background/background.png" <<'PY'
import struct
import sys
import zlib

path, width, height = sys.argv[1], 640, 380

def pixel(x, y):
    # Grey arrow between the two icons.
    shaft = 250 <= x <= 390 and 178 <= y <= 198
    head = 390 <= x <= 430 and abs(y - 188) <= (430 - x) * 0.7
    if shaft or head:
        return bytes((142, 142, 147))
    return bytes((245, 245, 247))

rows = bytearray()
for y in range(height):
    rows.append(0)
    for x in range(width):
        rows.extend(pixel(x, y))

def chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

raw = zlib.compress(bytes(rows), 9)
png = b"\x89PNG\r\n\x1a\n"
png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
png += chunk(b"IDAT", raw)
png += chunk(b"IEND", b"")
with open(path, "wb") as out:
    out.write(png)
PY
chflags hidden "$mount/.background"

# Finder draws the window. A runner without a desktop still ships a usable image.
if osascript <<EOF
tell application "Finder"
  tell disk "$vol"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {240, 120, 880, 500}
    set viewOptions to the icon view options of container window
    set arrangement of viewOptions to not arranged
    set icon size of viewOptions to 128
    set background picture of viewOptions to file ".background:background.png"
    set position of item "OpenRDP.app" of container window to {140, 180}
    set position of item "Applications" of container window to {460, 180}
    close
    open
    update without registering applications
    delay 2
    close
  end tell
end tell
EOF
then
  echo "arranged the disk-image window"
else
  echo "warning: Finder did not arrange the window; the image still contains the app and Applications" >&2
fi

sync
hdiutil detach "$mount" -quiet
attached=0
mkdir -p "$(dirname "$output")"
rm -f "$output"
hdiutil convert "$rw" -format UDZO -imagekey zlib-level=9 -o "$output" >/dev/null
echo "wrote $output"
