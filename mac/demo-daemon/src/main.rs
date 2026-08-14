//! A WebSocket server that serves a fictional, scripted CodeConnect fleet.
//!
//! It exists so Apple's App Review can see the product. Every screen but pairing
//! is behind a pairing that needs a Mac, and a reviewer has none — so this
//! stands in for one: five invented agent runs, an approval waiting at each of
//! the three risk classes, and a script that keeps them moving.
//!
//! **Nothing here is real and nothing here is destructive.** No command is run,
//! no file is read or written, no terminal is served and no notification is
//! sent. The whole fleet is in memory and the whole script is embedded in the
//! binary. What *is* real is the wire: every frame is a `protocol` type, so this
//! server cannot drift from `ccd` without failing to compile.
//!
//! Deployed behind a TLS edge that speaks plain `ws` to this process, which is
//! why `capabilities.tls` is false — see `conn::capabilities`.

mod conn;
mod fleet;
mod script;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::fleet::Fleet;

/// How often the script is stepped. Comfortably under the shortest delay any
/// step is authored with, so the granularity is never what a reviewer sees.
const TICK: Duration = Duration::from_millis(250);

/// The port used when the environment names none. The deployment always sets
/// `PORT`; this is for running the server by hand.
const DEFAULT_PORT: u16 = 8080;

/// The largest HTTP request head this server will read before deciding whether
/// it is a WebSocket upgrade. Generous for a handshake, which is well under a
/// kilobyte, and a bound rather than a guess.
const MAX_REQUEST_HEAD_BYTES: usize = 8192;

/// The shortest credential the phone will accept from its pairing field.
///
/// Measured against the client, not invented: `PairingCredentialInput.classify`
/// refuses anything under 16 characters locally, so a shorter token can never be
/// typed in and the reviewer would see a field that simply does nothing. Failing
/// at startup turns that invisible dead end into a sentence in the deploy log.
const MIN_TOKEN_LEN: usize = 16;

#[tokio::main]
async fn main() -> Result<()> {
    let token = std::env::var("REVIEW_TOKEN").unwrap_or_default();
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!(
            "REVIEW_TOKEN is unset or empty; refusing to serve a fleet nothing authenticates for"
        );
    }
    if token.chars().count() < MIN_TOKEN_LEN {
        bail!(
            "REVIEW_TOKEN is shorter than {MIN_TOKEN_LEN} characters, which the app refuses at its \
             pairing field before it dials; use a longer one"
        );
    }
    let port: u16 = match std::env::var("PORT") {
        Ok(value) => value
            .trim()
            .parse()
            .with_context(|| format!("PORT is {value:?}, which is not a port number"))?,
        Err(_) => DEFAULT_PORT,
    };

    let started = Instant::now();
    let fleet = Arc::new(Fleet::new(started)?);
    tokio::spawn({
        let fleet = Arc::clone(&fleet);
        async move {
            let mut ticker = tokio::time::interval(TICK);
            loop {
                ticker.tick().await;
                fleet.tick(Instant::now());
            }
        }
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    println!("demo-daemon listening on ws://{addr}");
    serve(fleet, listener, Arc::new(token)).await
}

/// The accept loop, separated from the bind so a test can hand it a listener on
/// an ephemeral port and speak the real protocol to it.
pub async fn serve(fleet: Arc<Fleet>, listener: TcpListener, token: Arc<String>) -> Result<()> {
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            // A failed accept is this listener's problem, not the fleet's; the
            // loop keeps going rather than taking the whole service down.
            Err(err) => {
                eprintln!("demo-daemon: accept failed: {err}");
                continue;
            }
        };
        let fleet = Arc::clone(&fleet);
        let token = Arc::clone(&token);
        tokio::spawn(async move {
            if let Err(err) = accept(fleet, stream, token).await {
                eprintln!("demo-daemon: connection ended: {err:#}");
            }
        });
    }
}

