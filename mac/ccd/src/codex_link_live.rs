//! GATED live integration for the **ccd control link** (Phase 2e-3), against a
//! real codex 0.147: a real coordinator, a real host, a real broker, a real
//! app-server and a real `codex --remote` TUI in a real tmux pane.
//!
//! `codex_link.rs`'s scripted test proves the state machine deterministically.
//! This proves the thing that actually ships — that the machine speaks the wire
//! the broker's ccd leg really answers with. Six claims, in order:
//!
//!   1. **The ccd leg answers the handshake and its census reads.** A raw client
//!      completes `initialize` over WS-over-UDS on `ccd.sock` and gets back a real
//!      app-server result; `thread/loaded/list` — a ccd-allowlisted read — is
//!      answered rather than refused.
//!   2. **The link binds from the live stream, and its frames become facts.** The
//!      real [`crate::codex_link::run`] task, given no thread id at all, learns one
//!      from the `thread/started` the TUI broadcasts to a merely-initialized
//!      connection (A1/D1/D2), and that frame lands in the store as a normalized
//!      fact through the ordinary [`crate::state::Daemon::ingest`] path.
//!   3. **The no-rollout answer, asserted at the only moment it exists.** Before
//!      any turn, a raw `thread/resume` is admitted by the broker's session binding
//!      and answers with the measured not-ready error — the world
//!      [`crate::codex_link`]'s attach contract was written against, pinned here at
//!      the last point in the run where it is still true.
//!   4. **A real turn runs through the broker, on the deferral-discharging path.**
//!      A prompt is typed into the pane and submitted; the broker forwards the
//!      `turn/start` it produces under the **exact** note that says the sandbox
//!      deferral was discharged by the verified thread binding, and the TUI renders
//!      the reply.
//!   5. **A connection in the link's position is handed no turn frames — counted,
//!      not inferred, on a connection proven alive at BOTH ends of the window.** A
//!      raw observer sitting where the link sits records every server→client `method`
//!      it receives across the turn; the claim is a count on that list — zero
//!      `turn/*`, zero `item/*`, and `thread/status/changed` as the whole of what
//!      that position IS handed — with a post-turn round trip on the observer's own
//!      connection acting as a wire barrier and proving the zeros are
//!      a fact about the wire rather than about a dead tap, and the link's unchanged
//!      fact set as corroboration. This is the tripwire for 2e-4b.
//!   6. **The populated resume answer agrees with the committed ground truth and with
//!      the launch, and STOP-AND-AMEND does its job.** The raw resume now returns a
//!      result whose effective policy, turn shape and thread identity match
//!      `fixtures/codex/resume-populated-answer.json` field for field on everything
//!      that is not per-run content, and whose `cwd` equals the launch cwd this test
//!      canonicalizes **independently** of the answer; a fresh link handed the same
//!      thread id refuses that answer, ends the connection, and reconnects — in
//!      initialize→resume cycles paired by the broker's own **connection identity** —
//!      emitting a redacted report this gate reads back out of the daemon's real log
//!      sink and asserts.
//!
//! # Turns now run — what that changes, and what it does not
//!
//! This gate used to assert the opposite: that the broker refused the TUI's
//! `turn/start`, and that no turn could run. It no longer does, and the assertion
//! that stood there was built to be flipped exactly here. Two things moved in the
//! broker: `fingerprint.rs` reads the real TUI's explicit `"sandboxPolicy": null`
//! as a **deferral** to the named thread's own policy rather than an unprovable
//! claim, and `refusal.rs`'s `FingerprintThenHeadCheck` discharges that deferral by
//! forwarding a `turn/start` only when its `threadId` is the session's one bound
//! thread. So the broker.log line a submitted prompt now produces is a forward, and
//! claim 4 asserts **that exact note** — not merely "a turn/start was forwarded",
//! because the discharge is the whole security claim — with the old refusal asserted
//! **negatively** beside it, so a regression names itself instead of quietly reading
//! as "no turn was attempted".
//!
//! **The ccd link still records nothing across that turn, and claim 5 now MEASURES
//! that rather than inferring it from the store.** Turn frames are delivered only to
//! the connection **subscribed** to the thread, and subscription is what a
//! successful `thread/resume` buys. A raw connection that completed
//! `initialize` + `initialized` and resumed nothing — exactly the position this link
//! is in while its `thread/resume` keeps failing — received, across a whole turn,
//! only `remoteControl/status/changed`, `thread/started`, `app/list/updated` and
//! `thread/status/changed`: **zero** `turn/*` and **zero** `item/*` frames, against
//! seven on the subscribed connection. Across the turn window itself the only method
//! that arrives is `thread/status/changed` — one or two of them, because the thread
//! going idle rides with `turn/completed` and can land after the window closes on
//! the TUI's rendering of the reply — so claim 5 pins the VOCABULARY, which a
//! fan-out change moves, and requires the count to be non-zero, which is what says
//! anybody was listening. Claim 5 attaches such a connection for real
//! and counts what it is handed, so "the link saw nothing" is a fact about the wire
//! and not about an empty table — a broken adapter that discarded everything would
//! satisfy the empty table just as well. `thread/status/changed` maps to
//! `Vec::new()` in [`crate::codex_adapter`] as observation noise, so the link's fact
//! set is byte-for-byte unchanged across a turn it can see happening; that equality
//! is asserted too, as **corroboration** of the count.
//!
//! Both halves are a **tripwire**: when 2e-4b lands and the link accepts the
//! populated resume answer, it becomes subscribed and *will* be handed `turn/*` and
//! `item/*` frames and *will* record turn-scoped facts — and these assertions
//! breaking is the signal that 2e-4b's landing is what changed them.
//!
//! # What this does NOT prove, stated exactly
//!
//! **The link still does not reconcile.** After a turn, `thread/resume` answers
//! with a `result` carrying a one-element `turns[]` — captured verbatim at
//! `fixtures/codex/resume-populated-answer.json` by an earlier run of this gate, and
//! now *asserted against* rather than merely pointed at. That is not the measured
//! not-ready error, so `settle_resume` takes its STOP-AND-AMEND branch, the
//! connection ends, and `run()` reconnects and asks again. Claim 6 asserts that loop
//! is what happens — by the **pairing** of initializes to resumes on the ccd leg,
//! since a bare count cannot tell a reconnect loop from a retry on one connection —
//! and it is **correct behaviour until 2e-4b**: guessing at a response shape nobody
//! had designed against is the one thing this chunk refuses to do, and a
//! reconnect-and-keep-observing loop is the honest cost of refusing. The redacted
//! report that loop emits is read back out of `log::capture` — the line production
//! really wrote, not one this gate reconstructed — and asserted to name the thread
//! and the turn count while carrying no frame dump, no result key name and none of
//! the session's content; the reconstruction is kept beside it as corroboration.
//!
//! **2e-4b obligation.** The evidence that was missing now exists, and the
//! reconciliation design must be grounded in it rather than assumed:
//!
//!   * whether `turns[]` is **complete** for the turns it reports, or a summary;
//!   * whether item ids **key uniquely** across a resume — D15 says they do not,
//!     inside an interrupted turn;
//!   * what a disconnect-completion actually looks like in the response, so the
//!     durable `…:pre:` call can be closed from real evidence;
//!
//!   and the assertions must reach the **store**: the recovered terminals' dedup
//!   keys and payloads, not merely a returned value. `first-turn.jsonl` and
//!   `resume-populated-answer.json` under `fixtures/codex/` are the wire this must
//!   be designed against.
//!
//! Claim 6 also drops the connection by **ending the link task**, which is the
//! daemon-restart shape (a new `run` starting from a known thread id), not a
//! mid-`run` EOF. The `run` loop's own reconnect — EOF, backoff, re-handshake and
//! the retryable not-ready attach — is proven deterministically in
//! `codex_link::tests::the_link_binds_reconnects_and_retries_the_measured_attach`,
//! because a live broker offers no lever to drop one leg on demand without also
//! killing what is under test.
//!
//! # Why `#[ignore]` AND env-gated (mirrors `live_codex_coordinator.rs`)
//!
//! It needs `codex` and `tmux` and spawns real subprocesses, so a normal
//! `cargo test` must never run it. Two independent guards: `#[ignore]`, and
//! `CC_CODEX_LIVE=1`. With the flag SET, a missing premise is a **failure**, never
//! a skip — an operator who demanded a live run must not be handed a vacuous green.
//!
//! Run it deliberately, after building the launcher this harness drives:
//! ```text
//! cargo build -p codeconnect
//! CC_CODEX_LIVE=1 cargo test -p ccd --bin ccd codex_link_live -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use protocol::event::SessionKey;
use serde_json::Value;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::codex_link::{ControlLink, TempDb};

/// Per-sandbox sequence, so two live sandboxes can never derive the same run dir.
static SANDBOX_SEQ: AtomicU32 = AtomicU32::new(0);

/// The codex series this gate is grounded against — the series `codeconnect`'s own
/// launcher pins.
const LIVE_CODEX_VERSION_PREFIX: &str = "0.147.";

const VERSION_PROBE_BUDGET: Duration = Duration::from_secs(20);
const PROBE_OUTPUT_LIMIT: u64 = 64 * 1024;

/// The stable prefix of the broker's `turn/start` forward note — enough to answer
/// "was a turn forwarded at all?", and therefore what the wait polls on. It is
/// **not** enough to assert with: see [`TURN_FORWARD_NOTE`].
const TURN_FORWARD_PREFIX: &str = "Tui: forward (turn/start";

/// The **exact** note a forwarded `turn/start` must carry, verbatim from
/// `codex-broker`'s `refusal.rs`.
///
/// The real TUI sends an explicit `"sandboxPolicy": null`, which the fingerprint
/// reads as a deferral to the named thread's own policy; the head-check discharges
/// it by forwarding only when `params.threadId` is the session's one bound thread.
/// That discharge is the whole security claim of this chunk, so the assertion is on
/// the note that names it. The prefix alone would still pass if the deferral were
/// replaced by some looser acceptance — a `Forward` that never checked the head, or
/// a fingerprint that took `null` as "no constraint" — which is exactly the
/// regression worth catching.
///
/// The sibling note, `turn/start: fingerprint asserted and head-checked`, belongs to
/// a turn carrying an explicit matching sandbox string; a real 0.147 TUI does not
/// send one, so it is unreachable here and asserting it would assert nothing.
const TURN_FORWARD_NOTE: &str = "Tui: forward (turn/start: head-checked; sandbox deferral \
                                 discharged by the verified thread binding)";

/// The broker's refusal when a session that already has a thread bound (or a
/// creation in flight) is asked for a second one. Verbatim from `refusal.rs`.
///
/// Asserted **negatively**: this gate's whole thread-identity story assumes the one
/// thread the link bound in claim 2 is the one every later claim is about. If the
/// TUI ever starts a second thread, the broker now refuses it — and without this
/// assertion that refusal would surface only as a downstream mystery (a resume for a
/// thread nobody ran a turn on), rather than naming itself.
const SECOND_THREAD_REFUSAL: &str = "thread/start: this session already has a thread bound";

/// The ccd-leg note for `initialize` and the census reads.
const CCD_INITIALIZE_NOTE: &str = "Ccd: forward (request allowlisted)";
/// The ccd-leg note for `initialized`.
const CCD_INITIALIZED_NOTE: &str = "Ccd: forward (notification allowlisted)";
/// The ccd-leg note for `thread/resume` — the ONE thing ccd may send that is
/// classified an ownership request, so it cannot be mistaken for anything else.
const CCD_RESUME_NOTE: &str = "Ccd: forward (ownership request: fingerprint asserted)";

/// The committed ground truth for a post-turn `thread/resume` answer, captured off
/// this very wire.
///
/// Used in **assertions**, not as documentation: the live answer must agree with it
/// on the structural, non-content fields — the effective sandbox, the approval
/// policy and reviewer, the shape of `turns[]`. Content fields (prompt, reply, cwd,
/// rollout path) legitimately differ per run and are precisely what must not be
/// pinned, so nothing here compares them.
const POPULATED_RESUME_FIXTURE: &str =
    include_str!("../../../fixtures/codex/resume-populated-answer.json");

// ---------------------------------------------------------------- scaffolding

fn tmux_bin() -> Option<PathBuf> {
    [
        "/opt/homebrew/bin/tmux",
        "/usr/local/bin/tmux",
        "/usr/bin/tmux",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

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
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    is_executable_file(&path).then_some(path)
}

/// The `codeconnect` launcher this harness drives, resolved from this test
/// binary's own target directory (`target/<profile>/deps/ccd-*` → `../codeconnect`).
///
/// `CARGO_BIN_EXE_*` is only handed to integration tests of the crate that owns the
/// binary, and `ccd` is a binary crate with no library, so the live link can only be
/// exercised from a unit test here. A missing launcher is a **failure** with the
/// command that fixes it, never a skip.
fn resolve_codeconnect() -> PathBuf {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps/<test binary>");
    let bin = profile_dir.join("codeconnect");
    assert!(
        is_executable_file(&bin),
        "CC_CODEX_LIVE=1 but the launcher this harness drives is not built at {}. \
         Run `cargo build -p codeconnect` first (same CARGO_TARGET_DIR).",
        bin.display()
    );
    bin
}

fn is_native_executable(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() {
        return false;
    }
    matches!(
        u32::from_be_bytes(magic),
        0xFEED_FACE
            | 0xFEED_FACF
            | 0xCEFA_EDFE
            | 0xCFFA_EDFE
            | 0xCAFE_BABE
            | 0xBEBA_FECA
            | 0xCAFE_BABF
            | 0xBFBA_FECA
    )
}

fn parse_codex_version(text: &str) -> Option<String> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let line = lines.next()?;
    if lines.next().is_some() {
        return None;
    }
    let version = match line.split_whitespace().collect::<Vec<_>>().as_slice() {
        [version] => *version,
        ["codex-cli", version] => *version,
        _ => return None,
    };
    version
        .starts_with(|c: char| c.is_ascii_digit())
        .then(|| version.to_string())
}

