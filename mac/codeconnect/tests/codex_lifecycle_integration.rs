//! End-to-end, real-process tests for the D7 launch coordination path, run
//! against an **isolated throwaway tmux server** (a private `-S <socket>` under
//! a temp dir — never the operator's live `codeconnect` server) and an isolated
//! `CODECONNECT_HOME`. These exercise the coordinator, the D6 exec gate, and the
//! custodian as the separate processes they really are.
//!
//! The load-bearing gate: **the launch outcome is owned even when the
//! coordinator is killed**. With the wrapper bring-up held (`--test-bringup
//! hang`), the coordinator is SIGKILLed *after* `tmux new-session`; the retained
//! custodian must then drive the record to `failed`, destroy the disposable tmux
//! session, and mark cleanup `complete` — with no guessed grace period.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn tmux_bin() -> Option<PathBuf> {
    for cand in [
        "/opt/homebrew/bin/tmux",
        "/usr/local/bin/tmux",
        "/usr/bin/tmux",
    ] {
        if Path::new(cand).exists() {
            return Some(PathBuf::from(cand));
        }
    }
    None
}

struct Sandbox {
    home: PathBuf,
    sock: PathBuf,
    tmux: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Sandbox {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("cc-life-{tag}-{}-{nanos}", std::process::id()));
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Sandbox {
            home,
            sock: base.join("tmux.sock"),
            tmux: tmux_bin().unwrap(),
        }
    }

    fn has_session(&self, name: &str) -> bool {
        Command::new(&self.tmux)
            .args([
                "-S",
                self.sock.to_str().unwrap(),
                "-f",
                "/dev/null",
                "has-session",
                "-t",
            ])
            .arg(format!("={name}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn spawn_coordinator(&self, uid: &str, bringup: &str) -> Child {
        self.spawn_coordinator_opts(uid, bringup, false)
    }

    /// Spawn the coordinator, optionally in the Principle-B "hang inside
    /// new-session" mode so it can be killed with tmux in flight.
    fn spawn_coordinator_opts(&self, uid: &str, bringup: &str, hang_newsession: bool) -> Child {
        let bin = env!("CARGO_BIN_EXE_codeconnect");
        let mut cmd = Command::new(bin);
        cmd.arg("internal-codex-coordinator")
            .args(["--uid", uid])
            .args(["--nonce", "launchnonce"])
            .args(["--custodian-nonce", "custnonce"])
            .args(["--session-name", "cc-1"])
            .args(["--cwd", "/tmp"])
            .args(["--tmux-socket", self.sock.to_str().unwrap()])
            .args(["--deadline-ms", "60000"])
            .args(["--poll-ms", "100"])
            .args(["--test-bringup", bringup]);
        if hang_newsession {
            cmd.args(["--test-newsession", "hang"]);
        }
        cmd.env("CODECONNECT_HOME", &self.home)
            .env("CODECONNECT_TMUX", &self.tmux)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn coordinator")
    }

    fn record_text(&self, uid: &str) -> Option<String> {
        std::fs::read_to_string(self.home.join("sessions").join(uid).join("launch.json")).ok()
    }

    /// The recorded custodian pid, or `None` before it is armed.
    fn custodian_pid(&self, uid: &str) -> Option<i32> {
        let text = self.record_text(uid)?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        v.get("custodian")?.get("pid")?.as_i64().map(|p| p as i32)
    }

    /// Run the ACTUAL gated `internal-codex-sweep` subcommand and return its exit
    /// status (finding 5: the sweep no longer always exits 0 — a rearm failure is
    /// surfaced as a non-zero exit, so callers can assert on it).
    fn run_sweep(&self) -> std::process::ExitStatus {
        let bin = env!("CARGO_BIN_EXE_codeconnect");
        Command::new(bin)
            .arg("internal-codex-sweep")
            .args(["--socket", self.sock.to_str().unwrap()])
            .env("CODECONNECT_HOME", &self.home)
            .env("CODECONNECT_TMUX", &self.tmux)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run sweep")
    }

    fn cleanup(&self) {
        Command::new(&self.tmux)
            .args(["-S", self.sock.to_str().unwrap(), "kill-server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
        if let Some(base) = self.home.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

/// Poll `f` until it is true or `within` elapses.
fn wait_until(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    f()
}

#[test]
fn a_ready_launch_creates_the_session_and_commits_ready() {
    if tmux_bin().is_none() {
        eprintln!("skipped: no tmux");
        return;
    }
    let sb = Sandbox::new("ready");
    let uid = "01JQXV9K7B8N4M2P6R3T5W9YQD";
    let mut coord = sb.spawn_coordinator(uid, "ready");

    // The coordinator should reach Ready, with the tmux session created.
    let ok = wait_until(Duration::from_secs(20), || {
        sb.record_text(uid)
            .map(|t| t.contains("\"Ready\""))
            .unwrap_or(false)
    });
    assert!(ok, "record: {:?}", sb.record_text(uid));
    assert!(
        sb.has_session("cc-1"),
        "the tmux session must exist at Ready"
    );
    // Round-5 finding 1: server A's identity is PERSISTED in the launch record
    // (the coordinator captured the resolved session+server and wrote it), so the
    // separate custodian/supervisor can bind cleanup/liveness to A — the
    // production pin is no longer None. It carries a proven server_birth.
    let record = sb.record_text(uid).expect("record exists");
    assert!(
        record.contains("\"server_a\"") && record.contains("\"server_birth\""),
        "server A (with a proven birth) must be persisted in the record: {record}"
    );
    let _ = coord.wait();

    // Cleanup: the session + custodian are ours to tear down here.
    sb.cleanup();
}

#[test]
fn killing_the_coordinator_after_new_session_lets_the_custodian_own_the_outcome() {
    if tmux_bin().is_none() {
        eprintln!("skipped: no tmux");
        return;
    }
    let sb = Sandbox::new("killcoord");
    let uid = "01JQXV9K7B8N4M2P6R3T5W9YQE";
    let mut coord = sb.spawn_coordinator(uid, "hang");

    // Wait until the coordinator has created the session and is hanging in
    // bring-up (record still Pending, tmux session present).
    let armed = wait_until(Duration::from_secs(20), || {
        sb.record_text(uid)
            .map(|t| t.contains("\"Pending\"") && t.contains("custodian"))
            .unwrap_or(false)
            && sb.has_session("cc-1")
    });
    assert!(
        armed,
        "coordinator should have armed + created the session: {:?}",
        sb.record_text(uid)
    );

    // Kill ONLY the coordinator (not its process group — the custodian is in its
    // own group via the exec gate and must survive).
    unsafe {
        libc::kill(coord.id() as i32, libc::SIGKILL);
    }
    let _ = coord.wait();

    // The retained custodian must now drive the record to Failed and clean up.
    let resolved = wait_until(Duration::from_secs(30), || {
        sb.record_text(uid)
            .map(|t| t.contains("\"Failed\"") && t.contains("\"Complete\""))
            .unwrap_or(false)
    });
    assert!(
        resolved,
        "custodian must fail + complete cleanup: {:?}",
        sb.record_text(uid)
    );
    // …and the disposable tmux session must be gone.
    let gone = wait_until(Duration::from_secs(10), || !sb.has_session("cc-1"));
    assert!(gone, "the custodian must destroy the disposable session");

    sb.cleanup();
}

#[test]
fn killing_the_coordinator_during_new_session_keeps_the_custodian_armed_to_clean_up() {
    // Principle B (durable-before-mutation): `new_session_indeterminate` is
    // fsynced BEFORE `tmux new-session`, so a coordinator killed **while tmux is
    // in flight** leaves a record that already says "indeterminate" and keeps the
    // custodian armed. Here the coordinator hangs *inside* new-session (session
    // already created, flag still set) and is SIGKILLed there; the retained
    // custodian must fail the launch and clean the (late) session.
    if tmux_bin().is_none() {
        eprintln!("skipped: no tmux");
        return;
    }
    let sb = Sandbox::new("killduring");
    let uid = "01JQXV9K7B8N4M2P6R3T5W9YQG";
    let mut coord = sb.spawn_coordinator_opts(uid, "ready", true);

    // Wait until: the durable flag is set, the custodian is armed, and the tmux
    // session exists — i.e. the coordinator is hanging inside new-session with
    // tmux "in flight".
    let armed = wait_until(Duration::from_secs(20), || {
        sb.record_text(uid)
            .map(|t| {
                t.contains("\"Pending\"")
                    && t.contains("\"new_session_indeterminate\": true")
                    && t.contains("custodian")
            })
            .unwrap_or(false)
            && sb.has_session("cc-1")
    });
    assert!(
        armed,
        "the coordinator should be hanging in new-session with the flag set: {:?}",
        sb.record_text(uid)
    );

    // Kill ONLY the coordinator, mid-new-session.
    unsafe {
        libc::kill(coord.id() as i32, libc::SIGKILL);
    }
    let _ = coord.wait();

    // The retained custodian fails the launch and cleans up the session that was
    // in flight — proving the durable flag kept it armed across the kill.
    let resolved = wait_until(Duration::from_secs(30), || {
        sb.record_text(uid)
            .map(|t| t.contains("\"Failed\"") && t.contains("\"Complete\""))
            .unwrap_or(false)
    });
    assert!(
        resolved,
        "the custodian must fail + complete cleanup after a mid-new-session kill: {:?}",
        sb.record_text(uid)
    );
    let gone = wait_until(Duration::from_secs(10), || !sb.has_session("cc-1"));
    assert!(
        gone,
        "the in-flight session must be destroyed by the custodian"
    );

    sb.cleanup();
}

#[test]
fn both_guardians_killed_then_the_sweep_rearms_a_custodian_that_cleans_up() {
    if tmux_bin().is_none() {
        eprintln!("skipped: no tmux");
        return;
    }
    let sb = Sandbox::new("bothkilled");
    let uid = "01JQXV9K7B8N4M2P6R3T5W9YQF";
    let mut coord = sb.spawn_coordinator(uid, "hang");

    // Wait until armed + session created (record Pending, custodian recorded).
    let armed = wait_until(Duration::from_secs(20), || {
        sb.custodian_pid(uid).is_some() && sb.has_session("cc-1")
    });
    assert!(
        armed,
        "coordinator should arm + create the session: {:?}",
        sb.record_text(uid)
    );
    let cust_pid = sb.custodian_pid(uid).unwrap();

    // Kill BOTH guardians: the coordinator and the custodian. The record is now
    // an orphaned Pending with a live tmux session and nobody to reap it.
    unsafe {
        libc::kill(coord.id() as i32, libc::SIGKILL);
        libc::kill(cust_pid, libc::SIGKILL);
    }
    let _ = coord.wait();
    // Confirm the custodian is really gone before sweeping.
    let dead = wait_until(Duration::from_secs(5), || unsafe {
        libc::kill(cust_pid, 0) != 0
    });
    assert!(dead, "the custodian must be dead before the sweep");

    // The recovery sweep: fails the orphaned pending and rearms a replacement
    // custodian, which then destroys the disposable session and completes. The
    // ACTUAL gated `internal-codex-sweep` subcommand must exit 0 when it rearms
    // successfully (finding 5: its outcome is surfaced, not swallowed).
    let sweep_status = sb.run_sweep();
    assert!(
        sweep_status.success(),
        "the gated sweep rearmed a custodian, so it must exit 0: {sweep_status:?}"
    );
    let resolved = wait_until(Duration::from_secs(30), || {
        sb.record_text(uid)
            .map(|t| t.contains("\"Failed\"") && t.contains("\"Complete\""))
            .unwrap_or(false)
            && !sb.has_session("cc-1")
    });
    assert!(
        resolved,
        "the sweep must rearm a custodian that fails + cleans up: {:?}",
        sb.record_text(uid)
    );

    sb.cleanup();
}
