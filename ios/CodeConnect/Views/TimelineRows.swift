import SwiftUI

/// One row of the semantic timeline.
///
/// The vertical rhythm — 8pt inside a turn, 20pt between turns — is owned by the
/// list, not by the rows, so a row cannot invent its own spacing and pull the
/// whole column out of true.
///
/// **No clock reaches this row.**
///
/// It used to take a `now` that `SessionDetailView` read from `AppModel.now`,
/// which advances every second. One of the five kinds of row draws an age; the
/// other four never look at the clock at all — but `now` was a stored property,
/// so every row in the timeline became a new value once a second, and this type
/// holds a closure, which is enough to stop SwiftUI proving two of its values
/// equal. Every realized row therefore re-evaluated its body once a second.
/// Measured on a 400-event timeline, untouched: **456 row bodies per second**,
/// and a `LazyVStack` scrolled to its tail has realized all of them.
///
/// The one row that draws an age owns its own clock now — see `ApprovalRow` —
/// so nothing here has a reason to change when only the time has.
struct TimelineRow: View {
    let item: TimelineItem
    /// Needed to read a card's risk: whether the daemon classifies decides
    /// whether an absent `risk_class` means "medium" or "this daemon never
    /// said".
    let profile: DaemonProfile
    /// Fired after a long message collapses, with this row's own id, so the
    /// screen can bring the row back under the viewport — see `AgentMessageRow`.
    var onCollapse: ((String) -> Void)? = nil
    /// Fired the instant a long message expands: the reader has chosen to
    /// read history, and the screen's tail-following must know *now*.
    var onExpand: (() -> Void)? = nil
    let onOpenApproval: (ApprovalItem) -> Void

    var body: some View {
        switch item.content {
        case .userMessage(let text, let isCommand):
            UserMessageRow(text: text, date: item.date, isCommand: isCommand)
        case .agentMessage(let text, let isInterrupted):
            AgentMessageRow(
                text: text, isInterrupted: isInterrupted,
                onCollapse: { onCollapse?(item.id) }, onExpand: onExpand)
        case .tool(let tool):
            ToolRow(tool: tool)
        case .approval(let approval):
            ApprovalRow(
                approval: approval, risk: approval.assessment(profile: profile).effective
            ) { onOpenApproval(approval) }
        case .notice(let notice):
            NoticeRow(notice: notice, date: item.date)
        }
    }
}

/// The session screen's own two columns — **the same two the whole app has**.
///
/// This used to be a third content column. The list supplies a 16pt margin, the
/// gutter was 16 and the gap 12, so every line of text on the session screen
/// started at 44 while the fleet's started at 52 and the Deck's at 16: four left
/// edges where the app allows two — 32 for marks and glyphs, 52 for language.
/// The gutter is 24 now, so text lands on 52 exactly as it does on a fleet row,
/// and its contents are **trailing**-aligned so a glyph ends on 40 — the same
/// edge the fleet's dots end on.
///
/// Named rather than repeated so a tool row, a notice row and a user message
/// cannot drift apart, which is exactly what happened when each of them owned
/// its own padding.
enum TimelineSpine {
    /// Holds the tool glyph, the notice glyph and the user-message bar: 16 → 40
    /// from the screen's edge, with its contents against the right of it.
    static let gutter: CGFloat = CC.space.xl
    static let gap: CGFloat = CC.space.sm
    /// 24 (gutter) + 12 (gap) = 36 from the list's leading edge; **52** from the
    /// screen's.
    static let content: CGFloat = gutter + gap
}

// MARK: - Messages

/// What you said, marked by a bar rather than by a colour.
///
/// This replaces a `Color.accentColor.opacity(0.12)` block. The accent is the
/// primary action's fill; spending it on "a human typed this" made every user
/// message look like a button, and put a second blue on a screen where blue is
/// supposed to mean nothing at all.
struct UserMessageRow: View {
    let text: String
    let date: Date
    /// A slash command the user issued — `/model sonnet` — rendered in the
    /// kit's command typography rather than prose, because every character
    /// position in a command is load-bearing.
    var isCommand: Bool = false

    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            HStack(spacing: CC.space.xs) {
                Text("YOU")
                    .ccType(CC.type.micro)
                    .foregroundStyle(CC.text.tertiary)
                Spacer(minLength: CC.space.xs)
                Text(date, format: .dateTime.hour().minute())
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
            }
            // **`YOU` is a text run, so it starts where text starts.** It sat on
            // the list's own 16pt margin — measured x=16.33 — while the message
            // it labels, the notice rows around it and the identity block above
            // it all held 52.67. Δ −35.67pt, on a screen that is allowed two
            // vertical edges. The timestamp keeps the trailing edge, which is a
            // different rule and is correct.
            .padding(.leading, TimelineSpine.content)