/// `codex --version` under a hard budget, in its own process group so a descendant
/// holding the pipes can be reached. Hitting the output ceiling is a failure, not an
/// end of output.
fn codex_version_bounded(bin: &Path, budget: Duration) -> Result<String, String> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("could not run it: {e}"))?;
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = match stdout {
            Some(pipe) => {
                let mut buf = Vec::new();
                let mut bounded = std::io::Read::take(pipe, PROBE_OUTPUT_LIMIT + 1);
                match std::io::Read::read_to_end(&mut bounded, &mut buf) {
                    Ok(_) if buf.len() as u64 > PROBE_OUTPUT_LIMIT => Err(
                        "it wrote more than a version line; that is a wrapper streaming \
                         something else"
                            .to_string(),
                    ),
                    Ok(_) => Ok(buf),
                    Err(e) => Err(format!("the read failed part-way ({e})")),
                }
            }
            None => Ok(Vec::new()),
        };
        let _ = tx.send(outcome);
    });
    let answer = rx.recv_timeout(budget);
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
    let bytes = answer.map_err(|_| format!("it did not answer within {budget:?}"))??;
    parse_codex_version(&String::from_utf8_lossy(&bytes))
        .ok_or_else(|| "its output was not a single parseable version line".to_string())
}

/// The gate. `None` ⇒ skip, and only for the one honest reason: the operator did not
/// ask for a live run.
fn live_gate() -> Option<PathBuf> {
    if std::env::var("CC_CODEX_LIVE").as_deref() != Ok("1") {
        eprintln!("SKIP codex_link_live: set CC_CODEX_LIVE=1 to run it");
        return None;
    }
    let Some(codex) = resolve_codex() else {
        panic!(
            "CC_CODEX_LIVE=1 was set but no codex binary could be found. A demanded live \
             run must FAIL rather than pass vacuously."
        )
    };
    assert!(
        is_native_executable(&codex),
        "CC_CODEX_LIVE=1 resolved {} but it is not a native executable.",
        codex.display()
    );
    let version = match codex_version_bounded(&codex, VERSION_PROBE_BUDGET) {
        Ok(v) => v,
        Err(why) => panic!(
            "CC_CODEX_LIVE=1 resolved {} but its version could not be established: {why}.",
            codex.display()
        ),
    };
    assert!(
        version.starts_with(LIVE_CODEX_VERSION_PREFIX),
        "CC_CODEX_LIVE=1 resolved {} reporting codex {version}, but this gate is grounded \
         against {LIVE_CODEX_VERSION_PREFIX}x.",
        codex.display()
    );
    assert!(
        tmux_bin().is_some(),
        "CC_CODEX_LIVE=1 was set but no tmux was found."
    );
    eprintln!(
        "live gate premise verified: {} is codex {version}",
        codex.display()
    );
    Some(codex)
}

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

fn read_file(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| format!("<unreadable: {e}>"))
}

async fn wait_until(budget: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The ccd-leg lines a broker log has grown since `skip` lines.
///
/// Both spellings the broker uses, because they are different facts and this gate
/// needs both: `Ccd: …` is what a connection *did* (leg opened, forwards, read
/// errors), while `Ccd leg ended (conn N): …` — no colon after the role — is how a
/// connection *finished*. Filtering on `"Ccd: "` alone silently drops every clean
/// disconnect, which is precisely the evidence claim 6b's STOP-AND-AMEND story rests
/// on.
fn ccd_tail(log: &str, skip: usize) -> Vec<&str> {
    log.lines()
        .skip(skip)
        .filter(|l| l.contains("Ccd: ") || l.contains("Ccd leg ended"))
        .collect()
}

/// Truncate `text` to at most `limit` **bytes**, backing up to a character boundary.
///
/// `&text[..limit]` panics when `limit` lands inside a multi-byte character, and the
/// frames on this wire carry multi-byte data (a captured resume answer runs 6178
/// characters in 6182 bytes), so whether the naive slice panics is a property of the
/// run's content rather than of the code. In a `println!` inside the observer task
/// that panic would kill the tap — and claim 5's zero-counts are then satisfied by a
/// connection nobody is reading, which is a vacuous pass dressed as evidence.
fn truncate_on_char_boundary(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// One wire frame, rendered for the transcript: whole if it is small, otherwise
/// capped at `limit` bytes on a character boundary with its real length named.
fn frame_preview(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    format!(
        "{}… [{} bytes]",
        truncate_on_char_boundary(text, limit),
        text.len()
    )
}

/// The two byte caps the transcript uses, proven not to split a character.
///
/// Not a hypothetical: `"€"` is three bytes, so a frame made of them has boundaries
/// only at multiples of three — and neither 2000 nor 4000 is one. The naive
/// `&text[..limit]` this replaced would panic on exactly that frame, inside the
/// observer task, taking the measurement claim 5 depends on down with it.
#[test]
fn a_frame_preview_never_splits_a_character() {
    let multibyte = "€".repeat(2000);
    assert_eq!(multibyte.len(), 6000, "'€' is three bytes");
    for limit in [2000usize, 4000] {
        assert!(
            !multibyte.is_char_boundary(limit),
            "this test is only worth anything if {limit} lands mid-character"
        );
        let preview = frame_preview(&multibyte, limit);
        let kept = limit - limit % 3;
        assert_eq!(
            preview,
            format!("{}… [6000 bytes]", "€".repeat(kept / 3)),
            "the preview must back up to the character boundary below {limit} and \
             still name the frame's real size"
        );
    }
    // The ordinary paths, so the fix cannot have been "always truncate".
    assert_eq!(frame_preview("short", 4000), "short");
    assert_eq!(
        frame_preview("abcd", 4),
        "abcd",
        "a frame exactly at the cap is whole"
    );
    assert_eq!(frame_preview("abcde", 4), "abcd… [5 bytes]");
}

/// The connection id the broker stamped on a ccd-leg line, if it carries one.
///
/// One reader serves every line the broker writes about a leg, because `relay.rs`
/// **appends** the id to all of them: `Ccd: leg opened (conn 7)`,
/// `Ccd: forward (request allowlisted) (conn 7)`, `Ccd leg ended (conn 7): closed`,
/// `Ccd: read error (conn 7): …`. Read from the LAST `(conn ` in the line, so a note
/// that one day grows a parenthesized aside of its own cannot shadow the identity.
fn conn_id(line: &str) -> Option<String> {
    let at = line.rfind("(conn ")?;
    let id = line[at + "(conn ".len()..].split_once(')')?.0;
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_digit())).then(|| id.to_string())
}

/// The ccd leg's lifecycle, in the only order a re-attaching link may walk it.
///
/// A link sends `initialize`, `initialized` and `thread/resume` and nothing else
/// (`codex_link`'s module doc), so these five markers are the whole of what one of
/// its connections can produce, and their ORDER is half the claim: a resume
/// pipelined ahead of the handshake violates A2, and a second resume on a connection
/// that never re-opened is the retry shape this branch must not be taking.
const CCD_LIFECYCLE: [&str; 5] = [
    "leg opened",
    "initialize",
    "initialized",
    "thread/resume",
    "ended",
];

/// Is this line one of the two spellings a ccd leg ends with?
///
/// Verbatim from `relay.rs`: a failed read logs `Ccd: read error (conn 7): …` and
/// then breaks, and the task that owned the connection logs
/// `Ccd leg ended (conn 7): …` for **every** outcome. So one ending can write two
/// lines, and [`ccd_connections`] treats the second as the same ending rather than
/// as a step out of order.
fn ccd_end_marker(line: &str) -> bool {
    line.contains("Ccd leg ended") || line.contains("Ccd: read error")
}

/// One broker connection's ccd-leg lifecycle, as the broker's own log recorded it.
#[derive(Debug)]
struct CcdConn {
    id: String,
    /// How many of [`CCD_LIFECYCLE`]'s markers this connection reached, in order. A
    /// connection enters this table on its `leg opened` line, so the floor is 1.
    stage: usize,
    /// Every line that arrived on this connection out of the lifecycle's order —
    /// verbatim, because a failure here is only diagnosable if it names the line.
    disordered: Vec<String>,
}

impl CcdConn {
    /// Did this connection walk the WHOLE lifecycle, in order, with nothing out of
    /// place — opened, initialize, initialized, thread/resume, and its own ending?
    fn complete(&self) -> bool {
        self.stage == CCD_LIFECYCLE.len() && self.disordered.is_empty()
    }

    /// The furthest marker this connection reached, named for the transcript.
    fn reached(&self) -> &'static str {
        CCD_LIFECYCLE[self.stage - 1]
    }
}

/// Group a ccd-leg tail by the **broker's own connection identity** and walk each
/// connection's lifecycle.
///
/// This is the discriminator counts cannot give. Two behaviours both produce "N
/// resumes on the ccd leg": a **retryable** not-ready answer keeps the SAME
/// connection and re-asks on it, while **STOP-AND-AMEND** ends the connection, so
/// `run()` reconnects, re-handshakes and asks again — every resume riding a
/// connection of its own that then closes. Only per-connection grouping tells them
/// apart, and it is what says the refusal branch — the one that refuses to guess at
/// an answer this build has never designed for — is what fired.
///
/// **Grouped on `(conn N)`, not on adjacency.** An earlier version paired by
/// position, which cannot distinguish "each resume rode a new connection" from "the
/// lines happened to interleave that way" — and they do interleave: a closing
/// connection's `leg ended` is written by its own task and can land after the next
/// connection's `leg opened`. Keying on the id the broker stamped makes the
/// segmentation the wire's answer rather than this test's inference, and makes an
/// ending count only for the connection that owns it.
///
/// A line whose connection this window never saw OPEN is ignored: it belongs to a
/// connection opened before the window — the first link's, the observer's tap — and
/// a window that did not see it start cannot judge how it ran.
fn ccd_connections(tail: &[&str]) -> Vec<CcdConn> {
    let mut conns: Vec<CcdConn> = Vec::new();
    for line in tail {
        let Some(id) = conn_id(line) else { continue };
        if line.contains("Ccd: leg opened") {
            conns.push(CcdConn {
                id,
                stage: 1,
                disordered: Vec::new(),
            });
            continue;
        }
        let step = if line.contains(CCD_INITIALIZE_NOTE) {
            2
        } else if line.contains(CCD_INITIALIZED_NOTE) {
            3
        } else if line.contains(CCD_RESUME_NOTE) {
            4
        } else if ccd_end_marker(line) {
            CCD_LIFECYCLE.len()
        } else {
            continue;
        };
        let Some(conn) = conns.iter_mut().rev().find(|c| c.id == id) else {
            continue;
        };
        // `read error` then `leg ended` is ONE ending written twice, not a second one.
        let repeat_ending = step == CCD_LIFECYCLE.len() && conn.stage == CCD_LIFECYCLE.len();
        if step == conn.stage + 1 {
            conn.stage = step;
        } else if !repeat_ending {
            conn.disordered.push((*line).to_string());
        }
    }
    conns
}

/// The verdict on one window of ccd connections: the ones that walked the whole
/// lifecycle, the single one that may legitimately still be running, and the ones
/// that are neither — `(complete, in_flight, partial)`.
///
/// **One connection is legitimately unfinished, and only one can be.** The window is
/// polled to its condition and then settles for two seconds, so the newest connection
/// may still be mid-lifecycle when the log is read — and `run()` reconnects one at a
/// time, so there is never a second one in that state. It is exempt only while it has
/// an ending still to come and nothing out of order behind it; every other connection
/// the window saw open has to be complete, and any that is not comes back in
/// `partial` to be named.
fn ccd_window_verdict(conns: &[CcdConn]) -> (Vec<&str>, Option<&str>, Vec<&CcdConn>) {
    let complete: Vec<&str> = conns
        .iter()
        .filter(|c| c.complete())
        .map(|c| c.id.as_str())
        .collect();
    let in_flight = conns
        .last()
        .filter(|c| c.stage < CCD_LIFECYCLE.len() && c.disordered.is_empty())
        .map(|c| c.id.as_str());
    let partial: Vec<&CcdConn> = conns
        .iter()
        .filter(|c| !c.complete() && Some(c.id.as_str()) != in_flight)
        .collect();
    (complete, in_flight, partial)
}

