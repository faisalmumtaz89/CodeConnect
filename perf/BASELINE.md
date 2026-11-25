# CodeConnect Baseline Performance & Memory Report

> **Phase 1 deliverable** — measured on unmodified `main` HEAD (`955d8eca6109acb0ccd4c6890341d8e83eb4cd72`) before any product code optimizations.

## 1. Environment

| Property | Value |
|---|---|
| **Commit SHA** | `955d8eca6109acb0ccd4c6890341d8e83eb4cd72` |
| **Branch** | `perf/baseline-and-optimizations` |
| **OS / Kernel** | Linux `6.18.57` (`x86_64`, Ubuntu 24.04 userland, glibc 2.39) |
| **CPU** | 2 vCPU — Intel(R) Xeon(R) CPU @ 2.30GHz |
| **RAM** | 7.77 GiB |
| **FD limits** | `ulimit -Sn 1024`, `ulimit -Hn 4096` (container ceiling) |
| **Rust toolchain** | `rustc 1.100.0 (0ef1bc386 2026-11-09)`, `cargo 1.100.0` |
| **Primary Linux target** | `x86_64-unknown-linux-musl` (static-pie release binary — exact artifact shipped by `.github/workflows/linux-build.yml`) |
| **Comparison / profiling target** | `x86_64-unknown-linux-gnu` (dynamic glibc release binary; required for `heaptrack` / `valgrind --tool=massif` `LD_PRELOAD` interposition) |
| **Release profile (`mac/Cargo.toml`)** | `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true` |
| **macOS toolchain** | Not available on Linux host; macOS GUI/Kit analyzed by code review (`perf/FINDINGS.md`) and native macOS scripts provided under `bench/macos/`. |

## 2. Pre-existing Test Status on `main`

Command:
```bash
mkdir -p /tmp/cc-test-tmp
TMPDIR=/tmp/cc-test-tmp cargo test --manifest-path mac/Cargo.toml --workspace --no-fail-fast
```
*(Note: `.github/workflows/ci.yml` sets `TMPDIR` to a user-owned directory because Linux `/tmp` is `root:root 1777` and `Store::open` enforces `0700` on its state dir.)*

| Crate | Passed | Failed | Ignored | Notes |
|---|---:|---:|---:|---|
| `ccd` (unit + integration) | 958 | 0 | 54 | All pass (`mail_at_start`, `second_daemon` included) |
| `codeconnect` (unit + integration) | 120 | 0 | 1 | All pass |
| `demo-daemon` | 32 | 0 | 0 | All pass |
| `push-core` | 16 | 0 | 0 | All pass |
| `push-relay` | 239 | 0 | 0 | All pass |
| `protocol` | 239 | **1** | 5 | **Pre-existing container failure:** `fdlimit::tests::a_higher_soft_limit_is_left_as_it_is` panics with `hard limit 4096: nothing above 10241 to raise to` (`protocol/src/fdlimit.rs:64`) because the cloud VM hard FD ceiling is `4096`. Left untouched per decision rules. |
| **Total** | **1,604** | **1** | **60** | |

## 3. Benchmark Method

All benchmarks are completely offline, deterministic, and runnable via one command (`./bench/run.sh`):
- **Real daemon (`ccd`)** launched in an isolated `CODECONNECT_HOME` with a real SQLite WAL store, real `ccd.sock` Unix IPC listener, real WebSocket listener on `127.0.0.1:<ephemeral>`, and real git working tree (~265 KiB dirty diff).
- **Compiled single-threaded Rust mocks (`bench/mocks/`)** keep mock overhead negligible (~1–2 MiB each) while speaking the exact real wire protocols:
  - `mock-claude-host`: real `protocol::host` JSONL over `<run>/host.sock` + `host.json`
  - `mock-codex`: real `codex app-server --listen unix://<run>/as.sock` JSON-RPC over WebSocket-on-UDS
  - `mock-client`: real `protocol::ws` client + `protocol::ipc` client (pairing, auth, `sessions`, `subscribe`, `history`, `get_diff`, `answer`, `start_session`, `stop_run`)
