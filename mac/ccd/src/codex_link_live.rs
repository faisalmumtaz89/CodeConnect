//! GATED live integration for the **ccd control link** (Phase 2e-3), against a
//! real codex 0.147: a real coordinator, a real host, a real broker, a real
//! app-server and a real `codex --remote` TUI in a real tmux pane.
//!
//! `codex_link.rs`'s scripted test proves the state machine deterministically.
//! This proves the thing that actually ships — that the machine speaks the wire the
//! broker's ccd leg really answers with. Eight claims, in order:
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
//!      and answers with the measured not-ready error — the first of the two answers
//!      [`crate::codex_link`] accepts, pinned here at the last point in the run where
//!      it is still the only one available.
//!   4. **A real turn runs through the broker, on the deferral-discharging path.**
//!      A prompt is typed into the pane and submitted; the broker forwards the
//!      `turn/start` it produces under the **exact** note that says the sandbox
//!      deferral was discharged by the verified thread binding, and the TUI renders
//!      the reply.
//!   5. **A connection that has not resumed is handed no turn frames (5a) — while the
//!      link, which DOES resume, ends up subscribed (5b).** A raw observer that
//!      completed `initialize` + `initialized` and resumed nothing records every
//!      server→client `method` it receives across the turn: zero `turn/*`, zero
//!      `item/*`, and `thread/status/changed` as the whole of what that position is
//!      handed, behind a post-turn round trip on its own connection acting as a wire
//!      barrier. The link is in a different position and 5b is where that shows: it
//!      bound from the broadcast and then kept asking, met the not-ready error until
//!      the turn created the rollout, and attached — on the connection it has held
//!      since before the TUI existed.
//!   6. **The populated answer agrees with the committed ground truth (6a), and the
//!      link ACCEPTED it (6b).** The raw resume returns a result whose effective
//!      policy, turn shape and thread identity match
//!      `fixtures/codex/resume-populated-answer.json` field for field on everything
//!      that is not per-run content, and whose `cwd` equals the launch cwd this test
//!      canonicalizes **independently** of the answer. The link's own ccd connection
//!      reached `thread/resume` and never ended — the inverse of the refuse loop — with
//!      no refusal in the daemon's real log sink.
//!   7. **The attached link observes the next turn LIVE.** A second real turn runs and
//!      lands as facts of its own turn — including the token usage, which exists only
//!      on the notification wire and therefore cannot have been recovered.
//!   7b. **That turn rings, and the link says what it is** (2e-5). The terminal claim 7
//!      just proved live is the only one in this run that was WATCHED arriving — turn 1
//!      came back through a resume answer, and an answer never rings — so the doorbell
//!      here is that turn's. It carries `Completed`, the Codex agent (what narrows the
//!      fan-out to phones that can open such a run) and this run's uid, and nothing the
//!      agent wrote. Beside it, the same connection publishes
//!      `Subscribed{thread}` — the state the inbound resolver reads, on the link that
//!      has been open since before the TUI existed. **Bound is not subscribed**, and
//!      only the latter is an addressee.
//!   8. **Two turns, re-described, still one set of facts.** The answer now describes
//!      both turns under the ids this gate watched them run with; a fresh link — the
//!      one place a restart happens, and deliberately the *daemon-restart* question
//!      rather than the milestone — accepts it and re-derives every fact already in the
//!      store except the usage totals, writing none of them twice.
//!
//! # What this gate is the successor to
//!
//! Two assertions here used to say the opposite, and both were built to be flipped
//! exactly here.
//!
//! The first was claim 4's: that the broker **refused** the TUI's `turn/start`. Two
//! broker changes retired it — `fingerprint.rs` reads the real TUI's explicit
//! `"sandboxPolicy": null` as a **deferral** to the named thread's own policy rather
//! than an unprovable claim, and `refusal.rs`'s `FingerprintThenHeadCheck` discharges
//! that deferral by forwarding a `turn/start` only when its `threadId` is the session's
//! one bound thread. So claim 4 asserts **that exact note**, with the old refusal
//! asserted **negatively** beside it so a regression names itself instead of quietly
//! reading as "no turn was attempted".
//!
//! The second was claim 6b's: that the link met the populated answer, refused to guess
//! at it, and reconnected for ever — a loop this gate asserted by pairing initializes to
//! resumes on the broker's own connection identity. That refusal was correct while
//! nothing had measured the answer's completeness, its keying, or D15's id stability.
//! Those measurements now exist, they are written down on
//! [`crate::codex_adapter::CodexAdapter::plan_resume_seed`], and one of them changed the
//! design: a **running** turn's `items[]` carry placeholder ids (`item-1`, `item-2`)
//! while the live wire emits the real ones, so a resume answer may be read for a turn
//! that has **finished** and may only be read for liveness otherwise. Claims 6b, 7 and 8
//! are what the old loop became.
//!
//! **Claim 5 split rather than flipped, and the split is the third thing that moved.**
//! Until 2e-4b a `thread/started` *discharged* the outstanding attach: a connection that
//! watched the thread start was taken to hold its whole stream already, so resuming
//! would only ask for a replay of what it had. Against the measured wire that reasoning
//! is exactly backwards — turn frames reach only the **resume-subscribed** connection,
//! so an announced, never-resumed link is handed the thread's identity and then nothing
//! else for the life of the session. Recovery was never what the resume bought.
//! Subscription is. So 5a still counts what an unresumed connection is handed (nothing),
//! and 5b now asserts that the link is not in that position: it binds, it keeps asking,
//! and it attaches. **Bound is not subscribed.**
//!
//! An earlier draft of this gate hid that defect by aborting the link and pointing a
//! fresh one at the thread id — which made acceptance true of a link that had just been
//! handed its target, and said nothing about the one that learned it from the broadcast.
//! The milestone is now asserted on the connection that was never restarted.
//!
//! # What this does NOT prove, stated exactly
//!
//! **The failure paths of the attach are scripted, not live.** A resume answer this
//! build refuses, and an attach whose facts cannot be written, are both asserted
//! deterministically in `codex_link`'s own suite (`every_answer_outside_the_two_accepted_shapes_fails_closed`,
//! `an_ingest_failure_fails_the_attach_rather_than_attaching_anyway`) — a live
//! app-server offers no lever to produce either on demand without also destroying what
//! is under test.
//!
//! **An interrupted turn is not exercised here or anywhere.** No resume answer has ever
//! been captured describing one, so [`crate::codex_adapter::CodexAdapter::plan_resume_seed`]
//! refuses any turn state other than the two measured, and an operator who interrupts a
//! turn gets a loud STOP-AND-AMEND rather than a guess. That refusal is the designed
//! re-grounding trigger for the chunk that captures it.
//!
//! **The `run` loop's own reconnect** — EOF, backoff, re-handshake and the retryable
//! not-ready attach — is proven deterministically in
//! `codex_link::tests::the_link_binds_reconnects_and_retries_the_measured_attach`,
//! because a live broker offers no lever to drop one leg on demand without also killing
//! what is under test. Claims 6b and 8 drop the connection by **ending the link task**,
//! which is the daemon-restart shape (a new `run` starting from a known thread id), not
//! a mid-`run` EOF.
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
    // Round-1 M13: build it before looking for it.
    build_launcher();
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

