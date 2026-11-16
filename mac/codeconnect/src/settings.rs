//! A run's own directory under the sessions folder.

use std::path::PathBuf;

/// A run's own directory under the sessions folder, named `<session id>-<session uid>`.
pub fn session_dir(session_id: &str, session_uid: &str) -> PathBuf {
    protocol::sessions_dir().join(format!("{session_id}-{session_uid}"))
}
