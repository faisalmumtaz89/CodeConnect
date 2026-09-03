//! DETERMINISTIC lifecycle gates for the `internal-codex-host` wrapper, driven by
//! a **fake codex** so they run in the normal `cargo test` suite with no real
//! `codex` installed and no network.
//!
//! The live gate (`live_codex_host.rs`) proves the host works against the real
//! thing; these prove the parts a real session will not reliably show you on
//! demand — the **fatal paths**:
//!
//!   1. `app_server_death_is_session_fatal` — SIGKILL the app-server under a live
//!      session and assert the host's contract: exit **70**, **no visible process
//!      references the run-dir tag** any more (the argv scan's exact claim — see
//!      [`processes_matching`]), and the run directory **removed**. This is the
//!      invariant a TUI's security depends on (never left talking to a dead
//!      upstream), and it is the one that silently regressed into a leak when a
//!      `?` sat between a spawn and the teardown.
//!   2. `signal_during_bringup_leaks_nothing` — SIGTERM the host while bring-up is
//!      still blocked, and assert 130 with nothing left behind: bring-up waits are
//!      cancellation-aware, not a window where a signal orphans a child.
//!   3. `an_existing_run_dir_is_refused` — the host owns its run dir. Readiness
//!      ("a 0600 socket appeared") only means "our app-server bound it" because
//!      the directory is provably fresh, so adopting an existing one is refused.
//!
//! # The fatal path that is NOT here: broker death
//!
//! The fourth session-fatal path — the broker task ending under a live session —
//! has no test in this file, deliberately. It is not drivable from outside the
//! process: `Broker::serve` returns only on an accept error, and nothing external
//! can end one tokio task inside a running host without killing the whole host.
//! So the host's race arms were made **thin** and the decision they make was
//! extracted into a pure function, `codex_host::resolve_outcome`, whose unit tests
//! (in `src/codex_host.rs`) are the real coverage for broker death — including the
//! masking scenario `biased` cannot prevent (broker finishes after its own poll
//! returned Pending, so the TUI arm fires with the boundary already dead). This
//! file covers the arms that CAN be driven from outside: an app-server killed by
//! signal, and a signalled host.
//!
//! # The fake
//!
//! A tiny python3 script written by the test. In `app-server` mode it binds a real
//! `SOCK_STREAM` unix socket at the `--listen unix://PATH` it is given, `chmod`s it
//! to 0600 (exactly what the host's readiness gate waits for, past the real
//! app-server's bind→chmod race), and then sleeps. In any other mode — which is
//! how the host invokes the TUI — it just sleeps, holding the session open.
//! python3 ships with the macOS developer tooling; if it is genuinely absent these
//! tests **fail loudly** rather than skip — `cargo test` hides a skip message
//! without `--nocapture`, and a fatal-path suite that reports green having executed
//! nothing is the same vacuous pass the live gate's `CC_CODEX_LIVE` guard forbids.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The A7.1 digest of the codex binary under test: the identity resolution pins,
/// which the host re-verifies immediately before each of its two execs. Computed
/// here rather than written down because these harnesses build (or copy) their
/// codex at run time.
///
/// **Derived locally, deliberately, and now that is a choice rather than the only
/// option.** Until 2e-7d nothing could pin a digest: `codeconnect codex` refused
/// before it would have spawned a coordinator, so every harness composed the
/// charter a launcher would have written. The launcher exists now and its own path
/// is gated end to end by `the_codex_command_launches_a_real_session_end_to_end`
/// (`live_codex_coordinator.rs`). This file still derives its own, because:
///
/// **several call sites here pass digests that are deliberately WRONG** — a digest
/// pinned before the file's bytes are swapped, a self-swapping fake codex, a
/// well-formed value that names nothing. Those are the host's fatal paths, and a
/// real launcher can only ever produce truthful digests, so it could not drive one
/// of them. Routing this file through the launcher would delete the tests.
fn codex_sha256(path: &Path) -> String {
    protocol::hash::sha256_file(path).expect("hash the codex binary under test")
}

/// A unix socket path must be shorter than `sun_len` (~104 on macOS), so every
/// path here lives under a SHORT `/tmp` dir, never a deep build path.
const SUN_LEN_LIMIT: usize = 104;

/// The host's `EX_SOFTWARE`: bring-up unproven, or a session-fatal death.
const EX_HOST_FATAL: i32 = 70;
/// The host's signalled exit.
const EX_HOST_SIGNALLED: i32 = 130;

// ---------------------------------------------------------------- scaffolding

/// A unique short `/tmp` path. Two flavours are needed: dirs this harness creates
/// (the fake's home, `CODEX_HOME`) and a path the *host* must create itself.
fn short_tmp_path(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/ccfatal.{}.{tag}.{n}.{nanos}",
        std::process::id()
    ))
}