            HStack(alignment: .top, spacing: TimelineSpine.gap) {
                // The marker. 2pt of `borderStrong` says "this one is yours"
                // without spending a hue on it — and it sits in the same gutter
                // as every glyph on this screen.
                Rectangle()
                    .fill(CC.color.borderStrong)
                    .frame(width: 2)
                    .frame(width: TimelineSpine.gutter, alignment: .trailing)
                    .accessibilityHidden(true)
                Text(text)
                    .ccType(isCommand ? CC.type.monoSmall : CC.type.callout)
                    .foregroundStyle(CC.text.primary)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(CC.space.sm)
                    .background(
                        CC.color.surfaceRaised,
                        in: RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous))
            }
            .fixedSize(horizontal: false, vertical: true)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("You said: \(text)")
    }
}

/// The ground of the timeline, and therefore **no container at all**.
///
/// Prose truncates at four lines with an explicit expander — long agent essays
/// are the main thing that makes a phone timeline unreadable — and the expansion
/// grows downward from the button, never yanking the reader upward.
struct AgentMessageRow: View {
    let text: String
    /// The turn was aborted while this reply was being written — the wire's
    /// `interrupted` flag, drawn rather than dropped. Silence here would let a
    /// half-sentence read as the whole answer.
    var isInterrupted: Bool = false
    /// Fired **before** the collapse, on a "Show less" tap. Collapsing removes
    /// a screen or more of height in place and the scroll view keeps its
    /// offset, which lands the reader in the blank where the text used to be.
    /// The screen anchors to this row while it is still expanded, so the
    /// height that vanishes goes from below the reader's eyes.
    var onCollapse: (() -> Void)? = nil
    /// Fired synchronously from the expand tap, before any layout moves.
    var onExpand: (() -> Void)? = nil
    @State private var expanded = false

    // CRLF is one grapheme to Swift, so counting "\n" alone misses every
    // line of a CRLF message — the same measured fact AgentProse.segments
    // splits around, applied to the collapse threshold.
    private var isLong: Bool {
        text.count > 280
            || text.split(whereSeparator: { $0 == "\n" || $0 == "\r\n" }).count > 4
    }

