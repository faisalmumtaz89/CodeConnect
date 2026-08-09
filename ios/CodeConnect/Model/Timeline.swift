import Foundation

// MARK: - Items

enum ToolStatus: Sendable, Hashable {
    case running
    case succeeded
    case failed
    case interrupted
    /// The call was never approved, so it never ran.
    case denied
    /// No result was ever recorded and the turn has moved on. Saying "running"
    /// here would be a lie with a very long tail.
    case unresolved

    var label: String {
        switch self {
        case .running: return "running"
        case .succeeded: return "done"
        case .failed: return "failed"
        case .interrupted: return "interrupted"
        case .denied: return "denied"
        case .unresolved: return "no result recorded"
        }
    }

    var symbol: String {
        switch self {
        case .running: return "circle.dotted"
        case .succeeded: return "checkmark.circle.fill"
        case .failed: return "xmark.octagon.fill"
        case .interrupted: return "stop.circle.fill"
        case .denied: return "hand.raised.fill"
        case .unresolved: return "questionmark.circle"
        }
    }
}

struct ToolItem: Sendable, Hashable {
    var toolUseID: String?
    var name: String
    var argument: String?
    var status: ToolStatus
    var durationMS: Int?
    var input: JSONValue?
    var output: String?
    var isTruncated: Bool
}

struct ApprovalItem: Sendable, Hashable, Identifiable {
    var card: ApprovalCard
    var requestedAt: Date
    /// Which *run* this card belongs to. Carried on the item because the Deck
    /// mixes cards from the whole fleet, where "the session you are looking at"
    /// is not a thing that exists — and because the answer has to name the run
    /// it came from, or a daemon holding two cards with one `request_id` cannot
    /// tell which of them was tapped.
    var sessionKey: String
    /// `nil` while nobody has answered. Silence is never consent.
    var outcome: AnswerOutcome?
    /// `capture-pane` text taken when Claude said the prompt was up — the only
    /// source for the exact option list.
    var paneSnapshot: String?
    /// The daemon's own classification, or nil on a daemon too old to send one.
    var risk: WireRisk?

    var isPending: Bool { outcome == nil }

    /// The run *and* the request, because a `request_id` is only unique within
    /// one run. Two runs of the same project can raise the same id — the daemon
    /// says so in `ccd/src/state.rs`, where an unscoped answer to a shared id is
    /// refused rather than guessed at — and the Deck mixes the whole fleet into
    /// one list.
    var id: String { "\(sessionKey)#\(card.requestID)" }

    /// The gate this card actually gets. See `RiskAssessment` for why the two
    /// classifiers are combined by taking the stricter one.
    func assessment(profile: DaemonProfile) -> RiskAssessment {
        RiskAssessment.resolve(
            wire: risk, tool: card.toolName, input: card.toolInput, profile: profile)
    }
}

enum NoticeSeverity: Sendable, Hashable {
    case info, success, warning, failure
}

/// What a notice *means*, separate from what it says. Fleet status is derived
/// from this, never from the display copy — rewording a string must not be able
/// to change which band a session sorts into.
enum NoticeKind: Sendable, Hashable {
    case sessionStart
    /// A `Stop` hook: the turn finished, the session is still alive.
    case turnComplete
    case sessionEnded
    case agentWaiting
    case agentFinished
    /// Observation-about-our-observation; never evidence about the agent.
    case link
    case failure
    case other
}

struct NoticeItem: Sendable, Hashable {
    var kind: NoticeKind
    var symbol: String
    var title: String
    var detail: String?
    var severity: NoticeSeverity
}

struct TimelineItem: Sendable, Hashable, Identifiable {
    enum Content: Sendable, Hashable {
        case userMessage(String, isCommand: Bool = false)
        case agentMessage(String)
        case tool(ToolItem)
        case approval(ApprovalItem)
        case notice(NoticeItem)
    }

    let id: String
    let seq: UInt64
    let date: Date
    let content: Content

    var pendingApproval: ApprovalItem? {
        if case .approval(let item) = content, item.isPending { return item }
        return nil
    }
}

// MARK: - Builder

