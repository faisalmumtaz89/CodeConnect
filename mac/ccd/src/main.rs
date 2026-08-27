//! `ccd` — the CodeConnect daemon.
//!
//! It is a notifier, card-builder and event log. It is deliberately *not* a
//! connectivity layer (Tailscale is) and deliberately *not* the agents' parent
//! (the per-session supervisor connects out to it). `kill -9 ccd` must cost
//! nothing but a reconnect, which is why every piece of session state either
//! lives in SQLite or is re-derived when a supervisor re-registers.

mod apns;
mod apns_sender;
mod catalog;
mod codex_adapter;
mod codex_link;
/// The gated live gate for the control link — a real codex, a real coordinator, a
/// real broker. Test-only, and never built into the daemon.
#[cfg(test)]
mod codex_link_live;
mod db;
#[cfg(test)]
mod fixture_replay;
mod git;
mod ipc_server;
mod legacy_credentials;
mod liveness;
mod log;
mod logrotate;
mod project_label;
mod push_gate;
mod push_queue;
mod relay_sender;
mod secret;
mod state;
mod store;
mod tailer;
mod terminal;
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
    let push = crate::apns::build(&config, Arc::clone(&store));

    let plan = resolve_bind(&config).await;
    let bind = plan.bind;
    tls::install_crypto_provider();
    let (acceptor, endpoint) = resolve_transport(&config, &plan).await;
    let certificate = acceptor.is_some();
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
    // Straight after the banner that names the bind, and before anything is
    // served: an operator whose listener will answer nobody has to learn it
    // here, with both ways out of it, not from a phone that silently never
    // connects.
    if let Some(advice) = plaintext_refusal_advice(&daemon.config, &plan, certificate) {
        crate::log_error!("{advice}");
    }
    report_log_integrity(&store);
    // Before anything is served: a grant an earlier release left behind outlives
    // the token it was installed beside, so the sweep takes back what it can
    // reach before any of it is reachable. `Daemon::revoke` sweeps too, and this
    // caller is what covers the Mac nobody revokes anything on — and the upgrade
    // itself, which is the moment the entries stop having any use. It is
    // best-effort by contract — five documented paths remove nothing and warn
    // instead, the file changing under the sweep among them — so neither caller
    // closes the gap between revoking a token and
    // revoking a phone's access; they narrow it, and say where they could not.
    // What settles whether a particular key is gone is the operator's own look
    // at the file, which every one of those four warnings hands over.
    legacy_credentials::purge_authorized_keys();
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
        plan.trust,
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
            // Loopback never leaves the machine, so plaintext is private on it
            // whatever the main listener's address allowed.
            let trust = ws_server::PlaintextTrust::TrustedPath;
            if let Err(err) = ws_server::serve(daemon, loopback, token, acceptor, trust).await {
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

    // **A12.2.** A registration whose parked link will not stop inside the link stop
    // budget is accepted with no link installed: Live in the fleet, observed by
    // nothing. No other part of the daemon ever builds that link — `codex_link::run`
    // never returns on its own, so a link never vacates the slot, and the only other
    // builder is the next registration for that uid. This is the retry.
    //
    // **A12.2 IS NOT CLOSED, and this is the mechanism half only.** What this ticker
    // guarantees is that a link gets BUILT. Which thread that link then binds is a
    // second question and it is open: a first thread or a `/new` emitting its
    // one-shot `thread/started` during the accepted observer gap is seen by nobody,
    // and recovery has only the predecessor's carry and the registration's hint to
    // chase — neither of which can name a thread that appeared while nothing was
    // watching. With no hint the link stays unbound; with a stale hint the broker
    // accepts a resume of a RETIRED thread (`is_session_thread` widens resume to
    // retired threads on purpose, for 2e-4c switching) while the active head goes
    // undiscovered.
    //
    // The blocker is that ccd cannot ask. The head lives in the broker's own
    // `Binding::creation`, `thread/started` is broadcast once and never replayed, and
    // no ccd-allowlisted method reports the binding — the broker can synthesize an
    // error and nothing else. Closing it needs new wire (the head replayed on
    // subscribe, or a head query), which is a design chunk, not a patch. Dormant
    // meanwhile: the registration hint has no production producer (the supervisor
    // sends `codex_thread_id: None`) and the command stays gated.
    //
    // Its own ticker rather than a limb of the liveness sweep: that one shells out
    // and can be turned off entirely (`liveness_sweep_secs: 0`), and this must keep
    // running when it is.
    //
    // Ten seconds. Each attempt can pay the full stop budget for the session it is
    // retrying, so a shorter period buys nothing but contention on that session's
    // registration gate; and a pass is a single uncontended lock while nothing is
    // owed, which in production today is always.
    let codex_recovery = {
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(10));
            // Delay rather than burst, exactly as the liveness sweep does — but for
            // the reason it actually gives, which is not the one this comment used
            // to claim. `Delay` does NOT withhold the overdue tick: measured, a pass
            // that outran its period gets its next tick in ~1µs under all three
            // behaviours. What it withholds is the CATCH-UP — the tick after the
            // overdue one is a full period away (~103ms) instead of instant (~125ns
            // under `Burst`). Every tick's work here is identical, so a burst of
            // them buys nothing and only contends on the registration gates.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                daemon.recover_stalled_codex_links().await;
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

    // Spawned, never awaited here: a daemon that waited for a tailnet before
    // opening its sockets would be exactly the startup dependency the three
    // second probe budget exists to refuse. On two of its three paths it never
    // completes at all.
    let tailnet_watch = tokio::spawn(watch_for_tailnet(plan.watch_tailnet));

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
        result = codex_recovery => Stop::Task(format!("codex link recovery exited: {result:?}")),
        result = rotate => Stop::Task(format!("log rotator exited: {result:?}")),
        // The one arm that is a deliberate exit rather than a failure: a
        // tailnet address turned up after this daemon had already fallen back
        // to loopback, and only a fresh process can probe for it again, bind it
        // and advertise it — with a certificate if one can be had, and over the
        // WireGuard path if not. It has already said so in the log. The
        // watcher completes only where launchd's job is there to provide that
        // process; started by hand it reports and keeps watching instead, so
        // this arm is never how a hand-started daemon ends.
        result = tailnet_watch => Stop::Task(format!("tailnet watcher exited: {result:?}")),
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

/// Everything one start's listener decisions come to.
///
/// A plain value, derived by [`plan_bind`] from the config and the addresses the
/// tailnet probe returned and nothing else. The probe is the only I/O in the
/// question, so keeping it outside is what makes every branch below assertable
/// rather than reachable only on a machine with Tailscale in a particular mood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BindPlan {
    /// Where the WebSocket listener goes.
    bind: SocketAddr,
    /// Whether plaintext is private on that address.
    trust: ws_server::PlaintextTrust,
    /// How the address was arrived at.
    origin: BindOrigin,
    /// What the operator's `ws_allow_plaintext` came to here.
    opt_in: PlaintextOptIn,
    /// Whether this node's own tailnet addresses are known.
    ///
    /// False means the probe came back with nothing — `tailscale ip` did not
    /// answer in time, or Tailscale is not running — and *not* that this node
    /// has no tailnet. The difference is the whole of what a refused operator
    /// is told: an address missing from a tailnet the daemon can see is
    /// genuinely not on it, while an address it could never check may be
    /// exactly the tailnet address they meant, and the way through is a restart
    /// rather than a different address. A loopback bind leaves this false
    /// without probing at all, which is inert — loopback is trusted by
    /// construction and is never refused.
    tailnet_confirmed: bool,
    /// Whether the bind is one of this node's own tailnet addresses.
    ///
    /// Distinct from [`BindOrigin::Tailnet`], which says how the address was
    /// *chosen*: an operator who writes their own tailnet address into
    /// `ws_bind` arrives at [`BindOrigin::Explicit`] and is on the tailnet all
    /// the same. What rests on it is [`resolve_transport`]: a listener on this
    /// node's tailnet address is reachable at this node's MagicDNS name by
    /// construction, which is the one case where the QR's name needs no lookup
    /// to be honest.
    ///
    /// False whenever the probe came back empty, so an address that could not
    /// be checked is never *asserted* to be on the tailnet. What follows from
    /// that is a lookup, not a refusal: a `ws_bind` onto this node's own tailnet
    /// address whose probe lost the three-second race has its name resolved
    /// instead of assumed, and a resolver that stays silent leaves the name
    /// unverified rather than refuted — a distinction
    /// [`advertised_name_reach`] draws precisely so a certificate this daemon
    /// already holds keeps serving. The probe answering on a later start makes
    /// this true again and settles the question without any lookup at all.
    bind_is_tailnet: bool,
    /// What this start does about a tailnet address that is not there yet.
    watch_tailnet: TailnetWatch,
}

/// How the bind address was arrived at.
///
/// The distinction that matters is between the two loopbacks: one an operator
/// asked for, which is working as intended, and one the daemon fell back to,
/// which is a daemon no phone can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindOrigin {
    /// `ws_bind` named this address.
    Explicit,
    /// This node's own tailnet address, which is what the QR advertises.
    Tailnet,
    /// Nothing else was available. The phone cannot reach this daemon.
    LoopbackFallback,
}

/// What this start does about a tailnet address that is not there yet.
///
/// Two questions, kept apart because answering them as one produced a daemon
/// that promised a recovery nothing would perform. *Whether there is anything
/// to watch for* is decided by the bind: a daemon in the loopback fallback is
/// unreachable, and it is unreachable however it was started. *Whether ending
/// the process is a recovery* is decided by launchd: only CodeConnect's own job
/// starts `ccd` again, and under anything else an exit is simply the end of the
/// daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailnetWatch {
    /// Nothing to wait for: an address was found, or the operator named one. A
    /// tailnet address arriving later changes neither.
    Idle,
    /// Unreachable, and CodeConnect's own launchd job manages this process — so
    /// the watcher ends it, and launchd starts one that can bind the tailnet
    /// address and advertise it, with a certificate if one can be had and
    /// plaintext over WireGuard if not.
    Restart,
    /// Unreachable, and nothing here would start this daemon again. The watcher
    /// reports the address a restart would make it reachable at and never ends
    /// the process, which would kill a working daemon nobody would replace.
    Report,
}

/// What `ws_allow_plaintext` came to on this bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaintextOptIn {
    /// The key is off, or the address is one where plaintext was private
    /// already and the key therefore changed nothing.
    Inert,
    /// Honoured: the bind is on a local network, and plaintext crosses it in
    /// the clear on the operator's instruction.
    Honoured,
    /// Set and refused: a wildcard or public address, which is not a network
    /// anybody can vouch for.
    Refused,
}

/// Bind to the tailnet address so the listener is not reachable from the LAN.
///
/// launchd gives the daemon no shell PATH, so `tailscale` is looked up at known
/// install locations rather than via `which`.
///
/// Where to listen and whether plaintext is private there are decided together,
/// because they rest on the same fact — this node's tailnet addresses. This
/// function is the I/O half: decide whether the probe is needed, run it, hand
/// the answer to [`plan_bind`], and say out loud anything the operator has to
/// know. An explicit `ws_bind` is honoured either way; one that is not loopback
/// and not a tailnet address becomes TLS-only unless `ws_allow_plaintext` says
/// the network it is on is acceptable.
async fn resolve_bind(config: &Config) -> BindPlan {
    if let Some(explicit) = &config.ws_bind {
        if explicit_bind_ip(config).is_none() {
            crate::log_warn!("ws_bind {explicit:?} is not an IP address; ignoring it");
        }
    }

    let tailnet = if needs_tailnet_probe(config) {
        tailnet_addresses().await
    } else {
        Vec::new()
    };
    let plan = plan_bind(config, &tailnet, managed_by_codeconnect_job());

    match plan.watch_tailnet {
        // Everything here is conditional on a tailnet address turning up, which
        // is the one thing this daemon cannot bring about. What it can promise
        // is the part it performs itself: it keeps looking for as long as it
        // runs, and when looking succeeds it ends so CodeConnect's launchd job
        // starts a fresh one. That fresh process is owed no address — it probes
        // on its own [`TAILSCALE_PROBE_TIMEOUT`] like this one did, so an
        // address that has gone again, or a `tailscale ip` that loses the same
        // race twice, binds loopback and arrives back in this same watch. The
        // promise is the retrying, not the landing. A Mac that never runs
        // Tailscale never leaves this state, and saying otherwise would be the
        // promise this whole watch exists to stop making.
        TailnetWatch::Restart => crate::log_warn!(
            "no tailnet IPv4 address found; binding loopback (the phone will not reach it). \
             Watching for one for as long as this process runs: launchd's {} job manages this \
             daemon, so when one appears this process ends and launchd starts another, which \
             tries the address for itself. Until a start finds one in time, this daemon stays \
             on loopback and no phone reaches it.",
            protocol::LAUNCHD_LABEL
        ),
        // The same watch, and the honest form of the same sentence. Nothing
        // here restarts a daemon CodeConnect's job does not manage, so what the
        // watcher has to offer is to say when there is an address a restart
        // could bind, and to keep saying so for as long as that stays true.
        TailnetWatch::Report => crate::log_warn!(
            "no tailnet IPv4 address found; binding loopback (the phone will not reach it). \
             Watching for one to appear and reporting it when it does — this process is not \
             run by launchd's {} job, so nothing here restarts it. Making this daemon \
             reachable is a restart you run.",
            protocol::LAUNCHD_LABEL
        ),
        TailnetWatch::Idle => {}
    }

    match plan.opt_in {
        PlaintextOptIn::Honoured => crate::log_warn!(
            "ws_allow_plaintext is set, so {} serves plaintext on your instruction. The \
             bearer token and everything a connection carries cross that network in the \
             clear, and are exactly as private as it is. The Terminal tab is the one thing \
             not offered on it: a live shell's keystrokes are not something this daemon \
             sends in the clear. Provide a certificate, or bind loopback or this Mac's \
             tailnet address, and the terminal is available again.",
            plan.bind
        ),
        PlaintextOptIn::Refused => crate::log_warn!("{}", refused_opt_in_message(&plan)),
        PlaintextOptIn::Inert => {}
    }

    plan
}

/// The address `ws_bind` names, when it names one at all.
fn explicit_bind_ip(config: &Config) -> Option<IpAddr> {
    config.ws_bind.as_ref()?.parse::<IpAddr>().ok()
}

/// Whether this start has to ask `tailscale ip` anything.
///
/// An explicit loopback bind does not: it is trusted by construction and needs
/// no address discovered, and its whole point is a daemon that starts even when
/// the tailnet is unavailable — so it must not spend
/// [`TAILSCALE_PROBE_TIMEOUT`] waiting on a service it does not depend on.
/// Every other case needs the answer, either to classify an explicit address or
/// to have an address at all.
fn needs_tailnet_probe(config: &Config) -> bool {
    !matches!(explicit_bind_ip(config), Some(ip) if ip.is_loopback())
}

/// The tailnet address the daemon prefers to bind: the IPv4 one, because that
/// is what the QR advertises.
///
/// One function, two callers, deliberately: [`plan_bind`] picks the bind with
/// it and [`watch_for_tailnet`] decides with it that a restart would now do
/// better. If those two ever disagreed, a tailnet that is up but IPv6-only
/// would restart the daemon into the same loopback fallback, for ever.
fn preferred_tailnet_ip(tailnet: &[IpAddr]) -> Option<IpAddr> {
    tailnet.iter().copied().find(|ip| ip.is_ipv4())
}

/// Whether an operator can meaningfully vouch for the privacy of an address —
/// which is the whole of what `ws_allow_plaintext` is allowed to cover.
///
/// Deliberately narrow. The unspecified address is excluded first and by name:
/// `0.0.0.0` and `::` are exactly the case where the operator cannot know which
/// interfaces they have just put a bearer token on, so there is nothing there
/// to vouch for. A public address is excluded because plaintext on it is
/// indefensible. Both have the same way through — bind the LAN address itself —
/// which is why refusing is a redirection rather than a dead end.
///
/// The IPv6 ranges are hand-rolled from `segments()` because
/// `Ipv6Addr::is_unique_local` and `is_unicast_link_local` are still unstable
/// and this workspace builds on a fixed minimum toolchain;
/// [`protocol::pairing::unreachable_host`] classifies link-local the same way
/// for the same reason.
fn is_local_network(ip: IpAddr) -> bool {
    if ip.is_unspecified() {
        return false;
    }
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        // `fc00::/7` unique-local and `fe80::/10` link-local.
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Tailscale's own IPv6 range, `fd7a:115c:a1e0::/48`.
///
/// Named separately because it sits inside `fc00::/7`, which every other
/// unique-local address shares — so "is this a private range" and "could this be
/// a tailnet address" have different answers here, and only this prefix
/// distinguishes them.
fn is_tailscale_ula(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            segments[0] == 0xfd7a && segments[1] == 0x115c && segments[2] == 0xa1e0
        }
        IpAddr::V4(_) => false,
    }
}

/// Whether an address is one Tailscale never hands out, so a daemon that could
/// not reach `tailscale ip` can still classify it with certainty.
///
/// [`is_local_network`] answers the different question of what an operator may
/// vouch for, and is not a safe stand-in: it admits the whole of `fc00::/7`,
/// and Tailscale allocates this node's IPv6 address out of
/// `fd7a:115c:a1e0::/48` inside it. An address there may be exactly the tailnet
/// address the operator meant, so a daemon that could not check has no ground
/// to call it public — while every other local-network address is one Tailscale
/// never assigns, which an unconfirmed probe does not change.
fn never_a_tailnet_address(ip: IpAddr) -> bool {
    is_local_network(ip) && !is_tailscale_ula(ip)
}

