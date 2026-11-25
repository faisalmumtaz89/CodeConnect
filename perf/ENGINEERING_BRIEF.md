# CodeConnect Performance & Memory Optimization — Engineering Brief

> **Target audience:** CodeConnect engineering team (evaluation, code review, macOS/ARM64 verification, and adoption)
> **Branch:** `perf/baseline-and-optimizations` (cut from `main` @ `955d8ec`; `main` untouched)
> **Scope:** `ccd` (Linux & macOS daemon), `codeconnect` CLI, shared Rust crates (`protocol`, `push-core`), `mac/agent-host`, and `macos/CodeConnect/` SwiftUI app. *(Out of scope: `ios/` app; wire protocol, SQLite schema, and config format are unchanged.)*

---

## 1. Executive Summary

CodeConnect runs continuously beside memory- and CPU-hungry coding agents (Claude Code, Codex). Every megabyte and idle wakeup it uses is taken directly from the user's agents and battery.

Across **13 implemented optimizations** (**F1–F10**, **F12**, and macOS-only **F13–F15**) organized as clean, independent commits on `perf/baseline-and-optimizations`, we cut `ccd`'s memory footprint, idle wakeups, thread count, and RPC/history latency substantially with **zero protocol, schema, config, feature, or security regressions**:

| Dimension (Shipped `x86_64-unknown-linux-musl`, 3-run medians) | Baseline (`955d8ec`) | Optimized (`c227433`) | Improvement |
|---|---:|---:|---:|
| **Cold idle (3 × 5 min) — RSS / Peak RSS** | 6.30 / 10.78 MiB | **5.42 / 5.48 MiB** | **-14.0% / -49.2%** |
| **Cold idle — Non-code RAM (`RssAnon`)** | 1.76 MiB | **0.90 MiB** | **-49.0%** *(now < 1 MiB)* |
| **Cold idle — Wakeups/sec / Startup** | 0.501/s / 114.5 ms | **0.174/s / 89.9 ms** | **-65.3% / -21.5%** |
| **50 concurrent sessions — RSS / Peak / Threads** | 9.43 / 10.76 MiB / 30–40 | **8.40 / 8.58 MiB / 19** | **-11.0% / -20.3% / -37% to -53%** |
| **50 concurrent sessions — `sessions` RPC p50 / p95** | 13.94 / 16.99 ms | **3.08 / 4.26 ms** | **-77.9% / -74.9%** *(4.5× faster)* |
| **Burst ingest — Throughput / Settled RSS** | 925 ev/s / 9.21 MiB | **1,227 ev/s / 6.75 MiB** | **+32.7% / -26.8%** |
| **Large payloads — Settled RSS (`RssAnon`) / Peak** | 8.84 (4.90) / 18.45 MiB | **5.76 (1.03) / 13.77 MiB** | **-34.8% (-79.0%) / -25.4%** |
| **Large payloads — History paging p50 / p95** | 79.38 / 84.88 ms | **40.86 / 45.44 ms** | **-48.5% / -46.5%** |
| **Reconnect churn — Cycle p50 / CPU / Peak RSS** | 8.91 ms / 61.3% / 10.75 MiB | **6.26 ms / 50.3% / 6.32 MiB** | **-29.7% / -18.0% / -41.2%** |
| **500 s soak (~22.3k ev, 2.34k approvals) — Settled / Peak** | 10.72 / 19.03 MiB | **7.78 / 8.76 MiB** | **-27.4% / -54.0%** |
| **glibc (`gnu`) comparison — `large_payloads` RSS** | 34.60 MiB | **9.90 MiB** | **-71.4%** |
| **`heaptrack` / `massif` — Peak heap / Alloc calls** | 31.22 MB (`29.73 MB`) / 359k | **1.93 MB (`1.09 MB`) / 209k** | **-93.8% (`-96.3%`) / -41.9%** |

