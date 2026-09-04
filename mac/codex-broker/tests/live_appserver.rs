//! GATED live end-to-end integration: the broker's real upstream path against a LIVE
//! `codex app-server` (validated with codex 0.147; the harness itself accepts whatever
//! `codex` is installed). This is the harness every later Phase-2e live gate reuses.
//!
//! Unlike `relay_integration.rs` (which drives a scripted fake upstream), these tests
//! stand up a real `codex app-server` on an isolated `CODEX_HOME`, point the production
//! [`WsUdsUpstreamFactory`] at its unix socket, and drive the full [`Broker`] over its
//! own two UDS legs — proving the whole relay path (client → broker security core →
//! live app-server → back) works on the real wire.
//!
//! # What the harness cleans up
//!
//! Cleanup is BEST-EFFORT, not guaranteed. On Drop, [`LiveAppServer`] SIGKILLs the DIRECT
//! `app-server` child it spawned and reaps it with a bounded poll (errors ignored; the reap
//! is not guaranteed), and removes its temp dirs; [`LiveBroker`] requests (does not await)
//! cancellation of the broker task it spawned. A bare app-server that never runs a turn
//! spawns no tool children, but this harness does not claim to guarantee freedom from every
//! conceivable descendant process.
//!
//! # Why every test is `#[ignore]` AND env-gated
//!
//! It needs `codex` installed and spawns a real subprocess, so a normal `cargo test`
//! must never run it. Two independent guards:
//!   * `#[ignore]` — excluded unless `-- --ignored` is passed.
//!   * `CC_CODEX_LIVE=1` — even under `--ignored`, each test no-ops (skip message, no
//!     panic) unless the env flag is set, and it also no-ops (skip, no panic) if the
//!     native `codex` binary cannot be located. Missing infra is a skip, never a failure.
//!
//! Run it deliberately:
//! ```text
//! CC_CODEX_LIVE=1 cargo test -p codex-broker --test live_appserver -- --ignored --nocapture
//! ```

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::WebSocketStream;

use codex_broker::relay::Broker;
use codex_broker::upstream::WsUdsUpstreamFactory;
use codex_broker::LaunchFingerprint;

// ---------------------------------------------------------------------------
// SUN_LEN: a unix-domain socket PATH must be shorter than `sun_len` (~104 bytes
// on macOS / ~108 on Linux); binding a longer path fails with
// `path must be shorter than SUN_LEN`. A deep repository/build path is easily too
// long, so every socket in this file lives under a SHORT `/tmp` dir.
// ---------------------------------------------------------------------------
const SUN_LEN_LIMIT: usize = 104;

/// The exact synthetic refusal the security core emits for a `command/exec` (code-exec
/// bypass) on every leg. Asserted verbatim so a wording drift in production is caught.
const CODE_EXEC_REFUSAL_MESSAGE: &str = "method refused: code-execution is not permitted";
/// The security core's synthetic policy-refusal JSON-RPC error code.
const E_POLICY_REFUSED: i64 = -32001;

/// Assert (and document) that a socket path is short enough to `bind()`. Called for every
/// socket this harness creates so a future path change can never silently reintroduce the
/// SUN_LEN failure.
fn assert_sun_len(path: &Path) {
    let len = path.as_os_str().len();
    assert!(
        len < SUN_LEN_LIMIT,
        "socket path {path:?} is {len} bytes; a unix socket path must be < {SUN_LEN_LIMIT} \
         (SUN_LEN) or bind() fails — keep sockets under a short /tmp dir, never a deep build path",
    );
}

/// A short-path temp directory under `/tmp` that removes itself on Drop. Used for socket
/// dirs and the isolated `CODEX_HOME` so a panicking test makes a best effort to leave
/// nothing behind (removal errors are ignored).
///
/// `/tmp` (not `std::env::temp_dir()`, which on macOS is a long `/var/folders/...` path)
/// keeps contained socket paths inside SUN_LEN.
///
/// The dir name mixes pid, a process-global atomic counter, and a high-resolution
/// wall-clock nanosecond stamp, and is created with a single `mkdir(path, 0700)` (which
/// FAILS if the path already exists). That makes creation exclusive AND private from birth:
/// a stale dir left by a reused pid can never be silently adopted (whose leftover socket
/// might otherwise satisfy startup detection), and there is no world-readable window.
/// Creation retries with a fresh name a bounded number of times.
struct ShortTmpDir {
    path: PathBuf,
}

impl ShortTmpDir {
    fn new(tag: &str) -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        static N: AtomicU32 = AtomicU32::new(0);
        let pid = std::process::id();
        let mut last_err: Option<std::io::Error> = None;
        for _ in 0..16 {
            let n = N.fetch_add(1, Ordering::SeqCst);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = PathBuf::from(format!("/tmp/ccas.{pid}.{tag}.{n}.{nanos}"));
            // Atomic, exclusive, private create: `DirBuilder::mode(0o700).create` issues one
            // `mkdir(path, 0700)` — it FAILS if the name is taken (so a stale pid-reused dir is
            // never adopted) and it REQUESTS mode 0700 at creation (masked by umask, so the
            // result is at most 0700 and can never gain group/world bits) — no world-readable
            // window between a default-0777 create and a later chmod, and no chmod-failure leak.
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not create a unique /tmp/ccas.* dir after 16 attempts",
            )
        }))
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for ShortTmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// True iff `path` is a regular FILE with at least one executable bit set. `.exists()` is
/// not enough: it matches a directory or a non-executable file, which then panics at spawn.
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

/// Resolve the NATIVE codex binary. Prefer `~/.local/bin/codex` (the standalone native
/// build); fall back to `which codex`. Returns `None` (never panics) if none is a valid
/// executable file — the caller turns that into a skip.
fn resolve_codex() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        let native = PathBuf::from(home).join(".local/bin/codex");
        if is_executable_file(&native) {
            return Some(native);
        }
    }
    let out = Command::new("which").arg("codex").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return None;
    }
    let path = PathBuf::from(path);
    if is_executable_file(&path) {
        Some(path)
    } else {
        None
    }
}

