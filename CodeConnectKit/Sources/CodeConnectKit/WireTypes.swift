import Foundation

/// Swift mirror of `mac/protocol/src/{ws,event,ipc}.rs`.
///
/// Hand-written `Codable` conformances throughout, for two reasons:
///   * the Rust enums are internally tagged (`type` / `status` / `mode`), which
///     Swift's synthesised conformance cannot express; and
///   * `JSONDecoder.keyDecodingStrategy` also rewrites the keys of
///     `[String: JSONValue]`, which would silently mangle every event payload —
///     so every key is spelled out instead.
public enum Wire {
    public static let protocolVersion: UInt32 = 2
    /// Matches `MAX_CLIENT_MESSAGE_BYTES`; the daemon closes anything larger.
    public static let maxClientMessageBytes = 1024 * 1024
    /// `MAX_COMPOSE_BYTES` (`ws.rs`), enforced at the Mac on `text.len()` —
    /// **bytes, not characters**. Mirrored here so the composer can refuse
    /// before sending rather than learning it from a round trip.
    public static let maxComposeBytes = 8192
}

// MARK: - Scalars

/// How this connection's daemon delivers push, normalized from the wire's two
/// raw flags. Computed once per handshake by `Capabilities.pushMode` and read
/// everywhere a push decision is made, so `push` and `pushRelay` are never
/// consulted directly outside that one property.
///
/// - `direct`: the daemon holds its own APNs key and sends straight to Apple; a
///   plain token registration is all it needs, and the relay is never contacted.
/// - `relay`: the daemon sends through the CodeConnect relay, which requires an
///   App Attest-enrolled credential bound to this phone's `(token, environment)`.
/// - `none`: this connection does not send push (off, or a bootstrap/static
///   connection that has no device row).
public enum PushMode: Equatable, Sendable {
    case none
    case direct
    case relay
}

/// Where an event came from — a *provenance*, so an unrecognised source must
/// never inherit the trusted `.daemon` label. `.daemon` used to be the
/// fallback, which meant a newer daemon's source this build had never heard of
/// was silently attributed to the daemon itself. Modelled on `AnswerPath`: a
/// tagged single-value string with the unknown retained in its own case.
public enum EventSource: Sendable, Hashable {
    case hook, transcript, daemon, pty
    /// The Codex control link: every timeline event the adapter emits for a
    /// Codex session carries it (`event.rs`). Named rather than left to
    /// `.unknown` because provenance is read on screen — an unnamed source is
    /// rendered as one this build cannot account for, which a Codex session's
    /// entire timeline is not.
    case codex
    /// A Claude run driven through the Agent SDK: the messages its agent host
    /// forwards (`Source::Sdk`, `event.rs`). Named for the same reason as `.codex`.
    case sdk
    /// A source a newer daemon knows about and this build does not. Retained
    /// rather than coerced, so nothing reads it as the trusted `.daemon`.
    case unknown(String)

    var rawValue: String {
        switch self {
        case .hook: return "hook"
        case .transcript: return "transcript"
        case .daemon: return "daemon"
        case .pty: return "pty"
        case .codex: return "codex"
        case .sdk: return "sdk"
        case .unknown(let raw): return raw
        }
    }
}

/// The `notification_type` values of the notifications the engine files itself
/// (`protocol::event::notice`), beside the ones Claude's own notifications carry.
public enum EngineNotice {
    /// `/clear` reset the conversation; the run's Claude session id changes.
    public static let conversationCleared = "conversation_cleared"
    /// The content-free line that wakes an idle agent to read its mail.
    public static let mailNudge = "mail_nudge"
    /// An effort an app set on a hosted run, which claude took (`{effort}`): claude echoes nothing for it.
    public static let modelSet = "model_set"
    /// The agent host confirmed a permission-mode change.
    public static let modeSet = "mode_set"
    /// Claude compacted the conversation (`/compact`, or on its own).
    public static let compacted = "compacted"
}

/// The two lines ccd sends to wake an idle Codex agent for its mail
/// (`protocol::ipc::MAIL_NUDGE_ACT` / `MAIL_NUDGE_SHOW`). Codex's app-server
/// reports each back as the turn's user message, so the timeline draws it as
/// the mail notice — never the person's words.
public enum MailNudge {
    public static let act =
        "You have CodeConnect mail: call the codeconnect read_messages tool, then handle each message as its sender asks."
    public static let show =
        "You have CodeConnect mail: call the codeconnect read_messages tool and show the messages to the person; do not act on them."
}

extension EventSource: Codable {
    public init(from decoder: Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "hook": self = .hook
        case "transcript": self = .transcript
        case "daemon": self = .daemon
        case "pty": self = .pty
        case "codex": self = .codex
        case "sdk": self = .sdk
        case let other: self = .unknown(other)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

public enum Lifecycle: String, Codable, Sendable, Hashable {
    /// `stopping` (minor 35): the run was asked to end and has not ended yet.
    case starting, live, stopping, exited, failed, unknown

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = Lifecycle(rawValue: raw) ?? .unknown
    }
}

/// Freshness of *our observation*, never the agent's state: `detached` means
/// this app has lost sight of the session, not that the agent stopped working.
/// The two must never be conflated on any surface.
public enum Link: String, Codable, Sendable, Hashable {
    case attached, detached

    /// An unrecognised link word reads as detached rather than pretending to be
    /// attached — claiming liveness we cannot justify is the one thing this enum
    /// exists to prevent.
    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = Link(rawValue: raw) ?? .detached
    }
}

public enum ResolvedBy: String, Codable, Sendable, Hashable {
    case phone, timeout
    /// A resolver this build does not name, from a newer daemon or an earlier
    /// one's record. Said so, never read as one of the others.
    case unknown

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = ResolvedBy(rawValue: raw) ?? .unknown
    }
}

/// The kind of paired device an answer came from (`answered_on`, minor 34), by
/// the name its app says hello with. Attribution only: it chooses words.
public enum AnsweredOn: String, Codable, Sendable, Hashable {
    case iphone, mac

    /// The kind of device `host` is.
    init(_ host: HostDevice) {
        self = host == .mac ? .mac : .iphone
    }
}

/// Which agent is running in a session.
///
/// `protocol/src/agent.rs`: `#[derive(Default)] Claude` plus
/// `#[serde(untagged)] Unsupported(String)`. Two consequences the phone must
/// honour exactly, and they pull in opposite directions:
///
///   * an **absent** `agent` is `claude` — every daemon below minor 15 sends
///     none, and every session it describes really is a Claude session;
///   * an unknown *present* string is **retained**, never coerced. Reading
///     `"gemini"` as Claude would offer a Claude session's whole vocabulary —
///     `send_text`, the slash palette, Allow/Deny — to an agent that answers
///     none of it.
public enum AgentKind: Sendable, Hashable {
    case claude
    case codex
    /// An agent a newer daemon hosts and this build does not know how to drive.
    case unsupported(String)

    var rawValue: String {
        switch self {
        case .claude: return "claude"
        case .codex: return "codex"
        case .unsupported(let raw): return raw
        }
    }

    /// What to call it on screen. An unsupported agent is named verbatim: the
    /// daemon's own word is the only true thing the phone can say about it.
    public var displayName: String {
        switch self {
        case .claude: return "Claude"
        case .codex: return "Codex"
        case .unsupported(let raw): return raw
        }
    }

    /// The product a run runs, where a label names it rather than a sentence addressing it: "Claude Code", as the
    /// start menu names it (`StartAgent.displayName`).
    public var productName: String {
        switch self {
        case .claude: return StartAgent.claude.displayName
        case .codex: return StartAgent.codex.displayName
        case .unsupported(let raw): return raw
        }
    }

    init(wire raw: String) {
        switch raw {
        case "claude": self = .claude
        case "codex": self = .codex
        case let other: self = .unsupported(other)
        }
    }
}

