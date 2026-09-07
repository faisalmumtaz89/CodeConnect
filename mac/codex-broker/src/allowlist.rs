//! The refuse-by-default allowlist (A4 clause 1).
//!
//! The client→server surface is **refuse-by-default**: only an explicit, per-leg,
//! vetted set of messages forwards; everything else is refused per the refusal
//! matrix. This is forced by A4's blocker — the wire-native code-exec methods
//! (`command/exec`, `thread/shellCommand`, `process/spawn`, `fs/writeFile`) run
//! code with caller-chosen `dangerFullAccess`, no approval and no observer, so a
//! *denylist* that forgets one is a full bypass over a 95-stable / 133-experimental
//! surface.
//!
//! ## Keying
//!
//! Each cell is `(endpoint role × JSON-RPC kind × method)` and carries exactly one
//! [`Disposition`]. Endpoint role is anchored to the **socket** ([`Role`]);
//! [`narrow_role`] enforces that a connection's caller-controlled `initialize`
//! identity may only *narrow* a role, never elevate it. The parameter-shape leg of
//! the key is applied by the [`Disposition::FingerprintAssert`] handler
//! ([`crate::fingerprint`]), not by name here.
//!
//! ## Exhaustiveness
//!
//! [`disposition`] is a total function — every method (pinned or unknown) maps to
//! exactly one disposition, and unknown/future methods fall through to
//! `Refuse(NotAllowlisted)`. `tests/exhaustiveness.rs` proves, against the pinned
//! `schema-0.147/methods-*.json` census, that every method resolves, the four
//! bypass methods resolve to `Refuse(CodeExecBypass)`, and every forwarded method
//! is a real pinned method (drift test).
//!
//! ## Deferred dispositions (clean seam for the fanout sub-chunk)
//!
//! ONE composable action from the plan's set — `consume-locally` — still requires
//! machinery that is deferred (the one-use response-capability fanout). Its table entry is
//! **final and correct** ([`Disposition::ConsumeLocally`]); only its *executor branch* is
//! stubbed, and it **fails closed** here (see [`crate::refusal`]).
//!
//! `head-check` is no longer one of them. It existed for exactly one cell, `turn/steer`,
//! held behind D2's vector barrier — and Phase 4b measured what that deferral cost: the
//! operator of a hosted session could not steer their own model from their own keyboard,
//! which is the same "refusing the only control is a safety decision, not a deferral"
//! that took `turn/interrupt` out of this paragraph in 4a. The cell is now
//! [`Disposition::SteerHeadThread`] on the TUI leg and [`Disposition::SteerRunningTurn`]
//! on ccd's, and a variant nothing maps to is dead weight, so `HeadCheck` is gone.
//!
//! The third — `hold/serialize` — is GONE as of 2e-4c. It existed for exactly one cell,
//! `thread/unsubscribe`, to hold the switch marker behind D2's linearization latch. D2 and
//! D3 are deferred to Phase 3 **together with their subject** (a ccd write that can
//! actuate; see [`crate::session`]), so there is no latch to serialize behind and a
//! variant nothing maps to is dead weight. That cell is now
//! [`Disposition::UnsubscribeSessionThread`].

/// Which unix socket a client connection arrived on. Role is anchored here, not to
/// the caller-controlled `initialize` identity.
///
/// O12 — the role is NOT part of any correlation key. The session thread binding correlates
/// an admitted creation to its response by `(ConnId, RequestId)` — the relay-minted
/// per-connection instance id, not the role (see [`crate::session`]). `Hash` is
/// kept because [`crate::response_capability`] keys a capability's grant by role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// `tui.sock` — the Codex TUI (and its `/resume` picker's second connection).
    Tui,
    /// `ccd.sock` — the CodeConnect daemon observation/actuation adapter.
    Ccd,
}

/// The JSON-RPC kind used to key the allowlist. Responses are classified by shape
/// (they carry no method) and never consult this table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonRpcKind {
    Request,
    Notification,
}

