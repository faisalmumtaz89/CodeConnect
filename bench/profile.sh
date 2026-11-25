#!/usr/bin/env bash
# Deep memory & allocation profiler for `ccd` on Linux:
#   - Builds a debug-info-preserving gnu release binary in a separate target-dir
#   - Runs `heaptrack` under concurrent + burst + large_payloads load
#   - Runs `valgrind --tool=massif` under idle + concurrent_10 load
#   - Captures `size` ELF breakdown and `cargo tree -d`
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="${1:-$REPO_ROOT/bench/out/profile}"
mkdir -p "$OUT_DIR"

PROF_TARGET_DIR="$REPO_ROOT/bench/out/prof-target"
MOCKS_DIR="$REPO_ROOT/bench/mocks/target/release"
MOCK_CLAUDE="$REPO_ROOT/bench/bin/mock-claude"

echo "[profile] Building ccd with debug symbols (release + line-tables, gnu)..."
CARGO_PROFILE_RELEASE_DEBUG=line-tables-only \
CARGO_PROFILE_RELEASE_STRIP=false \
CARGO_TARGET_DIR="$PROF_TARGET_DIR" \
  cargo build --release --locked --manifest-path "$REPO_ROOT/mac/Cargo.toml" --bin ccd --bin codeconnect

CCD_PROF="$PROF_TARGET_DIR/release/ccd"

run_workload() {
  local wrapper="$1" label="$2" workload="$3"
  local tmpdir home workdir port pid
  tmpdir="$(mktemp -d /tmp/cc-prof.XXXXXX)"
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

  if [ "$wrapper" = "heaptrack" ]; then
    CODECONNECT_HOME="$home" CODECONNECT_LOG="warn" SHELL="/bin/sh" \
      heaptrack -o "$OUT_DIR/heaptrack_${label}" "$CCD_PROF" > "$OUT_DIR/ccd_${label}.log" 2>&1 &
    pid=$!
  elif [ "$wrapper" = "massif" ]; then
    CODECONNECT_HOME="$home" CODECONNECT_LOG="warn" SHELL="/bin/sh" \
      valgrind --tool=massif --pages-as-heap=no --massif-out-file="$OUT_DIR/massif_${label}.out" \
      "$CCD_PROF" > "$OUT_DIR/ccd_${label}.log" 2>&1 &
    pid=$!
  else
    echo "Unknown wrapper: $wrapper" >&2
    exit 1
  fi

  "$MOCKS_DIR/mock-client" wait-ready --ws "ws://127.0.0.1:$port" --sock "$home/ccd.sock" --timeout-secs 45 \
    > "$OUT_DIR/${label}_ready.json"

  case "$workload" in
    mixed)
      "$MOCKS_DIR/mock-client" sessions --ws "ws://127.0.0.1:$port" --sock "$home/ccd.sock" \
        --claude 5 --codex 5 --workdir "$workdir" --hold-secs 5 --stop-after \
        > "$OUT_DIR/${label}_sessions.json"
      "$MOCKS_DIR/mock-client" burst --ws "ws://127.0.0.1:$port" --sock "$home/ccd.sock" \
        --claude 3 --codex 3 --events-per-session 50 --approvals-per-session 5 --interval-us 500 \
        --workdir "$workdir" > "$OUT_DIR/${label}_burst.json"
      "$MOCKS_DIR/mock-client" large --ws "ws://127.0.0.1:$port" --sock "$home/ccd.sock" \
        --events 60 --payload-bytes 65536 --diff-iters 3 \
        --workdir "$workdir" > "$OUT_DIR/${label}_large.json"
      ;;
    idle_and_10)
      sleep 5
      "$MOCKS_DIR/mock-client" sessions --ws "ws://127.0.0.1:$port" --sock "$home/ccd.sock" \
        --claude 5 --codex 5 --workdir "$workdir" --hold-secs 5 --stop-after \
        > "$OUT_DIR/${label}_sessions.json"
      ;;
  esac

  kill -TERM "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  rm -rf "$tmpdir"
}

if command -v heaptrack >/dev/null 2>&1; then
  echo "[profile] Running heaptrack (mixed workload)..."
  run_workload heaptrack mixed mixed
  HT_FILE="$(ls -t "$OUT_DIR"/heaptrack_mixed*.zst "$OUT_DIR"/heaptrack_mixed*.gz 2>/dev/null | head -n 1 || true)"
  if [ -n "$HT_FILE" ]; then
    heaptrack_print --print-peaks yes --print-allocators yes --print-leaks yes --peak-limit 20 "$HT_FILE" \
      > "$OUT_DIR/heaptrack_mixed_summary.txt" 2>&1 || true
    echo "[profile] Wrote $OUT_DIR/heaptrack_mixed_summary.txt"
  fi
fi

if command -v valgrind >/dev/null 2>&1; then
  echo "[profile] Running valgrind massif (idle + 10 sessions)..."
  run_workload massif idle10 idle_and_10
  if [ -f "$OUT_DIR/massif_idle10.out" ]; then
    ms_print "$OUT_DIR/massif_idle10.out" > "$OUT_DIR/massif_idle10_summary.txt" 2>&1 || true
    echo "[profile] Wrote $OUT_DIR/massif_idle10_summary.txt"
  fi
fi

echo "[profile] Capturing cargo tree -d and binary section sizes..."
cargo tree --manifest-path "$REPO_ROOT/mac/Cargo.toml" -d > "$OUT_DIR/cargo_tree_dups.txt" 2>&1 || true
cargo tree --manifest-path "$REPO_ROOT/mac/Cargo.toml" -p ccd --edges normal > "$OUT_DIR/cargo_tree_ccd.txt" 2>&1 || true
size "$CCD_PROF" > "$OUT_DIR/elf_size.txt" 2>&1 || true

echo "[profile] Complete. Reports in $OUT_DIR"
