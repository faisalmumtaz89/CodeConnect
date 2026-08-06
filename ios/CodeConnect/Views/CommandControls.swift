import SwiftUI

// =============================================================================
//  Slash commands, from a phone.
//
//  Typing `/` in the composer summons a palette of what this app can do with
//  a command natively — sheets for `/model`, `/effort` and `/compact`, a
//  confirmation for `/clear`, the app's own diff for `/diff`, and — when the
//  daemon can recover the Mac's composer — a captured snapshot of the views
//  `/status`, `/usage` and `/cost` open. The palette never lists a row whose
//  only behavior would be refusing the tap: rows the daemon cannot honour are
//  omitted, not disabled. The send-time policy underneath is unchanged:
//  typed native commands open their controls, typed dialog built-ins get the
//  composer's refusal note, and anything unknown passes through as the
//  custom skill or plain text it is — with the supervisor's recovery check
//  standing behind every word-shaped send.
// =============================================================================

/// One palette row: the command, what tapping it opens, and the action the
/// detail view routes — the same `CommandAction` a typed send resolves to,
/// so a tap and a typed command can never drift apart.
struct PaletteRow: Equatable, Identifiable {
    let command: String
    let subtitle: String
    let icon: String
    let action: CommandAction

    var id: String { command }
    var title: String { "/\(command)" }
}

/// The palette above the keyboard while the composer holds a bare `/` or a
/// prefix of a native command. Anything else — arguments, unknown fragments —
/// shows no palette at all; typing simply continues.
struct CommandPalette: View {
    /// What the palette shows for one composer text: which rows, and whether
    /// the discovery caption underneath. `nil` means no palette.
    struct Content: Equatable {
        var rows: [PaletteRow]
        var showsCaption: Bool
    }

    let content: Content
    let onAction: (CommandAction) -> Void

    /// The eight native commands, in the order they are worth a thumb's
    /// attention: the two levers, the two context acts, the diff, then the
    /// three Mac captures.
    static let allRows: [PaletteRow] = [
        PaletteRow(
            command: "model", subtitle: "Choose model", icon: "cpu",
            action: .nativeModel(prefillArgs: "")),
        PaletteRow(
            command: "effort", subtitle: "Choose reasoning effort", icon: "dial.medium",
            action: .nativeEffort),
        PaletteRow(
            command: "compact", subtitle: "Compact context",
            icon: "arrow.down.right.and.arrow.up.left",
            action: .nativeCompact(prefillInstructions: "")),
        PaletteRow(
            command: "clear", subtitle: "Clear Claude’s context", icon: "eraser",
            action: .nativeClear),
        PaletteRow(
            command: "diff", subtitle: "Review working-tree changes",
            icon: "plus.forwardslash.minus",
            action: .nativeDiff),
        PaletteRow(
            command: "status", subtitle: "Capture the Mac status view", icon: "info.circle",
            action: .nativeSnapshot(.status)),
        PaletteRow(
            command: "usage", subtitle: "Capture the Mac usage view", icon: "chart.bar",
            action: .nativeSnapshot(.usage)),
        PaletteRow(
            command: "cost", subtitle: "Capture the Mac cost view", icon: "creditcard",
            action: .nativeSnapshot(.cost)),
    ]

    /// The palette for one composer text, or `nil` for none. Bare `/` is the
    /// discovery moment: every available row plus the caption. A fragment
    /// filters case-insensitively. Whitespace after the fragment means
    /// arguments are coming, and arguments are typing, not browsing. The
    /// snapshot rows exist only when the daemon can close the Mac view they
    /// open — omitted, never disabled.
    static func content(for typed: String, recoversComposer: Bool) -> Content? {
        let afterLeading = typed.drop(while: \.isWhitespace)
        guard afterLeading.hasPrefix("/") else { return nil }
        let fragment = afterLeading.dropFirst()
        let available = allRows.filter { row in
            if case .nativeSnapshot = row.action { return recoversComposer }
            return true
        }
        if fragment.isEmpty {
            return Content(rows: available, showsCaption: true)
        }
        guard !fragment.contains(where: \.isWhitespace) else { return nil }
        let needle = fragment.lowercased()
        let matches = available.filter { $0.command.hasPrefix(needle) }
        return matches.isEmpty ? nil : Content(rows: matches, showsCaption: false)
    }

