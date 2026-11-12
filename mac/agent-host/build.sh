#!/bin/bash
# Build the agent host, `cc-agent-host`: the Agent SDK host in src/, compiled by Bun into one executable per target.
#
#   mac/agent-host/build.sh <out-dir> [<target>...]
#
# Targets are darwin-arm64, darwin-x86_64, linux-x86_64 and linux-aarch64; with none, this machine's own. Writes
# `<out-dir>/cc-agent-host-<target>` for each, `<out-dir>/cc-agent-host` (universal) when both macOS targets are
# built, and `<out-dir>/AGENT_HOST_NOTICES`. The release runs it once per target on native runners, so each
# executable is run where it is built (check.sh), and joins the two macOS ones with `lipo`, as it does the other
# binaries.
#
# Bun is the release SOURCES pins, each zip checked against its SHA-256 before it is used: the zip matching this
# machine compiles, and each target's zip is the runtime its executable is made from, so nothing is fetched unchecked.
# The packages are the ones bun.lock pins, without the SDK's optional per-platform packages (each carries a Claude
# Code of its own; the host runs the user's) or the peers the host does not import, and with no install scripts. Both
# autoloads are off: a standalone Bun executable otherwise loads `.env` and `bunfig.toml` from the folder it starts
# in, and the host starts in people's projects. Each macOS executable is signed with the hardened runtime and exactly
# entitlements.plist: allow-jit, without which JavaScriptCore cannot JIT and the SDK does not start. Ad hoc here; the
# release signs the universal one with its Developer ID. The same sources give the same bytes. It needs bash, curl
# and unzip, and for macOS targets lipo and codesign. Outside <out-dir> and a temporary folder it writes only Bun's
# package cache, ~/.bun/install/cache.
#
# To change a package: edit package.json, run the pinned Bun's `bun install --lockfile-only` here, commit bun.lock.
set -euo pipefail
export LC_ALL=C

out="${1:?usage: build.sh <out-dir> [<target>...]}"
shift
here="$(cd "$(dirname "$0")" && pwd)"
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) native=darwin-arm64 ;;
    Darwin-x86_64) native=darwin-x86_64 ;;
    Linux-x86_64) native=linux-x86_64 ;;
    Linux-aarch64) native=linux-aarch64 ;;
    *) echo "unsupported system: $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac
[ "$#" -gt 0 ] || set -- "$native"
# A target's Bun zip, and the name `bun build --target` gives it.
zip_of() {
    case "$1" in
        darwin-arm64) echo bun-darwin-aarch64 ;;
        darwin-x86_64) echo bun-darwin-x64 ;;
        linux-x86_64) echo bun-linux-x64 ;;
        linux-aarch64) echo bun-linux-aarch64 ;;
        *) echo "unknown target: $1" >&2; return 1 ;;
    esac
}
bun_target() {
    case "$1" in
        darwin-arm64) echo bun-darwin-arm64 ;;
        darwin-x86_64) echo bun-darwin-x64 ;;
        linux-x86_64) echo bun-linux-x64 ;;
        linux-aarch64) echo bun-linux-arm64 ;;
    esac
}
if command -v sha256sum >/dev/null; then sha256="sha256sum"; else sha256="shasum -a 256"; fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$out" "$work/bun" "$work/unsigned"

# A source SOURCES pins, into $work, refused unless it has the pinned SHA-256.
fetch() {
    local name="$1" url="" sum=""
    read -r url sum < <(awk -v n="$name" '$1 == n { print $3, $4 }' "$here/SOURCES") || true
    [ -n "$sum" ] || { echo "SOURCES pins no $name" >&2; exit 1; }
    curl -fsSL --connect-timeout 30 --retry 3 -o "$work/$(basename "$url")" "$url"
    if [ "$($sha256 "$work/$(basename "$url")" | cut -d' ' -f1)" != "$sum" ]; then
        echo "$name: $url does not match its pinned SHA-256" >&2
        exit 1
    fi
}
bun_of() {
    local zip
    zip="$(zip_of "$1")"
    if [ ! -x "$work/bun/$zip/bun" ]; then
        fetch "$zip"
        unzip -q "$work/$zip.zip" -d "$work/bun"
    fi
    echo "$work/bun/$zip/bun"
}
for target in "$@"; do zip_of "$target" >/dev/null; done
bun="$(bun_of "$native")"

mkdir "$work/app"
cp -R "$here/package.json" "$here/bun.lock" "$here/src" "$work/app/"
(cd "$work/app" && "$bun" install --frozen-lockfile --omit=optional --omit=peer --ignore-scripts --no-progress)
for target in "$@"; do
    runtime="$(bun_of "$target")"
    (cd "$work/app" && "$bun" build --compile --no-compile-autoload-dotenv --no-compile-autoload-bunfig \
        --target="$(bun_target "$target")" --compile-executable-path="$runtime" \
        ./src/main.ts --outfile "$work/unsigned/cc-agent-host-$target")
    cp "$work/unsigned/cc-agent-host-$target" "$out/cc-agent-host-$target"
done
macos=()
for target in "$@"; do
    case "$target" in darwin-*) macos+=("$out/cc-agent-host-$target") ;; esac
done
if [ -e "$work/unsigned/cc-agent-host-darwin-arm64" ] && [ -e "$work/unsigned/cc-agent-host-darwin-x86_64" ]; then
    lipo -create -output "$out/cc-agent-host" \
        "$work/unsigned/cc-agent-host-darwin-arm64" "$work/unsigned/cc-agent-host-darwin-x86_64"
    macos+=("$out/cc-agent-host")
fi
for path in ${macos[@]+"${macos[@]}"}; do
    codesign --force --options runtime --entitlements "$here/entitlements.plist" --sign - "$path"
done

# What the executable carries: Bun, and every package installed to build it, each with its licence.
fetch LICENSE.md
{
    echo "CodeConnect's agent host (cc-agent-host) is built from the following."
    echo
    echo "== Bun $(awk '$1 == "LICENSE.md" { print $2 }' "$here/SOURCES") ($(awk '$1 == "LICENSE.md" { print $3 }' "$here/SOURCES"))"
    echo
    cat "$work/LICENSE.md"
    for package in "$work/app/node_modules"/[!@.]*/ "$work/app/node_modules"/@*/*/; do
        [ -f "$package/package.json" ] || continue
        name="${package#"$work/app/node_modules/"}"
        echo
        echo "== ${name%/} $(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$package/package.json" | head -1)"
        echo
        found=""
        for licence in "$package"LICENSE* "$package"LICENCE* "$package"COPYING*; do
            [ -f "$licence" ] || continue
            cat "$licence"
            found=1
        done
        [ -n "$found" ] || { echo "${name%/} carries no licence file" >&2; exit 1; }
    done
} > "$out/AGENT_HOST_NOTICES"

for target in "$@"; do $sha256 "$out/cc-agent-host-$target"; done
[ ! -e "$out/cc-agent-host" ] || $sha256 "$out/cc-agent-host"
$sha256 "$out/AGENT_HOST_NOTICES"
