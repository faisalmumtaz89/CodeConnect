//! Asking tmux whether a session is still there — the one place that decides.
//!
//! Two processes ask this question and they must never answer it differently.
//! `cc`'s supervisor asks about the single session it owns, every two seconds,
//! from a blocking thread in the process that created it. `ccd` asks about every
//! row in the fleet, on a sweep, from a launchd job with a stripped environment
//! and an async runtime it must not block. *How* they run a child therefore
//! differs; *what the answer means* must not, so the argv and the classifier
//! live here and each crate supplies only a way to execute one.
//!
//! The rule the whole module exists to enforce is that only tmux's own words
//! count as evidence. `has-session` exits non-zero for "there is no such
//! session" and for every way it can fail to look, and those are opposite facts:
//! one means the agent is gone, the other means we could not see. A daemon that
//! conflates them reports exits that never happened, which is the same class of
//! lie as a fleet full of sessions it merely never heard the end of.
//!
//! Nothing here may start a tmux server. `has-session` does not carry tmux's
//! `CMD_STARTSERVER` flag, so asking about a dead socket answers rather than
//! resurrecting — and that property is asserted below, against the real binary,
//! because it is a promise about somebody else's program.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// launchd hands a process a minimal environment with no shell PATH, so the
/// binary is located explicitly rather than through `which`.
const TMUX_CANDIDATES: &[&str] = &[
    "/opt/homebrew/bin/tmux",
    "/usr/local/bin/tmux",
    "/usr/bin/tmux",
    "/opt/local/bin/tmux",
];

/// Consecutive "no such session" observations before an exit is believed.
///
/// Two, not one. A tmux server restarting between two looks, or a `has-session`
/// that lost a race with the server's own startup, produces a single `Gone` for
/// a session that is perfectly alive — and a reported exit is a durable fact
/// that cannot be withdrawn. Shared rather than duplicated because the daemon's
/// fleet sweep and the supervisor's own poll are the same policy applied at two
/// timescales, and a machine where they disagreed about how much evidence an
/// exit needs would be a machine where the answer depends on who asked.
pub const EXIT_CONFIRMATIONS: u32 = 2;

/// Whether a session exists, or whether we could not tell.
///
/// Three states, because the two-state version was a lie. `has-session` exiting
/// non-zero used to mean "gone" unconditionally, and the supervisor turned that
/// straight into a `SessionEnd` event — so a tmux binary that could not be
/// reached, a socket whose permissions had changed, a machine that had run out
/// of file descriptors, or a server briefly restarting all produced a *reported
/// agent exit*. The event log's rule is that the daemon never claims what it
/// does not know, and "tmux returned 1" is not knowledge of an exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPresence {
    Present,
    /// tmux said, in its own words, that there is no such session.
    Gone,
    /// We could not establish either. Never treated as an exit.
    Unknown(String),
}

pub fn tmux_bin() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("CODECONNECT_TMUX") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    TMUX_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .or_else(|| search_path("tmux"))
}

pub fn search_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Exact session target. tmux prefix-matches names unless they are anchored
/// with `=`; without this, `cc-1` would happily attach to `cc-12`.
///
/// The anchor does a second job that matters here: a target always begins with
/// `=`, so a session name that begins with `-` can never be parsed by tmux as a
/// flag. There is no shell in this path either — every argument is passed as its
/// own argv entry — so a name is data at every layer.
pub fn target_session(name: &str) -> String {
    format!("={name}")
}

/// How to reach a tmux server, from the `tmux_socket` recorded on a session row.
///
/// tmux itself draws this line and it is not a heuristic: a `-L` *label* is a
/// bare name that tmux resolves under its socket directory and it may not
/// contain `/` (tmux refuses one that does), while `-S` names a socket *path*
/// outright. Anything else — an empty value, or a relative path whose meaning
/// depends on a working directory neither process shares — is not addressable,
/// and the caller must treat that as [`SessionPresence::Unknown`] rather than
/// guessing. A relative path in particular would resolve to *somewhere*, and
/// "no such file" at the wrong place would look exactly like proof of an exit.
fn server_args(socket: &str) -> Option<[String; 2]> {
    if socket.is_empty() {
        return None;
    }
    if !socket.contains('/') {
        return Some(["-L".to_string(), socket.to_string()]);
    }
    Path::new(socket)
        .is_absolute()
        .then(|| ["-S".to_string(), socket.to_string()])
}

