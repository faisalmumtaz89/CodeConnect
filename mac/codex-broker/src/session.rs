//! Session-scoped thread binding (A4 / finding 5).
//!
//! An ownership-free `thread/resume` must not be honored for an arbitrary thread id — a
//! ccd (or any) client could otherwise resume a thread that does not belong to this
//! launch. The broker binds resume to the session: it learns the thread ids that belong
//! to this session by **observing the server→client stream** (the `thread/started`
//! broadcast carries the whole Thread including its id, A1/D1), and refuses a resume for
//! any thread it has not observed.
//!
//! ## What is bound now, and the labeled 2e seam
//!
//! Threads **created or attached through this broker in this session** are bound (any
//! leg's `thread/started` populates the shared set). A resume of a thread this session
//! never observed — e.g. a pre-existing thread the wrapper captured at bootstrap before
//! the broker was relaying — is **refused** (fail closed). Wiring that fuller lineage
//! (the wrapper's bootstrap thread-identity capture) is Phase 2e; until then an
//! unbindable resume refuses, never forwards. This is the deliberate fail-closed seam.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Oracle: does a thread id belong to this session?
pub trait ThreadBinding: Send + Sync {
    fn is_session_thread(&self, thread_id: &str) -> bool;
}

/// The set of thread ids observed to belong to this session, learned from the s2c stream.
#[derive(Debug, Clone, Default)]
pub struct SessionThreads {
    inner: Arc<Mutex<HashSet<String>>>,
}

impl SessionThreads {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a thread id as session-owned (e.g. from the wrapper bootstrap capture, 2e).
    pub fn note(&self, thread_id: impl Into<String>) {
        self.inner.lock().unwrap().insert(thread_id.into());
    }

    /// Observe one server→client frame and learn any thread id it announces.
    ///
    /// Cheap-guarded: only frames that mention a thread-producing method are parsed, so a
    /// multi-MB `app/list/updated` / `plugin/list` frame is skipped (it never contains the
    /// marker). A missed id simply means a later resume fails closed.
    pub fn observe_server_frame(&self, text: &str) {
        if !(text.contains("thread/started") || text.contains("thread/resumed")) {
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        let method = v.get("method").and_then(|m| m.as_str());
        if !matches!(method, Some("thread/started") | Some("thread/resumed")) {
            return;
        }
        if let Some(id) = v
            .get("params")
            .and_then(|p| p.get("thread"))
            .and_then(|t| t.get("id"))
            .and_then(|i| i.as_str())
        {
            self.note(id);
        }
    }
}

impl ThreadBinding for SessionThreads {
    fn is_session_thread(&self, thread_id: &str) -> bool {
        self.inner.lock().unwrap().contains(thread_id)
    }
}

/// A binding that knows no threads — every resume target is unbound (fail closed). Used
/// as the default in unit tests that are not exercising resume binding.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoThreads;

impl ThreadBinding for NoThreads {
    fn is_session_thread(&self, _thread_id: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_thread_id_from_started_broadcast() {
        let s = SessionThreads::new();
        assert!(!s.is_session_thread("01a0"));
        s.observe_server_frame(
            r#"{"method":"thread/started","params":{"thread":{"id":"01a0","path":"/x"}}}"#,
        );
        assert!(s.is_session_thread("01a0"));
    }

    #[test]
    fn ignores_unrelated_and_unparseable_frames() {
        let s = SessionThreads::new();
        s.observe_server_frame(r#"{"method":"app/list/updated","params":{"apps":[]}}"#);
        s.observe_server_frame("not json");
        assert!(!s.is_session_thread("anything"));
    }
}