/// The gate every test opens with. `Some(codex_path)` means run; `None` means skip (an
/// explanatory line was already printed). No panic on missing infra.
fn live_gate() -> Option<PathBuf> {
    if std::env::var("CC_CODEX_LIVE").as_deref() != Ok("1") {
        eprintln!("SKIP live_appserver: set CC_CODEX_LIVE=1 to run the live app-server tests");
        return None;
    }
    match resolve_codex() {
        Some(p) => Some(p),
        None => {
            eprintln!("SKIP live_appserver: no codex binary found (~/.local/bin/codex or PATH)");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The live app-server harness
// ---------------------------------------------------------------------------

/// A live `codex app-server` bound to an isolated unix socket with an isolated
/// `CODEX_HOME`. On Drop it makes a best-effort attempt to kill and reap the DIRECT child it
/// spawned and remove both temp dirs, so a passing or panicking test makes a best effort
/// not to leave behind the spawned app-server process, its socket, or its /tmp dirs
/// (kill/reap/remove errors are ignored and the reap is bounded, not guaranteed).
struct LiveAppServer {
    child: Child,
    sock_path: PathBuf,
    /// The socket dir and the isolated CODEX_HOME: kept alive for the server's lifetime,
    /// removed on Drop (field order gives: kill child, then drop dirs).
    _sock_dir: ShortTmpDir,
    codex_home: ShortTmpDir,
    stderr_path: PathBuf,
}

impl LiveAppServer {
    /// Spawn `codex app-server --listen unix://<sock>` and wait (bounded) for its bound
    /// 0600 socket to appear. Fails clearly — printing the child's captured stderr — if the
    /// process exits before binding.
    fn spawn(codex: &Path) -> LiveAppServer {
        LiveAppServer::spawn_in(codex, None)
    }

    /// As [`LiveAppServer::spawn`], but optionally in a chosen working directory.
    ///
    /// The app-server's own cwd is what it reports as `thread/start`'s `result.cwd`
    /// (MEASURED), so a test that needs a real creation to satisfy the broker's launch-cwd
    /// anchor has to put the app-server in the directory it claims to have launched in.
    /// That is exactly the production arrangement: the coordinator's
    /// `tmux new-session -c <launch cwd>` pane holds the host, and the app-server and TUI
    /// both inherit it.
    fn spawn_in(codex: &Path, cwd: Option<&Path>) -> LiveAppServer {
        let sock_dir = ShortTmpDir::new("sock").expect("mk sock dir");
        let codex_home = ShortTmpDir::new("home").expect("mk codex home");
        let sock_path = sock_dir.join("as.sock");
        assert_sun_len(&sock_path);

        // Capture stderr to a file (not an unread pipe, which could fill and block the
        // child); read it back only if startup fails.
        let stderr_path = codex_home.join("appserver.stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr log");

        let listen = format!("unix://{}", sock_path.display());
        let mut cmd = Command::new(codex);
        cmd.arg("app-server")
            .arg("--listen")
            .arg(&listen)
            .env("CODEX_HOME", &codex_home.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file));
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let child = cmd.spawn().expect("spawn codex app-server");

        let mut server = LiveAppServer {
            child,
            sock_path,
            _sock_dir: sock_dir,
            codex_home,
            stderr_path,
        };
        server.wait_for_socket(Duration::from_secs(5));
        server
    }

    /// Poll until the path is a REAL socket at mode 0600 (polling past a bind-then-chmod
    /// race, where a freshly bound socket is briefly not yet 0600), up to `timeout`. Panics
    /// (with captured stderr) if the child exits first or the socket never appears.
    fn wait_for_socket(&mut self, timeout: Duration) {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        let deadline = Instant::now() + timeout;
        loop {
            // Did the app-server die before binding?
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "codex app-server exited before binding (status {status}). stderr:\n{}",
                    self.read_stderr()
                );
            }
            if let Ok(meta) = std::fs::metadata(&self.sock_path) {
                // The app-server binds a 0600 socket shortly after start. Require BOTH the
                // socket file type and 0600 before proceeding.
                let is_sock = meta.file_type().is_socket();
                let mode = meta.permissions().mode() & 0o777;
                if is_sock && mode == 0o600 {
                    return;
                }
                // Not yet a 0600 socket (bind-then-chmod race): keep polling.
            }
            if Instant::now() >= deadline {
                panic!(
                    "codex app-server socket {:?} did not appear as a 0600 socket within {:?}. \
                     stderr:\n{}",
                    self.sock_path,
                    timeout,
                    self.read_stderr()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn read_stderr(&self) -> String {
        let mut s = String::new();
        if let Ok(mut f) = std::fs::File::open(&self.stderr_path) {
            let _ = f.read_to_string(&mut s);
        }
        s
    }

    fn sock_path(&self) -> &Path {
        &self.sock_path
    }

    /// The isolated `CODEX_HOME` path passed to the child — the value the app-server should
    /// echo back as `initialize` `result.codexHome`.
    fn codex_home(&self) -> &Path {
        &self.codex_home.path
    }
}

impl Drop for LiveAppServer {
    fn drop(&mut self) {
        // Best-effort teardown of the direct child: SIGKILL, then reap with a BOUNDED poll
        // (never an unbounded `wait()` that could hang teardown if the child lingers). After
        // SIGKILL a reap normally completes within a few polls; if not, we give up rather
        // than block — so this makes a best effort, it does not guarantee the reap.
        let _ = self.child.kill();
        for _ in 0..200 {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Broker harness over the LIVE upstream
// ---------------------------------------------------------------------------

/// The launch fingerprint for the tests that never create a thread.
///
/// Its `launch_cwd` is the sanitized placeholder `/work/proj`, and that is honest here for a
/// precise reason: `launch_cwd` is consulted ONLY on `thread/start` — by the creation-request
/// guards and the creation-response verifier — and none of the `initialize` /
/// allowlisted-read / code-exec-bypass tests sends one. A test that DOES create a thread must
/// use [`fingerprint_in`] with a real directory; see
/// `a_real_thread_start_is_admitted_with_the_workspace_anchor_in_place`.
fn fingerprint() -> LaunchFingerprint {
    fingerprint_in("/work/proj")
}

/// The same fingerprint anchored to a REAL directory — required for a real creation, because
/// the broker checks both `result.cwd` and `result.runtimeWorkspaceRoots` against this value.
fn fingerprint_in(launch_cwd: &str) -> LaunchFingerprint {
    LaunchFingerprint {
        approval_policy: "untrusted".into(),
        approvals_reviewer: "user".into(),
        sandbox: "read-only".into(),
        hooks_enabled: true,
        launch_cwd: launch_cwd.into(),
    }
}

/// A running [`Broker`] whose upstream factory points at the live app-server socket. Its
/// two UDS legs live under a self-cleaning short `/tmp` dir. On Drop the spawned broker
/// task is aborted on a best-effort basis (the abort is requested, not awaited).
struct LiveBroker {
    tui_sock: PathBuf,
    ccd_sock: PathBuf,
    _sock_dir: ShortTmpDir,
    task: JoinHandle<()>,
}

impl LiveBroker {
    fn start(upstream_sock: &Path) -> LiveBroker {
        LiveBroker::start_with(upstream_sock, fingerprint())
    }

    fn start_with(upstream_sock: &Path, fingerprint: LaunchFingerprint) -> LiveBroker {
        let sock_dir = ShortTmpDir::new("broker").expect("mk broker sock dir");
        let tui_sock = sock_dir.join("t.sock");
        let ccd_sock = sock_dir.join("c.sock");
        assert_sun_len(&tui_sock);
        assert_sun_len(&ccd_sock);

        let factory = WsUdsUpstreamFactory::new(upstream_sock.to_path_buf());
        // The verbatim frame recorder, off unless `CC_CODEX_FRAME_TEE` names a file.
        // This harness is the instrument's own ground-truth check: it drives KNOWN
        // frames through a real app-server, so a capture taken here can be compared
        // against what was sent.
        let broker = Broker::new(tui_sock.clone(), ccd_sock.clone(), fingerprint, factory)
            .with_frame_tee(codex_broker::FrameTee::from_env().expect("frame tee"));
        let task = tokio::spawn(async move {
            let _ = broker.serve().await;
        });
        LiveBroker {
            tui_sock,
            ccd_sock,
            _sock_dir: sock_dir,
            task,
        }
    }

    fn ccd_sock(&self) -> &Path {
        &self.ccd_sock
    }
}

impl Drop for LiveBroker {
    fn drop(&mut self) {
        // Request cancellation of the spawned serve loop (best-effort: not awaited).
        self.task.abort();
    }
}

/// Connect a WS client to one of the broker's UDS legs (retrying until the broker's
/// listeners are bound), under an overall bounded deadline. The WS handshake itself is
/// timeout-guarded — it can hang after a successful UDS connect.
async fn connect(path: &Path) -> WebSocketStream<UnixStream> {
    // One hard overall cap covers the whole connect+handshake retry loop, so neither an
    // unguarded `UnixStream::connect` nor a handshake started just before the deadline can
    // push total time past it.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(stream) = UnixStream::connect(path).await {
                let handshake = tokio::time::timeout(
                    Duration::from_secs(2),
                    tokio_tungstenite::client_async("ws://localhost/", stream),
                )
                .await;
                if let Ok(Ok((ws, _))) = handshake {
                    return ws;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("could not connect to broker leg at {path:?} within 5s"))
}

/// Send one frame under a short timeout so a wedged/backpressured leg surfaces as a clear
/// failure rather than an unbounded hang.
async fn send_frame(ws: &mut WebSocketStream<UnixStream>, msg: Message) {
    tokio::time::timeout(Duration::from_secs(5), ws.send(msg))
        .await
        .expect("ws send timed out")
        .expect("ws send failed");
}

/// Read frames until one is a JSON-RPC RESPONSE to `want_id`, or time out. Prints every
/// received frame (so `--nocapture` shows the real wire).
///
/// A response matches iff `id == want_id` AND the frame has NO `method` member AND exactly
/// one of `result`/`error` is present. JSON-RPC is bidirectional and reuses ids, so a
/// server→client REQUEST or a notification that happens to carry the same id (it has a
/// `method`) is NOT the response — it is skipped (these tests do not answer server
/// requests). No-id notifications are skipped too. A hard `deadline` re-checked at the top
/// of every iteration bounds total time even against a flood of immediately-ready frames.
async fn recv_response(
    ws: &mut WebSocketStream<UnixStream>,
    want_id: i64,
    timeout: Duration,
) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        // Hard deadline first: a flood of immediately-ready frames to skip must not outrun
        // the timeout (a zero-duration `timeout` can still return a ready frame).
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(r) if !r.is_zero() => r,
            _ => panic!("timed out waiting for a response with id={want_id}"),
        };
        let next = tokio::time::timeout(remaining, ws.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for a response with id={want_id}"));
        // An explicit match (not chained `unwrap_or_else`) keeps the large tungstenite
        // `Err` out of a closure return, which `clippy::result_large_err` would flag.
        let msg = match next {
            Some(Ok(m)) => m,
            Some(Err(e)) => panic!("ws read error before id={want_id}: {e}"),
            None => panic!("broker leg closed before id={want_id}"),
        };
        let text = match &msg {
            Message::Text(t) => t.clone(),
            Message::Close(_) => panic!("broker leg closed (Close frame) before id={want_id}"),
            _ => continue, // ping/pong/binary: not a JSON-RPC frame
        };
        println!("BROKER->CLIENT {text}");
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // We always send numeric ids and the app-server (and the broker's synthetic errors)
        // echo them as numbers, so match the numeric id strictly — a string `"N"` is not it.
        let id_matches = v.get("id").and_then(|id| id.as_i64()) == Some(want_id);
        let has_method = v.get("method").is_some();
        let has_result = v.get("result").is_some();
        let has_error = v.get("error").is_some();
        // A response: matching id, no method, exactly one of result/error.
        if id_matches && !has_method && (has_result ^ has_error) {
            return v;
        }
        // Otherwise it's a server→client request/notification (has a `method`), a no-id
        // notification, or a malformed frame — keep reading.
    }
}

/// The canonical `initialize` request (confirmed to round-trip on the live wire).
fn initialize_frame(id: i64) -> Message {
    Message::Text(format!(
        r#"{{"id":{id},"method":"initialize","params":{{"clientInfo":{{"name":"cc","title":"cc","version":"0.0.0"}}}}}}"#
    ))
}

/// The same `initialize`, additionally declaring the `experimentalApi` capability.
///
/// MEASURED on this gate: without it the app-server refuses `thread/start`'s
/// `runtimeWorkspaceRoots` outright with its own `-32600
/// "thread/start.runtimeWorkspaceRoots requires experimentalApi capability"`. The field is
/// capability-gated, so any client that populates it — the real TUI included — must declare
/// this at `initialize` first.
fn initialize_frame_experimental(id: i64) -> Message {
    Message::Text(format!(
        r#"{{"id":{id},"method":"initialize","params":{{"clientInfo":{{"name":"cc","title":"cc","version":"0.0.0"}},"capabilities":{{"experimentalApi":true}}}}}}"#
    ))
}

/// Assert a frame is exactly the broker's synthetic code-exec policy refusal for `id`.
fn assert_code_exec_refusal(v: &serde_json::Value, id: i64) {
    assert_eq!(v["id"], id, "id echoed on the synthetic refusal: {v}");
    assert_eq!(
        v["error"]["code"].as_i64(),
        Some(E_POLICY_REFUSED),
        "the security core's synthetic policy-refusal code (not an app-server error): {v}"
    );
    assert_eq!(
        v["error"]["message"].as_str(),
        Some(CODE_EXEC_REFUSAL_MESSAGE),
        "the exact broker-generated refusal message (app-server cannot emit this): {v}"
    );
}

/// Assert a frame is a genuine live `initialize` result: a NUMERIC `id` echoed, a
/// `userAgent` string, and a `codexHome` string equal (canonicalized) to the isolated
/// `CODEX_HOME` this harness created. Strict enough that `{"id":"N","result":{}}` fails.
fn assert_initialize_result(v: &serde_json::Value, id: i64, expected_home: &Path) {
    assert_eq!(v["id"].as_i64(), Some(id), "numeric id echoed: {v}");
    let result = &v["result"];
    assert!(
        result.is_object(),
        "initialize carries a result object: {v}"
    );
    assert!(
        result["userAgent"].is_string(),
        "result.userAgent is a string: {v}"
    );
    let reported_home = result["codexHome"]
        .as_str()
        .unwrap_or_else(|| panic!("result.codexHome is a string: {v}"));
    // Canonicalize both sides so the macOS /tmp → /private/tmp symlink can't spuriously
    // fail an otherwise-equal path.
    let expected = std::fs::canonicalize(expected_home).expect("canonicalize CODEX_HOME");
    let reported = std::fs::canonicalize(reported_home)
        .unwrap_or_else(|e| panic!("canonicalize reported codexHome {reported_home:?}: {e}"));
    assert_eq!(
        reported, expected,
        "result.codexHome equals the isolated CODEX_HOME the harness created \
         (reported {reported_home:?})"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test 1 — `initialize` round-trips through the broker end to end.
///
/// Proves the whole relay path (client → broker security core → live app-server → back):
/// the client's `initialize` is allowlisted-forwarded on the TUI leg, the live app-server
/// answers, and its `{"id":0,"result":{...}}` relays back carrying a string `userAgent` and
/// a `codexHome` equal to the isolated `CODEX_HOME` this harness created.
#[tokio::test]
#[ignore = "live: needs a real codex app-server; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn initialize_round_trips_through_broker() {
    let Some(codex) = live_gate() else { return };
    let server = LiveAppServer::spawn(&codex);
    let broker = LiveBroker::start(server.sock_path());
    let mut ws = connect(&broker.tui_sock).await;

    send_frame(&mut ws, initialize_frame(0)).await;
    let v = recv_response(&mut ws, 0, Duration::from_secs(10)).await;

    assert_initialize_result(&v, 0, server.codex_home());
    println!("PASS initialize_round_trips_through_broker");
}

/// Test 2 — a benign, allowlisted read-only request forwards and its response relays.
///
/// `model/list` is admitted for the TUI role by `allowlist::tui_request` (Forward). After
/// an `initialize` handshake on the same client leg, the request is
/// forwarded to the live app-server and its own response relays back — a genuine forward
/// through the real wire, distinct from the local synthetic refusal path in test 3. No
/// turn/tool execution is started.
#[tokio::test]
#[ignore = "live: needs a real codex app-server; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn allowlisted_read_only_request_forwards_and_relays() {
    let Some(codex) = live_gate() else { return };
    let server = LiveAppServer::spawn(&codex);
    let broker = LiveBroker::start(server.sock_path());
    let mut ws = connect(&broker.tui_sock).await;

    // Handshake first: the app-server expects `initialize` before other requests. Assert it
    // actually succeeded (result object) before relying on the leg.
    send_frame(&mut ws, initialize_frame(0)).await;
    let init = recv_response(&mut ws, 0, Duration::from_secs(10)).await;
    assert!(
        init["result"].is_object(),
        "initialize handshake succeeded with a result object: {init}"
    );

    // A read-only allowlisted request.
    send_frame(
        &mut ws,
        Message::Text(r#"{"id":1,"method":"model/list","params":{}}"#.into()),
    )
    .await;
    let v = recv_response(&mut ws, 1, Duration::from_secs(10)).await;

    assert_eq!(v["id"], 1, "id echoed");
    // A genuine upstream response: it came from the app-server, not the broker. A broker
    // refusal would be a synthetic error with code -32001; assert that did NOT happen.
    let is_broker_refusal = v["error"]["code"].as_i64() == Some(E_POLICY_REFUSED);
    assert!(
        !is_broker_refusal,
        "model/list must be forwarded, not refused by the broker: {v}"
    );
    // The stable shape of the app-server's response: a result object with a `data` array.
    let result = &v["result"];
    assert!(
        result.is_object(),
        "the app-server's model/list response relayed a result object: {v}"
    );
    assert!(
        result["data"].is_array(),
        "model/list result carries a data array (the stable shape): {v}"
    );
    println!("PASS allowlisted_read_only_request_forwards_and_relays");
}

/// Test 3 — a code-exec bypass method returns the security core's synthetic policy refusal,
/// and the refused leg stays usable.
///
/// `command/exec` resolves to `Refuse(CodeExecBypass)` on EVERY leg. This test proves the
/// client receives the broker's synthetic JSON-RPC error (code -32001, exact message
/// "method refused: code-execution is not permitted") on BOTH the TUI and ccd legs, and
/// that the TUI leg — kept open because a policy refusal is not hostile — can then still
/// reach a usable live app-server via a real `initialize`. (The refusal happens before any
/// allowed upstream request, so this shows the client leg remains usable, not that a
/// particular upstream connection pre-existed or persisted.)
///
/// This is a client-side test, so it CANNOT observe whether any byte reached upstream —
/// a broker that both refused the client AND forwarded upstream would still pass here.
/// Proving NON-forwarding (zero upstream bytes) is the job of the fake-upstream unit tests
/// in `relay_integration.rs`, where the fake records every admitted frame and the refused
/// method is simply absent from `recorded`. A live client has no interposing recorder
/// between broker and app-server and cannot see that; here we assert only that the refusal
/// is returned and the leg remains usable.
///
/// The payload command is inert (`/usr/bin/true`), not destructive: the method is what is
/// refused, and a test payload must stay harmless for defense-in-depth even if the core
/// ever regressed to forward.
#[tokio::test]
#[ignore = "live: needs a real codex app-server; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn code_exec_bypass_returns_policy_refusal_and_leg_remains_usable() {
    let Some(codex) = live_gate() else { return };
    let server = LiveAppServer::spawn(&codex);
    let broker = LiveBroker::start(server.sock_path());

    // Refused on the TUI leg. Inert command — the METHOD is the bypass being refused.
    let mut ws = connect(&broker.tui_sock).await;
    send_frame(
        &mut ws,
        Message::Text(
            r#"{"id":2,"method":"command/exec","params":{"command":["/usr/bin/true"]}}"#.into(),
        ),
    )
    .await;
    let v = recv_response(&mut ws, 2, Duration::from_secs(10)).await;
    assert_code_exec_refusal(&v, 2);

    // Refused on the ccd leg too — the refusal matrix refuses command/exec on every leg.
    let mut ccd = connect(broker.ccd_sock()).await;
    send_frame(
        &mut ccd,
        Message::Text(
            r#"{"id":5,"method":"command/exec","params":{"command":["/usr/bin/true"]}}"#.into(),
        ),
    )
    .await;
    let cv = recv_response(&mut ccd, 5, Duration::from_secs(10)).await;
    assert_code_exec_refusal(&cv, 5);

    // The TUI leg stayed open (a policy refusal is not hostile). A real `initialize` now
    // proves the same client leg can still reach a usable live app-server after the refusal
    // (a genuine result echoing the isolated CODEX_HOME — not a hollow `{"result":{}}`).
    // This shows the leg remains usable; it does not assert connection continuity versus a
    // lazily (re)established upstream.
    send_frame(&mut ws, initialize_frame(3)).await;
    let init = recv_response(&mut ws, 3, Duration::from_secs(10)).await;
    assert_initialize_result(&init, 3, server.codex_home());
    println!("PASS code_exec_bypass_returns_policy_refusal_and_leg_remains_usable");
}

/// Test 4 — the A10 follow-on workspace anchor, proven on the REAL wire.
///
/// A `thread/start` naming the session's one launch workspace through BOTH channels
/// (`cwd` and `runtimeWorkspaceRoots`) must still be ADMITTED and reach the live app-server,
/// and the response it comes back with must still install a binding. This is the "the anchor
/// does not refuse real traffic" half of the gate — the half a unit test with synthetic
/// constants cannot honestly claim, because it chooses both sides of the comparison.
///
/// The harness is made production-honest rather than the rule weakened: the app-server is
/// spawned IN a real directory and the fingerprint's `launch_cwd` is that same directory,
/// canonicalized — exactly what the coordinator does (`canonical_launch_cwd`, then
/// `tmux new-session -c` for the pane that both the host and the TUI inherit). The sanitized
/// `/work/proj` placeholder cannot be used here, and that is the anchor doing its job.
#[tokio::test]
#[ignore = "live: needs a real codex app-server; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn a_real_thread_start_is_admitted_with_the_workspace_anchor_in_place() {
    let Some(codex) = live_gate() else { return };

    // The launch workspace: a real directory, canonicalized exactly once — the coordinator's
    // discipline, reproduced. On macOS this resolves /tmp → /private/tmp, which is the very
    // symlink that makes exact string equality fail without it.
    let workspace = ShortTmpDir::new("ws").expect("mk workspace dir");
    let launch_cwd = std::fs::canonicalize(&workspace.path).expect("canonicalize workspace");
    let launch_cwd = launch_cwd
        .to_str()
        .expect("utf-8 workspace path")
        .to_string();

    let server = LiveAppServer::spawn_in(&codex, Some(Path::new(&launch_cwd)));
    let broker = LiveBroker::start_with(server.sock_path(), fingerprint_in(&launch_cwd));

    // MEASURED on this very gate: a plain `initialize` makes the app-server answer a
    // `thread/start` carrying `runtimeWorkspaceRoots` with its OWN error, `-32600
    // "thread/start.runtimeWorkspaceRoots requires experimentalApi capability"`. The field is
    // capability-gated at `initialize`, so the real TUI must declare it — and so must this
    // harness, or the creation never gets far enough to exercise the anchor.
    let mut ws = connect(&broker.tui_sock).await;
    send_frame(&mut ws, initialize_frame_experimental(0)).await;
    let init = recv_response(&mut ws, 0, Duration::from_secs(10)).await;
    assert_initialize_result(&init, 0, server.codex_home());

    // The production-shaped creation: `runtimeWorkspaceRoots: [<the TUI's own cwd>]`, which
    // in production IS the launch cwd (MEASURED against a real codex 0.147 TUI).
    let start = serde_json::json!({
        "id": 1,
        "method": "thread/start",
        "params": {
            "approvalPolicy": "untrusted",
            "approvalsReviewer": "user",
            "sandbox": "read-only",
            "cwd": serde_json::Value::Null,
            "runtimeWorkspaceRoots": [&launch_cwd]
        }
    });
    send_frame(&mut ws, Message::Text(start.to_string())).await;
    let v = recv_response(&mut ws, 1, Duration::from_secs(20)).await;

    // THE LOAD-BEARING ASSERTION: whatever came back is the APP-SERVER's own answer, not the
    // broker's synthetic policy refusal. If the anchor were wrong about the production shape,
    // this frame would carry E_POLICY_REFUSED and zero bytes would have left the broker.
    assert_ne!(
        v["error"]["code"].as_i64(),
        Some(E_POLICY_REFUSED),
        "the anchored creation must be ADMITTED, not policy-refused: {v}"
    );

    // ROUND-5 FINDING 10a — THIS BLOCK USED TO BE OPTIONAL, AND THAT MADE THE TEST HOLLOW.
    //
    // It was `if let Some(result) = … { assert }` with an `else { println!("NOTE …") }`, on
    // the argument that an unauthenticated isolated `CODEX_HOME` might answer with the
    // app-server's own error. So ANY upstream error passed, and the test could go green
    // having created and bound nothing — masking exactly the regressions it exists to catch.
    //
    // MEASURED (live 0.147.0, isolated `CODEX_HOME`, no auth, `initialize` declaring
    // `experimentalApi`): a `thread/start` in this shape returns a RESULT every time, and the
    // hedge was never true. Creation is now asserted unconditionally.
    let result = v
        .get("result")
        .filter(|r| r.is_object())
        .unwrap_or_else(|| panic!("the live creation must return a result, not an error: {v}"));
    let created = result["thread"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("a real creation carries result.thread.id: {v}"));
    assert!(!created.is_empty(), "a non-empty thread id: {v}");
    assert_eq!(
        result["cwd"].as_str(),
        Some(launch_cwd.as_str()),
        "the live app-server reports the launch cwd it was started in: {v}"
    );
    assert_eq!(
        result["runtimeWorkspaceRoots"],
        serde_json::json!([&launch_cwd]),
        "the live app-server echoes runtimeWorkspaceRoots back verbatim — the measured \
         behaviour this anchor is built on: {v}"
    );
    println!("PASS live creation CREATED thread {created}, both halves of the anchor verified");

    // …AND THAT THE BROKER ACTUALLY BOUND IT. Creating a thread upstream proves nothing about
    // `crate::session`: the binding is installed only when the correlated creation RESPONSE on
    // the SAME connection passes `verify_creation_result`. The observable is
    // `check_resume_binding`, which admits a `thread/resume` for a session thread and refuses
    // one for anything else — so the pair below distinguishes "bound" from "the broker forwards
    // every resume", which a single positive could not.
    send_frame(
        &mut ws,
        Message::Text(
            serde_json::json!({"id": 2, "method": "thread/resume",
                               "params": {"threadId": created}})
            .to_string(),
        ),
    )
    .await;
    let bound = recv_response(&mut ws, 2, Duration::from_secs(10)).await;
    assert_ne!(
        bound["error"]["code"].as_i64(),
        Some(E_POLICY_REFUSED),
        "a resume of the thread the broker just bound must be ADMITTED, not policy-refused \
         — if this is -32001 the creation response never installed a binding: {bound}"
    );
    send_frame(
        &mut ws,
        Message::Text(
            r#"{"id":3,"method":"thread/resume","params":{"threadId":"01a0399e-0000-0000-0000-000000000000"}}"#.into(),
        ),
    )
    .await;
    let stranger = recv_response(&mut ws, 3, Duration::from_secs(10)).await;
    assert_eq!(
        stranger["error"]["code"].as_i64(),
        Some(E_POLICY_REFUSED),
        "a resume of a thread this session never bound must be refused — otherwise the \
         positive above proves only that resumes forward: {stranger}"
    );
    println!("PASS live creation BOUND: {created} resumes, a stranger id does not");

    // And the NEGATIVE, on the same live wire: a creation naming any OTHER root is refused by
    // the broker itself. A fresh leg, because the slot machinery is per-session.
    let mut ws2 = connect(&broker.tui_sock).await;
    send_frame(&mut ws2, initialize_frame_experimental(10)).await;
    let _ = recv_response(&mut ws2, 10, Duration::from_secs(10)).await;
    let mut wide = start.clone();
    wide["id"] = serde_json::json!(11);
    wide["params"]["runtimeWorkspaceRoots"] = serde_json::json!([&launch_cwd, "/"]);
    send_frame(&mut ws2, Message::Text(wide.to_string())).await;
    let refused = recv_response(&mut ws2, 11, Duration::from_secs(10)).await;
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(E_POLICY_REFUSED),
        "a creation widening the workspace is refused by the broker on the live wire: {refused}"
    );
    println!("PASS a_real_thread_start_is_admitted_with_the_workspace_anchor_in_place");
}

/// Connect DIRECTLY to the live app-server's own socket, bypassing the broker entirely, and
/// complete its handshake.
///
/// This is the negative control the rest of this file cannot express: a client-side test sees
/// only what the broker returns, so "the broker refused it" and "the upstream would have
/// refused it anyway" are indistinguishable from the client leg. Speaking to the app-server on
/// the same socket the broker's own [`WsUdsUpstreamFactory`] uses settles that question with a
/// measurement instead of an argument — a pin over a field the upstream rejects on its own is
/// a pin nobody needs, and a pin over a field the upstream ACCEPTS is a hole that was open.
async fn connect_direct(server: &LiveAppServer, id: i64) -> WebSocketStream<UnixStream> {
    let stream = tokio::time::timeout(
        Duration::from_secs(5),
        UnixStream::connect(server.sock_path()),
    )
    .await
    .expect("direct connect to the app-server socket timed out")
    .expect("direct connect to the app-server socket failed");
    let (mut ws, _) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::client_async("ws://localhost/", stream),
    )
    .await
    .expect("direct app-server handshake timed out")
    .expect("direct app-server handshake failed");
    send_frame(&mut ws, initialize_frame_experimental(id)).await;
    let init = recv_response(&mut ws, id, Duration::from_secs(10)).await;
    assert!(
        init["result"].is_object(),
        "the direct app-server handshake succeeded: {init}"
    );
    ws
}

/// Test 5 — the `thread/start` capability boundary (round-5 finding 5), proven on the REAL
/// wire in BOTH directions.
///
/// Every frame here satisfies the 2e-7c workspace anchor and the full launch fingerprint; the
/// only thing wrong with it is a populated capability channel. The test asserts two things
/// that only a live run can pair:
///
/// 1. the broker REFUSES it (`-32001`, its own synthetic code, which the app-server cannot
///    emit), and
/// 2. the app-server, asked the same question DIRECTLY, **ACCEPTS it and creates a real
///    thread** — i.e. the pin closes a hole that was genuinely open, not one the upstream was
///    already covering.
///
/// `environments` is deliberately excluded from half 2 and asserted separately below: the
/// installed build rejects an UNREGISTERED environment id on its own, which is a real bound
/// but one that lives in the upstream's lookup table rather than in this broker.
#[tokio::test]
#[ignore = "live: needs a real codex app-server; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn populated_capability_channels_are_refused_but_the_app_server_would_accept_them() {
    let Some(codex) = live_gate() else { return };
    let workspace = ShortTmpDir::new("ws").expect("mk workspace dir");
    let launch_cwd = std::fs::canonicalize(&workspace.path).expect("canonicalize workspace");
    let launch_cwd = launch_cwd.to_str().expect("utf-8 path").to_string();
    let server = LiveAppServer::spawn_in(&codex, Some(Path::new(&launch_cwd)));
    let broker = LiveBroker::start_with(server.sock_path(), fingerprint_in(&launch_cwd));

    let creation = |id: i64, key: &str, value: serde_json::Value| {
        let mut f = serde_json::json!({
            "id": id,
            "method": "thread/start",
            "params": {
                "approvalPolicy": "untrusted",
                "approvalsReviewer": "user",
                "sandbox": "read-only",
                "cwd": serde_json::Value::Null,
                "runtimeWorkspaceRoots": [&launch_cwd]
            }
        });
        f["params"][key] = value;
        f
    };
    let cap_root = serde_json::json!([{"id": "probe-root",
        "location": {"type": "environment", "environmentId": "probe-env", "path": "/"}}]);
    let dyn_tools = serde_json::json!([{"type": "function", "name": "probe_tool",
        "description": "probe", "inputSchema": {"type": "object"}}]);
    let envs = serde_json::json!([{"environmentId": "probe-env", "cwd": "/",
        "runtimeWorkspaceRoots": ["/"]}]);

    // HALF 1 — the broker refuses all three, on the live wire, with its own code.
    let mut ws = connect(&broker.tui_sock).await;
    send_frame(&mut ws, initialize_frame_experimental(0)).await;
    let _ = recv_response(&mut ws, 0, Duration::from_secs(10)).await;
    for (id, key, value) in [
        (1, "selectedCapabilityRoots", cap_root.clone()),
        (2, "dynamicTools", dyn_tools.clone()),
        (3, "environments", envs.clone()),
    ] {
        send_frame(&mut ws, Message::Text(creation(id, key, value).to_string())).await;
        let v = recv_response(&mut ws, id, Duration::from_secs(20)).await;
        assert_eq!(
            v["error"]["code"].as_i64(),
            Some(E_POLICY_REFUSED),
            "a populated {key} must be refused by the BROKER on the live wire: {v}"
        );
    }
    println!("PASS all three capability channels refused by the broker on the live wire");

    // HALF 2 — the app-server, asked directly, creates a real thread for two of them. This is
    // the vulnerability the pin closes, measured rather than argued.
    let mut direct = connect_direct(&server, 100).await;
    for (id, key, value) in [
        (101, "selectedCapabilityRoots", cap_root),
        (102, "dynamicTools", dyn_tools),
    ] {
        send_frame(
            &mut direct,
            Message::Text(creation(id, key, value).to_string()),
        )
        .await;
        let v = recv_response(&mut direct, id, Duration::from_secs(20)).await;
        let created = v["result"]["thread"]["id"].as_str().unwrap_or_else(|| {
            panic!(
                "the live app-server ACCEPTS a populated {key} — if this ever stops being \
                 true the pin's justification must be re-grounded, not the pin removed: {v}"
            )
        });
        println!("PASS unpinned {key} would have created live thread {created}");
    }

    // `environments` is the one the installed build defends on its own, and the defence is
    // named rather than relied on: the app-server rejects an unregistered environment id, and
    // the only method that registers one — `environment/add` — is `Refuse(NotAllowlisted)` on
    // both legs. A defence in the upstream's lookup table is not one this broker can assert.
    send_frame(
        &mut direct,
        Message::Text(creation(103, "environments", envs).to_string()),
    )
    .await;
    let v = recv_response(&mut direct, 103, Duration::from_secs(20)).await;
    println!("NOTE direct environments answer (upstream-side bound, not the broker's): {v}");
    println!("PASS populated_capability_channels_are_refused_but_the_app_server_would_accept_them");
}

/// Test 6 — the `thread/resume` captured boundary (round-5 finding 6), on the REAL wire.
///
/// Every resume below names the session's OWN bound thread, so the binding check that used to
/// be a resume's only gate passes on all of them. Three halves:
///
/// 1. the broker refuses each bypass with its own `-32001`;
/// 2. the ccd's own legitimate resume — literally `{"threadId": <id>}`, which is what
///    `mac/ccd/src/codex_link.rs` constructs — is still ADMITTED and reaches the app-server;
/// 3. asked DIRECTLY, the app-server honours `history` and `runtimeWorkspaceRoots`: the same
///    resume that errors without `history` instead returns a **brand-new thread id** whose
///    preview is the injected text, and adding `runtimeWorkspaceRoots: ["/"]` binds the whole
///    filesystem as that thread's runtime workspace root.
#[tokio::test]
#[ignore = "live: needs a real codex app-server; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn resume_bypasses_are_refused_but_the_app_server_would_honour_them() {
    let Some(codex) = live_gate() else { return };
    let workspace = ShortTmpDir::new("ws").expect("mk workspace dir");
    let launch_cwd = std::fs::canonicalize(&workspace.path).expect("canonicalize workspace");
    let launch_cwd = launch_cwd.to_str().expect("utf-8 path").to_string();
    let server = LiveAppServer::spawn_in(&codex, Some(Path::new(&launch_cwd)));
    let broker = LiveBroker::start_with(server.sock_path(), fingerprint_in(&launch_cwd));

    let mut ws = connect(&broker.tui_sock).await;
    send_frame(&mut ws, initialize_frame_experimental(0)).await;
    let _ = recv_response(&mut ws, 0, Duration::from_secs(10)).await;
    send_frame(
        &mut ws,
        Message::Text(
            serde_json::json!({
                "id": 1, "method": "thread/start",
                "params": {
                    "approvalPolicy": "untrusted", "approvalsReviewer": "user",
                    "sandbox": "read-only", "cwd": serde_json::Value::Null,
                    "runtimeWorkspaceRoots": [&launch_cwd]
                }
            })
            .to_string(),
        ),
    )
    .await;
    let created = recv_response(&mut ws, 1, Duration::from_secs(20)).await;
    let bound = created["result"]["thread"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the live creation must bind a thread: {created}"))
        .to_string();

    let injected = serde_json::json!([{"type": "message", "role": "user",
        "content": [{"type": "input_text", "text": "INJECTED HISTORY"}]}]);
    let resume = |id: i64, key: &str, value: serde_json::Value, thread: &str| {
        let mut f = serde_json::json!({
            "id": id, "method": "thread/resume", "params": {"threadId": thread}
        });
        f["params"][key] = value;
        f
    };

    // HALF 1 — every bypass refused by the broker, naming the session's own bound thread.
    for (id, key, value) in [
        (2, "runtimeWorkspaceRoots", serde_json::json!(["/"])),
        (3, "cwd", serde_json::json!("/")),
        (4, "history", injected.clone()),
        (5, "path", serde_json::json!("/tmp/rollout-other.jsonl")),
        (
            6,
            "selectedCapabilityRoots",
            serde_json::json!([{"id": "r",
                "location": {"type": "environment", "environmentId": "e", "path": "/"}}]),
        ),
    ] {
        send_frame(
            &mut ws,
            Message::Text(resume(id, key, value, &bound).to_string()),
        )
        .await;
        let v = recv_response(&mut ws, id, Duration::from_secs(10)).await;
        assert_eq!(
            v["error"]["code"].as_i64(),
            Some(E_POLICY_REFUSED),
            "a resume carrying {key} must be refused by the BROKER, even naming the bound \
             thread: {v}"
        );
    }
    println!("PASS every resume bypass refused by the broker on the live wire");

    // HALF 2 — THE NON-NEGOTIABLE ONE. The ccd's own resume still reaches the app-server.
    send_frame(
        &mut ws,
        Message::Text(
            serde_json::json!({"id": 7, "method": "thread/resume",
                               "params": {"threadId": &bound}})
            .to_string(),
        ),
    )
    .await;
    let ccd_shape = recv_response(&mut ws, 7, Duration::from_secs(10)).await;
    assert_ne!(
        ccd_shape["error"]["code"].as_i64(),
        Some(E_POLICY_REFUSED),
        "the ccd's own legitimate resume must still be ADMITTED — a guard that breaks the \
         real resume is a worse bug than the one it fixes: {ccd_shape}"
    );
    println!("PASS the ccd's own resume shape is still admitted: {ccd_shape}");

    // HALF 3 — what the unpinned path would have allowed, straight from the app-server. The
    // resumed thread is NOT running on this direct connection, which is the state the schema
    // note is about ("If specified for a non-running thread, the thread_id param will be
    // ignored") — and it is the state a reconnecting client is always in.
    let mut direct = connect_direct(&server, 200).await;
    let absent = "01a0399e-0000-0000-0000-0000000000ab";
    send_frame(
        &mut direct,
        Message::Text(
            serde_json::json!({"id": 201, "method": "thread/resume",
                               "params": {"threadId": absent}})
            .to_string(),
        ),
    )
    .await;
    let control = recv_response(&mut direct, 201, Duration::from_secs(20)).await;
    assert!(
        control["error"].is_object(),
        "CONTROL: a resume of a non-running thread with no rollout must ERROR without \
         history, or half 3 proves nothing: {control}"
    );

    let mut steer = resume(202, "history", injected, absent);
    steer["params"]["runtimeWorkspaceRoots"] = serde_json::json!(["/"]);
    send_frame(&mut direct, Message::Text(steer.to_string())).await;
    let v = recv_response(&mut direct, 202, Duration::from_secs(20)).await;
    let minted = v["result"]["thread"]["id"].as_str().unwrap_or_else(|| {
        panic!(
            "the live app-server honours a substituted history on a non-running thread — if \
             this stops being true the pin's justification must be re-grounded, not the pin \
             removed: {v}"
        )
    });
    assert_ne!(
        minted, absent,
        "the whole point: the answer names a thread the client never asked for and the \
         broker never bound: {v}"
    );
    assert_eq!(
        v["result"]["runtimeWorkspaceRoots"],
        serde_json::json!(["/"]),
        "…and its runtime workspace root is the WHOLE FILESYSTEM: {v}"
    );
    println!(
        "PASS unpinned resume would have minted thread {minted} (asked for {absent}) rooted at /"
    );
    println!("PASS resume_bypasses_are_refused_but_the_app_server_would_honour_them");
}
