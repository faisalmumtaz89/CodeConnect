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
use codex_broker::upstream::{ConnectFuture, UpstreamChannels, UpstreamFactory, UpstreamWrite};
use codex_broker::COMMAND_EXEC_APPROVAL;

// ---------------------------------------------------------------------------
// Fake upstream + broker harness
// ---------------------------------------------------------------------------

/// The reply-table trigger for a frame that follows the reply before it. See
/// [`FakeState::replies`].
const FOLLOWS: &str = "&";

#[derive(Default)]
struct FakeState {
    recorded: Mutex<Vec<String>>,
    /// Every binary frame the broker forwarded, byte for byte.
    recorded_binary: Mutex<Vec<Vec<u8>>>,
    /// One scripted s2c frame list **per upstream connection**, popped front-first as each
    /// leg connects. This models the approval fanout: the same `serverRequest` is scripted
    /// onto each leg's own upstream, so every leg observes (and registers) its own copy.
    scripts: Mutex<VecDeque<Vec<Message>>>,
    /// One scripted **reply** table per upstream connection: `(trigger, frame)` pairs. When
    /// an admitted c2s message contains `trigger`, the fake emits `frame` s2c. A connect-time
    /// script cannot model a RESPONSE — a response only exists *because* a request was
    /// admitted, which is exactly the correlation the thread binding now requires. An entry
    /// whose trigger is [`FOLLOWS`] is sent straight after the reply before it, so one
    /// request can put several frames on the leg's upstream in order.
    replies: Mutex<VecDeque<Vec<(String, Message)>>>,
    /// One flag per upstream connection, popped front-first: `true` makes that connection's
    /// **upstream write side dead on arrival** (the receiver is dropped immediately), so the
    /// relay's `to_upstream.send` fails for the first message it tries to forward. This is
    /// how the failed-send tests produce a real write failure. The read side is
    /// deliberately kept alive (its sender is parked in `parked_senders`), because a closed
    /// read side would close the leg before any client message was even classified.
    dead_upstreams: Mutex<VecDeque<bool>>,
    /// One flag per upstream connection, popped front-first: `true` makes that
    /// connection's drain **discard** every admitted c2s message instead of writing it.
    /// This is the fake's model of a write that was handed off successfully and then
    /// failed at the socket: the channel accepted it, and zero bytes reached the
    /// app-server. The message is dropped whole — nothing acknowledges it — which is
    /// exactly what a leg awaiting proof of its write observes when the pump dies
    /// mid-write.
    discard_upstreams: Mutex<VecDeque<bool>>,
    /// One flag per upstream connection, popped front-first: `true` makes that
    /// connection's drain answer every acknowledged write `false` — the receipt the real
    /// [`codex_broker::upstream::pump`] sends when `sink.send` returned `Err`. Nothing is
    /// recorded, because nothing was written.
    ///
    /// This is the **proven** failure, and it is a different fact from
    /// [`FakeState::discard_upstreams`]: there the ack sender is dropped, which proves
    /// only that nobody can say. The two produce different dispositions and the tests
    /// below hold them apart.
    ///
    /// The fake keeps draining afterwards. Production's pump ends its outbound half on a
    /// failed send and the leg follows the upstream down; that teardown is the pump's own
    /// behaviour and is tested in `upstream.rs`. What this models is the one write.
    failed_writes: Mutex<VecDeque<bool>>,
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
        let discard_writes = self
            .inner
            .discard_upstreams
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(false);
        let failed_writes = self
            .inner
            .failed_writes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(false);
        Box::pin(async move {
            let (to_tx, mut to_rx) = tokio::sync::mpsc::channel::<UpstreamWrite>(64);
            let (from_tx, from_rx) = tokio::sync::mpsc::channel::<Message>(64);
            if dead_write {
                // Write side dead on arrival; read side parked open so the leg is not closed
                // by an upstream EOF before the client's first message is classified.
                drop(to_rx);
                // The script still plays. A leg whose write side is dead is exactly the leg
                // that must be able to hold a capability first — the s2c approval is what
                // gives it one — so that the answer it then fails to hand over is an answer
                // the arbiter had reserved a slot for. Without this the fault is only ever
                // reachable by a leg that had nothing to lose.
                for m in scripted {
                    if from_tx.send(m).await.is_err() {
                        break;
                    }
                }
                recorded.parked_senders.lock().unwrap().push(from_tx);
                return Ok(UpstreamChannels {
                    to_upstream: to_tx,
                    from_upstream: from_rx,
                    pump: None,
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
                    if discard_writes {
                        // Taken off the channel and thrown away: zero bytes reached the
                        // app-server, and the message is dropped whole rather than written
                        // or acknowledged.
                        drop(m);
                        continue;
                    }
                    if failed_writes {
                        // The write was attempted and the socket refused it: the pump
                        // answers `false`. Nothing is recorded, because nothing went out.
                        if let Some(ack) = m.ack {
                            let _ = ack.send(false);
                        }
                        continue;
                    }
                    // The fake IS the pump: it writes the message, then acknowledges the
                    // write. Recording it is this harness's stand-in for the bytes
                    // reaching the app-server.
                    match m.msg {
                        Message::Text(t) => {
                            // Answer the FIRST matching trigger, once (a real app-server
                            // answers each request exactly once).
                            if let Some(i) = replies.iter().position(|(trig, _)| t.contains(trig)) {
                                let (_, frame) = replies.remove(i);
                                if from_tx.send(frame).await.is_err() {
                                    return;
                                }
                                while replies.get(i).is_some_and(|(t, _)| t == FOLLOWS) {
                                    let (_, more) = replies.remove(i);
                                    if from_tx.send(more).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            recorded.recorded.lock().unwrap().push(t);
                        }
                        Message::Binary(b) => recorded.recorded_binary.lock().unwrap().push(b),
                        _ => {}
                    }
                    if let Some(ack) = m.ack {
                        let _ = ack.send(true);
                    }
                }
                drop(from_tx);
            });
            Ok(UpstreamChannels {
                to_upstream: to_tx,
                from_upstream: from_rx,
                // The scripted drain ends when the channel closes; there is no pump.
                pump: None,
            })
        })
    }
}

/// **The PRODUCTION pump, over a transport that takes the approval and then never reads
/// again.**
///
/// Every other test in this file drives [`FakeFactory`], which stands in for the pump: it
/// takes an [`UpstreamWrite`] off the channel and answers the receipt itself, so
/// `codex_broker::upstream::pump` and its real `sink.send` are never executed. That is
/// exactly the seam that matters: the bound the broker needs is a bound on a `sink.send`
/// parked against a socket that has stopped draining, and no fake can produce one.
///
/// So this factory builds the real thing. A `tokio::io::duplex(1)` gives a one-byte pipe;
/// the near half is handed to the production `pump`, the far half is a WebSocket that
/// SENDS the scripted approval (so the leg has a capability to answer) and is then parked
/// without ever reading. The first answer the relay forwards therefore blocks inside
/// `sink.send` for ever, which is the fault.
///
/// `queue` is the capacity of the c2s channel in front of that parked pump — production's
/// is 64. Narrowing it to one is how a test reaches the *second* fault on the same path:
/// a queue that is full while the pump is stuck, so the answer cannot even be handed over.
///
/// The FIRST connection's peer is the one exception to "never reads": it reads one frame
/// and answers it with `creation_response("th-A")`, so a keyboard leg can make `th-A` the
/// head that the phone's approval has to be on.
struct StalledUpstreamFactory {
    /// The far ends, kept alive so the writes block instead of erroring.
    parked: Arc<Mutex<Vec<WebSocketStream<tokio::io::DuplexStream>>>>,
    queue: usize,
    /// Whether the first connection has been made.
    connected: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for StalledUpstreamFactory {
    fn default() -> Self {
        Self {
            parked: Arc::default(),
            queue: 64,
            connected: Arc::default(),
        }
    }
}

impl HarnessFactory for StalledUpstreamFactory {
    type Fac = StalledUpstreamFactory;
    fn build(self, _state: &Arc<FakeState>) -> StalledUpstreamFactory {
        self
    }
}

impl UpstreamFactory for StalledUpstreamFactory {
    fn connect(&self) -> ConnectFuture {
        let parked = Arc::clone(&self.parked);
        let queue = self.queue;
        let first = !self.connected.swap(true, Ordering::SeqCst);
        Box::pin(async move {
            use tokio_tungstenite::tungstenite::protocol::Role as WsRole;
            let (near, far) = tokio::io::duplex(1);
            let ws = WebSocketStream::from_raw_socket(near, WsRole::Client, None).await;
            let mut peer = WebSocketStream::from_raw_socket(far, WsRole::Server, None).await;
            let (to_tx, to_rx) = tokio::sync::mpsc::channel::<UpstreamWrite>(queue);
            let (from_tx, from_rx) = tokio::sync::mpsc::channel::<Message>(64);
            // The real pump, started the way `WsUdsUpstreamFactory` starts one — handle
            // kept, so this leg's teardown ends it exactly as production's does.
            let pump = codex_broker::upstream::spawn_pump(ws, to_rx, from_tx);
            // One approval, so the leg holds a capability. Sent before the peer is parked;
            // the pump's inbound half reads it, so this does not block.
            peer.send(approval(COMMAND_EXEC_APPROVAL, "th-A", 0))
                .await
                .expect("the parked peer can still write");
            if first {
                tokio::spawn(async move {
                    let _ = peer.next().await;
                    peer.send(creation_response("th-A"))
                        .await
                        .expect("the peer answers the keyboard's move");
                    parked.lock().unwrap().push(peer);
                });
            } else {
                // …and from here it reads nothing, ever.
                parked.lock().unwrap().push(peer);
            }
            Ok(UpstreamChannels {
                to_upstream: to_tx,
                from_upstream: from_rx,
                pump: Some(pump),
            })
        })
    }
}

/// The workspace the fake app-server reports a created thread in.
const LAUNCH_CWD: &str = "/work/proj";

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
    /// first `to_upstream.send` on that leg fails.
    fn push_dead_upstream(&self, dead: bool) {
        self.state.dead_upstreams.lock().unwrap().push_back(dead);
    }

    /// Make the **next** upstream connection accept every admitted message and write
    /// none of it. See [`FakeState::discard_upstreams`].
    fn push_discard_upstream(&self, discard: bool) {
        self.state
            .discard_upstreams
            .lock()
            .unwrap()
            .push_back(discard);
    }