/// Decide whether this is a health check or a WebSocket, then hand off.
///
/// The platform this runs on health-checks with an ordinary `GET`, and a plain
/// HTTP request is not a WebSocket handshake — tungstenite would fail it and the
/// service would be marked unhealthy and restarted. So the request head is read
/// first and replayed into the handshake, and a request that does not ask to
/// upgrade is answered `200` and closed. The path is not consulted at all: the
/// app strips whatever path is typed at its pairing field, so a server that
/// routed on one could never be reached.
async fn accept(fleet: Arc<Fleet>, mut stream: TcpStream, token: Arc<String>) -> Result<()> {
    // Nagle costs latency on the small JSON frames this protocol is made of.
    let _ = stream.set_nodelay(true);

    let head = match tokio::time::timeout(conn::HANDSHAKE_TIMEOUT, read_request_head(&mut stream))
        .await
        .context("timed out reading the request head")??
    {
        // The peer closed before saying anything: a probe, and nothing to answer.
        Some(head) => head,
        None => return Ok(()),
    };

    if !asks_to_upgrade(&head) {
        let body = "codeconnect demo daemon\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await?;
        stream.flush().await?;
        return Ok(());
    }

    conn::handle(fleet, Replayed::new(head, stream), token).await
}

/// Read up to the end of the HTTP request head.
///
/// Reading rather than peeking: `peek` would avoid the copy but gives no way to
/// wait for *more* than what has already arrived, so a head split across two
/// segments would spin. What is read is handed to the handshake by [`Replayed`],
/// so nothing is consumed from the protocol's point of view.
async fn read_request_head(stream: &mut TcpStream) -> Result<Option<Vec<u8>>> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(if head.is_empty() { None } else { Some(head) });
        }
        head.extend_from_slice(&chunk[..read]);
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(Some(head));
        }
        if head.len() >= MAX_REQUEST_HEAD_BYTES {
            // No terminator inside the bound. Whatever this is, it is not a
            // request this server can answer.
            return Ok(None);
        }
    }
}

/// Does this request head carry `Upgrade: websocket`?
///
/// Parsed by header rather than by searching the whole head for a substring:
/// the value is case-insensitive and may be spelled with or without a space, and
/// a body could otherwise contain the phrase.
fn asks_to_upgrade(head: &[u8]) -> bool {
    String::from_utf8_lossy(head)
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.trim().eq_ignore_ascii_case("upgrade")
                && value.to_ascii_lowercase().contains("websocket")
        })
}

/// The bytes already read from the socket, replayed ahead of it.
struct Replayed {
    head: std::io::Cursor<Vec<u8>>,
    inner: TcpStream,
}

impl Replayed {
    fn new(head: Vec<u8>, inner: TcpStream) -> Replayed {
        Replayed {
            head: std::io::Cursor::new(head),
            inner,
        }
    }
}

impl tokio::io::AsyncRead for Replayed {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let position = self.head.position() as usize;
        let buffered = self.head.get_ref().len();
        if position < buffered {
            let take = (buffered - position).min(buf.remaining());
            let bytes = self.head.get_ref()[position..position + take].to_vec();
            buf.put_slice(&bytes);
            self.head.set_position((position + take) as u64);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for Replayed {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_upgrade_is_recognised_however_it_is_spelled() {
        for head in [
            b"GET / HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n".to_vec(),
            b"GET /anything HTTP/1.1\r\nupgrade:WebSocket\r\n\r\n".to_vec(),
            b"GET / HTTP/1.1\r\nUPGRADE: WEBSOCKET\r\n\r\n".to_vec(),
        ] {
            assert!(asks_to_upgrade(&head), "{}", String::from_utf8_lossy(&head));
        }
    }

    #[test]
    fn a_health_check_is_not_an_upgrade() {
        for head in [
            // What the platform's health check sends, on the path the app would
            // have stripped anyway.
            b"GET / HTTP/1.1\r\nHost: demo\r\nUser-Agent: Render/1.0\r\n\r\n".to_vec(),
            b"GET /healthz HTTP/1.1\r\nHost: demo\r\n\r\n".to_vec(),
            // The phrase in a place that is not the header.
            b"POST / HTTP/1.1\r\nHost: demo\r\nX-Note: websocket\r\n\r\n".to_vec(),
        ] {
            assert!(
                !asks_to_upgrade(&head),
                "{}",
                String::from_utf8_lossy(&head)
            );
        }
    }
}
