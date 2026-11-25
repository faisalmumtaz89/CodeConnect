# CodeConnect Performance & Memory Optimization Results

## Executive Summary
1. Across 13 optimization items (F1–F10, F12, and macOS-only F13–F15) on `perf/baseline-and-optimizations`, `ccd`'s resident memory, idle wakeups, thread count, and RPC/history latency were cut substantially with zero protocol, schema, config, or security changes.
2. **Idle footprint (`cold_idle`, 3 × 5 min, shipped `x86_64-unknown-linux-musl`):** RSS dropped `6.30 -> 5.42 MiB` (**-14.0%**), Peak RSS dropped `10.78 -> 5.48 MiB` (**-49.2%**), non-code anonymous RAM (`RssAnon`) dropped `1.76 -> 0.90 MiB` (**-49.0%**, now under 1 MiB), startup fell `114.5 -> 89.9 ms` (**-21.5%**), and idle wakeups fell `0.501 -> 0.174/s` (**-65.3%**).
3. **50 concurrent sessions (`concurrent_50`):** RSS dropped `9.43 -> 8.40 MiB` (**-11.0%**), Peak RSS `10.76 -> 8.58 MiB` (**-20.3%**), peak threads `30–40 -> 19` (**-36.7% to -52.5%**), CPU `6.43% -> 5.40%` (**-16.0%**), wakeups `473 -> 307/s` (**-35.1%**), and `sessions` RPC p50/p95 dropped `13.94/16.99 -> 3.08/4.26 ms` (**-77.9% / -74.9%**).
4. **Heavy workloads (`burst`, `large_payloads`, `reconnect_churn`):** `large_payloads` settled RSS fell `8.84 -> 5.76 MiB` (**-34.8%**, `RssAnon` **-79.0%**), Peak RSS `18.45 -> 13.77 MiB` (**-25.4%**), history p50 `79.4 -> 40.9 ms` (**-48.5%**); `burst` throughput rose `925 -> 1,227 ev/s` (**+32.7%**) while settled RSS fell `9.21 -> 6.75 MiB` (**-26.8%**); `reconnect_churn` cycle p50 fell `8.91 -> 6.26 ms` (**-29.7%**) and CPU fell **-18.0%**.
5. **500-second soak (`soak`, ~22.3k events, ~2.34k approvals, 24 churn sessions):** Settled RSS fell `10.72 -> 7.78 MiB` (**-27.4%**), Peak RSS fell `19.03 -> 8.76 MiB` (**-54.0%**), `RssAnon` fell `5.86 -> 2.92 MiB` (**-50.1%**), and max relay latency fell `2,538 -> 326 ms` (**-87.2%**).
6. **glibc (`x86_64-unknown-linux-gnu`) comparison:** `large_payloads` RSS fell `34.60 -> 9.90 MiB` (**-71.4%**), `burst` RSS fell `19.79 -> 14.20 MiB` (**-28.3%**), and `concurrent_50` threads dropped `34–61 -> 19`.
7. **Heap profiling (`heaptrack` & `valgrind --tool=massif`):** Mixed-workload peak heap fell `31.22 -> 1.93 MB` (**-93.8%**), total allocations fell `359,153 -> 208,719` (**-41.9%**), and `massif` peak heap fell `29.73 -> 1.09 MB` (**-96.3%**), with SQLite capped at `785 KiB`.
8. **Correctness:** Full Rust workspace test suite passes **1,611 tests** (up from 1,604 with 7 new regression tests) with **0 new failures** (only the pre-existing container `ulimit -Hn 4096` `protocol::fdlimit` failure recorded in `perf/BASELINE.md`).
9. **Unmeasured on macOS & ARM64:** Three low-risk SwiftUI fixes (F13–F15) were applied by code review and marked **unmeasured on macOS**; native macOS benchmark scripts (`bench/macos/`) are ready to run on a Mac, and `aarch64-unknown-linux-musl` lacked a C cross-linker in this VM.
10. **What's left:** Remaining major wins require architectural/protocol/Swift changes documented in `perf/PROPOSALS.md` — notably replacing per-run Bun `cc-agent-host` (~50–80 MiB RSS per Claude session, P4), `CodeConnectKit` 1 Hz ticker & diff cache (P5), `Box<RawValue>` live ingest (P3), and bounding `PushGate::permission` for long-lived sessions.

---

## Environment & Commit Range