extension AgentKind: Codable {
    public init(from decoder: Decoder) throws {
        self.init(wire: try decoder.singleValueContainer().decode(String.self))
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

/// **Whether this Mac can actuate on this Codex session right now** — the one
/// fact `codex_thread_id` could never carry, because a thread id reads the same
/// whether or not the run is being watched.
///
/// **Cross-checked against the Rust**: `event.rs`'s `CodexLink` is
/// `subscribed | bound_not_started | none`, and `#[derive(Default)] None` — so a
/// summary that carries no such field decodes as `none`.
///
/// **The whole rule, in one line: `subscribed` actuates both verbs;
/// `bound_not_started` actuates compose only; everything else neither.** The Mac
/// has proved a `bound_not_started` thread has never run a turn, so a first one
/// can be started on it and there is none to stop. Every other state greys Stop
/// and Compose **with the reason**, and the daemon's own refusal sentence is what
/// the operator reads if they ask anyway.
///
/// **`bound_not_started` is protocol minor 20**; `subscribed` and `none` are minor
/// 19, which build 72 shipped on 2026-09-08. So this decoder reads a strictly
/// newer daemon than that build does, and a daemon that predates the word simply
/// never sends it. The `unknown` arm below is what makes the reverse direction —
/// this build meeting a word a newer daemon adds — safe rather than lucky.
///
/// An unrecognised word is retained rather than read as `subscribed`: this enum
/// gates two mutations, and the only safe direction for a value this build
/// cannot interpret is "not offered".
public enum CodexLinkState: Sendable, Hashable {
    /// Watching this session's thread. The only state that can actuate.
    case subscribed
    /// **Bound to a thread the Mac has PROVED has never run a turn** (protocol
    /// minor 20), and therefore the one un-subscribed state a first message may
    /// be sent into.
    ///
    /// The daemon publishes it only when its own `thread/resume` for this
    /// thread was refused with the measured "no rollout" answer. No rollout
    /// means no turn has ever run, which means none is running, which means a
    /// first turn cannot collide with one.
    ///
    /// It exists because of a dead end: on a fresh Codex session the thread has
    /// no rollout until its first turn, and a composer gated on `subscribed`
    /// could never send the message that would start one.
    ///
    /// **Compose, and only compose.** There is no turn to stop, so Stop is not
    /// offered here — see `blockedReason(for:)`.
    case boundNotStarted
    /// There is no Codex control link for this session — including every
    /// Claude session and every daemon predating the field.
    case none
    /// A state a newer daemon names and this build does not.
    case unknown(String)

    var rawValue: String {
        switch self {
        case .subscribed: return "subscribed"
        case .boundNotStarted: return "bound_not_started"
        case .none: return "none"
        case .unknown(let raw): return raw
        }
    }

    /// **What is being asked of the link.** The two mutations this enum gates,
    /// which stopped having one answer the moment `boundNotStarted` existed:
    /// a first turn can be *started* on a thread that has never run one, and
    /// there is nothing there to *stop*.
    public enum Ask: Sendable, Hashable {
        /// Say something — the daemon decides at the write whether the words
        /// start a turn or join one.
        case compose
        /// Stop the turn this session is running.
        case stop
    }

    /// **The single gate. Nothing else in the app may re-derive it.**
    ///
    /// It takes the ask now, and that is the extension rather than a second
    /// gate: the rule still lives in exactly one type, and a caller is made to
    /// say which mutation it is asking about instead of guessing from a Bool
    /// that no longer has one meaning.
    func canActuate(_ ask: Ask) -> Bool { blockedReason(for: ask) == nil }

    /// Why not, in the phone's own words, or nil when it can.
    ///
    /// Deliberately shorter than the daemon's sentence and deliberately not a
    /// paraphrase of it: this is what a *disabled control* says before anything
    /// has been sent, and the daemon's own sentences are what a *refusal* says
    /// afterwards. Both are shown, in that order, and neither pretends to be
    /// the other.
    public func blockedReason(for ask: Ask) -> String? {
        switch self {
        case .subscribed: return nil
        // **The honest half of the new state.** It can be spoken to, and it
        // cannot be stopped: a thread with no rollout has never run a turn, so
        // there is no turn to name and the daemon would refuse one.
        case .boundNotStarted:
            switch ask {
            case .compose: return nil
            case .stop: return "This Codex run has not started a turn yet."
            }
        case .none: return "There is no live link to this Codex session."
        case .unknown: return "\(HostDevice.current.computer("The computer")) reports a link state this app does not recognise."
        }
    }

    init(wire raw: String) {
        switch raw {
        case "subscribed": self = .subscribed
        case "bound_not_started": self = .boundNotStarted
        case "none": self = .none
        case let other: self = .unknown(other)
        }
    }
}

extension CodexLinkState: Codable {
    public init(from decoder: Decoder) throws {
        self.init(wire: try decoder.singleValueContainer().decode(String.self))
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

/// How an answer was actually applied — an *actuation claim*, so an unknown
/// wire value must never collapse into a known one. Modelled on
/// `AnswerDecision`: a tagged single-value string, with the unknown retained in
/// its own case rather than folded into a known one.
public enum AnswerPath: Sendable, Hashable {
    /// Returned to Claude through a channel that confirms it: the hook Claude holds for the call, or, for a run
    /// started in an agent host, the host's return to the SDK's `canUseTool` request. The value names the
    /// confirmation, not the carrier.
    case hookReturn
    /// The Codex path (minor 16): the answer was written back as the response to
    /// the app-server's own approval request. A **real** actuation, and it has
    /// to be named — without the case every successful Codex answer decoded to
    /// `.unknown`, whose whole contract is that it never claims one, so a
    /// confirmed answer rendered as unvouchable.
    case codexResponse
    /// A path this build does not name — a newer daemon's, or an earlier one's
    /// `send_keys`. Never claims an actuation: this build cannot vouch for how
    /// the answer landed.
    case unknown(String)

    var rawValue: String {
        switch self {
        case .hookReturn: return "hook_return"
        case .codexResponse: return "codex_response"
        case .unknown(let raw): return raw
        }
    }

    /// An unrecognised value is retained, never coerced into a known one.
    init(wire raw: String) {
        switch raw {
        case "hook_return": self = .hookReturn
        case "codex_response": self = .codexResponse
        case let other: self = .unknown(other)
        }
    }
}

extension AnswerPath: Codable {
    public init(from decoder: Decoder) throws {
        self.init(wire: try decoder.singleValueContainer().decode(String.self))
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

/// `EventKind::Other` is load-bearing: an unknown kind from a newer daemon has
/// to survive a round-trip rather than being dropped.
public enum EventKind: Sendable, Hashable {
    case sessionStart, sessionEnd
    /// The end of a turn: the *turn* finished, the session lives on.
    case turnComplete
    case toolCall, toolResult
    case approvalRequest, approvalResolved
    case notification
    case userMessage, agentMessage, reasoning
    case usage, error
    case resync, linkState
    case other(String)

    var rawValue: String {
        switch self {
        case .sessionStart: return "session_start"
        case .sessionEnd: return "session_end"
        case .turnComplete: return "turn_complete"
        case .toolCall: return "tool_call"
        case .toolResult: return "tool_result"
        case .approvalRequest: return "approval_request"
        case .approvalResolved: return "approval_resolved"
        case .notification: return "notification"
        case .userMessage: return "user_message"
        case .agentMessage: return "agent_message"
        case .reasoning: return "reasoning"
        case .usage: return "usage"
        case .error: return "error"
        case .resync: return "resync"
        case .linkState: return "link_state"
        case .other(let raw): return raw
        }
    }

    init(rawValue: String) {
        switch rawValue {
        case "session_start": self = .sessionStart
        case "session_end": self = .sessionEnd
        case "turn_complete": self = .turnComplete
        case "tool_call": self = .toolCall
        case "tool_result": self = .toolResult
        case "approval_request": self = .approvalRequest
        case "approval_resolved": self = .approvalResolved
        case "notification": self = .notification
        case "user_message": self = .userMessage
        case "agent_message": self = .agentMessage
        case "reasoning": self = .reasoning
        case "usage": self = .usage
        case "error": self = .error
        case "resync": self = .resync
        case "link_state": self = .linkState
        default: self = .other(rawValue)
        }
    }
}

extension EventKind: Codable {
    public init(from decoder: Decoder) throws {
        self.init(rawValue: try decoder.singleValueContainer().decode(String.self))
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

// MARK: - Event

public struct Event: Codable, Sendable, Hashable, Identifiable {
    public let seq: UInt64
    /// The run's identity — a ULID minted at spawn and never reused. Empty from
    /// a daemon older than protocol minor 2, which had no such concept.
    let sessionUID: String
    /// The run's name: its uid for a run CodeConnect started, a reused name
    /// (`cc-1`) on an older daemon. Display only: it is not an identity.
    public let sessionID: String
    let ts: String
    public let kind: EventKind
    public let payload: JSONValue
    public let source: EventSource
    public let sourceEventID: String?
    public let turnID: String?
    let itemID: String?

    enum CodingKeys: String, CodingKey {
        case seq
        case sessionUID = "session_uid"
        case sessionID = "session_id"
        case ts
        case kind
        case payload
        case source
        case sourceEventID = "source_event_id"
        case turnID = "turn_id"
        case itemID = "item_id"
    }

    /// Written by hand rather than synthesised because `session_uid` has to
    /// *default* rather than fail: `protocol/src/event.rs` marks it
    /// `#[serde(default)]`, an event logged before uids existed carries none,
    /// and a strict decode here would drop that event entirely.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        seq = try c.decode(UInt64.self, forKey: .seq)
        sessionUID = try c.decodeIfPresent(String.self, forKey: .sessionUID) ?? ""
        sessionID = try c.decode(String.self, forKey: .sessionID)
        ts = try c.decode(String.self, forKey: .ts)
        kind = try c.decode(EventKind.self, forKey: .kind)
        payload = try c.decode(JSONValue.self, forKey: .payload)
        source = try c.decode(EventSource.self, forKey: .source)
        sourceEventID = try c.decodeIfPresent(String.self, forKey: .sourceEventID)
        turnID = try c.decodeIfPresent(String.self, forKey: .turnID)
        itemID = try c.decodeIfPresent(String.self, forKey: .itemID)
    }

    /// Which run this event belongs to, in the app's own stores.
    ///
    /// Derived from the event itself rather than from a capability flag held
    /// somewhere else: one frame can then never be filed under a different key
    /// than the next, whatever the connection believes about the daemon. A
    /// daemon that mints uids puts one on every event, so the fallback is only
    /// ever taken for a whole daemon at a time, never for one event in a stream.
    public var sessionKey: String { sessionUID.isEmpty ? sessionID : sessionUID }

    /// `seq` alone is not unique for the daemon's `Resync` marker, which is sent
    /// with `seq: 0` precisely so it cannot be mistaken for a logged fact. The
    /// *key* rather than the name, so two runs that shared `cc-1` cannot produce
    /// two different events with one id.
    public var id: String { "\(sessionKey)#\(seq)#\(ts)#\(kind.rawValue)" }

    public var date: Date { ISO8601.parse(ts) ?? .distantPast }
}

// MARK: - Session summary

public struct SessionSummary: Codable, Sendable, Hashable, Identifiable {
    /// The run's identity, minted once at spawn. Empty on a daemon older than
    /// protocol minor 2. Two entries in one `sessions` list can share a
    /// `sessionID` when a tmux name has been reused, and only this tells them
    /// apart — see the "Session identity" section of `mac/README.md`.
    public let sessionUID: String
    /// The run's name. A run CodeConnect started is named by its uid.
    public let sessionID: String
    public let cwd: String
    public let lifecycle: Lifecycle
    public let link: Link
    public let claudeSessionID: String?
    let transcriptPath: String?
    public let lastSeq: UInt64
    public let createdAt: String
    let updatedAt: String
    public let blockedOn: [String]
    /// **What to call this run out loud**, resolved by the daemon (minor 11).
    ///
    /// The final component of the run's working directory — the project someone
    /// is working in. Empty when the daemon could not name one, and empty from
    /// any daemon below minor 11, which is the same thing as far as a reader is
    /// concerned: nobody has said what this project is.
    ///
    /// **The only name the screens use** — see `RunLabel`, which is the one
    /// place that turns this into what a reader sees. Nothing in this app
    /// derives a second one from `cwd`: two rules produce two names for one
    /// run, and a notification cannot derive anything at all, since the phone
    /// may not be running when it is composed.
    public let projectLabel: String
    /// **Which agent is running here** (minor 15). Absent ⇒ `.claude`, which is
    /// what every daemon below minor 15 means by sending nothing.
    ///
    /// The first thing every Codex affordance is scoped by: the two agents share
    /// a fleet, a timeline and a decision card, and almost nothing else. Sending
    /// Claude's vocabulary to a Codex session is refused by name at the Mac, and
    /// the reverse likewise — so this field, not a capability flag, is what
    /// decides which controls a session gets.
    public let agent: AgentKind
    /// The Codex thread this run is bound to. **Opaque** — for correlation and
    /// for rendering, never parsed. Absent for a Claude session.
    ///
    /// It says nothing about whether the link can actuate: `codex_link` is that
    /// field, and a thread id is present in three of its four states.
    let codexThreadID: String?
    /// Whether this Mac can stop or speak to this Codex session **right now**.
    /// Defaulted to `.none` so a daemon predating the field still parses.
    public let codexLink: CodexLinkState
    /// **The daemon's own answer to "does the next message relaunch this run?"** (protocol minor
    /// 42), for both agents. Optional: a daemon predating it sends nothing, and the read falls back
    /// to the Codex-only `codexLink` signal.
    public let relaunchable: Bool?
    /// Every card in this run still waiting on a human, by reference (minor
    /// 23): the seq of each one's `approval_request`, so a phone holding only
    /// the newest events can fetch the event that carries the card. Empty from
    /// an older daemon and on a run with nothing open.
    public let pending: [PendingRef]
    /// The model this run last confirmed, with the event that confirmed it
    /// (minor 23). `nil` until something has said.
    public let model: SessionFact?
    /// The permission mode the main agent last reported, with the event that
    /// reported it (minor 23). `nil` until something has said.
    public let permissionMode: SessionFact?
    /// The effort this run last took, with the seq of the event that recorded it
    /// (minor 39). `nil` until something has said.
    public let effort: SessionFact?
    /// The model, effort and mode menus an app may set this run to (minor 39):
    /// present on a **live hosted Claude run this app started, once its start is
    /// answered**, and `nil` for every other run — a terminal or Codex run, one
    /// that has ended or detached, before its start is answered, and from a daemon
    /// below minor 39. The one field that decides whether the model/effort/mode
    /// controls are drawn at all.
    public let menus: RunMenus?
    /// The branch checked out in `cwd` (minor 31), as git read it when a turn
    /// last ended. `nil` outside a repository, on a detached head, before the
    /// first read, and from an older daemon.
    public let gitBranch: String?
    /// Lines added and removed in tracked files against `HEAD` (minor 31), read
    /// at the same moments. `nil` outside a repository, before the first read,
    /// and from an older daemon.
    public let diffStat: DiffStat?

    enum CodingKeys: String, CodingKey {
        case sessionUID = "session_uid"
        case sessionID = "session_id"
        case cwd
        case lifecycle
        case link
        case claudeSessionID = "claude_session_id"
        case transcriptPath = "transcript_path"
        case lastSeq = "last_seq"
        case createdAt = "created_at"
        case updatedAt = "updated_at"
        case blockedOn = "blocked_on"
        case projectLabel = "project_label"
        case agent
        case codexThreadID = "codex_thread_id"
        case codexLink = "codex_link"
        case relaunchable
        case pending
        case model
        case permissionMode = "permission_mode"
        case effort
        case menus
        case gitBranch = "git_branch"
        case diffStat = "diff_stat"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        sessionUID = try c.decodeIfPresent(String.self, forKey: .sessionUID) ?? ""
        sessionID = try c.decode(String.self, forKey: .sessionID)
        cwd = try c.decode(String.self, forKey: .cwd)
        lifecycle = try c.decode(Lifecycle.self, forKey: .lifecycle)
        link = try c.decode(Link.self, forKey: .link)
        claudeSessionID = try c.decodeIfPresent(String.self, forKey: .claudeSessionID)
        transcriptPath = try c.decodeIfPresent(String.self, forKey: .transcriptPath)
        lastSeq = try c.decode(UInt64.self, forKey: .lastSeq)
        createdAt = try c.decode(String.self, forKey: .createdAt)
        updatedAt = try c.decode(String.self, forKey: .updatedAt)
        blockedOn = try c.decodeIfPresent([String].self, forKey: .blockedOn) ?? []
        projectLabel = try c.decodeIfPresent(String.self, forKey: .projectLabel) ?? ""
        agent = try c.decodeIfPresent(AgentKind.self, forKey: .agent) ?? .claude
        codexThreadID = try c.decodeIfPresent(String.self, forKey: .codexThreadID)
        codexLink = try c.decodeIfPresent(CodexLinkState.self, forKey: .codexLink) ?? .none
        relaunchable = try c.decodeIfPresent(Bool.self, forKey: .relaunchable)
        pending = try c.decodeIfPresent([PendingRef].self, forKey: .pending) ?? []
        model = try c.decodeIfPresent(SessionFact.self, forKey: .model)
        permissionMode = try c.decodeIfPresent(SessionFact.self, forKey: .permissionMode)
        effort = try c.decodeIfPresent(SessionFact.self, forKey: .effort)
        menus = try c.decodeIfPresent(RunMenus.self, forKey: .menus)
        gitBranch = try c.decodeIfPresent(String.self, forKey: .gitBranch)
        diffStat = try c.decodeIfPresent(DiffStat.self, forKey: .diffStat)
    }

    /// Whether the Codex vocabulary applies here. The one place the question is
    /// asked, so a future agent cannot accidentally inherit it.
    public var isCodex: Bool { agent == .codex }

    /// **The daemon says the next message relaunches this ended run** (decision 210): its agent's
    /// process died without anyone stopping it, so a message brings it back and continues the
    /// conversation — unlike a run ended by End run or a failed start, which stay ended. One
    /// agent-neutral read for both agents: the daemon's own `relaunchable` field (protocol minor 42)
    /// answers it. A daemon predating the field sends nothing, so the read falls back to the Codex-only
    /// `codexLink` signal — a thread bound but not yet run, on a run that has exited.
    public var relaunchesOnMessage: Bool {
        relaunchable ?? (lifecycle == .exited && codexLink == .boundNotStarted)
    }

    /// **The run takes no reply**: it has ended (or never started) and the next message would not relaunch it, so
    /// both apps show it no reply box, only that it has ended (`SessionWords.runEnded`).
    public var takesNoReply: Bool {
        (lifecycle == .exited || lifecycle == .failed) && !relaunchesOnMessage
    }

    /// The `claude --resume` session id to offer at the computer for an ended run, or nil. **Not offered
    /// for a run the next message relaunches**: that run is picked up in the app by sending it a
    /// message, so the manual command would be a second, worse path beside the open composer. Also nil for
    /// a run still running, or one the daemon recorded no session id for.
    public var resumeSessionID: String? {
        guard lifecycle == .exited, !relaunchesOnMessage, let id = claudeSessionID, !id.isEmpty else {
            return nil
        }
        return id
    }

    /// What this app files the run's events, subscriptions and marks under, and
    /// what it sends when it has to name the run on the wire. See
    /// `Event.sessionKey` for why it is read off the data rather than off a flag.
    public var sessionKey: String { sessionUID.isEmpty ? sessionID : sessionUID }

    /// An adopted run: Claude Code reported it through its hooks, but nothing on
    /// the computer started or hosts it. ccd's own test is the session id's
    /// `claude:` prefix, which the hook mints for a session CodeConnect did not
    /// launch.
    var isAdopted: Bool { sessionID.hasPrefix("claude:") }

    /// Whether the phone may offer to remove this run: ccd's own rule.
    ///
    /// A run must have ended — `lifecycle` `exited` or `failed`, never the
    /// derived Ended status. An adopted run is offered at any lifecycle, because
    /// no probe can ever prove it ended and a row awaiting an unobtainable proof
    /// would be immortal.
    public var isRemovable: Bool { lifecycle == .exited || lifecycle == .failed || isAdopted }

    public var id: String { sessionKey }
    public var updatedDate: Date { ISO8601.parse(updatedAt) ?? .distantPast }
    public var createdDate: Date { ISO8601.parse(createdAt) ?? .distantPast }
}

/// How much a working tree has changed, in lines (`SessionSummary.diffStat`).
public struct DiffStat: Codable, Sendable, Hashable {
    public let added: UInt64
    public let removed: UInt64
}

/// One open card, named without its body (`SessionSummary.pending`).
public struct PendingRef: Codable, Sendable, Hashable {
    public let requestID: String
    /// The seq of the card's `approval_request` event.
    public let seq: UInt64
    /// The card's hold as it stands now. Absent on a card that has none, which
    /// is every Codex card.
    public let hold: QuestionHold?
    /// The turn a Codex card belongs to. Absent on a Claude card.
    let turnID: String?

    enum CodingKeys: String, CodingKey {
        case requestID = "request_id"
        case seq
        case hold
        case turnID = "turn_id"
    }
}

/// A fact and the seq of the event that decided it, so the phone can keep
/// whichever of this and its own events is newer.
public struct SessionFact: Codable, Sendable, Hashable {
    public let value: String
    public let seq: UInt64
}

/// The settings an app may choose for a live hosted Claude run (minor 39), as its agent offers
/// them — see `SessionSummary.menus`. Drawn exactly as the daemon sends it; no family list of this
/// app's own, so a model the agent adds or drops needs no release here.
public struct RunMenus: Codable, Sendable, Hashable {
    public let models: [MenuModel]
    public let modes: [MenuMode]
    /// The menu model `value` the daemon says is chosen now (minor 39). The daemon states it because
    /// only it and claude know which alias a run's model fact resolves to — the fact at every turn's
    /// init is claude's full id, not a menu value. Absent from an older daemon, or when the fact
    /// matches no entry. **Display only** — it changes nothing on the run. Effort and mode need no such
    /// field: their facts already carry the menu's own words.
    public let selectedModel: String?

    enum CodingKeys: String, CodingKey {
        case models, modes
        case selectedModel = "selected_model"
    }
}

/// One model of a run's menu: the agent's own `value` (what a `set_model` carries), its label and its
/// description, each exactly as the agent sends it, and the efforts this model takes.
public struct MenuModel: Codable, Sendable, Hashable {
    public let value: String
    public let label: String
    public let efforts: [String]
    /// The agent's own description of the model, as sent (Claude Code's carries the model's version); absent
    /// when it gives none.
    public let detail: String?
    /// The model id the agent says this entry runs now (minor 45; Claude Code's `resolvedModel`), absent from a
    /// Codex entry, whose `value` is the model, and from a daemon below 45.
    public let resolvedModel: String?

    enum CodingKeys: String, CodingKey {
        case value, label, efforts, detail
        case resolvedModel = "resolved_model"
    }

    public init(value: String, label: String, efforts: [String], detail: String? = nil, resolvedModel: String? = nil) {
        self.value = value
        self.label = label
        self.efforts = efforts
        self.detail = detail
        self.resolvedModel = resolvedModel
    }

    /// The name an app shows for a model: the agent's own label, except its default entry, which is "Default" for
    /// every agent (Claude Code's own reads "Default (recommended)"; the model's name is enough).
    public static func shownLabel(value: String, label: String) -> String { value == "default" ? "Default" : label }

    /// This model with the label an app shows.
    var shown: MenuModel {
        MenuModel(
            value: value, label: Self.shownLabel(value: value, label: label), efforts: efforts, detail: detail,
            resolvedModel: resolvedModel)
    }
}

/// One permission mode of a run's menu: the `value` a `set_mode` carries and the label it shows.
/// **The labels are the agent's** — this app hard-codes no mode names.
public struct MenuMode: Codable, Sendable, Hashable {
    public let value: String
    public let label: String
    /// Whether choosing this mode needs the person's confirmation first (minor 40), because it lets the
    /// agent work without asking. Absent reads false — a daemon below minor 40 marks nothing, and every
    /// mode is set without a confirmation, as before.
    public let confirm: Bool

    public init(value: String, label: String, confirm: Bool = false) {
        self.value = value
        self.label = label
        self.confirm = confirm
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        value = try c.decode(String.self, forKey: .value)
        label = try c.decode(String.self, forKey: .label)
        confirm = try c.decodeIfPresent(Bool.self, forKey: .confirm) ?? false
    }

    private enum CodingKeys: String, CodingKey { case value, label, confirm }
}

// MARK: - Capabilities

/// What CodeConnect can actually do, reported honestly rather than assumed —
/// the phone disables affordances it does not see advertised.
///
/// Decoding is deliberately total: every field has a conservative default and
/// the whole advertised map is kept verbatim in `advertised`. Two reasons, both
/// learned the hard way:
///
///   * a strict decode makes *one* renamed key fail the entire `hello_ack`,
///     which the UI can only render as "the daemon is unreachable" — a lie about
///     a daemon that is answering perfectly well; and
///   * capability keys are added additively and named on the Mac side, so a
///     build that only understands the names it was compiled with would go
///     silently blind to a feature that is actually there. `advertised` lets the
///     Link Health sheet list capabilities this build has never heard of.
///
/// Every default is "not available", so an unreadable field can only ever hide
/// an affordance, never invent one.
public struct Capabilities: Codable, Sendable, Hashable {
    public let canApproveReliably: Bool
    public let sendText: Bool
    let push: Bool
    /// A minor-14 relay daemon advertises this instead of `push`: it sends
    /// through the CodeConnect relay, which needs an enrolled credential the
    /// legacy token-only flow cannot supply. A relay daemon advertises `push =
    /// false` on purpose so an older app never prompts and registers a
    /// credential-less token. See `pushMode`.
    let pushRelay: Bool
    public let tls: Bool
    /// Exactly what arrived, unknown keys included.
    let advertised: [String: JSONValue]

    public init(
        canApproveReliably: Bool = false,
        sendText: Bool = false,
        push: Bool = false,
        pushRelay: Bool = false,
        tls: Bool = false,
        extra: [String: JSONValue] = [:]
    ) {
        self.canApproveReliably = canApproveReliably
        self.sendText = sendText
        self.push = push
        self.pushRelay = pushRelay
        self.tls = tls
        var map = extra
        map["can_approve_reliably"] = .bool(canApproveReliably)
        map["send_text"] = .bool(sendText)
        map["push"] = .bool(push)
        map["push_relay"] = .bool(pushRelay)
        map["tls"] = .bool(tls)
        advertised = map
    }

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode([String: JSONValue].self)
        advertised = raw
        canApproveReliably = raw["can_approve_reliably"]?.boolValue ?? false
        sendText = raw["send_text"]?.boolValue ?? false
        push = raw["push"]?.boolValue ?? false
        pushRelay = raw["push_relay"]?.boolValue ?? false
        tls = raw["tls"]?.boolValue ?? false
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(advertised)
    }

    /// True when *any* of `names` is advertised as a true flag.
    ///
    /// The keys below are the ones `protocol/src/ws.rs` actually ships; the
    /// synonyms cost one array literal each and mean a renamed key downgrades an
    /// affordance instead of making a working feature invisible. Unknown is
    /// always false, so this can only ever hide something, never invent it.
    func advertises(_ names: [String]) -> Bool {
        names.contains { advertised[$0]?.boolValue == true }
    }

    /// `get_diff` is answerable.
    public var servesDiff: Bool { advertises(["diff", "get_diff", "diffs"]) }
    /// `send_text` accepts `request_id` + `payload_hash` and replays a retry's
    /// original outcome instead of typing twice.
    var sendTextIdempotent: Bool { advertises(["send_text_idempotent"]) }
    /// `get_command_catalog` is answerable.
    var servesCommandCatalog: Bool { advertises(["command_catalog"]) }
    /// `list_projects` and `start_session` are answerable.
    public var startsSessions: Bool { advertises(["start_session"]) }
    /// `get_project_git` is answerable, and `start_session` takes a
    /// `worktree` (minor 29). Without it a `worktree` is ignored and the start
    /// runs in the folder, so none is sent.
    public var startsInWorktrees: Bool { advertises(["start_worktree"]) }
    /// `list_folder` and `create_folder` are answerable, and `start_session`
    /// takes any absolute folder (minor 30).
    public var browsesFolders: Bool { advertises(["browse_folders"]) }
    /// `get_machine_stats` is answerable (minor 31).
    public var reportsMachineStats: Bool { advertises(["machine_stats"]) }
    /// Approval cards carry a daemon-computed `risk` block.
    public var classifiesRisk: Bool { advertises(["risk_class", "risk", "risk_classes"]) }
    /// This connection is private, as the daemon judges it: TLS, its tailnet
    /// address or loopback (`private_transport`).
    public var isPrivateTransport: Bool { advertises(["private_transport"]) }
    /// The daemon said this connection is not private, rather than not saying at
    /// all as an older daemon does.
    public var saysNotPrivate: Bool { advertised["private_transport"]?.boolValue == false }
    /// The daemon accepts `delete_session`. Absent before minor 7, and unknown is
    /// false, so an older Mac simply does not offer the swipe rather than offering
    /// one that silently does nothing.
    public var deletesSessions: Bool { advertises(["delete_session"]) }
    /// `test_push` is answerable — distinct from `push`, which a minor-6 daemon
    /// advertises without understanding the test request.
    public var testsPush: Bool { advertises(["test_push"]) }
    /// The one push decision the rest of the app reads, normalized from the two
    /// raw flags with **direct precedence**: a daemon that somehow advertised
    /// both is a direct-key daemon, and the direct path needs no relay
    /// credential. Every push gate — eligibility, registration, the test button,
    /// the trust-screen row — keys off this, never off `push` or `pushRelay`
    /// alone, so the two flags cannot disagree anywhere downstream.
    public var pushMode: PushMode {
        if push { return .direct }
        if pushRelay { return .relay }
        return .none
    }
    /// This particular connection is encrypted — a different fact from `tls`,
    /// which only says the listener *holds* a certificate. The daemon accepts
    /// both schemes on one port during the migration, so the phone is entitled
    /// to both facts and must not conflate them.
    public var tlsActive: Bool { advertises(["tls_active"]) }
    /// Sessions and events carry a `session_uid`, and every message that names a
    /// session accepts one. False means a reused tmux name is the only identity
    /// there is, and two runs under one name will read as a single spliced
    /// timeline — which is exactly what this flag being true fixes.
    public var scopesSessionsByUID: Bool { advertises(["session_uid", "session_uids"]) }
    /// This daemon **honours** a stop for a Codex session whose control link is
    /// subscribed (minor 17). A minor-15/16 daemon decodes `interrupt` and
    /// answers a well-formed `Rejected` to every ask — legible, but guaranteed
    /// — so an unflagged daemon gets the button offered-and-silently-broken,
    /// which is why this gates the send and not only the control.
    public var codexInterrupt: Bool { advertises(["codex_interrupt"]) }
    /// This daemon **honours** a stop for a hosted Claude run (minor 37). Its own
    /// flag, not folded into `codexInterrupt`: a daemon can carry one Stop and not
    /// the other, and a Claude Stop sent to a daemon that only answers Codex would
    /// be the same offered-and-silently-broken control this gate exists to prevent.
    public var claudeInterrupt: Bool { advertises(["claude_interrupt"]) }
    /// This daemon **understands `compose` at all** (minor 18).
    ///
    /// **The load-bearing one.** A minor-17 daemon cannot decode the message and
    /// answers `Error{code:"bad_request"}` rather than a `ComposeResult`, so a
    /// phone that sent one would wait on a typed reply that is never coming.
    /// Hiding the button is not the same thing as not transmitting.
    public var codexCompose: Bool { advertises(["codex_compose"]) }
    /// This daemon ends a run it started (`stop_run`, minor 35). An older one
    /// answers `stop_run` with `bad_request`, so the frame is sent only when this is set.
    public var endsRuns: Bool { advertises(["stop_run"]) }
    /// This daemon answers Claude's `AskUserQuestion` from the phone (minor
    /// 21): its question cards carry `question_hold` and it takes the
    /// `answers` and `decline` decisions. Absent is false — an older daemon
    /// shows the question as an ordinary approval.
    public var questionCard: Bool { advertises(["question_card"]) }
    /// This daemon answers every Claude card only through the hook Claude holds
    /// for that card's own call, and never types (minor 22). Absent is false:
    /// an older daemon types an approval's answer into whatever prompt is on
    /// screen, which can be another agent's, so its Claude approvals are shown
    /// read-only.
    public var hookOnlyApprovals: Bool { advertises(["hook_only_approvals"]) }
    /// This daemon serves earlier pages of a run's log (`history`) and puts each
    /// run's open cards, model and mode on its summary (minor 23). Absent is
    /// false: an older daemon answers `history` with `bad_request`.
    public var historyPages: Bool { advertises(["history_pages"]) }
    /// Which agents this daemon hosts. **Omitted entirely while it would only
    /// name Claude** (`ws.rs`), so empty and absent both read as `["claude"]`.
    var supportedAgents: [String] {
        let named = advertised["supported_agents"]?.arrayValue?
            .compactMap(\.stringValue) ?? []
        return named.isEmpty ? ["claude"] : named
    }
    /// Whether this daemon says it hosts Codex sessions at all. Advisory: the
    /// per-session `agent` is what actually scopes an affordance, and this is
    /// the connection-wide fact the trust screen reports.
    var hostsCodex: Bool { supportedAgents.contains("codex") }
}

// MARK: - Answers

public enum AnswerDecision: Sendable, Hashable {
    case allow
    case deny
    /// Pick the nth option exactly as Claude numbered it (1-based).
    case option(index: UInt32)
    /// Pick an option by its stable id rather than its ordinal — the agent-seam
    /// shape, where Codex names options by id (`"acceptWithExecpolicyAmendment"`)
    /// rather than position. Additive to `option`.
    case optionId(String)
    case text(String)
    /// Answers to Claude's `AskUserQuestion`, one per question in card order
    /// (minor 21). The daemon builds Claude's answer from the question it
    /// stored and these indices; nothing is typed.
    case answers([QuestionAnswer])
    /// Decline an `AskUserQuestion` (minor 21): the daemon denies it through
    /// its held hook, or, for a run started from an app, through the run's
    /// agent host. The main conversation's question is denied with an
    /// interrupt, so the turn stops as Escape at its dialog does; a background
    /// agent's is denied without one.
    case decline
    /// A decision kind a newer daemon knows about and this build does not.
    /// Never sent — only received, inside a recorded outcome. Throwing here
    /// instead would make the *whole* `answer_result` frame undecodable, and the
    /// tap that is waiting on it would time out rather than learn what actually
    /// happened.
    case unrecognised(String)
}

extension AnswerDecision: Codable {
    private enum CodingKeys: String, CodingKey {
        case type, index, text, answers
        case optionId = "option_id"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .type) {
        case "allow": self = .allow
        case "deny": self = .deny
        case "option": self = .option(index: try c.decode(UInt32.self, forKey: .index))
        case "option_id": self = .optionId(try c.decode(String.self, forKey: .optionId))
        case "text": self = .text(try c.decode(String.self, forKey: .text))
        case "answers": self = .answers(try c.decode([QuestionAnswer].self, forKey: .answers))
        case "decline": self = .decline
        case let other: self = .unrecognised(other)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .allow:
            try c.encode("allow", forKey: .type)
        case .deny:
            try c.encode("deny", forKey: .type)
        case .option(let index):
            try c.encode("option", forKey: .type)
            try c.encode(index, forKey: .index)
        case .optionId(let id):
            try c.encode("option_id", forKey: .type)
            try c.encode(id, forKey: .optionId)
        case .text(let text):
            try c.encode("text", forKey: .type)
            try c.encode(text, forKey: .text)
        case .answers(let answers):
            try c.encode("answers", forKey: .type)
            try c.encode(answers, forKey: .answers)
        case .decline:
            try c.encode("decline", forKey: .type)
        case .unrecognised(let raw):
            try c.encode(raw, forKey: .type)
        }
    }

    public var label: String {
        switch self {
        case .allow: return "Allowed"
        case .deny: return "Denied"
        case .option(let index): return "Chose option \(index)"
        // "Allow for this session" is sent as this one option id, by a Claude
        // card that offered it and by the Codex MCP card alike; both mean the
        // same thing, so the resolved line reads it as the grant it was.
        case .optionId("acceptForSession"): return "Allowed for this session"
        case .optionId(let id): return "Chose option \(id)"
        case .text: return "Denied with a reason"
        case .answers: return "Answered"
        case .decline: return "Declined"
        case .unrecognised(let raw): return "Answered (\(raw))"
        }
    }
}

/// One question's answer on an `AskUserQuestion` card — `protocol::ws::QuestionAnswer`.
///
/// `selected` is 0-based option indices **in the order they were chosen**, and
/// is omitted when empty, as the daemon writes it. Single-select: exactly one of
/// one index or `other`. Multi-select: indices plus at most one `other`. `notes`
/// only on a single-select question whose options carry a preview.
public struct QuestionAnswer: Codable, Sendable, Hashable {
    public var selected: [UInt32] = []
    public var other: String?
    public var notes: String?

    private enum CodingKeys: String, CodingKey { case selected, other, notes }

    public init(selected: [UInt32] = [], other: String? = nil, notes: String? = nil) {
        self.selected = selected
        self.other = other
        self.notes = notes
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        selected = try c.decodeIfPresent([UInt32].self, forKey: .selected) ?? []
        other = try c.decodeIfPresent(String.self, forKey: .other)
        notes = try c.decodeIfPresent(String.self, forKey: .notes)
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        if !selected.isEmpty { try c.encode(selected, forKey: .selected) }
        try c.encodeIfPresent(other, forKey: .other)
        try c.encodeIfPresent(notes, forKey: .notes)
    }
}

/// Whether the phone can answer an `AskUserQuestion` card now (minor 21) —
/// `protocol::ws::QuestionHold`.
public enum QuestionHold: Sendable, Hashable {
    /// Claude is waiting on the daemon: the phone can answer. For the main
    /// conversation the Mac shows the same question, and the first complete
    /// answer wins.
    case held
    /// A word this build does not know. Never answerable.
    case unrecognised(String)

    init(wire: String) {
        switch wire {
        case "held": self = .held
        default: self = .unrecognised(wire)
        }
    }

    var wire: String {
        switch self {
        case .held: return "held"
        case .unrecognised(let raw): return raw
        }
    }
}

extension QuestionHold: Codable {
    public init(from decoder: Decoder) throws {
        self.init(wire: try decoder.singleValueContainer().decode(String.self))
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(wire)
    }
}

public struct AnswerOutcome: Codable, Sendable, Hashable {
    public let requestID: String
    /// The *name* of the run that was answered (`session.name` in ccd), not its
    /// uid. Display only; never key anything by it.
    let sessionID: String
    public let decision: AnswerDecision
    public let resolvedBy: ResolvedBy
    /// Which kind of device sent an answer from a paired device (minor 34). `nil`
    /// on every other outcome, from an older daemon, on an answer a restart
    /// settled, and for a kind this build does not know.
    public let answeredOn: AnsweredOn?
    public let appliedVia: AnswerPath
    public let resolvedAt: String
    public let detail: String?
    /// True when the daemon could not establish whether the answer landed at
    /// all — the agent-seam case: it asserts nothing about the write.
    /// Absent ⇒ false, mirroring the Rust `#[serde(default)]` bool.
    public let indeterminate: Bool

    enum CodingKeys: String, CodingKey {
        case requestID = "request_id"
        case sessionID = "session_id"
        case decision
        case resolvedBy = "resolved_by"
        case answeredOn = "answered_on"
        case appliedVia = "applied_via"
        case resolvedAt = "resolved_at"
        case detail
        case indeterminate
    }

    public init(
        requestID: String, sessionID: String, decision: AnswerDecision, resolvedBy: ResolvedBy,
        answeredOn: AnsweredOn? = nil, appliedVia: AnswerPath, resolvedAt: String, detail: String?,
        indeterminate: Bool = false
    ) {
        self.requestID = requestID
        self.sessionID = sessionID
        self.decision = decision
        self.resolvedBy = resolvedBy
        self.answeredOn = answeredOn
        self.appliedVia = appliedVia
        self.resolvedAt = resolvedAt
        self.detail = detail
        self.indeterminate = indeterminate
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        requestID = try c.decode(String.self, forKey: .requestID)
        sessionID = try c.decode(String.self, forKey: .sessionID)
        decision = try c.decode(AnswerDecision.self, forKey: .decision)
        resolvedBy = try c.decode(ResolvedBy.self, forKey: .resolvedBy)
        answeredOn = try c.decodeIfPresent(String.self, forKey: .answeredOn).flatMap(AnsweredOn.init(rawValue:))
        appliedVia = try c.decode(AnswerPath.self, forKey: .appliedVia)
        resolvedAt = try c.decode(String.self, forKey: .resolvedAt)
        detail = try c.decodeIfPresent(String.self, forKey: .detail)
        indeterminate = try c.decodeIfPresent(Bool.self, forKey: .indeterminate) ?? false
    }

    public var resolvedDate: Date { ISO8601.parse(resolvedAt) ?? .distantPast }

    /// How to describe this outcome without overclaiming. A `timeout` is how the
    /// daemon files a card nobody answered, closed when its run ended or its turn
    /// was stopped: its `deny` is nobody's decision, so it is "Closed".
    public var decisionLabel: String {
        resolvedBy == .timeout ? "Closed" : decision.label
    }

    /// The device a paired answer came from, by its kind, or nil where the
    /// iPhone's words stay as they were: where an iPhone answered, or the record
    /// does not say (an older daemon, an answer a restart settled). A Mac reading
    /// a record that names no device says "a paired device", which is all it
    /// knows. Never "this Mac" or "your iPhone": the record names a kind of
    /// device, not which one.
    static func answeringDevice(_ device: AnsweredOn?, on host: HostDevice) -> String? {
        switch device {
        case .mac: return "a Mac"
        case .iphone: return host == .mac ? "an iPhone" : nil
        case nil: return host == .mac ? "a paired device" : nil
        }
    }

    /// The daemon's words (`RUN_ENDED` and `STOPPED_BEFORE_ANSWER` in ccd) for a card
    /// closed, as a `timeout`, because its session ended or its turn was stopped. They
    /// are matched to choose a reason phrase rather than shown, because the two are not
    /// written as one grammatical shape; any other detail falls back to the timeout words.
    static let sessionEnded = "the session ended before anyone answered"
    static let turnStopped = "The turn was stopped before anyone answered."

    /// Where the answer came from, as far as the record proves it: `phone`
    /// names a paired device by its kind (`answeredOn`), and where the record
    /// does not say, "a phone" on the iPhone and "a paired device" on a Mac.
    public func provenance(on host: HostDevice = .current) -> String {
        switch resolvedBy {
        case .phone: return Self.answeringDevice(answeredOn, on: host).map { "from \($0)" } ?? "from a phone"
        case .unknown: return "by an answerer this app does not recognise"
        case .timeout where detail == Self.sessionEnded: return "when the session ended"
        case .timeout where detail == Self.turnStopped: return "when the turn was stopped"
        case .timeout: return "by timeout - nobody answered"
        }
    }
}

public enum AnswerResult: Sendable, Hashable {
    case applied(outcome: AnswerOutcome)
    /// Already resolved; carries the *original* outcome so a retried tap can
    /// never double-apply. Answering is idempotent: a repeat of a request that
    /// has already been decided reports what the first one decided, it does not
    /// decide again.
    case duplicate(outcome: AnswerOutcome, stalePayloadHash: Bool)
    case rejected(reason: String)
}

extension AnswerResult: Codable {
    private enum CodingKeys: String, CodingKey {
        case status, outcome, reason
        case stalePayloadHash = "stale_payload_hash"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .status) {
        case "applied":
            self = .applied(outcome: try c.decode(AnswerOutcome.self, forKey: .outcome))
        case "duplicate":
            self = .duplicate(
                outcome: try c.decode(AnswerOutcome.self, forKey: .outcome),
                stalePayloadHash: try c.decodeIfPresent(Bool.self, forKey: .stalePayloadHash)
                    ?? false)
        case "rejected":
            self = .rejected(reason: try c.decode(String.self, forKey: .reason))
        case let other:
            self = .rejected(reason: "unrecognised answer_result status: \(other)")
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .applied(let outcome):
            try c.encode("applied", forKey: .status)
            try c.encode(outcome, forKey: .outcome)
        case .duplicate(let outcome, let stale):
            try c.encode("duplicate", forKey: .status)
            try c.encode(outcome, forKey: .outcome)
            try c.encode(stale, forKey: .stalePayloadHash)
        case .rejected(let reason):
            try c.encode("rejected", forKey: .status)
            try c.encode(reason, forKey: .reason)
        }
    }
}

// MARK: - Codex resolution envelope

/// Who a Codex prompt was resolved by. Decode-only, and an unrecognised actor
/// is retained rather than coerced: nothing here may guess who acted.
public enum ResolutionActor: Decodable, Sendable, Hashable {
    case phone
    /// An actor a newer daemon names and this build does not.
    case unknown(String)

    public init(from decoder: Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "phone": self = .phone
        case let other: self = .unknown(other)
        }
    }
}

/// Why a Codex prompt was cleared without an answer. `turnAborted` is
/// load-bearing — it is how the app learns the turn itself went away — so it
/// must decode and be retained, never folded into the unknown fallback.
public enum ClearCause: Decodable, Sendable, Hashable {
    case turnAborted, turnCompleted, superseded
    /// **The item finished, and the turn is still running.** Deliberately not
    /// `turnCompleted` (`ws.rs`) — the two say opposite things about whether
    /// there is anything left to stop, and folding this one into the unknown
    /// bucket put it beside causes that mean the session is over.
    case itemCompleted
    /// A cause a newer daemon names and this build does not.
    case unknown(String)

    public init(from decoder: Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "turn_aborted": self = .turnAborted
        case "turn_completed": self = .turnCompleted
        case "superseded": self = .superseded
        case "item_completed": self = .itemCompleted
        case let other: self = .unknown(other)
        }
    }
}

/// How far a write got before the daemon lost sight of it, on the
/// `status:"unknown"` path. Each value is a strictly weaker claim than the
/// last, and an unrecognised stage is retained rather than assumed to be any
/// of them.
public enum WriteStage: Decodable, Sendable, Hashable {
    case claimedNotEnqueued, brokerIngressAccepted, upstreamWriteUnconfirmed
    /// A stage a newer daemon names and this build does not.
    case unknown(String)

    public init(from decoder: Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "claimed_not_enqueued": self = .claimedNotEnqueued
        case "broker_ingress_accepted": self = .brokerIngressAccepted
        case "upstream_write_unconfirmed": self = .upstreamWriteUnconfirmed
        case let other: self = .unknown(other)
        }
    }
}

/// What became of a Codex approval prompt at the agent seam, internally tagged
/// by `status`. **Decode-only** — nothing in the app sends one — and an
/// unrecognised status is kept in its own `unrecognisedStatus` case so a daemon
/// ahead of this build never collapses into `timeout` or any other real
/// outcome. Nothing renders these yet; they only need to decode and retain.
public enum CodexResolution: Decodable, Sendable, Hashable {
    /// The prompt was answered. `decision` is omitted when the daemon recorded
    /// no decision alongside the actor.
    case answered(by: ResolutionActor, decision: AnswerDecision?, answeredOn: AnsweredOn?)
    /// The prompt went away without an answer. `cause` says why.
    case cleared(cause: ClearCause)
    /// The prompt aged out.
    case timeout
    /// The wire's own `status:"unknown"` — the daemon attempted a write and
    /// could not confirm it. Distinct from an unrecognised status word.
    case unknown(
        attemptedBy: ResolutionActor, attemptedDecision: AnswerDecision?, writeStage: WriteStage,
        cause: String)
    /// A `status` word this build has never seen, retained verbatim. Kept apart
    /// from every real outcome so a future status can never be read as one.
    case unrecognisedStatus(String)

