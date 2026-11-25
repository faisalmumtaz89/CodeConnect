# CodeConnect Performance & Memory Findings (Ranked)

> **Phase 2 deliverable** — ranked by `impact × frequency` using baseline measurements (`perf/BASELINE.md`), `heaptrack`, `valgrind --tool=massif`, `/proc/<pid>/smaps`, and full codebase review (Rust workspace + `mac/agent-host` + `macos/` SwiftUI GUI + `CodeConnectKit`).

Status values: `todo` | `done` | `rejected` | `proposal` (requires user approval per decision rules).

---

## Tier 1 — High Impact, Low Risk (Structural Fixes)

### F1. Whole-binary `std::fs::read(&exe)` at startup spikes Peak RSS by +4.5–6 MiB (+30 MiB on unstripped builds)
- **Location:** `mac/ccd/src/main.rs:113-120`
- **Evidence:**
  - `valgrind --tool=massif` snapshot 8 (peak): `std::fs::read(&exe)` accounts for **96.69% of peak heap** (`30,143,400 B` of `31,176,600 B`).
  - `heaptrack` #1 Peak Memory Consumer: `30.14 MiB` from `ccd::main` line 114.
  - Shipped `musl` `cold_idle`: **Peak RSS = 10.78 MiB** vs settled **6.30 MiB** (+4.48 MiB startup spike just to SHA-256 the 6.0 MiB `ccd` binary!).
- **Proposed fix:** Add a streaming `protocol::hash::sha256_file_hex(path: &Path)` helper using a fixed 64 KiB stack/heap buffer and `Sha256::update`, and call it from `main.rs:114` (and any other whole-file binary hashers).
- **Expected gain:** **-4.5 to -5.5 MiB Peak RSS** on `cold_idle`, `idle_client`, `concurrent_1/10/50`, and `reconnect_churn` (~40–45% Peak RSS reduction!), lower allocator fragmentation, faster cold start.
- **Risk:** Very low (identical SHA-256 hex digest; verify with unit test against `sha256_hex`).
- **Status:** `done` (Peak RSS `-41.2%` idle / `-30.6%` 10-session; see `perf/CHANGELOG.md`)

### F2. `protocol::proc_identity::read_stat` allocates ~50 `String`s per PID + `codex_host` runs `TOOL_SCAN` synchronously on Tokio worker threads every 1 s per active Codex session
- **Location:** `mac/protocol/src/proc_identity.rs:254-292, 526-542`; `mac/ccd/src/codex_host.rs:773-777, 986-993`
- **Evidence:**
  - `heaptrack` **#1 allocation call site in the entire daemon**: `88,886+` heap allocations (>25% of all allocations!) from `read_stat` -> `fields.split_whitespace().map(str::to_owned).collect::<Vec<String>>()`.
  - `codex_host.rs:777`: `_ = scan.tick(), if running.is_some() => record_tools(host.as_ref(), &mut tools)` runs **synchronously on a Tokio worker thread**, walking the entire `/proc` directory (`O(total_system_pids)`) once per second **per active Codex session** (`25×/s` with 25 Codex sessions in `concurrent_50`!), blocking async WS/IPC I/O and driving `concurrent_50` wakeups to `473/s` and CPU to `6.43%`.
  - `tools: Vec<ProcessIdentity>` never prunes exited groups during long-lived turns/sessions.
