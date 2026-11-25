//! Starting a session from the phone: the folders it can pick from, browsing and
//! making folders, and the start.
//!
//! Everything the daemon runs on the phone's behalf goes through the user's own
//! login shell ([`login_shell_command`]), started from the user's session
//! environment ([`phone_environment`]), so the launcher and the agent it starts
//! see the PATH, version managers and credentials a new terminal would — not the
//! daemon's service environment.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use protocol::agent::AgentKind;
use protocol::host::Trust;
use protocol::ws::{
    AgentAvailability, AgentModel, ClientMessage, CreateFolderResult, FolderContents, Project,
    ProjectGit, ServerMessage, StartFailure, StartSessionResult, StartWorktree, Worktree,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::state::Daemon;

/// How long a start's run may take to come up, and a `git worktree add` to end, before
/// the start is given up: the backstop for one that never does.
pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(80);

/// How long one folder's git branch may take.
const BRANCH_TIMEOUT: Duration = Duration::from_secs(2);

/// How long one git read for a project's worktrees and branches may take.
pub(crate) const GIT_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest worktree name, as Claude Code's `--worktree` takes it.
const MAX_WORKTREE_NAME_BYTES: usize = 64;

/// How many folders' branches are read at once.
const BRANCH_READS_AT_ONCE: usize = 8;

/// How long the login shell and the launcher may take to say an agent is there.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// An account request answered. A change it made (anything but a listing) is pushed as that same listing, naming
/// no request; a sign-in's end is pushed when its login ends.
async fn accounts_answer(daemon: &Arc<Daemon>, message: ClientMessage) -> ServerMessage {
    let environment = phone_environment();
    // Only a sign-in and a removal run their agent; every other call is answered from the store and what is held.
    let runs_agent = matches!(
        message,
        ClientMessage::SignInAccount { .. } | ClientMessage::RemoveAccount { .. }
    );
    let (codex, claude) = if runs_agent {
        (
            codex_path(&environment, daemon.config.codex_bin.as_deref()).await,
            claude_path(&environment, daemon.config.claude_bin.as_deref()).await,
        )
    } else {
        (None, None)
    };
    let agents = crate::accounts::Agents {
        codex: codex.as_deref(),
        claude: claude.as_deref(),
    };
    let changes = !matches!(message, ClientMessage::ListAccounts { .. });
    // An account added or removed: the sign-ins are read again behind the answer.
    if matches!(
        message,
        ClientMessage::AddAccount { .. } | ClientMessage::RemoveAccount { .. }
    ) {
        daemon.account_facts.forget_signed();
    }
    let chosen = match &message {
        ClientMessage::ActivateAccount { agent, .. } => Some(agent.clone()),
        _ => None,
    };
    let reply = match message {
        ClientMessage::SignInAccount {
            request_id,
            account_id,
        } => {
            let ended = Arc::clone(daemon);
            let signed = account_id.clone();
            crate::accounts::sign_in(
                &daemon.store,
                &daemon.sign_ins,
                move || {
                    // What the account said of itself was its old sign-in's: read again, with its sign-in.
                    ended.account_facts.forget(&signed);
                    ended.account_facts.forget_signed();
                    tokio::spawn(async move { refresh_facts(&ended).await });
                },
                &environment,
                agents,
                request_id,
                account_id,
                crate::accounts::SIGN_IN_WAIT,
            )
            .await
            .0
        }
        // Antigravity's one sign-in: what it said of itself, and its offer to a start, are its old sign-in's.
        ClientMessage::SignInAgent { request_id, agent } => {
            let agy = agy_path(&environment, daemon.config.agy_bin.as_deref()).await;
            let ended = Arc::clone(daemon);
            crate::accounts::sign_in_agent(
                &daemon.store,
                &daemon.sign_ins,
                // What agy said is the sign-in the list carries at once; with no answer of agy's, it is read again.
                move |said| {
                    ended.agents.forget(&AgentKind::Antigravity);
                    let known = said.is_some();
                    crate::accounts::keep_antigravity(&ended.account_facts, said);
                    tokio::spawn(async move {
                        if known {
                            push_accounts(&ended).await
                        } else {
                            refresh_facts(&ended).await
                        }
                    });
                },
                &environment,
                agent,
                agy.as_deref(),
                request_id,
                crate::accounts::SIGN_IN_WAIT,
            )
            .await
            .0
        }
        ClientMessage::SignOutAgent { request_id, agent } => {
            let agy = agy_path(&environment, daemon.config.agy_bin.as_deref()).await;
            let reply = crate::accounts::sign_out_agent(
                &daemon.store,
                &daemon.account_facts,
                &environment,
                agent,
                agy.as_deref(),
                request_id,
            )
            .await;
            daemon.agents.forget(&AgentKind::Antigravity);
            // Refused, agy's sign-in is not known: read again.
            if !matches!(reply, ServerMessage::Accounts { .. }) {
                daemon
                    .account_facts
                    .forget(&crate::accounts::default_key(&AgentKind::Antigravity));
            }
            reply
        }
        ClientMessage::RemoveAccount {
            request_id,
            account_id,
        } => {
            daemon.account_facts.forget(&account_id);
            crate::accounts::remove_with(
                &daemon.store,
                &daemon.sign_ins,
                &environment,
                agents,
                request_id,
                account_id,
            )
            .await
        }
        message => {
            let store = daemon.store.clone();
            let request_id = crate::accounts::request_id(&message);
            let reply =
                tokio::task::spawn_blocking(move || crate::accounts::answer(&store, message))
                    .await
                    .unwrap_or_else(|err| {
                        crate::accounts::unavailable(
                            request_id,
                            "answer the request",
                            anyhow::anyhow!("{err}"),
                        )
                    });
            reply
        }
    };
    let reply = daemon.account_facts.told(daemon.sign_ins.marked(reply));
    // Behind every answer, each account's sign-in is read again, and its facts when they are old, then pushed.
    if matches!(reply, ServerMessage::Accounts { .. }) {
        let daemon = Arc::clone(daemon);
        tokio::spawn(async move { refresh_facts(&daemon).await });
    }
    // A choice made moves the agent's live runs to it (306 Slice E).
    if let (Some(agent), ServerMessage::Accounts { .. }) = (chosen, &reply) {
        let daemon = Arc::clone(daemon);
        tokio::spawn(async move { crate::switch::begin(&daemon, &agent).await });
    }
    if let (
        true,
        ServerMessage::Accounts {
            accounts,
            active,
            defaults,
            agent_sign_ins,
            ..
        },
    ) = (changes, &reply)
    {
        let _ = daemon.accounts_tx.send(ServerMessage::Accounts {
            request_id: None,
            accounts: accounts.clone(),
            active: active.clone(),
            defaults: defaults.clone(),
            agent_sign_ins: agent_sign_ins.clone(),
        });
    }
    reply
}

/// The accounts listing, with what is held, pushed to every connection that reads accounts.
pub(crate) async fn push_accounts(daemon: &Arc<Daemon>) {
    let store = daemon.store.clone();
    let Ok(listing) =
        tokio::task::spawn_blocking(move || crate::accounts::listing(&store, None)).await
    else {
        return;
    };
    if matches!(listing, ServerMessage::Accounts { .. }) {
        let _ = daemon
            .accounts_tx
            .send(daemon.account_facts.told(daemon.sign_ins.marked(listing)));
    }
}

/// The accounts' sign-ins and facts read again where they are old ([`crate::accounts::Facts`]), then pushed once.
/// One refresh at a time: a call during one has it go round once more, so what that call changed is read too.
pub(crate) async fn refresh_facts(daemon: &Arc<Daemon>) {
    if !daemon.account_facts.begin_refresh() {
        return;
    }
    loop {
        refresh_once(daemon).await;
        if !daemon.account_facts.end_refresh() {
            break;
        }
    }
}

/// One round of [`refresh_facts`]: when the sign-ins are old, every account's read again at once; each account, and
/// each agent's Default, whose facts are old or missing and not being read, read again (one read at a time per
/// account, all at once); then the listing pushed. Nothing old, nothing is started and nothing pushed. Every read is
/// bounded, so one agent that does not answer holds no other account's.
async fn refresh_once(daemon: &Arc<Daemon>) {
    let Ok(accounts) = daemon.store.accounts() else {
        return;
    };
    let mut wanted: Vec<(String, AgentKind, Option<crate::store::Account>)> = vec![(
        crate::accounts::default_key(&AgentKind::Antigravity),
        AgentKind::Antigravity,
        None,
    )];
    for agent in [AgentKind::Codex, AgentKind::Claude] {
        wanted.push((crate::accounts::default_key(&agent), agent.clone(), None));
        for account in accounts
            .iter()
            .filter(|account| account.agent == agent.as_str())
        {
            wanted.push((account.id.clone(), agent.clone(), Some(account.clone())));
        }
    }
    let keys: Vec<String> = wanted.iter().map(|(key, _, _)| key.clone()).collect();
    let claimed = daemon.account_facts.claim_stale(&keys);
    let signed = daemon.account_facts.signed_stale();
    if claimed.is_empty() && signed.is_none() {
        return;
    }
    let environment = phone_environment();
    let codex = codex_path(&environment, daemon.config.codex_bin.as_deref()).await;
    let claude = claude_path(&environment, daemon.config.claude_bin.as_deref()).await;
    let agy = agy_path(&environment, daemon.config.agy_bin.as_deref()).await;
    if let Some(asked) = signed {
        let agents = crate::accounts::Agents {
            codex: codex.as_deref(),
            claude: claude.as_deref(),
        };
        let store = daemon.store.clone();
        if let Ok(listing) =
            tokio::task::spawn_blocking(move || crate::accounts::listing(&store, None)).await
        {
            let listing = crate::accounts::with_signed_in(&environment, agents, listing).await;
            daemon.account_facts.keep_signed(&listing, asked);
        }
    }
    let root = protocol::root_dir();
    let reads = wanted
        .iter()
        .filter(|(key, _, _)| claimed.contains(key))
        .map(|(key, agent, account)| {
            let (environment, root, codex, claude, agy) =
                (&environment, &root, &codex, &claude, &agy);
            async move {
                // Antigravity's one sign-in: what agy offers a start (kept while it is signed in, so no new run of
                // agy), its plan its tier's name as agy gives it.
                if *agent == AgentKind::Antigravity {
                    let offer = match agy {
                        Some(agy) => agy_offer(&daemon.agents, environment, agy).await,
                        None => None,
                    };
                    if let Some(offer) = &offer {
                        daemon.account_facts.keep_agent_signed(key, offer.signed_in);
                    }
                    daemon
                        .account_facts
                        .read(key, offer.as_ref().and_then(agy_default_facts));
                    return;
                }
                let home = match account {
                    Some(account) => {
                        match crate::accounts::AccountEnv::for_account(root, account) {
                            Ok(home) => home,
                            Err(_) => {
                                daemon.account_facts.read(key, None);
                                return;
                            }
                        }
                    }
                    None => None,
                };
                let facts = match (agent, codex, claude) {
                    (AgentKind::Codex, Some(codex), _) => {
                        crate::accounts::read_facts(environment, home.as_ref(), codex).await
                    }
                    (AgentKind::Claude, _, Some(claude)) => {
                        let facts =
                            crate::accounts::read_claude_facts(environment, home.as_ref(), claude)
                                .await;
                        // Its plan's limits, through the host, for a signed-in account only.
                        match (facts, crate::claude_host::host_binary(&daemon.config)) {
                            (Some(mut facts), Ok(host)) => {
                                if let Some((available, plan, windows)) =
                                    crate::accounts::read_claude_usage(
                                        environment,
                                        home.as_ref(),
                                        claude,
                                        &host,
                                    )
                                    .await
                                {
                                    facts.limits_available = Some(available);
                                    facts.plan = facts.plan.or(plan);
                                    facts.windows = windows;
                                }
                                Some(facts)
                            }
                            (facts, _) => facts,
                        }
                    }
                    _ => None,
                };
                daemon.account_facts.read(key, facts);
            }
        });
    futures_util::future::join_all(reads).await;
    push_accounts(daemon).await;
}

/// The agents the phone can start. An app that does not know one leaves it out of its menu.
const AGENTS: [AgentKind; 3] = [AgentKind::Claude, AgentKind::Codex, AgentKind::Antigravity];

/// The answer to a `get_project_git`, `list_folder`, `create_folder`,
/// `start_session` or one of the folder-trust frames. A `list_projects` is answered
/// by [`list_projects`].
pub async fn answer(daemon: &Arc<Daemon>, message: ClientMessage) -> ServerMessage {
    match message {
        ClientMessage::ListFolderTrust { request_id } => folder_trust(daemon, request_id).await,
        ClientMessage::SetFolderTrust {
            request_id,
            path,
            trust,
        } => match daemon.db.set_folder_trust(path, trust).await {
            Ok(()) => folder_trust(daemon, request_id).await,
            Err(err) => folder_trust_unavailable("keep the folder's answer", err),
        },
        ClientMessage::ForgetFolderTrust { request_id, path } => {
            match daemon.db.forget_folder_trust(path).await {
                Ok(_) => folder_trust(daemon, request_id).await,
                Err(err) => folder_trust_unavailable("forget the folder's answer", err),
            }
        }
        ClientMessage::ListAccounts { .. }
        | ClientMessage::AddAccount { .. }
        | ClientMessage::ActivateAccount { .. }
        | ClientMessage::SignInAccount { .. }
        | ClientMessage::SignInAgent { .. }
        | ClientMessage::SignOutAgent { .. }
        | ClientMessage::RemoveAccount { .. } => accounts_answer(daemon, message).await,
        ClientMessage::GetProjectGit { path } => project_git(daemon, path).await,
        // Off the async workers: a read can wait on macOS asking the user.
        ClientMessage::ListFolder {
            request_id,
            path,
            show_hidden,
        } => {
            let asked = path.unwrap_or_else(|| protocol::home_dir().display().to_string());
            let path = asked.clone();
            let (path, contents) =
                tokio::task::spawn_blocking(move || list_folder(&path, show_hidden))
                    .await
                    .unwrap_or_else(|err| {
                        (
                            asked,
                            FolderContents::Unreadable {
                                error: format!("the listing stopped: {err}"),
                            },
                        )
                    });
            ServerMessage::FolderListing {
                request_id,
                path,
                contents,
            }
        }
        ClientMessage::CreateFolder {
            request_id,
            parent,
            name,
        } => ServerMessage::CreateFolderResult {
            result: tokio::task::spawn_blocking(move || create_folder(&parent, &name))
                .await
                .unwrap_or_else(|err| CreateFolderResult::Failed {
                    detail: format!("the folder was not made: {err}"),
                }),
            request_id,
        },
        ClientMessage::StartSession {
            request_id,
            path,
            agent,
            prompt,
            worktree,
            model,
            trust,
        } => {
            let state_dir = protocol::root_dir();
            // One environment per start: the check and the run it starts read the
            // same login shell, since `auth status` turns on variables alone.
            let environment = phone_environment();
            // The account the agent's starts use now, read once for this start: its checks and its run go on it.
            let account =
                match crate::accounts::AccountEnv::active(&daemon.store, &state_dir, &agent) {
                    Ok(account) => account,
                    Err(err) => {
                        let detail =
                            format!("could not read {}'s account: {err:#}", agent.as_str());
                        crate::log_error!("{detail}");
                        return ServerMessage::StartSessionResult {
                            result: failed(StartFailure::LaunchFailed, detail),
                            request_id,
                        };
                    }
                };
            // A start without a first message, or with one of only white space, starts
            // the run waiting for the person's message: its prompt is empty.
            let prompt = prompt
                .as_deref()
                .filter(|prompt| !prompt.trim().is_empty())
                .unwrap_or_default();
            let result = match agent {
                AgentKind::Claude => {
                    let start = hosted_start(
                        AgentKind::Claude,
                        daemon,
                        crate::state::codex_launcher(),
                        &state_dir,
                        &request_id,
                        &path,
                        prompt,
                        worktree.as_ref(),
                        model.as_deref(),
                        &environment,
                        account.as_ref(),
                        trust,
                    );
                    daemon.starts.once(&request_id, start).await
                }
                AgentKind::Codex => {
                    let start = codex_start(
                        daemon,
                        crate::state::codex_launcher(),
                        &state_dir,
                        &request_id,
                        &path,
                        prompt,
                        worktree.as_ref(),
                        model.as_deref(),
                        &environment,
                        account.as_ref(),
                        trust,
                    );
                    daemon.starts.once(&request_id, start).await
                }
                AgentKind::Antigravity => {
                    let start = hosted_start(
                        AgentKind::Antigravity,
                        daemon,
                        crate::state::codex_launcher(),
                        &state_dir,
                        &request_id,
                        &path,
                        prompt,
                        worktree.as_ref(),
                        model.as_deref(),
                        &environment,
                        account.as_ref(),
                        trust,
                    );
                    daemon.starts.once(&request_id, start).await
                }
                AgentKind::Unsupported(name) => failed(
                    StartFailure::LaunchFailed,
                    format!("{name} cannot be started from the phone"),
                ),
            };
            ServerMessage::StartSessionResult { result, request_id }
        }
        _ => unreachable!(
            "handle_client passes only get_project_git, list_folder, create_folder, \
             start_session and the folder-trust frames here"
        ),
    }
}

/// Every folder's answer to "trust this folder?", as a `folder_trust` reply.
async fn folder_trust(daemon: &Arc<Daemon>, request_id: String) -> ServerMessage {
    match daemon.db.folder_trusts().await {
        Ok(folders) => ServerMessage::FolderTrust {
            request_id,
            folders,
        },
        Err(err) => folder_trust_unavailable("read the folders' answers", err),
    }
}

fn folder_trust_unavailable(what: &str, err: anyhow::Error) -> ServerMessage {
    crate::log_error!("could not {what}: {err:#}");
    ServerMessage::Error {
        code: "folder_trust_unavailable".into(),
        message: format!("could not {what}: {err}"),
    }
}

/// Which agents could start, as the last check found: a project list answers
/// from it rather than wait seconds on login shells. Empty until the first list
/// after the daemon starts, which waits for the check. And what each agent last
/// offered a start, kept by its version ([`offer`]).
#[derive(Default)]
pub struct KnownAgents {
    /// The last check, with the accounts it was made on: a list answers from it only while those are still the ones
    /// each agent's starts use.
    last: Mutex<Option<(Selections, Vec<AgentAvailability>)>>,
    offers: Mutex<HashMap<String, (String, Offer)>>,
}

/// Each agent's account a check was made on, in [`AGENTS`]' order; None is its Default.
type Selections = Vec<Option<String>>;

impl KnownAgents {
    fn get(&self, selections: &Selections) -> Option<Vec<AgentAvailability>> {
        let last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        last.as_ref()
            .filter(|(made_on, _)| made_on == selections)
            .map(|(_, agents)| agents.clone())
    }

    pub(crate) fn set(&self, selections: Selections, agents: Vec<AgentAvailability>) {
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = Some((selections, agents));
    }

    /// What `agent` offered on `account` at `version`, if that is the version it was asked at.
    fn offered(
        &self,
        agent: &AgentKind,
        account: Option<&crate::accounts::AccountEnv>,
        version: &str,
    ) -> Option<Offer> {
        let offers = self.offers.lock().unwrap_or_else(|p| p.into_inner());
        offers
            .get(&offer_key(agent, account))
            .filter(|(at, _)| at == version)
            .map(|(_, offer)| offer.clone())
    }

    /// What `agent` offered is forgotten, so the next list asks it again: its sign-in has changed.
    pub(crate) fn forget(&self, agent: &AgentKind) {
        let mut offers = self.offers.lock().unwrap_or_else(|p| p.into_inner());
        offers.remove(&offer_key(agent, None));
    }

    fn keep(
        &self,
        agent: &AgentKind,
        account: Option<&crate::accounts::AccountEnv>,
        version: &str,
        offer: Offer,
    ) {
        let mut offers = self.offers.lock().unwrap_or_else(|p| p.into_inner());
        offers.insert(offer_key(agent, account), (version.to_string(), offer));
    }
}

/// An offer is `agent`'s on one account: another account's sign-in and models are its own.
fn offer_key(agent: &AgentKind, account: Option<&crate::accounts::AccountEnv>) -> String {
    match account {
        Some(account) => format!("{}/{}", agent.as_str(), account.id()),
        None => agent.as_str().to_string(),
    }
}

/// Each agent's account its starts use now, read once: the accounts a check is made on.
fn active_accounts(daemon: &Daemon) -> Vec<Option<crate::accounts::AccountEnv>> {
    let root = protocol::root_dir();
    AGENTS
        .iter()
        .map(
            |agent| match crate::accounts::AccountEnv::active(&daemon.store, &root, agent) {
                Ok(account) => account,
                Err(err) => {
                    crate::log_error!(
                        "could not read {}'s account; its Default stands: {err:#}",
                        agent.as_str()
                    );
                    None
                }
            },
        )
        .collect()
}

fn selections(accounts: &[Option<crate::accounts::AccountEnv>]) -> Selections {
    accounts
        .iter()
        .map(|account| account.as_ref().map(|account| account.id().to_string()))
        .collect()
}

/// The agents as a check finds them now, each on the account its starts use, kept for the next list.
async fn check_and_keep(
    daemon: &Arc<Daemon>,
    accounts: &[Option<crate::accounts::AccountEnv>],
) -> Vec<AgentAvailability> {
    let checked = check_agents(
        &daemon.agents,
        &phone_environment(),
        accounts,
        daemon.config.claude_bin.as_deref(),
        daemon.config.codex_bin.as_deref(),
        daemon.config.agy_bin.as_deref(),
    )
    .await;
    daemon.agents.set(selections(accounts), checked.clone());
    checked
}

async fn check_agents(
    known: &KnownAgents,
    environment: &HashMap<OsString, OsString>,
    accounts: &[Option<crate::accounts::AccountEnv>],
    claude_bin: Option<&str>,
    codex_bin: Option<&str>,
    agy_bin: Option<&str>,
) -> Vec<AgentAvailability> {
    futures_util::future::join_all(AGENTS.iter().zip(accounts).map(|(agent, account)| {
        let configured = match agent {
            AgentKind::Codex => codex_bin,
            AgentKind::Antigravity => agy_bin,
            _ => claude_bin,
        };
        agent_availability(agent, known, environment, account.as_ref(), configured)
    }))
    .await
}

/// Starts already asked for, by `request_id`: in memory, so a repeat on this
/// connection or after a reconnect gets the first outcome and starts nothing.
/// A daemon restart forgets them.
#[derive(Default)]
pub struct Starts(Mutex<HashMap<String, watch::Receiver<Option<StartSessionResult>>>>);

impl Starts {
    /// The outcome for `request_id`: `launch`'s, run once however often it is asked.
    pub(crate) async fn once(
        &self,
        request_id: &str,
        launch: impl std::future::Future<Output = StartSessionResult>,
    ) -> StartSessionResult {
        let (first, mut outcome) = {
            let mut asked = self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match asked.get(request_id) {
                Some(outcome) => (None, outcome.clone()),
                None => {
                    let (first, outcome) = watch::channel(None);
                    asked.insert(request_id.to_string(), outcome.clone());
                    (Some(first), outcome)
                }
            }
        };
        if let Some(first) = first {
            first.send_replace(Some(launch.await));
        }
        let result = match outcome.wait_for(Option::is_some).await {
            Ok(result) => result.clone().expect("waited until there is one"),
            Err(_) => failed(
                StartFailure::LaunchFailed,
                "the start was interrupted".into(),
            ),
        };
        result
    }
}

fn failed(reason: StartFailure, detail: String) -> StartSessionResult {
    StartSessionResult::Failed { reason, detail }
}

/// The answer for a start whose request id already has a run (H1): that run's, or
/// why the runs could not be read. `None` lets the start go on.
async fn existing_run(db: &crate::db::Db, request_id: &str) -> Option<StartSessionResult> {
    match db.hosted_run_for_request(request_id.to_string()).await {
        Ok(Some(run)) => Some(crate::claude_host::answer_for(run)),
        Ok(None) => None,
        Err(err) => Some(failed(
            StartFailure::LaunchFailed,
            format!("could not read this computer's runs: {err:#}"),
        )),
    }
}

/// A Codex start: the run its request already has (H1), or the start's checks, then its
/// row, its app-server and its first message ([`crate::codex_host`]). The checks come
/// before the row, so a start they refuse leaves none: the cheap refusals, the folder
/// question, the codex check and `launcher` (the `codeconnect` that runs the mail server),
/// all before the place, the one step that can change the repository. ccd makes a Codex
/// worktree itself, a new one included.
#[allow(clippy::too_many_arguments)]
async fn codex_start(
    daemon: &Arc<Daemon>,
    launcher: Result<PathBuf, String>,
    root: &Path,
    request_id: &str,
    path: &str,
    prompt: &str,
    worktree: Option<&StartWorktree>,
    model: Option<&str>,
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    trust: Option<Trust>,
) -> StartSessionResult {
    if let Some(answer) = existing_run(&daemon.db, request_id).await {
        return answer;
    }
    if let Some(refusal) = refusal(path, prompt) {
        return refusal;
    }
    // The start path's answer; the folder the run runs in can only lower it (below).
    let trust = match folder_question(&daemon.db, path, trust).await {
        Ok(trust) => trust,
        Err(asked) => return asked,
    };
    let ready = match codex_probe(environment, daemon.config.codex_bin.as_deref()).await {
        Ok(ready) => ready,
        Err((reason, detail)) => return failed(reason, detail),
    };
    let codeconnect = match launcher {
        Ok(codeconnect) => codeconnect,
        Err(why) => return failed(StartFailure::LaunchFailed, why),
    };
    let git_bin = daemon.config.git_bin.as_deref();
    let cwd = match worktree {
        None => path.to_string(),
        Some(worktree) => match place(environment, git_bin, path, worktree).await {
            Ok(cwd) => cwd,
            Err(why) => return failed(StartFailure::LaunchFailed, why),
        },
    };
    // As for a Claude run: the folder the run runs in, with its own kept `not_trusted`
    // answer, lowers the start's, and nothing lifts it.
    let trust = if trust == Trust::Trusted && declined(&daemon.db, Path::new(&cwd)).await {
        Trust::NotTrusted
    } else {
        trust
    };
    // The models this codex offers, for the run's settings menu, as a list finds them: the
    // answer kept for this version, or this codex asked now when none is kept that says it
    // is signed in (no list since ccd started, or one from before its sign-in). None when it
    // cannot be asked, and the run then offers its modes alone.
    let models = offer(
        &daemon.agents,
        environment,
        account,
        &ready.path,
        &AgentKind::Codex,
        &ready.version,
    )
    .await
    .map(|offer| offer.menu)
    .unwrap_or_default();
    crate::codex_host::start(
        daemon,
        crate::codex_host::Run {
            root,
            request_id,
            cwd: &cwd,
            prompt,
            model,
            models: &models,
            codex: &ready.path,
            codeconnect: &codeconnect,
            git_bin,
            environment,
            account,
            trust,
        },
    )
    .await
}

/// The refusals a start gets before anything is run: a first message past the limit,
/// a path that is not absolute, and a folder that is gone or that this daemon may not
/// reach.
fn refusal(path: &str, prompt: &str) -> Option<StartSessionResult> {
    if prompt.len() > protocol::ws::MAX_COMPOSE_BYTES {
        return Some(failed(
            StartFailure::LaunchFailed,
            "the first message is longer than 8 KiB".into(),
        ));
    }
    if let Err(why) = may_name(path) {
        return Some(failed(StartFailure::LaunchFailed, why));
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => None,
        // Refused, it is there but this daemon may not reach it; every other
        // error (gone, a file in the path, a symlink loop) is a folder no longer
        // there, as before.
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => Some(failed(
            StartFailure::LaunchFailed,
            format!("{path}: {}", refusal_words(&err, Path::new(path))),
        )),
        _ => Some(failed(
            StartFailure::FolderMissing,
            format!("{path} no longer exists"),
        )),
    }
}

/// The folder question, by the folder's real path: an answer given is kept, and with
/// none given or kept nothing runs and the app asks. `Ok` is the answer the start goes
/// on with, the one given or else the one kept: a Claude run loads the folder's project
/// only when it is `Trusted` (decision 157).
async fn folder_question(
    db: &crate::db::Db,
    path: &str,
    trust: Option<Trust>,
) -> Result<Trust, StartSessionResult> {
    let folder = match std::fs::canonicalize(path) {
        Ok(folder) => folder.to_string_lossy().into_owned(),
        Err(err) => return Err(failed(StartFailure::LaunchFailed, format!("{path}: {err}"))),
    };
    let answered = match trust {
        Some(trust) => db
            .set_folder_trust(folder.clone(), trust)
            .await
            .map(|()| Some(trust)),
        None => db.folder_trust(folder.clone()).await,
    };
    match answered {
        Ok(Some(trust)) => Ok(trust),
        Ok(None) => Err(StartSessionResult::NeedsTrust { path: folder }),
        Err(err) => Err(failed(
            StartFailure::LaunchFailed,
            format!("the folder's answer could not be read or kept: {err}"),
        )),
    }
}

/// The folder a Claude run runs in, for every start kind: the start folder; an existing or
/// a branch's worktree, as `place` found or made it (`cwd`); a new worktree's folder in
/// Claude's own layout under the repository's main folder, which `--worktree <name>`
/// reopens when it is already there.
async fn run_folder(
    path: &str,
    cwd: &str,
    worktree: Option<&StartWorktree>,
    git_bin: Option<&str>,
) -> PathBuf {
    match worktree {
        Some(StartWorktree::New { name }) => crate::git::worktrees(path, git_bin, GIT_READ_TIMEOUT)
            .await
            .and_then(|entries| {
                entries
                    .first()
                    .map(|main| worktree_folder(&main.path, name))
            })
            .unwrap_or_else(|| PathBuf::from(cwd)),
        None | Some(StartWorktree::Existing { .. } | StartWorktree::Branch { .. }) => {
            PathBuf::from(cwd)
        }
    }
}

/// Whether `folder` has its own kept `not_trusted` answer: by its real path, or by the path
/// itself while it is not there yet (a new worktree's). A folder whose answer cannot be
/// read counts as declined: it runs not trusted.
pub(crate) async fn declined(db: &crate::db::Db, folder: &Path) -> bool {
    let real = match std::fs::canonicalize(folder) {
        Ok(real) => real,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => folder.to_path_buf(),
        Err(_) => return true,
    };
    !matches!(
        db.folder_trust(real.to_string_lossy().into_owned()).await,
        Ok(None | Some(Trust::Trusted))
    )
}

/// A Claude or Antigravity start: the run its request already has (H1), or the start's
/// checks, then its row and its agent host ([`crate::claude_host`]). The checks here come before
/// the row, so a start they refuse leaves none: the cheap refusals, the folder
/// question, the claude check, the host binary, and `launcher` (the `codeconnect`
/// that runs the mail server), all before the place, the one step that can change the
/// repository. A refusal after the row, from the host's admission or from claude
/// itself, leaves the row `failed`. A new worktree is claude's to make, as
/// `--worktree` makes it today; Antigravity has no such flag, so ccd makes its new worktree, as Codex's.
#[allow(clippy::too_many_arguments)]
async fn hosted_start(
    agent: AgentKind,
    daemon: &Arc<Daemon>,
    launcher: Result<PathBuf, String>,
    root: &Path,
    request_id: &str,
    path: &str,
    prompt: &str,
    worktree: Option<&StartWorktree>,
    model: Option<&str>,
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    trust: Option<Trust>,
) -> StartSessionResult {
    if let Some(answer) = existing_run(&daemon.db, request_id).await {
        return answer;
    }
    if let Some(refusal) = refusal(path, prompt) {
        return refusal;
    }
    // The start path's answer; the folder the run runs in can only lower it (below).
    let trust = match folder_question(&daemon.db, path, trust).await {
        Ok(trust) => trust,
        Err(asked) => return asked,
    };
    let binary = match agent {
        AgentKind::Antigravity => {
            match agy_path(environment, daemon.config.agy_bin.as_deref()).await {
                Some(path) => path,
                None => {
                    let (reason, detail) = missing(&agent);
                    return failed(reason, detail);
                }
            }
        }
        _ => {
            match claude_preflight(environment, account, daemon.config.claude_bin.as_deref()).await
            {
                Ok(ready) => ready.path,
                Err(refusal) => return refusal,
            }
        }
    };
    let host_bin = match crate::claude_host::host_binary(&daemon.config) {
        Ok(host_bin) => host_bin,
        Err(why) => return failed(StartFailure::LaunchFailed, why),
    };
    let codeconnect = match launcher {
        Ok(codeconnect) => codeconnect,
        Err(why) => return failed(StartFailure::LaunchFailed, why),
    };
    let Some(claude_path) = binary.to_str() else {
        return failed(
            StartFailure::LaunchFailed,
            format!(
                "the {} at {} has a path that is not text",
                product(&agent),
                binary.display()
            ),
        );
    };
    let (cwd, new_worktree) = match worktree {
        None => (path.to_string(), None),
        // Claude makes its own (`--worktree`); agy has no such flag, so ccd makes one for it as for Codex.
        Some(StartWorktree::New { name }) if agent != AgentKind::Antigravity => {
            match check_worktree_name(name) {
                Ok(()) => (path.to_string(), Some(name.as_str())),
                Err(why) => return failed(StartFailure::LaunchFailed, why),
            }
        }
        Some(worktree) => match place(
            environment,
            daemon.config.git_bin.as_deref(),
            path,
            worktree,
        )
        .await
        {
            Ok(cwd) => (cwd, None),
            Err(why) => return failed(StartFailure::LaunchFailed, why),
        },
    };
    // A folder the person answered "not trusted" never runs trusted, whichever folder the
    // start named: the folder the run runs in, with its own kept `not_trusted` answer,
    // lowers the start's. It only ever lowers, and the stored start line records what was
    // sent, so a restart re-sends it without looking again.
    let trust = if trust == Trust::Trusted
        && declined(
            &daemon.db,
            &run_folder(path, &cwd, worktree, daemon.config.git_bin.as_deref()).await,
        )
        .await
    {
        Trust::NotTrusted
    } else {
        trust
    };
    crate::claude_host::start(
        daemon,
        crate::claude_host::Run {
            agent,
            root,
            request_id,
            cwd: &cwd,
            prompt,
            model,
            worktree: new_worktree,
            trust,
            claude_path,
            host_bin: &host_bin,
            codeconnect: &codeconnect,
            environment,
            account,
        },
    )
    .await
}

/// Whether the phone may name `path`, for a listing, a new folder, a start or a git
/// read alike: any absolute path, as a terminal may `cd` anywhere. Every other
/// refusal is the system's own, from the call that uses the path.
fn may_name(path: &str) -> Result<(), String> {
    if Path::new(path).is_absolute() {
        Ok(())
    } else {
        Err(format!("\"{path}\" is not an absolute path"))
    }
}

/// The folders in `path`, by its real path, and that path: what `ls` would list
/// as folders, symlinks to folders included, in byte order. Names that are not
/// UTF-8 cannot be named on the wire and are left out.
fn list_folder(path: &str, show_hidden: bool) -> (String, FolderContents) {
    let unreadable =
        |path: &str, error: String| (path.to_string(), FolderContents::Unreadable { error });
    if let Err(why) = may_name(path) {
        return unreadable(path, why);
    }
    let real = match std::fs::canonicalize(path) {
        Ok(real) => real,
        Err(err) => return unreadable(path, refusal_words(&err, Path::new(path))),
    };
    let Some(real_path) = real.to_str().map(str::to_string) else {
        return unreadable(path, format!("{path} has no UTF-8 real path"));
    };
    let entries = match std::fs::read_dir(&real) {
        Ok(entries) => entries,
        Err(err) => return unreadable(&real_path, refusal_words(&err, &real)),
    };
    let mut folders: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| match entry.file_type() {
            Ok(kind) if kind.is_symlink() => entry.path().is_dir(),
            Ok(kind) => kind.is_dir(),
            Err(_) => false,
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| show_hidden || !name.starts_with('.'))
        .collect();
    folders.sort_unstable();
    let truncated = folders.len() > protocol::ws::MAX_FOLDER_NAMES;
    folders.truncate(protocol::ws::MAX_FOLDER_NAMES);
    (real_path, FolderContents::Listed { folders, truncated })
}

/// `mkdir parent/name`: one folder, one level, its mode the umask's.
fn create_folder(parent: &str, name: &str) -> CreateFolderResult {
    let failed = |detail| CreateFolderResult::Failed { detail };
    if let Err(why) = may_name(parent) {
        return failed(why);
    }
    if name.contains('/') {
        return failed(format!("\"{name}\" is not a single folder name"));
    }
    let folder = Path::new(parent).join(name);
    match std::fs::create_dir(&folder) {
        Ok(()) => CreateFolderResult::Created,
        Err(err) => failed(refusal_words(&err, &folder)),
    }
}

/// [`system_words`], and where a macOS privacy "no" for `path` can be changed
/// when it may be one.
fn refusal_words(err: &std::io::Error, path: &Path) -> String {
    let words = system_words(err);
    match crate::folder_access::hint(err, path, &protocol::home_dir()) {
        Some(hint) => format!("{words}. {hint}"),
        None => words,
    }
}

/// An error in the words `ls` and `mkdir` print for it (`Permission denied`),
/// without the ` (os error 13)` Rust adds.
pub(crate) fn system_words(err: &std::io::Error) -> String {
    let words = err.to_string();
    match err.raw_os_error() {
        Some(code) => words
            .strip_suffix(&format!(" (os error {code})"))
            .unwrap_or(&words)
            .to_string(),
        None => words,
    }
}

/// The folder for a start in `worktree`, making the worktree first when it is
/// new. Every check reads the repository afresh, and every refusal says why.
async fn place(
    environment: &HashMap<OsString, OsString>,
    git_bin: Option<&str>,
    path: &str,
    worktree: &StartWorktree,
) -> Result<String, String> {
    let entries = crate::git::worktrees(path, git_bin, GIT_READ_TIMEOUT)
        .await
        .ok_or_else(|| format!("{path} is not in a git repository"))?;
    let main = entries
        .first()
        .map(|entry| entry.path.clone())
        .ok_or_else(|| format!("git listed no worktrees for {path}"))?;
    let made = |cwd: PathBuf| cwd.display().to_string();
    match worktree {
        StartWorktree::Existing { path: wanted } => {
            match entries.iter().find(|entry| &entry.path == wanted) {
                Some(entry) if entry.prunable => {
                    Err(format!("the worktree {wanted} no longer exists"))
                }
                Some(_) => Ok(made(PathBuf::from(wanted))),
                None => Err(format!("{wanted} is not a worktree of this repository")),
            }
        }
        StartWorktree::Branch { branch } => {
            if let Some(entry) = entries
                .iter()
                .find(|entry| entry.branch.as_deref() == Some(branch.as_str()))
            {
                return Err(format!("{branch} is already checked out in {}", entry.path));
            }
            let branches = crate::git::local_branches(path, git_bin, GIT_READ_TIMEOUT).await;
            if !branches.is_some_and(|branches| branches.contains(branch)) {
                return Err(format!("{branch} is not a local branch of this repository"));
            }
            let folder = worktree_folder(&main, branch);
            add_worktree(
                environment,
                path,
                &[OsStr::new("--"), folder.as_os_str(), OsStr::new(branch)],
            )
            .await?;
            Ok(made(folder))
        }
        StartWorktree::New { name } => {
            check_worktree_name(name)?;
            // The branch is the name as typed. Claude's own `worktree-` prefix marks a
            // branch Claude deletes on a clean exit, commits and all, should a later
            // `claude --worktree <name>` reopen this one.
            let folder = worktree_folder(&main, name);
            add_worktree(
                environment,
                path,
                &[
                    OsStr::new("-b"),
                    OsStr::new(name),
                    OsStr::new("--"),
                    folder.as_os_str(),
                    OsStr::new("HEAD"),
                ],
            )
            .await?;
            Ok(made(folder))
        }
    }
}

/// Where a worktree named `name` goes: Claude Code's own layout, with each `/`
/// written `+` as Claude writes it.
fn worktree_folder(main: &str, name: &str) -> PathBuf {
    Path::new(main)
        .join(".claude")
        .join("worktrees")
        .join(name.replace('/', "+"))
}

/// Claude Code's rule for a `--worktree` name (2.1.317), held to for both agents.
fn check_worktree_name(name: &str) -> Result<(), String> {
    let refuse = |why: &str| Err(format!("\"{name}\" is not a valid worktree name: {why}"));
    if name.len() > MAX_WORKTREE_NAME_BYTES {
        return refuse("it is longer than 64 characters");
    }
    for segment in name.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return refuse("each \"/\"-separated part must be non-empty and not \".\" or \"..\"");
        }
        if !segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return refuse("use only letters, digits, dots, underscores and dashes");
        }
    }
    Ok(())
}

