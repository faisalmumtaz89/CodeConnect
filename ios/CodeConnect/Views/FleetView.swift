import CodeConnectKit
import SwiftUI
import UniformTypeIdentifiers
import UserNotifications

/// Where a navigation route points: one *run*, by its key, never by its tmux
/// name. A route that held `cc-1` would follow the name to whichever run holds
/// it next. Reached from a `codeconnect://` URL or a row tap — a notification
/// names no run, so it never builds one of these.
struct SessionRoute: Hashable {
    let key: String
    /// Set when arriving from the Deck, so the card opens without a second tap.
    var openRequestID: String?
    /// Set for a session this phone just asked a computer to start: the
    /// route shows it starting until it is in the fleet.
    var startRequestID: String?
    /// Opens with the keyboard up in the composer.
    var focusComposer = false
}

/// What the removal question says, and what confirming it does.
///
/// Removal deletes CodeConnect's own record only; the agent's transcript on the
/// Mac is untouched, so the question says so.
private struct PendingRemoval {
    let title: String
    let message: String
    let remove: () -> Void

    @MainActor
    init(row: SessionSummary, model: AppModel, remove: @escaping () -> Void) {
        title = "Remove \(model.runLabel(for: row.sessionKey).project)?"
        message = "\(row.agent.displayName)'s own transcript stays on the computer."
        self.remove = remove
    }

    init(count: Int, remove: @escaping () -> Void) {
        title = "Remove \(count) sessions?"
        message = "Their own transcripts stay on the computer."
        self.remove = remove
    }
}

/// The 2am glance. One list of projects in the owner's own order, every fact
/// stamped with its age, one card per project saying where it is, and the one
/// number that matters — how many agents need you — directly under the title.
///
/// Three rules do most of the work here:
///
///  * **Nothing moves on its own.** A card's place is the owner's
///    (`ProjectOrder`): a status or an event changes what a card says, never
///    where it is or how tall it is, so a tap lands on the card it was aimed
///    at. Only a drag moves a card, and a new project arrives on top.
///  * **Colour is information.** Two borders are coloured: amber round a card
///    with a run waiting for you, red round one with a failed run. Risk is a
///    tag that is brightness, not hue.
///  * **One banner, ever.** Every candidate goes to `CCBannerSlot` and it picks
///    by rank: rejected beats offline beats stale beats cached. Facts that lose
///    still appear per-element: a cached row gets a hollow dot, never the word
///    "cached".
struct FleetView: View {
    /// The machine the app-wide surfaces speak for — see `Machines.linkModel`.
    /// A row, a session or a card is acted on through its own machine's model.
    @Environment(AppModel.self) private var model
    @Environment(Machines.self) private var machines
    @Environment(\.dynamicTypeSize) private var typeSize
    /// Every tool label's width on screen, so every command beside one starts
    /// on the same edge. Settles on the first pass and then stops changing, so
    /// it costs one extra layout and nothing after that.
    @State private var toolLabelWidths: Set<CGFloat> = []
    @State private var path: [SessionRoute] = []
    @State private var showSettings = false
    /// The one-open-swipe-row rules. Rows observe it; this view only calls
    /// methods on it, so a pulse closes a row without re-rendering the fleet.
    @State private var swipeCloses = CCSwipeCloseCoordinator()
    @State private var showDeck = false
    @State private var deckStartsAt: String?
    @State private var diffRoute: DiffRoute?
    /// Expanding `Ended` is deliberately not persisted: a fresh launch is a
    /// fresh glance.
    @State private var showEnded = false
    /// The band's own observe-only reason, set when its header note is tapped.
    @State private var capabilityReason: CapabilityReason?
    /// A removal waiting on its confirmation. One question for the screen,
    /// asked the same way whichever route raised it.
    @State private var pendingRemoval: PendingRemoval?
    /// Choosing ended sessions to remove together, and the ones chosen.
    @State private var selecting = false
    @State private var selected: Set<String> = []
    @State private var batchRunning = false
    /// Why each session the last batch did not remove was kept, by key. The
    /// note is drawn from whichever of them are still ended rows, so it shrinks
    /// as they go and is gone when they are.
    @State private var batchRefused: [String: String] = [:]
    /// Whether the scroll header's title has travelled far enough for the
    /// inline one to take over, so the screen never shows its name twice.
    ///
    /// Deliberately a `Bool` and not a continuous opacity: this view's `body`
    /// re-reads `machines.fleet`, and on a soak-tested daemon that walks two dozen
    /// event logs. A per-frame opacity turned a flick into two dozen full fleet
    /// rebuilds and hung the main thread — measured, on a 24-session daemon.
    /// One state change per direction, cross-faded by the animation, is
    /// indistinguishable and free.
    @State private var titleVisible = false
    /// When this screen first appeared, so the loading state can tick an honest
    /// elapsed counter instead of a spinner with no scale.
    @State private var appearedAt = Date()
    /// The fleet as it was when a chat or the Deck covered it, drawn instead of
    /// the model until it is uncovered. Drawn from this the body reads no
    /// session, so after at most one pass another session's event does not run
    /// it behind the cover. It is let go before the fleet's first pixel is back
    /// — as a swipe-back begins, or as the Deck starts to close — so the reader
    /// never sees it.
    @State private var hidden: FleetScreen?
    /// The project being dragged, while a hold-and-drag is moving it.
    @State private var dragging: ProjectKey?
    /// What the list shows while it is in sight: each project and the runs in
    /// it. A project or a run that arrives or leaves waits here, named by the
    /// header's pill, until the owner lets it in (the pill, a pull, coming back
    /// to the screen), so nothing moves under a finger. Nil, or empty, until
    /// there is a card to hold.
    @State private var held: [ProjectKey: Set<String>]?
    /// The last row of every run the list holds, drawn once the fleet no longer
    /// has it.
    @State private var heldRows: [String: FleetRow] = [:]
    @Environment(\.scenePhase) private var scenePhase
    /// The dot's own Dynamic Type ramp, mirrored so that the text column a band
    /// header lands on is the text column its rows land on.
    ///
    /// Not applied *to* a dot — `CCStatusDot` scales itself and doing it twice
    /// is how a disc bursts its gutter — but to the column a dot defines. A
    /// header pinned to a constant 52 sat 6pt left of the titles beneath it at
    /// AX5, which is a third vertical edge — the screen gets exactly two —
    /// arriving by accident at exactly the size they are hardest to hold.
    @ScaledMetric(relativeTo: .footnote) private var gutterDot: CGFloat = CC.size.dot
    /// Whether the start flow is open, and the start it sent (opened once the
    /// flow has gone).
    @State private var startOpen = false
    @State private var startedRequest: String?
    /// Every paired computer that can start a session.
    private var startContexts: [StartContext] {
        machines.models.filter { $0.connection.capabilities?.startsSessions == true }
            .map { StartContext(model: $0) }
    }
    private var canStart: Bool { !startContexts.isEmpty }

    /// Where every line of text on this screen starts: 52 at reading sizes.
    private var textColumn: CGFloat { ContentColumn.text(dot: gutterDot) }

    private static let scrollSpace = "cc.fleet.scroll"

    var body: some View {
        #if DEBUG
            FleetRowRenders.fleetBody()
        #endif
        let screen = hidden ?? liveScreen()
        return NavigationStack(path: $path) {
            fleet(screen)
        }
        #if DEBUG
            .overlay(alignment: .topLeading) { FleetRowRenderProbe() }
        #endif
    }

    /// The list and everything it answers to, a stage at a time, each stage's
    /// modifiers in the order they apply. One expression, it was more than the
    /// compiler would type-check in reasonable time.
    private func fleet(_ screen: FleetScreen) -> some View {
        let scroll = fleetScroll(screen)
        let held = holdingChanges(scroll, screen)
        let navigated = navigation(held, screen)
        let presented = presentations(navigated, screen)
        return lifecycle(presented)
    }

