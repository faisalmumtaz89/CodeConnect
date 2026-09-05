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

/// Every live process whose argv names `tag`, with the argv, so a caller can pick
/// one role out of a run's several.
///
/// One `ps` reader for both questions: a sandbox's teardown wants the pids and
/// nothing else, while a gate that has to reach the **broker** — which lives inside
/// the `internal-codex-host` process, beside the TUI and the app-server that carry
/// the same run dir — can only tell them apart by what they were launched as.
fn tagged_processes(tag: &str) -> Vec<(i32, String)> {
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
            (pid != me && cmd.contains(tag)).then_some((pid, cmd.to_string()))
        })
        .collect()
}

fn tagged_pids(tag: &str) -> Vec<i32> {
    tagged_processes(tag)
        .into_iter()
        .map(|(pid, _)| pid)
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
    /// Type one line into the composer and keep pressing Enter until the thing
    /// it was typed for has happened.
    ///
    /// **The retry is measured, not defensive.** A TUI that has painted its
    /// composer is not yet a TUI that acts on a keypress: the first Enter after
    /// a fresh paint is swallowed, the line sits in the composer, and the turn
    /// never starts. Two consecutive runs of the bounce gate died exactly there,
    /// on the warm-up turn, with the pane still showing the splash and the
    /// prompt unsent — which is how a probe that asserted nothing could sit in
    /// the tree looking like coverage. Driving to the *outcome* rather than to a
    /// fixed sleep is the difference between a gate and a coin flip.
    async fn submit_until(&self, line: &str, budget: Duration, done: impl Fn() -> bool) -> bool {
        self.send_keys(&[line]);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            self.send_keys(&["Enter"]);
            if wait_until(Duration::from_secs(5), &done).await {
                return true;
            }
        }
        false
    }

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

    /// Widen the window so a long option row is read whole.
    ///
    /// The pane is 80 columns by default and the TUI clips an option to it, so a
    /// label longer than the row comes back with its tail — and any closing
    /// punctuation — missing. A reader that took that for the label would pin a
    /// string the terminal invented.
    fn widen(&self, columns: u16) {
        let out = Command::new(&self.tmux)
            .args([
                "-S",
                self.sock.to_str().unwrap(),
                "-f",
                "/dev/null",
                "resize-window",
                "-t",
                "cc-live",
                "-x",
                &columns.to_string(),
                "-y",
                "50",
            ])
            .stdin(Stdio::null())
            .output()
            .expect("run tmux resize-window");
        println!(
            "tmux resize-window -x {columns}: status {} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
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

    /// The pane **including its scrollback**.
    ///
    /// [`LiveSandbox::capture_pane`] sees only the visible rows, and a TUI that
    /// answers a keystroke with one line and then streams three hundred more has
    /// pushed that line out of the window before anything can read it — which is
    /// exactly the reading a probe about a REFUSED keystroke must not miss.
    fn capture_pane_history(&self) -> String {
        let out = Command::new(&self.tmux)
            .args([
                "-S",
                self.sock.to_str().unwrap(),
                "-f",
                "/dev/null",
                "capture-pane",
                "-p",
                "-J",
                "-S",
                "-400",
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

// ------------------------------------------------------- the answering harness

/// Both broker legs, and a real `codex --remote` in the pane.
///
/// The premise every answering gate below shares, factored because it is the same
/// four waits every time and a gate that inlined them would be four more chances to
/// wait on the wrong thing.
async fn wait_for_the_broker_and_the_tui(sb: &LiveSandbox) {
    assert!(
        wait_until(Duration::from_secs(60), || sb.broker_legs_bound()).await,
        "the host must bind both broker legs. appserver.stderr:\n{}",
        read_file(&sb.run_dir.join("appserver.stderr.log"))
    );
    assert!(
        wait_until(Duration::from_secs(90), || sb.tui_running()).await,
        "the host must launch the real codex TUI. appserver.stderr:\n{}",
        read_file(&sb.run_dir.join("appserver.stderr.log"))
    );
}

/// **Wait for the composer before typing into it.**
///
/// `tui_running` is satisfied by a process that has not painted yet, and a key sent
/// that early lands in the buffer while the Enter is swallowed — so the prompt sits
/// in the composer for ever and the gate times out somewhere unrelated.
async fn wait_for_a_composer(sb: &LiveSandbox) {
    assert!(
        wait_until(Duration::from_secs(90), || sb
            .capture_pane()
            .contains("Ask Codex to do anything"))
        .await,
        "the TUI never painted a composer. pane:\n{}",
        sb.capture_pane()
    );
}

/// One completed turn, which is what puts a rollout on disk.
///
/// Measured: a thread the TUI has only just created has `turns: []` and no rollout,
/// and `thread/resume` on it is refused outright — so a link that never ran one of
/// these is unsubscribed while looking connected.
async fn run_the_warm_up_turn(sb: &LiveSandbox) {
    assert!(
        sb.submit_until(
            "Reply with the single word amber and nothing else.",
            Duration::from_secs(180),
            || sb.capture_pane().to_lowercase().contains("• amber"),
        )
        .await,
        "the warm-up turn never completed. pane:\n{}",
        sb.capture_pane()
    );
}

/// **Register this run the way a real Codex supervisor does**, and let the daemon
/// build its own link.
///
/// This is what replaced a test-only installer that staked an epoch by hand and
/// handed the daemon an answer channel a test had minted. Everything downstream of
/// a registration was therefore untested here: the agent gate, the identity guards,
/// the generation high-water, the row landing in `codex_sessions`, the epoch stake,
/// the supervisor publish, and — the one that actually carries a phone answer — the
/// link transaction that mints the answer channel and installs it beside the task it
/// belongs to. A gate that installs its own owner proves the answer path works for a
/// session no production frame could have produced.
///
/// The frame is the coordinator's own: `agent: Codex`, the broker's `ccd.sock`, and
/// generation 1 — the literal `supervise_ready_session` sends.
async fn register_the_run(
    daemon: &Arc<crate::state::Daemon>,
    session: &SessionKey,
    sb: &LiveSandbox,
) -> crate::state::Registration {
    register_the_run_on(daemon, session, &sb.ccd_sock()).await
}

/// The same registration, naming a ccd socket of the caller's choosing.
///
/// The two fault gates put a [`GatedCcdLeg`] in front of the broker's own leg and
/// register the run onto **that**, which is how they hold the broker's replies
/// without touching the daemon, the link or the broker. Everything else here is
/// identical, including that the daemon is the one that builds the link.
async fn register_the_run_on(
    daemon: &Arc<crate::state::Daemon>,
    session: &SessionKey,
    socket: &Path,
) -> crate::state::Registration {
    let (tx, rx) = tokio::sync::mpsc::channel::<protocol::ipc::DaemonFrame>(
        protocol::config::Config::default().ipc_write_queue,
    );
    // The daemon writes to this channel for the life of the session; a receiver that
    // dropped would turn every such write into an error about a supervisor that is
    // still, as far as this test is concerned, connected.
    Box::leak(Box::new(rx));
    daemon
        .register_supervisor(
            protocol::ipc::RegisterSession {
                session_id: session.name.clone(),
                session_uid: Some(session.uid.clone()),
                tmux_session: session.name.clone(),
                tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
                cwd: "/tmp".into(),
                supervisor_pid: std::process::id(),
                claude_bin: None,
                agent: protocol::agent::AgentKind::Codex,
                agent_bin: None,
                // The launch case: `thread/started` has not been seen yet, so the
                // registration claims no thread and the link learns one off the wire.
                codex_thread_id: None,
                codex_socket: Some(socket.to_string_lossy().into_owned()),
                codex_generation: Some(1),
                started_at: protocol::time::now_rfc3339(),
                protocol_minor: protocol::PROTOCOL_MINOR,
                exit_replay: false,
            },
            tx,
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        )
        .await
        .expect("a real Codex coordinator's registration frame must be accepted")
}

/// **The broker's own word about what became of a `ccd` leg's response.**
///
/// `relay.rs` writes one of these per response a `ccd` leg sends, in the same arm
/// that forwards it, so counting them counts responses written upstream — which is
/// how "the app-server was not sent a second answer" becomes a measurement rather
/// than an inference. Verbatim from `relay.rs`'s own `format!`.
const CCD_DISPOSITION_NOTE: &str = "Ccd: response disposition";

fn ccd_dispositions(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|line| line.contains(CCD_DISPOSITION_NOTE))
        .collect()
}

/// The `winner=` the broker recorded on one disposition line, as it spelled it.
///
/// `Option<Role>`'s `Debug`, so the three readings are `None`, `Some(Tui)` and
/// `Some(Ccd)` — and the first is not a fourth kind of loss but the broker
/// declining to name anybody, which is exactly the case a loser's sentence may not
/// describe as "answered at the Mac".
fn disposition_winner(line: &str) -> Option<String> {
    let at = line.find("winner=")? + "winner=".len();
    Some(line[at..].split_whitespace().next()?.to_string())
}

fn disposition_delivered(line: &str) -> Option<bool> {
    let at = line.find("delivered=")? + "delivered=".len();
    line[at..].split_whitespace().next()?.parse().ok()
}

/// **The `winner=` spellings a loser's sentence is allowed to be derived from**, and
/// the sentence each one earns.
///
/// Read off `settle_lost_answer`, which is the production code under test: only a
/// `tui` winner may be described as answered at the Mac, only a `ccd` winner as
/// answered through another daemon, and a disposition that names nobody gets the
/// sentence that claims nothing. The table is here so the race gate can check the
/// phone's sentence against the broker's log rather than against itself.
const LOSER_SENTENCE: [(&str, &str); 3] = [
    (
        "Some(Tui)",
        "this card was answered at the Mac first, so nothing you chose was applied",
    ),
    (
        "Some(Ccd)",
        "this card was answered through another CodeConnect daemon first, so nothing \
         you chose was applied",
    ),
    (
        "None",
        "something else answered this card first, so nothing you chose was applied",
    ),
];

/// **The sentence `settle_lost_answer` owes a loser, given what the broker named.**
///
/// Runs in the ordinary suite, because the mapping is the claim and it needs no
/// codex. **Mutation:** make a missing winner read as the Mac's and the third case
/// goes red — which is the exact dishonesty the race gate exists to catch, since
/// three of the four ways a response can fail to land are not the keyboard.
#[test]
fn only_a_named_tui_winner_earns_the_answered_at_the_mac_sentence() {
    let sentence = |line: &str| {
        let named = disposition_winner(line).expect("a disposition line names a winner field");
        LOSER_SENTENCE
            .iter()
            .find(|(spelling, _)| *spelling == named)
            .map(|(_, sentence)| *sentence)
    };
    assert_eq!(
        sentence("Ccd: response disposition delivered=false winner=Some(Tui) id=Int(3) (conn 5)"),
        Some(LOSER_SENTENCE[0].1)
    );
    assert_eq!(
        sentence("Ccd: response disposition delivered=false winner=Some(Ccd) id=Int(3) (conn 5)"),
        Some(LOSER_SENTENCE[1].1)
    );
    assert_eq!(
        sentence("Ccd: response disposition delivered=false winner=None id=Int(3) (conn 5)"),
        Some(LOSER_SENTENCE[2].1),
        "a disposition that names nobody must NOT be describable as the Mac's answer: \
         a response can also lose to another daemon, to a capability it never held, or \
         to a socket that died"
    );
    assert_eq!(
        disposition_delivered(
            "Ccd: response disposition delivered=true winner=None id=Int(9) (conn 2)"
        ),
        Some(true)
    );
    assert_eq!(
        disposition_delivered(
            "Ccd: response disposition delivered=false winner=Some(Tui) id=Int(9) (conn 2)"
        ),
        Some(false)
    );
    assert_eq!(
        ccd_dispositions("Ccd: leg opened (conn 1)\nTui: forward (turn/start)\n").len(),
        0,
        "a log with no response in it counts none"
    );
}

/// **A byte-for-byte pipe in front of the broker's ccd leg, with a valve on what
/// comes back.**
///
/// The one contrivance in the two fault gates below, and it is here — where the
/// reasoning can be read — rather than spread through them.
///
/// The window a restart has to be caught in is the interval between the link taking
/// its durable claim and the broker telling it what became of the response. In
/// production that is one relay-loop arm — `relay.rs` composes the disposition in the
/// very arm that forwarded the answer — plus however long the app-server takes to
/// accept the write, which on a healthy socket is microseconds and is bounded above by
/// `relay::UPSTREAM_WRITE_BUDGET` (10 s). Nothing in this build may widen it, because a
/// production hook that made the daemon pause there would be machinery existing only to
/// be tested. So the *transport* is what holds still.
///
/// **Downstream only, and that is the whole design.** The link→broker direction is
/// never held, so the answer really leaves the daemon, really reaches the broker, and
/// really actuates — which is what makes "after the write" a fact about the run
/// rather than a description of the staging. Only the broker→link direction is held,
/// which stages exactly one thing: *no answer came back*. That is a state the real
/// world produces — a Mac that lost power between writing and reading, a broker that
/// died, a socket that stalled — and it is staged without asking any code under test
/// to behave differently. The link's socket stays open throughout, so there is no EOF
/// and no error: from the link's side this is simply a wire that has gone quiet.
///
/// **It does not hold for ever, and must not be relied on to.** `codex_link`'s
/// `DISPOSITION_BUDGET` is what a link does about a wire that went quiet, and it is
/// 750 ms under `cfg(test)` — so a claim nobody has settled becomes terminal *in this
/// process* three quarters of a second later, which is a different ending from the
/// one these gates are about. The valve turns a two-millisecond coin flip into three
/// quarters of a second of room; the gates still poll for the claim and still fail
/// loudly if they miss it.
///
/// **The broker never notices.** Only the broker→link direction is held, and the
/// broker's own bound is on the app-server WRITE, which this valve is downstream of —
/// so the disposition is composed on time, queued into a pipe this process is holding,
/// and `UPSTREAM_WRITE_BUDGET` is never reached. What the link sees is silence, which
/// is the whole staging.
///
/// **Why not a signal.** Measured on this platform: `SIGSTOP` sent to a `tmux` pane's
/// own child returns success and does nothing at all — the process stays `S`,
/// keeps running, and the broker answers as though nothing happened. A staging built
/// on it would silently measure the ordinary delivered path while claiming to measure
/// a fault, which is the failure mode a gate exists to prevent rather than to have.
struct GatedCcdLeg {
    path: PathBuf,
    /// While false, every byte the broker sends is held in this process instead of
    /// being handed to the link. Never affects the other direction.
    delivering: Arc<std::sync::atomic::AtomicBool>,
    accepting: tokio::task::JoinHandle<()>,
}

impl GatedCcdLeg {
    async fn in_front_of(sb: &LiveSandbox) -> GatedCcdLeg {
        let path = sb.base.join(format!("gated-ccd.{}.sock", nanos()));
        let listener = tokio::net::UnixListener::bind(&path)
            .unwrap_or_else(|e| panic!("bind the gated ccd leg at {}: {e}", path.display()));
        let upstream = sb.ccd_sock();
        let delivering = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let valve = Arc::clone(&delivering);
        let accepting = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let Ok(broker) = UnixStream::connect(&upstream).await else {
                    return;
                };
                tokio::spawn(GatedCcdLeg::pipe(client, broker, Arc::clone(&valve)));
            }
        });
        GatedCcdLeg {
            path,
            delivering,
            accepting,
        }
    }

    /// One connection's two halves. Upstream is an unconditional copy; downstream
    /// reads first and only then asks the valve, so a frame that was already in
    /// flight when the valve closed is held rather than raced through.
    async fn pipe(
        client: UnixStream,
        broker: UnixStream,
        valve: Arc<std::sync::atomic::AtomicBool>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut from_link, mut to_link) = client.into_split();
        let (mut from_broker, mut to_broker) = broker.into_split();
        let up = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut from_link, &mut to_broker).await;
        });
        let down = tokio::spawn(async move {
            let mut buf = [0u8; 16 * 1024];
            loop {
                let read = match from_broker.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                while !valve.load(std::sync::atomic::Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                if to_link.write_all(&buf[..read]).await.is_err() {
                    break;
                }
            }
        });
        let _ = up.await;
        let _ = down.await;
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Hold everything the broker says from here on. Called **before** the answer, so
    /// no disposition can be in flight ahead of it.
    fn hold(&self) {
        self.delivering
            .store(false, std::sync::atomic::Ordering::SeqCst);
        println!("GATED CCD LEG — the broker's replies are held from here");
    }

    fn release(&self) {
        self.delivering
            .store(true, std::sync::atomic::Ordering::SeqCst);
        println!("GATED CCD LEG — the broker's replies flow again");
    }

    fn close(self) {
        self.accepting.abort();
    }
}

/// **A real phone: the daemon's own `ws_server`, on loopback, over a real WebSocket.**
///
/// Plan Phase 3's gates are written "WS test client", and the difference from calling
/// [`crate::state::Daemon::answer`] directly is not decoration. An answer that arrives
/// this way is decoded from bytes by `protocol::ws::ClientMessage`, admitted by the
/// handshake, dispatched by `ws_server`'s own match arm, and answered with a
/// `ServerMessage::AnswerResult` the phone has to be able to decode — four seams a
/// direct call skips, each of which has shipped a bug before. The daemon underneath is
/// the same one the link is installed on, so the card, the claim and the terminal are
/// all the production ones.
///
/// **Loopback, so no TLS and no pairing.** `plaintext_trust` classifies 127.0.0.1 as
/// [`crate::ws_server::PlaintextTrust::TrustedPath`] — the bytes never leave the
/// machine — and `Config::default()` does not set `tls_required`, so the server admits
/// a plaintext connection on its own production rules rather than on a test switch. The
/// credential is this daemon's static bootstrap token, which is what
/// `Daemon::authenticate` compares first and what an unpaired client legitimately
/// carries; answering is not one of the shell-equivalent verbs that require a paired
/// device, so nothing here is reached by a shortcut a real phone could not take.
struct PhoneOverTheWire {
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    serving: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl PhoneOverTheWire {
    /// Serve this daemon on a loopback port, connect, and complete the `hello`.
    async fn connect(daemon: &Arc<crate::state::Daemon>) -> PhoneOverTheWire {
        // **The port is claimed by holding it and then letting it go.** `serve` binds
        // the address itself — that bind is part of what is under test — so the only
        // way to learn a free one is to have owned it a moment earlier. A listener
        // that never accepted leaves no TIME_WAIT behind, so the rebind below is not
        // racing a socket in teardown; a port somebody else takes in the gap fails
        // the bind loudly rather than silently serving somewhere else.
        let probe = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("claim a loopback port");
        let addr = probe.local_addr().expect("the claimed port");
        drop(probe);

        let token = Arc::new(format!("live-gate-bootstrap-{}", nanos()));
        let serving = {
            let daemon = Arc::clone(daemon);
            let token = Arc::clone(&token);
            tokio::spawn(async move {
                crate::ws_server::serve(
                    daemon,
                    addr,
                    token,
                    None,
                    crate::ws_server::PlaintextTrust::TrustedPath,
                )
                .await
            })
        };

        let mut stream = None;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(open) => {
                    stream = Some(open);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        let stream = stream.unwrap_or_else(|| panic!("the daemon's ws_server never bound {addr}"));
        let (mut ws, _) = tokio_tungstenite::client_async(format!("ws://{addr}/"), stream)
            .await
            .expect("the daemon's own WebSocket handshake");

        let hello = serde_json::json!({
            "type": "hello",
            "protocol_version": protocol::PROTOCOL_VERSION,
            "token": token.as_str(),
            "client_name": "codex-live-gate",
        });
        println!("PHONE-> {hello}");
        ws.send(Message::Text(hello.to_string()))
            .await
            .expect("write the hello");
        let mut phone = PhoneOverTheWire { ws, serving };
        let ack = phone
            .read_until(Duration::from_secs(20), |frame| {
                frame["type"].as_str() == Some("hello_ack")
            })
            .await
            .expect("the daemon must answer a hello on its own listener");
        assert_eq!(
            ack["protocol_version"].as_u64(),
            Some(u64::from(protocol::PROTOCOL_VERSION)),
            "the ack must be for the protocol this client spoke: {ack}"
        );
        phone
    }

    /// Read server frames until one satisfies `want`, printing every one.
    async fn read_until(
        &mut self,
        budget: Duration,
        want: impl Fn(&Value) -> bool,
    ) -> Option<Value> {
        let deadline = Instant::now() + budget;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            let next = tokio::time::timeout(left, self.ws.next()).await.ok()??;
            let Ok(Message::Text(text)) = next else {
                continue;
            };
            println!("PHONE<- {}", frame_preview(&text, 2000));
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if want(&frame) {
                return Some(frame);
            }
        }
    }

    /// **The tap.** Sends the `answer` a phone sends and decodes the
    /// `answer_result` a phone decodes.
    async fn answer(
        &mut self,
        card: &protocol::ws::ApprovalCard,
        option_id: &str,
        uid: &str,
        budget: Duration,
    ) -> protocol::ws::AnswerResult {
        let frame = serde_json::json!({
            "type": "answer",
            "request_id": card.request_id,
            "payload_hash": card.payload_hash,
            "decision": {"type": "option_id", "option_id": option_id},
            "session_id": uid,
        });
        println!("PHONE-> {frame}");
        self.ws
            .send(Message::Text(frame.to_string()))
            .await
            .expect("write the answer");
        let request_id = card.request_id.clone();
        let reply = self
            .read_until(budget, |frame| {
                frame["type"].as_str() == Some("answer_result")
                    && frame["request_id"].as_str() == Some(request_id.as_str())
            })
            .await
            .unwrap_or_else(|| panic!("no answer_result for {request_id} within {budget:?}"));
        serde_json::from_value(reply["result"].clone())
            .expect("the phone must be able to decode the daemon's answer_result")
    }

    /// **The tap, without waiting for the verdict.**
    ///
    /// A phone answering a card whose disposition is being held gets no `answer_result`
    /// until the daemon's own `DISPOSITION_BUDGET` expires — 15 s in a real `ccd`
    /// process, which is longer than the window gate 5 has to kill it in. This writes the
    /// frame and leaves; what the daemon did with it is read out of its database.
    async fn send_answer(&mut self, card: &protocol::ws::ApprovalCard, option_id: &str, uid: &str) {
        let frame = serde_json::json!({
            "type": "answer",
            "request_id": card.request_id,
            "payload_hash": card.payload_hash,
            "decision": {"type": "option_id", "option_id": option_id},
            "session_id": uid,
        });
        println!("PHONE-> {frame}");
        self.ws
            .send(Message::Text(frame.to_string()))
            .await
            .expect("write the answer");
    }

    fn close(self) {
        self.serving.abort();
    }
}

/// **A phone on a daemon this process does not host**: the real `ccd` child's own
/// loopback listener, with the credential it minted into its own root.
///
/// [`PhoneOverTheWire::connect`] serves an in-process daemon and then dials it; there is
/// no daemon to serve here, so this dials what the child already bound. Everything after
/// the connect — the `hello`, the ack, the `answer` frame, the `answer_result` — is the
/// same code path, because it is the same struct.
impl PhoneOverTheWire {
    async fn connect_to(addr: std::net::SocketAddr, token: &str) -> PhoneOverTheWire {
        let stream = tokio::net::TcpStream::connect(addr)
            .await
            .unwrap_or_else(|e| panic!("dial the ccd child at {addr}: {e}"));
        let (mut ws, _) = tokio_tungstenite::client_async(format!("ws://{addr}/"), stream)
            .await
            .expect("the child's own WebSocket handshake");
        let hello = serde_json::json!({
            "type": "hello",
            "protocol_version": protocol::PROTOCOL_VERSION,
            "token": token,
            "client_name": "codex-live-gate",
        });
        println!("PHONE-> {hello}");
        ws.send(Message::Text(hello.to_string()))
            .await
            .expect("write the hello");
        // Nothing to abort: this process serves nothing. A completed future stands in
        // for the handle the in-process constructor holds.
        let mut phone = PhoneOverTheWire {
            ws,
            serving: tokio::spawn(async { Ok(()) }),
        };
        phone
            .read_until(Duration::from_secs(20), |frame| {
                frame["type"].as_str() == Some("hello_ack")
            })
            .await
            .expect("the ccd child must answer a hello on its own listener");
        phone
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
        crate::codex_link::answer_channel().1,
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
        crate::codex_link::answer_channel().1,
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

// ============================================================ MEASUREMENT PROBE
//
// Chunk 3a grounding, NOT a gate. It exists to answer four questions the design
// cannot be written without, and it answers them by reading the wire rather than
// by reasoning about it:
//
//   1. Does the app-server deliver `*/requestApproval` on the **ccd** upstream at
//      all, and does it require a subscription? (Two connections, one resumed and
//      one merely initialized, are tapped side by side.)
//   2. What does 0.153 actually put in `availableDecisions` — Phase 0 measured
//      exactly `[accept, acceptWithExecpolicyAmendment, cancel]` on 0.147, while
//      the schema admits six.
//   3. What retirement signal reaches ccd when the keyboard answers.
//   4. What a `fileChange` request carries, which has no `availableDecisions` at all.
//
// Delete once the answers are pinned in fixtures and gates.

/// Every frame one ccd connection was handed, verbatim, with the write half kept
/// so the connection can still be driven and barrier-probed.
struct WireTap {
    tx: futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<UnixStream>, Message>,
    frames: Arc<Mutex<Vec<Value>>>,
    handle: tokio::task::JoinHandle<()>,
    label: &'static str,
    next_id: i64,
}

impl WireTap {
    /// Split a connection that has already completed `initialize`/`initialized`.
    fn split(raw: RawCcd, label: &'static str) -> WireTap {
        let frames = Arc::new(Mutex::new(Vec::<Value>::new()));
        let (tx, mut rx) = raw.ws.split();
        let sink = Arc::clone(&frames);
        let handle = tokio::spawn(async move {
            while let Some(Ok(msg)) = rx.next().await {
                if let Message::Text(text) = msg {
                    println!("[{label}] {}", frame_preview(&text, 4000));
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        sink.lock().expect("wire tap sink").push(v);
                    }
                }
            }
            println!("[{label}] closed");
        });
        WireTap {
            tx,
            frames,
            handle,
            label,
            next_id: 7000,
        }
    }

    async fn send(&mut self, frame: Value) {
        println!("[{}] -> {frame}", self.label);
        self.tx
            .send(Message::Text(frame.to_string()))
            .await
            .expect("write to the tapped ccd leg");
    }

    fn seen(&self) -> Vec<Value> {
        self.frames.lock().expect("wire tap sink").clone()
    }

    /// Close this leg for real, the way a daemon bounce closes one.
    ///
    /// Aborting the reader alone is not enough: the task owns the read half, so
    /// the socket stays open and the app-server still counts this connection as
    /// present. Aborting drops the read half and returning drops the write half,
    /// and only then has the connection gone away.
    fn close(self) -> Vec<Value> {
        let seen = self.seen();
        self.handle.abort();
        seen
    }

    /// Every `method` this connection has been handed, in order.
    fn methods(&self) -> Vec<String> {
        self.seen()
            .iter()
            .filter_map(|v| v["method"].as_str().map(str::to_string))
            .collect()
    }

    /// The first frame carrying this method, if any.
    fn first(&self, method: &str) -> Option<Value> {
        self.seen()
            .into_iter()
            .find(|v| v["method"].as_str() == Some(method))
    }

    /// A round trip on this very connection, so a zero-count is a fact about the
    /// wire rather than about a corpse. Same barrier discipline as
    /// [`Observer::probe`]: everything the server had for this connection is in
    /// the sink once the answer to this id lands.
    async fn barrier(&mut self, budget: Duration) -> bool {
        let id = self.next_id;
        self.next_id += 1;
        self.send(serde_json::json!({"id": id, "method": "thread/loaded/list", "params": {}}))
            .await;
        let frames = Arc::clone(&self.frames);
        wait_until(budget, || {
            frames
                .lock()
                .expect("wire tap sink")
                .iter()
                .any(|v| v.get("id").and_then(Value::as_i64) == Some(id) && v["method"].is_null())
        })
        .await
    }
}

/// Drive one real turn that must ask for a command approval, and read what the
/// ccd leg was handed while it did.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn measure_the_approval_wire_on_the_ccd_leg() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("appr");
    let mut coord = sb.spawn_coordinator(&codex);

    assert!(
        wait_until(Duration::from_secs(60), || sb.broker_legs_bound()).await,
        "the host must bind both broker legs. appserver.stderr:\n{}",
        read_file(&sb.run_dir.join("appserver.stderr.log"))
    );
    assert!(
        wait_until(Duration::from_secs(90), || sb.tui_running()).await,
        "the host must launch the real codex TUI. appserver.stderr:\n{}",
        read_file(&sb.run_dir.join("appserver.stderr.log"))
    );

    // The control comparison: a ccd connection that completes the handshake and
    // then subscribes to NOTHING. If an approval reaches this one too, delivery
    // does not depend on the resume.
    let unsubscribed = {
        let mut raw = RawCcd::connect(&sb.ccd_sock()).await;
        let init = raw.initialize().await;
        assert!(
            init["result"].is_object(),
            "unsubscribed initialize: {init}"
        );
        raw.notify("initialized", serde_json::json!({})).await;
        WireTap::split(raw, "UNSUB")
    };

    // The subject: a ccd connection in exactly the production link's position —
    // initialized, then resumed onto the session's own thread.
    let mut raw = RawCcd::connect(&sb.ccd_sock()).await;
    let init = raw.initialize().await;
    assert!(init["result"].is_object(), "subscribed initialize: {init}");
    raw.notify("initialized", serde_json::json!({})).await;

