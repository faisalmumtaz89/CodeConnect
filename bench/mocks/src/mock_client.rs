//! Async mock iPhone / Mac client for CodeConnect offline benchmarks.
//! Speaks the real `protocol::ws` (WebSocket) and `protocol::ipc` (Unix socket) wire formats.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn summarize_us(samples: &mut [f64]) -> Value {
    if samples.is_empty() {
        return json!({"count": 0, "min_ms": 0.0, "p50_ms": 0.0, "p95_ms": 0.0, "p99_ms": 0.0, "max_ms": 0.0});
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    json!({
        "count": samples.len(),
        "min_ms": samples[0] / 1000.0,
        "p50_ms": percentile(samples, 0.50) / 1000.0,
        "p95_ms": percentile(samples, 0.95) / 1000.0,
        "p99_ms": percentile(samples, 0.99) / 1000.0,
        "max_ms": samples[samples.len() - 1] / 1000.0,
    })
}

fn extract_bench_sent_ns(val: &Value) -> Option<u128> {
    let s = val.to_string();
    let idx = s.find("bench_sent_ns:")?;
    let rest = &s[idx + "bench_sent_ns:".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u128>().ok()
}

async fn ipc_call(sock_path: &PathBuf, req: &Value) -> Result<Value> {
    let stream = UnixStream::connect(sock_path)
        .await
        .with_context(|| format!("connect IPC {}", sock_path.display()))?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    let mut reader = BufReader::new(r).lines();
    let reply = reader
        .next_line()
        .await?
        .context("IPC closed without reply")?;
    Ok(serde_json::from_str(&reply)?)
}

async fn ws_send(ws: &mut WsStream, msg: &Value) -> Result<()> {
    ws.send(Message::Text(serde_json::to_string(msg)?)).await?;
    Ok(())
}

async fn ws_recv_json(ws: &mut WsStream, timeout: Duration) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            bail!("timed out waiting for WS frame");
        }
        let msg = tokio::time::timeout(rem, ws.next())
            .await?
            .context("WS closed")??;
        match msg {
            Message::Text(t) => return Ok(serde_json::from_str(&t)?),
            Message::Ping(p) => {
                let _ = ws.send(Message::Pong(p)).await;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => bail!("WS closed by peer"),
            _ => continue,
        }
    }
}

async fn pair_and_connect(ws_url: &str, sock_path: &PathBuf) -> Result<(WsStream, String)> {
    let pairing = ipc_call(sock_path, &json!({"type": "create_pairing", "ttl_secs": 300})).await?;
    let code = pairing["code"]
        .as_str()
        .with_context(|| format!("no pairing code in {pairing}"))?
        .to_string();
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await?;
    ws_send(
        &mut ws,
        &json!({
            "type": "hello",
            "protocol_version": 2,
            "pairing_code": code,
            "client_name": "BenchClient",
            "device_name": "BenchPhone"
        }),
    )
    .await?;
    let ack = ws_recv_json(&mut ws, Duration::from_secs(10)).await?;
    if ack["type"] != "hello_ack" {
        bail!("expected hello_ack, got {ack}");
    }
    let token = ack["device_token"]
        .as_str()
        .context("hello_ack missing device_token")?
        .to_string();
    Ok((ws, token))
}

async fn connect_with_token(ws_url: &str, token: &str) -> Result<WsStream> {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await?;
    ws_send(
        &mut ws,
        &json!({
            "type": "hello",
            "protocol_version": 2,
            "token": token,
            "client_name": "BenchClient",
            "device_name": "BenchPhone"
        }),
    )
    .await?;
    let ack = ws_recv_json(&mut ws, Duration::from_secs(10)).await?;
    if ack["type"] != "hello_ack" {
        bail!("expected hello_ack, got {ack}");
    }
    Ok(ws)
}

