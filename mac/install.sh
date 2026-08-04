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
    --bin codeconnect --bin ccd --bin cc-hook

mkdir -p "$bin"
for binary in codeconnect ccd cc-hook; do
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

# Remember which clone produced these binaries, so `codeconnect update` can
# pull and reinstall without asking the user where their checkout lives.
repo_root="$(dirname "$here")"
printf '%s\n' "$repo_root" > "$prefix/source-checkout"


# **The restart is part of the install, not advice at the bottom of it.**
#
# This script used to end with "already installed? \`codeconnect daemon
# restart\` picks up these binaries" — and twice in one day the binaries were
# rebuilt, that line went unread, and the running daemon quietly stayed on
# yesterday's build while every visible sign said the deploy had happened.
# A deploy step a human has to remember is a deploy step that silently does
# not happen. Sessions survive the restart by architecture: ccd is never a
# session's parent, and the soak harness kills it mid-session to prove the
# log comes back gap-free.
# Gated on the plist file, not on running the binary: the kernel can refuse
# the freshly signed inode exactly once (observed: Abort trap: 6 on the first
# exec, healthy ever after), and a file test cannot be refused. The same
# transient gets one settle-and-retry around the restart itself.
if [ -f "$HOME/Library/LaunchAgents/com.codeconnect.ccd.plist" ]; then
    echo "restarting the daemon onto these binaries…"
    "$bin/codeconnect" daemon restart || { sleep 1; "$bin/codeconnect" daemon restart; }
else
    echo "next:"
    echo "  export PATH=\"$bin:\$PATH\""
    echo "  codeconnect daemon install   # run ccd under launchd (restarts on crash)"
    echo "  codeconnect claude          # run a session in the current directory"
    echo "  codeconnect pair            # QR code to pair the phone (add --ssh to"
    echo "                             # also install the app's SSH key)"
fi
