//! Tests grounded in the committed captured frames (`fixtures/codex/*.jsonl`).
//!
//! Two things are proven against real Codex app-server traffic: (1) the shape classifier
//! recognizes every captured frame as a legal JSON-RPC message (never Malformed/Array),
//! and (2) the server→client passthrough delivers real frames byte-exact and in order.

use std::path::PathBuf;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::protocol::Message;

use codex_broker::message::{classify_shape, Shape, WsPayload};
use codex_broker::relay::Broker;
use codex_broker::upstream::{ConnectFuture, UpstreamChannels, UpstreamFactory};
use codex_broker::LaunchFingerprint;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/codex")
        .canonicalize()
        .unwrap()
}

fn lines(file: &str) -> Vec<String> {
    let p = fixtures_dir().join(file);
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("read {p:?}: {e}"))
        .lines()
        .map(str::to_string)
        .filter(|l| !l.trim().is_empty())
        .collect()
}

#[test]
fn every_captured_frame_classifies_as_a_legal_message() {
    let files = [
        "lifecycle.jsonl",
        "command-execution.jsonl",
        "file-change.jsonl",
        "interrupt.jsonl",
    ];
    let mut total = 0;
    for file in files {
        for line in lines(file) {
            total += 1;
            match classify_shape(&WsPayload::Text(line.clone())) {
                // Server→client frames are notifications or server requests (approval
                // requests carry method+id). Neither is Malformed/Array/Binary.
                Shape::Notification { .. } | Shape::Request { .. } => {}
                other => panic!("captured frame classified as {other:?}: {line}"),
            }
        }
    }
    assert!(
        total >= 100,
        "expected the full captured corpus, saw {total}"
    );
}

// --- s2c byte-exact passthrough over real frames -----------------------------

#[derive(Default)]
struct FakeState {
    scripted: Mutex<Vec<Message>>,
}

#[derive(Clone)]
struct FakeFactory {
    inner: Arc<FakeState>,
}

impl UpstreamFactory for FakeFactory {
    fn connect(&self) -> ConnectFuture {
        let scripted: Vec<Message> = std::mem::take(&mut *self.inner.scripted.lock().unwrap());
        Box::pin(async move {
            let (to_tx, mut to_rx) = tokio::sync::mpsc::channel::<Message>(256);
            let (from_tx, from_rx) = tokio::sync::mpsc::channel::<Message>(256);
            tokio::spawn(async move {
                for m in scripted {
                    if from_tx.send(m).await.is_err() {
                        return;
                    }
                }
                while to_rx.recv().await.is_some() {}
                drop(from_tx);
            });
            Ok(UpstreamChannels {
                to_upstream: to_tx,
                from_upstream: from_rx,
            })
        })
    }
}

#[tokio::test]
async fn captured_frames_pass_through_s2c_byte_exact_and_in_order() {
    let frames = lines("lifecycle.jsonl");
    let state = Arc::new(FakeState::default());
    for f in &frames {
        state
            .scripted
            .lock()
            .unwrap()
            .push(Message::Text(f.clone()));
    }

    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let tui = format!("/tmp/ccbf-{pid}-{n}-t.sock");
    let ccd = format!("/tmp/ccbf-{pid}-{n}-c.sock");
    let _ = std::fs::remove_file(&tui);
    let _ = std::fs::remove_file(&ccd);

    let fp = LaunchFingerprint {
        approval_policy: "untrusted".into(),
        approvals_reviewer: "user".into(),
        sandbox: "read-only".into(),
        hooks_enabled: true,
        launch_cwd: "/work/proj".into(),
    };
    let broker = Broker::new(
        tui.clone(),
        ccd.clone(),
        fp,
        FakeFactory {
            inner: Arc::clone(&state),
        },
    );
    tokio::spawn(async move {
        let _ = broker.serve().await;
    });

    // Connect.
    let mut ws = loop {
        if let Ok(s) = UnixStream::connect(&tui).await {
            if let Ok((ws, _)) = tokio_tungstenite::client_async("ws://localhost/", s).await {
                break ws;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    for expected in &frames {
        let got = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("frame did not arrive")
            .expect("stream closed")
            .expect("ws error");
        assert_eq!(
            &got.into_text().unwrap(),
            expected,
            "s2c frame must be byte-exact"
        );
    }
}
