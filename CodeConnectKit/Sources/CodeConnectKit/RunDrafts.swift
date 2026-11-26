import Foundation
import Observation

/// **What the reader has typed on one run and not sent**: the reply and the send whose answer never came, a send on its
/// way and what became of the last one, the command typed last and where it stands, and for each request still waiting, by `ApprovalItem.id`, the reason a Deny
/// would carry and a question's answers.
///
/// Held by the run's `SessionState`, not by the views that show it, so the words outlive the pane being closed,
/// another run being opened and a trip to another tab, for as long as the app is open and the run is listed. Never
/// written to disk. What was typed for a request goes once the screen no longer shows it waiting.
@MainActor
@Observable
public final class RunDrafts {
    public var reply = ""
    /// The send whose answer never came, asked again once the link is back if Kit's rule (`AppModel.asksAgain`)
    /// still allows it.
    public var unanswered: UnansweredSend?
    /// A send of the reply, or a command typed in it, is on its way: whichever box shows the run, mounted again since
    /// or not, waits for it.
    public var sending = false
    /// What became of the last send or refused command, worded by Kit (`AppModel.outcomeNote`): the run's, so a box
    /// mounted again shows the outcome of a send an earlier one made.
    public var lastSend: ComposeAttempt?
    /// The command typed last (a compact) and where it stands, watched until its receipt: the run's, so a box mounted
    /// again still waits for it and shows its verdict.
    public var command: CommandWatch?
    /// The reason a Deny carries, by request.
    public var reasons: [String: String] = [:]
    /// A question's picks and words, by request.
    public var answers: [String: QuestionDraft] = [:]
    /// The files going with the next message, in order (`AppModel.attach`).
    public internal(set) var files: [Attachment] = []

    /// Drops what was typed for requests no longer waiting. Each dictionary is assigned only when something goes:
    /// `@Observable` fires on the assignment.
    func keep(waiting: Set<String>) {
        if reasons.keys.contains(where: { !waiting.contains($0) }) {
            reasons = reasons.filter { waiting.contains($0.key) }
        }
        if answers.keys.contains(where: { !waiting.contains($0) }) {
            answers = answers.filter { waiting.contains($0.key) }
        }
    }
}

/// A command typed in a run's reply box (`NativeCommand`), where it stands, and how far the run's events had got when it
/// was sent: only a receipt above that answers it.
public struct CommandWatch: Equatable, Sendable {
    public let command: NativeCommand
    public var phase: CommandPhase
    public let baseline: UInt64

    public init(command: NativeCommand, phase: CommandPhase, baseline: UInt64) {
        self.command = command
        self.phase = phase
        self.baseline = baseline
    }
}
