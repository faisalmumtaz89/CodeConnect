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

use std::collections::HashMap;
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

    /// A Codex account's home holds only its own credential (`auth.json`): every other entry of the person's own Codex
    /// home (`CODEX_HOME` in `environment`, else `~/.codex`) that it lacks is linked into it before each launch, so a
    /// start on the account has the person's config, trust, skills and threads, and a thread one account started
    /// resumes on another (measured on codex 0.170.0, 2026-11-22: linked homes share the state db and `sessions/`).
    /// Accepted misses: a `CODEX_HOME` the person sets only in a shell file is not seen here (the home linked is the
    /// environment's, else `~/.codex`); and an entry codex makes in the account's home before the person's home has
    /// it stays the account's own.
    pub(crate) fn link_shared(&self, environment: &HashMap<OsString, OsString>) {
        if self.variable != "CODEX_HOME" {
            return;
        }
        let own = match environment.get(std::ffi::OsStr::new("CODEX_HOME")) {
            Some(home) => PathBuf::from(home),
            None => match environment.get(std::ffi::OsStr::new("HOME")) {
                Some(home) => Path::new(home).join(".codex"),
                None => return,
            },
        };
        let Ok(entries) = std::fs::read_dir(&own) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let here = self.folder.join(&name);
            if name == "auth.json" || here.symlink_metadata().is_ok() {
                continue;
            }
            if let Err(err) = std::os::unix::fs::symlink(entry.path(), &here) {
                crate::log_error!(
                    "could not link {} into an account's home: {err}",
                    here.display()
                );
            }
        }
    }

    /// The account's home, as its agent is given it.
    pub(crate) fn folder(&self) -> &Path {
        &self.folder
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
                signed_in: None,
            })
            .collect(),
        active,
    }
}

/// The sign-in a computer is running, by its number, and how to end it. One at a time: the agent's callback is one port.
#[derive(Default)]
pub(crate) struct SignIns {
    going: std::sync::Mutex<Option<(u64, tokio::sync::oneshot::Sender<()>)>>,
    count: std::sync::atomic::AtomicU64,
}

/// How long a sign-in may wait for the person in the browser before its login is ended.
pub(crate) const SIGN_IN_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// `reply`, an accounts listing, with each Codex account's `signed_in` as `codex login status` says with its home
/// (`codex`, none when Codex is not here). Claude Code's accounts are not asked yet.
pub(crate) async fn with_signed_in(
    environment: &HashMap<OsString, OsString>,
    codex: Option<&Path>,
    mut reply: protocol::ws::ServerMessage,
) -> protocol::ws::ServerMessage {
    let (protocol::ws::ServerMessage::Accounts { accounts, .. }, Some(codex)) = (&mut reply, codex)
    else {
        return reply;
    };
    let root = protocol::root_dir();
    for account in accounts
        .iter_mut()
        .filter(|account| account.agent == AgentKind::Codex)
    {
        let row = Account {
            id: account.id.clone(),
            agent: "codex".into(),
            label: account.label.clone(),
        };
        if let Ok(Some(home)) = AccountEnv::for_account(&root, &row) {
            account.signed_in =
                Some(crate::start_session::codex_signed_in(environment, Some(&home), codex).await);
        }
    }
    reply
}