    private enum CodingKeys: String, CodingKey {
        case status, by, decision, cause
        case attemptedBy = "attempted_by"
        case attemptedDecision = "attempted_decision"
        case writeStage = "write_stage"
        case answeredOn = "answered_on"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .status) {
        case "answered":
            self = .answered(
                by: try c.decode(ResolutionActor.self, forKey: .by),
                decision: try c.decodeIfPresent(AnswerDecision.self, forKey: .decision),
                answeredOn: try c.decodeIfPresent(String.self, forKey: .answeredOn)
                    .flatMap(AnsweredOn.init(rawValue:)))
        case "cleared":
            self = .cleared(cause: try c.decode(ClearCause.self, forKey: .cause))
        case "timeout":
            self = .timeout
        case "unknown":
            self = .unknown(
                attemptedBy: try c.decode(ResolutionActor.self, forKey: .attemptedBy),
                attemptedDecision: try c.decodeIfPresent(
                    AnswerDecision.self, forKey: .attemptedDecision),
                writeStage: try c.decode(WriteStage.self, forKey: .writeStage),
                cause: try c.decode(String.self, forKey: .cause))
        case let other:
            self = .unrecognisedStatus(other)
        }
    }
}

/// What became of one `stop_run` (minor 35).
public enum StopRunResult: Decodable, Sendable, Hashable {
    /// The stop is accepted: the run is `stopping`, and reads ended once it has exited.
    case stopping
    /// The run had ended; nothing was done.
    case alreadyEnded
    /// Nothing was done, and `reason` says why, written to be shown verbatim.
    case refused(reason: String)
    /// A status this build has never seen: it claims neither that the run is stopping
    /// nor that it is not.
    case unknown(status: String)