/// Why a message is refused. Carried for the audit log; every variant forwards zero
/// bytes upstream (a request with a usable id additionally gets a synthetic error —
/// see [`crate::refusal`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// One of the four wire-native code-exec bypass methods (or a sibling in those
    /// families caught by refuse-by-default). Never forwarded on any leg.
    CodeExecBypass,
    /// A method-or-`config` key that would durably move approval/sandbox/hook
    /// ownership (e.g. `thread/settings/update`). Never forwarded.
    OwnershipAdjacent,
    /// A method the role's least-privilege model forbids (e.g. ccd attempting
    /// `thread/start` / `thread/fork` — ccd attaches to the thread the operator's own
    /// session created and never creates one).
    RoleNotPermitted,
    /// The launch fingerprint could not be asserted — a conflicting ownership value,
    /// or an absent one on a policy-setting method (absence ≠ forward). Produced by
    /// the fingerprint validator at runtime, not by the static table.
    Fingerprint,
    /// Refuse-by-default: an unknown/future method, or a method not vetted for this
    /// leg. The catch-all that makes the surface an allowlist.
    NotAllowlisted,
    /// A shape with no client→server form (array / binary / malformed).
    Malformed,
    /// The disposition is real and correct, but its enforcement machinery is deferred
    /// to the switch/fanout sub-chunk; it fails closed until then.
    Deferred,
}

