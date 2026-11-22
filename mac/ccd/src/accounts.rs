//! An agent's accounts on this computer (N4, protocol minor 47).
//!
//! An account is a row in the store ([`crate::store::Account`]) and a folder of ccd's own, `<root>/accounts/<agent>/<id>/`
//! (0700), named only by the id ccd made. A start on it is given the folder in the one variable its agent reads for
//! that: Claude Code's sign-in store (`CLAUDE_SECURESTORAGE_CONFIG_DIR`: only where the sign-in is kept moves; the
//! person's settings, MCP servers and sessions stay their own) and Codex's whole home (`CODEX_HOME`). An agent's
//! Default is the agent as it is on this computer: nothing is added to its environment and nothing taken out.
//!
//! The variable is set at exec, after the login shell's start-up files ([`AccountEnv::argv`]), so nothing those files
//! export can move a start off its account; `/usr/bin/env` execs the agent in its place, so the process ccd spawned
//! keeps its pid throughout.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use protocol::agent::AgentKind;

use crate::store::Account;

/// The variable that points `agent` at an account's folder.
fn variable(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some("CLAUDE_SECURESTORAGE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        _ => None,
    }
}

/// An account's folder, as a start on it is given it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountEnv {
    id: String,
    variable: &'static str,
    folder: PathBuf,
}

impl AccountEnv {
    /// `account`'s folder under `root`, made (0700) when it is not there yet. None for an agent with no such variable.
    pub(crate) fn for_account(root: &Path, account: &Account) -> std::io::Result<Option<AccountEnv>> {
        let Some(variable) = variable(&account.agent) else {
            return Ok(None);
        };
        // The id is ccd's own (`protocol::uid`), never anything a person typed: it is the only name in the path.
        if !protocol::uid::is_well_formed(&account.id) {
            return Err(std::io::Error::other("an account id ccd did not make"));
        }
        let folder = root.join("accounts").join(&account.agent).join(&account.id);
        protocol::fsperm::private_dir(&folder)?;
        Ok(Some(AccountEnv { id: account.id.clone(), variable, folder }))
    }

    /// The account `agent`'s starts use now, read once per start; None for its Default.
    pub(crate) fn active(
        store: &crate::store::Store,
        root: &Path,
        agent: &AgentKind,
    ) -> anyhow::Result<Option<AccountEnv>> {
        match store.active_account(agent.as_str())? {
            Some(account) => Ok(AccountEnv::for_account(root, &account)?),
            None => Ok(None),
        }
    }

    /// The account's id, as a run records it.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// The argv a launch runs its agent behind: `/usr/bin/env NAME=<folder>`, run by the login shell's `exec` after
    /// its start-up files.
    pub(crate) fn argv(&self) -> [OsString; 2] {
        let mut setting = OsString::from(self.variable);
        setting.push("=");
        setting.push(&self.folder);
        [OsString::from("/usr/bin/env"), setting]
    }
}

/// A device's ask about accounts, answered with every account and each agent's active one once the ask's change is
/// made. An unknown agent, or an id that is not one of the agent's, is refused and nothing changes.
pub(crate) fn answer(store: &crate::store::Store, message: protocol::ws::ClientMessage) -> protocol::ws::ServerMessage {
    use protocol::ws::ClientMessage;
    let request_id = match message {
        ClientMessage::ListAccounts { request_id } => request_id,
        ClientMessage::AddAccount { request_id, agent, label } => {
            if variable(agent.as_str()).is_none() {
                return refused(format!("{} has no accounts here", agent.as_str()));
            }
            if let Err(err) = store.add_account(agent.as_str(), label.trim()) {
                return unavailable("keep the account", err);
            }
            request_id
        }
        ClientMessage::ActivateAccount { request_id, agent, account_id } => {
            match store.activate_account(agent.as_str(), account_id.as_deref()) {
                Ok(true) => request_id,
                Ok(false) => return refused(format!("that is not one of {}'s accounts", agent.as_str())),
                Err(err) => return unavailable("choose the account", err),
            }
        }
        _ => unreachable!("only the account frames are answered here"),
    };
    listing(store, request_id)
}

fn listing(store: &crate::store::Store, request_id: String) -> protocol::ws::ServerMessage {
    let accounts = match store.accounts() {
        Ok(accounts) => accounts,
        Err(err) => return unavailable("read the accounts", err),
    };
    let mut active = Vec::new();
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        match store.active_account(agent.as_str()) {
            Ok(Some(account)) => active.push(protocol::ws::ActiveAccount { agent, account_id: account.id }),
            Ok(None) => {}
            Err(err) => return unavailable("read the accounts", err),
        }
    }
    protocol::ws::ServerMessage::Accounts {
        request_id,
        accounts: accounts
            .into_iter()
            .map(|account| protocol::ws::AgentAccount {
                id: account.id,
                agent: AgentKind::from_str_lossy(&account.agent),
                label: account.label,
            })
            .collect(),
        active,
    }
}

fn refused(message: String) -> protocol::ws::ServerMessage {
    protocol::ws::ServerMessage::Error { code: "bad_request".into(), message }
}

pub(crate) fn unavailable(what: &str, err: anyhow::Error) -> protocol::ws::ServerMessage {
    crate::log_error!("could not {what}: {err:#}");
    protocol::ws::ServerMessage::Error {
        code: "accounts_unavailable".into(),
        message: format!("could not {what}: {err}"),
    }
}
