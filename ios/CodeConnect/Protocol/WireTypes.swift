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
}

// MARK: - Scalars

enum EventSource: String, Codable, Sendable, Hashable {
    case hook, transcript, daemon, pty

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = EventSource(rawValue: raw) ?? .daemon
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

enum AnswerPath: String, Codable, Sendable, Hashable {
    case hookReturn = "hook_return"
    case sendKeys = "send_keys"

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = AnswerPath(rawValue: raw) ?? .sendKeys
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
    }

    /// What this app files the run's events, subscriptions and marks under, and
    /// what it sends when it has to name the run on the wire. See
    /// `Event.sessionKey` for why it is read off the data rather than off a flag.
    var sessionKey: String { sessionUID.isEmpty ? sessionID : sessionUID }

    var id: String { sessionKey }
    var updatedDate: Date { ISO8601.parse(updatedAt) ?? .distantPast }
    var createdDate: Date { ISO8601.parse(createdAt) ?? .distantPast }
    var displayName: String { sessionID }
    var folderName: String { (cwd as NSString).lastPathComponent }
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
        self.tls = tls
        var map = extra
        map["can_approve_reliably"] = .bool(canApproveReliably)
        map["fail_mode"] = .string(failMode)
        map["answer_path"] = .string(answerPath.rawValue)
        map["hold_secs"] = .int(Int64(holdSecs))
        map["send_text"] = .bool(sendText)
        map["capture"] = .bool(capture)
        map["push"] = .bool(push)
        map["tls"] = .bool(tls)
        advertised = map
    }

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode([String: JSONValue].self)
        advertised = raw
        canApproveReliably = raw["can_approve_reliably"]?.boolValue ?? false
        failMode = raw["fail_mode"]?.stringValue ?? "unknown"
        answerPath =
            raw["answer_path"]?.stringValue.flatMap(AnswerPath.init(rawValue:)) ?? .sendKeys
        holdSecs = (raw["hold_secs"]?.intValue).map { UInt64(max(0, $0)) } ?? 0
        sendText = raw["send_text"]?.boolValue ?? false
        capture = raw["capture"]?.boolValue ?? false
        push = raw["push"]?.boolValue ?? false
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
    /// Approval cards carry a daemon-computed `risk` block.
    var classifiesRisk: Bool { advertises(["risk_class", "risk", "risk_classes"]) }
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
    case text(String)
    /// A decision kind a newer daemon knows about and this build does not.
    /// Never sent — only received, inside a recorded outcome. Throwing here
    /// instead would make the *whole* `answer_result` frame undecodable, and the
    /// tap that is waiting on it would time out rather than learn what actually
    /// happened.
    case unrecognised(String)
}