/// A short-path `/tmp` dir that removes itself on Drop.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(tag: &str) -> ScratchDir {
        use std::os::unix::fs::DirBuilderExt;
        let path = short_tmp_path(tag);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("create a fresh short /tmp scratch dir");
        ScratchDir { path }
    }

    fn as_str(&self) -> &str {
        self.path.to_str().expect("short /tmp path is utf-8")
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A path the HOST must create. Nothing is made here; Drop only sweeps up if the
/// host failed to.
struct OwnedByHost {
    path: PathBuf,
}

impl OwnedByHost {
    fn new(tag: &str) -> OwnedByHost {
        OwnedByHost {
            path: short_tmp_path(tag),
        }
    }
    fn as_str(&self) -> &str {
        self.path.to_str().expect("short /tmp path is utf-8")
    }
}

impl Drop for OwnedByHost {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Locate python3 for the fake codex, or **fail the test**.
///
/// Deliberately not a skip. These are the only gates covering the session-fatal
/// paths, and `cargo test` swallows a `SKIP` printed to stderr unless `--nocapture`
/// is passed — so skipping here would report the load-bearing fatal-path suite as
/// green having executed nothing. That is exactly the vacuous pass the live
/// harness's `CC_CODEX_LIVE` gate was fixed to prevent; the same rule applies to
/// the deterministic suite. python3 ships with the macOS command line tools, so
/// its absence is a broken dev environment, not a legitimate configuration.
fn python3() -> PathBuf {
    for cand in ["/usr/bin/python3", "/opt/homebrew/bin/python3"] {
        if Path::new(cand).exists() {
            return PathBuf::from(cand);
        }
    }
    if let Ok(out) = Command::new("/usr/bin/which").arg("python3").output() {
        if out.status.success() {
            let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
            if p.exists() {
                return p;
            }
        }
    }
    panic!(
        "no python3 found (/usr/bin/python3, /opt/homebrew/bin/python3, or PATH). \
         The fatal-path gates need it to build the fake codex, and reporting them as \
         a silent pass would be worse than failing — install the macOS command line \
         tools."
    )
}

/// Write the fake `codex` into `dir` and return its path.
///
/// `app-server --listen unix://P` binds P, chmods it 0600 and sleeps; anything
/// else (the TUI invocation) just sleeps. Both ignore stdio, so no tty is needed —
/// which is why this gate needs no PTY and is fully deterministic.
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
    # The host's readiness gate waits for EXACTLY 0600, mirroring the real
    # app-server's bind-then-chmod. Do the same so the gate is exercised.
    os.chmod(path, 0o600)
    s.listen(16)

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

// ------------------------------------------------- the D7 admission the host runs
//
// Since 2e-2b the host presents itself to the D7 launch gate before it creates
// anything: it takes an exclusive `host_lease` on a `pending` launch record, or
// refuses and exits 75 having made nothing. That is a real gate, not a formality,
// so a harness that drives the host directly has to give it a launch to belong to.
//
// This writes the minimum admissible record by hand rather than through
// `codex_launch` (an integration test links the binary, not a library). The
// coordinator and custodian slots are set to **this test process**, which is
// genuinely alive, because admission requires both to be proven live — pointing
// them at a fabricated pid would be refused, correctly.

/// The launch identity a host harness presents to the gate.
struct Launch {
    home: PathBuf,
    uid: String,
    nonce: String,
}

impl Launch {
    /// Write a fresh admissible `pending` record under a private
    /// `CODECONNECT_HOME`, and return the identity to pass to the host.
    fn admissible(tag: &str) -> Launch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let home = short_tmp_path(&format!("{tag}home"));
        let uid = format!("01JQXV9K7B{:016X}", nanos as u64);
        let nonce = format!("{:016x}{:016x}", nanos as u64, std::process::id());
        let me = protocol::proc_identity::current_identity().expect("read our own identity");
        let boot = protocol::proc_identity::boot_identity().expect("read the boot identity");
        let now = protocol::proc_identity::monotonic_now_nanos().expect("read the monotonic clock");
        let identity = serde_json::json!({
            "pid": me.pid,
            "birth": { "start_sec": me.birth.start_sec, "start_usec": me.birth.start_usec },
        });
        let record = serde_json::json!({
            "schema": 1,
            "launch_nonce": nonce,
            "uid": uid,
            "session_name": "cc-host-harness",
            // Both guardians are THIS process: alive, so admission's
            // proven-live requirements are satisfied honestly.
            "coordinator": identity,
            "custodian": identity,
            "boot": { "boot_sec": boot.boot_sec, "boot_usec": boot.boot_usec },
            // Far enough out that a slow test machine cannot expire the launch
            // mid-run, which would surface as a confusing admission refusal.
            "deadline_monotonic_nanos": now + 600_000_000_000u64,
            "state": "Pending",
            "cleanup": "Pending",
            "new_session_indeterminate": false,
            "host_lease": serde_json::Value::Null,
            "pending_spawn": serde_json::Value::Null,
            "server_a": serde_json::Value::Null,
            "run_dir": serde_json::Value::Null,
            "children": [],
            "created_ms": 0,
        });
        let dir = home.join("sessions").join(&uid);
        std::fs::create_dir_all(&dir).expect("create the session dir");
        std::fs::write(
            dir.join("launch.json"),
            serde_json::to_vec_pretty(&record).expect("serialize the record"),
        )
        .expect("write the launch record");
        Launch { home, uid, nonce }
    }
}

impl Drop for Launch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// The host under test, spawned directly (the fake TUI needs no tty).
///
/// **Spawned into its own process group** (`process_group(0)`, so its pgid is its
/// pid) and Drop SIGKILLs the whole group `-pgid`, then sweeps by the run-dir tag.
/// Killing only `child` is not enough: the host's two children — both `sleep(3600)`
/// fakes — inherit its group, so a failing assertion between "the session is up"
/// and the host's own teardown would leave two infinite sleepers behind. Same
/// pattern as `live_codex_host.rs`'s `PtyHost`, for the same reason.
struct Host {
    child: Child,
    /// The run dir path — the argv tag every process in this session carries.
    tag: String,
    /// What is known about the leader (`child`) — three-valued, and what makes the
    /// group kill in [`Host::drop`] conditional.
    ///
    /// `kill(-pgid)` names a group by NUMBER, and that number is only meaningfully
    /// ours while the leader is [`LeaderState::Unreaped`]: once reaped, the pgid is
    /// a stale token and SIGKILLing it could land on whatever process group has
    /// since claimed the id. Every gate here calls [`Host::wait_code`] first, so
    /// `Reaped` is the COMMON case, not a corner. Cleanup after that point goes only
    /// through the tagged-pid scan, whose warrant is narrower but not race-free
    /// either — see [`Host::drop`].
    leader: LeaderState,
}

/// What is known about a spawned leader process — deliberately three-valued.
///
/// Two states would force `try_wait`'s error case to be filed as one of "reaped" or
/// "unreaped", and both filings are wrong: an `Err` proves only that *that call*
/// collected no status. Calling it "unreaped" would license a `kill(-pgid)` on the
/// strength of a failed syscall — a numeric group kill backed by no knowledge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaderState {
    /// No status collected and no failed attempt, so the leader is still in the
    /// process table — running or an unreaped zombie — which is what keeps its pid,
    /// and the pgid equal to it, allocated. **The only state in which a group kill
    /// is warranted.**
    Unreaped,
    /// Its status was collected; the pid and pgid may now be recycled.
    Reaped,
    /// A `try_wait` errored, so the state is genuinely unknown. Treated like
    /// `Reaped` for kill purposes — never a numeric pgid on uncertain knowledge.
    Unknown,
}

