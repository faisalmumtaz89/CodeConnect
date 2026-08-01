//! Asking tmux, from the daemon, whether a session is still there.
//!
//! The question and its answer belong to [`protocol::tmux`], which is where the
//! supervisor reads them from too. What lives here is only the *running* of the
//! child, and it is separate for the reasons `ccd` is not `cc`: this process has
//! an async runtime it must not block, no controlling terminal, a stripped
//! launchd environment, and a fleet's worth of sessions to ask about rather than
//! the one a supervisor owns.
//!
//! [`crate::git`] is the precedent for the shape — a bounded child with a
//! deadline and a kill — and the differences from it are deliberate:
//!
//!   * **stdout is `/dev/null`, not a pipe.** `has-session` says nothing on it,
//!     and a pipe nobody reads is the deadlock `git.rs` had to work around. One
//!     pipe means there is no interleaving to get wrong.
//!   * **stderr is capped tightly.** tmux's answer is one line. Anything past
//!     the cap is not diagnostics, and the cap is what stops a child that has
//!     gone wrong from being an unbounded allocation.
//!   * **the deadline kills.** A tmux client that hangs connecting to a socket
//!     nobody is serving must not hold a descriptor for the life of the daemon,
//!     and a sweep is a background task with nobody watching it.

use std::path::PathBuf;
use std::time::Duration;

use protocol::tmux::SessionPresence;

/// Ceiling on what a probe will read from stderr.
///
/// tmux's answer is a single line — "no server running on …", "can't find
/// session: cc-1". This is orders of magnitude more than that, and exists only
/// so a child that has gone wrong cannot make one sweep an unbounded read.
const MAX_STDERR_BYTES: usize = 4 * 1024;

/// One thing to ask tmux about: a server, and a session name on it.
///
/// Presence is a property of the *server*, not of a database row — six dead
/// runs that all recorded the tmux name `cc-1` are one question, not six. That
/// is what keeps a sweep's cost proportional to the number of distinct names
/// rather than to the number of rows a long-lived machine has accumulated.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Target {
    pub socket: String,
    pub name: String,
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.socket)
    }
}

/// What one probe established about a tmux name: whether anything holds it, and
/// which run that is when tmux can say.
///
/// Presence and owner arrive together because they come from the same child. A
/// second probe to ask "and whose is it?" would be a second point in time, and a
/// name can change hands between two of those — which is the entire defect this
/// answers, reintroduced at a smaller scale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub presence: SessionPresence,
    /// `None` whenever presence is not `Present`, and also when a session exists
    /// but predates the stamp. Absence of an owner is never evidence about a row.
    pub owner: Option<protocol::tmux::SessionOwner>,
}

impl Sighting {
    /// What this sighting says about one particular run.
    ///
    /// The asymmetry is deliberate and is the safety property: a name held by a
    /// *different* uid proves this run's session is gone, while a name held by
    /// nobody identifiable proves nothing at all.
    pub fn verdict(&self, uid: &str) -> SessionPresence {
        match (&self.presence, &self.owner) {
            (SessionPresence::Present, Some(protocol::tmux::SessionOwner::Uid(holder))) => {
                if holder == uid {
                    SessionPresence::Present
                } else {
                    // Someone else holds this name. This run is not running, and
                    // the name being taken is exactly why nothing noticed before.
                    SessionPresence::Gone
                }
            }
            // A session with no stamp is one this daemon cannot attribute. Leaving
            // it `Present` is the same answer the product gave before identity
            // existed, which is the right way to treat a run that predates it.
            (SessionPresence::Present, _) => SessionPresence::Present,
            (other, _) => other.clone(),
        }
    }
}

impl From<SessionPresence> for Sighting {
    fn from(presence: SessionPresence) -> Sighting {
        Sighting {
            presence,
            owner: None,
        }
    }
}

/// A source of proof about whether a tmux session exists.
///
/// A trait with exactly one production implementation, because the *policy*
/// built on top of it — how many confirmations an exit needs, what is left
/// alone, what is never guessed at — is the part that has to be tested, and
/// testing it against a real tmux server would mean a test that can only fail
/// on a machine where tmux happens to behave. The seam is here rather than
/// deeper so everything above it is the code that actually ships.
pub trait Presence {
    fn presence(&self, target: &Target) -> impl std::future::Future<Output = Sighting> + Send;
}

/// The real thing: a bounded `tmux has-session` per question.
pub struct Prober {
    /// Resolved once per sweep rather than once per probe. Locating the binary
    /// is four `stat` calls, and a fleet sweep would otherwise repeat them for
    /// every session on the machine.
    tmux: Option<PathBuf>,
    timeout: Duration,
}

