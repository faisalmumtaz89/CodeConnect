import SwiftUI
import UIKit

/// Identifies a diff sheet presentation. A wrapper rather than a bare `String`
/// so `sheet(item:)` can be used without making every string in the app
/// `Identifiable`.
struct DiffRoute: Identifiable, Hashable {
    /// The run's key, never its tmux name — see `SessionRoute`.
    let key: String
    var id: String { key }
}

/// Reading an agent's diff on a phone, well enough to decide.
///
/// Unified only — side-by-side on a 390-point screen is two columns of nothing.
/// The things that make it readable are all deliberate:
///
///   * **Folded context.** Three lines each side of a change stay; the rest of a
///     long unchanged run collapses to one 44pt row. Nothing is hidden that was
///     not unchanged.
///   * **Wrapping you can see.** Long lines soft-wrap with a `↳` on each
///     continuation, so a wrapped line is never mistaken for two lines. The wrap
///     is computed from the monospaced advance width, so the glyph lands where
///     the break actually is.
///   * **A three-part change signal**: a 2pt change bar, a marker glyph, and a
///     6% row tint. Never a 12% fill across a full-width mono row.
///   * **Word-level highlighting.** Within a deletion/addition pair the changed
///     runs take a 22% ground, so a line where one argument moved says *which*.
///   * **Pinch to scale.** Type size is a preference, not a constant, and it is
///     remembered.
///   * **Full-bleed in landscape.** Turning the phone is what you do when a line
///     is too long; the padding gets out of the way when you do.
struct DiffSheet: View {
    let key: String

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    /// When this presentation asked. The elapsed counter every wait owes the
    /// reader is measured from here, not from when the view rendered.
    @State private var askedAt: Date?
    /// The reader took the escape hatch at 20s. Distinguished from `.idle` so
    /// the screen does not silently start asking again.
    @State private var cancelled = false

    private var state: DiffState { model.diffState(for: key) }

    var body: some View {
        // `CCSheetChrome`, not a `NavigationStack`. The platform's toolbar glass
        // is wrong over this palette, and where the platform cannot be quieted
        // per-item the bar is hidden entirely rather than half-tamed.
        // Measured on iOS 26 — a `.plain` toolbar button still renders inside a
        // Liquid Glass capsule, and a self-drawn 36pt circle inside it is two
        // circles. There is nothing on this sheet that pushes, so the
        // navigation stack was buying nothing but the chrome we do not want.
        CCSheetChrome(
            // The name, not the key: a ULID in a title tells nobody anything,
            // and the fleet row it was opened from already distinguishes two
            // runs that share a name.
            //
            // Backticked, because `cc-1` is a run identifier and an identifier
            // is always monospace. `CCSheetChrome` resolves the markup through
            // `CCProse`, so the grave accents never reach the screen.
            "Diff · `\(model.displayName(for: key))`",
            onClose: { dismiss() },
            closeLabel: "Done",
            trailing: { refreshButton }
        ) {
            content
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        }
        .task {
            if case .idle = state, !cancelled { start(force: false) }
        }
        // A deep link at cold launch beats the socket: the sheet's `.task` fires
        // while the connection is still being made, `loadDiff` refuses, and the
        // screen dead-ends on a link state that stopped being true a second
        // later. Asking again the moment the link comes up is the difference
        // between a push that lands on the diff and a push that lands on an
        // apology. Only the app's own link failure is retried — a refusal the
        // daemon actually made is not re-asked behind the reader's back.
        .onChange(of: model.daemonProfile.isConnected) { _, isConnected in
            guard isConnected, !cancelled else { return }
            if case .idle = state {
                start(force: false)
            } else if state.failedOnTheLink {
                start(force: true)
            }
        }
    }

    // MARK: Chrome

    /// The bordered circle, drawn by us rather than by the platform's toolbar.
    ///
    /// The circle is a `@ScaledMetric`, not a constant: `CCIcon` grows with
    /// Dynamic Type, and a growing glyph inside a fixed 28pt circle escapes it —
    /// measured at `accessibility-extra-large`, where the arrow overflowed the
    /// border entirely and the ring vanished.
    private var refreshButton: some View {
        Button {
            start(force: true)
        } label: {
            CCIcon("arrow.clockwise", size: CC.size.icon, weight: .semibold, relativeTo: .footnote)
                .foregroundStyle(state.isLoading ? CC.text.disabled : CC.text.primary)
                .ccGlyphContainer(CC.size.glyph, relativeTo: .footnote)
                .ccHitTarget()
        }
        .buttonStyle(.plain)
        .disabled(state.isLoading || refreshBlockedReason != nil)
        .accessibilityLabel("Ask the Mac for a fresh diff")
        // The visible reason for this control lives in the content beneath it —
        // the ticking wait notice while it is loading, the link's own account of
        // itself when the link is what stopped it. A header row has nowhere to
        // draw a sentence, so the sentence is drawn where there is room for it.
        .accessibilityHint(refreshBlockedReason ?? "")
    }

    private var refreshBlockedReason: String? {
        model.linkHealth.disabledReason
    }

    // MARK: States

    @ViewBuilder
    private var content: some View {
        if let sample = DiffDesignState.sample {
            DiffDocumentView(
                key: key, raw: sample.raw, diff: sample.parsed, fetchedAt: Date(),
                refresh: { start(force: true) })
        } else {
            liveContent
        }
    }

    @ViewBuilder
    private var liveContent: some View {
        switch state {
        case .idle where cancelled:
            // "Stopped asking", not "stopped waiting": `waiting` is a decision
            // state elsewhere in the product — the Deck's, the fleet's — and one
            // word per state means this screen does not borrow it for a network
            // wait. It also matches the label the notice above it used.
            CCEmptyState(
                glyph: "stop.circle",
                title: "Stopped asking",
                message: "The Mac never answered, so nothing was rendered. Nothing was changed.",
                actionTitle: "Ask again",
                action: { start(force: true) })
        case .idle, .loading:
            if model.daemonProfile.isConnected || state.isLoading {
                loading
            } else {
                notConnected
            }
        // Who said it decides which screen this is. A string the app wrote
        // about its own link is never shown under a caption promising the
        // daemon's words: the daemon's own words are quoted verbatim and
        // nothing else is ever passed off as them. The branch that rule needs
        // has existed all along.
        case .failed(.app(let reason)):
            if model.daemonProfile.isConnected {
                unanswered(reason)
            } else {
                notConnected
            }
        case .failed(.daemon(let reason)):
            failed(reason)
        case .loaded(let raw, let parsed, let fetchedAt):
            DiffDocumentView(
                key: key, raw: raw, diff: parsed, fetchedAt: fetchedAt,
                refresh: { start(force: true) })
        }
    }

