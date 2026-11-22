//! Moving a computer's live runs to an agent's active account (decision 306, Slice E).
//!
//! Choosing an account (`ActivateAccount`) marks every live run of that agent switching. A marked run moves at its
//! next idle point, its turn over, no card waiting and no message of the person's still unread by the agent: ccd
//! ends its process, files that end with its own reason, and starts it again at once, with no message, on the
//! agent's ACTIVE account, resuming its session with its model, mode and effort. It reads live throughout. A run
//! already on the active account when its idle point comes only loses its mark, so choosing again, or choosing back,
//! is never a lock: each run moves once, to the latest choice, or not at all.
//!
//! Accepted misses: a message queued inside Codex itself (`codex queue`) does not survive the app-server's restart;
//! a choice made while a run is still starting again on the previous one leaves it on that previous one; a Codex run
//! whose link is not attached when the choice is made moves only at a later choice.

use std::sync::Arc;

use protocol::agent::AgentKind;

use crate::state::Daemon;

/// How a switched run's end reason begins: such a run is one to relaunch, as one whose process ended is.
pub(crate) const SWITCHED: &str = "Switched to ";

/// The reason a switched run's end carries; the apps draw it as one line, never as an end.
pub(crate) fn switched_reason(label: &str) -> String {
    format!("{SWITCHED}{label}")
}

/// Marks `agent`'s live runs and moves each that is idle now.
pub(crate) async fn begin(daemon: &Arc<Daemon>, agent: &AgentKind) {
    let uids = match daemon.db.mark_switching(agent.as_str().to_string()).await {
        Ok(uids) => uids,
        Err(err) => {
            crate::log_error!(
                "could not mark {}'s runs to switch account: {err:#}",
                agent.as_str()
            );
            return;
        }
    };
    for uid in uids {
        // Codex's link alone knows whether a turn runs.
        if *agent == AgentKind::Codex {
            daemon.switch_through_link(&uid).await;
        } else {
            at_idle(daemon, &uid).await;
        }
    }
}

/// The run `uid` at a point it may be idle: when it is marked, it moves if idle, or loses its mark if it is on the
/// active account already.
pub(crate) async fn at_idle(daemon: &Arc<Daemon>, uid: &str) {
    if !daemon.db.switching(uid.to_string()).await.unwrap_or(false) {
        return;
    }
    let Ok(Some(row)) = daemon.db.get_session(uid.to_string()).await else {
        return;
    };
    let active = daemon
        .store
        .active_account(row.agent.as_str())
        .ok()
        .flatten()
        .map(|account| account.id);
    let current = daemon
        .store
        .run_account(uid)
        .ok()
        .flatten()
        .map(|account| account.id);
    if active == current {
        if let Err(err) = daemon.db.clear_switching(uid.to_string()).await {
            crate::log_error!("{uid}: could not clear its switch: {err:#}");
        }
        return;
    }
    if !idle(daemon, uid, &row.agent).await {
        return;
    }
    // The run's own link stops it as an End run would, the run left live; its end, filed as switched, starts it
    // again (`relaunch`).
    daemon.stop_through_link(uid).await;
}

/// The run `uid`, its end filed as switched, started again on its agent's active account with no message.
pub(crate) fn relaunch(daemon: &Arc<Daemon>, uid: &str, agent: &AgentKind) {
    let (daemon, uid) = (Arc::clone(daemon), uid.to_string());
    match agent {
        AgentKind::Claude => tokio::spawn(crate::claude_host::relaunch_switched(daemon, uid)),
        _ => tokio::spawn(crate::codex_host::relaunch_switched(daemon, uid)),
    };
}

/// No turn running, no card waiting, and no message of the person's the agent has not read yet. A Codex run's turn
/// is its link's to know: it asks only once none runs (`LinkCommand::SwitchIfIdle`, a turn's end).
async fn idle(daemon: &Arc<Daemon>, uid: &str, agent: &AgentKind) -> bool {
    (*agent == AgentKind::Codex
        || !daemon
            .db
            .turn_running(uid.to_string())
            .await
            .unwrap_or(true))
        && !daemon.has_pending_card(uid).await
        && daemon
            .db
            .host_sends(uid.to_string())
            .await
            .map(|sends| sends.is_empty())
            .unwrap_or(false)
}

/// The label a switched run's end names: its agent's active account's, or Default.
pub(crate) fn target_label(daemon: &Daemon, agent: &AgentKind) -> String {
    daemon
        .store
        .active_account(agent.as_str())
        .ok()
        .flatten()
        .map(|account| account.label)
        .unwrap_or_else(|| "Default".to_string())
}