impl Host {
    /// Spawn `internal-codex-host` with a complete charter. `run_dir` must NOT
    /// exist — the host creates and owns it.
    fn spawn(codex: &Path, run_dir: &str, codex_home: &str, launch: &Launch) -> Host {
        use std::os::unix::process::CommandExt;
        let child = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
            .args([
                "internal-codex-host",
                // The D7 launch identity the host presents to the gate before it
                // creates anything (2e-2b).
                "--uid",
                &launch.uid,
                "--nonce",
                &launch.nonce,
                "--tmux-socket",
                "/tmp/cc-host-harness-no-server.sock",
                "--codex",
                codex.to_str().expect("utf-8"),
                // A7.1: the identity the host re-verifies before each exec.
                "--codex-sha256",
                &codex_sha256(codex),
                "--run-dir",
                run_dir,
                "--codex-home",
                codex_home,
                "--approval-policy",
                "untrusted",
                "--approvals-reviewer",
                "user",
                "--sandbox",
                "read-only",
                "--hooks-enabled",
                "true",
                // Round-2 P4: the canonical launch cwd (the workspace anchor).
                "--launch-cwd",
                "/tmp",
            ])
            .env("CODECONNECT_HOME", &launch.home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Own process group, so Drop can kill the group — the host AND the two
            // fake children that inherit it — rather than one pid.
            .process_group(0)
            .spawn()
            .expect("spawn internal-codex-host");
        Host {
            child,
            tag: run_dir.to_string(),
            leader: LeaderState::Unreaped,
        }
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    /// The leader's current state, as a phrase for a failure message — **with the
    /// same bookkeeping [`Host::wait_code`] does**.
    ///
    /// This exists because the obvious way to write the diagnostic is a bare
    /// `host.child.try_wait()` inside the panic message, and that is a trap: a
    /// `try_wait` is not a read-only question. `Ok(Some(_))` REAPS the leader and
    /// `Err(_)` destroys what was known about it, so a raw call leaves `leader`
    /// saying `Unreaped` when it no longer is — and the very next thing that happens
    /// on this path is the panic unwinding into [`Host::drop`], which reads that
    /// field to decide whether to `kill(-pgid)`. A diagnostic printed on the way to
    /// failing a test must not be able to redirect a SIGKILL at a recycled group id.
    fn leader_probe(&mut self) -> String {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.leader = LeaderState::Reaped;
                format!("exited ({status})")
            }
            Ok(None) => "still running".to_string(),
            Err(err) => {
                self.leader = LeaderState::Unknown;
                format!("state could not be determined ({err})")
            }
        }
    }

