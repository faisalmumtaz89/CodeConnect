//! Unix-socket server at `~/.codeconnect/ccd.sock`.
//!
//! Both client kinds are served by one uniform loop: every connection gets a
//! writer task fed by an mpsc channel, so a hook reply and a supervisor request
//! travel the same path and nothing has to own the socket exclusively.
//!
//! Connections are inbound-only. The daemon never dials a session, which is
//! what keeps it out of the agents' parent chain.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use protocol::ipc::{ClientFrame, DaemonFrame, MAX_FRAME_BYTES};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::state::Daemon;

pub async fn serve(daemon: Arc<Daemon>, socket_path: PathBuf) -> Result<()> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    reclaim_socket(&socket_path).await?;

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    // The socket is the daemon's whole trust boundary for hooks: owner-only.
    set_owner_only(&socket_path)?;
    crate::log_info!("ipc listening on {}", socket_path.display());

    // One permit per live connection. Not a defence against a hostile peer —
    // the socket is `0600`, so reaching it means being this account — but
    // against a runaway: a shell loop spawning `codeconnect ls`, or a hook storm, used
    // to spawn a task and hold a descriptor per attempt with no ceiling at all,
    // and exhausting the descriptor budget here takes the *tailnet* listener
    // down with it.
    let limit = Arc::new(tokio::sync::Semaphore::new(
        daemon.config.ipc_max_connections,
    ));

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                // Admitted before the spawn: a task that spawns and then waits
                // has already cost the descriptor and the task.
                let Ok(permit) = Arc::clone(&limit).try_acquire_owned() else {
                    crate::log_warn!(
                        "ipc: refusing a connection; at the limit of {}",
                        daemon.config.ipc_max_connections
                    );
                    drop(stream);
                    continue;
                };
                let daemon = Arc::clone(&daemon);
                tokio::spawn(async move {
                    // Released on every exit path, including a panic.
                    let _permit = permit;
                    if let Err(err) = handle_connection(daemon, stream).await {
                        crate::log_debug!("ipc connection ended: {err:#}");
                    }
                });
            }
            Err(err) => {
                crate::log_error!("ipc accept failed: {err}");
                // Back off rather than spin on a persistent error (EMFILE).
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

/// Remove a socket left behind by a `kill -9`, but never one a live daemon owns.
async fn reclaim_socket(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match UnixStream::connect(path).await {
        Ok(_) => bail!(
            "another ccd is already listening on {}; refusing to steal it",
            path.display()
        ),
        Err(_) => {
            // Not a warning. Reaching here means the connect *failed*, which is
            // to say nothing is listening — so the file is a leftover and
            // removing it is the only correct action, never a fault. It is the
            // ordinary state after a `kill -9`, and this product's central
            // claim is that a `kill -9` costs nothing but a reconnect: a daemon
            // that warns every time it makes good on that promise is teaching
            // its operator to skim past warnings. Measured on the owner's
            // machine: 90 of these in one error log, one per restart, against
            // 105 starts.
            crate::log_info!("removing stale socket {}", path.display());
            std::fs::remove_file(path)
                .with_context(|| format!("removing stale socket {}", path.display()))?;
            Ok(())
        }
    }
}

fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", path.display()))
}

type Inflight =
    Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<protocol::ipc::SupervisorResult>>>>;