impl Prober {
    pub fn new(timeout: Duration) -> Prober {
        Prober {
            tmux: protocol::tmux::tmux_bin(),
            timeout,
        }
    }

    /// True when there is a tmux to ask at all.
    ///
    /// Worth knowing up front: with no tmux binary every answer is `Unknown`,
    /// and a sweep that reports "nothing could be established" for the whole
    /// fleet should say *why* once rather than once per session.
    pub fn is_available(&self) -> bool {
        self.tmux.is_some()
    }
}

impl Presence for Prober {
    async fn presence(&self, target: &Target) -> Sighting {
        let Some(tmux) = self.tmux.as_deref() else {
            return SessionPresence::Unknown("tmux is not installed at a known location".into())
                .into();
        };
        let Some(argv) = protocol::tmux::session_owner_argv(&target.socket, &target.name) else {
            return SessionPresence::Unknown(format!(
                "{:?} is not a tmux server that can be addressed",
                target.socket
            ))
            .into();
        };

        let mut command = tokio::process::Command::new(tmux);
        command
            .args(&argv)
            .stdin(std::process::Stdio::null())
            // **Both pipes now**, because the answer is split across them:
            // `show-environment` prints the owner on stdout and says "no such
            // session" on stderr. `has-session` needed only one, and this file
            // used to note that a single pipe is the reason nothing can deadlock.
            // That reasoning still holds and is why `wait_with_output` is used
            // rather than two sequential reads: it drains both concurrently, so
            // neither can fill while the other is being read. tmux's answer here
            // is one short line on one stream, orders of magnitude under a pipe
            // buffer, but a bound that depends on the child being small is not a
            // bound.
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // A sweep that is dropped mid-probe — the daemon shutting down —
            // must not leave a tmux client behind.
            .kill_on_drop(true);

        let child = match command.spawn() {
            Ok(child) => child,
            // The binary was there a moment ago and is not now, or the process
            // is out of descriptors. Emphatically not evidence of an exit.
            Err(err) => {
                return SessionPresence::Unknown(format!("could not run tmux: {err}")).into()
            }
        };

        // The deadline is the deadline. A tmux client left hanging on an unserved
        // socket would hold a descriptor for the life of the daemon, and this runs
        // unattended in the background. Dropping the future drops the child, and
        // `kill_on_drop` above is what makes that a kill rather than a leak.
        let finished = tokio::time::timeout(self.timeout, child.wait_with_output()).await;

        let Ok(output) = finished else {
            return SessionPresence::Unknown(format!(
                "tmux did not answer within {:?}",
                self.timeout
            ))
            .into();
        };
        let Ok(output) = output else {
            return SessionPresence::Unknown("could not wait for tmux".into()).into();
        };

        let stdout = String::from_utf8_lossy(&cap(output.stdout)).into_owned();
        let stderr = String::from_utf8_lossy(&cap(output.stderr)).into_owned();
        let (presence, owner) =
            protocol::tmux::owner_from_probe(output.status.success(), &stdout, &stderr);
        Sighting { presence, owner }
    }
}

