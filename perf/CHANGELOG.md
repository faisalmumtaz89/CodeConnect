# CodeConnect Optimization Changelog

Each entry corresponds to one logical commit on `perf/baseline-and-optimizations`, with reproducible before/after numbers measured on the same machine (`x86_64-unknown-linux-musl` release binary, 3 runs median unless noted).

---

## F1 — Stream executable SHA-256 instead of reading whole binary into heap (`protocol::hash::sha256_file_hex`)

- **Files:** `mac/protocol/src/hash.rs`, `mac/ccd/src/main.rs`, `mac/codeconnect/src/service.rs`
- **Change:** Added `protocol::hash::sha256_file_hex` streaming files in 64 KiB stack chunks, replacing `std::fs::read(&exe)` in `ccd` startup and `codeconnect daemon status` staleness check. Added unit test `sha256_file_hex_matches_in_memory_digest`.
- **Before vs After (`x86_64-unknown-linux-musl`, 3 runs median):**
  - `cold_idle` Peak RSS: `10.78 MiB` -> **`6.34 MiB`** (**-4.44 MiB / -41.2%**)
  - `idle_client` Peak RSS: `10.89 MiB` -> **`6.36 MiB`** (**-4.53 MiB / -41.6%**)
  - `concurrent_10` Peak RSS: `10.64 MiB` -> **`7.38 MiB`** (**-3.26 MiB / -30.6%**)
  - `cold_idle` Startup: `114.5 ms` -> **`100.5 ms`** (**-12.2%**)
  - `idle_client` Startup: `109.3 ms` -> **`99.7 ms`** (**-8.8%**)
  - Settled RSS / CPU / binary size: unchanged (`6.31 MiB` / `0.00%` / `5.969 MiB`).

---

## F2 — Zero-allocation `/proc/<pid>/stat` parser + off-runtime shared `ProcessTable` scan (`codex_host`)

- **Files:** `mac/protocol/src/proc_identity.rs`, `mac/ccd/src/codex_host.rs`
- **Change:**
  - Replaced heap-allocating `Vec<String>` `/proc/<pid>/stat` parser (`88,886+` allocations in baseline `heaptrack`) with a zero-allocation stack-buffer parser (`StatFields`) + cached `CLK_TCK` + small-`Vec` member check in `led_by_members`.
  - Exposed `protocol::proc_identity::ProcessTable` snapshot; offloaded `codex_host::record_tools_async` onto `tokio::task::spawn_blocking` with a shared 500 ms TTL snapshot cache for periodic `TOOL_SCAN` ticks (initial, `turn/completed`, and `Stop` scans remain fresh), skipped immediate duplicate tick 0, and bounded exited tool groups above 32 entries.
  - Added unit test `parse_stat_line_handles_spaces_and_parens_in_comm`.
- **Before vs After (`x86_64-unknown-linux-musl`, 3 runs median, cumulative vs baseline):**
  - `concurrent_50` CPU: `6.43%` -> **`5.41%`** (**-15.9%**)
  - `concurrent_50` `relay_p50_ms`: `952.34 ms` -> **`813.24 ms`** (**-14.6%**)
  - `concurrent_50` Peak RSS: `10.76 MiB` -> **`9.92 MiB`** (**-7.8%**)
  - `burst` throughput: `924.66 ev/s` -> **`1,049.88 ev/s`** (**+13.5%**)
  - `burst` `relay_p50_ms` / `p95`: `810.53 / 957.99 ms` -> **`681.59 / 793.54 ms`** (**-15.9% / -17.2%**)
  - `concurrent_10` CPU: `1.35%` -> **`1.27%`** (**-5.9%**)

---

## F3 — Batched `Store::sessions_snapshot` + `prepare_cached` for SQLite hot paths