async fn handle_connection(daemon: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    // Bounded, and the bound is the point. This used to be an unbounded
    // channel, so a client that stopped reading — a `codeconnect ls` suspended with
    // ctrl-Z, a supervisor whose process is stopped — let the daemon buffer
    // frames on its behalf until memory ran out, with nothing anywhere saying
    // so. Bounded turns that into backpressure the *reader* feels: the read
    // loop awaits on a full queue and stops taking new work from a peer that
    // is not consuming its replies, which is the honest shape of the problem.
    let (tx, mut rx) = mpsc::channel::<DaemonFrame>(daemon.config.ipc_write_queue);

    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let Ok(mut line) = serde_json::to_vec(&frame) else {
                continue;
            };
            line.push(b'\n');
            if write_half.write_all(&line).await.is_err() {
                break;
            }
            let _ = write_half.flush().await;
        }
    });

    let mut reader = BufReader::new(read_half);
    let inflight: Inflight = Arc::new(std::sync::Mutex::new(HashMap::new()));
    // The *run* this connection registered, not its name: the socket closing is
    // what tells us the supervisor is gone, and detaching by name would detach
    // whichever run currently holds it.
    let mut registered: Option<crate::state::Registration> = None;

    let result = read_loop(&daemon, &mut reader, &tx, &inflight, &mut registered).await;

    if let Some(registration) = registered {
        daemon.unregister_supervisor(&registration).await;
    }
    drop(tx);
    let _ = writer.await;
    result
}

