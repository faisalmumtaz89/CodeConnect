//! `push-relay` — the process.
//!
//! Read the environment, say out loud what was read, open the database, serve
//! until the platform asks for the port back.
//!
//! Two of those steps are less obvious than they look.
//!
//! **The configuration is logged once, in full, at startup.** Every state this
//! service can be misconfigured into is silent otherwise: a topic that is an
//! empty placeholder, a key file the process cannot read, a kill switch left
//! off from an incident three weeks ago. Each of those produces a symptom far
//! from its cause — a `403` from Apple, a push that answers `unavailable`,
//! nothing at all — so the one line that names them is worth more than every
//! line the service prints afterwards.
//!
//! **A missing APNs key is not a startup failure.** The relay has to serve
//! enrollment before the first push is ever possible, and a deployment whose
//! key file is briefly unreadable must report that rather than restart until
//! someone notices. So it starts, says which environment cannot send, and keeps
//! saying it on `/readyz`.

use anyhow::{Context, Result};
use push_core::ApnsEnvironment;
use push_relay::api::{router, Relay};
use push_relay::config::RelayConfig;
use push_relay::{backup, db, logging};

#[tokio::main]
async fn main() -> Result<()> {
    logging::init()?;
    let config = RelayConfig::from_env()?;

    tracing::info!(
        port = config.port,
        db_path = %config.db_path.display(),
        attest_environment = config.attest_environment.as_str(),
        app_id_configured = config.app_id.is_some(),
        min_bundle_version = config.min_bundle_version.as_deref().unwrap_or("none"),
        send_enabled = config.send_enabled,
        enrollment_enabled = config.enrollment_enabled,
        generation_floor_file = %config.generation_floor_file.display(),
        backup_key_file = %config.backup_key_file.display(),
        ip_pepper_file = %config.ip_pepper_file.display(),
        backup_target = config.backup_target,
        backup_retention_days = config.backup_retention_days,
        git_sha = config.git_sha.as_deref().unwrap_or("unknown"),
        "configuration"
    );
    match config.generation_floor() {
        Ok(floor) => tracing::info!(generation_floor = floor, "credential generation floor"),
        // Reported rather than fatal, and reported again by `/readyz`, which
        // refuses readiness for exactly this reason: a floor that cannot be
        // read must never be treated as zero.
        Err(e) => tracing::error!(error = %e, "the credential generation floor is unreadable"),
    }
    for environment in [ApnsEnvironment::Sandbox, ApnsEnvironment::Production] {
        let slot = config.slot(environment);
        match (slot.key(), slot.absence()) {
            (Some(key), _) => tracing::info!(
                environment = environment.as_str(),
                key_id = key.identity.key_id,
                topic = key.identity.topic,
                key_file = %key.key_file.display(),
                "apns key loaded"
            ),
            (None, Some(why)) => tracing::warn!(
                environment = environment.as_str(),
                reason = why,
                "apns key absent; pushes for this environment answer unavailable"
            ),
            (None, None) => unreachable!("a slot is either ready or absent with a reason"),
        }
    }

    let database = db::open(&config.db_path)?;
    tracing::info!(
        schema_version = db::schema_version(&database)?,
        "database ready"
    );

    let address = std::net::SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("binding {address}"))?;
    tracing::info!(%address, "listening");

    let relay = Relay::new(config, database);
    // Says out loud whether backups are on and why not, and takes the first one
    // immediately — an instance that has just been restored or redeployed
    // should have a copy of the state it is actually serving.
    backup::spawn_daily(&relay);

    axum::serve(listener, router(relay))
        .with_graceful_shutdown(shutdown())
        .await
        .context("serving")
}

/// **`SIGTERM`, because that is how the platform stops a container.**
///
/// Without it the process is killed mid-response and the SQLite connection
/// closes without its final checkpoint, which turns every ordinary deploy into
/// a recovery on the next start. `ctrl_c` alongside it so a local run stops the
/// same way.
async fn shutdown() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            // A process that cannot install the handler still has to stop on
            // something, so it waits for the interrupt instead of returning
            // immediately and shutting the server down at startup.
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = terminate => {}
        result = tokio::signal::ctrl_c() => {
            if let Err(e) = result {
                tracing::error!(error = %e, "cannot listen for an interrupt");
            }
        }
    }
    tracing::info!("shutting down");
}
