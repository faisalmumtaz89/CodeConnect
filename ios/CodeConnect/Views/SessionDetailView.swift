import SwiftUI
import UIKit

/// What a developer most often tells an agent from a phone, most-used first.
///
/// **No "Stop" chip**, and the reason has narrowed rather than gone away. It
/// used to read *"an injected message needs the Mac's composer to be present,
/// so it cannot interrupt a running turn — a chip that reads like an interrupt
/// would be a control the product does not have"*. That is still exactly right
/// about `send_text` and Claude's TTY, and it is **not** right about Codex,
/// whose `interrupt` really does abort the turn over its own control link.
///
/// So Stop exists now, for a Codex session, as a real control with its own
/// affordance — and it is still not a chip here, because a chip *inserts text*
/// and a stop is a mutation. A control that looks like a phrase to type and
/// behaves like an abort would be the same lie in a new place.
///
/// Internal (not view-private) solely so the tests can pin the set.
enum ComposerTemplates {
    static let all = [
        "Continue", "Fix it", "Run the tests", "Commit & push", "Explain this",
        "Use a simpler approach",
    ]
}

/// The tail-following verdict, deliberately outside the screen's own state.
///
/// `following` flips as `TailObserver` reads the scroll offset, and as
/// `SessionDetailView` `@State` every flip re-evaluated the whole screen —
/// timeline included. With a taller-than-screen row expanded, that re-entry
/// re-windows the LazyVStack and re-lays the giant text: a measured limit
/// cycle that pegged the main thread for 30s+ (AttributeGraph churn under
/// `sample`; a build with no writes laid the same content out in under a
/// second). As `@Observable` state read only by `TailPill`, a flip repaints
/// one capsule and the timeline subtree is never re-entered — the cycle has
/// no fuel.
@MainActor
@Observable
final class TailWatch {
    /// Auto-scroll follows the tail until the reader scrolls away from it.
    var following = true
    /// Rows that arrived while the reader was away, for the pill's label.
    var newSinceLeaving = 0
    /// The reader deliberately moved away from the tail — a hand-scroll up,
    /// or opening a long message to read it. Set synchronously, cleared by
    /// `arrive`. `following` flips through a deliberate 400ms debounce so
    /// the pill cannot flicker; this flag exists precisely for that window,
    /// where an arriving event would otherwise yank the reader back to the
    /// tail and slip past the "N new" count. Ignored by Observation
    /// (nothing renders it) and raw on purpose — the debounce is for the
    /// pill; intent must not wait.
    @ObservationIgnored private(set) var handAway = false

    /// Away for every purpose but the pill's own visibility: the follow
    /// suppresses on this, and the "N new" counter counts on it. `following`
    /// alone lags real departures by the debounce — events landing in that
    /// lag used to both yank the reader and go uncounted.
    var isAway: Bool { handAway || !following }
    /// The one pending verdict — arrivals replace departures and vice versa.
    /// Ignored by Observation: replacing a timer is bookkeeping, not a fact
    /// any view renders.
    @ObservationIgnored private var verdict: Task<Void, Never>?

    /// **Both verdicts defer their writes; only the cancel is synchronous.**
    /// `arrive`/`depart` fire from the scroll view's offset stream, many
    /// times per gesture and — via KVO's `.initial` — from inside layout
    /// when the probe attaches. A synchronous state write there re-enters
    /// layout, and an earlier mechanism that wrote synchronously from layout
    /// callbacks livelocked exactly that way: a single 30s+ update cycle of
    /// repeated full-content `sizeThatFits` (`sample`; with the writes
    /// removed the same content settled in under a second). Cancel-and-
    /// replace is inert to layout, so a burst just swaps tasks, and
    /// whichever verdict survives writes once, between transactions.
    func arrive() {
        handAway = false
        verdict?.cancel()
        verdict = Task {
            guard !Task.isCancelled else { return }
            if !following { following = true }
            if newSinceLeaving != 0 { newSinceLeaving = 0 }
        }
    }

    /// The reader deliberately left the tail — dragged up, or expanded a
    /// message to read it. The pill still waits out the debounce; the
    /// follow-yank guard takes effect this instant. Self-correcting when
    /// the intent turns out not to have left the tail at all (a short
    /// expansion whose end stays on screen): the very next offset reading
    /// at the bottom calls `arrive`.
    func leave() {
        handAway = true
        depart()
    }

    /// Departure also outwaits one follow animation (0.22s): appending a row
    /// moves the content end away an instant before the follow-scroll
    /// catches up, and that transient must meet its `arrive()` before the
    /// verdict lands. A real departure meets the pill ~0.4s late.
    ///
    /// Called *without* `leave()` only when the tail slid away with no
    /// deliberate act this side can name — a rotation, a keyboard reshape.
    /// The two acts that *are* nameable — a hand-scroll up, and expanding a
    /// message — go through `leave()`, so no event in the debounce window
    /// can scroll a reader out of either.
    func depart() {
        verdict?.cancel()
        verdict = Task {
            try? await Task.sleep(for: .milliseconds(400))
            guard !Task.isCancelled else { return }
            // Guarded like `arrive`'s writes, and for the same reason:
            // Observation notifies on every set with no equality check, so an
            // unguarded `false` over `false` — one per row the reader scrolls
            // across while away — schedules a fresh pill animation
            // transaction each time. Measured as an app that never went
            // quiescent while scrolling the history: UI-test snapshots
            // starved at 30s apiece.
            if following { following = false }
        }
    }
}

/// Reads "is the reader at the bottom" off the hosting `UIScrollView`'s own
/// content offset, and feeds `TailWatch`.
///
/// UIKit introspection is the *last* resort, and every native route was
/// measured broken on this OS before it was taken: scroll-offset preferences
/// never fire; the tail sentinel's `onAppear`/`onDisappear` report the lazy
/// window, which runs a screen past the viewport, so real departures went
/// unnoticed and window-edge flapping livelocked layout; and
/// `scrollPosition(id:)` over a taller-than-screen row froze the screen
/// outright — frames identical 175 seconds apart. KVO on `contentOffset` is
/// exact viewport truth, delivered outside SwiftUI's layout, and writes
/// nothing SwiftUI lays out — the verdicts go through `TailWatch`'s deferred
/// tasks and repaint one capsule.
private struct TailObserver: UIViewRepresentable {
    let watch: TailWatch

    func makeUIView(context: Context) -> Probe { Probe(watch: watch) }
    func updateUIView(_ probe: Probe, context: Context) { probe.watch = watch }

    final class Probe: UIView {
        var watch: TailWatch
        private var watchers: [NSKeyValueObservation] = []
        /// The last offset seen, for telling hand motion away from the tail
        /// apart from content growing under a stationary reader.
        private var lastOffsetY = CGFloat.zero

        init(watch: TailWatch) {
            self.watch = watch
            super.init(frame: .zero)
            isUserInteractionEnabled = false
        }

        @available(*, unavailable)
        required init?(coder: NSCoder) { fatalError("not from a nib") }

        override func didMoveToWindow() {
            super.didMoveToWindow()
            guard window != nil else {
                watchers = []
                return
            }
            var view = superview
            while view != nil, !(view is UIScrollView) { view = view?.superview }
            guard let scrollView = view as? UIScrollView else { return }
            // All three geometry inputs, not offset alone: expanding the
            // last message grows `contentSize` under an unchanged offset,
            // and a keyboard or rotation reshapes `bounds` — either moves
            // the tail out of view with no offset event at all.
            watchers = [
                scrollView.observe(\.contentOffset, options: [.initial, .new]) {
                    [weak self] scrollView, _ in
                    MainActor.assumeIsolated { self?.read(scrollView) }
                },
                scrollView.observe(\.contentSize) { [weak self] scrollView, _ in
                    MainActor.assumeIsolated { self?.read(scrollView) }
                },
                scrollView.observe(\.bounds) { [weak self] scrollView, _ in
                    MainActor.assumeIsolated { self?.read(scrollView) }
                },
            ]
        }

        private func read(_ scrollView: UIScrollView) {
            let dy = scrollView.contentOffset.y - lastOffsetY
            lastOffsetY = scrollView.contentOffset.y
            // Content shorter than the viewport has no "away" to be.
            let span = scrollView.contentSize.height
                - (scrollView.bounds.height - scrollView.adjustedContentInset.bottom
                    - scrollView.adjustedContentInset.top)
            guard span > 0 else {
                watch.arrive()
                return
            }
            let bottomEdge = scrollView.contentOffset.y + scrollView.bounds.height
                - scrollView.adjustedContentInset.bottom
            // One row-spacing of slack absorbs sub-pixel rounding and
            // rubber-banding; without it the verdict flaps at rest.
            if bottomEdge >= scrollView.contentSize.height - 32 {
                watch.arrive()
            } else if dy < -0.5 {
                // The offset moved *up*: only a hand (or its deceleration)
                // does that. Content growth and follow-scrolls move it down
                // or not at all.
                watch.leave()
            } else {
                watch.depart()
            }
        }
    }
}

