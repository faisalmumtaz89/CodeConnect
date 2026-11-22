//! Moving a computer's live runs to an agent's active account (decision 306, Slice E).
//!
//! Choosing an account (`ActivateAccount`) marks every live run of that agent switching. A marked run moves at its
//! next idle point, its turn over, no card waiting and no message of the person's still unread by the agent: ccd
//! ends its process, files that end with its own reason, and starts it again at once, with no message, on the
//! agent's ACTIVE account, resuming its session with its model, mode and effort. It reads live throughout. A run
//! already on the active account when its idle point comes only loses its mark, so choosing again, or choosing back,
//! is never a lock: each run moves once, to the latest choice, or not at all.
//!
//! A choice made while a run is still starting again moves it once more as it settles; a Codex run with no link when
//! the choice is made moves when its link attaches. Accepted miss: a message queued inside Codex itself
//! (`codex queue`) does not survive the app-server's restart.

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

/// The run `uid`'s switch is over (started again, or not): its mark goes, and when the active account changed
/// meanwhile, it moves again, as a choice made now would move it.
pub(crate) async fn settled(daemon: &Arc<Daemon>, uid: &str) {
    if let Err(err) = daemon.db.clear_switching(uid.to_string()).await {
        crate::log_error!("{uid}: could not clear its switch: {err:#}");
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
    if active != current {
        begin(daemon, &row.agent).await;
    }
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

/// A switched run's end: the account it goes on, its agent's active one (Default for none), typed
/// ([`protocol::event::SWITCHED_TO`]) and in words.
pub(crate) fn switched_end(daemon: &Daemon, agent: &AgentKind) -> serde_json::Value {
    let active = daemon.store.active_account(agent.as_str()).ok().flatten();
    let label = active
        .as_ref()
        .map_or("Default", |account| account.label.as_str());
    serde_json::json!({
        "exit_code": null,
        "reason": switched_reason(label),
        protocol::event::SWITCHED_TO: {
            "account_id": active.as_ref().map(|account| account.id.clone()),
            "label": label,
        },
    })
}

/// What a connected device is sent across a switch, for the tests that pin it.
#[cfg(test)]
pub(crate) mod window {
    use std::sync::Arc;

    use crate::state::Daemon;

    /// Each of `uid`'s events broadcast from now until `stop` fires, as the `event` frame a device is sent, each
    /// followed by the `sessions` frame (that run's row only) a device listing then would read.
    pub(crate) fn record(
        daemon: &Arc<Daemon>,
        uid: &str,
    ) -> (
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<Vec<serde_json::Value>>,
    ) {
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let mut events = daemon.events_tx.subscribe();
        let (daemon, uid) = (Arc::clone(daemon), uid.to_string());
        let recording = tokio::spawn(async move {
            let mut frames = Vec::new();
            loop {
                let event = tokio::select! {
                    event = events.recv() => match event {
                        Ok(event) => event,
                        Err(_) => break,
                    },
                    _ = &mut stopped => break,
                };
                if event.session_uid != uid {
                    continue;
                }
                let sessions = daemon
                    .sessions()
                    .await
                    .unwrap()
                    .into_iter()
                    .filter(|row| row.session_uid == uid)
                    .collect();
                frames.push(
                    serde_json::to_value(protocol::ws::ServerMessage::Event { event }).unwrap(),
                );
                frames.push(
                    serde_json::to_value(protocol::ws::ServerMessage::Sessions { sessions })
                        .unwrap(),
                );
            }
            frames
        });
        (stop, recording)
    }

    /// `frames` hold exactly one end, the switch's, typed with `label`, and every listing among them reads the run
    /// live; written to `fixtures/accounts/<name>` when `CC_RECORD_SWITCH` is set.
    pub(crate) fn check(frames: &[serde_json::Value], label: &str, name: &str) {
        let ends: Vec<_> = frames
            .iter()
            .filter(|frame| frame["type"] == "event" && frame["event"]["kind"] == "session_end")
            .collect();
        assert_eq!(ends.len(), 1, "{frames:#?}");
        assert_eq!(
            ends[0]["event"]["payload"][protocol::event::SWITCHED_TO]["label"],
            label,
            "{ends:?}"
        );
        for listing in frames.iter().filter(|frame| frame["type"] == "sessions") {
            assert_eq!(
                listing["sessions"][0]["lifecycle"], "live",
                "never listed ended: {listing}"
            );
        }
        if std::env::var_os("CC_RECORD_SWITCH").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/accounts")
                .join(name);
            std::fs::write(path, serde_json::to_string_pretty(frames).unwrap() + "\n").unwrap();
        }
    }
}
