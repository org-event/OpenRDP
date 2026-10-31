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
printf '%s\n' 'xattr -dr com.apple.quarantine /Applications/OpenRDP.app' \
  > "$mount/command.txt"
# The picture lives inside the app, so the window has no extra folder to draw.
# Its stdout is the same 8-point grid the picture was drawn on.
swift "$root/scripts/dmg-background.swift" \
  "$mount/OpenRDP.app/Contents/Resources/background.png" > "$stage/layout"
# shellcheck disable=SC1091
source "$stage/layout"
# The title bar is outside the icon view. The picture matches the view.
titlebar=28
win_right=$((200 + width))
win_bottom=$((80 + height + titlebar))

# Finder draws the window. A runner without a desktop still ships a usable image.
# Icon positions are the top-left of the 128-point icon, so both sit on icon_top
# and the arrow (drawn at the icon center) crosses that row.
if osascript <<EOF
tell application "Finder"
  tell disk "$vol"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {200, 80, ${win_right}, ${win_bottom}}
    set viewOptions to the icon view options of container window
    set arrangement of viewOptions to not arranged
    set icon size of viewOptions to 128
    set text size of viewOptions to 13
    set background picture of viewOptions to file "OpenRDP.app:Contents:Resources:background.png"
    try
      set visible of item ".fseventsd" of container window to false
    end try
    set position of item "OpenRDP.app" of container window to {${icon_left}, ${icon_top}}
    set position of item "Applications" of container window to {${icon_right}, ${icon_top}}
    set position of item "command.txt" of container window to {${text_x}, ${text_y}}
    close
    open
    update without registering applications
    delay 2
    set the bounds of container window to {200, 80, ${win_right}, ${win_bottom}}
    try
      set visible of item ".fseventsd" of container window to false
    end try
    set position of item "OpenRDP.app" of container window to {${icon_left}, ${icon_top}}
    set position of item "Applications" of container window to {${icon_right}, ${icon_top}}
    set position of item "command.txt" of container window to {${text_x}, ${text_y}}
    close
  end tell
end tell
EOF
then
  echo "arranged the disk-image window"
else
  echo "warning: Finder did not arrange the window; the image still contains the app and Applications" >&2
fi

rm -rf "$mount/.fseventsd"
sync
hdiutil detach "$mount" -quiet
attached=0
mkdir -p "$(dirname "$output")"
rm -f "$output"
hdiutil convert "$rw" -format UDZO -imagekey zlib-level=9 -o "$output" >/dev/null
echo "wrote $output"
