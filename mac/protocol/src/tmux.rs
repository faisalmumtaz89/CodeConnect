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

use crate::proc_identity::{self, BirthIdentity, Liveness, ProcessIdentity};
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
    /// tmux returned the uid stamped at creation — the routing tag that tells
    /// honest name-reuse apart under trusted same-user tmux state. Not an
    /// ownership credential: a process running as this user can stamp any uid
    /// (see SECURITY.md, "Same-user tmux state is trusted").
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
pub fn owner_from_probe(
    ok: bool,
    stdout: &str,
    stderr: &str,
) -> (SessionPresence, Option<SessionOwner>) {
    if ok {
        let line = stdout.trim();
        let prefix = format!("{}=", crate::ENV_SESSION_UID);
        if let Some(uid) = line.strip_prefix(&prefix) {
            let uid = uid.trim();
            // A stamp that is present but empty says nothing about identity, and
            // must not be compared against a row's uid as though it did.
            if !uid.is_empty() {
                return (
                    SessionPresence::Present,
                    Some(SessionOwner::Uid(uid.to_string())),
                );
            }
        }
        // `-VAR`, an empty answer, or anything else tmux chose to print: the
        // session answered, so it is there; it just did not identify itself.
        return (SessionPresence::Present, Some(SessionOwner::Unstamped));
    }
    if stderr
        .trim()
        .to_ascii_lowercase()
        .contains("unknown variable")
    {
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
    // Bounded, because this probe runs on liveness sweeps: a tmux that stops
    // answering must cost a deadline and read as "could not look", not hold
    // the sweep hostage — and a timeout is *never* evidence of absence.
    match crate::proc::run_deadlined(
        Command::new(bin).args(&argv).stdin(Stdio::null()),
        std::time::Duration::from_secs(1),
    ) {
        Ok(crate::proc::RunOutcome::Completed { status, stderr, .. }) => {
            presence_from_probe(status.success(), &String::from_utf8_lossy(&stderr))
        }
        Ok(crate::proc::RunOutcome::TimedOut { waited }) => SessionPresence::Unknown(format!(
            "tmux did not answer within {}ms",
            waited.as_millis()
        )),
        // tmux could not even be run: the binary moved, or the process is out
        // of descriptors. Certainly not evidence that an agent exited.
        Err(err) => SessionPresence::Unknown(format!("could not run tmux: {err}")),
    }
}

// ------------------------------------------------------------- terminal attach
//
// The daemon opens a live terminal by spawning a disposable `tmux -C attach`
// control-mode client on plain pipes. Two problems have to be solved before a
// byte flows, and both are solved here so the caller (in `ccd`) stays thin:
//
//   1. **Which session?** tmux names are reused (`cc-1` is whatever ran last),
//      so a name is not an identity. The `session_uid` stamped at creation is,
//      and it rides the same atomic `list-sessions` line as the internal id
//      and the server epoch — no second probe, so no window to misbind them.
//   2. **Is it still that session after we attach?** A tmux internal id (`$0`)
//      is unique only for one server's lifetime; a server restart can hand the
//      same `$0` to a different session. So the attach is bound to an epoch
//      tuple captured before, and re-checked — by the spawned client's own pid
//      — after, before any output is streamed or any input accepted.

/// A live session, pinned to the exact server instance it was found on.
///
/// Two sessions with the same `session_id` on two different server lifetimes
/// are not the same session, so the server's identity travels with the
/// session's: a re-check that only compared `session_id` would accept a
/// restart that reused the id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSession {
    /// The `-L name` or `-S path` used to address the server.
    pub socket: String,
    /// tmux's internal, immutable-for-this-server-lifetime id, e.g. `$0`.
    pub session_id: String,
    /// The uid stamped into the session at creation — the routing tag this
    /// resolves by, trusted as same-user tmux state, not an ownership proof.
    pub uid: String,
    /// The server process and when it started: a restart changes these.
    pub server_pid: i64,
    pub server_start_time: i64,
    /// When this session was created; distinguishes a reused id.
    pub session_created: i64,
    /// The tmux **server process's kernel birth identity** (`proc_identity`),
    /// captured at resolve time from `server_pid`. This is what lets destructive
    /// cleanup PROVE — via process identity, not socket reachability — whether
    /// the exact server we resolved against is still alive (a `Killed` requires
    /// it live + the uid absent) or provably dead (a `ServerGone`). A live tmux
    /// server can drop and recreate its socket on `SIGUSR1` (man tmux), so a
    /// socket error is never proof of death; the server's `(pid, birth)` is.
    /// `None` only if the birth could not be read at resolve — a caller needing
    /// the proof then fails closed.
    pub server_birth: Option<BirthIdentity>,
}

/// Why a `session_uid` could not be turned into an attachable session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No live session on the server carries that uid.
    NotHosted,
    /// The uid resolved ambiguously, or the session changed under the attach.
    IdentityMismatch(String),
    /// tmux could not be run or answered indeterminately — never evidence of
    /// absence, so callers must not read it as "gone".
    Unavailable(String),
}

/// The epoch fields, space-separated. The uid is drawn straight from the session
/// environment, so identity, `$N`, and the server epoch all come from the *one*
/// atomic `list-sessions` answer — a name that is renamed between two probes can
/// no longer misbind a uid to a different session.
///
/// **Why a space, and not the unit separator this used to use.** tmux prints a
/// non-control client's command output through its `utf8_sanitize` pass whenever
/// that client's own environment does not declare a UTF-8 locale, and that pass
/// rewrites every byte outside `0x20..=0x7e` to `_`. launchd hands `ccd` no
/// `LANG`, so every installed daemon is exactly that client: the six-field line
/// came back as one `_`-joined token, [`parse_epoch_line`] saw one field instead
/// of six, and [`resolve_owned_session`] answered `NotHosted` for every session
/// on the machine. Measured on tmux 3.7b, the same server, one client per row:
///
/// ```text
/// client env   -F 'A\x1fB|C\tD|E F'  ->  output
/// env -i           (no LANG)             A_B|C_D|E F
/// LANG=…UTF-8                            A\x1fB|C\tD|E F
/// ```
///
/// The mangling is a property of the *client*, not the server — a UTF-8 client
/// reads a bare-environment server's sessions cleanly — which is why `cargo
/// test` never saw it and why the developer's shell never did either.
///
/// A space is the delimiter because it is the widest byte that both regimes pass
/// through untouched, and it is *safe* as a delimiter only because no field can
/// contain one. `#{session_name}` — the sole arbitrary-text field, and, uniquely,
/// the only field no caller reads — is gone rather than escaped. What is left is
/// `$N`, three decimal timestamps, and a stamp.
///
/// **Why the stamp is scrubbed rather than simply read.** The delimiter is only
/// half of a record; the newline that *ends* one is the other half, and the stamp
/// used to be able to carry both. This format read the stamp with `#{E:…}`, whose
/// `E` asks tmux to expand the variable's **value** a second time, as though the
/// value were itself a format. That value is session environment, and any shell
/// inside a live CodeConnect terminal sits on the shared `-L codeconnect` server
/// and may create a session — so an attacker could read a victim's uid out of
/// `list-sessions` and stamp a session of their own with
///
/// ```text
/// Z\n#{session_id} <the victim's uid> #{session_created} #{pid} #{start_time}\nQ
/// ```
///
/// Measured on tmux 3.7b under a UTF-8 client: three sessions answered in *six*
/// lines, and one of them was
///
/// ```text
/// $2 01JQXV9K7B8N4M2P6R3T5W9YQD 1786460275 20752 1786460275
/// ```
///
/// — five fields, a well-shaped id, the victim's uid, and the *attacker* session's
/// own genuine creation time and server epoch, because tmux expanded those three
/// sequences too. It parses, [`resolve_owned_session`] matches it, and it satisfies
/// [`reverify_owned_client`]'s epoch re-check as well, so a phone asking for the
/// victim's terminal is handed the attacker's session — keystrokes included. The
/// trailing newline is the exploit: without it the outer format's own
/// ` #{session_created} #{pid} #{start_time}` lands on the same line, the record is
/// eight fields wide, and nothing parses.
///
/// `#{s/[^0-9A-Za-z]//:…}` is the repair, and it is a repair at the *format*, which
/// is the only place it can be made — a forged line naming a real uid is otherwise
/// byte-identical to that uid's own honest line. tmux applies the substitution to
/// the raw variable and does not rescan the result (measured: a value of
/// `#{session_id}` prints as `sessionid`, never as `$0`), so one pass removes every
/// byte that could be a field delimiter, a record separator, or the start of a
/// format sequence, and leaves a well-formed uid — 26 Crockford Base32 symbols,
/// all alphanumeric — exactly as it was. Three things it must never become:
///
///   * not `…//g:…` — the substitution is already global (`aXbXcXdXe` → `abcde`
///     with no flag) and tmux accepts an **unknown** flag in silence (`s/X//Q:`
///     → `abcde`, status 0), so a flag here is only a way to be wrong quietly;
///   * not `#{s/[^0-9A-Za-z]//:E:…}` — it parses, never errors, and returns
///     empty for every value including a valid ULID: a silent, total outage;
///   * not `[^0-9A-Z]` — [`crate::uid::is_well_formed`] accepts either case, and
///     an upper-case-only class grinds a lowercase-stamped session's uid into
///     rubble (measured: `01jqxv9k7b8n4m2p6r3t5w9yqd` → `01978426359`).
///
/// With the substitution in place the same three sessions answer in three lines,
/// byte-identical under `LANG=en_US.UTF-8` and under `env -i`, and only the
/// honestly-stamped one carries the victim's uid.
fn epoch_fmt() -> String {
    format!(
        "#{{session_id}} #{{s/[^0-9A-Za-z]//:{}}} #{{session_created}} #{{pid}} #{{start_time}}",
        crate::ENV_SESSION_UID
    )
}

fn list_sessions_epoch_argv(socket: &str) -> Option<Vec<String>> {
    let [flag, value] = server_args(socket)?;
    Some(vec![
        flag,
        value,
        "list-sessions".to_string(),
        "-F".to_string(),
        epoch_fmt(),
    ])
}

/// tmux's own shape for a session id: `$` and at least one decimal digit.
///
/// Checked rather than assumed, because a `$N` that came off the wire goes on to
/// be interpolated *unquoted* into the control-mode command lines built by
/// [`seed_query_line`], [`capture_line`] and [`send_keys_line`]. tmux parses
/// those lines itself, so a token carrying a space would become a second
/// argument and one carrying `;` a second command. Constraining the id where it
/// is read is what makes those `format!`s safe by construction.
fn is_session_id(value: &str) -> bool {
    value
        .strip_prefix('$')
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// One `list-sessions` epoch line → (id, uid, created, server_pid, start).
/// `None` for anything that is not exactly five fields with a well-shaped id and
/// a uid that is either empty — an unstamped session, which then matches no
/// requested uid — or a well-formed stamp.
///
/// The field count is exact, no field is trimmed, and the uid is shape-checked;
/// all three are refusals rather than fussiness. The stamp is the one field whose
/// text this process does not author, because a session made by hand can stamp
/// itself anything at all, and "anything at all" is worse than it sounds. A stamp
/// holding the *field delimiter* was always harmless — it splits into a sixth
/// field, the count fails, and the line is refused whole. A stamp holding the
/// *record separator* was not: a newline ends the line, and what follows it is
/// read as a fresh one, so a crafted stamp could emit a second, perfectly
/// well-formed record binding another session's uid to itself. This doc used to
/// claim no crafted stamp could redirect the answer to a different session. That
/// was false, it was false through the newline, and [`epoch_fmt`] now strips
/// every non-alphanumeric byte out of the stamp before the line is ever written,
/// which is the guarantee that actually holds: neither delimiter can reach here.
///
/// The uid check below is the second wall, deliberately not the first. It cannot
/// catch that forgery — a crafted line naming a real victim's ULID is
/// byte-identical to that victim honestly stamping a session, which is exactly
/// why the repair had to be made in the format — but it does mean a stamp shape
/// [`resolve_owned_session`] would refuse to *look up* is also a shape no line can
/// be *read* as carrying. A future edit that loosens the format cannot quietly
/// turn arbitrary session text back into a parsed identity, and the two checks
/// now agree on what a uid is instead of only one of them having an opinion.
fn parse_epoch_line(line: &str) -> Option<(String, String, i64, i64, i64)> {
    let (id, uid, created, pid, start) = parse_epoch_row_structural(line)?;
    // Empty is a real answer (a session nobody stamped); anything else must be a
    // stamp this codebase would be willing to act on, or the line is not read.
    let uid_is_readable = uid.is_empty() || crate::uid::is_well_formed(&uid);
    uid_is_readable.then_some((id, uid, created, pid, start))
}

/// The **structural** half of [`parse_epoch_line`]: exactly five space-separated
/// fields, a valid `$N` id, and numeric created/pid/start. Returns the raw uid
/// field **unvalidated** — it may be empty (unstamped), a well-formed stamp, or a
/// garbage/unreadable one; matching is the caller's job.
///
/// This is the distinction Principle F (finding 11) turns on. `None` here means a
/// **structurally broken** row — the wrong field count, a bad id, or a
/// non-numeric field — which is truncation / an output cap / corruption, i.e.
/// *proof of nothing*, and callers must fail closed on it. A structurally intact
/// row whose uid merely isn't a readable stamp is **not** corruption: it is a
/// real other session with an unreadable (or hand-crafted) stamp, and it must be
/// *skipped by uid mismatch*, never allowed to collapse the whole census to
/// "Unavailable" — otherwise an attacker could deny a victim their own resolve
/// just by creating a session whose stamp is garbage (see the crafted-stamp
/// test).
fn parse_epoch_row_structural(line: &str) -> Option<(String, String, i64, i64, i64)> {
    let mut parts = line.split(' ');
    let id = parts.next()?;
    let uid = parts.next()?;
    let created = parts.next()?.parse().ok()?;
    let pid = parts.next()?.parse().ok()?;
    let start = parts.next()?.parse().ok()?;
    (parts.next().is_none() && is_session_id(id))
        .then(|| (id.to_string(), uid.to_string(), created, pid, start))
}

/// The argv for the daemon's disposable control-mode attach client.
///
/// Deliberately different from the interactive `exec_attach` a human runs at
/// the Mac, and every flag is load-bearing:
///
///   * `-N` — never start a server. A phone attaching to a session that just
///     died must fail, not silently create an empty server.
///   * `-C` — control mode. The client is a text protocol on plain pipes, not
///     a PTY: pane bytes arrive as `%output` lines and input is delivered with
///     `send-keys`, which reaches the pane program only — a phone's keystrokes
///     can never become tmux commands (no prefix key, no `kill-server`, no
///     switching into another session).
///   * `-E` — do not push this client's environment into the session.
///   * `-f ignore-size` — this client's size never governs the shared window,
///     so a small phone cannot shrink a human at the Mac. (When it is the only
///     client, its size governs, which is correct.)
///   * the target is the internal `session_id` (`$N`), never the reused name.
pub fn daemon_attach_argv(socket: &str, session_id: &str) -> Option<Vec<String>> {
    if session_id.is_empty() {
        return None;
    }
    let [flag, value] = server_args(socket)?;
    Some(vec![
        "-N".to_string(),
        "-C".to_string(),
        flag,
        value,
        "attach-session".to_string(),
        "-E".to_string(),
        "-f".to_string(),
        "ignore-size".to_string(),
        "-t".to_string(),
        session_id.to_string(),
    ])
}

/// One line of the control-mode conversation, classified as far as the carrier
/// needs and no further.
#[derive(Debug, PartialEq, Eq)]
pub enum ControlLine {
    /// Pane bytes: `%output %<pane> <escaped>`. The pane id is kept, not
    /// discarded: control mode multiplexes every pane of the session onto one
    /// stream, and the carrier forwards only the currently active pane.
    Output { pane: String, bytes: Vec<u8> },
    /// The active pane changed: `%window-pane-changed @<win> %<pane>`. The
    /// carrier follows it within its window, so its stream is always the live
    /// focused pane — a pane that dies is replaced by tmux and announced here,
    /// never leaving the carrier bound to something gone. The window is kept so
    /// a change in a *different* window can be told apart from a focus move in
    /// ours.
    PaneChanged { window: String, pane: String },
    /// The active window of session `$N` changed: `%session-window-changed
    /// $N @W`. tmux broadcasts this to *every* control client, not only those
    /// on `$N` (measured on 3.7b), so the session id is kept: the carrier acts
    /// on it only for its own session and ignores other sessions' window
    /// switches. For its own, it closes — it serves one window and does not
    /// follow a change of window.
    WindowChanged(String),
    /// The client's attached session: `%session-changed $N <name>`. The first
    /// is the attach; a later one naming a *different* session is an identity
    /// change the carrier refuses rather than silently streams.
    SessionChanged(String),
    /// The client is done: `%exit [reason]`. Nothing follows it.
    Exit,
    /// Every other notification (`%begin`/`%end`, layout and rename chatter):
    /// carried for protocol completeness, not consumed.
    Ignored,
}

/// Classify one control-mode line (without its trailing newline).
///
/// Bytes, not `str`: tmux passes high/UTF-8 bytes through `%output` literally,
/// so a line is not guaranteed to be valid UTF-8. Never fails — an unrecognised
/// or malformed line is [`ControlLine::Ignored`], because a viewer must survive
/// notifications added by future tmux versions.
pub fn parse_control_line(line: &[u8]) -> ControlLine {
    if let Some(rest) = line.strip_prefix(b"%output ") {
        // `%output %<pane> <payload>`: the pane token, then a space, then the
        // escaped bytes (which may themselves be empty).
        let Some(space) = rest.iter().position(|&b| b == b' ') else {
            return ControlLine::Ignored;
        };
        let pane = String::from_utf8_lossy(&rest[..space]).into_owned();
        if !pane.starts_with('%') {
            return ControlLine::Ignored;
        }
        return ControlLine::Output {
            pane,
            bytes: unescape_output(&rest[space + 1..]),
        };
    }
    if line == b"%exit" || line.starts_with(b"%exit ") {
        return ControlLine::Exit;
    }
    if let Some(rest) = line.strip_prefix(b"%window-pane-changed ") {
        // `%window-pane-changed @<win> %<pane>`.
        let mut parts = rest.split(|&b| b == b' ');
        let window = parts.next().unwrap_or_default();
        let pane = parts.next().unwrap_or_default();
        if window.starts_with(b"@") && pane.starts_with(b"%") {
            return ControlLine::PaneChanged {
                window: String::from_utf8_lossy(window).into_owned(),
                pane: String::from_utf8_lossy(pane).into_owned(),
            };
        }
        return ControlLine::Ignored;
    }
    if let Some(rest) = line.strip_prefix(b"%session-window-changed ") {
        let session = rest.split(|&b| b == b' ').next().unwrap_or_default();
        if session.starts_with(b"$") {
            return ControlLine::WindowChanged(String::from_utf8_lossy(session).into_owned());
        }
        return ControlLine::Ignored;
    }
    if let Some(rest) = line.strip_prefix(b"%session-changed ") {
        let id = rest.split(|&b| b == b' ').next().unwrap_or_default();
        return ControlLine::SessionChanged(String::from_utf8_lossy(id).into_owned());
    }
    ControlLine::Ignored
}

/// Undo control-mode output escaping, exactly.
///
/// Measured on tmux 3.7b rather than assumed: control bytes below 0x20 and the
/// backslash itself arrive as `\NNN` (three octal digits, always three); every
/// other byte — including high and multi-byte UTF-8 — is literal. A backslash
/// not followed by three octal digits does not occur in tmux's own output; it
/// is kept literally rather than guessed at.
fn unescape_output(escaped: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(escaped.len());
    let mut i = 0;
    while i < escaped.len() {
        if escaped[i] == b'\\' {
            if let Some(digits) = escaped.get(i + 1..i + 4) {
                if digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                    let value = digits
                        .iter()
                        .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
                    if value <= 0xff {
                        out.push(value as u8);
                        i += 4;
                        continue;
                    }
                }
            }
        }
        out.push(escaped[i]);
        i += 1;
    }
    out
}