/// The complete argv for "does this session exist?", or `None` when the server
/// cannot be addressed at all.
///
/// One builder for both crates, so the daemon and the supervisor cannot end up
/// asking tmux subtly different questions — and so the guarantee that this can
/// never *start* a server is a property of one argv rather than of every call
/// site that happens to spell it out.
pub fn has_session_argv(socket: &str, name: &str) -> Option<Vec<String>> {
    if name.is_empty() {
        return None;
    }
    let [flag, value] = server_args(socket)?;
    Some(vec![
        flag,
        value,
        "has-session".to_string(),
        "-t".to_string(),
        target_session(name),
    ])
}

/// The complete argv for "who holds this name?", or `None` when the server
/// cannot be addressed.
///
/// **Why this exists beside [`has_session_argv`].** `has-session` answers
/// *whether* a name is taken, never *whose* it is, and tmux reuses names — `cc-1`
/// is whatever ran most recently. A daemon holding several rows that all recorded
/// `cc-1` cannot tell them apart from that answer, so one `Present` marked every
/// one of them running, dead ones included. Measured: a run whose session had
/// been killed 452ms earlier still read `live` a minute later, and stayed that
/// way for as long as the name was held.
///
/// The identity needed to separate them is already there. The supervisor puts
/// [`crate::ENV_SESSION_UID`] into the environment it hands `new-session -e`, and
/// tmux keeps it on the session, so asking for that one variable answers both
/// questions at once — presence *and* owner — for the same single child.
pub fn session_owner_argv(socket: &str, name: &str) -> Option<Vec<String>> {
    if name.is_empty() {
        return None;
    }
    let [flag, value] = server_args(socket)?;
    Some(vec![
        flag,
        value,
        "show-environment".to_string(),
        "-t".to_string(),
        target_session(name),
        crate::ENV_SESSION_UID.to_string(),
    ])
}

/// Who tmux says is holding a name, when it says at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOwner {
    /// tmux returned the uid stamped at creation. This is proof of identity.
    Uid(String),
    /// The session is there and carries no stamp: created before this shipped,
    /// or by hand. Never evidence about *which* run it is, and so never grounds
    /// for calling any row dead.
    Unstamped,
}

/// What one `show-environment` run proved: presence, and owner where known.
///
/// The mapping is tmux's actual wording, measured on 3.7b rather than assumed:
///
/// | case | status | stream |
/// |---|---|---|
/// | stamped | 0 | stdout `CODECONNECT_SESSION_UID=<uid>` |
/// | explicitly unset (`-r`) | 0 | stdout `-CODECONNECT_SESSION_UID` |
/// | session exists, no stamp | 1 | stderr `unknown variable: …` |
/// | no such session | 1 | stderr `no such session: =cc-9` |
/// | no server | 1 | stderr `error connecting to … (No such file or directory)` |
///
/// The third row is the one that matters and the one easiest to get wrong:
/// `unknown variable` is tmux confirming the session **exists** and simply has no
/// such variable. Reading that failure as an absence would invent exactly the
/// false exit this whole path is written to avoid.
pub fn owner_from_probe(ok: bool, stdout: &str, stderr: &str) -> (SessionPresence, Option<SessionOwner>) {
    if ok {
        let line = stdout.trim();
        let prefix = format!("{}=", crate::ENV_SESSION_UID);
        if let Some(uid) = line.strip_prefix(&prefix) {
            let uid = uid.trim();
            // A stamp that is present but empty says nothing about identity, and
            // must not be compared against a row's uid as though it did.
            if !uid.is_empty() {
                return (SessionPresence::Present, Some(SessionOwner::Uid(uid.to_string())));
            }
        }
        // `-VAR`, an empty answer, or anything else tmux chose to print: the
        // session answered, so it is there; it just did not identify itself.
        return (SessionPresence::Present, Some(SessionOwner::Unstamped));
    }
    if stderr.trim().to_ascii_lowercase().contains("unknown variable") {
        return (SessionPresence::Present, Some(SessionOwner::Unstamped));
    }
    (classify_absence(stderr), None)
}