- **Proposed fix:**
  1. Rewrite `protocol::proc_identity::read_stat` on Linux to parse into a small `StatFields { state: u8, ppid: i32, pgid: i32, starttime_ticks: i64 }` using a stack `[u8; 1024]` read buffer and cached `sysconf(_SC_CLK_TCK)` — **zero heap allocations per PID**.
  2. In `protocol::proc_identity::process_table()`, avoid `format!("/proc/{pid}/stat")` heap strings (stack path buffer or `openat` on `/proc`) and add a batched `tool_groups_for_many(groups: &[i32])` or shared short-TTL snapshot cache (`500 ms`) run via `spawn_blocking` so N active Codex sessions share **1** `/proc` scan per second off the Tokio worker threads.
  3. Prune dead tool groups from `tools` when the recorded leader is no longer live (while preserving B3's warrant invariant for groups that still have members!). *Conservative check:* if a leader died while its group still has members, `group_has_members` is true — so only prune when `kill(-tool.pid, 0)` returns `ESRCH` (zero members left in the group).
- **Expected gain:** **-25% to -35% total allocation rate**, lower CPU & wakeups under active Codex sessions (`concurrent_10`, `concurrent_50`, `soak`, `burst`), zero async-worker stalls.
- **Risk:** Low (covered by existing `proc_identity` & `codex_host` tests + new unit tests for zero-alloc parser and `ESRCH` pruning).
- **Status:** `done` (`concurrent_50` CPU `-15.9%`, `burst` throughput `+13.5%`, `relay_p50` `-15.9%`; see `perf/CHANGELOG.md`)

### F3. `Daemon::sessions()` N+1 SQLite query & `spawn_blocking` storm + missing `prepare_cached`
- **Location:** `mac/ccd/src/state.rs:3392-3476`; `mac/ccd/src/store.rs` (all hot queries call `conn.prepare` instead of `conn.prepare_cached`)
- **Evidence:**
  - `sessions_rpc_p50_ms` scales linearly: `0.92 ms` (1 session) -> `4.49 ms` (10 sessions) -> **`13.94 ms` (`p95 16.99 ms`)** at 50 sessions.
  - A single `sessions` RPC with 50 sessions executes **~150–200 sequential `spawn_blocking` thread-pool hops** and individual SQLite queries (`max_seq` × 50, `hosted_models` × 50 + 50 JSON parses, `newest_model_receipt` × 25, `relaunchable` × E, `filed_event` × K).
  - `heaptrack`: `23,467` `sqlite3MemMalloc` calls from re-preparing SQL statements on every query instead of `prepare_cached`.
  - Drives `concurrent_50` peak threads to **40 (`musl`) / 61 (`gnu`)** and `reconnect_churn` wakeups to **11,248/s** (`cycle_p50 = 8.91 ms`).
- **Proposed fix:**
  1. Collapse all DB reads in `Daemon::sessions()` into a single `Store::sessions_snapshot(cards)` method called via **1** `spawn_blocking` hop, using batched SQL queries (`SELECT session_uid, MAX(seq) FROM events GROUP BY session_uid`, single `SELECT` over `run_hosts` for `models` & `relaunchable`, single query for `newest_model_receipt` & open card `filed_event`s).
  2. Switch hot-path queries in `store.rs` (`append_event`, `append_host_message`, `events_after`, `events_before`, `max_seq`, `device_lookup`, `list_sessions`, `sessions_snapshot`, etc.) from `conn.prepare` to `conn.prepare_cached`.
- **Expected gain:** **-60% to -80% `sessions_rpc` latency** at 50 sessions (`14 ms -> ~2–4 ms`), major drop in `reconnect_churn` CPU/latency/wakeups, fewer `spawn_blocking` threads.
- **Risk:** Low (pure read batching; exact same `SessionSummary` output; add unit tests verifying snapshot equivalence).
- **Status:** `done` (`concurrent_50` `sessions_rpc` `-76%`, wakeups `-32%`; `burst` CPU `-10%`; `reconnect_churn` `-21%` latency; see `perf/CHANGELOG.md`)

### F4. Double JSON parse/serialize in WebSocket `subscribe` replay & `history` paging + unbounded per-page memory
- **Location:** `mac/ccd/src/store.rs:1675-1728`; `mac/ccd/src/ws_server.rs:46, 1754-1788, 1840-1910`
- **Evidence:**
  - `large_payloads`: **Peak RSS = 18.45 MiB (`musl`) / 34.60 MiB (`gnu`)**, **`history_p50 = 79.38 ms` (`p95 84.88 ms`)**, **CPU = 75.26%**.
  - `Store::events_after` and `Store::events_before` read up to `REPLAY_PAGE = 500` rows from SQLite, parse EVERY JSON `payload` TEXT column into a full `serde_json::Value` heap DOM tree (500 × 64–512 KiB = tens of MB!), return `Vec<Event>`, and `ws_server` immediately re-serializes every `Event` back to JSON text to send over the WebSocket!
- **Proposed fix:**
  1. Add `raw_value` feature to `serde_json` (zero new crates) and introduce an internal `RawEvent` (identical wire JSON to `Event`, carrying `payload: Box<serde_json::value::RawValue>` directly from SQLite's validated `payload` TEXT column) plus `Store::raw_events_after` / `Store::raw_events_before` (with a byte budget per internal fetch chunk for `Backlog` so a single replay page never loads >4 MiB into RAM at once — while still streaming all events in exact seq order with zero protocol change!).
  2. For `History` (`events_before`), stream raw JSON frames without ever constructing `serde_json::Value` DOM trees.
- **Expected gain:** **-50% to -70% `large_payloads` `history_p50` latency** (`79 ms -> ~25 ms`), **-40% to -60% `large_payloads` Peak RSS** (`18.5 MiB -> ~9–11 MiB` on `musl`, `34.6 MiB -> ~14 MiB` on `gnu`), lower `reconnect_churn` CPU.
- **Risk:** Low (wire bytes are byte-for-byte valid identical JSON objects; validate with round-trip serialization tests against `ServerMessage::Event`).
- **Status:** `done` (`large_payloads` `history_p50` `-45.3%`, settled RSS `-20.8%`; `burst` `+33.7%` ev/s; `reconnect_churn` `-31.1%` cycle latency; see `perf/CHANGELOG.md`)

### F5. `broadcast::Sender<Event>` (1024 slots) pins large payloads, allocates 246 KiB permanently, and clones `Event` / `PendingEvent` unnecessarily
- **Location:** `mac/ccd/src/state.rs:56, 110, 946, 951, 991`; `mac/ccd/src/ws_server.rs`
- **Evidence:**
  - `heaptrack` #3 Peak Consumer: `245.76 KiB` (`1024 × ~240 B`) allocated permanently at startup for `events_tx: broadcast::Sender<Event>`.
  - During `large_payloads`, up to 1024 `Event` structs (each holding up to 64–512 KiB `serde_json::Value`) remain pinned inside the broadcast ring even after clients finish receiving them, driving `large_payloads` settled `anon_mmap` to `6.79 MiB` and `soak` Peak RSS to `19.03 MiB`.
  - `state.rs:946` calls `self.db.append_event(pending.clone())` even though `pending` is never used afterward (full deep clone of every ingested event!).
  - `state.rs:951, 991` calls `self.events_tx.send(event.clone())` even when `receiver_count() == 0`, AND clones per subscriber instead of sharing `Arc<Event>`.
- **Proposed fix:**
  1. Remove the redundant `.clone()` in `state.rs:946` (`append_event(pending)`).
  2. Change `events_tx` to `broadcast::Sender<Arc<Event>>` (so each slot is 8 bytes + broadcast metadata instead of 240 bytes, and subscribers share one `Arc<Event>` with zero deep cloning!).
  3. Only send to `events_tx` when `self.events_tx.receiver_count() > 0` (when no subscriber is attached, nothing is pinned in the ring; when a subscriber later attaches, it replays from SQLite via `subscribe`).
  4. Reduce `BROADCAST_CAPACITY` from `1024` to `256` (still handles huge bursts; any subscriber lagging >256 events cleanly triggers existing `Resync` from SQLite — verify all `ws_server` lag tests pass!).
- **Expected gain:** **-200 KiB permanent idle RSS**, **-2 to -5 MiB settled & peak RSS** after `large_payloads`/`burst`/`soak`, lower `burst` and `large_payloads` CPU and relay latency.
- **Risk:** Low (check all tests referencing `events_tx` or `BROADCAST_CAPACITY`; keep `Resync` behavior identical).
- **Status:** `done` (`large_payloads` `relay_p50` `-15.2%`, settled RSS `6.84 MiB`; `burst` `+40.6%` vs baseline; see `perf/CHANGELOG.md`)

### F6. SQLite connection pool over-provisioning & uncapped page caches + lookaside memory
- **Location:** `mac/ccd/src/store.rs:59, 1283-1293, 5696-5710`
- **Evidence:**
  - `massif` snapshot 9 & `heaptrack`: `sqlite3MemMalloc` is **713 KiB at startup** (`240 KiB` in `setupLookaside` across 5 eager connections: 1 writer + 4 readers, plus `340 KiB` initial `pcache1Alloc`).
  - Default SQLite `cache_size = -2000` (2 MiB per connection) × 5 connections = **up to 10 MiB** of duplicated page caches during long `soak` runs (`+2.64 MiB/hr` growth observed in 500 s soak as all 5 connections warm their page caches on growing WAL!).
  - Every `Store::open` eagerly opens 4 reader connections even when idle (consuming 4 extra FDs + 4 × lookaside + 4 × schema caches + 4 × page caches).
- **Proposed fix:**
  1. Reduce `READERS` from `4` to `2` (WAL allows concurrent reads; 2 readers + 1 writer handles 50 sessions cleanly while halving idle SQLite reader memory and FDs).
  2. Set `PRAGMA cache_size = -512` (512 KiB per connection = 1.5 MiB total ceiling instead of 10 MiB!) and `PRAGMA mmap_size = 0` (keeps RSS bounded and predictable; OS page cache already caches the DB file cleanly without inflating process RSS).
  3. Configure smaller SQLite lookaside (`SQLITE_DBCONFIG_LOOKASIDE`: e.g., 64 bytes × 128 slots = 8 KiB instead of 48 KiB per connection -> saves ~200 KiB immediately) via `rusqlite` or `PRAGMA shrink_memory` after `prune_ended_sessions` / periodic idle cleanup.
- **Expected gain:** **-0.5 to -1.0 MiB idle RSS**, **major reduction in `soak` RSS growth slope** (`+2.64 MiB/hr` -> near `0 MiB/hr`), -2 FDs.
- **Risk:** Low (WAL concurrency preserved; benchmark `concurrent_50`, `burst`, `large_payloads`, `reconnect_churn` to confirm zero latency regression).
- **Status:** `done` (F6: `cold_idle` `6.29 -> 5.63 MiB`, `large_payloads` Peak `18.84 -> 13.81 MiB` [-26.7%], `burst` `9.26 -> 7.14 MiB` [-22.9%], `reconnect_churn` `8.33 -> 6.38 MiB` [-23.4%], FDs `-4` to `-6`)

### F7. Unbounded in-memory session maps (`session_facts`, `tree_facts`, `Starts`) leak over long daemon lifetimes
- **Location:** `mac/ccd/src/state.rs:339, 343, 4746-4860`; `mac/ccd/src/start_session.rs:586-615`
- **Evidence:**
  - `Inner::session_facts` and `Inner::tree_facts` are inserted on every session/turn and **never removed** when sessions are deleted (`delete_session`) or pruned (`prune_ended_sessions`).
  - `start_session::Starts` holds every `request_id -> watch::Receiver<Option<StartSessionResult>>` forever in memory (`HashMap` never pruned/bounded!), even after the start finished and is durably persisted in SQLite (`run_hosts.start_request_id`).
  - Contributed to `soak` growth (`+2.64 MiB/hr` across 24 churn sessions in 500 s).
- **Proposed fix:**
  1. Evict `session_facts` and `tree_facts` on `delete_session` and `prune_ended_sessions`, and evict `tree_facts` when a session exits (`end_hosted_run` / `SessionEnd` — git working-tree facts for exited sessions can be re-read on demand if ever needed, or kept only for live sessions; check `SessionSummary` tests first so no exited-session field regresses!). *Conservative rule:* definitely evict both on `delete_session` and `prune_ended_sessions`, and shrink maps if vastly over-allocated.
  2. Bound `Starts` (`start_session.rs:586`) to completed entries using a small bounded ring/LRU (e.g., retain all in-flight starts + last `256` completed request IDs, since SQLite `run_hosts.start_request_id` already backs durable idempotency for older retries — verify with `start_session` tests!).
- **Expected gain:** Eliminates monotonic memory growth over days/weeks of session churn; contributes to ~0 MiB/hr soak slope.
- **Risk:** Low (covered by new unit tests for `delete_session`, `prune_ended_sessions`, and `Starts` bounding).
- **Status:** `done` (F7: evicted `session_facts`/`tree_facts` on delete & prune, bounded `Starts` to 256 completed entries; `soak` settled/Peak `10.72/19.03 -> 7.59/8.51 MiB`, start->end `6.95 -> 6.85 MiB`)

### F8. Eager outbound TLS root CA loading at startup (`relay_sender` & `apns_sender`)
- **Location:** `mac/ccd/src/relay_sender.rs:116-134`; `mac/ccd/src/apns_sender.rs:59-66`; `mac/ccd/src/tls.rs:81-92`
- **Evidence:**
  - `apns::build(&config, store)` runs at startup (`main.rs:92`) and calls `HttpsRelay::new()` -> `tls::outbound_h2("relay")` -> `rustls_native_certs::load_native_certs()`, reading and parsing all ~150 OS root CA certificates from `/etc/ssl/certs/ca-certificates.crt` (or macOS Keychain!) into a `RootCertStore` on heap even if push is never triggered or zero devices are registered.
  - Adds both startup latency (~10–25 ms) and ~300–600 KiB permanent idle heap RSS.
- **Proposed fix:** Initialize `TlsConnector` lazily on first push delivery via `std::sync::OnceLock` (or `Mutex<Option<TlsConnector>>` so a transient failure can retry), keeping exact error reporting when actually delivering a push. *(Verify no existing unit test asserts eager failure in `HttpsRelay::new()` / `ApnsPushSender::new()` — check tests first!)*
- **Expected gain:** **-0.3 to -0.6 MiB idle RSS**, **-10 to -25 ms startup latency**.
- **Risk:** Low (push delivery still validates full system trust anchors identically; zero security weakening).
- **Status:** `done` (F8: `LazyOutboundH2` in `tls.rs`, `relay_sender.rs`, `apns_sender.rs`; `cold_idle` `RssAnon` `1.008 -> 0.918 MiB` [-8.9%, -47.9% vs baseline], total `5.59 MiB`)

### F9. Unnecessary idle wakeups & subprocess spawns (`mail_sweep`, `ws_server` `peers_refresh`, Tokio runtime thread pool caps)
- **Location:** `mac/ccd/src/mail.rs:1022-1030`; `mac/ccd/src/ws_server.rs:906-922`; `mac/ccd/src/main.rs:54`
- **Evidence:**
  - `cold_idle` has **0.50 wakeups/sec** and **6–7 threads** (even with zero sessions and zero clients!); `idle_client` has **0.67 wakeups/sec**; `concurrent_50` spawns **40 (`musl`) / 61 (`gnu`) threads**!
  - `mail.rs:1022`: `run_mail_sweeps` calls `daemon.read_tailnet().await` (spawning `tailscale status --json`!) **every 30 s unconditionally**, even when the SQLite mail table has zero unsent/untold cross-computer messages! Plus `tokio::time::interval` fires immediately at `t = 0`.
  - `ws_server.rs:906-922`: Every connected WS client independently spawns `tailscale status --json` every 60 s AND serializes `peers` JSON **twice** (`serde_json::to_string(&frame)` for diffing, then `send(&mut sink, &frame)` re-serializes it!).
  - `main.rs:54`: `#[tokio::main]` defaults to `num_cpus` worker threads (10–16+ on Apple Silicon Macs, up to 64+ on Linux servers) and `max_blocking_threads = 512` (allowing 40–61 blocking threads to spawn during bursts, each with an OS thread stack and glibc malloc arena!).
- **Proposed fix:**
  1. In `mail::run_mail_sweeps`, add a cheap `store.has_pending_mail()` check (or skip `read_tailnet()` when `unsent_mail` and `untold_mail` are empty), and skip the redundant immediate tick (`interval_at(Instant::now() + SWEEP_EVERY, SWEEP_EVERY)`).
  2. Cache `daemon.read_tailnet()` with a short shared coalesced cache (e.g., 10 s TTL with single-flight dedup) so multiple clients + mail sweeps never spawn parallel `tailscale status --json` subprocesses, and avoid double-serializing `peers_frame` in `ws_server.rs:916-922` (send the already-serialized `json` string directly!).
  3. Build the Tokio runtime explicitly in `main.rs` with bounded `worker_threads(num_cpus.clamp(2, 4))`, `max_blocking_threads(16)`, and `thread_keep_alive(Duration::from_secs(10))` so idle blocking threads exit cleanly and multi-core Macs/servers don't allocate 16+ idle worker threads + stacks + arenas!
- **Expected gain:** **-50% to -80% idle wakeups**, **zero idle `tailscale` subprocess spawns**, **peak threads capped <= 16–20** (from 40–61 -> huge glibc arena savings on Linux & stack savings on macOS!).
- **Risk:** Low (preserve all mail delivery, tailnet peer refresh, and test semantics).
- **Status:** `done` (F9: `cold_idle` wakeups `0.500 -> 0.188/s` [-62.4%], RSS `5.50 MiB`; `concurrent_50` threads `41 -> 19` [-53.7%], Peak RSS `8.77 MiB`)

---

## Tier 2 — Medium Impact (Dependencies, Allocator, Build Profile, `agent-host`)

### F10. Unused crate features (`tokio-rustls` `logging`, `tokio` `fs`)
- **Location:** `mac/Cargo.toml:39-57`
- **Evidence:** `ccd` uses a custom logger (`mac/ccd/src/log.rs`) and never installs the `log` facade; `tokio-rustls` enables `"logging"` (compiling log formatting strings across rustls state machines into the 5.97 MiB binary). Check if `tokio` `"fs"` is used anywhere in `mac/`.
- **Proposed fix:** Remove `"logging"` from `tokio-rustls` (and `rustls` if unused by `push-relay`) and remove `"fs"` from `tokio` if unused across workspace crates. Measure binary size delta.
- **Expected gain:** ~30–100 KiB smaller `ccd` binary (reduces file-backed RSS directly since `.text` is 4.54 MiB of the 6.30 MiB idle RSS!).
- **Risk:** Very low (compile-time checked).
- **Status:** `done` (F10: removed `tokio/fs` and `tokio-rustls/logging`; `ccd` musl `-p ccd` binary `6,300,320 -> 6,279,264 B` [-20.6 KiB])

### F11. Allocator evaluation (separate commit, measured on `musl` and `gnu`)
- **Location:** `mac/ccd/Cargo.toml`, `mac/ccd/src/main.rs`
- **Evidence:**
  - `musl` (`mallocng`) keeps RSS tight (`6.30 MiB` idle, `8.84 MiB` `large_payloads`) but is ~15–20% slower on CPU (`burst` `925 vs 1,138 ev/s`, `large_payloads` `history_p50 79 ms vs 45 ms`).
  - `gnu` (`ptmalloc2`) is faster on CPU but bloats to **34.6 MiB** on `large_payloads` and **19.8 MiB** on `burst` due to per-thread arenas.
- **Proposed fix:** After code optimizations are complete, benchmark `mimalloc` or `jemalloc` (or `musl` default `mallocng` vs `mimalloc` with `MI_PURGE_DELAY=0`) on its own dedicated commit, verifying clean builds on `x86_64-unknown-linux-musl` AND `aarch64-unknown-linux-musl` (if cross-linker is available). Keep only if it improves RSS and/or CPU without regressing binary size or idle RSS beyond threshold; otherwise revert and document.
- **Expected gain:** Potentially +15–25% throughput on `musl` or -50% RSS on `gnu`, at the cost of ~100–200 KiB binary size and ~0.5–1 MiB baseline RSS (must measure carefully!).
- **Risk:** Medium (C build / musl / aarch64 cross-compile compatibility; evaluate strictly per decision rules).
- **Status:** `todo`

### F12. `mac/agent-host/src/link.ts` quadratic line slicing on multi-line chunks
- **Location:** `mac/agent-host/src/link.ts:115-122`
- **Evidence:** `read()` does `rest = rest.slice(end + 1)` inside a loop for every newline in a chunk (`O(k²)` copies on bursty chunks).
- **Proposed fix:** Track `start = 0` and iterate `text.indexOf('\n', start)`, slicing only each individual line and returning `text.slice(start)` once at the end.
- **Expected gain:** Eliminates quadratic string copies during bursty host IPC chunks (unmeasured in `ccd` harness since mock host is Rust; covered by `mac/agent-host/test/`).
- **Risk:** Very low.
- **Status:** `done` (F12: index-based scan in `mac/agent-host/src/link.ts`; 10k-line chunk scan `2.19 -> 1.20 ms` [1.8× faster] in Bun)

---

## Tier 3 — macOS GUI & `CodeConnectKit` (Code Review Only — **Unmeasured on macOS**)

### F13. `FileDiffView` uses eager `VStack` instead of `LazyVStack` for diff lines *(unmeasured on macOS)*
- **Location:** `macos/CodeConnect/Views/DiffView.swift:68`
- **Evidence:** Code review: `FileDiffView` builds a non-lazy `VStack` over `ForEach(rows)` inside `ChangesTab`'s `ScrollView`. Opening a large file diff (thousands of lines) eagerly instantiates every `DiffLine` view and `Text` layout upfront, spiking macOS app memory and UI hitching.
- **Proposed fix:** Change line 68 `VStack(alignment: .leading, spacing: 0)` to `LazyVStack(alignment: .leading, spacing: 0)` (verify `ChangesTab.swift` hosts it in a `ScrollView` and check approval-card inline usage where `lineLimit` is small — `LazyVStack` works identically in both).
- **Expected gain:** Major macOS GUI memory & render-latency reduction on large diffs (**unmeasured on macOS**).
- **Risk:** Very low (SwiftUI standard drop-in; visually identical).
- **Status:** `done` (**unmeasured on macOS** — `DiffView.swift:68` switched to `LazyVStack`)

### F14. `TableBlock` redundantly re-parses Markdown cells 3× inside `ViewThatFits` on every SwiftUI body pass *(unmeasured on macOS)*
- **Location:** `macos/CodeConnect/Views/AgentText.swift:150-200`
- **Evidence:** Code review: `TableBlock.body` uses `ViewThatFits`, which evaluates both `grid(sized: true)` (calling `shortColumns` + `needed`, which calls `shortColumns` again + `Self.width` on every cell) AND `grid(sized: false)` (calling `shortColumns` a third time!). Every `shortColumns` call parses inline Markdown (`AgentProse.inline(cell)`) for every cell in the table on every layout/body update.
- **Proposed fix:** Compute `let short = shortColumns` and `let minNeeded = needed(short: short)` once in `body` (or precompute in `init(table:)`), passing `short` into `grid(sized:short:)` so Markdown parsing happens once instead of 3× per pass.
- **Expected gain:** ~66% less CPU and transient `AttributedString` allocations rendering Markdown tables (**unmeasured on macOS**).
- **Risk:** Very low (pure local refactor inside private `TableBlock`).
- **Status:** `done` (**unmeasured on macOS** — `AgentText.swift` computes `shortColumns` and `needed(short:)` once per body pass instead of 4×/2×)

### F15. `Sidebar` polls `LocalDaemon.address(root:)` over Unix socket every 5 s forever, even while occluded by Settings *(unmeasured on macOS)*
- **Location:** `macos/CodeConnect/Views/MainWindow.swift:631-639`
- **Evidence:** Code review: `.task` spawns a detached thread + Unix socket IPC round-trip every 5 seconds forever, waking both the macOS app and `ccd` 12×/minute even when the user is idle or viewing Settings.
- **Proposed fix:** Back off the polling interval once an address is known (e.g., 5 s while unresolved/on loopback, 30 s once bound to a non-loopback tailnet address) — reduces idle macOS GUI + `ccd` IPC wakeups by 6× without delaying startup Tailscale detection.
- **Expected gain:** -83% steady-state macOS sidebar IPC wakeups (**unmeasured on macOS**).
- **Risk:** Very low (still polls every 5 s whenever disconnected/on loopback; 30 s steady-state check).
- **Status:** `done` (**unmeasured on macOS** — `MainWindow.swift` and `TailscaleNote.swift` back off to 30 s once non-loopback address settles)

### F16. `AppModel.startTicking()` mutates `@Observable` `self.now` every 1 s forever + unbounded `AppModel.diffs` cache *(unmeasured on macOS)*
- **Location:** `CodeConnectKit/Sources/CodeConnectKit/AppModel.swift:1887-1899`
- **Evidence:** Code review: `startTicking()` wakes every 1 second per connected machine, mutating `@Observable` `self.now = Date()`, triggering observation notifications even when no view reads `now` or no Codex stop-grey timer is active. Also `AppModel.diffs` holds every fetched diff without eviction.
- **Proposed fix:** Because `CodeConnectKit` is shared with `ios/` (out of scope) and no Swift compiler is available on this Linux VM to verify cross-target Swift type-checking, record `AppModel.startTicking()` and `AppModel.diffs` bounding in `perf/PROPOSALS.md` rather than risking an unverified Swift signature/observation change in the shared package.
- **Expected gain:** Eliminates 1 Hz SwiftUI observation wakeups per machine + bounds diff cache memory.
- **Risk:** Medium without a Swift compiler on Linux -> moved to `proposal`.
- **Status:** `proposal`

---

## Tier 4 — Higher-Impact Structural Proposals (Out of Scope Without Approval -> `perf/PROPOSALS.md`)

### P1. Replace `tokio` multi-thread runtime with `current_thread` (or single-threaded epoll + small blocking pool)
- **Status:** `proposal` (major runtime architecture change; also evaluate carefully against concurrent WS/IPC load).

### P2. Compress large `events.payload` blobs (zstd/lz4) in SQLite and/or on the wire
- **Status:** `proposal` (changes on-disk SQLite format / wire protocol).

### P3. Replace `serde_json` DOM payloads inside `protocol::Event` with `Box<RawValue>` across the protocol crate
- **Status:** `proposal` (public Rust API change across crates; F4 captures the bulk of the benefit internally without changing public types!).

### P4. Shared/native Rust Claude host replacing per-run Bun `cc-agent-host` (~50–80 MiB RSS per Claude session!)
- **Status:** `proposal` (major architectural change; note that each live Claude run spawns a Bun runtime `cc-agent-host`, which dominates *total system* RAM alongside `ccd`).
