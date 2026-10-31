//! **`codeconnect mcp`: the tool a hosted agent messages other agents with.**
//!
//! A stdio MCP server, started by Claude Code (`--mcp-config`) or by Codex's
//! app-server (`-c mcp_servers.codeconnect…`) for one session, and named after it:
//! `codeconnect mcp --session-uid <uid> --socket <ccd.sock>`. The socket is an
//! argument rather than read from `CODECONNECT_HOME`, because Codex starts its MCP
//! servers with an environment of its own.
//!
//! It offers two tools, `list_agents` and `send_message`, and each call is one
//! round trip to the daemon. Everything the agent is told comes from the daemon's
//! answer; this process decides nothing.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};
use protocol::ipc::{AgentMessageOutcome, ClientFrame, DaemonFrame};
use serde_json::{json, Value};

/// Longer than the daemon's own work on a message, so its answer is what the agent
/// is told.
const DAEMON_TIMEOUT: Duration = Duration::from_secs(30);

pub fn run(args: &[String]) -> Result<()> {
    let (session_uid, socket) = parse(args)?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let ask = |frame: &ClientFrame| crate::daemon::request_at(&socket, frame, DAEMON_TIMEOUT);
        if let Some(reply) = answer(&line, &session_uid, ask) {
            serde_json::to_writer(&mut stdout, &reply)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

fn parse(args: &[String]) -> Result<(String, PathBuf)> {
    let (mut uid, mut socket) = (None, None);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it.next();
        match (flag.as_str(), value) {
            ("--session-uid", Some(value)) => uid = Some(value.clone()),
            ("--socket", Some(value)) => socket = Some(PathBuf::from(value)),
            _ => bail!("usage: codeconnect mcp --session-uid <uid> --socket <path>"),
        }
    }
    match (uid, socket) {
        (Some(uid), Some(socket)) => Ok((uid, socket)),
        _ => bail!("usage: codeconnect mcp --session-uid <uid> --socket <path>"),
    }
}

/// The reply to one JSON-RPC line, or `None` for a notification.
fn answer(
    line: &str,
    session_uid: &str,
    ask: impl Fn(&ClientFrame) -> Result<DaemonFrame>,
) -> Option<Value> {
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        return Some(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32700, "message": "not JSON"},
        }));
    };
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").cloned()
                .unwrap_or_else(|| json!("2025-06-18")),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "codeconnect", "version": env!("CARGO_PKG_VERSION")},
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => call(&params, session_uid, ask),
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("method not found: {method}")},
            }))
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn tools() -> Value {
    json!([
        {
            "name": "list_agents",
            "description": "Say your own address, and list the other Claude Code and Codex \
                sessions you can message, each as its address (cc-N@computer), which agent it \
                is, and its project. Give your own address when you ask an agent to answer you \
                through another.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "send_message",
            "description": "Send a message to another Claude Code or Codex session. `to` is its \
                address, cc-N@computer, as list_agents shows it. The message reaches that agent \
                as a prompt of its own when it is free, never in the middle of its work. The \
                answer says whether it was delivered, held until the agent is free, or refused, \
                and why.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "The address, cc-N@computer."},
                    "text": {"type": "string", "description": "The message."},
                },
                "required": ["to", "text"],
            },
        },
    ])
}

fn call(
    params: &Value,
    session_uid: &str,
    ask: impl Fn(&ClientFrame) -> Result<DaemonFrame>,
) -> Value {
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    let text = |s: String, error: bool| json!({"content": [{"type": "text", "text": s}], "isError": error});
    match params.get("name").and_then(Value::as_str) {
        Some("list_agents") => {
            match ask(&ClientFrame::ListAgents {
                from_uid: session_uid.to_string(),
            }) {
                Ok(DaemonFrame::Agents { you, agents }) => {
                    let mut lines = vec![format!("You are {you}.")];
                    if agents.is_empty() {
                        lines.push("No other agents are running.".into());
                    }
                    lines.extend(
                        agents.iter().map(|a| {
                            format!("{} · {} · project {}", a.address, a.agent, a.project)
                        }),
                    );
                    text(lines.join("\n"), false)
                }
                Ok(other) => text(unexpected(&other), true),
                Err(err) => text(format!("{err:#}"), true),
            }
        }
        Some("send_message") => {
            let (Some(to), Some(body)) = (
                arguments.get("to").and_then(Value::as_str),
                arguments.get("text").and_then(Value::as_str),
            ) else {
                return text("send_message needs `to` and `text`.".into(), true);
            };
            match ask(&ClientFrame::SendAgentMessage {
                from_uid: session_uid.to_string(),
                to: to.to_string(),
                text: body.to_string(),
            }) {
                Ok(DaemonFrame::AgentMessageResult { outcome }) => {
                    text(outcome_sentence(&outcome), false)
                }
                Ok(other) => text(unexpected(&other), true),
                Err(err) => text(format!("{err:#}"), true),
            }
        }
        other => text(format!("no such tool: {}", other.unwrap_or("")), true),
    }
}

