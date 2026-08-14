import SwiftUI
import UserNotifications

/// Where a navigation route points: one *run*, by its key, never by its tmux
/// name. A route that held `cc-1` would follow the name to whichever run holds
/// it next. Reached from a `codeconnect://` URL or a row tap — a notification
/// names no run, so it never builds one of these.
struct SessionRoute: Hashable {
    let key: String
    /// Set when arriving from the Deck, so the card opens without a second tap.
    var openRequestID: String?
    /// Set from a "Done, unreviewed" fleet row: what you came for is the diff.
    var openDiff: Bool = false
}

/// The 2am glance. Fixed sort, every fact stamped with its age, and the one
/// number that matters — how many agents need you — in the largest type on the
/// screen rather than a caption under the word `Fleet`.
///
/// Three rules do most of the work here:
///
///  * **Visual weight follows urgency.** A row is two lines unless the third
///    carries something non-redundant. A calm fleet of eight running agents is
///    sixteen lines of text; two blockers grow exactly two rows.
///  * **Colour is information.** The only coloured containers on the screen are
///    the Blocked and Failed band borders, and they are doing real work — they
///    draw the eye to the band that needs you without adding a glyph.
///  * **One banner, ever.** Every candidate goes to `CCBannerSlot` and it picks
///    by rank: rejected beats offline beats stale beats cached. Facts that lose
///    still appear per-element: a cached row gets a hollow dot, never the word
///    "cached".
struct FleetView: View {
    @Environment(AppModel.self) private var model
    /// The widest tool label on screen, so every command beside one starts on
    /// the same edge. Settles on the first pass and then stops changing, so it
    /// costs one extra layout and nothing after that.
    @State private var toolColumn: CGFloat = 0
    @State private var path: [SessionRoute] = []
    @State private var showSettings = false
    /// The one-open-swipe-row rules. Rows observe it; this view only calls
    /// methods on it, so a pulse closes a row without re-rendering the fleet.
    @State private var swipeCloses = CCSwipeCloseCoordinator()
    @State private var showLinkDetail = false
    @State private var showDeck = false
    @State private var deckStartsAt: String?
    @State private var diffRoute: DiffRoute?
    /// Expanding `Ended` is deliberately not persisted: a fresh launch is a
    /// fresh glance.
    @State private var showEnded = false
    /// The band's own observe-only reason, set when its header note is tapped.
    @State private var capabilityReason: CapabilityReason?
    /// Whether the scroll header's title has travelled far enough for the
    /// inline one to take over, so the screen never shows its name twice.
    ///
    /// Deliberately a `Bool` and not a continuous opacity: this view's `body`
    /// re-reads `model.fleet`, and on a soak-tested daemon that walks two dozen
    /// event logs. A per-frame opacity turned a flick into two dozen full fleet
    /// rebuilds and hung the main thread — measured, on a 24-session daemon.
    /// One state change per direction, cross-faded by the animation, is
    /// indistinguishable and free.
    @State private var titleVisible = false
    /// When this screen first appeared, so the loading state can tick an honest
    /// elapsed counter instead of a spinner with no scale.
    @State private var appearedAt = Date()
    /// The dot's own Dynamic Type ramp, mirrored so that the text column a band
    /// header lands on is the text column its rows land on.
    ///
    /// Not applied *to* a dot — `CCStatusDot` scales itself and doing it twice
    /// is how a disc bursts its gutter — but to the column a dot defines. A
    /// header pinned to a constant 52 sat 6pt left of the titles beneath it at
    /// AX5, which is a third vertical edge — the screen gets exactly two —
    /// arriving by accident at exactly the size they are hardest to hold.
    @ScaledMetric(relativeTo: .footnote) private var gutterDot: CGFloat = CC.size.dot

    /// Where every line of text on this screen starts: 52 at reading sizes.
    private var textColumn: CGFloat { ContentColumn.text(dot: gutterDot) }

    private static let scrollSpace = "cc.fleet.scroll"

