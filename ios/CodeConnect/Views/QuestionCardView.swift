import SwiftUI

/// Claude's `AskUserQuestion`, answered as a question rather than approved.
///
/// The decision card's home, in both places it lives (the sheet and the Deck):
/// `DecisionCardView` hands a question to this view and keeps everything else.
/// One step per question — header chip, question, options — then a Review step
/// listing every answer, as the terminal's "Review your answers" does; with one
/// question there is no Review step and Submit sits on the question.
///
/// **There is no Allow.** The answer surface is Submit (offered only when every
/// question has an answer), Decline (as Escape at the Mac) and Reply instead (the
/// reader's words reach Claude in place of an answer). Every state that is not
/// answerable — the question is on the Mac, the phone's hold ended, the Mac is
/// too old, it was answered — shows the questions read-only and says why.
struct QuestionCardView: View {
    let approval: ApprovalItem
    let questions: QuestionCard
    var onSettled: (() -> Void)?
    var comeBackToThis: (() -> Void)?

    @Environment(AppModel.self) private var model
    @Environment(\.dynamicTypeSize) private var typeSize

    @State private var draft: QuestionDraft
    /// Which question is on screen; `questions.count` is the Review step.
    @State private var step = 0
    @State private var replying = false
    @State private var reply = ""
    /// Which control is spinning. Whether an answer is really in flight lives
    /// on the model, keyed by request id.
    @State private var spinningControl: String?
    /// Why the biometric check did not pass. Shown, never swallowed.
    @State private var authNotice: String?
    /// What a tap in the sample fleet would have done.
    @State private var sampleNotice: String?

    init(
        approval: ApprovalItem, questions: QuestionCard, onSettled: (() -> Void)? = nil,
        comeBackToThis: (() -> Void)? = nil
    ) {
        self.approval = approval
        self.questions = questions
        self.onSettled = onSettled
        self.comeBackToThis = comeBackToThis
        _draft = State(initialValue: QuestionDraft(card: questions))
    }

    // MARK: State

    private var live: ApprovalItem? {
        model.liveApproval(sessionKey: approval.sessionKey, id: approval.id)
    }

    private var attempt: AnswerAttempt? { model.lastAttempt(for: approval) }

    private var status: QuestionCardStatus {
        QuestionCardStatus.resolve(
            outcome: DecisionCardView.effectiveOutcome(
                live: live?.outcome, snapshot: approval.outcome),
            attempt: attempt, isBacked: live != nil,
            hold: (live ?? approval).questionHold,
            answersQuestions: model.daemonProfile.answersQuestions)
    }

    private var count: Int { questions.questions.count }
    private var isReview: Bool { count > 1 && step == count }

    private var inFlight: Bool { spinningControl != nil || model.isAnswering(approval) }

    private var risk: RiskClass { approval.assessment(profile: model.daemonProfile).effective }

    /// Everything that stops every control on this card, in the order that
    /// matters — the decision card's own list.
    private var blockedReason: String? {
        if inFlight { return "Waiting for the daemon to confirm…" }
        if let reason = model.actionsBlockedReason { return reason }
        if let summary = model.summary(for: approval.sessionKey) {
            let badge = FleetStatusRule.capability(
                summary: summary, capabilities: model.connection.capabilities)
            if let reason = badge.reason { return reason }
        }
        return nil
    }

    private var submitBlockedReason: String? {
        if let blockedReason { return blockedReason }
        return draft.answers(in: questions) == nil ? "Answer every question to submit." : nil
    }

    // MARK: Body