    // **One harmless turn first, and it is a premise rather than a warm-up.**
    // MEASURED: a thread the TUI has only just created has `turns: []` and no
    // rollout file yet, and `thread/resume` on it is refused outright —
    // `{"error":{"code":-32600,"message":"no rollout found for thread id …"}}`.
    // A leg that swallowed that error would be unsubscribed while looking
    // connected, and every frame it then failed to receive would read as a fact
    // about the app-server instead of a fact about this harness. Driving a turn
    // to completion is what puts a rollout on disk for the resume to find.
    // **Wait for the composer before typing into it.** `tui_running` is satisfied
    // by a process that has not painted yet: measured, keys sent that early land
    // in the buffer but the Enter is swallowed by the still-starting TUI, and the
    // prompt then sits in the composer for ever. The footer is the readiness
    // signal the other gates in this file use.
    assert!(
        wait_until(Duration::from_secs(90), || sb
            .capture_pane()
            .contains("Ask Codex to do anything"))
        .await,
        "the TUI never painted a composer to type into. pane:\n{}",
        sb.capture_pane()
    );

    sb.send_keys(&["Reply with the single word amber and nothing else."]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    sb.send_keys(&["Enter"]);
    assert!(
        wait_until(Duration::from_secs(180), || sb
            .capture_pane()
            .to_lowercase()
            .contains("• amber"))
        .await,
        "the warm-up turn never completed, so no rollout exists to resume onto. pane:\n{}",
        sb.capture_pane()
    );

    // Learn the live thread the way the link does: ask, do not guess.
    let mut thread_id = String::new();
    for _ in 0..60 {
        let loaded = raw
            .request(
                "thread/loaded/list",
                serde_json::json!({}),
                Duration::from_secs(20),
            )
            .await;
        // MEASURED shape: `{"result":{"data":["<threadId>", …],"nextCursor":null}}`
        // — a flat array of id strings, not objects.
        if let Some(id) = loaded["result"]["data"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_str)
        {
            thread_id = id.to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        !thread_id.is_empty(),
        "no loaded thread to resume onto; the TUI never started one. pane:\n{}",
        sb.capture_pane()
    );
    println!("MEASURED thread_id = {thread_id}");

    let mut subscribed = WireTap::split(raw, "SUB");
    // Retried the way the production link retries, and then ASSERTED. The whole
    // question this probe exists to answer is what a subscribed ccd leg is
    // handed, so an unsubscribed leg must fail the run rather than answer it.
    let mut resumed = false;
    for attempt in 0..12 {
        let id = 500 + attempt;
        subscribed
            .send(serde_json::json!({
                "id": id, "method": "thread/resume", "params": {"threadId": thread_id}
            }))
            .await;
        let frames = Arc::clone(&subscribed.frames);
        wait_until(Duration::from_secs(10), || {
            frames
                .lock()
                .expect("wire tap sink")
                .iter()
                .any(|v| v.get("id").and_then(Value::as_i64) == Some(id))
        })
        .await;
        let answer = subscribed
            .seen()
            .into_iter()
            .find(|v| v.get("id").and_then(Value::as_i64) == Some(id));
        match answer {
            Some(v) if v.get("result").is_some() => {
                println!("MEASURED thread/resume answered: {v}");
                resumed = true;
                break;
            }
            Some(v) => println!("resume attempt {attempt} refused: {v}"),
            None => println!("resume attempt {attempt} unanswered"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(
        resumed,
        "the ccd leg never subscribed, so nothing it fails to receive is evidence \
         about the app-server. broker.log:\n{}",
        read_file(&sb.run_dir.join("broker.log"))
    );
    assert!(
        subscribed.barrier(Duration::from_secs(30)).await,
        "the subscribed leg must answer a barrier after its resume"
    );

    // ---- the turn that must ask ------------------------------------------
    // The session's sandbox is read-only and its approval policy is on-request,
    // so any write at all has to be asked for.
    sb.send_keys(&[
        "Run the shell command `touch /tmp/cc-approval-probe.txt` now. Do not explain, just run it.",
    ]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    sb.send_keys(&["Enter"]);

    let asked = wait_until(Duration::from_secs(180), || {
        subscribed
            .methods()
            .iter()
            .any(|m| m.ends_with("/requestApproval"))
    })
    .await;
    println!("pane at the approval:\n{}", sb.capture_pane());

    println!("SUB methods: {:?}", subscribed.methods());
    println!("UNSUB methods: {:?}", unsubscribed.methods());

    assert!(
        asked,
        "no approval reached the subscribed ccd leg in 180s. pane:\n{}\nbroker.log:\n{}",
        sb.capture_pane(),
        read_file(&sb.run_dir.join("broker.log"))
    );

    let request = subscribed
        .first("item/commandExecution/requestApproval")
        .expect("the command-execution approval frame");
    println!(
        "MEASURED requestApproval VERBATIM:\n{}",
        serde_json::to_string_pretty(&request).expect("pretty")
    );
    println!(
        "MEASURED availableDecisions = {}",
        request["params"]["availableDecisions"]
    );

    // Q1, answered against the control — and now ASSERTED, because the whole
    // reason the observer sits behind a subscription is that an unsubscribed leg
    // is handed nothing to observe.
    let unsubscribed_approvals = unsubscribed
        .methods()
        .iter()
        .filter(|m| m.ends_with("/requestApproval"))
        .count();
    println!("MEASURED unsubscribed-leg approval count = {unsubscribed_approvals}");
    assert_eq!(
        unsubscribed_approvals,
        0,
        "an approval reached a connection that resumed NOTHING. Delivery would then \
         not depend on the subscription, and the whole shape of the observer — one \
         card per subscribed visit, filtered by thread — would be built on a premise \
         the wire had stopped honouring. unsubscribed saw: {:?}",
        unsubscribed.methods()
    );

    // ---- THE GATE: the real parser, on the real frame ---------------------
    //
    // This is what turns the probe above into something that keeps working. Every
    // fact `codex_approval` reads is read here from the frame the live 0.153
    // app-server just sent, by the production code, and the card that comes out
    // is checked against the two gates the phone applies before it will draw a
    // button. A codex release that moves any of it fails here rather than
    // silently putting an unreadable card on somebody's phone.
    let params = &request["params"];
    let approval =
        crate::codex_approval::Approval::read(crate::codex_approval::Family::Command, params, None)
            .expect("the production parser must read a live 0.153 command approval");

    // The option set is the WIRE's, and this is the assertion that says the
    // schema's stable/experimental split is not the runtime frame:
    // `availableDecisions` is declared only in the experimental bundle and
    // arrives here, populated, on a stable launch with no `--experimental`.
    assert!(
        params["availableDecisions"].is_array(),
        "0.153 stopped sending availableDecisions on the stable wire; the command \
         family would then have no option set to read and every card would be \
         refused. Got: {}",
        params["availableDecisions"]
    );
    let offered: Vec<&str> = approval
        .choices
        .iter()
        .map(|choice| choice.id.as_str())
        .collect();
    assert_eq!(
        offered,
        ["accept", "acceptWithExecpolicyAmendment", "cancel"],
        "the live option set moved. Three of the schema's six, measured on 0.147 and \
         again here; a new one arriving is not a failure of this build so much as a \
         label this build has not measured yet — add it to `Family::labels` with the \
         pane that shows what the TUI calls it, never by guessing."
    );
    assert_eq!(
        approval.choices[1].payload,
        Some(params["availableDecisions"][1]["acceptWithExecpolicyAmendment"].clone()),
        "the amendment the server proposed must ride the card verbatim, or an answer \
         could name one the server never offered"
    );

    let request_id = approval
        .request_id("01K1B3XQ8ZC0DE5FGH7JKMNPCX", 1)
        .expect("a live item id must fit the composite id's bounds");
    let card = approval.card(request_id, 1);
    println!(
        "MEASURED the card this build raises from that frame:\n{}",
        serde_json::to_string_pretty(&card).expect("pretty")
    );

    // Gate one: the app decodes five keys non-optionally, and a card missing any
    // of them renders as "Approval request could not be read".
    let wire = serde_json::to_value(&card).expect("the card must serialize");
    for required in [
        "request_id",
        "payload_hash",
        "tool_name",
        "tool_input",
        "display_text",
    ] {
        assert!(
            wire.get(required).is_some_and(|value| !value.is_null()),
            "the phone cannot decode a card without a non-null {required}: {wire}"
        );
    }
    // Gate two: `CardVerification.swift` recomputes this and replaces the card
    // with a banner — killing both actions — when it disagrees.
    assert_eq!(
        card.payload_hash,
        protocol::hash::sha256_hex(card.display_text.as_bytes()),
        "a card built from the live wire must still hash to its own display text"
    );
    assert_eq!(
        card.display_text,
        format!("{}\n{}", card.tool_name, card.tool_input),
        "and must still re-render as the app re-renders it"
    );
    // And the live command really is on the card, under the key the phone's
    // `principalArgument` reads first.
    assert_eq!(card.tool_input["command"], params["command"]);
    assert_eq!(card.tool_input["cwd"], params["cwd"]);
    println!("GATE PASS — the live 0.153 approval became a card the phone can verify");

    // ---- answer it at the keyboard, and watch the retirement --------------
    sb.send_keys(&["Enter"]);
    let resolved = wait_until(Duration::from_secs(60), || {
        subscribed
            .methods()
            .iter()
            .any(|m| m == "serverRequest/resolved")
    })
    .await;
    println!("pane after the keyboard answer:\n{}", sb.capture_pane());
    println!(
        "MEASURED serverRequest/resolved = {:?}",
        subscribed.first("serverRequest/resolved")
    );
    println!("MEASURED resolved-observed = {resolved}");
    println!("SUB methods after the answer: {:?}", subscribed.methods());

    let _ = subscribed.barrier(Duration::from_secs(30)).await;
    println!(
        "FINAL SUB methods: {:?}\nFINAL UNSUB methods: {:?}",
        subscribed.methods(),
        unsubscribed.methods()
    );

    subscribed.handle.abort();
    unsubscribed.handle.abort();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file("/tmp/cc-approval-probe.txt");
}
/// **MEASUREMENT: what does the app-server accept as the response to a
/// `requestApproval`, and what does the broker do with it?**
///
/// Nothing downstream of this can be designed honestly until three things are
/// facts rather than readings of a schema bundle that declares no `result` at
/// all for these server-requests: (a) the exact accepted response envelope,
/// (b) whether a response sent by the ccd leg actually actuates the command,
/// and (c) what the TUI's pane does when it does.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn measure_a_ccd_leg_answering_a_command_approval() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("ans");
    let mut coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3b-answer-probe.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);

    assert!(
        wait_until(Duration::from_secs(60), || sb.broker_legs_bound()).await,
        "the host must bind both broker legs. appserver.stderr:\n{}",
        read_file(&sb.run_dir.join("appserver.stderr.log"))
    );
    assert!(
        wait_until(Duration::from_secs(90), || sb.tui_running()).await,
        "the host must launch the real codex TUI"
    );

    let mut raw = RawCcd::connect(&sb.ccd_sock()).await;
    let init = raw.initialize().await;
    assert!(init["result"].is_object(), "initialize: {init}");
    raw.notify("initialized", serde_json::json!({})).await;

    assert!(
        wait_until(Duration::from_secs(90), || sb
            .capture_pane()
            .contains("Ask Codex to do anything"))
        .await,
        "the TUI never painted a composer. pane:\n{}",
        sb.capture_pane()
    );
    assert!(
        sb.submit_until(
            "Reply with the single word amber and nothing else.",
            Duration::from_secs(180),
            || sb.capture_pane().to_lowercase().contains("• amber"),
        )
        .await,
        "the warm-up turn never completed. pane:\n{}",
        sb.capture_pane()
    );

    let mut thread_id = String::new();
    for _ in 0..60 {
        let loaded = raw
            .request(
                "thread/loaded/list",
                serde_json::json!({}),
                Duration::from_secs(20),
            )
            .await;
        if let Some(id) = loaded["result"]["data"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_str)
        {
            thread_id = id.to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(!thread_id.is_empty(), "no loaded thread to resume onto");

    let mut sub = WireTap::split(raw, "SUB");
    let mut resumed = false;
    for attempt in 0..12 {
        let id = 500 + attempt;
        sub.send(serde_json::json!({
            "id": id, "method": "thread/resume", "params": {"threadId": thread_id}
        }))
        .await;
        let frames = Arc::clone(&sub.frames);
        wait_until(Duration::from_secs(10), || {
            frames
                .lock()
                .expect("wire tap sink")
                .iter()
                .any(|v| v.get("id").and_then(Value::as_i64) == Some(id))
        })
        .await;
        if sub
            .seen()
            .into_iter()
            .any(|v| v.get("id").and_then(Value::as_i64) == Some(id) && v.get("result").is_some())
        {
            resumed = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(resumed, "the ccd leg never subscribed");
    assert!(sub.barrier(Duration::from_secs(30)).await);

    // ---- provoke one command approval ------------------------------------
    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(180),
            || sub
                .methods()
                .iter()
                .any(|m| m.ends_with("/requestApproval")),
        )
        .await,
        "no approval reached the ccd leg. pane:\n{}",
        sb.capture_pane()
    );

    let request = sub
        .first("item/commandExecution/requestApproval")
        .expect("the command-execution approval frame");
    let wire_id = request["id"]
        .as_i64()
        .expect("a server-request carries a numeric id");
    println!("MEASURED wire id to answer on = {wire_id}");
    println!("PANE BEFORE THE ANSWER:\n{}", sb.capture_pane());
    assert!(
        !std::path::Path::new(&marker).exists(),
        "the command ran before anybody answered; the probe would prove nothing"
    );

    // ---- THE MEASUREMENT --------------------------------------------------
    // The shape under test, taken from the only place it is written down:
    // `codex_approval`'s decision grammar and the vendored schema it was read
    // from. If the server disagrees, that disagreement is the finding.
    // **Timed, because a bound has to be derived from a measurement.**
    // `relay::UPSTREAM_WRITE_BUDGET` is how long the broker waits for the pump's proof
    // that this very `ws.send` completed, and `codex_link::DISPOSITION_BUDGET` is how
    // long this side waits for the frame that proof produces. Both are set above what a
    // real acknowledged write costs on 0.153, and this is where that cost is read.
    let answered_at = Instant::now();
    sub.send(serde_json::json!({"id": wire_id, "result": {"decision": "accept"}}))
        .await;

    let told = wait_until(Duration::from_secs(30), || {
        sub.methods()
            .iter()
            .any(|m| m == "codeconnect/responseDisposition")
    })
    .await;
    let round_trip = answered_at.elapsed();
    println!(
        "MEASURED disposition round-trip (answer written -> disposition read) = {round_trip:?}"
    );
    println!(
        "MEASURED disposition = {:?}",
        sub.first("codeconnect/responseDisposition")
    );
    assert!(
        told,
        "the broker must tell the answering leg what became of its response; \
         without it a phone answer can only ever be recorded as unknown"
    );
    let disposition = sub
        .first("codeconnect/responseDisposition")
        .expect("the disposition frame");
    assert_eq!(disposition["params"]["delivered"], serde_json::json!(true));
    assert_eq!(
        disposition["params"]["requestId"],
        serde_json::json!(wire_id)
    );
    assert_eq!(
        disposition["params"]["threadId"],
        Value::String(thread_id.clone())
    );

    let actuated = wait_until(Duration::from_secs(90), || {
        std::path::Path::new(&marker).exists()
    })
    .await;
    let resolved = wait_until(Duration::from_secs(30), || {
        sub.methods().iter().any(|m| m == "serverRequest/resolved")
    })
    .await;
    let _ = sub.barrier(Duration::from_secs(30)).await;

    println!("PANE AFTER THE ANSWER:\n{}", sb.capture_pane());
    println!("MEASURED actuated (marker file exists) = {actuated}");
    println!("MEASURED serverRequest/resolved observed = {resolved}");
    println!(
        "MEASURED resolved frame = {:?}",
        sub.first("serverRequest/resolved")
    );
    println!("SUB methods after the answer: {:?}", sub.methods());
    println!(
        "MEASURED frames carrying our wire id back:\n{:#?}",
        sub.seen()
            .into_iter()
            .filter(|v| v.get("id").and_then(Value::as_i64) == Some(wire_id))
            .collect::<Vec<_>>()
    );
    println!("BROKER LOG:\n{}", read_file(&sb.run_dir.join("broker.log")));

    sub.handle.abort();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(&marker);

    assert!(
        actuated,
        "a response of {{id, result:{{decision:\"accept\"}}}} sent by the ccd leg did NOT \
         run the command. That is the finding: the shape, the capability, or the \
         arbiter is not what `codex_approval`'s decision grammar says."
    );
}

/// **MEASUREMENT: what does the LOSING leg learn?**
///
/// The keyboard answers first, and only then does the ccd leg send the response
/// it had already composed. Gate 3 of Phase 3 needs a truthful outcome for that
/// loser, and the whole question is whether anything at all comes back to the
/// leg that lost — because if nothing does, an honest loser outcome cannot be
/// derived from the wire and has to be built.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn measure_what_a_losing_ccd_response_is_told() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("lose");
    let mut coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3b-loser-probe.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);

    assert!(
        wait_until(Duration::from_secs(60), || sb.broker_legs_bound()).await,
        "the host must bind both broker legs"
    );
    assert!(
        wait_until(Duration::from_secs(90), || sb.tui_running()).await,
        "the host must launch the real codex TUI"
    );

    let mut raw = RawCcd::connect(&sb.ccd_sock()).await;
    assert!(raw.initialize().await["result"].is_object());
    raw.notify("initialized", serde_json::json!({})).await;

    assert!(
        wait_until(Duration::from_secs(90), || sb
            .capture_pane()
            .contains("Ask Codex to do anything"))
        .await,
        "the TUI never painted a composer"
    );
    assert!(
        sb.submit_until(
            "Reply with the single word amber and nothing else.",
            Duration::from_secs(180),
            || sb.capture_pane().to_lowercase().contains("• amber"),
        )
        .await,
        "the warm-up turn never completed. pane:\n{}",
        sb.capture_pane()
    );

    let mut thread_id = String::new();
    for _ in 0..60 {
        let loaded = raw
            .request(
                "thread/loaded/list",
                serde_json::json!({}),
                Duration::from_secs(20),
            )
            .await;
        if let Some(id) = loaded["result"]["data"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_str)
        {
            thread_id = id.to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(!thread_id.is_empty(), "no loaded thread to resume onto");

    let mut sub = WireTap::split(raw, "SUB");
    let mut resumed = false;
    for attempt in 0..12 {
        let id = 500 + attempt;
        sub.send(serde_json::json!({
            "id": id, "method": "thread/resume", "params": {"threadId": thread_id}
        }))
        .await;
        let frames = Arc::clone(&sub.frames);
        wait_until(Duration::from_secs(10), || {
            frames
                .lock()
                .expect("wire tap sink")
                .iter()
                .any(|v| v.get("id").and_then(Value::as_i64) == Some(id))
        })
        .await;
        if sub
            .seen()
            .into_iter()
            .any(|v| v.get("id").and_then(Value::as_i64) == Some(id) && v.get("result").is_some())
        {
            resumed = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(resumed, "the ccd leg never subscribed");
    assert!(sub.barrier(Duration::from_secs(30)).await);

    let frames = Arc::clone(&sub.frames);
    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(180),
            || frames.lock().expect("wire tap sink").iter().any(|f| f
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|m| m.ends_with("/requestApproval"))),
        )
        .await,
        "no approval reached the ccd leg. pane:\n{}",
        sb.capture_pane()
    );
    let request = sub
        .first("item/commandExecution/requestApproval")
        .expect("the command-execution approval frame");
    let wire_id = request["id"].as_i64().expect("a numeric server-request id");

    // ---- the KEYBOARD answers first --------------------------------------
    sb.send_keys(&["Enter"]);
    assert!(
        wait_until(Duration::from_secs(60), || sub
            .methods()
            .iter()
            .any(|m| m == "serverRequest/resolved"))
        .await,
        "the keyboard answer never resolved the request. pane:\n{}",
        sb.capture_pane()
    );
    let before_losing_send = sub.seen().len();
    println!("PANE AFTER THE KEYBOARD ANSWER:\n{}", sb.capture_pane());

    // ---- and only NOW does the ccd leg answer ----------------------------
    sub.send(serde_json::json!({"id": wire_id, "result": {"decision": "cancel"}}))
        .await;
    // A barrier on this very leg, so "nothing came back" is a fact about the
    // wire and not about how long the probe was willing to wait.
    assert!(
        sub.barrier(Duration::from_secs(30)).await,
        "the losing leg must still answer a barrier — if it does not, the broker \
         closed it, and THAT is the signal"
    );

    let disposition = sub
        .first("codeconnect/responseDisposition")
        .expect("the losing leg must be told its response went nowhere");
    println!("MEASURED loser disposition = {disposition}");
    assert_eq!(disposition["params"]["delivered"], serde_json::json!(false));
    assert_eq!(
        disposition["params"]["requestId"],
        serde_json::json!(wire_id)
    );

    let after = sub.seen();
    let arrived: Vec<Value> = after[before_losing_send..]
        .iter()
        .filter(|v| v["method"].as_str() != Some("thread/loaded/list"))
        .cloned()
        .collect();
    println!(
        "MEASURED frames handed to the losing leg after its response (barrier excluded):\n{:#?}",
        arrived
            .iter()
            .filter(|v| v.get("id").and_then(Value::as_i64).is_none() || v.get("method").is_some())
            .collect::<Vec<_>>()
    );
    println!("MEASURED leg still open after losing = true (the barrier answered)");
    println!("BROKER LOG:\n{}", read_file(&sb.run_dir.join("broker.log")));

    sub.handle.abort();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(&marker);
}

/// **THE PHASE-3b GATE: a phone answer, through the production daemon, reaches
/// the real app-server — and the TUI's prompt goes away without a key being
/// pressed.**
///
/// Everything else about answering is proven in halves: the unit gates drive
/// `Daemon::answer` against a link whose replies a test writes, and
/// [`measure_a_ccd_leg_answering_a_command_approval`] writes the response frame by
/// hand. This is the only place the whole chain runs against a real codex — the
/// observer raising the card, `Daemon::answer` validating and claiming it, the
/// link translating the durable request id into the wire id THIS connection was
/// handed, the broker's arbiter, the app-server actuating it, and the broker's
/// disposition coming back to settle the claim.
///
/// The pane assertion is the one that cannot be faked by any of the halves: the
/// TUI is a separate process that was showing an approval prompt, and nothing
/// typed into it.
///
/// **The owner is a real registration and the tap is a real socket.** Both used to
/// be stand-ins: the session was given an answer channel by a test-only installer
/// that staked an epoch by hand, and the tap was a direct call to
/// [`crate::state::Daemon::answer`]. Neither is here now.
/// [`register_the_run`] sends the coordinator's own frame through
/// `register_supervisor`, so the epoch, the row, the supervisor handle, the answer
/// channel and the link task are all built by the production acceptance; and
/// [`PhoneOverTheWire`] speaks `answer` to this daemon's own `ws_server` and decodes
/// the `answer_result` it sends back. What is left between a phone in a pocket and
/// this gate is the transport (a tailnet address instead of loopback) and the
/// device credential — nothing on the answer path itself.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn a_phone_answer_runs_the_command_and_dismisses_the_tui_prompt() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("e2e");
    let mut coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3b-e2e.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);

    wait_for_the_broker_and_the_tui(&sb).await;

    // The real daemon, and a link the daemon built for itself out of a real
    // registration.
    let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-live");
    let (daemon, _db, _push) = live_daemon(&session);
    let uid = session.uid.clone();
    let registration = register_the_run(&daemon, &session, &sb).await;
    let mut phone = PhoneOverTheWire::connect(&daemon).await;

    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    // ---- provoke one command approval, and let the OBSERVER card it ---------
    let cards = || {
        daemon
            .store
            .codex_pending_approvals(&uid)
            .expect("read the run's open cards")
    };
    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(240),
            || !cards().is_empty(),
        )
        .await,
        "the observer never raised a card for the approval. pane:\n{}\nbroker.log:\n{}",
        sb.capture_pane(),
        read_file(&sb.run_dir.join("broker.log"))
    );
    let held = cards().remove(0);
    let card: protocol::ws::ApprovalCard =
        serde_json::from_str(&held.card).expect("the stored card decodes");
    let pane_while_asking = sb.capture_pane();
    println!("PANE WHILE THE APPROVAL IS UP:\n{pane_while_asking}");
    assert!(
        pane_while_asking.contains("Would you like to run the following command?"),
        "the premise: the TUI is showing the prompt this answer is about"
    );
    assert!(
        !std::path::Path::new(&marker).exists(),
        "the command ran before anybody answered; this gate would prove nothing"
    );

    // ---- THE ANSWER, over the wire, exactly as the phone sends it ------------
    let result = phone
        .answer(&card, "accept", &uid, Duration::from_secs(120))
        .await;
    println!("MEASURED AnswerResult = {result:?}");
    match &result {
        protocol::ws::AnswerResult::Applied { outcome } => {
            assert_eq!(outcome.applied_via, protocol::ws::AnswerPath::CodexResponse);
            assert_eq!(outcome.resolved_by, protocol::ws::ResolvedBy::Phone);
        }
        other => panic!(
            "the phone's answer must be applied. broker.log:\n{}\ngot: {other:?}",
            read_file(&sb.run_dir.join("broker.log"))
        ),
    }

    // The app-server acted on it: the command really ran.
    let actuated = wait_until(Duration::from_secs(90), || {
        std::path::Path::new(&marker).exists()
    })
    .await;
    // And the prompt is gone from a pane nothing was typed into.
    let dismissed = wait_until(Duration::from_secs(60), || {
        !sb.capture_pane()
            .contains("Would you like to run the following command?")
    })
    .await;
    let pane_after = sb.capture_pane();
    println!("PANE AFTER THE PHONE ANSWERED:\n{pane_after}");
    println!("MEASURED actuated = {actuated}, tui dismissed = {dismissed}");

    let status = daemon
        .store
        .answer_status(&uid, &card.request_id)
        .expect("read the answer ledger")
        .expect("the claim is durable");
    let open_after = cards().len();
    let resolutions: Vec<protocol::ws::CodexResolution> = daemon
        .store
        .events_after(&uid, 0, 10_000)
        .expect("read the run's events")
        .into_iter()
        .filter(|e| e.kind == protocol::event::EventKind::ApprovalResolved)
        .map(|e| serde_json::from_value(e.payload).expect("a resolution decodes"))
        .collect();
    println!("MEASURED status = {status:?}, open cards after = {open_after}");
    println!("MEASURED resolutions = {resolutions:?}");
    println!("BROKER LOG:\n{}", read_file(&sb.run_dir.join("broker.log")));

    phone.close();
    daemon.unregister_supervisor(&registration).await;
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(&marker);

    assert!(
        actuated,
        "the app-server never ran the command the phone approved"
    );
    assert!(
        dismissed,
        "the TUI is still showing a prompt that has been answered. pane:\n{pane_after}"
    );
    assert_eq!(
        status,
        crate::store::AnswerStatus::Settled("delivered".into()),
        "the broker told this daemon its answer was the one that landed"
    );
    assert_eq!(open_after, 0, "an answered card is retired from the phone");
    assert_eq!(
        resolutions,
        vec![protocol::ws::CodexResolution::Answered {
            by: protocol::ws::ResolutionActor::Phone,
            decision: Some(protocol::ws::AnswerDecision::OptionId {
                option_id: "accept".into()
            }),
        }],
        "and the fleet is told who answered and with what — the two facts \
         serverRequest/resolved cannot carry"
    );
}

/// **The same gate for the other family, whose options the wire does not carry.**
///
/// A command approval's option set comes off the wire (`availableDecisions`); a
/// file change's comes from `Family::labels`, because the frame offers nothing
/// (measured — `measure_what_a_file_change_approval_offers`). So the id a phone
/// names for a file change is checked against, and rebuilt from, a table this
/// daemon owns rather than one the server sent — a genuinely different path to
/// the same wire decision, and the app-server has to accept it just the same.
///
/// The edit landing on disk is the actuation proof here, in place of the command
/// family's marker file.
///
/// **Answered through [`crate::state::Daemon::answer`] rather than over the wire, on
/// purpose.** The WS seam — decode, admit, dispatch, encode the reply — is the same
/// four steps for every family and is proven once, live, by the command gate's
/// [`PhoneOverTheWire`]. What is different here is the option table and the decision
/// built from it, and that is what this gate spends its run on.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn a_phone_answer_applies_a_file_change_and_dismisses_the_tui_prompt() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("fce2e");
    let mut coord = sb.spawn_coordinator(&codex);
    let target = std::path::PathBuf::from(format!("/tmp/cc-3b-fc.{}.txt", nanos()));
    std::fs::write(&target, "hello from the codex approvals probe\n").expect("seed the target");

    wait_for_the_broker_and_the_tui(&sb).await;

    let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-live");
    let (daemon, _db, _push) = live_daemon(&session);
    let uid = session.uid.clone();
    let registration = register_the_run(&daemon, &session, &sb).await;

    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let cards = || {
        daemon
            .store
            .codex_pending_approvals(&uid)
            .expect("read the run's open cards")
    };
    assert!(
        sb.submit_until(
            &format!(
                "Use apply_patch to edit {}, replacing the word hello with goodbye. \
                 Do not explain, just do it.",
                target.display()
            ),
            Duration::from_secs(300),
            || cards().iter().any(|card| card.family == "fileChange"),
        )
        .await,
        "the observer never raised a file-change card. pane:\n{}",
        sb.capture_pane()
    );
    let held = cards()
        .into_iter()
        .find(|card| card.family == "fileChange")
        .expect("the file-change card");
    let card: protocol::ws::ApprovalCard =
        serde_json::from_str(&held.card).expect("the stored card decodes");
    println!(
        "PANE WHILE THE FILE-CHANGE APPROVAL IS UP:\n{}",
        sb.capture_pane()
    );
    // The daemon-owned table, on the card, in the order it will be shown.
    let offered: Vec<&str> = card.tool_input["options"]
        .as_array()
        .expect("options ride the card")
        .iter()
        .map(|option| option["id"].as_str().expect("an option id"))
        .collect();
    assert_eq!(
        offered,
        ["accept", "acceptForSession", "cancel"],
        "the file-change family's options are this daemon's table, not the wire's"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "hello from the codex approvals probe\n",
        "the edit was applied before anybody answered; this gate would prove nothing"
    );