    var body: some View {
        // One sort per body: `model.fleet` re-sorts on every access. Nothing on
        // this screen ticks it any more — see `FleetRowView.now` — so a body
        // pass now means the fleet itself changed.
        let rows = model.fleet
        // Hoisted for the same reason, and for a sharper one: `model.deck`
        // flat-maps `pendingApprovals` across every session, which walks every
        // event log the app holds. Read inside the `safeAreaInset` builder it was
        // re-evaluated on every *layout* pass — so one flick on a 24-session
        // daemon walked two dozen logs a frame and took the main thread down for
        // thirty seconds. Measured. Read it once per body instead.
        let pending = model.deck
        return NavigationStack(path: $path) {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    scrollHeader(rows)
                    content(rows)
                }
                // The Deck bar's own height is reserved by `safeAreaInset`, and
                // that height triples at AX5 — a hard-coded 76 would leave the
                // last row under the bar exactly when the rows are tallest.
                .padding(.bottom, CC.space.xl)
                .background {
                    GeometryReader { proxy in
                        Color.clear.preference(
                            key: FleetScrollKey.self,
                            value: proxy.frame(in: .named(Self.scrollSpace)).minY)
                    }
                }
            }
            .coordinateSpace(name: Self.scrollSpace)
            .onPreferenceChange(FleetScrollKey.self) { minY in
                // 24pt of travel, with a 4pt dead band so a bounce at the top
                // cannot flap the title.
                let next = titleVisible ? (-minY > 20) : (-minY > 24)
                if next != titleVisible { titleVisible = next }
            }
            // Scrolling stands an open swipe row down, the way a `List` would.
            // A simultaneous gesture, not the scroll-offset preference:
            // measured on this OS, `FleetScrollKey` delivers its value once at
            // setup and never again mid-drag, so an offset-diffing close signal
            // simply never fired. It recognises alongside the scroll without
            // claiming the touch — the guard tests in `SwipeToRemoveUITests`
            // are the proof that scrolling itself stays fluid — and it no-ops
            // through the coordinator's nothing-is-open guard on every
            // ordinary interaction. Deliberately no tap equivalent: a blanket
            // tap-closes-everything fired on the revealed Remove button itself,
            // closing the row before its refusal could be shown. Taps are
            // handled where they mean something — the open row's own catcher,
            // and the navigation/sheet observers below.
            .simultaneousGesture(
                DragGesture(minimumDistance: 12).onChanged { value in
                    // The same 2:1-past-a-floor intent gate the app's other
                    // drags use. A bare "more vertical than horizontal" check
                    // fired on the first millimetres of a *horizontal* row
                    // swipe — early jitter is often vertical-biased — and the
                    // resulting pulse yanked shut the very row being dragged
                    // open. Measured, then gated.
                    let h = abs(value.translation.height)
                    let w = abs(value.translation.width)
                    if h > 24, h > w * 2 {
                        swipeCloses.closeAll()
                    }
                }
            )
            .background(CC.color.bg)
            .scrollIndicators(.hidden)
            .ccCollectsToolColumn(into: $toolColumn)
            .refreshable { model.refreshFleet() }
            // Opaque `bg` *and* a hard scroll edge, welded together in the kit
            // because either one alone is a different wrong bar.
            .ccNavigationChrome()
            .navigationTitle("Fleet")
            .navigationBarTitleDisplayMode(.inline)
            .navigationDestination(for: SessionRoute.self) { route in
                SessionDetailView(route: route)
            }
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    LinkPill { showLinkDetail = true }
                }
                .ccPlainToolbarItem()
                ToolbarItem(placement: .principal) {
                    // The same word the scroll header carries, arriving only as
                    // that header leaves. Two titles at once is the bug this
                    // fade exists to prevent.
                    Text("Fleet")
                        .ccType(CC.type.headline)
                        .foregroundStyle(CC.text.primary)
                        .opacity(titleVisible ? 1 : 0)
                        .ccAnimation(CC.motion.small, value: titleVisible)
                        .accessibilityHidden(true)
                }
                .ccPlainToolbarItem()
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        CCHaptic.light.fire()
                        showSettings = true
                    } label: {
                        CCIcon("gearshape", size: 17, weight: .medium)
                            .foregroundStyle(CC.text.primary)
                            // The circle scales with the glyph inside it — a
                            // fixed 36pt ring around a symbol that grows is a
                            // symbol that escapes its ring at AX sizes.
                            .ccGlyphContainer(CC.size.controlSm)
                            .ccHitTarget()
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Settings and pairing")
                }
                .ccPlainToolbarItem()
            }
            // The bar prints the headline's own number, not its own count of the
            // cards it happens to hold — see `FleetCount`.
            .safeAreaInset(edge: .bottom, spacing: 0) {
                deckBar(pending, waiting: FleetCount.decisions(in: rows))
            }
            // Leaving the screen — a push, or any sheet — stands the open
            // swipe row down. A destructive control must not sit armed under
            // whatever the user comes back to.
            .onChange(of: overlayFingerprint) { swipeCloses.closeAll() }
            .sheet(isPresented: $showSettings) { PairingView() }
            .sheet(isPresented: $showLinkDetail) { LinkHealthSheet() }
            .sheet(item: $capabilityReason) { CapabilitySheet(reason: $0.text) }
            .fullScreenCover(isPresented: $showDeck) {
                // A mode, not an inspection — and the presentation is
                // chosen partly because it removes the interactive-dismiss
                // gesture a `.sheet` would install under the reader's thumb.
                DeckView(startingAt: deckStartsAt)
                    .environment(model)
            }
            .sheet(item: $diffRoute) { route in
                DiffSheet(key: route.key)
                    .environment(model)
            }
            .onChange(of: model.pendingDeepLink) { _, _ in consumeDeepLink() }
            .onAppear {
                appearedAt = Date()
                consumeDeepLink()
            }
        }
    }

    // MARK: Header

    /// **The largest type on the triage screen is the answer, not the noun.**
    ///
    /// `Fleet` used to own the display slot: 32pt, white, the brightest thing on
    /// screen, and carrying no information at all — a tired reader learned the
    /// name of the app they had just opened. The name is now a `micro` eyebrow
    /// and the state sentence has the display slot, so the reading order is
    /// *how many need me* → *what is left* → *which agent*.
    private func scrollHeader(_ rows: [FleetRow]) -> some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            Text("Fleet")
                .ccType(CC.type.micro)
                .foregroundStyle(CC.text.tertiary)
                .accessibilityHidden(true)
            Text(headline(rows))
                .ccType(CC.type.display)
                .foregroundStyle(CC.text.primary)
                .fixedSize(horizontal: false, vertical: true)
                .contentTransition(.numericText(countsDown: true))
                .accessibilityAddTraits(.isHeader)
                .accessibilityLabel("Fleet. \(headline(rows))")
            if let summary = summaryLine(rows) {
                Text(summary)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, CC.space.md)
        .padding(.top, CC.space.xs)
        .padding(.bottom, CC.space.xl)
    }

    /// The one number that matters, in the canonical words — and it is the same
    /// number, from the same function, that the accessory bar prints. One
    /// function owns this sentence and every surface prints that one sentence,
    /// so no two surfaces can disagree about how many agents need you.
    ///
    /// It used to count rows in the Blocked band while the bar counted cards in
    /// the Deck, which is a lie the moment one agent stacks two approvals. See
    /// `FleetCount`.
    private func headline(_ rows: [FleetRow]) -> String {
        guard !rows.isEmpty else {
            return model.hasLiveFleet ? "No agents registered" : "Nothing has arrived yet"
        }
        let waiting = FleetCount.decisions(in: rows)
        if waiting > 0 { return FleetCount.needsYou(waiting) }
        // Nothing is held, but something broke, and that is still the first
        // thing a reader should be told.
        let failed = rows.filter { $0.status == .failed }.count
        if failed > 0 { return failed == 1 ? "1 failed" : "\(failed) failed" }
        return "Nothing needs you"
    }

    /// What is left after the headline has taken its count. Bands with nothing
    /// in them are omitted rather than printed as `0` — a zero is a fact nobody
    /// asked for — and `nil` draws no line at all.
    private func summaryLine(_ rows: [FleetRow]) -> String? {
        guard !rows.isEmpty else { return nil }
        var parts: [String] = []
        let blocked = rows.filter { $0.status == .blocked }.count
        let failed = rows.filter { $0.status == .failed }.count
        // Only when the headline did not already take it.
        if blocked > 0, failed > 0 { parts.append("\(failed) failed") }
        let running = rows.filter { $0.status == .running }.count
        let done = rows.filter { $0.status == .doneUnreviewed }.count
        let idle = rows.filter { $0.status == .idle }.count
        if running > 0 { parts.append("\(running) running") }
        if done > 0 { parts.append("\(done) done") }
        if idle > 0 { parts.append("\(idle) idle") }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    // MARK: Content

    @ViewBuilder
    private func content(_ rows: [FleetRow]) -> some View {
        // Its own view, and deliberately: it reads the link's age, and read from
        // here that read belonged to the whole fleet.
        FleetBanner { showSettings = true }

        if rows.isEmpty {
            if model.hasLiveFleet {
                emptyState
            } else {
                loadingState
            }
        } else {
            ForEach(bands(of: rows), id: \.status) { band in
                if band.status == .ended && !showEnded {
                    endedFooter(band)
                } else {
                    bandView(band)
                }
            }
        }
    }

    // MARK: Bands

    private struct Band {
        let status: FleetStatus
        let rows: [FleetRow]
        /// `control` / `observe` when every row in the band agrees, `nil` when
        /// they do not — and a band that does not agree is exactly the band
        /// where capability is worth printing per row.
        let capability: CapabilityBadge?
    }

    private func bands(of rows: [FleetRow]) -> [Band] {
        FleetStatus.allCases.compactMap { status in
            let matching = rows.filter { $0.status == status }
            guard !matching.isEmpty else { return nil }
            let first = matching[0].capability
            // Presentation state, not `canAct`: unknown and observe both gate
            // actions, but only one of them may be *said* — comparing on
            // `canAct` would let one settled observe row brand a band of
            // unknowns, or the reverse.
            let uniform = matching.allSatisfy {
                $0.capability.canAct == first.canAct
                    && $0.capability.isSettled == first.isSettled
            }
            return Band(status: status, rows: matching, capability: uniform ? first : nil)
        }
    }

    /// One value that changes whenever the screen is taken over — a push, or
    /// any of its sheets and covers. Six separate `.onChange` modifiers said
    /// the same thing and tipped the body over the type-checker's budget.
    private var overlayFingerprint: Int {
        var hasher = Hasher()
        hasher.combine(path)
        hasher.combine(showSettings)
        hasher.combine(diffRoute)
        hasher.combine(showLinkDetail)
        hasher.combine(capabilityReason != nil)
        hasher.combine(showDeck)
        return hasher.finalize()
    }

    private func bandView(_ band: Band) -> some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            bandHeader(band)
            CCCard(padding: 0, border: bandBorder(band.status)) {
                VStack(spacing: 0) {
                    ForEach(Array(band.rows.enumerated()), id: \.element.id) { index, row in
                        rowView(row, in: band, isLast: index == band.rows.count - 1)
                    }
                }
            }
            .padding(.horizontal, CC.space.md)
        }
        .padding(.bottom, CC.space.xl)
    }

    /// The only coloured containers on the screen, and the only place the Fleet
    /// spends a hue on something other than a dot.
    private func bandBorder(_ status: FleetStatus) -> Color {
        switch status {
        case .blocked: return CC.color.warning.opacity(0.30)
        case .failed: return CC.color.danger.opacity(0.30)
        default: return CC.color.border
        }
    }


    /// One fleet row, wrapped in its removal gesture.
    ///
    /// Extracted from `bandView` because the nesting defeated the type checker —
    /// a `ForEach` over a wrapper over a row over a trailing closure is more than
    /// it will infer in reasonable time. Splitting it is also the honest shape:
    /// the row and the gesture that can destroy it are two ideas.
    private func rowView(_ row: FleetRow, in band: Band, isLast: Bool) -> some View {
        // **Derived once.** The gesture and the named accessibility action have
        // to agree about whether this row can be removed, and about what happens
        // when it is. Written twice, they drift, and the drift is invisible:
        // VoiceOver offers an action the swipe does not, or the reverse.
        //
        // Removable runs only, and only when the daemon says it accepts the
        // request. `isRemovable` is `lifecycle`, not the derived status — the
        // rule can read Ended without the run being proven exited — plus the
        // unhosted case, where no proof of death can ever exist.
        let remove: (() async -> String?)? =
            row.summary.isRemovable && model.daemonProfile.removesSessions
            ? { await model.removeSession(row.summary).refusal } : nil

        return CCSwipeToRemove(
            isEnabled: remove != nil,
            // "Remove", not "Delete". Claude keeps its own transcript under
            // `~/.claude/projects`, so the conversation survives and
            // `claude --resume` still works; only CodeConnect's record goes.
            title: "Remove",
            action: { await remove?() },
            rowID: row.summary.sessionKey,
            coordinator: swipeCloses
        ) {
            FleetRowView(
                row: row,
                waitingSince: waitingSince(row),
                blocked: blockedCard(row),
                showsCapability: showsCapability(row, in: band),
                separator: !isLast,
                onOpenDiff: { diffRoute = DiffRoute(key: row.summary.sessionKey) },
                onMarkReviewed: { model.states[row.summary.sessionKey]?.markReviewed() },
                onRemove: remove
            ) {
                // "Done, unreviewed" is exactly the moment the diff is what you
                // want: the agent finished and you have not looked at what it did.
                // So the row opens diff-first rather than growing a second control.
                //
                // This note used to end "and emphatically not a swipe, which is the
                // one gesture this app does not teach anywhere". That held while
                // every row action was non-destructive and could be a tap. Removal
                // cannot: it must not sit under the tap that opens the run, and it
                // belongs to ended rows alone. Swipe is now taught in exactly one
                // place for one action, and mirrored as a named accessibility
                // action, because a gesture that is the only route to a feature is
                // not a feature everyone has.
                path.append(
                    SessionRoute(
                        key: row.summary.sessionKey,
                        openDiff: row.status == .doneUnreviewed))
            }
        }
    }
    private func bandHeader(_ band: Band) -> some View {
        // **No dot.** `DONE` never had one, so `BLOCKED`'s was emphasis and not
        // state — a fourth encoding of a fact the band border, the label and
        // the count already carry — and it sat at centre-20 above row dots at
        // centre-36, which is a third vertical edge on a screen that gets
        // exactly two. Deleting it also takes one of the five objects that
        // pulsed in unison; the accessory bar's aggregate is the only one left.
        // **No count either.** The overview line above the list already says
        // `3 running`, and a chip reading `3` beside `RUNNING` a few points below
        // it is the same number twice on one screen. One place owns the aggregate;
        // the band owns the grouping. The rows are still countable by looking at
        // them, which is what a band is for.
        CCSectionHeader(
            band.status.label,
            note: observeNote(band),
            noteAction: band.capability?.reason.map { reason in
                { capabilityReason = CapabilityReason(text: reason) }
            }
        )
        // **The page margin, not the column.** `CCSectionHeader` owns the step
        // from a card's edge out to the content column — 32 for marks, 52 for
        // language — and there is no parameter that says otherwise, so a call
        // site that also pays the 36 lands its label on 88. What this supplies
        // is the 16 the card below it sits on — the band header lives in a
        // full-bleed stack and has to be put on the page margin by hand — and
        // the trailing 16 is what pairs with the component's own to put a
        // note's right edge on 370.
        .padding(.horizontal, CC.space.md)
        .frame(minHeight: CC.space.xxl)
    }

    /// Show the exception, not the rule. A band where everything is
    /// observe-only says so once, in its header; a band where everything can act
    /// says nothing, because control is the promise and its presence is not
    /// news.
    private func observeNote(_ band: Band) -> String? {
        // The rule lives on the model (`CapabilityBadge.bandNote`), pinned by
        // its test: settled observe speaks, control and in-flight unknown say
        // nothing — the latter was the half-second launch flash.
        band.capability?.bandNote
    }

    private func showsCapability(_ row: FleetRow, in band: Band) -> Bool {
        // An unsettled badge is never shown, uniform band or not.
        guard row.capability.isSettled else { return false }
        guard let summary = band.capability else { return true }
        return summary.canAct != row.capability.canAct
    }

    /// The clock a blocked or failed row prints. The oldest card still waiting
    /// on a human is the honest start point; `updatedAt` is the fallback for a
    /// row the daemon says is blocked before its card has reached the stream.
    private func waitingSince(_ row: FleetRow) -> Date {
        // Only the two bands that print a wait clock pay for one. `pendingApprovals`
        // walks a session's whole timeline, and doing that for every row on every
        // body pass is what took the main thread down on a soak-tested daemon.
        guard row.status == .blocked || row.status == .failed else {
            return row.summary.updatedDate
        }
        return model.states[row.summary.sessionKey]?.pendingApprovals
            .map(\.requestedAt).min() ?? row.summary.updatedDate
    }

    /// The card this row is actually held on. Nil — and the line is omitted
    /// rather than guessed — when `blocked_on` arrived before the card itself
    /// did.
    ///
    /// Picked with `DeckOrdering`, not with `max(risk)`: it is the same rule the
    /// Deck ranks by, so the command a row advertises is the command `Review`
    /// opens for that run. A row naming one card while the Deck opens another is
    /// how a reader learns to distrust both.
    private func blockedCard(_ row: FleetRow) -> BlockedCard? {
        guard row.status == .blocked else { return nil }
        let pending = model.states[row.summary.sessionKey]?.pendingApprovals ?? []
        guard let front = DeckOrdering.sort(pending, profile: model.daemonProfile).first else {
            return nil
        }
        let tool = front.card.toolName
        return BlockedCard(
            activity: FleetActivity(
                tool: tool,
                argument: ToolSummary.principalArgument(
                    tool: tool, input: front.card.toolInput)),
            risk: front.assessment(profile: model.daemonProfile).effective,
            truncation: .head)
    }

    /// Dozens of dead rows below the fold is not a glance. Collapsed into one
    /// 52pt footer until asked.
    private func endedFooter(_ band: Band) -> some View {
        HStack(spacing: CC.space.sm) {
            Text("\(band.rows.count) ended")
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
            Spacer(minLength: CC.space.xs)
            CCButton("Show", variant: .ghost, size: .sm) {
                withAnimation(CC.motion.small) { showEnded = true }
            }
        }
        // **The screen's own edge, not the dot column.** `textColumn` is where text
        // sits *beside a status dot*, and this footer has no dot — so it was
        // indented past "Fleet" and the headline for a mark that is not drawn. The
        // same mistake `CCSectionHeader` had; this is `FleetView`'s private copy of
        // that column, which is why fixing the shared component did not reach here.
        .padding(.leading, CC.space.md)
        .padding(.trailing, CC.space.md)
        .frame(minHeight: CC.size.controlLg)
        .padding(.bottom, CC.space.xl)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("\(band.rows.count) ended sessions, collapsed")
    }

    // MARK: States

    private var emptyState: some View {
        CCEmptyState(
            glyph: "terminal",
            title: "No agents running",
            message: "Start one on the Mac:"
        ) {
            CCMonoBlock("codeconnect claude")
                .frame(maxWidth: 280)
                .padding(.top, CC.space.xxs)
        }
        .padding(.horizontal, CC.space.md)
        .padding(.top, CC.space.xxl)
    }

    /// A shape, a sentence, and a ticking counter. No shimmer: a stalled link
    /// must not look busy.
    private var loadingState: some View {
        VStack(alignment: .leading, spacing: CC.rhythm.sections) {
            ForEach(0..<2, id: \.self) { placeholderBand in
                VStack(alignment: .leading, spacing: CC.space.xs) {
                    CCSkeleton(width: 74, height: 11, radius: CC.radius.sm, relativeTo: .caption)
                        .padding(.leading, textColumn)
                    CCCard(padding: 0) {
                        VStack(spacing: 0) {
                            ForEach(0..<2, id: \.self) { placeholderRow in
                                CCSkeletonRow(shape: .fleetRow)
                                if placeholderRow == 0 { CCHairline() }
                            }
                        }
                    }
                    .padding(.horizontal, CC.space.md)
                }
                .accessibilityHidden(true)
                .id(placeholderBand)
            }

            FleetWaitingNotice(since: appearedAt)
                .padding(.horizontal, CC.space.md)
        }
    }

    // MARK: Deck

    /// The accessory bar that rises when decisions are pending, and the door to
    /// the cross-fleet queue behind it.
    ///
    /// `waiting` is the *fleet's* count, handed down rather than recomputed:
    /// the bar and the headline say the same sentence, so they must not each
    /// arrive at the number their own hand happens to hold.
    @ViewBuilder
    private func deckBar(_ pending: [ApprovalItem], waiting: Int) -> some View {
        if let top = pending.first {
            // The bar counts a wait in seconds, so it is one of the few things
            // here that has earned a tick. It owns that read rather than
            // charging it to the list above it.
            FleetDeckBar(
                count: waiting,
                topPlace: place(for: top),
                topCommand: command(for: top),
                waitingSince: top.requestedAt
            ) {
                deckStartsAt = nil
                showDeck = true
            }
        }
    }

    /// Where the card `Review` actually opens comes from.
    ///
    /// `pending` is urgency-ranked, so `first` is the riskiest — under the old
    /// age-first order this line named `app-3 · Read` while a `git push --force`
    /// sat above it in the queue, and the one detail the product volunteered at
    /// 2am was its least important item.
    private func place(for approval: ApprovalItem) -> String {
        model.runLabel(for: approval.sessionKey).inline
    }

    /// What that card wants to run. The command rather than the tool name, for
    /// the same reason the row carries it: `Bash` is not a fact.
    ///
    /// Handed to the bar separately from the place, because the two were one
    /// string and the string was middle-truncated — see `subtitleLine`.
    private func command(for approval: ApprovalItem) -> String {
        ToolSummary.principalArgument(
            tool: approval.card.toolName, input: approval.card.toolInput)?.firstLine
            ?? approval.card.toolName
    }

    /// Dismisses everything Fleet itself can present.
    ///
    /// One place, so a new sheet added to this screen has exactly one list to
    /// join — rather than being forgotten by whichever call site happened to
    /// enumerate the others.
    private func clearPresentations() {
        showSettings = false
        showLinkDetail = false
        capabilityReason = nil
        diffRoute = nil
        // The Deck too. A `.fleet` route arriving while the Deck is up must
        // dismiss it — that is the whole point of the route — and the `.deck`
        // route re-opens it immediately afterwards, so clearing it here costs
        // that path nothing.
        showDeck = false
        deckStartsAt = nil
        path = []
    }

    /// A deep link aimed at the Deck opens it here; anything session-shaped is
    /// pushed onto the stack and handled by the detail view.
    private func consumeDeepLink() {
        switch model.pendingDeepLink {
        case .fleet:
            _ = model.consumeDeepLink()
            clearPresentations()
        case .deck(let requestID):
            _ = model.consumeDeepLink()
            // **Everything on top goes first.** A warm app may be showing
            // settings, link health, a capability sheet, a diff, or a session —
            // and a Deck presented underneath one of those is a Deck the reader
            // cannot see. A tap has to land where it says it lands.
            clearPresentations()
            deckStartsAt = requestID
            showDeck = true
        case .session(let reference, _), .diff(let reference):
            // Leave the link in place: pushing the route makes the detail view
            // appear, and it consumes the link itself so the two cannot both
            // act on it. A reference that resolves to no known run is pushed
            // as it stands, so the detail view can say that plainly rather than
            // the tap doing nothing at all.
            let key = model.resolveSessionKey(reference: reference) ?? reference
            if path.last?.key != key {
                path = [SessionRoute(key: key)]
            }
        case .none:
            break
        }
    }
}

