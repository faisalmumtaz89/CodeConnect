import Combine
import SwiftTerm
import SwiftUI
import UIKit

// SwiftTerm exports its own `Color` (an RGB terminal colour), so SwiftUI's has
// to be named explicitly anywhere both modules are in scope. Aliasing once beats
// qualifying at twenty call sites and beats importing SwiftTerm submodules.
private typealias UIColour = SwiftUI.Color

/// The live terminal: the agent's own tmux pane, streamed over the connection
/// this phone is *already* paired on.
///
/// This is the half of the product that is Termius. It is deliberately the
/// *second* surface, not the first — everything the app knows about state comes
/// from the daemon's event log, and this is where you go when you want to be the
/// Mac's keyboard instead.
///
/// The terminal rides the same paired WebSocket as the timeline. There is no
/// second transport and nothing to switch on at the Mac: if the timeline is
/// live, the terminal can open, and no screen has to explain a second
/// connection failing while the first one works.
///
/// The reason for the strip at the top of every state: **the terminal is the
/// degraded layer and it must say which layer it is on at all times.** A
/// terminal showing the last bytes it received looks exactly like a live one. A
/// banner can be scrolled past; a 28pt strip that never moves cannot.
struct TerminalTabView: View {
    /// The run to attach to, by uid. The daemon resolves it to the one live
    /// session carrying that stamp, so a reused tmux name can never hand this
    /// tab a different agent's keyboard.
    let sessionUID: String
    /// The tmux name, for display only — nil when the daemon no longer lists
    /// this run, or never hosted it, which is also when there is nothing to
    /// attach to.
    let tmuxName: String?
    /// True when the run is listed but has no tmux location: adopted, observed
    /// through its hooks, launched by something other than CodeConnect. The
    /// blocked reason has to tell that story rather than claim the run is gone.
    let unhosted: Bool
    /// What to call this run on screen.
    /// What to call the run out loud — see `RunLabel.spoken`. The tmux name is
    /// still what this view *attaches* to; it is simply not what a reader is
    /// told they are looking at.
    let runLabel: String
    /// Whether this tab is the surface on show.
    ///
    /// It stays mounted while it is not, because its emulator holds scrollback
    /// nothing else has a copy of — so "not on screen" has to be said rather
    /// than inferred from being torn down. A pane behind another surface must
    /// not hold the keyboard, and coming back to one is the moment to try the
    /// connection again.
    var onScreen = true

    @Environment(AppModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.dynamicTypeSize) private var typeSize

    /// Owned by the model, not by this view: the terminal rides the paired
    /// connection, and a view-owned carrier would drop the session every time
    /// SwiftUI rebuilt the tab.
    private var session: TerminalCarrier { model.terminal }
    /// When the current attach started, for the elapsed counter every wait owes
    /// the reader.
    @State private var startedAt: Date?
    @State private var showPairing = false
    /// Whether *this tab's emulator* was handed a buffer that had already lost
    /// its head to the cap.
    ///
    /// **A fact about one emulator, which is why it is view state.** The
    /// carrier's `transcriptIsTruncated` says the buffer is short of the
    /// session; it says nothing about what any pane is showing. A pane that
    /// survived a rebuild still holds every byte in its own scrollback while
    /// that flag is true, and telling its reader something is missing is the
    /// lie in the other direction. Only the moment a *fresh* emulator is
    /// seeded turns the buffer's shortfall into a claim about a screen, so that
    /// is the only moment this is written — see `SwiftTermView.seeded`.
    ///
    /// **Written from exactly one place, which is what makes it clear itself.**
    /// Every route that empties the transcript is an attach, every attach is
    /// rendered as the connecting card, and the card is not the pane — so the
    /// emulator is built again on the other side of it and seeded from what the
    /// daemon has since repainted, which is whole. The same one line reports
    /// that, and the notice goes. A second writer that watched the carrier for
    /// the clearing would be a second answer to the same question, and this
    /// defect began as an indication whose truth depended on something other
    /// than the site that knows.
    ///
    /// It follows that a pane which one day *survives* an attach — the emulator
    /// is the only copy of its own scrollback, and keeping it across a reattach
    /// would be a real improvement — has to report its re-seeding here too, or
    /// this goes stale over a screen the Mac has just painted whole.
    @State private var seededFromTrimmedBuffer = false