    let result = daemon
        .answer(
            &card.request_id,
            &card.payload_hash,
            protocol::ws::AnswerDecision::OptionId {
                option_id: "accept".into(),
            },
            Some(&uid),
        )
        .await;
    println!("MEASURED AnswerResult = {result:?}");

    let applied = wait_until(Duration::from_secs(90), || {
        std::fs::read_to_string(&target)
            .map(|body| body.contains("goodbye"))
            .unwrap_or(false)
    })
    .await;
    let pane_after = sb.capture_pane();
    println!("PANE AFTER THE PHONE ANSWERED:\n{pane_after}");
    let status = daemon
        .store
        .answer_status(&uid, &card.request_id)
        .expect("read the answer ledger")
        .expect("the claim is durable");
    println!("MEASURED applied = {applied}, status = {status:?}");
    println!("BROKER LOG:\n{}", read_file(&sb.run_dir.join("broker.log")));

    daemon.unregister_supervisor(&registration).await;
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(&target);

    assert!(
        matches!(result, protocol::ws::AnswerResult::Applied { .. }),
        "the phone's answer must be applied: {result:?}"
    );
    assert!(
        applied,
        "the app-server never wrote the edit the phone approved"
    );
    assert_eq!(
        status,
        crate::store::AnswerStatus::Settled("delivered".into())
    );
}

/// Does this frame **settle** the approval on `item_id`?
///
/// Only two frames do: the `serverRequest/resolved` that answers the request,
/// and the item's own `item/completed`. Nothing else is an answer, and the
/// distinction is not pedantic — the bounce gate's predicate used to accept any
/// frame whose `/params/item/id` matched, which the `item/started` a leg is
/// handed *before* the approval satisfies on arrival. A gate resting on that
/// would go green without a single settling frame ever reaching the replacement
/// leg, which is the one thing it exists to prove.
fn settles_the_approval(frame: &Value, item_id: &str) -> bool {
    match frame.get("method").and_then(Value::as_str) {
        Some("serverRequest/resolved") => true,
        Some("item/completed") => {
            frame.pointer("/params/item/id").and_then(Value::as_str) == Some(item_id)
        }
        _ => false,
    }
}

/// **`item/started` is not an answer.**
///
/// Runs in the ordinary suite — it needs no codex — because the claim is about
/// the predicate and not about the wire. The frames are the real shapes:
/// `item/started` and `item/completed` carry the id at `/params/item/id`, the
/// approval request carries it at `/params/itemId`, and `serverRequest/resolved`
/// carries no item at all.
///
/// **Mutation:** widen the match back to any frame with a matching
/// `/params/item/id` and the first assertion goes red.
#[test]
fn only_a_terminal_settles_an_approval_for_the_bounce_gate() {
    const ITEM: &str = "exec-1a54b0d3-1d17-4025-8202-e478fb329f00";
    let with_item = |method: &str| serde_json::json!({"method": method, "params": {"item": {"id": ITEM, "type": "commandExecution"}}});

    assert!(
        !settles_the_approval(&with_item("item/started"), ITEM),
        "the frame that OPENS the item must never be read as the answer to it"
    );
    assert!(!settles_the_approval(
        &serde_json::json!({
            "method": "item/commandExecution/requestApproval",
            "params": {"itemId": ITEM},
        }),
        ITEM
    ));
    assert!(!settles_the_approval(
        &serde_json::json!({"method": "thread/status/changed", "params": {}}),
        ITEM
    ));

    assert!(settles_the_approval(&with_item("item/completed"), ITEM));
    assert!(settles_the_approval(
        &serde_json::json!({
            "method": "serverRequest/resolved",
            "params": {"threadId": "t", "requestId": 0},
        }),
        ITEM
    ));
    // And an item/completed for somebody ELSE's item settles nothing.
    assert!(!settles_the_approval(
        &serde_json::json!({
            "method": "item/completed",
            "params": {"item": {"id": "exec-other", "type": "commandExecution"}},
        }),
        ITEM
    ));
}

/// **What a rebinding connection is told about an approval that is still pending.**
///
/// The rebind gate needs one fact the wire has not yet been asked for: when the
/// daemon bounces mid-approval and resumes, does the app-server (a) re-deliver
/// the outstanding `requestApproval` to the new connection, (b) describe the
/// blocked item in the resume answer, or (c) say nothing at all? Each answer
/// implies a different rebind rule, and only one of them is real:
///
/// * re-delivered ⇒ the card must dedupe on `(threadId, itemId)`, because the
///   request id is per-connection and would otherwise mint a second card;
/// * described-only ⇒ the card rebinds from the store and the resume answer
///   confirms it is still live;
/// * silent ⇒ the store is the only witness, and a resumed link can never learn
///   that a pending card is already dead.
///
/// It was a probe that printed those three and asserted none of them, which left
/// the rebind rule resting on a Rust test that installed the ordering it wanted.
/// It is now a gate: it drives the bounce for real, pins the measured answer, and
/// writes the capture the Rust-side rebind gate replays.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn a_card_raised_before_a_daemon_bounce_rebinds_onto_one_card() {
    /// How many times the app-server re-delivers an outstanding
    /// `*/requestApproval` to a connection that resumes after the subscribed one
    /// went away.
    ///
    /// **Measured on live 0.153.2: one.** Of the three behaviours this gate was
    /// written to tell apart, the wire does the first — it re-delivers, with a
    /// fresh per-connection wire id and a byte-identical `itemId`. That is what
    /// makes deduping on the item the *correct* rebind rule rather than a
    /// defensive one: without it a bounced daemon mints a second card for a
    /// question already on the phone.
    const REBIND_REDELIVERIES: usize = 1;
    /// Whether the resume answer's own turn state names the pending item.
    ///
    /// **Measured: no.** The answer's `turns[]` came back
    /// `["completed", "inProgress"]` and never named the blocked item, so the
    /// resume answer is not a witness for a pending approval and nothing may
    /// read it as one. The re-delivery above is the witness.
    const REBIND_DESCRIBED: bool = false;

    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("rebind");
    let mut coord = sb.spawn_coordinator(&codex);

    assert!(
        wait_until(Duration::from_secs(60), || sb.broker_legs_bound()).await,
        "the host must bind both broker legs. appserver.stderr:\n{}",
        read_file(&sb.run_dir.join("appserver.stderr.log"))
    );
    assert!(
        wait_until(Duration::from_secs(90), || sb.tui_running()).await,
        "the host must launch the real codex TUI"
    );
    assert!(
        wait_until(Duration::from_secs(90), || sb
            .capture_pane()
            .contains("Ask Codex to do anything"))
        .await,
        "the TUI never painted a composer. pane:\n{}",
        sb.capture_pane()
    );

    // A rollout must exist before any resume is possible (measured: a thread with
    // `turns: []` is refused with "no rollout found for thread id").
    assert!(
        sb.submit_until(
            "Reply with the single word amber and nothing else.",
            Duration::from_secs(180),
            || sb.capture_pane().to_lowercase().contains("• amber"),
        )
        .await,
        "the warm-up turn never completed. pane:\n{}",
        sb.capture_pane()
    );

    let mut raw = RawCcd::connect(&sb.ccd_sock()).await;
    raw.initialize().await;
    raw.notify("initialized", serde_json::json!({})).await;
    let loaded = raw
        .request(
            "thread/loaded/list",
            serde_json::json!({}),
            Duration::from_secs(20),
        )
        .await;
    let thread_id = loaded["result"]["data"][0]
        .as_str()
        .expect("a loaded thread")
        .to_string();
    println!("MEASURED thread_id = {thread_id}");

    let mut watcher = WireTap::split(raw, "WATCH");
    let mut resumed = false;
    for attempt in 0..12 {
        let id = 500 + attempt;
        watcher
            .send(serde_json::json!({
                "id": id, "method": "thread/resume", "params": {"threadId": thread_id}
            }))
            .await;
        let frames = Arc::clone(&watcher.frames);
        wait_until(Duration::from_secs(10), || {
            frames
                .lock()
                .expect("wire tap sink")
                .iter()
                .any(|v| v.get("id").and_then(Value::as_i64) == Some(id))
        })
        .await;
        if watcher
            .seen()
            .iter()
            .any(|v| v.get("id").and_then(Value::as_i64) == Some(id) && v.get("result").is_some())
        {
            resumed = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(resumed, "the watcher never subscribed");

    // Park a turn on an approval and LEAVE it there.
    let frames = Arc::clone(&watcher.frames);
    assert!(
        sb.submit_until(
            "Run the shell command `touch /tmp/cc-rebind-probe.txt` now. Do not explain, just run it.",
            Duration::from_secs(180),
            || frames.lock().expect("wire tap sink").iter().any(|f| f
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|m| m.ends_with("/requestApproval"))),
        )
        .await,
        "no approval to rebind onto. pane:\n{}",
        sb.capture_pane()
    );
    println!("pane while the approval is pending:\n{}", sb.capture_pane());

    // The frame the rebind has to agree with: the approval this leg was handed
    // before it went away.
    let before = watcher.seen();
    let asked = before
        .iter()
        .find(|f| {
            f.get("method")
                .and_then(Value::as_str)
                .is_some_and(|m| m.ends_with("/requestApproval"))
        })
        .cloned()
        .expect("the approval frame the subscribed leg was handed");
    let asked_item = asked["params"]["itemId"]
        .as_str()
        .expect("an itemId")
        .to_string();
    println!("MEASURED pending approval itemId = {asked_item}");

    // ---- THE BOUNCE ---------------------------------------------------------
    // The subscribed leg goes away entirely, which is what a daemon bounce is.
    // Leaving it open would ask a different question: what a *second* observer
    // is handed, not what a *replacement* one is.
    let before = watcher.close();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut fresh = RawCcd::connect(&sb.ccd_sock()).await;
    fresh.initialize().await;
    fresh.notify("initialized", serde_json::json!({})).await;
    let answer = fresh
        .request(
            "thread/resume",
            serde_json::json!({"threadId": thread_id}),
            Duration::from_secs(30),
        )
        .await;
    println!(
        "MEASURED resume-during-pending-approval ANSWER:\n{}",
        serde_json::to_string_pretty(&answer).expect("pretty")
    );
    assert!(
        answer["result"].is_object(),
        "the replacement leg must resume, or nothing below is about a rebind: {answer}"
    );

    let mut rebound = WireTap::split(fresh, "REBIND");
    // A window in which the app-server could re-deliver the outstanding request
    // to the replacement connection, then a barrier — so what follows is a fact
    // about the wire and not about timing.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        rebound.barrier(Duration::from_secs(30)).await,
        "the rebound leg must answer a barrier"
    );
    let redelivered: Vec<Value> = rebound
        .seen()
        .into_iter()
        .filter(|f| {
            f.get("method")
                .and_then(Value::as_str)
                .is_some_and(|m| m.ends_with("/requestApproval"))
        })
        .collect();
    println!(
        "MEASURED re-delivered-to-the-replacement-leg count = {}",
        redelivered.len()
    );
    println!("REBIND methods: {:?}", rebound.methods());

    // Is the pending approval described in the resume answer's own turn state?
    let turns = &answer["result"]["thread"]["turns"];
    println!(
        "MEASURED resume answer turns[] statuses = {:?}",
        turns.as_array().map(|a| a
            .iter()
            .map(|t| t["status"].clone())
            .collect::<Vec<Value>>())
    );
    let described = turns.to_string().contains(&asked_item);
    println!("MEASURED pending item described in the resume answer = {described}");

    // ---- THE ASSERTION ------------------------------------------------------
    // **The measured answer, pinned.** Three behaviours were possible and they
    // imply three different rebind rules (re-delivered ⇒ the card must dedupe on
    // the item; described-only ⇒ the card rebinds from the store and the answer
    // confirms it is live; silent ⇒ the store is the only witness). Whichever it
    // is, it is now a gate: a codex release that changes it fails here rather
    // than silently changing what a bounced daemon shows a phone.
    assert_eq!(
        redelivered.len(),
        REBIND_REDELIVERIES,
        "the app-server's redelivery behaviour on a mid-approval resume has \
         changed. Measured {} re-deliveries against the pinned {REBIND_REDELIVERIES}; \
         if this is the new truth, re-derive the rebind rule and the committed \
         capture before moving the constant. REBIND methods: {:?}",
        redelivered.len(),
        rebound.methods()
    );
    if let Some(again) = redelivered.first() {
        assert_eq!(
            again["params"]["itemId"].as_str(),
            Some(asked_item.as_str()),
            "a re-delivery that names a different item is not a re-delivery"
        );
        assert_eq!(
            again["params"]["threadId"], asked["params"]["threadId"],
            "and it must be about the thread this leg resumed onto"
        );
        // The identity the card is derived from is byte-stable across the
        // bounce; the WIRE id is not, and that is the whole reason the derived
        // id is taken from the item.
        assert_ne!(
            again.get("id"),
            None,
            "a server request carries a wire id even on re-delivery"
        );
    }
    assert_eq!(
        described, REBIND_DESCRIBED,
        "whether the resume answer describes the pending item has changed"
    );

    // ---- THE TERMINAL AFTER THE BOUNCE --------------------------------------
    // The operator answers at the keyboard. The card raised before the bounce
    // must be retired by what the REPLACEMENT leg is handed, or a bounced daemon
    // leaves an answered question on a phone forever.
    sb.send_keys(&["Enter"]);
    let frames = Arc::clone(&rebound.frames);
    let settled = wait_until(Duration::from_secs(60), || {
        frames
            .lock()
            .expect("wire tap sink")
            .iter()
            .any(|f| settles_the_approval(f, &asked_item))
    })
    .await;
    println!("pane after the keyboard answer:\n{}", sb.capture_pane());
    assert!(
        settled,
        "the replacement leg was handed no terminal for the approval raised before \
         the bounce — neither serverRequest/resolved nor the item's own \
         item/completed. The card would stay on the phone after the question was \
         answered. REBIND methods: {:?}",
        rebound.methods()
    );

    // ---- THE CAPTURE --------------------------------------------------------
    // Written in the `{conn,dir,frame}` shape the committed fixtures use, so the
    // Rust-side rebind gate replays the real bounce rather than a hand-built one.
    let after = rebound.seen();
    // The run dir is swept when the sandbox drops, so a capture written there is
    // a capture nobody can commit. `CC_CODEX_BOUNCE_CAPTURE` names somewhere it
    // survives, the way `CC_CODEX_FRAME_TEE` does for the broker's tee.
    let capture = match std::env::var("CC_CODEX_BOUNCE_CAPTURE") {
        Ok(path) => PathBuf::from(path),
        Err(_) => sb.run_dir.join("bounce-capture.jsonl"),
    };
    let mut lines = String::new();
    let mut write = |conn: &str, dir: &str, frame: &Value| {
        lines.push_str(&serde_json::json!({"conn": conn, "dir": dir, "frame": frame}).to_string());
        lines.push('\n');
    };
    // **Method-less frames are kept, and that is the whole point of this one.**
    // An earlier writer dropped them, which silently excluded the single most
    // load-bearing frame in the capture: the `thread/resume` RESPONSE, whose
    // `turns[]` is the evidence for "the resume answer does not describe the
    // pending item". A fixture that cannot witness the claim the gate makes is
    // a fixture the Rust replay has to take on trust.
    for frame in &before {
        write("ccd-before-bounce", "s2c", frame);
    }
    // The request/response pair the replacement leg opened with. `RawCcd` owns
    // this exchange, so it never reaches the tap — it is written explicitly, in
    // arrival order, ahead of the tapped frames.
    write(
        "ccd-after-bounce",
        "c2s",
        &serde_json::json!({
            // The id the answer came back under, so the pair in the capture is
            // the pair that was actually exchanged.
            "id": answer.get("id").cloned().unwrap_or(Value::Null),
            "method": "thread/resume",
            "params": {"threadId": thread_id},
        }),
    );
    write("ccd-after-bounce", "s2c", &answer);
    for frame in &after {
        write("ccd-after-bounce", "s2c", frame);
    }
    std::fs::write(&capture, &lines).expect("write the bounce capture");
    println!("CAPTURE WRITTEN: {}", capture.display());
    println!("GATE PASS — a card raised before a daemon bounce rebinds onto one card and is retired by what the replacement leg is handed");

    rebound.handle.abort();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file("/tmp/cc-rebind-probe.txt");
}

/// **What a `fileChange` approval carries, and what the TUI offers for it.**
///
/// The one shape the design cannot be written without and that no capture in
/// this repository holds. `item/fileChange/requestApproval` declares **no
/// `availableDecisions` field at all** — verified against both vendored
/// bundles, 0.147 and 0.153, stable and experimental — so unlike the command
/// family there is nothing on the wire to read an option set from. The daemon
/// must therefore own a label table pinned to what the TUI actually offers,
/// and that can only be learned by looking at the screen while the decision is
/// up.
///
/// So this probe reads two things at the same moment: the request frame
/// verbatim (to pin which params are populated on 0.153 — 0.147 sent `reason`
/// and `grantRoot` both null), and the **pane**, which is where the TUI renders
/// the choices a human is being given.
///
/// Delete once the answers are pinned in fixtures and gates.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn measure_what_a_file_change_approval_offers() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("fc");
    let mut coord = sb.spawn_coordinator(&codex);

    assert!(
        wait_until(Duration::from_secs(60), || sb.broker_legs_bound()).await,
        "the host must bind both broker legs. appserver.stderr:\n{}",
        read_file(&sb.run_dir.join("appserver.stderr.log"))
    );
    assert!(
        wait_until(Duration::from_secs(90), || sb.tui_running()).await,
        "the host must launch the real codex TUI"
    );
    assert!(
        wait_until(Duration::from_secs(90), || sb
            .capture_pane()
            .contains("Ask Codex to do anything"))
        .await,
        "the TUI never painted a composer. pane:\n{}",
        sb.capture_pane()
    );

    // A file for it to change. `fileChange` is an edit of something that
    // exists; asking for a creation gets a shell command and the wrong family.
    let target = std::path::PathBuf::from("/tmp/cc-fc-probe.txt");
    std::fs::write(&target, "hello from the codex approvals probe\n").expect("seed the target");

    // The rollout the resume needs (measured: a thread with `turns: []` is
    // refused with "no rollout found for thread id").
    sb.send_keys(&["Reply with the single word amber and nothing else."]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    sb.send_keys(&["Enter"]);
    assert!(
        wait_until(Duration::from_secs(180), || sb
            .capture_pane()
            .to_lowercase()
            .contains("• amber"))
        .await,
        "the warm-up turn never completed. pane:\n{}",
        sb.capture_pane()
    );

    let mut raw = RawCcd::connect(&sb.ccd_sock()).await;
    raw.initialize().await;
    raw.notify("initialized", serde_json::json!({})).await;
    let loaded = raw
        .request(
            "thread/loaded/list",
            serde_json::json!({}),
            Duration::from_secs(20),
        )
        .await;
    let thread_id = loaded["result"]["data"][0]
        .as_str()
        .expect("a loaded thread")
        .to_string();
    println!("MEASURED thread_id = {thread_id}");

    let mut sub = WireTap::split(raw, "SUB");
    let mut resumed = false;
    for attempt in 0..12 {
        let id = 500 + attempt;
        sub.send(serde_json::json!({
            "id": id, "method": "thread/resume", "params": {"threadId": thread_id}
        }))
        .await;
        let frames = Arc::clone(&sub.frames);
        wait_until(Duration::from_secs(10), || {
            frames
                .lock()
                .expect("wire tap sink")
                .iter()
                .any(|v| v.get("id").and_then(Value::as_i64) == Some(id))
        })
        .await;
        if sub
            .seen()
            .iter()
            .any(|v| v.get("id").and_then(Value::as_i64) == Some(id) && v.get("result").is_some())
        {
            resumed = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(
        resumed,
        "the ccd leg never subscribed, so nothing it fails to receive is evidence"
    );

    sb.send_keys(&[
        "Use apply_patch to edit /tmp/cc-fc-probe.txt, replacing the word hello with goodbye. \
         Do not explain, just do it.",
    ]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    sb.send_keys(&["Enter"]);

    let asked = wait_until(Duration::from_secs(240), || {
        sub.methods()
            .iter()
            .any(|m| m == "item/fileChange/requestApproval")
    })
    .await;

    // **The pane, captured while the decision is up.** This is the measurement:
    // the wire carries no option set for this family, so what the TUI renders
    // is the only evidence of what a human is actually offered.
    let pane = sb.capture_pane();
    println!("MEASURED pane WHILE THE FILE-CHANGE APPROVAL IS UP:\n{pane}");
    println!("SUB methods: {:?}", sub.methods());

    if let Some(started) = sub.seen().into_iter().find(|v| {
        v["method"].as_str() == Some("item/started")
            && v.pointer("/params/item/type").and_then(Value::as_str) == Some("fileChange")
    }) {
        println!(
            "MEASURED the fileChange item/started that carries the CONTENT:\n{}",
            serde_json::to_string_pretty(&started).expect("pretty")
        );
    }

    assert!(
        asked,
        "no fileChange approval reached the subscribed ccd leg. pane:\n{pane}\nmethods: {:?}",
        sub.methods()
    );
    let request = sub
        .first("item/fileChange/requestApproval")
        .expect("the file-change approval frame");
    println!(
        "MEASURED fileChange requestApproval VERBATIM:\n{}",
        serde_json::to_string_pretty(&request).expect("pretty")
    );

    // ---- THE GATE: the real parser, on the real frame ---------------------
    //
    // The same gate the command probe carries, for the family that needs it
    // most: this request carries no content at all, so the card can only be
    // built by joining it against the `item/started` two frames earlier. If that
    // ordering ever stopped holding, the observer would refuse to card a real
    // approval — silently, in production — and this is where that shows up.
    let started = sub
        .seen()
        .into_iter()
        .find(|v| {
            v["method"].as_str() == Some("item/started")
                && v.pointer("/params/item/type").and_then(Value::as_str) == Some("fileChange")
                && v.pointer("/params/item/id").and_then(Value::as_str)
                    == request["params"]["itemId"].as_str()
        })
        .unwrap_or_else(|| {
            panic!(
                "no item/started for this fileChange item reached the subscribed leg before \
                 its approval. That ordering is the ONLY source of the content this family's \
                 card describes, and without it the observer refuses to card rather than ask \
                 a person about \"some files\". methods: {:?}",
                sub.methods()
            )
        });

    assert!(
        request["params"]["availableDecisions"].is_null(),
        "0.153 started sending availableDecisions on a fileChange. The pane-measured \
         label table in `Family::labels` is this family's only option set precisely \
         because the wire had none; if the wire now has one, read IT and delete the \
         table's set half. Got: {}",
        request["params"]["availableDecisions"]
    );

    let approval = crate::codex_approval::Approval::read(
        crate::codex_approval::Family::FileChange,
        &request["params"],
        started.pointer("/params/item"),
    )
    .expect("the production parser must read a live 0.153 file-change approval");
    let request_id = approval
        .request_id("01K1B3XQ8ZC0DE5FGH7JKMNPCX", 1)
        .expect("a live item id must fit the composite id's bounds");
    let card = approval.card(request_id, 1);
    println!(
        "MEASURED the card this build raises from that frame:\n{}",
        serde_json::to_string_pretty(&card).expect("pretty")
    );

    let wire = serde_json::to_value(&card).expect("the card must serialize");
    for required in [
        "request_id",
        "payload_hash",
        "tool_name",
        "tool_input",
        "display_text",
    ] {
        assert!(
            wire.get(required).is_some_and(|value| !value.is_null()),
            "the phone cannot decode a card without a non-null {required}: {wire}"
        );
    }
    assert_eq!(
        card.payload_hash,
        protocol::hash::sha256_hex(card.display_text.as_bytes()),
        "a card built from the live wire must still hash to its own display text"
    );
    assert_eq!(
        card.display_text,
        format!("{}\n{}", card.tool_name, card.tool_input),
        "and must still re-render as the app re-renders it"
    );
    // The content really came off the preceding `item/started`, and the option
    // set is this family's own — not the command family's.
    assert_eq!(
        card.tool_input["changes"], started["params"]["item"]["changes"],
        "the changes on the card are the ones the item/started carried"
    );
    assert_eq!(
        card.tool_input["options"][1]["id"],
        serde_json::json!("acceptForSession")
    );
    println!("GATE PASS — the live 0.153 file-change approval became a card the phone can verify");

    sb.send_keys(&["Enter"]);
    tokio::time::sleep(Duration::from_secs(6)).await;
    println!(
        "MEASURED methods after the keyboard answer: {:?}",
        sub.methods()
    );
    println!("pane after the answer:\n{}", sb.capture_pane());

    sub.handle.abort();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(&target);
}

// ==================================================== THE PHASE-3b FAULT GATES

/// **The same store, and a daemon that has never seen it.**
///
/// A bounce, expressed as the only thing a test can honestly express it as: the
/// process's in-memory state — the pending map, the link, the registration epochs —
/// is gone, and everything the next daemon knows it has to read back off disk. That
/// is precisely the boundary [`crate::state::Daemon::recover`] exists to cross, and
/// a gate that reused the old `Daemon` would be testing a restart that kept its
/// memory.
fn rebuild_the_daemon(store: &Arc<crate::store::Store>) -> Arc<crate::state::Daemon> {
    let (tail_tx, tail_rx) = tokio::sync::mpsc::unbounded_channel();
    Box::leak(Box::new(tail_rx));
    crate::state::Daemon::new(
        protocol::config::Config::default(),
        Arc::clone(store),
        Arc::new(crate::apns::LoggingPushSender::new()) as Arc<dyn crate::apns::PushSender>,
        crate::state::Endpoint {
            host: "test.ts.net".into(),
            port: 8787,
            tls: false,
        },
        tail_tx,
    )
}

/// Every `answer` row this run has in `mutation_ledger`, as
/// `(client_request_id, status, outcome)`.
///
/// Read from SQLite directly rather than through
/// [`crate::store::Store::answer_status`], because the claim under test is about
/// **how many rows exist**, and a reader that returns one status by key cannot tell
/// one row from two. `ORDER BY started_at` so a second row would be visible beside
/// the first rather than shadowing it.
fn answer_ledger(db: &TempDb, uid: &str) -> Vec<(String, String, Option<String>)> {
    let conn = rusqlite::Connection::open(db.path()).expect("open the daemon's own database");
    let mut stmt = conn
        .prepare(
            "SELECT client_request_id, status, outcome FROM mutation_ledger
              WHERE operation_kind = ?1 AND session_uid = ?2
              ORDER BY started_at ASC",
        )
        .expect("prepare the ledger read");
    let rows = stmt
        .query_map(
            rusqlite::params![crate::store::OPERATION_ANSWER, uid],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read the ledger")
        .collect::<Result<Vec<_>, _>>()
        .expect("decode the ledger");
    rows
}

/// Every resolution this run has filed, in order.
fn resolutions(
    daemon: &Arc<crate::state::Daemon>,
    uid: &str,
) -> Vec<protocol::ws::CodexResolution> {
    daemon
        .store
        .events_after(uid, 0, 10_000)
        .expect("read the run's events")
        .into_iter()
        .filter(|e| e.kind == protocol::event::EventKind::ApprovalResolved)
        .map(|e| serde_json::from_value(e.payload).expect("a resolution decodes"))
        .collect()
}

/// How many `ApprovalRequest` facts this run has filed — one per card ever raised.
fn cards_ever_raised(daemon: &Arc<crate::state::Daemon>, uid: &str) -> usize {
    daemon
        .store
        .events_after(uid, 0, 10_000)
        .expect("read the run's events")
        .into_iter()
        .filter(|e| e.kind == protocol::event::EventKind::ApprovalRequest)
        .count()
}

/// **How long a gate may spend catching a claim between its write and its
/// settlement.**
///
/// The window is real and it is short: `codex_link`'s `DISPOSITION_BUDGET` is
/// 750 ms under `cfg(test)`, so a claim nobody has settled is made terminal
/// **in this process** three quarters of a second after the response is written —
/// which is a different ending from the one gate 4 is about. Polling at 1 ms over a
/// local SQLite read costs microseconds per pass, so the abort lands with the whole
/// budget still ahead of it; a gate that missed the window fails saying so rather
/// than asserting the wrong terminal.
const CLAIM_POLL: Duration = Duration::from_millis(1);

/// Poll until this card's claim is durably `applying`.
async fn wait_for_an_applying_claim(
    daemon: &Arc<crate::state::Daemon>,
    uid: &str,
    request_id: &str,
    budget: Duration,
) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if matches!(
            daemon.store.answer_status(uid, request_id),
            Ok(Some(crate::store::AnswerStatus::Applying))
        ) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(CLAIM_POLL).await;
    }
}

/// **Stage one phone answer so that it is written, actuates, and is never answered
/// for.**
///
/// The staging both fault gates below share. Everything in it is production code
/// except the [`GatedCcdLeg`], whose whole reasoning is written down where it is
/// defined: the reply direction is held so no disposition can come back, the write
/// direction is not, so the answer really leaves the daemon and really runs.
///
/// The claim is then waited for rather than assumed. It is taken by the link at the
/// one moment it knows the response is about to be written, so a gate that bounced
/// the daemon before seeing it durably `applying` would be bouncing past the state
/// it exists to catch — and would then assert the wrong terminal for the right
/// reason.
async fn stage_an_unanswered_write(
    daemon: &Arc<crate::state::Daemon>,
    gate: &GatedCcdLeg,
    uid: &str,
    card: &protocol::ws::ApprovalCard,
    option_id: &str,
) -> tokio::task::JoinHandle<protocol::ws::AnswerResult> {
    // Closed BEFORE the answer, so no reply can be in flight ahead of it. The
    // link→broker direction is untouched, so the write below really leaves the
    // daemon and really actuates.
    gate.hold();
    let answering = {
        let daemon = Arc::clone(daemon);
        let uid = uid.to_string();
        let request_id = card.request_id.clone();
        let payload_hash = card.payload_hash.clone();
        let option_id = option_id.to_string();
        tokio::spawn(async move {
            daemon
                .answer(
                    &request_id,
                    &payload_hash,
                    protocol::ws::AnswerDecision::OptionId { option_id },
                    Some(&uid),
                )
                .await
        })
    };
    assert!(
        wait_for_an_applying_claim(daemon, uid, &card.request_id, Duration::from_secs(20)).await,
        "the link never took a durable claim for {}, so there is no in-flight answer \
         for a restart to be caught by. ledger: {:?}",
        card.request_id,
        daemon.store.answer_status(uid, &card.request_id)
    );
    println!("STAGED — the answer is claimed and written, and nothing can answer for it");
    answering
}

/// **A phone answer caught in flight by a daemon bounce ends as ONE terminal
/// unknown, against ONE card and ONE ledger row** (plan Phase 3, gate 4).
///
/// The gate the whole `applying` state exists for. A claim under `answer` is taken
/// by the link at the one moment it knows the response is about to be written, and
/// settled when the broker says what became of it — so a claim that survives a
/// restart is, by construction, an answer nothing left alive can describe. The only
/// truthful ending is terminal `Unknown`, and *terminal* is the whole point: the
/// app-server accepts exactly one answer to a request, so a card whose answer might
/// have landed must never become answerable again.
///
/// Four things are asserted and they fail in four different ways:
///
///   * **one card ever, across a real rebind.** The bounced daemon does not merely
///     recover: it REGISTERS again, and its fresh link resumes onto the same thread —
///     which is the one path that can put a second copy of this question in front of
///     the observer. The re-delivery rule dedupes on `(threadId, itemId)` (measured in
///     [`a_card_raised_before_a_daemon_bounce_rebinds_onto_one_card`]), so a second
///     `ApprovalRequest` fact would mean a bounced daemon asked a phone the same
///     question twice. Without the rebind this assertion is about a producer that was
///     never given the chance to file the duplicate.
///   * **one ledger row.** Two rows under one request id would mean the claim key is
///     not the identity, and first-terminal-wins would be deciding between rows
///     rather than between outcomes.
///   * **the claim is terminal `indeterminate`, and the card is retired with exactly
///     one `Unknown{attempted_by: Phone}`** carrying the option the phone actually
///     named — which is the only thing left that can say what was attempted.
///   * **the phone is refused for ever after.** Not "there is no link": the ledger is
///     read before the link is asked, so the operator gets the sentence that says
///     this may have happened and will not be tried again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn a_claim_outstanding_across_a_daemon_bounce_becomes_one_terminal_unknown() {
    /// The cause `recover_codex_answers` files, verbatim. Pinned because it is what
    /// an operator reads on a card that can never be answered again, and a change to
    /// it is a change to what the fleet was told.
    const RECOVERED_CAUSE: &str =
        "this Mac stopped between writing the answer and learning what became of it";
    /// What the caller waiting on the aborted link is told. `LinkAnswers::answer`'s
    /// dropped-sender arm: the task took the ask and then went away, so the only
    /// truthful thing to say is that nothing here knows.
    const LOST_CALLER_SENTENCE: &str =
        "the link stopped while this answer was being written, so whether it reached \
         Codex is not known; it will not be sent again. Check the Mac.";
    /// What a second tap is told, once the ledger is terminal.
    const REFUSED_AFTERWARDS: &str =
        "an answer to this card was already sent and what became of it is not known; \
         it will not be sent again. Check the Mac.";

    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("bounce4");
    let mut coord = sb.spawn_coordinator(&codex);
    // Under the sandbox's own private directory, not /tmp: a marker in /tmp outlives
    // the run whenever the gate fails before its cleanup, and three of them were left
    // behind by earlier runs. `LiveSandbox` removes its base tree on drop, so the
    // marker goes with it however the gate ends.
    let marker = sb.base.join(format!("cc-3b-gate4.{}.txt", nanos()));
    let marker = marker.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&marker);

    wait_for_the_broker_and_the_tui(&sb).await;
    let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-live");
    let (daemon, db, _push) = live_daemon(&session);
    let uid = session.uid.clone();
    let gate = GatedCcdLeg::in_front_of(&sb).await;
    let registration = register_the_run_on(&daemon, &session, gate.path()).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let cards = || {
        daemon
            .store
            .codex_pending_approvals(&uid)
            .expect("read the run's open cards")
    };
    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(240),
            || !cards().is_empty(),
        )
        .await,
        "the observer never raised a card for the approval. pane:\n{}\nbroker.log:\n{}",
        sb.capture_pane(),
        read_file(&sb.run_dir.join("broker.log"))
    );
    let held = cards().remove(0);
    let card: protocol::ws::ApprovalCard =
        serde_json::from_str(&held.card).expect("the stored card decodes");
    println!("PANE WHILE THE APPROVAL IS UP:\n{}", sb.capture_pane());

    // ---- the answer, claimed and written, with nobody left to answer for it ----
    let answering = stage_an_unanswered_write(&daemon, &gate, &uid, &card, "accept").await;

    // ---- THE BOUNCE ---------------------------------------------------------
    // The supervisor's disconnect is how a link is retired in production, and it
    // ABORTS the task rather than winding it down — which is exactly the shape of a
    // daemon that stopped, because the link's own `settle_open_answers` teardown is
    // code the cancelled future never reaches.
    daemon.unregister_supervisor(&registration).await;
    let caught = daemon.store.answer_status(&uid, &card.request_id);
    println!("MEASURED claim at the moment of the bounce = {caught:?}");
    assert_eq!(
        caught.expect("read the ledger"),
        Some(crate::store::AnswerStatus::Applying),
        "the claim was already settled before the bounce, so this run staged a \
         different fault from the one gate 4 is about. Widen the staging rather than \
         weakening the assertion."
    );
    let told = answering.await.expect("the answering task");
    println!("MEASURED what the caller on the aborted link was told = {told:?}");

    let store = Arc::clone(&daemon.store);
    drop(daemon);
    let fresh = rebuild_the_daemon(&store);
    fresh.recover().await;
    println!("RECOVERED — a daemon that has only ever seen this store from disk");

    // ---- AND THEN IT REBINDS, which is what the plan's gate 4 is about ------
    //
    // A recovery on its own proves the ledger and the card were read back. It does not
    // prove the thing the gate is named for: a restarted daemon does not sit there, it
    // registers and its link resumes onto the same thread — and everything the
    // app-server replays into that resume goes through the observer that RAISES CARDS.
    // Without the rebind, "exactly one card ever" is a claim about a producer that was
    // never given the chance to file a second one.
    //
    // The valve is released first so the fresh link's own traffic is not held, and the
    // wait is through the resume: the link attaches, the answer describes the turn, and
    // the item this card was raised for is in it. Ten seconds is the same settling the
    // sibling gate uses, and it is bounded by the assertions rather than by hope — a
    // duplicate card would be visible the moment it was filed.
    gate.release();
    let rebound = register_the_run_on(&fresh, &session, gate.path()).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    println!(
        "REBOUND — the fresh daemon registered and its link resumed; cards now = {}",
        fresh
            .store
            .codex_pending_approvals(&uid)
            .expect("read the run's open cards")
            .len()
    );

    // ---- THE ASSERTIONS -----------------------------------------------------
    let ledger = answer_ledger(&db, &uid);
    let filed = resolutions(&fresh, &uid);
    let raised = cards_ever_raised(&fresh, &uid);
    let open = fresh
        .store
        .codex_pending_approvals(&uid)
        .expect("read the run's open cards");
    println!("MEASURED ledger = {ledger:?}");
    println!("MEASURED resolutions = {filed:?}");
    println!(
        "MEASURED cards ever raised = {raised}, open now = {}",
        open.len()
    );

    let again = fresh
        .answer(
            &card.request_id,
            &card.payload_hash,
            protocol::ws::AnswerDecision::OptionId {
                option_id: "accept".into(),
            },
            Some(&uid),
        )
        .await;
    println!("MEASURED a second tap after the bounce = {again:?}");
    println!("BROKER LOG:\n{}", read_file(&sb.run_dir.join("broker.log")));

    fresh.unregister_supervisor(&rebound).await;
    gate.close();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(&marker);

    assert_eq!(
        told,
        protocol::ws::AnswerResult::Rejected {
            reason: LOST_CALLER_SENTENCE.into()
        },
        "a caller whose link went away mid-answer must be told nothing here knows, \
         never that the answer was applied"
    );
    assert_eq!(
        raised, 1,
        "exactly one card may ever have existed for this item, ACROSS the rebind above; \
         a second would be a bounced daemon asking a phone the same question twice"
    );
    assert_eq!(
        ledger,
        vec![(card.request_id.clone(), "indeterminate".to_string(), None)],
        "exactly one ledger row for this request id, and it is terminal"
    );
    assert_eq!(
        filed,
        vec![protocol::ws::CodexResolution::Unknown {
            attempted_by: protocol::ws::ResolutionActor::Phone,
            attempted_decision: Some(protocol::ws::AnswerDecision::OptionId {
                option_id: "accept".into()
            }),
            write_stage: protocol::ws::WriteStage::UpstreamWriteUnconfirmed,
            cause: RECOVERED_CAUSE.into(),
        }],
        "the card is retired ONCE, saying who attempted what and that its fate is \
         not known"
    );
    assert!(
        open.is_empty(),
        "a card whose answer can never be resolved must not still be on the phone: {open:?}"
    );
    assert_eq!(
        again,
        protocol::ws::AnswerResult::Rejected {
            reason: REFUSED_AFTERWARDS.into()
        },
        "the phone may never answer this card again, and must be told why rather \
         than told there is no link"
    );
    println!(
        "GATE PASS — a claim outstanding across a daemon bounce became one terminal \
         unknown, on one card and one ledger row, and the phone can never answer it again"
    );
}