/// The composable disposition of a single allowlist cell.
///
/// The plan's action set was `{refuse, consume-locally, hold/serialize, head-check,
/// fingerprint-assert/inject, forward}` and this enum used to claim all six were
/// represented. Two are not, and both left for the same reason rather than by neglect:
/// `hold/serialize` had one cell and lost it to 2e-4c, and `head-check` had one cell and
/// became two real checks in 4b. What is here is what the broker does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Forward the reassembled bytes upstream unchanged (vetted benign).
    Forward,
    /// Ownership-carrying request: assert the launch fingerprint over the params
    /// (typed fields *and* the `config` map, key-scoped) before forwarding; a
    /// conflict, or an absent ownership field on a policy-setting method, is refused.
    ///
    /// The **executor** adds two state-level rules this static table deliberately does not
    /// encode (so the golden matrix does not move):
    /// * `thread/start` additionally claims the session's single creation slot at the
    ///   moment it is admitted, and is refused when that slot is closed (one thread bound
    ///   or one creation already pending — [`crate::session`]).
    /// * `thread/fork` is **refused outright pre-2e-4c**: a fork's lineage rule is that its
    ///   SOURCE thread must be session-bound, and no fork frame exists in the wire capture,
    ///   so the source-thread field is unprovable. See [`crate::refusal`].
    FingerprintAssert,
    /// **Composable**: fingerprint-assert THEN a head-check, in that order — a conflicting
    /// ownership value stays its own distinct policy refusal before the head is consulted.
    /// The executor implements the **pre-D2 subset**: a `turn/start` may only name the
    /// session's ONE **verified** thread — one whose creation this broker admitted AND
    /// whose creation response it correlated and verified ([`crate::session`]) — and must
    /// carry exactly the `cwd`/`runtimeWorkspaceRoots` bound at that creation — both of which
    /// were themselves anchored to the coordinator-owned launch cwd before being bound
    /// (`cwd` equals it; `runtimeWorkspaceRoots` equals `[launch cwd]` — A10 follow-on,
    /// 2e-7c). This is also
    /// the ONLY thing that discharges the measured `sandboxPolicy: null` deferral (see
    /// [`crate::fingerprint`]). D2 — the latch, acknowledged quiesce, acceptance fence and
    /// upstream seal — replaces the subset wholesale when it lands; it fills in the
    /// executor, not this table.
    FingerprintThenHeadCheck,
    /// **`turn/start` from the phone**: everything [`Disposition::FingerprintThenHeadCheck`]
    /// proves, plus the thread must be IDLE, plus the text-only `input` rule
    /// [`Disposition::SteerRunningTurn`] carries.
    ///
    /// # The idle rule is the whole reason this is a separate cell
    ///
    /// MEASURED on 0.153.4, against the app-server's own socket: a `turn/start` sent while
    /// a turn is running is not queued and is not refused — it is ACCEPTED, and answered
    /// with the RUNNING turn's id. It is an implicit steer. And unlike a real
    /// `turn/steer` it carries no `expectedTurnId`, so it has no staleness guard at all:
    /// a phone whose view of the session is a second out of date would inject its text
    /// into whatever turn happened to be running when the bytes landed.
    ///
    /// So the phone's start is admitted only against an idle thread, and the phone's steer
    /// is the guarded method that exists for the other case. The TUI leg keeps
    /// [`Disposition::FingerprintThenHeadCheck`] and the server's own behaviour: it is the
    /// operator's own keyboard, they are looking at the pane, and the measured TUI sends a
    /// `turn/steer` there rather than a second `turn/start`.
    FingerprintThenIdleTurn,
    /// Refuse now (zero upstream bytes; synthetic error only for a request with a
    /// usable id).
    Refuse(RefuseReason),
    /// Forward iff `params.threadId` names a thread of THIS session — the active head or
    /// one it retired. Carries no ownership fields and cannot actuate anything: it only
    /// drops the *calling connection's* subscription (2e-4c MEASURED: an observer that
    /// unsubscribed stopped receiving that thread's `turn/*`/`item/*` frames while every
    /// other connection kept receiving them — the effect is strictly connection-local, so
    /// one leg can never unsubscribe another's).
    ///
    /// This replaces the `hold/serialize` cell `thread/unsubscribe` used to hold. That cell
    /// existed to serialize the marker behind D2's switch latch; with D2 deferred to Phase 3
    /// alongside its subject (see [`crate::session`]), there is no latch to serialize behind
    /// and the honest disposition is the one the wire supports: a scoped, read-only-ish
    /// forward that fails closed on any thread this session does not own.
    UnsubscribeSessionThread,
    /// Forward iff `params.threadId` names a thread of THIS session — the thread-scoped
    /// READS (`thread/read`, `thread/turns/list`, `thread/items/list`).
    ///
    /// **These were plain [`Disposition::Forward`] until the 0.153 re-grounding, and that
    /// was a live cross-session read.** All three take a caller-chosen `threadId`, the
    /// forward path validates no params, and `codex_home()` is the operator's own
    /// `~/.codex` — so every CodeConnect session on a machine shares one thread store and
    /// any leg could read any other session's conversation out of it.
    ///
    /// It was not theoretical. MEASURED end to end: the 0.153 TUI declares a
    /// `dynamicTools` bundle giving the MODEL a `read_thread` tool; the model called it on
    /// a foreign thread id; the TUI implemented that as `thread/read` followed by
    /// `thread/turns/list`; both forwarded; and the other session's prompt text, its turn
    /// items and its rollout file path came back and were handed to the model.
    ///
    /// `thread/loaded/list` is deliberately NOT here, and "it carries no `threadId`" is
    /// not the reason — a global read with no selector is less bindable, not automatically
    /// scoped. What it returns was MEASURED with a two-session canary against one real
    /// 0.153 app-server: three connections, two of which started a thread and one of which
    /// started none, and **every one of them got both thread ids back**. It enumerates the
    /// app-server PROCESS's in-memory set, not the calling connection's — two app-server
    /// processes sharing one `CODEX_HOME` see nothing of each other's. The response was
    /// measured to carry thread IDS ONLY: no title, preview, cwd or rollout path. It is
    /// also not optional — the real 0.153 TUI sends it once at startup, immediately after
    /// the handshake, so refusing it would refuse the boot.
    ///
    /// # The principal this is scoped to is the HOST PROCESS, not a connection
    ///
    /// Stated precisely, because the weaker-sounding version is the true one and the
    /// stronger one would be a claim this broker does not enforce. `Broker::serve` accepts
    /// **many** connections on `tui.sock` — it must, because the TUI's own `/resume`
    /// picker legitimately opens a second — and [`crate::session::SessionThreads`] is
    /// constructed once per broker and shared by every leg. So there is no per-connection
    /// ownership here and none is claimed: any connection the broker accepts joins one
    /// thread domain, and a thread one leg creates is visible to another through this
    /// method and readable through the bound reads.
    ///
    /// That domain is bounded by the process, and the bound is what makes this safe:
    /// CodeConnect gives each session its own host, its own broker and its own app-server
    /// under an isolated `CODEX_HOME`, and every method that could load a thread into that
    /// app-server is either slot-claimed (`thread/start`), bound (`thread/resume` and the
    /// three reads) or refused (`thread/list`, `thread/fork`). So what it enumerates is
    /// this host's own session, whichever of its legs asks.
    ///
    /// What that leaves is a **same-uid process connecting to the run directory's socket**,
    /// which is the accepted boundary the launch path states once
    /// (`codeconnect::codex::start`, A22): a process already running as the user can ptrace
    /// this one, so a socket it can reach is not a boundary this broker can defend. It is
    /// named here rather than left implied, because "one session per process" reads like a
    /// connection-level guarantee and is not one.
    ReadSessionThread,
    /// **`turn/steer` on the operator's own keyboard**: forward iff `params.threadId` is
    /// this session's bound head thread and the params are the measured shape.
    ///
    /// This was `HeadCheck` — deferred, refuse-always — and that was not a safe terminal
    /// state, for the same reason `turn/interrupt` was not. MEASURED on 0.153.4: typing
    /// during a running turn sends `turn/steer`, the broker refused it with `-32601`, and
    /// a hosted operator could not redirect their own model from their own keyboard. The
    /// composer's only other affordance is to wait for the turn to end.
    ///
    /// **The staleness guard is the server's, and it is native.** A steer names the turn
    /// it was composed against in `expectedTurnId`, and the app-server refuses one that
    /// does not match — so this leg passes it through rather than second-guessing it. The
    /// operator is looking at the pane; the server's answer reaches their screen.
    ///
    /// **It cannot move the launch fingerprint.** MEASURED: a steer carries no `cwd`, no
    /// `sandboxPolicy`, no `approvalPolicy` and no `runtimeWorkspaceRoots` — its whole
    /// params surface is a thread, a turn, some input and two null metadata fields. So the
    /// head-check is the entire authorization question, and there is no fingerprint to
    /// assert.
    SteerHeadThread,
    /// **`turn/steer` from the phone**: everything [`Disposition::SteerHeadThread`] proves,
    /// plus two things the ccd leg needs and the TUI leg does not.
    ///
    /// 1. **`expectedTurnId` must be the turn this session is actually running** — the same
    ///    [`crate::session::ThreadBinding::is_active_turn`] predicate that binds
    ///    [`Disposition::InterruptActiveTurn`]. MEASURED on 0.153.4: the app-server refuses
    ///    a stale one with `-32600 "expected active turn id X but found Y"`, which hands
    ///    the caller the REAL running turn id. A phone is not looking at the pane and has
    ///    no business learning that id from a refusal, so a stale steer forwards zero bytes
    ///    and is refused here instead of upstream.
    /// 2. **Every `input` item is TEXT.** The `UserInput` union also carries `localImage`,
    ///    `localAudio`, `skill` and `mention` — each taking an absolute filesystem PATH the
    ///    app-server reads itself, outside the model's sandbox — and `image`/`audio`, which
    ///    take a URL. A phone that could name those would have a read-and-exfiltrate
    ///    primitive that no sandbox policy fences. The person at the keyboard legitimately
    ///    drags a file into their own composer and can already read it; the phone cannot,
    ///    so this leg admits the one arm it needs.
    SteerRunningTurn,
    /// Forward iff `params.threadId` is this session's bound thread AND `params.turnId` is
    /// a turn this broker admitted, the server answered, and no terminal has cleared —
    /// i.e. the turn that is actually RUNNING.
    ///
    /// **This was `HeadCheck` (deferred, refuse-always), and that was not a safe terminal
    /// state.** Measured: with it deferred, Ctrl-C in a hosted session sends
    /// `turn/interrupt{threadId,turnId}`, the broker refuses it, the TUI prints a banner
    /// naming the broker, and the turn runs to its own end. That is tolerable for a turn
    /// that ends on its own and is not tolerable otherwise — a hung command, an
    /// unanswerable server request, or work the user needs to stop leaves the session with
    /// no way out but killing it. Refusing the only stop control is a safety decision, not
    /// a deferral.
    ///
    /// It is admissible without D2 because an interrupt is not a vector actuation: it
    /// carries no ownership fields, starts nothing, and names a turn rather than steering
    /// one. The two things it must not be able to do — reach another session's thread, or
    /// name a turn this session is not running — are exactly what
    /// [`crate::session::SessionThreads::is_active_turn`] already decides, and that
    /// predicate is the same one the turn ledger keeps for its own fencing.
    ///
    /// `turn/steer` is its own pair of cells — [`Disposition::SteerHeadThread`] and
    /// [`Disposition::SteerRunningTurn`] — because it INJECTS content into a running turn,
    /// which needs more of the phone than of the keyboard.
    InterruptActiveTurn,
    /// DEFERRED: consume as a one-use response capability and fan out the winner
    /// (method-less approval answers).
    ConsumeLocally,
}