/// The semantic timeline for one session, plus the two things you can do to it:
/// answer a card, or say something.
///
/// The identity block replaces the ambiguity of a bare nav title. The nav bar is
/// left with nothing but a back chevron on purpose: the pushed screen's own
/// header carries the project, and a project name is longer than a title bar
/// can render honestly.
struct SessionDetailView: View {
    let route: SessionRoute

    /// Timeline is what the daemon *knows*; Terminal is the Mac's own TTY. They
    /// are different kinds of truth and the app never blends them.
    enum Surface: String, CaseIterable, Identifiable {
        case timeline, terminal
        var id: String { rawValue }
        var label: String { self == .timeline ? "Timeline" : "Terminal" }
    }

    @Environment(AppModel.self) private var model
    /// Read for one decision only: the Stop control's label. "Stop this turn"
    /// clipped mid-word at AX5 and grew the header enough to push the pending
    /// card's own controls off screen.
    @Environment(\.dynamicTypeSize) private var screenTypeSize
    /// The widest tool label in this timeline, so every command beside one
    /// starts on the same edge. See `CCToolColumn`.
    @State private var toolColumn: CGFloat = 0
    @State private var composeText = ""
    @State private var openApproval: ApprovalItem?
    @State private var composeResult: ComposeAttempt?
    @State private var showModelSheet = false
    @State private var modelSheetPrefill = ""
    @State private var showEffortSheet = false
    @State private var showCompactSheet = false
    @State private var compactSheetPrefill = ""
    @State private var showClearConfirm = false
    @State private var snapshotCommand: SnapshotCommand?
    @State private var composeResultClearTask: Task<Void, Never>?
    @State private var sending = false
    /// The tail-following verdict — a box read only by `TailPill`, never by
    /// this body. See `TailWatch` for the measured limit cycle that scoping
    /// prevents.
    @State private var tailWatch = TailWatch()
    /// The decision a deep link or a route asked to land on, held until the
    /// card exists. `pendingApprovals` is filled by ingest after the frames
    /// arrive, so the id is routinely consumed before its card is there — on
    /// every launch, not only a slow one. Spent when the sheet opens, or when
    /// the screen goes; never on a miss.
    @State private var wantedRequestID: String?
    @State private var ownSurface: Surface = .timeline
    /// Whether the Terminal surface has ever been selected on this screen. The
    /// emulator is kept alive once built, and this is what keeps it in the view
    /// tree while the timeline is the one on show.
    @State private var terminalHasBeenOpened = false
    #if DEBUG
        /// Test seam: drive the picker from outside.
        ///
        /// The terminal's emulator has to survive a trip to the timeline and
        /// back, and that is a claim about one `UIView` instance rather than
        /// about anything a value can report. Proving it needs the switch
        /// thrown against a hosted screen, and the switch is `@State`.
        var surfaceForTesting: Binding<Surface>?
    #endif

    /// The picker's selection: this screen's own, or a test's.
    private var surfaceBinding: Binding<Surface> {
        #if DEBUG
            if let surfaceForTesting { return surfaceForTesting }
        #endif
        return $ownSurface
    }
    private var surface: Surface { surfaceBinding.wrappedValue }
    @State private var showDiff = false
    @State private var showLinkDetail = false
    @State private var appearedAt = Date()
    /// The composer's focus, owned here rather than inside the bar: the
    /// screen's chrome collapses around the keyboard, and only the screen can
    /// do the collapsing.
    @FocusState private var composerFocused: Bool

    /// The run this screen is about. Everything on it — the timeline, the
    /// compose bar, the diff, the terminal — is scoped to this one key, so a
    /// `cc-1` that exits while the screen is open cannot hand the screen to its
    /// successor.
    private var key: String { route.key }
    private var state: SessionState? { model.states[key] }
    private var summary: SessionSummary? { model.summary(for: key) }
    /// What to call this run — see `RunLabel`. The same words the fleet row,
    /// the card and the lock screen use.
    private var label: RunLabel { model.runLabel(for: key) }