    var body: some View {
        // One measurement, so the strip can be told what 45% of the viewport
        // is. A bar that grows without a ceiling is how an accessibility size
        // ends up showing chrome and no content.
        GeometryReader { proxy in
            VStack(spacing: 0) {
                livenessStrip(maxHeight: max(CC.size.hitTarget, proxy.size.height * 0.45))
                content
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        }
        .background(CC.color.bg)
        .onAppear(perform: connectIfPossible)
        .onChange(of: onScreen) { _, showing in
            // Returning to this tab is a fresh chance to reattach, the same as
            // arriving at it. The view outlives the visit now, so `onAppear`
            // alone would only ever fire for the first one.
            guard showing else { return }
            connectIfPossible()
        }
        .onChange(of: scenePhase) { _, phase in
            // Reconnect on foreground, for a terminal that ended while the link
            // itself held: a frame that failed to write, a session detached
            // from the Mac. It is deliberately *not* the recovery for a dropped
            // socket — waking is not being connected, and at the moment the app
            // is foregrounded the link is still being dialled, so an attach
            // then is refused. That case is the closure below.
            guard phase == .active else { return }
            connectIfPossible()
        }
        .onChange(of: model.connection.phase) { _, link in
            // The reattach after a dropped socket, at the only moment it can
            // work. iOS tears sockets down in the background and the drop ends
            // the terminal as it happens — before the app is ever foregrounded
            // — so the terminal becomes reopenable when the handshake lands,
            // which is here and nowhere earlier.
            //
            // Still only ever back into a terminal that was already open:
            // `connectIfPossible` refuses an idle carrier, so a handshake never
            // opens a shell nobody asked for.
            guard link.isConnected else { return }
            connectIfPossible()
        }
        // No observer of the byte count here, deliberately. The stamp this
        // screen renders is sampled at the carrier — see
        // `TerminalCarrier.lastOutputAt` — because a view body that read a
        // per-chunk counter would be re-run at chunk rate, up to a hundred
        // times a second on the main thread, competing with the emulator's own
        // drawing for the sake of a clock that shows whole seconds.
        .sheet(isPresented: $showPairing) {
            PairingView().environment(model)
        }
        // Leaving the tab is deliberately *not* leaving the terminal: the
        // carrier lives on the model, so switching to the timeline and back
        // does not drop you out of tmux.
    }

    /// The phase every part of this screen renders from — the strip included, so
    /// the design inspection seam produces a *faithful* screen rather than a real
    /// strip over a forced card.
    ///
    /// The seam forces only the phase, never an identifier: a review seam
    /// whose entire purpose is a truthful screen cannot invent the one thing
    /// the screen is about.
    ///
    /// **Only ever this run's.** One carrier serves the whole app, so a screen
    /// that rendered its phase unasked would draw another run's live pane under
    /// this run's label and type into it. A carrier that is not this run's reads
    /// as `idle` here — nothing of it is drawn — and `connectState` below says
    /// whether opening this run's terminal is free or means ending theirs.
    /// The seam cannot outrank a terminal that is genuinely open somewhere
    /// else: a forced `endedLive` would build a real emulator, take delivery of
    /// the output, and put this screen's keystrokes into that run's pane. A
    /// review seam is allowed to invent a screen, never a keyboard.
    private var phase: TerminalCarrier.Phase {
        if case .heldByAnotherRun = standing { return .idle }
        return TerminalDesignState.override() ?? session.phase(forRun: sessionUID)
    }

    /// Whose terminal the shared carrier is holding, from this run's screen.
    private var standing: TerminalCarrier.Standing {
        session.standing(forRun: sessionUID)
    }

    /// What the tab draws under the strip.
    ///
    /// **The terminal's two forms are one case, and that is the whole point of
    /// this type.** Every arm of a `@ViewBuilder` switch is its own identity, so
    /// a live pane and a snapshot pane written as two arms are two different
    /// views: SwiftUI dismantles the emulator and builds a fresh one at the
    /// exact moment the session ends — which is the moment the emulator's
    /// scrollback becomes the only copy of what the reader is looking at. All a
    /// replacement can replay is the capped transcript, so a build that printed
    /// megabytes would come back as its last couple of hundred kilobytes. One
    /// case with a parameter keeps the pane.
    private enum Pane {
        case connect
        case connecting
        /// The emulator: live, or holding what it last received.
        case terminal(live: Bool)
        case closed(reason: String)
    }

    private var pane: Pane {
        switch phase {
        case .idle:
            return .connect
        case .attaching:
            return .connecting
        case .attached:
            return .terminal(live: true)
        case .ended(let reason, let wasAttached):
            // A terminal that was ever live keeps its pane. The bytes on it are
            // what the reader came back for, and a card in their place discards
            // the only copy of them.
            guard wasAttached || !session.transcript(forRun: sessionUID).isEmpty else {
                return .closed(reason: reason)
            }
            return .terminal(live: false)
        }
    }

    @ViewBuilder
    private var content: some View {
        switch pane {
        case .connect:
            connectCard
        case .connecting:
            connectingCard
        case .terminal(let live):
            terminal(live: live)
        case .closed(let reason):
            ended(reason)
        }
    }

    // MARK: - Liveness strip

    /// Mandatory, always present. The one element on this screen that every
    /// state shares, so "is this live?" is answered in the same place and the
    /// same words no matter what else is on screen.
    ///
    /// **On the app's two left edges.** The dot takes the 32–40 gutter column
    /// and every string in the strip begins at **52**, which is where the card
    /// underneath starts its own text — so the two read as one column rather
    /// than as a bar bolted above a screen.
    ///
    /// **And it follows the card when the card moves.** At accessibility sizes
    /// a card abandons the gutter — a 52pt inset leaves about thirty characters
    /// of measure on a 402pt screen — and drops every string to 32. The strip
    /// goes with it: the dot moves **above** the word rather than beside it,
    /// and the strip's strings start on 32 with the card's. A strip that held
    /// 52 through that would measure as a *separate text column* on the one
    /// screen in the product that can least afford one — four edges at AX5,
    /// this being one of them.
    ///
    /// **No separator in the stacked form.** On one line the word and its
    /// detail are joined by `· `; stacked, they are two lines and there is
    /// nothing for a separator to sit between. Rendering `· detail` as one
    /// wrapping `Text` beside a fixed-size word puts the `·` alone, centred, on
    /// a line of its own above `Not live`, with the host wrapped underneath in
    /// a third alignment.
    private func livenessStrip(maxHeight: CGFloat) -> some View {
        ScrollView(.vertical, showsIndicators: false) {
            // Centred on one line; stacked, and leading-aligned, once the
            // strip takes an accessibility size. `.center` rather than `.top`
            // in the single-line form: a `Reconnect` button on the same row
            // makes the row taller than the text, the text centres inside it,
            // and a top-aligned dot floats above the word it belongs to.
            CCAdaptiveStack(
                horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xs,
                horizontalAlignment: .leading, verticalAlignment: .center
            ) {
                CCStatusDot(
                    color: liveness.dotColor, isHollow: liveness.isHollow,
                    pulses: liveness.pulses
                )
                // The gutter column: 8pt of layout at 32–40, aligned to the
                // first line of the word beside it rather than to the strip.
                // Stacked, the dot owns its own height — pinning it to a
                // scaled line box would hang it in ~40pt of empty strip at
                // AX5, which is height this bar has a 45% ceiling on.
                .frame(
                    width: CC.size.dot,
                    height: typeSize.isAccessibilitySize ? nil : stripLine,
                    alignment: .center)

                stripText
            }
            .padding(.leading, CC.space.xxl)
            .padding(.trailing, CC.space.md)
            .padding(.vertical, CC.space.xxs)
            .frame(minHeight: CC.space.xl + CC.space.xxs)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background {
                GeometryReader { proxy in
                    Color.clear
                        .onChange(of: proxy.size.height, initial: true) { _, new in
                            stripHeight = new
                        }
                }
            }
        }
        .scrollBounceBehavior(.basedOnSize, axes: .vertical)
        // **Only scrollable when it has actually been capped.** Found by
        // rendering: an always-scrollable 145pt strip at AX5 swallowed every
        // vertical drag that began inside it, so the card underneath could
        // not be scrolled at all from the top
        // half of the screen.
        .scrollDisabled(stripHeight <= maxHeight)
        // Sizes to its content, then stops at 45% of the viewport and scrolls
        // inside itself. The order matters: the cap has to be inside the
        // `fixedSize` or the strip claims the whole screen.
        .frame(maxHeight: maxHeight)
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(CC.color.surface)
        .overlay(alignment: .bottom) { CCHairline() }
        .accessibilityElement(children: .contain)
    }

    /// The height of the strip's own first line, so the dot centres on the word
    /// and not on a three-line stack.
    @ScaledMetric(relativeTo: .caption) private var stripLine: CGFloat = 14
    /// The strip's measured content height, so it only takes over vertical
    /// dragging when it has genuinely been capped.
    @State private var stripHeight: CGFloat = 0

    @ViewBuilder
    private var stripText: some View {
        if typeSize.isAccessibilitySize {
            VStack(alignment: .leading, spacing: CC.space.xxs) {
                stripWord
                if let detail = liveness.detail { stripDetail(detail) }
                trailingStripItem
            }
        } else {
            HStack(spacing: CC.space.xs) {
                stripWord
                if let detail = liveness.detail {
                    Text("·")
                        .ccType(CC.type.monoSmall)
                        .foregroundStyle(CC.text.tertiary)
                        .fixedSize()
                        .accessibilityHidden(true)
                    stripDetail(detail)
                }
                Spacer(minLength: CC.space.xs)
                trailingStripItem
            }
        }
    }

    /// `badgeLabel`, not `micro`. `micro` has exactly one job — section headers
    /// — and `Live` / `Not live` is a *label on an object*: it classifies the
    /// strip it sits in, the way a badge classifies the row it sits on. It was
    /// already being written as `micro.weight(.semibold)`, which is what
    /// `badgeLabel` is; now it says so.
    private var stripWord: some View {
        Text(liveness.word)
            .ccType(CC.type.badgeLabel)
            .foregroundStyle(liveness.wordColor)
            .fixedSize()
            .accessibilityLabel(liveness.word)
    }

    private func stripDetail(_ detail: String) -> some View {
        Text(detail)
            .ccType(CC.type.monoSmall)
            .foregroundStyle(CC.text.tertiary)
            // Head truncation keeps the *host*, which is the half that answers
            // "am I typing at the right Mac". At accessibility sizes the strip
            // has already stacked and there is room to wrap, so nothing is cut
            // at all.
            .lineLimit(typeSize.isAccessibilitySize ? nil : 1)
            .truncationMode(.head)
            .fixedSize(horizontal: false, vertical: true)
    }

    @ViewBuilder
    private var trailingStripItem: some View {
        if case .attached = phase {
            if let sessionID = tmuxName {
                // tmux's exact-match name, whole. It used to render
                // `tmux -L codeconnect =cc-1` truncated from the head, which is
                // a *command* with its verb cut off; the full attach line lives
                // on the connect card, and what the strip owes the reader here
                // is which run the keyboard is wired to.
                Text("=\(sessionID)")
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityLabel("Attached to tmux session \(sessionID)")
            }
        } else if canOfferReconnect {
            // `ghost` again: the variant draws its own 1pt edge at rest now, so
            // a recovery control no longer has to be promoted to `secondary`
            // just to look like a control at all.
            CCButton("Reconnect", variant: .ghost, size: .sm) { connect() }
        }
    }

    /// Offered only when attaching again could actually work: the carrier is
    /// free, nothing structural blocks it, and the daemon's own close code says
    /// a retry is not futile. A `Reconnect` that the connection would refuse is
    /// the control this screen must never show.
    private var canOfferReconnect: Bool {
        Self.mayOfferReconnect(
            canAttach: session.canAttach,
            isBlocked: blockedReason != nil,
            servesThisRun: standing.isMine,
            lastClose: session.lastClose)
    }

    /// The one answer to "may this screen offer a retry", so that the two places
    /// that ask cannot give two.
    ///
    /// Both the liveness strip and the "Terminal closed" card show a `Reconnect`,
    /// and they used to decide separately: the strip consulted the close code and
    /// the card consulted only whether something structural was blocking. On a
    /// close a retry cannot change — `session_not_hosted`, `session_exited` — the
    /// strip therefore hid the control while the card, on the same screen, still
    /// offered it, and the one a reader can tap was the one that re-sent an
    /// attach into the same refusal.
    ///
    /// Static and pure so the rule can be read and tested on its own. What that
    /// does not do is prove a caller asks it; only two call sites do, and they
    /// are one line each.
    static func mayOfferReconnect(
        canAttach: Bool, isBlocked: Bool, servesThisRun: Bool,
        lastClose: TerminalCarrier.CloseCode?
    ) -> Bool {
        guard canAttach, !isBlocked else { return false }
        // The close code answers for the run that closed. Another run's says
        // nothing about this one, and letting it speak here hides this screen's
        // only control because a session in a different tab exited.
        guard servesThisRun else { return true }
        if let lastClose, !lastClose.isRetryable { return false }
        return true
    }

    private struct Liveness {
        var word: String
        var wordColor: UIColour
        var dotColor: UIColour
        var isHollow = false
        var pulses = false
        var detail: String?
    }

    private var liveness: Liveness {
        switch phase {
        case .attached:
            return Liveness(
                word: "Live", wordColor: CC.color.success, dotColor: CC.color.success,
                detail: userAtHost)
        case .attaching:
            return Liveness(
                word: "Connecting", wordColor: CC.color.info, dotColor: CC.color.info,
                pulses: true, detail: userAtHost)
        default:
            return Liveness(
                word: "Not live", wordColor: CC.text.tertiary, dotColor: CC.text.tertiary,
                isHollow: true, detail: lastOutputText ?? userAtHost)
        }
    }

    /// Which Mac this keyboard is wired to. The host alone: the terminal rides
    /// the paired connection, so there is no second account to name.
    private var userAtHost: String? { host }

    /// When this run's terminal last received bytes, as the strip says it.
    ///
    /// **Only ever this run's**, the same rule `phase(forRun:)` follows and for
    /// the same reason: one carrier serves the whole app, so an unscoped read
    /// would stamp another run's output onto this screen — a terminal that has
    /// never drawn a byte claiming it was live a moment ago, under this run's
    /// label.
    private var lastOutputText: String? {
        guard case .mine = standing, let at = session.lastOutputAt else { return nil }
        return "last output \(Self.clock.string(from: at))"
    }

    private static let clock: DateFormatter = {
        let formatter = DateFormatter()
        formatter.dateFormat = "HH:mm:ss"
        return formatter
    }()

    // MARK: - Connect

    /// A centred composition that becomes scrollable **exactly when it stops
    /// fitting**, and not before.
    ///
    /// **Every full-screen state on this tab uses it, including the ones that
    /// always fit at the default size.** At AX5 the connect card's fix button
    /// measures y=1169 on an 874pt screen — 295pt below the fold — and a
    /// container with no scroll view puts the one control that can unblock the
    /// terminal out of reach. A state that fits at `L` and not at AX5 is a
    /// defect nobody sees, so no state here is trusted to fit.
    ///
    /// `minHeight: proxy.size.height` is what keeps the centring: while the
    /// content is shorter than the viewport it is centred in it and the scroll
    /// view has nothing to do. `.basedOnSize` stops it bouncing a card that
    /// fits.
    private func centredState<Content: View>(
        @ViewBuilder content: @escaping () -> Content
    ) -> some View {
        GeometryReader { proxy in
            ScrollView {
                content()
                    .frame(maxWidth: .infinity)
                    .frame(minHeight: proxy.size.height)
            }
            .scrollBounceBehavior(.basedOnSize, axes: .vertical)
        }
    }

    /// A 56pt bordered circle with a 24pt glyph, not a 42pt symbol: a 42pt
    /// glyph is a decoration, and this card is a control.
    private var connectCard: some View {
        centredState {
            // `contentColumn: false`: this card's content is *centred*, and a
            // centred block wants four even edges. The 20pt step the kit adds on
            // the leading side to put text on the app's 52pt content column is,
            // on a centred composition, a 10pt error in the middle of it.
            CCCard(padding: CC.space.xl, contentColumn: false) {
                VStack(spacing: CC.space.sm) {
                    glyphBadge("terminal")

                    Text("Live terminal")
                        .ccType(CC.type.headline)
                        .foregroundStyle(CC.text.primary)
                        // Found by rendering: at AX5 this read `Live termin…`.
                        // A card title is never truncated — it wraps.
                        .multilineTextAlignment(.center)
                        .fixedSize(horizontal: false, vertical: true)

                    // Mono holds the *target* and nothing else. It used to hold
                    // whichever of four strings applied, two of which were
                    // sentences — "No Mac address is known — pair first." set
                    // in SF Mono is prose wearing a machine face, which reads as
                    // a string to copy rather than a sentence to read. The
                    // sentences are now the blocked reason's job, in prose,
                    // where they were already half living.
                    if let targetLine {
                        Text(targetLine)
                            .ccType(CC.type.monoSmall)
                            .foregroundStyle(CC.text.secondary)
                            .multilineTextAlignment(.center)
                            .fixedSize(horizontal: false, vertical: true)
                    }

                    if let blocked = blockedReason {
                        // A reason without a route to the fix is half an error
                        // message.
                        Text(blocked.reason)
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.color.warning)
                            .multilineTextAlignment(.center)
                            .fixedSize(horizontal: false, vertical: true)
                            .padding(.top, CC.space.xxs)
                        if let fix = blocked.fix {
                            CCButton(
                                fix.title, variant: .secondary, size: .md, fullWidth: true,
                                action: fix.run)
                        }
                    } else {
                        CCButton("Connect", variant: .primary, size: .lg, fullWidth: true) {
                            connect()
                        }
                        .padding(.top, CC.space.xs)
                    }
                }
                .frame(maxWidth: .infinity)
            }
            .frame(maxWidth: cardWidth)
            .padding(CC.space.md)
        }
        .accessibilityElement(children: .contain)
    }

