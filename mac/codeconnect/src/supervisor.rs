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
                    match tmux::session_presence(&tmux_session) {
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
        } => send_text(
            args,
            config,
            &text,
            Target {
                presence: &require,
                is_composer: targets_composer,
            },
            submit,
            expect.as_ref(),
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

/// The pane and whether its cursor is visible, read together.
fn look_at_pane(session: &str) -> Result<(String, bool)> {
    Ok((
        tmux::capture_visible_pane(session)?,
        tmux::cursor_is_visible(session)?,
    ))
}

fn send_text(
    args: &SupervisorArgs,
    config: &Config,
    text: &str,
    target: Target<'_>,
    submit: bool,
    expect: Option<&PromptFingerprint>,
    recovery: Recovery,
) -> SupervisorResult {
    let require = target.presence;
    // One look: the pane, and who has the keyboard. Two facts, one moment,
    // one failure path — and **a look that failed is a refusal**, never a
    // permissive default. The interlock's whole job is to refuse when
    // readiness has not been established, and "we could not tell" is not
    // establishment. A pane's text cannot say who has the keyboard; see the
    // note above `recover_composer`.
    let (pane, keyboard) = match look_at_pane(&args.tmux_session) {
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

    // ESC has no literal-send story that survives every tmux version; the named
    // key does.
    let typed = if text == "\u{1b}" {
        tmux::send_key(&args.tmux_session, "Escape")
    } else {
        tmux::send_literal(&args.tmux_session, text)
    };
    if let Err(err) = typed {
        return SupervisorResult::Error {
            message: format!("{err:#}"),
        };
    }

    if submit {
        // Claude's composer needs a beat between the text landing and Enter, or
        // the newline can be swallowed by the render pass.
        std::thread::sleep(Duration::from_millis(config.send_keys_delay_ms));
        if let Err(err) = tmux::send_key(&args.tmux_session, "Enter") {
            return SupervisorResult::Error {
                message: format!("{err:#}"),
            };
        }
    }

    log_line(
        args,
        &format!("typed {} byte(s) after matching {matched:?}", text.len()),
    );
    if recovery.enabled && submit {
        return recover_composer(args, require, &recovery, text, matched);
    }
    SupervisorResult::Sent { matched }
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
    composer: &PromptPresence,
    recovery: &Recovery,
    text: &str,
    matched: String,
) -> SupervisorResult {
    let session = args.tmux_session.clone();
    let outcome = recover_composer_with(
        // One look is a pane *and* the cursor, taken together: apart, they
        // can describe two different moments, and the whole question is
        // whether this drawn composer has the keyboard.
        &|| look_at_pane(&session),
        &|key| tmux::send_key(&session, key),
        composer,
        recovery.asking.clone(),
        matched,
        recovery.capture,
        recovery.confirm.clone(),
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
        SupervisorResult::RecoveryUnconfirmed { reason, .. } => log_line(
            args,
            &format!("recovery: nothing claimed after {text:?}; {reason}"),
        ),
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
fn recover_composer_with(
    look: &dyn Fn() -> Result<(String, bool)>,
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
    // The operator's definition of a prompt, when they have one.
    asking: Option<PromptPresence>,
    matched: String,
    capture: bool,
    // Complete the view rather than dismiss it, and what must be on screen for
    // that to be allowed. `Enter` commits, so it is held to a standard the
    // dismissing path is not: the screen must show the selected affirmative row
    // for the value that was asked for, and must not have moved since it
    // appeared. When it cannot, the ordinary rescue runs instead.
    confirm: Option<String>,
) -> SupervisorResult {
    // "Ready" means able to take keys, not merely drawn: a view rendered above
    // a live-looking composer is the measured lockout this postcondition
    // exists to clear, and it must not read as recovery already having
    // happened. See the note above `recover_composer`.
    let ready = |pane: &str, cursor: bool| composer.find_match(pane, None).is_some() && cursor;
    // Nothing was observed yet, so nothing may be claimed. Every early exit
    // below says which look failed, and the daemon turns that into the
    // "typed, outcome unknown" result rather than a claim about a composer
    // nobody managed to see.
    let unconfirmed = |stage: &str| SupervisorResult::RecoveryUnconfirmed {
        matched: matched.clone(),
        reason: format!("the pane could not be read {stage}"),
    };

    // First look. A composer still here settles it — the overwhelmingly
    // common case, and it costs one look.
    //
    // The pane is **kept** rather than discarded: a confirming key is only
    // allowed on a screen that has been the same screen since it appeared, and
    // this is when it appeared. Taking a second look for that would buy the
    // same frame at the price of another failure point on a path that never
    // confirms anything.
    std::thread::sleep(RECOVERY_FIRST_CHECK);
    let first_absent = match look() {
        Ok((pane, cursor)) if ready(&pane, cursor) => return SupervisorResult::Sent { matched },
        Ok((pane, _)) => pane,
        Err(_) => return unconfirmed("after the command was typed"),
    };

    // Second look: a large inline render can hide the composer footer for a
    // while and then bring it back with nobody's help — measured on
    // `/context`, absent about 2.5s and self-restoring. Escaping into that
    // would be a rescue nothing needed.
    std::thread::sleep(RECOVERY_SECOND_CHECK - RECOVERY_FIRST_CHECK);
    let absent_pane = match look() {
        Ok((pane, cursor)) if ready(&pane, cursor) => return SupervisorResult::Sent { matched },
        Ok((pane, _)) => pane,
        Err(_) => return unconfirmed("while checking whether a view had opened"),
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
    // **One look, pane and cursor together.** Apart they describe two moments,
    // and the whole question is what this key will land on.
    let (before_key, cursor_before_key) = match look() {
        Ok(look) => look,
        Err(_) => return unconfirmed("just before the key was sent"),
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
                // A composer with the keyboard means the view is already gone.
                && !cursor_before_key;
            authorised
        }
    };

    let key = if confirming { "Enter" } else { "Escape" };
    if send_key(key).is_err() {
        return SupervisorResult::RecoveryUnconfirmed {
            matched,
            reason: format!("{key} could not be sent"),
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
    loop {
        if let Ok((pane, cursor)) = look() {
            saw_the_pane = true;
            if ready(&pane, cursor) {
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
            return if confirming {
                unconfirmed("while waiting for the composer after the confirmation")
            } else if saw_the_pane {
                SupervisorResult::ComposerLost { matched }
            } else {
                unconfirmed("after the key was sent")
            };
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// Why the composer's readiness is asked of the cursor and not of the pane.
//
// **Measured, and the reason this note exists.** Submitting `/status` while a
// turn is running opens the Settings view *inline*: when the turn finishes
// Claude redraws the transcript, the view, and — below it — the composer box
// and its footer. The composer-presence needle matches. The composer takes no
// keys: typed text never appears and Enter does nothing. A send authorised on
// that pane types into the void and reports success, and a recovery check that
// believed the needle would see a healthy composer and decline the Escape that
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
// See [`tmux::cursor_is_visible`].

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
    cursor_visible: bool,
) -> std::result::Result<String, String> {
    let Some(matched) = require.find_match(pane, None) else {
        return Err(format!(
            "expected prompt not on screen (looked for {:?}); nothing was typed",
            require.default_needles()
        ));
    };
    // Only the composer. A permission prompt is a view too — it hides the
    // cursor exactly as the others do — and answering one is the whole point
    // of this path, so the keyboard question is asked only where a *drawn*
    // target can turn out to be a dead one.
    if targets_composer && !cursor_visible {
        return Err(
            "a view on the Mac has the keyboard; the composer is drawn but takes \
             no keys, so nothing was typed"
                .into(),
        );
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
    if let Err(err) = send(&writer, &ClientFrame::Register(registration.clone())) {
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
        started_at: started_at.to_string(),
        // What this build can honour. The daemon uses it to decide whether an
        // approval may be actuated through us at all: a supervisor that
        // silently ignores the prompt fingerprint must not be handed one.
        protocol_minor: protocol::PROTOCOL_MINOR,
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
    use protocol::ipc::prompt_fingerprint;
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    /// Verbatim from `tmux -L codeconnect capture-pane -p -J` against a live
    /// permission prompt on claude 2.1.220.
    fn permission_pane(command: &str) -> String {
        format!(
            " Bash command\n   {command}\n   Create empty file\n\n Do you want to proceed?\n \
             ❯ 1. Yes\n   2. Yes, and always allow access to tmp/ from this project\n   3. No\n \
             Esc to cancel · Tab to amend · ctrl+e to explain"
        )
    }

    const IDLE_PANE: &str = "\
❯
────────────────────────────────────────
  ⏸ manual mode on · ? for shortcuts · ← for agents                    ● high · /effort";

    /// The measured lockout in disguise: `/status` submitted during a turn,
    /// captured after the turn finished. Claude redraws the transcript, the
    /// Settings view, and — below it — a composer box and footer that match
    /// the presence needle and accept no keys.
    const VIEW_ABOVE_COMPOSER_PANE: &str = "\
⏺ line 1
✻ Sautéed for 6s
────────────────────────────────────────
  Settings  Status   Config   Usage   Stats
  Version:          2.1.222
  Session kind:     interactive
  Esc to cancel
────────────────────────────────────────
❯
────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents";

    /// A live composer during a turn — the state the rule must NOT catch.
    /// Its footer offers `esc to interrupt`, which is a different offer:
    /// the composer is taking keys and queues them for the next turn.
    const MID_TURN_PANE: &str = "\
⏺ working…
────────────────────────────────────────
❯
────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt · ← for agents";

    // ------------------------------------------------- live tmux recovery

    /// A fake TUI with the two behaviours that matter: it shows the
    /// composer needle, and a line beginning `/dialog` hides the needle
    /// until Escape arrives (`/stuck` never releases it). Real tmux, real
    /// keys, real captures — the supervisor's own code path — on a server
    /// this test creates and destroys itself.
    struct FakeTui {
        socket_dir: std::path::PathBuf,
        session: String,
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
            let status = Command::new(&tmux)
                .args(["-S", root.join("sock").to_str()?])
                .args(["new-session", "-d", "-s", &session, "-x", "80", "-y", "24"])
                .arg("--")
                .arg(&script)
                .stdin(Stdio::null())
                .status()
                .ok()?;
            if !status.success() {
                return None;
            }
            let tui = FakeTui {
                socket_dir: root,
                session,
            };
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
            let out = Command::new(protocol::tmux::tmux_bin()?)
                .args(["-S", &self.socket()])
                .args(args)
                .output()
                .ok()?;
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }

        fn pane(&self) -> String {
            self.tmux(&["capture-pane", "-p", "-t", &self.session])
                .unwrap_or_default()
        }

        fn type_line(&self, text: &str) {
            let _ = self.tmux(&["send-keys", "-t", &self.session, "-l", "--", text]);
            let _ = self.tmux(&["send-keys", "-t", &self.session, "Enter"]);
        }

        /// The same `#{cursor_flag}` the supervisor reads on the real server.
        fn cursor_is_visible(&self) -> bool {
            self.tmux(&["display", "-p", "-t", &self.session, "#{cursor_flag}"])
                .map(|out| out.trim() == "1")
                .unwrap_or(true)
        }

        fn send_escape(&self) {
            let _ = self.tmux(&["send-keys", "-t", &self.session, "Escape"]);
        }
    }

    impl Drop for FakeTui {
        fn drop(&mut self) {
            // Only ever this test's own socket — never the shared server.
            let _ = self.tmux(&["kill-server"]);
            let _ = std::fs::remove_dir_all(&self.socket_dir);
        }
    }

    const FAKE_TUI: &str = r#"#!/bin/sh
# A composer that a "/dialog" line replaces with a view, restored by Escape.
# `stty raw` so Escape arrives as a byte we can read.
stty raw -echo 2>/dev/null
# `\033[?25l` / `\033[?25h` are DECTCEM — the same hide/show every TUI uses,
# and what tmux reports as `#{cursor_flag}`.
show_composer() { printf '\033[?25h\033[2J\033[H> \r\n  ? for shortcuts\r\n'; }
show_view()     { printf '\033[?25l\033[2J\033[HTHE VIEW IS UP\r\n'; }
# The measured lockout in disguise: the view drawn ABOVE a composer whose
# needle matches and which takes no keys. Only Escape clears it.
show_view_over_composer() {
  printf '\033[?25l\033[2J\033[HTHE VIEW IS UP\r\nEsc to cancel\r\n> \r\n  ? for shortcuts\r\n'
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
      *)        show_composer; state=composer ;;
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
            &|| Ok((tui.pane(), tui.cursor_is_visible())),
            &|key| {
                assert_eq!(
                    key, "Escape",
                    "the ordinary rescue dismisses, never commits"
                );
                tui.send_escape();
                Ok(())
            },
            &PromptPresence::InputBox,
            None,
            "forshortcuts".to_string(),
            capture,
            None,
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
    fn confirm_probe(looks: Vec<(String, bool)>) -> (SupervisorResult, Vec<String>) {
        use std::sync::Mutex;
        let index = Mutex::new(0usize);
        let sent: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let outcome = recover_composer_with(
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
            None,
            "forshortcuts".to_string(),
            false,
            Some(SONNET_NEEDLE.to_string()),
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
        let (outcome, keys) = confirm_probe(vec![(other, false)]);
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
            (moved, false),
            (CONFIRM_PANE.to_string(), false),
            (CONFIRM_PANE.to_string(), false),
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
            (CONFIRM_PANE.to_string(), false),
            (CONFIRM_PANE.to_string(), false),
            // Same frame, but the cursor is back.
            (CONFIRM_PANE.to_string(), true),
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
            (CONFIRM_PANE.to_string(), false),
            (CONFIRM_PANE.to_string(), false),
            (CONFIRM_PANE.to_string(), false),
            ("? for shortcuts".to_string(), true),
        ]);
        assert_eq!(confirmed, ["Enter"], "a established confirmation commits");

        use std::sync::Mutex;
        let sent: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let _ = recover_composer_with(
            &|| Ok(("some view".to_string(), false)),
            &|key| {
                sent.lock().unwrap().push(key.to_string());
                Ok(())
            },
            &PromptPresence::InputBox,
            None,
            "forshortcuts".to_string(),
            false,
            None,
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
                        false,
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
                        false,
                    )),
                }
            },
            &|key| {
                sent.lock().unwrap().push(key.to_string());
                Ok(())
            },
            &PromptPresence::InputBox,
            None,
            "forshortcuts".to_string(),
            false,
            Some("❯1.yes,switchtosonnet5".to_string()),
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
            &|| {
                // The same dialog, unmoved, at every look; the composer is back
                // once the key has been sent.
                if !sent.lock().unwrap().is_empty() {
                    return Ok(("? for shortcuts".to_string(), true));
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
                    false,
                ))
            },
            &|key| {
                sent.lock().unwrap().push(key.to_string());
                Ok(())
            },
            &PromptPresence::InputBox,
            None,
            "forshortcuts".to_string(),
            false,
            Some("❯1.yes,switchtosonnet5".to_string()),
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
                assert_eq!(matched, "forshortcuts");
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
            &|| {
                // Present at the first look so typing is authorised, then
                // blind: the composer went away and nothing after that could
                // be observed.
                use std::sync::atomic::{AtomicU32, Ordering};
                static LOOKS: AtomicU32 = AtomicU32::new(0);
                // Three successful looks — the two scheduled checks and the
                // re-read taken immediately before the key — then blind.
                match LOOKS.fetch_add(1, Ordering::SeqCst) {
                    0..=2 => Ok(("a view".to_string(), false)),
                    _ => Err(anyhow::anyhow!("the pane could not be read")),
                }
            },
            &|_| Ok(()),
            &PromptPresence::InputBox,
            None,
            "forshortcuts".to_string(),
            false,
            None,
        );
        match outcome {
            SupervisorResult::RecoveryUnconfirmed { reason, .. } => {
                assert!(reason.contains("after the key was sent"), "{reason}");
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
            &|| match LOOKS.fetch_add(1, Ordering::SeqCst) {
                // The two checks see a view and no question…
                0 | 1 => Ok(("THE VIEW IS UP".to_string(), false)),
                // …and by the re-look the tool has asked for permission.
                _ => Ok(("Bash\nDo you want to proceed?\n 1. Yes".to_string(), false)),
            },
            &|_| {
                ESCAPES.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            &PromptPresence::InputBox,
            None,
            "forshortcuts".to_string(),
            false,
            None,
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
            &|| Ok(("Shall I go ahead with this?".to_string(), false)),
            &|_| {
                ESCAPES.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            &PromptPresence::InputBox,
            Some(PromptPresence::AnyOf {
                needles: vec!["shall i go ahead".to_string()],
            }),
            "forshortcuts".to_string(),
            false,
            None,
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

    /// An ordinary line never leaves the composer, so recovery reports the
    /// plain send — no false "recovered" on the common path.
    #[test]
    fn an_inline_line_reports_plain_sent() {
        let Some(tui) = FakeTui::start("inline", FAKE_TUI) else {
            return;
        };
        match recovery_probe(&tui, "hello there", false) {
            SupervisorResult::Sent { matched } => assert_eq!(matched, "forshortcuts"),
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
                false
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
            false,
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
                false
            )
            .is_err());
            assert!(
                authorise(pane, &PromptPresence::PermissionPrompt, None, false, false).is_err()
            );
        }
    }

    #[test]
    fn free_text_is_authorised_by_the_composer_and_not_by_a_prompt() {
        // A takeover is not an answer to a prompt, so it carries no fingerprint
        // — but it must still be refused while a prompt is up, or "yes please
        // continue" gets typed at a permission prompt as the literal answer.
        assert!(authorise(IDLE_PANE, &PromptPresence::InputBox, None, true, true).is_ok());
        assert!(authorise(
            &permission_pane("touch /private/tmp/a.txt"),
            &PromptPresence::InputBox,
            None,
            true,
            false
        )
        .is_err());
    }

    /// **The lockout in disguise.** Measured: `/status` submitted during a
    /// turn leaves the Settings view drawn *above* a composer box whose
    /// presence needle matches and which accepts no keys. The needle says
    /// yes; only the hidden cursor says what is true.
    #[test]
    fn a_view_holding_the_keyboard_refuses_the_send_even_with_a_drawn_composer() {
        assert!(
            PromptPresence::InputBox
                .find_match(VIEW_ABOVE_COMPOSER_PANE, None)
                .is_some(),
            "the fixture must match the presence needle, or it proves nothing"
        );
        let refused = authorise(
            VIEW_ABOVE_COMPOSER_PANE,
            &PromptPresence::InputBox,
            None,
            true,
            false,
        )
        .expect_err("a view holding the keyboard must refuse");
        assert!(refused.contains("takes no keys"), "{refused}");
    }

    /// The counterpart: a live composer mid-turn keeps its cursor, queues
    /// what it is given, and must not be refused — including when the pane
    /// happens to contain a view's words. **The pane's text is not the
    /// signal.** A rule that read `Esc to cancel` out of the pane would
    /// refuse every send in this session until an agent's own output
    /// scrolled away, and would Escape a running turn to "rescue" it.
    #[test]
    fn a_live_composer_is_authorised_even_when_the_pane_says_esc_to_cancel() {
        assert!(authorise(MID_TURN_PANE, &PromptPresence::InputBox, None, true, true).is_ok());
        let agent_wrote_it = MID_TURN_PANE.replace(
            "⏺ working…",
            "⏺ The view offers Esc to cancel, so pressing it closes the dialog.",
        );
        assert!(
            authorise(&agent_wrote_it, &PromptPresence::InputBox, None, true, true).is_ok(),
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
            false
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