    var body: some View {
        VStack(spacing: 0) {
            // While the keyboard is up, the header earns its height or loses
            // it: the identity collapses to one line and the surface picker —
            // an intent nobody has mid-sentence — steps aside, so the freed
            // ~150pt goes to the conversation being typed at.
            if composerFocused {
                compactIdentityLine
            } else {
                identityBlock
            }
            // The surface picker lives in the content, not the navigation bar.
            // A segmented control in a `.principal` slot squeezes the trailing
            // items into an overflow menu on a phone — the diff button
            // disappeared behind a "…" — and it clips outright at large Dynamic
            // Type sizes. Below the bar it has the whole width and grows.
            // Absent in the sample fleet, like the link pill: the terminal is
            // the same connection as everything else, and the sample has none.
            // With the picker gone the timeline default is the only surface,
            // so the tab that would answer in pairing vocabulary cannot be
            // reached at all — offered nothing, not offered a dead end.
            if !composerFocused && !model.sampleFleetActive {
                CCSegmented(
                    selection: surfaceBinding,
                    options: Surface.allCases.map { CCSegmentedOption($0, title: $0.label) },
                    accessibilityLabel: "Session surface")
                .padding(.horizontal, CC.space.md)
                .padding(.top, CC.space.lg)
                // Nothing under the picker on the Terminal side: the liveness
                // strip sits *directly beneath* it and carries no top padding
                // of its own, so the 12 that used to sit here measured as a
                // 12pt seam of `bg` between two elements that are meant to
                // touch. The timeline keeps its 12 — a scroll view abutting a
                // control is not the same relationship.
                .padding(.bottom, surface == .terminal ? 0 : CC.space.sm)
                .background(CC.color.bg)
            }

            // **Two slots, not two branches.** A `switch` here puts the two
            // surfaces in one structural position, so flipping the picker
            // dismantles whichever was showing — and the terminal's emulator
            // is not a view that can be rebuilt from its inputs. It *is* the
            // scrollback: `cargo build` scrolls megabytes through it, the
            // carrier keeps only the last 224 KiB, and a glance at the
            // timeline would throw the rest away while the session is still
            // running. Separate `if`s keep the terminal in a slot of its own,
            // so it is hidden rather than destroyed.
            ZStack {
                if surface == .timeline {
                    if let state {
                        timeline(state)
                    } else {
                        notInTheList
                    }
                }
                // Built the first time it is asked for and kept after that.
                // Mounting it with the screen would cost every reader an
                // emulator they may never open; keeping it once opened costs
                // one, and buys back the pane they were reading.
                if surface == .terminal || terminalHasBeenOpened {
                    // Attached **by uid**: the daemon resolves the uid to the
                    // one live session that carries it, so a reused tmux name
                    // can never hand this tab a different agent's keyboard.
                    // The name is carried alongside only as something to show.
                    TerminalTabView(
                        sessionUID: key,
                        tmuxName: model.tmuxName(for: key),
                        unhosted: model.summary(for: key)?.tmuxSession.isEmpty == true,
                        runLabel: label.spoken,
                        onScreen: surface == .terminal
                    )
                    .opacity(surface == .terminal ? 1 : 0)
                    // A hidden pane takes no taps and is not read out. The
                    // keyboard is `onScreen`'s to give back — see
                    // `TerminalPaneView.acceptsInput`.
                    .allowsHitTesting(surface == .terminal)
                    .accessibilityHidden(surface != .terminal)
                }
            }
        }
        .background(CC.color.bg)
        // One coordinated animation for the chrome swap around the keyboard —
        // the header and picker collapse and return as a unit. The focus
        // change itself is never wrapped in `withAnimation`; the keyboard owns
        // its own transition and fights any second one.
        .onChange(of: surface, initial: true) { _, new in
            if new == .terminal { terminalHasBeenOpened = true }
        }
        .ccAnimation(CC.motion.small, value: composerFocused)
        .ccNavigationChrome()
        // Empty on purpose. The header below carries the project, at a width
        // the bar could never give it.
        .navigationTitle("")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) { diffControl }
                .ccPlainToolbarItem()
            ToolbarItem(placement: .topBarTrailing) {
                // Interactive everywhere. A control that looks tappable and is
                // not is worse than no control.
                SessionLinkPill { showLinkDetail = true }
            }
            .ccPlainToolbarItem()
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            // The compose bar types into Claude's prompt. In the terminal you
            // are already typing at the TTY, so a second text field there would
            // be two ways to say the same thing with different consequences.
            if surface == .timeline {
                VStack(spacing: 0) {
                    if let palette = paletteContent {
                        CommandPalette(content: palette, onAction: route)
                    }
                    SessionComposeBar(
                        text: $composeText,
                        result: composeResult,
                        sending: sending,
                        runLabel: label.spoken,
                        summary: summary,
                        onWhy: { showLinkDetail = true },
                        onSend: send,
                        focused: $composerFocused)
                }
                .onChange(of: composeText) { _, _ in
                    // Editing is the user's next act; a standing failure
                    // note has been read. Success notes retire themselves.
                    if case .refused = composeResult {
                        withAnimation(CC.motion.micro) { composeResult = nil }
                    } else if case .failed = composeResult {
                        withAnimation(CC.motion.micro) { composeResult = nil }
                    } else if case .indeterminate = composeResult {
                        withAnimation(CC.motion.micro) { composeResult = nil }
                    }
                }
            }
        }
        .sheet(isPresented: $showModelSheet) {
            ModelSheet(sessionKey: key, prefill: modelSheetPrefill, onLanded: consumeDraft)
                .environment(model)
                .ccResizableSheet()
        }
        .sheet(isPresented: $showEffortSheet) {
            EffortSheet(sessionKey: key, onLanded: consumeDraft)
                .environment(model)
                .ccResizableSheet()
        }
        .sheet(isPresented: $showCompactSheet) {
            CompactSheet(sessionKey: key, prefill: compactSheetPrefill, onLanded: consumeDraft)
                .environment(model)
                .ccResizableSheet()
        }
        .sheet(item: $snapshotCommand) { command in
            SnapshotSheet(
                sessionKey: key, command: command,
                onLanded: consumeDraft,
                onOpenTerminal: {
                    snapshotCommand = nil
                    surfaceBinding.wrappedValue = .terminal
                }
            )
            .environment(model)
            // **One detent, and it is the tall one.** This sheet carries a
            // captured screen; every point of height is another row of the
            // Mac's grid the reader does not have to scroll for. Offering a
            // second detent would not have opened it tall anyway —
            // `presentationDetents` takes a `Set`, so the order written is
            // not an order at all.
            .ccTallSheet()
        }
        .alert("Clear Claude’s context?", isPresented: $showClearConfirm) {
            Button("Clear context", role: .destructive) { performClear() }
            Button("Keep context", role: .cancel) {}
        } message: {
            Text(
                "Claude Code will forget this conversation’s context. "
                    + "CodeConnect’s timeline will stay here.")
        }
        .sheet(item: $openApproval) { approval in
            // `openApproval` is only the sheet's IDENTITY (and a last-known
            // display). `DecisionCardView` re-derives the authoritative, live
            // card from `AppModel.liveApproval(...)` on every render, so a
            // crash-recovery rebuild, a resolution, or the session leaving the
            // fleet all reach the open sheet: the outcome wins the banner and the
            // action bar disables, or — with no live backing at all — the card
            // shows a non-actionable "no longer available" state. It can never
            // present a frozen actionable snapshot.
            DecisionCardSheet(approval: approval)
                .environment(model)
        }
        .sheet(isPresented: $showDiff) {
            DiffSheet(key: key)
                .environment(model)
        }
        .sheet(isPresented: $showLinkDetail) { LinkHealthSheet() }
        .onAppear {
            appearedAt = Date()
            state?.markReviewed()
            // Arrived from a "Done, unreviewed" row: what you came for is the
            // diff, so it opens without a second tap.
            if route.openDiff { showDiff = true }
            if let requestID = route.openRequestID { wantedRequestID = requestID }
            consumeDeepLink()
            openWantedApproval()
        }
        .onDisappear {
            state?.markReviewed()
            composeResultClearTask?.cancel()
            wantedRequestID = nil
        }
        .onChange(of: model.pendingDeepLink) { _, _ in consumeDeepLink() }
        // The card the link named arriving is the moment to open it.
        .onChange(of: state?.pendingApprovals.map(\.card.requestID) ?? []) { _, _ in
            openWantedApproval()
        }
    }

    /// One line of who this is, for while the keyboard owns the screen: the
    /// status dot and the identity, nothing else. Everything the full block
    /// carries — folder, cwd, freshness, the permission note — is context a
    /// reader mid-sentence has already absorbed, and it returns the moment
    /// focus ends.
    private var compactIdentityLine: some View {
        HStack(spacing: CC.space.sm) {
            CCStatusDot(
                color: dotColour,
                size: CCStatusDot.Size.cardHeader.rawValue,
                isHollow: state?.loadedFromCacheAt != nil && state?.hasLiveData != true,
                pulses: (state?.pendingApprovals.isEmpty == false))
            Text(verbatim: label.project)
                .ccType(CC.type.headline)
                .foregroundStyle(CC.text.primary)
                .lineLimit(1)
                // From the front, as everywhere else a project is drawn.
                .truncationMode(.tail)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, CC.space.sm)
        .background(CC.color.surface)
        .overlay(alignment: .bottom) { CCHairline() }
    }

    // MARK: Identity block

    /// Chrome, not content: `surface` fill and a 1pt bottom rule, no card.
    private var identityBlock: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .top, spacing: TimelineSpine.gap) {
                CCStatusDot(
                    color: dotColour,
                    size: CCStatusDot.Size.cardHeader.rawValue,
                    isHollow: state?.loadedFromCacheAt != nil && state?.hasLiveData != true,
                    pulses: (state?.pendingApprovals.isEmpty == false))
                // The same gutter the timeline's glyphs sit in, so the block's
                // three lines start on the screen's one content column — 52,
                // the number the fleet's rows use.
                .frame(width: TimelineSpine.gutter, alignment: .trailing)
                .padding(.top, CC.space.xs)

                VStack(alignment: .leading, spacing: 2) {
                    HStack(alignment: .firstTextBaseline, spacing: CC.space.sm) {
                        Text(verbatim: label.project)
                            .ccType(CC.type.title)
                            .foregroundStyle(CC.text.primary)
                            .lineLimit(2)
                            .truncationMode(.tail)
                        Spacer(minLength: CC.space.xs)
                        SessionFreshness(lastEventAt: state?.lastEventAt)
                    }
                    if let qualifier = label.qualifier {
                        Text(verbatim: qualifier)
                            .ccType(CC.type.micro)
                            .foregroundStyle(CC.text.tertiary)
                            .lineLimit(1)
                    }
                    // `textDisabled` is permitted here — one of its few allowed
                    // positions — because the project name above it carries the
                    // same fact at full contrast, so nothing is only readable in
                    // the dimmed run. Head truncation so the tail survives:
                    // `…/GitHub/CodeConnect` is the part that identifies.
                    if let cwd = summary?.cwd {
                        Text(cwd)
                            .ccType(CC.type.monoSmall)
                            .foregroundStyle(CC.text.disabled)
                            .lineLimit(1)
                            .truncationMode(.head)
                            .onLongPressGesture { CCPasteboard.copy(cwd) }
                            .accessibilityLabel("Working directory, \(cwd)")
                    }
                    // **An ended run is not a dead end.** Claude Code keeps its own
                    // transcript under `~/.claude/projects`, so the conversation
                    // outlives our record of it and can be picked up at the Mac.
                    // Shown only when the daemon actually recorded the id — it is
                    // learned from a hook after the run starts, so a run that died
                    // early may not have one, and a command with a blank id would
                    // be worse than none.
                    //
                    // Resume happens at the Mac by design: the phone starting
                    // processes on your machine is a different product, and the
                    // daemon spawns nothing today.
                    if summary?.lifecycle == .exited,
                        let claudeID = summary?.claudeSessionID, !claudeID.isEmpty
                    {
                        VStack(alignment: .leading, spacing: CC.rhythm.text) {
                            Text("Pick this conversation up at the Mac")
                                .ccType(CC.type.footnote)
                                .foregroundStyle(CC.text.secondary)
                            // Quoted, because this is a command a person copies
                            // and runs. The id is whatever a hook reported —
                            // decoded as an unrestricted string, never validated
                            // — and a space in it would silently resume the
                            // wrong thing while anything shell-special would run
                            // as syntax. Quoting costs nothing on the UUID this
                            // is in practice.
                            CCMonoBlock("codeconnect claude --resume \(Shell.quoted(claudeID))")
                        }
                        .padding(.top, CC.rhythm.textSurface)
                    }
                    // Stated here rather than left to be inferred from an empty
                    // screen. `textTertiary`, not a banner and not a warning:
                    // deciding for itself is a thing the operator chose, and a
                    // product that shouts about a deliberate setting teaches
                    // people to ignore the places it shouts.
                    if let notice = state?.silentBecauseOfPermissions {
                        Text(notice)
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.text.tertiary)
                            .fixedSize(horizontal: false, vertical: true)
                            .padding(.top, CC.space.xxs)
                            .accessibilityIdentifier("session-permission-notice")
                    }
                    codexStopBlock
                }
            }
            .padding(.horizontal, CC.space.md)
            .padding(.vertical, CC.space.md)

            CCHairline()
        }
        .background(CC.color.surface)
        .accessibilityElement(children: .contain)
    }

    // MARK: Stop

    /// **Stop, in the session header**, where the reader is already looking at
    /// the turn they want to end.
    ///
    /// Absent when the phone holds no turn to name — the one honest hide. There
    /// is no "turn started" fact on the Codex wire at all, so the running turn
    /// is derived from the event envelopes, and a session with none has nothing
    /// this control could send.
    @ViewBuilder
    private var codexStopBlock: some View {
        // **One reader of the rule.** `stopUnavailable(for:)` is the same
        // function `AppModel.stopCodexTurn` consults before it mints anything,
        // so the control and the frame can never disagree — which they did:
        // this checked agent + capability + turn and left the link state to the
        // send path, which did not check it either.
        if model.stopUnavailable(for: key) == nil {
            CCButton(
                // **Two words at AX5, and that is not cosmetic.** "Stop this
                // turn" clipped mid-word to `▪ this` in a box the header had
                // grown to fit — and the header growing is the real cost: it
                // squeezed the timeline until the pending card's own `Review`
                // button sat under the compose bar, unreachable by scrolling.
                // Measured on `codex-card-command-worst--ax5.png`. The header
                // carries the control; its outcome goes where every other
                // mutation's outcome already goes, above the composer.
                stopLabel,
                icon: isStopping ? nil : "stop.fill",
                variant: .secondary,
                size: .md,
                isLoading: isStopping,
                disabledReason: CCDisabledReason(stopGreyedReason)
            ) {
                stopCodexTurn()
            }
            .accessibilityIdentifier("stop-\(key)")
            .accessibilityLabel("Stop the turn this session is running")
            .padding(.top, CC.space.sm)
        }
    }

    private var stopLabel: String {
        if isStopping { return "Stopping…" }
        return screenTypeSize.isAccessibilitySize ? "Stop" : "Stop this turn"
    }

    private var isStopping: Bool {
        if case .inFlight = model.codexControls(for: key).stop { return true }
        return false
    }

    /// Why the control is greyed, or nil.
    ///
    /// **Short, and only during the cooldown** — the same rule the fleet row
    /// follows, and for the same two reasons. Not the link state: the summary
    /// cannot tell a subscribed link from a reconnecting one (A29a), so Stop is
    /// offered and the daemon's refusal is what the operator reads. And not the
    /// daemon's sentence: the composer's own standing note is already printing
    /// it verbatim, and handing it to the button as well prints the same refusal
    /// twice — measured on the fleet, where it also rendered as a twelve-line
    /// column that pushed the row out of line with its Claude neighbour.
    private var stopGreyedReason: String? {
        model.codexControls(for: key).isStopGreyed(now: model.now)
            ? "Try again in a moment." : nil
    }

    private var dotColour: Color {
        guard let summary else { return CC.text.tertiary }
        let status = FleetStatusRule.status(
            summary: summary, state: state,
            reviewedSeq: ReviewMarks.reviewedSeq(for: key))
        return status.ccDotColor
    }

    // MARK: Toolbar

    /// Not an 11pt system glyph. A bordered pill carrying the changed-file count
    /// when the app has actually counted them, and a bare `±` when it has not —
    /// a number the app cannot evidence is a number it does not print.
    private var diffControl: some View {
        Button {
            CCHaptic.light.fire()
            showDiff = true
        } label: {
            HStack(spacing: CC.space.xxs + 1) {
                Text("±")
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.primary)
                if let count = changedFileCount {
                    Text("\(count)")
                        .ccType(CC.type.monoSmall)
                        .foregroundStyle(CC.text.primary)
                }
            }
            .padding(.horizontal, CC.space.sm)
            .frame(minWidth: CC.size.controlSm, minHeight: CC.size.controlSm)
            .background(CC.color.surfaceRaised, in: Capsule())
            .overlay { Capsule().strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline) }
            .ccHitTarget()
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Diff")
        .accessibilityHint("Asks the Mac for this session's uncommitted changes")
        .accessibilityIdentifier("open-diff")
    }

    private var changedFileCount: Int? {
        guard case .loaded(_, let parsed, _) = model.diffs[key] else { return nil }
        return parsed.files.isEmpty ? nil : parsed.files.count
    }

    /// A deep link aimed at *this* session opens what it asked for.
    private func consumeDeepLink() {
        switch model.pendingDeepLink {
        case .diff(let reference) where addresses(reference):
            _ = model.consumeDeepLink()
            showDiff = true
        case .session(let reference, let requestID) where addresses(reference):
            _ = model.consumeDeepLink()
            if let requestID { wantedRequestID = requestID }
            openWantedApproval()
        default:
            break
        }
    }

    /// Open the decision a link or a route asked for, if its card is here.
    /// A miss keeps the request standing; `pendingApprovals` changing is what
    /// asks again.
    private func openWantedApproval() {
        guard let requestID = wantedRequestID, let state,
            let approval = state.pendingApprovals.first(where: { $0.card.requestID == requestID })
        else { return }
        wantedRequestID = nil
        openApproval = approval
    }

    /// Does this link mean *this* run? A link carries a reference — a uid or a
    /// tmux name — and the model resolves a name the way the daemon does.
    private func addresses(_ reference: String) -> Bool {
        (model.resolveSessionKey(reference: reference) ?? reference) == key
    }

    // MARK: Timeline

    private func timeline(_ state: SessionState) -> some View {
        ScrollViewReader { proxy in
            // The pill rides an overlay, never a stack: an overlay is
            // positioned after its base and cannot feed back into the base's
            // layout, while a ZStack sizes itself from all children — and
            // that coupling, with an animated sibling over this scroll view,
            // was measured as a permanent layout loop (identical frames 175s
            // apart, ages frozen, `explicitAlignment` pegged in `sample`).
            scrollBody(state, proxy: proxy)
                .overlay(alignment: .bottomTrailing) { jumpToLatest(proxy) }
        }
    }

    private func scrollBody(_ state: SessionState, proxy: ScrollViewProxy) -> some View {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    SessionBanner(state: state) { showLinkDetail = true }

                    if state.headTruncated {
                        // At its position in time — the very top — rather than
                        // as a banner floated above the list.
                        CCGapMarker(
                            label: model.isLoadingEarlier(key)
                                ? "Earlier history not loaded · Loading…"
                                : "Earlier history not loaded · Load all",
                            actionLabel: "Load the earlier events",
                            action: model.isLoadingEarlier(key)
                                ? nil : { model.loadEarlier(key: key) })
                    }

                    if state.timeline.isEmpty {
                        emptyTimeline
                    }

                    ForEach(Array(state.timeline.enumerated()), id: \.element.id) { index, item in
                        TimelineRow(
                            item: item, profile: model.daemonProfile,
                            // Fired *before* the collapse — see
                            // `AgentMessageRow` — so the target is the row in
                            // its expanded geometry, which is always reachable.
                            // Anchored `.top`: the message the reader was
                            // inside comes back under their eyes, and the
                            // height that vanishes goes from below it.
                            onCollapse: { id in
                                proxy.scrollTo(id, anchor: .top)
                            },
                            // Expanding is choosing to read history: raise
                            // the away guard this instant, or an event
                            // landing in the next 400ms scrolls the reader
                            // through the whole message they just opened.
                            onExpand: { tailWatch.leave() }
                        ) { approval in
                            openApproval = approval
                        }
                        // 8pt inside a turn, 20pt between turns. One rule, and it
                        // does more for readability than any amount of styling.
                        .padding(.top, topSpacing(at: index, in: state.timeline))
                        .id(item.id)

                        if let gap = state.gap, index == gapIndex(state.timeline, gap: gap) {
                            CCGapMarker(
                                label: gap.message,
                                actionLabel: "Dismiss this notice",
                                action: { state.dismissGap() })
                        }
                    }

                    // The scroll target for "go to the latest" — nothing but
                    // an identity; the at-the-tail verdict lives in
                    // `TailObserver`, off the real scroll offset. The list's
                    // bottom breathing room rides inside this child so the
                    // target includes it.
                    Color.clear
                        .frame(height: 1)
                        .padding(.bottom, CC.space.lg)
                        .id(Self.tailAnchor)
                }
                .padding(.horizontal, CC.space.md)
                // Inside the scroll content on purpose: the probe walks its
                // superviews to the hosting UIScrollView. See `TailObserver`
                // for why the verdict reads UIKit's own offset.
                .background(TailObserver(watch: tailWatch))
            }
            .scrollIndicators(.hidden)
            // The Apple-standard dismissal pair. The drag tracks the keyboard
            // interactively; the tap fires only where nothing more specific
            // claims it — rows' buttons, links and text selection all win the
            // gesture arbitration, so this is precisely "tapping the empty
            // background" and never a stolen control tap.
            .scrollDismissesKeyboard(.interactively)
            // Simultaneous, not exclusive: with the keyboard up the visible
            // timeline is mostly selectable text, which consumes an exclusive
            // tap — measured, the "background" tap never fired. Simultaneous
            // lets a tap on prose or true background dismiss while buttons
            // keep winning their own taps; the keyboard-owning field itself is
            // outside this subtree, so typing never self-dismisses.
            .simultaneousGesture(TapGesture().onEnded { composerFocused = false })
            // Named so a UI test can tell this scroll view from the keyboard's
            // own input-assistant bar, which is also a scroll view and wins
            // `firstMatch` while the keyboard is up — measured, and exactly the
            // kind of impostor a coordinate tap then presses keys on.
            .accessibilityIdentifier("session-timeline")
            .ccCollectsToolColumn(into: $toolColumn)
            .background(CC.color.bg)
            .onChange(of: state.timeline.count) { old, new in
                // `isAway`, not `!following`: events landing inside the
                // departure debounce belong to "while you were away" too —
                // gated on the settled flag alone they went uncounted and
                // the pill said "Latest" over rows never seen.
                guard tailWatch.isAway else { return }
                tailWatch.newSinceLeaving += max(0, new - old)
            }
            // The follow trigger is the tail *item*, not the count: the
            // builder merges some events into the row they belong to, so the
            // last row can grow with no append — count would sit still while
            // the anchor is pushed out of view, and following would silently
            // end. Any change to the last item — new row or grown row — is
            // exactly "the tail moved".
            .onChange(of: state.timeline.last) { _, _ in
                // `isAway`, not `following`: the settled flag lags a real
                // departure by the debounce, and an event landing in that
                // lag must not yank a reader who just scrolled up or just
                // expanded a message back to the tail.
                guard !tailWatch.isAway else { return }
                withAnimation(CC.motion.medium) {
                    proxy.scrollTo(Self.tailAnchor, anchor: .bottom)
                }
            }
            .onAppear {
                proxy.scrollTo(Self.tailAnchor, anchor: .bottom)
                openWantedApproval()
            }
    }

    private static let tailAnchor = "codeconnect.tail"

    /// A turn boundary is a user message, a `turnComplete` or a `sessionEnded`.
    private func topSpacing(at index: Int, in items: [TimelineItem]) -> CGFloat {
        guard index > 0 else { return CC.space.xs }
        if case .userMessage = items[index].content { return CC.space.lg }
        if case .notice(let notice) = items[index - 1].content {
            switch notice.kind {
            case .turnComplete, .sessionEnded: return CC.space.lg
            default: break
            }
        }
        return CC.space.xs
    }

    /// Where in the log the discontinuity sits, so the marker can be drawn at
    /// its position in time rather than at the top of the screen.
    private func gapIndex(_ items: [TimelineItem], gap: GapNotice) -> Int {
        items.lastIndex { $0.date <= gap.at } ?? max(0, items.count - 1)
    }

    private var emptyTimeline: some View {
        // Anchored rather than centred, so the compose bar stays reachable —
        // you can always talk to an agent.
        CCEmptyState(
            glyph: "clock",
            title: "Nothing recorded yet",
            message: "Events appear here as the agent works.")
            // 96pt from the top of the list, counting the empty state's own
            // 32pt of vertical padding.
            .padding(.top, CC.space.xxxl + CC.space.md)
    }

    private var notInTheList: some View {
        VStack {
            CCEmptyState(
                glyph: "questionmark.folder",
                title: "Not in the daemon's list",
                message:
                    "This run may have exited, or the daemon has restarted since the link was made.",
                tone: .warning)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .padding(.top, CC.space.xxl)
        .background(CC.color.bg)
    }

    private func jumpToLatest(_ proxy: ScrollViewProxy) -> some View {
        TailPill(watch: tailWatch) {
            CCHaptic.light.fire()
            tailWatch.arrive()
            withAnimation(CC.motion.medium) {
                proxy.scrollTo(Self.tailAnchor, anchor: .bottom)
            }
        }
    }

    // MARK: Compose

    /// The palette's own visibility rule, gated on the keyboard being up.
    /// Catalog fetching keys off any leading `/` (broader than visibility on
    /// purpose — send-time classification needs the catalog even when the
    /// palette shows nothing).
    private var paletteContent: CommandPalette.Content? {
        guard composerFocused else { return nil }
        // **Claude Code's slash vocabulary, on a Claude session only.**
        //
        // `/model`, `/effort`, `/compact`, `/clear`, `/cost` are that binary's
        // built-ins. Offered on a Codex session they are wrong in both
        // directions: the palette would open a Claude sheet for a command that
        // session has never heard of, and the injection behind it is a
        // `send_text` the Mac refuses by name. A leading `/` on a Codex session
        // is just a message that happens to start with a slash, and it goes
        // through `compose` like any other.
        guard summary?.isCodex != true else { return nil }
        return CommandPalette.content(
            for: composeText,
            recoversComposer: model.connection.capabilities?.recoversComposer == true)
    }

    /// One place to show a compose outcome that did not go through `send()` —
    /// the same note strip, the same persistence rule: good news retires
    /// itself after four seconds, while a refusal stands until the text
    /// changes or another attempt begins. A blocked command's explanation
    /// vanishing mid-read was a shipped defect, not a style choice.
    private func noteComposeResult(_ result: ComposeAttempt) {
        withAnimation(CC.motion.micro) { composeResult = result }
        composeResultClearTask?.cancel()
        switch result {
        case .sent, .alreadyApplied, .composerRecovered:
            composeResultClearTask = Task {
                try? await Task.sleep(for: .seconds(4))
                guard !Task.isCancelled else { return }
                withAnimation(CC.motion.medium) { composeResult = nil }
            }
        case .refused, .failed, .indeterminate, .composerLost:
            break
        }
    }

    /// One router for both doors: a palette tap hands over the same
    /// `CommandAction` a typed send resolves to, so the tap and the text can
    /// never behave differently. The composer draft survives every route —
    /// the sheets consume it via `onLanded` only once keystrokes actually
    /// land on the Mac; a cancelled sheet leaves the draft exactly as typed.
    private func route(_ action: CommandAction) {
        switch action {
        case .nativeModel(let prefillArgs):
            modelSheetPrefill = prefillArgs
            showModelSheet = true
        case .nativeDiff:
            // The app renders the working tree itself; typing the Mac's own
            // `/diff` would open a view of something already on this screen.
            composeText = ""
            showDiff = true
        case .nativeEffort:
            showEffortSheet = true
        case .nativeCompact(let prefillInstructions):
            compactSheetPrefill = prefillInstructions
            showCompactSheet = true
        case .nativeClear:
            showClearConfirm = true
        case .nativeSnapshot(let command):
            snapshotCommand = command
        case .blocked(_, let reason):
            noteComposeResult(.refused(reason))
        case .passThrough:
            break
        }
    }

    /// A landed native operation consumes the draft that opened it — a
    /// stale `/model` left behind a successful sheet was a shipped defect.
    private func consumeDraft() {
        composeText = ""
    }

    // MARK: - Codex

    private var isCodexSession: Bool { model.summary(for: key)?.isCodex == true }

    /// **Say something to a Codex session.**
    ///
    /// The draft is consumed on `started`, `steered` and `duplicate` — every arm
    /// where the words are known to have landed once — and kept on a refusal and
    /// on `indeterminate`. Kept on `indeterminate` deliberately: the write may
    /// have landed, so the reader is shown the daemon's sentence and left
    /// holding their own text, rather than having it silently thrown away by a
    /// phone that could not say whether it arrived.
    private func sendToCodex(_ text: String) {
        sending = true
        composeResult = nil
        composeResultClearTask?.cancel()
        Task {
            let result = await model.composeToCodex(sessionKey: key, text: text)
            sending = false
            switch result {
            case .started, .steered, .duplicate:
                composeText = ""
                tailWatch.arrive()
            case .rejected, .indeterminate, .unknown, nil:
                // The draft is kept. On a refusal it can be retried as-is; on
                // an outcome nobody can account for the send path now refuses
                // an unchanged retry outright, so keeping the words is what
                // lets the reader edit them into a different message rather
                // than losing what they wrote.
                break
            }
        }
    }

    /// **Stop the running turn.**
    ///
    /// This exists, and the comment at the top of this file that says it cannot
    /// has been amended rather than worked around: *"an injected message needs
    /// the Mac's composer to be present, so it cannot interrupt a running turn"*
    /// is a fact about `send_text` and Claude's TTY. Codex's `interrupt` really
    /// aborts the turn, over its own control link, with no keyboard involved.
    private func stopCodexTurn() {
        Task { await model.stopCodexTurn(sessionKey: key) }
    }

    /// `/clear`, past its confirmation. The receipt claims typing, nothing
    /// more; "Conversation cleared." appears in the timeline when the
    /// rotated transcript's own `/clear` entry arrives — the observable
    /// fact, in the place records live.
    private func performClear() {
        sending = true
        composeResult = nil
        composeResultClearTask?.cancel()
        Task {
            let result = await model.sendClearCommand(to: key)
            sending = false
            switch result {
            case .sent, .alreadyApplied, .composerRecovered:
                consumeDraft()
                tailWatch.arrive()
            case .refused, .failed, .indeterminate, .composerLost:
                break
            }
            noteComposeResult(result)
        }
    }

    private func send() {
        let text = composeText
        // **A Codex session takes a different road entirely.** `send_text` is
        // Claude's TTY takeover and a Codex session refuses it by name; `compose`
        // is the message the app-server actually accepts, and it is not a
        // keystroke — there is no composer at the Mac to find, nothing to type
        // into, and no prompt-presence needle to satisfy.
        if isCodexSession {
            sendToCodex(text)
            return
        }
        // Slash commands answer to the policy before anything reaches the
        // Mac: native commands open their controls (measured: the Mac-side
        // picker forms lock the composer), dialog built-ins get the honest
        // refusal, and everything else — prose, custom skills — passes
        // through untouched.
        //
        // **Claude Code's vocabulary only.** `/model`, `/effort`, `/compact`,
        // `/clear`, `/diff` are that binary's built-ins; applied to a Codex
        // session they are wrong in both directions — the app would open a
        // Claude sheet for a session that has no such command, and the Mac
        // would refuse the injection anyway. The branch above is the gate.
        let action = ClaudeCommandPolicy.action(
            for: text,
            recoversComposer: model.connection.capabilities?.recoversComposer == true)
        guard case .passThrough = action else {
            route(action)
            return
        }
        sending = true
        composeResult = nil
        composeResultClearTask?.cancel()
        Task {
            let result = await model.send(text: text, to: key)
            sending = false
            withAnimation(CC.motion.micro) { composeResult = result }
            switch result {
            case .sent, .alreadyApplied, .composerRecovered:
                // Never optimistic: the field clears only on a landed
                // mutation — and an earlier attempt's landing is a landing.
                // Speaking is following: the reply lands at the tail.
                composeText = ""
                tailWatch.arrive()
                // Good news may retire itself.
                composeResultClearTask = Task {
                    try? await Task.sleep(for: .seconds(4))
                    guard !Task.isCancelled else { return }
                    withAnimation(CC.motion.medium) { composeResult = nil }
                }
            case .refused, .failed, .indeterminate, .composerLost:
                // The text stays — and so does the note. Four seconds is too
                // short for actionable failure information; it clears when
                // the text changes or another attempt begins. For
                // `.indeterminate`, the model kept the identity, so pressing
                // send again is a recognisable retry, not a second typing.
                break
            }
        }
    }
}

