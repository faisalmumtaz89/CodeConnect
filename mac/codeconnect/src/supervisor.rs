//! Per-session supervisor.
//!
//! One of these runs per `codeconnect claude`, spawned detached in its own process group
//! and **connecting out** to `ccd`. That direction is the whole point: the
//! daemon never owns a session, so `kill -9 ccd` costs a reconnect and nothing
//! else, and closing the terminal tab cannot take the supervisor with it.
//!
//! Its three jobs:
//!   1. register the session, and re-register after every daemon restart;
//!   2. report liveness positively (silence must read as `stale`, never `idle`);
//!   3. inject keys — but only after confirming the expected prompt is on screen.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use protocol::config::Config;
use protocol::ipc::{
    ClientFrame, DaemonFrame, PromptFingerprint, PromptPresence, RegisterSession,
    SupervisorRequest, SupervisorResult,
};
use protocol::tmux::EXIT_CONFIRMATIONS;

use crate::tmux;

const HEARTBEAT: Duration = Duration::from_secs(5);

/// How often this session's own liveness is checked.
///
/// Two seconds, and with [`EXIT_CONFIRMATIONS`] that puts a real exit in the
/// fleet within about four — fast enough that the view is not stale, and slow
/// enough that a tmux server restarting between polls cannot produce a
/// `SessionEnd` for a session that is still running. A reported exit is a
/// durable fact and cannot be withdrawn, which is what makes the second look
/// worth the two seconds. How much evidence an exit takes is shared with the
/// daemon's fleet sweep, which needs a second look for the same reason at a
/// much longer interval.
const LIVENESS_POLL: Duration = Duration::from_secs(2);
const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(10);

pub struct SupervisorArgs {
    pub session_id: String,
    /// The run's identity, minted by `codeconnect claude`. `None` only for a supervisor
    /// left over from a build that predates it — the daemon then resolves the
    /// name, which is the behaviour that keeps an in-place upgrade seamless.
    pub session_uid: Option<String>,
    pub tmux_session: String,
    pub cwd: String,
    pub claude_bin: Option<String>,
}

/// The connection `serve_once` is currently blocked on, so the liveness thread
/// can end that block from the outside.
type Link = Arc<Mutex<Option<UnixStream>>>;