    private enum CodingKeys: String, CodingKey {
        case status, reason
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let status = try c.decode(String.self, forKey: .status)
        switch status {
        case "stopping": self = .stopping
        case "already_ended": self = .alreadyEnded
        case "refused": self = .refused(reason: try c.decode(String.self, forKey: .reason))
        default: self = .unknown(status: status)
        }
    }
}

// MARK: - Codex mutations

/// What became of an `interrupt` — **four genuinely different pieces of news**,
/// and `ws.rs` says so in its own words: the turn reached its aborted boundary /
/// this exact ask already did that / nothing was actuated and here is why / the
/// stop was issued and this daemon did not live to see what it did.
///
/// Collapsing any pair of them would make the phone claim something it was not
/// told. Tagged on **`status`**, like every other result on this wire.
///
/// **Decode-only.** Nothing in the app constructs one to send.
public enum InterruptResult: Decodable, Sendable, Hashable {
    /// The turn reached its aborted boundary. `turnID` is the one the phone
    /// itself named, so returning it discloses nothing.
    case aborted(turnID: String)
    /// This exact request already did that. Idempotence, reported honestly
    /// rather than replayed as a fresh abort.
    case duplicate(turnID: String)
    /// Nothing was actuated, and `reason` says which condition failed. The
    /// daemon writes these sentences to be shown verbatim.
    case rejected(reason: String)
    /// The stop was issued and the daemon never learned what it did. **Not a
    /// refusal**: a refusal promises nothing happened, and this promises
    /// nothing at all — so it must never be offered a retry that re-sends.
    case indeterminate(reason: String)
    /// **A status this build has never seen.**
    ///
    /// Its own case, and it took a review to get here: it used to decode as
    /// `.indeterminate`, which is not a neutral fallback — `indeterminate`
    /// asserts *the mutation was issued*. A future `cancelled_before_send`
    /// would therefore have rendered "Sent, outcome unknown" and suppressed a
    /// retry that was perfectly safe. This case claims neither actuation nor
    /// non-actuation, which is the only honest reading of a word this build
    /// cannot interpret.
    case unknown(status: String)

