import SwiftUI

/// The Deck: one cross-fleet queue of every agent waiting on a human, emptied
/// with a thumb.
///
/// *Buzz → glance → tap-to-advance → "Fleet clear", in about eleven seconds.*
/// That sequence is implemented literally, and three beats are load-bearing:
///
///  * **260ms of confirmation before the stack moves.** That pause is not dead
///    time; it is the receipt. No optimistic advance — the stack does not move
///    until the daemon has said yes.
///  * **One haptic per advance, on arrival.** Arrival is what you feel;
///    departure is what you see.
///  * **120ms of nothing before the clear state.** The pause is what makes it
///    land, and it is skipped when the Deck opened empty, because nothing was
///    cleared and pretending otherwise would be a small lie.
///
/// **There are no swipe gestures here, and there never will be.** Advancing is a
/// button and answering is a button, because an accidental swipe approving
/// `rm -rf` is the one bug that ends this product. The cards behind the top one
/// are decoration; they are not interactive and they cannot be flicked.
struct DeckView: View {
    /// From a deep link: the card to put on top.
    var startingAt: String?

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    @State private var pass = DeckPass()
    @State private var pinned: String?
    /// The deck's own content height, published to everything inside it as the
    /// viewport `ccScrollCap` measures against. See `body`.
    @State private var deckHeight: CGFloat = 0
    @State private var openedAt = Date()
    /// **When the pass ended — the moment the queue emptied, captured once.**
    ///
    /// `THIS PASS` is a past-tense label on a completed measurement, and it was
    /// wired to `model.now`, which ticks every second forever. Four captures of
    /// one `Fleet clear` read `28s` → `55s` → `56s` → `57s`, monotonic: the
    /// queue emptied once, the pass took 28 seconds, and the screen was still
    /// counting. Left open on a table it would have claimed the pass took five
    /// minutes — on the one line people screenshot, and the only number on that
    /// screen with no age beside it to falsify it.
    ///
    /// Cleared again if cards arrive after the mark is drawn, because then the
    /// pass really did continue and freezing the old figure would be the same
    /// lie pointing the other way.
    @State private var endedAt: Date?
    /// Gates the clear state behind the deliberate 120ms of emptiness.
    @State private var clearRevealed = false
    /// True when the Deck was opened with nothing in it: no pause, no
    /// stroke-draw, no success haptic. Nothing was cleared.
    @State private var openedEmpty = false
    @State private var arrivedCount = 0
    @State private var arrivalNoticeTask: Task<Void, Never>?

    private var queue: [ApprovalItem] {
        let arranged = pass.arrange(model.deck, profile: model.daemonProfile)
        // A deep link knows a request id and not which run raised it, so the
        // first card with that id is the one it lands on.
        guard let pinned, let index = arranged.firstIndex(where: { $0.card.requestID == pinned })
        else { return arranged }
        var reordered = arranged
        let card = reordered.remove(at: index)
        reordered.insert(card, at: 0)
        return reordered
    }

