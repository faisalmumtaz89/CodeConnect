import SwiftTerm
import SwiftUI
import UIKit

// SwiftTerm exports its own `Color` (an RGB terminal colour), so SwiftUI's has
// to be named explicitly anywhere both modules are in scope. Aliasing once beats
// qualifying at twenty call sites and beats importing SwiftTerm submodules.
private typealias UIColour = SwiftUI.Color

/// The live terminal: SSH to the Mac, attached to the agent's own tmux session.
///
/// This is the half of the product that is Termius. It is deliberately the
/// *second* surface, not the first — everything the app knows about state comes
/// from the daemon's event log, and this is where you go when you want to be the
/// Mac's keyboard instead.
///
/// The reason for the strip at the top of every state: **the terminal is the
/// degraded layer and it must say which layer it is on at all times.** A
/// terminal showing the last bytes it received looks exactly like a live one. A
/// banner can be scrolled past; a 28pt strip that never moves cannot.
struct TerminalTabView: View {
    /// The tmux session name to attach to, or nil when the daemon no longer
    /// lists this run — or never hosted it. There is no fallback: `tmux attach
    /// -t =<uid>` cannot work, and attaching to a name nothing vouches for
    /// could hand the user a different agent's keyboard.
    let tmuxName: String?
    /// True when the run is listed but has no tmux location: adopted, observed
    /// through its hooks, launched by something other than CodeConnect. The
    /// blocked reason has to tell that story rather than claim the run is gone.
    let unhosted: Bool
    /// What to call this run on screen.
    let displayName: String

