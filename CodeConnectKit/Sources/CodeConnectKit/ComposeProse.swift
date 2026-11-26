import Foundation

/// What the line above a run's composer says: what became of the last thing sent, or why sending cannot work.
public enum ComposeProse {
    /// One line above a run's composer: the words, how serious they are, the mark beside them (the icon names
    /// `CodexProse.Banner.icon` uses), and whether it offers "Why?" (observe only).
    public struct Note: Sendable, Hashable {
        public let text: String
        public let tone: Tone
        public let icon: String?
        public let offersWhy: Bool

        init(text: String, tone: Tone, icon: String?, offersWhy: Bool = false) {
            self.text = text
            self.tone = tone
            self.icon = icon
            self.offersWhy = offersWhy
        }
    }

    /// How serious a note is, free of any view: each app maps it to its own colours.
    public enum Tone: Sendable, Hashable {
        case neutral, info, success, warning, danger
    }

    /// Whether Return, with these keys held, adds a line in a box that sends to an agent, as every message box does:
    /// ⇧Return and ⌥Return add a line; Return alone sends, and with ⌘ or ⌃ it is the box's own.
    public static func returnAddsLine(shift: Bool, option: Bool, command: Bool, control: Bool) -> Bool {
        (shift || option) && !command && !control
    }

    /// What became of a Claude send, or nil when it landed: the message appearing in the conversation says so.
    public static func note(_ attempt: ComposeAttempt) -> Note? {
        switch attempt {
        case .sent, .alreadyApplied:
            return nil
        case .refused(let reason):
            return Note(text: "Not typed: \(reason)", tone: .warning, icon: "warning-fill")
        case .failed(let reason):
            return Note(text: "Couldn’t type the message: \(reason)", tone: .danger, icon: "x-circle-fill")
        case .indeterminate(let reason):
            return Note(
                text: "Not confirmed: \(reason) Retry is safe; the message won’t be typed twice.",
                tone: .warning, icon: "question-fill")
        }
    }

    /// A settled Codex stop or message, on one line.
    static func note(_ banner: CodexProse.Banner) -> Note {
        Note(text: banner.oneLine, tone: Tone(banner.tone), icon: banner.icon)
    }
}

extension ComposeProse.Tone {
    init(_ tone: CodexProse.CodexTone) {
        switch tone {
        case .info: self = .info
        case .success: self = .success
        case .warning: self = .warning
        case .danger: self = .danger
        }
    }
}

extension ComposeAttempt {
    /// The words reached the run — typed, or typed earlier: the draft clears. Never optimistic: nothing else clears
    /// the draft.
    public var landed: Bool {
        switch self {
        case .sent, .alreadyApplied: return true
        case .refused, .failed, .indeterminate: return false
        }
    }

    /// A note the next edit retires: editing is the reader's next act, so a standing failure has been read. A landed
    /// outcome has no note.
    public var clearsOnEdit: Bool {
        switch self {
        case .refused, .failed, .indeterminate: return true
        case .sent, .alreadyApplied: return false
        }
    }
}

extension ComposeResult {
    /// A Codex message that landed once: started, steered, or a replay of one that did. The draft is kept on a
    /// refusal and on an outcome nobody can account for, so the reader still holds their own words.
    public var landed: Bool {
        switch self {
        case .started, .steered, .duplicate: return true
        case .rejected, .indeterminate, .unknown: return false
        }
    }
}

/// A composer send whose answer never came: what it said, and when it was sent.
public struct UnansweredSend: Sendable, Equatable {
    public let text: String
    public let sentAt: Date

    public init(text: String, sentAt: Date) {
        self.text = text
        self.sentAt = sentAt
    }
}

extension AppModel {
    /// Where a draft goes when it is sent: a Codex run's own message, Claude's typed text, or a slash command the
    /// app answers with its own control before anything reaches the computer.
    public enum Submission: Equatable {
        case codex
        case claude
        case command(CommandAction)
    }

