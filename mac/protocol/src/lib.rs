//! Shared CodeConnect wire types.
//!
//! Three surfaces live here so `ccd`, `cc` and `cc-hook` can never disagree:
//!   * [`event`] — the daemon-assigned event log envelope (source of truth).
//!   * [`config`] — one config file, shared: the shim generates hooks that
//!     match exactly what the daemon expects to gate on.
//!   * [`ipc`]   — unix-socket frames (hook posts + supervisor registration).
//!   * [`ws`]    — the tailnet WebSocket protocol the iPhone speaks.
//!
//! [`tmux`] is here for the same reason, even though it is not a wire type: two
//! processes ask tmux whether a session is still alive, and a machine where they
//! answered that differently would report exits that never happened. The
//! question is shared vocabulary; only the way each crate runs a child is not.
//!
//! Everything is plain serde JSON. Unknown fields are tolerated on decode and
//! unknown event kinds round-trip as [`event::EventKind::Other`], so a newer
//! daemon can add facts without breaking an older client (additive-only rule).

use std::path::PathBuf;

pub mod agent;
pub mod build_identity;
pub mod composite_id;
pub mod config;
pub mod event;
pub mod fsperm;
pub mod hash;
pub mod hook;
pub mod ipc;
pub mod pairing;
pub mod proc;
pub mod proc_identity;
pub mod risk;
pub mod secret;
pub mod time;
pub mod tmux;
pub mod uid;
pub mod ws;