async fn start_sessions(
    ws: &mut WsStream,
    claude_count: usize,
    codex_count: usize,
    workdir: &str,
    claude_prompt: &str,
    codex_prompt: &str,
) -> Result<(Vec<(String, String)>, Vec<f64>)> {
    // Send all start_session requests concurrently (pipelined), then collect results.
    let mut pending_reqs: HashMap<String, (String, Instant)> = HashMap::new();
    let mut idx = 0usize;
    for _ in 0..claude_count {
        idx += 1;
        let rid = format!("start-claude-{idx}-{}", now_ns());
        pending_reqs.insert(rid.clone(), ("claude".to_string(), Instant::now()));
        ws_send(
            ws,
            &json!({
                "type": "start_session",
                "request_id": rid,
                "path": workdir,
                "agent": "claude",
                "prompt": claude_prompt,
                "trust": "trusted"
            }),
        )
        .await?;
    }
    for _ in 0..codex_count {
        idx += 1;
        let rid = format!("start-codex-{idx}-{}", now_ns());
        pending_reqs.insert(rid.clone(), ("codex".to_string(), Instant::now()));
        ws_send(
            ws,
            &json!({
                "type": "start_session",
                "request_id": rid,
                "path": workdir,
                "agent": "codex",
                "prompt": codex_prompt,
                "trust": "trusted"
            }),
        )
        .await?;
    }

    let mut sessions = Vec::new();
    let mut start_latencies_us = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while !pending_reqs.is_empty() {
        let rem = deadline.saturating_duration_since(Instant::now());
        let msg = ws_recv_json(ws, rem).await?;
        if msg["type"] == "start_session_result" {
            let rid = msg["request_id"].as_str().unwrap_or_default().to_string();
            if let Some((agent, t0)) = pending_reqs.remove(&rid) {
                if msg["result"]["status"] == "started" {
                    let uid = msg["result"]["session_uid"]
                        .as_str()
                        .context("missing session_uid")?
                        .to_string();
                    start_latencies_us.push(t0.elapsed().as_secs_f64() * 1e6);
                    sessions.push((uid, agent));
                } else {
                    bail!("start_session failed for {agent}: {msg}");
                }
            }
        }
    }
    Ok((sessions, start_latencies_us))
}