- **Base commit (`main`):** `955d8eca6109acb0ccd4c6890341d8e83eb4cd72` (untouched)
- **Final commit (`perf/baseline-and-optimizations`):** `c2274339007542ec7716ed8cc8e36154e19cbbbd` (+ final docs commit)
- **Host:** Linux `6.18.57` x86_64 (Ubuntu 24.04), 2 vCPU (`Intel Xeon @ 2.30 GHz`), 7.77 GiB RAM
- **Toolchain:** `rustc 1.100.0 (0ef1bc386 2026-11-09)`, `bun 1.2.14`
- **Primary target:** `x86_64-unknown-linux-musl` release (`opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`), matching `.github/workflows/linux-build.yml`
- **Reproduce in one command:** `./bench/run.sh` (full, 3 runs × 8 scenarios including 5-min `cold_idle`), `./bench/run.sh --soak 7200` (2 h soak), `./bench/profile.sh` (`heaptrack` + `massif`)

---

## Final vs Baseline — Shipped `x86_64-unknown-linux-musl` (3-Run Medians)

> Note on `Wakeups/s`: Baseline process-wide per-TID wakeup rates come from `bench/out/baseline-wakeups` (collected on `955d8ec` immediately after adding `/proc/<pid>/task/*/status` summation) for `cold_idle`, `idle_client`, `concurrent_10`, and `concurrent_50`. `baseline-musl` had main-thread-only wakeups for `concurrent_1`, `burst`, `large_payloads`, and `reconnect_churn`, so those four rows show `n/a*` for baseline wakeups and report exact final process-wide rates.

### Memory, CPU, Wakeups, Threads, FDs & Startup

| Scenario | Metric | Baseline (med) | Final (med) | Delta | Final Range (min–max, CV%) |
|---|---|---:|---:|---:|---|
| **`cold_idle`** *(3 × 5 min)* | RSS / PSS / USS (MiB) | 6.30 / 6.29 / 6.29 | **5.42 / 5.41 / 5.41** | **-14.0%** | 5.30–5.45 (1.4%) |
| | Peak RSS (MiB) | 10.78 | **5.48** | **-49.2%** | 5.34–5.50 (1.5%) |
| | Non-code `RssAnon` (MiB) | 1.76 | **0.90** | **-49.0%** | 0.89–0.91 |
| | CPU (%) / Wakeups/s | 0.007% / 0.501/s | **0.000% / 0.174/s** | **-65.3% wakeups** | 0.17–0.21/s (11.7%) |
| | Threads / FDs / Startup | 6 / 23 / 114.5 ms | **5 / 19 / 89.9 ms** | **-17% FDs, -21.5% startup** | 88.1–100.5 ms |
| **`idle_client`** *(60 s)* | RSS / Peak RSS (MiB) | 6.26 / 10.89 | **5.47 / 5.48** | **-12.7% / -49.6%** | 5.41–5.57 (1.5%) |
| | Non-code `RssAnon` (MiB) | 1.73 | **0.97** | **-44.1%** | — |
| | CPU (%) / Wakeups/s | 0.00% / 0.668/s | **0.00% / 0.434/s** | **-35.0% wakeups** | 0.43–0.50/s (8.5%) |
| | Threads / FDs / Startup | 5 / 24 / 109.3 ms | **4 / 20 / 100.4 ms** | **-20% thr, -17% FDs** | 99.0–137.3 ms |
| **`concurrent_1`** | RSS / Peak RSS (MiB) | 6.47 / 10.58 | **5.86 / 5.91** | **-9.3% / -44.1%** | 5.70–5.96 (2.3%) |
| | CPU (%) / Threads / FDs | 0.25% / 3 / 26 | **0.17% / 3 / 22** | **-33.3% CPU, -4 FDs** | 22.1 wakeups/s (`n/a*`) |
| **`concurrent_10`** | RSS / Peak RSS (MiB) | 7.07 / 10.64 | **6.37 / 6.46** | **-9.8% / -39.3%** | 6.36–6.40 (0.4%) |
| | CPU (%) / Wakeups/s | 1.35% / 138.4/s | **1.12% / 104.7/s** | **-17.1% CPU, -24.4% wakeups** | 98.0–106.4/s (4.3%) |
| | FDs / Startup | 44 / 99.5 ms | **40 / 91.4 ms** | **-4 FDs, -8.1% startup** | 90.6–103.3 ms |
| **`concurrent_50`** | RSS / Peak RSS (MiB) | 9.43 / 10.76 | **8.40 / 8.58** | **-11.0% / -20.3%** | 8.35–8.45 (0.6%) |
| | Non-code `RssAnon` (MiB) | 4.59 | **3.63** | **-20.9%** | — |
| | CPU (%) / Wakeups/s | 6.43% / 473.4/s | **5.40% / 307.2/s** | **-16.0% CPU, -35.1% wakeups** | 289.9–321.4/s (5.2%) |
| | Peak Threads / FDs | 30–34 (41 pre-F9) / 124 | **19 / 120** | **-36.7% to -53.7% thr, -4 FDs** | 19 across all 3 runs |
| **`burst`** | RSS / Peak RSS (MiB) | 9.21 / 11.74 | **6.75 / 9.19** | **-26.8% / -21.7%** | 6.74–6.97 (1.9%) |
| | Non-code `RssAnon` (MiB) | 4.44 | **2.08** | **-53.2%** | — |
| | CPU (%) / Threads / FDs | 72.7% / 22 / 24 | **74.3% / 19 / 19** | **+32.7% throughput, -5 FDs** | 9,971 wakeups/s (`n/a*`) |
| **`large_payloads`** | RSS / Peak RSS (MiB) | 8.84 / 18.45 | **5.76 / 13.77** | **-34.8% / -25.4%** | 5.65–5.82 (1.6%; base CV 23.4%) |
| | Non-code `RssAnon` (MiB) | 4.90 | **1.03** | **-79.0%** | — |
| | CPU (%) / Threads / FDs | 75.3% / 8 / 25 | **76.9% / 7 / 19** | **-6 FDs, -48.5% history p50** | 2,584 wakeups/s (`n/a*`) |
| **`reconnect_churn`** | RSS / Peak RSS (MiB) | 6.81 / 10.75 | **6.18 / 6.32** | **-9.2% / -41.2%** | 6.15–6.20 (0.4%) |
| | CPU (%) / FDs / Startup | 61.3% / 23 / 100.8 ms | **50.3% / 19 / 89.7 ms** | **-18.0% CPU, -4 FDs** | 7,678 wakeups/s (`n/a*`) |