    var body: some View {
        NavigationStack {
            let cards = queue
            ZStack {
                CC.color.bg.ignoresSafeArea()
                if !cards.isEmpty {
                    stack(cards: cards)
                } else if clearRevealed || openedEmpty {
                    DeckClearState(
                        settled: pass.settledCount,
                        // The captured interval, never a live one. `endedAt` is
                        // nil only for the beat between the queue emptying and
                        // this branch being reached.
                        elapsed: (endedAt ?? model.now).timeIntervalSince(openedAt),
                        stillRunning: runningCount,
                        drawsMark: !openedEmpty,
                        onDone: { dismiss() })
                }
                // Between the last card leaving and the mark being drawn there
                // is nothing at all. Deliberate.
            }
            // **What the 45% ceiling is a ceiling of.**
            //
            // `ccScrollCap(0.45)` asks the key window when nothing upstream has
            // measured a viewport, and a deck card is not the window: it is a
            // 699.33pt slot inside an 874pt screen, so the pinned action bar
            // capped itself at 874 × 0.45 = **393.3** and took 56.1% of the
            // card, leaving the document 306.67pt at AX5.
            //
            // Measured **here**, on the deck's own container, and not inside the
            // card. The first attempt measured the card's scroll view and fed
            // the number back into the environment its own pinned footer lays
            // out in — and that closed exactly the loop this file's `probe`
            // comment describes: three AX5 tests went from 6s to 177s and failed
            // with `Failed to get matching snapshots`, the app never once
            // reporting itself idle. This `ZStack` is sized by the navigation
            // stack's safe area and by nothing inside it, so the value cannot
            // chase what it is bounding.
            .background {
                GeometryReader { proxy in
                    Color.clear
                        .onChange(of: proxy.size.height, initial: true) { _, height in
                            deckHeight = height.isFinite && height > 0 ? height : 0
                        }
                }
            }
            // Minus the inset that makes the peeks peek, because what the 45%
            // is a ceiling of is the **card**, and the card is this
            // container less that padding. `stack(cards:)` owns the number and
            // it is a constant, so taking it off here cannot introduce the
            // measurement loop this whole arrangement exists to avoid.
            .ccViewport(height: max(0, deckHeight - CC.space.xl))
            .ccNavigationChrome()
            // One vocabulary. The band header says `BLOCKED` from the agent's
            // side; every surface that speaks to the *reader* says `needs you`,
            // and this screen is the one where the reader answers.
            .navigationTitle("Needs you")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                // Nothing waiting is what the clear state says in full; a "0"
                // beside it is the same fact twice.
                if !cards.isEmpty {
                    ToolbarItem(placement: .topBarLeading) { counter(cards.count) }
                        .ccPlainToolbarItem()
                }
                ToolbarItem(placement: .topBarTrailing) {
                    CCButton("Done", variant: .ghost, size: .sm) { dismiss() }
                        .ccToolbarButton()
                        .accessibilityIdentifier("deck-done")
                }
                .ccPlainToolbarItem()
            }
            .onAppear {
                pinned = startingAt
                openedAt = Date()
                endedAt = nil
                openedEmpty = queue.isEmpty
            }
            .onChange(of: cards.isEmpty) { _, isEmpty in
                // The clock stops here, on the transition itself, rather than
                // where the mark is drawn: the pass ended when the last card was
                // answered, and the 120ms of deliberate emptiness that follows
                // is presentation, not elapsed work.
                endedAt = isEmpty ? Date() : nil
                guard isEmpty, !openedEmpty else { return }
                revealClearState()
            }
            .onChange(of: model.deck.count) { old, new in
                // A card that arrives mid-pass is appended to the **back** by
                // `DeckPass` and says so for three seconds. The stack never
                // reorders under the thumb.
                //
                // The one-second floor is honesty, not debouncing: opened from a
                // push while the first frames are still landing, the queue goes
                // 0 → 3 and `+3 arrived` claimed that three agents had asked for
                // you *during this pass*. They had not; the pass had not started.
                guard new > old, Date().timeIntervalSince(openedAt) > 1 else { return }
                arrivedCount += new - old
                announceArrival()
            }
            .onDisappear { arrivalNoticeTask?.cancel() }
        }
    }

    private var runningCount: Int {
        model.fleet.filter { $0.status == .running || $0.status == .idle }.count
    }

    /// Then **120ms of nothing.** Pure `bg`. The pause is what makes the clear
    /// state land.
    private func revealClearState() {
        guard !clearRevealed else { return }
        Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(reduceMotion ? 0 : 120))
            withAnimation(reduceMotion ? CC.motion.reduced : nil) { clearRevealed = true }
        }
    }

    private func announceArrival() {
        arrivalNoticeTask?.cancel()
        arrivalNoticeTask = Task { @MainActor in
            try? await Task.sleep(for: .seconds(3))
            guard !Task.isCancelled else { return }
            withAnimation(CC.motion.small) { arrivedCount = 0 }
        }
    }

    // MARK: Stack

    /// **One identity space for all three slots.**
    ///
    /// The signature beat — peek 1 springing to full size with a slight
    /// overshoot — used to be a cross-fade, and the reason was structural: the
    /// peek was an anonymous `RoundedRectangle` and the incoming card was a
    /// `DecisionCardView`, so SwiftUI saw two unrelated views. Peek 1 was
    /// *removed* and the card *inserted* at its final position with
    /// `insertion: .opacity`. Nothing travelled; the card in front of you had
    /// no mass.
    ///
    /// Now every slot is one `ForEach` row keyed on `card.id`, and inset, offset
    /// and opacity are driven from the index — so a card moving from slot 1 to
    /// slot 0 is a *property change on a stable identity*, which is exactly what
    /// `spring(response: 0.32, damping: 0.82)` is for. The transitions below
    /// only run at the ends of the stack: a card leaving it, or a fourth card
    /// arriving into the back.
    private func stack(cards: [ApprovalItem]) -> some View {
        ZStack(alignment: .top) {
            ForEach(Array(cards.prefix(3).enumerated()), id: \.element.id) { index, card in
                slot(card, index: index, canPostpone: cards.count > 1)
            }
        }
        // Full-bleed, so the card's own 16pt padding puts its content on the
        // same left edge as the sheet's — the two homes must not diverge, and a
        // presentation inset would have moved every line 4pt off the grid.
        // The bottom inset is what makes the peeks *peek*: without it the two
        // rectangles behind the top card sit entirely under it and the stack
        // reads as a single page.
        .padding(.bottom, CC.space.xl)
        .ccAnimation(CC.motion.physical, value: cards.first?.id)
    }

    /// Slot 0 is the card; slots 1 and 2 are peeks. Everything that says *which
    /// slot* lives here, on a view whose identity is the card — that is what
    /// makes the promotion travel.
    private func slot(_ card: ApprovalItem, index: Int, canPostpone: Bool) -> some View {
        let isTop = index == 0
        return face(card, isTop: isTop, canPostpone: canPostpone)
            .background(CC.color.surface)
            .clipShape(RoundedRectangle(cornerRadius: CC.radius.xl, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: CC.radius.xl, style: .continuous)
                    // The only coloured card border in the app, and only at HIGH.
                    .strokeBorder(
                        isTop ? cardBorder(card) : CC.color.border,
                        lineWidth: CC.stroke.hairline)
            }
            // Promotion up the stack: inset 8 → 0, y 8 → 0, opacity 0.55 → 1.
            .padding(.horizontal, CGFloat(index * 8))
            .offset(y: CGFloat(index * 8))
            .opacity(index == 0 ? 1 : (index == 1 ? 0.55 : 0.28))
            .zIndex(Double(-index))
            // Peeks carry no gesture and no hit area, so the only thing a stray
            // touch can land on is the card you are actually reading.
            .allowsHitTesting(isTop)
            .accessibilityHidden(!isTop)
            .transition(.asymmetric(insertion: .opacity, removal: exitTransition))
    }

    @ViewBuilder
    private func face(_ card: ApprovalItem, isTop: Bool, canPostpone: Bool) -> some View {
        if isTop {
            DecisionCardView(
                approval: card,
                onSettled: { settle(card) },
                comeBackToThis: canPostpone ? { advance(past: card) } : nil
            )
            // Belt and braces on the one piece of state that must never carry
            // over: a promoted card gets a fresh read gate, so "you have seen
            // this command" cannot be inherited from the card you just answered.
            .id(card.id)
        } else {
            // A peek is empty — no text, and emphatically no coloured border,
            // which was the rainbow problem one level down.
            Color.clear
        }
    }

    /// The exit: scale 1 → 0.94, opacity 1 → 0, y −16, 240ms **easeIn**.
    ///
    /// Stated on the transition rather than left to the container's spring. The
    /// container drives the promotion, and a departing card that bounces on its
    /// way out inverts the asymmetry: the arrival is the physical beat and gets
    /// the spring, the departure is crisp and gets an ease-in.
    private var exitTransition: AnyTransition {
        guard !reduceMotion else { return .opacity.animation(CC.motion.reduced) }
        return .scale(scale: 0.94)
            .combined(with: .opacity)
            .combined(with: .offset(y: -16))
            .animation(CC.motion.exit)
    }

    private func cardBorder(_ card: ApprovalItem) -> Color {
        card.assessment(profile: model.daemonProfile).effective == .high
            ? CC.color.danger.opacity(0.40)
            : CC.color.border
    }

    /// A 10pt pulsing dot, the number, and the word. No `hand.raised.fill` —
    /// `CCStatusDot` carries what the glyph was carrying.
    private func counter(_ count: Int) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: CC.space.xs) {
                CCStatusDot(
                    color: CC.color.warning, size: CCStatusDot.Size.cardHeader.rawValue,
                    pulses: count > 0)
                Text("\(count)")
                    .ccType(CC.type.title)
                    .monospacedDigit()
                    .foregroundStyle(CC.text.primary)
                    .contentTransition(.numericText(countsDown: true))
                    .ccAnimation(CC.motion.medium, value: count)
                // The words used to be here too — `1 need you` beside a title
                // reading `Needs you`, which is the same fact twice in one bar and
                // the third item competing for a width that only fits two. The
                // title owns the words; this owns the number. VoiceOver still
                // hears the whole sentence through `accessibilityLabel` below, so
                // nothing was lost but the repetition.
            }
            if arrivedCount > 0 {
                Text("+\(arrivedCount) arrived")
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
                    .transition(.opacity)
            }
        }
        // Toolbars compress custom content to its minimum width, which silently
        // collapses the count to nothing — and the count is the whole point.
        .fixedSize()
        .accessibilityElement(children: .ignore)
        // Cards, spoken as what they are. This counted cards and said "agents".
        .accessibilityLabel(FleetCount.needsYou(count))
        .accessibilityIdentifier("deck-count")
    }

    // MARK: Advancing

    private func settle(_ card: ApprovalItem) {
        // The confirmed button holds for 260ms so the eye registers what
        // happened, *then* the stack moves. Advancing sooner would mean the
        // receipt was never read.
        Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(reduceMotion ? 0 : 260))
            withAnimation(reduceMotion ? CC.motion.reduced : CC.motion.physical) {
                pass.settle(card.id)
                if pinned == card.card.requestID { pinned = nil }
            }
            // One haptic per advance, at the moment the new card lands —
            // 280ms after the promotion starts. Never on exit.
            try? await Task.sleep(for: .milliseconds(280))
            if !queue.isEmpty { CCHaptic.light.fire() }
        }
    }

    private func advance(past card: ApprovalItem) {
        withAnimation(reduceMotion ? CC.motion.reduced : CC.motion.physical) {
            pass.postpone(card.id)
            if pinned == card.card.requestID { pinned = nil }
        }
        Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(280))
            CCHaptic.light.fire()
        }
    }
}

