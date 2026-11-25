#!/usr/bin/env python3
"""
CodeConnect Offline Performance & Memory Benchmark Harness.

Drives the real `ccd` daemon through its real interfaces:
  - WebSocket listener (ws://127.0.0.1:<port>) via `mock-client`
  - Unix IPC socket (<root>/ccd.sock)
  - Claude agent host protocol (<root>/runs/<uid>/host.sock) via `mock-claude-host`
  - Codex app-server protocol (<root>/runs/<uid>/as.sock) via `mock-codex`
  - Real SQLite WAL event store & git working tree diffs

Outputs structured JSON (`results.json`) and Markdown tables (`results.md`).
"""

import argparse
import json
import math
import os
import platform
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

CLK_TCK = os.sysconf("SC_CLK_TCK")


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def parse_kb_map(text: str) -> dict[str, int]:
    out: dict[str, int] = {}
    for line in text.splitlines():
        if ":" not in line:
            continue
        k, v = line.split(":", 1)
        parts = v.strip().split()
        if parts and parts[0].isdigit():
            out[k.strip()] = int(parts[0])
    return out


def sample_proc(pid: int, tid_ctxt: dict[str, tuple[int, int]] | None = None) -> dict | None:
    base = Path(f"/proc/{pid}")
    if not base.exists():
        return None
    try:
        status_text = (base / "status").read_text()
        status = parse_kb_map(status_text)
        smaps_rollup_text = (base / "smaps_rollup").read_text()
        rollup = parse_kb_map(smaps_rollup_text)
        stat_text = (base / "stat").read_text()
        # Handle comm containing spaces/parens by finding last ')'
        rparen = stat_text.rfind(")")
        fields = stat_text[rparen + 2 :].split()
        utime = int(fields[11])
        stime = int(fields[12])

        # Sum voluntary + nonvoluntary context switches across ALL threads
        # (/proc/<pid>/status only counts the main thread). Keeping a monotonic
        # per-TID dict preserves switches from ephemeral blocking threads.
        task_dir = base / "task"
        vol_total = 0
        nonvol_total = 0
        try:
            seen_any = False
            for entry in os.scandir(task_dir):
                try:
                    t_status = parse_kb_map(Path(entry.path, "status").read_text())
                    v = t_status.get("voluntary_ctxt_switches", 0)
                    nv = t_status.get("nonvoluntary_ctxt_switches", 0)
                    seen_any = True
                    if tid_ctxt is not None:
                        pv, pnv = tid_ctxt.get(entry.name, (0, 0))
                        tid_ctxt[entry.name] = (max(pv, v), max(pnv, nv))
                    else:
                        vol_total += v
                        nonvol_total += nv
                except OSError:
                    pass
            if tid_ctxt is not None and seen_any:
                vol_total = sum(v for v, _ in tid_ctxt.values())
                nonvol_total = sum(nv for _, nv in tid_ctxt.values())
            elif not seen_any:
                vol_total = status.get("voluntary_ctxt_switches", 0)
                nonvol_total = status.get("nonvoluntary_ctxt_switches", 0)
        except OSError:
            vol_total = status.get("voluntary_ctxt_switches", 0)
            nonvol_total = status.get("nonvoluntary_ctxt_switches", 0)

        fds_dir = base / "fd"
        fd_total = 0
        fd_sockets = 0
        try:
            for entry in os.scandir(fds_dir):
                fd_total += 1
                try:
                    target = os.readlink(entry.path)
                    if target.startswith("socket:["):
                        fd_sockets += 1
                except OSError:
                    pass
        except OSError:
            pass

        rss_kb = rollup.get("Rss", status.get("VmRSS", 0))
        pss_kb = rollup.get("Pss", 0)
        uss_kb = rollup.get("Private_Clean", 0) + rollup.get("Private_Dirty", 0)
        peak_kb = status.get("VmHWM", rss_kb)
        anon_kb = rollup.get("Pss_Anon", status.get("RssAnon", 0))
        file_kb = rollup.get("Pss_File", status.get("RssFile", 0))
        shmem_kb = rollup.get("Pss_Shmem", status.get("RssShmem", 0))

        return {
            "ts": time.monotonic(),
            "rss_kb": rss_kb,
            "pss_kb": pss_kb,
            "uss_kb": uss_kb,
            "peak_rss_kb": peak_kb,
            "rss_anon_kb": status.get("RssAnon", anon_kb),
            "rss_file_kb": status.get("RssFile", file_kb),
            "rss_shmem_kb": status.get("RssShmem", shmem_kb),
            "threads": status.get("Threads", 0),
            "fds": fd_total,
            "sockets": fd_sockets,
            "voluntary_ctxt_switches": vol_total,
            "nonvoluntary_ctxt_switches": nonvol_total,
            "cpu_ticks": utime + stime,
        }
    except (FileNotFoundError, ProcessLookupError, PermissionError, IndexError, ValueError):
        return None


