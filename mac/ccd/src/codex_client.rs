//! The client for a Codex app-server: JSON-RPC over a WebSocket on a unix socket, one
//! connection per run (P3 R1).
//!
//! [`dial`] is the one WebSocket dial ccd makes to a Codex socket. A [`Client`] holds one
//! connection. Its calls carry ids counting from 1, and its reader task hands each response
//! to the call with that id. A frame with a `method` is
//! the server's own, never a response, so an id the server puts on its request never takes a
//! call's answer. Each such frame, a notification (no `id`) or a request (an `id`), is kept
//! in order for the client's owner ([`Client::server_frame`]), who answers a request with
//! [`Client::respond`] or [`Client::refuse`]. Dropping the client stops the reader, which
//! closes the socket. When the connection ends, every call still open gets
//! [`CallError::Closed`].

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::WebSocketStream;

/// How long a dial of a Codex socket may take before it counts as a failed attempt.
const CONNECT_BUDGET: Duration = Duration::from_secs(10);

/// How long the WebSocket upgrade may take once the socket is connected. Separate
/// from the dial: `connect()` returning only means something accepted, and a peer
/// that accepts and then never speaks would otherwise hold the caller for ever.
const UPGRADE_BUDGET: Duration = Duration::from_secs(15);

/// Byte-based WebSocket bounds. A single notification reaches multi-MB
/// (a `plugin/list` reply measures 10.4 MB on codex 0.153.4), so the ceiling is on
/// bytes and generous enough that a legitimate large frame is never rejected.
pub(crate) fn ws_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(64 << 20),
        max_frame_size: Some(64 << 20),
        ..Default::default()
    }
}

/// A WebSocket on a unix socket, as [`dial`] opens it.
pub(crate) type Socket = WebSocketStream<UnixStream>;

/// Connect to the unix socket at `path` and upgrade it to a WebSocket, each step within
/// its own budget. A socket nothing listens on yet fails at once, with the system's
/// error (`NotFound`, `ConnectionRefused`) under the context.
pub(crate) async fn dial(path: &Path) -> Result<Socket> {
    let stream = tokio::time::timeout(CONNECT_BUDGET, UnixStream::connect(path))
        .await
        .with_context(|| format!("dialling {} timed out", path.display()))?
        .with_context(|| format!("dialling {}", path.display()))?;
    let (ws, _response) = tokio::time::timeout(
        UPGRADE_BUDGET,
        tokio_tungstenite::client_async_with_config("ws://localhost/", stream, Some(ws_config())),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "the WebSocket upgrade on {} stalled for {UPGRADE_BUDGET:?}",
            path.display()
        )
    })?
    .with_context(|| format!("{} refused the WebSocket handshake", path.display()))?;
    Ok(ws)
}

/// Why a call has no result.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CallError {
    /// The server answered the call with an error.
    Rpc { code: i64, message: String },
    /// The connection ended before the answer came.
    Closed,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Rpc { code, message } => write!(f, "{message} ({code})"),
            CallError::Closed => f.write_str("the connection ended"),
        }
    }
}

/// The calls waiting for their answers, by id; `None` once the connection has ended.
type OpenCalls = Arc<Mutex<Option<HashMap<i64, oneshot::Sender<Result<Value, CallError>>>>>>;

/// One connection to an app-server.
pub(crate) struct Client {
    sink: tokio::sync::Mutex<SplitSink<Socket, Message>>,
    open: OpenCalls,
    next_id: AtomicI64,
    reader: tokio::task::JoinHandle<()>,
    server_frames: mpsc::UnboundedReceiver<Value>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// [`dial`] the app-server at `path`, and a client on the connection.
pub(crate) async fn connect(path: &Path) -> Result<Client> {
    Ok(Client::over(dial(path).await?))
}

impl Client {
    fn over(socket: Socket) -> Client {
        let (sink, stream) = socket.split();
        let open: OpenCalls = Arc::new(Mutex::new(Some(HashMap::new())));
        let (forwarded, server_frames) = mpsc::unbounded_channel();
        Client {
            sink: tokio::sync::Mutex::new(sink),
            reader: tokio::spawn(read(stream, Arc::clone(&open), forwarded)),
            open,
            next_id: AtomicI64::new(1),
            server_frames,
        }
    }

    /// The server's next notification or request, in the order it was sent, from the
    /// connection's first; `None` once the connection has ended and every one has been taken.
    pub(crate) async fn server_frame(&mut self) -> Option<Value> {
        self.server_frames.recv().await
    }

    /// Answer the server's request `id` with `result`.
    pub(crate) async fn respond(&self, id: &Value, result: Value) -> Result<(), CallError> {
        self.send(json!({"id": id, "result": result})).await
    }

