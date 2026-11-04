#!/bin/bash
# Builds a universal (arm64 + x86_64) "OpenRDP Client.app" into client/build.
# Needs only the Command Line Tools (swiftc, clang, lipo); Xcode is not required.
#
#   client/build.sh            build
#   client/build.sh --zip      also pack build/OpenRDP-Client-<version>.zip to copy to another Mac
#   client/build.sh --dmg      also make build/OpenRDP-Client-<version>.dmg, laid out like the
#                              server's by scripts/make-dmg.sh
#
# When client/build-freerdp.sh has been run, its sdl-freerdp and libraries go
# inside the app (Contents/MacOS and Contents/Frameworks) and the app uses them;
# otherwise the app needs FreeRDP from Homebrew.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
out="$here/build"
app="$out/OpenRDP Client.app"
version="${VERSION:-0.1.0}"
min_macos=13.0
# The macOS 27 SDK turns @State into a macro whose plugin ships only with Xcode;
# the 26 SDK, also part of the Command Line Tools, still has the property wrapper.
sdk="${SDKROOT:-}"
for candidate in /Library/Developer/CommandLineTools/SDKs/MacOSX26*.sdk; do
    if [[ -z "$sdk" && -d "$candidate" ]]; then sdk="$candidate"; fi
done
sdk="${sdk:-$(xcrun --show-sdk-path)}"

rm -rf "$app" "$out/obj"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$out/obj"

sources=("$here"/Sources/*.swift)
for arch in arm64 x86_64; do
    echo "swiftc $arch"
    xcrun swiftc -O -parse-as-library -swift-version 5 -sdk "$sdk" \
        -target "$arch-apple-macos$min_macos" \
        -module-name OpenRDPClient \
        -o "$out/obj/client-$arch" "${sources[@]}"
done
lipo -create "$out/obj/client-arm64" "$out/obj/client-x86_64" -output "$app/Contents/MacOS/OpenRDP Client"

echo "clang libopenrdp-keyboard.dylib"
xcrun clang -arch arm64 -arch x86_64 -mmacosx-version-min=$min_macos -O2 -Wall -Werror \
    -fobjc-arc -dynamiclib -framework AppKit -framework CoreGraphics \
    -undefined dynamic_lookup -install_name @rpath/libopenrdp-keyboard.dylib \
    -o "$app/Contents/Resources/libopenrdp-keyboard.dylib" "$here/Keyboard/openrdp_keyboard.m"

freerdp="$out/freerdp"
if [[ -x "$freerdp/bin/sdl-freerdp" ]]; then
    echo "bundling FreeRDP from $freerdp"
    mkdir -p "$app/Contents/Frameworks"
    cp "$freerdp/bin/sdl-freerdp" "$app/Contents/MacOS/sdl-freerdp"
    cp "$freerdp"/lib/*.dylib "$app/Contents/Frameworks/"
else
    echo "no bundled FreeRDP (run client/build-freerdp.sh); the app will use Homebrew's"
fi

sed "s/@VERSION@/$version/g" "$here/Resources/Info.plist" > "$app/Contents/Info.plist"
cp "$root/assets/icon/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
if [[ "${1:-}" == "--dmg" ]]; then
    # The disk-image window's picture, inside the app so the signature covers it.
    swift "$root/scripts/dmg-background.swift" \
        "$app/Contents/Resources/background.png" "OpenRDP Client" > /dev/null
fi

# Ad-hoc signature: enough to run on this Mac and, after removing quarantine, on another.
# Inner code first, the app last.
for f in "$app/Contents/Resources/libopenrdp-keyboard.dylib" "$app"/Contents/Frameworks/*.dylib "$app/Contents/MacOS/sdl-freerdp"; do
    if [[ -e "$f" ]]; then codesign --force --sign - "$f"; fi
done
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
rm -rf "$out/obj"

lipo -info "$app/Contents/MacOS/OpenRDP Client"
lipo -info "$app/Contents/Resources/libopenrdp-keyboard.dylib"
if [[ -e "$app/Contents/MacOS/sdl-freerdp" ]]; then lipo -info "$app/Contents/MacOS/sdl-freerdp"; fi
echo "built $app"

if [[ "${1:-}" == "--dmg" ]]; then
    "$root/scripts/make-dmg.sh" --app "$app" "$out/OpenRDP-Client-$version.dmg"
fi

if [[ "${1:-}" == "--zip" ]]; then
    zip="$out/OpenRDP-Client-$version.zip"
    rm -f "$zip"
    (cd "$out" && ditto -c -k --keepParent "OpenRDP Client.app" "$zip")
    echo "packed $zip"
fi
