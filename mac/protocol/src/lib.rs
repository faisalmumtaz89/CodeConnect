//! Shared CodeConnect wire types.
//!
//! These surfaces live here so `ccd` and `codeconnect` can never disagree:
//!   * [`event`] — the daemon-assigned event log envelope (source of truth).
//!   * [`config`] — one config file, shared.
//!   * [`ipc`]   — unix-socket frames (the CLI and each run's mail server).
//!   * [`ws`]    — the tailnet WebSocket protocol the iPhone speaks.
//!   * [`host`]  — the lines between `ccd` and the agent host of each Claude
//!     run.
//!   * [`sdk_map`] — the Agent SDK's messages as events in the log.
//!
//! Everything is plain serde JSON. Unknown fields are tolerated on decode and
//! unknown event kinds round-trip as [`event::EventKind::Other`], so a newer
//! daemon can add facts without breaking an older client (additive-only rule).

use std::path::PathBuf;

pub mod agent;
pub mod build_identity;
pub mod claude_bin;
pub mod codex_bin;
pub mod composite_id;
pub mod config;
pub mod event;
pub mod facts;
pub mod fdlimit;
pub mod fsperm;
pub mod hash;
pub mod host;
pub mod ipc;
pub mod pairing;
pub mod peer;
pub mod proc_identity;
pub mod risk;
pub mod sdk_map;
pub mod secret;
pub mod tailnet;
pub mod time;
pub mod uid;
pub mod ws;

/// Bumped only on breaking changes; negotiated in `hello`/`hello_ack`.
pub const PROTOCOL_VERSION: u32 = 2;

/// What a client on another major is told, before any credential is read. The Mac app, the iPhone app and the
/// service ship together, so the sentence names no side.
pub const PROTOCOL_MISMATCH: &str =
    "This computer runs a different version of CodeConnect. Update CodeConnect on both, then try again.";

