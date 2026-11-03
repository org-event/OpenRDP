#!/bin/bash
# Builds FreeRDP's sdl-freerdp and the libraries it needs, for arm64 and x86_64,
# into client/build/freerdp (bin/sdl-freerdp and lib/*.dylib, all universal).
# client/build.sh then puts them inside the app.
#
#   - SDL3 is built with -DSDL_MAC_NO_SANDBOX, so its keyboard grab stops the
#     client Mac from taking Cmd+Space and the other system shortcuts;
#   - FFmpeg has only what FreeRDP uses (H.264 decoding, swscale);
#   - OpenSSL, jansson and FreeType are linked in statically.
#
# Needs the Command Line Tools and `brew install cmake ninja nasm pkgconf`.
# Each finished step leaves a stamp in build/deps/stamps, so a rerun picks up
# where the last one stopped; delete build/deps to start over.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
work="$here/build/deps"
src="$work/src"
out="$here/build/freerdp"
archs=(${ARCHS:-arm64 x86_64})
export MACOSX_DEPLOYMENT_TARGET=13.0
jobs=$(sysctl -n hw.ncpu)

# Versions and checksums as in Homebrew's formulae.
OPENSSL=(4.1.0 https://github.com/openssl/openssl/releases/download/openssl-4.1.0/openssl-4.1.0.tar.gz 50c17baf54cb008f6ce42d50deb4b28050a1aad711636cabd4b241c7e90cc2fd)
FFMPEG=(9.0.2 https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz 8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e)
JANSSON=(2.15.1 https://github.com/akheron/jansson/releases/download/v2.15.1/jansson-2.15.1.tar.gz 0c7114dc0b2d22a670724a1f95922029d7077c19dbf79a584cb8084d2f267f2f)
FREETYPE=(2.15.0 https://downloads.sourceforge.net/project/freetype/freetype2/2.15.0/freetype-2.15.0.tar.xz 93cd47eb719c325107f2d3cf087d06299e944c3b48a40b55537302304dea1dd5)
SDL3=(3.4.18 https://github.com/libsdl-org/SDL/releases/download/release-3.4.18/SDL3-3.4.18.tar.gz 9c75cf16330322c217dedd2e0609f1124f1b54b8633e763467b4684d0f4334a3)
SDL3_TTF=(3.2.2 https://github.com/libsdl-org/SDL_ttf/releases/download/release-3.2.2/SDL3_ttf-3.2.2.tar.gz 63547d58d0185c833213885b635a2c0548201cc8f301e6587c0be1a67e1e045d)
FREERDP=(3.32.2 https://github.com/FreeRDP/FreeRDP/archive/refs/tags/3.32.2.tar.gz 9ccb8357bf0188044a460de775c7ea289ca7d2a2a917eca0f0a7c4dc9c8b08ea)

for tool in cmake ninja nasm pkg-config; do
    command -v "$tool" >/dev/null || { echo "missing $tool: brew install cmake ninja nasm pkgconf" >&2; exit 1; }
done
mkdir -p "$src" "$work/stamps" "$work/logs"

# fetch <name> <version> <url> <sha256>: download once, check, unpack fresh into $work/<name>-<arch>.
fetch() {
    local name=$1 url=$3 sha=$4 file="$src/$(basename "$3")"
    [[ "$file" == */3.32.2.tar.gz ]] && file="$src/FreeRDP-3.32.2.tar.gz"
    if [[ ! -f "$file" ]]; then
        echo "  download $url"
        curl -fsSL -o "$file.part" "$url"
        mv "$file.part" "$file"
    fi
    echo "$sha  $file" | shasum -a 256 -c --quiet
}

unpack() { # unpack <archive> <dir>
    rm -rf "$2" && mkdir -p "$2"
    tar -xf "$1" -C "$2" --strip-components 1
}

# step <name> <arch> <function>: runs the function once per arch, logging to build/deps/logs.
step() {
    local name=$1 arch=$2 fn=$3 stamp="$work/stamps/$1-$2"
    [[ -f "$stamp" ]] && return
    echo "== $name $arch"
    if ! "$fn" "$arch" > "$work/logs/$name-$arch.log" 2>&1; then
        echo "failed, last lines of $work/logs/$name-$arch.log:" >&2
        tail -30 "$work/logs/$name-$arch.log" >&2
        exit 1
    fi
    touch "$stamp"
}

cmake_build() { # cmake_build <arch> <source dir> <cmake args...>
    local arch=$1 dir=$2; shift 2
    local prefix="$work/prefix-$arch"
    cmake -S "$dir" -B "$dir/build" -G Ninja \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_OSX_ARCHITECTURES="$arch" \
        -DCMAKE_OSX_DEPLOYMENT_TARGET=$MACOSX_DEPLOYMENT_TARGET \
        -DCMAKE_INSTALL_PREFIX="$prefix" \
        -DCMAKE_INSTALL_LIBDIR=lib \
        -DCMAKE_PREFIX_PATH="$prefix" \
        -DCMAKE_IGNORE_PREFIX_PATH="/opt/homebrew;/usr/local" \
        -DCMAKE_FIND_FRAMEWORK=LAST \
        -DCMAKE_INSTALL_NAME_DIR=@rpath \
        "$@"
    cmake --build "$dir/build" -j "$jobs"
    cmake --install "$dir/build"
}

build_openssl() {
    local arch=$1 dir="$work/openssl-$1"
    unpack "$src/$(basename "${OPENSSL[1]}")" "$dir"
    cd "$dir"
    ./Configure "darwin64-$arch-cc" no-shared no-tests no-docs no-apps \
        --prefix="$work/prefix-$arch" --libdir=lib "-mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET"
    make -j "$jobs" build_libs
    make install_dev
}

build_ffmpeg() {
    local arch=$1 dir="$work/ffmpeg-$1" cross=()
    unpack "$src/$(basename "${FFMPEG[1]}")" "$dir"
    cd "$dir"
    [[ "$arch" != "$(uname -m)" ]] && cross=(--enable-cross-compile)
    ./configure --prefix="$work/prefix-$arch" --arch="$arch" --target-os=darwin "${cross[@]}" \
        --cc="clang -arch $arch" --cxx="clang++ -arch $arch" \
        --extra-cflags="-mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET" \
        --extra-ldflags="-mmacosx-version-min=$MACOSX_DEPLOYMENT_TARGET" \
        --install-name-dir=@rpath --enable-shared --disable-static \
        --disable-autodetect --disable-everything --disable-programs --disable-doc --disable-network \
        --disable-avformat --disable-avdevice --disable-avfilter --disable-swresample \
        --enable-avcodec --enable-avutil --enable-swscale \
        --enable-decoder=h264 --enable-parser=h264
    make -j "$jobs"
    make install
}

build_jansson() {
    local dir="$work/jansson-$1"
    unpack "$src/$(basename "${JANSSON[1]}")" "$dir"
    cmake_build "$1" "$dir" -DJANSSON_BUILD_DOCS=OFF -DJANSSON_EXAMPLES=OFF \
        -DJANSSON_WITHOUT_TESTS=ON -DJANSSON_BUILD_SHARED_LIBS=OFF
}

build_freetype() {
    local dir="$work/freetype-$1"
    unpack "$src/$(basename "${FREETYPE[1]}")" "$dir"
    cmake_build "$1" "$dir" -DBUILD_SHARED_LIBS=OFF -DFT_DISABLE_ZLIB=ON -DFT_DISABLE_BZIP2=ON \
        -DFT_DISABLE_PNG=ON -DFT_DISABLE_HARFBUZZ=ON -DFT_DISABLE_BROTLI=ON
}

build_sdl3() {
    local dir="$work/sdl3-$1"
    unpack "$src/$(basename "${SDL3[1]}")" "$dir"
    # SDL_MAC_NO_SANDBOX turns on CGSSetGlobalHotKeyOperatingMode in the keyboard grab.
    cmake_build "$1" "$dir" -DSDL_SHARED=ON -DSDL_STATIC=OFF -DSDL_TESTS=OFF -DSDL_EXAMPLES=OFF \
        -DCMAKE_C_FLAGS=-DSDL_MAC_NO_SANDBOX -DCMAKE_OBJC_FLAGS=-DSDL_MAC_NO_SANDBOX
}

build_sdl3_ttf() {
    local dir="$work/sdl3_ttf-$1"
    unpack "$src/$(basename "${SDL3_TTF[1]}")" "$dir"
    cmake_build "$1" "$dir" -DBUILD_SHARED_LIBS=ON -DSDLTTF_VENDORED=OFF -DSDLTTF_HARFBUZZ=OFF \
        -DSDLTTF_PLUTOSVG=OFF -DSDLTTF_SAMPLES=OFF
}

build_freerdp() {
    local arch=$1 dir="$work/freerdp-$1"
    unpack "$src/FreeRDP-3.32.2.tar.gz" "$dir"
    PKG_CONFIG_LIBDIR="$work/prefix-$arch/lib/pkgconfig" PKG_CONFIG_PATH="" \
    cmake_build "$arch" "$dir" -DBUILD_SHARED_LIBS=OFF -DBUILD_TESTING=OFF \
        -DWITH_CLIENT=ON -DWITH_CLIENT_SDL=ON -DWITH_CLIENT_SDL3=ON -DWITH_CLIENT_SDL2=OFF \
        -DWITH_SDL_LINK_SHARED=ON -DWITH_CLIENT_MAC=OFF -DWITH_X11=OFF -DWITH_WAYLAND=OFF \
        -DWITH_SERVER=OFF -DWITH_SHADOW=OFF -DWITH_PROXY=OFF -DWITH_SAMPLE=OFF -DWITH_MANPAGES=OFF \
        -DWITH_PLATFORM_SERVER=OFF -DWITH_FFMPEG=ON -DWITH_SWSCALE=ON -DWITH_DSP_FFMPEG=OFF \
        -DWITH_VIDEO_FFMPEG=OFF -DWITH_OPENH264=OFF -DWITH_CUPS=OFF -DWITH_PCSC=OFF -DWITH_KRB5=OFF \
        -DWITH_FUSE=OFF -DWITH_JPEG=OFF -DWITH_OPUS=OFF -DWITH_LAME=OFF -DWITH_FAAD2=OFF -DWITH_FAAC=OFF \
        -DWITH_SOXR=OFF -DWITH_URIPARSER=OFF -DWITH_WEBVIEW=OFF -DWITH_AAD=OFF -DWITH_FIDO=OFF \
        -DWITH_CAIRO=OFF -DWITH_LIBSYSTEMD=OFF -DWITH_UNICODE_BUILTIN=ON -DWITH_JANSSON_REQUIRED=ON \
        -DCHANNEL_URBDRC=OFF -DWITH_VERBOSE_WINPR_ASSERT=OFF -DWITH_INTERNAL_RC4=ON -DWITH_INTERNAL_MD4=ON \
        -DWITH_INTERNAL_MD5=ON -DOPENSSL_ROOT_DIR="$work/prefix-$arch" -DOPENSSL_USE_STATIC_LIBS=ON
}

for arch in "${archs[@]}"; do
    fetch openssl "${OPENSSL[@]}";   step openssl "$arch" build_openssl
    fetch ffmpeg "${FFMPEG[@]}";     step ffmpeg "$arch" build_ffmpeg
    fetch jansson "${JANSSON[@]}";   step jansson "$arch" build_jansson
    fetch freetype "${FREETYPE[@]}"; step freetype "$arch" build_freetype
    fetch sdl3 "${SDL3[@]}";         step sdl3 "$arch" build_sdl3
    fetch sdl3_ttf "${SDL3_TTF[@]}"; step sdl3_ttf "$arch" build_sdl3_ttf
    fetch freerdp "${FREERDP[@]}";   step freerdp "$arch" build_freerdp
done

# Universal binaries: sdl-freerdp and the dylibs it loads, found by walking its
# @rpath dependencies, with @executable_path/../Frameworks as the search path.
echo "== universal"
rm -rf "$out" && mkdir -p "$out/bin" "$out/lib"
first="$work/prefix-${archs[0]}"
lipo_all() { # lipo_all <relative path> <output>
    local inputs=()
    for arch in "${archs[@]}"; do inputs+=("$work/prefix-$arch/$1"); done
    lipo -create "${inputs[@]}" -output "$2"
}
lipo_all bin/sdl-freerdp "$out/bin/sdl-freerdp"

deps_of() { otool -L "$1" | tail -n +2 | awk '{print $1}' | grep '^@rpath/' | sed 's|^@rpath/||'; }
queue=($(deps_of "$out/bin/sdl-freerdp"))
while ((${#queue[@]})); do
    lib=${queue[0]}; queue=("${queue[@]:1}")
    [[ -f "$out/lib/$lib" ]] && continue
    lipo_all "lib/$lib" "$out/lib/$lib"
    queue+=($(deps_of "$out/lib/$lib"))
done

# Every load command must point into the app or the system.
install_name_tool -add_rpath @executable_path/../Frameworks "$out/bin/sdl-freerdp" 2>/dev/null || true
for f in "$out/bin/sdl-freerdp" "$out"/lib/*.dylib; do
    install_name_tool -delete_rpath "$first/lib" "$f" 2>/dev/null || true
    for arch in "${archs[@]}"; do install_name_tool -delete_rpath "$work/prefix-$arch/lib" "$f" 2>/dev/null || true; done
    bad=$(otool -L "$f" | tail -n +2 | awk '{print $1}' | grep -v -E '^(@rpath/|/usr/lib/|/System/)' || true)
    [[ -z "$bad" ]] || { echo "$f links outside the app: $bad" >&2; exit 1; }
done
ls -la "$out/bin" "$out/lib"
lipo -info "$out/bin/sdl-freerdp"
