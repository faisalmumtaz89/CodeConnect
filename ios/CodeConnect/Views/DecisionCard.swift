import SwiftUI

/// An approval is a document, not a dialog.
///
/// One implementation, two homes: the per-session sheet and the cross-fleet
/// Deck. They must not diverge — a risk gate that applies in one place and not
/// the other is worse than no gate, because it is the one you stop checking for.
///
/// **The read gate is not one of the tiers.** Every class waits until the
/// command has been on screen; what the class decides is how much *friction*
/// comes after that — a tap, a hold, a face. At reading sizes a LOW command fits
/// above the action bar on the first frame, so its gate is already open by the
/// time the card is drawn and the reader never meets it. At AX5 it is the only
/// thing standing between one tap and approving a path the product never
/// rendered.
///
/// The gates, by class:
///
///   * **LOW** — a single tap, once the command has been shown. Allow is the
///     filled white primary.
///   * **MEDIUM** — the same gate, and a fill morph and haptic at the moment it
///     opens, because at MEDIUM the reader usually had to scroll for it.
///   * **HIGH** — a 1.2-second hold *and* Face ID (passcode fallback). The
///     controls look exactly as they do at LOW: Allow is the filled white
///     affirmative, Deny the bordered alternative. What changes is the *effort*,
///     not the colour.
///
/// **Risk changes friction, never chrome.** An earlier version inverted the
/// hierarchy at HIGH — Deny took the white fill, Allow was drawn in `danger` —
/// on the reasoning that the safe action should carry the weight when the stakes
/// are highest. It reads well and it is wrong twice: it moves the primary
/// treatment onto something that is not the primary action, and it spends `red`
/// on approving, which is not destruction. Red belongs to `Unpair and erase
/// cache`, `Forget this iPhone's SSH key`, and to failures. A reader who learns
/// that red means "destroys something" is then told, on the busiest screen, that
/// it also means "the ordinary affirmative, but carefully" — and the signal is
/// gone from both.
///
/// Deny is never gated by any of it. Denying is the safe direction, and a
/// product whose promise is "silence never decides anything" cannot put friction
/// in front of saying no.
struct DecisionCardView: View {
    let approval: ApprovalItem
    /// Called once the card reaches a terminal state, so the Deck can advance.
    var onSettled: (() -> Void)?
    /// The Deck's own footer control, rendered inside the card's action bar so
    /// it reads as clearly subordinate to Deny.
    var comeBackToThis: (() -> Void)?

    @Environment(AppModel.self) private var model

    /// The read-before-you-decide gate. A geometry test, never a lifecycle one —
    /// see `ReadGate`.
    @State private var gate = ReadGate()
    /// Where the gate's four measurements live — **deliberately outside the
    /// view's own state.**
    ///
    /// Writing a scroll-varying number into `@State` re-runs `body`, which
    /// re-creates the `GeometryReader`s, which re-emit the preference. Inside a
    /// scroll view that loop never settles: measured, the app stopped reporting
    /// itself idle for the whole of a drag and XCUITest's quiescence wait timed
    /// out on every scroll. A reference type in `@State` keeps its identity
    /// across body evaluations and mutating it invalidates nothing, so the
    /// *decision* is the only thing that is view state — and the decision is
    /// sticky, so it can change at most twice in a card's life.
    @State private var probe = ReadGateProbe()
    /// When this card was first laid out. A gate satisfied by the first frame
    /// was not satisfied by a scroll, and a haptic never fires on entrance.
    @State private var appearedAt = Date()
    @State private var denyReason = ""
    @State private var showDenyField = false
    /// Which control is spinning. Purely cosmetic — whether an answer is
    /// actually in flight lives on the model, keyed by request id, so dismissing
    /// and re-presenting this card cannot start a second one.
    @State private var spinningControl: String?
    @State private var composeResult: ComposeAttempt?
    /// Why the biometric check did not pass. Shown, never swallowed.
    @State private var authNotice: String?
    /// When the answer left, so an unconfirmed one can say how long it has been
    /// unconfirmed rather than spinning forever.
    @State private var sentAt: Date?
    @Environment(\.dynamicTypeSize) private var typeSize

    /// The run this card came from. Its *name* is what the header shows and
    /// its *key* is what the answer is scoped to — the two are different
    /// strings on a daemon that mints uids, and only one of them identifies a
    /// run.
    private var sessionName: String { approval.sessionName }
    private var assessment: RiskAssessment { approval.assessment(profile: model.daemonProfile) }
    private var risk: RiskClass { assessment.effective }

    /// Two guards, because they cover different windows: `spinningControl`
    /// blocks a fast second tap before the model has even been entered, and the
    /// model's own flag survives this card being dismissed and re-presented.
    private var inFlight: String? {
        if let spinningControl { return spinningControl }
        return model.isAnswering(approval) ? "allow" : nil
    }
    private var attempt: AnswerAttempt? { model.lastAttempt(for: approval) }
    private var verification: ApprovalCard.Verification { approval.card.verification }
    private var options: [PaneOptions.Option] {
        approval.paneSnapshot.map(PaneOptions.parse) ?? []
    }