fn outcome_sentence(outcome: &AgentMessageOutcome) -> String {
    match outcome {
        AgentMessageOutcome::Delivered => "delivered".into(),
        AgentMessageOutcome::Held { reason } => format!("held: {reason}"),
        AgentMessageOutcome::Duplicate => "not sent again: the same message went to that agent \
            in the last ten minutes"
            .into(),
        AgentMessageOutcome::Refused { reason } => format!("refused: {reason}"),
        AgentMessageOutcome::Indeterminate { reason } => format!("indeterminate: {reason}"),
    }
}

fn unexpected(frame: &DaemonFrame) -> String {
    format!(
        "the CodeConnect daemon gave an answer this tool does not know ({}); \
         it may be older than this codeconnect",
        serde_json::to_value(frame)
            .ok()
            .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default()
    )
}

/// The arguments that start this server for one session.
pub fn server_args(session_uid: &str, socket: &Path) -> Vec<String> {
    vec![
        "mcp".into(),
        "--session-uid".into(),
        session_uid.into(),
        "--socket".into(),
        socket.to_string_lossy().into_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ipc::AgentEntry;
    use std::cell::RefCell;

    fn reply(line: &str, daemon: impl Fn(&ClientFrame) -> Result<DaemonFrame>) -> Value {
        answer(line, "01K1B3XQ8ZC0DE5FGH7JKMNPQR", daemon).expect("a request is answered")
    }

    fn no_daemon(_: &ClientFrame) -> Result<DaemonFrame> {
        bail!("cannot reach the CodeConnect daemon")
    }

    #[test]
    fn the_handshake_lists_two_tools_and_echoes_the_protocol_version() {
        let init = reply(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            no_daemon,
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "codeconnect");
        assert!(answer(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "u",
            no_daemon
        )
        .is_none());
        let list = reply(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            no_daemon,
        );
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["list_agents", "send_message"]);
        let unknown = reply(
            r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#,
            no_daemon,
        );
        assert_eq!(unknown["error"]["code"], -32601);
        assert_eq!(reply("{", no_daemon)["error"]["code"], -32700);
    }

    #[test]
    fn a_send_is_one_daemon_round_trip_and_its_answer_is_what_the_agent_reads() {
        let asked = RefCell::new(Vec::new());
        let daemon = |frame: &ClientFrame| {
            asked
                .borrow_mut()
                .push(serde_json::to_value(frame).unwrap());
            Ok(DaemonFrame::AgentMessageResult {
                outcome: AgentMessageOutcome::Held {
                    reason: "the agent is working".into(),
                },
            })
        };
        let sent = reply(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"send_message","arguments":{"to":"cc-2@studio","text":"hi"}}}"#,
            daemon,
        );
        assert_eq!(
            sent["result"]["content"][0]["text"],
            "held: the agent is working"
        );
        assert_eq!(sent["result"]["isError"], false);
        assert_eq!(
            asked.borrow()[..],
            [
                json!({"type": "send_agent_message", "from_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
                    "to": "cc-2@studio", "text": "hi"})
            ]
        );
        let missing = reply(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"send_message","arguments":{"to":"cc-2@studio"}}}"#,
            daemon,
        );
        assert_eq!(missing["result"]["isError"], true);
    }

    #[test]
    fn a_listing_names_each_agent_and_a_missing_daemon_is_an_error() {
        let daemon = |_: &ClientFrame| {
            Ok(DaemonFrame::Agents {
                you: "cc-1@studio".into(),
                agents: vec![AgentEntry {
                    address: "cc-3@studio".into(),
                    agent: "Codex".into(),
                    project: "api".into(),
                }],
            })
        };
        let call = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"list_agents","arguments":{}}}"#;
        assert_eq!(
            reply(call, daemon)["result"]["content"][0]["text"],
            "You are cc-1@studio.\ncc-3@studio · Codex · project api"
        );
        let down = reply(call, no_daemon);
        assert_eq!(down["result"]["isError"], true);
        assert!(down["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cannot reach"));
    }
}
