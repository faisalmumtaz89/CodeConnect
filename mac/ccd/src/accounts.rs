//! An agent's accounts on this computer (N4, protocol minor 48).
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
use std::sync::Arc;

use protocol::agent::AgentKind;

use crate::store::Account;

/// `program`'s argv for its own `what` (`login`, `logout`): Codex's `login` / `logout`, Claude Code's
/// `auth login` / `auth logout`.
fn login_args<'a>(agent: &str, program: &'a Path, what: &'static str) -> Vec<&'a std::ffi::OsStr> {
    let mut args = vec![program.as_os_str()];
    if agent == "claude" {
        args.push(std::ffi::OsStr::new("auth"));
    }
    args.push(std::ffi::OsStr::new(what));
    args
}

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
    /// Claude Code's config directory set to the folder too: for its own sign-in, status and logout only, so the
    /// identity they write and read is the account's own and the person's shared one is never touched (measured on
    /// claude 2.1.330, 2026-11-22). A session on the account runs with the storage variable alone, as before.
    signing: bool,
}

/// The programs of the agents on this computer, as a start would find them.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Agents<'a> {
    pub(crate) codex: Option<&'a Path>,
    pub(crate) claude: Option<&'a Path>,
}

impl Agents<'_> {
    /// The program of the agent `agent` names (`codex`, `claude`).
    pub(crate) fn path(&self, agent: &str) -> Option<&Path> {
        match agent {
            "codex" => self.codex,
            "claude" => self.claude,
            _ => None,
        }
    }
}

/// An agent's name as a person reads it.
fn product(agent: &str) -> &'static str {
    match agent {
        "claude" => "Claude Code",
        _ => "Codex",
    }
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
            signing: false,
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
        if self.signing && self.variable == "CLAUDE_SECURESTORAGE_CONFIG_DIR" {
            let mut config = OsString::from("CLAUDE_CONFIG_DIR=");
            config.push(&self.folder);
            argv.push(config);
        }
        argv
    }

    /// This account's environment for its agent's own sign-in, status and logout ([`AccountEnv::signing`]).
    pub(crate) fn signing(&self) -> AccountEnv {
        AccountEnv {
            signing: true,
            ..self.clone()
        }
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
    listing(store, Some(request_id))
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

pub(crate) fn listing(
    store: &crate::store::Store,
    request_id: Option<String>,
) -> protocol::ws::ServerMessage {
    let accounts = match store.accounts() {
        Ok(accounts) => accounts,
        Err(err) => {
            return unavailable(
                request_id.clone().unwrap_or_default(),
                "read the accounts",
                err,
            )
        }
    };
    let mut active = Vec::new();
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        match store.active_account(agent.as_str()) {
            Ok(Some(account)) => active.push(protocol::ws::ActiveAccount {
                agent,
                account_id: account.id,
            }),
            Ok(None) => {}
            Err(err) => {
                return unavailable(
                    request_id.clone().unwrap_or_default(),
                    "read the accounts",
                    err,
                )
            }
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
                signing_in: false,
                not_separate: false,
                facts: None,
                same_as_another: false,
            })
            .collect(),
        active,
        defaults: Vec::new(),
    }
}

/// What each account (and each agent's Default, keyed [`default_key`]) says of itself, and each account's sign-in, read
/// behind the accounts calls and kept in memory only: never stored, never logged. A read, found or not, is held for
/// [`FACTS_FRESH`]; the sign-ins are read again once that long has passed, or when a call may have changed one.
/// One refresh runs at a time; a call during one asks it to go round once more.
#[derive(Default)]
pub(crate) struct Facts {
    known:
        std::sync::Mutex<HashMap<String, (Option<protocol::ws::AccountFacts>, std::time::Instant)>>,
    /// Each account's `signed_in` and `not_separate` as its agent last said ([`with_signed_in`]), read behind every
    /// accounts call; an account not read yet has none, so its row says nothing of its sign-in.
    signed: std::sync::Mutex<HashMap<String, (Option<bool>, bool)>>,
    /// When the sign-ins were last read (none when a call may have changed one since), and how many times a call has
    /// so asked: a read begun before such a call is kept but not taken for fresh.
    signed_at: std::sync::Mutex<(Option<std::time::Instant>, u64)>,
    /// A refresh is running, and whether a call asked for another after it.
    refreshing: std::sync::Mutex<(bool, bool)>,
    reading: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Keys forgotten while being read: that read found the old sign-in's facts, so it is not kept.
    dropped: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// The key of `agent`'s Default among [`Facts`].
pub(crate) fn default_key(agent: &AgentKind) -> String {
    format!("default:{}", agent.as_str())
}

/// How long what an account says of itself is shown without reading it again.
pub(crate) const FACTS_FRESH: std::time::Duration = std::time::Duration::from_secs(60);

/// How long one read of an account may take: an app-server started, two questions, ended.
const FACTS_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

impl Facts {
    /// The keys among `keys` held for less than [`FACTS_FRESH`] are left; each other one not being read now is
    /// claimed for a read and returned.
    pub(crate) fn claim_stale(&self, keys: &[String]) -> Vec<String> {
        let known = self.known.lock().unwrap();
        let mut reading = self.reading.lock().unwrap();
        keys.iter()
            .filter(|key| {
                known
                    .get(*key)
                    .is_none_or(|(_, at)| at.elapsed() >= FACTS_FRESH)
            })
            .filter(|key| reading.insert((*key).clone()))
            .cloned()
            .collect()
    }

    /// The read of `key` is over: what it found, if anything, is kept.
    pub(crate) fn read(&self, key: &str, facts: Option<protocol::ws::AccountFacts>) {
        if !self.dropped.lock().unwrap().remove(key) {
            self.known
                .lock()
                .unwrap()
                .insert(key.to_string(), (facts, std::time::Instant::now()));
        }
        self.reading.lock().unwrap().remove(key);
    }

    /// When the sign-ins are to be read again (never read, read [`FACTS_FRESH`] ago, or a call may have changed one):
    /// the count of such calls the read begins at, for [`Facts::keep_signed`].
    pub(crate) fn signed_stale(&self) -> Option<u64> {
        let (at, asked) = *self.signed_at.lock().unwrap();
        at.is_none_or(|at| at.elapsed() >= FACTS_FRESH)
            .then_some(asked)
    }

    /// A call may have changed a sign-in (an account added, removed, or a sign-in ended): the next refresh reads them.
    pub(crate) fn forget_signed(&self) {
        let mut signed_at = self.signed_at.lock().unwrap();
        *signed_at = (None, signed_at.1 + 1);
    }

    /// Whether this caller runs the refresh: false while one runs, which then goes round once more.
    pub(crate) fn begin_refresh(&self) -> bool {
        let mut refreshing = self.refreshing.lock().unwrap();
        if refreshing.0 {
            refreshing.1 = true;
            return false;
        }
        *refreshing = (true, false);
        true
    }

    /// The refresh's round is over: whether a call asked for another, which the caller then runs.
    pub(crate) fn end_refresh(&self) -> bool {
        let mut refreshing = self.refreshing.lock().unwrap();
        if refreshing.1 {
            refreshing.1 = false;
            return true;
        }
        refreshing.0 = false;
        false
    }