    private func fleetScroll(_ screen: FleetScreen) -> some View {
        ScrollView {
            // A `VStack`, **not** a `LazyVStack`. Each card and the Ended
            // band is one item holding all its rows, so at the accessibility
            // sizes one item can be several screens tall, and over items that
            // tall the lazy stack never settled its placement: one of the
            // first few drags pinned the main thread for over a minute
            // (`sample`: spinning in `LazySubviewPlacements`), the same
            // failure the timeline had before it became a `List`. The items
            // are eager inside, and the eager stack measured no cost
            // at the largest real fleet — 45 ended runs expanded, default
            // size: expanding 1.59–1.62s lazy against 1.61–1.63s eager,
            // drags alike, idle CPU 12.7–14.4% against 11.6–12.5%.
            VStack(alignment: .leading, spacing: 0) {
                // An empty fleet is one instruction, not a list with no rows:
                // a header above it would name the same emptiness again.
                if !screen.isEmpty || !screen.projects.isEmpty { scrollHeader(screen) }
                content(screen)
            }
            // Whether the fleet is in the window, which is whether any of it
            // can be on screen: the navigation controller takes it out when
            // a chat has covered it and puts it back as a pop or a back
            // swipe begins. Not `onAppear`/`onDisappear`: a chat pushed as
            // the fleet first appears — a link followed at launch — leaves
            // those saying "appeared" until it is gone, and silent after.
            // Back in the window under the Deck — a Deck link followed from
            // a chat pops the chat beneath it — the fleet is still covered.
            .background(
                FleetWindowPresence { inWindow in inWindow && !showDeck ? thaw() : freeze() })
            // The scroll view's width, which the lazy stack took on its own.
            // A `VStack` is as wide as its widest row, and a long MCP tool
            // name makes a row wider than the screen: the scroll view then
            // centred the whole fleet on that row, and every band header
            // sat off the left edge.
            .frame(minWidth: 0, maxWidth: .infinity, alignment: .leading)
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
        .ccCollectsToolColumn(into: $toolLabelWidths)
        .refreshable {
            applyHeld()
            machines.refreshFleet()
        }
    }

    /// What the list holds while it is in sight, kept as the fleet changes.
    private func holdingChanges(_ list: some View, _ screen: FleetScreen) -> some View {
        list
            .onChange(of: scenePhase) { _, phase in
                if phase == .active { applyHeld() }
            }
            // The first fleet, and any while nothing is on screen (no card and
            // no Ended band), is let in at once: there is nothing to move.
            .onChange(of: screen.live, initial: true) { _, live in
                if held == nil || (held?.isEmpty == true && screen.ended == nil) { held = live }
            }
            .onChange(of: screen.rows, initial: true) { _, rows in
                guard let held else { return }
                let keys = Set(held.values.flatMap { $0 })
                for row in rows where keys.contains(row.summary.sessionKey) && heldRows[row.summary.sessionKey] != row {
                    heldRows[row.summary.sessionKey] = row
                }
            }
            // A project seen for the first time keeps the top place it appeared
            // in: written down once, so nothing later can move it.
            .onChange(of: screen.projects.map(\.id), initial: true) { _, keys in
                let saved = machines.settings.projectOrder
                let next = ProjectOrder.recording(keys, into: saved)
                if next != saved { machines.settings.projectOrder = next }
            }
    }

    private func navigation(_ list: some View, _ screen: FleetScreen) -> some View {
        list
            // Opaque `bg` *and* a hard scroll edge, welded together in the kit
            // because either one alone is a different wrong bar.
            .ccNavigationChrome()
            .navigationTitle("Projects")
            .navigationBarTitleDisplayMode(.inline)
            .navigationDestination(for: SessionRoute.self) { route in destination(route) }
            .toolbar { toolbarItems }
            // The bar prints the header's own number, not its own count of the
            // cards it happens to hold — see `FleetCount`.
            .safeAreaInset(edge: .bottom, spacing: 0) { footer(screen) }
    }

    // One chat per session. A link that swaps the chat on top for
    // another session's lands at the same place in the stack, and
    // SwiftUI would otherwise keep the old chat's view — its draft,
    // its open sheets, its timeline wiring — under the new key, and
    // never show it appearing, so the link it came by stays unread.
    @ViewBuilder
    private func destination(_ route: SessionRoute) -> some View {
        if let requestID = route.startRequestID {
            StartingSessionView(
                requestID: requestID,
                onLive: { path = [SessionRoute(key: $0, focusComposer: $1)] },
                onChooseAgain: {
                    path = []
                    Task {
                        try? await Task.sleep(for: .milliseconds(450))
                        openStart()
                    }
                }
            )
            .environment(
                machines.models.first { $0.starter.starts[requestID] != nil } ?? model)
        } else {
            SessionDetailView(route: route)
                .environment(owner(route.key))
                .id(route.key)
        }
    }

    @ToolbarContentBuilder
    private var toolbarItems: some ToolbarContent {
        if canStart {
            ToolbarItem(placement: .topBarLeading) {
                Button {
                    CCHaptic.light.fire()
                    openStart()
                } label: {
                    HStack(spacing: CC.space.xxs) {
                        CCIcon("plus", size: 16)
                        Text("New")
                            .ccType(CC.type.body)
                    }
                    .foregroundStyle(CC.text.primary)
                    .fixedSize()
                    .ccHitTarget()
                }
                .buttonStyle(.plain)
                .accessibilityLabel("New session")
                .accessibilityIdentifier("start-entry")
            }
            .ccPlainToolbarItem()
        }
        ToolbarItem(placement: .principal) {
            // The same word the scroll header carries, arriving only as
            // that header leaves. Two titles at once is the bug this
            // fade exists to prevent.
            Text("Projects")
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
                Text("Settings")
                    .ccType(CC.type.body)
                    .foregroundStyle(CC.text.primary)
                    .ccHitTarget()
            }
            .buttonStyle(.plain)
        }
        .ccPlainToolbarItem()
    }

    private func footer(_ screen: FleetScreen) -> some View {
        VStack(spacing: 0) {
            // The link's banner sits at the foot, over the list, not in
            // it: coming or going it changes how far the list scrolls,
            // never where a card is. At the accessibility sizes it is
            // in the list instead (`content`): pinned there, it and the
            // bar would cover most of the screen. Its own view, and
            // deliberately: it reads the link's age, and read from here
            // that read belonged to the whole fleet.
            if !typeSize.isAccessibilitySize { FleetBanner { showSettings = true } }
            if selecting {
                selectionBar(screen.selectable)
            } else {
                deckBar(screen)
            }
        }
    }