    /// Wait (bounded) for the host to exit and return its code.
    ///
    /// Collecting a status here IS the reap, so it records that fact: from this
    /// point the leader's pid — and the pgid equal to it — may be reused, and
    /// [`Host::drop`] must stop group-killing by that number.
    fn wait_code(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.leader = LeaderState::Reaped;
                    return Some(status.code().unwrap_or(-1));
                }
                Ok(None) => {}
                // An Err proves only that THIS call collected no status — not that
                // the child is still unreaped. `Unknown`, so Drop falls back to
                // tagged-pid cleanup rather than acting on a failed syscall.
                Err(_) => {
                    self.leader = LeaderState::Unknown;
                    return None;
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // The group kill is ONLY warranted while the leader is provably still ours,
        // which is exactly [`LeaderState::Unreaped`]. `kill(-pgid)` addresses a group
        // by number, and that number stops being ours the instant the leader's status
        // is collected — the pid becomes free, and a SIGKILL to `-pgid` after that can
        // land on an unrelated process group that has since been given the id.
        // `Unknown` is no better a warrant: a failed `try_wait` is not evidence the
        // leader survives, and a numeric kill on no knowledge is the same hazard. Both
        // fall through to the tagged-pid sweep. So: group-kill while the leader is
        // still ours (a failed assertion before `wait_code`, where the fake app-server
        // and fake TUI are both `sleep(3600)` in this group and must not be orphaned),
        // and never afterwards.
        if self.leader == LeaderState::Unreaped {
            let pgid = self.child.id() as i32;
            sigkill(-pgid);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // Then the tag sweep, which runs reaped or not because it is the cleanup for
        // anything that left the group. Its property, stated honestly: it kills only
        // pids observed carrying this run-dir tag in their argv moments earlier —
        // which is far better than a bare number, but not a guarantee. A tagged
        // process can exit between the `ps` and the `/bin/kill`, and its pid can be
        // recycled in that window; the race is inherent to any ps-based cleanup and
        // is accepted here because the alternative is leaving the residue. It is
        // narrower than the group kill's window, not absent.
        // Uses the raw scan, not `processes_matching`,
        // because that one panics on a bad scan and panicking in a Drop that is
        // itself running during an assertion unwind would abort the process.
        for pid in tagged_pids_best_effort(&self.tag) {
            sigkill(pid);
        }
    }
}

/// The run-dir-tagged pids, or an empty list if `ps` could not be run.
///
/// Deliberately the tolerant twin of [`processes_matching`]: this one is used only
/// by [`Host::drop`] for cleanup, where an empty answer costs nothing, and where a
/// panic during an assertion's unwind would abort the test binary instead of
/// reporting the real failure. Nothing asserts on this function's result.
fn tagged_pids_best_effort(tag: &str) -> Vec<i32> {
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

/// `(pid, command)` for every process whose full command line contains `needle`,
/// excluding this test process. `-ww` disables ps's column truncation.
///
/// **Fails closed.** Every caller uses an empty result as evidence that nothing
/// leaked, so a `ps` that could not be run — or that ran and failed — must panic
/// rather than return `Vec::new()`. Swallowing the error would turn a broken scan
/// into a green leak assertion, which is precisely the vacuous pass this suite
/// exists to prevent.
fn processes_matching(needle: &str) -> Vec<(i32, String)> {
    let me = std::process::id() as i32;
    let out = Command::new("/bin/ps")
        .args(["-Axww", "-o", "pid=,command="])
        .output()
        .expect("run /bin/ps to scan the process table — a scan that cannot run must FAIL the test, never silently report 'nothing found'");
    assert!(
        out.status.success(),
        "/bin/ps exited {} — the process scan is the evidence for every leak \
         assertion here, so a failed scan fails the test: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, cmd) = line.trim_start().split_once(char::is_whitespace)?;
            let pid: i32 = pid.trim().parse().ok()?;
            if pid != me && cmd.contains(needle) {
                Some((pid, cmd.to_string()))
            } else {
                None
            }
        })
        .collect()
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// SIGKILL `pid`, or — for a negative `pid` — that whole process GROUP.
///
/// Silent, like the live harness's `kill_pid`: on the [`Host::drop`] cleanup path
/// the group has usually already exited and been reaped, so "no such process" is
/// the expected, uninteresting case and must not print over a passing test's
/// output. A kill that genuinely mattered and did not land shows up as the next
/// assertion failing, not as this line.
fn sigkill(pid: i32) {
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// --------------------------------------------------------------------- gates

/// **The load-bearing fatal path.** With a session fully up, SIGKILL the
/// app-server and hold the host to its contract: the session is over (exit 70),
/// no visible process references the TUI's `--remote unix://<run>/tui.sock` any
/// more rather than being left orphaned against a dead upstream, and the run
/// directory the host created is gone.
#[test]
fn app_server_death_is_session_fatal() {
    let python = python3();
    let fake_home = ScratchDir::new("fake");
    let codex_home = ScratchDir::new("home");
    let run = OwnedByHost::new("run");
    assert!(
        run.path.join("tui.sock").as_os_str().len() < SUN_LEN_LIMIT,
        "run dir too long for SUN_LEN"
    );
    let fake = write_fake_codex(&fake_home.path, &python);

    let launch = Launch::admissible("fatal");
    let mut host = Host::spawn(&fake, run.as_str(), codex_home.as_str(), &launch);

    // Bring-up is complete once the TUI has been spawned against tui.sock.
    let tui_marker = format!("--remote unix://{}/tui.sock", run.as_str());
    let up = wait_until(Duration::from_secs(20), || {
        !processes_matching(&tui_marker).is_empty()
    });
    if !up {
        // Through `leader_probe`, never a raw `try_wait`: this panic unwinds
        // straight into `Host::drop`, which decides on a group kill from `leader`.
        let host_state = host.leader_probe();
        let listing = std::fs::read_dir(&run.path).map(|d| d.count());
        panic!("the fake TUI never came up; host {host_state}, run dir listing={listing:?}");
    }
    let tui_pids: Vec<i32> = processes_matching(&tui_marker)
        .into_iter()
        .map(|(pid, _)| pid)
        .collect();

    let as_marker = format!("app-server --listen unix://{}/as.sock", run.as_str());
    let as_pids: Vec<i32> = processes_matching(&as_marker)
        .into_iter()
        .map(|(pid, _)| pid)
        .collect();
    assert!(
        !as_pids.is_empty(),
        "the fake app-server should be running while the session is up"
    );

    println!("session up: app-server={as_pids:?} tui={tui_pids:?}; SIGKILLing the app-server");
    for pid in &as_pids {
        sigkill(*pid);
    }

    // 1. The session is fatal, not clean.
    let code = host.wait_code(Duration::from_secs(20));
    assert_eq!(
        code,
        Some(EX_HOST_FATAL),
        "app-server death must exit {EX_HOST_FATAL} (session-fatal), got {code:?}"
    );

    // 2. No visible process references the TUI marker any more — the argv scan's
    // exact claim. A TUI left orphaned against a dead upstream would still carry
    // `--remote unix://<run>/tui.sock` and would be seen here.
    let tui_gone = wait_until(Duration::from_secs(10), || {
        processes_matching(&tui_marker).is_empty()
    });
    assert!(
        tui_gone,
        "a process still references the TUI marker — the TUI outlived its dead \
         upstream (leak): {:?}",
        processes_matching(&tui_marker)
    );

    // 3. Nothing references the run dir at all, and the dir itself is gone.
    // Poll, like the sibling assertions: between the host exiting and the kernel
    // reaping the SIGKILLed app-server, `ps` can still list it as a zombie with
    // its original command line. A bare assert here is an intermittent failure.
    let no_refs = wait_until(Duration::from_secs(5), || {
        processes_matching(run.as_str()).is_empty()
    });
    assert!(
        no_refs,
        "processes still reference the run dir: {:?}",
        processes_matching(run.as_str())
    );
    assert!(
        !run.path.exists(),
        "the host did not remove the run dir it created: {}",
        run.path.display()
    );
    println!(
        "PASS app_server_death_is_session_fatal (exit 70, no visible process references \
         the TUI marker, run dir removed)"
    );
}

/// An app-server that dies *before* binding must fail bring-up closed, quote the
/// child's own stderr so the operator can see why, spawn no TUI, and clean up —
/// and must NOT report an un-proven reap, because the bring-up poll's own
/// `try_wait` is what collected that child's status.
#[test]
fn app_server_dying_before_bind_fails_closed_with_its_stderr() {
    let python = python3();
    use std::os::unix::fs::PermissionsExt;
    let fake_home = ScratchDir::new("fakedie");
    let codex_home = ScratchDir::new("homedie");
    let run = OwnedByHost::new("rundie");

    // An app-server that complains and exits without ever binding.
    let script = format!(
        "#!{}\nimport sys, time\nif sys.argv[1:2] == [\"app-server\"]:\n    \
         sys.stderr.write(\"fake-codex: refusing to bind, this is the reason\\n\")\n    \
         sys.exit(3)\nwhile True: time.sleep(3600)\n",
        python.display()
    );
    let fake = fake_home.path.join("fake-codex-die");
    std::fs::write(&fake, script).expect("write dying fake");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).expect("chmod");

    let launch = Launch::admissible("direct");
    let out = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
        .args([
            "internal-codex-host",
            "--uid",
            &launch.uid,
            "--nonce",
            &launch.nonce,
            "--tmux-socket",
            "/tmp/cc-host-harness-no-server.sock",
            "--codex",
            fake.to_str().expect("utf-8"),
            "--codex-sha256",
            &codex_sha256(&fake),
            "--run-dir",
            run.as_str(),
            "--codex-home",
            codex_home.as_str(),
            "--approval-policy",
            "untrusted",
            "--approvals-reviewer",
            "user",
            "--sandbox",
            "read-only",
            "--hooks-enabled",
            "true",
            // Round-2 P4: the canonical launch cwd (the workspace anchor).
            "--launch-cwd",
            "/tmp",
        ])
        .env("CODECONNECT_HOME", &launch.home)
        .stdin(Stdio::null())
        .output()
        .expect("run the host");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(EX_HOST_FATAL),
        "an app-server that never binds must fail bring-up closed: {stderr}"
    );
    assert!(
        stderr.contains("exited before binding"),
        "the refusal should name what happened: {stderr}"
    );
    // The child's OWN stderr is surfaced — an operator must not have to guess.
    assert!(
        stderr.contains("refusing to bind, this is the reason"),
        "the app-server's stderr should be quoted into the error: {stderr}"
    );
    // The poll's `try_wait` already reaped that child, so teardown must not claim
    // it could not prove the stop.
    assert!(
        !stderr.contains("could not prove"),
        "an already-reaped child must not be reported as un-proven: {stderr}"
    );
    assert!(
        processes_matching(run.as_str()).is_empty(),
        "a failed bring-up left processes behind: {:?}",
        processes_matching(run.as_str())
    );
    assert!(
        !run.path.exists(),
        "a failed bring-up left its run dir behind: {}",
        run.path.display()
    );
    println!("PASS app_server_dying_before_bind_fails_closed_with_its_stderr");
}

/// **A7.1, staged end to end in a real process.** The launcher pins the codex
/// binary by digest; the file at that path is then replaced *before the host runs*
/// — the `standalone/current` flip, the npm overwrite, the install landing
/// mid-launch. The host must refuse, and must refuse having spawned nothing at all:
/// the point of the pin is that those bytes never become a process.
///
/// The fake here is a perfectly good, perfectly working codex both before and after
/// the swap. Nothing about it is malformed — a magic check, an `--version` and an
/// exec would all be happy with it. Only the identity check sees the difference,
/// which is exactly the hole A7 named.
///
/// **This stages the swap that has already SETTLED by the time the host looks**, and
/// that is the easy half. The digest comparison alone is enough to catch it, which is
/// precisely why this test stayed green through the window where a swap landing
/// *during* the verification read was not caught at all: the read holds the old vnode
/// to its last byte, so the digest matches and the spawn runs the replacement. That
/// half needs a real mid-read race and lives with the guard that closes it — see
/// `protocol::hash`'s `a_rename_landing_mid_hash_is_refused_rather_than_hashed_clean`
/// and `codex`'s `a_binary_renamed_over_mid_inspection_is_never_resolved`.
#[test]
fn a_codex_replaced_after_the_pin_is_refused_before_anything_is_spawned() {
    let python = python3();
    use std::os::unix::fs::PermissionsExt;
    let fake_home = ScratchDir::new("fakeswap");
    let codex_home = ScratchDir::new("homeswap");
    let run = OwnedByHost::new("runswap");

    // The codex that gets pinned: a working app-server fake that binds nothing but
    // would otherwise be spawned and waited on.
    let fake = fake_home.path.join("fake-codex-swap");
    let sleeper = format!(
        "#!{}\nimport time\nwhile True: time.sleep(3600)\n",
        python.display()
    );
    std::fs::write(&fake, &sleeper).expect("write the pinned fake");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).expect("chmod");

