//! Lightweight offline mock for `codex` CLI + `codex app-server` (JSON-RPC over WS-on-UDS).
//! Uses a single-threaded Tokio runtime so 50+ concurrent mock sessions consume minimal host RAM/CPU.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn run_stdio_app_server() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg["method"].as_str().unwrap_or_default();
        let reply = match method {
            "initialize" => json!({
                "id": id,
                "result": { "userAgent": "codex-cli/0.170.0" }
            }),
            "model/list" => json!({
                "id": id,
                "result": {
                    "data": [
                        {
                            "model": "gpt-5.3-codex",
                            "displayName": "GPT-5.3 Codex",
                            "description": "Fast coding model",
                            "defaultReasoningEffort": "medium",
                            "supportedReasoningEfforts": [
                                {"reasoningEffort": "low"},
                                {"reasoningEffort": "medium"},
                                {"reasoningEffort": "high"}
                            ]
                        }
                    ]
                }
            }),
            "account/read" => json!({
                "id": id,
                "result": {
                    "account": {"email": "bench@local"},
                    "requiresOpenaiAuth": false
                }
            }),
            _ => json!({ "id": id, "result": {} }),
        };
        writeln!(stdout, "{}", serde_json::to_string(&reply)?)?;
        stdout.flush()?;
    }
    Ok(())
}

struct ServerState {
    thread_id: String,
    cwd: String,
    next_turn: u64,
    next_req_id: i64,
    pending_reqs: HashMap<i64, (String, String, String)>, // req_id -> (turn_id, item_id, command)
    stopping: bool,
}

impl ServerState {
    fn new() -> Self {
        Self {
            thread_id: format!("thr_mock_{}", std::process::id()),
            cwd: "/tmp".to_string(),
            next_turn: 0,
            next_req_id: 1000,
            pending_reqs: HashMap::new(),
            stopping: false,
        }
    }
}

