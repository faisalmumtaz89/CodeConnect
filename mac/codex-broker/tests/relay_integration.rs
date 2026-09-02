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
    /// One scripted **reply** table per upstream connection: `(trigger, frame)` pairs. When
    /// an admitted c2s message contains `trigger`, the fake emits `frame` s2c. A connect-time
    /// script cannot model a RESPONSE — a response only exists *because* a request was
    /// admitted, which is exactly the correlation the thread binding now requires.
    replies: Mutex<VecDeque<Vec<(String, Message)>>>,
    /// One flag per upstream connection, popped front-first: `true` makes that connection's
    /// **upstream write side dead on arrival** (the receiver is dropped immediately), so the
    /// relay's `to_upstream.send` fails for the first message it tries to forward. This is
    /// how the round-2 P3 "proven send" test produces a real write failure. The read side is
    /// deliberately kept alive (its sender is parked in `parked_senders`), because a closed
    /// read side would close the leg before any client message was even classified.
    dead_upstreams: Mutex<VecDeque<bool>>,
    /// Keeps the s2c senders of dead-write upstreams alive for the life of the test.
    parked_senders: Mutex<Vec<tokio::sync::mpsc::Sender<Message>>>,
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
        let mut replies: Vec<(String, Message)> = self
            .inner
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
        let dead_write = self
            .inner
            .dead_upstreams
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(false);
        Box::pin(async move {
            let (to_tx, mut to_rx) = tokio::sync::mpsc::channel::<Message>(64);
            let (from_tx, from_rx) = tokio::sync::mpsc::channel::<Message>(64);
            if dead_write {
                // Write side dead on arrival; read side parked open so the leg is not closed
                // by an upstream EOF before the client's first message is classified.
                drop(to_rx);
                recorded.parked_senders.lock().unwrap().push(from_tx);
                return Ok(UpstreamChannels {
                    to_upstream: to_tx,
                    from_upstream: from_rx,
                });
            }
            tokio::spawn(async move {
                for m in scripted {
                    if from_tx.send(m).await.is_err() {
                        return;
                    }
                }
                // Hold from_tx open by keeping it in scope while draining client traffic.
                while let Some(m) = to_rx.recv().await {
                    if let Message::Text(t) = m {
                        // Answer the FIRST matching trigger, once (a real app-server answers
                        // each request exactly once).
                        if let Some(i) = replies.iter().position(|(trig, _)| t.contains(trig)) {
                            let (_, frame) = replies.remove(i);
                            if from_tx.send(frame).await.is_err() {
                                return;
                            }
                        }
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

/// The workspace the coordinator launched this session in — already canonicalized, and the
/// anchor every creation response must match (round-2 P4).
const LAUNCH_CWD: &str = "/work/proj";

fn fingerprint() -> LaunchFingerprint {
    LaunchFingerprint {
        approval_policy: "untrusted".into(),
        approvals_reviewer: "user".into(),
        sandbox: "read-only".into(),
        hooks_enabled: true,
        launch_cwd: LAUNCH_CWD.into(),
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

    /// Script `(trigger, frame)` replies onto the **next** upstream connection: when that
    /// leg admits a c2s message containing `trigger`, the fake answers with `frame`.
    fn push_replies(&self, replies: Vec<(String, Message)>) {
        self.state.replies.lock().unwrap().push_back(replies);
    }

    /// Make the **next** upstream connection's write side dead on arrival, so the relay's
    /// first `to_upstream.send` on that leg fails (round-2 P3).
    fn push_dead_upstream(&self, dead: bool) {
        self.state.dead_upstreams.lock().unwrap().push_back(dead);
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

/// Await the next c2s reply frame, **bounded**. A test that expects a synthetic refusal
/// must fail rather than hang if the broker forwarded instead of refusing — an unbounded
/// `ws.next()` would block forever on exactly the regression the test exists to catch.
async fn next_frame(ws: &mut WebSocketStream<UnixStream>) -> serde_json::Value {
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("no reply frame within 5s (did the broker forward instead of replying?)")
        .expect("stream closed")
        .expect("ws error");
    serde_json::from_str(msg.to_text().unwrap()).unwrap()
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

// ---------------------------------------------------------------------------
// The correlated-creation thread binding (review findings P1/P3), end to end over the relay.
// ---------------------------------------------------------------------------

/// The captured `turn/start` shape (P4/P6), naming `thread` in the bound workspace.
fn turn_frame(thread: &str) -> String {
    turn_frame_in(thread, LAUNCH_CWD)
}

/// The same shape with an explicit `cwd`, so a test can make the TURN agree with a binding
/// that should never have been installed — which is what isolates the round-2 P4 anchor
/// from the (also-present) turn-vs-binding equality check.
fn turn_frame_in(thread: &str, cwd: &str) -> String {
    serde_json::json!({
        "method": "turn/start",
        "id": 3,
        "params": {
            "threadId": thread,
            "approvalPolicy": "untrusted",
            "approvalsReviewer": "user",
            "sandboxPolicy": null,
            "cwd": cwd,
            "runtimeWorkspaceRoots": [LAUNCH_CWD],
            "permissions": null,
            "environments": null,
            "multiAgentMode": null,
            "responsesapiClientMetadata": null,
            "additionalContext": null,
            "outputSchema": null,
            "collaborationMode": null
        }
    })
    .to_string()
}

const CREATION_REQUEST: &str = r#"{"method":"thread/start","id":"start-1","params":{"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":"read-only"}}"#;

/// The creation RESPONSE the fake app-server answers `CREATION_REQUEST` with: the three
/// proofs the binding requires (`result.thread.id`, `result.cwd`,
/// `result.runtimeWorkspaceRoots`) in the shape measured off a real codex 0.147 server.
fn creation_response(thread: &str) -> Message {
    creation_response_in(thread, LAUNCH_CWD)
}

/// A creation response naming an explicit `cwd` — used to prove the round-2 P4 anchor: a
/// response outside the coordinator-owned launch cwd binds nothing.
fn creation_response_in(thread: &str, cwd: &str) -> Message {
    Message::Text(
        serde_json::json!({
            "id": "start-1",
            "result": {
                "thread": {"id": thread, "path": "/x"},
                "cwd": cwd,
                "runtimeWorkspaceRoots": [LAUNCH_CWD]
            }
        })
        .to_string(),
    )
}

/// Drive one leg through the admitted creation and await its correlated response, so the
/// binding is installed before the caller's next message.
async fn create_thread(ws: &mut WebSocketStream<UnixStream>, thread: &str) -> serde_json::Value {
    ws.send(Message::Text(CREATION_REQUEST.into()))
        .await
        .unwrap();
    let v = next_frame(ws).await;
    assert_eq!(
        v["result"]["thread"]["id"], thread,
        "the creation response must reach the client (and the observer ran first)"
    );
    v
}

#[tokio::test]
async fn turn_start_without_a_bound_thread_fails_closed() {
    // The relay has admitted no creation, so nothing is bound: the turn is policy-refused
    // and forwards zero upstream bytes.
    let h = start_broker();
    let mut ws = connect(&h.tui_sock).await;
    ws.send(Message::Text(turn_frame("01a0-ours")))
        .await
        .unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(
        v["error"]["code"], -32001,
        "a turn naming no bound thread is policy-refused"
    );
    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "an unbound turn/start must reach zero upstream bytes",
    );
}

#[tokio::test]
async fn a_bare_thread_started_binds_nothing_over_the_relay() {
    // P1 ROOT FIX, end to end. The creation IS admitted (so a creation is pending and the
    // s2c observer is fully armed — the cheap guard is NOT what refuses here), but the
    // server answers with a `thread/started` ANNOUNCEMENT instead of a creation response.
    // An announcement binds NOTHING, so a turn naming that thread is refused and reaches
    // zero upstream bytes. In the rejected first cut this exact frame bound the thread.
    let h = start_broker();
    h.push_replies(vec![(
        "thread/start".into(),
        Message::Text(
            r#"{"method":"thread/started","params":{"thread":{"id":"01a0-ours","path":"/x"}}}"#
                .into(),
        ),
    )]);
    let mut ws = connect(&h.tui_sock).await;
    ws.send(Message::Text(CREATION_REQUEST.into()))
        .await
        .unwrap();
    let started = ws.next().await.unwrap().unwrap();
    assert!(started.to_text().unwrap().contains("thread/started"));

    ws.send(Message::Text(turn_frame("01a0-ours")))
        .await
        .unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(v["error"]["code"], -32001, "receipt is not lineage");
    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().clone(),
        vec![CREATION_REQUEST.to_string()],
        "an announcement-only thread must never authorize a turn",
    );
}

#[tokio::test]
async fn turn_start_on_the_verified_thread_forwards_original_bytes() {
    // Script the upstream to answer the admitted `thread/start` with its creation RESPONSE
    // (not a bare `thread/started`), then send the MEASURED turn shape and assert the
    // original bytes reach upstream.
    let h = start_broker();
    h.push_replies(vec![(
        "thread/start".into(),
        creation_response("01a0-ours"),
    )]);
    let mut ws = connect(&h.tui_sock).await;
    create_thread(&mut ws, "01a0-ours").await;

    let turn = turn_frame("01a0-ours");
    ws.send(Message::Text(turn.clone())).await.unwrap();

    let rec = recorded_after(&h.state, 2).await;
    assert_eq!(rec.len(), 2);
    assert_eq!(rec[0], CREATION_REQUEST);
    assert_eq!(rec[1], turn, "the head-checked turn forwards byte-exact");
}

#[tokio::test]
async fn turn_start_in_a_different_workspace_is_refused() {
    // P5, end to end: the thread is verified, but the turn names a cwd other than the one
    // bound from its creation response.
    let h = start_broker();
    h.push_replies(vec![(
        "thread/start".into(),
        creation_response("01a0-ours"),
    )]);
    let mut ws = connect(&h.tui_sock).await;
    create_thread(&mut ws, "01a0-ours").await;

    let mut turn: serde_json::Value = serde_json::from_str(&turn_frame("01a0-ours")).unwrap();
    turn["params"]["cwd"] = serde_json::json!("/somewhere/else");
    ws.send(Message::Text(turn.to_string())).await.unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(v["error"]["code"], -32001);

    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert_eq!(rec, vec![CREATION_REQUEST.to_string()], "zero turn bytes");
}

/// The measured `/new` switch, END TO END over the relay: unsubscribe ×2 → a second
/// `thread/start` → the head follows to B, the retired A is resumable but unturnable.
///
/// 2e-4c replaces the old `a_second_thread_start_is_refused_over_the_relay`, which pinned
/// the pre-switch rule ("once one thread is bound the creation slot stays closed") — the
/// rule that made a real user unable to press `/new`.
#[tokio::test]
async fn the_new_switch_flows_end_to_end_over_the_relay() {
    let h = start_broker();
    h.push_replies(vec![
        ("thread/start".into(), creation_response("01a0-a")),
        (
            "thread/unsubscribe".into(),
            Message::Text(r#"{"id":7,"result":{"status":"unsubscribed"}}"#.into()),
        ),
        (
            "thread/unsubscribe".into(),
            Message::Text(r#"{"id":8,"result":{"status":"unsubscribed"}}"#.into()),
        ),
        (
            "thread/start".into(),
            Message::Text(
                serde_json::json!({
                    "id": "start-2",
                    "result": {
                        "thread": {"id": "01a0-b", "path": "/x"},
                        "cwd": LAUNCH_CWD,
                        "runtimeWorkspaceRoots": [LAUNCH_CWD]
                    }
                })
                .to_string(),
            ),
        ),
    ]);
    let mut ws = connect(&h.tui_sock).await;
    create_thread(&mut ws, "01a0-a").await;

    // The marker, twice, naming the active head — exactly what `/new` sends. Both forward:
    // the switch behind them is admissible (round-1 P4).
    for id in [7, 8] {
        ws.send(Message::Text(
            serde_json::json!({"method":"thread/unsubscribe","id":id,
                               "params":{"threadId":"01a0-a"}})
            .to_string(),
        ))
        .await
        .unwrap();
        let v = next_frame(&mut ws).await;
        assert_eq!(
            v["result"]["status"], "unsubscribed",
            "unsubscribe #{id} must reach the server and be answered — the real TUI AWAITS \
             this response before sending the next frame, which is why the prefix cannot be \
             held"
        );
    }

    // The switch.
    ws.send(Message::Text(
        CREATION_REQUEST.replace("start-1", "start-2"),
    ))
    .await
    .unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(
        v["result"]["thread"]["id"], "01a0-b",
        "the switch must be admitted and its response relayed"
    );

    // The head FOLLOWED: a turn on B forwards...
    ws.send(Message::Text(turn_frame("01a0-b"))).await.unwrap();
    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert!(
        rec.contains(&turn_frame("01a0-b")),
        "a turn on the new head must reach the server; recorded: {rec:?}"
    );

    // ...and a turn on the RETIRED thread is policy-refused with zero upstream bytes.
    let before = h.state.recorded.lock().unwrap().len();
    ws.send(Message::Text(turn_frame("01a0-a"))).await.unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(
        v["error"]["code"], -32001,
        "a turn on a retired thread is refused"
    );
    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().len(),
        before,
        "zero bytes upstream for the refused turn"
    );
}

/// **P4 end to end: the prefix is refused rather than dropping the subscription.**
///
/// The defect: `unsubscribe, unsubscribe, thread/start` where the start is doomed leaves
/// the TUI on the old thread and UNSUBSCRIBED from it — silently blind. Here the switch is
/// doomed because a creation is already in flight, so the unsubscribe that would have begun
/// it is refused with ZERO upstream bytes and the subscription is never touched.
#[tokio::test]
async fn a_switch_prefix_is_refused_when_the_switch_behind_it_cannot_be_admitted() {
    let h = start_broker();
    h.push_replies(vec![(
        "thread/start".into(),
        creation_response("01a0-ours"),
    )]);
    let mut ws = connect(&h.tui_sock).await;
    create_thread(&mut ws, "01a0-ours").await;

    // Put a switch in flight and leave it unanswered.
    ws.send(Message::Text(
        CREATION_REQUEST.replace("start-1", "start-2"),
    ))
    .await
    .unwrap();
    settle().await;
    let before = h.state.recorded.lock().unwrap().clone();

    // Now the prefix of ANOTHER switch. It must refuse, not unsubscribe.
    ws.send(Message::Text(
        serde_json::json!({"method":"thread/unsubscribe","id":9,
                           "params":{"threadId":"01a0-ours"}})
        .to_string(),
    ))
    .await
    .unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(
        v["error"]["code"], -32001,
        "the prefix is policy-refused: {v}"
    );
    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().clone(),
        before,
        "a refused prefix must send ZERO upstream bytes — the whole point is that the \
         subscription survives a switch that was never going to happen"
    );
}

/// **ROUND-3 P4a, end to end: the prefix RESERVES, and a turn cannot slip in behind it.**
///
/// The reservation is claimed inside the prefix's own atomic admission (A16.1). This is the
/// only test that exercises the CLASSIFIER's half of that wiring: the session-level tests
/// call `try_admit_prefix` directly, so a classifier that stopped routing the prefix into it
/// would leave them green while the window between the prefix and the start stood wide open.
#[tokio::test]
async fn the_prefix_reserves_the_switch_and_fences_turns_behind_it() {
    let h = start_broker();
    h.push_replies(vec![
        ("thread/start".into(), creation_response("01a0-ours")),
        (
            "thread/unsubscribe".into(),
            Message::Text(r#"{"id":7,"result":{"status":"unsubscribed"}}"#.into()),
        ),
    ]);
    let mut ws = connect(&h.tui_sock).await;
    create_thread(&mut ws, "01a0-ours").await;

    // The prefix of a `/new`.
    ws.send(Message::Text(
        serde_json::json!({"method":"thread/unsubscribe","id":7,
                           "params":{"threadId":"01a0-ours"}})
        .to_string(),
    ))
    .await
    .unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(v["result"]["status"], "unsubscribed", "the prefix forwards");

    // Now a turn must be REFUSED: the prefix has already had a wire effect and the switch
    // behind it is expected next, so this turn would be authorized against a head that is
    // about to move.
    let before = h.state.recorded.lock().unwrap().len();
    ws.send(Message::Text(turn_frame("01a0-ours")))
        .await
        .unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(
        v["error"]["code"], -32001,
        "a turn admitted between the prefix and the start is the exact window the \
         reservation exists to close: {v}"
    );
    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().len(),
        before,
        "and it forwards zero bytes"
    );
}

/// One switch at a time, end to end: a second `thread/start` while the first switch is
/// still in flight is refused with zero upstream bytes. This is the half of the old P3 rule
/// that 2e-4c KEPT, and it is what stops two creations racing to re-point the head.
#[tokio::test]
async fn a_second_switch_while_one_is_in_flight_is_refused_over_the_relay() {
    let h = start_broker();
    // Only the FIRST creation is answered; the switch stays pending for the whole test.
    h.push_replies(vec![(
        "thread/start".into(),
        creation_response("01a0-ours"),
    )]);
    let mut ws = connect(&h.tui_sock).await;
    create_thread(&mut ws, "01a0-ours").await;

    ws.send(Message::Text(
        CREATION_REQUEST.replace("start-1", "start-2"),
    ))
    .await
    .unwrap();
    settle().await;
    let after_switch = h.state.recorded.lock().unwrap().clone();
    assert_eq!(after_switch.len(), 2, "the switch itself forwarded");

    ws.send(Message::Text(
        CREATION_REQUEST.replace("start-1", "start-3"),
    ))
    .await
    .unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(v["error"]["code"], -32001);

    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().clone(),
        after_switch,
        "a second switch while one is pending forwards zero bytes"
    );
}

// ---------------------------------------------------------------------------
// ROUND-2 P1 / P3 / P4, end to end over the relay.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_tui_connection_cannot_answer_the_first_ones_creation() {
    // ROUND-2 P1, THE VULNERABILITY. Two connections of the SAME role — exactly what the
    // TUI's `/resume` picker opens. Connection A's `thread/start` is admitted (pending on
    // A). Connection B then delivers a perfectly-shaped creation RESPONSE carrying A's
    // request id and a thread of B's choosing. Under the old `(Role, RequestId)` key that
    // installed a binding; under the connection-scoped key it correlates to nothing, so no
    // turn on that thread is ever authorized.
    let h = start_broker();
    h.push_script(vec![]); // connection A's upstream: silent
    h.push_script(vec![creation_response("attacker-thread")]); // connection B's upstream
    h.push_replies(vec![]); // A's thread/start is never answered on A's own leg

    let mut a = connect(&h.tui_sock).await;
    a.send(Message::Text(CREATION_REQUEST.into()))
        .await
        .unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1, "the creation was admitted on connection A");

    // B connects; its scripted frame is the forged answer to A's id.
    let mut b = connect(&h.tui_sock).await;
    let forged = b.next().await.unwrap().unwrap();
    assert!(forged.to_text().unwrap().contains("attacker-thread"));
    settle().await;

    // Neither connection may now turn on that thread.
    for ws in [&mut a, &mut b] {
        ws.send(Message::Text(turn_frame("attacker-thread")))
            .await
            .unwrap();
        let v = next_frame(ws).await;
        assert_eq!(
            v["error"]["code"], -32001,
            "a sibling connection's response must bind nothing"
        );
    }
    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().clone(),
        vec![CREATION_REQUEST.to_string()],
        "zero turn bytes after a cross-connection forgery",
    );
}

#[tokio::test]
async fn a_failed_upstream_send_rolls_the_creation_claim_back() {
    // ROUND-2 P3. The first leg's upstream write side is dead, so the admitted
    // `thread/start` claims the creation slot and then provably sends ZERO bytes. The claim
    // must be rolled back, or the session's one creation slot is burned for ever and the
    // real TUI can never create its thread.
    let h = start_broker();
    h.push_dead_upstream(true); // leg 1: writes fail
    h.push_dead_upstream(false); // leg 2: healthy
    h.push_replies(vec![]); // (leg 1 has no reply table)
    h.push_replies(vec![(
        "thread/start".into(),
        creation_response("01a0-ours"),
    )]);

    let mut dead = connect(&h.tui_sock).await;
    dead.send(Message::Text(CREATION_REQUEST.into()))
        .await
        .unwrap();
    // The leg closes (upstream gone) and nothing was recorded.
    let closed = loop {
        match dead.next().await {
            None => break true,
            Some(Ok(Message::Close(_))) | Some(Err(_)) => break true,
            Some(Ok(_)) => continue,
        }
    };
    assert!(closed, "a dead upstream closes the leg");
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "zero bytes reached the upstream",
    );
    settle().await;

    // A fresh connection must still be able to create the session's thread…
    let mut live = connect(&h.tui_sock).await;
    create_thread(&mut live, "01a0-ours").await;
    // …and turn on it.
    let turn = turn_frame("01a0-ours");
    live.send(Message::Text(turn.clone())).await.unwrap();
    let rec = recorded_after(&h.state, 2).await;
    assert_eq!(rec.len(), 2);
    assert_eq!(rec[1], turn, "the rolled-back claim did not burn the slot");
}

#[tokio::test]
async fn a_disconnect_with_a_pending_creation_closes_the_slot_terminally() {
    // ROUND-2 P3. The owning connection vanishes while its creation is in flight. The
    // request DID reach the server, so the slot goes to the indeterminate CLOSED state:
    // never reopened, never bound — a later creation is REFUSED, which is the fail-closed
    // choice that cannot produce a second thread.
    let h = start_broker();
    h.push_replies(vec![]); // leg 1: the creation is never answered
    h.push_replies(vec![(
        "thread/start".into(),
        creation_response("01a0-late"),
    )]);

    let mut first = connect(&h.tui_sock).await;
    first
        .send(Message::Text(CREATION_REQUEST.into()))
        .await
        .unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1, "the creation was admitted and forwarded");
    first.close(None).await.unwrap();
    drop(first);
    settle().await;

    let mut second = connect(&h.tui_sock).await;
    second
        .send(Message::Text(
            CREATION_REQUEST.replace("start-1", "start-2"),
        ))
        .await
        .unwrap();
    let v = next_frame(&mut second).await;
    assert_eq!(
        v["error"]["code"], -32001,
        "a creation left indeterminate by a disconnect must not reopen"
    );
    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().clone(),
        vec![CREATION_REQUEST.to_string()],
        "the second creation reaches zero upstream bytes",
    );
}

#[tokio::test]
async fn a_creation_response_outside_the_launch_cwd_binds_nothing_over_the_relay() {
    // ROUND-2 P4. The response is perfectly shaped, but names a workspace other than the
    // coordinator-owned launch cwd — i.e. the server echoing back a cwd the CLIENT chose.
    // Nothing binds, so the turn that response would have authorized is refused.
    let h = start_broker();
    h.push_replies(vec![(
        "thread/start".into(),
        creation_response_in("01a0-elsewhere", "/somewhere/else"),
    )]);
    let mut ws = connect(&h.tui_sock).await;
    ws.send(Message::Text(CREATION_REQUEST.into()))
        .await
        .unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(v["result"]["thread"]["id"], "01a0-elsewhere");

    // The turn names the SAME foreign workspace the response did, so the turn-vs-binding
    // equality check cannot be what refuses: only the launch-cwd anchor can.
    ws.send(Message::Text(turn_frame_in(
        "01a0-elsewhere",
        "/somewhere/else",
    )))
    .await
    .unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(v["error"]["code"], -32001, "the workspace anchor holds");
    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().clone(),
        vec![CREATION_REQUEST.to_string()],
        "zero turn bytes",
    );
}