    // Pin it — this is what `codex::resolve_codex_bin` would have handed the
    // coordinator, and what the coordinator puts on the host's argv.
    let pinned = codex_sha256(&fake);

    // The swap. Same path, same mode, still an executable that runs: only the bytes
    // differ. A marker in the replacement proves, if it ever ran, that it ran.
    let replacement = format!(
        "#!{}\nimport time\n# swapped-{}\nwhile True: time.sleep(3600)\n",
        python.display(),
        run.as_str()
    );
    std::fs::write(&fake, &replacement).expect("swap the fake");
    assert_ne!(
        pinned,
        codex_sha256(&fake),
        "the staged swap must actually change the digest, or this test proves nothing"
    );

    let launch = Launch::admissible("direct");
    let out = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
        .args([
            "internal-codex-host",
            "--uid",
            &launch.uid,
            "--nonce",
            &launch.nonce,
            "--tmux-socket",
            "/tmp/cc-host-harness-no-server.sock",
            "--codex",
            fake.to_str().expect("utf-8"),
            // The identity of the bytes as they were when they were inspected —
            // NOT as they are now.
            "--codex-sha256",
            &pinned,
            "--run-dir",
            run.as_str(),
            "--codex-home",
            codex_home.as_str(),
            "--approval-policy",
            "untrusted",
            "--approvals-reviewer",
            "user",
            "--sandbox",
            "read-only",
            "--hooks-enabled",
            "true",
            "--launch-cwd",
            "/tmp",
        ])
        .env("CODECONNECT_HOME", &launch.home)
        .stdin(Stdio::null())
        .output()
        .expect("run the host");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(EX_HOST_FATAL),
        "a swapped codex must be session-fatal: {stderr}"
    );
    assert!(
        stderr.contains("not the one this launch pinned"),
        "the refusal must be the identity check itself: {stderr}"
    );
    assert!(
        stderr.contains(&pinned),
        "it must name the digest that was pinned: {stderr}"
    );
    assert!(
        stderr.contains("immediately before the app-server spawn"),
        "and WHERE the check ran, so this is provably the pre-exec guard and not \
         some later failure: {stderr}"
    );
    // Nothing ran. Not the app-server, not the TUI — the marker is unique to this
    // run dir, so a live match would be this test's own swapped binary.
    assert!(
        processes_matching(run.as_str()).is_empty(),
        "a refused launch spawned something: {:?}",
        processes_matching(run.as_str())
    );
    assert!(
        !run.path.exists(),
        "a refused launch left its run dir behind: {}",
        run.path.display()
    );
    println!("PASS a_codex_replaced_after_the_pin_is_refused_before_anything_is_spawned");
}

