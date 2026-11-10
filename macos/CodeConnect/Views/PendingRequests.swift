import CodeConnectKit
import SwiftUI

/// What in this run waits on you, at the foot of the pane: every request still pending, the oldest first, outside the
/// conversation's scroll, so the conversation can follow the run and keep a reader in place without ever moving one.
/// Each request is read in this area's own scroll, the space its read gate measures; a request taller than the area
/// scrolls inside it. Once answered or resolved, a request is a row of the conversation again.
struct PendingRequests: View {
    let model: AppModel
    let key: String
    /// The most of the pane the area takes.
    let maxHeight: CGFloat
    @State private var height: CGFloat = 0

    var body: some View {
        let pending = model.shown(for: key)?.pendingApprovals ?? []
        if !pending.isEmpty {
            Rectangle().fill(Theme.hairline).frame(height: 1)
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    ForEach(pending) { RequestBlock(approval: $0, model: model) }
                }
                .padding(.horizontal, 28)
                .padding(.vertical, 14)
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height = $0 }
            }
            .scrollBounceBehavior(.basedOnSize)
            .frame(height: min(height, maxHeight))
            .accessibilityIdentifier("session.pending")
        }
    }
}

/// A request as the agent asked it: a Claude question as its questions, anything else as a decision.
struct RequestBlock: View {
    let approval: ApprovalItem
    let model: AppModel

    var body: some View {
        if let questions = DecisionCard.questionCard(
            card: approval.card, agent: model.shownSummary(for: approval.sessionKey)?.agent)
        {
            QuestionBlock(approval: approval, questions: questions, model: model)
        } else {
            DecisionBlock(approval: approval, model: model)
        }
    }
}
