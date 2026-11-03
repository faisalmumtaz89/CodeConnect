//! `codeconnect claude|codex --no-attach [-- <prompt>]`: start the session a
//! terminal start would, without showing it. The daemon runs this for a session the
//! phone starts, through the user's login shell in the project's folder.
//!
//! There is no terminal to take a size or a `TERM` from, so the session starts at
//! [`SIZE`] until a terminal attaches (the phone's, or `codeconnect attach` on the
//! Mac), and with `TERM=xterm-256color` when the shell's environment has none. The
//! outcome is written as one line to the file `CODECONNECT_LAUNCH_RESULT` names
//! ([`protocol::launch_result`]), never printed: what the shell's startup files
//! print shares stdout and stderr.

use std::path::PathBuf;

use anyhow::{bail, Result};
use protocol::agent::AgentKind;
use protocol::launch_result::{LaunchResult, ENV_LAUNCH_RESULT};
use protocol::ws::StartFailure;

pub const FLAG: &str = "--no-attach";

/// The size a session starts at with no terminal to take one from.
pub const SIZE: (u16, u16) = (120, 40);

/// The `TERM` a session is given when the environment has none.
const DEFAULT_TERM: &str = "xterm-256color";

/// The session a launch created.
pub struct Started {
    pub uid: String,
    pub session_name: String,
}

/// Whether `codeconnect <agent> <args>` asks for a launch with no attach.
pub fn requested(args: &[String]) -> bool {
    args.first().is_some_and(|arg| arg == FLAG)
}

/// Run the launch `args` (starting with [`FLAG`]) asks for and write its outcome.
pub fn run(agent: AgentKind, args: &[String]) -> Result<()> {
    // Read and removed first, so the session's environment never carries it.
    let result = std::env::var_os(ENV_LAUNCH_RESULT).map(PathBuf::from);
    std::env::remove_var(ENV_LAUNCH_RESULT);
    if std::env::var_os("TERM").is_none() {
        std::env::set_var("TERM", DEFAULT_TERM);
    }
    let started = agent_args(&args[1..]).and_then(|passthrough| match &agent {
        AgentKind::Claude => crate::start_detached(&passthrough),
        AgentKind::Codex => crate::codex::start(&passthrough, true),
        other => bail!("{} sessions cannot be started this way", other.as_str()),
    });
    let outcome = match &started {
        Ok(started) => LaunchResult::Started {
            uid: started.uid.clone(),
            session_name: started.session_name.clone(),
            agent,
        },
        Err(err) => LaunchResult::Failed {
            error: if err.is::<AgentMissing>() {
                StartFailure::AgentMissing
            } else {
                StartFailure::LaunchFailed
            },
            detail: format!("{err:#}"),
        },
    };
    if let Some(path) = result {
        outcome.write_to(&path)?;
    }
    started.map(|_| ())
}

/// What the agent is handed: nothing, or the first prompt after `--`, so a prompt
/// that starts with `-` is never read as a flag. Nothing else is accepted.
fn agent_args(args: &[String]) -> Result<Vec<String>> {
    match args {
        [] => Ok(Vec::new()),
        [dashes, prompt] if dashes == "--" => Ok(vec![dashes.clone(), prompt.clone()]),
        _ => bail!("usage: codeconnect claude|codex {FLAG} [-- <prompt>]"),
    }
}

/// The agent's executable could not be found, which the phone reports as the agent
/// not being installed. Shown exactly as the error it wraps.
#[derive(Debug)]
pub struct AgentMissing(anyhow::Error);

impl std::fmt::Display for AgentMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for AgentMissing {}

pub fn agent_missing(err: anyhow::Error) -> anyhow::Error {
    AgentMissing(err).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn the_agent_gets_nothing_or_one_prompt_after_dashes() {
        assert!(agent_args(&[]).unwrap().is_empty());
        for prompt in ["fix the tests", "-p", "--model x", "a;", "$(touch x)\nnext"] {
            assert_eq!(
                agent_args(&strings(&["--", prompt])).unwrap(),
                strings(&["--", prompt])
            );
        }
    }

    #[test]
    fn anything_else_is_refused() {
        for args in [
            &["--model", "x"][..],
            &["fix the tests"],
            &["--"],
            &["--", "a", "b"],
            &["--no-attach"],
        ] {
            assert!(agent_args(&strings(args)).is_err(), "{args:?}");
        }
    }

    #[test]
    fn a_missing_agent_reads_as_the_error_it_wraps() {
        let err = agent_missing(anyhow::anyhow!("could not find the claude binary"));
        assert!(err.is::<AgentMissing>());
        assert_eq!(format!("{err:#}"), "could not find the claude binary");
    }
}