def sample_smaps_heap_breakdown(pid: int) -> dict:
    """Break down RSS into [heap], anonymous mmap/stacks, and file-backed mappings."""
    heap_kb = 0
    anon_other_kb = 0
    file_kb = 0
    stack_kb = 0
    try:
        smaps = Path(f"/proc/{pid}/smaps").read_text()
    except OSError:
        return {}
    current_kind = "anon"
    for line in smaps.splitlines():
        if "-" in line[:20] and " " in line:
            parts = line.split(None, 5)
            path = parts[5].strip() if len(parts) >= 6 else ""
            if path == "[heap]":
                current_kind = "heap"
            elif path.startswith("[stack"):
                current_kind = "stack"
            elif path == "" or path.startswith("[anon"):
                current_kind = "anon"
            else:
                current_kind = "file"
        elif line.startswith("Rss:"):
            parts = line.split()
            if len(parts) >= 2 and parts[1].isdigit():
                kb = int(parts[1])
                if current_kind == "heap":
                    heap_kb += kb
                elif current_kind == "stack":
                    stack_kb += kb
                elif current_kind == "anon":
                    anon_other_kb += kb
                else:
                    file_kb += kb
    return {
        "heap_rss_kb": heap_kb,
        "stack_rss_kb": stack_kb,
        "anon_mmap_rss_kb": anon_other_kb,
        "file_rss_kb": file_kb,
    }


class Sampler:
    def __init__(self, pid: int, interval: float = 0.5):
        self.pid = pid
        self.interval = interval
        self.samples: list[dict] = []
        self._tid_ctxt: dict[str, tuple[int, int]] = {}
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self):
        s = sample_proc(self.pid, self._tid_ctxt)
        if s:
            self.samples.append(s)
        self._thread.start()

    def _run(self):
        while not self._stop.wait(self.interval):
            s = sample_proc(self.pid, self._tid_ctxt)
            if s:
                self.samples.append(s)

    def stop(self) -> list[dict]:
        self._stop.set()
        self._thread.join(timeout=5)
        s = sample_proc(self.pid, self._tid_ctxt)
        if s:
            self.samples.append(s)
        return self.samples


