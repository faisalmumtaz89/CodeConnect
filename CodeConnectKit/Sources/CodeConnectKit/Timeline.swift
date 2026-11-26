import Foundation

// MARK: - Items

public struct ToolItem: Sendable, Hashable {
    public var toolUseID: String?
    public var name: String
    public var argument: String?
    public var status: ToolStatus
    public var durationMS: Int?
    public var input: JSONValue?
    public var output: String?
    public var isTruncated: Bool
}

public struct ApprovalItem: Sendable, Hashable, Identifiable {
    public var card: ApprovalCard
    public var requestedAt: Date
    /// Which *run* this card belongs to. Carried on the item because the Deck
    /// mixes cards from the whole fleet, where "the session you are looking at"
    /// is not a thing that exists — and because the answer has to name the run
    /// it came from, or a daemon holding two cards with one `request_id` cannot
    /// tell which of them was tapped.
    public var sessionKey: String
    /// `nil` while nobody has answered. Silence is never consent.
    public var outcome: AnswerOutcome?
    /// What became of a **Codex** card, which the wire reports in an entirely
    /// different shape — a bare `CodexResolution` carrying no decision, no
    /// actor's transport and no timestamp on most arms.
    ///
    /// Kept beside `outcome` rather than mapped onto it, because the two say
    /// different things and mapping would have to invent the difference: a
    /// `cleared(turn_aborted)` is not "denied", an answer may name no decision
    /// at all, and `AnswerOutcome` has no way to express either
    /// without claiming something. Both make the card non-actionable; only this
    /// one is allowed to describe a Codex ending.
    public var codexResolution: CodexResolution?
    /// The daemon's own classification, or nil on a daemon too old to send one.
    public var risk: WireRisk?
    /// Whether the phone can answer this card now: the card's own
    /// `question_hold`. Nil from a daemon below minor 21.
    public var questionHold: QuestionHold? = nil

    /// Whether this card is still waiting on a human. **Either** ending
    /// retires it: a card the Mac already answered is not pending just because
    /// the ending arrived in Codex's shape rather than Claude's.
    public var isPending: Bool { outcome == nil && codexResolution == nil }

    /// The run *and* the request, because a `request_id` is only unique within
    /// one run. Two runs of the same project can raise the same id — the daemon
    /// says so in `ccd/src/state.rs`, where an unscoped answer to a shared id is
    /// refused rather than guessed at — and the Deck mixes the whole fleet into
    /// one list.
    public var id: String { "\(sessionKey)#\(card.requestID)" }

    /// The gate this card actually gets. See `RiskAssessment` for why the two
    /// classifiers are combined by taking the stricter one.
    public func assessment(profile: DaemonProfile) -> RiskAssessment {
        RiskAssessment.resolve(
            wire: risk, tool: card.toolName, input: card.toolInput, profile: profile)
    }
}

/// What a notice *means*, separate from what it says. Fleet status is derived
/// from this, never from the display copy — rewording a string must not be able
/// to change which band a session sorts into.
public enum NoticeKind: Sendable, Hashable {
    /// A `Stop` hook: the turn finished, the session is still alive.
    case turnComplete
    /// The person stopped the turn at the Mac. A boundary, like `turnComplete`:
    /// after an Esc, Claude writes no other end.
    case turnInterrupted
    case sessionEnded
    /// The run starting again after its first start: a restart or resume under
    /// the reader. Not work, so not activity.
    case sessionStart
    /// A background task's report handed to the agent. Claude Code delivers it
    /// as input, which wakes the agent — a turn the report starts, or the one it
    /// lands in — so it is activity, and never a boundary. A report recorded
    /// without being delivered (`queueTranscriptOnly`) is `.other`: it starts
    /// nothing.
    case taskReport
    /// Observation-about-our-observation; never evidence about the agent.
    case link
    case failure
    case other
}

/// One notice, as the timeline draws it. Its title and detail are often
/// someone else's words — a notification's message, a background task's
/// summary, an error, a command's output — so both are held `displaySafe`.
public struct NoticeItem: Sendable, Hashable {
    public var kind: NoticeKind
    public var symbol: String
    public var title: String
    public var detail: String?
    public var severity: NoticeSeverity
    /// A turn end that closes a command's own turn (`local_command`). The item stays, for the fleet's status; the
    /// session screen draws no row for it (`TimelineBuilder.drawn`), as a terminal run files no turn end there.
    public var endsCommand = false