async fn read_loop(
    daemon: &Arc<Daemon>,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    tx: &mpsc::Sender<DaemonFrame>,
    inflight: &Inflight,
    registered: &mut Option<crate::state::Registration>,
) -> Result<()> {
    loop {
        let Some(line) = read_line_limited(reader, MAX_FRAME_BYTES).await? else {
            return Ok(());
        };
        if line.is_empty() {
            continue;
        }
        let frame: ClientFrame = match serde_json::from_slice(&line) {
            Ok(frame) => frame,
            Err(err) => {
                crate::log_warn!("ipc: undecodable frame ({err})");
                let _ = tx
                    .send(DaemonFrame::Error {
                        message: format!("undecodable frame: {err}"),
                    })
                    .await;
                continue;
            }
        };

        // Before the dispatch below, so a request that asks this daemon to
        // manage an SSH key is answered without any part of it being carried
        // out — a device must not end up revoked, or a code minted, on the
        // strength of a flag the daemon then declines to honour.
        if let Some(refusal) = frame.ssh_refusal() {
            crate::log_warn!("ipc: refused an SSH request from an older shim ({refusal})");
            let _ = tx
                .send(DaemonFrame::Error {
                    message: refusal.to_string(),
                })
                .await;
            continue;
        }

        match frame {
            ClientFrame::Hook(post) => {
                let wait = post.wait;
                let decision = daemon.handle_hook(post).await;
                if wait {
                    let _ = tx.send(DaemonFrame::HookReply { decision }).await;
                }
            }
            ClientFrame::Register(info) => {
                let registration = daemon
                    .register_supervisor(info, tx.clone(), Arc::clone(inflight))
                    .await?;
                *registered = Some(registration);
                let _ = tx.send(DaemonFrame::Ack).await;
            }
            // Pre-`Register` support negotiation. The daemon answers honestly
            // with the agents it can host and the specific verdict for the asked
            // agent. The negotiation *flow* (a supervisor withholding `Register`
            // on a `false`) lands with the Codex launcher; a Claude supervisor
            // has no reason to ask, and Claude is always supported.
            ClientFrame::NegotiateSupport { agent, .. } => {
                let supported_agents = daemon.supported_agents();
                let supported = supported_agents.contains(&agent);
                let _ = tx
                    .send(DaemonFrame::SupportedAgents {
                        supported_agents,
                        supported,
                    })
                    .await;
            }
            ClientFrame::SupervisorResponse { id, result } => {
                let responder = inflight
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                match responder {
                    Some(responder) => {
                        let _ = responder.send(result);
                    }
                    // Late response to a request we already timed out. Dropping
                    // it is correct: the caller has moved on.
                    None => crate::log_debug!("ipc: late supervisor response {id}"),
                }
            }
            ClientFrame::Heartbeat {
                session_id,
                session_uid,
            } => {
                // The registered key is authoritative when there is one: it was
                // resolved once, at registration, and cannot drift.
                match registered.as_ref() {
                    Some(registration) => {
                        let session = &registration.session;
                        daemon.heartbeat(&session.name, Some(&session.uid)).await;
                    }
                    None => daemon.heartbeat(&session_id, session_uid.as_deref()).await,
                }
            }
            ClientFrame::SessionExited {
                session_id,
                session_uid,
                exit_code,
            } => {
                daemon
                    .session_exited(&session_id, session_uid.as_deref(), exit_code)
                    .await;
            }
            ClientFrame::ListSessions => {
                // An unreadable database is reported, not rendered as an empty
                // fleet. `codeconnect ls` printing nothing is the operator's evidence
                // that no sessions exist, and it must not also be what a failed
                // query looks like.
                let frame = match daemon.sessions().await {
                    Ok(sessions) => DaemonFrame::Sessions { sessions },
                    Err(err) => {
                        crate::log_error!("could not list sessions: {err:#}");
                        DaemonFrame::Error {
                            message: format!("could not read the session list: {err:#}"),
                        }
                    }
                };
                let _ = tx.send(frame).await;
            }
            ClientFrame::PruneSessions { dry_run } => {
                // Counted before the prune, so the two halves of the report
                // describe the same moment: what went, and what was left behind
                // and why.
                let (kept_live, kept_unknown) = match daemon.sessions().await {
                    Ok(sessions) => (
                        sessions
                            .iter()
                            .filter(|s| {
                                matches!(
                                    s.lifecycle,
                                    protocol::event::Lifecycle::Live
                                        | protocol::event::Lifecycle::Spawning
                                )
                            })
                            .count(),
                        sessions
                            .iter()
                            .filter(|s| s.lifecycle == protocol::event::Lifecycle::Unknown)
                            .count(),
                    ),
                    Err(err) => {
                        crate::log_error!("prune: could not read the session list: {err:#}");
                        (0, 0)
                    }
                };
                // Counted after the prune, so it describes what is left rather
                // than what was there — and reported however the prune went,
                // because an orphan is invisible everywhere else.
                let frame = match daemon.prune_ended_sessions(dry_run).await {
                    Ok(removed) => DaemonFrame::Pruned {
                        // Counted *after*, unlike the two above, and that is the
                        // point: an ended run still present once the prune has
                        // run is one the prune deliberately held back. On a dry
                        // run everything is still there, so subtract what would
                        // have gone rather than reporting the whole set as held.
                        kept_held: match daemon.sessions().await {
                            Ok(sessions) => sessions
                                .iter()
                                .filter(|s| s.lifecycle == protocol::event::Lifecycle::Exited)
                                .count()
                                .saturating_sub(if dry_run { removed.len() } else { 0 }),
                            Err(_) => 0,
                        },
                        orphan_events: daemon.db.orphan_event_count().await.unwrap_or_default(),
                        removed: removed
                            .into_iter()
                            .map(|row| protocol::ipc::PrunedSummary {
                                session_uid: row.session_uid,
                                session_id: row.session_id,
                                cwd: row.cwd,
                                created_at: row.created_at,
                                updated_at: row.updated_at,
                                events: row.events,
                            })
                            .collect(),
                        kept_live,
                        kept_unknown,
                        dry_run,
                    },
                    // A failed prune is an error, never an empty removal list:
                    // "nothing needed removing" and "the delete failed" must
                    // not print the same way.
                    Err(err) => DaemonFrame::Error {
                        message: format!("could not prune ended sessions: {err:#}"),
                    },
                };
                let _ = tx.send(frame).await;
            }
            ClientFrame::CreatePairing { ttl_secs, .. } => {
                let ttl = if ttl_secs == 0 {
                    daemon.config.pairing_ttl_secs
                } else {
                    ttl_secs
                };
                let frame = match daemon.create_pairing(ttl).await {
                    Ok((code, expires_at)) => DaemonFrame::Pairing {
                        code,
                        expires_at,
                        host: daemon.endpoint.host.clone(),
                        port: daemon.endpoint.port,
                        tls: daemon.endpoint.tls,
                        allow_ssh: protocol::ipc::AlwaysFalse,
                    },
                    Err(err) => DaemonFrame::Error {
                        message: format!("{err:#}"),
                    },
                };
                let _ = tx.send(frame).await;
            }
            ClientFrame::ListDevices => {
                let frame = match daemon.list_devices().await {
                    Ok(devices) => DaemonFrame::Devices { devices },
                    Err(err) => DaemonFrame::Error {
                        message: format!("{err:#}"),
                    },
                };
                let _ = tx.send(frame).await;
            }
            ClientFrame::RevokeDevice { device, .. } => {
                let frame = match daemon.revoke(&device).await {
                    // `AlwaysFalse` is not "no sweep was attempted": `revoke`
                    // does sweep `~/.ssh/authorized_keys`. It is that this
                    // reply carries no count, because five of the sweep's
                    // outcomes remove nothing — no absolute `$HOME`, a file it
                    // cannot read, one it cannot rewrite, one that changed
                    // underneath it, and tagged lines that are not the
                    // marker-and-key pair — and a `0` on the wire
                    // would read as "the file is clean now" for all of them.
                    // The sweep says which it was in the daemon's log.
                    Ok(outcome) => DaemonFrame::Revoked {
                        device: outcome.device,
                        token_revoked: outcome.token_revoked,
                        ssh_key_removed: protocol::ipc::AlwaysFalse,
                    },
                    Err(err) => DaemonFrame::Error {
                        message: format!("{err:#}"),
                    },
                };
                let _ = tx.send(frame).await;
            }
            ClientFrame::DaemonInfo => {
                let _ = tx.send(DaemonFrame::Daemon(daemon.info().await)).await;
            }
        }
    }
}