    /// **A Codex run takes a different road entirely**: `send_text` is Claude's typed text and a Codex run refuses
    /// it by name, while `compose` is a message, not keystrokes. On Claude, slash commands answer to
    /// `ClaudeCommandPolicy` first: native commands open their controls, and everything else — prose, Claude's other
    /// commands, custom skills — is typed as written.
    public func submission(of draft: String, to key: String) -> Submission {
        if shownSummary(for: key)?.isCodex == true { return .codex }
        let action = ClaudeCommandPolicy.action(for: draft)
        // An Antigravity run takes the words as they are, through its agent host, but for `/compact`, which opens
        // Compact's own controls as it does on Claude (agy runs it as a prompt; typed, the sender refuses it).
        if shownSummary(for: key)?.agent == .antigravity {
            if case .nativeCompact = action { return .command(action) }
            return .claude
        }
        if case .passThrough = action { return .claude }
        return .command(action)
    }

    /// What became of the last thing sent to this run, or nil.
    ///
    /// A Codex outcome outranks the last Claude send, because on a Codex run the Claude vocabulary — "not typed" —
    /// describes machinery that is not there. A stop and a message share
    /// this one slot, where the reader is looking after they act.
    public func outcomeNote(for key: String, lastSend: ComposeAttempt?) -> ComposeProse.Note? {
        let summary = shownSummary(for: key)
        // **A Stop's outcome shows for any agent that can be stopped** — a Codex
        // run or a hosted Claude run — because the reader is looking at this slot
        // right after they tapped Stop. It used to sit behind `isCodex`, so a
        // Claude Stop executed but its answer never appeared here.
        let stop = codexStop(for: key)
        if case .settled(let result) = stop {
            return ComposeProse.note(CodexProse.interrupt(result, agent: summary?.agent ?? .codex))
        }
        if case .notSent(let reason) = stop {
            return ComposeProse.Note(text: reason, tone: .warning, icon: "warning-circle-fill")
        }
        if case .sentNoAnswer(let reason) = stop {
            return ComposeProse.Note(text: reason, tone: .warning, icon: "question-fill")
        }
        // **Compose is a Codex-only road** (`send_text` is Claude's; `compose` is a
        // Codex message), so its outcomes stay behind that gate.
        if summary?.isCodex == true {
            let compose = codexControls(for: key).compose
            if case .settled(let result) = compose { return CodexProse.compose(result).map(ComposeProse.note) }
            if case .notSent(let reason) = compose {
                return ComposeProse.Note(text: reason, tone: .warning, icon: "warning-circle-fill")
            }
            if case .sentNoAnswer(let reason) = compose {
                return ComposeProse.Note(text: reason, tone: .warning, icon: "question-fill")
            }
        }
        return lastSend.flatMap(ComposeProse.note)
    }

    /// The compose slot's note when it is an outcome or a block, in precedence (the view adds the mic
    /// failure and the neutral standing note below this). A **just-taken** outcome — a Stop the reader
    /// tapped, read with no `lastSend` so a prior send's stale verdict does not qualify — wins over the
    /// block, because the reader is looking here right after they acted. Otherwise the block (a card
    /// waiting) wins over a stale outcome (R2T3-F1), then the stale outcome itself.
    public func composeOutcomeOrBlock(for key: String, draft: String, lastSend: ComposeAttempt?)
        -> ComposeProse.Note?
    {
        if let fresh = outcomeNote(for: key, lastSend: nil) { return fresh }
        if sendBlockedReason(for: key, draft: draft) != nil,
            let note = standingNote(for: key, draft: draft)
        {
            return note
        }
        return outcomeNote(for: key, lastSend: lastSend)
    }

