import CodeConnectKit
import SwiftUI

/// What in this run waits on you, at the foot of the pane where you write to it: the oldest request still pending.
/// What it asks is read in its own scroll above (`session.pending`), in at most `maxHeight` (the space its read gate
/// measures), and its answers take the reply box's place, which stands aside mounted (`HoldsReplyBox`). The requests
/// behind it wait their turn, counted on its title line, and the next takes the box when it is answered. While the
/// reply field holds the keyboard (`held`), the box stays the writer's: the request waits above it with its answers,
/// until the field lets go. Once answered or resolved, a request is a row of the conversation again.
struct PendingRequests: View {
    let model: AppModel
    let key: String
    /// The most of the pane what is asked takes.
    let maxHeight: CGFloat
    /// The reply field holds the keyboard.
    let held: Bool
    /// A sheet covers the window.
    let sheetUp: Bool

    var body: some View {
        let pending = model.shown(for: key)?.pendingApprovals ?? []
        if let first = pending.first {
            Rectangle().fill(Theme.hairline).frame(height: 1)
            RequestBlock(approval: first, model: model)
                // A request of its own: the next one starts with its own read gate and an empty reason.
                .id(first.id)
                .environment(
                    \.pendingSlot,
                    PendingSlot(maxHeight: maxHeight, held: held, sheetUp: sheetUp, more: pending.count - 1))
        }
    }
}

/// A request in the pane's foot rather than a row of the conversation: the most height what it asks may take,
/// whether the reply field holds the keyboard, whether a sheet covers the window (its answers then take no keys), and
/// how many requests wait behind it.
struct PendingSlot: Equatable {
    let maxHeight: CGFloat
    let held: Bool
    let sheetUp: Bool
    let more: Int
}

extension EnvironmentValues {
    @Entry var pendingSlot: PendingSlot?
}

/// A request holds the reply box's place: the reply box stands aside, mounted, while one does.
struct HoldsReplyBox: PreferenceKey {
    static let defaultValue = false
    static func reduce(value: inout Bool, nextValue: () -> Bool) { value = value || nextValue() }
}

/// The reply box's own surface, edge and margins, holding a request's answers in its place.
struct ReplyBoxFrame<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 10) { content }
            .padding(12)
            .background(Theme.raised, in: RoundedRectangle(cornerRadius: 10))
            .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color.white.opacity(0.09)))
            .padding(.horizontal, 20)
            .padding(.top, 4)
            .padding(.bottom, 16)
    }
}

/// How many requests wait behind the one in the box, in its words; nothing when none do.
struct MoreWaiting: View {
    let count: Int

    var body: some View {
        if count > 0 {
            Text("\(count) more waiting")
                .font(Theme.ui(11.5))
                .foregroundStyle(Theme.text3)
                .accessibilityIdentifier("session.pending.more")
        }
    }
}

extension View {
    /// The key an answer answers to, only while the box holds its request.
    @ViewBuilder func answerKey(_ key: KeyEquivalent, _ modifiers: EventModifiers, when on: Bool) -> some View {
        if on { keyboardShortcut(key, modifiers: modifiers) } else { self }
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