/// What one `has-session` run proved, read from its status *and* its stderr.
///
/// The status alone is not enough: it is 1 for "there is no such session" and 1
/// for "the server could not be reached", and only tmux's own message separates
/// them.
pub fn presence_from_probe(ok: bool, stderr: &str) -> SessionPresence {
    if ok {
        return SessionPresence::Present;
    }
    classify_absence(stderr)
}

/// Read tmux's own words for a definite "no such session".
///
/// Matched against the messages tmux emits rather than against the exit code,
/// because the exit code is 1 for every one of them. The list is tmux's actual
/// wording across the versions this ships against; anything unrecognised is
/// `Unknown`, which is the fail-toward-not-claiming direction — an unfamiliar
/// message must not be promoted into a reported exit.
fn classify_absence(stderr: &str) -> SessionPresence {
    let message = stderr.trim().to_ascii_lowercase();
    const GONE: &[&str] = &[
        // `has-session -t cc-1` with the server up and no such session.
        "can't find session",
        "session not found",
        "no such session",
        // The server itself is not running, so no session exists on it. tmux
        // words this several ways depending on version and platform.
        "no server running",
        "failed to connect to server: connection refused",
        "no such file or directory",
    ];
    if GONE.iter().any(|needle| message.contains(needle)) {
        return SessionPresence::Gone;
    }
    if message.is_empty() {
        return SessionPresence::Unknown("tmux failed without saying why".into());
    }
    SessionPresence::Unknown(stderr.trim().to_string())
}

