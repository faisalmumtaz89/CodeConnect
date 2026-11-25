//! Lightweight offline mock for `cc-agent-host` speaking `protocol::host` JSONL over `host.sock`.
//! Uses a single-threaded Tokio runtime so 50+ concurrent mock sessions consume minimal host RAM/CPU.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

struct SharedState {
    next_seq: u64,
    acked: u64,
    buffer: VecDeque<(u64, String)>,
    pending_perms: Vec<Value>,
    session_id: String,
    cwd: String,
    stopping: bool,
    exited_seq: Option<u64>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            next_seq: 1,
            acked: 0,
            buffer: VecDeque::new(),
            pending_perms: Vec::new(),
            session_id: format!("mock-claude-{}", std::process::id()),
            cwd: "/tmp".to_string(),
            stopping: false,
            exited_seq: None,
        }
    }

    fn alloc_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }

    fn record(&mut self, seq: u64, line: String) {
        self.buffer.push_back((seq, line));
    }

    fn ack(&mut self, seq: u64) -> bool {
        if seq > self.acked {
            self.acked = seq;
            while let Some((front_seq, _)) = self.buffer.front() {
                if *front_seq <= self.acked {
                    self.buffer.pop_front();
                } else {
                    break;
                }
            }
        }
        self.exited_seq.is_some_and(|e| self.acked >= e)
    }
}

async fn emit_line(
    state: &Arc<Mutex<SharedState>>,
    tx: &mpsc::UnboundedSender<String>,
    mut msg: Value,
) -> u64 {
    let mut st = state.lock().await;
    let seq = st.alloc_seq();
    msg["seq"] = json!(seq);
    let line = serde_json::to_string(&msg).unwrap();
    st.record(seq, line.clone());
    drop(st);
    let _ = tx.send(line);
    seq
}

async fn emit_turn(
    state: &Arc<Mutex<SharedState>>,
    tx: &mpsc::UnboundedSender<String>,
    turn_idx: u64,
    msg_count: usize,
    approvals: usize,
    payload_bytes: usize,
    interval_us: u64,
) {
    let pid = std::process::id();
    let (sid, cwd) = {
        let st = state.lock().await;
        (st.session_id.clone(), st.cwd.clone())
    };
    let turn_uuid = format!("init-{pid}-{turn_idx}");
    emit_line(
        state,
        tx,
        json!({
            "type": "sdk",
            "message": {
                "type": "system",
                "subtype": "init",
                "uuid": turn_uuid,
                "session_id": sid,
                "cwd": cwd,
                "model": "claude-sonnet-4-6",
                "permissionMode": "default"
            }
        }),
    )
    .await;

    let filler = if payload_bytes > 80 {
        "x".repeat(payload_bytes - 64)
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
        let ts = now_ns();
        let text = if filler.is_empty() {
            format!("bench_sent_ns:{ts} turn={turn_idx} step={i} working on task")
        } else {
            format!("bench_sent_ns:{ts} turn={turn_idx} step={i} {filler}")
        };
        let tool_id = format!("toolu_{pid}_{turn_idx}_{i}");
        emit_line(
            state,
            tx,
            json!({
                "type": "sdk",
                "message": {
                    "type": "assistant",
                    "uuid": format!("asst-{pid}-{turn_idx}-{i}"),
                    "parent_tool_use_id": null,
                    "message": {
                        "model": "claude-sonnet-4-6",
                        "content": [
                            {"type": "text", "text": text},
                            {"type": "tool_use", "id": tool_id, "name": "Bash", "input": {"command": format!("echo step {i}")}}
                        ],
                        "usage": {
                            "input_tokens": 1200 + i as u64,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 500,
                            "output_tokens": 180
                        }
                    }
                }
            }),
        )
        .await;

        if sent_approvals < approvals && (i % approval_every == 0) {
            sent_approvals += 1;
            let perm_id = format!("toolu_perm_{pid}_{turn_idx}_{sent_approvals}");
            let perm_ts = now_ns();
            let req = json!({
                "tool_use_id": perm_id,
                "tool_name": "Bash",
                "input": {
                    "command": format!("echo bench_sent_ns:{perm_ts} approval {sent_approvals}"),
                    "description": "Benchmark tool approval"
                },
                "options": {
                    "title": "Run shell command",
                    "displayName": "Bash"
                }
            });
            state.lock().await.pending_perms.push(req.clone());
            emit_line(
                state,
                tx,
                json!({
                    "type": "permission",
                    "request": req
                }),
            )
            .await;
        } else {
            let res_ts = now_ns();
            let result_text = if filler.is_empty() {
                format!("bench_sent_ns:{res_ts} ok step {i}")
            } else {
                format!("bench_sent_ns:{res_ts} {filler}")
            };
            emit_line(
                state,
                tx,
                json!({
                    "type": "sdk",
                    "message": {
                        "type": "user",
                        "uuid": format!("tr-{pid}-{turn_idx}-{i}"),
                        "parent_tool_use_id": null,
                        "message": {
                            "content": [
                                {"type": "tool_result", "tool_use_id": tool_id, "content": result_text}
                            ]
                        }
                    }
                }),
            )
            .await;
        }

        if interval_us > 0 {
            tokio::time::sleep(Duration::from_micros(interval_us)).await;
        }
    }

    emit_line(
        state,
        tx,
        json!({
            "type": "sdk",
            "message": {
                "type": "result",
                "uuid": format!("res-{pid}-{turn_idx}"),
                "subtype": "success",
                "is_error": false,
                "num_turns": 1,
                "usage": {
                    "input_tokens": 2000,
                    "output_tokens": 500
                },
                "total_cost_usd": 0.002,
                "modelUsage": {
                    "claude-sonnet-4-6": { "contextWindow": 200000 }
                }
            }
        }),
    )
    .await;
}

