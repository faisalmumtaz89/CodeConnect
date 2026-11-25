# CodeConnect Optimization Proposals (Requires Approval)

The optimizations below were identified during profiling and code review but **not implemented** on `perf/baseline-and-optimizations` because they alter the wire protocol, on-disk SQLite format, public cross-crate / iOS-shared APIs, runtime architecture, or cross-platform C allocator build dependencies. Each entry includes evidence, estimated gains, risks, and a concrete migration plan.

---

## P1. Single-Threaded (`current_thread`) Tokio Runtime + Small Blocking Pool for `ccd`

- **Location:** `mac/ccd/src/main.rs`
- **Current state:** After F9, `ccd` uses a multi-thread Tokio runtime with `2..=4` worker threads and `max_blocking_threads = 16`. On idle Linux `musl`, `ccd` holds 3–4 threads (`RssAnon = 0.91 MiB`).
- **Proposal:** Switch `ccd` from `Builder::new_multi_thread()` to `Builder::new_current_thread()` (keeping `max_blocking_threads(16)` for SQLite and blocking OS calls). `ccd` is almost entirely I/O-bound (Unix sockets, WebSocket frames, SQLite on blocking threads) with minimal CPU-bound async work.
- **Estimated gain:**
  - **-1 to -3 idle OS threads** (especially on 4–16 core Macs and servers)
  - **-150 to -400 KiB RSS** on macOS / Linux `gnu` (eliminating worker stacks, per-worker Tokio work-stealing queues, and per-thread malloc arenas)
  - **-10% to -25% cross-thread wakeups** under light load (no worker-to-worker task stealing / parking notifications)
- **Risks:**
  - Any accidentally synchronous CPU work on an async task (e.g., TLS handshake, large JSON serialization, or synchronous file I/O) would briefly stall other concurrent WebSocket/IPC connections.
  - Requires careful audit of all async handlers to guarantee no blocking call runs directly on the reactor thread.
- **Migration plan:**
  1. Audit all `ws_server`, `ipc_server`, `claude_host`, and `codex_host` tasks for blocking calls (`tokio-console` / `RUSTFLAGS` + poll-duration warning).
  2. Switch `main.rs` runtime builder behind a feature/env flag or dedicated branch, and benchmark `concurrent_50`, `burst`, `large_payloads`, and `reconnect_churn` with `./bench/run.sh` to confirm p95 relay latency does not regress.

---

## P2. Compress Large `events.payload` Blobs in SQLite and/or Wire Frames

- **Location:** `mac/ccd/src/store.rs` (`events.payload`), `mac/ccd/src/ws_server.rs`, `mac/protocol/`, `ios/`
- **Current state:** Every event payload (including 64 KiB tool outputs, diffs, and long assistant messages) is stored verbatim as uncompressed JSON `TEXT` in SQLite and transmitted uncompressed over WebSocket.
- **Proposal:**
  - **On-disk:** Store `events.payload` compressed with `zstd` (or `lz4`) when payload length > 4 KiB (e.g., flagged by a new `payload_encoding` column or magic prefix), keeping small events uncompressed for SQLite `INSTR`/`json_extract` fast paths.
  - **On-wire (optional):** Negotiate RFC 7692 `permessage-deflate` WebSocket compression between `ccd` and iPhone/macOS clients for history paging and diff responses.
- **Estimated gain:**
  - **-60% to -80% SQLite database file size** on diff- and tool-output-heavy sessions (also reducing SQLite WAL size and OS page cache footprint).
  - **-50% to -75% network bandwidth** and faster history paging over cellular Tailscale links.
  - **-2 to -4 MiB peak RSS** during `large_payloads` history paging if decompressed lazily on the client.