/// Bumped only on breaking changes; negotiated in `hello`/`hello_ack`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Bumped on every *additive* change, and reported in `hello_ack`.
///
/// The major version answers "can we talk at all"; this answers "what may I
/// assume is present". A client that needs `TurnComplete` or `get_diff` tests
/// `protocol_minor >= 1` rather than probing for the feature and guessing from
/// the silence — which is the same honesty rule the event log follows.
///
///   * `0` — hello/sessions/subscribe/answer/send_text/capture.
///   * `1` — QR pairing + per-device tokens, `wss://`, `get_diff`,
///     `EventKind::TurnComplete`, `ResolvedBy::Local`, `risk_class`.
///   * `2` — `session_uid` on every session and event, accepted wherever a
///     `session_id` is accepted, and `answer{session_id}`. A client on minor 2
///     keys its local store by `session_uid`; one on minor 1 keeps using
///     `session_id` and gets the newest run under that name.
///   * `3` — `send_text{request_id, payload_hash}` (idempotent, and the
///     server now chooses the interlock — `send_text.require` is ignored),
///     `SendTextResult::{Duplicate, Indeterminate}`, `ApprovalCard{generation,
///     identity_bound}`, `AnswerOutcome{indeterminate}`, and the
///     `send_text_idempotent` / `prompt_identity` capabilities. The supervisor
///     side of the same number is [`ipc::RegisterSession::protocol_minor`]:
///     minor 3 is what makes a supervisor able to honour a prompt fingerprint.
///   * `4` — hardening. Everything here is additive, and a client written
///     against minor 3 needs no change:
///       - a new `error.code`, `protocol_mismatch`, sent to a client whose
///         **major** differs. Minor 3 clients speak major 1 and never see it —
///         the majors have not moved. What did change is that a mismatched major
///         is now *refused* rather than warned about and admitted.
///       - a new `error.code`, `message_too_large`, for a reply that would not
///         fit one WebSocket frame.
///       - an `event` whose payload carries `codeconnect_truncated: true` in
///         place of an oversized one. Same envelope, same `seq`, so a client
///         that does not know the key still advances its watermark correctly.
///       - two new [`event::EventKind::Error`] payload shapes from the
///         transcript tailer, discriminated by an `error` field:
///         `transcript_line_too_long` and `transcript_line_unreadable`. `Error`
///         is not a new kind, so an older client renders them as it already
///         renders any error.
///   * `5` — **`lifecycle` is reconciled rather than merely remembered.** Up to
///     minor 4 the only thing that could ever set [`event::Lifecycle::Exited`]
///     was a supervisor reporting its own exit, so a session whose supervisor
///     was killed, or that died while `ccd` was down, stayed `live` in the fleet
///     for ever. A client had no way to know that, and rendered "Running" for
///     agents that had been gone for days. From minor 5 the daemon proves
///     liveness against tmux at startup and on a sweep, so `live` means it has
///     positive evidence rather than an absence of news. Additive on the wire:
///       - a `session_end` the daemon *derived* that way carries a `reason`
///         string alongside the existing `exit_code`, so the log says how the
///         end was established rather than implying somebody watched it happen.
///         The envelope and the kind are unchanged.
///       - nothing is ever marked exited on ambiguous evidence, so
///         [`event::Lifecycle::Unknown`] remains a state a client must render.
///   * `6` — push. A new client message, [`ws::ClientMessage::RegisterPush`],
///     carrying an APNs token and the environment it was issued for, plus the
///     `push` capability that says the daemon can act on one. Separate from
///     `hello` because notification permission can be granted or revoked at any
///     point in a session's life. Additive: a client that never sends it is
///     unchanged.
///   * `7` — a client can delete one ended run. A new client message,
///     [`ws::ClientMessage::DeleteSession`], a new
///     [`ws::ServerMessage::DeleteSessionResult`] answering it, and the
///     `delete_session` capability. Three things a client must know:
///       - it names the run by `session_uid` and never by `session_id`. A tmux
///         name is handed to the next run, so a name is not an identity a
///         destructive verb may be pointed at.
///       - for a run the daemon hosts, it refuses anything not `Exited`, and
///         anything it still holds live state for. `still_running` and
///         `not_exited` are complete answers, not errors. A run it never hosted
///         — an adopted session, recognisable by its empty `tmux_session` in
///         the summary — is deletable at any lifecycle: no probe can ever prove
///         such a run ended, and a record that cannot be removed until an
///         unobtainable proof arrives would be immortal. Claude Code's own
///         transcript is untouched either way.
///       - the daemon deletes its own record. Claude Code's transcript is its
///         own file and is untouched, so `claude --resume` still works
///         afterwards.
///
///     Also in 7: [`ws::ClientMessage::TestPush`] and its
///     [`ws::ServerMessage::TestPushResult`], gated by the `test_push`
///     capability — one real APNs notification to the requesting device, so
///     the doorbell can be proven rather than trusted. Refused for
///     static-token connections and rate-limited per device.
///
///   * `8` — a client can ask which slash commands the session's Claude Code
///     actually has. [`ws::ClientMessage::GetCommandCatalog`], answered by
///     [`ws::ServerMessage::CommandCatalog`], gated by the `command_catalog`
///     capability. The list is read from the installed binary's own
///     machine-readable init message and cached per binary fingerprint —
///     never hand-maintained, so a Claude Code upgrade changes the answer
///     instead of rotting a copy. `unavailable` is a complete answer: the
///     phone falls back to its conservative static policy, never to guessing.
///   * `9` — the daemon closes a Mac view its own injection opened.
///     Measured problem: `/status`, `/usage`, `/help`, `/export`, `/diff`
///     and bare `/model` replace Claude's composer, and while it is gone the
///     presence interlock refuses every further send — a phone that typed
///     one was locked out of its own session until somebody pressed Esc at
///     the Mac. From minor 9 the supervisor checks the composer after any
///     word-shaped slash injection, sends exactly one `Escape` if it is
///     gone, and reports what it observed:
///     [`ws::SendTextResult::ComposerRecovered`] (with the pane it captured,
///     for `/status`, `/usage` and `/cost` only) or
///     [`ws::SendTextResult::ComposerLost`] when one Escape was not enough —
///     measured on `/config`, and on `/keybindings`, which opens an editor.
///     Advertised as the `slash_composer_recovery` capability, and enforced
///     per session: a supervisor below minor 9 accepts the request fields and
///     drops them, so the daemon refuses word-shaped slash commands for that
///     session rather than typing one it cannot rescue.
///
///     **The new statuses are sent to every client**, because a `hello`
///     carries no client minor for the daemon to branch on. That is safe
///     forwards — a client built against this or later knows them — and it is
///     the reason `SendTextResult` decoding should treat an unknown status as
///     indeterminate rather than as a decode failure.
///   * `10` — the daemon *completes* a confirmation its own injection
///     opened, rather than dismissing it. `/model <value>` and
///     `/effort <value>` make Claude Code ask before switching, and the
///     composer-recovery Escape from minor 9 answered "no" to a question the
///     human had already answered on the phone. From minor 10 the supervisor
///     may send `Enter` instead, but only when the client set
///     `complete_native_confirmation`, the command is one of those two with an
///     argument, and the pane it captured still shows that argument selected.
///     Advertised per session, so a supervisor below minor 10 keeps escaping.
///   * `13` — a client can open a **live terminal** on a hosted session over
///     this same connection. `terminal_attach`/`terminal_input`/
///     `terminal_resize`/`terminal_credit`/`terminal_detach` from the client;
///     `terminal_attached`/`terminal_output`/`terminal_credit`/
///     `terminal_closed` from the daemon; gated by the `terminal_pty`
///     capability. Bytes ride as base64 in the JSON frames, flow-controlled by
///     a credit window in each direction (no sequence number — the socket is
///     ordered). The daemon speaks tmux **control mode** through a disposable
///     `tmux -N -C attach -f ignore-size` client against the exact session:
///     keystrokes are delivered with `send-keys` to a scoped
///     `session:window.pane` target — the bound pane the phone is shown, so a
///     migrated pane fails closed — and can never be interpreted as a tmux
///     control-protocol command (the shell in the pane can of course run `tmux`
///     itself — a terminal is a real shell); the phone never resizes a human at
///     the Mac; and the tmux
///     server, agent and supervisor outlive the attachment. The first bytes
///     after `terminal_attached` repaint the pane's current screen (clear,
///     rows, cursor), so an attach shows the session as it stands rather than
///     a blank viewport waiting for new output — ordinary `terminal_output`
///     bytes, spending credit like any others. Because a terminal
///     is shell-equivalent authority, the capability is connection-scoped:
///     false for the static bootstrap token.
///     Additive: an older daemon omits the capability and the phone offers no
///     Terminal; an older client ignores the messages.
///
///     *Shipped under 13 without adding to the wire.* The composer is now
///     recognised by the box Claude draws it in rather than by footer copy that
///     yields to other hints, and a pane where tmux holds the keyboard refuses
///     a send instead of reporting one that never arrived. No message, field or
///     capability changed — but [`ws::SendTextResult::Sent`]`.matched` gained a
///     value it can carry, the literal `composer`, for a send no needle
///     authorised. It is a reason, not an enumerable set; a client renders it
///     and must not match on it.
///   * `12` — a send carries **when its asker stops listening**.
///     `SupervisorRequest::SendText.respond_by_monotonic_ms` stamps the
///     daemon's own answer deadline, on the host's monotonic clock, into the
///     request. The supervisor budgets every deliberate wait against it —
///     stopping recovery honestly when the remainder cannot fit the next
///     step — so an answer computed in time is delivered in time, including
///     time the request spent queued. Additive: an older supervisor ignores
///     the field and budgets from its own config; an older daemon omits it.
///   * `11` — a run carries the **project** it is working in.
///     `SessionSummary.project_label` is the final component of `cwd` — trimmed,
///     stripped of anything that would break a line, and bounded — resolved by
///     the daemon rather than by each client, so that every surface naming a
///     run names it the same. A client must not compute its own from `cwd`:
///     two rules produce two names for one run. Empty when `cwd` names nothing, and
///     empty from any daemon below this minor — the two mean the same thing to a
///     reader: nobody has said what this project is. A client says so rather
///     than falling back to the tmux name, which is a reused counter.
///     Additive; an older client ignores the field and an older daemon omits it.
///     The **direct** notification names that project, but only while it speaks
///     for a single run: with no other run blocked the title is the triggering
///     run's label; with exactly one run blocked it is that sole blocked run's
///     label; with more than one blocked the project label is deliberately
///     suppressed — the title is `CodeConnect` and the body is `{n} agents need
///     you`. A single-run title with no label is `CodeConnect` too, and each
///     single-run body is one of four canned sentences chosen by the hook's
///     kind. The tool name and the risk class it used to carry are gone. (The
///     relay notification added at minor 14 deliberately carries no project
///     label and is titled `CodeConnect`.)
///     A tap opens the phone's decision list when an approval rang, and the
///     fleet otherwise — the list, never a particular card. The payload carries
///     no routing, session, request or device identifier, only which of four
///     kinds rang — so there is nothing in it that could point at a decision
///     somebody has since answered. The words themselves are a snapshot, like
///     any notification's.
///     Every surface that names a run — the fleet list, the decision card, the
///     session header, the diff title and the notification — takes its name
///     from this field and never derives one of its own, so a name on a lock
///     screen and a name in the app are the same name rather than two rules'
///     answers. A client may *add* to it where a screen has to tell two runs in
///     one project apart; it may not replace it. A run that changes directory
///     while a card is open takes that card with it: every writer of a run's
///     `cwd` relabels the cards it is holding, so a single-run doorbell names the
///     project the run is in when it rings, which is the project the app is
///     showing (a fleet doorbell, with more than one run blocked, is titled
///     `CodeConnect` and names no project).
///
///     **One bound, stated rather than implied.** A card takes its run's name
///     when it is filed, and filing is not atomic with the relabel: a card
///     raised in the same instant a run moves can be inserted carrying the
///     previous name. It is corrected by the next thing that writes that run's
///     `cwd`, and it is gone when the card resolves. The daemon does not
///     serialise every hook behind a database write to close that instant — a
///     doorbell is best-effort by construction, the app reconciles from the
///     event log, and the cost of the alternative is paid by every hook.
///   * `14` — a daemon with **no Apple key of its own** can ring a phone. The
///     key cannot be shipped to a customer's Mac, so a CodeConnect-operated
///     relay holds it: the daemon hands over a closed event document — the
///     schema, the device's token and its environment, and either which of four
///     kinds rang with how many runs are blocked or the fixed test variant —
///     with the opaque credential riding only in the `Authorization` header,
///     never in the body; the relay composes a generic alert titled
///     `CodeConnect`, carrying no project label, and talks to Apple. Every
///     decision about *whether* to ring stays on the Mac. Four
///     additions, and a client written against minor 13 needs none of them:
///       - [`ws::Capabilities::push_relay`] — this daemon sends through the
///         relay. **One-hot with the legacy `push`,** which from here means
///         *direct key* and nothing else: a relay daemon advertises
///         `push = false` on purpose, because a client that predates this minor
///         would otherwise ask for notification permission and register a token
///         with no credential attached, and every send would be refused. A
///         client on this minor resolves the two flags with `push` taking
///         precedence.
///       - [`ws::ClientMessage::RegisterPush`]`.relay_credential` — the opaque
///         bearer the phone obtained from the relay, absent for a direct
///         registration. The daemon stores it beside the token and presents it
///         on every send; it never mints one, and never sees the attestation
///         that authorised it.
///       - [`ws::TestPushResult::CredentialInvalid`] — the relay refused the
///         credential. Distinct from `no_registered_token`: what the relay
///         refused is the bearer, not the APNs registration, so a client must
///         not clear the token on this alone. It is not proof the token is good
///         either — a bearer bound to another token returns it, and so does a
///         binding the relay already retired on an APNs `410`. The repair is to
///         renew the bearer, not to throw away a token that may still be live.
///       - `hello_ack.push_environment` — the daemon's authoritative APNs
///         environment for this device's registered token, absent when no token
///         is registered. The relay's binding is the single authority for a
///         token's environment and corrects the daemon on an accepted send;
///         this is how that correction reaches the phone, which persists it
///         rather than resending the value it first cached.
///   * `15` — **the agent seam.** Everything here is additive and a client
///     written against minor 14 needs none of it; it is the wire, storage and
///     config groundwork for a second agent (Codex) whose *behaviour* lands in
///     later work. A Claude session is byte-identical to minor 14 — the point
///     of the number is that a peer can now *say* Codex without a Claude peer
///     having to understand it.
///       - [`agent::AgentKind`] (`claude` | `codex` | a preserved
///         `Unsupported` name). Absent decodes as Claude; an **unrecognised**
///         name never does — it fails closed. It rides
///         [`ipc::RegisterSession::agent`], [`event::SessionSummary::agent`],
///         and the session storage row.
///       - additive [`ipc::RegisterSession`] fields — `agent`, `agent_bin`,
///         `codex_thread_id`, `codex_socket`, `codex_generation` — all
///         tolerated-absent by an older daemon, and a pre-`Register`
///         support-negotiation pair ([`ipc::ClientFrame::NegotiateSupport`] /
///         [`ipc::DaemonFrame::SupportedAgents`]). The daemon adopts a
///         registration only when its `codex_generation` is not older than one
///         it already holds, so a stale supervisor frame cannot overwrite newer
///         adapter state.
///       - [`ws::Capabilities::supported_agents`] (the daemon's honest list —
///         `["claude"]` until Codex actuation ships), [`ws::ClientFeatures`] on
///         the [`ws::ClientMessage::Hello`] and [`ws::ClientMessage::RegisterPush`]
///         frames (a client that advertises no agents is Claude-only), and a
///         per-session [`event::SessionSummary::agent`] fact. A per-session fact
///         is authoritative; a missing one falls back to connection-global
///         **only for Claude**, and Codex or an unknown agent fails closed.
///       - the [`ws::CodexResolution`] envelope (a *separate* discriminated
///         type, so Claude's [`ws::AnswerOutcome`]/[`ws::AnswerResult`] stay
///         byte-identical), an additive opaque [`ws::AnswerDecision::OptionId`]
///         variant, and an [`ws::ClientMessage::Interrupt`] operation
///         (**defined here, and refused daemon-side at this minor**; honoured
///         from minor 17).
///       - the [`composite_id`] wire-id codec: a versioned, type-tagged,
///         length-bounded base64url encoding of `(session_uid, thread_id,
///         server_request_id, generation)`, opaque to the phone's request-id
///         correlation.
///
///     [`ws::ClientFeatures`] stays wire-legal on `hello` and `register_push`
///     and is **ignored**: no shipping client encodes it, so the daemon stores
///     nothing and every device sits at the Claude-only floor. The read that
///     authorizes a push is nonetheless already fail-closed — a stored set not
///     confirmed under the running daemon's epoch authorizes nothing rather than
///     falling back to the floor — which is what keeps a Codex doorbell from
///     reaching a phone that cannot render one. The write arrives with the phone
///     work that can advertise it.
///
///     [`ipc::RegisterSession::exit_replay`] rides here too, and it is the one
///     addition that is not about agents at all: it marks the registration a
///     supervisor replays on the way to reporting its own exit, so a daemon can
///     tell a corpse's frame from a supervisor arriving. **The number is not
///     bumped for it and must not be.** Nothing negotiates on it: absent decodes
///     `false`, which is precisely the behaviour that shipped, and an older daemon
///     ignores the field entirely. It only ever *withholds* an adoption — there is
///     no peer that has to understand it in order to stay correct, which is the
///     only thing the minor exists to say.
///   * `17` — **`interrupt` is honoured rather than refused.** Minor 15 put the
///     [`ws::ClientMessage::Interrupt`] operation on the wire and the daemon
///     answered [`ws::InterruptResult::Rejected`] to every one of them. From here
///     the daemon actually aborts the turn a Codex session is running: the local
///     gate binds the ask to the exact turn, thread and visit generation it holds,
///     the broker binds it again to the session's own active turn, and the
///     durable claim makes a retry replay a recorded outcome instead of stopping
///     something twice. Two things a client must know, and a client written
///     against minor 16 needs neither:
///       - [`ws::Capabilities::interrupt`] — this daemon honours the operation.
///         Advertised rather than assumed, because nothing else on the wire
///         separates a daemon that stops the turn from one that refuses every
///         ask: both accept the message and both answer an `interrupt_result`. A
///         phone would otherwise have to tap a Stop button to discover it does
///         nothing, and this app's rule is that an action the daemon cannot
///         perform is not offered. Absent decodes `false`, which is exactly what
///         an older daemon meant. The flag is a build fact and is *not* the whole
///         test: interrupt exists only for Codex, so a client scopes the control
///         by the session's [`event::SessionSummary::agent`] as well.
///       - the other three statuses actually arrive. `aborted`, `duplicate` and
///         `indeterminate` were wire-legal from minor 15 and never sent; they are
///         sent now, and they carry different news — the turn stopped, it had
///         already stopped, or a stop was issued whose outcome nobody can name.
///         **Sent to every client**, for the reason minor 9 gives about
///         `SendTextResult`: a `hello` carries no client minor to branch on, so a
///         decoder that treats an unrecognised status as a decode failure rather
///         than as indeterminate was always the thing that would break.
///
///     Nothing changed shape. [`ws::ClientMessage::Interrupt`]'s fields and
///     [`ws::InterruptResult`]'s variants are byte-identical to minor 15; what
///     changed is that the daemon now does the thing, which is precisely the kind
///     of fact a minor exists to let a peer assume rather than probe for.
pub const PROTOCOL_MINOR: u32 = 17;

