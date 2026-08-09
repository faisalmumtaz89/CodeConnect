//! `ccd` — the CodeConnect daemon.
//!
//! It is a notifier, card-builder and event log. It is deliberately *not* a
//! connectivity layer (Tailscale is) and deliberately *not* the agents' parent
//! (the per-session supervisor connects out to it). `kill -9 ccd` must cost
//! nothing but a reconnect, which is why every piece of session state either
//! lives in SQLite or is re-derived when a supervisor re-registers.

mod apns;
mod apns_sender;
mod apns_token;
mod catalog;
mod db;
#[cfg(test)]
mod fixture_replay;
mod git;
mod ipc_server;
mod liveness;
mod log;
mod logrotate;
mod project_label;
mod push_gate;
mod secret;
mod ssh_keys;
mod state;
mod store;
mod tailer;
mod tls;
mod ws_server;

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use crate::state::{Daemon, Endpoint};
use crate::store::Store;
use protocol::config::Config;

/// Approvals nobody answers are released after this, so `blocked_on` never
/// accumulates ghosts that would keep pushing.
const APPROVAL_MAX_AGE_MS: i64 = 15 * 60 * 1000;

#[tokio::main]
async fn main() -> Result<()> {
    log::init_from_env();
    if std::env::args().any(|arg| arg == "--version") {
        println!(
            "{}",
            protocol::build_identity::version_line("ccd", env!("CARGO_PKG_VERSION"))
        );
        return Ok(());
    }

    let root = protocol::root_dir();
    // Before anything is opened or written. The database, the token and the
    // logs all land inside this tree, and a directory created under the umask
    // is readable by every other account on the Mac for as long as it exists.
    harden_state_dir(&root)?;

    let config = Config::load();
    // `Store::open` and `report_log_integrity` below are the only synchronous
    // database calls left on a runtime thread, and deliberately so: they run
    // before a single listener is bound or task spawned, so there is nothing
    // for them to block. Everything after this point goes through
    // [`crate::db::Db`], which runs the same operations on the blocking pool.
    let store = Arc::new(Store::open(&protocol::db_path())?);
    let token = Arc::new(load_or_create_token(&protocol::token_path())?);
    let (transcript_tx, transcript_rx) = mpsc::unbounded_channel();
    let push = crate::apns_sender::build(&config, Arc::clone(&store));

    let bind = resolve_bind(&config).await;
    tls::install_crypto_provider();
    let (acceptor, endpoint) = resolve_transport(&config, bind).await;
    let local_resolve_poll = Duration::from_millis(config.local_resolve_poll_ms);
    let daemon = Daemon::new(config, Arc::clone(&store), push, endpoint, transcript_tx);
    // Told once, from the one place that knows it: `daemon_info` serves this
    // to the CLI's reachability advisory, which must reason about the socket
    // that exists, not the name the QR advertises.
    let _ = daemon.bind_ip.set(bind.ip().to_string());
    // Its own bytes, hashed now: the file at this path can change under a
    // running process (that is the whole point of recording it), so the hash
    // must be of what was actually loaded, as close to exec as we get.
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(bytes) = std::fs::read(&exe) {
            let _ = daemon.exe_identity.set((
                exe.to_string_lossy().to_string(),
                protocol::hash::sha256_hex(&bytes),
            ));
        }
    }

    crate::log_info!(
        "ccd {} (protocol {}.{}) starting; root={} bind={} host={} tls={} gate={} hold_ms={} managed={}",
        env!("CARGO_PKG_VERSION"),
        protocol::PROTOCOL_VERSION,
        protocol::PROTOCOL_MINOR,
        root.display(),
        bind,
        daemon.endpoint.host,
        daemon.endpoint.tls,
        daemon.config.gate_hook,
        daemon.config.hold_ms,
        state::launchd_label().unwrap_or_else(|| "no (started by hand)".into()),
    );
    report_log_integrity(&store);
    // Before a single socket is open: a phone that connects during recovery
    // would otherwise be told a card it can still see is unknown, and a
    // mutation left in flight by the last process could be typed a second time.
    daemon.recover().await;

    let socket_path = protocol::socket_path();
    let ipc = tokio::spawn(ipc_server::serve(Arc::clone(&daemon), socket_path.clone()));
    let ws = tokio::spawn(ws_server::serve(
        Arc::clone(&daemon),
        bind,
        Arc::clone(&token),
        acceptor.clone(),
    ));

    // A second listener on loopback, for tools running on this Mac.
    //
    // Not redundant with the tailnet address: reaching one's own tailnet IP
    // goes out through the utun interface, where a third-party network filter
    // gets a say. Measured on this machine — Little Snitch holds an unsigned
    // locally built client's connection open and silent until it times out,
    // while leaving loopback alone. A failure to bind it is logged and
    // survived: it is a convenience, and the phone never uses it.
    let loopback = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), bind.port());
    if daemon.config.ws_loopback && loopback != bind {
        let daemon = Arc::clone(&daemon);
        let token = Arc::clone(&token);
        tokio::spawn(async move {
            if let Err(err) = ws_server::serve(daemon, loopback, token, acceptor).await {
                crate::log_warn!("loopback listener unavailable ({err:#}); tailnet only");
            }
        });
    }
    let tail = tokio::spawn(tailer::run(Arc::clone(&daemon), transcript_rx));

    let sweeper = {
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                daemon.expire_stale_approvals(APPROVAL_MAX_AGE_MS).await;
            }
        })
    };

    // Separate from the expiry sweeper because it runs on a completely
    // different timescale: seconds (has the human already answered?) against
    // minutes (has everyone forgotten about this?).
    let local_watch = {
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(local_resolve_poll);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                daemon.sweep_local_resolutions().await;
            }
        })
    };

    // The backstop for every way a session can end without anybody being left
    // to say so: the daemon down at the moment of death, a supervisor killed, a
    // Mac that slept through it. Every session this process inherited was last
    // seen by a *previous* one, and the ones whose agents died meanwhile are
    // indistinguishable in the database from the ones still running — so the
    // first pass runs immediately, and the ticker takes over from there.
    //
    // **Concurrently with the listeners, not before them.** It is the only task
    // here that shells out, and it deliberately waits between the confirmations
    // an exit needs — up to two seconds. Holding the sockets shut for that long
    // would put a two-second hole in the hook path on every restart, and this
    // daemon's central promise is that `kill -9` costs nothing but a reconnect.
    // Nothing is lost by publishing late: a client connected during the sweep
    // learns each end through the ordinary event path, which is the whole point
    // of emitting `SessionEnd` rather than mutating the row.
    let liveness = {
        let daemon = Arc::clone(&daemon);
        let period = daemon.config.liveness_sweep_secs;
        tokio::spawn(async move {
            report_liveness(&daemon.reconcile_liveness().await);
            if period == 0 {
                crate::log_warn!(
                    "liveness_sweep_secs is 0: session state will not be reconciled after \
                     startup, so a session that ends while nothing is watching will keep \
                     reporting as running"
                );
                std::future::pending::<()>().await;
                return;
            }
            let mut ticker = tokio::time::interval(Duration::from_secs(period));
            // Delay rather than burst: a Mac that has just woken must not fire
            // every missed tick at once, and each tick's work is identical
            // anyway — catching up buys nothing and costs a spawn storm.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick is immediate, and the pass above already did it.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let sweep = daemon.reconcile_liveness().await;
                // Only when it changed something or could not see. A quiet
                // fleet must not write a line a minute into the log it shares
                // with everything else.
                if sweep.gone > 0 || sweep.unknown > 0 || sweep.unconfirmed > 0 {
                    report_liveness(&sweep);
                }
            }
        })
    };

    // launchd holds the log files open, so nothing outside this process can
    // rotate them without leaving launchd appending to an unlinked inode.
    let rotate = {
        let cap = daemon.config.log_max_bytes;
        let interval = Duration::from_secs(daemon.config.log_rotate_secs);
        tokio::spawn(logrotate::run(cap, interval))
    };

    // Every one of these is meant to run for the life of the process. A task
    // *returning* is therefore a failure, whatever it returned — and it has to
    // be reported as one, because launchd's `KeepAlive{SuccessfulExit: false}`
    // restarts on failure and only on failure. Exiting 0 here meant the daemon
    // could lose its WebSocket listener (a bind error, say) and stay dead, with
    // launchd content that it had finished its work.
    let stop = tokio::select! {
        result = ipc => Stop::Task(format!("ipc server exited: {result:?}")),
        result = ws => Stop::Task(format!("ws server exited: {result:?}")),
        result = tail => Stop::Task(format!("tailer exited: {result:?}")),
        result = sweeper => Stop::Task(format!("sweeper exited: {result:?}")),
        result = local_watch => Stop::Task(format!("local resolver exited: {result:?}")),
        result = liveness => Stop::Task(format!("liveness sweeper exited: {result:?}")),
        result = rotate => Stop::Task(format!("log rotator exited: {result:?}")),
        () = shutdown_signal() => Stop::Signal,
    };

    // Best-effort: a socket left behind is reclaimed on next start anyway.
    let _ = std::fs::remove_file(&socket_path);
    match stop {
        Stop::Signal => {
            crate::log_info!("ccd shutting down (signal)");
            Ok(())
        }
        Stop::Task(reason) => {
            crate::log_error!("ccd is exiting because a critical task ended: {reason}");
            Err(anyhow::anyhow!("{reason}"))
        }
    }
}