// MARK: - The clear state

/// **The mark, not a sticker.** A 72pt circle stroked 1.5pt in `success` over a
/// 12% fill, with a 28pt checkmark inside — both stroke-drawn, the circle over
/// 300ms and then the checkmark over 220ms. Restrained; expensive.
///
/// The success haptic fires the instant the checkmark *finishes drawing*, not on
/// `onAppear`: a haptic that lands off the visual beat reads as a second,
/// unrelated event.
private struct DeckClearState: View {
    let settled: Int
    /// **A captured interval, not a live one.** `THIS PASS` is past tense; the
    /// caller freezes this the moment the queue empties.
    let elapsed: TimeInterval
    let stillRunning: Int
    /// False when the Deck opened with an empty queue — nothing was cleared, so
    /// nothing is celebrated.
    let drawsMark: Bool
    let onDone: () -> Void

    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    @State private var circleProgress: CGFloat = 0
    @State private var checkProgress: CGFloat = 0
    @State private var textStage = 0

    var body: some View {
        VStack(spacing: 0) {
            Spacer(minLength: 0)

            VStack(spacing: CC.space.md) {
                mark
                    .padding(.bottom, CC.space.xs)

                Text("Fleet clear")
                    .ccType(CC.type.title)
                    .foregroundStyle(CC.text.primary)
                    .opacity(textStage >= 1 ? 1 : 0)
                    .offset(y: textStage >= 1 ? 0 : 8)

                Text(runningLine)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
                    .opacity(textStage >= 2 ? 1 : 0)
                    .offset(y: textStage >= 2 ? 0 : 8)

                statLine
                    .opacity(textStage >= 3 ? 1 : 0)
                    .offset(y: textStage >= 3 ? 0 : 8)
            }
            .frame(maxWidth: 320)
            // Optical centring: a group centred mathematically reads as low.
            .padding(.bottom, CC.space.xxxl)

            Spacer(minLength: 0)

            CCButton("Done", variant: .primary, size: .lg, fullWidth: true, action: onDone)
                .padding(.horizontal, CC.space.md)
                .padding(.bottom, CC.space.xl)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .onAppear(perform: play)
        .accessibilityElement(children: .contain)
        .accessibilityAddTraits(.isSummaryElement)
    }

    private var mark: some View {
        ZStack {
            Circle()
                .fill(CC.color.muted(.success))
            Circle()
                .trim(from: 0, to: circleProgress)
                .stroke(
                    CC.color.success,
                    style: StrokeStyle(lineWidth: 1.5, lineCap: .round)
                )
                // Starts at twelve o'clock rather than three: a ring that begins
                // at the right edge reads as a progress meter, not a mark.
                .rotationEffect(.degrees(-90))
            Checkmark()
                .trim(from: 0, to: checkProgress)
                .stroke(
                    CC.color.success,
                    style: StrokeStyle(lineWidth: 2.5, lineCap: .round, lineJoin: .round)
                )
                .frame(width: 28, height: 28)
        }
        .frame(width: 72, height: 72)
        .accessibilityHidden(true)
    }

    @ViewBuilder
    private var statLine: some View {
        if settled > 0 {
            // The eleven-second claim made visible, and the line a user
            // screenshots.
            CCStatStrip([
                CCStat(
                    "Cleared", value: "\(settled)",
                    spokenValue: settled == 1 ? "1 decision" : "\(settled) decisions"),
                CCStat(
                    "This pass", value: Format.age(elapsed),
                    spokenValue: Format.spokenAge(elapsed).replacingOccurrences(
                        of: " ago", with: "")),
            ])
        } else {
            Text("Nothing was waiting.")
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
        }
    }

    /// One vocabulary: `needs you` is the state, `waiting` belongs to the clock
    /// and nowhere else. The composition — green mark, two lines, no confetti —
    /// is untouched.
    private var runningLine: String {
        guard stillRunning > 0 else { return "Nothing needs you." }
        return stillRunning == 1
            ? "1 agent still running · nothing needs you"
            : "\(stillRunning) agents still running · nothing needs you"
    }

    private func play() {
        guard drawsMark, !reduceMotion else {
            circleProgress = 1
            checkProgress = 1
            textStage = 3
            if drawsMark { CCHaptic.success.fire() }
            return
        }
        withAnimation(CC.motion.draw) { circleProgress = 1 }
        Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(300))
            withAnimation(.easeOut(duration: 0.22)) { checkProgress = 1 }
            for stage in 1...3 {
                withAnimation(CC.motion.medium) { textStage = stage }
                try? await Task.sleep(for: .milliseconds(60))
            }
            // t + 300 + 220. The haptic lands on the visual beat.
            try? await Task.sleep(for: .milliseconds(40))
            CCHaptic.success.fire()
        }
    }
}

