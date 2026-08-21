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
//!
//! # What changed in 2e-2b, and why these tests now need a codex
//!
//! The pane no longer runs a `/bin/sh` placeholder: it runs the **real**
//! `internal-codex-host`, and the coordinator commits `ready` only after
//! observing that host's own evidence (both broker legs bound under the run dir,
//! the pane's session still proven ours). So a bring-up here is a real bring-up,
//! and it needs something to exec.
//!
//! It does not need the *real* codex. These are lifecycle tests — what is under
//! test is the coordinator/custodian's ownership of the launch, not codex's
//! protocol behaviour — so they use the same **fake codex** shape
//! `codex_host_fatal.rs` uses: a python3 script that binds the app-server socket
//! in `app-server` mode and sleeps otherwise. That is enough for the real host to
//! come all the way up and bind its two broker legs, which is exactly the
//! evidence the coordinator waits for. Proving the host against the real binary
//! is `live_codex_host.rs` and `live_codex_coordinator.rs`, both gated on
//! `CC_CODEX_LIVE=1`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Locate tmux, or **fail the test**.
///
/// Deliberately a panic rather than a skip, for exactly the reason stated below
/// for python3: this file used to open every test with `if tmux_bin().is_none() {
/// return }`, which on a tmux-less machine reports five passes having executed
/// nothing. tmux is not an optional dependency of a suite whose entire subject is
/// what happens inside a tmux pane.
/// Per-sandbox socket sequence.
///
/// A counter, not a timestamp. The first attempt used `SystemTime` nanos and two
/// sandboxes constructed on parallel test threads got the **same** value —
/// `SystemTime` is not nanosecond-unique on this platform — so they shared one
/// tmux server and the second `cc-keepalive` failed as a duplicate session. A
/// process-local counter cannot collide, and keeps the path short enough for
/// SUN_LEN.
static SOCK_SEQ: AtomicU32 = AtomicU32::new(0);

fn tmux_bin() -> PathBuf {
    for cand in [
        "/opt/homebrew/bin/tmux",
        "/usr/local/bin/tmux",
        "/usr/bin/tmux",
    ] {
        if Path::new(cand).exists() {
            return PathBuf::from(cand);
        }
    }
    if let Ok(out) = Command::new("/usr/bin/which").arg("tmux").output() {
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        if path.is_file() {
            return path;
        }
    }
    panic!(
        "no tmux found (/opt/homebrew/bin, /usr/local/bin, /usr/bin, or PATH). These gates \
         drive real tmux sessions, and reporting them green having executed nothing is the \
         vacuous pass every gate in this repo forbids."
    )
}

/// Locate python3 for the fake codex, or **fail the test**. Mirrors
/// `codex_host_fatal.rs`: a lifecycle suite that reports green having executed
/// nothing is the vacuous pass every gate in this repo forbids.
fn python3() -> PathBuf {
    for cand in ["/usr/bin/python3", "/opt/homebrew/bin/python3"] {
        if Path::new(cand).exists() {
            return PathBuf::from(cand);
        }
    }
    if let Ok(out) = Command::new("/usr/bin/which").arg("python3").output() {
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        if path.is_file() {
            return path;
        }
    }
    panic!(
        "no python3 found (/usr/bin/python3, /opt/homebrew/bin/python3, or PATH). \
         These gates need it to build the fake codex the pane's host execs."
    )
}

struct Sandbox {
    home: PathBuf,
    sock: PathBuf,
    tmux: PathBuf,
    /// The fake `codex` the pane's host execs for both children.
    codex: PathBuf,
    /// The isolated `CODEX_HOME` handed to the host.
    codex_home: PathBuf,
    /// The uid this sandbox's launch uses, so [`Drop`] can find the run dir.
    uid: String,
    /// This sandbox's launch nonce. Distinct per test **because the run dir is
    /// named from the uid prefix plus the nonce**: these tests use hand-written
    /// ULIDs that share their first ten characters, so a shared nonce would send
    /// two tests at the same `/tmp` directory — which the host, correctly,
    /// refuses to adopt. Production nonces are 128 random bits and never collide.
    nonce: String,
}