    /// One standard row's height, in the reader's type size — the unit the
    /// scroll cap is measured in.
    @ScaledMetric(relativeTo: .body) private var rowUnit: CGFloat = 52

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ScrollView {
                VStack(spacing: 0) {
                    ForEach(content.rows) { row in
                        CCRow(
                            row.title,
                            titleIsMono: true,
                            subtitle: row.subtitle,
                            separator: row != content.rows.last || content.showsCaption,
                            action: { onAction(row.action) }
                        ) {
                            CCIcon(row.icon, size: 16, weight: .regular, relativeTo: .body)
                                .foregroundStyle(CC.text.tertiary)
                        } trailing: {
                            EmptyView()
                        }
                    }
                    // **Inside the scroll, deliberately.** Pinned below it, the
                    // caption competed with the rows for a fixed height: at AX5
                    // it took the space of two of them and still truncated
                    // itself mid-word, and at reading size it sat flush against
                    // a half-clipped row so the two read as one broken element.
                    // Here it costs no row its place, it wraps instead of
                    // truncating, and it arrives exactly when a reader who has
                    // scanned the whole list starts wondering where the rest of
                    // the commands went.
                    if content.showsCaption {
                        Text("Run other commands on the Mac from the Terminal tab.")
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.text.tertiary)
                            .fixedSize(horizontal: false, vertical: true)
                            .padding(.horizontal, CC.space.sm)
                            .padding(.vertical, CC.space.sm)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            }
            .scrollBounceBehavior(.basedOnSize)
            // Roughly four standard rows, then the list scrolls — the
            // fractional cap keeps part of a row in view, which is the only
            // thing telling a reader there is more. Bounded so the largest
            // accessibility sizes still leave the composer on screen and
            // scroll the list instead of eating the keyboard.
            .frame(maxHeight: min(rowUnit * 4.4, 340))
        }
        .background(CC.color.surfaceOverlay)
        .clipShape(RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous)
                .strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline)
        }
        .padding(.horizontal, CC.space.md)
        .padding(.top, CC.space.xs)
    }
}

/// The native `/model` control: tap a row, it applies. The Mac's own picker
/// works the same way, the operation is trivially reversible from this same
/// sheet, and stating the consequence *above* the rows means consent precedes
/// the tap — which is what let the confirm-button dance, and its
/// disabled-at-rest scold, be deleted.
struct ModelSheet: View {
    let sessionKey: String
    var prefill: String = ""
    /// The keystrokes landed on the Mac — the composer draft that opened
    /// this sheet has been consumed and may be cleared.
    var onLanded: () -> Void = {}

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    /// The selectable ships, by alias — the argument `/model` accepts.
    private static let aliases = ["fable", "opus", "sonnet", "haiku"]

    private enum Phase: Equatable {
        case idle
        /// The alias being typed on the Mac right now.
        case sending(String)
        /// Keystrokes landed; Claude Code has not yet confirmed in the
        /// transcript.
        case waiting(String)
        case confirmed(String)
        case failed(String)
    }

    @State private var phase: Phase = .idle
    @State private var custom: String = ""
    @State private var timeoutTask: Task<Void, Never>?
    /// The model fact as it stood when a row was tapped; "confirmed" means a
    /// NEW command-confirmation fact relative to this.
    @State private var baseline: ConfirmedModel?

    private var state: SessionState? { model.states[sessionKey] }

    private var busy: Bool {
        switch phase {
        case .sending, .waiting: return true
        case .idle, .confirmed, .failed: return false
        }
    }

