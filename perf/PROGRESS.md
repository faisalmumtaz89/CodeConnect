# CodeConnect Performance & Memory Optimization Progress

## Branch & Isolation
- Branch: `perf/baseline-and-optimizations` (cut from `main` HEAD `955d8ec`)
- `main` untouched at `955d8ec`; no PRs opened/merged, no tags/releases, no CI release configs or signing keys touched.

## Phase Status
- **Phase 1 — Baseline**: **COMPLETE** (`3aaf8fe`, `perf/BASELINE.md`)
- **Phase 2 — Analysis**: **COMPLETE** (`5c51185`, `perf/FINDINGS.md` — F1–F16 + P1–P4)
- **Phase 3 — Optimize & Final Verification**: **COMPLETE** (F1–F10, F12, F13–F15 committed one logical change per commit; F11 & `opt-level="s"` evaluated and rejected; P1–P7 documented in `perf/PROPOSALS.md`; final benchmarks, soak, `gnu` comparison, `heaptrack`/`massif`, and `perf/RESULTS.md` complete)

## Completed
- [x] Built reproducible offline benchmark harness under `bench/` (`./bench/run.sh`, `bench/driver.py`, `bench/profile.sh`, `bench/mocks/`, `bench/bin/mock-claude`) and native macOS scripts under `bench/macos/`.
- [x] Collected full Phase 1 baseline (`musl` 3 runs × 8 scenarios incl. 3 × 5-min `cold_idle`, process-wide wakeups, 500 s soak, `gnu` 3-run comparison, `heaptrack` + `massif`) -> `perf/BASELINE.md`.
- [x] Profiled and ranked all findings -> `perf/FINDINGS.md`.
- [x] Implemented, tested, and benchmarked optimizations (each on its own commit with before/after numbers in `perf/CHANGELOG.md`):
  - [x] **F1 (`3c2c0fb`)** — Streaming 64 KiB SHA-256 (`protocol::hash::sha256_file_hex`)
  - [x] **F2 (`20b9624`)** — Zero-alloc `/proc/<pid>/stat` parser + off-runtime 500 ms `ProcessTable` in `codex_host`
  - [x] **F3 (`af190d0`)** — Batched `Store::sessions_snapshot`, SQL `INSTR` prefilter, `prepare_cached` hot paths
  - [x] **F4 (`f78b224`)** — Zero-DOM `RawEventFrame` replay/history paging with 4 MiB chunk cap
  - [x] **F5 (`df00bca`)** — `broadcast::Sender<Arc<Event>>` (256), `receiver_count() > 0` guard, zero-clone live send
  - [x] **F6 (`fccab84`)** — SQLite `READERS = 2`, 8 KiB lookaside, 256 KiB page cache cap, `mmap_size = 0`, `shrink_memory()`
  - [x] **F7 (`292b19a`)** — Evict `session_facts`/`tree_facts` on delete/prune; bound `start_session::Starts` to 256 completed entries
  - [x] **F8 (`15256ec`)** — `tls::LazyOutboundH2` lazy root CA loading for relay & APNs
  - [x] **F9 (`b6b8392`)** — Event-driven idle `run_mail_sweeps`, 10 s coalesced tailnet cache, single-pass peers JSON, bounded Tokio runtime (`2..=4` workers, `16` blocking)
  - [x] **F10 (`c689feb`)** — Removed unused `tokio/fs` and `tokio-rustls/logging` features (-20.6 KiB `-p ccd`)
  - [x] **F11 (evaluated & rejected)** — Documented `opt-level = "s"` regression (+541 KiB, +0.39 MiB idle RSS) and deferred allocator swap to `perf/PROPOSALS.md` (P6)
  - [x] **F12 (`28763ed`)** — Linear index-based line scanning in `mac/agent-host/src/link.ts` (1.8× faster)
  - [x] **F13–F15 (`c227433`, unmeasured on macOS)** — `LazyVStack` in `DiffView.swift`, single-pass `TableBlock` in `AgentText.swift`, 5 s -> 30 s settled address poll backoff in `MainWindow.swift` & `TailscaleNote.swift`
- [x] Verified full workspace test suite: **1,611 passed, 0 new failures** (only pre-existing `protocol::fdlimit` VM ceiling failure), 60 ignored.
- [x] Collected final verification benchmarks (`bench/out/final-musl` incl. 3 × 5-min `cold_idle`, `bench/out/final-soak` 500 s, `bench/out/final-gnu`, `bench/out/final-profile` `heaptrack` + `massif`).
- [x] Wrote `perf/PROPOSALS.md` and `perf/RESULTS.md`.

## Blockers Encountered & Workarounds
1. **Linux `/tmp` ownership in `cargo test`:** Used `TMPDIR=/tmp/cc-test-tmp` (0700) matching CI.
2. **10-minute non-daemon command timeout:** Added `--append` to `bench/driver.py` / `bench/run.sh` to split 3 × 5-min `cold_idle` runs cleanly across 3 batches, and matched 500 s soak duration across baseline and final.
3. **No macOS host / Swift compiler & no `aarch64-linux-musl` C cross-linker:** Marked F13–F15 clearly **unmeasured on macOS**, left shared `CodeConnectKit` (F16) as Proposal P5, rejected F11 C allocator swap under the all-targets decision rule (moved to Proposal P6), and provided native macOS scripts in `bench/macos/`.

## Key Decisions Made
1. Preserved wire protocol, on-disk SQLite schema/rollback compatibility, config format, all security boundaries (revocation never cached, Ed25519/TLS verification intact), and all user-visible features.
2. Added 7 new regression unit tests covering streaming SHA-256, `/proc` stat parsing, batched `sessions_snapshot`, `RawEventFrame` wire equivalence & byte budget, SQLite memory pragmas, `session_facts`/`tree_facts` eviction & `Starts` cap, lazy TLS init, and event-driven mail wakeup.
3. Stopped after Tier 1–3 because remaining code-level opportunities save < 2% or require protocol/schema/architectural changes (documented in `perf/PROPOSALS.md`).
