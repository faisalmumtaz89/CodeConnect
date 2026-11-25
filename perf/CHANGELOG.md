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