/// Turns the raw event log into the semantic timeline.
///
/// Two facts drive the whole design:
///   * the same tool call is reported by up to three sources (PreToolUse hook,
///     PostToolUse hook, transcript `tool_result`), and showing it three times
///     would be worse than showing nothing;
///   * an event log is append-only, so the builder is a pure function of the
///     events — no incremental state to get out of sync.
enum TimelineBuilder {
    static func build(_ events: [Event]) -> [TimelineItem] {
        guard !events.isEmpty else { return [] }

        var resultsByToolUse: [String: ToolOutcome] = [:]
        var outcomesByRequest: [String: AnswerOutcome] = [:]
        var panesByPrompt: [String: String] = [:]
        var hookToolCallIDs: Set<String> = []
        var approvalPromptIDs: Set<String> = []
        var lastTurnEndSeq: UInt64 = 0

        for event in events {
            switch event.kind {
            case .toolCall:
                if let id = event.toolUseID { hookToolCallIDs.insert(id) }
            case .toolResult:
                absorbResult(event, into: &resultsByToolUse)
            case .approvalResolved:
                if let outcome = event.approvalOutcome {
                    outcomesByRequest[outcome.requestID] = outcome
                }
            case .approvalRequest:
                if let promptID = event.approvalCard?.promptID { approvalPromptIDs.insert(promptID) }
            case .notification:
                if let pane = event.paneSnapshot,
                    let promptID = event.payload["prompt_id"]?.stringValue
                {
                    panesByPrompt[promptID] = pane
                }
            case .sessionEnd, .turnComplete:
                // Both close a turn, which is what an unresolved tool call is
                // measured against. Only one of them also ends the session.
                lastTurnEndSeq = max(lastTurnEndSeq, event.seq)
            default:
                break
            }
        }

        var items: [TimelineItem] = []
        items.reserveCapacity(events.count)

        for event in events {
            switch event.kind {
            case .userMessage:
                // Local-command lines first: a built-in slash command records
                // itself as XML-ish markup in user-type lines (measured:
                // caveat, invocation, stdout), and rendering those as a
                // person's words shows tag soup under a YOU label. The
                // invocation renders as the command the user actually issued;
                // Claude Code's stdout renders as the notice it is; the
                // caveat is boilerplate addressed to the model, not a message.
                if let local = event.localCommand {
                    switch local {
                    case .caveat:
                        break
                    case .invocation:
                        if let text = local.invocationText {
                            // `/clear` rotates the transcript: Claude Code
                            // opens a NEW file whose first user entry is this
                            // very invocation (measured), so its arrival IS
                            // the observable completion — the daemon is
                            // following the fresh conversation. Rendered as
                            // the fact it proves rather than as typed input.
                            if text == "/clear" {
                                items.append(
                                    event.item(
                                        .notice(
                                            NoticeItem(
                                                kind: .other,
                                                symbol: "eraser",
                                                title: "Conversation cleared.",
                                                detail: nil,
                                                severity: .info))))
                            } else {
                                items.append(event.item(.userMessage(text, isCommand: true)))
                            }
                        }
                    case .output(let text):
                        // A full sentence is not a title. The title names the
                        // kind of fact; the verbatim output is the detail.
                        if !text.isEmpty {
                            // **The verb decides the title.** `Kept model as X`
                            // is Claude Code's *no-change* receipt — what it
                            // prints when its confirmation was cancelled or the
                            // same model re-chosen. Filed under "Model changed"
                            // it is a permanent contradiction: the title saying
                            // one thing and the verbatim detail directly below
                            // it the other.
                            let title: String
                            switch ModelConfirmation.parse(text) {
                            case .set: title = "Model changed"
                            case .kept: title = "Model unchanged"
                            case nil: title = "Command output"
                            }
                            items.append(
                                event.item(
                                    .notice(
                                        NoticeItem(
                                            kind: .other,
                                            symbol: "terminal",
                                            title: title,
                                            detail: text,
                                            severity: .info))))
                        }
                    }
                } else if let text = event.userText {
                    items.append(event.item(.userMessage(text)))
                }

            case .agentMessage:
                if let text = event.agentText {
                    items.append(event.item(.agentMessage(text)))
                }
                // Only when the PreToolUse hook missed the call entirely.
                for use in event.agentToolUses where !hookToolCallIDs.contains(use.id) {
                    let outcome = resultsByToolUse[use.id]
                    let tool = ToolItem(
                        toolUseID: use.id,
                        name: use.name,
                        argument: ToolSummary.principalArgument(tool: use.name, input: use.input),
                        status: status(
                            for: use.id, outcome: outcome, eventSeq: event.seq,
                            approvals: outcomesByRequest, turnEnd: lastTurnEndSeq),
                        durationMS: outcome?.durationMS,
                        input: use.input,
                        output: outcome?.text,
                        isTruncated: event.isTruncated)
                    // One assistant turn can announce several tool uses, so the
                    // event id alone would not be unique.
                    items.append(event.item(.tool(tool), suffix: "#\(use.id)"))
                }

            case .toolCall:
                let id = event.toolUseID
                let outcome = id.flatMap { resultsByToolUse[$0] }
                let name = event.toolName ?? "tool"
                items.append(
                    event.item(
                        .tool(
                            ToolItem(
                                toolUseID: id,
                                name: name,
                                argument: ToolSummary.principalArgument(
                                    tool: name, input: event.toolInput),
                                status: status(
                                    for: id, outcome: outcome, eventSeq: event.seq,
                                    approvals: outcomesByRequest, turnEnd: lastTurnEndSeq),
                                durationMS: outcome?.durationMS,
                                input: event.toolInput,
                                output: outcome?.text,
                                isTruncated: event.isTruncated))))

            case .toolResult:
                // Folded into its call above, unless the call was never seen.
                guard let id = event.toolUseID, !hookToolCallIDs.contains(id) else { break }
                let outcome = resultsByToolUse[id]
                let name = event.toolName ?? "tool"
                items.append(
                    event.item(
                        .tool(
                            ToolItem(
                                toolUseID: id,
                                name: name,
                                argument: ToolSummary.principalArgument(
                                    tool: name, input: event.toolInput),
                                status: outcome?.status ?? .unresolved,
                                durationMS: outcome?.durationMS,
                                input: event.toolInput,
                                output: outcome?.text,
                                isTruncated: event.isTruncated))))

            case .approvalRequest:
                guard let card = event.approvalCard else {
                    items.append(
                        event.item(
                            .notice(
                                NoticeItem(
                                    kind: .failure,
                                    symbol: "exclamationmark.triangle",
                                    title: "Approval request could not be read",
                                    detail: "The daemon sent a card this app could not decode.",
                                    severity: .warning))))
                    break
                }
                items.append(
                    event.item(
                        .approval(
                            ApprovalItem(
                                card: card,
                                requestedAt: event.date,
                                sessionKey: event.sessionKey,
                                outcome: outcomesByRequest[card.requestID],
                                paneSnapshot: card.promptID.flatMap { panesByPrompt[$0] },
                                risk: card.risk ?? event.declaredRisk))))

            case .approvalResolved:
                break  // shown on the card it resolves

            case .notification:
                guard let notice = notice(for: event, approvalPromptIDs: approvalPromptIDs) else {
                    break
                }
                items.append(event.item(.notice(notice)))

            case .sessionStart:
                items.append(
                    event.item(
                        .notice(
                            NoticeItem(
                                kind: .sessionStart,
                                symbol: "play.circle",
                                title: "Session started",
                                // Through the same mapper the Model sheet
                                // uses. The hook hands over an API id —
                                // `claude-opus-5[1m]` — and one screen
                                // resolving that to "Opus 5 · 1M context"
                                // while another prints the id is the app
                                // disagreeing with itself about the same
                                // fact. An id this build does not recognise
                                // still passes through verbatim.
                                detail: event.modelName.map {
                                    let display = ModelDisplay.from($0)
                                    return [display.name, display.meta]
                                        .compactMap { $0 }
                                        .joined(separator: " · ")
                                },
                                severity: .info))))

            case .sessionEnd, .turnComplete:
                items.append(event.item(.notice(endNotice(for: event))))

            case .linkState:
                guard let fact = event.linkStateFact else { break }
                items.append(
                    event.item(
                        .notice(
                            NoticeItem(
                                kind: .link,
                                symbol: fact.link == "attached" ? "link" : "link.badge.plus",
                                title: "Link \(fact.link)",
                                detail: fact.reason,
                                severity: fact.link == "attached" ? .info : .warning))))

            case .error:
                items.append(
                    event.item(
                        .notice(
                            NoticeItem(
                                kind: .failure,
                                symbol: "exclamationmark.octagon",
                                title: "Error",
                                detail: event.payload.firstString("message", "error")
                                    ?? event.payload.prettyJSONString,
                                severity: .failure))))

            case .resync, .usage, .reasoning, .other:
                break  // resync is a banner, not a row; usage/reasoning are noise
            }
        }

        return items
    }