/// `git worktree add <args>` in `path`, through the user's login shell as the
/// launcher runs: a checkout runs the repository's filters and hooks (Git LFS
/// among them), and they need the PATH a terminal has. Git's own last line is
/// the reason when it refuses.
async fn add_worktree(
    environment: &HashMap<OsString, OsString>,
    path: &str,
    args: &[&OsStr],
) -> Result<(), String> {
    let mut argv: Vec<&OsStr> = vec![
        OsStr::new("git"),
        OsStr::new("-C"),
        OsStr::new(path),
        OsStr::new("worktree"),
        OsStr::new("add"),
    ];
    argv.extend_from_slice(args);
    let mut command = login_shell_command(environment, None, &argv);
    command
        .current_dir(path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(START_TIMEOUT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => Err(last_line(&output.stderr)
            .unwrap_or_else(|| format!("git worktree add ended ({})", output.status))),
        Ok(Err(err)) => Err(format!("could not run the login shell: {err}")),
        Err(_) => Err(format!(
            "git worktree add did not finish within {} s",
            START_TIMEOUT.as_secs()
        )),
    }
}

/// The answer to `get_project_git`.
async fn project_git(daemon: &Arc<Daemon>, path: String) -> ServerMessage {
    if let Err(message) = may_name(&path) {
        return ServerMessage::Error {
            code: "bad_request".into(),
            message,
        };
    }
    let git_bin = daemon.config.git_bin.as_deref();
    let Some(entries) = crate::git::worktrees(&path, git_bin, GIT_READ_TIMEOUT).await else {
        return ServerMessage::ProjectGit { path, git: None };
    };
    let mut worktrees = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        worktrees.push(Worktree {
            main: index == 0,
            branch: entry.branch.clone(),
            dirty: !entry.prunable
                && crate::git::dirty(&entry.path, git_bin, GIT_READ_TIMEOUT).await,
            missing: entry.prunable,
            path: entry.path.clone(),
        });
    }
    let mut branches: Vec<String> = crate::git::local_branches(&path, git_bin, GIT_READ_TIMEOUT)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|branch| {
            !entries
                .iter()
                .any(|entry| entry.branch.as_deref() == Some(branch.as_str()))
        })
        .collect();
    let branches_truncated = branches.len() > protocol::ws::MAX_PROJECT_BRANCHES;
    branches.truncate(protocol::ws::MAX_PROJECT_BRANCHES);
    ServerMessage::ProjectGit {
        path,
        git: Some(ProjectGit {
            worktrees,
            branches,
            branches_truncated,
        }),
    }
}

/// The last line of `bytes` with anything on it: where a failing program says why,
/// without the `Error: ` the launcher prints before it. The user's shell startup
/// speaks first, so it is never the last line.
pub(crate) fn last_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.strip_prefix("Error: ").unwrap_or(line).to_string())
}

/// The answer to `list_projects`, sent to `replies`: the folders, and the agents as
/// the last check found them, or, the first time, as a check waited for finds them.
/// An answer from the last check then waits for a fresh one, kept for the next
/// list, and when it finds the agents changed, the same folders go out again with
/// them, so a sign-in, an install or an update shows without another list. Both
/// are sent from here, so the second always follows the first.
pub(crate) async fn list_projects(
    daemon: &Arc<Daemon>,
    replies: &tokio::sync::mpsc::UnboundedSender<ServerMessage>,
) {
    let rows = match daemon.db.projects().await {
        Ok(rows) => rows,
        Err(err) => {
            crate::log_error!("could not list projects: {err:#}");
            let _ = replies.send(ServerMessage::Error {
                code: "projects_unavailable".into(),
                message: format!("could not read the project list: {err}"),
            });
            return;
        }
    };
    let git_bin = daemon.config.git_bin.as_deref();
    // A few at a time: every git child holds pipes, and two hundred at once
    // would spend the daemon's descriptors.
    let projects = futures_util::stream::iter(rows)
        .map(|row| async move {
            let exists = Path::new(&row.path).is_dir();
            let git_branch = if exists {
                crate::git::branch(&row.path, git_bin, BRANCH_TIMEOUT).await
            } else {
                None
            };
            Project {
                name: crate::project_label::project_label(&row.path),
                path: row.path,
                last_used: row.last_used,
                last_agent: row.last_agent,
                git_branch,
                exists,
            }
        })
        .buffered(BRANCH_READS_AT_ONCE)
        .collect::<Vec<_>>();
    // Each agent's account, read once for this list: the last check answers only if it was made on these.
    let accounts = active_accounts(daemon);
    let known = daemon.agents.get(&selections(&accounts));
    let agents = async {
        match &known {
            Some(known) => known.clone(),
            None => check_and_keep(daemon, &accounts).await,
        }
    };
    let (projects, agents) = tokio::join!(projects, agents);
    let _ = replies.send(ServerMessage::Projects {
        projects: projects.clone(),
        agents: agents.clone(),
    });
    if known.is_none() {
        return;
    }
    let checked = check_and_keep(daemon, &accounts).await;
    if checked != agents {
        let _ = replies.send(ServerMessage::Projects {
            projects,
            agents: checked,
        });
    }
}

/// Whether `agent` can start here, and what it offers a start: Claude Code as
/// [`claude_probe`] finds it, Codex as [`codex_probe`] does, `configured` being that
/// agent's binary from config.json.
async fn agent_availability(
    agent: &AgentKind,
    known: &KnownAgents,
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    configured: Option<&str>,
) -> AgentAvailability {
    let unavailable = |detail: String| AgentAvailability {
        agent: agent.clone(),
        available: false,
        detail: Some(detail),
        models: Vec::new(),
        signed_in: None,
    };
    // Antigravity is startable where an agy is found, with what agy offers.
    if *agent == AgentKind::Antigravity {
        return match agy_path(environment, configured).await {
            Some(agy) => {
                let offer = agy_offer(known, environment, &agy)
                    .await
                    .unwrap_or_default();
                startable(agent, offer.signed_in, offer.models, &this_computer())
            }
            None => unavailable(missing(agent).1),
        };
    }
    if *agent == AgentKind::Claude {
        return match claude_probe(environment, account, configured).await {
            Err((_, detail)) => unavailable(detail),
            Ok(ready) => startable(
                agent,
                ready.signed_in,
                claude_models(known, environment, account, &ready).await,
                &this_computer(),
            ),
        };
    }
    match codex_probe(environment, configured).await {
        Err((_, detail)) => unavailable(detail),
        Ok(ready) => {
            let offer = offer(
                known,
                environment,
                account,
                &ready.path,
                agent,
                &ready.version,
            )
            .await
            .unwrap_or_default();
            startable(agent, offer.signed_in, offer.models, &this_computer())
        }
    }
}

/// The lowest Claude Code a start runs (decision 171).
const CLAUDE_FLOOR: (u32, u32, u32) = (2, 1, 268);

/// The lowest Codex a start runs: the version every fact of CodeConnect's Codex
/// engine was measured on.
const CODEX_FLOOR: (u32, u32, u32) = (0, 167, 0);

/// The name the sentences give `agent`.
fn product(agent: &AgentKind) -> &'static str {
    match agent {
        AgentKind::Codex => "Codex",
        AgentKind::Antigravity => "Antigravity",
        _ => "Claude Code",
    }
}

/// The Claude Code a start runs, as [`claude_probe`] found it: where it is, the
/// version it reported, and whether `claude auth status` says it is signed in
/// (`None` when its answer was not one of the two it gives).
///
/// It holds nothing else of what `auth status` printed. That output carries the
/// account's email, organization and plan, so it is read once for `loggedIn` and
/// dropped: never parsed further, logged, stored or sent (P1 22).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReadyClaude {
    /// The claude a start runs, which the host is handed as its Claude Code path.
    pub(crate) path: PathBuf,
    pub(crate) version: String,
    pub(crate) signed_in: Option<bool>,
}

/// The Codex a start runs, as [`codex_probe`] found it: where it is and the version
/// it reported. Its sign-in is [`offer`]'s: `login status`, or else its `account/read`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReadyCodex {
    pub(crate) path: PathBuf,
    pub(crate) version: String,
}

/// Whether Claude Code can start here, and the claude a start runs. A refusal is
/// the start's own answer: it is missing, below [`CLAUDE_FLOOR`], did not say its
/// version, or is signed out.
pub(crate) async fn claude_preflight(
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    claude_bin: Option<&str>,
) -> Result<ReadyClaude, StartSessionResult> {
    let ready = claude_probe(environment, account, claude_bin)
        .await
        .map_err(|(reason, detail)| failed(reason, detail))?;
    if ready.signed_in == Some(false) {
        return Err(failed(
            StartFailure::LaunchFailed,
            signed_out(&AgentKind::Claude, &this_computer()),
        ));
    }
    Ok(ready)
}

/// The computer the refusals name: its name as the system shows it, or "this
/// computer". Each sentence states the fact only; what to do about it is the app's.
fn this_computer() -> String {
    crate::machine::computer_name().unwrap_or_else(|| "this computer".to_string())
}

/// The sentence a signed-out agent's start is refused with.
fn signed_out(agent: &AgentKind, computer: &str) -> String {
    format!("{} on {computer} isn’t signed in.", product(agent))
}

/// What the list says of an agent a start can run: its models and its sign-in, and,
/// signed out, the sentence its start is refused with.
fn startable(
    agent: &AgentKind,
    signed_in: Option<bool>,
    models: Vec<AgentModel>,
    computer: &str,
) -> AgentAvailability {
    AgentAvailability {
        agent: agent.clone(),
        available: true,
        detail: (signed_in == Some(false)).then(|| signed_out(agent, computer)),
        models,
        signed_in,
    }
}

/// Find the claude a start runs ([`claude_path`]) and ask it its version and its
/// sign-in, each from the user's login shell in `environment`. Each is run itself,
/// never through `codeconnect`. An error is the refusal's reason and its sentence.
async fn claude_probe(
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    claude_bin: Option<&str>,
) -> Result<ReadyClaude, (StartFailure, String)> {
    let path = claude_path(environment, claude_bin)
        .await
        .ok_or_else(|| missing(&AgentKind::Claude))?;
    let version = version_at_floor(environment, &AgentKind::Claude, &path, CLAUDE_FLOOR).await?;
    let signed_in = claude_signed_in(environment, account, &path).await;
    Ok(ReadyClaude {
        path,
        version,
        signed_in,
    })
}

/// Find the codex a start runs ([`codex_path`]) and ask it its version, from the
/// user's login shell in `environment`, run itself, never through `codeconnect`. An
/// error is the refusal's reason and its sentence.
pub(crate) async fn codex_probe(
    environment: &HashMap<OsString, OsString>,
    codex_bin: Option<&str>,
) -> Result<ReadyCodex, (StartFailure, String)> {
    let path = codex_path(environment, codex_bin)
        .await
        .ok_or_else(|| missing(&AgentKind::Codex))?;
    let version = version_at_floor(environment, &AgentKind::Codex, &path, CODEX_FLOOR).await?;
    Ok(ReadyCodex { path, version })
}

/// The refusal for an agent found nowhere.
fn missing(agent: &AgentKind) -> (StartFailure, String) {
    (
        StartFailure::AgentMissing,
        format!("{} isn’t installed on {}.", product(agent), this_computer()),
    )
}

/// The version the `agent` at `path` reports with `--version`, as written, refused
/// when it reports none (or does not exit 0) or one below `floor`.
async fn version_at_floor(
    environment: &HashMap<OsString, OsString>,
    agent: &AgentKind,
    path: &Path,
    floor: (u32, u32, u32),
) -> Result<String, (StartFailure, String)> {
    let unread = || {
        crate::log_warn!(
            "{} at {} did not report its version",
            product(agent),
            path.display()
        );
        (
            StartFailure::LaunchFailed,
            format!(
                "{} on {} didn’t report its version.",
                product(agent),
                this_computer()
            ),
        )
    };
    let said = probe_output(
        environment,
        None,
        &[path.as_os_str(), OsStr::new("--version")],
    )
    .await
    .filter(|output| output.status.success())
    .ok_or_else(unread)?;
    let (number, version) = last_line(&said.stdout)
        .as_deref()
        .and_then(version_in)
        .ok_or_else(unread)?;
    if number < floor {
        let (major, minor, patch) = floor;
        return Err((
            StartFailure::LaunchFailed,
            format!(
                "{} {version} on {} is older than {major}.{minor}.{patch}, the oldest \
                 CodeConnect works with.",
                product(agent),
                this_computer()
            ),
        ));
    }
    Ok(version)
}

/// Where the claude a start runs is, from [`protocol::claude_bin`]'s list with the login
/// shell's `PATH`: config.json's `claude_bin`, every `claude` on that `PATH`, then the
/// well-known install locations; the first that is a program and not the launcher itself.
async fn claude_path(
    environment: &HashMap<OsString, OsString>,
    claude_bin: Option<&str>,
) -> Option<PathBuf> {
    let home = home_in(environment);
    let path_hits = login_path_hits(environment, &home, "claude").await;
    let launcher = crate::state::codex_launcher().ok();
    protocol::claude_bin::first_runnable(
        not_installed_in_tests(
            protocol::claude_bin::candidates(claude_bin, &home, path_hits),
            claude_bin,
        ),
        launcher.as_deref(),
    )
}

/// `candidates` as a run takes them; in a test, only the one it names (`configured`) and one in a temporary folder
/// (its rig: its own home, or a folder of its own it put on `PATH`): never the computer's install locations, a
/// person's own home, nor a system folder a login shell's `PATH` adds, so no test runs an agent installed there.
fn not_installed_in_tests(candidates: Vec<PathBuf>, configured: Option<&str>) -> Vec<PathBuf> {
    #[cfg(test)]
    let candidates = candidates
        .into_iter()
        .filter(|candidate| {
            configured.is_some_and(|configured| candidate == Path::new(configured))
                || under_temporary(candidate)
        })
        .collect();
    #[cfg(not(test))]
    let _ = configured;
    candidates
}

/// Whether `path` is inside a temporary folder: where a test's rig, its homes and its stand-ins live.
#[cfg(test)]
pub(crate) fn under_temporary(path: &Path) -> bool {
    [
        std::env::temp_dir(),
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
    ]
    .iter()
    .any(|folder| path.starts_with(folder))
}