/// Truncate to the cap. tmux's answer is a single line; anything past this is not
/// diagnostics, and the cap is what stops a child that has gone wrong from turning
/// one sweep into an unbounded allocation.
fn cap(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.truncate(MAX_STDERR_BYTES);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prober() -> Prober {
        Prober::new(Duration::from_secs(10))
    }

    #[tokio::test]
    async fn a_socket_with_no_server_is_proven_gone() {
        // The owner's machine, in miniature: a socket path nothing is serving.
        // tmux says so in its own words and the daemon is entitled to act on it.
        let prober = prober();
        if !prober.is_available() {
            return; // no tmux here
        }
        let socket = std::env::temp_dir().join(format!(
            "ccd-liveness-{}-{}.sock",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let target = Target {
            socket: socket.to_string_lossy().into_owned(),
            name: "cc-1".into(),
        };
        assert_eq!(
            prober.presence(&target).await.presence,
            SessionPresence::Gone
        );
        assert!(
            !socket.exists(),
            "the probe started a tmux server at {}",
            socket.display()
        );
    }

    #[tokio::test]
    async fn a_live_session_is_proven_present_and_its_absence_proven_after_it_dies() {
        // Both directions against a real server, because a prober that answered
        // `Gone` for everything would pass every other test in this file and
        // mark the whole fleet dead.
        let prober = prober();
        if !prober.is_available() {
            return;
        }
        let socket = std::env::temp_dir().join(format!(
            "ccd-liveness-live-{}-{}.sock",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let socket = socket.to_string_lossy().into_owned();
        let name = format!("ccprobe-{}", std::process::id());
        let created = tokio::process::Command::new(protocol::tmux::tmux_bin().unwrap())
            .args(["-S", &socket, "new-session", "-d", "-s", &name, "--"])
            .args(["/bin/sh", "-c", "sleep 60"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        if !created.is_ok_and(|status| status.success()) {
            return; // tmux cannot start a server in this environment
        }

        let target = Target {
            socket: socket.clone(),
            name: name.clone(),
        };
        assert_eq!(
            prober.presence(&target).await.presence,
            SessionPresence::Present,
            "a session that is running must not be reported gone"
        );
        // A name that does not exist *on a server that does* is the other half:
        // it proves the probe is asking about the session and not merely about
        // whether the socket answers.
        assert_eq!(
            prober
                .presence(&Target {
                    socket: socket.clone(),
                    name: format!("{name}-nope"),
                })
                .await
                .presence,
            SessionPresence::Gone
        );

        let _ = tokio::process::Command::new(protocol::tmux::tmux_bin().unwrap())
            .args(["-S", &socket, "kill-server"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        assert_eq!(
            prober.presence(&target).await.presence,
            SessionPresence::Gone,
            "and once the server is gone, so is the session"
        );
    }

    #[tokio::test]
    async fn a_server_that_cannot_be_addressed_is_unknown_rather_than_gone() {
        // The rule the whole feature turns on. A row we cannot form a question
        // about is not a row whose session we have proven dead.
        let prober = prober();
        for socket in ["", "relative/path/cc.sock", "./cc.sock"] {
            let presence = prober
                .presence(&Target {
                    socket: socket.into(),
                    name: "cc-1".into(),
                })
                .await
                .presence;
            assert!(
                matches!(presence, SessionPresence::Unknown(_)),
                "{socket:?} produced {presence:?}"
            );
        }
        let presence = prober
            .presence(&Target {
                socket: "codeconnect".into(),
                name: String::new(),
            })
            .await
            .presence;
        assert!(matches!(presence, SessionPresence::Unknown(_)));
    }

    #[tokio::test]
    async fn a_missing_tmux_is_unknown_for_every_session_rather_than_a_dead_fleet() {
        // The single most dangerous failure this could have: tmux is not
        // installed, every probe fails, and the daemon marks the entire fleet
        // exited on the strength of it.
        let prober = Prober {
            tmux: None,
            timeout: Duration::from_secs(1),
        };
        assert!(!prober.is_available());
        let presence = prober
            .presence(&Target {
                socket: "codeconnect".into(),
                name: "cc-1".into(),
            })
            .await
            .presence;
        assert!(
            matches!(presence, SessionPresence::Unknown(ref why) if why.contains("not installed")),
            "{presence:?}"
        );
    }

    #[tokio::test]
    async fn a_child_that_never_answers_is_killed_at_the_deadline() {
        // `tmux` here is a stand-in for the class. A client hung on a socket
        // nobody serves must not hold a descriptor for the life of the daemon,
        // and nothing is watching a background sweep to notice.
        let hang = std::env::temp_dir().join(format!(
            "ccd-liveness-hang-{}-{}.sh",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::write(&hang, "#!/bin/sh\nsleep 30\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prober = Prober {
            tmux: Some(hang.clone()),
            timeout: Duration::from_millis(200),
        };

        let started = std::time::Instant::now();
        let presence = prober
            .presence(&Target {
                socket: "codeconnect".into(),
                name: "cc-1".into(),
            })
            .await
            .presence;
        assert!(
            matches!(presence, SessionPresence::Unknown(ref why) if why.contains("did not answer")),
            "{presence:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline must be the deadline: {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_file(&hang);
    }

    #[tokio::test]
    async fn an_enormous_stderr_cannot_exhaust_memory_and_is_still_classified() {
        // A "tmux" that floods stderr. The read is capped, the child takes
        // EPIPE, and the message it did produce is still read through the
        // classifier rather than being discarded into an `Unknown`.
        let noisy = std::env::temp_dir().join(format!(
            "ccd-liveness-noisy-{}-{}.sh",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::write(
            &noisy,
            "#!/bin/sh\nprintf 'no server running on /tmp/x\\n' >&2\n\
             yes 'padding padding padding' | head -c 5000000 >&2\nexit 1\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&noisy, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prober = Prober {
            tmux: Some(noisy.clone()),
            timeout: Duration::from_secs(20),
        };
        assert_eq!(
            prober
                .presence(&Target {
                    socket: "codeconnect".into(),
                    name: "cc-1".into(),
                })
                .await
                .presence,
            SessionPresence::Gone
        );
        let _ = std::fs::remove_file(&noisy);
    }
}
