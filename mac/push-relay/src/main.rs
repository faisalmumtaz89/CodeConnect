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
use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
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
    // The retention maxima, made maxima: every other sweep in the relay runs
    // only because somebody else made a request.
    db::spawn_sweeps(&relay);

    serve(listener, router(relay), shutdown()).await
}

/// The largest header block the relay will read before refusing the request.
///
/// **Stated here rather than inherited from hyper**, whose defaults are around
/// four hundred kilobytes across a hundred fields. That is parsing this relay
/// does for no request it actually serves: an `Authorization` is `Bearer ` and
/// forty-three base64url characters, an `X-Forwarded-For` is at most
/// forty-five, a content type and a content length are twenty bytes each, and
/// the body behind all of it is capped at a kilobyte. Eight kilobytes is
/// roughly fifty times what arrives, which leaves the tracing and forwarding
/// fields a proxy adds plenty of room and leaves a caller none to make the
/// relay buffer a megabyte before there is a handler to refuse it with.
///
/// **Too low is the failure to watch for.** The refusal is a `431` from the
/// protocol layer, before routing — so it never reaches the request log, and an
/// operator sees a client error with no relay-side trace of the request at all.
/// A proxy that starts adding a few trace-id fields is the realistic way to
/// arrive there, which is why the bound is fifty times the need rather than
/// twice it. Eight kilobytes is also hyper's own floor for this setting; it
/// panics below.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// The most header fields one request may carry.
///
/// **HTTP/1 only.** HTTP/2 has no per-request field count to bind — a header
/// list is one compressed block and the size above is the whole of what an
/// HTTP/2 caller is held to. Thirty-two is well past the dozen a proxy in front
/// of this relay sends and short of the hundred hyper would otherwise allocate
/// for.
const MAX_HEADER_FIELDS: usize = 32;

/// The server this repository configures, rather than the one `axum::serve`
/// would have built with hyper's defaults.
fn http_server() -> auto::Builder<TokioExecutor> {
    let mut server = auto::Builder::new(TokioExecutor::new());
    server
        .http1()
        .max_buf_size(MAX_HEADER_BYTES)
        .max_headers(MAX_HEADER_FIELDS);
    server.http2().max_header_list_size(MAX_HEADER_BYTES as u32);
    server
}

/// Accept until `shutdown` resolves, then let the connections in flight finish.
///
/// **The accept loop is written out because the header bounds have to be**:
/// `axum::serve` builds its own connection builder and offers no way to say
/// what this relay will read, so the builder is constructed here and the loop
/// around it is the price. What it must not lose is the property `SIGTERM`
/// exists for — a response half-written when the platform stops the container
/// means a SQLite connection closed without its final checkpoint, and every
/// ordinary deploy becomes a recovery on the next start. So the signal stops
/// the loop accepting, every live connection is told to finish its request and
/// close, and this returns only once the last of them has.
async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let server = http_server();
    let service = TowerToHyperService::new(router);
    // Cloned into every connection and never held here, so `closed()` below
    // resolves exactly when the last connection has dropped its clone.
    let (closing, closed) = tokio::sync::watch::channel(());
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                // One connection that could not be accepted is not a reason to
                // stop serving the rest: a momentary descriptor limit would
                // otherwise end the process instead of the connection.
                Err(e) => {
                    tracing::warn!(error = %e, "a connection could not be accepted");
                    continue;
                }
            },
            () = shutdown.as_mut() => break,
        };

        let server = server.clone();
        let service = service.clone();
        let mut closing = closed.clone();
        tokio::spawn(async move {
            let connection = server.serve_connection(TokioIo::new(stream), service);
            let mut connection = std::pin::pin!(connection);
            tokio::select! {
                // Debug rather than warn: a client that hangs up mid-request is
                // ordinary, and at warn it would be the loudest thing in the log.
                result = connection.as_mut() => {
                    if let Err(e) = result {
                        tracing::debug!(error = %e, "a connection ended");
                    }
                }
                _ = closing.changed() => {
                    connection.as_mut().graceful_shutdown();
                    let _ = connection.await;
                }
            }
        });
    }

    drop(closed);
    let _ = closing.send(());
    closing.closed().await;
    Ok(())
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

