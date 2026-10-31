//! The name Claude Code records for a session, which it also draws into the
//! session's message box once the session is named (`──── myjob ─`).
//!
//! Claude keeps the current name in a file of its own beside the transcript,
//! `<transcript without .jsonl>/custom-title.json` holding `{"customTitle": …}`:
//! written on every rename (a name another session already holds comes back
//! with a suffix, and that suffixed name is what is written and drawn), deleted
//! when the name is cleared, and read back by Claude itself (2.1.316, measured).
//! Anything else — no file, a file that does not parse, an empty name — is no
//! name, and a named session's box is then not recognised: the phone is refused,
//! as it always was.

use std::path::Path;

#[derive(serde::Deserialize)]
struct Title {
    #[serde(rename = "customTitle")]
    custom_title: String,
}

/// The session's recorded name, read now.
pub fn recorded(transcript: &Path) -> Option<String> {
    let text =
        std::fs::read_to_string(transcript.with_extension("").join("custom-title.json")).ok()?;
    let name = serde_json::from_str::<Title>(&text).ok()?.custom_title;
    (!name.is_empty()).then_some(name)
}

/// Whether `name` is also what the box of some agent draws in its rule, so
/// that a rule bearing it can be that agent's box rather than the session's.
///
/// Claude keeps a record of every agent it starts, nested ones included, at
/// `<projects>/<project>/<session>/subagents/agent-*.meta.json`, written when
/// the agent starts and holding the task exactly as the agent's box draws it —
/// trimmed, every run of whitespace one space — or, for an agent given a name,
/// the `@name` drawn instead (2.1.316, measured). A running agent's record stays
/// under the session that started it while the person moves on with `/branch`
/// or `/resume`, and the agent with them, so every record under Claude's
/// projects directory counts: any agent task recorded on this computer, running
/// or not. A directory or a record that cannot be read or parsed counts as
/// naming it: either way the name proves nothing.
pub fn names_an_agent(transcript: &Path, name: &str) -> bool {
    let Some(projects) = transcript.parent().and_then(Path::parent) else {
        return true;
    };
    let walk = || -> std::io::Result<bool> {
        for project in std::fs::read_dir(projects)? {
            let project = project?;
            if !project.file_type()?.is_dir() {
                continue;
            }
            for session in std::fs::read_dir(project.path())? {
                let session = session?;
                if !session.file_type()?.is_dir() {
                    continue;
                }
                let records = match std::fs::read_dir(session.path().join("subagents")) {
                    Ok(records) => records,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err),
                };
                for record in records {
                    let path = record?.path();
                    if !path.to_string_lossy().ends_with(".meta.json") {
                        continue;
                    }
                    let text = std::fs::read_to_string(&path)?;
                    let Ok(agent) = serde_json::from_str::<Agent>(&text) else {
                        return Ok(true);
                    };
                    if agent.description.as_deref() == Some(name)
                        || agent
                            .name
                            .is_some_and(|agent_name| format!("@{agent_name}") == name)
                    {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    };
    walk().unwrap_or(true)
}

/// The two fields of an agent's record that its box can draw.
#[derive(serde::Deserialize)]
struct Agent {
    description: Option<String>,
    name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A projects directory of its own holding one session's transcript and,
    /// when given, its sidecar and agent records, laid out as Claude lays them
    /// out (`<projects>/proj/2f0c.jsonl`). Returns the projects directory, for
    /// the caller to remove, and the transcript.
    fn session(sidecar: Option<&str>, agents: &[&str]) -> (PathBuf, PathBuf) {
        let projects =
            std::env::temp_dir().join(format!("cc-session-name-{}", protocol::uid::new().unwrap()));
        let session = projects.join("proj/2f0c");
        std::fs::create_dir_all(&session).unwrap();
        let transcript = projects.join("proj/2f0c.jsonl");
        std::fs::write(&transcript, "").unwrap();
        if let Some(sidecar) = sidecar {
            std::fs::write(session.join("custom-title.json"), sidecar).unwrap();
        }
        record_agents(&session, agents);
        (projects, transcript)
    }

    /// Agent records in `session/subagents/`, each beside its own transcript.
    fn record_agents(session: &Path, agents: &[&str]) {
        if agents.is_empty() {
            return;
        }
        std::fs::create_dir_all(session.join("subagents")).unwrap();
        for (i, agent) in agents.iter().enumerate() {
            std::fs::write(
                session.join(format!("subagents/agent-{i}.meta.json")),
                agent,
            )
            .unwrap();
            std::fs::write(session.join(format!("subagents/agent-{i}.jsonl")), "").unwrap();
        }
    }

    #[test]
    fn the_name_is_what_the_sidecar_records_and_nothing_else_is_one() {
        let (dir, named) = session(Some(r#"{"customTitle":"my job ☕ café"}"#), &[]);
        assert_eq!(recorded(&named).as_deref(), Some("my job ☕ café"));
        std::fs::remove_dir_all(dir).unwrap();
        for sidecar in [
            None,
            Some(""),
            Some("{"),
            Some(r#"{"customTitle":""}"#),
            Some(r#"{"title":"x"}"#),
        ] {
            let (dir, transcript) = session(sidecar, &[]);
            assert_eq!(recorded(&transcript), None, "{sidecar:?} is no name");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// Agent records as Claude Code 2.1.316 writes them: a background agent
    /// (`bgtask`), one whose task was given as `"  two   spaces\tTab\nline"`
    /// and is recorded and drawn squeezed, one started by another agent
    /// (`spawnDepth` 2, its Agent call only in that agent's own transcript),
    /// and one given the name `mate`, whose box draws `@mate`.
    const AGENTS: [&str; 4] = [
        include_str!(
            "../../../fixtures/claude/subagents-2.1.316/agent-a617e437af2276c7a.meta.json"
        ),
        include_str!(
            "../../../fixtures/claude/subagents-2.1.316/agent-a0cc4670c8aeb12ee.meta.json"
        ),
        include_str!(
            "../../../fixtures/claude/subagents-2.1.316/agent-a2cbd71d6eaf9f31a.meta.json"
        ),
        include_str!(
            "../../../fixtures/claude/subagents-2.1.316/agent-ac3c7966bcc8ef9b5.meta.json"
        ),
    ];

    /// What every agent's box draws is found, from the records alone: the
    /// transcript here holds no Agent call at all, as after `/clear`.
    #[test]
    fn a_name_an_agents_box_draws_is_found_and_no_other_is() {
        let (dir, transcript) = session(None, &AGENTS);
        for drawn in [
            "bgtask",
            "two spaces Tab line",
            "nested",
            "namedesc",
            "@mate",
        ] {
            assert!(names_an_agent(&transcript, drawn), "{drawn:?}");
        }
        for other in [
            "fsjob",
            "mate",
            "general-purpose",
            "two   spaces\tTab\nline",
        ] {
            assert!(!names_an_agent(&transcript, other), "{other:?}");
        }
        std::fs::remove_dir_all(dir).unwrap();
        let (dir, no_agents) = session(None, &[]);
        assert!(
            !names_an_agent(&no_agents, "fsjob"),
            "a session that started none"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A record that cannot be read for what it says proves nothing, and
    /// neither does a directory of them that cannot be listed.
    #[test]
    fn an_unreadable_record_names_every_agent() {
        let (dir, transcript) = session(None, &["{"]);
        assert!(
            names_an_agent(&transcript, "fsjob"),
            "a record that does not parse"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        let (dir, transcript) = session(None, &[]);
        std::fs::write(dir.join("proj/2f0c/subagents"), "").unwrap();
        assert!(
            names_an_agent(&transcript, "fsjob"),
            "a directory that cannot be listed"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An agent still running after `/branch` or `/resume` keeps its record
    /// under the session that started it, which is not the one the person is
    /// in now — or under another project's. Either counts.
    #[test]
    fn an_agent_recorded_under_another_session_or_project_still_counts() {
        let (dir, transcript) = session(Some(r#"{"customTitle":"bgtask"}"#), &[]);
        record_agents(&dir.join("proj/9a1e"), &[AGENTS[0]]);
        assert!(
            names_an_agent(&transcript, "bgtask"),
            "the session it was started in"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        let (dir, transcript) = session(Some(r#"{"customTitle":"bgtask"}"#), &[]);
        record_agents(&dir.join("other-proj/9a1e"), &[AGENTS[0]]);
        std::fs::write(dir.join("other-proj/9a1e.jsonl"), "").unwrap();
        assert!(
            names_an_agent(&transcript, "bgtask"),
            "another project's session"
        );
        assert!(!names_an_agent(&transcript, "fsjob"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