/// A reason, made presentable. `.sheet(item:)` needs identity and a sentence is
/// its own identity here — two bands with the same limitation want the same
/// sheet.
private struct CapabilityReason: Identifiable {
    let text: String
    var id: String { text }
}

/// The scroll offset the inline title fades against.
private struct FleetScrollKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}

// MARK: - The screen's two left edges

/// A screen gets exactly two vertical edges, and this is where they are written
/// down.
///
/// A `CCRow` derives them: the card sits 16 from the screen, the row pads 16
/// inside it, so the gutter starts at **32**; the dot is 8 wide and the row's
/// own gap is 12, so text starts at **52**. Anything drawn outside a row — a
/// band header, an `ENDED` footer, a loading skeleton — has to land on the same
/// two numbers by hand, and four different guesses at it is how one screen ends
/// up with four content columns.
enum ContentColumn {
    /// Where a dot or a glyph sits: 32, out to 40 at reading sizes.
    static let gutter: CGFloat = CC.space.xxl
    /// The 12pt gap `CCRow` puts between its leading slot and its text.
    static let gap: CGFloat = CC.space.sm

    /// Where text starts, given the *scaled* width of the dot in the gutter.
    ///
    /// A function rather than a constant because the column moves with Dynamic
    /// Type — `CCStatusDot` scales, so a header pinned to a literal 52 sat 6pt
    /// left of the titles beneath it at AX5, which is a third vertical edge
    /// arriving by accident at exactly the size the two are hardest to hold.
    /// Shared with the accessory bar so the bar's own dot and text land on
    /// the columns the rows it summarises are using, at every size.
    static func text(dot: CGFloat) -> CGFloat {
        gutter + min(dot, CC.size.dot * CC.size.dotMaxScale) + gap
    }