/// Read one newline-terminated frame with a hard ceiling.
///
/// `read_until` would happily grow its buffer to whatever the peer sends;
/// scanning `fill_buf` chunks bounds the allocation at the cap instead. Returns
/// `None` at clean EOF.
async fn read_line_limited(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    max: usize,
) -> Result<Option<Vec<u8>>> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let (eof, found, used) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                (true, false, 0)
            } else {
                match available.iter().position(|&byte| byte == b'\n') {
                    Some(pos) => {
                        out.extend_from_slice(&available[..pos]);
                        (false, true, pos + 1)
                    }
                    None => {
                        let len = available.len();
                        out.extend_from_slice(available);
                        (false, false, len)
                    }
                }
            }
        };
        reader.consume(used);
        if eof {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        if found {
            return Ok(Some(out));
        }
        if out.len() > max {
            bail!("frame exceeds {max} bytes");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::pairing::DeviceSummary;
    use tokio::io::AsyncBufReadExt;

    /// A daemon on its own database, served over a socket in a temp directory.
    ///
    /// The whole point of these tests is what the *socket* does with a frame, so
    /// they go through `serve` rather than calling the handler: the refusal has
    /// to sit ahead of the dispatch, and only a real round trip can show that.
    async fn served_daemon() -> (Arc<Daemon>, PathBuf) {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stamp = format!(
            "{}-{}-{}",
            std::process::id(),
            n,
            protocol::time::now_unix_ms()
        );

        let db = std::env::temp_dir().join(format!("ccd-ipc-{stamp}.db"));
        let _ = std::fs::remove_file(&db);
        let store = Arc::new(crate::store::Store::open(&db).unwrap());

        let (tail_tx, tail_rx) = mpsc::unbounded_channel();
        // Kept alive: a dropped receiver fails every transcript registration,
        // which is not what these tests are about.
        Box::leak(Box::new(tail_rx));
        let daemon = Daemon::new(
            protocol::config::Config::default(),
            store,
            Arc::new(crate::apns::LoggingPushSender::new()),
            crate::state::Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
            tail_tx,
        );

        let socket = std::env::temp_dir().join(format!("ccd-ipc-{stamp}.sock"));
        tokio::spawn(serve(Arc::clone(&daemon), socket.clone()));
        // The listener binds inside the task, so the first connect may arrive
        // before the path exists.
        for _ in 0..200 {
            if UnixStream::connect(&socket).await.is_ok() {
                return (daemon, socket);
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the ipc server never came up on {}", socket.display());
    }

    /// Send one frame, read one reply.
    async fn exchange(socket: &Path, line: &str) -> DaemonFrame {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        write_half.write_all(line.as_bytes()).await.unwrap();
        write_half.write_all(b"\n").await.unwrap();
        write_half.flush().await.unwrap();

        let mut reply = String::new();
        BufReader::new(read_half)
            .read_line(&mut reply)
            .await
            .unwrap();
        serde_json::from_str(&reply)
            .unwrap_or_else(|err| panic!("undecodable reply {reply:?}: {err}"))
    }

    fn error_message(frame: DaemonFrame) -> String {
        match frame {
            DaemonFrame::Error { message } => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    async fn paired_device(daemon: &Arc<Daemon>) -> DeviceSummary {
        let (code, _) = daemon.create_pairing(300).await.unwrap();
        daemon
            .authenticate("static-token-for-tests", None, Some(&code), Some("iPhone"))
            .await;
        let devices = daemon.list_devices().await.unwrap();
        assert_eq!(devices.len(), 1, "the pairing code must buy one device");
        devices.into_iter().next().unwrap()
    }

    #[tokio::test]
    async fn a_shim_asking_to_install_an_ssh_key_is_refused_instead_of_paired() {
        let (_daemon, socket) = served_daemon().await;
        let message = error_message(
            exchange(
                &socket,
                r#"{"type":"create_pairing","ttl_secs":300,"allow_ssh":true}"#,
            )
            .await,
        );
        assert!(message.contains("does not manage SSH keys"), "{message}");
        assert!(message.contains("--ssh"), "{message}");
    }

    #[tokio::test]
    async fn an_ssh_only_revoke_leaves_the_device_exactly_as_it_was() {
        let (daemon, socket) = served_daemon().await;
        let before = paired_device(&daemon).await;
        assert!(before.is_active(), "the device starts paired");

        let message = error_message(
            exchange(
                &socket,
                &format!(
                    r#"{{"type":"revoke_device","device":"{}","ssh_only":true}}"#,
                    before.device_id
                ),
            )
            .await,
        );
        assert!(message.contains("does not manage SSH keys"), "{message}");

        // The refusal is worth nothing if the device was revoked on the way to
        // it: a shim asking for half an operation must get none of it.
        let after = daemon.list_devices().await.unwrap();
        assert_eq!(after.len(), 1);
        assert!(after[0].is_active(), "the device must still be paired");
        assert_eq!(after[0].revoked_at, None);
    }

    #[tokio::test]
    async fn a_revoke_without_the_flag_still_revokes() {
        // This drives a real revocation, and a revocation sweeps
        // `$HOME/.ssh/authorized_keys`. Without a redirect it sweeps the
        // developer's own file — and, running beside the sweep's own tests,
        // it sweeps *theirs*: `$HOME` is process-global, so a sweep taken
        // without this lock reads whichever fake home another test currently
        // has installed and rewrites that test's fixture underneath it.
        let _home = crate::legacy_credentials::test_support::FakeHome::new("ipc-revoke");
        let (daemon, socket) = served_daemon().await;
        let device = paired_device(&daemon).await;

        let reply = exchange(
            &socket,
            &format!(
                r#"{{"type":"revoke_device","device":"{}"}}"#,
                device.device_id
            ),
        )
        .await;
        match reply {
            DaemonFrame::Revoked { token_revoked, .. } => assert!(token_revoked),
            other => panic!("expected a revocation, got {other:?}"),
        }
        assert!(!daemon.list_devices().await.unwrap()[0].is_active());
    }
}