/// Why the daemon stopped. The distinction is the whole point: one of these is
/// somebody asking it to stop, and the other is it falling over.
enum Stop {
    Signal,
    Task(String),
}

/// Establish — and repair — the owner-only boundary around `~/.codeconnect`.
///
/// SECURITY.md claims the daemon's database is "protected by file permissions".
/// It was not: every directory here was created by `create_dir_all` under the
/// process umask, so the macOS login default of `umask 022` produced a `0755`
/// state directory holding a world-readable event log — which is a verbatim copy
/// of every transcript line an agent has produced, secrets included.
///
/// Repair, not just creation. An installation made before this existed already
/// has the loose modes on disk, and a boundary enforced only on new state
/// protects nobody who has already run the daemon. Directory creation is fatal:
/// if the tree cannot be made private, continuing would mean writing the log
/// into a directory we have just failed to protect. Repairing an individual
/// *file* is not — a chmod that fails on one log file is not a reason to refuse
/// to start, and it is reported rather than swallowed.
fn harden_state_dir(root: &Path) -> Result<()> {
    let paths = state_paths(root);
    for dir in &paths.dirs {
        protocol::fsperm::private_dir(dir)
            .with_context(|| format!("securing {} as 0700", dir.display()))?;
    }
    for file in &paths.files {
        if let Err(err) = protocol::fsperm::harden_file(file) {
            crate::log_error!(
                "could not make {} owner-only ({err}); its contents may be readable by other \
                 accounts on this Mac",
                file.display()
            );
        }
    }
    Ok(())
}