    private var connectingCard: some View {
        centredState {
            // Centred content — see `connectCard`.
            CCCard(padding: CC.space.xl, contentColumn: false) {
                VStack(spacing: CC.space.sm) {
                    CCProgressRing(.md)

                    // Every phase string stays verbatim.
                    Text(busyText)
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                        .multilineTextAlignment(.center)
                        .fixedSize(horizontal: false, vertical: true)

                    // New and required: every fact carries its age, and a wait
                    // is a fact.
                    Text(Format.age(elapsed))
                        .ccType(CC.type.monoSmall)
                        .foregroundStyle(CC.text.tertiary)

                    // `ghost`, which draws its own 1pt edge at rest. It is the
                    // only control on the card and the only way to stop a
                    // connection that has hung, so it has to read as one —
                    // which, borderless and centred under a spinner, it did not.
                    CCButton("Cancel", variant: .ghost, size: .sm) {
                        session.detach(reason: "Cancelled.")
                    }
                    .padding(.top, CC.space.xxs)
                }
                .frame(maxWidth: .infinity)
            }
            .frame(maxWidth: cardWidth)
            .padding(CC.space.md)
        }
        .accessibilityElement(children: .contain)
    }

    private var busyText: String {
        switch phase {
        case .attaching: return "Opening a terminal on \(host ?? "the Mac")…"
        default: return "Working…"
        }
    }

