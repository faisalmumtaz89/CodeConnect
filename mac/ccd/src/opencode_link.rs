//! The OpenCode plugin's link: admission, its frames, and what they become.
//!
//! The CodeConnect plugin runs inside the OpenCode TUI
//! (`mac/codeconnect/opencode-plugin/codeconnect-opencode.js`) and dials the
//! daemon's IPC socket. Its first line is a hello
//! ([`protocol::ipc::OpencodeHello`]), which [`serve`] admits or refuses against
//! the kernel, tmux and the run's registration — never against the hello's own
//! word. Once admitted, every line it writes is one [`LinkFrame`], told apart by
//! `t`:
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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use protocol::event::{EventKind, PendingEvent, SessionKey, Source};
use protocol::ipc::{DaemonFrame, OpencodeApi, OpencodeBound, OpencodeHello, OpencodeStart};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::io::BufReader;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::opencode_adapter::OpencodeAdapter;
use crate::state::Daemon;

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
    /// A session stub's parent session, so the daemon can find its root.
    #[serde(rename = "parentID", default, skip_serializing_if = "Option::is_none")]
    pub(crate) parent_id: Option<String>,
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
    /// Child → parent for the sessions sent as a stub, whose info is gone.
    pub(crate) stub_parents: BTreeMap<String, String>,
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
    /// Validated when the frame is decoded and kept with the request. No card
    /// on an OpenCode run is answered from the phone, so only tests read it.
    #[cfg_attr(not(test), allow(dead_code))]
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
    items: u64,
    sessions: Vec<SnapSession>,
    index: HashMap<String, usize>,
    /// (session, message) → where that message is in its session.
    message_index: HashMap<(String, String), usize>,
    requests: Vec<SnapRequest>,
    broken: Option<String>,
    damaged: BTreeSet<String>,
    stub_parents: BTreeMap<String, String>,
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
            items: 0,
            sessions: Vec::new(),
            index: HashMap::new(),
            message_index: HashMap::new(),
            requests: Vec::new(),
            broken: None,
            damaged: BTreeSet::new(),
            stub_parents: BTreeMap::new(),
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
                self.items = self.items.saturating_add(items.len() as u64);
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
                    if let (StubKind::Session, Some(parent)) = (kind, &ids.parent_id) {
                        self.stub_parents.insert(session.clone(), parent.clone());
                    }
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
        match self
            .message_index
            .get(&(session.to_string(), id.to_string()))
        {
            Some(&known) if split && messages[known].split => messages[known].parts.extend(parts),
            Some(_) => self.fail(format!("message {id} sent twice")),
            None => {
                self.message_index
                    .insert((session.to_string(), id.to_string()), messages.len());
                messages.push(SnapMessage {
                    info: info.clone(),
                    parts,
                    split,
                });
            }
        }
    }

    /// A part stub joins the split message it belongs to, which the plugin has
    /// always sent ahead of it.
    fn part_of(&mut self, session: &str, message: &str, stub: SnapPart) {
        let at = self.index.get(session).copied();
        let known = self
            .message_index
            .get(&(session.to_string(), message.to_string()))
            .copied();
        match at.zip(known) {
            Some((at, known)) => self.sessions[at].messages[known].parts.push(stub),
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
            pages,
            items,
            ..
        } = frame
        else {
            return None;
        };
        if *sync != self.sync {
            self.fail(format!("snapshot {} ended by {sync}", self.sync));
        }
        if (*pages, *items) != (self.next_page, self.items) {
            self.fail(format!(
                "snapshot {sync} sent {pages} pages of {items} items, {} of {} arrived",
                self.next_page, self.items
            ));
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
            stub_parents: self.stub_parents,
        })
    }
}

fn message_id(info: &Map<String, Value>) -> &str {
    info.get("id").and_then(Value::as_str).unwrap_or_default()
}

// ------------------------------------------------------------ the live link

/// The longest line the link reads. The plugin writes no frame over 1 MiB — a
/// bigger one becomes a stub — and the slack keeps a frame at the cap from being
/// refused for its newline; a longer line ends the link.
const LINK_LINE_BYTES: usize = 1024 * 1024 + 64 * 1024;

/// The `link_state` reasons a run's timeline shows.
const ATTACHED: &str = "OpenCode plugin connected";
const LOST: &str = "OpenCode plugin link lost";
const ELSEWHERE: &str = "OpenCode is showing a session from another folder";
pub(crate) const NEVER: &str = "the CodeConnect plugin did not connect (OpenCode plugins \
                                disabled, or the plugin failed to load)";

/// What one run's link keeps from one connection to the next: the mapper, the
/// plugin's sequence mark, a snapshot being assembled, and the run's turns.
/// Held by one frame at a time, so a connection that took over waits for its
/// predecessor's frame.
pub(crate) struct Observer {
    pub(crate) adapter: OpencodeAdapter,
    seq: SeqFilter,
    sync: Option<SyncAssembly>,
    /// A fresh snapshot was asked for on this connection and no `settled` has
    /// arrived since.
    resync_asked: bool,
    /// The turns the log holds for the run, kept as facts are recorded; `None`
    /// until the log could be read.
    turns: Option<Turns>,
}

/// What the log holds of a run that an observer starts from, read when the
/// daemon first holds the run.
#[derive(Debug, Default)]
pub(crate) struct Logged {
    /// `(request id, fact key, turn)` of each card still open.
    open_cards: Vec<(String, String, Option<String>)>,
    /// `(fact key, line)` of each model change, oldest first.
    model_changes: Vec<(String, String)>,
    /// `(turn, root, closed)` of each turn; `None` when it could not be read.
    turns: Option<Vec<(String, String, bool)>>,
}

impl Logged {
    /// Read from the log. What cannot be read is logged and left empty.
    pub(crate) async fn read(daemon: &Daemon, session: &SessionKey) -> Logged {
        let uid = || session.uid.clone();
        let warn = |what: &str, err: anyhow::Error| {
            crate::log_warn!("could not read the {what} of {}: {err:#}", session.name);
        };
        let open_cards = daemon
            .db
            .opencode_open_cards(uid())
            .await
            .unwrap_or_else(|err| {
                warn("open OpenCode cards", err);
                Vec::new()
            });
        let model_changes = daemon
            .db
            .opencode_model_changes(uid())
            .await
            .unwrap_or_else(|err| {
                warn("OpenCode model changes", err);
                Vec::new()
            });
        let turns = daemon
            .db
            .opencode_turns(uid())
            .await
            .map_err(|err| warn("OpenCode turns", err))
            .ok();
        Logged {
            open_cards,
            model_changes,
            turns,
        }
    }
}

impl Observer {
    /// A fresh observer for `session`, told what the log holds of it: the
    /// cards open, so a snapshot can still clear them; each root's last model,
    /// so a change is still said; and the turns, which the welcome and every
    /// resync acknowledge.
    pub(crate) fn new(session: SessionKey, logged: Logged) -> Self {
        let mut adapter = OpencodeAdapter::new(session);
        for (request_id, key, turn) in logged.open_cards {
            let Some((asked_in, rest)) = key.split_once(':') else {
                continue;
            };
            let kind = if rest.starts_with("question:") {
                RequestKind::Question
            } else if rest.starts_with("perm:") {
                RequestKind::Permission
            } else {
                continue;
            };
            adapter.restore_open_card(&request_id, asked_in, kind, turn);
        }
        for (key, line) in logged.model_changes.iter().rev() {
            if let Some((root, _)) = key.split_once(':') {
                adapter.restore_selection(root, line);
            }
        }
        Observer {
            adapter,
            seq: SeqFilter::default(),
            sync: None,
            resync_asked: false,
            turns: logged.turns.map(Turns::from_log),
        }
    }

    /// Where each root's next snapshot starts. The log is read here only when
    /// it could not be read before.
    ///
    /// A root whose last agent and model this observer does not know — the
    /// daemon restarted and the log says no change — is acked from its newest
    /// closed turn inclusive, so the snapshot brings that turn's prompt and
    /// with it the choice the next prompt is compared with. What the snapshot
    /// repeats of that turn is already in the log under the same keys.
    async fn acked(
        &mut self,
        daemon: &Daemon,
        session_uid: &str,
    ) -> BTreeMap<String, OpencodeBound> {
        if self.turns.is_none() {
            match daemon.db.opencode_turns(session_uid.to_string()).await {
                Ok(turns) => self.turns = Some(Turns::from_log(turns)),
                Err(err) => {
                    crate::log_warn!("could not read the OpenCode turns of {session_uid}: {err:#}");
                }
            }
        }
        let adapter = &self.adapter;
        let mut acked = self.turns.as_ref().map(Turns::acked).unwrap_or_default();
        for (root, bound) in &mut acked {
            bound.inclusive |= !adapter.knows_selection(root);
        }
        acked
    }
}

/// The registered run a hello's nonce names, as admission needs it.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub(crate) session: SessionKey,
    /// The folder the run was launched in.
    pub(crate) cwd: String,
    pub(crate) tmux_socket: String,
    /// The run's own directory, where `agent.json` is written.
    pub(crate) dir: PathBuf,
}

/// A hello the run took: the link's name, the run's observer, and the signal
/// that this connection is no longer the run's link.
pub(crate) struct Admitted {
    pub(crate) link: String,
    pub(crate) observer: Arc<Mutex<Observer>>,
    pub(crate) stop: oneshot::Receiver<()>,
}

/// A hello turned away, as the plugin is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) reason: String,
    /// The plugin stops dialling for this activation.
    pub(crate) r#final: bool,
}

impl Refusal {
    pub(crate) fn last(reason: impl Into<String>) -> Self {
        Refusal {
            reason: reason.into(),
            r#final: true,
        }
    }

    pub(crate) fn again(reason: impl Into<String>) -> Self {
        Refusal {
            reason: reason.into(),
            r#final: false,
        }
    }
}

/// What admission asks of the kernel, of tmux and of the run's directory. One
/// seam, so the admission can be exercised without an OpenCode process.
pub(crate) trait Witness: Send + Sync {
    /// The parent of `pid`.
    fn parent(&self, pid: i32) -> Option<i32>;
    /// When `pid` started.
    fn start(&self, pid: i32) -> Option<OpencodeStart>;
    /// The process of the first pane of the tmux session the run `uid` owns,
    /// as [`protocol::tmux::pane_pid`] answers it.
    fn pane(&self, tmux_socket: &str, uid: &str) -> Result<i32, protocol::tmux::ResolveError>;
    /// The OpenCode process the run's `agent.json` records: its pid and start.
    fn recorded(&self, dir: &Path) -> Option<(i32, OpencodeStart)>;
}

