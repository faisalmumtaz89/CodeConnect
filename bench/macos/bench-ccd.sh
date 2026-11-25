#!/usr/bin/env bash
# Offline benchmark harness for `ccd` on macOS using native Darwin tools:
#   footprint, vmmap --summary, leaks, lsof, ps
#
# Usage:
#   ./bench/macos/bench-ccd.sh [--out DIR] [--idle-secs 300] [--runs 3]
set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
  echo "Error: bench-ccd.sh must be run on macOS (Darwin)." >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT_DIR="$REPO_ROOT/bench/out/macos-ccd-$(date -u +%Y%m%dT%H%M%SZ)"
IDLE_SECS=300
RUNS=3

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT_DIR="$2"; shift 2 ;;
    --idle-secs) IDLE_SECS="$2"; shift 2 ;;
    --runs) RUNS="$2"; shift 2 ;;
    *) echo "Unknown arg: $1" >&2; exit 1 ;;
  esac
done

mkdir -p "$OUT_DIR"

echo "[macos-bench] Building release ccd & codeconnect..."
cargo build --release --locked --manifest-path "$REPO_ROOT/mac/Cargo.toml" --bin ccd --bin codeconnect

echo "[macos-bench] Building offline benchmark mocks..."
cargo build --release --manifest-path "$REPO_ROOT/bench/mocks/Cargo.toml"

CCD_BIN="$REPO_ROOT/mac/target/release/ccd"
MOCKS_DIR="$REPO_ROOT/bench/mocks/target/release"
MOCK_CLAUDE="$REPO_ROOT/bench/bin/mock-claude"
chmod +x "$MOCK_CLAUDE"

snapshot_darwin() {
  local pid="$1" prefix="$2"
  ps -o pid,rss,vsz,%cpu,wq,thcount,etime -p "$pid" > "${prefix}.ps.txt" 2>&1 || true
  footprint -p "$pid" > "${prefix}.footprint.txt" 2>&1 || true
  footprint -p "$pid" -j "${prefix}.footprint.json" >/dev/null 2>&1 || true
  vmmap --summary "$pid" > "${prefix}.vmmap.txt" 2>&1 || true
  leaks "$pid" > "${prefix}.leaks.txt" 2>&1 || true
  lsof -p "$pid" > "${prefix}.lsof.txt" 2>&1 || true
}

run_one() {
  local scenario="$1" run_idx="$2"
  local tmpdir port home workdir ccd_pid
  tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/cc-mac-bench.XXXXXX")"
  home="$tmpdir/home"
  workdir="$tmpdir/repo"
  mkdir -p -m 0700 "$home" "$workdir"

  port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"

  cat > "$home/config.json" <<EOF
{
  "ws_bind": "127.0.0.1",
  "ws_port": $port,
  "ws_loopback": false,
  "push_enabled": true,
  "tls": false,
  "tailscale_bin": "/nonexistent/tailscale",
  "claude_bin": "$MOCK_CLAUDE",
  "agent_host_bin": "$MOCKS_DIR/mock-claude-host",
  "codex_bin": "$MOCKS_DIR/mock-codex"
}
EOF

  (
    cd "$workdir"
    git init -q
    python3 -c '
for idx in range(4):
    with open(f"mod_{idx}.rs","w") as f:
        for i in range(300): f.write(f"pub fn f_{idx}_{i}(x: u64) -> u64 {{ x + {i} }}\n")
'
    git -c user.name=Bench -c user.email=bench@local add .
    git -c user.name=Bench -c user.email=bench@local commit -q -m init
    python3 -c '
for idx in range(4):
    with open(f"mod_{idx}.rs","w") as f:
        for i in range(300): f.write(f"pub fn f_{idx}_{i}(x: u64, y: u64) -> u64 {{ x + y + {i} }}\n")
'
  )

  CODECONNECT_HOME="$home" CODECONNECT_LOG="warn" SHELL="/bin/sh" \
    "$CCD_BIN" > "$tmpdir/ccd.log" 2>&1 &
  ccd_pid=$!

  trap 'kill -TERM "$ccd_pid" 2>/dev/null || true; rm -rf "$tmpdir"' RETURN

  "$MOCKS_DIR/mock-client" wait-ready --ws "ws://127.0.0.1:$port" --sock "$home/ccd.sock" \
    > "$OUT_DIR/${scenario}_run${run_idx}.ready.json"

  case "$scenario" in
    cold_idle)
      sleep "$IDLE_SECS"
      ;;
    idle_client)
      "$MOCKS_DIR/mock-client" idle-client --ws "ws://127.0.0.1:$port" --sock "$home/ccd.sock" \
        --duration-secs 60 > "$OUT_DIR/${scenario}_run${run_idx}.client.json"
      ;;
    concurrent_10)
      "$MOCKS_DIR/mock-client" sessions --ws "ws://127.0.0.1:$port" --sock "$home/ccd.sock" \
        --claude 5 --codex 5 --workdir "$workdir" --hold-secs 15 \
        > "$OUT_DIR/${scenario}_run${run_idx}.client.json"
      ;;
    concurrent_50)
      "$MOCKS_DIR/mock-client" sessions --ws "ws://127.0.0.1:$port" --sock "$home/ccd.sock" \
        --claude 25 --codex 25 --workdir "$workdir" --hold-secs 15 \
        > "$OUT_DIR/${scenario}_run${run_idx}.client.json"
      ;;
    burst)
      "$MOCKS_DIR/mock-client" burst --ws "ws://127.0.0.1:$port" --sock "$home/ccd.sock" \
        --claude 5 --codex 5 --events-per-session 80 --approvals-per-session 10 \
        --workdir "$workdir" > "$OUT_DIR/${scenario}_run${run_idx}.client.json"
      ;;
    large_payloads)
      "$MOCKS_DIR/mock-client" large --ws "ws://127.0.0.1:$port" --sock "$home/ccd.sock" \
        --events 150 --payload-bytes 65536 --diff-iters 10 \
        --workdir "$workdir" > "$OUT_DIR/${scenario}_run${run_idx}.client.json"
      ;;
  esac

  snapshot_darwin "$ccd_pid" "$OUT_DIR/${scenario}_run${run_idx}"
  kill -TERM "$ccd_pid" 2>/dev/null || true
  wait "$ccd_pid" 2>/dev/null || true
  rm -rf "$tmpdir"
  trap - RETURN
}

{
  echo "Commit: $(git -C "$REPO_ROOT" rev-parse HEAD)"
  echo "Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  sw_vers
  sysctl -n machdep.cpu.brand_string hw.ncpu hw.memsize
  rustc --version
  ls -la "$CCD_BIN" "$REPO_ROOT/mac/target/release/codeconnect"
} > "$OUT_DIR/env.txt"

for scenario in cold_idle idle_client concurrent_10 concurrent_50 burst large_payloads; do
  for run_idx in $(seq 1 "$RUNS"); do
    echo "[macos-bench] scenario=$scenario run=$run_idx/$RUNS"
    run_one "$scenario" "$run_idx"
  done
done

echo "[macos-bench] Done. Results in $OUT_DIR"