### Latency & Throughput (`x86_64-unknown-linux-musl`, 3-Run Medians)

| Scenario | Metric | Baseline | Final | Delta |
|---|---|---:|---:|---:|
| `concurrent_1` | `sessions_rpc_p50` (ms) | 0.92 | **0.38** | **-58.7%** |
| `concurrent_10` | `relay_p50` (ms) | 173.63 | **111.67** | **-35.7%** |
| | `sessions_rpc_p50 / p95` (ms) | 4.49 / 6.15 | **1.14 / 4.63** | **-74.6% / -24.7%** |
| `concurrent_50` | `relay_p50 / p95` (ms) | 952.34 / 1105.84 | **913.07 / 1057.80** | **-4.1% / -4.3%** |
| | `sessions_rpc_p50 / p95` (ms) | 13.94 / 16.99 | **3.08 / 4.26** | **-77.9% / -74.9%** |
| `burst` | Throughput (`events_per_sec`) | 924.66 | **1,227.27** | **+32.7%** |
| | `relay_p50 / p95 / max` (ms) | 810.53 / 957.99 / 1067.28 | **598.17 / 712.80 / 803.11** | **-26.2% / -25.6% / -24.8%** |
| | `approval_rtt_p50` (ms) | 11.18 | 12.64 | +1.46 ms (within burst jitter) |
| `large_payloads` | `history_p50 / p95` (ms) | 79.38 / 84.88 | **40.86 / 45.44** | **-48.5% / -46.5%** |
| | `relay_p50 / p95` (ms) | 482.59 / 844.36 | **449.74 / 798.02** | **-6.8% / -5.5%** |
| | `diff_p50 / p95` (ms) | 22.12 / 25.15 | **22.65 / 23.82** | flat / **-5.3%** |
| `reconnect_churn` | `cycle_p50 / p95` (ms) | 8.91 / 11.85 | **6.26 / 7.81** | **-29.7% / -34.1%** |

### 500-Second Soak Test (`x86_64-unknown-linux-musl`)

Both baseline and final ran for 500 s under identical steady activity (~22.3k events, ~2,340 approvals answered, 10 persistent sessions + 24 churn sessions spawned and exited without pruning):

