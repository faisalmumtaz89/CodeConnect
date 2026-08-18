import Foundation

/// Swift mirror of `mac/protocol/src/{ws,event,ipc}.rs`.
///
/// Hand-written `Codable` conformances throughout, for two reasons:
///   * the Rust enums are internally tagged (`type` / `status` / `mode`), which
///     Swift's synthesised conformance cannot express; and
///   * `JSONDecoder.keyDecodingStrategy` also rewrites the keys of
///     `[String: JSONValue]`, which would silently mangle every event payload —
///     so every key is spelled out instead.
enum Wire {
    static let protocolVersion: UInt32 = 1
    /// Matches `MAX_CLIENT_MESSAGE_BYTES`; the daemon closes anything larger.
    static let maxClientMessageBytes = 1024 * 1024

    /// The live terminal's flow control, mirroring `protocol/src/ws.rs`. Bytes
    /// ride as base64 inside the JSON frames and each direction is governed by a
    /// credit window: a grant of `n` lets the peer send `n` more decoded bytes,
    /// and credit is returned only once those bytes have been consumed — for
    /// this side, once they have been fed to the terminal view. Exceeding a
    /// window is a protocol error the daemon closes the terminal for, so these
    /// numbers must match the Mac's exactly.
    ///
    /// **The two ceilings are now advertised, and these are the fallback.**
    /// `terminal_attached` carries `max_chunk_bytes` and
    /// `max_outstanding_credit`, and a carrier enforces what the daemon it is
    /// talking to actually said. Hand-mirrored numbers were a version-skew trap:
    /// they are enforced as protocol errors, so raising either at the Mac would
    /// have killed the terminal of every phone still compiled against the old
    /// pair. They stay here for three jobs that outlive the advertisement: the
    /// window before it arrives (a `terminal_attach` names its own output
    /// credit, and it is sent before any ack), the local queue bound, and the
    /// parity test that reads the Mac's source. They are *not* here for an
    /// older daemon: the Mac makes both fields required, and a daemon too old
    /// to carry them is one that never sends `terminal_attached` at all,
    /// because it does not advertise the capability the phone attaches on.
    enum Terminal {
        /// The largest decoded chunk one `terminal_input` may carry.
        static let maxChunkBytes = 16 * 1024
        /// What this phone grants the daemon at attach, and the ceiling it will
        /// let outstanding credit reach in either direction.
        static let initialOutputCredit: UInt32 = 64 * 1024
        /// What the daemon grants *this phone* at attach. Not this side's to
        /// choose — it arrives in `terminal_attached` — but pinned here so a
        /// test can drive a carrier with the window a real daemon opens rather
        /// than one that happens to be to hand.
        static let initialInputCredit: UInt32 = 32 * 1024
        static let maxOutstandingCredit: UInt32 = 256 * 1024
        /// Geometry the daemon accepts; anything outside is refused.
        static let minCols = 2
        static let maxCols = 512
        static let minRows = 2
        static let maxRows = 256
    }
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
enum PushMode: Equatable, Sendable {
    case none
    case direct
    case relay
}

/// Where an event came from — a *provenance*, so an unrecognised source must
/// never inherit the trusted `.daemon` label. `.daemon` used to be the
/// fallback, which meant a newer daemon's source this build had never heard of
/// was silently attributed to the daemon itself. Modelled on `AnswerPath`: a
/// tagged single-value string with the unknown retained in its own case.
enum EventSource: Sendable, Hashable {
    case hook, transcript, daemon, pty
    /// A source a newer daemon knows about and this build does not. Retained
    /// rather than coerced, so nothing reads it as the trusted `.daemon`.
    case unknown(String)

    var rawValue: String {
        switch self {
        case .hook: return "hook"
        case .transcript: return "transcript"
        case .daemon: return "daemon"
        case .pty: return "pty"
        case .unknown(let raw): return raw
        }
    }
}

extension EventSource: Codable {
    init(from decoder: Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "hook": self = .hook
        case "transcript": self = .transcript
        case "daemon": self = .daemon
        case "pty": self = .pty
        case let other: self = .unknown(other)
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

enum Lifecycle: String, Codable, Sendable, Hashable {
    case spawning, live, exited, unknown

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = Lifecycle(rawValue: raw) ?? .unknown
    }
}

/// Freshness of *our observation*, never the agent's state: `detached` means
/// this app has lost sight of the session, not that the agent stopped working.
/// The two must never be conflated on any surface.
enum Link: String, Codable, Sendable, Hashable {
    case attached, degraded, detached, stale

    /// An unrecognised link word from a newer daemon degrades rather than
    /// pretending to be attached — claiming liveness we cannot justify is the
    /// one thing this enum exists to prevent.
    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = Link(rawValue: raw) ?? .degraded
    }
}

enum ResolvedBy: String, Codable, Sendable, Hashable {
    case phone, local, timeout, superseded

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = ResolvedBy(rawValue: raw) ?? .superseded
    }
}

/// How an answer was actually applied — an *actuation claim*, so an unknown
/// wire value must never collapse into one this build would read as "we typed
/// it". `.sendKeys` used to be the fallback, which meant a newer daemon's path
/// this build had never heard of was silently reported as keystrokes it never
/// sent. Modelled on `AnswerDecision`: a tagged single-value string, with the
/// unknown retained in its own case rather than folded into a known one.
enum AnswerPath: Sendable, Hashable {
    case hookReturn
    case sendKeys
    /// A path a newer daemon knows about and this build does not. Never claims
    /// an actuation: a reader asking "did we type this?" must treat it as *not*
    /// `send_keys`, because this build cannot vouch for how the answer landed.
    case unknown(String)

