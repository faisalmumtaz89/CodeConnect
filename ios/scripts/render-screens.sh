#!/usr/bin/env bash
#
# render-screens.sh — photograph every catalogued screen, at L and at AX5.
#
# The render harness is an **inspection instrument, not an assertion test**. It
# makes no pixel assertions. It reaches every state in
# `CodeConnectRenderHarness/RenderCatalog.swift`, proves the process is at the
# content-size category the pass claims, photographs it, and **exits nonzero if
# any scenario cannot be reached or captured** — because a state nobody can
# reach is a state nobody has looked at.
#
#   ios/scripts/render-screens.sh --device "iPhone 17 Pro"
#
# Output:  ios/.artifacts/ui-renders/<timestamp>/{L,ax5}/<scenario>.png
#          ios/.artifacts/ui-renders/<timestamp>/report.txt
#          (gitignored — see ios/.gitignore)
#
# It runs its **own** simulator, cloned from the named device type, and deletes
# it afterwards, so a render pass can never leave your working simulator parked
# at AX5. That footgun cost four `DeckUITests` failures once already.
#
# Why `simctl` sets the type size and the test never calls `launch()`:
# `xcodebuild test` plus `XCUIApplication.launch()` resets the simulator's
# content-size category to the default, and `TEST_RUNNER_*` environment never
# reaches the runner. An "AX5 pass" that silently ran at `L` is indistinguishable
# from one that worked — several earlier measurements were exactly that. So the
# category is set here, once per invocation, and the harness asserts the category
# the app actually resolved to before it photographs anything.

set -euo pipefail

DEVICE_TYPE="iPhone 17 Pro"
KEEP_SIM=0
ONLY_SIZE=""

usage() {
    cat <<'USAGE'
usage: render-screens.sh [--device <name>] [--size L|ax5] [--keep-simulator]

  --device <name>     simulator device type to clone (default: "iPhone 17 Pro")
  --size L|ax5        render only one size (default: both)
  --keep-simulator    do not delete the simulator this run created
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --device) DEVICE_TYPE="${2:?--device needs a value}"; shift 2 ;;
        --size) ONLY_SIZE="${2:?--size needs a value}"; shift 2 ;;
        --keep-simulator) KEEP_SIM=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

IOS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$IOS_DIR"

STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="$IOS_DIR/.artifacts/ui-renders/$STAMP"
WORK="$IOS_DIR/.artifacts/ui-renders/.work-$STAMP"
REPORT="$OUT/report.txt"
mkdir -p "$OUT" "$WORK"

say() { printf '%s\n' "$*" | tee -a "$REPORT"; }

# ---------------------------------------------------------------------------
# A simulator of our own, deleted on the way out.
# ---------------------------------------------------------------------------

DEVICE_TYPE_ID="$(
    xcrun simctl list devicetypes --json \
        | python3 -c '
import json, sys
name = sys.argv[1]
for d in json.load(sys.stdin)["devicetypes"]:
    if d["name"] == name:
        print(d["identifier"]); break
' "$DEVICE_TYPE"
)"
[[ -n "$DEVICE_TYPE_ID" ]] || { echo "no such device type: $DEVICE_TYPE" >&2; exit 2; }

RUNTIME_ID="$(
    xcrun simctl list runtimes --json \
        | python3 -c '
import json, sys
runtimes = [r for r in json.load(sys.stdin)["runtimes"]
            if r["isAvailable"] and r["identifier"].startswith("com.apple.CoreSimulator.SimRuntime.iOS")]
runtimes.sort(key=lambda r: [int(p) for p in r["version"].split(".")])
print(runtimes[-1]["identifier"] if runtimes else "")
'
)"
[[ -n "$RUNTIME_ID" ]] || { echo "no available iOS simulator runtime" >&2; exit 2; }

SIM_ID="$(xcrun simctl create "CodeConnect Renders $STAMP" "$DEVICE_TYPE_ID" "$RUNTIME_ID")"

cleanup() {
    local status=$?
    if [[ $KEEP_SIM -eq 0 ]]; then
        xcrun simctl shutdown "$SIM_ID" >/dev/null 2>&1 || true
        xcrun simctl delete "$SIM_ID" >/dev/null 2>&1 || true
    else
        echo "simulator kept: $SIM_ID"
    fi
    rm -rf "$WORK"
    exit $status
}
trap cleanup EXIT

xcrun simctl boot "$SIM_ID" >/dev/null 2>&1 || true
xcrun simctl bootstatus "$SIM_ID" -b >/dev/null

say "CodeConnect render pass $STAMP"
say "  device    $DEVICE_TYPE ($SIM_ID)"
say "  runtime   $RUNTIME_ID"
say "  output    $OUT"
say ""

