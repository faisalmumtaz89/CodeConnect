//! The OpenCode plugin's link frames, and the pure pieces of reading them.
//!
//! The CodeConnect plugin runs inside the OpenCode TUI
//! (`mac/codeconnect/opencode-plugin/codeconnect-opencode.js`). After its hello
//! ([`protocol::ipc::OpencodeHello`]) is admitted, every line it writes is one
//! [`LinkFrame`], told apart by `t`:
//!
//!   * `ev` — one bus event, filtered and size-capped by the plugin; `stub` — the
//!     same over the frame cap, ids and digest only; `card_stub` — a permission or
//!     question card over the frame cap;
//!   * `head` — the session the keyboard is showing;
//!   * `sync_begin`, `sync_request`, `sync_page`, `sync_end`, `settled` — one
//!     snapshot read from the OpenCode server, which [`SyncAssembly`] turns into a
//!     [`Snapshot`].
//!
//! `fixtures/opencode/link-v1-frames.jsonl` is the contract: the plugin's tests
//! regenerate it and this module's tests decode every row of it.
//!
//! Decoding is strict for every `t` this build knows: a missing or mistyped
//! field fails the frame. A `t` it does not know is skipped and logged, so a
//! newer plugin's additions do not cost the link.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A wire field that is always `true`: the marker that tells a page item or a
/// request body which shape it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct True;

impl Serialize for True {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for True {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if bool::deserialize(deserializer)? {
            Ok(True)
        } else {
            Err(serde::de::Error::custom("expected true"))
        }
    }
}

/// One frame the plugin writes after the welcome.
///
/// `seq` counts the plugin's bus events and head moves per activation, strictly
/// ascending; the snapshot frames carry the `sync` they belong to instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum LinkFrame {
    Ev {
        seq: u64,
        #[serde(rename = "type")]
        event: String,
        properties: Map<String, Value>,
    },
    /// A bus event over the frame cap. `part_type` and `status` are kept from a
    /// replaced message part, so a turn can still be judged without it.
    Stub {
        seq: u64,
        #[serde(rename = "type")]
        event: String,
        ids: StubIds,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        part_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        size: u64,
        sha256: String,
    },
    /// A `permission.asked` or `question.asked` over the frame cap: the card's
    /// identity, and no request text.
    CardStub {
        seq: u64,
        #[serde(rename = "type")]
        event: String,
        properties: CardIds,
        size: u64,
        sha256: String,
    },
    Head {
        seq: u64,
        #[serde(flatten)]
        route: HeadRoute,
    },
    SyncBegin {
        sync: u64,
        /// `connect`, `requested`, or `trigger:<event type>`.
        reason: String,
        scope: SyncScope,
        /// The last `seq` written before the snapshot's reads began.
        as_of: u64,
        status: Map<String, Value>,
        status_ok: bool,
    },
    SyncRequest {
        sync: u64,
        kind: RequestKind,
        #[serde(flatten)]
        body: RequestBody,
        verdict: Verdict,
    },
    SyncPage {
        sync: u64,
        n: u64,
        items: Vec<PageItem>,
    },
    SyncEnd {
        sync: u64,
        /// The last `seq` written when the snapshot's reads ended.
        done_at: u64,
        /// Whether the session list was read. Absent from a requests-only
        /// snapshot, which reads no list.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        list_ok: Option<bool>,
        permissions_ok: bool,
        questions_ok: bool,
        requests_ok: bool,
        lower: BTreeMap<String, protocol::ipc::OpencodeBound>,
        pages: u64,
        items: u64,
        bytes: u64,
        activation: u64,
    },
    /// Every frame held during the snapshot up to `done_at` has been written.
    Settled { sync: u64 },
}

impl LinkFrame {
    /// The plugin's sequence number, for the frames that carry one.
    pub(crate) fn seq(&self) -> Option<u64> {
        match self {
            LinkFrame::Ev { seq, .. }
            | LinkFrame::Stub { seq, .. }
            | LinkFrame::CardStub { seq, .. }
            | LinkFrame::Head { seq, .. } => Some(*seq),
            _ => None,
        }
    }

