# CodeConnect macOS Benchmark & Profiling Scripts

These scripts measure both the macOS Rust daemon (`ccd`, launchd-managed or standalone) and the macOS SwiftUI app (`CodeConnect.app`) using native Apple profiling tools (`footprint`, `vmmap`, `leaks`, `xctrace`, `powermetrics`, `lsof`).

> **Note:** These scripts were authored on Linux (where no macOS host was available) and should be run on a macOS 26+ Mac (Apple silicon or Intel) to capture real Darwin memory (`phys_footprint`), Metal/SwiftUI allocations, and wakeup/energy metrics.

## Scripts

1. **`bench-ccd.sh`** — Runs offline `ccd` scenarios (`cold_idle`, `idle_client`, `concurrent_10`, `concurrent_50`, `burst`, `large_payloads`, `soak`) on macOS using the same `bench/mocks/` harness, collecting `footprint`, `vmmap --summary`, `leaks`, `lsof`, and `ps` metrics into JSON + text reports.
2. **`profile-app.sh`** — Attaches to a running `CodeConnect` (GUI) or `ccd` (daemon) process and captures:
   - `footprint` (physical memory footprint, dirty, compressed, wired)
   - `vmmap --summary` (region breakdown: MALLOC, IOSurface, CoreAnimation, SQLite, etc.)
   - `leaks` (malloc heap leak scan)
   - `xctrace` (`Allocations` and `Time Profiler` `.trace` bundles openable in Instruments.app)
   - `powermetrics` (optional, requires `sudo`, measures idle wakeups/sec and energy impact)

## Quick Start (on a Mac)

```sh
# 1. Benchmark ccd offline on macOS
./bench/macos/bench-ccd.sh --out bench/out/macos-ccd

# 2. Profile the running CodeConnect.app GUI (60s window)
./bench/macos/profile-app.sh --name CodeConnect --duration 60 --out bench/out/macos-gui

# 3. Profile the launchd-managed ccd daemon with powermetrics wakeups (sudo)
sudo ./bench/macos/profile-app.sh --name ccd --duration 60 --powermetrics --out bench/out/macos-ccd-live
```