    /// Make the **next** upstream connection answer every acknowledged write `false` —
    /// a write the socket provably refused. See [`FakeState::failed_writes`].
    fn push_failed_write(&self, failed: bool) {
        self.state.failed_writes.lock().unwrap().push_back(failed);
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

/// A broker whose upstream is the **production pump** over a transport that never
/// reads, with a short write budget so the bound can be watched expiring.
fn start_broker_stalled(budget: Duration) -> Harness {
    start_broker_with(false, Some(budget), StalledUpstreamFactory::default())
}

/// The same, with the c2s queue in front of the parked pump narrowed to `queue` slots,
/// so a test can fill it and reach the hand-off itself.
fn start_broker_stalled_behind_a_full_queue(budget: Duration, queue: usize) -> Harness {
    start_broker_with(
        false,
        Some(budget),
        StalledUpstreamFactory {
            queue,
            ..Default::default()
        },
    )
}

fn start_broker_inner(record_events: bool) -> Harness {
    start_broker_with(record_events, None, ())
}

/// What a harness needs from the factory it is built over.
trait HarnessFactory {
    type Fac: UpstreamFactory;
    fn build(self, state: &Arc<FakeState>) -> Self::Fac;
}

impl HarnessFactory for () {
    type Fac = FakeFactory;
    fn build(self, state: &Arc<FakeState>) -> FakeFactory {
        FakeFactory {
            inner: Arc::clone(state),
        }
    }
}

fn start_broker_with<H: HarnessFactory>(
    record_events: bool,
    budget: Option<Duration>,
    which: H,
) -> Harness {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    // Short paths: SUN_LEN caps a UDS path at 103 bytes, so /tmp, not the long
    // scratchpad path.
    let tui_sock = format!("/tmp/ccb-{pid}-{n}-t.sock");
    let ccd_sock = format!("/tmp/ccb-{pid}-{n}-c.sock");
    let _ = std::fs::remove_file(&tui_sock);
    let _ = std::fs::remove_file(&ccd_sock);

    let state = Arc::new(FakeState::default());
    let factory = which.build(&state);
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut broker = Broker::new(tui_sock.clone(), ccd_sock.clone(), factory);
    if let Some(budget) = budget {
        broker = broker.with_upstream_write_budget(budget);
    }
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

/// Wait, bounded, for an audit line containing `needle`. Returns whether it appeared.
///
/// The confirmation of a write happens on the winning leg's own task, after its receipt
/// resolves. A test that needs the arbiter to be in its post-write state waits for the
/// line rather than for a duration.
async fn event_containing(h: &Harness, needle: &str) -> bool {
    for _ in 0..200 {
        if h.events().iter().any(|e| e.contains(needle)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
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

/// Read whatever a leg still has for us, then say whether it was closed.
///
/// Bounded: a leg that is neither closed nor talking within the window is reported as
/// still open rather than hanging the suite, so a test asserting a close fails with
/// what it saw instead of timing out.
async fn frames_until_closed(ws: &mut WebSocketStream<UnixStream>) -> (Vec<String>, bool) {
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
            Err(_) => return (seen, false),
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return (seen, true),
            Ok(Some(Ok(m))) => seen.push(m.to_text().unwrap_or_default().to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_phone_leg_forwards_an_allowlisted_read_and_refuses_a_bypass_with_zero_bytes() {
    let h = start_broker();
    let mut ws = connect(&h.ccd_sock).await;

    ws.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":1,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        r#"{"method":"command/exec","id":2,"params":{"cmd":"rm -rf /"}}"#.into(),
    ))
    .await
    .unwrap();

    // The refused bypass produces exactly one frame back to the client: a synthetic error.
    let v = next_frame(&mut ws).await;
    assert_eq!(v["id"], 2);
    assert_eq!(v["error"]["code"], -32001);

    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert_eq!(rec.len(), 1, "only the allowlisted read forwards");
    assert!(rec[0].contains("thread/loaded/list"));
    assert!(
        !rec.iter().any(|m| m.contains("command/exec")),
        "command/exec must reach zero upstream bytes",
    );
}

#[tokio::test]
async fn the_phone_may_not_create_a_thread() {
    let h = start_broker();
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(
        r#"{"method":"thread/start","id":7,"params":{"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":"read-only"}}"#.into(),
    ))
    .await
    .unwrap();
    let v = next_frame(&mut ccd).await;
    assert_eq!(v["id"], 7);
    assert_eq!(v["error"]["code"], -32001);
    settle().await;
    assert!(h.state.recorded.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// The head the phone acts on follows the keyboard, end to end over the relay.
// ---------------------------------------------------------------------------

const CREATION_REQUEST: &str = r#"{"method":"thread/start","id":"start-1","params":{"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":"read-only"}}"#;

/// The answer the fake app-server gives `CREATION_REQUEST`, in the shape measured off a
/// real codex 0.147 server.
fn creation_response(thread: &str) -> Message {
    Message::Text(
        serde_json::json!({
            "id": "start-1",
            "result": {
                "thread": {"id": thread, "path": "/x"},
                "cwd": LAUNCH_CWD,
                "runtimeWorkspaceRoots": [LAUNCH_CWD]
            }
        })
        .to_string(),
    )
}

/// Drive one keyboard leg through its `thread/start` and await the answer, so the head
/// has moved before the caller's next message.
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

/// Bind `thread` as the head through a keyboard leg of its own, then clear what that
/// setup recorded, so the caller counts only its own traffic.
///
/// **Call it before pushing any other script or reply table**: this leg is the next
/// upstream connection, and it takes the next of each.
async fn keyboard_on(h: &Harness, thread: &str) -> WebSocketStream<UnixStream> {
    h.push_script(vec![]);
    h.push_replies(vec![("thread/start".into(), creation_response(thread))]);
    let mut ws = connect(&h.tui_sock).await;
    create_thread(&mut ws, thread).await;
    recorded_after(&h.state, 1).await;
    h.state.recorded.lock().unwrap().clear();
    ws
}

#[tokio::test]
async fn a_phone_turn_with_no_head_is_refused() {
    let h = start_broker();
    let mut ccd = connect(&h.ccd_sock).await;
    assert_eq!(
        phone_turn_outcome(&h, &mut ccd, "01a0-ours", 3)
            .await
            .as_deref(),
        Some(NOT_THE_HEAD)
    );
    assert!(h.state.recorded.lock().unwrap().is_empty());
}

/// An announcement is not an answer: a `thread/started` naming a thread moves nothing,
/// even while the keyboard's own creation is in flight.
#[tokio::test]
async fn a_bare_thread_started_moves_nothing_over_the_relay() {
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
    assert_eq!(next_frame(&mut ws).await["method"], "thread/started");

    let mut ccd = connect(&h.ccd_sock).await;
    assert!(phone_turn_outcome(&h, &mut ccd, "01a0-ours", 3)
        .await
        .is_some());
}

/// The measured `/new` switch, end to end: unsubscribe ×2, a second `thread/start`, and
/// the head follows to B; the phone may start a turn there, and not on the thread the
/// keyboard left.
#[tokio::test]
async fn the_new_switch_flows_end_to_end_over_the_relay() {
    let h = start_broker();
    h.push_replies(vec![
        ("\"start-1\"".into(), creation_response("01a0-a")),
        (
            "thread/unsubscribe".into(),
            Message::Text(r#"{"id":7,"result":{"status":"unsubscribed"}}"#.into()),
        ),
        (
            "thread/unsubscribe".into(),
            Message::Text(r#"{"id":8,"result":{"status":"unsubscribed"}}"#.into()),
        ),
        (
            "\"start-2\"".into(),
            Message::Text(
                serde_json::json!({"id": "start-2", "result": {"thread": {"id": "01a0-b"}}})
                    .to_string(),
            ),
        ),
    ]);
    let mut ws = connect(&h.tui_sock).await;
    create_thread(&mut ws, "01a0-a").await;
    for id in [7, 8] {
        ws.send(Message::Text(
            serde_json::json!({"method":"thread/unsubscribe","id":id,
                               "params":{"threadId":"01a0-a"}})
            .to_string(),
        ))
        .await
        .unwrap();
        assert_eq!(
            next_frame(&mut ws).await["result"]["status"],
            "unsubscribed"
        );
    }
    ws.send(Message::Text(
        CREATION_REQUEST.replace("start-1", "start-2"),
    ))
    .await
    .unwrap();
    assert_eq!(
        next_frame(&mut ws).await["result"]["thread"]["id"],
        "01a0-b"
    );

    let mut ccd = connect(&h.ccd_sock).await;
    assert_eq!(phone_turn_outcome(&h, &mut ccd, "01a0-b", 21).await, None);
    assert_eq!(
        phone_turn_outcome(&h, &mut ccd, "01a0-a", 22)
            .await
            .as_deref(),
        Some(NOT_THE_HEAD)
    );
}

/// A TUI launched as `codex resume` or `codex fork` opens with that request, not a
/// `thread/start`: its answer binds the head, and the phone may start a turn there.
#[tokio::test]
async fn a_tui_that_opens_with_resume_or_fork_binds_the_head() {
    for method in ["thread/resume", "thread/fork"] {
        let h = start_broker();
        h.push_replies(vec![(
            method.into(),
            Message::Text(
                serde_json::json!({"id": "open-1", "result": {"thread": {"id": "01a0-old"}}})
                    .to_string(),
            ),
        )]);
        let mut ws = connect(&h.tui_sock).await;
        ws.send(Message::Text(
            serde_json::json!({"method": method, "id": "open-1",
                               "params": {"threadId": "01a0-old"}})
            .to_string(),
        ))
        .await
        .unwrap();
        assert_eq!(
            next_frame(&mut ws).await["result"]["thread"]["id"],
            "01a0-old"
        );

        let mut ccd = connect(&h.ccd_sock).await;
        assert_eq!(
            phone_turn_outcome(&h, &mut ccd, "01a0-old", 31).await,
            None,
            "{method}: the phone follows the thread the TUI opened"
        );
    }
}

/// Correlation is by connection: another keyboard connection's frame carrying the first
/// one's request id settles nothing.
#[tokio::test]
async fn a_second_tui_connection_cannot_answer_the_first_ones_move() {
    let h = start_broker();
    h.push_script(vec![]); // connection A's upstream: silent
    h.push_script(vec![creation_response("attacker-thread")]); // connection B's upstream

    let mut a = connect(&h.tui_sock).await;
    a.send(Message::Text(CREATION_REQUEST.into()))
        .await
        .unwrap();
    recorded_after(&h.state, 1).await;
    let mut b = connect(&h.tui_sock).await;
    let forged = b.next().await.unwrap().unwrap();
    assert!(forged.to_text().unwrap().contains("attacker-thread"));
    settle().await;

    let mut ccd = connect(&h.ccd_sock).await;
    assert!(
        phone_turn_outcome(&h, &mut ccd, "attacker-thread", 3)
            .await
            .is_some(),
        "a sibling connection's response must move nothing"
    );
}

/// **A keyboard move whose bytes never left puts the head back.** Without the rollback,
/// the dead leg's close would find the move in flight and leave no head.
#[tokio::test]
async fn a_failed_upstream_send_rolls_the_move_back() {
    let h = start_broker();
    h.push_dead_upstream(false); // the keyboard that binds A
    h.push_dead_upstream(true); // a keyboard whose writes fail
    h.push_replies(vec![("thread/start".into(), creation_response("01a0-a"))]);
    let mut live = connect(&h.tui_sock).await;
    create_thread(&mut live, "01a0-a").await;

    let mut dead = connect(&h.tui_sock).await;
    dead.send(Message::Text(
        CREATION_REQUEST.replace("start-1", "start-2"),
    ))
    .await
    .unwrap();
    let (_, closed) = frames_until_closed(&mut dead).await;
    assert!(closed, "a dead upstream closes the leg");
    settle().await;

    let mut ccd = connect(&h.ccd_sock).await;
    assert_eq!(phone_turn_outcome(&h, &mut ccd, "01a0-a", 3).await, None);
}

#[tokio::test]
async fn ccd_resume_bound_to_session_thread() {
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "01a0-ours").await;

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
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1);
    assert!(rec[0].contains("01a0-ours"));
}

#[tokio::test]
async fn the_audit_log_carries_connection_scoped_open_and_close_markers() {
    // Every accepted connection announces itself with a conn-scoped OPEN marker and reports
    // its end with the same id, so a `broker.log` reader can attribute lines to one
    // connection. The markers live gates assert on — `Tui: forward (`,
    // `broker: listening on tui.sock and ccd.sock`, `Ccd leg ended` — are kept.
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
    for opened in ["Tui: leg opened (conn ", "Ccd: leg opened (conn "] {
        assert!(
            events.iter().any(|e| e.starts_with(opened)),
            "a conn-scoped OPEN marker: {events:?}"
        );
    }
    assert!(
        events
            .iter()
            .any(|e| e.starts_with("Tui: forward (app/list) (conn ")),
        "the keyboard's forward marker names its method: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.contains("Ccd leg ended") && e.contains("(conn ")),
        "the close marker keeps `Ccd leg ended` and gains the conn id: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// The phone leg's outstanding-request-id ledger, end to end over the relay.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_request_reusing_an_in_flight_id_forwards_zero_bytes_and_keeps_the_leg_open() {
    // A client that pipelines two live requests under ONE id has made its own responses
    // uncorrelatable. The frame is DROPPED (zero upstream bytes), the leg stays OPEN, and
    // the event is logged for the failure-containment seam.
    let h = start_broker_with_events();
    let mut ws = connect(&h.ccd_sock).await;
    ws.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":1,"params":{"tag":"first"}}"#.into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1, "the first request forwards");

    ws.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":1,"params":{"tag":"second"}}"#.into(),
    ))
    .await
    .unwrap();
    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert_eq!(rec.len(), 1, "a reused in-flight id forwards zero bytes");

    ws.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":2,"params":{"tag":"third"}}"#.into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 2).await;
    assert_eq!(rec.len(), 2);
    assert!(rec[1].contains("third"), "{rec:?}");

    assert!(
        h.events().iter().any(|e| {
            e.starts_with("Ccd: drop, keep open (")
                && e.contains("already outstanding")
                && e.contains("(conn ")
        }),
        "the reused-id drop must be logged and conn-scoped: {:?}",
        h.events()
    );
}

#[tokio::test]
async fn an_answered_id_is_usable_again_over_the_relay() {
    let h = start_broker();
    h.push_replies(vec![(
        r#""tag":"first""#.into(),
        Message::Text(r#"{"id":1,"result":{"data":[]}}"#.into()),
    )]);
    let mut ws = connect(&h.ccd_sock).await;
    ws.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":1,"params":{"tag":"first"}}"#.into(),
    ))
    .await
    .unwrap();
    assert_eq!(next_frame(&mut ws).await["id"], 1);

    ws.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":1,"params":{"tag":"second"}}"#.into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 2).await;
    assert_eq!(rec.len(), 2, "an answered id is free again: {rec:?}");
    assert!(rec[1].contains("second"));
}

#[tokio::test]
async fn an_over_long_request_id_forwards_zero_bytes() {
    let h = start_broker_with_events();
    let mut ws = connect(&h.ccd_sock).await;
    let long = "x".repeat(4096);
    ws.send(Message::Text(format!(
        r#"{{"method":"thread/loaded/list","id":"{long}","params":{{}}}}"#
    )))
    .await
    .unwrap();
    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "an over-long id must forward zero bytes",
    );
    let drop_line = h
        .events()
        .into_iter()
        .find(|e| e.contains("drop, keep open") && e.contains("byte cap"))
        .expect("the over-long id drop is audited");
    assert!(!drop_line.contains(&long), "the id leaked: {drop_line}");

    ws.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":1,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1, "the leg stayed usable");
}

#[tokio::test]
async fn every_forward_note_is_scoped_to_its_connection() {
    // A gate must be able to pair a forward with the connection that made it. The EXACT
    // substrings the live gates assert are kept and the conn id is APPENDED after the
    // closing paren of the note, never spliced into it.
    let h = start_broker_with_events();
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":1,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    ccd.send(Message::Text(
        r#"{"method":"initialized","params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let _ = recorded_after(&h.state, 2).await;

    let mut tui = connect(&h.tui_sock).await;
    tui.send(Message::Text(CREATION_REQUEST.into()))
        .await
        .unwrap();
    let _ = recorded_after(&h.state, 3).await;
    settle().await;

    let events = h.events();
    for marker in [
        "Ccd: forward (request allowlisted)",
        "Ccd: forward (notification allowlisted)",
        "Tui: forward (thread/start)",
    ] {
        assert!(
            events.iter().any(|e| e.contains(marker)),
            "the live-gate substring {marker:?} must survive verbatim: {events:?}"
        );
    }
    let forwards: Vec<&String> = events
        .iter()
        .filter(|e| e.contains(": forward ("))
        .collect();
    assert!(forwards.len() >= 3, "{events:?}");
    for line in &forwards {
        assert!(
            line.ends_with(')') && line.contains(") (conn "),
            "a forward note must carry its conn id, appended: {line}"
        );
    }
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
    // ~6 MB message (the plugin/list worst-case class).
    let big = "A".repeat(6 * 1024 * 1024);
    let msg = format!(r#"{{"method":"app/list","id":1,"params":{{"pad":"{big}"}}}}"#);
    ws.send(Message::Text(msg.clone())).await.unwrap();

    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].len(), msg.len(), "multi-MB message relayed whole");
    assert_eq!(rec[0], msg, "and byte-exact");
}

#[tokio::test]
async fn a_duplicate_key_frame_from_the_phone_forwards_zero_bytes_and_closes() {
    let h = start_broker();
    let mut ws = connect(&h.ccd_sock).await;
    // A frame with a duplicated key: our parse keeps one, the app-server might keep the
    // other. Reject: zero upstream bytes, leg closed.
    ws.send(Message::Text(
        r#"{"method":"thread/resume","id":1,"params":{"threadId":"a","threadId":"b"}}"#.into(),
    ))
    .await
    .unwrap();
    let (_, closed) = frames_until_closed(&mut ws).await;
    assert!(closed, "duplicate-key frame must close the leg");
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "duplicate-key frame must forward zero bytes",
    );
}

/// The phone's role is anchored to its socket: a second `initialize` closes its leg. The
/// keyboard's second `initialize` is its own business and goes through.
#[tokio::test]
async fn reinitialization_closes_the_phone_leg_only() {
    let h = start_broker();
    let mut ws = connect(&h.ccd_sock).await;
    ws.send(Message::Text(
        r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"x","version":"1"}}}"#
            .into(),
    ))
    .await
    .unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert!(rec[0].contains("initialize"));
    ws.send(Message::Text(
        r#"{"method":"initialize","id":2,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let (_, closed) = frames_until_closed(&mut ws).await;
    assert!(closed, "reinitialization must close the phone's leg");

    let mut tui = connect(&h.tui_sock).await;
    for id in [1, 2] {
        tui.send(Message::Text(format!(
            r#"{{"method":"initialize","id":{id},"params":{{}}}}"#
        )))
        .await
        .unwrap();
    }
    let rec = recorded_after(&h.state, 3).await;
    assert_eq!(rec.len(), 3, "both keyboard initializes forward: {rec:?}");
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

/// A notification scripted after a withheld request: the leg receiving it proves the
/// broker has finished with every frame scripted before it.
fn barrier() -> Message {
    Message::Text(r#"{"method":"thread/status/changed","params":{"threadId":"th-A"}}"#.into())
}

async fn drain_barrier(ws: &mut WebSocketStream<UnixStream>) {
    assert_eq!(next_frame(ws).await["method"], "thread/status/changed");
}

#[tokio::test]
async fn fanout_phone_family_ccd_wins_tui_sibling_revoked() {
    let h = start_broker_with_events();
    let _keyboard = keyboard_on(&h, "th-A").await;
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
    let _keyboard = keyboard_on(&h, "th-A").await;
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
async fn duplicate_response_on_same_leg_is_one_use() {
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-A").await;
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

/// **The winner is told, in the shape a reader can file.**
///
/// A `ccd` leg that answers an approval cannot otherwise observe whether its bytes
/// went upstream: the arbiter forwards one sibling and drops the rest with no reply
/// and no close. Measured on real 0.153 (`measure_a_ccd_leg_answering_a_command_approval`,
/// `measure_what_a_losing_ccd_response_is_told`): the winner's command ran and the
/// loser received zero frames. This is the frame that tells the two apart.
///
/// **Mutation:** drop the `if let Some(id) = answered` block at the end of
/// `handle_text` and the disposition never arrives, so the daemon can only ever
/// record a phone answer as unknown.
#[tokio::test]
async fn a_ccd_answer_that_forwards_is_told_it_was_delivered() {
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-A").await;
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]);
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    ccd.send(answer(0, "phone")).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(
        rec.len(),
        1,
        "the winning answer forwards its original bytes"
    );

    let told = next_frame(&mut ccd).await;
    assert_eq!(told["method"], "codeconnect/responseDisposition");
    assert_eq!(told["params"]["delivered"], serde_json::json!(true));
    // Named the way the reader files its cards: the bare id alone identifies the
    // request only on this connection.
    assert_eq!(told["params"]["threadId"], "th-A");
    assert_eq!(told["params"]["requestId"], serde_json::json!(0));
}

/// **And so is the loser, with the opposite bit.**
///
/// The TUI answers first, so the `ccd` sibling forwards zero bytes. `delivered:false`
/// is a statement about bytes, and it is what lets a daemon report an answer that
/// provably never left as not-taken rather than as unknown.
///
/// **Mutation:** compute `delivered` from anything other than the `Forward` action —
/// e.g. hard-code `true` — and the losing leg is told its answer landed, which is the
/// one lie this frame exists to prevent.
#[tokio::test]
async fn a_ccd_answer_that_loses_the_race_is_told_it_was_not_delivered() {
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-A").await;
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // tui leg
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // ccd leg
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    tui.send(answer(0, "keyboard")).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1);
    assert!(rec[0].contains(r#""by":"keyboard""#));

    ccd.send(answer(0, "phone")).await.unwrap();
    let told = next_frame(&mut ccd).await;
    assert_eq!(told["method"], "codeconnect/responseDisposition");
    assert_eq!(told["params"]["delivered"], serde_json::json!(false));
    assert!(
        told["params"].get("cause").is_none(),
        "a leg that never forwarded has no write of its own to explain: {told}"
    );
    assert_eq!(
        h.state.recorded.lock().unwrap().len(),
        1,
        "the losing answer still forwards zero bytes"
    );

    // And the slot the keyboard CONFIRMED stays spent, for every leg including its own:
    // an answer that actuated may never be sent a second time. This is the negative that
    // makes the release in `a_write_the_socket_refused_releases_the_slot_for_the_keyboard`
    // a statement about proven failure rather than a general re-opening.
    tui.send(answer(0, "keyboard-again")).await.unwrap();
    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().len(),
        1,
        "a confirmed win is one-use, and stays one-use"
    );
}

/// **`delivered:true` must mean the bytes were WRITTEN, not merely handed off.**
///
/// The forward path hands the answer to an in-process channel; the socket write
/// happens later, in the upstream pump. A leg told `delivered:true` on the hand-off
/// alone is told an answer landed whenever the pump dies between the two — and the
/// daemon durably records a resolution that wrote zero bytes, which is the one claim
/// this frame exists to make impossible.
///
/// The fake takes the message off the channel and throws it away without answering for
/// it, which is what a pump that died mid-write looks like from the relay's side: the
/// hand-off succeeded and no receipt ever came back. That is `unconfirmed` — see
/// [`a_write_the_socket_refused_releases_the_slot_for_the_keyboard`] for the other,
/// PROVEN failure, which is a different disposition and a different arbiter outcome.
///
/// **Mutation:** derive `delivered` from the `Forward` action instead of from the
/// pump's acknowledgement, and this leg is told its answer landed when nothing
/// acknowledged reaching the app-server.
#[tokio::test]
async fn a_ccd_answer_whose_upstream_write_never_completed_is_told_it_was_not_delivered() {
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-A").await;
    h.push_discard_upstream(true);
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]);
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    ccd.send(answer(0, "phone")).await.unwrap();

    let told = next_frame(&mut ccd).await;
    assert_eq!(told["method"], "codeconnect/responseDisposition");
    assert_eq!(told["params"]["threadId"], "th-A");
    assert_eq!(told["params"]["requestId"], serde_json::json!(0));
    assert_eq!(
        told["params"]["delivered"],
        serde_json::json!(false),
        "an answer the pump never wrote reached the app-server as zero bytes"
    );
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "nothing was written upstream"
    );
}

/// **A write the socket refused releases the slot: the keyboard can still answer.**
///
/// The arbiter used to record the winner at the moment a leg was *authorized*, before any
/// I/O. That made "first authorization wins" the rule instead of "first successfully
/// written answer wins": a phone answer that won arbitration and then failed its
/// `ws.send` consumed the request for ever. Nobody had answered, so no
/// `serverRequest/resolved` would ever follow — and the keyboard sitting in front of the
/// same approval could no longer answer it either, because the slot was spent.
///
/// The reservation is therefore released on a **proven** failed send, and the disposition
/// says why with `cause:"write_failed"` so the daemon can tell it from a lost race.
///
/// **Mutation:** record the winner in `consume` and never release it (an arbiter
/// that confirms on authorization) and the keyboard's answer forwards zero bytes.
#[tokio::test]
async fn a_write_the_socket_refused_releases_the_slot_for_the_keyboard() {
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-A").await;
    // The ccd leg connects first, so the failed-write upstream is its own.
    h.push_failed_write(true);
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // ccd leg
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // tui leg
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;

    ccd.send(answer(0, "phone")).await.unwrap();
    let told = next_frame(&mut ccd).await;
    assert_eq!(told["method"], "codeconnect/responseDisposition");
    assert_eq!(told["params"]["delivered"], serde_json::json!(false));
    assert_eq!(
        told["params"]["cause"], "write_failed",
        "a refused write is reported as a refused write, not as a lost race: {told}"
    );
    assert!(
        told["params"].get("winner").is_none(),
        "nobody won, so no winner may be named: {told}"
    );

    // The whole point: the approval is still answerable.
    tui.send(answer(0, "keyboard")).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(
        rec.len(),
        1,
        "the released slot must let the keyboard answer, saw {rec:?}"
    );
    assert!(rec[0].contains(r#""by":"keyboard""#), "{rec:?}");
}

/// **The hand-off is the OTHER place a write can be refused, and it releases the slot
/// for the same reason.**
///
/// An admitted answer reaches the socket in two steps, and only the second one is a
/// write. The first is the hand-off to the pump's channel, and when THAT fails the pump
/// is already gone: the envelope was never taken off the queue, so it was never fed to
/// `sink.send` at all. Zero bytes, and the failure itself is the proof — a stronger
/// proof than the socket refusal that shares the `write_failed` word, which only says
/// the write did not complete.
///
/// The leg is torn down either way, because its upstream is gone. The **session** is
/// not: the arbiter is shared across legs, so a reservation left standing by a leg that
/// provably wrote nothing spends the approval for the keyboard sitting in front of the
/// same prompt — nobody answered, no `serverRequest/resolved` is coming, and the card
/// can no longer be answered by anyone.
///
/// The creation rollback beside it has always drawn exactly this conclusion from exactly
/// this failure ([`a_failed_upstream_send_rolls_the_creation_claim_back`]); the
/// reservation is the second claim the same hand-off can leave behind.
///
/// **Mutation:** drop the release from the failed-hand-off arm and the keyboard's answer
/// forwards zero bytes. Drop the disposition and this leg is told nothing at all about
/// an answer whose fate is fully known.
#[tokio::test]
async fn a_write_the_channel_refused_releases_the_slot_for_the_keyboard() {
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-A").await;
    // The ccd leg connects first, so the dead write side is its own.
    h.push_dead_upstream(true); // ccd leg: the hand-off fails
    h.push_dead_upstream(false); // tui leg: healthy
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // ccd leg
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // tui leg
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;

    ccd.send(answer(0, "phone")).await.unwrap();
    let told = next_frame(&mut ccd).await;
    assert_eq!(told["method"], "codeconnect/responseDisposition");
    assert_eq!(told["params"]["delivered"], serde_json::json!(false));
    assert_eq!(
        told["params"]["cause"], "write_failed",
        "an envelope the pump's channel never took is a proven zero-byte write: {told}"
    );
    assert!(
        told["params"].get("winner").is_none(),
        "nobody won, so no winner may be named: {told}"
    );

    // The whole point: the approval is still answerable.
    tui.send(answer(0, "keyboard")).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(
        rec.len(),
        1,
        "the released slot must let the keyboard answer, saw {rec:?}"
    );
    assert!(rec[0].contains(r#""by":"keyboard""#), "{rec:?}");
}

/// **A dropped receipt is unconfirmed, and unconfirmed is not released.**
///
/// The pump was cancelled or died between taking the message and answering for it. That
/// proves only that this broker cannot say whether the bytes went out — tungstenite's
/// `send` is a feed plus a flush, so a partial write is a real state. Releasing the slot
/// on that would let a second answer actuate a command the first one may already have
/// actuated, which is strictly worse than leaving the approval unanswerable.
///
/// So the reservation stands, the disposition says `cause:"unconfirmed"`, and the
/// keyboard's later answer forwards zero bytes.
///
/// **Mutation:** release on a dropped receipt as well as on a proven failure, and the
/// keyboard's answer forwards a second time.
#[tokio::test]
async fn a_dropped_receipt_is_unconfirmed_and_keeps_the_slot_consumed() {
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-A").await;
    h.push_discard_upstream(true);
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // ccd leg
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // tui leg
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;

    ccd.send(answer(0, "phone")).await.unwrap();
    let told = next_frame(&mut ccd).await;
    assert_eq!(told["params"]["delivered"], serde_json::json!(false));
    assert_eq!(
        told["params"]["cause"], "unconfirmed",
        "a dropped receipt proves nothing about the bytes: {told}"
    );
    assert!(
        told["params"].get("winner").is_none(),
        "no winner is confirmed, so none is named: {told}"
    );

    tui.send(answer(0, "keyboard")).await.unwrap();
    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "an unconfirmed answer keeps the slot consumed: {:?}",
        h.state.recorded.lock().unwrap()
    );
}

/// **A confirmed win stays consumed, and the loser is told who won.**
///
/// The complement of the two above: when the winner's write really did complete, no
/// later answer may forward, and the disposition names the confirmed winner rather than
/// carrying a `cause`.
///
/// The loser's answer is sent only after the winner's confirmation is in the audit log,
/// so the assertion is about the arbiter's state and not about task scheduling.
///
/// **Mutation:** name the winner from a reservation rather than from a confirmation and
/// the second assertion below passes before the write has been proven.
#[tokio::test]
async fn a_confirmed_win_stays_consumed_and_names_its_winner() {
    let h = start_broker_with_events();
    let _keyboard = keyboard_on(&h, "th-A").await;
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // tui leg
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // ccd leg
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    tui.send(answer(0, "keyboard")).await.unwrap();
    let confirmed = event_containing(&h, "capability confirmed: winner=Tui").await;
    assert!(
        confirmed,
        "the keyboard's write was never confirmed: {:?}",
        h.events()
    );

    ccd.send(answer(0, "phone")).await.unwrap();
    let told = next_frame(&mut ccd).await;
    assert_eq!(told["params"]["delivered"], serde_json::json!(false));
    assert_eq!(told["params"]["winner"], "tui", "{told}");
    assert!(
        told["params"].get("cause").is_none(),
        "a lost race carries no write cause: {told}"
    );

    tui.send(answer(0, "keyboard-again")).await.unwrap();
    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().len(),
        1,
        "a confirmed slot is spent for every leg, including the winner's own"
    );
}

/// **The acknowledged write is BOUNDED, and its expiry is session-fatal.**
///
/// Neither `sink.send` nor the receipt await had a deadline, and
/// `handle_connection` awaits `handle_text` inline — so a `sink.send` parked on an
/// app-server socket that had stopped draining parked the whole relay task with it. The
/// leg could not notice its own client closing, could not read another frame, and the
/// `ccd` daemon on the other end was left to run out its own 15-second
/// `DISPOSITION_BUDGET` with the card still on somebody's phone.
///
/// This is the ONE test that runs the production `upstream::pump` — see
/// [`StalledUpstreamFactory`] for why the ordinary fake cannot produce this fault at all.
/// The write really enters `sink.send`, really blocks on a one-byte pipe nobody drains,
/// and the broker's own bound is what ends it.
///
/// Two things are then required, and they are the whole contract:
///
///   * the daemon is TOLD, `cause:"unconfirmed"` — not proven-failed, because a write
///     that entered `sink.send` may have been partly flushed, and not silence, because
///     silence is what the daemon would have had to time out on.
///   * the leg is CLOSED. A write this broker cannot account for means the upstream is no
///     longer behaving like the app-server; tearing the leg down takes the upstream with
///     it (its `to_upstream` sender drops, which ends the pump), which is the existing
///     session-fatal contract for an upstream that has stopped being one.
///
/// **Mutation:** delete the `tokio::time::timeout` around the receipt in `handle_text`
/// and this test hangs at `next_frame`'s own five-second bound — which is precisely the
/// hang it exists to prove is gone.
#[tokio::test]
async fn an_upstream_write_that_never_completes_is_bounded_and_closes_the_leg() {
    let h = start_broker_stalled(Duration::from_millis(300));
    let mut keyboard = connect(&h.tui_sock).await;
    drain_approval(&mut keyboard).await;
    create_thread(&mut keyboard, "th-A").await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    ccd.send(answer(0, "phone")).await.unwrap();

    let told = next_frame(&mut ccd).await;
    assert_eq!(told["method"], "codeconnect/responseDisposition");
    assert_eq!(told["params"]["delivered"], serde_json::json!(false));
    assert_eq!(
        told["params"]["cause"], "unconfirmed",
        "a write the broker gave up waiting for is unproven, not proven-failed: {told}"
    );

    let (seen, closed) = frames_until_closed(&mut ccd).await;
    assert!(
        seen.is_empty(),
        "nothing follows the disposition on a leg being torn down: {seen:?}"
    );
    assert!(
        closed,
        "an upstream whose write cannot be accounted for is session-fatal"
    );
}

/// **The budget has to start before the hand-off, because the hand-off is where a stuck
/// pump is actually felt.**
///
/// A deadline that begins at the receipt measures only the second half of the answer's
/// journey. The first half is `to_upstream.send`, and it waits: the channel in front of
/// the pump is bounded, so a pump parked inside `sink.send` with a full queue behind it
/// parks the answer BEFORE its timer starts. Nothing then expires, the disposition is
/// never composed, and the leg — which awaits this inline — cannot read another frame or
/// notice its own client leaving. That is the same stall the receipt bound closed, one
/// step earlier on the same path, and it is reached by exactly the fault that makes the
/// receipt bound necessary.
///
/// So the deadline covers the hand-off and the receipt together, as one budget for one
/// answer. The staging is the production pump over a socket that stopped draining (see
/// [`StalledUpstreamFactory`]) with its queue narrowed to a single slot: two allowlisted
/// notifications, which wait for nothing, leave the pump blocked on the first and the
/// second sitting on the queue with nowhere to go. The answer behind them cannot be
/// handed over at all.
///
/// What is reported is `unconfirmed`, and that is the conservative reading rather than
/// the tightest one. Cancelling the send does leave the envelope unqueued, but the
/// budget is one budget: from outside, "the queue never took it" and "the pump never
/// answered for it" are one expiry, and the outcome that is safe under both is the one
/// that keeps the reservation. Releasing a slot for an answer that may have been written
/// is how the same command gets actuated twice.
///
/// **Mutation:** start the deadline at the receipt again (bound only the receipt await)
/// and this hangs at `next_frame`'s own five-second bound.
#[tokio::test]
async fn an_upstream_write_that_cannot_be_handed_over_is_bounded_and_closes_the_leg() {
    let h = start_broker_stalled_behind_a_full_queue(Duration::from_millis(300), 1);
    let mut keyboard = connect(&h.tui_sock).await;
    drain_approval(&mut keyboard).await;
    create_thread(&mut keyboard, "th-A").await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    // The first is taken off the queue and blocks in `sink.send` for ever; the second
    // fills the one slot behind it. Neither waits for a receipt, so both return.
    for _ in 0..2 {
        ccd.send(Message::Text(
            r#"{"method":"initialized","params":{}}"#.into(),
        ))
        .await
        .unwrap();
        settle().await;
    }

    ccd.send(answer(0, "phone")).await.unwrap();

    let told = next_frame(&mut ccd).await;
    assert_eq!(told["method"], "codeconnect/responseDisposition");
    assert_eq!(told["params"]["delivered"], serde_json::json!(false));
    assert_eq!(
        told["params"]["cause"], "unconfirmed",
        "an answer the queue never took is unproven, not proven-failed: {told}"
    );

    let (seen, closed) = frames_until_closed(&mut ccd).await;
    assert!(
        seen.is_empty(),
        "nothing follows the disposition on a leg being torn down: {seen:?}"
    );
    assert!(
        closed,
        "an upstream that cannot even take a message is session-fatal"
    );
}

/// **The `codeconnect/` namespace is the broker's alone, and it is enforced at the
/// origin.**
///
/// The s2c direction is otherwise an unconditional passthrough, so an app-server that
/// emitted `codeconnect/responseDisposition` for a pending request would have its frame
/// handed to the `ccd` leg verbatim — consumed as if this broker had said it, and the
/// genuine disposition, arriving after, discarded as a duplicate. That inverts the one
/// fact the leg cannot otherwise observe.
///
/// Dropping the frame silently would not be enough. This broker is the only author in
/// that namespace, so a frame wearing it from upstream is not traffic to filter, it is
/// evidence that the thing on the other end of the socket is not what this leg thinks
/// it is — and nothing further on that leg can be trusted either. It follows the
/// crate's existing rule for an s2c frame that cannot be trusted: poison the capability
/// view and close.
///
/// **Mutation:** delete the namespace gate in `observe_server_frame` and the forgery is
/// delivered to the leg, which then files it as this broker's own word.
#[tokio::test]
async fn an_upstream_frame_in_the_brokers_namespace_is_dropped_and_closes_the_leg() {
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-A").await;
    h.push_script(vec![
        approval(COMMAND_EXEC_APPROVAL, "th-A", 0),
        Message::Text(
            r#"{"method":"codeconnect/responseDisposition","params":{"threadId":"th-A","requestId":0,"delivered":true}}"#
                .into(),
        ),
    ]);
    let mut ccd = connect(&h.ccd_sock).await;
    // The approval proves the leg was alive and delivering right up to the forgery.
    drain_approval(&mut ccd).await;

    let (seen, closed) = frames_until_closed(&mut ccd).await;
    assert!(
        seen.is_empty(),
        "an upstream frame wearing `codeconnect/` must never reach the leg, saw {seen:?}"
    );
    assert!(closed, "the leg must be closed, not left open");
}

/// **`delivered:false` alone does not say who answered, so the winner is named.**
///
/// A daemon that reads every `false` as "answered at the Mac" is telling the phone a
/// story it made up: the loser may have lost to the keyboard, lost to a second phone,
/// or never held the capability at all. The arbiter already knows which leg consumed
/// the slot, so the loser is told the winner's ROLE and the daemon stops guessing.
///
/// **Mutation:** drop the `winner` field from the disposition and the daemon is back to
/// inferring the answerer from a bare `false`.
#[tokio::test]
async fn a_losing_ccd_answer_names_the_keyboard_that_won() {
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-A").await;
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // tui leg
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // ccd leg
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    tui.send(answer(0, "keyboard")).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1, "the keyboard's answer forwards");

    ccd.send(answer(0, "phone")).await.unwrap();
    let told = next_frame(&mut ccd).await;
    assert_eq!(told["method"], "codeconnect/responseDisposition");
    assert_eq!(told["params"]["delivered"], serde_json::json!(false));
    assert_eq!(
        told["params"]["winner"], "tui",
        "the keyboard answered first, and the phone is told exactly that"
    );
}

/// **And the winner is a role, not an assumption: another phone can be it.**
///
/// This is the case the daemon reads most wrongly with only a bare `false` to go on —
/// "answered at the Mac" when in fact a second `ccd` leg answered from another phone.
/// It also fixes the other half of the contract: the leg whose own bytes went out is
/// told `delivered:true` and told nothing about a winner, because the winner is itself
/// and there is nobody else to name.
///
/// **Mutation:** emit `winner` whenever the arbiter has one recorded, without the
/// "this leg did not forward" guard, and the winning leg is handed a `winner` naming
/// itself — a leg being told its own answer was overtaken.
#[tokio::test]
async fn a_losing_ccd_answer_names_the_other_phone_leg_that_won() {
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-A").await;
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]);
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]);
    let mut first = connect(&h.ccd_sock).await;
    drain_approval(&mut first).await;
    let mut second = connect(&h.ccd_sock).await;
    drain_approval(&mut second).await;

    first.send(answer(0, "phone-a")).await.unwrap();
    let won = next_frame(&mut first).await;
    assert_eq!(won["params"]["delivered"], serde_json::json!(true));
    assert!(
        won["params"].get("winner").is_none(),
        "a leg told its own bytes went out is never told about somebody else's: {won}"
    );

    second.send(answer(0, "phone-b")).await.unwrap();
    let lost = next_frame(&mut second).await;
    assert_eq!(lost["params"]["delivered"], serde_json::json!(false));
    assert_eq!(
        lost["params"]["winner"], "ccd",
        "another phone answered first, which is not the same statement as \"the Mac did\""
    );
}