- **Files:** `mac/ccd/src/store.rs`, `mac/ccd/src/db.rs`, `mac/ccd/src/state.rs`
- **Change:**
  - Collapsed `Daemon::sessions()` (~150–200 sequential `spawn_blocking` hops and SQLite queries at N=50) into a single `Store::sessions_snapshot` blocking read holding one reader connection (single `run_hosts LEFT JOIN accounts` query for switching/accounts/models, single `GROUP BY` query for `max_seq`, SQL-prefiltered `INSTR(...)` model receipt lookup, and inline cached queries for open cards and exited relaunchables).
  - Switched hot-path SQLite statements (`append_in_tx`, `events_after`, `events_before`, `max_seq`, `filed_event`, `count_events`, `list_sessions`, `device_by_token_hash`, `device_is_active`, `sessions_snapshot`) from `prepare`/`query_row`/`execute` to `prepare_cached`.
- **Before vs After (`x86_64-unknown-linux-musl`, 3 runs median, vs baseline / previous step F2):**
  - `concurrent_50` `sessions_rpc_p50_ms` / `p95`: `13.94 / 16.99 ms` -> **`3.34 / 3.87 ms`** (**-76.0% / -77.2%**, **4.2× faster!**)
  - `concurrent_10` `sessions_rpc_p50_ms`: `4.49 ms` -> **`1.05 ms`** (**-76.6%**, **4.3× faster!**)
  - `concurrent_50` Wakeups/s: `473.4/s` -> **`320.0/s`** (**-32.4%**); CPU: `6.43%` (F2: `5.41%`) -> **`4.93%`** (**-23.3%** vs baseline)
  - `concurrent_10` Wakeups/s: `138.4/s` -> **`104.6/s`** (**-24.4%**); CPU: `1.35%` -> **`1.12%`** (**-17.0%**)
  - `burst` CPU: `72.73%` (F2: `73.27%`) -> **`65.38%`** (**-10.1%**); `relay_p50_ms`: `810.5 ms` -> **`614.4 ms`** (**-24.2%**); throughput: `924.7` -> **`1,072.1 ev/s`** (**+15.9%**)
  - `reconnect_churn` `cycle_p50_ms` / `p95`: `8.91 / 11.85 ms` -> **`7.04 / 9.22 ms`** (**-21.0% / -22.2%**); CPU: `61.34%` -> **`54.37%`** (**-11.4%**); Wakeups/s: `11,248` -> **`8,110/s`** (**-27.9%**)

---

## F4 — Zero-DOM raw JSON replay & history paging with 4 MiB chunk byte budget (`RawEventFrame`)

- **Files:** `mac/Cargo.toml`, `mac/ccd/src/store.rs`, `mac/ccd/src/db.rs`, `mac/ccd/src/ws_server.rs`
- **Change:**
  - Enabled built-in `serde_json` `raw_value` feature (zero new crates) and added `RawEventFrame` + `Store::raw_event_frames_after` (4 MiB soft byte budget per chunk) + `Store::raw_event_frames_before`, serializing `{"type":"event","event":{...}}` directly from SQLite's JSON TEXT column without building intermediate `serde_json::Value` DOM trees.
  - Updated `ws_server::Backlog` and `ws_server::History` to stream `RawEventFrame` via `send_raw_event_frame` (preserving exact `MAX_CLIENT_MESSAGE_BYTES` placeholder fallback).
  - Added unit test `raw_event_frames_match_typed_wire_and_respect_byte_budget` asserting byte-for-byte wire equivalence with `ServerMessage::Event`.