    /// What is held for `key`, its facts and its sign-in, goes (its sign-in changed, or it was removed).
    pub(crate) fn forget(&self, key: &str) {
        self.known.lock().unwrap().remove(key);
        self.signed.lock().unwrap().remove(key);
        if self.reading.lock().unwrap().contains(key) {
            self.dropped.lock().unwrap().insert(key.to_string());
        }
    }

    /// What `listing` ([`with_signed_in`]'s, begun at `asked`, [`Facts::signed_stale`]) says of each account's sign-in,
    /// kept for the answers that follow; nothing is kept if a call has asked for another read since it began.
    pub(crate) fn keep_signed(&self, listing: &protocol::ws::ServerMessage, asked: u64) {
        if let protocol::ws::ServerMessage::Accounts { accounts, .. } = listing {
            let mut signed_at = self.signed_at.lock().unwrap();
            // A call asked for another read since this one began: what it found may be older than that call.
            if signed_at.1 != asked {
                return;
            }
            let mut signed = self.signed.lock().unwrap();
            for account in accounts {
                signed.insert(
                    account.id.clone(),
                    (account.signed_in, account.not_separate),
                );
            }
            signed_at.0 = Some(std::time::Instant::now());
        }
    }

    /// `reply`, an accounts listing, with what is held: each account's sign-in as last read, each account's facts,
    /// each agent's Default's, and an account marked `same_as_another` when another account of its agent, or its
    /// Default, has its email.
    pub(crate) fn told(
        &self,
        mut reply: protocol::ws::ServerMessage,
    ) -> protocol::ws::ServerMessage {
        let protocol::ws::ServerMessage::Accounts {
            accounts, defaults, ..
        } = &mut reply
        else {
            return reply;
        };
        let signed = self.signed.lock().unwrap();
        let known = self.known.lock().unwrap();
        for account in accounts.iter_mut() {
            if let Some(&(signed_in, not_separate)) = signed.get(&account.id) {
                (account.signed_in, account.not_separate) = (signed_in, not_separate);
            }
            account.facts = known.get(&account.id).and_then(|(facts, _)| facts.clone());
        }
        for agent in [AgentKind::Codex, AgentKind::Claude] {
            let default = known
                .get(&default_key(&agent))
                .and_then(|(facts, _)| facts.clone());
            // Two of one agent's accounts, its Default among them, signed in as one email are one account.
            let emails: Vec<String> = accounts
                .iter()
                .filter(|account| account.agent == agent)
                .filter_map(|account| account.facts.as_ref()?.email.clone())
                .chain(default.as_ref().and_then(|facts| facts.email.clone()))
                .collect();
            for account in accounts.iter_mut().filter(|account| account.agent == agent) {
                account.same_as_another = account
                    .facts
                    .as_ref()
                    .and_then(|facts| facts.email.as_ref())
                    .is_some_and(|email| emails.iter().filter(|other| *other == email).count() > 1);
            }
            if let Some(facts) = default {
                defaults.push(protocol::ws::DefaultAccount { agent, facts });
            }
        }
        reply
    }
}

/// What Claude Code at `claude` says of the account whose folder `account` gives (its Default, the person's own,
/// for none): its email, and its plan when it names one, from `auth status` with the folder as its config directory
/// (an account's sign-in keeps its identity there). None when it is not signed in, or did not answer within
/// [`FACTS_WAIT`]. Its usage is not read here.
pub(crate) async fn read_claude_facts(
    environment: &HashMap<OsString, OsString>,
    account: Option<&AccountEnv>,
    claude: &Path,
) -> Option<protocol::ws::AccountFacts> {
    let signing = account.map(AccountEnv::signing);
    let mut command = crate::start_session::agent_command(
        environment,
        signing.as_ref(),
        &[
            claude.as_os_str(),
            std::ffi::OsStr::new("auth"),
            std::ffi::OsStr::new("status"),
        ],
    );
    command
        .current_dir(protocol::home_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let output = crate::start_session::agent_output(command, FACTS_WAIT).await?;
    let status = crate::start_session::auth_status_object(&output.stdout)?;
    if status["loggedIn"] != true {
        return None;
    }
    Some(protocol::ws::AccountFacts {
        limits_available: None,
        email: status["email"].as_str().map(str::to_string),
        plan: status["subscriptionType"].as_str().map(str::to_string),
        windows: Vec::new(),
        read_at: protocol::time::now_rfc3339(),
    })
}

/// What Claude Code at `claude` says of its plan's limits for the account whose folder `account` gives (its Default for
/// none), asked through the agent host's short `--usage` launch (agent-host/src/usage.ts) with the account's variables:
/// whether plan limits apply, its plan, and each plan-wide window it names (five hours, seven days). None when the
/// host did not answer within [`FACTS_WAIT`].
pub(crate) async fn read_claude_usage(
    environment: &HashMap<OsString, OsString>,
    account: Option<&AccountEnv>,
    claude: &Path,
    host: &Path,
) -> Option<(bool, Option<String>, Vec<protocol::ws::UsageWindow>)> {
    let signing = account.map(AccountEnv::signing);
    let mut command = crate::start_session::agent_command(
        environment,
        signing.as_ref(),
        &[
            host.as_os_str(),
            std::ffi::OsStr::new("--usage"),
            claude.as_os_str(),
        ],
    );
    command
        .current_dir(protocol::home_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let output = crate::start_session::agent_output(command, FACTS_WAIT).await?;
    // The host's one line, after whatever the user's shell printed as it started.
    let text = String::from_utf8_lossy(&output.stdout);
    let said: serde_json::Value = serde_json::from_str(text.lines().last()?).ok()?;
    let windows = said["windows"]
        .as_array()?
        .iter()
        .filter_map(|window| {
            let minutes = match window["name"].as_str()? {
                "five_hour" => 5 * 60,
                "seven_day" => 7 * 24 * 60,
                _ => return None,
            };
            Some(protocol::ws::UsageWindow {
                used_percent: window["utilization"].as_f64()?.round().clamp(0.0, 100.0) as u32,
                window_minutes: minutes,
                resets_at: window["resets_at"].as_i64()?,
            })
        })
        .collect();
    Some((
        said["available"].as_bool()?,
        said["plan"].as_str().map(str::to_string),
        windows,
    ))
}

/// What the codex at `codex` says of the account whose home `account` gives (its Default for none): its email and
/// plan (`account/read`) and its usage windows (`account/rateLimits/read`), asked of an app-server started in that
/// home and ended once it has answered. None when it could not be asked or did not answer within [`FACTS_WAIT`].
pub(crate) async fn read_facts(
    environment: &HashMap<OsString, OsString>,
    account: Option<&AccountEnv>,
    codex: &Path,
) -> Option<protocol::ws::AccountFacts> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let asking = concat!(
        r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"codeconnect-ccd","title":"CodeConnect daemon","version":"0"}}}"#,
        "\n",
        r#"{"method":"initialized"}"#,
        "\n",
        r#"{"id":2,"method":"account/read","params":{}}"#,
        "\n",
        r#"{"id":3,"method":"account/rateLimits/read","params":{}}"#,
        "\n",
    );
    let mut command = crate::start_session::agent_command(
        environment,
        account,
        &[codex.as_os_str(), std::ffi::OsStr::new("app-server")],
    );
    command
        .current_dir(protocol::home_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().ok()?;
    let asked = async {
        let mut stdin = child.stdin.take()?;
        stdin.write_all(asking.as_bytes()).await.ok()?;
        let mut lines = tokio::io::BufReader::new(child.stdout.take()?).lines();
        let (mut said_account, mut said_limits) = (None, None);
        while said_account.is_none() || said_limits.is_none() {
            let line = lines.next_line().await.ok()??;
            // What the user's shell prints as it starts is not the agent's.
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            // An answer is its result; a refusal or a null is no facts.
            if !value["result"].is_object() {
                if matches!(value["id"].as_u64(), Some(2 | 3)) {
                    return None;
                }
                continue;
            }
            match value["id"].as_u64() {
                Some(2) => said_account = Some(value["result"].clone()),
                Some(3) => said_limits = Some(value["result"].clone()),
                _ => {}
            }
        }
        drop(stdin);
        let (account, limits) = (said_account?, said_limits?);
        let window = |window: &serde_json::Value| {
            Some(protocol::ws::UsageWindow {
                used_percent: u32::try_from(window["usedPercent"].as_u64()?).ok()?,
                window_minutes: window["windowDurationMins"].as_u64()?,
                resets_at: window["resetsAt"].as_i64()?,
            })
        };
        Some(protocol::ws::AccountFacts {
            limits_available: None,
            email: account["account"]["email"].as_str().map(str::to_string),
            plan: account["account"]["planType"].as_str().map(str::to_string),
            windows: ["primary", "secondary"]
                .iter()
                .filter_map(|which| window(&limits["rateLimits"][which]))
                .collect(),
            read_at: protocol::time::now_rfc3339(),
        })
    };
    let facts = tokio::time::timeout(FACTS_WAIT, asked).await.ok().flatten();
    // A signed-in app-server keeps running: it is ended, with what it started, answered or not.
    crate::start_session::end_agent(&mut child).await;
    facts
}