/// The control-mode command whose reply names the attached client's current
/// window and active pane, so the carrier can bind them from the client itself
/// rather than a pre-attach probe (which would race the attach) or the first
/// `%output` (which, in a multi-pane session, may be a background pane). The
/// reply is a single `@<win> %<pane>` line inside a `%begin`/`%end` block.
pub fn seed_query_line(session_id: &str) -> String {
    // The format is double-quoted so its space cannot split it into two
    // arguments; tmux, not a shell, parses the quotes. `session_id` is `$N`
    // from tmux's own output, never attacker text.
    format!("display-message -p -t {session_id} \"#{{window_id}} #{{pane_id}}\"\n")
}

/// Parse a seed reply line `@<win> %<pane>` into its two ids, or `None` if the
/// line is not that shape (any other command reply the carrier ignores).
pub fn parse_seed_reply(line: &[u8]) -> Option<(String, String)> {
    let mut parts = line.split(|&b| b == b' ');
    let window = parts.next()?;
    let pane = parts.next()?;
    if parts.next().is_none() && window.starts_with(b"@") && pane.starts_with(b"%") {
        Some((
            String::from_utf8_lossy(window).into_owned(),
            String::from_utf8_lossy(pane).into_owned(),
        ))
    } else {
        None
    }
}

/// The control-mode command that prints one pane's whole visible screen, so an
/// attach paints what the pane already shows instead of a blank viewport that
/// stays blank until the pane next prints. `-p` puts the screen in the
/// command's own reply block and `-e` keeps the colours and attributes. `-q`
/// suppresses one error and only one: the "no alternate screen" that `-a`
/// raises (tmux 3.7b). It quiets nothing about a pane that vanished
/// mid-switch — that arrives as an `%error` closing the reply block, and the
/// carrier's fallback for it is the only handling there is.
///
/// The body shape, measured on tmux 3.7b: one line per visible row, trailing
/// spaces trimmed, empty rows present as empty lines, and — unlike `%output` —
/// **raw** escape bytes, not octal escapes. Command output is not escaped at
/// all, which is why [`reply_block_close`] matches on the `%begin` arguments.
pub fn capture_line(target: &str) -> String {
    format!("capture-pane -peq -t {target}\n")
}

/// The control-mode command whose reply is one pane's identity and cursor
/// position, issued straight after [`capture_line`] for the same target so the
/// painted screen can leave the cursor where the pane actually has it. The
/// reply is a single `%<pane> <x> <y>` line — zero-based, measured on 3.7b — in
/// its own block.
///
/// The pane id is in the format because the target does not prove which pane
/// answered. Measured on 3.7b: `display-message -p` against a composite whose
/// pane has stopped resolving does not fail the way `capture-pane` does — it
/// answers for the *window's active pane*, rc 0, no `%error`. Nothing else in
/// that reply distinguishes it from the one asked for, so the reply names its
/// own pane and the caller compares.
pub fn cursor_query_line(target: &str) -> String {
    // Double-quoted for the same reason as [`seed_query_line`]: tmux, not a
    // shell, parses the quotes, and the spaces inside them cannot split the
    // format into three arguments.
    format!("display-message -p -t {target} \"#{{pane_id}} #{{cursor_x}} #{{cursor_y}}\"\n")
}

/// Parse a cursor reply `%<pane> <x> <y>` into the pane that answered and its
/// zero-based column and row, or `None` if the line is not exactly that shape.
/// `u16` because a pane's geometry is; a value that does not fit is not a
/// position any screen can be painted at.
///
/// The pane is returned as it was written, unjudged: the caller's test is
/// equality with the pane it captured, which is stricter than any shape check
/// here — no token that is not that pane's id can pass it.
pub fn parse_cursor_reply(line: &[u8]) -> Option<(&str, u16, u16)> {
    let text = std::str::from_utf8(line).ok()?;
    let mut parts = text.split(' ');
    let pane = parts.next()?;
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((pane, x, y))
}

/// The three arguments of a `%begin`, if this line opens a command's reply
/// block — `None` for every other line.
pub fn reply_block_open(line: &[u8]) -> Option<&[u8]> {
    line.strip_prefix(b"%begin ")
}

/// The three arguments of a `%end`/`%error`, and whether the command failed, if
/// this line could close a reply block.
///
/// The arguments are returned rather than discarded because they are the only
/// thing that identifies the *matching* close. tmux does not escape command
/// output, so a `capture-pane` reply carries pane content verbatim — a pane
/// displaying the text `%end 1 2 3` puts that line inside the block, and a
/// reader keying on the prefix alone would end the block there (measured on
/// 3.7b). tmux's own wording is "%begin and matching %end or %error have three
/// arguments"; comparing all three is what stops a screen forging the end of
/// its own capture, since the arguments carry the server's command number.
pub fn reply_block_close(line: &[u8]) -> Option<(&[u8], bool)> {
    if let Some(args) = line.strip_prefix(b"%end ") {
        return Some((args, false));
    }
    line.strip_prefix(b"%error ").map(|args| (args, true))
}

/// The control-mode command that types `bytes` into `target` — a scoped
/// `session:window.pane`, so the keys reach exactly the bound pane and no other
/// session. `-H` passes each byte as hex, so the keys are data at every layer:
/// no byte can be read as a key *name*, a flag, or a tmux command.
pub fn send_keys_line(target: &str, bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut line = format!("send-keys -t {target} -H");
    for b in bytes {
        let _ = write!(line, " {b:02x}");
    }
    line.push('\n');
    line
}

/// The control-mode command that sizes this client's viewport. Under
/// `ignore-size` it governs the window only while it is the sole client.
pub fn resize_line(cols: u16, rows: u16) -> String {
    format!("refresh-client -C {cols}x{rows}\n")
}

/// The control-mode command that ends the client and leaves the session alive.
pub const DETACH_LINE: &str = "detach-client\n";

/// The reverify fields: which process, and which session it sits on.
///
/// Space-separated for the reason [`epoch_fmt`] sets out at length — this format
/// carried the same `\x1f` and failed in the same launchd environment, so a fix
/// to one that left the other alone would have moved the outage from "no session
/// resolves" to "no attach verifies". Both fields are locale-proof charsets that
/// need no escaping: `#{client_pid}` is decimal and `#{session_id}` is `$N`.
///
/// Audited for the stamp-forgery class [`epoch_fmt`] describes, and unchanged
/// because it is already immune rather than because it was overlooked. Both
/// fields are authored by tmux itself — `#{client_pid}` is decimal, `#{session_id}`
/// is `$N` — so no attacker-controlled text reaches a `list-clients` line at all,
/// and there is no `#{E:…}` here to expand a value a second time and invent some.
/// The nearest thing to user-supplied text on a client row would be the session's
/// *name*, which is absent from this format and could not carry a record separator
/// even if it were present: tmux refuses to create a session whose name contains a
/// newline (measured on 3.7b — `invalid session name`).
///
/// Public so `ccd`'s terminal tests can assert against *this* string rather than
/// a copy of it. The copy is how one of the two formats would get fixed alone.
pub const CLIENTS_FMT: &str = "#{client_pid} #{session_id}";

fn list_clients_argv(socket: &str) -> Option<Vec<String>> {
    let [flag, value] = server_args(socket)?;
    Some(vec![
        flag,
        value,
        "list-clients".to_string(),
        "-F".to_string(),
        CLIENTS_FMT.to_string(),
    ])
}

/// One [`CLIENTS_FMT`] line → (client pid, the session that client is on).
/// `None` for anything that is not exactly two fields with a well-shaped id.
fn parse_client_line(line: &str) -> Option<(i32, &str)> {
    let mut parts = line.split(' ');
    let pid = parts.next()?.parse().ok()?;
    let session_id = parts.next()?;
    (parts.next().is_none() && is_session_id(session_id)).then_some((pid, session_id))
}

/// Run a bounded tmux probe. The fourth element of the tuple is **truncated** —
/// the capture hit the byte cap and the output is a prefix (finding 3). Census
/// callers that must not silently omit a row treat truncation as Unavailable.
fn run_probe(bin: &Path, argv: &[String]) -> Result<(bool, String, String, bool), String> {
    match crate::proc::run_deadlined(
        Command::new(bin).args(argv).stdin(Stdio::null()),
        std::time::Duration::from_secs(1),
    ) {
        Ok(crate::proc::RunOutcome::Completed {
            status,
            stdout,
            stderr,
            truncated,
        }) => Ok((
            status.success(),
            String::from_utf8_lossy(&stdout).into_owned(),
            String::from_utf8_lossy(&stderr).into_owned(),
            truncated,
        )),
        Ok(crate::proc::RunOutcome::TimedOut { waited }) => Err(format!(
            "tmux did not answer within {}ms",
            waited.as_millis()
        )),
        Err(err) => Err(format!("could not run tmux: {err}")),
    }
}

/// Resolve the one live session carrying `uid`, pinned to its server epoch.
///
/// Bounded (this can run on a phone-driven request). Refuses `NotHosted` when
/// no session carries the uid and `IdentityMismatch` when more than one does —
/// a duplicate stamp is never silently disambiguated.
pub fn resolve_owned_session(socket: &str, uid: &str) -> Result<OwnedSession, ResolveError> {
    // A uid that is not a well-formed stamp can never legitimately name a
    // session. Refusing it here is what stops an empty or malformed request
    // from matching an unstamped (empty-uid) legacy session below.
    if !crate::uid::is_well_formed(uid) {
        return Err(ResolveError::NotHosted);
    }
    let bin = tmux_bin().ok_or_else(|| ResolveError::Unavailable("tmux not found".into()))?;
    let argv = list_sessions_epoch_argv(socket)
        .ok_or_else(|| ResolveError::Unavailable(format!("{socket:?} is not addressable")))?;
    let (ok, stdout, stderr, truncated) =
        run_probe(&bin, &argv).map_err(ResolveError::Unavailable)?;
    let mut session = resolve_from_probe(socket, uid, ok, &stdout, &stderr, truncated)?;
    // Capture the tmux SERVER process's kernel birth identity now, from the same
    // `#{pid}` the census reported (verified against the local binary: `#{pid}`
    // is the server process pid). **Fail closed** if it cannot be read (round-5
    // finding 3): a resolved session without a proven server identity A is not a
    // usable proof, so it is `Unavailable`, never a birth-less "success" that a
    // liveness/attach check would read as `Live`.
    match proc_identity::read_birth_identity(session.server_pid as i32) {
        Some(birth) => {
            session.server_birth = Some(birth);
            Ok(session)
        }
        None => Err(ResolveError::Unavailable(
            "could not read the tmux server's birth identity to prove server A".into(),
        )),
    }
}

/// The pure post-probe half of [`resolve_owned_session`], split out so the
/// fail-closed handling of a failed probe, a **truncated** census (finding 3),
/// and the row-level parse (finding 11) are all directly testable without a live
/// server.
fn resolve_from_probe(
    socket: &str,
    uid: &str,
    ok: bool,
    stdout: &str,
    stderr: &str,
    truncated: bool,
) -> Result<OwnedSession, ResolveError> {
    if !ok {
        // No server / no sessions is "nothing hosts this uid", not an error.
        return match classify_absence(stderr) {
            SessionPresence::Gone => Err(ResolveError::NotHosted),
            other => Err(ResolveError::Unavailable(format!("{other:?}"))),
        };
    }
    if truncated {
        // The census was cut at the byte cap — a target or duplicate row may have
        // been dropped at a boundary, so absence/uniqueness cannot be inferred
        // (Principle F / finding 3). Proof of nothing ⇒ Unavailable, never a
        // false NotHosted or a false unique claimant.
        return Err(ResolveError::Unavailable(
            "tmux session listing was truncated at the capture cap".into(),
        ));
    }
    resolve_uid_from_rows(socket, uid, stdout)
}

