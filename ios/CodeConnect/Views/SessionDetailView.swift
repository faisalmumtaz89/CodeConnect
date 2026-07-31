import SwiftUI

/// The semantic timeline for one session, plus the two things you can do to it:
/// answer a card, or say something.
///
/// The identity block replaces the ambiguity of a bare nav title. The nav bar is
/// left with nothing but a back chevron on purpose: the pushed screen's
/// own header carries the identity, and a run is identified by `cc-1 · K76F46`,
/// which no title bar can render honestly.
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
    @State private var composeText = ""
    @State private var openApproval: ApprovalItem?
    @State private var composeResult: ComposeAttempt?
    @State private var composeResultClearTask: Task<Void, Never>?
    @State private var sending = false
    /// Auto-scroll follows the tail until the reader scrolls away from it.
    @State private var following = true
    @State private var didAutoOpen = false
    @State private var surface: Surface = .timeline
    @State private var showDiff = false
    @State private var showLinkDetail = false
    @State private var newSinceLeaving = 0
    @State private var appearedAt = Date()
    @Environment(\.dynamicTypeSize) private var typeSize

    /// The run this screen is about. Everything on it — the timeline, the
    /// compose bar, the diff, the terminal — is scoped to this one key, so a
    /// `cc-1` that exits while the screen is open cannot hand the screen to its
    /// successor.
    private var key: String { route.key }
    private var state: SessionState? { model.states[key] }
    private var summary: SessionSummary? { model.summary(for: key) }
    /// What to call it out loud: the tmux name, falling back to the key when the
    /// daemon no longer lists this run.
    private var displayName: String { model.displayName(for: key) }

    var body: some View {
        VStack(spacing: 0) {
            identityBlock
            // The surface picker lives in the content, not the navigation bar.
            // A segmented control in a `.principal` slot squeezes the trailing
            // items into an overflow menu on a phone — the diff button
            // disappeared behind a "…" — and it clips outright at large Dynamic
            // Type sizes. Below the bar it has the whole width and grows.
            CCSegmented(
                selection: $surface,
                options: Surface.allCases.map { CCSegmentedOption($0, title: $0.label) },
                accessibilityLabel: "Session surface")
            .padding(.horizontal, CC.space.md)
            .padding(.top, CC.space.lg)
            // Nothing under the picker on the Terminal side: the liveness strip
            // sits *directly beneath* it and carries no top padding of its own,
            // so the 12 that used to sit here measured as a 12pt seam of `bg`
            // between two elements that are meant to touch. The timeline keeps
            // its 12 — a scroll view abutting a control is not the same
            // relationship.
            .padding(.bottom, surface == .terminal ? 0 : CC.space.sm)
            .background(CC.color.bg)

            switch surface {
            case .timeline:
                if let state {
                    timeline(state)
                } else {
                    notInTheList
                }
            case .terminal:
                // The tmux name comes from the fleet, not from the route: tmux
                // has never heard of a uid, and a name the daemon no longer
                // vouches for could attach to a different agent.
                TerminalTabView(tmuxName: model.tmuxName(for: key), displayName: displayName)
            }
        }
        .background(CC.color.bg)
        .ccNavigationChrome()
        // Empty on purpose. The identity block below carries the name, and it
        // carries the uid tail the bar could never fit.
        .navigationTitle("")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) { diffControl }
                .ccPlainToolbarItem()
            ToolbarItem(placement: .topBarTrailing) {
                // Interactive everywhere. A control that looks tappable and is
                // not is worse than no control.
                CCFreshnessPill(health: model.linkHealth) { showLinkDetail = true }
            }
            .ccPlainToolbarItem()
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            // The compose bar types into Claude's prompt. In the terminal you
            // are already typing at the TTY, so a second text field there would
            // be two ways to say the same thing with different consequences.
            if surface == .timeline { composeBar }
        }
        .sheet(item: $openApproval) { approval in
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
            consumeDeepLink()
        }
        .onDisappear {
            state?.markReviewed()
            composeResultClearTask?.cancel()
        }
        .onChange(of: model.pendingDeepLink) { _, _ in consumeDeepLink() }
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
                        Text(summary?.folderName ?? displayName)
                            .ccType(CC.type.title)
                            .foregroundStyle(CC.text.primary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                        Spacer(minLength: CC.space.xs)
                        Text(freshness)
                            .ccType(CC.type.monoSmall)
                            .foregroundStyle(CC.text.tertiary)
                    }
                    CCIdentity(name: identityName, tail: identityTail)
                    // `textDisabled` is permitted here — one of its few allowed
                    // positions — because the folder name above it carries the
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
                }
            }
            .padding(.horizontal, CC.space.md)
            .padding(.vertical, CC.space.md)

            CCHairline()
        }
        .background(CC.color.surface)
        .accessibilityElement(children: .contain)
    }

    private var dotColour: Color {
        guard let summary else { return CC.text.tertiary }
        let status = FleetStatusRule.status(
            summary: summary, state: state,
            reviewedSeq: ReviewMarks.reviewedSeq(for: key))
        return status.ccDotColor
    }

    /// The shortest *verified* distinguishing suffix, computed by the model.
    /// Read from `identityLabels` rather than from `model.fleet`, which re-sorts
    /// the whole fleet on every access and this view's body runs once a second.
    private var identity: (name: String, tail: String?) {
        let label = AppModel.identityLabels(for: model.summaries)[key] ?? displayName
        let parts = label.components(separatedBy: " · ")
        return (parts.first ?? label, parts.count > 1 ? parts[1] : nil)
    }

    private var identityName: String { identity.name }

    private var identityTail: String? { identity.tail }

    private var freshness: String {
        guard let last = state?.lastEventAt else { return "no events" }
        return Format.age(since: last, now: model.now)
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
            if let requestID, let state {
                openApproval = state.pendingApprovals.first { $0.card.requestID == requestID }
            }
        default:
            break
        }
    }

    /// Does this link mean *this* run? A link carries a reference — a uid or a
    /// tmux name — and the model resolves a name the way the daemon does.
    private func addresses(_ reference: String) -> Bool {
        (model.resolveSessionKey(reference: reference) ?? reference) == key
    }

    // MARK: Timeline

    private func timeline(_ state: SessionState) -> some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    banner(state)

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
                            item: item, now: model.now, profile: model.daemonProfile
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

                    Color.clear
                        .frame(height: 1)
                        .id(Self.tailAnchor)
                }
                .padding(.horizontal, CC.space.md)
                .padding(.bottom, CC.space.lg)
            }
            .scrollIndicators(.hidden)
            .background(CC.color.bg)
            .simultaneousGesture(
                DragGesture().onChanged { value in
                    // Dragging downward means reading history; stop chasing the tail.
                    if value.translation.height > 12 { following = false }
                }
            )
            .onChange(of: state.timeline.count) { old, new in
                guard following else {
                    newSinceLeaving += max(0, new - old)
                    return
                }
                withAnimation(CC.motion.medium) {
                    proxy.scrollTo(Self.tailAnchor, anchor: .bottom)
                }
            }
            .onAppear {
                proxy.scrollTo(Self.tailAnchor, anchor: .bottom)
                autoOpenIfRequested(state)
            }
            .overlay(alignment: .bottomTrailing) { jumpToLatest(proxy) }
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

    /// One banner, chosen by the ladder. The sequence gap and the truncated head
    /// are **not** candidates: both have a position in time and are drawn there
    /// as `CCGapMarker`s instead.
    @ViewBuilder
    private func banner(_ state: SessionState) -> some View {
        let candidates: [CCBannerItem?] = [
            model.linkHealth.ccBannerItem(
                onRetry: { model.connection.retryNow() },
                onSettings: { showLinkDetail = true }),
            cachedBanner(state),
        ]
        if candidates.contains(where: { $0 != nil }) {
            CCBannerSlot(candidates)
                .padding(.top, CC.space.sm)
                .padding(.bottom, CC.space.xs)
        }
    }

    private func cachedBanner(_ state: SessionState) -> CCBannerItem? {
        guard let cachedAt = state.loadedFromCacheAt, !state.hasLiveData else { return nil }
        return CCBannerItem(
            .cached,
            title: "From the cache, \(Format.age(since: cachedAt, now: model.now)) old",
            message: "Nothing live has arrived for this session yet.",
            tone: .warning,
            icon: "clock.arrow.circlepath")
    }

    @ViewBuilder
    private func jumpToLatest(_ proxy: ScrollViewProxy) -> some View {
        if !following {
            Button {
                CCHaptic.light.fire()
                following = true
                newSinceLeaving = 0
                withAnimation(CC.motion.medium) {
                    proxy.scrollTo(Self.tailAnchor, anchor: .bottom)
                }
            } label: {
                HStack(spacing: CC.space.xxs + 1) {
                    CCIcon("arrow.down", size: 12, weight: .semibold, relativeTo: .caption)
                    Text(newSinceLeaving > 0 ? "\(newSinceLeaving) new" : "Latest")
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
            .transition(.opacity.combined(with: .move(edge: .bottom)))
            .accessibilityLabel("Jump to the latest event")
        }
    }

    private func autoOpenIfRequested(_ state: SessionState) {
        guard !didAutoOpen, let requestID = route.openRequestID else { return }
        didAutoOpen = true
        openApproval = state.pendingApprovals.first { $0.card.requestID == requestID }
    }

    // MARK: Compose

    /// `surfaceRaised` and a 1pt top rule — never `.bar`, whose material over
    /// `#000` resolves to a flat mid-grey smear belonging to no palette.
    private var composeBar: some View {
        VStack(spacing: 0) {
            CCHairline()
            VStack(alignment: .leading, spacing: CC.space.sm) {
                templateChips

                // The reason goes *above* the field and is visible text, not an
                // accessibility hint — that rule made concrete in the app's
                // highest-traffic control.
                if let composeResult {
                    feedbackLine(composeResult)
                } else if let reason = sendBlockedReason {
                    ComposeNote(
                        text: reason, tone: .warning, glyph: "exclamationmark.circle.fill")
                } else if isObserveOnly {
                    ComposeNote(
                        text: "Observe only — answers are given at the Mac.",
                        tone: .neutral, glyph: nil, action: ("Why?", { showLinkDetail = true }))
                }

                // Side by side normally; stacked once Dynamic Type reaches the
                // accessibility range. Measured at AX5: side by side, the field
                // keeps ~55% of the width, the placeholder wraps to five lines
                // and the send button's label breaks as "Sen / d". Stacked, both
                // get the full measure and neither wraps.
                CCAdaptiveStack(
                    horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xs,
                    verticalAlignment: .bottom
                ) {
                    // No label. The placeholder — `Say something to this agent`
                    // — already names the field, and a `MESSAGE` caption above
                    // it repeats that in 11pt while costing ~22pt of the
                    // compose bar's height, which is height the timeline is
                    // paying for. `CCField`'s label is optional for this;
                    // VoiceOver still gets a name, from the override below.
                    CCField(
                        text: $composeText,
                        placeholder: placeholder,
                        // `.vertical` so the system dictation key is available —
                        // dictation is a first-class input here.
                        axis: .vertical,
                        lineLimit: typeSize.isAccessibilitySize ? 1...3 : 1...5)
                    .accessibilityLabel("Message for \(displayName)")

                    CCButton(
                        "Send", icon: "arrow.up", variant: .primary, size: .md,
                        fullWidth: typeSize.isAccessibilitySize,
                        haptic: nil
                    ) {
                        send()
                    }
                    .disabled(isSendDisabled)
                    .accessibilityHint(sendBlockedReason ?? "Types this into the agent's prompt")
                }
            }
            .padding(.horizontal, CC.space.md)
            .padding(.top, CC.space.sm)
            .padding(.bottom, CC.space.sm)
        }
        .background(CC.color.surfaceRaised)
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
        typeSize.isAccessibilitySize ? "Say something" : "Say something to this agent"
    }

    private static let templates = [
        "Stop", "Explain", "Test it", "Smaller", "Use existing helper", "Commit & push",
    ]

    private func insert(_ template: String) {
        composeText =
            composeText.isEmpty
            ? template
            : composeText.trimmingCharacters(in: .whitespacesAndNewlines) + " " + template
    }

    @ViewBuilder
    private func feedbackLine(_ result: ComposeAttempt) -> some View {
        switch result {
        case .sent(let matched):
            ComposeNote(
                text: "Typed into the session (matched \"\(matched)\")",
                tone: .success, glyph: "checkmark")
        case .refused(let reason):
            ComposeNote(
                text: "Not typed: \(reason)", tone: .warning,
                glyph: "exclamationmark.triangle.fill")
        case .failed(let reason):
            ComposeNote(text: reason, tone: .danger, glyph: "xmark.octagon.fill")
        }
    }

    private var isObserveOnly: Bool {
        guard let summary else { return false }
        return !FleetStatusRule.capability(
            summary: summary, capabilities: model.connection.capabilities
        ).canAct
    }

    private var isSendDisabled: Bool {
        composeText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            || sendBlockedReason != nil || sending
    }

    /// Text only lands if the composer is actually on screen at the Mac, so the
    /// reasons a send cannot work are the same reasons an answer cannot.
    private var sendBlockedReason: String? {
        if let reason = model.linkHealth.disabledReason { return reason }
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

    private func send() {
        let text = composeText
        sending = true
        composeResult = nil
        composeResultClearTask?.cancel()
        Task {
            let result = await model.send(text: text, to: key)
            sending = false
            withAnimation(CC.motion.micro) { composeResult = result }
            if case .sent = result {
                // Never optimistic: the field clears only on `.sent`.
                composeText = ""
                following = true
                newSinceLeaving = 0
            }
            composeResultClearTask = Task {
                try? await Task.sleep(for: .seconds(4))
                guard !Task.isCancelled else { return }
                withAnimation(CC.motion.medium) { composeResult = nil }
            }
        }
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

