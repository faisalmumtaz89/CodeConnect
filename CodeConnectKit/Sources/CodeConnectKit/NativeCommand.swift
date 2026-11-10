import Foundation

/// Where a native command's sheet stands: at rest, typing the command, waiting for Claude Code's own receipt, or
/// settled.
public enum CommandPhase: Equatable, Sendable {
    case idle
    /// The value being typed on the computer right now.
    case sending(String)
    /// Keystrokes landed; Claude Code has not yet answered in the transcript. `quietly` flips once the usual window
    /// has passed: the same watch, an honest caption. **It never becomes a failure** — a receipt that arrives late
    /// still has to be able to settle the sheet. A terminal "no confirmation" over a transcript that went on to say
    /// `Set model to X` is a stale claim, and stale claims are the thing these sheets may not make. The one end that
    /// is not a receipt is a `/model` wait outlived by the turn it was typed into: Claude Code writes nothing for a
    /// change made while it works, so the turn's end settles it, neutrally.
    case waiting(request: String, quietly: Bool)
    case confirmed(String)
    /// Terminal, and neither success nor failure: the request was answered, and the answer was not a change. Claude
    /// Code kept the value, or the daemon replayed a settled mutation that carries no outcome, or a turn ended with
    /// a `/model` typed into it unanswered. A green tick over any of them would be the exact lie this app exists not
    /// to tell; a warning would invent a fault that is not there. The sentence carries which it was.
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

/// A Claude Code command one of the native sheets types for the reader.
public enum NativeCommand: Sendable, Hashable {
    case model(String)
    case effort(String)
    case compact(instructions: String)

    /// What a phase carries for this command: the model, the level, or the instructions.
    public var request: String {
        switch self {
        case .model(let value), .effort(let value), .compact(instructions: let value): return value
        }
    }

    /// How long the sheet's caption promises an answer before it goes quiet. The observation window ends; the
    /// observation does not. A real compaction measured about 6 s on a small context and grows with the context.
    public var quietAfter: Duration {
        switch self {
        case .model, .effort: return .seconds(5)
        case .compact: return .seconds(10)
        }
    }

    /// One sentence, three sheets. A duplicate response proves the mutation was typed once and settled; it does not
    /// carry what Claude Code then did, so this claims neither a change nor the absence of one.
    public static let alreadySentLine = "This request was sent earlier; this retry does not confirm its outcome."

    /// A `/model` the turn outlived. Measured on 2.1.308, 2.1.316 and 2.1.322: typed while Claude Code works, it is
    /// applied at once and shown on Claude Code's own screen, and nothing is written to the transcript, then or
    /// later. So this says what CodeConnect can read, not what Claude Code did, and does not claim the change took.
    static func modelUnconfirmedLine(_ alias: String) -> String {
        "CodeConnect can't confirm /model \(alias): Claude Code writes no result to the conversation for a change "
            + "sent while it's working. The model shown may be out of date."
    }

    /// The phase a send leads to at once, or nil when the keystrokes landed and only Claude Code's own receipt can
    /// settle it: landing proves typing, not execution. A Compact whose composer came back returns a waiting phase
    /// without reading a receipt. Any waiting phase that is not quiet starts the `quietAfter` window.
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
        case .composerRecovered:
            let computer = HostDevice.current.computer("the computer")
            switch self {
            case .model(let alias):
                // The command took the composer and the daemon's generic
                // slash-command rescue pressed Esc to give it back. Measured on
                // 2.1.223: when the conversation is already cached for the
                // current model, `/model <alias>` opens a confirmation, and that
                // Esc answers it in the negative — Claude Code then prints
                // `Kept model as …`. So this is never a change. Only what was
                // observed is claimed: the composer went away and one Esc
                // brought it back; the daemon does not identify views.
                return .neutral(
                    "/model \(alias) took the composer on \(computer) and CodeConnect "
                        + "pressed Esc to give it back. No model change was confirmed.")
            case .effort(let value):
                // As for /model: on 2.1.223 `/effort <new value>` opens a
                // confirmation whenever the conversation is already cached for
                // the current level, and the rescue answers it with Esc; Claude
                // Code then prints `Kept effort level as …`.
                return .neutral(
                    "/effort \(value) took the composer on \(computer) and CodeConnect "
                        + "pressed Esc to give it back. No effort change was confirmed.")
            case .compact:
                // The composer came back and the compaction may still complete:
                // keep watching for its line.
                return .waiting(request: request, quietly: false)
            }
        case .composerLost:
            return .failed("Couldn’t restore the composer. Open Terminal to recover.")
        }
    }

    /// Claude Code's own answer to this command above `baseline`, as a terminal phase, or nil while there is none.
    /// The sequence fences out a backfill that publishes an older receipt; for `/model`, `ModelChangeWatch` also
    /// requires a change or a not-found line to name the value that was asked for, and a turn that ends above
    /// `baseline` with no such answer settles the wait in `modelUnconfirmedLine`'s words.
    func receiptPhase(from receipts: SessionState.Receipts?, after baseline: UInt64) -> CommandPhase? {
        switch self {
        case .model(let alias):
            guard
                let outcome = ModelChangeWatch.outcome(
                    after: baseline, requested: alias, signal: receipts?.modelSignal.command)
            else {
                guard let turnEnd = receipts?.modelSignal.turnEndSeq, turnEnd > baseline else { return nil }
                return .neutral(Self.modelUnconfirmedLine(alias))
            }
            switch outcome {
            case .receipt(let receipt):
                return receipt.isChange ? .confirmed(receipt.sheetStatus) : .neutral(receipt.sheetStatus)
            // Verbatim: Claude Code already said the useful thing, and the value
            // it is quoting is the one the reader typed.
            case .notFound(let line):
                return .failed(line)
            }
        case .effort:
            guard let fact = receipts?.effort, let seq = receipts?.effortSeq, seq > baseline else { return nil }
            return fact.isChange ? .confirmed(fact.sheetStatus) : .neutral(fact.sheetStatus)
        case .compact:
            guard let signal = receipts?.compact, let seq = receipts?.compactSeq, seq > baseline else { return nil }
            switch signal {
            case .compacted: return .confirmed("Claude Code confirmed the compaction.")
            case .notEnoughMessages: return .failed("Not enough messages to compact.")
            }
        }
    }
}