/// The "Latest / N new" capsule — the only view that reads `TailWatch`.
///
/// A separate struct on purpose: Observation scopes invalidation to the body
/// that did the reading, so a `following` flip repaints this capsule alone.
/// Read from `SessionDetailView.body` instead, the same flip re-evaluates the
/// whole screen — the measured limit cycle documented on `TailWatch`.
///
/// **Hidden by opacity, never by structure.** As an `if` in the overlay, the
/// pill's arrival is a structural change the scroll view re-lays; as a
/// permanent node its visibility is paint-only. A transparent button must be
/// as absent to fingers and to VoiceOver as it is to the eye — hence the hit-
/// testing and accessibility gates beside the opacity.
private struct TailPill: View {
    let watch: TailWatch
    let jump: () -> Void

    var body: some View {
        Button(action: jump) {
            HStack(spacing: CC.space.xxs + 1) {
                CCIcon("arrow.down", size: 12, weight: .semibold, relativeTo: .caption)
                Text(watch.newSinceLeaving > 0 ? "\(watch.newSinceLeaving) new" : "Latest")
                    .ccType(CC.type.footnote)
            }
            .foregroundStyle(CC.text.primary)
            .padding(.horizontal, CC.space.sm)
            .frame(minHeight: CC.size.controlSm)
            .background(CC.color.surfaceOverlay, in: Capsule())
            .overlay { Capsule().strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline) }
            .ccHitTarget(minWidth: 0)
        }
        .buttonStyle(.plain)
        .padding(CC.space.md)
        // A cut, not a fade, deliberately: an in-flight opacity animation
        // over this screen re-enters layout per frame, and with the timeline
        // deep enough each frame overruns its budget — the pill's own fade
        // was fuel for the layout loop documented at `timeline`.
        .opacity(watch.following ? 0 : 1)
        .allowsHitTesting(!watch.following)
        .accessibilityHidden(watch.following)
        .accessibilityLabel("Jump to the latest event")
    }
}