/// Private tmux server name. Never the user's default server.
pub const TMUX_SOCKET_NAME: &str = "codeconnect";

/// Session names are `cc-<n>`; the prefix is also the tmux session prefix.
///
/// The name is reused: `codeconnect claude` picks the lowest free number, so a `cc-1`
/// that exits frees the name for the next session. That is deliberate — it is
/// what keeps names short and typeable — and it is exactly why the *identity*
/// of a run is [`uid`], not this.
pub const SESSION_PREFIX: &str = "cc-";

/// Environment variable carrying the CodeConnect session id into the agent
/// process, so hooks can identify themselves even without an explicit `--session`.
pub const ENV_SESSION: &str = "CODECONNECT_SESSION";

/// Environment variable carrying the session's unique id, for the same reason
/// and with the same fallback role as [`ENV_SESSION`].
pub const ENV_SESSION_UID: &str = "CODECONNECT_SESSION_UID";

/// The LaunchAgent label. One constant so `codeconnect daemon install`, `codeconnect daemon
/// status` and the daemon's own "am I launchd-managed?" check can never disagree
/// about which job they are talking about.
pub const LAUNCHD_LABEL: &str = "com.codeconnect.ccd";

/// Root of all CodeConnect state. `CODECONNECT_HOME` exists so tests never
/// touch the real `~/.codeconnect`.
///
/// **Absolute whenever the cwd can be read** (A9.6(c)), and that is the whole point
/// of the wrapper. Both sources here can be relative — `CODECONNECT_HOME` is
/// whatever the operator exported, and the `$HOME`-less fallback is literally
/// `./.codeconnect` — and a relative root is not a root at all: it names a different
/// directory in every process that resolves it. The launcher, the pane's host and
/// the sweep run with **different working directories** by construction (the
/// coordinator hands tmux an explicit `-c`), so a process-relative root has them
/// addressing different records for the same session. Absolutising HERE, where the
/// root is first read, is what makes every consumer — `session_dir`, the launch
/// lock, the `CODECONNECT_HOME` the coordinator forwards into the pane — name one
/// directory.
///
/// `std::path::absolute` is prefix-only (it prepends the cwd and drops `.`
/// components; it resolves no symlinks and no `..`), so it is idempotent: an
/// already-absolute root is returned unchanged, and forwarding this value to a
/// child that calls `root_dir()` again yields the same path (measured).
///
/// **When it cannot be made absolute, this FAILS CLOSED** (round-4 finding 5). The
/// absolutisation can fail, and the previous revision returned the configured value
/// when it did — which is relative — on an argument that is now measured false.
///
/// That argument was: `std::path::absolute` fails only via `getcwd`; `getcwd` fails
/// only on a cwd unlinked out from under the process; and in that state every
/// relative path operation fails `ENOENT` too, so a relative root addresses nothing
/// rather than something else. The first two clauses are wrong on Darwin. Measured
/// here (Darwin 25.5.0, non-root): `getcwd` **also** fails `EACCES` when any ancestor
/// of the cwd loses its **search** bit — `chmod 0000`, `0400`, `0444`, `0666` on a
/// single ancestor all reproduce it, while `0111` and `0555` do not, so it is the
/// `x` bit and not the `r` bit — and in *that* state `stat(".")`, `mkdir("x")`,
/// `open("y", O_CREAT)`, `fs::write` and `fs::read` from the held cwd **all
/// succeed**. So a relative root does not address nothing; it addresses whatever
/// directory each process happens to be sitting in, which is precisely the
/// divergent-tree hazard this wrapper exists to prevent, and it does so while the
/// process remains perfectly able to create and read the wrong tree.
///
/// The failure arm therefore returns [`UNAVAILABLE_ROOT`] instead: an **absolute**
/// path under `/dev/null`. Three properties, all measured, are why that is a
/// fail-closed answer and not another wrong value:
///
///   * every filesystem operation under it fails `ENOTDIR` (20) — `create_dir_all`,
///     `metadata`, `write`, `read`, `read_dir`, `remove_dir` — because `/dev/null` is
///     a character device, so no process can create this tree even by accident;
///   * it is **absolute**, so it is byte-identical in every process regardless of
///     cwd, and cannot reintroduce the divergence;
///   * `std::path::absolute` is a no-op on it, so a `CODECONNECT_HOME` forwarded to
///     a child resolves to the same unusable root there.
///
/// The result is that `private_dir`, the launch lock, `store_atomic` and the record
/// read all fail, and the launch fails closed — which is what the old doc *claimed*
/// the relative fallback achieved and did not.
///
/// Note the failure arm is only reachable for a **relative** configured root:
/// measured, `std::path::absolute` on an already-absolute input never calls `getcwd`
/// and succeeds in every condition that breaks it. An operator with an absolute
/// `CODECONNECT_HOME` — and the `$HOME`-derived default whenever `$HOME` is absolute
/// — can never reach it.
pub fn root_dir() -> PathBuf {
    let configured = match std::env::var_os("CODECONNECT_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => home_dir().join(".codeconnect"),
    };
    std::path::absolute(&configured).unwrap_or_else(|_| PathBuf::from(UNAVAILABLE_ROOT))
}