- **Before vs After (`x86_64-unknown-linux-musl`, 3 runs median, vs baseline / previous step F3):**
  - `large_payloads` `history_p50_ms` / `p95`: `79.38 / 84.88 ms` -> **`43.46 / 48.16 ms`** (**-45.3% / -43.3%**)
  - `large_payloads` settled RSS: `8.84 MiB` (max `10.71`) -> **`7.00 MiB`** (max `7.04`) (**-20.8%**)
  - `reconnect_churn` `cycle_p50_ms` / `p95`: `8.91 / 11.85 ms` (F3: `7.04 / 9.22`) -> **`6.14 / 7.87 ms`** (**-31.1% / -33.6%** vs baseline)
  - `reconnect_churn` CPU / Wakeups/s: `61.34% / 11,248` (F3: `54.37% / 8,110`) -> **`49.37% / 7,390/s`** (**-19.5% / -34.3%** vs baseline)
  - `burst` throughput: `924.7 ev/s` (F3: `1,072.1`) -> **`1,236.7 ev/s`** (**+33.7%** vs baseline)
  - `burst` `relay_p50_ms` / `p95`: `810.5 / 958.0 ms` (F3: `614.4 / 742.4`) -> **`499.5 / 611.3 ms`** (**-38.4% / -36.2%** vs baseline)

---

## F5 — `broadcast::Sender<Arc<Event>>` (capacity 256), `receiver_count() > 0` guard, and zero-clone live delivery

- **Files:** `mac/ccd/src/state.rs`, `mac/ccd/src/ws_server.rs`, `mac/ccd/src/switch.rs`, `mac/ccd/src/claude_host.rs`
- **Change:**
  - Changed `Daemon::events_tx` from `broadcast::Sender<Event>` (1024 slots ≈ 246 KiB permanent heap) to `broadcast::Sender<Arc<Event>>` (256 slots ≈ 6 KiB).
  - Added `Daemon::broadcast_event` gated on `self.events_tx.receiver_count() > 0` so events are neither cloned nor pinned in the ring when no client is listening.
  - Removed redundant `pending.clone()` in `Daemon::ingest`.
  - Added `ws_server::send_event_ref` to serialize live `Arc<Event>` by reference without cloning per subscriber.
- **Before vs After (`x86_64-unknown-linux-musl`, 3 runs median, vs F4 / baseline):**
  - `large_payloads` `relay_p50_ms` / `p95`: `484.0 / 849.5 ms` -> **`410.5 / 782.1 ms`** (**-15.2% / -7.9%**)
  - `large_payloads` settled RSS: `7.00 MiB` (baseline `8.84 MiB`) -> **`6.84 MiB`** (**-22.6%** vs baseline)
  - `large_payloads` Wakeups/s: `3,133/s` -> **`2,744/s`** (**-12.4%**)
  - `burst` throughput: `1,236.7 ev/s` (baseline `924.7`) -> **`1,299.9 ev/s`** (**+40.6%** vs baseline)
  - `burst` settled / Peak RSS: `9.53 / 11.61 MiB` -> **`9.26 / 11.37 MiB`**

---

## F6 — SQLite pool sizing (`READERS = 2`), 256 KiB page cache cap, 8 KiB lookaside, and `PRAGMA shrink_memory`

- **Files:** `mac/ccd/src/store.rs`
- **Change:**
  - Reduced SQLite WAL reader pool from `READERS = 4` to `READERS = 2` (3 total connections instead of 5), saving FDs and per-connection memory.
  - Tuned `open_connection`: shrank lookaside allocator from SQLite's ~120 KiB default to 8 KiB (`64 x 128`) via `SQLITE_DBCONFIG_LOOKASIDE`, capped `PRAGMA cache_size = -256` (256 KiB per connection, 768 KiB pool ceiling vs. 10 MiB default), set `PRAGMA mmap_size = 0`, and set `set_prepared_statement_cache_capacity(32)`.
  - Added `Store::shrink_memory()` (`PRAGMA shrink_memory` across writer + all readers) and called it after `Store::migrate()`, `delete_exited_session`, and non-empty `prune_exited_sessions`.
  - Added unit test `open_connection_caps_memory_pragmas_and_shrink_memory_succeeds`.