| Metric | Baseline (500 s) | Final (500 s) | Delta |
|---|---:|---:|---:|
| Settled RSS / PSS / USS (MiB) | 10.72 / 10.72 / 10.72 | **7.78 / 7.78 / 7.78** | **-27.4%** |
| Peak RSS (MiB) | 19.03 | **8.76** | **-54.0% (-10.27 MiB)** |
| Non-code `RssAnon` (MiB) | 5.86 | **2.92** | **-50.1%** |
| CPU (%) / Wakeups/s | 6.91% / 682.2/s | **5.90% / 651.2/s** | **-14.6% CPU / -4.5%** |
| Peak Threads / FDs | 24 / 44 | **18 / 40** | **-25.0% thr / -4 FDs** |
| `relay_p50 / p95 / max` (ms) | 3.66 / 84.82 / 2,537.6 | **2.48 / 83.29 / 325.8** | **-32.2% / -1.8% / -87.2%** |
| Start -> End RSS (MiB) | 7.70 -> 9.11 (+1.41 MiB) | **6.71 -> 7.27 (+0.56 MiB)** | **-60.3% net delta** |
| Linear RSS slope (`t = 100..490 s`) | +3.16 MiB/hr *(+2.64 full)* | +3.90 MiB/hr *(+3.85 full)* | see note below |

> **Soak slope note:** Although net RSS gain over the 500 s run dropped 60% (`+1.41 -> +0.56 MiB`) and total RSS dropped nearly 3 MiB (`10.72 -> 7.78 MiB`, Peak `-10.27 MiB`), a residual ~0.50 MiB rise occurred between `t = 100 s` (`7.32 MiB`) and `t = 500 s` (`7.82 MiB`, settling to `7.27 MiB` on client stop). Audit identified two causes: (1) `PushGate::permission` retains every answered approval `request_id` string for the lifetime of a live session (~2,340 IDs ≈ 150–200 KiB), and (2) the soak driver leaves its 24 exited churn sessions unpruned (retaining per-session `session_facts`, `tree_facts`, and WS `watermarks` until deleted/pruned). Bounding `PushGate::permission` on approval resolution is called out in **What's Left** below.

---

## Final vs Baseline — `x86_64-unknown-linux-gnu` Comparison (3-Run Medians)

| Scenario | Baseline RSS -> Final RSS | Baseline Peak -> Final Peak | Baseline Thr -> Final Thr | Baseline CPU -> Final CPU |
|---|---|---|---|---|
| `cold_idle` | 7.15 -> **6.52 MiB** (-8.9%) | 11.40 -> **6.52 MiB** (-42.8%) | 3 -> **3** | 0.00 -> **0.00%** |
| `idle_client` | 7.17 -> **6.67 MiB** (-7.0%) | 11.23 -> **6.67 MiB** (-40.6%) | 3 -> **3** | 0.00 -> **0.00%** |
| `concurrent_1` | 7.70 -> **7.12 MiB** (-7.6%) | 11.28 -> **7.12 MiB** (-36.9%) | 3 -> **3** | 0.16 -> **0.16%** |
| `concurrent_10` | 8.55 -> **8.35 MiB** (-2.4%) | 11.28 -> **8.35 MiB** (-26.0%) | 10 -> **10** | 1.27 -> **0.96%** |
| `concurrent_50` | 11.77 -> **11.66 MiB** (-0.9%) | 11.94 -> **11.68 MiB** (-2.2%) | 34 -> **19** (-44.1%) | 5.94 -> **4.47%** (-24.7%) |
| `burst` | 19.79 -> **14.20 MiB** (-28.3%) | 19.79 -> **14.34 MiB** (-27.6%) | 21 -> **19** | 66.32 -> **62.46%** |
| `large_payloads` | 34.60 -> **9.90 MiB** (**-71.4%**) | 34.60 -> **17.13 MiB** (**-50.5%**) | 8 -> **8** | 60.80 -> **56.77%** |
| `reconnect_churn` | 10.68 -> **8.93 MiB** (-16.4%) | 11.39 -> **8.93 MiB** (-21.6%) | 14 -> **10** (-28.6%) | 58.29 -> **46.43%** (-20.4%) |

---

## Heap & Allocation Profiling (`heaptrack` & `valgrind --tool=massif`)

| Profiler & Metric | Baseline | Final | Delta |
|---|---:|---:|---:|
| **`heaptrack` (mixed workload)** Peak heap | 31.22 MB | **1.93 MB** | **-93.8%** |
| Peak RSS (incl. heaptrack overhead) | 39.10 MB | **20.00 MB** | **-48.8%** |
| Total allocation calls | 359,153 (4,823/s) | **208,719 (2,721/s)** | **-41.9%** |
| Temporary allocations | 30,865 (414/s) | **20,832 (271/s)** | **-32.5%** |
| Live heap at SIGTERM exit | 2.92 MB | **1.71 MB** | **-41.4%** |
| #1 baseline call site (`proc_identity::read_stat`) | 88,886 allocs | **0 allocs** | **-100%** |
| SQLite peak heap (`sqlite3MemMalloc`) | ~1.5+ MB | **785.1 KiB** | capped by `3 × 256 KiB` pool |
| **`valgrind --tool=massif` (idle + 10 sessions)** Peak heap | 29.73 MB | **1.087 MB** | **-96.3%** |

