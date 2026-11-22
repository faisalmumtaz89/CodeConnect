//! Moving a computer's live runs to an agent's active account (decision 306, Slice E; decision 311).
//!
//! Choosing an account (`ActivateAccount`) marks every live run of that agent switching. A marked run moves at its
//! next idle point, its turn over, no card waiting and no message of the person's still unread by the agent: ccd
//! ends its process and files that end typed as a switch (`switched_to`). The run reads live and `moved` from then
//! on, and its next message relaunches it, as a message relaunches any ended run, on the agent's ACTIVE account as of
//! that message, resuming its session with its model, its mode (Bypass and Full Access included, decision 308) and
//! its effort, with no effort notice. That relaunch going live ends the switch; one that does not go through leaves
//! the run ended and relaunchable, its own line saying why, `failed`. A run already on the active account when its
//! idle point comes only loses its mark, so choosing again, or choosing back, is never a lock.
//!
//! A Codex run with no link when the choice is made moves when its link attaches. A second message sent while the
//! first one's relaunch is starting is refused, as for any ended run. Accepted misses: a message queued inside Codex
//! itself (`codex queue`) does not survive the app-server's end; a command or helper an agent left running in the
//! background is stopped when its session moves (ccd sees no background shell, background subagent or Codex
//! terminal).

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
    // The run's own link stops it as an End run would, the run left live; its end is filed as switched.
    daemon.stop_through_link(uid).await;
}

/// The relaunch by the next message of the run `uid`, marked switching, went live on the new account (`Ok`), or
/// did not go through (`Err`, why): its mark goes, or becomes `failed` with a line of its own saying why.
pub(crate) async fn relaunched(daemon: &Arc<Daemon>, uid: &str, outcome: Result<(), String>) {
    let why = match outcome {
        Ok(()) => {
            if let Err(err) = daemon.db.clear_switching(uid.to_string()).await {
                crate::log_error!("{uid}: could not clear its switch: {err:#}");
            }
            return;
        }
        Err(why) => why,
    };
    if let Err(err) = daemon.db.fail_switching(uid.to_string()).await {
        crate::log_error!("{uid}: could not mark its switch failed: {err:#}");
    }
    let agent = match daemon.db.get_session(uid.to_string()).await {
        Ok(Some(row)) => row.agent,
        _ => return,
    };
    let label = daemon
        .store
        .active_account(agent.as_str())
        .ok()
        .flatten()
        .map_or_else(|| "Default".to_string(), |account| account.label);
    let notice = protocol::event::PendingEvent::new(
        &protocol::event::SessionKey::new(uid, uid),
        protocol::event::EventKind::Notification,
        serde_json::json!({
            "notification_type": protocol::event::notice::SWITCH_FAILED,
            "message": format!("Couldn’t start on {label}: {why}"),
        }),
        protocol::event::Source::Daemon,
    );
    if let Err(err) = daemon.ingest(notice).await {
        crate::log_error!("{uid}: could not file why its switch failed: {err:#}");
    }
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
