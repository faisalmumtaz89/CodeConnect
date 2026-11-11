import CodeConnectKit
import SwiftUI

/// Claude's question where it was asked: each question with its choices, picked here and sent as Claude reads them,
/// or declined. Kit decides whether this Mac may answer (`QuestionCardStatus`), what the picks mean
/// (`QuestionDraft`). At the pane's foot the questions are read in their own scroll and Decline and Submit take the
/// reply box's place, with their keys (⌘D, ⌘↩); in the conversation it is what was asked and what became of it.
struct QuestionBlock: View {
    let approval: ApprovalItem
    let questions: QuestionCard
    let model: AppModel
    @State private var sending: String?
    /// How tall the questions are, for their own scroll at the foot.
    @State private var askedHeight: CGFloat = 0
    /// How tall Decline and Submit are with their box: the questions scroll in what they leave of the foot.
    @State private var answersHeight: CGFloat = 0
    @Environment(\.pendingSlot) private var slot

    /// The picks and words so far: the run's (`RunDrafts`), so they outlive this view.
    private var draft: QuestionDraft {
        get { model.drafts(for: approval.sessionKey)?.answers[approval.id] ?? QuestionDraft(card: questions) }
        nonmutating set { model.drafts(for: approval.sessionKey)?.answers[approval.id] = newValue }
    }

    private var live: ApprovalItem? { model.shownApproval(sessionKey: approval.sessionKey, id: approval.id) }
    private var attempt: AnswerAttempt? { model.lastAttempt(for: approval) }
    private var inFlight: Bool { sending != nil || model.isAnswering(approval) }
    private var blocked: String? { model.answerBlockedReason(for: approval, inFlight: inFlight) }
    private var id: String { "question.\(approval.card.requestID)" }

    private var status: QuestionCardStatus {
        QuestionCardStatus.resolve(
            outcome: DecisionCard.effectiveOutcome(live: live?.outcome, snapshot: approval.outcome),
            attempt: attempt, isBacked: live != nil, hold: (live ?? approval).questionHold,
            answersQuestions: model.daemonProfile.knownAnswersQuestions, linkReason: model.actionsBlockedReason)
    }

    /// Picks may change only while the card can be answered and no answer is on its way.
    private var editable: Bool { status.isAnswerable && !inFlight }

    /// The reply field holds the keyboard: the box stays the writer's, and Decline and Submit wait above it, keyless.
    private var held: Bool { slot?.held == true }