/// The root returned when a **relative** configured root cannot be absolutised.
///
/// Not a placeholder to be special-cased: it is the fail-closed answer itself. Under
/// `/dev/null` — a character device — every path operation fails `ENOTDIR`, so a
/// process holding this root cannot create, read or delete any CodeConnect state.
/// See [`root_dir`] for why an unusable absolute root is strictly safer than a usable
/// relative one.
pub const UNAVAILABLE_ROOT: &str = "/dev/null/codeconnect-root-unavailable";

/// `$HOME`, falling back to the current directory so nothing panics in a
/// launchd context with a stripped environment.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn socket_path() -> PathBuf {
    root_dir().join("ccd.sock")
}

pub fn db_path() -> PathBuf {
    root_dir().join("events.db")
}

/// The static bearer token. QR-delivered per-device tokens live alongside it;
/// this one stays valid until the operator deletes it, so a phone that paired
/// before per-device tokens existed never locks itself out.
pub fn token_path() -> PathBuf {
    root_dir().join("token")
}

/// Cached `tailscale cert` material. Owner-only: the private key lives here.
pub fn tls_dir() -> PathBuf {
    root_dir().join("tls")
}

pub fn sessions_dir() -> PathBuf {
    root_dir().join("sessions")
}

pub fn logs_dir() -> PathBuf {
    root_dir().join("logs")
}

