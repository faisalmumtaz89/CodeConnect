//! CodeConnect's Codex broker: the WebSocket-over-Unix-socket relay in front of the Codex
//! app-server, and the refuse-by-default security core for the phone's leg.
//!
//! # What this crate is
//!
//! The broker sits between the Codex TUI / the CodeConnect daemon (ccd) and the Codex
//! app-server. It relays whole WebSocket messages. The TUI's frames pass through as they
//! are — the person at the keyboard is as trusted as in native codex — and the broker
//! watches them to know which thread the keyboard is on and what is running. The
//! daemon's frames, which speak for a phone, meet the security core: refuse-by-default
//! classification and the refusal matrix. It is the only line of defence for the phone —
//! `configRequirements/read` returns all-null by default, so there is no server-side
//! backstop.
//!
//! # Layering
//!
//! * [`message`], [`allowlist`], [`response_capability`], [`session`], [`refusal`] are the
//!   **pure, synchronous security core**. [`refusal::decide`] is the phone leg's single
//!   entrypoint: `(capability-registry, session, parsed message) → action`
//!   ([`refusal::classify`] parses first, for tests). No I/O, no async — unit-tested
//!   against captured frame shapes. (The one observability seam is the audit sink the
//!   per-leg [`response_capability::LegCapabilities`] logs winner-provenance through.)
//! * [`upstream`] and [`relay`] are the **async transport edge**: UDS listeners, the
//!   RFC6455 handshake, whole-message reassembly, and forwarding. The upstream
//!   app-server connection sits behind the [`upstream::UpstreamFactory`] trait so the relay is
//!   integration-tested against captured frames with no live app-server.

pub mod allowlist;
pub mod frame_tee;
pub mod message;
pub mod redact;
pub mod refusal;
pub mod response_capability;
pub mod session;

pub mod relay;
pub mod upstream;

pub use allowlist::{disposition, Disposition, JsonRpcKind, RefuseReason, Role};
pub use frame_tee::{FrameTee, FRAME_TEE_ENV};
pub use message::WsPayload;
pub use refusal::{classify, Env, RelayAction};
pub use response_capability::{
    LegCapabilities, NoCapabilities, ResponseArbiter, ResponseCapabilityRegistry,
    COMMAND_EXEC_APPROVAL, FILE_CHANGE_APPROVAL, GENERATION_UNSTAMPED,
};
pub use session::{ConnId, IdAdmission, IdLedgerCounts, SessionThreads, ThreadBinding};