/// **A real `ccd` process, on a private root, that this test can kill.**
///
/// Every other gate in this file drives an in-process [`crate::state::Daemon`], and for
/// most of them that is the right instrument: the daemon under test IS the library, and a
/// child process would only put a socket between the assertions and the thing they are
/// about. Gate 5 is the one that cannot use it. Its subject is *abrupt death and a
/// reopened database* — a process that stops between writing an answer and learning what
/// became of it, whose SQLite connection is never closed, whose WAL is left exactly where
/// the kill found it, and whose successor has to open that file and decide. Dropping an
/// `Arc<Daemon>` and constructing another over the same live `Arc<Store>` reproduces none
/// of that: the store object survives, the connection is never torn, and the "restart"
/// inherits a database that was closed politely by a process that is still running.
///
/// So this spawns the real binary.
///
/// **Isolated from the operator's own daemon by construction, not by care.** A private
/// `CODECONNECT_HOME` moves the database, the IPC socket, the token, the TLS directory
/// and the config together (`protocol::root_dir`), and the config it writes pins the
/// listener to `127.0.0.1` on a port claimed and released a moment earlier — so the child
/// never asks tailscale for anything, never binds 8787, and cannot be reached from
/// outside this machine. The root is short (`/tmp/ccg5.…`) because a unix socket path is
/// capped at `SUN_LEN`, which the scratchpad path is far past.
struct CcdChild {
    home: PathBuf,
    port: u16,
    token: String,
    child: Child,
}

impl CcdChild {
    /// The `ccd` binary this harness drives, from this test binary's own target
    /// directory — `CARGO_BIN_EXE_*` is not handed to a unit test of the crate that owns
    /// the binary, which is the same reason [`resolve_codeconnect`] looks it up by path.
    fn binary() -> PathBuf {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "ccd", "--bin", "ccd"])
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
            .status()
            .expect("run cargo build -p ccd");
        assert!(status.success(), "cargo build -p ccd failed");
        let exe = std::env::current_exe().expect("this test binary's own path");
        let bin = exe
            .parent()
            .and_then(Path::parent)
            .expect("target/<profile>/deps/<test binary>")
            .join("ccd");
        assert!(
            is_executable_file(&bin),
            "the ccd binary this gate kills is not at {}",
            bin.display()
        );
        bin
    }

    /// Create the private root and start the first process on it.
    fn start() -> CcdChild {
        let home = PathBuf::from(format!("/tmp/ccg5.{}.{}", std::process::id(), nanos()));
        create_private_dir(&home).expect("the child's private root");
        // Claimed by holding it and letting it go, for `PhoneOverTheWire::connect`'s
        // reason: the child binds it itself, so the only way to learn a free one is to
        // have owned it a moment earlier.
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("claim a loopback port");
        let port = probe.local_addr().expect("the claimed port").port();
        drop(probe);
        std::fs::write(
            home.join("config.json"),
            format!(r#"{{"ws_port":{port},"ws_bind":"127.0.0.1","ws_loopback":true}}"#),
        )
        .expect("write the child's config");
        let child = CcdChild::spawn_on(&home);
        // Minted by the first start and stable across restarts, so the phone below keeps
        // its credential over the kill.
        let token = wait_for_file(&home.join("token"), Duration::from_secs(30));
        let mut ccd = CcdChild {
            home,
            port,
            token: token.trim().to_string(),
            child,
        };
        ccd.wait_until_listening();
        ccd
    }

    fn spawn_on(home: &Path) -> Child {
        Command::new(CcdChild::binary())
            .env("CODECONNECT_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                std::fs::File::create(home.join("ccd.stdout.log")).expect("the child's log"),
            ))
            .stderr(Stdio::from(
                std::fs::File::create(home.join("ccd.stderr.log")).expect("the child's log"),
            ))
            .spawn()
            .expect("spawn the real ccd binary")
    }

    /// Both listeners up: the IPC socket a supervisor registers over, and the loopback
    /// WebSocket a phone taps on.
    fn wait_until_listening(&mut self) {
        let sock = self.home.join("ccd.sock");
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let ipc = std::os::unix::fs::FileTypeExt::is_socket(
                &std::fs::metadata(&sock)
                    .map(|m| m.file_type())
                    .unwrap_or_else(|_| std::fs::metadata("/").unwrap().file_type()),
            );
            let ws = std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok();
            if ipc && ws {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "the ccd child never bound both listeners. stderr:\n{}",
            read_file(&self.home.join("ccd.stderr.log"))
        );
    }

    fn db(&self) -> PathBuf {
        self.home.join("events.db")
    }

    /// **SIGKILL, by the pid of the process this harness started.** Never by name and
    /// never by a command-line match: the operator's own daemon is running while this
    /// gate runs, and a sweep by argv would find it.
    fn kill(&mut self) {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", &self.child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.wait();
    }

    /// Start again on the same root — the same `events.db`, opened by a process that has
    /// never seen it, with whatever the kill left in the WAL.
    fn restart(&mut self) {
        self.child = CcdChild::spawn_on(&self.home);
        self.wait_until_listening();
    }

    /// Register the Codex run over the child's own IPC socket, exactly as the
    /// coordinator's `supervise_ready_session` does — the same
    /// [`protocol::ipc::ClientFrame::Register`], newline-framed. The connection is
    /// returned because the daemon treats the socket closing as the supervisor going
    /// away, so it has to be held for the life of the run.
    async fn register(&self, session: &SessionKey, codex_socket: &Path) -> UnixStream {
        let mut stream = UnixStream::connect(self.home.join("ccd.sock"))
            .await
            .expect("dial the ccd child's ipc socket");
        let frame = protocol::ipc::ClientFrame::Register(protocol::ipc::RegisterSession {
            session_id: session.name.clone(),
            session_uid: Some(session.uid.clone()),
            tmux_session: session.name.clone(),
            tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
            cwd: "/tmp".into(),
            supervisor_pid: std::process::id(),
            claude_bin: None,
            agent: protocol::agent::AgentKind::Codex,
            agent_bin: None,
            codex_thread_id: None,
            codex_socket: Some(codex_socket.to_string_lossy().into_owned()),
            codex_generation: Some(1),
            started_at: protocol::time::now_rfc3339(),
            protocol_minor: protocol::PROTOCOL_MINOR,
            exit_replay: false,
        });
        let mut line = serde_json::to_vec(&frame).expect("the registration serializes");
        line.push(b'\n');
        use tokio::io::AsyncWriteExt as _;
        stream
            .write_all(&line)
            .await
            .expect("write the registration");
        stream.flush().await.expect("flush the registration");
        stream
    }
}

impl Drop for CcdChild {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Poll for a file the child writes at startup, and return its contents.
fn wait_for_file(path: &Path, budget: Duration) -> String {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.trim().is_empty() {
                return text;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{} never appeared", path.display());
}

/// The `answer` rows one run has in a database this process does not own.
fn answer_ledger_at(db: &Path, uid: &str) -> Vec<(String, String, Option<String>)> {
    let conn = rusqlite::Connection::open(db).expect("open the child's database");
    let mut stmt = conn
        .prepare(
            "SELECT client_request_id, status, outcome FROM mutation_ledger
              WHERE operation_kind = ?1 AND session_uid = ?2
              ORDER BY started_at ASC",
        )
        .expect("prepare the ledger read");
    let rows = stmt
        .query_map(
            rusqlite::params![crate::store::OPERATION_ANSWER, uid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("read the ledger")
        .collect::<Result<Vec<_>, _>>()
        .expect("decode the ledger");
    rows
}

/// The open Codex cards one run has in a database this process does not own.
fn open_cards_at(db: &Path, uid: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).expect("open the child's database");
    let mut stmt = conn
        .prepare("SELECT card FROM codex_pending_approvals WHERE session_uid = ?1")
        .expect("prepare the card read");
    let rows = stmt
        .query_map(rusqlite::params![uid], |r| r.get(0))
        .expect("read the cards")
        .collect::<Result<Vec<String>, _>>()
        .expect("decode the cards");
    rows
}

/// The resolutions one run has filed, in a database this process does not own.
fn resolutions_at(db: &Path, uid: &str) -> Vec<protocol::ws::CodexResolution> {
    let conn = rusqlite::Connection::open(db).expect("open the child's database");
    let mut stmt = conn
        .prepare(
            "SELECT payload FROM events WHERE session_uid = ?1 AND kind = 'approval_resolved'
              ORDER BY seq ASC",
        )
        .expect("prepare the event read");
    let rows = stmt
        .query_map(rusqlite::params![uid], |r| r.get::<_, String>(0))
        .expect("read the events")
        .collect::<Result<Vec<String>, _>>()
        .expect("decode the events");
    rows.into_iter()
        .map(|p| serde_json::from_str(&p).expect("a resolution decodes"))
        .collect()
}

/// **A REAL `ccd` process, SIGKILLed after the write, records `Unknown` once when it is
/// started again on the same database — and the app-server is never sent a second
/// answer** (plan Phase 3, gate 5 — the one live kill).
///
/// The sibling of gate 4 and a strictly stronger statement about the same fault, in two
/// separate ways.
///
/// **It is a process, and the process really dies.** Gate 4 bounces an in-process daemon:
/// the `Arc<Store>` survives, its SQLite connection is closed politely, and the
/// "restart" inherits a file nothing ever abandoned. That is a faithful model of a
/// supervisor disconnect and no model at all of a kill. Here [`CcdChild`] runs the
/// shipping binary on a private `CODECONNECT_HOME`, `SIGKILL` ends it by the pid this
/// harness started — never by name, because the operator's own daemon is running beside
/// it — and the successor opens `events.db` with whatever the kill left in the WAL. That
/// is the abrupt-death-and-reopen shape, and nothing short of a child process has it.
///
/// **And the command really runs.** The valve holds only what comes BACK, so the answer
/// reached the broker, reached the app-server and actuated while the daemon that wrote it
/// was dying without ever being told. That is what turns "after the write" from a
/// description of the staging into a measured fact about the run: the bytes were out, the
/// actuation happened, and the daemon that recorded `Unknown` could not have known
/// either.
///
/// **"Never re-answered" is measured, not reasoned.** `relay.rs` writes one
/// `response disposition` line per response a `ccd` leg sends, in the same arm that
/// forwards it, so the count of those lines in the broker's own log is the count of
/// answers this daemon ever put upstream. Exactly one, across a kill, a restart, a
/// refused second tap and a fresh link that resumes onto the same thread.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn a_daemon_killed_after_the_write_records_unknown_and_never_answers_again() {
    /// Responses a `ccd` leg may put upstream for one approval, ever. The
    /// app-server accepts exactly one answer to a `serverRequest`; a second would be
    /// this daemon answering a question it had already recorded as unanswerable.
    const RESPONSES_UPSTREAM: usize = 1;

    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("kill5");
    let mut coord = sb.spawn_coordinator(&codex);
    // Sandbox-private, so a gate that fails early leaves nothing in /tmp.
    let marker = sb
        .base
        .join(format!("cc-3b-gate5.{}.txt", nanos()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&marker);

    wait_for_the_broker_and_the_tui(&sb).await;
    let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-live");
    let uid = session.uid.clone();
    let gate = GatedCcdLeg::in_front_of(&sb).await;

    // ---- the daemon under test is a process ---------------------------------
    let mut ccd = CcdChild::start();
    println!(
        "SPAWNED a real ccd on {} (pid {}), loopback :{}",
        ccd.home.display(),
        ccd.child.id(),
        ccd.port
    );
    let supervisor = ccd.register(&session, gate.path()).await;
    let db = ccd.db();
    let cards = || open_cards_at(&db, &uid);
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(240),
            || !cards().is_empty(),
        )
        .await,
        "the ccd child never raised a card for the approval. pane:\n{}\nchild stderr:\n{}",
        sb.capture_pane(),
        read_file(&ccd.home.join("ccd.stderr.log"))
    );
    let card: protocol::ws::ApprovalCard =
        serde_json::from_str(&cards().remove(0)).expect("the stored card decodes");
    assert!(
        !std::path::Path::new(&marker).exists(),
        "the command ran before anybody answered; this gate would prove nothing"
    );

    // ---- a phone answers it, and nothing will ever answer the phone ---------
    gate.hold();
    let mut phone = PhoneOverTheWire::connect_to(
        std::net::SocketAddr::from(([127, 0, 0, 1], ccd.port)),
        &ccd.token,
    )
    .await;
    phone.send_answer(&card, "accept", &uid).await;

    // The claim proves the child accepted the ask and wrote the response — read out of
    // its own database, because there is no in-process handle to ask.
    let claimed = wait_until(Duration::from_secs(20), || {
        answer_ledger_at(&db, &uid)
            .first()
            .map(|(_, status, _)| status == "applying")
            .unwrap_or(false)
    })
    .await;
    println!(
        "MEASURED claim before the kill = {:?}",
        answer_ledger_at(&db, &uid)
    );
    assert!(
        claimed,
        "the child never took a durable claim, so there is no in-flight answer for a \
         kill to be caught by. child stderr:\n{}",
        read_file(&ccd.home.join("ccd.stderr.log"))
    );

    // ---- THE KILL -----------------------------------------------------------
    ccd.kill();
    println!("KILLED — the process that wrote the answer is gone, mid-flight");
    drop(supervisor);
    phone.close();

    // ---- and the write really had landed ------------------------------------
    let actuated = wait_until(Duration::from_secs(120), || {
        std::path::Path::new(&marker).exists()
    })
    .await;
    println!("MEASURED actuated = {actuated}");

    // ---- a NEW process, on the database the kill left behind -----------------
    gate.release();
    ccd.restart();
    println!("RESTARTED — a process that has only ever seen this database from disk");
    let after_recovery = resolutions_at(&db, &uid);
    println!("MEASURED resolutions immediately after the restart = {after_recovery:?}");

    // A fresh link on the same thread, which is what a restarted daemon really does.
    // It is the one thing that could answer a second time, so it has to be here.
    let supervisor = ccd.register(&session, gate.path()).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    let mut phone = PhoneOverTheWire::connect_to(
        std::net::SocketAddr::from(([127, 0, 0, 1], ccd.port)),
        &ccd.token,
    )
    .await;
    let second_tap = phone
        .answer(&card, "accept", &uid, Duration::from_secs(30))
        .await;
    println!("MEASURED a second tap after the restart = {second_tap:?}");

    let broker_log = read_file(&sb.run_dir.join("broker.log"));
    let dispositions = ccd_dispositions(&broker_log);
    let ledger = answer_ledger_at(&db, &uid);
    let filed = resolutions_at(&db, &uid);
    let open = cards();
    println!("MEASURED ccd response dispositions = {dispositions:?}");
    println!("MEASURED ledger = {ledger:?}");
    println!("MEASURED resolutions = {filed:?}");
    println!("MEASURED open cards = {}", open.len());
    println!("PANE AT THE END:\n{}", sb.capture_pane());
    println!("BROKER LOG:\n{broker_log}");
    println!(
        "CHILD STDERR:\n{}",
        read_file(&ccd.home.join("ccd.stderr.log"))
    );

    phone.close();
    drop(supervisor);
    gate.close();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();

    assert!(
        actuated,
        "the app-server never ran the command, so the response never left the daemon \
         and this gate did not stage a kill AFTER the write"
    );
    assert_eq!(
        filed.len(),
        1,
        "exactly one terminal for this card, ever: {filed:?}"
    );
    assert!(
        matches!(
            filed.first(),
            Some(protocol::ws::CodexResolution::Unknown {
                attempted_by: protocol::ws::ResolutionActor::Phone,
                ..
            })
        ),
        "and it is the terminal unknown a phone claim of unproven delivery earns: {filed:?}"
    );
    assert_eq!(
        after_recovery, filed,
        "nothing after the restart may add to or rewrite the terminal it filed"
    );
    assert_eq!(
        ledger.len(),
        1,
        "one ledger row for one answer, across the whole run: {ledger:?}"
    );
    assert_eq!(
        ledger[0].1, "indeterminate",
        "and it is terminal: {ledger:?}"
    );
    assert!(
        matches!(second_tap, protocol::ws::AnswerResult::Rejected { .. }),
        "a terminal-unknown card is never answerable again: {second_tap:?}"
    );
    assert!(
        open.is_empty(),
        "no card may be left standing for a question nothing can answer: {open:?}"
    );
    assert_eq!(
        dispositions.len(),
        RESPONSES_UPSTREAM,
        "the ccd side put {} response(s) upstream for this approval; exactly \
         {RESPONSES_UPSTREAM} is the whole claim, because the app-server accepts one \
         answer to a serverRequest and a second would be this daemon answering a \
         question it had already recorded as unanswerable. lines: {dispositions:?}",
        dispositions.len()
    );
    println!(
        "GATE PASS — a real ccd process killed after the write recorded exactly one \
         Unknown when it was restarted on the same database, the write it had already \
         made actuated, and the ccd side never wrote a second answer"
    );
}

/// **A second ccd connection, resumed, that only watches.**
///
/// The instrument for any claim about ORDER. The production link consumes the frames
/// it acts on and files terminals from them, so a gate that has to say which of two
/// frames arrived first cannot ask the link — it needs a connection in the same
/// position that does nothing but record. The resume is what makes it one at all:
/// [`measure_the_approval_wire_on_the_ccd_leg`] measured that an unsubscribed leg is
/// handed no turn or approval traffic whatsoever.
async fn subscribed_tap(sb: &LiveSandbox, label: &'static str) -> (WireTap, String) {
    let mut raw = RawCcd::connect(&sb.ccd_sock()).await;
    assert!(
        raw.initialize().await["result"].is_object(),
        "the tap's initialize must be answered, or every frame it fails to see is a \
         fact about an unopened connection"
    );
    raw.notify("initialized", serde_json::json!({})).await;
    let mut thread_id = String::new();
    for _ in 0..60 {
        let loaded = raw
            .request(
                "thread/loaded/list",
                serde_json::json!({}),
                Duration::from_secs(20),
            )
            .await;
        if let Some(id) = loaded["result"]["data"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_str)
        {
            thread_id = id.to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        !thread_id.is_empty(),
        "no loaded thread for the tap to resume onto"
    );
    let mut tap = WireTap::split(raw, label);
    let mut resumed = false;
    for attempt in 0..12 {
        let id = 600 + attempt;
        tap.send(serde_json::json!({
            "id": id, "method": "thread/resume", "params": {"threadId": thread_id}
        }))
        .await;
        let frames = Arc::clone(&tap.frames);
        wait_until(Duration::from_secs(10), || {
            frames
                .lock()
                .expect("wire tap sink")
                .iter()
                .any(|v| v.get("id").and_then(Value::as_i64) == Some(id))
        })
        .await;
        if tap
            .seen()
            .iter()
            .any(|v| v.get("id").and_then(Value::as_i64) == Some(id) && v.get("result").is_some())
        {
            resumed = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(
        resumed,
        "the tap never subscribed, so nothing it fails to see is evidence about the wire"
    );
    (tap, thread_id)
}

/// **The keyboard and the phone answer one approval at once: one winner actuates,
/// and the loser is told something true** (plan Phase 3, gate 3).
///
/// The two halves of this gate are answered by different evidence on purpose.
///
/// **One winner** is the fleet's own record: exactly one `ApprovalResolved` fact and
/// exactly one settled ledger row, whichever side won. Two would mean a card
/// resolved twice, which is what the first-terminal-wins rule and the ledger's
/// `status = 'applying'` guard exist to make impossible — and a race is the only
/// thing that ever tests either.
///
/// **A truthful loser** is the *broker's* record, and it has to be, because the
/// daemon's sentence is derived from what the broker named and checking a derivation
/// against itself proves nothing. `relay.rs` logs
/// `Ccd: response disposition delivered=… winner=…` in the same arm that decides it,
/// so this gate reads the winner out of the broker's own log and requires the
/// sentence the phone was given to be the one [`LOSER_SENTENCE`] owes for exactly
/// that spelling. The case that matters is `winner=None`: `delivered:false` on its
/// own conflates losing to the keyboard, losing to another daemon, never holding the
/// capability, and being written into a socket that died — so a loser told "answered
/// at the Mac" on a disposition that named nobody would be a false statement three
/// times out of four.
///
/// **Which side wins is a measurement, not a requirement.** The gate asserts the
/// invariants that must hold either way, prints which happened, and lets the run say
/// so — a gate that demanded a particular winner would be asserting the scheduler.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn the_keyboard_and_the_phone_racing_one_approval_leave_one_truthful_winner() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("race3");
    let mut coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3b-gate3.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);

    wait_for_the_broker_and_the_tui(&sb).await;
    let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-live");
    let (daemon, db, _push) = live_daemon(&session);
    let uid = session.uid.clone();
    let registration = register_the_run(&daemon, &session, &sb).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let cards = || {
        daemon
            .store
            .codex_pending_approvals(&uid)
            .expect("read the run's open cards")
    };
    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(240),
            || !cards().is_empty(),
        )
        .await,
        "the observer never raised a card for the approval. pane:\n{}",
        sb.capture_pane()
    );
    let held = cards().remove(0);
    let card: protocol::ws::ApprovalCard =
        serde_json::from_str(&held.card).expect("the stored card decodes");
    assert!(
        sb.capture_pane()
            .contains("Would you like to run the following command?"),
        "the premise: the TUI is showing the prompt both answers are about. pane:\n{}",
        sb.capture_pane()
    );
    assert!(
        !std::path::Path::new(&marker).exists(),
        "the command ran before anybody answered; this gate would prove nothing"
    );

