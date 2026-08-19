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
//! ## Deferred dispositions (clean seam for the switch/fanout sub-chunk)
//!
//! Three composable actions from the plan's set — `hold/serialize`, `head-check`,
//! `consume-locally` — require machinery this sub-chunk deliberately defers (the D2
//! vector barrier / quiesce / seal, and the one-use response-capability fanout).
//! Their table entries are **final and correct** ([`Disposition::HoldSerialize`],
//! [`Disposition::HeadCheck`], [`Disposition::ConsumeLocally`]); only their
//! *executor branches* are stubbed, and they **fail closed** here (see
//! [`crate::refusal`]). The switch sub-chunk fills in the executor, not the table.

/// Which unix socket a client connection arrived on. Role is anchored here, not to
/// the caller-controlled `initialize` identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// `thread/start` / `thread/fork` / `turn/start` — ccd observes and attaches only).
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

/// The composable disposition of a single allowlist cell. The plan's action set is
/// `{refuse, consume-locally, hold/serialize, head-check, fingerprint-assert/inject,
/// forward}`; all six are represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Forward the reassembled bytes upstream unchanged (vetted benign).
    Forward,
    /// Ownership-carrying request: assert the launch fingerprint over the params
    /// (typed fields *and* the `config` map, key-scoped) before forwarding; a
    /// conflict, or an absent ownership field on a policy-setting method, is refused.
    FingerprintAssert,
    /// **Composable**: fingerprint-assert THEN the D2 head-check acceptance barrier.
    /// `turn/start` needs later switch-quiesce sequencing, so with the head-check
    /// executor deferred to the next sub-chunk the composed disposition **fails closed**
    /// — the fingerprint is still evaluated (a conflict is a distinct policy refusal),
    /// but a fingerprint-clean `turn/start` is refused as deferred, never forwarded.
    FingerprintThenHeadCheck,
    /// Refuse now (zero upstream bytes; synthetic error only for a request with a
    /// usable id).
    Refuse(RefuseReason),
    /// DEFERRED: serialize behind the thread-switch latch (`thread/unsubscribe`).
    HoldSerialize,
    /// DEFERRED: the D2 vector acceptance barrier for a thread-scoped actuation
    /// (`turn/steer`, `turn/interrupt`).
    HeadCheck,
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
        // additionally needs the deferred head-check, so it fails closed until then.
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

        // Observation reads used by the picker / resume path (read-only).
        "thread/read" | "thread/loaded/list" | "thread/turns/list" | "thread/items/list" => Forward,

        // Switch marker and thread-scoped actuations — final dispositions whose
        // machinery is deferred (fail closed here).
        "thread/unsubscribe" => HoldSerialize,
        "turn/steer" | "turn/interrupt" => HeadCheck,

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
        // target-bound at runtime); it may NOT create threads or start turns.
        "thread/resume" => FingerprintAssert,
        "thread/start" | "thread/fork" | "turn/start" => Refuse(RoleNotPermitted),

        "initialize" => Forward,
        "thread/read" | "thread/loaded/list" | "thread/turns/list" | "thread/items/list" => Forward,
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

    #[test]
    fn ccd_leg_is_attach_only() {
        // ccd may resume (attach), but may not create threads or start turns, and does
        // not do the TUI's account/bootstrap reads.
        assert_eq!(
            disposition(Role::Ccd, Request, "thread/resume"),
            Disposition::FingerprintAssert
        );
        for m in ["thread/start", "thread/fork", "turn/start"] {
            assert_eq!(
                disposition(Role::Ccd, Request, m),
                Disposition::Refuse(RefuseReason::RoleNotPermitted),
                "{m}"
            );
        }
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