/// The tick, as a path, so it can be stroke-drawn rather than faded in. An SF
/// Symbol cannot be trimmed.
private struct Checkmark: Shape {
    func path(in rect: CGRect) -> Path {
        var path = Path()
        path.move(to: CGPoint(x: rect.minX + rect.width * 0.14, y: rect.midY + rect.height * 0.02))
        path.addLine(
            to: CGPoint(x: rect.minX + rect.width * 0.40, y: rect.midY + rect.height * 0.26))
        path.addLine(
            to: CGPoint(x: rect.minX + rect.width * 0.86, y: rect.midY - rect.height * 0.26))
        return path
    }
}

// MARK: - Accessory bar

/// The bar that rises from the bottom of the fleet when decisions are pending.
///
/// It is a summary and a door, never a decision: there is no approve button
/// here, because a control that answers without showing you what it is answering
/// is the thing this product exists to replace. The orange lives in the dot,
/// where one instance of it is enough — `Review` is white on black like every
/// other primary in the app.
struct DeckAccessoryBar: View {
    let count: Int
    /// Where the card `Review` opens comes from.
    let topPlace: String
    /// **What it wants to run**, on a line of its own — see `subtitleLine`.
    let topCommand: String?
    let waitingSince: Date?
    let now: Date
    /// Set when the link cannot carry an answer. The bar still rises — you must
    /// know agents are waiting — and says why it cannot be used.
    var blockedReason: String?
    let open: () -> Void

