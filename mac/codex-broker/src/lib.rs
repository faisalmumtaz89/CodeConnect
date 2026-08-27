//! CodeConnect Codex broker — the in-wrapper WebSocket-over-UDS relay and its
//! refuse-by-default allowlist security core (Phase 2d-i).
//!
//! # What this crate is
//!
//! The broker sits between the Codex TUI / the CodeConnect daemon (ccd) and the Codex
//! app-server. It relays whole WebSocket messages and enforces A4's security core:
//! refuse-by-default classification, the refusal matrix, and ownership-fingerprint
//! validation. It is the only line of defence — `configRequirements/read` returns
//! all-null by default, so there is no server-side backstop.
//!
//! # Layering (Karpathy: correct pure core first, I/O at the edges)
//!
//! * [`message`], [`allowlist`], [`fingerprint`], [`response_capability`], [`refusal`]
//!   are the **pure, synchronous security core**. [`refusal::classify`] is the single
//!   entrypoint: `(role, fingerprint, capability-registry, whole-message) → action`.
//!   No I/O, no async — unit-tested against captured Phase-0 frame shapes. (The one
//!   observability seam is the audit sink the per-leg
//!   [`response_capability::LegCapabilities`] logs winner-provenance through.)
//! * [`upstream`] and [`relay`] are the **async transport edge**: UDS listeners, the
//!   RFC6455 handshake, whole-message reassembly, and forwarding. The upstream
//!   app-server connection sits behind the [`upstream::Upstream`] trait so the relay is
//!   integration-tested against captured frames with no live app-server.
//!
//! # Deliberately deferred to the NEXT (switch/fanout) sub-chunk
//!
//! The D2 thread-switch vector barrier / quiesce / seal, the D3 quiescence control
//! protocol, the D4 generation/epoch stamping and attach envelope, and the
//! byte-fidelity comparison harness. The one-use response-capability fanout registry
//! itself IS built here ([`response_capability`]) — it authorizes/arbitrates approval
//! answers and records winner-provenance — but its D4 generation stamping and the
//! broker→ccd winner-signal *injection* remain labeled 2e seams. Clean seams are
//! marked `SEAM:` throughout and are
//! summarized in the crate README. Nothing here launches a real app-server (Phase 2e)
//! or wires the broker into the wrapper/coordinator (Phase 2c).

pub mod allowlist;
pub mod fingerprint;
pub mod message;
pub mod redact;
pub mod refusal;
pub mod response_capability;
pub mod session;

pub mod relay;
pub mod upstream;

pub use allowlist::{disposition, Disposition, JsonRpcKind, RefuseReason, Role};
pub use fingerprint::LaunchFingerprint;
pub use message::WsPayload;
pub use refusal::{classify, Env, RelayAction};
pub use response_capability::{
    LegCapabilities, NoCapabilities, ResponseArbiter, ResponseCapabilityRegistry,
    COMMAND_EXEC_APPROVAL, FILE_CHANGE_APPROVAL, GENERATION_UNSTAMPED,
};
pub use session::{
    ConnId, IdAdmission, IdLedgerCounts, PrefixAdmission, SessionThreads, ThreadBinding,
    VerifiedThread,
};