- **Before vs After (`x86_64-unknown-linux-musl`, 3 runs median, vs F5 / baseline):**
  - `cold_idle` settled / Peak RSS: `6.29 / 6.32 MiB` (baseline `6.30 / 10.78 MiB`) -> **`5.63 / 5.64 MiB`** (**-10.5% RSS** vs F5; **-47.7% Peak RSS** vs baseline), FDs `23 -> 19`
  - `concurrent_50` settled / Peak RSS: `10.02 / 10.50 MiB` (F3; baseline `10.67 / 11.49 MiB`) -> **`8.50 / 9.04 MiB`** (**-20.3% / -21.3%** vs baseline), `sessions_rpc_p50` `3.00 ms`, FDs `126 -> 120`
  - `large_payloads` settled / Peak RSS: `6.84 / 18.84 MiB` (baseline `8.84 / 19.52 MiB`) -> **`5.94 / 13.81 MiB`** (**-32.8% / -29.3%** vs baseline; **-5.03 MiB peak** vs F5!)
  - `burst` settled / Peak RSS: `9.26 / 11.37 MiB` (baseline `10.17 / 12.16 MiB`) -> **`7.14 / 9.36 MiB`** (**-22.9% / -17.7%** vs F5; **-29.8% / -23.0%** vs baseline), throughput `1,302.1 ev/s`, `approval_rtt_p95` `53.45 -> 34.66 ms`
  - `reconnect_churn` settled / Peak RSS: `8.33 / 8.70 MiB` (F4; baseline `8.85 / 10.64 MiB`) -> **`6.38 / 6.63 MiB`** (**-27.9% / -37.7%** vs baseline), `cycle_p50/p95` `6.04 / 7.26 ms`

---

## F7 — Evict `session_facts` / `tree_facts` on delete & prune, and bound `start_session::Starts`

- **Files:** `mac/ccd/src/state.rs`, `mac/ccd/src/start_session.rs`
- **Change:**
  - Evicted `session_facts` and `tree_facts` in `Daemon::delete_exited_session` (on `Deleted`) and `Daemon::prune_ended_sessions` (on non-dry-run removals), shrinking either map when capacity exceeds `max(64, 4 * len)`.
  - Bounded completed entries in `start_session::Starts` to `MAX_COMPLETED_STARTS = 256` (FIFO eviction of completed `request_id`s only; in-flight starts are never evicted; older completed runs remain idempotent via SQLite `run_hosts.start_request_id`).
  - Added unit tests `delete_and_prune_evict_in_memory_session_and_tree_facts` and `starts_bounds_completed_entries_and_preserves_in_flight`.
- **Before vs After (`x86_64-unknown-linux-musl`, soak verification vs baseline soak):**
  - `soak` settled / Peak RSS: `10.72 / 19.03 MiB` -> **`7.59 / 8.51 MiB`** (**-29.2% / -55.3%**)
  - `soak` RSS start -> end (180 s, 8,248 events, 870 approvals): **`6.95 -> 6.85 MiB`** (net **-0.10 MiB**; baseline grew monotonically `7.70 -> 9.11 MiB` -> `10.72 MiB` settled)
  - `soak` CPU: `6.91%` -> **`5.57%`** (**-19.4%**)
  - `soak` `relay_p50 / p95 / max`: `3.66 / 84.82 / 2,537.6 ms` -> **`2.31 / 58.68 / 321.9 ms`** (**-36.9% / -30.8% / -87.3%**)

---

## F8 — Lazy outbound TLS root CA loading (`LazyOutboundH2`)

- **Files:** `mac/ccd/src/tls.rs`, `mac/ccd/src/relay_sender.rs`, `mac/ccd/src/apns_sender.rs`
- **Change:**
  - Added `tls::LazyOutboundH2` to defer loading and parsing the OS root CA trust store (`rustls_native_certs`) until the first actual outbound push delivery (running the one-time load via `spawn_blocking` and caching the `TlsConnector`, while not caching failures so transient errors retry).
  - Wired `LazyOutboundH2` into both `HttpsRelay` and `ApnsPushSender`, preserving identical TLS/ALPN certificate verification on delivery.
  - Added lazy-init assertion in `relay_sender::tests::the_endpoint_is_baked_and_a_config_file_cannot_move_it`.