    /// Answer the server's request `id` with a JSON-RPC error.
    pub(crate) async fn refuse(
        &self,
        id: &Value,
        code: i64,
        message: &str,
    ) -> Result<(), CallError> {
        self.send(json!({"id": id, "error": {"code": code, "message": message}}))
            .await
    }

    /// The handshake: `initialize` as CodeConnect's daemon, with the experimental API
    /// on, then the `initialized` notification. The server's answer is the result.
    pub(crate) async fn initialize(&self) -> Result<Value, CallError> {
        let params = json!({
            "clientInfo": {
                "name": "codeconnect-ccd",
                "title": "CodeConnect daemon",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {"experimentalApi": true},
        });
        let answer = self.call("initialize", params).await?;
        self.send(json!({"method": "initialized"})).await?;
        Ok(answer)
    }

    /// Call `method` and wait for its answer. No deadline of its own: the caller bounds it.
    pub(crate) async fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        match self
            .open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
        {
            Some(open) => open.insert(id, answer),
            None => return Err(CallError::Closed),
        };
        self.send(json!({"id": id, "method": method, "params": params}))
            .await?;
        answered.await.unwrap_or(Err(CallError::Closed))
    }

    async fn send(&self, frame: Value) -> Result<(), CallError> {
        self.sink
            .lock()
            .await
            .send(Message::Text(frame.to_string()))
            .await
            .map_err(|_| CallError::Closed)
    }
}

/// The connection's reader: each response to its call, and each notification or request of
/// the server's to `forwarded`, until the connection ends; then every call still open is told
/// it is closed.
async fn read(
    mut stream: SplitStream<Socket>,
    open: OpenCalls,
    forwarded: mpsc::UnboundedSender<Value>,
) {
    while let Some(Ok(message)) = stream.next().await {
        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(Value::Object(mut frame)) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if frame.contains_key("method") {
            let _ = forwarded.send(Value::Object(frame));
            continue;
        }
        let Some(id) = frame.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let answer = match (frame.remove("result"), frame.remove("error")) {
            (Some(result), _) => Ok(result),
            (None, Some(error)) => Err(CallError::Rpc {
                code: error["code"].as_i64().unwrap_or_default(),
                message: error["message"].as_str().unwrap_or_default().to_string(),
            }),
            (None, None) => continue,
        };
        let waiting = open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
            .and_then(|open| open.remove(&id));
        if let Some(waiting) = waiting {
            let _ = waiting.send(answer);
        }
    }
    *open.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client and the server's end of its connection: a real WebSocket on a socket pair.
    async fn pair() -> (Client, Socket) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (client, server) = tokio::join!(
            tokio_tungstenite::client_async_with_config("ws://localhost/", ours, Some(ws_config())),
            tokio_tungstenite::accept_async_with_config(theirs, Some(ws_config())),
        );
        (Client::over(client.unwrap().0), server.unwrap())
    }

