#!/bin/bash
# Checks a built "OpenRDP Client.app": every Mach-O file inside is universal
# (arm64 + x86_64), and everything it loads is either inside the app or part of
# macOS. With --bundled, the app must also carry its own sdl-freerdp.
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

count=0
while IFS= read -r -d '' f; do
    file -b "$f" | grep -q '^Mach-O' || continue
    count=$((count + 1))
    name=${f#"$app/"}

    archs=$(lipo -archs "$f")
    for arch in arm64 x86_64; do
        [[ " $archs " == *" $arch "* ]] || fail "$name lacks $arch (has: $archs)"
    done

    # Dependency lines start with a tab; a dylib also lists its own id there.
    id=$(otool -D "$f" | tail -n +2 | head -1)
    while read -r dep; do
        [[ "$dep" == "$id" ]] && continue
        case "$dep" in
            /usr/lib/* | /System/*) ;;
            @rpath/*)
                [[ -e "$app/Contents/Frameworks/${dep#@rpath/}" ]] || fail "$name needs $dep, missing from Contents/Frameworks"
                ;;
            *) fail "$name links outside the app: $dep" ;;
        esac
    done < <(otool -L "$f" | grep $'^\t' | awk '{print $1}' | sort -u)
done < <(find "$app/Contents" -type f -print0)

codesign --verify --deep --strict "$app" || fail "signature does not verify"

((count > 0)) || fail "no Mach-O files found"
((failed == 0)) || exit 1
echo "ok: $count Mach-O files, all universal, nothing linked outside the app or macOS"