/// Where launchd is told to send the daemon's stdout, and where the daemon
/// looks when it rotates its own log.
///
/// Both halves of that sentence are why these are functions here rather than
/// strings in two files: `codeconnect daemon install` writes the path into the plist and
/// `ccd` truncates the same path when it grows past the cap. If they ever
/// disagreed the log would grow without bound and nothing would say so.
pub fn daemon_stdout_log() -> PathBuf {
    logs_dir().join("ccd.out.log")
}

pub fn daemon_stderr_log() -> PathBuf {
    logs_dir().join("ccd.err.log")
}

/// Where a Codex session's **broker decision log** is kept after the session is
/// over, beside the supervisor and coordinator logs.
///
/// The broker writes that log into its host's run dir, which is disposable by
/// design — three unix sockets and two log files under `/tmp`, removed on every
/// exit path the host has. Measured: a launch whose first `thread/start` is
/// refused says so in exactly one place, that file, and the sweep then deletes it
/// before anyone can read it, so the user is left with an empty pane and no
/// account of why. The host copies the file here on the way out
/// (`codeconnect::codex_host`), and `ccd` names this path in the `session_end`
/// reason it files for a session that never bound a thread — which is why the
/// spelling lives here rather than in either of them.
pub fn codex_broker_log(session_name: &str, session_uid: &str) -> PathBuf {
    logs_dir().join(format!("broker-{session_name}-{session_uid}.log"))
}