    // ---- THE RACE -----------------------------------------------------------
    // The answer is started first and then the key is pressed, with nothing awaited
    // between them: `send_keys` runs a `tmux` process, so pressing first would hand
    // the keyboard a head start measured in milliseconds. This is as close to
    // simultaneous as a harness driving one real TUI through a terminal can be.
    let answering = {
        let daemon = Arc::clone(&daemon);
        let uid = uid.clone();
        let request_id = card.request_id.clone();
        let payload_hash = card.payload_hash.clone();
        tokio::spawn(async move {
            daemon
                .answer(
                    &request_id,
                    &payload_hash,
                    protocol::ws::AnswerDecision::OptionId {
                        option_id: "accept".into(),
                    },
                    Some(&uid),
                )
                .await
        })
    };
    sb.send_keys(&["Enter"]);
    let result = answering.await.expect("the answering task");
    println!("MEASURED AnswerResult = {result:?}");

    let actuated = wait_until(Duration::from_secs(120), || {
        std::path::Path::new(&marker).exists()
    })
    .await;
    // Everything the wire had to say has to have landed before the counts are taken,
    // and the terminal is filed by whichever side won — so the wait is on the record
    // rather than on a sleep.
    let settled = wait_until(Duration::from_secs(60), || {
        !resolutions(&daemon, &uid).is_empty() && cards().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let broker_log = read_file(&sb.run_dir.join("broker.log"));
    let dispositions: Vec<String> = ccd_dispositions(&broker_log)
        .into_iter()
        .map(str::to_string)
        .collect();
    let filed = resolutions(&daemon, &uid);
    let ledger = answer_ledger(&db, &uid);
    let open = cards().len();
    println!("PANE AFTER THE RACE:\n{}", sb.capture_pane());
    println!("MEASURED actuated = {actuated}, settled = {settled}, open cards = {open}");
    println!("MEASURED ccd response dispositions = {dispositions:?}");
    println!("MEASURED resolutions = {filed:?}");
    println!("MEASURED ledger = {ledger:?}");
    println!("BROKER LOG:\n{broker_log}");

    daemon.unregister_supervisor(&registration).await;
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(&marker);

    // ---- what must hold whichever side won ----------------------------------
    assert!(
        actuated,
        "one of the two answers was `accept`, so the command must have run whoever won"
    );
    assert_eq!(
        dispositions.len(),
        1,
        "the ccd side wrote exactly one response, so the broker owes exactly one \
         disposition: {dispositions:?}"
    );
    assert_eq!(
        filed.len(),
        1,
        "a card is resolved once. Two terminals would mean first-terminal-wins did \
         not hold under the only conditions that test it: {filed:?}"
    );
    assert_eq!(ledger.len(), 1, "one answer, one ledger row: {ledger:?}");
    assert_eq!(open, 0, "the answered card must be off the phone");

    let line = &dispositions[0];
    let delivered = disposition_delivered(line).expect("the broker names delivered");
    let named = disposition_winner(line).expect("the broker names a winner field");

    if delivered {
        // ---- the phone won --------------------------------------------------
        println!("RACE OUTCOME = phone won (delivered=true, winner={named})");
        assert!(
            matches!(result, protocol::ws::AnswerResult::Applied { .. }),
            "the broker says the phone's bytes went upstream, so the phone must have \
             been told its answer was applied: {result:?}"
        );
        assert_eq!(
            ledger[0].2.as_deref(),
            Some("delivered"),
            "and the claim must be settled with the outcome a duplicate would replay"
        );
        assert_eq!(
            filed,
            vec![protocol::ws::CodexResolution::Answered {
                by: protocol::ws::ResolutionActor::Phone,
                decision: Some(protocol::ws::AnswerDecision::OptionId {
                    option_id: "accept".into()
                }),
            }],
            "a phone answer that WON must be recorded as the phone's, with the option \
             it named — never as `local`, which is what the terminals that follow the \
             resolution would file if the disposition had not already retired the card"
        );
    } else {
        // ---- the phone lost -------------------------------------------------
        println!("RACE OUTCOME = phone lost (delivered=false, winner={named})");
        let owed = LOSER_SENTENCE
            .iter()
            .find(|(spelling, _)| *spelling == named)
            .map(|(_, sentence)| *sentence)
            .unwrap_or_else(|| {
                panic!("the broker named a winner this build has no sentence for: {named}")
            });
        assert_eq!(
            result,
            protocol::ws::AnswerResult::Rejected {
                reason: owed.to_string()
            },
            "the loser's sentence must be the one the broker's own `winner={named}` \
             earns. A missing winner in particular may NOT be described as the Mac's \
             answer: it also covers losing to another daemon, to a capability never \
             held, and to a socket that died."
        );
        assert_eq!(
            ledger[0].2.as_deref(),
            Some("lost"),
            "a losing answer's claim is settled `lost`, which is what leaves the card \
             for the winner's own terminal to retire"
        );
        assert!(
            !matches!(
                filed[0],
                protocol::ws::CodexResolution::Answered {
                    by: protocol::ws::ResolutionActor::Phone,
                    ..
                }
            ),
            "the phone lost, so the fleet must not be told the phone answered: {filed:?}"
        );
    }
    println!(
        "GATE PASS — one winner actuated, one terminal was filed, and the loser was \
         told what the broker's own arbiter recorded"
    );
}

/// **`acceptForSession` on a file change is accepted by the real app-server, and the
/// edit lands** (plan Phase 3, "option variants round-trip by `option_id`").
///
/// The option that has the least evidence behind it anywhere in this build. A
/// command approval's options come off the wire in `availableDecisions`, so
/// answering with one is answering with something the server itself proposed. A file
/// change carries **no** option set at all (measured —
/// [`measure_what_a_file_change_approval_offers`]), so `acceptForSession` exists only
/// in `Family::labels`, a table this daemon owns, pinned to text somebody read off
/// the TUI's screen. Nothing but a live round trip can say whether the server
/// actually takes it.
///
/// And it is the one file-change option with a side effect beyond this decision: it
/// widens what the rest of the session may edit without asking. A card that offered
/// it and a server that refused it would be a button that does nothing; a card that
/// offered it and a server that took it as something else would be a widening
/// nobody chose.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn accept_for_session_on_a_file_change_is_taken_by_the_app_server() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("afs");
    let mut coord = sb.spawn_coordinator(&codex);
    let target = std::path::PathBuf::from(format!("/tmp/cc-3b-afs.{}.txt", nanos()));
    std::fs::write(&target, "hello from the codex approvals probe\n").expect("seed the target");

    wait_for_the_broker_and_the_tui(&sb).await;
    let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-live");
    let (daemon, db, _push) = live_daemon(&session);
    let uid = session.uid.clone();
    let registration = register_the_run(&daemon, &session, &sb).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let cards = || {
        daemon
            .store
            .codex_pending_approvals(&uid)
            .expect("read the run's open cards")
    };
    assert!(
        sb.submit_until(
            &format!(
                "Use apply_patch to edit {}, replacing the word hello with goodbye. \
                 Do not explain, just do it.",
                target.display()
            ),
            Duration::from_secs(300),
            || cards().iter().any(|card| card.family == "fileChange"),
        )
        .await,
        "the observer never raised a file-change card. pane:\n{}",
        sb.capture_pane()
    );
    let held = cards()
        .into_iter()
        .find(|card| card.family == "fileChange")
        .expect("the file-change card");
    let card: protocol::ws::ApprovalCard =
        serde_json::from_str(&held.card).expect("the stored card decodes");
    println!(
        "PANE WHILE THE FILE-CHANGE APPROVAL IS UP:\n{}",
        sb.capture_pane()
    );
    // The option this gate is about is on the card, under the id it will be answered
    // by — from this daemon's own table, because the wire offered none.
    assert_eq!(
        card.tool_input["options"][1]["id"],
        serde_json::json!("acceptForSession"),
        "the option under test must be the one the card offered: {}",
        card.tool_input["options"]
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "hello from the codex approvals probe\n",
        "the edit was applied before anybody answered; this gate would prove nothing"
    );

    let result = daemon
        .answer(
            &card.request_id,
            &card.payload_hash,
            protocol::ws::AnswerDecision::OptionId {
                option_id: "acceptForSession".into(),
            },
            Some(&uid),
        )
        .await;
    println!("MEASURED AnswerResult = {result:?}");

    let applied = wait_until(Duration::from_secs(120), || {
        std::fs::read_to_string(&target)
            .map(|body| body.contains("goodbye"))
            .unwrap_or(false)
    })
    .await;
    let dismissed = wait_until(Duration::from_secs(60), || {
        !sb.capture_pane()
            .contains("Would you like to make this change?")
    })
    .await;
    let status = daemon.store.answer_status(&uid, &card.request_id);
    let filed = resolutions(&daemon, &uid);
    let ledger = answer_ledger(&db, &uid);
    println!("PANE AFTER THE PHONE ANSWERED:\n{}", sb.capture_pane());
    println!("MEASURED applied = {applied}, dismissed = {dismissed}, status = {status:?}");
    println!("MEASURED resolutions = {filed:?}");
    println!("MEASURED ledger = {ledger:?}");
    println!("BROKER LOG:\n{}", read_file(&sb.run_dir.join("broker.log")));

    daemon.unregister_supervisor(&registration).await;
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(&target);

    assert!(
        matches!(result, protocol::ws::AnswerResult::Applied { .. }),
        "the app-server must accept a decision this daemon composed from its OWN \
         option table, or `acceptForSession` is a button that does nothing: {result:?}"
    );
    assert!(
        applied,
        "the app-server never wrote the edit the phone approved for the session"
    );
    assert_eq!(
        status.expect("read the answer ledger"),
        Some(crate::store::AnswerStatus::Settled("delivered".into())),
        "and the broker told this daemon the bytes went upstream"
    );
    assert_eq!(
        filed,
        vec![protocol::ws::CodexResolution::Answered {
            by: protocol::ws::ResolutionActor::Phone,
            decision: Some(protocol::ws::AnswerDecision::OptionId {
                option_id: "acceptForSession".into()
            }),
        }],
        "the fleet is told the phone answered, and with the exact option it named — \
         which is the only record of a widening that outlives this one decision"
    );
    println!(
        "GATE PASS — a live 0.153 app-server took `acceptForSession`, a decision \
         composed entirely from this daemon's own pane-measured option table"
    );
}

/// **A winning `cancel` is recorded as the phone's answer, not as a turn abort**
/// (plan Phase 3, "phone-first ⇒ …"; the provenance race, on the real wire).
///
/// `cancel` is the one decision whose own consequence can overwrite its record.
/// Measured on 0.153 (A25): a declined command lets the turn continue, while a
/// cancelled one **interrupts** it — and an interrupted turn's terminal retires every
/// card the visit was holding as [`protocol::ws::ClearCause::TurnAborted`]. Both
/// frames are broadcast to the same connection, microseconds apart, and both are
/// terminals for the same card. Whichever is filed first is what the fleet keeps.
///
/// So the honest answer — "the phone answered, and it chose cancel" — is available
/// only if the phone's terminal is filed by the loop that reads the disposition,
/// **before another frame is read**. That is exactly what
/// `Connection::note_response_disposition` does and exactly why it does it there
/// rather than on the waiting caller's task: the broker composes the disposition in
/// the arm that forwards the answer, so it is ahead of both of them on the answering
/// leg, and the two frames below then find the card already gone.
///
/// This gate is that design measured: it requires the interrupt to really arrive,
/// requires it to arrive fast, pins the order the two competing terminals actually
/// reach a watching leg in, and then requires the record to be
/// `Answered{Phone, cancel}` anyway.
///
/// The tap is what makes the ordering evidence rather than inference: the link
/// consumes what it acts on, so a second subscribed connection is the only thing
/// that can say which frame came first.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn a_winning_cancel_is_recorded_as_the_phones_answer_and_not_a_turn_abort() {
    /// **STOP AND AMEND: the plan states this ordering and the wire disagrees.**
    ///
    /// `internal/CODEX-PLAN.md` amendment **A25**, under its measured 0.153.2
    /// approval wire, records that "interrupt orders `turn/completed{interrupted}`
    /// before `resolved`".
    ///
    /// **Measured here, twice, against real codex 0.153.2: the opposite.** On a
    /// phone-driven `cancel`, a subscribed watching leg is handed the request's own
    /// `serverRequest/resolved` FIRST — at index 0 — and the interrupt's
    /// `turn/completed{interrupted}` six frames later, about 104 ms after the answer.
    /// Two runs, same indices, same magnitude. The measurement wins over the plan;
    /// A25's clause needs re-deriving rather than this constant being flipped to
    /// match it.
    ///
    /// **A hypothesis about why, which this gate did NOT test:** A25's ordering is
    /// probably the *keyboard* cancel, where the TUI issues its own `turn/interrupt`
    /// and that request's terminal therefore leads the approval's. Nothing here
    /// measured a keyboard cancel, so that is a reading offered for whoever amends
    /// the plan, not a second fact.
    ///
    /// **The provenance outcome is the same either way, and that is the point of
    /// pinning the order rather than depending on it.** Both frames are terminals for
    /// this card and each files a different resolution — `resolved` files
    /// `answered{by: local, decision: none}`, the aborted turn files
    /// `cleared{turn_aborted}` — but the broker composes the disposition in the arm
    /// that forwards the answer, so on the ANSWERING leg it precedes both, and
    /// `note_response_disposition` files `answered{by: phone}` before another frame is
    /// read. The two below then find the card already gone. A release that reordered
    /// them would not change that; it would change which wrong answer a regression
    /// produced, which is exactly why the order is asserted and not assumed.
    const RESOLUTION_LEADS_THE_INTERRUPT: bool = true;
    /// **How fast the cancelled turn terminalizes.** Not a tuning knob: an interrupt
    /// that took minutes could not race anything and the provenance question would
    /// not arise. Measured at ~0.1 s on 0.153.2; the ceiling is loose enough that a
    /// loaded machine does not fail the gate and tight enough that "fast" is an
    /// assertion rather than a description. The real elapsed time is printed.
    const INTERRUPT_BUDGET: Duration = Duration::from_secs(10);

    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("cancel");
    let mut coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3b-cancel.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);

    wait_for_the_broker_and_the_tui(&sb).await;
    let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-live");
    let (daemon, db, _push) = live_daemon(&session);
    let uid = session.uid.clone();
    let registration = register_the_run(&daemon, &session, &sb).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let (mut tap, thread_id) = subscribed_tap(&sb, "WATCH").await;
    println!("MEASURED thread under test = {thread_id}");

    let cards = || {
        daemon
            .store
            .codex_pending_approvals(&uid)
            .expect("read the run's open cards")
    };
    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(240),
            || !cards().is_empty(),
        )
        .await,
        "the observer never raised a card for the approval. pane:\n{}",
        sb.capture_pane()
    );
    let held = cards().remove(0);
    let card: protocol::ws::ApprovalCard =
        serde_json::from_str(&held.card).expect("the stored card decodes");
    let before = tap.seen().len();

    // ---- the phone cancels, unopposed ---------------------------------------
    let started = Instant::now();
    let result = daemon
        .answer(
            &card.request_id,
            &card.payload_hash,
            protocol::ws::AnswerDecision::OptionId {
                option_id: "cancel".into(),
            },
            Some(&uid),
        )
        .await;
    println!("MEASURED AnswerResult = {result:?}");

    let interrupted = |frames: &[Value]| {
        frames.iter().position(|f| {
            f["method"].as_str() == Some("turn/completed")
                && f.pointer("/params/turn/status").and_then(Value::as_str) == Some("interrupted")
        })
    };
    let frames_after = Arc::clone(&tap.frames);
    let saw_interrupt = wait_until(INTERRUPT_BUDGET, || {
        let frames = frames_after.lock().expect("wire tap sink");
        interrupted(&frames[before..]).is_some()
    })
    .await;
    let interrupt_took = started.elapsed();
    assert!(
        tap.barrier(Duration::from_secs(30)).await,
        "the tap must answer a barrier, or its frame list is a list from a corpse"
    );

    let after: Vec<Value> = tap.seen()[before..].to_vec();
    let interrupt_at = interrupted(&after);
    let resolved_at = after
        .iter()
        .position(|f| f["method"].as_str() == Some("serverRequest/resolved"));
    let filed = resolutions(&daemon, &uid);
    let ledger = answer_ledger(&db, &uid);
    let open = cards().len();
    println!("PANE AFTER THE CANCEL:\n{}", sb.capture_pane());
    println!("TAP methods after the answer: {:?}", tap.methods());
    println!(
        "MEASURED interrupt observed = {saw_interrupt} after {interrupt_took:?}; \
         turn/completed{{interrupted}} at index {interrupt_at:?}, \
         serverRequest/resolved at index {resolved_at:?}"
    );
    println!("MEASURED resolutions = {filed:?}");
    println!("MEASURED ledger = {ledger:?}, open cards = {open}");
    println!(
        "MEASURED marker created = {}",
        std::path::Path::new(&marker).exists()
    );
    println!("BROKER LOG:\n{}", read_file(&sb.run_dir.join("broker.log")));

    tap.handle.abort();
    daemon.unregister_supervisor(&registration).await;
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(&marker);

    assert!(
        matches!(result, protocol::ws::AnswerResult::Applied { .. }),
        "an unopposed cancel must be applied: {result:?}"
    );
    assert!(
        saw_interrupt,
        "a cancelled command approval must interrupt the turn, or this gate is not \
         about the frame that races the phone's terminal at all. TAP: {:?}",
        tap.methods()
    );
    let (interrupt_at, resolved_at) = (
        interrupt_at.expect("the interrupt was observed"),
        resolved_at.expect("the request's own resolution reached the tap"),
    );
    assert_eq!(
        resolved_at < interrupt_at,
        RESOLUTION_LEADS_THE_INTERRUPT,
        "the measured order of the two terminals moved: serverRequest/resolved at \
         {resolved_at}, turn/completed{{interrupted}} at {interrupt_at}. Both are \
         terminals for this card and they file different resolutions, so re-derive \
         which one a link would reach first before moving this constant."
    );
    assert!(
        !std::path::Path::new(&marker).exists(),
        "a cancelled command must not have run"
    );
    assert_eq!(ledger.len(), 1, "one answer, one ledger row: {ledger:?}");
    assert_eq!(
        ledger[0].2.as_deref(),
        Some("delivered"),
        "the winning cancel's claim is settled with the outcome a duplicate replays"
    );
    assert_eq!(
        filed,
        vec![protocol::ws::CodexResolution::Answered {
            by: protocol::ws::ResolutionActor::Phone,
            decision: Some(protocol::ws::AnswerDecision::OptionId {
                option_id: "cancel".into()
            }),
        }],
        "the record must be the phone's answer and the option it named. \
         `cleared{{turn_aborted}}` is what the interrupt this very cancel caused would \
         file, and this run measured it arriving at {interrupt_at} against \
         serverRequest/resolved at {resolved_at} — see RESOLUTION_LEADS_THE_INTERRUPT \
         above, which is a measured constant and not an invariant. Whichever of the two \
         came first, the terminal must be filed by the disposition that knew who \
         answered rather than by whichever frame won that race."
    );
    assert_eq!(open, 0, "the cancelled card must be off the phone");
    println!(
        "GATE PASS — a winning cancel interrupted the turn in {interrupt_took:?}; both \
         competing terminals reached a watching leg (resolved at {resolved_at}, the \
         aborted turn at {interrupt_at}) and the fleet still records \
         Answered{{Phone, cancel}} rather than Cleared{{TurnAborted}}"
    );
}

/// **A phone answer the keyboard beat is told what the broker's own arbiter
/// recorded** (plan Phase 3, gate 3's other half).
///
/// [`the_keyboard_and_the_phone_racing_one_approval_leave_one_truthful_winner`] runs
/// the two answers as close to together as this harness can and measures which won.
/// It has never been the keyboard, and the reason is structural rather than
/// interesting: the phone's answer is a function call in this process while the
/// keyboard's is a `tmux` fork, a keypress, a TUI redraw and a second socket. So the
/// losing branch — the one where the daemon has to say something true about an answer
/// that went nowhere — is not reachable by racing.
///
/// It is reachable by **ordering**, and this gate orders it with the same
/// [`GatedCcdLeg`] the fault gates use. The keyboard answers first and really wins:
/// its response reaches the app-server on the TUI's own leg, which this gate never
/// touches, and the broker's arbiter records `tui` for the request. What the valve
/// holds is only the `serverRequest/resolved` coming back to **ccd** — so the link
/// has not yet learned the question is settled, and still holds the wire id. The
/// phone then answers into exactly the state production produces whenever a phone is
/// a few milliseconds slow: a card that is still open, on a request that is already
/// decided.
///
/// A separate, unproxied tap is what says when to answer. It is subscribed to the
/// same broadcast, so it sees the `serverRequest/resolved` the link is being held
/// back from — which makes "the keyboard has won" an observation rather than a sleep.
///
/// The assertion that matters is the **sentence**, checked against the broker's log
/// rather than against the daemon's own reasoning: `settle_lost_answer` derives what
/// the phone is told from the `winner` the broker named, so checking it against
/// anything the daemon computed would be checking a derivation against itself.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn a_phone_answer_that_lost_to_the_keyboard_is_told_what_the_broker_named() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("loser3");
    let mut coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3b-loser3.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);

    wait_for_the_broker_and_the_tui(&sb).await;
    let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-live");
    let (daemon, db, _push) = live_daemon(&session);
    let uid = session.uid.clone();
    let gate = GatedCcdLeg::in_front_of(&sb).await;
    let registration = register_the_run_on(&daemon, &session, gate.path()).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    // Unproxied on purpose: this connection has to see what the link is being held
    // back from, so it dials the broker's own leg.
    let (tap, thread_id) = subscribed_tap(&sb, "WATCH").await;
    println!("MEASURED thread under test = {thread_id}");

    let cards = || {
        daemon
            .store
            .codex_pending_approvals(&uid)
            .expect("read the run's open cards")
    };
    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(240),
            || !cards().is_empty(),
        )
        .await,
        "the observer never raised a card for the approval. pane:\n{}",
        sb.capture_pane()
    );
    let held = cards().remove(0);
    let card: protocol::ws::ApprovalCard =
        serde_json::from_str(&held.card).expect("the stored card decodes");

    // ---- the keyboard answers, and the link is not told ---------------------
    gate.hold();
    let before = tap.seen().len();
    sb.send_keys(&["Enter"]);
    let frames = Arc::clone(&tap.frames);
    let keyboard_won = wait_until(Duration::from_secs(60), || {
        frames.lock().expect("wire tap sink")[before..]
            .iter()
            .any(|f| f["method"].as_str() == Some("serverRequest/resolved"))
    })
    .await;
    assert!(
        keyboard_won,
        "the keyboard's answer never resolved the request, so there is no winner for \
         the phone to lose to. pane:\n{}\nTAP: {:?}",
        sb.capture_pane(),
        tap.methods()
    );
    assert!(
        !cards().is_empty(),
        "the link learned the request was resolved despite the valve, so the phone \
         would be refused by the card lookup rather than by the arbiter — which is a \
         different refusal from the one this gate is about"
    );
    println!("KEYBOARD WON — and the link has not been told");

    // ---- and only now does the phone answer ---------------------------------
    let answering = {
        let daemon = Arc::clone(&daemon);
        let uid = uid.clone();
        let request_id = card.request_id.clone();
        let payload_hash = card.payload_hash.clone();
        tokio::spawn(async move {
            daemon
                .answer(
                    &request_id,
                    &payload_hash,
                    protocol::ws::AnswerDecision::OptionId {
                        option_id: "accept".into(),
                    },
                    Some(&uid),
                )
                .await
        })
    };
    // The claim proves the link accepted the ask and wrote the response — which is
    // what makes this a LOST answer rather than one that was never addressable.
    //
    // **Bounded by the gap this gate actually has, and ASSERTED.** `DISPOSITION_BUDGET`
    // is 750 ms under `cfg(test)`, so a claim nobody settles is made terminal by this
    // very process three quarters of a second after the write — a poll that ran to
    // 500 ms left 250 ms of margin, and a `println!` where the assertion should have
    // been meant a miss degraded the gate silently into measuring the wrong ending.
    // 400 ms is the poll; missing it is a failure that says so.
    let claimed =
        wait_for_an_applying_claim(&daemon, &uid, &card.request_id, Duration::from_millis(400))
            .await;
    println!("MEASURED the phone's answer reached the wire (claimed) = {claimed}");
    assert!(
        claimed,
        "the link never took a durable claim, so this run staged an answer that was \
         never addressable rather than one that LOST. ledger: {:?}",
        daemon.store.answer_status(&uid, &card.request_id)
    );
    gate.release();
    let result = answering.await.expect("the answering task");
    println!("MEASURED AnswerResult = {result:?}");

    let settled = wait_until(Duration::from_secs(60), || {
        !resolutions(&daemon, &uid).is_empty() && cards().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let broker_log = read_file(&sb.run_dir.join("broker.log"));
    let dispositions: Vec<String> = ccd_dispositions(&broker_log)
        .into_iter()
        .map(str::to_string)
        .collect();
    let filed = resolutions(&daemon, &uid);
    let ledger = answer_ledger(&db, &uid);
    let actuated = std::path::Path::new(&marker).exists();
    println!("PANE AFTER BOTH ANSWERS:\n{}", sb.capture_pane());
    println!("MEASURED settled = {settled}, actuated = {actuated}");
    println!("MEASURED ccd response dispositions = {dispositions:?}");
    println!("MEASURED resolutions = {filed:?}");
    println!("MEASURED ledger = {ledger:?}");
    println!("BROKER LOG:\n{broker_log}");

    tap.handle.abort();
    daemon.unregister_supervisor(&registration).await;
    gate.close();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(&marker);

    assert!(
        claimed,
        "the link never claimed the phone's answer, so nothing was written and this \
         gate measured a refusal rather than a loss"
    );
    assert!(
        actuated,
        "the keyboard accepted, so the command must have run — once, by the winner"
    );
    assert_eq!(
        dispositions.len(),
        1,
        "the ccd side wrote exactly one response: {dispositions:?}"
    );
    let line = &dispositions[0];
    assert_eq!(
        disposition_delivered(line),
        Some(false),
        "a response the arbiter dropped must be reported as having delivered nothing: \
         {line}"
    );
    let named = disposition_winner(line).expect("the broker names a winner field");
    assert_eq!(
        named, "Some(Tui)",
        "the keyboard won, so the broker's arbiter must be able to NAME it — a \
         `winner=None` here would mean the daemon could only say 'something else' \
         about an answer the Mac demonstrably gave: {line}"
    );
    let owed = LOSER_SENTENCE
        .iter()
        .find(|(spelling, _)| *spelling == named)
        .map(|(_, sentence)| *sentence)
        .expect("this build has a sentence for every winner the broker names");
    assert_eq!(
        result,
        protocol::ws::AnswerResult::Rejected {
            reason: owed.to_string()
        },
        "the loser's sentence must be the one the broker's own `winner={named}` earns"
    );
    assert_eq!(ledger.len(), 1, "one answer, one ledger row: {ledger:?}");
    assert_eq!(
        ledger[0].2.as_deref(),
        Some("lost"),
        "a losing answer's claim is settled `lost` — which is what leaves the card \
         standing for the WINNER's own terminal to retire, rather than filing a \
         resolution naming a decision nothing applied"
    );
    assert_eq!(
        filed,
        vec![protocol::ws::CodexResolution::Answered {
            by: protocol::ws::ResolutionActor::Local,
            decision: None,
        }],
        "the card is retired by the wire's own terminal, which carries no decision — \
         `answered{{by: local}}` with the choice absent is the honest reading, and \
         recording the phone's option here would be recording a decision that was \
         never applied"
    );
    println!(
        "GATE PASS — a phone answer the keyboard beat was dropped by the arbiter, \
         settled `lost`, retired as the Mac's answer, and the phone was told exactly \
         what the broker named"
    );
}

// ------------------------------------------------ approval quiescence (D3)

/// **MEASUREMENT: can the wire produce a thread switch while an approval is
/// unanswered?**
///
/// D3 was designed before the approval wire had been measured and before a phone
/// could answer anything. Its subject is a switch (`/new`, resume, fork) that
/// crosses an admitted answer, and every gate it enumerates presumes that crossing
/// exists. Nothing here builds a barrier: this establishes, on the real 0.153 wire,
/// whether the crossing is producible at all.
///
/// The probes run in order of how much they destroy, so a later one can never be
/// the reason an earlier one had nothing to see:
///
///   1. **The overlay's own reading of a keystroke.** `/new` is typed at the
///      keyboard with the approval prompt up and NOTHING is submitted — the pane
///      before and after is the whole evidence, because on this TUI `Enter` is the
///      overlay's own accept and pressing it would answer the very approval the
///      probe needs pending.
///   2. **What the broker's TUI leg was handed.** `/new` is `thread/unsubscribe`,
///      `thread/unsubscribe`, `thread/start` (2e-4c), and the broker logs its
///      decision for each. A switch that never reached the leg leaves no line.
///   3. **Only then, the destructive one.** `Enter` is pressed, so whatever the
///      overlay does with it is recorded rather than guessed at, and the run ends
///      with the approval settled one way or the other.
///
/// It asserts only the premises (an approval really was pending, unanswered, on a
/// leg that really was subscribed). Everything else is printed: which way the wire
/// answers is the finding, and a gate that demanded an answer would be asserting
/// the design rather than measuring the wire.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn measure_a_thread_switch_while_an_approval_is_pending() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("q3c");
    let mut coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3c-quiesce.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);

    wait_for_the_broker_and_the_tui(&sb).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let (mut sub, thread_a) = subscribed_tap(&sb, "SUB").await;
    println!("MEASURED thread A = {thread_a}");

    // ---- the approval this whole probe is about ---------------------------
    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(240),
            || sub
                .methods()
                .iter()
                .any(|m| m.ends_with("/requestApproval")),
        )
        .await,
        "no approval reached the subscribed ccd leg. pane:\n{}",
        sb.capture_pane()
    );
    let request = sub
        .first("item/commandExecution/requestApproval")
        .expect("the command-execution approval frame");
    let wire_id = request["id"]
        .as_i64()
        .expect("a server-request carries a numeric id");
    let pane_pending = sb.capture_pane();
    println!("PANE WITH THE APPROVAL PENDING:\n{pane_pending}");
    println!("MEASURED wire id = {wire_id}");
    assert!(
        pane_pending.contains("Would you like to run the following command?"),
        "the premise: the TUI is showing the prompt this probe is about. pane:\n{pane_pending}"
    );
    assert!(
        !std::path::Path::new(&marker).exists(),
        "the command ran before anybody answered; nothing below would be about a \
         PENDING approval"
    );
    let methods_before = sub.methods();
    let broker_log_before = read_file(&sb.run_dir.join("broker.log"));

    // ---- PROBE 1: type `/new` and submit nothing --------------------------
    sb.send_keys(&["/new"]);
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let pane_typed = sb.capture_pane();
    println!("PANE AFTER TYPING `/new` WITH NOTHING SUBMITTED:\n{pane_typed}");
    let overlay_survived = pane_typed.contains("Would you like to run the following command?");
    let text_landed = pane_typed.contains("/new");
    println!(
        "MEASURED overlay still up = {overlay_survived}, `/new` visible anywhere on the \
         pane = {text_landed}"
    );

    // ---- PROBE 2: what the broker's TUI leg was handed --------------------
    // `/new` is unsubscribe, unsubscribe, thread/start. If the TUI never sent any of
    // them, the switch never left the terminal emulator and the broker has nothing
    // to have decided about.
    let broker_log_typed = read_file(&sb.run_dir.join("broker.log"));
    let new_lines: Vec<&str> = broker_log_typed
        .lines()
        .filter(|line| !broker_log_before.lines().any(|old| old == *line))
        .collect();
    println!("BROKER LOG LINES ADDED WHILE `/new` WAS TYPED:\n{new_lines:#?}");
    println!(
        "MEASURED ccd-leg methods added while `/new` was typed = {:?}",
        sub.methods()
            .into_iter()
            .skip(methods_before.len())
            .collect::<Vec<_>>()
    );
    let switch_reached_the_broker = new_lines
        .iter()
        .any(|line| line.contains("thread/unsubscribe") || line.contains("thread/start"));
    println!("MEASURED a switch frame reached the broker = {switch_reached_the_broker}");

    // ---- PROBE 3: the destructive one -------------------------------------
    // Everything above is now recorded, so what `Enter` reaches can be measured
    // rather than avoided.
    sb.send_keys(&["Enter"]);
    let actuated = wait_until(Duration::from_secs(120), || {
        std::path::Path::new(&marker).exists()
    })
    .await;
    let resolved = wait_until(Duration::from_secs(60), || {
        sub.methods().iter().any(|m| m == "serverRequest/resolved")
    })
    .await;
    let _ = sub.barrier(Duration::from_secs(30)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let pane_after_enter = sb.capture_pane();
    let broker_log_final = read_file(&sb.run_dir.join("broker.log"));
    println!("PANE AFTER `Enter`:\n{pane_after_enter}");
    println!("MEASURED command actuated = {actuated}, serverRequest/resolved = {resolved}");
    println!("FINAL SUB methods: {:?}", sub.methods());
    println!("BROKER LOG (final):\n{broker_log_final}");

    // ---- the capture -------------------------------------------------------
    let capture = match std::env::var("CC_CODEX_3C_CAPTURE") {
        Ok(path) => PathBuf::from(path),
        Err(_) => sb.run_dir.join("quiesce-capture.jsonl"),
    };
    let mut lines = String::new();
    for frame in sub.seen() {
        lines.push_str(
            &serde_json::json!({"conn": "ccd-subscribed", "dir": "s2c", "frame": frame})
                .to_string(),
        );
        lines.push('\n');
    }
    std::fs::write(&capture, &lines).expect("write the quiescence capture");
    println!("CAPTURE WRITTEN: {}", capture.display());
    let panes = capture.with_extension("panes.txt");
    std::fs::write(
        &panes,
        format!(
            "--- pane: approval pending ---\n{pane_pending}\n\
             --- pane: after typing /new, nothing submitted ---\n{pane_typed}\n\
             --- pane: after Enter ---\n{pane_after_enter}\n\
             --- broker.log lines added while /new was typed ---\n{new_lines:#?}\n"
        ),
    )
    .expect("write the pane record");
    println!("PANES WRITTEN: {}", panes.display());

    sub.handle.abort();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(&marker);
}