    private enum CodingKeys: String, CodingKey {
        case status, reason
        case turnID = "turn_id"
    }

    /// The turn, on the two arms that carry one. `nil` on both refusal arms —
    /// the redaction contract, as a property rather than as a comment.
    var turnID: String? {
        switch self {
        case .aborted(let id), .duplicate(let id): return id
        case .rejected, .indeterminate, .unknown: return nil
        }
    }

    /// The reason received from the daemon, shown verbatim wherever it exists.
    var reason: String? {
        switch self {
        case .rejected(let reason), .indeterminate(let reason): return reason
        case .aborted, .duplicate, .unknown: return nil
        }
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .status) {
        case "aborted": self = .aborted(turnID: try c.decode(String.self, forKey: .turnID))
        case "duplicate": self = .duplicate(turnID: try c.decode(String.self, forKey: .turnID))
        case "rejected": self = .rejected(reason: try c.decode(String.self, forKey: .reason))
        case "indeterminate":
            self = .indeterminate(reason: try c.decode(String.self, forKey: .reason))
        case let other:
            self = .unknown(status: other)
        }
    }
}

/// What became of a `compose` — five arms, and `started` and `steered` must not
/// be collapsed (`ws.rs`). Which one arrives is the answer to *"what did my
/// words do"*, and it is decided by what the session was doing at the instant
/// the daemon wrote, not by anything the phone could predict when it tapped.
///
/// **Decode-only.** Tagged on `status`.
public enum ComposeResult: Decodable, Sendable, Hashable {
    /// Codex was idle: the words began the turn it is running now.
    case started(turnID: String)
    /// Codex was mid-turn: the words joined it. **The same turn**, not a new
    /// one — a steer produces no `turn/started`.
    case steered(turnID: String)
    /// A replay of a request already claimed. `started` is **required** on the
    /// wire, no default: the route is snapshotted at the claim, so a retry after
    /// the session moved reports what actually happened rather than what would
    /// happen now, and a defaulted flag would report the wrong route.
    case duplicate(turnID: String, started: Bool)
    /// Nothing was sent, and `reason` says why. Verbatim.
    case rejected(reason: String)
    /// Written, outcome unknown, **never retried automatically**.
    case indeterminate(reason: String)
    /// A status this build has never seen — see `InterruptResult.unknown`. It
    /// claims neither that the words were said nor that they were not.
    case unknown(status: String)

    private enum CodingKeys: String, CodingKey {
        case status, reason, started
        case turnID = "turn_id"
    }

    var turnID: String? {
        switch self {
        case .started(let id), .steered(let id), .duplicate(let id, _): return id
        case .rejected, .indeterminate, .unknown: return nil
        }
    }

    var reason: String? {
        switch self {
        case .rejected(let reason), .indeterminate(let reason): return reason
        case .started, .steered, .duplicate, .unknown: return nil
        }
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .status) {
        case "started": self = .started(turnID: try c.decode(String.self, forKey: .turnID))
        case "steered": self = .steered(turnID: try c.decode(String.self, forKey: .turnID))
        case "duplicate":
            self = .duplicate(
                turnID: try c.decode(String.self, forKey: .turnID),
                // Required, deliberately: `decode`, never `decodeIfPresent`.
                started: try c.decode(Bool.self, forKey: .started))
        case "rejected": self = .rejected(reason: try c.decode(String.self, forKey: .reason))
        case "indeterminate":
            self = .indeterminate(reason: try c.decode(String.self, forKey: .reason))
        case let other:
            self = .unknown(status: other)
        }
    }
}

/// What became of a `set_model` or `set_mode` (minor 39) — tagged on `status`, like every other
/// result on this wire. **Decode-only.** `effort` rides `set_model`, so its answer is one of these too.
public enum SettingResult: Decodable, Sendable, Hashable {
    /// The agent took it; `applies` says from when. Carries no sentence — the app words the applied
    /// case itself, naming the setting and the value.
    case applied(applies: SettingApplies)
    /// Nothing more was changed, and `reason` says why (verbatim, the agent's own words). When the
    /// model changed and the effort did not, `reason` says that.
    case refused(reason: String)
    /// Sent, and no answer came: it may or may not have been applied. Shown verbatim; **never**
    /// silently retried, exactly as `InterruptResult.indeterminate`.
    case indeterminate(reason: String)
    /// A status this build has never seen — see `InterruptResult.unknown`. Claims neither applied
    /// nor not.
    case unknown(status: String)

    private enum CodingKeys: String, CodingKey { case status, applies, reason }

    /// The agent's own sentence, where one was carried — refusals and the indeterminate case. `nil`
    /// on `applied` (the app words that) and on `unknown`.
    var reason: String? {
        switch self {
        case .refused(let reason), .indeterminate(let reason): return reason
        case .applied, .unknown: return nil
        }
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .status) {
        case "applied": self = .applied(applies: try c.decode(SettingApplies.self, forKey: .applies))
        case "refused": self = .refused(reason: try c.decode(String.self, forKey: .reason))
        case "indeterminate":
            self = .indeterminate(reason: try c.decode(String.self, forKey: .reason))
        case let other:
            self = .unknown(status: other)
        }
    }
}

/// When a setting takes hold. A type of its own, decoded through `init(wire:)`, so a value this
/// build has not seen is read — as `.nextTurn` at the rendering, the conservative reading — rather
/// than refused.
public enum SettingApplies: Sendable, Hashable {
    /// The turn running now already reflects it.
    case now
    /// It applies from the next message.
    case nextTurn
    /// A value this build has never seen.
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "now": self = .now
        case "next_turn": self = .nextTurn
        default: self = .unknown(wire)
        }
    }
}

extension SettingApplies: Decodable {
    public init(from decoder: Decoder) throws {
        self.init(wire: try decoder.singleValueContainer().decode(String.self))
    }
}

public enum SendTextResult: Sendable, Hashable {
    /// `matched` is the prompt-presence needle that authorised the keystrokes.
    case sent(matched: String)
    case refused(reason: String)
    /// This exact mutation already landed once; the daemon replayed the
    /// original outcome instead of typing twice. Only reachable when the
    /// request carried an identity.
    case duplicate(matched: String, appliedAt: String)
    /// The daemon never found out whether the keystrokes landed. Not a
    /// refusal: a refusal promises nothing was typed, and this promises
    /// nothing at all.
    case indeterminate(reason: String)
}