pub fn run(args: SupervisorArgs, config: &Config) -> Result<()> {
    let started_at = protocol::time::now_rfc3339();
    let socket = protocol::socket_path();
    let session_gone = Arc::new(AtomicBool::new(false));
    let link: Link = Arc::new(Mutex::new(None));
    // Built once and held for the life of the supervisor, because it has to
    // outlive the daemon: a session that starts and ends while `ccd` is down
    // has never been registered, and this is what introduces it before the exit
    // is reported. See `report_exit`.
    let registration = registration_frame(&args, &started_at);

    // Liveness is watched independently of the daemon connection, so a session
    // that ends while ccd is down is still reported the moment ccd returns.
    {
        let tmux_session = args.tmux_session.clone();
        // Carried by value so the thread can write to the run's own log without
        // borrowing the argument struct the main loop is using.
        let log_session_id = args.session_id.clone();
        let log_session_uid = args.session_uid.clone();
        let session_gone = Arc::clone(&session_gone);
        let link = Arc::clone(&link);
        std::thread::Builder::new()
            .name("liveness".into())
            .spawn(move || {
                // Consecutive observations of absence. One is not evidence: a
                // tmux server restarting, or a `has-session` that lost a race
                // with the server's own startup, produces a single `Gone` for a
                // session that is perfectly alive — and a reported exit cannot
                // be taken back.
                let mut gone_streak = 0u32;
                loop {
                    std::thread::sleep(LIVENESS_POLL);
                    match probe_liveness(&tmux_session, log_session_uid.as_deref()) {
                        tmux::SessionPresence::Present => gone_streak = 0,
                        // We could not look. Emphatically not an exit — this is
                        // the case that used to be indistinguishable from one,
                        // because any non-zero tmux status was read as absence.
                        tmux::SessionPresence::Unknown(why) => {
                            gone_streak = 0;
                            log_for(
                                &log_session_id,
                                log_session_uid.as_deref(),
                                &format!("tmux liveness unknown: {why}"),
                            );
                        }
                        tmux::SessionPresence::Gone => {
                            gone_streak += 1;
                            if gone_streak < EXIT_CONFIRMATIONS {
                                continue;
                            }
                            session_gone.store(true, Ordering::SeqCst);
                            // Shut the socket down so the read below returns.
                            //
                            // Without this the supervisor sits in a blocking
                            // read on a daemon that is perfectly healthy and
                            // never gets back to the `session_gone` check — so a
                            // session that exits while ccd is *up* is never
                            // reported as ended. Measured: the fleet showed it
                            // `live · detached` until the next daemon restart
                            // happened to close the socket.
                            if let Some(stream) = link
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .take()
                            {
                                let _ = stream.shutdown(Shutdown::Both);
                            }
                            return;
                        }
                    }
                }
            })
            .context("spawning the liveness thread")?;
    }

    let mut backoff = RECONNECT_MIN;
    loop {
        if session_gone.load(Ordering::SeqCst) {
            // Best effort: if ccd is down there is nobody to tell, and the
            // daemon infers the exit from the lost connection anyway.
            let _ = report_exit(&socket, &registration);
            return Ok(());
        }

        match serve_once(&socket, &args, &registration, config, &session_gone, &link) {
            Ok(()) => backoff = RECONNECT_MIN,
            Err(err) => {
                log_line(&args, &format!("daemon link lost: {err:#}"));
            }
        }
        // No backoff when the session is what ended: the exit is news, and a
        // reconnect delay would hold it back for no reason.
        if session_gone.load(Ordering::SeqCst) {
            continue;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

/// Liveness of this supervisor's own session.
///
/// When a well-formed `session_uid` is known, this resolves **by uid** on the
/// private server (the UID-atomic primitive, `protocol::tmux::owned_liveness`):
/// a `cc-N` name that a *different* run has since reused no longer resolves for
/// our uid, so a name-reuse race reads `Gone` and the supervisor terminates its
/// now-dead session — where the old name-addressed `has-session` would have seen
/// the stranger under the reused name and reported `Present` indefinitely. For a
/// healthy session both agree (`Live` ⇒ `Present`), and an unreachable server is
/// `Unknown` either way, so Claude's observable behavior on the healthy path is
/// **unchanged**; only the reuse race differs. A uid-less (legacy/adopted)
/// session keeps the exact name-addressed check it always had.
fn probe_liveness(tmux_session: &str, session_uid: Option<&str>) -> tmux::SessionPresence {
    match session_uid {
        Some(uid) if protocol::uid::is_well_formed(uid) => {
            match protocol::tmux::owned_liveness(protocol::TMUX_SOCKET_NAME, uid, None) {
                protocol::tmux::OwnedLiveness::Live => tmux::SessionPresence::Present,
                protocol::tmux::OwnedLiveness::Gone => tmux::SessionPresence::Gone,
                protocol::tmux::OwnedLiveness::Unknown(why) => tmux::SessionPresence::Unknown(why),
            }
        }
        _ => tmux::session_presence(tmux_session),
    }
}

fn serve_once(
    socket: &std::path::Path,
    args: &SupervisorArgs,
    registration: &RegisterSession,
    config: &Config,
    session_gone: &Arc<AtomicBool>,
    link: &Link,
) -> Result<()> {
    let stream = UnixStream::connect(socket).context("connecting to ccd")?;
    let writer = Arc::new(Mutex::new(
        stream.try_clone().context("cloning the socket")?,
    ));
    // Published before the first read so the liveness thread can always reach
    // it; cleared on the way out so a stale handle is never shut down twice.
    *link.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(stream.try_clone().context("cloning the socket")?);
    let reader = BufReader::new(stream);

    // The same frame `report_exit` replays. Built once, in one place, so a
    // registration that introduces a session cannot drift from the one that
    // reports it.
    send(&writer, &ClientFrame::Register(registration.clone()))?;
    log_line(args, "registered with ccd");

    // Heartbeats stop when the connection dies: the send fails, the thread ends,
    // and the next connection starts a fresh one.
    {
        let writer = Arc::clone(&writer);
        let session_id = args.session_id.clone();
        let session_uid = args.session_uid.clone();
        let session_gone = Arc::clone(session_gone);
        std::thread::Builder::new()
            .name("heartbeat".into())
            .spawn(move || loop {
                std::thread::sleep(HEARTBEAT);
                if session_gone.load(Ordering::SeqCst) {
                    return;
                }
                if send(
                    &writer,
                    &ClientFrame::Heartbeat {
                        session_id: session_id.clone(),
                        session_uid: session_uid.clone(),
                    },
                )
                .is_err()
                {
                    return;
                }
            })
            .context("spawning the heartbeat thread")?;
    }

    let outcome = read_frames(reader, args, config, &writer);
    // The handle is dropped whether the loop ended cleanly or not: leaving a
    // closed socket published would make the liveness thread shut down a
    // descriptor that the next connection may already have reused.
    let _ = link
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    outcome
}

fn read_frames(
    reader: BufReader<UnixStream>,
    args: &SupervisorArgs,
    config: &Config,
    writer: &Arc<Mutex<UnixStream>>,
) -> Result<()> {
    for line in reader.lines() {
        let line = line.context("reading from ccd")?;
        if line.trim().is_empty() {
            continue;
        }
        let frame: DaemonFrame = match serde_json::from_str(&line) {
            Ok(frame) => frame,
            Err(err) => {
                log_line(args, &format!("undecodable frame: {err}"));
                continue;
            }
        };
        if let DaemonFrame::SupervisorRequest { id, request } = frame {
            let result = handle_request(args, config, request);
            send(writer, &ClientFrame::SupervisorResponse { id, result })?;
        }
    }
    Ok(())
}

fn handle_request(
    args: &SupervisorArgs,
    config: &Config,
    request: SupervisorRequest,
) -> SupervisorResult {
    match request {
        SupervisorRequest::Ping => SupervisorResult::Pong,
        SupervisorRequest::Capture {
            lines,
            visible_only,
        } => {
            let captured = if visible_only {
                tmux::capture_visible_pane(&args.tmux_session)
            } else {
                tmux::capture_pane(&args.tmux_session, lines.clamp(1, 5000))
            };
            match captured {
                Ok(text) => SupervisorResult::Snapshot { text },
                Err(err) => SupervisorResult::Error {
                    message: format!("{err:#}"),
                },
            }
        }
        SupervisorRequest::SendText {
            text,
            require,
            targets_composer,
            submit,
            expect,
            asking,
            recover_composer,
            capture_recovered,
            confirm_view,
            respond_by_monotonic_ms,
        } => send_text(
            args,
            config,
            SendPlan {
                text: &text,
                submit,
                expect: expect.as_ref(),
                respond_by_monotonic_ms,
            },
            Target {
                presence: &require,
                is_composer: targets_composer,
            },
            Recovery {
                enabled: recover_composer,
                capture: capture_recovered,
                confirm: confirm_view,
                asking,
            },
        ),
    }
}

/// Inject keys, gated on positive prompt-presence confirmation.
///
/// The check is the safety interlock for the whole mirror model: without it a
/// phone answer that arrives a second late types "1" into whatever replaced the
/// prompt. Refusing is always the safe outcome — the operator still has the
/// keyboard.
///
/// Two things make the check mean what it says:
///
/// * It reads the **visible pane only**. With scrollback, "a permission prompt
///   is on screen" was true for as long as one had ever been on screen, so a
///   prompt answered half an hour ago could authorise typing into a composer.
/// * When `expect` is set it must still be **the same prompt**. Presence alone
///   cannot tell prompt A from prompt B — they have the same wording and the
///   same options — and "1" typed at the wrong one approves something nobody
///   read.
///
/// Both happen here, in the same breath as the keystroke, because any distance
/// between "we looked" and "we typed" is a window the screen can change in.
/// What the caller asked for after the keys land. Measured need: some slash
/// commands replace Claude's composer with a view, and while it is up the
/// presence interlock refuses every further send — the phone is locked out
/// of its own session until a human presses Esc at the Mac.
#[derive(Debug, Clone)]
struct Recovery {
    enabled: bool,
    capture: bool,
    /// Complete the view instead of dismissing it, and the needle that must be
    /// on screen before that key is allowed — see
    /// `SupervisorRequest::SendText::confirm_view`. `None` is the ordinary
    /// rescue.
    confirm: Option<String>,
    /// The operator's own definition of a prompt that is asking something,
    /// if they have given one. See `SupervisorRequest::SendText::asking`.
    asking: Option<PromptPresence>,
}

/// The measured timings behind the recovery check. Every number came from
/// injecting the real commands and sampling the composer needle:
///
///  * dialogs take the composer away **immediately** — absent at the 50ms
///    sample, every time — so waiting longer buys nothing for detection;
///  * a large inline render (`/context`) can hide the composer footer for
///    up to ~2.5s and then bring it back **by itself**, so a single check
///    at 1.5s would Escape into a view that needed no rescue. Hence the
///    second look at 3.0s: only a composer still gone then is a view;
///  * Escape restores the composer in 93-126ms wherever it works at all
///    (nine runs, three pane widths),
///    so 250ms of verification is generous.
const RECOVERY_FIRST_CHECK: Duration = Duration::from_millis(1_500);
const RECOVERY_SECOND_CHECK: Duration = Duration::from_millis(3_000);
const RECOVERY_VERIFY_WINDOW: Duration = Duration::from_millis(250);
/// One recovery "look" at worst: a capture and a cursor read, each bounded
/// by the tmux operation deadline.
const LOOK_WORST: Duration = Duration::from_secs(2);
/// One keystroke at worst — a single bounded tmux call.
const KEY_WORST: Duration = Duration::from_secs(1);
/// Answering costs a serialize and a socket write; the budget keeps this
/// much aside so an answer computed in time is also *delivered* in time.
const RESPONSE_MARGIN: Duration = Duration::from_millis(500);
/// What a daemon too old to stamp its deadline (pre-minor-12) actually
/// waits: its compiled default. An unstamped request is budgeted against
/// this, never against the newer, larger default.
const LEGACY_DAEMON_WAIT_MS: u64 = 5_000;
/// The confirming path's own window. `Enter` commits a model change and Claude
/// Code redraws the transcript around it, which the 250ms figure above — taken
/// from dismissing an already-drawn view — was never measured against. Measured
/// on 2.1.223: the composer returned within 250ms in the observed runs, so this
/// is headroom for a slower redraw rather than a figure the fast path needs.
const CONFIRM_VERIFY_WINDOW: Duration = Duration::from_millis(1_500);

/// What these keys are being typed into: how to recognise it on screen, and
/// whether it is Claude's composer.
///
/// One type because they are one fact. Kept apart, "is this the composer" was
/// read off the presence variant — which stops being true the moment an
/// operator configures needle overrides and the variant becomes `AnyOf`.
struct Target<'a> {
    presence: &'a PromptPresence,
    is_composer: bool,
}

/// The pane and who has its keyboard, read together.
fn look_at_pane(session: &str) -> Result<(String, tmux::Keyboard)> {
    Ok((
        tmux::capture_visible_pane(session)?,
        tmux::who_has_the_keyboard(session)?,
    ))
}

/// One send as the daemon asked for it: the keys, whether they are
/// submitted, the prompt identity they may be bound to, and when the daemon
/// stops listening for the answer.
struct SendPlan<'a> {
    text: &'a str,
    submit: bool,
    expect: Option<&'a PromptFingerprint>,
    respond_by_monotonic_ms: Option<u64>,
}

fn send_text(
    args: &SupervisorArgs,
    config: &Config,
    plan: SendPlan<'_>,
    target: Target<'_>,
    recovery: Recovery,
) -> SupervisorResult {
    let SendPlan {
        text,
        submit,
        expect,
        respond_by_monotonic_ms,
    } = plan;
    // The whole request answers by this moment or the daemon stops
    // listening; every deliberate wait below spends against it. The daemon
    // stamps the deadline when it *sends* — the host's one monotonic clock,
    // immune to wall steps — so
    // time this request spent queued is already spent; a daemon old enough
    // not to stamp gets the config-derived budget from when reading began.
    let margin = |total_ms: u64| Duration::from_millis(total_ms).saturating_sub(RESPONSE_MARGIN);
    // The local anchor is taken BEFORE the shared clock is read: any
    // descheduling between the two samples then shrinks the budget instead
    // of extending it past the daemon's patience. Clock failure never
    // grants: a stamped request whose clock cannot be read is refused, and
    // only a stamp-less legacy request falls back — capped at what that
    // daemon actually waits.
    let anchored = std::time::Instant::now();
    let answer_by = match (respond_by_monotonic_ms, protocol::time::now_monotonic_ms()) {
        (Some(stamp), Some(now)) => anchored + margin(stamp.saturating_sub(now)),
        // No stamp: a daemon from before minor 12, whose compiled wait was
        // five seconds and whose queue delay is unknowable. The budget is
        // the smaller of the shared config value and that compiled wait —
        // a larger config helps only a daemon new enough to stamp, and
        // over-granting here is exactly the types-after-the-daemon-hung-up
        // window; under-granting merely refuses, which a retry survives.
        (None, _) => anchored + margin(config.supervisor_timeout_ms.min(LEGACY_DAEMON_WAIT_MS)),
        // A stamp arrived and this process cannot read the clock it is
        // written against. Any substitute budget could outlive the daemon's
        // real wait; nothing has been typed, so refusing is the safe answer.
        (Some(_), None) => {
            let reason = "this Mac's monotonic clock could not be read, so the send could \
                          not be bounded; nothing was typed"
                .to_string();
            log_line(args, &format!("refused: {reason}"));
            return SupervisorResult::Refused { reason };
        }
    };
    let require = target.presence;
    // A request that aged out in the queue — or that cannot afford even the
    // first pane read — is refused before anything is looked at: nothing
    // has been typed, so this is the one place lateness is still harmless.
    if std::time::Instant::now() + LOOK_WORST > answer_by {
        let reason = format!(
            "the daemon stops listening in {}ms, which is not enough to read the pane; \
             nothing was typed",
            answer_by
                .saturating_duration_since(std::time::Instant::now())
                .as_millis()
        );
        log_line(args, &format!("refused: {reason}"));
        return SupervisorResult::Refused { reason };
    }
    // One look: the pane, and who has the keyboard. Two facts, one moment,
    // one failure path — and **a look that failed is a refusal**, never a
    // permissive default. The interlock's whole job is to refuse when
    // readiness has not been established, and "we could not tell" is not
    // establishment. A pane's text cannot say who has the keyboard; see the
    // note above `recover_composer`.
    let (mut pane, mut keyboard) = match look_at_pane(&args.tmux_session) {
        Ok(look) => look,
        Err(err) => {
            let reason = format!("could not read the pane: {err:#}");
            log_line(args, &format!("refused: {reason}"));
            return SupervisorResult::Refused { reason };
        }
    };

    // The two keystrokes and the settle between them. Computed here because
    // the exit below has to know what it is making room for, and checked
    // again after the gates because time passes in between.
    let actuation_worst = KEY_WORST
        + if submit {
            Duration::from_millis(config.send_keys_delay_ms) + KEY_WORST
        } else {
            Duration::ZERO
        };

    // A pane parked on a scroll position is left here, by us, and the send
    // goes on as if it had never been scrolled.
    //
    // The clock is the other half of the condition, and it is not a detail.
    // This is the one place the daemon changes what the Mac is showing
    // without being asked to, so it may only do so for a send it can still
    // finish: the exit, the second look and the keystrokes all have to fit in
    // what is left. With no room for all three, nothing is touched and the
    // refusal below is the one that was always given — which tells the person
    // at the Mac what to press.
    let affordable =
        std::time::Instant::now() + KEY_WORST + LOOK_WORST + actuation_worst <= answer_by;
    (pane, keyboard) = match look_the_gates_run_on(
        &target,
        (pane, keyboard),
        affordable,
        &|| {
            // Reported, never concluded from: a tmux that exits 0 without
            // leaving the mode and one that exits 1 having left it are both
            // answered by the look that follows. The log records both — the
            // one unasked-for act on the Mac's screen this program performs,
            // and the reason a refusal that comes anyway had one.
            match tmux::leave_copy_mode(&args.tmux_session) {
                Ok(()) => log_line(args, "left the pane's copy-mode to type"),
                Err(err) => log_line(
                    args,
                    &format!("could not leave the pane's copy-mode: {err}"),
                ),
            }
        },
        &|| look_at_pane(&args.tmux_session),
    ) {
        Ok(look) => look,
        Err(err) => {
            let reason = format!("could not read the pane: {err:#}");
            log_line(args, &format!("refused: {reason}"));
            return SupervisorResult::Refused { reason };
        }
    };

    let matched = match authorise(&pane, require, expect, target.is_composer, keyboard) {
        Ok(matched) => matched,
        Err(reason) => {
            log_line(args, &format!("refused: {reason}"));
            return SupervisorResult::Refused { reason };
        }
    };

    // Affordable *before* the first keystroke goes out: there is deliberately
    // no stop between typing and Enter — stopping there strands staged text —
    // so the check that keeps this request inside the daemon's window lives
    // here, where refusing is still a refusal.
    if std::time::Instant::now() + actuation_worst > answer_by {
        let reason = format!(
            "typing needs up to {}ms and the daemon stops listening in {}ms; nothing was typed",
            actuation_worst.as_millis(),
            answer_by
                .saturating_duration_since(std::time::Instant::now())
                .as_millis()
        );
        log_line(args, &format!("refused: {reason}"));
        return SupervisorResult::Refused { reason };
    }

    // **What the keyboard read above is, and is not.** It is a look, and the
    // keys go out a few milliseconds after it — so a wheel tick in between is
    // delivered to a mode that was not there when this was decided. Measured
    // on tmux 3.7b: `send-keys -l` into a copy-mode **exits 0** and the text
    // reaches the program not at all, and an `Enter` arriving during the
    // settle below is swallowed the same way, leaving the text staged and
    // unsubmitted. Either way this reports `Sent`.
    //
    // The window sits between the look and the keystroke, and the one hand
    // able to open it is on the wheel: a scrolled pane is left and typed
    // into rather than refused, so a wheel that keeps moving can put the
    // mode back inside the gap. Closing it means reading the keyboard once
    // more *after* the keys, which is what would turn `Sent` back into an
    // observation.
    //
    // ESC has no literal-send story that survives every tmux version; the named
    // key does.
    let session = args.tmux_session.clone();
    if let Err(failure) = actuate_with(
        &|| {
            if text == "\u{1b}" {
                tmux::send_key(&session, "Escape")
            } else {
                tmux::send_literal(&session, text)
            }
        },
        &|| tmux::send_key(&session, "Enter"),
        submit,
        Duration::from_millis(config.send_keys_delay_ms),
    ) {
        let result = failure.result();
        match &result {
            SupervisorResult::Refused { reason } => log_line(args, &format!("refused: {reason}")),
            SupervisorResult::Error { message } => log_line(args, &format!("degraded: {message}")),
            _ => {}
        }
        return result;
    }

    log_line(
        args,
        &format!("typed {} byte(s) after matching {matched:?}", text.len()),
    );
    if recovery.enabled && submit {
        return recover_composer(args, answer_by, require, &recovery, text, matched);
    }
    SupervisorResult::Sent { matched }
}

/// Where the actuation failed — because the phase decides what the failure
/// *means* to whoever asked for the keys.
#[derive(Debug)]
enum ActuationFailure {
    /// Typing failed. Whether anything landed depends on how it failed.
    Typing(tmux::TmuxError),
    /// Enter failed after the text landed. The text is staged in the
    /// composer whatever kind of failure this is.
    Submitting(tmux::TmuxError),
}

impl ActuationFailure {
    /// The honest answer for each phase, in the daemon's existing vocabulary:
    /// `Refused` releases the claim (a retry types fresh), everything else
    /// leaves it held (a retry is told "unknown" instead of typing twice).
    ///
    ///   * Typing failed provably — spawn failure or a tmux refusal — nothing
    ///     landed: `Refused`, retry safe.
    ///   * Typing was killed at the deadline — the server may have acted
    ///     before stalling: indeterminate.
    ///   * Enter failed in any way at all — the text is already staged, and a
    ///     retry would stage it twice: indeterminate.
    fn result(&self) -> SupervisorResult {
        match self {
            ActuationFailure::Typing(err) if !err.outcome_is_indeterminate() => {
                SupervisorResult::Refused {
                    reason: format!("{err}; nothing was typed"),
                }
            }
            ActuationFailure::Typing(err) => SupervisorResult::Error {
                message: format!(
                    "{err}. The keys may have reached the session, so they will not be sent \
                     again; check the Mac."
                ),
            },
            ActuationFailure::Submitting(err) => SupervisorResult::Error {
                message: format!(
                    "the text was typed but Enter was not confirmed ({err}); it may or may \
                     not have been submitted, so it will not be sent again — check the Mac."
                ),
            },
        }
    }
}

/// The two keystrokes of an injection, over injectable steps so a test can
/// fail either one and prove what happens to the other. Claude's composer
/// needs a beat between the text landing and Enter, or the newline can be
/// swallowed by the render pass — hence the delay between them.
fn actuate_with(
    type_keys: &dyn Fn() -> Result<(), tmux::TmuxError>,
    press_enter: &dyn Fn() -> Result<(), tmux::TmuxError>,
    submit: bool,
    settle: Duration,
) -> Result<(), ActuationFailure> {
    type_keys().map_err(ActuationFailure::Typing)?;
    if submit {
        std::thread::sleep(settle);
        press_enter().map_err(ActuationFailure::Submitting)?;
    }
    Ok(())
}

/// Did the keys we just typed take the composer away — and if so, give it
/// back.
///
/// The postcondition, not a guess about what appeared: the only claims made
/// are "the composer was gone" and "after one Escape it was back". The pane
/// is read for meaning for exactly one purpose — never to identify what
/// opened, only to notice that the screen is *asking something*, which is
/// the one state no key may be sent into.
fn recover_composer(
    args: &SupervisorArgs,
    answer_by: std::time::Instant,
    composer: &PromptPresence,
    recovery: &Recovery,
    text: &str,
    matched: String,
) -> SupervisorResult {
    let session = args.tmux_session.clone();
    let outcome = recover_composer_with(
        RecoveryPass {
            answer_by,
            asking: recovery.asking.clone(),
            matched,
            capture: recovery.capture,
            confirm: recovery.confirm.clone(),
        },
        // One look is a pane *and* the cursor, taken together: apart, they
        // can describe two different moments, and the whole question is
        // whether this drawn composer has the keyboard.
        &|| look_at_pane(&session),
        &|key| tmux::send_key(&session, key).map_err(anyhow::Error::from),
        composer,
    );
    match &outcome {
        SupervisorResult::ComposerRecovered { .. } => log_line(
            args,
            &format!("recovery: {text:?} took the composer; Escape restored it"),
        ),
        SupervisorResult::ViewConfirmed { .. } => log_line(
            args,
            &format!("recovery: {text:?} opened a view; Enter completed it"),
        ),
        SupervisorResult::ComposerLost { .. } => log_line(
            args,
            &format!("recovery: composer gone after {text:?}; Escape did not restore it"),
        ),
        SupervisorResult::RecoveryUnconfirmed { reason, .. } => {
            if reason.contains("did not answer") {
                log_line(args, &format!("degraded: {reason}"));
            }
            log_line(
                args,
                &format!("recovery: nothing claimed after {text:?}; {reason}"),
            )
        }
        _ => {}
    }
    outcome
}

/// The postcondition itself, over injectable look and Escape steps so a test
/// can drive it against a real tmux pane with no daemon and no Claude.
///
/// A view is never *identified* — only observed to have been there and then
/// gone. The pane is read for meaning for exactly one purpose, and it is not
/// to say what opened: to notice that the screen is **asking something**,
/// which is the one state no key may be sent into.
/// Everything one recovery pass needs beyond its two injectable steps.
struct RecoveryPass {
    /// When the daemon stops listening for this request's answer, minus the
    /// response margin. Recovery's observation windows are deliberate and
    /// are not shortened under pressure — a window too short to observe
    /// honestly proves nothing — so when the remaining budget cannot fit
    /// the next step, recovery stops and says so instead of answering after
    /// the daemon has hung up.
    answer_by: std::time::Instant,
    /// The operator's definition of a prompt, when they have one.
    asking: Option<PromptPresence>,
    matched: String,
    capture: bool,
    /// Complete the view rather than dismiss it, and what must be on screen
    /// for that to be allowed. `Enter` commits, so it is held to a standard
    /// the dismissing path is not: the screen must show the selected
    /// affirmative row for the value that was asked for, and must not have
    /// moved since it appeared. When it cannot, the ordinary rescue runs
    /// instead.
    confirm: Option<String>,
}

fn recover_composer_with(
    pass: RecoveryPass,
    look: &dyn Fn() -> Result<(String, tmux::Keyboard)>,
    // Takes the key because the choice is made *here*, after the guards: a
    // confirmation that cannot be established downgrades to the ordinary
    // rescue rather than stranding the view.
    send_key: &dyn Fn(&str) -> Result<()>,
    // The **effective** composer presence, overrides included — never
    // `InputBox` assumed. An operator configures `input_box_needles`
    // precisely when the defaults have stopped matching Claude's TUI, and a
    // recovery pass still looking for the defaults would find no composer on
    // a perfectly healthy screen and Escape it after every slash command.
    composer: &PromptPresence,
) -> SupervisorResult {
    let RecoveryPass {
        answer_by,
        asking,
        matched,
        capture,
        confirm,
    } = pass;
    // "Ready" means able to take keys, not merely drawn: a view rendered above
    // a live-looking composer is the measured lockout this postcondition
    // exists to clear, and it must not read as recovery already having
    // happened. See the note above `recover_composer`.
    //
    // A pane in tmux's scrollback is not ready either, and the response to
    // that is the same `Escape` this pass already sends — but only under
    // `mode-keys emacs`, where Escape is bound to `cancel`. Measured on tmux
    // 3.7b: under `mode-keys vi` — which tmux picks by itself when `$EDITOR`
    // or `$VISUAL` looks like vi, and which [`tmux::render_server_conf`] does
    // not pin — Escape is `clear-selection`, the pane stays in the mode, and
    // this pass runs out its window and reports the composer lost.
    //
    // The way in is somebody scrolling between the keys going out and the
    // first check — and the send path invites that hand closer than any
    // other, since a scrolled pane is left and typed into rather than
    // refused. Left as it is because the postcondition holds either way,
    // and an Escape is what the drawn-but-dead composer needs; the send
    // path's narrower act is [`may_leave_copy_mode`].
    let ready = |pane: &str, keyboard: tmux::Keyboard| {
        composer.find_match(pane, None).is_some() && keyboard.reaches_the_program()
    };
    // Nothing was observed yet, so nothing may be claimed. Every early exit
    // below says which look failed, and the daemon turns that into the
    // "typed, outcome unknown" result rather than a claim about a composer
    // nobody managed to see.
    let unconfirmed = |stage: &str| SupervisorResult::RecoveryUnconfirmed {
        matched: matched.clone(),
        reason: format!("the pane could not be read {stage}"),
    };
    // The reason carries the look's own error: "could not be read" without
    // the why turned a killed-at-deadline tmux into an anonymous shrug.
    let unconfirmed_because =
        |stage: &str, err: &anyhow::Error| SupervisorResult::RecoveryUnconfirmed {
            matched: matched.clone(),
            reason: format!("the pane could not be read {stage}: {err:#}"),
        };
    let out_of_budget = |needs: Duration| SupervisorResult::RecoveryUnconfirmed {
        matched: matched.clone(),
        reason: format!(
            "the keys were typed, but the next recovery step needs {}ms and the daemon \
             stops listening in {}ms; recovery stopped rather than answer after the daemon hung up",
            needs.as_millis(),
            answer_by
                .saturating_duration_since(std::time::Instant::now())
                .as_millis()
        ),
    };
    // What each block below costs at worst: its deliberate wait plus its
    // bounded tmux calls. Checked before spending, not after.
    let budget_allows = |needs: Duration| std::time::Instant::now() + needs <= answer_by;

    // First look. A composer still here settles it — the overwhelmingly
    // common case, and it costs one look.
    //
    // The pane is **kept** rather than discarded: a confirming key is only
    // allowed on a screen that has been the same screen since it appeared, and
    // this is when it appeared. Taking a second look for that would buy the
    // same frame at the price of another failure point on a path that never
    // confirms anything.
    let first_window = RECOVERY_FIRST_CHECK + LOOK_WORST;
    if !budget_allows(first_window) {
        return out_of_budget(first_window);
    }
    std::thread::sleep(RECOVERY_FIRST_CHECK);
    let first_absent = match look() {
        Ok((pane, keyboard)) if ready(&pane, keyboard) => {
            return SupervisorResult::Sent { matched }
        }
        Ok((pane, _)) => pane,
        Err(err) => return unconfirmed_because("after the command was typed", &err),
    };

    // Second look: a large inline render can hide the composer footer for a
    // while and then bring it back with nobody's help — measured on
    // `/context`, absent about 2.5s and self-restoring. Escaping into that
    // would be a rescue nothing needed.
    let second_window = (RECOVERY_SECOND_CHECK - RECOVERY_FIRST_CHECK) + LOOK_WORST;
    if !budget_allows(second_window) {
        return out_of_budget(second_window);
    }
    std::thread::sleep(RECOVERY_SECOND_CHECK - RECOVERY_FIRST_CHECK);
    let absent_pane = match look() {
        Ok((pane, keyboard)) if ready(&pane, keyboard) => {
            return SupervisorResult::Sent { matched }
        }
        Ok((pane, _)) => pane,
        Err(err) => return unconfirmed_because("while checking whether a view had opened", &err),
    };

    // **A decision on screen is never Escaped.** A slash command can be a
    // skill, a skill can reach a tool, and a tool can raise a permission
    // prompt inside this window — which hides the composer exactly as a view
    // does. Escape there cancels a decision the human never made, and does it
    // silently. The prompt is not a lockout: it is the session asking for
    // something, the phone can answer it through the card it already has, and
    // the composer returns the moment it is answered.
    //
    // The question itself, not the generic `Esc to cancel` that
    // `PermissionPrompt` also accepts: the measured Settings view offers that
    // same hint, so keying on it would decline to rescue the very lockout
    // this postcondition exists for. A prompt asks something.
    // The operator's own needles when they have configured any — they exist
    // precisely because Claude's wording changed, and a guard still looking
    // for the old question would Escape the prompt they taught us to see.
    // Otherwise the question itself: narrower than the default permission
    // needles, which include `esctocancel`, a hint Claude's *views* also
    // offer and which would stop the rescue this postcondition exists for.
    let asking = asking.unwrap_or(PromptPresence::AnyOf {
        needles: vec!["do you want to proceed".to_string()],
    });
    // Looked at again here, not judged from the frame saved a moment ago: a
    // tool call reaches its prompt on its own schedule, and the pane this key
    // is about to land on is the pane as it is *now*. Checking the older frame
    // would leave exactly the window this guard exists to close.
    //
    // **One look, pane and keyboard together.** Apart they describe two
    // moments, and the whole question is what this key will land on.
    if !budget_allows(LOOK_WORST) {
        return out_of_budget(LOOK_WORST);
    }
    let (before_key, keyboard_before_key) = match look() {
        Ok(look) => look,
        Err(err) => return unconfirmed_because("just before the key was sent", &err),
    };
    if asking.find_match(&before_key, None).is_some()
        || asking.find_match(&absent_pane, None).is_some()
    {
        return SupervisorResult::RecoveryUnconfirmed {
            matched,
            reason: "a prompt is waiting for an answer on the Mac, so no key was sent".into(),
        };
    }

    // **Confirming commits; dismissing does not.** `Escape` closes whatever is
    // there; `Enter` *selects* whatever is highlighted. So the confirming path
    // has to establish more — and when it cannot, it does not strand the Mac:
    // it falls back to the ordinary rescue, which is what this postcondition
    // would have done for this command anyway. Withholding both keys would
    // leave the view open and the composer captured, which is the lockout the
    // whole mechanism exists to clear.
    let confirming = match &confirm {
        None => false,
        Some(needle) => {
            // One needle, anchored to the *selected* affirmative row and
            // carrying the requested value. Split into separate structural and
            // value needles it proved nothing: the pane includes Claude Code's
            // echo of the command just submitted, so a bare value needle
            // matched itself, and a bare affirmative needle matched whichever
            // row was highlighted. Joined, it can only match the row that is
            // both selected and about the value that was asked for.
            let shown = protocol::ipc::normalize_for_match(&before_key);
            let same_screen = before_key == absent_pane && absent_pane == first_absent;
            let authorised = shown.contains(needle.as_str())
                // Raw, not normalised: normalising folds whitespace and case,
                // and a difference there is still a different screen.
                && same_screen
                // The view itself has to be the thing holding the keyboard.
                // A composer with it means the view is already gone, and a
                // pane in scrollback means tmux takes this `Enter` and the
                // dialog never sees it.
                && keyboard_before_key == tmux::Keyboard::View;
            authorised
        }
    };

    let key = if confirming { "Enter" } else { "Escape" };
    let key_and_verify = KEY_WORST
        + if confirming {
            CONFIRM_VERIFY_WINDOW
        } else {
            RECOVERY_VERIFY_WINDOW
        }
        + LOOK_WORST;
    if !budget_allows(key_and_verify) {
        return out_of_budget(key_and_verify);
    }
    if let Err(err) = send_key(key) {
        return SupervisorResult::RecoveryUnconfirmed {
            matched,
            reason: format!("{key} could not be sent: {err:#}"),
        };
    }

    // Exactly one Escape, then verify. No second key of any kind: a view
    // that ignores Escape (measured: `/keybindings` spawns an editor, where
    // Escape is a mode key) needs a human, and guessing further keys into
    // an unknown state is how a rescue becomes damage.
    // `Escape` dismisses a view that is already drawn; `Enter` commits a
    // choice and Claude Code then redraws the transcript around it. The 250ms
    // window was measured for the first and says nothing about the second, so
    // the confirming path gets its own.
    let window = if confirming {
        CONFIRM_VERIFY_WINDOW
    } else {
        RECOVERY_VERIFY_WINDOW
    };
    let deadline = std::time::Instant::now() + window;
    let mut saw_the_pane = false;
    let mut last_failed_look: Option<anyhow::Error>;
    loop {
        match look() {
            Ok((pane, keyboard)) => {
                saw_the_pane = true;
                last_failed_look = None;
                if ready(&pane, keyboard) {
                    if confirming {
                        return SupervisorResult::ViewConfirmed { matched };
                    }
                    return SupervisorResult::ComposerRecovered {
                        matched,
                        pane_snapshot: capture.then_some(absent_pane),
                        captured_at: protocol::time::now_rfc3339(),
                    };
                }
            }
            Err(err) => last_failed_look = Some(err),
        }
        if std::time::Instant::now() >= deadline {
            // `ComposerLost` is a claim — a key went out and the composer did
            // not come back — so it is only made when the looking that would
            // have seen it actually worked.
            //
            // **Never on the confirming path.** There the key was `Enter`, so
            // the change has most likely already been made; calling that a lost
            // composer would report a successful switch as a hard failure, and
            // the sheet would go terminal on it. The window not being long
            // enough to *see* the redraw is exactly the "typed, outcome
            // unknown" state, and the transcript receipt still settles it.
            // A window that ended on a *failed* look proves nothing about
            // the composer — `ComposerLost` is a claim, and it is only made
            // when the last observation actually worked. The failure's own
            // reason travels, so a tmux killed at its deadline is named.
            return match (&last_failed_look, confirming, saw_the_pane) {
                (Some(err), _, _) => unconfirmed_because("while verifying after the key", err),
                (None, true, _) => {
                    unconfirmed("while waiting for the composer after the confirmation")
                }
                (None, false, true) => SupervisorResult::ComposerLost { matched },
                (None, false, false) => unconfirmed("after the key was sent"),
            };
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// Why the composer's readiness is asked of tmux and not of the pane.
//
// **Measured, and the reason this note exists.** Submitting `/status` while a
// turn is running opens the Settings view *inline*: when the turn finishes
// Claude redraws the transcript, the view, and — below it — the composer box
// and its footer. The composer-presence check matches. The composer takes no
// keys: typed text never appears and Enter does nothing. A send authorised on
// that pane types into the void and reports success, and a recovery check that
// believed the box would see a healthy composer and decline the Escape that
// fixes it. That is the lockout this feature exists to prevent, wearing the
// disguise of a working screen.
//
// The pane's *text* cannot settle it. The obvious string — the view's own
// `Esc to cancel` — is a string an agent can write into its own output, and
// then every send is refused and a live turn gets an Escape it never earned.
// The cursor can't be spelled: tmux tracks the terminal's DECTCEM state per
// pane, Claude's composer keeps it visible while idle, while a turn streams
// and after it ends (327 consecutive samples through a live turn, none
// hidden), and every view it opens hides it — including the disguised one.
//
// A visible cursor is not the whole answer either. A pane in one of tmux's own
// modes routes every keystroke to that mode instead of to Claude, and does it
// with the cursor still showing — so the wheel over the inline transcript,
// which is what `render_server_conf`'s `mouse on` is for, produces a screen
// where the composer is drawn, the cursor is up, `send-keys` exits 0 and the
// text is never delivered. Both facts come back from one `display`; see
// [`tmux::who_has_the_keyboard`].

/// May this send leave the pane's copy-mode by itself, rather than asking the
/// person at the Mac to?
///
/// **Yes, for a scroll position.** A prompt arriving from the phone *is* the
/// intent to type, there is no second reader whose place in the transcript
/// this would take, and a scroll position is a view of history rather than a
/// question: leaving it loses nothing that was not still in the pane's
/// history a moment later. Refusing it made the wheel — which
/// [`tmux::render_server_conf`]'s `mouse on` puts under every scroll at the
/// Mac — into something that silently disabled the phone until somebody
/// walked back to the keyboard.
///
/// **No, for anything else tmux is holding.** A clock-mode, a `choose-tree`,
/// a `customize-mode` — and a copy-mode with any of them underneath — are
/// things the person at the Mac opened and is looking at. Popping one is
/// taking their screen away, and typing into what is underneath answers a
/// question they never saw: the same reason [`tmux::Keyboard::View`] is
/// refused rather than dismissed. Those keep the refusal they have always had.
///
/// **No, unless these keys are for the composer.** This is the one place the
/// keyboard is asked about at all — a permission prompt is answered through a
/// view, where the question is which prompt is on screen, not who holds the
/// keys.
///
/// **No, unless the composer is already on the screen.** The capture is one
/// the caller has already paid for, and it says the same thing either side of
/// the exit: measured on tmux 3.7b, at every scroll depth, and against a real
/// claude pane, `capture-pane` reports the **live** screen while a pane is
/// scrolled back — byte-identical to the capture taken afterwards. So this can
/// never refuse a send that the look after the exit would have authorised, and
/// it keeps the daemon from clearing somebody's scroll position for a send
/// that was never going to be typed: a view drawn under the mode, or a pane
/// that is not Claude's at all. That last one is not hypothetical — every tmux
/// call here addresses the session's *current* pane, so a second window the
/// person at the Mac is reading `git log` in is the pane the daemon reads, and
/// the one it would otherwise scroll to the bottom for them.
///
/// **What it costs when the answer is yes.** The scroll position, and — since
/// nothing tmux reports separates these from a parked pane — a selection being
/// dragged, the copy-mode mark, and a search being typed. Measured: buffers
/// already copied survive, the transcript survives, and `#{pane_in_mode}` and
/// `#{pane_mode}` read identically for every one of them.
fn may_leave_copy_mode(target: &Target<'_>, pane: &str, keyboard: tmux::Keyboard) -> bool {
    target.is_composer
        && matches!(
            keyboard,
            tmux::Keyboard::Scrollback(tmux::TmuxMode::CopyMode)
        )
        && target.presence.find_match(pane, None).is_some()
}

/// The look the gates are then run against: the first one, or — where a bare
/// scroll position was left — the one taken afterwards.
///
/// Over injectable steps for the same reason [`actuate_with`] is: the
/// *ordering* is the safety property here, and the real steps address the
/// shared tmux server, so nothing could drive this end to end without one.
/// What a test can pin instead is every claim the ordering makes.
///
///   * the exit runs only when [`may_leave_copy_mode`] and the clock both say
///     so, and it runs at most once — this never fights the person at the Mac
///     for the pane;
///   * the second look replaces **both** halves, so no fact survives the exit
///     that was read before it;
///   * a second look that fails is a refusal, exactly as the first one is.
fn look_the_gates_run_on(
    target: &Target<'_>,
    first: (String, tmux::Keyboard),
    affordable: bool,
    leave: &dyn Fn(),
    look_again: &dyn Fn() -> Result<(String, tmux::Keyboard)>,
) -> Result<(String, tmux::Keyboard)> {
    let (pane, keyboard) = first;
    if !affordable || !may_leave_copy_mode(target, &pane, keyboard) {
        return Ok((pane, keyboard));
    }
    leave();
    look_again()
}

/// The interlock as a decision over one pane. Returns the needle that
/// authorised the send, or the reason nothing may be typed.
///
/// Split out from the injection so it can be exercised against real captured
/// panes: everything it depends on is text, and it is the single point where
/// "may these keys be sent?" is decided.
fn authorise(
    pane: &str,
    require: &PromptPresence,
    expect: Option<&PromptFingerprint>,
    // Whether `require` names the composer. Carried rather than inferred from
    // the enum: a configured needle override arrives as `AnyOf`, and reading
    // "this is the composer" off the variant silently disabled the keyboard
    // check for exactly the operator who had already had to fix something.
    targets_composer: bool,
    keyboard: tmux::Keyboard,
) -> std::result::Result<String, String> {
    let Some(matched) = require.find_match(pane, None) else {
        return Err(format!(
            "expected prompt not on screen (looked for {}); nothing was typed",
            require.requirement()
        ));
    };
    // Only the composer. A permission prompt is a view too — it hides the
    // cursor exactly as the others do — and answering one is the whole point
    // of this path, so the keyboard question is asked only where a *drawn*
    // target can turn out to be a dead one.
    //
    // The two ways of losing it are told apart because they need different
    // things: a view is dismissed, and a mode is left.
    if targets_composer {
        match keyboard {
            tmux::Keyboard::Program => {}
            tmux::Keyboard::View => {
                return Err(
                    "a view on the Mac has the keyboard; the composer is drawn but takes \
                     no keys, so nothing was typed"
                        .into(),
                );
            }
            // Whatever tmux is still holding at this point is the person at
            // the Mac's to let go of. A bare scroll position may already have
            // been left by [`may_leave_copy_mode`] — but this is also where
            // one arrives that was never eligible for that, that the clock
            // left no room for, that did not clear, or that was re-entered
            // while this was being decided.
            //
            // The wording names the mode and not the scroll, because most of
            // what reaches here is a clock face, a `choose-tree` or a
            // `customize-mode`, and none of those is scrolled anywhere; and
            // `q` leaves one mode per press, so a stack takes one each.
            tmux::Keyboard::Scrollback(_) => {
                return Err(
                    "tmux has the keyboard on the Mac: the pane is in one of tmux's own \
                     modes, so the composer takes none — press q there until it clears — \
                     and nothing was typed"
                        .into(),
                );
            }
        }
    }
    if let Some(expect) = expect {
        if !expect.still_on_screen(pane) {
            return Err(
                "the prompt on screen is not the one this answer was created for; \
                        nothing was typed"
                    .into(),
            );
        }
    }
    Ok(matched)
}

/// Tell the daemon this run has ended.
///
/// A *fresh* connection, opened after the session is gone, so it has to name the
/// run explicitly: by the time this is sent the tmux name may already belong to
/// a session somebody started in the meantime, and marking that one exited would
/// be worse than saying nothing.
/// Tell the daemon this run has ended — introducing it first if necessary.
///
/// **The registration replay is the fix, not a nicety.** A session that starts
/// *and* ends while `ccd` is down has never been registered, so the daemon has
/// no row for it; `session_exited` then logs "exit reported for unknown session"
/// and drops the frame. The run vanishes: it never appears in the fleet, it has
/// no event log, and there is nothing anywhere saying an agent ran at all. The
/// event log's whole claim is that a kill loses nothing, and this was a case
/// where it lost an entire session.
///
/// So the registration this supervisor has been holding since startup is sent
/// on the same connection, immediately before the exit. The daemon upserts the
/// session, assigns it a uid if it never had one, and then has something to
/// record the `SessionEnd` against. Ordering matters and is guaranteed: both
/// frames go down one stream and the daemon's read loop handles them in order.
///
/// Idempotent by construction. A session that *was* registered normally is
/// simply re-registered under the same uid, which is the same path a supervisor
/// reconnecting after a daemon restart already takes.
fn report_exit(socket: &std::path::Path, registration: &RegisterSession) -> Result<()> {
    let stream = UnixStream::connect(socket)?;
    let writer = Arc::new(Mutex::new(stream));
    // Best-effort: a daemon that rejects the introduction may still accept the
    // exit for a session it already knows about, so a failure here must not
    // stop the news getting through.
    // **Marked as the replay it is.** The frame is otherwise byte-identical to the
    // one this supervisor sent when it was alive, which is what the daemon needs
    // told: a registration it cannot tell from a live one is a registration it hands
    // the session to, and this process is already dead. Saying so is what stops the
    // replay taking the session away from whatever supervisor has resumed it in the
    // meantime — and, one frame later, ending that supervisor's run instead of this
    // one. See `RegisterSession::exit_replay`.
    let replay = RegisterSession {
        exit_replay: true,
        ..registration.clone()
    };
    if let Err(err) = send(&writer, &ClientFrame::Register(replay)) {
        log_for(
            &registration.session_id,
            registration.session_uid.as_deref(),
            &format!("could not replay the registration before reporting exit: {err:#}"),
        );
    }
    send(
        &writer,
        &ClientFrame::SessionExited {
            session_id: registration.session_id.clone(),
            session_uid: registration.session_uid.clone(),
            exit_code: None,
        },
    )
}

/// What this supervisor tells the daemon about its run.
fn registration_frame(args: &SupervisorArgs, started_at: &str) -> RegisterSession {
    RegisterSession {
        session_id: args.session_id.clone(),
        session_uid: args.session_uid.clone(),
        tmux_session: args.tmux_session.clone(),
        tmux_socket: protocol::TMUX_SOCKET_NAME.to_string(),
        cwd: args.cwd.clone(),
        supervisor_pid: std::process::id(),
        claude_bin: args.claude_bin.clone(),
        // The agent this supervisor hosts. Phase 1 only ever launches Claude, so
        // this is Claude, and `agent_bin` mirrors `claude_bin`; the fields are
        // present so a daemon on the agent seam records the run's agent from the
        // registration rather than inferring it. A Codex supervisor sets these
        // when its launch path lands.
        agent: protocol::agent::AgentKind::Claude,
        agent_bin: args.claude_bin.clone(),
        codex_thread_id: None,
        codex_socket: None,
        codex_generation: None,
        started_at: started_at.to_string(),
        // What this build can honour. The daemon uses it to decide whether an
        // approval may be actuated through us at all: a supervisor that
        // silently ignores the prompt fingerprint must not be handed one.
        protocol_minor: protocol::PROTOCOL_MINOR,
        exit_replay: false,
    }
}

fn send(writer: &Arc<Mutex<UnixStream>>, frame: &ClientFrame) -> Result<()> {
    let mut line = serde_json::to_vec(frame)?;
    line.push(b'\n');
    let mut guard = writer.lock().unwrap_or_else(|p| p.into_inner());
    guard.write_all(&line).context("writing to ccd")?;
    guard.flush().context("flushing to ccd")
}

/// The supervisor has no terminal: diagnostics go to a per-run log file.
///
/// Per *run*, not per name: `cc-1` is reused, and two sessions interleaving
/// their reconnect messages in one file makes it useless at exactly the moment
/// somebody is reading it to find out why a session lost its link. A supervisor
/// with no uid (an older build) keeps the old flat name.
fn log_line(args: &SupervisorArgs, message: &str) {
    log_for(&args.session_id, args.session_uid.as_deref(), message);
}

/// The same, addressed by identity rather than by the whole argument struct, so
/// a worker thread that only carries the session's name can still write to it.
fn log_for(session_id: &str, session_uid: Option<&str>, message: &str) {
    let dir = protocol::logs_dir();
    // Owner-only: a supervisor log carries the session's name, its working
    // directory and whatever a refusal reason quoted off the pane.
    if protocol::fsperm::private_dir(&dir).is_err() {
        return;
    }
    let name = match session_uid {
        Some(uid) => format!("supervisor-{session_id}-{uid}.log"),
        None => format!("supervisor-{session_id}.log"),
    };
    let path = dir.join(name);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(protocol::fsperm::FILE_MODE)
        .open(&path)
    {
        let _ = writeln!(file, "{} {message}", protocol::time::now_rfc3339());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::{Keyboard, TmuxMode};
    use protocol::ipc::prompt_fingerprint;
    use std::os::unix::net::UnixListener;
    use std::process::Command;

    /// A uid-less or malformed-uid session keeps the exact name-addressed
    /// liveness check it always had (Claude's legacy/adopted path is
    /// byte-for-byte unchanged). The uid-atomic path's *mechanism* — a reused
    /// `cc-N` name reading `Gone` for the old uid, and an epoch change refusing —
    /// is proven against real tmux in `protocol::tmux`'s `owned_liveness` tests;
    /// here we only pin that the fallback is chosen when no well-formed uid is
    /// present, using a name that resolves to `Gone` on any server.
    #[test]
    fn probe_liveness_falls_back_to_the_name_check_without_a_wellformed_uid() {
        let ghost = "cc-nonexistent-supervisor-probe";
        // No uid: name-addressed check. On a machine with no such session this
        // is Gone (or Unknown if tmux is unreachable) — never a panic, and
        // identical to calling `session_presence` directly.
        let via_helper = probe_liveness(ghost, None);
        let via_name = tmux::session_presence(ghost);
        assert_eq!(
            std::mem::discriminant(&via_helper),
            std::mem::discriminant(&via_name),
            "uid-less probe must equal the plain name check"
        );
        // A malformed uid is not a routing tag, so it also falls back.
        let via_bad_uid = probe_liveness(ghost, Some("not-a-ulid"));
        assert_eq!(
            std::mem::discriminant(&via_bad_uid),
            std::mem::discriminant(&via_name),
            "a malformed uid falls back to the name check"
        );
    }

    // ------------------------------------------------- actuation phases
    //
    // The injection's two keystrokes over injectable steps: what a failure
    // *means* depends on where it happened, and these pin the mapping the
    // phone's claim handling relies on. A send-keys killed at its deadline
    // may or may not have typed — treating it as a refusal would let the
    // retry type twice.

    fn timed_out() -> tmux::TmuxError {
        tmux::TmuxError::TimedOut {
            what: "send-keys".into(),
            waited: Duration::from_secs(1),
        }
    }

    fn refused_by_tmux() -> tmux::TmuxError {
        tmux::TmuxError::Spawn {
            what: "send-keys".into(),
            source: std::io::Error::other("no such file"),
        }
    }

    #[test]
    fn a_typing_timeout_is_indeterminate_and_enter_is_never_pressed() {
        let entered = std::cell::Cell::new(false);
        let failure = actuate_with(
            &|| Err(timed_out()),
            &|| {
                entered.set(true);
                Ok(())
            },
            true,
            Duration::ZERO,
        )
        .expect_err("the typing failure must surface");
        assert!(
            !entered.get(),
            "Enter after an unconfirmed typing is a second mutation into an unknown screen"
        );
        match failure.result() {
            SupervisorResult::Error { message } => {
                assert!(
                    message.contains("will not be sent again"),
                    "the phone must be told a retry will not type: {message}"
                );
            }
            other => panic!("a deadline kill may have typed; got {other:?}"),
        }
    }

    #[test]
    fn a_provable_typing_failure_is_a_refusal_a_retry_may_act_on() {
        let failure = actuate_with(&|| Err(refused_by_tmux()), &|| Ok(()), true, Duration::ZERO)
            .expect_err("the spawn failure must surface");
        match failure.result() {
            SupervisorResult::Refused { reason } => {
                assert!(reason.contains("nothing was typed"), "{reason}");
            }
            other => panic!("nothing ran, so nothing was typed; got {other:?}"),
        }
    }

    #[test]
    fn any_enter_failure_after_typing_is_indeterminate() {
        // Even a *provable* Enter failure: the text is already staged, and a
        // released claim would let a retry stage it twice.
        for make_failure in [timed_out as fn() -> tmux::TmuxError, refused_by_tmux] {
            let typed = std::cell::Cell::new(0u32);
            let failure = actuate_with(
                &|| {
                    typed.set(typed.get() + 1);
                    Ok(())
                },
                &|| Err(make_failure()),
                true,
                Duration::ZERO,
            )
            .expect_err("the Enter failure must surface");
            assert_eq!(typed.get(), 1, "the text was typed exactly once");
            match failure.result() {
                SupervisorResult::Error { message } => {
                    assert!(message.contains("the text was typed"), "{message}");
                    assert!(message.contains("will not be sent again"), "{message}");
                }
                other => panic!("staged text must never map to Refused; got {other:?}"),
            }
        }
    }

    /// Recovery under a spent budget: it must stop before its first
    /// deliberate wait — no look taken, no key sent — and say that the keys
    /// were typed, because they were.
    #[test]
    fn recovery_with_no_budget_left_stops_before_spending_anything() {
        let looked = std::cell::Cell::new(false);
        let keyed = std::cell::Cell::new(false);
        let outcome = recover_composer_with(
            RecoveryPass {
                answer_by: std::time::Instant::now(),
                asking: None,
                matched: "composer".into(),
                capture: false,
                confirm: None,
            },
            &|| {
                looked.set(true);
                Ok((String::new(), Keyboard::Program))
            },
            &|_| {
                keyed.set(true);
                Ok(())
            },
            &PromptPresence::InputBox,
        );
        assert!(!looked.get(), "an unaffordable look must not be taken");
        assert!(!keyed.get(), "and certainly no key sent");
        match outcome {
            SupervisorResult::RecoveryUnconfirmed { reason, .. } => {
                assert!(
                    reason.contains("the keys were typed"),
                    "the phone must still learn the typing happened: {reason}"
                );
                assert!(reason.contains("recovery stopped"), "{reason}");
            }
            other => panic!("a spent budget is indeterminate, not {other:?}"),
        }
    }

    /// A generous budget changes nothing: the first look sees a ready
    /// composer and recovery concludes `Sent` exactly as before.
    #[test]
    fn recovery_with_budget_behaves_as_before() {
        let outcome = recover_composer_with(
            RecoveryPass {
                answer_by: std::time::Instant::now() + Duration::from_secs(60),
                asking: None,
                matched: "composer".into(),
                capture: false,
                confirm: None,
            },
            &|| Ok(("composer > for shortcuts".into(), Keyboard::Program)),
            &|_| Ok(()),
            &PromptPresence::AnyOf {
                needles: vec!["for shortcuts".into()],
            },
        );
        assert!(
            matches!(outcome, SupervisorResult::Sent { .. }),
            "{outcome:?}"
        );
    }

    /// A request whose daemon has already stopped listening is refused
    /// before anything is looked at: the stamp is hours in the past, so a
    /// pane read — up to two seconds of bounded tmux — would be spent on an
    /// answer nobody will hear. The refusal must be immediate, which is
    /// also what proves no look ran: a real look at even an absent session
    /// costs a process spawn this assertion's bound does not allow.
    #[test]
    fn an_expired_request_is_refused_before_any_look() {
        use std::io::{BufRead, Write};
        let (daemon_side, supervisor_side) = UnixStream::pair().expect("socketpair");
        let args = SupervisorArgs {
            session_id: format!("cc-expired-{}", std::process::id()),
            session_uid: None,
            tmux_session: format!("cc-expired-{}", std::process::id()),
            cwd: "/tmp".into(),
            claude_bin: None,
        };
        let config = Config::load();
        let reader = BufReader::new(supervisor_side.try_clone().expect("clone"));
        let writer = Arc::new(Mutex::new(supervisor_side));
        let loop_thread = std::thread::spawn(move || {
            let _ = read_frames(reader, &args, &config, &writer);
        });

        let mut ask = daemon_side.try_clone().expect("clone");
        daemon_side
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let mut answers = std::io::BufReader::new(daemon_side);

        let expired = DaemonFrame::SupervisorRequest {
            id: "late".into(),
            request: SupervisorRequest::SendText {
                text: "too late".into(),
                require: PromptPresence::InputBox,
                asking: None,
                targets_composer: true,
                submit: true,
                expect: None,
                recover_composer: false,
                capture_recovered: false,
                confirm_view: None,
                // A monotonic stamp of 1ms: hours behind any live reading.
                respond_by_monotonic_ms: Some(1),
            },
        };
        let mut line = serde_json::to_vec(&expired).unwrap();
        line.push(b'\n');
        let asked = std::time::Instant::now();
        ask.write_all(&line).unwrap();

        let mut answer = String::new();
        answers.read_line(&mut answer).expect("the refusal arrives");
        let waited = asked.elapsed();
        match serde_json::from_str::<ClientFrame>(&answer).unwrap() {
            ClientFrame::SupervisorResponse { id, result } => {
                assert_eq!(id, "late");
                match result {
                    SupervisorResult::Refused { reason } => {
                        assert!(reason.contains("nothing was typed"), "{reason}");
                        assert!(
                            reason.contains("stops listening"),
                            "the refusal names the budget: {reason}"
                        );
                    }
                    other => panic!("an expired request refuses; got {other:?}"),
                }
            }
            other => panic!("wrong frame: {other:?}"),
        }
        assert!(
            waited < Duration::from_millis(300),
            "the refusal was immediate — no pane was read: {waited:?}"
        );
        drop(ask);
        drop(answers);
        loop_thread.join().expect("the loop ends");
    }

    /// The dispatch loop outlives a request that failed: a send aimed at a
    /// session that does not exist is refused — quickly, through the real
    /// bounded pane look — and the very next request on the same stream is
    /// answered. This is the loop the wedge silenced, proven live over a
    /// socket pair.
    #[test]
    fn the_dispatch_loop_answers_the_next_request_after_a_failed_send() {
        use std::io::{BufRead, Write};
        if tmux::tmux_bin().is_err() {
            eprintln!("skipped: no tmux on this machine");
            return;
        }
        let (daemon_side, supervisor_side) = UnixStream::pair().expect("socketpair");
        let args = SupervisorArgs {
            session_id: format!("cc-none-{}", std::process::id()),
            session_uid: None,
            tmux_session: format!("cc-none-{}", std::process::id()),
            cwd: "/tmp".into(),
            claude_bin: None,
        };
        let config = Config::load();
        let reader = BufReader::new(supervisor_side.try_clone().expect("clone"));
        let writer = Arc::new(Mutex::new(supervisor_side));
        let loop_thread = std::thread::spawn(move || {
            let _ = read_frames(reader, &args, &config, &writer);
        });

        let mut ask = daemon_side.try_clone().expect("clone");
        daemon_side
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("read timeout");
        let mut answers = std::io::BufReader::new(daemon_side);

        let send = DaemonFrame::SupervisorRequest {
            id: "one".into(),
            request: SupervisorRequest::SendText {
                text: "never lands".into(),
                require: PromptPresence::InputBox,
                asking: None,
                targets_composer: true,
                submit: true,
                expect: None,
                recover_composer: false,
                capture_recovered: false,
                confirm_view: None,
                respond_by_monotonic_ms: None,
            },
        };
        let ping = DaemonFrame::SupervisorRequest {
            id: "two".into(),
            request: SupervisorRequest::Ping,
        };
        for frame in [&send, &ping] {
            let mut line = serde_json::to_vec(frame).unwrap();
            line.push(b'\n');
            ask.write_all(&line).unwrap();
        }

        let mut line = String::new();
        answers
            .read_line(&mut line)
            .expect("the failed send answers");
        let first: ClientFrame = serde_json::from_str(&line).unwrap();
        match first {
            ClientFrame::SupervisorResponse { id, result } => {
                assert_eq!(id, "one");
                assert!(
                    matches!(result, SupervisorResult::Refused { .. }),
                    "an absent session refuses the look: {result:?}"
                );
            }
            other => panic!("wrong first frame: {other:?}"),
        }
        line.clear();
        answers
            .read_line(&mut line)
            .expect("the next request answers");
        let second: ClientFrame = serde_json::from_str(&line).unwrap();
        match second {
            ClientFrame::SupervisorResponse { id, result } => {
                assert_eq!(id, "two");
                assert!(matches!(result, SupervisorResult::Pong), "{result:?}");
            }
            other => panic!("wrong second frame: {other:?}"),
        }
        drop(ask);
        drop(answers);
        loop_thread
            .join()
            .expect("the loop ends when the daemon hangs up");
    }

    #[test]
    fn a_send_without_submit_never_touches_enter() {
        let entered = std::cell::Cell::new(false);
        actuate_with(
            &|| Ok(()),
            &|| {
                entered.set(true);
                Ok(())
            },
            false,
            Duration::ZERO,
        )
        .expect("typing succeeded");
        assert!(!entered.get());
    }

    /// The shape `tmux capture-pane -p -J` returns for a live permission
    /// prompt on claude 2.1.232, modelled on
    /// `fixtures/panes/no-composer/permission-prompt.txt` and parameterised by
    /// the command, because the prompt interlock has to tell two of them
    /// apart.
    fn permission_pane(command: &str) -> String {
        format!(
            "─────────────────────────────────────────\n \
             Bash command\n\n   {command}\n   Create empty file\n\n \
             Permission rule Bash requires confirmation for this command.\n \
             /permissions to update rules\n\n \
             Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n \
             Esc to cancel · Tab to amend · ctrl+e to explain"
        )
    }

    /// A composer with nothing typed into it, manual mode, one subagent.
    const IDLE_PANE: &str = include_str!("../../../fixtures/panes/composer/manual-shortcuts.txt");

    /// The measured lockout in disguise: `/status` submitted during a turn,
    /// captured after the turn finished. Claude redraws the transcript, the
    /// Settings view, and — below it — a composer box and footer that stand
    /// there taking no keys.
    ///
    /// Hand-modelled rather than captured: the state lasts exactly as long as
    /// the view is up, and the pane was read after it closed. Its composer is
    /// drawn the way Claude draws one, so presence says yes and only the
    /// cursor says what is true.
    const VIEW_ABOVE_COMPOSER_PANE: &str = "\
⏺ line 1
✻ Sautéed for 6s
────────────────────────────────────────
  Settings  Status   Config   Usage   Stats
  Version:          2.1.232
  Session kind:     interactive
  Esc to cancel
────────────────────────────────────────
❯
────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← 1 agent";

    /// A real capture with no composer drawn on it at all — the state where
    /// leaving a scroll position would buy nothing, because there would be
    /// nothing underneath to type into.
    const NO_COMPOSER_PANE: &str =
        include_str!("../../../fixtures/panes/no-composer/status-view.txt");

    /// A live composer during a turn — the state the rule must NOT catch.
    /// Its footer offers `esc to interrupt`, which is a different offer:
    /// the composer is taking keys and queues them for the next turn.
    const MID_TURN_PANE: &str = include_str!("../../../fixtures/panes/composer/mid-turn.txt");

    /// A transcript line in [`MID_TURN_PANE`], for the tests that rewrite what
    /// the agent wrote.
    const MID_TURN_TRANSCRIPT_LINE: &str = "⏺ Bash(touch /private/tmp/ccprobe_x.txt)";

    // ------------------------------------------------- live tmux recovery

    /// A fake TUI with the two behaviours that matter: it draws the
    /// composer's box, and a line beginning `/dialog` takes the box away
    /// until Escape arrives (`/stuck` never releases it). Real tmux, real
    /// keys, real captures — the supervisor's own code path — on a server
    /// this test creates and destroys itself.
    struct FakeTui {
        socket_dir: std::path::PathBuf,
        session: String,
        /// Recorded while the server is healthy, for the teardown that runs
        /// when it no longer is: a wedged server cannot be asked its pid.
        server_pid: std::cell::Cell<Option<i32>>,
    }

    impl FakeTui {
        fn start(name: &str, script_body: &str) -> Option<FakeTui> {
            let tmux = protocol::tmux::tmux_bin()?;
            let root = std::env::temp_dir().join(format!(
                "cc-faketui-{}-{}-{}",
                std::process::id(),
                name,
                protocol::time::now_unix_ms()
            ));
            std::fs::create_dir_all(&root).ok()?;
            let script = root.join("tui.sh");
            std::fs::write(&script, script_body).ok()?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).ok()?;

            let session = format!("faketui-{name}");
            // The guard exists BEFORE the spawn: a server created by a call
            // whose status was never seen — a hang, a panic between spawn
            // and construction — must still die with the test.
            let tui = FakeTui {
                socket_dir: root,
                session,
                server_pid: std::cell::Cell::new(None),
            };
            // Plain `new-session`, not `-P -F '#{pid}'`: `-P` writes the pid
            // to the piped client stdout the server then retains, so the
            // bounded runner would wait for an EOF that never comes and time
            // the creation out. The pid is read by a separate `display`, and
            // read before the success check so a server that started under a
            // timed-out create is still owned by the guard.
            let started = protocol::proc::run_deadlined(
                Command::new(&tmux)
                    .args(["-S", &tui.socket()])
                    .args([
                        "new-session",
                        "-d",
                        "-s",
                        &tui.session,
                        "-x",
                        "80",
                        "-y",
                        "24",
                    ])
                    .arg("--")
                    .arg(&script),
                Duration::from_secs(5),
            )
            .ok()?;
            if let Some(pid) = tui
                .tmux(&["display", "-p", "#{pid}"])
                .and_then(|out| out.trim().parse::<i32>().ok())
            {
                tui.server_pid.set(Some(pid));
            }
            match started {
                protocol::proc::RunOutcome::Completed { status, .. } if status.success() => {}
                _ => return None,
            }
            // Wait for the composer rather than sleeping a guessed interval.
            // A fixed 700ms held while these tests ran one at a time and
            // stopped holding once six tmux servers started at once — and a
            // harness that has not finished drawing makes its test fail for
            // a reason the test is not about.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if tui.pane().contains("shortcuts") {
                    return Some(tui);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            None
        }

        fn socket(&self) -> String {
            self.socket_dir.join("sock").to_string_lossy().into_owned()
        }

        fn tmux(&self, args: &[&str]) -> Option<String> {
            let mut command = Command::new(protocol::tmux::tmux_bin()?);
            command.args(["-S", &self.socket()]).args(args);
            match protocol::proc::run_deadlined(&mut command, Duration::from_secs(2)).ok()? {
                protocol::proc::RunOutcome::Completed { stdout, .. } => {
                    Some(String::from_utf8_lossy(&stdout).into_owned())
                }
                protocol::proc::RunOutcome::TimedOut { .. } => None,
            }
        }

        /// `-J` as well as `-p`, because that is what
        /// [`tmux::capture_visible_pane`] asks for: without it a wrapped
        /// composer row arrives split and these tests would be authorising
        /// text the daemon never sees.
        fn pane(&self) -> String {
            self.tmux(&["capture-pane", "-p", "-J", "-t", &self.session])
                .unwrap_or_default()
        }

        fn type_line(&self, text: &str) {
            let _ = self.tmux(&["send-keys", "-t", &self.session, "-l", "--", text]);
            let _ = self.tmux(&["send-keys", "-t", &self.session, "Enter"]);
        }

        /// The same question, and the same parse, as
        /// [`tmux::who_has_the_keyboard`] puts to the shared server — which
        /// this cannot call, because it addresses `-L codeconnect` and this
        /// server is the test's own socket. Only the *addressing* differs, so
        /// only the addressing is written twice: a second reading of the reply
        /// here would be a copy free to drift away from the one that ships.
        /// An unreadable answer fails the test here rather than being turned
        /// into a state. The daemon's own answer to one is a refusal, so the
        /// tempting fallbacks are both wrong: `Program` is the most permissive
        /// state there is and would let a broken format read as a healthy
        /// keyboard, and `View` would pass a refusal off as the thing under
        /// test. This is a test harness, and a tmux that cannot answer is a
        /// test that cannot make its claim.
        fn keyboard(&self) -> tmux::Keyboard {
            let out = self
                .tmux(&["display", "-p", "-t", &self.session, tmux::KEYBOARD_FORMAT])
                .expect("this test's own tmux answered nothing about the keyboard");
            tmux::read_keyboard(&out)
                .unwrap_or_else(|err| panic!("tmux answered {out:?}, which is not readable: {err}"))
        }

        /// tmux's own name for the mode on top of this pane's stack.
        fn mode(&self) -> String {
            self.tmux(&["display", "-p", "-t", &self.session, "#{pane_mode}"])
                .unwrap_or_default()
                .trim()
                .to_string()
        }

        /// Put the pane into one of tmux's modes that is *not* copy-mode, the
        /// way the person at the Mac would: a real tmux command against a real
        /// pane, so what the daemon reads back is what tmux really says.
        fn enter_mode(&self, command: &str) {
            let _ = self.tmux(&[command, "-t", &self.session]);
        }

        /// tmux's own count of the modes stacked on this pane.
        fn modes(&self) -> String {
            self.tmux(&["display", "-p", "-t", &self.session, "#{pane_in_mode}"])
                .unwrap_or_default()
                .trim()
                .to_string()
        }

        fn enter_scrollback(&self) {
            let _ = self.tmux(&["copy-mode", "-t", &self.session]);
        }

        /// Put the pane back in *no* mode, whatever depth it is at: the reset
        /// a test wants between arrangements, not the act under test.
        /// `copy-mode -q` because it is idempotent and flattens the whole
        /// stack in one call — which is exactly why the daemon does not use
        /// it, and why [`leave_copy_mode`] below exists beside it.
        fn leave_scrollback(&self) {
            let _ = self.tmux(&["copy-mode", "-q", "-t", &self.session]);
        }

        /// Leave one copy-mode by running the command the daemon runs. The
        /// argv comes from [`tmux::leave_copy_mode_args`] rather than being
        /// written out again, so what these tests measure is what ships.
        fn leave_copy_mode(&self) {
            let _ = self.tmux(&tmux::leave_copy_mode_args(&self.session));
        }

        fn send_escape(&self) {
            let _ = self.tmux(&["send-keys", "-t", &self.session, "Escape"]);
        }

        /// Wait for the harness to finish drawing. Its redraw is a shell
        /// script's round trip, so it is not on the caller's thread.
        fn wait_for(&self, needle: &str) -> bool {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if self.pane().contains(needle) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            false
        }
    }

    impl Drop for FakeTui {
        fn drop(&mut self) {
            // Only ever this test's own socket — never the shared server.
            // Bounded, because an unbounded `kill-server` against a wedged
            // server would recreate inside the cleanup the very hang some of
            // these tests exist to prevent. If asking nicely fails, the
            // exact server pid is killed; the directory goes only once the
            // server cannot still be alive to be stranded by it.
            // Only a zero exit from `kill-server` is "the server obeyed":
            // a nonzero completion, a timeout, a spawn failure, or tmux not
            // being locatable at all each leave a server that may still be
            // alive — and the pid recorded while it was healthy is the
            // answer to every one of them.
            let obeyed = protocol::tmux::tmux_bin().is_some_and(|tmux| {
                match protocol::proc::run_deadlined(
                    Command::new(&tmux)
                        .args(["-S", &self.socket()])
                        .arg("kill-server"),
                    Duration::from_secs(2),
                ) {
                    Ok(protocol::proc::RunOutcome::Completed { status, stderr, .. }) => {
                        // "no server" is as dead as a successful kill.
                        status.success() || String::from_utf8_lossy(&stderr).contains("no server")
                    }
                    _ => false,
                }
            });
            let dead = obeyed
                || match self.server_pid.get() {
                    Some(pid) => {
                        // A sent signal is a request, not a fact: death is
                        // verified by `kill(pid, 0)` answering `ESRCH`,
                        // boundedly, before the socket directory may go.
                        unsafe {
                            libc::kill(pid, libc::SIGCONT);
                            libc::kill(pid, libc::SIGKILL);
                        }
                        (0..50).any(|_| {
                            let gone = unsafe { libc::kill(pid, 0) } == -1
                                && std::io::Error::last_os_error().raw_os_error()
                                    == Some(libc::ESRCH);
                            if !gone {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            gone
                        })
                    }
                    None => false,
                };
            // Deleting a live server's socket strands it unaddressable; a
            // leftover directory is the lesser harm.
            if dead {
                let _ = std::fs::remove_dir_all(&self.socket_dir);
            } else {
                eprintln!(
                    "fake TUI server at {} could not be proven dead; leaving its directory",
                    self.socket_dir.display()
                );
            }
        }
    }

    const FAKE_TUI: &str = r#"#!/bin/sh
# A composer that a "/dialog" line replaces with a view, restored by Escape.
# The composer is drawn in the box Claude draws one in — rule, prompt row,
# rule, footer — because that box is what the presence check reads.
# `stty raw` so Escape arrives as a byte we can read.
stty raw -echo 2>/dev/null
# Every ordinary line it has received, drawn above the box. A test that has to
# prove a keystroke did NOT arrive needs the pane itself to say so.
log=""
# `\033[?25l` / `\033[?25h` are DECTCEM — the same hide/show every TUI uses,
# and what tmux reports as `#{cursor_flag}`.
show_composer() {
  printf '\033[?25h\033[2J\033[H%s\r\n────────────────────────\r\n❯ \r\n────────────────────────\r\n  ? for shortcuts\r\n' "$log"
}
show_view()     { printf '\033[?25l\033[2J\033[HTHE VIEW IS UP\r\n'; }
# The measured lockout in disguise: the view drawn ABOVE a composer that is
# present by every readable sign and takes no keys. Only Escape clears it.
show_view_over_composer() {
  printf '\033[?25l\033[2J\033[HTHE VIEW IS UP\r\nEsc to cancel\r\n────────────────────────\r\n❯ \r\n────────────────────────\r\n  ? for shortcuts\r\n'
}
show_prompt() {
  printf '\033[?25l\033[2J\033[HBash command\r\nDo you want to proceed?\r\n 1. Yes\r\n 2. No\r\nEsc to cancel\r\n'
}
show_composer
line=""
while :; do
  c=$(dd bs=1 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')
  [ -z "$c" ] && continue
  if [ "$c" = "0d" ] || [ "$c" = "0a" ]; then
    case "$line" in
      /dialog*) show_view; state=view ;;
      /stuck*)  show_view; state=stuck ;;
      /over*)   show_view_over_composer; state=view ;;
      # A permission prompt: hides the composer like any view, and is the one
      # thing recovery must leave standing.
      /prompt*) show_prompt; state=stuck ;;
      # The measured `/context` shape: a big render hides the footer and
      # brings it back with nobody's help.
      /transient*) show_view; ( sleep 2; show_composer ) & state=composer ;;
      *)        log="$log[$line]"; show_composer; state=composer ;;
    esac
    line=""
  elif [ "$c" = "1b" ]; then
    if [ "${state:-composer}" = "view" ]; then show_composer; state=composer; fi
  else
    line="$line$(printf "\\x$c")"
  fi
done
"#;

    fn recovery_probe(tui: &FakeTui, text: &str, capture: bool) -> SupervisorResult {
        // The supervisor's own postcondition, driven against this fake TUI
        // through its two injected steps. Nothing here touches process-wide
        // state — an earlier version pointed the real tmux helpers at the
        // test socket with an environment variable, which is shared, so two
        // of these tests running at once could look at each other's TUI.
        tui.type_line(text);
        recover_composer_with(
            RecoveryPass {
                answer_by: std::time::Instant::now() + Duration::from_secs(60),
                asking: None,
                matched: "composer".to_string(),
                capture,
                confirm: None,
            },
            &|| Ok((tui.pane(), tui.keyboard())),
            &|key| {
                assert_eq!(
                    key, "Escape",
                    "the ordinary rescue dismisses, never commits"
                );
                tui.send_escape();
                Ok(())
            },
            &PromptPresence::InputBox,
        )
    }

    /// The real `Switch model?` capture, verbatim from the 2.1.223 rig,
    /// **including Claude Code's echo of the command that opened it**. The echo
    /// is the point: a bare value needle matched itself against it, and a bare
    /// affirmative needle matched whichever row was highlighted. Only a needle
    /// anchored to the selection marker survives this frame.
    const CONFIRM_PANE: &str = "\
❯ /model sonnet

────────────────────────────────────────
  Switch model?
  Your next response will be slower and use more tokens

  This conversation is cached for the current model. Switching to Sonnet 5 \
means the full history gets re-read on your next message.

  ❯ 1. Yes, switch to Sonnet 5
    2. No, go back";

    /// The needle the daemon builds for `/model sonnet`.
    const SONNET_NEEDLE: &str = "❯1.yes,switchtosonnet";

    /// Drives the confirm path over a scripted sequence of looks and records
    /// every key sent. `looks` supplies one `(pane, cursor_visible)` per call,
    /// repeating its last entry once exhausted.
    fn confirm_probe(looks: Vec<(String, Keyboard)>) -> (SupervisorResult, Vec<String>) {
        use std::sync::Mutex;
        let index = Mutex::new(0usize);
        let sent: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let outcome = recover_composer_with(
            RecoveryPass {
                answer_by: std::time::Instant::now() + Duration::from_secs(60),
                asking: None,
                matched: "composer".to_string(),
                capture: false,
                confirm: Some(SONNET_NEEDLE.to_string()),
            },
            &|| {
                let mut i = index.lock().unwrap();
                let at = (*i).min(looks.len() - 1);
                *i += 1;
                Ok(looks[at].clone())
            },
            &|key| {
                sent.lock().unwrap().push(key.to_string());
                Ok(())
            },
            &PromptPresence::InputBox,
        );
        let keys = sent.lock().unwrap().clone();
        (outcome, keys)
    }

    /// The dialog is up and unmoved, but it is about a different model than the
    /// one this send asked for. Only the needle check stands between that and a
    /// committed change nobody requested.
    #[test]
    fn a_confirming_key_is_withheld_when_the_dialog_does_not_name_the_requested_model() {
        let other = CONFIRM_PANE.replace("Sonnet 5", "Haiku 4.5");
        let (outcome, keys) = confirm_probe(vec![(other, Keyboard::View)]);
        assert_eq!(keys, ["Escape"], "dismissed, never committed: {outcome:?}");
        assert!(!matches!(outcome, SupervisorResult::ViewConfirmed { .. }));
    }

    /// The two later frames agree with each other; the frame at the first check
    /// does not. Only keeping that first frame catches a dialog replaced inside
    /// the ladder.
    #[test]
    fn a_confirming_key_is_withheld_when_the_first_absent_frame_changed() {
        let moved = CONFIRM_PANE.replace("❯ 1.", "❯  1.");
        let (outcome, keys) = confirm_probe(vec![
            (moved, Keyboard::View),
            (CONFIRM_PANE.to_string(), Keyboard::View),
            (CONFIRM_PANE.to_string(), Keyboard::View),
        ]);
        assert_eq!(keys, ["Escape"], "dismissed, never committed: {outcome:?}");
        assert!(!matches!(outcome, SupervisorResult::ViewConfirmed { .. }));
    }

    /// Every earlier check passes, and then the composer takes the keyboard
    /// back before the key goes out — so `Enter` would submit an empty prompt
    /// rather than answer anything.
    #[test]
    fn a_confirming_key_is_withheld_when_the_composer_returns_before_the_key() {
        let (outcome, keys) = confirm_probe(vec![
            (CONFIRM_PANE.to_string(), Keyboard::View),
            (CONFIRM_PANE.to_string(), Keyboard::View),
            // Same frame, but the composer has the keyboard back.
            (CONFIRM_PANE.to_string(), Keyboard::Program),
        ]);
        assert_eq!(keys, ["Escape"], "dismissed, never committed: {outcome:?}");
        assert!(!matches!(outcome, SupervisorResult::ViewConfirmed { .. }));
    }

    /// **The production key mapping, pinned.** Transposing these two arms would
    /// press `Enter` into every view the ordinary rescue dismisses — `/clear`,
    /// `/compact`, `/config`, `/keybindings` — committing whatever Claude Code
    /// has highlighted. Nothing else in the suite fails when they are swapped.
    #[test]
    fn recovery_uses_escape_without_confirmation_and_enter_with_confirmation() {
        let (_, confirmed) = confirm_probe(vec![
            (CONFIRM_PANE.to_string(), Keyboard::View),
            (CONFIRM_PANE.to_string(), Keyboard::View),
            (CONFIRM_PANE.to_string(), Keyboard::View),
            (IDLE_PANE.to_string(), Keyboard::Program),
        ]);
        assert_eq!(confirmed, ["Enter"], "a established confirmation commits");

        use std::sync::Mutex;
        let sent: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let _ = recover_composer_with(
            RecoveryPass {
                answer_by: std::time::Instant::now() + Duration::from_secs(60),
                asking: None,
                matched: "composer".to_string(),
                capture: false,
                confirm: None,
            },
            &|| Ok(("some view".to_string(), Keyboard::View)),
            &|key| {
                sent.lock().unwrap().push(key.to_string());
                Ok(())
            },
            &PromptPresence::InputBox,
        );
        assert_eq!(
            sent.lock().unwrap().as_slice(),
            ["Escape"],
            "the ordinary rescue dismisses"
        );
    }

    /// **A committing key may only land on the frame that was already there —
    /// and when it may not, the view is still dismissed.**
    ///
    /// `Enter` selects whatever the screen has highlighted, so the confirm path
    /// re-reads the pane immediately before sending and will not commit if a
    /// byte moved. Withholding *both* keys there would leave the view open and
    /// the composer captured, which is the lockout this postcondition exists to
    /// clear — so it falls back to the ordinary rescue.
    #[test]
    fn a_confirmation_that_cannot_be_established_falls_back_to_escape() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Mutex;
        static LOOKS: AtomicU32 = AtomicU32::new(0);
        LOOKS.store(0, Ordering::SeqCst);
        let sent: Mutex<Vec<String>> = Mutex::new(Vec::new());

        let outcome = recover_composer_with(
            RecoveryPass {
                answer_by: std::time::Instant::now() + Duration::from_secs(60),
                asking: None,
                matched: "composer".to_string(),
                capture: false,
                confirm: Some("❯1.yes,switchtosonnet5".to_string()),
            },
            &|| {
                // The dialog at every look until the one taken immediately
                // before the key, where a different screen has replaced it.
                match LOOKS.fetch_add(1, Ordering::SeqCst) {
                    0..=1 => Ok((
                        "❯ /model sonnet\n\
             \n\
             ────────────────────────────────────────\n\
               Switch model?\n\
               Your next response will be slower and use more tokens\n\
             \n\
               This conversation is cached for the current model. Switching to \
             Sonnet 5 means the full history gets re-read on your next message.\n\
             \n\
               ❯ 1. Yes, switch to Sonnet 5\n\
                 2. No, go back"
                            .to_string(),
                        Keyboard::View,
                    )),
                    // The same dialog with one character moved: the needle
                    // still matches, so this exercises the sameness guard and
                    // not the needle guard.
                    _ => Ok((
                        "❯ /model sonnet\n\
             \n\
             ────────────────────────────────────────\n\
               Switch model?\n\
               Your next response will be slower and use more tokens\n\
             \n\
               This conversation is cached for the current model. Switching to \
             Sonnet 5 means the full history gets re-read on your next message.\n\
             \n\
               ❯ 1. Yes, switch to Sonnet 5\n\
                 2. No, go back"
                            .replace("❯ 1.", "❯  1."),
                        Keyboard::View,
                    )),
                }
            },
            &|key| {
                sent.lock().unwrap().push(key.to_string());
                Ok(())
            },
            &PromptPresence::InputBox,
        );
        assert_eq!(
            sent.lock().unwrap().as_slice(),
            ["Escape"],
            "an unestablished confirmation downgrades to the ordinary rescue: {outcome:?}"
        );
        assert!(
            !matches!(outcome, SupervisorResult::ViewConfirmed { .. }),
            "nothing was committed: {outcome:?}"
        );
    }

    /// The confirm path's success shape: the frame held still, the key went
    /// out, the composer came back — reported as `ViewConfirmed`, which claims
    /// a key was delivered and nothing about what the command achieved.
    #[test]
    fn a_confirming_key_on_an_unchanged_frame_reports_view_confirmed() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Mutex;
        static LOOKS: AtomicU32 = AtomicU32::new(0);
        LOOKS.store(0, Ordering::SeqCst);
        let sent: Mutex<Vec<String>> = Mutex::new(Vec::new());

        let outcome = recover_composer_with(
            RecoveryPass {
                answer_by: std::time::Instant::now() + Duration::from_secs(60),
                asking: None,
                matched: "composer".to_string(),
                capture: false,
                confirm: Some("❯1.yes,switchtosonnet5".to_string()),
            },
            &|| {
                // The same dialog, unmoved, at every look; the composer is back
                // once the key has been sent.
                if !sent.lock().unwrap().is_empty() {
                    return Ok((IDLE_PANE.to_string(), Keyboard::Program));
                }
                LOOKS.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "❯ /model sonnet\n\
             \n\
             ────────────────────────────────────────\n\
               Switch model?\n\
               Your next response will be slower and use more tokens\n\
             \n\
               This conversation is cached for the current model. Switching to \
             Sonnet 5 means the full history gets re-read on your next message.\n\
             \n\
               ❯ 1. Yes, switch to Sonnet 5\n\
                 2. No, go back"
                        .to_string(),
                    Keyboard::View,
                ))
            },
            &|key| {
                sent.lock().unwrap().push(key.to_string());
                Ok(())
            },
            &PromptPresence::InputBox,
        );
        assert_eq!(
            sent.lock().unwrap().as_slice(),
            ["Enter"],
            "exactly one Enter"
        );
        assert!(
            matches!(outcome, SupervisorResult::ViewConfirmed { .. }),
            "{outcome:?}"
        );
    }

    /// The measured lockout, reproduced and rescued: a command takes the
    /// composer away, and the supervisor gives it back without anyone
    /// touching the Mac.
    #[test]
    fn a_view_that_hides_the_composer_is_escaped_and_reported() {
        let Some(tui) = FakeTui::start("recover", FAKE_TUI) else {
            eprintln!("tmux unavailable; skipping");
            return;
        };
        assert!(
            tui.pane().contains("shortcuts"),
            "composer first: {}",
            tui.pane()
        );
        match recovery_probe(&tui, "/dialog", true) {
            SupervisorResult::ComposerRecovered {
                pane_snapshot,
                matched,
                ..
            } => {
                assert_eq!(matched, "composer");
                assert!(
                    pane_snapshot.unwrap_or_default().contains("THE VIEW IS UP"),
                    "the snapshot is the screen while the view was up"
                );
                assert!(tui.pane().contains("shortcuts"), "composer is back");
            }
            other => panic!("expected recovery, got {other:?}"),
        }
    }

    /// A view Escape cannot close — the measured `/config` and
    /// `/keybindings` class — is reported as lost, and no further keys are
    /// guessed at.
    #[test]
    fn a_view_that_survives_escape_is_reported_lost() {
        let Some(tui) = FakeTui::start("lost", FAKE_TUI) else {
            return;
        };
        match recovery_probe(&tui, "/stuck", false) {
            SupervisorResult::ComposerLost { .. } => {}
            other => panic!("expected loss, got {other:?}"),
        }
    }

    /// The transient render — measured on `/context`, which hides the
    /// composer for ~2.5s and restores it by itself. The second look is what
    /// stops a rescue nothing needed: no Escape, and a plain `sent`.
    #[test]
    fn a_transient_render_is_never_escaped() {
        let Some(tui) = FakeTui::start("transient", FAKE_TUI) else {
            return;
        };
        match recovery_probe(&tui, "/transient", false) {
            SupervisorResult::Sent { .. } => {
                assert!(
                    tui.pane().contains("shortcuts"),
                    "the composer came back on its own: {}",
                    tui.pane()
                );
            }
            other => panic!("a self-healing render must read as sent, got {other:?}"),
        }
    }

    /// The claim `ComposerLost` makes is "Escape went out and the composer
    /// did not come back". A run of looks that all *failed* has seen no such
    /// thing, so it may not say so — the honest answer promises nothing and
    /// the daemon reports it as typed-outcome-unknown.
    #[test]
    fn a_verify_pass_that_never_saw_the_pane_claims_nothing() {
        let outcome = recover_composer_with(
            RecoveryPass {
                answer_by: std::time::Instant::now() + Duration::from_secs(60),
                asking: None,
                matched: "composer".to_string(),
                capture: false,
                confirm: None,
            },
            &|| {
                // Present at the first look so typing is authorised, then
                // blind: the composer went away and nothing after that could
                // be observed.
                use std::sync::atomic::{AtomicU32, Ordering};
                static LOOKS: AtomicU32 = AtomicU32::new(0);
                // Three successful looks — the two scheduled checks and the
                // re-read taken immediately before the key — then blind.
                match LOOKS.fetch_add(1, Ordering::SeqCst) {
                    0..=2 => Ok(("a view".to_string(), Keyboard::View)),
                    _ => Err(anyhow::anyhow!("the pane could not be read")),
                }
            },
            &|_| Ok(()),
            &PromptPresence::InputBox,
        );
        match outcome {
            SupervisorResult::RecoveryUnconfirmed { reason, .. } => {
                assert!(reason.contains("while verifying after the key"), "{reason}");
                assert!(
                    reason.contains("the pane could not be read"),
                    "the look's own failure travels in the reason: {reason}"
                );
            }
            other => panic!("nothing was observed, so nothing may be claimed: {other:?}"),
        }
    }

    /// **A decision on screen is never Escaped.** A slash command can be a
    /// skill, a skill can reach a tool, and the permission prompt that
    /// follows hides the composer exactly as a view does. Escaping it would
    /// cancel a decision the human never made, silently.
    #[test]
    fn a_permission_prompt_is_never_escaped() {
        let Some(tui) = FakeTui::start("prompt", FAKE_TUI) else {
            return;
        };
        match recovery_probe(&tui, "/prompt", false) {
            SupervisorResult::RecoveryUnconfirmed { reason, .. } => {
                assert!(reason.contains("waiting for an answer"), "{reason}");
                assert!(
                    tui.pane().contains("Do you want to proceed?"),
                    "the prompt must still be standing: {}",
                    tui.pane()
                );
            }
            other => panic!("a prompt must never be escaped, got {other:?}"),
        }
    }

    /// A look that failed is a refusal, and the refusal comes from the look
    /// itself rather than from a default. Asked of a session name that cannot
    /// exist — a **read**, addressed at nothing, so it cannot disturb any
    /// session on the shared server.
    #[test]
    fn a_pane_that_cannot_be_looked_at_yields_an_error_not_a_guess() {
        let nowhere = format!("cc-does-not-exist-{}", std::process::id());
        assert!(
            look_at_pane(&nowhere).is_err(),
            "a pane that cannot be read must be an error; the interlock turns that into \
             a refusal, and any permissive default there types into a screen nobody saw"
        );
    }

    /// **The window the re-look closes.** A skill's tool call reaches its
    /// permission prompt on its own schedule. Judged from the frame saved at
    /// the second check, a prompt that arrived a moment later would be
    /// Escaped — cancelling a decision nobody made. The pane is therefore
    /// looked at again immediately before the key, and it is *that* look the
    /// guard reads. Driven synthetically because the window is microseconds
    /// wide: no fake TUI can be timed into it.
    #[test]
    fn a_prompt_that_appears_after_the_saved_frame_is_still_not_escaped() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static ESCAPES: AtomicU32 = AtomicU32::new(0);
        static LOOKS: AtomicU32 = AtomicU32::new(0);
        let outcome = recover_composer_with(
            RecoveryPass {
                answer_by: std::time::Instant::now() + Duration::from_secs(60),
                asking: None,
                matched: "composer".to_string(),
                capture: false,
                confirm: None,
            },
            &|| match LOOKS.fetch_add(1, Ordering::SeqCst) {
                // The two checks see a view and no question…
                0 | 1 => Ok(("THE VIEW IS UP".to_string(), Keyboard::View)),
                // …and by the re-look the tool has asked for permission.
                _ => Ok((
                    "Bash\nDo you want to proceed?\n 1. Yes".to_string(),
                    Keyboard::View,
                )),
            },
            &|_| {
                ESCAPES.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            &PromptPresence::InputBox,
        );
        match outcome {
            SupervisorResult::RecoveryUnconfirmed { reason, .. } => {
                assert!(reason.contains("waiting for an answer"), "{reason}");
            }
            other => panic!("expected the prompt to be left alone, got {other:?}"),
        }
        assert_eq!(
            ESCAPES.load(Ordering::SeqCst),
            0,
            "no key may be sent at a screen that is asking something"
        );
    }

    /// **The operator's own definition of a prompt is honoured.** Configured
    /// needles exist because Claude's wording changed; a guard still looking
    /// only for the old question would Escape the very prompt the operator
    /// taught us to recognise — the same bug as reading "this is the
    /// composer" off a variant an override has replaced.
    #[test]
    fn a_prompt_recognised_only_by_configured_needles_is_not_escaped() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static ESCAPES: AtomicU32 = AtomicU32::new(0);
        let outcome = recover_composer_with(
            RecoveryPass {
                answer_by: std::time::Instant::now() + Duration::from_secs(60),
                asking: Some(PromptPresence::AnyOf {
                    needles: vec!["shall i go ahead".to_string()],
                }),
                matched: "composer".to_string(),
                capture: false,
                confirm: None,
            },
            &|| Ok(("Shall I go ahead with this?".to_string(), Keyboard::View)),
            &|_| {
                ESCAPES.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            &PromptPresence::InputBox,
        );
        match outcome {
            SupervisorResult::RecoveryUnconfirmed { reason, .. } => {
                assert!(reason.contains("waiting for an answer"), "{reason}");
            }
            other => panic!("the configured prompt must be left standing, got {other:?}"),
        }
        assert_eq!(
            ESCAPES.load(Ordering::SeqCst),
            0,
            "no key at a screen that is asking"
        );
    }

    /// **The lockout in disguise, end to end.** A view drawn above a
    /// composer whose needle matches must still be rescued: recovery has to
    /// read "present" as "takes keys", or it reports a healthy session and
    /// leaves the phone locked out. Real tmux, real Escape.
    #[test]
    fn a_view_drawn_above_a_live_looking_composer_is_still_escaped() {
        let Some(tui) = FakeTui::start("over", FAKE_TUI) else {
            return;
        };
        match recovery_probe(&tui, "/over", false) {
            SupervisorResult::ComposerRecovered { .. } => {
                assert!(
                    !tui.pane().contains("Esc to cancel"),
                    "the view is still up: {}",
                    tui.pane()
                );
            }
            other => panic!("a view over the composer must be escaped, got {other:?}"),
        }
    }

    /// The snapshot is an allowlisted privilege, not a side effect: a
    /// recovered command that was not granted one carries no pane at all.
    #[test]
    fn a_recovered_command_without_the_grant_keeps_no_snapshot() {
        let Some(tui) = FakeTui::start("nosnap", FAKE_TUI) else {
            return;
        };
        match recovery_probe(&tui, "/dialog", false) {
            SupervisorResult::ComposerRecovered { pane_snapshot, .. } => {
                assert!(
                    pane_snapshot.is_none(),
                    "only the snapshot commands may keep the Mac's screen"
                );
            }
            other => panic!("expected recovery, got {other:?}"),
        }
    }

    /// Free text aimed at the composer: the target every test below sends with.
    fn composer_target() -> Target<'static> {
        Target {
            presence: &PromptPresence::InputBox,
            is_composer: true,
        }
    }

    /// The send as `send_text` sequences it, over a pane a test owns: look,
    /// leave a bare scroll position, look again, authorise on what is true
    /// *now*, and type only if authorised. Typing regardless would test
    /// nothing.
    ///
    /// The two decisions are the daemon's own — [`may_leave_copy_mode`] and
    /// [`authorise`], called here, not restated — so a copy cannot agree with
    /// the prose and disagree with the daemon. What is **not** here is the
    /// clock: `send_text`'s budget arithmetic decides whether there is room to
    /// do any of this, and these tests are about what happens when there is.
    fn send_through_the_interlock(
        tui: &FakeTui,
        text: &str,
    ) -> std::result::Result<String, String> {
        let target = composer_target();
        let (mut pane, mut keyboard) = (tui.pane(), tui.keyboard());
        if may_leave_copy_mode(&target, &pane, keyboard) {
            tui.leave_copy_mode();
            (pane, keyboard) = (tui.pane(), tui.keyboard());
        }
        let matched = authorise(&pane, target.presence, None, target.is_composer, keyboard)?;
        tui.type_line(text);
        Ok(matched)
    }

    /// **A pane parked on a scroll position is left by the send itself.**
    ///
    /// Measured on claude 2.1.232: copy-mode leaves `#{cursor_flag}` at 1 while
    /// every keystroke goes to tmux's own mode table, `send-keys` still exits
    /// 0, and the text never reaches the composer. The mouse wheel over the
    /// inline transcript is what puts a pane there, and
    /// [`tmux::render_server_conf`] turns the mouse on, so this is the ordinary
    /// state of somebody reading their own session at the Mac — and a prompt
    /// from the phone is the intent to type into it.
    ///
    /// Real tmux and real copy-mode, because the whole claim is about what tmux
    /// does with a keystroke. The harness's own record of what it received is
    /// what proves the text *arrived*, rather than that a function returned
    /// `Ok`; `#{pane_in_mode}` at zero afterwards is what proves the mode was
    /// left rather than typed around.
    #[test]
    fn a_pane_in_copy_mode_is_left_by_the_send_that_needs_it() {
        let Some(tui) = FakeTui::start("scrollback", FAKE_TUI) else {
            eprintln!("tmux unavailable; skipping");
            return;
        };

        assert_eq!(tui.keyboard(), Keyboard::Program);
        assert_eq!(
            send_through_the_interlock(&tui, "BEFORE").as_deref(),
            Ok("composer")
        );
        assert!(tui.wait_for("[BEFORE]"), "the pane took it: {}", tui.pane());

        tui.enter_scrollback();
        assert_eq!(tui.mode(), "copy-mode");
        assert_eq!(tui.modes(), "1", "one mode, and it is the wheel's own");
        assert_eq!(tui.keyboard(), Keyboard::Scrollback(TmuxMode::CopyMode));

        assert_eq!(
            send_through_the_interlock(&tui, "AFTER").as_deref(),
            Ok("composer"),
            "a scroll position is not a reason to refuse a prompt"
        );
        assert!(
            tui.wait_for("[AFTER]"),
            "the text has to have ARRIVED, not merely been authorised: {}",
            tui.pane()
        );
        assert_eq!(
            tui.modes(),
            "0",
            "the mode has to be gone, not stepped around"
        );
        assert_eq!(tui.keyboard(), Keyboard::Program);
        // What the exit costs is the position. The transcript above it — the
        // thing somebody scrolls back to read — is still there.
        assert!(
            tui.pane().contains("[BEFORE]"),
            "the transcript survived the exit: {}",
            tui.pane()
        );
    }

    /// **A mode the person at the Mac opened is never popped for a send.**
    ///
    /// `#{pane_mode}` names the mode on *top* of the stack, so a name alone
    /// cannot tell a bare scroll position from a copy-mode with somebody's
    /// `choose-tree` underneath it — and `send-keys -X cancel` would pop the
    /// copy-mode off that stack quite happily. The count is what refuses it.
    ///
    /// Measured on tmux 3.7b, and the reason each of these is here: clock-mode
    /// answers the same `1` copy-mode does and only the name separates them;
    /// `choose-tree` is a question on the screen, and typing into what is
    /// underneath answers one nobody saw. `customize-mode` belongs in this
    /// list and is not driven here: it arrived in tmux 3.2, and a test that
    /// silently does nothing on an older server is worse than one mode fewer.
    /// Its name is covered where no tmux is needed to cover it — see
    /// [`crate::tmux`]'s reply table.
    #[test]
    fn a_mode_the_mac_opened_is_refused_and_left_standing() {
        let Some(tui) = FakeTui::start("foreignmode", FAKE_TUI) else {
            eprintln!("tmux unavailable; skipping");
            return;
        };
        assert_eq!(
            send_through_the_interlock(&tui, "BEFORE").as_deref(),
            Ok("composer")
        );
        assert!(tui.wait_for("[BEFORE]"), "the pane took it: {}", tui.pane());

        for (command, name) in [("clock-mode", "clock-mode"), ("choose-tree", "tree-mode")] {
            tui.enter_mode(command);
            assert_eq!(tui.mode(), name, "tmux's own name for {command}");
            assert_eq!(tui.keyboard(), Keyboard::Scrollback(TmuxMode::Other));
            assert!(
                !may_leave_copy_mode(&composer_target(), &tui.pane(), tui.keyboard()),
                "{command} is not a scroll position"
            );

            let refused = send_through_the_interlock(&tui, "SWALLOWED")
                .expect_err("a mode that is not a scroll position must refuse");
            assert!(refused.contains("tmux has the keyboard"), "{refused}");
            assert!(refused.contains("nothing was typed"), "{refused}");
            assert_eq!(tui.mode(), name, "{command} must still be up");
            assert_eq!(tui.modes(), "1", "{command} must still be up");
            let during = tui.pane();
            assert!(
                during.contains("[BEFORE]") && !during.contains("SWALLOWED"),
                "the refusal has to have saved the keystroke, not merely reported one: {during}"
            );
            tui.leave_scrollback();
        }

        // The stack, which is the case a name-only reading gets wrong: the
        // top mode really is `copy-mode`, and popping it would take away a
        // `choose-tree` that was never anybody's scroll position.
        tui.enter_mode("choose-tree");
        tui.enter_scrollback();
        assert_eq!(tui.mode(), "copy-mode", "the top of the stack");
        assert_eq!(tui.modes(), "2");
        assert_eq!(tui.keyboard(), Keyboard::Scrollback(TmuxMode::Other));
        let refused = send_through_the_interlock(&tui, "SWALLOWED")
            .expect_err("a copy-mode with something under it must refuse");
        assert!(refused.contains("nothing was typed"), "{refused}");
        assert_eq!(
            tui.modes(),
            "2",
            "neither mode may be popped: the tree is still the Mac's"
        );
        assert!(!tui.pane().contains("SWALLOWED"), "{}", tui.pane());
    }

    /// **A scroll position over a view is not taken, because the send was
    /// never going to be typed.**
    ///
    /// The composer is read off the capture the look already paid for, and
    /// that capture is the live screen whether the pane is scrolled or not —
    /// so it says the same thing before the exit as after it. Checking it
    /// first therefore costs nothing and refuses nothing that would otherwise
    /// have succeeded, and it keeps the daemon from clearing somebody's scroll
    /// position for a send with no composer to type into. The pane a mode is
    /// popped in is not always Claude's: every call here addresses the
    /// session's *current* pane, which follows the window the person at the
    /// Mac is looking at.
    ///
    /// Both things are left standing: the mode, and the view under it —
    /// dismissing that would answer a question nobody asked.
    #[test]
    fn a_view_under_the_scroll_position_keeps_the_mode_and_the_view() {
        let Some(tui) = FakeTui::start("underview", FAKE_TUI) else {
            eprintln!("tmux unavailable; skipping");
            return;
        };
        tui.type_line("/dialog");
        assert!(tui.wait_for("THE VIEW IS UP"), "{}", tui.pane());
        assert_eq!(tui.keyboard(), Keyboard::View);

        tui.enter_scrollback();
        assert_eq!(
            tui.keyboard(),
            Keyboard::Scrollback(TmuxMode::CopyMode),
            "a mode holds the keyboard whatever is drawn under it"
        );
        assert!(
            !may_leave_copy_mode(&composer_target(), &tui.pane(), tui.keyboard()),
            "no composer on the screen, so nothing is worth taking the scroll for"
        );

        let refused =
            send_through_the_interlock(&tui, "SWALLOWED").expect_err("no composer, no keystroke");
        assert!(
            refused.contains("expected prompt not on screen"),
            "the composer's own refusal, reached without touching the pane: {refused}"
        );
        assert!(refused.contains("nothing was typed"), "{refused}");
        assert_eq!(tui.modes(), "1", "the scroll position was not taken");
        let after = tui.pane();
        assert!(
            after.contains("THE VIEW IS UP"),
            "the view under it is not ours to dismiss: {after}"
        );
        assert!(!after.contains("SWALLOWED"), "{after}");
    }

    /// **The exit command itself, against the pane it is used on.**
    ///
    /// [`may_leave_copy_mode`] only ever points this at a lone copy-mode, and
    /// on a lone copy-mode `copy-mode -q` does the same thing — so no test
    /// above this one can tell the two commands apart. They are not the same
    /// command, and the difference is the whole of why doing this unasked is
    /// safe: between the look that decided and the command that acts, the
    /// person at the Mac can open anything. This pins what tmux 3.7b really
    /// does with each of the panes that window can produce — **including the
    /// one it does not protect**, which is worth a failing test the day it
    /// changes: `view-mode` shares copy-mode's command table, so a `-X cancel`
    /// that lands on one pops it.
    #[test]
    fn the_exit_command_pops_one_mode_of_copy_modes_family_and_no_other() {
        let Some(tui) = FakeTui::start("exitcmd", FAKE_TUI) else {
            eprintln!("tmux unavailable; skipping");
            return;
        };

        // A lone scroll position: gone, which is the whole job.
        tui.enter_scrollback();
        assert_eq!(tui.modes(), "1");
        tui.leave_copy_mode();
        assert_eq!(tui.modes(), "0", "a lone copy-mode is left");

        // A copy-mode with something under it: exactly one pops, and what was
        // underneath is still the Mac's. `copy-mode -q` answers "0" here — it
        // flattens the stack — which is why it is not what the daemon runs.
        tui.enter_mode("choose-tree");
        tui.enter_scrollback();
        assert_eq!(tui.modes(), "2");
        tui.leave_copy_mode();
        assert_eq!(tui.modes(), "1", "one mode pops, not the stack");
        assert_eq!(tui.mode(), "tree-mode", "the Mac's own view survives");
        tui.leave_scrollback();

        // A mode that is not copy-mode's own: tmux answers `not in a mode` and
        // changes nothing. `copy-mode -q` clears a clock-mode as readily as a
        // scroll position, so the guard would be the only thing standing there.
        // `clock-mode` stands for all of them here because it is as old as
        // tmux; `options-mode` needs 3.2 to exist at all, and a step that
        // quietly does nothing on an older server is worse than one mode
        // fewer. Their names are covered where no tmux is needed to cover
        // them — see [`crate::tmux`]'s reply table.
        tui.enter_mode("clock-mode");
        tui.leave_copy_mode();
        assert_eq!(tui.modes(), "1", "clock-mode is untouched by the exit");
        assert_eq!(tui.mode(), "clock-mode", "clock-mode is untouched");
        tui.leave_scrollback();

        // The exception, pinned rather than papered over. tmux pushes
        // `view-mode` to show output — the stock `prefix ?` does — and it
        // shares copy-mode's command table, so this pops it. Nothing points
        // the command at one: `read_keyboard` calls view-mode `Other` and the
        // send refuses. What is left is the few milliseconds between that look
        // and this command, and this is the size of what can be lost in them.
        tui.enter_mode("clock-mode");
        assert_eq!(tui.modes(), "1");
        tui.leave_scrollback();
        tui.enter_scrollback();
        let _ = tui.tmux(&["run-shell", "-t", &tui.session, "echo pushed-by-tmux"]);
        if tui.mode() == "view-mode" {
            assert_eq!(tui.modes(), "2", "view-mode stacks on the scroll position");
            tui.leave_copy_mode();
            assert_eq!(tui.modes(), "1", "one pops");
            assert_eq!(
                tui.mode(),
                "copy-mode",
                "and it is the view-mode that went, not the scroll position"
            );
        }
        tui.leave_scrollback();

        // And against a pane in no mode at all — the losing side of the race,
        // where the mode cleared between the look and the command. tmux exits
        // non-zero and types nothing: the word `cancel` never reaches the
        // program, which the next line proves by arriving alone.
        assert_eq!(tui.modes(), "0");
        tui.leave_copy_mode();
        tui.type_line("MARKER");
        assert!(tui.wait_for("[MARKER]"), "{}", tui.pane());
        let pane = tui.pane();
        assert!(
            !pane.contains("cancel"),
            "the exit command must never be typed as text: {pane}"
        );
    }

    /// **The ordering, which is the safety property.**
    ///
    /// [`look_the_gates_run_on`] decides whether the Mac's screen is touched
    /// and which look the gates then judge. Every row states what the send
    /// path claims: the exit happens only where it is allowed and affordable,
    /// it happens once, the second look replaces *both* facts, and a look that
    /// fails refuses rather than falling back on the one before it.
    #[test]
    fn the_gates_run_on_the_look_taken_after_the_exit_and_the_exit_runs_once() {
        let second = || (IDLE_PANE.to_string(), Keyboard::Program);
        // Every arrangement, and what it is entitled to do.
        for (what, pane, keyboard, affordable, leaves) in [
            (
                "nothing is holding it",
                IDLE_PANE,
                Keyboard::Program,
                true,
                false,
            ),
            (
                "a view is holding it",
                IDLE_PANE,
                Keyboard::View,
                true,
                false,
            ),
            (
                "a mode the Mac opened",
                IDLE_PANE,
                Keyboard::Scrollback(TmuxMode::Other),
                true,
                false,
            ),
            (
                "no composer to type into",
                NO_COMPOSER_PANE,
                Keyboard::Scrollback(TmuxMode::CopyMode),
                true,
                false,
            ),
            (
                "no time to finish the send",
                IDLE_PANE,
                Keyboard::Scrollback(TmuxMode::CopyMode),
                false,
                false,
            ),
            (
                "a bare scroll position under a drawn composer",
                IDLE_PANE,
                Keyboard::Scrollback(TmuxMode::CopyMode),
                true,
                true,
            ),
        ] {
            let exits = std::cell::Cell::new(0);
            let looks = std::cell::Cell::new(0);
            let got = look_the_gates_run_on(
                &composer_target(),
                (pane.to_string(), keyboard),
                affordable,
                &|| exits.set(exits.get() + 1),
                &|| {
                    looks.set(looks.get() + 1);
                    Ok(second())
                },
            )
            .expect("a look that answered is never a refusal");
            assert_eq!(exits.get(), usize::from(leaves), "exits for {what}");
            assert_eq!(looks.get(), usize::from(leaves), "second looks for {what}");
            if leaves {
                // Both halves come from the second look, not one of each.
                assert_eq!(got, second(), "the gates judge the pane after the exit");
            } else {
                assert_eq!(
                    got,
                    (pane.to_string(), keyboard),
                    "nothing was touched, so nothing was re-read: {what}"
                );
            }
        }
    }

    /// A second look that fails is a refusal, and never the look before it.
    /// The pane was changed a moment ago; carrying the stale read forward
    /// would authorise a send against a screen that no longer exists.
    #[test]
    fn a_second_look_that_fails_refuses_rather_than_reusing_the_first() {
        let exits = std::cell::Cell::new(0);
        let failed = look_the_gates_run_on(
            &composer_target(),
            (
                IDLE_PANE.to_string(),
                Keyboard::Scrollback(TmuxMode::CopyMode),
            ),
            true,
            &|| exits.set(exits.get() + 1),
            &|| Err(anyhow::anyhow!("the pane went away")),
        );
        assert_eq!(exits.get(), 1, "the exit still ran");
        assert!(
            failed.is_err(),
            "a look that failed cannot become the look the gates use"
        );
    }

    /// Every state the keyboard can be in, crossed with both kinds of target
    /// and with whether the composer is on the screen at all. The table is the
    /// point: one cell out of twelve authorises the only act in this program
    /// that changes the Mac's screen without being asked to.
    #[test]
    fn only_a_bare_scroll_position_under_a_drawn_composer_may_be_left_unasked() {
        let prompt_target = Target {
            presence: &PromptPresence::PermissionPrompt,
            is_composer: false,
        };
        for (keyboard, may) in [
            (Keyboard::Program, false),
            (Keyboard::View, false),
            (Keyboard::Scrollback(TmuxMode::CopyMode), true),
            (Keyboard::Scrollback(TmuxMode::Other), false),
        ] {
            assert_eq!(
                may_leave_copy_mode(&composer_target(), IDLE_PANE, keyboard),
                may,
                "composer drawn, keys for the composer, with {keyboard:?}"
            );
            // No composer on the screen: nothing here is worth taking a scroll
            // position for, because nothing could be typed if it were taken.
            assert!(
                !may_leave_copy_mode(&composer_target(), NO_COMPOSER_PANE, keyboard),
                "no composer drawn, with {keyboard:?}"
            );
            // A permission prompt is answered through a view, where the
            // question is which prompt is on screen. Nothing is left for it.
            assert!(
                !may_leave_copy_mode(
                    &prompt_target,
                    &permission_pane("touch /private/tmp/a"),
                    keyboard
                ),
                "keys for a prompt with {keyboard:?}"
            );
        }
    }

    /// An ordinary line never leaves the composer, so recovery reports the
    /// plain send — no false "recovered" on the common path.
    #[test]
    fn an_inline_line_reports_plain_sent() {
        let Some(tui) = FakeTui::start("inline", FAKE_TUI) else {
            return;
        };
        match recovery_probe(&tui, "hello there", false) {
            SupervisorResult::Sent { matched } => assert_eq!(matched, "composer"),
            other => panic!("expected sent, got {other:?}"),
        }
    }

    #[test]
    fn a_bound_answer_is_only_authorised_against_its_own_prompt() {
        // The supervisor half of the prompt interlock. Presence alone cannot separate two
        // permission prompts — same wording, same options, same footer — so an
        // answer that arrives a moment late used to be typed into whichever one
        // was up. The fingerprint is checked here, in the same breath as the
        // presence check and immediately before the keys go out.
        let mine = permission_pane("touch /private/tmp/a.txt");
        let theirs = permission_pane("rm -rf /private/tmp");
        let expect = prompt_fingerprint(&mine, "doyouwanttoproceed").unwrap();

        assert_eq!(
            authorise(
                &mine,
                &PromptPresence::PermissionPrompt,
                Some(&expect),
                false,
                Keyboard::View
            )
            .unwrap(),
            "doyouwanttoproceed"
        );

        // A *different* prompt still passes the presence check, and must not
        // pass this one.
        assert!(
            PromptPresence::PermissionPrompt
                .find_match(&theirs, None)
                .is_some(),
            "the presence check on its own is blind to which prompt this is"
        );
        let refused = authorise(
            &theirs,
            &PromptPresence::PermissionPrompt,
            Some(&expect),
            false,
            Keyboard::View,
        )
        .expect_err("a different prompt must not be typed into");
        assert!(refused.contains("not the one"), "{refused}");
    }

    #[test]
    fn no_prompt_means_no_keystroke_whatever_else_is_offered() {
        let mine = permission_pane("touch /private/tmp/a.txt");
        let expect = prompt_fingerprint(&mine, "doyouwanttoproceed").unwrap();
        for pane in [IDLE_PANE, "", "some other tmux window entirely\n$ ls"] {
            assert!(authorise(
                pane,
                &PromptPresence::PermissionPrompt,
                Some(&expect),
                false,
                Keyboard::View
            )
            .is_err());
            assert!(authorise(
                pane,
                &PromptPresence::PermissionPrompt,
                None,
                false,
                Keyboard::View
            )
            .is_err());
        }
    }

    #[test]
    fn free_text_is_authorised_by_the_composer_and_not_by_a_prompt() {
        // A takeover is not an answer to a prompt, so it carries no fingerprint
        // — but it must still be refused while a prompt is up, or "yes please
        // continue" gets typed at a permission prompt as the literal answer.
        assert!(authorise(
            IDLE_PANE,
            &PromptPresence::InputBox,
            None,
            true,
            Keyboard::Program
        )
        .is_ok());
        assert!(authorise(
            &permission_pane("touch /private/tmp/a.txt"),
            &PromptPresence::InputBox,
            None,
            true,
            Keyboard::View
        )
        .is_err());
    }

    /// **The lockout in disguise.** Measured: `/status` submitted during a
    /// turn leaves the Settings view drawn *above* a composer box that is
    /// present by every readable sign and accepts no keys. The screen says
    /// yes; only the hidden cursor says what is true.
    #[test]
    fn a_view_holding_the_keyboard_refuses_the_send_even_with_a_drawn_composer() {
        assert!(
            PromptPresence::InputBox
                .find_match(VIEW_ABOVE_COMPOSER_PANE, None)
                .is_some(),
            "the fixture must read as a composer, or it proves nothing"
        );
        let refused = authorise(
            VIEW_ABOVE_COMPOSER_PANE,
            &PromptPresence::InputBox,
            None,
            true,
            Keyboard::View,
        )
        .expect_err("a view holding the keyboard must refuse");
        assert!(refused.contains("takes no keys"), "{refused}");
    }

    /// **And tmux holding it refuses too — every kind of holding, including
    /// the one the send path knows how to leave.**
    ///
    /// This is asked of [`authorise`] directly and on purpose. By the time it
    /// runs, a bare scroll position has usually been left already, so the
    /// tests that drive a real pane cannot reach this arm with
    /// [`tmux::TmuxMode::CopyMode`] in hand — and that is exactly the state it
    /// still has to refuse: an exit that exited non-zero, a mode re-entered
    /// while the second look was being taken, a send the clock left no room
    /// to clear. Typing into any of them is a keystroke tmux swallows and this
    /// program reports as sent.
    #[test]
    fn tmux_holding_the_keyboard_refuses_the_send_whichever_mode_it_is() {
        assert!(
            PromptPresence::InputBox
                .find_match(IDLE_PANE, None)
                .is_some(),
            "the fixture must read as a composer, or the keyboard is never asked about"
        );
        for mode in [TmuxMode::CopyMode, TmuxMode::Other] {
            let refused = authorise(
                IDLE_PANE,
                &PromptPresence::InputBox,
                None,
                true,
                Keyboard::Scrollback(mode),
            )
            .expect_err("a pane tmux is holding must refuse");
            assert!(
                refused.contains("tmux has the keyboard"),
                "{mode:?}: {refused}"
            );
            assert!(refused.contains("nothing was typed"), "{mode:?}: {refused}");
        }
    }

    /// The counterpart: a live composer mid-turn keeps its cursor, queues
    /// what it is given, and must not be refused — including when the pane
    /// happens to contain a view's words. **The pane's text is not the
    /// signal.** A rule that read `Esc to cancel` out of the pane would
    /// refuse every send in this session until an agent's own output
    /// scrolled away, and would Escape a running turn to "rescue" it.
    #[test]
    fn a_live_composer_is_authorised_even_when_the_pane_says_esc_to_cancel() {
        assert!(authorise(
            MID_TURN_PANE,
            &PromptPresence::InputBox,
            None,
            true,
            Keyboard::Program
        )
        .is_ok());
        let agent_wrote_it = MID_TURN_PANE.replace(
            MID_TURN_TRANSCRIPT_LINE,
            "⏺ The view offers Esc to cancel, so pressing it closes the dialog.",
        );
        assert_ne!(
            agent_wrote_it, MID_TURN_PANE,
            "the transcript line has to be there for this test to say anything"
        );
        assert!(
            authorise(
                &agent_wrote_it,
                &PromptPresence::InputBox,
                None,
                true,
                Keyboard::Program
            )
            .is_ok(),
            "an agent quoting a view's dismissal hint must not lock its own session"
        );
    }

    /// A permission prompt hides the cursor exactly as every other view
    /// does — and answering one is the whole point of that path, so the
    /// keyboard question is asked of the composer only.
    #[test]
    fn answering_a_permission_prompt_is_untouched() {
        let prompt = "Do you want to proceed?\n  1. Yes\n  2. No\nEsc to cancel";
        assert!(authorise(
            prompt,
            &PromptPresence::PermissionPrompt,
            None,
            false,
            Keyboard::View
        )
        .is_ok());
    }

    #[test]
    fn an_exit_report_introduces_the_session_before_announcing_its_end() {
        // A session that starts *and* ends while `ccd` is down was never
        // registered, so the daemon had no row to attach the exit to and logged
        // "exit reported for unknown session" before dropping the frame. The run
        // vanished entirely: not in the fleet, no event log, nothing anywhere
        // saying an agent had ever run. Replaying the registration on the same
        // connection, immediately before the exit, is what gives the daemon
        // something to record against.
        let path = std::env::temp_dir().join(format!(
            "cc-supervisor-exit-{}-{}.sock",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let collected = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut frames = Vec::new();
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                frames.push(serde_json::from_str::<serde_json::Value>(&line).unwrap());
            }
            frames
        });

        let args = SupervisorArgs {
            session_id: "cc-7".into(),
            session_uid: Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR".into()),
            tmux_session: "cc-7".into(),
            cwd: "/tmp/project".into(),
            claude_bin: None,
        };
        let registration = registration_frame(&args, "2026-07-31T00:00:00Z");
        report_exit(&path, &registration).unwrap();

        let frames = collected.join().unwrap();
        assert_eq!(frames.len(), 2, "expected register then exit: {frames:?}");
        // Order is the property: the daemon's read loop handles them in the
        // order they arrive, so the row exists before the exit needs it.
        assert_eq!(frames[0]["type"], "register");
        assert_eq!(frames[0]["session_id"], "cc-7");
        assert_eq!(frames[0]["session_uid"], "01K1B3XQ8ZC0DE5FGH7JKMNPQR");
        // The registration has to carry enough to *create* the session, not
        // merely name it: a row with no working directory is not a session.
        assert_eq!(frames[0]["cwd"], "/tmp/project");
        assert_eq!(frames[0]["tmux_session"], "cc-7");
        // **And it says on the wire that it is a replay.** Everything else here
        // is a copy of the live registration, which is the point and also the
        // danger: a frame the daemon cannot tell from a supervisor arriving is
        // one it hands the session to, and the process behind this one is
        // already dead. A daemon that adopts it then ends whichever run has
        // since resumed the uid instead of this one — measured, round-10 F1.
        assert_eq!(
            frames[0]["exit_replay"], true,
            "the replay must identify itself: {frames:?}"
        );
        assert_eq!(frames[1]["type"], "session_exited");
        assert_eq!(frames[1]["session_uid"], "01K1B3XQ8ZC0DE5FGH7JKMNPQR");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_replayed_registration_is_the_one_the_supervisor_registers_with() {
        // Two copies of this frame would drift, and the drift would only ever
        // show up in the case that is hardest to reproduce — a daemon that was
        // down for a whole session.
        let args = SupervisorArgs {
            session_id: "cc-3".into(),
            session_uid: Some("01K1B3XQ8ZC0DE5FGH7JKMNPQR".into()),
            tmux_session: "cc-3".into(),
            cwd: "/tmp".into(),
            claude_bin: Some("/usr/local/bin/claude".into()),
        };
        let frame = registration_frame(&args, "2026-07-31T00:00:00Z");
        assert_eq!(frame.session_id, args.session_id);
        assert_eq!(frame.session_uid, args.session_uid);
        assert_eq!(frame.cwd, args.cwd);
        assert_eq!(frame.claude_bin, args.claude_bin);
        assert_eq!(frame.tmux_socket, protocol::TMUX_SOCKET_NAME);
        assert_eq!(frame.protocol_minor, protocol::PROTOCOL_MINOR);
        // The frame a *living* supervisor registers with. `report_exit` sets
        // this on its copy and only there; a live registration that claimed to
        // be a replay would be refused the session it is asking to host.
        assert!(!frame.exit_replay);
    }

    #[test]
    fn shutting_a_socket_down_ends_a_blocking_read_on_it() {
        // The mechanism the exit report depends on. The supervisor spends its
        // life blocked reading from a healthy daemon; when its tmux session
        // dies, the only way back to the `session_gone` check is for another
        // thread to end that read. If this ever stopped being true, a session
        // that exits while ccd is up would silently stay `live` in the fleet —
        // which is exactly what it did before this existed.
        let path = std::env::temp_dir().join(format!(
            "cc-supervisor-test-{}-{}.sock",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let accepted = std::thread::spawn(move || {
            // Held open, saying nothing: a daemon with no requests to make.
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(30));
            drop(stream);
        });

        let stream = UnixStream::connect(&path).unwrap();
        let handle = stream.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line)
        });

        std::thread::sleep(Duration::from_millis(100));
        handle.shutdown(Shutdown::Both).unwrap();

        let start = std::time::Instant::now();
        let read = reader.join().unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the read did not end promptly: {:?}",
            start.elapsed()
        );
        // Either clean EOF or an error — both return control to the caller,
        // which is the whole requirement.
        assert!(matches!(read, Ok(0) | Err(_)), "unexpected read: {read:?}");

        let _ = std::fs::remove_file(&path);
        drop(accepted);
    }
}
