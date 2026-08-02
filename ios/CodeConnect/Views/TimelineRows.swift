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
    let onOpenApproval: (ApprovalItem) -> Void

    var body: some View {
        switch item.content {
        case .userMessage(let text):
            UserMessageRow(text: text, date: item.date)
        case .agentMessage(let text):
            AgentMessageRow(text: text)
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
                    .ccType(CC.type.callout)
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
    @State private var expanded = false

    private var isLong: Bool { text.count > 280 || text.split(separator: "\n").count > 4 }

    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            Text(text)
                .ccType(CC.type.body)
                .foregroundStyle(CC.text.primary)
                .textSelection(.enabled)
                .lineLimit(expanded ? nil : 4)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
            if isLong {
                CCButton(expanded ? "Show less" : "Show more", variant: .ghost, size: .sm) {
                    withAnimation(CC.motion.small) { expanded.toggle() }
                }
                .padding(.leading, -CC.space.sm)
            }
        }
        // **No container is not the same as no column.** The agent's prose is
        // the ground of the timeline and draws nothing around itself, which is
        // right — but it was also the only run on the screen still starting on
        // the list's 16pt margin, measured at x=17.33 against the 52.67 that the
        // notice rows, the identity block and the user's own message hold
        // perfectly. It has no glyph to hang in the gutter, so it pays the same
        // step the gutter would have cost it.
        .padding(.leading, TimelineSpine.content)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Agent said: \(text)")
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
            Text(approval.isPending ? "NEEDS YOU" : "RESOLVED")
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
            Text(resolutionText(outcome))
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

    private func resolutionText(_ outcome: AnswerOutcome) -> String {
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

    private var accessibilityLabel: String {
        if let outcome = approval.outcome {
            return "Resolved approval. \(resolutionText(outcome)). \(approval.card.toolName)."
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
                    Text(detail)
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                        .lineLimit(3)
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
