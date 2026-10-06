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
/// question has an answer) and Decline (as Escape at the Mac). Every state that is not
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

    /// Whether the answers on screen may still be changed. Not while an answer
    /// is being sent: Submit captured the draft when it was tapped, so an edit
    /// made during the send would show an answer that was never sent.
    static func acceptsEdits(status: QuestionCardStatus, inFlight: Bool) -> Bool {
        status.isAnswerable && !inFlight
    }

    private var risk: RiskClass { approval.assessment(profile: model.daemonProfile).effective }

    /// Everything that stops every control on this card — the decision card's
    /// own list.
    private var blockedReason: String? {
        DecisionCardView.blockedReason(model: model, approval: approval, inFlight: inFlight)
    }

    /// Whether this step's forward control is Submit.
    private var submitsHere: Bool { count == 1 || isReview }

    private var isComplete: Bool { draft.answers(in: questions) != nil }

    /// What Submit is waiting for while the answers are incomplete. Not a
    /// warning: an untouched form is not a fault.
    private var incompleteNote: String {
        count == 1 ? "Choose an answer to submit." : "Answer every question to submit."
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
                        Group {
                            if isReview { reviewList } else { questionStep(step) }
                        }
                        .disabled(!Self.acceptsEdits(status: status, inFlight: inFlight))
                        // After the question, beside the bar it is about: above
                        // it, at AX5 this took the screen and the question
                        // started below the fold.
                        if risk == .high {
                            Text("Submitting takes Face ID. \(risk.rationale)")
                                .ccType(CC.type.footnote)
                                .foregroundStyle(CC.text.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
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

    /// Which run is asking — the whole run label, so two runs of one project
    /// stay apart — and, on a HIGH card, its tag. Why the answer takes Face ID
    /// is said after the question, not here. No title: the sheet's own says
    /// "Question", and the question itself is the heading that matters.
    private var header: some View {
        HStack(alignment: .firstTextBaseline, spacing: CC.space.sm) {
            Text(verbatim: model.runLabel(for: approval.sessionKey).inline)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
                .lineLimit(2)
                .frame(maxWidth: .infinity, alignment: .leading)
            if risk == .high { CCRiskTag(risk) }
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
                        if option < question.options.count - 1 || question.takesOther {
                            CCHairline()
                        }
                    }
                    if question.takesOther {
                        otherRow(index)
                    }
                }
            }
            if entry.otherChosen {
                CCField(
                    label: "Other", text: otherBinding(index), placeholder: "Type your answer",
                    labelOnContentColumn: false)
                .accessibilityIdentifier("question-\(index)-other-text")
            }
            if question.takesNotes {
                CCField(
                    label: "Notes", text: notesBinding(index),
                    placeholder: "Optional notes for Claude on this choice", axis: .vertical,
                    lineLimit: 1...4, labelOnContentColumn: false)
                .accessibilityIdentifier("question-\(index)-notes")
            }
        }
    }

    private func questionHeading(_ index: Int) -> some View {
        let question = questions.questions[index]
        return VStack(alignment: .leading, spacing: CC.rhythm.text) {
            // Only when it has something to show: empty, it was still a row,
            // and the stack's spacing above the question with it.
            if count > 1 || question.header != nil {
                CCAdaptiveStack(horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xxs) {
                    if count > 1 {
                        Text("Question \(index + 1) of \(count)")
                            .ccType(CC.type.fieldLabel)
                            .foregroundStyle(CC.text.secondary)
                    }
                    if let header = question.header { CCBadge(header) }
                    CCAdaptiveSpacer(minLength: 0)
                }
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

    /// What was sent from the phone, once the Mac has confirmed it. Under the
    /// banner that already says so, with no heading saying it a third time.
    private func answeredList(_ answers: [QuestionAnswer]) -> some View {
        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
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
                CCAdaptiveSpacer(minLength: 0)
                if canChange {
                    CCButton("Change", variant: .ghost, size: .sm) { step = index }
                        .accessibilityIdentifier("question-\(index)-change")
                }
            }
            Text(question.text)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
            let choices = answer.map { questions.choices(of: $0, for: index) } ?? []
            if !choices.isEmpty {
                ForEach(choices.indices, id: \.self) { choice in
                    Text(choices[choice])
                        .ccType(CC.type.callout)
                        .foregroundStyle(CC.text.primary)
                        .fixedSize(horizontal: false, vertical: true)
                }
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
                        // No radio or checkbox: nothing here can be chosen, and
                        // an empty control reads as a form still to fill in.
                        ChoiceRowLabel(
                            symbol: nil, isChosen: false, label: question.options[option].label,
                            detail: question.options[option].description, order: nil)
                        .background(CC.color.surface)
                        if let preview = question.options[option].preview {
                            CCMonoBlock(preview, showsCopy: false)
                                .padding(.horizontal, CC.space.md)
                                .padding(.bottom, CC.space.sm)
                                .accessibilityIdentifier("question-\(index)-option-\(option)-preview")
                        }
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
                    barNote
                }
            } else if let comeBackToThis {
                CCActionBar { comeBackButton(comeBackToThis) }
            }
        }
        .ccScrollCap()
        .ccActionBarSafeAreaFill(status.isAnswerable || comeBackToThis != nil)
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

    /// Why a control on the bar cannot be pressed, said **once**, under all of
    /// them: a reason that stops Submit and Decline alike was printed under
    /// each. Submit's own prerequisite keeps its slot when it is met — drawn
    /// clear rather than removed — so choosing the last answer does not move the
    /// bar (it jumped 22.7pt when the line went).
    @ViewBuilder
    private var barNote: some View {
        if let blockedReason {
            HStack(alignment: .firstTextBaseline, spacing: CC.space.xxs + 1) {
                CCIcon(
                    "exclamationmark.circle.fill", size: 11, weight: .semibold,
                    relativeTo: .caption
                )
                .foregroundStyle(CC.color.warning)
                CCProse(blockedReason, style: CC.type.footnote, color: CC.color.warning)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .accessibilityElement(children: .combine)
            .accessibilityLabel(CCInlineCode.plain(blockedReason))
        } else if submitsHere {
            Text(incompleteNote)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .opacity(isComplete ? 0 : 1)
                .accessibilityHidden(isComplete)
        }
    }

    @ViewBuilder
    private var navigation: some View {
        if step > 0 {
            CCActionPair {
                CCButton("Back", variant: .secondary, size: .lg, fullWidth: true) { step -= 1 }
                    .disabled(inFlight)
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
                isLoading: spinningControl == "submit"
            ) {
                if let answers = draft.answers(in: questions) {
                    submit(.answers(answers), key: "submit")
                }
            }
            // The reason is on the bar, once (`barNote`); the control carries
            // it as its hint.
            .disabled(blockedReason != nil || !isComplete)
            .accessibilityHint(blockedReason ?? (isComplete ? "" : incompleteNote))
            .accessibilityIdentifier("question-submit")
        } else {
            CCButton(step == count - 1 ? "Review" : "Next", variant: .primary, size: .lg, fullWidth: true) {
                step += 1
            }
            .disabled(inFlight)
            .accessibilityIdentifier("question-next")
        }
    }

    /// Decline, and in the Deck `Come back to this` — both quieter than Submit:
    /// one is another way of not answering and the other is not an answer at all.
    private var subordinateControls: some View {
        VStack(spacing: CC.rhythm.controls) {
            CCButton(
                "Decline", icon: "xmark", variant: .secondary, size: .md, fullWidth: true,
                isLoading: spinningControl == "decline"
            ) {
                submit(.decline, key: "decline")
            }
            .disabled(blockedReason != nil)
            .accessibilityIdentifier("question-decline")
            .accessibilityHint(blockedReason ?? "Declines the question, as Escape does at the Mac.")
            if let comeBackToThis { comeBackButton(comeBackToThis) }
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
        // A HIGH card's affirmative answer takes a face, as Allow does on the
        // decision card. Declining never does.
        var needsFace = false
        if risk == .high, case .answers = decision { needsFace = true }
        Task {
            let sent = await DecisionCardView.send(
                decision, for: approval, model: model, needsFace: needsFace)
            spinningControl = nil
            switch sent {
            case .faceRefused(let message):
                withAnimation(CC.motion.small) { authNotice = message }
            case .answered(let result):
                authNotice = nil
                if result.isTerminal { onSettled?() }
            }
        }
    }
}

/// One choice: its mark, its label and description, and — on a question that
/// takes several — the order it was picked in, which is the order it is sent.
/// No mark on a choice that cannot be made.
private struct ChoiceRowLabel: View {
    let symbol: String?
    let isChosen: Bool
    let label: String
    let detail: String?
    let order: Int?

    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: CC.space.sm) {
            if let symbol {
                CCIcon(symbol, size: 18, weight: .regular, relativeTo: .callout)
                    .foregroundStyle(isChosen && isEnabled ? CC.text.primary : CC.text.tertiary)
            }
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