/// The four wire-native code-exec bypass methods (A4). They MUST resolve to
/// `Refuse(CodeExecBypass)` on every leg — this is asserted by the exhaustiveness
/// test so a refactor can never silently drop one back into a forward path.
pub const BYPASS_METHODS: [&str; 4] = [
    "command/exec",
    "thread/shellCommand",
    "process/spawn",
    "fs/writeFile",
];

/// The launch-fingerprint / ownership-carrying requests (A4). Present on both legs
/// (the ccd adapter never *emits* ownership fields, but the allowlist is the
/// backstop). 0.147 embeds ownership on `thread/start`, `thread/resume`,
/// `thread/fork`, and **`turn/start` (every turn)**; `turn/steer` carries none.
pub const OWNERSHIP_METHODS: [&str; 4] =
    ["thread/start", "thread/resume", "thread/fork", "turn/start"];

/// Total disposition function. Refuse-by-default: any method not matched below
/// resolves to `Refuse(NotAllowlisted)`.
pub fn disposition(role: Role, kind: JsonRpcKind, method: &str) -> Disposition {
    use Disposition::*;
    use RefuseReason::*;

    // Notifications: the census has exactly one (`initialized`). Every other
    // notification is a bypass and is refused (A4: notifications are not exempt).
    if kind == JsonRpcKind::Notification {
        return match method {
            "initialized" => Forward,
            _ => Refuse(NotAllowlisted),
        };
    }

    // The four code-exec bypass methods refuse on EVERY leg, matched first so no
    // later clause can shadow them.
    if BYPASS_METHODS.contains(&method) {
        return Refuse(CodeExecBypass);
    }

    // `thread/settings/update` durably widens policy with a bare `result:{}` (proven
    // in the spike) — ownership-adjacent, refused on both legs.
    if method == "thread/settings/update" {
        return Refuse(OwnershipAdjacent);
    }

    // Ownership-carrying requests are role-scoped (least privilege): the TUI drives the
    // session, ccd observes and attaches. Handled inside the per-role tables so ccd
    // cannot start threads/turns even after a fingerprint match.
    match role {
        Role::Tui => tui_request(method),
        Role::Ccd => ccd_request(method),
    }
}