async fn stop_all_sessions(ws: &mut WsStream, sessions: &[(String, String)]) -> Result<()> {
    for (uid, _) in sessions {
        let rid = format!("stop-{uid}");
        let _ = ws_send(
            ws,
            &json!({
                "type": "stop_run",
                "session_id": uid,
                "request_id": rid
            }),
        )
        .await;
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut stopped = 0usize;
    while stopped < sessions.len() && Instant::now() < deadline {
        if let Ok(msg) = ws_recv_json(ws, Duration::from_millis(500)).await {
            if msg["type"] == "stop_run_result" {
                stopped += 1;
            }
        }
    }
    Ok(())
}

fn get_arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let ws_url = get_arg(&args, "--ws").unwrap_or_else(|| "ws://127.0.0.1:8787".to_string());
    let sock_path = PathBuf::from(get_arg(&args, "--sock").unwrap_or_default());

    match cmd {
        "wait-ready" => {
            let t0 = Instant::now();
            let timeout = Duration::from_secs(
                get_arg(&args, "--timeout-secs")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(15),
            );
            let deadline = t0 + timeout;
            loop {
                if let Ok(info) = ipc_call(&sock_path, &json!({"type": "daemon_info"})).await {
                    if info["type"] == "daemon" {
                        if let Ok((mut ws, token)) = pair_and_connect(&ws_url, &sock_path).await {
                            let ready_ms = t0.elapsed().as_secs_f64() * 1000.0;
                            let _ = ws.close(None).await;
                            println!(
                                "{}",
                                json!({
                                    "ready_ms": ready_ms,
                                    "device_token": token
                                })
                            );
                            return Ok(());
                        }
                    }
                }
                if Instant::now() >= deadline {
                    bail!("daemon did not become ready within {:?}", timeout);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        "idle-client" => {
            let duration_secs: u64 = get_arg(&args, "--duration-secs")
                .and_then(|s| s.parse().ok())
                .unwrap_or(60);
            let t0 = Instant::now();
            let (mut ws, _token) = pair_and_connect(&ws_url, &sock_path).await?;
            let connect_ms = t0.elapsed().as_secs_f64() * 1000.0;
            ws_send(&mut ws, &json!({"type": "sessions"})).await?;
            let mut frames = 0u64;
            let end = Instant::now() + Duration::from_secs(duration_secs);
            while Instant::now() < end {
                let rem = end.saturating_duration_since(Instant::now());
                match tokio::time::timeout(rem.min(Duration::from_secs(5)), ws.next()).await {
                    Ok(Some(Ok(Message::Text(_)))) => frames += 1,
                    Ok(Some(Ok(Message::Ping(p)))) => {
                        let _ = ws.send(Message::Pong(p)).await;
                    }
                    Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                    _ => {}
                }
            }
            let _ = ws.close(None).await;
            println!(
                "{}",
                json!({
                    "connect_ms": connect_ms,
                    "frames_received": frames,
                    "duration_secs": duration_secs
                })
            );
        }
        "sessions" => {
            let claude: usize = get_arg(&args, "--claude")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            let codex: usize = get_arg(&args, "--codex")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let workdir = get_arg(&args, "--workdir").context("--workdir required")?;
            let hold_secs: u64 = get_arg(&args, "--hold-secs")
                .and_then(|s| s.parse().ok())
                .unwrap_or(15);
            let stop_after = args.iter().any(|a| a == "--stop-after");

            let (mut ws, _token) = pair_and_connect(&ws_url, &sock_path).await?;
            let (sessions, mut start_lat) =
                start_sessions(&mut ws, claude, codex, &workdir, "BENCH:idle", "BENCH:idle")
                    .await?;
            for (uid, _) in &sessions {
                ws_send(
                    &mut ws,
                    &json!({"type": "subscribe", "session_id": uid, "after_seq": 0}),
                )
                .await?;
            }
            let mut completed: HashSet<String> = HashSet::new();
            let mut relay_us = Vec::new();
            let mut events_rx = 0u64;
            let wait_done = Instant::now() + Duration::from_secs(60);
            while completed.len() < sessions.len() && Instant::now() < wait_done {
                if let Ok(msg) = ws_recv_json(&mut ws, Duration::from_secs(2)).await {
                    if msg["type"] == "event" {
                        events_rx += 1;
                        let ev = &msg["event"];
                        if let Some(sent_ns) = extract_bench_sent_ns(ev) {
                            let now = now_ns();
                            if now >= sent_ns {
                                relay_us.push((now - sent_ns) as f64 / 1000.0);
                            }
                        }
                        if ev["kind"] == "turn_complete" {
                            if let Some(uid) = ev["session_uid"].as_str() {
                                completed.insert(uid.to_string());
                            }
                        }
                    }
                }
            }
            // Measure `sessions` listing RPC latency with N live sessions
            let mut sessions_rpc_us = Vec::new();
            for _ in 0..5 {
                let t0 = Instant::now();
                ws_send(&mut ws, &json!({"type": "sessions"})).await?;
                let rpc_dl = Instant::now() + Duration::from_secs(15);
                while Instant::now() < rpc_dl {
                    if let Ok(msg) = ws_recv_json(&mut ws, Duration::from_secs(2)).await {
                        if msg["type"] == "sessions" {
                            sessions_rpc_us.push(t0.elapsed().as_secs_f64() * 1e6);
                            break;
                        }
                    }
                }
            }
            // Hold steady so driver can sample settled memory & CPU with N live sessions
            let hold_end = Instant::now() + Duration::from_secs(hold_secs);
            while Instant::now() < hold_end {
                let rem = hold_end.saturating_duration_since(Instant::now());
                let _ = tokio::time::timeout(rem.min(Duration::from_secs(2)), ws.next()).await;
            }
            if stop_after {
                let _ = stop_all_sessions(&mut ws, &sessions).await;
            }
            let _ = ws.close(None).await;
            println!(
                "{}",
                json!({
                    "sessions_started": sessions.len(),
                    "sessions_completed_init": completed.len(),
                    "events_received": events_rx,
                    "start_latency": summarize_us(&mut start_lat),
                    "relay_latency": summarize_us(&mut relay_us),
                    "sessions_rpc_latency": summarize_us(&mut sessions_rpc_us),
                })
            );
        }
        "burst" => {
            let claude: usize = get_arg(&args, "--claude")
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            let codex: usize = get_arg(&args, "--codex")
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            let events_per: usize = get_arg(&args, "--events-per-session")
                .and_then(|s| s.parse().ok())
                .unwrap_or(100);
            let approvals_per: usize = get_arg(&args, "--approvals-per-session")
                .and_then(|s| s.parse().ok())
                .unwrap_or(10);
            let interval_us: u64 = get_arg(&args, "--interval-us")
                .and_then(|s| s.parse().ok())
                .unwrap_or(500);
            let workdir = get_arg(&args, "--workdir").context("--workdir required")?;

            let (mut ws, _token) = pair_and_connect(&ws_url, &sock_path).await?;
            let burst_t0 = Instant::now();
            let burst_cmd = format!("BENCH:burst:{events_per}:{approvals_per}:{interval_us}");
            let (sessions, _) =
                start_sessions(&mut ws, claude, codex, &workdir, &burst_cmd, &burst_cmd).await?;
            let all_agents: HashMap<String, String> = sessions.iter().cloned().collect();
            for (uid, _) in &sessions {
                ws_send(
                    &mut ws,
                    &json!({"type": "subscribe", "session_id": uid, "after_seq": 0}),
                )
                .await?;
            }
            let expected_uids: HashSet<String> =
                sessions.iter().map(|(u, _)| u.clone()).collect();

            let mut burst_done: HashSet<String> = HashSet::new();
            let mut relay_us = Vec::new();
            let mut approval_rtt_us = Vec::new();
            let mut pending_answers: HashMap<String, Instant> = HashMap::new();
            let mut answered_ids: HashSet<String> = HashSet::new();
            let mut events_rx = 0u64;
            let mut approvals_answered = 0u64;
            let deadline = Instant::now() + Duration::from_secs(90);

            while burst_done.len() < expected_uids.len() && Instant::now() < deadline {
                let Ok(msg) = ws_recv_json(&mut ws, Duration::from_secs(2)).await else {
                    continue;
                };
                match msg["type"].as_str().unwrap_or_default() {
                    "event" => {
                        events_rx += 1;
                        let ev = &msg["event"];
                        let uid = ev["session_uid"].as_str().unwrap_or_default();
                        if let Some(sent_ns) = extract_bench_sent_ns(ev) {
                            let now = now_ns();
                            if now >= sent_ns {
                                relay_us.push((now - sent_ns) as f64 / 1000.0);
                            }
                        }
                        if ev["kind"] == "approval_request" {
                            let card = if ev["payload"]["card"].is_object() {
                                &ev["payload"]["card"]
                            } else {
                                &ev["payload"]
                            };
                            let req_id = card["request_id"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string();
                            let hash = card["payload_hash"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string();
                            if !req_id.is_empty() && !hash.is_empty() && answered_ids.insert(req_id.clone()) {
                                let is_codex = all_agents.get(uid).is_some_and(|a| a == "codex");
                                let decision = if is_codex {
                                    let opt_id = card["options"]
                                        .as_array()
                                        .and_then(|arr| arr.first())
                                        .and_then(|o| o["id"].as_str())
                                        .unwrap_or("accept");
                                    json!({"type": "option_id", "option_id": opt_id})
                                } else {
                                    json!({"type": "allow"})
                                };
                                pending_answers.insert(req_id.clone(), Instant::now());
                                let _ = ws_send(
                                    &mut ws,
                                    &json!({
                                        "type": "answer",
                                        "request_id": req_id,
                                        "payload_hash": hash,
                                        "decision": decision,
                                        "session_id": uid
                                    }),
                                )
                                .await;
                            }
                        } else if ev["kind"] == "turn_complete" && expected_uids.contains(uid) {
                            burst_done.insert(uid.to_string());
                        }
                    }
                    "answer_result" => {
                        let req_id = msg["request_id"].as_str().unwrap_or_default();
                        if let Some(t0) = pending_answers.remove(req_id) {
                            approval_rtt_us.push(t0.elapsed().as_secs_f64() * 1e6);
                            approvals_answered += 1;
                        }
                    }
                    _ => {}
                }
            }
            let elapsed_s = burst_t0.elapsed().as_secs_f64();
            let _ = stop_all_sessions(&mut ws, &sessions).await;
            let _ = ws.close(None).await;
            println!(
                "{}",
                json!({
                    "elapsed_secs": elapsed_s,
                    "events_received": events_rx,
                    "events_per_sec": events_rx as f64 / elapsed_s.max(0.001),
                    "approvals_answered": approvals_answered,
                    "sessions_finished": burst_done.len(),
                    "sessions_expected": expected_uids.len(),
                    "relay_latency": summarize_us(&mut relay_us),
                    "approval_rtt": summarize_us(&mut approval_rtt_us),
                })
            );
        }
        "large" => {
            let events: usize = get_arg(&args, "--events")
                .and_then(|s| s.parse().ok())
                .unwrap_or(200);
            let payload_bytes: usize = get_arg(&args, "--payload-bytes")
                .and_then(|s| s.parse().ok())
                .unwrap_or(65536);
            let diff_iters: usize = get_arg(&args, "--diff-iters")
                .and_then(|s| s.parse().ok())
                .unwrap_or(10);
            let workdir = get_arg(&args, "--workdir").context("--workdir required")?;

            let (mut ws, _token) = pair_and_connect(&ws_url, &sock_path).await?;
            let prompt = format!("BENCH:large:{events}:{payload_bytes}");
            let (sessions, _) = start_sessions(&mut ws, 1, 0, &workdir, &prompt, "").await?;
            let uid = sessions[0].0.clone();
            ws_send(
                &mut ws,
                &json!({"type": "subscribe", "session_id": uid, "after_seq": 0}),
            )
            .await?;

            let mut max_seq = 0u64;
            let mut events_rx = 0u64;
            let mut bytes_rx = 0u64;
            let mut relay_us = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(120);
            while Instant::now() < deadline {
                let Ok(msg) = ws_recv_json(&mut ws, Duration::from_secs(5)).await else {
                    break;
                };
                if msg["type"] == "event" {
                    events_rx += 1;
                    let ev = &msg["event"];
                    let s = ev.to_string();
                    bytes_rx += s.len() as u64;
                    if let Some(seq) = ev["seq"].as_u64() {
                        max_seq = max_seq.max(seq);
                    }
                    if let Some(sent_ns) = extract_bench_sent_ns(ev) {
                        let now = now_ns();
                        if now >= sent_ns {
                            relay_us.push((now - sent_ns) as f64 / 1000.0);
                        }
                    }
                    if ev["kind"] == "turn_complete" {
                        break;
                    }
                }
            }

            // Exercise history paging backwards across the whole long transcript
            let mut history_us = Vec::new();
            let mut cursor = max_seq + 1;
            let mut pages = 0u64;
            while cursor > 1 && pages < 20 {
                pages += 1;
                let rid = format!("hist-{pages}");
                let t0 = Instant::now();
                ws_send(
                    &mut ws,
                    &json!({
                        "type": "history",
                        "session_id": uid,
                        "before_seq": cursor,
                        "limit": 100,
                        "request_id": rid
                    }),
                )
                .await?;
                loop {
                    let msg = ws_recv_json(&mut ws, Duration::from_secs(30)).await?;
                    if msg["type"] == "history_end" && msg["request_id"] == rid {
                        history_us.push(t0.elapsed().as_secs_f64() * 1e6);
                        let first_seq = msg["first_seq"].as_u64().unwrap_or(1);
                        if first_seq >= cursor {
                            cursor = 1;
                        } else {
                            cursor = first_seq;
                        }
                        break;
                    }
                }
            }

            // Exercise get_diff on large modified git repo
            let mut diff_us = Vec::new();
            let mut diff_bytes = 0u64;
            for _ in 0..diff_iters {
                let t0 = Instant::now();
                ws_send(&mut ws, &json!({"type": "get_diff", "session_id": uid})).await?;
                loop {
                    let msg = ws_recv_json(&mut ws, Duration::from_secs(30)).await?;
                    if msg["type"] == "diff" {
                        diff_us.push(t0.elapsed().as_secs_f64() * 1e6);
                        diff_bytes = msg.to_string().len() as u64;
                        break;
                    }
                }
            }

            let _ = stop_all_sessions(&mut ws, &sessions).await;
            let _ = ws.close(None).await;
            println!(
                "{}",
                json!({
                    "events_received": events_rx,
                    "bytes_received": bytes_rx,
                    "max_seq": max_seq,
                    "history_pages": pages,
                    "diff_bytes": diff_bytes,
                    "relay_latency": summarize_us(&mut relay_us),
                    "history_latency": summarize_us(&mut history_us),
                    "diff_latency": summarize_us(&mut diff_us),
                })
            );
        }
        "churn" => {
            let cycles: usize = get_arg(&args, "--cycles")
                .and_then(|s| s.parse().ok())
                .unwrap_or(60);
            let session_count: usize = get_arg(&args, "--sessions")
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            let workdir = get_arg(&args, "--workdir").context("--workdir required")?;

            let (mut setup_ws, token) = pair_and_connect(&ws_url, &sock_path).await?;
            let (sessions, _) = start_sessions(
                &mut setup_ws,
                session_count,
                0,
                &workdir,
                "BENCH:burst:20:0:0",
                "",
            )
            .await?;
            for (uid, _) in &sessions {
                ws_send(
                    &mut setup_ws,
                    &json!({"type": "subscribe", "session_id": uid, "after_seq": 0}),
                )
                .await?;
            }
            let mut done = HashSet::new();
            let dl = Instant::now() + Duration::from_secs(30);
            while done.len() < sessions.len() && Instant::now() < dl {
                if let Ok(msg) = ws_recv_json(&mut setup_ws, Duration::from_secs(2)).await {
                    if msg["type"] == "event" && msg["event"]["kind"] == "turn_complete" {
                        if let Some(u) = msg["event"]["session_uid"].as_str() {
                            done.insert(u.to_string());
                        }
                    }
                }
            }
            let _ = setup_ws.close(None).await;

            let mut cycle_us = Vec::new();
            for _ in 0..cycles {
                let t0 = Instant::now();
                let mut ws = connect_with_token(&ws_url, &token).await?;
                ws_send(&mut ws, &json!({"type": "sessions"})).await?;
                loop {
                    let msg = ws_recv_json(&mut ws, Duration::from_secs(10)).await?;
                    if msg["type"] == "sessions" {
                        break;
                    }
                }
                // Subscribe to first session and drain replay up to turn_complete
                ws_send(
                    &mut ws,
                    &json!({"type": "subscribe", "session_id": sessions[0].0, "after_seq": 0}),
                )
                .await?;
                loop {
                    let msg = ws_recv_json(&mut ws, Duration::from_secs(10)).await?;
                    if msg["type"] == "event" && msg["event"]["kind"] == "turn_complete" {
                        break;
                    }
                }
                cycle_us.push(t0.elapsed().as_secs_f64() * 1e6);
                let _ = ws.close(None).await;
            }

            let mut cleanup_ws = connect_with_token(&ws_url, &token).await?;
            let _ = stop_all_sessions(&mut cleanup_ws, &sessions).await;
            let _ = cleanup_ws.close(None).await;
            println!(
                "{}",
                json!({
                    "cycles_completed": cycle_us.len(),
                    "cycle_latency": summarize_us(&mut cycle_us),
                })
            );
        }
        "soak" => {
            let claude: usize = get_arg(&args, "--claude")
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            let codex: usize = get_arg(&args, "--codex")
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            let duration_secs: u64 = get_arg(&args, "--duration-secs")
                .and_then(|s| s.parse().ok())
                .unwrap_or(600);
            let interval_ms: u64 = get_arg(&args, "--interval-ms")
                .and_then(|s| s.parse().ok())
                .unwrap_or(2000);
            let workdir = get_arg(&args, "--workdir").context("--workdir required")?;

            let (mut ws, token) = pair_and_connect(&ws_url, &sock_path).await?;
            let prompt = format!("BENCH:soak:{interval_ms}:1");
            let (sessions, _) =
                start_sessions(&mut ws, claude, codex, &workdir, &prompt, &prompt).await?;
            let agent_by_uid: HashMap<String, String> = sessions.iter().cloned().collect();
            let mut watermarks: HashMap<String, u64> = HashMap::new();
            for (uid, _) in &sessions {
                ws_send(
                    &mut ws,
                    &json!({"type": "subscribe", "session_id": uid, "after_seq": 0}),
                )
                .await?;
            }

            let end = Instant::now() + Duration::from_secs(duration_secs);
            let mut last_reconnect = Instant::now();
            let mut last_poll = Instant::now();
            let mut last_churn = Instant::now();
            let mut events_rx = 0u64;
            let mut approvals_answered = 0u64;
            let mut reconnects = 0u64;
            let mut diffs = 0u64;
            let mut churn_sessions = 0u64;
            let mut relay_us = Vec::new();

            while Instant::now() < end {
                // Every 60s, drop and reconnect WS client to exercise reconnect & watermark replay
                if last_reconnect.elapsed() >= Duration::from_secs(60) {
                    last_reconnect = Instant::now();
                    let _ = ws.close(None).await;
                    if let Ok(new_ws) = connect_with_token(&ws_url, &token).await {
                        ws = new_ws;
                        reconnects += 1;
                        let _ = ws_send(&mut ws, &json!({"type": "sessions"})).await;
                        for (uid, _) in &sessions {
                            let wm = watermarks.get(uid).copied().unwrap_or(0);
                            let _ = ws_send(
                                &mut ws,
                                &json!({"type": "subscribe", "session_id": uid, "after_seq": wm}),
                            )
                            .await;
                        }
                    }
                }
                // Every 20s, start and stop a short-lived session to catch per-session map leaks
                if last_churn.elapsed() >= Duration::from_secs(20) {
                    last_churn = Instant::now();
                    if let Ok((eph, _)) =
                        start_sessions(&mut ws, 1, 0, &workdir, "BENCH:burst:10:0:0", "").await
                    {
                        churn_sessions += eph.len() as u64;
                        let _ = stop_all_sessions(&mut ws, &eph).await;
                    }
                }
                // Every 15s, exercise sessions, get_diff, get_machine_stats, and IPC list_sessions
                if last_poll.elapsed() >= Duration::from_secs(15) {
                    last_poll = Instant::now();
                    let _ = ws_send(&mut ws, &json!({"type": "sessions"})).await;
                    let _ = ws_send(&mut ws, &json!({"type": "get_machine_stats"})).await;
                    if let Some((uid, _)) = sessions.first() {
                        let _ = ws_send(&mut ws, &json!({"type": "get_diff", "session_id": uid})).await;
                        diffs += 1;
                    }
                    let _ = ipc_call(&sock_path, &json!({"type": "list_sessions"})).await;
                }

                if let Ok(msg) = ws_recv_json(&mut ws, Duration::from_millis(500)).await {
                    if msg["type"] == "event" {
                        events_rx += 1;
                        let ev = &msg["event"];
                        let uid = ev["session_uid"].as_str().unwrap_or_default();
                        if let Some(seq) = ev["seq"].as_u64() {
                            let entry = watermarks.entry(uid.to_string()).or_insert(0);
                            *entry = (*entry).max(seq);
                        }
                        if relay_us.len() < 100_000 {
                            if let Some(sent_ns) = extract_bench_sent_ns(ev) {
                                let now = now_ns();
                                if now >= sent_ns {
                                    relay_us.push((now - sent_ns) as f64 / 1000.0);
                                }
                            }
                        }
                        if ev["kind"] == "approval_request" {
                            let card = if ev["payload"]["card"].is_object() {
                                &ev["payload"]["card"]
                            } else {
                                &ev["payload"]
                            };
                            let req_id = card["request_id"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string();
                            let hash = card["payload_hash"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string();
                            if !req_id.is_empty() && !hash.is_empty() {
                                let is_codex = agent_by_uid.get(uid).is_some_and(|a| a == "codex");
                                let decision = if is_codex {
                                    let opt_id = card["options"]
                                        .as_array()
                                        .and_then(|arr| arr.first())
                                        .and_then(|o| o["id"].as_str())
                                        .unwrap_or("accept");
                                    json!({"type": "option_id", "option_id": opt_id})
                                } else {
                                    json!({"type": "allow"})
                                };
                                let _ = ws_send(
                                    &mut ws,
                                    &json!({
                                        "type": "answer",
                                        "request_id": req_id,
                                        "payload_hash": hash,
                                        "decision": decision,
                                        "session_id": uid
                                    }),
                                )
                                .await;
                                approvals_answered += 1;
                            }
                        }
                    }
                }
            }
            let _ = stop_all_sessions(&mut ws, &sessions).await;
            let _ = ws.close(None).await;
            println!(
                "{}",
                json!({
                    "duration_secs": duration_secs,
                    "events_received": events_rx,
                    "approvals_answered": approvals_answered,
                    "reconnects": reconnects,
                    "diffs_requested": diffs,
                    "churn_sessions": churn_sessions,
                    "relay_latency": summarize_us(&mut relay_us),
                })
            );
        }
        _ => bail!("unknown command: {cmd}"),
    }
    Ok(())
}