    var body: some View {
        // Spacing 0 at this level so the expander owns the whole gap on both
        // of its sides. Inherited from the stack, the gap above it was the
        // segment spacing and the gap below it was whatever the row's own
        // padding happened to be — two different numbers, and different again
        // depending on whether prose, a code block or a table came last.
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: CC.space.xxs) {
                if isLong && !expanded {
                // The collapsed preview is ONE `Text`, whole-message. A
                // `lineLimit` on the segmented form below would clamp each
                // segment separately — four lines *per paragraph and per code
                // block* is not a four-line preview. Fences appear literally
                // here, which a preview can afford; the expansion renders them
                // properly.
                //
                // Clamped **only because the expander exists.** `isLong` is a
                // character-count proxy for rendered lines, and a proxy
                // misses: a 202-character sentence wraps to five lines on a
                // phone, and the old unconditional clamp amputated line five
                // with no button to reveal it. Below the threshold the message
                // renders whole; the clamp is the price of a "Show more",
                // never a tax on its absence.
                Text(AgentProse.inline(AgentProse.previewSource(text)))
                    .ccType(CC.type.reading)
                    .foregroundStyle(CC.text.primary)
                    .textSelection(.enabled)
                    .lineLimit(4)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                // The full form: prose with inline markdown rendered —
                // `**bold**` as bold, never as asterisks — and fenced code in
                // the kit's own mono block, copyable and clamped behind its
                // own disclosure rather than reflowed as prose.
                ForEach(Array(AgentProse.segments(text).enumerated()), id: \.offset) {
                    _, segment in
                    switch segment {
                    case .prose(let prose):
                        Text(AgentProse.inline(prose))
                            .ccType(CC.type.reading)
                            .foregroundStyle(CC.text.primary)
                            .textSelection(.enabled)
                            .fixedSize(horizontal: false, vertical: true)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    case .code(let code):
                        CCMonoBlock(code, lineLimit: 12)
                    case .table(let table):
                        AgentTableView(table: table)
                    case .heading(let heading):
                        // One style for all six levels; the marker's job was
                        // hierarchy in a document, and here it is a title over
                        // a paragraph.
                        Text(AgentProse.inline(heading))
                            .ccType(CC.type.readingHeading)
                            .foregroundStyle(CC.text.primary)
                            .textSelection(.enabled)
                            .fixedSize(horizontal: false, vertical: true)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.top, CC.space.xs)
                    }
                }
            }
            }
            // **The abort, said once, under the words it applies to.** Not a
            // banner and not a colour: an interrupted reply is still the
            // agent's prose, and the fact that it stops early belongs where the
            // stopping happened.
            if isInterrupted {
                HStack(spacing: CC.space.xxs) {
                    CCIcon("stop.circle", size: 12, weight: .regular, relativeTo: .caption)
                    Text("Interrupted")
                        .ccType(CC.type.micro)
                }
                .foregroundStyle(CC.text.tertiary)
                .padding(.top, CC.space.xs)
                .accessibilityElement(children: .combine)
                .accessibilityLabel("Interrupted — this reply was cut short")
            }
            if isLong {
                CCButton(expanded ? "Show less" : "Show more", variant: .ghost, size: .sm) {
                    let collapsing = expanded
                    if !collapsing { onExpand?() }
                    // **Anchored before the collapse, and only before it.**
                    //
                    // Re-anchoring afterwards asks the scroll view to put this
                    // row at the top of a timeline that has just lost several
                    // screens of height — a position that may no longer exist.
                    // A reader who had scrolled to the foot of a long message
                    // was left in blank and nothing recovered it. Reported
                    // twice from a device.
                    //
                    // Anchoring first is satisfiable by construction: the row
                    // is still in its expanded geometry, so its top is a real
                    // target, and that top does not move when height is later
                    // removed from below it. The collapse then shrinks the row
                    // beneath the reader's eyes, which can strand nothing.
                    // Neither this anchor nor the toggle below it animates, so
                    // "before" is a strict ordering rather than two easings
                    // overlapping.
                    // **A cut, not a slide** — the same call the "Latest" pill
                    // makes, for the same reason, and here it is a liveness
                    // property rather than a frame-budget one.
                    //
                    // Expanded, this row is taller than the screen, and it
                    // sits in the timeline's `LazyVStack`, whose content
                    // height is an *estimate* derived from the rows it has
                    // realized. Animating the toggle makes that estimate an
                    // input to the animation's own target: the in-flight
                    // frame moves the estimate, the estimate re-derives the
                    // target, and the interpolation is restarted against it —
                    // so 0.18s of easing need never finish. Measured on
                    // iPhone 17 Pro / iOS 26.3.1 under `sample`: the whole
                    // main thread inside `GraphHost.flushTransactions` →
                    // `RootGeometry` → `sizeThatFits`, `propagate_dirty` the
                    // hottest leaf and `AnimatableAttributeHelper.checkReset`
                    // in every sample, for as long as the process was left
                    // alive. No view body is re-evaluated in that window,
                    // which is what leaves the animation rather than an
                    // invalidation loop. Winding up, it also drives the
                    // scroll offset ~119pt past the content end and lets the
                    // clamp pull it back, over and over; once it latches even
                    // that stops and only the graph still turns. A height
                    // change this large has to land in one step.
                    //
                    // The transaction, not a bare toggle, is what makes it
                    // one: every tap on the timeline also clears composer
                    // focus through a simultaneous gesture, and the screen's
                    // chrome animation is keyed on that focus — with the
                    // keyboard up, one tap changes both in one update and the
                    // ancestor `.animation(value:)` would enrol this height
                    // change in its own easing. `disablesAnimations` is the
                    // documented override for exactly that inheritance.
                    withTransaction(\.disablesAnimations, true) {
                        if collapsing { onCollapse?() }
                        expanded.toggle()
                    }
                }
                .padding(.leading, -CC.space.sm)
                // The same on both sides, and the same in both states: the
                // control reads as belonging to the message rather than
                // crowding whatever it follows.
                .padding(.vertical, CC.space.sm)
            }
        }
        // **No container is not the same as no column.** The agent's prose is
        // the ground of the timeline and draws nothing around itself, so it
        // reads in its own lane: one `sm` step off the list's 16pt margin, which
        // lands text on x=28. That is deliberately inside the 52 the tool,
        // notice and user rows hold — those carry a glyph or a bar in the gutter
        // and stay on the operational column, while prose, which has no mark to
        // hang there, reads wider. The asymmetry is the point of the reading lane.
        .padding(.leading, CC.space.sm)
        // And having stepped out, it says so: the declared column is what lets a
        // mono block or a table inside this message hang its border one inner
        // padding back — border on the 16pt page margin, text on the prose
        // column at 28 — by the kit's one rule instead of by private arithmetic.
        // One border edge, one text edge, for every surface.
        .ccColumnInset(CC.space.sm)
        // **Combined only while collapsed.** The collapsed preview is one
        // clamped Text, and "Agent said: …" makes it one clean VoiceOver
        // stop. Expanded, the message can be arbitrarily long — folding the
        // whole of it into a single synthetic label while its selectable
        // segments are also combined makes the accessibility element's cost
        // proportional to the message squared: measured as UI-test snapshot
        // queries timing out at 30s apiece over one expanded message, and a
        // VoiceOver user pays the same bill. Expanded, the segments stand as
        // their own elements and read in order, which is also how a long
        // document should sound.
        .accessibilityElement(children: expanded ? .contain : .combine)
        // The semantic preview, not the raw text: while collapsed, VoiceOver
        // should say "Table: Command, Status — 4 rows", never recite pipe
        // delimiters, and heading markers are noise read aloud too.
        .accessibilityLabel(expanded ? "" : "Agent said: \(AgentProse.previewSource(text))")
    }
}

