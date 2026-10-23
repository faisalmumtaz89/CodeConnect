#!/bin/bash
# Build CodeConnect's own tmux, `cc-tmux`, for this machine's architecture.
#
#   mac/tmux/build.sh <out-dir>
#
# Writes `<out-dir>/cc-tmux-<arch>` and `<out-dir>/THIRD_PARTY_NOTICES`. The
# release runs it once per architecture on native runners and joins the two
# with `lipo`, as it does the Rust binaries; locally, `arch -x86_64` runs it
# for Intel under Rosetta.
#
# Every source is the exact tarball pinned in `SOURCES`, checked against its
# SHA-256 before it is unpacked. libevent, utf8proc and jemalloc are linked in
# statically; ncurses and libSystem come from the macOS SDK, so the result
# depends only on what every Mac ships. `pkg-config` is pointed at the private
# prefix alone — a Homebrew copy of any of these libraries on the build
# machine must never be what gets linked.
set -euo pipefail

out="${1:?usage: build.sh <out-dir>}"
here="$(cd "$(dirname "$0")" && pwd)"
arch="$(uname -m)"
case "$arch" in
    arm64) host=aarch64-apple-darwin ;;
    x86_64) host=x86_64-apple-darwin ;;
    *) echo "unsupported architecture: $arch" >&2; exit 1 ;;
esac

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
prefix="$work/prefix"
mkdir -p "$out" "$prefix/include" "$prefix/lib/pkgconfig"

bundle=""
fetch() {
    local name="$1" url="$2" sum="$3" file
    file="$work/$(basename "$url")"
    curl -fsSL --retry 3 -o "$file" "$url"
    if [ "$(shasum -a 256 "$file" | cut -d' ' -f1)" != "$sum" ]; then
        echo "$name: $url does not match its pinned SHA-256" >&2
        exit 1
    fi
    mkdir -p "$work/src/$name"
    tar -xf "$file" -C "$work/src/$name" --strip-components 1
}
while read -r name version url sum; do
    case "$name" in
        '#'* | '') ;;
        BUNDLE) bundle="$version" ;;
        *) fetch "$name" "$url" "$sum" ;;
    esac
done <"$here/SOURCES"
[ -n "$bundle" ] || { echo "SOURCES names no BUNDLE" >&2; exit 1; }

sdk="$(xcrun --sdk macosx --show-sdk-path)"
# Through the environment as well as the flags: jemalloc compiles its symbol
# tables without CFLAGS, and clang reads SDKROOT on its own.
export SDKROOT="$sdk"
export MACOSX_DEPLOYMENT_TARGET=13.0
CC="$(xcrun -f clang)"
export CC
export CFLAGS="-arch $arch -isysroot $sdk -mmacosx-version-min=13.0 -O2"
export LDFLAGS="-arch $arch -isysroot $sdk -mmacosx-version-min=13.0"
export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
export PKG_CONFIG_PATH=""
jobs="$(sysctl -n hw.ncpu)"

(
    cd "$work/src/libevent"
    ./configure --host="$host" --prefix="$prefix" --disable-shared --disable-openssl \
        --disable-samples --disable-libevent-regress --disable-debug-mode
    make -j"$jobs"
    make install
)

(
    cd "$work/src/utf8proc"
    make -j"$jobs" libutf8proc.a CC="$CC" CFLAGS="$CFLAGS"
    cp utf8proc.h "$prefix/include/"
    cp libutf8proc.a "$prefix/lib/"
    cat >"$prefix/lib/pkgconfig/libutf8proc.pc" <<EOF
Name: libutf8proc
Description: UTF-8 processing
Version: $(awk '$1 == "utf8proc" { print $2 }' "$here/SOURCES")
Cflags: -I$prefix/include
Libs: -L$prefix/lib -lutf8proc
EOF
)

(
    cd "$work/src/jemalloc"
    # No symbol prefix, as Homebrew builds it: tmux calls `mallctl`, and on
    # macOS jemalloc otherwise names it `je_mallctl`.
    ./configure --host="$host" --prefix="$prefix" --disable-cxx --with-jemalloc-prefix=
    make -j"$jobs" build_lib_static
    make install_include install_lib_static install_lib_pc
    # All of it, not just the objects tmux names. jemalloc takes over as the
    # process's default malloc zone from code nothing references, and without
    # it tmux frees memory macOS's own libraries allocated — a crash at the
    # first `tmux -V`. tmux uses jemalloc on macOS to avoid a calloc bug that
    # aborts it entering copy mode (tmux issue 5385).
    sed -i '' "s|^Libs: .*|Libs: -Wl,-force_load,$prefix/lib/libjemalloc.a|" \
        "$prefix/lib/pkgconfig/jemalloc.pc"
)

(
    cd "$work/src/tmux"
    # The bundle revision is the version tmux reports: `tmux -V` prints
    # `tmux 3.7c-cc1`, which is how `codeconnect` tells its own build apart
    # from any other 3.7c, including an earlier bundle of the same release.
    upstream="$(awk '$1 == "tmux" { print $2 }' "$here/SOURCES")"
    sed -i '' \
        -e "s/^PACKAGE_VERSION='$upstream'\$/PACKAGE_VERSION='$bundle'/" \
        -e "s/^PACKAGE_STRING='tmux $upstream'\$/PACKAGE_STRING='tmux $bundle'/" \
        -e "s/^ VERSION='$upstream'\$/ VERSION='$bundle'/" \
        configure
    # The features Homebrew's tmux is built with, so a pane sees the terminal
    # it saw before: sixel is part of what tmux reports to a program asking
    # what the terminal supports. The built-in TERM is screen-256color because
    # macOS 13's terminfo has no tmux-256color entry; `codeconnect` gives a
    # Codex pane tmux-256color wherever the system has it, as Homebrew's
    # per-macOS builds do.
    ./configure --host="$host" --enable-utf8proc --enable-jemalloc --enable-sixel \
        --with-TERM=screen-256color
    make -j"$jobs"
    cp tmux "$out/cc-tmux-$arch"
)

binary="$out/cc-tmux-$arch"
if [ "$("$binary" -V)" != "tmux $bundle" ]; then
    echo "built tmux reports '$("$binary" -V)', not 'tmux $bundle'" >&2
    exit 1
fi
if otool -L "$binary" | tail -n +2 | grep -v -E '^\s+(/usr/lib/|/System/Library/)'; then
    echo "cc-tmux links a library outside the operating system" >&2
    exit 1
fi
if [ "$(vtool -show-build "$binary" | awk '$1 == "minos" { print $2 }')" != "13.0" ]; then
    echo "cc-tmux does not target macOS 13.0" >&2
    exit 1
fi

{
    echo "CodeConnect's tmux ($bundle) is built from the following sources."
    for name in tmux libevent utf8proc jemalloc; do
        for licence in COPYING LICENSE LICENSE.md; do
            if [ -f "$work/src/$name/$licence" ]; then
                printf '\n==== %s %s — %s ====\n\n' "$name" \
                    "$(awk -v n="$name" '$1 == n { print $2 }' "$here/SOURCES")" "$licence"
                cat "$work/src/$name/$licence"
            fi
        done
    done
} >"$out/THIRD_PARTY_NOTICES"

echo "built $binary ($("$binary" -V))"