    /// A permission or question card. Cards are keyed by request id, so the
    /// plugin sends them at once even while it holds other frames for a
    /// snapshot, and a card is never refused for its `seq`.
    fn is_card(&self) -> bool {
        match self {
            LinkFrame::Ev { event, .. } => is_card_event(event),
            LinkFrame::CardStub { .. } => true,
            _ => false,
        }
    }
}

pub(crate) fn is_card_event(event: &str) -> bool {
    event == "permission.asked" || event == "question.asked"
}

/// The identifiers a `stub` keeps, each present only when the event had it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StubIds {
    #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<String>,
    #[serde(rename = "messageID", default, skip_serializing_if = "Option::is_none")]
    pub(crate) message_id: Option<String>,
    #[serde(rename = "partID", default, skip_serializing_if = "Option::is_none")]
    pub(crate) part_id: Option<String>,
    #[serde(rename = "callID", default, skip_serializing_if = "Option::is_none")]
    pub(crate) call_id: Option<String>,
}

/// What a card over the frame cap keeps: whose request it is, and the tool call
/// it is about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CardIds {
    pub(crate) id: String,
    #[serde(rename = "sessionID")]
    pub(crate) session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) permission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tool: Option<Value>,
}

/// The keyboard's view. A session view always names its session; its folder is
/// `null` while the TUI has not loaded that session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "route", rename_all = "snake_case")]
pub(crate) enum HeadRoute {
    Home,
    Other,
    Session {
        session_id: String,
        directory: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SyncScope {
    /// The status, the pending requests, then history pages.
    Full,
    /// The status and the pending requests only.
    Requests,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequestKind {
    Permission,
    Question,
}

/// A pending request as the server listed it, or, over the frame cap, its
/// identity alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum RequestBody {
    Request { request: Map<String, Value> },
    Stub { stub: RequestStub },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RequestStub {
    pub(crate) properties: CardIds,
    pub(crate) size: u64,
    pub(crate) sha256: String,
}

/// The plugin's judgement of whether a pending request can still be answered,
/// made at snapshot time from the server, the TUI store and its own record of
/// the activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Verdict {
    pub(crate) live: bool,
    /// Why it cannot, when it cannot. See [`Verdict::is_well_formed`].
    pub(crate) dead: Option<String>,
    pub(crate) listed: Listed,
    pub(crate) in_store: bool,
}

/// Whether the server's list named the request: `"unknown"` when the list
/// could not be read and the request came from the TUI store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Listed {
    Answer(bool),
    Unknown(UnknownWord),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum UnknownWord {
    #[serde(rename = "unknown")]
    Unknown,
}

impl Verdict {
    /// A live request names no reason; a dead one names a reason the plugin
    /// knows how to give.
    pub(crate) fn is_well_formed(&self) -> bool {
        match self.dead.as_deref() {
            None => self.live,
            Some(reason) => !self.live && dead_reason_is_known(reason),
        }
    }
}

fn dead_reason_is_known(reason: &str) -> bool {
    const PLAIN: [&str; 6] = [
        "absent",
        "session-missing",
        "not-busy",
        "anchor",
        "asked-before-activation",
        "epoch-evicted",
    ];
    const EPOCH: [&str; 6] = ["abort", "stop", "idle", "retry", "deleted", "disposed"];
    if PLAIN.contains(&reason) {
        return true;
    }
    let Some(rest) = reason.strip_prefix("epoch:") else {
        return false;
    };
    let event = rest.strip_suffix(":ancestor").unwrap_or(rest);
    EPOCH.contains(&event)
}

/// One item of a history page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum PageItem {
    /// An item over the page cap, kept as its identity and digest.
    Stub {
        stub: True,
        kind: StubKind,
        ids: StubIds,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        part_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        size: u64,
        sha256: String,
    },
    /// A session whose messages could not be read.
    Error {
        error: True,
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    Session {
        session: Map<String, Value>,
    },
    /// One part of a message too big for a page on its own.
    SplitPart {
        #[serde(rename = "sessionID")]
        session_id: String,
        info: Map<String, Value>,
        part: Map<String, Value>,
        split: True,
    },
    /// A whole message, or a run of its parts when it is split.
    Message {
        #[serde(rename = "sessionID")]
        session_id: String,
        info: Map<String, Value>,
        parts: Vec<Map<String, Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        split: Option<True>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StubKind {
    Part,
    Message,
    Session,
}

/// Decode one line. `Ok(None)` is a frame of a kind this build does not know,
/// skipped; `Err` is a known frame with a missing or mistyped field, or a line
/// that is not a frame at all.
pub(crate) fn decode(line: &[u8]) -> Result<Option<LinkFrame>, String> {
    let value: Value = serde_json::from_slice(line).map_err(|err| err.to_string())?;
    let Some(tag) = value.get("t").and_then(Value::as_str).map(str::to_string) else {
        return Err("a link frame without a string `t`".into());
    };
    const KNOWN: [&str; 9] = [
        "ev",
        "stub",
        "card_stub",
        "head",
        "sync_begin",
        "sync_request",
        "sync_page",
        "sync_end",
        "settled",
    ];
    if !KNOWN.contains(&tag.as_str()) {
        crate::log_info!("opencode link: skipped a frame of unknown kind `{tag}`");
        return Ok(None);
    }
    let frame: LinkFrame =
        serde_json::from_value(value).map_err(|err| format!("`{tag}` frame: {err}"))?;
    if let LinkFrame::SyncRequest { verdict, .. } = &frame {
        if !verdict.is_well_formed() {
            return Err(format!(
                "`sync_request` with an unreadable verdict: {verdict:?}"
            ));
        }
    }
    Ok(Some(frame))
}

/// Drops a frame the link has already passed.
///
/// `seq` is per plugin activation, so the high-water mark belongs to one
/// `(pid, start, activation)` and starts over with another. Cards pass
/// regardless: they are idempotent by request id, and the plugin sends one
/// immediately even while a snapshot holds the frames around it.
#[derive(Debug, Default)]
pub(crate) struct SeqFilter {
    activation: Option<(i32, protocol::ipc::OpencodeStart, u64)>,
    last: Option<u64>,
}

impl SeqFilter {
    /// The link is now this activation's.
    pub(crate) fn admitted(&mut self, hello: &protocol::ipc::OpencodeHello) {
        let activation = (hello.pid, hello.start, hello.activation);
        if self.activation != Some(activation) {
            self.activation = Some(activation);
            self.last = None;
        }
    }

    /// Whether `frame` is new on this activation.
    pub(crate) fn pass(&mut self, frame: &LinkFrame) -> bool {
        let Some(seq) = frame.seq() else {
            return true;
        };
        if frame.is_card() {
            return true;
        }
        if self.last.is_some_and(|last| seq <= last) {
            return false;
        }
        self.last = Some(seq);
        true
    }
}

/// One snapshot, assembled from its frames.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub(crate) scope: SyncScope,
    /// The server's `session.status`: busy or retrying sessions; idle ones are
    /// usually absent.
    pub(crate) status: Map<String, Value>,
    pub(crate) status_ok: bool,
    /// Whether the session list was read. True for a requests-only snapshot,
    /// which does not need it.
    pub(crate) list_ok: bool,
    /// Whether both pending-request lists were read.
    pub(crate) requests_ok: bool,
    /// Sessions in the order the plugin sent them, each with its messages
    /// ordered by id.
    pub(crate) sessions: Vec<SnapSession>,
    pub(crate) requests: Vec<SnapRequest>,
    /// Why nothing may be read from this snapshot: its history is not whole.
    pub(crate) broken: Option<String>,
    /// Sessions whose history arrived incomplete: a session or message over
    /// the page cap, or a part of a message that was never sent. Nothing is
    /// read of the session tree they belong to; the rest of the snapshot is.
    pub(crate) damaged: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SnapSession {
    pub(crate) info: Map<String, Value>,
    pub(crate) messages: Vec<SnapMessage>,
}

#[derive(Debug, Clone)]
pub(crate) struct SnapMessage {
    pub(crate) info: Map<String, Value>,
    pub(crate) parts: Vec<SnapPart>,
    split: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum SnapPart {
    Whole(Map<String, Value>),
    /// A part over the page cap: its identity, its type and its tool status.
    Stub {
        ids: StubIds,
        part_type: Option<String>,
        status: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct SnapRequest {
    pub(crate) kind: RequestKind,
    pub(crate) body: RequestBody,
    pub(crate) verdict: Verdict,
}

impl SnapRequest {
    pub(crate) fn id(&self) -> Option<&str> {
        match &self.body {
            RequestBody::Request { request } => request.get("id").and_then(Value::as_str),
            RequestBody::Stub { stub } => Some(&stub.properties.id),
        }
    }

    pub(crate) fn session(&self) -> Option<&str> {
        match &self.body {
            RequestBody::Request { request } => request.get("sessionID").and_then(Value::as_str),
            RequestBody::Stub { stub } => Some(&stub.properties.session_id),
        }
    }
}

/// Builds one [`Snapshot`] from `sync_begin`, its pages and requests, and
/// `sync_end`.
///
/// Message items arrive newest first, and a message too big for a page arrives
/// as consecutive items holding its parts in order; they are merged back here
/// and every session's messages are ordered by id. A session or message over
/// the page cap marks only its own session `damaged`. A history that cannot be
/// read at all — a session the server could not page, a duplicate message, a
/// frame of another snapshot — leaves the whole snapshot `broken`.
#[derive(Debug)]
pub(crate) struct SyncAssembly {
    sync: u64,
    scope: SyncScope,
    status: Map<String, Value>,
    status_ok: bool,
    next_page: u64,
    sessions: Vec<SnapSession>,
    index: HashMap<String, usize>,
    requests: Vec<SnapRequest>,
    broken: Option<String>,
    damaged: BTreeSet<String>,
}

impl SyncAssembly {
    /// Starts on a `sync_begin`; `None` for any other frame.
    pub(crate) fn begin(frame: &LinkFrame) -> Option<SyncAssembly> {
        let LinkFrame::SyncBegin {
            sync,
            scope,
            status,
            status_ok,
            ..
        } = frame
        else {
            return None;
        };
        Some(SyncAssembly {
            sync: *sync,
            scope: *scope,
            status: status.clone(),
            status_ok: *status_ok,
            next_page: 0,
            sessions: Vec::new(),
            index: HashMap::new(),
            requests: Vec::new(),
            broken: None,
            damaged: BTreeSet::new(),
        })
    }

    fn fail(&mut self, why: String) {
        self.broken.get_or_insert(why);
    }

    /// Takes a `sync_request` or `sync_page` of this snapshot. Any other frame
    /// is not this assembly's to take.
    pub(crate) fn add(&mut self, frame: &LinkFrame) {
        match frame {
            LinkFrame::SyncRequest {
                sync,
                kind,
                body,
                verdict,
            } => {
                if *sync != self.sync {
                    return self.fail(format!("a request of snapshot {sync} inside {}", self.sync));
                }
                self.requests.push(SnapRequest {
                    kind: *kind,
                    body: body.clone(),
                    verdict: verdict.clone(),
                });
            }
            LinkFrame::SyncPage { sync, n, items } => {
                if *sync != self.sync || *n != self.next_page {
                    return self.fail(format!("page {n} of snapshot {sync} out of place"));
                }
                self.next_page += 1;
                for item in items {
                    self.item(item);
                }
            }
            _ => {}
        }
    }

    fn item(&mut self, item: &PageItem) {
        match item {
            PageItem::Session { session } => {
                let Some(id) = session.get("id").and_then(Value::as_str) else {
                    return self.fail("a session without an id".into());
                };
                if self.index.contains_key(id) {
                    return self.fail(format!("session {id} sent twice"));
                }
                self.index.insert(id.to_string(), self.sessions.len());
                self.sessions.push(SnapSession {
                    info: session.clone(),
                    messages: Vec::new(),
                });
            }
            PageItem::Error { session_id, .. } => {
                self.fail(format!("the messages of {session_id} could not be read"))
            }
            PageItem::Stub {
                kind: StubKind::Part,
                ids,
                part_type,
                status,
                ..
            } => {
                let stub = SnapPart::Stub {
                    ids: ids.clone(),
                    part_type: part_type.clone(),
                    status: status.clone(),
                };
                match (&ids.session_id, &ids.message_id) {
                    (Some(session), Some(message)) => self.part_of(session, message, stub),
                    _ => self.fail("a part stub without its session or message".into()),
                }
            }
            PageItem::Stub { kind, ids, .. } => match &ids.session_id {
                Some(session) => {
                    self.damaged.insert(session.clone());
                }
                None => self.fail(format!("a {kind:?} stub without its session")),
            },
            PageItem::SplitPart {
                session_id,
                info,
                part,
                ..
            } => self.message(session_id, info, vec![SnapPart::Whole(part.clone())], true),
            PageItem::Message {
                session_id,
                info,
                parts,
                split,
            } => {
                let parts = parts.iter().cloned().map(SnapPart::Whole).collect();
                self.message(session_id, info, parts, split.is_some())
            }
        }
    }

    fn message(
        &mut self,
        session: &str,
        info: &Map<String, Value>,
        parts: Vec<SnapPart>,
        split: bool,
    ) {
        let Some(id) = info.get("id").and_then(Value::as_str) else {
            return self.fail(format!("a message of {session} without an id"));
        };
        let Some(&at) = self.index.get(session) else {
            // A session over the page cap is still followed by its messages.
            if !self.damaged.contains(session) {
                self.fail(format!(
                    "message {id} of a session the snapshot did not send"
                ));
            }
            return;
        };
        let messages = &mut self.sessions[at].messages;
        match messages
            .iter_mut()
            .find(|m| m.info.get("id") == info.get("id"))
        {
            Some(known) if split && known.split => known.parts.extend(parts),
            Some(_) => self.fail(format!("message {id} sent twice")),
            None => messages.push(SnapMessage {
                info: info.clone(),
                parts,
                split,
            }),
        }
    }

    /// A part stub joins the split message it belongs to, which the plugin has
    /// always sent ahead of it.
    fn part_of(&mut self, session: &str, message: &str, stub: SnapPart) {
        let known = self.index.get(session).and_then(|&at| {
            self.sessions[at]
                .messages
                .iter_mut()
                .find(|m| m.info.get("id").and_then(Value::as_str) == Some(message))
        });
        match known {
            Some(known) => known.parts.push(stub),
            None => {
                self.damaged.insert(session.to_string());
            }
        }
    }

    /// Ends on a `sync_end`; `None` for any other frame.
    pub(crate) fn finish(mut self, frame: &LinkFrame) -> Option<Snapshot> {
        let LinkFrame::SyncEnd {
            sync,
            list_ok,
            requests_ok,
            permissions_ok,
            questions_ok,
            ..
        } = frame
        else {
            return None;
        };
        if *sync != self.sync {
            self.fail(format!("snapshot {} ended by {sync}", self.sync));
        }
        for session in &mut self.sessions {
            session
                .messages
                .sort_by(|a, b| message_id(&a.info).cmp(message_id(&b.info)));
        }
        Some(Snapshot {
            scope: self.scope,
            status: self.status,
            status_ok: self.status_ok,
            list_ok: list_ok.unwrap_or(self.scope == SyncScope::Requests),
            requests_ok: *requests_ok && *permissions_ok && *questions_ok,
            sessions: self.sessions,
            requests: self.requests,
            broken: self.broken,
            damaged: self.damaged,
        })
    }
}

fn message_id(info: &Map<String, Value>) -> &str {
    info.get("id").and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ipc::{ClientFrame, DaemonFrame};

    const LINK_V1: &str = include_str!("../../../fixtures/opencode/link-v1-frames.jsonl");

    struct Row {
        dir: String,
        case: Option<String>,
        frame: Value,
    }

    fn rows() -> Vec<Row> {
        LINK_V1
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let row: Value = serde_json::from_str(line).expect("a JSON row");
                Row {
                    dir: row["dir"].as_str().unwrap().to_string(),
                    case: row["case"].as_str().map(str::to_string),
                    frame: row["frame"].clone(),
                }
            })
            .collect()
    }

    fn link_frame(frame: &Value) -> LinkFrame {
        decode(frame.to_string().as_bytes())
            .unwrap_or_else(|err| panic!("{err}: {frame}"))
            .unwrap_or_else(|| panic!("a known frame: {frame}"))
    }

    fn case(starts: &str) -> Value {
        rows()
            .into_iter()
            .find(|row| row.case.as_deref().is_some_and(|c| c.starts_with(starts)))
            .unwrap_or_else(|| panic!("no case row `{starts}`"))
            .frame
    }

    #[test]
    fn every_plugin_frame_in_the_contract_decodes_and_encodes_back_unchanged() {
        let mut link = 0;
        for row in rows().into_iter().filter(|row| row.dir == "plugin") {
            if row.frame.get("t").is_none() {
                let _: ClientFrame = serde_json::from_value(row.frame.clone()).unwrap();
                continue;
            }
            link += 1;
            let frame = link_frame(&row.frame);
            assert_eq!(
                serde_json::to_value(&frame).unwrap(),
                row.frame,
                "the frame must survive a decode and an encode"
            );
        }
        assert_eq!(link, 32, "every link frame the contract holds");
    }

    #[test]
    fn a_frame_of_a_kind_this_build_does_not_know_is_skipped_and_a_malformed_one_is_not() {
        assert_eq!(decode(br#"{"t":"later","seq":9,"anything":[1]}"#), Ok(None));
        // A known kind is held to its shape.
        for line in [
            r#"{"t":"ev","seq":1,"type":"session.idle"}"#,
            r#"{"t":"ev","seq":"1","type":"session.idle","properties":{}}"#,
            r#"{"t":"head","seq":2,"route":"session"}"#,
            r#"{"t":"settled"}"#,
            r#"{"seq":1}"#,
            r#"not json"#,
        ] {
            assert!(decode(line.as_bytes()).is_err(), "{line}");
        }
        let unreadable = r#"{"t":"sync_request","sync":1,"kind":"question","request":{"id":"q"},
            "verdict":{"live":false,"dead":"lost-in-thought","listed":true,"in_store":false}}"#;
        assert!(decode(unreadable.as_bytes()).is_err());
    }

    #[test]
    fn an_observe_only_hello_says_which_members_are_missing() {
        let frame = case("an OpenCode whose plugin API lacks a member");
        let ClientFrame::OpencodeHello(hello) = serde_json::from_value(frame).unwrap() else {
            panic!("not a hello");
        };
        assert!(!hello.api.ok && !hello.api.missing.is_empty());
        assert_eq!(hello.api.version, None);
    }

    #[test]
    fn a_root_wanted_from_its_start_is_acked_from_null() {
        let frame = case("a root the daemon wants from its start");
        let DaemonFrame::OpencodeWelcome { acked, .. } = serde_json::from_value(frame).unwrap()
        else {
            panic!("not a welcome");
        };
        let bound = acked.values().next().expect("one acked root");
        assert_eq!((bound.from.as_deref(), bound.inclusive), (None, false));
    }

    #[test]
    fn a_request_card_over_the_cap_in_a_snapshot_carries_ids_size_and_digest_only() {
        let frame = case("a request card over the frame cap inside a snapshot");
        let LinkFrame::SyncRequest {
            body: RequestBody::Stub { stub },
            kind: RequestKind::Permission,
            ..
        } = link_frame(&frame)
        else {
            panic!("not a request stub");
        };
        assert!(stub.properties.id.starts_with("per_"));
        assert_eq!(stub.properties.permission.as_deref(), Some("bash"));
        assert!(stub.size > 1_048_576 && stub.sha256.len() == 64);
        let text = frame.to_string();
        assert!(
            !text.contains("patterns") && !text.contains("metadata"),
            "{text}"
        );
    }

    #[test]
    fn split_stubbed_and_failed_history_items_assemble_in_part_order_and_set_aside_or_break() {
        let page = link_frame(&case("history items as the plugin builds them"));
        let begin = link_frame(
            &serde_json::json!({"t":"sync_begin","sync":4,"reason":"connect",
            "scope":"full","as_of":0,"status":{},"status_ok":true}),
        );
        let end = link_frame(&serde_json::json!({"t":"sync_end","sync":4,"done_at":0,
            "list_ok":true,"permissions_ok":true,"questions_ok":true,"requests_ok":true,
            "lower":{},"pages":1,"items":6,"bytes":1,"activation":1}));
        let mut assembly = SyncAssembly::begin(&begin).unwrap();
        assembly.add(&page);
        let snapshot = assembly.finish(&end).unwrap();

        let session = &snapshot.sessions[0];
        let ids: Vec<&str> = session
            .messages
            .iter()
            .map(|m| message_id(&m.info))
            .collect();
        assert_eq!(
            ids,
            [
                "msg_0f0000800000xxxxxxxxxxxxxxx",
                "msg_0f0000900000xxxxxxxxxxxxxxx"
            ],
            "messages arrive newest first and are read oldest first"
        );
        let split = &session.messages[1];
        let parts: Vec<String> = split
            .parts
            .iter()
            .map(|part| match part {
                SnapPart::Whole(p) => p["id"].as_str().unwrap().to_string(),
                SnapPart::Stub {
                    ids,
                    part_type,
                    status,
                } => format!(
                    "{} stub {} {}",
                    ids.part_id.as_deref().unwrap(),
                    part_type.as_deref().unwrap(),
                    status.as_deref().unwrap()
                ),
            })
            .collect();
        assert_eq!(
            parts,
            [
                "prt_0f0009000000xxxxxxxxxxxxxxx",
                "prt_0f0009100000xxxxxxxxxxxxxxx",
                "prt_0f0009200000xxxxxxxxxxxxxxx stub tool completed",
            ],
            "the split message's parts, in order, the stub keeping its type and status"
        );
        assert_eq!(
            session.messages[0].parts.len(),
            1,
            "the user message, whole"
        );
        assert_eq!(
            snapshot.damaged,
            BTreeSet::from(["ses_0f00d000000dDDDDDDDDDDDDDD".to_string()]),
            "the session over the page cap is set aside, and only it"
        );
        let broken = snapshot.broken.expect("a session whose messages failed");
        assert!(broken.contains("could not be read"), "{broken}");
    }

    #[test]
    fn a_request_from_the_tui_store_is_listed_unknown_and_dead_by_its_ancestor() {
        let LinkFrame::SyncRequest { verdict, .. } =
            link_frame(&case("the permission list failed"))
        else {
            panic!("not a request");
        };
        assert_eq!(verdict.listed, Listed::Unknown(UnknownWord::Unknown));
        assert_eq!(verdict.dead.as_deref(), Some("epoch:abort:ancestor"));
        assert!(!verdict.live && verdict.in_store);
    }

    #[test]
    fn a_request_asked_before_the_activation_is_never_live() {
        let LinkFrame::SyncRequest { verdict, .. } =
            link_frame(&case("a request asked before this activation"))
        else {
            panic!("not a request");
        };
        assert!(!verdict.live);
        assert_eq!(verdict.dead.as_deref(), Some("asked-before-activation"));
    }

    #[test]
    fn a_snapshot_whose_lists_failed_says_so() {
        let end = link_frame(&case(
            "a snapshot whose session list and permission list failed",
        ));
        let begin = link_frame(
            &serde_json::json!({"t":"sync_begin","sync":5,"reason":"connect",
            "scope":"full","as_of":30,"status":{},"status_ok":true}),
        );
        let snapshot = SyncAssembly::begin(&begin).unwrap().finish(&end).unwrap();
        assert!(!snapshot.list_ok && !snapshot.requests_ok);
    }

    fn hello(activation: u64) -> protocol::ipc::OpencodeHello {
        let frame = rows()
            .into_iter()
            .find(|row| {
                row.frame["type"] == "opencode_hello" && row.frame["activation"] == activation
            })
            .unwrap()
            .frame;
        let ClientFrame::OpencodeHello(hello) = serde_json::from_value(frame).unwrap() else {
            panic!("not a hello");
        };
        hello
    }

    fn ev(seq: u64, event: &str) -> LinkFrame {
        LinkFrame::Ev {
            seq,
            event: event.into(),
            properties: Map::new(),
        }
    }

    #[test]
    fn a_frame_already_passed_is_dropped_and_a_second_activation_starts_over() {
        let mut filter = SeqFilter::default();
        filter.admitted(&hello(1));
        assert!(filter.pass(&ev(5, "session.idle")));
        assert!(!filter.pass(&ev(5, "session.idle")));
        assert!(!filter.pass(&ev(4, "session.status")));
        // A card held back by nothing is never refused for its place.
        assert!(filter.pass(&ev(3, "permission.asked")));
        // A reconnect of the same activation keeps the mark.
        filter.admitted(&hello(1));
        assert!(!filter.pass(&ev(5, "session.idle")));
        assert!(filter.pass(&ev(6, "session.idle")));

        let second = hello(2);
        assert_eq!((second.pid, second.start), (hello(1).pid, hello(1).start));
        filter.admitted(&second);
        assert!(
            filter.pass(&ev(1, "session.idle")),
            "a new activation counts from 1"
        );
    }
}
