//! The phone leg's refuse-by-default allowlist.
//!
//! What a phone may send through CodeConnect's daemon is **refuse-by-default**: only an
//! explicit, vetted set of messages forwards; everything else is refused per the refusal
//! matrix. The wire-native code-exec methods (`command/exec`, `thread/shellCommand`,
//! `process/spawn`, `fs/writeFile`) run code with caller-chosen `dangerFullAccess`, no
//! approval and no observer, so a *denylist* that forgets one is a full bypass.
//!
//! The keyboard leg has no table. It is the person at the machine, as trusted as in native
//! codex, and its frames pass through ([`crate::relay`]).
//!
//! ## Keying
//!
//! Each cell is `(JSON-RPC kind × method)` and carries exactly one [`Disposition`].
//!
//! ## Exhaustiveness
//!
//! [`disposition`] is a total function — every method (pinned or unknown) maps to exactly
//! one disposition, and unknown/future methods fall through to `Refuse(NotAllowlisted)`.
//! `tests/exhaustiveness.rs` restates the table cell by cell and proves that every client
//! method codex's schema declares (`tests/codex-client-methods.txt`, 0.147.0 and 0.155.1)
//! outside it refuses, and that the four bypass methods resolve to `Refuse(CodeExecBypass)`.

/// Which unix socket a client connection arrived on. Role is anchored here, not to the
/// caller-controlled `initialize` identity.
///
/// `Hash` is kept because [`crate::response_capability`] keys a winner by role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// `tui.sock` — the Codex TUI (and its `/resume` picker's second connection).
    Tui,
    /// `ccd.sock` — the CodeConnect daemon, speaking for a phone.
    Ccd,
}

/// The JSON-RPC kind used to key the allowlist. Responses are classified by shape (they
/// carry no method) and never consult this table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonRpcKind {
    Request,
    Notification,
}

/// Why a message is refused. Carried for the audit log; every variant forwards zero bytes
/// upstream (a request with a usable id additionally gets a synthetic error — see
/// [`crate::refusal`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// One of the four wire-native code-exec bypass methods (or a sibling in those
    /// families caught by refuse-by-default).
    CodeExecBypass,
    /// A method that would durably move approval/sandbox/hook ownership
    /// (`thread/settings/update`). Only the keyboard changes a thread's settings.
    OwnershipAdjacent,
    /// A method the phone's least-privilege model forbids: `thread/start` / `thread/fork`
    /// — the phone attaches to the thread the keyboard is on and never creates one.
    RoleNotPermitted,
    /// Refuse-by-default: an unknown/future method, or a method not vetted for the phone.
    NotAllowlisted,
    /// A shape with no client→server form (array / binary / malformed).
    Malformed,
}