/// Ask, synchronously, whether one session exists on one server.
///
/// For callers with no async runtime — `cc`'s supervisor is a blocking thread by
/// design, and paying for a runtime to wait on a child that answers in
/// milliseconds would be pure startup cost. `ccd` runs the same argv through
/// [`crate::tmux::has_session_argv`] on a bounded async child instead; both
/// funnel their result through [`presence_from_probe`], which is the whole point
/// of this module.
pub fn session_presence_on(socket: &str, name: &str) -> SessionPresence {
    let Some(argv) = has_session_argv(socket, name) else {
        return SessionPresence::Unknown(format!(
            "{socket:?} is not a tmux server that can be addressed, so {name:?} cannot be checked"
        ));
    };
    let Some(bin) = tmux_bin() else {
        return SessionPresence::Unknown("tmux is not installed at a known location".into());
    };
    match Command::new(bin).args(&argv).stdin(Stdio::null()).output() {
        Ok(output) => presence_from_probe(
            output.status.success(),
            &String::from_utf8_lossy(&output.stderr),
        ),
        // tmux could not even be run: the binary moved, or the process is out
        // of descriptors. Certainly not evidence that an agent exited.
        Err(err) => SessionPresence::Unknown(format!("could not run tmux: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_saying_there_is_no_such_session_is_the_only_thing_read_as_gone() {
        // The defect this replaces: *any* non-zero exit from `has-session` was
        // read as absence, and the supervisor turned absence into a durable
        // `SessionEnd`. tmux exits 1 for "no such session" and also for every
        // way it can fail to look — so a moved binary, an unreachable socket, a
        // process out of descriptors, or a server mid-restart all produced a
        // reported agent exit for a session that was still running.
        for gone in [
            "can't find session: cc-1",
            "no server running on /private/tmp/tmux-501/codeconnect",
            "session not found: cc-1",
            "no such session",
            "failed to connect to server: Connection refused",
            "error connecting to /tmp/tmux-501/codeconnect (No such file or directory)",
        ] {
            assert_eq!(
                presence_from_probe(false, gone),
                SessionPresence::Gone,
                "{gone:?} is tmux saying the session is not there"
            );
        }

        for unknown in [
            "permission denied",
            "lost server",
            "server exited unexpectedly",
            "open terminal failed: not a terminal",
            "too many open files",
            // tmux's own refusal of a `-L` label containing a slash. Not
            // evidence about a session at all.
            "socket name /private/tmp/cc-fake.sock contains /",
        ] {
            assert!(
                matches!(
                    presence_from_probe(false, unknown),
                    SessionPresence::Unknown(_)
                ),
                "{unknown:?} must never be promoted into a reported exit"
            );
        }
    }

    #[test]
    fn a_silent_failure_is_unknown_rather_than_gone() {
        // The worst case for the old code: tmux exits non-zero and says
        // nothing. There is no evidence of an exit here at all.
        assert!(matches!(
            presence_from_probe(false, ""),
            SessionPresence::Unknown(_)
        ));
        assert!(matches!(
            presence_from_probe(false, "   \n  "),
            SessionPresence::Unknown(_)
        ));
    }

    #[test]
    fn a_zero_exit_is_presence_whatever_was_printed() {
        // tmux writing a warning to stderr while still succeeding must not be
        // read through the absence table.
        assert_eq!(presence_from_probe(true, ""), SessionPresence::Present);
        assert_eq!(
            presence_from_probe(true, "no server running"),
            SessionPresence::Present
        );
    }

    #[test]
    fn the_probe_can_never_be_the_thing_that_starts_a_server() {
        // The guard rail, asserted on the argv rather than only on behaviour:
        // a daemon that *creates* the tmux server it is asking about would
        // resurrect a dead fleet on every sweep, and would do it silently.
        let argv = has_session_argv("codeconnect", "cc-1").unwrap();
        assert_eq!(argv, ["-L", "codeconnect", "has-session", "-t", "=cc-1"]);
        for starts_a_server in [
            "new-session",
            "new",
            "start-server",
            "start",
            "new-window",
            "source-file",
            "attach-session",
            "attach",
            "run-shell",
        ] {
            assert!(
                !argv.iter().any(|arg| arg == starts_a_server),
                "{starts_a_server} carries tmux's CMD_STARTSERVER flag"
            );
        }
    }

    #[test]
    fn a_socket_path_is_addressed_with_capital_s_and_a_label_with_l() {
        // tmux's own dichotomy. A label may not contain `/`; a path must be
        // named with `-S`. Getting this backwards would make every probe fail
        // with a message that is not about the session at all.
        assert_eq!(
            has_session_argv("/private/tmp/cc.sock", "soak-1").unwrap(),
            ["-S", "/private/tmp/cc.sock", "has-session", "-t", "=soak-1"]
        );
        assert_eq!(
            has_session_argv("codeconnect", "soak-1").unwrap()[0],
            "-L".to_string()
        );
    }

    #[test]
    fn a_server_we_cannot_address_yields_no_argv_at_all() {
        // The direction that matters: a row whose socket we cannot interpret
        // must produce *no question*, so the caller is forced to report
        // `Unknown` rather than receiving a confident answer to a malformed
        // one. A relative path is the sharp case — it would resolve against
        // whatever working directory the asker happens to have, and "no such
        // file" at the wrong place looks exactly like proof of an exit.
        assert!(has_session_argv("", "cc-1").is_none());
        assert!(has_session_argv("relative/dir/cc.sock", "cc-1").is_none());
        assert!(has_session_argv("./cc.sock", "cc-1").is_none());
        assert!(has_session_argv("codeconnect", "").is_none());

        // …and the sync wrapper turns that into `Unknown`, never `Gone`.
        assert!(matches!(
            session_presence_on("relative/cc.sock", "cc-1"),
            SessionPresence::Unknown(_)
        ));
        assert!(matches!(
            session_presence_on("codeconnect", ""),
            SessionPresence::Unknown(_)
        ));
    }

    #[test]
    fn targets_are_anchored_so_a_name_can_be_neither_a_prefix_nor_a_flag() {
        assert_eq!(target_session("cc-1"), "=cc-1");
        // Without the anchor `cc-1` prefix-matches `cc-12`, and a name starting
        // with a dash would be read by tmux as a flag.
        assert!(target_session("-x").starts_with('='));
    }

    #[test]
    fn asking_a_dead_socket_answers_gone_and_leaves_no_server_behind() {
        // The guard rail, against the real binary. `has-session` must not carry
        // tmux's `CMD_STARTSERVER` flag — and that is a promise about somebody
        // else's program, so it is measured rather than assumed. If a tmux
        // release ever changed it, a fleet sweep would quietly spawn one server
        // per dead session it asked about, and the reconciliation built on this
        // would resurrect the very fleet it exists to lay to rest.
        //
        // Addressed by *path* so the assertion can name the exact file that
        // must not appear; the socket form is the only difference from `-L`,
        // and whether a server is started is a property of the subcommand.
        if tmux_bin().is_none() {
            return; // no tmux here; the classifier tests above still hold
        }
        let socket = std::env::temp_dir().join(format!(
            "ccprobe-{}-{}.sock",
            std::process::id(),
            crate::time::now_unix_ms()
        ));
        assert!(!socket.exists(), "the probe socket must not exist yet");

        let presence = session_presence_on(&socket.to_string_lossy(), "cc-1");
        assert_eq!(
            presence,
            SessionPresence::Gone,
            "a socket with no server is tmux saying the session is not there"
        );
        assert!(
            !socket.exists(),
            "asking about {} started a tmux server",
            socket.display()
        );

        // The label form answers the same way. Its socket lives wherever tmux
        // resolves `-L`, which is exactly the path this test cannot name — so
        // only the answer is asserted here, and the file check above is what
        // holds the no-autostart guarantee.
        let label = format!(
            "ccprobe-{}-{}",
            std::process::id(),
            crate::time::now_unix_ms()
        );
        assert_eq!(session_presence_on(&label, "cc-1"), SessionPresence::Gone);
    }

    /// The five answers tmux actually gives, transcribed from a live 3.7b server
    /// rather than imagined. Each was produced by running the command and
    /// recording status, stdout and stderr verbatim.
    #[test]
    fn owner_probe_maps_tmuxs_own_words() {
        // Stamped: the case the whole change exists to reach.
        assert_eq!(
            owner_from_probe(true, "CODECONNECT_SESSION_UID=UID_X\n", ""),
            (
                SessionPresence::Present,
                Some(SessionOwner::Uid("UID_X".into()))
            )
        );
        // Present but never stamped. tmux fails the command; the session is there.
        assert_eq!(
            owner_from_probe(false, "", "unknown variable: CODECONNECT_SESSION_UID\n"),
            (SessionPresence::Present, Some(SessionOwner::Unstamped))
        );
        // Explicitly unset with `set-environment -r`: status 0, `-VAR` on stdout.
        assert_eq!(
            owner_from_probe(true, "-CODECONNECT_SESSION_UID\n", ""),
            (SessionPresence::Present, Some(SessionOwner::Unstamped))
        );
        // A name nothing holds.
        assert_eq!(
            owner_from_probe(false, "", "no such session: =s9\n"),
            (SessionPresence::Gone, None)
        );
        // No server at all, which tmux words as a connection error.
        assert_eq!(
            owner_from_probe(
                false,
                "",
                "error connecting to /private/tmp/tmux-501/b13nosrv (No such file or directory)\n"
            ),
            (SessionPresence::Gone, None)
        );
    }

    /// An unfamiliar failure is never promoted into an absence — the same
    /// fail-toward-not-claiming direction the rest of this module takes.
    #[test]
    fn an_unrecognised_failure_stays_unknown() {
        let (presence, owner) = owner_from_probe(false, "", "tmux: protocol version mismatch");
        assert!(matches!(presence, SessionPresence::Unknown(_)));
        assert_eq!(owner, None);
    }

    /// A stamp that is present but blank identifies nobody. Returning it as a uid
    /// would let an empty string be compared against a row and decide its fate.
    #[test]
    fn an_empty_stamp_identifies_nobody() {
        assert_eq!(
            owner_from_probe(true, "CODECONNECT_SESSION_UID=\n", ""),
            (SessionPresence::Present, Some(SessionOwner::Unstamped))
        );
    }

    /// The owner probe must be as unable to start a server as `has-session` is.
    #[test]
    fn the_owner_probe_addresses_a_server_and_never_starts_one() {
        let argv = session_owner_argv("codeconnect", "cc-1").expect("addressable");
        assert_eq!(argv[0], "-L");
        assert!(argv.contains(&"show-environment".to_string()));
        // Anchored, or `cc-1` would prefix-match `cc-12`.
        assert!(argv.contains(&"=cc-1".to_string()));
        assert!(argv.contains(&crate::ENV_SESSION_UID.to_string()));
        assert_eq!(session_owner_argv("codeconnect", ""), None);
        assert_eq!(session_owner_argv("relative/path", "cc-1"), None);
    }
}