#[tokio::test]
async fn thread_start_naming_a_foreign_cwd_is_refused_over_the_relay() {
    // ROUND-2 P4, request side: a creation may not name a workspace other than the launch
    // cwd. The measured real TUI sends `cwd: null` here, which must keep passing.
    let h = start_broker();
    let mut ws = connect(&h.tui_sock).await;
    let mut start: serde_json::Value = serde_json::from_str(CREATION_REQUEST).unwrap();
    start["params"]["cwd"] = serde_json::json!("/somewhere/else");
    ws.send(Message::Text(start.to_string())).await.unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(v["error"]["code"], -32001);
    settle().await;
    assert!(h.state.recorded.lock().unwrap().is_empty(), "zero bytes");

    // The measured shape still forwards (and the refused one did not burn the slot).
    let mut null_cwd: serde_json::Value = serde_json::from_str(CREATION_REQUEST).unwrap();
    null_cwd["params"]["cwd"] = serde_json::Value::Null;
    let text = null_cwd.to_string();
    ws.send(Message::Text(text.clone())).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec, vec![text], "cwd: null is the measured shape");
}

#[tokio::test]
async fn a_creation_response_with_foreign_workspace_roots_binds_nothing_over_the_relay() {
    // A10 FOLLOW-ON (2e-7c), response side — the sibling of the launch-cwd test above.
    //
    // The response is perfectly shaped AND carries the correct launch `cwd`; only its
    // `runtimeWorkspaceRoots` name a workspace the coordinator did not launch. That is the
    // realistic attack, because the app-server echoes this field back VERBATIM from the
    // request (MEASURED): the client chooses it, the server repeats it. Before the anchor
    // this bound successfully and every later turn was then checked against the client's
    // choice. Now nothing binds, so the turn it would have authorized is refused.
    let h = start_broker();
    h.push_replies(vec![(
        "thread/start".into(),
        Message::Text(
            serde_json::json!({
                "id": "start-1",
                "result": {
                    "thread": {"id": "01a0-wide", "path": "/x"},
                    "cwd": LAUNCH_CWD,
                    "runtimeWorkspaceRoots": ["/"]
                }
            })
            .to_string(),
        ),
    )]);
    let mut ws = connect(&h.tui_sock).await;
    ws.send(Message::Text(CREATION_REQUEST.into()))
        .await
        .unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(v["result"]["thread"]["id"], "01a0-wide");

    // The turn names the SAME wide roots the response did, so the turn-vs-binding equality
    // check cannot be what refuses: only the launch-workspace anchor can.
    let mut turn: serde_json::Value = serde_json::from_str(&turn_frame("01a0-wide")).unwrap();
    turn["params"]["runtimeWorkspaceRoots"] = serde_json::json!(["/"]);
    ws.send(Message::Text(turn.to_string())).await.unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(
        v["error"]["code"], -32001,
        "the workspace-roots anchor holds"
    );
    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().clone(),
        vec![CREATION_REQUEST.to_string()],
        "zero turn bytes",
    );
}