/// The disposition of a single allowlist cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Forward the reassembled bytes upstream unchanged (vetted benign).
    Forward,
    /// **`turn/start` from the phone**: the head-check on an IDLE thread, with the
    /// text-only `input` rule [`Disposition::SteerRunningTurn`] carries — and no ownership
    /// of its own. Every param but `threadId` and `input` is JSON null, so the turn runs
    /// under the thread's current settings, which only the keyboard can change.
    ///
    /// # The idle rule
    ///
    /// MEASURED on 0.153.4, against the app-server's own socket: a `turn/start` sent while
    /// a turn is running is not queued and is not refused — it is ACCEPTED, and answered
    /// with the RUNNING turn's id. It is an implicit steer, and unlike a real `turn/steer`
    /// it carries no `expectedTurnId`, so it has no staleness guard at all: a phone whose
    /// view of the session is a second out of date would inject its text into whatever turn
    /// happened to be running when the bytes landed. So the phone's start is admitted only
    /// against an idle thread, and the phone's steer is the guarded method for the other
    /// case.
    NullOwnedIdleTurn,
    /// Refuse now (zero upstream bytes; synthetic error only for a request with a usable
    /// id).
    Refuse(RefuseReason),
    /// **`thread/resume` from the phone**: forward iff the params are exactly
    /// `{"threadId": <string>}` and that thread is one of THIS session's.
    ///
    /// ccd attaches to the thread the keyboard's session is on and names nothing else about
    /// it, so its resume carries no ownership fields and no workspace. Any other key is a
    /// setting the phone would be choosing, and is refused.
    ResumeSessionThread,
    /// Forward iff `params.threadId` names a thread of THIS session — the thread-scoped
    /// READS (`thread/read`, `thread/turns/list`, `thread/items/list`).
    ///
    /// All three take a caller-chosen `threadId`, and `codex_home()` is the operator's own
    /// `~/.codex` — so every CodeConnect session on a machine shares one thread store and an
    /// unscoped read would reach any other session's conversation.
    ///
    /// `thread/loaded/list` is deliberately NOT here. MEASURED with a two-session canary
    /// against one real 0.153 app-server, it enumerates the app-server PROCESS's in-memory
    /// thread set and returns thread IDS ONLY: no title, preview, cwd or rollout path.
    /// CodeConnect gives each session its own app-server under its own `CODEX_HOME`, so
    /// what it enumerates is this host's own session.
    ReadSessionThread,
    /// **`turn/steer` from the phone**: forward iff `params.threadId` is the head, the
    /// params are the measured shape, and:
    ///
    /// 1. **`expectedTurnId` is the turn the head is actually running** — the same
    ///    [`crate::session::ThreadBinding::is_active_turn`] predicate that binds
    ///    [`Disposition::InterruptActiveTurn`]. MEASURED on 0.153.4: the app-server refuses
    ///    a stale one with `-32600 "expected active turn id X but found Y"`, which hands
    ///    the caller the REAL running turn id. A phone is not looking at the pane and has
    ///    no business learning that id from a refusal, so a stale steer forwards zero bytes
    ///    and is refused here instead.
    /// 2. **Every `input` item is TEXT.** The `UserInput` union also carries `localImage`,
    ///    `localAudio`, `skill` and `mention` — each taking an absolute filesystem PATH the
    ///    app-server reads itself, outside the model's sandbox — and `image`/`audio`, which
    ///    take a URL. A phone that could name those would have a read-and-exfiltrate
    ///    primitive that no sandbox policy fences.
    SteerRunningTurn,
    /// Forward iff `params.threadId` is the head AND `params.turnId` is a turn the server
    /// announced or answered and no terminal has cleared — i.e. the turn that is actually
    /// RUNNING.
    ///
    /// An interrupt carries no ownership fields, starts nothing, and names a turn rather
    /// than steering one. The two things it must not be able to do — reach another
    /// session's thread, or name a turn this session is not running — are exactly what
    /// [`crate::session::ThreadBinding::is_active_turn`] decides.
    InterruptActiveTurn,
}

/// The four wire-native code-exec bypass methods. They MUST resolve to
/// `Refuse(CodeExecBypass)` — asserted by the exhaustiveness test so a refactor can never
/// silently drop one back into a forward path.
pub const BYPASS_METHODS: [&str; 4] = [
    "command/exec",
    "thread/shellCommand",
    "process/spawn",
    "fs/writeFile",
];