    /// Not a bare `ProgressView`. A skeleton with the shape of the thing being
    /// waited for, a sentence, and a counter that ticks.
    private var loading: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: CC.space.md) {
                if let caveat = model.daemonProfile.diffCaveat {
                    // Verbatim. It is the daemon's own admission that it never
                    // advertised this, and paraphrasing it would soften a fact.
                    CCBanner("Asking anyway", message: caveat, tone: .info)
                }

                DiffSkeleton()

                CCWaitingNotice(
                    elapsed: elapsed,
                    label: "Asking the Mac for git diff HEAD",
                    lateLabel: "Still asking the Mac",
                    lateAfter: 8,
                    actionTitle: "Cancel",
                    action: cancelAction)
            }
            .padding(CC.space.md)
        }
    }

    private var notConnected: some View {
        CCEmptyState(
            glyph: "bolt.horizontal.circle",
            title: "Not connected",
            message: "Not connected — the daemon cannot be asked for a diff.",
            tone: .warning,
            actionTitle: "Try again",
            action: { start(force: true) }
        ) {
            // The link's own account of itself, so the reader does not have to
            // leave the screen to learn why.
            //
            // `footnote`, not `monoSmall`: `LinkHealth.detail` is English —
            // "The socket is open but the daemon has gone quiet." — and prose is
            // never monospace.
            Text(model.linkHealth.detail)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// The daemon answered, and what it said was a refusal. **Only reachable
    /// from `.failed(.daemon)`** — the caption below is a promise about the
    /// provenance of the block under it, and the type system now keeps it.
    private func failed(_ reason: String) -> some View {
        ScrollView {
            CCEmptyState(
                glyph: "exclamationmark.octagon",
                title: "The Mac could not read the diff",
                message: "No diff was produced. The daemon's own reason follows, verbatim.",
                tone: .danger,
                actionTitle: "Try again",
                action: { start(force: true) }
            ) {
                // Verbatim, monospace, selectable, never truncated, never
                // paraphrased.
                CCMonoBlock(reason, tone: .neutral, wraps: true)
                    .padding(.top, CC.space.xs)
            }
            .padding(.horizontal, CC.space.md)
        }
    }

    /// The link was up, the request went out, and nothing came back.
    ///
    /// Not `failed(_:)`: there is no daemon sentence to quote, so the screen
    /// says who is speaking and states what it actually observed. Not
    /// `notConnected` either, which would claim a link problem the app has no
    /// evidence for. `footnote` rather than `CCMonoBlock` for the same reason
    /// the `notConnected` branch uses it — this is the app's English, and prose
    /// is never monospace.
    private func unanswered(_ reason: String) -> some View {
        ScrollView {
            CCEmptyState(
                glyph: "questionmark.circle",
                title: "No answer from the Mac",
                message:
                    "The link is up and the request went out. Nothing came back, and the daemon "
                    + "gave no reason. This is the app's own account of what it saw.",
                tone: .warning,
                actionTitle: "Ask again",
                action: { start(force: true) }
            ) {
                Text(reason)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.tertiary)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.top, CC.space.xs)
            }
            .padding(.horizontal, CC.space.md)
        }
    }

    private var elapsed: TimeInterval {
        guard let askedAt else { return 0 }
        return max(0, model.now.timeIntervalSince(askedAt))
    }

    /// No spinner without an escape. The way out appears at 20s, not
    /// before — an escape hatch offered immediately reads as an expectation of
    /// failure.
    private var cancelAction: (() -> Void)? {
        guard elapsed >= 20 else { return nil }
        return {
            cancelled = true
            model.clearDiff(key: key)
        }
    }

    private func start(force: Bool) {
        cancelled = false
        askedAt = Date()
        model.loadDiff(key: key, force: force)
    }
}

// MARK: - Design inspection seam

/// A diff, for looking at.
///
/// The grid is the craft centre of the app and it is the one surface that cannot
/// be put on screen on demand: it needs a daemon, a session, a readable
/// worktree and uncommitted changes, all at once. Without this, the change bar,
/// the 6% tint, the word-level highlight, the fold row and the wrap glyph ship
/// having been read but never seen.
///
/// `#if DEBUG` and driven from the launch command line, exactly like the
/// `-CC_FIXTURE` seam the app already ships: none of it exists in a release
/// build.
///
///     xcrun simctl launch <udid> com.codeconnect.CodeConnect -cc.debug.diff sample
///     xcrun simctl launch <udid> com.codeconnect.CodeConnect -cc.debug.diff truncated
enum DiffDesignState {
    static var sample: (raw: SessionDiff, parsed: UnifiedDiff)? {
        #if DEBUG
            switch UserDefaults.standard.string(forKey: "cc.debug.diff") {
            case "sample":
                let raw = SessionDiff(
                    sessionID: "sample", unified: unified, truncated: false,
                    capturedAt: ISO8601DateFormatter().string(from: Date()), note: nil)
                return (raw, UnifiedDiff.parse(unified))
            case "truncated":
                let text = oversized
                let raw = SessionDiff(
                    sessionID: "sample", unified: text, truncated: true,
                    capturedAt: ISO8601DateFormatter().string(from: Date()), note: nil)
                return (raw, UnifiedDiff.parse(text))
            default:
                return nil
            }
        #else
            return nil
        #endif
    }

    #if DEBUG
        /// Deliberately shaped to exercise every part of the grid at once: a
        /// deletion/addition pair that differs by one token (word-level
        /// highlight), a run of context long enough to fold, a line wider than
        /// the column count (continuation glyph), and a second file so the chip
        /// row and the sticky file header have something to do.
        private static let unified = """
            diff --git a/ios/CodeConnect/Net/Sender.swift b/ios/CodeConnect/Net/Sender.swift
            --- a/ios/CodeConnect/Net/Sender.swift
            +++ b/ios/CodeConnect/Net/Sender.swift
            @@ -12,16 +12,18 @@ func send(_ text: String) async throws
                 let payload = encode(text)
                 var attempt = 0
            -    try await transport.write(payload)
            +    while attempt < maxAttempts {
            +        try await transport.write(payload, deadline: .now() + .seconds(5))
            +        attempt += 1
            +    }
                 return
                 // eight lines of context follow so the fold row has something to fold
                 let a = 1
                 let b = 2
                 let c = 3
                 let d = 4
                 let e = 5
                 let f = 6
                 let g = 7
            -    logger.debug("sent \\(payload.count) bytes to \\(transport.endpoint.description) after \\(attempt) attempts")
            +    logger.info("sent \\(payload.count) bytes to \\(transport.endpoint.description) after \\(attempt) attempts")
                 }
            diff --git a/ios/CodeConnect/Views/FleetView.swift b/ios/CodeConnect/Views/FleetView.swift
            --- a/ios/CodeConnect/Views/FleetView.swift
            +++ b/ios/CodeConnect/Views/FleetView.swift
            @@ -40,7 +40,7 @@ struct FleetView: View
                 var body: some View {
            -        List(rows) { row in
            +        ScrollView { LazyVStack(spacing: 0) { rows } }
                     }
            """