    private var current: ModelDisplay? {
        state?.lastConfirmedModel.map { ModelDisplay.from($0.name) }
    }

    var body: some View {
        CCSheetChrome("Model", onClose: { dismiss() }) {
            ScrollView {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    CCSectionHeader("Current model")
                    currentCard
                    CCSectionHeader("Choose model")
                    Text("Also becomes your default for new sessions.")
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                    chooserCard
                    customEntry
                    Text("For this session only, use the model picker in the Terminal tab.")
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.tertiary)
                        .fixedSize(horizontal: false, vertical: true)
                    statusSlot
                }
                .padding(CC.space.md)
            }
            .scrollBounceBehavior(.basedOnSize)
        }
        .onAppear {
            if custom.isEmpty, !prefill.isEmpty { custom = prefill }
        }
        .onChange(of: state?.lastConfirmedModel) { _, currentFact in
            guard case .waiting = phase,
                let name = ModelChangeWatch.confirmed(baseline: baseline, current: currentFact)
            else { return }
            timeoutTask?.cancel()
            let display = ModelDisplay.from(name).name
            withAnimation(CC.motion.small) { phase = .confirmed("Model set to \(display).") }
        }
        .onDisappear { timeoutTask?.cancel() }
    }

    // MARK: Current

    private var currentCard: some View {
        CCCard(padding: 0) {
            if let fact = state?.lastConfirmedModel, let current {
                CCRow(
                    current.name,
                    titleIsMono: current.isRawFallback,
                    subtitle: current.meta,
                    meta: Self.provenanceLine(fact, now: Date()),
                    showsChevron: false,
                    separator: false
                ) {
                    CCIcon(
                        "checkmark.circle.fill", size: 16, weight: .semibold,
                        relativeTo: .body
                    )
                    .foregroundStyle(CCTone.success.color)
                } trailing: {
                    EmptyView()
                }
            } else {
                CCRow(
                    "Not confirmed",
                    meta: "This session has not reported a model.",
                    showsChevron: false,
                    separator: false
                ) {
                    CCIcon("circle.dashed", size: 16, weight: .regular, relativeTo: .body)
                        .foregroundStyle(CC.text.tertiary)
                } trailing: {
                    EmptyView()
                }
            }
        }
    }

    /// `Confirmed at session start · 1m ago` — one line, one place.
    static func provenanceLine(_ fact: ConfirmedModel, now: Date) -> String {
        let origin =
            fact.source == "Command confirmation"
            ? "Confirmed by /model" : "Confirmed at session start"
        return "\(origin) · \(RelativeAge.text(since: fact.at, now: now))"
    }

    // MARK: Chooser

    private var chooserCard: some View {
        CCCard(padding: 0) {
            VStack(spacing: 0) {
                ForEach(Self.aliases, id: \.self) { alias in
                    aliasRow(alias)
                }
            }
        }
        .accessibilityValue(busy ? "Busy" : "")
    }

    private func aliasRow(_ alias: String) -> some View {
        let display = ModelDisplay.from(alias)
        let isCurrent = current.map { $0.name == display.name } ?? false
        let isApplying: Bool = {
            if case .sending(let active) = phase { return active == alias }
            if case .waiting(let active) = phase { return active == alias }
            return false
        }()
        return CCRow(
            display.name,
            showsChevron: false,
            separator: alias != Self.aliases.last,
            action: (busy || isCurrent) ? nil : { apply(alias) }
        ) {
            if isApplying {
                CCProgressRing(.sm)
            } else {
                CCIcon(
                    isCurrent ? "checkmark.circle.fill" : "circle",
                    size: 16, weight: isCurrent ? .semibold : .regular, relativeTo: .body
                )
                .foregroundStyle(isCurrent ? CCTone.success.color : CC.text.tertiary)
            }
        } trailing: {
            EmptyView()
        } meta: {
            EmptyView()
        }
    }

    // MARK: Custom entry

    private var customEntry: some View {
        // Carded for the same reason as the Compact sheet's field: outside
        // one, the label lands on the content column and its own box on the
        // container's edge, 36pt apart.
        CCCard {
            HStack(alignment: .bottom, spacing: CC.space.xs) {
                CCField(
                    label: "Model id or alias", text: $custom,
                    placeholder: "e.g. claude-sonnet-5",
                    submitLabel: .done,
                    autocapitalization: .never,
                    disableAutocorrection: true,
                    isMono: true,
                    onSubmit: { applyCustom() })
                if !custom.trimmingCharacters(in: .whitespaces).isEmpty {
                    CCButton("Set", variant: .secondary, size: .sm) { applyCustom() }
                }
            }
        }
    }

    private func applyCustom() {
        let trimmed = custom.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !busy else { return }
        apply(trimmed)
    }

    // MARK: Status

    @ViewBuilder
    private var statusSlot: some View {
        switch phase {
        case .idle:
            EmptyView()
        case .sending(let alias):
            statusLine("Typing /model \(alias) on the Mac…", tone: .neutral)
        case .waiting:
            statusLine("Waiting for Claude Code to confirm…", tone: .neutral)
        case .confirmed(let line):
            statusLine(line, tone: .success)
        case .failed(let line):
            statusLine(line, tone: .warning)
        }
    }

    private func statusLine(_ text: String, tone: CCTone) -> some View {
        CCProse(
            text, style: CC.type.footnote,
            color: tone == .neutral ? CC.text.secondary : tone.color
        )
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    // MARK: Apply

    private func apply(_ alias: String) {
        baseline = state?.lastConfirmedModel
        withAnimation(CC.motion.micro) { phase = .sending(alias) }
        Task {
            let attempt = await model.sendModelCommand(alias, to: sessionKey)
            switch attempt {
            case .sent:
                // Keystrokes landed — which proves typing, not execution.
                // Claude Code's own transcript confirmation flips this to
                // done, and it may already have landed while `.sending`.
                onLanded()
                if let name = ModelChangeWatch.confirmed(
                    baseline: baseline, current: state?.lastConfirmedModel)
                {
                    phase = .confirmed("Model set to \(ModelDisplay.from(name).name).")
                } else {
                    phase = .waiting(alias)
                    armTimeout()
                }
            case .alreadyApplied:
                // The daemon replayed an earlier attempt without typing; no
                // new transcript line is coming. It proves the keys were
                // typed once — never that Claude acted on them — so this says
                // "sent", exactly as the Effort and Compact sheets do.
                onLanded()
                phase = .confirmed("This model request was already sent earlier.")
            case .refused(let reason):
                phase = .failed("Nothing was typed: \(reason)")
            case .failed(let reason):
                phase = .failed("Couldn’t type the command: \(reason)")
            case .indeterminate(let reason):
                phase = .failed(
                    "Not confirmed: \(reason) Retry is safe; the command won’t be typed twice.")
            case .composerRecovered:
                // `/model <alias>` is measured inline, so this is not the
                // expected path — but the keys landed, and the transcript
                // confirmation is still what proves the change.
                onLanded()
                phase = .waiting(alias)
                armTimeout()
            case .composerLost:
                phase = .failed(
                    "Couldn’t restore the composer. Open Terminal to recover.")
            }
        }
    }

    private func armTimeout() {
        timeoutTask?.cancel()
        timeoutTask = Task {
            try? await Task.sleep(for: .seconds(5))
            guard !Task.isCancelled else { return }
            if case .waiting = phase {
                withAnimation(CC.motion.small) {
                    phase = .failed(
                        "Not confirmed yet. The change may still have run; "
                            + "check the timeline or Terminal.")
                }
            }
        }
    }
}

