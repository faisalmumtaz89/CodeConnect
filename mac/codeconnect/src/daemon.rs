//! Talking to `ccd` over the unix socket.
//!
//! One request, one reply, one connection. The shim is a short-lived process
//! and the daemon may be down at any moment, so every call here is bounded in
//! time and fails with a sentence the operator can act on rather than an
//! io::Error.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use protocol::ipc::{ClientFrame, DaemonFrame};

/// Generous next to a local socket round-trip, but bounded: `codeconnect pair` must fail
/// with a message rather than hang at a terminal the user is waiting at.
const TIMEOUT: Duration = Duration::from_secs(5);

pub fn request(frame: &ClientFrame) -> Result<DaemonFrame> {
    request_within(frame, TIMEOUT)
}

/// What a daemon says about hosting an agent — in four states, not two.
///
/// [`request_within`] cannot answer this question: it turns both "nothing is
/// listening" and "the daemon answered `error`" into one `Err`, and those two
/// have opposite consequences for a launch. A daemon that is **there and says
/// no** must stop the launch; a daemon that is **absent, or that we could not
/// establish anything about**, must not — a session launched while `ccd` is down
/// is a supported state (it registers when the daemon returns), and refusing to
/// start on a hiccup would take that away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSupport {
    /// The daemon answered that it hosts this agent.
    Hosted,
    /// The daemon answered, and the answer was not yes. Carries the sentence to
    /// show the operator — including the case that matters most, a build
    /// predating the agent seam, which cannot decode the question and says so.
    Refused(String),
    /// Nothing is listening on the socket: no daemon is running.
    Absent,
    /// Neither could be established. Carries why.
    Indeterminate(String),
}

/// Ask a daemon whether it hosts `agent`, without mutating anything.
///
/// One `negotiate_support` frame on a fresh connection — the same question the
/// supervisor asks before it introduces a session
/// (`crate::supervisor::withhold_unless_hosted`), and deliberately the same
/// wire, so the launcher cannot form a rosier opinion than the supervisor will.
/// The daemon reads it and answers; nothing is written, no session is touched,
/// and the connection is dropped on the way out.
///
/// Doubt resolves to [`AgentSupport::Indeterminate`], never to `Refused`: the
/// durable safety property — that a Codex registration never reaches a daemon
/// that would file it as a Claude shadow — belongs to the supervisor's withhold,
/// which fails closed on exactly this answer. This probe only decides whether
/// starting the session is worth the operator's time.
pub fn agent_support(agent: &protocol::agent::AgentKind) -> AgentSupport {
    agent_support_on(&protocol::socket_path(), agent)
}