- **Sampling (`bench/driver.py`)**: 500 ms sampler reading `/proc/<pid>/smaps_rollup` (RSS, PSS, USS), `/proc/<pid>/smaps` (`[heap]`, anon mmap, file-backed mappings), `/proc/<pid>/status` (`VmHWM`, `Threads`), `/proc/<pid>/task/*/status` (monotonic process-wide voluntary + nonvoluntary context switches across all threads), `/proc/<pid>/stat` (CPU ticks), and `/proc/<pid>/fd` (open FDs and sockets).
- **Statistics**: 3 runs per scenario (except 1 long soak run); reported as `median (min–max, CV%)`, with `p95` included in JSON and latency tables.
- **Note on wakeup accounting**: The initial 3-run `baseline-musl` run read `/proc/<pid>/status` (main thread only), which missed Tokio worker & blocking-pool wakeups. We upgraded `bench/driver.py` to sum `/proc/<pid>/task/*/status` monotonically across all threads and collected dedicated process-wide wakeup readings (`baseline-wakeups` on `musl`, plus full 3-run `baseline-gnu` and `baseline-soak` on `musl`). Both are recorded below.

## 4. Baseline Results — `x86_64-unknown-linux-musl` (Shipped Static Binary)

### 4.1 Memory, CPU, Wakeups, Threads, FDs, Startup (3 runs each; `cold_idle` = 3 × 5 min)

| Scenario | Runs | RSS (MiB) | PSS (MiB) | USS (MiB) | Peak RSS (MiB) | CPU (%) | Wakeups/s (all threads)* | Threads (peak) | FDs (Socks) | Startup (ms) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `cold_idle` (5 min) | 3 | **6.30** (6.27–6.36, 0.7%) | 6.29 (6.26–6.35) | 6.29 (6.26–6.35) | **10.78** (10.73–10.84, 0.5%) | 0.01 (0.00–0.01) | **0.50** | 6 (7) | 23 (5) | 114.5 (102.5–124.8) |
| `idle_client` (60 s) | 3 | **6.26** (6.26–6.31, 0.5%) | 6.26 (6.25–6.30) | 6.26 (6.25–6.30) | **10.89** (10.84–10.90, 0.3%) | 0.00 (0.00–0.00) | **0.67** | 5 (5) | 24 (6) | 109.3 (98.5–133.6) |
| `concurrent_1` | 3 | **6.46** (6.46–6.46, 0.0%) | 6.46 (6.46–6.46) | 6.46 (6.46–6.46) | **10.58** (10.46–10.70, 1.2%) | 0.25 (0.17–0.25, 21.6%) | ~23.7 | 3 (8) | 26 (7) | 115.5 (114.4–135.2) |
| `concurrent_10` | 3 | **7.07** (7.06–7.24, 1.4%) | 7.06 (7.05–7.23) | 7.06 (7.05–7.23) | **10.64** (10.63–10.75, 0.7%) | 1.35 (1.20–1.35, 6.9%) | **138.40** | 3 (14) | 44 (16) | 99.5 (99.1–115.2) |
| `concurrent_50` | 3 | **9.43** (9.27–9.56, 1.5%) | 9.43 (9.27–9.56) | 9.43 (9.27–9.56) | **10.76** (10.63–10.97, 1.6%) | **6.43** (6.13–6.47, 2.9%) | **473.42** | **30 (40)** | 124 (56) | 100.3 (99.3–100.8) |
| `burst` (10 sess, 1540 ev, 100 appr) | 3 | **9.21** (9.15–9.38, 1.2%) | 9.21 (9.15–9.37) | 9.21 (9.15–9.37) | **11.74** (11.61–12.38, 3.5%) | **72.73** (69.54–74.51, 3.5%) | ~8,856 | **22 (23)** | 24 (6) | 102.4 (102.3–110.0) |
| `large_payloads` (300×64 KiB + history + diffs) | 3 | **8.84** (6.63–10.71, 23.4%) | 8.84 (6.63–10.70) | 8.84 (6.63–10.70) | **18.45** (18.39–18.54, 0.4%) | **75.26** (71.38–76.45, 3.6%) | ~2,869 | 8 (9) | 25 (6) | 99.1 (91.2–114.3) |
| `reconnect_churn` (60 cycles) | 3 | **6.81** (6.71–6.84, 0.9%) | 6.80 (6.71–6.83) | 6.80 (6.71–6.83) | **10.75** (10.56–10.77, 1.1%) | **61.34** (60.04–63.03, 2.4%) | ~11,248 | 11 (12) | 23 (5) | 100.8 (99.6–102.7) |
| `soak` (500 s, 10 steady + 24 churn) | 1 | **10.72** (tail median) | 10.72 | 10.72 | **19.03** | 6.91 | **682.16** | 24 (24) | 44 (16) | 91.9 |