    var body: some View {
        ScrollViewReader { reader in
            ScrollView {
                VStack(alignment: .leading, spacing: CC.rhythm.sections) {
                    header.id(Self.top)
                    // Why a card cannot be answered is the first thing it says,
                    // in the document beside the questions it is about — not
                    // pinned over them.
                    if let banner = status.banner {
                        CCBanner(
                            banner.title, message: banner.message, tone: banner.tone.ccTone,
                            icon: banner.icon)
                    }
                    switch status {
                    case .answerable:
                        if isReview { reviewList } else { questionStep(step) }
                        // At accessibility sizes the subordinate controls leave the
                        // pinned bar, as on the decision card: stacked there they
                        // own the screen and leave the question unreadable.
                        if typeSize.isAccessibilitySize { subordinateControls }
                    case .answeredOnPhone(let answers):
                        answeredList(answers)
                    default:
                        ForEach(questions.questions.indices, id: \.self) { readOnlyQuestion($0) }
                    }
                    Color.clear.frame(height: CC.space.xs)
                }
                .padding(.horizontal, CC.space.md)
                .padding(.top, CC.space.md)
            }
            // A new step starts at its own top: the scroll position of the last
            // one would land the reader halfway down a question they have not
            // read.
            .onChange(of: step) { reader.scrollTo(Self.top, anchor: .top) }
        }
        .scrollIndicators(.hidden)
        .background(CC.color.bg)
        .safeAreaInset(edge: .bottom, spacing: 0) { pinnedFooter }
    }

    private static let top = "question-card-top"

    // MARK: Header