/// The real answers: `proc_pidinfo`, tmux, and the file the pane wrote.
struct Kernel;

impl Witness for Kernel {
    fn parent(&self, pid: i32) -> Option<i32> {
        protocol::proc_identity::read_ppid(pid)
    }

    fn start(&self, pid: i32) -> Option<OpencodeStart> {
        let birth = protocol::proc_identity::read_birth_identity(pid)?;
        Some(OpencodeStart {
            sec: birth.start_sec,
            usec: birth.start_usec,
        })
    }

    fn pane(&self, tmux_socket: &str, uid: &str) -> Result<i32, protocol::tmux::ResolveError> {
        protocol::tmux::pane_pid(tmux_socket, uid)
    }

    /// Read through one descriptor: opened without following a link and
    /// without waiting for a writer, then judged by what it is, not by what
    /// the name pointed at a moment before.
    fn recorded(&self, dir: &Path) -> Option<(i32, OpencodeStart)> {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        /// The file is a few dozen bytes; anything larger is not one the pane wrote.
        const MOST: u64 = 4096;
        #[derive(Deserialize)]
        struct Recorded {
            pid: i32,
            start: OpencodeStart,
        }
        // A FIFO or a device opens at once under `O_NONBLOCK`, and is refused
        // below as not a regular file.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(dir.join("agent.json"))
            .ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.len() > MOST {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MOST + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > MOST {
            return None;
        }
        let recorded: Recorded = serde_json::from_slice(&bytes).ok()?;
        Some((recorded.pid, recorded.start))
    }
}

/// Serve one plugin connection whose first line was `hello`, until it ends.
///
/// Admission runs first, in order, and the first check that fails refuses:
///
///   1. the link grammar is version 1;
///   2. the nonce names a registered OpenCode run that has not ended — not a
///      final refusal, because the supervisor may not have registered yet;
///   3. the pid the kernel reports for the connection's peer is the pid the
///      hello names;
///   4. that process's parent is the first pane's process of the run's own tmux
///      session, so an `opencode` the agent's shell started is refused;
///   5. its kernel start time is the one the hello names, and the run's
///      `agent.json` records the same pid and start;
///   6. it opened the run's folder;
///   7. the run's first admission pinned this pid and start for its life; a
///      later activation of the same process may link, an older one may not;
///   8. a live link of the same process is taken over: its connection closes,
///      and nothing it says afterwards is read.
///
/// Steps 3 to 6 get [`VERIFY_BUDGET`]; a check that takes longer is refused,
/// not final. A refused hello changes nothing, a live link least of all.
pub(crate) async fn serve(
    daemon: &Arc<Daemon>,
    hello: OpencodeHello,
    peer: Option<i32>,
    reader: &mut BufReader<OwnedReadHalf>,
    tx: &mpsc::Sender<DaemonFrame>,
) {
    serve_with(daemon, &Kernel, hello, peer, reader, tx).await
}

pub(crate) async fn serve_with(
    daemon: &Arc<Daemon>,
    witness: &'static dyn Witness,
    hello: OpencodeHello,
    peer: Option<i32>,
    reader: &mut BufReader<OwnedReadHalf>,
    tx: &mpsc::Sender<DaemonFrame>,
) {
    let (run, mut admitted) = match admit(daemon, witness, &hello, peer).await {
        Ok(admitted) => admitted,
        Err(refusal) => {
            crate::log_info!(
                "opencode link refused for pid {} (final: {}): {}",
                hello.pid,
                refusal.r#final,
                refusal.reason
            );
            let _ = tx
                .send(DaemonFrame::OpencodeRefused {
                    reason: refusal.reason,
                    r#final: refusal.r#final,
                })
                .await;
            return;
        }
    };
    let link = admitted.link.clone();
    let acked = {
        let mut observer = admitted.observer.lock().await;
        observer.seq.admitted(&hello);
        observer.sync = None;
        observer.resync_asked = false;
        observer.acked(daemon, &run.session.uid).await
    };
    record_link_state(
        daemon,
        &run.session,
        &format!("link:{link}:attached"),
        "attached",
        &attached_reason(&hello.api),
    )
    .await;
    let _ = tx
        .send(DaemonFrame::OpencodeWelcome {
            link: link.clone(),
            acked,
        })
        .await;
    crate::log_info!(
        "opencode link {link} attached for {} (pid {}, activation {})",
        run.session.name,
        hello.pid,
        hello.activation
    );

    let reason = loop {
        let line = tokio::select! {
            line = crate::ipc_server::read_link_line(reader, LINK_LINE_BYTES) => line,
            // Taken over, or the run ended: this connection is no longer the
            // run's link, and its close is nobody's news.
            _ = &mut admitted.stop => return,
        };
        let line = match line {
            Ok(Some(line)) => line,
            Ok(None) => break LOST,
            Err(err) => {
                crate::log_warn!("opencode link {link}: {err:#}; closing it");
                break LOST;
            }
        };
        let frame = match decode(&line) {
            Ok(Some(frame)) => Ok(frame),
            Ok(None) => continue,
            Err(err) => Err(err),
        };
        let mut observer = admitted.observer.lock().await;
        if !daemon
            .opencode_link_is_current(&run.session.uid, &link)
            .await
        {
            return;
        }
        let frame = match frame {
            Ok(frame) => frame,
            // What the frame carried is lost; a fresh snapshot brings it back.
            Err(err) => {
                crate::log_warn!("opencode link {link}: dropped an unreadable frame: {err}");
                ask_resync(daemon, &run, &mut observer, tx).await;
                continue;
            }
        };
        if let Some(reason) = step(daemon, &run, &link, &mut observer, frame, tx).await {
            break reason;
        }
    };
    if daemon.opencode_link_closed(&run.session.uid, &link).await {
        crate::log_info!(
            "opencode link {link} for {} closed: {reason}",
            run.session.name
        );
        record_link_state(
            daemon,
            &run.session,
            &format!("link:{link}:detached"),
            "detached",
            reason,
        )
        .await;
    }
}

/// How long steps 3 to 6 may take. Each tmux call they make is bounded at one
/// second, and the rest are reads of the kernel and of a file of a few dozen
/// bytes. The plugin waits 3 s for its answer to a hello, so a refusal for
/// time arrives while it still listens, and it dials again.
const VERIFY_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Steps 1 to 8 of [`serve`]'s admission.
async fn admit(
    daemon: &Arc<Daemon>,
    witness: &'static dyn Witness,
    hello: &OpencodeHello,
    peer: Option<i32>,
) -> Result<(Candidate, Admitted), Refusal> {
    if hello.wire != 1 {
        return Err(Refusal::last(format!(
            "this daemon reads OpenCode link version 1, not {}",
            hello.wire
        )));
    }
    let Some(run) = daemon.opencode_candidate(&hello.nonce).await else {
        return Err(Refusal::again(
            "no OpenCode run is registered under this nonce",
        ));
    };
    let checked = {
        let (hello, run) = (hello.clone(), run.clone());
        let blocking = tokio::task::spawn_blocking(move || verify(witness, &hello, peer, &run));
        tokio::time::timeout(VERIFY_BUDGET, blocking).await
    };
    match checked {
        Ok(Ok(verified)) => verified?,
        Ok(Err(_)) => return Err(Refusal::last("the OpenCode process could not be checked")),
        Err(_) => {
            return Err(Refusal::again(format!(
                "the OpenCode process could not be checked within {VERIFY_BUDGET:?}"
            )))
        }
    }
    let admitted = daemon.opencode_admit(&run.session.uid, hello).await?;
    Ok((run, admitted))
}

/// Steps 3 to 6: the hello's claims against the kernel, tmux, the run's
/// recorded start and its folder. Every answer that was read and does not
/// match is final: none of them changes for the same process. Only a tmux that
/// could not be asked, or that names more than one session for the run, is
/// not.
fn verify(
    witness: &dyn Witness,
    hello: &OpencodeHello,
    peer: Option<i32>,
    run: &Candidate,
) -> Result<(), Refusal> {
    if peer != Some(hello.pid) {
        return Err(Refusal::last(
            "the process on this connection is not the OpenCode process the hello names",
        ));
    }
    // A tmux that could not be asked, or that holds more than one session for
    // the run, proves nothing either way, and may answer the next dial.
    let pane = match witness.pane(&run.tmux_socket, &run.session.uid) {
        Ok(pid) => Some(pid),
        Err(protocol::tmux::ResolveError::NotHosted) => None,
        Err(protocol::tmux::ResolveError::IdentityMismatch(why)) => {
            return Err(Refusal::again(format!(
                "the run's terminal is ambiguous: more than one tmux session carries \
                 this run ({why})"
            )))
        }
        Err(protocol::tmux::ResolveError::Unavailable(why)) => {
            return Err(Refusal::again(format!(
                "the run's terminal could not be checked ({why})"
            )))
        }
    };
    if pane.is_none() || witness.parent(hello.pid) != pane {
        return Err(Refusal::last(
            "this OpenCode is not the one CodeConnect started in the run's pane",
        ));
    }
    if witness.start(hello.pid) != Some(hello.start)
        || witness.recorded(&run.dir) != Some((hello.pid, hello.start))
    {
        return Err(Refusal::last("stale start time"));
    }
    if hello.directory.as_deref() != Some(run.cwd.as_str()) {
        return Err(Refusal::last(
            "OpenCode opened a different folder than the one CodeConnect launched it in",
        ));
    }
    Ok(())
}

/// Apply one admitted frame. `Some` ends the link, with the reason its
/// timeline is given.
async fn step(
    daemon: &Daemon,
    run: &Candidate,
    link: &str,
    observer: &mut Observer,
    frame: LinkFrame,
    tx: &mpsc::Sender<DaemonFrame>,
) -> Option<&'static str> {
    if !observer.seq.pass(&frame) {
        return None;
    }
    match &frame {
        LinkFrame::Head { route, .. } => {
            let (head, directory) = match route {
                HeadRoute::Session {
                    session_id,
                    directory,
                } => (Some(session_id.clone()), directory.as_deref()),
                HeadRoute::Home | HeadRoute::Other => (None, None),
            };
            if directory.is_some_and(|directory| directory != run.cwd) {
                let _ = tx
                    .send(DaemonFrame::OpencodeRefused {
                        reason: ELSEWHERE.into(),
                        r#final: true,
                    })
                    .await;
                return Some(ELSEWHERE);
            }
            daemon.opencode_head(&run.session.uid, link, head).await;
        }
        LinkFrame::SyncBegin { .. } => {
            observer.adapter.sync_begin();
            observer.sync = SyncAssembly::begin(&frame);
        }
        LinkFrame::SyncRequest { .. } | LinkFrame::SyncPage { .. } => match &mut observer.sync {
            Some(assembly) => assembly.add(&frame),
            None => crate::log_warn!("opencode link {link}: a snapshot frame outside a snapshot"),
        },
        LinkFrame::SyncEnd { .. } => {
            let snapshot = observer.sync.take().and_then(|a| a.finish(&frame));
            match snapshot
                .as_ref()
                .and_then(|s| observer.adapter.plan_resync(s))
            {
                // The facts are made durable before the adapter forgets what it
                // held open: they are all that is left of it afterwards.
                Some((facts, staged)) => {
                    if record(daemon, &mut observer.turns, facts).await {
                        observer.adapter.apply_resync(staged);
                    }
                }
                None => crate::log_info!(
                    "opencode link {link}: snapshot not read ({})",
                    snapshot
                        .as_ref()
                        .and_then(|s| s.broken.clone())
                        .unwrap_or_else(|| "incomplete or unreadable".into())
                ),
            }
        }
        LinkFrame::Settled { .. } => {
            observer.resync_asked = false;
            let facts = observer.adapter.settle();
            record(daemon, &mut observer.turns, facts).await;
        }
        LinkFrame::Ev { .. } | LinkFrame::Stub { .. } | LinkFrame::CardStub { .. } => {
            if let LinkFrame::Stub {
                event,
                size,
                sha256,
                ..
            }
            | LinkFrame::CardStub {
                event,
                size,
                sha256,
                ..
            } = &frame
            {
                crate::log_info!(
                    "opencode link {link}: {event} over the frame cap ({size} bytes, sha256 {sha256})"
                );
            }
            let facts = observer.adapter.ingest(&frame);
            record(daemon, &mut observer.turns, facts).await;
        }
    }
    if observer.adapter.take_resync_request() {
        ask_resync(daemon, run, observer, tx).await;
    }
    None
}