/// **The second exec has its own window, and its own guard.** The app-server's
/// check says nothing about the TUI: the app-server's bring-up and the broker's
/// bind stand between them, seconds during which the path is not watched. So the
/// swap here is staged *inside* that interval — the fake app-server replaces its own
/// binary (by `os.replace`, a rename onto the pathname, which is the shape a
/// `standalone/current` flip and an installer both have) before it binds the socket
/// the host is waiting on, so the replacement is provably complete by the time
/// bring-up proceeds.
///
/// The first verify passed. The app-server is running. The broker is bound. And the
/// TUI must still not start — and the app-server that was legitimately spawned must
/// be torn down, because a refused launch leaves nothing behind.
#[test]
fn a_codex_replaced_between_the_two_spawns_stops_the_tui() {
    let python = python3();
    use std::os::unix::fs::PermissionsExt;
    let fake_home = ScratchDir::new("fakemid");
    let codex_home = ScratchDir::new("homemid");
    let run = OwnedByHost::new("runmid");

    let fake = fake_home.path.join("fake-codex-mid");
    let script = format!(
        r#"#!{}
import os, socket, sys, time

argv = sys.argv[1:]
if argv and argv[0] == "app-server":
    path = None
    for i, a in enumerate(argv):
        if a == "--listen" and i + 1 < len(argv):
            path = argv[i + 1]
    path = path[len("unix://"):]
    # Replace our OWN binary, by rename, BEFORE binding. The host is blocked on
    # that socket appearing, so the swap is complete before bring-up moves on.
    me = os.path.realpath(sys.argv[0])
    with open(me) as f:
        body = f.read()
    tmp = me + ".new"
    with open(tmp, "w") as f:
        f.write(body + "\n# swapped between the two spawns\n")
    os.chmod(tmp, 0o700)
    os.replace(tmp, me)
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.bind(path)
    os.chmod(path, 0o600)
    s.listen(16)

while True:
    time.sleep(3600)
"#,
        python.display()
    );
    std::fs::write(&fake, &script).expect("write the self-swapping fake");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let pinned = codex_sha256(&fake);

    let launch = Launch::admissible("direct");
    let out = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
        .args([
            "internal-codex-host",
            "--uid",
            &launch.uid,
            "--nonce",
            &launch.nonce,
            "--tmux-socket",
            "/tmp/cc-host-harness-no-server.sock",
            "--codex",
            fake.to_str().expect("utf-8"),
            "--codex-sha256",
            &pinned,
            "--run-dir",
            run.as_str(),
            "--codex-home",
            codex_home.as_str(),
            "--approval-policy",
            "untrusted",
            "--approvals-reviewer",
            "user",
            "--sandbox",
            "read-only",
            "--hooks-enabled",
            "true",
            "--launch-cwd",
            "/tmp",
        ])
        .env("CODECONNECT_HOME", &launch.home)
        .stdin(Stdio::null())
        .output()
        .expect("run the host");

    let stderr = String::from_utf8_lossy(&out.stderr);
    // The swap must actually have happened, or this test proves nothing.
    assert_ne!(
        pinned,
        codex_sha256(&fake),
        "the fake app-server did not replace its own binary: {stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(EX_HOST_FATAL),
        "a codex swapped before the TUI spawn must be session-fatal: {stderr}"
    );
    assert!(
        stderr.contains("not the one this launch pinned"),
        "the refusal must be the identity check: {stderr}"
    );
    assert!(
        stderr.contains("immediately before the TUI spawn"),
        "and it must be the SECOND check that caught it — the first one passed: {stderr}"
    );
    // The app-server that legitimately started is gone, and so is the run dir: a
    // refusal on the second exec still ends the session cleanly.
    assert!(
        processes_matching(run.as_str()).is_empty(),
        "the refused launch left processes behind: {:?}",
        processes_matching(run.as_str())
    );
    assert!(
        !run.path.exists(),
        "the refused launch left its run dir behind: {}",
        run.path.display()
    );
    println!("PASS a_codex_replaced_between_the_two_spawns_stops_the_tui");
}