    var rawValue: String {
        switch self {
        case .hookReturn: return "hook_return"
        case .sendKeys: return "send_keys"
        case .unknown(let raw): return raw
        }
    }

    /// Build from a wire string outside a decoder — used by `Capabilities`,
    /// which reads `answer_path` out of its verbatim map. An unrecognised value
    /// is retained, never coerced to `.sendKeys`.
    init(wire raw: String) {
        switch raw {
        case "hook_return": self = .hookReturn
        case "send_keys": self = .sendKeys
        case let other: self = .unknown(other)
        }
    }
}

extension AnswerPath: Codable {
    init(from decoder: Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "hook_return": self = .hookReturn
        case "send_keys": self = .sendKeys
        case let other: self = .unknown(other)
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

/// `EventKind::Other` is load-bearing: an unknown kind from a newer daemon has
/// to survive a round-trip rather than being dropped.
enum EventKind: Sendable, Hashable {
    case sessionStart, sessionEnd
    /// A feature-level-1 daemon's Stop-hook fact: the *turn* finished, the
    /// session lives on. Older daemons sent the same fact as `session_end`
    /// carrying `hook_event_name: "Stop"`, which is why `Event.isTurnComplete`
    /// still recognises that shape.
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
    init(from decoder: Decoder) throws {
        self.init(rawValue: try decoder.singleValueContainer().decode(String.self))
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

// MARK: - Event

struct Event: Codable, Sendable, Hashable, Identifiable {
    let seq: UInt64
    /// The run's identity — a ULID minted at spawn and never reused. Empty from
    /// a daemon older than protocol minor 2, which had no such concept.
    let sessionUID: String
    /// The tmux name (`cc-1`). Display and `tmux attach` only: it is handed to
    /// the *next* session when this one exits, so it is not an identity.
    let sessionID: String
    let ts: String
    let kind: EventKind
    let payload: JSONValue
    let source: EventSource
    let sourceEventID: String?
    let turnID: String?
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
    init(from decoder: Decoder) throws {
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
    var sessionKey: String { sessionUID.isEmpty ? sessionID : sessionUID }

    /// `seq` alone is not unique for the daemon's `Resync` marker, which is sent
    /// with `seq: 0` precisely so it cannot be mistaken for a logged fact. The
    /// *key* rather than the name, so two runs that shared `cc-1` cannot produce
    /// two different events with one id.
    var id: String { "\(sessionKey)#\(seq)#\(ts)#\(kind.rawValue)" }

    var date: Date { ISO8601.parse(ts) ?? .distantPast }
}

// MARK: - Session summary

struct SessionSummary: Codable, Sendable, Hashable, Identifiable {
    /// The run's identity, minted once at spawn. Empty on a daemon older than
    /// protocol minor 2. Two entries in one `sessions` list can share a
    /// `sessionID` when a tmux name has been reused, and only this tells them
    /// apart — see the "Session identity" section of `mac/README.md`.
    let sessionUID: String
    /// The tmux name. Display and `tmux attach` only.
    let sessionID: String
    let tmuxSession: String
    let cwd: String
    let lifecycle: Lifecycle
    let link: Link
    let claudeSessionID: String?
    let transcriptPath: String?
    let lastSeq: UInt64
    let createdAt: String
    let updatedAt: String
    let blockedOn: [String]
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
    let projectLabel: String

    enum CodingKeys: String, CodingKey {
        case sessionUID = "session_uid"
        case sessionID = "session_id"
        case tmuxSession = "tmux_session"
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
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        sessionUID = try c.decodeIfPresent(String.self, forKey: .sessionUID) ?? ""
        sessionID = try c.decode(String.self, forKey: .sessionID)
        tmuxSession = try c.decode(String.self, forKey: .tmuxSession)
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
    }

    /// What this app files the run's events, subscriptions and marks under, and
    /// what it sends when it has to name the run on the wire. See
    /// `Event.sessionKey` for why it is read off the data rather than off a flag.
    var sessionKey: String { sessionUID.isEmpty ? sessionID : sessionUID }

    /// Whether the phone may offer to remove this run.
    ///
    /// Two routes, matching the daemon's own delete rule exactly. A **hosted**
    /// run must be proven `exited` — `lifecycle`, never the derived Ended
    /// status. An **unhosted** run — adopted, empty `tmux_session`, nothing
    /// ever put it in tmux — is removable at any lifecycle, because no probe
    /// can ever prove it ended and a row awaiting an unobtainable proof would
    /// be immortal.
    var isRemovable: Bool { lifecycle == .exited || tmuxSession.isEmpty }

    var id: String { sessionKey }
    var updatedDate: Date { ISO8601.parse(updatedAt) ?? .distantPast }
    var createdDate: Date { ISO8601.parse(createdAt) ?? .distantPast }
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
struct Capabilities: Codable, Sendable, Hashable {
    let canApproveReliably: Bool
    let failMode: String
    let answerPath: AnswerPath
    let holdSecs: UInt64
    let sendText: Bool
    let capture: Bool
    let push: Bool
    /// A minor-14 relay daemon advertises this instead of `push`: it sends
    /// through the CodeConnect relay, which needs an enrolled credential the
    /// legacy token-only flow cannot supply. A relay daemon advertises `push =
    /// false` on purpose so an older app never prompts and registers a
    /// credential-less token. See `pushMode`.
    let pushRelay: Bool
    let tls: Bool
    /// Exactly what arrived, unknown keys included.
    let advertised: [String: JSONValue]

    init(
        canApproveReliably: Bool = false,
        failMode: String = "unknown",
        answerPath: AnswerPath = .sendKeys,
        holdSecs: UInt64 = 0,
        sendText: Bool = false,
        capture: Bool = false,
        push: Bool = false,
        pushRelay: Bool = false,
        tls: Bool = false,
        extra: [String: JSONValue] = [:]
    ) {
        self.canApproveReliably = canApproveReliably
        self.failMode = failMode
        self.answerPath = answerPath
        self.holdSecs = holdSecs
        self.sendText = sendText
        self.capture = capture
        self.push = push
        self.pushRelay = pushRelay
        self.tls = tls
        var map = extra
        map["can_approve_reliably"] = .bool(canApproveReliably)
        map["fail_mode"] = .string(failMode)
        map["answer_path"] = .string(answerPath.rawValue)
        map["hold_secs"] = .int(Int64(holdSecs))
        map["send_text"] = .bool(sendText)
        map["capture"] = .bool(capture)
        map["push"] = .bool(push)
        map["push_relay"] = .bool(pushRelay)
        map["tls"] = .bool(tls)
        advertised = map
    }

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode([String: JSONValue].self)
        advertised = raw
        canApproveReliably = raw["can_approve_reliably"]?.boolValue ?? false
        failMode = raw["fail_mode"]?.stringValue ?? "unknown"
        answerPath =
            raw["answer_path"]?.stringValue.map(AnswerPath.init(wire:)) ?? .sendKeys
        holdSecs = (raw["hold_secs"]?.intValue).map { UInt64(max(0, $0)) } ?? 0
        sendText = raw["send_text"]?.boolValue ?? false
        capture = raw["capture"]?.boolValue ?? false
        push = raw["push"]?.boolValue ?? false
        pushRelay = raw["push_relay"]?.boolValue ?? false
        tls = raw["tls"]?.boolValue ?? false
    }

    func encode(to encoder: Encoder) throws {
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
    var servesDiff: Bool { advertises(["diff", "get_diff", "diffs"]) }
    /// `send_text` accepts `request_id` + `payload_hash` and replays a retry's
    /// original outcome instead of typing twice.
    var sendTextIdempotent: Bool { advertises(["send_text_idempotent"]) }
    /// `get_command_catalog` is answerable.
    var servesCommandCatalog: Bool { advertises(["command_catalog"]) }
    /// The daemon closes a Mac view its own injection opened, and reports
    /// `composer_recovered` / `composer_lost`. False means the snapshot
    /// commands must not be offered: their entire safety story is that the
    /// daemon closes the view again.
    var recoversComposer: Bool { advertises(["slash_composer_recovery"]) }
    /// Approval cards carry a daemon-computed `risk` block.
    var classifiesRisk: Bool { advertises(["risk_class", "risk", "risk_classes"]) }
    /// A live terminal can be opened over *this* connection. Connection-scoped,
    /// not a build fact: a terminal is shell-equivalent authority, so the daemon
    /// answers false for the static bootstrap token. False means the Terminal
    /// tab must say what to fix rather than offer a session it cannot open.
    var servesTerminal: Bool { advertises(["terminal_pty"]) }
    /// The daemon accepts `delete_session`. Absent before minor 7, and unknown is
    /// false, so an older Mac simply does not offer the swipe rather than offering
    /// one that silently does nothing.
    var deletesSessions: Bool { advertises(["delete_session"]) }
    /// `test_push` is answerable — distinct from `push`, which a minor-6 daemon
    /// advertises without understanding the test request.
    var testsPush: Bool { advertises(["test_push"]) }
    /// The one push decision the rest of the app reads, normalized from the two
    /// raw flags with **direct precedence**: a daemon that somehow advertised
    /// both is a direct-key daemon, and the direct path needs no relay
    /// credential. Every push gate — eligibility, registration, the test button,
    /// the trust-screen row — keys off this, never off `push` or `pushRelay`
    /// alone, so the two flags cannot disagree anywhere downstream.
    var pushMode: PushMode {
        if push { return .direct }
        if pushRelay { return .relay }
        return .none
    }
    /// This particular connection is encrypted — a different fact from `tls`,
    /// which only says the listener *holds* a certificate. The daemon accepts
    /// both schemes on one port during the migration, so the phone is entitled
    /// to both facts and must not conflate them.
    var tlsActive: Bool { advertises(["tls_active"]) }
    /// Sessions and events carry a `session_uid`, and every message that names a
    /// session accepts one. False means a reused tmux name is the only identity
    /// there is, and two runs under one name will read as a single spliced
    /// timeline — which is exactly what this flag being true fixes.
    var scopesSessionsByUID: Bool { advertises(["session_uid", "session_uids"]) }

    /// Everything advertised, sorted, for the trust screen. Boolean-valued keys
    /// come back as `(name, isOn)`; anything else is rendered as its value.
    var advertisedRows: [(name: String, value: String, isFlag: Bool, isOn: Bool)] {
        advertised.keys.sorted().map { key in
            let value = advertised[key] ?? .null
            if let flag = value.boolValue {
                return (key, flag ? "yes" : "no", true, flag)
            }
            return (key, value.stringValue ?? value.canonicalJSONString, false, false)
        }
    }
}

// MARK: - Answers

enum AnswerDecision: Sendable, Hashable {
    case allow
    case deny
    /// Pick the nth option exactly as Claude numbered it (1-based).
    case option(index: UInt32)
    /// Pick an option by its stable id rather than its ordinal — the agent-seam
    /// shape, where Codex names options by id (`"acceptWithExecpolicyAmendment"`)
    /// rather than position. Additive to `option`.
    case optionId(String)
    case text(String)
    /// A decision kind a newer daemon knows about and this build does not.
    /// Never sent — only received, inside a recorded outcome. Throwing here
    /// instead would make the *whole* `answer_result` frame undecodable, and the
    /// tap that is waiting on it would time out rather than learn what actually
    /// happened.
    case unrecognised(String)
}

extension AnswerDecision: Codable {
    private enum CodingKeys: String, CodingKey {
        case type, index, text
        case optionId = "option_id"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .type) {
        case "allow": self = .allow
        case "deny": self = .deny
        case "option": self = .option(index: try c.decode(UInt32.self, forKey: .index))
        case "option_id": self = .optionId(try c.decode(String.self, forKey: .optionId))
        case "text": self = .text(try c.decode(String.self, forKey: .text))
        case let other: self = .unrecognised(other)
        }
    }

    func encode(to encoder: Encoder) throws {
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
        case .unrecognised(let raw):
            try c.encode(raw, forKey: .type)
        }
    }

    var label: String {
        switch self {
        case .allow: return "Allowed"
        case .deny: return "Denied"
        case .option(let index): return "Chose option \(index)"
        case .optionId(let id): return "Chose option \(id)"
        case .text: return "Replied with text"
        case .unrecognised(let raw): return "Answered (\(raw))"
        }
    }
}

struct AnswerOutcome: Codable, Sendable, Hashable {
    let requestID: String
    /// The tmux *name* of the run that was answered — `ccd/src/state.rs` fills
    /// this from `session.name`, not from the uid. Display only; never key
    /// anything by it.
    let sessionID: String
    let decision: AnswerDecision
    let resolvedBy: ResolvedBy
    let appliedVia: AnswerPath
    let resolvedAt: String
    let detail: String?
    /// True when `decision` is the daemon's *inference* rather than an observed
    /// answer — the local-resolution path, where all it truly knows is that the
    /// prompt is gone. The UI must not state an inferred decision as fact.
    let inferred: Bool
    /// True when the daemon could not establish whether the answer landed at
    /// all — the agent-seam case, parallel to `inferred` but weaker: `inferred`
    /// still asserts the prompt is gone, this asserts nothing about the write.
    /// Absent ⇒ false, mirroring the Rust `#[serde(default)]` bool.
    let indeterminate: Bool

    enum CodingKeys: String, CodingKey {
        case requestID = "request_id"
        case sessionID = "session_id"
        case decision
        case resolvedBy = "resolved_by"
        case appliedVia = "applied_via"
        case resolvedAt = "resolved_at"
        case detail
        case inferred
        case indeterminate
    }

    init(
        requestID: String, sessionID: String, decision: AnswerDecision, resolvedBy: ResolvedBy,
        appliedVia: AnswerPath, resolvedAt: String, detail: String?, inferred: Bool,
        indeterminate: Bool = false
    ) {
        self.requestID = requestID
        self.sessionID = sessionID
        self.decision = decision
        self.resolvedBy = resolvedBy
        self.appliedVia = appliedVia
        self.resolvedAt = resolvedAt
        self.detail = detail
        self.inferred = inferred
        self.indeterminate = indeterminate
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        requestID = try c.decode(String.self, forKey: .requestID)
        sessionID = try c.decode(String.self, forKey: .sessionID)
        decision = try c.decode(AnswerDecision.self, forKey: .decision)
        resolvedBy = try c.decode(ResolvedBy.self, forKey: .resolvedBy)
        appliedVia = try c.decode(AnswerPath.self, forKey: .appliedVia)
        resolvedAt = try c.decode(String.self, forKey: .resolvedAt)
        detail = try c.decodeIfPresent(String.self, forKey: .detail)
        inferred = try c.decodeIfPresent(Bool.self, forKey: .inferred) ?? false
        indeterminate = try c.decodeIfPresent(Bool.self, forKey: .indeterminate) ?? false
    }

    var resolvedDate: Date { ISO8601.parse(resolvedAt) ?? .distantPast }

    /// How to describe this outcome without overclaiming. An inferred decision
    /// is reported as "the prompt went away", never as "Allowed".
    var decisionLabel: String {
        inferred ? "Answered at the keyboard" : decision.label
    }
}

enum AnswerResult: Sendable, Hashable {
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

    init(from decoder: Decoder) throws {
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

    func encode(to encoder: Encoder) throws {
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
enum ResolutionActor: Decodable, Sendable, Hashable {
    case phone, local
    /// An actor a newer daemon names and this build does not.
    case unknown(String)

    init(from decoder: Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "phone": self = .phone
        case "local": self = .local
        case let other: self = .unknown(other)
        }
    }
}

/// Why a Codex prompt was cleared without an answer. `turnAborted` is
/// load-bearing — it is how the app learns the turn itself went away — so it
/// must decode and be retained, never folded into the unknown fallback.
enum ClearCause: Decodable, Sendable, Hashable {
    case turnAborted, turnCompleted, superseded
    /// A cause a newer daemon names and this build does not.
    case unknown(String)

    init(from decoder: Decoder) throws {
        switch try decoder.singleValueContainer().decode(String.self) {
        case "turn_aborted": self = .turnAborted
        case "turn_completed": self = .turnCompleted
        case "superseded": self = .superseded
        case let other: self = .unknown(other)
        }
    }
}

/// How far a write got before the daemon lost sight of it, on the
/// `status:"unknown"` path. Each value is a strictly weaker claim than the
/// last, and an unrecognised stage is retained rather than assumed to be any
/// of them.
enum WriteStage: Decodable, Sendable, Hashable {
    case claimedNotEnqueued, brokerIngressAccepted, upstreamWriteUnconfirmed
    /// A stage a newer daemon names and this build does not.
    case unknown(String)

    init(from decoder: Decoder) throws {
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
enum CodexResolution: Decodable, Sendable, Hashable {
    /// The prompt was answered. `decision` is omitted when the daemon recorded
    /// no decision alongside the actor.
    case answered(by: ResolutionActor, decision: AnswerDecision?)
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
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .status) {
        case "answered":
            self = .answered(
                by: try c.decode(ResolutionActor.self, forKey: .by),
                decision: try c.decodeIfPresent(AnswerDecision.self, forKey: .decision))
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

enum SendTextResult: Sendable, Hashable {
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
    /// The keys landed, the Mac's composer disappeared, and the daemon's own
    /// Escape brought it back. As final as `.sent`. `paneSnapshot` is the
    /// Mac's screen while the view was up — present only for the snapshot
    /// commands, and never stored.
    case composerRecovered(matched: String, paneSnapshot: String?, capturedAt: String)
    /// The keys landed, the composer disappeared, and one Escape was not
    /// enough. Somebody has to look at the Mac.
    case composerLost(matched: String)
}

extension SendTextResult: Codable {
    private enum CodingKeys: String, CodingKey {
        case status, matched, reason
        case appliedAt = "applied_at"
        case paneSnapshot = "pane_snapshot"
        case capturedAt = "captured_at"
    }

    init(from decoder: Decoder) throws {
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
        case "composer_recovered":
            self = .composerRecovered(
                matched: try c.decode(String.self, forKey: .matched),
                paneSnapshot: try c.decodeIfPresent(String.self, forKey: .paneSnapshot),
                capturedAt: try c.decode(String.self, forKey: .capturedAt))
        case "composer_lost":
            self = .composerLost(matched: try c.decode(String.self, forKey: .matched))
        case let other:
            // A status this build has never seen is a mutation result it
            // cannot vouch for. "Refused" would promise nothing was typed —
            // a promise on the daemon's behalf — so the unknown decodes as
            // the case that promises nothing.
            self = .indeterminate(reason: "unrecognised send_text status: \(other)")
        }
    }

    func encode(to encoder: Encoder) throws {
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
        case .composerRecovered(let matched, let paneSnapshot, let capturedAt):
            try c.encode("composer_recovered", forKey: .status)
            try c.encode(matched, forKey: .matched)
            try c.encodeIfPresent(paneSnapshot, forKey: .paneSnapshot)
            try c.encode(capturedAt, forKey: .capturedAt)
        case .composerLost(let matched):
            try c.encode("composer_lost", forKey: .status)
            try c.encode(matched, forKey: .matched)
        }
    }
}

/// What must be on screen before keys are injected. The daemon substitutes
/// operator-configured needles for the named modes.
enum PromptPresence: Sendable, Hashable {
    case inputBox
    case permissionPrompt
    case anyOf(needles: [String])
}

extension PromptPresence: Codable {
    private enum CodingKeys: String, CodingKey { case mode, needles }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .mode) {
        case "input_box": self = .inputBox
        case "permission_prompt": self = .permissionPrompt
        case "any_of": self = .anyOf(needles: try c.decode([String].self, forKey: .needles))
        case let other:
            throw DecodingError.dataCorruptedError(
                forKey: .mode, in: c, debugDescription: "unknown presence \(other)")
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .inputBox: try c.encode("input_box", forKey: .mode)
        case .permissionPrompt: try c.encode("permission_prompt", forKey: .mode)
        case .anyOf(let needles):
            try c.encode("any_of", forKey: .mode)
            try c.encode(needles, forKey: .needles)
        }
    }
}

// MARK: - Approval card

/// Payload of an `approval_request` event, under the `card` key.
struct ApprovalCard: Codable, Sendable, Hashable {
    let requestID: String
    let payloadHash: String
    let toolName: String
    let toolInput: JSONValue
    /// The exact text hashed into `payloadHash`.
    let displayText: String
    let permissionSuggestions: JSONValue?
    let promptID: String?
    let permissionMode: String?
    /// The daemon's own classification, decided at the Mac where the repo, the
    /// worktree and the operator's rules are actually visible. Sent only from
    /// feature level 1; absent on older daemons. Interpreting it is
    /// `RiskAssessment`'s job, not this type's.
    let risk: WireRisk?

    enum CodingKeys: String, CodingKey {
        case requestID = "request_id"
        case payloadHash = "payload_hash"
        case toolName = "tool_name"
        case toolInput = "tool_input"
        case displayText = "display_text"
        case permissionSuggestions = "permission_suggestions"
        case promptID = "prompt_id"
        case permissionMode = "permission_mode"
        case risk
    }
}

/// `protocol::risk::RiskAssessment` — the class plus the rule that produced it.
///
/// `matchedPattern` is present on `high` and absent otherwise, by design at the
/// Mac: a classification you cannot see the reason for is a badge nobody trusts
/// twice, and the cases where it matters are the destructive ones.
struct WireRisk: Codable, Sendable, Hashable {
    let cls: String
    let matchedPattern: String?

    enum CodingKeys: String, CodingKey {
        case cls = "class"
        case matchedPattern = "matched_pattern"
    }
}

// MARK: - Diff

/// Answer to `get_diff`. On-demand only: a diff is a thing you ask for when you
/// are about to read it, never a thing that streams at you.
struct SessionDiff: Sendable, Hashable {
    let sessionID: String
    /// `git -C <cwd> diff HEAD` plus untracked names, exactly as the daemon
    /// captured it.
    let unified: String
    /// The daemon hit its 512KB cap. Rendering a truncated diff as if it were
    /// whole is precisely the class of lie this flag exists to prevent.
    let truncated: Bool
    let capturedAt: String
    /// Why the diff is empty when it is. `unified` is blank both for a clean
    /// tree and for a directory that was never a git repository, and the
    /// daemon's note is the only thing that tells those apart — rendering "no
    /// changes" for the second would be a claim the app cannot support.
    let note: String?

    var capturedDate: Date? { ISO8601.parse(capturedAt) }
}

extension SessionDiff: Decodable {
    private enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case unified
        case truncated
        case capturedAt = "captured_at"
        case note
    }

    init(from decoder: Decoder) throws {
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
enum HelloCredential: Sendable, Hashable {
    /// A device token from a previous pairing, or the static `codeconnect token`.
    case token(String)
    /// Single-use, 5-minute code straight off the QR. The daemon answers with a
    /// device token, which is what gets stored.
    case pairingCode(String)
}

/// Everything the phone can say to a daemon.
///
/// `session` on these cases is a **session reference**, which
/// `protocol/src/ws.rs` defines as either a `session_uid` (exact) or a tmux name
/// like `cc-1` (legacy, and resolved by the daemon to the newest run under that
/// name). The app sends the uid whenever the daemon mints them, because a name
/// can resolve to a *different run* than the one the screen is showing — and for
/// `sendText` that would type into the wrong agent's TTY.
enum ClientMessage: Sendable {
    case hello(credential: HelloCredential, clientID: String?, clientName: String?)
    case sessions
    case subscribe(session: String, afterSeq: UInt64)
    case unsubscribe(session: String)
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
        session: String, text: String, require: PromptPresence?, submit: Bool,
        requestID: String?, payloadHash: String?, completeNativeConfirmation: Bool)
    case capture(session: String, lines: UInt32?)
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
    /// Open a live terminal on a hosted session. Minor 13.
    ///
    /// By uid, like every other session-scoped message: a tmux name is reused,
    /// a uid is not. `outputCredit` is how many decoded bytes the daemon may
    /// send before this phone replenishes.
    case terminalAttach(
        attachmentID: String, sessionUID: String, cols: Int, rows: Int, outputCredit: UInt32)
    /// Keystrokes for the pane, base64 of the raw bytes. Spends input credit.
    case terminalInput(attachmentID: String, base64: String)
    /// This phone's viewport changed. Applied to the daemon's own disposable
    /// client only, so a human at the Mac is never resized by it.
    case terminalResize(attachmentID: String, cols: Int, rows: Int)
    /// `bytes` of output have been consumed; grant that much more.
    case terminalCredit(attachmentID: String, bytes: UInt32)
    /// Close the terminal. The session and the agent are untouched.
    case terminalDetach(attachmentID: String)
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
        case sessionID = "session_id"
        case sessionUID = "session_uid"
        case afterSeq = "after_seq"
        case environment
        case relayCredential = "relay_credential"
        case requestID = "request_id"
        case payloadHash = "payload_hash"
        case decision
        case text
        case require
        case submit
        case lines
        case completeNativeConfirmation = "complete_native_confirmation"
        case attachmentID = "attachment_id"
        case cols
        case rows
        case outputCredit = "output_credit"
        case bytes
        case data
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .hello(let credential, let clientID, let clientName):
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
            }
            try c.encodeIfPresent(clientID, forKey: .clientID)
            try c.encodeIfPresent(clientName, forKey: .clientName)
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
        case .sendText(
            let session, let text, let require, let submit, let requestID, let payloadHash,
            let completeNativeConfirmation):
            try c.encode("send_text", forKey: .type)
            try c.encode(session, forKey: .sessionID)
            try c.encode(text, forKey: .text)
            try c.encodeIfPresent(require, forKey: .require)
            try c.encode(submit, forKey: .submit)
            // Omitted, never null: the daemon reads `Option<String>` and a
            // null would say "present but empty".
            try c.encodeIfPresent(requestID, forKey: .requestID)
            try c.encodeIfPresent(payloadHash, forKey: .payloadHash)
            // Only ever true from a control that stated this command's
            // consequences before the tap. It is a permission, not an
            // instruction: the daemon still decides which commands it applies
            // to, so this can never nominate one.
            try c.encode(completeNativeConfirmation, forKey: .completeNativeConfirmation)
        case .capture(let session, let lines):
            try c.encode("capture", forKey: .type)
            try c.encode(session, forKey: .sessionID)
            try c.encodeIfPresent(lines, forKey: .lines)
        case .registerPush(let token, let environment, let relayCredential):
            try c.encode("register_push", forKey: .type)
            try c.encode(token, forKey: .token)
            try c.encode(environment, forKey: .environment)
            // Omitted, never null, when absent: an old daemon's `deny_unknown`
            // is not in play here, but a missing key is what "direct, no
            // credential" means on the wire, and a present-but-null would read
            // as "relay, credential lost".
            try c.encodeIfPresent(relayCredential, forKey: .relayCredential)
        case .terminalAttach(
            let attachmentID, let sessionUID, let cols, let rows, let outputCredit):
            try c.encode("terminal_attach", forKey: .type)
            try c.encode(attachmentID, forKey: .attachmentID)
            try c.encode(sessionUID, forKey: .sessionUID)
            try c.encode(cols, forKey: .cols)
            try c.encode(rows, forKey: .rows)
            try c.encode(outputCredit, forKey: .outputCredit)
        case .terminalInput(let attachmentID, let base64):
            try c.encode("terminal_input", forKey: .type)
            try c.encode(attachmentID, forKey: .attachmentID)
            try c.encode(base64, forKey: .data)
        case .terminalResize(let attachmentID, let cols, let rows):
            try c.encode("terminal_resize", forKey: .type)
            try c.encode(attachmentID, forKey: .attachmentID)
            try c.encode(cols, forKey: .cols)
            try c.encode(rows, forKey: .rows)
        case .terminalCredit(let attachmentID, let bytes):
            try c.encode("terminal_credit", forKey: .type)
            try c.encode(attachmentID, forKey: .attachmentID)
            try c.encode(bytes, forKey: .bytes)
        case .terminalDetach(let attachmentID):
            try c.encode("terminal_detach", forKey: .type)
            try c.encode(attachmentID, forKey: .attachmentID)
        case .ping:
            try c.encode("ping", forKey: .type)
        }
    }
}

// MARK: - Server -> client

/// Everything `hello_ack` carries. A struct rather than eight associated values:
/// the ack has already grown by four fields once and will grow again.
struct HelloAck: Sendable, Hashable {
    var protocolVersion: UInt32
    /// Additive feature level. `1` advertises `turn_complete`, `get_diff`,
    /// pairing codes and daemon-side risk classification; `protocolVersion`
    /// stays at 1 because those additions break nothing, so this is the field
    /// that actually distinguishes a newer daemon from an older one.
    var protocolMinor: UInt32
    var serverTime: String
    var capabilities: Capabilities
    /// Present exactly once: in the ack for a `pairing_code` hello. The durable
    /// credential; the code that bought it is spent.
    var deviceToken: String?
    var deviceID: String?
    /// What `codeconnect devices` lists this phone as, and what `codeconnect revoke` takes. May
    /// differ from the requested name when that one was taken.
    var deviceName: String?
    /// The daemon's current authoritative APNs environment for this device's
    /// registered token, absent when no token is registered. Minor 14. The relay
    /// binding is the single authority for a token's environment (§4); a relay
    /// send against the wrong advisory environment is corrected downstream and
    /// the daemon CAS-persists the truth, then reports it here. The app compares
    /// this to its cached tuple on every handshake and **persists a difference
    /// rather than resending its stale value** — the daemon→app correction path.
    var pushEnvironment: String?
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
enum DeleteSessionResult: Decodable, Sendable, Hashable {
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
    var meansItIsGone: Bool {
        switch self {
        case .deleted, .notFound: true
        case .stillRunning, .notExited, .failed, .unknown: false
        }
    }

    init(from decoder: Decoder) throws {
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
                    ?? "The Mac could not remove that session.")
        default: self = .unknown(status: status)
        }
    }
}

/// What became of a `test_push`. Decode-only, like every reply; `accepted`
/// claims exactly what Apple's 200 proves — accepted for delivery, with the
/// banner as the device's own final word. An unknown status stays inert.
enum TestPushResult: Decodable, Sendable, Hashable {
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