        /// **The state that shipped as a blank black rectangle.**
        ///
        /// A capture the daemon cut at its 512KB cap, holding one hunk far
        /// longer than the grid draws in a pass — so it exercises the
        /// truncation banner, the per-hunk marker, the `Draw more` control and
        /// the terminal `Truncated at 512KB` gap in one launch, without a
        /// 1.5MB worktree and a live Mac. This state was reached exactly once
        /// by building a real one; a state that expensive to reach is a state
        /// nobody ever looks at.
        private static var oversized: String {
            let count = DiffDocumentView.hunkRowBudget * 2 + 100
            var lines = [
                "diff --git a/ios/CodeConnect/Model/Generated.swift"
                    + " b/ios/CodeConnect/Model/Generated.swift",
                "--- a/ios/CodeConnect/Model/Generated.swift",
                "+++ b/ios/CodeConnect/Model/Generated.swift",
                "@@ -1,\(count + 2) +1,\(count + 2) @@ func generated()",
                " import Foundation",
                " ",
            ]
            for index in 0..<(count / 2) {
                lines.append("-    let value_\(index) = compute(index: \(index), scale: \(index % 17))")
                lines.append(
                    "+    let value_\(index) = recompute(index: \(index), factor: \(index % 17))")
            }
            lines.append("… diff truncated by CodeConnect")
            return lines.joined(separator: "\n")
        }
    #endif
}

// MARK: - Skeleton

/// The shape of a diff that has not arrived: a stamp bar, three chip bars, then
/// mono-width bars with their `+` / `−` markers already drawn.
///
/// The markers are the point. A block of grey bars could be anything; a block of
/// grey bars with a column of `+` and `−` down the left is unmistakably a diff,
/// so the wait is spent looking at the right place.
private struct DiffSkeleton: View {
    /// Varied, not random: a skeleton that reshuffles on every render reads as
    /// content arriving and then leaving again.
    private static let widths: [CGFloat] = [220, 150, 260, 190, 120, 240, 170, 200, 130, 210]
    private static let markers = ["+", "+", " ", "−", "+", " ", " ", "−", "+", " "]

    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.md) {
            CCSkeleton(width: 180, height: 20, relativeTo: .body)

            HStack(spacing: CC.space.xs) {
                CCSkeleton(width: 96, height: 32, radius: CC.radius.pill, relativeTo: .footnote)
                CCSkeleton(width: 110, height: 32, radius: CC.radius.pill, relativeTo: .footnote)
                CCSkeleton(width: 84, height: 32, radius: CC.radius.pill, relativeTo: .footnote)
            }

            VStack(alignment: .leading, spacing: CC.space.xs) {
                ForEach(Array(Self.widths.enumerated()), id: \.offset) { index, width in
                    HStack(spacing: CC.space.sm) {
                        Text(Self.markers[index])
                            .ccType(CC.type.mono)
                            .foregroundStyle(CC.text.disabled)
                            .frame(width: CC.space.sm, alignment: .center)
                        CCSkeleton(width: width, height: 12, radius: CC.radius.sm, relativeTo: .caption)
                    }
                }
            }
            .padding(CC.space.sm)
            .frame(maxWidth: .infinity, alignment: .leading)
            .ccSurface(.surface, radius: CC.radius.md)
        }
        .accessibilityHidden(true)
    }
}

// MARK: - Document

struct DiffDocumentView: View {
    let key: String
    let raw: SessionDiff
    let diff: UnifiedDiff
    let fetchedAt: Date
    var refresh: () -> Void = {}

    @Environment(AppModel.self) private var model
    @Environment(\.verticalSizeClass) private var verticalSizeClass
    @Environment(\.dynamicTypeSize) private var typeSize

    @State private var expanded: Set<String> = []
    @State private var pinchBase: Double?
    @State private var pinchBadgeUntil: Date?
    @State private var scrollTarget: String?
    @State private var comment: DiffCommentTarget?
    /// Word-level highlight ranges, keyed by line id. Computed once per document
    /// rather than per render: it is a character-level LCS per changed pair, and
    /// re-running it on every scroll tick is how a diff starts to stutter.
    @State private var wordRanges: [Int: [Range<Int>]] = [:]
    /// Rows a hunk has been granted beyond `hunkRowBudget`, because the reader
    /// asked for them. Keyed by hunk id; absent means the standing budget.
    @State private var hunkRows: [String: Int] = [:]

    /// **How many rows one hunk draws before it stops and says so.**
    ///
    /// A `LazyVStack` is lazy in its own elements and in nothing else, and this
    /// screen used to hand it *one element per file* whose body contained every
    /// hunk and every line. So on a real 512KB capture — 6,935 lines in a single
    /// hunk — SwiftUI had to build and lay out all 6,935 rows, inserting a
    /// `CALayer` for each, before it could present its first frame. Measured
    /// against a live daemon that answered `truncated=true` in 124ms: the sheet
    /// drew its chrome and then **99.8% pure `#000000` for 56 seconds and
    /// counting** — no stamp, no truncation banner, no chips, no rows. A CPU
    /// sample had the main thread 100% inside
    /// `CA::Layer::insert_sublayer` under `DiffDocumentView.hunkView`. The
    /// reader of a large change saw an empty surface and could only conclude
    /// there was nothing to review.
    ///
    /// Two things fix it and both are needed: the document now gives the lazy
    /// stack **one element per hunk**, and a hunk draws at most this many rows.
    /// 400 is well past a screenful at every type size, renders in one frame,
    /// and the rest is one tap away at the point it was cut — where the marker
    /// says exactly how many lines are waiting.
    ///
    /// `nonisolated` because it is a constant and belongs to no actor: the view
    /// is `@MainActor` by its `View` conformance, and the debug seam that builds
    /// a document large enough to trip it is not.
    ///
    /// `-cc.debug.diffRows <n>` lowers it in debug builds, and only there. A
    /// test that has to drag through 400 rows to reach the marker is a test
    /// nobody runs; with the seam the whole held-back path — banner, marker,
    /// `Draw more` — is three seconds and no daemon.
    nonisolated static let hunkRowBudget: Int = {
        #if DEBUG
            let override = UserDefaults.standard.integer(forKey: "cc.debug.diffRows")
            if override > 0 { return override }
        #endif
        return 400
    }()

    /// Landscape on a phone: give the text the whole width. Full-bleed is the
    /// point.
    private var isLandscape: Bool { verticalSizeClass == .compact }
    private var horizontalPadding: CGFloat { isLandscape ? CC.space.xxs : CC.space.md }