/// Bumped on every *additive* change, and reported in `hello_ack`.
///
/// The major version answers "can we talk at all"; this answers "what may I
/// assume is present". A client that needs `TurnComplete` or `get_diff` tests
/// `protocol_minor >= 1` rather than probing for the feature and guessing from
/// the silence — which is the same honesty rule the event log follows.
///
///   * `0` — hello/sessions/subscribe/answer/send_text/capture. (`capture`, the
///     Mac pane snapshot, is gone in X12: no run is left with a pane to read.)
///     **Gone in X12:** a summary's `link` words `degraded` and `stale`; a run's link
///     is `attached` or `detached`. Also the `applied_via` word `send_keys`, since
///     nothing is typed; an outcome's `inferred`; the resolver `superseded`, which no
///     daemon has recorded since minor 22; and the capabilities `fail_mode`,
///     `answer_path` and `hold_secs`, constants no app reads.
///   * `1` — QR pairing + per-device tokens, `wss://`, `get_diff`,
///     `EventKind::TurnComplete`, `ResolvedBy::Local`, `risk_class`. **Gone in
///     X12:** `ResolvedBy::Local`; nothing records an answer made at the keyboard.
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
///     **Gone in X12:** `send_text.require`; a client that still sends it is
///     unaffected, as an unknown key is ignored, not refused.
///   * `4` — hardening. Everything here is additive, and a client written
///     against minor 3 needs no change:
///       - a new `error.code`, `protocol_mismatch`, sent to a client whose
///         **major** differs. Minor 3 clients speak major 1 and never see it —
///         the majors have not moved. What did change is that a mismatched major
///         is now *refused* rather than warned about and admitted.
///       - a new `error.code`, `message_too_large`, for a reply that would not
///         fit one WebSocket frame.
///       - an `event` whose payload carries `_codeconnect_truncated: true` in
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
///     `SendTextResult::ComposerRecovered` (with the pane it captured,
///     for `/status`, `/usage` and `/cost` only) or
///     `SendTextResult::ComposerLost` when one Escape was not enough —
///     measured on `/config`, and on `/keybindings`, which opens an editor.
///     Advertised as the `slash_composer_recovery` capability, and enforced
///     per session: a supervisor below minor 9 accepts the request fields and
///     drops them, so the daemon refuses word-shaped slash commands for that
///     session rather than typing one it cannot rescue. **Gone in X12** with the
///     supervisor: no run is typed into through a pane, so the recovery, its two
///     statuses and the capability no longer exist.
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
///     **Gone in X12:** `complete_native_confirmation`; a send that still carries it
///     is read without it.
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
///     Terminal; an older client ignores the messages. **Gone in X12:** the
///     terminal and its nine messages; its capability is now
///     `private_transport`, which says only whether the connection is unreadable
///     in transit.
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
///     kinds rang with how many decisions are waiting or the fixed test variant —
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
///         `["claude"]` until Codex actuation ships), a `features` agent list on
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
///         from minor 17). **Gone in X12:** the envelope's `local` actor; a Codex
///         card is answered by an app or cleared.
///       - the [`composite_id`] wire-id codec: a versioned, type-tagged,
///         length-bounded base64url encoding of `(session_uid, thread_id,
///         server_request_id, generation)`, opaque to the phone's request-id
///         correlation.
///
///     That `features` list is **no longer a field**: no shipping client ever
///     encoded it, and every iPhone build since 1.1.0 renders and answers a Codex
///     run, so a push is no longer scoped by agent and the daemon has nothing to
///     read it for. Both messages decode with or without the key — an unknown key
///     is ignored, not refused — so a client that still sends it is unaffected.
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
///   * `16` — **a phone answer to a Codex approval, and an `applied_via` that can
///     name what it did.** One addition, [`ws::AnswerPath::CodexResponse`]: the
///     JSON-RPC response to the app-server's own `requestApproval`, written on the
///     Codex link's socket. Neither of the two paths that had existed since minor
///     0 can describe it, and the difference is not cosmetic — nothing is rendered
///     and nothing is typed, so first-answer-wins is a property of the broker's
///     arbiter rather than of a TTY, and the loser of that race is a fact the
///     daemon is *told* rather than one it infers. A successful Codex answer that
///     had to report `send_keys` would have claimed an actuation that never
///     happened, at the Mac's keyboard, in a session Claude was not running.
///
///     **Additive, and a client written against minor 15 cannot even be shown it.**
///     Claude's [`ws::AnswerPath`] serialization is byte-identical: no Claude
///     answer takes this path. `answer_result` is a reply on the connection that
///     sent the `answer`, so the only client that receives a `codex_response` is
///     one that just answered a Codex approval — which a client below minor 15
///     cannot do. What the variant does rely on is a property every decoder should
///     already have had, because this is the *first* variant ever added to the
///     enum: an unrecognised `applied_via` is retained and reported as
///     unvouchable, never treated as a frame failure.
///
///     Nothing else in that work reached the wire. The durable answer claim, the
///     broker's arbiter and the `responseDisposition` receipt it sends the daemon
///     about its own write are Mac-side machinery; they change what the daemon
///     *knows* before it speaks, not what it says, and the number records only the
///     second.
///   * `17` — **`interrupt` is honoured rather than refused.** Minor 15 put the
///     [`ws::ClientMessage::Interrupt`] operation on the wire and the daemon
///     answered [`ws::InterruptResult::Rejected`] to every one of them. From here
///     the daemon actually aborts the turn a Codex session is running: the local
///     gate binds the ask to the exact turn, thread and visit generation it holds,
///     the broker binds it again to the session's own active turn, and the
///     durable claim makes a retry replay a recorded outcome instead of stopping
///     something twice. Two things a client must know, and a client written
///     against minor 16 needs neither:
///       - [`ws::Capabilities::codex_interrupt`] — this daemon honours the operation.
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
///   * `18` — **`compose`: the phone says something to a Codex session.**
///     [`ws::ClientMessage::Compose`] carries words and a ledger identity; the daemon
///     decides what becomes of them from what the session is doing when it writes — a
///     `turn/start` on an idle thread, a `turn/steer` into a running turn — and answers
///     with a [`ws::ComposeResult`] that says which. Three things a client must know,
///     and a client written against minor 17 needs all three:
///       - [`ws::Capabilities::codex_compose`] — this daemon understands the message.
///         Stronger than its interrupt sibling: `compose` is a NEW message, so a daemon
///         below this minor decodes nothing and answers nothing, and a phone that sent
///         one would wait for ever. Absent decodes `false`, which is what such a daemon
///         meant. Compose exists only for Codex, so the affordance is scoped by the
///         session's [`event::SessionSummary::agent`] as well.
///       - **`started` and `steered` are different news and must not be collapsed.** The
///         first says these words began a turn; the second says they joined one already
///         running, whose id is the SAME turn — a steer produces no `turn/started` and
///         the only terminal is the original turn's. A client that rendered both as
///         "sent" would lose the one fact the operator asked for.
///       - **the route is snapshotted, so a duplicate replays what happened rather than
///         what would happen now.** A compose issued as a start is re-issued as a start;
///         it never becomes a steer because the session has since become busy, which
///         would put the words into a turn nobody composed them for.
///
///     Nothing existing changed shape. `send_text` is untouched and stays Claude's: it
///     types at the Mac's TTY and shares no ledger, result type or failure vocabulary
///     with this.
///   * `19` — **a phone can drive a Codex session from the fleet without inferring
///     anything.** Three additive fields, each closing a hole a client would
///     otherwise have had to guess across, and a client written against minor 18
///     needs all three:
///       - [`ws::CodexResolutionPayload::request_id`] — the `approval_resolved`
///         payload for a Codex card now names the card. It carried no id at all, so
///         the only correlation was a `"resolved:"` prefix parse on the event's
///         `source_event_id`: a string parse on an `Option<String>` whose failure is
///         silent and whose symptom is an answered card standing on somebody's phone
///         for ever. Claude's `AnswerOutcome` has carried `request_id` in the payload
///         since minor 0; this is the same field in the same place, so one rule
///         correlates both agents. Flattened onto the resolution, so every `status`
///         arm is byte-identical and a minor-18 decoder still reads the payload.
///       - **`turn_id` on the approval card's event envelope.** `interrupt` requires
///         a turn id and the card gave the phone none, so a Stop offered from a card
///         had to name a turn inferred from the last `tool_call` — an ordering the
///         phone would have been relying on rather than a contract. The
///         `*/requestApproval` frame carries `turnId` as a required field (a frame
///         without it is refused, not carded), so the envelope now carries the turn
///         the card's own request named. Same envelope field every other Codex event
///         already uses.
///       - [`event::SessionSummary::codex_link`] — `"subscribed" | "bound" |
///         "offline" | "none"`, resolved from the same addressee
///         `codex_thread_id` is resolved from. **Gone in X12:** `"bound"` and
///         `"offline"`, which nothing sends.
///
///         The four words minor 19 shipped are exactly those. `"bound_not_started"`
///         is minor 20's, and the entry below says why it could not stay here.
///
///         This is the limit `ws::Capabilities::codex_interrupt`'s doc named: the
///         capability is build-shaped and connection-global, and `codex_thread_id`
///         reads the same whether the link is subscribed, bound or reconnecting, so
///         per-session actuatability was not computable from the fleet. It is now:
///         `agent == codex` and `codex_link` is `subscribed`. Absent decodes `none`,
///         which is exactly what an older daemon's fleet is to a client that cannot
///         address any of it. A refusal is still the last word — a link can move
///         between the summary and the tap.
///
///     Nothing existing changed shape, and no message was added. `CodexResolution`'s
///     arms, `ApprovalCard`, and every other summary field are byte-identical to
///     minor 18.
///   * `20` — **`codex_link` gains a fifth word: `"bound_not_started"`.**
///
///     [`event::SessionSummary::codex_link`] is now `"subscribed" | "bound" |
///     "bound_not_started" | "offline" | "none"`, and the rule a client scopes its two
///     Codex controls by is, plainly:
///
///     > **`subscribed` actuates both verbs; `bound_not_started` actuates compose only;
///     > everything else neither.**
///
///     The new word is a link bound to a thread the daemon has PROVED has never run a
///     turn — its own `thread/resume` was refused `-32600 "no rollout found for thread
///     id …"` — which is the one un-subscribed state a compose may be admitted in: no
///     rollout means no turn has ever run, so a first `turn/start` cannot collide with
///     one, and there is correspondingly no turn to stop. It exists to break a dead
///     end: a fresh Codex thread has no rollout until its first turn, so a phone gated
///     on `subscribed` could never send the message that would create one.
///
///     # Why it is a minor of its own, and why the argument that said otherwise expired
///
///     It was written into minor 19 on the ground that minor 19 had never shipped, so
///     no client anywhere could know four of the words and not the fifth. **That
///     premise died on 2026-09-08:** commit `66a03a3` "Version 1.1.0 (72): the
///     TestFlight train for Codex support" — a descendant of `d9b719f`, the minor-19
///     commit — put build 72 on TestFlight. Build 72 is a live consumer of minor 19, it
///     knows exactly the four words above, and adding a fifth to a number it has
///     already negotiated on is the one thing a minor exists to prevent. So the word
///     takes 20, which is what an unreleased minor would have cost nothing and a
///     released one costs by definition.
///
///     A minor-19 client meeting `"bound_not_started"` is not broken by it: the rule
///     for a word a client does not know is "not actuatable", which greys both controls
///     — the same answer it gives for `bound`, and strictly safe, because the only
///     thing the new word ever widens is a compose. What that client loses is the
///     affordance, not correctness. A minor-20 client gets the composer on a fresh
///     thread; that is the whole of the difference.
///
///     Nothing changed shape and no message was added. Every other field of
///     [`event::SessionSummary`] is byte-identical to minor 19, and a decoder that
///     treats the field as an opaque string — which is what the phone's unrecognised
///     arm does — reads both minors with one code path.
///   * `21` — **Claude's `AskUserQuestion` is answered from the phone.**
///
///     Claude Code asks every such question through a `PermissionRequest`.
///     Additive, and a client written against minor 20 needs all of it:
///       - [`ws::Capabilities::question_card`] — this daemon understands the rest.
///       - [`ws::ApprovalCard::question_hold`] — on a question card only: whether
///         the phone can answer it now ([`ws::QuestionHold`]). It changes while the
///         card is open; each change is a `question_hold` event
///         (`{request_id, question_hold}`), and the latest one wins.
///       - [`ws::AnswerDecision::Answers`] and [`ws::AnswerDecision::Decline`] —
///         the only decisions a question card takes. The daemon returns them
///         through Claude's held hook, or, for a run started in an agent host,
///         through that host; nothing is typed.
///
///     What changed for every client, including older ones: `allow` and `option`
///     on a question card are refused before anything is claimed or typed, and
///     `deny` on one is a decline. A minor-20 phone still shows the question as an
///     approval; nothing it can tap chooses an answer for the person.
///     **Gone in X12:** the holds `at_mac` and `ended`, and the `question_hold` event; a
///     card's hold is `held`.
///   * `22` — **Every Claude card is answered only through the hook Claude holds
///     for its own call (or, for a run started in an agent host, through that
///     host). Nothing is typed into a permission prompt.**
///
///     A phone's answer used to be typed into whichever prompt was on screen,
///     and Claude queues background agents' prompts there, so it could answer
///     another agent's call. Additive, and a client written against minor 21
///     needs all of it:
///       - [`ws::Capabilities::hook_only_approvals`] — this daemon answers
///         Claude cards only that way. A client shows a daemon's Claude
///         approvals as answerable only when it is advertised.
///       - [`ws::ApprovalCard::question_hold`] — on every Claude card now, not
///         only a question: whether the phone can answer it now, with its
///         `question_hold` events as in minor 21.
///       - [`ws::AnswerDecision::Text`] on a Claude approval — a denial with
///         that reason, sent to the agent that asked.
///
///     What changed for every client, including older ones: nothing is typed.
///     `allow`, `deny` and `option` 1 on a held approval go back through its
///     hook; any other `option` is refused before anything is claimed, and so
///     is any decision on a card that is not held. `identity_bound` is true
///     exactly on a held approval.
///   * `23` — **Earlier pages, and what a session's latest facts are without
///     its whole log.**
///
///     A phone that holds only a run's newest events needs two things from the
///     Mac. Additive, and a client written against minor 22 needs all of it:
///       - [`ws::Capabilities::history_pages`] — this daemon understands the rest.
///       - [`ws::ClientMessage::History`] and [`ws::ServerMessage::HistoryEnd`] —
///         the newest events below a seq, at most [`ws::MAX_HISTORY_PAGE`], as
///         ordinary `event` frames, then one end naming the request.
///       - [`event::SessionSummary::pending`] — every open card as a reference
///         (request id, the seq of its request event, its hold, a Codex card's
///         turn), never its body.
///       - [`event::SessionSummary::model`] and
///         [`event::SessionSummary::permission_mode`] — each with the seq of the
///         event that decided it, so a client keeps whichever of these and its
///         own events is newer. **Gone in X12:** the model's `source` and `at`; it
///         is `{value, seq}`, as the mode is.
///
///     What changed for every client, including older ones: nothing. The new
///     summary fields are omitted when empty and a client that never sends
///     `history` never receives `history_end`.
///   * `24` — **Where a Codex item read back from history belongs.**
///
///     A Codex item the daemon recovers from a thread's history after later facts
///     of its turn were filed carries `belongs_before` in its payload: the
///     `source_event_id` of the fact it belongs immediately before. A client draws
///     it there. Additive, and a client written against minor 23 needs none of it:
///     without it such an item is drawn where it was filed, as before.
///     **Gone in X12:** `belongs_before`; nothing sends it.
///   * `25` — **Which computer this is.**
///
///     A phone names the computer it is paired with the way its owner does.
///     Additive, and a client written against minor 24 needs none of it:
///       - [`ws::ServerMessage::HelloAck`]'s `machine_name` — the computer's name
///         (a Mac's computer name, a Linux machine's hostname), absent when it
///         could not be read.
///       - its `machine_os` — `"macos"` or `"linux"`, absent when not said.
///
///     A client that gets neither names the computer by the host it paired with.
///   * `26` — **Which daemon this is, whatever address reached it.**
///
///     A phone can pair one daemon under two addresses (an IP, then its MagicDNS
///     name) and must still hold it as one machine. Additive, and a client written
///     against minor 25 needs none of it:
///       - [`ws::ServerMessage::HelloAck`]'s `machine_id` — minted once into the
///         daemon's home ([`machine_id_path`]) and the same on every handshake.
///
///     A client that gets none tells machines apart by address, as before.
///   * `27` — **Agents message each other.**
///
///     A hosted agent can send another a message, which the receiving computer's
///     daemon keeps until that agent reads it. Between this computer's own
///     processes, and nothing a phone sends or reads changes:
///       - [`ipc::ClientFrame::SendAgentMessage`], [`ipc::ClientFrame::ListAgents`],
///         [`ipc::ClientFrame::ReadAgentMessages`] and [`ipc::ClientFrame::MailRead`]
///         — the `codeconnect` tool's requests.
///       - [`peer`] — one daemon asking another: a frame that is not a phone
///         message, before any phone hello, may be a [`peer::PeerRequest`], answered by one
///         [`peer::PeerReply`] and the connection closed. A daemon below 27
///         answers `bad_request`, which the sender reads as "older CodeConnect".
///   * `28` — **A session started from the phone.**
///
///     A phone picks a folder sessions have run in and starts Claude Code or
///     Codex there, as if `codeconnect <agent>` had been typed in it. Additive,
///     and a client written against minor 27 needs none of it:
///       - [`ws::Capabilities::start_session`] — this daemon understands the rest.
///       - [`ws::ClientMessage::ListProjects`] and [`ws::ServerMessage::Projects`]
///         — the folders, most recent first, and which agents this computer can
///         start.
///       - [`ws::ClientMessage::StartSession`] and
///         [`ws::ServerMessage::StartSessionResult`] — one start, idempotent by
///         `request_id`, answered with the new session's uid or why it failed.
///       - `launch_result` — between this computer's own processes: how
///         `codeconnect <agent> --no-attach` tells the process that ran it what it
///         started.
///
///     A daemon below 28 answers both requests `bad_request`, so a client sends
///     them only when the capability is advertised.
///   * `29` — **A start picks a branch or worktree.**
///
///     The main folder is never switched: another branch gets a worktree of its
///     own, in Claude Code's layout (`.claude/worktrees/<name>`). Additive, and a
///     client written against minor 28 needs none of it:
///       - [`ws::Capabilities::start_worktree`] — this daemon understands the rest.
///       - [`ws::ClientMessage::GetProjectGit`] and [`ws::ServerMessage::ProjectGit`]
///         — one folder's worktrees and its local branches, read only.
///       - [`ws::StartWorktree`], as [`ws::ClientMessage::StartSession`]'s
///         `worktree` — an existing worktree, a branch, or a new worktree.
///       - `cwd` on [`ws::StartSessionResult::Started`] — the folder the session
///         started in, when the daemon chose it.
///
///     A daemon below 29 answers `get_project_git` with `bad_request` and ignores
///     `worktree`, starting in the main folder, so a client sends either only
///     when the capability is advertised.
///   * `30` — **The phone browses folders and starts in any of them.**
///
///     As a terminal can `cd` anywhere and start an agent there. Additive:
///       - [`ws::Capabilities::browse_folders`] — this daemon understands the rest.
///       - [`ws::ClientMessage::ListFolder`] and [`ws::ServerMessage::FolderListing`]
///         — one folder's folders, home when no path is given, by real path.
///       - [`ws::ClientMessage::CreateFolder`] and
///         [`ws::ServerMessage::CreateFolderResult`] — one `mkdir`.
///       - [`ws::ClientMessage::StartSession`] and [`ws::ClientMessage::GetProjectGit`]
///         take any absolute folder, not only one [`ws::ServerMessage::Projects`]
///         listed; a relative path is refused, and `not_a_project` is never sent.
///
///     A daemon below 30 answers `list_folder` and `create_folder` with
///     `bad_request` and refuses an unlisted folder, so a client sends them, and
///     offers folders outside the list, only when the capability is advertised.
///   * `31` — **Each session's branch and line counts, and the computer's load.**
///
///     For the Mac app. Additive:
///       - [`event::SessionSummary::git_branch`] and
///         [`event::SessionSummary::diff_stat`] — read from git when a turn ends,
///         when the run registers and when the daemon starts; never on a listing.
///         Both are optional and an older daemon sends neither, which a client
///         reads as unknown, so no capability gates them.
///       - [`ws::Capabilities::machine_stats`], [`ws::ClientMessage::GetMachineStats`]
///         and [`ws::ServerMessage::MachineStats`] — one sample of CPU, memory and
///         disk. A daemon below 31 answers `bad_request`, so a client asks only
///         when the capability is advertised.
///   * `32` — **The owner's own devices sign in through Tailscale, and learn the
///     owner's other computers.**
///
///     Additive:
///       - A [`ws::ClientMessage::Hello`] with neither `token` nor `pairing_code`,
///         from a device of this computer's Tailscale user that is not tagged, is
///         paired as a code would pair it: the ack carries a fresh `device_token`.
///         A daemon below 32, or any other source, answers `unauthorized`, and
///         the client offers a code.
///       - [`ws::ServerMessage::Peers`] — sent to a paired device after its
///         `hello_ack`, and again when it changes: this computer's Tailscale state
///         and the owner's other computers it can reach. A `hello` names no client
///         minor, so it goes to every paired device: an app from before 32 reads it
///         as a frame it cannot read and asks to be updated.
///   * `33` — **A start picks the agent's model.**
///
///     Additive, and gated by the fields themselves rather than a capability:
///       - [`ws::AgentAvailability::models`] and [`ws::AgentAvailability::signed_in`]
///         — what each agent offers a start, and whether it is signed in, as the
///         agent itself answers. A daemon below 33 sends neither, which a client
///         reads as the agent's default only, and unknown.
///       - `model` on [`ws::ClientMessage::StartSession`] — one of those models,
///         given to the agent as `--model=<value>`, except Claude Code's own
///         `default`, which is no flag. A client sends it only for a model the
///         daemon listed, so a daemon below 33 never receives it.
///   * `34` — **An answer says which kind of device sent it.**
///
///     Additive:
///       - [`ws::AnswerOutcome::answered_on`] — `iphone` or `mac` on an answer from
///         a paired device, by the `client_name` its connection said hello with
///         ([`ws::AnsweredOn`]). A daemon below 34 sends none, and a client reads
///         that as before. `resolved_by` stays `phone` for both, and an answer sent
///         to Claude is detailed "Sent to Claude from a phone" whichever device
///         sent it, because that sentence is how every app knows it was sent; so an
///         app that does not know the field reads every answer as it always has.
///   * `35` — **A Claude run CodeConnect started can be ended from an app.**
///
///     Additive:
///       - [`ws::Capabilities::stop_run`], [`ws::ClientMessage::StopRun`] and
///         [`ws::ServerMessage::StopRunResult`] (`stopping`, `already_ended`,
///         `refused`). A daemon below 35 answers `stop_run` with `bad_request`, so a
///         client sends it only when the capability is advertised.
///       - [`event::Lifecycle::Stopping`] — the run was asked to end and has not yet.
///         A client from before 35 reads the word as `unknown`.
///   * `36` — **"Allow for this session" on a hosted Claude run's card.**
///
///     Additive:
///       - [`ws::ApprovalCard::allow_for_session`] — the card offers it: a hosted
///         run's approval whose request carries Claude's own allow rule. Left out
///         when false, so a client from before 36 reads every card as it did.
///       - The app answers it with [`ws::AnswerDecision::OptionId`]
///         `"acceptForSession"`, the id a Codex card's session option already has.
///         The daemon refuses it on a card that does not offer it, nothing sent, and
///         on one that does it returns Claude's allow rules for this session only,
///         never written to Claude's settings.
///   * `37` — **Stop on a Claude run CodeConnect started.**
///
///     Additive:
///       - [`ws::Capabilities::claude_interrupt`]: this daemon stops the running turn
///         of such a run. [`ws::ClientMessage::Interrupt`] and
///         [`ws::InterruptResult`] are Codex's, unchanged; the turn is the
///         `turn_id` of the run's events. A daemon below 37 refuses every Claude Stop.
///       - A `notification` event `send_not_sent` (`{request_id, reason: "stopped"}`):
///         a Stop cancelled the person's message before claude ran it. The person's
///         own row of a message sent to such a run carries its `request_id`.
///   * `38` — **The command menu and a command's output on a Claude run CodeConnect
///     started.**
///
///     Additive:
///       - [`ws::ClientMessage::GetCommandCatalog`] for such a run answers `available`
///         once an init has named the commands claude keeps to its own screen: the
///         commands its claude last reported, but for those screen-only ones and
///         those whose app screen the daemon refuses on such a run (`clear`,
///         `model`, `effort`), with
///         [`ws::CommandCatalogResult::Available`]'s `entries` (each command's
///         description and argument hint). Left out when empty, so a probed catalog
///         reads as before. A daemon below 38 answers `unavailable` for such a run.
///       - A command's output (`/usage`, `/effort low`, `/compact`'s "Compacted") is
///         a `user_message` whose `message.content` is the
///         `<local-command-stdout>…</local-command-stdout>` string a terminal run
///         files, where a daemon below 38 filed nothing, or `/effort`'s output as an
///         `agent_message`.
///       - `/usage` and `/cost` sent to such a run are messages; `/status` is
///         refused, as its claude does not offer it.
///   * `39` — **A Claude run's model, effort and permission mode, set from an app.**
///
///     Additive:
///       - [`ws::ClientMessage::SetModel`] (a model, an effort, or both) and
///         [`ws::ClientMessage::SetMode`], answered with [`ws::ServerMessage::SettingResult`]
///         (`applied {applies: now | next_turn}`, `refused`, `indeterminate`), for a Claude
///         run CodeConnect started. A daemon below 39 answers both `bad_request`.
///       - [`event::SessionSummary`]'s `menus` ([`event::RunMenus`]: the run's models with
///         their efforts, its permission modes, and `selected_model`, the model the run's
///         model fact names, for display) on such a run while it is live, once its
///         start is answered; and `effort`, the effort the run last took (an app's change, or
///         claude's own receipt for a typed `/effort`). No capability: a client offers the
///         controls where a summary carries `menus`, which a daemon below 39 never sends.
///       - A `notification` event `model_set {effort}` when claude took an app's effort
///         change. A model or mode change shows as claude itself reports it: its
///         `Set model to …` output and its permission-mode status.
///   * `40` — **The mode that asks nothing, behind a confirmation.**
///
///     Additive:
///       - [`event::MenuMode`]'s `confirm` (left out when false), set on a Claude run's
///         `bypassPermissions` ("Bypass permissions"), which its menus now list.
///       - [`ws::ClientMessage::SetMode`]'s `confirmed` (left out when false). A mode marked
///         `confirm` is refused without it, nothing sent; every other mode is as before. A
///         daemon below 40 lists no such mode and refuses `bypassPermissions` as not offered.
///       - On a Claude run CodeConnect started, a `turn_complete`'s `local_command`, the
///         command whose own turn it ends (`/clear`, `/compact`, `/effort low`), claude's
///         own field, and the person's `user_message`'s `command`, the one of claude's
///         commands its first word names (`/effort low` is `effort`). Each is left out
///         otherwise, an agent turn's end and a plain message as before.
///   * `41` — **A Codex card's answer says which kind of device sent it.**
///
///     Additive:
///       - `answered_on` on [`ws::CodexResolution::Answered`] — `iphone` or `mac` on a
///         Codex card answered from a paired device (`by: phone`), by the `client_name`
///         its connection said hello with ([`ws::AnsweredOn`]), as
///         [`ws::AnswerOutcome::answered_on`] has said for Claude's since 34. A daemon
///         below 41 sends none, nor does any daemon for an answer a restart settled, and a
///         client reads that as before.
///   * `42` — **The next message relaunches a run whose agent ended on its own.**
///
///     Additive:
///       - [`event::SessionSummary`]'s `relaunchable` (left out when false): a run CodeConnect
///         started, of either agent, that has `exited` with its end reading "Stopped: its process
///         ended", so a message to it relaunches it and continues the conversation. Never a run
///         End run ended or a start that failed. A daemon below 42 sends no such field and keeps
///         every ended Claude run ended.
///
///     What changed for every client, including older ones: a model or a mode set on a
///     Claude run while a turn runs is answered `applied {applies: now}`, as claude takes it
///     from its next request, the running turn's next step included; an effort set while a
///     turn runs is still answered `next_turn`. `/model` or `/effort` sent alone, or as its
///     screen's confirmation, is refused with "/model’s screen isn’t available in this
///     session; nothing was changed." (or `/effort`'s).
///   * `43` — **A Codex run's settings from the apps.**
///
///     No message changes shape. A live Codex run CodeConnect started now carries
///     [`event::SessionSummary`]'s `menus` (its models from Codex's own list, and the modes
///     `auto` "Default", `read-only` "Read Only", `plan` "Plan" and `full-access` "Full
///     Access", marked `confirm`), and its `model`, `effort` and `permission_mode` facts,
///     read off the settings Codex names in its answers to the run's start and resumes and
///     in its report of each change. [`ws::ClientMessage::SetModel`] and
///     [`ws::ClientMessage::SetMode`] serve it as they serve a Claude run, answered with
///     [`ws::ServerMessage::SettingResult`]. A daemon below 43 sends a Codex run no `menus`.
///   * `44` — **A device names itself in its hello.**
///
///     [`ws::ClientMessage::Hello`] may carry `device_name`, what the device calls itself (a Mac's computer
///     name, an iPhone's "iPhone"). The daemon lists a new device under it, made unique, and renames a paired
///     device to it on its next hello when it differs. A client below 44 sends none and is listed under its
///     `client_name` as before; a daemon below 44 ignores the field.
///   * `45` — **A run's menu names the model each entry runs.**
///
///     No message changes shape. Each [`event::MenuModel`] of a Claude run's `menus` may carry
///     `resolved_model`, the model id Claude Code's own list gives the entry (`resolvedModel`), so an
///     app names the model a run is on by that id, never by the text of claude's receipt of a change.
///     A Codex entry carries none: its `value` is the model. A daemon below 45 sends none.
///   * `46` — **An ended run names its model as its menu did.**
///
///     No message changes shape. A [`event::SessionSummary`] of a Claude run CodeConnect started that is
///     sent no `menus` (it is not live: stopping or ended) may carry `model_label`, the label of the
///     start's entry for its model, by the rule the apps name a live run's model by, so an ended run
///     reads "Opus", not `claude-opus-5-5`. A daemon below 46 sends none, and an app shows the model as
///     reported.
///   * `47` — **A run's menu names each model's default effort.**
///
///     No message changes shape. Each [`event::MenuModel`] of a run's `menus` may carry
///     `default_effort`, the effort that model runs at when none is chosen, so an app names and marks it
///     while the run reports no effort: a Codex entry Codex's own (`model/list`'s `defaultReasoningEffort`);
///     a Claude entry the default Claude Code's documentation names for the model it resolves to, and none
///     for a model it does not name. A daemon below 47 sends none.
///   * `48` — **Several accounts per agent.**
///
///     [`ws::ClientMessage::ListAccounts`], [`ws::ClientMessage::AddAccount`], [`ws::ClientMessage::ActivateAccount`],
///     [`ws::ClientMessage::SignInAccount`] and [`ws::ClientMessage::RemoveAccount`], each answered with one
///     [`ws::ServerMessage::Accounts`] naming its request, or [`ws::ServerMessage::AccountsRefused`]; the same
///     listing pushed, with no request id, whenever it changes. To paired devices only, behind
///     [`ws::Capabilities::accounts`]. An account is a folder of the computer's own that a start on it is given, its
///     agent's credential its own and the rest of the person's home shared; an agent's Default is the agent as it is.
///     Choosing one moves the agent's running sessions to it: each is ended when idle, its end typed
///     [`event::SWITCHED_TO`], and its next message starts it on the account active then; it reads live meanwhile,
///     [`event::SessionSummary::switching`] saying where its move stands, and a start that did not go through files
///     [`event::notice::SWITCH_FAILED`]. Each run CodeConnect started names the account its process runs on
///     ([`event::SessionSummary::account`]; Default is `id` null). A listing carries what each Codex account and the
///     Default say of themselves ([`ws::AccountFacts`]: email, plan, usage windows, read on demand and held in the
///     computer's memory only), and marks two signed in as one email. An account whose Codex sign-in is kept outside its home reads
///     [`ws::AgentAccount::not_separate`]. A daemon below 48 answers them `bad_request` and advertises no
///     `accounts`.
pub const PROTOCOL_MINOR: u32 = 48;

