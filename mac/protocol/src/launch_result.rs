//! What a `codeconnect <agent> --no-attach` started, told to the daemon that ran it.
//!
//! The daemon names a file in [`ENV_LAUNCH_RESULT`]; the launcher writes one
//! [`LaunchResult`] there as one JSON line and exits. The launcher's stdout and
//! stderr carry whatever the user's shell startup prints, so they are only ever
//! the detail of a failure, never the result.

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::ws::StartFailure;

/// The environment variable that names the result file.
pub const ENV_LAUNCH_RESULT: &str = "CODECONNECT_LAUNCH_RESULT";

/// One launch's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LaunchResult {
    /// The session exists: its tmux session and supervisor (Claude), or its
    /// coordinator's ready record (Codex).
    Started {
        uid: String,
        session_name: String,
        agent: AgentKind,
    },
    /// Nothing was started. `detail` is the launcher's error text.
    Failed { error: StartFailure, detail: String },
}

impl LaunchResult {
    /// Write this result to `path` as one JSON line, replacing anything there.
    pub fn write_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        std::fs::write(path, line)
    }

    /// Read the line [`Self::write_to`] wrote.
    pub fn read_from(path: &std::path::Path) -> std::io::Result<LaunchResult> {
        let text = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(text.trim_end())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_shapes_are_the_lines_the_launcher_and_daemon_agreed() {
        let dir = std::env::temp_dir().join(format!("cc-launch-result-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("result.json");

        let started = LaunchResult::Started {
            uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            session_name: "cc-3".into(),
            agent: AgentKind::Codex,
        };
        started.write_to(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"uid\":\"01K1B3XQ8ZC0DE5FGH7JKMNPQR\",\"session_name\":\"cc-3\",\"agent\":\"codex\"}\n"
        );
        assert_eq!(LaunchResult::read_from(&path).unwrap(), started);

        let failed = LaunchResult::Failed {
            error: StartFailure::AgentMissing,
            detail: "claude: not found".into(),
        };
        failed.write_to(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"error\":\"agent_missing\",\"detail\":\"claude: not found\"}\n"
        );
        assert_eq!(LaunchResult::read_from(&path).unwrap(), failed);

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
}