    @Environment(\.dynamicTypeSize) private var typeSize
    /// The bar's own copy of the screen's Dynamic Type ramp for the gutter, so
    /// its dot and its text land on the two columns the rows above it use —
    /// at **every** size, not just at the one the constants were written for.
    @ScaledMetric(relativeTo: .footnote) private var gutterDot: CGFloat = CC.size.dot

    private var isLargeType: Bool { typeSize.isAccessibilitySize }

    /// Where this bar's text starts: 52 at reading sizes, and the same number
    /// the fleet rows 400pt above it are using. It measured **39.00** — the bar
    /// laid out as `[16 pad][10 dot][12 gap][text]` instead of the spine's
    /// `[32 gutter][20 gap][52 content]`, so scanning the left edge of the root
    /// screen top to bottom stepped out twice at the bottom.
    private var textColumn: CGFloat { ContentColumn.text(dot: gutterDot) }

    var body: some View {
        VStack(spacing: 0) {
            CCHairline()
            Button {
                CCHaptic.openHeavy.fire()
                open()
            } label: {
                summary
            }
            // The accessibility modifiers go on the `Button` itself rather than on a
            // wrapper: `accessibilityElement(children: .ignore)` around it produces a
            // plain element that no longer reports the button trait, which loses
            // both the VoiceOver affordance and any way to find it by identifier.
            // The same sentence a sighted reader gets. `N agents need you` was a
            // third wording over the card count — the noun was wrong as well as
            // unshared.
            .accessibilityLabel(aggregate)
            // The full subtitle even where the visible one is dropped at
            // accessibility sizes: a screen reader has no height budget.
            .accessibilityValue(subtitle)
            .accessibilityHint(
                blockedReason ?? "Opens the deck of pending decisions, riskiest first")
            .accessibilityIdentifier("deck-bar")
            .buttonStyle(.plain)
            .disabled(blockedReason != nil)

            // **Outside the button, on purpose.** See `reasonLine`.
            if let blockedReason { reasonLine(blockedReason) }
        }
        // The 45% ceiling, tightened. The decision card's action bar may claim
        // 45% of the viewport because that bar is where the decision is made;
        // this one summarises the list behind it and opens a door, so it gets a
        // **sixth**. At reading sizes that is more room than the 76pt row
        // needs — the bar measures 109.33 against the 110.0 budget and never
        // caps — and at AX5 it is what stops the blocked state, whose sentence
        // runs to five lines there, from annexing a third of the screen. Past
        // the ceiling the bar gives the room back and scrolls; nothing is
        // truncated, and `ccScrollCap` shows the indicator that says so.
        .ccScrollCap(Self.viewportShare, minimum: CC.size.rowRoomy)
        // `surfaceRaised` and a hairline, never `.bar` — a blurred UIKit
        // material over `#000` resolves to a flat mid-grey smear that belongs to
        // no palette.
        .background(CC.color.surfaceRaised)
        .ccAnimation(CC.motion.physical, value: count)
    }