/// **The grouping claim 6b's whole discriminator rests on, over the six log shapes
/// that decide it.**
///
/// Claim 6b can only be run by paying for a real model call, so the one component
/// that decides whether its connection pairing is real would otherwise be the only
/// piece of this chunk with no in-tree evidence behind it. A parser that mis-groups
/// does not fail loudly: it makes claim 6b pass vacuously, or fail for a reason that
/// is not the one being tested.
///
/// Two of these six are REGRESSION EVIDENCE — `never-ended` and `pipelined` are
/// shapes the previous positional pairing accepted as clean cycles. They are the
/// reason this test exists, and they are commented as such below.
#[test]
fn a_ccd_leg_is_grouped_by_the_brokers_own_connection_id() {
    // The four spellings `relay.rs` writes, with the id APPENDED in every one — twice
    // of them mid-line, before a trailing `: …`, which is why the id is read from the
    // last `(conn ` rather than from the end of the line.
    assert_eq!(
        conn_id("Ccd: leg opened (conn 41)").as_deref(),
        Some("41"),
        "the open marker's id"
    );
    assert_eq!(
        conn_id("Ccd: forward (request allowlisted) (conn 7)").as_deref(),
        Some("7"),
        "a forward's id is appended AFTER the note's closing paren, so the last \
         `(conn ` is the identity and the note is not"
    );
    assert_eq!(
        conn_id("Ccd leg ended (conn 12): closed").as_deref(),
        Some("12"),
        "a clean close carries its id mid-line, before the reason"
    );
    assert_eq!(
        conn_id("Ccd: read error (conn 3): connection reset").as_deref(),
        Some("3"),
        "so does a failed read"
    );
    assert_eq!(
        conn_id("broker: listening on tui.sock and ccd.sock"),
        None,
        "a line with no connection identity has none to give"
    );

    /// `(every connection as (id, stage, out-of-order count), complete, in_flight, partial)`
    /// — the whole verdict claim 6b asserts on, in one comparable value.
    type Verdict = (
        Vec<(String, usize, usize)>,
        Vec<String>,
        Option<String>,
        Vec<String>,
    );
    let verdict = |lines: &[&str]| -> Verdict {
        let conns = ccd_connections(lines);
        let shape = conns
            .iter()
            .map(|c| (c.id.clone(), c.stage, c.disordered.len()))
            .collect();
        let (complete, in_flight, partial) = ccd_window_verdict(&conns);
        (
            shape,
            complete.iter().map(|id| (*id).to_string()).collect(),
            in_flight.map(str::to_string),
            partial.iter().map(|c| c.id.clone()).collect(),
        )
    };

    // 1. The shape a passing claim 6b sees: two connections through the whole
    //    lifecycle and a third still in flight — with conn 1's `leg ended` landing
    //    AFTER conn 2's `leg opened`, which is not contrived: the ending is written by
    //    the closing connection's own task while the accept loop writes the next open.
    //    The positional pairing this replaced could not model that at all.
    assert_eq!(
        verdict(&[
            "Ccd: leg opened (conn 1)",
            "Ccd: forward (request allowlisted) (conn 1)",
            "Ccd: forward (notification allowlisted) (conn 1)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 1)",
            "Ccd: leg opened (conn 2)",
            "Ccd leg ended (conn 1): closed",
            "Ccd: forward (request allowlisted) (conn 2)",
            "Ccd: forward (notification allowlisted) (conn 2)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 2)",
            "Ccd leg ended (conn 2): closed",
            "Ccd: leg opened (conn 3)",
            "Ccd: forward (request allowlisted) (conn 3)",
        ]),
        (
            vec![("1".into(), 5, 0), ("2".into(), 5, 0), ("3".into(), 2, 0)],
            vec!["1".into(), "2".into()],
            Some("3".into()),
            vec![],
        ),
        "an interleaved ending belongs to the connection whose id it carries, not to \
         whichever connection opened most recently"
    );

    // 2. One ending, written twice. `relay.rs` logs `read error` and breaks, and the
    //    task that owned the connection then logs `leg ended` for every outcome — so
    //    the second line must not read as a step out of order.
    assert_eq!(
        verdict(&[
            "Ccd: leg opened (conn 1)",
            "Ccd: forward (request allowlisted) (conn 1)",
            "Ccd: forward (notification allowlisted) (conn 1)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 1)",
            "Ccd: read error (conn 1): connection reset",
            "Ccd leg ended (conn 1): connection reset",
        ]),
        (vec![("1".into(), 5, 0)], vec!["1".into()], None, vec![],),
        "`read error` followed by `leg ended` on one connection is ONE ending, and a \
         parser that counted the second as disorder would fail every real reset"
    );

    // 3. The RETRYABLE shape — the opposite fact about the code, and the one claim 6b
    //    exists to exclude. One connection, three resumes, no reconnect: the link
    //    treated the answer as routine instead of refusing it.
    assert_eq!(
        verdict(&[
            "Ccd: leg opened (conn 1)",
            "Ccd: forward (request allowlisted) (conn 1)",
            "Ccd: forward (notification allowlisted) (conn 1)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 1)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 1)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 1)",
        ]),
        (vec![("1".into(), 4, 2)], vec![], None, vec!["1".into()],),
        "resuming again on a connection that already resumed is the retry shape, and \
         it must surface as a partial connection rather than as a cycle"
    );

    // 4. **PREVIOUSLY PASSED.** A resume pipelined ahead of the handshake (A2). The
    //    positional pairing only asked whether an `initialize` had been seen on this
    //    connection at some point before the resume, never whether `initialized` came
    //    between them — so this read as one clean cycle.
    assert_eq!(
        verdict(&[
            "Ccd: leg opened (conn 1)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 1)",
            "Ccd: forward (request allowlisted) (conn 1)",
            "Ccd: forward (notification allowlisted) (conn 1)",
            "Ccd leg ended (conn 1): closed",
        ]),
        (vec![("1".into(), 3, 2)], vec![], None, vec!["1".into()],),
        "a resume sent ahead of the handshake violates A2 and is not a cycle, however \
         complete the connection's line count looks"
    );

    // 5. **PREVIOUSLY PASSED.** Two connections resumed and only the newer one closed.
    //    The old rule was `ended.len() >= cycles.len() - 1`, which exempted ANY one
    //    connection rather than specifically the newest — so a connection that asked a
    //    resume and then never went away counted as a cycle, and the next `initialize`
    //    stayed indistinguishable from a second client's traffic. Only conn 2 is
    //    exempt now, because only conn 2 opened last.
    assert_eq!(
        verdict(&[
            "Ccd: leg opened (conn 1)",
            "Ccd: forward (request allowlisted) (conn 1)",
            "Ccd: forward (notification allowlisted) (conn 1)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 1)",
            "Ccd: leg opened (conn 2)",
            "Ccd: forward (request allowlisted) (conn 2)",
            "Ccd: forward (notification allowlisted) (conn 2)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 2)",
        ]),
        (
            vec![("1".into(), 4, 0), ("2".into(), 4, 0)],
            vec![],
            Some("2".into()),
            vec!["1".into()],
        ),
        "the in-flight exemption covers the newest connection and only the newest; an \
         older one that resumed and never ended is a partial, not a cycle"
    );

    // 6. A leg opened BEFORE the window — the first link's connection, or the
    //    observer's tap — whose lines land inside it. The window did not see it start,
    //    so it cannot judge how it ran, and it must not appear as a partial.
    assert_eq!(
        verdict(&[
            "Ccd leg ended (conn 99): closed",
            "Ccd: forward (request allowlisted) (conn 99)",
            "Ccd: leg opened (conn 1)",
            "Ccd: forward (request allowlisted) (conn 1)",
            "Ccd: forward (notification allowlisted) (conn 1)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 1)",
            "Ccd leg ended (conn 1): closed",
        ]),
        (vec![("1".into(), 5, 0)], vec!["1".into()], None, vec![],),
        "a connection this window never saw open belongs to an earlier one and is \
         neither complete nor partial here"
    );
}

// -------------------------------------------------------------- the sandbox

/// One live launch's isolated world: a private tmux server, a private
/// `CODECONNECT_HOME`, and a fresh `CODEX_HOME` seeded with the operator's
/// credentials (a turn cannot run without them). Everything is torn down on `Drop`.
struct LiveSandbox {
    base: PathBuf,
    home: PathBuf,
    codex_home: PathBuf,
    sock: PathBuf,
    tmux: PathBuf,
    uid: String,
    nonce: String,
    run_dir: PathBuf,
}

/// Create `path` as a private (0700) directory that must not already exist.
///
/// `create_dir_all` succeeds on a directory somebody else made, at whatever mode
/// they chose — exactly the wrong behaviour for one this harness is about to put
/// credentials in. `DirBuilder::create` with `mode(0700)` is atomic-or-fail: the
/// same idiom as `ShortTmpDir` in `live_codex_host.rs`.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

/// Assert a path is a directory this account owns, readable by nobody else.
fn assert_private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
    assert!(meta.is_dir(), "{} is not a directory", path.display());
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(
        mode,
        0o700,
        "{} is mode {mode:o}; a directory holding credentials must be 0700",
        path.display()
    );
}

impl LiveSandbox {
    fn new(tag: &str) -> LiveSandbox {
        // **A unique, unguessable, atomically created private base.** A path derived
        // from a running process's pid is predictable, and `create_dir_all` would
        // happily adopt a directory planted there in advance — under which the
        // credential written below could be read. Nanos make the name unguessable;
        // `create_private_dir` refuses to adopt anything that already exists.
        let seq = SANDBOX_SEQ.fetch_add(1, Ordering::SeqCst);
        let mut base = PathBuf::new();
        for attempt in 0..16 {
            let candidate = PathBuf::from(format!(
                "/tmp/ccll.{}.{tag}.{seq}.{:x}",
                std::process::id(),
                nanos()
            ));
            match create_private_dir(&candidate) {
                Ok(()) => {
                    base = candidate;
                    break;
                }
                Err(e) if attempt == 15 => panic!("could not create a private sandbox dir: {e}"),
                Err(_) => continue,
            }
        }
        let home = base.join("home");
        let codex_home = base.join("codexhome");
        create_private_dir(&home).expect("mk CODECONNECT_HOME");
        create_private_dir(&codex_home).expect("mk CODEX_HOME");
        assert_private_dir(&base);
        assert_private_dir(&codex_home);

        // **The operator's credentials, written into the sandbox's own CODEX_HOME.**
        //
        // This is what gets the TUI past its sign-in screen: measured on an empty
        // `CODEX_HOME`, a real `codex --remote` completes its handshake, issues two
        // bootstrap reads, and then parks on "Sign in with ChatGPT" for ever —
        // never creating a thread, so there is no `thread/started` for a control
        // link to bind to and claim 2 cannot even be attempted. Claim 4 runs a real
        // turn on this credential, which is a real (small) model call: the prompt is
        // chosen to trigger no tools and to answer in one word.
        //
        // **Read-then-write, never `fs::copy`.** `copy` follows a symlink at the
        // destination and inherits the source's mode; this reads the bytes and
        // creates the destination with `create_new` at 0600, so it cannot be made to
        // write through a planted link and cannot land world-readable. The file goes
        // with the sandbox on `Drop`, which `the_sandbox_credential_...` asserts.
        let real_auth =
            PathBuf::from(std::env::var("HOME").expect("HOME")).join(".codex/auth.json");
        let credential = std::fs::read(&real_auth).unwrap_or_else(|e| {
            panic!(
                "CC_CODEX_LIVE=1 but {} could not be read ({e}). Without it the TUI sits \
                 on its sign-in screen and never starts a thread, and a live run whose \
                 premise is unmet must FAIL rather than pass vacuously; run \
                 `codex login` first.",
                real_auth.display()
            )
        });
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(codex_home.join("auth.json"))
                .expect("create the sandbox credential");
            f.write_all(&credential).expect("write the credential");
        }