    private func presentations(_ list: some View, _ screen: FleetScreen) -> some View {
        list
            .sheet(isPresented: $startOpen, onDismiss: openStarted) {
                StartStepsSheet(contexts: startContexts, onStarted: started, onClose: { startOpen = false })
                .ccTallSheet()
            }
            .onChange(of: startContexts.map(\.id), initial: true) { _, _ in loadStart() }
            // A row that leaves the fleet, or stops being removable, leaves the
            // selection with it; with nothing left to choose, choosing ends. The
            // note about a batch stays: it is what says what was not removed.
            // Behind a chat the screen holds still, so this runs as it is
            // revealed, against the fleet as it is then.
            .onChange(of: screen.selectable) { _, keys in
                selected.formIntersection(keys)
                if keys.isEmpty {
                    // A question about a selection that just ended has nothing
                    // left to answer for.
                    if selecting { pendingRemoval = nil }
                    selecting = false
                    selected = []
                }
            }
            // Leaving the screen — a push, or any sheet — stands the open
            // swipe row down. A destructive control must not sit armed under
            // whatever the user comes back to.
            .onChange(of: overlayFingerprint) { swipeCloses.closeAll() }
            .sheet(isPresented: $showSettings, onDismiss: applyHeld) {
                PairingView().environment(machines.current)
            }
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
                    .environment(owner(route.key))
            }
            // An alert, not a confirmation dialog: on iOS 26 a dialog pops out of
            // whatever it is attached to, and this one is attached to the screen.
            .alert(
                pendingRemoval?.title ?? "",
                isPresented: Binding(
                    get: { pendingRemoval != nil }, set: { if !$0 { pendingRemoval = nil } }),
                presenting: pendingRemoval
            ) { removal in
                Button("Remove", role: .destructive, action: removal.remove)
            } message: { removal in
                Text(removal.message)
            }
    }

    private func lifecycle(_ list: some View) -> some View {
        list
            .onChange(of: machines.pendingDeepLink) { _, _ in consumeDeepLink() }
            // A name that waited out a catch-up is resolved at its release.
            .onChange(of: machines.isCatchingUp) { _, _ in consumeDeepLink() }
            .onAppear {
                appearedAt = Date()
                consumeDeepLink()
            }
            // Held from the moment a chat is pushed, as well as when the fleet
            // leaves the window: a chat pushed as the fleet first appears never
            // brings the fleet into the window, so it never leaves it either.
            // The sheets leave the fleet in the window and in sight, and live.
            .onChange(of: path.isEmpty) { _, empty in
                if !empty { freeze() }
            }
            // The Deck covers all of the fleet but leaves it in the window, so
            // the window says nothing: the cover itself holds the fleet, and
            // lets it go as it closes — unless a chat is on top, which keeps
            // the fleet out of the window and held.
            .onChange(of: showDeck) { _, shown in
                if shown {
                    freeze()
                } else if path.isEmpty {
                    thaw()
                }
            }
    }

    // MARK: Screen

    /// Everything the fleet draws from the model, read in one place —
    /// `liveScreen()` — so that drawing it reads nothing. Actions still resolve
    /// the model when they run.
    private struct FleetScreen {
        let rows: [FleetRow]
        /// The runs the list holds, one card per working directory, in the
        /// owner's order.
        let projects: [Project]
        /// Which runs make up which project in the fleet as it is now.
        let live: [ProjectKey: Set<String>]
        /// What the list is holding back, or nil when it holds nothing.
        let change: HeldChange?
        /// The runs that have ended, as one band; nil with none.
        let ended: Band?
        /// The rows a selection can hold, in fleet order: ended, and removable.
        let selectable: [String]
        /// The count under the title, which the Deck bar prints too.
        let decisions: Int
        /// How many waiting cards sit at each risk, for the Deck bar; empty until
        /// a card has arrived, and then the bar rises.
        let risks: [RiskClass: Int]
        let hasLiveFleet: Bool

        /// The daemon has said there is no session, as opposed to not having
        /// said anything yet.
        var isEmpty: Bool { rows.isEmpty && hasLiveFleet }
    }

    /// Hold the fleet as it is now, unless it is already held.
    private func freeze() {
        if hidden == nil { hidden = liveScreen() }
    }

    /// Draw from the model again. Unanimated: what changed behind the chat is
    /// already true, not news arriving now — a count that rolls from its old
    /// value as the fleet slides back in is a count shown stale.
    private func thaw() {
        guard hidden != nil else { return }
        var unanimated = Transaction()
        unanimated.disablesAnimations = true
        withTransaction(unanimated) {
            hidden = nil
            applyHeld()
        }
    }

    /// Let in what the list has held back: the arrivals take their places and
    /// what left goes.
    private func applyHeld() {
        let rows = machines.fleet
        let membership = Self.membership(of: rows)
        let keys = Set(membership.values.flatMap { $0 })
        held = membership
        heldRows = Dictionary(
            rows.filter { keys.contains($0.summary.sessionKey) }.map { ($0.summary.sessionKey, $0) },
            uniquingKeysWith: { first, _ in first })
    }

    /// Which runs make up which project, from the runs that have not ended.
    private static func membership(of rows: [FleetRow]) -> [ProjectKey: Set<String>] {
        rows.filter { $0.status != .ended }.reduce(into: [:]) { held, row in
            held[ProjectKey(machineID: row.machineID, folder: row.summary.shownCwd), default: []]
                .insert(row.summary.sessionKey)
        }
    }

    /// What the list holds back: in plain words, "1 new project · 1 ended", and
    /// as a count, "2 changes", for when the words do not fit on one line.
    struct HeldChange {
        let words: String
        let count: String
    }

    private static func change(
        from shown: [ProjectKey: Set<String>], to live: [ProjectKey: Set<String>]
    ) -> HeldChange? {
        let newProjects = live.keys.filter { shown[$0] == nil }.count
        var newRuns = 0
        var ended = 0
        for (key, runs) in shown {
            let now = live[key] ?? []
            newRuns += now.subtracting(runs).count
            ended += runs.subtracting(now).count
        }
        let words = [
            newProjects > 0 ? "\(newProjects) new \(newProjects == 1 ? "project" : "projects")" : nil,
            newRuns > 0 ? "\(newRuns) new \(newRuns == 1 ? "agent" : "agents")" : nil,
            ended > 0 ? "\(ended) ended" : nil,
        ].compactMap { $0 }
        let total = newProjects + newRuns + ended
        return words.isEmpty
            ? nil
            : HeldChange(words: words.joined(separator: " · "), count: "\(total) \(total == 1 ? "change" : "changes")")
    }

    /// The fleet as the model has it now.
    private func liveScreen() -> FleetScreen {
        #if DEBUG
            FleetRowRenders.fleetRead()
        #endif
        // One sort per screen: `machines.fleet` re-sorts on every access. Nothing
        // on this screen ticks it any more — see `FleetRowView.now` — so a body
        // pass now means the fleet itself changed, or a Stop cooldown is
        // counting down (`stopBlockedReason`).
        let rows = machines.fleet
        // Read once for the same reason, and for a sharper one: `machines.deck`
        // flat-maps `pendingApprovals` across every session, which walks every
        // event log the app holds. Read inside the `safeAreaInset` builder it was
        // re-evaluated on every *layout* pass — so one flick on a 24-session
        // daemon walked two dozen logs a frame and took the main thread down for
        // thirty seconds. Measured.
        let deck = machines.deck
        let live = Self.membership(of: rows)
        let shown = held ?? live
        let inCards = Set(shown.values.flatMap { $0 })
        return FleetScreen(
            rows: rows,
            // Everything each row draws, read here and nowhere below: a model
            // read inside a row's own builders would redraw that row on any
            // session's event. See `FleetRowCell`.
            projects: projects(of: rows, shown: shown),
            live: live,
            change: Self.change(from: shown, to: live),
            // A run the list still holds in its card is not listed again here.
            ended: bands(of: rows.filter { $0.status == .ended && !inCards.contains($0.summary.sessionKey) })
                .first,
            selectable: selectableKeys(rows),
            decisions: FleetCount.decisions(in: rows),
            // Each card at the risk its own card shows.
            risks: deck.reduce(into: [:]) {
                $0[$1.assessment(profile: machines.profile(forSession: $1.sessionKey)).effective, default: 0] += 1
            },
            hasLiveFleet: machines.hasLiveFleet)
    }

    /// The model of the machine this session is on.
    private func owner(_ key: String) -> AppModel {
        machines.model(forSession: key) ?? model
    }

    private func owner(of row: FleetRow) -> AppModel {
        machines.model(for: row.machineID) ?? model
    }

    // MARK: Header

    /// **A large, calm title, and the one sentence that may not be calm.**
    ///
    /// `Projects` in the iOS large-title place, then the shape of the work in one
    /// quiet line. When decisions wait, their count comes between the two, in the
    /// warning hue the accessory bar's dot uses, so it is the first thing read
    /// after the screen's own name and it cannot be mistaken for the summary. The
    /// block is as tall with the count as without it.
    private func scrollHeader(_ screen: FleetScreen) -> some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            CCAdaptiveStack(horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xs) {
                Text("Projects")
                    .ccType(CC.type.display)
                    .foregroundStyle(CC.text.primary)
                    .accessibilityAddTraits(.isHeader)
                CCAdaptiveSpacer(minLength: CC.space.sm)
                // The pill is always in its place, unseen and inert while the list
                // holds nothing back, and always one line: the whole change where
                // it fits, its count where it does not. So it comes and goes
                // without moving anything.
                ViewThatFits(in: .horizontal) {
                    heldPill(screen.change?.words ?? "Up to date", change: screen.change)
                    heldPill(screen.change?.count ?? "Up to date", change: screen.change)
                }
            }
            // The line is always there, empty while nothing needs a person, so
            // the first decision to arrive moves no card below it.
            let alert = alert(screen)
            Text(alert?.text ?? " ")
                .ccType(CC.type.headline)
                .foregroundStyle(alert?.colour ?? .clear)
                .fixedSize(horizontal: false, vertical: true)
                .contentTransition(.numericText(countsDown: true))
                .accessibilityHidden(alert == nil)
            // One line at reading sizes for the same reason: a count that grows
            // a word must not wrap and push the cards down.
            Text(summaryLine(screen))
                .ccType(CC.type.callout)
                .foregroundStyle(CC.text.secondary)
                .lineLimit(typeSize.isAccessibilitySize ? nil : 1)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, CC.space.md)
        .padding(.top, CC.space.xs)
        .padding(.bottom, CC.space.xl)
    }

    private func heldPill(_ title: String, change: HeldChange?) -> some View {
        CCButton(title, variant: .secondary, size: .sm) {
            withAnimation(CC.motion.small) { applyHeld() }
        }
        .lineLimit(1)
        .opacity(change == nil ? 0 : 1)
        .allowsHitTesting(change != nil)
        .accessibilityHidden(change == nil)
        .accessibilityLabel(change?.words ?? title)
        .accessibilityHint("Shows them in the list")
        .accessibilityIdentifier("fleet-held-changes")
    }

    /// What needs a person: the decisions, in the canonical words — the same
    /// number, from the same function, that the accessory bar prints (see
    /// `FleetCount`) — or failing those the failures.
    private func alert(_ screen: FleetScreen) -> (text: String, colour: Color)? {
        if screen.decisions > 0 {
            return (FleetCount.waitingForYou(screen.decisions), CC.color.warning)
        }
        let failed = screen.rows.filter { $0.status == .failed }.count
        return failed > 0 ? ("\(failed) failed", CC.color.danger) : nil
    }

    /// `4 agents on 2 computers · 1 done`: every live run, the computers they
    /// are on when there are several, and what they are doing — less whatever
    /// the alert above said. A status with nothing in it is omitted rather than
    /// printed as `0`.
    private func summaryLine(_ screen: FleetScreen) -> String {
        guard !screen.projects.isEmpty else {
            return screen.rows.isEmpty ? "Nothing has arrived yet" : "No agents running"
        }
        let live = screen.rows.filter { $0.status != .ended }
        let computers = Set(live.map(\.machineID)).count
        let overview = "\(live.count) \(live.count == 1 ? "agent" : "agents")"
            + (computers > 1 ? " on \(computers) computers" : "")
        let taken: FleetStatus? =
            screen.decisions > 0 ? .blocked : (live.contains { $0.status == .failed } ? .failed : nil)
        let rest = FleetStatus.allCases.compactMap { status -> String? in
            guard status != taken else { return nil }
            let count = live.filter { $0.status == status }.count
            return count > 0 ? "\(count) \(status.label.lowercased())" : nil
        }
        return ([overview] + rest).joined(separator: " · ")
    }

    // MARK: Content

    @ViewBuilder
    private func content(_ screen: FleetScreen) -> some View {
        // At the accessibility sizes the banner scrolls away with the list, as
        // everything there wraps and moves anyway; at reading sizes it is at the
        // foot of the screen (the bottom inset).
        if typeSize.isAccessibilitySize { FleetBanner { showSettings = true } }

        if screen.isEmpty && screen.projects.isEmpty {
            emptyState
        } else if screen.rows.isEmpty && screen.projects.isEmpty {
            loadingState
        } else {
            // One list in the owner's order: a card that starts waiting turns
            // amber where it is, rather than moving to the top under a header.
            let keys = screen.projects.map(\.id)
            ForEach(screen.projects, id: \.id) { projectCard($0, shown: keys) }
            if let ended = screen.ended {
                if showEnded {
                    bandView(ended).padding(.top, CC.space.lg)
                } else {
                    endedFooter(ended).padding(.top, CC.space.md)
                }
            }
        }
    }

    // MARK: Projects

    /// Every live run in one working directory on one computer.
    private struct Project {
        /// The computer and the working directory as drawn, which together make
        /// two runs one project: one folder on two computers is two.
        let id: ProjectKey
        let name: String
        /// The folder as a shell prints it: `~/infra`.
        let path: String
        /// What its computer is called; nil in the sample fleet.
        let machineName: String?
        let cells: [FleetRowCell.Content]
        let border: Color
        let needsYou: Bool
    }

    /// The home folder as `~`, the way a shell prints it: `/Users/<name>/…` on a
    /// Mac and `/home/<name>/…` on Linux.
    private static func shellPath(_ cwd: String) -> String {
        let parts = cwd.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count >= 3, parts[0].isEmpty, parts[1] == "Users" || parts[1] == "home"
        else { return cwd }
        return (["~"] + parts.dropFirst(3)).joined(separator: "/")
    }

    /// The projects and runs the list holds (`shown`), in the owner's order
    /// (`ProjectOrder`), the new ones on top in fleet order, and each project's
    /// runs in the order they started: no status and no event moves a card or
    /// a row. A held run the fleet no longer has is drawn as it last was.
    private func projects(of rows: [FleetRow], shown: [ProjectKey: Set<String>]) -> [Project] {
        let byKey = Dictionary(rows.map { ($0.summary.sessionKey, $0) }, uniquingKeysWith: { first, _ in first })
        var present: [ProjectKey] = []
        for row in rows where row.status != .ended {
            let id = ProjectKey(machineID: row.machineID, folder: row.summary.shownCwd)
            if shown[id] != nil, !present.contains(id) { present.append(id) }
        }
        present += shown.keys.filter { !present.contains($0) }
        return ProjectOrder.arrange(present, saved: machines.settings.projectOrder).compactMap { id in
            let members = (shown[id] ?? []).compactMap { byKey[$0] ?? heldRows[$0] }.sorted(by: Self.startOrder)
            guard let lead = members.first else { return nil }
            // Told apart among the runs this card holds, so a run held back
            // from it adds no line to the ones it shows.
            let labels = RunLabel.labels(for: members.map(\.summary))
            let waiting = members.contains { $0.status == .blocked }
            return Project(
                id: id,
                name: lead.label.project,
                path: Self.shellPath(id.folder),
                machineName: lead.machineName,
                // Control is the promise and its presence is not news, so a
                // card has no header note and an observe-only row says so
                // beside what it last did.
                cells: members.enumerated().map { index, member in
                    var row = member
                    row.label = labels[row.summary.sessionKey] ?? row.label
                    return cellContent(
                        row, bandCapability: .control, isLast: index == members.count - 1,
                        titledByAgent: true)
                },
                border: waiting
                    ? CC.color.warning.opacity(0.55)
                    : bandBorder(members.contains { $0.status == .failed } ? .failed : lead.status),
                needsYou: waiting)
        }
    }

    /// When a run started, then its key: an order no event changes.
    private static func startOrder(_ lhs: FleetRow, _ rhs: FleetRow) -> Bool {
        let left = lhs.summary.createdDate
        let right = rhs.summary.createdDate
        if left != right { return left < right }
        return lhs.summary.sessionKey < rhs.summary.sessionKey
    }

    /// One card per project, headed by where it is — its name, its folder and
    /// the computer it is on — with each run below named by its agent. Held and
    /// dragged, it moves; VoiceOver moves it with the header's own actions.
    private func projectCard(_ project: Project, shown keys: [ProjectKey]) -> some View {
        let index = keys.firstIndex(of: project.id)
        return CCCard(padding: 0, border: project.border) {
            VStack(spacing: 0) {
                VStack(alignment: .leading, spacing: CC.space.xxs) {
                    CCAdaptiveStack(horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xxs) {
                        // Wrapped, never cut: a folder's name is what tells two
                        // cards apart.
                        Text(project.name)
                            .ccType(CC.type.headline)
                            .foregroundStyle(CC.text.primary)
                            .fixedSize(horizontal: false, vertical: true)
                        CCAdaptiveSpacer(minLength: CC.space.sm)
                        MachineLabel(machineID: project.id.machineID, name: project.machineName)
                    }
                    // Cut from the front: the end of a folder is the part that
                    // names it. Whole at accessibility sizes, where the cut would
                    // leave little more than the name above it.
                    Text(verbatim: project.path)
                        .ccType(CC.type.monoSmall)
                        .foregroundStyle(CC.text.tertiary)
                        .lineLimit(typeSize.isAccessibilitySize ? nil : 1)
                        .truncationMode(.head)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, CC.space.md)
                .padding(.vertical, CC.space.sm)
                .accessibilityElement(children: .combine)
                .accessibilityAddTraits(.isHeader)
                .accessibilityActions {
                    if let index, index > 0 {
                        Button("Move up") { move(project.id, to: keys[index - 1], shown: keys) }
                    }
                    if let index, index < keys.count - 1 {
                        Button("Move down") { move(project.id, to: keys[index + 1], shown: keys) }
                    }
                }
                CCHairline()
                ForEach(project.cells, id: \.row.id) { rowCell($0) }
            }
        }
        .padding(.horizontal, CC.space.md)
        .padding(.bottom, CC.space.sm)
        .onDrag {
            dragging = project.id
            return NSItemProvider(object: project.name as NSString)
        }
        .onDrop(
            of: [.text],
            delegate: CardDrop(target: project.id, shown: keys, dragging: $dragging) { key, target in
                withAnimation(.snappy) { move(key, to: target, shown: keys) }
            })
    }

    /// `key` put where `target` is: the one way a card changes place.
    private func move(_ key: ProjectKey, to target: ProjectKey, shown: [ProjectKey]) {
        machines.settings.projectOrder = ProjectOrder.moving(
            key, to: target, shown: shown, saved: machines.settings.projectOrder)
    }

    // MARK: Start

    private func openStart() {
        guard canStart else { return }
        startOpen = true
    }

    /// A flow sent a start: close it, and open the new session as it goes.
    private func started(_ requestID: String) {
        startedRequest = requestID
        startOpen = false
    }

    private func openStarted() {
        guard let id = startedRequest else { return }
        startedRequest = nil
        path = [SessionRoute(key: "start-\(id)", startRequestID: id)]
    }

    /// Ask every computer for its projects. The computer is chosen in the
    /// flow, by a tap, and held there.
    private func loadStart() {
        for context in startContexts { context.load() }
    }

    // MARK: Bands

    private struct Band {
        let status: FleetStatus
        let rows: [FleetRow]
        /// `control` / `observe` when every row in the band agrees, `nil` when
        /// they do not — and a band that does not agree is exactly the band
        /// where capability is worth printing per row.
        let capability: CapabilityBadge?
        /// What each row draws, in band order.
        let cells: [FleetRowCell.Content]
        /// Whether its header offers Select: it holds a row a selection can.
        let offersSelect: Bool
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
            let capability = uniform ? first : nil
            // None for a folded Ended band: it draws a footer, not its rows,
            // and a cell costs a Stop lookup per row on every pass.
            let cells =
                status == .ended && !showEnded
                ? []
                : matching.enumerated().map { index, row in
                    cellContent(
                        row, bandCapability: capability, isLast: index == matching.count - 1)
                }
            return Band(
                status: status, rows: matching, capability: capability, cells: cells,
                offersSelect: !selectableKeys(matching).isEmpty)
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
        hasher.combine(capabilityReason != nil)
        hasher.combine(showDeck)
        return hasher.finalize()
    }

    private func bandView(_ band: Band) -> some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            bandHeader(band)
            if band.status == .ended, let note = batchNote(band.rows) {
                CCBanner(
                    note.title, message: note.reasons, tone: .warning,
                    icon: "warning")
                    .padding(.horizontal, CC.space.md)
            }
            CCCard(padding: 0, border: bandBorder(band.status)) {
                VStack(spacing: 0) {
                    ForEach(band.cells, id: \.row.id) { cell in
                        rowCell(cell)
                    }
                }
            }
            .padding(.horizontal, CC.space.md)
        }
        .padding(.bottom, CC.space.xl)
    }

    /// Red round a failed run's card or band; a project card with a run
    /// waiting for you is amber instead (`projects(of:)`).
    private func bandBorder(_ status: FleetStatus) -> Color {
        switch status {
        case .failed: return CC.color.danger.opacity(0.30)
        default: return CC.color.border
        }
    }


    /// Everything one row draws or decides, read here — from `liveScreen()`,
    /// through `projects(of:)` or `bands(of:)` — so the row itself reads nothing.
    private func cellContent(
        _ row: FleetRow, bandCapability: CapabilityBadge?, isLast: Bool,
        titledByAgent: Bool = false
    ) -> FleetRowCell.Content {
        let key = row.summary.sessionKey
        let owner = owner(of: row)
        // Removable runs only, and only when the daemon says it accepts the
        // request. `isRemovable` is `lifecycle`, not the derived status — the
        // rule can read Ended without the run being proven exited — plus the
        // unhosted case, where no proof of death can ever exist.
        let removable = canRemove(row)
        let choosable = selecting && row.status == .ended && removable
        return FleetRowCell.Content(
            row: row,
            waitingSince: waitingSince(row),
            blocked: blockedCard(row),
            showsCapability: showsCapability(row, bandCapability: bandCapability),
            separator: !isLast,
            titledByAgent: titledByAgent,
            // Not while choosing: removal then goes through the selection, and
            // a second route would race the batch for the same session.
            removalOffered: removable && !selecting,
            selection: choosable ? selected.contains(key) : nil,
            // **Offered only when the phone genuinely holds a turn to name.**
            // `stopUnavailable` returns nil exactly when this run is Codex, the
            // daemon honours a stop, its control link is subscribed and an
            // unterminated turn is in the log — and `runningTurn` is the one
            // honest hide: with no turn there is nothing to stop, so the
            // control is absent rather than dead.
            //
            // **`stopUnavailable(for:)` is the only reader of the rule.** This
            // used to re-derive it — agent, capability, running turn, and
            // deliberately *not* the link state — while the session header
            // re-derived it a third way and the send path a fourth. They
            // disagreed, and the disagreement shipped: an `interrupt` left for
            // sessions the phone already knew were `bound`, `offline` or `none`,
            // measured at the send stub.
            stopOffered: owner.stopUnavailable(for: key) == nil,
            stopBlockedReason: stopBlockedReason(for: row),
            stop: owner.codexControls(for: key).stop)
    }

    /// One row, and the actions it is handed. Each action is built from what
    /// `FleetRowCell` allows: see its rule.
    private func rowCell(_ cell: FleetRowCell.Content) -> some View {
        let key = cell.row.summary.sessionKey
        let summary = cell.row.summary
        let owner = owner(of: cell.row)
        return FleetRowCell(
            content: cell,
            remove: {
                let refusal = await owner.removeSession(summary).refusal
                // The owner's own removal is not news: it leaves at once.
                if refusal == nil { applyHeld() }
                return refusal
            },
            confirm: { removal in
                pendingRemoval = PendingRemoval(row: summary, model: owner, remove: removal)
            },
            coordinator: swipeCloses,
            onOpenDiff: { diffRoute = DiffRoute(key: key) },
            onMarkReviewed: { owner.markReviewed(key) },
            onStop: { Task { await owner.stopCodexTurn(sessionKey: key) } },
            onTap: { tap(cell) }
        )
        .equatable()
    }

    /// A row tapped: chosen or unchosen while choosing, opened otherwise.
    private func tap(_ cell: FleetRowCell.Content) {
        let key = cell.row.summary.sessionKey
        if cell.selection != nil {
            // The batch already holds what was chosen when it was asked.
            guard !batchRunning else { return }
            if selected.contains(key) { selected.remove(key) } else { selected.insert(key) }
        } else {
            path.append(SessionRoute(key: key))
        }
    }

    /// Why the offered control is greyed, or nil when it is live.
    ///
    /// **Short, and only during the cooldown.** Two rules meet here and the
    /// first render got both wrong:
    ///
    ///   * **Not the link state.** The fleet cannot tell a subscribed link from
    ///     a reconnecting one, so Stop is *offered* and the daemon's refusal —
    ///     which always names which condition failed — is what the operator
    ///     reads. Greying on a link state the summary reports but cannot act on
    ///     would hide the button on the one signal that is genuinely informative.
    ///   * **Not the daemon's sentence.** It is already drawn, verbatim, in the
    ///     banner directly beneath this row. Handed to the button as well it
    ///     rendered as a twelve-line orange column half the row wide, pushing the
    ///     activity line down and printing the same refusal twice — "two
    ///     identical warnings side by side read as two problems", which is the
    ///     rule the decision card already states about Deny. Measured on
    ///     `codex-stop-link-down--L.png`.
    ///
    /// Read in `liveScreen()`, like everything a row draws, so while a
    /// cooldown stands and the fleet is on screen the clock runs the body once
    /// a second; every other row is skipped.
    private func stopBlockedReason(for row: FleetRow) -> String? {
        let owner = owner(of: row)
        guard owner.codexControls(for: row.summary.sessionKey).isStopGreyed(now: owner.now)
        else { return nil }
        return "Try again in a moment."
    }

    /// Whether its Mac will remove this run: the one rule the swipe, VoiceOver
    /// and a selection all ask.
    private func canRemove(_ row: FleetRow) -> Bool {
        row.summary.isRemovable && owner(of: row).daemonProfile.removesSessions
    }

    /// The rows a selection can hold, in fleet order: ended, and removable.
    private func selectableKeys(_ rows: [FleetRow]) -> [String] {
        rows.filter { $0.status == .ended && canRemove($0) }.map(\.summary.sessionKey)
    }

    private func endSelection() {
        selecting = false
        selected = []
        batchRefused = [:]
    }

    /// What the last batch did not remove, among these ended rows.
    private func batchNote(_ ended: [FleetRow]) -> (title: String, reasons: String)? {
        let kept = ended.compactMap { batchRefused[$0.summary.sessionKey] }
        guard !kept.isEmpty else { return nil }
        return ("\(kept.count) not removed", Set(kept).sorted().joined(separator: " · "))
    }

    /// Replaces the deck bar while choosing.
    private func selectionBar(_ selectable: [String]) -> some View {
        let all = !selectable.isEmpty && selected.isSuperset(of: selectable)
        return VStack(spacing: 0) {
            CCHairline()
            CCAdaptiveStack(horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xs) {
                CCButton(all ? "Deselect All" : "Select All", variant: .ghost, size: .md) {
                    selected = all ? [] : Set(selectable)
                }
                .disabled(batchRunning)
                CCAdaptiveSpacer(minLength: CC.space.sm)
                CCButton(
                    batchRunning ? "Removing…" : "Remove (\(selected.count))",
                    variant: .destructive, size: .md
                ) {
                    confirmBatch(in: selectable)
                }
                .disabled(selected.isEmpty || batchRunning)
            }
            .padding(.horizontal, CC.space.md)
            .padding(.vertical, CC.space.sm)
        }
        .background(CC.color.surfaceRaised)
    }

    /// One question for the whole selection, then each removal in fleet order.
    /// Whatever the Mac did not remove stays selected, with its reason.
    private func confirmBatch(in selectable: [String]) {
        let keys = selectable.filter(selected.contains)
        let run = {
            batchRunning = true
            batchRefused = [:]
            Task {
                let refused = await machines.removeSessions(keys: keys)
                batchRunning = false
                batchRefused = refused
                // The owner's own removal is not news: it leaves at once.
                applyHeld()
                // What the batch did not remove is exactly what it has a reason
                // for. Still choosing, that is what stays chosen; if the link
                // went and choosing ended, the note is what is left to say so.
                if selecting {
                    selected = Set(refused.keys).intersection(selectableKeys(machines.fleet))
                }
                if let note = batchNote(machines.fleet.filter { $0.status == .ended }) {
                    UIAccessibility.post(
                        notification: .announcement, argument: "\(note.title). \(note.reasons)")
                }
            }
        }
        if keys.count == 1, let row = owner(keys[0]).shownSummary(for: keys[0]) {
            pendingRemoval = PendingRemoval(row: row, model: owner(keys[0]), remove: run)
        } else {
            pendingRemoval = PendingRemoval(count: keys.count, remove: run)
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
            },
            // Ended is the one band that can be folded, so it is the one that
            // folds back from its own header — standing any open row down
            // first, so a destructive control is not waiting when it unfolds.
            // No Done while a batch runs: the batch owns the selection until it
            // has answered.
            actionTitle: band.status == .ended
                ? (selecting ? (batchRunning ? nil : "Done") : "Hide") : nil,
            action: {
                if selecting {
                    endSelection()
                } else {
                    swipeCloses.closeAll()
                    batchRefused = [:]
                    withAnimation(CC.motion.small) { showEnded = false }
                }
            },
            secondaryActionTitle: band.status == .ended && !selecting && band.offersSelect
                ? "Select" : nil,
            secondaryAction: {
                swipeCloses.closeAll()
                batchRefused = [:]
                selecting = true
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

    private func showsCapability(_ row: FleetRow, bandCapability: CapabilityBadge?) -> Bool {
        // An unsettled badge is never shown, uniform band or not.
        guard row.capability.isSettled else { return false }
        guard let summary = bandCapability else { return true }
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
        return owner(of: row).shown(for: row.summary.sessionKey)?.pendingApprovals
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
        let owner = owner(of: row)
        let pending = owner.shown(for: row.summary.sessionKey)?.pendingApprovals ?? []
        guard let front = DeckOrdering.sort(pending, profile: owner.daemonProfile).first else {
            return nil
        }
        let tool = front.card.toolName
        return BlockedCard(
            activity: FleetActivity(
                tool: tool,
                argument: ToolSummary.principalArgument(
                    tool: tool, input: front.card.toolInput)?.firstLine),
            risk: front.assessment(profile: owner.daemonProfile).effective,
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
        // indented past the title and the count for a mark that is not drawn. The
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

    /// How an agent gets onto this screen, in words and never a command, with a
    /// button to start one from here when a paired computer can. Centred in the
    /// viewport.
    private var emptyState: some View {
        let start: (() -> Void)? = canStart ? { openStart() } : nil
        return ZStack {
            // At least the viewport's height, so the instruction sits on its
            // centre; taller content at the largest sizes still scrolls.
            Color.clear.containerRelativeFrame(.vertical)
            CCEmptyState(
                glyph: "terminal-window",
                title: "Start an agent on your computer",
                message:
                    "Start one with CodeConnect and its project shows up here, ready to follow and answer from this phone.",
                actionTitle: start == nil ? nil : "Start an agent",
                action: start)
        }
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

    /// The accessory bar that rises when anything is waiting for you, and the
    /// door to the cross-fleet queue behind it.
    ///
    /// Its count is the *fleet's*, the header's own, rather than recomputed:
    /// the bar and the header say the same sentence, so they must not each
    /// arrive at the number their own hand happens to hold.
    @ViewBuilder
    private func deckBar(_ screen: FleetScreen) -> some View {
        if !screen.risks.isEmpty {
            FleetDeckBar(count: screen.decisions, risks: screen.risks) {
                deckStartsAt = nil
                showDeck = true
            }
        }
    }

    /// Dismisses everything Fleet itself can present.
    ///
    /// One place, so a new sheet added to this screen has exactly one list to
    /// join — rather than being forgotten by whichever call site happened to
    /// enumerate the others.
    private func clearPresentations() {
        showSettings = false
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
        switch machines.pendingDeepLink {
        case .fleet:
            _ = machines.consumeDeepLink()
            clearPresentations()
        case .deck(let requestID):
            _ = machines.consumeDeepLink()
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
            // the tap doing nothing at all — except during a catch-up, when the
            // name waits for the list it ends on.
            guard let key = machines.resolveSessionKey(reference: reference)
                ?? (machines.isCatchingUp ? nil : reference)
            else { return }
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

/// **Six elements, five of them facts, and it adds the one that was missing.**
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
/// CLAUDE CODE  Bash · git push --force origin main
/// ```
///
/// Two lines, inside the 76pt row: `16 + 20 + 4 + 18 + 16`. The third
/// line comes back only for a run that needs disambiguating from a namesake, a
/// session whose capability disagrees with its band, or a run holding more than
/// one decision — a third line has to carry something the two above it do not.
// MARK: - The three views that are allowed to read the clock

/// **The computer a project is on, and whether it is answering** — the shared
/// `CCMachineLabel`, reading that computer's own link here because the link is
/// derived from the clock, and read from `FleetView.body` that second was charged
/// to the whole list. Nothing when the computer has no name — nothing is paired
/// and no daemon has named one, the sample fleet among them, whose dot would
/// report a link that does not exist.
/// A card dragged over another takes that card's place, live, so the drop
/// only has to let go; the order is already kept.
private struct CardDrop: DropDelegate {
    let target: ProjectKey
    let shown: [ProjectKey]
    @Binding var dragging: ProjectKey?
    let move: (ProjectKey, ProjectKey) -> Void

    func dropEntered(info: DropInfo) {
        guard let dragging, dragging != target, shown.contains(dragging) else { return }
        move(dragging, target)
    }

    func dropUpdated(info: DropInfo) -> DropProposal? { DropProposal(operation: .move) }

    func performDrop(info: DropInfo) -> Bool {
        dragging = nil
        return true
    }
}

private struct MachineLabel: View {
    @Environment(Machines.self) private var machines
    @Environment(\.dynamicTypeSize) private var typeSize
    let machineID: String?
    let name: String?

    var body: some View {
        if let name, let model = machines.model(for: machineID) {
            let level = model.linkHealth.level
            // At reading sizes one line, as wide as it is with its widest
            // word, so the word coming and going takes no width from the
            // project's name and the card's header keeps its height.
            ZStack(alignment: .trailing) {
                if !typeSize.isAccessibilitySize {
                    ForEach(Self.worded.indices, id: \.self) { index in
                        CCMachineLabel(name: name, level: Self.worded[index], lineLimit: 1).hidden()
                    }
                }
                CCMachineLabel(name: name, level: level, lineLimit: typeSize.isAccessibilitySize ? nil : 1)
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel("On \(name), \(level.machineMark.word ?? "connected")")
        }
    }

    /// Every level that says a word beside the computer's name.
    private static let worded: [LinkHealth.Level] = [.stale, .connecting, .offline, .rejected]
}

/// **The banner, clocked by itself.**
///
/// It reads `linkHealth` to decide what to say and whether to say anything, and
/// `linkHealth` is derived from `now` — so hoisting this read into
/// `FleetView.body` put a one-second heartbeat under the whole fleet. The
/// ladder, the two-second grace on a reconnect, and the compound cached message
/// are unchanged; only the owner of the read moved.
private struct FleetBanner: View {
    /// `Machines.linkModel`: the link banner speaks only once no machine can be
    /// reached.
    @Environment(AppModel.self) private var model
    @Environment(Machines.self) private var machines
    @Environment(\.dynamicTypeSize) private var typeSize
    let onSettings: () -> Void

    var body: some View {
        Group {
            let candidates = bannerCandidates
            if candidates.contains(where: { $0 != nil }) {
                CCBannerSlot(candidates)
                    .padding(.horizontal, CC.space.md)
                    .padding(.vertical, CC.space.sm)
                    .frame(maxWidth: .infinity)
                    .background(CC.color.bg)
                    .padding(.bottom, typeSize.isAccessibilitySize ? CC.space.xl : 0)
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
        // Permanently, and before any other: the sample fleet is the whole context for
        // everything under it, and it is the only state here that a reader can
        // leave rather than wait out.
        if model.sampleFleetActive {
            return [.sampleFleet(onLeave: { model.stopSampleFleet() })]
        }
        // **A message this build cannot read is never swallowed.**
        //
        // It outranks the link banner because it is a *different* problem and
        // the louder one: the link is healthy, the Mac is working, and this
        // phone is the thing that is behind. Rendered as what the reader can act
        // on — update the app — with the type named, so the next engineer has
        // the whole diagnosis rather than "an unreadable frame".
        if let unreadable = machines.models.lazy.compactMap(\.connection.unreadableFrame).first {
            return [
                CCBannerItem(
                    // Above `offline` and everything under it: this is a
                    // problem with the app, not with the link, and a reader
                    // hunting a missing feature must not be shown a freshness
                    // note that says the connection is fine.
                    .rejected,
                    title: "Update CodeConnect on this iPhone",
                    message: "\(unreadable) Some of what your computer reports will be missing here.",
                    tone: .warning,
                    icon: "arrow-circle-down")
            ]
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
            onRetry: { machines.retryNow() },
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
            icon: "clock-counter-clockwise",
            actionTitle: "Retry",
            action: { machines.retryNow() })
    }
}

/// The accessory bar, reading the link by itself.
///
/// It quotes the link's own reason for disabling `Review`, and the link's state
/// is a function of the clock — it goes stale on its own. That is one view at
/// the bottom of the screen; read from the fleet, it was costing a rebuild of
/// every row above it.
private struct FleetDeckBar: View {
    @Environment(AppModel.self) private var model
    let count: Int
    let risks: [RiskClass: Int]
    let onOpen: () -> Void

    var body: some View {
        DeckAccessoryBar(
            count: count,
            risks: risks,
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
    @Environment(Machines.self) private var machines
    let since: Date

    var body: some View {
        CCWaitingNotice(elapsed: model.now.timeIntervalSince(since)) {
            machines.retryNow()
        }
    }
}

/// Reports whether the fleet is in the window. A zero-size view placed in the
/// fleet's content, so it joins and leaves the window with it.
private struct FleetWindowPresence: UIViewRepresentable {
    let changed: (Bool) -> Void

    func makeUIView(context: Context) -> Probe { Probe(changed: changed) }
    func updateUIView(_ probe: Probe, context: Context) { probe.changed = changed }

    final class Probe: UIView {
        var changed: (Bool) -> Void

        init(changed: @escaping (Bool) -> Void) {
            self.changed = changed
            super.init(frame: .zero)
            isUserInteractionEnabled = false
        }

        @available(*, unavailable)
        required init?(coder: NSCoder) { fatalError("not from a nib") }

        override func didMoveToWindow() {
            super.didMoveToWindow()
            changed(window != nil)
        }
    }
}

/// **One fleet row, drawn only when what it draws changed.**
///
/// On screen, `FleetView.body` reads every session, so any session's event runs
/// it. A row built there is handed closures, which SwiftUI cannot compare, so it
/// used to redraw every row on every event — measured on a streaming fleet,
/// about 46 row bodies where one row had changed. This view is compared by
/// `content` alone (`.equatable()`), so an unchanged row is skipped.
///
/// Two rules make that safe, and both are about something being left out:
///
///  * **`Content` holds everything the row draws or decides**, and its
///    equality is synthesized, so no field can be missed by `==`. Nothing
///    below reads the model: a read there would run in the row's own scope,
///    where the comparison cannot see it. The row reads only `Content`, the
///    environment and its own state.
///  * **An action captures only what stays true while `content` is equal**:
///    the row's key, values in `Content`, `FleetView`'s state read when the
///    action runs (never copied into a `let` first), the one `AppModel`, and
///    the screen's swipe coordinator. A skipped row keeps the actions it was
///    last handed, so anything else would act on stale state.
struct FleetRowCell: View, Equatable {
    struct Content: Equatable {
        let row: FleetRow
        /// When the oldest still-unanswered card in this run was raised.
        let waitingSince: Date
        let blocked: BlockedCard?
        let showsCapability: Bool
        let separator: Bool
        /// Inside a project card that names the project above it.
        let titledByAgent: Bool
        /// The swipe and VoiceOver's named action: this Mac will remove the
        /// run, and no selection is being made.
        let removalOffered: Bool
        /// Whether the row is chosen, while ended rows are being chosen to
        /// remove and this one can be; nil otherwise.
        let selection: Bool?
        let stopOffered: Bool
        let stopBlockedReason: String?
        /// The Stop in flight, or what the daemon last said about it.
        let stop: CodexControls.Activity<InterruptResult>
    }

    let content: Content
    /// Returns `nil` when it worked, or the reason it did not.
    let remove: () async -> String?
    /// Asks before a removal runs, and runs it only on a yes.
    let confirm: (@escaping () -> Void) -> Void
    let coordinator: CCSwipeCloseCoordinator
    let onOpenDiff: () -> Void
    let onMarkReviewed: () -> Void
    let onStop: () -> Void
    let onTap: () -> Void

    nonisolated static func == (lhs: FleetRowCell, rhs: FleetRowCell) -> Bool {
        lhs.content == rhs.content
    }

    /// The row, and whatever the daemon last said about a stop on it.
    ///
    /// **Directly under the row that produced it**, rather than in a screen-wide
    /// banner slot: on a fleet of two dozen runs, "Nothing was sent" floating at
    /// the top of the list is a sentence about a row the reader has to go and
    /// find. The outcome belongs where the tap was.
    var body: some View {
        VStack(spacing: 0) {
            row
            stopOutcome
        }
    }

    /// The row, wrapped in its removal gesture.
    ///
    /// **Derived once.** The gesture and the named accessibility action have to
    /// agree about whether this row can be removed, and about what happens when
    /// it is. Written twice, they drift, and the drift is invisible: VoiceOver
    /// offers an action the swipe does not, or the reverse.
    private var row: some View {
        CCSwipeToRemove(
            isEnabled: content.removalOffered,
            // "Remove", not "Delete". Claude keeps its own transcript under
            // `~/.claude/projects`, so the conversation survives and
            // `claude --resume` still works; only CodeConnect's record goes.
            title: "Remove",
            action: remove,
            confirm: confirm,
            rowID: content.row.summary.sessionKey,
            coordinator: coordinator
        ) {
            FleetRowView(
                row: content.row,
                waitingSince: content.waitingSince,
                blocked: content.blocked,
                showsCapability: content.showsCapability,
                separator: content.separator,
                titledByAgent: content.titledByAgent,
                onOpenDiff: onOpenDiff,
                onMarkReviewed: onMarkReviewed,
                onRemove: content.removalOffered ? { removeWithAnnouncement() } : nil,
                onStop: content.stopOffered ? onStop : nil,
                stopBlockedReason: content.stopBlockedReason,
                stopping: content.stop == .inFlight,
                selection: content.selection,
                action: onTap)
        }
    }

    private func removeWithAnnouncement() {
        confirm {
            Task {
                // The swipe shows a refusal in the button it revealed. There is
                // no button here — the row either disappears or it does not —
                // so the reason has to be spoken, or a VoiceOver user gets
                // silence and an unchanged list.
                if let refusal = await remove() {
                    UIAccessibility.post(notification: .announcement, argument: refusal)
                }
            }
        }
    }

    @ViewBuilder
    private var stopOutcome: some View {
        switch content.stop {
        case .settled(let result):
            let banner = CodexProse.interrupt(result)
            CCBanner(
                banner.title,
                // **The daemon's sentence, verbatim.** Every one of the eleven
                // refusals names which condition failed and what to do instead;
                // they were written to be shown.
                message: banner.message,
                tone: banner.tone.ccTone, icon: banner.icon)
                .padding(.horizontal, CC.space.md)
                .padding(.bottom, CC.space.sm)
        case .notSent(let reason):
            // The app's own words, never dressed as the daemon's: nothing was
            // sent, so the daemon has said nothing about it.
            CCBanner(
                "Nothing was sent", message: reason, tone: .warning,
                icon: "warning-fill")
                .padding(.horizontal, CC.space.md)
                .padding(.bottom, CC.space.sm)
        case .sentNoAnswer(let reason):
            // **Not "nothing was sent".** The frame left; what became of it is
            // the thing nobody knows.
            CCBanner(
                "Sent, no answer heard", message: reason, tone: .warning,
                icon: "question")
                .padding(.horizontal, CC.space.md)
                .padding(.bottom, CC.space.sm)
        case .idle, .inFlight:
            EmptyView()
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
    /// Titled with its agent rather than its project, because the project card
    /// it sits in is headed with the project.
    var titledByAgent = false
    let onOpenDiff: () -> Void
    let onMarkReviewed: () -> Void
    /// Present only on a row that can actually be removed, so VoiceOver never
    /// announces an action that would be refused. A gesture cannot be the only
    /// route to a feature — that is the rule `CCDiffPrimitives` writes down for
    /// context menus and it applies with more force to a swipe, which is
    /// invisible and unreachable by Switch Control.
    /// Asks before it removes, the same question the swipe asks.
    var onRemove: (() -> Void)?
    /// **Stop this Codex session's running turn**, or nil when there is nothing
    /// to stop or nothing that could stop it.
    ///
    /// Passed in rather than derived here for the same reason `onRemove` is: the
    /// row draws what it is given, and whether a mutation is offered is a
    /// question about the fleet's state, which the screen above holds.
    var onStop: (() -> Void)?
    /// Why Stop is greyed, or nil when it is live. A greyed control **states its
    /// reason** — a dead control with no explanation is the failure the
    /// verification bar names.
    var stopBlockedReason: String?
    /// True from the tap until the daemon answers.
    var stopping: Bool = false
    /// Whether this row is chosen, while ended rows are being chosen to remove;
    /// nil otherwise. The checkmark takes the status dot's place.
    var selection: Bool?
    let action: () -> Void

    @Environment(\.dynamicTypeSize) private var typeSize
    /// Every tool label's width on screen, from which this row's label takes
    /// its column, so the commands beside them share one left edge. Empty until
    /// the first pass has measured.
    @Environment(\.ccToolLabelWidths) private var toolLabelWidths

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
        #if DEBUG
            let _ = FleetRowRenders.record(row.summary.sessionKey, lastTick: lastTick)
        #endif
        Group {
            if let selection {
                content {
                    CCIcon(selection ? "check-circle-fill" : "circle", size: 18)
                        .foregroundStyle(selection ? CC.text.primary : CC.text.tertiary)
                        .accessibilityHidden(true)
                }
            } else if let dot = dotColour {
                content {
                    CCStatusDot(
                        color: dot, isHollow: row.cachedAt != nil, pulses: false,
                        accessibilityText: nil)
                }
            } else if titledByAgent {
                content { CCStatusDot().hidden() }
            } else {
                content { EmptyView() }
            }
        }
        .accessibilityIdentifier("session-\(row.summary.sessionKey)")
        .accessibilityAddTraits(selection == true ? .isSelected : [])
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
                Button("Remove from CodeConnect", action: onRemove)
            }
        }
        // Restarted whenever the timestamp this row is counting from changes —
        // a new event, or a blocked row's oldest card being answered — because
        // the old deadline was computed from the old anchor.
        .task(id: clock) { await AgeTick.follow(clock) { lastTick = $0 } }
        #if DEBUG
            .onAppear { FleetRowRenders.mounted(row.summary.sessionKey) }
        #endif
    }

    /// **The dot is gone from blocked and failed runs**, where it was a fourth
    /// encoding of a state the section header, its place in the list and the
    /// clock all carry. It stays where it discriminates: hollow means *this row
    /// came off the disk*, which nothing else on the screen says.
    ///
    /// **So is its column**, outside a project card: a row with no dot starts
    /// its title at the card's own content edge rather than holding an empty
    /// 8pt gutter. In a project card the column stays, so a run that starts or
    /// stops waiting keeps its title where it was.
    private func content<Leading: View>(
        @ViewBuilder leading: @escaping () -> Leading
    ) -> some View {
        CCRow(
            // The place, always. A fleet is a set of places, and the place is
            // the stable, scannable anchor — never the AI title, which is
            // sometimes a sentence and sometimes a folder name. Inside a
            // project's card the card names the place, and the row its agent.
            titledByAgent ? CCTag(agent: row.summary.agent).text : placeName,
            // Regular under a project's semibold name, so the card reads as one
            // project and its agents rather than as several titles.
            titleStyle: titledByAgent ? CC.type.callout : CC.type.rowTitle,
            // **From the front.** The title is a project — a directory
            // somebody named — and a directory is recognised by how it starts.
            // Trimming the middle of a long one returns
            // `platform-s…nciliation…`, two elisions deep and readable as
            // neither name; trimming the tail returns the beginning of the word
            // the reader is looking for. Runs that share a project are told
            // apart on the line below, not by the shape of this one.
            titleTruncation: .tail,
            showsChevron: false,
            separator: separator,
            density: titledByAgent ? .compact : .comfortable,
            isDimmed: row.status == .ended,
            accessibilityLabelText: accessibilityLabel,
            action: action,
            leading: leading,
            trailing: { titleTrailing },
            meta: { metaBlock })
    }

    /// Nil where the band already says it and nothing else needs saying.
    private var dotColour: Color? {
        if row.cachedAt != nil { return row.status.ccDotColor }
        return isBlockedOrFailed ? nil : row.status.ccDotColor
    }

    @ViewBuilder
    private var titleTrailing: some View {
        // The two facts that decide which row you open first, on the line the
        // eye is already on. Stacked at accessibility sizes: measured at AX5,
        // side by side the clock's fixed single line squeezed the badge to
        // about half its width and `HIGH` broke as `HIG` / `H`.
        CCAdaptiveStack(horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xxs) {
            riskSlot
            // A tag's height without one: low risk draws none.
            if (blocked?.risk ?? .low) == .low { chipHeight(CCRiskTag(.high)) }
            if isBlockedOrFailed {
                // `monoSmall`, like every other age on the screen: the wait
                // is set apart by where it sits, not by its size or a hue.
                CCWaitClock(since: waitingSince, now: now, prefix: nil)
            } else {
                Text(freshness)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
                    .lineLimit(1)
            }
        }
    }

    /// The card this run waits on, at its risk, and how many more wait behind
    /// it. It fits inside the title's line, so a run that starts or stops
    /// waiting keeps its height.
    @ViewBuilder
    private var riskSlot: some View {
        if let blocked {
            HStack(spacing: CC.space.xs) {
                CCRiskTag(blocked.risk)
                if row.blockedCount > 1 {
                    // A bare count, never cut: beside the tag and the clock a
                    // phone has no room for words.
                    Text("+\(row.blockedCount - 1)")
                        .ccType(CC.type.micro)
                        .foregroundStyle(CC.text.tertiary)
                        .lineLimit(1)
                        .fixedSize()
                        .accessibilityHidden(true)
                }
            }
        }
    }

    @ViewBuilder
    private var metaBlock: some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            // **Which agent this run is**, on every row, Claude included: a
            // tag only some rows carry reads as an alarm on those rows, and a
            // fleet that mixes agents is read by which one each row is. It
            // leads the activity line rather than sitting beside the title,
            // where the risk tag and the wait clock already are. The line is
            // always there, empty or not, so a run's height does not change
            // with what it is doing.
            CCAdaptiveStack(horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xxs) {
                if !titledByAgent { CCTag(agent: row.summary.agent) }
                if let activity {
                    activityLine(
                        activity,
                        truncation: blocked?.truncation ?? .head,
                        // The tool name takes full contrast only where the row
                        // is asking for something. A Running row's activity is
                        // not urgent, so it stays one step down and the *face*
                        // — prose against monospace — is what carries the
                        // distinction.
                        tone: blocked == nil ? CC.text.secondary : CC.text.primary)
                } else {
                    // The daemon's own *sentence* — a user message, an agent
                    // reply, a notice — which has no argument to set as code.
                    Text(verbatim: row.subtitle.isEmpty ? " " : row.subtitle)
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                        .lineLimit(typeSize.isAccessibilitySize ? nil : 1)
                        .fixedSize(horizontal: false, vertical: true)
                }
                CCAdaptiveSpacer(minLength: 0)
                // Beside what the run last did, which gives up width rather
                // than wrapping, so a run whose link or life changes what it
                // can do keeps its height.
                if showsCapability {
                    CCBadge(capability: row.capability)
                } else {
                    chipHeight(CCBadge(capability: .observe(reason: "")))
                }
                stopControl
            }
            if showsIdentity { exceptionLine }
        }
    }

    /// A chip's height, kept on a card's line that does not show the chip, so
    /// one arriving there moves nothing below it. Not in the stacked form,
    /// where it would be a blank line of its own.
    @ViewBuilder
    private func chipHeight<Chip: View>(_ chip: Chip) -> some View {
        if titledByAgent, !typeSize.isAccessibilitySize {
            chip.hidden().frame(width: 0).accessibilityHidden(true)
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
    /// under the risk tag and the clock rather than beside them.
    private func activityLine(
        _ card: FleetActivity, truncation: CCMonoTruncation, tone: Color
    ) -> some View {
        // Stacked at accessibility sizes. Measured at AX5: side by side, the
        // command started in the middle of the row and wrapped into a three-line
        // column about half the width of the card — `/Users/` / `dev/app/` /
        // `…re.swift`, which is a path nobody can read. Stacked, it gets the
        // whole measure. Side by side, the tool column — see `CCToolColumn`.
        let layout =
            typeSize.isAccessibilitySize
            ? AnyLayout(VStackLayout(alignment: .leading, spacing: 2))
            : AnyLayout(CCToolPairLayout(labels: toolLabelWidths, alignment: .firstTextBaseline))
        return layout {
            Text(card.tool)
                .ccType(CC.type.footnote)
                .foregroundStyle(tone)
                .lineLimit(1)
                .ccToolLabelColumn(disabled: typeSize.isAccessibilitySize)
            if let argument = card.argument {
                CCMonoBlock(inline: argument, truncation: truncation)
            }
        }
    }

    /// **Stop, on the row.**
    ///
    /// On the activity line rather than beside the title, deliberately: the
    /// title row already carries the risk tag and the wait clock, and a third
    /// control there pushed this row's age out of line with every Claude
    /// neighbour's — an alignment failure the HTML pass caught before any Swift
    /// was written.
    ///
    /// Absent, not disabled, when there is nothing running: a control whose only
    /// behaviour is refusing the tap is the item this product does not ship.
    /// Present-and-greyed is for the bounded window after a link-state refusal,
    /// where the daemon's own sentence has just said to try again shortly — and
    /// there the reason is drawn, never merely hinted.
    ///
    /// In a project card a Codex run keeps the control's height while it has
    /// nothing to stop, so a turn starting or ending moves nothing below it.
    @ViewBuilder
    private var stopControl: some View {
        if let onStop {
            CCButton(
                stopping ? "Stopping…" : "Stop",
                icon: stopping ? nil : "stop-fill",
                variant: .secondary,
                size: .sm,
                isLoading: stopping,
                disabledReason: CCDisabledReason(stopBlockedReason),
                action: onStop
            )
            .accessibilityIdentifier("stop-\(row.summary.sessionKey)")
            .accessibilityLabel("Stop the turn \(placeName) is running")
            // Width goes to the control first, so its label never breaks beside
            // the agent tag and the activity: the command truncates instead.
            .layoutPriority(1)
        } else if titledByAgent, row.summary.agent == .codex {
            CCButton("Stop", icon: "stop-fill", variant: .secondary, size: .sm) {}
                .hidden()
                // At its own size: proposed no width, it stood 42 pt taller
                // than the Stop it holds the place of.
                .fixedSize()
                .frame(width: 0)
                .accessibilityHidden(true)
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
            CCAdaptiveSpacer(minLength: CC.space.xs)
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
    /// the risk badge published "Risk HIGH. Destructive, credentialed, or
    /// publishes something." and the parent threw it away, so a sighted reader
    /// saw a red chip and a VoiceOver user could not tell a `Read` from a
    /// `git push --force` without opening it. On an app whose subject is risk
    /// triage that was the most consequential accessibility defect in the build.
    ///
    /// Everything the row draws now survives: the agent, the class, the command,
    /// the project, and the age in words. Capability is
    /// announced on **every** row regardless of visual suppression — screen
    /// readers read, they do not scan, and the economics are different.
    private var accessibilityLabel: String {
        var parts = [
            row.label.spoken, CCTag(agent: row.summary.agent).accessibilityText, row.status.label,
        ]
        if let blocked {
            parts.append("risk \(blocked.risk.label)")
            parts.append(blocked.risk.rationale)
        }
        if row.cachedAt != nil { parts.append("from cache") }
        if row.blockedCount > 1 { parts.append(FleetCount.waitingForYou(row.blockedCount)) }
        parts.append(spokenActivity)
        if isBlockedOrFailed {
            parts.append("waiting \(Format.spokenDuration(now.timeIntervalSince(waitingSince)))")
        } else if let last = row.lastEventAt {
            parts.append("last activity \(Format.spokenAge(now.timeIntervalSince(last)))")
        }
        // Silent while unsettled: VoiceOver asserting "observe only" during
        // the handshake is the same flash, spoken.
        if row.capability.isSettled {
            parts.append(row.capability.canAct ? "control" : "observe only")
        }
        return parts.filter { !$0.isEmpty }.joined(separator: ", ")
    }

    /// What the row's second line says, out loud.
    ///
    /// The flat sentence where the row draws one, and the split read back as two
    /// clauses where it draws two — a screen reader has no faces to tell prose
    /// from code with, so the comma is what does the work. An argument whose
    /// first line is blank is no argument, not an empty clause (`Bash, ,`).
    private var spokenActivity: String {
        guard let activity else { return row.subtitle }
        return [activity.tool, activity.argument].compactMap { $0 }.filter { !$0.isEmpty }
            .joined(separator: ", ")
    }
}

#if DEBUG

    /// **How often each fleet row has drawn other than on its own clock, and
    /// how often it has been made anew**, so a UI test can see whether one
    /// session's event redraws the others — and how often the fleet's own body
    /// has run and read the model, so it can see whether the fleet does work
    /// while a chat covers it.
    ///
    /// A row's clock redraws it on its own schedule — every second while it
    /// counts a wait — and that redraw is right. A pass whose `lastTick` is the
    /// one the row last drew with was not the clock's, and is counted. A row
    /// made anew starts with a fresh `lastTick`, so its first pass looks like
    /// the clock's: it is counted as a mount instead, when it appears. Recorded
    /// only under `-CC_ROW_RENDER_PROBE YES`, and `#if DEBUG` so it cannot
    /// exist in a shipped build.
    @MainActor
    enum FleetRowRenders {
        static let enabled = UserDefaults.standard.bool(forKey: "CC_ROW_RENDER_PROBE")
        private static var rows: [String: (renders: Int, mounts: Int, lastTick: Date?)] = [:]
        private static var fleetBodies = 0
        private static var fleetReads = 0

        static func fleetBody() {
            guard enabled else { return }
            fleetBodies += 1
        }

        /// A `FleetScreen` built: the fleet read from the model.
        static func fleetRead() {
            guard enabled else { return }
            fleetReads += 1
        }

        static func record(_ key: String, lastTick: Date) {
            guard enabled else { return }
            var seen = rows[key] ?? (0, 0, nil)
            if seen.lastTick == lastTick { seen.renders += 1 }
            seen.lastTick = lastTick
            rows[key] = seen
        }

        static func mounted(_ key: String) {
            guard enabled else { return }
            rows[key, default: (0, 0, nil)].mounts += 1
        }

        /// `fleet=12+12 late=1 fx-1=0+1 fx-2=3+1 …`: the fleet's body passes,
        /// then how often it built its screen from the model; the held-back fixture frames delivered
        /// (`AppModel.lateFramesDelivered`); then, by row, draws other than the
        /// clock's, then mounts.
        static var label: String {
            (["fleet=\(fleetBodies)+\(fleetReads)", "late=\(AppModel.lateFramesDelivered)"]
                + rows.keys.sorted().map { key in
                    "\(key)=\(rows[key]?.renders ?? 0)+\(rows[key]?.mounts ?? 0)"
                }).joined(separator: " ")
        }
    }

    /// The counts, as an invisible element `cc-row-render-probe` re-read four
    /// times a second. Its own view, so re-reading them redraws nothing else.
    struct FleetRowRenderProbe: View {
        var body: some View {
            if FleetRowRenders.enabled {
                TimelineView(.periodic(from: .now, by: 0.25)) { _ in
                    Color.clear
                        .frame(width: 1, height: 1)
                        .accessibilityElement()
                        .accessibilityIdentifier("cc-row-render-probe")
                        .accessibilityLabel(FleetRowRenders.label)
                        .allowsHitTesting(false)
                }
            }
        }
    }

#endif

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
                        "Answers given on this phone reach an agent only when CodeConnect is attached to its terminal and your computer says it can deliver them."
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
                        "Everything on this screen is still true. You can read the timeline, open the diff and attach the terminal, you just answer at the computer."
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
///  * **Capabilities are reported, not guessed**: each row is what the computer
///    said about itself, and an unknown is not a no.
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
                    "your computer last answered \(Format.ago(since: model.connection.lastContactAt, now: model.now))"
                )
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
                if !model.linkHealth.actionsEnabled {
                    Text("Actions are disabled until your computer answers.")
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
        case .rejected: return "Pair again"
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
                "Event gaps", value: "\(gapCount)",
                tone: gapCount > 0 ? .danger : .neutral),
            CCStat("Reconnects", value: "\(model.connection.reconnectCount)"),
        ])
    }

    /// Sessions whose event log has a discontinuity the app has not seen filled.
    private var gapCount: Int { model.sessionDiagnostics.gaps }

    private var linkSection: some View {
        section("Link") {
            CCFactRow(
                "State", value: model.linkHealth.detail,
                tone: model.linkHealth.level.ccTone, separator: true)
            CCFactRow(
                "Computer last answered",
                value: Format.ago(since: model.connection.lastContactAt, now: model.now),
                separator: true)
            // Not measured on this build. `—`, never `0ms`.
            CCFactRow("Round trip", value: Self.unmeasured, isUnmeasured: true, separator: true)
            CCFactRow(
                "Encryption", value: transport, tone: transportTone, separator: true)
            CCFactRow(
                "Address", value: model.endpoint?.displayAddress ?? Self.unmeasured,
                isUnmeasured: model.endpoint == nil, separator: true)
            CCFactRow(
                "Link version",
                value: model.daemonProfile.isConnected
                    ? "v\(model.daemonProfile.protocolVersion).\(model.daemonProfile.protocolMinor)"
                    : Self.unmeasured,
                isUnmeasured: !model.daemonProfile.isConnected,
                separator: true)
            CCFactRow(
                "Computer's clock",
                value: model.connection.serverTime ?? Self.unmeasured,
                isUnmeasured: model.connection.serverTime == nil,
                separator: model.connection.lastErrorMessage != nil)
            if let error = model.connection.lastErrorMessage {
                // The daemon's words, verbatim and monospace. Never paraphrased.
                noteRow(
                    "Last error",
                    text: error,
                    age: model.connection.lastErrorAt.map {
                        Format.ago(since: $0, now: model.now)
                    })
            }
        }
    }

    /// `tls` says the listener *holds* a certificate; `tls_active` says *this*
    /// connection is encrypted. They are different facts and are shown
    /// separately — the degraded pairing gets the verbatim explanation. The
    /// words are the computer screen's (`PairingView.transport`), so the two
    /// screens never describe one connection differently.
    private var transport: String {
        guard model.daemonProfile.isConnected else { return Self.unmeasured }
        return PairingView.transport(
            encrypted: model.daemonProfile.connectionEncrypted,
            paired: model.connection.helloAck?.deviceID != nil,
            capabilities: model.connection.capabilities
        ).value
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
            CCSectionHeader("What your computer can do")
            CCCard(padding: 0) {
                if let capabilities = model.connection.capabilities {
                    VStack(spacing: 0) {
                        capabilityRow("Answer approvals", capabilities.canApproveReliably)
                        capabilityRow("Type into the session", capabilities.sendText)
                        capabilityRow("Screen snapshots", capabilities.capture)
                        // Normalized, not raw `push`: a relay daemon advertises
                        // `push = false` on purpose and would otherwise read here
                        // as "no", though it is exactly the daemon that does send.
                        capabilityRow("Push notifications", capabilities.pushMode != .none)
                        capabilityRow("Holds a certificate", capabilities.tls)
                        // These two are the difference between a Stop button
                        // that works and one that is offered and guaranteed
                        // broken, and a reader asking "why can't I stop this?"
                        // should find the answer in the same words the control used.
                        capabilityRow("Stop a Codex turn", capabilities.codexInterrupt)
                        capabilityRow("Say something to Codex", capabilities.codexCompose)
                        capabilityRow(
                            "This connection encrypted", capabilities.tlsActive, separator: false)
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
                Text("Your computer can encrypt this connection itself, but this iPhone isn't using that.")
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.color.warning)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, CC.space.md)
            }
        }
    }

    private var eventStreamSection: some View {
        section("Event stream") {
            CCFactRow(
                "Sessions with a gap", value: "\(gapCount)",
                tone: gapCount > 0 ? .danger : .neutral, separator: true)
            CCFactRow(
                "Latest event number",
                value: highestSeq.map(String.init) ?? Self.unmeasured,
                isUnmeasured: highestSeq == nil,
                separator: true)
            CCFactRow(
                "Sessions from cache", value: "\(cachedSessionCount)",
                tone: cachedSessionCount > 0 ? .warning : .neutral, separator: false)
        }
    }

    private var highestSeq: UInt64? { model.sessionDiagnostics.highestSeq }

    private var cachedSessionCount: Int { model.sessionDiagnostics.fromCache }

    private var fleetSection: some View {
        section("Fleet") {
            CCFactRow("Known sessions", value: "\(model.sessionDiagnostics.known)", separator: true)
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
            if model.connection.capabilities?.pushMode == .relay,
                model.connection.helloAck?.deviceID != nil
            {
                // Relay only, and only once paired: this forgets the App Attest
                // credential and enrolls again from scratch — the fix for a
                // credential the relay has stopped trusting. Direct mode holds
                // nothing to reset, and a bootstrap connection has nothing to
                // enroll, so the action would do nothing there.
                CCButton(
                    "Reset notification registration", variant: .ghost, size: .lg, fullWidth: true
                ) {
                    model.resetNotificationRegistration()
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
            return "Your computer isn't set up to send notifications, so nothing was sent."
        case .notPairedDevice:
            return "This iPhone is connected without pairing. Pair it to test notifications."
        case .noRegisteredToken:
            return "Your computer can't notify this iPhone yet. Turn on notifications, then try again."
        case .credentialInvalid:
            return "The relay no longer trusts this phone’s credential. Reset notification registration to enroll again."
        case .rateLimited(let secs):
            return "Tested a moment ago — try again in \(secs)s."
        case .failed(let reason):
            return "The computer could not send it: \(reason)"
        case .unknown(let status):
            return "The computer answered “\(status)”, which this build does not know."
        case .none:
            return "No answer from the computer."
        }
    }

    private var pushDeniedInSettings: Bool { pushPermission == .denied }

    /// The ladder, each rung a fact the reader can act on. Live permission
    /// state, re-read on appearance — the stored boolean goes stale the moment
    /// the user visits Settings.
    private var pushReason: String? {
        guard let capabilities = model.connection.capabilities else {
            return "Not connected. Your computer hasn't said what it can do yet."
        }
        guard capabilities.pushMode != .none else {
            return "Your computer doesn't send notifications, so there is nothing to test."
        }
        guard capabilities.testsPush else {
            return "Your computer's CodeConnect is too old to send a test. Update it."
        }
        // Relay mode has an enrollment step direct mode does not; its states fold
        // in here so the one button explains why it is waiting or stuck.
        if capabilities.pushMode == .relay {
            // A bootstrap/static connection has no device row, so enrollment can
            // never begin — say that plainly instead of "Setting up…" forever.
            guard model.connection.helloAck?.deviceID != nil else {
                return "This iPhone is connected without pairing. Pair it with your computer to receive notifications."
            }
            switch model.relayPushState {
            case .unsupported:
                return "This iPhone can’t use CodeConnect’s push relay — it has no App Attest support. Everything else still works; a computer with its own Apple push key can notify you directly."
            case .enrolling:
                return "Enrolling this iPhone with the push relay…"
            case .failed(let reason):
                return "Couldn’t enroll with the push relay: \(reason)"
            case .idle:
                // Relay mode with no credential yet: there is nothing for the
                // daemon to send to, so a test would only report a missing token.
                return "Setting up notifications on this iPhone…"
            case .ready:
                break
            }
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
}



