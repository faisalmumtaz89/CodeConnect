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
//!
//! A named Claude Code account also drops the environment's credentials that outrank a stored login, so the account's
//! own sign-in is what pays. Claude Code's order (code.claude.com/docs/en/authentication, "Authentication precedence"):
//! a cloud provider (`CLAUDE_CODE_USE_BEDROCK`, `_VERTEX`, `_FOUNDRY`), `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`,
//! `apiKeyHelper`, `CLAUDE_CODE_OAUTH_TOKEN`, a named profile (`ANTHROPIC_PROFILE`) or federation variables
//! (`ANTHROPIC_FEDERATION_RULE_ID` with `ANTHROPIC_ORGANIZATION_ID`; unsetting either stops them), then the stored
//! login. `apiKeyHelper` and an active `oidc_federation` profile are settings and files, not the environment, and stay
//! the person's. Codex's documentation (learn.chatgpt.com/docs/auth) names no environment credential that outranks its
//! home's login, so a Codex account drops nothing.

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

/// The environment's credentials that outrank `agent`'s stored login (see the module's note).
fn outranking(agent: &str) -> &'static [&'static str] {
    match agent {
        "claude" => &[
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_PROFILE",
            "ANTHROPIC_FEDERATION_RULE_ID",
        ],
        _ => &[],
    }
}

/// The longest label kept, in characters.
const LABEL_MAX: usize = 64;

/// An account's folder, as a start on it is given it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountEnv {
    id: String,
    variable: &'static str,
    outranking: &'static [&'static str],
    folder: PathBuf,
}

