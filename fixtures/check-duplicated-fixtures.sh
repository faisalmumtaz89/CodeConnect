#!/usr/bin/env bash
# Every fixture the iOS target carries a *copy* of must be byte-identical to the
# copy under `fixtures/codex/`, which is the one the Rust side reads and the one
# the capture tooling regenerates. `ci.yml` runs this exact script — the workflow
# invokes it rather than restating the loop, so the local gate and the CI gate
# cannot drift apart the way the fixtures themselves did.
#
# **This absence shipped a blocker.** Xcode cannot reference a file outside its
# own source root, so the phone's fixtures are literal duplicates rather than
# links, and nothing compared them. A regenerated `refusal-sentences.json` on the
# Rust side left the iOS copy a release behind: the composer's refusal table said
# a link was composable while the daemon it was talking to was refusing it, and
# the phone failed *open* against a daemon that said no. Two copies of a file are
# a fact about the build system; two copies that are allowed to differ is a bug
# waiting for whoever regenerates one of them.
#
# The pairing is discovered, never listed: any file under either iOS resource
# directory whose basename also names a file in `fixtures/codex/` is a copy and
# is compared. A sixth duplicate added later is therefore covered without anyone
# remembering to add it here — which is the same reason the workflow lint job
# discovers its workflows instead of naming them.
#
# What it deliberately does not catch: an iOS copy whose source was *renamed* out
# of `fixtures/codex/` matches no basename and is silently skipped. Closing that
# would mean naming the expected set, and a hard-coded list is the failure mode
# this check exists to remove.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_dir="$root/fixtures/codex"
copy_dirs=("$root/ios/CodeConnect/Resources" "$root/ios/CodeConnectTests/Resources")

# A directory that moved must fail the check rather than quietly contribute no
# files to it. A guard that passes because it looked nowhere is worse than none.
for dir in "$source_dir" "${copy_dirs[@]}"; do
  if [ ! -d "$dir" ]; then
    echo "::error::${dir#"$root/"} does not exist — this check is pointing at nothing"
    exit 1
  fi
done

differing=0
while IFS= read -r -d '' copy; do
  source="$source_dir/$(basename "$copy")"
  [ -f "$source" ] || continue
  if ! cmp -s "$source" "$copy"; then
    echo "::error::${copy#"$root/"} differs from fixtures/codex/$(basename "$copy")"
    cmp "$source" "$copy" || true
    differing=$((differing + 1))
  fi
done < <(find "${copy_dirs[@]}" -type f -print0)

if [ "$differing" -ne 0 ]; then
  echo "::error::$differing duplicated fixture(s) are stale — re-copy from fixtures/codex/"
  exit 1
fi