#[cfg(test)]
mod ledger_tests {
    use super::PROTOCOL_MINOR;

    /// The one shape a ledger entry is written in.
    ///
    /// Scanning the source is only honest if this prefix cannot match anything but a
    /// top-level entry, and it cannot: every line nested inside an entry is indented
    /// past this column, so a sub-bullet that happens to quote a number is not
    /// mistaken for an entry about that number.
    const ENTRY: &str = "///   * `";

    /// The minors the ledger names, in the order it names them.
    fn minors_the_ledger_names() -> Vec<u32> {
        let source = include_str!("lib.rs");
        // The scan is bounded by the two constants the ledger sits between, so it
        // cannot wander into some other doc comment in this file that bullets a
        // number. Both anchors appear exactly once in the file.
        let start = source
            .find("pub const PROTOCOL_VERSION")
            .expect("the version constant opens the block the ledger sits in");
        let end = source
            .find("pub const PROTOCOL_MINOR")
            .expect("the ledger documents the minor constant, so it ends at it");
        source[start..end]
            .lines()
            .filter_map(|line| line.strip_prefix(ENTRY))
            .filter_map(|rest| rest.split_once('`'))
            .filter_map(|(number, _)| number.parse().ok())
            .collect()
    }

    /// **Every minor from 0 to [`PROTOCOL_MINOR`] has an entry, and the ledger claims
    /// no minor that does not exist.**
    ///
    /// Minor 16 shipped with no entry at all. Nothing caught it — the ledger is prose,
    /// and prose has no compiler — so the gap was found by a human reading the file
    /// during a release review, which is not a gate and does not run again. This is
    /// the gate. It fails on a bump that forgets to say what it bought, and equally on
    /// an entry written for a number the constant has not reached.
    ///
    /// It deliberately does **not** check order. The ledger runs `10, 13, 12, 11` for a
    /// real reason — the terminal work landed before the two smaller additions it was
    /// developed alongside — and the order entries are told in is the ledger's to
    /// choose. What it may not do is skip.
    #[test]
    fn the_ledger_names_every_minor_from_zero_to_the_current_one() {
        let named = minors_the_ledger_names();

        let missing: Vec<u32> = (0..=PROTOCOL_MINOR)
            .filter(|minor| !named.contains(minor))
            .collect();
        assert!(
            missing.is_empty(),
            "minor(s) {missing:?} have no entry in the ledger above `PROTOCOL_MINOR`. \
             A minor nobody wrote down is one a client cannot negotiate on: say what \
             it added, or do not bump the number."
        );

        let unshipped: Vec<u32> = named
            .iter()
            .copied()
            .filter(|minor| *minor > PROTOCOL_MINOR)
            .collect();
        assert!(
            unshipped.is_empty(),
            "the ledger has entries for minor(s) {unshipped:?}, past `PROTOCOL_MINOR` = \
             {PROTOCOL_MINOR} — either the constant was not bumped with the prose, or \
             the prose describes work that has not shipped."
        );

        let mut sorted = named.clone();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(
            before,
            sorted.len(),
            "the ledger names a minor twice ({named:?}); two entries for one number \
             means one of them describes a bump that never happened."
        );
    }
}