impl AccountEnv {
    /// `account`'s folder under `root`, made (0700) when it is not there yet. None for an agent with no such variable.
    pub(crate) fn for_account(
        root: &Path,
        account: &Account,
    ) -> std::io::Result<Option<AccountEnv>> {
        let Some(variable) = variable(&account.agent) else {
            return Ok(None);
        };
        // The id is ccd's own (`protocol::uid`), never anything a person typed: it is the only name in the path.
        if !protocol::uid::is_well_formed(&account.id) {
            return Err(std::io::Error::other("an account id ccd did not make"));
        }
        let folder = root.join("accounts").join(&account.agent).join(&account.id);
        protocol::fsperm::private_dir(&folder)?;
        Ok(Some(AccountEnv {
            id: account.id.clone(),
            variable,
            outranking: outranking(&account.agent),
            folder,
        }))
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

    /// The argv a launch runs its agent behind: `/usr/bin/env -u <outranking>… NAME=<folder>`, run by the login
    /// shell's `exec` after its start-up files.
    pub(crate) fn argv(&self) -> Vec<OsString> {
        let mut setting = OsString::from(self.variable);
        setting.push("=");
        setting.push(&self.folder);
        let mut argv = vec![OsString::from("/usr/bin/env")];
        for name in self.outranking {
            argv.extend([OsString::from("-u"), OsString::from(name)]);
        }
        argv.push(setting);
        argv
    }
}

/// A device's ask about accounts, answered with every account and each agent's active one once the ask's change is
/// made. An unknown agent, or an id that is not one of the agent's, is refused and nothing changes; a refusal names the
/// request it answers (`AccountsRefused`).
pub(crate) fn answer(
    store: &crate::store::Store,
    message: protocol::ws::ClientMessage,
) -> protocol::ws::ServerMessage {
    use protocol::ws::ClientMessage;
    let request_id = request_id(&message);
    match message {
        ClientMessage::ListAccounts { .. } => {}
        ClientMessage::AddAccount { agent, label, .. } => {
            if variable(agent.as_str()).is_none() {
                return refused(
                    request_id,
                    format!("{} has no accounts here", agent.as_str()),
                );
            }
            let label = label.trim();
            if label.is_empty() || label.chars().count() > LABEL_MAX {
                return refused(
                    request_id,
                    format!("an account's name is 1 to {LABEL_MAX} characters"),
                );
            }
            if let Err(err) = store.add_account(agent.as_str(), label) {
                return unavailable(request_id, "keep the account", err);
            }
        }
        ClientMessage::ActivateAccount { agent, .. } if variable(agent.as_str()).is_none() => {
            return refused(
                request_id,
                format!("{} has no accounts here", agent.as_str()),
            );
        }
        ClientMessage::ActivateAccount {
            agent, account_id, ..
        } => match store.activate_account(agent.as_str(), account_id.as_deref()) {
            Ok(true) => {}
            Ok(false) => {
                return refused(
                    request_id,
                    format!("that is not one of {}'s accounts", agent.as_str()),
                )
            }
            Err(err) => return unavailable(request_id, "choose the account", err),
        },
        _ => unreachable!("only the account frames are answered here"),
    }
    listing(store, request_id)
}

/// The request an accounts ask names.
pub(crate) fn request_id(message: &protocol::ws::ClientMessage) -> String {
    use protocol::ws::ClientMessage;
    match message {
        ClientMessage::ListAccounts { request_id }
        | ClientMessage::AddAccount { request_id, .. }
        | ClientMessage::ActivateAccount { request_id, .. } => request_id.clone(),
        _ => unreachable!("only the account frames are answered here"),
    }
}

fn listing(store: &crate::store::Store, request_id: String) -> protocol::ws::ServerMessage {
    let accounts = match store.accounts() {
        Ok(accounts) => accounts,
        Err(err) => return unavailable(request_id, "read the accounts", err),
    };
    let mut active = Vec::new();
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        match store.active_account(agent.as_str()) {
            Ok(Some(account)) => active.push(protocol::ws::ActiveAccount {
                agent,
                account_id: account.id,
            }),
            Ok(None) => {}
            Err(err) => return unavailable(request_id, "read the accounts", err),
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

fn refused(request_id: String, message: String) -> protocol::ws::ServerMessage {
    protocol::ws::ServerMessage::AccountsRefused {
        request_id,
        code: "bad_request".into(),
        message,
    }
}

pub(crate) fn unavailable(
    request_id: String,
    what: &str,
    err: anyhow::Error,
) -> protocol::ws::ServerMessage {
    crate::log_error!("could not {what}: {err:#}");
    protocol::ws::ServerMessage::AccountsRefused {
        request_id,
        code: "accounts_unavailable".into(),
        message: format!("could not {what}: {err}"),
    }
}

#[cfg(test)]
mod tests {
    /// **The accounts wire the apps read is what ccd answers** (`fixtures/accounts/minor-47-wire.json`, recorded from a
    /// ccd at 35433a6f by one paired device). Each request, in the recorded order, is answered by this build on a store
    /// of its own; the reply's bytes are the recorded reply's, the ids aside: ccd makes new ones, so each is read as the
    /// recorded id it stands for, in the order the replies first name them.
    #[test]
    fn the_recorded_accounts_wire_is_what_this_build_answers() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/accounts/minor-47-wire.json"
        ))
        .unwrap();
        let store = crate::store::Store::open_temp("ccd-accounts-wire");
        let _db = store.temp_db();
        let mut ids: Vec<(String, String)> = Vec::new(); // (recorded, made here)
                                                         // In the order they were recorded (their request ids, fx-a1 on): a JSON object keeps no order.
        let mut exchanges: Vec<_> = fixture
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, exchange)| exchange.is_object())
            .collect();
        exchanges.sort_by_key(|(_, exchange)| {
            exchange["request"]["request_id"]
                .as_str()
                .unwrap()
                .to_string()
        });
        for (name, exchange) in exchanges {
            let mut request = exchange["request"].to_string();
            for (recorded, made) in &ids {
                request = request.replace(recorded, made);
            }
            let reply = super::answer(&store, serde_json::from_str(&request).unwrap());
            let mut said = serde_json::to_value(&reply).unwrap();
            let recorded_ids: Vec<&str> = exchange["reply"]["accounts"]
                .as_array()
                .map(|accounts| accounts.iter().map(|a| a["id"].as_str().unwrap()).collect())
                .unwrap_or_default();
            if let Some(accounts) = said["accounts"].as_array() {
                for (account, recorded) in accounts.iter().zip(&recorded_ids) {
                    let made = account["id"].as_str().unwrap();
                    if !ids.iter().any(|(_, m)| m == made) {
                        ids.push((recorded.to_string(), made.to_string()));
                    }
                }
            }
            let mut text = said.to_string();
            for (recorded, made) in &ids {
                text = text.replace(made, recorded);
            }
            said = serde_json::from_str(&text).unwrap();
            assert_eq!(said, exchange["reply"], "{name}");
        }
        assert_eq!(fixture["hello_ack_capabilities_accounts"], true);
    }
}