/// How a sign-in going is ended: it is sent where to say its login's group is gone.
type Ending = tokio::sync::oneshot::Sender<tokio::sync::oneshot::Sender<()>>;

/// The sign-in a computer is running, by its number, and how to end it. One at a time: the agent's callback is one port.
#[derive(Default)]
pub(crate) struct SignIns {
    /// The one going: its number, its account, and how to end it.
    going: std::sync::Mutex<Option<(u64, String, Ending)>>,
    count: std::sync::atomic::AtomicU64,
}

/// How long a sign-in may wait for the person in the browser before its login is ended.
pub(crate) const SIGN_IN_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// `reply`, an accounts listing, with each account's `signed_in` as its agent says with its home: Codex's
/// `login status`, and `not_separate` when that sign-in is not in the home; Claude Code's `auth status`, its config
/// directory the account's folder. Not known for an agent not on this computer.
pub(crate) async fn with_signed_in(
    environment: &HashMap<OsString, OsString>,
    agents: Agents<'_>,
    mut reply: protocol::ws::ServerMessage,
) -> protocol::ws::ServerMessage {
    let protocol::ws::ServerMessage::Accounts { accounts, .. } = &mut reply else {
        return reply;
    };
    let root = protocol::root_dir();
    // Every account asked at once: each ask is a login shell and the agent's own start.
    let asked = accounts.iter_mut().map(|account| {
        let row = Account {
            id: account.id.clone(),
            agent: account.agent.as_str().to_string(),
            label: account.label.clone(),
        };
        let root = &root;
        async move {
            let (Some(program), Ok(Some(home))) =
                (agents.path(&row.agent), AccountEnv::for_account(root, &row))
            else {
                return;
            };
            if row.agent == "claude" {
                account.signed_in = crate::start_session::claude_signed_in(
                    environment,
                    Some(&home.signing()),
                    program,
                )
                .await;
                return;
            }
            let signed_in =
                crate::start_session::codex_signed_in(environment, Some(&home), program).await;
            // Signed in with no credential in the account's own home: Codex keeps it in the computer's
            // credential store, shared, so this account is not separate.
            account.not_separate = signed_in && !home.folder().join("auth.json").exists();
            account.signed_in = Some(signed_in && !account.not_separate);
        }
    });
    futures_util::future::join_all(asked).await;
    reply
}

/// Signs `account_id` in with its agent's own login on this computer, `codex` being Codex's path here. Answers at
/// once: the listing, that account `signing_in`. The login runs on in a group of its own; when it ends (finished in
/// the browser, after `wait`, or replaced by a newer sign-in), its group is ended and `on_end` called, which pushes
/// the list. The handle is the login's, for a test to wait on.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn sign_in(
    store: &crate::store::Store,
    sign_ins: &Arc<SignIns>,
    on_end: impl FnOnce() + Send + 'static,
    environment: &HashMap<OsString, OsString>,
    agents: Agents<'_>,
    request_id: String,
    account_id: String,
    wait: std::time::Duration,
) -> (
    protocol::ws::ServerMessage,
    Option<tokio::task::JoinHandle<()>>,
) {
    let account = match store.accounts() {
        Ok(accounts) => accounts
            .into_iter()
            .find(|account| account.id == account_id),
        Err(err) => return (unavailable(request_id, "read the accounts", err), None),
    };
    let Some(account) = account else {
        return (
            refused(
                request_id,
                "that is not one of this computer's accounts".into(),
            ),
            None,
        );
    };
    let Some(program) = agents.path(&account.agent) else {
        let missing = format!(
            "{} isn't installed on this computer",
            product(&account.agent)
        );
        return (refused(request_id, missing), None);
    };
    let home = match AccountEnv::for_account(&protocol::root_dir(), &account) {
        Ok(Some(home)) => home.signing(),
        Ok(None) => {
            return (
                refused(
                    request_id,
                    "this account's agent has no sign-in here".into(),
                ),
                None,
            )
        }
        Err(err) => {
            return (
                unavailable(request_id, "make the account's home", err.into()),
                None,
            )
        }
    };
    // In a group of its own, so ending it ends what it started ([`crate::start_session::agent_command`]).
    let mut login = crate::start_session::agent_command(
        environment,
        Some(&home),
        &login_args(&account.agent, program, "login"),
    );
    login
        .current_dir(protocol::home_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let (end, ended) = tokio::sync::oneshot::channel::<tokio::sync::oneshot::Sender<()>>();
    let mine = sign_ins
        .count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some((_, _, earlier)) =
        sign_ins
            .going
            .lock()
            .unwrap()
            .replace((mine, account.id.clone(), end))
    {
        let _ = earlier.send(tokio::sync::oneshot::channel().0);
    }
    // The answer is read before the login starts, so it says this account is signing in, and the push at the
    // login's end is the newer.
    let reply = sign_ins.marked(listing(store, Some(request_id.clone())));
    let mut child = match login.spawn() {
        Ok(child) => child,
        Err(err) => {
            sign_ins.forget(mine);
            return (
                unavailable(request_id, "start the sign-in", err.into()),
                None,
            );
        }
    };
    let running = {
        let sign_ins = Arc::clone(sign_ins);
        tokio::spawn(async move {
            let mut told = None;
            tokio::select! {
                _ = child.wait() => {}
                ending = ended => told = ending.ok(),
                _ = tokio::time::sleep(wait) => {}
            }
            // Its group ended as a run host's is ([`crate::start_session::end_agent`]).
            crate::start_session::end_agent(&mut child).await;
            sign_ins.forget(mine);
            if let Some(told) = told {
                let _ = told.send(());
            }
            on_end();
        })
    };
    (reply, Some(running))
}

impl SignIns {
    /// The sign-in numbered `mine` is over: it is no longer the one going.
    fn forget(&self, mine: u64) {
        let mut going = self.going.lock().unwrap();
        if going.as_ref().is_some_and(|(number, _, _)| *number == mine) {
            *going = None;
        }
    }

    /// `reply`, an accounts listing, with the account a sign-in is going for marked `signing_in`.
    pub(crate) fn marked(
        &self,
        mut reply: protocol::ws::ServerMessage,
    ) -> protocol::ws::ServerMessage {
        let going = self
            .going
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, account, _)| account.clone());
        if let (protocol::ws::ServerMessage::Accounts { accounts, .. }, Some(going)) =
            (&mut reply, going)
        {
            for account in accounts.iter_mut().filter(|account| account.id == going) {
                account.signing_in = true;
            }
        }
        reply
    }

    /// Ends the sign-in going for `account_id`, if one is, and waits until its login's group is gone.
    async fn end_for(&self, account_id: &str) {
        let going = {
            let mut going = self.going.lock().unwrap();
            match going.as_ref() {
                Some((_, account, _)) if account == account_id => going.take(),
                _ => None,
            }
        };
        if let Some((_, _, end)) = going {
            let (told, gone) = tokio::sync::oneshot::channel();
            if end.send(told).is_ok() {
                let _ = gone.await;
            }
        }
    }
}