    /// The next frame the client sent, as JSON.
    async fn sent(server: &mut Socket) -> Value {
        loop {
            match server.next().await.unwrap().unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                _ => continue,
            }
        }
    }

    async fn say(server: &mut Socket, frame: Value) {
        server.send(Message::Text(frame.to_string())).await.unwrap();
    }

    const SOON: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn each_call_gets_the_answer_to_its_own_id() {
        let (client, mut server) = pair().await;
        let calls = async {
            tokio::join!(
                client.call("first", json!({})),
                client.call("second", json!({}))
            )
        };
        let server_side = async {
            let (one, two) = (sent(&mut server).await, sent(&mut server).await);
            let id_of = |method: &str| {
                [&one, &two]
                    .into_iter()
                    .find(|f| f["method"] == method)
                    .unwrap()["id"]
                    .clone()
            };
            say(&mut server, json!({"id": id_of("second"), "result": "B"})).await;
            say(&mut server, json!({"id": id_of("first"), "result": "A"})).await;
        };
        let ((first, second), ()) = tokio::join!(calls, server_side);
        assert_eq!(first, Ok(json!("A")));
        assert_eq!(second, Ok(json!("B")));
    }

    #[tokio::test]
    async fn an_error_answer_is_the_calls_error() {
        let (client, mut server) = pair().await;
        let (answer, ()) = tokio::join!(client.call("thread/resume", json!({})), async {
            let id = sent(&mut server).await["id"].clone();
            say(
                &mut server,
                json!({"id": id, "error": {"code": -32600, "message": "no rollout found"}}),
            )
            .await;
        });
        assert_eq!(
            answer,
            Err(CallError::Rpc {
                code: -32600,
                message: "no rollout found".into()
            })
        );
    }

    #[tokio::test]
    async fn the_servers_own_frames_and_unknown_ids_take_no_answer() {
        let (client, mut server) = pair().await;
        let (answer, ()) = tokio::join!(client.call("initialize", json!({})), async {
            let id = sent(&mut server).await["id"].clone();
            // A request of the server's under the call's own id, a notification, an answer
            // for an id never called, and a line that is not JSON: none is the answer.
            say(
                &mut server,
                json!({"id": id, "method": "item/commandExecution/requestApproval", "params": {}}),
            )
            .await;
            say(
                &mut server,
                json!({"method": "thread/started", "params": {}}),
            )
            .await;
            say(&mut server, json!({"id": 99, "result": "not yours"})).await;
            server.send(Message::Text("not json".into())).await.unwrap();
            say(&mut server, json!({"id": id, "result": "yours"})).await;
        });
        assert_eq!(answer, Ok(json!("yours")));
        let (again, ()) = tokio::join!(client.call("model/list", json!({})), async {
            let id = sent(&mut server).await["id"].clone();
            say(&mut server, json!({"id": id, "result": "again"})).await;
        });
        assert_eq!(again, Ok(json!("again")));
    }

    #[tokio::test]
    async fn the_servers_notifications_and_requests_reach_the_owner_in_order() {
        let (mut client, mut server) = pair().await;
        let request =
            json!({"id": 7, "method": "item/commandExecution/requestApproval", "params": {}});
        say(
            &mut server,
            json!({"method": "thread/started", "params": {"n": 1}}),
        )
        .await;
        say(&mut server, request.clone()).await;
        say(
            &mut server,
            json!({"method": "turn/completed", "params": {"n": 2}}),
        )
        .await;
        server.close(None).await.unwrap();
        assert_eq!(
            client.server_frame().await,
            Some(json!({"method": "thread/started", "params": {"n": 1}}))
        );
        assert_eq!(client.server_frame().await, Some(request));
        assert_eq!(
            client.server_frame().await,
            Some(json!({"method": "turn/completed", "params": {"n": 2}}))
        );
        assert_eq!(client.server_frame().await, None);
    }

    #[tokio::test]
    async fn a_request_is_answered_or_refused_under_its_own_id() {
        let (client, mut server) = pair().await;
        client
            .respond(&json!(3), json!({"decision": "accept"}))
            .await
            .unwrap();
        assert_eq!(
            sent(&mut server).await,
            json!({"id": 3, "result": {"decision": "accept"}})
        );
        client
            .refuse(&json!(4), -32601, "not offered by CodeConnect")
            .await
            .unwrap();
        assert_eq!(
            sent(&mut server).await,
            json!({"id": 4, "error": {"code": -32601, "message": "not offered by CodeConnect"}})
        );
    }

    #[tokio::test]
    async fn a_call_open_when_the_server_closes_is_closed() {
        let (client, mut server) = pair().await;
        let (answer, ()) = tokio::join!(
            tokio::time::timeout(SOON, client.call("initialize", json!({}))),
            async {
                sent(&mut server).await;
                server.close(None).await.unwrap();
            }
        );
        assert_eq!(
            answer.expect("the call ends within 1 s"),
            Err(CallError::Closed)
        );
        assert_eq!(
            client.call("model/list", json!({})).await,
            Err(CallError::Closed)
        );
    }

    #[tokio::test]
    async fn dropping_the_client_closes_the_socket() {
        let (client, mut server) = pair().await;
        drop(client);
        let ended = tokio::time::timeout(SOON, async {
            loop {
                match server.next().await {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                    Some(Ok(_)) => continue,
                }
            }
        })
        .await;
        assert!(
            ended.is_ok(),
            "the server still had the socket open after 1 s"
        );
    }

    #[tokio::test]
    async fn initialize_says_who_it_is_and_then_initialized() {
        let (client, mut server) = pair().await;
        let (answer, (asked, then)) = tokio::join!(client.initialize(), async {
            let asked = sent(&mut server).await;
            say(
                &mut server,
                json!({"id": asked["id"], "result": {"userAgent": "codex"}}),
            )
            .await;
            (asked.clone(), sent(&mut server).await)
        });
        assert_eq!(answer, Ok(json!({"userAgent": "codex"})));
        assert_eq!(asked["method"], "initialize");
        assert_eq!(asked["params"]["clientInfo"]["name"], "codeconnect-ccd");
        assert_eq!(asked["params"]["capabilities"]["experimentalApi"], true);
        assert_eq!(then, json!({"method": "initialized"}));
    }

    #[tokio::test]
    async fn a_dial_where_nothing_listens_fails_with_the_systems_error() {
        let err = dial(Path::new("/nonexistent/codeconnect/as.sock"))
            .await
            .err()
            .unwrap();
        assert_eq!(
            err.downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::NotFound)
        );
    }
}