def linear_regression_slope_mb_per_hour(samples: list[dict], key: str) -> float:
    if len(samples) < 4:
        return 0.0
    # Drop first 10% warm-up to measure steady-state growth rate
    start_idx = max(1, len(samples) // 10)
    sub = samples[start_idx:]
    t0 = sub[0]["ts"]
    xs = [s["ts"] - t0 for s in sub]
    ys = [s[key] / 1024.0 for s in sub]  # MB
    n = len(xs)
    mx = sum(xs) / n
    my = sum(ys) / n
    num = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    den = sum((x - mx) ** 2 for x in xs)
    if den <= 1e-9:
        return 0.0
    slope_mb_per_sec = num / den
    return slope_mb_per_sec * 3600.0


def summarize_samples(samples: list[dict], heap_breakdown: dict | None = None) -> dict:
    if not samples:
        return {}
    first = samples[0]
    last = samples[-1]
    dt = max(last["ts"] - first["ts"], 1e-6)
    dcpu = max(last["cpu_ticks"] - first["cpu_ticks"], 0) / CLK_TCK
    cpu_pct = (dcpu / dt) * 100.0
    dctx = max(
        (last["voluntary_ctxt_switches"] + last["nonvoluntary_ctxt_switches"])
        - (first["voluntary_ctxt_switches"] + first["nonvoluntary_ctxt_switches"]),
        0,
    )
    wakeups_per_sec = dctx / dt

    # Measure settled tail (last 20% of samples, at least 1)
    tail_n = max(1, len(samples) // 5)
    tail = samples[-tail_n:]

    out = {
        "duration_s": round(dt, 3),
        "samples": len(samples),
        "rss_mb": round(statistics.median([s["rss_kb"] for s in tail]) / 1024.0, 3),
        "pss_mb": round(statistics.median([s["pss_kb"] for s in tail]) / 1024.0, 3),
        "uss_mb": round(statistics.median([s["uss_kb"] for s in tail]) / 1024.0, 3),
        "peak_rss_mb": round(max(s["peak_rss_kb"] for s in samples) / 1024.0, 3),
        "rss_anon_mb": round(statistics.median([s["rss_anon_kb"] for s in tail]) / 1024.0, 3),
        "rss_file_mb": round(statistics.median([s["rss_file_kb"] for s in tail]) / 1024.0, 3),
        "cpu_pct": round(cpu_pct, 4),
        "wakeups_per_sec": round(wakeups_per_sec, 3),
        "threads": int(max(s["threads"] for s in tail)),
        "peak_threads": int(max(s["threads"] for s in samples)),
        "fds": int(statistics.median([s["fds"] for s in tail])),
        "peak_fds": int(max(s["fds"] for s in samples)),
        "sockets": int(statistics.median([s["sockets"] for s in tail])),
        "peak_sockets": int(max(s["sockets"] for s in samples)),
    }
    if heap_breakdown:
        out["heap_mb"] = round(heap_breakdown.get("heap_rss_kb", 0) / 1024.0, 3)
        out["anon_mmap_mb"] = round(heap_breakdown.get("anon_mmap_rss_kb", 0) / 1024.0, 3)
        out["stack_mb"] = round(heap_breakdown.get("stack_rss_kb", 0) / 1024.0, 3)
        out["file_map_mb"] = round(heap_breakdown.get("file_rss_kb", 0) / 1024.0, 3)
    return out


def setup_git_workdir(workdir: Path):
    """Create a realistic git repo with ~300 KB of dirty diff for get_diff benchmarks."""
    workdir.mkdir(parents=True, exist_ok=True)
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "Bench",
        "GIT_AUTHOR_EMAIL": "bench@local",
        "GIT_COMMITTER_NAME": "Bench",
        "GIT_COMMITTER_EMAIL": "bench@local",
    }
    subprocess.run(["git", "init", "-q"], cwd=workdir, env=env, check=True)
    for idx in range(6):
        lines = [f"pub fn module_{idx}_func_{i}(x: u64) -> u64 {{ x.wrapping_add({i}) }}\n" for i in range(400)]
        (workdir / f"mod_{idx}.rs").write_text("".join(lines))
    subprocess.run(["git", "add", "."], cwd=workdir, env=env, check=True)
    subprocess.run(["git", "commit", "-q", "-m", "initial"], cwd=workdir, env=env, check=True)
    # Introduce a ~250 KB uncommitted diff across tracked files
    for idx in range(6):
        lines = [
            f"pub fn module_{idx}_func_{i}(x: u64, y: u64) -> u64 {{ x.wrapping_add(y).wrapping_mul({i + 1}) }}\n"
            if i % 2 == 0
            else f"pub fn module_{idx}_func_{i}(x: u64) -> u64 {{ x.wrapping_add({i}) }}\n"
            for i in range(400)
        ]
        (workdir / f"mod_{idx}.rs").write_text("".join(lines))


def kill_proc_tree(pid: int):
    try:
        os.killpg(os.getpgid(pid), signal.SIGTERM)
    except ProcessLookupError:
        return
    except PermissionError:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            return
    deadline = time.monotonic() + 3.0
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
            time.sleep(0.05)
        except ProcessLookupError:
            return
    try:
        os.killpg(os.getpgid(pid), signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def cleanup_run_hosts(home: Path):
    """Ensure any detached mock hosts recorded under <home>/runs are stopped cleanly."""
    runs_dir = home / "runs"
    if not runs_dir.exists():
        return
    for run in runs_dir.iterdir():
        for fname in ("host.json", "as.pid"):
            p = run / fname
            if not p.exists():
                continue
            try:
                if fname == "host.json":
                    pid = int(json.loads(p.read_text())["host_pid"])
                else:
                    pid = int(p.read_text().strip())
                kill_proc_tree(pid)
            except Exception:
                pass


class DaemonFixture:
    def __init__(self, ccd_bin: Path, mocks_dir: Path, mock_claude_sh: Path):
        self.ccd_bin = ccd_bin
        self.mocks_dir = mocks_dir
        self.mock_claude_sh = mock_claude_sh
        self.tmpdir = Path(tempfile.mkdtemp(prefix="cc-bench-"))
        self.home = self.tmpdir / "home"
        self.home.mkdir(mode=0o700)
        self.workdir = self.tmpdir / "repo"
        self.port = free_port()
        self.ws_url = f"ws://127.0.0.1:{self.port}"
        self.sock_path = self.home / "ccd.sock"
        self.proc: subprocess.Popen | None = None
        self.log_file = None

    def write_config(self):
        cfg = {
            "ws_bind": "127.0.0.1",
            "ws_port": self.port,
            "ws_loopback": False,
            "push_enabled": True,
            "tls": False,
            "tailscale_bin": "/nonexistent/tailscale",
            "claude_bin": str(self.mock_claude_sh),
            "agent_host_bin": str(self.mocks_dir / "mock-claude-host"),
            "codex_bin": str(self.mocks_dir / "mock-codex"),
        }
        (self.home / "config.json").write_text(json.dumps(cfg, indent=2))

    def start(self) -> float:
        self.write_config()
        env = {
            **os.environ,
            "CODECONNECT_HOME": str(self.home),
            "CODECONNECT_LOG": "warn",
            "SHELL": "/bin/sh",
        }
        self.log_file = open(self.tmpdir / "ccd.log", "wb")
        t0 = time.monotonic()
        self.proc = subprocess.Popen(
            [str(self.ccd_bin)],
            env=env,
            stdout=self.log_file,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        # Wait until both IPC and WS hello_ack succeed
        res = subprocess.run(
            [
                str(self.mocks_dir / "mock-client"),
                "wait-ready",
                "--ws",
                self.ws_url,
                "--sock",
                str(self.sock_path),
                "--timeout-secs",
                "15",
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        startup_ms = (time.monotonic() - t0) * 1000.0
        return round(startup_ms, 2)

    def stop(self):
        if self.proc is not None:
            kill_proc_tree(self.proc.pid)
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()
            self.proc = None
        if self.log_file is not None:
            self.log_file.close()
            self.log_file = None
        cleanup_run_hosts(self.home)
        shutil.rmtree(self.tmpdir, ignore_errors=True)


def run_client(mocks_dir: Path, args: list[str], timeout: int = 300) -> dict:
    cmd = [str(mocks_dir / "mock-client"), *args]
    res = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    if res.returncode != 0:
        raise RuntimeError(f"mock-client failed ({res.returncode}): {res.stderr}\nstdout: {res.stdout}")
    return json.loads(res.stdout.strip().splitlines()[-1])


def run_scenario_once(
    scenario: str,
    ccd_bin: Path,
    mocks_dir: Path,
    mock_claude_sh: Path,
    idle_secs: int,
    idle_client_secs: int,
    soak_secs: int,
) -> dict:
    fx = DaemonFixture(ccd_bin, mocks_dir, mock_claude_sh)
    try:
        if scenario in ("concurrent_1", "concurrent_10", "concurrent_50", "burst", "large_payloads", "reconnect_churn", "soak"):
            setup_git_workdir(fx.workdir)

        startup_ms = fx.start()
        assert fx.proc is not None
        pid = fx.proc.pid

        client_metrics: dict = {}
        extra: dict = {}

        if scenario == "cold_idle":
            # Measure settled idle window after startup
            sampler = Sampler(pid, interval=1.0)
            sampler.start()
            time.sleep(idle_secs)
            samples = sampler.stop()
            heap_bd = sample_smaps_heap_breakdown(pid)
            # Compute idle CPU specifically over the second half (post-warmup)
            mid = len(samples) // 2
            proc_stats = summarize_samples(samples[mid:] if mid > 1 else samples, heap_bd)
            proc_stats["peak_rss_mb"] = round(max(s["peak_rss_kb"] for s in samples) / 1024.0, 3)

        elif scenario == "idle_client":
            sampler = Sampler(pid, interval=1.0)
            sampler.start()
            client_metrics = run_client(
                mocks_dir,
                [
                    "idle-client",
                    "--ws",
                    fx.ws_url,
                    "--sock",
                    str(fx.sock_path),
                    "--duration-secs",
                    str(idle_client_secs),
                ],
                timeout=idle_client_secs + 30,
            )
            samples = sampler.stop()
            heap_bd = sample_smaps_heap_breakdown(pid)
            mid = len(samples) // 2
            proc_stats = summarize_samples(samples[mid:] if mid > 1 else samples, heap_bd)
            proc_stats["peak_rss_mb"] = round(max(s["peak_rss_kb"] for s in samples) / 1024.0, 3)

        elif scenario in ("concurrent_1", "concurrent_10", "concurrent_50"):
            counts = {
                "concurrent_1": (1, 0),
                "concurrent_10": (5, 5),
                "concurrent_50": (25, 25),
            }
            c_n, x_n = counts[scenario]
            sampler = Sampler(pid, interval=0.5)
            sampler.start()
            client_metrics = run_client(
                mocks_dir,
                [
                    "sessions",
                    "--ws",
                    fx.ws_url,
                    "--sock",
                    str(fx.sock_path),
                    "--claude",
                    str(c_n),
                    "--codex",
                    str(x_n),
                    "--workdir",
                    str(fx.workdir),
                    "--hold-secs",
                    "12",
                ],
                timeout=180,
            )
            heap_bd = sample_smaps_heap_breakdown(pid)
            samples = sampler.stop()
            proc_stats = summarize_samples(samples, heap_bd)
            # Also capture steady-state idle CPU during the hold phase (last 10s)
            hold_samples = [s for s in samples if s["ts"] >= samples[-1]["ts"] - 10.0]
            if len(hold_samples) >= 2:
                hold_stats = summarize_samples(hold_samples)
                proc_stats["hold_cpu_pct"] = hold_stats["cpu_pct"]
                proc_stats["hold_wakeups_per_sec"] = hold_stats["wakeups_per_sec"]

        elif scenario == "burst":
            sampler = Sampler(pid, interval=0.25)
            sampler.start()
            client_metrics = run_client(
                mocks_dir,
                [
                    "burst",
                    "--ws",
                    fx.ws_url,
                    "--sock",
                    str(fx.sock_path),
                    "--claude",
                    "5",
                    "--codex",
                    "5",
                    "--events-per-session",
                    "80",
                    "--approvals-per-session",
                    "10",
                    "--interval-us",
                    "500",
                    "--workdir",
                    str(fx.workdir),
                ],
                timeout=180,
            )
            heap_bd = sample_smaps_heap_breakdown(pid)
            samples = sampler.stop()
            proc_stats = summarize_samples(samples, heap_bd)

        elif scenario == "large_payloads":
            sampler = Sampler(pid, interval=0.25)
            sampler.start()
            client_metrics = run_client(
                mocks_dir,
                [
                    "large",
                    "--ws",
                    fx.ws_url,
                    "--sock",
                    str(fx.sock_path),
                    "--events",
                    "150",
                    "--payload-bytes",
                    "65536",
                    "--diff-iters",
                    "10",
                    "--workdir",
                    str(fx.workdir),
                ],
                timeout=180,
            )
            heap_bd = sample_smaps_heap_breakdown(pid)
            samples = sampler.stop()
            proc_stats = summarize_samples(samples, heap_bd)

        elif scenario == "reconnect_churn":
            sampler = Sampler(pid, interval=0.25)
            sampler.start()
            client_metrics = run_client(
                mocks_dir,
                [
                    "churn",
                    "--ws",
                    fx.ws_url,
                    "--sock",
                    str(fx.sock_path),
                    "--cycles",
                    "60",
                    "--sessions",
                    "5",
                    "--workdir",
                    str(fx.workdir),
                ],
                timeout=180,
            )
            heap_bd = sample_smaps_heap_breakdown(pid)
            samples = sampler.stop()
            proc_stats = summarize_samples(samples, heap_bd)

        elif scenario == "soak":
            sampler = Sampler(pid, interval=5.0)
            sampler.start()
            client_metrics = run_client(
                mocks_dir,
                [
                    "soak",
                    "--ws",
                    fx.ws_url,
                    "--sock",
                    str(fx.sock_path),
                    "--claude",
                    "5",
                    "--codex",
                    "5",
                    "--duration-secs",
                    str(soak_secs),
                    "--interval-ms",
                    "2000",
                    "--workdir",
                    str(fx.workdir),
                ],
                timeout=soak_secs + 120,
            )
            heap_bd = sample_smaps_heap_breakdown(pid)
            samples = sampler.stop()
            proc_stats = summarize_samples(samples, heap_bd)
            extra = {
                "rss_growth_mb_per_hour": round(linear_regression_slope_mb_per_hour(samples, "rss_kb"), 3),
                "pss_growth_mb_per_hour": round(linear_regression_slope_mb_per_hour(samples, "pss_kb"), 3),
                "uss_growth_mb_per_hour": round(linear_regression_slope_mb_per_hour(samples, "uss_kb"), 3),
                "start_rss_mb": round(samples[min(2, len(samples) - 1)]["rss_kb"] / 1024.0, 3),
                "end_rss_mb": round(samples[-1]["rss_kb"] / 1024.0, 3),
                "start_uss_mb": round(samples[min(2, len(samples) - 1)]["uss_kb"] / 1024.0, 3),
                "end_uss_mb": round(samples[-1]["uss_kb"] / 1024.0, 3),
                "timeseries": [
                    {
                        "t_s": round(s["ts"] - samples[0]["ts"], 1),
                        "rss_mb": round(s["rss_kb"] / 1024.0, 3),
                        "pss_mb": round(s["pss_kb"] / 1024.0, 3),
                        "uss_mb": round(s["uss_kb"] / 1024.0, 3),
                        "fds": s["fds"],
                        "threads": s["threads"],
                    }
                    for s in samples[:: max(1, len(samples) // 120)]
                ],
            }
        else:
            raise ValueError(f"unknown scenario: {scenario}")

        return {
            "startup_ms": startup_ms,
            "proc": proc_stats,
            "client": client_metrics,
            **({"soak": extra} if extra else {}),
        }
    finally:
        fx.stop()


def agg_values(vals: list[float]) -> dict:
    s = sorted(vals)
    n = len(s)
    med = statistics.median(s)
    p95 = s[min(n - 1, int(math.ceil(0.95 * n)) - 1)]
    mean = statistics.mean(s)
    stdev = statistics.stdev(s) if n > 1 else 0.0
    cv_pct = (stdev / abs(mean) * 100.0) if abs(mean) > 1e-9 else 0.0
    return {
        "min": round(s[0], 3),
        "median": round(med, 3),
        "p95": round(p95, 3),
        "max": round(s[-1], 3),
        "cv_pct": round(cv_pct, 2),
    }


def aggregate_runs(runs: list[dict]) -> dict:
    proc_keys = [
        "rss_mb",
        "pss_mb",
        "uss_mb",
        "peak_rss_mb",
        "rss_anon_mb",
        "rss_file_mb",
        "heap_mb",
        "anon_mmap_mb",
        "cpu_pct",
        "wakeups_per_sec",
        "hold_cpu_pct",
        "hold_wakeups_per_sec",
        "threads",
        "peak_threads",
        "fds",
        "peak_fds",
        "sockets",
        "peak_sockets",
    ]
    out: dict = {
        "runs": len(runs),
        "startup_ms": agg_values([r["startup_ms"] for r in runs]),
        "proc": {},
        "client": {},
    }
    for k in proc_keys:
        vals = [float(r["proc"][k]) for r in runs if k in r["proc"]]
        if vals:
            out["proc"][k] = agg_values(vals)

    # Aggregate latency/throughput client metrics if present
    def extract_path(d: dict, path: list[str]):
        cur = d
        for p in path:
            if not isinstance(cur, dict) or p not in cur:
                return None
            cur = cur[p]
        return cur

    client_paths = [
        ("relay_p50_ms", ["relay_latency", "p50_ms"]),
        ("relay_p95_ms", ["relay_latency", "p95_ms"]),
        ("relay_max_ms", ["relay_latency", "max_ms"]),
        ("approval_rtt_p50_ms", ["approval_rtt", "p50_ms"]),
        ("approval_rtt_p95_ms", ["approval_rtt", "p95_ms"]),
        ("events_per_sec", ["events_per_sec"]),
        ("events_received", ["events_received"]),
        ("approvals_answered", ["approvals_answered"]),
        ("history_p50_ms", ["history_latency", "p50_ms"]),
        ("history_p95_ms", ["history_latency", "p95_ms"]),
        ("diff_p50_ms", ["diff_latency", "p50_ms"]),
        ("diff_p95_ms", ["diff_latency", "p95_ms"]),
        ("cycle_p50_ms", ["cycle_latency", "p50_ms"]),
        ("cycle_p95_ms", ["cycle_latency", "p95_ms"]),
        ("start_p50_ms", ["start_latency", "p50_ms"]),
        ("start_p95_ms", ["start_latency", "p95_ms"]),
        ("sessions_rpc_p50_ms", ["sessions_rpc_latency", "p50_ms"]),
        ("sessions_rpc_p95_ms", ["sessions_rpc_latency", "p95_ms"]),
    ]
    for name, path in client_paths:
        vals = []
        for r in runs:
            v = extract_path(r.get("client", {}), path)
            if isinstance(v, (int, float)):
                vals.append(float(v))
        if vals:
            out["client"][name] = agg_values(vals)

    if any("soak" in r for r in runs):
        out["soak"] = [r["soak"] for r in runs if "soak" in r]
    out["raw_runs"] = runs
    return out


def collect_build_metrics(repo_root: Path) -> dict:
    mac_dir = repo_root / "mac"
    binaries = {}
    for target in ("x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"):
        for bin_name in ("ccd", "codeconnect"):
            p = mac_dir / "target" / target / "release" / bin_name
            if p.exists():
                binaries[f"{target}/{bin_name}"] = {
                    "bytes": p.stat().st_size,
                    "mb": round(p.stat().st_size / (1024 * 1024), 3),
                }
        # Also check default release dir (host target)
    for bin_name in ("ccd", "codeconnect"):
        p = mac_dir / "target" / "release" / bin_name
        if p.exists():
            binaries[f"host-release/{bin_name}"] = {
                "bytes": p.stat().st_size,
                "mb": round(p.stat().st_size / (1024 * 1024), 3),
            }

    deps = {}
    for pkg in ("ccd", "codeconnect", "protocol"):
        r = subprocess.run(
            ["cargo", "tree", "--manifest-path", str(mac_dir / "Cargo.toml"), "-p", pkg, "--prefix=none"],
            capture_output=True,
            text=True,
        )
        if r.returncode == 0:
            uniq = sorted({line.strip().split(" (")[0] for line in r.stdout.splitlines() if line.strip()})
            deps[pkg] = len(uniq)
    r_dup = subprocess.run(
        ["cargo", "tree", "--manifest-path", str(mac_dir / "Cargo.toml"), "-d", "--depth", "0", "--prefix=none"],
        capture_output=True,
        text=True,
    )
    dup_versions = []
    dup_names = set()
    if r_dup.returncode == 0:
        for line in r_dup.stdout.splitlines():
            s = line.strip()
            if s:
                dup_versions.append(s)
                dup_names.add(s.split()[0])

    return {
        "binaries": binaries,
        "unique_deps": deps,
        "duplicate_crate_groups": len(dup_names),
        "duplicate_crates": sorted(dup_versions)[:40],
    }


def collect_env(repo_root: Path, ccd_bin: Path) -> dict:
    sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo_root, text=True).strip()
    rustc = subprocess.check_output(["rustc", "--version"], text=True).strip()
    cpu_model = ""
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu_model = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    mem_kb = 0
    try:
        mem_kb = parse_kb_map(Path("/proc/meminfo").read_text()).get("MemTotal", 0)
    except OSError:
        pass
    return {
        "timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "commit_sha": sha,
        "os": platform.platform(),
        "kernel": platform.release(),
        "arch": platform.machine(),
        "cpus": os.cpu_count(),
        "cpu_model": cpu_model,
        "ram_gb": round(mem_kb / (1024 * 1024), 2),
        "rustc": rustc,
        "ccd_binary": str(ccd_bin),
        "ccd_binary_bytes": ccd_bin.stat().st_size if ccd_bin.exists() else 0,
    }


def render_markdown(report: dict) -> str:
    env = report["env"]
    lines = [
        "# CodeConnect Benchmark Results",
        "",
        "## Environment",
        f"- **Commit:** `{env['commit_sha']}`",
        f"- **Timestamp (UTC):** {env['timestamp_utc']}",
        f"- **OS / Kernel:** {env['os']} (`{env['kernel']}`)",
        f"- **CPU:** {env['cpus']} vCPU — {env['cpu_model']}",
        f"- **RAM:** {env['ram_gb']} GiB",
        f"- **Rust:** `{env['rustc']}`",
        f"- **Measured binary:** `{env['ccd_binary']}` ({env['ccd_binary_bytes'] / (1024*1024):.2f} MiB)",
        "",
        "## Scenario Summary (median [min / p95 / max], CV%)",
        "",
        "| Scenario | Runs | RSS (MiB) | PSS (MiB) | USS (MiB) | Peak RSS (MiB) | CPU (%) | Wakeups/s | Threads | FDs (Socks) | Startup (ms) |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for name, data in report["scenarios"].items():
        p = data["proc"]
        def fmt(k: str) -> str:
            if k not in p:
                return "-"
            v = p[k]
            return f"{v['median']:.2f} ({v['min']:.2f}–{v['max']:.2f}, {v['cv_pct']:.1f}%)"
        def fmt_int(k: str) -> str:
            if k not in p:
                return "-"
            return f"{int(round(p[k]['median']))}"
        st = data["startup_ms"]
        st_s = f"{st['median']:.1f} ({st['min']:.1f}–{st['max']:.1f})"
        fds_s = f"{fmt_int('fds')} ({fmt_int('sockets')})"
        lines.append(
            f"| `{name}` | {data['runs']} | {fmt('rss_mb')} | {fmt('pss_mb')} | {fmt('uss_mb')} | "
            f"{fmt('peak_rss_mb')} | {fmt('cpu_pct')} | {fmt('wakeups_per_sec')} | "
            f"{fmt_int('threads')} | {fds_s} | {st_s} |"
        )

    lines.extend(["", "## Latency & Throughput Metrics", ""])
    lines.append("| Scenario | Metric | Min | Median | p95 | Max | CV% |")
    lines.append("|---|---|---:|---:|---:|---:|---:|")
    for name, data in report["scenarios"].items():
        for mk, mv in data.get("client", {}).items():
            lines.append(
                f"| `{name}` | `{mk}` | {mv['min']:.2f} | {mv['median']:.2f} | {mv['p95']:.2f} | {mv['max']:.2f} | {mv['cv_pct']:.1f}% |"
            )

    if "soak" in report["scenarios"] and report["scenarios"]["soak"].get("soak"):
        lines.extend(["", "## Soak Growth Analysis", ""])
        for i, sk in enumerate(report["scenarios"]["soak"]["soak"], 1):
            lines.append(
                f"- **Run {i}:** RSS {sk['start_rss_mb']:.2f} -> {sk['end_rss_mb']:.2f} MiB "
                f"(slope **{sk['rss_growth_mb_per_hour']:+.2f} MiB/hr**), "
                f"PSS slope **{sk['pss_growth_mb_per_hour']:+.2f} MiB/hr**, "
                f"USS {sk['start_uss_mb']:.2f} -> {sk['end_uss_mb']:.2f} MiB "
                f"(slope **{sk['uss_growth_mb_per_hour']:+.2f} MiB/hr**)"
            )

    bm = report.get("build", {})
    if bm:
        lines.extend(["", "## Binary Size & Dependencies", ""])
        for bk, bv in sorted(bm.get("binaries", {}).items()):
            lines.append(f"- `{bk}`: **{bv['mb']:.3f} MiB** ({bv['bytes']} bytes)")
        for pk, cnt in sorted(bm.get("unique_deps", {}).items()):
            lines.append(f"- `{pk}` unique crates: **{cnt}**")
        lines.append(f"- Duplicate crate groups (`cargo tree -d`): **{bm.get('duplicate_crate_groups', 0)}**")

    lines.append("")
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description="CodeConnect offline benchmark driver")
    ap.add_argument("--binary", required=True, help="Path to ccd release binary")
    ap.add_argument("--mocks-dir", required=True, help="Directory holding mock-claude-host, mock-codex, mock-client")
    ap.add_argument("--mock-claude", required=True, help="Path to mock-claude script")
    ap.add_argument("--out", required=True, help="Output directory for results.json and results.md")
    ap.add_argument(
        "--scenarios",
        default="cold_idle,idle_client,concurrent_1,concurrent_10,concurrent_50,burst,large_payloads,reconnect_churn",
        help="Comma-separated scenarios to run",
    )
    ap.add_argument("--runs", type=int, default=3, help="Number of runs per scenario")
    ap.add_argument("--idle-secs", type=int, default=300, help="Duration of cold_idle scenario (seconds)")
    ap.add_argument("--idle-client-secs", type=int, default=60, help="Duration of idle_client scenario (seconds)")
    ap.add_argument("--soak-secs", type=int, default=600, help="Duration of soak scenario (seconds)")
    ap.add_argument("--soak-runs", type=int, default=1, help="Number of soak runs")
    ap.add_argument("--append", action="store_true", help="Append new runs into existing out/results.json")
    args = ap.parse_args()

    repo_root = Path(__file__).resolve().parent.parent
    ccd_bin = Path(args.binary).resolve()
    mocks_dir = Path(args.mocks_dir).resolve()
    mock_claude_sh = Path(args.mock_claude).resolve()
    out_dir = Path(args.out).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    scenarios = [s.strip() for s in args.scenarios.split(",") if s.strip()]
    results = {}
    res_json = out_dir / "results.json"
    if args.append and res_json.exists():
        prev = json.loads(res_json.read_text())
        results = prev.get("scenarios", {})

    def flush():
        report = {
            "env": collect_env(repo_root, ccd_bin),
            "scenarios": results,
            "build": collect_build_metrics(repo_root),
        }
        res_json.write_text(json.dumps(report, indent=2))
        (out_dir / "results.md").write_text(render_markdown(report))

    for sc in scenarios:
        n_runs = args.soak_runs if sc == "soak" else args.runs
        runs = list(results.get(sc, {}).get("raw_runs", [])) if args.append else []
        for idx in range(1, n_runs + 1):
            print(f"[bench] scenario={sc} run={idx}/{n_runs} ...", flush=True)
            r = run_scenario_once(
                sc,
                ccd_bin,
                mocks_dir,
                mock_claude_sh,
                idle_secs=args.idle_secs,
                idle_client_secs=args.idle_client_secs,
                soak_secs=args.soak_secs,
            )
            runs.append(r)
            p = r["proc"]
            print(
                f"  -> RSS={p.get('rss_mb')} MiB PSS={p.get('pss_mb')} MiB USS={p.get('uss_mb')} MiB "
                f"Peak={p.get('peak_rss_mb')} MiB CPU={p.get('cpu_pct')}% Wakeups={p.get('wakeups_per_sec')}/s "
                f"Thr={p.get('threads')} FDs={p.get('fds')} Startup={r['startup_ms']}ms",
                flush=True,
            )
            results[sc] = aggregate_runs(runs)
            flush()

    flush()
    print(f"\n[bench] wrote {res_json} and {out_dir / 'results.md'}", flush=True)


if __name__ == "__main__":
    main()