    /// The most of the screen this bar may ever take.
    ///
    /// **The 110.0 row budget is this rule's value at `L`, not the rule.**
    /// Measured, the bar is 109.33 at `L` — inside the budget, uncapped — and
    /// 167.00 at AX5, where one line of `callout` is 55pt and the aggregate
    /// needs two of them.
    /// There is no layout that states `3 decisions need you` at AX5 inside
    /// 110pt, and the two ways to force it — truncating the sentence or capping
    /// Dynamic Type — are both worse than the height. So the constant travels as
    /// a *share*: a sixth, which is 145.67 of an 874pt viewport.
    ///
    /// The 189.67 measured before this was 21.7% of the viewport, and most of it
    /// was one decorative `Review` button that `CCAdaptiveStack` had stacked
    /// under the text.
    private static let viewportShare: CGFloat = 1.0 / 6.0

    /// The dot, the aggregate, the preview, and the door.
    private var summary: some View {
        HStack(alignment: .center, spacing: CC.space.sm) {
            HStack(alignment: .top, spacing: 0) {
                CCStatusDot(
                    color: CC.color.warning,
                    size: CCStatusDot.Size.cardHeader.rawValue,
                    pulses: true)
                // Optically centres the disc on the aggregate's cap height
                // rather than on its line box.
                .padding(.top, 3)
                // The gutter is a *column*, not a dot plus a gap: the bar's disc
                // is 10pt where a row's is 8, and hanging the text off the dot's
                // own width would put this line 2pt right of every row above it.
                .frame(
                    width: ContentColumn.gutterWidth(dot: gutterDot), alignment: .leading)

                VStack(alignment: .leading, spacing: 1) {
                    aggregateLine
                    subtitleLine
                }
                Spacer(minLength: CC.space.xs)
            }

            // Not independently tappable: the whole bar is the hit area, and
            // two nested targets is how a 2am thumb lands on the wrong one.
            //
            // **Gone at accessibility sizes**, where `CCAdaptiveStack` used to
            // stack it under the text: measured, that one decorative control
            // cost the bar 67.33pt of its own height plus a 12pt gap, which is
            // most of the 79.67pt it was over budget by. It carries no hit area
            // and no accessibility element — the `Button` above owns both — so
            // what replaces it is a chevron on the aggregate's own line, which
            // is the same promise in one glyph's width.
            if !isLargeType {
                CCButton("Review", variant: .primary, size: .md) {}
                    .allowsHitTesting(false)
                    .accessibilityHidden(true)
            }
        }
        .padding(.leading, ContentColumn.gutter)
        .padding(.trailing, CC.space.md)
        .padding(.vertical, CC.space.sm)
        .contentShape(Rectangle())
    }