/// Environment variable carrying the session's unique id into the agent process,
/// so the mail server it runs can say which run it speaks for without an explicit
/// `--session`.
pub const ENV_SESSION_UID: &str = "CODECONNECT_SESSION_UID";

/// The LaunchAgent label. One constant so `codeconnect daemon install`, `codeconnect daemon
/// status` and the daemon's own "am I launchd-managed?" check can never disagree
/// about which job they are talking about.
pub const LAUNCHD_LABEL: &str = "com.codeconnect.ccd";

/// The label this build installs, manages and recognises itself by: [`LAUNCHD_LABEL`], always,
/// in a shipped build.
///
/// A rig build — `--features test-launchd-label`, never the default — takes it from
/// [`TEST_LABEL_VAR`] instead, which must read `com.codeconnect.ccd.test-<id>`. A test install
/// then addresses its own job in the user's launchd domain and never the real one; with the
/// variable unset or malformed it is a label no job has, and [`test_label_refusal`] stops the
/// install, restart and removal outright.
#[cfg(not(feature = "test-launchd-label"))]
pub fn launchd_label() -> &'static str {
    LAUNCHD_LABEL
}

/// Why this build may not install, restart or remove a job: never, in a shipped build.
#[cfg(not(feature = "test-launchd-label"))]
pub fn test_label_refusal() -> Option<String> {
    None
}