impl Sandbox {
    fn new(tag: &str, uid: &str) -> Sandbox {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("cc-life-{tag}-{}-{nanos}", std::process::id()));
        let home = base.join("home");
        let codex_home = base.join("codexhome");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&codex_home).unwrap();
        let codex = write_fake_codex(&base, &python3());
        Sandbox {
            home,
            // The tmux socket is a UNIX SOCKET, so it is bound by SUN_LEN (104)
            // like every other socket in this chunk — and the macOS temp dir is
            // long enough that `<tempdir>/cc-life-<tag>-<pid>-<nanos>/tmux.sock`
            // depends on the tag's length to fit. It does not fit for a tag as
            // ordinary as "lastsession": tmux answers `File name too long`, the
            // coordinator records a new-session failure, and the test fails for a
            // reason that has nothing to do with what it tests. Short `/tmp` path,
            // like every other socket here.
            sock: PathBuf::from(format!(
                "/tmp/cclife.{}.{}.sock",
                std::process::id(),
                SOCK_SEQ.fetch_add(1, Ordering::SeqCst)
            )),
            tmux: tmux_bin(),
            codex,
            codex_home,
            uid: uid.to_string(),
            nonce: format!("{tag}{}", nanos % 1_000_000_007),
        }
    }

    /// The run dir the coordinator will choose for `uid` — the same name
    /// `codex_coordinator::choose_run_dir` derives, restated here rather than
    /// imported because integration tests link the binary, not a library. The
    /// record is asserted to agree with it, so a drift shows up as a failure
    /// rather than as a test quietly watching the wrong directory.
    fn expected_run_dir(&self, uid: &str) -> PathBuf {
        // The uid contributes its LAST ten alphanumerics (a ULID's random half),
        // the nonce its first sixteen — mirroring `choose_run_dir`.
        let kept: Vec<char> = uid.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let uid_slug: String = kept[kept.len().saturating_sub(10)..].iter().collect();
        let nonce_slug: String = self
            .nonce
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(16)
            .collect();
        PathBuf::from(format!("/tmp/cch.{uid_slug}.{nonce_slug}"))
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

    /// Put an unrelated session on this sandbox's tmux server and keep it there.
    ///
    /// **Every test needs this, and the reason is a real property, not tidiness.**
    /// Production runs every CodeConnect session on one shared server, so the
    /// server outlives any individual session. A private per-test server does not:
    /// when its last session dies the server exits with it, the socket goes away,
    /// and `destroy_owned_session` can only answer `Unavailable` — because a
    /// server that does not answer is never proof our uid is absent (tmux.rs,
    /// round-4 finding 1 / round-5 finding 4). The custodian then retries that
    /// forever and never reaches `Complete`.
    ///
    /// That rule is correct and this suite is not the place to argue with it. What
    /// the rule means is that a **single-session** tmux server is a topology where
    /// cleanup can never be proven — a genuine product residual, reachable in
    /// production only by the very first session on a fresh server. Measured here:
    /// without a keepalive, `killing_the_coordinator_after_new_session…` failed
    /// roughly 1 run in 15, and it began failing only once the pane ran the real
    /// host, whose teardown takes long enough to widen the window.
    ///
    /// So the keepalive is what makes these tests run against production's
    /// topology instead of an artificial one.
    fn keepalive(&self) {
        let out = Command::new(&self.tmux)
            .args([
                "-S",
                self.sock.to_str().unwrap(),
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "cc-keepalive",
                "--",
                "/bin/sh",
                "-c",
                "while :; do sleep 1; done",
            ])
            .stdin(Stdio::null())
            .output()
            .expect("run tmux");
        assert!(
            out.status.success(),
            "the keepalive session must be created (socket {}): {}",
            self.sock.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    fn spawn_coordinator(&self, uid: &str) -> Child {
        self.spawn_coordinator_opts(uid, &[])
    }

    /// Spawn the coordinator with the full charter, plus any test-only hang
    /// injection the caller needs.
    fn spawn_coordinator_opts(&self, uid: &str, extra: &[&str]) -> Child {
        let bin = env!("CARGO_BIN_EXE_codeconnect");
        let mut cmd = Command::new(bin);
        cmd.arg("internal-codex-coordinator")
            .args(["--uid", uid])
            .args(["--nonce", &self.nonce])
            .args(["--custodian-nonce", "custnonce"])
            .args(["--session-name", "cc-1"])
            .args(["--cwd", "/tmp"])
            .args(["--tmux-socket", self.sock.to_str().unwrap()])
            .args(["--deadline-ms", "60000"])
            // The seven dimensions the host applies no default to. The
            // coordinator carries them verbatim into the pane command.
            .args(["--codex", self.codex.to_str().unwrap()])
            .args(["--codex-home", self.codex_home.to_str().unwrap()])
            .args(["--approval-policy", "untrusted"])
            .args(["--approvals-reviewer", "user"])
            .args(["--sandbox", "read-only"])
            .args(["--hooks-enabled", "true"])
            .args(extra);
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

    /// The recorded custodian's full `(pid, birth)` identity — the only thing
    /// this harness will signal.
    fn custodian_identity(&self, uid: &str) -> Option<protocol::proc_identity::ProcessIdentity> {
        self.recorded_identity(uid, "custodian")
    }

    /// The recorded coordinator's identity.
    fn coordinator_identity(&self, uid: &str) -> Option<protocol::proc_identity::ProcessIdentity> {
        self.recorded_identity(uid, "coordinator")
    }

    fn recorded_identity(
        &self,
        uid: &str,
        field: &str,
    ) -> Option<protocol::proc_identity::ProcessIdentity> {
        let v: serde_json::Value = serde_json::from_str(&self.record_text(uid)?).ok()?;
        let c = v.get(field)?;
        Some(protocol::proc_identity::ProcessIdentity {
            pid: c.get("pid")?.as_i64()? as i32,
            birth: protocol::proc_identity::BirthIdentity {
                start_sec: c.get("birth")?.get("start_sec")?.as_i64()?,
                start_usec: c.get("birth")?.get("start_usec")?.as_i64()?,
            },
        })
    }

    /// The recorded custodian pid, or `None` before it is armed.
    fn custodian_pid(&self, uid: &str) -> Option<i32> {
        let text = self.record_text(uid)?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        v.get("custodian")?.get("pid")?.as_i64().map(|p| p as i32)
    }

    /// The record's `state`, as an exact JSON discriminant: `"Pending"`,
    /// `"Ready"`, or `"Failed"`. Parsed rather than substring-matched — a record
    /// containing the text `Failed` inside a *reason string* would satisfy
    /// `contains("\"Failed\"")` while the state was something else entirely.
    fn state(&self, uid: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(&self.record_text(uid)?).ok()?;
        match v.get("state")? {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Object(o) => o.keys().next().cloned(),
            _ => None,
        }
    }

    /// The record's `cleanup` field, exactly.
    fn cleanup_state(&self, uid: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(&self.record_text(uid)?).ok()?;
        v.get("cleanup")?.as_str().map(|s| s.to_string())
    }

    /// Terminal cleanup, by exact fields rather than substrings.
    fn failed_and_complete(&self, uid: &str) -> bool {
        self.state(uid).as_deref() == Some("Failed")
            && self.cleanup_state(uid).as_deref() == Some("Complete")
    }

    /// Every child the HOST recorded, as `(role, pid, pgid)`.
    fn host_children(&self, uid: &str) -> Vec<(String, i32, i32)> {
        let Some(text) = self.record_text(uid) else {
            return Vec::new();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Vec::new();
        };
        v.get("children")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| {
                        let role = c.get("role")?.as_str()?.to_string();
                        if role == "custodian" {
                            return None;
                        }
                        let pid = c.get("identity")?.get("pid")?.as_i64()? as i32;
                        let pgid = c.get("pgid")?.as_i64()? as i32;
                        Some((role, pid, pgid))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The run dir the coordinator recorded, read back out of the record.
    fn recorded_run_dir(&self, uid: &str) -> Option<String> {
        let text = self.record_text(uid)?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        v.get("run_dir")?.as_str().map(|s| s.to_string())
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

    /// Tear everything down. Called from [`Drop`], so it runs on the failure path
    /// too — which is the path that matters: an assertion that panics past a
    /// manual cleanup call leaks a tmux server, and the keepalive session means
    /// that server never exits on its own.
    fn cleanup(&self, uid: &str) {
        // Kill the custodian FIRST, while its record still exists to name it.
        //
        // This is not tidiness. A custodian outlives the coordinator by design,
        // and a test that ends with the launch still `ready` leaves one running:
        // it then finds its tmux server killed (an unanswered socket, which
        // round-4 forbids reading as absence) and its record deleted (a load
        // error, retried by design), so it retries **forever** — a spinning
        // process per test run, accumulating across the day. Measured on this
        // machine before this line existed: 75 of them.
        // The coordinator too, and for a reason worth stating: a test that spawns
        // it with `--test-bringup hang` leaves it blocked forever, and the tag
        // sweep below cannot reach it — the coordinator DERIVES the run dir rather
        // than carrying it in its argv, so it never matches. A test body that
        // panics before its own kill therefore used to leak a hung coordinator
        // per run (observed). The record names it, so cleanup can too.
        if let Some(identity) = self.coordinator_identity(uid) {
            if protocol::proc_identity::liveness(&identity)
                == protocol::proc_identity::Liveness::Alive
            {
                unsafe {
                    libc::kill(identity.pid, libc::SIGKILL);
                }
            }
        }
        // Signalled by VERIFIED identity, not by the bare pid in the record.
        //
        // By the time `Drop` runs the custodian has usually exited on its own, and
        // its pid may have been recycled — sending SIGKILL to a recorded number
        // would then kill somebody else's process. The record stores the birth
        // identity precisely so this can be checked, and the production code makes
        // the same check before every signal; a harness that skipped it would be
        // modelling something the product does not do.
        if let Some(identity) = self.custodian_identity(uid) {
            if protocol::proc_identity::liveness(&identity)
                == protocol::proc_identity::Liveness::Alive
            {
                unsafe {
                    libc::kill(identity.pid, libc::SIGKILL);
                }
            }
        }
        Command::new(&self.tmux)
            .args(["-S", self.sock.to_str().unwrap(), "kill-server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
        // Nothing that carries this launch's run dir in its argv may outlive the
        // test, whichever way the assertions went.
        for pid in tagged_pids(self.expected_run_dir(uid).to_str().unwrap()) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_dir_all(self.expected_run_dir(uid));
        let _ = std::fs::remove_file(&self.sock);
        if let Some(base) = self.home.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let uid = self.uid.clone();
        self.cleanup(&uid);
    }
}

/// Write the fake `codex` into `dir` and return its path. Same shape as
/// `codex_host_fatal.rs`'s: `app-server --listen unix://P` binds P, chmods it
/// 0600 (past the real app-server's bind→chmod race, which is what the host's
/// readiness gate waits for) and sleeps; every other invocation — the TUI — just
/// sleeps, holding the session open.
fn write_fake_codex(dir: &Path, python: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        r#"#!{}
import os, socket, sys, time

argv = sys.argv[1:]
if argv and argv[0] == "app-server":
    path = None
    for i, a in enumerate(argv):
        if a == "--listen" and i + 1 < len(argv):
            path = argv[i + 1]
    if path is None or not path.startswith("unix://"):
        sys.stderr.write("fake-codex: no --listen unix://PATH in %r\n" % (argv,))
        sys.exit(2)
    path = path[len("unix://"):]
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.bind(path)
    os.chmod(path, 0o600)
    s.listen(16)
    # A descendant in the SAME process group as the app-server. The host puts the
    # app-server in its own group precisely so cleanup can kill the group; this
    # child is what makes that observable, because a pid-only kill leaves it
    # running and the leak assertions then fail.
    if os.fork() == 0:
        while True:
            time.sleep(3600)

while True:
    time.sleep(3600)
"#,
        python.display()
    );
    let path = dir.join("fake-codex");
    std::fs::write(&path, script).expect("write fake codex");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .expect("chmod fake codex");
    path
}

/// `(pid, command)` for every process whose full command line contains `tag`,
/// excluding this test process. `-ww` disables ps's column truncation so a tag
/// deep in a long argv is still matched.
///
/// What it proves is precisely **no visible process references `tag`** — not
/// universal descendant absence. A descendant that re-execed without the run dir
/// in its argv would not be seen. Fails closed: a `ps` that cannot run panics
/// rather than reporting an empty list, because every leak assertion here reads
/// empty as proof that nothing was left behind.
fn processes_referencing(tag: &str) -> Vec<(i32, String)> {
    let me = std::process::id() as i32;
    let out = Command::new("/bin/ps")
        .args(["-Axww", "-o", "pid=,command="])
        .output()
        .expect(
            "run /bin/ps — a scan that cannot run must FAIL the test, never report 'nothing found'",
        );
    assert!(out.status.success(), "/bin/ps exited {}", out.status);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, cmd) = line.trim_start().split_once(char::is_whitespace)?;
            let pid: i32 = pid.trim().parse().ok()?;
            (pid != me && cmd.contains(tag)).then(|| (pid, cmd.to_string()))
        })
        .collect()
}

/// The tolerant twin, for cleanup paths where a panic would abort a test binary
/// mid-unwind and hide the real failure. Nothing asserts on its result.
fn tagged_pids(tag: &str) -> Vec<i32> {
    let me = std::process::id() as i32;
    let Ok(out) = Command::new("/bin/ps")
        .args(["-Axww", "-o", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, cmd) = line.trim_start().split_once(char::is_whitespace)?;
            let pid: i32 = pid.trim().parse().ok()?;
            (pid != me && cmd.contains(tag)).then_some(pid)
        })
        .collect()
}

/// What the world looks like at a failed wait: the custodian's identity and
/// whether it is still breathing, what tmux thinks it is serving, and anything
/// still holding the run dir.
///
/// Attached to the cleanup assertions because their failure mode is "nothing
/// happened", and a record dump alone cannot distinguish a custodian that died
/// from one that is alive and stuck — which are opposite bugs. Both were hit
/// while building this chunk, and this is what told them apart.
fn diagnose(sb: &Sandbox, uid: &str) -> String {
    let cust = sb.custodian_pid(uid);
    let cust_alive = cust.map(|p| unsafe { libc::kill(p, 0) == 0 });
    let sessions = Command::new(&sb.tmux)
        .args([
            "-S",
            sb.sock.to_str().unwrap(),
            "-f",
            "/dev/null",
            "list-sessions",
        ])
        .output()
        .map(|o| {
            format!(
                "rc={} out={:?} err={:?}",
                o.status,
                String::from_utf8_lossy(&o.stdout).trim().to_string(),
                String::from_utf8_lossy(&o.stderr).trim().to_string()
            )
        })
        .unwrap_or_else(|e| format!("<{e}>"));
    let procs = processes_referencing(sb.expected_run_dir(uid).to_str().unwrap());
    format!("\n  custodian={cust:?} alive={cust_alive:?}\n  tmux: {sessions}\n  procs: {procs:?}")
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

/// Both broker legs are bound under `run` — the host's step 2 complete, and the
/// exact evidence the coordinator's bring-up waits for.
fn broker_legs_bound(run: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    ["tui.sock", "ccd.sock"].iter().all(|leg| {
        std::fs::metadata(run.join(leg))
            .map(|m| m.file_type().is_socket())
            .unwrap_or(false)
    })
}

#[test]
fn a_ready_launch_runs_the_real_host_in_the_pane_and_commits_ready() {
    // Evolved from `a_ready_launch_creates_the_session_and_commits_ready`. It used
    // to pass `--test-bringup ready`, which committed `ready` on nothing at all.
    // That switch is gone: the pane runs the real `internal-codex-host` and the
    // coordinator commits only after observing its evidence — so this test now
    // proves the bring-up rather than asserting around it.
    let uid = "01JQXV9K7B8N4M2P6R3T5W9YQD";
    let sb = Sandbox::new("ready", uid);
    // Production's tmux server is shared; make this one shared too (see
    // `keepalive`), so the custodian can prove cleanup rather than retry forever.
    sb.keepalive();
    let run = sb.expected_run_dir(uid);
    let mut coord = sb.spawn_coordinator(uid);

    // The run dir is durable evidence, written BEFORE new-session, and it is the
    // path the custodian will sweep — so the record must name the directory the
    // host actually got. Checked first: it lands before the mutation, so it is
    // there long before anything else below.
    let recorded = wait_until(Duration::from_secs(30), || {
        sb.recorded_run_dir(uid).is_some()
    });
    assert!(
        recorded,
        "the coordinator must record its run dir before new-session: {:?}",
        sb.record_text(uid)
    );
    assert_eq!(
        sb.recorded_run_dir(uid).as_deref(),
        run.to_str(),
        "the record must name the run dir the pane's host was given"
    );

    // The real host comes up in the pane and binds both broker legs — the exact
    // evidence the coordinator's bring-up waits for.
    //
    // Observed BEFORE waiting on Ready, and the order is load-bearing. Committing
    // `Ready` is what *starts* the teardown here: the coordinator exits, and a
    // `ready` record whose coordinator is proven gone is session-fatal, so the
    // custodian destroys the session and the run dir goes with it. Asserting the
    // legs *after* Ready therefore races the teardown and loses — measured, 2 runs
    // in 20. Waiting for them first is not a workaround: the legs are bound
    // strictly before Ready is committed, so this is simply observing the
    // precondition at the point it exists.
    let legs = wait_until(Duration::from_secs(30), || broker_legs_bound(&run));
    assert!(
        legs,
        "the pane's host must bind tui.sock + ccd.sock under {}: {:?}",
        run.display(),
        std::fs::read_dir(&run).map(|d| d.flatten().count())
    );

    // …and only then does the coordinator commit Ready.
    let ok = wait_until(Duration::from_secs(30), || {
        sb.state(uid).as_deref() == Some("Ready")
    });
    assert!(ok, "record: {:?}", sb.record_text(uid));
    // Round-5 finding 1: server A's identity is PERSISTED in the launch record
    // (the coordinator captured the resolved session+server and wrote it), so the
    // separate custodian/supervisor can bind cleanup/liveness to A — the
    // production pin is no longer None. It carries a proven server_birth.
    let record = sb.record_text(uid).expect("record exists");
    assert!(
        record.contains("\"server_a\"") && record.contains("\"server_birth\""),
        "server A (with a proven birth) must be persisted in the record: {record}"
    );
    // Bounded. Every other `coord.wait()` in this file follows a SIGKILL, so it
    // returns as fast as the kernel reaps; this one waits on a coordinator exiting
    // of its own accord, which is exactly the case that can fail to happen. A
    // coordinator still running after `ready` is a finding, not a reason to block
    // the suite forever.
    assert!(
        wait_until(Duration::from_secs(30), || matches!(
            coord.try_wait(),
            Ok(Some(_))
        )),
        "the coordinator must exit once it has committed ready; it is still running"
    );
}

#[test]
fn killing_the_coordinator_after_new_session_lets_the_custodian_own_the_outcome() {
    let uid = "01JQXV9K7B8N4M2P6R3T5W9YQE";
    let sb = Sandbox::new("killcoord", uid);
    // Production's tmux server is shared; make this one shared too (see
    // `keepalive`), so the custodian can prove cleanup rather than retry forever.
    sb.keepalive();
    // `--test-bringup hang` holds the coordinator at the bring-up boundary, after
    // new-session. It is a HANG, not a fake readiness: there is no spelling of
    // this flag that reports `Ready` without the host's evidence.
    let mut coord = sb.spawn_coordinator_opts(uid, &["--test-bringup", "hang"]);

    // Wait until the coordinator has created the session and is hanging in
    // bring-up (record still Pending, tmux session present).
    let armed = wait_until(Duration::from_secs(30), || {
        sb.state(uid).as_deref() == Some("Pending")
            && sb.custodian_pid(uid).is_some()
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
    let resolved = wait_until(Duration::from_secs(30), || sb.failed_and_complete(uid));
    assert!(
        resolved,
        "custodian must fail + complete cleanup: {:?}{}",
        sb.record_text(uid),
        diagnose(&sb, uid)
    );
    // …and the disposable tmux session must be gone.
    let gone = wait_until(Duration::from_secs(10), || !sb.has_session("cc-1"));
    assert!(gone, "the custodian must destroy the disposable session");
}

#[test]
fn killing_the_coordinator_during_new_session_keeps_the_custodian_armed_to_clean_up() {
    // Principle B (durable-before-mutation): `new_session_indeterminate` is
    // fsynced BEFORE `tmux new-session`, so a coordinator killed **while tmux is
    // in flight** leaves a record that already says "indeterminate" and keeps the
    // custodian armed. Here the coordinator hangs *inside* new-session (session
    // already created, flag still set) and is SIGKILLed there; the retained
    // custodian must fail the launch and clean the (late) session.
    let uid = "01JQXV9K7B8N4M2P6R3T5W9YQG";
    let sb = Sandbox::new("killduring", uid);
    // Production's tmux server is shared; make this one shared too (see
    // `keepalive`), so the custodian can prove cleanup rather than retry forever.
    sb.keepalive();
    // Only the new-session hang is passed now. The bring-up mode this test used
    // to carry (`ready`) was never reached — the hang happens first — and no
    // longer exists to pass.
    let mut coord = sb.spawn_coordinator_opts(uid, &["--test-newsession", "hang"]);

    // Wait until: the durable flag is set, the custodian is armed, and the tmux
    // session exists — i.e. the coordinator is hanging inside new-session with
    // tmux "in flight". The run dir is recorded by now too: it is written under
    // the same lock as the flag, before the mutation.
    let armed = wait_until(Duration::from_secs(30), || {
        sb.record_text(uid)
            .map(|t| t.contains("\"new_session_indeterminate\": true"))
            .unwrap_or(false)
            && sb.state(uid).as_deref() == Some("Pending")
            && sb.custodian_pid(uid).is_some()
            && sb.has_session("cc-1")
    });
    assert!(
        armed,
        "the coordinator should be hanging in new-session with the flag set: {:?}",
        sb.record_text(uid)
    );
    assert_eq!(
        sb.recorded_run_dir(uid).as_deref(),
        sb.expected_run_dir(uid).to_str(),
        "the run dir must be durable BEFORE the mutation, so a coordinator killed \
         with tmux in flight still leaves it nameable"
    );

    // Kill ONLY the coordinator, mid-new-session.
    unsafe {
        libc::kill(coord.id() as i32, libc::SIGKILL);
    }
    let _ = coord.wait();

    // The retained custodian fails the launch and cleans up the session that was
    // in flight — proving the durable flag kept it armed across the kill.
    let resolved = wait_until(Duration::from_secs(30), || sb.failed_and_complete(uid));
    assert!(
        resolved,
        "the custodian must fail + complete cleanup after a mid-new-session kill: {:?}{}",
        sb.record_text(uid),
        diagnose(&sb, uid)
    );
    let gone = wait_until(Duration::from_secs(10), || !sb.has_session("cc-1"));
    assert!(
        gone,
        "the in-flight session must be destroyed by the custodian"
    );
}

#[test]
fn both_guardians_killed_then_the_sweep_rearms_a_custodian_that_cleans_up() {
    let uid = "01JQXV9K7B8N4M2P6R3T5W9YQF";
    let sb = Sandbox::new("bothkilled", uid);
    // Production's tmux server is shared; make this one shared too (see
    // `keepalive`), so the custodian can prove cleanup rather than retry forever.
    sb.keepalive();
    let mut coord = sb.spawn_coordinator_opts(uid, &["--test-bringup", "hang"]);

    // Wait until the launch is fully up: custodian armed, session created, and the
    // pane's host admitted with BOTH children recorded.
    //
    // Waiting for the host — not just the session — is what gives this test
    // something to reap. Killing the guardians earlier leaves the host refused at
    // the gate (its coordinator is already gone), so it records no children and the
    // rearmed custodian has no identities to act on; the cleanup assertions below
    // would then pass vacuously against an empty list.
    let run = sb.expected_run_dir(uid);
    let armed = wait_until(Duration::from_secs(30), || {
        sb.custodian_pid(uid).is_some()
            && sb.has_session("cc-1")
            // Both children recorded, not merely the legs bound: the TUI is
            // spawned AFTER the legs, so waiting on the legs alone leaves a window
            // where only the app-server is in the record.
            && sb.host_children(uid).len() == 2
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
    // custodian, which then destroys the disposable session and completes.
    //
    // Swept in a LOOP, because that is what production does and because a single
    // pass is genuinely allowed to do nothing: `recovery_sweep` skips a record
    // whose launch lock is held, and the pane's host holds it for a moment while
    // it takes its admission lease. A one-shot sweep that happened to land in that
    // window used to exit 0 having examined nothing, and the test then waited out
    // its whole budget for a rearm that was never going to come — which is exactly
    // the "did everything" / "did nothing" confusion the sweep now reports as a
    // non-zero exit. Loop until a pass exits 0, and assert that one arrives.
    let swept = wait_until(Duration::from_secs(20), || sb.run_sweep().success());
    assert!(
        swept,
        "a sweep pass must eventually examine the record and exit 0: {:?}{}",
        sb.record_text(uid),
        diagnose(&sb, uid)
    );
    let resolved = wait_until(Duration::from_secs(30), || {
        sb.failed_and_complete(uid) && !sb.has_session("cc-1")
    });
    assert!(
        resolved,
        "the sweep must rearm a custodian that fails + cleans up: {:?}{}",
        sb.record_text(uid),
        diagnose(&sb, uid)
    );

    // Cleanup means all three things, asserted separately rather than inferred
    // from the `Complete` marker — the marker is what the custodian WRITES, and
    // the point of these is to check it was entitled to.
    assert!(
        wait_until(Duration::from_secs(10), || !run.exists()),
        "the rearmed custodian must sweep the run dir: {}",
        run.display()
    );
    assert_recorded_children_dead(&sb, uid);
    let leftover = processes_referencing(run.to_str().unwrap());
    assert!(
        leftover.is_empty(),
        "nothing may still reference the run dir: {leftover:?}"
    );
}

/// Every child the host recorded — by the exact `(pid, pgid)` in the record — is
/// gone, and so is its process group.
///
/// Reads the identities the custodian itself acted on, so a teardown that
/// signalled the wrong thing (or nothing) fails here rather than passing because
/// some unrelated process happened to exit.
fn assert_recorded_children_dead(sb: &Sandbox, uid: &str) {
    let children = sb.host_children(uid);
    // BOTH roles, and non-empty. An empty list would satisfy every loop below
    // vacuously, so a regression that stopped recording children entirely — the
    // exact thing that makes cleanup impossible — would turn this gate green.
    for role in ["app-server", "tui"] {
        assert!(
            children.iter().any(|(r, _, _)| r == role),
            "the record must name a {role} for cleanup to act on; recorded: {children:?}"
        );
    }
    for (role, pid, pgid) in &children {
        let dead = wait_until(Duration::from_secs(10), || unsafe {
            libc::kill(*pid, 0) != 0
        });
        assert!(
            dead,
            "the recorded {role} (pid {pid}) is still alive after cleanup completed"
        );
        // A child that led its own group must have had that GROUP reaped, not
        // just its leader — that is the whole reason the pgid is recorded.
        if pgid == pid {
            let group_gone = wait_until(Duration::from_secs(10), || unsafe {
                libc::kill(-*pgid, 0) != 0
            });
            assert!(
                group_gone,
                "the recorded {role}'s process group ({pgid}) still has members after cleanup"
            );
        }
    }
    println!("  recorded children all dead: {children:?}");
}

#[test]
fn a_sigkilled_host_leaves_the_run_dir_for_the_custodian_to_sweep() {
    // The one case the host's own cleanup cannot cover, and therefore the reason
    // the custodian sweeps at all. The host removes its run dir on every exit
    // path *it* takes — so to prove the backstop, the host must be denied an exit
    // path: SIGKILL it, and only the custodian is left to remove the directory.
    //
    // This also walks the orphan story end to end. Killing the host ends the pane
    // command, so tmux hangs the pane up; the two fake-codex children are in the
    // pane's process group (the host neither `setsid`s nor sets a process group),
    // so the SIGHUP reaches them and no visible process is left referencing the
    // run dir.
    let uid = "01JQXV9K7B8N4M2P6R3T5W9YQH";
    let sb = Sandbox::new("hostkill", uid);
    // Production's tmux server is shared; make this one shared too (see
    // `keepalive`), so the custodian can prove cleanup rather than retry forever.
    sb.keepalive();
    let run = sb.expected_run_dir(uid);
    let tag = run.to_str().unwrap().to_string();
    // Hold the coordinator at the bring-up boundary so the record stays Pending
    // while the host is killed — the custodian's coordinator-loss path then owns
    // the whole cleanup, sweep included.
    let mut coord = sb.spawn_coordinator_opts(uid, &["--test-bringup", "hang"]);

    // Wait for the REAL host to come all the way up in the pane: both broker legs
    // bound under the run dir it created.
    let up = wait_until(Duration::from_secs(30), || broker_legs_bound(&run));
    assert!(
        up,
        "the pane's host should have bound both broker legs under {}: record {:?}",
        run.display(),
        sb.record_text(uid)
    );

    // SIGKILL the host itself — not the `codeconnect` coordinator, and not the
    // children. A SIGKILLed host runs no teardown and removes nothing.
    let host_pids: Vec<i32> = processes_referencing(&tag)
        .into_iter()
        .filter(|(_, cmd)| cmd.contains("internal-codex-host"))
        .map(|(pid, _)| pid)
        .collect();
    assert!(
        !host_pids.is_empty(),
        "the internal-codex-host process should be running in the pane"
    );
    for pid in &host_pids {
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
        }
    }
    // The directory survives the kill — that is the precondition for the sweep
    // being the thing under test rather than the host's own teardown.
    assert!(
        run.exists(),
        "a SIGKILLed host cannot have removed its own run dir"
    );

    // Now lose the coordinator too, so the custodian takes the launch over.
    unsafe {
        libc::kill(coord.id() as i32, libc::SIGKILL);
    }
    let _ = coord.wait();

    let resolved = wait_until(Duration::from_secs(30), || sb.failed_and_complete(uid));
    assert!(
        resolved,
        "the custodian must own the outcome: {:?}",
        sb.record_text(uid)
    );
    // THE GATE: the run dir the SIGKILLed host left behind is gone, and only the
    // custodian's sweep could have removed it.
    assert!(
        wait_until(Duration::from_secs(10), || !run.exists()),
        "the custodian must sweep the run dir a SIGKILLed host left behind: {}",
        run.display()
    );
    assert!(
        wait_until(Duration::from_secs(10), || !sb.has_session("cc-1")),
        "the disposable session must be destroyed"
    );
    // The orphan story, now asserted rather than tolerated.
    //
    // This gate used to only REPORT survivors, because they were real: a SIGKILLed
    // host runs no teardown, and its children then depended on the kernel's hangup
    // for the pane's session leader, which races tmux's teardown of that pty.
    // Measured at roughly one run in ten, permanently, more under load.
    //
    // The custodian no longer needs to win that race. The host records each child
    // as `(pid, birth, pgid)` before the session can be declared ready, and cleanup
    // signals those verified identities directly — including killing a recorded
    // GROUP whose leader is already gone, since a descendant it forked is a member
    // in its own right and outlives it. So this is now a deterministic property and
    // is asserted as one. If it ever flakes again, that is a regression in the
    // recorded-identity teardown, not terminal weather.
    let leftover = processes_referencing(&tag);
    assert!(
        leftover.is_empty(),
        "the custodian must stop the SIGKILLed host's recorded children by identity; \
         these still reference the run dir: {leftover:?}"
    );
}

#[test]
fn the_last_session_on_a_server_still_reaches_terminal_cleanup() {
    // The topology every other test in this file deliberately avoids, and the one
    // every tmux server passes through exactly once: **our session is the only
    // session**. Killing it drains the server, the server exits, the socket stops
    // answering — and from then on no census can say anything but `Unavailable`,
    // which tmux.rs correctly refuses to read as absence.
    //
    // Before this was fixed the custodian retried that forever and the record
    // never reached `Complete`, so nothing ever swept the run dir either. The
    // escape is identity, not reachability: the record names server A by pid and
    // birth, and a session cannot outlive the server process that hosted it.
    //
    // Deliberately NO keepalive here. That is the whole point of the test.
    let uid = "01JQXV9K7B8N4M2P6R3T5W9YQJ";
    let sb = Sandbox::new("lastsession", uid);
    let run = sb.expected_run_dir(uid);
    let mut coord = sb.spawn_coordinator_opts(uid, &["--test-bringup", "hang"]);

    // The real host comes up, so the record carries server A and a host lease.
    let up = wait_until(Duration::from_secs(30), || broker_legs_bound(&run));
    assert!(
        up,
        "the pane's host should have bound both broker legs: {:?}",
        sb.record_text(uid)
    );
    let record = sb.record_text(uid).expect("record");
    assert!(
        record.contains("\"server_a\""),
        "server A must be recorded — it is the identity the escape is bound to: {record}"
    );

    // Kill the host: the pane dies, it was the server's only session, so the tmux
    // SERVER exits too. From here the socket answers nothing.
    let host_pids: Vec<i32> = processes_referencing(run.to_str().unwrap())
        .into_iter()
        .filter(|(_, cmd)| cmd.contains("internal-codex-host"))
        .map(|(pid, _)| pid)
        .collect();
    assert!(
        !host_pids.is_empty(),
        "the host should be running in the pane"
    );
    for pid in &host_pids {
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
        }
    }
    assert!(
        wait_until(Duration::from_secs(15), || !sb.has_session("cc-1")),
        "the session must be gone once its pane's host is killed"
    );

    // Hand the launch to the custodian.
    unsafe {
        libc::kill(coord.id() as i32, libc::SIGKILL);
    }
    let _ = coord.wait();

    // THE GATE: cleanup reaches a terminal state against a server that no longer
    // exists — proven by A's recorded identity being dead, never by the socket
    // being unreachable.
    let resolved = wait_until(Duration::from_secs(30), || sb.failed_and_complete(uid));
    assert!(
        resolved,
        "cleanup must terminalise even though no server is left to ask: {:?}{}",
        sb.record_text(uid),
        diagnose(&sb, uid)
    );
    // …and because it terminalised, the run dir was swept.
    assert!(
        wait_until(Duration::from_secs(10), || !run.exists()),
        "a wedged cleanup never sweeps; a terminal one must: {}",
        run.display()
    );
}

#[test]
fn cleanup_terminalises_when_server_a_was_never_persisted() {
    // The sibling of `the_last_session_on_a_server_still_reaches_terminal_cleanup`,
    // and the window that test does NOT cover: it stages the kill *after* server A
    // is recorded, so the escape it proves is the A-bound one.
    //
    // Here A is never persisted at all. `--test-newsession hang` holds the
    // coordinator INSIDE `new_session`, after tmux created and resolved the session
    // but before `record_server_a` runs — so the record carries `server_a: null`
    // forever. Combined with this being the server's only session, cleanup has no
    // server identity to bind to and no socket that will ever answer, which used to
    // mean armed until the next reboot.
    //
    // The escape is the HOST's recorded identity: the pane's command is the host,
    // so a host proven dead means tmux has already reaped the pane and the session
    // with it. Deliberately NO keepalive.
    let uid = "01JQXV9K7B8N4M2P6R3T5W9YQK";
    let sb = Sandbox::new("noserverA", uid);
    let run = sb.expected_run_dir(uid);
    let mut coord = sb.spawn_coordinator_opts(uid, &["--test-newsession", "hang"]);

    // The pane's host comes all the way up while the coordinator is still stuck
    // inside new-session — which is exactly why A never gets written.
    let up = wait_until(Duration::from_secs(30), || broker_legs_bound(&run));
    assert!(
        up,
        "the pane's host should have bound both broker legs: {:?}",
        sb.record_text(uid)
    );
    let record = sb.record_text(uid).expect("record");
    let parsed: serde_json::Value = serde_json::from_str(&record).unwrap();
    assert!(
        parsed.get("server_a").is_some_and(|v| v.is_null()),
        "this test is only meaningful while server A is UNRECORDED: {record}"
    );
    assert_eq!(
        parsed
            .get("new_session_indeterminate")
            .and_then(|v| v.as_bool()),
        Some(true),
        "the in-flight flag should still be set: {record}"
    );

    // Kill the host: its pane dies, and it was the only session, so the tmux server
    // exits too. Nothing will answer that socket again.
    let host_pids: Vec<i32> = processes_referencing(run.to_str().unwrap())
        .into_iter()
        .filter(|(_, cmd)| cmd.contains("internal-codex-host"))
        .map(|(pid, _)| pid)
        .collect();
    assert!(
        !host_pids.is_empty(),
        "the host should be running in the pane"
    );
    for pid in &host_pids {
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
        }
    }
    unsafe {
        libc::kill(coord.id() as i32, libc::SIGKILL);
    }
    let _ = coord.wait();

    // THE GATE: terminal cleanup with no server A, no answering socket, and the
    // indeterminate flag set — bound to the host identity the record does carry.
    let resolved = wait_until(Duration::from_secs(30), || sb.failed_and_complete(uid));
    assert!(
        resolved,
        "cleanup must terminalise with no recorded server A: {:?}{}",
        sb.record_text(uid),
        diagnose(&sb, uid)
    );
    assert!(
        wait_until(Duration::from_secs(10), || !run.exists()),
        "a wedged cleanup never sweeps; a terminal one must: {}",
        run.display()
    );
}