// MARK: - The four views that are allowed to read the clock

/// **The age in the identity block, clocked by itself.**
///
/// It is one string — `21h` — and reading it from `SessionDetailView.body` put a
/// one-second heartbeat under the entire timeline. It sleeps until its own
/// string stops being true: a second while the run is seconds old, a minute
/// while it is minutes old, an hour after that.
private struct SessionFreshness: View {
    let lastEventAt: Date?

    /// Read, not merely written — see `ApprovalRow.lastTick`. A `@State` the
    /// body never looks at does not invalidate the view, and an age that never
    /// invalidates is an age that has quietly stopped being true.
    @State private var lastTick = Date()

    /// The wall clock, not the stored tick: a view waking from an hour's sleep
    /// must measure against the real clock, never against the stamp it fell
    /// asleep holding. Both rules are pinned by test in `AgeTick.renderTime`.
    private var now: Date { AgeTick.renderTime(lastTick: lastTick) }

    /// Nil when this run has no events at all. There is no age to keep true, so
    /// there is nothing to wake for.
    private var clock: AgeClock? {
        lastEventAt.map { AgeClock(since: $0, scale: .age) }
    }

    var body: some View {
        Text(lastEventAt.map { Format.age(since: $0, now: now) } ?? "no events")
            .ccType(CC.type.monoSmall)
            .foregroundStyle(CC.text.tertiary)
            .task(id: clock) {
                guard let clock else { return }
                await AgeTick.follow(clock) { lastTick = $0 }
            }
    }
}