    var body: some View {
        ScrollView {
            // Deliberately **not** a `LazyVStack`. The gate used to rely on one:
            // a 1pt marker with `onAppear` was nested inside a lazy child on the
            // premise that laziness would turn the callback into "scrolled into
            // view". Laziness applies to a stack's *direct children*, the marker
            // was one level deeper, and the gate armed at first layout with the
            // command off screen. The gate is a geometry test now, and a stack
            // whose probes only report once a child has been materialised is a
            // hazard this card cannot carry. Eight children do not need laziness.
            VStack(alignment: .leading, spacing: CC.space.xl) {
                header
                commandBlock
                ifYouDenyBlock
                if !options.isEmpty { exactOptions }
                disclosures
                verificationLine
                // The status banner is **not** here any more; it is pinned above
                // the action bar. See `pinnedFooter`.
                //
                // At accessibility sizes the two subordinate controls leave the
                // pinned bar and land here, in the card's own footer — which is
                // where `Come back to this` belongs anyway. Measured at AX5:
                // four stacked controls made the bar 440pt tall and left the
                // document a four-line sliver, so the command you are being
                // asked to approve could not be read at all.
                if typeSize.isAccessibilitySize { subordinateControls }
                Color.clear.frame(height: CC.space.xs)
            }
            .padding(.horizontal, CC.space.md)
            .padding(.top, CC.space.md)
        }
        .scrollIndicators(.hidden)
        .background(CC.color.bg)
        .background {
            GeometryReader { proxy in
                Color.clear.preference(
                    key: ViewportBottomKey.self, value: proxy.frame(in: .global).maxY)
            }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) { pinnedFooter }
        // After `safeAreaInset`, so the bar's own geometry reaches these.
        .onPreferenceChange(CommandBottomKey.self) { value in
            probe.commandBottom = value
            updateGate()
        }
        .onPreferenceChange(ProvenanceBottomKey.self) { value in
            probe.provenanceBottom = value
            updateGate()
        }
        .onPreferenceChange(ViewportBottomKey.self) { value in
            probe.viewportBottom = value
            updateGate()
        }
        .onPreferenceChange(ActionBarHeightKey.self) { value in
            probe.actionBarHeight = value
            updateGate()
        }
        .onAppear { appearedAt = Date() }
    }

    // MARK: Header