async fn emit_codex_turn(
    state: Arc<Mutex<ServerState>>,
    tx: mpsc::UnboundedSender<String>,
    turn_id: String,
    msg_count: usize,
    approvals: usize,
    payload_bytes: usize,
    interval_us: u64,
) {
    let (tid, cwd) = {
        let st = state.lock().await;
        (st.thread_id.clone(), st.cwd.clone())
    };
    let send_val = |v: Value| {
        let _ = tx.send(serde_json::to_string(&v).unwrap());
    };

    send_val(json!({
        "method": "turn/started",
        "params": {
            "threadId": tid,
            "turn": { "id": turn_id }
        }
    }));

    let filler = if payload_bytes > 80 {
        "y".repeat(payload_bytes - 64)
    } else {
        String::new()
    };
    let approval_every = if approvals > 0 {
        (msg_count / approvals).max(1)
    } else {
        usize::MAX
    };
    let mut sent_approvals = 0usize;

    for i in 0..msg_count {
        if state.lock().await.stopping {
            return;
        }
        let item_id = format!("item_{turn_id}_{i}");
        let ts = now_ns();
        let text = if filler.is_empty() {
            format!("bench_sent_ns:{ts} codex turn={turn_id} step={i}")
        } else {
            format!("bench_sent_ns:{ts} codex turn={turn_id} step={i} {filler}")
        };
        send_val(json!({
            "method": "item/started",
            "params": {
                "threadId": tid,
                "turnId": turn_id,
                "item": { "id": item_id, "type": "agentMessage" }
            }
        }));
        send_val(json!({
            "method": "item/completed",
            "params": {
                "threadId": tid,
                "turnId": turn_id,
                "item": { "id": item_id, "type": "agentMessage", "text": text }
            }
        }));

        if sent_approvals < approvals && (i % approval_every == 0) {
            sent_approvals += 1;
            let cmd_item = format!("cmd_{turn_id}_{sent_approvals}");
            let perm_ts = now_ns();
            let cmd = format!("echo bench_sent_ns:{perm_ts} codex approval {sent_approvals}");
            let req_id = {
                let mut st = state.lock().await;
                st.next_req_id += 1;
                let id = st.next_req_id;
                st.pending_reqs
                    .insert(id, (turn_id.clone(), cmd_item.clone(), cmd.clone()));
                id
            };
            send_val(json!({
                "method": "item/started",
                "params": {
                    "threadId": tid,
                    "turnId": turn_id,
                    "item": {
                        "id": cmd_item,
                        "type": "commandExecution",
                        "command": cmd,
                        "cwd": cwd,
                        "commandActions": []
                    }
                }
            }));
            send_val(json!({
                "id": req_id,
                "method": "item/commandExecution/requestApproval",
                "params": {
                    "threadId": tid,
                    "turnId": turn_id,
                    "itemId": cmd_item,
                    "command": cmd,
                    "cwd": cwd,
                    "reason": "Benchmark approval",
                    "environmentId": "local",
                    "availableDecisions": ["accept", "decline"]
                }
            }));
        }

        if interval_us > 0 {
            tokio::time::sleep(Duration::from_micros(interval_us)).await;
        }
    }

    // Wait briefly for any in-flight approvals to be answered before completing the turn
    if approvals > 0 {
        for _ in 0..100 {
            if state.lock().await.pending_reqs.is_empty() || state.lock().await.stopping {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    send_val(json!({
        "method": "turn/completed",
        "params": {
            "threadId": tid,
            "turn": { "id": turn_id, "status": "completed" }
        }
    }));
}

fn spawn_codex_workload(
    prompt: String,
    turn_id: String,
    state: Arc<Mutex<ServerState>>,
    tx: mpsc::UnboundedSender<String>,
) {
    tokio::spawn(async move {
        if let Some(rest) = prompt.strip_prefix("BENCH:burst:") {
            let parts: Vec<&str> = rest.split(':').collect();
            let count = parts.first().and_then(|s| s.parse().ok()).unwrap_or(100);
            let approvals = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            let interval_us = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(500);
            emit_codex_turn(state, tx, turn_id, count, approvals, 0, interval_us).await;
        } else if let Some(rest) = prompt.strip_prefix("BENCH:large:") {
            let parts: Vec<&str> = rest.split(':').collect();
            let events = parts.first().and_then(|s| s.parse().ok()).unwrap_or(200);
            let payload_bytes = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(32768);
            emit_codex_turn(state, tx, turn_id, events, 0, payload_bytes, 0).await;
        } else if let Some(rest) = prompt.strip_prefix("BENCH:soak:") {
            let parts: Vec<&str> = rest.split(':').collect();
            let interval_ms: u64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(1000);
            let approvals: usize = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
            emit_codex_turn(
                Arc::clone(&state),
                tx.clone(),
                turn_id,
                3,
                approvals,
                256,
                2000,
            )
            .await;
            loop {
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
                if state.lock().await.stopping {
                    break;
                }
                let next_tid = {
                    let mut st = state.lock().await;
                    st.next_turn += 1;
                    format!("turn_{}_{}", std::process::id(), st.next_turn)
                };
                emit_codex_turn(
                    Arc::clone(&state),
                    tx.clone(),
                    next_tid,
                    3,
                    approvals,
                    256,
                    2000,
                )
                .await;
            }
        } else {
            emit_codex_turn(state, tx, turn_id, 1, 0, 0, 0).await;
        }
    });
}

async fn handle_ws(stream: UnixStream, state: Arc<Mutex<ServerState>>) {
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut sink, mut source) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if sink.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = source.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(val) = serde_json::from_str::<Value>(&text) else {
            continue;
        };

        // Check if this is a response to our serverRequest (has id, no method)
        if val.get("method").is_none() {
            if let Some(req_id) = val.get("id").and_then(Value::as_i64) {
                let removed = {
                    let mut st = state.lock().await;
                    let tid = st.thread_id.clone();
                    let cwd = st.cwd.clone();
                    st.pending_reqs.remove(&req_id).map(|info| (tid, cwd, info))
                };
                if let Some((tid, cwd, (turn_id, item_id, cmd))) = removed {
                    let _ = tx.send(
                        serde_json::to_string(&json!({
                            "method": "serverRequest/resolved",
                            "params": { "threadId": tid, "requestId": req_id }
                        }))
                        .unwrap(),
                    );
                    let _ = tx.send(
                        serde_json::to_string(&json!({
                            "method": "item/completed",
                            "params": {
                                "threadId": tid,
                                "turnId": turn_id,
                                "item": {
                                    "id": item_id,
                                    "type": "commandExecution",
                                    "command": cmd,
                                    "cwd": cwd,
                                    "commandActions": [],
                                    "status": "completed",
                                    "exitCode": 0,
                                    "durationMs": 4,
                                    "aggregatedOutput": "ok\n"
                                }
                            }
                        }))
                        .unwrap(),
                    );
                }
            }
            continue;
        }

        let method = val["method"].as_str().unwrap_or_default();
        let id = val.get("id").cloned();

        match method {
            "initialize" => {
                if let Some(id) = id {
                    let _ = tx.send(
                        serde_json::to_string(&json!({
                            "id": id,
                            "result": { "userAgent": "codex-cli/0.170.0" }
                        }))
                        .unwrap(),
                    );
                }
            }
            "initialized" => {}
            "skills/list" => {
                if let Some(id) = id {
                    let _ = tx.send(
                        serde_json::to_string(&json!({
                            "id": id,
                            "result": { "data": [] }
                        }))
                        .unwrap(),
                    );
                }
            }
            "thread/start" | "thread/resume" => {
                let tid = {
                    let mut st = state.lock().await;
                    if let Some(cwd) = val["params"]["cwd"].as_str() {
                        st.cwd = cwd.to_string();
                    }
                    if let Some(req_tid) = val["params"]["threadId"].as_str() {
                        st.thread_id = req_tid.to_string();
                    }
                    st.thread_id.clone()
                };
                if let Some(id) = id {
                    let _ = tx.send(
                        serde_json::to_string(&json!({
                            "id": id,
                            "result": {
                                "thread": { "id": tid, "turns": [] },
                                "model": "gpt-5.3-codex",
                                "approvalPolicy": "on-request",
                                "sandbox": "workspace-write"
                            }
                        }))
                        .unwrap(),
                    );
                }
            }
            "turn/start" | "turn/steer" => {
                let prompt = val["params"]["input"]
                    .as_array()
                    .and_then(|arr| arr.first())
                    .and_then(|item| item["text"].as_str())
                    .unwrap_or_default()
                    .to_string();
                let turn_id = {
                    let mut st = state.lock().await;
                    st.next_turn += 1;
                    format!("turn_{}_{}", std::process::id(), st.next_turn)
                };
                if let Some(id) = id {
                    let _ = tx.send(
                        serde_json::to_string(&json!({
                            "id": id,
                            "result": { "turn": { "id": turn_id } }
                        }))
                        .unwrap(),
                    );
                }
                spawn_codex_workload(prompt, turn_id, Arc::clone(&state), tx.clone());
            }
            "turn/interrupt" => {
                let tid = state.lock().await.thread_id.clone();
                let turn_id = val["params"]["turnId"]
                    .as_str()
                    .unwrap_or("turn_1")
                    .to_string();
                if let Some(id) = id {
                    let _ = tx.send(
                        serde_json::to_string(&json!({ "id": id, "result": {} })).unwrap(),
                    );
                }
                let _ = tx.send(
                    serde_json::to_string(&json!({
                        "method": "turn/completed",
                        "params": {
                            "threadId": tid,
                            "turn": { "id": turn_id, "status": "interrupted" }
                        }
                    }))
                    .unwrap(),
                );
            }
            _ => {
                if let Some(id) = id {
                    let _ = tx.send(
                        serde_json::to_string(&json!({ "id": id, "result": {} })).unwrap(),
                    );
                }
            }
        }
    }

    drop(tx);
    let _ = writer.await;
}

#[tokio::main(flavor = "current_thread")]
async fn run_uds_app_server(sock_path: PathBuf) -> Result<()> {
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;
    let state = Arc::new(Mutex::new(ServerState::new()));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let st = Arc::clone(&state);
                        tokio::spawn(async move {
                            handle_ws(stream, st).await;
                        });
                    }
                    Err(_) => break,
                }
            }
            _ = term.recv() => {
                state.lock().await.stopping = true;
                break;
            }
        }
    }
    let _ = std::fs::remove_file(&sock_path);
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!("codex-cli 0.170.0");
        return Ok(());
    }
    if args.len() >= 2 && args[0] == "login" && args[1] == "status" {
        println!("Logged in");
        return Ok(());
    }
    // Check for `--listen unix://<path>`
    if let Some(pos) = args.iter().position(|a| a == "--listen") {
        if let Some(target) = args.get(pos + 1) {
            if let Some(path) = target.strip_prefix("unix://") {
                return run_uds_app_server(PathBuf::from(path));
            }
        }
    }
    if args.iter().any(|a| a == "app-server") {
        return run_stdio_app_server();
    }
    eprintln!("mock-codex: unexpected args: {args:?}");
    std::process::exit(1);
}
