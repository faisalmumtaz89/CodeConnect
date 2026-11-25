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
