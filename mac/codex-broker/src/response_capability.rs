//! The one-use response-capability registry — a **clean seam for the switch/fanout
//! sub-chunk**.
//!
//! A4: method-less responses are one-use capabilities, one per upstream request/visit,
//! atomically consumed across all deliveries (the first matching `{id,result}` **or**
//! `{id,error}` wins; siblings are revoked). Each grant is bound to socket role and
//! connection/delivery epoch; the two phone-supported families grant both a ccd and a
//! TUI response, the observe-only families grant only the TUI.
//!
//! That machinery — registration on the upstream `serverRequest`, atomic
//! cross-delivery consumption, the visit-generation key, and the winner-provenance
//! `ccd` disposition side channel — is **deferred**. This sub-chunk ships the trait and
//! a fail-closed [`NoCapabilities`] implementation: with no registered capability,
//! **every** method-less response is unauthorized, so it forwards zero upstream bytes
//! (a normal losing-fanout race — logged, leg kept open — per the refusal matrix). The
//! switch sub-chunk swaps in the real registry with no change to the classifier or the
//! refusal matrix.

use crate::allowlist::Role;
use crate::message::RequestId;

/// Authorization oracle for a method-less response (an approval answer).
///
/// The classifier consults this to decide whether a `{id, result|error}` response
/// matches a live, authorized, one-use capability granted to this endpoint. Anything
/// unauthorized (unsolicited / duplicate / cross-thread / stale-visit / losing-sibling
/// / wrong-role) forwards zero bytes.
pub trait ResponseCapabilityRegistry {
    /// True iff `id` names a live capability granted to `role` that this response may
    /// consume. Implementations MUST consume atomically (a real one revokes siblings);
    /// the deferred impl here never authorizes.
    fn authorize(&self, role: Role, id: &RequestId, is_error: bool) -> bool;
}

/// The deferred, fail-closed registry: no capability is ever authorized, so every
/// method-less response forwards zero upstream bytes.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCapabilities;

impl ResponseCapabilityRegistry for NoCapabilities {
    fn authorize(&self, _role: Role, _id: &RequestId, _is_error: bool) -> bool {
        false
    }
}
