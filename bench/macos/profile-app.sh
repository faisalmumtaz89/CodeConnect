#!/usr/bin/env bash
# Native macOS profiler for `CodeConnect.app` (SwiftUI GUI) or `ccd` (daemon).
# Captures: footprint, vmmap --summary, leaks, xctrace (Allocations & Time Profiler),
# lsof, and optionally powermetrics (wakeups & energy impact).
#
# Usage:
#   ./bench/macos/profile-app.sh --name CodeConnect --duration 60 --out bench/out/macos-gui
#   sudo ./bench/macos/profile-app.sh --name ccd --duration 60 --powermetrics --out bench/out/macos-ccd
set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
  echo "Error: profile-app.sh must be run on macOS (Darwin)." >&2
  exit 1
fi

PROC_NAME="CodeConnect"
PID=""
DURATION=60
OUT_DIR="bench/out/macos-profile-$(date -u +%Y%m%dT%H%M%SZ)"
DO_POWERMETRICS=0
DO_XCTRACE=1

while [ $# -gt 0 ]; do
  case "$1" in
    --name) PROC_NAME="$2"; shift 2 ;;
    --pid) PID="$2"; shift 2 ;;
    --duration) DURATION="$2"; shift 2 ;;
    --out) OUT_DIR="$2"; shift 2 ;;
    --powermetrics) DO_POWERMETRICS=1; shift ;;
    --no-xctrace) DO_XCTRACE=0; shift ;;
    *) echo "Unknown arg: $1" >&2; exit 1 ;;
  esac
done

if [ -z "$PID" ]; then
  PID="$(pgrep -x "$PROC_NAME" | head -n 1 || true)"
fi
if [ -z "$PID" ]; then
  echo "Error: process '$PROC_NAME' not found. Pass --pid <PID> or launch it first." >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
echo "[macos-profile] Profiling PID=$PID ($PROC_NAME) for ${DURATION}s -> $OUT_DIR"

{
  echo "Timestamp: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "PID: $PID"
  echo "Process: $PROC_NAME"
  sw_vers
  sysctl -n machdep.cpu.brand_string hw.ncpu hw.memsize
} > "$OUT_DIR/meta.txt"

echo "[macos-profile] Capturing BEFORE memory snapshots (footprint, vmmap, leaks, lsof)..."
footprint -p "$PID" > "$OUT_DIR/before.footprint.txt" 2>&1 || true
footprint -p "$PID" -j "$OUT_DIR/before.footprint.json" >/dev/null 2>&1 || true
vmmap --summary "$PID" > "$OUT_DIR/before.vmmap.txt" 2>&1 || true
leaks "$PID" > "$OUT_DIR/before.leaks.txt" 2>&1 || true
lsof -p "$PID" > "$OUT_DIR/before.lsof.txt" 2>&1 || true

PM_PID=""
if [ "$DO_POWERMETRICS" -eq 1 ]; then
  if [ "$(id -u)" -ne 0 ]; then
    echo "[macos-profile] Warning: --powermetrics requires sudo; skipping powermetrics." >&2
  else
    echo "[macos-profile] Starting powermetrics (wakeups + energy)..."
    powermetrics --samplers tasks --show-process-energy --show-process-wakeups \
      -i 5000 -n "$(( (DURATION + 4) / 5 ))" \
      > "$OUT_DIR/powermetrics.txt" 2>&1 &
    PM_PID=$!
  fi
fi

if [ "$DO_XCTRACE" -eq 1 ] && command -v xctrace >/dev/null 2>&1; then
  HALF=$(( DURATION / 2 ))
  [ "$HALF" -lt 10 ] && HALF=10
  echo "[macos-profile] Recording Instruments Time Profiler (${HALF}s)..."
  xctrace record --template "Time Profiler" --attach "$PID" \
    --time-limit "${HALF}s" --output "$OUT_DIR/time_profiler.trace" \
    > "$OUT_DIR/xctrace_time.log" 2>&1 || true

  echo "[macos-profile] Recording Instruments Allocations (${HALF}s)..."
  xctrace record --template "Allocations" --attach "$PID" \
    --time-limit "${HALF}s" --output "$OUT_DIR/allocations.trace" \
    > "$OUT_DIR/xctrace_alloc.log" 2>&1 || true
else
  echo "[macos-profile] Sleeping ${DURATION}s while sampling ps..."
  for _ in $(seq 1 "$DURATION"); do
    ps -o pid,rss,vsz,%cpu,thcount -p "$PID" >> "$OUT_DIR/ps_series.txt" 2>&1 || break
    sleep 1
  done
fi

if [ -n "$PM_PID" ]; then
  wait "$PM_PID" 2>/dev/null || true
fi

echo "[macos-profile] Capturing AFTER memory snapshots (footprint, vmmap, leaks, lsof)..."
footprint -p "$PID" > "$OUT_DIR/after.footprint.txt" 2>&1 || true
footprint -p "$PID" -j "$OUT_DIR/after.footprint.json" >/dev/null 2>&1 || true
vmmap --summary "$PID" > "$OUT_DIR/after.vmmap.txt" 2>&1 || true
leaks "$PID" > "$OUT_DIR/after.leaks.txt" 2>&1 || true
lsof -p "$PID" > "$OUT_DIR/after.lsof.txt" 2>&1 || true

echo "[macos-profile] Complete. Artifacts in $OUT_DIR"