/// tui.sock request dispositions. Grounded in the TUI bootstrap census
/// (INTERCEPTION-FINDINGS: 11 distinct bootstrap methods) plus the switch/turn
/// affordances observed in full runs.
fn tui_request(method: &str) -> Disposition {
    use Disposition::*;
    use RefuseReason::*;
    match method {
        // Ownership-carrying: thread creation/attach is fingerprint-asserted; a turn
        // additionally head-checks against the session's verified thread. (`thread/fork`
        // keeps its cell but is refused in the executor pre-2e-4c — see the disposition
        // docs; keeping the cell keeps the golden matrix stable.)
        "thread/start" | "thread/resume" | "thread/fork" => FingerprintAssert,
        "turn/start" => FingerprintThenHeadCheck,

        // Bootstrap reads (the 11-method census): read-only, no code-exec, no ownership.
        "initialize"
        | "account/read"
        | "account/rateLimits/read"
        | "hooks/list"
        | "model/list"
        | "configRequirements/read"
        | "skills/list"
        | "plugin/list"
        | "app/list" => Forward,

        // Observation reads used by the picker / resume path. Read-only, but NOT
        // unscoped: the three that name a thread are bound to this session's own
        // threads (see [`Disposition::ReadSessionThread`]). `thread/loaded/list` names
        // none and stays a plain forward.
        "thread/read" | "thread/turns/list" | "thread/items/list" => ReadSessionThread,
        "thread/loaded/list" => Forward,

        // The measured `/new` switch marker: scoped to a session thread (2e-4c).
        "thread/unsubscribe" => UnsubscribeSessionThread,
        // The composer, mid-turn. Bound to this session's head thread; the turn it names
        // is the app-server's to guard. See [`Disposition::SteerHeadThread`].
        "turn/steer" => SteerHeadThread,
        // The session's one stop control — bound to the running turn, not deferred. See
        // [`Disposition::InterruptActiveTurn`].
        "turn/interrupt" => InterruptActiveTurn,

        // Everything else on the TUI leg: refuse-by-default.
        _ => Refuse(NotAllowlisted),
    }
}