    private var elapsed: TimeInterval {
        guard let startedAt else { return 0 }
        return max(0, model.now.timeIntervalSince(startedAt))
    }

    /// 320 is the readable measure for a centred card — and a straitjacket at
    /// accessibility sizes, where 320pt holds about three words per line and the
    /// card's own title starts wrapping to four. Past AX1 the card takes the
    /// screen and the 16pt page padding is the only limit.
    private var cardWidth: CGFloat {
        typeSize.isAccessibilitySize ? .infinity : 320
    }

    private func glyphBadge(_ symbol: String) -> some View {
        // A 24pt glyph, deliberately smaller than `CCEmptyState`'s 32 — a 42pt
        // glyph is a decoration and this card is a control — inside the *same*
        // circle, on the **same ramp**.
        //
        // The ramp is the fix. Both circles are `CC.size.emptyGlyphCircle`, so
        // they measured 63.33pt apiece at the default size and looked settled;
        // `.title2` grows without a ceiling at accessibility sizes and
        // `.largeTitle` does not, so at AX5 this one reached **150.33pt on a
        // 402pt screen — 37% of the width for a container around a glyph** —
        // against the empty state's 108.67. 41.66pt apart, on one tab, from
        // one word. `relativeTo:` has to match the component whose mark this
        // is reusing, and it has to match on the icon and its container both,
        // or the glyph and the ring scale on two ramps again.
        CCIcon(symbol, size: CC.space.xl, weight: .regular, relativeTo: .largeTitle)
            .foregroundStyle(CC.text.tertiary)
            .ccGlyphContainer(CC.size.emptyGlyphCircle, relativeTo: .largeTitle)
    }