*\*Process-wide `/proc/<pid>/task/*/status` wakeup rates: `cold_idle`, `idle_client`, `concurrent_10`, `concurrent_50`, and `soak` measured directly on `musl`; `concurrent_1`, `burst`, `large_payloads`, `reconnect_churn` cross-referenced from the identical `gnu` run (within ~3% of `musl` on overlapping scenarios).*

### 4.2 Heap vs Non-Heap (`smaps` Breakdown, Median MiB)

On `musl`, `mallocng` allocates via anonymous `mmap` rather than `brk`, so Rust/SQLite heap + thread stacks appear under `anon_mmap`:

| Scenario | Total RSS | `[heap]` (`brk`) | Anon `mmap` (heap + stacks) | File-backed (`ccd` binary + DB) |
|---|---:|---:|---:|---:|
| `cold_idle` | 6.30 | 0.02 | 1.57 | 4.54 |
| `idle_client` | 6.26 | 0.02 | 1.52 | 4.53 |
| `concurrent_1` | 6.46 | 0.02 | 1.59 | 4.64 |
| `concurrent_10` | 7.07 | 0.02 | 2.11 | 4.70 |
| `concurrent_50` | 9.43 | 0.04 | **4.29** | 4.84 |
| `burst` | 9.21 | 0.05 | **4.18** | 4.83 |
| `large_payloads` | 8.84 | 0.02 | **6.79** | 4.64 |
| `reconnect_churn` | 6.81 | 0.02 | 1.89 | 4.74 |
| `soak` | 10.72 | 0.03 | **4.23** | 4.86 |

### 4.3 Latency & Throughput Metrics (`musl`, 3 runs)

| Scenario | Metric | Min | Median | p95 | Max | CV% |
|---|---|---:|---:|---:|---:|---:|
| `concurrent_1` | `start_p50_ms` | 104.05 | 104.59 | 106.61 | 106.61 | 1.3% |
| `concurrent_1` | `relay_p50_ms` | 3.50 | 3.85 | 4.65 | 4.65 | 14.8% |
| `concurrent_1` | `sessions_rpc_p50_ms` | 0.71 | **0.92** | 1.72 | 1.72 | 47.9% |
| `concurrent_10` | `start_p50_ms` | 431.40 | 453.82 | 513.58 | 513.58 | 9.1% |
| `concurrent_10` | `relay_p50_ms` / `p95` | 140.34 | **173.63** | 323.99 | 390.43 | 15.2% |
| `concurrent_10` | `sessions_rpc_p50_ms` / `p95` | 2.75 | **4.49** | 6.15 | 6.59 | 26.4% |
| `concurrent_50` | `start_p50_ms` / `p95` | 2037.22 | **2108.28** | 2295.91 | 2464.60 | 2.4% |
| `concurrent_50` | `relay_p50_ms` / `p95` | 948.40 | **952.34** | 1105.84 | 1140.06 | 3.1% |
| `concurrent_50` | `sessions_rpc_p50_ms` / `p95` | 13.77 | **13.94** | **16.99** | 18.47 | 6.8% |
| `burst` | `events_per_sec` | 896.27 | **924.66** | 1003.21 | 1003.21 | 5.9% |
| `burst` | `relay_p50_ms` / `p95` | 733.76 | **810.53** | 957.99 | 1183.46 | 18.7% |
| `burst` | `approval_rtt_p50_ms` / `p95` | 7.14 | **11.18** | **38.04** | 72.50 | 25.2% |
| `burst` | `approvals_answered` | 100 | 100 | 100 | 100 | 0.0% |
| `large_payloads` | `relay_p50_ms` / `p95` | 444.06 | **482.59** | 844.36 | 1115.88 | 12.8% |
| `large_payloads` | `history_p50_ms` / `p95` | 65.34 | **79.38** | **84.88** | 89.39 | 12.8% |
| `large_payloads` | `diff_p50_ms` / `p95` | 22.09 | 22.12 | 25.15 | 25.26 | 5.3% |
| `reconnect_churn` | `cycle_p50_ms` / `p95` | 8.79 | **8.91** | **11.85** | 12.55 | 0.8% |