/// Where the codex a start runs is, from [`protocol::codex_bin`]'s list with the login
/// shell's `PATH`: config.json's `codex_bin`, every `codex` on that `PATH`, then the
/// well-known install locations; the first that is a program and not the launcher itself.
pub(crate) async fn codex_path(
    environment: &HashMap<OsString, OsString>,
    codex_bin: Option<&str>,
) -> Option<PathBuf> {
    let home = home_in(environment);
    let path_hits = login_path_hits(environment, &home, "codex").await;
    let launcher = crate::state::codex_launcher().ok();
    protocol::claude_bin::first_runnable(
        not_installed_in_tests(
            protocol::codex_bin::candidates(codex_bin, &home, path_hits),
            codex_bin,
        ),
        launcher.as_deref(),
    )
}

/// The `agy` an Antigravity start runs: config `agy_bin`, else the first on the login
/// shell's `PATH`, else its installer's `~/.local/bin/agy`.
pub(crate) async fn agy_path(
    environment: &HashMap<OsString, OsString>,
    agy_bin: Option<&str>,
) -> Option<PathBuf> {
    let home = home_in(environment);
    let mut candidates: Vec<PathBuf> = agy_bin.map(PathBuf::from).into_iter().collect();
    candidates.extend(login_path_hits(environment, &home, "agy").await);
    candidates.push(home.join(".local/bin/agy"));
    protocol::claude_bin::first_runnable(not_installed_in_tests(candidates, agy_bin), None)
}

/// Every `name` on the `PATH` the user's login shell in `environment` sets up, in
/// its order.
async fn login_path_hits(
    environment: &HashMap<OsString, OsString>,
    home: &Path,
    name: &str,
) -> Vec<PathBuf> {
    probe_output(
        environment,
        None,
        &[
            OsStr::new("/bin/sh"),
            OsStr::new("-c"),
            OsStr::new("printf '%s\\n' \"$PATH\""),
        ],
    )
    .await
    .filter(|output| output.status.success())
    .and_then(|output| last_line(&output.stdout))
    .map(|path| protocol::claude_bin::hits_on(OsStr::new(&path), Some(home), name))
    .unwrap_or_default()
}

/// Whether `claude auth status` says the claude at `path` is signed in: exit 0 with
/// `loggedIn` true, signed out exit 1 with it false, and anything else unknown.
/// Its output is read for that one field only, and dropped here.
pub(crate) async fn claude_signed_in(
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    path: &Path,
) -> Option<bool> {
    let output = probe_output(
        environment,
        account,
        &[path.as_os_str(), OsStr::new("auth"), OsStr::new("status")],
    )
    .await?;
    auth_reading(output.status.code(), &output.stdout)
}

/// What `auth status` exiting `code` with `stdout` says of the sign-in.
fn auth_reading(code: Option<i32>, stdout: &[u8]) -> Option<bool> {
    match (code, logged_in(stdout)) {
        (Some(0), Some(true)) => Some(true),
        (Some(1), Some(false)) => Some(false),
        _ => None,
    }
}

/// The `loggedIn` of the object `auth status` prints ([`auth_status_object`]). Nothing else of it is kept.
fn logged_in(stdout: &[u8]) -> Option<bool> {
    auth_status_object(stdout)?.get("loggedIn")?.as_bool()
}

/// The JSON object `auth status` prints (over several lines) after whatever the user's shell printed as it
/// started: the last line that opens an object at its first column, read to the end.
pub(crate) fn auth_status_object(stdout: &[u8]) -> Option<serde_json::Value> {
    let text = String::from_utf8_lossy(stdout);
    let starts = text
        .match_indices('{')
        .map(|(at, _)| at)
        .filter(|&at| at == 0 || text.as_bytes()[at - 1] == b'\n');
    starts
        .rev()
        .find_map(|at| serde_json::from_str::<serde_json::Value>(&text[at..]).ok())
}

/// The first `X.Y.Z` in what `--version` printed, as numbers and as written.
fn version_in(line: &str) -> Option<((u32, u32, u32), String)> {
    line.split_whitespace().find_map(|word| {
        let mut parts = word.split('.').map(|part| part.parse::<u32>().ok());
        match (parts.next()??, parts.next()??, parts.next()??, parts.next()) {
            (major, minor, patch, None) => Some(((major, minor, patch), word.to_string())),
            _ => None,
        }
    })
}

/// One probe command from the user's login shell, in their home folder, with its
/// stdout; `None` when it could not run or did not end within [`PROBE_TIMEOUT`].
async fn probe_output(
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    args: &[&OsStr],
) -> Option<std::process::Output> {
    let mut command = agent_command(environment, account, args);
    command
        .current_dir(home_in(environment))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    agent_output(command, PROBE_TIMEOUT).await
}

/// The home folder `environment` names, else this daemon's.
fn home_in(environment: &HashMap<OsString, OsString>) -> PathBuf {
    environment
        .get(OsStr::new("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(protocol::home_dir)
}

/// The models Claude Code at `ready` offers a start. Asking starts it, and Claude
/// Code refreshes its caches in `~/.claude.json` as it starts, so a signed-in
/// claude's answer is kept for its version, and asked again once it is updated or
/// the daemon restarts. A claude that `auth status` says is signed out is not asked,
/// since its start is refused. One whose sign-in is not known is asked at every
/// list, since its start goes on.
async fn claude_models(
    known: &KnownAgents,
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    ready: &ReadyClaude,
) -> Vec<AgentModel> {
    if ready.signed_in == Some(false) {
        return Vec::new();
    }
    if ready.signed_in == Some(true) {
        if let Some(kept) = known.offered(&AgentKind::Claude, account, &ready.version) {
            if kept.signed_in == Some(true) {
                return kept.models;
            }
        }
    }
    let Some(mut asked) = ask(
        environment,
        account,
        &[ready.path.as_os_str()],
        &AgentKind::Claude,
        false,
    )
    .await
    else {
        return Vec::new();
    };
    asked.signed_in = ready.signed_in;
    known.keep(&AgentKind::Claude, account, &ready.version, asked.clone());
    asked.models
}

/// What an agent offers a start: the models it lists, and whether it is signed in.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Offer {
    models: Vec<AgentModel>,
    pub(crate) signed_in: Option<bool>,
    /// Antigravity's plan, as agy names it (`_meta.antigravity.tier.name`); none for any other agent, and when agy
    /// names none.
    pub(crate) tier: Option<String>,
    /// Codex's own entries for those models, as `model/list` gives them, their efforts
    /// among them: what a Codex run started on this answer keeps for its settings menu
    /// ([`crate::codex_host::Run::models`]). Empty for Claude.
    menu: Vec<serde_json::Value>,
}

/// What the codex at `codex`, at `version`, offers a start.
///
/// Unless a signed-in answer is kept for this version, `login status`, which starts
/// nothing and contacts nothing, is read first. Signed
/// in, Codex is asked for its models alone: a signed-in app-server answers
/// `model/list` at once and `account/read` not at all (0.167.0, measured). Otherwise
/// it is asked for its account too, which says whether it is signed out or needs no
/// OpenAI sign-in (a model provider of the user's own).
///
/// Asking starts it, and it reaches chatgpt.com, and signed out github.com too (its
/// plugin sync, which leaves a 4 KB `~/.codex/.tmp/git-*` folder Codex never
/// removes). So an answer that says it is signed in is kept for that version: it is
/// asked again once Codex is updated or the daemon restarts. A signed-out answer is
/// asked again only once `login status` says it is signed in. That status does not
/// count a model provider of the user's own, so one set up while the daemon runs is
/// offered once Codex is updated or the daemon restarts.
async fn offer(
    known: &KnownAgents,
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    codex: &Path,
    agent: &AgentKind,
    version: &str,
) -> Option<Offer> {
    let kept = known.offered(agent, account, version);
    if let Some(kept) = &kept {
        if kept.signed_in == Some(true) {
            return Some(kept.clone());
        }
    }
    let signed_in = codex_signed_in(environment, account, codex).await;
    if let (Some(kept), false) = (&kept, signed_in) {
        return Some(kept.clone());
    }
    let mut asked = ask(
        environment,
        account,
        &[codex.as_os_str()],
        agent,
        !signed_in,
    )
    .await?;
    if signed_in {
        asked.signed_in = Some(true);
    }
    known.keep(agent, account, version, asked.clone());
    Some(asked)
}

/// What asks Claude Code what it offers: one `initialize`, which it answers
/// without starting a conversation.
const CLAUDE_ASK: &str = concat!(
    r#"{"type":"control_request","request_id":"models","request":{"subtype":"initialize"}}"#,
    "\n"
);

/// What asks Codex's app-server for its models: its handshake, then `model/list`.
const CODEX_ASK: &str = concat!(
    r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"codeconnect-ccd","title":"CodeConnect daemon","version":""#,
    env!("CARGO_PKG_VERSION"),
    r#""}}}"#,
    "\n",
    r#"{"method":"initialized"}"#,
    "\n",
    r#"{"id":2,"method":"model/list","params":{}}"#,
    "\n"
);

/// What asks it for its account as well, when `login status` has not said it is signed
/// in, which it answers in any order with its models.
const CODEX_ACCOUNT_ASK: &str = concat!(r#"{"id":3,"method":"account/read","params":{}}"#, "\n");

/// Ask `agent` itself what it offers, `program` run from the user's login shell
/// with no terminal (the claude [`claude_probe`] found, or the codex [`codex_probe`] found):
/// Claude Code in print mode with no session file, no MCP server and no hook, or
/// Codex's app-server, asked for its account too when `with_account`. From the home
/// folder, so no project's settings run. Once it has answered, its stdin is closed:
/// Claude Code then ends, and is let end, since it writes its own files as it does;
/// Codex's app-server is ended, since a signed-in one keeps running. None when it
/// could not be asked, or did not answer within [`PROBE_TIMEOUT`].
async fn ask(
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    program: &[&OsStr],
    agent: &AgentKind,
    with_account: bool,
) -> Option<Offer> {
    let codex_ask = if with_account {
        format!("{CODEX_ASK}{CODEX_ACCOUNT_ASK}")
    } else {
        CODEX_ASK.to_string()
    };
    let (flags, asking): (&[&str], &str) = match agent {
        AgentKind::Claude => (
            &[
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--settings",
                r#"{"disableAllHooks":true}"#,
            ],
            CLAUDE_ASK,
        ),
        _ => (&["app-server"], codex_ask.as_str()),
    };
    let mut args = program.to_vec();
    args.extend(flags.iter().map(OsStr::new));
    let mut command = agent_command(environment, account, &args);
    command
        .current_dir(home_in(environment))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let asked = async {
        let mut stdin = child.stdin.take()?;
        stdin.write_all(asking.as_bytes()).await.ok()?;
        let mut lines = BufReader::new(child.stdout.take()?).lines();
        let mut said = Vec::new();
        while !answered(agent, with_account, &said) {
            let line = lines.next_line().await.ok()??;
            // What the user's shell prints as it starts is not the agent's.
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                said.push(value);
            }
        }
        drop(stdin);
        if *agent == AgentKind::Claude {
            child.wait().await.ok()?;
        }
        offer_from(agent, &said)
    };
    let offer = tokio::time::timeout(PROBE_TIMEOUT, asked)
        .await
        .ok()
        .flatten();
    // A signed-in app-server keeps running, and one that did not answer is ended too, with what it started.
    if *agent != AgentKind::Claude || offer.is_none() {
        end_agent(&mut child).await;
    }
    offer
}

/// What Antigravity's Default says of itself, from what agy offers a start: signed in, its plan is agy's tier's name
/// exactly as agy gives it (none when agy names none); signed out or not known, nothing.
pub(crate) fn agy_default_facts(offer: &Offer) -> Option<protocol::ws::AccountFacts> {
    (offer.signed_in == Some(true)).then(|| protocol::ws::AccountFacts {
        email: None,
        plan: offer.tier.clone(),
        windows: Vec::new(),
        limits_available: None,
        read_at: protocol::time::now_rfc3339(),
    })
}

/// One `agy acp` ccd talks to itself (the start box's ask, a sign-in, a sign-out), started as an agent is
/// ([`agent_command`], the user's environment, in the home folder, its own process group), its stderr dropped. Its
/// calls are numbered from 1; a call's answer is agy's reply to it, its notifications and anything else skipped.
pub(crate) struct Agy {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next: u64,
}

impl Agy {
    pub(crate) fn start(environment: &HashMap<OsString, OsString>, agy: &Path) -> Option<Agy> {
        let mut command = agent_command(environment, None, &[agy.as_os_str(), OsStr::new("acp")]);
        command
            .current_dir(home_in(environment))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().ok()?;
        let stdin = child.stdin.take()?;
        let lines = BufReader::new(child.stdout.take()?).lines();
        Some(Agy {
            child,
            stdin,
            lines,
            next: 1,
        })
    }

    /// agy's reply to `method` with `params` (its `result` or its `error`); none when agy ended first.
    pub(crate) async fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Option<serde_json::Value> {
        let id = self.next;
        self.next += 1;
        let line =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .ok()?;
        loop {
            let line = self.lines.next_line().await.ok()??;
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                if value["id"] == id && value.get("method").is_none() {
                    return Some(value);
                }
            }
        }
    }

    /// agy ended, with what it started ([`end_agent`]).
    pub(crate) async fn end(mut self) {
        end_agent(&mut self.child).await;
    }
}

/// What the agy at `agy` offers a start: the model families its settings name, and whether it is signed in. agy has
/// no list of its own over ACP, so it is asked by opening one session with no prompt (`session/new`, which a signed-out
/// agy refuses with "Authentication required") and closing it at once (`session/close`): agy 1.5.15 lists such a
/// session in no history (measured, `session/list` before and after), though its empty conversation file stays in
/// agy's folder. Asking starts agy and reaches Google, so a
/// signed-in answer is kept for that binary (its size and time: agy names no version); a signed-out one is asked
/// again at each list, as nothing cheaper says when it is signed in. None when agy answered neither way in time.
pub(crate) async fn agy_offer(
    known: &KnownAgents,
    environment: &HashMap<OsString, OsString>,
    agy: &Path,
) -> Option<Offer> {
    let version = std::fs::metadata(agy)
        .map(|meta| format!("{} {:?}", meta.len(), meta.modified().ok()))
        .unwrap_or_default();
    if let Some(kept) = known.offered(&AgentKind::Antigravity, None, &version) {
        return Some(kept);
    }
    let home = home_in(environment);
    let mut asking = Agy::start(environment, agy)?;
    let asked = async {
        asking
            .call(
                "initialize",
                serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
            )
            .await?;
        let opened = asking
            .call(
                "session/new",
                serde_json::json!({"cwd": home, "mcpServers": []}),
            )
            .await?;
        if opened["error"]["message"] == "Authentication required" {
            return Some(Offer {
                signed_in: Some(false),
                ..Offer::default()
            });
        }
        let session = opened["result"]["sessionId"].as_str()?;
        asking
            .call("session/close", serde_json::json!({"sessionId": session}))
            .await?;
        let models = opened["result"]["configOptions"]
            .as_array()?
            .iter()
            .find(|option| option["category"] == "model")?["options"]
            .as_array()?
            .iter()
            .filter_map(|entry| {
                Some(AgentModel {
                    value: entry["value"].as_str()?.to_string(),
                    label: entry["name"].as_str()?.to_string(),
                    detail: None,
                })
            })
            .collect();
        Some(Offer {
            models,
            signed_in: Some(true),
            menu: Vec::new(),
            tier: opened["result"]["_meta"]["antigravity"]["tier"]["name"]
                .as_str()
                .map(str::to_string),
        })
    };
    let offer = tokio::time::timeout(PROBE_TIMEOUT, asked)
        .await
        .ok()
        .flatten();
    asking.end().await;
    if offer
        .as_ref()
        .is_some_and(|offer| offer.signed_in == Some(true))
    {
        known.keep(&AgentKind::Antigravity, None, &version, offer.clone()?);
    }
    offer
}

/// Whether `said` holds every answer `agent` gives to the ask: for Codex, its
/// models, and its account when `account` asked for it.
fn answered(agent: &AgentKind, account: bool, said: &[serde_json::Value]) -> bool {
    let has = |id: u64| said.iter().any(|line| line["id"] == id);
    match agent {
        AgentKind::Claude => said.iter().any(|line| line["type"] == "control_response"),
        _ => has(2) && (!account || has(3)),
    }
}

/// What `agent`'s answers say it offers: the models it lists, in its order and
/// words, Claude Code's own `default` among them, and without those Codex hides;
/// and, for Codex, whether it is signed in (Claude Code's sign-in is `auth
/// status`'s, [`claude_signed_in`], and its answer's `account` is not read). None
/// when they list no models.
fn offer_from(agent: &AgentKind, said: &[serde_json::Value]) -> Option<Offer> {
    let model = |entry: &serde_json::Value, value: &str| {
        Some(AgentModel {
            value: entry[value].as_str()?.to_string(),
            label: entry["displayName"].as_str()?.to_string(),
            detail: entry["description"].as_str().map(str::to_string),
        })
    };
    match agent {
        AgentKind::Claude => {
            let reply = said
                .iter()
                .find(|line| line["type"] == "control_response")?;
            let answer = &reply["response"]["response"];
            Some(Offer {
                models: answer["models"]
                    .as_array()?
                    .iter()
                    .filter_map(|entry| model(entry, "value"))
                    .collect(),
                signed_in: None,
                menu: Vec::new(),
                tier: None,
            })
        }
        _ => {
            let result = |id: u64| {
                said.iter()
                    .find(|line| line["id"] == id)
                    .map(|line| &line["result"])
            };
            let listed: Vec<&serde_json::Value> = result(2)?["data"]
                .as_array()?
                .iter()
                .filter(|entry| entry["hidden"] != true)
                .collect();
            Some(Offer {
                models: listed
                    .iter()
                    .filter_map(|entry| model(entry, "model"))
                    .collect(),
                signed_in: result(3)
                    .filter(|account| account.is_object())
                    .map(|account| {
                        !account["account"].is_null() || account["requiresOpenaiAuth"] == false
                    }),
                menu: listed.into_iter().cloned().collect(),
                tier: None,
            })
        }
    }
}