/// A signal delivered *during* bring-up must tear down whatever started. The fake
/// is told to be an app-server that never binds (an unknown mode → the sleeping
/// branch), so the host is parked in its bounded socket wait when SIGTERM lands.
#[test]
fn signal_during_bringup_leaks_nothing() {
    let python = python3();
    let fake_home = ScratchDir::new("fakehang");
    let codex_home = ScratchDir::new("homehang");
    let run = OwnedByHost::new("runhang");

    // A fake that NEVER binds: `app-server` mode is spelled differently, so the
    // script falls through to the sleep branch and as.sock never appears.
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        "#!{}\nimport time\nwhile True: time.sleep(3600)\n",
        python.display()
    );
    let fake = fake_home.path.join("fake-codex-hang");
    std::fs::write(&fake, script).expect("write hanging fake");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).expect("chmod");

    let launch = Launch::admissible("fatal");
    let mut host = Host::spawn(&fake, run.as_str(), codex_home.as_str(), &launch);

    // Wait until the app-server child exists — i.e. we are inside the bounded
    // socket wait, which is exactly the window a signal used to be able to orphan.
    let as_marker = format!("app-server --listen unix://{}/as.sock", run.as_str());
    let spawned = wait_until(Duration::from_secs(10), || {
        !processes_matching(&as_marker).is_empty()
    });
    assert!(spawned, "the fake app-server child never appeared");

    let _ = Command::new("/bin/kill")
        .args(["-TERM", &host.pid().to_string()])
        .status();

    let code = host.wait_code(Duration::from_secs(15));
    assert_eq!(
        code,
        Some(EX_HOST_SIGNALLED),
        "a signal during bring-up must exit {EX_HOST_SIGNALLED}, got {code:?}"
    );
    let clean = wait_until(Duration::from_secs(10), || {
        processes_matching(run.as_str()).is_empty()
    });
    assert!(
        clean,
        "bring-up was signalled but left children behind: {:?}",
        processes_matching(run.as_str())
    );
    assert!(
        !run.path.exists(),
        "the run dir survived a signalled bring-up: {}",
        run.path.display()
    );
    println!("PASS signal_during_bringup_leaks_nothing (exit 130, no children, run dir removed)");
}