/// Turn the config and this node's tailnet addresses into one start's plan.
///
/// Pure, and the only place these choices are made.
/// Whether the process is run by CodeConnect's own launchd job — the one thing
/// that starts `ccd` again after the tailnet watcher ends it.
///
/// **CodeConnect's own job, not merely some job.** macOS sets
/// `XPC_SERVICE_NAME` for every process a launchd job starts, so a `ccd` run
/// from inside another one — a build step, a wrapper script's agent — carries a
/// label that will never restart it. Equality with [`protocol::LAUNCHD_LABEL`]
/// is what tells the two apart; anything looser reads "a label exists" as
/// "something will restart me", which is exactly the promise this must not
/// make on a process nothing is watching.
fn managed_by_codeconnect_job() -> bool {
    state::launchd_label().as_deref() == Some(protocol::LAUNCHD_LABEL)
}

/// Held for the length of any test that writes `XPC_SERVICE_NAME`, by way of
/// [`LaunchdLabelEnv`] — which is the only thing that can take it, and the only
/// place in this crate that writes the variable at all.
///
/// The variable is process-global and `cargo test` runs the crate's tests on a
/// thread pool, so two tests writing it are one interleaving away from reading
/// each other's value. Two do: [`managed_by_codeconnect_job`] is asserted from
/// this module and [`state::launchd_label`] from `state`, both by setting the
/// variable and reading back what the production code makes of it. A lock taken
/// by one of them excludes nothing, because the other never takes it — which is
/// exactly the shape those two were in, one holding a lock private to itself
/// while the other wrote the same variable freely and neither could see the
/// other doing it.
///
/// So it lives at the crate root, above the function whose answer it protects,
/// where both modules reach it the same way and a third test that starts
/// writing that variable has one obvious thing to use. `#[cfg(test)]` because
/// production reads this variable and never writes it: outside a test run there
/// is nothing here to serialise.
#[cfg(test)]
static LAUNCHD_LABEL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Exclusive use of `XPC_SERVICE_NAME` for as long as this value is alive.
///
/// Bundled rather than left as a bare lock because the two mistakes here are
/// the same mistake: writing the variable without excluding the other test, and
/// leaving somebody else's value overwritten afterwards. Taking the lock and
/// capturing the old value is one act, and restoring is [`Drop`]'s, so a test
/// that fails an assertion mid-loop still hands the environment back the way it
/// found it.
///
/// The old value is kept as an [`std::ffi::OsString`], not a `String`.
/// `std::env::var(..).ok()` folds "unset" together with "set to bytes that are
/// not UTF-8", and restoring from that would silently *delete* a variable it
/// could not read rather than put it back.
///
/// Poisoning is ignored on purpose: the data this lock guards is the process
/// environment, and a panicking holder has already restored it on the way out,
/// so the next test inherits a clean variable rather than a permanently
/// unusable lock.
#[cfg(test)]
pub(crate) struct LaunchdLabelEnv {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl LaunchdLabelEnv {
    pub(crate) fn take() -> Self {
        Self {
            _lock: LAUNCHD_LABEL_ENV_LOCK
                .lock()
                .unwrap_or_else(|held| held.into_inner()),
            previous: std::env::var_os("XPC_SERVICE_NAME"),
        }
    }

    /// `None` is "no launchd job set one", which is an absent variable rather
    /// than an empty one — the two are different inputs to
    /// [`state::launchd_label`] and both have to be reachable from a test.
    pub(crate) fn set(&self, label: Option<&str>) {
        match label {
            Some(value) => std::env::set_var("XPC_SERVICE_NAME", value),
            None => std::env::remove_var("XPC_SERVICE_NAME"),
        }
    }
}

#[cfg(test)]
impl Drop for LaunchdLabelEnv {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("XPC_SERVICE_NAME", value),
            None => std::env::remove_var("XPC_SERVICE_NAME"),
        }
    }
}

fn plan_bind(config: &Config, tailnet: &[IpAddr], managed_by_codeconnect_job: bool) -> BindPlan {
    if let Some(ip) = explicit_bind_ip(config) {
        let (trust, opt_in) = plaintext_admission(config, ip, tailnet);
        return BindPlan {
            bind: SocketAddr::new(ip, config.ws_port),
            trust,
            origin: BindOrigin::Explicit,
            opt_in,
            tailnet_confirmed: !tailnet.is_empty(),
            bind_is_tailnet: tailnet.contains(&ip),
            // An operator who named an address is not waiting for a different
            // one, and exiting on them would be a daemon that will not stay up.
            watch_tailnet: TailnetWatch::Idle,
        };
    }

    if let Some(ip) = preferred_tailnet_ip(tailnet) {
        return BindPlan {
            bind: SocketAddr::new(ip, config.ws_port),
            trust: ws_server::PlaintextTrust::TrustedPath,
            origin: BindOrigin::Tailnet,
            opt_in: PlaintextOptIn::Inert,
            tailnet_confirmed: true,
            bind_is_tailnet: true,
            watch_tailnet: TailnetWatch::Idle,
        };
    }

    BindPlan {
        bind: SocketAddr::new(IpAddr::from([127, 0, 0, 1]), config.ws_port),
        trust: ws_server::PlaintextTrust::TrustedPath,
        origin: BindOrigin::LoopbackFallback,
        opt_in: PlaintextOptIn::Inert,
        tailnet_confirmed: !tailnet.is_empty(),
        bind_is_tailnet: false,
        // The one state the daemon cannot get itself out of, so it is watched
        // either way — it is equally unreachable either way. What launchd
        // decides is not whether to look, but whether ending this process is a
        // recovery or merely a death.
        watch_tailnet: if managed_by_codeconnect_job {
            TailnetWatch::Restart
        } else {
            TailnetWatch::Report
        },
    }
}

/// Whether plaintext is admissible on `ip`, and what the opt-in did about it.
///
/// The fail-closed classification in [`ws_server::plaintext_trust`] is the
/// policy and is asked first. `ws_allow_plaintext` can only ever soften the
/// answer, only on a network the operator can vouch for, and never against
/// `tls_required` — which is the stronger, opposite statement, and honouring an
/// exception to it would announce plaintext the admission gate then refuses.
///
/// Softened is [`ws_server::PlaintextTrust::OperatorAllowed`] and **not**
/// `TrustedPath`: the operator vouched for their LAN, which admits the
/// connection, and vouching for a network is not the same statement as the
/// bytes on it being unreadable. Everything gated on the second — the terminal
/// — asks for `TrustedPath` and does not get it here.
fn plaintext_admission(
    config: &Config,
    ip: IpAddr,
    tailnet: &[IpAddr],
) -> (ws_server::PlaintextTrust, PlaintextOptIn) {
    match ws_server::plaintext_trust(ip, tailnet) {
        trusted @ ws_server::PlaintextTrust::TrustedPath => (trusted, PlaintextOptIn::Inert),
        // `plaintext_trust` is the fail-closed classifier and answers only
        // these two; the opt-in below is the only source of the third.
        allowed @ ws_server::PlaintextTrust::OperatorAllowed => (allowed, PlaintextOptIn::Inert),
        ws_server::PlaintextTrust::RequireTls => {
            if !config.ws_allow_plaintext || config.tls_required {
                (ws_server::PlaintextTrust::RequireTls, PlaintextOptIn::Inert)
            } else if is_local_network(ip) {
                (
                    ws_server::PlaintextTrust::OperatorAllowed,
                    PlaintextOptIn::Honoured,
                )
            } else {
                (
                    ws_server::PlaintextTrust::RequireTls,
                    PlaintextOptIn::Refused,
                )
            }
        }
    }
}

/// Why `ws_allow_plaintext` was not honoured, in the operator's terms.
///
/// Three situations that [`PlaintextOptIn::Refused`] collapses into one, and
/// only two of them are a statement about the address: a wildcard is nobody's
/// tailnet address, and an address checked against a tailnet the daemon can see
/// is genuinely not on it. The third — an address it could not check, because
/// the probe came back with nothing — may be exactly the tailnet address the
/// operator meant, and on that one the way through is a restart rather than a
/// different address.
///
/// Which is why this is a function and not an interpolated clause: it is logged
/// immediately before [`plaintext_refusal_advice`], which makes the same
/// distinction, and two adjacent lines disagreeing about whether an address is
/// public would leave the operator with no way to tell which to believe.
fn refused_opt_in_message(plan: &BindPlan) -> String {
    let bind = plan.bind;
    let ip = bind.ip();
    // Held in common: an address that is refused here is refused because it is
    // not one anybody can vouch for, and a certificate is what serves clients
    // on it regardless of which of the three it is.
    let tail = "Bind this Mac's own LAN address and it is honoured there; on this one only a \
                certificate makes the listener serve anybody.";
    if ip.is_unspecified() {
        return format!(
            "ws_allow_plaintext is set but is not honoured on {bind}: a wildcard bind puts the \
             bearer token on every interface this Mac has, including the ones nobody can \
             enumerate in advance. {tail}"
        );
    }
    if !plan.tailnet_confirmed {
        return format!(
            "ws_allow_plaintext is set but is not honoured on {bind}: `tailscale ip` did not \
             answer inside the {}s probe budget, so this daemon could not confirm whether {ip} \
             is one of its own tailnet addresses, and it does not vouch for an address it could \
             not check. If it is your tailnet address, bring Tailscale up and restart — \
             plaintext is private there, with or without this key.",
            TAILSCALE_PROBE_TIMEOUT.as_secs()
        );
    }
    format!(
        "ws_allow_plaintext is set but is not honoured on {bind}: plaintext on a public address \
         is private on no hop of the way. {tail}"
    )
}

/// The one line that turns a daemon refusing every connection into something
/// fixable, or `None` when this listener will serve somebody.
///
/// `ws_server::serve` already says that a listener is refusing everything, but
/// it cannot say what to do about it: it knows the address and whether there is
/// a certificate, not which addresses an operator is permitted to vouch for.
/// This does, so it names both ways through and picks the pair that are true of
/// *this* address — offering `ws_allow_plaintext` on a wildcard or public bind
/// would be advice that does not work when followed. For the same reason it
/// separates an address known not to be on the tailnet from one that could not
/// be checked against it at all: those look identical in the classification and
/// are different situations with different remedies. And it carries the one
/// condition the certificate route has here — the name that certificate is for
/// has to be one [`advertised_name_reach`] does not answer
/// [`AdvertisedName::Misses`] on — because an instruction that is followed and
/// then does not work is worse than none.
///
/// Silent when `tls_required` is set. That refusal is precisely what the
/// operator asked for, plaintext is on offer at no address, and the only thing
/// left to say — that a certificate is missing — has already been said.
fn plaintext_refusal_advice(config: &Config, plan: &BindPlan, certificate: bool) -> Option<String> {
    if certificate || config.tls_required || plan.trust != ws_server::PlaintextTrust::RequireTls {
        return None;
    }
    let bind = plan.bind;
    let ip = bind.ip();

    // **An address the daemon could not check is not an address it may call
    // public.** `ws_bind` set to this node's own tailnet address is an ordinary
    // thing to do, and at login `tailscale ip` can lose the three-second race —
    // which lands a genuinely private address here with an empty tailnet behind
    // it. Saying "plaintext is not private on this address" would be false, and
    // sending them to a LAN address would fix nothing: the situation is that
    // this daemon could not confirm its own tailnet, and the way through is to
    // start again once it can.
    //
    // Excluded from this: the wildcard, which is nobody's tailnet address
    // whatever the probe did, and the IPv4 addresses Tailscale never allocates
    // — for those the classification below is certain with nothing to compare
    // against. [`never_a_tailnet_address`] draws that line rather than
    // [`is_local_network`], which admits `fc00::/7` and so would sweep in the
    // very range Tailscale hands its own IPv6 addresses out of: an operator
    // bound to their tailnet IPv6 address would be told plaintext is not
    // private there, which WireGuard makes false, while the IPv4 half of the
    // same tailnet got the accurate answer from this same function.
    if !plan.tailnet_confirmed && !ip.is_unspecified() && !never_a_tailnet_address(ip) {
        return Some(format!(
            "ws {bind} will REFUSE EVERY CONNECTION: `tailscale ip` did not answer inside the \
             {}s probe budget at startup, so this daemon could not confirm whether {ip} is one \
             of its own tailnet addresses — and plaintext is admitted only on an address it can \
             vouch for. If {ip} is your tailnet address, bring Tailscale up and run `codeconnect \
             daemon restart` (`codeconnect daemon status` shows what it is bound to). If it is \
             not, obtain a certificate (`tailscale cert`, which needs HTTPS Certificates enabled \
             for this tailnet).",
            TAILSCALE_PROBE_TIMEOUT.as_secs(),
        ));
    }

    let way_through = if is_local_network(ip) {
        format!(
            "or, if you trust that network, set \"ws_allow_plaintext\": true in {}",
            protocol::config_path().display()
        )
    } else {
        "or bind this Mac's own LAN address instead: plaintext is never opted into on a \
         wildcard or a public address"
            .to_string()
    };
    // The certificate route carries a condition on any address whose names the
    // daemon has to check: it declines to advertise a name it can see points
    // somewhere it does not listen, so a certificate obtained for a name that
    // resolves elsewhere is one it will never serve. Said as its own sentence
    // rather than folded into the instruction, so the two ways through stay
    // parallel. A wildcard bind needs none of it — the listener answers on
    // every interface, so every name of this Mac reaches it.
    // Which certificate to obtain depends on the bind, and naming the wrong
    // source is an instruction that fails when followed. A wildcard listener
    // answers on every interface, so this node's MagicDNS name reaches it and
    // `tailscale cert` is exactly the tool. Any other address needs a name that
    // resolves to *it*, which `tailscale cert` will not issue — so that route
    // is a certificate the operator already holds, dropped where `tls::ensure`
    // looks before it asks Tailscale for anything.
    let certificate = if ip.is_unspecified() {
        "Obtain a certificate (`tailscale cert`, which needs HTTPS Certificates enabled for this \
         tailnet)"
            .to_string()
    } else {
        format!(
            "Obtain a certificate for a name that resolves to {ip}, set \"tls_hostname\" to that \
             name and put its certificate and key in {tls} as <name>.crt and <name>.key \
             (`tailscale cert` issues only for this node's MagicDNS name, which resolves to the \
             tailnet interface rather than to {ip}, and this daemon will not advertise a name it \
             can see resolves somewhere other than {ip})",
            tls = protocol::tls_dir().display()
        )
    };
    Some(format!(
        "ws {bind} will REFUSE EVERY CONNECTION: plaintext is not private on this address and \
         no certificate is available, so no phone can reach this daemon. {certificate}, \
         {way_through}."
    ))
}

/// How long the tailnet watcher waits before looking again, and the ceiling it
/// backs off to. These are the idle gaps: each attempt also costs up to
/// [`TAILSCALE_PROBE_TIMEOUT`] inside `tailnet_addresses` itself.
const TAILNET_WATCH_FIRST_DELAY: Duration = Duration::from_secs(5);
const TAILNET_WATCH_MAX_DELAY: Duration = Duration::from_secs(60);

/// The gap before attempt `n`, counted from zero: doubling from
/// [`TAILNET_WATCH_FIRST_DELAY`] up to a [`TAILNET_WATCH_MAX_DELAY`] ceiling.
///
/// Fast at first because the case it exists for resolves in seconds, and capped
/// because a machine with no Tailscale must not be probed on a schedule that
/// keeps shortening the odds of catching one.
fn tailnet_watch_delay(attempt: u32) -> Duration {
    let secs = TAILNET_WATCH_FIRST_DELAY
        .as_secs()
        .saturating_mul(1u64 << attempt.min(16));
    Duration::from_secs(secs.min(TAILNET_WATCH_MAX_DELAY.as_secs()))
}

