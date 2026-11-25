#!/usr/bin/env bash
# One-command reproducible offline benchmark harness for CodeConnect (Linux).
#
# Usage:
#   ./bench/run.sh                       # Full benchmark (3 runs, 5-min cold_idle)
#   ./bench/run.sh --quick               # Fast verification (3 runs, 15s cold_idle)
#   ./bench/run.sh --soak 7200           # Run 2-hour soak test
#   ./bench/run.sh --target gnu          # Measure glibc build instead of static musl
#   ./bench/run.sh --scenarios burst,large_payloads --runs 3
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="musl"
MODE="full"
RUNS=3
IDLE_SECS=300
IDLE_CLIENT_SECS=60
SOAK_SECS=0
SCENARIOS="cold_idle,idle_client,concurrent_1,concurrent_10,concurrent_50,burst,large_payloads,reconnect_churn"
OUT_DIR="$REPO_ROOT/bench/out/latest"
SKIP_BUILD=0
APPEND_FLAG=""

while [ $# -gt 0 ]; do
  case "$1" in
    --quick)
      MODE="quick"
      IDLE_SECS=15
      IDLE_CLIENT_SECS=15
      shift
      ;;
    --soak)
      SOAK_SECS="${2:?--soak requires duration in seconds}"
      SCENARIOS="soak"
      shift 2
      ;;
    --with-soak)
      SOAK_SECS="${2:?--with-soak requires duration in seconds}"
      SCENARIOS="${SCENARIOS},soak"
      shift 2
      ;;
    --target)
      TARGET="${2:?--target requires musl or gnu}"
      shift 2
      ;;
    --runs)
      RUNS="${2:?--runs requires count}"
      shift 2
      ;;
    --idle-secs)
      IDLE_SECS="${2:?--idle-secs requires seconds}"
      shift 2
      ;;
    --scenarios)
      SCENARIOS="${2:?--scenarios requires comma-separated list}"
      shift 2
      ;;
    --out)
      OUT_DIR="${2:?--out requires directory}"
      shift 2
      ;;
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    --append)
      APPEND_FLAG="--append"
      shift
      ;;
    -h|--help)
      sed -n '2,10p' "$0"
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

ARCH="$(uname -m)"
if [ "$TARGET" = "musl" ]; then
  RUST_TARGET="${ARCH}-unknown-linux-musl"
else
  RUST_TARGET="${ARCH}-unknown-linux-gnu"
fi

CCD_BIN="$REPO_ROOT/mac/target/${RUST_TARGET}/release/ccd"
MOCKS_DIR="$REPO_ROOT/bench/mocks/target/release"

if [ "$SKIP_BUILD" -eq 0 ]; then
  echo "[bench] Building ccd & codeconnect (${RUST_TARGET}, release)..."
  cargo build --release --locked --manifest-path "$REPO_ROOT/mac/Cargo.toml" \
    --target "$RUST_TARGET" --bin ccd --bin codeconnect

  echo "[bench] Building offline benchmark mocks..."
  cargo build --release --manifest-path "$REPO_ROOT/bench/mocks/Cargo.toml"
fi

chmod +x "$REPO_ROOT/bench/bin/mock-claude" "$REPO_ROOT/bench/driver.py"

echo "[bench] Running scenarios=${SCENARIOS} runs=${RUNS} target=${RUST_TARGET} out=${OUT_DIR}"
python3 "$REPO_ROOT/bench/driver.py" \
  --binary "$CCD_BIN" \
  --mocks-dir "$MOCKS_DIR" \
  --mock-claude "$REPO_ROOT/bench/bin/mock-claude" \
  --out "$OUT_DIR" \
  --scenarios "$SCENARIOS" \
  --runs "$RUNS" \
  --idle-secs "$IDLE_SECS" \
  --idle-client-secs "$IDLE_CLIENT_SECS" \
  --soak-secs "${SOAK_SECS:-600}" \
  $APPEND_FLAG