# ---------------------------------------------------------------------------
# Build once; run once per size.
# ---------------------------------------------------------------------------

# Kept between runs, deliberately: a render pass is something you do several
# times in an afternoon while looking at what changed, and rebuilding the whole
# package graph each time is how a review instrument stops being used.
DD="$IOS_DIR/.artifacts/render-derived-data"
say "building CodeConnect Renders…"
xcodebuild build-for-testing \
    -project CodeConnect.xcodeproj \
    -scheme "CodeConnect Renders" \
    -destination "platform=iOS Simulator,id=$SIM_ID" \
    -derivedDataPath "$DD" \
    >"$WORK/build.log" 2>&1 \
    || { echo "build failed — see $WORK/build.log" >&2; tail -40 "$WORK/build.log" >&2; exit 1; }

# size key -> (simctl content_size argument, test method)
render_size() {
    local key="$1" content_size="$2" method="$3"
    local bundle="$WORK/$key.xcresult"
    local dir="$OUT/$key"
    mkdir -p "$dir"

    say "── $key ($content_size)"
    # The whole reason this lives in the script and not in the test. See the
    # header note.
    xcrun simctl ui "$SIM_ID" content_size "$content_size" >/dev/null

    local status=0
    xcodebuild test-without-building \
        -project CodeConnect.xcodeproj \
        -scheme "CodeConnect Renders" \
        -destination "platform=iOS Simulator,id=$SIM_ID" \
        -derivedDataPath "$DD" \
        -resultBundlePath "$bundle" \
        -only-testing:"CodeConnectRenderHarness/RenderPass/$method" \
        >"$WORK/$key.log" 2>&1 || status=$?

    # Attachments come out whether the pass succeeded or not: a scenario that
    # could not be reached is photographed too, and what the app was showing
    # when it stopped being drivable is usually the whole answer.
    if [[ -d "$bundle" ]]; then
        xcrun xcresulttool export attachments \
            --path "$bundle" --output-path "$WORK/$key-attachments" >/dev/null 2>&1 || true
        python3 - "$WORK/$key-attachments" "$dir" <<'PY'
import json, os, re, shutil, sys

src, dst = sys.argv[1], sys.argv[2]
manifest = os.path.join(src, "manifest.json")
if not os.path.exists(manifest):
    sys.exit(0)
with open(manifest) as handle:
    entries = json.load(handle)

# XCTest decorates an attachment's name with its index and a UUID
# (`fleet-cached--L_0_3B4D6CF0-….png`). The scenario name is the artefact; the
# uniquifier is noise, and a filename you cannot predict is a filename nobody
# can diff against last week's pass.
DECORATION = re.compile(r"_\d+_[0-9A-Fa-f-]{36}$")

for test in entries:
    for attachment in test.get("attachments", []):
        exported = attachment.get("exportedFileName")
        if not exported:
            continue
        source = os.path.join(src, exported)
        if not os.path.exists(source):
            continue
        name = attachment.get("suggestedHumanReadableName") or exported
        stem = DECORATION.sub("", os.path.splitext(name)[0])
        shutil.copyfile(source, os.path.join(dst, f"{stem}.png"))
PY
    fi

    local shots
    shots="$(find "$dir" -name '*.png' | wc -l | tr -d ' ')"
    say "   $shots renders → $dir"

    if [[ $status -ne 0 ]]; then
        say "   FAILED — scenarios that could not be reached:"
        grep -E "could not reach|rendered at .*, not |scenarios could not be rendered" \
            "$WORK/$key.log" | sed 's/^/     /' | sort -u | tee -a "$REPORT" || true
        say "   full log: $WORK/$key.log (copied beside the renders)"
        cp "$WORK/$key.log" "$dir/xcodebuild.log" 2>/dev/null || true
        return 1
    fi
    return 0
}

FAILED=0
if [[ -z "$ONLY_SIZE" || "$ONLY_SIZE" == "L" ]]; then
    render_size L large testRendersEveryScenarioAtLarge || FAILED=1
fi
if [[ -z "$ONLY_SIZE" || "$ONLY_SIZE" == "ax5" ]]; then
    render_size ax5 accessibility-extra-extra-extra-large testRendersEveryScenarioAtAX5 \
        || FAILED=1
fi

say ""
TOTAL="$(find "$OUT" -name '*.png' | wc -l | tr -d ' ')"
say "$TOTAL renders in $OUT"

if [[ $FAILED -ne 0 ]]; then
    say "RENDER PASS FAILED — at least one catalogued state could not be reached or captured."
    exit 1
fi
say "render pass complete."