/// Keep only the `keep` most recently modified `~/.codeconnect/logs/<prefix>*`
/// files; delete the rest. Returns how many were removed.
///
/// **Written as one policy over a named family rather than as a rule about broker
/// logs**, because the problem is not the broker's: every per-session log in this
/// directory is minted per uid and none of them has ever been pruned. Measured on a
/// working install before this existed — 2542 files, 10 MB, of which 2487 were
/// `supervisor-*` — so "one file per session, for ever" is the directory's existing
/// habit and the broker family would simply have joined it. A family-agnostic helper
/// is what lets the other families adopt the same bound without a second, differently
/// argued implementation.
///
/// Newest-first by mtime, and a file whose mtime cannot be read sorts oldest so it is
/// a candidate for removal rather than an immortal one — an unreadable timestamp must
/// not be a way to pin a file in place for ever.
///
/// Best-effort throughout: this runs on the way out of a session that has already
/// ended, and a directory that cannot be read or a file that cannot be removed is not
/// worth failing a teardown over. It never touches a name outside the prefix, so the
/// daemon's own `ccd.err.log` and anything an operator has put here by hand are out of
/// its reach by construction.
pub fn prune_session_logs(prefix: &str, keep: usize) -> usize {
    prune_logs_in_dir(&logs_dir(), prefix, keep)
}