    private var aggregateLine: some View {
        HStack(alignment: .firstTextBaseline, spacing: CC.space.xs) {
            Text(aggregate)
                .ccType(CC.type.callout)
                .foregroundStyle(CC.text.primary)
                .contentTransition(.numericText(countsDown: true))
                .fixedSize(horizontal: false, vertical: true)
            if isLargeType {
                Spacer(minLength: CC.space.xs)
                CCIcon("chevron.right", size: CC.size.iconSm, weight: .semibold)
                    .foregroundStyle(CC.text.tertiary)
                    .accessibilityHidden(true)
            }
        }
    }

    /// **Why the bar cannot be used, drawn outside the control it is about.**
    ///
    /// It shipped as line 2 *inside* the button, which cost it twice. `.disabled`
    /// on a `.plain` button dims its whole label, so the sentence rendered at
    /// **#845D1B on #131313 — 3.16:1**, below AA; WCAG 1.4.3's exemption for an
    /// inactive component covers the control's *label*, never the explanation of
    /// why it is inactive. And `.lineLimit(2)` cut it mid-word: at `L` the bar
    /// read `…The socket is open but the daem…`, ellipsis at x=263.00, on a
    /// sentence whose job is to tell the reader which screen to go to.
    ///
    /// Out here it is neither dimmed nor limited: `warning` on `surfaceRaised`
    /// is **8.20:1**, and it wraps to as many lines as it needs. This is exactly
    /// what `ccDisabled` does for every other dead control in the product; the
    /// bar could not use it because the reason has to sit on the bar's own
    /// columns rather than on a generic modifier's guess at them.
    private func reasonLine(_ reason: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 0) {
            CCIcon(
                "exclamationmark.circle.fill", size: 11, weight: .semibold, relativeTo: .caption
            )
            .foregroundStyle(CC.color.warning)
            // **A mark in the gutter takes no layout width**, the way
            // `CCSectionHeader` hangs its dot. Given a column to sit in instead,
            // this glyph grew past it at AX5 — an 11pt `caption` symbol scales to
            // about 26pt there and `CCIcon` frames it at 1.35× that — and pushed
            // its own sentence right, so the render read `⚠Link stale`, the mark
            // welded to the L. Centred on the gutter axis with no width, an
            // oversized mark bleeds *symmetrically* into the empty screen margin
            // and the text column never moves.
            .frame(width: 0)
            .offset(x: -(textColumn - ContentColumn.gutter - CC.size.dot / 2))
            Text(reason)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.color.warning)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
        }
        .padding(.leading, textColumn)
        .padding(.trailing, CC.space.md)
        .padding(.bottom, CC.space.sm)
        // Not hidden, for the reason `ccDisabled` states: a hint is a *setting*,
        // and a reader with Speak Hints off would otherwise be told nothing.
        .accessibilityElement(children: .combine)
        .accessibilityLabel(reason)
    }

    /// The canonical aggregate — **the same function the fleet's display line
    /// calls**, on a number the fleet handed down. One function owns this
    /// sentence and every surface prints that one sentence.
    ///
    /// It used to be a second copy of the wording over a second count: `3 need
    /// you` here against `2 need you` in the display slot 631pt above, because
    /// this counted cards and that counted sessions. Identical words, different
    /// numbers, one viewport.
    private var aggregate: String { FleetCount.needsYou(count) }

    /// **The command gets a line of its own.**
    ///
    /// It shipped glued to the place inside one `Text` — `app-1 · git push
    /// --force origin main` — set in *proportional* type and middle-truncated,
    /// which on a bar that also carries a clock and an 84pt `Review` rendered
    /// `app-1 · git p…origin main`. Two halves of a command spliced by an
    /// ellipsis is not a command, and the half that went is the one that says
    /// whether the branch about to be overwritten is `main` or `master`.
    ///
    /// The place and the clock are short and fixed; the command is neither, so
    /// it takes the whole measure underneath them. That costs the bar one 16pt
    /// line and buys it 33 monospace characters — enough for the command this
    /// screen exists to advertise, whole.
    @ViewBuilder
    private var subtitleLine: some View {
        // Nothing when the link cannot carry an answer: what the bar owes the
        // reader then is the reason, and `reasonLine` draws that below, outside
        // the disabled control so it is neither dimmed nor cut.
        if blockedReason == nil, !isLargeType {
            // Dropped entirely at accessibility sizes. Measured at AX5: the bar
            // wrapped `3 agents` / `need you` onto two lines, then the card
            // name, then the clock, then a full-width `Review` — roughly 45% of
            // the viewport, leaving the fleet behind it one clipped row. The
            // aggregate and the door are what the bar is for; the preview is a
            // luxury, and VoiceOver still gets it as the button's value.
            HStack(alignment: .firstTextBaseline, spacing: CC.space.xxs + 1) {
                Text(verbatim: topPlace)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.tertiary)
                    .lineLimit(1)
                    // From the front, as the rows are: this is a project, and a
                    // project is recognised by how it starts. Trimming its
                    // middle returns two elisions and neither name.
                    .truncationMode(.tail)
                if let waitingSince {
                    Text("·")
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.disabled)
                    CCWaitClock(
                        since: waitingSince, now: now, prefix: "waiting",
                        style: CC.type.monoSmall)
                }
            }
            if let topCommand {
                // The kit's container-less mono run, shared with the fleet row
                // and the session's approval preview.
                CCMonoBlock(inline: topCommand)
                    .padding(.top, 1)
            }
        }
    }

    /// What VoiceOver hears. Always the whole command, at every type size —
    /// including the sizes where the visible preview is dropped for height,
    /// because a screen reader has no height budget to spend.
    ///
    /// **Not the blocked reason.** It used to be, back when the reason replaced
    /// this line inside the button; the reason is now its own visible element
    /// with its own label, and it is the button's hint besides, so returning it
    /// here would have a reader hear the same sentence three times and lose the
    /// preview at exactly the moment they are deciding whether to open the Deck
    /// at all.
    private var subtitle: String {
        var parts = [topPlace]
        if let topCommand { parts.append(topCommand) }
        if let waitingSince {
            parts.append("waiting \(Format.age(since: waitingSince, now: now))")
        }
        return parts.joined(separator: " · ")
    }
}

