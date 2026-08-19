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
        let sock_dir = ShortTmpDir::new("sock").expect("mk sock dir");
        let codex_home = ShortTmpDir::new("home").expect("mk codex home");
        let sock_path = sock_dir.join("as.sock");
        assert_sun_len(&sock_path);

        // Capture stderr to a file (not an unread pipe, which could fill and block the
        // child); read it back only if startup fails.
        let stderr_path = codex_home.join("appserver.stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr log");

        let listen = format!("unix://{}", sock_path.display());
        let child = Command::new(codex)
            .arg("app-server")
            .arg("--listen")
            .arg(&listen)
            .env("CODEX_HOME", &codex_home.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .expect("spawn codex app-server");

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

fn fingerprint() -> LaunchFingerprint {
    LaunchFingerprint {
        approval_policy: "untrusted".into(),
        approvals_reviewer: "user".into(),
        sandbox: "read-only".into(),
        hooks_enabled: true,
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
        let sock_dir = ShortTmpDir::new("broker").expect("mk broker sock dir");
        let tui_sock = sock_dir.join("t.sock");
        let ccd_sock = sock_dir.join("c.sock");
        assert_sun_len(&tui_sock);
        assert_sun_len(&ccd_sock);

        let factory = WsUdsUpstreamFactory::new(upstream_sock.to_path_buf());
        let broker = Broker::new(tui_sock.clone(), ccd_sock.clone(), fingerprint(), factory);
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