/// Recover from a tailnet that was not up yet when this daemon started.
///
/// `tailnet_addresses` gives `tailscale ip` three seconds and no more, because
/// a wedged tailscale must never stop the daemon binding at all — and at login,
/// which is exactly when launchd starts ccd, three seconds is a real race.
/// Losing it binds loopback, and `resolve_bind` runs once, so loopback is what
/// the daemon stays on for the whole life of the process: unreachable by any
/// phone until something starts it again.
///
/// The way back is a restart, not a second socket. `daemon.endpoint` is set
/// once at construction and is what the QR advertises, so merely opening
/// another listener on the tailnet address would leave `codeconnect pair` still
/// refusing to print a code — and there would still be no certificate. Only a
/// fresh process re-runs the whole startup path, which is the only way the
/// bind, the certificate and the advertised endpoint are got right together.
///
/// [`TailnetWatch::Restart`] asks for that process by returning: every task in
/// the `select!` is meant to outlive the daemon, so one returning exits
/// non-zero, and launchd's `KeepAlive{SuccessfulExit: false}` starts ccd again.
/// In the only state this fires in, the daemon is serving no remote client, so
/// it interrupts nothing that was working.
///
/// **What returning buys is the attempt, not the address.** The fresh process
/// runs `resolve_bind` from the top and gets [`TAILSCALE_PROBE_TIMEOUT`] and no
/// more, exactly as this one did — so an address that has gone again, or a
/// `tailscale ip` that loses the same race a second time, binds loopback again
/// and arrives back at this watch, which handles it the same way. That is a
/// retry rather than a spin: each pass costs a full backoff before the probe
/// that ends it, and launchd's own `ThrottleInterval` bounds the restarts
/// themselves. The daemon keeps asking until a start finds the address in
/// time, and that — not the landing — is the whole of what it can promise
/// about a process it does not supervise.
///
/// [`TailnetWatch::Report`] never returns. There is no CodeConnect launchd job
/// behind that daemon, so returning would end it and leave nothing to take its
/// place — strictly worse than the loopback bind it set out to fix. It says
/// instead what a restart would achieve, and keeps watching so that what it
/// said stays true.
///
/// Neither path stops looking. A Tailscale that arrives an hour after login, or
/// a day after it, leaves the daemon in exactly the state a Tailscale that
/// arrives a second late does, and it is recoverable in exactly the same way.
/// The backoff settles at one probe a minute, which is what makes watching for
/// the life of the process cost little enough not to need bounding.
///
/// **What no test pins, and what a wrong one would cost.** Every watcher test
/// hands [`watch_tailnet_with`] its own probe, delay and report, so nothing
/// asserts which three *this* function names. Reaching them from a test means
/// waiting out the whole five-second [`TAILNET_WATCH_FIRST_DELAY`] and then
/// letting [`tailnet_addresses`] shell out to `tailscale ip` — an answer that
/// differs per machine and is absent on most — so what stands behind these
/// three arguments is review, not a test.
/// `the_production_watcher_never_returns_on_the_startup_path` runs this
/// function for the part of that which is cheap: that none of the three states
/// returns at startup. It cannot tell one probe from another.
///
/// What each would cost is why that review has to be careful. A probe that
/// answered nothing leaves [`TailnetWatch::Restart`] watching for ever and the
/// daemon unreachable for the life of the process — the state this function
/// exists to end, failing silently. A probe that answered an address
/// [`plan_bind`] would not bind is worse: the exit buys a start that falls back
/// to loopback and asks to exit again, for ever. A zero delay puts a `tailscale
/// ip` on the startup path the [`TAILSCALE_PROBE_TIMEOUT`] budget exists to
/// keep off it, and another on every pass after. A report that said nothing
/// would leave [`TailnetWatch::Report`] — which never returns — watching with
/// no output at all, when the output is the whole of what that path does.
async fn watch_for_tailnet(watch: TailnetWatch) {
    watch_tailnet_with(
        watch,
        tailnet_addresses,
        tailnet_watch_delay,
        report_tailnet_change,
    )
    .await;
}

/// How many probes an unreachable daemon goes without repeating itself.
///
/// A report naming an address is an instruction — restart, because a start that
/// finds that address binds it and this one never will — and it stays worth
/// acting on for as long as the daemon stays on loopback. What the restart
/// settles is which of the two probes wins, not the landing: a fresh start
/// spends its own [`TAILSCALE_PROBE_TIMEOUT`] and one that loses the race again
/// is back on loopback and back in this watch. Said once and never again, the
/// instruction can be rotated out of a log it shares with everything else while
/// the state it describes is still current.
/// At the backoff's ceiling that is an hour of idle gaps, and a little over an
/// hour once each probe's own [`TAILSCALE_PROBE_TIMEOUT`] is counted: often
/// enough for an operator who was away to find it, rare enough to drown
/// nothing.
const TAILNET_REPORT_REPEAT_PROBES: u32 = 60;

/// Say what a restart would achieve, when the answer to that has changed.
///
/// The daemon this runs in has no CodeConnect launchd job behind it, so it can
/// act on none of this: the whole of what it can do is leave the state where
/// somebody who *can* act will find it. Said on every change — the first
/// sighting, an address that replaces it, and its disappearance — because
/// saying it only once would leave a stale instruction standing after the
/// address it named had gone.
///
/// What that bounds is repetition of a *standing* answer, which is the common
/// case: an address that turns up and stays is named once and then at most
/// every [`TAILNET_REPORT_REPEAT_PROBES`] probes. It does not bound an answer
/// that keeps changing — a tailnet flapping on every probe is reported on every
/// probe — and that is the honest outcome rather than a bounded one, because a
/// tailnet this daemon cannot see for two consecutive probes is a different
/// fault from the one this watch exists for, and hiding it behind a floor would
/// leave the operator reading a report that was already out of date.
fn report_tailnet_change(found: Option<IpAddr>) {
    match found {
        Some(ip) => crate::log_warn!(
            "tailnet address {ip} is up now, and this daemon bound loopback at startup because \
             `tailscale ip` did not answer in time, so no phone can reach it. Restarting ccd \
             probes again, and a start that finds {ip} binds it and advertises it — with a \
             certificate if one can be had, and plaintext over WireGuard if not. A start that \
             does not find it in time is on loopback and watching again. Nothing here will do \
             any of that: this process is not run by launchd's {} job, so the restart is yours \
             to run.",
            protocol::LAUNCHD_LABEL
        ),
        None => crate::log_warn!(
            "the tailnet address reported earlier is gone again; this daemon is still on \
             loopback, still unreachable by any phone, and still watching for one to appear."
        ),
    }
}

/// The watcher itself, with everything slow or noisy about it handed in: what
/// it asks, how long it waits between asking, and where it says what it found.
///
/// The seam exists because the behaviour that matters most here is a *negative*
/// one — that two of the three paths through this never complete — and a test
/// cannot wait out a daemon's lifetime to watch nothing happen. With a canned
/// probe and no gaps, a whole lifetime runs in microseconds and the promise is
/// provable. `tokio::time::pause` would be the other way, and this workspace
/// deliberately does not enable the tokio feature it needs.
async fn watch_tailnet_with<Probe, Fut, Report>(
    watch: TailnetWatch,
    probe: Probe,
    delay: fn(u32) -> Duration,
    report: Report,
) where
    Probe: Fn() -> Fut,
    Fut: std::future::Future<Output = Vec<IpAddr>>,
    Report: Fn(Option<IpAddr>),
{
    if watch == TailnetWatch::Idle {
        std::future::pending::<()>().await;
        return;
    }
    // What the last report named, and how many probes ago it was said.
    let mut reported: Option<IpAddr> = None;
    let mut since_report: u32 = 0;
    let mut attempt: u32 = 0;
    loop {
        tokio::time::sleep(delay(attempt)).await;
        // Saturating rather than wrapping: the gap reaches its ceiling within
        // the first handful of attempts, so the count past that decides
        // nothing — but wrapping it would silently restart the backoff at five
        // seconds after four billion probes.
        attempt = attempt.saturating_add(1);
        let found = preferred_tailnet_ip(&probe().await);
        match watch {
            TailnetWatch::Restart => {
                let Some(ip) = found else { continue };
                // Said before returning, so the exit reads in the log as the
                // bid for recovery it is rather than as the daemon falling
                // over. What it must not read as is an arrival: this process
                // ends here, and the address is the next one's to win.
                crate::log_warn!(
                    "tailnet address {ip} is up now, and this daemon bound loopback at startup \
                     because `tailscale ip` did not answer in time. Exiting so launchd starts a \
                     fresh ccd, which probes for itself: a start that finds {ip} binds it and \
                     advertises it — with a certificate if one can be had, and plaintext over \
                     WireGuard if not. A start that does not is on loopback and watching again."
                );
                return;
            }
            TailnetWatch::Report => {
                // Whenever the answer changes, and otherwise only once the
                // standing instruction has gone long enough unrepeated to be
                // worth repeating. A withdrawal is not repeated: there is no
                // address left to name, so there is nothing to act on and
                // saying so hourly would be noise about nothing.
                // Counted before it is read, so the gap between two repeats is
                // exactly [`TAILNET_REPORT_REPEAT_PROBES`] probes: reading it
                // first would let the probe that resets the count go unnumbered
                // and stretch every interval by one.
                since_report = since_report.saturating_add(1);
                let overdue = found.is_some() && since_report >= TAILNET_REPORT_REPEAT_PROBES;
                if found != reported || overdue {
                    report(found);
                    reported = found;
                    since_report = 0;
                }
            }
            // Answered before the loop, where it waits for ever. Written out
            // rather than folded into a wildcard so that a state added later
            // has to be decided here too — and waiting is what it does
            // wherever it is met, because returning ends the daemon.
            TailnetWatch::Idle => std::future::pending::<()>().await,
        }
    }
}

/// How long the whole `tailscale ip` probe gets — across every candidate binary
/// — before the daemon gives up on it. One total budget, not per candidate, so
/// several hung candidates cannot add up. A wedged tailscale service must not
/// keep `ccd` from ever binding its sockets.
const TAILSCALE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Every address this node holds on the tailnet (IPv4 and IPv6), or empty when
/// tailscale is not installed, not up, or does not answer in time. Bounded by
/// one absolute deadline and `kill_on_drop`, so a hung `tailscale` process is
/// killed rather than awaited forever.
async fn tailnet_addresses() -> Vec<IpAddr> {
    let deadline = tokio::time::Instant::now() + TAILSCALE_PROBE_TIMEOUT;
    for candidate in protocol::pairing::TAILSCALE_CANDIDATES {
        if !Path::new(candidate).exists() {
            continue;
        }
        let run = tokio::process::Command::new(candidate)
            .arg("ip")
            .kill_on_drop(true)
            .output();
        let output = match tokio::time::timeout_at(deadline, run).await {
            Ok(Ok(output)) => output,
            Ok(Err(_)) => continue,
            Err(_) => {
                crate::log_warn!(
                    "`{candidate} ip` did not answer within the {}s tailnet probe budget; \
                     treating the tailnet as unavailable",
                    TAILSCALE_PROBE_TIMEOUT.as_secs()
                );
                break;
            }
        };
        let addresses: Vec<IpAddr> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect();
        if !addresses.is_empty() {
            return addresses;
        }
    }
    Vec::new()
}

/// How long the name the QR would carry gets to resolve before the daemon stops
/// waiting on it.
///
/// Bounded for the reason [`TAILSCALE_PROBE_TIMEOUT`] is: this runs before a
/// single socket is opened, and a resolver that never answers must not be able
/// to keep the daemon from starting. A lookup that does not finish inside it
/// leaves the name unverified rather than refuted — [`AdvertisedName`] keeps
/// those two apart.
const ADVERTISED_NAME_TIMEOUT: Duration = Duration::from_secs(3);

/// What is known about whether a phone dialling `hostname` arrives at this
/// listener.
///
/// Three answers, not two, because "known to point elsewhere" and "could not be
/// checked" call for different things and conflating them is what made a daemon
/// that was serving `wss://` refuse every connection: a resolver that cannot
/// answer for a name is not evidence against it.
enum AdvertisedName {
    /// The bind settles it, or the resolver put this listener's address behind
    /// the name.
    Reaches,
    /// Nothing decided it either way. Not grounds to refuse the name, and not
    /// grounds to prefer it over an address already known to be right.
    Unverified(String),
    /// The name is known to name somewhere this daemon is not listening.
    Misses(String),
}

