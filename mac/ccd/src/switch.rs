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
//! A moved run offers no settings, as an ended run offers none: they can be changed once its next message has
//! started it. A Codex run with no link when the choice is made moves when its link
//! attaches. A second message sent while the
//! first one's relaunch is starting is refused, as for any ended run. Accepted misses: a message queued inside Codex
//! itself (`codex queue`) does not survive the app-server's end; a command or helper an agent left running in the
//! background is stopped when its session moves (ccd sees no background shell, background subagent or Codex
//! terminal).

use std::sync::Arc;

use protocol::agent::AgentKind;

use crate::state::Daemon;
use crate::store::Store;

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
    if daemon.db.switch_state(uid.to_string()).await.ok().flatten() != Some(Store::SWITCH_WAITING) {
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
        set(daemon, uid, None).await;
        return;
    }
    if !idle(daemon, uid, &row.agent).await {
        return;
    }
    // Ended by its switch from here: the end its process makes is filed as switched, any other end (a crash) as
    // what it is. The run's own link stops it as an End run would, the run left live.
    set(daemon, uid, Some(Store::SWITCH_MOVED)).await;
    if !daemon.stop_through_link(uid).await {
        set(daemon, uid, Some(Store::SWITCH_WAITING)).await;
    }
}

/// The run `uid`'s switch now stands at `state`.
pub(crate) async fn set(daemon: &Daemon, uid: &str, state: Option<i64>) {
    if let Err(err) = daemon.db.set_switch_state(uid.to_string(), state).await {
        crate::log_error!("{uid}: could not keep where its switch stands: {err:#}");
    }
}

/// Whether the run `uid`'s newest end was its switch's, so its relaunch is the switch's: moved, or its start on the
/// new account not gone through yet.
pub(crate) async fn switched(daemon: &Daemon, uid: &str) -> bool {
    matches!(
        daemon.db.switch_state(uid.to_string()).await.ok().flatten(),
        Some(Store::SWITCH_MOVED | Store::SWITCH_FAILED)
    )
}

/// End run on the run `uid`, ended by its switch and not started again: its move is off, and the end End run files
/// on any run is filed, so it reads ended and takes no message, as any run End run ended.
pub(crate) async fn ended(daemon: &Arc<Daemon>, uid: &str) {
    set(daemon, uid, None).await;
    let Ok(id) = protocol::uid::uuid_v4() else {
        return;
    };
    let end = protocol::event::PendingEvent::new(
        &protocol::event::SessionKey::new(uid, uid),
        protocol::event::EventKind::SessionEnd,
        crate::claude_host::stopped_end(),
        protocol::event::Source::Daemon,
    )
    .with_source_event_id(format!("switch-end:{id}"));
    if let Err(err) = daemon.ingest(end).await {
        crate::log_error!("{uid}: could not file its end: {err:#}");
    }
}

/// The relaunch by the next message of the run `uid`, marked switching, went live on the new account (`Ok`), or
/// did not go through (`Err`, why): its mark goes, or becomes `failed` with a line of its own saying why.
pub(crate) async fn relaunched(daemon: &Arc<Daemon>, uid: &str, outcome: Result<(), String>) {
    let why = match outcome {
        Ok(()) => {
            set(daemon, uid, None).await;
            return;
        }
        Err(why) => why,
    };
    set(daemon, uid, Some(Store::SWITCH_FAILED)).await;
    let agent = match daemon.db.get_session(uid.to_string()).await {
        Ok(Some(row)) => row.agent,
        _ => return,
    };
    let active = daemon.store.active_account(agent.as_str()).ok().flatten();
    let label = active
        .as_ref()
        .map_or("Default", |account| account.label.as_str());
    let notice = protocol::event::PendingEvent::new(
        &protocol::event::SessionKey::new(uid, uid),
        protocol::event::EventKind::Notification,
        serde_json::json!({
            "notification_type": protocol::event::notice::SWITCH_FAILED,
            "message": if why.starts_with(crate::codex_host::NO_ROLLOUT_AFTER_A_MESSAGE) {
                // Its conversation's record is not where the account's home can see it; the account it ran on can.
                format!(
                    "This conversation could not be continued on {label}: its record was not found. Choose the \
                     account it ran on to continue it."
                )
            } else {
                format!("Couldn’t start on {label}: {why}")
            },
            protocol::event::SWITCHED_TO: {
                "account_id": active.as_ref().map(|account| account.id.clone()),
                "label": label,
            },
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
        keep(frames, name);
    }

    /// `frames` written to `fixtures/accounts/<name>` when `CC_RECORD_SWITCH` is set.
    pub(crate) fn keep(frames: &[serde_json::Value], name: &str) {
        if std::env::var_os("CC_RECORD_SWITCH").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/accounts")
                .join(name);
            std::fs::write(path, serde_json::to_string_pretty(frames).unwrap() + "\n").unwrap();
        }
    }
}