/// [`prune_session_logs`] with the directory passed in rather than read from
/// process-global state.
///
/// Split out so the policy can be tested against a directory of its own.
/// `logs_dir()` resolves `CODECONNECT_HOME`, and a test that set that variable to
/// exercise this would be mutating state every other test in the process shares —
/// which is not a hypothetical: this module already has three tests that set it, and
/// the first version of the retention test raced them into a failure. A function that
/// takes its directory cannot have that bug, and the public wrapper above is then the
/// only thing that needs to know where the logs live.
pub fn prune_logs_in_dir(dir: &std::path::Path, prefix: &str, keep: usize) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut matching: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(prefix))
                && e.file_type().is_ok_and(|t| t.is_file())
        })
        .map(|e| {
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (mtime, e.path())
        })
        .collect();
    if matching.len() <= keep {
        return 0;
    }
    matching.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    matching
        .into_iter()
        .skip(keep)
        .filter(|(_, path)| std::fs::remove_file(path).is_ok())
        .count()
}

#[cfg(test)]
mod log_retention_tests {
    /// **The pruner keeps `keep` files and touches nothing outside its family.**
    ///
    /// Both halves matter. The count is the bound the review asked for; the prefix
    /// confinement is what makes it safe to run from a session teardown at all — the
    /// daemon's own `ccd.err.log` and anything an operator has put in this directory
    /// must be out of reach by construction, not by luck of ordering.
    ///
    /// Runs against a directory of its own and never touches `CODECONNECT_HOME`: three
    /// other tests in this module set that variable, and the first version of this one
    /// set it too and raced them into a failure. `prune_logs_in_dir` takes its directory
    /// precisely so this test needs no process-global state.
    #[test]
    fn it_keeps_the_newest_and_never_leaves_its_prefix() {
        let logs = std::env::temp_dir().join(format!("cc-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&logs);
        std::fs::create_dir_all(&logs).unwrap();

        // Ten in the family, written oldest-first, plus two files that are not.
        for i in 0..10 {
            let p = logs.join(format!("broker-cc-1-{i:02}.log"));
            std::fs::write(&p, format!("log {i}")).unwrap();
            // Distinct mtimes, so "newest" is a fact and not a tie.
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i);
            filetime_set(&p, t);
        }
        std::fs::write(logs.join("ccd.err.log"), "daemon").unwrap();
        std::fs::write(logs.join("supervisor-cc-1-x.log"), "other family").unwrap();

        let removed = super::prune_logs_in_dir(&logs, "broker-", 3);
        assert_eq!(removed, 7, "ten in the family, three kept");

        let mut left: Vec<String> = std::fs::read_dir(&logs)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "broker-cc-1-07.log".to_string(),
                "broker-cc-1-08.log".to_string(),
                "broker-cc-1-09.log".to_string(),
                "ccd.err.log".to_string(),
                "supervisor-cc-1-x.log".to_string(),
            ],
            "the three NEWEST of the family survive, and neither the daemon's log nor \
             another family is touched"
        );

