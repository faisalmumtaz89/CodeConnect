import CodeConnectKit
import SwiftUI

/// Claude's question where it was asked: each question with its choices, picked here and sent as Claude reads them,
/// or declined. Kit decides whether this Mac may answer (`QuestionCardStatus`), what the picks mean
/// (`QuestionDraft`), and that at HIGH submitting asks Touch ID or the password first while declining never does.
struct QuestionBlock: View {
    let approval: ApprovalItem
    let questions: QuestionCard
    let model: AppModel
    @State private var draft: QuestionDraft
    @State private var sending: String?
    @State private var authNotice: String?

    init(approval: ApprovalItem, questions: QuestionCard, model: AppModel) {
        self.approval = approval
        self.questions = questions
        self.model = model
        _draft = State(initialValue: QuestionDraft(card: questions))
    }

    private var live: ApprovalItem? { model.shownApproval(sessionKey: approval.sessionKey, id: approval.id) }
    private var attempt: AnswerAttempt? { model.lastAttempt(for: approval) }
    private var inFlight: Bool { sending != nil || model.isAnswering(approval) }
    private var risk: RiskClass { approval.assessment(profile: model.daemonProfile).effective }
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

    var body: some View {
        HStack(spacing: 0) {
            Rectangle().fill(status.isAnswerable ? Theme.amber : Theme.text3).frame(width: 2)
            VStack(alignment: .leading, spacing: 14) {
                Text(questions.questions.count == 1 ? "Claude asks" : "Claude asks \(questions.questions.count) questions")
                    .font(Theme.ui(12.5, .medium))
                    .foregroundStyle(status.isAnswerable ? Theme.amber : Theme.text2)
                ForEach(Array(questions.questions.enumerated()), id: \.offset) { index, question in
                    questionView(question, at: index)
                }
                if let banner = status.banner {
                    BannerLine(title: banner.title, message: banner.message, tone: banner.tone)
                        .accessibilityIdentifier("\(id).banner")
                } else if let attempt, !attempt.isTerminal {
                    BannerLine(DecisionCard.resultBanner(for: attempt)).accessibilityIdentifier("\(id).banner")
                }
                if let authNotice {
                    BannerLine(title: HostDevice.current.biometric, message: authNotice, tone: .warning)
                }
                if status.isAnswerable { controls }
            }
            .padding(12)
        }
        .background(Theme.raised.opacity(0.45))
        .clipShape(RoundedRectangle(cornerRadius: 8))
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(id)
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
                    field("Your answer", text: $draft.entries[index].otherText, id: "\(id).q\(index).other.text")
                }
            }
            if question.takesNotes {
                field("Notes for Claude (optional)", text: $draft.entries[index].notes, id: "\(id).q\(index).notes")
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

    private var controls: some View {
        let answers = draft.answers(in: questions)
        let submitTitle =
            DecisionCard.needsSecondFactor(risk: risk, decision: .answers([]))
            ? "Submit with \(HostDevice.current.biometric)" : "Submit"
        return VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Spacer()
                ActionButton(title: "Decline", key: nil, prominent: false, id: "\(id).decline") {
                    send(.decline, from: "decline")
                }
                .disabled(blocked != nil)
                ActionButton(title: submitTitle, key: nil, prominent: true, id: "\(id).submit") {
                    if let answers { send(.answers(answers), from: "submit") }
                }
                .disabled(blocked != nil || answers == nil)
                .opacity(blocked != nil || answers == nil ? 0.45 : 1)
            }
            if let note = blocked ?? (answers == nil ? incompleteNote : nil) {
                Text(note).font(Theme.ui(11.5)).foregroundStyle(Theme.text3).accessibilityIdentifier("\(id).blocked")
            }
        }
    }

    /// What Submit waits for while the answers are incomplete: not a warning, an untouched form is not a fault.
    private var incompleteNote: String {
        questions.questions.count == 1 ? "Choose an answer to submit." : "Answer every question to submit."
    }

    /// One answer, as the decision card sends one: never while the card cannot be answered or one is on its way,
    /// and at HIGH only after the second factor says yes.
    private func send(_ decision: AnswerDecision, from control: String) {
        guard status.isAnswerable, !inFlight, blocked == nil else { return }
        sending = control
        authNotice = nil
        Task {
            if DecisionCard.needsSecondFactor(risk: risk, decision: decision) {
                let check = await BiometricGate.confirm(reason: BiometricGate.reason(for: approval.card.toolName))
                guard check.isAuthenticated else {
                    sending = nil
                    authNotice = check.message
                    return
                }
            }
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
