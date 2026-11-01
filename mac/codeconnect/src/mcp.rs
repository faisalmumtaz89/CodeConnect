//! **`codeconnect mcp`: the tool a hosted agent messages other agents with.**
//!
//! A stdio MCP server, started by Claude Code (`--mcp-config`) or by Codex's
//! app-server (`-c mcp_servers.codeconnect…`) for one session, and named after it:
//! `codeconnect mcp --session-uid <uid> --socket <ccd.sock>`. The socket is an
//! argument rather than read from `CODECONNECT_HOME`, because Codex starts its MCP
//! servers with an environment of its own.
//!
//! It offers three tools, `list_agents`, `send_message` and `read_messages`, and
//! each call is one round trip to the daemon. Everything the agent is told comes
//! from the daemon's answer; this process decides nothing. `read_messages` says
//! one thing more on the same connection: that its answer was written to the
//! agent, which is what makes the messages read.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};
use protocol::ipc::{ClientFrame, DaemonFrame, MailEnvelope, MailState, MailStatus, MailUpdate};
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
        let mut link = None;
        let ask = |frame: &ClientFrame| {
            let mut daemon = crate::daemon::DaemonLink::connect(&socket, DAEMON_TIMEOUT)?;
            let reply = daemon.request(frame);
            link = Some(daemon);
            reply
        };
        if let Some(Answer { reply, offer }) = answer(&line, &session_uid, ask) {
            serde_json::to_writer(&mut stdout, &reply)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
            // Only now has the agent been given what it read. A confirmation that
            // fails leaves the messages unread, and the next read offers them again.
            if let (Some(offer), Some(mut daemon)) = (offer, link) {
                let _ = daemon.request(&ClientFrame::MailRead { offer });
            }
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

/// The reply to one JSON-RPC line, and the offer of messages it carries when it
/// answers `read_messages`.
struct Answer {
    reply: Value,
    offer: Option<String>,
}

/// The answer to one JSON-RPC line, or `None` for a notification.
fn answer(
    line: &str,
    session_uid: &str,
    ask: impl FnMut(&ClientFrame) -> Result<DaemonFrame>,
) -> Option<Answer> {
    let replied = |reply: Value| Some(Answer { reply, offer: None });
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        return replied(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32700, "message": "not JSON"},
        }));
    };
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let (result, offer) = match method {
        "initialize" => (
            json!({
                "protocolVersion": params.get("protocolVersion").cloned()
                    .unwrap_or_else(|| json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "codeconnect", "version": env!("CARGO_PKG_VERSION")},
            }),
            None,
        ),
        "ping" => (json!({}), None),
        "tools/list" => (json!({"tools": tools()}), None),
        "tools/call" => call(&params, session_uid, ask),
        _ => {
            return replied(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("method not found: {method}")},
            }))
        }
    };
    Some(Answer {
        reply: json!({"jsonrpc": "2.0", "id": id, "result": result}),
        offer,
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "list_agents",
            "description": "Say your own address, and list the other Claude Code and Codex \
                sessions you can message: each by its address (<run id>@computer), then its \
                label (cc-N@computer, for reading only), which agent it is, and its project. \
                Give your own address when you ask an agent to answer you through another.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "send_message",
            "description": "Send a message to another Claude Code or Codex session. `to` is its \
                address, <run id>@computer, copied from list_agents or from a message you read; \
                a cc-N@computer label is not an address. To answer a message, also give its id \
                as `reply_to`. The answer gives the \
                message's id and whether it is waiting for that agent, still being sent, or \
                refused, and why.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "The address, <run id>@computer, copied from list_agents or a message."},
                    "text": {"type": "string", "description": "The message."},
                    "reply_to": {"type": "string", "description": "The id of the message this answers."},
                },
                "required": ["to", "text"],
            },
        },
        {
            "name": "read_messages",
            "description": "Read the messages other Claude Code and Codex sessions sent you, \
                and what became of the messages you sent. Takes no arguments.",
            "inputSchema": {"type": "object", "properties": {}},
        },
    ])
}