/// Signs `account_id` in with its agent's own login on this computer, `codex` being Codex's path here, and answers
/// once the login ends: the listing (`signed_in` as it then stands), or `sign_in_timed_out` after `wait`. A sign-in
/// already going is ended first and its own request answered with the listing as it stands.
pub(crate) async fn sign_in(
    store: &crate::store::Store,
    sign_ins: &SignIns,
    environment: &HashMap<OsString, OsString>,
    codex: Option<&Path>,
    request_id: String,
    account_id: String,
    wait: std::time::Duration,
) -> protocol::ws::ServerMessage {
    let account = match store.accounts() {
        Ok(accounts) => accounts
            .into_iter()
            .find(|account| account.id == account_id),
        Err(err) => return unavailable(request_id, "read the accounts", err),
    };
    let Some(account) = account else {
        return refused(
            request_id,
            "that is not one of this computer's accounts".into(),
        );
    };
    if account.agent != "codex" {
        return refused(
            request_id,
            "Claude Code's sign-in can't be started from here yet".into(),
        );
    }
    let Some(codex) = codex else {
        return refused(request_id, "Codex isn't installed on this computer".into());
    };
    let home = match AccountEnv::for_account(&protocol::root_dir(), &account) {
        Ok(Some(home)) => home,
        Ok(None) => {
            return refused(
                request_id,
                "this account's agent has no sign-in here".into(),
            )
        }
        Err(err) => return unavailable(request_id, "make the account's home", err.into()),
    };
    let (end, ended) = tokio::sync::oneshot::channel();
    let mine = sign_ins
        .count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some((_, earlier)) = sign_ins.going.lock().unwrap().replace((mine, end)) {
        let _ = earlier.send(());
    }
    let mut login = crate::start_session::login_shell_command(
        environment,
        Some(&home),
        &[codex.as_os_str(), std::ffi::OsStr::new("login")],
    );
    login
        .current_dir(protocol::home_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let outcome = match login.spawn() {
        Ok(mut child) => {
            let outcome = tokio::select! {
                _ = child.wait() => None,
                _ = ended => None,
                _ = tokio::time::sleep(wait) => Some(refused_as(
                    request_id.clone(),
                    "sign_in_timed_out",
                    "the sign-in wasn't finished in the browser within ten minutes".into(),
                )),
            };
            let _ = child.kill().await;
            outcome
        }
        Err(err) => Some(unavailable(
            request_id.clone(),
            "start the sign-in",
            err.into(),
        )),
    };
    {
        let mut going = sign_ins.going.lock().unwrap();
        if going.as_ref().is_some_and(|(number, _)| *number == mine) {
            *going = None;
        }
    }
    if let Some(outcome) = outcome {
        return outcome;
    }
    let listing = listing(store, request_id);
    with_signed_in(environment, Some(codex), listing).await
}

/// How long an agent's logout may take before the removal is refused.
const LOGOUT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Removes `account_id`: its agent's own logout in its home, `codex` being Codex's path here; once the credential is
/// gone, the row (and any choice of it). The home stays, its links and nothing of the account's: a thread started
/// on the account is recorded through it and still resumes from another account (measured on codex 0.170.0,
/// 2026-11-22). Refused, with nothing removed, when the credential is still there after the logout.
pub(crate) async fn remove(
    store: &crate::store::Store,
    environment: &HashMap<OsString, OsString>,
    codex: Option<&Path>,
    request_id: String,
    account_id: String,
) -> protocol::ws::ServerMessage {
    let account = match store.accounts() {
        Ok(accounts) => accounts
            .into_iter()
            .find(|account| account.id == account_id),
        Err(err) => return unavailable(request_id, "read the accounts", err),
    };
    let Some(account) = account else {
        return refused(
            request_id,
            "that is not one of this computer's accounts".into(),
        );
    };
    if account.agent != "codex" {
        return refused(
            request_id,
            "a Claude Code account can't be removed from here yet".into(),
        );
    }
    let Some(codex) = codex else {
        return refused(request_id, "Codex isn't installed on this computer".into());
    };
    let home = match AccountEnv::for_account(&protocol::root_dir(), &account) {
        Ok(Some(home)) => home,
        Ok(None) => {
            return refused(
                request_id,
                "this account's agent has no sign-in here".into(),
            )
        }
        Err(err) => return unavailable(request_id, "find the account's home", err.into()),
    };
    let mut logout = crate::start_session::login_shell_command(
        environment,
        Some(&home),
        &[codex.as_os_str(), std::ffi::OsStr::new("logout")],
    );
    logout
        .current_dir(protocol::home_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Ok(mut child) = logout.spawn() {
        let _ = tokio::time::timeout(LOGOUT_WAIT, child.wait()).await;
    }
    if home.folder().join("auth.json").symlink_metadata().is_ok() {
        return refused_as(
            request_id,
            "accounts_unavailable",
            "Codex could not sign the account out, so nothing was removed".into(),
        );
    }
    if let Err(err) = store.remove_account(&account.id) {
        return unavailable(request_id, "remove the account", err);
    }
    let listing = listing(store, request_id);
    with_signed_in(environment, Some(codex), listing).await
}

fn refused_as(request_id: String, code: &str, message: String) -> protocol::ws::ServerMessage {
    protocol::ws::ServerMessage::AccountsRefused {
        request_id,
        code: code.into(),
        message,
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
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::Path;

    /// A stand-in codex in `dir`: `login` writes `auth.json` into its `CODEX_HOME` (or, while `dir/hang` exists,
    /// writes its pid to `dir/pid` and waits); `login status` succeeds once that home has one.
    fn stand_in_codex(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("codex");
        std::fs::write(
            &path,
            "#!/bin/sh\nhere=$(dirname \"$0\")\nif [ \"$1\" = login ] && [ \"$2\" = status ]; then [ -f \"$CODEX_HOME/auth.json\" ]; exit $?; fi\n\
             if [ \"$1\" = login ]; then if [ -e \"$here/hang\" ]; then echo $$ > \"$here/pid\"; exec sleep 30; fi; echo '{}' > \"$CODEX_HOME/auth.json\"; fi\n\
             if [ \"$1\" = logout ]; then echo \"$CODEX_HOME\" >> \"$here/logouts\"; [ -e \"$here/keep\" ] || rm -f \"$CODEX_HOME/auth.json\"; fi\n",
        )
        .unwrap();
        std::process::Command::new("chmod")
            .arg("755")
            .arg(&path)
            .status()
            .unwrap();
        path
    }

    /// Every entry under `dir`, with its kind and bytes, to compare a tree before and after.
    fn tree(dir: &Path) -> Vec<(String, bool, Vec<u8>)> {
        let mut out = Vec::new();
        for entry in walk(dir) {
            let meta = entry.symlink_metadata().unwrap();
            let bytes = if meta.is_file() {
                std::fs::read(&entry).unwrap()
            } else {
                Vec::new()
            };
            out.push((
                entry.strip_prefix(dir).unwrap().display().to_string(),
                meta.is_dir(),
                bytes,
            ));
        }
        out.sort();
        out
    }

    fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                out.extend(walk(&path));
            }
            out.push(path);
        }
        out
    }

    /// **A sign-in writes only the account's own credential, and its listing says so** (306 Slice B, B1, B2): the
    /// stand-in codex's login, run by the service with A's home, leaves the person's own Codex home and B's home as
    /// they were; the reply names A signed in and B not.
    #[tokio::test]
    async fn a_sign_in_writes_only_its_own_credential() {
        let _env = crate::ProcessEnvRead::take();
        let root = std::env::temp_dir().join(format!(
            "ccs-signin.{}",
            &protocol::uid::new().unwrap()[20..]
        ));
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".codex").join("sessions")).unwrap();
        std::fs::write(home.join(".codex").join("config.toml"), "x").unwrap();
        let codex = stand_in_codex(&root);
        let environment: HashMap<OsString, OsString> = HashMap::from([
            (OsString::from("HOME"), home.clone().into_os_string()),
            (OsString::from("SHELL"), OsString::from("/bin/sh")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]);
        let store = crate::store::Store::open_temp("ccd-signin");
        let _db = store.temp_db();
        let a = store.add_account("codex", "A").unwrap();
        let b = store.add_account("codex", "B").unwrap();
        let folder = |id: &str| protocol::root_dir().join("accounts").join("codex").join(id);
        let person_before = tree(&home);
        let reply = super::sign_in(
            &store,
            &super::SignIns::default(),
            &environment,
            Some(&codex),
            "s1".into(),
            a.id.clone(),
            std::time::Duration::from_secs(20),
        )
        .await;
        let reply = serde_json::to_value(&reply).unwrap();
        assert_eq!(reply["type"], "accounts", "{reply}");
        let signed = |id: &str| {
            reply["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == id)
                .unwrap()["signed_in"]
                .clone()
        };
        assert_eq!(
            (signed(&a.id), signed(&b.id)),
            (serde_json::json!(true), serde_json::json!(false)),
            "B2"
        );
        assert!(
            folder(&a.id)
                .join("auth.json")
                .symlink_metadata()
                .unwrap()
                .is_file(),
            "A's own credential"
        );
        assert!(
            folder(&b.id).join("auth.json").symlink_metadata().is_err(),
            "B untouched"
        );
        assert_eq!(
            tree(&home),
            person_before,
            "B1: the person's home is as it was"
        );
        let _ = std::fs::remove_dir_all(folder(&a.id));
        let _ = std::fs::remove_dir_all(folder(&b.id));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A sign-in not finished ends, and a new one replaces the one going** (306 Slice B, B3): a login that waits past
    /// its bound is ended and refused `sign_in_timed_out`; a second sign-in ends the first's login, whose request is
    /// answered with the listing as it stands, and runs its own.
    #[tokio::test]
    async fn a_sign_in_times_out_and_a_new_one_replaces_the_one_going() {
        let _env = crate::ProcessEnvRead::take();
        let root = std::env::temp_dir().join(format!(
            "ccs-signin2.{}",
            &protocol::uid::new().unwrap()[20..]
        ));
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        let codex = stand_in_codex(&root);
        let environment: HashMap<OsString, OsString> = HashMap::from([
            (OsString::from("HOME"), home.clone().into_os_string()),
            (OsString::from("SHELL"), OsString::from("/bin/sh")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]);
        let store = crate::store::Store::open_temp("ccd-signin2");
        let _db = store.temp_db();
        let a = store.add_account("codex", "A").unwrap();
        let sign_ins = super::SignIns::default();
        let gone = |pid: i32| async move {
            for _ in 0..100 {
                // SAFETY: kill(2) with signal 0 only asks whether the process exists.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            false
        };
        let pid = || async {
            for _ in 0..200 {
                if let Some(pid) = std::fs::read_to_string(root.join("pid"))
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok())
                {
                    return pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            panic!("the login never started");
        };

        std::fs::write(root.join("hang"), "").unwrap();
        let late = super::sign_in(
            &store,
            &sign_ins,
            &environment,
            Some(&codex),
            "s1".into(),
            a.id.clone(),
            std::time::Duration::from_millis(600),
        )
        .await;
        let late = serde_json::to_value(&late).unwrap();
        assert_eq!(
            (late["type"].as_str(), late["code"].as_str()),
            (Some("accounts_refused"), Some("sign_in_timed_out")),
            "{late}"
        );
        assert!(gone(pid().await).await, "the timed-out login is ended");
        std::fs::remove_file(root.join("pid")).unwrap();

        let (first, second) = tokio::join!(
            super::sign_in(
                &store,
                &sign_ins,
                &environment,
                Some(&codex),
                "s2".into(),
                a.id.clone(),
                std::time::Duration::from_secs(20)
            ),
            async {
                let first_pid = pid().await;
                std::fs::remove_file(root.join("hang")).unwrap();
                let second = super::sign_in(
                    &store,
                    &sign_ins,
                    &environment,
                    Some(&codex),
                    "s3".into(),
                    a.id.clone(),
                    std::time::Duration::from_secs(20),
                )
                .await;
                assert!(
                    gone(first_pid).await,
                    "the first login is ended by the second sign-in"
                );
                second
            }
        );
        let (first, second) = (
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&second).unwrap(),
        );
        assert_eq!(
            (first["type"].as_str(), first["request_id"].as_str()),
            (Some("accounts"), Some("s2")),
            "{first}"
        );
        assert_eq!(
            (
                second["type"].as_str(),
                second["accounts"][0]["signed_in"].as_bool()
            ),
            (Some("accounts"), Some(true)),
            "{second}"
        );
        let _ = std::fs::remove_dir_all(
            protocol::root_dir()
                .join("accounts")
                .join("codex")
                .join(&a.id),
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **Removing an account signs it out and keeps its home as links** (306 Slice D, D1, D2): the stand-in's logout runs
    /// with the account's home and takes its credential; the row and the agent's choice of it go (Default again); the
    /// home stays, its links still the person's, so a thread recorded through it still resolves. A logout that leaves
    /// the credential is refused and nothing is removed.
    #[tokio::test]
    async fn removing_an_account_signs_it_out_and_keeps_its_home_as_links() {
        let _env = crate::ProcessEnvRead::take();
        let root = std::env::temp_dir().join(format!(
            "ccs-remove.{}",
            &protocol::uid::new().unwrap()[20..]
        ));
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".codex").join("sessions")).unwrap();
        std::fs::write(
            home.join(".codex").join("sessions").join("rollout-a.jsonl"),
            "{}",
        )
        .unwrap();
        let codex = stand_in_codex(&root);
        let environment: HashMap<OsString, OsString> = HashMap::from([
            (OsString::from("HOME"), home.clone().into_os_string()),
            (OsString::from("SHELL"), OsString::from("/bin/sh")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]);
        let store = crate::store::Store::open_temp("ccd-remove");
        let _db = store.temp_db();
        let a = store.add_account("codex", "A").unwrap();
        assert!(store.activate_account("codex", Some(&a.id)).unwrap());
        let folder = protocol::root_dir()
            .join("accounts")
            .join("codex")
            .join(&a.id);
        let signed = super::sign_in(
            &store,
            &super::SignIns::default(),
            &environment,
            Some(&codex),
            "s1".into(),
            a.id.clone(),
            std::time::Duration::from_secs(20),
        )
        .await;
        assert_eq!(
            serde_json::to_value(&signed).unwrap()["accounts"][0]["signed_in"],
            true
        );

        std::fs::write(root.join("keep"), "").unwrap();
        let kept = serde_json::to_value(
            super::remove(
                &store,
                &environment,
                Some(&codex),
                "r1".into(),
                a.id.clone(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(
            (kept["type"].as_str(), kept["code"].as_str()),
            (Some("accounts_refused"), Some("accounts_unavailable")),
            "{kept}"
        );
        assert_eq!(
            store.accounts().unwrap().len(),
            1,
            "nothing removed while the credential is there"
        );
        std::fs::remove_file(root.join("keep")).unwrap();

        let gone = serde_json::to_value(
            super::remove(
                &store,
                &environment,
                Some(&codex),
                "r2".into(),
                a.id.clone(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(
            (
                gone["type"].as_str(),
                gone["accounts"].clone(),
                gone["active"].clone()
            ),
            (
                Some("accounts"),
                serde_json::json!([]),
                serde_json::json!([])
            ),
            "{gone}"
        );
        let logouts = std::fs::read_to_string(root.join("logouts")).unwrap();
        assert!(
            logouts.lines().all(|line| Path::new(line) == folder),
            "D1: {logouts}"
        );
        assert!(
            folder.join("auth.json").symlink_metadata().is_err(),
            "the credential is gone"
        );
        let sessions = folder.join("sessions");
        assert!(
            sessions
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink(),
            "D2: the home stays as links"
        );
        assert!(
            sessions.join("rollout-a.jsonl").exists(),
            "a thread recorded through it still resolves"
        );
        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A Codex account's home is the person's, but for its credential** (306 Slice A, gates A1–A3): after a launch's
    /// linking, the account sees the person's config; a file one account writes under `sessions/` is the other's too;
    /// the person's `auth.json` is never linked; an entry the person's home gains later is linked at the next launch,
    /// and a link already there is left as it is.
    #[test]
    fn a_codex_accounts_home_is_the_persons_but_for_its_credential() {
        let root =
            std::env::temp_dir().join(format!("ccs-home.{}", &protocol::uid::new().unwrap()[20..]));
        let own = root.join("own-codex");
        std::fs::create_dir_all(own.join("sessions")).unwrap();
        std::fs::write(own.join("config.toml"), "model = \"x\"\n").unwrap();
        std::fs::write(own.join("auth.json"), "{}").unwrap();
        std::fs::write(own.join("state_5.sqlite"), "").unwrap();
        let environment: HashMap<OsString, OsString> =
            HashMap::from([(OsString::from("CODEX_HOME"), own.clone().into_os_string())]);
        let account = |label: &str| {
            let account = crate::store::Account {
                id: protocol::uid::new().unwrap(),
                agent: "codex".into(),
                label: label.into(),
            };
            super::AccountEnv::for_account(&root, &account)
                .unwrap()
                .unwrap()
        };
        let (a, b) = (account("A"), account("B"));
        let home = |env: &super::AccountEnv| {
            Path::new(
                env.argv()
                    .last()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .split_once('=')
                    .unwrap()
                    .1,
            )
            .to_path_buf()
        };
        a.link_shared(&environment);
        b.link_shared(&environment);
        assert_eq!(
            std::fs::read_to_string(home(&a).join("config.toml")).unwrap(),
            "model = \"x\"\n",
            "A1"
        );
        std::fs::write(home(&a).join("sessions").join("rollout-1.jsonl"), "{}").unwrap();
        assert!(
            home(&b).join("sessions").join("rollout-1.jsonl").exists(),
            "A2: A's thread is B's too"
        );
        assert!(home(&a)
            .join("state_5.sqlite")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(
            home(&a).join("auth.json").symlink_metadata().is_err(),
            "the credential is the account's own"
        );
        std::fs::create_dir(own.join("skills")).unwrap();
        a.link_shared(&environment);
        for name in ["config.toml", "sessions", "state_5.sqlite", "skills"] {
            let meta = home(&a).join(name).symlink_metadata().unwrap();
            assert!(meta.file_type().is_symlink(), "A3: {name} is still a link");
            assert_eq!(
                std::fs::read_link(home(&a).join(name)).unwrap(),
                own.join(name)
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The accounts wire the apps read is what ccd answers** (`fixtures/accounts/minor-47-wire.json`, recorded from a
    /// ccd at 05faeaa3 by one paired device). Each request, in the recorded order, is answered by this build on a store
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
