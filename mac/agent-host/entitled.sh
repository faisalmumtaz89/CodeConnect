#!/bin/bash
# Whether a macOS agent host carries exactly the entitlements entitlements.plist names, in every architecture's
# signature. Without allow-jit the host does not start under the hardened runtime; anything beyond it widens what the
# signed executable may do.
#
#   mac/agent-host/entitled.sh <executable>
set -euo pipefail
executable="$1"
expected="$(plutil -convert json -o - "$(cd "$(dirname "$0")" && pwd)/entitlements.plist")"
for arch in $(lipo -archs "$executable"); do
  carried="$(codesign --display --entitlements - --xml --arch "$arch" "$executable" 2>/dev/null \
    | plutil -convert json -o - - 2>/dev/null)" || carried=""
  if [ "$carried" != "$expected" ]; then
    echo "$executable ($arch) carries the entitlements ${carried:-(none)}, not exactly $expected" >&2
    exit 1
  fi
done
echo "$executable carries exactly $expected in $(lipo -archs "$executable")"