    init(kind: NoticeKind, symbol: String, title: String, detail: String?, severity: NoticeSeverity) {
        self.kind = kind
        self.symbol = symbol
        self.title = title.displaySafe
        self.detail = detail?.displaySafe
        self.severity = severity
    }

    /// An agent woken to read its CodeConnect messages: a Codex wake line, or ccd's `mail_nudge` notice on a Claude run.
    /// A task report, so a woken run reads Running until the agent replies.
    static let mailWake = NoticeItem(
        kind: .taskReport, symbol: "chat-text", title: "Reading CodeConnect messages", detail: nil, severity: .info)

    /// The conversation was reset: a terminal run's `/clear` invocation, or ccd's `conversation_cleared` notice on a
    /// hosted run.
    static let conversationCleared = NoticeItem(
        kind: .other, symbol: "eraser", title: "Conversation cleared.", detail: nil, severity: .info)
}

public struct TimelineItem: Sendable, Hashable, Identifiable {
    public enum Content: Sendable, Hashable {
        case userMessage(String, isCommand: Bool = false)
        /// `isInterrupted` is the wire's own `interrupted` flag on a Codex
        /// message item: the reply was cut short by an abort, and a row that
        /// draws it as a finished thought is the phone contradicting the Mac.
        case agentMessage(String, isInterrupted: Bool = false)
        case tool(ToolItem)
        case approval(ApprovalItem)
        case notice(NoticeItem)
    }

    public let id: String
    public let seq: UInt64
    public let date: Date
    public let content: Content
    /// A person's message a Stop cancelled ("Not sent: stopped"): set only on a
    /// `userMessage` row whose send a `send_not_sent` notice named. Every other row
    /// is false. The row model carries it rather than the enum, so nothing that
    /// pattern-matches `Content` changes.
    public let notSent: Bool

    public init(id: String, seq: UInt64, date: Date, content: Content, notSent: Bool = false) {
        self.id = id
        self.seq = seq
        self.date = date
        self.content = content
        self.notSent = notSent
    }

    var pendingApproval: ApprovalItem? {
        if case .approval(let item) = content, item.isPending { return item }
        return nil
    }