/// Every broker-log line written after the first `from` lines.
///
/// **Positional, never by content.** A content diff was written first and it was
/// wrong in the one way that mattered: the switch prefix logs the SAME line twice
/// (`/new` sends two `thread/unsubscribe` frames), and identical lines already in
/// the file made a real switch read as no switch at all — which is exactly the
/// reading a probe like this must never produce by accident.
fn log_lines_after(log: &str, from: usize) -> Vec<&str> {
    log.lines().skip(from).collect()
}

/// The thread ids this leg has been told started, in order.
fn started_threads(tap: &WireTap) -> Vec<String> {
    tap.seen()
        .iter()
        .filter(|v| v["method"].as_str() == Some("thread/started"))
        .filter_map(|v| v["params"]["thread"]["id"].as_str().map(str::to_string))
        .collect()
}

/// **MEASUREMENT: is there any producer at all of a thread switch while an approval
/// is pending?**
///
/// The first probe typed `/new` with the prompt up and the approval came back
/// **declined** — one of those four characters was the prompt's own "No", and the
/// remainder reached the composer only once the question was already terminal. That
/// answers "can the operator compose a command", and leaves four things unmeasured.
/// All four are checked here, on one live session, in an order chosen so that no
/// probe can be the reason a later one had nothing to see:
///
///   1. **Keys the prompt has no use for.** `/` and `z` are neither accept hotkey,
///      neither `esc`, neither an option number — so what the pane does with them
///      says whether the composer is reachable at all while the prompt is up, or
///      merely hostile to the letters `/new` happens to contain.
///   2. **The ccd leg's own two frames** — `thread/start`, and the
///      `thread/unsubscribe` that reserves a switch behind it.
///   3. **A second connection on the TUI leg.** That leg accepts more than one
///      connection by design (the real `/resume` picker opens a second), so it is
///      the last position a `thread/start` could arrive from.
///
/// The fourth producer — `/new` typed while a turn runs and no approval is up — is
/// measured on its own session in
/// [`measure_whether_new_is_offered_while_a_turn_runs`], because it turns out to
/// take the pane somewhere the probes below cannot be run from.
///
/// **The control is the point of the test.** "No switch frames" proves nothing
/// unless the same detector can see a switch that really happens, so once the
/// approval is settled the same `/new` is typed into the same pane, and the switch
/// must show up on both instruments — a `thread/started` naming a new thread on the
/// ccd leg, and the broker's own forward lines. Without it, an unproducible switch
/// and a blind probe read identically.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn measure_what_else_could_switch_a_thread_while_an_approval_is_pending() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("p3c");
    let mut coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3c-producers.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);
    let broker_log = sb.run_dir.join("broker.log");

    wait_for_the_broker_and_the_tui(&sb).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let (mut sub, thread_a) = subscribed_tap(&sb, "SUB").await;
    println!("MEASURED thread A = {thread_a}");

    // ---- the approval every remaining probe is about ----------------------
    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(240),
            || sub
                .methods()
                .iter()
                .any(|m| m.ends_with("/requestApproval")),
        )
        .await,
        "no approval reached the subscribed ccd leg. pane:\n{}",
        sb.capture_pane()
    );
    let request = sub
        .first("item/commandExecution/requestApproval")
        .expect("the command-execution approval frame");
    let wire_id = request["id"]
        .as_i64()
        .expect("a server-request carries a numeric id");
    let pane_pending = sb.capture_pane();
    println!("PANE WITH THE APPROVAL PENDING:\n{pane_pending}");
    assert!(
        pane_pending.contains("Would you like to run the following command?"),
        "the premise: the TUI is showing the prompt this probe is about. pane:\n{pane_pending}"
    );
    let pending_from = read_file(&broker_log).lines().count();
    let started_at_pending = started_threads(&sub);

    // ---- P1: keys the prompt has no use for -------------------------------
    sb.send_keys(&["/"]);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let pane_slash = sb.capture_pane();
    println!("PANE AFTER A BARE `/`:\n{pane_slash}");
    sb.send_keys(&["z"]);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let pane_z = sb.capture_pane();
    println!("PANE AFTER `/` THEN `z`:\n{pane_z}");
    println!(
        "MEASURED prompt survived `/` = {}, survived `/z` = {}",
        pane_slash.contains("Would you like to run the following command?"),
        pane_z.contains("Would you like to run the following command?")
    );

    // ---- P2: the ccd leg's own two frames ---------------------------------
    // Smallest well-formed params on purpose: what is measured is the ROLE, and a
    // refusal naming a bad parameter would be measuring this harness instead.
    let start_id = 8100;
    sub.send(serde_json::json!({
        "id": start_id, "method": "thread/start", "params": {"cwd": "/tmp"}
    }))
    .await;
    let unsub_id = 8101;
    sub.send(serde_json::json!({
        "id": unsub_id, "method": "thread/unsubscribe", "params": {"threadId": thread_a}
    }))
    .await;
    let _ = sub.barrier(Duration::from_secs(30)).await;
    let seen_after_ccd_probes = sub.seen();
    let answer_to = |id: i64| {
        seen_after_ccd_probes
            .iter()
            .find(|v| v.get("id").and_then(Value::as_i64) == Some(id) && v["method"].is_null())
            .cloned()
    };
    let ccd_start_answer = answer_to(start_id);
    let ccd_unsub_answer = answer_to(unsub_id);
    println!("MEASURED ccd thread/start answered: {ccd_start_answer:?}");
    println!("MEASURED ccd thread/unsubscribe answered: {ccd_unsub_answer:?}");

    // ---- P3: a second connection on the TUI leg ---------------------------
    let (second_tui_init, second_tui_start) = {
        let mut raw = RawCcd::connect(&sb.run_dir.join("tui.sock")).await;
        let init = raw.initialize().await;
        println!("MEASURED second TUI-leg connection initialize: {init}");
        raw.notify("initialized", serde_json::json!({})).await;
        let started = raw
            .request(
                "thread/start",
                serde_json::json!({"cwd": "/tmp"}),
                Duration::from_secs(30),
            )
            .await;
        println!("MEASURED second TUI-leg connection thread/start: {started}");
        (init, started)
    };

    // ---- what the probes did and did not produce --------------------------
    let pane_before_answer = sb.capture_pane();
    println!("PANE BEFORE THE ANSWER:\n{pane_before_answer}");
    let still_pending = pane_before_answer.contains("Would you like to run the following command?");
    println!("MEASURED the approval was STILL pending through every probe = {still_pending}");
    let probe_lines: Vec<String> = log_lines_after(&read_file(&broker_log), pending_from)
        .into_iter()
        .map(str::to_string)
        .collect();
    println!("BROKER LOG LINES ADDED BY THE PROBES:\n{probe_lines:#?}");
    let started_during_pending: Vec<String> = started_threads(&sub)
        .into_iter()
        .skip(started_at_pending.len())
        .collect();
    println!(
        "MEASURED threads started while the approval was pending = {started_during_pending:?}"
    );
    let forwarded_during_pending: Vec<&String> = probe_lines
        .iter()
        .filter(|line| line.contains("forward"))
        .collect();
    println!("MEASURED frames FORWARDED while pending = {forwarded_during_pending:?}");

    // ---- settle it, from the leg the design is about ----------------------
    sub.send(serde_json::json!({"id": wire_id, "result": {"decision": "accept"}}))
        .await;
    let actuated = wait_until(Duration::from_secs(120), || {
        std::path::Path::new(&marker).exists()
    })
    .await;
    println!("MEASURED the phone's answer actuated = {actuated}");
    assert!(
        wait_until(Duration::from_secs(90), || !sb
            .capture_pane()
            .contains("Would you like to run the following command?"))
        .await,
        "the prompt never left the pane after the answer. pane:\n{}",
        sb.capture_pane()
    );
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ---- THE CONTROL: the same `/new`, on the same pane, now idle ---------
    let control_from = read_file(&broker_log).lines().count();
    let started_before_control = started_threads(&sub);
    sb.send_keys(&["/new"]);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let pane_control_typed = sb.capture_pane();
    println!("PANE AFTER TYPING `/new` WITH THE SESSION IDLE:\n{pane_control_typed}");
    sb.send_keys(&["Enter"]);
    let switched = wait_until(Duration::from_secs(90), || {
        started_threads(&sub).len() > started_before_control.len()
    })
    .await;
    let control_lines: Vec<String> = log_lines_after(&read_file(&broker_log), control_from)
        .into_iter()
        .map(str::to_string)
        .collect();
    let pane_after_control = sb.capture_pane();
    println!("PANE AFTER THE CONTROL `/new`:\n{pane_after_control}");
    println!("BROKER LOG LINES ADDED BY THE CONTROL `/new`:\n{control_lines:#?}");
    println!(
        "MEASURED the control switch was detected = {switched}; threads started = {:?}",
        started_threads(&sub)
    );

    let panes = match std::env::var("CC_CODEX_3C_PANES") {
        Ok(path) => PathBuf::from(path),
        Err(_) => sb.run_dir.join("producers-panes.txt"),
    };
    std::fs::write(
        &panes,
        format!(
            "--- pane: approval pending ---\n{pane_pending}\n\
             --- pane: after a bare `/` ---\n{pane_slash}\n\
             --- pane: after `/` then `z` ---\n{pane_z}\n\
             --- pane: before the answer ---\n{pane_before_answer}\n\
             --- ccd thread/start answer ---\n{ccd_start_answer:?}\n\
             --- ccd thread/unsubscribe answer ---\n{ccd_unsub_answer:?}\n\
             --- second TUI-leg connection initialize ---\n{second_tui_init}\n\
             --- second TUI-leg connection thread/start ---\n{second_tui_start}\n\
             --- broker.log lines added by the probes ---\n{probe_lines:#?}\n\
             --- threads started while the approval was pending ---\n{started_during_pending:?}\n\
             --- pane: control `/new` typed, session idle ---\n{pane_control_typed}\n\
             --- pane: after the control `/new` ---\n{pane_after_control}\n\
             --- broker.log lines added by the control ---\n{control_lines:#?}\n"
        ),
    )
    .expect("write the producer record");
    println!("PANES WRITTEN: {}", panes.display());

    sub.handle.abort();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(&marker);

    assert!(
        still_pending,
        "the approval must have survived every probe, or the probes measured a session \
         that had already answered it"
    );
    assert!(
        started_during_pending.is_empty(),
        "a thread started while an approval was pending: {started_during_pending:?}"
    );
    assert!(
        switched,
        "THE CONTROL FAILED: the same `/new`, typed into the same pane with the session \
         idle, started no thread — so this probe cannot tell an unproducible switch from \
         a detector that sees nothing"
    );
}

/// **MEASUREMENT: is `/new` offered at all while a turn is running?**
///
/// This is the leg that decides the whole question of a switch crossing an approval,
/// and it decides it without reference to the approval prompt: **an approval exists
/// only inside a turn**. It is raised by a running turn and every one of its
/// terminals — answered, declined, interrupted — ends with that turn ending. So a
/// client that will not switch threads during a turn cannot switch during an
/// approval either, whatever its prompt does with the keyboard.
///
/// The claim was measured once, on 0.147, and written into
/// `codex_broker::session`'s note on the busy mark. This re-measures it on 0.153.2,
/// on its own session, because the reading is destructive: the pane it leaves is not
/// one the sibling probes can be run from.
///
/// **The scrollback, not the window.** A refusal is one line, and this turn streams
/// four hundred; a probe reading only the visible rows would miss the answer it came
/// for and report silence. The TUI's liveness is recorded on both sides of the
/// keystroke for the same reason — "no switch frames" from a TUI that has exited is
/// not a measurement of anything.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn measure_whether_new_is_offered_while_a_turn_runs() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("t3c");
    let mut coord = sb.spawn_coordinator(&codex);
    let broker_log = sb.run_dir.join("broker.log");

    wait_for_the_broker_and_the_tui(&sb).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let (sub, thread_a) = subscribed_tap(&sb, "SUB").await;
    println!("MEASURED thread A = {thread_a}");

    let from = read_file(&broker_log).lines().count();
    sb.send_keys(&["Count from 1 to 400, one number per line, and nothing else."]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    sb.send_keys(&["Enter"]);
    let running = wait_until(Duration::from_secs(90), || {
        sb.capture_pane().contains("esc to interrupt")
    })
    .await;
    println!("MEASURED a turn is visibly running = {running}");
    println!(
        "MEASURED the TUI is alive before the keys = {}",
        sb.tui_running()
    );
    assert!(
        running,
        "the premise: a turn has to be running for this probe to be about one. pane:\n{}",
        sb.capture_pane()
    );

    sb.send_keys(&["/new"]);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let pane_typed = sb.capture_pane_history();
    sb.send_keys(&["Enter"]);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let pane_after = sb.capture_pane_history();
    let alive_after = sb.tui_running();
    let added: Vec<String> = log_lines_after(&read_file(&broker_log), from)
        .into_iter()
        .map(str::to_string)
        .collect();
    let started = started_threads(&sub);
    println!("PANE (with scrollback) AFTER TYPING `/new` MID-TURN:\n{pane_typed}");
    println!("PANE (with scrollback) AFTER SUBMITTING IT:\n{pane_after}");
    println!("MEASURED the TUI is alive after the keys = {alive_after}");
    println!("BROKER LOG LINES ADDED ACROSS THE PROBE:\n{added:#?}");
    println!("MEASURED threads started = {started:?}");
    let switch_frames: Vec<&String> = added
        .iter()
        .filter(|line| line.contains("thread/unsubscribe") || line.contains("ownership request"))
        .collect();
    println!("MEASURED switch frames in the broker log = {switch_frames:?}");

    let panes = match std::env::var("CC_CODEX_3C_MIDTURN") {
        Ok(path) => PathBuf::from(path),
        Err(_) => sb.run_dir.join("midturn-panes.txt"),
    };
    std::fs::write(
        &panes,
        format!(
            "--- pane+scrollback: `/new` typed mid-turn, not submitted ---\n{pane_typed}\n\
             --- pane+scrollback: after submitting it ---\n{pane_after}\n\
             --- TUI alive after = {alive_after} ---\n\
             --- broker.log lines added across the probe ---\n{added:#?}\n\
             --- threads started ---\n{started:?}\n"
        ),
    )
    .expect("write the mid-turn record");
    println!("PANES WRITTEN: {}", panes.display());

    sub.handle.abort();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();

    assert!(
        started.is_empty(),
        "a thread started from a `/new` typed while a turn was running: {started:?}"
    );
}

/// **A registration handover on the real wire: the replacement waits for the answer
/// the outgoing link has already put on the socket.**
///
/// The scripted gates in `state.rs` stage this interleaving deterministically; what
/// they cannot show is that the answer they are holding is a real response, claimed
/// by the real link and written on a real broker leg to a real app-server. This runs
/// it there: a genuine `commandExecution` approval, answered from the daemon, with
/// the broker's reply direction held so the response really leaves and really
/// actuates while its disposition can never come back — and a second production
/// registration for the same session arriving in exactly that window.
///
/// **The assertion is an ORDER, not a clock.** Whatever the budgets are on any
/// machine, the replacement must not finish while the answer it is replacing is
/// still outstanding: the loop watches both tasks and fails only if the handover
/// wins. A timing assertion would be measuring this laptop; this measures the rule.
///
/// The answer's own ending is deliberately not pinned here — the reply direction is
/// held, so it ends the way every unattributable write ends, and `Unknown` is 3b's
/// gate rather than this one's. What is pinned is that there is exactly ONE of it: a
/// handover that overtook the answer would be a second writer for a request the
/// app-server accepts one answer to.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn a_replacement_registration_waits_for_an_answer_already_on_the_wire() {
    let Some(codex) = live_gate() else { return };
    // Capture the daemon's own log, so the replacement link's attach fact can be read
    // back the way the attach gate reads it. Discarded and re-armed here, then drained
    // to a checkpoint just before the gate releases the replacement onto the wire.
    crate::log::capture::install();
    let sb = LiveSandbox::new("hand3c");
    let mut coord = sb.spawn_coordinator(&codex);
    // In the sandbox's own tree, so it goes with the run however the gate ends.
    let marker = sb
        .base
        .join(format!("cc-3c-handover.{}.txt", nanos()))
        .to_string_lossy()
        .into_owned();

    wait_for_the_broker_and_the_tui(&sb).await;
    let session = SessionKey::new(protocol::uid::new().unwrap(), "cc-live");
    let (daemon, db, _push) = live_daemon(&session);
    let uid = session.uid.clone();
    let gate = GatedCcdLeg::in_front_of(&sb).await;
    let registration = register_the_run_on(&daemon, &session, gate.path()).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let cards = || {
        daemon
            .store
            .codex_pending_approvals(&uid)
            .expect("read the run's open cards")
    };
    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(240),
            || !cards().is_empty(),
        )
        .await,
        "the observer never raised a card for the approval. pane:\n{}",
        sb.capture_pane()
    );
    let held = cards().remove(0);
    let card: protocol::ws::ApprovalCard =
        serde_json::from_str(&held.card).expect("the stored card decodes");

    // The answer is claimed, written, actuating — and its disposition can never
    // arrive, so it is outstanding for as long as the link's own budget allows.
    let answering = stage_an_unanswered_write(&daemon, &gate, &uid, &card, "accept").await;

    // The handover, through the production acceptance, onto the same gated leg.
    let handover = {
        let daemon = Arc::clone(&daemon);
        let session = session.clone();
        let path = gate.path().to_path_buf();
        tokio::spawn(async move { register_the_run_on(&daemon, &session, &path).await })
    };

    // **The gate.** Poll both, and record which finished first.
    let mut overtaken = false;
    for _ in 0..1500 {
        if answering.is_finished() {
            break;
        }
        if handover.is_finished() {
            overtaken = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    println!("MEASURED the handover overtook the outstanding answer = {overtaken}");

    let result = tokio::time::timeout(Duration::from_secs(60), answering)
        .await
        .expect("the answer must reach a terminal")
        .expect("the answering task");
    println!("MEASURED the outstanding answer ended as: {result:?}");
    // Discard the ORIGINAL link's attach history: the warm-up subscribed it long ago,
    // so any subscribe line after this drain is the REPLACEMENT's — which the gate
    // below has held off the wire until now.
    let _ = crate::log::capture::drain();
    gate.release();
    // `register_the_run_on` asserts its own acceptance, so a refused handover
    // surfaces here as a failed join rather than as a value to branch on — which is
    // the reading this gate wants: the session must still move once the answer has
    // ended, and a registration that refused is a session left unregistered.
    let replacement = tokio::time::timeout(Duration::from_secs(60), handover)
        .await
        .expect("the replacement registration must complete once the answer has");
    println!(
        "MEASURED the replacement registration was accepted = {}",
        replacement.is_ok()
    );

    let actuated = wait_until(Duration::from_secs(60), || {
        std::path::Path::new(&marker).exists()
    })
    .await;
    let settled = wait_until(Duration::from_secs(60), || {
        !resolutions(&daemon, &uid).is_empty()
    })
    .await;
    let filed = resolutions(&daemon, &uid);
    let ledger = answer_ledger(&db, &uid);
    println!("MEASURED actuated = {actuated}, settled = {settled}");
    println!("MEASURED resolutions = {filed:?}");
    println!("MEASURED ledger = {ledger:?}");
    let broker_log = read_file(&sb.run_dir.join("broker.log"));
    let dispositions = ccd_dispositions(&broker_log);
    println!("MEASURED dispositions = {dispositions:?}");
    println!("BROKER LOG:\n{broker_log}");

    // **A healthy replacement link, bound and announced on the session.** The handover
    // installed a fresh link; once the gate lets it onto the wire it must dial, bind to
    // the session's thread and announce on its connection — the proof the session moved
    // to a live observer rather than being left registered and unobserved.
    //
    // It is deliberately NOT asserted to be *subscribed*: the answer this gate leaves
    // outstanding sits in the thread's newest turn, and a link that resumes onto a thread
    // whose latest turn is an unresolved approval STOP-AND-AMENDs rather than read a state
    // it was never measured against — which is the link being careful, not the handover
    // being unhealthy. A subscribe waits on the approval resolving; a bound, announced
    // replacement is the health the handover itself owes.
    let mut repl_log: Vec<String> = Vec::new();
    let replacement_bound = wait_until(Duration::from_secs(90), || {
        repl_log.extend(crate::log::capture::drain());
        repl_log
            .iter()
            .any(|l| l.contains("announced on this connection"))
    })
    .await;
    println!("MEASURED replacement link bound and announced = {replacement_bound}");

    daemon.unregister_supervisor(&registration).await;
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    crate::log::capture::uninstall();

    assert!(
        !overtaken,
        "the replacement registration completed while an answer written by the link \
         it replaces was still outstanding — the window this gate exists to close"
    );
    assert!(
        replacement.is_ok(),
        "and the handover must still happen once the answer has ended; a refused \
         registration leaves the session with no supervisor at all"
    );
    assert_eq!(
        ledger.len(),
        1,
        "one answer, one ledger row — a handover that overtook it would be a second \
         writer for a request the app-server accepts one answer to: {ledger:?}"
    );
    assert_eq!(
        filed.len(),
        1,
        "and one terminal for the card it was about: {filed:?}"
    );
    assert!(
        actuated,
        "the answer the link wrote reached Codex and ran the command — its write leaves \
         the daemon even while its disposition is held, which is the whole reason the \
         answer is outstanding rather than never sent"
    );
    assert_eq!(
        dispositions.len(),
        1,
        "one answer, one broker disposition — a second would mean the response was \
         accounted for twice: {dispositions:?}"
    );
    assert!(
        replacement_bound,
        "the replacement link must dial, bind and announce once the gate releases it; a \
         handover that installs a link which never reaches the session leaves it \
         registered and observed by nothing"
    );
    println!(
        "GATE PASS — a replacement registration waited for an answer already on the wire, \
         the answer actuated under exactly one disposition, and the session moved to a \
         healthy bound-and-announced replacement only after it had ended"
    );
}

// ============================================================================
// The four switch positions that establish the switch half is unproducible.
//
// An earlier "switch is unproducible" verdict rested on probes that never
// reached the switch decision: `/new` typed with the prompt up consumed a key
// as the prompt's own decision; the second-TUI leg sent `{"cwd":"/tmp"}` and
// was refused by fingerprint before any busy check; the mid-turn probe carried
// no approval. The keyboard-interrupt window that A25/A26 say REVERSES the
// terminal order (`turn/completed{interrupted}` PRECEDES `serverRequest/
// resolved` on a KEYBOARD interrupt) was never probed at all.
//
// These four measure each position as a raw ordered single-session capture, and
// each asks the two decisive questions outcome (A) is defined by:
//   (i)  is a NEW thread admitted (thread/started for B, id != A) BEFORE A's
//        serverRequest/resolved lands? and
//   (ii) once B is admitted / the window has opened, is A's approval capability
//        still LATE-ANSWERABLE — does a ccd-leg answer to A's wire id still
//        actuate the command?
// A yes to either is outcome (A). No to both across every probe is outcome (B).
// ============================================================================

/// One decisive frame on the ccd tap, in arrival order.
/// `(arrival_index, label, thread_id, request_id, app_server_stamp_ms)`.
#[allow(clippy::type_complexity)]
fn decisive_timeline(tap: &WireTap) -> Vec<(usize, String, String, Option<i64>, Option<i64>)> {
    tap.seen()
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            let method = v["method"].as_str()?;
            let is_approval = method.ends_with("/requestApproval");
            let interesting = is_approval
                || matches!(
                    method,
                    "serverRequest/resolved"
                        | "thread/started"
                        | "turn/completed"
                        | "item/completed"
                );
            if !interesting {
                return None;
            }
            let thread_id = v["params"]["threadId"]
                .as_str()
                .or_else(|| v["params"]["thread"]["id"].as_str())
                .unwrap_or("")
                .to_string();
            let request_id = v["params"]["requestId"]
                .as_i64()
                .or_else(|| v["id"].as_i64());
            let stamp = v["emittedAtMs"]
                .as_i64()
                .or_else(|| v["params"]["completedAtMs"].as_i64())
                .or_else(|| v["params"]["startedAtMs"].as_i64());
            let label = match method {
                "turn/completed" => {
                    let st = v["params"]["turn"]["status"].as_str().unwrap_or("?");
                    format!("turn/completed[{st}]")
                }
                "item/completed" => {
                    let st = v["params"]["item"]["status"].as_str().unwrap_or("?");
                    format!("item/completed[{st}]")
                }
                other => other.to_string(),
            };
            Some((i, label, thread_id, request_id, stamp))
        })
        .collect()
}