extension NativeCommand {
    /// What a Model control states above its rows. The daemon completes Claude Code's confirmation for the tap, so the
    /// tap is the consent, and both consequences are said before it.
    public static let modelConsent =
        "A confirmed change also becomes your default for new sessions. Switching mid-conversation makes Claude Code "
        + "re-read the whole conversation on your next message."

    /// What an Effort control states above its rows, for the same reason. The scope is not claimed: it differs per
    /// level, and Claude Code's receipt states it afterwards.
    public static let effortConsent =
        "Switching mid-conversation makes Claude Code re-read the whole conversation on your next message."

    /// The five levels an Effort control offers, the value typed and the label shown, in order. `ultracode` and `auto`
    /// stay typing-only.
    public static let effortLevels: [(value: String, label: String)] = ["low", "medium", "high", "xhigh", "max"].map {
        ($0, EffortConfirmation.label(for: $0))
    }
}

extension AppModel {
    /// Claude Code's own answer to `command` in this run since `baseline` (`eventHighWater(for:)`, taken just
    /// before the send), or for `/model` the end of the turn it was typed into, as a terminal phase, or nil while
    /// there is neither.
    public func receiptPhase(for command: NativeCommand, in key: String, after baseline: UInt64) -> CommandPhase? {
        command.receiptPhase(from: receipts(for: key), after: baseline)
    }

    /// What a command's status says in `phase`, naming the computer in the words of the device reading it; nil at
    /// rest. Typing and waiting are neutral; a settled phase is its own words, a change confirmed, a kept value
    /// neutral, a failure a warning.
    public func commandStatus(_ command: NativeCommand, _ phase: CommandPhase, on host: HostDevice = .current)
        -> ComposeProse.Note?
    {
        let computer = host.computer("the computer", named: machineName)
        let typed: String
        switch command {
        case .model(let alias): typed = "/model \(alias)"
        case .effort(let value): typed = "/effort \(value)"
        case .compact: typed = "/compact"
        }
        switch phase {
        case .idle:
            return nil
        case .sending:
            return ComposeProse.Note(text: "Typing \(typed) on \(computer)…", tone: .neutral, icon: nil)
        case .waiting(_, quietly: false):
            return ComposeProse.Note(text: "Waiting for Claude Code to confirm…", tone: .neutral, icon: nil)
        case .waiting(_, quietly: true):
            if case .compact = command {
                return ComposeProse.Note(
                    text: "Compaction requested on \(computer). CodeConnect has not received a completion signal.",
                    tone: .neutral, icon: nil)
            }
            return ComposeProse.Note(
                text: "Sent \(typed) to \(computer). Still waiting for Claude Code to confirm.", tone: .neutral,
                icon: nil)
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
    public static let title = "Clear Claude’s context?"
    public static let clear = "Clear context"
    public static let keep = "Keep context"
    public static let message =
        "Claude Code will forget this conversation’s context. CodeConnect’s timeline will stay here."
}

/// The words of the control `/compact` opens.
public enum CompactPrompt {
    public static let title = "Compact context"
    public static let explanation = "Ask Claude Code to compact its current context. Instructions are optional."
    public static let fieldLabel = "Instructions (optional)"
    public static let placeholder = "What should Claude preserve?"
    public static let action = "Compact now"
}