extension AnswerDecision: Codable {
    private enum CodingKeys: String, CodingKey { case type, index, text }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .type) {
        case "allow": self = .allow
        case "deny": self = .deny
        case "option": self = .option(index: try c.decode(UInt32.self, forKey: .index))
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

    enum CodingKeys: String, CodingKey {
        case requestID = "request_id"
        case sessionID = "session_id"
        case decision
        case resolvedBy = "resolved_by"
        case appliedVia = "applied_via"
        case resolvedAt = "resolved_at"
        case detail
        case inferred
    }

    init(
        requestID: String, sessionID: String, decision: AnswerDecision, resolvedBy: ResolvedBy,
        appliedVia: AnswerPath, resolvedAt: String, detail: String?, inferred: Bool
    ) {
        self.requestID = requestID
        self.sessionID = sessionID
        self.decision = decision
        self.resolvedBy = resolvedBy
        self.appliedVia = appliedVia
        self.resolvedAt = resolvedAt
        self.detail = detail
        self.inferred = inferred
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

enum SendTextResult: Sendable, Hashable {
    /// `matched` is the prompt-presence needle that authorised the keystrokes.
    case sent(matched: String)
    case refused(reason: String)
}

extension SendTextResult: Codable {
    private enum CodingKeys: String, CodingKey { case status, matched, reason }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .status) {
        case "sent": self = .sent(matched: try c.decode(String.self, forKey: .matched))
        case "refused": self = .refused(reason: try c.decode(String.self, forKey: .reason))
        case let other:
            self = .refused(reason: "unrecognised send_text status: \(other)")
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
    /// `sshPublicKey` is offered on every hello but only ever *acted on* by a
    /// daemon whose operator ran `codeconnect pair --ssh` — consent lives at the Mac's
    /// terminal, not in this message.
    case hello(
        credential: HelloCredential, clientID: String?, clientName: String?,
        sshPublicKey: String?)
    case sessions
    case subscribe(session: String, afterSeq: UInt64)
    case unsubscribe(session: String)
    /// `session` scopes the answer to one run. Optional on the wire so a
    /// pre-uid daemon still accepts it; when present the daemon refuses to apply
    /// the answer to a different run that happens to be showing a card with the
    /// same `request_id`.
    case answer(
        requestID: String, payloadHash: String, decision: AnswerDecision, session: String?)
    case sendText(session: String, text: String, require: PromptPresence?, submit: Bool)
    case capture(session: String, lines: UInt32?)
    case getDiff(session: String)
    /// "Push me here." Sent whenever Apple issues a token, not only at
    /// handshake: permission can be granted mid-session and the token is
    /// reissued on reinstall and on restore-from-backup.
    case registerPush(token: String, environment: String)
    case ping
}

extension ClientMessage: Encodable {
    private enum CodingKeys: String, CodingKey {
        case type
        case protocolVersion = "protocol_version"
        case token
        case pairingCode = "pairing_code"
        case sshPublicKey = "ssh_pubkey"
        case clientID = "client_id"
        case clientName = "client_name"
        case sessionID = "session_id"
        case afterSeq = "after_seq"
        case environment
        case requestID = "request_id"
        case payloadHash = "payload_hash"
        case decision
        case text
        case require
        case submit
        case lines
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .hello(let credential, let clientID, let clientName, let sshPublicKey):
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
            try c.encodeIfPresent(sshPublicKey, forKey: .sshPublicKey)
        case .getDiff(let session):
            try c.encode("get_diff", forKey: .type)
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
        case .answer(let requestID, let payloadHash, let decision, let session):
            try c.encode("answer", forKey: .type)
            try c.encode(requestID, forKey: .requestID)
            try c.encode(payloadHash, forKey: .payloadHash)
            try c.encode(decision, forKey: .decision)
            // Omitted rather than sent as null when there is nothing to scope
            // to: `ws.rs` reads `Option<String>`, and a null would decode as
            // "no scope" anyway while making the frame wrong to read.
            try c.encodeIfPresent(session, forKey: .sessionID)
        case .sendText(let session, let text, let require, let submit):
            try c.encode("send_text", forKey: .type)
            try c.encode(session, forKey: .sessionID)
            try c.encode(text, forKey: .text)
            try c.encodeIfPresent(require, forKey: .require)
            try c.encode(submit, forKey: .submit)
        case .capture(let session, let lines):
            try c.encode("capture", forKey: .type)
            try c.encode(session, forKey: .sessionID)
            try c.encodeIfPresent(lines, forKey: .lines)
        case .registerPush(let token, let environment):
            try c.encode("register_push", forKey: .type)
            try c.encode(token, forKey: .token)
            try c.encode(environment, forKey: .environment)
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
    /// Whether the offered SSH key was actually installed. Reported even when
    /// false, so "the operator did not consent" is distinguishable from "no key
    /// was offered" — the two need very different things said about them.
    var sshKeyInstalled: Bool?
}

enum ServerMessage: Sendable {
    case helloAck(HelloAck)
    case sessions([SessionSummary])
    case event(Event)
    case answerResult(requestID: String, result: AnswerResult)
    case sendTextResult(sessionID: String, result: SendTextResult)
    case captureResult(sessionID: String, text: String)
    case diff(SessionDiff)
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
        case deviceName = "device_name"
        case sshKeyInstalled = "ssh_key_installed"
        case sessions
        case event
        case requestID = "request_id"
        case sessionID = "session_id"
        case result
        case text
        case code
        case message
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
                    sshKeyInstalled: try c.decodeIfPresent(Bool.self, forKey: .sshKeyInstalled)))
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
        case "capture_result":
            self = .captureResult(
                sessionID: try c.decode(String.self, forKey: .sessionID),
                text: try c.decode(String.self, forKey: .text))
        case "error":
            self = .error(
                code: try c.decode(String.self, forKey: .code),
                message: try c.decode(String.self, forKey: .message))
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