/// A `list-sessions` census, carrying the identity of the **server that served
/// it** so every conclusion can be bound to a proven server (findings 1–4). The
/// binding is what makes "our uid is absent" or "the server is gone" a proof
/// rather than an inference from socket reachability — which a `SIGUSR1`
/// socket-recreate, or a *different* server rebinding the socket path, can fake.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Census {
    /// The listing ran (untruncated) and was served by the server whose process
    /// id is `server_pid`; `presence` is the uid's status in its rows. A caller
    /// that needs a *specific* server must still confirm `server_pid` + its birth
    /// match the pinned identity (see [`census_served_by`]).
    Served {
        server_pid: i64,
        presence: RowPresence,
    },
    /// No server answers the socket (a connection error / "no server running").
    /// On its own this is **never** absence — it is only meaningful combined with
    /// a *proven-dead* pinned server.
    NoServer,
    /// The listing could not be trusted: a probe error, a permission / lost-server
    /// error, a truncated listing, or an empty one (a zero-session tmux server
    /// exits, so an empty listing is a rare transient whose server id we cannot
    /// read to bind). **Never** read as absence.
    CannotTell(String),
}

/// The uid's status within a served census's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowPresence {
    Present(Box<OwnedSession>),
    Absent,
    Ambiguous(String),
    Malformed(String),
}

/// The serving server's pid, read off any one parseable row (all rows on one
/// server share `#{pid}`).
fn census_server_pid(stdout: &str) -> Option<i64> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .find_map(|l| parse_epoch_row_structural(l).map(|(_, _, _, pid, _)| pid))
}

/// The pure classifier for a [`Census`] over a probe's `(ok, stderr, stdout,
/// truncated)`. Split out so the socket-loss / truncation / empty-listing / row
/// cases are directly testable.
fn census_from_probe(
    socket: &str,
    uid: &str,
    ok: bool,
    stdout: &str,
    stderr: &str,
    truncated: bool,
) -> Census {
    if !ok {
        return match classify_absence(stderr) {
            // No server on the socket. NOT absence on its own (findings 1–2).
            SessionPresence::Gone => Census::NoServer,
            _ => Census::CannotTell(stderr.trim().to_string()),
        };
    }
    if truncated {
        return Census::CannotTell("the tmux listing was truncated".into());
    }
    let Some(server_pid) = census_server_pid(stdout) else {
        return Census::CannotTell("the tmux listing was empty; server identity unknown".into());
    };
    let presence = match resolve_uid_from_rows(socket, uid, stdout) {
        Ok(session) => RowPresence::Present(Box::new(session)),
        Err(ResolveError::NotHosted) => RowPresence::Absent,
        Err(ResolveError::IdentityMismatch(why)) => RowPresence::Ambiguous(why),
        Err(ResolveError::Unavailable(why)) => RowPresence::Malformed(why),
    };
    Census::Served {
        server_pid,
        presence,
    }
}

/// Run a bounded census of `uid` on `socket`.
fn census(socket: &str, uid: &str) -> Census {
    let Some(bin) = tmux_bin() else {
        return Census::CannotTell("tmux not found".into());
    };
    let Some(argv) = list_sessions_epoch_argv(socket) else {
        return Census::CannotTell(format!("{socket:?} is not addressable"));
    };
    match run_probe(&bin, &argv) {
        Ok((ok, stdout, stderr, truncated)) => {
            census_from_probe(socket, uid, ok, &stdout, &stderr, truncated)
        }
        Err(why) => Census::CannotTell(why),
    }
}

/// Whether a served census was served by **exactly** the pinned server `expected`
/// — its `#{pid}` equals `expected.pid` AND that pid's *current* kernel birth
/// equals `expected.birth`. The birth read is compared fail-closed, so a pid that
/// exited and was reused in the read gap (its birth now differs, or is
/// unreadable) fails the match rather than letting `A`'s row be mixed with `B`'s
/// identity (finding 4). Returns `false` for a `NoServer`/`CannotTell` census.
fn census_served_by(census: &Census, expected: &ProcessIdentity) -> bool {
    match census {
        Census::Served { server_pid, .. } => {
            *server_pid == expected.pid as i64
                && proc_identity::read_birth_identity(*server_pid as i32) == Some(expected.birth)
        }
        _ => false,
    }
}

/// Resolve `uid` out of a `list-sessions` census body (Principle F / finding 11).
///
/// Pure over the census text so the proof-or-Unknown contract is directly
/// testable. The census is **proof-or-Unknown**: a non-blank row that does not
/// parse is truncation / an output cap / corruption — **proof of nothing**, so
/// it yields [`ResolveError::Unavailable`] ("cannot tell", caller retries),
/// **never** an inferred `NotHosted`/absence. A clean census with no matching row
/// is a real [`ResolveError::NotHosted`]; two matching rows are
/// [`ResolveError::IdentityMismatch`] (never silently disambiguated).
fn resolve_uid_from_rows(
    socket: &str,
    uid: &str,
    stdout: &str,
) -> Result<OwnedSession, ResolveError> {
    let mut found: Option<OwnedSession> = None;
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let Some((session_id, line_uid, session_created, server_pid, server_start_time)) =
            parse_epoch_row_structural(line)
        else {
            // A **structurally broken** non-blank row (wrong field count / bad id
            // / non-numeric field) is truncation / an output cap / corruption —
            // proof of nothing, never inferred absence (Principle F / finding
            // 11). Refusing `Unavailable` here means a uid whose row was cut off
            // reads as "cannot tell", so cleanup retries and the supervisor does
            // not falsely declare its session gone, instead of silently skipping
            // the row and reporting `NotHosted`. (A structurally *intact* row
            // whose uid is merely unreadable is a real other session and falls
            // through to the uid-mismatch skip below — it never collapses the
            // census, so a garbage stamp cannot DoS a victim's resolve.)
            return Err(ResolveError::Unavailable(format!(
                "unparseable session row (truncated or capped output): {line:?}"
            )));
        };
        // Identity and epoch came from the same atomic answer; no second probe,
        // so no name-reassignment window between reading `$N` and the uid. The
        // requested `uid` is well-formed (checked by the caller), so a row whose
        // uid is empty or a non-well-formed/garbage stamp can never equal it and
        // is skipped here.
        if line_uid != uid {
            continue;
        }
        let session = OwnedSession {
            socket: socket.to_string(),
            session_id,
            uid: uid.to_string(),
            server_pid,
            server_start_time,
            session_created,
            // Filled by `resolve_owned_session` (a syscall it does not belong in
            // this pure row parser); `None` here and in the census unit tests.
            server_birth: None,
        };
        if found.is_some() {
            // Two live sessions claiming one uid: refuse rather than pick.
            return Err(ResolveError::IdentityMismatch(format!(
                "more than one live session carries uid {uid}"
            )));
        }
        found = Some(session);
    }
    found.ok_or(ResolveError::NotHosted)
}

/// Prove the client we spawned (identified by its own pid) is attached to the
/// session we resolved, on the same server epoch — the post-attach half of the
/// bind. Run before a single byte is streamed or accepted.
pub fn reverify_owned_client(client_pid: i32, expected: &OwnedSession) -> Result<(), ResolveError> {
    let bin = tmux_bin().ok_or_else(|| ResolveError::Unavailable("tmux not found".into()))?;

    // The server must still be the same instance: a restart between resolve
    // and attach can hand our `session_id` to a different session.
    let epoch_argv = list_sessions_epoch_argv(&expected.socket)
        .ok_or_else(|| ResolveError::Unavailable("socket not addressable".into()))?;
    let (ok, stdout, stderr, truncated) =
        run_probe(&bin, &epoch_argv).map_err(ResolveError::Unavailable)?;
    if !ok {
        return Err(ResolveError::IdentityMismatch(format!(
            "the session vanished before the attach was verified: {}",
            stderr.trim()
        )));
    }
    if truncated {
        // A truncated listing could omit our row and make `still_there` falsely
        // false — fail closed rather than declare the bind broken (finding 3).
        return Err(ResolveError::Unavailable(
            "tmux session listing was truncated while verifying the attach".into(),
        ));
    }
    let still_there =
        stdout
            .lines()
            .filter_map(parse_epoch_line)
            .any(|(id, uid, created, spid, sstart)| {
                id == expected.session_id
                    && uid == expected.uid
                    && created == expected.session_created
                    && spid == expected.server_pid
                    && sstart == expected.server_start_time
            });
    if !still_there {
        return Err(ResolveError::IdentityMismatch(
            "the session or its server changed during attach".into(),
        ));
    }

    // Bind the server's PROCESS identity — and **fail closed** (round-5 finding
    // 3): the attach is not verified without a proven server A. The pin's birth
    // must be present AND still match the server pid's current kernel birth, so a
    // pid the kernel reused for a different server that rebound the socket path
    // cannot hand our `session_id` to a stranger. A healthy, unchanged server
    // matches (happy path unaffected); a missing pin birth or a pid-reuse refuses.
    let Some(expected_birth) = expected.server_birth else {
        return Err(ResolveError::Unavailable(
            "the session pin has no proven server birth; refusing to verify the attach".into(),
        ));
    };
    if proc_identity::read_birth_identity(expected.server_pid as i32) != Some(expected_birth) {
        return Err(ResolveError::IdentityMismatch(
            "the tmux server's process identity changed during attach (pid reuse)".into(),
        ));
    }

    // And our client, by its own pid, must be on that exact session.
    let clients_argv = list_clients_argv(&expected.socket)
        .ok_or_else(|| ResolveError::Unavailable("socket not addressable".into()))?;
    let (c_ok, c_out, _c_err, c_truncated) =
        run_probe(&bin, &clients_argv).map_err(ResolveError::Unavailable)?;
    if !c_ok {
        return Err(ResolveError::IdentityMismatch(
            "could not list clients".into(),
        ));
    }
    if c_truncated {
        // A truncated client listing could omit our client row and make the bind
        // read as broken — fail closed (finding 3).
        return Err(ResolveError::Unavailable(
            "tmux client listing was truncated while verifying the attach".into(),
        ));
    }
    let on_session = c_out
        .lines()
        .filter_map(parse_client_line)
        .any(|(pid, session_id)| pid == client_pid && session_id == expected.session_id);
    if on_session {
        Ok(())
    } else {
        Err(ResolveError::IdentityMismatch(
            "our client is not attached to the resolved session".into(),
        ))
    }
}

/// The argv for `kill-session` targeting an exact internal id (`$N`). The id is
/// [`is_session_id`]-checked before it reaches here, so interpolating it into
/// the `=`-anchored target is safe by construction.
fn kill_session_id_argv(socket: &str, session_id: &str) -> Option<Vec<String>> {
    let [flag, value] = server_args(socket)?;
    Some(vec![
        flag,
        value,
        "kill-session".to_string(),
        "-t".to_string(),
        session_id.to_string(),
    ])
}

/// The outcome of a UID-guarded destructive tmux operation (kill / rollback).
///
/// The two facts D5/D7 turn on live here: **`Absent` is not `Unavailable`** (a
/// server that cannot be reached is never proof the session is gone), and an
/// **epoch change refuses** rather than acting on a possibly-reused id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// The internal id resolved from the uid was killed (or was already gone by
    /// the time the kill landed — either way the session no longer exists).
    Killed,
    /// No live session carries the uid. After an *indeterminate* `new-session`,
    /// the caller must read this as "not yet", not "done" (D7).
    Absent,
    /// The pinned server epoch no longer matches — a restart or id reuse — so
    /// the operation refuses rather than risk killing a stranger.
    EpochChanged,
    /// The kill ran and the pinned tmux **server process is PROVEN dead** — its
    /// `(pid, birth)` is `proc_identity`-`Gone` (findings 1+2), not merely a
    /// socket that stopped answering (a live server recreates its socket on
    /// `SIGUSR1`). Its session and the server are gone.
    ///
    /// Deliberately **not** [`CleanupOutcome::Killed`]: a restart in the
    /// resolve→kill window could have made the kill a collateral hit on a
    /// stranger's reused id, so we never claim a proven kill of *our* session. But
    /// the cleanup *goal* is met — and, exactly like [`CleanupOutcome::EpochChanged`],
    /// a dead server cannot host a late session, so a caller cleaning up (even
    /// after an indeterminate `new-session`) treats this as done, not a retry.
    ServerGone,
    /// More than one live session claims the uid: refuse rather than pick.
    Ambiguous(String),
    /// tmux could not be run or answered indeterminately. **Never** absence.
    Unavailable(String),
}

/// Destroy the one live session carrying `uid` (D5/D7 destructive contract),
/// epoch-atomic per Principle C / findings 1–2.
pub fn destroy_owned_session(
    socket: &str,
    uid: &str,
    pin: Option<&OwnedSession>,
) -> CleanupOutcome {
    destroy_owned_session_hooked(socket, uid, pin, || {}, || {})
}