    /// Each row's place in a column that keeps the reader's row while a page of earlier history lands, in the order of
    /// `items`. A tool call is one row built from whichever of its events the loaded window holds (its call, the
    /// transcript's `tool_use`, or its result alone), and its id is that event's, so the id changes when the page brings
    /// the call; its place is the call itself (`toolUseID`), the same before and after. Every other row is placed by its
    /// id: a tool row with no `toolUseID`, a second row of a call already placed (two results drawn while their call is
    /// not loaded), and anything that is not a tool. Unique within `items`.
    public static func places(_ items: [TimelineItem]) -> [String] {
        var calls: Set<String> = []
        return items.map { item in
            guard case .tool(let tool) = item.content, let call = tool.toolUseID, calls.insert(call).inserted else {
                return item.id
            }
            return "tool.\(call)"
        }
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
public enum TimelineBuilder {
    /// The seq of the newest prompt handed to the agent (`Event.startsTurn`),
    /// drawn or not, or 0 when there is none. A prompt Claude Code annulled is
    /// none: resuming a session whose last prompt went unanswered, it writes
    /// its placeholder reply, and that prompt will never be answered.
    static func lastPromptSeq(_ events: [Event]) -> UInt64 {
        for event in events.reversed() {
            if event.isClaudePlaceholderReply { return 0 }
            if event.startsTurn { return event.seq }
        }
        return 0
    }

    /// **The rows the session screen draws: every item, less a "Turn complete"
    /// that something on screen already ends.**
    ///
    /// A turn that ends on Claude's words needs no second line saying so, nor
    /// one that ends on "Interrupted", which says where the turn ended. A
    /// command's own turn end (`/clear`, `/compact`, `/effort low` on a run
    /// CodeConnect started) draws none either, as a terminal run files none. One
    /// that ends on a tool call, a card or nothing at all has no other end on
    /// screen, so its line stays. Only the row goes: the item stays in the timeline, where fleet
    /// status reads it.
    public static func drawn(_ items: [TimelineItem]) -> [TimelineItem] {
        items.enumerated().compactMap { index, item in
            guard case .notice(let notice) = item.content, notice.kind == .turnComplete else { return item }
            if notice.endsCommand { return nil }
            guard index > 0 else { return item }
            switch items[index - 1].content {
            case .agentMessage: return nil
            case .notice(let before) where before.kind == .turnInterrupted: return nil
            // A command's output row (`terminal-window`, its only user) already ends the turn on screen,
            // so a hosted command whose end carries no `local_command` — a declined `/help`, whose words
            // are the daemon's not claude's — draws no second "Turn complete" after it.
            case .notice(let before) where before.symbol == "terminal-window": return nil
            default: return item
            }
        }
    }

    /// `accountName` names an account by its id (nil: the agent's Default) as this app's account book knows it, or nil
    /// when it does not; a failed switch's row reads it, else the label its event carries.
    static func build(_ events: [Event], accountName: (String?) -> String? = { _ in nil }) -> [TimelineItem] {
        guard !events.isEmpty else { return [] }

        var resultsByToolUse: [String: ToolOutcome] = [:]
        var outcomesByRequest: [String: AnswerOutcome] = [:]
        var codexResolutionsByRequest: [String: CodexResolution] = [:]
        var hookToolCallIDs: Set<String> = []
        // Where Claude's transcript puts each call. The hook files a call when
        // Claude runs it, which for a prompt queued during a turn can be before
        // the daemon has read the last turn's end; the transcript has the call
        // after that end, in the turn it belongs to.
        var transcriptToolUseSeqs: [String: UInt64] = [:]
        /// Claude's questions, by tool-use id. A question is one thing to the
        /// reader: its card says everything its tool call does — the question,
        /// and how it ended — and the call's own duration is Claude's, not the
        /// minute the reader took to answer.
        var questionToolUseIDs: Set<String> = []
        var lastTurnEndSeq: UInt64 = 0
        // The sends a Stop cancelled, by request_id: their person-message rows read
        // "Not sent: stopped" and the `send_not_sent` notice itself draws no row.
        var stoppedSends: Set<String> = []

        for event in events {
            switch event.kind {
            case .toolCall:
                if let id = event.toolUseID { hookToolCallIDs.insert(id) }
            case .agentMessage:
                for use in event.agentToolUses { transcriptToolUseSeqs[use.id] = event.seq }
            case .toolResult:
                absorbResult(event, into: &resultsByToolUse)
            case .approvalResolved:
                if let outcome = event.approvalOutcome {
                    outcomesByRequest[outcome.requestID] = outcome
                } else if let requestID = event.codexResolvedRequestID,
                    let resolution = event.codexResolution
                {
                    // The second arm, and the order matters: Claude's shape is
                    // tried first and wins where both could apply, so the Codex arm
                    // cannot change what a Claude session does. A Codex payload
                    // fails the first read (it has no `request_id` *inside* an
                    // `AnswerOutcome`, and none of that struct's other required
                    // fields) and lands here.
                    codexResolutionsByRequest[requestID] = resolution
                }
            case .approvalRequest:
                if let card = event.approvalCard, card.toolName == QuestionCard.toolName {
                    questionToolUseIDs.insert(card.requestID)
                }
            case .notification:
                if let requestID = event.sendNotSentStoppedRequestID {
                    stoppedSends.insert(requestID)
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
        /// Whether the link has been anything but attached. Until then an
        /// `attached` is the agent host registering at spawn, which says
        /// nothing to the reader; after it, `attached` is the recovery.
        var linkWasDown = false
        /// Whether the run has ended. Its agent host's link coming and going
        /// around the end — it lets go of the daemon to say the run is over —
        /// is the ending itself, which the end's own row already says; until a
        /// new start, the link says nothing about anything still running.
        var runEnded = false
        /// Events that observe the session and say nothing about its turns.
        let observations: [EventKind] = [.linkState, .error, .notification]
        /// The session's own start is the top of the timeline and draws no
        /// row: the first start in a history that begins at seq 1, with only
        /// observations before it. The daemon numbers a run's events from 1
        /// with no gaps, so such a history is the whole run and everything
        /// before the start is in hand. A cold open loads only the newest
        /// events, so a start first in view can be a restart under history not
        /// yet loaded, and one after any of the conversation is a restart under
        /// it; either says something.
        var atSessionTop = events.first?.seq == 1
        /// The run was moved to another account and has not started again yet: the start that follows is that
        /// restart, which says nothing to the reader.
        var switched = false

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
                    case .invocation(let command, _):
                        if let text = local.invocationText {
                            // `/clear` rotates the transcript: Claude Code
                            // opens a NEW file whose first user entry is this
                            // very invocation (measured), so its arrival IS
                            // the observable completion — the daemon is
                            // following the fresh conversation. Rendered as
                            // the fact it proves rather than as typed input.
                            // By name: it takes an optional one (`/clear
                            // next-task`), and `/new` and `/reset` are recorded
                            // as `/clear` (2.1.289).
                            if command == "/clear" {
                                items.append(event.item(.notice(.conversationCleared)))
                            } else {
                                items.append(
                                    event.item(
                                        .userMessage(text, isCommand: true),
                                        notSent: event.sendRequestID.map(stoppedSends.contains) ?? false))
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
                                            symbol: "terminal-window",
                                            title: title,
                                            detail: text,
                                            severity: .info))))
                        }
                    }
                } else if event.isClaudeInterrupt {
                    // The Mac's own row for it, in the Mac's words.
                    items.append(
                        event.item(
                            .notice(
                                NoticeItem(
                                    kind: .turnInterrupted,
                                    symbol: "stop-circle",
                                    title: "Interrupted · What should Claude do instead?",
                                    detail: nil,
                                    severity: .info))))
                } else if let notification = event.taskNotification {
                    // The Mac's one row: `⏺ <summary> · <time>`, its dot
                    // coloured by the task's status. A report with no summary
                    // draws nothing, as it says nothing the reader can use.
                    if let line = notification.line {
                        items.append(
                            event.item(
                                .notice(
                                    NoticeItem(
                                        kind: event.isTranscriptOnly ? .other : .taskReport,
                                        symbol: "circle-fill",
                                        title: line,
                                        detail: nil,
                                        severity: notification.severity))))
                    }
                } else if event.source == .codex, let text = event.userText,
                    MailNudge.all.contains(text) {
                    // ccd files a Codex mail wake (protocol::ipc MAIL_NUDGE_*) as the turn's
                    // user message, but it is not the person speaking — so it draws as the
                    // mail notice, like `/clear` above, never a `you:` bubble. `.taskReport`
                    // (not `.other`) keeps a mail-woken run Running: Codex emits no
                    // turn-start event, so the wake line is the only activity until the
                    // agent's first reply.
                    items.append(event.item(.notice(.mailWake)))
                } else if event.sentCommand == "clear" {
                    // A `/clear` an app sent a run CodeConnect started draws no row of its own: ccd's
                    // `conversation_cleared` notice is the row, as a terminal run's clear draws the notice alone.
                } else if event.isPersonsWords, let text = event.userText {
                    // A command an app sent (`command`) draws as a command row, as a terminal run's invocation
                    // does; it starts its turn by `Event.startsTurn`, so the run reads Running until that turn ends.
                    items.append(
                        event.item(
                            .userMessage(text, isCommand: event.sentCommand != nil),
                            notSent: event.sendRequestID.map(stoppedSends.contains) ?? false))
                }

            case .agentMessage:
                // Claude's resume placeholder is not a reply: no row, and so
                // no activity on the fleet.
                if event.isClaudePlaceholderReply { break }
                if let text = event.agentText {
                    items.append(
                        event.item(.agentMessage(text, isInterrupted: event.isInterruptedItem)))
                }
                // Only when the PreToolUse hook missed the call entirely.
                for use in event.agentToolUses
                where !hookToolCallIDs.contains(use.id) && !questionToolUseIDs.contains(use.id) {
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
                if let id, questionToolUseIDs.contains(id) { break }
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
                                    for: id, outcome: outcome,
                                    eventSeq: id.flatMap { transcriptToolUseSeqs[$0] } ?? event.seq,
                                    approvals: outcomesByRequest, turnEnd: lastTurnEndSeq),
                                durationMS: outcome?.durationMS,
                                input: event.toolInput,
                                output: outcome?.text,
                                isTruncated: event.isTruncated))))

            case .toolResult:
                // Folded into its call above, the hook's or the transcript's,
                // unless the call was never seen.
                guard let id = event.toolUseID, !hookToolCallIDs.contains(id),
                    transcriptToolUseSeqs[id] == nil, !questionToolUseIDs.contains(id)
                else { break }
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
                                // What the result says its call was given, never the outcome as if it were the input.
                                input: event.callInput,
                                output: outcome?.text,
                                isTruncated: event.isTruncated))))

            case .other("codex_mcpToolCall"):
                // A Codex MCP call (msg_read/msg_send) is call+result in one event, filed as
                // Other by ccd; without this it drew no row (the person never saw what messages Codex
                // read or sent). Its result is an MCP content-block array, read via the shared mcpBlocksText.
                guard let call = event.codexMcpToolCall else { break }
                items.append(
                    event.item(
                        .tool(
                            ToolItem(
                                toolUseID: event.itemID,
                                name: call.name,
                                argument: ToolSummary.principalArgument(
                                    tool: call.name, input: call.input),
                                status: call.status,
                                durationMS: call.durationMS,
                                input: call.input,
                                output: call.output,
                                isTruncated: event.isTruncated))))

            case .other("codex_plan"):
                // Codex's plan, filed as Other by ccd, is the turn's reply in Plan mode: drawn as the agent's
                // prose, where it happened.
                guard let text = event.codexPlanText else { break }
                items.append(event.item(.agentMessage(text, isInterrupted: event.isInterruptedItem)))

            case .approvalRequest:
                guard let card = event.approvalCard else {
                    items.append(
                        event.item(
                            .notice(
                                NoticeItem(
                                    kind: .failure,
                                    symbol: "warning",
                                    title: "Approval request could not be read",
                                    detail: "\(HostDevice.current.computer("The computer")) sent a card this app couldn’t read.",
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
                                codexResolution: codexResolutionsByRequest[card.requestID],
                                risk: card.risk ?? event.declaredRisk,
                                questionHold: card.questionHold))))

            case .approvalResolved:
                break  // shown on the card it resolves

            case .notification:
                // A send_not_sent notice is consumed into the person's row it cancelled
                // (its `notSent`, set above), not drawn as its own row.
                if event.sendNotSentStoppedRequestID != nil { break }
                guard let notice = notice(for: event, accountName: accountName) else {
                    break
                }
                items.append(event.item(.notice(notice)))

            case .sessionEnd:
                while case .notice(let notice)? = items.last?.content, notice.kind == .link {
                    items.removeLast()
                }
                // A switch of account is no end: the run reads live throughout and nothing is drawn for it; its link
                // letting go and coming back, and its next process's start, are the switch itself. The person's message
                // that moved it is its one row.
                if event.switchedToLabel != nil {
                    switched = true
                    linkWasDown = false
                    break
                }
                switched = false
                runEnded = true
                items.append(event.item(.notice(endNotice(for: event))))

            case .turnComplete:
                var end = endNotice(for: event)
                end.endsCommand = event.endedCommand != nil
                items.append(event.item(.notice(end)))

            case .sessionStart:
                runEnded = false
                if switched {
                    switched = false
                    break
                }
                if atSessionTop { break }
                guard let title = startTitle(source: event.payload["source"]?.stringValue, origin: event.source)
                else { break }
                items.append(
                    event.item(
                        .notice(
                            NoticeItem(
                                kind: .sessionStart,
                                symbol: "play-circle",
                                title: title,
                                // The model as the agent reported it.
                                detail: event.modelName,
                                severity: .info))))

            case .linkState:
                guard !runEnded, let fact = event.linkStateFact else { break }
                let wasDown = linkWasDown
                linkWasDown = fact.link != "attached"
                if fact.link == "attached", !wasDown { break }
                items.append(
                    event.item(
                        .notice(
                            NoticeItem(
                                kind: .link,
                                symbol: fact.link == "attached" ? "link" : "link-simple",
                                title: "Link \(fact.link)",
                                detail: fact.reason,
                                severity: fact.link == "attached" ? .info : .warning))))

            case .error:
                items.append(
                    event.item(
                        .notice(
                            NoticeItem(
                                kind: .failure,
                                symbol: "warning-octagon",
                                title: "Error",
                                detail: event.payload.firstString("message", "error")
                                    ?? event.payload.prettyJSONString,
                                severity: .failure))))

            case .resync, .usage, .reasoning, .other:
                // resync is a banner, not a row; usage/reasoning are noise
                break
            }

            if !observations.contains(event.kind) { atSessionTop = false }
        }

        return items
    }

    /// What a start that draws a row says, by what Claude Code reports started
    /// it (2.1.289: `startup`, `resume`, `clear`, `compact`, `fork`). A resume
    /// or a compaction is the same session going on; `/clear` already has its
    /// row, "Conversation cleared.", so its start draws none.
    ///
    /// A hosted run's agent host files a start for every init claude sends: a
    /// launch's first says `startup`, a clear's says `clear`, and each later
    /// turn's says nothing, so a hosted start with no source is a turn going
    /// on and draws none. A Codex start never says what started it (ccd's Codex
    /// adapter files the thread's folder, rollout and CLI version only), so it
    /// keeps its row.
    private static func startTitle(source: String?, origin: EventSource) -> String? {
        switch source {
        case "resume": return "Run resumed"
        case "compact": return "Conversation compacted"
        case "clear": return nil
        case nil where origin == .sdk: return nil
        default: return "Run started"
        }
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
        // **The Codex adapter's own result shape**, which is not the hook's:
        // `tool_result_payload` writes `status`, `interrupted`, `exit_code` and
        // `aggregated_output` at the top level, and there is no `tool_response`
        // for the block above to read. Left unread, every Codex command in the
        // daemon's own capture rendered as "done" — including a non-zero exit,
        // which is the outcome a reader most needs the truth about.
        if let codex = event.codexToolOutcome {
            outcome.status = codex.status
            if let text = codex.output, outcome.text == nil { outcome.text = text }
        }
        if let duration = event.durationMS { outcome.durationMS = duration }

        if let transcript = event.transcriptToolResult {
            if transcript.isError { outcome.status = .failed }
            if outcome.text == nil { outcome.text = transcript.text }
        }
        if event.isTruncated { outcome.text = "(output too large; \(HostDevice.current.computer("the computer")) cut it short)" }

        table[id] = outcome
    }

    /// A call with no result is only "running" while there is still a turn it
    /// could belong to.
    ///
    /// A deny given from an app, and confirmed, means the call never ran, so it
    /// outranks the error result Claude then files for it. A deny the computer
    /// recorded is no such proof (a Yes at the Mac followed by Esc records one),
    /// so the result keeps its reading.
    private static func status(
        for toolUseID: String?, outcome: ToolOutcome?, eventSeq: UInt64,
        approvals: [String: AnswerOutcome], turnEnd: UInt64
    ) -> ToolStatus {
        if let toolUseID, approvals[toolUseID]?.resolvedBy == .phone,
            let denied = statusFromApproval(toolUseID: toolUseID, approvals: approvals)
        {
            return denied
        }
        if let outcome { return outcome.status }
        if let fromApproval = statusFromApproval(toolUseID: toolUseID, approvals: approvals) {
            return fromApproval
        }
        return eventSeq < turnEnd ? .unresolved : .running
    }

    /// The tool status an approval *alone* justifies, or `nil` when it justifies
    /// none and the timing verdict should stand. Internal (not private) and free
    /// of the private `ToolOutcome` type so the honesty rule is unit-testable.
    ///
    /// An indeterminate answer is one the daemon could not confirm reached the
    /// agent, so it is no evidence the call was blocked and must never stamp a
    /// definitive `denied` — it falls through exactly as an unrecognised decision
    /// does. A `timeout` is a card nobody answered, so its `deny` blocked nothing
    /// anyone decided, and it falls through too.
    static func statusFromApproval(
        toolUseID: String?, approvals: [String: AnswerOutcome]
    ) -> ToolStatus? {
        guard let toolUseID, let answer = approvals[toolUseID], !answer.indeterminate,
            answer.resolvedBy != .timeout
        else {
            return nil
        }
        switch answer.decision {
        case .deny, .decline: return .denied
        case .allow, .option, .optionId, .text, .answers, .unrecognised: return nil
        }
    }

    // MARK: Notices

    private static func notice(for event: Event, accountName: (String?) -> String?) -> NoticeItem? {
        let type = event.notificationType ?? "notification"
        switch type {
        case EngineNotice.conversationCleared:
            // ccd's own notice that a hosted run's `/clear` reset the conversation: the row a terminal run's clear
            // draws, once. Drawn by its type, it said the words twice, as title and detail.
            return .conversationCleared
        case EngineNotice.compacted:
            // A hosted run's compaction, which ccd files from claude's own boundary: the row a terminal run's
            // compaction draws, without the model its start carries. Drawn by its type, it read "Compacted" over it.
            return NoticeItem(
                kind: .sessionStart, symbol: "play-circle", title: "Conversation compacted", detail: nil,
                severity: .info)
        case EngineNotice.modelSet:
            // ccd's record of an effort an app set on a hosted run, which claude echoes nothing for, in the effort
            // menu's own words. Drawn by its type, it read "Model Set" over "Effort set to low".
            let effort = event.payload["effort"]?.stringValue.map(EffortConfirmation.label(for:))
            return NoticeItem(
                kind: .other, symbol: "gauge", title: effort.map { "Effort set to \($0)" } ?? "Effort set",
                detail: nil, severity: .info)
        case EngineNotice.switchFailed:
            // ccd's own notice that a run moved to another account could not start there: a warning by the account
            // it names, ccd's sentence beneath. Drawn by its type, it read a grey "Switch Failed".
            return NoticeItem(
                kind: .other, symbol: "warning",
                title: "Couldn’t move to \(accountName(event.switchedToID) ?? event.switchedToLabel ?? "the account")",
                detail: event.notificationMessage, severity: .warning)
        case EngineNotice.modelUnavailable:
            // ccd's own notice that a run moved onto a plan without its model goes on with that plan's default: one
            // quiet row from its typed names, as the switch's own row is. Drawn by its type, it read "Model Unavailable".
            let to = event.payload["to_label"]?.stringValue ?? ""
            let from = event.payload["from_label"]?.stringValue ?? ""
            let account = accountName(event.payload["account"]?["id"]?.stringValue)
                ?? event.payload["account"]?["label"]?.stringValue ?? ""
            return NoticeItem(
                kind: .other, symbol: "arrows-clockwise", title: "Now on \(to): \(from) isn’t in \(account)’s plan.",
                detail: nil, severity: .info)
        // The computer's note of a commit made from the Changes panel: how many files, and the commit. One without
        // them is drawn as any other notice.
        case EngineNotice.committed
        where event.payload["files"]?.intValue != nil && event.payload["oid"]?.stringValue != nil:
            return NoticeItem(
                kind: .other, symbol: "check-circle",
                title: ChangesWords.committed(
                    files: event.payload["files"]?.intValue ?? 0, sha: event.payload["oid"]?.stringValue ?? ""),
                detail: nil, severity: .info)
        case EngineNotice.mailNudge:
            // ccd's own notice that it woke a Claude run for its mail; its message is the wake line, which is ccd's
            // words to the agent, not the reader's, so the row is the one a Codex wake draws.
            return .mailWake
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
                symbol: "check-circle",
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
                symbol: code == 0 ? "power" : "warning-octagon",
                title: code == 0 ? "Run ended" : "Run ended (exit \(code))",
                detail: nil,
                severity: code == 0 ? .info : .failure)
        }
        return NoticeItem(
            kind: .sessionEnded, symbol: "power", title: "Run ended", detail: nil,
            severity: .info)
    }
}

// MARK: - Helpers

extension Event {
    fileprivate func item(_ content: TimelineItem.Content, suffix: String = "", notSent: Bool = false)
        -> TimelineItem
    {
        TimelineItem(id: id + suffix, seq: seq, date: date, content: content, notSent: notSent)
    }
}
