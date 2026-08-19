//! Integration tests for the relay transport against a fake upstream (no live infra).
//!
//! The fake [`FakeFactory`] records every client→server message the broker **admits**
//! (forwards) and can script server→client frames. Because refused messages never reach
//! the fake, "zero upstream bytes" is directly observable: the forbidden method is simply
//! absent from `recorded`.

use std::collections::VecDeque;
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
use codex_broker::{LaunchFingerprint, COMMAND_EXEC_APPROVAL};

// ---------------------------------------------------------------------------
// Fake upstream + broker harness
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeState {
    recorded: Mutex<Vec<String>>,
    /// One scripted s2c frame list **per upstream connection**, popped front-first as each
    /// leg connects. This models the approval fanout: the same `serverRequest` is scripted
    /// onto each leg's own upstream, so every leg observes (and registers) its own copy.
    scripts: Mutex<VecDeque<Vec<Message>>>,
}

#[derive(Clone)]
struct FakeFactory {
    inner: Arc<FakeState>,
}

impl UpstreamFactory for FakeFactory {
    fn connect(&self) -> ConnectFuture {
        let recorded = Arc::clone(&self.inner);
        let scripted: Vec<Message> = self
            .inner
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
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
    /// Recorded audit-log lines (winner-provenance etc.). Empty unless the broker was
    /// started with the event-recording variant.
    events: Arc<Mutex<Vec<String>>>,
}

impl Harness {
    /// Script one s2c frame onto the **next** upstream connection to be established
    /// (front-first). Push these in the order the legs will connect.
    fn push_script(&self, frames: Vec<Message>) {
        self.state.scripts.lock().unwrap().push_back(frames);
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

fn start_broker() -> Harness {
    start_broker_inner(false)
}

/// Like [`start_broker`] but with the audit sink wired to a recording buffer, so a test
/// can assert on emitted log lines (e.g. winner-provenance).
fn start_broker_with_events() -> Harness {
    start_broker_inner(true)
}

fn start_broker_inner(record_events: bool) -> Harness {
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
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut broker = Broker::new(tui_sock.clone(), ccd_sock.clone(), fingerprint(), factory);
    if record_events {
        let sink = Arc::clone(&events);
        broker = broker.with_event_sink(Arc::new(move |line: &str| {
            sink.lock().unwrap().push(line.to_string());
        }));
    }
    tokio::spawn(async move {
        let _ = broker.serve().await;
    });
    Harness {
        tui_sock,
        ccd_sock,
        state,
        events,
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
    h.push_script(vec![Message::Text(
        r#"{"method":"thread/started","params":{"thread":{"id":"01a0-ours","path":"/x"}}}"#.into(),
    )]);
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
    h.push_script(vec![Message::Text(payload.clone())]);

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
// One-use response-capability fanout (2d-ii)
//
// An approval `serverRequest` is scripted onto each leg's own upstream (the fanout),
// then c2s responses are driven. A refused/losing response is directly observable as
// absent from the shared `recorded`, since it forwards zero upstream bytes.
// ---------------------------------------------------------------------------

/// An s2c approval `serverRequest`: a server→client request carrying a top-level id and a
/// `params.threadId` (the shape captured in `fixtures/codex/*.jsonl`).
fn approval(method: &str, thread: &str, id: i64) -> Message {
    Message::Text(format!(
        r#"{{"method":"{method}","id":{id},"params":{{"threadId":"{thread}","itemId":"x"}}}}"#
    ))
}

/// A method-less response (approval answer) tagged so a winner is distinguishable from a
/// losing sibling in `recorded`.
fn answer(id: i64, tag: &str) -> Message {
    Message::Text(format!(r#"{{"id":{id},"result":{{"by":"{tag}"}}}}"#))
}

/// Drain (and assert) the scripted approval frame off a leg, guaranteeing the broker has
/// observed and registered the capability on that connection before responses are sent.
async fn drain_approval(ws: &mut WebSocketStream<UnixStream>) {
    let f = ws.next().await.unwrap().unwrap();
    assert!(
        f.to_text().unwrap().contains("requestApproval"),
        "expected the scripted approval serverRequest",
    );
}

const PERMISSIONS_APPROVAL: &str = "item/permissions/requestApproval";

#[tokio::test]
async fn fanout_phone_family_ccd_wins_tui_sibling_revoked() {
    let h = start_broker_with_events();
    // Same approval fans out onto both legs' upstreams (tui connects first, then ccd).
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]);
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]);
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    // ccd answers first → its bytes forward.
    ccd.send(answer(0, "ccd")).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1);
    assert!(rec[0].contains(r#""by":"ccd""#), "ccd winner forwarded");

    // The TUI sibling then answers the same id → revoked, zero upstream bytes.
    tui.send(answer(0, "tui")).await.unwrap();
    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert_eq!(rec.len(), 1, "losing sibling forwards zero bytes");
    assert!(!rec.iter().any(|m| m.contains(r#""by":"tui""#)));
    // Winner-provenance is logged.
    assert!(
        h.events()
            .iter()
            .any(|e| e.contains("capability won: winner=Ccd")),
        "winner-provenance = ccd, events: {:?}",
        h.events()
    );
}

#[tokio::test]
async fn fanout_phone_family_tui_wins_ccd_sibling_revoked() {
    let h = start_broker_with_events();
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]);
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]);
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    // TUI answers first → its bytes forward; ccd is then the losing sibling.
    tui.send(answer(0, "tui")).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1);
    assert!(rec[0].contains(r#""by":"tui""#), "tui winner forwarded");

    ccd.send(answer(0, "ccd")).await.unwrap();
    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert_eq!(rec.len(), 1, "losing sibling forwards zero bytes");
    assert!(!rec.iter().any(|m| m.contains(r#""by":"ccd""#)));
    assert!(
        h.events()
            .iter()
            .any(|e| e.contains("capability won: winner=Tui")),
        "winner-provenance = tui, events: {:?}",
        h.events()
    );
}

#[tokio::test]
async fn observe_only_family_refuses_ccd_authorizes_tui() {
    let h = start_broker();
    // A permissions approval grants ONLY the TUI (ccd can never answer it).
    h.push_script(vec![approval(PERMISSIONS_APPROVAL, "th-P", 0)]);
    h.push_script(vec![approval(PERMISSIONS_APPROVAL, "th-P", 0)]);
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    // ccd answers → zero bytes (never granted).
    ccd.send(answer(0, "ccd")).await.unwrap();
    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "ccd cannot answer an observe-only family",
    );

    // The TUI answer to the same request forwards.
    tui.send(answer(0, "tui")).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1);
    assert!(rec[0].contains(r#""by":"tui""#));
}

#[tokio::test]
async fn unsolicited_response_forwards_zero_bytes() {
    let h = start_broker();
    // No serverRequest was ever observed on this leg.
    let mut tui = connect(&h.tui_sock).await;
    tui.send(answer(99, "ghost")).await.unwrap();
    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "an unsolicited response has no live capability",
    );
}

#[tokio::test]
async fn duplicate_response_on_same_leg_is_one_use() {
    let h = start_broker();
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]);
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    // First answer forwards; the second on the same leg is spent (one-use).
    ccd.send(answer(0, "first")).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1);
    assert!(rec[0].contains(r#""by":"first""#));

    ccd.send(answer(0, "second")).await.unwrap();
    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert_eq!(rec.len(), 1, "duplicate answer forwards zero bytes");
    assert!(!rec.iter().any(|m| m.contains(r#""by":"second""#)));
}

#[tokio::test]
async fn per_thread_id_reuse_resolves_to_the_correct_slot() {
    let h = start_broker();
    // Both legs see the SAME bare id=0, but for DIFFERENT threads — the per-leg view
    // disambiguates id→thread, so the two arbiter slots are independent and answering one
    // leaves the other live.
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // tui leg
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-B", 0)]); // ccd leg
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    // TUI answers id=0 (thread A) and ccd answers id=0 (thread B): both forward, because
    // they consume different slots — neither revokes the other.
    tui.send(answer(0, "tui-A")).await.unwrap();
    ccd.send(answer(0, "ccd-B")).await.unwrap();
    let rec = recorded_after(&h.state, 2).await;
    assert_eq!(rec.len(), 2, "two distinct threads' answers both forward");
    assert!(rec.iter().any(|m| m.contains(r#""by":"tui-A""#)));
    assert!(rec.iter().any(|m| m.contains(r#""by":"ccd-B""#)));
}

// ---------------------------------------------------------------------------
// Tombstone regressions (the CRITICAL bare-id ambiguity routes)
//
// Server-request ids are per-thread small ints reused from 0. A bare Response frame
// carries no provenance, so the instant an id is observed twice on a leg the broker can
// no longer prove which request a `{id}` response answers. Each bare id is a per-leg
// 3-state (Unseen → Bound → Tombstoned): the first observation binds, ANY second
// observation tombstones it permanently, and a Tombstoned/Unseen id cannot authorize —
// so even the original binding becomes unanswerable (fail closed). These tests drive the
// three alias routes (reverse alias, late/duplicate after reuse, losing sibling) end to
// end through the fake upstream and assert ZERO forwarded bytes after a collision.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reverse_alias_original_unanswerable_after_collision() {
    // THE CRITICAL CASE, end to end. One leg observes phone-capable id=0 for th-A, then
    // reuses id=0 for th-B. The collision tombstones id=0, so th-A — never answered before
    // the collision — is now unanswerable: an id=0 response forwards ZERO bytes.
    let h = start_broker();
    h.push_script(vec![
        approval(COMMAND_EXEC_APPROVAL, "th-A", 0),
        approval(COMMAND_EXEC_APPROVAL, "th-B", 0),
    ]);
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await; // th-A binds id=0
    drain_approval(&mut ccd).await; // th-B reuses id=0 ⇒ collision ⇒ tombstone

    ccd.send(answer(0, "would-be-A")).await.unwrap();
    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "after a collision the original binding is unanswerable (zero bytes)",
    );
}

#[tokio::test]
async fn late_duplicate_after_same_id_reuse_forwards_zero_bytes() {
    // One leg observes id=0 for th-A, then id=0 again for th-B (reuse). The reuse is a
    // collision that tombstones id=0, so NO id=0 response forwards — neither the answer the
    // attacker means for th-B nor a late one for th-A. (Under the old never-rebind form
    // th-A stayed answerable once; that was the reverse-alias defect.)
    let h = start_broker();
    h.push_script(vec![
        approval(COMMAND_EXEC_APPROVAL, "th-A", 0),
        approval(COMMAND_EXEC_APPROVAL, "th-B", 0),
    ]);
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await; // th-A
    drain_approval(&mut ccd).await; // th-B reuse ⇒ tombstone

    ccd.send(answer(0, "first")).await.unwrap();
    ccd.send(answer(0, "stale")).await.unwrap();
    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert!(
        rec.is_empty(),
        "every id=0 answer after a collision forwards zero bytes, got {rec:?}"
    );
}

#[tokio::test]
async fn losing_sibling_after_same_id_reuse_forwards_zero_bytes() {
    // Fanout to both legs at id=0 (th-A), each leg ALSO reuses id=0 for th-B. On EACH leg
    // the reuse is a collision that tombstones id=0, so neither leg can answer id=0 — no
    // aliasing to th-A or th-B on either side.
    let h = start_broker();
    h.push_script(vec![
        approval(COMMAND_EXEC_APPROVAL, "th-A", 0),
        approval(COMMAND_EXEC_APPROVAL, "th-B", 0),
    ]);
    h.push_script(vec![
        approval(COMMAND_EXEC_APPROVAL, "th-A", 0),
        approval(COMMAND_EXEC_APPROVAL, "th-B", 0),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    drain_approval(&mut tui).await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;
    drain_approval(&mut ccd).await;

    ccd.send(answer(0, "ccd")).await.unwrap();
    tui.send(answer(0, "tui")).await.unwrap();
    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert!(
        rec.is_empty(),
        "both legs tombstoned id=0; no sibling forwards, got {rec:?}"
    );
}

#[tokio::test]
async fn observe_only_then_phone_same_bare_id_tombstones_both_legs() {
    // A TUI-only permissions request at id=0, then a phone-family request reuses id=0, on
    // both legs. The reuse is a collision that tombstones id=0, so BOTH ccd and the
    // original tui answer forward zero bytes (no phone upgrade AND no original tui answer).
    let h = start_broker();
    h.push_script(vec![
        approval(PERMISSIONS_APPROVAL, "th-P", 0),
        approval(COMMAND_EXEC_APPROVAL, "th-C", 0),
    ]);
    h.push_script(vec![
        approval(PERMISSIONS_APPROVAL, "th-P", 0),
        approval(COMMAND_EXEC_APPROVAL, "th-C", 0),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    drain_approval(&mut tui).await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;
    drain_approval(&mut ccd).await;

    ccd.send(answer(0, "ccd")).await.unwrap();
    tui.send(answer(0, "tui")).await.unwrap();
    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "a collision tombstones id=0 on both legs — no answer forwards",
    );
}

#[tokio::test]
async fn divergent_legs_same_bare_id_different_threads_are_independent() {
    // The DIVERGENT-LEGS scenario, and why it is fail-closed under a PER-LEG tombstone.
    // Two GENUINELY DIFFERENT approvals happen to share bare id=0: th-A on the tui leg,
    // th-B on the ccd leg. Each leg observes its id=0 exactly ONCE, so each holds a clean
    // Bound (no collision on either leg) and each is answerable on its own leg. Both
    // forwarding is CORRECT — they are different approvals, not the same logical one. The
    // same-logical-approval fanned to both legs is the arbiter's job (one winner), covered
    // by the fanout tests. There is no under-refusal to produce here: a per-leg tombstone
    // fires only on a same-leg reuse, which this scenario does not contain.
    let h = start_broker();
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // tui leg
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-B", 0)]); // ccd leg
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    tui.send(answer(0, "tui-A")).await.unwrap();
    ccd.send(answer(0, "ccd-B")).await.unwrap();
    let rec = recorded_after(&h.state, 2).await;
    assert_eq!(rec.len(), 2, "two distinct threads' answers both forward");
    assert!(rec.iter().any(|m| m.contains(r#""by":"tui-A""#)));
    assert!(rec.iter().any(|m| m.contains(r#""by":"ccd-B""#)));
}

#[tokio::test]
async fn escaped_request_approval_method_is_observed_end_to_end() {
    // An escaped method name defeats a raw substring guard but decodes to a real approval.
    // The size-only observe gate parses it, so the capability IS registered and a matching
    // response forwards — proving the escape is not silently skipped.
    let h = start_broker();
    // The final `l` of the method is JSON-escaped (l), so the raw bytes carry no literal
    // "requestApproval" marker but decode to item/commandExecution/requestApproval. Built
    // at runtime so the source has no fragile literal escape (`"\\u006c"` == `l`).
    let escaped_method = format!(
        "{}\\u006c",
        &COMMAND_EXEC_APPROVAL[..COMMAND_EXEC_APPROVAL.len() - 1]
    );
    let escaped = Message::Text(format!(
        r#"{{"method":"{escaped_method}","id":0,"params":{{"threadId":"th-A","itemId":"x"}}}}"#
    ));
    assert!(
        !escaped.to_text().unwrap().contains("requestApproval"),
        "the scripted frame must be genuinely escaped"
    );
    h.push_script(vec![escaped]);
    let mut ccd = connect(&h.ccd_sock).await;
    // Byte-exact passthrough still delivers the frame (drain it, but it lacks the literal
    // marker so we don't use drain_approval's assertion here).
    let _ = ccd.next().await.unwrap().unwrap();

    ccd.send(answer(0, "decoded")).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(
        rec.len(),
        1,
        "the escaped approval was decoded and registered"
    );
    assert!(rec[0].contains(r#""by":"decoded""#));
}

#[tokio::test]
async fn duplicate_member_approval_frame_registers_no_capability() {
    // An s2c approval frame with a duplicate `id` member is ambiguous (parser-differential)
    // — it must not register a capability, so a response for it forwards zero bytes.
    let h = start_broker();
    h.push_script(vec![Message::Text(format!(
        r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":0,"id":1,"params":{{"threadId":"th-A","itemId":"x"}}}}"#
    ))]);
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await; // byte-exact passthrough still delivers the frame

    ccd.send(answer(0, "dup")).await.unwrap();
    ccd.send(answer(1, "dup")).await.unwrap();
    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "a duplicate-member approval frame registers no capability",
    );
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