    @Environment(AppModel.self) private var model
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.dynamicTypeSize) private var typeSize

    @State private var session = SSHTerminalSession()
    /// When the current connection attempt started, for the elapsed counter
    /// every wait owes the reader.
    @State private var startedAt: Date?
    /// When bytes last arrived. Sampled at most once a second — the strip shows
    /// a clock time, and re-rendering on every chunk would cost the terminal
    /// its scroll performance to gain nothing.
    @State private var lastOutputAt: Date?
    @State private var showSSHSettings = false
    @State private var showPairing = false

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
        .onChange(of: scenePhase) { _, phase in
            // Reconnect on foreground. iOS tears down sockets in the
            // background, so coming back to a terminal that *looks* attached is
            // the normal case, not the exception.
            guard phase == .active else { return }
            connectIfPossible()
        }
        .onChange(of: session.transcript.count) { _, _ in
            let now = Date()
            guard lastOutputAt == nil || now.timeIntervalSince(lastOutputAt!) > 1 else { return }
            lastOutputAt = now
        }
        .sheet(isPresented: $showSSHSettings) {
            NavigationStack { TerminalSettingsView().environment(model) }
        }
        .sheet(isPresented: $showPairing) {
            PairingView().environment(model)
        }
        // Leaving the tab is deliberately *not* leaving the session: the SSH
        // connection stays up so switching to the timeline and back does not
        // drop you out of tmux. It is torn down when the view is destroyed.
    }

    /// The phase every part of this screen renders from — the strip included, so
    /// the design inspection seam produces a *faithful* screen rather than a real
    /// strip over a forced card.
    ///
    /// The seam is handed this tab's **real** target. It used to mint its own
    /// (`studio.tail1234.ts.net:22`), which drew a host-key card about one Mac
    /// above a liveness strip reading another — a review seam whose entire
    /// purpose is a truthful screen cannot invent the one identifier the screen
    /// is about.
    private var phase: SSHTerminalSession.Phase {
        TerminalDesignState.override(host: host, port: model.settings.sshPort) ?? session.phase
    }

    @ViewBuilder
    private var content: some View {
        switch phase {
        case .idle:
            connectCard
        case .probing, .connecting, .authenticating:
            connectingCard
        case .needsSetup(let guidance):
            SSHSetupCard(guidance: guidance) { connect() }
        case .hostKeyChanged(let change):
            HostKeyChangedCard(change: change) { session.trustNewHostKey() }
        case .attached:
            terminal(live: true)
        case .ended(let reason, let wasAttached):
            if wasAttached || !session.transcript.isEmpty {
                terminal(live: false)
            } else {
                ended(reason)
            }
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
    /// `HostKeyChangedCard` abandons the gutter — a 52pt inset leaves about
    /// thirty characters of measure on a 402pt screen — and drops every string
    /// to 32. The strip held 52 through that, which measured as a *separate
    /// text column* on the one screen in the product that can least afford one
    /// (four edges at AX5, this being one of them). So at accessibility sizes
    /// the dot moves **above** the word rather than beside it, exactly as
    /// `SSHSetupCard` does with `lock.slash` and as this card's own header now
    /// does with its shield, and the strip's strings start on 32 with the
    /// card's. One column at every size, which is what the paragraph above
    /// claims and what it now does.
    ///
    /// **The orphaned separator this replaces.** The strip used to render
    /// `· detail` as one wrapping `Text` beside a fixed-size word. At AX5 that
    /// put the `·` alone, centred, on a line of its own above `Not live`, with
    /// the host wrapped underneath in a third alignment — measured on
    /// at accessibility sizes. A separator only means anything between two things
    /// on one line, so the stacked form does not have one.
    private func livenessStrip(maxHeight: CGFloat) -> some View {
        ScrollView(.vertical, showsIndicators: false) {
            // Centred on one line; stacked, and leading-aligned, once the
            // strip takes an accessibility size. Measured failure from the
            // version this replaces: `.top` in the single-line form floated
            // the dot above the word, because a `Reconnect` button on the same
            // row makes the row taller than the text and the text centres
            // inside it.
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
        // rendering: a always-scrollable 145pt strip at AX5 swallowed every
        // vertical drag that began inside it, so the card underneath — the one
        // carrying the fingerprints — could not be scrolled at all from the top
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
            if let sessionID = session.target?.sessionID {
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

    /// On a changed host key there is **no dismiss path that silently
    /// continues**. `session.canConnect` already refuses, but the strip states
    /// the rule itself rather than inheriting it — a Reconnect button on that
    /// screen would be the one exit the design forbids.
    private var canOfferReconnect: Bool {
        if case .hostKeyChanged = phase { return false }
        return session.canConnect && blockedReason == nil
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
        case .probing, .connecting, .authenticating:
            return Liveness(
                word: "Connecting", wordColor: CC.color.info, dotColor: CC.color.info,
                pulses: true, detail: userAtHost)
        default:
            return Liveness(
                word: "Not live", wordColor: CC.text.tertiary, dotColor: CC.text.tertiary,
                isHollow: true, detail: lastOutputText ?? userAtHost)
        }
    }

    private var userAtHost: String? {
        guard let host else { return nil }
        guard let username else { return host }
        return "\(username)@\(host)"
    }

    private var lastOutputText: String? {
        guard let lastOutputAt else { return nil }
        return "last output \(Self.clock.string(from: lastOutputAt))"
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
    /// Found by rendering at AX5, after the fix to the ended state made the
    /// screen taller: the connect card's `Terminal and SSH` button measured
    /// y=1169 on an 874pt screen — **295pt below the fold, in a container with
    /// no scroll view**, so the one control that could unblock the terminal
    /// could not be reached at all. The ended state was one line of prose away
    /// from the same fault. `SSHSetupCard` and `HostKeyChangedCard` have always
    /// scrolled; these three were the exception only because at the default size
    /// they always fit, which is the definition of a defect nobody sees.
    ///
    /// `minHeight: proxy.size.height` is what keeps the centring: while the
    /// content is shorter than the viewport it is centred in it, exactly as it
    /// is today, and the scroll view has nothing to do. `.basedOnSize` stops it
    /// bouncing a card that fits.
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
                        session.disconnect(reason: "Cancelled.")
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
        case .probing: return "Checking whether the Mac is listening for SSH…"
        case .connecting: return "Connecting to \(host ?? "the Mac")…"
        case .authenticating: return "Authenticating with this iPhone's key…"
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

    /// The third of this tab's three reconnect affordances to consult the
    /// block, which is what it was measured failing to do.
    ///
    /// The strip suppresses its own `Reconnect` when a connection cannot be
    /// made (`canOfferReconnect`) and the connect card replaces `Connect` with
    /// the reason plus a route to the fix. This one offered `Reconnect`
    /// unconditionally: a coordinate tap at its exact centre changed nothing at
    /// 2s or 6s, because `connect()` returned at `guard let username` — and it
    /// was the only control on the screen. "There are no dead buttons without
    /// an explanation."
    ///
    /// It now builds what the connect card builds, from the same value: the
    /// reason above the control it explains, and the route in place of an
    /// action that would be refused. Where there is no route — the daemon has
    /// stopped listing the run — there is no button either, and the reason is
    /// the whole answer, exactly as on the card.
    private func ended(_ reason: String) -> some View {
        let blocked = blockedReason
        // Resolved once: nothing blocking makes reconnecting itself the route;
        // a block hands over its own route, which is `nil` when there honestly
        // is not one. The title and the action come from the same value, so a
        // label can no longer outlive the action under it.
        let route: Blocked.Fix? =
            blocked == nil
            ? Blocked.Fix(title: "Reconnect", run: { connect() })
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

    private func terminal(live: Bool) -> some View {
        VStack(spacing: 0) {
            if live, let fingerprint = session.firstUseFingerprint {
                CCBanner(
                    "Pinned this Mac's SSH host key", message: fingerprint, tone: .info,
                    icon: "lock.shield")
                    .padding(CC.space.md)
            }

            SwiftTermView(session: session, fontSize: model.settings.terminalFontSize)
                // Never a 55% dim. A dimmed terminal is unreadable *and* still
                // looks live; the content stays at full opacity and the frame
                // carries the fact.
                .padding(.leading, live ? CC.space.md : 0)
                .modifier(SnapshotFrame(isLive: live, stamp: lastOutputText))
                .accessibilityLabel("Terminal for \(displayName)")
                // Pinch to scale, the same preference the diff surface has.
                // `simultaneousGesture` so the emulator keeps its own selection
                // and scroll gestures.
                .simultaneousGesture(pinch)
        }
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

    private var username: String? {
        model.settings.effectiveUsername(sessionPaths: model.sessionPaths)
    }

    private var host: String? {
        model.settings.effectiveHost(pairedHost: model.pairing.endpoint?.host)
    }

    /// Where this terminal would connect, as an identifier — never a sentence.
    /// `nil` when there is nothing true to print, in which case the blocked
    /// reason underneath is the whole answer.
    private var targetLine: String? {
        guard let host else { return nil }
        guard let username else { return host }
        guard let tmuxName else { return "\(username)@\(host)" }
        return "\(username)@\(host) · tmux -L codeconnect attach -t =\(tmuxName)"
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
    /// It is one value because the defect it replaces was these two answers
    /// being computed in two places. `blockedReason` consulted three facts and
    /// the *ended* state ignored it, so `Reconnect` was offered into a
    /// `connect()` that returned at `guard let username` — a control that did
    /// nothing at 2s and nothing at 6s, on the one screen where it was the only
    /// control. Three reconnect affordances on one tab; two consulted the block
    /// and one did not. Deriving both from this makes "an action the connection
    /// would refuse" unconstructible rather than something three call sites have
    /// to remember.
    private enum ConnectState {
        case ready(SSHTerminalSession.Target)
        case blocked(Blocked)
    }

    private var connectState: ConnectState {
        guard let host else {
            return .blocked(
                Blocked(
                    reason: "Pair with your Mac first; the terminal uses the same address.",
                    fix: .init(title: "Pair", run: { showPairing = true })))
        }
        guard let username else {
            return .blocked(
                Blocked(
                    reason:
                        "CodeConnect could not read the Mac's account name from your sessions. Set it under Settings › Terminal and SSH.",
                    fix: .init(title: "Terminal and SSH", run: { showSSHSettings = true })))
        }
        guard let tmuxName else {
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
        return .ready(
            .init(
                host: host, port: model.settings.sshPort, username: username,
                sessionID: tmuxName))
    }

    private var blockedReason: Blocked? {
        guard case .blocked(let blocked) = connectState else { return nil }
        return blocked
    }

    private func connectIfPossible() {
        guard session.canConnect else { return }
        // Only auto-connect back into a session that was already established.
        // A first connection is a deliberate act — it mints an SSH key and may
        // pin a host key.
        if case .idle = session.phase, session.transcript.isEmpty { return }
        connect()
    }

    private func connect() {
        // The same value the screen renders from. There is no second guard
        // here that could disagree with the reason the user was shown.
        guard case .ready(let target) = connectState else { return }
        startedAt = Date()
        session.connect(to: target)
    }
}

// MARK: - Design inspection seam

/// Renders a terminal phase that needs a broken Mac to reach.
///
/// `needsSetup`, `hostKeyChanged` and `ended` are three of the most consequential
/// screens in the product and none of them can be reached without switching off
/// an SSH server, regenerating a host key, or killing a session mid-flight.
/// Design review that cannot *see* the host-key screen is design review that
/// approves it by description, which is how the alarming screen ends up being
/// the one nobody looked at.
///
/// `#if DEBUG` and driven from the launch command line, exactly like the
/// `-CC_FIXTURE` and `-CC_BIOMETRICS` seams the app already ships (see the
/// README's "Test seams"): none of this exists in a release build.
///
///     xcrun simctl launch <udid> com.codeconnect.remote \
///       -cc.debug.terminalState hostKeyChanged
enum TerminalDesignState {
    /// - Parameters:
    ///   - host: **the tab's real target.** A seam that mints its own host
    ///     renders a card about `studio.tail1234.ts.net:22` above a liveness
    ///     strip reading `127.0.0.1`, and a review pass then approves a screen
    ///     nobody could ever see. The fixture only invents what there is no
    ///     truth to borrow.
    ///   - port: likewise, from settings.
    static func override(host: String?, port: Int) -> SSHTerminalSession.Phase? {
        #if DEBUG
            let target = host ?? "studio.tail1234.ts.net"
            switch UserDefaults.standard.string(forKey: "cc.debug.terminalState") {
            case "needsSetup":
                return .needsSetup(
                    SSHSetupGuidance.forOutcome(.refused, host: target, port: port)
                        ?? SSHSetupGuidance(title: "", detail: "", steps: []))
            case "hostKeyChanged":
                return .hostKeyChanged(
                    .init(
                        host: target, port: port,
                        pinned: "SHA256:8Wt1qKMz0mB4vJ7dR2xLp9NcYfT6hQeA3sVuE5oXgIk",
                        offered: "SHA256:8Wt1qKMz0mB4vJ7dR2xLp9NcYfT6hQeA3sVuE5oXgZk",
                        pinnedAt: pinnedAt))
            case "ended":
                return .ended(
                    reason: "tmux detached. The agent is still running on the Mac.",
                    wasAttached: false)
            default:
                return nil
            }
        #else
            return nil
        #endif
    }

    #if DEBUG
        /// Fixed once per process, and deliberately a little past four days.
        ///
        /// `override` is read on every render, so a date computed inside it slid
        /// forward continuously — the card's own evidence changed under the
        /// reader. And `Format.age` truncates whole days, so a fixture minted
        /// from `Date()` and read against the model's once-a-second clock
        /// rendered a four-day-old pin as `PINNED 3D AGO`. On this screen the
        /// age *is* evidence; the fixture may not round it down by a day
        /// because of a sub-second race with its own clock.
        private static let pinnedAt = Date().addingTimeInterval(-4 * 24 * 3600 - 60)
    #endif
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

    func body(content: Content) -> some View {
        if isLive {
            content
        } else {
            content
                .padding(CC.space.xs)
                .overlay {
                    RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous)
                        .strokeBorder(CC.color.borderStrong, lineWidth: CC.stroke.hairline)
                }
                .overlay(alignment: .top) {
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
                }
                .padding(CC.space.md)
                .accessibilityElement(children: .contain)
                .accessibilityLabel(label)
        }
    }

    private var label: String {
        guard let stamp else { return "Snapshot" }
        return "Snapshot · \(stamp.replacingOccurrences(of: "last output ", with: ""))"
    }
}

// MARK: - SwiftTerm bridge

/// Hosts SwiftTerm's `TerminalView` and wires it to the SSH session.
///
/// Bytes flow one way through the emulator and one way back out; nothing in this
/// app ever reads them for meaning.
struct SwiftTermView: UIViewRepresentable {
    let session: SSHTerminalSession
    let fontSize: Double

    func makeUIView(context: Context) -> TerminalView {
        let view = TerminalView(frame: .zero)
        view.terminalDelegate = context.coordinator
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
        context.coordinator.attach(view: view, session: session)
        return view
    }

    func updateUIView(_ view: TerminalView, context: Context) {
        if abs(view.font.pointSize - fontSize) > 0.5 {
            view.font = UIFont.monospacedSystemFont(ofSize: fontSize, weight: .regular)
        }
        context.coordinator.attach(view: view, session: session)
    }

    static func dismantleUIView(_ view: TerminalView, coordinator: Coordinator) {
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
        @MainActor private var session: SSHTerminalSession?
        @MainActor private var accessory: UIHostingController<TerminalKeyRow>?
        @MainActor private var replayed = false

        @MainActor
        func attach(view: TerminalView, session: SSHTerminalSession) {
            self.view = view
            guard self.session !== session || !replayed else { return }
            self.session = session
            // Replay what already arrived, so switching tabs or rotating does
            // not show an empty terminal for a live session.
            if !replayed, !session.transcript.isEmpty {
                view.feed(byteArray: ArraySlice(session.transcript))
            }
            replayed = true
            session.onOutput = { [weak view] bytes in
                view?.feed(byteArray: bytes)
            }
        }

        @MainActor
        func detach() {
            session?.onOutput = nil
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
                isControlActive: { [weak self] in self?.view?.controlModifier ?? false })
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
    }
}

// MARK: - SSH setup

/// What to switch on at the Mac, with the exact commands — and nothing that
/// switches anything on from here.
///
/// The highest-friction screen in the app, and the one where the product's
/// promise is most concrete: *CodeConnect never enables a system service on your
/// Mac.* Every string the daemon supplied is rendered verbatim.
struct SSHSetupCard: View {
    let guidance: SSHSetupGuidance
    let retry: () -> Void

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: CC.space.md) {
                // The mark sits *above* the headline rather than beside it.
                // Inline, it indented the title 28pt from the sentence
                // underneath — one ragged line in an otherwise straight stack,
                // and at AX5 a 40pt glyph wrapping against a 40pt headline.
                // Above, every string in this block starts on the page column
                // and the mark reads as an alarm rather than as a bullet.
                VStack(alignment: .leading, spacing: CC.space.xs) {
                    CCIcon("lock.slash", size: CC.size.iconLg, weight: .semibold)
                        .foregroundStyle(CC.color.warning)
                        .padding(.bottom, CC.space.xxs)

                    Text(guidance.title)
                        .ccType(CC.type.headline)
                        .foregroundStyle(CC.text.primary)
                        .fixedSize(horizontal: false, vertical: true)

                    Text(guidance.detail)
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .accessibilityElement(children: .combine)

                CCCard(padding: 0) {
                    VStack(spacing: 0) {
                        ForEach(Array(guidance.steps.enumerated()), id: \.element.id) { index, step in
                            if index > 0 { CCHairline() }
                            CCStepRow(
                                index: index + 1,
                                title: step.title,
                                message: Self.body(of: step),
                                command: step.command
                            ) {
                                if Self.isRecommended(step) {
                                    CCBadge("Recommended", tone: .success)
                                }
                            }
                        }
                    }
                }

                Text(
                    "CodeConnect never enables a system service on your Mac. These are things only you can turn on."
                )
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)

                CCButton("Try again", variant: .primary, size: .lg, fullWidth: true, action: retry)
            }
            .padding(CC.space.md)
        }
        .accessibilityElement(children: .contain)
    }

    private static func isRecommended(_ step: SSHSetupGuidance.Step) -> Bool {
        step.body.hasPrefix("Recommended")
    }

    /// The daemon's own sentence, minus the one word the badge beside it now
    /// carries. Nothing else is edited — the daemon's guidance is rendered
    /// verbatim, always — but rendering `RECOMMENDED` and "Recommended:" on the
    /// same row reads as a bug rather than as emphasis.
    private static func body(of step: SSHSetupGuidance.Step) -> String {
        guard isRecommended(step) else { return step.body }
        let stripped = step.body.replacingOccurrences(of: "Recommended: ", with: "")
        return stripped.prefix(1).uppercased() + stripped.dropFirst()
    }
}

// MARK: - Host key changed

/// A changed host key is a hard stop, not a warning.
///
/// **The app's most serious screen**, and the one place the design is allowed to
/// be alarming, because the situation is. The alarm is carried by the glyph and
/// the border; the words stay legible, because red type at this length fails
/// contrast and reads as a broken stylesheet rather than as danger.
///
/// The two fingerprints are diffed character by character and the differing
/// characters are lit — nobody reads two 47-character strings correctly at 2am,
/// so the component does it for them and shows its work.
struct HostKeyChangedCard: View {
    let change: SSHTerminalSession.HostKeyChange
    let trustNew: () -> Void

    @Environment(\.dynamicTypeSize) private var typeSize
    @State private var confirming = false

    /// **This card's own clock.**
    ///
    /// `PINNED 3D AGO` changes once an hour at most. It used to be handed
    /// `AppModel.now`, which advances every second, so the whole alarm — two
    /// diffed 47-character fingerprints, a wrapped command, a hold control — was
    /// rebuilt once a second on the app's most serious screen, to redraw a
    /// string that had not moved since the key was pinned.
    ///
    /// Read, not merely written. See `AgeTick.renderTime` for why that sentence
    /// is here.
    @State private var lastTick = Date()

    private var pinnedClock: AgeClock { AgeClock(since: change.pinnedAt, scale: .age) }

    private var now: Date { AgeTick.renderTime(lastTick: lastTick) }

    /// **The spine, on the screen that most needs one.**
    ///
    /// The card measured four text columns before this: prose at 32, the
    /// fingerprint labels at 48 inside a nested card, the mono block's content
    /// at 44, and the headline at 68 because the shield pushed it. Now the
    /// shield takes the 32–40 gutter, every string on the card starts at **52**,
    /// and the two full-bleed structures — the hairlines and the fingerprint
    /// band — run the card's whole width.
    ///
    /// 16 to the gutter, 8 of gutter, 12 of gap. Written as its three parts
    /// rather than as `36` so the arithmetic is checkable against the two left
    /// edges every other screen holds — 32 for marks, 52 for language.
    private var textInset: CGFloat {
        // At accessibility sizes the gutter is abandoned rather than defended:
        // a 52pt inset on a 402pt screen leaves ~30 characters of measure at
        // AX5, and a paragraph is worth more than a column here.
        typeSize.isAccessibilitySize
            ? CC.space.md : CC.space.md + CC.size.dot + CC.space.sm
    }

    /// The header's own leading inset — the *gutter*, so the shield hangs at
    /// 32–40 and the title lands on 52 with the prose.
    ///
    /// Once the gutter is abandoned there is no gutter to hang in, so the
    /// header takes the text inset like everything else. The two happen to be
    /// the same 16 today; written as a derivation rather than as a repeated
    /// constant, so a change to `textInset` cannot leave the title behind on a
    /// column of its own — which is exactly how the title came to sit 72.80pt
    /// right of the paragraph it heads.
    private var headerInset: CGFloat {
        typeSize.isAccessibilitySize ? textInset : CC.space.md
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: CC.space.md) {
                header
                    .padding(.leading, headerInset)
                    .padding(.trailing, CC.space.md)
                    .padding(.top, CC.space.md)

                Text(
                    "CodeConnect pinned a key for \(change.host):\(change.port) and is being offered a different one. That happens when a Mac is rebuilt or its host keys are regenerated, and it also happens when something else is answering on that address. Nothing has been sent."
                )
                .ccType(CC.type.body)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.leading, textInset)
                .padding(.trailing, CC.space.md)

                fingerprints

                VStack(alignment: .leading, spacing: CC.space.xs) {
                    Text("Check it at the Mac with")
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                    // The kit's default — the grid wrap, with `↳` on the
                    // continuation. A command is never truncated, anywhere. On
                    // the highest-stakes screen in the product this line used to
                    // run under the copy button and dissolve into a gradient at
                    // `…key.pub`, which is precisely where a hostile suffix
                    // would sit. No `wraps:` — that is the *prose* wrap, and a
                    // command is not prose.
                    CCMonoBlock("ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub", isSmall: true)
                    Text("before you trust it.")
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                }
                .padding(.leading, textInset)
                .padding(.trailing, CC.space.md)
                // The block is a nested surface, and the two-left-edges rule has
                // a corollary for those: its *text* belongs on this card's text
                // column and its *border* hangs 12pt left of it. The card is the
                // one place `textInset` is not a kit constant — it collapses to
                // the card's own inset at accessibility sizes — so the column is
                // declared rather than assumed, and the block reads it. Measured
                // before this at AX5: every string on the card at 32.00 and the
                // command at **44.00**, the last extra text edge here.
                .ccColumnInset(textInset)

                // Transparent fill, `danger` border and label. It fills solid
                // only inside the confirmation dialog — a filled red button is
                // the last confirmation, and this is not it.
                //
                // Inset on the card's own edge rather than the text column: a
                // full-width control belongs to its container, which is why the
                // pairing screen's primary spans the page and not the prose.
                // **Neutral, not red.** Trusting a changed key destroys nothing,
                // and red in this app means exactly one thing: this erases
                // something. The warning lives where it belongs — the banner
                // above, the fingerprint diff, and the confirmation that
                // follows — none of which this button needs to repeat in the
                // one colour reserved for `Forget this iPhone's SSH key`.
                CCButton(
                    "I checked - trust the new key", variant: .secondary, size: .lg,
                    fullWidth: true
                ) {
                    confirming = true
                }
                .padding(.horizontal, CC.space.md)

                // There is no dismiss path that silently continues. The only
                // exits are trusting the key or leaving the tab.
                Text(
                    "Until you do, this terminal stays closed. Nothing was typed and nothing was sent."
                )
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.leading, textInset)
                .padding(.trailing, CC.space.md)
                .padding(.bottom, CC.space.md)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            // `danger` at 8% with a 60% border. Not a `CCCard`: the card takes
            // its fill from the surface ladder, and this is the one surface in
            // the product that is allowed to be tinted.
            .ccSurface(
                fill: CC.color.danger.opacity(0.08), radius: CC.radius.lg,
                border: CC.color.danger.opacity(0.6))
            .padding(CC.space.md)
        }
        .confirmationDialog(
            "Trust the new host key?", isPresented: $confirming, titleVisibility: .visible
        ) {
            Button("Trust it", role: .destructive, action: trustNew)
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Only do this if you verified the fingerprint at the Mac itself.")
        }
        .task(id: pinnedClock) { await AgeTick.follow(pinnedClock) { lastTick = $0 } }
    }

    /// The shield is a *gutter mark*, not a word in the headline: it takes the
    /// 8pt column at 32–40 and is allowed to bleed symmetrically out of it, the
    /// same trade `CCStepRow` documents for its index badge. The title starts
    /// on 52 with every other string on the card.
    ///
    /// **At accessibility sizes the mark moves above the title**, exactly as
    /// `SSHSetupCard` does with `lock.slash`. Kept inline, a scaled shield is
    /// its own width plus a gap wide, and it pushed the title to **104.80**
    /// while `textInset` moved every other string on the card *left* to 32 —
    /// the one string that travelled the wrong way, +72.80pt from the paragraph
    /// it heads, and the fourth text edge on a card that is allowed two. Above,
    /// the mark and the title both start on the card's own scaled inset and the
    /// alarm reads as an alarm rather than as a bullet.
    private var header: some View {
        CCAdaptiveStack(
            horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xs,
            horizontalAlignment: .leading, verticalAlignment: .top
        ) {
            CCIcon("exclamationmark.shield.fill", size: CC.size.icon, weight: .semibold)
                .foregroundStyle(CC.color.danger)
                .frame(width: typeSize.isAccessibilitySize ? nil : CC.size.dot)

            Text("The Mac's SSH key changed")
                .ccType(CC.type.title)
                .foregroundStyle(CC.text.primary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("The Mac's SSH key changed")
    }

    /// A full-bleed band, not a nested card.
    ///
    /// It used to be a `CCCard` inside the alarm surface: a second 1pt border
    /// and a second 12pt radius 16pt inside the first — boxes inside boxes, and
    /// it pushed the two fingerprints onto a third column at 48. A
    /// `surfaceRaised` band with hairlines top and bottom groups them exactly as
    /// well, costs no border, and puts the labels on 52 with the prose above
    /// them.
    ///
    /// What the band keeps, because between them they are how a reader tells the
    /// two keys apart before reading either: `PINNED 3D AGO` in `success` over
    /// one key, `OFFERED NOW` in `danger` over the other, and the differing
    /// characters lit by `CCFingerprint`.
    private var fingerprints: some View {
        VStack(spacing: 0) {
            CCHairline(color: CC.color.danger.opacity(0.3))
            fingerprint(
                label: "Pinned \(Format.age(since: change.pinnedAt, now: now)) ago",
                tone: .success,
                // **Diffed too.** It is *the two* fingerprints that are diffed
                // against each other; this line was passed `comparedTo: nil` —
                // so it rendered as one uniform run at `textSecondary` and its
                // own differing character was the only one on the card not lit.
                // Worse, at #A1A1A1 it measured **1.49× the contrast of the
                // offered key's body** (7.19:1 against 4.83:1), which put the
                // second-brightest thing in the band on the 49 characters
                // carrying no information — the emphasis inversion this
                // component exists to prevent, applied one level up. Both lines
                // now run one rule: matching characters `textTertiary`, the
                // character that moved at full `text`, and — because both keys
                // wrap at the same character — the two lit glyphs sit one
                // directly above the other.
                //
                // `name:` is not decoration on this screen: the two blocks
                // below are the same 47 characters differing in a handful of
                // places, and each spells itself out one character at a time so
                // it can be checked against the Mac. Spoken without a name they
                // are two indistinguishable streams of letters — and a label on
                // the *outside* cannot fix that, because it would replace the
                // spelling rather than introduce it.
                //
                // `referenceName:` because a diffed line names what it is being
                // compared against, and this one is compared against the *other*
                // key. Without it the spoken label read "The key you pinned: …
                // 1 character differs from the pinned key" — a line naming
                // itself as its own reference, on the screen where knowing which
                // key is which is the entire task.
                view: CCIdentity.fingerprint(
                    change.pinned, comparedTo: change.offered, name: "The key you pinned",
                    referenceName: "the key offered now"))
            CCHairline(color: CC.color.danger.opacity(0.3))
            fingerprint(
                label: "Offered now",
                tone: .danger,
                // Diffed against the pinned key: the characters that moved
                // are the bright ones.
                view: CCIdentity.fingerprint(
                    change.offered, comparedTo: change.pinned, name: "The key offered now"))
            CCHairline(color: CC.color.danger.opacity(0.3))
        }
        .background(CC.color.surfaceRaised)
    }

    private func fingerprint(label: String, tone: CCTone, view: CCFingerprint) -> some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            // `fieldLabel`: this *names the value underneath it* — which key,
            // and how old — exactly as `ACCOUNT NAME` names the field below it.
            // The one documented departure is the colour: `success` over the
            // pinned key and `danger` over the offered one is the whole reason
            // a reader can tell the two apart before reading either.
            Text(label.uppercased())
                .ccType(CC.type.fieldLabel)
                .foregroundStyle(tone.color)
                .accessibilityLabel(label)
            view
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.leading, textInset)
        .padding(.trailing, CC.space.md)
        .padding(.vertical, CC.space.md)
    }
}
