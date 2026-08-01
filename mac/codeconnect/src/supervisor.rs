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
            submit,
            expect,
        } => send_text(args, config, &text, &require, submit, expect.as_ref()),
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
fn send_text(
    args: &SupervisorArgs,
    config: &Config,
    text: &str,
    require: &PromptPresence,
    submit: bool,
    expect: Option<&PromptFingerprint>,
) -> SupervisorResult {
    let pane = match tmux::capture_visible_pane(&args.tmux_session) {
        Ok(pane) => pane,
        Err(err) => {
            return SupervisorResult::Refused {
                reason: format!("could not read the pane: {err:#}"),
            }
        }
    };

    let matched = match authorise(&pane, require, expect) {
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
    SupervisorResult::Sent { matched }
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
) -> std::result::Result<String, String> {
    let Some(matched) = require.find_match(pane, None) else {
        return Err(format!(
            "expected prompt not on screen (looked for {:?}); nothing was typed",
            require.default_needles()
        ));
    };
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
            authorise(&mine, &PromptPresence::PermissionPrompt, Some(&expect)).unwrap(),
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
        let refused = authorise(&theirs, &PromptPresence::PermissionPrompt, Some(&expect))
            .expect_err("a different prompt must not be typed into");
        assert!(refused.contains("not the one"), "{refused}");
    }

    #[test]
    fn no_prompt_means_no_keystroke_whatever_else_is_offered() {
        let mine = permission_pane("touch /private/tmp/a.txt");
        let expect = prompt_fingerprint(&mine, "doyouwanttoproceed").unwrap();
        for pane in [IDLE_PANE, "", "some other tmux window entirely\n$ ls"] {
            assert!(authorise(pane, &PromptPresence::PermissionPrompt, Some(&expect)).is_err());
            assert!(authorise(pane, &PromptPresence::PermissionPrompt, None).is_err());
        }
    }

    #[test]
    fn free_text_is_authorised_by_the_composer_and_not_by_a_prompt() {
        // A takeover is not an answer to a prompt, so it carries no fingerprint
        // — but it must still be refused while a prompt is up, or "yes please
        // continue" gets typed at a permission prompt as the literal answer.
        assert!(authorise(IDLE_PANE, &PromptPresence::InputBox, None).is_ok());
        assert!(authorise(
            &permission_pane("touch /private/tmp/a.txt"),
            &PromptPresence::InputBox,
            None
        )
        .is_err());
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