### 4.4 Soak Growth Analysis (`musl`, 500 s steady load + session churn)

Workload: 5 steady Claude + 5 steady Codex sessions emitting events & approvals every 2 s, plus 1 ephemeral start/burst/stop session every 20 s, client drop+reconnect every 60 s, and `sessions` + `get_machine_stats` + `get_diff` + IPC `list_sessions` every 15 s (total: **22,385 events**, **2,357 approvals answered**, **24 ephemeral churn sessions**, **8 reconnects**, **33 diffs**; duration bounded at 500 s due to the 10-min command execution ceiling, while `./bench/run.sh --soak 7200` is provided for multi-hour runs).

- **Start RSS -> End RSS (post-stop):** `7.70 MiB -> 9.11 MiB` (tail median during active soak: `10.72 MiB`, Peak: `19.03 MiB`)
- **Linear regression growth rate (after 10% warm-up):** **+2.64 MiB/hour** (RSS, PSS, and USS all `+2.64 MiB/hr`)
- **Process-wide wakeups/sec:** **682.16/s**; **Threads:** 16 steady / 24 peak; **FDs:** 44 steady / 54 peak (returns cleanly to 23 on stop — no FD leak).

## 5. Comparison Baseline — `x86_64-unknown-linux-gnu` (glibc `ptmalloc2`)

Measured with `./bench/run.sh --target gnu --quick --runs 3` (15 s idle settle, same active workloads):

| Scenario | RSS (MiB) | PSS (MiB) | USS (MiB) | Peak RSS (MiB) | Anon `mmap` (MiB) | CPU (%) | Wakeups/s | Peak Threads |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `cold_idle` | 7.15 | 6.37 | 6.27 | 11.40 | 0.75 | 0.00 | 0.00 | 6 |
| `idle_client` | 7.17 | 6.39 | 6.29 | 11.23 | 0.80 | 0.00 | 0.57 | 6 |
| `concurrent_1` | 7.70 | 6.89 | 6.80 | 11.28 | 1.06 | 0.17 | 23.74 | 8 |
| `concurrent_10` | 8.55 | 7.71 | 7.64 | 11.28 | 1.85 | 1.27 | 137.82 | 11 |
| `concurrent_50` | 11.77 | 10.90 | 10.86 | 11.94 | 5.03 | 5.94 | 455.39 | **61** |
| `burst` | **19.79** | 19.00 | 18.90 | 19.79 | **12.77** | 66.32 | **8,856.50** | 22 |
| `large_payloads` | **34.60** | 33.81 | 33.72 | **34.60** | **27.98** | 60.80 | **2,868.51** | 10 |
| `reconnect_churn` | 10.68 | 9.89 | 9.79 | 11.39 | 4.05 | 58.29 | **11,247.56** | 22 |

**Key allocator finding:** On glibc (`ptmalloc2`), `burst` retains **19.8 MiB** (2.1× `musl`) and `large_payloads` retains **34.6 MiB** (3.9× `musl`, peaking at 37.3 MiB!) due to per-thread `ptmalloc2` arenas (up to 61 threads!) and unreturned heap chunks. On static `musl` (`mallocng`), `large_payloads` still peaks at **18.45 MiB** and runs ~18% slower on CPU (`75.3%` vs `60.8%`, `history_p50` `79.4 ms` vs `44.9 ms`).