/// The variable a rig build reads its job's label from (see [`launchd_label`]).
#[cfg(feature = "test-launchd-label")]
pub const TEST_LABEL_VAR: &str = "CODECONNECT_LAUNCHD_LABEL";

/// The label of an unconfigured rig build: no job has it, and [`test_label_refusal`] names it.
#[cfg(feature = "test-launchd-label")]
const UNSET_TEST_LABEL: &str = "com.codeconnect.ccd.test-unset";

#[cfg(feature = "test-launchd-label")]
pub fn launchd_label() -> &'static str {
    static LABEL: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    LABEL
        .get_or_init(|| {
            std::env::var(TEST_LABEL_VAR)
                .ok()
                .filter(|label| is_test_label(label))
        })
        .as_deref()
        .unwrap_or(UNSET_TEST_LABEL)
}

/// A rig build refuses unless its job is a test job that cannot reach the owner's: a test label,
/// and a `TMUX_TMPDIR` that exists as a directory of this user's with mode 0700. A tmux whose
/// `TMUX_TMPDIR` is missing falls back to the default socket — the owner's server.
#[cfg(feature = "test-launchd-label")]
pub fn test_label_refusal() -> Option<String> {
    if launchd_label() == UNSET_TEST_LABEL {
        return Some(format!(
            "this is a test build: set {TEST_LABEL_VAR}=com.codeconnect.ccd.test-<id> (letters, digits and -)"
        ));
    }
    tmux_tmpdir_refusal(
        std::env::var_os("TMUX_TMPDIR")
            .map(std::path::PathBuf::from)
            .as_deref(),
    )
}