- **Before vs After (`x86_64-unknown-linux-musl`, 3 runs median, vs F6 / baseline):**
  - `cold_idle` anonymous heap+stack RSS (`RssAnon`): `1.008 MiB` (baseline `1.762 MiB`) -> **`0.918 MiB`** (**-8.9%** of remaining anonymous RAM; **-47.9%** vs baseline — total non-code RAM now under 1 MiB!)
  - `cold_idle` total RSS / Peak RSS: `5.63 / 5.64 MiB` (baseline `6.30 / 10.78 MiB`) -> **`5.59 / 5.60 MiB`**
  - `idle_client` total RSS / Peak RSS: baseline `6.37 / 10.89 MiB` -> **`5.64 / 5.68 MiB`** (**-11.5% / -47.8%** vs baseline)

---

## F9 — Event-driven idle mail sweep, coalesced 10 s tailnet cache, single-pass peers JSON, and bounded Tokio thread pool

- **Files:** `mac/ccd/src/store.rs`, `mac/ccd/src/db.rs`, `mac/ccd/src/mail.rs`, `mac/ccd/src/state.rs`, `mac/ccd/src/ws_server.rs`, `mac/ccd/src/main.rs`
- **Change:**
  - Made `mail::run_mail_sweeps` event-driven via `Store::mail_has_rows` + `tokio::sync::Notify` (`wait_for_mail`) and `Store::has_mail_activity`: starts `true` so pre-existing DB rows are swept immediately at startup (verified by `mail_at_start` integration test), then sleeps with zero timer wakeups, zero SQLite write transactions, and zero `tailscale status --json` spawns when `mail` and `mail_sent` are empty. Added `sweep_mail_smart` so `read_tailnet()` is only called when unsent or untold mail rows actually exist.
  - Added a 10-second coalesced `tailnet_cache` on `Daemon::read_status` (non-test) while keeping `blocked_tailnet_nodes()` read fresh on every `read_tailnet()` call so device revocation remains immediate and fail-closed.
  - Eliminated double JSON serialization of `peers_frame` in `ws_server.rs` via `send_serialized`, and added an early return in `watch_hosted_runs` when no hosted runs exist.
  - Replaced `#[tokio::main]` in `main.rs` with an explicit runtime builder: `worker_threads(available_parallelism.clamp(2, 4))`, `max_blocking_threads(16)`, `thread_keep_alive(10s)`, handling `--version` before constructing the runtime.
  - Added unit test `mail_activity_flag_and_wait_for_mail_wakes_on_insert`.
- **Before vs After (`x86_64-unknown-linux-musl`, 3 runs median, vs F6–F8 / baseline):**
  - `cold_idle` (65 s × 3) wakeups/s: baseline `0.500/s` -> **`0.188/s`** (**-62.4%**, 0.0% CV across all 3 runs)
  - `cold_idle` RSS / Peak RSS: `5.59 / 5.60 MiB` (baseline `6.30 / 10.78 MiB`) -> **`5.50 / 5.50 MiB`** (**-12.7% / -49.0%** vs baseline), Startup **`89.1 ms`** (**-22.2%** vs baseline)
  - `concurrent_50` Peak threads: `41` (`40` baseline; `61` on `gnu`) -> **`19`** (**-53.7%**)
  - `concurrent_50` Peak RSS / Wakeups/s: `9.04 MiB / 344/s` (baseline `11.49 MiB / 473/s`) -> **`8.77 MiB / 307/s`** (**-23.7% / -35.1%** vs baseline)
  - `burst` Peak threads / RSS / `relay_p50`: `23 / 7.14 MiB / 638 ms` -> **`19 / 6.99 MiB / 559 ms`**