    var body: some View {
        // One measurement for the whole document. The wrap column count is
        // derived from it and handed down, so the text and the `↳` markers can
        // never be computed from two different widths.
        GeometryReader { proxy in
            let metrics = CCDiffMetrics(
                fontSize: model.settings.diffFontSize * CCDiffMetrics.typeScale(for: typeSize),
                availableWidth: proxy.size.width - horizontalPadding * 2,
                gutter: CCDiffMetrics.gutter(for: typeSize))
            // Counted, not collected: one allocation-free walk of the document's
            // segments so the stamp can state what the grid is holding back
            // without building a plan for a hunk that is not on screen.
            let coverage = coverage()

            VStack(spacing: 0) {
                // No stamp on an empty diff: `+0 −0 0 files` is a row of
                // zeroes standing in for information, and the empty state
                // below already carries the capture time. Measured on a live
                // daemon — it rendered the age line twice.
                if !diff.isEmpty {
                    stampBand(coverage, within: proxy.size.height)
                    CCHairline()
                }
                if !diff.files.isEmpty, !isLandscape { fileChips }
                document(metrics: metrics)
            }
            .overlay(alignment: .topTrailing) { pinchBadge }
        }
        .gesture(pinch)
        // Off the main actor, and only over the lines the grid is drawing.
        // A character-level LCS on every changed pair of a 512KB capture is
        // seconds of work; run where it used to run it froze the surface it was
        // decorating, and computed for rows nobody can see it is seconds spent
        // on nothing. Keyed on what can change the answer rather than on the
        // document itself, so a 500KB structure is not deep-compared once a
        // second by the age line ticking above it.
        .task(id: HighlightKey(fetchedAt: fetchedAt, rows: hunkRows, expanded: expanded)) {
            let document = diff
            let limits = highlightLimits()
            let computed = await Task.detached(priority: .userInitiated) {
                Self.highlights(for: document, limits: limits)
            }.value
            guard !Task.isCancelled else { return }
            wordRanges = computed
        }
        .sheet(item: $comment) { target in
            DiffCommentSheet(key: key, target: target)
                .environment(model)
        }
    }

    // MARK: What the grid draws

    /// What one hunk draws at its current budget, and what it is holding back.
    struct HunkPlan {
        /// The segments to render, the last of them possibly cut short.
        var segments: [UnifiedDiff.Segment] = []
        /// Lines represented on screen — a collapsed fold represents all of its
        /// own, because the fold row states its count and opens on a tap.
        var drawn = 0
        /// Lines in the hunk, drawn or not.
        var total = 0

        var heldBack: Int { max(0, total - drawn) }
        var isPartial: Bool { heldBack > 0 }
    }

    /// The document's totals, from the same rule the hunks draw by.
    struct Coverage {
        var drawn = 0
        var total = 0

        var heldBack: Int { max(0, total - drawn) }
        var isPartial: Bool { heldBack > 0 }
    }

    /// **The single rule for what a hunk draws**, in both of the forms the
    /// screen needs it.
    ///
    /// `collecting: false` runs the identical walk without building the segment
    /// array, which is what lets the stamp ask every hunk in the document what
    /// it is holding back on every pass while the plans themselves are built
    /// only for the hunks the lazy stack actually materialises.
    private func plan(
        file: UnifiedDiff.FileDiff, hunk: UnifiedDiff.Hunk, collecting: Bool = true
    ) -> HunkPlan {
        let budget = hunkRows[hunk.id] ?? Self.hunkRowBudget
        var plan = HunkPlan()
        var rows = 0
        for segment in hunk.segments {
            plan.total += segment.lines.count
            guard rows < budget else { continue }

            let isCollapsed =
                segment.isFolded && !expanded.contains(foldID(file: file, hunk: hunk, segment: segment))
            if isCollapsed {
                // One row, whatever it holds.
                if collecting { plan.segments.append(segment) }
                rows += 1
                plan.drawn += segment.lines.count
                continue
            }

            let room = budget - rows
            if segment.lines.count <= room {
                if collecting { plan.segments.append(segment) }
                rows += segment.lines.count
                plan.drawn += segment.lines.count
            } else {
                if collecting { plan.segments.append(Self.prefix(segment, room)) }
                rows += room
                plan.drawn += room
            }
        }
        return plan
    }

    private func coverage() -> Coverage {
        var coverage = Coverage()
        for file in diff.files {
            for hunk in file.hunks {
                let plan = plan(file: file, hunk: hunk, collecting: false)
                coverage.drawn += plan.drawn
                coverage.total += plan.total
            }
        }
        return coverage
    }

    /// How far into each hunk the word-level highlighter needs to look.
    private func highlightLimits() -> [String: Int] {
        var limits: [String: Int] = [:]
        for file in diff.files {
            for hunk in file.hunks {
                limits[hunk.id] = plan(file: file, hunk: hunk, collecting: false).drawn
            }
        }
        return limits
    }

    private func foldID(
        file: UnifiedDiff.FileDiff, hunk: UnifiedDiff.Hunk, segment: UnifiedDiff.Segment
    ) -> String {
        "\(file.id)#\(hunk.id)#\(segment.id)"
    }

    private static func prefix(_ segment: UnifiedDiff.Segment, _ count: Int)
        -> UnifiedDiff.Segment
    {
        switch segment {
        case .shown(let id, let lines): return .shown(id: id, lines: Array(lines.prefix(count)))
        case .folded(let id, let lines): return .folded(id: id, lines: Array(lines.prefix(count)))
        }
    }

    /// The reader asked for more of one hunk, so it gets another budget's worth.
    private func grow(_ hunk: UnifiedDiff.Hunk) {
        let current = hunkRows[hunk.id] ?? Self.hunkRowBudget
        withAnimation(CC.motion.small) {
            hunkRows[hunk.id] = current + Self.hunkRowBudget
        }
    }

    /// What can change the highlight answer. Not the document — deep-comparing
    /// half a megabyte of parsed diff once a second is what the age line above
    /// it would otherwise cost.
    private struct HighlightKey: Equatable {
        let fetchedAt: Date
        let rows: [String: Int]
        let expanded: Set<String>
    }

    // MARK: Stamp