    private var header: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            Text(count == 1 ? "Claude has a question" : "Claude has \(count) questions")
                .ccType(CC.type.title)
                .foregroundStyle(CC.text.primary)
                .fixedSize(horizontal: false, vertical: true)
            Text(verbatim: model.runLabel(for: approval.sessionKey).project)
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.secondary)
                .lineLimit(2)
        }
    }

    // MARK: A question

    private func questionStep(_ index: Int) -> some View {
        let question = questions.questions[index]
        let entry = draft.entries[index]
        return VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            questionHeading(index)
            if question.multiSelect {
                Text("Choose any. They are sent in the order you pick them.")
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.tertiary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    ForEach(question.options.indices, id: \.self) { option in
                        optionRow(option, of: index)
                        if option == focusedPreview(index), let preview = question.options[option].preview {
                            CCMonoBlock(preview, showsCopy: false)
                                .padding(.horizontal, CC.space.md)
                                .padding(.bottom, CC.space.sm)
                                .accessibilityIdentifier("question-\(index)-preview")
                        }
                        CCHairline()
                    }
                    if question.takesOther {
                        otherRow(index)
                    }
                }
            }
            if entry.otherChosen {
                CCField(
                    label: "Other", text: otherBinding(index), placeholder: "Type your answer")
                .accessibilityIdentifier("question-\(index)-other-text")
            }
            if question.takesNotes {
                CCField(
                    label: "Notes", text: notesBinding(index),
                    placeholder: "Optional notes for Claude on this choice", axis: .vertical,
                    lineLimit: 1...4)
                .accessibilityIdentifier("question-\(index)-notes")
            }
        }
    }

    private func questionHeading(_ index: Int) -> some View {
        let question = questions.questions[index]
        return VStack(alignment: .leading, spacing: CC.rhythm.text) {
            CCAdaptiveStack(horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xxs) {
                if count > 1 {
                    Text("Question \(index + 1) of \(count)")
                        .ccType(CC.type.fieldLabel)
                        .foregroundStyle(CC.text.secondary)
                }
                if let header = question.header { CCBadge(header) }
                Spacer(minLength: 0)
            }
            Text(question.text)
                .ccType(CC.type.headline)
                .foregroundStyle(CC.text.primary)
                .fixedSize(horizontal: false, vertical: true)
                .accessibilityAddTraits(.isHeader)
        }
    }

    /// The option whose preview is drawn: the one chosen, or before any choice
    /// the first that has one.
    private func focusedPreview(_ index: Int) -> Int? {
        let question = questions.questions[index]
        if let chosen = draft.entries[index].selected.last { return chosen }
        return question.options.firstIndex { $0.preview != nil }
    }

    private func optionRow(_ option: Int, of index: Int) -> some View {
        let question = questions.questions[index]
        let order = draft.entries[index].selected.firstIndex(of: option)
        let chosen = order != nil
        return Button {
            draft.tap(option: option, of: index, in: questions)
        } label: {
            ChoiceRowLabel(
                symbol: Self.symbol(chosen: chosen, multiSelect: question.multiSelect),
                isChosen: chosen, label: question.options[option].label,
                detail: question.options[option].description,
                order: question.multiSelect ? order.map { $0 + 1 } : nil)
        }
        .buttonStyle(
            CCPressReporter { label, pressed in
                label
                    .background(pressed ? CCSurfaceLevel.surface.pressed : CC.color.surface)
                    .ccAnimation(CC.motion.micro, value: pressed)
            })
        .accessibilityIdentifier("question-\(index)-option-\(option)")
        .accessibilityAddTraits(chosen ? .isSelected : [])
    }

    private func otherRow(_ index: Int) -> some View {
        let question = questions.questions[index]
        let chosen = draft.entries[index].otherChosen
        return Button {
            draft.tapOther(of: index, in: questions)
        } label: {
            ChoiceRowLabel(
                symbol: Self.symbol(chosen: chosen, multiSelect: question.multiSelect),
                isChosen: chosen, label: "Other", detail: "Type your own answer",
                order: nil)
        }
        .buttonStyle(
            CCPressReporter { label, pressed in
                label
                    .background(pressed ? CCSurfaceLevel.surface.pressed : CC.color.surface)
                    .ccAnimation(CC.motion.micro, value: pressed)
            })
        .accessibilityIdentifier("question-\(index)-other")
        .accessibilityAddTraits(chosen ? .isSelected : [])
    }

    private static func symbol(chosen: Bool, multiSelect: Bool) -> String {
        if multiSelect { return chosen ? "checkmark.square.fill" : "square" }
        return chosen ? "largecircle.fill.circle" : "circle"
    }

    private func otherBinding(_ index: Int) -> Binding<String> {
        Binding(get: { draft.entries[index].otherText }, set: { draft.entries[index].otherText = $0 })
    }

    private func notesBinding(_ index: Int) -> Binding<String> {
        Binding(get: { draft.entries[index].notes }, set: { draft.entries[index].notes = $0 })
    }

    // MARK: Review

    /// Every answer before it is sent, as the terminal's "Review your answers".
    private var reviewList: some View {
        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            CCSectionHeader("Review your answers")
            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    ForEach(questions.questions.indices, id: \.self) { index in
                        answerRow(
                            index, answer: draft.answer(for: index, in: questions), canChange: true)
                        if index < count - 1 { CCHairline() }
                    }
                }
            }
        }
    }

    /// What was sent from this phone, once the Mac has confirmed it.
    private func answeredList(_ answers: [QuestionAnswer]) -> some View {
        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            CCSectionHeader("Your answers")
            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    ForEach(questions.questions.indices, id: \.self) { index in
                        answerRow(
                            index, answer: index < answers.count ? answers[index] : nil,
                            canChange: false)
                        if index < count - 1 { CCHairline() }
                    }
                }
            }
        }
    }

    private func answerRow(_ index: Int, answer: QuestionAnswer?, canChange: Bool) -> some View {
        let question = questions.questions[index]
        return VStack(alignment: .leading, spacing: CC.space.xxs) {
            CCAdaptiveStack(horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xxs) {
                if let header = question.header { CCBadge(header) }
                Spacer(minLength: 0)
                if canChange {
                    CCButton("Change", variant: .ghost, size: .sm) { step = index }
                        .accessibilityIdentifier("question-\(index)-change")
                }
            }
            Text(question.text)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if let summary = answer.flatMap({ questions.summary(of: $0, for: index) }) {
                Text(summary)
                    .ccType(CC.type.callout)
                    .foregroundStyle(CC.text.primary)
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                Text("Not answered yet")
                    .ccType(CC.type.callout)
                    .foregroundStyle(CC.color.warning)
            }
            if let notes = answer?.notes {
                Text("Notes: \(notes)")
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, CC.space.sm)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("question-\(index)-answer")
    }

    // MARK: Read-only

    /// A question that cannot be answered from here, shown whole so the reader
    /// knows what is being asked at the Mac.
    private func readOnlyQuestion(_ index: Int) -> some View {
        let question = questions.questions[index]
        return VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            questionHeading(index)
            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    ForEach(question.options.indices, id: \.self) { option in
                        ChoiceRowLabel(
                            symbol: Self.symbol(chosen: false, multiSelect: question.multiSelect),
                            isChosen: false, label: question.options[option].label,
                            detail: question.options[option].description, order: nil)
                        .background(CC.color.surface)
                        if option < question.options.count - 1 { CCHairline() }
                    }
                }
            }
        }
    }

    // MARK: Footer

    private var pinnedFooter: some View {
        VStack(spacing: 0) {
            if status.isAnswerable, let notice {
                notice
                    .padding(.horizontal, CC.space.md)
                    .padding(.top, CC.space.sm)
                    .padding(.bottom, CC.space.xs)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(CC.color.bg)
            }
            if status.isAnswerable {
                CCActionBar {
                    navigation
                    if !typeSize.isAccessibilitySize { subordinateControls }
                }
            } else if let comeBackToThis {
                CCActionBar { comeBackButton(comeBackToThis) }
            }
        }
        .ccScrollCap()
    }

    /// What an answerable card says above its controls: why the last answer
    /// did not land, a refused Face ID, or the sample fleet.
    private var notice: AnyView? {
        if let attempt { return AnyView(ResolutionBanner(attempt: attempt, compose: nil)) }
        if let authNotice {
            return AnyView(CCBanner("Face ID", message: authNotice, tone: .warning, icon: "faceid"))
        }
        if let sampleNotice {
            return AnyView(CCBanner("Sample fleet", message: sampleNotice, tone: .info, icon: "eye"))
        }
        return nil
    }

    @ViewBuilder
    private var navigation: some View {
        if step > 0 {
            CCActionPair {
                CCButton("Back", variant: .secondary, size: .lg, fullWidth: true) { step -= 1 }
                    .accessibilityIdentifier("question-back")
            } allow: {
                forward
            }
        } else {
            forward
        }
    }

    @ViewBuilder
    private var forward: some View {
        if count == 1 || isReview {
            CCButton(
                "Submit", variant: .primary, size: .lg, fullWidth: true,
                isLoading: spinningControl == "submit",
                disabledReason: CCDisabledReason(submitBlockedReason)
            ) {
                if let answers = draft.answers(in: questions) {
                    submit(.answers(answers), key: "submit")
                }
            }
            .accessibilityIdentifier("question-submit")
        } else {
            CCButton(step == count - 1 ? "Review" : "Next", variant: .primary, size: .lg, fullWidth: true) {
                step += 1
            }
            .accessibilityIdentifier("question-next")
        }
    }

    /// Decline, Reply instead, and in the Deck `Come back to this` — all
    /// quieter than Submit: two are other ways of not answering and the third
    /// is not an answer at all.
    @ViewBuilder
    private var subordinateControls: some View {
        if replying {
            replyField
        } else {
            VStack(spacing: CC.rhythm.controls) {
                CCAdaptiveStack(horizontalSpacing: CC.space.sm, verticalSpacing: CC.rhythm.controls) {
                    CCButton(
                        "Decline", icon: "xmark", variant: .secondary, size: .md, fullWidth: true,
                        isLoading: spinningControl == "decline",
                        disabledReason: CCDisabledReason(blockedReason)
                    ) {
                        submit(.decline(message: nil), key: "decline")
                    }
                    .accessibilityIdentifier("question-decline")
                    .accessibilityHint("Declines the question, as Escape does at the Mac.")
                    CCButton(
                        "Reply instead", icon: "text.bubble", variant: .ghost, size: .md,
                        fullWidth: true
                    ) {
                        withAnimation(CC.motion.small) { replying = true }
                    }
                    .accessibilityIdentifier("question-reply")
                }
                if let comeBackToThis { comeBackButton(comeBackToThis) }
            }
        }
    }

    private var replyField: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            CCField(
                label: "Your reply", text: $reply, placeholder: "Tell Claude what to do instead",
                axis: .vertical, lineLimit: 1...4)
            CCAdaptiveStack(
                horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xs,
                verticalAlignment: .top
            ) {
                CCButton("Cancel", variant: .ghost, size: .sm) {
                    withAnimation(CC.motion.small) { replying = false }
                }
                Spacer(minLength: CC.space.xs)
                CCButton(
                    "Send reply", variant: .secondary, size: .md,
                    isLoading: spinningControl == "reply",
                    disabledReason: CCDisabledReason(blockedReason)
                ) {
                    submit(
                        .decline(message: reply.trimmingCharacters(in: .whitespacesAndNewlines)),
                        key: "reply")
                }
                .disabled(reply.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                .accessibilityIdentifier("question-send-reply")
            }
            Text("Claude gets your words instead of an answer, and carries on.")
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func comeBackButton(_ action: @escaping () -> Void) -> some View {
        CCButton(
            "Come back to this", icon: "arrow.uturn.down", variant: .ghost, size: .md,
            fullWidth: true, haptic: nil, action: action
        )
        .accessibilityIdentifier("deck-later")
        .accessibilityHint("Moves this decision to the back of the queue. It stays unanswered.")
    }

    // MARK: Submission

    private func submit(_ decision: AnswerDecision, key: String) {
        guard status.isAnswerable, !inFlight, blockedReason == nil else { return }
        if model.sampleFleetActive {
            withAnimation(CC.motion.small) {
                sampleNotice = DecisionCardView.sampleNotice(for: decision)
            }
            CCHaptic.warning.fire()
            return
        }
        spinningControl = key
        Task {
            // A HIGH card's affirmative answer takes a face, as Allow does on the
            // decision card. Declining never does.
            if risk == .high, case .answers = decision {
                let outcome = await BiometricGate.confirm(
                    reason: BiometricGate.reason(for: approval.card.toolName))
                guard outcome.isAuthenticated else {
                    spinningControl = nil
                    withAnimation(CC.motion.small) { authNotice = outcome.message }
                    CCHaptic.warning.fire()
                    return
                }
            }
            authNotice = nil
            let result = await model.answer(item: approval, decision: decision)
            spinningControl = nil
            switch result {
            case .applied: CCHaptic.success.fire()
            case .indeterminate, .duplicate, .answeredAtKeyboard: CCHaptic.warning.fire()
            case .staleCard, .rejected, .failed: CCHaptic.failure.fire()
            }
            if result.isTerminal { onSettled?() }
        }
    }
}

/// One choice: its mark, its label and description, and — on a question that
/// takes several — the order it was picked in, which is the order it is sent.
private struct ChoiceRowLabel: View {
    let symbol: String
    let isChosen: Bool
    let label: String
    let detail: String?
    let order: Int?

    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: CC.space.sm) {
            CCIcon(symbol, size: 18, weight: .regular, relativeTo: .callout)
                .foregroundStyle(isChosen && isEnabled ? CC.text.primary : CC.text.tertiary)
            VStack(alignment: .leading, spacing: CC.space.xxs) {
                Text(label)
                    .ccType(CC.type.callout)
                    .foregroundStyle(isEnabled ? CC.text.primary : CC.text.disabled)
                    .multilineTextAlignment(.leading)
                    .fixedSize(horizontal: false, vertical: true)
                if let detail {
                    Text(detail)
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                        .multilineTextAlignment(.leading)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: CC.space.xs)
            if let order {
                Text("\(order)")
                    .ccType(CC.type.mono)
                    .foregroundStyle(CC.text.secondary)
                    .accessibilityLabel("picked \(order)")
            }
        }
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, CC.space.sm)
        .frame(minHeight: CC.size.controlLg)
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
    }
}