---

## Binary Size & Dependencies

| Artifact / Metric | Baseline | Final | Delta |
|---|---:|---:|---:|
| `x86_64-unknown-linux-musl/ccd` (`-p ccd`) | 5.969 MiB (6,259,296 B) | **5.988 MiB (6,279,264 B)** | +19.5 KiB (+0.3%; F10 saved -20.6 KiB vs F9) |
| `x86_64-unknown-linux-musl/codeconnect` | 0.997 MiB (1,045,312 B) | **0.997 MiB (1,045,312 B)** | 0 B |
| `x86_64-unknown-linux-gnu/ccd` | 5.836 MiB (6,119,864 B) | **5.836 MiB (6,119,864 B)** | 0 B |
| Unique crates (`ccd` / `codeconnect` / `protocol`) | 93 / 33 / 27 | **93 / 33 / 27** | 0 new crates added |
| Duplicate crate groups (`cargo tree -d`) | 2 | **2** | unchanged (`hashbrown`, `syn`) |

---

## Summary of Changes Implemented

All details and per-commit before/after numbers are recorded in `perf/CHANGELOG.md` and `perf/FINDINGS.md`:
- **F1 (`3c2c0fb`):** Streaming 64 KiB buffered SHA-256 (`protocol::hash::sha256_file_hex`) replacing whole-binary `std::fs::read(&exe)` at startup.
- **F2 (`20b9624`):** Zero-allocation stack-buffered `/proc/<pid>/stat` parser + off-runtime `spawn_blocking` shared 500 ms `ProcessTable` in `codex_host` + bounded exited tool groups.
- **F3 (`af190d0`):** Batched `Store::sessions_snapshot` collapsing ~150–200 blocking pool hops per `sessions` RPC into 1 call, SQL `INSTR(...)` prefiltering for model receipts, and `prepare_cached` on SQLite hot paths.
- **F4 (`f78b224`):** Zero-DOM raw JSON replay & history paging (`RawEventFrame`) with 4 MiB chunk cap using built-in `serde_json` `raw_value`.
- **F5 (`df00bca`):** `broadcast::Sender<Arc<Event>>` (capacity 256, down from 1024 × `Event`), `receiver_count() > 0` guard, removed redundant `pending.clone()`, zero-clone live delivery (`send_event_ref`).
- **F6 (`fccab84`):** SQLite `READERS = 2`, 8 KiB lookaside (`SQLITE_DBCONFIG_LOOKASIDE`), `cache_size = -256` (256 KiB/conn), `mmap_size = 0`, statement cache cap 32, and `Store::shrink_memory()` after migrate/delete/prune.
- **F7 (`292b19a`):** Evict `Inner::session_facts` and `Inner::tree_facts` on delete/prune (shrinking sparse hash tables), and bound completed entries in `start_session::Starts` to 256 while retaining all in-flight starts.
- **F8 (`15256ec`):** `tls::LazyOutboundH2` deferring OS root CA parsing (`rustls_native_certs`) until first outbound push delivery (relay & APNs).
- **F9 (`b6b8392`):** Event-driven zero-wakeup idle `mail::run_mail_sweeps` (preserving instant startup sweep), 10 s coalesced `tailscale status --json` cache (keeping `blocked_tailnet_nodes()` fresh on every call), single-pass `peers_frame` serialization, early return in `watch_hosted_runs`, and bounded Tokio runtime (`2..=4` workers, `16` max blocking threads).
- **F10 (`c689feb`):** Removed unused `tokio/fs` and `tokio-rustls/logging` features (`-20.6 KiB` on `-p ccd`).
- **F12 (`28763ed`):** Linear index-based line scanning in `mac/agent-host/src/link.ts` (`1.8×` faster on 10k-line chunks in Bun).
- **F13–F15 (`c227433`, unmeasured on macOS):** `LazyVStack` in `DiffView.swift`, single-pass `shortColumns`/`needed` in `AgentText.swift`, and 5 s -> 30 s settled address poll backoff in `MainWindow.swift` & `TailscaleNote.swift`.