/// The implementation, with **two** test seams straddling the two irreducible
/// windows tmux leaves open (it offers no atomic resolve-and-kill):
///
///   * `between_resolve_and_kill` runs before the pre-kill re-resolve, so a
///     restart injected there is caught by that re-resolve — the outer window.
///   * `after_reresolve_before_kill` runs *after* the pre-kill re-resolve and
///     *before* the kill, exercising the true inner window that only the
///     post-kill check can (partly) backstop (finding 2).
///
/// **Proof by process identity, not socket reachability (findings 1+2).** A live
/// tmux server can drop and recreate its socket on `SIGUSR1` (man tmux), so a
/// socket error is never proof the server died; and "kill-session can't find
/// `$N`" is never proof our uid is gone (it may be at `$M` on the same server
/// after a same-uid recreation). Both proofs therefore turn on the **server
/// process's `(pid, birth)`** captured at resolve, checked with
/// [`proc_identity::liveness`], plus a fresh **uid re-resolve**:
///
///   * `Killed` requires the pinned server process still **`Live`** *and* the uid
///     now **absent** from a fresh re-resolve on it — proof the session we killed
///     was ours. `kill-session`'s own stderr is not trusted for this.
///   * `ServerGone` requires the pinned server process be **`Gone`**
///     (proc-identity dead) — the ONLY valid basis. It is terminal (server gone ⇒
///     no late session possible, as `EpochChanged` already reasons), but is not a
///     proven `Killed`.
///   * `Unknown` server liveness (the `SIGUSR1` socket-recreate case and any
///     hiccup) is `Unavailable` — a retry, **never** `ServerGone`.
///
/// The only irreducible residual is a collateral kill of a stranger's reused
/// `$N` in the sub-instruction inner window — reported as a non-success, never a
/// false success.
fn destroy_owned_session_hooked(
    socket: &str,
    uid: &str,
    pin: Option<&OwnedSession>,
    between_resolve_and_kill: impl FnOnce(),
    after_reresolve_before_kill: impl FnOnce(),
) -> CleanupOutcome {
    // --- Establish server A: the identity EVERY conclusion binds to (round-5) ---
    // With the persisted pin, A is the pin's server (pid + proven birth) and the
    // target is the pin's `$N`: cleanup is bound to the EXACT server the launch
    // created, never re-established from whoever owns the socket now — so a
    // different server B rebinding the path cannot fake a proven absence. Without
    // a pin (the late-host / never-created-a-session cases), A is established from
    // the initial resolve.
    let (server_a, target_session_id) =
        match pin {
            Some(p) => {
                let Some(birth) = p.server_birth else {
                    return CleanupOutcome::Unavailable(
                        "the launch-record pin has no proven server birth; cannot bind A".into(),
                    );
                };
                (
                    ProcessIdentity {
                        pid: p.server_pid as i32,
                        birth,
                    },
                    p.session_id.clone(),
                )
            }
            None => match census(socket, uid) {
                Census::Served {
                    server_pid,
                    presence: RowPresence::Present(s),
                } => {
                    let Some(birth) = proc_identity::read_birth_identity(server_pid as i32) else {
                        return CleanupOutcome::Unavailable(
                            "could not read the serving server's birth to bind A".into(),
                        );
                    };
                    (
                        ProcessIdentity {
                            pid: server_pid as i32,
                            birth,
                        },
                        s.session_id,
                    )
                }
                Census::Served {
                    presence: RowPresence::Absent,
                    ..
                } => return CleanupOutcome::Absent,
                Census::Served {
                    presence: RowPresence::Ambiguous(why),
                    ..
                } => return CleanupOutcome::Ambiguous(why),
                Census::Served {
                    presence: RowPresence::Malformed(why),
                    ..
                } => return CleanupOutcome::Unavailable(why),
                Census::NoServer => return CleanupOutcome::Unavailable(
                    "no server answers the socket; cannot confirm our uid is absent (finding 1)"
                        .into(),
                ),
                Census::CannotTell(why) => return CleanupOutcome::Unavailable(why),
            },
        };

    let Some(bin) = tmux_bin() else {
        return CleanupOutcome::Unavailable("tmux not found".into());
    };
    let Some(argv) = kill_session_id_argv(socket, &target_session_id) else {
        return CleanupOutcome::Unavailable(format!("{socket:?} is not addressable"));
    };

    // Outer-window seam.
    between_resolve_and_kill();

    // --- Confirm our session is present on A, bound to A (Principle C) ---
    match confirm_session_on_a(socket, uid, &server_a, &target_session_id) {
        ConfirmA::Present => { /* our session on A: proceed to kill */ }
        ConfirmA::Done(outcome) => return outcome,
    }

    // Inner-window seam.
    after_reresolve_before_kill();

    // Run the kill; its stderr is NOT trusted as proof (the identity proof is).
    let _kill_result = run_probe(&bin, &argv);

    // --- Post-kill proof, bound to A ---
    // We just CONFIRMED our session on A and killed it, so we have positive
    // knowledge here: A dying with no reachable server is our session gone
    // (`ServerGone`, the benign single-session drain). A reachable successor
    // carrying our uid is a survivor ⇒ retry.
    match proc_identity::liveness(&server_a) {
        Liveness::Alive => {
            let post = census(socket, uid);
            if !census_served_by(&post, &server_a) {
                return CleanupOutcome::Unavailable(match post {
                    Census::Served { .. } => {
                        "the socket is now served by a different server; cannot confirm on A".into()
                    }
                    Census::NoServer => {
                        "no server answers A after the kill (socket-recreate window); retry".into()
                    }
                    Census::CannotTell(why) => why,
                });
            }
            match post {
                Census::Served {
                    presence: RowPresence::Absent,
                    ..
                } => CleanupOutcome::Killed,
                Census::Served {
                    presence: RowPresence::Present(_),
                    ..
                } => CleanupOutcome::Unavailable(
                    "the uid still resolves on server A after the kill".into(),
                ),
                Census::Served {
                    presence: RowPresence::Ambiguous(why),
                    ..
                } => CleanupOutcome::Ambiguous(why),
                Census::Served {
                    presence: RowPresence::Malformed(why),
                    ..
                } => CleanupOutcome::Unavailable(why),
                _ => unreachable!("census_served_by guarantees a Served census here"),
            }
        }
        Liveness::Gone => match census(socket, uid) {
            // A dead + no reachable server: we killed our confirmed session on A
            // and A drained ⇒ our session is gone (benign single-session drain).
            Census::NoServer => CleanupOutcome::ServerGone,
            Census::Served {
                presence: RowPresence::Absent,
                ..
            } => CleanupOutcome::ServerGone,
            // A successor carries our uid ⇒ NOT complete (survivor).
            Census::Served {
                presence: RowPresence::Present(_),
                ..
            } => CleanupOutcome::Unavailable(
                "the pinned server is gone but a successor carries our uid; not complete".into(),
            ),
            Census::Served {
                presence: RowPresence::Ambiguous(why),
                ..
            } => CleanupOutcome::Ambiguous(why),
            Census::Served {
                presence: RowPresence::Malformed(why),
                ..
            } => CleanupOutcome::Unavailable(why),
            Census::CannotTell(why) => CleanupOutcome::Unavailable(why),
        },
        Liveness::Unknown => CleanupOutcome::Unavailable(
            "the pinned tmux server process liveness is unknown after the kill".into(),
        ),
    }
}

/// The result of confirming our session is present on server A before the kill.
enum ConfirmA {
    /// Our session (the target `$N`) is present on A — proceed to kill.
    Present,
    /// A terminal or retry outcome: do not kill.
    Done(CleanupOutcome),
}

/// Confirm, **bound to server A**, that the target session still carries our uid
/// on A (round-5 findings 1/4). Refuses to kill on any uncertainty.
fn confirm_session_on_a(
    socket: &str,
    uid: &str,
    server_a: &ProcessIdentity,
    target_session_id: &str,
) -> ConfirmA {
    let c = census(socket, uid);
    if census_served_by(&c, server_a) {
        return match c {
            Census::Served {
                presence: RowPresence::Present(s),
                ..
            } if s.session_id == target_session_id => ConfirmA::Present,
            // Present on A but a DIFFERENT `$N` (recreated): our target is stale ⇒
            // retry so the next pass re-resolves (never complete while it lives).
            Census::Served {
                presence: RowPresence::Present(_),
                ..
            } => ConfirmA::Done(CleanupOutcome::Unavailable(
                "the uid was recreated at a new session id on server A; retry".into(),
            )),
            // Proven absent ON A: our session is gone from the exact server it
            // lived on. Terminal.
            Census::Served {
                presence: RowPresence::Absent,
                ..
            } => ConfirmA::Done(CleanupOutcome::Absent),
            Census::Served {
                presence: RowPresence::Ambiguous(why),
                ..
            } => ConfirmA::Done(CleanupOutcome::Ambiguous(why)),
            Census::Served {
                presence: RowPresence::Malformed(why),
                ..
            } => ConfirmA::Done(CleanupOutcome::Unavailable(why)),
            _ => unreachable!("census_served_by guarantees a Served census"),
        };
    }
    // The socket is NOT served by A. Whether our session is gone turns on A's
    // process liveness — never on socket reachability.
    match proc_identity::liveness(server_a) {
        Liveness::Gone => {
            // A is proven dead ⇒ our session (which lived on A) is gone. But a
            // SUCCESSOR may carry a recreated session with our uid: complete only
            // on a PROVEN absence everywhere; a survivor (reachable) is a retry,
            // and a survivor in its socket-recreation window (unreachable ⇒
            // NoServer) is ALSO a retry — never a false ServerGone (finding 4).
            match census(socket, uid) {
                Census::Served {
                    presence: RowPresence::Absent,
                    ..
                } => ConfirmA::Done(CleanupOutcome::ServerGone),
                Census::Served {
                    presence: RowPresence::Present(_),
                    ..
                } => ConfirmA::Done(CleanupOutcome::Unavailable(
                    "server A is gone but a reachable successor carries our uid; retry".into(),
                )),
                // A dead but no reachable server: we did NOT kill anything this
                // pass, and a successor B may be in its socket-recreation window
                // carrying our uid — cannot prove absence, so retry, never
                // ServerGone (finding 4).
                Census::NoServer => ConfirmA::Done(CleanupOutcome::Unavailable(
                    "server A is gone but no server answers to prove our uid is absent; retry"
                        .into(),
                )),
                Census::Served {
                    presence: RowPresence::Ambiguous(why),
                    ..
                } => ConfirmA::Done(CleanupOutcome::Ambiguous(why)),
                Census::Served {
                    presence: RowPresence::Malformed(why),
                    ..
                } => ConfirmA::Done(CleanupOutcome::Unavailable(why)),
                Census::CannotTell(why) => ConfirmA::Done(CleanupOutcome::Unavailable(why)),
            }
        }
        // A is alive but the socket is answered by a different/absent server (a
        // SIGUSR1 socket-recreate/rebind window), or we cannot tell: retry.
        Liveness::Alive => ConfirmA::Done(CleanupOutcome::Unavailable(
            "server A is alive but the socket is not served by A (recreate window); retry".into(),
        )),
        Liveness::Unknown => ConfirmA::Done(CleanupOutcome::Unavailable(
            "server A's liveness is unknown; retry".into(),
        )),
    }
}

/// A UID-atomic liveness verdict for the supervisor's own session.
///
/// The distinction from a name-addressed `has-session` is the whole point: this
/// resolves by *uid* and pins the epoch, so a session that died and had its
/// `cc-N` name reused by a different run reads `Gone` (owner mismatch), not a
/// false `Live`. A server that cannot be reached is `Unknown` — the supervisor
/// must not tear a live session down on a probe hiccup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedLiveness {
    Live,
    Gone,
    Unknown(String),
}