## 6. Allocation & Heap Profiles (`heaptrack` + `valgrind --tool=massif`)

Captured via `./bench/profile.sh bench/out/baseline-profile` on a debug-symbol-preserving `gnu` release build (mixed workload: 10 sessions + burst + large payloads for `heaptrack`; idle + 10 sessions for `massif`):

- **Total runtime (`heaptrack` mixed):** `74.46 s`
- **Total allocations:** `359,153` (`4,823 allocs/sec`), `30,865` temporary (`414/sec`)
- **Peak heap consumption:** `31.22 MiB` (`massif` snapshot 8: `31,176,600 B`, of which `30,143,400 B` = **96.69%** is a single call!)

### Top Peak Heap Consumers (`heaptrack` + `massif`)
1. **`std::fs::read(&exe)` (`mac/ccd/src/main.rs:114`) — 96.69% of startup peak heap** (30.14 MiB on debug binary; **6.0 MiB** on stripped `musl` binary -> causes the `10.8 MiB` Peak RSS even in `cold_idle`!).
2. **`sqlite3MemMalloc` (`mac/ccd/src/store.rs:5698, 1283, 1292, 5875`) — 613–713 KiB at startup**:
   - `240 KiB` in `setupLookaside` alone (48 KiB × 5 eager connections: 1 writer + 4 readers).
   - `340 KiB` in `pcache1Alloc` (default `-2000` KiB cap per connection = up to **10 MiB** page cache across 5 connections under sustained DB growth!).
3. **`broadcast::channel::<Event>(1024)` (`mac/ccd/src/state.rs:56`) — 245.76 KiB permanently allocated at startup** (`1024 × ~240 B` slots), plus pins up to 1024 full `Event` payloads in RAM (up to 64–512 KiB each during `large_payloads`).

### Top Allocation-Rate Call Sites (`heaptrack`)
1. **`protocol::proc_identity::read_stat` (`mac/protocol/src/proc_identity.rs:258`) — 88,886+ allocations (>25% of ALL daemon allocations!)**:
   - Called via `codex_host.rs:777` (`TOOL_SCAN` every 1 s per active Codex session, **synchronously on the Tokio async worker thread**!).
   - `fields.split_whitespace().map(str::to_owned).collect::<Vec<String>>()` allocates ~50 heap `String`s per `/proc/<pid>/stat` entry for EVERY PID on the system, even though only 4 numeric/char fields are ever read.
2. **SQLite statement preparation (`sqlite3MemMalloc`, 23,467 calls)**: Every query in `store.rs` calls `conn.prepare(...)` rather than `prepare_cached(...)`, reparsing SQL and allocating VDBE bytecode on every call.

## 7. Binary Size & Dependency Count

| Target / Artifact | Size (MiB) | Size (bytes) | ELF `.text` / `.data` / `.bss` |
|---|---:|---:|---|
| `x86_64-unknown-linux-musl/ccd` (stripped static-pie) | **5.969 MiB** | 6,259,296 | — |
| `x86_64-unknown-linux-musl/codeconnect` (stripped static-pie) | **0.997 MiB** | 1,045,312 | — |
| `x86_64-unknown-linux-gnu/ccd` (stripped dynamic) | **5.836 MiB** | 6,119,864 | `5,951,302` / `167,568` / `5,416` (`ccd` prof build) |
| `x86_64-unknown-linux-gnu/codeconnect` (stripped dynamic) | **0.902 MiB** | 945,424 | — |

- **Unique crate counts (`cargo tree --prefix=none`)**:
  - `ccd`: **93** unique crates
  - `codeconnect`: **33** unique crates
  - `protocol`: **27** unique crates
- **Workspace duplicate crate groups (`cargo tree -d`)**: **2** (`hashbrown` `0.14.5` via `rusqlite`/`hashlink` vs `0.17.1` via `h2`/`indexmap`; `syn` `2.0.119` vs `3.0.3` proc-macro only).