        // A well-formed ULID (26 Crockford Base32 symbols): `resolve_owned_session`
        // validates the uid stamp's shape, and a malformed one makes the
        // coordinator's post-`new-session` resolve refuse.
        let uid = format!(
            "01JQXV9K7B{:012X}{seq:04X}",
            (nanos() as u64) & 0xFFFF_FFFF_FFFF
        );
        assert_eq!(uid.len(), protocol::uid::UID_LEN, "the uid must be a ULID");
        // Only the nonce's first sixteen characters reach the run-dir name, so the
        // varying part leads and includes a process-local sequence — `SystemTime` is
        // not unique across parallel threads on this platform.
        let nonce = format!(
            "{seq:04x}{:08x}{:04x}{:016x}",
            nanos() as u32,
            std::process::id() as u16,
            nanos() as u64
        );
        // The path `codex_coordinator::choose_run_dir` derives: the uid's last ten
        // alphanumerics and the nonce's first sixteen. Restated here and asserted
        // against the record, so drift fails the test.
        let kept: Vec<char> = uid.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let uid_slug: String = kept[kept.len().saturating_sub(10)..].iter().collect();
        let nonce_slug: String = nonce
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(16)
            .collect();
        let run_dir = PathBuf::from(format!("/tmp/cch.{uid_slug}.{nonce_slug}"));
        LiveSandbox {
            sock: base.join("t.sock"),
            base,
            home,
            codex_home,
            tmux: tmux_bin().expect("tmux checked by the gate"),
            uid,
            nonce,
            run_dir,
        }
    }

    /// The coordinator, held at the bring-up boundary so the session stays alive for
    /// as long as the assertions need. The hang is a hang, not a fake readiness:
    /// nothing here reports `Ready`.
    fn spawn_coordinator(&self, codex: &Path) -> Child {
        Command::new(resolve_codeconnect())
            .arg("internal-codex-coordinator")
            .args(["--uid", &self.uid])
            .args(["--nonce", &self.nonce])
            .args(["--custodian-nonce", "livelink"])
            .args(["--session-name", "cc-live"])
            .args(["--cwd", "/tmp"])
            .args(["--tmux-socket", self.sock.to_str().unwrap()])
            // The launch deadline is what the custodian enforces on a launch that
            // never reaches `ready`, and `--test-bringup hang` guarantees this one
            // never will. It has to outlast the assertions, or the session is torn
            // down mid-run by the very machinery that is working correctly.
            .args(["--deadline-ms", "900000"])
            .args(["--codex", codex.to_str().expect("codex path is utf-8")])
            .args(["--codex-home", self.codex_home.to_str().unwrap()])
            // `on-request`, not `untrusted`, and that is a **measured** choice: a
            // real 0.147 TUI's `thread/start` carries `approvalPolicy:"on-request"`,
            // so a launch fingerprint of `untrusted` makes the broker refuse it
            //
            //   Tui: refuse->synthetic error (thread/start: fingerprint refused
            //   (Conflict): params.approvalPolicy: "on-request" but fingerprint is
            //   "untrusted")
            //
            // and the TUI then exits fatally, taking the pane and the whole session
            // with it about two seconds in. No thread is ever created, so there is
            // nothing for a control link to observe. Nothing in this chunk changes
            // the coordinator's own defaults; this gate names the value that lets a
            // thread exist, and the mismatch is reported rather than papered over.
            .args(["--approval-policy", "on-request"])
            .args(["--approvals-reviewer", "user"])
            .args(["--sandbox", "read-only"])
            .args(["--hooks-enabled", "true"])
            .args(["--test-bringup", "hang"])
            .env("CODECONNECT_HOME", &self.home)
            .env("CODECONNECT_TMUX", &self.tmux)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the real coordinator")
    }

    fn ccd_sock(&self) -> PathBuf {
        self.run_dir.join("ccd.sock")
    }

    fn broker_legs_bound(&self) -> bool {
        use std::os::unix::fs::FileTypeExt;
        ["tui.sock", "ccd.sock"].iter().all(|leg| {
            std::fs::metadata(self.run_dir.join(leg))
                .map(|m| m.file_type().is_socket())
                .unwrap_or(false)
        })
    }

    /// Type into the pane's TTY. The only way to drive the session: ccd may not
    /// start turns (the broker refuses `turn/start` to the ccd role by design), so
    /// a turn can only ever be started by the real TUI.
    fn send_keys(&self, keys: &[&str]) {
        let out = Command::new(&self.tmux)
            .args([
                "-S",
                self.sock.to_str().unwrap(),
                "-f",
                "/dev/null",
                "send-keys",
                "-t",
                "cc-live",
            ])
            .args(keys)
            .stdin(Stdio::null())
            .output()
            .expect("run tmux send-keys");
        assert!(
            out.status.success(),
            "tmux send-keys {keys:?} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Is the real `codex --remote` TUI up in the pane yet? The host launches it
    /// *after* both broker legs bind, so a link that connects on the legs alone can
    /// still be earlier than the thread it is there to watch.
    fn tui_running(&self) -> bool {
        !tagged_pids(&format!("--remote unix://{}/tui.sock", self.tag())).is_empty()
    }

    fn capture_pane(&self) -> String {
        let out = Command::new(&self.tmux)
            .args([
                "-S",
                self.sock.to_str().unwrap(),
                "-f",
                "/dev/null",
                "capture-pane",
                "-p",
                "-J",
                "-t",
                "cc-live",
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            Ok(o) => format!(
                "<capture exited {}: {}>",
                o.status,
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(e) => format!("<capture failed: {e}>"),
        }
    }

    /// What the host actually left in the run dir — named when an assertion needs
    /// to say why the log it wanted was not there.
    fn run_dir_listing(&self) -> String {
        match std::fs::read_dir(&self.run_dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(" "),
            Err(e) => format!("<unreadable: {e}>"),
        }
    }

    fn recorded_identity(&self, field: &str) -> Option<(i32, i64, i64)> {
        let text = std::fs::read_to_string(
            self.home
                .join("sessions")
                .join(&self.uid)
                .join("launch.json"),
        )
        .ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let c = v.get(field)?;
        Some((
            c.get("pid")?.as_i64()? as i32,
            c.get("birth")?.get("start_sec")?.as_i64()?,
            c.get("birth")?.get("start_usec")?.as_i64()?,
        ))
    }

    fn tag(&self) -> &str {
        self.run_dir.to_str().expect("short /tmp path is utf-8")
    }

    /// The base and the credential it holds, so a test can prove they are GONE
    /// after `Drop`. A sweep nobody checks is one that can silently stop happening,
    /// and this one is what keeps the operator's token out of `/tmp`.
    fn credential_paths(&self) -> (PathBuf, PathBuf) {
        (self.base.clone(), self.codex_home.join("auth.json"))
    }
}

impl Drop for LiveSandbox {
    fn drop(&mut self) {
        // The guardians, by VERIFIED identity: neither carries the run dir in its
        // argv, so the tag sweep below cannot reach them, and a failed assertion
        // would otherwise leave a custodian polling a deleted record forever.
        for field in ["coordinator", "custodian"] {
            if let Some((pid, sec, usec)) = self.recorded_identity(field) {
                let id = protocol::proc_identity::ProcessIdentity {
                    pid,
                    birth: protocol::proc_identity::BirthIdentity {
                        start_sec: sec,
                        start_usec: usec,
                    },
                };
                if protocol::proc_identity::liveness(&id)
                    == protocol::proc_identity::Liveness::Alive
                {
                    let _ = Command::new("/bin/kill")
                        .args(["-KILL", &pid.to_string()])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
        for pid in tagged_pids(self.tag()) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = Command::new(&self.tmux)
            .args(["-S", self.sock.to_str().unwrap(), "kill-server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.run_dir);
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

// ------------------------------------------------------------- a raw client

/// A raw WS-over-UDS client on the broker's ccd leg, used to read the wire
/// directly: what `initialize` answers, what a ccd-allowlisted census read returns,
/// and — the fact this chunk most needs on the record — the RAW shape of a
/// `thread/resume` response.
struct RawCcd {
    ws: tokio_tungstenite::WebSocketStream<UnixStream>,
    next_id: i64,
}

impl RawCcd {
    async fn connect(sock: &Path) -> RawCcd {
        let stream = UnixStream::connect(sock)
            .await
            .unwrap_or_else(|e| panic!("dial {}: {e}", sock.display()));
        let (ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream)
            .await
            .expect("the ccd leg's WebSocket handshake");
        RawCcd { ws, next_id: 100 }
    }

    /// Send one request and read frames until its response arrives. Every frame is
    /// printed, so `--nocapture` shows the real wire.
    async fn request(&mut self, method: &str, params: Value, budget: Duration) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let frame = serde_json::json!({"id": id, "method": method, "params": params});
        println!("CCD->BROKER {frame}");
        self.ws
            .send(Message::Text(frame.to_string()))
            .await
            .expect("write to the ccd leg");
        let deadline = Instant::now() + budget;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|r| !r.is_zero())
                .unwrap_or_else(|| panic!("timed out waiting for the {method} response"));
            let next = tokio::time::timeout(remaining, self.ws.next())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for the {method} response"));
            let msg = match next {
                Some(Ok(m)) => m,
                Some(Err(e)) => panic!("read error before the {method} response: {e}"),
                None => panic!("the ccd leg closed before the {method} response"),
            };
            let Message::Text(text) = msg else { continue };
            println!("BROKER->CCD {}", frame_preview(&text, 4000));
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if v.get("id").and_then(Value::as_i64) == Some(id)
                && v.get("method").is_none()
                && (v.get("result").is_some() ^ v.get("error").is_some())
            {
                return v;
            }
        }
    }

    async fn initialize(&mut self) -> Value {
        self.request(
            "initialize",
            serde_json::json!({"clientInfo": {"name": "cc", "title": "cc", "version": "0.0.0"}}),
            Duration::from_secs(20),
        )
        .await
    }

    /// Send a notification — no id, so nothing is waited for.
    async fn notify(&mut self, method: &str, params: Value) {
        let frame = serde_json::json!({"method": method, "params": params});
        println!("CCD->BROKER {frame}");
        self.ws
            .send(Message::Text(frame.to_string()))
            .await
            .expect("write a notification to the ccd leg");
    }

    /// A second, merely-initialized connection that prints every server→client frame
    /// it is handed **and records the `method` of each**, for as long as the test
    /// runs.
    ///
    /// It is the wire itself on the record: what the link sees, printed verbatim, so
    /// a claim about a notification is evidence rather than inference. It completes
    /// `initialize` + `initialized` and then sends nothing at all, which is the whole
    /// point — that is exactly the position the control link occupies while its
    /// `thread/resume` keeps being refused, so what this connection is broadcast is
    /// what the link is broadcast (A1/D2).
    ///
    /// The recorded method list is what makes claim 5 a **measurement**: "the link
    /// recorded nothing" inferred from an empty table is a claim a broken adapter
    /// satisfies too, while "no `turn/*` frame was ever delivered to a connection in
    /// this position" is a fact about the wire.
    ///
    /// **The stream is split, and that is load-bearing.** A task that owned the whole
    /// WebSocket could only ever be read *from*, so the only liveness the gate could
    /// establish would be historical — `thread/started` proves the connection was
    /// alive early, and every zero-count taken afterwards stays satisfiable by a
    /// connection that died mid-turn. The write half stays with the caller so
    /// [`Observer::probe`] can ask this very connection a question **after** the turn
    /// window closes and require an answer.
    async fn observer(sock: &Path) -> Observer {
        let mut raw = RawCcd::connect(sock).await;
        let init = raw.initialize().await;
        assert!(
            init["result"].is_object(),
            "the observer's initialize must be answered, or every count it takes is \
             a count of an unopened connection: {init}"
        );
        raw.notify("initialized", serde_json::json!({})).await;
        let methods = Arc::new(Mutex::new(Vec::<String>::new()));
        let answered = Arc::new(Mutex::new(Vec::<i64>::new()));
        let (tx, mut rx) = raw.ws.split();
        let method_sink = Arc::clone(&methods);
        let answer_sink = Arc::clone(&answered);
        let handle = tokio::spawn(async move {
            while let Some(Ok(msg)) = rx.next().await {
                if let Message::Text(text) = msg {
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        if let Some(method) = v["method"].as_str() {
                            method_sink
                                .lock()
                                .expect("the observer's method sink")
                                .push(method.to_string());
                        } else if let Some(id) = v.get("id").and_then(Value::as_i64) {
                            answer_sink
                                .lock()
                                .expect("the observer's answer sink")
                                .push(id);
                        }
                    }
                    println!("TAP {}", frame_preview(&text, 2000));
                }
            }
            println!("TAP closed");
        });
        Observer {
            tx,
            methods,
            answered,
            handle,
            next_id: 9000,
        }
    }
}

/// The tap of [`RawCcd::observer`]: a live ccd connection being read by a task,
/// still writable by the test.
struct Observer {
    /// The write half, kept out of the task so the connection can be probed while it
    /// is being tapped.
    tx: futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<UnixStream>, Message>,
    methods: Arc<Mutex<Vec<String>>>,
    /// The `id` of every response the tap has been handed — the evidence
    /// [`Observer::probe`] waits on.
    answered: Arc<Mutex<Vec<i64>>>,
    handle: tokio::task::JoinHandle<()>,
    /// Well clear of [`RawCcd`]'s own ids, so a probe can never be confused for one
    /// of them on a shared upstream.
    next_id: i64,
}

impl Observer {
    /// **Is this connection still open AND still being served?** Sends a
    /// ccd-allowlisted census read on the observer's own connection and waits for a
    /// response carrying that exact id.
    ///
    /// This is what keeps claim 5's zero-counts from being satisfiable by a corpse.
    /// `thread/started` proves the tap was live *before* the turn; a round trip taken
    /// *after* the turn window proves it was still there to be handed a `turn/*`
    /// frame and was not, which is the only version of that claim worth making.
    ///
    /// It is also claim 5's **barrier**, which is why it is awaited before the method
    /// sink is read rather than after. A response cannot overtake frames the server
    /// sent before the request, and this task reads that one stream in order — so once
    /// `answered` carries this id, everything the wire had for this connection is
    /// already in the sink. Counting first and probing afterwards would miss exactly
    /// the frames still in flight, which is the direction that makes a zero-count
    /// falsely pass.
    ///
    /// `thread/loaded/list` is the probe because claim 1 already established it is
    /// answered rather than refused on this leg, and it subscribes to nothing — so it
    /// cannot perturb the fan-out the counts above were taken on.
    async fn probe(&mut self, budget: Duration) -> bool {
        let id = self.next_id;
        self.next_id += 1;
        let frame = serde_json::json!({"id": id, "method": "thread/loaded/list", "params": {}});
        println!("TAP PROBE-> {frame}");
        if self
            .tx
            .send(Message::Text(frame.to_string()))
            .await
            .is_err()
        {
            return false;
        }
        let answered = Arc::clone(&self.answered);
        wait_until(budget, || {
            answered
                .lock()
                .expect("the observer's answer sink")
                .contains(&id)
        })
        .await
    }
}

/// Every method the observer has been handed so far.
fn observed(methods: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    methods.lock().expect("the observer's method sink").clone()
}

// ----------------------------------------------------------- the daemon side

/// A daemon on its own database, with the live run already in the session table.
/// The [`TempDb`] goes with the test, so a live run leaves no database behind.
fn live_daemon(session: &SessionKey) -> (Arc<crate::state::Daemon>, TempDb) {
    let db = TempDb::new(&format!(
        "ccd-link-live-{}-{}",
        std::process::id(),
        protocol::time::now_unix_ms()
    ));
    let store = Arc::new(crate::store::Store::open(db.path()).unwrap());
    let now = protocol::time::now_rfc3339();
    store
        .upsert_session(&crate::store::SessionRow {
            session_uid: session.uid.clone(),
            session_id: session.name.clone(),
            tmux_session: session.name.clone(),
            tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: protocol::event::Lifecycle::Live,
            created_at: now.clone(),
            updated_at: now,
            agent: protocol::agent::AgentKind::Codex,
            codex_thread_id: None,
            codex_socket: None,
        })
        .unwrap()
        .assert_present();
    let (tail_tx, tail_rx) = tokio::sync::mpsc::unbounded_channel();
    Box::leak(Box::new(tail_rx));
    let daemon = crate::state::Daemon::new(
        protocol::config::Config::default(),
        store,
        Arc::new(crate::apns::LoggingPushSender::new()),
        crate::state::Endpoint {
            host: "test.ts.net".into(),
            port: 8787,
            tls: false,
        },
        tail_tx,
    );
    (daemon, db)
}

/// Every fact the daemon holds for this run, as `(source_event_id, seq)`.
fn recorded(daemon: &Arc<crate::state::Daemon>, uid: &str) -> Vec<(String, u64)> {
    daemon
        .store
        .events_after(uid, 0, 10_000)
        .expect("read the event log")
        .into_iter()
        .map(|e| (e.source_event_id.unwrap_or_default(), e.seq))
        .collect()
}

fn print_events(daemon: &Arc<crate::state::Daemon>, uid: &str, header: &str) {
    println!("--- {header} ---");
    for e in daemon.store.events_after(uid, 0, 10_000).unwrap() {
        println!(
            "  seq={} kind={} src={} id={:?} turn={:?} item={:?}",
            e.seq,
            e.kind.as_str(),
            e.source.as_str(),
            e.source_event_id,
            e.turn_id,
            e.item_id
        );
    }
}

// ------------------------------------------------------------------- the gate

/// **The sandbox's copy of the operator's credential is private, and gone once it
/// drops.**
///
/// `Drop` sweeping the base is what keeps a real ChatGPT token out of `/tmp` after
/// a live run, and a sweep nobody checks is one that can silently stop happening —
/// a `remove_dir_all` whose error is discarded looks exactly like a successful one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn the_sandbox_credential_is_written_privately_and_removed_on_drop() {
    use std::os::unix::fs::PermissionsExt;
    if live_gate().is_none() {
        return;
    }
    let (base, credential) = {
        let sb = LiveSandbox::new("cred");
        let paths = sb.credential_paths();
        assert!(paths.1.is_file(), "the credential must have been written");
        let mode = std::fs::metadata(&paths.1)
            .expect("stat the credential")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the sandbox credential is mode {mode:o}; it must be readable by this \
             account alone"
        );
        assert_private_dir(&paths.0);
        println!(
            "credential written 0600 under a 0700 base: {}",
            paths.0.display()
        );
        paths
    };
    assert!(
        !credential.exists(),
        "the sandbox credential survived Drop: {}",
        credential.display()
    );
    assert!(
        !base.exists(),
        "the sandbox base survived Drop: {}",
        base.display()
    );
    println!("CREDENTIAL CLEANUP PASS — {} is gone", base.display());
}

/// The whole control link, live: handshake, bind, observe, reconnect, resume.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn the_control_link_observes_a_real_codex_session_and_reattaches_by_resume() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("link");
    let mut coord = sb.spawn_coordinator(&codex);

    // --- 1. A real host, a real broker, a real app-server, a real TUI ---------
    let legs = wait_until(Duration::from_secs(60), || sb.broker_legs_bound()).await;
    assert!(
        legs,
        "the pane's host should have bound tui.sock + ccd.sock under {}.\nappserver.stderr:\n{}\nbroker.log:\n{}",
        sb.run_dir.display(),
        read_file(&sb.run_dir.join("appserver.stderr.log")),
        read_file(&sb.run_dir.join("broker.log"))
    );
    println!(
        "HOST UP — both broker legs bound under {}",
        sb.run_dir.display()
    );

    // --- 2. The real link, connected BEFORE the TUI ---------------------------
    //
    // Order matters and is the whole design of claim 2: the host binds both legs
    // and only then launches the TUI, so a link that connects the moment the legs
    // appear is a merely-initialized connection when the TUI creates its thread —
    // which is precisely who `thread/started` is broadcast to (A1/D1/D2). Nothing
    // is handed to the link: it must learn the identity off the wire.
    let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-live");
    let (daemon, _db) = live_daemon(&session);
    let uid = session.uid.clone();
    let first = tokio::spawn(crate::codex_link::run(
        Arc::clone(&daemon),
        session.clone(),
        ControlLink {
            socket: sb.ccd_sock(),
            generation: 1,
            thread_id: None,
        },
    ));

    // The measuring instrument for claim 5, attached HERE rather than later: it has
    // to be in place before the TUI creates its thread, or the `thread/started` that
    // proves it is live and correctly positioned has already been broadcast to
    // nobody. It sends nothing after `initialize` + `initialized`, so it occupies the
    // link's exact position and perturbs no fan-out.
    let mut tap = RawCcd::observer(&sb.ccd_sock()).await;

    // --- 3. CLAIM 1: the ccd leg answers the handshake and its census reads ---
    let mut raw = RawCcd::connect(&sb.ccd_sock()).await;
    let init = raw.initialize().await;
    assert!(
        init["result"].is_object() && init["result"]["userAgent"].is_string(),
        "a live app-server initialize result on the ccd leg: {init}"
    );
    println!("CLAIM 1a PASS — initialize round-tripped on the ccd leg");

    // `thread/loaded/list` is one of the four reads the ccd role is allowed —
    // proven answered rather than refused, which is what makes the census leg real.
    let loaded = raw
        .request(
            "thread/loaded/list",
            serde_json::json!({}),
            Duration::from_secs(30),
        )
        .await;
    assert!(
        loaded["result"].is_object() || loaded["result"].is_array(),
        "a ccd-allowlisted census read must be answered, not refused: {loaded}"
    );
    println!("CLAIM 1b PASS — thread/loaded/list answered on the ccd leg");

    // --- 4. CLAIM 2: the real link binds from the live stream ----------------
    assert!(
        wait_until(Duration::from_secs(60), || sb.tui_running()).await,
        "the host must launch the real codex TUI. appserver.stderr:\n{}",
        read_file(&sb.run_dir.join("appserver.stderr.log"))
    );
    println!("TUI UP — the real `codex --remote` is running in the pane");

    let bound = wait_until(Duration::from_secs(120), || {
        daemon
            .store
            .events_after(&uid, 0, 100)
            .unwrap()
            .iter()
            .any(|e| e.kind == protocol::event::EventKind::SessionStart)
    })
    .await;
    print_events(&daemon, &uid, "after the TUI started its thread");
    assert!(
        bound,
        "the link must bind from a live thread/started. pane:\n{}\nrun dir: {}\nbroker.log:\n{}",
        sb.capture_pane(),
        sb.run_dir_listing(),
        read_file(&sb.run_dir.join("broker.log"))
    );
    let thread_id = daemon
        .store
        .events_after(&uid, 0, 100)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == protocol::event::EventKind::SessionStart)
        .and_then(|e| e.payload["thread_id"].as_str().map(str::to_string))
        .expect("the SessionStart fact names the thread");
    println!("CLAIM 2 PASS — the link bound to thread {thread_id} off the live stream");
    println!("pane:\n{}", sb.capture_pane());
    println!("run dir holds: {}", sb.run_dir_listing());

    // --- 5. CLAIM 3: the no-rollout answer, at the ONLY moment it exists -----
    //
    // This is the pre-turn world, and this is the last point in the gate where it
    // is true. A thread that has never run a turn has no rollout, so the resume it
    // answers with is the measured not-ready error — the single answer
    // `codex_link::settle_resume` accepts, and the whole reason its contract could
    // be total. Claim 4 below runs a turn, and from then on this thread can never
    // return to this state; asserting it afterwards would be asserting nothing.
    //
    // Issued on the live `raw` connection, with the first link still running: the
    // point is the wire's answer, not the link's, and the not-ready error leaves
    // `raw` unsubscribed, so it does not perturb the fan-out fact claim 5 measures.
    let pre_turn_resume = raw
        .request(
            "thread/resume",
            serde_json::json!({"threadId": thread_id}),
            Duration::from_secs(60),
        )
        .await;
    println!("RAW pre-turn thread/resume answer: {pre_turn_resume}");
    assert_ne!(
        pre_turn_resume["error"]["code"].as_i64(),
        Some(-32001),
        "the BROKER refused the pre-turn resume, so its session binding did not \
         learn this thread from its own stream — the attach path is unreachable \
         before the app-server ever sees it: {pre_turn_resume}"
    );
    assert!(
        crate::codex_link::is_measured_not_ready(&pre_turn_resume, &thread_id),
        "a turn-less thread must answer thread/resume with the measured not-ready \
         error. Anything else means the wire moved under the ONE answer \
         codex_link::settle_resume accepts, and the retryable attach path this \
         build ships is no longer grounded. Answer: {pre_turn_resume}"
    );
    println!(
        "CLAIM 3 PASS — before any turn, the live answer IS the measured not-ready \
         error and the link retries it rather than reporting an anomaly: {}",
        pre_turn_resume["error"]
    );

    // --- 6. CLAIM 4: a real turn RUNS through the broker ---------------------
    //
    // This replaces the old tripwire, which asserted the broker REFUSED the TUI's
    // `turn/start` and was built to be flipped exactly here. Two broker changes
    // made the forward possible: `fingerprint.rs` reads the real TUI's explicit
    // `"sandboxPolicy": null` as a deferral to the named thread's policy, and
    // `refusal.rs`'s head-check discharges that deferral by forwarding only a
    // `turn/start` whose `threadId` is the session's one bound thread.
    //
    // The prompt is chosen to trigger no tools and answer in one word, so this
    // costs one small model call and completes in seconds.
    //
    // Both measurements are snapshotted BEFORE the prompt is submitted, because
    // claim 5's whole content is what moved across the window they open: the
    // observer's method list is the primary evidence, the fact set the corroboration.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let facts_before_turn = recorded(&daemon, &uid);
    assert!(
        !facts_before_turn.is_empty(),
        "the link must have recorded the live stream before the turn"
    );
    let observed_before_turn = observed(&tap.methods).len();
    sb.send_keys(&["Reply with the single word ok and nothing else."]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    sb.send_keys(&["Enter"]);

    // The WAIT is on the stable prefix, because all it has to answer is "was a turn
    // forwarded at all?" — hanging the full 90s on a note that moved by one word
    // would report "no turn was submitted", which is a different and misleading
    // failure. The ASSERTION below is on the exact note.
    let forwarded = wait_until(Duration::from_secs(90), || {
        read_file(&sb.run_dir.join("broker.log")).contains(TURN_FORWARD_PREFIX)
    })
    .await;
    let broker_log = read_file(&sb.run_dir.join("broker.log"));
    assert!(
        forwarded,
        "the broker did not forward the TUI's turn/start within 90s. Either no turn \
         was submitted (check the pane), or the forwarding path regressed and the \
         session can no longer run work at all. pane:\n{}\nbroker.log:\n{broker_log}",
        sb.capture_pane()
    );
    // **The exact note, not the prefix.** A forward is only the right outcome if it
    // happened for the right reason: the TUI's explicit `"sandboxPolicy": null` was
    // read as a DEFERRAL and that deferral was DISCHARGED by the head-check proving
    // `params.threadId` is the session's one bound thread. The prefix alone would
    // still pass if the deferral were replaced by some looser acceptance — a
    // fingerprint that read `null` as "no constraint", or a forward that skipped the
    // head-check — and that discharge is the entire security claim of this chunk.
    //
    // The method-level refusals that guard the OTHER fingerprint fields — a
    // `turn/start` diverging on permissions, cwd or workspace roots — are broker unit
    // tests, in `codex-broker/src/refusal.rs` and `codex-broker/src/fingerprint.rs`.
    // A live TUI cannot be made to send a divergent fingerprint on demand, so
    // asserting them here would mean asserting nothing.
    assert!(
        broker_log.contains(TURN_FORWARD_NOTE),
        "the broker forwarded a turn/start, but NOT under the note that says the \
         sandbox deferral was discharged by the verified thread binding. Expected \
         verbatim:\n  {TURN_FORWARD_NOTE}\nA forward under any other note means the \
         TUI's explicit `sandboxPolicy: null` was accepted by some path other than \
         the head-check that proves the turn names the session's one bound thread — \
         which is the whole of what this gate certifies about turn safety.\n\
         broker.log:\n{broker_log}"
    );
    // Asserted negatively so the OLD failure mode names itself. A build that starts
    // refusing again would otherwise show up only as a timeout above, which reads
    // identically to "the operator's keystrokes never landed".
    assert!(
        !broker_log.contains("refuse->synthetic error (turn/start"),
        "the broker refused a turn/start. That is the pre-2e-4 behaviour returning: \
         the session cannot run work, and every claim below about a populated \
         resume answer is unreachable.\nbroker.log:\n{broker_log}"
    );
    // **The single-thread session invariant, surfaced rather than left to confuse.**
    // The broker binds a thread by correlated admitted creation and then closes the
    // slot: a second `thread/start` or `thread/fork` is refused. Every claim from 2
    // onwards is about ONE thread id, so a TUI that tried to start another would make
    // the resume claims below assert against a thread nothing ran on — and the
    // refusal would otherwise surface only as that downstream mystery.
    assert!(
        !broker_log.contains(SECOND_THREAD_REFUSAL),
        "the TUI tried to start a SECOND thread and the broker refused it (the \
         single-thread session invariant). Every claim in this gate is about the one \
         thread the link bound in claim 2, so this is not a random failure: the \
         session's thread identity is not what the assertions below assume, and the \
         resume answers they read may be about a thread that never ran a \
         turn.\nbroker.log:\n{broker_log}"
    );

    // **The turn COMPLETED, not merely started.** The only two signals this gate has
    // actually measured are the broker's forward, above, and the TUI's own
    // rendering of the reply — measured as the pane line `• ok`. Nothing else is
    // used, because nothing else has been seen: the bullet prefix is what tells the
    // agent's reply apart from the prompt echo (which contains the word "ok" too),
    // and inventing a spinner/"esc to interrupt" signal this harness has never
    // captured would be a guess dressed as an anchor. The pane is printed either
    // way, so a failure here is diagnosable rather than mysterious.
    let replied = wait_until(Duration::from_secs(90), || {
        sb.capture_pane().to_lowercase().contains("• ok")
    })
    .await;
    println!("pane after the turn:\n{}", sb.capture_pane());
    assert!(
        replied,
        "the broker forwarded the turn/start but the TUI never rendered the reply \
         within 90s, so the turn did not complete — and claim 6's populated resume \
         answer would then be asserting a wire state no turn produced. pane:\n{}",
        sb.capture_pane()
    );
    println!(
        "CLAIM 4 PASS — a real TUI ran a real turn through the broker, and it \
         completed. The forward:\n{}",
        broker_log
            .lines()
            .filter(|l| l.contains("turn/start"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // --- 7. CLAIM 5: the wire hands a connection in the link's position no ----
    //         turn frames — COUNTED on a live observer, not inferred
    //
    // **Why counting, and not the store.** Turn frames are delivered only to the
    // connection SUBSCRIBED to the thread, and subscription is what a successful
    // `thread/resume` buys. This link never resumed successfully — claim 3 shows the
    // wire refusing it — so it should be handed no `turn/*` and no `item/*` frame at
    // all. Reading that off an unchanged fact set is an INFERENCE, and a broken
    // adapter that dropped every frame it was handed would satisfy it exactly as
    // well. So the primary evidence is a direct measurement: an observer sitting in
    // the link's own position (`initialize` + `initialized`, resumes nothing),
    // recording every method the wire hands it across the turn.
    //
    // The observer having seen `thread/started` is what makes the zero-counts mean
    // anything: without it, "zero turn frames" is equally consistent with a
    // connection that was never opened, never subscribed to the broadcast, or died.
    //
    // Nothing here asserts a "second turn shape". This run submits exactly one turn,
    // so what a second one would deliver to an unsubscribed connection has not been
    // measured, and an assertion about it would be a guess.
    //
    // **The probe runs FIRST, and it is a wire-level barrier — not merely a liveness
    // check.** The order matters in exactly one direction: a snapshot taken before the
    // probe misses every frame still in flight, and "in flight" is precisely where a
    // regression's `turn/*` frame would be, so the omission makes the zero-counts
    // falsely PASS. Awaiting the probe's response closes that gap twice over. On the
    // wire, a response to a request sent after the turn window cannot overtake
    // anything the server sent before it, so its arrival proves everything the server
    // meant to deliver has been delivered. In this process, the tap task reads that
    // one stream in order and pushes to `answered` only after pushing every method it
    // read earlier, so once the id shows up the method sink is complete up to it.
    // Only then is the snapshot taken, and only then are the counts run — which is
    // what makes "zero turn frames" a statement about the wire rather than about the
    // moment this test happened to look.
    //
    // It doubles as the liveness proof it always was: everything below is a statement
    // about what did not arrive, and every such statement is worthless unless the
    // thing that would have received it was still there. An answered census read says
    // the connection is open, the broker is still relaying for it, and the app-server
    // is still replying to it.
    let still_served = tap.probe(Duration::from_secs(30)).await;
    assert!(
        still_served,
        "after the turn, the observer's own connection did not answer a \
         ccd-allowlisted census read within 30s. Without that answer there is no \
         barrier: the counts below would be taken on a connection that was closed, \
         wedged, or no longer being relayed — which satisfies zero turn/* and zero \
         item/* trivially and proves nothing about the wire — and any frame still in \
         flight would go uncounted. This probe exists so that failure names itself \
         instead of arriving as a green claim 5."
    );
    // The snapshot, taken AFTER the barrier: every frame the wire had for this
    // connection is in the sink by now.
    let observed_all = observed(&tap.methods);
    let across_turn = &observed_all[observed_before_turn.min(observed_all.len())..];
    println!("OBSERVER, every method it was handed: {observed_all:?}");
    println!("OBSERVER, across the turn window: {across_turn:?}");
    assert!(
        observed_all.iter().any(|m| m == "thread/started"),
        "the observer never saw thread/started, so it was not live and correctly \
         positioned — and the zero-counts below would be counts of a connection that \
         is not receiving the broadcast at all, which is vacuous rather than \
         reassuring. Every method it did see: {observed_all:?}"
    );
    // **A positive count across the SAME window the zero-counts are taken on.**
    // `thread/started` is a fact about the past: it is broadcast before the turn, so
    // it proves the tap was alive EARLY and says nothing about whether it was still
    // being served when the turn ran. A connection that died the moment the prompt
    // was submitted satisfies "zero turn/*" perfectly.
    //
    // What a live, correctly-positioned, unsubscribed connection IS handed across a
    // turn is `thread/status/changed` — the thread going active, then idle. At least
    // one of them landing is what makes the zeros beside it mean "nothing was sent"
    // instead of "nobody was listening".
    //
    // **The count is 1 OR 2, and pinning it at 2 was wrong.** Both have now been seen
    // live on this same build. The turn's completion signal is claim 4's PANE
    // RENDERING of the reply, and the TUI renders on the agent message; the idle
    // status rides with `turn/completed`, slightly later, and reaches a
    // NON-subscribed connection asynchronously. So the window can legitimately close
    // after `active` and before `idle`. The probe barrier cannot close that gap and
    // is not meant to: it orders this connection's own stream, while the idle frame's
    // delay is in a fan-out this connection is not subscribed to — which is the very
    // fact claim 5 exists to prove. Pinning `== 2` encoded that delivery race as if
    // it were a property of the wire.
    //
    // So the two directions of the original intent are asserted separately, and
    // neither depends on the timing:
    //
    //   * ZERO stays a hard failure. That is the vacuous-pass guard and the whole
    //     reason this assertion exists.
    //   * "The fan-out moved" is caught by the DISTINCT methods rather than by the
    //     count. A fan-out change shows up as a new method arriving at a connection
    //     in this position, which is visible however many status frames beat the
    //     barrier.
    let status_across_turn = across_turn
        .iter()
        .filter(|m| m.as_str() == "thread/status/changed")
        .count();
    assert!(
        (1..=2).contains(&status_across_turn),
        "across the turn window the observer was handed {status_across_turn} \
         thread/status/changed frame(s), outside the one-or-two this wire delivers \
         (the thread going active, then idle). ZERO means the tap was not being \
         served while the turn ran, and every zero-count below is then a count of a \
         dead connection — a vacuous pass, not evidence. MORE than two means the \
         app-server's status fan-out moved. Do NOT re-tighten this to exactly two: \
         the turn window closes on the TUI rendering the reply, while the idle status \
         rides with turn/completed and reaches this unsubscribed connection \
         asynchronously, so 1 and 2 have both been observed live on this build and \
         pinning either one asserts a delivery race rather than the wire. Across the \
         turn: {across_turn:?} — everything: {observed_all:?}"
    );
    // **What the count can no longer say, said by the vocabulary instead.** A fan-out
    // that started handing this position something new is a change in WHICH methods
    // arrive, not in how many of one of them did — so it is detectable without
    // depending on which frames beat the barrier.
    let distinct_across_turn: std::collections::BTreeSet<&str> =
        across_turn.iter().map(String::as_str).collect();
    let measured_vocabulary: std::collections::BTreeSet<&str> =
        ["thread/status/changed"].into_iter().collect();
    assert_eq!(
        distinct_across_turn, measured_vocabulary,
        "across the turn window a merely-initialized ccd connection was handed a \
         different set of methods than the one this position was measured to be \
         delivered. A method appearing here that was not measured is what an \
         app-server fan-out change looks like, and every count below is then being \
         taken on a wire that is no longer the one it describes. Across the turn: \
         {across_turn:?} — everything: {observed_all:?}"
    );
    let turn_frames: Vec<&String> = observed_all
        .iter()
        .filter(|m| m.starts_with("turn/"))
        .collect();
    let item_frames: Vec<&String> = observed_all
        .iter()
        .filter(|m| m.starts_with("item/"))
        .collect();
    // **THIS IS THE TRIPWIRE FOR 2e-4b**, in its measured half: when reconciliation
    // lands and the link accepts the populated resume answer, it becomes SUBSCRIBED,
    // and a connection in that position IS handed turn/* and item/* frames. This
    // assertion going non-zero is that flip, and it is expected then — not a defect.
    assert!(
        turn_frames.is_empty() && item_frames.is_empty(),
        "TRIPWIRE (2e-4b half): a merely-initialized ccd connection — the exact \
         position the link occupies while its thread/resume keeps being refused — was \
         handed {} turn/* and {} item/* frame(s) across a turn it never subscribed \
         to. If 2e-4b has landed and the link now accepts the populated resume \
         answer, this is the EXPECTED flip: it is subscribed now, and this assertion \
         is the thing that was built to break here. Otherwise the app-server's \
         fan-out changed and the link's whole 'observes without subscribing' position \
         no longer holds. turn/*: {turn_frames:?} item/*: {item_frames:?} \
         everything: {observed_all:?}",
        turn_frames.len(),
        item_frames.len()
    );
    println!(
        "CLAIM 5a PASS — the observer answered a census read AFTER the turn window, \
         and the snapshot taken behind that barrier holds {} method(s) across the turn \
         ({status_across_turn} of them thread/status/changed, which is the whole of \
         this position's measured vocabulary), ZERO of them turn/* or item/*; the \
         zeros are a fact about the wire, not about a dead connection or about when \
         this test looked: {across_turn:?}",
        across_turn.len()
    );

    // **Corroboration of the count above, not the evidence for it.** The frames a
    // connection in this position does get (`thread/status/changed`) are ones
    // `codex_adapter.rs` deliberately maps to `Vec::new()` as observation noise, so
    // the link's fact set must be byte-for-byte unchanged across a turn it can see
    // happening. That it did not fabricate a timeline it was never shown is the
    // second half of the tripwire, and it flips with the first.
    let facts_after_turn = recorded(&daemon, &uid);
    print_events(&daemon, &uid, "after the turn ran");
    assert_eq!(
        facts_after_turn, facts_before_turn,
        "TRIPWIRE (2e-4b half), corroborating: the control link recorded facts across \
         a turn it is not subscribed to. Either 2e-4b has landed and the link now \
         accepts the populated resume answer (expected — retarget this assertion at \
         what it recovers), or the link normalized frames the measurement above says \
         it was never handed, which is fabrication. \
         before={facts_before_turn:?} after={facts_after_turn:?}"
    );
    println!(
        "CLAIM 5b PASS — corroborated: the link recorded nothing new ({} facts, \
         unchanged) across the turn the observer proves it was told nothing about",
        facts_after_turn.len()
    );

    // --- 8. CLAIM 6: the populated resume answer, and STOP-AND-AMEND ---------
    //
    // The broker's own account of the ccd leg first: `Ccd: forward` appears only
    // when a real client completed the WS-over-UDS handshake on `ccd.sock` AND a
    // request off that leg was classified `Forward` and relayed upstream.
    // Independent of anything this test observed from the client side.
    assert!(
        broker_log.contains("Ccd: forward"),
        "the broker must record the ccd leg forwarding this link's requests:\n{broker_log}"
    );
    println!("broker.log:\n{broker_log}");

    // Drop the link's connection.
    first.abort();
    let _ = first.await;
    println!(
        "link connection dropped ({} facts recorded)",
        facts_after_turn.len()
    );

    // **The wire fact 2e-4b is designed against, verbatim.** Post-turn the same
    // request that answered not-ready in claim 3 now answers with a `result` whose
    // `thread.turns` carries the turn that just ran. Printed whole, because a
    // summary of it is exactly what a reconciliation must not be built from — the
    // committed copy is `fixtures/codex/resume-populated-answer.json`.
    let post_turn_resume = raw
        .request(
            "thread/resume",
            serde_json::json!({"threadId": thread_id}),
            Duration::from_secs(60),
        )
        .await;
    println!("RAW post-turn thread/resume answer: {post_turn_resume}");
    assert_ne!(
        post_turn_resume["error"]["code"].as_i64(),
        Some(-32001),
        "the BROKER refused the post-turn resume, so its session binding lost the \
         thread it had already admitted a resume for: {post_turn_resume}"
    );
    assert!(
        !crate::codex_link::is_measured_not_ready(&post_turn_resume, &thread_id),
        "after a turn ran, thread/resume still answered with the not-ready error. \
         The rollout a completed turn is supposed to create does not exist, so 2e-4b \
         has nothing to reconcile against and this gate's evidence is not what it \
         claims: {post_turn_resume}"
    );
    let turns = post_turn_resume["result"]["thread"]["turns"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "the post-turn resume must carry result.thread.turns[]; without it \
                 there is no recovered history for 2e-4b to reconcile, whatever else \
                 the answer says: {post_turn_resume}"
            )
        });
    assert!(
        !turns.is_empty(),
        "result.thread.turns[] came back EMPTY after a turn completed. An empty \
         array has never been observed on this wire, and a reconciliation designed \
         against it would be designed against nothing: {post_turn_resume}"
    );
    // **Exactly one, because exactly one was submitted.** `>= 1` is satisfied by an
    // answer carrying somebody else's history, or by a `turns[]` the app-server pads;
    // the gate typed one prompt into the pane and the reconciliation 2e-4b is being
    // designed from must be designed from an answer whose turn count is known, not
    // merely non-zero.
    assert_eq!(
        turns.len(),
        1,
        "this run submitted ONE turn and result.thread.turns[] came back with {}. \
         Either the pane ran work this gate did not ask for — in which case the \
         redaction and shape claims below are about a session whose content is not \
         accounted for — or the app-server's turns[] is not the per-turn array 2e-4b \
         will be written against: {post_turn_resume}",
        turns.len()
    );

    // **The live answer, against the committed ground truth.** `!turns.is_empty()`
    // is satisfied by an array of anything; what 2e-4b will be designed against is
    // the answer's SUBSTANCE, so the substance is what is checked — field for field
    // against `fixtures/codex/resume-populated-answer.json`, the copy an earlier run
    // of this very gate captured.
    //
    // Only the structural, non-content fields are compared. The prompt, the reply,
    // the `cwd` and the rollout `path` legitimately differ every run and are exactly
    // what must NOT be pinned; pinning them would make this gate fail on a fixture's
    // staleness rather than on the wire moving.
    let fixture: Value =
        serde_json::from_str(POPULATED_RESUME_FIXTURE).expect("the committed answer is JSON");
    // Not "some id": the thread the LINK bound off the live stream in claim 2. An
    // answer about a different thread would satisfy every shape assertion here while
    // describing a session this gate never ran a turn on.
    assert_eq!(
        post_turn_resume["result"]["thread"]["id"].as_str(),
        Some(thread_id.as_str()),
        "the post-turn resume answered about a different thread than the one the link \
         bound ({thread_id}). Everything below reads this answer as the recovered \
         history of THIS session's turn: {post_turn_resume}"
    );
    // **The fixture, pinned to its literal values.** Every assertion below that reads
    // `fixture[...]` is only as good as the committed file: an edit that quietly
    // relaxed the ground truth would relax the live comparison with it and nothing
    // would fail. So the two fields that carry the fixture's whole claim about turn
    // shape are spelled out here, and a silent edit to `resume-populated-answer.json`
    // breaks this before it can weaken anything else.
    let fixture_turns = fixture["result"]["thread"]["turns"]
        .as_array()
        .expect("the committed answer carries result.thread.turns[]");
    assert_eq!(
        fixture_turns.len(),
        1,
        "the committed ground truth no longer carries exactly one turn; the live \
         comparisons below are calibrated against a one-turn answer"
    );
    assert_eq!(
        fixture_turns[0]["id"].as_str(),
        Some("01a03652-fe8e-79d2-99f8-2d5e445e6d8d"),
        "the committed ground truth's turn id changed. This is the FIXTURE's own id, \
         captured off this wire and pinned so the file cannot be edited without the \
         gate saying so — it is deliberately not compared to the live answer's, \
         because a live run mints a fresh turn id every time and an assertion that \
         the two are equal could only ever pass by accident. The live half is \
         asserted structurally below."
    );
    assert_eq!(
        fixture_turns[0]["status"].as_str(),
        Some("completed"),
        "the committed ground truth's turn status is no longer \"completed\", so the \
         live comparison below has stopped meaning \"the turn finished\""
    );
    assert_eq!(
        turns[0]["status"].as_str(),
        fixture["result"]["thread"]["turns"][0]["status"].as_str(),
        "the recovered turn is not in the state the ground truth records \
         ({:?} vs the fixture's {:?}). A turn that resumes as anything but completed \
         is a different reconciliation problem than the one 2e-4b was scoped from, \
         and this gate would be handing it the wrong evidence: {post_turn_resume}",
        turns[0]["status"],
        fixture["result"]["thread"]["turns"][0]["status"]
    );
    // **The live turn id: structural, and honestly so.** The fixture's id is pinned
    // above; this one cannot be, because the app-server mints it fresh for every run.
    // What IS checkable is that the live answer names its turn the way the fixture
    // names its own — a nonempty id, distinct from the thread's, occurring in the
    // serialized answer exactly as many times as the fixture's does (once: `turns[]`
    // is where a turn declares itself; the backwards cursors key on the THREAD id).
    // The expected occurrence count is derived from the fixture rather than written
    // as a literal, so recapturing the fixture recalibrates it.
    let live_turn_id = turns[0]["id"].as_str().unwrap_or_default();
    assert!(
        !live_turn_id.is_empty(),
        "the recovered turn carries no id. 2e-4b has to key recovered terminals by \
         turn, and an answer that does not name its turn cannot be reconciled at \
         all: {post_turn_resume}"
    );
    assert_ne!(
        live_turn_id, thread_id,
        "the recovered turn's id is the THREAD's id ({thread_id}). A reconciliation \
         keyed on that would collapse every turn in the thread onto one key: \
         {post_turn_resume}"
    );
    let fixture_turn_mentions = POPULATED_RESUME_FIXTURE
        .matches("01a03652-fe8e-79d2-99f8-2d5e445e6d8d")
        .count();
    let live_turn_mentions = post_turn_resume.to_string().matches(live_turn_id).count();
    assert_eq!(
        live_turn_mentions, fixture_turn_mentions,
        "the live answer mentions its turn id {live_turn_mentions} time(s) where the \
         committed ground truth mentions its own {fixture_turn_mentions} time(s). The \
         turn id is where a turn declares itself, so a different number of mentions \
         means the answer's keying moved — either the id now appears in a cursor or \
         an item where it did not, or `turns[0].id` is a copy of something else: \
         {post_turn_resume}"
    );
    // **M9: an expectation computed INDEPENDENTLY of the answer.** Reading `cwd` out
    // of the frame and comparing it to the frame is a tautology, and comparing it to
    // the fixture's would be worse — the fixture's `cwd` is per-run content this gate
    // deliberately does not pin. But the test KNOWS what it launched: the coordinator
    // above was given `--cwd /tmp`, which it canonicalizes once (its own
    // `canonical_launch_cwd`) before handing it to the host as `--launch-cwd`, and the
    // broker's session binding then requires the created thread's cwd to equal it. So
    // the expectation is `/tmp` canonicalized HERE — `/private/tmp` on macOS — and it
    // is derived from the launch, not from the answer.
    let expected_cwd = std::fs::canonicalize("/tmp").expect("/tmp resolves");
    assert_eq!(
        post_turn_resume["result"]["cwd"].as_str(),
        expected_cwd.to_str(),
        "the resumed thread reports cwd={:?}, but this gate launched the coordinator \
         with `--cwd /tmp`, which canonicalizes to {}. That value is the workspace \
         anchor the broker verifies a thread creation against, so a divergence means \
         the session ran somewhere other than where it was launched — and the sandbox \
         and workspace claims beside it are about a workspace nobody asserted: \
         {post_turn_resume}",
        post_turn_resume["result"]["cwd"],
        expected_cwd.display()
    );
    // The roots are checked by TYPE, not by value: what a live run's workspace roots
    // resolve to is not something this gate launched, so pinning them would pin an
    // accident. What must hold is that the field is the nonempty array of nonempty
    // paths 2e-4b will read it as.
    let roots = post_turn_resume["result"]["runtimeWorkspaceRoots"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "result.runtimeWorkspaceRoots is not an array. It is one of the fields \
                 2e-4b reads a recovered session's workspace from: {post_turn_resume}"
            )
        });
    assert!(
        !roots.is_empty()
            && roots
                .iter()
                .all(|r| r.as_str().is_some_and(|s| !s.is_empty())),
        "result.runtimeWorkspaceRoots must be a nonempty array of nonempty strings; a \
         session with no workspace root, or a root that is not a path, is not \
         something a reconciliation can anchor to: {roots:?}"
    );
    // **The launch fingerprint governed the turn that actually ran.** This is the
    // assertion that closes the loop opened by claim 4: the coordinator launched with
    // `--sandbox read-only`, the broker forwarded the turn only by discharging the
    // TUI's sandbox deferral to the bound thread's own policy — and here is that
    // thread's policy, read back off the wire after the turn, still read-only with no
    // network. If the deferral ever resolved to something laxer, the forward note
    // could stay the same while THIS moved.
    assert_eq!(
        post_turn_resume["result"]["sandbox"], fixture["result"]["sandbox"],
        "the effective sandbox the resumed thread reports is not the read-only, \
         no-network policy the launch fingerprint pinned. The turn/start forward \
         discharges a sandbox DEFERRAL to this thread's own policy, so this value is \
         what the deferral resolved to — and a divergence here means the turn ran \
         under a policy nobody asserted. live={} fixture={}",
        post_turn_resume["result"]["sandbox"], fixture["result"]["sandbox"]
    );
    for field in ["approvalPolicy", "approvalsReviewer"] {
        assert_eq!(
            post_turn_resume["result"][field], fixture["result"][field],
            "the resumed thread reports {field}={} where the ground truth records {}. \
             The launcher pins this value and the broker's fingerprint refuses a \
             thread/start that diverges from it, so a change here is either the \
             launcher's pin or the app-server's reporting of it moving underneath \
             this gate: {post_turn_resume}",
            post_turn_resume["result"][field], fixture["result"][field]
        );
    }
    println!(
        "CLAIM 6a PASS — the post-turn resume answers with a populated turns[] \
         (exactly {} turn, id {live_turn_id}, completed) whose thread id, turn \
         status, effective sandbox, approval policy and reviewer all agree with the \
         committed ground truth, whose cwd is the independently canonicalized launch \
         cwd {} and whose workspace roots are {roots:?}; this is the answer \
         codex_link refuses to guess at",
        turns.len(),
        expected_cwd.display()
    );

    // A fresh link re-attaches with the thread id, exactly as a daemon coming back
    // after a restart would — and now meets that populated answer.
    let lines_before = read_file(&sb.run_dir.join("broker.log")).lines().count();
    // **Start capturing what production LOGS, before it logs it.** `crate::log::emit`
    // writes to stderr, which the harness does not capture, so until now the only
    // thing this gate could assert about the STOP-AND-AMEND report was a copy it
    // rebuilt itself — and a rebuilt line proves the builder, not the daemon. The
    // capture sink mirrors the real formatted line at the point it is written, so the
    // redaction claim below is about the bytes that actually reached the log.
    //
    // The sink is process-global (see `log::capture`), so the line is selected by
    // content rather than by position.
    crate::log::capture::install();
    let second = tokio::spawn(crate::codex_link::run(
        Arc::clone(&daemon),
        session.clone(),
        ControlLink {
            socket: sb.ccd_sock(),
            generation: 1,
            thread_id: Some(thread_id.clone()),
        },
    ));
    // **Wait for the CONDITION, not for a clock.** `codex_link`'s reconnect backoff
    // is RECONNECT_BACKOFF_MIN = 250 ms doubling to RECONNECT_BACKOFF_MAX = 15 s, and
    // a connection that ends this fast never earns the reset — so on this machine two
    // complete cycles arrive in well under a second. But a fixed sleep sized to that
    // silently UNDER-OBSERVES on a loaded or slow one: the window closes with one
    // cycle visible and the discriminator below reads it as "the link stayed on one
    // connection", which is the opposite conclusion. So the window is polled to the
    // fact it exists to establish, and a timeout FAILS naming what it waited for.
    //
    // Polled to the SAME condition the assertions below read — two connections that
    // walked the whole lifecycle, ending included — so the window cannot close on a
    // fact the assertions then have to relax to accept.
    let cycled = wait_until(Duration::from_secs(60), || {
        let log = read_file(&sb.run_dir.join("broker.log"));
        ccd_connections(&ccd_tail(&log, lines_before))
            .iter()
            .filter(|c| c.complete())
            .count()
            >= 2
    })
    .await;
    // A short settle so the log holds whatever landed alongside the second cycle —
    // the disconnect line, the next handshake — rather than truncating mid-cycle.
    tokio::time::sleep(Duration::from_secs(2)).await;
    // **Take the log lines the daemon emitted across the window that just closed.**
    // Drained after the settle for the same reason the broker log is re-read after
    // it: a drain taken earlier is a snapshot of a window that had not finished.
    let captured = crate::log::capture::drain();
    crate::log::capture::uninstall();
    print_events(&daemon, &uid, "after the reconnect");

    // **Read the log AFTER the window closes.** A read taken before the poll would be
    // a stale snapshot, and undercounting here reads as the retry shape rather than
    // as a stale read.
    let broker_log = read_file(&sb.run_dir.join("broker.log"));
    println!("broker.log after the reconnect:\n{broker_log}");
    let tail = ccd_tail(&broker_log, lines_before);
    println!("ccd leg, after the drop: {tail:#?}");
    assert!(
        cycled,
        "60s after a fresh link was pointed at a thread whose resume answers with a \
         populated result, fewer than TWO connections had walked the ccd leg's whole \
         lifecycle (leg opened, initialize, initialized, thread/resume, ended). Either \
         the link never re-attached at all, or it is cycling far slower than its own \
         backoff allows — and every count below would be taken on a window that never \
         opened: {tail:#?}"
    );

    // **The specific sequence the reconnect must produce**, read off the broker's
    // own log — not a bare count, which a repeated `initialize` or three unrelated
    // reads would satisfy just as well. On the ccd leg the three dispositions are
    // distinguishable: `initialize` and the census reads are "request allowlisted",
    // `initialized` is "notification allowlisted", and `thread/resume` is the only
    // thing ccd may send that is an "ownership request", so it is the one line that
    // cannot be mistaken for anything else.
    let position = |needle: &str| tail.iter().position(|l| l.contains(needle));
    let count = |needle: &str| tail.iter().filter(|l| l.contains(needle)).count();
    let init = position(CCD_INITIALIZE_NOTE)
        .unwrap_or_else(|| panic!("the re-attached link must send initialize: {tail:#?}"));
    let initialized =
        position(CCD_INITIALIZED_NOTE).unwrap_or_else(|| panic!("...then initialized: {tail:#?}"));
    let resumed =
        position(CCD_RESUME_NOTE).unwrap_or_else(|| panic!("...then thread/resume: {tail:#?}"));
    assert!(
        init < initialized && initialized < resumed,
        "the reconnect must be initialize -> initialized -> thread/resume, in that \
         order; nothing may be pipelined ahead of the attach (A2): {tail:#?}"
    );

    // **The STOP-AND-AMEND discriminator, by CONNECTION IDENTITY rather than by
    // counts.** Two outcomes look alike from the store's side — both record nothing —
    // but they are opposite facts about the code. A retryable not-ready answer keeps
    // the SAME connection and re-asks on it: one `initialize`, N resumes. A refused
    // answer ends the connection (`serve_connection` bails), so `run()` reconnects and
    // re-handshakes: every resume rides a connection of its own, which then closes.
    //
    // Counts alone cannot tell those apart — "N initializes and N resumes" is also
    // what you get from N initializes in a burst followed by N resumes on the last
    // connection. Neither can a disconnect line "somewhere in the log": the first
    // link's connection, aborted a few lines above, supplies one for free.
    //
    // So the lines are GROUPED by the `(conn N)` the broker stamped on each of them
    // and every connection is walked through its whole lifecycle in order — leg
    // opened, initialize, initialized, thread/resume, and its OWN ending. That makes
    // "every resume rode a connection of its own, and that connection is the one that
    // then ended" a fact read off the broker's connection identity rather than
    // inferred from how the lines happened to interleave.
    let conns = ccd_connections(&tail);
    let initializes = count(CCD_INITIALIZE_NOTE);
    let resumes = count(CCD_RESUME_NOTE);
    println!(
        "ccd leg after the drop: {initializes} initialize(s), {resumes} resume(s), \
         grouped by the connection the broker stamped them with:"
    );
    for conn in &conns {
        println!(
            "  conn {}: reached {} ({}/{}){}",
            conn.id,
            conn.reached(),
            conn.stage,
            CCD_LIFECYCLE.len(),
            match conn.disordered.as_slice() {
                [] => String::new(),
                lines => format!("  OUT OF ORDER: {lines:#?}"),
            }
        );
    }
    // The exempt in-flight connection is named in the failure text below rather than
    // silently skipped; see [`ccd_window_verdict`] for why exactly one may exist.
    let (complete, in_flight, partial) = ccd_window_verdict(&conns);
    assert!(
        complete.len() >= 2,
        "the re-attached link met the populated resume answer and must have taken the \
         STOP-AND-AMEND branch: that ENDS the connection, so every retry rides a NEW \
         one — leg opened, initialize, initialized, thread/resume, ended, again. Only \
         {} of the {} connection(s) the broker opened in this window walked that whole \
         lifecycle in order (complete: {complete:?}; {initializes} initialize(s) and \
         {resumes} resume(s) in total), so the refuse-and-reconnect loop this claim is \
         about did not happen: {tail:#?}",
        complete.len(),
        conns.len()
    );
    assert!(
        partial.is_empty(),
        "{} connection(s) opened in this window never walked the ccd leg's lifecycle \
         through, and every shape that produces one is a shape this claim excludes: a \
         connection that resumed twice without reconnecting (the retry-on-the-same-\
         connection shape a RETRYABLE answer produces — it would mean the link treated \
         a populated result as routine instead of refusing it, the guess codex_link \
         exists not to make), one that pipelined its resume ahead of the handshake \
         (A2), or one that asked and then never ended on the record, which leaves the \
         next initialize indistinguishable from a second client's. Counts would have \
         hidden all three: {initializes} initialize(s) against {resumes} resume(s) \
         looks paired.{} unfinished: {partial:#?}\nthe whole leg: {tail:#?}",
        partial.len(),
        match in_flight {
            Some(id) => format!(
                " (conn {id} is exempt and not counted here: it opened last and has no \
                 ending yet, so it was still in flight when the log was read.)"
            ),
            None => String::new(),
        }
    );
    // The disconnects themselves must be visible on the leg: without them, "N
    // initializes" could be some other client's traffic rather than this link
    // cycling.
    assert!(
        broker_log.contains("Ccd: read error") || broker_log.contains("Ccd leg ended"),
        "the dropped connections must appear as real disconnects on the ccd leg; a \
         stop-and-amend that ended no connection did not happen:\n{broker_log}"
    );

    // **The line the daemon ACTUALLY logged — not one this test rebuilt.**
    //
    // The rebuilt copy below is kept as corroboration, but it can only ever prove the
    // rebuilder: `stop_and_amend_report` called here with arguments this test chose
    // says nothing about what `settle_resume` chose to pass it. A redaction that held
    // in the constructor and leaked at the call site would pass a rebuilt assertion
    // and write the session's content to the log anyway. So the primary evidence is
    // the real formatted line, taken from the capture sink at the point `log::emit`
    // wrote it, across the very reconnect window measured above.
    //
    // Selected by CONTENT — the sink is process-global — and the selection is itself
    // an assertion: no line means the branch this whole claim is about never logged.
    //
    // **EVERY report, not the first one found.** Taking one line and auditing it is a
    // claim about that line; the property the redaction has to hold is a property of
    // the branch, which recurs on every reconnect for as long as the condition lasts.
    // A leak that appears only in the report carrying a suppressed count, or only in
    // the second cycle's, would slip past a `find` and land in the daemon's log
    // exactly as often as the first one would not.
    println!(
        "captured {} daemon log line(s) across the window",
        captured.len()
    );
    let reports: Vec<&String> = captured
        .iter()
        .filter(|l| l.contains("STOP-AND-AMEND") && l.contains(&thread_id))
        .collect();
    assert!(
        !reports.is_empty(),
        "the daemon emitted no STOP-AND-AMEND line for {thread_id} across a window in \
         which {} connection(s) completed the whole refuse-and-reconnect lifecycle. The \
         branch is what this claim is about, and a silent refusal leaves an operator \
         with a link that reconnects for ever and no reason why.\ncaptured:\n{}",
        complete.len(),
        captured.join("\n")
    );
    for (n, report) in reports.iter().enumerate() {
        println!(
            "the STOP-AND-AMEND line the daemon LOGGED ({} of {}):\n{report}",
            n + 1,
            reports.len()
        );
    }
    // **The throttle, asserted rather than described.** `AMEND_REPORT_QUIET` is 300s
    // and the throttle is keyed on (thread, described answer), so one report must
    // cover this whole reconnect window however many cycles it contains — that is the
    // throttle's entire purpose, and the reason it exists is that the unthrottled
    // branch wrote six copies of this report in fifteen seconds and would have kept
    // going for the life of the daemon. Left unasserted, that regression comes back
    // silently.
    assert_eq!(
        reports.len(),
        1,
        "codex_link's STOP-AND-AMEND THROTTLE stopped working: {} report(s) reached the \
         log across {} refuse-and-reconnect cycle(s) on one thread and one unchanged \
         answer, where AMEND_REPORT_QUIET (300s, keyed on thread + described answer) \
         must collapse them to exactly one carrying the suppressed count. A permanent \
         condition is supposed to stay visible without becoming the thing that fills \
         the log.\nreports:\n{}",
        reports.len(),
        complete.len(),
        reports
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );

    // The rebuilt line, from the populated answer the wire just produced. Kept
    // because it pins the *frame* — `codex_link`'s unit test only ever sees the
    // committed fixture, while this one carries THIS run's prompt, reply, rollout
    // path and cwd — and because a divergence between it and the captured line is
    // itself worth seeing in the transcript.
    let digest = crate::codex_link::frame_digest(&post_turn_resume);
    let live_report = crate::codex_link::stop_and_amend_report(
        &session.name,
        &thread_id,
        &crate::codex_link::describe_resume_answer(&post_turn_resume, &digest),
        0,
    );
    println!("the STOP-AND-AMEND line this answer produces:\n{live_report}");
    // Every report the daemon wrote, plus the rebuilt one. The redaction is a
    // property of the branch, so it is checked on each line the branch produced —
    // nothing here reads a digest, because entropy failure renders `digest
    // unavailable` and requiring a value would encode a property this build does not
    // guarantee.
    let audited: Vec<(String, &str)> = reports
        .iter()
        .enumerate()
        .map(|(n, r)| {
            (
                format!("the daemon logged ({} of {})", n + 1, reports.len()),
                r.as_str(),
            )
        })
        .chain(std::iter::once((
            "rebuilt here".to_string(),
            live_report.as_str(),
        )))
        .collect();
    for (whose, report) in &audited {
        assert!(
            report.contains(&thread_id),
            "the report {whose} does not name the thread it could not read an answer \
             for, so an operator cannot act on it:\n{report}"
        );
        // `turns={n}` — the separator is part of `describe_resume_answer`'s rendering
        // and is spelled here exactly as that function spells it. Matching on the
        // number alone would be satisfied by the digest or the "other top-level keys"
        // count happening to contain it.
        assert!(
            report.contains(&format!("turns={}", turns.len())),
            "the report {whose} does not carry `turns={}` — the fact that says WHY the \
             branch fired at all:\n{report}",
            turns.len()
        );
        // **No frame dump, in the one spelling that would reintroduce it.** The
        // redaction replaced a `Frame: {frame}` in this branch; this is the tripwire
        // for it coming back.
        assert!(
            !report.contains("Frame:"),
            "the report {whose} carries a `Frame:` dump. That is the exact leak the \
             described-rather-than-quoted rendering replaced, and this branch recurs \
             on every reconnect for as long as the condition lasts:\n{report}"
        );
        // Result key names are peer-supplied text drawn from no fixed vocabulary, so
        // printing them is printing the frame. `describe_resume_answer` names only the
        // five in its own constant and COUNTS the rest; these three are among the rest
        // and must never appear.
        for key in [
            "itemsBackwardsCursor",
            "instructionSources",
            "activePermissionProfile",
        ] {
            assert!(
                !report.contains(key),
                "the report {whose} names the result key {key:?}. Only the keys spelled \
                 in codex_link's own DESCRIBED_RESULT_KEYS may reach the log; the rest \
                 are counted, because a key name is unbounded peer-supplied \
                 text:\n{report}"
            );
        }
        // Everything the frame carries that is the session's own content. Each is read
        // out of the LIVE answer rather than assumed, so this catches a redaction that
        // holds for the fixture's values and leaks for another run's.
        let secrets = [
            (
                "the prompt",
                post_turn_resume["result"]["thread"]["preview"].as_str(),
            ),
            (
                "the reply",
                post_turn_resume["result"]["thread"]["turns"][0]["items"]
                    .as_array()
                    .and_then(|items| items.iter().find(|i| i["type"] == "agentMessage"))
                    .and_then(|item| item["text"].as_str()),
            ),
            (
                "the rollout path",
                post_turn_resume["result"]["thread"]["path"].as_str(),
            ),
            ("the cwd", post_turn_resume["result"]["cwd"].as_str()),
        ];
        for (what, value) in secrets {
            let Some(value) = value.filter(|v| !v.is_empty()) else {
                continue;
            };
            assert!(
                !report.contains(value),
                "the STOP-AND-AMEND report {whose} leaked {what} ({value:?}) out of the \
                 live resume answer. This branch recurs on every reconnect for as long \
                 as the condition lasts, so a leak here is the session's content \
                 written to the daemon's log for ever:\n{report}"
            );
        }
    }
    println!(
        "CLAIM 6b PASS — STOP-AND-AMEND fired: {} connection(s) {complete:?} each walked \
         leg opened -> initialize -> initialized -> thread/resume -> ended in order \
         under their own broker connection id, with no partial one left over{}; and \
         every one of the {} report(s) the daemon ACTUALLY logged — exactly one, as \
         the throttle requires — names the thread and the turn count while carrying no \
         frame dump, no result key name and none of the answer's content. A \
         reconnect-and-keep-observing loop is the DESIGNED behaviour until 2e-4b \
         teaches the link to read the populated answer — not a defect.",
        complete.len(),
        match in_flight {
            Some(id) => format!(" (conn {id} still in flight)"),
            None => String::new(),
        },
        reports.len()
    );

    // Nothing recorded before all this was written again: a re-append would mint a
    // new `seq`, and the dedup key is what stops it.
    //
    // **Labelled honestly: this is a negative, not a positive.** The re-attached
    // link never sees a `thread/started` — a reconnect to a running thread does not
    // get one — so it stays unbound, and it refuses the resume answer that could
    // have told it anything, so it records nothing at all. What this proves is the
    // thing that matters here: neither the turn nor the reconnect re-appended the
    // facts the FIRST link had already made durable. The positive form — an attach
    // that succeeds, a replay over it, and the pre-drop keys still holding their
    // original `seq` — needs a resume answer the link ACCEPTS, which is the 2e-4b
    // obligation recorded in the module doc.
    let after = recorded(&daemon, &uid);
    for (key, seq) in &facts_before_turn {
        let now = after.iter().find(|(k, _)| k == key);
        assert_eq!(
            now.map(|(_, s)| *s),
            Some(*seq),
            "{key} was re-appended across the turn and the reconnect (seq moved)"
        );
    }
    let mut keys = std::collections::HashSet::new();
    for (key, _) in &after {
        assert!(!key.is_empty(), "every Codex fact carries a dedup key");
        assert!(
            keys.insert(key.clone()),
            "duplicate fact {key} after resume"
        );
    }
    println!(
        "CLAIM 6 PASS — a turn ran, the resume came back populated, the link refused \
         to guess and kept reconnecting; {} facts before the turn, {} after \
         everything, none duplicated",
        facts_before_turn.len(),
        after.len()
    );

    second.abort();
    tap.handle.abort();
    // Lose the coordinator: the retained custodian owns the teardown, which is the
    // same path as a coordinator that died mid-launch.
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    println!("PASS the_control_link_observes_a_real_codex_session_and_reattaches_by_resume");
}