#[tokio::test]
async fn thread_start_naming_foreign_workspace_roots_is_refused_over_the_relay() {
    // A10 FOLLOW-ON, request side: a creation may not name workspace ROOTS other than the
    // session's one launch workspace, and the refusal costs ZERO upstream bytes. The measured
    // real TUI sends `[<its own cwd>]` here — which in production is the launch cwd — and
    // that must keep passing.
    let h = start_broker();
    let mut ws = connect(&h.tui_sock).await;
    for roots in [
        serde_json::json!(["/somewhere/else"]),
        serde_json::json!([LAUNCH_CWD, "/somewhere/else"]),
        serde_json::json!([]),
    ] {
        let mut start: serde_json::Value = serde_json::from_str(CREATION_REQUEST).unwrap();
        start["params"]["runtimeWorkspaceRoots"] = roots.clone();
        ws.send(Message::Text(start.to_string())).await.unwrap();
        let v = next_frame(&mut ws).await;
        assert_eq!(v["error"]["code"], -32001, "roots {roots}");
        settle().await;
        assert!(
            h.state.recorded.lock().unwrap().is_empty(),
            "zero bytes for roots {roots}"
        );
    }

    // The measured shape still forwards (and none of the refusals burned the creation slot).
    let mut ok: serde_json::Value = serde_json::from_str(CREATION_REQUEST).unwrap();
    ok["params"]["runtimeWorkspaceRoots"] = serde_json::json!([LAUNCH_CWD]);
    let text = ok.to_string();
    ws.send(Message::Text(text.clone())).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec, vec![text], "[launch cwd] is the measured shape");
}

