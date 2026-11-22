#!/bin/zsh
#
# ui-suite-gate.sh — the iPhone UI suite's gate for one commit (RELEASING.md, step 1).
#
#   ios/scripts/ui-suite-gate.sh <40-hex sha> [only-testing]
#
# Runs the whole UI suite, one test at a time, on a fresh iPhone 17 Pro, in a clean checkout of exactly that commit,
# with no computer given: the live classes (a file that reads the runner's environment) skip. Success is xcodebuild's
# zero exit, a closing line of 0 failures, and the skipped classes being exactly the live ones; anything else is
# failure. The verdict is posted on the commit as the status `iphone-ui (this Mac)`, which the release's pin job
# requires; GitHub takes a status only for a commit it has, so a run made before the push waits for it, at most
# GATE_WAIT_MIN minutes (default 120), and posts nothing if it never comes. `only-testing` narrows the run (a must-fail
# of one class); such a run skips no live class, so it can never be a success. Exit 0 only on a posted success.
# The caller holds this Mac's locks; the script takes none.

set -u
SHA=${1:-}
ONLY=${2:-CodeConnectUITests}
CONTEXT="iphone-ui (this Mac)"
WAIT_MIN=${GATE_WAIT_MIN:-120}
[[ $SHA =~ ^[0-9a-f]{40}$ ]] || { print "usage: ui-suite-gate.sh <40-hex sha> [only-testing]"; exit 2; }
REPO_DIR=$(git -C "${0:A:h}" rev-parse --show-toplevel) || exit 2
git -C "$REPO_DIR" cat-file -e "$SHA^{commit}" 2>/dev/null || { print "no such commit here: $SHA"; exit 2; }
env | grep -q '^TEST_RUNNER_CC_' && { print "refusing: a computer is given for the live classes"; exit 2; }

WORK=$(mktemp -d "${TMPDIR:-/tmp}/ui-suite-gate.XXXXXX")
TREE=$WORK/tree
SIM=""
POSTED=no
VERDICT=failure
cleanup() {
    [[ -n $SIM ]] && { xcrun simctl shutdown "$SIM" >/dev/null 2>&1; xcrun simctl delete "$SIM" >/dev/null 2>&1; }
    [[ -d $TREE ]] && git -C "$REPO_DIR" worktree remove --force "$TREE"
    rm -rf "$WORK"
    print "ui-suite-gate: $SHA $VERDICT, status posted: $POSTED"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

git -C "$REPO_DIR" worktree add --quiet --detach "$TREE" "$SHA" || exit 2
[[ $(git -C "$TREE" rev-parse HEAD) == "$SHA" && -z $(git -C "$TREE" status --porcelain) ]] \
    || { print "refusing: the checkout is not exactly $SHA, clean"; exit 2; }

TYPE=com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro
RUNTIME=$(xcrun simctl list runtimes --json | python3 -c '
import json, sys
rs = [r for r in json.load(sys.stdin)["runtimes"] if r["isAvailable"] and r["identifier"].startswith("com.apple.CoreSimulator.SimRuntime.iOS")
      and any(d["identifier"] == sys.argv[1] for d in r.get("supportedDeviceTypes", []))]
rs.sort(key=lambda r: [int(p) for p in r["version"].split(".")])
print(rs[-1]["identifier"] if rs else "")' $TYPE)
[[ -n $RUNTIME ]] || { print "no available iOS runtime runs the iPhone 17 Pro"; exit 2; }
SIM=$(xcrun simctl create "iPhone 17 Pro (UI gate)" $TYPE "$RUNTIME") || exit 2
print "ui-suite-gate: $SHA on $RUNTIME, simulator $SIM"

LOG=$WORK/ui.log
(cd "$TREE/ios" && xcodebuild test -project CodeConnect.xcodeproj -scheme CodeConnect -destination "id=$SIM" \
    -derivedDataPath "$WORK/derived" -clonedSourcePackagesDirPath "$WORK/spm" -only-testing:"$ONLY") >"$LOG" 2>&1
STATUS=$?
EXECUTED=$(grep -E 'Executed [0-9]+ tests?,' "$LOG" | tail -1 | sed -E 's/^[[:space:]]+//')
SKIPPED=$(grep -E "Test Case .* skipped" "$LOG" | sed -E 's/.*-\[CodeConnectUITests\.([A-Za-z0-9_]+) .*/\1/' | sort -u)
LIVE=$(cd "$TREE/ios/CodeConnectUITests" && grep -l 'processInfo.environment' -- *.swift \
    | xargs sed -n 's/^final class \([A-Za-z0-9_]*\): .*/\1/p' | sort -u)
print "ui-suite-gate: xcodebuild exit $STATUS; ${EXECUTED:-no closing line}"
if [[ $STATUS -eq 0 && $EXECUTED == *" 0 failures "* && -n $LIVE && $SKIPPED == "$LIVE" ]]; then
    VERDICT=success
fi
[[ $VERDICT == success ]] || grep -E "error: -\[|' failed \(" "$LOG" | head -20

SLUG=$(cd "$REPO_DIR" && gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null)
for _ in $(seq 0 "$WAIT_MIN"); do
    gh api "repos/$SLUG/commits/$SHA" --jq .sha >/dev/null 2>&1 && break
    [[ $WAIT_MIN -eq 0 ]] && break
    sleep 60
done
if [[ -n $SLUG ]] && gh api "repos/$SLUG/commits/$SHA" --jq .sha >/dev/null 2>&1; then
    gh api -X POST "repos/$SLUG/statuses/$SHA" -f state="$VERDICT" -f context="$CONTEXT" \
        -f description="${${EXECUTED:-no closing line}[1,140]}" >/dev/null && POSTED=yes
else
    print "ui-suite-gate: GitHub does not have $SHA; no status posted"
fi
[[ $VERDICT == success && $POSTED == yes ]] && exit 0
exit 1