/// The supervisor's own-session liveness, bound to server identity A (round-5
/// findings 2/3).
///
///   * **Socket loss is `Unknown`, never `Gone`** (finding 2): a `NoServer` /
///     unreachable census must not become a durable session exit — a live server
///     recreates its socket on `SIGUSR1`, and the supervisor must keep observing.
///   * **`Live` fails closed** (finding 3): a session is `Live` only from a
///     successful census whose serving server's **birth is provably readable**
///     (and, when a pin is given, matches A). If the server's identity cannot be
///     proven, that is `Unknown`, never `Live`.
///   * The **healthy path stays `Live`**: a real live session on a reachable
///     server with the uid present (birth proven, pin matching or absent) ⇒
///     `Live`.
///   * A successful census that lacks our uid is a proven `Gone`.
pub fn owned_liveness(socket: &str, uid: &str, pin: Option<&OwnedSession>) -> OwnedLiveness {
    match census(socket, uid) {
        // Socket loss / no server: cannot tell. NEVER a durable Gone (finding 2).
        Census::NoServer => OwnedLiveness::Unknown("no server answers the socket".into()),
        Census::CannotTell(why) => OwnedLiveness::Unknown(why),
        Census::Served {
            server_pid,
            presence,
        } => match presence {
            RowPresence::Present(session) => {
                // Fail closed: prove the serving server's birth (finding 3).
                let Some(birth) = proc_identity::read_birth_identity(server_pid as i32) else {
                    return OwnedLiveness::Unknown(
                        "could not read the serving server's birth; not proven Live".into(),
                    );
                };
                // When pinned to A, the session + full server identity must match
                // (session_id + pid + start_time + created + BIRTH): a same-second
                // pid-reuse or a different server cannot masquerade as ours.
                if let Some(pin) = pin {
                    if session.session_id != pin.session_id
                        || server_pid != pin.server_pid
                        || session.server_start_time != pin.server_start_time
                        || session.session_created != pin.session_created
                        || Some(birth) != pin.server_birth
                    {
                        return OwnedLiveness::Gone;
                    }
                }
                OwnedLiveness::Live
            }
            // A successful listing lacking our uid ⇒ proven gone.
            RowPresence::Absent => OwnedLiveness::Gone,
            // Two claimants / a malformed row is not evidence ours is alive.
            RowPresence::Ambiguous(why) => OwnedLiveness::Unknown(why),
            RowPresence::Malformed(why) => OwnedLiveness::Unknown(why),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real, well-formed ULID, fixed rather than minted so the transcribed
    /// tmux output below can quote it verbatim. It is the *victim's* uid in
    /// every forgery fixture in this module — the identity a crafted stamp
    /// tries to bind to somebody else's session.
    const VICTIM_UID: &str = "01JQXV9K7B8N4M2P6R3T5W9YQD";

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

    #[test]
    fn the_daemon_attach_argv_is_the_disposable_viewport_shape() {
        let argv = daemon_attach_argv("codeconnect", "$0").expect("addressable");
        // -N so a phone attaching to a dead session cannot spin up a server.
        assert_eq!(argv[0], "-N");
        // -C: control mode, so keystrokes are data for the pane and can never
        // become tmux commands.
        assert_eq!(argv[1], "-C");
        // -E (no env push) and -f ignore-size (never resize a human at the Mac).
        assert!(argv.contains(&"-E".to_string()));
        let pos = argv.iter().position(|a| a == "-f").expect("-f");
        assert_eq!(argv[pos + 1], "ignore-size");
        // Targeted by internal id, never a reused name.
        let t = argv.iter().position(|a| a == "-t").expect("-t");
        assert_eq!(argv[t + 1], "$0");
        assert_eq!(daemon_attach_argv("codeconnect", ""), None);
        assert_eq!(daemon_attach_argv("relative/path", "$0"), None);
    }

    #[test]
    fn control_output_unescapes_the_measured_wire_bytes_exactly() {
        // The fixture is tmux 3.7b's actual answer to a pane emitting
        // backslash, "é", "€", a raw 0x80, a tab, "X", and CR — captured over a
        // real control-mode client, not derived from documentation. Control
        // bytes and the backslash arrive as three-octal-digit escapes; UTF-8
        // and high bytes are literal.
        let line = b"%output %3 \\134\xc3\xa9\xe2\x82\xac\x80\\011X\\015\\012";
        assert_eq!(
            parse_control_line(line),
            ControlLine::Output {
                pane: "%3".to_string(),
                bytes: b"\\\xc3\xa9\xe2\x82\xac\x80\tX\r\n".to_vec(),
            }
        );
    }

    #[test]
    fn control_lines_classify_without_ever_failing() {
        assert_eq!(
            parse_control_line(b"%session-changed $7 cc-1"),
            ControlLine::SessionChanged("$7".to_string())
        );
        assert_eq!(
            parse_control_line(b"%window-pane-changed @0 %4"),
            ControlLine::PaneChanged {
                window: "@0".to_string(),
                pane: "%4".to_string()
            }
        );
        assert_eq!(
            parse_control_line(b"%session-window-changed $1 @3"),
            ControlLine::WindowChanged("$1".to_string())
        );
        assert_eq!(parse_control_line(b"%exit"), ControlLine::Exit);
        assert_eq!(parse_control_line(b"%exit detached"), ControlLine::Exit);
        // Chatter the carrier does not consume.
        for line in [
            b"%begin 1786 285 0".as_slice(),
            b"%end 1786 285 0",
            b"%layout-change @0 aa7d,100x40,0,0,0 aa7d,100x40,0,0,0 *",
            b"%window-renamed @0 bash",
            b"%window-pane-changed @0 notapane",
            b"%window-pane-changed notawindow %0",
            b"%some-future-notification with args",
            b"",
        ] {
            assert_eq!(parse_control_line(line), ControlLine::Ignored);
        }
        // Malformed %output shapes are ignored, not panics: no payload space,
        // and a payload whose "pane" token is not a `%id`.
        assert_eq!(parse_control_line(b"%output"), ControlLine::Ignored);
        assert_eq!(parse_control_line(b"%output %0"), ControlLine::Ignored);
        assert_eq!(parse_control_line(b"%output 0 hi"), ControlLine::Ignored);
        // An empty payload is a valid, empty chunk, tagged with its pane.
        assert_eq!(
            parse_control_line(b"%output %0 "),
            ControlLine::Output {
                pane: "%0".to_string(),
                bytes: vec![]
            }
        );
        // A backslash tmux would never emit bare stays literal; a truncated
        // escape at end-of-line survives unmangled.
        assert_eq!(
            parse_control_line(b"%output %0 a\\zb\\01"),
            ControlLine::Output {
                pane: "%0".to_string(),
                bytes: b"a\\zb\\01".to_vec()
            }
        );
    }

    #[test]
    fn keystrokes_become_hex_data_never_names_or_flags() {
        // -H hex: every byte is data. A leading dash, a semicolon (tmux command
        // separator), and a newline are inert.
        assert_eq!(
            send_keys_line("$0", b"-x;\n"),
            "send-keys -t $0 -H 2d 78 3b 0a\n"
        );
        assert_eq!(send_keys_line("$3", b""), "send-keys -t $3 -H\n");
        assert_eq!(resize_line(120, 40), "refresh-client -C 120x40\n");
    }

    #[test]
    fn the_seed_query_and_its_reply_round_trip() {
        assert_eq!(
            seed_query_line("$0"),
            "display-message -p -t $0 \"#{window_id} #{pane_id}\"\n"
        );
        // The exact reply shape measured on tmux 3.7b.
        assert_eq!(
            parse_seed_reply(b"@0 %0"),
            Some(("@0".to_string(), "%0".to_string()))
        );
        assert_eq!(
            parse_seed_reply(b"@7 %12"),
            Some(("@7".to_string(), "%12".to_string()))
        );
        // Anything not `@win %pane` is some other command's reply.
        assert_eq!(parse_seed_reply(b""), None);
        assert_eq!(parse_seed_reply(b"@0"), None);
        assert_eq!(parse_seed_reply(b"0 %0"), None);
        assert_eq!(parse_seed_reply(b"@0 %0 extra"), None);
    }

    #[test]
    fn the_snapshot_queries_and_their_replies_round_trip() {
        assert_eq!(capture_line("$0:@0.%0"), "capture-pane -peq -t $0:@0.%0\n");
        assert_eq!(
            cursor_query_line("$0:@0.%0"),
            "display-message -p -t $0:@0.%0 \"#{pane_id} #{cursor_x} #{cursor_y}\"\n"
        );
        // The exact reply shape measured on tmux 3.7b: the answering pane, then
        // zero-based x then y.
        assert_eq!(parse_cursor_reply(b"%0 8 5"), Some(("%0", 8, 5)));
        assert_eq!(parse_cursor_reply(b"%12 0 0"), Some(("%12", 0, 0)));
        // A reply naming a pane other than the one asked about is still a
        // well-formed reply. Which pane answered is the caller's test — this
        // parser's job is to say what the line means, not whether it is wanted.
        assert_eq!(parse_cursor_reply(b"%7 1 2"), Some(("%7", 1, 2)));
        // What 3.7b really answers for a bare pane id it cannot resolve: one
        // line, all three fields expanded to nothing, and no error.
        assert_eq!(parse_cursor_reply(b"  "), None);
        // Anything not exactly an id and two numbers is some other command's
        // reply, or a position no pane can have. Never a guess.
        assert_eq!(parse_cursor_reply(b""), None);
        assert_eq!(parse_cursor_reply(b"%0 8"), None);
        assert_eq!(parse_cursor_reply(b"%0 8 5 1"), None);
        assert_eq!(parse_cursor_reply(b"%0 8  5"), None);
        assert_eq!(parse_cursor_reply(b"%0 -1 5"), None);
        assert_eq!(parse_cursor_reply(b"%0 65536 5"), None);
        assert_eq!(parse_cursor_reply(b"%0 x y"), None);
        assert_eq!(parse_cursor_reply(b"8 5"), None);
        assert_eq!(parse_cursor_reply(b"can't find pane: %99"), None);
    }

    /// A reply block is delimited by its arguments, not by a prefix — the
    /// property that lets a `capture-pane` body carry pane content verbatim.
    #[test]
    fn a_reply_block_is_closed_only_by_its_own_arguments() {
        assert_eq!(
            reply_block_open(b"%begin 1786439181 299 1"),
            Some(b"1786439181 299 1".as_slice())
        );
        assert_eq!(reply_block_open(b"%beginning"), None);
        assert_eq!(reply_block_open(b"%output %0 hi"), None);

        assert_eq!(
            reply_block_close(b"%end 1786439181 299 1"),
            Some((b"1786439181 299 1".as_slice(), false))
        );
        assert_eq!(
            reply_block_close(b"%error 1786439183 301 1"),
            Some((b"1786439183 301 1".as_slice(), true))
        );
        assert_eq!(reply_block_close(b"%exit"), None);
        assert_eq!(reply_block_close(b"%output %0 hi"), None);

        // The case measured on 3.7b: a pane displaying the text `%end 1 2 3`
        // puts that line inside its own capture reply. It parses as a close —
        // with arguments that are not the open's, which is what keeps the block
        // from ending on content the screen merely shows.
        let opened = reply_block_open(b"%begin 1786439181 299 1").expect("an open");
        let (forged, _) = reply_block_close(b"%end 1 2 3").expect("shaped like a close");
        assert_ne!(forged, opened);
    }

    #[test]
    fn epoch_lines_parse_exactly_or_not_at_all() {
        // A stamp the parser is willing to read has to be a real one, so the
        // positive case uses a real ULID rather than a placeholder. If this
        // fixture ever stops being well-formed, every case below goes vacuous.
        assert!(
            crate::uid::is_well_formed(VICTIM_UID),
            "the fixture stamp must be a well-formed ULID or these cases prove nothing"
        );
        assert_eq!(
            parse_epoch_line(&format!("$0 {VICTIM_UID} 1786 42 1786")),
            Some(("$0".to_string(), VICTIM_UID.to_string(), 1786, 42, 1786))
        );
        // An unstamped session parses with an empty uid — it simply matches no
        // requested uid later. On the wire this is the two adjacent spaces tmux
        // prints for an unset variable, measured on 3.7b both before and after
        // the stamp began going through a substitution.
        assert_eq!(
            parse_epoch_line("$0  1786 42 1786"),
            Some(("$0".to_string(), String::new(), 1786, 42, 1786))
        );
        // `UID-9` used to parse here, and that is precisely what changed: the
        // uid field is now empty or a well-formed stamp and nothing else, so no
        // line can be read as carrying an identity `resolve_owned_session` would
        // then refuse to look up. A shape the two disagreed about was a shape
        // the parser had to be trusted to have handled.
        assert_eq!(parse_epoch_line("$0 UID-9 1786 42 1786"), None);
        assert_eq!(parse_epoch_line("$0 x 1786 42 1786"), None);
        // 25 and 27 symbols, and a symbol Crockford Base32 excludes.
        assert_eq!(
            parse_epoch_line(&format!("$0 {} 1786 42 1786", &VICTIM_UID[1..])),
            None
        );
        assert_eq!(
            parse_epoch_line(&format!("$0 {VICTIM_UID}Z 1786 42 1786")),
            None
        );
        assert_eq!(
            parse_epoch_line(&format!("$0 {}I 1786 42 1786", &VICTIM_UID[1..])),
            None
        );
        // A missing field, a non-numeric field, an extra field: none of these is
        // a session this will act on. Each carries a valid stamp, so each fails
        // for exactly the one reason it is here to pin.
        assert_eq!(parse_epoch_line(&format!("$0 {VICTIM_UID} 1786 42")), None);
        assert_eq!(
            parse_epoch_line(&format!("$0 {VICTIM_UID} x 42 1786")),
            None
        );
        assert_eq!(parse_epoch_line(&format!("$0 {VICTIM_UID} 1 2 3 4")), None);
        // The id field carries the refusal the dropped name field used to: an
        // empty one, and now also one that is not tmux's `$N`. The last two are
        // the ones that matter, because this id is interpolated into a tmux
        // command line by [`capture_line`] and friends.
        assert_eq!(parse_epoch_line(&format!(" {VICTIM_UID} 1 2 3")), None);
        assert_eq!(parse_epoch_line(&format!("cc-1 {VICTIM_UID} 1 2 3")), None);
        assert_eq!(parse_epoch_line(&format!("$ {VICTIM_UID} 1 2 3")), None);
        assert_eq!(parse_epoch_line(&format!("$0; {VICTIM_UID} 1 2 3")), None);
    }

    /// The shipping defect, in one line. Under launchd `ccd` has no `LANG`, so
    /// tmux ran every `-F` answer through `utf8_sanitize` and the `\x1f` this
    /// format used to be delimited with came back as `_`. The parser then saw
    /// one field where it wanted six and [`resolve_owned_session`] answered
    /// `NotHosted` — for *every* session on *every* installed daemon, while
    /// `cargo test`, inheriting a developer's UTF-8 locale, saw nothing wrong.
    ///
    /// The armoured line is fed to the current parser here to pin the property
    /// that actually protects us: it is **refused**, never misread into a
    /// plausible-looking tuple. A line this parser cannot understand must fail
    /// closed, because the alternative — binding a phone's terminal to whatever
    /// session a garbled line happened to name — is worse than not attaching.
    #[test]
    fn the_ascii_armoured_line_from_a_daemon_with_no_locale_is_refused_not_misread() {
        // Verbatim from the gauntlet: the old six-field line as it arrived, with
        // every `\x1f` rewritten to `_`.
        let armoured = "cc-1_$3_01JTESTULIDTESTULIDTEST0_1786439181_98985_1786439100";
        assert_eq!(parse_epoch_line(armoured), None);
        // And the same treatment of the reverify format, which failed identically
        // and would have been the next outage had only the epoch line been fixed.
        assert_eq!(parse_client_line("99476_$0"), None);
    }

    /// The invariant that keeps the defect from coming back: no format the
    /// daemon sends to tmux may contain a byte that `utf8_sanitize` would
    /// rewrite. Measured on tmux 3.7b — that pass keeps `0x20..=0x7e` and turns
    /// everything else, the unit separator and a tab included, into `_`.
    ///
    /// Asserted on the format strings themselves rather than on a captured
    /// answer, so re-introducing a separator fails here rather than only on a
    /// machine that happens to have no locale.
    #[test]
    fn every_daemon_format_survives_a_client_with_no_utf8_locale() {
        for format in [epoch_fmt(), CLIENTS_FMT.to_string()] {
            for byte in format.bytes() {
                assert!(
                    (0x20..=0x7e).contains(&byte),
                    "{format:?} contains {byte:#04x}, which tmux rewrites to `_` \
                     for a client with no UTF-8 locale"
                );
            }
            // A space is the delimiter, so a format that has stopped using it is
            // one field and cannot be a delimited record at all.
            assert!(format.contains(' '), "{format:?} has no delimiter");
        }
        // The dropped field, named so its absence is deliberate rather than
        // incidental: the session name is the only arbitrary text tmux would put
        // on these lines, no caller has ever read it, and a name may contain a
        // space (or anything else) — so it cannot ride a delimited format.
        assert!(!epoch_fmt().contains("session_name"));
    }

    /// The one field whose text this process does not author, and the two things
    /// a hand-made session must never be able to put in it: the field delimiter
    /// and the record separator.
    ///
    /// The delimiter was always harmless — a stamp holding a space splits the
    /// line into a sixth field and the count refuses it. The separator was not.
    /// A newline *ends the record*, so the text after it is read as a fresh line,
    /// and that is how a crafted stamp forged a second well-formed record binding
    /// a victim's uid to the attacker's own session. This test used to be named
    /// for the half that was true and to assert only that half; it now pins both,
    /// and the newline half is pinned where the fix lives — in the format, which
    /// deletes every byte that is not alphanumeric before the line is written.
    #[test]
    fn a_stamp_can_carry_neither_the_field_delimiter_nor_the_record_separator() {
        // The delimiter: a sixth field, so the count fails and the line is
        // refused whole. It never reaches the parser now, but the refusal stays.
        assert_eq!(
            parse_epoch_line(&format!("$0 {VICTIM_UID} 9 1786 42 1786")),
            None
        );

        // The separator, and the delimiter, and every other byte that could
        // punctuate a record or open a format sequence: the class the format
        // strips is the complement of the alphanumerics, so each of these is
        // deleted by construction rather than by hope.
        assert!(
            epoch_fmt().contains(&format!("#{{s/[^0-9A-Za-z]//:{}}}", crate::ENV_SESSION_UID)),
            "the stamp must be read through the sanitizing substitution: {:?}",
            epoch_fmt()
        );
        // Space and newline are the two delimiters; CR and tab are the bytes a
        // sanitizing client would turn into one; `#{`, `}` and `$` are how a
        // format sequence and a session id are written; `;` separates tmux
        // commands. Not one of them is alphanumeric, so not one of them survives.
        for &dangerous in b" \n\r\t#{}$;" {
            assert!(
                !dangerous.is_ascii_alphanumeric(),
                "{:?} survives `[^0-9A-Za-z]`, so the stamp could still carry it",
                dangerous as char
            );
        }

        // And the strip has to be lossless for a real stamp, or the fix would be
        // an outage wearing a fix's clothes. A ULID is 26 Crockford Base32
        // symbols, every one of them alphanumeric, in whichever case it is
        // written — `is_well_formed` accepts both, so both must survive.
        let uid = crate::uid::new().unwrap();
        assert!(crate::uid::is_well_formed(&uid));
        assert!(
            uid.bytes().all(|b| b.is_ascii_alphanumeric()),
            "{uid} would not survive its own sanitization"
        );
        let lowered = uid.to_ascii_lowercase();
        assert!(crate::uid::is_well_formed(&lowered));
        assert!(lowered.bytes().all(|b| b.is_ascii_alphanumeric()));
        // The resolver's own refusal, unchanged: a stamp is never looked up
        // unless it is a ULID, and a ULID has no room for a delimiter in it.
        assert!(!crate::uid::is_well_formed("UID 9"));
    }

    /// The format-level invariant that *is* the fix, asserted on the format
    /// string rather than on a captured answer, so an edit that undoes it fails
    /// here at `cargo test` instead of on a machine that happens to have a
    /// hostile session sitting on the shared server.
    #[test]
    fn the_epoch_format_never_expands_a_session_stamp_a_second_time() {
        let fmt = epoch_fmt();
        assert!(
            !fmt.contains("#{E:"),
            "{fmt:?} reintroduces tmux's `#{{E:…}}` secondary expansion. `E` tells tmux to \
             expand the variable's value as though it were a format, and that value is \
             session environment any shell on the shared server can write. A stamp of \
             `Z\\n#{{session_id}} <a victim's uid> #{{session_created}} #{{pid}} \
             #{{start_time}}\\nQ` then emits a second, perfectly well-formed record that \
             binds the victim's uid to the attacker's session, satisfies the epoch \
             re-check as well, and hands a phone somebody else's terminal. Read the stamp \
             through the sanitizing substitution instead."
        );
        // However it is spelled, the `E` modifier has no business in this format.
        assert!(
            !fmt.contains("E:"),
            "{fmt:?} carries an `E:` modifier; only tmux-authored fields and one \
             sanitized variable may appear here"
        );
        // The one attacker-controlled field goes through the substitution,
        // spelled exactly: no flag, because tmux accepts an unknown flag in
        // silence, and both cases, because a uid may be written in either.
        assert!(
            fmt.contains(&format!("#{{s/[^0-9A-Za-z]//:{}}}", crate::ENV_SESSION_UID)),
            "{fmt:?} must read the stamp as `#{{s/[^0-9A-Za-z]//:{}}}` — the substitution \
             is what deletes the record separator a crafted stamp would forge with",
            crate::ENV_SESSION_UID
        );
        // Once, and only once. A second, unsanitized reading of the same
        // variable sitting beside the sanitized one would reopen the whole hole.
        assert_eq!(
            fmt.matches(crate::ENV_SESSION_UID).count(),
            1,
            "{fmt:?} reads the stamp more than once; only the sanitized reading may appear"
        );
        // Everything else on the line is tmux's own words, and needs nothing.
        for authored in [
            "#{session_id}",
            "#{session_created}",
            "#{pid}",
            "#{start_time}",
        ] {
            assert!(
                fmt.contains(authored),
                "{fmt:?} has lost the tmux-authored field {authored}"
            );
        }
    }

    /// The forgery at parser level, in both regimes tmux can print it — and the
    /// reason the repair had to be made in [`epoch_fmt`] rather than here.
    ///
    /// The fixtures are transcribed `list-sessions` stdout from tmux 3.7b: three
    /// sessions on one scratch server, where `$0` is the victim honestly stamped
    /// with [`VICTIM_UID`], `$1` carries the payload *without* a trailing newline
    /// and `$2` carries it *with* one. The property pinned is that only the
    /// sanitized format leaves the victim's uid answering for the victim alone;
    /// the parser, asked the same question about the old format's output, cannot
    /// tell the forgery from the truth, and this test says so out loud rather
    /// than pretending otherwise.
    #[test]
    fn only_the_sanitized_format_keeps_a_uid_answering_for_one_session() {
        // Which sessions a given answer says are carrying the victim's uid.
        let bound = |stdout: &str| -> Vec<String> {
            stdout
                .lines()
                .filter_map(parse_epoch_line)
                .filter(|(_, uid, ..)| uid == VICTIM_UID)
                .map(|(id, ..)| id)
                .collect()
        };

        // The old `#{E:…}` format under a UTF-8 client: three sessions, six
        // lines. Line 2 is the payload without the trailing newline — eight
        // fields, because the outer format's own tail landed on it — and it does
        // not parse. Line 4 is the same payload *with* the newline, and it is a
        // flawless five-field record for `$2`.
        let expanded = "\
$1 ZZZZZZZZZZZZZZZZZZZZZZZZZZ
$1 01JQXV9K7B8N4M2P6R3T5W9YQD 1786459620 9445 1786459620 1786459620 9445 1786459620
$2 Z
$2 01JQXV9K7B8N4M2P6R3T5W9YQD 1786459620 9445 1786459620
Q 1786459620 9445 1786459620
$0 01JQXV9K7B8N4M2P6R3T5W9YQD 1786459620 9445 1786459620";
        assert_eq!(
            bound(expanded),
            vec!["$2".to_string(), "$0".to_string()],
            "the old format really did let two sessions answer to one uid, and no parser \
             check can undo that: the forged line is byte-identical to a line the victim \
             would have written honestly"
        );
        // With the victim's own session absent — a phone reconnecting to a run
        // whose session has just been replaced — the forged record is the *only*
        // answer, so the misbinding is total rather than a denial of service.
        let attacker_alone = expanded
            .lines()
            .filter(|line| !line.starts_with("$0 "))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(bound(&attacker_alone), vec!["$2".to_string()]);

        // The same three sessions as launchd's environment prints them. tmux
        // rewrites the newlines to `_` for a client that declares no UTF-8
        // locale, which is the accident that kept the *installed* daemon safe
        // and the reason a foreground daemon was the exposed one.
        let mangled = "\
$1 ZZZZZZZZZZZZZZZZZZZZZZZZZZ_$1 01JQXV9K7B8N4M2P6R3T5W9YQD 1786459620 9445 1786459620 1786459620 9445 1786459620
$2 Z_$2 01JQXV9K7B8N4M2P6R3T5W9YQD 1786459620 9445 1786459620_Q 1786459620 9445 1786459620
$0 01JQXV9K7B8N4M2P6R3T5W9YQD 1786459620 9445 1786459620";
        assert_eq!(
            bound(mangled),
            vec!["$0".to_string()],
            "the `_`-mangled answer must bind the uid to the victim and nobody else"
        );

        // And the same three sessions under the format that ships now, measured
        // byte-identical under `LANG=en_US.UTF-8` and under `env -i`: one record
        // per session, both payloads flattened into a single alphanumeric token
        // that is no longer a well-formed stamp, and one answer to the uid.
        let sanitized = "\
$1 ZZZZZZZZZZZZZZZZZZZZZZZZZZsessionid01JQXV9K7B8N4M2P6R3T5W9YQDsessioncreatedpidstarttime 1786459620 9445 1786459620
$2 Zsessionid01JQXV9K7B8N4M2P6R3T5W9YQDsessioncreatedpidstarttimeQ 1786459620 9445 1786459620
$0 01JQXV9K7B8N4M2P6R3T5W9YQD 1786459620 9445 1786459620";
        assert_eq!(
            sanitized.lines().count(),
            3,
            "three sessions must produce three records; a fourth is a forgery"
        );
        assert_eq!(
            bound(sanitized),
            vec!["$0".to_string()],
            "under the sanitized format only the honestly-stamped session answers"
        );
    }

    #[test]
    fn client_lines_parse_exactly_or_not_at_all() {
        assert_eq!(parse_client_line("99476 $0"), Some((99476, "$0")));
        assert_eq!(parse_client_line("1 $12"), Some((1, "$12")));
        // A missing field, an extra field, a non-numeric pid, an id that is not
        // tmux's `$N`: none of these proves our client is on our session.
        assert_eq!(parse_client_line("99476"), None);
        assert_eq!(parse_client_line("99476 $0 extra"), None);
        assert_eq!(parse_client_line("notapid $0"), None);
        assert_eq!(parse_client_line("99476 cc-1"), None);
        assert_eq!(parse_client_line(""), None);
    }

    /// The proof the unit test above cannot give: real tmux, and a client whose
    /// environment is *empty* — which is what launchd hands `ccd`, and the exact
    /// condition under which the shipped daemon could resolve nothing.
    ///
    /// `env_clear` on the child is the whole point. Every other test in this file
    /// inherits the developer's UTF-8 locale, which is precisely why none of them
    /// caught this. The session is deliberately named with spaces in it, so the
    /// test also proves the reason the name had to leave the format rather than
    /// be escaped inside it.
    #[test]
    fn a_tmux_client_with_launchds_empty_environment_still_yields_parseable_lines() {
        let Some(bin) = tmux_bin() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let dir = std::env::temp_dir().join(format!("cc-nolocale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sock").to_string_lossy().into_owned();
        let uid = crate::uid::new().unwrap();

        let created = Command::new(&bin)
            .args(["-S", &sock, "-f", "/dev/null", "new-session", "-d"])
            // A name with spaces, and one that is not ASCII: both are legal tmux
            // names and both would have shredded a space-delimited line.
            .args(["-s", "a name with spaces ünï"])
            .args(["-e", &format!("{}={uid}", crate::ENV_SESSION_UID)])
            .args(["--", "/bin/sh", "-c", "while :; do sleep 1; done"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "new-session failed: {}",
            String::from_utf8_lossy(&created.stderr)
        );

        let ask = |format: &str| {
            let out = Command::new(&bin)
                .env_clear() // launchd, exactly
                .args([
                    "-S",
                    &sock,
                    "-f",
                    "/dev/null",
                    "list-sessions",
                    "-F",
                    format,
                ])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8_lossy(&out.stdout).into_owned()
        };

        // The format that ships: every line parses, and the stamp survives.
        let stdout = ask(&epoch_fmt());
        let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!(!lines.is_empty(), "no sessions listed");
        let parsed: Vec<_> = lines.iter().copied().filter_map(parse_epoch_line).collect();
        assert_eq!(
            parsed.len(),
            lines.len(),
            "a line came back unparseable from a client with no locale: {stdout:?}"
        );
        assert!(
            parsed
                .iter()
                .any(|(id, line_uid, ..)| line_uid == &uid && is_session_id(id)),
            "the stamp must survive the trip intact: {stdout:?}"
        );

        // …and the teeth: the separator this used to use really is destroyed in
        // this environment, so the assertions above are not passing by accident.
        let armoured = ask("A\x1fB\tC");
        assert!(
            armoured.contains("A_B_C"),
            "expected tmux to armour \\x1f and \\t; got {armoured:?}"
        );
        assert!(!armoured.contains('\x1f'));

        Command::new(&bin)
            .args(["-S", &sock, "kill-server"])
            .stdin(Stdio::null())
            .output()
            .ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A private throwaway server, addressed by an absolute socket path, with
    /// `-f /dev/null` so it borrows nothing from the operator's config and
    /// never touches the live `codeconnect` server. The whole identity story
    /// proven against real tmux: right uid resolves, wrong uid is absent, a
    /// reused name is not mistaken for the run that held it, two claimants are
    /// refused, and the attach never conjures a server.
    #[test]
    fn identity_resolution_is_race_proof_against_real_tmux() {
        let Some(bin) = tmux_bin() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let dir = std::env::temp_dir().join(format!("cc-idresolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sock").to_string_lossy().into_owned();

        let tmux = |args: &[&str]| {
            std::process::Command::new(&bin)
                .args(["-S", &sock, "-f", "/dev/null"])
                .args(args)
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        let start = |name: &str, uid: &str| {
            let out = tmux(&[
                "new-session",
                "-d",
                "-s",
                name,
                "-e",
                &format!("{}={uid}", crate::ENV_SESSION_UID),
                "--",
                "/bin/sh",
                "-c",
                "while :; do sleep 1; done",
            ]);
            // A failed creation would make the sub-proofs below vacuous, so it
            // is a hard failure, not a silent no-op.
            assert!(
                out.status.success(),
                "new-session {name} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };

        // Real minted uids: `resolve` refuses anything not well-formed before
        // it looks at a single session, so the stamps a test uses must be the
        // real thing.
        let uid_keep = crate::uid::new().unwrap();
        let uid_a = crate::uid::new().unwrap();
        let uid_b = crate::uid::new().unwrap();
        let uid_z = crate::uid::new().unwrap();

        // No server yet: a uid resolves to NotHosted, and it did not create one.
        assert_eq!(
            resolve_owned_session(&sock, &uid_a),
            Err(ResolveError::NotHosted)
        );
        assert!(
            !dir.join("sock").exists(),
            "resolve must not start a server"
        );

        // A keepalive so the server survives sessions coming and going: a
        // server that loses its last session exits, and a fresh server restarts
        // the id counter — a *different* race, caught by the epoch fields in the
        // attach phase. Here we test same-server name reuse, where ids increment.
        start("keep", &uid_keep);

        start("cc-1", &uid_a);
        let a = resolve_owned_session(&sock, &uid_a).expect("uid_a resolves");
        assert_eq!(a.uid, uid_a);
        assert!(a.session_id.starts_with('$'), "internal id, not a name");
        // A uid nothing carries is NotHosted, not the session that happens to
        // be there.
        assert_eq!(
            resolve_owned_session(&sock, &uid_z),
            Err(ResolveError::NotHosted)
        );

        // Name reuse: kill cc-1, start a NEW cc-1 with a different uid. The old
        // uid must no longer resolve; the new one must.
        tmux(&["kill-session", "-t", "=cc-1"]);
        start("cc-1", &uid_b);
        assert_eq!(
            resolve_owned_session(&sock, &uid_a),
            Err(ResolveError::NotHosted)
        );
        let b = resolve_owned_session(&sock, &uid_b).expect("the reused name's new uid resolves");
        assert_ne!(b.session_id, a.session_id, "a new session has a new id");

        // **Pinned against tmux, not against each other.** `b.server_pid ==
        // a.server_pid` and `b.session_created >= a.session_created` compare
        // two resolutions of the same server, so a resolver that stopped
        // reading the epoch and returned zero for both would satisfy them —
        // `0 == 0` and `0 >= 0` — and the fields that exist to catch a
        // restarted server would be untested. What each field claims to be is
        // something tmux will state independently, so that is what it is
        // checked against.
        // Read from `list-sessions` rather than `display-message`: with a
        // `=name` target the latter resolves a pane and leaves the session
        // formats empty (measured on tmux 3.7b — `#{pid}` answers,
        // `#{session_created}` comes back blank), whereas each row here is
        // unambiguously one session's.
        let epoch_of = |name: &str| -> (i64, i64) {
            let out = tmux(&[
                "list-sessions",
                "-F",
                "#{session_name} #{session_created} #{pid}",
            ]);
            assert!(out.status.success(), "tmux could not list its sessions");
            let listing = String::from_utf8_lossy(&out.stdout).into_owned();
            let row = listing
                .lines()
                .find(|line| line.split(' ').next() == Some(name))
                .unwrap_or_else(|| panic!("tmux does not list {name}: {listing:?}"));
            let fields: Vec<&str> = row.split(' ').collect();
            let parse = |at: usize, what: &str| -> i64 {
                fields[at]
                    .parse()
                    .unwrap_or_else(|err| panic!("tmux reported {what} as {:?}: {err}", fields[at]))
            };
            (parse(1, "#{session_created}"), parse(2, "#{pid}"))
        };
        let (created, server_pid) = epoch_of("cc-1");
        assert_ne!(server_pid, 0, "a live server has a pid");
        assert_ne!(created, 0, "a live session has a creation time");
        assert_eq!(
            a.server_pid, server_pid,
            "the epoch has to name the server that is actually running"
        );
        assert_eq!(b.server_pid, server_pid, "still the one server");
        assert_eq!(
            b.session_created, created,
            "the creation time has to be the session's own"
        );
        // And the ordering the original assertion was reaching for, now that
        // both sides are known to be real values rather than a shared default.
        assert!(b.session_created >= a.session_created);

        // An unstamped session (created by hand, no `-e`) exists, but neither a
        // malformed uid nor an empty one may borrow its identity. The creation
        // is asserted or the sub-proof would be vacuous.
        assert!(
            tmux(&[
                "new-session",
                "-d",
                "-s",
                "unstamped",
                "--",
                "/bin/sh",
                "-c",
                "while :; do sleep 1; done",
            ])
            .status
            .success(),
            "the unstamped session is created"
        );
        assert!(
            has_session_argv(&sock, "unstamped").is_some_and(|argv| {
                std::process::Command::new(&bin)
                    .args(&argv)
                    .stdin(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            }),
            "the unstamped session actually exists before the negative proof"
        );
        assert_eq!(
            resolve_owned_session(&sock, ""),
            Err(ResolveError::NotHosted)
        );
        assert_eq!(
            resolve_owned_session(&sock, "not-a-real-uid"),
            Err(ResolveError::NotHosted)
        );

        // Duplicate stamp: two live sessions claiming one uid is refused, never
        // silently disambiguated.
        start("cc-2", &uid_b);
        assert!(matches!(
            resolve_owned_session(&sock, &uid_b),
            Err(ResolveError::IdentityMismatch(_))
        ));

        tmux(&["kill-server"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The UID-atomic destructive contract against real tmux (D5/D7): a kill
    /// resolves the internal id from the uid immediately before acting and
    /// destroys only that id; a reused `cc-N` name never lets a stale uid's
    /// cleanup capture the new run; an epoch pin from a dead server refuses; and
    /// `owned_liveness` reports `Gone` when the name has been reused under it.
    #[test]
    fn destructive_ops_are_uid_atomic_against_real_tmux() {
        let Some(bin) = tmux_bin() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let dir = std::env::temp_dir().join(format!("cc-destroy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sock").to_string_lossy().into_owned();
        let tmux = |args: &[&str]| {
            std::process::Command::new(&bin)
                .args(["-S", &sock, "-f", "/dev/null"])
                .args(args)
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        let start = |name: &str, uid: &str| {
            let out = tmux(&[
                "new-session",
                "-d",
                "-s",
                name,
                "-e",
                &format!("{}={uid}", crate::ENV_SESSION_UID),
                "--",
                "/bin/sh",
                "-c",
                "while :; do sleep 1; done",
            ]);
            assert!(
                out.status.success(),
                "new-session {name} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };

        let uid_keep = crate::uid::new().unwrap();
        let uid_a = crate::uid::new().unwrap();
        let uid_b = crate::uid::new().unwrap();

        // No server yet: destroy cannot PROVE absence without a server to confirm
        // against (round-4 finding 1: a socket error is never a false `Absent`),
        // so it is `Unavailable` — and it starts nothing.
        assert!(matches!(
            destroy_owned_session(&sock, &uid_a, None),
            CleanupOutcome::Unavailable(_)
        ));
        assert!(
            !dir.join("sock").exists(),
            "cleanup must not start a server"
        );

        start("keep", &uid_keep);
        start("cc-1", &uid_a);
        let a = resolve_owned_session(&sock, &uid_a).expect("uid_a resolves");
        assert_eq!(owned_liveness(&sock, &uid_a, Some(&a)), OwnedLiveness::Live);

        // Name reuse *before* cleanup: kill cc-1's session out from under uid_a
        // and start a new cc-1 carrying uid_b. Destroying uid_a must now be
        // Absent (its session is gone) and must NOT touch uid_b's new session.
        tmux(&["kill-session", "-t", "=cc-1"]);
        start("cc-1", &uid_b);
        assert_eq!(
            destroy_owned_session(&sock, &uid_a, None),
            CleanupOutcome::Absent
        );
        let b = resolve_owned_session(&sock, &uid_b).expect("uid_b's new cc-1 is untouched");
        assert_ne!(b.session_id, a.session_id);
        // uid_a's stale pin now reads Gone (owner mismatch under the reused name).
        // This is the load-bearing name-reuse guard: the name is the same, the
        // session is not, and the uid+epoch says so. (A stale-epoch destroy
        // *refusal* across a genuine server restart is proven separately in
        // `a_server_restart_inside_the_window_is_refused_by_the_epoch_pin`;
        // within one server, tmux's 1-second `session_created` resolution cannot
        // distinguish two same-second sessions, so that case is not asserted
        // here where it would be flaky.)
        assert_eq!(owned_liveness(&sock, &uid_a, Some(&a)), OwnedLiveness::Gone);

        // Destroy uid_b for real, pinned to its own epoch: Killed, and gone.
        assert_eq!(
            destroy_owned_session(&sock, &uid_b, Some(&b)),
            CleanupOutcome::Killed
        );
        assert_eq!(
            resolve_owned_session(&sock, &uid_b),
            Err(ResolveError::NotHosted)
        );

        tmux(&["kill-server"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A tmux server restart inside the check/act window: `owned_liveness` and a
    /// destroy pinned to the pre-restart epoch must not act on a same-id session
    /// on the *new* server. Exercised by pinning to a resolved epoch, restarting
    /// the server, and re-creating the same uid — the epoch pin refuses.
    #[test]
    fn a_server_restart_inside_the_window_is_refused_by_the_epoch_pin() {
        let Some(bin) = tmux_bin() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let dir = std::env::temp_dir().join(format!("cc-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sock").to_string_lossy().into_owned();
        let tmux = |args: &[&str]| {
            std::process::Command::new(&bin)
                .args(["-S", &sock, "-f", "/dev/null"])
                .args(args)
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        let start = |name: &str, uid: &str| {
            assert!(tmux(&[
                "new-session",
                "-d",
                "-s",
                name,
                "-e",
                &format!("{}={uid}", crate::ENV_SESSION_UID),
                "--",
                "/bin/sh",
                "-c",
                "while :; do sleep 1; done",
            ])
            .status
            .success());
        };
        let uid = crate::uid::new().unwrap();
        start("cc-1", &uid);
        let pinned = resolve_owned_session(&sock, &uid).expect("resolves");

        // Restart the whole server, then re-create the same uid/name. The new
        // server has a different pid/start_time; the pinned epoch is stale.
        tmux(&["kill-server"]);
        start("cc-1", &uid);

        // Liveness against the stale pin: Gone (different epoch), never a false Live.
        assert_eq!(
            owned_liveness(&sock, &uid, Some(&pinned)),
            OwnedLiveness::Gone
        );
        // Destroy pinned to the stale A (round-5): the pinned server A is DEAD, but
        // a reachable successor carries the uid ⇒ it must NOT kill the new run and
        // must NOT complete — it retries (`Unavailable`), chasing the uid on the
        // successor on the next pass. Never a false Killed/ServerGone.
        let outcome = destroy_owned_session(&sock, &uid, Some(&pinned));
        assert!(
            matches!(outcome, CleanupOutcome::Unavailable(_)),
            "a stale-A destroy over a live successor must retry, got {outcome:?}"
        );
        assert_ne!(outcome, CleanupOutcome::Killed);
        assert_ne!(outcome, CleanupOutcome::ServerGone);
        assert!(
            resolve_owned_session(&sock, &uid).is_ok(),
            "the refusal protected the new server's session"
        );

        tmux(&["kill-server"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The forgery attempted against real tmux, with the trailing newline that is
    /// the whole exploit, on a private throwaway server addressed by an absolute
    /// socket path and configured from `/dev/null` — never the operator's live
    /// `codeconnect` server, whose sessions are somebody's actual work.
    ///
    /// The property pinned: a session may stamp itself with anything, including a
    /// payload that names another session's uid and asks tmux to fill in a whole
    /// second record around it, and [`resolve_owned_session`] still answers with
    /// the session that honestly carries the uid. Not the attacker's, and not
    /// `IdentityMismatch` either — a forged duplicate that merely *denied* the
    /// victim their terminal would still be the attacker choosing the outcome.
    ///
    /// The locale is set on the questions this test asks rather than inherited,
    /// because the forgery only survives for a client that declares a UTF-8
    /// codeset: under launchd's empty environment tmux rewrites the newlines to
    /// `_` and the payload collapses harmlessly. Pinning the locale here is what
    /// makes the exploitable regime the one under test wherever `cargo test` runs.
    #[test]
    fn a_crafted_stamp_cannot_forge_a_record_for_another_sessions_uid() {
        let Some(bin) = tmux_bin() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let dir = std::env::temp_dir().join(format!(
            "cc-forge-{}-{}",
            std::process::id(),
            crate::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sock").to_string_lossy().into_owned();

        let tmux = |args: &[&str]| {
            Command::new(&bin)
                .args(["-S", &sock, "-f", "/dev/null"])
                .args(args)
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        let start = |name: &str, stamp: &str| {
            let out = tmux(&[
                "new-session",
                "-d",
                "-s",
                name,
                "-e",
                &format!("{}={stamp}", crate::ENV_SESSION_UID),
                "--",
                "/bin/sh",
                "-c",
                "while :; do sleep 1; done",
            ]);
            // A creation that quietly failed would make every proof below
            // vacuous, so it is a hard failure rather than a silent no-op.
            assert!(
                out.status.success(),
                "new-session {name} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let ask = |format: &str| {
            let out = Command::new(&bin)
                // A client that declares UTF-8 is the regime the payload needs;
                // the other two variables are cleared because tmux consults
                // LC_ALL, then LC_CTYPE, then LANG, and the developer's shell
                // must not get a vote in what this test measures.
                .env("LANG", "en_US.UTF-8")
                .env_remove("LC_ALL")
                .env_remove("LC_CTYPE")
                .args([
                    "-S",
                    &sock,
                    "-f",
                    "/dev/null",
                    "list-sessions",
                    "-F",
                    format,
                ])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "list-sessions failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let bound_to = |stdout: &str, uid: &str| -> Vec<String> {
            stdout
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(parse_epoch_line)
                .filter(|(_, line_uid, ..)| line_uid == uid)
                .map(|(id, ..)| id)
                .collect()
        };

        let victim_uid = crate::uid::new().unwrap();
        start("victim", &victim_uid);
        let victim = resolve_owned_session(&sock, &victim_uid)
            .expect("the victim resolves before the attack");

        // The payload, built from the uid an attacker reads straight out of
        // `list-sessions` — which any shell on the shared server can run. The
        // trailing newline is the exploit: without it the outer format's own
        // ` #{session_created} #{pid} #{start_time}` lands on the same line, the
        // record is eight fields wide, and nothing parses.
        let payload = format!(
            "Z\n#{{session_id}} {victim_uid} #{{session_created}} #{{pid}} #{{start_time}}\nQ"
        );
        start("attacker", &payload);
        assert!(
            has_session_argv(&sock, "attacker").is_some_and(|argv| {
                Command::new(&bin)
                    .args(&argv)
                    .stdin(Stdio::null())
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false)
            }),
            "the attacker session must actually exist, or this test proves nothing"
        );

        // The teeth, first: under the format this replaced, the payload really
        // does forge a record, so the assertions that follow are not passing
        // because the payload is inert. If a future tmux stops expanding `#{E:}`
        // this fails, and it should — it would mean the threat model moved.
        let vulnerable = format!(
            "#{{session_id}} #{{E:{}}} #{{session_created}} #{{pid}} #{{start_time}}",
            crate::ENV_SESSION_UID
        );
        let forged = ask(&vulnerable);
        let forged_bound = bound_to(&forged, &victim_uid);
        assert!(
            forged_bound.len() > 1 && forged_bound.iter().any(|id| id != &victim.session_id),
            "the payload must still forge a record under `#{{E:…}}`, or this test has \
             stopped testing anything: {forged:?}"
        );

        // The whole point, through the real entry point and asserted first so a
        // regression reads as what it is. Two ways to fail and both are the
        // attacker choosing the outcome: the resolver binds the uid to the
        // forged session, or it sees two claimants and refuses the victim their
        // own terminal.
        let resolved = resolve_owned_session(&sock, &victim_uid).unwrap_or_else(|err| {
            panic!("a forged record must not cost the victim their own session: {err:?}")
        });
        assert_eq!(
            resolved.session_id, victim.session_id,
            "the resolver bound the victim's uid to the attacker's session"
        );
        assert_eq!(resolved.uid, victim_uid);

        // And the same thing read off the wire: two sessions, two records. The
        // line count is the direct anti-forgery assertion — a third line is a
        // stamp that ended its own record and began somebody else's.
        let listed = ask(&epoch_fmt());
        let lines: Vec<&str> = listed.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            2,
            "two sessions must answer with exactly two records; an extra line is a stamp \
             that carried the record separator: {listed:?}"
        );
        assert_eq!(
            bound_to(&listed, &victim_uid),
            vec![victim.session_id.clone()],
            "the victim's uid must be carried by the victim's session and nothing else: \
             {listed:?}"
        );

        tmux(&["kill-server"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ----------------------------------------------------------------------
    // Principle F: the census is proof-or-Unknown. A malformed/truncated/capped
    // non-blank row must yield Unavailable ("cannot tell"), NEVER a false
    // NotHosted/absence — proven directly against the pure row resolver.
    // ----------------------------------------------------------------------

    #[test]
    fn a_malformed_census_row_is_unavailable_never_a_false_absence() {
        let uid = crate::uid::new().unwrap();
        // A clean, matching row resolves.
        let good = format!("$3 {uid} 100 200 300");
        let ok = resolve_uid_from_rows("sock", &uid, &good).expect("a clean row resolves");
        assert_eq!(ok.session_id, "$3");
        assert_eq!(ok.server_pid, 200);

        // A row for OUR uid that is truncated mid-record (the last two fields
        // cut off, as an output cap would do) is Unavailable, NOT NotHosted —
        // the whole point of Principle F.
        let truncated = format!("$3 {uid} 100");
        assert!(
            matches!(
                resolve_uid_from_rows("sock", &uid, &truncated),
                Err(ResolveError::Unavailable(_))
            ),
            "a truncated row must be Unavailable, never NotHosted"
        );

        // A *structurally broken* non-blank row (non-numeric fields) is likewise
        // Unavailable — proof of nothing, so absence cannot be inferred.
        let garbage = "this is not a session record at all";
        assert!(
            matches!(
                resolve_uid_from_rows("sock", &uid, garbage),
                Err(ResolveError::Unavailable(_))
            ),
            "a corrupt row must be Unavailable, never NotHosted"
        );

        // But a *structurally intact* row whose uid is merely a garbage/unreadable
        // stamp is a real other session — it is SKIPPED by uid mismatch, never
        // collapses the census to Unavailable. This is the anti-forgery/anti-DoS
        // property: an attacker cannot deny us our resolve by parking a session
        // with a junk stamp. Here it means a clean NotHosted (no row is ours).
        let junk_stamp = format!("$9 GARBAGESTAMP123 1 2 3\n$3 {uid} 100 200 300\n");
        let resolved =
            resolve_uid_from_rows("sock", &uid, &junk_stamp).expect("our row still resolves");
        assert_eq!(
            resolved.session_id, "$3",
            "a junk-stamped neighbour must not hide our session"
        );
        let only_junk = "$9 GARBAGESTAMP123 1 2 3\n";
        assert_eq!(
            resolve_uid_from_rows("sock", &uid, only_junk),
            Err(ResolveError::NotHosted),
            "a junk-stamped session we don't own is a clean absence, not Unavailable"
        );

        // A clean census that simply has no row for our uid IS a real absence.
        let other = crate::uid::new().unwrap();
        let clean_other = format!("$4 {other} 1 2 3\n");
        assert_eq!(
            resolve_uid_from_rows("sock", &uid, &clean_other),
            Err(ResolveError::NotHosted),
            "a clean census with no matching row is a true NotHosted"
        );

        // Two rows claiming our uid refuse rather than pick.
        let dup = format!("$5 {uid} 1 2 3\n$6 {uid} 4 5 6\n");
        assert!(matches!(
            resolve_uid_from_rows("sock", &uid, &dup),
            Err(ResolveError::IdentityMismatch(_))
        ));

        // Blank lines are ignored (an empty census is a clean absence).
        assert_eq!(
            resolve_uid_from_rows("sock", &uid, "\n  \n"),
            Err(ResolveError::NotHosted)
        );
    }

    /// Finding 1/2 (census classification): a socket error is `NoServer`, a
    /// truncated/empty/malformed listing is `CannotTell` — **never** a served
    /// `Absent`. Only a successful, untruncated, non-empty listing yields a
    /// `Served{Absent}`. So the post-kill `Killed`/`ServerGone` proofs can never be
    /// faked by a socket error (a `SIGUSR1`-recreate `No such file`).
    #[test]
    fn census_never_reads_a_socket_error_as_a_served_absence() {
        let uid = crate::uid::new().unwrap();
        let other = crate::uid::new().unwrap();

        // ok=false with a "no server" stderr ⇒ NoServer (not absence).
        assert_eq!(
            census_from_probe(
                "sock",
                &uid,
                false,
                "",
                "no server running on /tmp/x",
                false
            ),
            Census::NoServer
        );
        // ok=false with a permission/other error ⇒ CannotTell.
        assert!(matches!(
            census_from_probe("sock", &uid, false, "", "permission denied", false),
            Census::CannotTell(_)
        ));
        // Truncated ⇒ CannotTell.
        assert!(matches!(
            census_from_probe("sock", &uid, true, &format!("$1 {other} 1 2 3\n"), "", true),
            Census::CannotTell(_)
        ));
        // Empty successful listing ⇒ CannotTell (server id unknown; a zero-session
        // server exits, so this is a rare transient we do not bind).
        assert!(matches!(
            census_from_probe("sock", &uid, true, "", "", false),
            Census::CannotTell(_)
        ));
        // A successful, complete listing WITHOUT our uid ⇒ Served{Absent}, and it
        // carries the serving server's pid so a caller can bind it.
        assert_eq!(
            census_from_probe(
                "sock",
                &uid,
                true,
                &format!("$1 {other} 1 200 3\n"),
                "",
                false
            ),
            Census::Served {
                server_pid: 200,
                presence: RowPresence::Absent
            }
        );
        // A successful listing WITH our uid ⇒ Served{Present}.
        assert!(matches!(
            census_from_probe(
                "sock",
                &uid,
                true,
                &format!("$4 {uid} 9 250 7\n"),
                "",
                false
            ),
            Census::Served {
                server_pid: 250,
                presence: RowPresence::Present(_)
            }
        ));
        // Two claimants ⇒ Served{Ambiguous}; a malformed row ⇒ Served{Malformed}.
        assert!(matches!(
            census_from_probe(
                "sock",
                &uid,
                true,
                &format!("$4 {uid} 1 200 3\n$5 {uid} 4 200 6\n"),
                "",
                false
            ),
            Census::Served {
                presence: RowPresence::Ambiguous(_),
                ..
            }
        ));
        assert!(matches!(
            census_from_probe("sock", &uid, true, "this is not a row", "", false),
            Census::CannotTell(_)
        ));
    }

    /// Finding 2/4/10: binding a census to a server identity A. A census is
    /// "served by A" ONLY when its `#{pid}` equals A.pid AND that pid's CURRENT
    /// kernel birth equals A.birth. So a **wrong endpoint** (a different server's
    /// pid) and a **pid reuse** (same pid, different birth) both fail the bind —
    /// they can never fake a proven absence/kill on A.
    #[test]
    fn census_served_by_requires_matching_pid_and_birth() {
        let me = crate::proc_identity::current_identity().unwrap();
        let uid = crate::uid::new().unwrap();
        // A census whose serving pid IS ours, and we assert against our real
        // identity ⇒ served by us.
        let served_ours = census_from_probe(
            "sock",
            &uid,
            true,
            &format!("$1 {uid} 1 {} 3\n", me.pid),
            "",
            false,
        );
        assert!(
            census_served_by(&served_ours, &me),
            "our own pid+birth binds"
        );

        // Wrong endpoint: the census is served by a DIFFERENT pid than A ⇒ not
        // served by A (never a proven absence on A).
        let served_other = census_from_probe(
            "sock",
            &uid,
            true,
            &format!("$1 {uid} 1 999999 3\n"),
            "",
            false,
        );
        assert!(
            !census_served_by(&served_other, &me),
            "a different pid is not A"
        );

        // PID reuse: same pid as A, but A's recorded birth differs from the pid's
        // CURRENT birth ⇒ not A (fail-closed against reuse in the read gap).
        let mut reused = me;
        reused.birth.start_usec ^= 0x5A5A;
        assert!(
            !census_served_by(&served_ours, &reused),
            "a pid whose current birth differs from the pinned birth is not A"
        );

        // A NoServer / CannotTell census is never "served by A".
        assert!(!census_served_by(&Census::NoServer, &me));
        assert!(!census_served_by(&Census::CannotTell("x".into()), &me));
    }

    /// Principle C, the **outer** window (`pin: None`, the real custodian call): a
    /// restart injected before the pre-kill re-resolve is caught by that
    /// re-resolve and refused as `EpochChanged`, never a false `Killed`.
    #[test]
    fn a_restart_in_the_outer_window_is_refused_by_the_pre_kill_recheck() {
        let Some(bin) = tmux_bin() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let dir = std::env::temp_dir().join(format!("cc-window-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sock").to_string_lossy().into_owned();
        let restart_and_recreate = restart_recreater(&bin, &sock);
        let uid = crate::uid::new().unwrap();
        start_session(&bin, &sock, "cc-1", &uid);

        // Inject the restart in the OUTER seam (before the pre-kill re-confirm):
        // the census is now served by a DIFFERENT server than A, so the destroy
        // refuses to kill and returns `Unavailable` (retry) — NEVER a false
        // `Killed` and never a `complete`. (For the custodian this means the next
        // pass re-resolves and targets the new server's session.)
        let uid_for_hook = uid.clone();
        let outcome = destroy_owned_session_hooked(
            &sock,
            &uid,
            None,
            move || restart_and_recreate(&uid_for_hook),
            || {},
        );
        assert!(
            matches!(outcome, CleanupOutcome::Unavailable(_)),
            "a restart before the pre-kill re-confirm must refuse (Unavailable), got {outcome:?}"
        );
        assert_ne!(outcome, CleanupOutcome::Killed);
        assert!(
            resolve_owned_session(&sock, &uid).is_ok(),
            "the refusal protected the post-restart session"
        );

        kill_server(&bin, &sock);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Principle C, the **inner** window and finding 1's core: a restart injected
    /// AFTER the pre-kill re-resolve and BEFORE the kill exercises the post-kill
    /// backstop. The kill may land on the restarted server's reused `$N` and even
    /// drain it — but the outcome must **never** be `Killed`. It is a non-success
    /// (`Ambiguous` when the server drained, `EpochChanged` when it survived on a
    /// new epoch); a false success is impossible.
    #[test]
    fn a_restart_in_the_inner_window_is_never_reported_as_a_false_killed() {
        let Some(bin) = tmux_bin() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let dir = std::env::temp_dir().join(format!("cc-inner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sock").to_string_lossy().into_owned();
        let uid = crate::uid::new().unwrap();
        start_session(&bin, &sock, "cc-1", &uid);

        // Inject the **$N→$M SURVIVOR** restart in the INNER seam (finding 2/10):
        // kill server A, then bring up a fresh server B with `keep` sessions AND
        // our uid recreated at a NEW `$N`. The stale kill lands on B's reused id,
        // but B survives (keep holds it up) and STILL carries our uid. The
        // post-kill proof: liveness(A) is Gone, but a census of B shows our uid
        // PRESENT ⇒ this must be `Unavailable` (retry), never `ServerGone`/complete
        // while a session bearing our uid provably lives.
        let bin_hook = bin.clone();
        let sock_hook = sock.clone();
        let uid_hook = uid.clone();
        let outcome = destroy_owned_session_hooked(
            &sock,
            &uid,
            None,
            || {},
            move || {
                kill_server(&bin_hook, &sock_hook);
                // Fresh server B with two keep sessions ($0, $1) plus our uid at $2.
                start_session(&bin_hook, &sock_hook, "keep-a", &crate::uid::new().unwrap());
                start_session(&bin_hook, &sock_hook, "keep-b", &crate::uid::new().unwrap());
                start_session(&bin_hook, &sock_hook, "cc-1", &uid_hook);
            },
        );
        assert!(
            matches!(outcome, CleanupOutcome::Unavailable(_)),
            "a survivor bearing our uid on a successor server must NOT complete: {outcome:?}"
        );
        assert_ne!(
            outcome,
            CleanupOutcome::ServerGone,
            "survivor lives ⇒ not ServerGone"
        );
        assert_ne!(outcome, CleanupOutcome::Killed);
        // And our uid provably still resolves on B.
        assert!(
            resolve_owned_session(&sock, &uid).is_ok(),
            "the survivor lives"
        );

        kill_server(&bin, &sock);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- shared helpers for the window tests ---

    fn start_session(bin: &std::path::Path, sock: &str, name: &str, uid: &str) {
        assert!(std::process::Command::new(bin)
            .args(["-S", sock, "-f", "/dev/null"])
            .args([
                "new-session",
                "-d",
                "-s",
                name,
                "-e",
                &format!("{}={uid}", crate::ENV_SESSION_UID),
                "--",
                "/bin/sh",
                "-c",
                "while :; do sleep 1; done",
            ])
            .stdin(Stdio::null())
            .status()
            .unwrap()
            .success());
    }

    fn kill_server(bin: &std::path::Path, sock: &str) {
        std::process::Command::new(bin)
            .args(["-S", sock, "kill-server"])
            .stdin(Stdio::null())
            .output()
            .ok();
    }

    /// A closure that kills the server and re-creates `cc-1` for a given uid on a
    /// fresh server (a new epoch) — the worst case injected into a destroy seam.
    fn restart_recreater(bin: &std::path::Path, sock: &str) -> impl Fn(&str) {
        let bin = bin.to_path_buf();
        let sock = sock.to_string();
        move |uid: &str| {
            kill_server(&bin, &sock);
            start_session(&bin, &sock, "cc-1", uid);
        }
    }

    /// Finding 3: a **truncated** census (the capture hit the byte cap and the
    /// output is a prefix) is proof of nothing — it must be `Unavailable`, never a
    /// false `NotHosted`/absence — even when the prefix we did receive is itself a
    /// clean, complete set of rows that happens not to name our uid. Proven
    /// through the pure `resolve_from_probe`, since a real >8 MiB tmux census is
    /// not feasible in a unit test.
    #[test]
    fn a_truncated_census_is_unavailable_never_a_false_absence() {
        let uid = crate::uid::new().unwrap();
        let other = crate::uid::new().unwrap();
        // A clean prefix that does NOT name our uid, but the listing was cut: we
        // cannot conclude absence, because the dropped tail could have held it.
        let clean_prefix = format!("$1 {other} 1 2 3\n");
        assert!(
            matches!(
                resolve_from_probe("sock", &uid, true, &clean_prefix, "", true),
                Err(ResolveError::Unavailable(_))
            ),
            "a truncated listing must be Unavailable, never NotHosted"
        );
        // The very same bytes, NOT truncated, ARE a clean absence.
        assert_eq!(
            resolve_from_probe("sock", &uid, true, &clean_prefix, "", false),
            Err(ResolveError::NotHosted),
            "the same complete listing is a true absence"
        );
        // A failed probe still classifies by stderr regardless of truncation.
        assert_eq!(
            resolve_from_probe("sock", &uid, false, "", "no server running", false),
            Err(ResolveError::NotHosted)
        );
    }

    /// Finding 9: the destroy/liveness pin must distinguish a same-uid session
    /// recreated on the **same server** (same epoch), where only the internal
    /// `$N` differs — the case a creation-time-only pin (1-second resolution)
    /// could miss. A `keep` session holds the server up so the epoch is unchanged
    /// across the kill+recreate; the old pin must then read `Gone`, and a destroy
    /// pinned to it must refuse (`EpochChanged`), never act on the replacement.
    #[test]
    fn the_pin_distinguishes_a_same_epoch_recreation_by_session_id() {
        let Some(bin) = tmux_bin() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let dir = std::env::temp_dir().join(format!("cc-pinid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sock").to_string_lossy().into_owned();

        let keep_uid = crate::uid::new().unwrap();
        let uid = crate::uid::new().unwrap();
        // `keep` keeps the server alive so the epoch survives the kill below.
        start_session(&bin, &sock, "keep", &keep_uid);
        start_session(&bin, &sock, "cc-1", &uid);
        let pin = resolve_owned_session(&sock, &uid).expect("cc-1 resolves");

        // Kill cc-1 and recreate it under the SAME uid on the SAME server (keep
        // held it up), retrying until the replacement lands in the SAME `session_created`
        // second as the original. Requiring equal `session_created` is the whole
        // point of the hardening: it forces `session_id` to be the *only*
        // discriminator, so a creation-time-only pin (the old impl) could not
        // pass. Same-second recreation is the common case, so this converges fast.
        let kill_cc1 = || {
            std::process::Command::new(&bin)
                .args([
                    "-S",
                    &sock,
                    "-f",
                    "/dev/null",
                    "kill-session",
                    "-t",
                    "=cc-1",
                ])
                .stdin(Stdio::null())
                .output()
                .unwrap();
        };
        let mut replacement = None;
        for _ in 0..50 {
            kill_cc1();
            start_session(&bin, &sock, "cc-1", &uid);
            let r = resolve_owned_session(&sock, &uid).expect("the replacement resolves");
            if r.session_created == pin.session_created {
                replacement = Some(r);
                break;
            }
        }
        let replacement = replacement.expect(
            "could not land a same-second recreation in 50 tries (needed to isolate session_id)",
        );

        // Same server epoch AND same session_created, different internal id: this
        // is exactly the case a creation-time-only pin could not tell apart.
        assert_eq!(
            (replacement.server_pid, replacement.server_start_time),
            (pin.server_pid, pin.server_start_time),
            "the replacement is on the same server epoch (keep held it up)"
        );
        assert_eq!(
            replacement.session_created, pin.session_created,
            "the replacement shares the original's creation second (session_id is the only difference)"
        );
        assert_ne!(
            replacement.session_id, pin.session_id,
            "the replacement has a fresh internal id"
        );

        // The old pin includes `$N`, so it reads Gone…
        assert_eq!(
            owned_liveness(&sock, &uid, Some(&pin)),
            OwnedLiveness::Gone,
            "the old pin's session_id no longer matches — our session is gone"
        );
        // …and a destroy pinned to the old `$N` must NOT act on the replacement:
        // A is alive (same server) but the uid is now at a different `$N`, so it
        // retries (`Unavailable`) rather than kill the wrong id. Never a false
        // Killed.
        let outcome = destroy_owned_session(&sock, &uid, Some(&pin));
        assert!(
            matches!(outcome, CleanupOutcome::Unavailable(_)),
            "a destroy pinned to a stale session_id must retry, got {outcome:?}"
        );
        assert_ne!(outcome, CleanupOutcome::Killed);
        assert!(
            resolve_owned_session(&sock, &uid).is_ok(),
            "the replacement survived the pinned-destroy refusal"
        );

        kill_server(&bin, &sock);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Round-5 finding 2: `owned_liveness` never turns socket loss into a durable
    /// `Gone`. A reachable live session is `Live`; an UNREACHABLE socket (no
    /// server / removed) is `Unknown` (keep observing); a successful listing
    /// lacking our uid is a proven `Gone`.
    #[test]
    fn owned_liveness_socket_loss_is_unknown_not_gone() {
        let Some(bin) = tmux_bin() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let dir = std::env::temp_dir().join(format!("cc-ol-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sock").to_string_lossy().into_owned();
        let uid = crate::uid::new().unwrap();

        // Before any server: the socket is unreachable ⇒ Unknown (NOT Gone).
        assert!(matches!(
            owned_liveness(&sock, &uid, None),
            OwnedLiveness::Unknown(_)
        ));

        // A real live session on a reachable server ⇒ Live (healthy path intact).
        start_session(&bin, &sock, "cc-1", &uid);
        assert_eq!(owned_liveness(&sock, &uid, None), OwnedLiveness::Live);

        // Remove the socket file while the server is STILL ALIVE (a SIGUSR1-style
        // socket loss): unreachable ⇒ Unknown, never a durable Gone.
        std::fs::remove_file(&sock).ok();
        assert!(matches!(
            owned_liveness(&sock, &uid, None),
            OwnedLiveness::Unknown(_)
        ));

        kill_server(&bin, &sock);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Round-5 finding 4: after server A is proven dead, a successor B that
    /// carries our uid but whose socket is in its recreation window (unreachable
    /// ⇒ `NoServer`) must be `Unavailable`, never a false `ServerGone` — the
    /// custodian keeps chasing the uid, it does not falsely complete.
    #[test]
    fn a_survivor_in_the_socket_recreation_window_is_not_servergone() {
        let Some(bin) = tmux_bin() else {
            eprintln!("skipped: no tmux");
            return;
        };
        let dir = std::env::temp_dir().join(format!("cc-bwin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("sock").to_string_lossy().into_owned();
        let uid = crate::uid::new().unwrap();

        // Server A carries our uid; pin A.
        start_session(&bin, &sock, "cc-1", &uid);
        let pin = resolve_owned_session(&sock, &uid).expect("A resolves");

        // A dies; a successor B is brought up carrying our uid (keep holds it up).
        kill_server(&bin, &sock);
        start_session(&bin, &sock, "keep", &crate::uid::new().unwrap());
        start_session(&bin, &sock, "cc-1", &uid);
        // B is alive and carries our uid — but simulate B's socket-recreation
        // window by removing the socket file (B keeps running, unreachable).
        std::fs::remove_file(&sock).ok();

        // Destroy pinned to the DEAD A: A is proc-gone, the socket is unreachable
        // (B's recreation window). We cannot prove our uid is absent everywhere (B
        // may carry it) ⇒ Unavailable, NEVER ServerGone.
        let outcome = destroy_owned_session(&sock, &uid, Some(&pin));
        assert!(
            matches!(outcome, CleanupOutcome::Unavailable(_)),
            "a survivor in its recreation window must be Unavailable, got {outcome:?}"
        );
        assert_ne!(outcome, CleanupOutcome::ServerGone);
        assert_ne!(outcome, CleanupOutcome::Killed);

        kill_server(&bin, &sock);
        std::fs::remove_dir_all(&dir).ok();
    }
}