    init(from decoder: Decoder) throws {
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
                    ?? "The Mac could not send the test.")
        default: self = .unknown(status: status)
        }
    }
}

/// What the Mac knows about its Claude Code's slash commands.
enum CommandCatalogResult: Sendable, Hashable {
    /// The binary's own inventory — names without the leading slash, exactly
    /// as it emitted them. `probedAt` is when the list was actually read; a
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

    init(from decoder: Decoder) throws {
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

    func encode(to encoder: Encoder) throws {
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

enum ServerMessage: Sendable {
    case helloAck(HelloAck)
    case sessions([SessionSummary])
    case event(Event)
    case answerResult(requestID: String, result: AnswerResult)
    case sendTextResult(sessionID: String, result: SendTextResult)
    case captureResult(sessionID: String, text: String)
    case commandCatalog(sessionID: String, result: CommandCatalogResult)
    case deleteSessionResult(sessionUID: String, result: DeleteSessionResult)
    case testPushResult(requestID: String, result: TestPushResult)
    case diff(SessionDiff)
    /// The terminal is live. `inputCredit` is how many decoded bytes this phone
    /// may send before the first `terminal_credit`. Minor 13.
    ///
    /// `maxChunkBytes` and `maxOutstandingCredit` are **the daemon's own flow
    /// control ceilings**. They are carried rather than assumed for one reason:
    /// exceeding either is a protocol error the daemon closes the terminal for,
    /// so a Mac that raised one would otherwise be killing terminals on every
    /// phone compiled against the old number.
    ///
    /// Optional here, required at the Mac. That is not a compatibility window —
    /// a daemon too old to carry them never sends this message at all — it is
    /// this side declining to lose a whole ack over a field: `nil` falls back to
    /// the mirrored `Wire.Terminal` constants, which is what the phone enforced
    /// before the ceilings were advertised and what it still sends its own
    /// attach against.
    case terminalAttached(
        attachmentID: String, inputCredit: UInt32, maxChunkBytes: UInt32?,
        maxOutstandingCredit: UInt32?)
    /// Pane bytes, base64. Spends the output credit this phone granted.
    case terminalOutput(attachmentID: String, base64: String)
    /// The daemon has taken `bytes` of input and grants that much more.
    case terminalCredit(attachmentID: String, bytes: UInt32)
    /// The terminal ended. `code` is one of the `terminal_close` strings and
    /// `reason` is human text; terminal for this attachment id.
    case terminalClosed(attachmentID: String, code: String, reason: String)
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
        case sessions
        case event
        case requestID = "request_id"
        case sessionID = "session_id"
        case result
        case text
        case code
        case message
        case attachmentID = "attachment_id"
        case inputCredit = "input_credit"
        case maxChunkBytes = "max_chunk_bytes"
        case maxOutstandingCredit = "max_outstanding_credit"
        case bytes
        case data
        case reason
    }

    init(from decoder: Decoder) throws {
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
                    pushEnvironment: try c.decodeIfPresent(String.self, forKey: .pushEnvironment)))
        case "diff":
            self = .diff(try SessionDiff(from: decoder))
        case "sessions":
            self = .sessions(try c.decode([SessionSummary].self, forKey: .sessions))
        case "event":
            self = .event(try c.decode(Event.self, forKey: .event))
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
        case "capture_result":
            self = .captureResult(
                sessionID: try c.decode(String.self, forKey: .sessionID),
                text: try c.decode(String.self, forKey: .text))
        case "command_catalog":
            self = .commandCatalog(
                sessionID: try c.decode(String.self, forKey: .sessionID),
                result: try c.decode(CommandCatalogResult.self, forKey: .result))
        case "error":
            self = .error(
                code: try c.decode(String.self, forKey: .code),
                message: try c.decode(String.self, forKey: .message))
        case "terminal_attached":
            self = .terminalAttached(
                attachmentID: try c.decode(String.self, forKey: .attachmentID),
                inputCredit: try c.decodeIfPresent(UInt32.self, forKey: .inputCredit) ?? 0,
                // Absent stays absent rather than defaulting here: the fallback
                // belongs where the ceiling is enforced, and a decoder that
                // silently substituted a constant would make "the daemon said
                // 16 KiB" and "the daemon said nothing" the same fact.
                maxChunkBytes: try c.decodeIfPresent(UInt32.self, forKey: .maxChunkBytes),
                maxOutstandingCredit: try c.decodeIfPresent(
                    UInt32.self, forKey: .maxOutstandingCredit))
        case "terminal_output":
            self = .terminalOutput(
                attachmentID: try c.decode(String.self, forKey: .attachmentID),
                base64: try c.decode(String.self, forKey: .data))
        case "terminal_credit":
            self = .terminalCredit(
                attachmentID: try c.decode(String.self, forKey: .attachmentID),
                bytes: try c.decodeIfPresent(UInt32.self, forKey: .bytes) ?? 0)
        case "terminal_closed":
            self = .terminalClosed(
                attachmentID: try c.decode(String.self, forKey: .attachmentID),
                code: try c.decodeIfPresent(String.self, forKey: .code) ?? "",
                reason: try c.decodeIfPresent(String.self, forKey: .reason) ?? "")
        case "pong":
            self = .pong
        case let other:
            self = .unknown(type: other)
        }
    }
}

// MARK: - Timestamps

enum ISO8601 {
    /// The daemon emits RFC3339 UTC with millisecond precision, but a strategy
    /// that assumes fractional seconds fails on a whole-second timestamp, so
    /// both are tried. `ISO8601FormatStyle` is a value type — unlike
    /// `ISO8601DateFormatter` it can be shared across tasks safely.
    private static let withFraction = Date.ISO8601FormatStyle(includingFractionalSeconds: true)
    private static let plain = Date.ISO8601FormatStyle()

    static func parse(_ string: String) -> Date? {
        (try? withFraction.parse(string)) ?? (try? plain.parse(string))
    }
}