    /// The third of this tab's three reconnect affordances, and it consults the
    /// block like the other two. The strip suppresses its own `Reconnect` when
    /// a connection cannot be made (`canOfferReconnect`); the connect card
    /// replaces `Connect` with the reason plus a route to the fix.
    ///
    /// This builds what the connect card builds, from the same value: the
    /// reason above the control it explains, and the route in place of an
    /// action that would be refused. Where there is no route — the daemon has
    /// stopped listing the run — there is no button either, and the reason is
    /// the whole answer. **There are no dead buttons without an explanation**,
    /// and on a screen whose only control is this one, a `Reconnect` that
    /// `connect()` would return from at an unmentioned guard is exactly that.
    private func ended(_ reason: String) -> some View {
        let blocked = blockedReason
        // Resolved once: nothing blocking makes reconnecting itself the route;
        // a block hands over its own route, which is `nil` when there honestly
        // is not one. The title and the action come from the same value, so a
        // label can no longer outlive the action under it.
        // `canOfferReconnect` and not `blocked == nil`: nothing blocking is only
        // half the question, and the half this card used to ask alone. The other
        // half is whether the daemon's close code leaves a retry any room — a
        // session that is not hosted, or has ended, answers the same way however
        // many times it is asked. The strip already refused to show the control
        // on that answer, so a card that showed it put two different answers to
        // one question on one screen, and the tappable one was the wrong one.
        let route: Blocked.Fix? =
            blocked == nil
            ? (canOfferReconnect ? Blocked.Fix(title: "Reconnect", run: { connect() }) : nil)
            : blocked?.fix

        return centredState {
            CCEmptyState(
                glyph: "terminal",
                title: "Terminal closed",
                message: reason,
                actionTitle: route?.title,
                action: route?.run
            ) {
                if let blocked {
                    // Same construction as the connect card's: `footnote` in
                    // `warning`, capped at the readable measure, and *above* the
                    // control it explains.
                    Text(blocked.reason)
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.color.warning)
                        .multilineTextAlignment(.center)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: 320)
                }
            }
        }
    }

    // MARK: - Terminal

    /// One pane, in two states — never two panes. Every modifier below is
    /// applied in both states and switches on `live` by value, because a
    /// modifier applied to only one of them is a second view type at this
    /// position and costs the emulator its buffer. See `Pane`.
    ///
    /// **The marker above it is a sibling, not a modifier and not an arm.** An
    /// `if` with no `else` is an `Optional` view: the pane stays the second
    /// element of this stack's tuple whether the marker is drawn or not, so
    /// showing it does not rebuild the emulator underneath — which would throw
    /// away the very scrollback the marker is there to describe.
    /// `testTheNoticeDoesNotCostTheEmulatorWhatItIsDescribing` measures that.
    private func terminal(live: Bool) -> some View {
        VStack(spacing: 0) {
            if seededFromTrimmedBuffer {
                // **Out of band, and view-local.** This used to be bytes fed to
                // the emulator ahead of the replay, which put a claim about the
                // stream inside the stream: the replayed tail erases it with
                // `ESC[2J`, hides it by switching to the alternate screen, or
                // simply scrolls it out of the retained region — and the pane
                // then presents a partial tail as the whole session, silently.
                // Nothing the far end sends can reach a SwiftUI row.
                //
                // **And `CCGapMarker` is the right component after all.** The
                // objection to it was that a gap belongs at its position rather
                // than pinned above a pane that scrolls, and that a pinned row
                // cannot know whether a surviving pane really lost anything.
                // Neither survives the change: `seededFromTrimmedBuffer` is set
                // at the one site that seeds a *fresh* emulator, so the row is
                // only ever drawn over a pane that genuinely began in the middle
                // of the session; and that pane's whole content — scrollback
                // included — begins after the cut, so the top of the pane *is*
                // the gap's position, and it is a boundary that cannot drift.
                CCGapMarker(label: Self.trimmedBufferNotice)
                    // The live pane's own leading inset, so the rule starts on
                    // the same edge as the first column of terminal output.
                    .padding(.horizontal, CC.space.md)
            }
            SwiftTermView(
                session: session, sessionUID: sessionUID, acceptsInput: live && onScreen,
                fontSize: model.settings.terminalFontSize,
                seeded: { fromTrimmedBuffer in
                    // Off the update pass. Seeding happens inside
                    // `makeUIView`/`updateUIView`, and writing view state from
                    // there is undefined behaviour by SwiftUI's own rules.
                    DispatchQueue.main.async { seededFromTrimmedBuffer = fromTrimmedBuffer }
                })
                // Never a 55% dim. A dimmed terminal is unreadable *and* still
                // looks live; the content stays at full opacity and the frame
                // carries the fact.
                .padding(.leading, live ? CC.space.md : 0)
                .modifier(SnapshotFrame(isLive: live, stamp: lastOutputText))
                .accessibilityElement(children: .contain)
                .accessibilityLabel(paneLabel(live: live))
                // Pinch to scale, the same preference the diff surface has.
                // `simultaneousGesture` so the emulator keeps its own selection
                // and scroll gestures.
                .simultaneousGesture(pinch)
        }
    }

    /// What the marker says, on screen and out loud — `CCGapMarker` speaks its
    /// own label, so this string is both.
    ///
    /// A sentence about what is missing rather than a word about the buffer.
    /// A listener who is handed "truncated" has been told the app hit a limit;
    /// what they need to know is that the session printed more than this pane
    /// is showing. Short enough not to be cut at two lines, which is the one
    /// thing the component asks of a label.
    static let trimmedBufferNotice = "Earlier output not shown"

    /// What the pane is called out loud, which has to carry *which of the two
    /// it is*. The strip and the frame both say so on screen and neither is
    /// legible to a listener, so a snapshot announced as a terminal is the same
    /// lie those two exist to prevent, in the one channel that cannot see them.
    private func paneLabel(live: Bool) -> String {
        if live { return "Terminal for \(runLabel)" }
        guard let stamp = lastOutputText else {
            return "Snapshot of the terminal for \(runLabel)"
        }
        return "Snapshot of the terminal for \(runLabel), \(stamp)"
    }

    private var pinch: some Gesture {
        MagnifyGesture(minimumScaleDelta: 0.04)
            .onChanged { value in
                let base = pinchBase ?? model.settings.terminalFontSize
                if pinchBase == nil { pinchBase = base }
                model.settings.terminalFontSize = min(
                    AppSettings.terminalFontRange.upperBound,
                    max(AppSettings.terminalFontRange.lowerBound, base * value.magnification))
            }
            .onEnded { _ in pinchBase = nil }
    }

    @State private var pinchBase: Double?

    // MARK: - Target

    /// The paired daemon's host. There is nothing to override: the terminal goes
    /// wherever the timeline already is.
    private var host: String? {
        model.pairing.endpoint?.host.isEmpty == false ? model.pairing.endpoint?.host : nil
    }

    /// Where this terminal would connect, as an identifier — never a sentence.
    /// `nil` when there is nothing true to print, in which case the blocked
    /// reason underneath is the whole answer.
    private var targetLine: String? {
        guard let host else { return nil }
        guard let tmuxName else { return host }
        return "\(host) · =\(tmuxName)"
    }

    /// Why Connect is not offered, and where to go to change that.
    private struct Blocked {
        var reason: String
        /// A reason without a route to the fix is half an error message — and a
        /// route is a title *and* an action, so it is one value rather than two
        /// optionals that can disagree. `nil` where there honestly is no route.
        var fix: Fix?

        struct Fix {
            var title: String
            var run: () -> Void
        }
    }

    /// **One switch, two answers.**
    ///
    /// Either the connection has everything it needs — in which case this
    /// carries the exact target `connect()` will dial — or it is blocked, in
    /// which case it carries the sentence the screen has to print and the
    /// route to the fix.
    ///
    /// **One value, because the two answers have to agree.** Three reconnect
    /// affordances share this tab, and each of them needs both halves: what to
    /// dial, and what to say instead. Computed separately they drift, and the
    /// drift shows up as a control that does nothing — `connect()` returning at
    /// a guard the screen never stated. Deriving both from here makes "an
    /// action the connection would refuse" unconstructible rather than
    /// something three call sites have to remember.
    private enum ConnectState {
        /// Everything the attach needs: the run's uid, which the daemon
        /// resolves to the one live session carrying it.
        case ready(sessionUID: String)
        case blocked(Blocked)
    }

    private var connectState: ConnectState {
        guard host != nil else {
            return .blocked(
                Blocked(
                    reason: "Pair with your Mac first; the terminal uses the same connection.",
                    fix: .init(title: "Pair", run: { showPairing = true })))
        }
        // The terminal is the same connection as everything else, so a link
        // that is down is the whole story — there is no second thing to try.
        guard model.connection.phase.isConnected else {
            return .blocked(
                Blocked(reason: "Not connected to the Mac. The terminal uses the same connection."))
        }
        // Shell-equivalent authority is never granted to the bootstrap token,
        // so the daemon says per connection whether it will open one at all.
        guard model.connection.capabilities?.servesTerminal == true else {
            return .blocked(
                Blocked(
                    reason:
                        "This connection may not open a terminal. Pair this device with the Mac, then try again.",
                    fix: .init(title: "Pair", run: { showPairing = true })))
        }
        guard tmuxName != nil else {
            // No route to a fix, and honestly so — but two different truths.
            // An adopted run was never in CodeConnect's tmux, so "no longer
            // lists" would be a lie about a session the fleet is showing.
            return .blocked(
                Blocked(
                    reason: unhosted
                        ? "CodeConnect did not launch this session, so there is no tmux session of its own to attach to."
                        : "The daemon no longer lists this run, so there is no tmux session to attach to."
                ))
        }
        // The daemon allows one terminal per connection and the carrier is
        // shared, so this is the one block whose route ends something the
        // reader owns. It names the run it would end: a control that closes
        // another agent's keyboard without saying whose is not one to offer.
        if case .heldByAnotherRun(let held) = standing {
            return .blocked(
                Blocked(
                    reason:
                        "\(model.runLabel(for: held).spoken) has the terminal open, and this connection allows one at a time.",
                    fix: .init(title: "End that terminal and open this one", run: takeOver)))
        }
        return .ready(sessionUID: sessionUID)
    }

    private var blockedReason: Blocked? {
        guard case .blocked(let blocked) = connectState else { return nil }
        return blocked
    }

    private func connectIfPossible() {
        // Back into *this run's* terminal or none. The carrier is shared, so
        // anything automatic here would open a shell for a run the reader
        // merely navigated to — and, where another run holds the terminal,
        // end that one to do it. A takeover is a decision, never a side
        // effect of appearing.
        guard case .mine = standing, session.canAttach else { return }
        // Only auto-attach back into a terminal that was already open. Opening
        // one is a deliberate act: it is shell-equivalent authority, and a tab
        // that opens a shell merely by being looked at is not something to do
        // on the user's behalf.
        if case .idle = session.phase, session.transcript(forRun: sessionUID).isEmpty { return }
        // And never re-open one the daemon closed for a reason a retry cannot
        // change — a session that ended does not come back by asking again.
        if let close = session.lastClose, !close.isRetryable { return }
        connect()
    }

    /// Take the terminal from the run that has it, which is the only way this
    /// run gets one while another is open. Reachable from the block's own
    /// route and nowhere else: every automatic path stops at `standing`.
    private func takeOver() {
        startedAt = Date()
        session.takeOver(
            sessionUID: sessionUID, cols: session.lastSize.cols, rows: session.lastSize.rows)
    }

    private func connect() {
        // The same value the screen renders from. There is no second guard
        // here that could disagree with the reason the user was shown.
        guard case .ready(let uid) = connectState else { return }
        startedAt = Date()
        session.attach(sessionUID: uid, cols: session.lastSize.cols, rows: session.lastSize.rows)
    }
}