/// The native `/effort` control: five direct-apply rows and no current-state
/// marker — Claude Code does not report the standing level, and this app
/// does not present what it cannot know. The measured machine values
/// (`xhigh`, `max`) appear only in command receipts; the rows wear the
/// human labels. `ultracode` and `auto` are deliberately typing-only: both
/// pass through the composer unimpeded, but neither has earned a row —
/// one is a token-expensive power mode, the other's semantics are unmeasured.
struct EffortSheet: View {
    let sessionKey: String
    var onLanded: () -> Void = {}

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    /// The five ruled levels, machine value and human label.
    private static let levels: [(value: String, label: String)] = [
        ("low", "Low"),
        ("medium", "Medium"),
        ("high", "High"),
        ("xhigh", "Extra high"),
        ("max", "Maximum"),
    ]

    private enum Phase: Equatable {
        case idle
        case sending(String)
        /// Keystrokes landed; watching for Claude Code's own
        /// "Set effort level to …" line.
        case waiting(String)
        case confirmed(String)
        case failed(String)
    }

    @State private var phase: Phase = .idle
    @State private var timeoutTask: Task<Void, Never>?
    /// The effort fact as it stood when a row was tapped; "confirmed" means
    /// a NEW fact relative to this.
    @State private var baseline: ConfirmedEffort?