/// The phone leg's total disposition function. Refuse-by-default: any method not matched
/// below resolves to `Refuse(NotAllowlisted)`.
pub fn disposition(kind: JsonRpcKind, method: &str) -> Disposition {
    use Disposition::*;
    use RefuseReason::*;

    // Notifications: the census has exactly one (`initialized`). Every other notification
    // is a bypass and is refused (notifications are not exempt).
    if kind == JsonRpcKind::Notification {
        return match method {
            "initialized" => Forward,
            _ => Refuse(NotAllowlisted),
        };
    }

    // The four code-exec bypass methods, matched first so no later clause can shadow them.
    if BYPASS_METHODS.contains(&method) {
        return Refuse(CodeExecBypass);
    }

    match method {
        // `thread/settings/update` durably widens policy with a bare `result:{}` (proven in
        // the spike). Only the keyboard changes a thread's settings.
        "thread/settings/update" => Refuse(OwnershipAdjacent),

        // Attach only: the phone may resume a session thread, naming nothing but the
        // thread; it may NOT create threads.
        "thread/resume" => ResumeSessionThread,
        "thread/start" | "thread/fork" => Refuse(RoleNotPermitted),

        // **The phone puts words in the model's mouth, and these are the two cells that
        // let it.** They are kept apart because the wire keeps them apart: a start is for
        // an idle thread and a steer is for a running turn, and the measured app-server
        // will happily accept a start for BOTH — answering the second with the running
        // turn's id and no staleness guard.
        "turn/start" => NullOwnedIdleTurn,
        "turn/steer" => SteerRunningTurn,

        // **The session's one stop control.** A phone is a second pair of hands on the
        // same session, and the thing a person most needs from one is the ability to stop
        // work they can see going wrong while they are away from the machine.
        //
        // **The gate is load-bearing rather than tidy.** Measured on a real app-server: an
        // interrupt naming a turn that has already ended is answered with nothing at all,
        // indefinitely — no result and no error. This predicate is what stops that frame
        // being written in the ordinary case. It reads the session's active turn at
        // classification, and a turn can end between that read and the hand-off upstream,
        // so it does not claim more than that.
        "turn/interrupt" => InterruptActiveTurn,

        "initialize" => Forward,
        "thread/read" | "thread/turns/list" | "thread/items/list" => ReadSessionThread,
        "thread/loaded/list" => Forward,
        _ => Refuse(NotAllowlisted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use JsonRpcKind::{Notification, Request};

    #[test]
    fn bypass_methods_refuse() {
        for m in BYPASS_METHODS {
            assert_eq!(
                disposition(Request, m),
                Disposition::Refuse(RefuseReason::CodeExecBypass),
                "{m}"
            );
        }
    }

    #[test]
    fn unknown_method_refuses_by_default() {
        assert_eq!(
            disposition(Request, "future/method/nobody/pinned"),
            Disposition::Refuse(RefuseReason::NotAllowlisted)
        );
    }

    #[test]
    fn only_initialized_notification_forwards() {
        assert_eq!(
            disposition(Notification, "initialized"),
            Disposition::Forward
        );
        for m in ["thread/unsubscribe", "turn/steer", "turn/start"] {
            assert_eq!(
                disposition(Notification, m),
                Disposition::Refuse(RefuseReason::NotAllowlisted),
                "{m}"
            );
        }
    }

    #[test]
    fn the_phone_attaches_composes_steers_and_stops_and_does_nothing_else() {
        assert_eq!(
            disposition(Request, "thread/resume"),
            Disposition::ResumeSessionThread
        );
        assert_eq!(
            disposition(Request, "turn/start"),
            Disposition::NullOwnedIdleTurn
        );
        assert_eq!(
            disposition(Request, "turn/steer"),
            Disposition::SteerRunningTurn
        );
        assert_eq!(
            disposition(Request, "turn/interrupt"),
            Disposition::InterruptActiveTurn
        );
        for m in ["thread/start", "thread/fork"] {
            assert_eq!(
                disposition(Request, m),
                Disposition::Refuse(RefuseReason::RoleNotPermitted),
                "{m}"
            );
        }
        for m in [
            "account/read",
            "thread/unsubscribe",
            "thread/list",
            "config/read",
        ] {
            assert_eq!(
                disposition(Request, m),
                Disposition::Refuse(RefuseReason::NotAllowlisted),
                "{m}"
            );
        }
    }

    #[test]
    fn settings_update_is_ownership_adjacent_refuse() {
        assert_eq!(
            disposition(Request, "thread/settings/update"),
            Disposition::Refuse(RefuseReason::OwnershipAdjacent)
        );
    }
}