    /// `+142 −38 4 files` over `Captured on the Mac 8s ago · git diff HEAD`.
    ///
    /// The second line is not decoration: it is the whole difference between
    /// reading a diff and reading *a capture of* a diff, and the age ticks.
    /// The stamp is fixed above the document, so at accessibility sizes it is
    /// capable of taking the whole screen and leaving the diff nowhere to be.
    ///
    /// Found by rendering the live 512KB capture at AX5: stamp plus truncation
    /// banner filled the viewport, the document's scroll view was pushed
    /// entirely below the fold, and dragging did nothing because every drag
    /// landed in the fixed band. The reader could not reach a single line of the
    /// diff they had opened. Bounded to 45% with its own scroll — the same
    /// accommodation the compose bar and the Deck's action bar make at these
    /// sizes — the band keeps every word and the grid keeps a majority of the
    /// screen.
    /// **And the banner goes first inside it.** Bounding the band puts whatever
    /// is last below its fold, and what is last is the sentence saying this is
    /// not the whole diff — the one thing on the band that has to be loud. The
    /// counts it displaces are restated by the file chip and the file header a
    /// few points below; the truncation is stated nowhere else.
    /// **`ccScrollCap`, not `.frame(maxHeight:)`** — and the difference is 138pt
    /// of black.
    ///
    /// This shipped as a hand-rolled `ScrollView` with `maxHeight: height * 0.45`,
    /// which is precisely the mistake the kit's modifier exists to prevent: *a
    /// `ScrollView` takes every point it is offered*, so the band claimed 45% of
    /// the sheet whether or not it had anything to put there. Measured at AX5 on
    /// a two-file diff: **253.33pt of band around 115pt of content**, on a screen
    /// already carrying ~570pt of chrome before the first line of code.
    /// `ccScrollCap` measures the content and takes only what it needs;
    /// past the cap it gives the room back and scrolls, with the indicator that
    /// says so.
    @ViewBuilder
    private func stampBand(_ coverage: Coverage, within height: CGFloat) -> some View {
        if typeSize.isAccessibilitySize, height > 0 {
            stamp(coverage, bannerFirst: true)
                // **A third, not the 45% used elsewhere.** That figure is the
                // ceiling for a block where the *decision is made* — the Deck's
                // action bar argues its way down to a sixth on the same
                // reasoning. This band is provenance standing above the thing
                // the reader opened the sheet for, and on a truncated capture
                // its content genuinely exceeds any cap, so the number decides
                // how much code is left. A third still opens on `TRUNCATED` and
                // the first lines of the sentence, which is the part that has to
                // be loud; the rest scrolls.
                .ccScrollCap(1.0 / 3.0)
                // The sheet's own bounds, from the document's `GeometryReader`.
                // Measured outside the thing it bounds, which is the rule — see
                // `ccScrollCap`'s note.
                .ccViewport(height: height)
                .background(CC.color.surface)
        } else {
            stamp(coverage, bannerFirst: false)
        }
    }

    private func stamp(_ coverage: Coverage, bannerFirst: Bool) -> some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            if bannerFirst {
                CCBannerSlot([partialityBanner(coverage)])
                    .padding(.bottom, partialityBanner(coverage) == nil ? 0 : CC.space.xs)
            }

            counts

            // The stamp collapses to one line in landscape, where the point is
            // the code and not the provenance.
            if !isLandscape {
                Text(ageLine)
                    .ccType(CC.type.monoSmall)
                    // The age turns `danger` on a stale link: the capture is
                    // still true, but nothing newer is coming.
                    .foregroundStyle(
                        model.linkHealth.level == .stale ? CC.color.danger : CC.text.tertiary)
                    .fixedSize(horizontal: false, vertical: true)
                    // The full sentence is still what is read out; only the
                    // drawn one shortens. See `ageLine`.
                    .accessibilityLabel(Self.fullAgeLine(captured: capturedAge))
            }