    /// Why sending to this run cannot work now, or nil.
    ///
    /// A send cannot work for the reasons an answer cannot: the link, and what the computer said it can do.
    /// **Except for Codex**: there is no `send_text`, so its reasons are its own and are asked first.
    /// `files`: the reply box's own send, which the run's files hold back; any other box's (a diff comment) is not theirs.
    public func sendBlockedReason(for key: String, draft: String, files: Bool = true) -> String? {
        // The sample fleet's reason, not the link's: "Not paired with a computer."
        // beside a card that answers in sample vocabulary would be two different
        // accounts of the same absence on one screen.
        if sampleFleetActive {
            let computer = HostDevice.current.computer("your computer")
            return "These agents are not real. Pair with \(computer) to talk to your own."
        }
        if let reason = linkHealth.disabledReason { return reason }
        if files, let reason = filesBlockedReason(for: key) { return reason }
        let summary = shownSummary(for: key)
        if let summary, summary.isCodex {
            // The draft's own ceiling comes first: it is the one the reader can
            // fix without anything changing at the computer, and it names the
            // number they have to cut. Only the *oversize* refusal is a reason:
            // an empty draft is a message nobody has written yet.
            let composed = ComposeDraft(text: draft)
            if composed.isOverCeiling { return composed.blockedReason }
            return composeUnavailable(for: summary.sessionKey)
        }
        if connection.capabilities?.sendText == false {
            return "\(HostDevice.current.computer("The computer"))’s CodeConnect can’t take typed text."
        }
        if let summary,
            let reason = FleetStatusRule.capability(summary: summary, capabilities: connection.capabilities).reason
        {
            return reason
        }
        return nil
    }

    /// The line above the composer when nothing was just sent: why sending cannot work, else "observe only" with the
    /// run's own reason it cannot be answered from here (offering "Why?"), else nil.
    public func standingNote(for key: String, draft: String) -> ComposeProse.Note? {
        if let reason = sendBlockedReason(for: key, draft: draft) {
            // An offline computer by name, and when a reply can go; an offline link with a cause of its own keeps it.
            let offline = reason == linkHealth.disabledReason && linkHealth.level == .offline && linkHealth.cause == nil
            let text = offline ? machineName.map(SessionWords.computerOffline) ?? reason : reason
            return ComposeProse.Note(text: text, tone: .warning, icon: "warning-circle-fill")
        }
        if let summary = shownSummary(for: key),
            let reason = FleetStatusRule.capability(summary: summary, capabilities: connection.capabilities).reason
        {
            return ComposeProse.Note(text: "Observe only. \(reason)", tone: .neutral, icon: nil, offersWhy: true)
        }
        return nil
    }

    /// Whether to ask an unanswered send once more, now that the link is back: connected, not already sending, the
    /// draft still exactly that text, within a minute of the send, and the computer still holding its identity, so
    /// it answers "already typed" or types it the one time it never arrived.
    public func asksAgain(
        _ unanswered: UnansweredSend, to key: String, draft: String, sending: Bool, now: Date = Date()
    ) -> Bool {
        !sending && connection.phase.isConnected && draft == unanswered.text
            && now.timeIntervalSince(unanswered.sentAt) < 60
            && holdsSendIdentity(text: unanswered.text, to: key)
    }

    /// The counterpart to `asksAgain`: the reader's composer draft changed from `old` to `new`.
    /// A send kept unanswered is its retry only while the draft still shows those words; once the
    /// draft moves off them — compared through `normalizedSendText`, the same normalisation the
    /// send's mutation key uses, so a whitespace-only edit is not a move — the send kept under the
    /// words the draft just left is abandoned, so the same words typed later are a new send and not
    /// the stale identity the daemon replays as "already applied". Re-sending the kept draft
    /// unchanged keeps the retry.
    public func abandonHeldSend(whenDraftChangesFrom old: String, to new: String, in key: String) {
        guard Self.normalizedSendText(old) != Self.normalizedSendText(new) else { return }
        dropSendIdentity(text: old, to: key)
    }
}