/// **The one thing in this toolbar that has to change every second.**
///
/// The fleet's `LinkPill`, on the pushed screen, and it is here for the reason
/// stated there: how long ago the Mac last spoke is what licenses everything
/// else on the screen, `linkHealth` is derived from `now`, and read from
/// `SessionDetailView.body` that second was charged to a 400-event timeline to
/// redraw two characters. The read now lives with the only view whose output
/// depends on it.
private struct SessionLinkPill: View {
    @Environment(AppModel.self) private var model
    let action: () -> Void

    var body: some View {
        // Absent in the sample fleet, for the reason `LinkPill` states.
        if !model.sampleFleetActive {
            CCFreshnessPill(health: model.linkHealth, action: action)
        }
    }
}

/// **The banner, clocked by itself.**
///
/// One banner, chosen by the ladder. The sequence gap and the truncated head are
/// **not** candidates: both have a position in time and are drawn there as
/// `CCGapMarker`s instead.
///
/// It reads `linkHealth`, which is derived from `now`, and it has to: a link
/// that goes stale must say so without being touched. That is a hairline and two
/// lines of text ticking, rather than the whole timeline behind it.
private struct SessionBanner: View {
    @Environment(AppModel.self) private var model
    let state: SessionState
    let onSettings: () -> Void

    var body: some View {
        // The sample fleet says so on every screen it reaches, not only on the
        // one it was entered from: a reader who pushed into a session is the
        // reader furthest from the sentence that explains it.
        let candidates: [CCBannerItem?] =
            model.sampleFleetActive
            ? [.sampleFleet(onLeave: { model.stopSampleFleet() })]
            : [
                model.linkHealth.ccBannerItem(
                    onRetry: { model.connection.retryNow() },
                    onSettings: onSettings,
                    onTailscale: { TailscaleAssist.open() }),
                cachedBanner,
            ]
        if candidates.contains(where: { $0 != nil }) {
            CCBannerSlot(candidates)
                .padding(.top, CC.space.sm)
                .padding(.bottom, CC.space.xs)
        }
    }

    private var cachedBanner: CCBannerItem? {
        guard let cachedAt = state.loadedFromCacheAt, !state.hasLiveData else { return nil }
        // The same earned rule as the fleet's cached banner, for the same flash:
        // a cold deep link paints this screen from the cache and the live replay
        // lands half a second later. Amber in that half-second is noise.
        guard
            FleetFreshness.cachedBannerEarned(
                restoredAt: state.cacheRestoredAt,
                connectingSince: model.connection.connectingSince,
                now: model.now)
        else { return nil }
        return CCBannerItem(
            .cached,
            title: "From the cache, \(Format.age(since: cachedAt, now: model.now)) old",
            message: "Nothing live has arrived for this session yet.",
            tone: .warning,
            icon: "clock.arrow.circlepath")
    }
}

/// **The compose bar, clocked by itself.**
///
/// `surfaceRaised` and a 1pt top rule — never `.bar`, whose material over `#000`
/// resolves to a flat mid-grey smear belonging to no palette.
///
/// It owns the `linkHealth` read for the same reason the banner does, and this
/// one is a safety property rather than a cosmetic one: **a link that has gone
/// stale must disable `Send` and say why, on its own, with nobody touching the
/// screen.** So this view genuinely ticks. What changed is that it ticks alone —
/// the timeline above it used to be rebuilt to keep this sentence true.
///
/// The text and the send result stay bound to `SessionDetailView`, so a half
/// typed message still survives a trip to the Terminal surface and back.
private struct SessionComposeBar: View {
    @Binding var text: String
    let result: ComposeAttempt?
    let sending: Bool
    /// What to call the run out loud — see `RunLabel.spoken`.
    let runLabel: String
    let summary: SessionSummary?
    let onWhy: () -> Void
    let onSend: () -> Void

    @Environment(AppModel.self) private var model
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(\.openURL) private var openURL