/// How long an agent's logout may take before the removal is refused.
const LOGOUT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The folder of the removed Claude Code account `id` deleted (`root`'s `accounts/claude/<id>`, the id ccd's own):
/// nothing of a removed account is left. Only a real folder is deleted; one that is a link is left and logged. The
/// deletion does not follow a link inside it (`std::fs::remove_dir_all` removes a link, never what it names). A
/// failure is logged; the account is removed all the same.
fn remove_claude_folder(root: &Path, id: &str) {
    let folder = root.join("accounts").join("claude").join(id);
    match folder.symlink_metadata() {
        Ok(found) if found.is_dir() => {
            if let Err(err) = std::fs::remove_dir_all(&folder) {
                crate::log_error!(
                    "could not delete the folder of removed account {id} ({}): {err}",
                    folder.display()
                );
            }
        }
        Ok(_) => crate::log_error!("left the folder of removed account {id}: it is not a folder"),
        Err(_) => {}
    }
}

/// Removes `account_id`: its agent's own logout in its home, `codex` being Codex's path here; once the credential is
/// gone, the row (and any choice of it). The home stays, its links and nothing of the account's: a thread started
/// on the account is recorded through it and still resumes from another account (measured on codex 0.170.0,
/// 2026-11-22). Refused, with nothing removed, when the credential is still there after the logout.
pub(crate) async fn remove_with(
    store: &crate::store::Store,
    sign_ins: &SignIns,
    environment: &HashMap<OsString, OsString>,
    agents: Agents<'_>,
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
    match store.account_in_use(&account.id) {
        Ok(false) => {}
        Ok(true) => {
            return refused_as(
                request_id,
                "accounts_in_use",
                "Sessions are running on this account. Choose another account first.".into(),
            )
        }
        Err(err) => return unavailable(request_id, "read the account's sessions", err),
    }
    // A sign-in held for it is ended first, so no credential lands in its home after it is gone.
    sign_ins.end_for(&account.id).await;
    let Some(program) = agents.path(&account.agent) else {
        let missing = format!(
            "{} isn't installed on this computer",
            product(&account.agent)
        );
        return refused(request_id, missing);
    };
    let home = match AccountEnv::for_account(&protocol::root_dir(), &account) {
        Ok(Some(home)) => home.signing(),
        Ok(None) => {
            return refused(
                request_id,
                "this account's agent has no sign-in here".into(),
            )
        }
        Err(err) => return unavailable(request_id, "find the account's home", err.into()),
    };
    let mut logout = crate::start_session::agent_command(
        environment,
        Some(&home),
        &login_args(&account.agent, program, "logout"),
    );
    logout
        .current_dir(protocol::home_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _ = crate::start_session::agent_output(logout, LOGOUT_WAIT).await;
    // Signed out: Codex's credential gone from the home; Claude Code's own status says so with the folder.
    let signed_out = if account.agent == "claude" {
        crate::start_session::claude_signed_in(environment, Some(&home), program).await
            == Some(false)
    } else {
        home.folder().join("auth.json").symlink_metadata().is_err()
    };
    if !signed_out {
        return refused_as(
            request_id,
            "accounts_unavailable",
            format!(
                "{} could not sign the account out, so nothing was removed",
                product(&account.agent)
            ),
        );
    }
    if let Err(err) = store.remove_account(&account.id) {
        return unavailable(request_id, "remove the account", err);
    }
    // Signed out and its row gone: Claude Code's folder goes too. Codex's home is kept, as links its state records.
    if account.agent == "claude" {
        remove_claude_folder(&protocol::root_dir(), &account.id);
    }
    // The remaining accounts' sign-ins are what is held; the read behind the answer asks again.
    listing(store, Some(request_id))
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
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::Arc;

    /// A stand-in codex in `dir`: `login` writes `auth.json` into its `CODEX_HOME`, or, while `dir/hang` exists, waits
    /// as the npm launcher does: its native login a child (pid in `dir/kid`), passed SIGTERM and nothing else (its own
    /// pid in `dir/pid`); `login status` succeeds once that home has one, after 3 s while `dir/slowstatus` exists.
    /// Each run's first two words go into `dir/runs`. Its app-server names an account's email only once that home
    /// is signed in (the Default's always).
    pub(crate) fn stand_in_codex(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("codex");
        std::fs::write(
            &path,
            "#!/bin/sh\nhere=$(dirname \"$0\")\necho \"$1 $2\" >> \"$here/runs\"\nif [ \"$1\" = login ] && [ \"$2\" = status ]; then [ -e \"$here/slowstatus\" ] && sleep 3; [ -f \"$CODEX_HOME/auth.json\" ] || [ -f \"$here/keyring-login\" ]; exit $?; fi\n\
             if [ \"$1\" = login ] && [ -e \"$here/keyring\" ]; then echo '{}' > \"$here/keyring-login\"; exit 0; fi\n\
             if [ \"$1\" = login ]; then if [ -e \"$here/hang\" ]; then sleep 30 & kid=$!; echo $kid > \"$here/kid\"; echo $$ > \"$here/pid\"; trap 'kill -TERM $kid; exit 0' TERM; wait; fi; echo '{}' > \"$CODEX_HOME/auth.json\"; fi\n\
             if [ \"$1\" = app-server ] && [ -e \"$here/hangfacts\" ]; then sleep 120 & kid=$!; echo $kid > \"$here/kid\"; wait; fi\n\
             if [ \"$1\" = app-server ]; then n=$(basename \"${CODEX_HOME:-default}\"); while IFS= read -r line; do case \"$line\" in *'\"id\":1,'*) printf '{\"id\":1,\"result\":{\"userAgent\":\"stand-in\"}}\\n';; *account/rateLimits/read*) printf '{\"id\":3,\"result\":{\"rateLimits\":{\"primary\":{\"usedPercent\":42,\"windowDurationMins\":300,\"resetsAt\":1800000000},\"secondary\":null,\"planType\":\"plus\"}}}\\n';; *account/read*) [ -e \"$here/same\" ] && n=same; if [ -n \"$CODEX_HOME\" ] && [ ! -f \"$CODEX_HOME/auth.json\" ] && [ ! -e \"$here/keyring-login\" ]; then printf '{\"id\":2,\"result\":{\"account\":null,\"requiresOpenaiAuth\":true}}\\n'; else printf '{\"id\":2,\"result\":{\"account\":{\"email\":\"%s@example.test\",\"planType\":\"plus\",\"type\":\"chatgpt\",\"workspaceName\":null},\"requiresOpenaiAuth\":true}}\\n' \"$n\"; fi;; esac; done; exit 0; fi\n\
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

    /// A stand-in claude in `dir`, shaped from real claude 2.1.330 (app-qa's records, 2026-11-22): `auth login`
    /// writes the account's identity where its config directory points and a keychain item named from its storage
    /// folder (a file `item-<folder>` here), prints "Login successful" and exits 0; `auth status` prints one JSON
    /// object, pretty-printed over several lines as the real one is, with the real key list (signed out: analyticsDisabled, apiProvider, authMethod, configDirectory,
    /// consoleProfileSignInOffered, loggedIn, loginExpiresAt null, projectsDirectory; signed in: those, plus email (the
    /// identity its config directory holds, the person's shared one without it), orgId, orgName, planTier,
    /// storedClaudeAiLogin and subscriptionType, all made up but the email) and exits 0 signed in, 1 signed out; `auth logout` takes the item and exits 0. A login
    /// run without a config directory of its own writes the person's shared identity, and says so in `shared`.
    pub(crate) fn stand_in_claude(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("claude");
        std::fs::write(
            &path,
            "#!/bin/sh\nhere=$(dirname \"$0\")\nn=$(basename \"${CLAUDE_SECURESTORAGE_CONFIG_DIR:-default}\")\n\
             if [ \"$1\" = auth ] && [ \"$2\" = login ]; then if [ -n \"$CLAUDE_CONFIG_DIR\" ]; then echo \"$n@example.test\" > \"$CLAUDE_CONFIG_DIR/.claude.json\"; else echo \"$n@example.test\" > \"$here/shared-identity\"; echo \"$n\" >> \"$here/shared\"; fi; : > \"$here/item-$n\"; echo 'Login successful'; exit 0; fi\n\
             if [ \"$1\" = auth ] && [ \"$2\" = status ]; then if [ -n \"$CLAUDE_CONFIG_DIR\" ]; then e=$(cat \"$CLAUDE_CONFIG_DIR/.claude.json\" 2>/dev/null); else e=$(cat \"$here/shared-identity\" 2>/dev/null || echo default@example.test); fi; if [ -e \"$here/item-$n\" ]; then printf '{\\n  \"loggedIn\": true,\\n  \"authMethod\": \"claude.ai\",\\n  \"apiProvider\": \"firstParty\",\\n  \"email\": \"%s\",\\n  \"orgId\": \"org-made-up\",\\n  \"orgName\": \"Made Up Org\",\\n  \"planTier\": \"made-up-tier\",\\n  \"subscriptionType\": \"max\",\\n  \"storedClaudeAiLogin\": true,\\n  \"loginExpiresAt\": \"2026-12-01T00:00:00Z\",\\n  \"consoleProfileSignInOffered\": false,\\n  \"analyticsDisabled\": false,\\n  \"projectsDirectory\": \"/p\",\\n  \"configDirectory\": \"%s\"\\n}\\n' \"$e\" \"${CLAUDE_CONFIG_DIR:-}\"; exit 0; fi; printf '{\\n  \"loggedIn\": false,\\n  \"authMethod\": \"none\",\\n  \"apiProvider\": \"firstParty\",\\n  \"loginExpiresAt\": null,\\n  \"consoleProfileSignInOffered\": true,\\n  \"analyticsDisabled\": false,\\n  \"projectsDirectory\": \"/p\",\\n  \"configDirectory\": \"%s\"\\n}\\n' \"${CLAUDE_CONFIG_DIR:-}\"; exit 1; fi\n\
             if [ \"$1\" = auth ] && [ \"$2\" = logout ]; then rm -f \"$here/item-$n\"; exit 0; fi\nexit 2\n",
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
        let sign_ins = Arc::new(super::SignIns::default());
        let (changed, heard) = std::sync::mpsc::channel::<()>();
        let (answered, running) = super::sign_in(
            &store,
            &sign_ins,
            {
                let changed = changed.clone();
                move || {
                    let _ = changed.send(());
                }
            },
            &environment,
            super::Agents {
                codex: Some(&codex),
                claude: None,
            },
            "s1".into(),
            a.id.clone(),
            std::time::Duration::from_secs(20),
        )
        .await;
        let answered = serde_json::to_value(&answered).unwrap();
        let row_of = |reply: &serde_json::Value, id: &str| {
            reply["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(answered["request_id"], "s1", "answered at once: {answered}");
        assert_eq!(row_of(&answered, &a.id)["signing_in"], true, "{answered}");
        running.unwrap().await.unwrap();
        assert!(
            heard.try_recv().is_ok(),
            "the list is pushed when the login ends"
        );
        let reply = serde_json::to_value(
            sign_ins.marked(
                super::with_signed_in(
                    &environment,
                    super::Agents {
                        codex: Some(&codex),
                        claude: None,
                    },
                    super::listing(&store, None),
                )
                .await,
            ),
        )
        .unwrap();
        assert_eq!(
            row_of(&reply, &a.id).get("signing_in"),
            None,
            "no longer signing in: {reply}"
        );
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

    /// **What a Codex account says of itself is read in its own home, shown, and never stored** (306 Slices C and F):
    /// email and plan from `account/read`, each usage window `account/rateLimits/read` names (an unnamed one absent),
    /// with when it was read; the Default's too, from the person's own home; two accounts signed in as one email are
    /// marked; and none of it reaches the store.
    #[tokio::test]
    async fn what_a_codex_account_says_of_itself_is_shown_and_never_stored() {
        let _env = crate::ProcessEnvRead::take();
        let root = std::env::temp_dir().join(format!(
            "ccs-facts.{}",
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
        let store = crate::store::Store::open_temp("ccd-facts");
        let db = store.temp_db();
        let a = store.add_account("codex", "A").unwrap();
        let on_a = super::AccountEnv::for_account(&protocol::root_dir(), &a)
            .unwrap()
            .unwrap();
        // Signed in, as its sign-in leaves it: the stand-in names an account's email only then.
        std::fs::create_dir_all(on_a.folder()).unwrap();
        std::fs::write(on_a.folder().join("auth.json"), "{}").unwrap();
        let facts = super::Facts::default();
        let mut keys = vec![
            a.id.clone(),
            super::default_key(&protocol::agent::AgentKind::Codex),
        ];
        assert_eq!(facts.claim_stale(&keys), keys, "nothing held: both read");
        assert_eq!(
            facts.claim_stale(&keys),
            Vec::<String>::new(),
            "one read at a time per account"
        );
        facts.read(
            &a.id,
            super::read_facts(&environment, Some(&on_a), &codex).await,
        );
        facts.read(
            &super::default_key(&protocol::agent::AgentKind::Codex),
            super::read_facts(&environment, None, &codex).await,
        );
        assert_eq!(
            facts.claim_stale(&keys),
            Vec::<String>::new(),
            "fresh: not read again"
        );
        let told = serde_json::to_value(facts.told(super::listing(&store, None))).unwrap();
        let row = &told["accounts"][0];
        assert_eq!(
            row["facts"]["email"],
            format!("{}@example.test", a.id),
            "{told}"
        );
        assert_eq!(row["facts"]["plan"], "plus");
        assert_eq!(
            row["facts"]["windows"],
            serde_json::json!([{"used_percent": 42, "window_minutes": 300, "resets_at": 1800000000}]),
            "the one window Codex names; the other absent"
        );
        assert!(row["facts"]["read_at"].is_string());
        assert_eq!(row.get("same_as_another"), None);
        assert_eq!(
            told["defaults"][0]["facts"]["email"], "default@example.test",
            "{told}"
        );
        // Two accounts signed in as one email are marked.
        std::fs::write(root.join("same"), "").unwrap();
        let b = store.add_account("codex", "B").unwrap();
        let on_b = super::AccountEnv::for_account(&protocol::root_dir(), &b)
            .unwrap()
            .unwrap();
        // Signed in, as its sign-in leaves it: the stand-in names an account's email only then.
        std::fs::create_dir_all(on_b.folder()).unwrap();
        std::fs::write(on_b.folder().join("auth.json"), "{}").unwrap();
        facts.forget(&a.id);
        keys.push(b.id.clone());
        for key in facts.claim_stale(&keys) {
            let home = if key == a.id {
                Some(&on_a)
            } else {
                Some(&on_b)
            };
            facts.read(&key, super::read_facts(&environment, home, &codex).await);
        }
        let told = serde_json::to_value(facts.told(super::listing(&store, None))).unwrap();
        assert_eq!(
            (
                told["accounts"][0]["same_as_another"].as_bool(),
                told["accounts"][1]["same_as_another"].as_bool()
            ),
            (Some(true), Some(true)),
            "{told}"
        );
        crate::switch::window::keep(std::slice::from_ref(&told), "accounts-facts-wire.json");
        // Held in memory only: not a byte of it in the store.
        drop(store);
        for file in [db.path().to_path_buf(), db.path().with_extension("db-wal")] {
            if let Ok(bytes) = std::fs::read(&file) {
                assert!(
                    !bytes
                        .windows(b"example.test".len())
                        .any(|window| window == b"example.test"),
                    "{} holds an email",
                    file.display()
                );
            }
        }
        for id in [&a.id, &b.id] {
            let _ = std::fs::remove_dir_all(
                protocol::root_dir().join("accounts").join("codex").join(id),
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A facts read that hangs leaves nothing running** (the agent-launch class): an app-server that starts a child
    /// of its own, as Codex's npm launcher does, and never answers is ended at the read's bound with what it started.
    #[tokio::test]
    async fn a_facts_read_that_hangs_leaves_nothing_running() {
        let _env = crate::ProcessEnvRead::take();
        let root =
            std::env::temp_dir().join(format!("ccs-hang.{}", &protocol::uid::new().unwrap()[20..]));
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        let codex = stand_in_codex(&root);
        std::fs::write(root.join("hangfacts"), "").unwrap();
        let environment: HashMap<OsString, OsString> = HashMap::from([
            (OsString::from("HOME"), home.clone().into_os_string()),
            (OsString::from("SHELL"), OsString::from("/bin/sh")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]);
        assert_eq!(
            super::read_facts(&environment, None, &codex).await,
            None,
            "no answer, no facts"
        );
        let kid: i32 = std::fs::read_to_string(root.join("kid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while unsafe { libc::kill(kid, 0) } == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the app-server's child {kid} was left running"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A Claude Code account signs in, is read and is removed in its own folder, never the person's identity**
    /// (306 Slices B, C and D for Claude; measured on claude 2.1.330, 2026-11-22): its `auth login` runs with both the
    /// storage variable and the config directory set to the account's folder, so the person's shared identity is never
    /// written; its status reads signed in; its email is read from that folder; the Default's is the person's own; and
    /// its removal signs it out with the same two variables and takes the row.
    #[tokio::test]
    async fn a_claude_account_lives_in_its_own_folder() {
        let _env = crate::ProcessEnvRead::take();
        let root = std::env::temp_dir().join(format!(
            "ccs-claude.{}",
            &protocol::uid::new().unwrap()[20..]
        ));
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let claude = stand_in_claude(&root);
        let agents = super::Agents {
            codex: None,
            claude: Some(&claude),
        };
        let environment: HashMap<OsString, OsString> = HashMap::from([
            (OsString::from("HOME"), home.clone().into_os_string()),
            (OsString::from("SHELL"), OsString::from("/bin/sh")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]);
        let store = crate::store::Store::open_temp("ccd-claude-acct");
        let _db = store.temp_db();
        let a = store.add_account("claude", "A").unwrap();
        let folder = protocol::root_dir()
            .join("accounts")
            .join("claude")
            .join(&a.id);
        let (answered, running) = super::sign_in(
            &store,
            &Arc::new(super::SignIns::default()),
            || {},
            &environment,
            agents,
            "s1".into(),
            a.id.clone(),
            std::time::Duration::from_secs(20),
        )
        .await;
        assert_eq!(
            serde_json::to_value(&answered).unwrap()["accounts"][0]["signing_in"],
            true,
            "{answered:?}"
        );
        running.unwrap().await.unwrap();
        assert!(
            !root.join("shared").exists(),
            "the person's shared identity was written"
        );
        assert!(
            folder.join(".claude.json").exists(),
            "the account's identity is in its own folder"
        );
        let listed = serde_json::to_value(
            super::with_signed_in(&environment, agents, super::listing(&store, None)).await,
        )
        .unwrap();
        assert_eq!(listed["accounts"][0]["signed_in"], true, "{listed}");
        // Its email, and the Default's, the person's own.
        let on_a = super::AccountEnv::for_account(&protocol::root_dir(), &a)
            .unwrap()
            .unwrap();
        let facts = super::read_claude_facts(&environment, Some(&on_a), &claude)
            .await
            .unwrap();
        // Only the email and the plan are kept: the organisation, its tier and the rest never leave the read.
        let mut kept = serde_json::to_value(&facts).unwrap();
        kept.as_object_mut().unwrap().remove("read_at");
        assert_eq!(
            kept,
            serde_json::json!({"email": format!("{}@example.test", a.id), "plan": "max"})
        );
        std::fs::write(root.join("item-default"), "").unwrap();
        let default = super::read_claude_facts(&environment, None, &claude).await;
        assert_eq!(
            default.and_then(|facts| facts.email).as_deref(),
            Some("default@example.test")
        );
        // A link inside the folder to a place outside it, which its removal must not follow.
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("kept"), "the person's").unwrap();
        std::os::unix::fs::symlink(&outside, folder.join("link-out")).unwrap();
        // The Default has no row, so no id reaches a folder outside an added account's.
        let refused = serde_json::to_value(
            super::remove_with(
                &store,
                &super::SignIns::default(),
                &environment,
                agents,
                "r0".into(),
                super::default_key(&protocol::agent::AgentKind::Claude),
            )
            .await,
        )
        .unwrap();
        assert_eq!(refused["type"], "accounts_refused", "{refused}");
        // Removed: signed out with the same two variables, the row gone.
        let removed = serde_json::to_value(
            super::remove_with(
                &store,
                &super::SignIns::default(),
                &environment,
                agents,
                "r1".into(),
                a.id.clone(),
            )
            .await,
        )
        .unwrap();
        assert_eq!(removed["accounts"], serde_json::json!([]), "{removed}");
        assert!(
            !root.join(format!("item-{}", a.id)).exists(),
            "its keychain item is gone"
        );
        assert!(
            root.join("item-default").exists(),
            "the person's own is untouched"
        );
        // Nothing of a removed account is left: its folder is gone, and nothing a link in it named.
        assert!(folder.symlink_metadata().is_err(), "its folder is gone");
        assert_eq!(
            std::fs::read_to_string(outside.join("kept")).unwrap(),
            "the person's"
        );
        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A Claude Code account's plan limits are read through the host's `--usage` launch** (306 Slice F) with the
    /// account's two variables: the plan-wide windows it names become usage windows (seven days as 10 080 minutes, the
    /// share rounded to a whole percent, the reset in Unix seconds), with whether plan limits apply and the plan; a line
    /// the user's shell printed first is passed over.
    #[tokio::test]
    async fn a_claude_accounts_plan_limits_are_read_through_the_host() {
        let _env = crate::ProcessEnvRead::take();
        let root = std::env::temp_dir().join(format!(
            "ccs-usage.{}",
            &protocol::uid::new().unwrap()[20..]
        ));
        std::fs::create_dir_all(root.join("home")).unwrap();
        let host = root.join("cc-agent-host");
        std::fs::write(
            &host,
            "#!/bin/sh\nprintf '%s %s %s\\n' \"$1\" \"$CLAUDE_SECURESTORAGE_CONFIG_DIR\" \"$CLAUDE_CONFIG_DIR\" > \"$(dirname \"$0\")/asked\"\n\
             echo 'a line from the shell'\n\
             echo '{\"available\":true,\"plan\":\"max\",\"windows\":[{\"name\":\"seven_day\",\"utilization\":41.6,\"resets_at\":1795928400}]}'\n",
        )
        .unwrap();
        std::process::Command::new("chmod")
            .arg("755")
            .arg(&host)
            .status()
            .unwrap();
        let environment: HashMap<OsString, OsString> = HashMap::from([
            (OsString::from("HOME"), root.join("home").into_os_string()),
            (OsString::from("SHELL"), OsString::from("/bin/sh")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]);
        let store = crate::store::Store::open_temp("ccd-claude-usage");
        let _db = store.temp_db();
        let a = store.add_account("claude", "A").unwrap();
        let on_a = super::AccountEnv::for_account(&protocol::root_dir(), &a)
            .unwrap()
            .unwrap();
        let read = super::read_claude_usage(
            &environment,
            Some(&on_a),
            Path::new("/stand-in/claude"),
            &host,
        )
        .await;
        assert_eq!(
            read,
            Some((
                true,
                Some("max".to_string()),
                vec![protocol::ws::UsageWindow {
                    used_percent: 42,
                    window_minutes: 7 * 24 * 60,
                    resets_at: 1_795_928_400
                }]
            ))
        );
        let folder = on_a.folder().display().to_string();
        assert_eq!(
            std::fs::read_to_string(root.join("asked")).unwrap().trim(),
            format!("--usage {folder} {folder}"),
            "asked with both of the account's variables"
        );
        let _ = std::fs::remove_dir_all(on_a.folder());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A sign-in read begun before a sign-in ended is never shown** (round 2): the read's answer, older than the
    /// end, is not written, and the account's own held answer went with the end, so its row says nothing of its
    /// sign-in until it is read again; no timing, the calls in the order the race makes them.
    #[test]
    fn a_sign_in_read_begun_before_a_sign_in_ended_is_never_shown() {
        let store = crate::store::Store::open_temp("ccd-signed-race");
        let _db = store.temp_db();
        let x = store.add_account("codex", "X").unwrap();
        let facts = super::Facts::default();
        let read_as = |signed_in: bool| {
            let mut listing = super::listing(&store, None);
            if let protocol::ws::ServerMessage::Accounts { accounts, .. } = &mut listing {
                accounts[0].signed_in = Some(signed_in);
            }
            listing
        };
        // An earlier round held X signed out.
        let first = facts.signed_stale().unwrap();
        facts.keep_signed(&read_as(false), first);
        // A round begins its read; X's login ends meanwhile; the round's read, from before the end, comes in.
        facts.forget_signed();
        let begun = facts.signed_stale().unwrap();
        facts.forget(&x.id);
        facts.forget_signed();
        facts.keep_signed(&read_as(false), begun);
        let told = serde_json::to_value(facts.told(super::listing(&store, None))).unwrap();
        assert_eq!(
            told["accounts"][0].get("signed_in"),
            None,
            "not read yet: {told}"
        );
        assert!(facts.signed_stale().is_some(), "read again");
    }

    /// **A removed Claude Code account's folder that is a link is left**, and what it names untouched: only a real
    /// folder directly under the accounts root is deleted.
    #[test]
    fn a_removed_accounts_folder_that_is_a_link_is_left() {
        let root =
            std::env::temp_dir().join(format!("ccs-link.{}", &protocol::uid::new().unwrap()[20..]));
        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("kept"), "x").unwrap();
        let id = protocol::uid::new().unwrap();
        std::fs::create_dir_all(root.join("accounts").join("claude")).unwrap();
        let folder = root.join("accounts").join("claude").join(&id);
        std::os::unix::fs::symlink(&target, &folder).unwrap();
        super::remove_claude_folder(&root, &id);
        assert!(
            folder.symlink_metadata().unwrap().file_type().is_symlink(),
            "the link is left"
        );
        assert!(target.join("kept").exists(), "what it names is untouched");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A sign-in Codex keeps in the computer's credential store is not separate** (306): with Codex storing its
    /// credential outside the account's home (the keyring), a sign-in that ends signed in leaves no auth.json in
    /// the home: the listing says `not_separate`, and not signed in.
    #[tokio::test]
    async fn a_sign_in_kept_outside_the_accounts_home_is_not_separate() {
        let _env = crate::ProcessEnvRead::take();
        let root = std::env::temp_dir().join(format!(
            "ccs-keyring.{}",
            &protocol::uid::new().unwrap()[20..]
        ));
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        let codex = stand_in_codex(&root);
        std::fs::write(root.join("keyring"), "").unwrap();
        let environment: HashMap<OsString, OsString> = HashMap::from([
            (OsString::from("HOME"), home.clone().into_os_string()),
            (OsString::from("SHELL"), OsString::from("/bin/sh")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ]);
        let store = crate::store::Store::open_temp("ccd-keyring");
        let _db = store.temp_db();
        let a = store.add_account("codex", "A").unwrap();
        let sign_ins = Arc::new(super::SignIns::default());
        let (_, running) = super::sign_in(
            &store,
            &sign_ins,
            || {},
            &environment,
            super::Agents {
                codex: Some(&codex),
                claude: None,
            },
            "s1".into(),
            a.id.clone(),
            std::time::Duration::from_secs(20),
        )
        .await;
        running.unwrap().await.unwrap();
        let reply = serde_json::to_value(
            super::with_signed_in(
                &environment,
                super::Agents {
                    codex: Some(&codex),
                    claude: None,
                },
                super::listing(&store, None),
            )
            .await,
        )
        .unwrap();
        assert_eq!(
            (
                reply["accounts"][0]["signed_in"].as_bool(),
                reply["accounts"][0]["not_separate"].as_bool()
            ),
            (Some(false), Some(true)),
            "{reply}"
        );
        let _ = std::fs::remove_dir_all(
            protocol::root_dir()
                .join("accounts")
                .join("codex")
                .join(&a.id),
        );
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
        let sign_ins = Arc::new(super::SignIns::default());
        let (changed, heard) = std::sync::mpsc::channel::<()>();
        let signed_in = || async {
            let reply = super::with_signed_in(
                &environment,
                super::Agents {
                    codex: Some(&codex),
                    claude: None,
                },
                super::listing(&store, None),
            )
            .await;
            serde_json::to_value(&reply).unwrap()["accounts"][0]["signed_in"].clone()
        };
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
        let (_, late) = super::sign_in(
            &store,
            &sign_ins,
            {
                let changed = changed.clone();
                move || {
                    let _ = changed.send(());
                }
            },
            &environment,
            super::Agents {
                codex: Some(&codex),
                claude: None,
            },
            "s1".into(),
            a.id.clone(),
            std::time::Duration::from_millis(600),
        )
        .await;
        late.unwrap().await.unwrap();
        assert!(
            heard.try_recv().is_ok(),
            "the list is pushed when the bound ends the login"
        );
        assert_eq!(
            signed_in().await,
            false,
            "a login ended by the bound signed nothing in"
        );
        let kid = || {
            std::fs::read_to_string(root.join("kid"))
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap()
        };
        assert!(gone(pid().await).await, "the timed-out login is ended");
        assert!(
            gone(kid()).await,
            "and the login it started, as a launcher's child"
        );
        std::fs::remove_file(root.join("pid")).unwrap();

        let (_, first) = super::sign_in(
            &store,
            &sign_ins,
            {
                let changed = changed.clone();
                move || {
                    let _ = changed.send(());
                }
            },
            &environment,
            super::Agents {
                codex: Some(&codex),
                claude: None,
            },
            "s2".into(),
            a.id.clone(),
            std::time::Duration::from_secs(20),
        )
        .await;
        let first_pid = pid().await;
        let first_kid = kid();
        std::fs::remove_file(root.join("hang")).unwrap();
        let (_, second) = super::sign_in(
            &store,
            &sign_ins,
            {
                let changed = changed.clone();
                move || {
                    let _ = changed.send(());
                }
            },
            &environment,
            super::Agents {
                codex: Some(&codex),
                claude: None,
            },
            "s3".into(),
            a.id.clone(),
            std::time::Duration::from_secs(20),
        )
        .await;
        first.unwrap().await.unwrap();
        assert!(
            gone(first_pid).await,
            "the first login is ended by the second sign-in"
        );
        assert!(gone(first_kid).await, "and the login it started");
        second.unwrap().await.unwrap();
        assert_eq!(
            signed_in().await,
            true,
            "the second sign-in's login signed A in"
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
        let (_, running) = super::sign_in(
            &store,
            &Arc::new(super::SignIns::default()),
            || {},
            &environment,
            super::Agents {
                codex: Some(&codex),
                claude: None,
            },
            "s1".into(),
            a.id.clone(),
            std::time::Duration::from_secs(20),
        )
        .await;
        running.unwrap().await.unwrap();
        assert!(folder.join("auth.json").exists(), "signed in");

        std::fs::write(root.join("keep"), "").unwrap();
        let kept = serde_json::to_value(
            super::remove_with(
                &store,
                &super::SignIns::default(),
                &environment,
                super::Agents {
                    codex: Some(&codex),
                    claude: None,
                },
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
            super::remove_with(
                &store,
                &super::SignIns::default(),
                &environment,
                super::Agents {
                    codex: Some(&codex),
                    claude: None,
                },
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

    /// **Removing an account ends its sign-in first** (306 Slice D, minor 1): a sign-in held for A, then A removed: by
    /// the time the removal answers, the login and the child it started are gone, so no credential can land in the
    /// kept home later.
    #[tokio::test]
    async fn removing_an_account_ends_its_sign_in_first() {
        let _env = crate::ProcessEnvRead::take();
        let root = std::env::temp_dir().join(format!(
            "ccs-remove2.{}",
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
        let store = crate::store::Store::open_temp("ccd-remove2");
        let _db = store.temp_db();
        let a = store.add_account("codex", "A").unwrap();
        let sign_ins = Arc::new(super::SignIns::default());
        std::fs::write(root.join("hang"), "").unwrap();
        let read = |name: &str| -> Option<i32> {
            std::fs::read_to_string(root.join(name))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
        let (_, running) = super::sign_in(
            &store,
            &sign_ins,
            || {},
            &environment,
            super::Agents {
                codex: Some(&codex),
                claude: None,
            },
            "s1".into(),
            a.id.clone(),
            std::time::Duration::from_secs(20),
        )
        .await;
        while read("kid").is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let removed = super::remove_with(
            &store,
            &sign_ins,
            &environment,
            super::Agents {
                codex: Some(&codex),
                claude: None,
            },
            "r1".into(),
            a.id.clone(),
        )
        .await;
        assert!(
            !alive(read("pid").unwrap()),
            "the login is gone when the removal answers"
        );
        assert!(!alive(read("kid").unwrap()), "and the login it started");
        running.unwrap().await.unwrap();
        assert_eq!(
            serde_json::to_value(&removed).unwrap()["accounts"],
            serde_json::json!([])
        );
        let _ = std::fs::remove_dir_all(
            protocol::root_dir()
                .join("accounts")
                .join("codex")
                .join(&a.id),
        );
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

    /// **The accounts wire the apps read is what ccd answers** (`fixtures/accounts/minor-48-wire.json`, recorded from a
    /// ccd at 05faeaa3 by one paired device). Each request, in the recorded order, is answered by this build on a store
    /// of its own; the reply's bytes are the recorded reply's, the ids aside: ccd makes new ones, so each is read as the
    /// recorded id it stands for, in the order the replies first name them.
    #[test]
    fn the_recorded_accounts_wire_is_what_this_build_answers() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/accounts/minor-48-wire.json"
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
