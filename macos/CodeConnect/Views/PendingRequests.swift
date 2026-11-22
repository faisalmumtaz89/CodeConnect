import CodeConnectKit
import SwiftUI

/// What in this run waits on you, at the foot of the pane where you write to it: the oldest request still pending.
/// What it asks is read in its own scroll above (`session.pending`, the space its read gate measures), in what its
/// answers leave of `maxHeight`, so at every window size the answers stay inside the window; the answers take the reply
/// box's place, which stands aside mounted (`HoldsReplyBox`). The requests behind it wait their turn, counted on its
/// title line, and the next takes the box when it is answered. While the reply field holds the keyboard (`held`), the
/// box stays the writer's: the request waits above it with its answers, until the field lets go. Once answered or
/// resolved, a request is a row of the conversation again.
struct PendingRequests: View {
    let model: AppModel
    let key: String
    /// The most of the pane a request takes, its answers included.
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

/// A request in the pane's foot rather than a row of the conversation: the most height it may take, answers included,
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

/// The reply box's own surface, edge and margins, holding a request's answers in its place, and at its trailing edge,
/// apart from the answers, the Stop the reply box has while the run's turn runs (`StopTurnButton`).
struct ReplyBoxFrame<Content: View>: View {
    let model: AppModel
    let key: String
    @ViewBuilder let content: Content

    var body: some View {
        HStack(alignment: .bottom, spacing: 12) {
            VStack(alignment: .leading, spacing: 10) { content }
            StopTurnButton(model: model, key: key)
        }
            .padding(12)
            .background(Theme.raised, in: RoundedRectangle(cornerRadius: 10))
            .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color.white.opacity(0.09)))
            // On the pane's one content column, as the reply box it stands in for: the card's text stands 28 pt in.
            .padding(.horizontal, 28)
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

/// A request as the agent asked it: a question (Claude's or Codex's) as its questions, anything else as a decision.
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

/// Stops the turn the run is running, a Codex run's over Codex's own link and a Claude run's through the host it was
/// started on: one white button with the stop square, where the send button would be. It shows whenever Kit says the
/// turn can be stopped, in the reply box in Send's place and in the foot of a card that has taken the box's place,
/// apart from the card's answers. Greyed while the stop is on its way, and for a moment after a turn changes, as Kit
/// rules; the note above the box says what became of it.
struct StopTurnButton: View {
    let model: AppModel
    let key: String
    static let size: CGFloat = 24

    var body: some View {
        if model.stopUnavailable(for: key) == nil {
            let stopping = model.codexStop(for: key) == .inFlight
            let greyed = model.codexControls(for: key).isStopGreyed(now: model.now)
            Button {
                Task { await model.stopCodexTurn(sessionKey: key) }
            } label: {
                RoundedRectangle(cornerRadius: 2)
                    .fill(Color.black)
                    .frame(width: 8, height: 8)
                    .frame(width: Self.size, height: Self.size)
                    .background(Circle().fill(Theme.text))
            }
            .buttonStyle(.hover(radius: 12, over: true, id: "session.stop"))
            .disabled(stopping || greyed)
            .help(stopping ? SessionWords.stopping : greyed ? SessionWords.tryAgainInAMoment : SessionWords.stopHelp)
            .accessibilityLabel("Stop")
            .accessibilityIdentifier("session.stop")
        }
    }
}