#[tokio::test]
async fn thread_fork_is_refused_over_the_relay() {
    // P2: no fork frame exists in the wire capture, so a fork's source-thread lineage is
    // unprovable — refused pre-2e-4c, zero upstream bytes.
    let h = start_broker();
    let mut ws = connect(&h.tui_sock).await;
    ws.send(Message::Text(
        r#"{"method":"thread/fork","id":9,"params":{"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":"read-only"}}"#.into(),
    ))
    .await
    .unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(v["id"], 9);
    assert_eq!(v["error"]["code"], -32001);
    settle().await;
    assert!(h.state.recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ccd_resume_bound_to_session_thread() {
    let h = start_broker();
    // The TUI leg creates the thread; its own upstream answers with the creation response,
    // which is what binds the SESSION-wide thread the ccd leg may then resume.
    h.push_replies(vec![(
        "thread/start".into(),
        creation_response("01a0-ours"),
    )]);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, "01a0-ours").await;

    let mut ccd = connect(&h.ccd_sock).await;
    // Resume of an UNKNOWN thread -> refused, zero bytes.
    ccd.send(Message::Text(
        r#"{"method":"thread/resume","id":1,"params":{"threadId":"99-not-ours"}}"#.into(),
    ))
    .await
    .unwrap();
    let bv = next_frame(&mut ccd).await;
    assert_eq!(bv["id"], 1);
    assert_eq!(bv["error"]["code"], -32001);

    // Resume of the SESSION thread -> forwarded.
    ccd.send(Message::Text(
        r#"{"method":"thread/resume","id":2,"params":{"threadId":"01a0-ours"}}"#.into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 2).await;
    assert_eq!(rec.len(), 2);
    assert!(rec[1].contains("01a0-ours"));
}

#[tokio::test]
async fn a_resume_that_re_points_or_substitutes_the_bound_thread_costs_zero_upstream_bytes() {
    // ROUND-5 FINDING 6, over the real relay. Every frame below names the session's OWN bound
    // thread, so `check_resume_binding` — the only thing a resume used to be checked against —
    // passes on all of them. What refuses is the SHAPE, and the refusal must be free: a
    // client-side test cannot see upstream bytes, but the fake upstream here records every
    // frame it is handed, so `recorded` staying at its pre-resume length IS the zero-byte
    // proof.
    let h = start_broker();
    h.push_replies(vec![(
        "thread/start".into(),
        creation_response("01a0-ours"),
    )]);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, "01a0-ours").await;
    let after_creation = h.state.recorded.lock().unwrap().len();

    let mut ccd = connect(&h.ccd_sock).await;
    for (id, extra) in [
        // MEASURED live: `result.runtimeWorkspaceRoots` came back `["/"]` for this frame.
        (1, serde_json::json!({"runtimeWorkspaceRoots": ["/"]})),
        (2, serde_json::json!({"cwd": "/"})),
        // MEASURED live: the same resume that errors WITHOUT history answers WITH it, naming
        // a brand-new thread id whose preview is the injected text.
        (
            3,
            serde_json::json!({"history": [{"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "INJECTED"}]}]}),
        ),
        // MEASURED live A/B: with `path` set the server resolves the PATH's rollout and the
        // requested threadId appears nowhere in its own answer.
        (4, serde_json::json!({"path": "/work/rollout-other.jsonl"})),
        // Neither is a `ThreadResumeParams` property at all in the real 0.147 schema.
        (
            5,
            serde_json::json!({"selectedCapabilityRoots": [{"id": "r",
                "location": {"type": "environment", "environmentId": "e", "path": "/"}}]}),
        ),
        (
            6,
            serde_json::json!({"environments": [{"environmentId": "e", "cwd": "/"}]}),
        ),
    ] {
        let mut frame = serde_json::json!({
            "method": "thread/resume", "id": id,
            "params": {"threadId": "01a0-ours"}
        });
        for (k, v) in extra.as_object().unwrap() {
            frame["params"][k] = v.clone();
        }
        ccd.send(Message::Text(frame.to_string())).await.unwrap();
        let v = next_frame(&mut ccd).await;
        assert_eq!(v["id"], id, "{frame}");
        assert_eq!(v["error"]["code"], -32001, "{frame}");
        settle().await;
        assert_eq!(
            h.state.recorded.lock().unwrap().len(),
            after_creation,
            "zero upstream bytes for {frame}"
        );
    }

    // And the ccd's own legitimate resume — the exact frame `mac/ccd/src/codex_link.rs`
    // builds — still forwards on the same leg, unchanged.
    let ok = r#"{"method":"thread/resume","id":9,"params":{"threadId":"01a0-ours"}}"#;
    ccd.send(Message::Text(ok.into())).await.unwrap();
    let rec = recorded_after(&h.state, after_creation + 1).await;
    assert_eq!(
        rec.last().map(String::as_str),
        Some(ok),
        "the real ccd resume must forward BYTE-EXACT after all those refusals"
    );
}

#[tokio::test]
async fn a_creation_carrying_a_capability_channel_costs_zero_upstream_bytes() {
    // ROUND-5 FINDING 5, over the real relay. Each frame satisfies the 2e-7c workspace anchor
    // (`runtimeWorkspaceRoots: [LAUNCH_CWD]`) and the full launch fingerprint — the only thing
    // wrong with it is a populated capability channel. Two of these three were MEASURED being
    // ACCEPTED by a live codex 0.147 app-server, which created a real thread.
    let h = start_broker();
    let mut ws = connect(&h.tui_sock).await;
    for (id, key, value) in [
        (
            1,
            "selectedCapabilityRoots",
            serde_json::json!([{"id": "probe-root",
                "location": {"type": "environment", "environmentId": "e", "path": "/"}}]),
        ),
        (
            2,
            "dynamicTools",
            serde_json::json!([{"type": "function", "name": "probe_tool",
                "description": "probe", "inputSchema": {"type": "object"}}]),
        ),
        (
            3,
            "environments",
            serde_json::json!([{"environmentId": "e", "cwd": "/",
                "runtimeWorkspaceRoots": ["/"]}]),
        ),
    ] {
        let mut start: serde_json::Value = serde_json::from_str(CREATION_REQUEST).unwrap();
        start["id"] = serde_json::json!(id);
        start["params"]["runtimeWorkspaceRoots"] = serde_json::json!([LAUNCH_CWD]);
        start["params"][key] = value;
        ws.send(Message::Text(start.to_string())).await.unwrap();
        let v = next_frame(&mut ws).await;
        assert_eq!(v["error"]["code"], -32001, "{key}");
        settle().await;
        assert!(
            h.state.recorded.lock().unwrap().is_empty(),
            "zero upstream bytes for a populated {key}"
        );
    }

    // …and none of them burned the single creation slot, so the measured shape still forwards.
    let mut ok: serde_json::Value = serde_json::from_str(CREATION_REQUEST).unwrap();
    ok["params"]["runtimeWorkspaceRoots"] = serde_json::json!([LAUNCH_CWD]);
    ok["params"]["environments"] = serde_json::Value::Null;
    ok["params"]["selectedCapabilityRoots"] = serde_json::Value::Null;
    ok["params"]["dynamicTools"] = serde_json::Value::Null;
    let text = ok.to_string();
    ws.send(Message::Text(text.clone())).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(
        rec,
        vec![text],
        "the measured null shape is what the real TUI sends, and it must forward"
    );
}

#[tokio::test]
async fn the_audit_log_carries_connection_scoped_open_and_close_markers() {
    // ROUND-2 P1 (observability half). Every accepted connection announces itself with a
    // conn-scoped OPEN marker and reports its end with the same id, so a `broker.log` reader
    // (and the later gate) can attribute lines to one connection. The pre-existing markers
    // live gates assert on — `Tui: forward (`, `broker: listening on tui.sock and ccd.sock`,
    // `Ccd leg ended` — are ADDED TO, never rewritten.
    let h = start_broker_with_events();
    let mut tui = connect(&h.tui_sock).await;
    tui.send(Message::Text(
        r#"{"method":"app/list","id":1,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let _ = recorded_after(&h.state, 1).await;
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.close(None).await.unwrap();
    drop(ccd);
    settle().await;

    let events = h.events();
    assert!(
        events
            .iter()
            .any(|e| e == "broker: listening on tui.sock and ccd.sock"),
        "the listening marker is unchanged: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.starts_with("Tui: leg opened (conn ")),
        "a conn-scoped OPEN marker: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.starts_with("Ccd: leg opened (conn ")),
        "a conn-scoped OPEN marker for the ccd leg: {events:?}"
    );
    assert!(
        events.iter().any(|e| e.starts_with("Tui: forward (")),
        "the forward marker keeps its exact live-gate substring: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.contains("Ccd leg ended") && e.contains("(conn ")),
        "the close marker keeps `Ccd leg ended` and gains the conn id: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// ROUND-3 P1 — the total outstanding-request-id ledger, end to end over the relay.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_request_reusing_an_in_flight_id_forwards_zero_bytes_and_keeps_the_leg_open() {
    // A client that pipelines two live requests under ONE id has made its own responses
    // uncorrelatable — and, before round 3, could have had an ordinary answer read as a
    // creation answer. The frame is DROPPED (zero upstream bytes), the leg stays OPEN, and
    // the event is logged for the failure-containment seam.
    let h = start_broker_with_events();
    let mut ws = connect(&h.tui_sock).await;
    ws.send(Message::Text(
        r#"{"method":"app/list","id":1,"params":{"tag":"first"}}"#.into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1, "the first request forwards");

    // Nothing has answered id 1, so it is still outstanding.
    ws.send(Message::Text(
        r#"{"method":"model/list","id":1,"params":{"tag":"second"}}"#.into(),
    ))
    .await
    .unwrap();
    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert_eq!(rec.len(), 1, "a reused in-flight id forwards zero bytes");
    assert!(
        !rec.iter().any(|m| m.contains("second")),
        "the colliding frame must be absent from upstream: {rec:?}"
    );

    // The leg is still open and fully usable under a fresh id.
    ws.send(Message::Text(
        r#"{"method":"app/list","id":2,"params":{"tag":"third"}}"#.into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 2).await;
    assert_eq!(rec.len(), 2);
    assert!(rec[1].contains("third"), "{rec:?}");

    // The drop is audited, and the line is scoped to the connection that made it (M8).
    assert!(
        h.events().iter().any(|e| {
            e.starts_with("Tui: drop, keep open (")
                && e.contains("already outstanding")
                && e.contains("(conn ")
        }),
        "the reused-id drop must be logged and conn-scoped: {:?}",
        h.events()
    );
}

#[tokio::test]
async fn an_answered_id_is_usable_again_over_the_relay() {
    // The other half of the rule: a RESPONSE releases its entry, so a client that reuses an
    // id AFTER it has been answered — which every real client's per-connection counter makes
    // unnecessary, but which the rule must not forbid retroactively — still forwards.
    let h = start_broker();
    h.push_replies(vec![(
        r#""tag":"first""#.into(),
        Message::Text(r#"{"id":1,"result":{"data":[]}}"#.into()),
    )]);
    let mut ws = connect(&h.tui_sock).await;
    ws.send(Message::Text(
        r#"{"method":"app/list","id":1,"params":{"tag":"first"}}"#.into(),
    ))
    .await
    .unwrap();
    // Await the answer, so the release has provably happened.
    let v = next_frame(&mut ws).await;
    assert_eq!(v["id"], 1);

    ws.send(Message::Text(
        r#"{"method":"app/list","id":1,"params":{"tag":"second"}}"#.into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 2).await;
    assert_eq!(rec.len(), 2, "an answered id is free again: {rec:?}");
    assert!(rec[1].contains("second"));
}

#[tokio::test]
async fn a_non_creation_request_holding_the_creation_id_binds_nothing_over_the_relay() {
    // ROUND-3 P1, THE CROSS-METHOD COLLISION, end to end. `app/list` takes the id
    // `start-1` first; the `thread/start` that wanted that id is therefore DROPPED with zero
    // upstream bytes, so no creation is ever pending. The upstream then answers `start-1`
    // with a perfectly-shaped creation RESPONSE — the exact frame that, under a ledger which
    // tracked only creations, would have matched a pending entry. Nothing binds, so the turn
    // it would have authorized is refused.
    let h = start_broker();
    // The creation-shaped answer for id `start-1` is emitted when the THIRD message is
    // admitted, so the ordering is deterministic: the collision is decided before any
    // response can release the id.
    h.push_replies(vec![("model/list".into(), creation_response("smuggled"))]);
    let mut ws = connect(&h.tui_sock).await;

    ws.send(Message::Text(
        r#"{"method":"app/list","id":"start-1","params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1, "the non-creation request took the id");

    // The creation cannot take an in-flight id: zero bytes, no reply, leg open.
    ws.send(Message::Text(CREATION_REQUEST.into()))
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().len(),
        1,
        "the colliding thread/start must forward zero bytes"
    );

    // Now let the creation-shaped answer for `start-1` arrive.
    ws.send(Message::Text(
        r#"{"method":"model/list","id":2,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let smuggled = next_frame(&mut ws).await;
    assert_eq!(smuggled["result"]["thread"]["id"], "smuggled");

    // Nothing bound: a turn naming that thread is policy-refused and forwards zero bytes.
    ws.send(Message::Text(turn_frame("smuggled")))
        .await
        .unwrap();
    let v = next_frame(&mut ws).await;
    assert_eq!(
        v["error"]["code"], -32001,
        "a response to a NON-creation request must never install a binding"
    );
    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert_eq!(rec.len(), 2, "zero thread/start bytes and zero turn bytes");
    assert!(!rec.iter().any(|m| m.contains("thread/start")));
    assert!(!rec.iter().any(|m| m.contains("turn/start")));
}

#[tokio::test]
async fn an_over_long_request_id_forwards_zero_bytes() {
    // ROUND-3 P6. The measured maximum on the wire is 59 bytes; anything past the cap is
    // refused before it is stored, so it can neither grow the ledger nor burn the creation
    // slot. The leg stays open and a normal request still works.
    let h = start_broker_with_events();
    let mut ws = connect(&h.tui_sock).await;
    let long = "x".repeat(4096);
    ws.send(Message::Text(format!(
        r#"{{"method":"app/list","id":"{long}","params":{{}}}}"#
    )))
    .await
    .unwrap();
    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "an over-long id must forward zero bytes",
    );
    // The audit line must not echo the id.
    let drop_line = h
        .events()
        .into_iter()
        .find(|e| e.contains("drop, keep open") && e.contains("byte cap"))
        .expect("the over-long id drop is audited");
    assert!(!drop_line.contains(&long), "the id leaked: {drop_line}");

    ws.send(Message::Text(
        r#"{"method":"app/list","id":1,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1, "the leg stayed usable");
}

#[tokio::test]
async fn every_forward_note_is_scoped_to_its_connection() {
    // M8 — a gate must be able to pair a forward with the connection that made it, by real
    // connection identity. The EXACT substrings the live gates assert are unchanged and the
    // conn id is APPENDED after the closing paren of the note, never spliced into it.
    let h = start_broker_with_events();
    h.push_replies(vec![]); // tui leg
    h.push_replies(vec![(
        "thread/start".into(),
        creation_response("01a0-ours"),
    )]);
    // Leg 1: a plain allowlisted read and an allowlisted notification on the ccd leg.
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(
        r#"{"method":"thread/read","id":1,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    ccd.send(Message::Text(
        r#"{"method":"initialized","params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let _ = recorded_after(&h.state, 2).await;

    // Leg 2: the ownership creation and a head-checked turn on the TUI leg.
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, "01a0-ours").await;
    tui.send(Message::Text(turn_frame("01a0-ours")))
        .await
        .unwrap();
    let _ = recorded_after(&h.state, 4).await;
    settle().await;

    let events = h.events();
    // The gate-asserted substrings, verbatim.
    for marker in [
        "Ccd: forward (request allowlisted)",
        "Ccd: forward (notification allowlisted)",
        "Tui: forward (ownership request: fingerprint asserted)",
        "Tui: forward (turn/start: head-checked; sandbox deferral discharged by the verified \
         thread binding)",
    ] {
        assert!(
            events.iter().any(|e| e.contains(marker)),
            "the live-gate substring {marker:?} must survive verbatim: {events:?}"
        );
    }
    // …and every forward line ENDS with its connection id, appended after the note.
    let forwards: Vec<&String> = events
        .iter()
        .filter(|e| e.contains(": forward ("))
        .collect();
    assert!(forwards.len() >= 4, "{events:?}");
    for line in &forwards {
        assert!(
            line.ends_with(')') && line.contains(") (conn "),
            "a forward note must carry its conn id, appended: {line}"
        );
    }
    // The two legs report DIFFERENT connection ids, which is what makes pairing real.
    let conn_of = |prefix: &str| -> String {
        forwards
            .iter()
            .find(|e| e.starts_with(prefix))
            .map(|e| e[e.rfind("(conn ").unwrap()..].to_string())
            .unwrap_or_else(|| panic!("no forward line for {prefix}: {events:?}"))
    };
    assert_ne!(
        conn_of("Ccd: forward ("),
        conn_of("Tui: forward ("),
        "two legs must not share a connection id"
    );
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

// ---------------------------------------------------------------------------
// The head replayed to a late `ccd` subscriber
// ---------------------------------------------------------------------------

/// The announcement the app-server broadcasts when a thread starts, in the shape
/// measured off a real codex 0.147 server — and written **deliberately
/// noncanonically** (round-2 F6).
///
/// Every byte here is chosen to differ from what `serde_json` would emit for the
/// same value: space before a colon and a newline inside the object (whitespace),
/// `params` before `method` and `path` before `id` (key order), and `7e0` for the
/// unknown field (numeric spelling — a reserialization writes `7.0`). A replay
/// that parsed and re-emitted the frame would therefore be visible as a byte
/// difference even though it is semantically identical, which is what
/// [`text_within`] compares. The unknown field is still here for the second, weaker
/// claim it always made: a frame this broker *composed* could not carry it at all.
fn started_announcement_text(thread: &str) -> String {
    format!(
        "{{ \"params\" :{{\"someFutureField\": 7e0 ,\n  \"thread\":{{\"path\":\"/x\", \
         \"id\":\"{thread}\"}}}}, \"method\":\"thread/started\" }}"
    )
}

fn started_announcement(thread: &str) -> Message {
    Message::Text(started_announcement_text(thread))
}

/// The `ccd` link's own handshake opener.
const CCD_INITIALIZE: &str = r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"codeconnect-ccd","title":"CodeConnect daemon","version":"t"}}}"#;

/// Await one frame **as the bytes it arrived in**, or prove none came. `None` ⇒ the
/// broker sent nothing.
///
/// Raw text and not `serde_json::Value` (round-2 F6): the claim under test is that
/// the replay is the app-server's own frame repeated, and a parsed comparison is
/// satisfied by any reserialization of it — different whitespace, different key
/// order, `7.0` where the server wrote `7e0`. Only the bytes can say "repeated".
async fn text_within(ws: &mut WebSocketStream<UnixStream>, budget: Duration) -> Option<String> {
    let msg = tokio::time::timeout(budget, ws.next()).await.ok()?;
    let msg = msg.expect("stream closed").expect("ws error");
    Some(msg.to_text().unwrap().to_string())
}

/// [`text_within`], parsed — for the assertions that are about the frame's meaning
/// rather than its bytes.
async fn frame_within(
    ws: &mut WebSocketStream<UnixStream>,
    budget: Duration,
) -> Option<serde_json::Value> {
    let text = text_within(ws, budget).await?;
    Some(serde_json::from_str(&text).unwrap())
}

/// **A `ccd` leg that arrives after `thread/started` is still told which thread.**
///
/// This is the ordering the launch actually produces: the daemon's control link is
/// built by a registration, the registration is sent once the launch is `ready`,
/// and `ready` already requires the TUI to be running — so the observer is
/// structurally late and the one-shot announcement is broadcast to nobody. The
/// replay is what makes the late subscriber's outcome the same as an early one's.
#[tokio::test]
async fn a_late_ccd_subscriber_is_replayed_the_announcement_it_missed() {
    let h = start_broker();
    // The TUI leg's upstream: the announcement, then the correlated creation
    // response that verifies it.
    h.push_script(vec![started_announcement("01a0-a")]);
    h.push_replies(vec![("thread/start".into(), creation_response("01a0-a"))]);

    let mut tui = connect(&h.tui_sock).await;
    let announced = next_frame(&mut tui).await;
    assert_eq!(
        announced["method"], "thread/started",
        "the fixture's premise: the announcement went out on the leg that was there"
    );
    create_thread(&mut tui, "01a0-a").await;

    // The observer, connecting only now — after the announcement it needed.
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();

    let replayed = text_within(&mut ccd, Duration::from_secs(5))
        .await
        .expect("a late ccd subscriber must be told the session's head");
    // **Byte-for-byte, against a deliberately noncanonical original** (round-2 F6).
    // A parsed comparison passes for any reserialization; this one fails for a
    // changed space, a reordered key or `7.0` in place of `7e0`.
    assert_eq!(
        replayed,
        started_announcement_text("01a0-a"),
        "the replay must be the app-server's own bytes repeated, not a frame this \
         broker parsed and wrote out again"
    );
    let parsed: serde_json::Value = serde_json::from_str(&replayed).unwrap();
    assert_eq!(parsed["method"], "thread/started");
    assert_eq!(
        parsed["params"]["thread"]["id"], "01a0-a",
        "and it must be the thread this launch is actually on"
    );
}

/// **Nothing to replay is nothing sent.** The ordinary launch: the observer is up
/// before the thread exists, and the real broadcast is still to come. A replay
/// here would be a `thread/started` for a thread that does not exist.
#[tokio::test]
async fn a_ccd_subscriber_with_no_bound_head_is_replayed_nothing() {
    let h = start_broker();
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    assert!(
        frame_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "with no verified binding the broker must announce nothing"
    );
}

/// **The head comparison is load-bearing, not decoration.** An announcement this
/// broker forwarded but never verified — a thread that is not the session's head —
/// is not evidence about where the session is, and replaying it would hand the
/// observer a thread to chase that the broker itself refused to bind.
#[tokio::test]
async fn an_announcement_that_is_not_the_head_is_not_replayed() {
    let h = start_broker();
    h.push_replies(vec![("thread/start".into(), creation_response("01a0-a"))]);
    // Announced on the TUI leg AFTER the binding, so `announced` names a stranger
    // while `bound_thread` still names the head.
    h.push_script(vec![]);

    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, "01a0-a").await;

    // A second leg whose upstream announces a thread nothing bound.
    h.push_script(vec![started_announcement("01a0-ghost")]);
    let mut other = connect(&h.tui_sock).await;
    let ghost = next_frame(&mut other).await;
    assert_eq!(ghost["params"]["thread"]["id"], "01a0-ghost");

    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    assert!(
        frame_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "the last announcement does not name the head, so there is nothing this \
         broker can honestly say"
    );
}

/// **A leg that initializes DURING a creation is served when the head binds**
/// (round-2 F1).
///
/// Replaying once, at subscribe time, left this hole open. The measured `/new`
/// interleave puts the `thread/started` broadcast BEFORE the `thread/start`
/// response (`fixtures/codex/thread-switch.jsonl:31`), and the app-server only
/// broadcasts to connections it has already answered `initialize` for. So a `ccd`
/// leg reconnecting into that window is doubly missed: there is no bound head to
/// replay to it, and it is not yet in the broadcast set for the live frame. Under
/// replay-on-subscribe it stayed unbound for the life of the session.
///
/// Driven exactly in that order — announcement forwarded, leg initializes with
/// nothing bound and is proven to get nothing, and only then does the creation
/// response land.
#[tokio::test]
async fn a_ccd_leg_that_initializes_mid_creation_is_served_when_the_head_binds() {
    let h = start_broker();
    // Leg 1: the announcement goes out with no creation correlated yet — the
    // fixture's interleave.
    h.push_script(vec![started_announcement("01a0-b")]);
    let mut tui = connect(&h.tui_sock).await;
    let announced = next_frame(&mut tui).await;
    assert_eq!(
        announced["params"]["thread"]["id"], "01a0-b",
        "the premise: the broadcast is out and the creation is still in flight"
    );

    // The reconnecting observer, arriving inside that window.
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "with the creation still pending there is no verified head, so the leg is \
         told nothing — this is the state replay-on-subscribe left it in for ever"
    );

    // The creation response lands, on the leg that asked. The head binds NOW, with
    // the observer already initialized and waiting.
    h.push_replies(vec![("thread/start".into(), creation_response("01a0-b"))]);
    let mut creator = connect(&h.tui_sock).await;
    create_thread(&mut creator, "01a0-b").await;

    let delivered = text_within(&mut ccd, Duration::from_secs(5))
        .await
        .expect("the head must reach a leg that was already there when it bound");
    assert_eq!(
        delivered,
        started_announcement_text("01a0-b"),
        "and it is the original announcement's bytes, delivered late"
    );
}

/// **A leg already on the broadcast stream is not sent a second copy** — which is
/// also what keeps the repair from ever walking the reader backwards (round-2 F1,
/// the D4 interaction).
///
/// Once a leg has forwarded a `thread/started` of its own, the app-server has it in
/// the broadcast set and every later announcement arrives live; it is not late any
/// more and there is nothing to repair. That matters beyond tidiness: `ccd`'s visit
/// filter reads an announcement naming neither the bound thread nor the held
/// candidate as a person pressing `/new`, so a leg that has seen a successor
/// announced must never afterwards be handed the head it is leaving.
/// (`ccd::codex_link`'s `a_re_announcement_of_the_bound_thread_is_not_a_switch`
/// pins the other half: the head arriving late while a candidate is held is
/// `Passed`, and the candidate stands.)
///
/// Driven on the ordinary launch's own ordering: the observer is up first, the
/// announcement reaches it live, and the head binds afterwards.
#[tokio::test]
async fn a_leg_already_on_the_broadcast_stream_is_not_replayed_the_head_it_saw_live() {
    let h = start_broker();
    // The observer's own upstream answers its `initialize` with the broadcast —
    // which is the only order the server produces, since it broadcasts to a
    // connection only once it has answered that request.
    h.push_replies(vec![("initialize".into(), started_announcement("01a0-c"))]);
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    let live = text_within(&mut ccd, Duration::from_secs(5))
        .await
        .expect("the leg receives the announcement live, on its own passthrough");
    assert_eq!(live, started_announcement_text("01a0-c"));

    // The creation response now binds exactly that thread, so the (head,
    // announcement) pair is complete and the ONLY thing standing between this leg
    // and a second copy is that it has already been on the stream.
    h.push_replies(vec![("thread/start".into(), creation_response("01a0-c"))]);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, "01a0-c").await;

    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "a leg that is on the broadcast stream is owed nothing, and a duplicate here \
         would be the broker re-announcing a thread the reader already has"
    );
}

/// **The `ccd` role is the authorization, and it is load-bearing** (round-2 F7).
///
/// The head fan-out delivers the app-server's own frame to a connection that was
/// not there to receive it. Who may be that connection is a security question, not
/// an ergonomic one: the TUI is the client that *creates* threads and was there for
/// the announcement by construction, so a copy sent to it is a frame it never asked
/// for on a leg the broker has no reason to write to unsolicited.
///
/// The rule was already right; nothing pinned it. This is the negative case: a
/// fresh TUI leg initializing when a head is established and its announcement is
/// held — every precondition the `ccd` path needs — must neither receive a delivery
/// nor cause one.
#[tokio::test]
async fn a_tui_leg_is_neither_replayed_a_head_nor_able_to_trigger_one() {
    let h = start_broker();
    h.push_script(vec![started_announcement("01a0-d")]);
    h.push_replies(vec![("thread/start".into(), creation_response("01a0-d"))]);
    let mut tui = connect(&h.tui_sock).await;
    next_frame(&mut tui).await;
    create_thread(&mut tui, "01a0-d").await;

    // The head is established AND deliverable — proven by delivering it, so this
    // test cannot pass merely because there was nothing to send.
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    assert_eq!(
        text_within(&mut ccd, Duration::from_secs(5))
            .await
            .as_deref(),
        Some(started_announcement_text("01a0-d").as_str()),
        "the premise: with a bound head and its announcement held, a ccd leg IS served"
    );

    // The same `initialize`, on the TUI socket. The role is the only difference.
    let mut late_tui = connect(&h.tui_sock).await;
    late_tui
        .send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    assert!(
        text_within(&mut late_tui, Duration::from_millis(300))
            .await
            .is_none(),
        "a TUI leg is not a subscriber: it receives no head, and its arrival is not a \
         delivery point"
    );
    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "and it triggers nothing for anybody else either"
    );
}

/// A frame big enough that writing it to a socket **nobody is reading** blocks the
/// leg's task inside `ws.send`.
///
/// Two megabytes against a unix-socket buffer measured in kilobytes: the park is a
/// hard block on the kernel, not a scheduling hope. It is what lets the tests below
/// hold a `ccd` leg still while the head moves underneath it.
///
/// Deliberately NOT a `thread/started`: this must not mark the leg `live_seen`, which
/// would retire it from the repair and make the tests pass for the wrong reason.
fn parking_frame() -> Message {
    Message::Text(
        serde_json::json!({"method": "x/noise", "params": {"blob": "z".repeat(2 * 1024 * 1024)}})
            .to_string(),
    )
}

/// One `/new` on `ws`: the measured prefix (`thread/unsubscribe` ×2, each awaited) and
/// then the second `thread/start`. The caller's reply table must answer all three.
async fn switch_thread(ws: &mut WebSocketStream<UnixStream>, from: &str, to: &str) {
    for id in [7, 8] {
        ws.send(Message::Text(
            serde_json::json!({"method":"thread/unsubscribe","id":id,
                               "params":{"threadId":from}})
            .to_string(),
        ))
        .await
        .unwrap();
        let v = next_frame(ws).await;
        assert_eq!(v["result"]["status"], "unsubscribed", "prefix #{id}");
    }
    ws.send(Message::Text(
        CREATION_REQUEST.replace("start-1", "start-2"),
    ))
    .await
    .unwrap();
    let v = next_frame(ws).await;
    assert_eq!(
        v["result"]["thread"]["id"], to,
        "the switch must be admitted and its response relayed"
    );
}

/// The switch's creation response, correlated to `switch_thread`'s `start-2`.
fn switch_response(thread: &str) -> Message {
    Message::Text(
        serde_json::json!({
            "id": "start-2",
            "result": {
                "thread": {"id": thread, "path": "/x"},
                "cwd": LAUNCH_CWD,
                "runtimeWorkspaceRoots": [LAUNCH_CWD]
            }
        })
        .to_string(),
    )
}

fn unsubscribed(id: u32) -> (String, Message) {
    (
        "thread/unsubscribe".into(),
        Message::Text(
            serde_json::json!({"id": id, "result": {"status": "unsubscribed"}}).to_string(),
        ),
    )
}

/// **A replay queued for the old head is dropped rather than sent once the head has
/// moved** (round-3 F1).
///
/// Enqueueing is not sending. The replay arm is the last of three in the leg's
/// `select!`, so between "A is owed to this leg" and "A leaves this socket" the leg can
/// pass a whole `/new`. Sending A then is not a harmless duplicate: `ccd`'s reader takes
/// an announcement naming neither its visit nor its candidate as a person pressing
/// `/new`, so the repair would walk the link BACK to a thread the session has left.
/// `live_seen` does not cover it — it stops future enqueues, not one already written.
///
/// **Staged, not raced.** The leg is parked inside `ws.send` on a 2 MB frame that
/// nothing is reading, which is a kernel-level block: while it is held, the TUI legs
/// bind A (queueing A to this leg), announce B and switch to B (queueing B behind it),
/// all deterministically. Only then does the client start reading.
///
/// **Mutation:** drop the `head_is` guard from the replay arm and the first frame after
/// the parking frame is `01a0-a` — the switch back the reader must never see.
#[tokio::test]
async fn a_queued_replay_is_dropped_when_the_head_moves_before_it_is_sent() {
    let h = start_broker_with_events();

    // The observer subscribes with NOTHING bound, so it is owed nothing yet — and its
    // own `initialize` is what triggers the frame that parks it.
    h.push_replies(vec![("initialize".into(), parking_frame())]);
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    settle().await; // the leg is now blocked writing 2 MB into a socket nobody reads

    // A is announced and bound while the observer cannot move: A is queued to it.
    h.push_script(vec![started_announcement("01a0-a")]);
    h.push_replies(vec![
        ("thread/start".into(), creation_response("01a0-a")),
        unsubscribed(7),
        unsubscribed(8),
        ("thread/start".into(), switch_response("01a0-b")),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    assert_eq!(
        next_frame(&mut tui).await["params"]["thread"]["id"],
        "01a0-a"
    );
    create_thread(&mut tui, "01a0-a").await;

    // The successor is broadcast (the measured order: announcement before response)…
    h.push_script(vec![started_announcement("01a0-b")]);
    let mut other = connect(&h.tui_sock).await;
    assert_eq!(
        next_frame(&mut other).await["params"]["thread"]["id"],
        "01a0-b"
    );
    // …and then adopted. B is queued behind the A the observer still has not sent.
    switch_thread(&mut tui, "01a0-a", "01a0-b").await;

    // Now the observer is released. The parking frame first, and then the head — the
    // CURRENT one.
    let parked = text_within(&mut ccd, Duration::from_secs(10))
        .await
        .expect("the parking frame is the leg's own passthrough and must arrive");
    assert!(
        parked.contains("x/noise"),
        "the premise: the leg was held inside its passthrough, not idling"
    );
    let next = text_within(&mut ccd, Duration::from_secs(10))
        .await
        .expect("the head owed to this leg must still be delivered");
    assert_eq!(
        next,
        started_announcement_text("01a0-b"),
        "the queued predecessor must be dropped at the send, not sent: this reader \
         would take a late 01a0-a for a `/new` back onto a thread the session left"
    );
    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "and nothing follows it — the stale entry is dropped, not merely reordered"
    );
    let events = h.events();
    assert!(
        events
            .iter()
            .any(|e| e.contains("dropped a stale head replay for 01a0-a")),
        "the drop is stated in the log, so an operator can tell it from a delivery \
         that never happened: {events:?}"
    );
}

/// **A leg descheduled across a `/new` cannot deny the new head to later subscribers**
/// (round-3 F2).
///
/// The app-server broadcasts once, to every connection; each broker leg reads its copy
/// off its own upstream socket, so cross-leg the order is scheduling. A single
/// last-one-wins announcement slot therefore followed arrival rather than the head: leg
/// 2 records B, leg 3 wakes up and records A over it, and from then on the slot names a
/// thread the session has left. That is not a stale read that repairs itself — no
/// further announcement of B is coming, so `deliver_head` refuses to say anything at
/// all and B is denied to every later `ccd` subscriber for the rest of the session.
///
/// **Mutation:** collapse `announced` back to a single last-one-wins slot and the
/// observer below is told nothing.
#[tokio::test]
async fn a_delayed_leg_replaying_an_old_announcement_cannot_strand_the_head() {
    let h = start_broker();
    h.push_script(vec![started_announcement("01a0-a")]);
    h.push_replies(vec![
        ("thread/start".into(), creation_response("01a0-a")),
        unsubscribed(7),
        unsubscribed(8),
        ("thread/start".into(), switch_response("01a0-b")),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    assert_eq!(
        next_frame(&mut tui).await["params"]["thread"]["id"],
        "01a0-a"
    );
    create_thread(&mut tui, "01a0-a").await;

    // A leg that is on time with the successor's broadcast.
    h.push_script(vec![started_announcement("01a0-b")]);
    let mut fast = connect(&h.tui_sock).await;
    assert_eq!(
        next_frame(&mut fast).await["params"]["thread"]["id"],
        "01a0-b"
    );

    // And the delayed one, arriving with the PREDECESSOR's copy afterwards. This is the
    // write that used to overwrite the slot.
    h.push_script(vec![started_announcement("01a0-a")]);
    let mut delayed = connect(&h.tui_sock).await;
    assert_eq!(
        next_frame(&mut delayed).await["params"]["thread"]["id"],
        "01a0-a",
        "the premise: an old announcement really is processed after the new one"
    );

    // The switch response lands: B is the head.
    switch_thread(&mut tui, "01a0-a", "01a0-b").await;

    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    assert_eq!(
        text_within(&mut ccd, Duration::from_secs(5))
            .await
            .as_deref(),
        Some(started_announcement_text("01a0-b").as_str()),
        "the head's own announcement must survive a delayed leg re-presenting its \
         predecessor's"
    );
}