            // The daemon's own note, verbatim, when it had one and there is a
            // diff for it to qualify. With no files it *is* the content and the
            // empty state renders it instead.
            if let note = raw.note, !diff.files.isEmpty {
                Text(note)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            // A partial diff shown without a loud statement is a lie about state.
            // One banner, ever — and when both things are true it carries both,
            // rather than letting a ladder drop one of them.
            if !bannerFirst {
                CCBannerSlot([partialityBanner(coverage)])
                    .padding(.top, partialityBanner(coverage) == nil ? 0 : CC.space.xs)
            }
        }
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, CC.space.sm)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(CC.color.surface)
        .accessibilityElement(children: .contain)
    }

    /// `+142 −38 4 files`, and at accessibility sizes **one wrapping run**
    /// instead of three stacked ones.
    ///
    /// Two failures, one line. Side by side and clipped, AX5 drew
    /// `+2,… −2,… 1 fi…` — three numbers the app has counted exactly, shown as
    /// ellipses, on the line whose only job is to say how big this change is.
    /// Stacking them fixed the clipping and cost 135pt: three lines of `mono` at
    /// AX5, on a screen measured at **~570pt of chrome before the first line of
    /// code** on an 874pt display.
    ///
    /// So at accessibility sizes it is a single `monoSmall` run that *wraps*
    /// rather than three `mono` lines that cannot. Nothing is truncated, nothing
    /// is dropped, and the interpunct is what a wrapped line breaks at. The step
    /// down in size is the same one `CCMonoBlock`'s inline form takes for the
    /// same reason: a summary that out-weighs the thing it summarises has
    /// inverted the hierarchy.
    @ViewBuilder
    private var counts: some View {
        let files = diff.files.count == 1 ? "1 file" : "\(diff.files.count) files"
        if typeSize.isAccessibilitySize {
            (Text("+\(diff.additions)").foregroundStyle(CC.color.success)
                + Text(" −\(diff.deletions)").foregroundStyle(CC.color.danger)
                + Text(" · \(files)").foregroundStyle(CC.text.secondary))
                .ccType(CC.type.monoSmall)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        } else {
            HStack(alignment: .firstTextBaseline, spacing: CC.space.sm) {
                Text("+\(diff.additions)")
                    .foregroundStyle(CC.color.success)
                Text("−\(diff.deletions)")
                    .foregroundStyle(CC.color.danger)
                Text(files)
                    .foregroundStyle(CC.text.secondary)
                Spacer(minLength: 0)
            }
            .ccType(CC.type.mono)
            .lineLimit(1)
            .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// The one sentence at the top that says the reader is not looking at all
    /// of it — for either of the two independent reasons that can be true, or
    /// both at once.
    ///
    /// **They are different facts and the banner never conflates them.** The
    /// daemon's cap means bytes that do not exist on this phone; the grid's
    /// budget means lines that arrived and are one tap from being drawn. The
    /// first is a warning, the second is not; both are stated, because a reader
    /// who scrolls no further than the stamp still has to know.
    /// **Short, because this banner is read at AX5 too.** Found by rendering:
    /// the first draft explained the mechanism — "a hunk holding lines back says
    /// so at its end and draws more when you ask" — which at AX5 is fifteen
    /// lines and the entire viewport, on a screen already measured at ~570pt of
    /// chrome before the first line of code. The marker states the
    /// mechanism where the mechanism is; up here, two facts and no lesson.
    private func partialityBanner(_ coverage: Coverage) -> CCBannerItem? {
        let cut = "Truncated by the daemon at its 512KB cap — this is not the whole diff."
        let drawn = "\(coverage.drawn.formatted()) of \(coverage.total.formatted()) lines drawn."

        switch (raw.truncated, coverage.isPartial) {
        case (false, false):
            return nil
        case (true, false):
            return CCBannerItem(
                .truncated, title: "Truncated", message: cut, tone: .warning, icon: "scissors")
        case (true, true):
            return CCBannerItem(
                .truncated, title: "Truncated", message: "\(cut) \(drawn)",
                tone: .warning, icon: "scissors")
        case (false, true):
            return CCBannerItem(
                .truncated, title: "Not all of it is drawn",
                message: "\(drawn) Each hunk says what it is holding back.",
                tone: .info, icon: "rectangle.compress.vertical")
        }
    }

    private var capturedAge: String {
        Format.age(since: raw.capturedDate ?? fetchedAt, now: model.now)
    }

    static func fullAgeLine(captured age: String) -> String {
        "Captured on the Mac \(age) ago · git diff HEAD"
    }

    /// **The provenance line, shortened at accessibility sizes — not dropped.**
    ///
    /// The full sentence is 41 characters, which at AX5 is *three* lines of
    /// `monoSmall` on a 402pt sheet: 120pt of an 874pt screen spent restating a
    /// constant. `git diff HEAD` is the same command on every diff this product
    /// has ever drawn — the loading state says it, the empty states say it, and
    /// nothing about it varies — while the **age** is the whole reason the line
    /// exists, because it is the difference between reading a diff and reading a
    /// *capture of* one.
    ///
    /// So at accessibility sizes the constant goes and the measurement stays,
    /// which is the same trade the landscape layout already makes ("the point
    /// is the code and not the provenance"). VoiceOver is given the full
    /// sentence either way — a screen reader has no height budget.
    private var ageLine: String {
        typeSize.isAccessibilitySize
            ? "Captured \(capturedAge) ago"
            : Self.fullAgeLine(captured: capturedAge)
    }

    // MARK: File chips

    private var fileChips: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            // **Not `LazyHStack`.** Found by rendering: a lazy stack claims the
            // whole height it is offered rather than its content's, so the chip
            // row went from a 68pt band to ~190pt of `bg` with one pill floating
            // in the middle of it. The chips are cheap and bounded by the file
            // count of a 512KB capture; the height is not negotiable.
            HStack(spacing: CC.space.xs) {
                ForEach(diff.files) { file in
                    CCDiffFileChip(file: file) {
                        scrollTarget = file.id
                    }
                }
            }
            .padding(.horizontal, CC.space.md)
            .padding(.bottom, CC.space.sm)
            .padding(.top, CC.space.sm)
        }
        .scrollBounceBehavior(.basedOnSize, axes: .horizontal)
        // The right edge fades rather than being guillotined, so a chip row that
        // runs off the screen says so.
        .mask {
            LinearGradient(
                stops: [
                    .init(color: .black, location: 0),
                    .init(color: .black, location: 0.94),
                    .init(color: .black.opacity(0), location: 1),
                ],
                startPoint: .leading, endPoint: .trailing)
        }
        .background(CC.color.surface)
        .overlay(alignment: .bottom) { CCHairline() }
    }

    // MARK: Document body

    @ViewBuilder
    private func document(metrics: CCDiffMetrics) -> some View {
        if diff.isEmpty {
            emptyTree
        } else {
            ScrollViewReader { scroller in
                ScrollView {
                    // The file headers pin. Only one level of a `LazyVStack`
                    // can, and this is the one worth having: on a four-file
                    // diff, "which file am I in" outlives "which hunk".
                    //
                    // **Each hunk is its own element of the stack.** The
                    // sections' contents used to be one `VStack` per file, which
                    // is one element — so the stack was lazy in the files and in
                    // nothing below them, and a single-file capture had to
                    // materialise every row before the sheet could draw a pixel.
                    // Handing the `ForEach`s straight to the `Section` is what
                    // makes the laziness reach the rows.
                    LazyVStack(alignment: .leading, spacing: 0, pinnedViews: [.sectionHeaders]) {
                        ForEach(diff.files) { file in
                            Section {
                                // Full-bleed headers, inset bodies: a header
                                // that shares the body's inset lets content
                                // slide past it in the margins. 12 under the
                                // header, 8 between, 16 before the next header —
                                // the rhythm the single `VStack` used to own.
                                ForEach(Array(file.notes.enumerated()), id: \.offset) {
                                    index, note in
                                    fileNote(note, metrics: metrics)
                                        .padding(.horizontal, horizontalPadding)
                                        .padding(.top, index == 0 ? CC.space.sm : CC.space.xs)
                                }
                                ForEach(Array(file.hunks.enumerated()), id: \.element.id) {
                                    index, hunk in
                                    hunkView(file: file, hunk: hunk, metrics: metrics)
                                        .padding(.horizontal, horizontalPadding)
                                        .padding(
                                            .top,
                                            index == 0 && file.notes.isEmpty
                                                ? CC.space.sm : CC.space.xs)
                                }
                                Color.clear.frame(height: CC.space.md)
                            } header: {
                                CCDiffFileHeader(file: file)
                                    .id(file.id)
                            }
                        }
                        extraLines(title: "Before the first file", lines: diff.preamble, metrics: metrics)
                        extraLines(title: "Also reported", lines: diff.trailing, metrics: metrics)
                        if raw.truncated {
                            // A reader who scrolls to the end learns it there
                            // too, not only from the banner they scrolled past.
                            CCGapMarker(label: "Truncated at 512KB")
                                .padding(.horizontal, CC.space.md)
                        }
                        Color.clear.frame(height: CC.space.xl)
                    }
                }
                .onChange(of: scrollTarget) { _, target in
                    guard let target else { return }
                    withAnimation(CC.motion.medium) { scroller.scrollTo(target, anchor: .top) }
                    scrollTarget = nil
                }
            }
        }
    }

    /// An empty diff means a clean tree *or* a directory that was never a git
    /// repository *or* one the daemon could not read. Only the daemon's note
    /// tells those apart, so when there is one it is shown verbatim **and the
    /// title stops claiming anything**.
    ///
    /// Found by rendering: against a live daemon this screen read
    /// `No uncommitted changes` in `title` over the daemon's own
    /// `session directory "…" is not readable`. The headline asserted a clean
    /// tree the daemon had explicitly declined to vouch for — a lie about state
    /// produced by a hard-coded title, which is exactly the class of lie the
    /// note exists to prevent.
    /// **Neutral in both branches, and the glyph does the discriminating.**
    ///
    /// Amber is not a mood in this product. It is `MEDIUM` risk, a stale link
    /// and a disabled control's reason — *act on this* — and nothing on this
    /// screen qualifies. A clean worktree is a good outcome; a daemon note is a
    /// sentence the app has read no part of and cannot grade, so printing it in
    /// the colour of a warning is the app asserting a severity it never
    /// measured, which is the same class of lie the verbatim note exists to
    /// prevent. A real failure has its own state (`failed(_:)`), its own title
    /// and `danger`.
    ///
    /// The two states stay unmistakable on three signals that are not colour:
    /// `checkmark` against `questionmark.folder`, two different headlines, and
    /// the presence of the daemon's own words underneath.
    private var emptyTree: some View {
        ScrollView {
            CCEmptyState(
                glyph: raw.note == nil ? "checkmark" : "questionmark.folder",
                title: raw.note == nil ? "No uncommitted changes" : "No diff to show",
                message: raw.note == nil
                    ? "This session's worktree was clean when it was read."
                    : "No diff was produced. The daemon's own reason follows, verbatim.",
                actionTitle: "Check again",
                action: refresh
            ) {
                VStack(spacing: CC.space.sm) {
                    // The daemon's note is a sentence with a *path* in it, and a
                    // path set in the message slot is set in proportional type:
                    // it rendered `"/private/tmp/ccsoak-` / `work.giePJZ"`
                    // hyphen-broken across two lines, where a reader cannot tell
                    // whether the hyphen belongs to the path. A path is never a
                    // word. Mono, wrapped, complete, selectable, and never
                    // paraphrased — the same treatment `failed(_:)` already
                    // gives the daemon's other verbatim string, so this screen
                    // has one rule for machine text instead of two.
                    if let note = raw.note {
                        CCMonoBlock(note, wraps: true, isSmall: true)
                    }

                    Text(ageLine)
                        .ccType(CC.type.monoSmall)
                        .foregroundStyle(CC.text.tertiary)
                        .multilineTextAlignment(.center)
                        .fixedSize(horizontal: false, vertical: true)
                }
                // The same 320 the message above it uses. A centred composition
                // has one column, and it is the width of its widest permitted
                // measure — not the container's, which put the block's edge 17pt
                // outside the sentence it belongs to.
                .frame(maxWidth: 320)
            }
        }
    }

    private func fileNote(_ note: String, metrics: CCDiffMetrics) -> some View {
        Text(note)
            .font(.system(size: metrics.fontSize - 1, design: .monospaced))
            .foregroundStyle(CC.text.secondary)
            .padding(.horizontal, CC.space.xs)
            .textSelection(.enabled)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func hunkView(
        file: UnifiedDiff.FileDiff, hunk: UnifiedDiff.Hunk, metrics: CCDiffMetrics
    ) -> some View {
        let plan = plan(file: file, hunk: hunk)
        return VStack(alignment: .leading, spacing: 0) {
            // The hunk's actions as values. The kit draws the control, owns the
            // 44pt target and carries the long-press — which could never work
            // here, because `.contextMenu` on the hunk loses to the
            // `textSelection(.enabled)` every diff row carries. Moving the
            // gesture onto the one band that has no selectable text is what
            // makes the conflict unconstructible rather than merely lost.
            CCHunkHeader(
                header: hunk.header, fontSize: metrics.fontSize,
                actions: CCHunkActions(
                    comment: { comment = DiffCommentTarget(file: file, hunk: hunk) },
                    copyHunk: { CCPasteboard.copy(Self.hunkText(hunk)) },
                    copyPath: { CCPasteboard.copy(file.displayPath) }))

            ForEach(plan.segments) { segment in
                let id = foldID(file: file, hunk: hunk, segment: segment)
                if segment.isFolded, !expanded.contains(id) {
                    CCFoldRow(count: segment.lines.count, metrics: metrics) {
                        withAnimation(CC.motion.small) { _ = expanded.insert(id) }
                    }
                } else {
                    ForEach(segment.lines) { line in
                        CCDiffRow(
                            line: line, metrics: metrics, wordRanges: wordRanges[line.id] ?? [])
                    }
                }
            }

            // Where the grid stopped, saying what it stopped short of. A reader
            // who scrolls to the end of a hunk finds the number there as well as
            // in the banner they scrolled past, and the way to see the rest.
            if plan.isPartial {
                CCGapMarker(
                    // The fact and the way on, in the timeline row's own idiom.
                    // The count of lines one tap draws is in the hint rather
                    // than the label:
                    // it changes on every tap, and a label that renumbers itself
                    // as you use it is noise.
                    label: "\(plan.heldBack.formatted()) more lines · Draw more",
                    actionLabel:
                        "Draws the next \(min(plan.heldBack, Self.hunkRowBudget).formatted()) "
                        + "lines of this hunk",
                    action: { grow(hunk) })
            }
        }
        .ccSurface(.surface, radius: CC.radius.md)
        .accessibilityAction(named: "Comment to agent") {
            comment = DiffCommentTarget(file: file, hunk: hunk)
        }
    }

    @ViewBuilder
    private func extraLines(title: String, lines: [String], metrics: CCDiffMetrics) -> some View {
        let meaningful = lines.filter { !$0.trimmingCharacters(in: .whitespaces).isEmpty }
        if !meaningful.isEmpty {
            // **The header goes in flush.** `CCSectionHeader` owns the content
            // column now and steps its own label out to 52 from whatever inset
            // it finds itself in; a leading padding added here would step it out
            // twice and land the label on 88. The lines it introduces stay on
            // the document's own 16, where every hunk container starts — they
            // are grid content, and the grid has its own columns.
            VStack(alignment: .leading, spacing: CC.space.xs) {
                CCSectionHeader(title, note: meaningful.count == 1 ? nil : "\(meaningful.count) lines")
                ForEach(Array(meaningful.enumerated()), id: \.offset) { _, line in
                    Text(line)
                        .font(.system(size: metrics.fontSize, design: .monospaced))
                        .foregroundStyle(CC.text.primary)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            .padding(.horizontal, horizontalPadding)
            .padding(.vertical, CC.space.sm)
        }
    }

    // MARK: Pinch

    private var pinch: some Gesture {
        MagnifyGesture(minimumScaleDelta: 0.02)
            .onChanged { value in
                let base = pinchBase ?? model.settings.diffFontSize
                if pinchBase == nil { pinchBase = base }
                model.settings.diffFontSize = min(
                    AppSettings.diffFontRange.upperBound,
                    max(AppSettings.diffFontRange.lowerBound, base * value.magnification))
                pinchBadgeUntil = Date().addingTimeInterval(0.8)
            }
            .onEnded { _ in pinchBase = nil }
    }

    /// The size, while the fingers are on it. A pinch with no readout is a
    /// setting you cannot return to.
    @ViewBuilder
    private var pinchBadge: some View {
        if let until = pinchBadgeUntil, until > model.now {
            Text("\(Int(model.settings.diffFontSize.rounded()))pt")
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
                .padding(.horizontal, CC.space.xs)
                .padding(.vertical, CC.space.xxs)
                .background(CC.color.surfaceOverlay, in: Capsule())
                .overlay { Capsule().strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline) }
                .padding(CC.space.sm)
                .transition(.opacity)
                .accessibilityHidden(true)
        }
    }

    // MARK: Helpers

    private static func hunkText(_ hunk: UnifiedDiff.Hunk) -> String {
        let body = hunk.lines.map { line -> String in
            switch line.kind {
            case .addition: return "+\(line.text)"
            case .deletion: return "-\(line.text)"
            case .context: return " \(line.text)"
            case .note: return line.text
            }
        }
        return ([hunk.header] + body).joined(separator: "\n")
    }

    /// Word-level ranges for the lines the grid is drawing.
    ///
    /// `nonisolated` and pure so it can run off the main actor: it is a
    /// character-level LCS per changed pair, which on a 512KB capture is
    /// seconds of arithmetic, and it used to run on the thread that had to draw
    /// the result. `limits` bounds it to what a hunk is actually showing, so the
    /// cost tracks the screen rather than the capture.
    nonisolated private static func highlights(
        for diff: UnifiedDiff, limits: [String: Int]
    ) -> [Int: [Range<Int>]] {
        var result: [Int: [Range<Int>]] = [:]
        for file in diff.files {
            for hunk in file.hunks {
                let limit = limits[hunk.id] ?? hunk.lines.count
                guard limit > 0 else { continue }
                let lines =
                    limit >= hunk.lines.count ? hunk.lines : Array(hunk.lines.prefix(limit))
                result.merge(CCDiffWordHighlight.map(for: lines)) { current, _ in current }
            }
        }
        return result
    }
}

// MARK: - Comment to agent

/// Which hunk the reader long-pressed. Identifiable so it can drive
/// `sheet(item:)`.
struct DiffCommentTarget: Identifiable {
    let file: UnifiedDiff.FileDiff
    let hunk: UnifiedDiff.Hunk

    var id: String { "\(file.id)#\(hunk.id)" }

    /// The anchor the agent needs: which file, which lines. Prefixed on the wire
    /// so a comment is never a sentence floating free of the code it is about.
    var anchor: String {
        let numbers = hunk.lines.compactMap { $0.newNumber ?? $0.oldNumber }
        guard let first = numbers.min(), let last = numbers.max() else {
            return "In `\(file.displayPath)`"
        }
        return first == last
            ? "In `\(file.displayPath)` line \(first)"
            : "In `\(file.displayPath)` lines \(first)–\(last)"
    }
}

/// Read → steer in one gesture. The thesis of the diff surface: the place you
/// notice the problem is the place you say so.
private struct DiffCommentSheet: View {
    let key: String
    let target: DiffCommentTarget

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    @State private var text = ""
    @State private var sending = false
    @State private var result: ComposeAttempt?
    /// Opens at `.large` and can be pulled back to `.medium`.
    ///
    /// Found by rendering: at `.medium` the primary sat on the screen's bottom
    /// edge and the reason `ccDisabled` draws underneath it — "Nothing typed
    /// yet." — was below the fold, so the sheet opened showing a dead button
    /// with no explanation. A disabled control's reason has to be *visible*,
    /// not merely present, and the detent is what decides that here.
    @State private var detent: PresentationDetent = .large

    var body: some View {
        CCSheetChrome("Comment to agent", subtitle: target.anchor, onClose: { dismiss() }) {
            ScrollView {
                // The section headers go in flush: `CCSectionHeader` and
                // `CCField`'s label own the content column and step out to 52
                // from this 16 by themselves. Anything added here would be
                // counted twice.
                VStack(alignment: .leading, spacing: CC.space.md) {
                    CCSectionHeader("The hunk", note: "first \(excerptLineCount) lines")
                    // The grid wrap, not the prose wrap: these are source lines,
                    // where a break is a fact and wears a `↳`. The reader is
                    // being asked to confirm which code they are commenting on,
                    // so the ends of the long lines may not fade — which is what
                    // the shipped horizontal-scroll default did to every line
                    // wider than the sheet.
                    CCMonoBlock(excerpt, isSmall: true)

                    CCField(
                        label: "Your comment",
                        text: $text,
                        placeholder: "What should the agent do here?",
                        axis: .vertical,
                        lineLimit: 3...8)

                    if let result {
                        CCBanner(
                            resultTitle(result), message: resultMessage(result),
                            tone: resultTone(result))
                    }

                    // The run name is an identifier, so it is monospace here as
                    // it is everywhere else. `CCButton`'s label goes through
                    // `CCProse`, which resolves the backticks and drops them.
                    CCButton(
                        "Send to CodeConnect · `\(model.displayName(for: key))`",
                        variant: .primary, size: .lg, fullWidth: true, isLoading: sending,
                        disabledReason: blockedReason
                    ) {
                        send()
                    }
                }
                .padding(CC.space.md)
            }
        }
        .presentationDetents([.medium, .large], selection: $detent)
    }

    private var excerptLineCount: Int { min(8, target.hunk.lines.count) }

    private var excerpt: String {
        target.hunk.lines.prefix(8).map { line -> String in
            switch line.kind {
            case .addition: return "+\(line.text)"
            case .deletion: return "-\(line.text)"
            case .context: return " \(line.text)"
            case .note: return line.text
            }
        }.joined(separator: "\n")
    }

    /// Disabled with a stated reason on observe-only sessions and stale links.
    /// Never a dead button.
    private var blockedReason: CCDisabledReason? {
        if let reason = model.linkHealth.disabledReason { return CCDisabledReason(reason) }
        if let summary = model.summary(for: key) {
            let capability = FleetStatusRule.capability(
                summary: summary, capabilities: model.connection.capabilities)
            if !capability.canAct, let reason = capability.reason {
                return CCDisabledReason(reason)
            }
        }
        if text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return CCDisabledReason("Nothing typed yet.")
        }
        return nil
    }

    private func send() {
        sending = true
        result = nil
        Task {
            let attempt = await model.send(text: "\(target.anchor): \(text)", to: key)
            sending = false
            result = attempt
            if case .sent = attempt {
                CCHaptic.success.fire()
                try? await Task.sleep(for: .seconds(0.6))
                dismiss()
            } else {
                CCHaptic.failure.fire()
            }
        }
    }

    private func resultTitle(_ attempt: ComposeAttempt) -> String {
        switch attempt {
        case .sent: return "Sent"
        case .refused: return "Refused"
        case .failed: return "Failed"
        }
    }

    private func resultMessage(_ attempt: ComposeAttempt) -> String {
        switch attempt {
        case .sent(let matched): return "The daemon typed it into \(matched)."
        case .refused(let reason), .failed(let reason): return reason
        }
    }

    private func resultTone(_ attempt: ComposeAttempt) -> CCTone {
        switch attempt {
        case .sent: return .success
        case .refused: return .warning
        case .failed: return .danger
        }
    }
}