/// Ask the plugin for a fresh full snapshot, unless one is already asked for
/// and no `settled` has come since: a stream of bad frames asks once.
async fn ask_resync(
    daemon: &Daemon,
    run: &Candidate,
    observer: &mut Observer,
    tx: &mpsc::Sender<DaemonFrame>,
) {
    if std::mem::replace(&mut observer.resync_asked, true) {
        return;
    }
    let acked = observer.acked(daemon, &run.session.uid).await;
    let _ = tx.send(DaemonFrame::OpencodeResync { acked }).await;
}

/// Each fact into the log, in the order made, and into the run's turns once
/// it is there. False when one could not be written.
async fn record(daemon: &Daemon, turns: &mut Option<Turns>, facts: Vec<PendingEvent>) -> bool {
    let mut whole = true;
    for fact in facts {
        let counted = Turns::counted(&fact);
        match daemon.ingest(fact).await {
            Ok(_) => {
                if let (Some(turns), Some(counted)) = (turns.as_mut(), counted) {
                    turns.note(counted);
                }
            }
            Err(err) => {
                crate::log_error!("opencode link: a fact could not be recorded: {err:#}");
                whole = false;
            }
        }
    }
    whole
}

/// A `link_state` row of the run's timeline.
pub(crate) async fn record_link_state(
    daemon: &Daemon,
    session: &SessionKey,
    key: &str,
    link: &str,
    reason: &str,
) {
    let pending = PendingEvent::new(
        session,
        EventKind::LinkState,
        serde_json::json!({"link": link, "reason": reason}),
        Source::Daemon,
    )
    .with_source_event_id(key.to_string());
    if let Err(err) = daemon.ingest(pending).await {
        crate::log_error!(
            "failed to record the OpenCode link state of {}: {err:#}",
            session.name
        );
    }
}

/// An admitted link's reason, naming what a plugin that cannot use the whole
/// API is missing.
fn attached_reason(api: &OpencodeApi) -> String {
    if api.ok {
        return ATTACHED.to_string();
    }
    let opencode = match &api.version {
        Some(version) => format!("OpenCode {version}"),
        None => "OpenCode".to_string(),
    };
    format!(
        "{ATTACHED}; observe-only: {opencode} lacks {}",
        api.missing.join(", ")
    )
}

/// The turns of a run, as [`crate::store::Store::opencode_turns`] reads them
/// from the log: turn → its root, and whether its TurnComplete is there.
#[derive(Debug, Default)]
pub(crate) struct Turns(BTreeMap<String, (String, bool)>);

impl Turns {
    /// From the log's `(turn, root, closed)` rows.
    fn from_log(rows: Vec<(String, String, bool)>) -> Turns {
        Turns(
            rows.into_iter()
                .map(|(turn, root, closed)| (turn, (root, closed)))
                .collect(),
        )
    }

    /// What a fact tells of its turn — `(turn, root, closed)` — when it
    /// counts. Only a turn's facts on its root do, as in the log's reading: a
    /// subagent's carry `subagent_session`.
    fn counted(fact: &PendingEvent) -> Option<(String, String, bool)> {
        let (Some(turn), Some(key)) = (&fact.turn_id, &fact.source_event_id) else {
            return None;
        };
        if fact.source != Source::Opencode || !fact.payload["subagent_session"].is_null() {
            return None;
        }
        let root = key.split_once(':').map_or(key.as_str(), |(root, _)| root);
        let closed = fact.kind == EventKind::TurnComplete;
        Some((turn.clone(), root.to_string(), closed))
    }

    /// A counted fact, now in the log.
    fn note(&mut self, (turn, root, closed): (String, String, bool)) {
        self.0.entry(turn).or_insert((root, false)).1 |= closed;
    }

    fn acked(&self) -> BTreeMap<String, OpencodeBound> {
        acked(
            self.0
                .iter()
                .map(|(turn, (root, closed))| (turn.as_str(), root.as_str(), *closed)),
        )
    }
}

