import Foundation

/// Where a native command's sheet stands: at rest, typing the command, waiting for Claude Code's own receipt, or
/// settled.
public enum CommandPhase: Equatable, Sendable {
    case idle
    /// The value being typed on the computer right now.
    case sending(String)
    /// Keystrokes landed; Claude Code has not yet answered in the transcript. `quietly` flips once the usual window
    /// has passed: the same watch, an honest caption. **It never becomes a failure** — a receipt that arrives late
    /// still has to be able to settle the sheet.
    case waiting(request: String, quietly: Bool)
    case confirmed(String)
    /// Terminal, and neither success nor failure: the daemon replayed a settled mutation that carries no outcome. A
    /// green tick over it would be the exact lie this app exists not to tell; a warning would invent a fault that is
    /// not there.
    case neutral(String)
    case failed(String)

    /// **Quiet waiting is not busy.** The watch stays armed for a receipt that may still arrive, but once the usual
    /// window has passed the controls come back: a sheet whose every row is inert, with a spinner running and no
    /// statement that only dismissing it will help, is a dead end. A receipt that never arrives — reworded copy, a
    /// gap reset, a dropped socket — must not cost the reader the sheet.
    public var busy: Bool {
        switch self {
        case .sending, .waiting(_, quietly: false): return true
        case .waiting(_, quietly: true), .idle, .confirmed, .neutral, .failed: return false
        }
    }
}

/// A Claude Code command a native sheet types for the reader.
public enum NativeCommand: Sendable, Hashable {
    case compact(instructions: String)

    /// What a phase carries for this command: the instructions.
    public var request: String {
        switch self {
        case .compact(instructions: let value): return value
        }
    }

    /// How long the sheet's caption promises an answer before it goes quiet. The observation window ends; the
    /// observation does not. A real compaction measured about 6 s on a small context and grows with the context.
    public var quietAfter: Duration { .seconds(10) }

    /// A duplicate response proves the mutation was typed once and settled; it does not carry what Claude Code then
    /// did, so this claims neither a change nor the absence of one.
    public static let alreadySentLine = "This request was sent earlier; this retry does not confirm its outcome."

    /// The phase a send leads to at once, or nil when the keystrokes landed and only Claude Code's own receipt can
    /// settle it: landing proves typing, not execution. Any waiting phase that is not quiet starts the `quietAfter`
    /// window.
    public func phase(after attempt: ComposeAttempt) -> CommandPhase? {
        switch attempt {
        case .sent:
            return nil
        case .alreadyApplied:
            // The daemon replayed a settled mutation without typing, so no new
            // transcript line is coming. It proves the keys were typed once and
            // nothing else — in particular, the settled outcome it is replaying
            // may have been a cancelled confirmation.
            return .neutral(Self.alreadySentLine)
        case .refused(let reason):
            return .failed("Nothing was typed: \(reason)")
        case .failed(let reason):
            return .failed("Couldn’t type the command: \(reason)")
        case .indeterminate(let reason):
            return .failed("Not confirmed: \(reason) Retry is safe; the command won’t be typed twice.")
        }
    }

    /// The agent's own answer to this command above `baseline`, as a terminal phase, or nil while there is none: Claude
    /// Code's receipt; for Antigravity, which runs `/compact` as a prompt, that prompt's own turn ending, confirmed only
    /// when agy ended it as done. The sequence fences out a backfill that publishes an older receipt.
    func receiptPhase(
        from receipts: SessionState.Receipts?, after baseline: UInt64, agent: AgentKind = .claude
    ) -> CommandPhase? {
        switch self {
        case .compact where agent == .antigravity:
            guard let turn = receipts?.compactTurn, turn.sentSeq > baseline, let reason = turn.endReason else {
                return nil
            }
            return reason == "end_turn"
                ? .confirmed("Antigravity compacted its context.")
                : .failed("Antigravity didn’t finish compacting.")
        case .compact:
            guard let signal = receipts?.compact, let seq = receipts?.compactSeq, seq > baseline else { return nil }
            switch signal {
            case .compacted: return .confirmed("Claude Code confirmed the compaction.")
            case .notEnoughMessages: return .failed("Not enough messages to compact.")
            }
        }
    }
}

extension AppModel {
    /// The agent's own answer to `command` in this run since `baseline` (`eventHighWater(for:)`, taken just before
    /// the send), as a terminal phase, or nil while there is none.
    public func receiptPhase(
        for command: NativeCommand, in key: String, after baseline: UInt64, agent: AgentKind = .claude
    ) -> CommandPhase? {
        command.receiptPhase(from: receipts(for: key), after: baseline, agent: agent)
    }

    /// What a command's status says in `phase`, naming the computer in the words of the device reading it; nil at
    /// rest. Typing and waiting are neutral; a settled phase is its own words, a change confirmed, a kept value
    /// neutral, a failure a warning.
    public func commandStatus(
        _ command: NativeCommand, _ phase: CommandPhase, agent: AgentKind = .claude, on host: HostDevice = .current
    ) -> ComposeProse.Note? {
        let computer = host.computer("the computer", named: machineName)
        let typed: String
        switch command {
        case .compact: typed = "/compact"
        }
        switch phase {
        case .idle:
            return nil
        case .sending:
            return ComposeProse.Note(text: "Typing \(typed) on \(computer)…", tone: .neutral, icon: nil)
        case .waiting(_, quietly: false):
            return ComposeProse.Note(text: "Waiting for \(agent.productName) to confirm…", tone: .neutral, icon: nil)
        case .waiting(_, quietly: true):
            return ComposeProse.Note(
                text: "Compaction requested on \(computer). CodeConnect has not received a completion signal.",
                tone: .neutral, icon: nil)
        case .confirmed(let line):
            return ComposeProse.Note(text: line, tone: .success, icon: nil)
        case .neutral(let line):
            return ComposeProse.Note(text: line, tone: .neutral, icon: nil)
        case .failed(let line):
            return ComposeProse.Note(text: line, tone: .warning, icon: nil)
        }
    }
}

/// The words of the confirmation `/clear` asks for first.
public enum ClearConfirmation {
    public static let ask = AskWords(
        title: "Clear Claude’s context?",
        items: [.init(mark: "trash", words: "Claude Code forgets this conversation’s context")],
        note: "CodeConnect’s timeline stays here.", cancel: "Keep Context", confirm: "Clear Context")
}

/// The words of the control `/compact` opens, naming the run's agent.
public enum CompactPrompt {
    public static let title = "Compact context"
    public static func explanation(for agent: AgentKind = .claude) -> String {
        "Ask \(agent.productName) to compact its current context. Instructions are optional."
    }
    public static let fieldLabel = "Instructions (optional)"
    public static func placeholder(for agent: AgentKind = .claude) -> String {
        "What should \(agent.displayName) preserve?"
    }
    public static let action = "Compact now"
}
