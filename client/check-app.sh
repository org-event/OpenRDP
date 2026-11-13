#!/bin/bash
# Checks a built "OpenRDP Client.app": every Mach-O file inside is universal
# (arm64 + x86_64), runs on the macOS in the app's LSMinimumSystemVersion, and
# everything it loads is either inside the app or part of macOS. With --bundled, the app must also carry its own sdl-freerdp.
#
#   client/check-app.sh [--bundled] [path/to/OpenRDP Client.app]
set -euo pipefail

bundled=false
if [[ "${1:-}" == "--bundled" ]]; then
    bundled=true
    shift
fi
app="${1:-$(cd "$(dirname "$0")" && pwd)/build/OpenRDP Client.app}"
[[ -d "$app" ]] || { echo "no app at $app" >&2; exit 1; }

failed=0
fail() {
    echo "FAIL: $*" >&2
    failed=1
}

if $bundled; then
    [[ -x "$app/Contents/MacOS/sdl-freerdp" ]] || fail "no bundled sdl-freerdp"
fi

min_macos=$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$app/Contents/Info.plist")

# minos <file> <arch>: the oldest macOS that slice says it runs on.
minos() {
    otool -arch "$2" -l "$1" | awk '
        /cmd LC_BUILD_VERSION/ { build = 1 }
        /cmd LC_VERSION_MIN_MACOSX/ { old = 1 }
        build && $1 == "minos" { print $2; exit }
        old && $1 == "version" { print $2; exit }'
}

count=0
swift_rpath_needed=0
while IFS= read -r -d '' f; do
    file -b "$f" | grep -q '^Mach-O' || continue
    count=$((count + 1))
    name=${f#"$app/"}

    archs=$(lipo -archs "$f")
    for arch in arm64 x86_64; do
        if [[ " $archs " != *" $arch "* ]]; then
            fail "$name lacks $arch (has: $archs)"
            continue
        fi
        needs=$(minos "$f" "$arch")
        if [[ -n "$needs" && "$(printf '%s\n%s\n' "$needs" "$min_macos" | sort -V | tail -1)" != "$min_macos" ]]; then
            fail "$name ($arch) needs macOS $needs, the app says $min_macos"
        fi
    done

    # Dependency lines start with a tab; a dylib also lists its own id there.
    id=$(otool -D "$f" | tail -n +2 | head -1)
    while read -r dep; do
        [[ "$dep" == "$id" ]] && continue
        case "$dep" in
            /usr/lib/* | /System/*) ;;
            @rpath/libswift*.dylib)
                # The Swift runtime: in /usr/lib/swift, which the app's rpath
                # lists first, or (concurrency before macOS 12) inside the app.
                [[ -e "$app/Contents/Frameworks/${dep#@rpath/}" ]] || swift_rpath_needed=1
                ;;
            @rpath/*)
                [[ -e "$app/Contents/Frameworks/${dep#@rpath/}" ]] || fail "$name needs $dep, missing from Contents/Frameworks"
                ;;
            *) fail "$name links outside the app: $dep" ;;
        esac
    done < <(otool -L "$f" | grep $'^\t' | awk '{print $1}' | sort -u)
done < <(find "$app/Contents" -type f -print0)

if ((swift_rpath_needed)) && ! otool -l "$app/Contents/MacOS/OpenRDP Client" | grep -q 'path /usr/lib/swift '; then
    fail "the app loads Swift libraries through @rpath but has no /usr/lib/swift in its rpath"
fi

codesign --verify --deep --strict "$app" || fail "signature does not verify"

((count > 0)) || fail "no Mach-O files found"
((failed == 0)) || exit 1
echo "ok: $count Mach-O files, all universal and for macOS $min_macos, nothing linked outside the app or macOS"
