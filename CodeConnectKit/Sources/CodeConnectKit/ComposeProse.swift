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
        case .composerRecovered(let command, _, _):
            // **Not a success.** The same daemon result on the Model and Effort
            // sheets reports that no change was confirmed, and a typed command
            // reaches this note by the identical path — `/effort high` routes
            // through here, and on a cache-warm conversation it opens a
            // confirmation that the daemon's one Esc then cancels. A green tick
            // over that is the claim this app exists not to make.
            let computer = HostDevice.current.computer("the computer")
            return Note(
                text: "After \(command) was typed, \(computer)'s composer disappeared. "
                    + "CodeConnect pressed Esc and confirmed it returned. "
                    + "The command's outcome was not confirmed.",
                tone: .warning, icon: "question-fill")
        case .composerLost(let command):
            let computer = HostDevice.current.computer("the computer")
            return Note(
                text: "\(command) was typed, but \(computer)'s composer did not come back. "
                    + "Open Terminal to recover.",
                tone: .danger, icon: "warning-fill")
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
    /// The words reached the run — typed, typed earlier, or typed with the composer recovered after: the draft
    /// clears, and the note, which only a recovered composer has, retires by itself. Never optimistic: nothing else
    /// clears the draft.
    public var landed: Bool {
        switch self {
        case .sent, .alreadyApplied, .composerRecovered: return true
        case .refused, .failed, .indeterminate, .composerLost: return false
        }
    }

    /// A note the next edit retires: editing is the reader's next act, so a standing failure has been read. A lost
    /// composer stands until another attempt, and a landed outcome's note retires by itself.
    public var clearsOnEdit: Bool {
        switch self {
        case .refused, .failed, .indeterminate: return true
        case .sent, .alreadyApplied, .composerRecovered, .composerLost: return false
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

    /// **A Codex run takes a different road entirely**: `send_text` is Claude's TTY takeover and a Codex run refuses
    /// it by name, while `compose` is a message, not keystrokes. On Claude, slash commands answer to
    /// `ClaudeCommandPolicy` first: native commands open their controls, dialog built-ins get the honest refusal,
    /// and everything else — prose, custom skills — is typed as written.
    public func submission(of draft: String, to key: String) -> Submission {
        if shownSummary(for: key)?.isCodex == true { return .codex }
        let action = ClaudeCommandPolicy.action(
            for: draft, recoversComposer: connection.capabilities?.recoversComposer == true)
        if case .passThrough = action { return .claude }
        return .command(action)
    }

    /// What became of the last thing sent to this run, or nil.
    ///
    /// A Codex outcome outranks the last Claude send, because on a Codex run the Claude vocabulary — "not typed",
    /// "the composer did not come back" — describes machinery that is not there. A stop and a message share
    /// this one slot, where the reader is looking after they act.
    public func outcomeNote(for key: String, lastSend: ComposeAttempt?) -> ComposeProse.Note? {
        if shownSummary(for: key)?.isCodex == true {
            let stop = codexStop(for: key)
            if case .settled(let result) = stop { return ComposeProse.note(CodexProse.interrupt(result)) }
            if case .notSent(let reason) = stop {
                return ComposeProse.Note(text: reason, tone: .warning, icon: "warning-circle-fill")
            }
            if case .sentNoAnswer(let reason) = stop {
                return ComposeProse.Note(text: reason, tone: .warning, icon: "question-fill")
            }
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

    /// Why sending to this run cannot work now, or nil.
    ///
    /// Text only lands if the composer is actually on screen at the computer, so the reasons a send cannot work are
    /// the reasons an answer cannot. **Except for Codex**: there is no composer, no keystrokes and no `send_text`, so
    /// its reasons are its own and are asked first, and a Codex run is never refused with a sentence about a TTY it
    /// does not have.
    public func sendBlockedReason(for key: String, draft: String) -> String? {
        // The sample fleet's reason, not the link's: "Not paired with a computer."
        // beside a card that answers in sample vocabulary would be two different
        // accounts of the same absence on one screen.
        if sampleFleetActive {
            let computer = HostDevice.current.computer("your computer")
            return "These agents are not real. Pair with \(computer) to talk to your own."
        }
        if let reason = linkHealth.disabledReason { return reason }
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
            return "\(HostDevice.current.computer("The computer"))'s CodeConnect can't take typed text."
        }
        if let summary,
            let reason = FleetStatusRule.capability(summary: summary, capabilities: connection.capabilities).reason
        {
            return reason
        }
        return nil
    }

    /// The line above the composer when nothing was just sent: why sending cannot work, else "observe only"
    /// (offering "Why?"), else nil.
    public func standingNote(for key: String, draft: String) -> ComposeProse.Note? {
        if let reason = sendBlockedReason(for: key, draft: draft) {
            return ComposeProse.Note(text: reason, tone: .warning, icon: "warning-circle-fill")
        }
        if let summary = shownSummary(for: key),
            !FleetStatusRule.capability(summary: summary, capabilities: connection.capabilities).canAct
        {
            let computer = HostDevice.current.computer("the computer")
            return ComposeProse.Note(
                text: "Observe only - answers are given at \(computer).", tone: .neutral, icon: nil, offersWhy: true)
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
}