fn spawn_workload(
    prompt: String,
    state: Arc<Mutex<SharedState>>,
    tx: mpsc::UnboundedSender<String>,
    turn_counter: Arc<Mutex<u64>>,
) {
    tokio::spawn(async move {
        if let Some(rest) = prompt.strip_prefix("BENCH:burst:") {
            // BENCH:burst:<count>:<approvals>:<interval_us>
            let parts: Vec<&str> = rest.split(':').collect();
            let count = parts.first().and_then(|s| s.parse().ok()).unwrap_or(100);
            let approvals = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            let interval_us = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(500);
            let t = {
                let mut c = turn_counter.lock().await;
                *c += 1;
                *c
            };
            emit_turn(&state, &tx, t, count, approvals, 0, interval_us).await;
        } else if let Some(rest) = prompt.strip_prefix("BENCH:large:") {
            // BENCH:large:<events>:<payload_bytes>
            let parts: Vec<&str> = rest.split(':').collect();
            let events = parts.first().and_then(|s| s.parse().ok()).unwrap_or(200);
            let payload_bytes = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(32768);
            let t = {
                let mut c = turn_counter.lock().await;
                *c += 1;
                *c
            };
            emit_turn(&state, &tx, t, events, 0, payload_bytes, 0).await;
        } else if let Some(rest) = prompt.strip_prefix("BENCH:soak:") {
            // BENCH:soak:<interval_ms>:<approvals_per_turn>
            let parts: Vec<&str> = rest.split(':').collect();
            let interval_ms: u64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(1000);
            let approvals: usize = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
            loop {
                if state.lock().await.stopping {
                    break;
                }
                let t = {
                    let mut c = turn_counter.lock().await;
                    *c += 1;
                    *c
                };
                emit_turn(&state, &tx, t, 3, approvals, 256, 2000).await;
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            }
        } else {
            // Default / BENCH:idle: one quick turn then stay idle.
            let t = {
                let mut c = turn_counter.lock().await;
                *c += 1;
                *c
            };
            emit_turn(&state, &tx, t, 1, 0, 0, 0).await;
        }
    });
}