// MARK: - Tool calls

/// One semantic line per tool call: the tool *name* is prose and its *argument*
/// is code, and the type says so.
///
/// Running is conveyed by the absence of a duration and a static glyph — the
/// `symbolEffect(.pulse)` is gone, because in a busy session that was four or
/// five independently breathing glyphs.
struct ToolRow: View {
    let tool: ToolItem
    @State private var expanded: Bool
    /// A failure auto-expands **once**, on arrival. A reader who collapses it
    /// stays collapsed, which is why this is state and not a computed property.
    @State private var didAutoExpand: Bool

    @Environment(\.dynamicTypeSize) private var typeSize
    /// The shared width every tool label on this screen is drawn into, so the
    /// commands beside them share one left edge. See `CCToolColumn`.
    @Environment(\.ccToolColumn) private var toolColumn

    init(tool: ToolItem) {
        self.tool = tool
        let failed = tool.status == .failed
        _expanded = State(initialValue: failed)
        _didAutoExpand = State(initialValue: failed)
    }

    private var hasDetail: Bool {
        (tool.input?.isNull == false) || !(tool.output ?? "").isEmpty || tool.isTruncated
    }

    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            Button {
                guard hasDetail else { return }
                withAnimation(CC.motion.small) { expanded.toggle() }
            } label: {
                summaryLine
            }
            .buttonStyle(CCToolRowStyle())
            .allowsHitTesting(hasDetail)

