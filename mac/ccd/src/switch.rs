//! Moving a computer's open runs to an agent's active account (decision 306, Slice E; decision 311).
//!
//! Choosing an account (`ActivateAccount`) marks every live run of that agent due to move, in the same write that
//! makes the choice; nothing is stopped then. A due run is simply live: its menus, settings, mail and listing are
//! as before. It moves when the person next sends it a message, if it is certainly quiet ([`moves`]): its turn over,
//! no message of the person's still to answer, no card waiting, no wake outstanding, and nothing it started in its
//! last turn that may still be running. ccd then ends its process and files that end typed as a switch
//! (`switched_to`), and the message relaunches it, as a message relaunches any ended run, on the agent's ACTIVE
//! account as of that message, resuming its session with its model, its mode (Bypass and Full Access included,
//! decision 308) and its effort, with no effort notice. That relaunch going live ends the switch; one that does not
//! go through leaves the run ended and relaunchable, its own line saying why, `failed`. A run not certainly quiet
//! takes the message on the account it is on and stays due: any doubt waits. A run already on the active account
//! when a message comes only loses its mark, so choosing again, or choosing back, is never a lock.
//!
//! The decision and the send are one step under the run's move gate ([`crate::state::Daemon::move_gate`]): a
//! second message, a mail wake, a settings change or a Stop waits for it. Work claude runs in the background holds
//! the move for as long as its process runs (ccd files when it starts, never when it ends). Accepted misses: a
//! message queued inside Codex itself (`codex queue`) does not survive the app-server's end; and a process a session
//! started outside its agent's own tracking (a Claude `cmd &`, a Codex background command from an earlier turn) ends
//! when the session moves.

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

/// What the person's message `request_id` to the run `uid`, read live under its move gate, does with a switch: `false`, it is
/// sent as any message is; `true`, its process is being ended by its switch ([`Store::SWITCH_MOVED`]) and the
/// message relaunches it once it has ([`ended_for_move`]). A run due to move moves only when certainly quiet
/// ([`crate::store::Store::certainly_quiet`], a card, and `turn_running`, a Codex link's own word on its turn); a
/// run on the active account already loses its mark and is not moved. A run already moving goes on moving.
pub(crate) async fn moves(
    daemon: &Arc<Daemon>,
    uid: &str,
    request_id: &str,
    turn_running: bool,
) -> bool {
    match daemon.db.switch_state(uid.to_string()).await {
        Ok(Some(Store::SWITCH_WAITING)) => {}
        Ok(Some(Store::SWITCH_MOVED)) => return true,
        _ => return false,
    }
    let Ok(Some(row)) = daemon.db.get_session(uid.to_string()).await else {
        return false;
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
        return false;
    }
    if turn_running
        || !daemon
            .db
            .certainly_quiet(uid.to_string(), request_id.to_string())
            .await
            .unwrap_or(false)
        || daemon.has_pending_card(uid).await
    {
        return false;
    }
    // Ended by its switch from here: the end its process makes is filed as switched, any other end (a crash) as
    // what it is. The run's own link stops it as an End run would, the run left live.
    set(daemon, uid, Some(Store::SWITCH_MOVED)).await;
    if !daemon.stop_through_link(uid).await {
        set(daemon, uid, Some(Store::SWITCH_WAITING)).await;
        return false;
    }
    true
}

/// The run `uid`, live on the account it records: due to move when that is not its agent's active account (a choice
/// made while it was starting or moving, which marked nothing), or when `doubt`; otherwise its switch is over.
pub(crate) async fn settle(daemon: &Daemon, uid: &str, doubt: bool) {
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
    let due = (doubt || active != current).then_some(Store::SWITCH_WAITING);
    set(daemon, uid, due).await;
}

/// How often [`ended_for_move`] looks again.
const POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Whether the run `uid`, its process stopped by its switch, has ended by `deadline`: its row `exited` and its
/// recorded host or app-server gone with no member left in its group, as a message's relaunch requires. What its
/// group still holds once that host is gone (a process started outside the agent's own tracking) is stopped as a dead
/// host's group is ([`crate::claude_host::stop_group`]), once.
pub(crate) async fn ended_for_move(
    daemon: &Daemon,
    uid: &str,
    deadline: tokio::time::Instant,
) -> bool {
    let mut laddered = false;
    loop {
        if let Ok(Some(watch)) = daemon.db.watched_host(uid.to_string()).await {
            if watch.lifecycle == protocol::event::Lifecycle::Exited
                && protocol::proc_identity::liveness(&watch.host)
                    == protocol::proc_identity::Liveness::Gone
            {
                if !protocol::proc_identity::group_has_members(watch.host.pid) {
                    return true;
                }
                if let (false, Some(boot)) = (laddered, watch.boot) {
                    laddered = true;
                    crate::claude_host::stop_group(watch.host, boot).await;
                    continue;
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL).await;
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

/// The relaunch by the message of the run `uid`, marked switching, did not go through (`why`): its mark becomes
/// `failed`, with a line of its own saying why. One that goes live is [`settle`]d.
pub(crate) async fn relaunch_failed(daemon: &Arc<Daemon>, uid: &str, why: String) {
    set(daemon, uid, Some(Store::SWITCH_FAILED)).await;
    crate::log_warn!("{uid}: its switch's start failed: {why}");
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
            // Drawn under a row that names the account: the sentence names none, and the reason is the log's.
            "message": if why.starts_with(crate::codex_host::NO_ROLLOUT_AFTER_A_MESSAGE) {
                // Its conversation's record is not where the account's home can see it; the account it ran on can.
                "This conversation could not be continued on the new account: its record was not found. Choose the \
                 account it ran on to continue it."
            } else {
                "The session could not be started on the new account. Try again, or choose the account it ran on."
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

    /// `frames` hold exactly one end, the switch's, typed with `label`, and no link let go; every listing among them
    /// reads the run live, linked and offering its settings, as an unswitched run's would; written to
    /// `fixtures/accounts/<name>` when `CC_RECORD_SWITCH` is set.
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
        assert!(
            !frames
                .iter()
                .any(|frame| frame["event"]["kind"] == "link_state"
                    && frame["event"]["payload"]["link"] == "detached"),
            "no link let go: {frames:#?}"
        );
        for listing in frames.iter().filter(|frame| frame["type"] == "sessions") {
            let run = &listing["sessions"][0];
            assert_eq!(run["lifecycle"], "live", "never listed ended: {listing}");
            assert_eq!(run["link"], "attached", "never listed unlinked: {listing}");
            assert!(
                run.get("menus").is_some(),
                "its settings offered: {listing}"
            );
            if run["agent"] == "codex" {
                assert_eq!(run["codex_link"], "subscribed", "{listing}");
            }
        }
        keep(frames, name);
    }

    /// `frames`, which file no start a timeline draws on any page (none names why it started), written to
    /// `fixtures/accounts/<name>` when `CC_RECORD_SWITCH` is set.
    pub(crate) fn keep(frames: &[serde_json::Value], name: &str) {
        assert!(
            !frames
                .iter()
                .any(|frame| frame["event"]["kind"] == "session_start"
                    && frame["event"]["payload"].get("source").is_some()),
            "a switch's start draws nothing: {frames:#?}"
        );
        if std::env::var_os("CC_RECORD_SWITCH").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/accounts")
                .join(name);
            std::fs::write(path, serde_json::to_string_pretty(frames).unwrap() + "\n").unwrap();
        }
    }
}
