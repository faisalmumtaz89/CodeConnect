//! Integration tests for the relay transport against a fake upstream (no live infra).
//!
//! The fake [`FakeFactory`] records every client→server message the broker **admits**
//! (forwards) and can script server→client frames. Because refused messages never reach
//! the fake, "zero upstream bytes" is directly observable: the forbidden method is simply
//! absent from `recorded`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::WebSocketStream;

use codex_broker::relay::Broker;
use codex_broker::upstream::{ConnectFuture, UpstreamChannels, UpstreamFactory};
use codex_broker::LaunchFingerprint;

// ---------------------------------------------------------------------------
// Fake upstream + broker harness
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeState {
    recorded: Mutex<Vec<String>>,
    scripted: Mutex<Vec<Message>>,
}

#[derive(Clone)]
struct FakeFactory {
    inner: Arc<FakeState>,
}

impl UpstreamFactory for FakeFactory {
    fn connect(&self) -> ConnectFuture {
        let recorded = Arc::clone(&self.inner);
        let scripted: Vec<Message> = std::mem::take(&mut *self.inner.scripted.lock().unwrap());
        Box::pin(async move {
            let (to_tx, mut to_rx) = tokio::sync::mpsc::channel::<Message>(64);
            let (from_tx, from_rx) = tokio::sync::mpsc::channel::<Message>(64);
            tokio::spawn(async move {
                for m in scripted {
                    if from_tx.send(m).await.is_err() {
                        return;
                    }
                }
                // Hold from_tx open by keeping it in scope while draining client traffic.
                while let Some(m) = to_rx.recv().await {
                    if let Message::Text(t) = m {
                        recorded.recorded.lock().unwrap().push(t);
                    }
                }
                drop(from_tx);
            });
            Ok(UpstreamChannels {
                to_upstream: to_tx,
                from_upstream: from_rx,
            })
        })
    }
}

fn fingerprint() -> LaunchFingerprint {
    LaunchFingerprint {
        approval_policy: "untrusted".into(),
        approvals_reviewer: "user".into(),
        sandbox: "read-only".into(),
        hooks_enabled: true,
    }
}

struct Harness {
    tui_sock: String,
    ccd_sock: String,
    state: Arc<FakeState>,
}

fn start_broker() -> Harness {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    // Short paths: SUN_LEN caps a UDS path at 103 bytes (A1/D7), so /tmp, not the long
    // scratchpad path.
    let tui_sock = format!("/tmp/ccb-{pid}-{n}-t.sock");
    let ccd_sock = format!("/tmp/ccb-{pid}-{n}-c.sock");
    let _ = std::fs::remove_file(&tui_sock);
    let _ = std::fs::remove_file(&ccd_sock);

    let state = Arc::new(FakeState::default());
    let factory = FakeFactory {
        inner: Arc::clone(&state),
    };
    let broker = Broker::new(tui_sock.clone(), ccd_sock.clone(), fingerprint(), factory);
    tokio::spawn(async move {
        let _ = broker.serve().await;
    });
    Harness {
        tui_sock,
        ccd_sock,
        state,
    }
}