/// Whether `login status` says the codex at `codex` is signed in. It reads the stored
/// sign-in and starts nothing, so, unlike an ask, it leaves nothing behind.
pub(crate) async fn codex_signed_in(
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    codex: &Path,
) -> bool {
    let mut command = agent_command(
        environment,
        account,
        &[codex.as_os_str(), OsStr::new("login"), OsStr::new("status")],
    );
    command
        .current_dir(protocol::home_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    agent_output(command, PROBE_TIMEOUT)
        .await
        .is_some_and(|output| output.status.success())
}

/// `args` run through the user's login shell, as one argv each, never one string,
/// from `environment` ([`phone_environment`] for everything the daemon runs).
///
/// zsh and bash run interactive and login (`-lic`), which is what reads the files
/// a new terminal reads. Any other shell, or none in `$SHELL`, runs `/bin/sh -lc`:
/// a written miss, so a fish user gets the system shell's environment.
///
/// What those files print with no newline at its end, as a terminal's shell
/// integration prints its escape codes, is ended with one before `args` run, so
/// the first line `args` print is theirs alone.
pub(crate) fn login_shell_command(
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    args: &[&OsStr],
) -> tokio::process::Command {
    let shell = environment
        .get(OsStr::new("SHELL"))
        .cloned()
        .unwrap_or_default();
    let name = Path::new(&shell).file_name().unwrap_or_default();
    let (program, flags) = if name == "zsh" || name == "bash" {
        (PathBuf::from(&shell), "-lic")
    } else {
        (PathBuf::from("/bin/sh"), "-lc")
    };
    if let Some(account) = account {
        account.link_shared(environment);
    }
    // A test's login shell never starts in a person's home, whose start-up files it would run.
    #[cfg(test)]
    assert!(
        environment
            .get(OsStr::new("HOME"))
            .is_some_and(|home| under_temporary(Path::new(home))),
        "a test's login shell would start outside a temporary home: {:?}",
        environment.get(OsStr::new("HOME"))
    );
    let mut command = tokio::process::Command::new(program);
    command
        .env_clear()
        .envs(environment.iter())
        .arg(flags)
        .arg("printf '\\n'; exec \"$@\"")
        .arg("codeconnect-start")
        // An account's variable, set by `env` at exec, after the shell's files ([`crate::accounts`]).
        .args(account.map(|account| account.argv()).into_iter().flatten())
        .args(args);
    command
}

/// `args`, an agent's program, run through the user's login shell ([`login_shell_command`]) in a session of its own,
/// so ending it ends what it started: Codex's npm launcher runs its native program as a child and passes on only some
/// signals. Every agent process ccd starts but a run's host ([`spawn_detached`], which lives on) is started here and,
/// when it is not let end by itself, ended by [`end_agent`].
pub(crate) fn agent_command(
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    args: &[&OsStr],
) -> tokio::process::Command {
    let mut command = login_shell_command(environment, account, args);
    command.kill_on_drop(true);
    // SAFETY: `setsid` is async-signal-safe and touches nothing of the parent's.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

/// The agent process `child` ([`agent_command`]) and everything it started, ended as a run host's group is: SIGTERM,
/// then SIGKILL after the grace ([`crate::claude_host::stop_group`]), the leader reaped meanwhile.
pub(crate) async fn end_agent(child: &mut tokio::process::Child) {
    // Only a process leading its own group (as `agent_command` starts one) is ended by its group.
    let leader = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        // SAFETY: getpgid(2) only reads the process's group.
        .filter(|pid| unsafe { libc::getpgid(*pid) } == *pid)
        .and_then(|pid| Some((pid, protocol::proc_identity::read_birth_identity(pid)?)))
        .zip(protocol::proc_identity::boot_identity());
    match leader {
        Some(((pid, birth), boot)) => {
            let _ = tokio::join!(
                crate::claude_host::stop_group(
                    protocol::proc_identity::ProcessIdentity { pid, birth },
                    boot
                ),
                child.wait()
            );
        }
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

/// `command` ([`agent_command`]) run to its end within `wait`, with what it printed; past `wait`, its group is ended
/// and there is none.
pub(crate) async fn agent_output(
    mut command: tokio::process::Command,
    wait: std::time::Duration,
) -> Option<std::process::Output> {
    async fn all<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> Vec<u8> {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut bytes).await;
        }
        bytes
    }
    let mut child = command.spawn().ok()?;
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let ran = tokio::time::timeout(wait, async {
        let (stdout, stderr) = tokio::join!(all(stdout), all(stderr));
        let status = child.wait().await.ok()?;
        Some(std::process::Output {
            status,
            stdout,
            stderr,
        })
    })
    .await;
    match ran {
        Ok(output) => output,
        Err(_) => {
            end_agent(&mut child).await;
            None
        }
    }
}

/// `args` run as a run's agent host: through the user's login shell
/// ([`login_shell_command`]) in a session of its own, so it outlives this daemon; from
/// `cwd`; with `uid` as `CODECONNECT_SESSION_UID`; stdin closed; and stdout and stderr
/// into `log`. It must be a file, never a pipe, which would break under the host's next
/// write once this daemon is gone. [`reap`] waits for the child once the caller no
/// longer needs it.
pub(crate) fn spawn_detached(
    environment: &HashMap<OsString, OsString>,
    account: Option<&crate::accounts::AccountEnv>,
    args: &[&OsStr],
    cwd: &Path,
    uid: &str,
    log: std::fs::File,
) -> std::io::Result<tokio::process::Child> {
    let log_err = log.try_clone()?;
    let mut command = login_shell_command(environment, account, args);
    command
        .env(protocol::ENV_SESSION_UID, uid)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    // SAFETY: `setsid` is async-signal-safe and touches nothing of the parent's.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

/// Wait for `child` while this daemon lives, so it is never left a zombie; after it,
/// the system adopts the process.
pub(crate) fn reap(mut child: tokio::process::Child) {
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
}

/// `runs/` and the run's own folder, both 0700, the folder made here and only here,
/// and its `host.log` (0600), which the run's output goes to.
pub(crate) fn make_run_dir(root: &Path, dir: &Path) -> std::io::Result<std::fs::File> {
    protocol::fsperm::private_dir(&root.join("runs"))?;
    std::fs::DirBuilder::new()
        .mode(protocol::fsperm::DIR_MODE)
        .create(dir)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(protocol::fsperm::FILE_MODE)
        .open(dir.join("host.log"))
}

/// An error once the run's [`spawn_detached`] child has ended (with what it last said
/// in the run's `host.log` past `since`, the log's length when it was launched, so a
/// launch into a folder that held one before reads only its own) or the start's time is up.
pub(crate) fn give_up_if_ended(
    dir: &Path,
    since: u64,
    child: &mut tokio::process::Child,
    deadline: tokio::time::Instant,
    waiting: &str,
) -> Result<(), String> {
    if let Ok(Some(status)) = child.try_wait() {
        let said = std::fs::read(dir.join("host.log")).ok().and_then(|log| {
            let own = usize::try_from(since).ok().and_then(|at| log.get(at..));
            last_line(own.unwrap_or_default())
        });
        return Err(said.unwrap_or_else(|| format!("the agent host ended ({status})")));
    }
    if tokio::time::Instant::now() >= deadline {
        return Err(format!("{waiting} within {} s", START_TIMEOUT.as_secs()));
    }
    Ok(())
}

/// What CodeConnect's own service definition sets for the daemon alone.
const SERVICE_ONLY: [&str; 2] = ["CODECONNECT_LOG", "CODECONNECT_SERVICE"];

/// Where this daemon keeps its state, and the `TMUX_TMPDIR` it was given: a phone start has
/// to reach this daemon, and a rig's must not fall back to the owner's tmux server, so these
/// go with it whatever the session says.
const DAEMON_LOCATION: [&str; 2] = ["CODECONNECT_HOME", "TMUX_TMPDIR"];

/// The character encoding a session displayed by CodeConnect, a UTF-8 terminal,
/// is described by when nothing else names one, in the platform's own spelling.
#[cfg(target_os = "macos")]
const UTF8_CTYPE: &str = "UTF-8";
#[cfg(not(target_os = "macos"))]
const UTF8_CTYPE: &str = "C.UTF-8";

/// The environment a phone-started program starts from: the user's session's, as
/// a terminal opened in that session hands its shell, not this service's.
///
/// Run as CodeConnect's own service, the daemon carries what its service
/// definition gave it; so the session is read afresh ([`session_environment`]),
/// and when it cannot be, the daemon's own is used (a written miss). Run any other
/// way, the daemon's environment is the session's already.
pub(crate) fn phone_environment() -> HashMap<OsString, OsString> {
    #[cfg(test)]
    assert!(
        crate::READING_ENV.with(std::cell::Cell::get) > 0,
        "a test copied the process environment without holding it: take crate::ProcessEnvRead first"
    );
    let own: HashMap<OsString, OsString> = std::env::vars_os().collect();
    let session = (crate::state::launchd_label().as_deref() == Some(protocol::launchd_label()))
        .then(|| session_environment(&own))
        .flatten();
    #[allow(unused_mut)]
    let mut environment = phone_environment_from(own, session);
    // A test given a person's own home (a bare `cargo test`) runs in a temporary one, so nothing it starts sees theirs.
    #[cfg(test)]
    if !environment
        .get(OsStr::new("HOME"))
        .is_some_and(|home| under_temporary(Path::new(home)))
    {
        let home = std::env::temp_dir().join("ccd-test-home");
        std::fs::create_dir_all(&home).unwrap();
        environment.insert("HOME".into(), home.into_os_string());
    }
    environment
}

/// [`phone_environment`], from the daemon's environment and, when it was read, the
/// session's.
fn phone_environment_from(
    own: HashMap<OsString, OsString>,
    session: Option<HashMap<OsString, OsString>>,
) -> HashMap<OsString, OsString> {
    let mut environment = match session {
        Some(mut session) => {
            for name in DAEMON_LOCATION {
                if let Some(value) = own.get(OsStr::new(name)) {
                    session.insert(name.into(), value.clone());
                }
            }
            session
        }
        None => own,
    };
    for name in SERVICE_ONLY {
        environment.remove(OsStr::new(name));
    }
    let names_an_encoding = ["LC_ALL", "LC_CTYPE", "LANG"].iter().any(|name| {
        environment
            .get(OsStr::new(name))
            .is_some_and(|value| !value.is_empty())
    });
    if !names_an_encoding {
        environment.insert("LC_CTYPE".into(), UTF8_CTYPE.into());
    }
    // No terminal ever told this session its type; this is the one it gets.
    environment
        .entry("TERM".into())
        .or_insert_with(|| "xterm-256color".into());
    environment
}

/// The session's environment on macOS: what launchd hands a job that is not a
/// service. That is the daemon's own but for what names the service: the `PATH`
/// its service definition set (the session's is the one `launchctl setenv` gave
/// it, or launchd's default), and launchd's `XPC_SERVICE_NAME` and `XPC_FLAGS`,
/// which carry this job's label where every other process has `0` and `0x0`.
/// `None` when `launchctl` cannot say.
#[cfg(target_os = "macos")]
fn session_environment(own: &HashMap<OsString, OsString>) -> Option<HashMap<OsString, OsString>> {
    let output = std::process::Command::new("/bin/launchctl")
        .args(["getenv", "PATH"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let set = String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string();
    let mut session = own.clone();
    session.insert(
        "PATH".into(),
        if set.is_empty() {
            "/usr/bin:/bin:/usr/sbin:/sbin".into()
        } else {
            set.into()
        },
    );
    session.insert("XPC_SERVICE_NAME".into(), "0".into());
    session.insert("XPC_FLAGS".into(), "0x0".into());
    Some(session)
}

/// The session's environment on Linux: the systemd user manager's own, which a
/// desktop terminal inherits, read as its D-Bus property so every value arrives as
/// it is. `None` when the manager cannot be asked.
#[cfg(not(target_os = "macos"))]
fn session_environment(_own: &HashMap<OsString, OsString>) -> Option<HashMap<OsString, OsString>> {
    let busctl = ["/usr/bin/busctl", "/bin/busctl"]
        .iter()
        .find(|path| Path::new(path).is_file())?;
    let output = std::process::Command::new(busctl)
        .args([
            "--user",
            "--json=short",
            "get-property",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "Environment",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let property: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    property["data"]
        .as_array()?
        .iter()
        .map(|entry| {
            let (name, value) = entry.as_str()?.split_once('=')?;
            Some((name.into(), value.into()))
        })
        .collect()
}

/// Where every [`test_claude`] appends the `HOME`, `PATH` and `XPC_SERVICE_NAME` it
/// ran with, one line a run (`unset` for a variable it did not have), so a test run
/// can be checked for a probe that ran in another test's fake `HOME` or took the
/// service branch.
#[cfg(test)]
pub(crate) fn claude_env_log() -> PathBuf {
    std::env::temp_dir().join("claude-env.log")
}

/// A Claude Code for tests, `<dir>/bin/claude`, and the environment a start finds it
/// in. It says it is 2.1.268 and signed in unless `<claude>.version`,
/// `<claude>.version_exit`, `<claude>.auth` or `<claude>.auth_exit` say otherwise,
/// answers `-p` with `<claude>.ask`, adds each run's arguments to `<claude>.argv`,
/// and each run's environment to [`claude_env_log`]. The environment's home is
/// `<dir>/home`, whose `.profile` makes the PATH `<dir>/bin:/usr/bin:/bin`, so no
/// claude, shell file or sign-in of the person running the tests is ever reached.
#[cfg(test)]
pub(crate) fn test_claude(dir: &Path) -> (PathBuf, HashMap<OsString, OsString>) {
    let bin = dir.join("bin");
    let home = dir.join("home");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        format!(
            "#!/bin/sh\nprintf 'HOME=%s PATH=%s XPC_SERVICE_NAME=%s\\n' \"${{HOME-unset}}\" \
             \"${{PATH-unset}}\" \"${{XPC_SERVICE_NAME-unset}}\" >> '{}'\n",
            claude_env_log().display()
        ) + r#"printf '%s\n' "$*" >> "$0.argv"
answer() { if [ -f "$0.$1" ]; then cat "$0.$1"; else printf '%b' "$2"; fi; }
case "$1" in
--version) answer version '2.1.268 (Claude Code)\n'; exit "$(cat "$0.version_exit" 2>/dev/null || echo 0)" ;;
auth) while [ -f "$0.auth_hold" ]; do sleep 0.02; done
      answer auth '{\n  "loggedIn": true\n}\n'; exit "$(cat "$0.auth_exit" 2>/dev/null || echo 0)" ;;
-p) [ -f "$0.ask" ] || exit 1; cat "$0.ask"; cat > /dev/null ;;
esac
"#,
    )
    .unwrap();
    std::fs::write(
        home.join(".profile"),
        format!("PATH='{}:/usr/bin:/bin'; export PATH\n", bin.display()),
    )
    .unwrap();
    std::process::Command::new("chmod")
        .arg("755")
        .arg(&claude)
        .status()
        .unwrap();
    let environment = HashMap::from([
        (OsString::from("HOME"), home.into_os_string()),
        (OsString::from("SHELL"), OsString::from("/bin/sh")),
        (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
    ]);
    (claude, environment)
}

/// A codex for tests beside `claude`, a [`test_claude`] claude, so on the `PATH` its
/// environment's login shell sets. It answers nothing and exits 1, so a test daemon
/// whose `codex_bin` names it lists Codex as unavailable and never reaches a codex of
/// the person running the tests.
#[cfg(test)]
pub(crate) fn test_codex(claude: &Path) -> PathBuf {
    let codex = claude.with_file_name("codex");
    std::fs::write(&codex, "#!/bin/sh\nexit 1\n").unwrap();
    std::process::Command::new("chmod")
        .arg("755")
        .arg(&codex)
        .status()
        .unwrap();
    codex
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// What the rig's stand-in agent host says as it ends, a start's answer once its
    /// checks have let it through to a Claude run.
    const HOST_SAYS: &str = "the stand-in host ends here";

    /// What the rig's stand-in codex says as it ends, a start's answer once its checks
    /// have let it through to a Codex run.
    const CODEX_SAYS: &str = "the stand-in codex ends here";

    /// A folder for one test, short and under `/tmp` so a run's socket fits `sun_path`:
    /// a project folder; a stub `codeconnect` running `body`, which the probe tests also
    /// run as a stand-in agent; a ready Claude Code ([`test_claude`]) with the environment
    /// every command of the test runs in; and a daemon over the rig's store whose starts
    /// run stand-ins for the agent host and for codex. Each stand-in keeps its runs, its
    /// folder (the host) or its arguments (codex), then ends saying [`HOST_SAYS`] or
    /// [`CODEX_SAYS`].
    struct Rig {
        root: PathBuf,
        db: crate::db::Db,
        daemon: Arc<Daemon>,
        project: PathBuf,
        launcher: PathBuf,
        claude: PathBuf,
        env: HashMap<OsString, OsString>,
        requests: std::sync::atomic::AtomicUsize,
    }

    impl Rig {
        fn new(tag: &str, body: &str) -> Rig {
            let uid = protocol::uid::new().unwrap();
            let root = PathBuf::from(format!("/tmp/ccs-{tag}.{}", &uid[20..]));
            let project = root.join("my app");
            std::fs::create_dir_all(&project).unwrap();
            let store = Arc::new(crate::store::Store::open(&root.join("events.db")).unwrap());
            let program = |name: &str, script: String| {
                let path = root.join(name);
                std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
                std::process::Command::new("chmod")
                    .arg("755")
                    .arg(&path)
                    .status()
                    .unwrap();
                path
            };
            let launcher = program("codeconnect", body.to_string());
            let host = program(
                "stand-in-host",
                format!("echo run >> \"$0.runs\"; pwd -P > \"$0.cwd\"; echo '{HOST_SAYS}'; exit 1"),
            );
            let codex = program(
                "stand-in-codex",
                format!(
                    "printf 'HOME=%s PATH=%s XPC_SERVICE_NAME=%s X2_MARKER=%s\\n' \"${{HOME-unset}}\" \
                     \"${{PATH-unset}}\" \"${{XPC_SERVICE_NAME-unset}}\" \"${{X2_MARKER-unset}}\" >> \"$0.env\"\n\
                     [ \"$1\" = --version ] && {{ echo 'codex-cli 0.167.0'; exit 0; }}\n\
                     [ \"$*\" = 'login status' ] && exit 1\n\
                     [ \"$*\" = app-server ] && exit 1\n\
                     echo run >> \"$0.runs\"; printf '%s\\0' \"$@\" > \"$0.argv\"; echo '{CODEX_SAYS}'; exit 1"
                ),
            );
            let (claude, env) = test_claude(&root.join("claude-rig"));
            // An agy the stand-in host is handed for an Antigravity start; never run here.
            let agy = program("stand-in-agy", "exit 1".into());
            let daemon = Daemon::new(
                protocol::config::Config {
                    claude_bin: Some(claude.display().to_string()),
                    codex_bin: Some(codex.display().to_string()),
                    agent_host_bin: Some(host.display().to_string()),
                    agy_bin: Some(agy.display().to_string()),
                    ..Default::default()
                },
                Arc::clone(&store),
                Arc::new(crate::apns::LoggingPushSender::new()),
                crate::state::Endpoint {
                    host: "test.ts.net".into(),
                    port: 8787,
                    tls: false,
                },
            );
            Rig {
                db: crate::db::Db::new(store),
                daemon,
                root,
                project,
                launcher,
                claude,
                env,
                requests: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        /// Make the stub claude answer `what` (`version`, `version_exit`, `auth`,
        /// `auth_exit` or `ask`) with `text`.
        fn claude_says(&self, what: &str, text: &str) {
            std::fs::write(format!("{}.{what}", self.claude.display()), text).unwrap();
        }

        /// Every run of the stub claude, one line of arguments each.
        fn claude_runs(&self) -> String {
            std::fs::read_to_string(format!("{}.argv", self.claude.display())).unwrap_or_default()
        }

        /// A start of `agent` in `path` with the first message `prompt`, in `worktree`,
        /// giving `trust` as the folder's answer, through the agent's own arm
        /// ([`claude_start`] or [`codex_start`]) under `request_id`.
        async fn start_request(
            &self,
            request_id: &str,
            agent: AgentKind,
            path: &str,
            prompt: &str,
            worktree: Option<&StartWorktree>,
            trust: Option<Trust>,
        ) -> StartSessionResult {
            match agent {
                AgentKind::Codex => {
                    codex_start(
                        &self.daemon,
                        Ok(self.launcher.clone()),
                        &self.root,
                        request_id,
                        path,
                        prompt,
                        worktree,
                        None,
                        &self.env,
                        None,
                        trust,
                    )
                    .await
                }
                agent => {
                    hosted_start(
                        agent,
                        &self.daemon,
                        Ok(self.launcher.clone()),
                        &self.root,
                        request_id,
                        path,
                        prompt,
                        worktree,
                        None,
                        &self.env,
                        None,
                        trust,
                    )
                    .await
                }
            }
        }

        /// [`Rig::start_request`] under a request id of its own.
        async fn start_as(
            &self,
            agent: AgentKind,
            path: &str,
            prompt: &str,
            worktree: Option<&StartWorktree>,
            trust: Option<Trust>,
        ) -> StartSessionResult {
            let n = self
                .requests
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.start_request(&format!("req-{n}"), agent, path, prompt, worktree, trust)
                .await
        }

        /// A Claude start in `path`, its folder answered.
        async fn start(&self, path: &str, prompt: Option<&str>) -> StartSessionResult {
            let prompt = prompt.unwrap_or("fix the tests");
            self.start_as(AgentKind::Claude, path, prompt, None, Some(Trust::Trusted))
                .await
        }

        /// A Claude start in `path` with a first message, giving `trust` as the folder's
        /// answer, or none.
        async fn start_answering(&self, path: &str, trust: Option<Trust>) -> StartSessionResult {
            self.start_as(AgentKind::Claude, path, "fix the tests", None, trust)
                .await
        }

        /// A start of `agent` in `path`'s `worktree`, its folder answered.
        async fn start_in(
            &self,
            path: &str,
            agent: AgentKind,
            worktree: StartWorktree,
        ) -> StartSessionResult {
            let trust = Some(Trust::Trusted);
            self.start_as(agent, path, "fix the tests", Some(&worktree), trust)
                .await
        }

        /// The worktree on `branch`, as git lists it.
        async fn worktree_on(&self, branch: &str) -> crate::git::WorktreeEntry {
            crate::git::worktrees(&self.project(), None, GIT_READ_TIMEOUT)
                .await
                .unwrap()
                .into_iter()
                .find(|entry| entry.branch.as_deref() == Some(branch))
                .unwrap()
        }

        /// The project as a repository: `main` at commit A, a linked worktree on
        /// `wt/existing` one commit ahead at B, a worktree whose folder is gone, and
        /// branches whose names a shell would read as code.
        fn make_repository(&self) {
            git(&self.project, &["init", "-q", "-b", "main"]);
            git(&self.project, &["commit", "-q", "--allow-empty", "-m", "A"]);
            for branch in BRANCHES {
                git(&self.project, &["branch", branch]);
            }
            let existing = self.existing().display().to_string();
            git(
                &self.project,
                &["worktree", "add", "-q", "-b", "wt/existing", &existing],
            );
            git(
                &self.existing(),
                &["commit", "-q", "--allow-empty", "-m", "B"],
            );
            let gone = self.root.join("gone");
            let gone_path = gone.display().to_string();
            git(
                &self.project,
                &["worktree", "add", "-q", "-b", "wt/gone", &gone_path],
            );
            std::fs::remove_dir_all(&gone).unwrap();
        }

        fn existing(&self) -> PathBuf {
            self.root.join("wt existing")
        }

        /// What `git` itself calls this repository's main folder.
        fn main(&self) -> String {
            git(
                &self.project,
                &["rev-parse", "--path-format=absolute", "--show-toplevel"],
            )
        }

        /// Everything about the main folder a start must leave alone.
        fn main_state(&self) -> String {
            [
                git(&self.project, &["rev-parse", "HEAD"]),
                git(&self.project, &["symbolic-ref", "HEAD"]),
                git(&self.project, &["status", "--porcelain=v2", "--branch"])
                    .lines()
                    .filter(|line| *line != "? .claude/")
                    .collect::<Vec<_>>()
                    .join("\n"),
            ]
            .join("\n")
        }

        /// How many runs the starts reached: the stand-in host's and the stand-in codex's.
        fn runs(&self) -> usize {
            ["stand-in-host.runs", "stand-in-codex.runs"]
                .iter()
                .map(|name| {
                    std::fs::read_to_string(self.root.join(name))
                        .map(|runs| runs.lines().count())
                        .unwrap_or(0)
                })
                .sum()
        }

        /// The folder the stand-in host last ran in, a Claude run's folder.
        fn host_cwd(&self) -> PathBuf {
            let cwd = std::fs::read_to_string(self.root.join("stand-in-host.cwd")).unwrap();
            PathBuf::from(cwd.trim())
        }

        /// The arguments the stand-in codex was last launched with.
        fn codex_argv(&self) -> Vec<String> {
            let argv = std::fs::read(self.root.join("stand-in-codex.argv")).unwrap();
            argv.split(|byte| *byte == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
                .collect()
        }

        /// The folder the run of `request_id` was recorded to run in.
        async fn run_cwd(&self, request_id: &str) -> String {
            self.db
                .hosted_run_for_request(request_id.to_string())
                .await
                .unwrap()
                .expect("the start wrote its row")
                .cwd
        }

        fn project(&self) -> String {
            self.project.display().to_string()
        }
    }

    /// Branches a start must hand git as they are.
    const BRANCHES: [&str; 3] = [
        "unicode-ünï-☕-分支",
        "sub$(touch${IFS}PWNED)",
        "feature/search",
    ];

    /// `git -C dir <args>` as a person would run it, its output trimmed.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A daemon for a Claude start's arm: its store, `test_claude`'s claude and a stand-in
    /// host in `root`, a folder of the test's own under `/tmp` (removed by the caller).
    fn hosted_daemon(root: &Path, claude: &Path, host: &Path) -> Arc<Daemon> {
        let store = crate::store::Store::open(&root.join("events.db")).unwrap();
        Daemon::new(
            protocol::config::Config {
                claude_bin: Some(claude.display().to_string()),
                agent_host_bin: Some(host.display().to_string()),
                ..Default::default()
            },
            Arc::new(store),
            Arc::new(crate::apns::LoggingPushSender::new()),
            crate::state::Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
        )
    }

    fn arm_root(tag: &str) -> PathBuf {
        let uid = protocol::uid::new().unwrap();
        let root = PathBuf::from(format!("/tmp/cca{tag}.{}", &uid[16..]));
        std::fs::create_dir_all(root.join("project")).unwrap();
        root
    }

    /// The arm's way to the run: a Claude start whose claude is ready gets its row,
    /// under the frame's request id, and its host is spawned in the run's folder.
    #[tokio::test]
    async fn a_claude_start_gets_its_row_and_its_host() {
        let root = arm_root("1");
        let (claude, environment) = test_claude(&root);
        let host = root.join("stand-in-host");
        std::fs::write(&host, "#!/bin/sh\ntouch \"$1/reached\"\n").unwrap();
        std::process::Command::new("chmod")
            .arg("755")
            .arg(&host)
            .status()
            .unwrap();
        let daemon = hosted_daemon(&root, &claude, &host);
        let project = root.join("project");
        let result = hosted_start(
            AgentKind::Claude,
            &daemon,
            Ok(root.join("codeconnect")),
            &root,
            "req-arm",
            project.to_str().unwrap(),
            "fix the tests",
            None,
            None,
            &environment,
            None,
            Some(Trust::Trusted),
        )
        .await;
        let run = daemon
            .db
            .hosted_run_for_request("req-arm".into())
            .await
            .unwrap()
            .expect("the start wrote its row");
        let reached = crate::claude_host::run_dir(&root, &run.session_uid)
            .join("reached")
            .exists();
        let checked = std::fs::read_to_string(claude.with_extension("argv")).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            matches!(result, StartSessionResult::Failed { .. }),
            "{result:?}"
        );
        assert_eq!(run.lifecycle, protocol::event::Lifecycle::Failed);
        assert!(reached, "the host was not spawned in the run's folder");
        assert!(
            checked.contains("--version"),
            "the start did not check its claude: {checked}"
        );
    }

    /// **B10: a run starts with the folder's answer** (decision 157). A Claude start's
    /// stored start line carries the answer it resolved for the folder it names: the one
    /// given with the start, else the one kept, either way round. For every start kind, the
    /// folder the run runs in lowers that answer and never raises it: one answered not
    /// trusted runs not trusted; one answered trusted under a start folder answered not
    /// trusted runs not trusted; one with no answer of its own keeps the start folder's.
    #[tokio::test]
    async fn a_claude_start_carries_its_folders_answer() {
        let root = arm_root("5");
        let (claude, environment) = test_claude(&root);
        let host = root.join("stand-in-host");
        std::fs::write(&host, "#!/bin/sh\n").unwrap();
        std::process::Command::new("chmod")
            .arg("755")
            .arg(&host)
            .status()
            .unwrap();
        let daemon = hosted_daemon(&root, &claude, &host);
        let started = |request_id: String,
                       path: String,
                       place: Option<StartWorktree>,
                       trust: Option<Trust>| {
            let (daemon, root, environment) = (&daemon, &root, &environment);
            async move {
                hosted_start(
                    AgentKind::Claude,
                    daemon,
                    Ok(root.join("codeconnect")),
                    root,
                    &request_id,
                    &path,
                    "fix the tests",
                    place.as_ref(),
                    None,
                    environment,
                    None,
                    trust,
                )
                .await;
                let run = daemon
                    .db
                    .hosted_run_for_request(request_id)
                    .await
                    .unwrap()
                    .expect("the start wrote its row");
                let line = daemon
                    .db
                    .hosted_link(run.session_uid)
                    .await
                    .unwrap()
                    .and_then(|link| link.start)
                    .expect("the start stored its line");
                match serde_json::from_str(&line).unwrap() {
                    protocol::host::ToHost::Start { options, .. } => options,
                    other => panic!("not a start: {other:?}"),
                }
            }
        };
        let path = root.join("project").to_str().unwrap().to_string();
        let given = started("req-given".into(), path.clone(), None, Some(Trust::Trusted)).await;
        let kept = started("req-kept".into(), path.clone(), None, None).await;
        let changed = started(
            "req-changed".into(),
            path.clone(),
            None,
            Some(Trust::NotTrusted),
        )
        .await;
        let kept_again = started("req-kept-again".into(), path, None, None).await;

        // Every start kind, each row its own repository: the folder its run runs in is the
        // start folder, an existing worktree, a branch's worktree, or a new worktree's folder
        // in Claude's layout, there or not there (removed, its answer kept). The answers are
        // kept ones: the start folder's by its real path, the run folder's by its real path,
        // or by its own path while it is not there.
        #[derive(Debug)]
        enum Kind {
            StartFolder,
            Existing,
            Branch,
            New { there: bool },
        }
        use Trust::{NotTrusted as Not, Trusted as Yes};
        let rows = [
            // (the start kind, the start folder's answer, the run folder's own, the run's)
            (Kind::StartFolder, Not, None, Not),
            (Kind::Existing, Yes, Some(Not), Not),
            (Kind::Branch, Yes, Some(Not), Not),
            (Kind::New { there: true }, Yes, Some(Not), Not),
            (Kind::New { there: false }, Yes, Some(Not), Not),
            // The mirror: a run folder answered trusted never raises the start folder's.
            (Kind::Existing, Not, Some(Yes), Not),
            (Kind::Branch, Not, Some(Yes), Not),
            (Kind::New { there: true }, Not, Some(Yes), Not),
            (Kind::New { there: false }, Not, Some(Yes), Not),
            // A run folder with no answer of its own keeps the start folder's.
            (Kind::StartFolder, Yes, None, Yes),
            (Kind::Existing, Yes, None, Yes),
            (Kind::Branch, Yes, None, Yes),
            (Kind::New { there: true }, Yes, None, Yes),
            (Kind::New { there: false }, Yes, None, Yes),
        ];
        let mut wrong = Vec::new();
        for (row, (kind, start, own, runs)) in rows.iter().enumerate() {
            let project = root.join(format!("row{row}"));
            std::fs::create_dir(&project).unwrap();
            git(&project, &["init", "-q", "-b", "main"]);
            git(&project, &["commit", "-q", "--allow-empty", "-m", "A"]);
            let project = std::fs::canonicalize(&project).unwrap();
            let main = project.to_str().unwrap();
            let (place, folder) = match kind {
                Kind::StartFolder => (None, project.clone()),
                Kind::Existing => {
                    let folder = root.join(format!("row{row}-wt"));
                    git(
                        &project,
                        &[
                            "worktree",
                            "add",
                            "-q",
                            "-b",
                            "wt",
                            folder.to_str().unwrap(),
                        ],
                    );
                    let folder = std::fs::canonicalize(folder).unwrap();
                    let path = folder.to_str().unwrap().to_string();
                    (Some(StartWorktree::Existing { path }), folder)
                }
                Kind::Branch => {
                    git(&project, &["branch", "fix/b10"]);
                    let branch = "fix/b10".to_string();
                    (
                        Some(StartWorktree::Branch { branch }),
                        worktree_folder(main, "fix/b10"),
                    )
                }
                Kind::New { there } => {
                    let folder = worktree_folder(main, "feat/b10");
                    if *there {
                        git(
                            &project,
                            &[
                                "worktree",
                                "add",
                                "-q",
                                "-b",
                                "worktree-feat",
                                folder.to_str().unwrap(),
                            ],
                        );
                    }
                    let name = "feat/b10".to_string();
                    (Some(StartWorktree::New { name }), folder)
                }
            };
            daemon
                .db
                .set_folder_trust(main.to_string(), *start)
                .await
                .unwrap();
            if let Some(own) = own {
                let folder = folder.to_str().unwrap().to_string();
                daemon.db.set_folder_trust(folder, *own).await.unwrap();
            }
            let ran = started(format!("req-row{row}"), main.to_string(), place, None).await;
            if ran.trust != *runs {
                wrong.push(format!(
                    "{kind:?}, start {start:?}, own {own:?}: ran {:?}",
                    ran.trust
                ));
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(given.trust, Trust::Trusted, "the answer given");
        assert_eq!(kept.trust, Trust::Trusted, "the answer kept");
        assert_eq!(changed.trust, Trust::NotTrusted, "the answer given");
        assert_eq!(kept_again.trust, Trust::NotTrusted, "the answer kept");
        assert!(
            wrong.is_empty(),
            "rows that ran with the wrong answer: {wrong:#?}"
        );
    }

    /// B10 G3's project, `<root>/project`, a git repository with its project checked in: a
    /// SessionStart hook writing `<root>/hook-canary`, a project command `g10canary`, a
    /// CLAUDE.md asking every reply to end with ZEBRA-G10, an allow rule for one Bash
    /// command (`touch <root>/allow-canary`), and a stdio MCP server in .mcp.json writing
    /// `<root>/mcp-canary` when claude starts it. By its real path.
    fn g3_project(root: &Path) -> PathBuf {
        let project = root.join("project");
        std::fs::create_dir_all(project.join(".claude/commands")).unwrap();
        let canary = |name: &str| root.join(name).display().to_string();
        let settings = serde_json::json!({
            "hooks": {"SessionStart": [{"hooks": [
                {"type": "command", "command": format!("touch {}", canary("hook-canary"))}
            ]}]},
            "permissions": {"allow": [format!("Bash(touch {})", canary("allow-canary"))]},
        });
        std::fs::write(project.join(".claude/settings.json"), settings.to_string()).unwrap();
        std::fs::write(
            project.join(".claude/commands/g10canary.md"),
            "Say the word G10.\n",
        )
        .unwrap();
        std::fs::write(
            project.join("CLAUDE.md"),
            "End every reply with the word ZEBRA-G10.\n",
        )
        .unwrap();
        let mcp = serde_json::json!({"mcpServers": {"g10mcp": {
            "type": "stdio", "command": "/bin/sh",
            "args": ["-c", format!("touch {}; exec cat", canary("mcp-canary"))],
        }}});
        std::fs::write(project.join(".mcp.json"), mcp.to_string()).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(&project, &["add", "-A"]);
        git(&project, &["commit", "-q", "-m", "G3"]);
        std::fs::canonicalize(project).unwrap()
    }

    /// One B10 G3 run and what it showed by its first reply.
    struct G3 {
        daemon: Arc<Daemon>,
        root: PathBuf,
        uid: String,
        /// The text of the run's first reply a model wrote, empty when none came within 90 s.
        reply: String,
        /// Whether the run's initialize answer lists the project command `g10canary`.
        listed: bool,
        /// The folder claude replied from, as its own transcript records it.
        ran_in: Option<String>,
    }

    /// **B10 G3's start**: a real, signed-in claude on the real agent host, started as the
    /// app starts one, through the folder question: in `g3_project` under
    /// `CODECONNECT_TEST_RIG_DIR/<name>`, its answer `trust` given with the start; with
    /// `worktree`, in a new worktree of that name whose folder is already there (checked out
    /// from the project, so its project is there too) and answered not trusted in its own
    /// right. The host and claude get this test's `HOME`, `USER`, `LOGNAME`, `PATH` and
    /// `CLAUDE_CONFIG_DIR`, as B7's G2 gives them.
    async fn g3_start(name: &str, trust: Trust, worktree: Option<&str>) -> G3 {
        let var =
            |name: &str| std::env::var_os(name).unwrap_or_else(|| panic!("{name} is not set"));
        let host = PathBuf::from(var("CODECONNECT_TEST_AGENT_HOST"));
        let claude = PathBuf::from(var("CODECONNECT_TEST_REAL_CLAUDE"));
        let config = PathBuf::from(var("CLAUDE_CONFIG_DIR"));
        let root = PathBuf::from(var("CODECONNECT_TEST_RIG_DIR")).join(name);
        std::fs::create_dir(&root).unwrap();
        let project = g3_project(&root);
        let daemon = hosted_daemon(&root, &claude, &host);
        let mut environment: HashMap<OsString, OsString> = [
            "HOME",
            "USER",
            "LOGNAME",
            "PATH",
            "CLAUDE_CONFIG_DIR",
            "DISABLE_AUTOUPDATER",
        ]
        .into_iter()
        .map(|name| (OsString::from(name), var(name)))
        .collect();
        environment.insert(OsString::from("SHELL"), OsString::from("/bin/zsh"));
        let place = match worktree {
            Some(name) => {
                let folder = worktree_folder(project.to_str().unwrap(), name);
                let branch = format!("worktree-{name}");
                git(
                    &project,
                    &[
                        "worktree",
                        "add",
                        "-q",
                        "-b",
                        &branch,
                        folder.to_str().unwrap(),
                    ],
                );
                let folder = std::fs::canonicalize(folder).unwrap();
                let folder = folder.to_str().unwrap().to_string();
                daemon
                    .db
                    .set_folder_trust(folder, Trust::NotTrusted)
                    .await
                    .unwrap();
                Some(StartWorktree::New {
                    name: name.to_string(),
                })
            }
            None => None,
        };
        let started = hosted_start(
            AgentKind::Claude,
            &daemon,
            Ok(root.join("codeconnect")),
            &root,
            name,
            project.to_str().unwrap(),
            "Say hello in one short sentence.",
            place.as_ref(),
            None,
            &environment,
            None,
            Some(trust),
        )
        .await;
        let StartSessionResult::Started { session_uid, .. } = &started else {
            panic!("not started: {started:?}");
        };
        let uid = session_uid.clone();
        let store = rusqlite::Connection::open(root.join("events.db")).unwrap();
        let mut reply = String::new();
        for _ in 0..900 {
            let mut statement = store
                .prepare(
                    "SELECT payload FROM events WHERE session_uid = ?1 AND kind = 'agent_message'
                      ORDER BY seq",
                )
                .unwrap();
            let payloads: Vec<String> = statement
                .query_map([&uid], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            reply = payloads
                .iter()
                .map(|payload| serde_json::from_str::<serde_json::Value>(payload).unwrap())
                // Claude writes an API error as its own "<synthetic>" message: no model answered.
                .filter(|payload| payload["message"]["model"] != "<synthetic>")
                .map(|payload| {
                    let blocks = payload["message"]["content"].as_array().cloned();
                    let texts = blocks.into_iter().flatten();
                    texts
                        .filter_map(|block| block["text"].as_str().map(str::to_string))
                        .collect::<String>()
                })
                .find(|text| !text.is_empty())
                .unwrap_or_default();
            if !reply.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let (initialize, session_id): (Option<String>, Option<String>) = store
            .query_row(
                "SELECT h.initialize, s.claude_session_id
                   FROM run_hosts AS h JOIN sessions AS s USING(session_uid)
                  WHERE session_uid = ?1",
                [&uid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let listed = initialize.is_some_and(|json| {
            let answer: serde_json::Value = serde_json::from_str(&json).unwrap();
            let commands = answer["commands"].as_array().cloned();
            commands
                .into_iter()
                .flatten()
                .any(|command| command["name"] == "g10canary")
        });
        let ran_in = session_id.and_then(|id| {
            let name = format!("{id}.jsonl");
            let transcript = std::fs::read_dir(config.join("projects"))
                .ok()?
                .filter_map(Result::ok)
                .map(|folder| folder.path().join(&name))
                .find(|path| path.is_file())?;
            std::fs::read_to_string(transcript)
                .ok()?
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|line| line["type"] == "assistant")?["cwd"]
                .as_str()
                .map(str::to_string)
        });
        G3 {
            daemon,
            root,
            uid,
            reply,
            listed,
            ran_in,
        }
    }

    /// The G3 run ended as an app ends it, its host's group gone, and its folder removed.
    async fn g3_end(g3: &G3) {
        let host_file = crate::claude_host::run_dir(&g3.root, &g3.uid).join("host.json");
        let host_pid = std::fs::read_to_string(host_file)
            .ok()
            .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
            .and_then(|file| file["host_pid"].as_i64())
            .and_then(|pid| i32::try_from(pid).ok());
        let _ = g3.daemon.stop_run(&g3.uid).await;
        if let Some(pid) = host_pid {
            let mut gone = false;
            for _ in 0..150 {
                // SAFETY: kill(2) with signal 0 only asks whether the group has a member.
                gone = unsafe { libc::kill(-pid, 0) } != 0;
                if gone {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !gone {
                // SAFETY: kill(2) of the process group this test's host leads.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
        }
        let _ = std::fs::remove_dir_all(&g3.root);
    }

    /// **B10 G3, trusted** (decision 157): a start from a folder answered trusted loads its
    /// project into a real claude: its SessionStart hook runs, its command is listed, its
    /// CLAUDE.md shapes the reply. Then one more message asks for the allowed Bash command,
    /// and whether it ran within 30 s is printed as a fact, as is whether the .mcp.json
    /// server was started. Ignored and gated as B7's G2 is.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_folder_answered_trusted_loads_its_project_into_a_real_claude() {
        let g3 = g3_start("g3t", Trust::Trusted, None).await;
        let hook = g3.root.join("hook-canary").exists();
        let mcp = g3.root.join("mcp-canary").exists();
        println!(
            "G3 trusted: hook canary {hook}; g10canary listed {}; ran in {:?}; reply {:?}",
            g3.listed, g3.ran_in, g3.reply
        );
        println!("G3 fact: .mcp.json server started {mcp}");
        let checks = (hook, g3.listed, g3.reply.contains("ZEBRA-G10"));
        if checks == (true, true, true) {
            let allow = g3.root.join("allow-canary");
            let text = format!(
                "Run the Bash command `touch {}`, then reply done.",
                allow.display()
            );
            let hash = protocol::hash::send_text_hash(&g3.uid, &text, true);
            let sent = g3
                .daemon
                .send_text(&g3.uid, text, Some("g3t-2"), Some(&hash), true)
                .await;
            let mut ran = false;
            for _ in 0..300 {
                ran = allow.exists();
                if ran {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            println!("G3 fact: sent {sent:?}; the allowed command ran within 30 s {ran}");
        }
        g3_end(&g3).await;
        assert_eq!(
            checks,
            (true, true, true),
            "hook canary, g10canary listed, ZEBRA-G10 in the reply"
        );
    }

    /// **B10 G3, not trusted** (decision 157): a start from a folder answered not trusted
    /// loads nothing of its project into a real claude: no hook canary, `g10canary` not
    /// listed, no ZEBRA-G10 in the reply, and the .mcp.json server not started
    /// (`--strict-mcp-config`). Ignored and gated as B7's G2 is.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_folder_answered_not_trusted_loads_nothing_of_its_project_into_a_real_claude() {
        let g3 = g3_start("g3u", Trust::NotTrusted, None).await;
        let hook = g3.root.join("hook-canary").exists();
        let mcp = g3.root.join("mcp-canary").exists();
        println!(
            "G3 not trusted: hook canary {hook}; g10canary listed {}; mcp canary {mcp}; ran in {:?}; reply {:?}",
            g3.listed, g3.ran_in, g3.reply
        );
        g3_end(&g3).await;
        assert!(
            !g3.reply.is_empty(),
            "the run never replied, so the reply's canary proves nothing"
        );
        assert_eq!(
            (hook, g3.listed, g3.reply.contains("ZEBRA-G10"), mcp),
            (false, false, false, false),
            "hook canary, g10canary listed, ZEBRA-G10 in the reply, mcp canary"
        );
    }

    /// **B10 G3, a new worktree's folder reopened**: from a folder answered trusted, a new
    /// worktree whose folder, `.claude/worktrees/<name>` in Claude's layout, is already there
    /// and answered not trusted in its own right. A real claude reopens that folder (its
    /// transcript's `cwd`) and loads nothing of the project checked out there. Ignored and
    /// gated as B7's G2 is.
    #[tokio::test]
    #[ignore = "needs the built agent host and a signed-in claude: CODECONNECT_TEST_AGENT_HOST, CODECONNECT_TEST_REAL_CLAUDE, CODECONNECT_TEST_RIG_DIR"]
    async fn a_new_worktree_reopens_its_declined_folder_and_loads_nothing_into_a_real_claude() {
        let g3 = g3_start("g3w", Trust::Trusted, Some("g3")).await;
        let reopened = std::fs::canonicalize(g3.root.join("project/.claude/worktrees/g3"))
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let hook = g3.root.join("hook-canary").exists();
        println!(
            "G3 new worktree: ran in {:?}, the folder {reopened:?}; hook canary {hook}; g10canary listed {}; reply {:?}",
            g3.ran_in, g3.listed, g3.reply
        );
        g3_end(&g3).await;
        assert!(
            !g3.reply.is_empty(),
            "the run never replied, so the reply's canary proves nothing"
        );
        assert_eq!(
            g3.ran_in.as_deref(),
            Some(reopened.as_str()),
            "claude reopened the folder that was there"
        );
        assert_eq!(
            (hook, g3.listed, g3.reply.contains("ZEBRA-G10")),
            (false, false, false),
            "hook canary, g10canary listed, ZEBRA-G10 in the reply"
        );
    }

    /// A signed-out claude is refused with B9's sentence, before any row or folder.
    #[tokio::test]
    async fn a_signed_out_claude_start_is_refused_before_its_row() {
        let root = arm_root("2");
        let (claude, environment) = test_claude(&root);
        std::fs::write(
            claude.with_extension("auth"),
            "{\n  \"loggedIn\": false\n}\n",
        )
        .unwrap();
        std::fs::write(claude.with_extension("auth_exit"), "1").unwrap();
        let host = root.join("stand-in-host");
        std::fs::write(&host, "#!/bin/sh\n").unwrap();
        let daemon = hosted_daemon(&root, &claude, &host);
        let project = root.join("project");
        let result = hosted_start(
            AgentKind::Claude,
            &daemon,
            Ok(root.join("codeconnect")),
            &root,
            "req-out",
            project.to_str().unwrap(),
            "fix the tests",
            None,
            None,
            &environment,
            None,
            Some(Trust::Trusted),
        )
        .await;
        let row = daemon
            .db
            .hosted_run_for_request("req-out".into())
            .await
            .unwrap();
        let folder = root.join("runs").exists();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            matches!(&result, StartSessionResult::Failed { detail, .. } if detail.contains("isn’t signed in")),
            "{result:?}"
        );
        assert_eq!(row, None, "a refused start wrote a row");
        assert!(!folder, "a refused start made a run folder");
    }

    /// **A Claude start asks the folder question first.** In a folder nobody has
    /// answered for, it is answered `needs_trust` with the folder's real path before
    /// its claude is checked, and leaves no row and no run folder. Answered, the same
    /// start goes on to the check and its row.
    #[tokio::test]
    async fn a_claude_start_asks_the_folder_question_first() {
        let root = arm_root("5");
        let (claude, environment) = test_claude(&root);
        let host = root.join("stand-in-host");
        std::fs::write(&host, "#!/bin/sh\n").unwrap();
        std::process::Command::new("chmod")
            .arg("755")
            .arg(&host)
            .status()
            .unwrap();
        let daemon = hosted_daemon(&root, &claude, &host);
        let project = root.join("project");
        let real = std::fs::canonicalize(&project)
            .unwrap()
            .display()
            .to_string();
        let start = |request_id: &'static str, trust: Option<Trust>| {
            hosted_start(
                AgentKind::Claude,
                &daemon,
                Ok(root.join("codeconnect")),
                &root,
                request_id,
                project.to_str().unwrap(),
                "fix the tests",
                None,
                None,
                &environment,
                None,
                trust,
            )
        };
        let asked = start("req-ask", None).await;
        let asked_row = daemon
            .db
            .hosted_run_for_request("req-ask".into())
            .await
            .unwrap();
        let asked_folder = root.join("runs").exists();
        let checked_before = claude.with_extension("argv").exists();
        start("req-answered", Some(Trust::Trusted)).await;
        let answered_row = daemon
            .db
            .hosted_run_for_request("req-answered".into())
            .await
            .unwrap();
        let checked = std::fs::read_to_string(claude.with_extension("argv")).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(asked, StartSessionResult::NeedsTrust { path: real });
        assert_eq!(asked_row, None, "an unanswered start wrote a row");
        assert!(!asked_folder, "an unanswered start made a run folder");
        assert!(
            !checked_before,
            "the claude was checked before the folder was answered for"
        );
        assert!(answered_row.is_some(), "the answered start wrote no row");
        assert!(
            checked.contains("--version"),
            "the answered start did not check its claude: {checked}"
        );
    }

    /// A Claude start the host or the launcher refuses leaves the repository as it was:
    /// those refusals come before the place, so a start on a branch makes no worktree.
    #[tokio::test]
    async fn a_refused_claude_start_on_a_branch_makes_no_worktree() {
        let root = arm_root("3");
        let (claude, environment) = test_claude(&root);
        let project = root.join("project");
        git(&project, &["init", "-q", "-b", "main"]);
        git(&project, &["commit", "-q", "--allow-empty", "-m", "A"]);
        git(&project, &["branch", "feature"]);
        let host = root.join("stand-in-host");
        std::fs::write(&host, "#!/bin/sh\n").unwrap();
        let daemon = hosted_daemon(&root, &claude, &host);
        let result = hosted_start(
            AgentKind::Claude,
            &daemon,
            Err("no codeconnect here".into()),
            &root,
            "req-branch",
            project.to_str().unwrap(),
            "fix the tests",
            Some(&StartWorktree::Branch {
                branch: "feature".into(),
            }),
            None,
            &environment,
            None,
            Some(Trust::Trusted),
        )
        .await;
        let worktrees = git(&project, &["worktree", "list", "--porcelain"]);
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            matches!(&result, StartSessionResult::Failed { detail, .. } if detail == "no codeconnect here"),
            "{result:?}"
        );
        assert_eq!(
            worktrees.matches("worktree ").count(),
            1,
            "a refused start made a worktree: {worktrees}"
        );
    }

    /// The claude check and the host see one environment, the start's own: a sign-in
    /// variable the check saw reaches the host unchanged, both see the same home, and
    /// neither has a Claude config folder.
    #[tokio::test]
    async fn the_check_and_the_host_see_one_environment() {
        let root = arm_root("4");
        let (claude, mut environment) = test_claude(&root);
        environment.insert(
            OsString::from("ANTHROPIC_API_KEY"),
            OsString::from("sk-test-marker"),
        );
        // The stand-in claude also records the environment the check runs it in.
        let script = std::fs::read_to_string(&claude).unwrap();
        let (shebang, body) = script.split_once('\n').unwrap();
        std::fs::write(&claude, format!("{shebang}\nenv > \"$0.env\"\n{body}")).unwrap();
        let host = root.join("stand-in-host");
        std::fs::write(&host, "#!/bin/sh\nenv > \"$1/env.txt\"\n").unwrap();
        std::process::Command::new("chmod")
            .arg("755")
            .arg(&host)
            .status()
            .unwrap();
        let daemon = hosted_daemon(&root, &claude, &host);
        let project = root.join("project");
        hosted_start(
            AgentKind::Claude,
            &daemon,
            Ok(root.join("codeconnect")),
            &root,
            "req-env",
            project.to_str().unwrap(),
            "fix the tests",
            None,
            None,
            &environment,
            None,
            Some(Trust::Trusted),
        )
        .await;
        let run = daemon
            .db
            .hosted_run_for_request("req-env".into())
            .await
            .unwrap()
            .expect("the start wrote its row");
        let checked = std::fs::read_to_string(claude.with_extension("env")).unwrap_or_default();
        let hosted = std::fs::read_to_string(
            crate::claude_host::run_dir(&root, &run.session_uid).join("env.txt"),
        )
        .unwrap_or_default();
        let _ = std::fs::remove_dir_all(&root);
        let line = |seen: &str, name: &str| {
            seen.lines()
                .find(|line| line.starts_with(&format!("{name}=")))
                .map(str::to_string)
        };
        for name in ["ANTHROPIC_API_KEY", "HOME"] {
            assert!(
                line(&checked, name).is_some(),
                "the check saw no {name}: {checked}"
            );
            assert_eq!(line(&checked, name), line(&hosted, name), "{name} differs");
        }
        assert_eq!(
            line(&hosted, "ANTHROPIC_API_KEY").as_deref(),
            Some("ANTHROPIC_API_KEY=sk-test-marker")
        );
        assert!(!checked.contains("CLAUDE_CONFIG_DIR") && !hosted.contains("CLAUDE_CONFIG_DIR"));
    }

    /// Whether `result` is a start its checks let through to its run: the run's
    /// stand-in ended it, saying so.
    fn reached(result: &StartSessionResult) -> bool {
        matches!(result, StartSessionResult::Failed { detail, .. } if detail == HOST_SAYS || detail == CODEX_SAYS)
    }

    /// **The reason a launcher gives reads the same wherever it is shown**: its
    /// last line, after the shell's noise, without the `Error: ` it prints first.
    #[test]
    fn a_launcher_reason_is_its_last_line_without_the_error_prefix() {
        let stderr =
            b"bash: cannot set terminal process group (1): Inappropriate ioctl for device\n\
            bash: no job control in this shell\n\
            Error: could not find the claude binary: install Claude Code\n\n";
        assert_eq!(
            last_line(stderr).as_deref(),
            Some("could not find the claude binary: install Claude Code")
        );
        assert_eq!(last_line(b"  \n\n"), None);
    }

    /// **A double tap starts one session**: the second ask waits for the first
    /// and gets its answer.
    #[tokio::test]
    async fn the_same_request_id_starts_one_session() {
        let rig = counting("once");
        let starts = Starts::default();
        let path = rig.project();
        let (first, second) = tokio::join!(
            starts.once("r1", rig.start(&path, None)),
            starts.once("r1", rig.start(&path, None)),
        );
        let again = starts.once("r1", rig.start(&path, None)).await;
        assert_eq!(first, second);
        assert_eq!(first, again);
        assert!(reached(&first), "{first:?}");
        assert_eq!(rig.runs(), 1);
    }

    /// A rig whose starts reach their stand-ins.
    fn counting(tag: &str) -> Rig {
        Rig::new(tag, "exit 1")
    }

    /// **A folder is asked about once.** The first start there, with no answer,
    /// is answered `needs_trust` with the folder's real path, and runs nothing. A
    /// start that answers runs and keeps the answer; a later one without an answer
    /// runs on it. Forgetting it asks again.
    #[tokio::test]
    async fn a_folder_is_asked_about_once_and_again_once_forgotten() {
        let rig = counting("trust");
        let path = rig.project();
        let real = std::fs::canonicalize(&rig.project)
            .unwrap()
            .display()
            .to_string();
        let asked = StartSessionResult::NeedsTrust { path: real.clone() };
        assert_eq!(rig.start_answering(&path, None).await, asked);
        assert_eq!(rig.runs(), 0, "an unanswered start runs nothing");

        let answered = rig.start_answering(&path, Some(Trust::NotTrusted)).await;
        assert!(reached(&answered), "{answered:?}");
        assert_eq!(
            rig.db.folder_trust(real.clone()).await.unwrap(),
            Some(Trust::NotTrusted)
        );
        let later = rig.start_answering(&path, None).await;
        assert!(reached(&later), "{later:?}");
        assert_eq!(rig.runs(), 2);

        assert!(rig.db.forget_folder_trust(real.clone()).await.unwrap());
        assert_eq!(rig.start_answering(&path, None).await, asked);
        assert_eq!(rig.runs(), 2, "asked again, and nothing ran");
    }

    /// **The folder question comes before the claude check.** A folder nobody has
    /// answered for runs nothing at all, not even the check of its claude, which runs
    /// processes in the person's login shell. Once it is answered, the check runs and
    /// its refusal is the start's answer, so the check is never skipped either.
    #[tokio::test]
    async fn the_folder_question_comes_before_the_claude_check() {
        let rig = counting("trust-order");
        std::fs::write(
            rig.claude.with_extension("auth"),
            "{\n  \"loggedIn\": false\n}\n",
        )
        .unwrap();
        std::fs::write(rig.claude.with_extension("auth_exit"), "1").unwrap();
        let path = rig.project();
        let asked = rig.start_answering(&path, None).await;
        assert!(
            matches!(asked, StartSessionResult::NeedsTrust { .. }),
            "{asked:?}"
        );
        assert!(
            !rig.claude.with_extension("argv").exists(),
            "the claude was checked before the folder was answered for"
        );
        let answered = rig.start_answering(&path, Some(Trust::Trusted)).await;
        assert!(
            matches!(&answered, StartSessionResult::Failed { detail, .. } if detail.contains("isn’t signed in")),
            "{answered:?}"
        );
        assert_eq!(rig.runs(), 0, "a signed-out claude's start ran");
    }

    /// **A folder reached through a symlink is the same folder**: the answer given
    /// through the link is kept under the real path and found from it.
    #[tokio::test]
    async fn a_folder_reached_through_a_symlink_has_its_one_answer() {
        let rig = counting("trust-link");
        let link = rig.root.join("link to my app");
        std::os::unix::fs::symlink(&rig.project, &link).unwrap();
        let through_link = rig
            .start_answering(&link.display().to_string(), Some(Trust::Trusted))
            .await;
        assert!(reached(&through_link), "{through_link:?}");
        let direct = rig.start_answering(&rig.project(), None).await;
        assert!(reached(&direct), "{direct:?}");
        let kept = rig.db.folder_trusts().await.unwrap();
        let real = std::fs::canonicalize(&rig.project).unwrap();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].path, real.display().to_string());
        assert_eq!(kept[0].trust, Trust::Trusted);
    }

    /// **The answered start is a request of its own.** The asking request keeps its
    /// `needs_trust`; the answering one starts once, and asked again starts nothing
    /// new.
    #[tokio::test]
    async fn the_answered_start_is_a_request_of_its_own() {
        let rig = counting("trust-ids");
        let starts = Starts::default();
        let path = rig.project();
        let asked = starts.once("r1", rig.start_answering(&path, None)).await;
        assert!(
            matches!(asked, StartSessionResult::NeedsTrust { .. }),
            "{asked:?}"
        );
        let answered = starts
            .once("r2", rig.start_answering(&path, Some(Trust::Trusted)))
            .await;
        assert!(reached(&answered), "{answered:?}");
        assert_eq!(
            starts
                .once("r1", rig.start_answering(&path, Some(Trust::Trusted)))
                .await,
            asked
        );
        assert_eq!(
            starts
                .once("r2", rig.start_answering(&path, Some(Trust::Trusted)))
                .await,
            answered
        );
        assert_eq!(rig.runs(), 1);
    }

    /// **Each failure says which one it is**, and none of them runs anything.
    #[tokio::test]
    async fn a_failed_start_says_why() {
        let reason = |result: StartSessionResult| match result {
            StartSessionResult::Failed { reason, detail } => (reason, detail),
            other => panic!("expected a failure, got {other:?}"),
        };

        let rig = recording("relative");
        assert_eq!(
            reason(rig.start("my app", None).await),
            (
                StartFailure::LaunchFailed,
                "\"my app\" is not an absolute path".into()
            )
        );
        let file = rig.root.join("notes.txt");
        std::fs::write(&file, "").unwrap();
        let (why, _) = reason(rig.start(&file.display().to_string(), None).await);
        assert_eq!(why, StartFailure::FolderMissing);

        let long = "x".repeat(protocol::ws::MAX_COMPOSE_BYTES + 1);
        assert_eq!(
            reason(rig.start(&rig.project(), Some(&long)).await),
            (
                StartFailure::LaunchFailed,
                "the first message is longer than 8 KiB".into()
            )
        );

        std::fs::remove_dir_all(&rig.project).unwrap();
        let (why, _) = reason(rig.start(&rig.project(), None).await);
        assert_eq!(why, StartFailure::FolderMissing);
        assert_eq!(rig.runs(), 0, "a refused start runs nothing");
    }

    /// A folder that exists but cannot be reached is refused in the system's
    /// words, not called missing; one behind a file is still missing.
    #[tokio::test]
    async fn a_start_in_a_folder_it_cannot_reach_says_why() {
        use std::os::unix::fs::PermissionsExt;
        let rig = recording("unreachable");
        let locked = rig.root.join("locked");
        let inside = locked.join("app");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o000)).unwrap();
        let result = rig.start(&inside.display().to_string(), None).await;
        std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(
            refusal(result),
            format!("{}: Permission denied", inside.display())
        );
        let file = rig.root.join("plain");
        std::fs::write(&file, "").unwrap();
        let behind_a_file = file.join("app").display().to_string();
        assert!(matches!(
            rig.start(&behind_a_file, None).await,
            StartSessionResult::Failed {
                reason: StartFailure::FolderMissing,
                ..
            }
        ));
        assert_eq!(rig.runs(), 0, "a refused start runs nothing");
    }

    /// A rig whose project is a repository ([`Rig::make_repository`]).
    fn recording(tag: &str) -> Rig {
        let rig = counting(tag);
        rig.make_repository();
        rig
    }

    fn refusal(result: StartSessionResult) -> String {
        match result {
            StartSessionResult::Failed {
                reason: StartFailure::LaunchFailed,
                detail,
            } => detail,
            other => panic!("expected launch_failed, got {other:?}"),
        }
    }

    /// **An existing worktree is started in as it is**: the run is recorded there, and
    /// git is not written to.
    #[tokio::test]
    async fn an_existing_worktree_is_started_in_as_it_is() {
        let rig = recording("existing");
        let existing = rig.worktree_on("wt/existing").await.path;
        let listed = git(&rig.project, &["worktree", "list", "--porcelain"]);
        let before = rig.main_state();

        let worktree = StartWorktree::Existing {
            path: existing.clone(),
        };
        let trust = Some(Trust::Trusted);
        let result = rig
            .start_request(
                "existing",
                AgentKind::Codex,
                &rig.project(),
                "fix the tests",
                Some(&worktree),
                trust,
            )
            .await;
        assert!(reached(&result), "{result:?}");
        assert_eq!(rig.run_cwd("existing").await, existing);
        assert_eq!(
            git(&rig.project, &["worktree", "list", "--porcelain"]),
            listed
        );
        assert_eq!(rig.main_state(), before);

        let gone = rig.worktree_on("wt/gone").await.path;
        let elsewhere = rig.root.display().to_string();
        for (path, says) in [
            (gone, "no longer exists"),
            (elsewhere, "is not a worktree of this repository"),
        ] {
            let detail = refusal(
                rig.start_in(
                    &rig.project(),
                    AgentKind::Claude,
                    StartWorktree::Existing { path },
                )
                .await,
            );
            assert!(detail.contains(says), "{detail}");
        }
        assert_eq!(rig.runs(), 1, "a refused start runs nothing");
    }

    /// **A branch gets a worktree of its own, on that branch, in Claude Code's
    /// layout**, its name reaching git as it is, and the main folder is untouched.
    #[tokio::test]
    async fn a_branch_gets_its_own_worktree_and_the_main_folder_stays_as_it_was() {
        let rig = recording("branch");
        let main = rig.main();
        let before = rig.main_state();

        for branch in BRANCHES {
            let result = rig
                .start_in(
                    &rig.project(),
                    AgentKind::Claude,
                    StartWorktree::Branch {
                        branch: branch.into(),
                    },
                )
                .await;
            let folder = Path::new(&main)
                .join(".claude/worktrees")
                .join(branch.replace('/', "+"));
            assert!(reached(&result), "{result:?}");
            assert_eq!(git(&folder, &["symbolic-ref", "--short", "HEAD"]), branch);
            assert_eq!(rig.host_cwd(), folder.canonicalize().unwrap());
        }
        let pwned = std::process::Command::new("find")
            .arg(&rig.root)
            .args(["-name", "PWNED"])
            .output()
            .unwrap();
        assert!(pwned.stdout.is_empty(), "nothing in a branch name may run");
        assert_eq!(rig.main_state(), before);

        for (branch, says) in [
            ("wt/existing", "already checked out"),
            ("wt/gone", "already checked out"),
            ("main", "already checked out"),
            ("no-such-branch", "is not a local branch"),
        ] {
            let detail = refusal(
                rig.start_in(
                    &rig.project(),
                    AgentKind::Codex,
                    StartWorktree::Branch {
                        branch: branch.into(),
                    },
                )
                .await,
            );
            assert!(detail.contains(says), "{branch}: {detail}");
        }
        assert_eq!(rig.runs(), BRANCHES.len());
    }

    /// **A new Codex worktree is made where Claude Code makes one, on a branch named
    /// as typed**, from the commit of the folder it was asked in: here a linked
    /// worktree one commit ahead of main. A second one by the same name is git's to
    /// refuse.
    #[tokio::test]
    async fn a_new_codex_worktree_starts_from_the_folder_it_is_asked_in() {
        let rig = recording("new-codex");
        let existing = rig.worktree_on("wt/existing").await.path;
        let main = rig.main();
        let ahead = git(Path::new(&existing), &["rev-parse", "HEAD"]);
        assert_ne!(ahead, git(&rig.project, &["rev-parse", "main"]));

        let new = || StartWorktree::New {
            name: "fix/login".into(),
        };
        let trust = Some(Trust::Trusted);
        let result = rig
            .start_request(
                "new",
                AgentKind::Codex,
                &existing,
                "fix it",
                Some(&new()),
                trust,
            )
            .await;
        let folder = Path::new(&main).join(".claude/worktrees/fix+login");
        assert!(reached(&result), "{result:?}");
        assert_eq!(rig.run_cwd("new").await, folder.display().to_string());
        assert_eq!(
            git(&folder, &["symbolic-ref", "--short", "HEAD"]),
            "fix/login"
        );
        assert_eq!(git(&folder, &["rev-parse", "HEAD"]), ahead);

        let detail = refusal(rig.start_in(&existing, AgentKind::Codex, new()).await);
        assert!(detail.contains("already exists"), "{detail}");
        assert_eq!(rig.runs(), 1);
    }

    /// **A repeated Codex start on a branch finds its run and makes no second worktree**:
    /// the request id is looked up in the store before the place, so the repeat is the
    /// run's own answer, not git's refusal of a branch checked out already.
    #[tokio::test]
    async fn a_repeated_codex_start_on_a_branch_finds_its_run() {
        let rig = recording("repeat-branch");
        let branch = StartWorktree::Branch {
            branch: "feature/search".into(),
        };
        let (project, trust) = (rig.project(), Some(Trust::Trusted));
        let start = || {
            rig.start_request(
                "r-branch",
                AgentKind::Codex,
                &project,
                "fix it",
                Some(&branch),
                trust,
            )
        };
        let first = start().await;
        assert!(reached(&first), "{first:?}");
        let again = start().await;
        assert_eq!(
            again,
            failed(StartFailure::LaunchFailed, "did not start".into())
        );
        assert_eq!(rig.runs(), 1);
    }

    /// **A Codex start with no `codeconnect` to run its mail server is refused before its
    /// row**, with nothing launched.
    #[tokio::test]
    async fn a_codex_start_with_no_codeconnect_is_refused_before_its_row() {
        let rig = counting("no-codeconnect");
        let result = codex_start(
            &rig.daemon,
            Err("no codeconnect here".into()),
            &rig.root,
            "r-no-cc",
            &rig.project(),
            "fix it",
            None,
            None,
            &rig.env,
            None,
            Some(Trust::Trusted),
        )
        .await;
        assert_eq!(
            result,
            failed(StartFailure::LaunchFailed, "no codeconnect here".into())
        );
        assert_eq!(
            rig.db
                .hosted_run_for_request("r-no-cc".into())
                .await
                .unwrap(),
            None
        );
        assert_eq!(rig.runs(), 0);
    }

    /// **A Codex start's check and its launch see one environment**, the start's own:
    /// the codex check's `--version` and the launched codex run with the same HOME, PATH
    /// and session variables, and a variable only the start's environment has.
    #[tokio::test]
    async fn a_codex_start_checks_and_launches_from_one_environment() {
        let mut rig = counting("codex-env");
        rig.env.insert("X2_MARKER".into(), "x2-env-marker".into());
        let result = rig
            .start_as(
                AgentKind::Codex,
                &rig.project(),
                "fix it",
                None,
                Some(Trust::Trusted),
            )
            .await;
        assert!(reached(&result), "{result:?}");
        let seen = std::fs::read_to_string(rig.root.join("stand-in-codex.env")).unwrap();
        let lines: Vec<&str> = seen.lines().collect();
        assert_eq!(
            lines.len(),
            4,
            "the check, its `login status` and its ask for the models, and the launch: {seen}"
        );
        assert!(lines.iter().all(|line| *line == lines[0]), "{seen}");
        assert!(lines[0].ends_with("X2_MARKER=x2-env-marker"), "{seen}");
    }

    /// **A Codex start whose ask for the models never answers still starts**, within the
    /// ask's own bound and with no models kept, its run offering its modes alone, as a
    /// start did before it asked.
    #[tokio::test]
    async fn a_codex_start_whose_ask_never_answers_still_starts() {
        let rig = counting("codex-ask-hangs");
        let codex = rig.root.join("stand-in-codex");
        std::fs::write(
            &codex,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n--version) echo 'codex-cli 0.167.0' ;;\n\
             \"login status\") exit 0 ;;\napp-server) exec sleep 60 ;;\n\
             *) echo run >> \"$0.runs\"; echo '{CODEX_SAYS}'; exit 1 ;;\nesac\n"
            ),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let result = rig
            .start_request(
                "r-hangs",
                AgentKind::Codex,
                &rig.project(),
                "fix it",
                None,
                Some(Trust::Trusted),
            )
            .await;
        let took = started.elapsed();
        assert!(reached(&result), "{result:?}");
        assert!(
            took < PROBE_TIMEOUT + Duration::from_secs(5),
            "took {took:?}"
        );
        assert_eq!(rig.runs(), 1, "the run was launched");
        let run = rig
            .db
            .hosted_run_for_request("r-hangs".into())
            .await
            .unwrap()
            .expect("the start's run");
        assert_eq!(
            rig.db.hosted_models(run.session_uid).await.unwrap(),
            Some(Vec::new())
        );
    }

    /// **A Codex start with no list before it offers the models Codex lists** (97, 11-18):
    /// with no answer kept since ccd started, the start asks this codex as a list asks it,
    /// so the run keeps Codex's models for its settings menu, whoever asked first; a second
    /// start takes the signed-in answer kept, and asks nothing more.
    #[tokio::test]
    async fn a_codex_start_with_no_list_before_it_keeps_the_models_codex_lists() {
        let rig = counting("codex-no-list");
        let codex = rig.root.join("stand-in-codex");
        std::fs::write(
            &codex,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.asks\"\ncase \"$*\" in\n\
             --version) echo 'codex-cli 0.167.0' ;;\n\"login status\") exit 0 ;;\n\
             app-server) cat \"$0.codex\"; exec sleep 60 ;;\n*) exit 1 ;;\nesac\n",
        )
        .unwrap();
        let models_only: String = CODEX_API_KEY
            .lines()
            .filter(|line| !line.contains(r#""id":3"#))
            .map(|line| format!("{line}\n"))
            .collect();
        codex_says(&codex, "codex", &models_only);
        async fn kept(rig: &Rig, request_id: &str) -> Option<Vec<serde_json::Value>> {
            let run = rig
                .db
                .hosted_run_for_request(request_id.into())
                .await
                .unwrap()
                .expect("the start's run");
            rig.db.hosted_models(run.session_uid).await.unwrap()
        }
        let (project, trust) = (rig.project(), Some(Trust::Trusted));
        rig.start_request("r-first", AgentKind::Codex, &project, "fix it", None, trust)
            .await;
        let first = kept(&rig, "r-first").await.expect("the run keeps models");
        assert_eq!(first.len(), 9, "Codex's listed models: {first:?}");
        let menus = crate::codex_host::menus(&first);
        assert_eq!(menus.models[0].value, "gpt-6.1-sol");
        assert_eq!(menus.models[0].label, "GPT-6.1-Sol");
        assert!(!menus.models[0].efforts.is_empty());
        let asked = codex_runs(&codex);
        assert_eq!(
            asked.lines().filter(|line| *line == "app-server").count(),
            1,
            "{asked}"
        );

        rig.start_request(
            "r-second",
            AgentKind::Codex,
            &project,
            "fix it",
            None,
            trust,
        )
        .await;
        assert_eq!(kept(&rig, "r-second").await, Some(first));
        let again = codex_runs(&codex);
        assert_eq!(
            again.lines().filter(|line| *line == "app-server").count(),
            1,
            "the kept signed-in answer is taken, nothing asked: {again}"
        );
        assert_eq!(
            again.lines().filter(|line| *line == "login status").count(),
            1,
            "{again}"
        );
    }

    /// The `-c projects=` list the stand-in codex was last launched with.
    fn launched_projects(rig: &Rig) -> String {
        rig.codex_argv()
            .into_iter()
            .find(|arg| arg.starts_with("projects="))
            .expect("the launch names its trust list")
    }

    /// **XT G1: a Codex start launches its folders at the person's answer**, given or
    /// kept: `trusted` given, then kept, then `not_trusted` given.
    #[tokio::test]
    async fn a_codex_start_launches_its_folders_at_the_folder_answer() {
        let rig = counting("codex-trust");
        let mut launched = Vec::new();
        for trust in [Some(Trust::Trusted), None, Some(Trust::NotTrusted)] {
            let result = rig
                .start_as(AgentKind::Codex, &rig.project(), "fix it", None, trust)
                .await;
            assert!(reached(&result), "{trust:?}: {result:?}");
            launched.push(launched_projects(&rig));
        }
        let real = std::fs::canonicalize(&rig.project)
            .unwrap()
            .display()
            .to_string();
        let list =
            |level: &str| format!(r#"projects={{ "{real}" = {{ trust_level = "{level}" }} }}"#);
        assert_eq!(
            launched,
            [list("trusted"), list("trusted"), list("untrusted")]
        );
    }

    /// **XT G2: a Codex run in a worktree with its own `not_trusted` answer runs untrusted**
    /// though its start folder is trusted; with no answer of its own it runs trusted.
    #[tokio::test]
    async fn a_codex_worktree_with_its_own_not_trusted_answer_runs_untrusted() {
        let rig = recording("codex-worktree-trust");
        let existing = rig.worktree_on("wt/existing").await.path;
        let worktree = StartWorktree::Existing {
            path: existing.clone(),
        };
        let project = rig.project();
        let start = || {
            rig.start_as(
                AgentKind::Codex,
                &project,
                "fix it",
                Some(&worktree),
                Some(Trust::Trusted),
            )
        };
        assert!(reached(&start().await));
        let own_none = launched_projects(&rig);
        let real = std::fs::canonicalize(&existing).unwrap();
        rig.db
            .set_folder_trust(real.to_string_lossy().into_owned(), Trust::NotTrusted)
            .await
            .unwrap();
        assert!(reached(&start().await));
        let own_declined = launched_projects(&rig);
        assert!(
            own_none.contains(r#"trust_level = "trusted""#) && !own_none.contains("untrusted"),
            "{own_none}"
        );
        assert!(
            own_declined.contains(r#"trust_level = "untrusted""#)
                && !own_declined.contains(r#"trust_level = "trusted""#),
            "{own_declined}"
        );
    }

    /// **A new Antigravity worktree is made as a Codex one is** (S3, row 6): agy has no worktree flag of its own, so
    /// ccd makes it, where Claude Code makes one, on a branch named as typed, and the run starts in it.
    #[tokio::test]
    async fn a_new_antigravity_worktree_is_made_as_a_codex_one_is() {
        let rig = recording("new-agy");
        let main = rig.main();
        let result = rig
            .start_in(
                &rig.project(),
                AgentKind::Antigravity,
                StartWorktree::New {
                    name: "fix/agy".into(),
                },
            )
            .await;
        assert!(reached(&result), "{result:?}");
        let folder = Path::new(&main).join(".claude/worktrees/fix+agy");
        assert_eq!(rig.host_cwd(), folder.canonicalize().unwrap());
        assert_eq!(
            git(&folder, &["symbolic-ref", "--short", "HEAD"]),
            "fix/agy"
        );
    }

    /// **A new Claude worktree is Claude's own to make**: the run starts in the folder
    /// it was asked in, and the daemon makes nothing.
    #[tokio::test]
    async fn a_new_claude_worktree_is_left_to_claude() {
        let rig = recording("new-claude");
        let before = rig.main_state();
        let result = rig
            .start_in(
                &rig.project(),
                AgentKind::Claude,
                StartWorktree::New {
                    name: "-fix/login".into(),
                },
            )
            .await;
        assert!(reached(&result), "{result:?}");
        assert_eq!(rig.host_cwd(), rig.project.canonicalize().unwrap());
        assert!(!Path::new(&rig.main()).join(".claude").exists());
        assert_eq!(rig.main_state(), before);
    }

    /// **A name Claude Code would refuse is refused before anything runs**, for
    /// both agents.
    #[tokio::test]
    async fn a_worktree_name_claude_would_refuse_is_refused_before_anything_runs() {
        let rig = recording("names");
        let branches = git(&rig.project, &["branch", "--list"]);
        let too_long = "x".repeat(MAX_WORKTREE_NAME_BYTES + 1);
        for name in [
            "semi;",
            "has space",
            "ünï",
            "..",
            "a//b",
            "a/./b",
            "",
            "trailing/",
            too_long.as_str(),
        ] {
            for agent in [AgentKind::Claude, AgentKind::Codex] {
                let detail = refusal(
                    rig.start_in(
                        &rig.project(),
                        agent,
                        StartWorktree::New { name: name.into() },
                    )
                    .await,
                );
                assert!(
                    detail.contains("is not a valid worktree name"),
                    "{name:?}: {detail}"
                );
            }
        }
        assert!(check_worktree_name(&"x".repeat(MAX_WORKTREE_NAME_BYTES)).is_ok());
        assert_eq!(rig.runs(), 0);
        assert!(!Path::new(&rig.main()).join(".claude").exists());
        assert_eq!(git(&rig.project, &["branch", "--list"]), branches);
    }

    /// **Outside a repository there is no worktree to pick**, and it says so.
    #[tokio::test]
    async fn a_folder_outside_a_repository_has_no_worktrees() {
        let rig = counting("plain");
        let detail = refusal(
            rig.start_in(
                &rig.project(),
                AgentKind::Claude,
                StartWorktree::Branch {
                    branch: "main".into(),
                },
            )
            .await,
        );
        assert!(detail.contains("is not in a git repository"), "{detail}");
    }

    /// The folder the listing and creation tests browse, as the phone would find a
    /// real one: folders with awkward names, a hidden one, symlinks to a folder, to
    /// nothing and to a file, a file, a folder nobody may read, one with more
    /// folders than a listing holds, and an empty one to make folders in.
    struct Browse {
        root: PathBuf,
    }

    impl Browse {
        const FOLDERS: [&'static str; 11] = [
            "a",
            "B",
            "10",
            "2",
            ".hidden",
            "ünï ☕",
            "semi;",
            "-dash",
            "$(touch PWNED)",
            "new\nline",
            "new",
        ];

        fn new(tag: &str) -> Browse {
            let root = std::env::temp_dir().join(format!(
                "ccd-browse-{tag}-{}-{}",
                std::process::id(),
                protocol::time::now_unix_ms()
            ));
            for folder in Self::FOLDERS {
                std::fs::create_dir_all(root.join(folder)).unwrap();
            }
            std::os::unix::fs::symlink(root.join("a"), root.join("link")).unwrap();
            std::os::unix::fs::symlink(root.join("nowhere"), root.join("dead")).unwrap();
            std::fs::write(root.join("file.txt"), "").unwrap();
            std::os::unix::fs::symlink(root.join("file.txt"), root.join("flink")).unwrap();
            for index in 0..=protocol::ws::MAX_FOLDER_NAMES {
                std::fs::create_dir_all(root.join("big").join(format!("f{index:05}"))).unwrap();
            }
            let locked = root.join("locked");
            std::fs::create_dir(&locked).unwrap();
            std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000))
                .unwrap();
            Browse {
                root: root.canonicalize().unwrap(),
            }
        }

        fn at(&self, name: &str) -> String {
            self.root.join(name).display().to_string()
        }
    }

    impl Drop for Browse {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(
                self.root.join("locked"),
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            );
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn listed(contents: FolderContents) -> (Vec<String>, bool) {
        match contents {
            FolderContents::Listed { folders, truncated } => (folders, truncated),
            other => panic!("expected a listing, got {other:?}"),
        }
    }

    fn unreadable(contents: FolderContents) -> String {
        match contents {
            FolderContents::Unreadable { error } => error,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// **A listing is the folders `ls` would show**, symlinks to folders among
    /// them, in byte order, dot names only when asked for, at its real path; and
    /// what cannot be read says so in the words `ls` uses.
    #[test]
    fn a_listing_holds_the_folders_a_terminal_would_see() {
        let browse = Browse::new("list");
        let (path, contents) = list_folder(&browse.at(""), false);
        assert_eq!(path, browse.root.display().to_string());
        let (folders, truncated) = listed(contents);
        assert_eq!(
            folders,
            [
                "$(touch PWNED)",
                "-dash",
                "10",
                "2",
                "B",
                "a",
                "big",
                "link",
                "locked",
                "new",
                "new\nline",
                "semi;",
                "ünï ☕"
            ]
        );
        assert!(!truncated);

        let (_, contents) = list_folder(&browse.at(""), true);
        let (with_hidden, _) = listed(contents);
        let mut expected = folders.clone();
        expected.insert(2, ".hidden".into());
        assert_eq!(with_hidden, expected);

        let (_, contents) = list_folder(&browse.at("big"), false);
        let (big, truncated) = listed(contents);
        assert_eq!(big.len(), protocol::ws::MAX_FOLDER_NAMES);
        assert_eq!(big.first().map(String::as_str), Some("f00000"));
        assert_eq!(big.last().map(String::as_str), Some("f01999"));
        assert!(truncated);

        // Through a symlink, the folder is named by where it really is.
        let (path, _) = list_folder(&browse.at("link"), false);
        assert_eq!(path, browse.at("a"));

        let (path, contents) = list_folder(&browse.at("locked"), false);
        assert_eq!(path, browse.at("locked"));
        assert_eq!(unreadable(contents), "Permission denied");
        let (_, contents) = list_folder(&browse.at("nope"), false);
        assert_eq!(unreadable(contents), "No such file or directory");
        let (_, contents) = list_folder(&browse.at("file.txt"), false);
        assert_eq!(unreadable(contents), "Not a directory");
        let (path, contents) = list_folder("rel/x", false);
        assert_eq!(path, "rel/x");
        assert_eq!(unreadable(contents), "\"rel/x\" is not an absolute path");
    }

    /// The root has a listing of its own, and with no path the home folder is
    /// listed (`answer` fills it in): the phone starts there.
    #[test]
    fn the_root_and_the_home_folder_list() {
        let _env = crate::ProcessEnvRead::take();
        let (path, contents) = list_folder("/", false);
        assert_eq!(path, "/");
        assert!(!listed(contents).0.is_empty());
        let home = protocol::home_dir().display().to_string();
        let (path, _) = list_folder(&home, false);
        assert_eq!(
            Path::new(&path),
            protocol::home_dir().canonicalize().unwrap()
        );
    }

    /// A Linux name that is not UTF-8 cannot be named on the wire, so it is left
    /// out and the rest still list (written miss).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_name_that_is_not_utf8_is_left_out() {
        use std::os::unix::ffi::OsStrExt;
        let browse = Browse::new("bytes");
        std::fs::create_dir(browse.root.join(OsStr::from_bytes(b"bad\xff"))).unwrap();
        let (folders, _) = listed(list_folder(&browse.at(""), false).1);
        assert_eq!(folders.len(), 13, "{folders:?}");
    }

    /// **A new folder is one `mkdir`**: the name exactly as given, the mode `mkdir`
    /// gives, and every refusal in `mkdir`'s own words; only a `/` in the name is
    /// refused before the call.
    #[test]
    fn a_new_folder_is_made_as_mkdir_makes_it() {
        let browse = Browse::new("create");
        let parent = browse.at("new");
        for name in ["x", "-x", ".x", "semi;", "ünï ☕", "$(touch PWNED)"] {
            assert_eq!(
                create_folder(&parent, name),
                CreateFolderResult::Created,
                "{name}"
            );
            assert!(browse.root.join("new").join(name).is_dir(), "{name}");
        }
        let made: Vec<String> = std::fs::read_dir(browse.root.join("new"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(made.len(), 6, "{made:?}");
        assert!(!browse.root.join("new").join("PWNED").exists());

        let by_mkdir = browse.root.join("by-mkdir");
        assert!(std::process::Command::new("mkdir")
            .arg(&by_mkdir)
            .status()
            .unwrap()
            .success());
        let mode = |path: &Path| {
            std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(path).unwrap().permissions())
        };
        assert_eq!(mode(&browse.root.join("new").join("x")), mode(&by_mkdir));

        let failure = |parent: &str, name: &str| match create_folder(parent, name) {
            CreateFolderResult::Failed { detail } => detail,
            CreateFolderResult::Created => panic!("{name} was made"),
        };
        assert_eq!(failure(&parent, "x"), "File exists");
        for name in ["", ".", ".."] {
            assert_eq!(failure(&parent, name), "File exists", "{name:?}");
        }
        assert_eq!(
            failure(&parent, "a/b"),
            "\"a/b\" is not a single folder name"
        );
        assert!(!browse.root.join("new").join("a").exists());
        assert_eq!(failure(&browse.at("locked"), "y"), "Permission denied");
        assert_eq!(
            failure(&browse.at("nope"), "y"),
            "No such file or directory"
        );
        assert_eq!(failure("rel", "y"), "\"rel\" is not an absolute path");
    }

    /// **A start runs in any folder, as a terminal can `cd` anywhere**, not only one
    /// a session has run in, each name reaching the run as its folder and nothing in
    /// it running: Claude's host runs in it, and codex is told its trust by its real
    /// path, as one argument.
    #[tokio::test]
    async fn a_start_runs_in_any_folder() {
        let rig = recording("anywhere");
        let marker = rig.root.join("PWNED");
        for name in ["never listed ünï ☕", "$(touch PWNED)", "new\nline"] {
            let folder = rig.root.join(name);
            std::fs::create_dir(&folder).unwrap();
            let path = folder.display().to_string();
            let real = folder.canonicalize().unwrap();
            for agent in [AgentKind::Claude, AgentKind::Codex] {
                let request_id = format!("{name}-{}", agent.as_str());
                let trust = Some(Trust::Trusted);
                let prompt = "x; $(touch PWNED) \"q\"";
                let result = rig
                    .start_request(&request_id, agent.clone(), &path, prompt, None, trust)
                    .await;
                assert!(reached(&result), "{name}: {result:?}");
                assert_eq!(rig.run_cwd(&request_id).await, path, "{name}");
                if agent == AgentKind::Claude {
                    assert_eq!(rig.host_cwd(), real, "{name}");
                } else {
                    let projects = serde_json::to_string(&real.display().to_string()).unwrap();
                    let argv = rig.codex_argv();
                    assert!(
                        argv.iter().any(|arg| arg.contains(&projects)),
                        "{name}: {argv:?}"
                    );
                }
            }
        }
        assert!(
            !marker.exists(),
            "nothing in a folder name or prompt may run"
        );
        assert_eq!(rig.runs(), 6);
    }

    /// **What one account offered is never another's** (N4 G4): an offer kept for Codex on account A is not B's, so a
    /// listing on B at the same version asks B itself on its first reply; and the last check, made on A, does not
    /// answer a listing made on B.
    #[test]
    fn what_one_account_offered_is_never_anothers() {
        let root = std::env::temp_dir().join(format!(
            "ccs-offers.{}",
            &protocol::uid::new().unwrap()[20..]
        ));
        let on = |label: &str| {
            let account = crate::store::Account {
                id: protocol::uid::new().unwrap(),
                agent: "codex".into(),
                label: label.into(),
            };
            crate::accounts::AccountEnv::for_account(&root, &account)
                .unwrap()
                .unwrap()
        };
        let (a, b) = (on("A"), on("B"));
        let known = KnownAgents::default();
        let offer = Offer {
            signed_in: Some(true),
            ..Default::default()
        };
        known.keep(&AgentKind::Codex, Some(&a), "0.167.0", offer.clone());
        assert_eq!(
            known.offered(&AgentKind::Codex, Some(&a), "0.167.0"),
            Some(offer)
        );
        assert_eq!(
            known.offered(&AgentKind::Codex, Some(&b), "0.167.0"),
            None,
            "B is asked itself"
        );
        assert_eq!(
            known.offered(&AgentKind::Codex, None, "0.167.0"),
            None,
            "and so is Default"
        );

        let made_on_a = selections(&[None, Some(a.clone())]);
        known.set(made_on_a.clone(), Vec::new());
        assert_eq!(known.get(&made_on_a), Some(Vec::new()));
        assert_eq!(
            known.get(&selections(&[None, Some(b)])),
            None,
            "a check on A does not answer for B"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **An account's folder is what a launch sees, whatever the login shell exports** (N4 G3). The person's own
    /// start-up file exports a conflicting `CODEX_HOME` and `CLAUDE_SECURESTORAGE_CONFIG_DIR`: a launch on an account
    /// sees the account's folder; a launch on Default sees the file's own value, untouched. The red half: the same
    /// variable set in the environment handed to the shell, as a naive launch would, is overwritten by that file. The
    /// file also exports an API key, which outranks a stored login: a Claude Code account never sees it, Default does.
    #[tokio::test]
    async fn an_accounts_folder_wins_over_the_login_shells_own_export() {
        let root = std::env::temp_dir().join(format!(
            "ccs-account.{}",
            &protocol::uid::new().unwrap()[20..]
        ));
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".profile"),
            "export CODEX_HOME=/conflict/codex\nexport CLAUDE_SECURESTORAGE_CONFIG_DIR=/conflict/claude\n\
             export ANTHROPIC_API_KEY=sk-from-profile\n",
        )
        .unwrap();
        let env = environment(&[
            ("HOME", home.to_str().unwrap()),
            ("SHELL", "/bin/sh"),
            ("PATH", "/usr/bin:/bin"),
        ]);
        let said = |environment: HashMap<OsString, OsString>,
                    account: Option<crate::accounts::AccountEnv>| async move {
            let mut command = login_shell_command(
                &environment,
                account.as_ref(),
                &[
                    OsStr::new("/bin/sh"),
                    OsStr::new("-c"),
                    OsStr::new(
                        "printf '%s %s %s\\n' \"$CODEX_HOME\" \"$CLAUDE_SECURESTORAGE_CONFIG_DIR\" \
                         \"${ANTHROPIC_API_KEY-unset}\"",
                    ),
                ],
            );
            let out = command.stdin(Stdio::null()).output().await.unwrap();
            String::from_utf8(out.stdout)
                .unwrap()
                .lines()
                .last()
                .unwrap_or_default()
                .to_string()
        };
        let account = |agent: &str| crate::store::Account {
            id: protocol::uid::new().unwrap(),
            agent: agent.into(),
            label: "Work".into(),
        };
        let codex = crate::accounts::AccountEnv::for_account(&root, &account("codex"))
            .unwrap()
            .unwrap();
        let claude = crate::accounts::AccountEnv::for_account(&root, &account("claude"))
            .unwrap()
            .unwrap();
        let folder = |env: &crate::accounts::AccountEnv| {
            env.argv()
                .last()
                .unwrap()
                .to_str()
                .unwrap()
                .split_once('=')
                .unwrap()
                .1
                .to_string()
        };

        assert_eq!(
            said(env.clone(), None).await,
            "/conflict/codex /conflict/claude sk-from-profile",
            "Default is the person's own, its key included"
        );
        assert_eq!(
            said(env.clone(), Some(codex.clone())).await,
            format!("{} /conflict/claude sk-from-profile", folder(&codex)),
            "Codex's account wins over the file, and nothing else moves"
        );
        assert_eq!(
            said(env.clone(), Some(claude.clone())).await,
            format!("/conflict/codex {} unset", folder(&claude)),
            "Claude Code's account wins over the file, and the key that would outrank it is gone"
        );
        // Red: set before the shell, the file's export wins and the start would leave its account.
        let mut naive = env.clone();
        naive.insert(OsString::from("CODEX_HOME"), OsString::from(folder(&codex)));
        assert_eq!(
            said(naive, None).await,
            "/conflict/codex /conflict/claude sk-from-profile"
        );
        assert_eq!(
            protocol::fsperm::mode_of(Path::new(&folder(&codex))).unwrap() & 0o777,
            0o700,
            "its folder is private"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A launch on a Codex account reads the person's own Codex config** (306 Slice A, A1): the login shell runs the
    /// agent with the account's home, whose entries but its credential are the person's `~/.codex` ones.
    #[tokio::test]
    async fn a_launch_on_a_codex_account_reads_the_persons_config() {
        let root = std::env::temp_dir().join(format!(
            "ccs-launch.{}",
            &protocol::uid::new().unwrap()[20..]
        ));
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex").join("config.toml"),
            "the person's config",
        )
        .unwrap();
        let env = environment(&[
            ("HOME", home.to_str().unwrap()),
            ("SHELL", "/bin/sh"),
            ("PATH", "/usr/bin:/bin"),
        ]);
        let account = crate::store::Account {
            id: protocol::uid::new().unwrap(),
            agent: "codex".into(),
            label: "Work".into(),
        };
        let account = crate::accounts::AccountEnv::for_account(&root, &account)
            .unwrap()
            .unwrap();
        let out = login_shell_command(
            &env,
            Some(&account),
            &[
                OsStr::new("/bin/sh"),
                OsStr::new("-c"),
                OsStr::new("cat \"$CODEX_HOME/config.toml\""),
            ],
        )
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
        let said = String::from_utf8(out.stdout).unwrap();
        assert_eq!(said.lines().last(), Some("the person's config"), "{said}");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn environment(pairs: &[(&str, &str)]) -> HashMap<OsString, OsString> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    /// **A phone start begins from the session's environment**: the service's own
    /// variables and PATH are gone, the session's values (its PATH, an agent socket)
    /// are there, and the daemon's own location goes with it.
    #[test]
    fn a_phone_start_begins_from_the_session_not_the_service() {
        let own = environment(&[
            ("PATH", "/service/bin:/usr/bin"),
            ("HOME", "/home/me"),
            ("CODECONNECT_LOG", "info"),
            ("CODECONNECT_SERVICE", "com.codeconnect.ccd"),
            ("INVOCATION_ID", "unit-only"),
            ("CODECONNECT_HOME", "/home/me/.cc"),
            ("TMUX_TMPDIR", "/tmp/t"),
        ]);
        let session = environment(&[
            ("PATH", "/usr/local/bin:/usr/bin"),
            ("HOME", "/home/me"),
            ("LANG", "C.UTF-8"),
            ("SSH_AUTH_SOCK", "/run/user/1000/agent"),
        ]);
        assert_eq!(
            phone_environment_from(own.clone(), Some(session)),
            environment(&[
                ("PATH", "/usr/local/bin:/usr/bin"),
                ("HOME", "/home/me"),
                ("LANG", "C.UTF-8"),
                ("SSH_AUTH_SOCK", "/run/user/1000/agent"),
                ("CODECONNECT_HOME", "/home/me/.cc"),
                ("TMUX_TMPDIR", "/tmp/t"),
                ("TERM", "xterm-256color"),
            ])
        );

        // When the session cannot be read, the daemon's own is used, without the
        // service's own variables.
        let fallback = phone_environment_from(own, None);
        assert_eq!(
            fallback.get(OsStr::new("PATH")),
            Some(&"/service/bin:/usr/bin".into())
        );
        for name in SERVICE_ONLY {
            assert!(!fallback.contains_key(OsStr::new(name)), "{name}");
        }
    }

    /// **The UTF-8 ctype is set only when nothing names an encoding**; language and
    /// region are never touched, and an existing TERM stays.
    #[test]
    fn the_utf8_ctype_is_set_only_when_nothing_names_an_encoding() {
        let ctype = |pairs: &[(&str, &str)]| {
            phone_environment_from(environment(pairs), None)
                .get(OsStr::new("LC_CTYPE"))
                .cloned()
        };
        assert_eq!(ctype(&[("HOME", "/h")]), Some(UTF8_CTYPE.into()));
        assert_eq!(ctype(&[("LANG", "")]), Some(UTF8_CTYPE.into()));
        assert_eq!(ctype(&[("LANG", "fr_FR.UTF-8")]), None);
        assert_eq!(ctype(&[("LC_ALL", "C")]), None);
        assert_eq!(
            ctype(&[("LC_CTYPE", "en_US.UTF-8")]),
            Some("en_US.UTF-8".into())
        );
        let kept = phone_environment_from(
            environment(&[("LANG", "fr_FR.UTF-8"), ("TERM", "screen")]),
            None,
        );
        assert_eq!(kept.get(OsStr::new("LANG")), Some(&"fr_FR.UTF-8".into()));
        assert_eq!(kept.get(OsStr::new("TERM")), Some(&"screen".into()));
    }

    /// On macOS the session's PATH is launchd's: the one `launchctl setenv` gave it,
    /// or launchd's default, never the service definition's.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_session_path_on_macos_is_launchds() {
        let _env = crate::ProcessEnvRead::take();
        let own = environment(&[
            ("PATH", "/service/bin:/usr/bin"),
            ("HOME", "/h"),
            ("XPC_SERVICE_NAME", "com.codeconnect.ccd"),
            ("XPC_FLAGS", "1"),
        ]);
        let session = session_environment(&own).expect("launchctl answers");
        let set = std::process::Command::new("/bin/launchctl")
            .args(["getenv", "PATH"])
            .output()
            .unwrap();
        let set = String::from_utf8_lossy(&set.stdout).trim().to_string();
        let want = if set.is_empty() {
            "/usr/bin:/bin:/usr/sbin:/sbin".to_string()
        } else {
            set
        };
        assert_eq!(session.get(OsStr::new("PATH")), Some(&want.into()));
        assert_eq!(session.get(OsStr::new("HOME")), Some(&"/h".into()));
        assert_eq!(
            session.get(OsStr::new("XPC_SERVICE_NAME")),
            Some(&"0".into())
        );
        assert_eq!(session.get(OsStr::new("XPC_FLAGS")), Some(&"0x0".into()));
    }

    const CLAUDE_SIGNED_OUT: &str =
        include_str!("../../../fixtures/claude/initialize-signed-out-2.1.319.jsonl");
    const CLAUDE_API_KEY: &str =
        include_str!("../../../fixtures/claude/initialize-api-key-2.1.319.jsonl");
    /// What Claude Code 2.1.268's `auth status` printed signed out, exiting 1
    /// (app-qa's floor run).
    const AUTH_SIGNED_OUT: &str =
        include_str!("../../../fixtures/claude/auth-status-signed-out-2.1.268.json");
    const CODEX_SIGNED_OUT: &str =
        include_str!("../../../fixtures/codex/app-server-models-signed-out-0.167.0.jsonl");
    const CODEX_API_KEY: &str =
        include_str!("../../../fixtures/codex/app-server-models-api-key-0.167.0.jsonl");

    /// A codex that answers its ask as Codex answered it: with the recorded reply in
    /// `<codex>.codex`, then waiting for its stdin to close, as Codex does; `login
    /// status` with the exit status in `<codex>.login`. Each run goes into
    /// `<codex>.asks`, and its folder into `<codex>.cwd`.
    const ANSWERING: &str = r#"printf '%s\n' "$*" >> "$0.asks"; pwd -P > "$0.cwd"
case "$*" in
"login status") exit "$(cat "$0.login")" ;;
"app-server") [ -f "$0.codex" ] || exit 1; cat "$0.codex"; cat > /dev/null ;;
esac"#;

    fn answer(rig: &Rig, agent: &str, recorded: &str) {
        std::fs::write(format!("{}.{agent}", rig.launcher.display()), recorded).unwrap();
    }

    fn login_status(rig: &Rig, status: u8) {
        std::fs::write(
            format!("{}.login", rig.launcher.display()),
            status.to_string(),
        )
        .unwrap();
    }

    /// How many runs of the launcher began with `what`.
    fn asked(rig: &Rig, what: &str) -> usize {
        std::fs::read_to_string(format!("{}.asks", rig.launcher.display()))
            .unwrap_or_default()
            .lines()
            .filter(|run| run.starts_with(what))
            .count()
    }

    fn said(recorded: &str) -> Vec<serde_json::Value> {
        recorded
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn values(offer: &Offer) -> Vec<&str> {
        offer
            .models
            .iter()
            .map(|model| model.value.as_str())
            .collect()
    }

    /// **What the agents answered, read as what they offer**: their models, in
    /// their order and words, Claude Code's own `default` first, with the model it
    /// is now; and whether Codex is signed in. Claude Code's sign-in is `auth
    /// status`'s, so its answer's `account` says nothing here.
    #[test]
    fn the_agents_answers_say_what_they_offer() {
        let claude = offer_from(&AgentKind::Claude, &said(CLAUDE_SIGNED_OUT)).unwrap();
        assert_eq!(
            values(&claude),
            ["default", "opus", "fable", "sonnet", "haiku"]
        );
        assert_eq!(
            claude.models[..2],
            [
                AgentModel {
                    value: "default".into(),
                    label: "Default (recommended)".into(),
                    detail: Some(
                        "Use the default model (currently Opus 5.5) · $4/$20 per Mtok".into()
                    ),
                },
                AgentModel {
                    value: "opus".into(),
                    label: "Opus".into(),
                    detail: Some(
                        "Opus 5.5 · Best for everyday, complex tasks · $4/$20 per Mtok".into()
                    ),
                },
            ]
        );
        assert_eq!(claude.signed_in, None);
        assert_eq!(
            offer_from(&AgentKind::Claude, &said(CLAUDE_API_KEY))
                .unwrap()
                .signed_in,
            None
        );
        let wire = serde_json::to_value(AgentAvailability {
            agent: AgentKind::Claude,
            available: true,
            detail: None,
            models: claude.models.clone(),
            signed_in: claude.signed_in,
        })
        .unwrap();
        assert_eq!(
            wire["models"][0]["detail"],
            "Use the default model (currently Opus 5.5) · $4/$20 per Mtok"
        );
        let read: AgentAvailability = serde_json::from_value(wire).unwrap();
        assert_eq!(read.models, claude.models);

        let codex = offer_from(&AgentKind::Codex, &said(CODEX_SIGNED_OUT)).unwrap();
        assert_eq!(
            values(&codex),
            [
                "gpt-6.1-sol",
                "gpt-6-astra",
                "gpt-6-sol",
                "gpt-6-luna",
                "gpt-5.6-sol",
                "gpt-6.1-terra",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5"
            ]
        );
        assert_eq!(
            codex.models[0],
            AgentModel {
                value: "gpt-6.1-sol".into(),
                label: "GPT-6.1-Sol".into(),
                detail: Some("Latest workhorse model for coding and everyday work.".into()),
            }
        );
        assert_eq!(codex.signed_in, Some(false));
        let with_key = offer_from(&AgentKind::Codex, &said(CODEX_API_KEY)).unwrap();
        assert_eq!(with_key.signed_in, Some(true));
    }

    /// A model Codex lists as hidden is not offered (the recording with one entry
    /// marked so).
    #[test]
    fn a_model_codex_hides_is_not_offered() {
        let mut said = said(CODEX_SIGNED_OUT);
        let listed = said.iter_mut().find(|line| line["id"] == 2).unwrap();
        listed["result"]["data"][1]["hidden"] = true.into();
        let offer = offer_from(&AgentKind::Codex, &said).unwrap();
        assert_eq!(offer.models.len(), 8);
        assert!(!values(&offer).contains(&"gpt-6-astra"));
    }

    /// **An agent is asked as a start runs it, from the home folder, and let go**:
    /// each itself, never through the launcher, and its stdin closed once it has
    /// answered.
    #[tokio::test]
    async fn an_agent_is_asked_as_a_start_runs_it_and_let_go() {
        let rig = Rig::new("ask", ANSWERING);
        rig.claude_says("ask", CLAUDE_SIGNED_OUT);
        answer(&rig, "codex", CODEX_API_KEY);
        assert_eq!(
            ask(
                &rig.env,
                None,
                &[rig.claude.as_os_str()],
                &AgentKind::Claude,
                false
            )
            .await,
            offer_from(&AgentKind::Claude, &said(CLAUDE_SIGNED_OUT))
        );
        assert_eq!(
            ask(
                &rig.env,
                None,
                &[rig.launcher.as_os_str()],
                &AgentKind::Codex,
                true
            )
            .await,
            offer_from(&AgentKind::Codex, &said(CODEX_API_KEY))
        );
        assert_eq!(
            rig.claude_runs(),
            "-p --input-format stream-json --output-format stream-json --verbose \
             --no-session-persistence --strict-mcp-config --settings {\"disableAllHooks\":true}\n"
        );
        assert_eq!(
            std::fs::read_to_string(format!("{}.asks", rig.launcher.display())).unwrap(),
            "app-server\n"
        );
        let cwd = std::fs::read_to_string(format!("{}.cwd", rig.launcher.display())).unwrap();
        assert_eq!(
            Path::new(cwd.trim()),
            home_in(&rig.env).canonicalize().unwrap()
        );
    }

    /// **A signed-out Codex is asked once, and again only once `login status`
    /// says it is signed in.** Each ask leaves a folder behind in Codex's own
    /// `.tmp`, so asking at every list would leave one per list. Signed in, it is
    /// asked once per version.
    #[tokio::test]
    async fn a_signed_out_codex_is_asked_again_only_once_login_status_says_signed_in() {
        let _env = crate::ProcessEnvRead::take();
        let rig = Rig::new("codex-offer", ANSWERING);
        let known = KnownAgents::default();
        let codex = |version: &'static str| {
            offer(
                &known,
                &rig.env,
                None,
                &rig.launcher,
                &AgentKind::Codex,
                version,
            )
        };
        answer(&rig, "codex", CODEX_SIGNED_OUT);
        login_status(&rig, 1);
        for _ in 0..3 {
            assert_eq!(
                codex("codex-cli 0.167.0").await.unwrap().signed_in,
                Some(false)
            );
        }
        assert_eq!(
            (asked(&rig, "app-server"), asked(&rig, "login status")),
            (1, 3)
        );

        answer(&rig, "codex", CODEX_API_KEY);
        login_status(&rig, 0);
        assert_eq!(
            codex("codex-cli 0.167.0").await.unwrap().signed_in,
            Some(true)
        );
        assert_eq!(
            codex("codex-cli 0.167.0").await.unwrap().signed_in,
            Some(true)
        );
        assert_eq!(
            (asked(&rig, "app-server"), asked(&rig, "login status")),
            (2, 4)
        );
        codex("codex-cli 0.168.0").await.unwrap();
        assert_eq!(asked(&rig, "app-server"), 3);
    }

    /// **A signed-out Claude Code is never asked; signed in, it is asked once per
    /// version; its sign-in unknown, at every list**: asking starts it, which a
    /// signed-out list has no use for, since its start is refused.
    #[tokio::test]
    async fn claude_is_not_asked_signed_out_and_once_per_version_signed_in() {
        let rig = Rig::new("claude-offer", ANSWERING);
        let known = KnownAgents::default();
        let ready = |version: &str, signed_in| ReadyClaude {
            path: rig.claude.clone(),
            version: version.into(),
            signed_in,
        };
        let asks = || {
            rig.claude_runs()
                .lines()
                .filter(|run| run.starts_with("-p"))
                .count()
        };
        rig.claude_says("ask", CLAUDE_SIGNED_OUT);
        for _ in 0..2 {
            let models =
                claude_models(&known, &rig.env, None, &ready("2.1.319", Some(false))).await;
            assert!(models.is_empty(), "{models:?}");
        }
        assert_eq!(asks(), 0, "a signed-out list starts no claude");

        for _ in 0..2 {
            let models = claude_models(&known, &rig.env, None, &ready("2.1.319", None)).await;
            assert_eq!(models.len(), 5);
        }
        assert_eq!(asks(), 2, "its sign-in unknown, it is asked at every list");

        rig.claude_says("ask", CLAUDE_API_KEY);
        for _ in 0..2 {
            claude_models(&known, &rig.env, None, &ready("2.1.319", Some(true))).await;
        }
        assert_eq!(asks(), 3);
        claude_models(&known, &rig.env, None, &ready("2.1.320", Some(true))).await;
        assert_eq!(asks(), 4);
    }

    /// **An agent that gives no answer offers nothing, and is asked again.**
    #[tokio::test]
    async fn an_agent_that_gives_no_answer_is_asked_again() {
        let rig = Rig::new("no-answer", ANSWERING);
        let known = KnownAgents::default();
        for _ in 0..2 {
            let offered = offer(
                &known,
                &rig.env,
                None,
                &rig.launcher,
                &AgentKind::Codex,
                "codex-cli 0.167.0",
            )
            .await;
            assert_eq!(offered, None);
        }
        assert_eq!(asked(&rig, "app-server"), 2);
        assert_eq!(asked(&rig, "login status"), 2);
    }

    /// A codex beside the rig's claude, so on its login shell's `PATH`. It says the
    /// version in `<codex>.version`, exiting with `<codex>.version_exit` (else 0), and
    /// otherwise answers as [`ANSWERING`] does, its ask from `<codex>.codex`. Each run
    /// goes into `<codex>.asks`.
    fn codex_on_path(rig: &Rig, version: &str) -> PathBuf {
        let codex = rig.claude.with_file_name("codex");
        std::fs::write(
            &codex,
            format!(
                "#!/bin/sh\nif [ \"$*\" = --version ]; then printf '%s\\n' \"$*\" >> \"$0.asks\"; \
                 cat \"$0.version\"; exit \"$(cat \"$0.version_exit\" 2>/dev/null || echo 0)\"; \
                 fi\n{ANSWERING}\n"
            ),
        )
        .unwrap();
        std::process::Command::new("chmod")
            .arg("755")
            .arg(&codex)
            .status()
            .unwrap();
        codex_says(&codex, "version", version);
        codex
    }

    fn codex_says(codex: &Path, what: &str, text: &str) {
        std::fs::write(format!("{}.{what}", codex.display()), text).unwrap();
    }

    fn codex_runs(codex: &Path) -> String {
        std::fs::read_to_string(format!("{}.asks", codex.display())).unwrap_or_default()
    }

    async fn codex_listed(rig: &Rig, known: &KnownAgents) -> AgentAvailability {
        agent_availability(&AgentKind::Codex, known, &rig.env, None, None).await
    }

    /// A stand-in `agy` at `path` answering `agy acp` as agy 1.5.15 does: `initialize`, then `session/new` with its
    /// settings (two model families), or, `signed_out`, with "Authentication required"; `session/close` with nothing.
    /// Every line it reads goes to `<path>.asks`.
    fn agy_says(path: &Path, signed_out: bool) {
        let opened = if signed_out {
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"Authentication required"}}"#
        } else {
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-ask","configOptions":[{"id":"model","category":"model","currentValue":"gemini-4-neon","options":[{"value":"gemini-4-neon","name":"Gemini 4 Neon"},{"value":"gemini-3.8-flash","name":"Gemini 3.8 Flash"}]},{"id":"mode","category":"mode","currentValue":"default","options":[]}]}}"#
        };
        let asks = format!("{}.asks", path.display());
        let script = format!(
            "#!/bin/sh\nwhile read -r line; do\n  printf '%s\\n' \"$line\" >> '{asks}'\n  case \"$line\" in\n    *'\"initialize\"'*) echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":1}}}}' ;;\n    *'\"session/new\"'*) echo '{opened}' ;;\n    *'\"session/close\"'*) echo '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{}}}}' ;;\n  esac\ndone\n",
        );
        std::fs::write(path, script).unwrap();
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
    }

    /// **Antigravity is listed with agy's own models** (S1): its start box offers the model families agy's settings
    /// name, read from one session opened with no prompt and closed again (which agy lists in no history), asked
    /// once while it is signed in. A signed-out agy is listed as such, with no models; none found, as not installed,
    /// in its own name.
    #[tokio::test]
    async fn antigravity_is_listed_with_agys_own_models() {
        let rig = Rig::new("agy-listed", ANSWERING);
        let known = KnownAgents::default();
        let agy = rig.root.join("agy");
        agy_says(&agy, false);
        let listed = || {
            agent_availability(
                &AgentKind::Antigravity,
                &known,
                &rig.env,
                None,
                agy.to_str(),
            )
        };
        let found = listed().await;
        assert_eq!(found.agent, AgentKind::Antigravity);
        assert!(found.available, "{found:?}");
        assert_eq!(found.detail, None);
        assert_eq!(found.signed_in, Some(true));
        assert_eq!(
            found.models,
            vec![
                AgentModel {
                    value: "gemini-4-neon".into(),
                    label: "Gemini 4 Neon".into(),
                    detail: None
                },
                AgentModel {
                    value: "gemini-3.8-flash".into(),
                    label: "Gemini 3.8 Flash".into(),
                    detail: None
                },
            ]
        );
        let asks = || std::fs::read_to_string(format!("{}.asks", agy.display())).unwrap();
        assert!(
            asks().contains(r#""method":"session/close","params":{"sessionId":"sess-ask"}"#),
            "{}",
            asks()
        );
        assert_eq!(listed().await, found);
        assert_eq!(
            asks().matches("initialize").count(),
            1,
            "asked once while signed in"
        );

        let signed_out = rig.root.join("agy-out");
        agy_says(&signed_out, true);
        let out = agent_availability(
            &AgentKind::Antigravity,
            &KnownAgents::default(),
            &rig.env,
            None,
            signed_out.to_str(),
        )
        .await;
        assert!(out.available);
        assert_eq!(out.signed_in, Some(false));
        assert_eq!(out.models, Vec::new());
        assert_eq!(
            out.detail,
            Some(format!(
                "Antigravity on {} isn’t signed in.",
                this_computer()
            ))
        );
        let missing = agent_availability(
            &AgentKind::Antigravity,
            &known,
            &rig.env,
            None,
            Some("/nonexistent/agy"),
        )
        .await;
        assert!(!missing.available);
        assert_eq!(
            missing.detail,
            Some(format!(
                "Antigravity isn’t installed on {}.",
                this_computer()
            ))
        );
    }

    /// **Codex is the user's own codex, run itself**: one on the login shell's `PATH`
    /// at the floor is listed with what its own ask answers, and the launcher runs
    /// nothing.
    #[tokio::test]
    async fn a_codex_at_the_floor_is_listed_from_its_own_answers() {
        let rig = Rig::new("codex-listed", ANSWERING);
        let codex = codex_on_path(&rig, "codex-cli 0.167.0\n");
        codex_says(&codex, "codex", CODEX_API_KEY);
        assert_eq!(
            codex_listed(&rig, &KnownAgents::default()).await,
            AgentAvailability {
                agent: AgentKind::Codex,
                available: true,
                detail: None,
                models: offer_from(&AgentKind::Codex, &said(CODEX_API_KEY))
                    .unwrap()
                    .models,
                signed_in: Some(true),
            }
        );
        assert_eq!(codex_runs(&codex), "--version\nlogin status\napp-server\n");
        assert!(
            !rig.launcher.with_extension("asks").exists(),
            "the launcher ran"
        );
    }

    /// **A codex below the floor is refused with the floor sentence**, before it is
    /// asked anything else.
    #[tokio::test]
    async fn a_codex_below_the_floor_is_refused_with_the_floor_sentence() {
        let rig = Rig::new("codex-floor", ANSWERING);
        let codex = codex_on_path(&rig, "codex-cli 0.166.9\n");
        codex_says(&codex, "codex", CODEX_API_KEY);
        let listed = codex_listed(&rig, &KnownAgents::default()).await;
        let floor = format!(
            "Codex 0.166.9 on {} is older than 0.167.0, the oldest CodeConnect works with.",
            this_computer()
        );
        assert_eq!(
            (listed.available, listed.detail.as_deref()),
            (false, Some(floor.as_str()))
        );
        assert_eq!(codex_runs(&codex), "--version\n", "asked nothing more");
    }

    /// **A codex whose version cannot be read is refused with the version sentence**:
    /// no number, or one printed by a `--version` that failed.
    #[tokio::test]
    async fn a_codex_that_does_not_say_its_version_is_refused() {
        let rig = Rig::new("codex-unread", ANSWERING);
        let codex = codex_on_path(&rig, "codex-cli\n");
        let unread = format!("Codex on {} didn’t report its version.", this_computer());
        let listed = codex_listed(&rig, &KnownAgents::default()).await;
        assert_eq!(
            (listed.available, listed.detail.as_deref()),
            (false, Some(unread.as_str()))
        );
        codex_says(&codex, "version", "codex-cli 0.167.0\n");
        codex_says(&codex, "version_exit", "1");
        let listed = codex_listed(&rig, &KnownAgents::default()).await;
        assert_eq!(
            (listed.available, listed.detail.as_deref()),
            (false, Some(unread.as_str()))
        );
    }

    /// **A test's login shell never starts in a person's home**, whose start-up files it would run: refused before
    /// anything is started.
    #[test]
    #[should_panic(expected = "outside a temporary home")]
    fn a_tests_login_shell_never_starts_in_a_persons_home() {
        let environment = HashMap::from([
            (OsString::from("HOME"), OsString::from("/Users/person")),
            (OsString::from("SHELL"), OsString::from("/bin/zsh")),
        ]);
        let _ = login_shell_command(&environment, None, &[OsStr::new("/nonexistent/agent")]);
    }

    /// **No test can reach an agent installed on the computer**: the places a test looks for codex or claude are the
    /// one it names, its fenced `PATH` and a temporary home of its own, never the computer's install locations
    /// (Homebrew's, `/usr/local`) nor a person's own home, which a bare `cargo test` would be given.
    #[test]
    fn no_test_looks_in_the_computers_install_locations() {
        // A person's own home, as a bare `cargo test` would be given, and a test's own temporary one.
        let person = Path::new("/Users/person");
        let rig = std::env::temp_dir().join("rig-home");
        let codex = not_installed_in_tests(
            protocol::codex_bin::candidates(None, person, Vec::new()),
            None,
        );
        let claude = not_installed_in_tests(
            protocol::claude_bin::candidates(None, person, Vec::new()),
            None,
        );
        assert_eq!(
            (codex, claude),
            (Vec::new(), Vec::new()),
            "nothing of the computer's or the person's"
        );
        // Found on the login shell's `PATH` (path_helper adds Homebrew's folder; a person's shell adds their own):
        // still not a test's.
        let hits = [
            PathBuf::from("/opt/homebrew/bin/codex"),
            person.join(".local/bin/claude"),
        ];
        let codex = not_installed_in_tests(
            protocol::codex_bin::candidates(None, person, hits[..1].to_vec()),
            None,
        );
        let claude = not_installed_in_tests(
            protocol::claude_bin::candidates(None, person, hits[1..].to_vec()),
            None,
        );
        assert_eq!(
            (codex, claude),
            (Vec::new(), Vec::new()),
            "nothing found on PATH outside a test's own folders"
        );
        let kept = not_installed_in_tests(
            protocol::codex_bin::candidates(None, &rig, Vec::new()),
            None,
        );
        assert_eq!(
            kept,
            [rig.join(".local/bin/codex")],
            "a test's own home is searched"
        );
        let named = not_installed_in_tests(
            protocol::codex_bin::candidates(Some("/opt/stand-in/codex"), person, Vec::new()),
            Some("/opt/stand-in/codex"),
        );
        assert_eq!(
            named,
            [PathBuf::from("/opt/stand-in/codex")],
            "the one a test names"
        );
    }

    /// **A codex that cannot be found is not available**, with the sentence that says
    /// so: none on the login shell's `PATH` and none in the install locations.
    #[tokio::test]
    async fn a_codex_that_is_not_there_is_missing() {
        let rig = Rig::new("no-codex", ANSWERING);
        let listed = codex_listed(&rig, &KnownAgents::default()).await;
        assert_eq!(
            (listed.available, listed.detail),
            (
                false,
                Some(format!("Codex isn’t installed on {}.", this_computer()))
            )
        );
    }

    /// **A signed-out codex is listed with the sign-in sentence; one whose own model
    /// provider needs no OpenAI sign-in is signed in; and nothing of an account its
    /// `account/read` names is kept or sent.**
    #[tokio::test]
    async fn a_signed_out_codex_is_listed_with_the_sign_in_sentence() {
        let rig = Rig::new("codex-signed-out", ANSWERING);
        let codex = codex_on_path(&rig, "codex-cli 0.167.0\n");
        codex_says(&codex, "codex", CODEX_SIGNED_OUT);
        let listed = codex_listed(&rig, &KnownAgents::default()).await;
        let sentence = format!("Codex on {} isn’t signed in.", this_computer());
        assert_eq!(
            (listed.available, listed.signed_in, listed.detail.as_deref()),
            (true, Some(false), Some(sentence.as_str()))
        );

        let provider = CODEX_SIGNED_OUT.replace(
            r#""requiresOpenaiAuth":true"#,
            r#""requiresOpenaiAuth":false"#,
        );
        assert_ne!(provider, CODEX_SIGNED_OUT);
        codex_says(&codex, "codex", &provider);
        let listed = codex_listed(&rig, &KnownAgents::default()).await;
        assert_eq!((listed.signed_in, listed.detail), (Some(true), None));

        let marked = CODEX_API_KEY.replace(
            r#"{"type":"apiKey"}"#,
            r#"{"type":"chatgpt","email":"x10-marker@example.invalid","planType":"plus"}"#,
        );
        assert_ne!(marked, CODEX_API_KEY);
        codex_says(&codex, "codex", &marked);
        let known = KnownAgents::default();
        let listed = codex_listed(&rig, &known).await;
        assert_eq!(listed.signed_in, Some(true));
        let kept = format!("{:?}", known.offered(&AgentKind::Codex, None, "0.167.0"));
        assert!(kept.starts_with("Some"), "the ask was kept: {kept}");
        let frame = serde_json::to_string(&ServerMessage::Projects {
            projects: Vec::new(),
            agents: vec![listed],
        })
        .unwrap();
        for seen in [&kept, &frame] {
            assert!(!seen.contains("x10-marker"), "{seen}");
        }
    }

    /// **A signed-in Codex is listed with its models at once, and asked once per
    /// version.** A signed-in app-server answers `model/list` but never `account/read`,
    /// and keeps running after its stdin closes (0.167.0, measured four times). `login
    /// status` says it is signed in, so the ask waits for the models alone and ends the
    /// app-server; the next list asks nothing.
    #[tokio::test]
    async fn a_signed_in_codex_is_listed_with_its_models_at_once_and_asked_once() {
        let rig = Rig::new("codex-signed-in", ANSWERING);
        let codex = codex_on_path(&rig, "codex-cli 0.167.0\n");
        std::fs::write(
            &codex,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.asks\"\ncase \"$*\" in\n\
             --version) cat \"$0.version\" ;;\n\"login status\") exit 0 ;;\n\
             app-server) cat \"$0.codex\"; exec sleep 60 ;;\nesac\n",
        )
        .unwrap();
        let models_only: String = CODEX_API_KEY
            .lines()
            .filter(|line| !line.contains(r#""id":3"#))
            .map(|line| format!("{line}\n"))
            .collect();
        codex_says(&codex, "codex", &models_only);
        let known = KnownAgents::default();
        let started = std::time::Instant::now();
        let listed = codex_listed(&rig, &known).await;
        assert!(
            started.elapsed() < PROBE_TIMEOUT / 2,
            "listed after {:?}",
            started.elapsed()
        );
        assert_eq!(
            (listed.available, listed.signed_in, listed.detail.as_deref()),
            (true, Some(true), None)
        );
        assert_eq!(
            listed.models,
            offer_from(&AgentKind::Codex, &said(&models_only))
                .unwrap()
                .models
        );
        assert!(!listed.models.is_empty());
        codex_listed(&rig, &known).await;
        assert_eq!(
            codex_runs(&codex),
            "--version\nlogin status\napp-server\n--version\n",
            "the second list asks nothing"
        );
    }

    /// **What the list says of Codex, emitted by this build**: below the floor, its
    /// version unread, signed out and signed in, each a `projects` frame whose one
    /// agent is that Codex, the computer named "this computer". It is the contract for
    /// the apps' readers of Codex's sentences.
    #[tokio::test]
    async fn the_codex_availability_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/codex/availability-wire.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            derived_codex_availability_wire().await,
            committed,
            "the committed Codex availability wire no longer matches what this build emits. \
             Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_codex_availability_wire_fixture"
        );
    }

    /// Rewrite `fixtures/codex/availability-wire.json` with what this build emits.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_codex_availability_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_codex_availability_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/codex/availability-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// Codex as the list finds it, a stub codex on the login shell's `PATH` saying
    /// each version and answering with the recorded signed-out and API-key answers.
    async fn derived_codex_availability_wire() -> serde_json::Value {
        let rig = Rig::new("codex-wire", ANSWERING);
        let codex = codex_on_path(&rig, "codex-cli 0.166.9\n");
        // Only the sentence names the computer, so only the sentence is normalised.
        let frame = |mut listed: AgentAvailability| {
            listed.detail = listed
                .detail
                .map(|detail| detail.replace(&this_computer(), "this computer"));
            serde_json::to_value(ServerMessage::Projects {
                projects: Vec::new(),
                agents: vec![listed],
            })
            .unwrap()
        };
        let below_the_floor = frame(codex_listed(&rig, &KnownAgents::default()).await);
        codex_says(&codex, "version", "codex-cli\n");
        let version_unread = frame(codex_listed(&rig, &KnownAgents::default()).await);
        codex_says(&codex, "version", "codex-cli 0.167.0\n");
        codex_says(&codex, "codex", CODEX_SIGNED_OUT);
        let signed_out = frame(codex_listed(&rig, &KnownAgents::default()).await);
        codex_says(&codex, "codex", CODEX_API_KEY);
        let signed_in = frame(codex_listed(&rig, &KnownAgents::default()).await);
        serde_json::json!({
            "below_the_floor": below_the_floor,
            "version_unread": version_unread,
            "signed_out": signed_out,
            "signed_in": signed_in,
        })
    }

    /// **The codex a check finds is config.json's before any on the `PATH`.**
    #[tokio::test]
    async fn a_configured_codex_comes_before_the_one_on_the_path() {
        let rig = Rig::new("codex-configured", ANSWERING);
        let on_path = codex_on_path(&rig, "codex-cli 0.167.0\n");
        let configured = rig.root.join("configured-codex");
        std::fs::copy(&on_path, &configured).unwrap();
        assert_eq!(
            codex_path(&rig.env, configured.to_str()).await,
            Some(configured.clone())
        );
        assert_eq!(codex_path(&rig.env, None).await, Some(on_path));
    }

    /// **A claude that cannot be found is not available, and its start is
    /// `agent_missing`, with nothing launched**: none on the login shell's PATH and
    /// none in the install locations.
    #[tokio::test]
    async fn a_claude_that_is_not_there_is_missing() {
        let rig = recording("no-claude");
        std::fs::remove_file(&rig.claude).unwrap();
        let listed = agent_availability(
            &AgentKind::Claude,
            &KnownAgents::default(),
            &rig.env,
            None,
            None,
        )
        .await;
        assert!(!listed.available);
        assert_eq!(
            listed.detail,
            Some(format!(
                "Claude Code isn’t installed on {}.",
                this_computer()
            ))
        );
        let result = rig.start(&rig.project(), Some("hi")).await;
        assert!(
            matches!(
                result,
                StartSessionResult::Failed {
                    reason: StartFailure::AgentMissing,
                    ..
                }
            ),
            "{result:?}"
        );
        assert_eq!(rig.runs(), 0, "a refused start runs nothing");
    }

    /// **The claude a start runs is the one the launcher finds, from its list in its
    /// order**: config.json's `claude_bin` (passed over when it is not a program),
    /// the login shell's PATH, `~/.local/bin/claude`, where Anthropic's installer puts
    /// it whether or not it is on the PATH, then the legacy local install
    /// `~/.claude/local/claude`, which its installer reached by an alias, not the PATH.
    #[tokio::test]
    async fn the_claude_a_start_runs_is_the_one_the_launchers_list_finds() {
        let rig = recording("which-claude");
        let home = home_in(&rig.env);
        let copy_to = |at: PathBuf| {
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::copy(&rig.claude, &at).unwrap();
            at
        };
        let local = copy_to(home.join(".local/bin/claude"));
        let legacy = copy_to(home.join(".claude/local/claude"));
        let (configured, _) = test_claude(&rig.root.join("configured"));
        std::fs::write(
            format!("{}.version", configured.display()),
            "2.1.300 (Claude Code)\n",
        )
        .unwrap();
        let not_a_program = rig.root.join("not-a-program");
        std::fs::write(&not_a_program, "2.1.300 (Claude Code)\n").unwrap();
        let found = |claude_bin: Option<PathBuf>| {
            let env = rig.env.clone();
            async move {
                let claude_bin = claude_bin.map(|path| path.display().to_string());
                claude_probe(&env, None, claude_bin.as_deref())
                    .await
                    .unwrap()
            }
        };
        let ready = found(Some(configured.clone())).await;
        assert_eq!(
            (ready.path, ready.version.as_str()),
            (configured, "2.1.300")
        );
        assert_eq!(found(Some(not_a_program)).await.path, rig.claude);
        assert_eq!(found(None).await.path, rig.claude);
        std::fs::remove_file(&rig.claude).unwrap();
        assert_eq!(found(None).await.path, local);
        std::fs::remove_file(&local).unwrap();
        assert_eq!(found(None).await.path, legacy);
    }

    /// **Below 2.1.268 Claude Code is refused with the floor sentence, in the list
    /// and at the start, and nothing is launched**; 2.1.268 itself starts.
    #[tokio::test]
    async fn claude_below_the_floor_is_refused_with_the_floor_sentence() {
        let rig = recording("floor");
        let sentence = format!(
            "Claude Code 2.1.267 on {} is older than 2.1.268, the oldest CodeConnect works with.",
            this_computer()
        );
        rig.claude_says("version", "2.1.267 (Claude Code)\n");
        let listed = agent_availability(
            &AgentKind::Claude,
            &KnownAgents::default(),
            &rig.env,
            None,
            None,
        )
        .await;
        assert!(!listed.available);
        assert_eq!(listed.detail, Some(sentence.clone()));
        let result = rig.start(&rig.project(), Some("hi")).await;
        assert_eq!(result, failed(StartFailure::LaunchFailed, sentence));
        assert_eq!(rig.runs(), 0, "a refused start runs nothing");

        rig.claude_says("version", "2.1.268 (Claude Code)\n");
        let result = rig.start(&rig.project(), Some("hi")).await;
        assert!(reached(&result), "{result:?}");
        assert_eq!(rig.runs(), 1);
    }

    /// **A claude whose `--version` fails, or names no version, is refused with
    /// its own sentence**, not as missing, and what it printed is not repeated.
    #[tokio::test]
    async fn a_claude_that_does_not_say_its_version_is_refused() {
        let rig = recording("no-version");
        let sentence = format!(
            "Claude Code on {} didn’t report its version.",
            this_computer()
        );
        for (said, exit) in [("2.1.300 (Claude Code)\n", "1"), ("unknown\n", "0")] {
            rig.claude_says("version", said);
            rig.claude_says("version_exit", exit);
            let result = rig.start(&rig.project(), Some("hi")).await;
            assert_eq!(result, failed(StartFailure::LaunchFailed, sentence.clone()));
        }
        assert_eq!(rig.runs(), 0, "a refused start runs nothing");
    }

    /// **A signed-out Claude Code shows as signed out in the list, and its start
    /// is refused with the sentence naming this computer**, nothing launched:
    /// `auth status` as 2.1.268 printed it signed out (app-qa's floor run).
    #[tokio::test]
    async fn a_signed_out_claude_is_refused_at_the_start() {
        let rig = recording("signed-out");
        rig.claude_says("auth", AUTH_SIGNED_OUT);
        rig.claude_says("auth_exit", "1");
        let listed = agent_availability(
            &AgentKind::Claude,
            &KnownAgents::default(),
            &rig.env,
            None,
            None,
        )
        .await;
        assert!(listed.available);
        assert_eq!(listed.signed_in, Some(false));
        let sentence = format!("Claude Code on {} isn’t signed in.", this_computer());
        assert_eq!(listed.detail, Some(sentence.clone()));
        let result = rig.start(&rig.project(), Some("hi")).await;
        assert_eq!(result, failed(StartFailure::LaunchFailed, sentence));
        assert_eq!(rig.runs(), 0, "a refused start runs nothing");
    }

    /// What a signed-in `auth status` prints, with an email and an organization
    /// that must reach nothing.
    const AUTH_MARKED: &str = "{\n  \"loggedIn\": true,\n  \"authMethod\": \"claude.ai\",\n  \
                               \"email\": \"b9-marker@example.invalid\",\n  \
                               \"orgName\": \"B9-MARKER-ORG\"\n}\n";

    /// A signed-in `initialize` answer to the models ask, whose `account` carries the
    /// same marker email and organization, with one model named by a control marker
    /// that the list's frame shows.
    const ASK_MARKED: &str = concat!(
        r#"{"type":"control_response","response":{"subtype":"success","request_id":"models","#,
        r#""response":{"models":[{"value":"default","displayName":"B18-CONTROL-MODEL","#,
        r#""description":"Default"}],"account":{"email":"b9-marker@example.invalid","#,
        r#""organization":"B9-MARKER-ORG","subscriptionType":"max","tokenSource":"claude.ai","#,
        r#""apiProvider":"firstParty"}}}}"#,
        "\n"
    );

    /// **Of the account, nothing is kept from `auth status` or from the models ask's
    /// answer (P1 22)**: `auth status` is read for `loggedIn` alone and the ask's
    /// answer for its models alone. Signed in, and on an exit `auth status` does not
    /// give, the email and organization in either are in neither what the probe
    /// keeps, the offer the list keeps, the list's frame, nor a refusal; the ask's
    /// control model is in the frame and the kept offer, so its answer was read.
    #[tokio::test]
    async fn nothing_of_the_account_is_kept_from_auth_status_or_the_models_ask() {
        let rig = recording("account");
        rig.claude_says("auth", &format!("startup noise {{\n{AUTH_MARKED}"));
        rig.claude_says("ask", ASK_MARKED);
        let markers = ["b9-marker@example.invalid", "B9-MARKER-ORG"];
        let clean = |text: &str| markers.iter().all(|marker| !text.contains(marker));
        for exit in ["0", "2"] {
            rig.claude_says("auth_exit", exit);
            let ready = claude_probe(&rig.env, None, None).await.unwrap();
            assert_eq!(ready.signed_in, (exit == "0").then_some(true));
            assert!(clean(&format!("{ready:?}")), "{ready:?}");
            let known = KnownAgents::default();
            let listed = agent_availability(&AgentKind::Claude, &known, &rig.env, None, None).await;
            let kept = format!(
                "{:?}",
                known.offered(&AgentKind::Claude, None, &ready.version)
            );
            assert!(clean(&kept), "{kept}");
            assert!(
                kept.contains("B18-CONTROL-MODEL"),
                "the ask was kept: {kept}"
            );
            let frame = serde_json::to_string(&ServerMessage::Projects {
                projects: Vec::new(),
                agents: vec![listed],
            })
            .unwrap();
            assert!(clean(&frame), "{frame}");
            assert!(
                frame.contains("B18-CONTROL-MODEL"),
                "the ask was read: {frame}"
            );
        }
        rig.claude_says("version", "2.1.267 (Claude Code)\n");
        let refused = format!("{:?}", rig.start(&rig.project(), Some("hi")).await);
        assert!(clean(&refused), "{refused}");
    }

    /// **Only the two answers `auth status` gives are read as a sign-in**: exit 0
    /// with `loggedIn` true, exit 1 with it false. Anything else (another exit, no
    /// JSON, the two mismatched pairs) is unknown, and the start goes on.
    #[tokio::test]
    async fn any_other_auth_status_is_unknown_and_the_start_goes_on() {
        let rig = recording("unknown-auth");
        for (said, exit) in [
            ("{\n  \"loggedIn\": false\n}\n", "2"),
            ("Not logged in.\n", "1"),
            ("{\n  \"loggedIn\": false\n}\n", "0"),
            ("{\n  \"loggedIn\": true\n}\n", "1"),
        ] {
            rig.claude_says("auth", said);
            rig.claude_says("auth_exit", exit);
            assert_eq!(
                claude_probe(&rig.env, None, None).await.unwrap().signed_in,
                None,
                "{said:?} {exit}"
            );
            let result = rig.start(&rig.project(), Some("hi")).await;
            assert!(reached(&result), "{result:?}");
        }
        assert_eq!(rig.runs(), 4);
    }

    /// **The probe runs claude itself, never the launcher**, and adds no
    /// `CLAUDE_CONFIG_DIR` (decision 159): the claude is asked its version, its
    /// sign-in and its models, and the launcher only the start.
    #[tokio::test]
    async fn the_probe_runs_claude_itself_with_no_config_dir() {
        let rig = recording("direct");
        rig.claude_says("ask", CLAUDE_SIGNED_OUT);
        let listed = agent_availability(
            &AgentKind::Claude,
            &KnownAgents::default(),
            &rig.env,
            None,
            None,
        )
        .await;
        assert_eq!(listed.models.len(), 5);
        let runs = rig.claude_runs();
        assert!(runs.lines().any(|run| run == "--version"), "{runs}");
        assert!(runs.lines().any(|run| run == "auth status"), "{runs}");
        assert!(runs.lines().any(|run| run.starts_with("-p ")), "{runs}");
        assert_eq!(rig.runs(), 0, "the list starts no run");
        let seen = rig.root.join("config-dir-seen");
        std::fs::write(
            &rig.claude,
            format!(
                "#!/bin/sh\nprintf '%s' \"${{CLAUDE_CONFIG_DIR-unset}}\" > '{}'\necho '2.1.268 (Claude Code)'\n",
                seen.display()
            ),
        )
        .unwrap();
        claude_probe(&rig.env, None, None).await.unwrap();
        assert_eq!(std::fs::read_to_string(&seen).unwrap(), "unset");
    }

    /// **What the user's shell start-up prints with no newline at its end does not
    /// reach the probe**: iTerm2's shell integration prints its escape codes so in
    /// every interactive shell. The PATH, the version and the sign-in are read as
    /// claude gave them.
    #[tokio::test]
    async fn start_up_output_with_no_newline_does_not_reach_the_probe() {
        let rig = recording("start-up-output");
        let profile = home_in(&rig.env).join(".profile");
        let read = std::fs::read_to_string(&profile).unwrap();
        std::fs::write(
            &profile,
            format!("printf '\\033]1337;ShellIntegrationVersion=23;shell=sh\\007'\n{read}"),
        )
        .unwrap();
        let ready = claude_probe(&rig.env, None, None).await.unwrap();
        assert_eq!(
            (ready.path, ready.version.as_str(), ready.signed_in),
            (rig.claude.clone(), "2.1.268", Some(true))
        );
        rig.claude_says("auth", AUTH_SIGNED_OUT);
        rig.claude_says("auth_exit", "1");
        assert_eq!(
            claude_probe(&rig.env, None, None).await.unwrap().signed_in,
            Some(false)
        );
    }

    /// **The models a minor-33 daemon offers, and a start that picks one, emitted
    /// by this build.** It is the contract for the apps' decoders of these fields.
    #[test]
    fn the_minor_33_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/claude/minor-33-wire.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            derived_minor_33_wire(),
            committed,
            "the committed minor-33 wire contract no longer matches what this build emits. \
             Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_minor_33_wire_fixture"
        );
    }

    /// **The folder question on the wire, emitted by this build**: a start without
    /// an answer and the `needs_trust` it gets, the start that answers, and the
    /// three folder-trust frames with the list they get. It is the contract for
    /// the apps' readers and writers of these frames.
    #[tokio::test]
    async fn the_folder_trust_wire_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/wire/folder-trust-wire.json");
        let committed: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            derived_folder_trust_wire().await,
            committed,
            "the committed folder-trust wire contract no longer matches what this build \
             emits. Regenerate it with cargo test -p ccd --bin ccd -- --ignored \
             regenerate_the_folder_trust_wire_fixture"
        );
    }

    /// Rewrite `fixtures/wire/folder-trust-wire.json` with what this build emits.
    #[tokio::test]
    #[ignore = "generator, not a gate"]
    async fn regenerate_the_folder_trust_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_folder_trust_wire().await).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/wire/folder-trust-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// From a start in a fresh folder, answered, then listed from the store. The
    /// folder's path, as asked and as resolved, and the answer's time are replaced
    /// by fixed values, so the fixture is the same on every machine.
    async fn derived_folder_trust_wire() -> serde_json::Value {
        const FOLDER: &str = "/tmp/ccd-folder-trust/my app";
        let rig = counting("trust-wire");
        let path = rig.project();
        let real = std::fs::canonicalize(&rig.project)
            .unwrap()
            .display()
            .to_string();
        let start = |request_id: &str, trust: Option<Trust>| {
            serde_json::to_value(ClientMessage::StartSession {
                request_id: request_id.into(),
                path: path.clone(),
                agent: AgentKind::Claude,
                prompt: Some("fix the tests".into()),
                worktree: None,
                model: None,
                trust,
            })
            .unwrap()
        };
        let needs_trust = ServerMessage::StartSessionResult {
            request_id: "r1".into(),
            result: rig.start_answering(&path, None).await,
        };
        rig.start_answering(&path, Some(Trust::NotTrusted)).await;
        let folders = rig
            .db
            .folder_trusts()
            .await
            .unwrap()
            .into_iter()
            .map(|answer| protocol::ws::FolderAnswer {
                answered_at: "2026-11-12T20:00:00Z".into(),
                ..answer
            })
            .collect();
        let frame = |message: ClientMessage| serde_json::to_value(message).unwrap();
        let wire = serde_json::json!({
            "start_session_request": start("r1", None),
            "start_session_needs_trust": serde_json::to_value(needs_trust).unwrap(),
            "start_session_answered_request": start("r2", Some(Trust::NotTrusted)),
            "list_folder_trust_request": frame(ClientMessage::ListFolderTrust {
                request_id: "t1".into(),
            }),
            "set_folder_trust_request": frame(ClientMessage::SetFolderTrust {
                request_id: "t2".into(),
                path: real.clone(),
                trust: Trust::Trusted,
            }),
            "forget_folder_trust_request": frame(ClientMessage::ForgetFolderTrust {
                request_id: "t3".into(),
                path: real.clone(),
            }),
            "folder_trust": serde_json::to_value(ServerMessage::FolderTrust {
                request_id: "t1".into(),
                folders,
            })
            .unwrap(),
        });
        serde_json::from_str(
            &wire
                .to_string()
                .replace(&real, FOLDER)
                .replace(&path, FOLDER),
        )
        .unwrap()
    }

    /// Rewrite `fixtures/claude/minor-33-wire.json` with what this build emits.
    #[test]
    #[ignore = "generator, not a gate"]
    fn regenerate_the_minor_33_wire_fixture() {
        let wire = serde_json::to_string_pretty(&derived_minor_33_wire()).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude/minor-33-wire.json"
            ),
            wire + "\n",
        )
        .unwrap();
    }

    /// A project list whose agents carry what this build makes of the agents' own
    /// recorded answers (Claude Code signed out, Codex with an API key), and a start
    /// that names one of the models. From the recordings and fixed values, the
    /// computer named "this computer", so the fixture is the same on every machine.
    /// Claude Code's entry pairs the signed-out sentence with its recorded answer's
    /// models, for the apps' decoders; a list sends a signed-out Claude Code with none.
    fn derived_minor_33_wire() -> serde_json::Value {
        let claude = startable(
            &AgentKind::Claude,
            auth_reading(Some(1), AUTH_SIGNED_OUT.as_bytes()),
            offer_from(&AgentKind::Claude, &said(CLAUDE_SIGNED_OUT))
                .unwrap()
                .models,
            "this computer",
        );
        let codex_offer = offer_from(&AgentKind::Codex, &said(CODEX_API_KEY)).unwrap();
        let codex = AgentAvailability {
            agent: AgentKind::Codex,
            available: true,
            detail: None,
            models: codex_offer.models,
            signed_in: codex_offer.signed_in,
        };
        serde_json::json!({
            "projects": serde_json::to_value(ServerMessage::Projects {
                projects: vec![Project {
                    path: "/tmp/ccd-minor-33/app".into(),
                    name: "app".into(),
                    last_used: "2026-11-08T09:00:00.000Z".into(),
                    last_agent: AgentKind::Claude,
                    git_branch: Some("main".into()),
                    exists: true,
                }],
                agents: vec![claude, codex],
            })
            .unwrap(),
            "start_session_request": serde_json::to_value(ClientMessage::StartSession {
                request_id: "r1".into(),
                path: "/tmp/ccd-minor-33/app".into(),
                agent: AgentKind::Claude,
                prompt: Some("fix the tests".into()),
                worktree: None,
                model: Some("haiku".into()),
                trust: None,
            })
            .unwrap(),
        })
    }
}