    /// The width of the gutter column itself — what a mark gets before the text
    /// column starts.
    static func gutterWidth(dot: CGFloat) -> CGFloat { text(dot: dot) - gutter }
}

// MARK: - What a blocked row is held on

/// Enough to triage a blocked agent **without opening anything**: the tool, the
/// argument that identifies the call, and the class the gate will be set at.
///
/// The absence of the argument was the fleet's defining failure. A row spent
/// nine visual elements on four facts and omitted the only one that decides
/// which row you open first — you could not tell from the fleet that `app-1`
/// wanted to run `git push --force origin main`, only that three things needed
/// you, which is what the notification had already said.
struct BlockedCard: Equatable {
    /// The tool and its argument, in the shape every band now uses.
    let activity: FleetActivity
    let risk: RiskClass
    /// **Which end of the argument identifies it**, for the case where even a
    /// wrapped line runs out.
    ///
    /// `.head` on both, and for the same reason: what a preview carries is the
    /// *object*, because the prose label beside it already names the verb.
    /// `…/Sources/Feature.swift` identifies a file and `/Users/dev/…` does not;
    /// `…--force origin main` says which branch is about to be overwritten and
    /// `git push --force …` — the version that shipped — does not, which is how
    /// a row spent 90pt of empty space hiding the difference between `main` and
    /// `master`. `.middle` stays available for a string whose two ends identify
    /// it together.
    let truncation: CCMonoTruncation
}

// MARK: - Row

/// **Five elements, four of them facts, and it adds the one that was missing.**
///
/// The row this replaces spent nine visual elements on four facts: an amber dot,
/// the place, an amber clock, `needs you: Bash`, `fx-1`, a `HIGH` chip, `HELD
/// FOR YOU`, an amber card border and an amber band header. Four of those said
/// *blocked* and nothing else, and the one fact that lets a fleet be triaged —
/// what the agent wants to run — was not on the screen at all.
///
/// What is left:
///
/// ```
/// app-1                                          HIGH   1m03s
/// Bash · git push --force origin main
/// ```
///
/// Two lines, which is exactly the 76pt row: `16 + 22 + 4 + 18 + 16`. The third
/// line comes back only for a run that needs disambiguating from a namesake, a
/// session whose capability disagrees with its band, or a run holding more than
/// one decision — a third line has to carry something the two above it do not.
// MARK: - The three views that are allowed to read the clock

/// **The one thing on this screen that has to change every second.**
///
/// How long ago the Mac last spoke is the screen's whole warrant, so the pill
/// ticks — and it is also the smallest thing on the screen. Read from
/// `FleetView.body`, that second was charged to the entire 45-session list,
/// which was rebuilt and re-sorted to redraw two characters in the toolbar. The
/// read now lives with the only view whose output depends on it.
private struct LinkPill: View {
    @Environment(AppModel.self) private var model
    let action: () -> Void

    var body: some View {
        // Nothing at all in the sample fleet. The pill's whole subject is how
        // long ago a Mac last spoke, and a control reporting on a link that does
        // not exist is worse than an empty slot however honestly it renders.
        if !model.sampleFleetActive {
            CCFreshnessPill(health: model.linkHealth, action: action)
        }
    }
}

/// **The banner, clocked by itself.**
///
/// It reads `linkHealth` to decide what to say and whether to say anything, and
/// `linkHealth` is derived from `now` — so hoisting this read into
/// `FleetView.body` put a one-second heartbeat under the whole fleet. The
/// ladder, the two-second grace on a reconnect, and the compound cached message
/// are unchanged; only the owner of the read moved.
private struct FleetBanner: View {
    @Environment(AppModel.self) private var model
    let onSettings: () -> Void

    var body: some View {
        Group {
            let candidates = bannerCandidates
            if candidates.contains(where: { $0 != nil }) {
                CCBannerSlot(candidates)
                    .padding(.horizontal, CC.space.md)
                    .padding(.bottom, CC.space.xl)
            }
        }
    }

    /// The dial clock is the connection's own (`DaemonConnection.connectingSince`,
    /// maintained where `phase` is written). It used to be `@State` here, which
    /// meant any re-identity of this view — navigation, a sheet — restarted the
    /// grace and could re-flash a banner mid-connect.

    /// **The link and the cache are not alternatives, so they do not compete.**
    ///
    /// The ladder is right and the composition was wrong. `offline` outranks
    /// `cached`, so on a cold launch off the disk the screen rendered `OFFLINE
    /// — Could not connect to the server… retrying in 7s.` and the cached
    /// notice never drew at all: **no cache age anywhere on the screen**, not in
    /// the banner, not in the pill, not on a row — while two wait clocks read
    /// `2m10s` and `5m40s` in `warning` and kept incrementing off data read from
    /// a file. A reader at 2am sees an amber `5m40s` and cannot tell it from a
    /// live one.
    ///
    /// The two facts are merged into one candidate before the slot sees them,
    /// rather than letting `cached` survive as a second banner: "one banner,
    /// ever" is the rule the slot exists to enforce, and a compound state is
    /// still one state. The link keeps the classification — it is the one with
    /// a `Retry` — and the cache stamp leads the message, because *how old this
    /// is* is what decides how much of the screen to believe. The clocks are
    /// untouched: the age of the card is still true, it was the age of the
    /// observation that was missing.
    private var bannerCandidates: [CCBannerItem?] {
        // Permanently, and at the top: the sample fleet is the whole context for
        // everything under it, and it is the only state here that a reader can
        // leave rather than wait out.
        if model.sampleFleetActive {
            return [.sampleFleet(onLeave: { model.stopSampleFleet() })]
        }
        let link = model.pairing.isPaired || model.fixturesActive ? linkBannerItem : nil
        let stamp = cachedStamp
        // The decision is a value (`FleetFreshness.bannerChoice`) so the rule is
        // testable apart from this view; what follows only renders the choice.
        switch FleetFreshness.bannerChoice(
            hasLink: link != nil, hasStamp: stamp != nil, earned: cachedBannerItem != nil)
        {
        case .compound:
            guard let link, let stamp else { return [] }
            return [
                CCBannerItem(
                    link.priority,
                    title: link.title,
                    message: FleetFreshness.message(stamp: stamp, linkDetail: link.message),
                    // The louder of the two. A neutral `Offline` over stale
                    // numbers undersells what the reader is looking at; a
                    // `danger` link stays `danger`.
                    tone: link.tone == .neutral || link.tone == .info ? .warning : link.tone,
                    icon: link.icon,
                    actionTitle: link.actionTitle,
                    action: link.action)
            ]
        case .link: return [link]
        case .cachedOnly: return [cachedBannerItem]
        case .none: return []
        }
    }