    @State private var dictation = DictationController()
    /// The screen's, not the bar's: `SessionDetailView` collapses its chrome
    /// around the keyboard, and focus mirrored through a second flag is focus
    /// that drifts. One owner, one binding, passed in.
    let focused: FocusState<Bool>.Binding
    /// The 0.9s the checkmark holds the circle's face after a confirmed send.
    @State private var sentFlash = false
    /// When Stop handed text back. The send face ignores taps for 300ms after,
    /// so the finger that just ended a recording cannot also fire the send.
    @State private var stoppedAt: Date?

    var body: some View {
        VStack(spacing: 0) {
            CCHairline()
            VStack(alignment: .leading, spacing: CC.space.sm) {
                // The reason goes *above* the composer and is visible text, not
                // an accessibility hint — that rule made concrete in the app's
                // highest-traffic control.
                standingNote
                composer
            }
            .padding(.horizontal, CC.space.md)
            .padding(.top, CC.space.sm)
            .padding(.bottom, CC.space.sm)
        }
        .background(CC.color.surfaceRaised)
        .onChange(of: result) { _, newValue in
            guard case .sent = newValue else { return }
            sentFlash = true
            Task {
                try? await Task.sleep(for: .seconds(0.9))
                withAnimation(CC.motion.micro) { sentFlash = false }
            }
        }
        // Typing is the user's next act as much as retrying the mic is; a
        // stale mic-failure note must not stand over a hand-typed message.
        .onChange(of: text) { _, _ in dictation.clearFailure() }
        // A hot mic must not outlive the composer that started it.
        .onDisappear { dictation.cancel() }
    }

    // MARK: The one card