/// The same, against a named socket, so the four answers can be driven against a
/// scripted daemon instead of the one this machine happens to be running.
pub fn agent_support_on(
    socket: &std::path::Path,
    agent: &protocol::agent::AgentKind,
) -> AgentSupport {
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(err) => {
            return match err.kind() {
                // Nothing is listening: either the socket file is not there at
                // all, or it is a stale one no process has bound.
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                    AgentSupport::Absent
                }
                _ => AgentSupport::Indeterminate(format!(
                    "connecting to ccd at {}: {err}",
                    socket.display()
                )),
            };
        }
    };
    let indeterminate = |what: &str, err: std::io::Error| {
        AgentSupport::Indeterminate(format!("{what} ccd's support answer: {err}"))
    };
    if let Err(err) = stream.set_read_timeout(Some(TIMEOUT)) {
        return indeterminate("bounding", err);
    }
    if let Err(err) = stream.set_write_timeout(Some(TIMEOUT)) {
        return indeterminate("bounding", err);
    }
    let frame = ClientFrame::NegotiateSupport {
        agent: agent.clone(),
        agent_version: None,
    };
    let mut line = match serde_json::to_vec(&frame) {
        Ok(line) => line,
        Err(err) => return AgentSupport::Indeterminate(format!("encoding the question: {err}")),
    };
    line.push(b'\n');
    if let Err(err) = stream.write_all(&line).and_then(|()| stream.flush()) {
        return indeterminate("asking for", err);
    }
    let mut response = String::new();
    match BufReader::new(stream).read_line(&mut response) {
        // A daemon that hangs up without answering has told us nothing.
        Ok(0) => {
            return AgentSupport::Indeterminate(
                "ccd closed the connection without answering whether it hosts this agent".into(),
            )
        }
        Ok(_) => {}
        Err(err) => return indeterminate("reading", err),
    }
    match serde_json::from_str::<DaemonFrame>(response.trim()) {
        Ok(DaemonFrame::SupportedAgents {
            supported: true, ..
        }) => AgentSupport::Hosted,
        Ok(DaemonFrame::SupportedAgents {
            supported_agents, ..
        }) => AgentSupport::Refused(format!(
            "the running ccd does not host {}; it hosts {}",
            agent.as_str(),
            supported_agents
                .iter()
                .map(|a| a.as_str().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        // The shape a daemon that predates the agent seam answers with:
        // `negotiate_support` is a `ClientFrame` variant it cannot decode, and
        // unknown variants — unlike unknown fields — fail loudly.
        Ok(DaemonFrame::Error { message }) => AgentSupport::Refused(format!(
            "the running ccd could not read the support negotiation ({message}), so it \
             predates the agent seam and cannot host {}",
            agent.as_str()
        )),
        Ok(other) => AgentSupport::Indeterminate(format!(
            "ccd answered the support negotiation with {other:?}, which says nothing about {}",
            agent.as_str()
        )),
        Err(err) => {
            AgentSupport::Indeterminate(format!("ccd's answer was undecodable ({err}): {response}"))
        }
    }
}

/// The same, for the one request that legitimately takes longer than a
/// round-trip.
///
/// `codeconnect sessions prune` deletes across seven tables in one transaction, and on a
/// machine with a long history that is real work. Timing it out at five seconds
/// would abandon the *reply* while the daemon carried on committing, leaving the
/// operator with no idea whether their history had been removed — which is the
/// one thing this command must never be ambiguous about.
pub fn request_within(frame: &ClientFrame, timeout: Duration) -> Result<DaemonFrame> {
    let socket = protocol::socket_path();
    let mut stream = UnixStream::connect(&socket).with_context(|| {
        format!(
            "cannot reach ccd at {}; is the daemon running?",
            socket.display()
        )
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut line = serde_json::to_vec(frame)?;
    line.push(b'\n');
    stream.write_all(&line).context("writing to ccd")?;
    stream.flush().context("flushing to ccd")?;

    let mut response = String::new();
    let read = BufReader::new(stream)
        .read_line(&mut response)
        .context("reading ccd's reply")?;
    if read == 0 {
        bail!("ccd closed the connection without replying");
    }

    match serde_json::from_str::<DaemonFrame>(response.trim())? {
        // Surfaced here so no caller has to remember to check for it.
        DaemonFrame::Error { message } => bail!("{message}"),
        frame => Ok(frame),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn socket(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cc-preflight-{tag}-{}-{}.sock",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Answer one `negotiate_support` with `answer`, or hang up without one.
    fn scripted_daemon(
        listener: UnixListener,
        answer: Option<&'static str>,
    ) -> std::thread::JoinHandle<Vec<serde_json::Value>> {
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut out = stream.try_clone().expect("clone");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            let mut asked = Vec::new();
            if reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                asked.push(serde_json::from_str(&line).expect("a decodable question"));
            }
            if let Some(answer) = answer {
                let _ = writeln!(out, "{answer}");
                let _ = out.flush();
            }
            asked
        })
    }

    const HOSTED: &str =
        "{\"type\":\"supported_agents\",\"supported_agents\":[\"claude\",\"codex\"],\"supported\":true}";
    const REFUSED: &str =
        "{\"type\":\"supported_agents\",\"supported_agents\":[\"claude\"],\"supported\":false}";
    /// Measured against the real v0.6.0 binary: `negotiate_support` is a
    /// `ClientFrame` variant it cannot decode, and unknown variants fail loudly.
    const V060_ANSWER: &str = "{\"type\":\"error\",\"message\":\"undecodable frame: unknown \
                               variant `negotiate_support`, expected one of `hook`, `register`\"}";

    #[test]
    fn nothing_listening_is_absence_and_never_a_refusal() {
        // The distinction the launch preflight turns on: a daemon that is not
        // running must not stop a launch, because a session started while `ccd`
        // is down registers itself when `ccd` comes back.
        let missing = socket("absent");
        assert_eq!(
            agent_support_on(&missing, &protocol::agent::AgentKind::Codex),
            AgentSupport::Absent
        );
        // A socket file left behind by a dead daemon is the same answer, and it
        // is the shape a crash actually leaves: the inode exists, and connecting
        // to it is refused because nothing is bound.
        let stale = socket("stale");
        std::fs::write(&stale, b"").expect("stage a stale socket path");
        assert!(matches!(
            agent_support_on(&stale, &protocol::agent::AgentKind::Codex),
            AgentSupport::Absent | AgentSupport::Indeterminate(_)
        ));
        let _ = std::fs::remove_file(&stale);
    }

    #[test]
    fn a_daemon_that_hosts_the_agent_is_asked_exactly_once_and_says_so() {
        let path = socket("hosted");
        let listener = UnixListener::bind(&path).expect("bind");
        let daemon = scripted_daemon(listener, Some(HOSTED));
        assert_eq!(
            agent_support_on(&path, &protocol::agent::AgentKind::Codex),
            AgentSupport::Hosted
        );
        let asked = daemon.join().expect("the daemon thread");
        assert_eq!(asked.len(), 1, "one question, one round trip: {asked:?}");
        assert_eq!(asked[0]["type"], "negotiate_support");
        assert_eq!(asked[0]["agent"], "codex");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_daemon_that_cannot_host_the_agent_refuses_however_it_says_it() {
        // Both spellings of no. The second is the one that matters: a daemon
        // rolled back past the agent seam cannot decode the question at all, and
        // answering `error` is exactly how it says it does not host Codex.
        for (tag, answer) in [("refused", REFUSED), ("v060", V060_ANSWER)] {
            let path = socket(tag);
            let listener = UnixListener::bind(&path).expect("bind");
            let daemon = scripted_daemon(listener, Some(answer));
            match agent_support_on(&path, &protocol::agent::AgentKind::Codex) {
                AgentSupport::Refused(why) => assert!(
                    why.contains("codex"),
                    "the refusal must name the agent it is about: {why}"
                ),
                other => panic!("{tag}: a daemon that said no read as {other:?}"),
            }
            let _ = daemon.join();
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn an_answer_that_settles_nothing_is_indeterminate_rather_than_a_refusal() {
        // Fail-closed belongs to the supervisor's withhold, which is what
        // protects the daemon's history. Here the direction is the opposite: a
        // daemon that hung up, or answered something else entirely, has told us
        // nothing about support, and refusing to launch on that would take away
        // a session for a hiccup.
        for (tag, answer) in [
            ("silence", None),
            ("unrelated", Some("{\"type\":\"ack\"}")),
            ("garbage", Some("not json at all")),
        ] {
            let path = socket(tag);
            let listener = UnixListener::bind(&path).expect("bind");
            let daemon = scripted_daemon(listener, answer);
            let verdict = agent_support_on(&path, &protocol::agent::AgentKind::Codex);
            assert!(
                matches!(verdict, AgentSupport::Indeterminate(_)),
                "{tag}: expected indeterminate, got {verdict:?}"
            );
            let _ = daemon.join();
            let _ = std::fs::remove_file(&path);
        }
    }
}