    /// The sentence that says how old the fleet on screen is, or `nil` when it
    /// came off the wire. Shared by the standalone cached banner and by the
    /// compound one, so the two cannot state the age differently.
    private var cachedStamp: String? {
        FleetFreshness.stamp(
            cachedAt: model.fleetCachedAt, hasLiveFleet: model.hasLiveFleet, now: model.now)
    }

    private var linkBannerItem: CCBannerItem? {
        // A banner that flashes on every ordinary reconnect is noise; the link
        // gets the launch grace to sort itself out before the screen says
        // anything. The shared constant, so this grace and the cached banner's
        // cannot drift apart. Redials never reach this branch: `evaluate`
        // keeps them at `.offline` with the standing failure, which is what
        // stopped every retry from blanking the banner.
        if model.linkHealth.level == .connecting {
            guard let since = model.connection.connectingSince,
                model.now.timeIntervalSince(since) >= FleetFreshness.launchGrace
            else {
                return nil
            }
        }
        return model.linkHealth.ccBannerItem(
            onRetry: { model.connection.retryNow() },
            onSettings: onSettings,
            onTailscale: { TailscaleAssist.open() })
    }

    private var cachedBannerItem: CCBannerItem? {
        guard let cachedAt = model.fleetCachedAt, !model.hasLiveFleet else { return nil }
        // Earned, not instant. This banner used to render in the half-second
        // between the cache painting the screen and the first live frame
        // replacing it — an amber flash on every healthy launch, filling
        // exactly the silence the link banner's grace above holds open.
        // "Nothing live has arrived this launch" is only worth saying once the
        // launch has had a fair chance to deliver.
        guard
            FleetFreshness.cachedBannerEarned(
                restoredAt: model.fleetCacheRestoredAt,
                connectingSince: model.connection.connectingSince,
                now: model.now)
        else { return nil }
        return CCBannerItem(
            .cached,
            title: "Last known state, \(Format.age(since: cachedAt, now: model.now)) old",
            message: "Nothing live has arrived this launch.",
            tone: .warning,
            icon: "clock.arrow.circlepath",
            actionTitle: "Retry",
            action: { model.connection.retryNow() })
    }
}

/// The accessory bar, clocked by itself.
///
/// It prints the oldest wait in seconds and quotes the link's own reason for
/// disabling `Review`, so it genuinely does change every second while a card is
/// waiting. That is one view at the bottom of the screen; it was costing a
/// rebuild of every row above it.
private struct FleetDeckBar: View {
    @Environment(AppModel.self) private var model
    let count: Int
    let topPlace: String
    let topCommand: String
    let waitingSince: Date
    let onOpen: () -> Void

    var body: some View {
        DeckAccessoryBar(
            count: count,
            topPlace: topPlace,
            topCommand: topCommand,
            waitingSince: waitingSince,
            now: model.now,
            // The link's own per-level sentence. A hard-coded "Link stale"
            // reported a *rejected token* as a stale link, which sends the
            // reader to the wrong screen.
            blockedReason: model.actionsBlockedReason,
            open: onOpen)
    }
}

/// The loading state's elapsed counter. An honest counter with a scale beats a
/// spinner, and it is the only thing in that state that moves — so it is the
/// only thing that needs the clock.
private struct FleetWaitingNotice: View {
    @Environment(AppModel.self) private var model
    let since: Date

    var body: some View {
        CCWaitingNotice(elapsed: model.now.timeIntervalSince(since)) {
            model.connection.retryNow()
        }
    }
}

struct FleetRowView: View {
    let row: FleetRow
    /// When the oldest still-unanswered card in this run was raised. The clock
    /// measures the *oldest* wait; the line below names the card that will be
    /// answered first. They are different questions and both are honest.
    let waitingSince: Date
    /// The card the run is held on, when it is held on one.
    let blocked: BlockedCard?
    let showsCapability: Bool
    let separator: Bool
    let onOpenDiff: () -> Void
    let onMarkReviewed: () -> Void
    /// Present only on a row that can actually be removed, so VoiceOver never
    /// announces an action that would be refused. A gesture cannot be the only
    /// route to a feature — that is the rule `CCDiffPrimitives` writes down for
    /// context menus and it applies with more force to a swipe, which is
    /// invisible and unreachable by Switch Control.
    /// Returns `nil` when the run was removed, or the reason it was not.
    var onRemove: (() async -> String?)?
    let action: () -> Void

    @Environment(\.dynamicTypeSize) private var typeSize
    /// The shared width every tool label is drawn into, so the commands beside
    /// them share one left edge. Zero until the first pass has measured.
    @Environment(\.ccToolColumn) private var toolColumn

    /// **This row's own clock, ticking at the rate this row can actually show.**
    ///
    /// It used to be handed `AppModel.now`, which advances every second. Because
    /// `now` was a stored property, every row in the fleet became a new value
    /// once a second and re-rendered — and because `FleetView.body` read the
    /// same clock to build them, the whole 45-session list was rebuilt and
    /// re-sorted first. All of it to redraw strings like `21h` that change once
    /// an hour. Measured on the owner's fleet, untouched: 14.2ms of main-thread
    /// work per second, in one burst, against an 8.3ms frame.
    ///
    /// Nothing here is slower or less true. A row that is *waiting on a human*
    /// still ticks every second, because `CCWaitClock` prints seconds and that
    /// number really is changing. A row that last spoke 21 hours ago sleeps for
    /// an hour, because that is when its string stops being true.
    ///
    @State private var lastTick = Date()

    /// The clock this row renders with.
    ///
    /// Two things are load-bearing and both were learned by measurement:
    ///
    /// **`lastTick` is read, not just written.** A `@State` the body never looks
    /// at does not invalidate the view — the tick fired on schedule 31 times and
    /// the rows never redrew a single age. The read is the dependency.
    ///
    /// **The value returned is `Date()`, not the stored tick.** A row that has
    /// slept for an hour holds an hour-old stamp, and when a fleet refresh then
    /// moves its `updated_at` the row would measure a three-minute-old fact
    /// against that stale clock and print `0s` — the app claiming something is
    /// newer than it is, which is the one failure this screen exists to prevent.
    ///
    /// Both rules live in `AgeTick.renderTime`, where they are pinned by test,
    /// rather than in the three views that would otherwise each restate them.
    private var now: Date { AgeTick.renderTime(lastTick: lastTick) }

    private var isBlockedOrFailed: Bool { row.status == .blocked || row.status == .failed }

    /// The one age this row draws, and the resolution it draws it at. Its
    /// spoken age is bucketed no finer than its printed one, so keeping the
    /// printed one true keeps VoiceOver true with it.
    private var clock: AgeClock {
        isBlockedOrFailed
            ? AgeClock(since: waitingSince, scale: .clock)
            : AgeClock(since: row.lastEventAt ?? row.summary.updatedDate, scale: .age)
    }

    var body: some View {
        CCRow(
            // The place, always. A fleet is a set of places, and the place is
            // the stable, scannable anchor — never the AI title, which is
            // sometimes a sentence and sometimes a folder name.
            placeName,
            // A row's activity lives in `meta`, where the tool can be prose and
            // the command it wants to run can be monospace. What stays here is
            // the daemon's own *sentence* — a user message, an agent reply, a
            // notice — which has no argument to set as code.
            subtitle: activity == nil ? row.subtitle : nil,
            // **From the front.** The title is a project — a directory
            // somebody named — and a directory is recognised by how it starts.
            // Trimming the middle of a long one returns
            // `platform-s…nciliation…`, two elisions deep and readable as
            // neither name; trimming the tail returns the beginning of the word
            // the reader is looking for. Runs that share a project are told
            // apart on the line below, not by the shape of this one.
            titleTruncation: .tail,
            subtitleLineLimit: 1,
            showsChevron: false,
            separator: separator,
            density: .comfortable,
            isDimmed: row.status == .ended,
            accessibilityLabelText: accessibilityLabel,
            action: action
        ) {
            gutter
        } trailing: {
            titleTrailing
        } meta: {
            metaBlock
        }
        .accessibilityIdentifier("session-\(row.summary.sessionKey)")
        // The class as a *property*, so it survives any future label override —
        // which is exactly how it went missing the first time.
        .accessibilityValue(blocked.map { "Risk \($0.risk.label)" } ?? "")
        // The row's four actions, as **named accessibility actions** rather than
        // a `.contextMenu`. One change fixes two things: a long press is
        // invisible and unreachable by Switch Control (a standing rule in this
        // codebase, written down at `CCDiffPrimitives`), and a
        // `UIContextMenuInteraction` on every one of two dozen rows is real
        // accessibility-snapshot payload on a tree that was collapsed to one
        // element per row precisely to keep it cheap.
        .accessibilityAction(named: "Copy working directory") {
            CCPasteboard.copy(row.summary.cwd)
        }
        .accessibilityAction(named: "Open diff", onOpenDiff)
        .accessibilityAction(named: "Mark reviewed", onMarkReviewed)
        .accessibilityActions {
            if let onRemove {
                Button("Remove from CodeConnect") {
                    Task {
                        // The swipe shows a refusal in the button it revealed.
                        // There is no button here — the row either disappears or
                        // it does not — so the reason has to be spoken, or a
                        // VoiceOver user gets silence and an unchanged list.
                        if let refusal = await onRemove() {
                            UIAccessibility.post(notification: .announcement, argument: refusal)
                        }
                    }
                }
            }
        }
        // Restarted whenever the timestamp this row is counting from changes —
        // a new event, or a blocked row's oldest card being answered — because
        // the old deadline was computed from the old anchor.
        .task(id: clock) { await AgeTick.follow(clock) { lastTick = $0 } }
    }