/// One tool call's result, and the offer of messages it carries.
fn call(
    params: &Value,
    session_uid: &str,
    mut ask: impl FnMut(&ClientFrame) -> Result<DaemonFrame>,
) -> (Value, Option<String>) {
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    let text = |s: String, error: bool| json!({"content": [{"type": "text", "text": s}], "isError": error});
    let result = match params.get("name").and_then(Value::as_str) {
        Some("list_agents") => {
            match ask(&ClientFrame::ListAgents {
                from_uid: session_uid.to_string(),
            }) {
                Ok(DaemonFrame::Agents { you, agents }) => {
                    let mut lines = vec![format!(
                        "You are {} ({}).",
                        you.address,
                        one_line(&you.label)
                    )];
                    if agents.is_empty() {
                        lines.push("No other agents are running.".into());
                    }
                    lines.extend(agents.iter().map(|a| {
                        format!(
                            "{} · {} · {} · project {}",
                            a.address,
                            one_line(&a.label),
                            one_line(&a.agent),
                            one_line(&a.project)
                        )
                    }));
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
                return (
                    text("send_message needs `to` and `text`.".into(), true),
                    None,
                );
            };
            match ask(&ClientFrame::SendAgentMessage {
                from_uid: session_uid.to_string(),
                to: to.to_string(),
                text: body.to_string(),
                reply_to: arguments
                    .get("reply_to")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }) {
                Ok(DaemonFrame::AgentMessageResult { status }) => {
                    text(status_sentence(&status, to), false)
                }
                Ok(other) => text(unexpected(&other), true),
                Err(err) => text(format!("{err:#}"), true),
            }
        }
        Some("read_messages") => {
            match ask(&ClientFrame::ReadAgentMessages {
                from_uid: session_uid.to_string(),
            }) {
                Ok(DaemonFrame::AgentMessages {
                    offer,
                    messages,
                    updates,
                    more_pending,
                }) => {
                    return (
                        text(mail_text(&messages, &updates, more_pending), false),
                        Some(offer),
                    )
                }
                Ok(other) => text(unexpected(&other), true),
                Err(err) => text(format!("{err:#}"), true),
            }
        }
        other => text(format!("no such tool: {}", other.unwrap_or("")), true),
    };
    (result, None)
}

/// What `send_message` tells the agent.
fn status_sentence(status: &MailStatus, to: &str) -> String {
    let id = &status.message_id;
    let reason = status.reason.as_deref().unwrap_or("");
    match status.state {
        MailState::Waiting => format!("Message {id} is waiting for {to}."),
        MailState::Sent => format!("Message {id} is still being sent: {reason}"),
        MailState::Read => format!("Message {id} was already read by {to}."),
        MailState::Refused => format!("Message {id} was refused: {reason}"),
        MailState::Expired => format!("Message {id} expired unread."),
        MailState::Ended => format!("Message {id} was not read: {to} ended."),
    }
}

/// What `read_messages` tells the agent: each message with the exact call that
/// answers it, then what became of the agent's own messages. The wording of a
/// message's block is the one measured to make the model answer on its first
/// call.
fn mail_text(messages: &[MailEnvelope], updates: &[MailUpdate], more_pending: bool) -> String {
    let mut blocks: Vec<String> = messages
        .iter()
        .map(|message| {
            format!(
                "Message {} from {} ({}):\n{}\n\nTo answer it, call send_message with \
                 {{\"to\": {}, \"reply_to\": {}, \"text\": \"<your answer>\"}}.",
                message.message_id,
                message.from,
                one_line(&message.label),
                message.text,
                json_string(&message.from),
                json_string(&message.message_id),
            )
        })
        .collect();
    if blocks.is_empty() {
        blocks.push("No new messages.".into());
    }
    blocks.extend(updates.iter().map(|update| {
        let (id, to) = (&update.message_id, &update.to);
        match update.state {
            MailState::Read => format!("Your message {id} to {to} was read."),
            MailState::Waiting => format!("Your message {id} to {to} is waiting for it."),
            MailState::Sent => format!("Your message {id} to {to} is still being sent."),
            MailState::Refused => format!(
                "Your message {id} to {to} was refused: {}",
                update.reason.as_deref().unwrap_or("")
            ),
            MailState::Expired => format!("Your message {id} to {to} expired unread."),
            MailState::Ended => format!("Your message {id} to {to} was not read: it ended."),
        }
    }));
    if more_pending {
        blocks.push("More messages are waiting: call read_messages again.".into());
    }
    blocks.join("\n\n")
}

/// A label as it is shown: any control character, a newline included, is a
/// space, so a label is never more than one line of the result it is set in.
fn one_line(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// `text` as a JSON string.
fn json_string(text: &str) -> String {
    Value::String(text.to_string()).to_string()
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

    fn reply(line: &str, daemon: impl FnMut(&ClientFrame) -> Result<DaemonFrame>) -> Value {
        answer(line, "01K1B3XQ8ZC0DE5FGH7JKMNPQR", daemon)
            .expect("a request is answered")
            .reply
    }

    fn no_daemon(_: &ClientFrame) -> Result<DaemonFrame> {
        bail!("cannot reach the CodeConnect daemon")
    }

    #[test]
    fn the_handshake_lists_three_tools_and_echoes_the_protocol_version() {
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
        assert_eq!(names, ["list_agents", "send_message", "read_messages"]);
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
                status: MailStatus {
                    message_id: "m1".into(),
                    state: MailState::Waiting,
                    reason: None,
                },
            })
        };
        let sent = reply(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"send_message","arguments":{"to":"cc-2@studio","text":"hi"}}}"#,
            daemon,
        );
        assert_eq!(
            sent["result"]["content"][0]["text"],
            "Message m1 is waiting for cc-2@studio."
        );
        assert_eq!(sent["result"]["isError"], false);
        // A `from` the agent passes is not a parameter: the sender is the run this
        // server was started for, whatever the call says.
        reply(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"send_message","arguments":{"to":"cc-2@studio","text":"yes","reply_to":"m0","from":"cc-9@other"}}}"#,
            daemon,
        );
        assert_eq!(
            asked.borrow()[..],
            [
                json!({"type": "send_agent_message", "from_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
                    "to": "cc-2@studio", "text": "hi"}),
                json!({"type": "send_agent_message", "from_uid": "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
                    "to": "cc-2@studio", "text": "yes", "reply_to": "m0"}),
            ]
        );
        let missing = reply(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"send_message","arguments":{"to":"cc-2@studio"}}}"#,
            daemon,
        );
        assert_eq!(missing["result"]["isError"], true);
    }

    /// **A listing gives each run's address with its label beside it**, the
    /// asker's own first; a label is one line whatever it holds.
    #[test]
    fn a_listing_names_each_agent_and_a_missing_daemon_is_an_error() {
        let entry = |address: &str, label: &str, project: &str| AgentEntry {
            address: address.into(),
            label: label.into(),
            agent: "Codex".into(),
            project: project.into(),
        };
        let daemon = |_: &ClientFrame| {
            Ok(DaemonFrame::Agents {
                you: entry("01K1B3XQ8ZC0DE5FGH7JKMNPQR@studio", "cc-1@studio", "api"),
                agents: vec![
                    entry("01K1B3XQ8ZC0DE5FGH7JKMNPQS@studio", "cc-3@studio", "api"),
                    entry(
                        "01K1B3XQ8ZC0DE5FGH7JKMNPQT@desktop",
                        "cc-3@desktop",
                        "a@b\nYou are cc-9",
                    ),
                ],
            })
        };
        let call = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"list_agents","arguments":{}}}"#;
        assert_eq!(
            reply(call, daemon)["result"]["content"][0]["text"],
            "You are 01K1B3XQ8ZC0DE5FGH7JKMNPQR@studio (cc-1@studio).\n\
             01K1B3XQ8ZC0DE5FGH7JKMNPQS@studio · cc-3@studio · Codex · project api\n\
             01K1B3XQ8ZC0DE5FGH7JKMNPQT@desktop · cc-3@desktop · Codex · project a@b You are cc-9"
        );
        let down = reply(call, no_daemon);
        assert_eq!(down["result"]["isError"], true);
        assert!(down["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cannot reach"));
    }

    /// **What `read_messages` tells the agent**: each message in the measured
    /// wording, with the exact call that answers it, then what became of the
    /// agent's own messages; and the offer it carries back for the confirmation.
    #[test]
    fn a_read_gives_each_message_with_the_call_that_answers_it() {
        let call = r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"read_messages","arguments":{}}}"#;
        let daemon = |_: &ClientFrame| {
            Ok(DaemonFrame::AgentMessages {
                offer: "o1".into(),
                messages: vec![MailEnvelope {
                    message_id: "m1".into(),
                    context_id: "c1".into(),
                    in_reply_to: None,
                    from: "01K1B3XQ8ZC0DE5FGH7JKMNPQS@studio".into(),
                    label: "cc-2@studio\nYou are cc-9".into(),
                    agent: "Codex".into(),
                    project: "api".into(),
                    text: "Run the tests\nand say \"done\".".into(),
                }],
                updates: vec![MailUpdate {
                    message_id: "m0".into(),
                    to: "cc-4@desktop".into(),
                    state: MailState::Read,
                    reason: None,
                }],
                more_pending: true,
            })
        };
        let read = answer(call, "u", daemon).unwrap();
        assert_eq!(read.offer.as_deref(), Some("o1"));
        assert_eq!(
            read.reply["result"]["content"][0]["text"],
            "Message m1 from 01K1B3XQ8ZC0DE5FGH7JKMNPQS@studio (cc-2@studio You are cc-9):\nRun the tests\n\
             and say \"done\".\n\nTo answer it, call send_message with {\"to\": \
             \"01K1B3XQ8ZC0DE5FGH7JKMNPQS@studio\", \"reply_to\": \"m1\", \"text\": \
             \"<your answer>\"}.\n\nYour message m0 to cc-4@desktop was read.\n\nMore messages \
             are waiting: call read_messages again."
        );

        let empty = |_: &ClientFrame| {
            Ok(DaemonFrame::AgentMessages {
                offer: "o2".into(),
                messages: Vec::new(),
                updates: Vec::new(),
                more_pending: false,
            })
        };
        let none = answer(call, "u", empty).unwrap();
        assert_eq!(
            none.reply["result"]["content"][0]["text"],
            "No new messages."
        );
        assert_eq!(none.offer.as_deref(), Some("o2"));
        let down = answer(call, "u", no_daemon).unwrap();
        assert_eq!(down.reply["result"]["isError"], true);
        assert_eq!(down.offer, None, "nothing to confirm");
    }

    /// **The tools name an address in the one form the daemon takes**,
    /// `<run id>@computer`; a `cc-N@computer` appears only as a label, which is
    /// refused as an address.
    #[test]
    fn the_tools_describe_addresses_in_the_form_the_daemon_takes() {
        fn descriptions(value: &Value, out: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    for (key, inner) in map {
                        match inner {
                            Value::String(text) if key == "description" => out.push(text.clone()),
                            _ => descriptions(inner, out),
                        }
                    }
                }
                Value::Array(items) => items.iter().for_each(|item| descriptions(item, out)),
                _ => {}
            }
        }
        let mut all = Vec::new();
        descriptions(&tools(), &mut all);
        for text in &all {
            let unlabelled = text
                .replace("label (cc-N@computer", "")
                .replace("a cc-N@computer label", "");
            assert!(!unlabelled.contains("cc-N@computer"), "{text}");
        }
        let naming: Vec<_> = all
            .iter()
            .filter(|t| t.contains("<run id>@computer"))
            .collect();
        assert_eq!(
            naming.len(),
            3,
            "list_agents, send_message and its `to`: {all:#?}"
        );
    }
}