    // MARK: Tool results

    private struct ToolOutcome {
        var status: ToolStatus
        var durationMS: Int?
        var text: String?
    }

    /// Merges the PostToolUse hook and the transcript `tool_result` for one
    /// call. They disagree usefully: the hook has the timing, the transcript has
    /// Claude's own `is_error` verdict.
    private static func absorbResult(_ event: Event, into table: inout [String: ToolOutcome]) {
        guard let id = event.toolUseID else { return }
        var outcome = table[id] ?? ToolOutcome(status: .succeeded, durationMS: nil, text: nil)

        if let response = event.toolResponse {
            if response["interrupted"]?.boolValue == true { outcome.status = .interrupted }
            let stdout = response["stdout"]?.stringValue ?? ""
            let stderr = response["stderr"]?.stringValue ?? ""
            let combined = [stdout, stderr].filter { !$0.isEmpty }.joined(separator: "\n")
            if !combined.isEmpty { outcome.text = combined }
            if outcome.text == nil, let plain = response.stringValue { outcome.text = plain }
        }
        if let duration = event.durationMS { outcome.durationMS = duration }

        if let transcript = event.transcriptToolResult {
            if transcript.isError { outcome.status = .failed }
            if outcome.text == nil { outcome.text = transcript.text }
        }
        if event.isTruncated { outcome.text = "(output too large; the daemon truncated it)" }

        table[id] = outcome
    }

