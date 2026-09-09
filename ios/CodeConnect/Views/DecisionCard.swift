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
/// cache` and to failures. A reader who learns
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
    /// What a tap in the sample fleet would have done. See `submit`.
    @State private var sampleNotice: String?
    /// When the answer left, so an unconfirmed one can say how long it has been
    /// unconfirmed rather than spinning forever.
    @State private var sentAt: Date?
    /// How wide the inline patch may be drawn, once a layout pass has said.
    /// `nil` until then, and the fallback below it is a reading-size iPhone —
    /// one frame at a slightly wrong wrap column, never a clipped one.
    @State private var diffWidth: CGFloat?
    /// Whether the reader has asked for the files beyond the fold. Sticky: a
    /// card that has been opened out stays open out.
    @State private var showAllChanges = false
    @Environment(\.dynamicTypeSize) private var typeSize

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

    /// **How this card may be answered at all** — one decision, read by the
    /// action bar, the option rows and `submit`, so no answer surface can be
    /// offered that another withholds.
    ///
    /// The two agents do not share an answer vocabulary and the Mac enforces it
    /// in both directions: a Codex card answered with `allow`/`deny`/`option` is
    /// refused by name — *"a Codex card is answered by naming one of the options
    /// it offered; this decision names none of them, so nothing was sent"* — and
    /// an `option_id` aimed at a Claude session is refused just as plainly. So
    /// this is a fork, not an addition.
    enum AnswerSurface: Sendable, Hashable {
        /// Claude's card: Allow, Deny, and the pane's numbered options when they
        /// offer something the bar cannot already say.
        case allowDeny(options: [PaneOptions.Option])
        /// A Codex card: **the option table and nothing else.** Every count,
        /// including two — the `> 2` rule that suppresses Claude's redundant
        /// yes/no pair would leave a two-option Codex card with Allow and Deny,
        /// which are the exact two decisions it refuses, and the card would be
        /// unanswerable.
        case codexOptions([CodexCard.Option])
        /// **No answer this phone can honestly offer.** A Codex card whose
        /// options are missing or malformed, or a card on an agent this build
        /// does not know how to drive. The card is still readable; it is simply
        /// not answerable from here, and it says so rather than offering a
        /// vocabulary the Mac would refuse.
        case noneAnswerable
    }

    /// Decided from the **session's agent**, which is the daemon's own word for
    /// what is running — never from `tool_input`, which is content the agent
    /// itself authored.
    ///
    /// It was the card's shape, and that was a real hole: `tool_input` is
    /// arbitrary tool content, so a Claude tool that happened to emit an
    /// `options[]` table lost Allow and Deny and transmitted an `option_id` the
    /// Mac refuses by name — and a malformed Codex card with no usable options
    /// fell back to Allow/Deny, which Codex refuses. The discriminator has to be
    /// something the agent cannot write.
    ///
    /// For `.codex`, empty or malformed options make the card **non-actionable**
    /// rather than falling through: a Codex card is answered by naming one of
    /// its own options, and if there are none to name there is no honest answer
    /// this phone can offer.
    /// **A missing agent is not Claude.**
    ///
    /// `agent` is nil when the run has no live summary — it left the fleet, the
    /// daemon has not sent one yet, or the card outlived its session. That used
    /// to default to `.claude`, which is the one answer that can transmit: an
    /// `allow` for a run whose agent nobody knows. Unknown fails closed.
    static func answerSurface(
        card: ApprovalCard, agent: AgentKind?, paneSnapshot: String?,
        resolvesCodexCards: Bool = true
    ) -> AnswerSurface {
        guard let agent else { return .noneAnswerable }
        switch agent {
        case .codex:
            // **F4.** Below minor 19 a Codex resolution carries no `request_id`,
            // so an answered card can never be retired — it would stay live and
            // tappable for ever. A card that cannot be retired must not be
            // answerable.
            guard resolvesCodexCards else { return .noneAnswerable }
            // **And a card whose rendering cannot be vouched for.** The options
            // are read out of `tool_input`; if the structured fields do not
            // reproduce the hashed `display_text`, the table on screen is not
            // provably the table the daemon sent, and answering by index into it
            // is answering a question nobody can check.
            guard card.verification.renderMatchesDisplayText else { return .noneAnswerable }
            let options = CodexCard.options(in: card.toolInput)
            return options.isEmpty ? .noneAnswerable : .codexOptions(options)
        case .claude:
            let pane = paneSnapshot.map(PaneOptions.parse) ?? []
            // The count test, unchanged. For Claude's ordinary two-item prompt
            // the rows *are* Allow and Deny drawn a second time in a second
            // style, and four buttons for two outcomes makes the reader work out
            // which pair is which before deciding anything.
            return .allowDeny(options: pane.count > 2 ? pane : [])
        case .unsupported:
            // An agent this build cannot drive gets no answer surface at all.
            // Guessing a vocabulary for it would be guessing on the wire.
            return .noneAnswerable
        }
    }

    /// **Which agent this card belongs to, or nil when nobody knows.**
    ///
    /// The single source for every branch below. It used to be re-derived per
    /// site from `codexOptions.isEmpty` — presentation state standing in for
    /// identity — and that is what put `Deny with a reason` back on a read-only
    /// Codex card at accessibility sizes and printed "If you deny, Claude…"
    /// over a Codex refusal.
    private var sessionAgent: AgentKind? {
        model.summary(for: approval.sessionKey)?.agent
    }

    private var isCodexCard: Bool { sessionAgent == .codex }
    private var isClaudeCard: Bool { sessionAgent == .claude }

    private var answerSurface: AnswerSurface {
        Self.answerSurface(
            card: approval.card,
            agent: sessionAgent,
            paneSnapshot: approval.paneSnapshot,
            resolvesCodexCards: model.daemonProfile.resolvesCodexCards)
    }

    /// The Codex option table, or empty for a Claude card.
    private var codexOptions: [CodexCard.Option] {
        if case .codexOptions(let options) = answerSurface { return options }
        return []
    }

    /// The numbered options, but only when they offer something the action bar
    /// cannot already say.
    ///
    /// The rows are real controls — each sends `.option(index:)` — so this is not a
    /// caption being tidied away. It is that for Claude's ordinary two-item prompt
    /// the rows *are* Allow and Deny, drawn a second time in a second style, and a
    /// card that offers four buttons for two outcomes makes the reader work out
    /// which pair is which before deciding anything.
    ///
    /// A longer menu is the opposite case. `Yes, and don't ask again` is a third
    /// outcome with consequences beyond this card, and Allow cannot express it, so
    /// there the list is the only way to choose it and it stays.
    ///
    /// The test is the count, not the wording. Matching on the words `yes` and `no`
    /// would make this depend on Claude's phrasing in a language this app does not
    /// control, and getting that wrong hides a real choice.
    private var distinctOptions: [PaneOptions.Option] {
        if case .allowDeny(let options) = answerSurface { return options }
        return []
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
            VStack(alignment: .leading, spacing: CC.rhythm.sections) {
                header
                commandBlock
                // Between the command and the consequence of refusing it: a
                // Codex `fileChange` card ships its whole patch inline, and
                // *what would be written* is part of the question, not a
                // disclosure underneath the answer.
                if !changes.isEmpty || changesOmitted != nil { fileChangeBlock }
                consequenceBlock
                // Codex's own options are the ONLY way to answer its card, so
                // they sit above the disclosures rather than below them, and
                // they render at any count.
                if !codexOptions.isEmpty, isActionable { codexOptionsBlock }
                whatWasChosenBlock
                // Gated on `isActionable`: the option rows call `submit`, so on a
                // resolved/unbacked card they would be a second answer surface
                // the action-bar gate does not cover. Hidden when the card cannot
                // be acted on, exactly like the bar.
                if !distinctOptions.isEmpty, isActionable { exactOptions }
                disclosures
                verificationLine
                // **The read-only note leaves the pinned bar at accessibility
                // sizes**, for the same measured reason the subordinate controls
                // do — and it is the same trap, one surface along.
                //
                // A pinned footer is capped at 45% of the screen
                // (`ccScrollCap`), because a bar that owns the screen leaves the
                // command unreadable. That cap is right for a bar of controls,
                // which must stay reachable however far the reader scrolls. This
                // footer has no controls: it is one sentence saying there is
                // nothing to tap. Pinned, it bought nothing and cost twice —
                // measured at AX5 on a 6.9" phone, it covered the EXACT COMMAND
                // block and still clipped its own last word off the bottom of
                // the screen.
                //
                // In the document it scrolls with the card it is about, so it
                // can neither cover the command nor be cut off. Below
                // accessibility sizes the bar fits comfortably and keeps its
                // place, so the card's geometry — and the read gate that
                // measures against it — are unchanged there.
                if Self.readOnlyNoteInDocument(
                    isAccessibilitySize: typeSize.isAccessibilitySize,
                    isActionable: isActionable, isAnswerable: isAnswerable)
                {
                    unanswerableNote
                        .padding(.top, CC.rhythm.textSurface)
                }
                // The status banner is **not** here any more; it is pinned above
                // the action bar. See `pinnedFooter`.
                //
                // At accessibility sizes the two subordinate controls leave the
                // pinned bar and land here, in the card's own footer — which is
                // where `Come back to this` belongs anyway. Measured at AX5:
                // four stacked controls made the bar 440pt tall and left the
                // document a four-line sliver, so the command you are being
                // asked to approve could not be read at all.
                // Gated on `isActionable` exactly like the action bar (which
                // carries `subordinateControls` at non-accessibility sizes): at
                // accessibility sizes this is the ONLY place the "Deny with a
                // reason" affordance lives, so without the gate an unbacked /
                // `.unavailable` card would still expose a working deny path at
                // AX sizes — the same answer surface the bar withholds.
                // Not on a Codex card: at accessibility sizes this is the only
                // home of `Deny with a reason`, which types a denial into
                // Claude's composer — machinery a Codex session does not have.
                // Its own `Come back to this` rides in the bar above instead, at
                // every size, because that one is a queue control and not an
                // answer.
                if isClaudeCard,
                    Self.subordinateControlsShown(
                        isAccessibilitySize: typeSize.isAccessibilitySize,
                        isActionable: isActionable)
                {
                    subordinateControls
                }
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
        .onPreferenceChange(DiffWidthKey.self) { value in
            // Guarded: assigning an equal width would re-run `body`, which
            // re-creates the reader, which re-emits the preference.
            if value > 0, diffWidth != value { diffWidth = value }
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
                Text(verbatim: label.project)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.secondary)
                    // **Bounded, because the decision comes first.** A project
                    // may be forty characters, and at the largest accessibility
                    // sizes that is four lines of provenance standing between
                    // the reader and the command they came to answer. The full
                    // path is on the line below, so nothing here is the only
                    // copy of anything.
                    .lineLimit(2)
                    .truncationMode(.tail)
                if let qualifier = label.qualifier {
                    Text("·")
                        .ccType(CC.type.monoSmall)
                        .foregroundStyle(CC.text.disabled)
                    Text(verbatim: qualifier)
                        .ccType(CC.type.monoSmall)
                        .foregroundStyle(CC.text.disabled)
                }
                Spacer(minLength: 0)
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(label.spoken)
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

            // The promise, stated where the decision is made. Verbatim, and it
            // survives every redesign — but only on a card there is a decision
            // to make on. `ApprovalCard.pendingPromise` holds that condition.
            //
            // The running clock that used to sit above it is gone. It answered
            // a question this screen does not ask: how long a card has waited
            // cannot make a command safer or more dangerous, so the only thing
            // a ticking number adds at the moment of deciding is pressure — on
            // a card whose next line promises there is no timer. Age is real
            // triage information and it still exists, on Fleet and on the deck,
            // where choosing *which* card to open is the actual question.
            if approval.isPending, let promise = ApprovalCard.pendingPromise(isAnswerable: isAnswerable) {
                Text(promise)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.tertiary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.top, CC.rhythm.textSurface)
            }
        }
    }

    /// What to call the run this decision belongs to — see `RunLabel`.
    private var label: RunLabel { model.runLabel(for: approval.sessionKey) }

    // MARK: Command

    private var commandBlock: some View {
        // Three relationships, so three spacings — one stack could only ever get
        // two of them wrong. Label and block are text meeting a surface (12); the
        // block and the prose beneath it likewise (12); the prose lines are one
        // thought and sit on text rhythm (8). Written as a single 8pt stack this
        // read as a command jammed against its own explanation.
        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            CCSectionHeader(commandHeader)
            CCMonoBlock(command)
                // The gate's first half, measured on the block that carries the
                // command itself rather than on a marker somewhere beneath it.
                .background { bottomProbe(CommandBottomKey.self) }

            VStack(alignment: .leading, spacing: CC.rhythm.text) {
                if let intent = describedIntent {
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
                    .foregroundStyle(
                        risk.ccTone == .neutral ? CC.text.secondary : risk.ccTone.color
                    )
                    .fixedSize(horizontal: false, vertical: true)
                Text(assessment.provenance)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
                    .fixedSize(horizontal: false, vertical: true)
                    // The gate's second half, and the one HIGH names explicitly.
                    .background { bottomProbe(ProvenanceBottomKey.self) }
            }
        }
    }

    private var command: String {
        approval.card.primaryText(verification: verification, agent: sessionAgent)
    }

    /// What the block beneath the header actually holds.
    ///
    /// A Codex `file change` card's principal argument is a **path**, and this
    /// header read `EXACT COMMAND` over it — a noun for a thing that is not
    /// there, on the card whose whole job is to say precisely what will happen.
    /// Caught by looking at `codex-card-filechange-wide--L.png`.
    ///
    /// The block itself stays, even though the patch below repeats the path: it
    /// is what the read gate measures (`CommandBottomKey`), and a gate that
    /// measured a block that sometimes does not exist is a gate that sometimes
    /// does not exist.
    private var commandHeader: String {
        !changes.isEmpty || changesOmitted != nil ? "First file changed" : "Exact command"
    }

    /// The tool's own description, unless it is the command again in sentence case.
    ///
    /// `ToolSummary.intent` hands back whatever the tool supplied, and for a shell
    /// command that is frequently the command itself — `echo SECONDCARD` printed
    /// under `echo SECONDCARD`, one line apart. A restatement is not a second
    /// source; it is the same source taking up the space where a reason should be,
    /// and on a card whose whole job is *decide this* that is worse than blank.
    ///
    /// Compared on letters and digits alone, so punctuation, case and the sentence
    /// capitalisation tools add cannot smuggle a duplicate past. Anything that adds
    /// a goal, a destination or a consequence survives, because it will not reduce
    /// to the same string.
    private var describedIntent: String? {
        guard
            let intent = ToolSummary.intent(
                tool: approval.card.toolName, input: approval.card.toolInput)
        else { return nil }
        return Self.saysSomethingNew(intent, beyond: command) ? intent : nil
    }

    /// `nonisolated` because it reads nothing: two strings in, a Bool out. Without
    /// it the compiler inherits the view's `@MainActor` and every caller from a
    /// synchronous test is a concurrency warning, which is how eight of them
    /// accumulated unnoticed behind incremental builds.
    nonisolated static func saysSomethingNew(_ intent: String, beyond command: String) -> Bool {
        func core(_ text: String) -> String {
            text.lowercased().filter { $0.isLetter || $0.isNumber }
        }
        return core(intent) != core(command)
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
        // Never on entrance: a card whose command already fits on the first
        // frame arms silently. If a notification brought the reader here it has
        // already buzzed, and a second buzz inside the entrance window below
        // reads as a second event rather than as this one.
        guard !wasOpen, next.isOpen(at: risk), approval.isPending else { return }
        if Date().timeIntervalSince(appearedAt) > Self.entranceWindow { CCHaptic.light.fire() }
    }

    /// How long after the card appears an arming still counts as "it was
    /// already on screen" rather than "you scrolled to it".
    private static let entranceWindow: TimeInterval = 0.6

    // MARK: If you deny

    /// **Whose consequence, in whose words.**
    ///
    /// A Codex card gets the reason *it* supplied and no "if you deny" at all;
    /// a Claude card keeps the block it has always had. They are not
    /// interchangeable and running one on the other is not a wording slip: a
    /// Codex card was rendering `Claude receives "denied". What it does next is
    /// up to it.` — the wrong agent, and a decision (`deny`) that a Codex card
    /// does not offer and the Mac refuses by name. Caught by looking at
    /// `codex-card-command-worst--L.png`.
    @ViewBuilder
    private var consequenceBlock: some View {
        if isCodexCard {
            codexReasonBlock
        } else if isClaudeCard {
            ifYouDenyBlock
        }
    }

    /// **Why Codex is asking**, in the words the app-server sent — present on
    /// every measured command card and absent on every measured file-change one.
    ///
    /// Nothing is invented in its place. There is no Codex equivalent of "if you
    /// deny": what refusing does is decided by Codex, the card's own third
    /// option already says it in the daemon's words ("No, and tell Codex what to
    /// do differently"), and a sentence this app made up about an agent it does
    /// not run would be a guess printed as guidance.
    @ViewBuilder
    private var codexReasonBlock: some View {
        if let reason = CodexCard.reason(in: approval.card.toolInput) {
            VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
                CCSectionHeader("Why Codex is asking")
                Text(reason)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    /// Absent from the previous build: the consequence of saying no, stated
    /// beside the consequence of saying yes.
    private var ifYouDenyBlock: some View {
        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
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
        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            CCSectionHeader("Claude's exact options")
            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    ForEach(Array(distinctOptions.enumerated()), id: \.element.id) { index, option in
                        optionRow(option, separator: index < distinctOptions.count - 1)
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

    // MARK: The patch (Codex file-change cards)

    private var changes: [CodexCard.Change] { CodexCard.changes(in: approval.card.toolInput) }
    private var changesOmitted: CodexCard.ChangesOmitted? {
        CodexCard.changesOmitted(in: approval.card.toolInput)
    }

    /// **The patch, inline, on the card.**
    ///
    /// A Codex `fileChange` card ships its whole diff in `tool_input.changes[]`,
    /// bounded at the Mac to 32 files, 16 KiB per diff and 128 KiB across the
    /// card. `get_diff` is the wrong instrument for it: that answers *"what has
    /// this agent changed so far"*, and this asks *"may I write this"*. Sending
    /// somebody to a separate sheet to find out what they are approving is the
    /// same failure as hiding the command.
    ///
    /// Drawn with `CCDiffPrimitives` rather than `DiffView`, so it inherits the
    /// diff sheet's wrapping, folding and AX5 behaviour without its request
    /// machinery — there is nothing to request; the bytes are already here.
    private var fileChangeBlock: some View {
        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            CCSectionHeader(changesHeader)
            // **Bounded.** A valid card carries up to 32 files and 128 KiB of
            // diff, and every row was built eagerly in a non-lazy `VStack` with
            // the diff reparsed during `body` — thousands of `CCDiffRow`s
            // constructed synchronously on the surface whose whole job is to be
            // read before a decision. The first few files are what a reader
            // checks; the rest are behind one tap that says how many.
            ForEach(Array(shownChanges.enumerated()), id: \.element.id) { index, change in
                changeView(change, allowance: diffPlan.allowances[index])
            }
            if diffPlan.heldBack > 0 { heldBackLine(diffPlan.heldBack) }
            if foldedChangeCount > 0 { foldedChangesControl }
            if let changesOmitted { omittedLine(changesOmitted) }
        }
    }

    /// How many files are drawn before the fold. Four is what fits above the
    /// action bar at reading size, which is the number a reader scans before
    /// they start scrolling anyway.
    private static let foldedChangeThreshold = 4

    private var shownChanges: [CodexCard.Change] {
        showAllChanges ? changes : Array(changes.prefix(Self.foldedChangeThreshold))
    }

    private var foldedChangeCount: Int {
        showAllChanges ? 0 : max(0, changes.count - Self.foldedChangeThreshold)
    }

    /// How the card's row budget is spent on the files it is showing.
    private var diffPlan: CodexCard.DiffPlan { CodexCard.diffPlan(for: shownChanges) }

    /// **What is not drawn, said out loud.**
    ///
    /// The alternative is a diff that simply stops, which reads as "that was all
    /// of it" — and on the surface where somebody is about to approve a write,
    /// that is the one misreading this card cannot afford. There is no "show
    /// more": the card is bounded on purpose, and the whole patch is at the Mac.
    private func heldBackLine(_ rows: Int) -> some View {
        Text("\(rows) more line\(rows == 1 ? "" : "s") of this patch are not drawn here.")
            .ccType(CC.type.footnote)
            .foregroundStyle(CC.text.secondary)
            .fixedSize(horizontal: false, vertical: true)
            .accessibilityIdentifier("diff-rows-held-back")
    }

    /// **The read gate is not weakened by the fold.** It measures the *command*
    /// block, which is above this and always drawn; folding files below it
    /// changes what is on screen, never what the gate requires to have been.
    private var foldedChangesControl: some View {
        CCButton(
            "Show \(foldedChangeCount) more file\(foldedChangeCount == 1 ? "" : "s")",
            icon: "chevron.down", variant: .ghost, size: .sm, fullWidth: true
        ) {
            withAnimation(CC.motion.small) { showAllChanges = true }
        }
        .accessibilityIdentifier("show-all-changes")
    }

    /// **Always the whole count**, whatever the fold is showing. The header
    /// answers "how much would this write", and hiding rows must never change
    /// that number.
    private var changesHeader: String {
        let carried = changes.count
        guard let changesOmitted else {
            return carried == 1 ? "1 file" : "\(carried) files"
        }
        return "\(carried) of \(carried + changesOmitted.count) files"
    }

    @ViewBuilder
    private func changeView(_ change: CodexCard.Change, allowance: Int) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: CC.space.xs) {
                Text(change.path)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.primary)
                    // Head truncation: the file name is the end of a path, and
                    // it is the part that identifies the change.
                    .lineLimit(2)
                    .truncationMode(.head)
                Spacer(minLength: CC.space.xs)
                Text(change.kind)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
            }
            .padding(.bottom, CC.space.xxs)

            if let movePath = change.movePath {
                Text("moves to \(movePath)")
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
                    .lineLimit(2)
                    .truncationMode(.head)
                    .padding(.bottom, CC.space.xxs)
            }

            if let file = CodexCard.parsedDiff(for: change) {
                // **A file the budget never reached keeps its name and loses its
                // body.** Not "no diff was sent" — one was; the card simply
                // stopped drawing, and the line below the list says how much.
                if allowance > 0 { diffRows(file, allowance: allowance) }
            } else {
                // A change with no diff is a real shape — a deletion, or a file
                // whose contents the app-server did not send. Saying so is the
                // honest render; a blank space would read as "no changes".
                Text("No diff was sent for this file.")
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.tertiary)
            }
        }
    }

    private func diffRows(_ file: UnifiedDiff.FileDiff, allowance: Int) -> some View {
        // **Measured, and on the kit's own Dynamic Type rules.**
        //
        // `CCDiffMetrics` derives its wrap column count from the width it is
        // given, so a hard-coded number is the diff sheet's oldest bug in a new
        // place: wrong at AX5, it breaks a path in the middle of a filename.
        // `gutter(for:)` drops the line-number column entirely at accessibility
        // sizes — four advances is a quarter of the visible line there — and
        // `typeScale` stops growing the glyphs at `xxLarge`, because a 24pt line
        // of code at AX5 would be four characters wide.
        //
        // The width is measured into `@State` rather than read inside a
        // `GeometryReader` wrapping the rows: a `GeometryReader` takes the whole
        // height it is proposed, and these rows self-size — a wrapped line is
        // two visual lines — so wrapping them in one would either clip the last
        // line or reserve a screen of blank space. Width is stable (it changes
        // on rotation, not on scroll), so unlike the read gate's edges it is
        // safe to route through view state.
        let metrics = CCDiffMetrics(
            fontSize: 12 * CCDiffMetrics.typeScale(for: typeSize),
            availableWidth: diffWidth ?? 320,
            gutter: CCDiffMetrics.gutter(for: typeSize))
        return CCCard(padding: 0) {
            VStack(alignment: .leading, spacing: 0) {
                // **The budget, spent.** Flattened first so the cap is a count
                // of rows the reader sees, not of hunks — a single hunk at the
                // ceiling is longer than the whole allowance.
                ForEach(file.hunks.flatMap(\.lines).prefix(allowance)) { line in
                    CCDiffRow(line: line, metrics: metrics)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .background {
                GeometryReader { proxy in
                    Color.clear.preference(key: DiffWidthKey.self, value: proxy.size.width)
                }
            }
        }
    }

    /// **The one card state no capture proves** (contract §4.6): the patch
    /// exceeded 32 files and the Mac sent a count and a digest instead of the
    /// rest. Rendered as exactly that — never as a list of paths the phone does
    /// not have, and never elided, because approving a patch whose size you were
    /// not told is the failure this whole card exists to prevent.
    private func omittedLine(_ omitted: CodexCard.ChangesOmitted) -> some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            Text(
                "\(omitted.count) more file\(omitted.count == 1 ? "" : "s") "
                    + "would change and are not shown here."
            )
            .ccType(CC.type.footnote)
            .foregroundStyle(CC.color.warning)
            .fixedSize(horizontal: false, vertical: true)
            Text("Approving this approves those too. Review them at the Mac.")
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    // MARK: Codex's options

    /// **The only way a Codex card can be answered.**
    ///
    /// Not "Claude's exact options" drawn again: those rows send
    /// `.option(index:)`, which a Codex session refuses by name, and they are
    /// suppressed below three because for Claude the first two *are* Allow and
    /// Deny. Neither applies here. A Codex card offers two options whenever its
    /// amendment argv contains a line break — the daemon withholds the label
    /// rather than shorten it — and those two are still the only answers the
    /// Mac will take.
    private var codexOptionsBlock: some View {
        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            CCSectionHeader("Choose one")
            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    ForEach(Array(codexOptions.enumerated()), id: \.element.id) { index, option in
                        codexOptionRow(
                            option, number: index + 1, separator: index < codexOptions.count - 1)
                    }
                }
            }
            .ccDisabled(CCDisabledReason(blockedReason))
        }
    }

    private func codexOptionRow(
        _ option: CodexCard.Option, number: Int, separator: Bool
    ) -> some View {
        Button {
            // The id **verbatim**. The daemon validates it against its own
            // stored option table and rebuilds the wire decision from that
            // option's own payload, so a synthesised or shortened id is not a
            // near miss — it is refused, and the reader's tap does nothing.
            submit(.optionId(option.id), key: "option-\(option.id)")
        } label: {
            OptionRowLabel(
                index: UInt32(number), label: option.label,
                isBusy: inFlight == "option-\(option.id)",
                // **No two-line clamp on a Codex option.**
                //
                // Claude's numbered options are short restatements of Allow and
                // Deny, so two lines is generous. A Codex amendment label is the
                // whole consequence of the choice — *"don't ask again for
                // commands that start with `touch '/tmp/cc-label-d.… spaced.txt'`"*
                // — and the daemon builds it from the exact argv it would
                // whitelist. Clamped at two it read `…'/tmp/cc-label-d.…`, which
                // hides precisely which commands the reader would be
                // whitelisting for ever. Measured at 110 characters, four lines
                // at reading size, and it must be all four.
                lineLimit: nil)
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
        .accessibilityLabel("Option \(number): \(option.label)")
    }

    // MARK: Disclosures

    @ViewBuilder
    private var disclosures: some View {
        VStack(alignment: .leading, spacing: 0) {
            // **D9: a Codex card shows no wire on a product screen.**
            //
            // This drew `toolInput.prettyJSONString` plus the `request_id` and
            // `payload_hash` lines for every card — so every Codex card had raw
            // JSON one tap away, and the 159-character composite id whose whole
            // documented purpose is that nothing renders it was rendered.
            //
            // Claude's disclosure is untouched and out of scope: its
            // `tool_input` is the tool's own arguments, which is what a Claude
            // reader is checking, and its `request_id` is a short opaque token.
            // Claude's, and only on a card known to be Claude's. An unknown
            // agent gets no raw disclosure: the JSON may be Codex's.
            if isClaudeCard {
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
            } else if isCodexCard {
                codexDetails
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

    /// **The same facts, as facts.** What a reader checks on a Codex card is
    /// where the command would run and what it would touch — not the JSON that
    /// carried it. Nothing here is an id or a hash.
    @ViewBuilder
    private var codexDetails: some View {
        let cwd = CodexCard.cwd(in: approval.card.toolInput)
        if cwd != nil || !changes.isEmpty {
            CCDisclosure("Details") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    if let cwd {
                        detailRow("Runs in", cwd)
                    }
                    ForEach(changes) { change in
                        detailRow(change.kind.capitalized, change.path)
                    }
                    if let omitted = changesOmitted {
                        detailRow(
                            "Not shown",
                            "\(omitted.count) more file\(omitted.count == 1 ? "" : "s")")
                    }
                }
            }
        }
    }

    /// What the reader actually chose, once a Codex card has ended — a
    /// sentence, never the wire's option table replayed.
    ///
    /// Prose rather than a status block, on purpose (D9): `status answered / by
    /// local / decision absent` is the *evidence* for this sentence, and it
    /// belongs in a capture, not on the screen of somebody who wants to know
    /// whether their file got written.
    @ViewBuilder
    private var whatWasChosenBlock: some View {
        if let resolution = effectiveCodexResolution {
            VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
                CCSectionHeader("What was chosen")
                Text(CodexProse.whatWasChosen(resolution))
                    .ccType(CC.type.callout)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func detailRow(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(label)
                .ccType(CC.type.micro)
                .foregroundStyle(CC.text.tertiary)
            Text(value)
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.secondary)
                .lineLimit(3)
                .truncationMode(.head)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
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
    /// The authoritative, LIVE card for this identity — the single source every
    /// actionability/outcome decision below derives from. `nil` when nothing in
    /// the fleet still backs it (session departed, log reset/rewound, or the card
    /// left the timeline). The passed `approval` supplies identity and a
    /// last-known display only; it never decides whether the card is actionable.
    private var live: ApprovalItem? {
        model.liveApproval(sessionKey: approval.sessionKey, id: approval.id)
    }

    /// The outcome that governs this card, from **either** the live lookup or the
    /// snapshot the card was opened from. A recorded outcome only ever makes the
    /// card *more* restrictive (non-actionable) — it can never turn a card
    /// actionable — so taking it from either source closes a regression our own
    /// live-only derivation opened: a card opened from a RESOLVED row would go
    /// `.unavailable` after reset and then revert to ACTIONABLE if a behind/replay
    /// re-supplied the same request id as pending. A resolved card must never
    /// revert to actionable, and its resolved/indeterminate banner must stand.
    ///
    /// A genuinely *pending* card whose backing vanished still has `nil` from both
    /// sources, so the `isBacked` gate below renders it `.unavailable` — the
    /// round-5/6/7 behaviour is preserved.
    private var effectiveOutcome: AnswerOutcome? {
        Self.effectiveOutcome(live: live?.outcome, snapshot: approval.outcome)
    }

    /// The governing outcome, from the live lookup OR the opened snapshot. Static
    /// and pure so the regression it closes is testable off the real resolver: a
    /// recorded outcome from either source makes the card non-actionable, so a
    /// resolved card can never revert to actionable when a replay re-supplies its
    /// id as pending. A card pending in both sources is `nil`, and the `isBacked`
    /// gate decides whether it is live-pending or `.unavailable`.
    static func effectiveOutcome(live: AnswerOutcome?, snapshot: AnswerOutcome?) -> AnswerOutcome? {
        live ?? snapshot
    }

    private var statusBanner: StatusBanner? {
        if !verification.hashMatchesDisplayText { return .hashMismatch }
        // **Above the Claude ladder, below the hash gate.** A Codex resolution
        // is authoritative — the daemon watched the question end — and it is the
        // only thing that can describe a Codex ending at all: the Claude ladder
        // below would render `.unavailable` ("the run left the daemon's list or
        // its log was reset"), which is the wrong sentence for a question that
        // was answered at the keyboard thirty seconds ago.
        if let resolution = effectiveCodexResolution { return .codex(resolution) }
        switch Self.resolvedBanner(attempt: attempt, persisted: effectiveOutcome, isBacked: live != nil) {
        case .attempt(let attempt): return .resolution(attempt)
        case .persisted(let outcome): return .alreadyResolved(outcome)
        case .unavailable: return .unavailable
        case .none: break
        }
        if let authNotice { return .authRefused(authNotice) }
        if let sampleNotice { return .sample(sampleNotice) }
        return nil
    }

    /// Which resolved banner a card shows, given this session's own answer
    /// `attempt`, any authoritative outcome the LIVE card carries (`persisted`),
    /// and whether live state still backs the card at all (`isBacked`). Pure and
    /// static so the whole precedence is testable off the real resolver.
    ///
    /// The rules, in order:
    ///   * a **terminal** local attempt (`.applied`, `.indeterminate`,
    ///     `.duplicate`, `.answeredAtKeyboard`) is this session's own observation
    ///     and keeps its richer receipt, even if the backing was later dropped;
    ///   * an authoritative persisted outcome outranks a *stale, non-terminal*
    ///     local attempt — the crash/recovery false-negative, where a dead-socket
    ///     `.failed` would otherwise mask the recovered `approval_resolved`;
    ///   * **no live backing** and no terminal receipt ⇒ `.unavailable`: a
    ///     departed/reset/left-the-log card is neither actionable nor an outcome
    ///     we can claim, so it must never fall back to a frozen actionable
    ///     snapshot or a stale local failure the user could retry;
    ///   * a live pending card with a non-terminal attempt still shows that
    ///     attempt (a genuine failure the user may retry); with none, `.none`
    ///     (the ordinary pending path, action bar available).
    enum ResolvedBanner {
        case attempt(AnswerAttempt)
        case persisted(AnswerOutcome)
        /// No live state backs this card, and this session holds no terminal
        /// receipt for it. Non-actionable by construction.
        case unavailable
        case none
    }

    static func resolvedBanner(
        attempt: AnswerAttempt?, persisted: AnswerOutcome?, isBacked: Bool
    ) -> ResolvedBanner {
        if let attempt {
            if attempt.isTerminal { return .attempt(attempt) }
            if let persisted { return .persisted(persisted) }
            if !isBacked { return .unavailable }
            return .attempt(attempt)
        }
        if let persisted { return .persisted(persisted) }
        if !isBacked { return .unavailable }
        return .none
    }

    /// The card's one banner slot, by case.
    private enum StatusBanner {
        case hashMismatch
        case resolution(AnswerAttempt)
        case alreadyResolved(AnswerOutcome)
        /// What became of a **Codex** question, in Codex's own vocabulary.
        case codex(CodexResolution)
        /// No live state backs this card — a decision that is no longer available.
        case unavailable
        /// Why the biometric check did not pass, in the gate's own words.
        case authRefused(String)
        /// What this tap would have done, had there been a Mac to send it to.
        case sample(String)
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
                Self.alreadyResolvedTitle(for: outcome),
                message: Self.alreadyResolvedMessage(for: outcome, now: model.now),
                tone: outcome.indeterminate ? .warning : .info,
                icon: Self.alreadyResolvedIcon(for: outcome))
        case .codex(let resolution):
            let banner = CodexProse.resolution(resolution)
            CCBanner(
                banner.title, message: banner.message, tone: banner.tone.ccTone,
                icon: banner.icon)
        case .unavailable:
            // No live state backs this card: the session left the fleet, the log
            // was reset, or the card left the timeline. Non-actionable, and it
            // says so rather than presenting a frozen card the reader could act on.
            CCBanner(
                "No longer available",
                message:
                    "This decision can't be acted on here — the run left the daemon's list or its log was reset.",
                tone: .warning, icon: "questionmark.circle")
        case .authRefused(let notice):
            // The gate's own words in the message, never a paraphrase, and never
            // swallowed: a biometric check that failed silently is
            // indistinguishable from a tap that did nothing.
            CCBanner("Face ID", message: notice, tone: .warning, icon: "faceid")
        case .sample(let notice):
            // The fleet banner's own title and glyph: this is the same fact,
            // arriving where the reader is looking.
            CCBanner("Sample fleet", message: notice, tone: .info, icon: "eye")
        }
    }

    /// The already-resolved banner's copy, extracted so the honesty rule is
    /// unit-testable without rendering the card: a **recorded** outcome carrying
    /// `indeterminate: true` — the shape the daemon replays for a
    /// locally-resolved, never-confirmed answer — is "Unconfirmed", never
    /// "Already resolved", and never wears the confirming seal.
    static func alreadyResolvedTitle(for outcome: AnswerOutcome) -> String {
        outcome.indeterminate ? "Unconfirmed" : "Already resolved"
    }

    static func alreadyResolvedMessage(for outcome: AnswerOutcome, now: Date) -> String {
        let who = outcome.resolvedBy == .phone ? "from this app" : "at the keyboard"
        let age = "\(Format.age(since: outcome.resolvedDate, now: now)) ago"
        if outcome.indeterminate {
            return
                "\(outcome.decisionLabel) \(who) · \(age), but the daemon couldn’t confirm it reached the agent"
        }
        return "\(outcome.decisionLabel) \(who) · \(age)"
    }

    /// Never a checkmark for an unconfirmed outcome — the seal *is* the visual
    /// "confirmed" claim this rule exists to prevent.
    static func alreadyResolvedIcon(for outcome: AnswerOutcome) -> String {
        outcome.indeterminate ? "questionmark.circle" : "checkmark.seal"
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

    /// Whether the answer controls should be offered. Requires **live backing**
    /// (`isBacked`) and withdraws the moment the card carries an authoritative
    /// outcome or the local attempt is terminal. Static and pure so the whole
    /// guard is testable off the real resolver: a card with no live state behind
    /// it can never present an action bar, closing the "act on a departed/reset
    /// card" half of the class, and a resolved card disables it even while a
    /// stale non-terminal `.failed` is still stored locally.
    /// `codex` is the second authoritative ending, and it is checked here rather
    /// than mapped onto `outcome`: a Codex resolution says things `AnswerOutcome`
    /// cannot express — cleared because the turn was aborted, answered at the Mac
    /// with no decision recorded, retired because the item finished — and every
    /// one of them means the same thing about this bar, which is that it must go.
    static func actionBarAvailable(
        outcome: AnswerOutcome?, codex: CodexResolution?, attempt: AnswerAttempt?, isBacked: Bool
    ) -> Bool {
        isBacked && outcome == nil && codex == nil && attempt?.isTerminal != true
    }

    /// The single actionability verdict for this card — the one computed the
    /// action bar, the in-body option rows and `submit` all read, so no answer
    /// surface can be offered (or acted on) that the others withhold. False the
    /// moment the card carries an outcome, the local attempt is terminal, or —
    /// the class this closes — no live state backs it at all.
    private var isActionable: Bool {
        Self.actionBarAvailable(
            outcome: effectiveOutcome, codex: effectiveCodexResolution, attempt: attempt,
            isBacked: live != nil)
    }

    /// The Codex ending that governs this card, from the live lookup **or** the
    /// snapshot it was opened from — the same precedence `effectiveOutcome`
    /// uses, and for the same reason: a recorded ending can only ever make the
    /// card less actionable, so taking it from either source cannot make a
    /// resolved card revert to actionable when a replay re-supplies its id.
    private var effectiveCodexResolution: CodexResolution? {
        live?.codexResolution ?? approval.codexResolution
    }

    /// Whether this card offers any answer at all. A `noneAnswerable` surface
    /// is readable and not answerable — and it says which, rather than drawing a
    /// control the Mac would refuse.
    private var isAnswerable: Bool {
        if case .noneAnswerable = answerSurface { return false }
        return true
    }

    /// **Where the "nothing to tap here" sentence is drawn.** In the pinned bar
    /// at reading sizes, in the card's own document at accessibility sizes —
    /// never both, and never nowhere. Static and pure so the choice can be
    /// asserted without a `View`, exactly like `subordinateControlsShown`.
    static func readOnlyNoteInDocument(
        isAccessibilitySize: Bool, isActionable: Bool, isAnswerable: Bool
    ) -> Bool {
        isAccessibilitySize && isActionable && !isAnswerable
    }

    static func readOnlyNoteInPinnedBar(
        isAccessibilitySize: Bool, isActionable: Bool, isAnswerable: Bool
    ) -> Bool {
        !isAccessibilitySize && isActionable && !isAnswerable
    }

    /// The sentence itself, drawn the same way in either home.
    private var unanswerableNote: some View {
        Text(unanswerableReason)
            .ccType(CC.type.footnote)
            .foregroundStyle(CC.text.secondary)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder
    private var actionBar: some View {
        if Self.readOnlyNoteInPinnedBar(
            isAccessibilitySize: typeSize.isAccessibilitySize,
            isActionable: isActionable, isAnswerable: isAnswerable)
        {
            CCActionBar { unanswerableNote }
        } else if isActionable, !isAnswerable {
            // Accessibility sizes: the sentence is in the document above, and
            // the bar draws nothing rather than an empty chrome strip.
            EmptyView()
        } else if isActionable, isCodexCard {
            // **A Codex card gets no Allow and no Deny**, at any option count.
            // Both are refused by name at the Mac, so a bar offering them would
            // be two controls whose only behaviour is a refusal — and the option
            // rows above are the whole answer surface. The bar keeps its place
            // so the card's geometry, and the read gate that measures against
            // it, are unchanged.
            CCActionBar {
                Text(codexBarNote)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                if let sentAt, inFlight != nil, model.now.timeIntervalSince(sentAt) >= 3 {
                    unconfirmedNotice(sentAt)
                }
                // **`Come back to this` survives.** It is not an answer — it is
                // the Deck's queue control — so it belongs on a Codex card
                // exactly as much as on a Claude one, and losing it would leave
                // the Deck with no way past a card the reader is not ready for.
                // `Deny with a reason` does not survive, and must not: denying
                // is `send_text` into Claude's composer, which a Codex session
                // has none of, and the card's own third option is the daemon's
                // way of saying no with a reason.
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
        } else if isActionable {
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

    /// What the Codex bar says instead of Allow and Deny.
    ///
    /// **Short at accessibility sizes, and it never says "above".** It read
    /// *"Answer by choosing one of Codex's options above."* and cost three
    /// lines at AX5 — a quarter of the screen, drawn over the exact command,
    /// which is the same failure the subordinate controls were relocated out of
    /// this bar to avoid. And "above" was false as printed: at AX5 the option
    /// rows are far below the fold, so the one word telling the reader where to
    /// look pointed the wrong way. Measured on
    /// `codex-card-command-worst--ax5.png`.
    /// Why there is nothing to tap. Named for the reader, not for the wire.
    private var unanswerableReason: String {
        // Unknown agent: the run is gone or has not been described yet, and
        // there is no vocabulary to offer. Never Claude's sentence by default.
        guard let agent = sessionAgent else {
            return "This run is no longer on the fleet, so this card cannot be answered from here."
        }
        switch agent {
        case .codex:
            if let caveat = model.daemonProfile.codexAnswerCaveat { return caveat }
            guard verification.renderMatchesDisplayText else {
                return ApprovalCard.unverifiableCodexLine
            }
            return "This card arrived without the options Codex answers by, so it can only be "
                + "answered at the Mac."
        case .unsupported(let raw):
            return "This app does not know how to answer a \(raw) session. Answer it at the Mac."
        case .claude:
            return "This card cannot be answered from here."
        }
    }

    private var codexBarNote: String {
        typeSize.isAccessibilitySize
            ? "Choose one of Codex's options."
            : "Answer by choosing one of Codex's options."
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
                Text("Still waiting on the Mac - \(Format.age(elapsed)).")
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

    /// Whether the accessibility-size relocation of `subordinateControls` (the
    /// "Deny with a reason" affordance) is shown. It appears only at accessibility
    /// sizes AND only when the card `isActionable` — so an unbacked / `.unavailable`
    /// card exposes no answer/deny affordance at ANY text size, consistently with
    /// the action bar. Static and pure so the gate is testable off the real
    /// `isActionable` the view computes, matching every other surface's methodology.
    static func subordinateControlsShown(isAccessibilitySize: Bool, isActionable: Bool) -> Bool {
        isAccessibilitySize && isActionable
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
    /// Red is reserved for destroying something (`Unpair and erase cache`) and
    /// for reporting a failure.
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
        if let reason = model.actionsBlockedReason { return reason }
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

    /// What a decision would have been called on the wire, in the words the
    /// control the reader just pressed uses.
    private static func sampleNotice(for decision: AnswerDecision) -> String {
        let sent: String
        switch decision {
        case .allow: sent = "Approve"
        case .deny: sent = "Deny"
        case .option(let index): sent = "option \(index)"
        case .optionId(let id): sent = "option \(id)"
        case .text, .unrecognised: sent = "this answer"
        }
        return "In a live session this would send \(sent) to your Mac."
    }

    private func submit(_ decision: AnswerDecision, key: String) {
        // The answer-path safety net for the whole class: never act on a card
        // that carries an outcome, is terminally resolved locally, or has no live
        // state backing it. The action bar and the option rows are already hidden
        // when this is false; this guards the path itself so no future re-exposure
        // of a control can answer a card no daemon would accept.
        guard isActionable else { return }
        guard inFlight == nil else { return }
        // **Before the biometric gate and before the model**, because in the
        // sample fleet there is nothing on the other end of either: no Mac to
        // send to, so the card says what the tap would have done rather than
        // resolving an answer no daemon ever asked for. Asking for Face ID first
        // would be theatre in front of a send that never happens.
        if model.sampleFleetActive {
            withAnimation(CC.motion.small) { sampleNotice = Self.sampleNotice(for: decision) }
            CCHaptic.warning.fire()
            return
        }
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
        // Same answer-path safety net as `submit`: a deny-with-reason is still an
        // answer, so it must never act on an outcome-carrying, terminally
        // resolved, or unbacked card.
        guard isActionable else { return }
        guard inFlight == nil else { return }
        // A denial with a reason is two sends, and in the sample fleet neither
        // has anywhere to go.
        if model.sampleFleetActive {
            withAnimation(CC.motion.small) { sampleNotice = Self.sampleNotice(for: .deny) }
            CCHaptic.warning.fire()
            return
        }
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
        case .indeterminate, .duplicate, .answeredAtKeyboard: CCHaptic.warning.fire()
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
    /// being the ones that tell a README from `~/.aws/credentials`.
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

/// How wide the inline patch is drawn. A plain `max`, because the rows are one
/// column: two readers cannot disagree, and zero — which `safeAreaInset`'s spare
/// render reports — must not win.
private struct DiffWidthKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        let next = nextValue()
        if next.isFinite { value = max(value, next) }
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
    /// `nil` means "as many lines as it takes". See the Codex option row.
    var lineLimit: Int? = 2

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
                .lineLimit(lineLimit)
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