/// **The TUI is never told.**
///
/// The disposition is a frame this broker composed, and the `Tui` leg is a real Codex
/// client whose stream is a byte-exact passthrough of the app-server. Only
/// CodeConnect's own daemon is addressable this way.
///
/// **Mutation:** drop the `matches!(role, Role::Ccd)` conjunct where `answered` is
/// noted and the real TUI is handed a method it has never been told about.
#[tokio::test]
async fn a_tui_answer_is_never_told_a_disposition() {
    let h = start_broker();
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]);
    h.push_replies(vec![(
        "keyboard".into(),
        Message::Text(r#"{"method":"thread/status/changed","params":{}}"#.into()),
    )]);
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;

    tui.send(answer(0, "keyboard")).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1);
    // **A barrier, not a sleep.** The scripted reply is emitted by the fake when it
    // sees the answer arrive upstream, so once it lands here everything the broker
    // had for this leg has been written. A disposition would have had to come first.
    let next = next_frame(&mut tui).await;
    assert_eq!(
        next["method"], "thread/status/changed",
        "a TUI leg must see the app-server's next frame, never a broker-composed one"
    );
}

/// **An answer the leg could never have been authorized for is told nothing.**
///
/// `bound_thread` returns `None` for an id this leg never observed, so there is no
/// thread the broker can truthfully name and it says nothing rather than guessing —
/// the same fail-closed shape as `authorize`. A daemon that is told nothing records
/// the answer as unknown, which is the honest reading of a claim whose fate the
/// broker cannot describe.
///
/// **Mutation:** make `bound_thread` fall back to the empty string instead of `None`
/// and an unsolicited response is answered with a disposition naming no thread.
#[tokio::test]
async fn an_unsolicited_ccd_response_is_told_nothing() {
    let h = start_broker();
    h.push_replies(vec![(
        "thread/loaded/list".into(),
        Message::Text(r#"{"method":"thread/status/changed","params":{}}"#.into()),
    )]);
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(answer(99, "ghost")).await.unwrap();
    // The barrier is a request this leg IS allowed to make: it forwards, the fake
    // answers it, and the answer arriving proves the broker has finished with
    // everything sent before it — a disposition would have had to precede it.
    ccd.send(Message::Text(
        r#"{"id":7,"method":"thread/loaded/list","params":{}}"#.into(),
    ))
    .await
    .unwrap();
    let next = next_frame(&mut ccd).await;
    assert_eq!(
        next["method"], "thread/status/changed",
        "an id this leg never observed names no thread, so nothing is said about it"
    );
    assert_eq!(
        h.state.recorded.lock().unwrap().len(),
        1,
        "only the barrier reached upstream; the unsolicited response forwarded zero bytes"
    );
}

#[tokio::test]
async fn per_thread_id_reuse_resolves_to_the_correct_slot() {
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-B").await;
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
// Server-request ids come from one monotonic counter per app-server process and are not
// reused across threads, so a second observation
// of an id on a leg comes only from a broken or hostile upstream. A bare Response frame
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
    let _keyboard = keyboard_on(&h, "th-A").await;
    h.push_script(vec![
        approval(COMMAND_EXEC_APPROVAL, "th-A", 0),
        approval(COMMAND_EXEC_APPROVAL, "th-B", 0),
        barrier(),
    ]);
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await; // th-A binds id=0
    drain_barrier(&mut ccd).await; // th-B reuses id=0 ⇒ collision ⇒ tombstone, withheld

    ccd.send(answer(0, "would-be-A")).await.unwrap();
    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert!(
        rec.is_empty(),
        "after a collision the original binding is unanswerable, and nobody answered the \
         colliding request on this leg, got {rec:?}"
    );
}

#[tokio::test]
async fn late_duplicate_after_same_id_reuse_forwards_zero_bytes() {
    // One leg observes id=0 for th-A, then id=0 again for th-B (reuse). The reuse is a
    // collision that tombstones id=0, so NO id=0 response forwards — neither the answer the
    // attacker means for th-B nor a late one for th-A. (Under the old never-rebind form
    // th-A stayed answerable once; that was the reverse-alias defect.)
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-A").await;
    h.push_script(vec![
        approval(COMMAND_EXEC_APPROVAL, "th-A", 0),
        approval(COMMAND_EXEC_APPROVAL, "th-B", 0),
        barrier(),
    ]);
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await; // th-A
    drain_barrier(&mut ccd).await; // th-B reuse ⇒ tombstone, withheld

    ccd.send(answer(0, "first")).await.unwrap();
    ccd.send(answer(0, "stale")).await.unwrap();
    settle().await;
    let rec = h.state.recorded.lock().unwrap().clone();
    assert!(
        rec.is_empty(),
        "every id=0 ANSWER after a collision forwards zero bytes, got {rec:?}"
    );
}

#[tokio::test]
async fn losing_sibling_after_same_id_reuse_forwards_zero_bytes() {
    // Fanout to both legs at id=0 (th-A), each leg ALSO reuses id=0 for th-B. On EACH leg
    // the reuse is a collision that tombstones id=0, so the phone cannot answer id=0 and the
    // keyboard's answer takes no slot: it forwards, as native codex would, and the
    // app-server decides which request it answers.
    let h = start_broker();
    let _keyboard = keyboard_on(&h, "th-A").await;
    h.push_script(vec![
        approval(COMMAND_EXEC_APPROVAL, "th-A", 0),
        approval(COMMAND_EXEC_APPROVAL, "th-B", 0),
    ]);
    h.push_script(vec![
        approval(COMMAND_EXEC_APPROVAL, "th-A", 0),
        approval(COMMAND_EXEC_APPROVAL, "th-B", 0),
        barrier(),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    drain_approval(&mut tui).await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;
    drain_barrier(&mut ccd).await;

    ccd.send(answer(0, "ccd")).await.unwrap();
    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "the phone cannot answer a tombstoned id"
    );
    tui.send(answer(0, "tui")).await.unwrap();
    let rec = recorded_after(&h.state, 1).await;
    assert_eq!(rec.len(), 1, "{rec:?}");
    assert!(rec[0].contains(r#""by":"tui""#));
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
    let _keyboard = keyboard_on(&h, "th-B").await;
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
    let _keyboard = keyboard_on(&h, "th-A").await;
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
async fn a_duplicate_member_approval_frame_closes_the_leg() {
    // An s2c approval frame with a duplicate `id` member is ambiguous
    // (parser-differential): it registers no capability, so any phone answer to it would
    // forward zero bytes, and it poisons the phone's view so that every LATER approval
    // would have the same fate. The phone's leg closes rather than deliver it.
    let h = start_broker();
    h.push_script(vec![Message::Text(format!(
        r#"{{"method":"{COMMAND_EXEC_APPROVAL}","id":0,"id":1,"params":{{"threadId":"th-A","itemId":"x"}}}}"#
    ))]);
    let mut ccd = connect(&h.ccd_sock).await;

    // The leg ends without the frame being delivered. The relay closes a leg by ending
    // its loop and dropping the socket — the same shape every other `DropCloseLeg` takes
    // — so the client sees EOF or a reset, never the frame.
    match ccd.next().await {
        None | Some(Err(_)) => {}
        Some(Ok(Message::Close(_))) => {}
        Some(Ok(other)) => panic!("the leg must close, not deliver: {other:?}"),
    }
    settle().await;
    assert!(
        h.state.recorded.lock().unwrap().is_empty(),
        "a duplicate-member approval frame forwards nothing upstream either",
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
/// noncanonically**.
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
/// Raw text and not `serde_json::Value`: the claim under test is that
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
    // **Byte-for-byte, against a deliberately noncanonical original**.
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

/// **An `ephemeral` thread's announcement does not displace the head's.** Measured on
/// 0.153.4, the TUI announces two such threads on its own — the title thread of the first
/// user turn and a `/side` fork (`fixtures/codex/title-thread-0.153.4.jsonl`,
/// `fixtures/codex/side-fork-0.153.4.jsonl`) — and neither becomes the head. A `ccd` leg
/// that connects after both is still owed the head's announcement.
#[tokio::test]
async fn a_late_ccd_subscriber_is_replayed_the_head_past_ephemeral_announcements() {
    let h = start_broker();
    h.push_script(vec![started_announcement("01a0-a")]);
    h.push_replies(vec![("thread/start".into(), creation_response("01a0-a"))]);
    let mut tui = connect(&h.tui_sock).await;
    next_frame(&mut tui).await;
    create_thread(&mut tui, "01a0-a").await;

    let ephemeral = |thread: &str| {
        Message::Text(
            serde_json::json!({"method": "thread/started",
                   "params": {"thread": {"id": thread, "ephemeral": true, "path": null}}})
            .to_string(),
        )
    };
    h.push_script(vec![ephemeral("01a0-title"), ephemeral("01a0-side")]);
    let mut other = connect(&h.tui_sock).await;
    assert_eq!(
        next_frame(&mut other).await["params"]["thread"]["id"],
        "01a0-title"
    );
    assert_eq!(
        next_frame(&mut other).await["params"]["thread"]["id"],
        "01a0-side"
    );

    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    assert_eq!(
        text_within(&mut ccd, Duration::from_secs(5)).await,
        Some(started_announcement_text("01a0-a")),
        "the head's own announcement, replayed to the late leg"
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
/// observer a thread to chase that the broker itself refused to bind. The head it did
/// bind, which nothing announced, is told in the broker's own word.
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
    assert_eq!(
        frame_within(&mut ccd, Duration::from_secs(5)).await,
        Some(head_notice("01a0-a")),
        "the last announcement does not name the head, so it is not repeated; the head \
         itself is what the leg is told"
    );
    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "and the stranger never follows it"
    );
}

/// **A leg that initializes DURING a creation is served when the head binds.**
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

/// **A leg already on the broadcast stream is not sent a second copy.**
///
/// A leg that carried a thread's announcement live has been told that thread; when the
/// thread binds there is nothing to repeat, and a duplicate would be the broker
/// re-announcing a thread the reader already has.
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

/// **The `ccd` role is the authorization, and it is load-bearing.**
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
/// Deliberately NOT a `thread/started`: that would record the leg as told a thread, and
/// make the tests pass for the wrong reason.
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

/// **The head a leg is sent is the head when it is sent, not when it moved.**
///
/// The head arm is the last of three in the leg's `select!`, so between "A binds" and
/// "the leg can speak" the leg can pass a whole `/new`. Sending A then is not a harmless
/// duplicate: `ccd`'s reader takes an announcement naming neither its visit nor its
/// candidate as a person pressing `/new`, so the repair would walk the link BACK to a
/// thread the session has left.
///
/// **Staged, not raced.** The leg is parked inside `ws.send` on a 2 MB frame that
/// nothing is reading, which is a kernel-level block: while it is held, the TUI legs
/// bind A (waking this leg), announce B and switch to B, all deterministically. Only then
/// does the client start reading.
#[tokio::test]
async fn a_head_that_moves_before_the_leg_can_speak_is_sent_as_it_now_is() {
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
        "the predecessor must never be sent: this reader would take a late 01a0-a for \
         a `/new` back onto a thread the session left"
    );
    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "and nothing follows it"
    );
    let events = h.events();
    assert!(
        events
            .iter()
            .any(|e| e.contains("replayed thread/started for 01a0-b")),
        "the delivery is stated in the log: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e.contains("for 01a0-a")),
        "and no delivery of the thread the session left: {events:?}"
    );
}

/// **A leg descheduled across a `/new` cannot deny the new head to later subscribers.**
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

// ---------------------------------------------------------------------------
// A head the app-server never announces: the keyboard's `/resume`.
// ---------------------------------------------------------------------------

/// The broker's own word that the head bound to `thread`.
fn head_notice(thread: &str) -> serde_json::Value {
    serde_json::json!({"method": "codeconnect/head", "params": {"threadId": thread}})
}

/// A `ccd` leg that heard `thread`'s announcement live, on its own upstream, with a
/// keyboard leg that then binds `thread` — the ordinary launch. Returns both legs.
///
/// The keyboard's further replies are `replies`, answered on its leg after the creation.
async fn live_leg_on(
    h: &Harness,
    thread: &str,
    ccd_replies: Vec<(String, Message)>,
    replies: Vec<(String, Message)>,
) -> (WebSocketStream<UnixStream>, WebSocketStream<UnixStream>) {
    let mut ccd_table = vec![("initialize".to_string(), started_announcement(thread))];
    ccd_table.extend(ccd_replies);
    h.push_replies(ccd_table);
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    assert_eq!(
        text_within(&mut ccd, Duration::from_secs(5)).await,
        Some(started_announcement_text(thread)),
        "the premise: the leg hears the announcement live"
    );
    h.push_script(vec![]);
    let mut table = vec![("thread/start".to_string(), creation_response(thread))];
    table.extend(replies);
    h.push_replies(table);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, thread).await;
    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "the premise: a head the leg heard announced is not told again"
    );
    (ccd, tui)
}

/// **A keyboard `/resume` reaches the phone's leg as the broker's own word.**
///
/// The app-server announces a thread it creates and never one it resumes
/// (`fixtures/codex/thread-switch.jsonl`: resume, answer, unsubscribe, no
/// `thread/started`). The head follows the resume, so without this the daemon's link
/// stays on the thread the keyboard left and every phone turn is refused.
#[tokio::test]
async fn a_ccd_leg_is_told_the_thread_a_keyboard_resume_moves_the_head_to() {
    let h = start_broker();
    let (resume, resumed) = keyboard_resume("01a0-b", "r1");
    let (mut ccd, mut tui) = live_leg_on(
        &h,
        "01a0-a",
        vec![],
        vec![("thread/resume".into(), resumed)],
    )
    .await;

    tui.send(Message::Text(resume)).await.unwrap();
    assert_eq!(
        next_frame(&mut tui).await["result"]["thread"]["id"],
        "01a0-b"
    );
    assert_eq!(
        frame_within(&mut ccd, Duration::from_secs(5)).await,
        Some(head_notice("01a0-b")),
        "the leg must be told the thread the keyboard resumed"
    );
    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "once"
    );
    assert_eq!(phone_turn_outcome(&h, &mut ccd, "01a0-b", 21).await, None);
    assert_eq!(
        phone_turn_outcome(&h, &mut ccd, "01a0-a", 22)
            .await
            .as_deref(),
        Some(NOT_THE_HEAD)
    );
}

/// **A resume of the thread the head is already on tells nobody anything.**
#[tokio::test]
async fn a_keyboard_resume_of_the_head_itself_is_not_told_again() {
    let h = start_broker();
    let (resume, resumed) = keyboard_resume("01a0-a", "r1");
    let (mut ccd, mut tui) = live_leg_on(
        &h,
        "01a0-a",
        vec![],
        vec![("thread/resume".into(), resumed)],
    )
    .await;

    tui.send(Message::Text(resume)).await.unwrap();
    assert_eq!(
        next_frame(&mut tui).await["result"]["thread"]["id"],
        "01a0-a"
    );
    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "the leg already follows this head"
    );
}

/// **A resume the app-server refuses moved nothing, so nothing is said.**
#[tokio::test]
async fn a_keyboard_resume_the_server_refuses_is_not_told() {
    let h = start_broker();
    let (resume, _) = keyboard_resume("01a0-b", "r1");
    let refused = Message::Text(r#"{"id":"r1","error":{"code":-32600,"message":"no"}}"#.into());
    let (mut ccd, mut tui) = live_leg_on(
        &h,
        "01a0-a",
        vec![],
        vec![("thread/resume".into(), refused)],
    )
    .await;

    tui.send(Message::Text(resume)).await.unwrap();
    assert_eq!(next_frame(&mut tui).await["error"]["code"], -32600);
    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "the head is back where it was, and the leg already follows it"
    );
    assert_eq!(phone_turn_outcome(&h, &mut ccd, "01a0-a", 21).await, None);
}

/// **Two moves before the leg can speak: it is told the last one, and only that.**
///
/// Staged with the parking frame: the leg is held inside `ws.send` while the keyboard
/// binds A and resumes B and then C. Released, it must say C — telling it B, or A, would
/// walk the link through threads the session has already left.
#[tokio::test]
async fn a_leg_held_across_two_keyboard_moves_is_told_only_the_last() {
    let h = start_broker();
    h.push_replies(vec![("initialize".into(), parking_frame())]);
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    settle().await;

    let (to_b, to_b_answer) = keyboard_resume("01a0-b", "r1");
    let (to_c, to_c_answer) = keyboard_resume("01a0-c", "r2");
    h.push_script(vec![started_announcement("01a0-a")]);
    h.push_replies(vec![
        ("thread/start".into(), creation_response("01a0-a")),
        ("\"r1\"".into(), to_b_answer),
        ("\"r2\"".into(), to_c_answer),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    next_frame(&mut tui).await;
    create_thread(&mut tui, "01a0-a").await;
    for resume in [to_b, to_c] {
        tui.send(Message::Text(resume)).await.unwrap();
        next_frame(&mut tui).await;
    }

    let parked = text_within(&mut ccd, Duration::from_secs(10))
        .await
        .expect("the parking frame");
    assert!(parked.contains("x/noise"), "the premise: the leg was held");
    assert_eq!(
        frame_within(&mut ccd, Duration::from_secs(10)).await,
        Some(head_notice("01a0-c")),
        "the head at the moment the leg speaks, and no thread it passed through"
    );
    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "and nothing after it"
    );
}

/// **A leg that connects after a resume is told the resumed thread, once.**
///
/// No announcement names it, so the replay of the app-server's bytes has nothing to
/// repeat; the broker's own word is what the late leg hears.
#[tokio::test]
async fn a_ccd_leg_that_connects_after_a_keyboard_resume_is_told_it_once() {
    let h = start_broker();
    let (resume, resumed) = keyboard_resume("01a0-b", "r1");
    h.push_script(vec![started_announcement("01a0-a")]);
    h.push_replies(vec![
        ("thread/start".into(), creation_response("01a0-a")),
        ("thread/resume".into(), resumed),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    next_frame(&mut tui).await;
    create_thread(&mut tui, "01a0-a").await;
    tui.send(Message::Text(resume)).await.unwrap();
    next_frame(&mut tui).await;

    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    assert_eq!(
        frame_within(&mut ccd, Duration::from_secs(5)).await,
        Some(head_notice("01a0-b"))
    );
    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "once"
    );
}

/// **Past the session's memory of the threads it left, a late announcement still owes the
/// head.** The session remembers 64 retired threads; the leg's debt must not depend on
/// that list. Sixty-four resumes first, then the slow-leg staging above.
#[tokio::test]
async fn a_late_left_thread_owes_the_head_past_sixty_four_moves() {
    let h = start_broker();
    h.push_replies(vec![
        ("initialize".into(), parking_frame()),
        (FOLLOWS.into(), started_announcement("01a0-a")),
    ]);
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    settle().await;

    let mut replies = Vec::new();
    let mut requests = Vec::new();
    for i in 0..64 {
        let (request, answer) = keyboard_resume(&format!("01a0-x{i}"), &format!("q{i}"));
        replies.push((format!("\"q{i}\""), answer));
        requests.push(request);
    }
    replies.push(("thread/start".into(), creation_response("01a0-a")));
    let (to_b, to_b_answer) = keyboard_resume("01a0-b", "r1");
    replies.push(("\"r1\"".into(), to_b_answer));
    h.push_script(vec![started_announcement("01a0-a")]);
    h.push_replies(replies);
    let mut tui = connect(&h.tui_sock).await;
    next_frame(&mut tui).await;
    for request in requests {
        tui.send(Message::Text(request)).await.unwrap();
        next_frame(&mut tui).await;
    }
    create_thread(&mut tui, "01a0-a").await;
    tui.send(Message::Text(to_b)).await.unwrap();
    next_frame(&mut tui).await;

    assert_eq!(
        frames_until_quiet(&mut ccd).await,
        vec![
            "<parking>".to_string(),
            started_announcement_text("01a0-a"),
            head_notice("01a0-b").to_string(),
        ]
    );
}

/// **A thread announced and never bound owes the head the same way.** A keyboard's `/new`
/// is announced and its leg closes before the answer, so the thread never becomes the
/// head; another keyboard then resumes H. A slow leg hearing the orphan late must hear H
/// behind it.
#[tokio::test]
async fn a_late_announcement_of_a_thread_that_never_bound_owes_the_head() {
    let h = start_broker();
    h.push_replies(vec![
        ("initialize".into(), parking_frame()),
        (FOLLOWS.into(), started_announcement("01a0-s")),
    ]);
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    settle().await;

    h.push_script(vec![]);
    h.push_replies(vec![(
        "thread/start".into(),
        started_announcement("01a0-s"),
    )]);
    let mut orphaned = connect(&h.tui_sock).await;
    orphaned
        .send(Message::Text(CREATION_REQUEST.into()))
        .await
        .unwrap();
    next_frame(&mut orphaned).await;
    drop(orphaned);
    settle().await;

    let (to_h, to_h_answer) = keyboard_resume("01a0-h", "r1");
    h.push_script(vec![]);
    h.push_replies(vec![("\"r1\"".into(), to_h_answer)]);
    let mut tui = connect(&h.tui_sock).await;
    tui.send(Message::Text(to_h)).await.unwrap();
    next_frame(&mut tui).await;

    assert_eq!(
        frames_until_quiet(&mut ccd).await,
        vec![
            "<parking>".to_string(),
            started_announcement_text("01a0-s"),
            head_notice("01a0-h").to_string(),
        ]
    );
}

/// **A fork is announced, so the leg that heard it is not told it again.**
///
/// Staged in the measured order (`fixtures/codex/thread-switch.jsonl`: the request at line
/// 30, the broadcasts at 31 and 33, the answer at 32): the keyboard's fork goes out, the
/// leg hears the new thread announced on its own upstream, and only then does the answer
/// bind it. The keyboard leg is held on the parking frame with the answer queued behind
/// it, so the announcement provably lands inside the move.
#[tokio::test]
async fn a_keyboard_fork_the_leg_heard_announced_is_not_told_again() {
    let h = start_broker();
    let forked = Message::Text(
        serde_json::json!({"id": "f1", "result": {"thread": {"id": "01a0-f"}}}).to_string(),
    );
    let (mut ccd, mut tui) = live_leg_on(
        &h,
        "01a0-a",
        vec![("thread/loaded/list".into(), started_announcement("01a0-f"))],
        vec![
            ("thread/fork".into(), parking_frame()),
            (FOLLOWS.into(), forked),
        ],
    )
    .await;

    tui.send(Message::Text(
        r#"{"method":"thread/fork","id":"f1","params":{"threadId":"01a0-a"}}"#.into(),
    ))
    .await
    .unwrap();
    settle().await;
    ccd.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":2,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    assert_eq!(
        text_within(&mut ccd, Duration::from_secs(5)).await,
        Some(started_announcement_text("01a0-f")),
        "the premise: the fork's announcement reaches the leg live, mid-move"
    );
    assert!(
        text_within(&mut tui, Duration::from_secs(10))
            .await
            .is_some_and(|t| t.contains("x/noise")),
        "the premise: the keyboard leg was held with the answer behind it"
    );
    assert_eq!(
        next_frame(&mut tui).await["result"]["thread"]["id"],
        "01a0-f"
    );
    assert!(
        text_within(&mut ccd, Duration::from_millis(300))
            .await
            .is_none(),
        "the leg already follows the fork"
    );
    assert_eq!(phone_turn_outcome(&h, &mut ccd, "01a0-f", 21).await, None);
}

/// Every frame a leg says until it falls quiet, the parking frame abbreviated.
async fn frames_until_quiet(ws: &mut WebSocketStream<UnixStream>) -> Vec<String> {
    let mut seen = Vec::new();
    while let Some(t) = text_within(ws, Duration::from_secs(2)).await {
        seen.push(if t.contains("x/noise") {
            "<parking>".into()
        } else {
            t
        });
    }
    seen
}

/// **A leg that hears a thread the session has left, after the head moved, is told the
/// head again.**
///
/// A slow leg's own copy of A's broadcast can reach it only after the keyboard has moved
/// on to B by a `/resume`. The reader follows what it heard last, so the leg is left on A
/// unless the broker repeats the head behind it. Staged: the leg is parked on the 2 MB
/// frame with A's broadcast queued behind it while the keyboard creates A and resumes B.
#[tokio::test]
async fn a_leg_that_hears_a_left_thread_late_is_told_the_head_after_it() {
    let h = start_broker();
    h.push_replies(vec![
        ("initialize".into(), parking_frame()),
        (FOLLOWS.into(), started_announcement("01a0-a")),
    ]);
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    settle().await;

    let (to_b, to_b_answer) = keyboard_resume("01a0-b", "r1");
    h.push_script(vec![started_announcement("01a0-a")]);
    h.push_replies(vec![
        ("thread/start".into(), creation_response("01a0-a")),
        ("\"r1\"".into(), to_b_answer),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    next_frame(&mut tui).await;
    create_thread(&mut tui, "01a0-a").await;
    tui.send(Message::Text(to_b)).await.unwrap();
    next_frame(&mut tui).await;

    assert_eq!(
        frames_until_quiet(&mut ccd).await,
        vec![
            "<parking>".to_string(),
            started_announcement_text("01a0-a"),
            head_notice("01a0-b").to_string(),
        ],
        "the late announcement of the thread left behind, and then the head"
    );
}

/// **The same, after the leg was already told the head**: its own late copy of the
/// predecessor's broadcast would otherwise be the last word.
#[tokio::test]
async fn a_leg_told_the_head_then_hearing_a_left_thread_is_told_the_head_again() {
    let h = start_broker();
    h.push_replies(vec![(
        "thread/loaded/list".into(),
        started_announcement("01a0-c"),
    )]);
    let mut ccd = connect(&h.ccd_sock).await;
    ccd.send(Message::Text(CCD_INITIALIZE.into()))
        .await
        .unwrap();
    settle().await;

    let (to_b, to_b_answer) = keyboard_resume("01a0-b", "r1");
    h.push_script(vec![started_announcement("01a0-c")]);
    h.push_replies(vec![
        ("thread/start".into(), creation_response("01a0-c")),
        ("\"r1\"".into(), to_b_answer),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    next_frame(&mut tui).await;
    create_thread(&mut tui, "01a0-c").await;
    assert_eq!(
        text_within(&mut ccd, Duration::from_secs(5)).await,
        Some(started_announcement_text("01a0-c")),
        "the premise: C is replayed to the leg"
    );
    tui.send(Message::Text(to_b)).await.unwrap();
    next_frame(&mut tui).await;
    assert_eq!(
        frame_within(&mut ccd, Duration::from_secs(5)).await,
        Some(head_notice("01a0-b")),
        "the premise: the leg is told B"
    );

    ccd.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":2,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    assert_eq!(
        frames_until_quiet(&mut ccd).await,
        vec![
            started_announcement_text("01a0-c"),
            head_notice("01a0-b").to_string(),
        ],
        "the leg's own late copy of C, and then the head again"
    );
}

// ---------------------------------------------------------------------------
// The keyboard leg is a passthrough: the TUI's frames reach the app-server as they are,
// and the broker only watches them to keep the phone's head, busy mark and approval
// arbitration true.
// ---------------------------------------------------------------------------

/// A captured `interrupt-0.153.jsonl` frame, verbatim: its line `line` (1-based), with the
/// recorder's envelope removed.
fn captured_interrupt_frame(line: usize) -> String {
    const CAPTURE: &str = include_str!("../../../fixtures/codex/interrupt-0.153.jsonl");
    let row: serde_json::Value =
        serde_json::from_str(CAPTURE.lines().nth(line - 1).expect("the line exists")).unwrap();
    row["frame"].to_string()
}

/// The thread the captured interrupt session ran on.
const CAPTURED_THREAD: &str = "01a073f6-09c8-7c10-8212-4d369b80140b";

/// The turn/start a phone's daemon writes: fourteen keys, twelve of them null.
fn phone_turn(thread: &str, id: i64) -> String {
    serde_json::json!({
        "method": "turn/start",
        "id": id,
        "params": {
            "threadId": thread,
            "input": [{"type": "text", "text": "hello from the phone", "text_elements": []}],
            "clientUserMessageId": null,
            "approvalPolicy": null,
            "approvalsReviewer": null,
            "sandboxPolicy": null,
            "cwd": null,
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

/// A keyboard `thread/resume` of `thread` under request id `id`, and the app-server's
/// success answer to it.
fn keyboard_resume(thread: &str, id: &str) -> (String, Message) {
    let request = serde_json::json!({
        "method": "thread/resume",
        "id": id,
        "params": {"threadId": thread, "cwd": null, "runtimeWorkspaceRoots": ["/elsewhere"]}
    })
    .to_string();
    let answer = Message::Text(
        serde_json::json!({
            "id": id,
            "result": {"thread": {"id": thread}, "cwd": "/elsewhere", "runtimeWorkspaceRoots": ["/elsewhere"]}
        })
        .to_string(),
    );
    (request, answer)
}

/// Send a phone `turn/start` and return what the broker did with it: `None` when it
/// forwarded (the upstream recorded it), or the refusal's message.
async fn phone_turn_outcome(
    h: &Harness,
    ccd: &mut WebSocketStream<UnixStream>,
    thread: &str,
    id: i64,
) -> Option<String> {
    let frame = phone_turn(thread, id);
    let before = h.state.recorded.lock().unwrap().len();
    ccd.send(Message::Text(frame.clone())).await.unwrap();
    match tokio::time::timeout(Duration::from_millis(500), ccd.next()).await {
        Ok(Some(Ok(m))) => {
            let v: serde_json::Value = serde_json::from_str(m.to_text().unwrap()).unwrap();
            assert_eq!(v["id"], id, "the refusal answers this turn: {v}");
            Some(
                v["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        }
        _ => {
            let rec = recorded_after(&h.state, before + 1).await;
            assert_eq!(
                rec.last(),
                Some(&frame),
                "a turn that is not refused forwards"
            );
            None
        }
    }
}

const NOT_THE_HEAD: &str = "turn refused: it does not name this session's bound thread";
const ALREADY_BUSY: &str = "turn refused: this session is already running a turn";

/// **Every keyboard frame reaches the app-server as it was sent.** Methods
/// the phone's leg refuses, a notification, a malformed frame and a binary frame, one
/// after another on one open leg.
#[tokio::test]
async fn every_keyboard_frame_reaches_upstream_byte_identical() {
    let h = start_broker();
    let mut tui = connect(&h.tui_sock).await;
    let texts = [
        r#"{"method":"collaborationMode/list","id":1,"params":{}}"#,
        r#"{"method":"config/read","id":2,"params":{"includeLayers":false}}"#,
        r#"{"method":"thread/list","id":3,"params":{"limit":25}}"#,
        r#"{"method":"command/exec","id":4,"params":{"command":["ls"]}}"#,
        r#"{"method":"thread/settings/update","id":5,"params":{"threadId":"t","approvalPolicy":"never"}}"#,
        r#"{"method":"thread/fork","id":6,"params":{"threadId":"t"}}"#,
        r#"{"method":"some/notification","params":{"x":1}}"#,
        r#"{"method":"x","id":7,"id":8}"#,
        "not json at all",
    ];
    for t in texts {
        tui.send(Message::Text(t.into())).await.unwrap();
    }
    tui.send(Message::Binary(vec![0, 159, 146, 150]))
        .await
        .unwrap();

    let rec = recorded_after(&h.state, texts.len()).await;
    assert_eq!(rec, texts.map(str::to_string).to_vec());
    for _ in 0..200 {
        if !h.state.recorded_binary.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        h.state.recorded_binary.lock().unwrap().clone(),
        vec![vec![0u8, 159, 146, 150]]
    );
}

/// **The head follows the keyboard.** A keyboard `/resume` of a thread the
/// session never created moves the head there, so the phone may start a turn on it and
/// not on the thread the keyboard left.
#[tokio::test]
async fn the_head_follows_a_keyboard_resume() {
    let h = start_broker();
    let (resume, resumed) = keyboard_resume("01a0-x", "r1");
    h.push_replies(vec![
        ("thread/start".into(), creation_response("01a0-a")),
        ("thread/resume".into(), resumed),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, "01a0-a").await;
    tui.send(Message::Text(resume)).await.unwrap();
    assert_eq!(
        next_frame(&mut tui).await["result"]["thread"]["id"],
        "01a0-x"
    );

    let mut ccd = connect(&h.ccd_sock).await;
    assert_eq!(phone_turn_outcome(&h, &mut ccd, "01a0-x", 21).await, None);
    assert_eq!(
        phone_turn_outcome(&h, &mut ccd, "01a0-a", 22)
            .await
            .as_deref(),
        Some(NOT_THE_HEAD)
    );
}

/// **A keyboard move the app-server refuses leaves the head where it was.**
#[tokio::test]
async fn a_keyboard_move_that_errors_restores_the_head() {
    let h = start_broker();
    h.push_replies(vec![
        ("\"start-1\"".into(), creation_response("01a0-a")),
        (
            "\"start-2\"".into(),
            Message::Text(r#"{"id":"start-2","error":{"code":-32600,"message":"no"}}"#.into()),
        ),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, "01a0-a").await;
    tui.send(Message::Text(
        CREATION_REQUEST.replace("start-1", "start-2"),
    ))
    .await
    .unwrap();
    assert_eq!(next_frame(&mut tui).await["error"]["code"], -32600);

    let mut ccd = connect(&h.ccd_sock).await;
    assert_eq!(phone_turn_outcome(&h, &mut ccd, "01a0-a", 21).await, None);
}

/// **A keyboard leg that closes with a move in flight leaves the phone
/// with no head** until the keyboard binds one again.
#[tokio::test]
async fn a_keyboard_leg_that_closes_mid_move_leaves_no_head() {
    let h = start_broker();
    h.push_replies(vec![("\"start-1\"".into(), creation_response("01a0-a"))]);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, "01a0-a").await;
    tui.send(Message::Text(
        CREATION_REQUEST.replace("start-1", "start-2"),
    ))
    .await
    .unwrap();
    recorded_after(&h.state, 2).await;
    tui.close(None).await.unwrap();
    drop(tui);
    settle().await;

    let mut ccd = connect(&h.ccd_sock).await;
    assert_eq!(
        phone_turn_outcome(&h, &mut ccd, "01a0-a", 21)
            .await
            .as_deref(),
        Some(NOT_THE_HEAD)
    );
}

/// **A keyboard that unsubscribes from the head takes the phone off it**
/// until the keyboard's next thread binds.
#[tokio::test]
async fn a_keyboard_unsubscribe_of_the_head_keeps_the_phone_off_it_until_the_next_bind() {
    let h = start_broker();
    h.push_replies(vec![
        ("\"start-1\"".into(), creation_response("01a0-a")),
        (
            "thread/unsubscribe".into(),
            Message::Text(r#"{"id":7,"result":{"status":"unsubscribed"}}"#.into()),
        ),
        (
            "\"start-2\"".into(),
            Message::Text(
                serde_json::json!({"id": "start-2", "result": {"thread": {"id": "01a0-b"}}})
                    .to_string(),
            ),
        ),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, "01a0-a").await;
    tui.send(Message::Text(
        r#"{"method":"thread/unsubscribe","id":7,"params":{"threadId":"01a0-a"}}"#.into(),
    ))
    .await
    .unwrap();
    assert_eq!(next_frame(&mut tui).await["id"], 7);

    let mut ccd = connect(&h.ccd_sock).await;
    let refused = phone_turn_outcome(&h, &mut ccd, "01a0-a", 21).await;
    assert!(
        refused.is_some(),
        "the phone may not start a turn on a thread the keyboard left"
    );

    tui.send(Message::Text(
        CREATION_REQUEST.replace("start-1", "start-2"),
    ))
    .await
    .unwrap();
    assert_eq!(
        next_frame(&mut tui).await["result"]["thread"]["id"],
        "01a0-b"
    );
    assert_eq!(phone_turn_outcome(&h, &mut ccd, "01a0-b", 22).await, None);
}

/// **A keyboard turn in flight makes the thread busy for the phone**, even
/// when it is a shape no rule of the broker's ever vetted: the phone cannot become an
/// implicit steer into it.
#[tokio::test]
async fn a_keyboard_turn_in_flight_keeps_the_phone_from_starting_one() {
    let h = start_broker();
    h.push_replies(vec![("thread/start".into(), creation_response("01a0-a"))]);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, "01a0-a").await;
    let keyboard_turn = serde_json::json!({
        "method": "turn/start",
        "id": 30,
        "params": {
            "threadId": "01a0-a",
            "input": [{"type": "text", "text": "from the keyboard", "text_elements": []}],
            "model": "gpt-5.5",
            "effort": "high",
            "cwd": "/anywhere"
        }
    })
    .to_string();
    tui.send(Message::Text(keyboard_turn.clone()))
        .await
        .unwrap();
    let rec = recorded_after(&h.state, 2).await;
    assert_eq!(
        rec.last(),
        Some(&keyboard_turn),
        "the keyboard's turn forwards as sent"
    );

    let mut ccd = connect(&h.ccd_sock).await;
    assert_eq!(
        phone_turn_outcome(&h, &mut ccd, "01a0-a", 21)
            .await
            .as_deref(),
        Some(ALREADY_BUSY)
    );
}

/// **A turn the server announces while the keyboard is moving the head is kept**, so
/// when the move fails and the old head comes back, the phone sees it busy. Captured
/// frames from `fixtures/codex/interrupt-0.153.jsonl`.
#[tokio::test]
async fn a_turn_announced_during_a_move_is_busy_when_the_move_fails() {
    let h = start_broker();
    h.push_replies(vec![
        ("\"start-1\"".into(), creation_response(CAPTURED_THREAD)),
        (
            "\"start-2\"".into(),
            Message::Text(captured_interrupt_frame(2)),
        ),
        (
            "thread/loaded/list".into(),
            Message::Text(r#"{"id":"start-2","error":{"code":-32600,"message":"no"}}"#.into()),
        ),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, CAPTURED_THREAD).await;
    tui.send(Message::Text(
        CREATION_REQUEST.replace("start-1", "start-2"),
    ))
    .await
    .unwrap();
    assert_eq!(next_frame(&mut tui).await["method"], "turn/started");
    tui.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":9,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    assert_eq!(next_frame(&mut tui).await["id"], "start-2");

    let mut ccd = connect(&h.ccd_sock).await;
    assert_eq!(
        phone_turn_outcome(&h, &mut ccd, CAPTURED_THREAD, 21)
            .await
            .as_deref(),
        Some(ALREADY_BUSY)
    );
}

/// **A turn whose end the broker has seen can never be marked running again.** The
/// captured `turn/completed` arrives first, then a lagging leg's `turn/started` for the
/// same turn; the thread stays idle.
#[tokio::test]
async fn a_late_announcement_of_a_turn_that_already_ended_marks_nothing() {
    let h = start_broker();
    h.push_replies(vec![
        ("\"start-1\"".into(), creation_response(CAPTURED_THREAD)),
        (
            "\"first\"".into(),
            Message::Text(captured_interrupt_frame(8)),
        ),
        (
            "\"second\"".into(),
            Message::Text(captured_interrupt_frame(2)),
        ),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, CAPTURED_THREAD).await;
    tui.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":"first","params":{}}"#.into(),
    ))
    .await
    .unwrap();
    assert_eq!(next_frame(&mut tui).await["method"], "turn/completed");
    tui.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":"second","params":{}}"#.into(),
    ))
    .await
    .unwrap();
    assert_eq!(next_frame(&mut tui).await["method"], "turn/started");

    let mut ccd = connect(&h.ccd_sock).await;
    assert_eq!(
        phone_turn_outcome(&h, &mut ccd, CAPTURED_THREAD, 21).await,
        None
    );
}

/// The three server requests a phone cannot answer, as the app-server sends them.
fn non_phone_requests(thread: &str) -> Vec<Message> {
    [
        ("item/tool/call", 40),
        ("item/tool/requestUserInput", 41),
        ("mcpServer/elicitation/request", 42),
    ]
    .iter()
    .map(|(method, id)| {
        Message::Text(
            serde_json::json!({
                "method": method,
                "id": id,
                "params": {"threadId": thread, "turnId": "01a0-turn", "itemId": "x"}
            })
            .to_string(),
        )
    })
    .collect()
}

/// **A server request the phone cannot answer is the
/// keyboard's.** The keyboard is handed it and its answer reaches the app-server; the
/// phone's leg is handed nothing and answers nothing, because an answer from it — even
/// the broker's refusal — would settle the keyboard's question for it.
#[tokio::test]
async fn a_request_the_phone_cannot_answer_is_the_keyboards_alone() {
    let h = start_broker();
    h.push_replies(vec![("thread/start".into(), creation_response("01a0-a"))]);
    h.push_script(vec![]);
    h.push_script(non_phone_requests("01a0-a"));
    h.push_script(non_phone_requests("01a0-a"));
    let mut binder = connect(&h.tui_sock).await;
    create_thread(&mut binder, "01a0-a").await;

    let mut ccd = connect(&h.ccd_sock).await;
    let mut tui = connect(&h.tui_sock).await;
    for want in [
        "item/tool/call",
        "item/tool/requestUserInput",
        "mcpServer/elicitation/request",
    ] {
        assert_eq!(
            next_frame(&mut tui).await["method"],
            want,
            "the keyboard is handed it"
        );
    }
    assert_eq!(
        text_within(&mut ccd, Duration::from_millis(300)).await,
        None,
        "the phone is handed none of them"
    );
    assert_eq!(
        h.state.recorded.lock().unwrap().clone(),
        vec![CREATION_REQUEST.to_string()],
        "and nobody answered any of them upstream"
    );

    for id in [40, 41, 42] {
        let answer = format!(r#"{{"id":{id},"result":{{"answers":{{}}}}}}"#);
        tui.send(Message::Text(answer.clone())).await.unwrap();
        let rec = recorded_after(&h.state, (id - 38) as usize).await;
        assert_eq!(
            rec.last(),
            Some(&answer),
            "the keyboard's answer reaches the app-server"
        );
    }
}

/// **Phone first: the keyboard's late answer to an approval the phone
/// already won never reaches the app-server**, and the phone hears nothing more about it.
#[tokio::test]
async fn the_keyboards_late_answer_to_an_approval_the_phone_won_is_dropped() {
    let h = start_broker();
    h.push_replies(vec![("thread/start".into(), creation_response("th-A"))]);
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]);
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]);
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    create_thread(&mut tui, "th-A").await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    ccd.send(answer(0, "phone")).await.unwrap();
    let rec = recorded_after(&h.state, 2).await;
    assert!(rec[1].contains(r#""by":"phone""#));
    assert_eq!(next_frame(&mut ccd).await["params"]["delivered"], true);

    tui.send(answer(0, "keyboard")).await.unwrap();
    settle().await;
    assert_eq!(
        h.state.recorded.lock().unwrap().len(),
        2,
        "zero keyboard bytes"
    );
    assert_eq!(
        text_within(&mut ccd, Duration::from_millis(300)).await,
        None
    );
}

/// **A phone may answer only an approval on the thread the keyboard is on.** One
/// raised on a thread that is not the head is never handed to the phone, and an approval
/// the phone was handed stops being answerable from it when the keyboard moves away.
#[tokio::test]
async fn a_phone_answers_approvals_only_on_the_current_head() {
    let h = start_broker();
    let (resume, resumed) = keyboard_resume("01a0-b", "r1");
    h.push_replies(vec![
        ("thread/start".into(), creation_response("th-A")),
        ("thread/resume".into(), resumed),
    ]);
    h.push_script(vec![]);
    h.push_script(vec![
        approval(COMMAND_EXEC_APPROVAL, "01a0-other", 1),
        approval(COMMAND_EXEC_APPROVAL, "th-A", 0),
    ]);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, "th-A").await;
    let mut ccd = connect(&h.ccd_sock).await;
    let handed = next_frame(&mut ccd).await;
    assert_eq!(
        handed["params"]["threadId"], "th-A",
        "the approval on a thread that is not the head is not handed to the phone"
    );

    tui.send(Message::Text(resume)).await.unwrap();
    assert_eq!(
        next_frame(&mut tui).await["result"]["thread"]["id"],
        "01a0-b"
    );

    ccd.send(answer(0, "phone")).await.unwrap();
    let told = next_frame(&mut ccd).await;
    assert_eq!(told["params"]["delivered"], false);
    settle().await;
    assert!(
        !h.state
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.contains(r#""by":"phone""#)),
        "the phone's answer to an approval on a thread the keyboard left forwards zero bytes"
    );
}

/// **A keyboard turn in flight makes the thread busy for the phone** — certified with the
/// turn shape a real 0.147 TUI sends, so the refusal below is the busy rule itself and not
/// a side effect of how the broker treats the keyboard's frame.
#[tokio::test]
async fn a_measured_keyboard_turn_in_flight_refuses_the_phones_start() {
    let h = start_broker();
    h.push_replies(vec![("thread/start".into(), creation_response("01a0-a"))]);
    let mut tui = connect(&h.tui_sock).await;
    create_thread(&mut tui, "01a0-a").await;
    let keyboard_turn = serde_json::json!({
        "method": "turn/start",
        "id": 3,
        "params": {
            "threadId": "01a0-a",
            "input": [{"type": "text", "text": "from the keyboard", "text_elements": []}],
            "clientUserMessageId": null,
            "approvalPolicy": "untrusted",
            "approvalsReviewer": "user",
            "sandboxPolicy": null,
            "cwd": LAUNCH_CWD,
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
    .to_string();
    tui.send(Message::Text(keyboard_turn.clone()))
        .await
        .unwrap();
    let rec = recorded_after(&h.state, 2).await;
    assert_eq!(rec.last(), Some(&keyboard_turn));

    let mut ccd = connect(&h.ccd_sock).await;
    assert_eq!(
        phone_turn_outcome(&h, &mut ccd, "01a0-a", 21)
            .await
            .as_deref(),
        Some(ALREADY_BUSY)
    );
}

/// **A keyboard that closes mid-move leaves no head, and the next keyboard's thread
/// becomes it.**
#[tokio::test]
async fn a_new_keyboard_binds_after_one_closed_mid_move() {
    let h = start_broker();
    h.push_replies(vec![("\"start-1\"".into(), creation_response("01a0-a"))]);
    h.push_replies(vec![(
        "\"start-3\"".into(),
        Message::Text(
            serde_json::json!({"id": "start-3", "result": {
                "thread": {"id": "01a0-c", "path": "/x"},
                "cwd": LAUNCH_CWD,
                "runtimeWorkspaceRoots": [LAUNCH_CWD]
            }})
            .to_string(),
        ),
    )]);
    let mut first = connect(&h.tui_sock).await;
    create_thread(&mut first, "01a0-a").await;
    first
        .send(Message::Text(
            CREATION_REQUEST.replace("start-1", "start-2"),
        ))
        .await
        .unwrap();
    recorded_after(&h.state, 2).await;
    first.close(None).await.unwrap();
    drop(first);
    settle().await;

    let mut second = connect(&h.tui_sock).await;
    second
        .send(Message::Text(
            CREATION_REQUEST.replace("start-1", "start-3"),
        ))
        .await
        .unwrap();
    assert_eq!(
        next_frame(&mut second).await["result"]["thread"]["id"],
        "01a0-c"
    );

    let mut ccd = connect(&h.ccd_sock).await;
    assert_eq!(phone_turn_outcome(&h, &mut ccd, "01a0-c", 21).await, None);
}

/// **A phone that lost to the keyboard is told so, even when the app-server's
/// `serverRequest/resolved` reaches its leg before its own answer does.** The daemon
/// waits on this disposition; without it the phone's card waits out the daemon's whole
/// budget and the leg is dropped.
#[tokio::test]
async fn a_phone_that_lost_after_the_resolution_arrived_is_told_the_keyboard_won() {
    let h = start_broker_with_events();
    let _keyboard = keyboard_on(&h, "th-A").await;
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // tui leg
    h.push_script(vec![approval(COMMAND_EXEC_APPROVAL, "th-A", 0)]); // ccd leg
    h.push_replies(vec![]); // tui leg
    h.push_replies(vec![(
        "thread/loaded/list".into(),
        Message::Text(
            r#"{"method":"serverRequest/resolved","params":{"threadId":"th-A","requestId":0}}"#
                .into(),
        ),
    )]); // ccd leg
    let mut tui = connect(&h.tui_sock).await;
    drain_approval(&mut tui).await;
    let mut ccd = connect(&h.ccd_sock).await;
    drain_approval(&mut ccd).await;

    tui.send(answer(0, "keyboard")).await.unwrap();
    assert!(event_containing(&h, "capability confirmed: winner=Tui").await);

    ccd.send(Message::Text(
        r#"{"method":"thread/loaded/list","id":50,"params":{}}"#.into(),
    ))
    .await
    .unwrap();
    assert_eq!(
        next_frame(&mut ccd).await["method"],
        "serverRequest/resolved"
    );

    ccd.send(answer(0, "phone")).await.unwrap();
    let told = next_frame(&mut ccd).await;
    assert_eq!(told["method"], "codeconnect/responseDisposition", "{told}");
    assert_eq!(told["params"]["delivered"], false);
    assert_eq!(told["params"]["winner"], "tui", "{told}");
    settle().await;
    let upstream = h.state.recorded.lock().unwrap().clone();
    assert_eq!(
        upstream
            .iter()
            .filter(|f| f.contains(r#""by":"keyboard""#))
            .count(),
        1
    );
    assert!(
        !upstream.iter().any(|f| f.contains(r#""by":"phone""#)),
        "the losing answer forwards zero bytes: {upstream:?}"
    );
}