/// ccd.sock request dispositions — least privilege. The ccd adapter observes and
/// attaches (resume/read); it never starts threads or turns and never does the
/// TUI's account/bootstrap reads.
fn ccd_request(method: &str) -> Disposition {
    use Disposition::*;
    use RefuseReason::*;
    match method {
        // Attach only: ccd may resume a session-owned thread (fingerprint-asserted +
        // target-bound at runtime); it may NOT create threads.
        "thread/resume" => FingerprintAssert,
        "thread/start" | "thread/fork" => Refuse(RoleNotPermitted),

        // **The phone puts words in the model's mouth, and these are the two cells that
        // let it.** They are kept apart because the wire keeps them apart: a start is for
        // an idle thread and a steer is for a running turn, and the measured app-server
        // will happily accept a start for BOTH — answering the second with the running
        // turn's id and no staleness guard. Admitting one cell for both cases would have
        // been admitting that. See [`Disposition::FingerprintThenIdleTurn`] and
        // [`Disposition::SteerRunningTurn`] for what each proves.
        "turn/start" => FingerprintThenIdleTurn,
        "turn/steer" => SteerRunningTurn,

        // **The session's one stop control, and the one actuation this leg has.**
        // The same disposition and the same gate the TUI leg gets: an interrupt is
        // forwarded only when it names this session's bound thread and the turn
        // that is actually running. See [`Disposition::InterruptActiveTurn`], and
        // the note there on why an interrupt is not a vector actuation.
        //
        // A phone is a second pair of hands on the same session, and the thing a
        // person most needs from one is the ability to stop work they can see going
        // wrong while they are away from the machine. Refusing it here left them
        // with nothing but killing the session, which is the same reasoning that
        // admitted it on the TUI leg — read from the other end of the wire.
        //
        // **The gate is load-bearing rather than tidy.** Measured on a real
        // app-server: an interrupt naming a turn that has already ended is answered
        // with nothing at all, indefinitely — no result and no error. This predicate
        // is what stops that frame being written, so a caller is not left waiting on
        // an answer that is not coming.
        //
        // **Point-in-time, and the wording is deliberate.** The predicate reads the
        // session's active turn at classification, and a turn can end between that
        // read and the hand-off upstream — so what it removes is the ordinary case,
        // not every case. Saying it keeps the frame from EVER being written would be
        // claiming an atomicity this leg does not have. The daemon's own local gate
        // is the half that matters for the record: it is what keeps a durable row
        // from being taken for a write this one was always going to refuse.
        "turn/interrupt" => InterruptActiveTurn,

        "initialize" => Forward,
        "thread/read" | "thread/turns/list" | "thread/items/list" => ReadSessionThread,
        "thread/loaded/list" => Forward,
        _ => Refuse(NotAllowlisted),
    }
}