    /// **The dot is gone from the Blocked band**, where it was a fourth encoding
    /// of a state the band header, the band border and the clock all carry — and
    /// where five of them pulsed in unison. It stays where it discriminates:
    /// hollow means *this row came off the disk*, which nothing else on the
    /// screen says.
    ///
    /// The column is 8pt wide either way. A row that drops its dot must not drag
    /// its own title 20pt left of the row above it.
    @ViewBuilder
    private var gutter: some View {
        if let dot = dotColour {
            CCStatusDot(
                color: dot, isHollow: row.cachedAt != nil, pulses: false, accessibilityText: nil)
        } else {
            // The same disc, drawn in nothing. A `Color.clear` in a hard-coded
            // 8pt frame would have been simpler and wrong twice over: the dot
            // scales with Dynamic Type now (`CC.size.dotMaxScale`), so a fixed
            // frame would let it overflow its own column at AX5, and a blank
            // gutter of a different width would drag the row's title 6pt off
            // the column the row above it uses. `CCStatusDot` is the only thing
            // that knows how wide this column is once type size has had its say.
            CCStatusDot(color: .clear, pulses: false)
        }
    }

    /// Nil where the band already says it and nothing else needs saying.
    private var dotColour: Color? {
        if row.cachedAt != nil { return row.status.ccDotColor }
        return isBlockedOrFailed ? nil : row.status.ccDotColor
    }

    @ViewBuilder
    private var titleTrailing: some View {
        if isBlockedOrFailed {
            // The two facts that decide which row you open first, on the line
            // the eye is already on. Stacked at accessibility sizes: measured at
            // AX5, side by side the clock's fixed single line squeezed the badge
            // to about half its width and `HIGH` broke as `HIG` / `H`.
            CCAdaptiveStack(horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xxs) {
                if let blocked { CCBadge(risk: blocked.risk) }
                // The only value on this screen set in `mono` rather than
                // `monoSmall`: the wait is the most important number here.
                CCWaitClock(since: waitingSince, now: now, prefix: nil)
            }
        } else {
            Text(freshness)
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
                .lineLimit(1)
        }
    }

    @ViewBuilder
    private var metaBlock: some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            if let activity {
                activityLine(
                    activity,
                    truncation: blocked?.truncation ?? .head,
                    // The tool name takes full contrast only where the row is
                    // asking for something. A Running row's activity is not
                    // urgent, so it stays one step down and the *face* — prose
                    // against monospace — is what carries the distinction.
                    tone: blocked == nil ? CC.text.secondary : CC.text.primary)
            }
            if showsIdentity || showsCapability || row.blockedCount > 1 { exceptionLine }
        }
    }

    /// What this row is on: the card it is held on where there is one, and
    /// otherwise whatever its last timeline item was.
    private var activity: FleetActivity? { blocked?.activity ?? row.activity }

    /// The 4-second glance's payload. Tool in prose at full contrast, command in
    /// monospace one step down — the same construction the session detail
    /// timeline uses, and which reads instantly there.
    ///
    /// The command is a `CCMonoBlock(inline:)`, which is the kit's one
    /// container-less mono run: this line, the approval preview and the Deck's
    /// accessory bar had each hand-written a `Text(…).ccType(.monoSmall)`,
    /// because the full block draws a raised surface and a copy button — a tenth
    /// element and 22pt of height inside a 76pt row — and one of the three had
    /// drifted into proportional type as a result.
    ///
    /// It gets the row's whole width, because `CCRow`'s `meta` slot now spans
    /// under the badge and the clock rather than beside them.
    private func activityLine(
        _ card: FleetActivity, truncation: CCMonoTruncation, tone: Color
    ) -> some View {
        // Stacked at accessibility sizes. Measured at AX5: side by side, the
        // command started in the middle of the row and wrapped into a three-line
        // column about half the width of the card — `/Users/` / `dev/app/` /
        // `…re.swift`, which is a path nobody can read. Stacked, it gets the
        // whole measure.
        CCAdaptiveStack(
            horizontalSpacing: CC.space.xs, verticalSpacing: 2,
            verticalAlignment: .firstTextBaseline
        ) {
            Text(card.tool)
                .ccType(CC.type.footnote)
                .foregroundStyle(tone)
                .lineLimit(1)
                // One left edge for every command in the list — see
                // `CCToolColumn`. Off at accessibility sizes, where these stack.
                .ccToolLabelColumn(toolColumn, disabled: typeSize.isAccessibilitySize)
                .layoutPriority(1)
            if let argument = card.argument {
                CCMonoBlock(inline: argument, truncation: truncation)
            }
            Spacer(minLength: 0)
        }
    }

    /// The third line, and only what the two above it do not already say.
    private var exceptionLine: some View {
        CCAdaptiveStack(horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xs) {
            if let qualifier = row.label.qualifier {
                Text(verbatim: qualifier)
                    .ccType(CC.type.micro)
                    .foregroundStyle(CC.text.tertiary)
                    .lineLimit(1)
                    .accessibilityHidden(true)
            }
            if showsCapability {
                CCBadge(capability: row.capability)
            }
            if row.blockedCount > 1 {
                Text("+\(row.blockedCount - 1) more")
                    .ccType(CC.type.micro)
                    .foregroundStyle(CC.text.tertiary)
                    .lineLimit(1)
                    .accessibilityHidden(true)
            }
            Spacer(minLength: CC.space.xs)
        }
    }

    // MARK: Content

    /// A qualifier earns the third line only when it is the **only** thing
    /// telling this run from another in the same project — see `RunLabel`,
    /// which withholds it unless it genuinely does.
    private var showsIdentity: Bool { row.label.qualifier != nil }

    /// Always the project, as the daemon resolved it — the same word the card,
    /// the session header and the lock screen use. When the daemon could not
    /// name one, the row says so rather than falling back to the tmux counter,
    /// which is reused by the next run and names nothing.
    private var placeName: String { row.label.project }

    /// Every fact carries its age. The word "cached" never appears here — the
    /// banner owns that fact and the hollow dot carries it per row.
    ///
    /// A session with no events still has one: `updated_at` is when the daemon
    /// last touched the record. Printing `no events` there spent the row's
    /// trailing column on a non-fact, and it was the widest string on the screen.
    private var freshness: String {
        Format.age(since: row.lastEventAt ?? row.summary.updatedDate, now: now)
    }

    /// **One meaningful stop that says everything the row says.**
    ///
    /// `.combine` gathers a row's children into one element and then this label
    /// *replaces* what it gathered — which is correct, because the gathered
    /// version is a shuffled bag of fragments, but it means anything the
    /// children were publishing has to be re-stated here by hand. It was not:
    /// `CCBadge(risk:)` publishes "Risk HIGH. Destructive, credentialed, or
    /// publishes something." and the parent threw it away, so a sighted reader
    /// saw a red chip and a VoiceOver user could not tell a `Read` from a
    /// `git push --force` without opening it. On an app whose subject is risk
    /// triage that was the most consequential accessibility defect in the build.
    ///
    /// Everything the row draws now survives: the class, the command, the
    /// project, and the age in words. Capability is
    /// announced on **every** row regardless of visual suppression — screen
    /// readers read, they do not scan, and the economics are different.
    private var accessibilityLabel: String {
        var parts = [row.label.spoken, row.status.label]
        if let blocked {
            parts.append("risk \(blocked.risk.label)")
            parts.append(blocked.risk.rationale)
        }
        if row.cachedAt != nil { parts.append("from cache") }
        if row.blockedCount > 1 { parts.append("\(row.blockedCount) pending decisions") }
        parts.append(spokenActivity)
        if isBlockedOrFailed {
            parts.append("waiting \(Format.spokenAge(now.timeIntervalSince(waitingSince)))")
        } else if let last = row.lastEventAt {
            parts.append("last activity \(Format.spokenAge(now.timeIntervalSince(last)))")
        }
        // Silent while unsettled: VoiceOver asserting "observe only" during
        // the handshake is the same flash, spoken.
        if row.capability.isSettled {
            parts.append(row.capability.canAct ? "control" : "observe only")
        }
        return parts.joined(separator: ", ")
    }

    /// What the row's second line says, out loud.
    ///
    /// The flat sentence where the row draws one, and the split read back as two
    /// clauses where it draws two — a screen reader has no faces to tell prose
    /// from code with, so the comma is what does the work.
    private var spokenActivity: String {
        guard let activity else { return row.subtitle }
        return [activity.tool, activity.argument].compactMap { $0 }.joined(separator: ", ")
    }
}

