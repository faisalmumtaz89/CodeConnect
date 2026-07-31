#!/bin/bash
# Build and install the CodeConnect Mac binaries into ~/.codeconnect/bin.
#
# The `rm` before each `cp` is not cosmetic. macOS caches a binary's ad-hoc code
# signature against its inode; overwriting an installed binary in place leaves
# the cached signature pointing at different bytes, and the kernel then SIGKILLs
# every subsequent exec with no diagnostic at all (observed: exit 137, no
# output). Replacing the inode avoids it, and the re-sign makes it explicit.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
prefix="${CODECONNECT_HOME:-$HOME/.codeconnect}"
bin="$prefix/bin"

echo "building release binaries…"
# Named explicitly rather than building the whole workspace: `soak/` is a test
# harness, and there is no reason for `./install.sh` to spend a minute of LTO on
# something that never ships.
cargo build --release --manifest-path "$here/Cargo.toml" \
    --bin cc --bin ccd --bin cc-hook

mkdir -p "$bin"
for binary in cc ccd cc-hook; do
    rm -f "$bin/$binary"
    cp "$here/target/release/$binary" "$bin/$binary"
    codesign --force --sign - "$bin/$binary" 2>/dev/null || true
    "$bin/$binary" --version </dev/null >/dev/null || {
        echo "installed $binary does not run" >&2
        exit 1
    }
done

echo "installed to $bin"
echo
echo "next:"
echo "  export PATH=\"$bin:\$PATH\""
echo "  cc daemon install          # run ccd under launchd (restarts on crash)"
echo "  cc claude                  # run a session in the current directory"
echo "  cc pair                    # QR code to pair the phone (add --ssh to"
echo "                             # also install the app's SSH key)"
echo
echo "already installed? \`cc daemon restart\` picks up these binaries."