/// The host owns its run dir: it refuses to adopt one it did not create, because
/// its whole readiness argument ("a 0600 socket appeared at as.sock, therefore our
/// app-server bound it") collapses the moment the directory could have been
/// prepared by someone else.
#[test]
fn an_existing_run_dir_is_refused() {
    let codex_home = ScratchDir::new("homeexist");
    // Pre-create the run dir — a hostile or stale directory, planted socket or not.
    let run = ScratchDir::new("runexist");
    // No fake is needed: the refusal must happen before anything is ever spawned,
    // so a path that does not even exist is the sharper probe.
    let fake = PathBuf::from("/nonexistent/codex");

    let launch = Launch::admissible("direct");
    let out = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
        .args([
            "internal-codex-host",
            "--uid",
            &launch.uid,
            "--nonce",
            &launch.nonce,
            "--tmux-socket",
            "/tmp/cc-host-harness-no-server.sock",
            "--codex",
            fake.to_str().expect("utf-8"),
            // A7.1: a well-formed digest that will never be checked — the
            // run-dir refusal must happen BEFORE anything is inspected or
            // spawned, so a codex path that does not even exist is the
            // sharper probe here too.
            "--codex-sha256",
            "4444444444444444444444444444444444444444444444444444444444444444",
            "--run-dir",
            run.as_str(),
            "--codex-home",
            codex_home.as_str(),
            "--approval-policy",
            "untrusted",
            "--approvals-reviewer",
            "user",
            "--sandbox",
            "read-only",
            "--hooks-enabled",
            "true",
            // Round-2 P4: the canonical launch cwd (the workspace anchor).
            "--launch-cwd",
            "/tmp",
        ])
        .env("CODECONNECT_HOME", &launch.home)
        .stdin(Stdio::null())
        .output()
        .expect("run the host");

    assert_eq!(
        out.status.code(),
        Some(EX_HOST_FATAL),
        "adopting an existing run dir must fail closed"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("must NOT already exist"),
        "the refusal should name the reason: {stderr}"
    );
    assert!(
        processes_matching(run.as_str()).is_empty(),
        "a refused bring-up must spawn nothing: {:?}",
        processes_matching(run.as_str())
    );
    println!("PASS an_existing_run_dir_is_refused ({})", stderr.trim());
}