---

## What We Tried That Didn't Help (or Was Rejected)

1. **`opt-level = "s"` vs existing `opt-level = "z"` (`[profile.release]`):** Built `ccd` (`musl`) with `CARGO_PROFILE_RELEASE_OPT_LEVEL=s` and benchmarked against `"z"`. Binary size grew **+540.7 KiB (+8.6%, `6.28 -> 6.82 MB`)**, `cold_idle` RSS regressed **`5.50 -> 5.89 MiB` (+7.1%)**, and `burst` RSS/Peak regressed **`6.99/9.38 -> 7.54/9.89 MiB`**. Rejected; existing `[profile.release]` (`opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`) is already optimal for minimal RSS.
2. **Custom allocator swap (`mimalloc` / `jemalloc`, F11):** Rejected under decision rules because (a) no `aarch64-linux-musl` C cross-linker is available in this environment to verify all supported Linux targets, and (b) `musl`'s `mallocng` already achieves `5.42 MiB` idle RSS (`0.90 MiB` anon) and `1,227 ev/s` burst throughput; adding `mimalloc` to `musl` would likely regress binary size (+150–250 KiB) and idle RSS (+0.5–1.0 MiB). Documented as Proposal P6 for `gnu`-only evaluation if glibc binaries are ever shipped.
3. **Removing `publish_gates` / `registration_gates` / `actuation_gates` / `answer_locks`:** Inspected during Phase 2; code already calls `retain(|_, g| Arc::strong_count(g) > 1)` on lookup, so no leak exists there.
4. **Workspace feature unification quirk on `rustls/logging`:** Building from the virtual workspace root without `-p ccd` unifies `push-relay`'s `rustls/logging` feature into `ccd` (+20.6 KiB). Release builds using `-p ccd` get the full -20.6 KiB reduction from F10.

---

## Unmeasured Targets & Platforms

- **macOS (`macos/CodeConnect/`, SwiftUI app & launchd daemon):** No macOS host or Swift toolchain was available. Shared Rust crate improvements (F1–F10) apply directly to macOS `ccd` (with F8 likely saving even more on macOS by avoiding eager Keychain trust-store parsing, and F9 capping workers from 10–16 down to 4 on Apple Silicon). The three macOS-only SwiftUI changes (**F13, F14, F15**) are **unmeasured on macOS** and should be verified on a Mac using `./bench/macos/run_macos_bench.sh` (`footprint`, `vmmap --summary`, `leaks`, `xctrace`, `powermetrics`).
- **`aarch64-unknown-linux-musl`:** The Rust target `aarch64-unknown-linux-musl` is installed, but no `aarch64-linux-musl` C cross-compiler/linker is installed in this VM to compile C dependencies (`libsqlite3-sys`, `ring`). All changes are pure Rust / portable POSIX (using `std::ffi::c_void` for `SQLITE_DBCONFIG_LOOKASIDE`) with no x86_64-specific code.

---

## What's Left (With Estimated Gains)

1. **Bound `PushGate::permission` on approval resolution (small follow-up, low risk):** Removing answered `request_id` entries (or capping the per-session set to the most recent 256 cards) eliminates ~150–200 KiB of growth per ~2,300 approvals on very long-lived sessions and brings steady-state soak slope closer to `0 MiB/hr`.
2. **Proposals requiring approval (see `perf/PROPOSALS.md` for migration plans):**
   - **P4 — Native Rust or shared Claude host:** **-45 to -75 MiB RSS per live Claude session** (by far the largest remaining *system-wide* memory win once `ccd` itself is at 5.4–8.4 MiB).
   - **P5 (F16) — `CodeConnectKit` on-demand ticker + bounded `diffs` LRU:** Eliminates 1 Hz macOS/iOS observation wakeups per machine and caps GUI diff cache RAM.
   - **P3 — `Box<RawValue>` for `protocol::Event::payload`:** **-30% to -50% allocation rate**, **+15% to +25% burst throughput**, **-1 to -2 MiB `large_payloads` peak RSS**.
   - **P2 — Compress large `events.payload` blobs (SQLite / wire):** **-60% to -80% DB & WAL disk size**, **-50% to -75% cellular bandwidth**.
   - **P1 — `current_thread` Tokio runtime for `ccd`:** **-1 to -3 idle threads**, **-150 to -400 KiB RSS** on macOS / `gnu`.
   - **P7 — `codeconnect sessions compact` / incremental vacuum:** Reclaims disk space in `events.db` after bulk prunes.