// MARK: - Design inspection seam

/// Renders a terminal phase that needs a broken Mac to reach.
///
/// The `ended` screens are among the most consequential in the product and
/// neither can be reached without killing a session mid-flight or dropping the
/// link. Design review that cannot *see* them is review that approves them by
/// description, which is how the alarming screen ends up being the one nobody
/// looked at.
///
/// `#if DEBUG` and driven from the launch command line, exactly like the
/// `-CC_FIXTURE` and `-CC_BIOMETRICS` seams the app already ships (see the
/// README's "Test seams"): none of this exists in a release build.
///
///     xcrun simctl launch <udid> com.codeconnect.remote \
///       -cc.debug.terminalState endedLive
enum TerminalDesignState {
    /// The states worth inspecting are the ones a healthy Mac never shows.
    /// `ended` is reachable only by killing a session mid-flight, and design
    /// review that cannot *see* it is review that approves it by description.
    static func override() -> TerminalCarrier.Phase? {
        #if DEBUG
            switch UserDefaults.standard.string(forKey: "cc.debug.terminalState") {
            case "attaching":
                return .attaching
            case "ended":
                return .ended(
                    reason: "The session ended.",
                    wasAttached: false)
            case "endedLive":
                // The other half of `ended`: a terminal that *was* live, so the
                // screen keeps the transcript under a snapshot frame instead of
                // replacing it with a card.
                return .ended(
                    reason: "The connection to the Mac dropped.", wasAttached: true)
            default:
                return nil
            }
        #else
            return nil
        #endif
    }
}

// MARK: - Snapshot frame

/// The dead-terminal treatment: a 1pt inset frame with a `micro` label
/// centred on its top edge reading `SNAPSHOT · 09:14:22`. **The frame is the
/// fact.**
///
/// It replaces `.opacity(0.55)`, which was two mistakes at once — it made the
/// text unreadable *and* it still looked live, because a dim terminal is what a
/// terminal looks like at night.
private struct SnapshotFrame: ViewModifier {
    let isLive: Bool
    let stamp: String?

    /// **`content` appears once here, at one position, in both states.** An
    /// `if` would put the live pane and the snapshot pane in two arms of a
    /// `_ConditionalContent`, which is two identities: SwiftUI would dismantle
    /// the emulator and build a fresh one at the moment the session ends and
    /// its scrollback becomes the only copy of itself. So the decoration
    /// switches on values instead — insets that go to zero, and overlays that
    /// are absent while the terminal is live, both of which leave the type of
    /// this view the same either way.
    ///
    /// The frame is not labelled here. The pane carries one label for both
    /// states, in `TerminalTabView.paneLabel`, because a listener needs the
    /// snapshot and its stamp said once rather than as two elements.
    func body(content: Content) -> some View {
        content
            .padding(isLive ? 0 : CC.space.xs)
            .overlay {
                if !isLive {
                    RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous)
                        .strokeBorder(CC.color.borderStrong, lineWidth: CC.stroke.hairline)
                }
            }
            .overlay(alignment: .top) {
                if !isLive {
                    // `badgeLabel`: `SNAPSHOT · 09:14:22` classifies the frame
                    // it is punched into. It is not a section header.
                    Text(label)
                        .ccType(CC.type.badgeLabel)
                        .foregroundStyle(CC.text.tertiary)
                        .padding(.horizontal, CC.space.xs)
                        // Punches a hole in the rule it sits on, so the label
                        // reads as part of the frame rather than on top of it.
                        .background(CC.color.bg)
                        .offset(y: -CC.space.xs + 1)
                        .accessibilityHidden(true)
                }
            }
            .padding(isLive ? 0 : CC.space.md)
    }

    private var label: String {
        guard let stamp else { return "Snapshot" }
        return "Snapshot · \(stamp.replacingOccurrences(of: "last output ", with: ""))"
    }
}

// MARK: - SwiftTerm bridge