            if expanded {
                detail
                    // Inset to the content column, so an expanded payload lines
                    // up with the tool name rather than with its glyph.
                    .padding(.leading, TimelineSpine.content)
            }
        }
        // A failed call keeps a 2pt bar down its whole gutter for as long as it
        // is on screen — the one place a tool row is allowed a colour. Drawn as
        // an overlay rather than as a layout child so that a failure does not
        // shunt its own row sideways: the glyph stays exactly where it sits on
        // every other row.
        .overlay(alignment: .leading) {
            if tool.status == .failed {
                Rectangle()
                    .fill(CC.color.danger)
                    .frame(width: 2)
                    .accessibilityHidden(true)
            }
        }
        .onChange(of: tool.status) { _, status in
            // Auto-expansion happens once, on arrival. A reader who collapses it
            // stays collapsed.
            guard status == .failed, !didAutoExpand else { return }
            didAutoExpand = true
            withAnimation(CC.motion.small) { expanded = true }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(accessibilityLabel)
        .accessibilityHint(
            hasDetail ? "Double tap to \(expanded ? "collapse" : "expand") this tool call" : "")
    }

    private var summaryLine: some View {
        CCAdaptiveStack(horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xxs) {
            HStack(spacing: TimelineSpine.gap) {
                CCIcon(ToolSummary.symbol(tool: tool.name), size: CC.size.icon, weight: .medium)
                    .foregroundStyle(CC.text.tertiary)
                    // Fixed to the gutter column, because `CCIcon` sizes its own
                    // frame from the glyph and a column of tool rows has to line
                    // up whatever symbols it happens to hold.
                    .frame(width: TimelineSpine.gutter, alignment: .trailing)
                Text(tool.name)
                    .ccType(CC.type.callout)
                    .foregroundStyle(CC.text.primary)
                    .lineLimit(1)
                    // The glyph above is already pinned to the gutter so a
                    // column of tool rows lines up; this is the same argument
                    // applied to the name, so the commands beside them line up
                    // too. See `CCToolColumn`.
                    .ccToolLabelColumn(toolColumn, disabled: typeSize.isAccessibilitySize)
                if let argument = tool.argument {
                    Text(argument.firstLine)
                        .ccType(CC.type.monoSmall)
                        .foregroundStyle(CC.text.secondary)
                        .lineLimit(typeSize.isAccessibilitySize ? 2 : 1)
                        .truncationMode(.middle)
                }
                Spacer(minLength: CC.space.xxs)
            }
            statusView
        }
        // **36 is the band; 44 is the target.** `DESIGN-SCREENS.md` specifies
        // this row as "one semantic line, 36pt tall, 44pt hit area" and the
        // same document's rule is "44×44pt minimum, no exceptions" — the 36 was
        // built and the 44 was not, which made this the only tappable thing in
        // the app under the floor. Measured: 370×36, `hittable=Y`.
        //
        // It escaped because this row hand-rolls a `Button` rather than going
        // through `CCButton`, whose `init` enforces the rule. `CCDisclosure` is
        // the same three-modifier chain done correctly, and its own doc comment
        // names these very rows as its use case.
        //
        // Conditional because a row with nothing to expand is not a control —
        // `allowsHitTesting(hasDetail)` above already says so — and this kit
        // pays density "only where a control exists".
        .frame(minHeight: hasDetail ? CC.size.hitTarget : CC.size.controlSm)
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
    }

    @ViewBuilder
    private var detail: some View {
        if let input = tool.input, !input.isNull {
            CCMonoBlock(input.prettyJSONString, lineLimit: 40)
        }
        if let output = tool.output, !output.isEmpty {
            CCMonoBlock(
                output, lineLimit: 40, tone: tool.status == .failed ? .danger : .neutral)
        }
        if tool.isTruncated {
            // Verbatim. This sentence is the honest description of a payload cap
            // and it must survive every redesign.
            Text("The daemon truncated this payload; the fact is complete, the bulk is not.")
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.color.warning)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var statusView: some View {
        HStack(spacing: CC.space.xs) {
            if let duration = tool.durationMS {
                Text(Format.duration(ms: duration))
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
            }
            CCIcon(tool.status.symbol, size: 12, weight: .semibold, relativeTo: .caption)
                .foregroundStyle(
                    tool.status.ccTone == .neutral
                        ? (tool.status == .running ? CC.text.primary : CC.text.tertiary)
                        : tool.status.ccTone.color)
        }
    }

    private var accessibilityLabel: String {
        var parts = ["\(tool.name) tool call"]
        if let argument = tool.argument { parts.append(argument.firstLine) }
        parts.append(tool.status.label)
        if let duration = tool.durationMS { parts.append("took \(Format.duration(ms: duration))") }
        return parts.joined(separator: ", ")
    }
}

private struct CCToolRowStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(configuration.isPressed ? CC.color.surfaceRaised : Color.clear)
            .ccAnimation(CC.motion.micro, value: configuration.isPressed)
    }
}