/// A caller's claimed `initialize` identity, restricted to what may *narrow* a role.
///
/// Role is anchored to the socket; identity may only narrow (mark a sub-role such as
/// a bootstrap/aux probe), never select or elevate a role. Reinitialization or an
/// identity that names a different role fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimedRole {
    /// No cross-role claim — leaves the socket-anchored role as is.
    Unspecified,
    /// Claims the TUI role.
    Tui,
    /// Claims the ccd role.
    Ccd,
}

/// Validate a connection's `initialize` identity against its socket-anchored role.
///
/// Returns the (possibly narrowed) effective role, or an error if the identity would
/// elevate/cross roles. A ccd connection claiming Tui (or vice versa) fails closed.
pub fn narrow_role(socket_role: Role, claimed: ClaimedRole) -> Result<Role, RoleError> {
    match (socket_role, claimed) {
        (r, ClaimedRole::Unspecified) => Ok(r),
        (Role::Tui, ClaimedRole::Tui) => Ok(Role::Tui),
        (Role::Ccd, ClaimedRole::Ccd) => Ok(Role::Ccd),
        // A claim naming the *other* socket's role is an elevation/cross attempt.
        (Role::Tui, ClaimedRole::Ccd) | (Role::Ccd, ClaimedRole::Tui) => {
            Err(RoleError::CrossRoleClaim {
                socket: socket_role,
            })
        }
    }
}

/// A role-narrowing failure — the connection must be closed (fail closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleError {
    /// The `initialize` identity named a role other than the socket's.
    CrossRoleClaim { socket: Role },
    /// A second `initialize` on an already-initialized connection.
    Reinitialization,
}

#[cfg(test)]
mod tests {
    use super::*;
    use JsonRpcKind::{Notification, Request};

    #[test]
    fn bypass_methods_refuse_on_both_legs() {
        for m in BYPASS_METHODS {
            assert_eq!(
                disposition(Role::Tui, Request, m),
                Disposition::Refuse(RefuseReason::CodeExecBypass),
                "tui {m}"
            );
            assert_eq!(
                disposition(Role::Ccd, Request, m),
                Disposition::Refuse(RefuseReason::CodeExecBypass),
                "ccd {m}"
            );
        }
    }

    #[test]
    fn ownership_methods_are_fingerprint_governed_on_tui() {
        for m in ["thread/start", "thread/resume", "thread/fork"] {
            assert_eq!(
                disposition(Role::Tui, Request, m),
                Disposition::FingerprintAssert,
                "{m}"
            );
        }
        // turn/start additionally composes the deferred head-check (fails closed).
        assert_eq!(
            disposition(Role::Tui, Request, "turn/start"),
            Disposition::FingerprintThenHeadCheck
        );
    }

    #[test]
    fn unknown_method_refuses_by_default() {
        assert_eq!(
            disposition(Role::Tui, Request, "future/method/nobody/pinned"),
            Disposition::Refuse(RefuseReason::NotAllowlisted)
        );
    }

    #[test]
    fn only_initialized_notification_forwards() {
        assert_eq!(
            disposition(Role::Tui, Notification, "initialized"),
            Disposition::Forward
        );
        assert_eq!(
            disposition(Role::Ccd, Notification, "thread/unsubscribe"),
            Disposition::Refuse(RefuseReason::NotAllowlisted)
        );
    }

