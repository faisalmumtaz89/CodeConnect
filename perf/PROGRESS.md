# CodeConnect Performance & Memory Optimization Progress

## Branch & Isolation
- Branch: `perf/baseline-and-optimizations` (cut from `main` HEAD `955d8ec`)
- Never touching `main`, release configs, tags, or signing keys.

## Current Phase
- **Phase 1 — Baseline**: **COMPLETE** (`3aaf8fe`)
- **Phase 2 — Analysis**: **COMPLETE** (`perf/FINDINGS.md` written with 16 ranked findings + 4 proposals)
- **Phase 3 — Optimize**: **IN PROGRESS** (executing Tier 1 -> Tier 2 -> Tier 3 optimizations, one logical commit per change, verified with before/after benchmarks)

## Completed
- [x] Inspected repository structure (`mac/` Rust workspace: `ccd`, `codeconnect`, `protocol`, `push-core`, `push-relay`, `demo-daemon`; `mac/agent-host` Bun/TS helper; `macos/` SwiftUI app + `CodeConnectKit`).
- [x] Created branch `perf/baseline-and-optimizations` from `main` (`955d8ec`).
- [x] Installed profiling & build toolchain (`musl-tools`, `heaptrack`, `valgrind`, `pidstat`, `strace`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`).
- [x] Ran baseline workspace test suite (`1,604 passed, 1 failed, 60 ignored`; pre-existing container FD-ceiling failure in `protocol::fdlimit::tests::a_higher_soft_limit_is_left_as_it_is` documented and left untouched).
- [x] Built reproducible offline benchmark harness under `bench/` (`bench/mocks/{mock-claude-host,mock-codex,mock-client}`, `bench/bin/mock-claude`, `bench/driver.py`, `bench/run.sh`, `bench/profile.sh`) and native macOS scripts under `bench/macos/`.
- [x] Collected full Phase 1 baseline on `x86_64-unknown-linux-musl` (3 runs × all 8 scenarios including 3 × 5-min `cold_idle`, process-wide thread wakeup measurements, 500 s soak test with session churn), `x86_64-unknown-linux-gnu` comparison (3 runs), and `heaptrack` + `valgrind --tool=massif` profiles.
- [x] Wrote and committed `perf/BASELINE.md`.
- [x] Completed Phase 2 analysis and wrote `perf/FINDINGS.md` (F1–F16 + P1–P4).

## Blockers Encountered & Workarounds
1. **Linux `/tmp` ownership in `cargo test`**: `Store::open` hardens its state dir to `0700`, which fails on root-owned `/tmp`. Workaround: run tests with `TMPDIR=/tmp/cc-test-tmp` (matching `.github/workflows/ci.yml`).
2. **10-minute command execution ceiling**: Non-daemon commands time out around 600 s. Workaround: added `--append` and incremental result flushing to `bench/driver.py` / `bench/run.sh` so 3 × 5-min `cold_idle` runs execute in separate 5-min batches and aggregate into the exact same `results.json`/`results.md`, and bounded the baseline soak run to 500 s (with `--soak 7200` supported for multi-hour runs).
3. **Main-thread-only `/proc/<pid>/status` wakeup counters**: Initially showed `0.0/s` during `cold_idle` because Tokio workers run on child threads. Fixed `bench/driver.py` to sum monotonic per-TID context switches across `/proc/<pid>/task/*/status`.

## Decisions Made
1. **Standalone benchmark crate (`bench/mocks/`)**: Kept outside `mac/Cargo.toml` so harness dependencies never alter `mac/Cargo.lock` or product crate feature resolution.
2. **Compiled Rust mocks**: Single-threaded Rust binaries (~1 MiB each) keep 50-session benchmarks distortion-free on a 2-vCPU VM while speaking real `protocol::host`, Codex app-server WS-over-UDS, `protocol::ws`, and `protocol::ipc`.
3. **Shipped `musl` as primary Linux metric + `gnu` for allocator comparison & profiling**: Matches `.github/workflows/linux-build.yml` static musl artifacts while allowing `heaptrack`/`massif` interposition and revealing glibc `ptmalloc2` arena blowup (34.6 MiB vs 8.8 MiB on `large_payloads`).