// MARK: - Approvals

/// An approval is a document, not a dialog — this row is the *preview* of that
/// document, and it never becomes the tap target for the decision itself.
///
/// One card fill plus a coloured hairline, never an orange-tinted background:
/// fills are for the Decision Card's action bar, not for list rows. The card's
/// own surface is `CCCard`'s single construction — against the timeline's `bg`
/// ground it already reads as raised, and a second fill invented for one
/// component is how a kit ends up with two ways to draw one object.
struct ApprovalRow: View {
    let approval: ApprovalItem
    let risk: RiskClass
    let onOpen: () -> Void
    @Environment(\.dynamicTypeSize) private var typeSize
    /// See `CCToolColumn` — one left edge for every command on this screen.
    @Environment(\.ccToolColumn) private var toolColumn

    /// Mirrors `CCStatusDot`'s own ramp and ceiling, so the gutter the dot hangs
    /// in is always exactly the dot plus one 8pt gap.
    @ScaledMetric(relativeTo: .footnote) private var scaledDot: CGFloat =
        CCStatusDot.Size.cardHeader.rawValue

    /// **This row's own clock, ticking at the rate this row can actually show.**
    ///
    /// The same mechanism the fleet's rows use, for the same reason — see
    /// `FleetRowView.now`. Both of the rules that make it safe live in
    /// `AgeTick.renderTime`: the tick has to be *read* during render or nothing
    /// invalidates, and what is rendered is the wall clock rather than the stamp
    /// this row fell asleep holding.
    @State private var lastTick = Date()

    private var now: Date { AgeTick.renderTime(lastTick: lastTick) }

    /// The one age this row draws, and the resolution it draws it at.
    ///
    /// A **pending** card is the timeline's one honest per-second tick:
    /// `CCWaitClock` prints seconds all the way to an hour, and how long an
    /// agent has been held is the number this row exists to carry. A
    /// **resolved** card prints a coarsening age, so it sleeps until that age
    /// stops being true — an hour at a time, once it is hours old.
    private var clock: AgeClock {
        if let outcome = approval.outcome {
            return AgeClock(since: outcome.resolvedDate, scale: .age)
        }
        // A Codex resolution carries no timestamp on any arm, so a card that
        // ended that way has no age to tick — and the coarse scale is what stops
        // a settled row waking the screen once a second for a clock it does not
        // draw.
        if approval.codexResolution != nil {
            return AgeClock(since: approval.requestedAt, scale: .age)
        }
        return AgeClock(since: approval.requestedAt, scale: .clock)
    }