        // Under the bound is a no-op, not a rewrite.
        assert_eq!(super::prune_logs_in_dir(&logs, "broker-", 50), 0);
        let _ = std::fs::remove_dir_all(&logs);
    }

    /// `utimes(2)` through libc, so the test can make mtimes deterministic without
    /// pulling a crate in for three lines.
    fn filetime_set(path: &std::path::Path, t: std::time::SystemTime) {
        let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let times = [
            libc::timeval {
                tv_sec: secs,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: secs,
                tv_usec: 0,
            },
        ];
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) }, 0);
    }
}

pub fn config_path() -> PathBuf {
    root_dir().join("config.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_honours_override() {
        // Serialised implicitly: this is the only test touching the var, and it
        // stays the only one — everything about the override is asserted here.
        std::env::set_var("CODECONNECT_HOME", "/tmp/cc-test-home");
        assert_eq!(root_dir(), PathBuf::from("/tmp/cc-test-home"));
        assert_eq!(socket_path(), PathBuf::from("/tmp/cc-test-home/ccd.sock"));

        // A9.6(c): a RELATIVE override is absolutised, because the launcher, the
        // pane's host and the sweep do not share a working directory — a
        // process-relative root has them addressing different records.
        std::env::set_var("CODECONNECT_HOME", "rel-cc-home");
        let root = root_dir();
        assert!(
            root.is_absolute(),
            "a relative CODECONNECT_HOME must not stay relative: {}",
            root.display()
        );
        assert_eq!(root, std::env::current_dir().unwrap().join("rel-cc-home"));
        // Every consumer inherits it, so nothing downstream has to re-normalise.
        assert!(sessions_dir().is_absolute());
        assert!(socket_path().is_absolute());
        // Idempotent: this value is forwarded to the pane as `CODECONNECT_HOME`,
        // and the child resolves it again in a DIFFERENT working directory. Only
        // a fixed point makes both processes name one record.
        std::env::set_var("CODECONNECT_HOME", &root);
        assert_eq!(root_dir(), root);

        std::env::remove_var("CODECONNECT_HOME");
    }

    /// Round-4 finding 5: the arm reached when a **relative** root cannot be
    /// absolutised must be fail-CLOSED, not merely honest.
    ///
    /// The condition itself — `getcwd` failing `EACCES` because an ancestor of the
    /// cwd lost its search bit — is measured (see [`root_dir`]) but cannot be staged
    /// in-process: `set_current_dir` and the ancestor's mode are both process-global,
    /// and this crate's tests run in parallel, so reproducing it here would corrupt
    /// every other test's filesystem view. What IS asserted here is the property the
    /// whole fix rests on, and it is the property that would silently rot: that the
    /// value the arm returns cannot address anything.
    #[test]
    fn the_unavailable_root_addresses_nothing() {
        let root = PathBuf::from(UNAVAILABLE_ROOT);
        // Absolute — so it is byte-identical in every process and cannot
        // reintroduce the divergence a relative root causes.
        assert!(root.is_absolute(), "{}", root.display());
        // …and a fixed point, so a child that resolves a forwarded
        // `CODECONNECT_HOME` lands on the same unusable root.
        assert_eq!(std::path::absolute(&root).unwrap(), root);

        // Every operation CodeConnect performs on its root fails, and fails with
        // `ENOTDIR` — the parent is a character device, so no process can create
        // this tree even by accident. This is what makes `private_dir`, the launch
        // lock and `store_atomic` fail the launch closed.
        let enotdir = |err: std::io::Error| {
            assert_eq!(
                err.raw_os_error(),
                Some(libc::ENOTDIR),
                "expected ENOTDIR, got {err}"
            );
        };
        enotdir(std::fs::create_dir_all(&root).unwrap_err());
        enotdir(std::fs::metadata(&root).unwrap_err());
        enotdir(std::fs::read_dir(&root).unwrap_err());
        enotdir(std::fs::remove_dir(&root).unwrap_err());
        enotdir(std::fs::write(root.join("token"), b"x").unwrap_err());
        enotdir(std::fs::read(root.join("token")).unwrap_err());
        // And the private-dir boundary — the one the launch path actually calls —
        // refuses too, which is the sentence "the launch fails closed" in code.
        assert!(fsperm::private_dir(&root).is_err());
    }
}