/// A terminal pane that can be told it is no longer a keyboard.
///
/// The pane outlives its session on purpose — it holds the scrollback, and the
/// carrier's capped transcript is not a copy of it — so when the session ends,
/// this one view has to stop offering input it cannot deliver. Every route from
/// a keystroke to the wire ends at `TerminalCarrier.send`, which drops what
/// arrives for a terminal that is not attached, silently and by design.
///
/// **Refusing first responder is the mechanism, and it is chosen because one
/// refusal covers every way in.** The soft keyboard is shown to a first
/// responder; the esc/tab/ctrl/^C row is an `inputAccessoryView`, so it comes
/// up with that keyboard or not at all; and hardware key presses travel the
/// responder chain, which a view that is neither first responder nor focusable
/// is not on. Scrolling is untouched — a scroll view does not need to be first
/// responder — so the scrollback the pane is kept for stays readable.
///
/// It costs the dead pane UIKit's edit menu, which is validated against the
/// first responder — Paste included, which is the one menu action that puts
/// bytes on the wire. A snapshot that cannot be copied from is a smaller loss
/// than a keyboard that swallows what is typed into it.
final class TerminalPaneView: TerminalView {
    /// Whether the session behind this pane can still receive what is typed
    /// into it.
    var acceptsInput = true {
        didSet {
            // A pane that goes dead with the keyboard already up gives it back.
            // The refusal below governs only *becoming* first responder, and a
            // session commonly ends under someone who is mid-command.
            guard !acceptsInput, isFirstResponder else { return }
            _ = resignFirstResponder()
        }
    }

    override var canBecomeFirstResponder: Bool { acceptsInput }

    override var canBecomeFocused: Bool { acceptsInput }
}

/// Hosts SwiftTerm's `TerminalView` and wires it to the terminal carrier.
///
/// Bytes flow one way through the emulator and one way back out; nothing in this
/// app ever reads them for meaning.
struct SwiftTermView: UIViewRepresentable {
    let session: TerminalCarrier
    /// The run this emulator shows. The carrier is shared, so replay is asked
    /// for by run rather than taken: bytes it happens to be holding may belong
    /// to a run that is not this one.
    let sessionUID: String
    /// Whether this pane may take what is typed into it. False for a session
    /// that has ended, and equally for one that is live behind another surface
    /// — a pane nobody is looking at must not be holding the keyboard. The same
    /// pane is kept across both changes, so this is carried as a value the view
    /// is updated with rather than expressed as a second view.
    let acceptsInput: Bool
    let fontSize: Double
    /// Called once for every emulator this view builds, with whether the bytes
    /// it was seeded with had already lost their head to the carrier's cap.
    ///
    /// The only honest place the question can be answered: a rebuild is the one
    /// path where a fresh emulator is handed a capped buffer, and this is the
    /// one line that hands it over.
    ///
    /// **Reported for every seeding, including the whole ones**, which is what
    /// makes this the notice's only writer. A pane re-seeded for another run
    /// must stop claiming the first run's gap; a pane built after a reattach is
    /// seeded from the daemon's repaint and must stop claiming any gap at all.
    let seeded: (_ fromTrimmedBuffer: Bool) -> Void

    func makeUIView(context: Context) -> TerminalPaneView {
        let view = TerminalPaneView(frame: .zero)
        view.terminalDelegate = context.coordinator
        // The hosted tmux server runs `mouse on` (that is what makes wheel
        // scrolling work at the Mac), so it advertises mouse tracking to every
        // client — including this one. SwiftTerm answers by turning a
        // one-finger pan into SGR button-drag events, which tmux reads as
        // copy-mode *selection*: touch-scrolling the terminal would start
        // highlighting text instead. It never produces wheel events, so
        // opting in buys nothing and costs the pan. Declined here until phone
        // scrolling is designed deliberately.
        view.allowMouseReporting = false
        view.font = UIFont.monospacedSystemFont(ofSize: fontSize, weight: .regular)
        // The bug this avoids: `.systemBackground` renders **white** under a
        // light trait, and a white terminal is not a thing this product has.
        // Absolute tokens, never a dynamic provider.
        let background = UIColor(CC.ansi.background)
        view.backgroundColor = background
        view.nativeBackgroundColor = background
        view.nativeForegroundColor = UIColor(CC.ansi.foreground)
        view.caretColor = UIColor(CC.ansi.foreground)
        // The app's own explicit 16-colour table. SwiftTerm's defaults are tuned
        // for a light background — its normal blue is #0000EE, which measures
        // 1.19:1 on `bg` — so inheriting them puts unreadable text in the one
        // surface this app does not control the content of.
        view.installColors(
            CC.ansi.table.map { Color(red: $0.red, green: $0.green, blue: $0.blue) })
        view.inputAccessoryView = context.coordinator.makeAccessory(for: view)
        view.acceptsInput = acceptsInput
        context.coordinator.attach(
            view: view, session: session, sessionUID: sessionUID, seeded: seeded)
        return view
    }

    func updateUIView(_ view: TerminalPaneView, context: Context) {
        if abs(view.font.pointSize - fontSize) > 0.5 {
            view.font = UIFont.monospacedSystemFont(ofSize: fontSize, weight: .regular)
        }
        view.acceptsInput = acceptsInput
        context.coordinator.attach(
            view: view, session: session, sessionUID: sessionUID, seeded: seeded)
    }