    var body: some View {
        CCCard(
            border: approval.isPending ? CC.color.warning.opacity(0.45) : CC.color.border
        ) {
            VStack(alignment: .leading, spacing: CC.space.sm) {
                headerLine
                summaryLine
                footer
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(accessibilityLabel)
        .accessibilityAddTraits(approval.isPending ? [.isButton] : [])
        .accessibilityAction { if approval.isPending { onOpen() } }
        // Restarted whenever the timestamp this row counts from changes — a card
        // being answered moves it from `requestedAt` to `resolvedDate` and from
        // seconds to a coarsening age — because the old deadline was computed
        // from the old anchor and the old resolution.
        .task(id: clock) { await AgeTick.follow(clock) { lastTick = $0 } }
    }

    /// The dot **hangs in the gutter**, the way `CCSectionHeader`'s does: drawn
    /// as an overlay and offset out of the leading edge, so it takes no layout
    /// width at all.
    ///
    /// Inline, it pushed `NEEDS YOU` to 49 while the two lines under it sat on
    /// 32 — two text edges inside one card, which is the misalignment the whole
    /// app is arranged to avoid, in miniature. Now the card runs one gutter (32,
    /// the dot) and one content column (52, everything written), the same two
    /// the fleet's rows use.
    private var headerLine: some View {
        CCAdaptiveStack(horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xs) {
            Text(Self.headerTitle(for: approval))
                .ccType(CC.type.micro.weight(.semibold))
                .foregroundStyle(approval.isPending ? CC.color.warning : CC.text.tertiary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .overlay(alignment: .leading) {
                    CCStatusDot(
                        color: approval.isPending ? CC.color.warning : CC.text.tertiary,
                        size: CCStatusDot.Size.cardHeader.rawValue,
                        pulses: approval.isPending)
                    .offset(x: -dotGutter)
                    .accessibilityHidden(true)
                }
            CCBadge(risk: risk)
        }
    }

    /// The offset scales with the dot, for the reason `CCSectionHeader` states
    /// in the same words: fixed at 20 while the disc grew to 14 at AX5, the two
    /// closed to a 6pt gap and the dot read as a bullet glued to the N of
    /// NEEDS YOU.
    private var dotGutter: CGFloat {
        min(scaledDot, CCStatusDot.Size.cardHeader.rawValue * CC.size.dotMaxScale) + CC.space.xs
    }

    private var summaryLine: some View {
        // Adaptive, like the fleet row's activity line: at AX5 a `callout` tool
        // name and a command cannot share 340pt, and side by side the command
        // wrapped into a column half the card wide.
        CCAdaptiveStack(
            horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xxs,
            verticalAlignment: .firstTextBaseline
        ) {
            Text(approval.card.toolName)
                .ccType(CC.type.callout)
                .foregroundStyle(CC.text.primary)
                .ccToolLabelColumn(toolColumn, disabled: typeSize.isAccessibilitySize)
                .layoutPriority(1)
            if let argument = ToolSummary.principalArgument(
                tool: approval.card.toolName, input: approval.card.toolInput)
            {
                // The kit's container-less mono run, shared with the fleet row
                // and the Deck's accessory bar. It wraps rather than truncating
                // at accessibility sizes: measured at AX5, cut to one line this
                // read `git…main`, which is two halves of a command and not a
                // fact — on the row whose whole job is to say what is waiting.
                CCMonoBlock(inline: argument.firstLine)
            }
            Spacer(minLength: 0)
        }
    }

    @ViewBuilder
    private var footer: some View {
        if let outcome = approval.outcome {
            resolvedFooter(outcome)
        } else if let resolution = approval.codexResolution {
            codexResolvedFooter(resolution)
        } else {
            CCAdaptiveStack(horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xs) {
                CCWaitClock(since: approval.requestedAt, now: now, prefix: "waiting")
                Spacer(minLength: CC.space.xs)
                CCButton("Review", variant: .primary, size: .sm, action: onOpen)
            }
        }
    }

    @ViewBuilder
    private func resolvedFooter(_ outcome: AnswerOutcome) -> some View {
        CCAdaptiveStack(horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xs) {
            Text(Self.resolutionText(for: outcome))
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: CC.space.xs)
            Text(Format.age(since: outcome.resolvedDate, now: now))
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
            CCButton("View", variant: .ghost, size: .sm, action: onOpen)
        }
    }

    /// **What a Codex card ended as**, in its own vocabulary.
    ///
    /// A separate footer rather than a mapping onto `AnswerOutcome`, for the
    /// same reason `ApprovalItem` keeps the two apart: a `cleared(turn_aborted)`
    /// is not "Denied", an `answered(by: .local)` names no decision at all, and
    /// `AnswerOutcome` cannot express either without claiming something.
    ///
    /// No timestamp. The Codex wire carries a `resolved_at` on **no arm at all**
    /// — `AnswerOutcome`'s is what the age beside a Claude row is drawn from —
    /// and an age computed from when this phone happened to receive the frame
    /// would be a number about the phone dressed as a number about the Mac.
    @ViewBuilder
    private func codexResolvedFooter(_ resolution: CodexResolution) -> some View {
        CCAdaptiveStack(horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xs) {
            Text(CodexProse.resolution(resolution).title)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: CC.space.xs)
            CCButton("View", variant: .ghost, size: .sm, action: onOpen)
        }
    }

    /// The persisted approval row's header word. Pending is "NEEDS YOU";
    /// otherwise the row carries a recorded outcome, and an `indeterminate` one —
    /// which the daemon could not confirm reached the agent — reads "UNCONFIRMED",
    /// never the definitive "RESOLVED". Static and pure so the string is testable
    /// off the real `ApprovalItem` the builder produces, without rendering the row.
    ///
    /// **A Codex ending counts.** Read against `approval.outcome` alone this row
    /// said `NEEDS YOU` — with a live `Review` button and a ticking wait clock —
    /// on a card that had already been answered at the Mac. Caught by looking at
    /// `codex-resolved-at-mac--L.png`; the builder's own tests were green,
    /// because the defect was in a second reader that had not been told.
    static func headerTitle(for approval: ApprovalItem) -> String {
        if let outcome = approval.outcome {
            return outcome.indeterminate ? "UNCONFIRMED" : "RESOLVED"
        }
        if let resolution = approval.codexResolution { return Self.codexHeader(for: resolution) }
        return "NEEDS YOU"
    }

    /// One word for how a Codex card ended. `RETIRED` rather than `RESOLVED` for
    /// the endings nobody answered: a question that timed out or went away with
    /// its turn was not resolved by anyone, and saying so would credit a
    /// decision that was never made.
    static func codexHeader(for resolution: CodexResolution) -> String {
        switch resolution {
        case .answered: return "RESOLVED"
        case .cleared, .timeout: return "RETIRED"
        case .unknown, .unrecognisedStatus: return "UNCONFIRMED"
        }
    }

    static func resolutionText(for outcome: AnswerOutcome) -> String {
        // The daemon accepted the answer but never confirmed it reached the
        // agent, so the footer must not state a decision or an actor as fact.
        if outcome.indeterminate { return "Answer not confirmed" }
        // An inferred decision is the daemon noticing the prompt is gone, not
        // watching an answer happen. Rendering it as "Allowed" would put a fact
        // on screen that nobody observed.
        if outcome.inferred { return "Answered at the keyboard" }
        let who: String
        switch outcome.resolvedBy {
        case .phone: who = "from this app"
        case .local: who = "at the keyboard"
        case .timeout: who = "by timeout - nobody answered"
        case .superseded: who = "superseded"
        }
        return "\(outcome.decision.label) \(who)"
    }

    /// The spoken label for a row carrying a recorded outcome. An unconfirmed
    /// outcome is announced as such — never "Resolved approval", which would
    /// speak a confirmation VoiceOver users cannot see is false.
    static func resolvedAccessibilityLabel(for outcome: AnswerOutcome, toolName: String) -> String {
        let lead = outcome.indeterminate ? "Unconfirmed approval" : "Resolved approval"
        return "\(lead). \(resolutionText(for: outcome)). \(toolName)."
    }

    private var accessibilityLabel: String {
        if let outcome = approval.outcome {
            return Self.resolvedAccessibilityLabel(for: outcome, toolName: approval.card.toolName)
        }
        if let resolution = approval.codexResolution {
            let banner = CodexProse.resolution(resolution)
            return "\(banner.oneLine) \(approval.card.toolName)."
        }
        return
            "Pending approval, risk \(risk.label), \(approval.card.toolName), waiting \(Format.spokenAge(now.timeIntervalSince(approval.requestedAt)))"
    }
}

// MARK: - Notices

struct NoticeRow: View {
    let notice: NoticeItem
    let date: Date

    var body: some View {
        HStack(alignment: .top, spacing: TimelineSpine.gap) {
            CCIcon(notice.symbol, size: CC.size.icon, weight: .medium)
                .foregroundStyle(tone == .neutral ? CC.text.tertiary : tone.color)
                .frame(width: TimelineSpine.gutter, alignment: .trailing)
                .padding(.top, 1)

            VStack(alignment: .leading, spacing: CC.space.xxs) {
                Text(notice.title)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.primary)
                    .fixedSize(horizontal: false, vertical: true)
                if let detail = notice.detail, !detail.isEmpty {
                    // Inline-parsed like the prose it previews — asterisks in a
                    // summary are the same artifact as asterisks in the body.
                    // Never clamped: a notice's detail is the whole of what the
                    // reader gets. (Turn-complete used to preview the agent's
                    // message here, clamped, directly above the message row —
                    // which read as a duplicate; its detail is now nil at the
                    // builder, so the boundary is a boundary.)
                    Text(AgentProse.inline(detail))
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            Text(date, format: .dateTime.hour().minute())
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
        }
        .frame(minHeight: 28)
        .accessibilityElement(children: .combine)
    }

    private var tone: CCTone { notice.severity.ccTone }
}