/// A rig's `TMUX_TMPDIR` must exist as this user's own directory, mode 0700, not a link to one:
/// checked as the folder is, so a missing or loose one stops the job before any tmux runs.
#[cfg(feature = "test-launchd-label")]
fn tmux_tmpdir_refusal(dir: Option<&std::path::Path>) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let Some(dir) = dir else {
        return Some("this is a test build: set TMUX_TMPDIR to a scratch folder".to_string());
    };
    match std::fs::symlink_metadata(dir) {
        // SAFETY: getuid has no preconditions and cannot fail.
        Ok(meta)
            if meta.is_dir()
                && meta.uid() == unsafe { libc::getuid() }
                && meta.mode() & 0o777 == fsperm::DIR_MODE =>
        {
            None
        }
        _ => Some(format!(
            "this is a test build: TMUX_TMPDIR {} must exist as this user's directory, mode 0700",
            dir.display()
        )),
    }
}

#[cfg(feature = "test-launchd-label")]
fn is_test_label(label: &str) -> bool {
    label
        .strip_prefix("com.codeconnect.ccd.test-")
        .is_some_and(|id| {
            !id.is_empty()
                && id != "unset"
                && id.len() <= 64
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Root of all CodeConnect state. `CODECONNECT_HOME` exists so tests never
/// touch the real `~/.codeconnect`.
///
/// **Absolute whenever the cwd can be read**, and that is the whole point
/// of the wrapper. Both sources here can be relative — `CODECONNECT_HOME` is
/// whatever the operator exported, and the `$HOME`-less fallback is literally
/// `./.codeconnect` — and a relative root is not a root at all: it names a different
/// directory in every process that resolves it. The daemon and the processes it starts
/// run with **different working directories**, so a process-relative root has them
/// addressing different records for the same session. Absolutising HERE, where the
/// root is first read, is what makes every consumer name one directory.
///
/// `std::path::absolute` is prefix-only (it prepends the cwd and drops `.`
/// components; it resolves no symlinks and no `..`), so it is idempotent: an
/// already-absolute root is returned unchanged, and forwarding this value to a
/// child that calls `root_dir()` again yields the same path (measured).
///
/// **When it cannot be made absolute, this FAILS CLOSED.** The
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

/// Which daemon this is, minted once and sent in every `hello_ack`. Lives with
/// the rest of the daemon's state, so it is this home's identity: an update or a
/// reinstall keeps it, and a fresh home is a new machine.
pub fn machine_id_path() -> PathBuf {
    root_dir().join("machine_id")
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

        // A RELATIVE override is absolutised, because the launcher, the
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

    /// The arm reached when a **relative** root cannot be
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

    /// A shipped build's label is the constant, whatever the environment says: the rig
    /// variable is not read, and the refusal never fires.
    #[cfg(not(feature = "test-launchd-label"))]
    #[test]
    fn a_shipped_build_manages_the_real_job_only() {
        std::env::set_var(
            "CODECONNECT_LAUNCHD_LABEL",
            "com.codeconnect.ccd.test-ignored",
        );
        assert_eq!(launchd_label(), LAUNCHD_LABEL);
        assert_eq!(test_label_refusal(), None);
    }

    /// A rig build accepts only `com.codeconnect.ccd.test-<id>`: never the real label, never one
    /// that merely starts like it.
    #[cfg(feature = "test-launchd-label")]
    #[test]
    fn a_rig_label_is_a_test_label_or_nothing() {
        for good in [
            "com.codeconnect.ccd.test-a",
            "com.codeconnect.ccd.test-run-42-B",
        ] {
            assert!(is_test_label(good), "{good}");
        }
        for bad in [
            LAUNCHD_LABEL,
            "com.codeconnect.ccd.test-",
            "com.codeconnect.ccd.test-unset",
            "com.codeconnect.ccd.test-a/b",
            "com.codeconnect.ccd.test-a b",
            "com.codeconnect.ccdx.test-a",
            "",
        ] {
            assert!(!is_test_label(bad), "{bad:?}");
        }
        assert!(!is_test_label(&format!(
            "com.codeconnect.ccd.test-{}",
            "a".repeat(65)
        )));
    }

    /// A rig's TMUX_TMPDIR must already be this user's own 0700 directory: unset, missing, loose
    /// or a link to a good one are each refused.
    #[cfg(feature = "test-launchd-label")]
    #[test]
    fn a_rig_tmux_folder_must_exist_private() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("cc-tmux-check-{}", std::process::id()));
        let good = base.join("good");
        let loose = base.join("loose");
        let link = base.join("link");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir_all(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&good, &link).unwrap();

        assert_eq!(tmux_tmpdir_refusal(Some(&good)), None);
        assert!(tmux_tmpdir_refusal(None).is_some());
        assert!(tmux_tmpdir_refusal(Some(&base.join("never-created"))).is_some());
        assert!(tmux_tmpdir_refusal(Some(&loose)).is_some());
        assert!(tmux_tmpdir_refusal(Some(&link)).is_some());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