/// **These bind a port, which the rest of the suite deliberately does not.**
/// The bound under test is enforced by the HTTP parser and not by the router,
/// so calling the router as a `tower::Service` would exercise nothing: the
/// refusal has to be shown arriving over a socket, before any handler exists to
/// see the request.
#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// A route that counts the requests that reached it, which is how "refused
    /// at the protocol layer" is told apart from "answered by a handler".
    fn probe() -> (Router, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&hits);
        let router = Router::new().route(
            "/healthz",
            axum::routing::get(move || {
                let counted = Arc::clone(&counted);
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    "ok\n"
                }
            }),
        );
        (router, hits)
    }

    async fn listening(router: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, router, std::future::pending::<()>()));
        address
    }

    /// Send bytes and read whatever comes back until the server closes.
    ///
    /// The write is allowed to fail: a request over the bound is refused while
    /// it is still being sent, so the server may have answered and hung up
    /// before the last of it is written.
    async fn speak(address: SocketAddr, request: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let _ = stream.write_all(request.as_bytes()).await;
        let _ = stream.flush().await;
        let mut answer = Vec::new();
        let _ =
            tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut answer)).await;
        String::from_utf8_lossy(&answer).into_owned()
    }

    #[tokio::test]
    async fn a_header_block_over_the_bound_never_reaches_a_handler() {
        let (router, hits) = probe();
        let address = listening(router).await;

        let answer = speak(
            address,
            &format!(
                "GET /healthz HTTP/1.1\r\nHost: relay\r\nX-Padding: {}\r\n\r\n",
                "p".repeat(MAX_HEADER_BYTES * 2)
            ),
        )
        .await;

        assert!(
            answer.starts_with("HTTP/1.1 431 "),
            "the parser must refuse it, not the router: {answer:?}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// The count bound, which is the half a caller can spend without ever
    /// exceeding the size bound: thousands of tiny fields are cheap to send and
    /// were an allocation the relay made per request.
    #[tokio::test]
    async fn too_many_header_fields_are_refused_although_they_are_small() {
        let (router, hits) = probe();
        let address = listening(router).await;

        let mut request = String::from("GET /healthz HTTP/1.1\r\nHost: relay\r\n");
        for index in 0..MAX_HEADER_FIELDS * 2 {
            request.push_str(&format!("X-Trace-{index}: 1\r\n"));
        }
        request.push_str("\r\n");
        assert!(request.len() < MAX_HEADER_BYTES, "{}", request.len());

        let answer = speak(address, &request).await;
        assert!(answer.starts_with("HTTP/1.1 431 "), "{answer:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// **What the relay actually receives still goes through**, which is the
    /// assertion that makes the two above a bound rather than an outage.
    #[tokio::test]
    async fn a_request_carrying_a_real_bearer_and_a_forwarded_address_is_served() {
        let (router, hits) = probe();
        let address = listening(router).await;

        let answer = speak(
            address,
            &format!(
                "GET /healthz HTTP/1.1\r\nHost: relay\r\nAuthorization: Bearer {}\r\n\
                 X-Forwarded-For: 203.0.113.7\r\nContent-Type: application/json\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n",
                "A".repeat(43)
            ),
        )
        .await;

        assert!(answer.starts_with("HTTP/1.1 200 "), "{answer:?}");
        assert!(answer.ends_with("ok\n"), "{answer:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// **The HTTP/2 half is a number the server states on the wire**, so it is
    /// read off the wire. There is no field-count equivalent to assert: HTTP/2
    /// carries one compressed header block and [`MAX_HEADER_BYTES`] is the whole
    /// of what an HTTP/2 caller is held to.
    #[tokio::test]
    async fn the_http2_bound_is_advertised_before_a_request_is_taken() {
        /// The connection preface every HTTP/2 client sends first, which is what
        /// the connection builder detects the version by.
        const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        /// Three length bytes, the frame type, no flags, the connection stream.
        const EMPTY_SETTINGS: &[u8] = &[0, 0, 0, 0x4, 0, 0, 0, 0, 0];
        const SETTINGS: u8 = 0x4;
        const MAX_HEADER_LIST_SIZE: u16 = 0x6;

        let (router, hits) = probe();
        let address = listening(router).await;
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(PREFACE).await.unwrap();
        stream.write_all(EMPTY_SETTINGS).await.unwrap();
        stream.flush().await.unwrap();

        let advertised = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut head = [0u8; 9];
                stream.read_exact(&mut head).await.unwrap();
                let length = (usize::from(head[0]) << 16)
                    | (usize::from(head[1]) << 8)
                    | usize::from(head[2]);
                let mut payload = vec![0u8; length];
                stream.read_exact(&mut payload).await.unwrap();
                if head[3] != SETTINGS {
                    continue;
                }
                for entry in payload.chunks_exact(6) {
                    let id = u16::from_be_bytes([entry[0], entry[1]]);
                    let value = u32::from_be_bytes([entry[2], entry[3], entry[4], entry[5]]);
                    if id == MAX_HEADER_LIST_SIZE {
                        return value;
                    }
                }
            }
        })
        .await
        .expect("the server states its header bound before it takes a request");

        assert_eq!(advertised, MAX_HEADER_BYTES as u32);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// **The property `SIGTERM` exists for**: the request in flight is answered
    /// and only then does the server return, because a SQLite connection closed
    /// under a half-written response is a recovery on the next start.
    #[tokio::test]
    async fn shutdown_waits_for_the_request_it_is_in_the_middle_of() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();

        let held = Arc::new(Mutex::new(Some(released)));
        let router = Router::new().route(
            "/healthz",
            axum::routing::get(move || {
                let held = Arc::clone(&held);
                async move {
                    let released = held.lock().unwrap().take().expect("one request");
                    let _ = released.await;
                    "ok\n"
                }
            }),
        );
        let server = tokio::spawn(serve(listener, router, async {
            let _ = stopped.await;
        }));

        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        stream.flush().await.unwrap();
        // The handler is inside the route and waiting, so the shutdown below
        // arrives with a response still owed.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let _ = stop.send(());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!server.is_finished(), "it returned owing a response");

        let _ = release.send(());
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut answer))
            .await
            .expect("the answer must arrive")
            .unwrap();
        assert!(
            String::from_utf8_lossy(&answer).starts_with("HTTP/1.1 200 "),
            "{:?}",
            String::from_utf8_lossy(&answer)
        );

        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("and only then does the server return")
            .unwrap()
            .unwrap();
    }
}