- **Risks:**
  - **Changes on-disk SQLite format** (older `ccd` binaries would fail to parse compressed payloads on downgrade, violating CodeConnect's documented rollback compatibility contract in `store.rs`).
  - Adds a compression crate (`zstd` or `lz4_flex`) and ~100–300 KiB to the `ccd` binary.
  - CPU trade-off on compression/decompression (fast with `lz4_flex` or `zstd` level 1).
- **Migration plan:**
  1. Ship a forward-compatible reader first (schema v7 with nullable `payload_codec` column; older daemons ignore unknown columns only if `payload` remains valid JSON, OR bump minimum rollback version once adopted).
  2. Gate wire compression on capability negotiation in the WebSocket `hello` handshake so older iPhone builds continue receiving plain JSON unchanged.
  3. Benchmark `large_payloads` and `burst` with `./bench/run.sh` before enabling by default.

---

## P3. Use `Box<serde_json::value::RawValue>` for `protocol::event::Event::payload`

- **Location:** `mac/protocol/src/event.rs`, `mac/ccd/src/state.rs`, `mac/ccd/src/store.rs`, `mac/ccd/src/ws_server.rs`
- **Current state:** F4 eliminated `serde_json::Value` DOM parsing on SQLite replay (`Backlog`) and history paging (`History`) via `RawEventFrame`. However, *live* ingest (`PendingEvent` -> `Event`) still parses `payload` into a `serde_json::Value` tree (dozens of small heap allocations per nested JSON object), extracts a few metadata fields (`model`, `permission_mode`, approval card fields), re-serializes to JSON `TEXT` for SQLite, and re-serializes again for WebSocket broadcast.
- **Proposal:** Represent `Event.payload` as `Box<serde_json::value::RawValue>` (or an enum `Payload::Raw(Box<RawValue>)`) and parse only the specific fields needed by `facts::extract` / approval classification on demand using targeted zero-copy structs.
- **Estimated gain:**
  - **-30% to -50% allocation rate** on `burst` and `large_payloads` live ingest paths.
  - **-1 to -2 MiB Peak RSS** on `large_payloads` (eliminating the live `Value` DOM tree + re-serialization copy during ingest).
  - **+15% to +25% burst throughput** on `musl`.
- **Risks:**
  - Breaking Rust API change in `protocol::event::Event` across `ccd`, `codeconnect`, `demo-daemon`, and hundreds of unit tests that construct `Event` with `serde_json::json!({...})`.
- **Migration plan:**
  1. Add helper constructors (`Event::with_json_payload(Value)` / `Event::payload_value()`) so existing test suites migrate mechanically.
  2. Convert hot ingest paths (`claude_host`, `codex_host`, `ipc_server`) to construct `RawValue` directly from incoming JSON lines without full DOM round-trips.
  3. Verify byte-for-byte wire equivalence against `mac/protocol` golden fixtures.

---

## P4. Native Rust Claude Host (or Shared Multi-Session Host) Replacing Per-Run Bun `cc-agent-host`

- **Location:** `mac/agent-host/`, `mac/ccd/src/claude_host.rs`
- **Current state:** Each live hosted Claude session spawns its own standalone Bun compiled binary (`cc-agent-host`). While `ccd` itself now uses only **~5.5 MiB idle / ~8.5 MiB at 50 sessions**, each real Bun `cc-agent-host` process consumes **~50–80 MiB RSS** of JavaScriptCore + Bun runtime overhead! Running 10 concurrent Claude sessions costs ~500–800 MiB in `cc-agent-host` processes alone — nearly **100×** `ccd`'s own footprint.
- **Proposal:** Either:
  - **(A) Native Rust Claude host** speaking Claude Code's NDJSON stdio protocol directly (single ~2–3 MiB binary or lightweight per-session Rust helper process using ~3–4 MiB RSS per run, preserving crash/lifecycle isolation across `ccd` restarts); or
  - **(B) Multiplexed single `cc-agent-host` supervisor** hosting multiple Claude child processes inside one Bun runtime (1× ~60 MiB instead of N× ~60 MiB).
- **Estimated gain:**
  - Option A: **-45 to -75 MiB RSS per concurrent Claude session** (**-450 to -750 MiB** at 10 sessions, **-2.2+ GiB** at 50 sessions!).
  - Option B: **-50+ MiB RSS per additional session beyond the first**.
- **Risks:**
  - Option A requires porting `@anthropic-ai/claude-agent-sdk` integration (or Claude CLI NDJSON protocol) to Rust and keeping pace with Claude Code CLI protocol changes.
  - Option B shares one Bun process fate across sessions (a crash affects all hosted Claude runs unless isolated).
- **Migration plan:**
  1. Profile real `cc-agent-host` RSS on macOS (`footprint`) and Linux (`smaps_rollup`) across 1 and 5 real Claude sessions to confirm exact baseline.
  2. Prototype Option A (or Option B) behind a config toggle (`claude_host_mode`) speaking the exact existing `mac/protocol/src/host.rs` UDS protocol so `ccd` and iPhone require zero protocol changes.

---

## P5 (F16). `CodeConnectKit` On-Demand Clock Ticker & Bounded `AppModel.diffs` Cache

- **Location:** `CodeConnectKit/Sources/CodeConnectKit/AppModel.swift:1887-1899` (and `diffs` dictionary)
- **Current state:**
  1. `AppModel.startTicking()` runs a `while !Task.isCancelled` loop sleeping 1 second forever and mutating `@Observable` `self.now = Date()` every second per connected machine, even when no view displays a ticking elapsed timer or stop-grey countdown.
  2. `AppModel.diffs` caches fetched `DiffState` responses per session UID without a size or count cap; inspecting large diffs across many sessions retains them in macOS/iOS app memory indefinitely.
- **Why not implemented here:** `CodeConnectKit` is shared directly with the out-of-scope `ios/` app, and no Swift compiler / macOS SDK is available in this Linux environment to verify Swift concurrency/observation semantics across both targets.
- **Estimated gain:**
  - Eliminates 1 Hz SwiftUI observation wakeups per connected machine (measurable on macOS via `powermetrics --samplers tasks`).
  - Caps macOS/iOS diff cache memory (e.g., LRU of last 10–16 sessions or 16 MiB total, re-fetching evicted diffs on demand from `ccd`).
- **Migration plan:**
  1. Run `startTicking()` only while at least one session has an active stop-grey window or elapsed timer visible (or switch timer views to SwiftUI's `TimelineView(.periodic(...))` so `AppModel` performs zero background ticks).
  2. Cap `AppModel.diffs` to the N most recently viewed sessions (evicting oldest entries when `diffs.count > 16`; evicted sessions re-fetch transparently from `ccd` on tab selection).
  3. Build and test both macOS and iOS targets in Xcode and measure with `bench/macos/run_macos_bench.sh`.

---

## P6 (F11). Custom Allocator Evaluation (`mimalloc` / `jemalloc`) Across All Linux Targets

- **Location:** `mac/ccd/Cargo.toml`, `mac/ccd/src/main.rs`
- **Current state:**
  - Shipped Linux release binaries target `musl` (`mallocng`), which keeps memory very tight (**5.50 MiB** idle, **8.58 MiB** at 50 sessions, **5.94 MiB** settled / **13.81 MiB** peak on `large_payloads`) and already hits **1,300+ ev/s** burst throughput after F1–F10.
  - Dynamically linked `gnu` builds use glibc `ptmalloc2`, which bloats RSS under transient large allocations and multi-thread load (24–35 MiB in baseline, improved by F6/F9's SQLite cap and thread cap, but glibc arenas still retain freed pages longer than `mallocng`).
- **Why not implemented here:** Per decision rules, an allocator swap may only be committed if verified to build and pass tests on *all* supported Linux targets (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, and `gnu`), and this environment lacks an `aarch64-linux-musl` C cross-compiler/linker (as well as offline cached allocator crates).
- **Estimated gain:**
  - On `gnu` builds: `mimalloc` (`MI_PURGE_DELAY=0`) could cut `large_payloads` and `burst` RSS by **-40% to -60%** while matching glibc CPU speed.
  - On `musl` builds: `mimalloc` could improve `burst` CPU throughput by **+10% to +20%**, at the cost of **+150–250 KiB binary size** and **+0.5–1.0 MiB higher idle RSS** (likely a net *loss* for the idle footprint goal!).
- **Migration plan:**
  1. In a CI matrix with both `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` C cross-toolchains, benchmark `mimalloc` (with `MI_PURGE_DELAY=0`) vs default `mallocng` using `./bench/run.sh`.
  2. Only adopt for `gnu` (e.g., `#[cfg(all(target_os = "linux", target_env = "gnu"))]`) if glibc Linux builds are ever shipped; keep `mallocng` on `musl` if idle RSS regresses.

---

## P7. Optional SQLite `auto_vacuum = INCREMENTAL` & Periodic `PRAGMA incremental_vacuum`

- **Location:** `mac/ccd/src/store.rs`
- **Current state:** Deleting or pruning ended sessions removes rows from SQLite and releases in-memory page cache (`PRAGMA shrink_memory` from F6), but SQLite keeps freed database pages on a freelist inside `events.db` rather than shrinking the file on disk.
- **Proposal:** Enable `PRAGMA auto_vacuum = INCREMENTAL` on newly created databases (or via an explicit `codeconnect sessions compact` CLI subcommand running `VACUUM` / `PRAGMA wal_checkpoint(TRUNCATE)`), allowing operators to reclaim disk space after bulk prunes without altering existing schemas.
- **Estimated gain:** Reclaims 100% of deleted session disk space and reduces WAL/DB file size on long-lived installations.
- **Risks:** Changing `auto_vacuum` on an existing database requires a one-time full `VACUUM` (locking the DB momentarily and temporarily requiring 2× free disk space); `auto_vacuum = INCREMENTAL` can increase B-tree fragmentation slightly on random deletes.
- **Migration plan:**
  1. Add `codeconnect sessions compact` running `PRAGMA wal_checkpoint(TRUNCATE); VACUUM;` on demand (zero risk to existing databases).
  2. Optionally set `PRAGMA auto_vacuum = INCREMENTAL` on freshly initialized databases (`user_version == 0`) and run `PRAGMA incremental_vacuum(64)` after `prune_exited_sessions`.