// MARK: - Capability sheet

/// The honest expansion of a four-character claim: what observe-only means, and
/// what the Mac would need for control.
struct CapabilitySheet: View {
    /// The daemon's own words for why this band cannot act, handed in by the
    /// band that raised the sheet. Never re-derived here: a sheet that explains
    /// a *different* session's limitation is worse than no sheet.
    let reason: String

    @Environment(\.dismiss) private var dismiss

    var body: some View {
        CCSheetChrome("Observe only", onClose: { dismiss() }) {
            ScrollView {
                VStack(alignment: .leading, spacing: CC.rhythm.sections) {
                    Text(
                        "Answers given on this phone reach an agent only when a supervisor is attached to its tmux session and the daemon says it can deliver them."
                    )
                    .ccType(CC.type.body)
                    .foregroundStyle(CC.text.primary)
                    .fixedSize(horizontal: false, vertical: true)

                    CCSectionHeader("What is missing")
                    Text(reason)
                        .ccType(CC.type.callout)
                        .foregroundStyle(CC.text.secondary)
                        .fixedSize(horizontal: false, vertical: true)

                    CCSectionHeader("What still works")
                    Text(
                        "Everything on this screen is still true. You can read the timeline, open the diff and attach the terminal, you just answer at the Mac."
                    )
                    .ccType(CC.type.callout)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                }
                .padding(CC.space.md)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .background(CC.color.bg)
        }
    }
}

// MARK: - Link health sheet

/// The trust screen, and the only place these facts now exist — Settings
/// deliberately stopped restating them rather than keeping a second copy that
/// could drift.
///
/// Its whole job is to refuse to overclaim. Three rules run through every row:
///
///  * **A value the app cannot measure renders `—` in `textDisabled`, never
///    `0`.** `0` is a measurement. Round-trip time and the never-miss ledger
///    are not measured yet; printing zeroes for them now would
///    be the exact lie this screen exists to disprove.
///  * **Capabilities are reported, not guessed** — including the advertised keys
///    this build has no word for, which are listed verbatim rather than dropped.
///  * **A control that cannot work says why.** The test notification is disabled
///    with the daemon's own reason on a build that advertises `push: false`.
struct LinkHealthSheet: View {
    @Environment(AppModel.self) private var model
    /// Live iOS permission state, read on appearance. `nil` until read.
    @State private var pushPermission: UNAuthorizationStatus?
    @State private var testingPush = false
    /// The last test's outcome sentence, held until the next test.
    @State private var testOutcome: String?
    @Environment(\.dismiss) private var dismiss

    /// Nothing measured yet. Deliberately not `0`, which is a measurement.
    private static let unmeasured = "-"

    var body: some View {
        CCSheetChrome("Link health", onClose: { dismiss() }) {
            ScrollView {
                VStack(alignment: .leading, spacing: CC.rhythm.sections) {
                    hero
                    stats
                    linkSection
                    capabilitySection
                    eventStreamSection
                    fleetSection
                    actions
                }
                .padding(CC.space.md)
                .padding(.bottom, CC.space.xl)
            }
            .background(CC.color.bg)
        }
    }

    /// The word, its age, and — when the link is degraded — the *consequence*
    /// rather than the cause. Causes belong in the LINK card below.
    private var hero: some View {
        HStack(alignment: .top, spacing: CC.space.sm) {
            CCStatusDot(
                color: model.linkHealth.level.ccTone.color,
                size: CCStatusDot.Size.hero.rawValue,
                isHollow: model.linkHealth.level == .offline,
                pulses: model.linkHealth.level == .connecting)
            .padding(.top, CC.space.xs)

            VStack(alignment: .leading, spacing: CC.space.xxs) {
                Text(heroWord)
                    .ccType(CC.type.title)
                    .foregroundStyle(
                        model.linkHealth.actionsEnabled
                            ? CC.text.primary : model.linkHealth.level.ccTone.color)
                Text(
                    "daemon last spoke \(Format.age(since: model.connection.lastContactAt, now: model.now)) ago"
                )
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
                if !model.linkHealth.actionsEnabled {
                    Text("Actions are disabled until the daemon answers.")
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.color.warning)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 0)
        }
        .accessibilityElement(children: .combine)
    }

    private var heroWord: String {
        switch model.linkHealth.level {
        case .live: return "Live"
        case .lagging: return "Lagging"
        case .stale: return "Stale"
        case .connecting: return "Connecting"
        case .offline: return "Offline"
        case .rejected: return "Token rejected"
        }
    }

    /// The product's central promise, expressed as a number, at the top of the
    /// trust screen.
    ///
    /// `MISSED DECISIONS` is `—` and not `0` on purpose: this build has no
    /// never-miss ledger — nothing measures it yet — and a zero
    /// here would be a claim about something nobody counted. Sequence gaps
    /// *are* counted, so that column carries a real number and turns `danger`
    /// when it is not zero.
    private var stats: some View {
        CCStatStrip([
            CCStat(
                "Missed decisions", value: Self.unmeasured,
                spokenValue: "not measured on this build"),
            CCStat(
                "Seq gaps", value: "\(gapCount)",
                tone: gapCount > 0 ? .danger : .neutral),
            CCStat("Reconnects", value: "\(model.connection.reconnectCount)"),
        ])
    }

    /// Sessions whose event log has a discontinuity the app has not seen filled.
    private var gapCount: Int {
        model.states.values.filter { $0.gap != nil }.count
    }

    private var linkSection: some View {
        section("Link") {
            CCFactRow(
                "State", value: model.linkHealth.detail,
                tone: model.linkHealth.level.ccTone, separator: true)
            CCFactRow(
                "Daemon last spoke",
                value: Format.age(since: model.connection.lastContactAt, now: model.now) + " ago",
                separator: true)
            // Not measured on this build. `—`, never `0ms`.
            CCFactRow("Round trip", value: Self.unmeasured, isUnmeasured: true, separator: true)
            CCFactRow(
                "Transport", value: transport, tone: transportTone, separator: true)
            CCFactRow(
                "Address", value: model.pairing.endpoint?.displayAddress ?? Self.unmeasured,
                isUnmeasured: model.pairing.endpoint == nil, separator: true)
            CCFactRow(
                "Protocol",
                value: model.daemonProfile.isConnected
                    ? "v\(model.daemonProfile.protocolVersion).\(model.daemonProfile.protocolMinor)"
                    : Self.unmeasured,
                isUnmeasured: !model.daemonProfile.isConnected,
                separator: true)
            CCFactRow(
                "Daemon clock",
                value: model.connection.serverTime ?? Self.unmeasured,
                isUnmeasured: model.connection.serverTime == nil,
                separator: model.connection.lastErrorMessage != nil)
            if let error = model.connection.lastErrorMessage {
                // The daemon's words, verbatim and monospace. Never paraphrased.
                noteRow(
                    "Last error",
                    text: error,
                    age: model.connection.lastErrorAt.map {
                        Format.age(since: $0, now: model.now) + " ago"
                    })
            }
        }
    }

    /// `tls` says the listener *holds* a certificate; `tls_active` says *this*
    /// connection is encrypted. They are different facts and are shown
    /// separately — the degraded pairing gets the verbatim explanation.
    private var transport: String {
        guard model.daemonProfile.isConnected else { return Self.unmeasured }
        return model.daemonProfile.connectionEncrypted ? "wss (encrypted)" : "ws (plain)"
    }

    private var transportTone: CCTone {
        guard model.daemonProfile.isConnected else { return .neutral }
        if model.daemonProfile.connectionEncrypted { return .success }
        return model.connection.capabilities?.tls == true ? .warning : .neutral
    }