/// Whether `hostname` reaches this listener, as far as this Mac can tell.
///
/// This answers the question and decides nothing; [`resolve_transport_with`]
/// acts on the answer, and what it does with each of the three is the rule this
/// function exists to serve:
///
/// * [`AdvertisedName::Misses`] is never advertised. The QR carries the bind's
///   own address and no certificate is obtained at all — the `Misses` branch
///   returns before the certifier is called — because a certificate for a name
///   no phone reaches this daemon at protects a connection nobody opens.
/// * [`AdvertisedName::Reaches`] is advertised either way. With a certificate
///   the QR says `wss://` and the name; with none it still says the name,
///   plainly, because the name survives a Tailscale address change that an IP
///   literal does not and is what the phone will need once one is obtained.
/// * [`AdvertisedName::Unverified`] is advertised **only** when a certificate
///   was obtained for it. Validating one is the whole reason a phone dials a
///   name rather than an address, so the certificate is what buys an
///   unconfirmed name its place; with none, the address this daemon knows it
///   listens on is the better of the two and the QR carries that instead.
///
/// So the guarantee is not that everything advertised was checked — the third
/// case above deliberately carries a name nothing confirmed, and says so in the
/// log. It is that nothing *known* to point elsewhere is ever advertised, and
/// that an unchecked name is carried only where a certificate makes the name
/// the thing the phone has to dial. A name that resolves somewhere else is
/// worse than an IP literal, because the literal is visibly wrong and the name
/// is not — the phone dials it, the DNS answer is perfect, and nothing is
/// listening.
///
/// The shape of the bind is asked first, because it settles two cases outright:
///
/// * The unspecified address puts the listener on every interface this Mac has,
///   so any name that arrives at this Mac arrives at it.
/// * This node's own tailnet address is what this node's MagicDNS name maps to
///   — the same node `tailscale ip` was asked for. It is also the one path
///   where a lookup would be actively wrong: the phone resolves that name over
///   the tailnet, and a Mac running `--accept-dns=false` cannot resolve it here
///   at all, so checking would refuse names that work.
///
/// Anywhere else the resolver decides it, and only a positive answer decides it
/// against the name. An answer that puts some other address behind the name is
/// a refutation; silence is not. The one place silence still refutes is a
/// MagicDNS name — which names this node's tailnet address and nothing else —
/// beside a bind [`never_a_tailnet_address`] calls certain, where no lookup is
/// needed to know the two disagree. `from_config` is what tells the two name
/// sources apart: a name the operator set is theirs to vouch for, and this
/// daemon does not overrule it on a resolver that stayed silent.
async fn advertised_name_reach<Resolve, Fut>(
    hostname: &str,
    plan: &BindPlan,
    from_config: bool,
    resolve: Resolve,
) -> AdvertisedName
where
    Resolve: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<Vec<IpAddr>>>,
{
    let ip = plan.bind.ip();
    if ip.is_unspecified() || plan.bind_is_tailnet {
        return AdvertisedName::Reaches;
    }
    let silence =
        match tokio::time::timeout(ADVERTISED_NAME_TIMEOUT, resolve(hostname.to_string())).await {
            Ok(Ok(addresses)) if addresses.contains(&ip) => return AdvertisedName::Reaches,
            Ok(Ok(addresses)) if !addresses.is_empty() => {
                return AdvertisedName::Misses(format!(
                    "it resolves to {}, and this daemon listens on {ip}",
                    addresses
                        .iter()
                        .map(|address| address.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
            Ok(Ok(_)) => "it resolves to no address here".to_string(),
            Ok(Err(err)) => format!("this Mac cannot resolve it ({err})"),
            Err(_) => format!(
                "it did not resolve inside {}s",
                ADVERTISED_NAME_TIMEOUT.as_secs()
            ),
        };
    if !from_config && never_a_tailnet_address(ip) {
        return AdvertisedName::Misses(format!(
            "{silence}, and a MagicDNS name names this node's tailnet address, never {ip}"
        ));
    }
    AdvertisedName::Unverified(silence)
}

/// Every address `hostname` resolves to, as this Mac's own resolver answers it.
///
/// The port is part of the resolver's input shape and no part of the question;
/// only the addresses are read.
async fn resolve_name(hostname: String) -> std::io::Result<Vec<IpAddr>> {
    Ok(tokio::net::lookup_host((hostname, 0u16))
        .await?
        .map(|address| address.ip())
        .collect())
}

/// Certificate material for `hostname`, or `None` when there is none to be had.
///
/// Every failure here is a degradation to plaintext on the same host, never a
/// daemon that refuses to start, and each one is reported in the operator's
/// terms at the point the reason is known.
async fn certificate_for(hostname: String, refresh_days: u64) -> Option<tokio_rustls::TlsAcceptor> {
    match tls::ensure(&hostname, refresh_days).await {
        Ok(material) => match tls::acceptor(&material) {
            Ok(acceptor) => {
                crate::log_info!(
                    "tls: serving wss:// as {} ({} days of validity left)",
                    material.hostname,
                    material.days_remaining(protocol::time::now_unix_ms()),
                );
                Some(acceptor)
            }
            Err(err) => {
                crate::log_error!("tls: certificate unusable ({err:#}); serving ws://");
                None
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
            None
        }
    }
}

/// Decide how the phone reaches us, and get a certificate if we can.
///
/// The QR host and the listener's certificate are decided together and once,
/// because they are two halves of one claim: a certificate is issued for a DNS
/// name, and a QR that sends the phone to an IP literal can never validate
/// against it. Which of the two the QR carries is [`advertised_name_reach`]'s
/// three-valued answer, applied as its documentation sets out: a name
/// [`AdvertisedName::Misses`] is never advertised and takes no certificate with
/// it; a name it [`AdvertisedName::Reaches`] is advertised with or without one;
/// a name left [`AdvertisedName::Unverified`] is advertised only when a
/// certificate was obtained for it, and otherwise gives way to the bind's own
/// address. The one thing none of the three does is advertise a host this
/// daemon knows a phone would not arrive at.
///
/// Any failure below degrades to plain `ws://` on the bind address — never to a
/// daemon that will not start. On a loopback or tailnet bind that is safe: TLS
/// there is an upgrade to a link Tailscale already encrypts. On an explicit
/// non-tailnet `ws_bind` (a LAN address, say) plaintext would cross a network in
/// the clear, so the admission gate refuses it — the process still starts, but
/// that listener serves nobody until a certificate for a name that resolves to
/// the bind is provided. That fail-closed case is stated at the bind, not here.
async fn resolve_transport(
    config: &Config,
    plan: &BindPlan,
) -> (Option<tokio_rustls::TlsAcceptor>, Endpoint) {
    resolve_transport_with(
        config,
        plan,
        tls::magic_dns_name,
        resolve_name,
        |hostname| certificate_for(hostname, config.cert_refresh_days),
    )
    .await
}

/// The transport decision, with all three things that leave this Mac handed in:
/// the MagicDNS name, the resolver that says where a name points, and the
/// certificate.
///
/// The seam exists because the decision is worth asserting on every shape of
/// bind, and each of those three shells out to `tailscale` or goes to the
/// network — so without it there is no test of this function at all.
async fn resolve_transport_with<Name, NameFut, Resolve, ResolveFut, Certify, CertifyFut>(
    config: &Config,
    plan: &BindPlan,
    magic_dns: Name,
    resolve: Resolve,
    certify: Certify,
) -> (Option<tokio_rustls::TlsAcceptor>, Endpoint)
where
    Name: FnOnce() -> NameFut,
    NameFut: std::future::Future<Output = Option<String>>,
    Resolve: FnOnce(String) -> ResolveFut,
    ResolveFut: std::future::Future<Output = std::io::Result<Vec<IpAddr>>>,
    Certify: FnOnce(String) -> CertifyFut,
    CertifyFut: std::future::Future<Output = Option<tokio_rustls::TlsAcceptor>>,
{
    let bind = plan.bind;
    let fallback = Endpoint {
        host: bind.ip().to_string(),
        port: bind.port(),
        tls: false,
    };

    // Settled by the bind alone, and settled before anything is looked up: no
    // device but this Mac can open a connection to a loopback listener, so
    // there is no name that would be honest here and no certificate worth
    // obtaining, since nothing off this machine can open the connection it
    // would protect.
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

    let from_config = config.tls_hostname.is_some();
    let hostname = match &config.tls_hostname {
        Some(explicit) => Some(explicit.clone()),
        None => magic_dns().await,
    };
    let Some(hostname) = hostname else {
        crate::log_warn!("no MagicDNS name available; serving ws:// on {bind}");
        return (None, fallback);
    };

    let reach = advertised_name_reach(&hostname, plan, from_config, resolve).await;
    if let AdvertisedName::Misses(reason) = &reach {
        // The certificate route is named with the tool that can actually
        // deliver it. `tailscale cert` issues for this node's MagicDNS name and
        // nothing else, which is the very name being refused here — sending an
        // operator to it would be an instruction that fails when followed. A
        // certificate they hold already goes in the directory `tls::ensure`
        // reads before it asks Tailscale for anything.
        crate::log_warn!(
            "not advertising {hostname}: {reason}. The QR carries {ip} instead, which is where \
             this daemon listens, and no certificate is obtained for a name that would send a \
             phone elsewhere. For wss:// on this bind, set \"tls_hostname\" to a name that \
             resolves to {ip} and put that name's certificate and key in {tls} as <name>.crt \
             and <name>.key — `tailscale cert` issues only for this node's MagicDNS name, which \
             is not a name that reaches {ip}.",
            ip = bind.ip(),
            tls = protocol::tls_dir().display()
        );
        return (None, fallback);
    }

    let named = Endpoint {
        host: hostname.clone(),
        port: bind.port(),
        tls: true,
    };
    match (certify(hostname.clone()).await, reach) {
        // Carried unverified only because a certificate is behind it: validating
        // one is the whole reason a phone must dial a name rather than an
        // address, so here the name earns its place without having been
        // confirmed.
        (Some(acceptor), AdvertisedName::Unverified(reason)) => {
            crate::log_warn!(
                "advertising {hostname} without having confirmed it: {reason}. Nothing found \
                 says it points anywhere other than {ip}, and it carries a certificate, which \
                 is what a phone has to dial a name to validate.",
                ip = bind.ip()
            );
            (Some(acceptor), named)
        }
        (Some(acceptor), _) => (Some(acceptor), named),
        // Nothing confirmed the name and no certificate needs it, so the address
        // this daemon knows it listens on is the better of the two.
        (None, AdvertisedName::Unverified(reason)) => {
            crate::log_warn!(
                "not advertising {hostname}: {reason}, and no certificate was obtained that a \
                 phone would need the name to validate. The QR carries {ip}, which is where \
                 this daemon listens.",
                ip = bind.ip()
            );
            (None, fallback)
        }
        // The name reaches this listener, so it stays the QR host even with no
        // certificate behind it: it survives a Tailscale IP change, which an IP
        // literal does not, and it is what the phone will need once one is
        // obtained.
        (None, _) => (None, plain_named(hostname, bind)),
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
                agent: protocol::agent::AgentKind::Claude,
                codex_thread_id: None,
                codex_socket: None,
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

    use crate::ws_server::PlaintextTrust;

    /// The port is pinned rather than defaulted so the assertions below are
    /// about the address that was chosen, not about a constant elsewhere.
    fn config_with(ws_bind: Option<&str>) -> Config {
        Config {
            ws_bind: ws_bind.map(str::to_string),
            ws_port: 8787,
            ..Config::default()
        }
    }

    /// This node's own addresses, as `tailscale ip` prints them.
    fn tailnet() -> Vec<IpAddr> {
        vec![
            "100.64.12.34".parse().unwrap(),
            "fd7a:115c:a1e0::1".parse().unwrap(),
        ]
    }

    #[test]
    fn an_explicit_lan_bind_without_a_certificate_refuses_plaintext() {
        // The policy, which every convenience built around it has to leave
        // standing: a LAN address is not loopback and not this node's tailnet
        // address, so nothing encrypts the path the bearer token takes across
        // it. Refusing the connection is the correct behaviour.
        let config = config_with(Some("192.168.1.20"));
        let plan = plan_bind(&config, &[], true);
        assert_eq!(plan.bind, "192.168.1.20:8787".parse().unwrap());
        assert_eq!(plan.trust, PlaintextTrust::RequireTls);
        assert_eq!(plan.opt_in, PlaintextOptIn::Inert);

        // What was missing is the way through. One line, the address, and both
        // remedies — an operator upgrading into this refusal has phones that
        // worked yesterday and nothing in the log that names a fix.
        let advice = plaintext_refusal_advice(&config, &plan, false).expect("must be advised");
        assert!(advice.contains("192.168.1.20:8787"), "{advice}");
        assert!(advice.contains("REFUSE EVERY CONNECTION"), "{advice}");
        assert!(advice.contains("tailscale cert"), "{advice}");
        assert!(advice.contains("ws_allow_plaintext"), "{advice}");

        // With a certificate the listener serves, and there is nothing to say.
        assert_eq!(plaintext_refusal_advice(&config, &plan, true), None);
    }

    #[test]
    fn the_plaintext_opt_in_restores_a_private_lan_bind() {
        // The migration path: an operator with paired phones, a LAN bind and no
        // certificate says once that they accept their own network, and the
        // listener serves again.
        let mut config = config_with(Some("192.168.1.20"));
        config.ws_allow_plaintext = true;
        let plan = plan_bind(&config, &[], false);
        // `OperatorAllowed`, never `TrustedPath`: the listener serves, and the
        // LAN it serves on is not thereby private. The terminal is what rests
        // on the difference — see `PlaintextTrust`.
        assert_eq!(plan.trust, PlaintextTrust::OperatorAllowed);
        assert!(!plan.trust.is_private());
        assert_eq!(plan.opt_in, PlaintextOptIn::Honoured);
        assert_eq!(plaintext_refusal_advice(&config, &plan, false), None);

        // Every local range the key covers, including the two IPv6 ones that
        // have to be recognised by hand.
        for address in ["10.1.2.3", "172.16.0.9", "169.254.3.4", "fd7a:115c:a1e0::9"] {
            let mut config = config_with(Some(address));
            config.ws_allow_plaintext = true;
            let plan = plan_bind(&config, &[], false);
            assert_eq!(plan.trust, PlaintextTrust::OperatorAllowed, "{address}");
            assert_eq!(plan.opt_in, PlaintextOptIn::Honoured, "{address}");
        }
    }

    #[test]
    fn the_plaintext_opt_in_is_refused_on_the_wildcard_address() {
        // The case the key must not cover: on `0.0.0.0` the operator cannot
        // know which interfaces they have just put the bearer token on, so
        // there is no network for them to be vouching for.
        for wildcard in ["0.0.0.0", "::"] {
            let mut config = config_with(Some(wildcard));
            config.ws_allow_plaintext = true;
            let plan = plan_bind(&config, &tailnet(), false);
            assert_eq!(plan.trust, PlaintextTrust::RequireTls, "{wildcard}");
            assert_eq!(plan.opt_in, PlaintextOptIn::Refused, "{wildcard}");

            // And the advice redirects rather than repeating a key that is
            // already set and already refused here.
            let advice = plaintext_refusal_advice(&config, &plan, false).expect("must be advised");
            assert!(advice.contains("LAN address"), "{advice}");
            assert!(
                !advice.contains("ws_allow_plaintext"),
                "advising a key that does nothing here: {advice}"
            );
        }
    }

    #[test]
    fn the_plaintext_opt_in_is_refused_on_a_public_address() {
        // Plaintext on a public address is private on no hop, whatever anybody
        // declares about it.
        let mut config = config_with(Some("203.0.113.7"));
        config.ws_allow_plaintext = true;
        let plan = plan_bind(&config, &tailnet(), false);
        assert_eq!(plan.trust, PlaintextTrust::RequireTls);
        assert_eq!(plan.opt_in, PlaintextOptIn::Refused);
        let advice = plaintext_refusal_advice(&config, &plan, false).expect("must be advised");
        assert!(advice.contains("LAN address"), "{advice}");

        // The trap: an address in 100.64/10 looks like a tailnet address and
        // proves nothing about who carries the packets. It is not this node's,
        // so it is as public as any other.
        let mut config = config_with(Some("100.64.99.99"));
        config.ws_allow_plaintext = true;
        let plan = plan_bind(&config, &tailnet(), false);
        assert_eq!(plan.trust, PlaintextTrust::RequireTls);
        assert_eq!(plan.opt_in, PlaintextOptIn::Refused);
    }

    #[test]
    fn a_bind_the_tailnet_probe_could_not_check_is_told_to_restart_rather_than_to_move() {
        // `ws_bind` set to this node's own tailnet address is an ordinary thing
        // to do — the README documents that address as the default — and at
        // login `tailscale ip` can lose the three-second race, which leaves the
        // probe empty and this genuinely private address classified as one the
        // daemon cannot vouch for. Every clause of the ordinary refusal is
        // wrong here: the address is not public, plaintext on it is private,
        // and binding a LAN address instead would fix nothing.
        let config = config_with(Some("100.64.12.34"));
        let plan = plan_bind(&config, &[], false);
        assert_eq!(
            plan.trust,
            PlaintextTrust::RequireTls,
            "the policy is intact"
        );
        assert!(!plan.tailnet_confirmed);

        let advice = plaintext_refusal_advice(&config, &plan, false).expect("must be advised");
        assert!(advice.contains("could not confirm"), "{advice}");
        assert!(advice.contains("codeconnect daemon restart"), "{advice}");
        assert!(
            !advice.contains("LAN address"),
            "an address the daemon never checked must not be called public: {advice}"
        );
        assert!(
            !advice.contains("plaintext is not private on this address"),
            "the daemon does not know that, and here it is probably false: {advice}"
        );
    }

    #[test]
    fn a_refused_opt_in_never_calls_an_unconfirmed_address_public_either() {
        // These two lines are logged within moments of each other about the
        // same address, so they must make the same distinction. The advice goes
        // out of its way not to call an address it could not check public; this
        // one asserted it outright, which left an operator whose `ws_bind` is
        // their own tailnet address being told it was public by one line and
        // unverifiable by the next.
        let mut config = config_with(Some("100.64.12.34"));
        config.ws_allow_plaintext = true;
        let plan = plan_bind(&config, &[], false);
        assert_eq!(plan.opt_in, PlaintextOptIn::Refused);
        let refusal = refused_opt_in_message(&plan);
        assert!(refusal.contains("could not confirm"), "{refusal}");
        assert!(
            !refusal.contains("public"),
            "an address the daemon never checked is not one it may call public: {refusal}"
        );
        // The key's fate is still stated plainly — the operator set something
        // and it did not take effect, which is the whole point of the line.
        assert!(refusal.contains("not honoured"), "{refusal}");
        // And it agrees with the line that follows it.
        let advice = plaintext_refusal_advice(&config, &plan, false).expect("must be advised");
        assert!(!advice.contains("public"), "{advice}");

        // Confirmed, and genuinely absent from it: an established fact, and the
        // plain wording stays.
        let mut config = config_with(Some("203.0.113.7"));
        config.ws_allow_plaintext = true;
        let plan = plan_bind(&config, &tailnet(), false);
        assert_eq!(plan.opt_in, PlaintextOptIn::Refused);
        let refusal = refused_opt_in_message(&plan);
        assert!(refusal.contains("public address"), "{refusal}");
        assert!(!refusal.contains("could not confirm"), "{refusal}");

        // The wildcard keeps its own reason whatever the probe did: it is
        // nobody's tailnet address, so "could not check it" would be wrong.
        let mut config = config_with(Some("0.0.0.0"));
        config.ws_allow_plaintext = true;
        let plan = plan_bind(&config, &[], false);
        let refusal = refused_opt_in_message(&plan);
        assert!(refusal.contains("every interface"), "{refusal}");
        assert!(!refusal.contains("could not confirm"), "{refusal}");
    }

    #[test]
    fn an_address_that_is_genuinely_not_on_the_tailnet_is_still_told_to_move() {
        // The other half of the distinction: the probe answered, and this
        // address is not among what it said. That is a fact, not an uncertainty,
        // and a restart would change nothing about it.
        let config = config_with(Some("203.0.113.7"));
        let plan = plan_bind(&config, &tailnet(), false);
        assert!(plan.tailnet_confirmed);
        let advice = plaintext_refusal_advice(&config, &plan, false).expect("must be advised");
        assert!(advice.contains("LAN address"), "{advice}");
        assert!(!advice.contains("daemon restart"), "{advice}");

        // The trap in the distinction: the wildcard is nobody's tailnet address
        // whatever the probe did or did not say, so an empty tailnet must not
        // turn `0.0.0.0` into "we could not check this one".
        let config = config_with(Some("0.0.0.0"));
        let plan = plan_bind(&config, &[], false);
        assert!(!plan.tailnet_confirmed);
        let advice = plaintext_refusal_advice(&config, &plan, false).expect("must be advised");
        assert!(advice.contains("LAN address"), "{advice}");
        assert!(!advice.contains("daemon restart"), "{advice}");

        // And the same for the LAN bind F2 exists for: Tailscale hands out no
        // RFC1918 address, so an empty probe leaves that classification certain
        // and the opt-in is still the way through.
        let config = config_with(Some("192.168.1.20"));
        let plan = plan_bind(&config, &[], false);
        let advice = plaintext_refusal_advice(&config, &plan, false).expect("must be advised");
        assert!(advice.contains("ws_allow_plaintext"), "{advice}");
        assert!(!advice.contains("daemon restart"), "{advice}");
    }

    #[test]
    fn the_plaintext_opt_in_changes_nothing_where_plaintext_is_already_private() {
        // Loopback never leaves the machine and this node's own tailnet
        // addresses are carried by WireGuard. The key has nothing to soften, so
        // it must not produce a warning about a network being crossed in the
        // clear when none is.
        for address in ["127.0.0.1", "::1", "100.64.12.34", "fd7a:115c:a1e0::1"] {
            let mut config = config_with(Some(address));
            config.ws_allow_plaintext = true;
            let plan = plan_bind(&config, &tailnet(), false);
            assert_eq!(plan.trust, PlaintextTrust::TrustedPath, "{address}");
            assert_eq!(plan.opt_in, PlaintextOptIn::Inert, "{address}");
            assert_eq!(
                plaintext_refusal_advice(&config, &plan, false),
                None,
                "{address}"
            );
        }
    }

    #[test]
    fn tls_required_is_not_quietly_undone_by_the_plaintext_opt_in() {
        // Two keys that mean opposite things, and the stricter one wins. The
        // admission gate refuses plaintext whenever `tls_required` is set, so
        // honouring an exception here would announce a listener serving
        // plaintext that then refuses every plaintext connection.
        let mut config = config_with(Some("192.168.1.20"));
        config.ws_allow_plaintext = true;
        config.tls_required = true;
        let plan = plan_bind(&config, &[], false);
        assert_eq!(plan.trust, PlaintextTrust::RequireTls);
        assert_eq!(plan.opt_in, PlaintextOptIn::Inert);
        // Nothing to advise either: the refusal is what was asked for, and a
        // certificate is the only thing missing — which `ws_server` says.
        assert_eq!(plaintext_refusal_advice(&config, &plan, false), None);
    }

    #[test]
    fn the_tailnet_watcher_watches_only_for_the_loopback_fallback() {
        // Watched: nothing was configured, no tailnet answered, and CodeConnect's
        // launchd job is there to restart the process. This is the one state the
        // daemon cannot get out of on its own — `resolve_bind` runs exactly once.
        let plan = plan_bind(&config_with(None), &[], true);
        assert_eq!(plan.bind, "127.0.0.1:8787".parse().unwrap());
        assert_eq!(plan.origin, BindOrigin::LoopbackFallback);
        assert_eq!(plan.watch_tailnet, TailnetWatch::Restart);

        // An operator who asked for loopback got what they asked for. Exiting
        // on them would be a daemon that refuses to stay running.
        let plan = plan_bind(&config_with(Some("127.0.0.1")), &[], true);
        assert_eq!(plan.origin, BindOrigin::Explicit);
        assert_eq!(plan.watch_tailnet, TailnetWatch::Idle);

        // An explicit LAN bind is not waiting for a tailnet address either.
        let plan = plan_bind(&config_with(Some("192.168.1.20")), &[], true);
        assert_eq!(plan.origin, BindOrigin::Explicit);
        assert_eq!(plan.watch_tailnet, TailnetWatch::Idle);

        // A tailnet address was found: there is nothing left to wait for, and
        // the IPv4 one is chosen because that is what the QR advertises.
        let plan = plan_bind(&config_with(None), &tailnet(), true);
        assert_eq!(plan.bind, "100.64.12.34:8787".parse().unwrap());
        assert_eq!(plan.origin, BindOrigin::Tailnet);
        assert_eq!(plan.trust, PlaintextTrust::TrustedPath);
        assert_eq!(plan.watch_tailnet, TailnetWatch::Idle);
    }

    #[test]
    fn a_daemon_nothing_would_restart_still_watches_but_never_exits() {
        // Whether the daemon is unreachable and whether ending it would help
        // are two different facts. Started by hand it is exactly as unreachable,
        // so there is exactly as much to watch for — but an exit there is the
        // end of the daemon, with nothing to take its place, which is strictly
        // worse than the loopback bind it would be fixing.
        let plan = plan_bind(&config_with(None), &[], false);
        assert_eq!(plan.origin, BindOrigin::LoopbackFallback);
        assert_eq!(plan.watch_tailnet, TailnetWatch::Report);
    }

    #[test]
    fn a_tailnet_with_no_ipv4_address_is_still_the_loopback_fallback() {
        // The restart loop this closes: the watcher fires on the same fact the
        // planner binds on. If it fired on any address at all while the planner
        // needed an IPv4 one, a tailnet that is up but IPv6-only would restart
        // the daemon into an identical loopback fallback, for ever.
        let v6_only: Vec<IpAddr> = vec!["fd7a:115c:a1e0::1".parse().unwrap()];
        assert_eq!(preferred_tailnet_ip(&v6_only), None);
        let plan = plan_bind(&config_with(None), &v6_only, true);
        assert_eq!(plan.bind, "127.0.0.1:8787".parse().unwrap());
        assert_eq!(plan.origin, BindOrigin::LoopbackFallback);
    }

    #[test]
    fn an_explicit_loopback_bind_never_waits_on_tailscale() {
        // A loopback bind exists so the daemon starts when the tailnet does
        // not, and `tailscale ip` costs up to three seconds of not starting.
        // Loopback is trusted by construction, so the probe could tell us
        // nothing about it anyway.
        assert!(!needs_tailnet_probe(&config_with(Some("127.0.0.1"))));
        assert!(!needs_tailnet_probe(&config_with(Some("::1"))));
        assert_eq!(
            plan_bind(&config_with(Some("127.0.0.1")), &[], true).trust,
            PlaintextTrust::TrustedPath
        );

        // Every other case needs the answer: to classify an explicit address,
        // or to have an address at all.
        assert!(needs_tailnet_probe(&config_with(Some("192.168.1.20"))));
        assert!(needs_tailnet_probe(&config_with(None)));
        assert!(needs_tailnet_probe(&config_with(Some(
            "mac.example.ts.net"
        ))));
    }

    #[test]
    fn a_ws_bind_that_is_not_an_address_falls_through_to_the_tailnet() {
        let plan = plan_bind(&config_with(Some("mac.example.ts.net")), &tailnet(), true);
        assert_eq!(plan.bind, "100.64.12.34:8787".parse().unwrap());
        assert_eq!(plan.origin, BindOrigin::Tailnet);
        // And with no tailnet either, it is the fallback — which is a state the
        // daemon can still recover from.
        let plan = plan_bind(&config_with(Some("")), &[], true);
        assert_eq!(plan.origin, BindOrigin::LoopbackFallback);
        assert_eq!(plan.watch_tailnet, TailnetWatch::Restart);
    }

    #[test]
    fn only_a_network_an_operator_can_vouch_for_counts_as_local() {
        for local in [
            "127.0.0.1",
            "::1",
            "10.1.2.3",
            "172.16.0.9",
            "172.31.255.254",
            "192.168.1.20",
            "169.254.3.4",
            "fd7a:115c:a1e0::1",
            "fe80::1",
        ] {
            assert!(is_local_network(local.parse().unwrap()), "{local}");
        }
        for elsewhere in [
            // The wildcards, which are the whole reason this is a check rather
            // than a bare trust of the key.
            "0.0.0.0",
            "::",
            "203.0.113.7",
            "2606:4700::1111",
            // 100.64/10 is CGNAT-shaped, which proves nothing, and 172.32 is
            // one octet outside the private 172.16/12 block.
            "100.64.99.99",
            "172.32.0.1",
        ] {
            assert!(!is_local_network(elsewhere.parse().unwrap()), "{elsewhere}");
        }
    }

    #[test]
    fn the_tailnet_watch_backs_off_to_a_ceiling_it_keeps_for_ever() {
        assert_eq!(tailnet_watch_delay(0), TAILNET_WATCH_FIRST_DELAY);
        assert_eq!(tailnet_watch_delay(1), Duration::from_secs(10));
        assert_eq!(tailnet_watch_delay(2), Duration::from_secs(20));
        // Capped, and never overflowing however far the count is pushed.
        assert_eq!(tailnet_watch_delay(4), TAILNET_WATCH_MAX_DELAY);
        assert_eq!(tailnet_watch_delay(u32::MAX), TAILNET_WATCH_MAX_DELAY);
        // Monotone: a probe never comes sooner than the one before it.
        for attempt in 1..64 {
            assert!(tailnet_watch_delay(attempt) >= tailnet_watch_delay(attempt - 1));
        }
        // Fast enough at the start to catch a `tailscaled` that is merely slow
        // at login: three looks inside the first minute, before the gaps grow
        // to anything a person would notice.
        let three_looks: u64 = (0..3)
            .map(|attempt| tailnet_watch_delay(attempt).as_secs())
            .sum();
        assert!(
            three_looks <= 60,
            "{three_looks}s to reach the third look is too slow off the mark"
        );
        // The watch lasts as long as the daemon does, so what has to be bounded
        // is not how long it looks but how often: at the ceiling it is one
        // probe a minute, for ever, which is the whole cost of never giving up.
        assert_eq!(TAILNET_WATCH_MAX_DELAY, Duration::from_secs(60));
    }

    /// A probe that answers from a script, and counts how often it was asked.
    fn scripted_probe(
        answer: Vec<IpAddr>,
        after: u32,
    ) -> (
        impl Fn() -> std::future::Ready<Vec<IpAddr>>,
        Arc<std::sync::atomic::AtomicU32>,
    ) {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = Arc::clone(&calls);
        let probe = move || {
            let seen = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::future::ready(if seen >= after {
                answer.clone()
            } else {
                Vec::new()
            })
        };
        (probe, calls)
    }

    // ------------------------------------------------------------------
    // The transport decision: which host the QR carries, and whether a
    // certificate is obtained for it. Every one of the three things this
    // function does off the machine — `tailscale status`, DNS, `tailscale
    // cert` — is handed in, so these run the real decision with none of it.
    // ------------------------------------------------------------------

    /// The MagicDNS name this node would report for itself.
    const MAGIC_DNS: &str = "mac.example.ts.net";

    fn magic_dns() -> std::future::Ready<Option<String>> {
        std::future::ready(Some(MAGIC_DNS.to_string()))
    }

    /// A resolver with one answer in it.
    fn resolving_to(
        addresses: &[&str],
    ) -> impl FnOnce(String) -> std::future::Ready<std::io::Result<Vec<IpAddr>>> {
        let answer: Vec<IpAddr> = addresses
            .iter()
            .map(|address| address.parse().unwrap())
            .collect();
        move |_| std::future::ready(Ok(answer))
    }

    /// The resolver for every bind whose shape already answers the question. A
    /// lookup on the ordinary tailnet path would cost startup latency on every
    /// Mac, and on one running `--accept-dns=false` it would refuse the very
    /// name that works — so being asked at all is the failure.
    fn never_resolves(_: String) -> std::future::Ready<std::io::Result<Vec<IpAddr>>> {
        panic!("the transport looked up a name whose reachability the bind already settles");
    }

    /// A resolver that fails, as this Mac's does for a name it cannot find.
    fn resolution_fails(_: String) -> std::future::Ready<std::io::Result<Vec<IpAddr>>> {
        std::future::ready(Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "nodename nor servname provided, or not known",
        )))
    }

    fn no_certificate(_: String) -> std::future::Ready<Option<tokio_rustls::TlsAcceptor>> {
        std::future::ready(None)
    }

    /// One type for both canned certifiers, so a loop can carry them side by
    /// side. Two `fn` items are two distinct types; a `fn` pointer is the one
    /// thing they have in common.
    type Certifier = fn(String) -> std::future::Ready<Option<tokio_rustls::TlsAcceptor>>;

    /// The name source for a bind that has already decided against carrying a
    /// name, where asking `tailscale status` would be work spent on an answer
    /// that cannot be used.
    fn no_name_asked() -> std::future::Ready<Option<String>> {
        panic!("the transport asked what this node is called after deciding not to use a name");
    }

    /// The certifier for the paths that must obtain nothing: a certificate for
    /// a name this listener is not reachable at protects a connection nobody
    /// opens.
    fn no_certificate_obtained(_: String) -> std::future::Ready<Option<tokio_rustls::TlsAcceptor>> {
        panic!("the transport obtained a certificate for a name it does not advertise");
    }

    /// An acceptor holding no certificate at all.
    ///
    /// Every assertion here is about which host is advertised and whether a
    /// certificate was taken up, never about what would be presented on the
    /// wire — and building one this way keeps `tailscale cert` out of the test
    /// entirely.
    fn a_certificate(_: String) -> std::future::Ready<Option<tokio_rustls::TlsAcceptor>> {
        use tokio_rustls::rustls;
        #[derive(Debug)]
        struct NoKeys;
        impl rustls::server::ResolvesServerCert for NoKeys {
            fn resolve(
                &self,
                _: rustls::server::ClientHello<'_>,
            ) -> Option<Arc<rustls::sign::CertifiedKey>> {
                None
            }
        }
        tls::install_crypto_provider();
        let server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(NoKeys));
        std::future::ready(Some(tokio_rustls::TlsAcceptor::from(Arc::new(server))))
    }

    /// The two ways a resolver declines to put an address behind a name: it
    /// fails outright, or it answers with nothing. Neither is evidence the name
    /// misses this listener, and both have to reach the same decision.
    #[derive(Debug, Clone, Copy)]
    enum Resolver {
        Fails,
        Empty,
    }

    /// `resolve_transport_with` under one of those, with the name source fixed
    /// at this node's MagicDNS name.
    async fn transport_with_resolver<Certify, CertifyFut>(
        config: &Config,
        plan: &BindPlan,
        resolver: Resolver,
        certify: Certify,
    ) -> (Option<tokio_rustls::TlsAcceptor>, Endpoint)
    where
        Certify: FnOnce(String) -> CertifyFut,
        CertifyFut: std::future::Future<Output = Option<tokio_rustls::TlsAcceptor>>,
    {
        match resolver {
            Resolver::Fails => {
                resolve_transport_with(config, plan, magic_dns, resolution_fails, certify).await
            }
            Resolver::Empty => {
                resolve_transport_with(config, plan, magic_dns, resolving_to(&[]), certify).await
            }
        }
    }

    #[tokio::test]
    async fn an_explicit_lan_bind_advertises_the_address_it_listens_on() {
        // The whole of the claim: MagicDNS resolves to the tailnet interface,
        // and this listener is on the LAN interface. A QR carrying that name
        // scans perfectly, resolves perfectly and reaches nothing — which is
        // strictly worse than an address that is visibly what it is, because
        // there is no sign anywhere that it is wrong.
        let config = config_with(Some("192.168.1.20"));
        let plan = plan_bind(&config, &tailnet(), true);
        let (acceptor, endpoint) = resolve_transport_with(
            &config,
            &plan,
            magic_dns,
            resolving_to(&["100.64.12.34"]),
            no_certificate,
        )
        .await;
        assert_eq!(endpoint.host, "192.168.1.20");
        assert_eq!(endpoint.port, 8787);
        assert!(!endpoint.tls);
        assert!(
            acceptor.is_none(),
            "a certificate for a name no phone reaches this listener at protects nothing"
        );
        // And the address it fell back to is one a phone can be sent to, so
        // refusing the name leaves pairing working rather than dead.
        assert_eq!(protocol::pairing::unreachable_host(&endpoint.host), None);
    }

    #[tokio::test]
    async fn a_lan_bind_keeps_a_name_that_really_does_resolve_to_it() {
        // The legitimate case the refusal must not take with it: an operator
        // with a LAN address, a name of their own that points at it, and a
        // certificate for that name. The claim is true, so it is advertised.
        let mut config = config_with(Some("192.168.1.20"));
        config.tls_hostname = Some("mac.lan.example.com".into());
        let plan = plan_bind(&config, &tailnet(), true);
        let (acceptor, endpoint) = resolve_transport_with(
            &config,
            &plan,
            magic_dns,
            resolving_to(&["192.168.1.20"]),
            a_certificate,
        )
        .await;
        assert_eq!(endpoint.host, "mac.lan.example.com");
        assert!(endpoint.tls);
        assert!(acceptor.is_some());
    }

    #[tokio::test]
    async fn a_tailnet_bind_advertises_its_magicdns_name_without_looking_anything_up() {
        // The ordinary path, and the one that must neither change nor slow
        // down: the name maps to this node's tailnet addresses by construction,
        // because both came from the same node. The resolver panics if it is
        // consulted, so this is a proof and not an observation.
        let config = config_with(None);
        let plan = plan_bind(&config, &tailnet(), true);
        assert_eq!(plan.bind, "100.64.12.34:8787".parse().unwrap());
        let (acceptor, endpoint) =
            resolve_transport_with(&config, &plan, magic_dns, never_resolves, a_certificate).await;
        assert_eq!(endpoint.host, MAGIC_DNS);
        assert!(endpoint.tls);
        assert!(acceptor.is_some());

        // The same by an explicit `ws_bind` onto this node's own tailnet
        // address, which arrives at `BindOrigin::Explicit` and is on the
        // tailnet all the same.
        let config = config_with(Some("100.64.12.34"));
        let plan = plan_bind(&config, &tailnet(), true);
        assert_eq!(plan.origin, BindOrigin::Explicit);
        let (_, endpoint) =
            resolve_transport_with(&config, &plan, magic_dns, never_resolves, no_certificate).await;
        assert_eq!(endpoint.host, MAGIC_DNS);
    }

    #[tokio::test]
    async fn a_wildcard_bind_accepts_any_name_of_this_mac_without_looking_it_up() {
        // `0.0.0.0` puts the listener on every interface this Mac has, so a
        // name that arrives at the Mac arrives at the listener. There is
        // nothing a lookup could add.
        for wildcard in ["0.0.0.0", "::"] {
            let config = config_with(Some(wildcard));
            let plan = plan_bind(&config, &tailnet(), true);
            let (_, endpoint) =
                resolve_transport_with(&config, &plan, magic_dns, never_resolves, no_certificate)
                    .await;
            assert_eq!(endpoint.host, MAGIC_DNS, "{wildcard}");
        }
    }

    #[tokio::test]
    async fn a_loopback_bind_still_advertises_loopback() {
        // Decided by the bind before anything else is asked, because no device
        // but this Mac can open a connection here at any name.
        let config = config_with(Some("127.0.0.1"));
        let plan = plan_bind(&config, &[], true);
        let (acceptor, endpoint) = resolve_transport_with(
            &config,
            &plan,
            no_name_asked,
            never_resolves,
            no_certificate_obtained,
        )
        .await;
        assert_eq!(endpoint.host, "127.0.0.1");
        assert!(!endpoint.tls);
        assert!(acceptor.is_none());
        // Which is what makes `codeconnect pair` refuse to print a code.
        assert!(protocol::pairing::unreachable_host(&endpoint.host).is_some());
    }

    #[tokio::test]
    async fn a_name_this_mac_cannot_resolve_is_not_carried_without_a_certificate_behind_it() {
        // A resolver that says nothing has refuted nothing, so the name is not
        // refused outright — but with no certificate obtained there is no
        // reason a phone would need it, and the address this daemon knows it
        // listens on is the better of the two.
        let mut config = config_with(Some("192.168.1.20"));
        config.tls_hostname = Some("nowhere.invalid".into());
        let plan = plan_bind(&config, &tailnet(), true);
        for resolver in [Resolver::Fails, Resolver::Empty] {
            let (acceptor, endpoint) =
                transport_with_resolver(&config, &plan, resolver, no_certificate).await;
            assert_eq!(endpoint.host, "192.168.1.20", "{resolver:?}");
            assert!(acceptor.is_none(), "{resolver:?}");
        }
    }

    /// The permissive half of the same discriminator: a name the operator set
    /// is theirs to vouch for, and silence from this Mac's resolver does not
    /// overrule it.
    ///
    /// Split-horizon DNS is the ordinary reason this happens — the name answers
    /// for the phone and not for the Mac serving it — so a lookup here settles
    /// nothing, and a daemon that refused on it would turn a working `wss://`
    /// listener into one that serves nobody. The certificate is what makes the
    /// name the thing a phone must dial, which is why it is the case that
    /// separates this from [`a_name_this_mac_cannot_resolve_is_not_carried_without_a_certificate_behind_it`]:
    /// with no certificate both halves fall back to the bind and the
    /// discriminator is invisible.
    #[tokio::test]
    async fn an_operators_own_name_is_not_overruled_by_a_resolver_that_stays_silent() {
        let mut config = config_with(Some("192.168.1.20"));
        config.tls_hostname = Some("mac.lan.example".into());
        let plan = plan_bind(&config, &tailnet(), true);

        for resolver in [Resolver::Fails, Resolver::Empty] {
            let (acceptor, endpoint) =
                transport_with_resolver(&config, &plan, resolver, a_certificate).await;
            assert_eq!(
                endpoint.host, "mac.lan.example",
                "{resolver:?}: silence is not evidence against a name the operator named"
            );
            assert!(endpoint.tls, "{resolver:?}: the certificate is served");
            assert!(acceptor.is_some(), "{resolver:?}");
        }
    }

    #[tokio::test]
    async fn a_magicdns_name_is_refused_beside_a_lan_bind_even_when_nothing_resolves() {
        // The one place silence still refutes. A MagicDNS name maps to this
        // node's tailnet address and to nothing else, and Tailscale hands out
        // no RFC1918 address — so no lookup is needed to know the two disagree,
        // and no certificate is worth obtaining for a name that would send a
        // phone to another interface.
        let config = config_with(Some("192.168.1.20"));
        let plan = plan_bind(&config, &tailnet(), true);
        for resolver in [Resolver::Fails, Resolver::Empty] {
            let (acceptor, endpoint) =
                transport_with_resolver(&config, &plan, resolver, no_certificate_obtained).await;
            assert_eq!(endpoint.host, "192.168.1.20", "{resolver:?}");
            assert!(acceptor.is_none(), "{resolver:?}");
        }
    }

    #[tokio::test]
    async fn a_probe_that_raced_does_not_cost_a_daemon_the_certificate_it_already_holds() {
        // The bind is this node's own tailnet address and the operator named
        // it, but `tailscale ip` lost its three-second race, so the daemon
        // cannot confirm the address from the probe. This Mac's resolver cannot
        // answer for MagicDNS either — `--accept-dns=false` does exactly that.
        // None of it is evidence the name misses this listener, and refusing it
        // would take a daemon that serves wss:// down to one that refuses every
        // connection.
        let config = config_with(Some("100.64.12.34"));
        let raced = plan_bind(&config, &[], true);
        assert!(
            !raced.bind_is_tailnet,
            "the empty probe is the whole premise"
        );

        // Counted, not inferred: the regression was that the certificate on
        // disk was never even asked for, so the number of times the certifier
        // is reached is the thing under test.
        let certify_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = Arc::clone(&certify_calls);
        let counting = move |hostname: String| {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            a_certificate(hostname)
        };

        let (acceptor, endpoint) =
            transport_with_resolver(&config, &raced, Resolver::Fails, counting).await;
        assert_eq!(
            certify_calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the certificate this daemon already holds has to be asked for"
        );
        assert_eq!(
            endpoint.host, MAGIC_DNS,
            "an unconfirmed name with a certificate behind it is still the name a phone dials"
        );
        assert!(endpoint.tls, "the listener still serves wss://");
        assert!(acceptor.is_some());
    }

    /// The bind decides whether the listener is reachable at a name; only
    /// CodeConnect's own launchd job decides whether ending this process is a
    /// recovery. Reading "a label exists" as "something will restart me" is the
    /// promise the daemon must not make about a process nothing is watching.
    #[test]
    fn only_codeconnects_own_launchd_job_counts_as_something_that_would_restart_it() {
        // `XPC_SERVICE_NAME` is process-global while tests run in parallel, and
        // `state`'s sentinel test writes it too — see [`LaunchdLabelEnv`], which
        // is what makes those two exclusive rather than merely orderly.
        let env = LaunchdLabelEnv::take();

        for (label, restartable) in [
            (Some(protocol::LAUNCHD_LABEL), true),
            // A `ccd` started from inside somebody else's launchd job carries
            // that job's label, and that job will never restart it.
            (Some("com.example.other"), false),
            (Some("com.codeconnect.ccd.extra"), false),
            // macOS's "not an XPC service" sentinel, and a shell.
            (Some("0"), false),
            (Some(""), false),
            (None, false),
        ] {
            env.set(label);
            assert_eq!(
                managed_by_codeconnect_job(),
                restartable,
                "XPC_SERVICE_NAME={label:?}"
            );
        }
    }

    /// [`resolve_transport`] hands three things to [`resolve_transport_with`],
    /// and every other test here supplies its own three — so a production
    /// wiring that named the wrong function would leave all of them green.
    ///
    /// Run both on one input and require the same answer. The input is chosen
    /// so the whole decision is hermetic: `tls_hostname` is an IP literal, which
    /// `lookup_host` parses without asking DNS anything, and it names an address
    /// the listener is not on — so the name is refused and the certifier, which
    /// shells out to `tailscale cert`, is never reached.
    ///
    /// **What this covers and what it leaves to its neighbour.** It pins the
    /// resolver, because that is the seam this input exercises. The certifier
    /// is pinned by
    /// [`the_production_transport_obtains_certificates_with_the_production_certifier`],
    /// which needs an input this one deliberately does not have. The name
    /// source is pinned by neither: `tls_hostname` is set here, and setting it
    /// is what makes the input hermetic, so on this path `tls::magic_dns_name`
    /// is never called. Reaching it means letting `resolve_transport` ask
    /// `tailscale status` what this machine is called — an answer that differs
    /// per machine and is absent on most — so what stands behind that argument
    /// is review, not this test.
    #[tokio::test]
    async fn the_production_transport_resolves_names_with_the_production_resolver() {
        let mut config = config_with(Some("192.168.1.20"));
        config.tls_hostname = Some("203.0.113.9".into());
        let plan = plan_bind(&config, &tailnet(), true);

        let (wired, wired_endpoint) = resolve_transport(&config, &plan).await;
        let (spelled_out, spelled_out_endpoint) = resolve_transport_with(
            &config,
            &plan,
            tls::magic_dns_name,
            resolve_name,
            |hostname| certificate_for(hostname, config.cert_refresh_days),
        )
        .await;

        assert_eq!(wired_endpoint.host, spelled_out_endpoint.host);
        assert_eq!(wired_endpoint.tls, spelled_out_endpoint.tls);
        assert_eq!(wired.is_some(), spelled_out.is_some());
        // And the answer both give is the one this input has: a name pointing
        // somewhere else is refused, so the bind is what the QR carries.
        assert_eq!(wired_endpoint.host, "192.168.1.20");
        assert!(!wired_endpoint.tls);
        assert!(wired.is_none());
    }

    /// A self-signed P-256 certificate for `hostname` and its key, in the two
    /// PEM shapes [`tls::acceptor`] reads.
    ///
    /// Generated at test time and never committed. A private key in a tracked
    /// file fails this repo's hygiene check, and a fixture carrying a real
    /// expiry is a test that starts failing on a date nobody chose — so
    /// `notAfter` is in 2100, far enough out that the 30-day refresh floor
    /// [`tls::ensure`] applies can never be reached by the clock.
    ///
    /// Genuinely self-signed, because it has to be: `with_single_cert` parses
    /// the leaf and compares its `SubjectPublicKeyInfo` against the private
    /// key's, so a certificate carrying anybody else's public key never becomes
    /// an acceptor. Two more things that parser insists on and a hand-built
    /// certificate can easily get wrong: the `[0] EXPLICIT` v3 version tag has
    /// to be there, and `tbsCertificate.signature` has to be byte-identical to
    /// the outer `signatureAlgorithm`.
    fn self_signed_certificate(hostname: &str) -> (String, String) {
        use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};

        const SEQUENCE: u8 = 0x30;
        const SET: u8 = 0x31;
        const INTEGER: u8 = 0x02;
        const BIT_STRING: u8 = 0x03;
        const UTF8_STRING: u8 = 0x0c;
        const UTC_TIME: u8 = 0x17;
        const GENERALIZED_TIME: u8 = 0x18;
        const VERSION: u8 = 0xa0;
        // Object identifiers with their tag and length already on them:
        // ecdsa-with-SHA256, id-ecPublicKey, prime256v1, commonName.
        const ECDSA_SHA256: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
        const EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
        const PRIME256V1: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
        const COMMON_NAME: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03];

        /// One DER tag-length-value. Everything built here is well under 64 KiB,
        /// so two length bytes are the most any field needs.
        fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
            let mut out = vec![tag];
            match body.len() {
                len if len < 0x80 => out.push(len as u8),
                len if len < 0x100 => out.extend_from_slice(&[0x81, len as u8]),
                len => out.extend_from_slice(&[0x82, (len >> 8) as u8, len as u8]),
            }
            out.extend_from_slice(body);
            out
        }

        /// PEM armour, wrapped at the customary 64 characters.
        ///
        /// The label is an argument rather than written into the line below,
        /// because this repo's CI greps every file for the opening line of a
        /// private key and cannot tell a generator from a leak. Inlining it
        /// would fail that check on a file that contains no key at all.
        fn armour(label: &str, der: &[u8]) -> String {
            use base64::Engine as _;
            let body = base64::engine::general_purpose::STANDARD.encode(der);
            let lines: Vec<&str> = body
                .as_bytes()
                .chunks(64)
                .map(|line| std::str::from_utf8(line).expect("base64 is ascii"))
                .collect();
            format!(
                "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
                lines.join("\n")
            )
        }

        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
            .expect("generating a P-256 key");
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
            .expect("reading back the key just generated");

        let algorithm = tlv(SEQUENCE, ECDSA_SHA256);
        // One RDN holding one commonName, used as both issuer and subject.
        // Issuer equal to subject is half of self-signed; the other half is the
        // signature below, made with this certificate's own key.
        let name = tlv(
            SEQUENCE,
            &tlv(
                SET,
                &tlv(
                    SEQUENCE,
                    &[
                        COMMON_NAME,
                        tlv(UTF8_STRING, hostname.as_bytes()).as_slice(),
                    ]
                    .concat(),
                ),
            ),
        );
        let spki = tlv(
            SEQUENCE,
            &[
                tlv(SEQUENCE, &[EC_PUBLIC_KEY, PRIME256V1].concat()),
                // The leading zero is the BIT STRING's count of unused bits.
                tlv(
                    BIT_STRING,
                    &[&[0u8][..], key.public_key().as_ref()].concat(),
                ),
            ]
            .concat(),
        );
        // GeneralizedTime for `notAfter`, because UTCTime cannot express a year
        // past 2049 and this fixture is deliberately later than that.
        let validity = tlv(
            SEQUENCE,
            &[
                tlv(UTC_TIME, b"200101000000Z"),
                tlv(GENERALIZED_TIME, b"21000101000000Z"),
            ]
            .concat(),
        );
        let tbs = tlv(
            SEQUENCE,
            &[
                tlv(VERSION, &tlv(INTEGER, &[2])), // v3
                tlv(INTEGER, &[1]),                // serialNumber
                algorithm.clone(),
                name.clone(), // issuer
                validity,
                name, // subject
                spki,
            ]
            .concat(),
        );
        let signature = key.sign(&rng, &tbs).expect("signing the certificate body");
        let certificate = tlv(
            SEQUENCE,
            &[
                tbs,
                algorithm,
                tlv(BIT_STRING, &[&[0u8][..], signature.as_ref()].concat()),
            ]
            .concat(),
        );

        (
            armour("CERTIFICATE", &certificate),
            armour("PRIVATE KEY", pkcs8.as_ref()),
        )
    }

    /// The other half of that wiring: [`resolve_transport`] hands
    /// [`resolve_transport_with`] the real [`certificate_for`]. Every other
    /// transport test here supplies a canned certifier, so a build that named
    /// the wrong function — or none — would serve `ws://` with all of them
    /// still green.
    ///
    /// **The discriminator.** With a valid certificate on disk for the name
    /// being advertised, the production transport comes back with an acceptor
    /// and `tls: true`. A wiring that reached no certifier, or one that produces
    /// nothing, falls to the `plain_named` branch instead and comes back
    /// `tls: false` with no acceptor, on the same host. That difference is the
    /// whole of this test.
    ///
    /// **Hermetic: no DNS, no `tailscale`, no ACME.**
    /// * `ws_bind` is the unspecified address, so [`advertised_name_reach`]
    ///   answers from the shape of the bind and the resolver is never called.
    /// * `tls_hostname` is set, so `tailscale status` is never asked for a name.
    /// * The certificate on disk has decades left, so [`tls::ensure`] returns
    ///   the cached material and never runs `tailscale cert`. Asserted rather
    ///   than assumed — the line it logs on that branch alone is checked below —
    ///   and the name is under `.invalid`, which no certificate authority will
    ///   ever issue for, so there is no second way for one to appear.
    ///
    /// **Why a child process.** `CODECONNECT_HOME` is what points
    /// `protocol::tls_dir()` at the fixture, and it is process-global: setting
    /// it here would move the state directory underneath every test running
    /// beside this one. The child is this same binary running this same test,
    /// with the sentinel below telling it which half of the test it is.
    #[tokio::test]
    async fn the_production_transport_obtains_certificates_with_the_production_certifier() {
        const CHILD: &str = "CCD_TRANSPORT_CERTIFIER_CHILD";
        const HOSTNAME: &str = "transport-wiring.invalid";

        if std::env::var_os(CHILD).is_some() {
            let mut config = config_with(Some("0.0.0.0"));
            config.tls_hostname = Some(HOSTNAME.into());
            // No tailnet answered and none is needed: the unspecified address is
            // the bind whatever `tailscale ip` would have said.
            let plan = plan_bind(&config, &[], false);
            // Where `main` installs it, and for the same reason: before
            // anything builds a `ServerConfig`.
            tls::install_crypto_provider();

            let (acceptor, endpoint) = resolve_transport(&config, &plan).await;
            assert_eq!(endpoint.host, HOSTNAME, "the certified name is the QR host");
            assert!(
                endpoint.tls,
                "the production wiring reached a certifier that found the certificate on disk"
            );
            assert!(acceptor.is_some(), "and took up the acceptor built from it");
            return;
        }

        let home = std::env::temp_dir().join(format!(
            "ccd-transport-wiring-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let tls_dir = home.join("tls");
        std::fs::create_dir_all(&tls_dir).unwrap();
        let (certificate, key) = self_signed_certificate(HOSTNAME);
        std::fs::write(tls_dir.join(format!("{HOSTNAME}.crt")), certificate).unwrap();
        std::fs::write(tls_dir.join(format!("{HOSTNAME}.key")), key).unwrap();

        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "tests::the_production_transport_obtains_certificates_with_the_production_certifier",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("CODECONNECT_HOME", &home)
            .output()
            .unwrap();
        let told = String::from_utf8_lossy(&child.stderr).into_owned();
        let harness = String::from_utf8_lossy(&child.stdout).into_owned();
        let _ = std::fs::remove_dir_all(&home);

        assert!(
            child.status.success(),
            "the production transport did not serve wss:// from a certificate on disk: {told}"
        );
        // The child is picked out by a path spelled as a string, so a rename
        // leaves it running no test at all and exiting zero — a green test that
        // asserts nothing. Counted, so that goes loud instead.
        assert!(
            harness.contains("test result: ok. 1 passed"),
            "the child ran no test: the path in `args` no longer names this one. {harness}"
        );
        // And it got there the hermetic way. `tls::ensure` says this on the
        // branch that returns cached material and on no other, so the line is
        // what rules out a certificate obtained by shelling out.
        assert!(
            told.contains(&format!("tls: using cached certificate for {HOSTNAME}")),
            "the child got a certificate some way other than reading the fixture: {told}"
        );
        // Written by `certificate_for` and nowhere else, so it is the certifier
        // itself — not merely something that returned an acceptor — that ran.
        assert!(
            told.contains(&format!("tls: serving wss:// as {HOSTNAME}")),
            "{told}"
        );
    }

    /// The two halves of one tailnet, told apart by nothing but address family,
    /// must be given the same account of themselves.
    #[test]
    fn a_tailnet_address_the_probe_could_not_confirm_is_never_called_public() {
        for address in ["100.64.12.34", "fd7a:115c:a1e0::1"] {
            let config = config_with(Some(address));
            // No probe answered, so neither address can be confirmed.
            let plan = plan_bind(&config, &[], false);
            let advice = plaintext_refusal_advice(&config, &plan, false)
                .expect("a bind with no certificate and no confirmed tailnet refuses everything");
            assert!(advice.contains("could not confirm"), "{address}: {advice}");
            assert!(
                !advice.contains("plaintext is not private on this address"),
                "{address}: WireGuard makes that false on a tailnet address: {advice}"
            );
        }
    }

    #[tokio::test]
    async fn a_resolver_that_never_answers_costs_the_daemon_a_bounded_wait() {
        // This runs before a single socket is opened, so a resolver that hangs
        // must not be able to hold the daemon shut. Refusing the name is the
        // same answer as any other lookup that did not establish reachability.
        let config = config_with(Some("192.168.1.20"));
        let plan = plan_bind(&config, &tailnet(), true);
        let started = std::time::Instant::now();
        let (_, endpoint) = resolve_transport_with(
            &config,
            &plan,
            magic_dns,
            |_| std::future::pending::<std::io::Result<Vec<IpAddr>>>(),
            no_certificate,
        )
        .await;
        assert_eq!(endpoint.host, "192.168.1.20");
        assert!(
            started.elapsed() < ADVERTISED_NAME_TIMEOUT * 2,
            "the lookup was not bounded"
        );
    }

    #[tokio::test]
    async fn tls_turned_off_advertises_the_bind_and_asks_nothing() {
        // With no certificate in prospect there is no name to be had either:
        // the endpoint the phone is given is the socket, plainly.
        let mut config = config_with(Some("192.168.1.20"));
        config.tls = false;
        let plan = plan_bind(&config, &tailnet(), true);
        let (acceptor, endpoint) = resolve_transport_with(
            &config,
            &plan,
            no_name_asked,
            never_resolves,
            no_certificate_obtained,
        )
        .await;
        assert_eq!(endpoint.host, "192.168.1.20");
        assert!(!endpoint.tls);
        assert!(acceptor.is_none());
    }

    #[tokio::test]
    async fn a_node_with_no_magicdns_name_advertises_the_bind() {
        // `tailscale status` answered nothing, so there is no name to check and
        // none to carry.
        let config = config_with(None);
        let plan = plan_bind(&config, &tailnet(), true);
        let (_, endpoint) = resolve_transport_with(
            &config,
            &plan,
            || std::future::ready(None),
            never_resolves,
            no_certificate_obtained,
        )
        .await;
        assert_eq!(endpoint.host, "100.64.12.34");
    }

    /// **No bind advertises a host this daemon knows a phone would not arrive
    /// at.** That is the rule, and it is weaker than "everything advertised was
    /// checked" — deliberately, because one branch carries a name nothing
    /// confirmed, and a test named for the stronger rule would have to leave
    /// that branch out to stay green.
    ///
    /// So all three of [`AdvertisedName`] are driven here: a resolver that puts
    /// the name at the bind, one that puts it somewhere else, and one that says
    /// nothing at all — each against a certifier that produces one and a
    /// certifier that produces none. The unconfirmed-with-a-certificate case is
    /// the one the old shape of this test could not reach, and it is counted
    /// rather than merely permitted: a loop that quietly stopped producing it
    /// would leave that branch untested while still passing.
    #[tokio::test]
    async fn no_bind_advertises_a_host_it_knows_a_phone_would_not_arrive_at() {
        let mut unconfirmed_names_carried = 0u32;
        for bind in [
            None,
            Some("127.0.0.1"),
            Some("::1"),
            Some("100.64.12.34"),
            Some("192.168.1.20"),
            Some("203.0.113.7"),
            Some("0.0.0.0"),
        ] {
            for confirmed in [Vec::new(), tailnet()] {
                let config = config_with(bind);
                let plan = plan_bind(&config, &confirmed, true);
                let bound = plan.bind.ip();
                let here = bound.to_string();
                // Two binds need no lookup to settle: the listener answers on
                // every interface, or the name is this node's own name for the
                // tailnet address it is bound to.
                let settled_by_the_bind = bound.is_unspecified() || plan.bind_is_tailnet;
                // Here, somewhere else, and nowhere — the third being the
                // silence that leaves a name unverified rather than refuted.
                for answer in [Some(here.as_str()), Some("203.0.113.9"), None] {
                    let name_arrives_here = answer == Some(here.as_str());
                    let addresses: Vec<&str> = answer.into_iter().collect();
                    for (certified, certifier) in [
                        (false, no_certificate as Certifier),
                        (true, a_certificate as Certifier),
                    ] {
                        let (acceptor, endpoint) = resolve_transport_with(
                            &config,
                            &plan,
                            magic_dns,
                            resolving_to(&addresses),
                            certifier,
                        )
                        .await;
                        let case = format!(
                            "{bind:?} bound {here}, resolver said {answer:?}, certificate \
                             {certified}, advertised {}",
                            endpoint.host
                        );
                        assert_eq!(endpoint.port, plan.bind.port(), "{case}");
                        // `tls: true` is the phone's instruction to open a TLS
                        // handshake, so it may never outrun the acceptor that
                        // would answer one.
                        assert_eq!(endpoint.tls, acceptor.is_some(), "{case}");
                        // The bound address is honest at every bind, always.
                        if endpoint.host == here {
                            continue;
                        }
                        // A name was carried. Two ways that needs no defending:
                        // the resolver put it here, or the shape of the bind
                        // did.
                        if name_arrives_here || settled_by_the_bind {
                            continue;
                        }
                        // Which leaves the third, and only the third. A name the
                        // resolver placed elsewhere is refuted, and refuted
                        // names are never carried whatever the certifier says.
                        assert!(answer.is_none(), "{case}");
                        // And silence carries a name only behind a certificate,
                        // because validating one is the whole reason a phone
                        // dials a name rather than the address the daemon knows
                        // it listens on.
                        assert!(endpoint.tls && acceptor.is_some(), "{case}");
                        unconfirmed_names_carried += 1;
                    }
                }
            }
        }
        assert!(
            unconfirmed_names_carried > 0,
            "no case reached the branch that advertises an unconfirmed name, so the loop above \
             proves nothing about it"
        );
    }

    /// A probe that answers a script in order and then repeats its last answer
    /// for ever: a tailnet that arrives, moves and goes away again.
    fn probe_sequence(script: Vec<Vec<IpAddr>>) -> impl Fn() -> std::future::Ready<Vec<IpAddr>> {
        assert!(!script.is_empty(), "a script with no answers in it");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        move || {
            let seen = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::future::ready(script[seen.min(script.len() - 1)].clone())
        }
    }

    /// A report sink that keeps what the watcher said, so the cadence of the
    /// unmanaged path is asserted rather than described.
    #[allow(clippy::type_complexity)]
    fn recording_report() -> (
        impl Fn(Option<IpAddr>),
        Arc<std::sync::Mutex<Vec<Option<IpAddr>>>>,
    ) {
        let said = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&said);
        (move |found| sink.lock().unwrap().push(found), said)
    }

    /// The sink for the paths that have nothing to report, which must therefore
    /// never reach it.
    fn no_report(_: Option<IpAddr>) {
        panic!("a watcher that recovers by exiting reported instead of exiting");
    }

    #[tokio::test]
    async fn a_watcher_with_nothing_to_watch_waits_for_ever_rather_than_returning() {
        // The most dangerous shape a watcher can take. Every task joined in the
        // `select!` ends the process when it returns, so a watcher that
        // completed with nothing to report would exit a working daemon — and
        // idle, it would do it within microseconds of startup, on every
        // machine, for ever.
        let (probe, calls) = scripted_probe(vec!["100.64.12.34".parse().unwrap()], 0);
        let idle = tokio::time::timeout(
            Duration::from_millis(200),
            watch_tailnet_with(TailnetWatch::Idle, probe, |_| Duration::ZERO, no_report),
        )
        .await;
        assert!(
            idle.is_err(),
            "an idle watcher returned, which exits the daemon at startup"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "an idle watcher must not probe either"
        );
    }

    #[tokio::test]
    async fn a_tailnet_that_arrives_after_hundreds_of_probes_still_ends_the_watcher() {
        // A Mac whose Tailscale is brought up an hour after login, or the next
        // morning, is in exactly the state a Mac that lost the race by a second
        // is in, and is recoverable in exactly the same way. Any bound on how
        // many times the daemon looks is a length of absence past which it
        // stays unreachable while the fix sits there unnoticed.
        let (probe, calls) = scripted_probe(vec!["100.64.12.34".parse().unwrap()], 500);
        tokio::time::timeout(
            Duration::from_secs(10),
            watch_tailnet_with(TailnetWatch::Restart, probe, |_| Duration::ZERO, no_report),
        )
        .await
        .expect(
            "the watcher must still end, which is how a restart is asked for, however long the \
             tailnet took to arrive",
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            501,
            "it must look until there is an answer, then stop at the first one"
        );
    }

    #[tokio::test]
    async fn a_tailnet_that_arrives_late_ends_the_watcher_that_asks_for_a_restart() {
        // How the recovery is asked for. `resolve_bind` runs once and
        // `daemon.endpoint` is fixed at construction, so a daemon that lost the
        // three-second race at login cannot bind, certify or advertise the
        // tailnet address without a fresh process. Returning is the request;
        // whether the fresh process wins its own probe is not something this
        // one can observe, and not something the watcher promises.
        let (probe, calls) = scripted_probe(vec!["100.64.12.34".parse().unwrap()], 2);
        tokio::time::timeout(
            Duration::from_secs(10),
            watch_tailnet_with(TailnetWatch::Restart, probe, |_| Duration::ZERO, no_report),
        )
        .await
        .expect("the watcher must end, because ending is what launchd reads as a restart request");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "it must stop at the first answer, not keep probing"
        );
    }

    /// A standing instruction repeats after exactly
    /// [`TAILNET_REPORT_REPEAT_PROBES`] probes.
    ///
    /// Asserted as the gap between the probes a report was made on, not as a
    /// count of reports. A count is equally satisfied by an interval one probe
    /// longer than the constant names — which is exactly what reading the
    /// counter before incrementing it produces, and what turns the hour this
    /// constant documents into sixty-one minutes.
    #[tokio::test]
    async fn a_standing_instruction_repeats_on_exactly_the_interval_it_names() {
        let address: IpAddr = "100.64.12.34".parse().unwrap();
        // Three full intervals, then the probe stalls so the endless loop has
        // somewhere to stop rather than spinning until the timeout.
        let budget = TAILNET_REPORT_REPEAT_PROBES * 3 + 1;
        let probes = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = Arc::clone(&probes);
        let probe = move || {
            let spent = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            async move {
                if spent > budget {
                    std::future::pending::<()>().await;
                }
                vec![address]
            }
        };

        let spoken_at = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&spoken_at);
        let seen = Arc::clone(&probes);
        let report = move |_| {
            sink.lock()
                .unwrap()
                .push(seen.load(std::sync::atomic::Ordering::Relaxed));
        };

        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            watch_tailnet_with(TailnetWatch::Report, probe, |_| Duration::ZERO, report),
        )
        .await;

        let spoken_at = spoken_at.lock().unwrap();
        assert!(
            spoken_at.len() >= 4,
            "three intervals must fit four reports: {spoken_at:?}"
        );
        assert_eq!(spoken_at[0], 1, "the first sighting is never delayed");
        for pair in spoken_at.windows(2) {
            assert_eq!(
                pair[1] - pair[0],
                TAILNET_REPORT_REPEAT_PROBES,
                "repeats must land exactly {TAILNET_REPORT_REPEAT_PROBES} probes apart: \
                 {spoken_at:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_watcher_nothing_would_restart_reports_the_tailnet_and_still_never_returns() {
        // The half of the recovery a hand-started daemon can perform. Ending
        // the process is the other half, and here it is not a recovery at all:
        // nothing would start this daemon again, so returning would leave the
        // Mac with no daemon instead of an unreachable one.
        let (probe, calls) = scripted_probe(vec!["100.64.12.34".parse().unwrap()], 2);
        let (report, said) = recording_report();
        let outcome = tokio::time::timeout(
            Duration::from_millis(400),
            watch_tailnet_with(TailnetWatch::Report, probe, |_| Duration::ZERO, report),
        )
        .await;
        assert!(
            outcome.is_err(),
            "a watcher with nothing behind it returned, which ends a daemon nobody would start \
             again"
        );
        // It kept looking long past the arrival — a bounded look would show as
        // a small count here.
        let looks = calls.load(std::sync::atomic::Ordering::Relaxed);
        assert!(looks > 100, "it stopped looking after {looks} probes");

        let said = said.lock().unwrap();
        // Every report names the address, because it never went away: a
        // withdrawal here would be the watcher reporting a state it was not in.
        assert!(
            said.iter().all(Option::is_some),
            "an address that never went away was reported as gone: {said:?}"
        );
        // Said more than once, because the instruction stays true and a single
        // line can be rotated out from under an operator who was away — and
        // said no oftener than the floor, because one address repeated every
        // time it is seen is a line nobody reads.
        assert!(
            said.len() >= 2,
            "said {} time(s) in {looks} probes: an instruction that stands has to be repeatable",
            said.len()
        );
        assert!(
            said.len() as u32 <= looks / TAILNET_REPORT_REPEAT_PROBES + 1,
            "{} reports in {looks} probes is more often than the floor allows",
            said.len()
        );
    }

    #[tokio::test]
    async fn a_watcher_nothing_would_restart_speaks_again_whenever_the_answer_changes() {
        // What it says is an instruction — restart, and this daemon is
        // reachable at that address — so it has to be made again whenever the
        // address it names changes, and withdrawn when there is no longer one.
        // An instruction naming an address that has since gone is worse than
        // silence, and the same instruction repeated every minute is the same
        // as none.
        let first: IpAddr = "100.64.12.34".parse().unwrap();
        let moved: IpAddr = "100.64.99.1".parse().unwrap();
        let probe = probe_sequence(vec![
            // Nothing yet, and nothing to say about it.
            vec![],
            vec![],
            // It arrives, and is still there on the next look.
            vec![first],
            vec![first],
            // A different address: the instruction has changed.
            vec![moved],
            // Gone: the instruction left standing would name nothing.
            vec![],
            vec![],
            // Back again, and it stays — this is what the probe repeats.
            vec![first],
        ]);
        // Exactly the eight looks the script has answers for, and then a gap
        // no test budget reaches — so what was said is the whole of what the
        // changes produced, with none of the long-floor repeats mixed in.
        fn eight_looks_then_stall(attempt: u32) -> Duration {
            if attempt < 8 {
                Duration::ZERO
            } else {
                Duration::from_secs(3_600)
            }
        }
        let (report, said) = recording_report();
        let outcome = tokio::time::timeout(
            Duration::from_millis(400),
            watch_tailnet_with(TailnetWatch::Report, probe, eight_looks_then_stall, report),
        )
        .await;
        assert!(outcome.is_err(), "this watcher must never return");
        assert_eq!(
            *said.lock().unwrap(),
            vec![Some(first), Some(moved), None, Some(first)],
            "each change said once, and nothing said while the answer stood still"
        );
    }

    // ------------------------------------------------------------------
    // Independent adversarial validation of the plaintext opt-in (F2) and
    // the tailnet watcher (F5). Written against the promises rather than
    // against the implementation, and deliberately overlapping the tests
    // above: two of these are the same claim asked at the exact edge of
    // every range, and one is the coupling between the planner and the
    // watcher stated as an equivalence rather than as two examples.
    // ------------------------------------------------------------------

    /// Every address the opt-in is allowed to cover, at the exact edges of
    /// each range it names, and in more than one spelling where there is one.
    const VOUCHABLE: &[&str] = &[
        // Loopback, which the policy trusts before the opt-in is consulted.
        "127.0.0.0",
        "127.0.0.1",
        "127.255.255.255",
        "::1",
        "0000:0000:0000:0000:0000:0000:0000:0001",
        // 10/8, 172.16/12, 192.168/16 at both ends.
        "10.0.0.0",
        "10.255.255.255",
        "172.16.0.0",
        "172.31.255.255",
        "192.168.0.0",
        "192.168.255.255",
        // 169.254/16 link-local.
        "169.254.0.0",
        "169.254.255.255",
        // fc00::/7 unique-local at both ends of both halves.
        "fc00::",
        "fcff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        "fd00::",
        "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        // fe80::/10 link-local at both ends.
        "fe80::",
        "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
    ];

    /// One step outside each of those ranges, plus every shape that must
    /// never be vouchable however it is spelled.
    const NOT_VOUCHABLE: &[&str] = &[
        // One address below and one above each private range.
        "9.255.255.255",
        "11.0.0.0",
        "172.15.255.255",
        "172.32.0.0",
        "192.167.255.255",
        "192.169.0.0",
        "169.253.255.255",
        "169.255.0.0",
        "126.255.255.255",
        "128.0.0.0",
        // The wildcards, in every spelling of them.
        "0.0.0.0",
        "::",
        "0:0:0:0:0:0:0:0",
        // Adjacent to the wildcard but not it.
        "0.0.0.1",
        "255.255.255.255",
        // CGNAT: the shape of a tailnet address proves nothing about who
        // carries the packets, at both ends of 100.64/10 and outside it.
        "100.63.255.255",
        "100.64.0.0",
        "100.100.100.100",
        "100.127.255.255",
        "100.128.0.0",
        // Plainly public.
        "203.0.113.7",
        "8.8.8.8",
        "2001:db8::1",
        "2606:4700::1111",
        // One step outside fc00::/7 and fe80::/10, and the deprecated
        // site-local block that sits just above link-local.
        "fbff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        "fe00::",
        "fe7f:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        "fec0::",
        "ff02::1",
        // IPv4-mapped IPv6. Not vouchable in either direction, which is the
        // fail-closed answer: `Ipv6Addr` reports neither loopback nor private
        // for these, and `ws_server::plaintext_trust` compares them the same
        // way, so the opt-in refusing them keeps the two in step. The way
        // through is to spell the address as IPv4, which the advice already
        // says.
        "::ffff:192.168.1.20",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::ffff:0:0",
    ];

    fn allowing(ws_bind: &str) -> Config {
        let mut config = config_with(Some(ws_bind));
        config.ws_allow_plaintext = true;
        config
    }

    #[test]
    fn adv_the_fail_closed_default_survives_the_existence_of_the_opt_in() {
        // The key is off unless it is written down, including in a config
        // file that predates it — which is every config file in the field.
        assert!(!Config::default().ws_allow_plaintext);
        let upgraded: Config =
            serde_json::from_str(r#"{"ws_bind": "192.168.1.20", "ws_port": 8787}"#).unwrap();
        assert!(!upgraded.ws_allow_plaintext);
        let plan = plan_bind(&upgraded, &[], true);
        assert_eq!(plan.trust, PlaintextTrust::RequireTls);
        assert_eq!(plan.opt_in, PlaintextOptIn::Inert);

        // And with the key off every address the key *could* have covered is
        // still refused, whether or not a tailnet answered behind it.
        for address in VOUCHABLE.iter().chain(NOT_VOUCHABLE) {
            let ip: IpAddr = address.parse().unwrap_or_else(|_| panic!("{address}"));
            if ip.is_loopback() {
                continue;
            }
            for tail in [Vec::new(), tailnet()] {
                let config = config_with(Some(address));
                let plan = plan_bind(&config, &tail, true);
                assert_eq!(plan.trust, PlaintextTrust::RequireTls, "{address}");
                assert_eq!(plan.opt_in, PlaintextOptIn::Inert, "{address}");
                assert!(
                    plaintext_refusal_advice(&config, &plan, false).is_some(),
                    "a listener that refuses everything must say so: {address}"
                );
            }
        }
    }

    #[test]
    fn adv_the_opt_in_stops_at_the_exact_edge_of_every_range_it_names() {
        for address in VOUCHABLE {
            let ip: IpAddr = address.parse().unwrap_or_else(|_| panic!("{address}"));
            assert!(is_local_network(ip), "{address}");
            // Empty tailnet on purpose: the defect F2 fixes is an operator
            // with no Tailscale at all, so the opt-in has to hold there.
            let config = allowing(address);
            let plan = plan_bind(&config, &[], false);
            assert_eq!(
                plan.trust,
                if ip.is_loopback() {
                    // Private before the key was consulted.
                    PlaintextTrust::TrustedPath
                } else {
                    // Served on the operator's instruction, and not private.
                    PlaintextTrust::OperatorAllowed
                },
                "{address}"
            );
            assert_eq!(
                plan.opt_in,
                if ip.is_loopback() {
                    // Trusted before the key was consulted, so it changed
                    // nothing and must not claim to have.
                    PlaintextOptIn::Inert
                } else {
                    PlaintextOptIn::Honoured
                },
                "{address}"
            );
            assert_eq!(
                plaintext_refusal_advice(&config, &plan, false),
                None,
                "a listener that serves has nothing to advise: {address}"
            );

            // With the key off, a local address that is not loopback refuses
            // — and is told about the one key that would change that.
            if !ip.is_loopback() {
                let off = config_with(Some(address));
                let plan = plan_bind(&off, &[], false);
                assert_eq!(plan.trust, PlaintextTrust::RequireTls, "{address}");
                let advice = plaintext_refusal_advice(&off, &plan, false).expect("must be advised");
                assert!(advice.contains("ws_allow_plaintext"), "{address}: {advice}");
            }
        }

        for address in NOT_VOUCHABLE {
            let ip: IpAddr = address.parse().unwrap_or_else(|_| panic!("{address}"));
            assert!(!is_local_network(ip), "{address}");
            // Both tailnet states: an address the probe never saw must not be
            // vouchable either, and the advice differs between them.
            for tail in [Vec::new(), tailnet()] {
                let config = allowing(address);
                let plan = plan_bind(&config, &tail, false);
                assert_eq!(plan.trust, PlaintextTrust::RequireTls, "{address}");
                assert_eq!(plan.opt_in, PlaintextOptIn::Refused, "{address}");
                let advice =
                    plaintext_refusal_advice(&config, &plan, false).expect("must be advised");
                assert!(
                    advice.contains(&plan.bind.to_string()),
                    "{address}: {advice}"
                );
                assert!(
                    !advice.contains("ws_allow_plaintext"),
                    "advice that does not work when followed — the key is already \
                     set and refused on {address}: {advice}"
                );
            }
        }
    }

    #[test]
    fn adv_no_address_outside_the_documented_ranges_is_vouchable() {
        // Exhaustive rather than sampled, which it can be: the IPv4 answer
        // depends on nothing below the second octet and the IPv6 answer on
        // nothing below the first segment, so 65,536 of each is the whole
        // input space of the question. Any mask that is one bit or one octet
        // wrong fails here whether or not anybody thought to name that
        // address.
        for first in 0..=255u8 {
            for second in 0..=255u8 {
                let ip = IpAddr::from([first, second, 0, 1]);
                let documented = first == 127
                    || first == 10
                    || (first == 172 && (16..=31).contains(&second))
                    || (first == 192 && second == 168)
                    || (first == 169 && second == 254);
                assert_eq!(is_local_network(ip), documented, "{ip}");
            }
        }
        for segment in 0..=u16::MAX {
            let ip = IpAddr::from(std::net::Ipv6Addr::new(segment, 0, 0, 0, 0, 0, 0, 1));
            let documented = (0xfc00..=0xfdff).contains(&segment) // fc00::/7
                || (0xfe80..=0xfebf).contains(&segment) // fe80::/10
                || segment == 0; // ::1, the loopback this enumeration ends on
            assert_eq!(is_local_network(ip), documented, "{ip}");
        }
        // And with the host bits zeroed, the same first segment answers the
        // same way — except `::`, which is the wildcard rather than loopback.
        for segment in 0..=u16::MAX {
            let ip = IpAddr::from(std::net::Ipv6Addr::new(segment, 0, 0, 0, 0, 0, 0, 0));
            let documented =
                (0xfc00..=0xfdff).contains(&segment) || (0xfe80..=0xfebf).contains(&segment);
            assert_eq!(is_local_network(ip), documented, "{ip}");
        }
    }

    #[test]
    fn adv_tls_required_wins_at_every_address_the_opt_in_would_have_covered() {
        for address in VOUCHABLE.iter().chain(NOT_VOUCHABLE) {
            let ip: IpAddr = address.parse().unwrap_or_else(|_| panic!("{address}"));
            let mut config = allowing(address);
            config.tls_required = true;
            let plan = plan_bind(&config, &[], false);
            assert_eq!(plan.opt_in, PlaintextOptIn::Inert, "{address}");
            if !ip.is_loopback() {
                assert_eq!(plan.trust, PlaintextTrust::RequireTls, "{address}");
            }
            // Silent: the refusal is what was asked for.
            assert_eq!(
                plaintext_refusal_advice(&config, &plan, false),
                None,
                "{address}"
            );
        }
    }

    #[test]
    fn adv_the_opt_in_is_a_no_op_everywhere_it_was_not_asked_about() {
        // Every start where plaintext was already private, or where no
        // address was named at all, must produce a byte-identical plan with
        // the key on and off. The key may only ever soften one classification
        // and must never reach the bind, the origin or the watcher.
        let v6_only: Vec<IpAddr> = vec!["fd7a:115c:a1e0::1".parse().unwrap()];
        for (bind, tail) in [
            (Some("127.0.0.1"), Vec::new()),
            (Some("127.0.0.1"), tailnet()),
            (Some("::1"), tailnet()),
            (Some("100.64.12.34"), tailnet()),
            (Some("fd7a:115c:a1e0::1"), tailnet()),
            (None, Vec::new()),
            (None, tailnet()),
            (None, v6_only.clone()),
            (Some("mac.example.ts.net"), tailnet()),
            (Some(""), Vec::new()),
        ] {
            for launchd in [false, true] {
                let off = plan_bind(&config_with(bind), &tail, launchd);
                let mut on = config_with(bind);
                on.ws_allow_plaintext = true;
                let on = plan_bind(&on, &tail, launchd);
                assert_eq!(off, on, "{bind:?} {tail:?} launchd={launchd}");
            }
        }
    }

    #[test]
    fn adv_a_cgnat_address_that_is_not_this_nodes_is_never_vouchable() {
        // The trap the opt-in must not fall into: 100.64/10 is the shape of a
        // tailnet address and proves nothing. Only membership of *this
        // node's* addresses is evidence, and that is decided before the key
        // is consulted.
        for address in ["100.64.0.0", "100.64.99.99", "100.127.255.255"] {
            for tail in [Vec::new(), tailnet()] {
                let plan = plan_bind(&allowing(address), &tail, false);
                assert_eq!(plan.trust, PlaintextTrust::RequireTls, "{address}");
                assert_eq!(plan.opt_in, PlaintextOptIn::Refused, "{address}");
            }
        }
        // And this node's own is trusted by the policy, with the key inert.
        let plan = plan_bind(&allowing("100.64.12.34"), &tailnet(), false);
        assert_eq!(plan.trust, PlaintextTrust::TrustedPath);
        assert_eq!(plan.opt_in, PlaintextOptIn::Inert);
    }

    /// Whether the watcher completes for a given probe answer, and how many
    /// times it looked. `expect_return` picks the budget: proving completion
    /// deserves a generous one, proving non-completion needs only enough for
    /// work that takes microseconds when it happens at all.
    async fn watcher_outcome(answer: Vec<IpAddr>, expect_return: bool) -> u32 {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = Arc::clone(&calls);
        let probe = move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::future::ready(answer.clone())
        };
        let budget = if expect_return {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(400)
        };
        let returned = tokio::time::timeout(
            budget,
            watch_tailnet_with(TailnetWatch::Restart, probe, |_| Duration::ZERO, no_report),
        )
        .await
        .is_ok();
        assert_eq!(returned, expect_return);
        calls.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[tokio::test]
    async fn adv_the_watcher_returns_exactly_when_a_restart_would_do_better() {
        // Stated as an equivalence rather than as two examples, because the
        // failure it rules out is a *disagreement*: the watcher exiting on an
        // answer the planner would not bind restarts the daemon straight back
        // into the loopback fallback, which asks for another restart, for
        // ever. One function decides both, and this is what that has to mean.
        let v4: IpAddr = "100.64.12.34".parse().unwrap();
        let v6: IpAddr = "fd7a:115c:a1e0::1".parse().unwrap();
        for answer in [
            vec![],
            vec![v6],
            vec![v6, "fe80::1".parse().unwrap()],
            vec![v4],
            vec![v6, v4],
            vec![v4, v6],
        ] {
            let planner_leaves_loopback =
                plan_bind(&config_with(None), &answer, true).origin != BindOrigin::LoopbackFallback;
            let looks = watcher_outcome(answer.clone(), planner_leaves_loopback).await;
            if planner_leaves_loopback {
                assert_eq!(looks, 1, "{answer:?}: it must stop at the first answer");
            } else {
                // Proof the timeout was not a watcher stuck on its first sleep,
                // and proof of the other half: on an answer that would not
                // improve the bind it keeps looking, without any count at which
                // it settles for being unreachable.
                assert!(
                    looks > 100,
                    "{answer:?}: it stopped looking after {looks} probes"
                );
            }
        }
    }

    #[tokio::test]
    async fn adv_an_idle_watcher_neither_probes_nor_sleeps_nor_returns() {
        // Harder than asking whether it returned: the delay function panics
        // if it is consulted at all, so an idle watcher that merely slept
        // before its early return would fail here rather than time out.
        fn never(_: u32) -> Duration {
            panic!("an idle watcher must not schedule anything");
        }
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = Arc::clone(&calls);
        let probe = move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::future::ready(vec!["100.64.12.34".parse().unwrap()])
        };
        let outcome = tokio::time::timeout(
            Duration::from_millis(400),
            watch_tailnet_with(TailnetWatch::Idle, probe, never, no_report),
        )
        .await;
        assert!(
            outcome.is_err(),
            "an idle watcher returned; every task in the select! ends the \
             process when it returns, so this exits a healthy daemon at startup"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn adv_the_watcher_does_nothing_at_all_on_the_startup_path() {
        // With the real delays, the first thing either watching state does is
        // wait five seconds — so nothing it does can be on the critical path
        // between `main` spawning it and the sockets opening, whatever the
        // probe would have cost.
        for watch in [TailnetWatch::Restart, TailnetWatch::Report] {
            let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let counter = Arc::clone(&calls);
            let probe = move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::future::ready(vec!["100.64.12.34".parse().unwrap()])
            };
            let outcome = tokio::time::timeout(
                Duration::from_millis(50),
                watch_tailnet_with(watch, probe, tailnet_watch_delay, |_| {}),
            )
            .await;
            assert!(outcome.is_err(), "{watch:?}");
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::Relaxed),
                0,
                "{watch:?}: the watcher probed before its first delay, which puts a \
                 `tailscale ip` call on the startup path the probe budget exists \
                 to keep off it"
            );
        }
    }

    /// [`watch_for_tailnet`] is the statement production spawns, and every
    /// watcher test above calls the seam beneath it with a probe, a delay and a
    /// report of its own — so a build that named the wrong three would leave
    /// all of them green. This one runs the production statement itself.
    ///
    /// **What it proves.** That the wiring compiles, runs, and keeps the
    /// contract all three states share: none of them returns. Every task in
    /// `main`'s `select!` ends the process when it returns, so
    /// [`TailnetWatch::Idle`] returning would end a daemon that had found its
    /// address and had nothing to watch for, and either watching state
    /// returning inside its first delay would end one that had not yet run a
    /// single probe — an exit asking for a restart on no evidence at all,
    /// where the whole of what returning buys is the attempt.
    ///
    /// **What it does not prove.** It cannot tell [`tailnet_addresses`] from
    /// any other probe, [`report_tailnet_change`] from any other report, or the
    /// real backoff from a shorter one, because it observes none of the three.
    /// [`watch_for_tailnet`]'s own documentation records what is therefore left
    /// to review, and why reaching it from a test is not cheap.
    ///
    /// The 100ms budget is deliberately far below the five-second
    /// [`TAILNET_WATCH_FIRST_DELAY`], so no arm can reach `tailscale ip` inside
    /// it and the answer is the same on a Mac with Tailscale up and on one
    /// without.
    #[tokio::test]
    async fn the_production_watcher_never_returns_on_the_startup_path() {
        for watch in [
            TailnetWatch::Idle,
            TailnetWatch::Restart,
            TailnetWatch::Report,
        ] {
            let outcome =
                tokio::time::timeout(Duration::from_millis(100), watch_for_tailnet(watch)).await;
            assert!(
                outcome.is_err(),
                "{watch:?}: the production watcher returned; every task in main's \
                 select! ends the process when it returns, so this exits the daemon \
                 at startup"
            );
        }
    }

    #[test]
    fn adv_the_backoff_is_never_a_busy_loop_and_never_overflows() {
        // Every attempt the loop can actually reach, plus far beyond it: no
        // panic on the shift, no zero gap, and a ceiling that holds.
        for attempt in (0..=64u32).chain([1_000, u32::MAX]) {
            let delay = tailnet_watch_delay(attempt);
            assert!(
                delay >= TAILNET_WATCH_FIRST_DELAY,
                "attempt {attempt} would spin"
            );
            assert!(delay <= TAILNET_WATCH_MAX_DELAY, "attempt {attempt}");
            if attempt > 0 {
                assert!(
                    delay >= tailnet_watch_delay(attempt - 1),
                    "attempt {attempt}"
                );
            }
        }
        // It must reach the ceiling early, because the watch lasts as long as
        // the daemon does: the ceiling, not the schedule up to it, is what a
        // Mac that never runs Tailscale pays.
        assert_eq!(tailnet_watch_delay(4), TAILNET_WATCH_MAX_DELAY);
    }

    #[test]
    fn adv_the_watch_is_the_bind_and_the_exit_is_the_launchd_job() {
        // The truth table, stated as the two independent predicates it is made
        // of. *Whether there is anything to watch for* reads only on the bind:
        // no address the operator named and no address the tailnet offered.
        // *Whether ending the process recovers anything* reads only on
        // CodeConnect's launchd job. Answering them as one is what produced a
        // daemon that promised a restart nothing would perform.
        let v6_only: Vec<IpAddr> = vec!["fd7a:115c:a1e0::1".parse().unwrap()];
        for bind in [
            None,
            // Not IP addresses, so nothing was named — the rule reads on the
            // parsed address, not on the presence of the key.
            Some(""),
            Some("mac.example.ts.net"),
            Some("127.0.0.1"),
            Some("::1"),
            Some("192.168.1.20"),
            Some("100.64.12.34"),
            Some("0.0.0.0"),
        ] {
            for tail in [Vec::new(), tailnet(), v6_only.clone()] {
                for launchd in [false, true] {
                    let plan = plan_bind(&config_with(bind), &tail, launchd);
                    let unreachable = explicit_bind_ip(&config_with(bind)).is_none()
                        && preferred_tailnet_ip(&tail).is_none();
                    let expected = match (unreachable, launchd) {
                        (false, _) => TailnetWatch::Idle,
                        (true, true) => TailnetWatch::Restart,
                        (true, false) => TailnetWatch::Report,
                    };
                    assert_eq!(
                        plan.watch_tailnet, expected,
                        "{bind:?} {tail:?} launchd={launchd}"
                    );
                    // Both watching states carry a message about the loopback
                    // fallback, so both must only ever be reached from it.
                    if plan.watch_tailnet != TailnetWatch::Idle {
                        assert_eq!(plan.origin, BindOrigin::LoopbackFallback);
                        assert_eq!(plan.bind, "127.0.0.1:8787".parse().unwrap());
                    }
                }
            }
        }
    }
}