    /// **The phone may steer, and the operator's own keyboard may steer.**
    ///
    /// Both legs carry `turn/steer`, and they carry DIFFERENT dispositions — which is the
    /// point of naming them here. The TUI leg is the person at the machine: the broker
    /// proves the steer names this session's head thread and lets the app-server's own
    /// `expectedTurnId` guard decide staleness. The ccd leg is a phone that cannot see the
    /// pane: the broker additionally proves the turn it names is the one this session is
    /// running, so a stale steer forwards zero bytes instead of being refused upstream by
    /// an error that names the real turn.
    #[test]
    fn steering_is_admitted_on_both_legs_under_different_rules() {
        assert_eq!(
            disposition(Role::Tui, Request, "turn/steer"),
            Disposition::SteerHeadThread
        );
        assert_eq!(
            disposition(Role::Ccd, Request, "turn/steer"),
            Disposition::SteerRunningTurn
        );
        // Steering is a request. A notification-shaped one answers nobody and is refused
        // on both legs, like every other actuation.
        for r in [Role::Tui, Role::Ccd] {
            assert_eq!(
                disposition(r, Notification, "turn/steer"),
                Disposition::Refuse(RefuseReason::NotAllowlisted)
            );
        }
    }

    /// **A turn the phone starts is admitted only on an IDLE thread.**
    ///
    /// MEASURED on 0.153.4: the app-server accepts a `turn/start` while a turn is running
    /// and answers with the RUNNING turn's id — an implicit steer carrying no
    /// `expectedTurnId` and therefore no staleness guard at all. The TUI leg keeps that
    /// behaviour (it is the operator's own keyboard, and the TUI does not send one); the
    /// ccd leg gets its own disposition so the table itself says the rule is different.
    #[test]
    fn the_phone_may_start_a_turn_only_on_an_idle_thread() {
        assert_eq!(
            disposition(Role::Ccd, Request, "turn/start"),
            Disposition::FingerprintThenIdleTurn
        );
        assert_eq!(
            disposition(Role::Tui, Request, "turn/start"),
            Disposition::FingerprintThenHeadCheck
        );
    }

    #[test]
    fn ccd_leg_attaches_and_may_stop_a_turn_and_does_nothing_else() {
        // ccd may resume (attach) and may stop the turn this session is running. It
        // may not create threads, start turns, steer one, or do the TUI's
        // account/bootstrap reads.
        assert_eq!(
            disposition(Role::Ccd, Request, "thread/resume"),
            Disposition::FingerprintAssert
        );
        // **The one actuation, and it is the gated one.** The disposition is what
        // carries the binding — naming it here is what stops a later edit from
        // widening this leg to a `Forward` that reaches any turn.
        assert_eq!(
            disposition(Role::Ccd, Request, "turn/interrupt"),
            Disposition::InterruptActiveTurn
        );
        for m in ["thread/start", "thread/fork"] {
            assert_eq!(
                disposition(Role::Ccd, Request, m),
                Disposition::Refuse(RefuseReason::RoleNotPermitted),
                "{m}"
            );
        }
        // Composing is the leg's other actuation, and it is TWO cells rather than one
        // because the wire treats a start on a busy thread as a steer with no staleness
        // guard. They are asserted in their own test so the pair reads as considered.
        assert_eq!(
            disposition(Role::Ccd, Request, "turn/start"),
            Disposition::FingerprintThenIdleTurn
        );
        assert_eq!(
            disposition(Role::Ccd, Request, "turn/steer"),
            Disposition::SteerRunningTurn
        );
        assert_eq!(
            disposition(Role::Ccd, Request, "account/read"),
            Disposition::Refuse(RefuseReason::NotAllowlisted)
        );
    }

    #[test]
    fn settings_update_is_ownership_adjacent_refuse() {
        assert_eq!(
            disposition(Role::Ccd, Request, "thread/settings/update"),
            Disposition::Refuse(RefuseReason::OwnershipAdjacent)
        );
    }

    #[test]
    fn role_is_anchored_to_socket() {
        assert_eq!(
            narrow_role(Role::Tui, ClaimedRole::Unspecified),
            Ok(Role::Tui)
        );
        assert_eq!(narrow_role(Role::Ccd, ClaimedRole::Ccd), Ok(Role::Ccd));
        // A ccd socket connection claiming Tui cannot elevate.
        assert!(narrow_role(Role::Ccd, ClaimedRole::Tui).is_err());
        assert!(narrow_role(Role::Tui, ClaimedRole::Ccd).is_err());
    }
}