    static func dismantleUIView(_ view: TerminalPaneView, coordinator: Coordinator) {
        coordinator.detach()
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    /// Not `@MainActor` as a whole: `TerminalViewDelegate` is not isolated, so
    /// conforming from an isolated type is a data-race warning today and an
    /// error under Swift 6. SwiftTerm calls every one of these from the main
    /// thread — `TerminalView` is a `UIView` — so the callbacks assert that
    /// rather than hopping, which would reorder keystrokes.
    ///
    /// `@unchecked Sendable` is safe here by construction rather than by
    /// assertion: *every* stored property below is `@MainActor`, so there is no
    /// state this class can touch off the main actor at all.
    final class Coordinator: NSObject, TerminalViewDelegate, @unchecked Sendable {
        @MainActor private weak var view: TerminalView?
        @MainActor private var session: TerminalCarrier?
        @MainActor private var accessory: UIHostingController<TerminalKeyRow>?
        /// The run whose transcript has been replayed into this emulator, so a
        /// rebuild replays once and a *different* run replays its own rather
        /// than inheriting a screen it never wrote.
        @MainActor private var replayedRun: String?

        @MainActor
        func attach(
            view: TerminalView, session: TerminalCarrier, sessionUID: String,
            seeded: (_ fromTrimmedBuffer: Bool) -> Void
        ) {
            self.view = view
            self.session = session
            guard replayedRun != sessionUID else { return }
            replayedRun = sessionUID
            // Replay what already arrived, so switching tabs or rotating does
            // not show an empty terminal for a live session. Asked for by run:
            // the carrier is shared, and what it holds may be another run's.
            let transcript = session.transcript(forRun: sessionUID)
            if !transcript.isEmpty {
                view.feed(byteArray: transcript)
            }
            // A trimmed buffer begins in the middle of the session, and a pane
            // that draws it without saying so presents a partial tail as the
            // whole of what was printed. Reported *from here* rather than read
            // off the carrier by the view, because this is the only place a
            // genuinely fresh emulator is seeded: the pane that survived the
            // rebuild still holds every byte in its own scrollback, and telling
            // that reader something is missing would be the lie in the other
            // direction.
            seeded(!transcript.isEmpty && session.transcriptIsTruncated)
            session.deliverOutput(to: self) { [weak view] bytes in
                view?.feed(byteArray: bytes)
            }
        }

        @MainActor
        func detach() {
            // Stamped with this coordinator, so a teardown that lands after the
            // next emulator is already receiving cannot silence it. SwiftUI
            // routinely builds the replacement before dismantling what it
            // replaces, and a live terminal drawing nothing reads as a hung
            // agent rather than as a bug on this side.
            session?.stopDeliveringOutput(to: self)
            accessory = nil
        }

        @MainActor
        func makeAccessory(for view: TerminalView) -> UIView {
            let row = TerminalKeyRow(
                onEscape: { [weak self] in self?.sendKey([0x1b]) },
                onTab: { [weak self] in self?.sendKey([0x09]) },
                onControl: { [weak self] in self?.toggleControl() },
                onInterrupt: { [weak self] in self?.sendKey([0x03]) },
                onArrow: { [weak self] direction in self?.sendArrow(direction) },
                // The view in hand, not the one stored on this coordinator:
                // `attach` sets that a line later than this is built, and a cap
                // reading a modifier off `nil` is dark while Control is held.
                isControlActive: { [weak view] in view?.controlModifier ?? false })
            let controller = UIHostingController(rootView: row)
            controller.view.backgroundColor = .clear
            // An input accessory needs a concrete height; the width follows the
            // keyboard. 40pt of key cap inside 44pt of hit area plus 8pt of
            // padding each side is 60; `UIFontMetrics` keeps the row usable at
            // large Dynamic Type sizes instead of clipping the legends.
            let height = min(UIFontMetrics.default.scaledValue(for: 60), 120)
            controller.view.frame = CGRect(x: 0, y: 0, width: view.bounds.width, height: height)
            controller.view.autoresizingMask = [.flexibleWidth]
            accessory = controller
            return controller.view
        }

        @MainActor
        private func sendKey(_ bytes: [UInt8]) {
            view?.send(bytes)
        }

        @MainActor
        private func toggleControl() {
            guard let view else { return }
            view.controlModifier.toggle()
            // A mode change, not a keystroke — the input click is the
            // keystroke's, and this deserves its own confirmation.
            CCHaptic.light.fire()
        }

        /// Arrow keys, in the form the far end is currently asking for.
        ///
        /// A terminal in application-cursor mode expects `ESC O A`; in normal
        /// mode, `ESC [ A`. Sending the wrong one puts a literal `OA` in the
        /// agent's prompt, so the emulator's own mode decides.
        @MainActor
        private func sendArrow(_ direction: TerminalKeyRow.Arrow) {
            guard let view else { return }
            let application = view.getTerminal().applicationCursor
            let prefix: [UInt8] = application ? [0x1b, 0x4f] : [0x1b, 0x5b]
            sendKey(prefix + [direction.finalByte])
        }

        // MARK: TerminalViewDelegate

        nonisolated func send(source: TerminalView, data: ArraySlice<UInt8>) {
            MainActor.assumeIsolated { session?.send(data) }
        }

        nonisolated func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
            MainActor.assumeIsolated { session?.resize(cols: newCols, rows: newRows) }
        }

        nonisolated func setTerminalTitle(source: TerminalView, title: String) {}
        nonisolated func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
        nonisolated func scrolled(source: TerminalView, position: Double) {}
        nonisolated func requestOpenLink(
            source: TerminalView, link: String, params: [String: String]
        ) {}
        nonisolated func bell(source: TerminalView) {
            MainActor.assumeIsolated { CCHaptic.warning.fire() }
        }
        nonisolated func clipboardCopy(source: TerminalView, content: Data) {
            let text = String(decoding: content, as: UTF8.self)
            MainActor.assumeIsolated { UIPasteboard.general.string = text }
        }
        /// Denied on purpose: an agent that can read the phone's clipboard can
        /// read whatever was last copied, which is routinely a token.
        nonisolated func clipboardRead(source: TerminalView) -> Data? { nil }
        nonisolated func iTermContent(source: TerminalView, content: ArraySlice<UInt8>) {}
        nonisolated func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
    }
}

// MARK: - Key row

/// The keys a phone keyboard does not have but a terminal needs.
struct TerminalKeyRow: View {
    enum Arrow: CaseIterable {
        case up, down, left, right

        var finalByte: UInt8 {
            switch self {
            case .up: return 0x41
            case .down: return 0x42
            case .right: return 0x43
            case .left: return 0x44
            }
        }

        var symbol: String {
            switch self {
            case .up: return "arrow.up"
            case .down: return "arrow.down"
            case .left: return "arrow.left"
            case .right: return "arrow.right"
            }
        }

        var label: String {
            switch self {
            case .up: return "Up arrow"
            case .down: return "Down arrow"
            case .left: return "Left arrow"
            case .right: return "Right arrow"
            }
        }
    }

    let onEscape: () -> Void
    let onTab: () -> Void
    let onControl: () -> Void
    let onInterrupt: () -> Void
    let onArrow: (Arrow) -> Void
    let isControlActive: () -> Bool

    /// Whether the ctrl cap is lit. SwiftTerm owns the modifier itself and
    /// clears it as soon as it has applied it to one character, so this is
    /// never written from a guess about what it holds — only re-read from it.
    @State private var controlOn = false

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: CC.space.xs) {
                CCKeyCap("esc", spokenLabel: "Escape", action: onEscape)
                CCKeyCap("tab", spokenLabel: "Tab", action: onTab)
                CCKeyCap(
                    "ctrl", spokenLabel: controlOn ? "Control, on" : "Control",
                    isLatched: controlOn
                ) {
                    onControl()
                    controlOn = isControlActive()
                }
                CCKeyCap("^C", spokenLabel: "Control C, interrupt", action: onInterrupt)
                CCKeyCapDivider()
                ForEach(Arrow.allCases, id: \.self) { arrow in
                    CCKeyCap(symbol: arrow.symbol, spokenLabel: arrow.label) { onArrow(arrow) }
                }
            }
            .padding(.horizontal, CC.space.sm)
            .padding(.vertical, CC.space.xs)
        }
        .scrollBounceBehavior(.basedOnSize, axes: .horizontal)
        // `surfaceRaised` + a 1pt rule, never `.bar`: a blurred UIKit material
        // over #000 resolves to a flat mid-grey that belongs to no palette.
        .background(CC.color.surfaceRaised)
        .overlay(alignment: .top) { CCHairline() }
        .onAppear { controlOn = isControlActive() }
        // SwiftTerm clears `controlModifier` the moment it has applied it to a
        // character, and posts this as it does. Without it the cap stays lit
        // after `^C`, and the `d` typed next for `^D` reaches the agent's pane
        // as a literal `d` under a cap still claiming Control is held — the
        // wrong-character failure this row exists to prevent.
        //
        // The notification is the trigger, never the value: the modifier on the
        // view stays the single source of truth, so all three paths that clear
        // it — an ordinary character, a Kitty-protocol character, a mouse event
        // — resolve to one answer that is read rather than inferred.
        .onReceive(NotificationCenter.default.publisher(for: .terminalViewControlModifierReset)) {
            _ in
            controlOn = isControlActive()
        }
    }
}