- **Tests:** Full Rust workspace test suite passes **1,611 tests** (+7 new regression tests) with **0 new failures**.
- **Unmeasured platforms:** No macOS host/Swift compiler or `aarch64-linux-musl` C cross-linker was available in the cloud Linux VM. Three clear-cut SwiftUI fixes (**F13–F15**) are isolated in their own commit (`c227433`) and marked **unmeasured on macOS**; native macOS benchmark scripts are included under `bench/macos/`.
- **Biggest remaining win (requires team approval):** Each live Claude session spawns a separate Bun `cc-agent-host` (~50–80 MiB RSS each — ~10× `ccd`'s own footprint). Replacing or multiplexing `cc-agent-host` (**P4** in Section 7) is the #1 opportunity for system-wide memory reduction.

---

## 2. Measurement Environment & Reproducible Harness

- **Machine:** Linux `6.18.57` x86_64 (Ubuntu 24.04), 2 vCPU (`Intel Xeon @ 2.30 GHz`), 7.77 GiB RAM
- **Toolchain:** `rustc 1.100.0 (0ef1bc386 2026-11-09)`, `bun 1.2.14`, `heaptrack`, `valgrind --tool=massif`
- **Primary measured target:** `x86_64-unknown-linux-musl` release (`opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`), matching `.github/workflows/linux-build.yml`.
- **Offline benchmark harness (`bench/`):** Exercises the real `ccd` and `codeconnect` binaries over their real interfaces (Unix domain sockets, WebSocket listener, SQLite `events.db`, Codex WS-over-UDS, Claude host protocol) with compiled single-threaded Rust mocks (`bench/mocks/` — `mock-claude-host`, `mock-codex`, `mock-client`) so 50-session benchmarks run distortion-free and 100% offline.
- **Process-wide accounting:** Collects `/proc/<pid>/smaps_rollup` (RSS, PSS, USS, `RssAnon`, `RssFile`, Peak RSS), `/proc/<pid>/task/*/status` (summed per-TID voluntary + non-voluntary context switches for accurate Tokio worker wakeups), threads, FDs, sockets, startup latency, and client-observed RPC/relay/history/diff latencies (min/median/p95/max + CV% over 3 runs).

### One-Command Reproduction

```bash
# Full Linux musl benchmark (3 runs x 8 scenarios, incl. 5-min cold_idle)
./bench/run.sh

# Fast verification (~2-3 min, 15s cold_idle)
./bench/run.sh --quick

# Long soak test (e.g. 2 hours)
./bench/run.sh --soak 7200

# glibc comparison
./bench/run.sh --target gnu --quick --runs 3

# Heaptrack + Valgrind Massif allocation profiling
./bench/profile.sh bench/out/profile

# Native macOS profiling (run on a Mac; see bench/macos/README.md)
./bench/macos/bench-ccd.sh
./bench/macos/profile-app.sh
```

---

## 3. Full Baseline vs Final Benchmark Results

### 3.1 Shipped `x86_64-unknown-linux-musl` — Memory, CPU, Wakeups, Threads, FDs, Startup (3-Run Medians)

> *Wakeups note:* Process-wide per-TID baseline wakeups were recorded on `955d8ec` (`bench/out/baseline-wakeups`) for `cold_idle`, `idle_client`, `concurrent_10`, and `concurrent_50`. The remaining four scenarios (`n/a*`) were captured before per-TID summation was added to `driver.py`, so final process-wide rates are reported directly.

| Scenario | Metric | Baseline (med) | Final (med) | Delta | Final min–max (CV%) |
|---|---|---:|---:|---:|---|
| **`cold_idle`** *(3 × 5 min)* | RSS / PSS / USS (MiB) | 6.30 / 6.29 / 6.29 | **5.42 / 5.41 / 5.41** | **-14.0%** | 5.30–5.45 (1.4%) |
| | Peak RSS (MiB) | 10.78 | **5.48** | **-49.2%** | 5.34–5.50 (1.5%) |
| | Non-code `RssAnon` (MiB) | 1.76 | **0.90** | **-49.0%** | 0.89–0.91 |
| | CPU (%) / Wakeups/s | 0.007% / 0.501/s | **0.000% / 0.174/s** | **-65.3% wakeups** | 0.17–0.21/s (11.7%) |
| | Threads / FDs / Startup | 6 / 23 / 114.5 ms | **5 / 19 / 89.9 ms** | **-17% FDs, -21.5% start** | 88.1–100.5 ms |
| **`idle_client`** *(60 s)* | RSS / Peak RSS (MiB) | 6.26 / 10.89 | **5.47 / 5.48** | **-12.7% / -49.6%** | 5.41–5.57 (1.5%) |
| | Non-code `RssAnon` (MiB) | 1.73 | **0.97** | **-44.1%** | — |
| | CPU (%) / Wakeups/s | 0.00% / 0.668/s | **0.00% / 0.434/s** | **-35.0% wakeups** | 0.43–0.50/s (8.5%) |
| | Threads / FDs / Startup | 5 / 24 / 109.3 ms | **4 / 20 / 100.4 ms** | **-20% thr, -17% FDs** | 99.0–137.3 ms |
| **`concurrent_1`** | RSS / Peak RSS (MiB) | 6.47 / 10.58 | **5.86 / 5.91** | **-9.3% / -44.1%** | 5.70–5.96 (2.3%) |
| | CPU (%) / Threads / FDs | 0.25% / 3 / 26 | **0.17% / 3 / 22** | **-33.3% CPU, -4 FDs** | 22.1/s (`n/a*`) |
| **`concurrent_10`** | RSS / Peak RSS (MiB) | 7.07 / 10.64 | **6.37 / 6.46** | **-9.8% / -39.3%** | 6.36–6.40 (0.4%) |
| | CPU (%) / Wakeups/s | 1.35% / 138.4/s | **1.12% / 104.7/s** | **-17.1% CPU, -24.4% wk** | 98.0–106.4/s (4.3%) |
| | FDs / Startup | 44 / 99.5 ms | **40 / 91.4 ms** | **-4 FDs, -8.1% start** | 90.6–103.3 ms |
| **`concurrent_50`** | RSS / Peak RSS (MiB) | 9.43 / 10.76 | **8.40 / 8.58** | **-11.0% / -20.3%** | 8.35–8.45 (0.6%) |
| | Non-code `RssAnon` (MiB) | 4.59 | **3.63** | **-20.9%** | — |
| | CPU (%) / Wakeups/s | 6.43% / 473.4/s | **5.40% / 307.2/s** | **-16.0% CPU, -35.1% wk** | 289.9–321.4/s (5.2%) |
| | Peak Threads / FDs | 30–40 / 124 | **19 / 120** | **-37% to -53% thr, -4 FDs** | 19 in all 3 runs |
| **`burst`** | RSS / Peak RSS (MiB) | 9.21 / 11.74 | **6.75 / 9.19** | **-26.8% / -21.7%** | 6.74–6.97 (1.9%) |
| | Non-code `RssAnon` (MiB) | 4.44 | **2.08** | **-53.2%** | — |
| | CPU (%) / Threads / FDs | 72.7% / 22 / 24 | **74.3% / 19 / 19** | **+32.7% tput, -5 FDs** | 9,971/s (`n/a*`) |
| **`large_payloads`** | RSS / Peak RSS (MiB) | 8.84 / 18.45 | **5.76 / 13.77** | **-34.8% / -25.4%** | 5.65–5.82 (1.6%) |
| | Non-code `RssAnon` (MiB) | 4.90 | **1.03** | **-79.0%** | — |
| | CPU (%) / Threads / FDs | 75.3% / 8 / 25 | **76.9% / 7 / 19** | **-6 FDs, -48.5% hist** | 2,584/s (`n/a*`) |
| **`reconnect_churn`** | RSS / Peak RSS (MiB) | 6.81 / 10.75 | **6.18 / 6.32** | **-9.2% / -41.2%** | 6.15–6.20 (0.4%) |
| | CPU (%) / FDs / Startup | 61.3% / 23 / 100.8 ms | **50.3% / 19 / 89.7 ms** | **-18.0% CPU, -4 FDs** | 7,678/s (`n/a*`) |

### 3.2 Latency & Throughput (`x86_64-unknown-linux-musl`, 3-Run Medians)

| Scenario | Metric | Baseline | Final | Delta |
|---|---|---:|---:|---:|
| `concurrent_1` | `sessions_rpc_p50` (ms) | 0.92 | **0.38** | **-58.7%** |
| `concurrent_10` | `relay_p50` (ms) | 173.63 | **111.67** | **-35.7%** |
| | `sessions_rpc_p50 / p95` (ms) | 4.49 / 6.15 | **1.14 / 4.63** | **-74.6% / -24.7%** |
| `concurrent_50` | `relay_p50 / p95` (ms) | 952.34 / 1105.84 | **913.07 / 1057.80** | **-4.1% / -4.3%** |
| | `sessions_rpc_p50 / p95` (ms) | 13.94 / 16.99 | **3.08 / 4.26** | **-77.9% / -74.9%** |
| `burst` | Throughput (`events_per_sec`) | 924.66 | **1,227.27** | **+32.7%** |
| | `relay_p50 / p95 / max` (ms) | 810.53 / 957.99 / 1067.28 | **598.17 / 712.80 / 803.11** | **-26.2% / -25.6% / -24.8%** |
| `large_payloads` | `history_p50 / p95` (ms) | 79.38 / 84.88 | **40.86 / 45.44** | **-48.5% / -46.5%** |
| | `relay_p50 / p95` (ms) | 482.59 / 844.36 | **449.74 / 798.02** | **-6.8% / -5.5%** |
| | `diff_p50 / p95` (ms) | 22.12 / 25.15 | **22.65 / 23.82** | flat / **-5.3%** |
| `reconnect_churn` | `cycle_p50 / p95` (ms) | 8.91 / 11.85 | **6.26 / 7.81** | **-29.7% / -34.1%** |

### 3.3 500-Second Soak Test (`x86_64-unknown-linux-musl`)

Steady workload: 10 persistent sessions + 24 churn sessions spawned/exited without pruning, ~22,300 events ingested and relayed, ~2,340 approval cards raised and answered.

| Metric | Baseline (500 s) | Final (500 s) | Delta |
|---|---:|---:|---:|
| Settled RSS / PSS / USS (MiB) | 10.72 | **7.78** | **-27.4%** |
| Peak RSS (MiB) | 19.03 | **8.76** | **-54.0% (-10.27 MiB)** |
| Non-code `RssAnon` (MiB) | 5.86 | **2.92** | **-50.1%** |
| CPU (%) / Wakeups/s | 6.91% / 682.2/s | **5.90% / 651.2/s** | **-14.6% CPU / -4.5%** |
| Peak Threads / FDs | 24 / 44 | **18 / 40** | **-25.0% thr / -4 FDs** |
| `relay_p50 / p95 / max` (ms) | 3.66 / 84.82 / 2,537.6 | **2.48 / 83.29 / 325.8** | **-32.2% / -1.8% / -87.2%** |
| Start -> End RSS delta | 7.70 -> 9.11 (+1.41 MiB) | **6.71 -> 7.27 (+0.56 MiB)** | **-60.3% net growth** |

> **Engineering note on remaining ~0.50 MiB soak drift (`t = 100 s -> 500 s`):** Two sources account for the residual drift: (1) `PushGate::permission` (`mac/ccd/src/push_gate.rs:55`) retains every answered approval `request_id` string for the session's lifetime (~2,340 strings ≈ 150–200 KiB), and (2) the soak driver intentionally leaves 24 churn sessions unpruned (retaining per-session `session_facts`, `tree_facts`, and WS `watermarks` until delete/prune). See **Section 7.1** for the low-risk follow-up fix.

### 3.4 `x86_64-unknown-linux-gnu` Comparison & Heap Profiling (`heaptrack` / `massif`)

- **glibc (`gnu`) RSS wins (3-run medians):**
  - `large_payloads`: `34.60 -> 9.90 MiB` (**-71.4%**), Peak `34.60 -> 17.13 MiB` (**-50.5%**)
  - `burst`: `19.79 -> 14.20 MiB` (**-28.3%**)
  - `reconnect_churn`: `10.68 -> 8.93 MiB` (**-16.4%**), CPU `58.3% -> 46.4%`
  - `concurrent_50`: threads `34 -> 19` (**-44.1%**), CPU `5.94% -> 4.47%` (**-24.7%**)
  - `cold_idle`: `7.15 -> 6.52 MiB` (**-8.9%**), Peak `11.40 -> 6.52 MiB` (**-42.8%**)
- **`heaptrack` (mixed workload):** Peak heap `31.22 -> 1.93 MB` (**-93.8%**), total alloc calls `359,153 -> 208,719` (**-41.9%**), temporary allocs `30,865 -> 20,832` (**-32.5%**), live heap at exit `2.92 -> 1.71 MB` (**-41.4%**), baseline #1 call site (`proc_identity::read_stat`, 88,886 allocs) reduced to **0**, SQLite peak heap capped at **785.1 KiB**.
- **`valgrind --tool=massif` (idle + 10 sessions):** Peak heap `29.73 -> 1.087 MB` (**-96.3%**).
- **Binary size (`x86_64-unknown-linux-musl`, `-p ccd`):** `6,259,296 B` (`5.969 MiB`) -> `6,279,264 B` (`5.988 MiB`, `+0.3%`, **0 new crates added**).

---

## 4. Technical Guide to Implemented Optimizations (Commit-by-Commit)

Every optimization is isolated in a single logical commit on `perf/baseline-and-optimizations`, so the team can review, benchmark, or cherry-pick items individually.

### Overview Matrix

| ID | Commit | Target Area | Risk | Primary Win | Status |
|---|---|---|---|---|---|
| **F1** | `3c2c0fb` | Startup executable SHA-256 | Very low | `-41%` startup Peak RSS, `-12%` startup latency | Verified (Linux) |
| **F2** | `20b9624` | `/proc/stat` parser & `codex_host` scan | Low | `-88,886` heap allocs, `-16%` 50-sess CPU, `+13.5%` burst | Verified (Linux) |
| **F3** | `af190d0` | `Daemon::sessions` SQLite batching + `prepare_cached` | Low | **-76% `sessions` RPC latency** (`13.9 -> 3.3 ms`), `-32%` wakeups | Verified (Linux) |
| **F4** | `f78b224` | Zero-DOM `RawEventFrame` replay & history paging | Low | **-45% history p50**, `-21%` `large_payloads` RSS, `+34%` burst | Verified (Linux) |
| **F5** | `df00bca` | `broadcast::Sender<Arc<Event>>` (256) + zero-clone send | Low | `-240 KiB` idle ring, `-15%` large relay p50, `+41%` burst | Verified (Linux) |
| **F6** | `fccab84` | SQLite pool (`READERS=2`), 256 KiB cache, 8 KiB lookaside | Low | `-5.0 MiB` `large_payloads` Peak, `-23%` burst/churn RSS, `-4` FDs | Verified (Linux) |
| **F7** | `292b19a` | Evict `session_facts`/`tree_facts`; bound `Starts` (256) | Low | Prevents unbounded daemon memory growth on long lifetimes | Verified (Linux) |
| **F8** | `15256ec` | Lazy outbound TLS root CA loading (`LazyOutboundH2`) | Low | `-90 KiB` Linux `RssAnon` (< 1 MiB); avoids Keychain load on macOS | Verified (Linux) |
| **F9** | `b6b8392` | Event-driven mail sweep, 10 s tailnet cache, bounded Tokio pool | Low | **-65% idle wakeups**, `concurrent_50` threads `41 -> 19` | Verified (Linux) |
| **F10** | `c689feb` | Remove unused `tokio/fs` and `tokio-rustls/logging` | Very low | `-20.6 KiB` `ccd` binary size (`-p ccd`) | Verified (Linux) |
| **F12** | `28763ed` | Index-based line scanner in `agent-host/src/link.ts` | Very low | `1.8×` faster multi-line IPC parsing in Bun | Verified (Bun) |
| **F13–F15** | `c227433` | macOS SwiftUI `LazyVStack`, `TableBlock`, address poll backoff | Very low | Lower diff RAM, 4×->1× table layout parse, 6× fewer idle IPC polls | **Unmeasured on macOS** |

---

### Detailed Engineering Breakdown

#### F1 — Stream Executable SHA-256 (`3c2c0fb`)
- **Files:** `mac/protocol/src/hash.rs`, `mac/ccd/src/main.rs`, `mac/codeconnect/src/service.rs`
- **Root cause:** At startup, `ccd` ran `std::fs::read(&exe)` to hash its own binary, pulling the entire ~6 MiB stripped (or ~29 MiB debug) executable into heap — creating a 4.5+ MiB startup Peak RSS spike (`massif` peak `29.73 MB`). `codeconnect daemon status` did the same.
- **Fix:** Added `protocol::hash::sha256_file_hex` streaming in 64 KiB stack chunks.
- **Impact:** `cold_idle` Peak RSS `10.78 -> 6.34 MiB` (`-41.2%`), startup `114.5 -> 100.5 ms` (`-12.2%`).
- **Test added:** `protocol::hash::tests::sha256_file_hex_matches_in_memory_digest`.

#### F2 — Zero-Allocation `/proc/<pid>/stat` Parser + Off-Runtime Shared `ProcessTable` (`20b9624`)
- **Files:** `mac/protocol/src/proc_identity.rs`, `mac/ccd/src/codex_host.rs`
- **Root cause:** `proc_identity::read_stat` allocated a `Vec<String>` per PID (88,886 heap allocations — #1 in baseline `heaptrack`). Under 50 sessions, each Codex session's 2 s `TOOL_SCAN` synchronously scanned `/proc` on Tokio worker threads.
- **Fix:** Replaced `read_stat` with a stack-buffered zero-alloc parser (`StatFields`), cached `CLK_TCK`, exposed a reusable `ProcessTable` snapshot, offloaded periodic `codex_host::record_tools_async` onto `spawn_blocking` with a shared 500 ms TTL snapshot cache (keeping initial, `turn/completed`, and `Stop` scans fresh), skipped redundant immediate tick 0, and bounded exited tool groups > 32.
- **Impact:** Eliminated all 88,886 `read_stat` allocs; `concurrent_50` CPU `-15.9%`, `relay_p50` `-14.6%`; `burst` throughput `+13.5%`.
- **Test added:** `protocol::proc_identity::tests::parse_stat_line_handles_spaces_and_parens_in_comm`.

#### F3 — Batched `Store::sessions_snapshot` + `prepare_cached` (`af190d0`)
- **Files:** `mac/ccd/src/store.rs`, `mac/ccd/src/db.rs`, `mac/ccd/src/state.rs`
- **Root cause:** `Daemon::sessions()` issued 3–4 sequential `spawn_blocking` hops *per session* (~150–200 thread handoffs and SQLite statement re-parses per RPC at N=50), plus unindexed scans for model receipts.
- **Fix:** Added `Store::sessions_snapshot` executing all per-fleet queries on a single reader connection in one blocking hop (`run_hosts LEFT JOIN accounts`, `GROUP BY` `max_seq`, SQL `INSTR(...)` prefilter for model receipts, inline open cards & relaunchables). Switched hot-path queries (`append_in_tx`, `events_after`, `events_before`, `list_sessions`, `device_by_token_hash`, `device_is_active`, etc.) to `prepare_cached`.
- **Impact:** `concurrent_50` `sessions` RPC p50/p95 `13.94/16.99 -> 3.34/3.87 ms` (**-76.0% / -77.2%**); `concurrent_50` wakeups `-32.4%`; `reconnect_churn` cycle p50 `-21.0%`.

#### F4 — Zero-DOM Raw JSON Replay & History Paging (`f78b224`)
- **Files:** `mac/Cargo.toml`, `mac/ccd/src/store.rs`, `mac/ccd/src/db.rs`, `mac/ccd/src/ws_server.rs`
- **Root cause:** Backlog replay and history paging read SQLite's `events.payload` TEXT column, parsed every 64 KiB JSON payload into a full `serde_json::Value` DOM tree in heap, cloned it into `ServerMessage::Event`, and immediately re-serialized it back to JSON text.
- **Fix:** Enabled `serde_json`'s built-in `raw_value` feature (0 new crates), added `RawEventFrame` + `Store::raw_event_frames_after` (with a 4 MiB soft chunk byte cap so large backlogs never spike memory) + `Store::raw_event_frames_before`, and wired `ws_server::Backlog` and `History` through `send_raw_event_frame` (preserving the exact `MAX_CLIENT_MESSAGE_BYTES` placeholder fallback).
- **Impact:** `large_payloads` history p50/p95 `79.4/84.9 -> 43.5/48.2 ms` (**-45.3% / -43.3%**), settled RSS `-20.8%`; `burst` throughput `+33.7%` vs baseline.
- **Test added:** `store::tests::raw_event_frames_match_typed_wire_and_respect_byte_budget` (asserts byte-for-byte equivalence with `ServerMessage::Event`).

#### F5 — `broadcast::Sender<Arc<Event>>` (cap 256), `receiver_count()` Guard & Zero-Clone Send (`df00bca`)
- **Files:** `mac/ccd/src/state.rs`, `mac/ccd/src/ws_server.rs`, `mac/ccd/src/switch.rs`, `mac/ccd/src/claude_host.rs`
- **Root cause:** `Daemon::events_tx` pre-allocated 1,024 full `Event` slots (~246 KiB permanent heap) and cloned every ingested `Event` into the ring even with zero WS subscribers, plus cloned again per subscriber on live send. `Daemon::ingest` also had a redundant `pending.clone()`.
- **Fix:** Switched to `broadcast::Sender<Arc<Event>>` (256 slots ≈ 6 KiB), gated broadcast on `receiver_count() > 0`, removed redundant `pending.clone()`, and added `send_event_ref` to serialize live `&Event` without cloning.
- **Impact:** `large_payloads` `relay_p50` `-15.2%`, wakeups `-12.4%`; `burst` throughput rose to `1,300 ev/s` (`+40.6%` vs baseline).

#### F6 — SQLite Pool Sizing, Page Cache Cap, 8 KiB Lookaside & `PRAGMA shrink_memory` (`fccab84`)
- **Files:** `mac/ccd/src/store.rs`
- **Root cause:** 5 SQLite connections (`1 writer + 4 readers`), each with SQLite's default 2 MiB page cache (up to 10 MiB total) and ~120 KiB lookaside (~600 KiB idle heap), never shrunk after migrations or bulk prunes.
- **Fix:** Reduced `READERS` from `4` to `2` (3 total WAL connections), configured `SQLITE_DBCONFIG_LOOKASIDE` to `64 × 128 = 8 KiB` before first query, set `PRAGMA cache_size = -256` (768 KiB pool ceiling), `PRAGMA mmap_size = 0`, `set_prepared_statement_cache_capacity(32)`, and added `Store::shrink_memory()` invoked after `migrate()`, `delete_exited_session`, and non-empty `prune_exited_sessions`.
- **Impact:** `cold_idle` RSS `6.29 -> 5.63 MiB` (`-10.5%`), FDs `-4`; `large_payloads` Peak RSS `18.84 -> 13.81 MiB` (**-5.03 MiB / -26.7%**); `burst` RSS `9.26 -> 7.14 MiB` (**-22.9%**); `reconnect_churn` RSS `8.33 -> 6.38 MiB` (**-23.4%**) with zero latency regression.
- **Test added:** `store::tests::open_connection_caps_memory_pragmas_and_shrink_memory_succeeds`.

#### F7 — Evict `session_facts`/`tree_facts` on Delete/Prune & Bound `start_session::Starts` (`292b19a`)
- **Files:** `mac/ccd/src/state.rs`, `mac/ccd/src/start_session.rs`
- **Root cause:** `Inner::session_facts` and `Inner::tree_facts` were never evicted when sessions were deleted or pruned, and `start_session::Starts` retained every completed `request_id` forever in memory despite durable idempotency in SQLite `run_hosts.start_request_id`.
- **Fix:** Evicted `session_facts` and `tree_facts` on `delete_exited_session` and non-dry-run `prune_ended_sessions` (shrinking maps when `cap > max(64, 4 * len)`). Bounded completed entries in `Starts` to `MAX_COMPLETED_STARTS = 256` (FIFO; in-flight starts are never evicted).
- **Tests added:** `state::tests::delete_and_prune_evict_in_memory_session_and_tree_facts` and `start_session::tests::starts_bounds_completed_entries_and_preserves_in_flight`.

#### F8 — Lazy Outbound TLS Root CA Loading (`15256ec`)
- **Files:** `mac/ccd/src/tls.rs`, `mac/ccd/src/relay_sender.rs`, `mac/ccd/src/apns_sender.rs`
- **Root cause:** Startup eagerly called `rustls_native_certs::load_native_certs()` (~150 OS root CAs / macOS Keychain) via `HttpsRelay::new()` and `ApnsPushSender::new()`, even if push is never triggered.
- **Fix:** Added `tls::LazyOutboundH2` that loads root CAs once on a blocking thread on first actual push delivery (failures are not cached so transient errors retry), keeping identical TLS/ALPN verification.
- **Impact:** `cold_idle` `RssAnon` dropped `1.008 -> 0.918 MiB` (`-8.9%`; total non-code RAM now under 1 MiB). Expected to save even more startup time and memory on macOS (avoiding eager Keychain access).
- **Test updated:** Lazy-init assertion added in `relay_sender::tests::the_endpoint_is_baked_and_a_config_file_cannot_move_it`.

#### F9 — Event-Driven Idle Mail Sweep, 10 s Coalesced Tailnet Cache & Bounded Tokio Threads (`b6b8392`)
- **Files:** `mac/ccd/src/store.rs`, `mac/ccd/src/db.rs`, `mac/ccd/src/mail.rs`, `mac/ccd/src/state.rs`, `mac/ccd/src/ws_server.rs`, `mac/ccd/src/main.rs`
- **Root cause:**
  1. `mail::run_mail_sweeps` woke every 30 s unconditionally, spawning `tailscale status --json` and taking a SQLite `BEGIN IMMEDIATE` write lock even when `mail` and `mail_sent` were empty.
  2. Concurrent WS clients and mail sweeps could spawn parallel `tailscale status --json` subprocesses; `ws_server` also JSON-serialized `peers_frame` twice (once to diff, once in `send`).
  3. `#[tokio::main]` spawned `num_cpus` workers (10–16+ on Apple Silicon Macs / large Linux hosts) and allowed up to 512 blocking threads (40–61 threads observed during `concurrent_50`).
- **Fix:**
  1. Made `run_mail_sweeps` event-driven via `Store::mail_has_rows` + `tokio::sync::Notify` (`wait_for_mail`) and `has_mail_activity()`. Starts `true` so pre-existing DB rows are swept immediately on startup (verified by `tests/mail_at_start.rs`), then sleeps with **0 wakeups / 0 SQLite writes / 0 `tailscale` spawns** when empty. `sweep_mail_smart` skips `read_tailnet()` unless `unsent` or `untold` is non-empty.
  2. Added a 10 s coalesced `tailnet_cache` in `Daemon::read_status` (non-test), while `read_tailnet()` **still reads `blocked_tailnet_nodes()` fresh from SQLite on every call** so device revocation stays immediate and fail-closed.
  3. Added `ws_server::send_serialized` to eliminate double peers JSON serialization, and an early return in `watch_hosted_runs` when no hosted runs exist.
  4. Built Tokio runtime explicitly with `worker_threads(available_parallelism.clamp(2, 4))`, `max_blocking_threads(16)`, `thread_keep_alive(10s)`, handling `--version` before creating the runtime.
- **Impact:** `cold_idle` wakeups `0.501 -> 0.174/s` (**-65.3%**); `concurrent_50` peak threads `41 -> 19` (**-53.7%**), Peak RSS `8.58 MiB`.
- **Test added:** `store::tests::mail_activity_flag_and_wait_for_mail_wakes_on_insert`.

#### F10 — Unused Crate Feature Cleanup (`c689feb`)
- **Files:** `mac/Cargo.toml`
- **Fix:** Removed unused `"fs"` from workspace `tokio` (0 `tokio::fs` calls in workspace) and `"logging"` from `tokio-rustls` (`ccd` uses its own logger `mac/ccd/src/log.rs`).
- **Impact:** `-20.6 KiB` on `ccd` release binary when built with `-p ccd` (`6,300,320 -> 6,279,264 B`). *(Note: building from the virtual workspace root without `-p ccd` unifies `push-relay`'s `rustls/logging` feature back in.)*

#### F12 — Linear Line Scanning in `agent-host` (`28763ed`)
- **Files:** `mac/agent-host/src/link.ts`
- **Root cause:** `read()` looped `rest = rest.slice(end + 1)` on every newline (`O(k²)` copies on bursty multi-line chunks).
- **Fix:** Replaced with index-based `text.indexOf('\n', start)`, slicing only each completed line and slicing the partial tail once.
- **Impact:** Bun 1.2.14 microbenchmark on 10,000-line chunks: `2.19 -> 1.20 ms` (**1.8× faster**).

#### F13–F15 — macOS SwiftUI Clear-Cut Fixes (`c227433` — **Unmeasured on macOS**)
- **Files:** `macos/CodeConnect/Views/DiffView.swift`, `macos/CodeConnect/Views/AgentText.swift`, `macos/CodeConnect/Views/MainWindow.swift`, `macos/CodeConnect/Views/TailscaleNote.swift`
- **Changes (by code review; must be compiled and measured on macOS before merge):**
  - **F13 (`DiffView.swift:68`):** Changed `VStack` -> `LazyVStack` for `FileDiffView` rows inside `ChangesTab` / `DecisionBlock` scroll views so large diffs do not materialize all offscreen lines upfront.
  - **F14 (`AgentText.swift:150-200`):** Hoisted `let short = shortColumns` and `let minNeeded = needed(short: short)` once at the top of `TableBlock.body` (previously `shortColumns` re-parsed every cell's inline Markdown **4×** and `needed` ran **2×** across `.frame(minWidth: needed, idealWidth: needed)` and both `ViewThatFits` branches).
  - **F15 (`MainWindow.swift:631-639`, `TailscaleNote.swift:71-78`):** Backed off `LocalDaemon.address(root:)` polling from 5 s to 30 s once a reachable non-loopback address settles, while preserving 5 s polling whenever unresolved or on loopback for fast Tailscale startup detection.

---

## 5. Security, Protocol & Compatibility Guarantees

- **Wire protocol (`ios/` compatibility):** Zero wire format changes. `RawEventFrame` is verified byte-for-byte identical to `ServerMessage::Event` and preserves `MAX_CLIENT_MESSAGE_BYTES` placeholder fallback.
- **On-disk SQLite schema & rollback safety:** Zero schema changes (`SCHEMA_VERSION = 6` unchanged); all rollback triggers and downgrade guarantees are preserved.
- **Config format:** Zero config schema changes.
- **Security invariants preserved:**
  - Ed25519 (Linux) and Developer ID (macOS) signature verification untouched; streaming SHA-256 produces identical digests.
  - Tailscale-only / localhost-only default binding untouched.
  - Approval risk classification and Face ID / passcode gating untouched.
  - Device revocation (`blocked_tailnet_nodes`) is **never cached** (read fresh from SQLite on every `read_tailnet()` call); `LazyOutboundH2` performs identical platform-root TLS + ALPN verification.
- **Test verification:** `TMPDIR=/tmp/cc-test-tmp cargo test --manifest-path mac/Cargo.toml --workspace --no-fail-fast`:
  - **1,611 passed** (baseline 1,604 + 7 new regression tests), **0 new failures**, 60 ignored.
  - Only failure is the pre-existing VM container FD-ceiling test `protocol::fdlimit::tests::a_higher_soft_limit_is_left_as_it_is` (`ulimit -Hn 4096` < 10,241), documented in `perf/BASELINE.md` and left untouched.

---

## 6. Rejected Experiments & Dead Ends (Save Team Effort)

1. **`opt-level = "s"` vs existing `opt-level = "z"`:** Tested on `x86_64-unknown-linux-musl` (`CARGO_PROFILE_RELEASE_OPT_LEVEL=s`). Grew `ccd` by **+540.7 KiB (+8.6%, `6.28 -> 6.82 MB`)** and regressed `cold_idle` RSS (`5.50 -> 5.89 MiB`, `+7.1%`) and `burst` RSS (`6.99 -> 7.54 MiB`). **Keep existing `[profile.release]`** (`opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`) — `.text` is ~83% of idle RSS, so `"z"` is optimal.
2. **Custom allocator (`mimalloc`/`jemalloc`, F11) on `musl`:** Not implemented because (a) no `aarch64-linux-musl` C cross-linker was available to verify ARM64 builds, and (b) `musl`'s `mallocng` already delivers `5.42 MiB` idle RSS (`0.90 MiB` anon) and `1,227 ev/s` burst. Adding `mimalloc` to `musl` would likely regress binary size (+150–250 KiB) and idle RSS (+0.5–1.0 MiB). Only worth evaluating for `gnu` if glibc Linux binaries are ever shipped.
3. **`publish_gates` / `registration_gates` / `actuation_gates` / `answer_locks`:** Audited during Phase 2 — already self-prune via `retain(|_, g| Arc::strong_count(g) > 1)` on lookup; no leak exists.

---

## 7. Prioritized Follow-Up Roadmap (For Engineering Evaluation)

### 7.1 Quick Low-Risk Follow-Up
- **Bound `PushGate::permission` (`mac/ccd/src/push_gate.rs:55`):** Currently stores every answered approval `request_id` until the session is deleted (~150–200 KiB per ~2,300 approvals on long-lived sessions). Removing entries on approval settlement or capping per-session history (e.g., 256-entry ring) will flatten steady-state soak slope toward `0 MiB/hr`.

### 7.2 Higher-Impact Architectural / Cross-Target Proposals (`perf/PROPOSALS.md`)

| Priority | Proposal | Location | Estimated Gain | Why Team Approval Needed |
|---|---|---|---|---|
| **1 (Highest system RAM)** | **P4. Native Rust or multiplexed Claude host** replacing per-run Bun `cc-agent-host` | `mac/agent-host/`, `mac/ccd/src/claude_host.rs` | **-45 to -75 MiB RSS per live Claude session** (-450–750 MiB @ 10 sessions!) | Major architectural change (porting or multiplexing SDK host while keeping `protocol::host` UDS protocol) |
| **2 (Highest macOS/iOS GUI)** | **P5 (F16). `CodeConnectKit` on-demand ticker + bounded `AppModel.diffs` LRU** | `CodeConnectKit/Sources/CodeConnectKit/AppModel.swift:1887-1899` | Eliminates 1 Hz `@Observable` wakeups per machine; bounds diff RAM | Shared package with `ios/`; requires Xcode + macOS/iOS verification |
| **3 (Ingest allocs & CPU)** | **P3. `Box<RawValue>` for `protocol::Event::payload`** on live ingest | `mac/protocol/src/event.rs`, `ccd` | `-30% to -50%` ingest allocs, `+15–25%` burst tput, `-1 to -2 MiB` `large_payloads` Peak | Public Rust crate API change across workspace |
| **4 (Disk & cellular BW)** | **P2. Compress `events.payload` > 4 KiB (`zstd`/`lz4`)** in SQLite / WS `permessage-deflate` | `store.rs`, `ws_server.rs`, `ios/` | `-60% to -80%` DB/WAL size, `-50% to -75%` cellular bandwidth | Changes on-disk SQLite format (rollback compatibility) / wire negotiation |
| **5 (Idle threads)** | **P1. `current_thread` Tokio runtime** for `ccd` (+ small blocking pool) | `mac/ccd/src/main.rs` | `-1 to -3` idle threads, `-150 to -400 KiB` RSS on macOS/`gnu` | Runtime architecture change; needs audit to ensure zero blocking on reactor |
| **6 (glibc only)** | **P6 (F11). `mimalloc` (`MI_PURGE_DELAY=0`)** on `gnu` builds | `mac/ccd/Cargo.toml` | `-40% to -60%` `gnu` RSS under load | C build / ARM64 cross-compile CI verification |
| **7 (Disk space)** | **P7. `codeconnect sessions compact`** (`VACUUM` / `auto_vacuum = INCREMENTAL`) | `store.rs`, `codeconnect` CLI | Reclaims freed SQLite disk pages after bulk prunes | New CLI command / DB pragma policy |

---

## 8. Recommended Evaluation & Adoption Checklist

1. **Review & cherry-pick strategy:**
   - Commits **F1–F10 & F12** (`3c2c0fb` through `28763ed`) are self-contained, Linux-verified, zero-breaking-change wins — recommended for direct review and merge after macOS + `aarch64-linux-musl` CI verification.
   - Commit **F13–F15** (`c227433`) is macOS SwiftUI-only — build in Xcode, visually inspect large diffs (`DiffView`) and Markdown tables (`AgentText`), and verify Tailscale startup + settled poll cadence (`MainWindow`, `TailscaleNote`).
2. **Run native macOS benchmarks before & after on Apple Silicon:**
   ```bash
   # Compare main (955d8ec) vs perf/baseline-and-optimizations (a5fe8af)
   ./bench/macos/bench-ccd.sh
   ./bench/macos/profile-app.sh
   ```
   Verify `footprint`, `vmmap --summary`, `leaks`, `xctrace` Allocations/Time Profiler, and `powermetrics --samplers tasks` (expect even larger relative wins on Apple Silicon due to F8 lazy Keychain load and F9 capping Tokio workers from 10–16 down to 4).
3. **Run CI Linux matrix (`x86_64-unknown-linux-musl` + `aarch64-unknown-linux-musl`):**
   - Confirm `SQLITE_DBCONFIG_LOOKASIDE` FFI call and `proc_identity` compile and pass tests cleanly on `aarch64-unknown-linux-musl`.
   - Note that building `-p ccd` separately from `push-relay` gives the full `-20.6 KiB` F10 binary savings.
4. **Prioritize P4 and P5 (F16)** in the next sprint for the largest remaining *user-visible* RAM and battery wins across the full CodeConnect stack.

---

### Supporting Files on Branch `perf/baseline-and-optimizations`

- `perf/RESULTS.md` — Final vs baseline tables (`musl`, `gnu`, 500 s soak, `heaptrack`/`massif`)
- `perf/BASELINE.md` — Phase 1 baseline methodology and numbers
- `perf/FINDINGS.md` — Full ranked F1–F16 + P1–P4 analysis
- `perf/CHANGELOG.md` — Per-commit before/after numbers
- `perf/PROPOSALS.md` — Detailed migration plans for P1–P7
- `bench/` & `bench/macos/` — Reproducible Linux & macOS benchmark/profiling harness