async fn connect(path: &str) -> WebSocketStream<UnixStream> {
    for _ in 0..100 {
        if let Ok(stream) = UnixStream::connect(path).await {
            if let Ok((ws, _)) = tokio_tungstenite::client_async("ws://localhost/", stream).await {
                return ws;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("could not connect to broker at {path}");
}

async fn recorded_after(state: &Arc<FakeState>, want: usize) -> Vec<String> {
    for _ in 0..200 {
        {
            let g = state.recorded.lock().unwrap();
            if g.len() >= want {
                return g.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    state.recorded.lock().unwrap().clone()
}

/// Give the broker a beat to process a message that it must NOT forward.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(60)).await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn allowlisted_forwards_bypass_is_zero_bytes_with_synthetic_error() {
    let h = start_broker();
    let mut ws = connect(&h.tui_sock).await;

    ws.send(Message::Text(
        r#"{"method":"app/list","id":1,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        r#"{"method":"command/exec","id":2,"params":{"cmd":"rm -rf /"}}"#.into(),
    ))
    .await
    .unwrap();

    // The refused bypass produces exactly one frame back to the client: a synthetic error.
    let reply = ws.next().await.unwrap().unwrap();
    let v: serde_json::Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
    assert_eq!(v["id"], 2);
    assert_eq!(v["error"]["code"], -32001);

    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert_eq!(rec.len(), 1, "only the allowlisted app/list forwards");
    assert!(rec[0].contains("app/list"));
    assert!(
        !rec.iter().any(|m| m.contains("command/exec")),
        "command/exec must reach zero upstream bytes",
    );
}

#[tokio::test]
async fn ownership_conflict_and_ccd_role_refused_zero_bytes() {
    let h = start_broker();

    // TUI: thread/start with a conflicting approvalPolicy -> policy error.
    let mut tui = connect(&h.tui_sock).await;
    tui.send(Message::Text(
        r#"{"method":"thread/start","id":"a","params":{"approvalPolicy":"never","approvalsReviewer":"user","sandbox":"read-only"}}"#.into(),
    ))
    .await
    .unwrap();
    let r1 = tui.next().await.unwrap().unwrap();
    let v1: serde_json::Value = serde_json::from_str(r1.to_text().unwrap()).unwrap();
    assert_eq!(v1["id"], "a");
    assert_eq!(v1["error"]["code"], -32001);

    // ccd: thread/start is role-refused (attach-only), even fingerprint-clean.
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(
        r#"{"method":"thread/start","id":7,"params":{"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":"read-only"}}"#.into(),
    ))
    .await
    .unwrap();
    let r2 = ccd.next().await.unwrap().unwrap();
    let v2: serde_json::Value = serde_json::from_str(r2.to_text().unwrap()).unwrap();
    assert_eq!(v2["id"], 7);
    assert_eq!(v2["error"]["code"], -32001);

    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "no refused ownership request forwards on either leg",
    );
}

#[tokio::test]
async fn ownership_matching_thread_start_forwards() {
    let h = start_broker();
    let mut ws = connect(&h.tui_sock).await;
    ws.send(Message::Text(
        r#"{"method":"thread/start","id":3,"params":{"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":"read-only","config":{"model_reasoning_effort":"high"}}}"#.into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1);
    assert!(rec[0].contains("thread/start"));
}

#[tokio::test]
async fn turn_start_fails_closed_until_headcheck() {
    let h = start_broker();
    let mut ws = connect(&h.tui_sock).await;
    ws.send(Message::Text(
        r#"{"method":"turn/start","id":3,"params":{"approvalPolicy":"untrusted","approvalsReviewer":"user","sandboxPolicy":"read-only"}}"#.into(),
    ))
    .await
    .unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    let v: serde_json::Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
    assert_eq!(
        v["error"]["code"], -32601,
        "turn/start needs the deferred head-check"
    );
    settle().await;
    assert!(h.state.recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ccd_resume_bound_to_session_thread() {
    let h = start_broker();
    // Script a thread/started so the broker observes and binds the thread id on the ccd
    // leg's s2c stream before the resume arrives.
    h.state.scripted.lock().unwrap().push(Message::Text(
        r#"{"method":"thread/started","params":{"thread":{"id":"01a0-ours","path":"/x"}}}"#.into(),
    ));
    let mut ccd = connect(&h.ccd_sock).await;
    // Drain the observed thread/started (guarantees binding happened).
    let started = ccd.next().await.unwrap().unwrap();
    assert!(started.to_text().unwrap().contains("thread/started"));

    // Resume of an UNKNOWN thread -> refused, zero bytes.
    ccd.send(Message::Text(
        r#"{"method":"thread/resume","id":1,"params":{"threadId":"99-not-ours"}}"#.into(),
    ))
    .await
    .unwrap();
    let bad = ccd.next().await.unwrap().unwrap();
    let bv: serde_json::Value = serde_json::from_str(bad.to_text().unwrap()).unwrap();
    assert_eq!(bv["id"], 1);
    assert_eq!(bv["error"]["code"], -32001);

    // Resume of the SESSION thread -> forwarded.
    ccd.send(Message::Text(
        r#"{"method":"thread/resume","id":2,"params":{"threadId":"01a0-ours"}}"#.into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1);
    assert!(rec[0].contains("01a0-ours"));
}

#[tokio::test]
async fn s2c_passthrough_is_byte_exact() {
    let h = start_broker();
    // Script a server->client frame BEFORE connecting (connect drains the script).
    let payload = format!(
        r#"{{"method":"app/list/updated","params":{{"pad":"{}"}}}}"#,
        "Z".repeat(300_000)
    );
    h.state
        .scripted
        .lock()
        .unwrap()
        .push(Message::Text(payload.clone()));

    let mut ws = connect(&h.tui_sock).await;
    let got = ws.next().await.unwrap().unwrap();
    assert_eq!(got.into_text().unwrap(), payload, "s2c must be byte-exact");
}

#[tokio::test]
async fn multi_mb_message_relays_whole_and_exact() {
    let h = start_broker();
    let mut ws = connect(&h.tui_sock).await;
    // ~6 MB allowlisted message (the plugin/list worst case class from A4/D8).
    let big = "A".repeat(6 * 1024 * 1024);
    let msg = format!(r#"{{"method":"app/list","id":1,"params":{{"pad":"{big}"}}}}"#);
    ws.send(Message::Text(msg.clone())).await.unwrap();

    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].len(), msg.len(), "multi-MB message relayed whole");
    assert_eq!(rec[0], msg, "and byte-exact");
}

#[tokio::test]
async fn duplicate_key_frame_forwards_zero_bytes_and_closes() {
    let h = start_broker();
    let mut ws = connect(&h.tui_sock).await;
    // A frame with a duplicated typed ownership key: our parse keeps one, the app-server
    // might keep the other. Reject: zero upstream bytes, leg closed.
    ws.send(Message::Text(
        r#"{"method":"thread/start","id":1,"approvalPolicy":"untrusted","approvalPolicy":"never"}"#
            .into(),
    ))
    .await
    .unwrap();
    // The leg closes (malformed/hostile), and nothing is forwarded.
    let closed = loop {
        match ws.next().await {
            None => break true,
            Some(Ok(Message::Close(_))) | Some(Err(_)) => break true,
            Some(Ok(_)) => continue,
        }
    };
    assert!(closed, "duplicate-key frame must close the leg");
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "duplicate-key frame must forward zero bytes",
    );
}

#[tokio::test]
async fn reinitialization_closes_the_leg() {
    let h = start_broker();
    let mut ws = connect(&h.tui_sock).await;
    ws.send(Message::Text(
        r#"{"method":"initialize","id":1,"params":{"capabilities":{"experimentalApi":true}}}"#
            .into(),
    ))
    .await
    .unwrap();
    // First initialize forwards.
    let rec = recorded_after(&h.state, 1).await;
    assert!(rec[0].contains("initialize"));

    // Second initialize on the same connection -> fail closed (leg closes).
    ws.send(Message::Text(
        r#"{"method":"initialize","id":2,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    // The stream must end (close/EOF) rather than deliver a normal message.
    let closed = loop {
        match ws.next().await {
            None => break true,
            Some(Ok(Message::Close(_))) => break true,
            Some(Err(_)) => break true,
            Some(Ok(_)) => continue,
        }
    };
    assert!(closed, "reinitialization must close the leg");
}

// ---------------------------------------------------------------------------
// Raw fragmented-frame client (proves whole-message reassembly before forwarding)
// ---------------------------------------------------------------------------

async fn raw_handshake(stream: &mut UnixStream) {
    let req = "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await.unwrap();
        assert!(n > 0, "server closed during handshake");
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    assert!(head.contains("101"), "handshake not upgraded: {head}");
}

/// Write one masked WebSocket data frame. `fin` marks the final fragment; `opcode` is
/// 0x1 (text) for the first fragment and 0x0 (continuation) for the rest.
async fn write_masked_frame(stream: &mut UnixStream, fin: bool, opcode: u8, payload: &[u8]) {
    let mut hdr = Vec::new();
    hdr.push((if fin { 0x80 } else { 0x00 }) | (opcode & 0x0f));
    let mask_bit = 0x80u8;
    let len = payload.len();
    if len < 126 {
        hdr.push(mask_bit | len as u8);
    } else if len < 65536 {
        hdr.push(mask_bit | 126);
        hdr.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        hdr.push(mask_bit | 127);
        hdr.extend_from_slice(&(len as u64).to_be_bytes());
    }
    let key = [0x12u8, 0x34, 0x56, 0x78];
    hdr.extend_from_slice(&key);
    let masked: Vec<u8> = payload
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ key[i % 4])
        .collect();
    stream.write_all(&hdr).await.unwrap();
    stream.write_all(&masked).await.unwrap();
    stream.flush().await.unwrap();
}

#[tokio::test]
async fn fragmented_message_reassembled_into_one_forward() {
    let h = start_broker();
    let mut stream = loop {
        if let Ok(s) = UnixStream::connect(&h.tui_sock).await {
            break s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    raw_handshake(&mut stream).await;

    // One allowlisted app/list request, split across three fragments. No byte may be
    // forwarded until the FINAL fragment completes the whole message.
    let whole = r#"{"method":"app/list","id":42,"params":{"k":"vvvvv"}}"#;
    let (a, rest) = whole.split_at(20);
    let (b, c) = rest.split_at(15);
    write_masked_frame(&mut stream, false, 0x1, a.as_bytes()).await;
    write_masked_frame(&mut stream, false, 0x0, b.as_bytes()).await;
    // Before the final fragment, nothing should be forwarded.
    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "no byte forwarded before the message is whole",
    );
    write_masked_frame(&mut stream, true, 0x0, c.as_bytes()).await;

    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(
        rec.len(),
        1,
        "fragments reassembled into exactly one message"
    );
    assert_eq!(
        rec[0], whole,
        "reassembled message is byte-exact and forwarded"
    );
}
