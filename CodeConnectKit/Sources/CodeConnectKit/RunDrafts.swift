import Foundation
import Observation

/// **What the reader has typed on one run and not sent**: the reply and the send whose answer never came, and for each
/// request still waiting, by `ApprovalItem.id`, the reason a Deny would carry and a question's answers.
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
    /// The reason a Deny carries, by request.
    public var reasons: [String: String] = [:]
    /// A question's picks and words, by request.
    public var answers: [String: QuestionDraft] = [:]

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