    private var state: SessionState? { model.states[sessionKey] }

    private var busy: Bool {
        switch phase {
        case .sending, .waiting: return true
        case .idle, .confirmed, .failed: return false
        }
    }

    var body: some View {
        CCSheetChrome("Effort", onClose: { dismiss() }) {
            ScrollView {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    CCSectionHeader("Choose effort")
                    Text(
                        "CodeConnect reports only what Claude Code confirms; "
                            + "it does not assume how long the choice lasts."
                    )
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    chooserCard
                    statusSlot
                }
                .padding(CC.space.md)
            }
            .scrollBounceBehavior(.basedOnSize)
        }
        .onChange(of: state?.lastConfirmedEffort) { _, fact in
            guard case .waiting = phase, let fact, fact != baseline else { return }
            timeoutTask?.cancel()
            let label = EffortConfirmation.label(for: fact.value)
            withAnimation(CC.motion.small) {
                phase = .confirmed("Claude Code confirmed \(label) effort.")
            }
        }
        .onDisappear { timeoutTask?.cancel() }
    }

    private var chooserCard: some View {
        CCCard(padding: 0) {
            VStack(spacing: 0) {
                ForEach(Self.levels, id: \.value) { level in
                    levelRow(level.value, label: level.label)
                }
            }
        }
        .accessibilityValue(busy ? "Busy" : "")
    }

    private func levelRow(_ value: String, label: String) -> some View {
        let isApplying: Bool = {
            if case .sending(let active) = phase { return active == value }
            if case .waiting(let active) = phase { return active == value }
            return false
        }()
        return CCRow(
            label,
            showsChevron: false,
            separator: value != Self.levels.last?.value,
            action: busy ? nil : { apply(value) }
        ) {
            if isApplying {
                CCProgressRing(.sm)
            } else {
                CCIcon("circle", size: 16, weight: .regular, relativeTo: .body)
                    .foregroundStyle(CC.text.tertiary)
            }
        } trailing: {
            EmptyView()
        } meta: {
            EmptyView()
        }
    }

    @ViewBuilder
    private var statusSlot: some View {
        switch phase {
        case .idle:
            EmptyView()
        case .sending(let value):
            effortStatusLine("Typing /effort \(value) on the Mac…", tone: .neutral)
        case .waiting:
            effortStatusLine("Waiting for Claude Code to confirm…", tone: .neutral)
        case .confirmed(let line):
            effortStatusLine(line, tone: .success)
        case .failed(let line):
            effortStatusLine(line, tone: .warning)
        }
    }

    private func effortStatusLine(_ text: String, tone: CCTone) -> some View {
        CCProse(
            text, style: CC.type.footnote,
            color: tone == .neutral ? CC.text.secondary : tone.color
        )
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func apply(_ value: String) {
        baseline = state?.lastConfirmedEffort
        withAnimation(CC.motion.micro) { phase = .sending(value) }
        Task {
            let attempt = await model.sendEffortCommand(value, to: sessionKey)
            switch attempt {
            case .sent:
                onLanded()
                // The confirmation may already have landed while `.sending`.
                if let fact = state?.lastConfirmedEffort, fact != baseline {
                    let label = EffortConfirmation.label(for: fact.value)
                    phase = .confirmed("Claude Code confirmed \(label) effort.")
                } else {
                    phase = .waiting(value)
                    armTimeout(value)
                }
            case .alreadyApplied:
                onLanded()
                phase = .confirmed("This effort request was already sent earlier.")
            case .refused(let reason):
                phase = .failed("Nothing was typed: \(reason)")
            case .failed(let reason):
                phase = .failed("Couldn’t type the command: \(reason)")
            case .indeterminate(let reason):
                phase = .failed(
                    "Not confirmed: \(reason) Retry is safe; the command won’t be typed twice.")
            case .composerRecovered:
                // Measured inline, so not the expected path — but the keys
                // landed, and the transcript still proves the change.
                onLanded()
                phase = .waiting(value)
                armTimeout(value)
            case .composerLost:
                phase = .failed("Couldn’t restore the composer. Open Terminal to recover.")
            }
        }
    }

    private func armTimeout(_ value: String) {
        timeoutTask?.cancel()
        timeoutTask = Task {
            try? await Task.sleep(for: .seconds(5))
            guard !Task.isCancelled else { return }
            if case .waiting = phase {
                withAnimation(CC.motion.small) {
                    phase = .failed(
                        "Sent /effort \(value) to the Mac. "
                            + "CodeConnect has not received confirmation.")
                }
            }
        }
    }
}

