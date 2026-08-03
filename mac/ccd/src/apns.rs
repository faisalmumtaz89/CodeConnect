//! Push, behind a trait so the daemon runs without an Apple key.
//!
//! Push is a **doorbell, not a data channel**: it carries no fact and grants no
//! authority, it only tells the phone to reconnect and ask the event log what is
//! true. APNs stores exactly one offline notification per bundle id, so the
//! sender coalesces to an aggregate body and the durable event log stays
//! authoritative. The phone reconciles on foreground via `subscribe(after_seq)`;
//! a dropped push costs a delay, never a fact.

use std::sync::Mutex;

/// One thing worth waking a human for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushHint {
    pub session_id: String,
    pub title: String,
    /// Deliberately generic: APNs payloads are visible to Apple, so the command
    /// text stays on the tailnet and is fetched on tap.
    pub body: String,
    pub blocked_sessions: usize,
}

/// What one deliberate test delivery came to. Internal twin of the wire's
/// `TestPushResult`; the ws layer does the translation so this module never
/// depends on the protocol crate's wire shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestDelivery {
    /// Apple accepted it; `apns_id` is Apple's own receipt header when present.
    Accepted {
        apns_id: Option<String>,
    },
    /// The sender is the logging stub — no key configured.
    Unconfigured,
    /// The device has no registered token to send to.
    NoToken,
    Failed(String),
}

pub trait PushSender: Send + Sync {
    /// `excluded` is the seen-filter's verdict: devices whose live socket
    /// already delivered the fact this push announces. Computed by the caller
    /// at dispatch time; the sender's only job is to honour it in the fan-out.
    fn send(&self, hint: &PushHint, excluded: &[String]);
    /// One real notification to **one named device**, with the outcome
    /// reported. `send` is deliberately fire-and-forget spray; a test whose
    /// result nobody can see would prove nothing, so this one answers on the
    /// returned channel.
    fn send_test(&self, _device_id: &str) -> tokio::sync::oneshot::Receiver<TestDelivery> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(TestDelivery::Unconfigured);
        rx
    }
    /// Advertised in `hello_ack` so the phone can tell "no push configured"
    /// from "push failed".
    fn is_live(&self) -> bool {
        false
    }
}

/// The sender used until a `.p8` key exists: it logs what it *would* have sent,
/// including the coalesced body, so the coalescing logic is exercised and
/// inspectable before any Apple credential is configured.
pub struct LoggingPushSender {
    last: Mutex<Option<PushHint>>,
}

impl LoggingPushSender {
    pub fn new() -> Self {
        LoggingPushSender {
            last: Mutex::new(None),
        }
    }

    /// Test hook: the most recent hint.
    #[cfg(test)]
    pub fn last(&self) -> Option<PushHint> {
        self.last.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl PushSender for LoggingPushSender {
    fn send(&self, hint: &PushHint, excluded: &[String]) {
        let body = coalesce(hint);
        crate::log_info!(
            "push[stub] session={} title={:?} body={:?} excluded={}",
            hint.session_id,
            hint.title,
            body,
            excluded.len()
        );
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = Some(hint.clone());
    }
}

/// Aggregate-first bodies: with more than one session blocked, the count is the
/// useful fact and the per-session detail is noise on a lock screen.
pub fn coalesce(hint: &PushHint) -> String {
    match hint.blocked_sessions {
        0 | 1 => hint.body.clone(),
        n => format!("{n} agents need you"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(blocked: usize) -> PushHint {
        PushHint {
            session_id: "cc-1".into(),
            title: "cc-1 needs you".into(),
            body: "Claude needs your permission".into(),
            blocked_sessions: blocked,
        }
    }

    #[test]
    fn single_block_keeps_the_specific_body() {
        assert_eq!(coalesce(&hint(1)), "Claude needs your permission");
        assert_eq!(coalesce(&hint(0)), "Claude needs your permission");
    }

    #[test]
    fn multiple_blocks_coalesce_to_an_aggregate() {
        assert_eq!(coalesce(&hint(3)), "3 agents need you");
    }

    #[test]
    fn stub_records_but_reports_itself_as_not_live() {
        let sender = LoggingPushSender::new();
        assert!(!sender.is_live());
        sender.send(&hint(2), &[]);
        assert_eq!(sender.last().unwrap().blocked_sessions, 2);
    }
}