extension SendTextResult: Codable {
    private enum CodingKeys: String, CodingKey {
        case status, matched, reason
        case appliedAt = "applied_at"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .status) {
        case "sent": self = .sent(matched: try c.decode(String.self, forKey: .matched))
        case "refused": self = .refused(reason: try c.decode(String.self, forKey: .reason))
        case "duplicate":
            self = .duplicate(
                matched: try c.decode(String.self, forKey: .matched),
                appliedAt: try c.decode(String.self, forKey: .appliedAt))
        case "indeterminate":
            self = .indeterminate(reason: try c.decode(String.self, forKey: .reason))
        case let other:
            // A status this build has never seen is a mutation result it
            // cannot vouch for. "Refused" would promise nothing was typed —
            // a promise on the daemon's behalf — so the unknown decodes as
            // the case that promises nothing.
            self = .indeterminate(reason: "unrecognised send_text status: \(other)")
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .sent(let matched):
            try c.encode("sent", forKey: .status)
            try c.encode(matched, forKey: .matched)
        case .refused(let reason):
            try c.encode("refused", forKey: .status)
            try c.encode(reason, forKey: .reason)
        case .duplicate(let matched, let appliedAt):
            try c.encode("duplicate", forKey: .status)
            try c.encode(matched, forKey: .matched)
            try c.encode(appliedAt, forKey: .appliedAt)
        case .indeterminate(let reason):
            try c.encode("indeterminate", forKey: .status)
            try c.encode(reason, forKey: .reason)
        }
    }
}

// MARK: - Approval card

/// Payload of an `approval_request` event, under the `card` key.
public struct ApprovalCard: Codable, Sendable, Hashable {
    public let requestID: String
    public let payloadHash: String
    public let toolName: String
    public let toolInput: JSONValue
    /// The exact text hashed into `payloadHash`.
    public let displayText: String
    let permissionSuggestions: JSONValue?
    public let permissionMode: String?
    /// The daemon's own classification, decided at the Mac where the repo, the
    /// worktree and the operator's rules are actually visible. Sent only from
    /// feature level 1; absent on older daemons. Interpreting it is
    /// `RiskAssessment`'s job, not this type's.
    public let risk: WireRisk?
    /// Whether the phone can answer it: on an `AskUserQuestion` card from a
    /// minor-21 daemon, and on every Claude card from minor 22.
    public var questionHold: QuestionHold? = nil
    /// Whether the computer offers "Allow for this session" on this card: a
    /// hosted Claude card whose permission suggestions include a session rule
    /// the daemon would apply. Absent — older daemons, or no such rule — reads
    /// as false; the phone then shows only Allow and Deny.
    public var allowForSession: Bool? = nil

    enum CodingKeys: String, CodingKey {
        case requestID = "request_id"
        case payloadHash = "payload_hash"
        case toolName = "tool_name"
        case toolInput = "tool_input"
        case displayText = "display_text"
        case permissionSuggestions = "permission_suggestions"
        case permissionMode = "permission_mode"
        case risk
        case questionHold = "question_hold"
        case allowForSession = "allow_for_session"
    }
}

/// `protocol::risk::RiskAssessment` — the class plus the rule that produced it.
///
/// `matchedPattern` is present on `high` and absent otherwise, by design at the
/// Mac: a classification you cannot see the reason for is a badge nobody trusts
/// twice, and the cases where it matters are the destructive ones.
public struct WireRisk: Codable, Sendable, Hashable {
    public let cls: String
    public let matchedPattern: String?

    enum CodingKeys: String, CodingKey {
        case cls = "class"
        case matchedPattern = "matched_pattern"
    }
}

// MARK: - Machine stats

/// One sample of a computer's load, as `machine_stats` carries it. Each value
/// is `nil` when the computer could not read it.
public struct MachineStats: Decodable, Sendable, Hashable {
    /// The share of all cores busy over half a second, from 0 to 1.
    public let cpu: Double?
    /// Bytes of memory not available for new work, and of physical memory.
    public let memoryUsed: UInt64?
    public let memoryTotal: UInt64?
    /// Bytes in use on the volume holding the daemon user's home folder, and
    /// its size.
    public let diskUsed: UInt64?
    public let diskTotal: UInt64?
    let capturedAt: String

    enum CodingKeys: String, CodingKey {
        case cpu
        case memoryUsed = "memory_used"
        case memoryTotal = "memory_total"
        case diskUsed = "disk_used"
        case diskTotal = "disk_total"
        case capturedAt = "captured_at"
    }

    public var capturedDate: Date? { ISO8601.parse(capturedAt) }
}

// MARK: - Peers

/// The owner's other computers a daemon can reach on Tailscale, and that
/// computer's own Tailscale state, as `peers` carries them. Every computer
/// listed is the same Tailscale user as the daemon's, untagged and online.
public struct PeerDirectory: Decodable, Sendable, Hashable {
    /// `running`, `not_running` or `not_installed`.
    public let tailnet: String
    /// The daemon's computer's Tailscale login, while Tailscale runs there.
    public let login: String?
    /// The daemon's computer is a tagged device, so every connection to it
    /// needs a code.
    public let tagged: Bool
    public let computers: [PeerComputer]
}

/// One computer in a `peers` frame: where its daemon listens.
public struct PeerComputer: Decodable, Sendable, Hashable {
    /// The first label of its MagicDNS name.
    public let name: String
    /// Its whole MagicDNS name, or its IPv4 address when it has none.
    public let host: String
    public let port: Int

    /// Where to ask its daemon to let this device in, with no credential.
    var endpoint: DaemonEndpoint {
        DaemonEndpoint(host: host, port: port, credential: .tailnet, useTLS: false)
    }
}

// MARK: - Diff

/// Answer to `get_diff`. On-demand only: a diff is a thing you ask for when you
/// are about to read it, never a thing that streams at you.
public struct SessionDiff: Sendable, Hashable {
    public let sessionID: String
    /// `git -C <cwd> diff HEAD` plus untracked names, exactly as the daemon
    /// captured it.
    public let unified: String
    /// The daemon cut the capture: the diff passed its 512KB cap, or the list of
    /// untracked names passed its own. Rendering a truncated diff as if it were
    /// whole is precisely the class of lie this flag exists to prevent.
    public let truncated: Bool
    let capturedAt: String
    /// Why the diff is empty when it is. `unified` is blank both for a clean
    /// tree and for a directory that was never a git repository, and the
    /// daemon's note is the only thing that tells those apart — rendering "no
    /// changes" for the second would be a claim the app cannot support.
    public let note: String?

    public init(sessionID: String, unified: String, truncated: Bool, capturedAt: String, note: String?) {
        self.sessionID = sessionID
        self.unified = unified
        self.truncated = truncated
        self.capturedAt = capturedAt
        self.note = note
    }

    public var capturedDate: Date? { ISO8601.parse(capturedAt) }
}

extension SessionDiff: Decodable {
    private enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case unified
        case truncated
        case capturedAt = "captured_at"
        case note
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        sessionID = try c.decode(String.self, forKey: .sessionID)
        unified = try c.decodeIfPresent(String.self, forKey: .unified) ?? ""
        truncated = try c.decodeIfPresent(Bool.self, forKey: .truncated) ?? false
        capturedAt = try c.decodeIfPresent(String.self, forKey: .capturedAt) ?? ""
        note = try c.decodeIfPresent(String.self, forKey: .note)
    }
}

// MARK: - Client -> server

/// How the phone proves it may talk to this daemon.
public enum HelloCredential: Sendable, Hashable {
    /// A device token from a previous pairing, or the static `codeconnect token`.
    case token(String)
    /// Single-use, 5-minute code straight off the QR. The daemon answers with a
    /// device token, which is what gets stored.
    case pairingCode(String)
    /// No credential: the daemon lets this device in by its Tailscale identity
    /// — the same user as the computer, untagged — and answers with a device
    /// token, which is what gets stored. Anything else it refuses.
    case tailnet
}

/// Everything the phone can say to a daemon.
///
/// `session` on these cases is a **session reference**, which
/// `protocol/src/ws.rs` defines as either a `session_uid` (exact) or a tmux name
/// like `cc-1` (legacy, and resolved by the daemon to the newest run under that
/// name). The app sends the uid whenever the daemon mints them, because a name
/// can resolve to a *different run* than the one the screen is showing — and for
/// `sendText` that would send to the wrong agent.
public enum ClientMessage: Sendable {
    case hello(credential: HelloCredential, clientID: String?, clientName: String?, deviceName: String? = nil)
    case sessions
    case subscribe(session: String, afterSeq: UInt64)
    case unsubscribe(session: String)
    /// One earlier page of a run's log (minor 23, `history_pages`): the newest
    /// `limit` events below `beforeSeq`, as ordinary `event` frames, then one
    /// `history_end` carrying `requestID`. Moves no subscription.
    case history(session: String, beforeSeq: UInt64, limit: UInt32, requestID: String)
    /// Remove one **ended** run's record from the Mac. Minor 7.
    ///
    /// By uid, never by tmux name: a name is handed to the next run, so a stale
    /// `cc-1` could name a session this phone never saw. The daemon refuses
    /// anything still live, in SQL, whatever this end believed.
    case deleteSession(sessionUID: String)
    /// One real APNs notification to this device — the doorbell, proven.
    case testPush(requestID: String)
    /// `session` scopes the answer to one run. Optional on the wire so a
    /// pre-uid daemon still accepts it; when present the daemon refuses to apply
    /// the answer to a different run that happens to be showing a card with the
    /// same `request_id`.
    case answer(
        requestID: String, payloadHash: String, decision: AnswerDecision, session: String?)
    case sendText(
        session: String, text: String, submit: Bool, requestID: String?, payloadHash: String?)
    case getDiff(session: String)
    case getCommandCatalog(session: String)
    /// "Push me here." Sent whenever Apple issues a token, not only at
    /// handshake: permission can be granted mid-session and the token is
    /// reissued on reinstall and on restore-from-backup.
    ///
    /// `relayCredential` is the App Attest-issued bearer, present only for a
    /// relay daemon and only once enrollment has minted it for this exact
    /// `(token, environment)`. Absent for a direct-key daemon, which the relay
    /// path never touches — a `nil` here is the wire's `relay_credential` key
    /// being omitted entirely, so an old daemon that never learned the key is
    /// unaffected.
    case registerPush(token: String, environment: String, relayCredential: String?)
    /// **Stop a Codex session's running turn.** Minor 15 to decode, minor 17 to
    /// honour. All four fields are required — none is defaulted at the Mac.
    ///
    /// `session` is the `session_uid`, always: `payloadHash` is computed over
    /// *the string this field carries* (`state.rs` recomputes with the client's
    /// own spelling), so alternating spellings under one request id is a
    /// self-inflicted `stale payload_hash`.
    case interrupt(session: String, requestID: String, turnID: String, payloadHash: String)
    /// **End a run CodeConnect started**, Claude's or Codex's. Minor 35, sent only when
    /// `Capabilities.endsRuns`. `session` is the run's `session_uid`.
    case stopRun(session: String, requestID: String)
    /// **Say something to a Codex session.** Minor 18, and the one message an
    /// older daemon cannot decode at all — see `Capabilities.codexCompose`.
    case compose(session: String, requestID: String, text: String, payloadHash: String)
    /// **Set a hosted Claude run's model and/or effort** (minor 39). `effort` rides this message, so
    /// a change to effort alone sends `model: nil, effort: X`. At least one of the two is present —
    /// the daemon refuses a change naming neither.
    case setModel(session: String, requestID: String, model: String?, effort: String?)
    /// **Set a hosted Claude run's permission mode** (minor 39), to one its menu lists.
    case setMode(session: String, requestID: String, mode: String, confirmed: Bool)
    /// The folders sessions have run in on this computer. Minor 28, sent only
    /// when `Capabilities.startsSessions`.
    case listProjects
    /// Start an agent in one of those folders. `requestID` is fresh per tap and
    /// resent unchanged after a reconnect: the daemon answers a repeat with the
    /// first outcome and starts nothing new. `path` is a listed path, byte for byte.
    /// `worktree` (minor 29) starts in one of its repository's worktrees, existing
    /// or new; `.folder` sends none, and the folder is never switched. `model`
    /// (minor 33) is the `value` of a model the computer listed for the agent;
    /// nil names none, and the agent starts on its own default. `trust` is the
    /// person's answer to "trust this folder?", kept for the folder; nil gives
    /// none, and a folder with none kept is answered `needsTrust`.
    case startSession(
        requestID: String, path: String, agent: String, prompt: String?, worktree: StartPlace = .folder,
        model: String? = nil, trust: FolderTrust? = nil)
    /// Every folder's answer to "trust this folder?", answered `folderTrust`.
    case listFolderTrust(requestID: String)
    /// Change one folder's answer (`path` as the computer gave it), answered
    /// `folderTrust`.
    case setFolderTrust(requestID: String, path: String, trust: FolderTrust)
    /// Forget one folder's answer, so the next start there asks again; answered
    /// `folderTrust`.
    case forgetFolderTrust(requestID: String, path: String)
    /// One folder's repository: its checkouts and free branches. Minor 29,
    /// sent only when `Capabilities.startsInWorktrees`.
    case getProjectGit(path: String)
    /// One folder's subfolders; nil asks for the home folder. Minor 30, sent
    /// only when `Capabilities.browsesFolders`.
    case listFolder(requestID: String, path: String?, showHidden: Bool)
    /// Make one folder, as `mkdir` does. Minor 30.
    case createFolder(requestID: String, parent: String, name: String)
    /// One sample of the computer's CPU, memory and disk. Minor 31, sent only
    /// when `Capabilities.reportsMachineStats`.
    case getMachineStats
    case ping
}