    var body: some View {
        Group {
            if let slot {
                VStack(spacing: 0) {
                    ScrollView {
                        asked
                            .padding(.horizontal, 28)
                            .padding(.vertical, 14)
                            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { askedHeight = $0 }
                    }
                    .scrollBounceBehavior(.basedOnSize)
                    .frame(height: min(askedHeight, max(0, slot.maxHeight - (status.isAnswerable ? answersHeight : 0))))
                    .accessibilityIdentifier("session.pending")
                    if status.isAnswerable {
                        Group {
                            if held {
                                controls(keys: false).padding(.horizontal, 28).padding(.bottom, 14)
                            } else {
                                // Its keys answer while no sheet covers the window.
                                ReplyBoxFrame { controls(keys: !slot.sheetUp) }
                                    .preference(key: HoldsReplyBox.self, value: true)
                            }
                        }
                        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { answersHeight = $0 }
                    }
                }
            } else {
                asked
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(id)
    }

    /// What Claude asks: each question with its choices while it can be answered here; once it can't, what was asked
    /// and, when known, what was answered. Then what became of it.
    private var asked: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 8) {
                if status.isAnswerable { StateIcon(status: .blocked) }
                Text(questions.questions.count == 1 ? "Claude asks" : "Claude asks \(questions.questions.count) questions")
                    .font(Theme.ui(12.5, .medium))
                    .foregroundStyle(status.isAnswerable ? Theme.text : Theme.text2)
                Spacer(minLength: 8)
                MoreWaiting(count: slot?.more ?? 0)
            }
            if status.isAnswerable {
                ForEach(Array(questions.questions.enumerated()), id: \.offset) { index, question in
                    questionView(question, at: index)
                }
            } else {
                ForEach(Array(questions.questions.enumerated()), id: \.offset) { _, question in
                    Text(question.text).font(Theme.ui(13)).foregroundStyle(Theme.text2)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if let summary = status.summary(of: questions) {
                    Text(summary.displaySafe).font(Theme.ui(12.5)).foregroundStyle(Theme.text)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("\(id).summary")
                }
            }
            if let banner = status.banner {
                BannerLine(title: banner.title, message: banner.message, tone: banner.tone)
                    .accessibilityIdentifier("\(id).banner")
            } else if let attempt, !attempt.isTerminal {
                BannerLine(DecisionCard.resultBanner(for: attempt)).accessibilityIdentifier("\(id).banner")
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func questionView(_ question: QuestionCard.Question, at index: Int) -> some View {
        let entry = draft.entries[index]
        return VStack(alignment: .leading, spacing: 6) {
            if let header = question.header {
                Text(header.uppercased()).font(Theme.ui(10.5, .semibold)).foregroundStyle(Theme.text3)
            }
            Text(question.text).font(Theme.ui(13)).foregroundStyle(Theme.text).fixedSize(horizontal: false, vertical: true)
            ForEach(Array(question.options.enumerated()), id: \.offset) { option, choice in
                let order = entry.selected.firstIndex(of: option)
                Button { draft.tap(option: option, of: index, in: questions) } label: {
                    ChoiceLabel(
                        chosen: order != nil, multi: question.multiSelect, label: choice.label,
                        detail: choice.description, order: question.multiSelect ? order.map { $0 + 1 } : nil)
                }
                .buttonStyle(.hover(id: "\(id).q\(index).option.\(option)"))
                .disabled(!editable)
                .accessibilityIdentifier("\(id).q\(index).option.\(option)")
                .accessibilityAddTraits(order != nil ? .isSelected : [])
                if order != nil, let preview = choice.preview {
                    Text(preview).font(Theme.mono(11.5)).foregroundStyle(Theme.text2).textSelection(.enabled)
                        .padding(10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(Theme.code, in: RoundedRectangle(cornerRadius: 6))
                }
            }
            if question.takesOther {
                Button { draft.tapOther(of: index, in: questions) } label: {
                    ChoiceLabel(chosen: entry.otherChosen, multi: question.multiSelect, label: "Other", detail: nil, order: nil)
                }
                .buttonStyle(.hover(id: "\(id).q\(index).other"))
                .disabled(!editable)
                .accessibilityIdentifier("\(id).q\(index).other")
                .accessibilityAddTraits(entry.otherChosen ? .isSelected : [])
                if entry.otherChosen {
                    field(
                        "Your answer",
                        text: Binding { draft.entries[index].otherText } set: { draft.entries[index].otherText = $0 },
                        id: "\(id).q\(index).other.text")
                }
            }
            if question.takesNotes {
                field(
                    "Notes for Claude (optional)",
                    text: Binding { draft.entries[index].notes } set: { draft.entries[index].notes = $0 },
                    id: "\(id).q\(index).notes")
            }
        }
    }

    private func field(_ prompt: String, text: Binding<String>, id: String) -> some View {
        TextField(prompt, text: text)
            .textFieldStyle(.plain)
            .font(Theme.ui(12.5))
            .padding(.horizontal, 8)
            .frame(height: 26)
            .background(Theme.code, in: RoundedRectangle(cornerRadius: 6))
            .disabled(!editable)
            .accessibilityLabel(prompt)
            .accessibilityIdentifier(id)
    }

    /// Decline and Submit, with what Submit waits for beside them; `keys`: in the box, where ⌘D declines and ⌘↩
    /// submits.
    private func controls(keys: Bool) -> some View {
        let answers = draft.answers(in: questions)
        return HStack(spacing: 8) {
            if let note = blocked ?? (answers == nil ? incompleteNote : nil) {
                Text(note).font(Theme.ui(11.5)).foregroundStyle(Theme.text3).accessibilityIdentifier("\(id).blocked")
            }
            Spacer(minLength: 8)
            ActionButton(title: "Decline", key: keys ? "⌘D" : nil, prominent: false, id: "\(id).decline") {
                send(.decline, from: "decline")
            }
            .disabled(blocked != nil)
            .answerKey("d", .command, when: keys)
            ActionButton(title: "Submit", key: keys ? "⌘↩" : nil, prominent: true, id: "\(id).submit") {
                if let answers { send(.answers(answers), from: "submit") }
            }
            .disabled(blocked != nil || answers == nil)
            .opacity(blocked != nil || answers == nil ? 0.45 : 1)
            .answerKey(.return, .command, when: keys)
        }
    }

    /// What Submit waits for while the answers are incomplete: not a warning, an untouched form is not a fault.
    private var incompleteNote: String {
        questions.questions.count == 1 ? "Choose an answer to submit." : "Answer every question to submit."
    }

    /// One answer, as the decision card sends one: never while the card cannot be answered or one is on its way.
    private func send(_ decision: AnswerDecision, from control: String) {
        guard status.isAnswerable, !inFlight, blocked == nil else { return }
        sending = control
        Task {
            _ = await model.answer(item: approval, decision: decision)
            sending = nil
        }
    }
}

/// One choice: its mark (a ring or a box, filled when chosen), its label and description, and on a question that
/// takes several, the order it was picked in, which is the order it is sent.
private struct ChoiceLabel: View {
    let chosen: Bool
    let multi: Bool
    let label: String
    let detail: String?
    let order: Int?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: multi ? (chosen ? "checkmark.square.fill" : "square") : (chosen ? "largecircle.fill.circle" : "circle"))
                .font(.system(size: 12))
                .foregroundStyle(chosen ? Theme.text : Theme.text3)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(label).font(Theme.ui(12.5)).foregroundStyle(Theme.text)
                if let detail { Text(detail).font(Theme.ui(11.5)).foregroundStyle(Theme.text3) }
            }
            Spacer(minLength: 0)
            if let order { Text(verbatim: "\(order)").font(Theme.mono(11)).foregroundStyle(Theme.text3) }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
    }
}