    /// A call with no result is only "running" while there is still a turn it
    /// could belong to.
    private static func status(
        for toolUseID: String?, outcome: ToolOutcome?, eventSeq: UInt64,
        approvals: [String: AnswerOutcome], turnEnd: UInt64
    ) -> ToolStatus {
        if let outcome { return outcome.status }
        if let toolUseID, let answer = approvals[toolUseID] {
            switch answer.decision {
            case .deny: return .denied
            // An unrecognised decision from a newer daemon is not evidence the
            // call was blocked, so it falls through to the timing-based verdict
            // rather than claiming a denial that may not have happened.
            case .allow, .option, .text, .unrecognised: break
            }
        }
        return eventSeq < turnEnd ? .unresolved : .running
    }

    // MARK: Notices

    private static func notice(for event: Event, approvalPromptIDs: Set<String>) -> NoticeItem? {
        let type = event.notificationType ?? "notification"
        let promptID = event.payload["prompt_id"]?.stringValue
        switch type {
        case "permission_prompt":
            // The approval card carries the same fact with far more detail.
            if let promptID, approvalPromptIDs.contains(promptID) { return nil }
            return NoticeItem(
                kind: .agentWaiting,
                symbol: "bell.badge",
                title: "Claude asked for permission",
                // Built as a sentence rather than glued to the message with
                // punctuation: the daemon does not always send one, and a
                // conjunction with nothing before it reads as a truncated string.
                detail: [event.notificationMessage, "No card arrived, so it can only be answered at the Mac."]
                    .compactMap { $0 }
                    .filter { !$0.isEmpty }
                    .joined(separator: " "),
                severity: .warning)
        case "agent_needs_input", "idle_prompt":
            return NoticeItem(
                kind: .agentWaiting,
                symbol: "questionmark.bubble",
                title: "Waiting for you",
                detail: event.notificationMessage,
                severity: .warning)
        case "agent_completed":
            return NoticeItem(
                kind: .agentFinished,
                symbol: "checkmark.seal",
                title: "Agent finished",
                detail: event.notificationMessage,
                severity: .success)
        default:
            return NoticeItem(
                kind: .other,
                symbol: "bell",
                title: type.replacingOccurrences(of: "_", with: " ").capitalized,
                detail: event.notificationMessage,
                severity: .info)
        }
    }

    private static func endNotice(for event: Event) -> NoticeItem {
        if event.isTurnComplete {
            return NoticeItem(
                kind: .turnComplete,
                symbol: "checkmark.circle",
                title: "Turn complete",
                // A boundary, not a bearer: the hook's copy of the message
                // rendered here, clamped, directly above the transcript's own
                // message row — the same words twice, which read as a
                // duplicated render. The transcript row is the record
                // (`event.rs` calls transcripts authoritative and slightly
                // lagging), and a briefly content-less boundary is honest
                // where an adjacency fallback would flicker.
                detail: nil,
                severity: .success)
        }
        if let code = event.exitCode {
            return NoticeItem(
                kind: .sessionEnded,
                symbol: code == 0 ? "power" : "exclamationmark.octagon",
                title: code == 0 ? "Session ended" : "Session ended (exit \(code))",
                detail: nil,
                severity: code == 0 ? .info : .failure)
        }
        return NoticeItem(
            kind: .sessionEnded, symbol: "power", title: "Session ended", detail: nil,
            severity: .info)
    }
}

// MARK: - Helpers

extension Event {
    fileprivate func item(_ content: TimelineItem.Content, suffix: String = "") -> TimelineItem {
        TimelineItem(id: id + suffix, seq: seq, date: date, content: content)
    }
}