extension ClientMessage: Encodable {
    private enum CodingKeys: String, CodingKey {
        case type
        case protocolVersion = "protocol_version"
        case token
        case pairingCode = "pairing_code"
        case clientID = "client_id"
        case clientName = "client_name"
        case deviceName = "device_name"
        case sessionID = "session_id"
        case sessionUID = "session_uid"
        case afterSeq = "after_seq"
        case beforeSeq = "before_seq"
        case limit
        case environment
        case relayCredential = "relay_credential"
        case requestID = "request_id"
        case payloadHash = "payload_hash"
        case decision
        case path
        case agent
        case prompt
        case worktree
        case model
        case effort
        case mode
        case confirmed
        case showHidden = "show_hidden"
        case parent
        case name
        case text
        case submit
        case turnID = "turn_id"
        case trust
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .hello(let credential, let clientID, let clientName, let deviceName):
            try c.encode("hello", forKey: .type)
            try c.encode(Wire.protocolVersion, forKey: .protocolVersion)
            switch credential {
            case .token(let token):
                try c.encode(token, forKey: .token)
            case .pairingCode(let code):
                // `token` is omitted, not blanked. The daemon *prefers* a
                // token whenever both are present (`ws.rs`: "one carrying both
                // prefers the token"), so an empty string would be taken as the
                // credential to check and the pairing code would never be
                // looked at — pairing would fail with an authentication error
                // that had nothing to do with the code.
                try c.encode(code, forKey: .pairingCode)
            case .tailnet:
                // Neither key: a hello carrying no credential is the one the
                // daemon checks against Tailscale.
                break
            }
            try c.encodeIfPresent(clientID, forKey: .clientID)
            try c.encodeIfPresent(clientName, forKey: .clientName)
            try c.encodeIfPresent(deviceName, forKey: .deviceName)
        case .getDiff(let session):
            try c.encode("get_diff", forKey: .type)
            try c.encode(session, forKey: .sessionID)
        case .getCommandCatalog(let session):
            try c.encode("get_command_catalog", forKey: .type)
            try c.encode(session, forKey: .sessionID)
        case .sessions:
            try c.encode("sessions", forKey: .type)
        case .subscribe(let session, let afterSeq):
            try c.encode("subscribe", forKey: .type)
            try c.encode(session, forKey: .sessionID)
            try c.encode(afterSeq, forKey: .afterSeq)
        case .unsubscribe(let session):
            try c.encode("unsubscribe", forKey: .type)
            try c.encode(session, forKey: .sessionID)
        case .history(let session, let beforeSeq, let limit, let requestID):
            try c.encode("history", forKey: .type)
            try c.encode(session, forKey: .sessionID)
            try c.encode(beforeSeq, forKey: .beforeSeq)
            try c.encode(limit, forKey: .limit)
            try c.encode(requestID, forKey: .requestID)
        case .deleteSession(let sessionUID):
            try c.encode("delete_session", forKey: .type)
            try c.encode(sessionUID, forKey: .sessionUID)
        case .testPush(let requestID):
            try c.encode("test_push", forKey: .type)
            try c.encode(requestID, forKey: .requestID)
        case .answer(let requestID, let payloadHash, let decision, let session):
            try c.encode("answer", forKey: .type)
            try c.encode(requestID, forKey: .requestID)
            try c.encode(payloadHash, forKey: .payloadHash)
            try c.encode(decision, forKey: .decision)
            // Omitted rather than sent as null when there is nothing to scope
            // to: `ws.rs` reads `Option<String>`, and a null would decode as
            // "no scope" anyway while making the frame wrong to read.
            try c.encodeIfPresent(session, forKey: .sessionID)
        case .sendText(let session, let text, let submit, let requestID, let payloadHash):
            try c.encode("send_text", forKey: .type)
            try c.encode(session, forKey: .sessionID)
            try c.encode(text, forKey: .text)
            try c.encode(submit, forKey: .submit)
            // Omitted, never null: the daemon reads `Option<String>` and a
            // null would say "present but empty".
            try c.encodeIfPresent(requestID, forKey: .requestID)
            try c.encodeIfPresent(payloadHash, forKey: .payloadHash)
        case .registerPush(let token, let environment, let relayCredential):
            try c.encode("register_push", forKey: .type)
            try c.encode(token, forKey: .token)
            try c.encode(environment, forKey: .environment)
            // Omitted, never null, when absent: an old daemon's `deny_unknown`
            // is not in play here, but a missing key is what "direct, no
            // credential" means on the wire, and a present-but-null would read
            // as "relay, credential lost".
            try c.encodeIfPresent(relayCredential, forKey: .relayCredential)
        case .interrupt(let session, let requestID, let turnID, let payloadHash):
            try c.encode("interrupt", forKey: .type)
            // All four required at the Mac, so all four are plain `encode` —
            // an omitted key here is a frame the daemon rejects outright.
            try c.encode(session, forKey: .sessionID)
            try c.encode(requestID, forKey: .requestID)
            try c.encode(turnID, forKey: .turnID)
            try c.encode(payloadHash, forKey: .payloadHash)
        case .stopRun(let session, let requestID):
            try c.encode("stop_run", forKey: .type)
            try c.encode(session, forKey: .sessionID)
            try c.encode(requestID, forKey: .requestID)
        case .compose(let session, let requestID, let text, let payloadHash):
            try c.encode("compose", forKey: .type)
            try c.encode(session, forKey: .sessionID)
            try c.encode(requestID, forKey: .requestID)
            try c.encode(text, forKey: .text)
            try c.encode(payloadHash, forKey: .payloadHash)
        case .setModel(let session, let requestID, let model, let effort):
            try c.encode("set_model", forKey: .type)
            try c.encode(session, forKey: .sessionID)
            try c.encode(requestID, forKey: .requestID)
            // Omitted, never null: the daemon reads `Option<String>` with
            // `skip_serializing_if`, and a change naming neither is refused by name.
            try c.encodeIfPresent(model, forKey: .model)
            try c.encodeIfPresent(effort, forKey: .effort)
        case .setMode(let session, let requestID, let mode, let confirmed):
            try c.encode("set_mode", forKey: .type)
            try c.encode(session, forKey: .sessionID)
            try c.encode(requestID, forKey: .requestID)
            try c.encode(mode, forKey: .mode)
            // The person's confirmation of a mode the menu marks `confirm`; a bare request omits it,
            // which the daemon refuses for a mode that needs it.
            if confirmed { try c.encode(true, forKey: .confirmed) }
        case .listProjects:
            try c.encode("list_projects", forKey: .type)
        case .startSession(let requestID, let path, let agent, let prompt, let worktree, let model, let trust):
            try c.encode("start_session", forKey: .type)
            try c.encode(requestID, forKey: .requestID)
            try c.encode(path, forKey: .path)
            try c.encode(agent, forKey: .agent)
            try c.encodeIfPresent(prompt, forKey: .prompt)
            let request: [String: String]? =
                switch worktree {
                case .folder: nil
                case .existing(let checkout): ["kind": "existing", "path": checkout.path]
                case .branch(let branch): ["kind": "branch", "branch": branch]
                case .new(let name): ["kind": "new", "name": name]
                }
            try c.encodeIfPresent(request, forKey: .worktree)
            try c.encodeIfPresent(model, forKey: .model)
            try c.encodeIfPresent(trust, forKey: .trust)
        case .listFolderTrust(let requestID):
            try c.encode("list_folder_trust", forKey: .type)
            try c.encode(requestID, forKey: .requestID)
        case .setFolderTrust(let requestID, let path, let trust):
            try c.encode("set_folder_trust", forKey: .type)
            try c.encode(requestID, forKey: .requestID)
            try c.encode(path, forKey: .path)
            try c.encode(trust, forKey: .trust)
        case .forgetFolderTrust(let requestID, let path):
            try c.encode("forget_folder_trust", forKey: .type)
            try c.encode(requestID, forKey: .requestID)
            try c.encode(path, forKey: .path)
        case .getProjectGit(let path):
            try c.encode("get_project_git", forKey: .type)
            try c.encode(path, forKey: .path)
        case .listFolder(let requestID, let path, let showHidden):
            try c.encode("list_folder", forKey: .type)
            try c.encode(requestID, forKey: .requestID)
            try c.encodeIfPresent(path, forKey: .path)
            // Absent means false, as the daemon reads it.
            if showHidden { try c.encode(true, forKey: .showHidden) }
        case .createFolder(let requestID, let parent, let name):
            try c.encode("create_folder", forKey: .type)
            try c.encode(requestID, forKey: .requestID)
            try c.encode(parent, forKey: .parent)
            try c.encode(name, forKey: .name)
        case .getMachineStats:
            try c.encode("get_machine_stats", forKey: .type)
        case .ping:
            try c.encode("ping", forKey: .type)
        }
    }
}

// MARK: - Server -> client

/// Everything `hello_ack` carries. A struct rather than eight associated values:
/// the ack has already grown by four fields once and will grow again.
public struct HelloAck: Sendable, Hashable {
    public var protocolVersion: UInt32
    /// Additive feature level. `1` advertises `turn_complete`, `get_diff`,
    /// pairing codes and daemon-side risk classification. Additions that break
    /// nothing leave `protocolVersion` alone, so this is the field that actually
    /// distinguishes a newer daemon from an older one.
    public var protocolMinor: UInt32
    public var serverTime: String
    public var capabilities: Capabilities
    /// Present exactly once: in the ack for a `pairing_code` hello. The durable
    /// credential; the code that bought it is spent.
    public var deviceToken: String?
    public var deviceID: String?
    /// What `codeconnect devices` lists this phone as, and what `codeconnect revoke` takes. May
    /// differ from the requested name when that one was taken.
    public var deviceName: String?
    /// The daemon's current authoritative APNs environment for this device's
    /// registered token, absent when no token is registered. Minor 14. The relay
    /// binding is the single authority for a token's environment (§4); a relay
    /// send against the wrong advisory environment is corrected downstream and
    /// the daemon CAS-persists the truth, then reports it here. The app compares
    /// this to its cached tuple on every handshake and **persists a difference
    /// rather than resending its stale value** — the daemon→app correction path.
    public var pushEnvironment: String?
    /// What the owner calls the computer the daemon runs on (a Mac's computer
    /// name). Minor 25; absent from an older daemon, or when it could not be read.
    public var machineName: String?
    /// `"macos"` or `"linux"`. Minor 25; absent when the daemon does not say.
    public var machineOS: String?
    /// Which daemon this is, the same whatever address reached it. Minor 26;
    /// absent from an older daemon, which is told apart by address instead.
    public var machineID: String?

    public init(
        protocolVersion: UInt32, protocolMinor: UInt32, serverTime: String, capabilities: Capabilities,
        deviceToken: String? = nil, deviceID: String? = nil, deviceName: String? = nil,
        pushEnvironment: String? = nil, machineName: String? = nil, machineOS: String? = nil,
        machineID: String? = nil
    ) {
        self.protocolVersion = protocolVersion
        self.protocolMinor = protocolMinor
        self.serverTime = serverTime
        self.capabilities = capabilities
        self.deviceToken = deviceToken
        self.deviceID = deviceID
        self.deviceName = deviceName
        self.pushEnvironment = pushEnvironment
        self.machineName = machineName
        self.machineOS = machineOS
        self.machineID = machineID
    }
}

/// What became of a `delete_session`.
///
/// Typed rather than a bare ack: "there was nothing there", "it is still
/// running, so no" and "I tried and could not" are different answers, and only
/// the first two mean the phone may forget the run.
///
/// **`Decodable`, not `Codable`.** This only ever arrives; nothing in the app
/// sends one. An encoder here would also be a trap rather than dead weight: it
/// cannot round-trip, because `.unknown(status: "deleted")` would encode as
/// `{"status":"deleted"}` and decode back as `.deleted` — collapsing the exact
/// distinction the `unknown` case exists to hold.
public enum DeleteSessionResult: Decodable, Sendable, Hashable {
    case deleted(events: UInt64)
    /// The daemon refused because the run is alive, or because it is still
    /// holding live state for it. Its rule, not ours.
    case stillRunning
    /// Refused, and the daemon cannot say the run is alive either — it never
    /// established what happened to it. Not folded into `stillRunning`, because
    /// that one asserts the agent is there and this one asserts nothing.
    case notExited(lifecycle: String)
    /// Already gone. Two phones swiping the same row is a race, not an error.
    case notFound
    /// The daemon tried and could not.
    case failed(message: String)
    /// A status this build has never heard of, from a daemon ahead of it.
    ///
    /// **Its own case, and deliberately not folded into `notFound`.** Every other
    /// unknown on this wire is decoded to something inert; this one used to decode
    /// to the single most destructive answer in the enum, so a future daemon
    /// inventing any new status would have wiped the row and its cache locally
    /// while never having said the session was gone. Only the literal `not_found`
    /// may mean that.
    case unknown(status: String)

    private enum CodingKeys: String, CodingKey { case status, events, message, lifecycle }

    /// True only for the two answers that mean the Mac does not have this run.
    /// The one place the destructive reading is decided, so it cannot drift.
    public var meansItIsGone: Bool {
        switch self {
        case .deleted, .notFound: true
        case .stillRunning, .notExited, .failed, .unknown: false
        }
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let status = try c.decode(String.self, forKey: .status)
        switch status {
        case "deleted":
            self = .deleted(events: try c.decodeIfPresent(UInt64.self, forKey: .events) ?? 0)
        case "still_running": self = .stillRunning
        case "not_exited":
            self = .notExited(
                lifecycle: try c.decodeIfPresent(String.self, forKey: .lifecycle) ?? "unknown")
        case "not_found": self = .notFound
        case "failed":
            self = .failed(
                message: try c.decodeIfPresent(String.self, forKey: .message)
                    ?? "\(HostDevice.current.computer("The computer")) could not remove that session.")
        default: self = .unknown(status: status)
        }
    }
}