async fn handle_conn(
    stream: UnixStream,
    state: Arc<Mutex<SharedState>>,
    turn_counter: Arc<Mutex<u64>>,
    exit_tx: mpsc::UnboundedSender<()>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let writer = tokio::spawn(async move {
        while let Some(mut line) = rx.recv().await {
            line.push('\n');
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(read_half).lines();
    let mut started = false;

    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = msg["type"].as_str().unwrap_or_default();
        match kind {
            "attach" => {
                let after = msg["after"].as_u64().unwrap_or(0);
                let replay: Vec<String> = {
                    let st = state.lock().await;
                    st.buffer
                        .iter()
                        .filter(|(s, _)| *s > after)
                        .map(|(_, l)| l.clone())
                        .collect()
                };
                for r in replay {
                    let _ = tx.send(r);
                }
                let (sid, pending) = {
                    let st = state.lock().await;
                    (st.session_id.clone(), st.pending_perms.clone())
                };
                emit_line(
                    &state,
                    &tx,
                    json!({
                        "type": "state",
                        "session_id": sid,
                        "model": "claude-sonnet-4-6",
                        "permission_mode": "default",
                        "pending": pending
                    }),
                )
                .await;
            }
            "ack" => {
                if let Some(seq) = msg["seq"].as_u64() {
                    let should_exit = state.lock().await.ack(seq);
                    if should_exit {
                        let _ = exit_tx.send(());
                        break;
                    }
                }
            }
            "start" => {
                let uuid = msg["uuid"].as_str().unwrap_or("start").to_string();
                let prompt = msg["prompt"].as_str().unwrap_or_default().to_string();
                if let Some(cwd) = msg["options"]["cwd"].as_str() {
                    state.lock().await.cwd = cwd.to_string();
                }
                if !started {
                    started = true;
                    emit_line(
                        &state,
                        &tx,
                        json!({
                            "type": "outcome",
                            "uuid": uuid,
                            "status": "done",
                            "initialize": {
                                "commands": [
                                    {"name": "help", "description": "Show help"},
                                    {"name": "compact", "description": "Compact context"}
                                ],
                                "models": [
                                    {"value": "claude-sonnet-4-6", "displayName": "Claude Sonnet 4.6", "description": "Fast"}
                                ],
                                "account": {"api_provider": "firstParty"},
                                "claude_code_version": "2.1.300"
                            }
                        }),
                    )
                    .await;
                    spawn_workload(prompt, Arc::clone(&state), tx.clone(), Arc::clone(&turn_counter));
                }
            }
            "say" | "wake" => {
                let uuid = msg["uuid"].as_str().unwrap_or("say").to_string();
                let text = msg["text"].as_str().unwrap_or_default().to_string();
                emit_line(
                    &state,
                    &tx,
                    json!({
                        "type": "outcome",
                        "uuid": uuid,
                        "status": "done"
                    }),
                )
                .await;
                spawn_workload(text, Arc::clone(&state), tx.clone(), Arc::clone(&turn_counter));
            }
            "answer" => {
                let uuid = msg["uuid"].as_str().unwrap_or("ans").to_string();
                let tool_use_id = msg["tool_use_id"].as_str().unwrap_or_default().to_string();
                state
                    .lock()
                    .await
                    .pending_perms
                    .retain(|p| p["tool_use_id"].as_str() != Some(tool_use_id.as_str()));
                emit_line(
                    &state,
                    &tx,
                    json!({
                        "type": "outcome",
                        "uuid": uuid,
                        "status": "done"
                    }),
                )
                .await;
                let ts = now_ns();
                emit_line(
                    &state,
                    &tx,
                    json!({
                        "type": "sdk",
                        "message": {
                            "type": "user",
                            "uuid": format!("ans-tr-{}-{ts}", std::process::id()),
                            "parent_tool_use_id": null,
                            "message": {
                                "content": [
                                    {"type": "tool_result", "tool_use_id": tool_use_id, "content": format!("bench_sent_ns:{ts} approved")}
                                ]
                            }
                        }
                    }),
                )
                .await;
            }
            "interrupt" => {
                let uuid = msg["uuid"].as_str().unwrap_or("int").to_string();
                emit_line(
                    &state,
                    &tx,
                    json!({
                        "type": "outcome",
                        "uuid": uuid,
                        "status": "done",
                        "terminal_reason": "aborted_tools"
                    }),
                )
                .await;
            }
            "set_model" | "set_mode" => {
                let uuid = msg["uuid"].as_str().unwrap_or("set").to_string();
                emit_line(
                    &state,
                    &tx,
                    json!({
                        "type": "outcome",
                        "uuid": uuid,
                        "status": "done"
                    }),
                )
                .await;
            }
            "stop" => {
                let uuid = msg["uuid"].as_str().unwrap_or("stop").to_string();
                state.lock().await.stopping = true;
                emit_line(
                    &state,
                    &tx,
                    json!({
                        "type": "outcome",
                        "uuid": uuid,
                        "status": "done"
                    }),
                )
                .await;
                let ex_seq = emit_line(
                    &state,
                    &tx,
                    json!({
                        "type": "exited",
                        "code": 0
                    }),
                )
                .await;
                state.lock().await.exited_seq = Some(ex_seq);
                let exit_tx_timer = exit_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let _ = exit_tx_timer.send(());
                });
            }
            _ => {}
        }
    }

    drop(tx);
    let _ = writer.await;
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let run_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("usage: mock-claude-host <run_dir>")?,
    );
    let pid = std::process::id() as i32;
    let host_json_path = run_dir.join("host.json");
    let body = serde_json::to_vec(&json!({
        "protocol_version": 1,
        "host_pid": pid
    }))?;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&host_json_path)
        .with_context(|| format!("creating {}", host_json_path.display()))?;
    f.write_all(&body)?;
    f.flush()?;
    drop(f);

    let sock_path = run_dir.join("host.sock");
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;

    let state = Arc::new(Mutex::new(SharedState::new()));
    let turn_counter = Arc::new(Mutex::new(0u64));
    let (exit_tx, mut exit_rx) = mpsc::unbounded_channel::<()>();

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let st = Arc::clone(&state);
                        let tc = Arc::clone(&turn_counter);
                        let etx = exit_tx.clone();
                        tokio::spawn(async move {
                            handle_conn(stream, st, tc, etx).await;
                        });
                    }
                    Err(_) => break,
                }
            }
            _ = exit_rx.recv() => break,
            _ = term.recv() => break,
        }
    }
    let _ = std::fs::remove_file(&sock_path);
    Ok(())
}