/// The native `/compact` control: an optional instruction, one always-active
/// button. Completion is watched, not assumed — Claude Code's own
/// "Compacted (…)" line confirms it, its measured refusal on a near-empty
/// context fails it verbatim, and past the observation window the sheet
/// says plainly that no signal has arrived while continuing to watch.
struct CompactSheet: View {
    let sessionKey: String
    var prefill: String = ""
    var onLanded: () -> Void = {}

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    private enum Phase: Equatable {
        case idle
        case sending
        /// Keystrokes landed; watching for the completion line. `quietly`
        /// flips after the observation window: same watch, honest caption.
        case waiting(quietly: Bool)
        case confirmed(String)
        case failed(String)
    }

    @State private var phase: Phase = .idle
    @State private var instructions: String = ""
    @State private var timeoutTask: Task<Void, Never>?
    @State private var baseline: CompactSignal?

    private var state: SessionState? { model.states[sessionKey] }

    private var busy: Bool {
        switch phase {
        case .sending, .waiting: return true
        case .idle, .confirmed, .failed: return false
        }
    }

    var body: some View {
        CCSheetChrome("Compact context", onClose: { dismiss() }) {
            ScrollView {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    Text("Ask Claude Code to compact its current context. Instructions are optional.")
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    // In a card, like every other field in the app. A field
                    // label sits on the *content* column while a bare `Text`
                    // and a field's own border sit on the container's edge —
                    // so a field standing free on a sheet puts its label 36pt
                    // right of the box it names. The card is what gives all
                    // three the same edge. Measured against the shipped
                    // Terminal and SSH screen, where label, border and hint
                    // share one column inside exactly this container.
                    CCCard {
                        CCField(
                            label: "Instructions (optional)", text: $instructions,
                            placeholder: "What should Claude preserve?",
                            submitLabel: .done,
                            onSubmit: { apply() })
                    }
                    CCButton(
                        "Compact now",
                        variant: .primary,
                        isLoading: busy
                    ) { apply() }
                    statusSlot
                }
                .padding(CC.space.md)
            }
            .scrollBounceBehavior(.basedOnSize)
        }
        .onAppear {
            if instructions.isEmpty, !prefill.isEmpty { instructions = prefill }
        }
        .onChange(of: state?.lastCompactSignal) { _, signal in
            guard case .waiting = phase, let signal, signal != baseline else { return }
            timeoutTask?.cancel()
            withAnimation(CC.motion.small) {
                switch signal.outcome {
                case .compacted:
                    phase = .confirmed("Claude Code confirmed the compaction.")
                case .notEnoughMessages:
                    phase = .failed("Not enough messages to compact.")
                }
            }
        }
        .onDisappear { timeoutTask?.cancel() }
    }

    @ViewBuilder
    private var statusSlot: some View {
        switch phase {
        case .idle:
            EmptyView()
        case .sending:
            compactStatusLine("Typing /compact on the Mac…", tone: .neutral)
        case .waiting(quietly: false):
            compactStatusLine("Waiting for Claude Code to confirm…", tone: .neutral)
        case .waiting(quietly: true):
            compactStatusLine(
                "Compaction requested on the Mac. "
                    + "CodeConnect has not received a completion signal.",
                tone: .neutral)
        case .confirmed(let line):
            compactStatusLine(line, tone: .success)
        case .failed(let line):
            compactStatusLine(line, tone: .warning)
        }
    }

    private func compactStatusLine(_ text: String, tone: CCTone) -> some View {
        CCProse(
            text, style: CC.type.footnote,
            color: tone == .neutral ? CC.text.secondary : tone.color
        )
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func apply() {
        guard !busy else { return }
        baseline = state?.lastCompactSignal
        withAnimation(CC.motion.micro) { phase = .sending }
        Task {
            let attempt = await model.sendCompactCommand(
                instructions: instructions, to: sessionKey)
            switch attempt {
            case .sent:
                onLanded()
                if let signal = state?.lastCompactSignal, signal != baseline {
                    switch signal.outcome {
                    case .compacted:
                        phase = .confirmed("Claude Code confirmed the compaction.")
                    case .notEnoughMessages:
                        phase = .failed("Not enough messages to compact.")
                    }
                } else {
                    phase = .waiting(quietly: false)
                    armQuietFlip()
                }
            case .alreadyApplied:
                onLanded()
                phase = .confirmed("This compaction request was already sent earlier.")
            case .refused(let reason):
                phase = .failed("Nothing was typed: \(reason)")
            case .failed(let reason):
                phase = .failed("Couldn’t type the command: \(reason)")
            case .indeterminate(let reason):
                phase = .failed(
                    "Not confirmed: \(reason) Retry is safe; the command won’t be typed twice.")
            case .composerRecovered:
                onLanded()
                phase = .waiting(quietly: false)
                armQuietFlip()
            case .composerLost:
                phase = .failed("Couldn’t restore the composer. Open Terminal to recover.")
            }
        }
    }

    /// A real compaction measured ~6s on a small context and grows with the
    /// context, so the window's end is not a failure — the watch continues;
    /// only the caption stops promising.
    private func armQuietFlip() {
        timeoutTask?.cancel()
        timeoutTask = Task {
            try? await Task.sleep(for: .seconds(10))
            guard !Task.isCancelled else { return }
            if case .waiting(quietly: false) = phase {
                withAnimation(CC.motion.small) { phase = .waiting(quietly: true) }
            }
        }
    }
}

/// "12m ago" — one place, so every age in this feature reads the same.
enum RelativeAge {
    static func text(since date: Date, now: Date = Date()) -> String {
        let seconds = max(0, now.timeIntervalSince(date))
        if seconds < 60 { return "just now" }
        if seconds < 3600 { return "\(Int(seconds / 60))m ago" }
        if seconds < 86_400 { return "\(Int(seconds / 3600))h ago" }
        return "\(Int(seconds / 86_400))d ago"
    }
}