/// Print the timeline and return the ordering verdict:
/// `(b_started_idx, a_resolved_idx, switch_admitted_before_A_resolved)`.
fn ordering_verdict(
    tap: &WireTap,
    thread_a: &str,
    label: &str,
) -> (Option<usize>, Option<usize>, bool) {
    let tl = decisive_timeline(tap);
    println!(
        "--- DECISIVE TIMELINE [{label}] (arrival_index | frame | thread | req | stampMs) ---"
    );
    for (i, m, t, r, s) in &tl {
        let tshort = if t.len() > 8 { &t[..8] } else { t.as_str() };
        println!("  #{i:<3} {m:<26} thread={tshort:<8} req={r:?} stamp={s:?}");
    }
    let b_started_idx = tl
        .iter()
        .find(|(_, m, t, _, _)| m == "thread/started" && !t.is_empty() && t != thread_a)
        .map(|(i, _, _, _, _)| *i);
    let a_resolved_idx = tl
        .iter()
        .find(|(_, m, t, _, _)| m == "serverRequest/resolved" && (t == thread_a || t.is_empty()))
        .map(|(i, _, _, _, _)| *i);
    let switch_before_resolve = match (b_started_idx, a_resolved_idx) {
        (Some(b), Some(a)) => b < a,
        (Some(_), None) => true,
        _ => false,
    };
    println!(
        "MEASURED [{label}] b_started_idx={b_started_idx:?} a_resolved_idx={a_resolved_idx:?} \
         switch_admitted_before_A_resolved={switch_before_resolve}"
    );
    (b_started_idx, a_resolved_idx, switch_before_resolve)
}

/// Raise the command approval `touch {marker}` on the subscribed leg and return
/// `(sub, thread_a, wire_id)` with the prompt confirmed up and the marker absent.
async fn raise_command_approval(sb: &LiveSandbox, marker: &str) -> (WireTap, String, i64) {
    let (sub, thread_a) = subscribed_tap(sb, "SUB").await;
    println!("MEASURED thread A = {thread_a}");
    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(240),
            || sub
                .methods()
                .iter()
                .any(|m| m.ends_with("/requestApproval")),
        )
        .await,
        "no approval reached the subscribed ccd leg. pane:\n{}",
        sb.capture_pane()
    );
    let request = sub
        .first("item/commandExecution/requestApproval")
        .expect("the command-execution approval frame");
    let wire_id = request["id"]
        .as_i64()
        .expect("a server-request carries a numeric id");
    // The approval frame reaches the ccd tap a beat before the TUI paints the
    // overlay; wait for the pane to actually show the prompt so the premise is
    // a rendered prompt, not a race.
    let painted = wait_until(Duration::from_secs(30), || {
        sb.capture_pane()
            .contains("Would you like to run the following command?")
    })
    .await;
    let pane = sb.capture_pane();
    assert!(
        painted,
        "the premise: the TUI is showing the prompt this probe is about. pane:\n{pane}"
    );
    assert!(
        !std::path::Path::new(marker).exists(),
        "the command ran before anybody answered; nothing below is about a PENDING approval"
    );
    println!("MEASURED wire id = {wire_id}");
    (sub, thread_a, wire_id)
}

/// After the producer step, ask A one more time on the ccd leg and see whether
/// the answer still actuates — the C1/A -> C2/B -> delayed-A late-answerability
/// test. Returns `(actuated, resolved_seen_before, answer_reply)`.
async fn late_answer_a(
    _sb: &LiveSandbox,
    sub: &mut WireTap,
    wire_id: i64,
    marker: &str,
) -> (bool, bool, Option<Value>) {
    let already = std::path::Path::new(marker).exists();
    let resolved_seen_before = sub.methods().iter().any(|m| m == "serverRequest/resolved");
    println!(
        "LATE-A: marker already present before the late answer = {already}, \
         serverRequest/resolved already seen = {resolved_seen_before}"
    );
    sub.send(serde_json::json!({"id": wire_id, "result": {"decision": "accept"}}))
        .await;
    let actuated = wait_until(Duration::from_secs(45), || {
        std::path::Path::new(marker).exists()
    })
    .await;
    let _ = sub.barrier(Duration::from_secs(20)).await;
    let reply = sub
        .seen()
        .into_iter()
        .find(|v| v.get("id").and_then(Value::as_i64) == Some(wire_id) && v["method"].is_null());
    println!(
        "LATE-A: after re-answering A wire_id={wire_id}: actuated={actuated} \
         (already_present={already}) broker_reply={reply:?}"
    );
    // The late answer counts as producing a live A-capability only if the marker
    // was NOT already there and it actuates now.
    (actuated && !already, resolved_seen_before, reply)
}

fn write_probe_record(sb: &LiveSandbox, sub: &WireTap, stem: &str, panes: &str) {
    let dir = std::env::var("CC_CODEX_3C_EVID")
        .map(PathBuf::from)
        .unwrap_or_else(|_| sb.run_dir.clone());
    let _ = std::fs::create_dir_all(&dir);
    let jsonl = dir.join(format!("{stem}.jsonl"));
    let mut lines = String::new();
    for frame in sub.seen() {
        lines.push_str(
            &serde_json::json!({"conn": "ccd-subscribed", "dir": "s2c", "frame": frame})
                .to_string(),
        );
        lines.push('\n');
    }
    std::fs::write(&jsonl, &lines).expect("write the probe capture");
    let panes_path = dir.join(format!("{stem}.panes.txt"));
    std::fs::write(&panes_path, panes).expect("write the probe panes");
    println!(
        "PROBE RECORD WRITTEN: {} + {}",
        jsonl.display(),
        panes_path.display()
    );
}

fn teardown(sub: WireTap, mut coord: Child, marker: &str) {
    sub.handle.abort();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &coord.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = coord.wait();
    let _ = std::fs::remove_file(marker);
}

/// **P1 — THE DECISIVE PROBE: a keyboard interrupt, then `/new`, with a command
/// approval pending.**
///
/// A25's `interrupt.jsonl` recorded, on a Ctrl-C of a running turn with NO approval
/// prompt up, `turn/completed{interrupted}` before `serverRequest/resolved`. This
/// probes a DIFFERENT scenario — Ctrl-C WHILE the approval prompt is showing — where
/// the prompt consumes the interrupt as its own decline, so the item IS declined and
/// `resolved` leads the turn terminal (both orderings are real for their own
/// scenario; this is not a contradiction of A25). The concern either way is the busy
/// mark: the broker clears it on ANY `turn/completed`, so if the terminal preceded the
/// resolution a switch could be admitted while A was unresolved. This types Ctrl-C,
/// then `/new`+Enter, and watches whether a new thread B is admitted before A resolves,
/// and whether A stays late-answerable afterward.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn p1_ctrl_c_then_new_with_an_approval_pending() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("p1cc");
    let coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3c-p1.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);
    let broker_log = sb.run_dir.join("broker.log");

    wait_for_the_broker_and_the_tui(&sb).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let (mut sub, thread_a, wire_id) = raise_command_approval(&sb, &marker).await;
    let pane_pending = sb.capture_pane();
    println!("PANE WITH THE APPROVAL PENDING:\n{pane_pending}");
    let from = read_file(&broker_log).lines().count();

    // ---- the interrupt window, then /new ----------------------------------
    sb.send_keys(&["C-c"]);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let pane_after_ctrl_c = sb.capture_pane();
    println!("PANE AFTER Ctrl-C:\n{pane_after_ctrl_c}");
    sb.send_keys(&["/new"]);
    tokio::time::sleep(Duration::from_millis(700)).await;
    sb.send_keys(&["Enter"]);

    // Wait until BOTH the interrupt's resolution and any new thread have had a
    // chance to land, so the order between them is the measurement.
    let _ = wait_until(Duration::from_secs(60), || {
        let m = sub.methods();
        m.iter().any(|x| x == "serverRequest/resolved")
            && started_threads(&sub).iter().any(|t| t != &thread_a)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _ = sub.barrier(Duration::from_secs(30)).await;

    let pane_after_new = sb.capture_pane();
    println!("PANE AFTER Ctrl-C THEN /new:\n{pane_after_new}");
    let (b_idx, a_idx, switch_before_resolve) = ordering_verdict(&sub, &thread_a, "P1");
    let started_after: Vec<String> = started_threads(&sub)
        .into_iter()
        .filter(|t| t != &thread_a)
        .collect();
    println!("MEASURED new threads started after Ctrl-C+/new = {started_after:?}");

    // ---- (ii) is A still late-answerable after the window? ----------------
    let (late_live, resolved_before, _reply) = late_answer_a(&sb, &mut sub, wire_id, &marker).await;

    let broker_lines: Vec<String> = log_lines_after(&read_file(&broker_log), from)
        .into_iter()
        .map(str::to_string)
        .collect();
    println!("BROKER LOG LINES ADDED ACROSS P1:\n{broker_lines:#?}");

    write_probe_record(
        &sb,
        &sub,
        "p1-ctrl-c-then-new",
        &format!(
            "--- pane: approval pending ---\n{pane_pending}\n\
             --- pane: after Ctrl-C ---\n{pane_after_ctrl_c}\n\
             --- pane: after Ctrl-C then /new ---\n{pane_after_new}\n\
             --- broker.log added ---\n{broker_lines:#?}\n\
             --- verdict: b_idx={b_idx:?} a_idx={a_idx:?} switch_before_resolve={switch_before_resolve} late_live={late_live} resolved_before_late={resolved_before} ---\n"
        ),
    );

    teardown(sub, coord, &marker);
    println!(
        "P1 OUTCOME SIGNALS: switch_admitted_before_A_resolved={switch_before_resolve}, \
         A_late_answerable={late_live}, new_threads={started_after:?}"
    );
}

/// **P1b — the same interrupt window, but the switch is a real `/resume` and a
/// real `/fork` picker rather than `/new`.** `/resume` and `/fork` are the other
/// two producers the operator can reach; approval-rebind shows a ccd resume
/// accepted mid-approval, so this asks whether the TUI's own resume/fork move
/// the head or leave A late-answerable in the post-interrupt window.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn p1b_ctrl_c_then_resume_and_fork_with_an_approval_pending() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("p1bcc");
    let coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3c-p1b.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);
    let broker_log = sb.run_dir.join("broker.log");

    wait_for_the_broker_and_the_tui(&sb).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let (sub, thread_a, _wire_id) = raise_command_approval(&sb, &marker).await;
    let from = read_file(&broker_log).lines().count();

    // Measurement-only and teardown-tolerant: a real /resume in this parked test
    // coordinator can re-exec the TUI out from under the pane, so nothing here
    // asserts — the tap's frames are the record (read-only, captured before any
    // teardown) and every keystroke is best-effort.
    let _ = try_send_keys(&sb, &["C-c"]);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let pane_after_ctrl_c = sb.capture_pane();

    let resume_sent = try_send_keys(&sb, &["/resume"]);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let pane_resume = sb.capture_pane();
    println!("PANE AFTER Ctrl-C THEN /resume (sent={resume_sent}):\n{pane_resume}");
    let enter_sent = try_send_keys(&sb, &["Enter"]);
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let pane_resume_pick = sb.capture_pane();
    println!("PANE AFTER PICKING A RESUME TARGET (enter_sent={enter_sent}):\n{pane_resume_pick}");
    let tui_alive_after_resume = sb.tui_running();
    println!("MEASURED TUI alive after /resume = {tui_alive_after_resume}");

    let mut pane_fork = String::from("<skipped: session not alive after /resume>");
    if tui_alive_after_resume {
        let _ = try_send_keys(&sb, &["/fork"]);
        tokio::time::sleep(Duration::from_millis(1000)).await;
        pane_fork = sb.capture_pane();
        println!("PANE AFTER /fork:\n{pane_fork}");
        let _ = try_send_keys(&sb, &["Enter"]);
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    let (b_idx, a_idx, switch_before_resolve) = ordering_verdict(&sub, &thread_a, "P1b");
    let started_after: Vec<String> = started_threads(&sub)
        .into_iter()
        .filter(|t| t != &thread_a)
        .collect();
    println!("MEASURED new threads started after Ctrl-C+/resume(+/fork) = {started_after:?}");
    let resolved_seen = sub.methods().iter().any(|m| m == "serverRequest/resolved");

    let broker_lines: Vec<String> = log_lines_after(&read_file(&broker_log), from)
        .into_iter()
        .map(str::to_string)
        .collect();
    println!("BROKER LOG LINES ADDED ACROSS P1b:\n{broker_lines:#?}");

    write_probe_record(
        &sb,
        &sub,
        "p1b-ctrl-c-then-resume-fork",
        &format!(
            "--- pane: after Ctrl-C ---\n{pane_after_ctrl_c}\n\
             --- pane: after /resume ---\n{pane_resume}\n\
             --- pane: after picking resume ---\n{pane_resume_pick}\n\
             --- TUI alive after /resume = {tui_alive_after_resume} ---\n\
             --- pane: after /fork ---\n{pane_fork}\n\
             --- broker.log added ---\n{broker_lines:#?}\n\
             --- verdict: b_idx={b_idx:?} a_idx={a_idx:?} switch_before_resolve={switch_before_resolve} resolved_seen={resolved_seen} ---\n"
        ),
    );

    teardown(sub, coord, &marker);
    println!(
        "P1b OUTCOME SIGNALS: switch_admitted_before_A_resolved={switch_before_resolve}, \
         new_threads={started_after:?}, tui_alive_after_resume={tui_alive_after_resume}, \
         resolved_seen={resolved_seen}"
    );
}

/// Best-effort `tmux send-keys` that reports success instead of panicking, for
/// probes that deliberately drive a pane which may tear itself down mid-sequence.
fn try_send_keys(sb: &LiveSandbox, keys: &[&str]) -> bool {
    Command::new(&sb.tmux)
        .args([
            "-S",
            sb.sock.to_str().unwrap(),
            "-f",
            "/dev/null",
            "send-keys",
            "-t",
            "cc-live",
        ])
        .args(keys)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// **P2 — the second-TUI-leg switch done right: a fully fingerprinted
/// `thread/start`.** The prior probe sent `{"cwd":"/tmp"}` and was refused by
/// fingerprint before reaching any busy/switch decision. A real `/new`'s
/// `thread/start` carries the launch fingerprint's own dimensions and passes
/// `assert_fingerprint`. This replays that full shape from a second connection
/// on the TUI leg, with A's approval pending, and records the broker's verbatim
/// answer — a fingerprint conflict (did not reach the decision), a session-policy
/// refusal (reached the decision and was refused), or a `thread/started` (B
/// admitted). It sends a small matrix so the answer cannot be blamed on one
/// guessed field.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn p2_second_tui_leg_fully_fingerprinted_thread_start() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("p2ft");
    let coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3c-p2.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);

    wait_for_the_broker_and_the_tui(&sb).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let (mut sub, thread_a, wire_id) = raise_command_approval(&sb, &marker).await;

    // The launch fingerprint of THIS sandbox: on-request / user / read-only, and
    // its workspace is the coordinator's --cwd /tmp. The real 0.153 TUI shape
    // sends cwd:null with runtimeWorkspaceRoots:[workspace]; model is not a
    // fingerprint dimension. Send several well-formed variants and record each
    // verbatim answer.
    let base = |cwd: Value, roots: Value| {
        serde_json::json!({
            "approvalPolicy": "on-request",
            "approvalsReviewer": "user",
            "baseInstructions": null,
            "config": {"personality": "pragmatic", "web_search": "cached"},
            "cwd": cwd,
            "developerInstructions": null,
            "dynamicTools": null,
            "environments": null,
            "ephemeral": false,
            "historyMode": "paginated",
            "mockExperimentalField": null,
            "model": "gpt-5.6-luna",
            "modelProvider": null,
            "multiAgentMode": null,
            "permissions": null,
            "personality": null,
            "runtimeWorkspaceRoots": roots,
            "sandbox": "read-only",
            "selectedCapabilityRoots": null,
            "serviceName": null,
            "sessionStartSource": null,
            "threadSource": "user"
        })
    };
    let attempts: Vec<(&str, Value)> = vec![
        ("baseline_malformed", serde_json::json!({"cwd": "/tmp"})),
        (
            "fingerprinted_cwd_null_roots_tmp",
            base(Value::Null, serde_json::json!(["/tmp"])),
        ),
        (
            "fingerprinted_cwd_tmp_roots_tmp",
            base(serde_json::json!("/tmp"), serde_json::json!(["/tmp"])),
        ),
        (
            "fingerprinted_cwd_null_roots_null",
            base(Value::Null, Value::Null),
        ),
    ];

    let mut answers: Vec<(String, Value)> = Vec::new();
    let mut started_any = false;
    for (name, params) in attempts {
        let mut raw = RawCcd::connect(&sb.run_dir.join("tui.sock")).await;
        let init = raw.initialize().await;
        assert!(
            init["result"].is_object(),
            "second TUI init must be answered: {init}"
        );
        raw.notify("initialized", serde_json::json!({})).await;
        let started = raw
            .request("thread/start", params.clone(), Duration::from_secs(30))
            .await;
        println!(
            "MEASURED second-TUI thread/start [{name}] params={params}\n  -> answer={started}"
        );
        if started.get("result").is_some() {
            started_any = true;
        }
        answers.push((name.to_string(), started));
        // drop raw -> the connection closes
    }

    let still_pending = sb
        .capture_pane()
        .contains("Would you like to run the following command?");
    println!("MEASURED approval still pending after every second-TUI attempt = {still_pending}");
    let started_during: Vec<String> = started_threads(&sub)
        .into_iter()
        .filter(|t| t != &thread_a)
        .collect();
    println!("MEASURED new threads started on the tap during P2 = {started_during:?}");

    let (late_live, resolved_before, _reply) = late_answer_a(&sb, &mut sub, wire_id, &marker).await;

    let answers_dump = answers
        .iter()
        .map(|(n, a)| format!("[{n}] {a}"))
        .collect::<Vec<_>>()
        .join("\n");
    write_probe_record(
        &sb,
        &sub,
        "p2-second-tui-fingerprinted",
        &format!(
            "--- second-TUI thread/start answers ---\n{answers_dump}\n\
             --- still_pending_through_P2={still_pending} ---\n\
             --- new threads started during P2 ---\n{started_during:?}\n\
             --- verdict: started_any={started_any} late_live={late_live} resolved_before_late={resolved_before} ---\n"
        ),
    );

    teardown(sub, coord, &marker);
    println!(
        "P2 OUTCOME SIGNALS: any_thread_start_result={started_any}, \
         new_threads_during_pending={started_during:?}, A_late_answerable={late_live}, \
         still_pending_through_probe={still_pending}"
    );
}

/// **P3 — the ccd leg's one permitted method: `thread/resume` naming a thread
/// OTHER than the card's, with A's approval pending.** approval-rebind shows a
/// ccd resume accepted mid-approval onto the SAME thread; this asks whether a
/// resume aimed elsewhere moves the head or leaves A late-answerable. A fresh
/// session has only thread A, so this resumes (a) A again (the known rebind) and
/// (b) a synthetic foreign id, records both answers, and then late-answer-tests A.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn p3_ccd_resume_to_another_thread_with_an_approval_pending() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("p3rz");
    let coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3c-p3.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);
    let broker_log = sb.run_dir.join("broker.log");

    wait_for_the_broker_and_the_tui(&sb).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let (mut sub, thread_a, wire_id) = raise_command_approval(&sb, &marker).await;
    let from = read_file(&broker_log).lines().count();

    let foreign = "01a00000-0000-7000-8000-000000000000";
    let rz_a_id = 8200;
    sub.send(serde_json::json!({
        "id": rz_a_id, "method": "thread/resume", "params": {"threadId": thread_a}
    }))
    .await;
    let rz_foreign_id = 8201;
    sub.send(serde_json::json!({
        "id": rz_foreign_id, "method": "thread/resume", "params": {"threadId": foreign}
    }))
    .await;
    let _ = sub.barrier(Duration::from_secs(30)).await;
    let seen = sub.seen();
    let answer_to = |id: i64| {
        seen.iter()
            .find(|v| v.get("id").and_then(Value::as_i64) == Some(id) && v["method"].is_null())
            .cloned()
    };
    let rz_a = answer_to(rz_a_id);
    let rz_foreign = answer_to(rz_foreign_id);
    println!("MEASURED ccd resume->A answer: {rz_a:?}");
    println!("MEASURED ccd resume->foreign answer: {rz_foreign:?}");

    let (b_idx, a_idx, switch_before_resolve) = ordering_verdict(&sub, &thread_a, "P3");
    let started_during: Vec<String> = started_threads(&sub)
        .into_iter()
        .filter(|t| t != &thread_a)
        .collect();
    println!("MEASURED new threads started during P3 = {started_during:?}");
    let still_pending = sb
        .capture_pane()
        .contains("Would you like to run the following command?");
    println!("MEASURED approval still pending through P3 = {still_pending}");

    let (late_live, resolved_before, _reply) = late_answer_a(&sb, &mut sub, wire_id, &marker).await;
    let broker_lines: Vec<String> = log_lines_after(&read_file(&broker_log), from)
        .into_iter()
        .map(str::to_string)
        .collect();
    println!("BROKER LOG LINES ADDED ACROSS P3:\n{broker_lines:#?}");

    write_probe_record(
        &sb,
        &sub,
        "p3-ccd-resume-other",
        &format!(
            "--- ccd resume->A ---\n{rz_a:?}\n\
             --- ccd resume->foreign ---\n{rz_foreign:?}\n\
             --- still_pending={still_pending} new_threads={started_during:?} ---\n\
             --- broker.log added ---\n{broker_lines:#?}\n\
             --- verdict: b_idx={b_idx:?} a_idx={a_idx:?} switch_before_resolve={switch_before_resolve} late_live={late_live} resolved_before_late={resolved_before} ---\n"
        ),
    );

    teardown(sub, coord, &marker);
    println!(
        "P3 OUTCOME SIGNALS: switch_admitted_before_A_resolved={switch_before_resolve}, \
         new_threads_during_pending={started_during:?}, A_late_answerable={late_live}, \
         still_pending_through_probe={still_pending}"
    );
}

/// **P4 — the Esc position: the approval is dismissed by Esc, then `/new`.** Esc
/// cancels the pending request (its own serverRequest/resolved) and interrupts
/// the turn; `/new` immediately after asks whether B is admitted before A's
/// resolution lands, and whether A stays late-answerable across the dismissal.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn p4_esc_then_new_with_an_approval_pending() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("p4esc");
    let coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3c-p4.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);
    let broker_log = sb.run_dir.join("broker.log");

    wait_for_the_broker_and_the_tui(&sb).await;
    wait_for_a_composer(&sb).await;
    run_the_warm_up_turn(&sb).await;

    let (mut sub, thread_a, wire_id) = raise_command_approval(&sb, &marker).await;
    let from = read_file(&broker_log).lines().count();

    sb.send_keys(&["Escape"]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let pane_after_esc = sb.capture_pane();
    println!("PANE AFTER Esc:\n{pane_after_esc}");
    sb.send_keys(&["/new"]);
    tokio::time::sleep(Duration::from_millis(700)).await;
    sb.send_keys(&["Enter"]);

    let _ = wait_until(Duration::from_secs(60), || {
        let m = sub.methods();
        m.iter().any(|x| x == "serverRequest/resolved")
            && started_threads(&sub).iter().any(|t| t != &thread_a)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _ = sub.barrier(Duration::from_secs(30)).await;

    let pane_after_new = sb.capture_pane();
    println!("PANE AFTER Esc THEN /new:\n{pane_after_new}");
    let (b_idx, a_idx, switch_before_resolve) = ordering_verdict(&sub, &thread_a, "P4");
    let started_after: Vec<String> = started_threads(&sub)
        .into_iter()
        .filter(|t| t != &thread_a)
        .collect();
    println!("MEASURED new threads started after Esc+/new = {started_after:?}");

    let (late_live, resolved_before, _reply) = late_answer_a(&sb, &mut sub, wire_id, &marker).await;
    let broker_lines: Vec<String> = log_lines_after(&read_file(&broker_log), from)
        .into_iter()
        .map(str::to_string)
        .collect();
    println!("BROKER LOG LINES ADDED ACROSS P4:\n{broker_lines:#?}");

    write_probe_record(
        &sb,
        &sub,
        "p4-esc-then-new",
        &format!(
            "--- pane: after Esc ---\n{pane_after_esc}\n\
             --- pane: after Esc then /new ---\n{pane_after_new}\n\
             --- broker.log added ---\n{broker_lines:#?}\n\
             --- verdict: b_idx={b_idx:?} a_idx={a_idx:?} switch_before_resolve={switch_before_resolve} late_live={late_live} resolved_before_late={resolved_before} ---\n"
        ),
    );

    teardown(sub, coord, &marker);
    println!(
        "P4 OUTCOME SIGNALS: switch_admitted_before_A_resolved={switch_before_resolve}, \
         A_late_answerable={late_live}, new_threads={started_after:?}"
    );
}

/// **P5 — the ccd leg resumes a DIFFERENT session-bound thread while an approval is
/// pending on the head, and the head does not move.**
///
/// P3 could only resume the card's OWN thread (a rebind, head unmoved) and a FOREIGN
/// id (refused "not bound to this session"), because a fresh session has exactly one
/// bound thread. This builds the missing state: a first thread T1 is bound and given a
/// rollout, `/new` retires it while binding a second thread T2 as the head, and the
/// command approval is raised on T2. Then the ccd leg sends `thread/resume` naming
/// T1 — a thread that IS session-bound (retired-but-bound, so its resume binding check
/// passes) but is NOT the head the approval sits on.
///
/// The measured verdict is the module's two questions, and both are `no`: the resume is
/// ACCEPTED but returns T1 idle, no new thread is admitted before T2 resolves, and T2's
/// approval stays the ccd leg's to answer (a late ccd answer still actuates). A
/// `thread/resume` subscribes; only a head-moving `thread/start` moves the head, and the
/// live-turn/approval gate fences that — so a bound resume is not a switch producer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn p5_ccd_resume_to_a_second_bound_thread_with_an_approval_pending() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("p5rz");
    let coord = sb.spawn_coordinator(&codex);
    let marker = format!("/tmp/cc-3c-p5.{}.txt", nanos());
    let _ = std::fs::remove_file(&marker);
    let broker_log = sb.run_dir.join("broker.log");

    wait_for_the_broker_and_the_tui(&sb).await;
    wait_for_a_composer(&sb).await;

    // T1: first bound thread with a rollout on disk (a thread with no rollout is
    // refused by thread/resume, so a retired thread must have run a turn first).
    run_the_warm_up_turn(&sb).await;

    // Subscribe the tap. Only T1 is loaded, so it resumes onto T1 = the head.
    let (mut sub, t1) = subscribed_tap(&sb, "SUB").await;
    println!("MEASURED T1 (first bound thread) = {t1}");

    // /new retires T1 and binds a fresh head T2. /new does not re-exec the TUI, but it
    // is refused client-side ("'/new' is disabled while a task is in progress") if the
    // warm-up turn's terminal has not fully landed — the composer can paint the answer a
    // beat before the TUI leaves its task state. So wait for a genuinely idle composer
    // and retry until a distinct second thread actually appears on the tap.
    let mut got_two = false;
    for _ in 0..6 {
        let _ = wait_until(Duration::from_secs(30), || {
            let pane = sb.capture_pane();
            pane.contains("Ask Codex to do anything")
                && !pane.contains("is disabled while a task is in progress")
        })
        .await;
        sb.send_keys(&["/new"]);
        tokio::time::sleep(Duration::from_millis(800)).await;
        sb.send_keys(&["Enter"]);
        got_two = wait_until(Duration::from_secs(20), || {
            started_threads(&sub).iter().any(|t| t != &t1)
        })
        .await;
        if got_two {
            break;
        }
    }
    let mut t2 = started_threads(&sub)
        .into_iter()
        .find(|t| t != &t1)
        .unwrap_or_default();
    if t2.is_empty() {
        let ll_id = 8290;
        sub.send(serde_json::json!({
            "id": ll_id, "method": "thread/loaded/list", "params": {}
        }))
        .await;
        let _ = sub.barrier(Duration::from_secs(20)).await;
        if let Some(ans) = sub
            .seen()
            .into_iter()
            .find(|v| v.get("id").and_then(Value::as_i64) == Some(ll_id) && v["method"].is_null())
        {
            if let Some(arr) = ans["result"]["data"].as_array() {
                t2 = arr
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|id| *id != t1)
                    .unwrap_or_default()
                    .to_string();
            }
        }
    }
    println!("MEASURED T2 (second bound thread, new head) = {t2} (got_two={got_two})");
    assert!(
        !t2.is_empty() && t2 != t1,
        "the /new must bind a distinct second thread, or there is no two-bound-thread \
         state to measure. pane:\n{}",
        sb.capture_pane()
    );

    // T2 has no rollout until a turn runs on it, and thread/resume refuses a
    // rollout-less thread — so a warm-up turn on the new head first, with a sentinel
    // distinct from T1's ("amber") so a leftover line cannot pass this check for a
    // turn that never ran on T2.
    assert!(
        sb.submit_until(
            "Reply with the single word cobalt and nothing else.",
            Duration::from_secs(180),
            || sb.capture_pane().to_lowercase().contains("• cobalt"),
        )
        .await,
        "the warm-up turn on the new head T2 never completed. pane:\n{}",
        sb.capture_pane()
    );

    // Follow the switch onto the new head, the way the production ccd link does, so
    // the tap is subscribed to T2 where the approval will be raised and answered.
    let follow_id = 8291;
    sub.send(serde_json::json!({
        "id": follow_id, "method": "thread/resume", "params": {"threadId": t2}
    }))
    .await;
    let _ = sub.barrier(Duration::from_secs(30)).await;

    // Raise the command approval on T2 (the current head).
    assert!(
        sb.submit_until(
            &format!("Run the shell command `touch {marker}` now. Do not explain, just run it."),
            Duration::from_secs(240),
            || sub
                .methods()
                .iter()
                .any(|m| m.ends_with("/requestApproval")),
        )
        .await,
        "no approval reached the subscribed ccd leg. pane:\n{}",
        sb.capture_pane()
    );
    let request = sub
        .first("item/commandExecution/requestApproval")
        .expect("the command-execution approval frame");
    let wire_id = request["id"]
        .as_i64()
        .expect("a server-request carries a numeric id");
    let painted = wait_until(Duration::from_secs(30), || {
        sb.capture_pane()
            .contains("Would you like to run the following command?")
    })
    .await;
    assert!(
        painted,
        "the premise: the TUI is showing the prompt this probe is about. pane:\n{}",
        sb.capture_pane()
    );
    assert!(
        !std::path::Path::new(&marker).exists(),
        "the command ran before anybody answered; nothing below is about a PENDING approval"
    );

    let from = read_file(&broker_log).lines().count();

    // THE PRODUCER: the ccd leg resumes T1 — a session-bound (retired) thread that is
    // NOT the head the approval sits on — while T2's approval is pending.
    let rz_t1_id = 8300;
    sub.send(serde_json::json!({
        "id": rz_t1_id, "method": "thread/resume", "params": {"threadId": t1}
    }))
    .await;
    let _ = sub.barrier(Duration::from_secs(30)).await;
    let rz_t1 = sub
        .seen()
        .into_iter()
        .find(|v| v.get("id").and_then(Value::as_i64) == Some(rz_t1_id) && v["method"].is_null());
    let rz_t1_accepted = rz_t1
        .as_ref()
        .map(|v| v.get("result").is_some())
        .unwrap_or(false);
    println!("MEASURED ccd resume->T1 (retired, other bound thread) answer: {rz_t1:?}");

    let (b_idx, a_idx, switch_before_resolve) = ordering_verdict(&sub, &t2, "P5");
    let started_extra: Vec<String> = started_threads(&sub)
        .into_iter()
        .filter(|t| t != &t1 && t != &t2)
        .collect();
    let still_pending = sb
        .capture_pane()
        .contains("Would you like to run the following command?");

    // Is T2's capability still the ccd leg's? Re-answer T2's wire id and see whether it
    // still actuates the command.
    let (late_live, resolved_before, _reply) = late_answer_a(&sb, &mut sub, wire_id, &marker).await;

    let broker_lines: Vec<String> = log_lines_after(&read_file(&broker_log), from)
        .into_iter()
        .map(str::to_string)
        .collect();

    write_probe_record(
        &sb,
        &sub,
        "p5-ccd-resume-second-bound",
        &format!(
            "--- T1(retired)={t1} T2(head)={t2} wire_id={wire_id} ---\n\
             --- PRODUCER ccd resume->T1 ---\n{rz_t1:?}\n\
             --- resume->T1 accepted={rz_t1_accepted} still_pending={still_pending} \
             brand_new_threads={started_extra:?} ---\n\
             --- verdict: b_idx={b_idx:?} a_idx={a_idx:?} \
             switch_before_resolve={switch_before_resolve} \
             T2_late_answerable={late_live} resolved_before_late={resolved_before} ---\n\
             --- broker.log added ---\n{broker_lines:#?}\n"
        ),
    );

    teardown(sub, coord, &marker);

    // **The verdict, asserted.** A bound resume to another session thread is admitted
    // as a subscription/read (accepted), it moves no head (no brand-new thread, none
    // admitted before T2 resolved), and it leaves T2's approval the ccd leg's to answer
    // (the late answer still actuates). This is outcome (B): the switch is not
    // producible through a `thread/resume`.
    assert!(
        rz_t1_accepted,
        "a resume to a retired-but-bound thread passes the binding check and is accepted"
    );
    assert!(
        started_extra.is_empty(),
        "no brand-new thread is admitted by a resume: {started_extra:?}"
    );
    assert!(
        !switch_before_resolve,
        "no head-move is admitted before the approval resolves"
    );
    assert!(
        late_live,
        "the head does not move: T2's approval stays the ccd leg's to answer, and the \
         late ccd answer still actuates the command"
    );
}