    private var header: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: CC.space.sm) {
                // Its own accessibility element, deliberately: "Bash" is the
                // first thing a reader — human or automated — looks for.
                Text(approval.card.toolName)
                    .ccType(CC.type.title)
                    .foregroundStyle(CC.text.primary)
                Spacer(minLength: CC.space.xs)
                CCBadge(risk: risk)
                    // A badge is a piece of text that happens to have a border.
                    // `CCBadge` publishes the spelled-out class ("Risk HIGH.
                    // Destructive, credentialed, or publishes something.") but
                    // its own element carries no trait, so assistive technology
                    // — and the approval tests, which read the same tree —
                    // classify it as an untyped container rather than as
                    // something to read out with the heading.
                    .accessibilityAddTraits(.isStaticText)
            }

            // One left edge for the whole card. The pulsing dot that used to
            // lead this line pushed the identity 21pt right of every other line
            // on the card — a third vertical edge on a screen that gets exactly
            // two — and it was a fourth encoding of "pending" beside the
            // risk badge above it and the amber wait clock below. The dot went;
            // the column came back.
            HStack(spacing: CC.space.xs) {
                Text(folderName)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.secondary)
                Text("·")
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.disabled)
                CCIdentity(name: identity.name, tail: identity.tail)
                Spacer(minLength: 0)
            }
            .padding(.top, 6)

            if let cwd = model.summary(for: approval.sessionKey)?.cwd {
                Text(cwd)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.disabled)
                    .lineLimit(1)
                    .truncationMode(.head)
                    .padding(.top, 2)
                    .accessibilityLabel("Working directory, \(cwd)")
            }

            if approval.isPending {
                VStack(alignment: .leading, spacing: CC.space.xxs) {
                    CCWaitClock(since: approval.requestedAt, now: model.now, prefix: "waiting")
                    // The promise, stated where the decision is made. Verbatim,
                    // and it survives every redesign.
                    Text("Nothing decides this but you. There is no timer on this card.")
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.tertiary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(.top, CC.space.md)
            }
        }
    }

    private var folderName: String {
        model.summary(for: approval.sessionKey)?.folderName ?? sessionName
    }

    private var identity: (name: String, tail: String?) {
        let label =
            AppModel.identityLabels(for: model.summaries)[approval.sessionKey] ?? sessionName
        let parts = label.components(separatedBy: " · ")
        return (parts.first ?? label, parts.count > 1 ? parts[1] : nil)
    }

    // MARK: Command

    private var commandBlock: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            CCSectionHeader("Exact command")
            CCMonoBlock(approval.card.primaryText(verification: verification))
                // The gate's first half, measured on the block that carries the
                // command itself rather than on a marker somewhere beneath it.
                .background { bottomProbe(CommandBottomKey.self) }

            if let intent = ToolSummary.intent(
                tool: approval.card.toolName, input: approval.card.toolInput)
            {
                Text(intent)
                    .ccType(CC.type.callout)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            // Two lines, not three sizes at three colours: the
            // rationale in `callout` at the risk colour, the provenance in
            // `monoSmall` `textTertiary`.
            Text(rationale)
                .ccType(CC.type.callout)
                .foregroundStyle(risk.ccTone == .neutral ? CC.text.secondary : risk.ccTone.color)
                .fixedSize(horizontal: false, vertical: true)
            Text(assessment.provenance)
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)
                // The gate's second half, and the one HIGH names explicitly.
                .background { bottomProbe(ProvenanceBottomKey.self) }
        }
    }

    /// Reports a view's bottom edge in screen coordinates.
    ///
    /// Screen coordinates rather than the scroll view's own space because the
    /// thing the block has to clear is the **pinned action bar**, and
    /// `safeAreaInset` draws that bar over the scroll view rather than
    /// shortening it — so a test against the scroll view's frame would call a
    /// command "seen" while the bar was covering it, which is exactly what a
    /// render of a MEDIUM card at AX5 showed.
    private func bottomProbe<K: PreferenceKey>(_ key: K.Type) -> some View
    where K.Value == CGFloat? {
        GeometryReader { proxy in
            Color.clear.preference(key: key, value: proxy.frame(in: .global).maxY)
        }
    }

    private var rationale: String {
        guard let pattern = assessment.matchedPattern else { return risk.rationale }
        return "\(risk.rationale) Matched \(pattern)."
    }

    /// The arming moment. The phone is telling you *"you may now
    /// decide"*, so it says so with a fill morph and a light impact rather than
    /// by silently enabling a control.
    ///
    /// Sticky: a flag, once set, stays set. Scrolling *past* the command does
    /// not un-read it, and a fling that never produced a frame with the block
    /// inside the viewport leaves the gate shut — which is the safe direction.
    private func updateGate() {
        // Nothing to say until the scroll view itself has been laid out. Not a
        // fallback, not a sentinel: no measurement means no arming.
        guard let visibleBottom = probe.visibleBottom else { return }
        var next = gate
        if ReadGate.isVisible(bottom: probe.commandBottom, visibleBottom: visibleBottom) {
            next.hasSeenCommand = true
        }
        if ReadGate.isVisible(bottom: probe.provenanceBottom, visibleBottom: visibleBottom) {
            next.hasSeenProvenance = true
        }
        guard next != gate else { return }
        let wasOpen = gate.isOpen(at: risk)
        withAnimation(CC.motion.medium) { gate = next }
        // Never on entrance: a card deep-linked from a push notification whose
        // command already fits on the first frame arms silently, because the
        // notification has already buzzed and a second buzz 400ms behind it
        // reads as a second event.
        guard !wasOpen, next.isOpen(at: risk), approval.isPending else { return }
        if Date().timeIntervalSince(appearedAt) > Self.entranceWindow { CCHaptic.light.fire() }
    }

    /// How long after the card appears an arming still counts as "it was
    /// already on screen" rather than "you scrolled to it".
    private static let entranceWindow: TimeInterval = 0.6

    // MARK: If you deny

    /// Absent from the previous build: the consequence of saying no, stated
    /// beside the consequence of saying yes.
    private var ifYouDenyBlock: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            CCSectionHeader("If you deny")
            Text(denyConsequence)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// **Never invent a consequence.** Where the tool is unrecognised, the
    /// honest third string says only what is certain.
    private var denyConsequence: String {
        switch approval.card.toolName {
        case "Bash", "Write", "Edit", "NotebookEdit", "Read", "Glob", "Grep":
            return
                "Claude receives \"denied\" and picks another approach. Nothing is lost and the session keeps running."
        case "WebFetch", "WebSearch":
            return "The request is skipped. Claude will report what it could not do."
        default:
            return "Claude receives \"denied\". What it does next is up to it."
        }
    }

    // MARK: Options

    /// Claude's own numbered options are the *primary* answer path when a pane
    /// offers them, and they obey the same risk gate as Allow.
    private var exactOptions: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            CCSectionHeader("Claude's exact options")
            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    ForEach(Array(options.enumerated()), id: \.element.id) { index, option in
                        optionRow(option, separator: index < options.count - 1)
                    }
                }
            }
            // The reason once, for the group, rather than under each of three
            // rows — `ccDisabled` on the card disables every option inside it
            // and draws the single visible explanation a dead control owes.
            .ccDisabled(CCDisabledReason(blockedReason))

            Text("Accepting one of Claude's suggestions means picking its numbered option.")
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func optionRow(_ option: PaneOptions.Option, separator: Bool) -> some View {
        Button {
            submit(.option(index: option.index), key: "option-\(option.index)")
        } label: {
            OptionRowLabel(
                index: option.index, label: option.label,
                isBusy: inFlight == "option-\(option.index)")
        }
        .buttonStyle(
            CCPressReporter { label, pressed in
                label
                    .background(pressed ? CCSurfaceLevel.surface.pressed : CC.color.surface)
                    .overlay(alignment: .bottom) {
                        if separator { CCHairline() }
                    }
                    .ccAnimation(CC.motion.micro, value: pressed)
            }
        )
        .accessibilityLabel("Option \(option.index): \(option.label)")
    }

    // MARK: Disclosures

    private var disclosures: some View {
        VStack(alignment: .leading, spacing: 0) {
            CCDisclosure("Full tool input") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    CCMonoBlock(approval.card.toolInput.prettyJSONString)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("request_id     \(approval.card.requestID)")
                        Text("payload_hash   \(approval.card.payloadHash)")
                        if let mode = approval.card.permissionMode {
                            Text("permission_mode \(mode)")
                        }
                        if let declared = approval.risk {
                            Text("risk.class     \(declared.cls)")
                        }
                    }
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                }
            }

            if let pane = approval.paneSnapshot {
                CCDisclosure("The Mac's screen when Claude asked") {
                    VStack(alignment: .leading, spacing: CC.space.xs) {
                        // This block must never imply liveness: no pulsing dot,
                        // no "live" wording, no auto-refresh.
                        Text(
                            "Not live. A snapshot taken at \(approval.requestedAt, format: .dateTime.hour().minute().second())."
                        )
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.tertiary)
                        CCMonoBlock(pane.trimmingCharacters(in: .whitespacesAndNewlines))
                    }
                }
            }
        }
    }

    // MARK: Verification

    /// A footnote when it passes. On mismatch it is not a footnote — it becomes
    /// the card's banner and both actions die, because a card we cannot vouch
    /// for is not a card.
    @ViewBuilder
    private var verificationLine: some View {
        if verification.hashMatchesDisplayText {
            HStack(alignment: .firstTextBaseline, spacing: CC.space.xs) {
                CCIcon("checkmark.shield", size: 14, weight: .semibold, relativeTo: .footnote)
                    .foregroundStyle(CC.color.success)
                // Deliberately its own element rather than combined with the
                // glyph: this sentence is the one a reader — and the approval
                // test — goes looking for by name.
                Text(verification.summary)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.color.success)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
            }
        }
    }

    /// **One banner**, as a value rather than as an `if` ladder in a
    /// `ViewBuilder`.
    ///
    /// A hash mismatch outranks a resolution, which outranks an
    /// already-resolved notice, which outranks a refused biometric. Choosing it
    /// here rather than in the body is what lets `pinnedFooter` ask *whether
    /// there is one* — `some View` cannot be asked that, and the answer decides
    /// whether the card reserves a strip above the action bar at all.
    private var statusBanner: StatusBanner? {
        if !verification.hashMatchesDisplayText { return .hashMismatch }
        if let attempt { return .resolution(attempt) }
        if let outcome = approval.outcome { return .alreadyResolved(outcome) }
        if let authNotice { return .authRefused(authNotice) }
        return nil
    }

    /// The card's one banner slot, by case.
    private enum StatusBanner {
        case hashMismatch
        case resolution(AnswerAttempt)
        case alreadyResolved(AnswerOutcome)
        /// Why the biometric check did not pass, in the gate's own words.
        case authRefused(String)
    }

    @ViewBuilder
    private func bannerView(_ banner: StatusBanner) -> some View {
        switch banner {
        case .hashMismatch:
            CCBanner(
                "Hash mismatch",
                message:
                    "The text on this card does not match its hash. It cannot be answered safely.",
                tone: .danger, icon: "exclamationmark.shield.fill")
        case .resolution(let attempt):
            ResolutionBanner(attempt: attempt, compose: composeResult)
        case .alreadyResolved(let outcome):
            CCBanner(
                "Already resolved",
                message:
                    "\(outcome.decisionLabel) \(outcome.resolvedBy == .phone ? "from this app" : "at the keyboard") · \(Format.age(since: outcome.resolvedDate, now: model.now)) ago",
                tone: .info, icon: "checkmark.seal")
        case .authRefused(let notice):
            // The gate's own words in the message, never a paraphrase, and never
            // swallowed: a biometric check that failed silently is
            // indistinguishable from a tap that did nothing.
            CCBanner("Face ID", message: notice, tone: .warning, icon: "faceid")
        }
    }

    // MARK: Actions

    /// **Everything pinned to the bottom of the card: the banner, then the bar.**
    ///
    /// The banner used to live in the scroll content, as the seventh child of a
    /// document the reader has already scrolled to the end of — so it appeared
    /// *underneath* the bar that had just refused them. Measured on a render of
    /// the refused-biometric state: the banner box at y=613.33 h=53.00 against a
    /// bar top edge of y=631.00, which hides 66.6% of it, and its message — the
    /// only part that says *what went wrong* — at y=640.33 h=14.00, **100%
    /// occluded**. A refused Face ID that cannot state its reason is, measured,
    /// indistinguishable from a tap that did nothing.
    ///
    /// The link-failure banner is already pinned above the bar. This puts the
    /// card's whole banner slot in the same place, so the fix is structural
    /// rather than one case's: a hash mismatch, a resolution receipt, an
    /// already-resolved notice and a refused biometric all arrive at the moment
    /// the reader is looking at the bottom of the card, and all four were in the
    /// same trap.
    ///
    /// Pinned as a **sibling of the bar rather than inside it**, because the bar
    /// disappears the moment an answer is terminal (`actionBar` draws nothing
    /// once `attempt?.isTerminal`), and that is precisely when the resolution
    /// receipt has to be readable.
    private var pinnedFooter: some View {
        VStack(spacing: 0) {
            if let statusBanner {
                bannerView(statusBanner)
                    .padding(.horizontal, CC.space.md)
                    .padding(.top, CC.space.sm)
                    .padding(.bottom, CC.space.xs)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    // Opaque, and the same fill as the document: the strip is
                    // drawn *over* the scroll view, and a transparent one would
                    // let the last line of the command run under the banner.
                    .background(CC.color.bg)
                    .transition(.opacity)
            }
            actionBar
        }
        // The 45% ceiling, applied to the footer as a whole rather than to the
        // bar alone — otherwise a banner and a bar could each keep inside 45%
        // and together own the screen. Applied *before* the height probe below
        // on purpose: what the read gate has to clear is the height the footer
        // actually draws at, not the height its content wanted.
        .ccScrollCap()
        .background {
            GeometryReader { proxy in
                // How much of the document the pinned footer is covering, and so
                // what the command block has to clear to count as read. The
                // banner is part of that: a command hidden behind a Face ID
                // notice has not been seen either.
                Color.clear.preference(
                    key: ActionBarHeightKey.self, value: CGFloat?.some(proxy.size.height))
            }
        }
        .ccAnimation(CC.motion.small, value: statusBanner != nil)
    }

    @ViewBuilder
    private var actionBar: some View {
        if approval.outcome == nil, attempt?.isTerminal != true {
            CCActionBar {
                buttonRow

                // The answer left, the daemon has not answered, and the card
                // says so with a live counter rather than spinning forever. It
                // never advances and it never resolves itself.
                if let sentAt, inFlight != nil, model.now.timeIntervalSince(sentAt) >= 3 {
                    unconfirmedNotice(sentAt)
                }

                if showDenyField {
                    denyReasonField
                } else if !typeSize.isAccessibilitySize {
                    subordinateControls
                }
            }
            // The 45% ceiling is applied one level up, on `pinnedFooter`, so
            // that the banner pinned above this bar counts against the same 45%
            // rather than against nothing.
            //
            // Nothing measures this bar's *width* any more — see `buttonRow`.
        }
    }

    private func unconfirmedNotice(_ sentAt: Date) -> some View {
        let elapsed = model.now.timeIntervalSince(sentAt)
        return CCAdaptiveStack(horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xxs) {
            HStack(alignment: .firstTextBaseline, spacing: CC.space.xxs + 1) {
                CCIcon(
                    "exclamationmark.circle.fill", size: 11, weight: .semibold,
                    relativeTo: .caption
                )
                .foregroundStyle(CC.color.warning)
                Text("Still waiting on the Mac — \(Format.age(elapsed)).")
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.color.warning)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if elapsed >= 20 {
                CCButton("Check link", variant: .ghost, size: .sm) {
                    model.connection.retryNow()
                }
            }
            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// `Deny with a reason` and, in the Deck, `Come back to this`. Both are
    /// deliberately quieter than Deny: one is a longer way of saying no and the
    /// other is not an answer at all.
    @ViewBuilder
    private var subordinateControls: some View {
        VStack(spacing: CC.space.sm) {
            if !showDenyField {
                CCButton(
                    "Deny with a reason", icon: "text.bubble", variant: .ghost, size: .md,
                    fullWidth: true,
                    disabledReason: CCDisabledReason(denyBlockedReason)
                ) {
                    withAnimation(CC.motion.small) { showDenyField = true }
                }
            }
            if let comeBackToThis {
                CCButton(
                    "Come back to this", icon: "arrow.uturn.down", variant: .ghost,
                    size: .md, fullWidth: true, haptic: nil
                ) {
                    comeBackToThis()
                }
                .accessibilityIdentifier("deck-later")
                .accessibilityHint(
                    "Moves this decision to the back of the queue. It stays unanswered.")
            }
        }
    }

    /// 40 : 60 — Deny leading, Allow trailing — with a 12pt gap, and at
    /// accessibility sizes a stack, because two 52pt buttons and an AX5 label
    /// cannot share 361pt.
    ///
    /// **The ratio is laid out, not measured.** This was a `GeometryReader` in
    /// the action bar's `.background` writing a `PreferenceKey` into `@State`,
    /// with a `denyWidth` that returned `nil` until the measurement landed "so
    /// the first frame is an even split rather than a zero-width button". Both
    /// buttons still measured exactly **179.00pt = 358/2** four seconds after
    /// launch, at both type sizes, on the card and in the sheet — the signature
    /// of `denyWidth == nil`. The first frame was the only frame, because
    /// `safeAreaInset` renders its content into the hierarchy more than once and
    /// the spare instance reports **zero width**; `ActionBarWidthKey.reduce`
    /// took last-wins, so the phantom zero survived forever. (The height key
    /// beside it reduced with `max` and was therefore correct, which is why the
    /// read gate works and this did not — the same hazard, guarded once.)
    ///
    /// `CCActionPair` asks SwiftUI for the width it is *already* being given and
    /// divides it during layout, so there is no state, no preference and nothing
    /// to fail to propagate.
    private var buttonRow: some View {
        CCActionPair {
            denyButton
        } allow: {
            allowControl
        }
    }

    /// **Deny is always the same control.** It used to become the filled white
    /// primary at HIGH while Allow was drawn in `danger` — an inverted hierarchy
    /// plus a red approve button, on the one screen where approving is the
    /// ordinary thing to do. Both were wrong for the same reason: risk is
    /// already stated by the class badge, the rationale and the hold, and
    /// restating it in the *chrome* spends the two strongest signals the kit has
    /// — a white fill and the colour red — on something that is neither the
    /// primary action nor destructive.
    ///
    /// Red is reserved for destroying something (`Unpair and erase cache`,
    /// `Forget this iPhone's SSH key`) and for reporting a failure.
    @ViewBuilder
    private var denyButton: some View {
        CCButton(
            "Deny",
            variant: .secondary,
            size: .lg,
            fullWidth: true,
            isLoading: inFlight == "deny"
        ) {
            submit(.deny, key: "deny")
        }
        // Disabled without a second copy of the reason: everything that can stop
        // Deny also stops Allow, and Allow is where the sentence lives. Two
        // identical warnings side by side read as two problems.
        .disabled(denyBlockedReason != nil)
        .accessibilityHint(denyBlockedReason ?? "")
        .accessibilityLabel("Deny this \(approval.card.toolName) call")
    }

    @ViewBuilder
    private var allowControl: some View {
        if risk == .high {
            CCHoldButton(
                "Hold to allow",
                // Filled white, like the tap `Allow` it replaces. What makes
                // HIGH different is the 1.2s hold and Face ID, not the colour.
                emphasis: .primary,
                isLoading: inFlight == "allow",
                disabledReason: CCDisabledReason(blockedReason)
            ) {
                submit(.allow, key: "allow")
            }
        } else {
            CCButton(
                "Allow",
                variant: .primary,
                size: .lg,
                fullWidth: true,
                isLoading: inFlight == "allow",
                disabledReason: CCDisabledReason(blockedReason)
            ) {
                submit(.allow, key: "allow")
            }
            // The arming morph: the fill animates in when the gate opens.
            .ccAnimation(CC.motion.medium, value: gate)
            .accessibilityLabel("Allow this \(approval.card.toolName) call")
        }
    }

    private var denyReasonField: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            // Vertical axis so the dictation key has room to fill: denying with
            // a reason is the highest-leverage control in the product and it has
            // to be usable by voice.
            CCField(
                label: "Reason for denying",
                text: $denyReason,
                placeholder: "Tell Claude what to do instead",
                axis: .vertical,
                lineLimit: 1...4)

            CCAdaptiveStack(
                horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xs,
                verticalAlignment: .top
            ) {
                CCButton("Cancel", variant: .ghost, size: .sm) {
                    withAnimation(CC.motion.small) { showDenyField = false }
                }
                Spacer(minLength: CC.space.xs)
                CCButton(
                    "Deny and send",
                    // Denying is the safe direction. It was drawn `destructive`,
                    // which said the opposite in the loudest colour available.
                    variant: .secondary,
                    size: .md,
                    isLoading: inFlight == "deny-reason",
                    // Only a *stated* reason goes here. An empty field is not a
                    // blocked control — the placeholder above already says what
                    // is missing, and drawing a warning under the button made it
                    // wrap two lines and knocked Cancel off its baseline.
                    disabledReason: CCDisabledReason(denyBlockedReason)
                ) {
                    submitDenyWithReason()
                }
                .disabled(denyReason.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }

            Text("Denies with Escape, then types your reason into the session.")
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// Every reason an action cannot be taken, in the order that matters.
    private var blockedReason: String? {
        if let shared = sharedBlockedReason { return shared }
        return gate.blockedReason(at: risk)
    }

    /// Denying skips the read-it-first gate. Refusing is the safe direction, and
    /// a product that promises silence never decides anything cannot put a
    /// scroll requirement between you and "no".
    private var denyBlockedReason: String? { sharedBlockedReason }

    private var sharedBlockedReason: String? {
        // A second tap while the first is in flight would be a silent no-op;
        // saying "waiting" is the honest version of the same refusal.
        if inFlight != nil { return "Waiting for the daemon to confirm…" }
        if let reason = model.linkHealth.disabledReason { return reason }
        if let summary = model.summary(for: approval.sessionKey) {
            let badge = FleetStatusRule.capability(
                summary: summary, capabilities: model.connection.capabilities)
            if let reason = badge.reason { return reason }
        }
        if !verification.hashMatchesDisplayText {
            return "The text on this card does not match its hash. It cannot be answered safely."
        }
        return nil
    }

    // MARK: Submission

    /// HIGH-risk *approvals* need a second factor. Denials never do.
    private func requiresBiometrics(for decision: AnswerDecision) -> Bool {
        guard risk == .high else { return false }
        if case .deny = decision { return false }
        return true
    }

    private func submit(_ decision: AnswerDecision, key: String) {
        guard inFlight == nil else { return }
        spinningControl = key
        sentAt = Date()
        Task {
            if requiresBiometrics(for: decision) {
                let outcome = await BiometricGate.confirm(
                    reason: BiometricGate.reason(for: approval.card.toolName))
                guard outcome.isAuthenticated else {
                    spinningControl = nil
                    sentAt = nil
                    withAnimation(CC.motion.small) { authNotice = outcome.message }
                    CCHaptic.warning.fire()
                    return
                }
            }
            authNotice = nil
            let result = await model.answer(item: approval, decision: decision)
            spinningControl = nil
            report(result)
            if result.isTerminal { onSettled?() }
        }
    }

    private func submitDenyWithReason() {
        guard inFlight == nil else { return }
        spinningControl = "deny-reason"
        sentAt = Date()
        Task {
            let (denial, compose) = await model.denyWithReason(
                item: approval, reason: denyReason)
            spinningControl = nil
            composeResult = compose
            report(denial)
            if denial.isTerminal { onSettled?() }
        }
    }

    /// The app's global haptic table, and nothing outside it.
    private func report(_ attempt: AnswerAttempt) {
        switch attempt {
        case .applied: CCHaptic.success.fire()
        case .duplicate, .answeredAtKeyboard: CCHaptic.warning.fire()
        case .staleCard, .rejected, .failed: CCHaptic.failure.fire()
        }
    }
}

// MARK: - The read gate

/// The read-before-you-decide gate, as a value.
///
/// **This is a viewport test, and it was not one.** The previous implementation
/// armed on `onAppear` of a 1pt marker nested inside a `LazyVStack` child, on
/// the premise that laziness would make the callback mean "scrolled into view".
/// Laziness operates on a stack's *direct children*; `commandBlock` was the
/// second one, so it was materialised at first layout and the marker inside it
/// fired then. `onAppear` is a lifecycle callback and never a visibility test.
///
/// What that cost, read off the AX5 renders rather than off the code:
///
///  * A MEDIUM `Write` outside the worktree, with thirteen characters of the
///    path on screen and `Allow` drawn as the enabled white primary. At MEDIUM
///    there is no hold and no Face ID, so this gate is the *only* thing between
///    one tap and a write to an arbitrary path.
///  * A HIGH `git push --force origin main` with **zero characters of the
///    command rendered** and `Hold to allow` fully armed: a face authenticating
///    a command the product never showed.
///
/// The rule lives here, apart from the view, so it can be tested without a
/// viewport — and the geometry test lives here too, so it can be tested without
/// a rule.
struct ReadGate: Equatable {
    /// The bottom edge of the block carrying the exact command has been inside
    /// the visible region at least once.
    var hasSeenCommand = false
    /// …and so has the bottom edge of the risk class and its provenance.
    var hasSeenProvenance = false

    /// Both edges are measured in screen coordinates. `visibleBottom` is the
    /// scroll view's own bottom less whatever the pinned action bar covers,
    /// because the bar is drawn *over* the scroll view rather than shortening
    /// it.
    ///
    /// Every way of not knowing answers **no**: an unmeasured edge (`nil`), a
    /// layout that has not happened (`visibleBottom <= 0`), a non-finite value
    /// from a degenerate proposal, or a block that has scrolled off the top.
    /// Only a real measurement can open a gate.
    static func isVisible(bottom: CGFloat?, visibleBottom: CGFloat) -> Bool {
        guard let bottom, bottom.isFinite, visibleBottom.isFinite else { return false }
        guard visibleBottom > 0, bottom > 0 else { return false }
        return bottom <= visibleBottom
    }

    /// Whether this class may be answered yet.
    ///
    /// **The gate applies at every tier**, and the reason is that risk governs
    /// *friction* — tap, hold, Face ID — and never whether the command has to be
    /// visible. LOW used to return `true` unconditionally on the argument that
    /// "a `Read` that costs a scroll is a gate people learn to defeat". The
    /// argument is sound and the implementation did not follow from it: at
    /// reading sizes a LOW command fits above the bar on the first frame, so the
    /// gate is already open by the time the card is drawn and costs the reader
    /// nothing at all. It is only when the command does *not* fit that the two
    /// behaviours differ, and that is precisely the case an AX5 render of a LOW
    /// card measured: `Allow` enabled, in the thumb zone, over **13 of the 24
    /// characters** of `/Users/dev/app/README.md` — the 11 that were missing
    /// being the ones that tell a README from `~/.ssh/id_ed25519`.
    ///
    /// A tier-shaped exception here is not "less friction for a small risk", it
    /// is "the product will let you approve something it never showed you", and
    /// that is not a property any tier gets.
    func isOpen(at risk: RiskClass) -> Bool {
        switch risk {
        case .low:
            return hasSeenCommand
        case .medium:
            return hasSeenCommand
        // Stated separately rather than left to fall out of the layout: at HIGH
        // a face is about to authorise this, and the two facts that say what it
        // authorises are the command and the class it was given. That the one
        // is drawn above the other is not evidence that both were seen.
        case .high:
            return hasSeenCommand && hasSeenProvenance
        }
    }

    /// What a closed gate says. Drawn by `ccDisabled`, never only hinted.
    func blockedReason(at risk: RiskClass) -> String? {
        guard !isOpen(at: risk) else { return nil }
        guard hasSeenCommand else { return "Scroll the command into view before deciding." }
        return "Scroll to the end of the command block before deciding."
    }
}

/// The four numbers the read gate is decided from.
///
/// A class, and held in `@State` rather than as `@State` values, for one
/// measured reason: `commandBottom` changes on every frame of a scroll, and
/// routing that through view state re-ran `body`, which re-created the
/// `GeometryReader`s, which re-emitted the preference. The app never reported
/// itself idle for the duration of a drag, so XCUITest's quiescence wait timed
/// out and the runner restarted mid-suite. Geometry is an input to the
/// decision, not part of the view's identity; only the decision is state.
@MainActor
final class ReadGateProbe {
    /// The bottom edge of the block carrying the exact command, in screen
    /// coordinates.
    ///
    /// **`nil` means "not measured yet", stated as `nil`.** A sentinel —
    /// `.infinity`, `.greatestFiniteMagnitude` — puts a value into the program
    /// that arithmetic, formatters and `Int(_:)` cannot all be trusted with,
    /// and it makes "unmeasured" indistinguishable from "very far down the
    /// document" at every call site.
    var commandBottom: CGFloat?
    /// The bottom edge of the risk class and its provenance, likewise.
    var provenanceBottom: CGFloat?
    /// The bottom of the scroll view's own frame.
    var viewportBottom: CGFloat?
    /// How much of that frame the pinned action bar covers. A *height* rather
    /// than an edge: `safeAreaInset` renders its content into the hierarchy
    /// more than once, and the spare instance reports zero — zero height means
    /// "covers nothing" and is harmless, where a zero *edge* would have meant
    /// "the document ends at the top of the screen" and shut the gate forever.
    ///
    /// Optional for the same reason every edge here is: **"not measured yet"
    /// and "measures zero" are different facts.** This defaulted to `0`, and the
    /// four geometry reports arrive as four separate `onPreferenceChange`
    /// callbacks in whatever order SwiftUI delivers them. A frame with the
    /// viewport reported and the bar not yet reported therefore computed the
    /// readable region as the *whole* scroll view — which extends underneath
    /// the bar — so a command hidden behind it measured as visible. The flags
    /// are deliberately sticky, so that one frame opened the gate for good.
    ///
    /// A real device found it and every simulator run missed it: an iPhone Air
    /// at AX5 showed a MEDIUM `Write` with 13 characters of its path on screen
    /// and `Allow` drawn as the enabled white primary.
    var actionBarHeight: CGFloat?

    /// Where the readable part of the document ends, or `nil` when the layout
    /// has not happened yet — which includes "the bar has not said how much of
    /// the viewport it covers" and "the bar reported a size it cannot really
    /// have".
    ///
    /// **That last clause is the bug a real device found.** Measured on an
    /// iPhone Air at AX5, the bar reported `1.0` on an early layout pass:
    /// `cmd=848.0 vis=852.0 vp=853.0 bar=1.0`. The command sat 4pt inside a
    /// readable region that was wrong by 167pt, the flags are deliberately
    /// sticky, and so that single pass opened the gate for good — leaving
    /// `Allow` enabled over a path whose last line was behind the bar.
    ///
    /// The floor is not a tuned constant: the bar always carries at least one
    /// tappable control, and `CC.size.hitTarget` is this product's rule for the
    /// smallest a tappable thing may be. A bar shorter than one control has not
    /// been laid out yet, whatever number it just reported.
    var visibleBottom: CGFloat? {
        guard let viewportBottom, viewportBottom.isFinite else { return nil }
        guard let actionBarHeight, actionBarHeight.isFinite else { return nil }
        guard actionBarHeight >= CC.size.hitTarget else { return nil }
        return viewportBottom - actionBarHeight
    }
}

/// A bottom edge measured for the read gate.
///
/// `nil` is "nobody measured this", and it is `nil` rather than a sentinel so
/// that no arithmetic anywhere can be handed an infinity it did not expect. Two
/// rules, both of which resolve every ambiguity toward a **shut** gate: with no
/// emitter the value stays `nil`; with more than one it takes the *lowest*
/// edge, which is the last of them a reader would reach.
private protocol ReadGateEdgeKey: PreferenceKey where Value == CGFloat? {}

extension ReadGateEdgeKey {
    static var defaultValue: CGFloat? { nil }
    static func reduce(value: inout CGFloat?, nextValue: () -> CGFloat?) {
        guard let next = nextValue(), next.isFinite else { return }
        value = value.map { max($0, next) } ?? next
    }
}

/// The bottom edge of the block carrying the exact command.
private struct CommandBottomKey: ReadGateEdgeKey {}

/// The bottom edge of the risk class and its provenance.
private struct ProvenanceBottomKey: ReadGateEdgeKey {}

/// The bottom of the scroll view's own frame.
private struct ViewportBottomKey: ReadGateEdgeKey {}

/// How tall the pinned action bar is — a *height*, not an edge.
///
/// `safeAreaInset` renders its content into the hierarchy more than once, and
/// the spare instance reports zero. Zero height is discarded by `max` and
/// simply means "covers nothing"; zero *edge* would have meant "the document
/// ends at the top of the screen" and shut the gate permanently.
/// `nil` is "the bar has not reported", **not** zero. Defaulting to zero let the
/// gate compute a readable region that included everything behind the bar, on
/// any frame where this key had not arrived yet — see `ReadGateProbe`.
private struct ActionBarHeightKey: PreferenceKey {
    static let defaultValue: CGFloat? = nil
    static func reduce(value: inout CGFloat?, nextValue: () -> CGFloat?) {
        guard let next = nextValue(), next.isFinite else { return }
        value = value.map { max($0, next) } ?? next
    }
}

/// One of Claude's own numbered options.
///
/// Split into its own view for one reason: it has to read `isEnabled` from the
/// environment. `ccDisabled` on the card above disables every row inside it, and
/// a row that is dead has to *look* dead — the disabled treatment in this app is
/// a `textDisabled` label, never a dimmed control.
private struct OptionRowLabel: View {
    let index: UInt32
    let label: String
    let isBusy: Bool

    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        HStack(alignment: .top, spacing: CC.space.sm) {
            Text("\(index)")
                .ccType(CC.type.mono)
                .foregroundStyle(isEnabled ? CC.text.primary : CC.text.disabled)
                // A fixed 24pt column, so a two-digit option does not shunt its
                // own label out of line with the one above it.
                .frame(width: CC.space.xl, alignment: .leading)
            Text(label)
                .ccType(CC.type.callout)
                .foregroundStyle(isEnabled ? CC.text.primary : CC.text.disabled)
                .lineLimit(2)
                .multilineTextAlignment(.leading)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: CC.space.xs)
            if isBusy { CCProgressRing(.sm) }
        }
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, CC.space.sm)
        .frame(minHeight: CC.size.controlLg)
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
    }
}

// `ActionBarWidthKey` used to live here — a last-wins `PreferenceKey` emitted
// from a `safeAreaInset` subtree, which is a subtree SwiftUI renders more than
// once, and the spare instance reports zero. It is gone: `CCActionPair` divides
// the width it is handed during layout, so there is no measurement to lose.