/// **Build the launcher, then use it** (round-1 M13).
///
/// Found the hard way, in 2e-4c, while mutation-testing a live gate: `cargo test -p ccd`
/// does not rebuild `codeconnect`, and this harness drives `codeconnect` — which is what
/// carries the broker into the pane. A deliberate, load-bearing mutation of
/// `codex-broker/src/fingerprint.rs` was therefore invisible to the live run, and the gate
/// PASSED against a binary built before the mutation existed. A live gate that can pass
/// against a stale binary is not a gate; it will just as happily pass against a broken
/// change nobody rebuilt.
///
/// The first fix here was an mtime scan over the workspace sources, which only DETECTED
/// staleness and then failed. Building is the complete fix: the gate cannot run against
/// anything but the current tree, and there is no third state where the binary is stale
/// and the scan happens to agree with it (a touched file, a clock skew, a
/// `--offline` edit). `cargo build` is a no-op when nothing changed, so the cost on the
/// common path is a lock acquisition.
///
/// It inherits this process's `CARGO_TARGET_DIR`, so the binary it produces is the one
/// [`resolve_codeconnect`] then finds beside this test binary.
fn build_launcher() {
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "-p", "codeconnect"])
        // The workspace root, derived from this crate rather than from the cwd — a test
        // binary's cwd is the crate dir, and `cargo` would find the same workspace either
        // way, but naming it makes the invocation independent of how the test was launched.
        .current_dir(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("mac/ccd -> mac"),
        )
        .stdin(Stdio::null())
        .output()
        .expect("run cargo build -p codeconnect");
    assert!(
        out.status.success(),
        "the live gate could not build the launcher it drives, so it would otherwise run \
         against a stale binary and could pass against a change it never contained.\n\
         stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    // The premise is the gate's own verdict, not a version literal: `codeconnect`'s
    // launcher no longer pins a version, it pins the guarded surface. See
    // `codeconnect/tests/live_codex_host.rs` for the full reasoning.
    match codex_broker::guarded_surface::unadjudicated_against_baseline(&codex) {
        Ok(changes) if changes.is_empty() => {}
        Ok(changes) => panic!(
            "CC_CODEX_LIVE=1 resolved {} reporting codex {version}, whose guarded surface \
             CodeConnect is NOT grounded against:\n  {}",
            codex.display(),
            changes.join("\n  ")
        ),
        Err(why) => panic!(
            "CC_CODEX_LIVE=1 resolved {} but its guarded surface could not be read: {why}.",
            codex.display()
        ),
    }
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
    /// How many `thread/resume`s rode this connection. **More than one is normal.** A
    /// thread has no rollout until its first turn, so the link's attach is a retry loop
    /// on one connection until the answer becomes readable — see `codex_link`'s
    /// `Attach::Backoff`. An earlier version of this walker counted the second resume as
    /// a step out of order, which was true only while an announcement discharged the
    /// attach and a link therefore asked at most once.
    resumes: usize,
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
                resumes: 0,
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
        // A repeated `thread/resume` on a connection that has already reached the attach
        // stage is the **not-ready retry loop**, which is the ordinary pre-turn state of
        // every link: a thread has no rollout until its first turn, so the link asks,
        // backs off and asks again on the same connection. Counted, never disorder.
        let attach_retry = step == 4 && conn.stage == 4;
        if step == conn.stage + 1 {
            conn.stage = step;
            if step == 4 {
                conn.resumes += 1;
            }
        } else if attach_retry {
            conn.resumes += 1;
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

    // 3. The RETRY shape — three resumes on one connection that never ends. This is the
    //    ordinary pre-turn state of every link: a thread has no rollout until its first
    //    turn, so the attach backs off and asks again on the same connection. It must
    //    read as a healthy in-flight connection, NOT as disorder.
    //
    //    **This expectation was inverted for one round**, when an announcement
    //    discharged the attach and a link therefore asked at most once per connection.
    //    Under that rule a second resume really was anomalous; now it is the norm, and a
    //    walker that still called it disorder would fail every live run before the first
    //    turn.
    assert_eq!(
        verdict(&[
            "Ccd: leg opened (conn 1)",
            "Ccd: forward (request allowlisted) (conn 1)",
            "Ccd: forward (notification allowlisted) (conn 1)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 1)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 1)",
            "Ccd: forward (ownership request: fingerprint asserted) (conn 1)",
        ]),
        (vec![("1".into(), 4, 0)], vec![], Some("1".into()), vec![],),
        "a retry on the same connection is the not-ready attach loop, and it must read \
         as in-flight rather than as a step out of order"
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
            // A7.1 executable hash-pin: the coordinator carries the identity of the
            // codex binary beside its path, and the host re-verifies it immediately
            // before each of its two execs. Required — a missing digest is refused,
            // never defaulted to trusting the pathname.
            //
            // Derived here rather than taken from the launcher, and since 2e-7d
            // that is a choice: `codeconnect codex` pins its own digest now, and
            // its whole path is gated end to end by
            // `the_codex_command_launches_a_real_session_end_to_end`
            // (`codeconnect/tests/live_codex_coordinator.rs`). This harness keeps
            // spawning the coordinator directly because it holds the session at
            // `--test-bringup hang` — a test-only charter flag the shipping
            // launcher cannot emit, and the thing that lets this file drive the
            // LINK against a session that is deliberately parked mid-bring-up.
            .args([
                "--codex-sha256",
                &protocol::hash::sha256_file(codex).expect("hash the codex binary under test"),
            ])
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

    /// Pin this sandbox's codex to a specific model and reasoning effort, by writing the
    /// `config.toml` a real operator's `~/.codex` would carry (2e-4c).
    ///
    /// **This is the only way a brokered session's model can differ from the default**, and
    /// that is a MEASURED constraint rather than a harness convenience: the TUI's `/model`
    /// affordance drives `thread/settings/update` (plus a `config/batchWrite` to persist
    /// it), and the broker refuses `thread/settings/update` outright as
    /// `OwnershipAdjacent` — it is the same method that can move `approval_policy`, and it
    /// durably widens policy with a bare `result:{}`. So within a brokered session the
    /// model is FIXED at launch, and the population the 2e-4c pin widening actually serves
    /// is "an operator whose configured model is not the one the fixture captured".
    ///
    /// Must be called before `spawn_coordinator`: the app-server reads the config once, at
    /// start.
    fn pin_model(&self, model: &str, effort: &str) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.codex_home.join("config.toml"))
            .expect("create the sandbox config.toml");
        writeln!(f, "model = \"{model}\"").expect("write model");
        writeln!(f, "model_reasoning_effort = \"{effort}\"").expect("write effort");
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
/// The daemon this gate's link records into, with its push sender kept.
///
/// [`crate::apns::LoggingPushSender`] records the last hint it was handed, which is
/// all claim 7b needs: whether a real turn, finishing on a real app-server, rang a
/// real doorbell.
///
/// **Where that evidence stops, said out loud.** It records the hint at the
/// `PushSender` seam — *before* either real sender reads a device row — so it
/// proves the trigger and the subject, and nothing about delivery. Two further
/// things stood between this and a phone that buzzes, and **one of the two has
/// since been removed.** The daemon no longer refuses a Codex registration:
/// [`crate::state::Daemon::supported_agents`] admits Codex, so a real coordinator
/// can register a real run and this daemon will host it. What remains is the
/// device side, and it is deliberate state of the build rather than a gap here —
/// no device can be Codex-eligible yet, because the shipping iOS client
/// advertises a feature set on neither `hello` nor `register_push` and nothing in
/// this phase writes the column at all, so every stored device decodes as the
/// Claude-only floor and [`crate::push_queue::recipients`] narrows a Codex
/// doorbell to nobody. That narrowing is the read side standing on its own, which
/// is precisely why the write side was deleted rather than repaired. Claim 7b is
/// therefore still the staged half — the doorbell rings, correctly addressed —
/// and the delivery half arrives with the phone work that can render what it
/// opens onto.
fn live_daemon(
    session: &SessionKey,
) -> (
    Arc<crate::state::Daemon>,
    TempDb,
    Arc<crate::apns::LoggingPushSender>,
) {
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
    let push = Arc::new(crate::apns::LoggingPushSender::new());
    let daemon = crate::state::Daemon::new(
        protocol::config::Config::default(),
        store,
        Arc::clone(&push) as Arc<dyn crate::apns::PushSender>,
        crate::state::Endpoint {
            host: "test.ts.net".into(),
            port: 8787,
            tls: false,
        },
        tail_tx,
    );
    (daemon, db, push)
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

/// **A session launched on a model the fixture never captured still runs a turn** (2e-4c).
///
/// The live subject of the model-pin widening, and the reason it needed one.
///
/// 2e-4a pinned `turn/start`'s `collaborationMode` to the captured value BYTE FOR BYTE.
/// That field carries `settings.model` and `settings.reasoning_effort`, so the pin bound
/// the whole broker to one specific model: an operator whose `~/.codex/config.toml` says
/// `model = "gpt-5.6-terra"` got a policy refusal on their very first turn, with an audit
/// note about a captured boundary. A13 recorded that refusal as "the designed 2e-4c
/// re-grounding trigger", and this is the gate that proves the trigger was discharged
/// against a real wire rather than against an argument.
///
/// The spike measured eleven real `turn/start` frames across three models and found the
/// field splits cleanly: `mode` and `developer_instructions` byte-identical every time —
/// including against the 2e-4a fixture captured a week earlier on a different sandbox —
/// while `model` and `reasoning_effort` simply carried whatever the picker last set. The
/// instruction channel stays pinned exactly; the two knobs are type-checked. This gate
/// runs a session on `gpt-5.6-terra` at `high`, neither of which appears in the fixture,
/// and requires the turn to REACH THE MODEL.
///
/// Failure here is unambiguous and is the point: under the old rule the pane shows a
/// policy refusal and no reply ever arrives.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn a_session_launched_on_an_uncaptured_model_still_runs_a_turn() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("model");
    // Neither value appears anywhere in `fixtures/codex/turn-start-request.json`.
    sb.pin_model("gpt-5.6-terra", "high");
    let mut coord = sb.spawn_coordinator(&codex);

    assert!(
        wait_until(Duration::from_secs(60), || sb.broker_legs_bound()).await,
        "the host must bind both broker legs. appserver.stderr:\n{}",
        read_file(&sb.run_dir.join("appserver.stderr.log"))
    );
    assert!(
        wait_until(Duration::from_secs(60), || sb.tui_running()).await,
        "the host must launch the real codex TUI. appserver.stderr:\n{}",
        read_file(&sb.run_dir.join("appserver.stderr.log"))
    );
    // The pin took: the TUI's own footer names the model it will send.
    let pane_shows_model = wait_until(Duration::from_secs(60), || {
        sb.capture_pane().contains("gpt-5.6-terra")
    })
    .await;
    println!("pane after launch:\n{}", sb.capture_pane());
    assert!(
        pane_shows_model,
        "the TUI is not on gpt-5.6-terra, so this gate would pass vacuously on the \
         captured model. pane:\n{}",
        sb.capture_pane()
    );

    sb.send_keys(&["Reply with the single word amber and nothing else."]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    sb.send_keys(&["Enter"]);
    let replied = wait_until(Duration::from_secs(180), || {
        sb.capture_pane().to_lowercase().contains("• amber")
    })
    .await;
    println!("pane after the turn:\n{}", sb.capture_pane());
    let broker_log = read_file(&sb.run_dir.join("broker.log"));
    assert!(
        replied,
        "a turn on an uncaptured model did not complete. If broker.log carries a \
         `collaborationMode: captured boundary` refusal, the widening did not take and \
         the pin is still model-bound. broker.log:\n{broker_log}\npane:\n{}",
        sb.capture_pane()
    );
    // And prove it forwarded rather than being refused-then-somehow-answered.
    assert!(
        !broker_log.contains("collaborationMode"),
        "the broker refused something about collaborationMode during a session whose \
         turn nevertheless completed — that combination needs explaining before this \
         gate can be believed. broker.log:\n{broker_log}"
    );
    // **M12 — the turn actually carried the uncaptured PAIR, and HIGH effort.**
    //
    // "The turn completed" alone is compatible with a codex that silently ignored the
    // config and ran the captured model, in which case this gate would prove nothing about
    // the widening. The TUI's footer is the observable: it renders the model and effort it
    // is sending, and `high` in particular is a value the fixture does not contain anywhere
    // — the capture's efforts are `null` and `medium`.
    let pane = sb.capture_pane();
    assert!(
        pane.contains("gpt-5.6-terra") && pane.contains("high"),
        "the session must have run on the uncaptured model AND the uncaptured effort; \
         without both, a broker that still refused one half of the pair would pass this \
         gate. pane:\n{pane}"
    );
    // And the broker FORWARDED a turn — named in its own log — rather than merely not
    // refusing one.
    assert!(
        broker_log.contains("turn/start: head-checked"),
        "broker.log must show the turn/start FORWARDING through the head-check; its \
         absence would mean the turn never reached the broker at all. \
         broker.log:\n{broker_log}"
    );
    println!(
        "PASS a_session_launched_on_an_uncaptured_model_still_runs_a_turn — a real turn \
         on gpt-5.6-terra/high forwarded through the broker's widened collaborationMode \
         boundary, with the instruction channel still pinned byte-for-byte."
    );

    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
}

/// The whole control link, live: handshake, bind, observe, reconnect, resume, SWITCH.
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
    let (daemon, _db, push) = live_daemon(&session);
    let uid = session.uid.clone();
    // **Capture what production LOGS, from before it logs anything.** `crate::log::emit`
    // writes to stderr, which the harness does not capture, so the attach evidence every
    // claim below rests on is the line the daemon really wrote rather than one this gate
    // reconstructed. Installed before the link exists, because the attach it has to
    // witness can happen as soon as the first turn creates the rollout.
    crate::log::capture::install();
    // The link's published connection state, held so claim 5b can assert what a
    // resolver would have been told at the moment the turn landed.
    let first_presence = crate::codex_link::LinkPresence::new();
    let first = tokio::spawn(crate::codex_link::run(
        Arc::clone(&daemon),
        session.clone(),
        ControlLink {
            socket: sb.ccd_sock(),
            generation: 1,
            thread_id: None,
        },
        first_presence.clone(),
        crate::codex_link::LinkCarry::new(),
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
    // **The fan-out tripwire, taken over the WHOLE run rather than over the window.**
    //
    // Two runs of this gate in a row failed here for the same non-reason: a method that
    // is always delivered to this position — `thread/goal/cleared`, then
    // `app/list/updated` — drifted across the window boundary and read as "new". The
    // boundary is `observed_before_turn`, a snapshot taken a few seconds before the
    // prompt, so which side of it a periodic broadcast lands on is a property of timing,
    // not of the wire. Pinning a *slice* was the same mistake as the `== 2` status count
    // this file already carries a warning about.
    //
    // So the vocabulary claim is made where it is stable: over everything this
    // connection was handed, start to finish. That is what a fan-out change actually
    // moves, and it cannot drift.
    let distinct_all: std::collections::BTreeSet<&str> =
        observed_all.iter().map(String::as_str).collect();
    // Measured on this position across every run of this gate. All but one are methods
    // `codex_adapter` maps to `Vec::new()` — observation noise by construction. The
    // exception is `thread/started`, which is the announcement itself: it does carry a
    // fact, and it is broadcast to merely-initialized connections by design (A1/D2).
    // That is the one delivery an unsubscribed connection is *supposed* to get.
    //
    // **`skills/changed` was added in round 3, and its provenance is recorded rather
    // than assumed.** It began appearing when a `~/.codex/skills` directory came to
    // exist on this machine; the app-server broadcasts it and this position receives
    // it, intermittently, depending on when the watcher fires relative to the run.
    // Two things were checked before widening the set, because this comment is the
    // only thing standing between "measured" and "whatever showed up":
    //
    //   * **Against the adapter**, as the assertion below demands: `skills/changed`
    //     matches no arm of `CodexAdapter::normalize` and falls to its unknown-method
    //     drop, so it mints NO fact. Note the weaker footing — the other four are
    //     listed there explicitly as observation noise, this one is merely unknown —
    //     which is why it is called out here instead of being quietly appended.
    //   * **Against this round's diff**: with the broker's head fan-out neutralized
    //     back to its round-2 behaviour, `skills/changed` still arrived on 5 of 8
    //     runs. It is environment drift, not something the fan-out change produced —
    //     and it could not be, since the only frame that change can add to this
    //     position is the `thread/started` already named above.
    let measured_fanout: std::collections::BTreeSet<&str> = [
        "thread/started",
        "thread/status/changed",
        "thread/goal/cleared",
        "app/list/updated",
        "remoteControl/status/changed",
        "skills/changed",
    ]
    .into_iter()
    .collect();
    assert!(
        distinct_all.is_subset(&measured_fanout),
        "a ccd connection that resumed nothing was handed a method this position has \
         never been measured to receive. Anything new here has to be checked against \
         the adapter before it is added: a method that maps to a FACT arriving on an \
         unsubscribed connection would break the whole `bound is not subscribed` story \
         this chunk rests on. measured: {measured_fanout:?} — got: {distinct_all:?}"
    );
    let turn_frames: Vec<&String> = observed_all
        .iter()
        .filter(|m| m.starts_with("turn/"))
        .collect();
    let item_frames: Vec<&String> = observed_all
        .iter()
        .filter(|m| m.starts_with("item/"))
        .collect();
    // **This is the measurement that says why attaching matters.** A connection that
    // has not resumed is handed nothing of a turn — so turn observation is not a
    // property of being connected, or even of being bound, but of having attached.
    // Claim 7 below is the other side of it: the SAME wire, to a connection that DID
    // resume, delivers the whole turn.
    assert!(
        turn_frames.is_empty() && item_frames.is_empty(),
        "a ccd connection that resumed NOTHING was handed {} turn/* and {} item/* \
         frame(s) across a turn. Subscription is what a successful thread/resume buys, \
         and this connection never sent one — so the app-server's fan-out has changed, \
         and with it the reason the attach in claim 6b is what unlocks turn \
         observation at all. turn/*: {turn_frames:?} item/*: {item_frames:?} \
         everything: {observed_all:?}",
        turn_frames.len(),
        item_frames.len()
    );
    println!(
        "CLAIM 5a PASS — the observer answered a census read AFTER the turn window, \
         and the snapshot taken behind that barrier holds {} method(s) across the turn \
         ({status_across_turn} of them thread/status/changed), and across the whole run \
         every method it received was one this position is measured to get, ZERO of \
         them turn/* or item/*; the \
         zeros are a fact about the wire, not about a dead connection or about when \
         this test looked: {across_turn:?}",
        across_turn.len()
    );

    // **The other half of claim 5, and it now points the opposite way.**
    //
    // The tap resumed nothing and was handed nothing — that is the count above. THIS
    // link resumed: it bound from the broadcast and then, per the attach contract, kept
    // asking. Before the turn every ask met the measured not-ready error, because a
    // thread with no rollout cannot be resumed. The turn created the rollout, so one of
    // those asks is answered, and acceptance subscribes the connection that has been
    // watching all along.
    //
    // Until 2e-4b an announcement DISCHARGED the attach and this link would never have
    // asked at all. Against the measured wire that is a link bound to its thread and
    // permanently deaf — which is the defect this assertion now guards, in the honest
    // form: the same connection, never restarted, has to end up subscribed.
    //
    // Polled to the fact rather than to a clock: the attach retry backoff runs to 30s,
    // so how long the link waits after the rollout appears is a property of when it
    // started, not of the wire.
    let mut daemon_log: Vec<String> = Vec::new();
    let attached = wait_until(Duration::from_secs(120), || {
        daemon_log.extend(crate::log::capture::drain());
        daemon_log
            .iter()
            .any(|line| line.contains("attached to thread") && line.contains(&thread_id))
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    daemon_log.extend(crate::log::capture::drain());
    let facts_after_turn = recorded(&daemon, &uid);
    print_events(&daemon, &uid, "after the turn ran");
    assert!(
        attached,
        "the ORIGINAL link never attached. It bound from thread/started and must keep \
         resuming until the first turn creates the rollout; an announcement does not \
         settle the attach, because only an accepted answer subscribes a connection. A \
         link that stops asking here is bound, silent and deaf for the life of the \
         session.\ncaptured:\n{}",
        daemon_log.join("\n")
    );
    for line in daemon_log
        .iter()
        .filter(|l| l.contains("attached to thread"))
    {
        println!("the attach line the daemon LOGGED:\n{line}");
    }
    assert!(
        !daemon_log
            .iter()
            .any(|l| l.contains("STOP-AND-AMEND") && l.contains(&thread_id)),
        "the link both attached and reported the answer unreadable for {thread_id}: \
         the acceptance rule is not stable across the answers it accepts:\n{}",
        daemon_log.join("\n")
    );
    // Its timeline grew, which is the tripwire flip. The turn terminal is what is
    // asserted, because it is the one fact that lands whichever side of the turn the
    // attach fell on: recovered from a `completed` answer, or observed live as
    // `turn/completed` after attaching mid-turn.
    assert!(
        facts_after_turn.len() > facts_before_turn.len(),
        "the link attached but recorded nothing of the turn. before={facts_before_turn:?} \
         after={facts_after_turn:?}"
    );
    let turn_one_terminal = daemon
        .store
        .events_after(&uid, 0, 10_000)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == protocol::event::EventKind::TurnComplete)
        .expect("the attached link records the turn's terminal");
    let turn_one_id = turn_one_terminal
        .turn_id
        .clone()
        .expect("a turn terminal names its turn");
    assert_eq!(turn_one_terminal.payload["status"], "completed");
    println!(
        "CLAIM 5b PASS — the ORIGINAL link (never restarted) is SUBSCRIBED: it bound \
         from the broadcast, kept resuming through the not-ready answers, and attached \
         once the turn created the rollout. Its timeline went from {} fact(s) to {}, \
         including the terminal for turn {turn_one_id} — while the tap, which resumed \
         nothing, was handed no turn frame at all across the same window.",
        facts_before_turn.len(),
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

    // **The link is deliberately NOT restarted here.** An earlier version of this gate
    // aborted it and pointed a fresh one at the thread id, which made the acceptance
    // claim true of a link that had just been handed its target — and quietly hid the
    // defect P1 fixes, that a link which bound from the broadcast never asked at all.
    // The milestone has to hold on the connection that has been watching since before
    // the TUI existed, so that is the connection every claim below is about.
    println!(
        "the ORIGINAL link is still running and attached ({} facts recorded)",
        facts_after_turn.len()
    );

    // **The wire fact the acceptance rule is designed against, verbatim.** Post-turn the same
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
         The rollout a completed turn is supposed to create does not exist, so there \
         is nothing to reconcile and this gate's evidence is not what it claims: \
         {post_turn_resume}"
    );
    let turns = post_turn_resume["result"]["thread"]["turns"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "the post-turn resume must carry result.thread.turns[]; without it \
                 there is no recovered history to reconcile, whatever else the \
                 answer says: {post_turn_resume}"
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
    // the gate typed one prompt into the pane, and a reconciliation must be grounded
    // in an answer whose turn count is KNOWN, not merely non-zero.
    assert_eq!(
        turns.len(),
        1,
        "this run submitted ONE turn and result.thread.turns[] came back with {}. \
         Either the pane ran work this gate did not ask for — in which case the \
         redaction and shape claims below are about a session whose content is not \
         accounted for — or the app-server's turns[] is not the per-turn array the \
         acceptance rule reads: {post_turn_resume}",
        turns.len()
    );

    // **The live answer, against the committed ground truth.** `!turns.is_empty()`
    // is satisfied by an array of anything; what the acceptance rule reads is the
    // answer's SUBSTANCE, so the substance is what is checked — field for field
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
         is a turn state the acceptance rule refuses outright (no answer describing \
         one has ever been captured), so this gate would be certifying a path nothing \
         exercises: {post_turn_resume}",
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
        "the recovered turn carries no id. Recovered terminals are keyed by turn, and \
         an answer that does not name its turn cannot be reconciled at all: \
         {post_turn_resume}"
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
    // **And the roots are checked by VALUE now, for the same reason the cwd is.**
    //
    // This used to be a type check, justified by "what a live run's workspace roots
    // resolve to is not something this gate launched, so pinning them would pin an
    // accident". That reasoning was MEASURED FALSE in 2e-7c. Proxying a real
    // `codex --remote` TUI against a real app-server — from a git repository root,
    // from a deep subdirectory of one, and from a directory in no repository at all —
    // the TUI sent `runtimeWorkspaceRoots: [<its own cwd, canonicalized>]` in every
    // case, and the app-server echoed it back verbatim. It is not the git root and it
    // does not vary with repo-ness: it is exactly the launch directory, which IS
    // something this gate launched.
    //
    // So the broker now anchors it (A10 follow-on) exactly as it anchors `cwd`, and
    // this assertion is the live end of that anchor: a real session, resumed after a
    // real turn, still reporting the one workspace root the launch asked for.
    let roots = post_turn_resume["result"]["runtimeWorkspaceRoots"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "result.runtimeWorkspaceRoots is not an array. It is one of the fields \
                 a recovered session's workspace is read from: {post_turn_resume}"
            )
        });
    // **Compared as the array the wire sent, element for element** — not as a
    // projection of it. This read `filter_map(Value::as_str)`, which is a filter
    // and not a decode: every element that is not a string was silently dropped
    // before the comparison, so `[<the launch cwd>, 7]` collapsed to the expected
    // one-element vector and PASSED an assertion whose message claims the roots
    // are exactly what the launch anchored. A non-string root is precisely the
    // shape a widened or malformed scope would arrive in, so the one class of
    // answer worth catching was the one the filter removed. Comparing the
    // `Vec<Value>` against a `serde_json` array makes length, order and type all
    // load-bearing, and there is nothing left for an element to hide behind.
    let expected_roots =
        serde_json::json!([expected_cwd.to_str().expect("the launch cwd is utf-8")]);
    assert_eq!(
        Value::Array(roots.clone()),
        expected_roots,
        "the resumed thread reports workspace roots {roots:?}, but this gate launched \
         the coordinator with `--cwd /tmp` ({}). The roots are the writable scope the \
         session runs against, so anything else — another path, an extra element, or an \
         element that is not a path at all — means the recovered session's workspace is \
         wider, or simply other, than the one the launch anchored: {post_turn_resume}",
        expected_cwd.display()
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

    // --- 9. CLAIM 6b: the ORIGINAL link accepted that answer, and it holds -------
    //
    // Claim 5b already established the attach off the daemon's own log. What is checked
    // here is the shape of it on the wire and in the store: an accepted answer keeps its
    // connection, and what the answer described is durable under the ids the live wire
    // uses.
    //
    // **The ccd leg's own account, by connection identity.** A REFUSAL ends the
    // connection — `serve_connection` bails and `run` reconnects — so the refuse loop
    // shows up as a run of connections each walking the whole lifecycle: opened,
    // initialize, initialized, thread/resume, **ended**. An attach is the exact inverse:
    // the connection reaches thread/resume and then stays. Read off the broker's own
    // `(conn N)` stamps rather than off counts, because counts cannot tell a cycle from
    // a retry.
    // **The link's own account first, because it is the only one that is correlated.**
    //
    // The broker's ccd leg carries this test's other clients too — the tap and the raw
    // census/resume client — so "some connection reached thread/resume and stayed open"
    // is a statement about the leg, not about the link. An earlier version of this claim
    // asserted exactly that and called it evidence.
    //
    // The link, though, stamps its own connection counter into every handshake it
    // completes: `initialized on the ccd leg (epoch N)`. A refusal ENDS the connection —
    // `serve_connection` bails and `run` reconnects — so a refused answer is always
    // followed by another handshake line. The attach is therefore correlated by the
    // link's own log: after the line where it attached, it must never have handshaken
    // again.
    let attach_at = daemon_log
        .iter()
        .position(|line| line.contains("attached to thread") && line.contains(&thread_id))
        .expect("claim 5b established the attach line exists");
    let rehandshakes: Vec<&String> = daemon_log[attach_at..]
        .iter()
        .filter(|line| line.contains("initialized on the ccd leg"))
        .collect();
    assert!(
        rehandshakes.is_empty(),
        "after attaching, the link handshaked again — so its connection ENDED, which is \
         what a refusal does and precisely what acceptance is supposed to retire. This \
         is the link's own log, so it is about the link's own connection rather than \
         about whichever connection on the leg happened to look orderly: \
         {rehandshakes:#?}\nwhole log:\n{}",
        daemon_log.join("\n")
    );
    let epochs: Vec<&String> = daemon_log
        .iter()
        .filter(|line| line.contains("initialized on the ccd leg"))
        .collect();
    println!(
        "the link handshaked {} time(s), all of them BEFORE it attached: {epochs:#?}",
        epochs.len()
    );

    // Corroboration from the broker's side, labelled as such: some connection on the leg
    // reached the attach and never ended. It cannot be attributed to the link by id —
    // the link is never told its broker connection number — so it is printed and
    // weakly asserted rather than relied on.
    let ccd_lines = ccd_tail(&broker_log, 0);
    for conn in ccd_connections(&ccd_lines) {
        println!(
            "  conn {}: reached {} ({}/{}) after {} resume(s){}",
            conn.id,
            conn.reached(),
            conn.stage,
            CCD_LIFECYCLE.len(),
            conn.resumes,
            match conn.disordered.as_slice() {
                [] => String::new(),
                lines => format!("  OUT OF ORDER: {lines:#?}"),
            }
        );
    }
    assert!(
        ccd_connections(&ccd_lines)
            .iter()
            .any(|c| c.stage == 4 && c.disordered.is_empty()),
        "the broker's leg shows no connection that reached thread/resume and stayed \
         open: {ccd_lines:#?}"
    );

    // What the link holds for that turn, under the ids the wire uses.
    let stored = daemon.store.events_after(&uid, 0, 10_000).unwrap();
    assert_eq!(
        stored
            .iter()
            .filter(|e| e.kind == protocol::event::EventKind::TurnComplete)
            .count(),
        1,
        "exactly one turn has run, so exactly one turn terminal: {stored:?}"
    );
    assert_eq!(
        turn_one_id, live_turn_id,
        "the recorded terminal is attributed to a different turn than the one the \
         resume answer reported ({live_turn_id})"
    );
    println!(
        "CLAIM 6b PASS — the ORIGINAL link ACCEPTED the populated answer, and its OWN \
         log proves the connection was kept: it handshaked only before attaching and \
         never again, which is what a refusal would have forced. No refusal was emitted, \
         and it holds turn {turn_one_id} as a completed terminal. It has been on this \
         connection since before the TUI existed; nothing restarted it."
    );

    // --- 10. CLAIM 6c: the mid-turn attach FINISHES ITSELF ------------------------
    //
    // A link that attaches while a turn is running is told that turn is `inProgress`,
    // whose item ids are placeholders — so it recovers none of that turn's items, and
    // the ones that had already finished are unreachable from both directions at once:
    // no live frame will carry them (they completed before the subscription existed) and
    // no answer will name them (a running turn does not report real ids). Left alone the
    // link sits `Attached` and perfectly quiescent with that gap permanent.
    //
    // So when a turn it attached across terminalizes, it asks **once** more. This claim
    // is that the gap is closed by the link itself, on its own connection, with nothing
    // restarted — and it is asserted against the ids the ANSWER gives, not against a
    // count.
    let seeded_running = daemon_log
        .get(attach_at)
        .is_some_and(|line| !line.contains("0 still running"));
    println!(
        "the attach {} the first turn",
        if seeded_running {
            "joined MID-FLIGHT of"
        } else {
            "landed after"
        }
    );
    let wanted_items: Vec<String> = post_turn_resume["result"]["thread"]["turns"][0]["items"]
        .as_array()
        .expect("the answer's items")
        .iter()
        .filter_map(|item| item["id"].as_str().map(str::to_string))
        .collect();
    assert!(
        !wanted_items.is_empty(),
        "the answer describes no items for the completed turn, so this claim would be \
         vacuous: {post_turn_resume}"
    );
    let complete = wait_until(Duration::from_secs(60), || {
        let have: std::collections::BTreeSet<String> = daemon
            .store
            .events_after(&uid, 0, 10_000)
            .unwrap()
            .into_iter()
            .filter_map(|e| e.item_id)
            .collect();
        wanted_items.iter().all(|id| have.contains(id))
    })
    .await;
    print_events(&daemon, &uid, "after the follow-up attach");
    daemon_log.extend(crate::log::capture::drain());
    assert!(
        complete,
        "the link's timeline for the first turn is INCOMPLETE: the answer describes \
         {wanted_items:?} and the store does not hold them all. If the attach joined the \
         turn mid-flight, the items that finished first can only be recovered by the \
         follow-up resume the link owes itself — and nothing else in the machine will \
         ever ask again, so this gap would be permanent.\ndaemon log:\n{}",
        daemon_log.join("\n")
    );
    if seeded_running {
        let follow_ups: Vec<&String> = daemon_log
            .iter()
            .filter(|line| line.contains("asking once more"))
            .collect();
        assert!(
            !follow_ups.is_empty(),
            "the attach joined the turn mid-flight, so a follow-up was owed and the link \
             must have logged it. A complete timeline without one would mean the items \
             arrived by some route this claim does not understand:\n{}",
            daemon_log.join("\n")
        );
        println!("the follow-up the link fired: {follow_ups:#?}");
        // Still no re-handshake: the follow-up rides the connection it already has.
        assert!(
            daemon_log[attach_at..]
                .iter()
                .all(|line| !line.contains("initialized on the ccd leg")),
            "the follow-up must ride the SAME connection — re-handshaking would throw \
             away the subscription it exists to preserve:\n{}",
            daemon_log.join("\n")
        );
    }
    println!(
        "CLAIM 6c PASS — the link's timeline for turn {turn_one_id} is complete \
         ({} item(s), all present){}. No fresh link was involved.",
        wanted_items.len(),
        if seeded_running {
            " — and it attached MID-TURN, so the items that finished before it \
             subscribed were recovered by the follow-up resume it fired on its own \
             connection when the turn terminalized"
        } else {
            " (the attach landed after the turn, so the answer described it whole)"
        }
    );

    // --- 11. CLAIM 7: the attached link OBSERVES A SECOND TURN, LIVE --------------
    //
    // Acceptance is what subscribes (measured in 2e-4a: turn frames reach only the
    // resume-subscribed connection), so the whole point of attaching is what happens
    // next. A second real turn runs, and this link — the ORIGINAL one, which bound from
    // the broadcast, kept asking, and attached when the rollout appeared — must now be
    // handed the `turn/*` and `item/*` frames the tap is measured never to get, and must
    // record them as facts of their own turn.
    //
    // **This is the turn the whole chunk is for**, and it is asserted on the connection
    // that has been open since before the TUI started. Nothing was restarted to make it
    // work.
    //
    // The discriminator that this is LIVE observation and not another recovery is the
    // **usage** fact: token totals arrive only as `thread/tokenUsage/updated`
    // notifications and the resume answer does not carry them anywhere. A usage fact
    // for turn two can only have come off the wire.
    let facts_before_turn_two = recorded(&daemon, &uid);
    sb.send_keys(&["Reply with the single word blue and nothing else."]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    sb.send_keys(&["Enter"]);
    let replied_again = wait_until(Duration::from_secs(120), || {
        sb.capture_pane().to_lowercase().contains("• blue")
    })
    .await;
    println!("pane after the second turn:\n{}", sb.capture_pane());
    assert!(
        replied_again,
        "the second turn did not complete within 120s, so there is no live turn for the \
         attached link to have observed. pane:\n{}",
        sb.capture_pane()
    );
    // Polled to the fact, not to a clock: a fixed sleep would report "the link observed
    // nothing" on a slow machine, which is the opposite conclusion.
    let observed_live = wait_until(Duration::from_secs(60), || {
        daemon
            .store
            .events_after(&uid, 0, 10_000)
            .unwrap()
            .iter()
            .any(|e| {
                e.kind == protocol::event::EventKind::TurnComplete
                    && e.turn_id.as_deref() != Some(turn_one_id.as_str())
            })
    })
    .await;
    print_events(&daemon, &uid, "after the second turn");
    assert!(
        observed_live,
        "the attached link did not record a second turn. Acceptance is \
         what subscribes a connection, so a link that accepted the answer and then saw \
         nothing of the next turn means either the attach did not subscribe it or the \
         adapter dropped what it was handed. before={facts_before_turn_two:?}"
    );
    let stored = daemon.store.events_after(&uid, 0, 10_000).unwrap();
    let turn_two: Vec<&protocol::event::Event> = stored
        .iter()
        .filter(|e| {
            e.turn_id
                .as_deref()
                .is_some_and(|t| t != turn_one_id.as_str())
        })
        .collect();
    let turn_two_id = turn_two
        .iter()
        .find_map(|e| e.turn_id.clone())
        .expect("the second turn names itself");
    assert_ne!(
        turn_two_id, turn_one_id,
        "the second turn must be a turn of its own"
    );
    for kind in [
        protocol::event::EventKind::UserMessage,
        protocol::event::EventKind::AgentMessage,
        protocol::event::EventKind::TurnComplete,
        // The one that can ONLY have come off the wire.
        protocol::event::EventKind::Usage,
    ] {
        assert!(
            turn_two.iter().any(|e| e.kind == kind),
            "the attached link recorded no {kind:?} for the live second turn \
             {turn_two_id}. A missing usage fact in particular would mean the facts \
             below came from a resume answer rather than from live frames, since an \
             answer carries no token totals at all: {turn_two:?}"
        );
    }
    assert_eq!(
        turn_two
            .iter()
            .find(|e| e.kind == protocol::event::EventKind::TurnComplete)
            .map(|e| e.payload["status"].clone()),
        Some(serde_json::json!("completed"))
    );
    println!(
        "CLAIM 7 PASS — the attached link was handed the second turn LIVE and recorded \
         {} fact(s) under turn {turn_two_id}, including the token usage that exists \
         only on the notification wire. The link observes turns now; that is what the \
         attach bought.",
        turn_two.len()
    );

    // --- 11b. CLAIM 7b: that turn RANG, and the link says what it is ---------------
    //
    // The doorbell hangs off a terminal this link watched arrive, and the turn above
    // is the only one in this run that qualifies — turn 1 was recovered through a
    // resume answer, and a resume answer never rings. So a doorbell here is a
    // doorbell for the turn claim 7 just proved was live, and its absence would mean
    // the trigger is wired to something a real app-server does not produce.
    //
    // Past the dispatch grace first: `dispatch_push` waits out a window before it
    // rings, and reading the probe too early would report silence for a push that was
    // merely still in flight.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let rang = push
        .last()
        .expect("a turn observed finishing live must ring the doorbell");
    assert_eq!(
        rang.kind,
        crate::apns::PushKind::Completed,
        "the doorbell says a turn finished and carries nothing the agent wrote: {rang:?}"
    );
    assert_eq!(
        rang.agent,
        protocol::agent::AgentKind::Codex,
        "without the agent the fan-out cannot narrow to phones that can open a Codex \
         run, and every Claude-only device would be rung: {rang:?}"
    );
    assert_eq!(
        rang.session_uid, session.uid,
        "the doorbell must be about this run: {rang:?}"
    );
    // And the resolver's own view of the same connection, taken from the link that
    // has been open since before the TUI existed. **Bound is not subscribed**, and
    // this is the state that says an inbound request would have an addressee.
    assert_eq!(
        first_presence.get(),
        crate::codex_link::CodexAddressee::Subscribed {
            thread_id: thread_id.clone()
        },
        "the link that observed the turn must publish itself as subscribed to the \
         thread it observed it on — that published state is what the inbound resolver \
         reads, and anything weaker would resolve a request to nowhere"
    );
    println!(
        "CLAIM 7b PASS — the live turn rang a content-free {:?} doorbell for agent \
         {}, and the link publishes Subscribed{{{thread_id}}} — the addressee an \
         inbound request resolves to.",
        rang.kind,
        rang.agent.as_str()
    );

    // --- 12. CLAIM 8: two turns, re-described, and every fact still ONE ------------
    //
    // The completeness premise, asserted rather than assumed: after two turns the answer
    // must describe TWO, and they must be the two this gate watched happen.
    //
    // Then the dedup claim in its strongest form. The original link is stopped — the
    // **daemon-restart** shape, and the only place in this gate where a link is replaced
    // — and a fresh one is pointed at the thread id, exactly as a daemon coming back
    // after a restart would be. It is handed an answer describing both turns, and every
    // fact it re-derives has to land on a row that already exists.
    //
    // This is not the milestone being papered over: the milestone was proven above on
    // the link that was never restarted. This is the separate question of what a
    // restarted daemon does to a timeline that is already there.
    first.abort();
    let _ = first.await;
    // A short settle before the snapshot: `abort` stops the task at its next await
    // point, and a fact it was mid-ingest on would otherwise land just after this line
    // and read as a row the re-attach added.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let before_third = recorded(&daemon, &uid);

    let two_turn_resume = raw
        .request(
            "thread/resume",
            serde_json::json!({"threadId": thread_id}),
            Duration::from_secs(60),
        )
        .await;
    let turns_now = two_turn_resume["result"]["thread"]["turns"]
        .as_array()
        .unwrap_or_else(|| panic!("the answer must carry turns[]: {two_turn_resume}"));
    let described: Vec<&str> = turns_now.iter().filter_map(|t| t["id"].as_str()).collect();
    assert_eq!(
        described,
        vec![turn_one_id.as_str(), turn_two_id.as_str()],
        "after two turns the resume answer must describe BOTH, oldest first, under the \
         very ids this gate watched them run with. A short answer means turns[] pages or \
         truncates, and the reconciliation reads it as complete: {two_turn_resume}"
    );
    for turn in turns_now {
        assert_eq!(
            turn["itemsView"].as_str(),
            Some("full"),
            "the seeding path only reads an item list the answer itself calls complete; \
             a turn reporting anything else is refused, and this gate would then be \
             certifying a rule nothing exercises: {turn}"
        );
        assert_eq!(turn["status"].as_str(), Some("completed"), "{turn}");
    }
    println!(
        "two-turn completeness: the answer describes {:?}, both `full` and `completed`",
        described
    );

    crate::log::capture::install();
    let third = tokio::spawn(crate::codex_link::run(
        Arc::clone(&daemon),
        session.clone(),
        ControlLink {
            socket: sb.ccd_sock(),
            generation: 1,
            thread_id: Some(thread_id.clone()),
        },
        crate::codex_link::LinkPresence::new(),
        crate::codex_link::LinkCarry::new(),
    ));
    let mut third_log: Vec<String> = Vec::new();
    let reattached = wait_until(Duration::from_secs(60), || {
        third_log.extend(crate::log::capture::drain());
        third_log
            .iter()
            .any(|line| line.contains("attached to thread") && line.contains(&thread_id))
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    third_log.extend(crate::log::capture::drain());
    crate::log::capture::uninstall();
    assert!(
        reattached,
        "a link re-attaching to a thread with TWO turns behind it must accept the \
         answer just as readily as one with a single turn:\n{}",
        third_log.join("\n")
    );

    let after_third = recorded(&daemon, &uid);
    print_events(&daemon, &uid, "after the second re-attach");
    // **Nothing doubled, nothing moved.** Every fact that existed before still exists,
    // at the same seq; and nothing is in the store twice.
    for (key, seq) in &before_third {
        assert_eq!(
            after_third.iter().find(|(k, _)| k == key).map(|(_, s)| *s),
            Some(*seq),
            "{key} was re-appended when the answer described it a second time. Turn one \
             was recovered FROM an answer and turn two was observed LIVE, and re-reading \
             an answer that describes both must collapse onto both — that collapse is \
             the only thing standing between a reconnect and a duplicated timeline"
        );
    }
    // **What a re-attach is allowed to ADD, and why it is not a duplicate.**
    //
    // A link that attaches while a turn is still running gets an answer reporting that
    // turn `inProgress`, whose item ids are measured placeholders — so it recovers none
    // of that turn's already-finished items, and picks up only what the wire sends it
    // from that moment on. Those missed items are not lost: the next answer reports the
    // turn `completed`, with the real ids, and they land then. That is the design
    // healing its own edge, and this run exercised it — the link attached partway
    // through the first turn.
    //
    // So new rows here are legitimate, but only of one shape: an ITEM of a turn whose
    // terminal is already recorded. A new turn terminal or a new usage fact would mean
    // something else entirely — a turn being re-keyed, or a stale total being promoted
    // under a first-wins key.
    let added: Vec<&(String, u64)> = after_third
        .iter()
        .filter(|(key, _)| !before_third.iter().any(|(had, _)| had == key))
        .collect();
    let stored_now = daemon.store.events_after(&uid, 0, 10_000).unwrap();
    let terminalized: std::collections::BTreeSet<String> = stored_now
        .iter()
        .filter(|e| e.kind == protocol::event::EventKind::TurnComplete)
        .filter_map(|e| e.turn_id.clone())
        .collect();
    for (key, _) in &added {
        assert!(
            key.contains(":item:"),
            "the re-attach added {key}, which is not an item fact. Only items missed by \
             a mid-turn attach may appear late; a turn terminal or a usage total \
             appearing here would mean a turn was re-keyed or a stale total promoted"
        );
        let event = stored_now
            .iter()
            .find(|e| e.source_event_id.as_deref() == Some(key.as_str()))
            .expect("the added fact is in the store");
        let turn = event
            .turn_id
            .clone()
            .unwrap_or_else(|| panic!("a recovered item is attributed to its turn: {key}"));
        assert!(
            terminalized.contains(&turn),
            "the re-attach added {key} for turn {turn}, which has no terminal in the \
             store. A late-recovered item must belong to a turn the answer reports \
             FINISHED — that is the only state whose item ids are the real ones"
        );
    }
    // **With the follow-up attach in place, there is nothing left for a restart to
    // heal.** Before it, a mid-turn attach left its turn's already-finished items
    // permanently missing and only a fresh link recovered them — which made a restart
    // look like part of the design. It is not: the link settles its own debt the moment
    // the turn terminalizes (claim 6c), so by the time a daemon restarts, the timeline
    // is already whole.
    assert!(
        added.is_empty(),
        "the re-attach recovered {} fact(s) that the ORIGINAL link should already have \
         had: {added:?}. A mid-turn attach owes a follow-up resume when its turn \
         terminalizes, and claim 6c asserts it fires — so anything appearing only now \
         means that follow-up did not happen, and a restart is silently doing work the \
         link is supposed to do for itself.",
        added.len()
    );

    // **The EXACT set the answer can re-derive, computed from the ANSWER.** Not a count
    // taken off the store — that would be checking the store against itself. Every
    // non-usage fact in the store has to be exactly what this answer describes: the
    // session identity, one terminal per turn, and one fact per item of every finished
    // turn.
    let mut derivable_keys: std::collections::BTreeSet<String> =
        [format!("{thread_id}:thread_started")]
            .into_iter()
            .collect();
    for turn in turns_now {
        let tid = turn["id"].as_str().expect("a turn names itself");
        derivable_keys.insert(format!("{thread_id}:turn:{tid}"));
        for item in turn["items"].as_array().expect("a turn carries items") {
            let iid = item["id"].as_str().expect("an item names itself");
            derivable_keys.insert(format!("{thread_id}:item:{iid}"));
        }
    }
    let stored_non_usage: std::collections::BTreeSet<String> = after_third
        .iter()
        .map(|(key, _)| key.clone())
        .filter(|key| !key.contains(":usage:"))
        .collect();
    assert_eq!(
        stored_non_usage, derivable_keys,
        "the store's non-usage facts are not exactly what the answer describes. Every \
         one of them should be re-derivable from this answer — that is what makes a \
         reconnect free — and every fact the answer describes should already be there. A \
         key on one side only is either a fact the link recorded that no answer accounts \
         for, or one the answer describes that never landed."
    );
    let mut keys = std::collections::HashSet::new();
    for (key, _) in &after_third {
        assert!(!key.is_empty(), "every Codex fact carries a dedup key");
        assert!(keys.insert(key.clone()), "duplicate fact {key}");
    }

    // **The arithmetic, honestly.** "Re-derived every fact" would be a nice sentence and
    // a false one: the answer carries no token totals anywhere, so the usage facts are
    // the one thing a re-attach cannot reproduce — by design, because a held total for a
    // turn whose completion was missed is a mid-turn snapshot, and `usage:<turn>` is
    // first-wins, so promoting it would durably shadow the real total rather than leave
    // a gap. The gap is the honest disposition, and it is asserted rather than glossed.
    //
    // The property is read off the ANSWER, not assumed: if the app-server ever started
    // carrying totals in a resume result, this is what would say so.
    let answer_text = two_turn_resume.to_string();
    for token_field in ["tokenUsage", "totalTokens", "inputTokens"] {
        assert!(
            !answer_text.contains(token_field),
            "the resume answer now carries {token_field}. The seeding path deliberately \
             emits no usage fact because an answer was measured to carry none; if it \
             does now, that decision must be re-grounded rather than left as a silent \
             gap"
        );
    }
    let usage_facts: Vec<&(String, u64)> = after_third
        .iter()
        .filter(|(key, _)| key.contains(":usage:"))
        .collect();
    assert!(
        !usage_facts.is_empty(),
        "the LIVE path must have recorded at least one usage fact — the turn the link \
         watched end. Without one, the derivable/underivable split below is vacuous: \
         {after_third:?}"
    );
    let derivable = after_third.len() - usage_facts.len();
    println!(
        "CLAIM 8 PASS — the answer describes both turns ({described:?}); a fresh link \
         (the daemon-restart shape) accepted it and re-derived {derivable} of the {} \
         facts now in the store, writing none of them twice and moving no seq. The \
         other {} are the usage totals, which the answer carries nowhere and which a \
         recovery must NOT invent: a held total is a mid-turn snapshot, and usage:<turn> \
         is first-wins, so a guess there would shadow the truth for ever. It also \
         late-recovered {} item(s) that no earlier attach could have had. Turn one was \
         observed partly live and completed by RECOVERY, turn two wholly by LIVE \
         OBSERVATION, and one answer collapses onto both.",
        after_third.len(),
        usage_facts.len(),
        added.len()
    );

    // --- 13. CLAIM 9: THE THREAD SWITCH, FOLLOWED LIVE ----------------------------
    //
    // The 2e-4c milestone. Everything above happened on ONE thread; a real operator
    // presses `/new`. Three things have to be true at once, and each was measured on the
    // wire before any of it was written:
    //
    //   * the BROKER admits the second `thread/start` as a SWITCH — before 2e-4c its
    //     single-thread invariant refused it, so a real user could not start a new chat
    //     at all;
    //   * the LINK follows, on the connection it already has: `thread/started` for the
    //     new thread is broadcast to every connection (measured), and it is the ONLY
    //     frame about the new thread a connection subscribed elsewhere receives, so
    //     acting on it is the difference between observing the new thread and observing
    //     nothing for the rest of the session;
    //   * the old thread's timeline is INTACT and no fact crosses between them.
    //
    // Asserted on `third` — the link that is currently attached — with no restart.
    let before_switch = recorded(&daemon, &uid);
    let threads_before: std::collections::BTreeSet<String> = before_switch
        .iter()
        .filter_map(|(key, _)| key.split(':').next().map(str::to_string))
        .collect();
    assert_eq!(
        threads_before.len(),
        1,
        "everything so far must belong to ONE thread, or the switch claim below is \
         measuring something that had already happened: {threads_before:?}"
    );
    println!("--- CLAIM 9: pressing /new in the real TUI ---");
    sb.send_keys(&["/new"]);
    tokio::time::sleep(Duration::from_millis(800)).await;
    sb.send_keys(&["Enter"]);

    // The switch itself is observable before any turn: `thread/started` is a fact, and
    // the link records it under the NEW thread's own namespace.
    let switched = wait_until(Duration::from_secs(90), || {
        recorded(&daemon, &uid)
            .iter()
            .any(|(key, _)| !threads_before.iter().any(|t| key.starts_with(t.as_str())))
    })
    .await;
    println!("pane after /new:\n{}", sb.capture_pane());
    print_events(&daemon, &uid, "after /new");
    assert!(
        switched,
        "the link recorded nothing under a new thread after /new. Either the broker \
         refused the second thread/start (the pre-2e-4c single-thread invariant) or the \
         link ignored the announcement and is still watching the old thread. \
         broker.log:\n{}",
        read_file(&sb.run_dir.join("broker.log"))
    );
    let new_thread = recorded(&daemon, &uid)
        .into_iter()
        .filter_map(|(key, _)| key.split(':').next().map(str::to_string))
        .find(|t| !threads_before.contains(t))
        .expect("the switch named a new thread");
    println!(
        "CLAIM 9a PASS — the broker admitted the switch and the link followed it to {new_thread}"
    );

    // --- CLAIM 9b: a turn on the NEW thread is observed LIVE ----------------------
    //
    // Following a switch is only worth anything if the link is SUBSCRIBED to what it
    // followed to. The usage fact is the discriminator again: token totals ride only the
    // notification wire, so a usage fact under the new thread cannot have come from a
    // resume answer.
    sb.send_keys(&["Reply with the single word green and nothing else."]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    sb.send_keys(&["Enter"]);
    let replied = wait_until(Duration::from_secs(150), || {
        sb.capture_pane().to_lowercase().contains("• green")
    })
    .await;
    println!("pane after the post-switch turn:\n{}", sb.capture_pane());
    assert!(
        replied,
        "the post-switch turn never completed in the TUI, so there is no live turn for \
         the link to have observed. If the pane shows a policy refusal, the broker's \
         head-check did not follow the switch. broker.log:\n{}",
        read_file(&sb.run_dir.join("broker.log"))
    );
    let observed = wait_until(Duration::from_secs(90), || {
        recorded(&daemon, &uid)
            .iter()
            .any(|(key, _)| key.starts_with(&new_thread) && key.contains(":usage:"))
    })
    .await;
    print_events(&daemon, &uid, "after the post-switch turn");
    let after_switch = recorded(&daemon, &uid);
    assert!(
        observed,
        "the link recorded no USAGE fact under {new_thread}. A usage total exists only \
         on the notification wire, so its absence means the link followed the switch in \
         name (it learned the id) but was never SUBSCRIBED to the new thread — which is \
         the whole thing the re-targeted resume buys: {after_switch:?}"
    );
    for kind in [":turn:", ":item:"] {
        assert!(
            after_switch
                .iter()
                .any(|(key, _)| key.starts_with(&new_thread) && key.contains(kind)),
            "no {kind} fact under the new thread {new_thread}: {after_switch:?}"
        );
    }
    println!("CLAIM 9b PASS — a turn on the switched-to thread was observed LIVE");

    // --- CLAIM 9c: the old timeline is INTACT and NOTHING crossed -----------------
    //
    // The switch retired a VISIT, not a thread's history (D4). Every fact recorded
    // before the switch must still be present, at the same seq — a switch that quietly
    // renumbered or dropped the old thread's timeline would be worse than one that
    // refused. And every fact in the store must belong to exactly one of the two
    // threads, which is the D4 filter proven on live traffic rather than in a unit test.
    for (key, seq) in &before_switch {
        assert!(
            after_switch.contains(&(key.clone(), *seq)),
            "the pre-switch fact {key} (seq {seq}) is gone or moved after the switch. \
             A switch retires a VISIT; it never retracts a recorded fact."
        );
    }
    let old_thread = threads_before.iter().next().expect("one thread before");
    let mut foreign = Vec::new();
    for (key, _) in &after_switch {
        if !key.starts_with(old_thread.as_str()) && !key.starts_with(&new_thread) {
            foreign.push(key.clone());
        }
    }
    assert!(
        foreign.is_empty(),
        "every fact must be namespaced to one of this session's two threads; these are \
         neither: {foreign:?}"
    );
    let mut seen = std::collections::HashSet::new();
    for (key, _) in &after_switch {
        assert!(
            seen.insert(key.clone()),
            "duplicate fact across the switch: {key}"
        );
    }
    println!(
        "CLAIM 9c PASS — {} pre-switch facts intact at their original seq, {} facts \
         total across exactly two threads ({old_thread} then {new_thread}), zero \
         cross-thread contamination, zero duplicates. The D4 filter is proven on live \
         traffic.",
        before_switch.len(),
        after_switch.len()
    );

    third.abort();
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