// ======================================== MEASUREMENT: THE NON-PHONE FAMILIES
//
// Grounding for the observe-only work, NOT a gate. The vendored 0.153 bundle
// declares three server→client request families besides the two a phone answers:
// `item/permissions/requestApproval`, `item/tool/requestUserInput` and
// `mcpServer/elicitation/request`. What a schema declares and what a wire emits
// are different facts, and what a wire emits and what a ccd leg is HANDED are a
// third — the broker answers an unadmitted server request upstream and never
// delivers it. This reads all three off a live session rather than reasoning
// about them.

/// The three families this probe is about, as the bundle spells them.
const NON_PHONE_FAMILIES: [&str; 3] = [
    "item/permissions/requestApproval",
    "item/tool/requestUserInput",
    "mcpServer/elicitation/request",
];

/// The pane text the command-approval prompt paints, which is also the signal
/// that a probe's turn is parked on a question this probe is not about.
const COMMAND_PROMPT: &str = "Would you like to run the following command?";
/// The file-change prompt's own sentence, as 0.153.2 paints it.
const FILE_CHANGE_PROMPT: &str = "Would you like to make the following edits?";

/// Drive one prompt and wait until either a watched family reaches the tapped
/// leg or the turn ends, accepting at the keyboard any ordinary approval that
/// parks the turn on the way.
///
/// A read-only sandbox turns almost every useful producer prompt into a command
/// approval first, and a probe that let the turn sit there would measure the
/// approval it already understands instead of the family it is asking about.
/// Answering it at the keyboard is what lets the turn reach the point where a
/// permission or a question could be asked.
async fn drive_looking_for(
    sb: &LiveSandbox,
    sub: &WireTap,
    prompt: &str,
    answer: &str,
    budget: Duration,
) -> (bool, usize) {
    let watched = |sub: &WireTap| {
        sub.methods()
            .into_iter()
            .filter(|m| NON_PHONE_FAMILIES.contains(&m.as_str()))
            .count()
    };
    let before = watched(sub);
    let terminals_before = sub
        .methods()
        .iter()
        .filter(|m| *m == "turn/completed")
        .count();
    sb.send_keys(&[prompt]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    sb.send_keys(&["Enter"]);
    let deadline = std::time::Instant::now() + budget;
    let mut accepted = 0usize;
    while std::time::Instant::now() < deadline {
        if watched(sub) > before {
            return (true, accepted);
        }
        let pane = sb.capture_pane();
        if pane.contains(COMMAND_PROMPT) || pane.contains(FILE_CHANGE_PROMPT) {
            // Accept it and let the turn go on. This probe is not about this
            // question, and a parked turn asks nothing else.
            sb.send_keys(&[answer]);
            accepted += 1;
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        let terminals = sub
            .methods()
            .iter()
            .filter(|m| *m == "turn/completed")
            .count();
        if terminals > terminals_before {
            return (watched(sub) > before, accepted);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    (watched(sub) > before, accepted)
}

/// Every broker-log line that names one of the three families, whichever verdict
/// it records. The capability observer logs the method verbatim, so the log is
/// the witness for a frame the app-server EMITTED and the broker did not deliver
/// — the one thing a tap on the delivered side structurally cannot see.
fn capability_lines_naming_a_non_phone_family(log: &str) -> Vec<String> {
    log.lines()
        .filter(|line| {
            line.contains(ANSWERED_UPSTREAM)
                || NON_PHONE_FAMILIES
                    .iter()
                    .any(|method| line.contains(method))
        })
        .map(str::to_string)
        .collect()
}

/// The broker's own words for "this was emitted and not handed on".
const ANSWERED_UPSTREAM: &str = "answer upstream";

/// The feature that turns the permissions producer on, and that a CodeConnect
/// launch pins off.
///
/// Restated here rather than shared with the launcher, which is a different crate
/// this harness does not depend on. The pin itself is asserted from the running
/// app-server's own argv below, so a launcher that stopped writing it fails this
/// probe rather than quietly changing what the probe measures.
const PERMISSIONS_FEATURE: &str = "request_permissions_tool";

/// What the codex under test says about one feature in a given `CODEX_HOME`: the
/// value the config yields on its own, and the value the launcher's override
/// yields on top of it.
///
/// A zero from this probe is a fact about a LAUNCH, and a launch is a config plus
/// an argv. Reading the effective registry both ways is what turns "the feature is
/// off" from an assumption about defaults into a measurement of the override — and
/// it is also the only place the two are recorded together, which is what a reader
/// needs to tell "nothing produces this" from "this launch does not".
fn feature_registry_rows(codex: &Path, codex_home: &Path, feature: &str) -> String {
    let read = |extra: &[&str]| -> String {
        let out = Command::new(codex)
            .args(extra)
            .args(["features", "list"])
            .env("CODEX_HOME", codex_home)
            .stdin(Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .find(|line| line.starts_with(feature))
                .map(|line| line.trim_end().to_string())
                .unwrap_or_else(|| format!("<{feature} is not in this registry>")),
            Ok(o) => format!(
                "<exited {}: {}>",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => format!("<could not run: {e}>"),
        }
    };
    format!(
        "  as the config leaves it : {}\n  with the launch's pin   : {}",
        read(&[]),
        read(&["-c", &format!("features.{feature}=false")]),
    )
}

/// The instrument is a parser, so it is tested rather than trusted.
///
/// A zero from it is the whole finding, and a filter that matched nothing would
/// produce the same zero as a wire that emitted nothing — opposite conclusions
/// from identical output.
#[test]
fn the_broker_log_reader_finds_a_family_it_refused_and_ignores_the_traffic() {
    let log = "\
2026-01-01T00:00:00Z INFO  Ccd: capability tombstoned (id-bearing non-answerable frame): id=Int(0) method=\"item/tool/requestUserInput\"
2026-01-01T00:00:00Z INFO  Ccd: answer upstream (server request not serviceable through this broker; not delivered) (conn 3)
2026-01-01T00:00:00Z INFO  Ccd: capability tombstoned (ambiguous bare id): id=Int(1) method=\"item/tool/call\"
2026-01-01T00:00:00Z INFO  Tui leg ended (conn 2): closed
";
    let found = capability_lines_naming_a_non_phone_family(log);
    assert_eq!(
        found.len(),
        2,
        "the refused family and the upstream answer, and nothing else: {found:#?}"
    );
    assert!(found[0].contains("item/tool/requestUserInput"));
    assert!(found[1].contains(ANSWERED_UPSTREAM));
    assert!(
        capability_lines_naming_a_non_phone_family("").is_empty(),
        "and an empty log finds nothing, which is why the probe asserts the log \
         is not empty before it believes a zero"
    );
}

/// **MEASUREMENT: which non-phone server-request families reach the ccd leg, and
/// with what shape.**
///
/// Three questions, answered on one live session:
///
///   1. Does a real 0.153 app-server EMIT any of the three at all, under prompts
///      written to ask for exactly what each family is for — a permission
///      profile, a question put to the user, an MCP elicitation?
///   2. Of the ones it emits, which are DELIVERED to a subscribed ccd leg? The
///      broker binds `*/requestApproval` and tombstones everything else, so the
///      two answers can differ, and only the delivered ones are a card this
///      daemon could ever raise.
///   3. What is on the frame, verbatim, for the ones that arrive?
///
/// The delivered side is the tap; the emitted side is the broker's own
/// capability log, which names the method of every id-bearing s2c request it
/// refuses to hand on. A family absent from both is a family with no producer
/// this probe could find.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn measure_which_non_phone_families_reach_the_ccd_leg() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("nonph");
    let coord = sb.spawn_coordinator(&codex);
    let broker_log = sb.run_dir.join("broker.log");

    wait_for_the_broker_and_the_tui(&sb).await;
    wait_for_a_composer(&sb).await;

    // ------------------------------------------------------------- the premise
    //
    // **What this probe measures is a LAUNCH, and the launch's own terms are read
    // first.** The permissions family has a producer — a model-callable tool the
    // app-server exposes when `features.request_permissions_tool` is on — so a
    // session in which nothing produces it is telling you about this session's
    // configuration unless the configuration is on the record. Two readings go on
    // the record: what the registry says with the config alone, and what it says
    // with the override a CodeConnect launch writes.
    let feature_rows = feature_registry_rows(&codex, &sb.codex_home, PERMISSIONS_FEATURE);
    println!("FEATURE REGISTRY (this sandbox's CODEX_HOME):\n{feature_rows}");
    assert!(
        feature_rows
            .lines()
            .last()
            .is_some_and(|pinned| pinned.ends_with("false")),
        "the launch's own override must leave the feature off, or the zero below \
         is a measurement of nothing: {feature_rows}"
    );

    // And the pin is on the argv of the process that is actually running, read
    // back from the process table rather than from the launcher's source. The
    // app-server is where a model-callable tool is offered from, so this is the
    // spawn the finding rests on.
    let app_server_argv: Vec<String> =
        tagged_processes(&format!("app-server --listen unix://{}/as.sock", sb.tag()))
            .into_iter()
            .map(|(_, command)| command)
            .collect();
    println!("APP-SERVER ARGV: {app_server_argv:#?}");
    assert!(
        app_server_argv
            .iter()
            .any(|argv| argv.contains(&format!("features.{PERMISSIONS_FEATURE}=false"))),
        "the running app-server must carry the launch's feature pin; without it \
         this probe measures a default rather than a launch: {app_server_argv:#?}"
    );

    run_the_warm_up_turn(&sb).await;
    let (sub, thread_a) = subscribed_tap(&sb, "SUB").await;
    println!("MEASURED thread = {thread_a}");

    // One prompt per family, each written to ask for the thing that family is
    // the wire's word for. They are deliberately explicit: the question is
    // whether a producer EXISTS, so a prompt that leaves the model room to do
    // something else measures the model rather than the wire.
    let probes: [(&str, &str, &str); 5] = [
        (
            "user-input",
            "Enter",
            "Before you do anything else, ask me a clarifying multiple-choice question \
             about what I want, using whatever tool you have for putting a question to \
             the user. Do not run any shell commands.",
        ),
        (
            "network-permission",
            "Enter",
            "Fetch https://example.com and print its first line. Your sandbox has no \
             network access: when the fetch is refused, request the additional network \
             permission you need rather than giving up.",
        ),
        (
            "filesystem-permission-approved",
            "Enter",
            "Write the word hello into /etc/codeconnect-probe-does-not-exist. Your \
             sandbox is read-only: when the write is refused, request the additional \
             filesystem permission you need rather than giving up.",
        ),
        (
            "filesystem-permission-denied",
            "Escape",
            "Write the word hello into /etc/codeconnect-probe-two. When the command \
             approval is declined, do not run another command: ask for the additional \
             filesystem permission itself.",
        ),
        (
            "elicitation",
            "Enter",
            "If any MCP server is connected, call a tool on it that asks me to fill in \
             a form. If no MCP server is connected, say exactly: no mcp server.",
        ),
    ];

    let mut table: Vec<String> = Vec::new();
    let mut compact: Vec<String> = Vec::new();
    for (label, answer, prompt) in probes {
        let from = read_file(&broker_log).lines().count();
        let before: Vec<String> = sub.methods();
        let (hit, accepted) =
            drive_looking_for(&sb, &sub, prompt, answer, Duration::from_secs(240)).await;
        let after = sub.methods();
        let added: Vec<String> = after[before.len().min(after.len())..].to_vec();
        let broker_added = log_lines_after(&read_file(&broker_log), from).join("\n");
        let capability_lines = capability_lines_naming_a_non_phone_family(&broker_added);
        println!("---- PROBE {label} ----");
        println!("  watched family arrived on the ccd leg = {hit}");
        println!(
            "  ordinary approvals answered ({answer}) at the keyboard on the way = {accepted}"
        );
        println!("  methods added to the ccd leg: {added:?}");
        println!("  broker capability/upstream lines: {capability_lines:#?}");
        println!("  pane:\n{}", sb.capture_pane());
        table.push(format!(
            "{label}: delivered_to_ccd={hit} approvals_answered={accepted} \
             ccd_methods={added:?} broker_lines={capability_lines:?}"
        ));
        // The same row without the transcript. A turn puts hundreds of ordinary
        // frames on the leg and none of them is the finding; what a reader needs
        // is how far the turn got and whether any watched family appeared.
        compact.push(format!(
            "{label:34} watched_family_delivered={hit:5} ordinary_approvals_answered={accepted} \
             frames_on_the_ccd_leg={} broker_refusals_naming_one={}",
            added.len(),
            capability_lines.len(),
        ));
    }

    // The verbatim frames, for every watched family that actually arrived.
    for method in NON_PHONE_FAMILIES {
        match sub.first(method) {
            Some(frame) => println!(
                "MEASURED {method} VERBATIM:\n{}",
                serde_json::to_string_pretty(&frame).expect("pretty")
            ),
            None => println!("MEASURED {method}: never delivered to the ccd leg"),
        }
    }
    let whole_log = read_file(&broker_log);
    // **The zero below is only a fact about the wire if the instrument is live.**
    // An unreadable or empty broker.log produces the same empty list as a run in
    // which the app-server emitted none of these families, and the two are
    // opposite conclusions.
    assert!(
        whole_log.lines().count() > 10,
        "the broker log is the witness for a frame that was EMITTED and not \
         delivered; an empty one makes every count below meaningless. {}",
        broker_log.display()
    );
    println!(
        "INSTRUMENT: broker.log has {} lines, {} of them capability rulings",
        whole_log.lines().count(),
        whole_log
            .lines()
            .filter(|l| l.contains("capability "))
            .count()
    );
    println!(
        "ALL broker capability/answer-upstream lines for the run:\n{:#?}",
        capability_lines_naming_a_non_phone_family(&whole_log)
    );
    println!("MEASUREMENT TABLE:\n{}", table.join("\n"));

    write_probe_record(
        &sb,
        &sub,
        "nonphone-families",
        &format!(
            "--- thread={thread_a} ---\n--- feature registry ---\n{feature_rows}\n\
             --- app-server argv ---\n{}\n\
             --- table ---\n{}\n--- broker capability lines naming a watched family ---\n{:#?}\n\
             --- final pane ---\n{}\n",
            app_server_argv.join("\n"),
            compact.join("\n"),
            capability_lines_naming_a_non_phone_family(&whole_log),
            sb.capture_pane()
        ),
    );

    // ------------------------------------------------------ and now the assertion
    //
    // **The zero is asserted, not printed.** A probe that only prints leaves the
    // reading to whoever ran it, and the reading is the finding: what this session
    // establishes is that a CodeConnect launch produces none of these families, and
    // a later release that starts producing one must fail here rather than change a
    // number in somebody's terminal scrollback.
    //
    // It is a claim about a launch, not about codex: the permissions family has a
    // producer, behind a feature this launch pins off (asserted above, before the
    // session was driven). What is measured is the pinned launch.
    let delivered: Vec<&str> = NON_PHONE_FAMILIES
        .into_iter()
        .filter(|method| sub.first(method).is_some())
        .collect();
    assert!(
        delivered.is_empty(),
        "a non-phone family reached the ccd leg: {delivered:?}. The daemon cards \
         none of them, so a delivered one is a question a phone will never be \
         shown — see the verbatim frames printed above."
    );
    // And the other half of the same fact: nothing was emitted-and-refused either.
    // The tap sees only what the broker delivered, so a family the app-server sent
    // and the broker answered upstream would be invisible to the check above; the
    // broker's own capability log is the witness for it, and it names the method of
    // every id-bearing server request it declines to hand on.
    let emitted_and_refused: Vec<String> = capability_lines_naming_a_non_phone_family(&whole_log)
        .into_iter()
        .filter(|line| NON_PHONE_FAMILIES.iter().any(|m| line.contains(m)))
        .collect();
    assert!(
        emitted_and_refused.is_empty(),
        "the app-server emitted a non-phone family and the broker refused it: \
         {emitted_and_refused:#?}. That is a producer, and this daemon's account of \
         these families has to say so."
    );

    teardown(sub, coord, "/tmp/cc-nonphone-probe-unused");
}

// ================================== MEASUREMENT: WHAT THE TUI CALLS THE OPTIONS
//
// The option words this daemon puts on a card are pane-measured, because the
// wire carries no labels. One of them turned out to be command-SPECIFIC: the TUI
// renders the amendment option as "Yes, and don't ask again for commands that
// start with `<argv>`". A single measurement cannot say what `<argv>` is a
// function of, and a set of measurements that AGREE cannot say how its tokens
// are spelled — so this drives programs chosen to disagree on both, and reads
// the rendered row beside the wire body that produced it.

/// The numbered option lines the approval prompt paints, as labels.
///
/// The pane rows look like `› 1. Yes, proceed (y)` and `  2. … (p)`; this strips
/// the selection marker, the ordinal and the hotkey so what is left is the words
/// a person reads. Measured against
/// `fixtures/codex/approval-switch-panes-0.153.txt`.
fn rendered_option_labels(pane: &str) -> Vec<String> {
    let mut out = Vec::new();
    for row in pane.lines() {
        let row = row.trim_start_matches(['›', ' ']).trim_end();
        let Some(dot) = row.find(". ") else { continue };
        if dot == 0 || !row[..dot].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let mut label = row[dot + 2..].trim().to_string();
        // The hotkey the TUI appends, e.g. ` (y)` / ` (p)` / ` (esc)`.
        if label.ends_with(')') {
            if let Some(open) = label.rfind(" (") {
                label.truncate(open);
            }
        }
        if !label.is_empty() {
            out.push(label);
        }
    }
    out
}

/// The approval prompt itself, cut out of a pane that also holds the transcript.
///
/// A pane capture is the whole visible screen, so most of it is earlier turns; the
/// part worth recording is the question and the rows under it. Starts at the
/// prompt's own sentence and runs to the confirm line, with each row's trailing
/// padding trimmed so the record is the text rather than the terminal's width.
fn prompt_section(pane: &str) -> String {
    let start = pane
        .lines()
        .position(|line| line.contains(COMMAND_PROMPT) || line.contains(FILE_CHANGE_PROMPT));
    let Some(start) = start else {
        return format!("<no prompt on the pane>\n{pane}");
    };
    let mut out: Vec<String> = Vec::new();
    for line in pane.lines().skip(start) {
        out.push(line.trim_end().to_string());
        if line.contains("Press enter to confirm") {
            break;
        }
    }
    out.join("\n")
}

/// The pane parser is a parser, so it is tested on the committed capture rather
/// than only on whatever a live run happens to paint.
#[test]
fn the_option_rows_of_a_captured_prompt_are_read_as_labels() {
    const PANES: &str = include_str!("../../../fixtures/codex/approval-switch-panes-0.153.txt");
    let first = PANES
        .split("===== 2.")
        .next()
        .expect("the capture's first section");
    assert_eq!(
        rendered_option_labels(first),
        [
            "Yes, proceed",
            "Yes, and don't ask again for commands that start with `touch`",
            "No, and tell Codex what to do differently",
        ]
    );
}

/// **MEASUREMENT: is the amendment label a function of the command, of which part
/// of it, and does the rendering survive a token a shell would have to quote?**
///
/// One capture showed `touch`, which is equally consistent with the first token
/// of the wire's `command`, the first token of its `commandActions`, and the
/// first element of `proposedExecpolicyAmendment` — they agreed on that one
/// frame. The first three prompts here disagree on all three candidates, so the
/// derivation is read off disagreement rather than assumed from agreement.
///
/// The last three ask the harder question. Every token in the first three is one
/// a shell would leave alone, so "join with spaces" and "escape each token"
/// render identically and the samples cannot tell them apart. These drive a token
/// with a space in it, one with a metacharacter, and one with a newline, and
/// print the painted row beside the wire body that produced it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs a real codex + tmux; run with CC_CODEX_LIVE=1 -- --ignored"]
async fn measure_what_the_tui_calls_the_amendment_option() {
    let Some(codex) = live_gate() else { return };
    let sb = LiveSandbox::new("label");
    let coord = sb.spawn_coordinator(&codex);

    wait_for_the_broker_and_the_tui(&sb).await;
    wait_for_a_composer(&sb).await;
    // Wide enough that no option row is clipped: the label is what the TUI
    // renders, not what an eighty-column pane leaves of it.
    sb.widen(200);
    run_the_warm_up_turn(&sb).await;
    let (sub, thread_a) = subscribed_tap(&sb, "SUB").await;
    println!("MEASURED thread = {thread_a}");

    let stamp = nanos();
    // Different programs, so a label that names one of them says which.
    //
    // The first three separate the candidate derivations: two tokens, three
    // tokens, and one whose argv[0] is an absolute path (if the label carries
    // `/bin/mkdir` the derivation keeps argv[0] verbatim, and if it carries
    // `mkdir` it takes a basename).
    //
    // The last three ask a different question. Every token above is shell-safe,
    // so "the tokens joined by spaces" and "the tokens rendered the way a shell
    // would have to be given them" agree on all of them, and a rule read off
    // agreement is not a rule. These three disagree: a token holding a SPACE, a
    // token holding a shell METACHARACTER, and a token holding a NEWLINE. What
    // the terminal paints for each is the whole point of driving them.
    let commands = [
        format!("mkdir /tmp/cc-label-a.{stamp}"),
        format!("cp /etc/hosts /tmp/cc-label-b.{stamp}"),
        format!("/bin/mkdir /tmp/cc-label-c.{stamp}"),
        format!("touch '/tmp/cc-label-d.{stamp} spaced.txt'"),
        format!("touch '/tmp/cc-label-e.{stamp};semi.txt'"),
        // ANSI-C quoting, so the token really carries a newline rather than a
        // backslash and an `n`. Whether an amendment for it exists at all is
        // part of the measurement.
        format!("touch $'/tmp/cc-label-f.{stamp}\\nnewline.txt'"),
    ];

    let mut rows: Vec<String> = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let before = sub
            .methods()
            .iter()
            .filter(|m| m.ends_with("/requestApproval"))
            .count();
        sb.send_keys(&[&format!(
            "Run the shell command `{command}` now. Do not explain, just run it."
        )]);
        tokio::time::sleep(Duration::from_millis(600)).await;
        sb.send_keys(&["Enter"]);
        let asked = wait_until(Duration::from_secs(240), || {
            sub.methods()
                .iter()
                .filter(|m| m.ends_with("/requestApproval"))
                .count()
                > before
        })
        .await;
        let painted = wait_until(Duration::from_secs(30), || {
            sb.capture_pane().contains(COMMAND_PROMPT)
        })
        .await;
        let pane = sb.capture_pane();
        let frames: Vec<Value> = sub
            .seen()
            .into_iter()
            .filter(|v| v["method"].as_str() == Some("item/commandExecution/requestApproval"))
            .collect();
        let frame = frames.last().cloned().unwrap_or(Value::Null);
        let params = &frame["params"];
        let labels = rendered_option_labels(&pane);
        println!("---- LABEL PROBE {index}: {command} ----");
        println!("  asked={asked} painted={painted}");
        println!("  wire command                     = {}", params["command"]);
        println!(
            "  wire commandActions              = {}",
            params["commandActions"]
        );
        println!(
            "  wire proposedExecpolicyAmendment = {}",
            params["proposedExecpolicyAmendment"]
        );
        println!(
            "  wire availableDecisions          = {}",
            params["availableDecisions"]
        );
        println!("  RENDERED OPTION LABELS           = {labels:#?}");
        println!("  pane:\n{pane}");
        rows.push(format!(
            "===== {} =====\n\
             asked for                        = {command}\n\
             wire command                     = {}\n\
             wire commandActions              = {}\n\
             wire proposedExecpolicyAmendment = {}\n\
             wire availableDecisions          = {}\n\
             {}",
            index + 1,
            params["command"],
            params["commandActions"],
            params["proposedExecpolicyAmendment"],
            params["availableDecisions"],
            // The prompt as the terminal painted it, cut to the prompt itself:
            // the pane also carries every earlier turn's transcript, and a
            // record of the row this prompt painted is what a reader needs.
            prompt_section(&pane),
        ));

        // Esc dismisses the prompt without running anything, so the next probe
        // starts from an idle composer rather than from a turn that is still
        // waiting on this question.
        sb.send_keys(&["Escape"]);
        let cleared = wait_until(Duration::from_secs(60), || {
            !sb.capture_pane().contains(COMMAND_PROMPT)
        })
        .await;
        println!("  prompt cleared by Escape = {cleared}");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // And the other family, whose words the wire says nothing about at all.
    let target = format!("/tmp/cc-label-fc.{stamp}.txt");
    std::fs::write(&target, "hello from the label probe\n").expect("seed the target");
    sb.send_keys(&[&format!(
        "Edit the file {target} so that it says goodbye instead of hello. Use your \
         file-editing tool, not a shell command."
    )]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    sb.send_keys(&["Enter"]);
    let fc_asked = wait_until(Duration::from_secs(240), || {
        sub.methods()
            .iter()
            .any(|m| m == "item/fileChange/requestApproval")
    })
    .await;
    let fc_painted = wait_until(Duration::from_secs(30), || {
        let pane = sb.capture_pane();
        pane.contains(FILE_CHANGE_PROMPT) || pane.contains("Would you like to")
    })
    .await;
    let fc_pane = sb.capture_pane();
    let fc_labels = rendered_option_labels(&fc_pane);
    println!("---- LABEL PROBE fileChange ----");
    println!("  asked={fc_asked} painted={fc_painted}");
    println!(
        "  wire request = {}",
        sub.first("item/fileChange/requestApproval")
            .unwrap_or(Value::Null)
    );
    println!("  RENDERED OPTION LABELS = {fc_labels:#?}");
    println!("  pane:\n{fc_pane}");
    rows.push(format!(
        "===== {} (the other family, whose words name no command) =====\n\
         asked for                        = an edit to {target}\n\
         wire proposedExecpolicyAmendment = (this family declares no decisions)\n\
         {}",
        commands.len() + 1,
        prompt_section(&fc_pane),
    ));
    sb.send_keys(&["Escape"]);
    tokio::time::sleep(Duration::from_secs(3)).await;

    println!("LABEL MEASUREMENT TABLE:\n{}", rows.join("\n\n"));
    write_probe_record(&sb, &sub, "amendment-labels", &rows.join("\n\n"));

    let _ = std::fs::remove_file(&target);
    // **Swept by reading the directory, not by re-parsing the commands.** Some of
    // these commands quote their argument precisely because it holds a space or a
    // metacharacter, so a last-token split of the command text names a fragment
    // rather than a path — and a relative fragment handed to `remove_dir_all` is a
    // deletion aimed at whatever the working directory happens to be. Every path
    // this probe can create is `/tmp/cc-label-*` bearing this run's own stamp, so
    // that is what is removed.
    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("cc-label-") && name.contains(&stamp.to_string()) {
                let path = entry.path();
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
    teardown(sub, coord, "/tmp/cc-label-probe-unused");
}