/// Where each root's next snapshot starts, from the turns the log holds —
/// `(turn, root, closed)`: after its newest closed turn, or, while none of its
/// turns is closed, from its oldest turn, inclusive.
pub(crate) fn acked<'a>(
    turns: impl IntoIterator<Item = (&'a str, &'a str, bool)>,
) -> BTreeMap<String, OpencodeBound> {
    let mut closed: BTreeMap<&str, &str> = BTreeMap::new();
    let mut open: BTreeMap<&str, &str> = BTreeMap::new();
    for (turn, root, done) in turns {
        if done {
            let newest = closed.entry(root).or_insert(turn);
            if turn > *newest {
                *newest = turn;
            }
        } else {
            let oldest = open.entry(root).or_insert(turn);
            if turn < *oldest {
                *oldest = turn;
            }
        }
    }
    let mut out: BTreeMap<String, OpencodeBound> = open
        .into_iter()
        .map(|(root, turn)| {
            let bound = OpencodeBound {
                from: Some(turn.to_string()),
                inclusive: true,
            };
            (root.to_string(), bound)
        })
        .collect();
    for (root, turn) in closed {
        let bound = OpencodeBound {
            from: Some(turn.to_string()),
            inclusive: false,
        };
        out.insert(root.to_string(), bound);
    }
    out
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
        assert_eq!(
            snapshot.stub_parents,
            BTreeMap::from([(
                "ses_0f00d000000dDDDDDDDDDDDDDD".to_string(),
                "ses_0f00c000000cCCCCCCCCCCCCCC".to_string()
            )]),
            "its parent is kept, so its root tree can be set aside"
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

    #[test]
    fn a_snapshot_missing_a_page_or_an_item_it_announced_is_broken() {
        let begin = link_frame(
            &serde_json::json!({"t":"sync_begin","sync":6,"reason":"connect",
            "scope":"full","as_of":0,"status":{},"status_ok":true}),
        );
        let page = link_frame(&serde_json::json!({"t":"sync_page","sync":6,"n":0,"items":[
            {"session":{"id":"ses_0f00a000000aAAAAAAAAAAAAAA"}},
            {"session":{"id":"ses_0f00b000000bBBBBBBBBBBBBBB"}}]}));
        let end = |pages: u64, items: u64| {
            link_frame(&serde_json::json!({"t":"sync_end","sync":6,"done_at":0,
                "list_ok":true,"permissions_ok":true,"questions_ok":true,"requests_ok":true,
                "lower":{},"pages":pages,"items":items,"bytes":1,"activation":1}))
        };
        let read = |pages: u64, items: u64| {
            let mut assembly = SyncAssembly::begin(&begin).unwrap();
            assembly.add(&page);
            assembly.finish(&end(pages, items)).unwrap().broken
        };
        assert_eq!(read(1, 2), None);
        let lost_page = read(2, 3).expect("a page never arrived");
        assert!(
            lost_page.contains("2 pages of 3 items, 1 of 2 arrived"),
            "{lost_page}"
        );
        assert!(read(1, 3).is_some(), "an item never arrived");
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

    // ------------------------------------------------------------ the live link

    use protocol::agent::AgentKind;
    use protocol::tmux::ResolveError;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    const FOLDER: &str = "/Users/ada/project";
    const PANE: i32 = 4000;

    fn fresh_database() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let db = std::env::temp_dir().join(format!(
            "ccd-opencode-{}-{n}-{}.db",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&db);
        db
    }

    fn daemon() -> Arc<Daemon> {
        daemon_on(&fresh_database())
    }

    fn daemon_on(db: &Path) -> Arc<Daemon> {
        let store = Arc::new(crate::store::Store::open(db).unwrap());
        let (tail_tx, tail_rx) = mpsc::unbounded_channel();
        Box::leak(Box::new(tail_rx));
        Daemon::new(
            protocol::config::Config::default(),
            store,
            Arc::new(crate::apns::LoggingPushSender::new()),
            crate::state::Endpoint {
                host: "test.ts.net".into(),
                port: 8787,
                tls: false,
            },
            tail_tx,
        )
    }

    fn opencode_frame(uid: &str, nonce: Option<&str>) -> protocol::ipc::RegisterSession {
        protocol::ipc::RegisterSession {
            session_id: "cc-1".into(),
            session_uid: Some(uid.into()),
            tmux_session: "cc-1".into(),
            tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
            cwd: FOLDER.into(),
            supervisor_pid: 4242,
            claude_bin: None,
            agent: AgentKind::Opencode,
            agent_bin: Some("/opt/homebrew/bin/opencode".into()),
            codex_thread_id: None,
            codex_socket: None,
            codex_generation: None,
            opencode_nonce: nonce.map(str::to_string),
            started_at: protocol::time::now_rfc3339(),
            protocol_minor: protocol::PROTOCOL_MINOR,
            exit_replay: false,
        }
    }

    async fn try_register(
        daemon: &Arc<Daemon>,
        info: protocol::ipc::RegisterSession,
    ) -> anyhow::Result<crate::state::Registration> {
        let (tx, _rx) = mpsc::channel(8);
        daemon
            .register_supervisor(info, tx, Arc::new(std::sync::Mutex::new(HashMap::new())))
            .await
    }

    async fn register(daemon: &Arc<Daemon>) -> (String, crate::state::Registration) {
        let uid = protocol::uid::new().unwrap();
        let registration = try_register(daemon, opencode_frame(&uid, Some(NONCE)))
            .await
            .expect("an OpenCode registration with its nonce is accepted");
        (uid, registration)
    }

    /// Admission's view of the world.
    #[derive(Clone)]
    struct Fake {
        parent: Option<i32>,
        start: Option<OpencodeStart>,
        pane: Result<i32, ResolveError>,
        recorded: Option<(i32, OpencodeStart)>,
        /// How long tmux takes to answer.
        wait: std::time::Duration,
    }

    /// A [`Fake`] that answers only questions about the process a hello names
    /// and the run its nonce names, and fails the admission on any other.
    struct Asked {
        world: Fake,
        pid: i32,
        run: Option<Candidate>,
    }

    impl Asked {
        fn run(&self) -> &Candidate {
            self.run
                .as_ref()
                .expect("a question about a run the nonce names")
        }
    }

    impl Witness for Asked {
        fn parent(&self, pid: i32) -> Option<i32> {
            assert_eq!(pid, self.pid, "the parent of another process");
            self.world.parent
        }
        fn start(&self, pid: i32) -> Option<OpencodeStart> {
            assert_eq!(pid, self.pid, "the start of another process");
            self.world.start
        }
        fn pane(&self, tmux_socket: &str, uid: &str) -> Result<i32, ResolveError> {
            let run = self.run();
            assert_eq!(
                (tmux_socket, uid),
                (run.tmux_socket.as_str(), run.session.uid.as_str()),
                "the pane of another run"
            );
            std::thread::sleep(self.world.wait);
            self.world.pane.clone()
        }
        fn recorded(&self, dir: &Path) -> Option<(i32, OpencodeStart)> {
            assert_eq!(dir, self.run().dir, "the record of another run");
            self.world.recorded
        }
    }

    /// The world of the OpenCode process `hello` names, started in the run's pane.
    fn honest(hello: &OpencodeHello) -> Fake {
        Fake {
            parent: Some(PANE),
            start: Some(hello.start),
            pane: Ok(PANE),
            recorded: Some((hello.pid, hello.start)),
            wait: std::time::Duration::ZERO,
        }
    }

    /// One plugin connection, driven from the plugin's side.
    struct Plugin {
        write: tokio::net::unix::OwnedWriteHalf,
        replies: mpsc::Receiver<DaemonFrame>,
        served: tokio::task::JoinHandle<()>,
    }

    impl Plugin {
        async fn dial(
            daemon: &Arc<Daemon>,
            witness: Fake,
            hello: OpencodeHello,
            peer: Option<i32>,
        ) -> Plugin {
            let (plugin, ours) = UnixStream::pair().unwrap();
            let (read, _) = ours.into_split();
            let (_, write) = plugin.into_split();
            let (tx, replies) = mpsc::channel(64);
            let witness: &'static Asked = Box::leak(Box::new(Asked {
                world: witness,
                pid: hello.pid,
                run: daemon.opencode_candidate(&hello.nonce).await,
            }));
            let daemon = Arc::clone(daemon);
            let served = tokio::spawn(async move {
                let mut reader = BufReader::new(read);
                serve_with(&daemon, witness, hello, peer, &mut reader, &tx).await;
            });
            Plugin {
                write,
                replies,
                served,
            }
        }

        async fn send(&mut self, frame: &Value) {
            let mut line = serde_json::to_vec(frame).unwrap();
            line.push(b'\n');
            self.write.write_all(&line).await.unwrap();
        }

        async fn reply(&mut self) -> Option<DaemonFrame> {
            tokio::time::timeout(std::time::Duration::from_secs(5), self.replies.recv())
                .await
                .expect("the daemon answers or closes")
        }

        async fn welcome(&mut self) -> (String, BTreeMap<String, OpencodeBound>) {
            match self.reply().await {
                Some(DaemonFrame::OpencodeWelcome { link, acked }) => (link, acked),
                other => panic!("expected a welcome, got {other:?}"),
            }
        }

        /// The plugin hangs up, and the daemon is done with everything it sent.
        async fn hang_up(self) {
            drop(self.write);
            self.served.await.unwrap();
        }
    }

    fn hello_for(pid: i32, sec: i64, activation: u64) -> OpencodeHello {
        OpencodeHello {
            wire: 1,
            nonce: NONCE.into(),
            pid,
            start: OpencodeStart { sec, usec: 7 },
            activation,
            directory: Some(FOLDER.into()),
            api: OpencodeApi {
                ok: true,
                missing: Vec::new(),
                version: Some("1.18.34".into()),
            },
        }
    }

    async fn admitted(daemon: &Arc<Daemon>, hello: OpencodeHello) -> (Plugin, String) {
        let pid = hello.pid;
        let mut plugin = Plugin::dial(daemon, honest(&hello), hello, Some(pid)).await;
        let (link, _) = plugin.welcome().await;
        (plugin, link)
    }

    fn link_rows(daemon: &Daemon, uid: &str) -> Vec<(String, Value)> {
        daemon
            .store
            .events_after(uid, 0, 1000)
            .unwrap()
            .into_iter()
            .filter(|e| {
                e.kind == EventKind::LinkState && e.payload["reason"] != "supervisor registered"
            })
            .map(|e| (e.source_event_id.unwrap_or_default(), e.payload))
            .collect()
    }

    async fn head(daemon: &Daemon, uid: &str) -> Option<String> {
        daemon
            .sessions()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.session_uid == uid)
            .unwrap()
            .opencode_session_id
    }

    async fn until<F: std::future::Future<Output = bool>>(mut check: impl FnMut() -> F) {
        for _ in 0..400 {
            if check().await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the condition never held");
    }

    #[tokio::test]
    async fn every_refused_hello_says_why_and_whether_to_dial_again_and_writes_nothing() {
        let daemon = daemon();
        let (uid, _registration) = register(&daemon).await;
        let good = hello_for(4242, 1_700_000_000, 1);
        let world = honest(&good);
        let with = |change: &dyn Fn(&mut OpencodeHello)| {
            let mut hello = good.clone();
            change(&mut hello);
            hello
        };
        let refusal = |hello: OpencodeHello, witness: Fake, peer: Option<i32>| {
            let daemon = Arc::clone(&daemon);
            async move {
                let mut plugin = Plugin::dial(&daemon, witness, hello, peer).await;
                let refused = match plugin.reply().await {
                    Some(DaemonFrame::OpencodeRefused { reason, r#final }) => (reason, r#final),
                    other => panic!("expected a refusal, got {other:?}"),
                };
                assert!(plugin.reply().await.is_none(), "the connection closes");
                refused
            }
        };
        let says = |(reason, last): (String, bool), want: &str, r#final: bool| {
            assert!(reason.contains(want), "{reason}");
            assert_eq!(last, r#final, "{reason}");
        };
        let peer = Some(4242);
        says(
            refusal(with(&|h| h.wire = 2), world.clone(), peer).await,
            "link version 1, not 2",
            true,
        );
        says(
            refusal(with(&|h| h.nonce = "f".repeat(32)), world.clone(), peer).await,
            "no OpenCode run is registered under this nonce",
            false,
        );
        for peer in [None, Some(4243)] {
            says(
                refusal(good.clone(), world.clone(), peer).await,
                "not the OpenCode process",
                true,
            );
        }
        let nested = Fake {
            parent: Some(PANE + 1),
            ..world.clone()
        };
        let paneless = Fake {
            pane: Err(ResolveError::NotHosted),
            parent: None,
            ..world.clone()
        };
        for witness in [nested, paneless] {
            says(
                refusal(good.clone(), witness, peer).await,
                "not the one CodeConnect started",
                true,
            );
        }
        // tmux could not be asked, or named two sessions for the run: the
        // plugin dials again, and nothing else was decided.
        let unasked = Fake {
            pane: Err(ResolveError::Unavailable(
                "tmux did not answer within 1000ms".into(),
            )),
            ..world.clone()
        };
        says(
            refusal(good.clone(), unasked, peer).await,
            "the run's terminal could not be checked (tmux did not answer within 1000ms)",
            false,
        );
        let ambiguous = Fake {
            pane: Err(ResolveError::IdentityMismatch(
                "2 sessions carry the uid".into(),
            )),
            ..world.clone()
        };
        says(
            refusal(good.clone(), ambiguous, peer).await,
            "the run's terminal is ambiguous: more than one tmux session carries this run \
             (2 sessions carry the uid)",
            false,
        );
        let reborn = Fake {
            start: Some(OpencodeStart { sec: 1, usec: 7 }),
            ..world.clone()
        };
        let misrecorded = Fake {
            recorded: Some((4243, good.start)),
            ..world.clone()
        };
        let unrecorded = Fake {
            recorded: None,
            ..world.clone()
        };
        for witness in [reborn, misrecorded, unrecorded] {
            says(
                refusal(good.clone(), witness, peer).await,
                "stale start time",
                true,
            );
        }
        for directory in [Some("/Users/ada/other".to_string()), None] {
            says(
                refusal(
                    with(&|h| h.directory = directory.clone()),
                    world.clone(),
                    peer,
                )
                .await,
                "a different folder",
                true,
            );
        }
        assert!(
            link_rows(&daemon, &uid).is_empty(),
            "a refused hello writes nothing"
        );

        let (_plugin, _) = admitted(&daemon, good).await;
        assert_eq!(link_rows(&daemon, &uid)[0].1["link"], "attached");
    }

    #[tokio::test]
    async fn an_admitted_link_is_attached_with_what_an_observe_only_plugin_lacks() {
        let daemon = daemon();
        let (uid, _registration) = register(&daemon).await;
        let mut hello = hello_for(4242, 1_700_000_000, 1);
        hello.api = OpencodeApi {
            ok: false,
            missing: vec!["api.route.current".into(), "api.slots.register".into()],
            version: Some("1.18.34".into()),
        };
        let (_plugin, link) = admitted(&daemon, hello).await;
        assert_eq!(
            link_rows(&daemon, &uid),
            [(
                format!("link:{link}:attached"),
                json!({"link": "attached", "reason": "OpenCode plugin connected; observe-only: \
                    OpenCode 1.18.34 lacks api.route.current, api.slots.register"})
            )]
        );
        let api = OpencodeApi {
            ok: false,
            missing: vec!["api.app.version".into()],
            version: None,
        };
        assert_eq!(
            attached_reason(&api),
            "OpenCode plugin connected; observe-only: OpenCode lacks api.app.version"
        );
    }

    #[tokio::test]
    async fn the_first_admission_pins_the_process_and_a_reconnect_takes_the_link_over() {
        let daemon = daemon();
        let (uid, _registration) = register(&daemon).await;
        let (mut first, first_link) = admitted(&daemon, hello_for(4242, 1_700_000_000, 1)).await;

        // Another OpenCode process is refused for good, and the live link is
        // left as it was.
        let other = hello_for(5151, 1_700_000_100, 1);
        let mut intruder = Plugin::dial(&daemon, honest(&other), other, Some(5151)).await;
        match intruder.reply().await {
            Some(DaemonFrame::OpencodeRefused { reason, r#final }) => {
                assert!(reason.contains("another OpenCode process"), "{reason}");
                assert!(r#final);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        first
            .send(
                &json!({"t": "head", "seq": 1, "route": "session", "session_id": "ses_a",
                "directory": FOLDER}),
            )
            .await;
        until(|| async { head(&daemon, &uid).await.as_deref() == Some("ses_a") }).await;

        // The same process again, a later activation: the new connection is the
        // link, and the old one is closed without a word in the timeline.
        let (second, second_link) = admitted(&daemon, hello_for(4242, 1_700_000_000, 2)).await;
        assert!(
            first.reply().await.is_none(),
            "the old connection is closed"
        );
        first.hang_up().await;
        // Had its end raced the takeover, the old link's close would still be
        // nobody's news.
        assert!(!daemon.opencode_link_closed(&uid, &first_link).await);
        assert!(daemon.opencode_link_is_current(&uid, &second_link).await);
        assert_eq!(head(&daemon, &uid).await.as_deref(), Some("ses_a"));

        // An activation older than the live link's is refused for good.
        let older = hello_for(4242, 1_700_000_000, 1);
        let mut stale = Plugin::dial(&daemon, honest(&older), older, Some(4242)).await;
        match stale.reply().await {
            Some(DaemonFrame::OpencodeRefused { reason, r#final }) => {
                assert!(reason.contains("newer activation"), "{reason}");
                assert!(r#final);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        second.hang_up().await;
        assert_eq!(
            link_rows(&daemon, &uid)
                .into_iter()
                .map(|(key, payload)| (key, payload["reason"].as_str().unwrap().to_string()))
                .collect::<Vec<_>>(),
            [
                (format!("link:{first_link}:attached"), ATTACHED.to_string()),
                (format!("link:{second_link}:attached"), ATTACHED.to_string()),
                (format!("link:{second_link}:detached"), LOST.to_string()),
            ],
            "the link that was taken over closes without a row"
        );
        assert_eq!(head(&daemon, &uid).await, None, "no live link, no head");
    }

    #[test]
    fn a_root_is_acked_after_its_newest_closed_turn_or_from_its_oldest_open_one() {
        let turn = |turn: &str, root: &str, closed: bool| (turn.into(), root.into(), closed);
        let acked = Turns::from_log(vec![
            turn("msg_02", "ses_a", true),
            turn("msg_05", "ses_a", true),
            turn("msg_07", "ses_a", false),
            turn("msg_03", "ses_a", false),
            turn("msg_09", "ses_b", false),
            turn("msg_04", "ses_b", false),
        ])
        .acked();
        let bound = |from: &str, inclusive: bool| OpencodeBound {
            from: Some(from.into()),
            inclusive,
        };
        assert_eq!(
            acked,
            BTreeMap::from([
                ("ses_a".to_string(), bound("msg_05", false)),
                ("ses_b".to_string(), bound("msg_04", true)),
            ])
        );
        assert!(Turns::default().acked().is_empty());
    }

    #[tokio::test]
    async fn the_welcome_acks_each_root_from_the_turns_in_the_log() {
        let db = fresh_database();
        let before = daemon_on(&db);
        let (uid, _registration) = register(&before).await;
        let session = SessionKey::new(uid.clone(), "cc-1");
        let fact = |kind: EventKind, key: &str, turn: &str, payload: Value| {
            PendingEvent::new(&session, kind, payload, Source::Opencode)
                .with_source_event_id(key.to_string())
                .with_turn_id(Some(turn.to_string()))
        };
        for pending in [
            fact(
                EventKind::UserMessage,
                "ses_a:user:msg_01",
                "msg_01",
                json!({}),
            ),
            fact(
                EventKind::TurnComplete,
                "ses_a:turn:msg_01",
                "msg_01",
                json!({}),
            ),
            fact(
                EventKind::UserMessage,
                "ses_a:user:msg_03",
                "msg_03",
                json!({}),
            ),
            // A subagent's fact names its own session, which is not a root.
            fact(
                EventKind::AgentMessage,
                "ses_c:text:prt_04",
                "msg_03",
                json!({"subagent_session": "ses_c"}),
            ),
            fact(
                EventKind::UserMessage,
                "ses_b:user:msg_02",
                "msg_02",
                json!({}),
            ),
        ] {
            before.ingest(pending).await.unwrap();
        }
        // A daemon that starts holding the run finds them in the log.
        let daemon = daemon_on(&db);
        try_register(&daemon, opencode_frame(&uid, Some(NONCE)))
            .await
            .unwrap();
        let hello = hello_for(4242, 1_700_000_000, 1);
        let mut plugin = Plugin::dial(&daemon, honest(&hello), hello, Some(4242)).await;
        let (_, acked) = plugin.welcome().await;
        assert_eq!(
            serde_json::to_value(&acked).unwrap(),
            // Whose model ses_a last used is not in the log: its newest
            // closed turn is read again, to learn it.
            json!({
                "ses_a": {"from": "msg_01", "inclusive": true},
                "ses_b": {"from": "msg_02", "inclusive": true},
            })
        );
    }

    /// What the backstop writes, and when it writes nothing, is all decided
    /// here; the timer that calls it is tested on its own.
    #[tokio::test]
    async fn a_plugin_that_never_dials_is_reported_and_one_that_did_is_not() {
        let silent = daemon();
        let (uid, _registration) = register(&silent).await;
        silent.opencode_backstop(&uid, NONCE).await;
        assert_eq!(
            link_rows(&silent, &uid),
            [(
                format!("link:{uid}:never"),
                json!({"link": "detached", "reason": NEVER})
            )]
        );

        // Linked once: nothing to say, even after the link is gone.
        let linked = daemon();
        let (uid, _registration) = register(&linked).await;
        let (plugin, _) = admitted(&linked, hello_for(4242, 1_700_000_000, 1)).await;
        plugin.hang_up().await;
        linked.opencode_backstop(&uid, NONCE).await;
        assert!(link_rows(&linked, &uid)
            .iter()
            .all(|(key, _)| !key.ends_with(":never")));

        // Ended: nothing either.
        let ended = daemon();
        let (uid, registration) = register(&ended).await;
        ended
            .session_exited("cc-1", Some(&uid), Some(0), None, Some(&registration))
            .await;
        ended.opencode_backstop(&uid, NONCE).await;
        assert!(link_rows(&ended, &uid).is_empty());
    }

    #[tokio::test]
    async fn the_head_names_the_session_the_keyboard_shows_and_another_folder_ends_the_link() {
        let daemon = daemon();
        let (uid, _registration) = register(&daemon).await;
        assert_eq!(head(&daemon, &uid).await, None);
        let (mut plugin, link) = admitted(&daemon, hello_for(4242, 1_700_000_000, 1)).await;

        plugin
            .send(
                &json!({"t": "head", "seq": 1, "route": "session", "session_id": "ses_a",
                "directory": null}),
            )
            .await;
        until(|| async { head(&daemon, &uid).await.as_deref() == Some("ses_a") }).await;
        plugin
            .send(&json!({"t": "head", "seq": 2, "route": "home"}))
            .await;
        until(|| async { head(&daemon, &uid).await.is_none() }).await;
        plugin
            .send(
                &json!({"t": "head", "seq": 3, "route": "session", "session_id": "ses_b",
                "directory": FOLDER}),
            )
            .await;
        until(|| async { head(&daemon, &uid).await.as_deref() == Some("ses_b") }).await;

        plugin
            .send(
                &json!({"t": "head", "seq": 4, "route": "session", "session_id": "ses_x",
                "directory": "/Users/ada/elsewhere"}),
            )
            .await;
        match plugin.reply().await {
            Some(DaemonFrame::OpencodeRefused { reason, r#final }) => {
                assert_eq!(reason, ELSEWHERE);
                assert!(r#final);
            }
            other => panic!("expected the link to be refused, got {other:?}"),
        }
        assert!(plugin.reply().await.is_none(), "and closed");
        assert_eq!(head(&daemon, &uid).await, None);
        assert_eq!(
            link_rows(&daemon, &uid).last().unwrap(),
            &(
                format!("link:{link}:detached"),
                json!({"link": "detached", "reason": ELSEWHERE})
            )
        );
    }

    #[tokio::test]
    async fn a_line_over_the_cap_ends_the_link_and_a_cut_frame_is_not_read() {
        let daemon = daemon();
        let (uid, _registration) = register(&daemon).await;
        let (mut plugin, link) = admitted(&daemon, hello_for(4242, 1_700_000_000, 1)).await;
        // The daemon may hang up before the last byte is written.
        let _ = plugin
            .write
            .write_all(&vec![b'x'; LINK_LINE_BYTES + 2])
            .await;
        assert!(plugin.reply().await.is_none(), "the link is closed");
        assert_eq!(
            link_rows(&daemon, &uid).last().unwrap().0,
            format!("link:{link}:detached")
        );

        let (mut plugin, _) = admitted(&daemon, hello_for(4242, 1_700_000_000, 2)).await;
        let cut = json!({"t": "head", "seq": 1, "route": "session", "session_id": "ses_cut",
            "directory": FOLDER})
        .to_string();
        plugin.write.write_all(cut.as_bytes()).await.unwrap();
        plugin.hang_up().await;
        assert_eq!(head(&daemon, &uid).await, None);
        let heads = daemon
            .store
            .events_after(&uid, 0, 1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.payload.to_string().contains("ses_cut"))
            .count();
        assert_eq!(heads, 0);
    }

    #[tokio::test]
    async fn the_end_of_the_run_closes_its_link_without_a_row_and_retires_its_nonce() {
        let daemon = daemon();
        let (uid, registration) = register(&daemon).await;
        let (mut plugin, link) = admitted(&daemon, hello_for(4242, 1_700_000_000, 1)).await;
        daemon
            .session_exited("cc-1", Some(&uid), Some(0), None, Some(&registration))
            .await;
        assert!(plugin.reply().await.is_none(), "the link is closed");
        plugin.hang_up().await;
        let rows = link_rows(&daemon, &uid);
        assert_eq!(
            rows,
            [(
                format!("link:{link}:attached"),
                json!({"link": "attached", "reason": ATTACHED})
            )]
        );
        assert!(daemon.opencode_candidate(NONCE).await.is_none());
        let again = hello_for(4242, 1_700_000_000, 2);
        let mut late = Plugin::dial(&daemon, honest(&again), again, Some(4242)).await;
        assert!(matches!(
            late.reply().await,
            Some(DaemonFrame::OpencodeRefused { r#final: false, .. })
        ));
    }

    #[tokio::test]
    async fn the_nonce_belongs_to_an_opencode_registration_alone() {
        let daemon = daemon();
        let uid = || protocol::uid::new().unwrap();
        let refused = |result: anyhow::Result<crate::state::Registration>| {
            result.expect_err("refused").to_string()
        };

        let claude = protocol::ipc::RegisterSession {
            agent: AgentKind::Claude,
            agent_bin: None,
            ..opencode_frame(&uid(), Some(NONCE))
        };
        assert!(refused(try_register(&daemon, claude).await).contains("carried an OpenCode nonce"));
        let codex_fields = protocol::ipc::RegisterSession {
            codex_generation: Some(1),
            ..opencode_frame(&uid(), Some(NONCE))
        };
        assert!(refused(try_register(&daemon, codex_fields).await).contains("Codex identity"));
        for malformed in ["0123", "0123456789ABCDEF0123456789ABCDEF", &"g".repeat(32)] {
            let frame = opencode_frame(&uid(), Some(malformed));
            assert!(
                refused(try_register(&daemon, frame).await).contains("32 lowercase hex"),
                "{malformed}"
            );
        }
        assert!(
            daemon.supported_agents().contains(&AgentKind::Opencode),
            "OpenCode is hosted"
        );

        let (first, _registration) = register(&daemon).await;
        let second = uid();
        assert!(
            refused(try_register(&daemon, opencode_frame(&second, Some(NONCE))).await)
                .contains("already names another run")
        );
        assert!(daemon.store.get_session(&second).unwrap().is_none());
        let row = daemon.store.get_session(&first).unwrap().unwrap();
        assert_eq!(row.agent, AgentKind::Opencode, "filed as an OpenCode run");

        // A reconnecting supervisor keeps the run, and its pin.
        let (_plugin, _) = admitted(&daemon, hello_for(4242, 1_700_000_000, 1)).await;
        try_register(&daemon, opencode_frame(&first, Some(NONCE)))
            .await
            .expect("the same launch registers again");
        let other = hello_for(5151, 1_700_000_100, 1);
        let mut intruder = Plugin::dial(&daemon, honest(&other), other, Some(5151)).await;
        assert!(matches!(
            intruder.reply().await,
            Some(DaemonFrame::OpencodeRefused { r#final: true, .. })
        ));
    }

    /// A card the log holds open is cleared by the first snapshot that does not
    /// list it, in the turn it was asked in, even when the daemon that saw it
    /// asked has since restarted.
    #[tokio::test]
    async fn a_card_open_in_the_log_is_cleared_by_a_snapshot_after_a_restart() {
        let db = fresh_database();
        let before = daemon_on(&db);
        let (uid, _registration) = register(&before).await;
        let (mut plugin, _) = admitted(&before, hello_for(4242, 1_700_000_000, 1)).await;
        plugin.send(&prompted(1, "msg_01", "m1")).await;
        plugin
            .send(
                &json!({"t": "ev", "seq": 2, "type": "permission.asked", "properties": {
                "id": "per_01", "sessionID": "ses_a", "permission": "bash",
                "patterns": ["make"], "metadata": {"command": "make"}, "always": [],
                "tool": {"messageID": "msg_01", "callID": "call_1"}}}),
            )
            .await;
        plugin.hang_up().await;
        assert_eq!(
            before.db.opencode_open_cards(uid.clone()).await.unwrap(),
            [(
                "per_01".to_string(),
                "ses_a:perm:per_01".to_string(),
                Some("msg_01".to_string())
            )]
        );

        // A new daemon on the same log; the supervisor registers the run again.
        let daemon = daemon_on(&db);
        try_register(&daemon, opencode_frame(&uid, Some(NONCE)))
            .await
            .unwrap();
        let (mut plugin, _) = admitted(&daemon, hello_for(4242, 1_700_000_000, 2)).await;
        for frame in [
            json!({"t": "sync_begin", "sync": 1, "reason": "trigger:session.idle",
                "scope": "requests", "as_of": 0, "status": {}, "status_ok": true}),
            json!({"t": "sync_end", "sync": 1, "done_at": 0, "permissions_ok": true,
                "questions_ok": true, "requests_ok": true, "lower": {}, "pages": 0,
                "items": 0, "bytes": 0, "activation": 1}),
            json!({"t": "settled", "sync": 1}),
        ] {
            plugin.send(&frame).await;
        }
        plugin.hang_up().await;
        let cleared: Vec<(Option<String>, Value)> = daemon
            .store
            .events_after(&uid, 0, 1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EventKind::ApprovalResolved)
            .map(|e| (e.turn_id, e.payload))
            .collect();
        assert_eq!(
            cleared,
            [(
                Some("msg_01".to_string()),
                json!({"request_id": "per_01", "status": "cleared", "cause": "superseded"})
            )],
            "cleared in the turn it was asked in"
        );
        assert!(daemon.db.opencode_open_cards(uid).await.unwrap().is_empty());
    }

    /// A known frame that does not decode is dropped and repaired by a fresh
    /// snapshot, asked for once however many such frames follow, and asked for
    /// again only after a `settled`.
    #[tokio::test]
    async fn an_unreadable_frame_asks_for_one_snapshot_until_the_next_settled() {
        let daemon = daemon();
        let (uid, _registration) = register(&daemon).await;
        let (mut plugin, _) = admitted(&daemon, hello_for(4242, 1_700_000_000, 1)).await;
        let bad = json!({"t": "ev", "seq": "7", "type": "session.idle", "properties": {}});
        for _ in 0..3 {
            plugin.send(&bad).await;
        }
        assert!(matches!(
            plugin.reply().await,
            Some(DaemonFrame::OpencodeResync { .. })
        ));
        // Every frame before this one has been read once its head lands.
        plugin
            .send(
                &json!({"t": "head", "seq": 1, "route": "session", "session_id": "ses_a",
                "directory": FOLDER}),
            )
            .await;
        until(|| async { head(&daemon, &uid).await.as_deref() == Some("ses_a") }).await;
        assert!(
            plugin.replies.try_recv().is_err(),
            "one snapshot asked for, not one per frame"
        );

        plugin.send(&json!({"t": "settled", "sync": 1})).await;
        plugin.send(&bad).await;
        assert!(matches!(
            plugin.reply().await,
            Some(DaemonFrame::OpencodeResync { .. })
        ));
        plugin.hang_up().await;
    }

    /// The contract file, end to end: the plugin's frames over a socket, the
    /// daemon's replies, and what the run's log holds afterwards.
    #[tokio::test]
    async fn the_contract_run_over_a_socket_lands_in_the_log() {
        let daemon = daemon();
        let (uid, _registration) = register(&daemon).await;
        let frames: Vec<Value> = rows()
            .into_iter()
            .filter(|row| row.dir == "plugin" && row.case.is_none())
            .map(|row| row.frame)
            .collect();
        let ClientFrame::OpencodeHello(hello) = serde_json::from_value(frames[0].clone()).unwrap()
        else {
            panic!("the contract opens with a hello");
        };
        assert_eq!(hello.directory.as_deref(), Some(FOLDER));
        let pid = hello.pid;
        let mut plugin = Plugin::dial(&daemon, honest(&hello), hello, Some(pid)).await;
        let (link, acked) = plugin.welcome().await;
        assert!(acked.is_empty(), "nothing in the log yet");
        for frame in &frames[1..] {
            plugin.send(frame).await;
        }
        drop(plugin.write);
        plugin.served.await.unwrap();
        let mut replies = Vec::new();
        while let Ok(frame) = plugin.replies.try_recv() {
            replies.push(frame);
        }
        assert!(
            replies
                .iter()
                .all(|frame| matches!(frame, DaemonFrame::OpencodeResync { .. })),
            "{replies:?}"
        );

        let events = daemon.store.events_after(&uid, 0, 1000).unwrap();
        let card = |id: &str| -> Vec<Value> {
            events
                .iter()
                .filter(|e| e.item_id.as_deref() == Some(id))
                .map(|e| json!([e.kind.as_str(), e.payload["status"], e.payload["cause"]]))
                .collect()
        };
        assert_eq!(
            card("per_0f00a00000001xxxxxxxxxxxxx"),
            [
                json!(["approval_request", null, null]),
                json!(["approval_resolved", "answered", null])
            ]
        );
        assert_eq!(
            card("que_0f00b00000001xxxxxxxxxxxxx"),
            [json!(["approval_request", null, null])]
        );
        assert_eq!(
            card("que_0f00b00000002xxxxxxxxxxxxx"),
            [
                json!(["approval_request", null, null]),
                json!(["approval_resolved", "cleared", "superseded"])
            ]
        );
        assert_eq!(
            link_rows(&daemon, &uid),
            [
                (
                    format!("link:{link}:attached"),
                    json!({"link": "attached", "reason": ATTACHED})
                ),
                (
                    format!("link:{link}:detached"),
                    json!({"link": "detached", "reason": LOST})
                ),
            ]
        );
        // Shown, never answerable: no card reaches the pending store or a summary.
        let summary = daemon
            .sessions()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.session_uid == uid)
            .unwrap();
        assert!(summary.blocked_on.is_empty());
        assert_eq!(summary.agent, AgentKind::Opencode);
    }

    // ------------------------------------------------------------ agent.json

    fn run_dir() -> PathBuf {
        let dir = fresh_database().with_extension("run");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path that outlives the call.
        let made = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo {path:?}");
    }

    const RECORD: &str = r#"{"pid":4242,"start":{"sec":1700000000,"usec":7}}"#;
    const RECORDED: (i32, OpencodeStart) = (
        4242,
        OpencodeStart {
            sec: 1_700_000_000,
            usec: 7,
        },
    );

    /// `Kernel::recorded`, on a thread of its own that must answer in time.
    fn recorded_in_time(dir: &Path) -> Option<(i32, OpencodeStart)> {
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = dir.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(Kernel.recorded(&dir));
        });
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .expect("reading agent.json never waits")
    }

    #[test]
    fn agent_json_is_read_only_as_a_small_regular_file_and_never_waits() {
        let dir = run_dir();
        let path = dir.join("agent.json");
        std::fs::write(&path, RECORD).unwrap();
        assert_eq!(recorded_in_time(&dir), Some(RECORDED));

        std::fs::remove_file(&path).unwrap();
        fifo(&path);
        assert_eq!(recorded_in_time(&dir), None, "a FIFO");

        let elsewhere = run_dir();
        fifo(&elsewhere.join("pipe"));
        std::fs::write(elsewhere.join("agent.json"), RECORD).unwrap();
        for target in ["pipe", "agent.json"] {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(elsewhere.join(target), &path).unwrap();
            assert_eq!(recorded_in_time(&dir), None, "a link to {target}");
        }

        std::fs::remove_file(&path).unwrap();
        let mut padded = RECORD.as_bytes().to_vec();
        padded.resize(4096, b' ');
        std::fs::write(&path, &padded).unwrap();
        assert_eq!(recorded_in_time(&dir), Some(RECORDED), "4 KiB is the most");
        padded.push(b' ');
        std::fs::write(&path, &padded).unwrap();
        assert_eq!(recorded_in_time(&dir), None, "over 4 KiB");
    }

    /// The file swapped for a FIFO between any two steps of the read, over and
    /// over: every read answers, with the record or with nothing.
    #[test]
    fn agent_json_swapped_for_a_fifo_under_the_reader_never_holds_it() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = run_dir();
        let path = dir.join("agent.json");
        std::fs::write(&path, RECORD).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let swapper = {
            let (dir, path, stop) = (dir.clone(), path.clone(), Arc::clone(&stop));
            std::thread::spawn(move || {
                let (file, pipe) = (dir.join("file.tmp"), dir.join("pipe.tmp"));
                while !stop.load(Ordering::Relaxed) {
                    std::fs::write(&file, RECORD).unwrap();
                    std::fs::rename(&file, &path).unwrap();
                    fifo(&pipe);
                    std::fs::rename(&pipe, &path).unwrap();
                }
            })
        };
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                let mut reads = 0u64;
                while std::time::Instant::now() < deadline {
                    let read = Kernel.recorded(&dir);
                    assert!(read.is_none() || read == Some(RECORDED), "{read:?}");
                    reads += 1;
                }
                let _ = tx.send(reads);
            });
        }
        let finished = rx.recv_timeout(std::time::Duration::from_secs(10));
        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();
        if finished.is_err() {
            // A reader held in a FIFO's open is let go by a writer.
            use std::os::unix::fs::OpenOptionsExt;
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path);
        }
        let reads = finished.expect("a read was held by a FIFO");
        assert!(reads > 100, "{reads} reads");
    }

    // ------------------------------------------------------- registration

    /// How long a test daemon waits for a plugin before its backstop speaks.
    const SHORT: std::time::Duration = std::time::Duration::from_millis(250);

    async fn hurried() -> Arc<Daemon> {
        let daemon = daemon();
        daemon.set_opencode_backstop_wait(SHORT).await;
        daemon
    }

    /// A registration of a live run under another nonce is not that run's
    /// launch. It is refused, and the run keeps its nonce, its pin and its
    /// link.
    #[tokio::test]
    async fn a_live_run_registered_again_under_another_nonce_keeps_its_link() {
        let daemon = hurried().await;
        let (uid, _registration) = register(&daemon).await;
        let (mut plugin, link) = admitted(&daemon, hello_for(4242, 1_700_000_000, 1)).await;
        let other = "fedcba9876543210fedcba9876543210";
        let again = try_register(&daemon, opencode_frame(&uid, Some(other))).await;

        assert!(
            daemon.opencode_link_is_current(&uid, &link).await,
            "the live link is still the run's"
        );
        assert!(daemon.opencode_candidate(other).await.is_none());
        tokio::time::sleep(SHORT * 2).await;
        assert!(
            link_rows(&daemon, &uid)
                .iter()
                .all(|(key, _)| !key.ends_with(":never")),
            "{:?}",
            link_rows(&daemon, &uid)
        );
        let (_redial, _) = admitted(&daemon, hello_for(4242, 1_700_000_000, 2)).await;
        assert!(plugin.reply().await.is_none(), "taken over by the redial");
        let refused = again.expect_err("refused").to_string();
        assert!(refused.contains("another plugin nonce"), "{refused}");
    }

    /// The same launch registering again — a supervisor reconnecting — leaves
    /// the run exactly as it was.
    #[tokio::test]
    async fn the_same_launch_registered_again_keeps_its_link_and_its_pin() {
        let daemon = hurried().await;
        let (uid, _registration) = register(&daemon).await;
        let (mut plugin, link) = admitted(&daemon, hello_for(4242, 1_700_000_000, 1)).await;
        try_register(&daemon, opencode_frame(&uid, Some(NONCE)))
            .await
            .expect("the same launch registers again");
        assert!(daemon.opencode_link_is_current(&uid, &link).await);
        tokio::time::sleep(SHORT * 2).await;
        assert!(link_rows(&daemon, &uid)
            .iter()
            .all(|(key, _)| !key.ends_with(":never")));
        plugin
            .send(
                &json!({"t": "head", "seq": 1, "route": "session", "session_id": "ses_a",
                "directory": FOLDER}),
            )
            .await;
        until(|| async { head(&daemon, &uid).await.as_deref() == Some("ses_a") }).await;
        let other = hello_for(5151, 1_700_000_100, 1);
        let mut intruder = Plugin::dial(&daemon, honest(&other), other, Some(5151)).await;
        assert!(matches!(
            intruder.reply().await,
            Some(DaemonFrame::OpencodeRefused { r#final: true, .. })
        ));
    }

    /// Two runs registering with one nonce at once: one of them holds it, and
    /// the daemon finds that run, and only it, under the nonce.
    #[tokio::test]
    async fn one_nonce_names_one_run_however_its_registrations_race() {
        for _ in 0..20 {
            let daemon = daemon();
            let (a, b) = (protocol::uid::new().unwrap(), protocol::uid::new().unwrap());
            let _ = tokio::join!(
                try_register(&daemon, opencode_frame(&a, Some(NONCE))),
                try_register(&daemon, opencode_frame(&b, Some(NONCE))),
            );
            let (nonces, runs) = daemon.opencode_nonce_table().await;
            let holders: Vec<&String> = runs
                .iter()
                .filter(|(_, nonce)| nonce.as_str() == NONCE)
                .map(|(uid, _)| uid)
                .collect();
            assert_eq!(holders.len(), 1, "{runs:?}");
            assert_eq!(nonces.get(NONCE), Some(holders[0]), "{nonces:?}");
        }
    }

    // ------------------------------------------------------- the backstop

    #[tokio::test]
    async fn the_backstop_speaks_once_its_wait_passes_without_an_admission() {
        assert_eq!(
            daemon().opencode_backstop_wait().await,
            std::time::Duration::from_secs(15),
            "the wait a daemon starts with"
        );
        let silent = hurried().await;
        let (uid, _registration) = register(&silent).await;
        assert!(link_rows(&silent, &uid).is_empty());
        until(|| async { !link_rows(&silent, &uid).is_empty() }).await;
        assert_eq!(
            link_rows(&silent, &uid),
            [(
                format!("link:{uid}:never"),
                json!({"link": "detached", "reason": NEVER})
            )]
        );

        let linked = hurried().await;
        let (uid, _registration) = register(&linked).await;
        let (_plugin, _) = admitted(&linked, hello_for(4242, 1_700_000_000, 1)).await;
        tokio::time::sleep(SHORT * 2).await;
        assert!(link_rows(&linked, &uid)
            .iter()
            .all(|(key, _)| !key.ends_with(":never")));
    }

    // ------------------------------------------------------- restarts

    fn prompted(seq: u64, message: &str, model: &str) -> Value {
        json!({"t": "ev", "seq": seq, "type": "message.updated", "properties": {
            "sessionID": "ses_a", "info": {"id": message, "sessionID": "ses_a",
            "role": "user", "agent": "build", "time": {"created": 1},
            "model": {"providerID": "mock", "modelID": model}}}})
    }

    fn went_idle(seq: u64) -> Value {
        json!({"t": "ev", "seq": seq, "type": "session.idle",
            "properties": {"sessionID": "ses_a"}})
    }

    /// The keyboard's choice is compared with the one before it even when the
    /// daemon restarted in between.
    #[tokio::test]
    async fn a_model_change_after_a_restart_is_said_against_the_choice_before_it() {
        let db = fresh_database();
        let before = daemon_on(&db);
        let (uid, _registration) = register(&before).await;
        let (mut plugin, _) = admitted(&before, hello_for(4242, 1_700_000_000, 1)).await;
        for frame in [
            prompted(1, "msg_01", "m1"),
            went_idle(2),
            prompted(3, "msg_02", "m2"),
            went_idle(4),
            prompted(5, "msg_03", "m3"),
            went_idle(6),
        ] {
            plugin.send(&frame).await;
        }
        plugin.hang_up().await;

        let daemon = daemon_on(&db);
        try_register(&daemon, opencode_frame(&uid, Some(NONCE)))
            .await
            .unwrap();
        let (mut plugin, _) = admitted(&daemon, hello_for(4242, 1_700_000_000, 2)).await;
        plugin.send(&prompted(1, "msg_04", "m2")).await;
        plugin.hang_up().await;
        let notices: Vec<(String, Value)> = daemon
            .store
            .events_after(&uid, 0, 1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EventKind::Notification)
            .map(|e| {
                (
                    e.source_event_id.unwrap_or_default(),
                    e.payload["message"].clone(),
                )
            })
            .collect();
        assert_eq!(
            notices,
            [
                ("ses_a:model:msg_02".to_string(), json!("build · mock/m2")),
                ("ses_a:model:msg_03".to_string(), json!("build · mock/m3")),
                ("ses_a:model:msg_04".to_string(), json!("build · mock/m2")),
            ]
        );
    }

    /// A check that outlasts its budget is refused while the plugin still
    /// waits for the answer, and the plugin dials again.
    #[tokio::test]
    async fn a_check_that_outlasts_its_budget_is_refused_and_dialled_again() {
        let daemon = daemon();
        let (uid, _registration) = register(&daemon).await;
        let hello = hello_for(4242, 1_700_000_000, 1);
        let slow = Fake {
            wait: VERIFY_BUDGET + std::time::Duration::from_secs(1),
            ..honest(&hello)
        };
        let asked = std::time::Instant::now();
        let mut plugin = Plugin::dial(&daemon, slow, hello, Some(4242)).await;
        match plugin.reply().await {
            Some(DaemonFrame::OpencodeRefused { reason, r#final }) => {
                assert_eq!(
                    reason,
                    "the OpenCode process could not be checked within 2s"
                );
                assert!(!r#final);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(asked.elapsed() < std::time::Duration::from_secs(3));
        assert!(link_rows(&daemon, &uid).is_empty());
    }

    /// What the welcome and each resync acknowledge follows the facts as they
    /// are recorded, and a restarted daemon reads the same turns from the log.
    /// Not knowing the model each root last used, it acks their closed turns
    /// inclusive.
    #[tokio::test]
    async fn every_resync_and_a_restart_ack_the_turns_recorded_so_far() {
        let db = fresh_database();
        let before = daemon_on(&db);
        let (uid, _registration) = register(&before).await;
        let (mut plugin, _) = admitted(&before, hello_for(4242, 1_700_000_000, 1)).await;
        let prompt = |seq: u64, session: &str, message: &str| {
            [
                json!({"t": "ev", "seq": seq, "type": "message.updated", "properties": {
                    "sessionID": session, "info": {"id": message, "sessionID": session,
                    "role": "user", "time": {"created": 1}, "agent": "build",
                    "model": {"providerID": "mock", "modelID": "m1"}}}}),
                json!({"t": "ev", "seq": seq + 1, "type": "message.part.updated",
                    "properties": {"sessionID": session, "part": {
                    "id": format!("prt_{message}"), "sessionID": session,
                    "messageID": message, "type": "text", "text": "go"}}}),
            ]
        };
        let idle = |seq: u64, session: &str| {
            json!({"t": "ev", "seq": seq, "type": "session.idle",
                "properties": {"sessionID": session}})
        };
        let bad = json!({"t": "ev", "seq": "x", "type": "session.idle", "properties": {}});
        let bound = |from: &str, inclusive: bool| json!({"from": from, "inclusive": inclusive});
        let steps: [(Vec<Value>, Value); 3] = [
            (
                [
                    &prompt(1, "ses_a", "msg_01")[..],
                    &[idle(3, "ses_a")],
                    &prompt(4, "ses_b", "msg_02"),
                ]
                .concat(),
                json!({"ses_a": bound("msg_01", false), "ses_b": bound("msg_02", true)}),
            ),
            (
                [&[idle(6, "ses_b")][..], &prompt(7, "ses_a", "msg_03")].concat(),
                json!({"ses_a": bound("msg_01", false), "ses_b": bound("msg_02", false)}),
            ),
            (
                vec![idle(9, "ses_a")],
                json!({"ses_a": bound("msg_03", false), "ses_b": bound("msg_02", false)}),
            ),
        ];
        let mut last = Value::Null;
        for (sync, (frames, want)) in (1..).zip(steps) {
            for frame in frames.iter().chain([&bad]) {
                plugin.send(frame).await;
            }
            match plugin.reply().await {
                Some(DaemonFrame::OpencodeResync { acked }) => {
                    assert_eq!(serde_json::to_value(&acked).unwrap(), want, "step {sync}");
                }
                other => panic!("expected a resync, got {other:?}"),
            }
            plugin.send(&json!({"t": "settled", "sync": sync})).await;
            last = want;
        }
        plugin.hang_up().await;

        let daemon = daemon_on(&db);
        try_register(&daemon, opencode_frame(&uid, Some(NONCE)))
            .await
            .unwrap();
        let hello = hello_for(4242, 1_700_000_000, 2);
        let mut plugin = Plugin::dial(&daemon, honest(&hello), hello, Some(4242)).await;
        let (_, acked) = plugin.welcome().await;
        for bound in last.as_object_mut().unwrap().values_mut() {
            bound["inclusive"] = json!(true);
        }
        assert_eq!(serde_json::to_value(&acked).unwrap(), last);
    }

    /// A run that prompted root `ses_a` once, on `mock/m1`, and whose daemon
    /// then restarted: the new daemon, the run, and its plugin past the welcome.
    async fn restarted_after_one_turn_on_m1() -> (Arc<Daemon>, String, Plugin) {
        let session = json!({"id": "ses_a", "directory": FOLDER, "time": {"created": 0}});
        let user = prompted(0, "msg_01", "m1")["properties"]["info"].clone();
        let part = json!({"id": "prt_01", "sessionID": "ses_a", "messageID": "msg_01",
            "type": "text", "text": "go"});
        let reply = json!({"id": "msg_02", "sessionID": "ses_a", "role": "assistant",
            "parentID": "msg_01", "finish": "stop", "time": {"created": 2, "completed": 3}});
        let db = fresh_database();
        let before = daemon_on(&db);
        let (uid, _registration) = register(&before).await;
        let (mut plugin, _) = admitted(&before, hello_for(4242, 1_700_000_000, 1)).await;
        for frame in [
            json!({"t": "ev", "seq": 1, "type": "session.created",
                "properties": {"info": session}}),
            json!({"t": "ev", "seq": 2, "type": "message.updated",
                "properties": {"sessionID": "ses_a", "info": user}}),
            json!({"t": "ev", "seq": 3, "type": "message.part.updated",
                "properties": {"sessionID": "ses_a", "part": part}}),
            json!({"t": "ev", "seq": 4, "type": "message.updated",
                "properties": {"sessionID": "ses_a", "info": reply}}),
            went_idle(5),
        ] {
            plugin.send(&frame).await;
        }
        plugin.hang_up().await;
        let facts = |daemon: &Daemon| -> Vec<Value> {
            daemon
                .store
                .events_after(&uid, 0, 1000)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind != EventKind::LinkState)
                .map(|e| json!([e.kind.as_str(), e.source_event_id, e.turn_id, e.payload]))
                .collect()
        };
        let logged = facts(&before);
        assert!(
            logged.iter().any(|f| f[1] == "ses_a:turn:msg_01"),
            "{logged:?}"
        );

        let daemon = daemon_on(&db);
        try_register(&daemon, opencode_frame(&uid, Some(NONCE)))
            .await
            .unwrap();
        let hello = hello_for(4242, 1_700_000_000, 2);
        let mut plugin = Plugin::dial(&daemon, honest(&hello), hello, Some(4242)).await;
        let (_, acked) = plugin.welcome().await;
        assert_eq!(
            serde_json::to_value(&acked).unwrap(),
            json!({"ses_a": {"from": "msg_01", "inclusive": true}}),
            "the closed turn is read again: its model is not in the log"
        );
        // What the plugin sends for that bound: the root and its closed turn.
        for frame in [
            json!({"t": "sync_begin", "sync": 1, "reason": "connect", "scope": "full",
                "as_of": 0, "status": {}, "status_ok": true}),
            json!({"t": "sync_page", "sync": 1, "n": 0, "items": [
                {"session": session},
                {"sessionID": "ses_a", "info": user, "parts": [part]},
                {"sessionID": "ses_a", "info": reply, "parts": []}]}),
            json!({"t": "sync_end", "sync": 1, "done_at": 0, "list_ok": true,
                "permissions_ok": true, "questions_ok": true, "requests_ok": true,
                "lower": {}, "pages": 1, "items": 3, "bytes": 1, "activation": 2}),
            json!({"t": "settled", "sync": 1}),
            json!({"t": "ev", "seq": "x", "type": "session.idle", "properties": {}}),
        ] {
            plugin.send(&frame).await;
        }
        match plugin.reply().await {
            Some(DaemonFrame::OpencodeResync { acked }) => assert_eq!(
                serde_json::to_value(&acked).unwrap(),
                json!({"ses_a": {"from": "msg_01", "inclusive": false}}),
                "the model is known again"
            ),
            other => panic!("expected a resync, got {other:?}"),
        }
        plugin.send(&json!({"t": "settled", "sync": 2})).await;
        assert_eq!(
            facts(&daemon),
            logged,
            "the turn read again changes no fact"
        );
        (daemon, uid, plugin)
    }

    fn model_notices(daemon: &Daemon, uid: &str) -> Vec<(String, Value)> {
        daemon
            .store
            .events_after(uid, 0, 1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EventKind::Notification)
            .map(|e| {
                (
                    e.source_event_id.unwrap_or_default(),
                    e.payload["message"].clone(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn a_first_model_change_after_a_restart_is_said() {
        let (daemon, uid, mut plugin) = restarted_after_one_turn_on_m1().await;
        plugin.send(&prompted(1, "msg_03", "m2")).await;
        plugin.hang_up().await;
        assert_eq!(
            model_notices(&daemon, &uid),
            [("ses_a:model:msg_03".to_string(), json!("build · mock/m2"))]
        );
    }

    #[tokio::test]
    async fn the_same_model_after_a_restart_is_not_said() {
        let (daemon, uid, mut plugin) = restarted_after_one_turn_on_m1().await;
        plugin.send(&prompted(1, "msg_03", "m1")).await;
        plugin.hang_up().await;
        assert_eq!(model_notices(&daemon, &uid), []);
    }
}