/// What the boundary covers.
struct StatePaths {
    dirs: Vec<std::path::PathBuf>,
    files: Vec<std::path::PathBuf>,
}

/// Derive the boundary from one explicit root.
///
/// Explicit rather than read from `protocol::root_dir()` inside the loop so the
/// modes can be asserted against a scratch directory, without a test having to
/// mutate a process-global environment variable that every other test in this
/// binary is also reading.
fn state_paths(root: &Path) -> StatePaths {
    // The `-wal` and `-shm` sidecars are named explicitly: SQLite creates them
    // itself, and while it copies the main database's mode onto a *new* one, a
    // sidecar left behind by an older daemon still has whatever the umask gave
    // it — and it holds the committed pages that have not been checkpointed
    // yet, which is to say the newest facts in the log.
    let db = root.join("events.db");
    StatePaths {
        dirs: vec![
            root.to_path_buf(),
            root.join("sessions"),
            root.join("logs"),
            root.join("tls"),
        ],
        files: vec![
            root.join("token"),
            db.clone(),
            sidecar(&db, "-wal"),
            sidecar(&db, "-shm"),
            root.join("config.json"),
            root.join("logs/ccd.out.log"),
            root.join("logs/ccd.err.log"),
        ],
    }
}

/// `events.db` + `-wal` → `events.db-wal`.
///
/// Appended to the whole filename rather than via `Path::with_extension`, which
/// would *replace* `.db` and quietly produce the wrong path for any database
/// name without an extension.
fn sidecar(db: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

/// Say what a liveness sweep established.
///
/// A sweep that proved nothing is as worth reporting as one that marked rows
/// exited: "26 sessions could not be established" and "26 sessions are running"
/// look identical in a fleet listing and are opposite facts about the daemon's
/// health. Which level it is logged at follows from that — a sweep that could
/// not see is a warning, a sweep that found deaths is news, a quiet one is not
/// worth a line.
fn report_liveness(sweep: &state::LivenessSweep) {
    let summary = sweep.summary();
    if sweep.unknown > 0 {
        crate::log_warn!("liveness: {summary}");
    } else {
        crate::log_info!("liveness: {summary}");
    }
}

/// Say, at startup, whether the log is internally consistent.
///
/// `seq` is promised to be gap-free per run, which makes `MAX(seq) == COUNT(*)`
/// an exact statement of that promise rather than an approximation of it. It is
/// checked here — once, cheaply, over an indexed aggregate — because the failure
/// it detects is silent by nature: a log with a hole in it serves replays that
/// look perfectly well-formed to a client, and the only moment anybody would
/// otherwise notice is when a phone renders a timeline with something missing.
/// The report is the *result* of the check, not a fixed sentence printed after
/// it. It used to say "sequences intact" unconditionally — including on the line
/// straight after reporting a hole, and including when the check never ran
/// because the query failed. Both are the daemon claiming something it does not
/// know, which is the one thing it is not allowed to do.
fn report_log_integrity(store: &Store) {
    match check_log_integrity(store) {
        Ok(integrity) => {
            let summary = integrity.summary();
            if integrity.is_clean() {
                crate::log_info!("event log: {summary}");
            } else if integrity.holes > 0 {
                crate::log_error!("event log: {summary}");
            } else {
                crate::log_warn!("event log: {summary}");
            }
        }
        Err(err) => crate::log_error!(
            "event log: could not be checked ({err:#}); its consistency is unknown"
        ),
    }
}

/// What the check found. Three numbers rather than a verdict, because "we did
/// not look" and "we looked and it was fine" are different answers.
#[derive(Debug, PartialEq, Eq)]
struct LogIntegrity {
    checked: usize,
    holes: usize,
    unchecked: usize,
}

impl LogIntegrity {
    fn is_clean(&self) -> bool {
        self.holes == 0 && self.unchecked == 0
    }

    /// The sentence the operator reads. It has to be derivable from the numbers
    /// alone: the old code printed "sequences intact" unconditionally, on the
    /// line immediately after reporting a hole.
    fn summary(&self) -> String {
        match (self.holes, self.unchecked) {
            (0, 0) => format!("{} run(s), sequences intact", self.checked),
            (0, unchecked) => format!(
                "{} run(s) intact, {unchecked} could not be read — their consistency is unknown",
                self.checked
            ),
            (holes, 0) => format!(
                "{holes} of {} checked run(s) have a hole in the sequence",
                self.checked
            ),
            (holes, unchecked) => format!(
                "{holes} of {} checked run(s) have a hole in the sequence, and {unchecked} more \
                 could not be read",
                self.checked
            ),
        }
    }
}

fn check_log_integrity(store: &Store) -> Result<LogIntegrity> {
    let rows = store.list_sessions()?;
    let total = rows.len();
    let mut checked = 0usize;
    let mut holes = 0usize;
    for row in rows {
        let (Ok(max), Ok(count)) = (
            store.max_seq(&row.session_uid),
            store.count_events(&row.session_uid),
        ) else {
            continue;
        };
        checked += 1;
        if max != count {
            holes += 1;
            crate::log_error!(
                "event log integrity: {} ({}) has max_seq {max} but {count} events — \
                 the sequence has a hole in it",
                row.session_id,
                row.session_uid
            );
        }
    }
    Ok(LogIntegrity {
        checked,
        holes,
        unchecked: total - checked,
    })
}

async fn shutdown_signal() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(err) => {
            crate::log_error!("cannot listen for SIGTERM: {err}");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// Bind to the tailnet address so the listener is not reachable from the LAN.
///
/// launchd gives the daemon no shell PATH, so `tailscale` is looked up at known
/// install locations rather than via `which`.
async fn resolve_bind(config: &Config) -> SocketAddr {
    if let Some(explicit) = &config.ws_bind {
        if let Ok(ip) = explicit.parse::<IpAddr>() {
            return SocketAddr::new(ip, config.ws_port);
        }
        crate::log_warn!("ws_bind {explicit:?} is not an IP address; ignoring it");
    }

    for candidate in protocol::pairing::TAILSCALE_CANDIDATES {
        if !Path::new(candidate).exists() {
            continue;
        }
        let output = tokio::process::Command::new(candidate)
            .args(["ip", "-4"])
            .output()
            .await;
        if let Ok(output) = output {
            if let Some(line) = String::from_utf8_lossy(&output.stdout).lines().next() {
                if let Ok(ip) = line.trim().parse::<IpAddr>() {
                    return SocketAddr::new(ip, config.ws_port);
                }
            }
        }
    }

    crate::log_warn!("no tailnet address found; binding loopback (the phone will not reach it)");
    SocketAddr::new(IpAddr::from([127, 0, 0, 1]), config.ws_port)
}

/// Decide how the phone reaches us, and get a certificate if we can.
///
/// The QR host and the listener's certificate are decided together and once,
/// because they are two halves of one claim: a certificate is issued for a DNS
/// name, and a QR that sends the phone to an IP literal can never validate
/// against it. Any failure below degrades to plain `ws://` on the tailnet IP —
/// never to a daemon that will not start. TLS here is an upgrade to a link
/// Tailscale already encrypts, so refusing to run without it would trade a
/// working daemon for no measurable gain in confidentiality.
async fn resolve_transport(
    config: &Config,
    bind: SocketAddr,
) -> (Option<tokio_rustls::TlsAcceptor>, Endpoint) {
    let fallback = Endpoint {
        host: bind.ip().to_string(),
        port: bind.port(),
        tls: false,
    };

    // **A name is only an honest endpoint if the socket is reachable at it.**
    //
    // Everything below resolves a MagicDNS name independently of `bind`, so a
    // daemon listening on loopback would still advertise its tailnet name — and
    // the QR built from it looks perfect, resolves, and reaches nothing. That is
    // strictly worse than advertising loopback, because loopback is visibly wrong
    // and a name is not.
    //
    // This is the same "decided together and once" rule the doc comment above
    // states; it simply was not enforced when the two halves disagreed. A
    // certificate is pointless here for the same reason: nothing off this machine
    // can open the connection it would protect.
    if bind.ip().is_loopback() {
        crate::log_warn!(
            "listening on {bind}, so no other device can reach this daemon; advertising the \
             loopback address rather than a tailnet name it could not connect to. \
             `codeconnect pair` will refuse to print a code until this is fixed."
        );
        return (None, fallback);
    }

    if !config.tls {
        crate::log_info!("tls disabled by config; serving ws://");
        return (None, fallback);
    }

    let hostname = match &config.tls_hostname {
        Some(explicit) => Some(explicit.clone()),
        None => tls::magic_dns_name().await,
    };
    let Some(hostname) = hostname else {
        crate::log_warn!("no MagicDNS name available; serving ws:// on the tailnet address");
        return (None, fallback);
    };

    match tls::ensure(&hostname, config.cert_refresh_days).await {
        Ok(material) => match tls::acceptor(&material) {
            Ok(acceptor) => {
                crate::log_info!(
                    "tls: serving wss:// as {} ({} days of validity left)",
                    material.hostname,
                    material.days_remaining(protocol::time::now_unix_ms()),
                );
                (
                    Some(acceptor),
                    Endpoint {
                        host: material.hostname,
                        port: bind.port(),
                        tls: true,
                    },
                )
            }
            Err(err) => {
                crate::log_error!("tls: certificate unusable ({err:#}); serving ws://");
                (None, plain_named(hostname, bind))
            }
        },
        Err(err) => {
            crate::log_warn!("tls: no certificate for {hostname} ({err:#}); serving ws://");
            // Measured on this tailnet: the failure is an account setting, not
            // a fault, and it is not discoverable from the error alone. Saying
            // where the switch is turns a dead end into a two-click fix.
            if format!("{err:#}").contains("does not support getting TLS certs") {
                crate::log_warn!(
                    "tls: enable HTTPS Certificates for this tailnet (Tailscale admin console \
                     -> DNS -> HTTPS Certificates), then restart ccd to pick up a certificate"
                );
            }
            // The DNS name still resolves on the tailnet, so it stays the QR
            // host: it survives a Tailscale IP change, which an IP literal does
            // not, and it is what the phone will need once TLS does come up.
            (None, plain_named(hostname, bind))
        }
    }
}

fn plain_named(hostname: String, bind: SocketAddr) -> Endpoint {
    Endpoint {
        host: hostname,
        port: bind.port(),
        tls: false,
    }
}

/// The static bearer token: one long-lived credential, owner-readable only.
///
/// QR-delivered per-device tokens live alongside it, but this one stays valid. A
/// phone that paired before per-device tokens existed must not stop working
/// because the Mac was upgraded, and there has to be a credential available when
/// no phone has paired yet.
fn load_or_create_token(path: &Path) -> Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            // A token file written by a daemon that predates the boundary is
            // still whatever the umask made it, and this is the one path that
            // reads it without rewriting it — so it is the one that has to
            // repair it. `harden_state_dir` does this too; doing it here as
            // well means the credential is never returned from a loose file
            // even if the sweep above failed on it.
            protocol::fsperm::harden_file(path)
                .with_context(|| format!("securing {}", path.display()))?;
            return Ok(trimmed);
        }
    }
    let token = protocol::hash::sha256_hex(&secret::random_bytes::<32>()?);
    protocol::fsperm::write_private(path, format!("{token}\n").as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    crate::log_info!("generated a static token at {}", path.display());
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SessionRow;
    use protocol::event::{EventKind, Lifecycle, PendingEvent, SessionKey, Source};

    fn temp_store() -> Store {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ccd-integrity-{}-{}-{}.db",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        Store::open(&path).unwrap()
    }

    fn seed(store: &Store, uid: &str, events: u32) -> SessionKey {
        let key = SessionKey::new(uid, "cc-1");
        let now = protocol::time::now_rfc3339();
        store
            .upsert_session(&SessionRow {
                session_uid: key.uid.clone(),
                session_id: key.name.clone(),
                tmux_session: key.name.clone(),
                tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
                cwd: "/tmp".into(),
                claude_session_id: None,
                transcript_path: None,
                lifecycle: Lifecycle::Live,
                created_at: now.clone(),
                updated_at: now,
            })
            .unwrap()
            .assert_present();
        for i in 0..events {
            store
                .append_event(
                    &PendingEvent::new(
                        &key,
                        EventKind::ToolCall,
                        serde_json::json!({"i": i}),
                        Source::Hook,
                    )
                    .with_source_event_id(format!("e{i}")),
                )
                .unwrap();
        }
        key
    }

    fn scratch_root(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ccd-perm-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn mode(path: &Path) -> u32 {
        protocol::fsperm::mode_of(path)
            .unwrap_or_else(|err| panic!("stat {}: {err}", path.display()))
    }

    #[test]
    fn the_state_directory_is_owner_only_from_the_moment_it_exists() {
        // SECURITY.md says the database is "protected by file permissions".
        // Before this, every directory was `create_dir_all` under the umask —
        // `0755` under the macOS login default — so the event log, which is a
        // verbatim copy of every transcript line an agent produced, was
        // readable by every other account on the Mac.
        let root = scratch_root("fresh");
        harden_state_dir(&root).unwrap();
        for dir in state_paths(&root).dirs {
            assert_eq!(mode(&dir), 0o700, "{} is not owner-only", dir.display());
        }
    }

    #[test]
    fn an_installation_made_before_the_boundary_is_repaired_at_startup() {
        // The case that actually matters: the loose modes are already on disk.
        // A boundary enforced only on *new* state protects nobody who has run
        // the daemon before, and the token file is the one credential the old
        // code never repaired at all.
        use std::os::unix::fs::PermissionsExt;
        let root = scratch_root("legacy");
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(root.join("logs"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let token = root.join("token");
        std::fs::write(&token, "deadbeef\n").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        let db = root.join("events.db");
        std::fs::write(&db, b"").unwrap();
        std::fs::write(sidecar(&db, "-wal"), b"").unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(sidecar(&db, "-wal"), std::fs::Permissions::from_mode(0o644))
            .unwrap();

        harden_state_dir(&root).unwrap();

        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("logs")), 0o700);
        assert_eq!(
            mode(&token),
            0o600,
            "the static token stayed world-readable"
        );
        assert_eq!(mode(&db), 0o600, "the event log stayed world-readable");
        assert_eq!(mode(&sidecar(&db, "-wal")), 0o600);
        // Repair must never disturb the credential itself.
        assert_eq!(std::fs::read_to_string(&token).unwrap(), "deadbeef\n");
    }

    #[test]
    fn the_token_is_created_owner_only_and_a_loose_one_is_repaired_on_read() {
        let root = scratch_root("token");
        protocol::fsperm::private_dir(&root).unwrap();
        let path = root.join("token");

        let created = load_or_create_token(&path).unwrap();
        assert!(!created.is_empty());
        assert_eq!(mode(&path), 0o600);

        // A file written by a daemon that predates the boundary: the read path
        // is the only one that touches it, so it is the one that has to repair
        // it rather than hand back a credential out of a world-readable file.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let reread = load_or_create_token(&path).unwrap();
        assert_eq!(reread, created, "the existing token must be preserved");
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn opening_the_database_leaves_it_and_its_sidecars_owner_only() {
        // The subtle half: SQLite creates `-wal`/`-shm` itself, copying the
        // main database's mode. If the database is only tightened *after* the
        // `journal_mode=WAL` pragma, the sidecars keep the umask's mode and
        // carry the newest, not-yet-checkpointed facts in the log.
        let root = scratch_root("sqlite");
        let db = root.join("events.db");
        let store = Store::open(&db).unwrap();
        seed(&store, "01K1B3XQ8ZC0DE5FGH7JKMNPQR", 3);

        assert_eq!(mode(&root), 0o700, "the parent directory");
        assert_eq!(mode(&db), 0o600);
        for suffix in ["-wal", "-shm"] {
            let path = sidecar(&db, suffix);
            if path.exists() {
                assert_eq!(mode(&path), 0o600, "{} is not owner-only", path.display());
            }
        }
    }

    #[test]
    fn the_boundary_covers_every_path_the_daemon_writes() {
        // `state_paths` derives from an explicit root so it can be tested; this
        // is what keeps that derivation in step with the protocol crate's own
        // idea of where things live. Both sides read `root_dir()` at the same
        // instant, so no environment variable has to be moved to compare them.
        let root = protocol::root_dir();
        let paths = state_paths(&root);
        for expected in [
            protocol::sessions_dir(),
            protocol::logs_dir(),
            protocol::tls_dir(),
        ] {
            assert!(
                paths.dirs.contains(&expected),
                "{} is written by the daemon but outside the boundary",
                expected.display()
            );
        }
        for expected in [
            protocol::token_path(),
            protocol::db_path(),
            protocol::config_path(),
            protocol::daemon_stdout_log(),
            protocol::daemon_stderr_log(),
        ] {
            assert!(
                paths.files.contains(&expected),
                "{} is written by the daemon but outside the boundary",
                expected.display()
            );
        }
    }

    #[test]
    fn a_healthy_log_is_reported_as_intact() {
        let store = temp_store();
        seed(&store, "01K1B3XQ8ZC0DE5FGH7JKMNPQR", 4);
        let integrity = check_log_integrity(&store).unwrap();
        assert_eq!(
            integrity,
            LogIntegrity {
                checked: 1,
                holes: 0,
                unchecked: 0
            }
        );
        assert!(integrity.is_clean());
        assert!(integrity.summary().contains("sequences intact"));
    }

    #[test]
    fn a_log_with_a_hole_is_never_also_reported_as_intact() {
        // The startup banner used to say "sequences intact" on the line *after*
        // it had just reported a hole. A log with a gap in it serves replays
        // that look perfectly well-formed to a client, so the banner is the only
        // place anybody would ever find out — and it was lying.
        let store = temp_store();
        let key = seed(&store, "01K1B3XQ8ZC0DE5FGH7JKMNPQR", 5);
        store.punch_hole_for_tests(&key.uid, 3);

        let integrity = check_log_integrity(&store).unwrap();
        assert_eq!(integrity.holes, 1);
        assert!(!integrity.is_clean());
        let summary = integrity.summary();
        assert!(summary.contains("hole"), "{summary}");
        assert!(
            !summary.contains("intact"),
            "a log with a hole must not also be described as intact: {summary}"
        );
    }

    #[test]
    fn a_check_that_could_not_run_is_not_a_clean_bill_of_health() {
        let store = temp_store();
        let empty = check_log_integrity(&store).unwrap();
        assert!(empty.is_clean(), "no runs is genuinely nothing wrong");

        // A run whose events could not be counted is *unknown*, and the summary
        // has to say so rather than folding it into the intact count.
        let unknown = LogIntegrity {
            checked: 2,
            holes: 0,
            unchecked: 1,
        };
        assert!(!unknown.is_clean());
        assert!(
            unknown.summary().contains("unknown"),
            "{}",
            unknown.summary()
        );
        assert!(!unknown.summary().contains("sequences intact"));
    }
}