    /// Chips, field and button stopped being three stacked strips: one
    /// `surface` card, input on top, controls along the bottom edge. The focus
    /// ring wraps the whole instrument — and so does recording, because while
    /// the mic is hot the card *is* the transcript.
    ///
    /// One 44pt circle does the whole job — mic when the field is empty, send
    /// once a single character exists — so the row never grows a second button
    /// for a thumb to arbitrate at 2am. At AX sizes nothing here stacks: the
    /// circle is fixed, the chips wrap inside their strip, and the field
    /// already owns the full measure.
    private var composer: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            if dictation.isRecording {
                transcript
            } else {
                field
            }
            HStack(alignment: .center, spacing: CC.space.xs) {
                // While the mic is hot the row carries no meters or clocks:
                // the live transcript above is proof the mic hears you, and
                // the button's stop face is the recording state. Chrome that
                // restates both was removed at the owner's call.
                // **Withdrawn when the composer cannot send.**
                //
                // The mic already dims in that state; the chips did not, so on a
                // too-old Mac three of them read at full contrast beside "this
                // Mac's CodeConnect is too old to carry a message" — a
                // live-looking affordance on a dead composer. They are withdrawn
                // rather than dimmed because the kit's own rule is that a list
                // never holds an item whose only behaviour is refusing the tap,
                // and a chip that inserts text into a field with no destination
                // is exactly that. The field itself stays draftable.
                if !dictation.isRecording, sendBlockedReason == nil {
                    templateChips
                }
                Spacer(minLength: 0)
                byteCounter
                if dictation.isRecording {
                    cancelButton
                }
                CCVoiceButton(phase: phase, blockedReason: sendBlockedReason) { act() }
            }
        }
        .padding(CC.space.sm)
        .ccSurface(
            fill: CC.color.surface, radius: CC.radius.xl,
            border: (focused.wrappedValue || dictation.isRecording)
                ? CC.color.borderFocus : CC.color.border)
        .ccAnimation(CC.motion.micro, value: focused.wrappedValue)
        .ccAnimation(CC.motion.small, value: dictation.isRecording)
    }

    private var field: some View {
        TextField(
            "", text: $text,
            prompt: Text(placeholder).foregroundStyle(CC.text.tertiary),
            // `.vertical` keeps the system keyboard's dictation key available
            // too — the mic in the circle is the first-class door, not the
            // only one.
            axis: .vertical
        )
        .lineLimit(typeSize.isAccessibilitySize ? 1...3 : 1...5)
        .ccType(CC.type.body)
        .foregroundStyle(CC.text.primary)
        .focused(focused)
        .accessibilityLabel("Message for \(runLabel)")
    }

    /// What the recognizer heard, streaming. Committed words at full strength;
    /// the current hypothesis in `textTertiary`, solidifying as the engine
    /// commits — uncertainty rendered as state, never hidden.
    @ViewBuilder
    private var transcript: some View {
        if dictation.transcript.isEmpty {
            Text("Listening…")
                .ccType(CC.type.body)
                .foregroundStyle(CC.text.tertiary)
                .frame(maxWidth: .infinity, alignment: .topLeading)
                .accessibilityLabel("Listening")
        } else {
            (Text(dictation.finalizedText)
                + Text(
                    dictation.finalizedText.isEmpty || dictation.volatileText.isEmpty
                        ? "" : " ")
                + Text(dictation.volatileText).foregroundStyle(CC.text.tertiary))
                .ccType(CC.type.body)
                .foregroundStyle(CC.text.primary)
                .frame(maxWidth: .infinity, alignment: .topLeading)
                .accessibilityLabel("Transcript: \(dictation.transcript)")
        }
    }

    // MARK: The circle's state

    private var phase: CCVoiceButtonPhase {
        // Starting reads as stop on purpose: during a first-use model
        // download the tap must have a visible consequence, and the honest
        // one is "this cancels what you started".
        if dictation.isRecording || dictation.isStarting { return .stop }
        if sending { return .sending }
        if sentFlash { return .sent }
        return text.isEmpty ? .dictate : .send
    }

    private func act() {
        switch phase {
        case .dictate:
            // `.light`, the mode-change weight — starting a recording is not a
            // keystroke and not a decision.
            CCHaptic.light.fire()
            Task { await dictation.start() }
        case .stop:
            CCHaptic.light.fire()
            let heard = dictation.stop()
            if !heard.isEmpty {
                // Staged, never sent: the transcript lands in the editable
                // field and takes the same deliberate tap as typed text.
                text = heard
                focused.wrappedValue = true
            }
            stoppedAt = .now
        case .send:
            if let stoppedAt, Date.now.timeIntervalSince(stoppedAt) < 0.3 { return }
            guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
                return
            }
            // Deliberately silent: Send fires dozens of times a day, and the
            // haptic that matters is the daemon's own confirm.
            onSend()
        case .sending, .sent:
            break
        }
    }

    /// Discards the recording *and* the transcript. Ghost weight — the circle
    /// beside it keeps the decision.
    private var cancelButton: some View {
        Button {
            // Silent, like every dismissal: buzzing on a discard trains the
            // user to ignore the haptic that matters.
            dictation.cancel()
        } label: {
            CCIcon("xmark", size: CC.size.iconSm, weight: .semibold, relativeTo: .body)
                .foregroundStyle(CC.text.secondary)
                .frame(width: CC.size.controlSm, height: CC.size.controlSm)
                .background {
                    Circle().strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline)
                }
        }
        .ccHitTarget()
        .accessibilityLabel("Cancel dictation")
        .accessibilityHint("Discards the transcript")
    }

    // MARK: The line above

    @ViewBuilder
    private var standingNote: some View {
        // A Codex outcome outranks the Claude ladder below, because on a Codex
        // session the ladder's vocabulary — "typed into the session", "the
        // composer did not come back" — describes machinery that is not there.
        if summary?.isCodex == true, case .settled(let result) = codexControls?.stop {
            // **One outcome slot, both mutations.** The stop's result used to
            // live under the header's Stop button; at AX5 that made the header
            // tall enough to push the pending card's own controls off screen.
            // It belongs here, where the compose result already goes and where
            // the reader is looking after they act.
            codexStopNote(result)
        } else if summary?.isCodex == true, case .notSent(let reason) = codexControls?.stop {
            ComposeNote(text: reason, tone: .warning, glyph: "exclamationmark.circle.fill")
        } else if summary?.isCodex == true, case .sentNoAnswer(let reason) = codexControls?.stop {
            ComposeNote(text: reason, tone: .warning, glyph: "questionmark.circle.fill")
        } else if summary?.isCodex == true, case .settled(let result) = codexControls?.compose {
            codexComposeNote(result)
        } else if summary?.isCodex == true, case .notSent(let reason) = codexControls?.compose {
            ComposeNote(
                text: reason, tone: .warning, glyph: "exclamationmark.circle.fill")
        } else if summary?.isCodex == true,
            case .sentNoAnswer(let reason) = codexControls?.compose
        {
            ComposeNote(text: reason, tone: .warning, glyph: "questionmark.circle.fill")
        } else if let result {
            feedbackLine(result)
        } else if case .failed(let reason, let needsSettings) = dictation.phase {
            micFailureNote(reason: reason, needsSettings: needsSettings)
        } else if let reason = sendBlockedReason {
            ComposeNote(
                text: reason, tone: .warning, glyph: "exclamationmark.circle.fill")
        } else if isObserveOnly {
            ComposeNote(
                text: "Observe only - answers are given at the Mac.",
                tone: .neutral, glyph: nil, action: ("Why?", onWhy))
        }
    }

    private var codexControls: CodexControls? {
        guard let key = summary?.sessionKey else { return nil }
        return model.codexControls(for: key)
    }

    /// What became of a Codex message, in Codex's own five arms.
    ///
    /// **`started` and `steered` are two different sentences**, and a duplicate
    /// reads with the verb the original earned — see `CodexProse`, where the
    /// rule lives and is tested.
    private func codexComposeNote(_ result: ComposeResult) -> some View {
        let banner = CodexProse.compose(result)
        return ComposeNote(
            text: banner.oneLine,
            tone: banner.tone.ccTone,
            glyph: banner.icon)
    }

    /// What became of a stop, in the daemon's own four arms — refusals verbatim.
    private func codexStopNote(_ result: InterruptResult) -> some View {
        let banner = CodexProse.interrupt(result)
        return ComposeNote(
            text: banner.oneLine,
            tone: banner.tone.ccTone,
            glyph: banner.icon)
    }

    /// **The byte counter**, appearing only as the 8192-byte ceiling comes into
    /// view. Bytes, not characters: the Mac measures `text.len()`, and a
    /// message of accented characters is twice the length it looks.
    @ViewBuilder
    private var byteCounter: some View {
        let draft = ComposeDraft(text: text)
        if summary?.isCodex == true, let counter = draft.counterText {
            Text(counter)
                .ccType(CC.type.monoSmall)
                .foregroundStyle(draft.isOverCeiling ? CC.color.danger : CC.text.tertiary)
                .lineLimit(1)
                .accessibilityLabel(
                    draft.isOverCeiling
                        ? "Too long. \(counter)." : "\(counter).")
        }
    }

    /// Built outside the `@ViewBuilder` so the optional action tuple is typed
    /// here, once — inline, the builder's type inference chokes on it.
    private func micFailureNote(reason: String, needsSettings: Bool) -> ComposeNote {
        guard needsSettings else {
            return ComposeNote(
                text: reason, tone: .warning, glyph: "exclamationmark.circle.fill")
        }
        return ComposeNote(
            text: reason, tone: .warning, glyph: "exclamationmark.circle.fill",
            action: (title: "Settings", perform: { openSettings() }))
    }

    private func openSettings() {
        guard let url = URL(string: UIApplication.openSettingsURLString) else { return }
        openURL(url)
    }

    /// Tapping a chip **inserts** the template. It never sends: a one-tap send
    /// is a decision taken without reading, which is the failure this whole
    /// product is arranged around.
    private var templateChips: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: CC.space.xs) {
                ForEach(Self.templates, id: \.self) { template in
                    CCBadge(template) {
                        insert(template)
                    }
                }
            }
            .padding(.trailing, CC.space.xl)
        }
        .scrollBounceBehavior(.basedOnSize, axes: .horizontal)
        // A right-edge fade says there are more chips rather than guillotining
        // one mid-word.
        .mask {
            LinearGradient(
                stops: [
                    .init(color: .black, location: 0),
                    .init(color: .black, location: 0.86),
                    .init(color: .black.opacity(0), location: 1),
                ],
                startPoint: .leading, endPoint: .trailing)
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Message templates")
    }

    /// The full sentence at reading sizes; two words once the type is large
    /// enough that the sentence would own the screen. The field is already
    /// labelled `MESSAGE`, so the short form loses nothing.
    private var placeholder: String {
        // **Named, where the app knows the name.** "Say something to this agent"
        // is the honest phrasing when the app cannot say which agent — and on a
        // Codex session it can, so it does. The two paths behind this field are
        // genuinely different messages (`send_text` into a TTY, `compose` over a
        // control link), and a reader who knows which one they are talking to is
        // a reader who can read the outcome sentence that comes back.
        let named = summary?.isCodex == true ? "Ask Codex to do anything" : "Say something to this agent"
        return typeSize.isAccessibilitySize ? "Say something" : named
    }

    /// What a developer most often tells an agent from a phone, most-used
    /// first. **No "Stop" chip** — see `ComposerTemplates`, where the rationale
    /// now says which agent it is about. These insert, never send; the contract
    /// lives at `insert(_:)`.
    private static let templates = ComposerTemplates.all

    private func insert(_ template: String) {
        text =
            text.isEmpty
            ? template
            : text.trimmingCharacters(in: .whitespacesAndNewlines) + " " + template
    }

    @ViewBuilder
    private func feedbackLine(_ result: ComposeAttempt) -> some View {
        switch result {
        case .sent:
            ComposeNote(
                text: "Typed into the session.",
                tone: .success, glyph: "checkmark")
        case .refused(let reason):
            ComposeNote(
                text: "Not typed: \(reason)", tone: .warning,
                glyph: "exclamationmark.triangle.fill")
        case .failed(let reason):
            ComposeNote(
                text: "Couldn’t type the message: \(reason)", tone: .danger,
                glyph: "xmark.octagon.fill")
        case .alreadyApplied:
            ComposeNote(
                text: "Already typed earlier — not repeated.",
                tone: .success, glyph: "checkmark")
        case .indeterminate(let reason):
            ComposeNote(
                text: "Not confirmed: \(reason) "
                    + "Retry is safe; the message won’t be typed twice.",
                tone: .warning, glyph: "questionmark.circle.fill")
        case .composerRecovered(let command, _, _):
            // **Not a success.** The same daemon result on the Model and Effort
            // sheets reports that no change was confirmed, and a typed command
            // reaches this note by the identical path — `/effort high` routes
            // through here, and on a cache-warm conversation it opens a
            // confirmation that the daemon's one Esc then cancels. A green tick
            // over that is the claim this app exists not to make.
            ComposeNote(
                text: "After \(command) was typed, the Mac composer disappeared. "
                    + "CodeConnect pressed Esc and confirmed it returned. "
                    + "The command's outcome was not confirmed.",
                tone: .warning, glyph: "questionmark.circle.fill")
        case .composerLost(let command):
            ComposeNote(
                text: "\(command) was typed, but the Mac's composer did not come back. "
                    + "Open Terminal to recover.",
                tone: .danger, glyph: "exclamationmark.triangle.fill")
        }
    }

    private var isObserveOnly: Bool {
        guard let summary else { return false }
        return !FleetStatusRule.capability(
            summary: summary, capabilities: model.connection.capabilities
        ).canAct
    }

    /// Text only lands if the composer is actually on screen at the Mac, so the
    /// reasons a send cannot work are the same reasons an answer cannot.
    ///
    /// **Except for Codex**, where none of that applies: there is no composer at
    /// the Mac, no keystrokes and no `send_text`. Its reasons are its own, and
    /// they are asked first so a Codex session is never refused with a sentence
    /// about a TTY it does not have.
    private var sendBlockedReason: String? {
        // The sample fleet's reason, not the link's: "Not paired with a daemon."
        // beside a card that answers in sample vocabulary would be two different
        // accounts of the same absence on one screen.
        if model.sampleFleetActive {
            return "These agents are not real. Pair with your Mac to talk to your own."
        }
        if let reason = model.linkHealth.disabledReason { return reason }
        if let summary, summary.isCodex {
            // The draft's own ceiling comes first: it is the one the reader can
            // fix without anything changing at the Mac, and it names the number
            // they have to cut. Only the *oversize* refusal is shown here — an
            // empty draft is not a problem to announce, it is a message nobody
            // has written yet.
            let draft = ComposeDraft(text: text)
            if draft.isOverCeiling { return draft.blockedReason }
            return model.composeUnavailable(for: summary.sessionKey)
        }
        if model.connection.capabilities?.sendText == false {
            return "This daemon does not accept typed text."
        }
        if let summary, let reason = FleetStatusRule.capability(
            summary: summary, capabilities: model.connection.capabilities).reason
        {
            return reason
        }
        return nil
    }
}

// MARK: - Compose note

/// The one-line statement above the compose field: a blocked reason, a send
/// result, or the observe-only standing notice.
///
/// Three call sites in one file, all the same shape, all driven by `CCTone` —
/// the kit's own semantic axis — so a refusal and a failure can never end up as
/// two different oranges.
private struct ComposeNote: View {
    let text: String
    var tone: CCTone = .warning
    var glyph: String?
    var action: (title: String, perform: () -> Void)?

    var body: some View {
        CCAdaptiveStack(horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xxs) {
            HStack(alignment: .firstTextBaseline, spacing: CC.space.xxs + 1) {
                if let glyph {
                    CCIcon(glyph, size: 11, weight: .semibold, relativeTo: .caption)
                        .foregroundStyle(tone == .neutral ? CC.text.tertiary : tone.color)
                }
                Text(text)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(tone == .neutral ? CC.text.tertiary : tone.color)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let action {
                CCButton(action.title, variant: .ghost, size: .sm, action: action.perform)
            }
            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .transition(.opacity)
        .accessibilityElement(children: .combine)
    }
}