    @ViewBuilder
    private var capabilitySection: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            // Flush: the sheet's own 16pt page margin is already applied to this
            // stack, and `CCSectionHeader` owns the step from there to the
            // content column. A second `CC.space.md` here is how ten headers in
            // this build ended up on x=32 instead of x=52.
            CCSectionHeader("What this daemon can do")
            CCCard(padding: 0) {
                if let capabilities = model.connection.capabilities {
                    VStack(spacing: 0) {
                        capabilityRow("Answer approvals", capabilities.canApproveReliably)
                        capabilityRow("Type into the session", capabilities.sendText)
                        capabilityRow("Screen snapshots", capabilities.capture)
                        capabilityRow("Push notifications", capabilities.push)
                        capabilityRow("Holds a certificate", capabilities.tls)
                        capabilityRow(
                            "This connection encrypted", capabilities.tlsActive,
                            separator: !extraAdvertised.isEmpty)
                        // Everything else it advertised, including keys this
                        // build has no name for. Dropping them would make a
                        // working feature invisible.
                        ForEach(Array(extraAdvertised.enumerated()), id: \.element.name) {
                            index, row in
                            advertisedRow(row, separator: index < extraAdvertised.count - 1)
                        }
                    }
                } else {
                    // Verbatim, and deliberately not "no": an unknown is not a
                    // denial.
                    Text("Unknown - not connected.")
                        .ccType(CC.type.callout)
                        .foregroundStyle(CC.text.secondary)
                        .padding(CC.space.md)
                }
            }
            if model.daemonProfile.connectionEncrypted == false,
                model.connection.capabilities?.tls == true
            {
                Text("The daemon holds a certificate but this connection is not using it.")
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.color.warning)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, CC.space.md)
            }
        }
    }

    /// The advertised keys this build does not have a named row for, sorted.
    private var extraAdvertised: [(name: String, value: String, isFlag: Bool, isOn: Bool)] {
        let named: Set<String> = [
            "approve", "can_approve_reliably", "send_text", "capture", "push", "tls",
            "tls_active", "fail_mode", "answer_path", "hold_secs",
        ]
        return (model.connection.capabilities?.advertisedRows ?? [])
            .filter { !named.contains($0.name) }
    }

    private var eventStreamSection: some View {
        section("Event stream") {
            CCFactRow(
                "Sessions with a gap", value: "\(gapCount)",
                tone: gapCount > 0 ? .danger : .neutral, separator: true)
            CCFactRow(
                "Highest seq seen",
                value: highestSeq.map(String.init) ?? Self.unmeasured,
                isUnmeasured: highestSeq == nil,
                separator: true)
            CCFactRow(
                "Sessions from cache", value: "\(cachedSessionCount)",
                tone: cachedSessionCount > 0 ? .warning : .neutral, separator: false)
        }
    }

    private var highestSeq: UInt64? {
        let seqs = model.states.values.map(\.lastSeq).filter { $0 > 0 }
        return seqs.max()
    }

    private var cachedSessionCount: Int {
        model.states.values.filter { $0.loadedFromCacheAt != nil && !$0.hasLiveData }.count
    }

    private var fleetSection: some View {
        section("Fleet") {
            CCFactRow("Known sessions", value: "\(model.summaries.count)", separator: true)
            CCFactRow(
                "Blocked", value: "\(model.blockedCount)",
                tone: model.blockedCount > 0 ? .warning : .neutral, separator: true)
            CCFactRow(
                "Fleet list",
                value: model.hasLiveFleet
                    ? "live"
                    : "cached \(Format.age(since: model.fleetCachedAt, now: model.now))",
                tone: model.hasLiveFleet ? .success : .warning,
                separator: false)
        }
    }

    private var actions: some View {
        VStack(spacing: CC.rhythm.controls) {
            // Disabled with a reason, never dead — and when nothing disables it,
            // the tap sends one real notification through Apple to this phone:
            // stored token, provider key, APNs, banner, proven end to end.
            CCButton(
                testingPush ? "Sending…" : "Send a test notification",
                variant: .secondary, size: .lg, fullWidth: true,
                disabledReason: CCDisabledReason(pushReason)
            ) {
                guard !testingPush else { return }
                testingPush = true
                testOutcome = nil
                Task {
                    let result = try? await model.connection.testPush()
                    testingPush = false
                    let outcome = Self.sentence(for: result)
                    testOutcome = outcome
                    // Nothing on screen moves on a refusal, and the banner a
                    // success promises is outside the app — either way a
                    // screen-reader user hears the outcome or nothing.
                    UIAccessibility.post(notification: .announcement, argument: outcome)
                }
            }
            if let testOutcome {
                // Held, not flashed — the same rule as the swipe's refusal: a
                // sentence that clears itself is one the reader can miss.
                Text(testOutcome)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            if pushDeniedInSettings {
                // The one rung of the ladder the app cannot fix itself: the
                // user said no in Settings, and only Settings can unsay it.
                CCButton("Open notification settings", variant: .ghost, size: .lg, fullWidth: true)
                {
                    if let url = URL(string: UIApplication.openSettingsURLString) {
                        UIApplication.shared.open(url)
                    }
                }
            }
            CCButton("Reconnect now", variant: .ghost, size: .lg, fullWidth: true) {
                model.connection.retryNow()
            }
            CCButton("Refresh sessions", variant: .ghost, size: .lg, fullWidth: true) {
                model.refreshFleet()
            }
        }
        .task { pushPermission = await model.pushAuthorizationStatus() }
    }

    /// The reader's sentence for each typed answer. `nil` result — a timeout or
    /// a dropped link — is a refusal too, and says so.
    private static func sentence(for result: TestPushResult?) -> String {
        switch result {
        case .accepted(let apnsID):
            let receipt = apnsID.map { " Apple's receipt: \($0)." } ?? ""
            return "Accepted by Apple — the banner on this phone is the proof.\(receipt)"
        case .pushUnconfigured:
            return "The Mac has no APNs key configured, so nothing was sent."
        case .notPairedDevice:
            return "This connection uses the bootstrap token; pair the phone to test push."
        case .noRegisteredToken:
            return "The Mac holds no notification token for this phone yet. Enable notifications, then try again."
        case .rateLimited(let secs):
            return "Tested a moment ago — try again in \(secs)s."
        case .failed(let reason):
            return "The Mac could not send it: \(reason)"
        case .unknown(let status):
            return "The Mac answered “\(status)”, which this build does not know."
        case .none:
            return "No answer from the Mac."
        }
    }

    private var pushDeniedInSettings: Bool { pushPermission == .denied }

    /// The ladder, each rung a fact the reader can act on. Live permission
    /// state, re-read on appearance — the stored boolean goes stale the moment
    /// the user visits Settings.
    private var pushReason: String? {
        guard let capabilities = model.connection.capabilities else {
            return "Not connected. The daemon has not told us what it can do."
        }
        guard capabilities.push else {
            return "This daemon does not send push notifications, so there is nothing to test."
        }
        guard capabilities.testsPush else {
            return "This daemon predates push testing. Update the Mac to prove the doorbell."
        }
        switch pushPermission {
        case .denied:
            return "Notifications are off for CodeConnect in iOS Settings, so a test could not show."
        case .notDetermined:
            return "Notifications have not been requested yet. Pair and allow them first."
        case nil:
            // The live read has not answered yet. A button that is enabled for
            // the first frame and disables itself is a button that lies for a
            // frame; unknown holds it shut instead.
            return "Checking notification permission…"
        default:
            return nil
        }
    }

    // MARK: Row primitives

    private func section<Content: View>(
        _ title: String, @ViewBuilder content: @escaping () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            // Flush — see `capabilitySection`. This helper draws LINK, EVENT
            // STREAM and FLEET, so the doubled inset was three of the eleven.
            CCSectionHeader(title)
            CCCard(padding: 0) {
                VStack(spacing: 0, content: content)
            }
        }
    }

    /// A daemon sentence, wrapped rather than squeezed into a value column.
    ///
    /// A `CCFactRow` with a `detail` body: the daemon's words are verbatim and
    /// monospace, never paraphrased, and they get a whole line rather than a
    /// right-aligned column that would truncate them.
    private func noteRow(_ label: String, text: String, age: String?) -> some View {
        // Named rather than trailing: `value` and `detail` are both single
        // closures, so a bare trailing closure cannot say which slot it is.
        CCFactRow(
            label, age: age, separator: false,
            detail: { CCMonoBlock(text, tone: .danger, isSmall: true) })
    }

    /// **Words, not check/x glyphs** — a glyph makes the reader translate; a
    /// word does not.
    private func capabilityRow(_ title: String, _ enabled: Bool, separator: Bool = true)
        -> some View
    {
        CCFactRow(
            title, separator: separator,
            accessibilityValueText: enabled ? "yes" : "no",
            value: {
                CCBadge(
                    enabled ? "Yes" : "No", tone: enabled ? .success : .neutral,
                    accessibilityText: enabled ? "available" : "not available")
            })
    }

    /// An advertised key this build has never heard of. Printed as it arrived,
    /// in monospace, because the name is the daemon's and not ours to prettify —
    /// which is what `CCFactLabelStyle.key` means.
    @ViewBuilder
    private func advertisedRow(
        _ row: (name: String, value: String, isFlag: Bool, isOn: Bool), separator: Bool
    ) -> some View {
        if row.isFlag {
            CCFactRow(
                row.name, labelStyle: .key, separator: separator,
                accessibilityValueText: row.isOn ? "yes" : "no",
                value: {
                    CCBadge(
                        row.isOn ? "Yes" : "No", tone: row.isOn ? .success : .neutral,
                        accessibilityText: row.isOn ? "available" : "not available")
                })
        } else {
            CCFactRow(row.name, value: row.value, labelStyle: .key, separator: separator)
        }
    }
}