/// What became of a `test_push`. Decode-only, like every reply; `accepted`
/// claims exactly what Apple's 200 proves — accepted for delivery, with the
/// banner as the device's own final word. An unknown status stays inert.
public enum TestPushResult: Decodable, Sendable, Hashable {
    case accepted(apnsID: String?)
    case pushUnconfigured
    case notPairedDevice
    case noRegisteredToken
    /// The relay refused the daemon's bearer for this token — expired, revoked,
    /// below the generation floor, or bound to a different token. The token
    /// itself is intact; the phone must re-enroll to mint a fresh credential.
    /// Distinct from `noRegisteredToken`, which means the Mac holds no token at
    /// all. Minor 14.
    case credentialInvalid
    case rateLimited(retryAfterSecs: UInt32)
    case failed(reason: String)
    case unknown(status: String)

    private enum CodingKeys: String, CodingKey {
        case status
        case apnsID = "apns_id"
        case retryAfterSecs = "retry_after_secs"
        case reason
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let status = try c.decode(String.self, forKey: .status)
        switch status {
        case "accepted":
            self = .accepted(apnsID: try c.decodeIfPresent(String.self, forKey: .apnsID))
        case "push_unconfigured": self = .pushUnconfigured
        case "not_paired_device": self = .notPairedDevice
        case "no_registered_token": self = .noRegisteredToken
        case "credential_invalid": self = .credentialInvalid
        case "rate_limited":
            self = .rateLimited(
                retryAfterSecs: try c.decodeIfPresent(UInt32.self, forKey: .retryAfterSecs) ?? 30)
        case "failed":
            self = .failed(
                reason: try c.decodeIfPresent(String.self, forKey: .reason)
                    ?? "\(HostDevice.current.computer("The computer")) could not send the test.")
        default: self = .unknown(status: status)
        }
    }
}

/// A Claude run's own slash commands, as the run reported them.
public enum CommandCatalogResult: Sendable, Hashable {
    /// The run's own inventory — names without the leading slash, exactly as
    /// it reported them. `probedAt` is when the run reported that list; a
    /// cache hit keeps the original stamp because the age of a fact is part
    /// of the fact.
    case available(commands: [String], claudeVersion: String?, probedAt: String)
    /// A complete answer, not an error: the phone falls back to its
    /// conservative static policy, never to guessing.
    case unavailable(reason: String)
}

extension CommandCatalogResult: Codable {
    private enum CodingKeys: String, CodingKey {
        case status, commands, reason
        case claudeVersion = "claude_version"
        case probedAt = "probed_at"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .status) {
        case "available":
            self = .available(
                commands: try c.decode([String].self, forKey: .commands),
                claudeVersion: try c.decodeIfPresent(String.self, forKey: .claudeVersion),
                probedAt: try c.decode(String.self, forKey: .probedAt))
        case "unavailable":
            self = .unavailable(reason: try c.decode(String.self, forKey: .reason))
        case let other:
            self = .unavailable(reason: "unrecognised catalog status: \(other)")
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .available(let commands, let claudeVersion, let probedAt):
            try c.encode("available", forKey: .status)
            try c.encode(commands, forKey: .commands)
            try c.encodeIfPresent(claudeVersion, forKey: .claudeVersion)
            try c.encode(probedAt, forKey: .probedAt)
        case .unavailable(let reason):
            try c.encode("unavailable", forKey: .status)
            try c.encode(reason, forKey: .reason)
        }
    }
}

public enum ServerMessage: Sendable {
    case helloAck(HelloAck)
    case sessions([SessionSummary])
    case event(Event)
    /// The end of one `history` page, after its events (minor 23). `sessionID`
    /// and `requestID` are echoed as the phone sent them. `firstSeq` is the
    /// lowest seq the page sent, or `beforeSeq` when it sent none; `error` says
    /// why a page is incomplete, and such a page may have sent some events first.
    case historyEnd(
        sessionID: String, requestID: String, beforeSeq: UInt64, firstSeq: UInt64, error: String?)
    case answerResult(requestID: String, result: AnswerResult)
    case sendTextResult(sessionID: String, result: SendTextResult)
    case commandCatalog(sessionID: String, result: CommandCatalogResult)
    case deleteSessionResult(sessionUID: String, result: DeleteSessionResult)
    case testPushResult(requestID: String, result: TestPushResult)
    case diff(SessionDiff)
    /// The answer to one `interrupt`. `sessionID` and `requestID` are echoed
    /// back as the phone sent them, so correlation is exact-string on the id
    /// this end minted.
    case interruptResult(sessionID: String, requestID: String, result: InterruptResult)
    /// The answer to one `stop_run`, same correlation.
    case stopRunResult(sessionID: String, requestID: String, result: StopRunResult)
    /// The answer to one `compose`, same correlation.
    case composeResult(sessionID: String, requestID: String, result: ComposeResult)
    case settingResult(sessionID: String, requestID: String, result: SettingResult)
    /// The answer to `list_projects`: most recent first, and which agents the
    /// computer has.
    case projects(projects: [StartProject], agents: [StartAgentStatus])
    case startSessionResult(requestID: String, outcome: StartOutcome)
    /// The answer to `list_folder_trust`, `set_folder_trust` and
    /// `forget_folder_trust`: every answer the computer keeps, newest first, once
    /// the request's change is made.
    case folderTrust(requestID: String, folders: [FolderAnswer])
    /// The answer to `get_project_git`: nil when the folder is not a repository.
    case projectGit(path: String, git: StartGit?)
    /// The answer to `list_folder`.
    case folderListing(requestID: String, listing: StartFolderListing)
    /// The answer to `create_folder`: nil once made, else the computer's own
    /// words.
    case createFolderResult(requestID: String, refusal: String?)
    /// The answer to `get_machine_stats`.
    case machineStats(MachineStats)
    /// The daemon's list of the owner's other computers, after `hello_ack` and
    /// whenever it changes.
    case peers(PeerDirectory)
    case error(code: String, message: String)
    case pong
    /// A message type this build does not know. Kept rather than thrown away so
    /// the connection survives a daemon that is ahead of the app.
    case unknown(type: String)
}

extension ServerMessage: Decodable {
    private enum CodingKeys: String, CodingKey {
        case type
        case protocolVersion = "protocol_version"
        case serverTime = "server_time"
        case capabilities
        case protocolMinor = "protocol_minor"
        case deviceToken = "device_token"
        case deviceID = "device_id"
        case sessionUID = "session_uid"
        case deviceName = "device_name"
        case pushEnvironment = "push_environment"
        case machineName = "machine_name"
        case machineOS = "machine_os"
        case machineID = "machine_id"
        case sessions
        case event
        case requestID = "request_id"
        case sessionID = "session_id"
        case result
        case code
        case message
        case beforeSeq = "before_seq"
        case firstSeq = "first_seq"
        case error
        case projects
        case agents
        case path
        case git
        case folders
        case truncated
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .type) {
        case "hello_ack":
            self = .helloAck(
                HelloAck(
                    protocolVersion: try c.decode(UInt32.self, forKey: .protocolVersion),
                    protocolMinor: try c.decodeIfPresent(UInt32.self, forKey: .protocolMinor) ?? 0,
                    serverTime: try c.decodeIfPresent(String.self, forKey: .serverTime) ?? "",
                    capabilities: try c.decodeIfPresent(Capabilities.self, forKey: .capabilities)
                        ?? Capabilities(),
                    deviceToken: try c.decodeIfPresent(String.self, forKey: .deviceToken),
                    deviceID: try c.decodeIfPresent(String.self, forKey: .deviceID),
                    deviceName: try c.decodeIfPresent(String.self, forKey: .deviceName),
                    pushEnvironment: try c.decodeIfPresent(String.self, forKey: .pushEnvironment),
                    machineName: try c.decodeIfPresent(String.self, forKey: .machineName),
                    machineOS: try c.decodeIfPresent(String.self, forKey: .machineOS),
                    machineID: try c.decodeIfPresent(String.self, forKey: .machineID)))
        case "diff":
            self = .diff(try SessionDiff(from: decoder))
        case "sessions":
            self = .sessions(try c.decode([SessionSummary].self, forKey: .sessions))
        case "event":
            self = .event(try c.decode(Event.self, forKey: .event))
        case "history_end":
            self = .historyEnd(
                sessionID: try c.decode(String.self, forKey: .sessionID),
                requestID: try c.decode(String.self, forKey: .requestID),
                beforeSeq: try c.decode(UInt64.self, forKey: .beforeSeq),
                firstSeq: try c.decode(UInt64.self, forKey: .firstSeq),
                error: try c.decodeIfPresent(String.self, forKey: .error))
        case "answer_result":
            self = .answerResult(
                requestID: try c.decode(String.self, forKey: .requestID),
                result: try c.decode(AnswerResult.self, forKey: .result))
        case "send_text_result":
            self = .sendTextResult(
                sessionID: try c.decode(String.self, forKey: .sessionID),
                result: try c.decode(SendTextResult.self, forKey: .result))
        case "test_push_result":
            self = .testPushResult(
                requestID: try c.decode(String.self, forKey: .requestID),
                result: try c.decode(TestPushResult.self, forKey: .result))
        case "delete_session_result":
            self = .deleteSessionResult(
                sessionUID: try c.decode(String.self, forKey: .sessionUID),
                result: try c.decode(DeleteSessionResult.self, forKey: .result))
        case "command_catalog":
            self = .commandCatalog(
                sessionID: try c.decode(String.self, forKey: .sessionID),
                result: try c.decode(CommandCatalogResult.self, forKey: .result))
        case "error":
            self = .error(
                code: try c.decode(String.self, forKey: .code),
                message: try c.decode(String.self, forKey: .message))
        case "interrupt_result":
            self = .interruptResult(
                sessionID: try c.decode(String.self, forKey: .sessionID),
                requestID: try c.decode(String.self, forKey: .requestID),
                result: try c.decode(InterruptResult.self, forKey: .result))
        case "stop_run_result":
            self = .stopRunResult(
                sessionID: try c.decode(String.self, forKey: .sessionID),
                requestID: try c.decode(String.self, forKey: .requestID),
                result: try c.decode(StopRunResult.self, forKey: .result))
        case "compose_result":
            self = .composeResult(
                sessionID: try c.decode(String.self, forKey: .sessionID),
                requestID: try c.decode(String.self, forKey: .requestID),
                result: try c.decode(ComposeResult.self, forKey: .result))
        case "setting_result":
            self = .settingResult(
                sessionID: try c.decode(String.self, forKey: .sessionID),
                requestID: try c.decode(String.self, forKey: .requestID),
                result: try c.decode(SettingResult.self, forKey: .result))
        case "projects":
            self = .projects(
                projects: try c.decode([StartProject].self, forKey: .projects),
                // An agent this build has never heard of cannot be offered.
                agents: try c.decode([StartAgentStatus.Wire].self, forKey: .agents)
                    .compactMap(\.status))
        case "start_session_result":
            self = .startSessionResult(
                requestID: try c.decode(String.self, forKey: .requestID),
                outcome: try c.decode(StartOutcome.self, forKey: .result))
        case "folder_trust":
            self = .folderTrust(
                requestID: try c.decode(String.self, forKey: .requestID),
                folders: try c.decode([FolderAnswer].self, forKey: .folders))
        case "project_git":
            self = .projectGit(
                path: try c.decode(String.self, forKey: .path),
                git: try c.decodeIfPresent(StartGit.self, forKey: .git))
        case "folder_listing":
            self = .folderListing(
                requestID: try c.decode(String.self, forKey: .requestID),
                listing: StartFolderListing(
                    path: try c.decode(String.self, forKey: .path),
                    folders: try c.decodeIfPresent([String].self, forKey: .folders) ?? [],
                    truncated: try c.decodeIfPresent(Bool.self, forKey: .truncated) ?? false,
                    error: try c.decodeIfPresent(String.self, forKey: .error)))
        case "create_folder_result":
            let result = try c.decode(StartCreateFolderResult.self, forKey: .result)
            self = .createFolderResult(
                requestID: try c.decode(String.self, forKey: .requestID),
                refusal: result.status == "created" ? nil : result.detail ?? result.status)
        case "machine_stats":
            self = .machineStats(try MachineStats(from: decoder))
        case "peers":
            self = .peers(try PeerDirectory(from: decoder))
        case "pong":
            self = .pong
        case let other:
            self = .unknown(type: other)
        }
    }
}

// MARK: - Timestamps

public enum ISO8601 {
    /// The daemon emits RFC3339 UTC with millisecond precision, but a strategy
    /// that assumes fractional seconds fails on a whole-second timestamp, so
    /// both are tried. `ISO8601FormatStyle` is a value type — unlike
    /// `ISO8601DateFormatter` it can be shared across tasks safely.
    private static let withFraction = Date.ISO8601FormatStyle(includingFractionalSeconds: true)
    private static let plain = Date.ISO8601FormatStyle()

    public static func parse(_ string: String) -> Date? {
        (try? withFraction.parse(string)) ?? (try? plain.parse(string))
    }
}
