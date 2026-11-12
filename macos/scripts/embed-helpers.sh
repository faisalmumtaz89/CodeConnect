#!/bin/sh
# Xcode build phase: copies CodeConnect's own binaries into the app (Contents/Helpers) and signs each with
# the app's identity, so the app can install them (Installer.swift). They come from $CODECONNECT_HELPERS,
# by default macos/Helpers, which is not in git: ccd, codeconnect and cc-hook from `cargo build --release`
# in mac/, cc-tmux and its THIRD_PARTY_NOTICES from mac/tmux/build.sh, cc-agent-host and its AGENT_HOST_NOTICES
# from mac/agent-host/build.sh. Their notices travel with them, as in every release archive: the repository's
# LICENSE and the two notices files go into Contents/Resources, and the installer puts them beside the binaries.
# A Release build without all of them fails; a Debug build without them carries none, and the app's checklist
# says so.
set -eu
src="${CODECONNECT_HELPERS:-$SRCROOT/Helpers}"
dst="$TARGET_BUILD_DIR/$CONTENTS_FOLDER_PATH/Helpers"
res="$TARGET_BUILD_DIR/$UNLOCALIZED_RESOURCES_FOLDER_PATH"
names="ccd codeconnect cc-hook cc-tmux cc-agent-host"
notices="THIRD_PARTY_NOTICES AGENT_HOST_NOTICES"
missing=""
for name in $names; do
    [ -x "$src/$name" ] || missing="$missing $name"
done
for name in $notices; do
    [ -f "$src/$name" ] || missing="$missing $name"
done
if [ -n "$missing" ]; then
    if [ "$CONFIGURATION" = Release ]; then
        echo "error: $src lacks:$missing" >&2
        exit 1
    fi
    echo "warning: $src lacks:$missing; this build carries no background service"
    rm -rf "$dst"
    rm -f "$res/LICENSE" "$res/THIRD_PARTY_NOTICES" "$res/AGENT_HOST_NOTICES"
    exit 0
fi
mkdir -p "$dst" "$res"
# A real identity signs them as notarization requires, as the release workflow signs the same binaries: the
# hardened runtime and a secure timestamp. An ad hoc signature carries neither. The agent host keeps the one
# entitlement it needs either way (allow-jit, mac/agent-host/entitlements.plist).
identity="${EXPANDED_CODE_SIGN_IDENTITY:--}"
for name in $names; do
    rm -f "$dst/$name"
    cp "$src/$name" "$dst/$name"
    if [ "$name" = cc-agent-host ]; then
        set -- --entitlements "$SRCROOT/../mac/agent-host/entitlements.plist"
    else
        set --
    fi
    if [ "$identity" = "-" ]; then
        codesign --force --sign - "$@" "$dst/$name"
    else
        codesign --force --options runtime --timestamp "$@" --sign "$identity" "$dst/$name"
    fi
done
cp "$SRCROOT/../LICENSE" "$res/LICENSE"
for name in $notices; do
    cp "$src/$name" "$res/$name"
done
