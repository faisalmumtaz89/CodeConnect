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
        // Kept as a row because Claude Code accepts `/cost` and a reader who
        // thinks in cost should find it — but the subtitle states the measured
        // fact instead of inventing a "cost view". See `SnapshotCommand.title`.
        PaletteRow(
            command: "cost", subtitle: "Alias of /usage", icon: "creditcard",
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
        /// Keystrokes landed; Claude Code has not yet answered in the
        /// transcript. `quietly` flips once the usual window has passed: the
        /// same watch, an honest caption. **It never becomes a failure** — a
        /// receipt that arrives late still has to be able to settle this sheet.
        /// A terminal "no confirmation" over a transcript that went on to say
        /// `Set model to X` is a stale claim, and stale claims are the thing
        /// this sheet may not make.
        case waiting(request: String, quietly: Bool)
        case confirmed(String)
        /// Terminal, and neither success nor failure: the request was
        /// answered, and the answer was not a change. Claude Code kept the
        /// model, or the daemon replayed a settled mutation that carries no
        /// outcome. A green tick over either would be the exact lie this app
        /// exists not to tell; a warning would invent a fault that is not
        /// there. The sentence carries which it was.
        case neutral(String)
        case failed(String)
    }

    @State private var phase: Phase = .idle
    @State private var custom: String = ""
    @State private var timeoutTask: Task<Void, Never>?
    /// How far the event stream had got when a row was tapped. Only a `/model`
    /// signal above this line can be an answer to this send — see
    /// `ModelChangeWatch`.
    @State private var baselineSeq: UInt64 = 0

    private var state: SessionState? { model.states[sessionKey] }

    /// **Quiet waiting is not busy.** The watch stays armed for a receipt that
    /// may still arrive, but once the usual window has passed the controls come
    /// back: a sheet whose every row is inert, with a spinner running and no
    /// statement that only dismissing it will help, is a dead end. A receipt
    /// that never arrives — reworded copy, a gap reset, a dropped socket —
    /// must not cost the reader the sheet.
    private var busy: Bool {
        switch phase {
        case .sending, .waiting(_, quietly: false): return true
        case .waiting(_, quietly: true), .idle, .confirmed, .neutral, .failed: return false
        }
    }

    private var current: ModelDisplay? {
        state?.lastConfirmedModel.map { ModelDisplay.from($0.name) }
    }

    var body: some View {
        CCSheetChrome("Model", onClose: { dismiss() }) {
            VStack(spacing: 0) {
                ScrollView {
                    VStack(alignment: .leading, spacing: CC.space.md) {
                        CCSectionHeader("Current model")
                        currentCard
                        CCSectionHeader("Choose model")
                        // Conditional on purpose. Unqualified, this promised a
                        // consequence that does not occur on the common path:
                        // when the conversation is already cached for the
                        // current model the command opens a confirmation, the
                        // daemon's rescue cancels it, and nothing is set or
                        // saved. Consent copy may predict what an action does;
                        // it may not assert an outcome the action often fails
                        // to reach.
                        // **The rule for *consent* prose on these sheets:** state
                        // a consequence before the tap only where the app will
                        // act on it for you. Here it will — Claude Code asks
                        // about the re-read itself, and the daemon completes
                        // that confirmation — so this is where the human
                        // consents to it. Effort has none because nothing is
                        // completed on its behalf; Compact's line describes its
                        // optional field, which is a different job.
                        //
                        // "already run" was wrong: the measured trigger is a
                        // conversation cached for the current model, and after
                        // `/clear` a conversation has run and the command is
                        // inline with no re-read.
                        Text(
                            "A confirmed change also becomes your default for new sessions. "
                                + "Switching mid-conversation makes Claude Code re-read the "
                                + "whole conversation on your next message."
                        )
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.text.secondary)
                        chooserCard
                        customEntry
                        Text("For this session only, use the model picker in the Terminal tab.")
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.text.tertiary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(CC.space.md)
                }
                .scrollBounceBehavior(.basedOnSize)
                pinnedStatus
            }
        }
        .onAppear {
            if custom.isEmpty, !prefill.isEmpty { custom = prefill }
        }
        .onChange(of: state?.lastModelCommandSignal) { _, signal in
            guard case .waiting(let request, _) = phase,
                let outcome = ModelChangeWatch.outcome(
                    after: baselineSeq, requested: request, signal: signal)
            else { return }
            timeoutTask?.cancel()
            withAnimation(CC.motion.small) { phase = Self.phase(for: outcome) }
        }
        .onDisappear { timeoutTask?.cancel() }
    }

    /// Terminal phase for one `/model` outcome — the sentences live on
    /// `ModelConfirmation` and on Claude Code's own error line, where a test
    /// can assert them.
    private static func phase(for outcome: ModelCommandOutcome) -> Phase {
        switch outcome {
        case .receipt(let receipt):
            return receipt.isChange
                ? .confirmed(receipt.sheetStatus) : .neutral(receipt.sheetStatus)
        // Verbatim: Claude Code already said the useful thing, and the value it
        // is quoting is the one the reader typed.
        case .notFound(let line):
            return .failed(line)
        }
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
    ///
    /// A `kept` receipt keeps the same origin words: it is still `/model`
    /// reporting the model in force, and inventing a third phrase for it would
    /// say more about the request than about the fact.
    static func provenanceLine(_ fact: ConfirmedModel, now: Date) -> String {
        let origin: String
        switch fact.provenance {
        case .sessionStart: origin = "Confirmed at session start"
        case .command: origin = "Confirmed by /model"
        }
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
        // The spinner tracks `busy`, not the request: once the wait goes quiet
        // the row is tappable again, and a ring still turning on a live control
        // would say it is not.
        let isApplying: Bool = {
            if case .sending(let active) = phase { return active == alias }
            if case .waiting(let active, quietly: false) = phase { return active == alias }
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
                    // An **alias**, because a full API id will not switch: the
                    // dialog renders `claude-sonnet-5` as "Sonnet 5", so the
                    // daemon cannot establish that the confirmation on screen is
                    // about the value asked for, and safely declines to complete
                    // it. Suggesting one here would be suggesting a value that
                    // reports back "no model change was confirmed".
                    placeholder: "e.g. sonnet",
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

    /// **Pinned, not scrolled.** Below the chooser, the custom field and two
    /// footnotes, the outcome of a tap lands under the fold of a medium-detent
    /// sheet — present, and unreadable. A report the reader cannot see is not a
    /// report, and this is the one line on the sheet that has to be read.
    ///
    /// Nothing at rest: `CCActionBar` only exists once there is something to
    /// say, so an untouched sheet is exactly as it was.
    ///
    /// **A sibling in a `VStack`, deliberately not `safeAreaInset`.** Measured:
    /// the inset form hangs the main thread at the largest accessibility sizes,
    /// where the bar is tallest. An inset whose height depends on the width it
    /// is inset into, inside a scroll view that then re-lays out, is a layout
    /// that can chase itself. A plain stack cannot.
    @ViewBuilder
    private var pinnedStatus: some View {
        if phase != .idle {
            CCActionBar { statusSlot }
        }
    }

    @ViewBuilder
    private var statusSlot: some View {
        switch phase {
        case .idle:
            EmptyView()
        case .sending(let alias):
            statusLine("Typing /model \(alias) on the Mac…", tone: .neutral)
        case .waiting(_, quietly: false):
            statusLine("Waiting for Claude Code to confirm…", tone: .neutral)
        case .waiting(let alias, quietly: true):
            statusLine(
                "Sent /model \(alias) to the Mac. Still waiting for Claude Code to confirm.",
                tone: .neutral)
        case .confirmed(let line):
            statusLine(line, tone: .success)
        case .neutral(let line):
            statusLine(line, tone: .neutral)
        case .failed(let line):
            statusLine(line, tone: .warning)
        }
    }

    /// One sentence, three sheets. A duplicate response proves the mutation was
    /// typed once and settled; it does not carry what Claude Code then did, so
    /// this claims neither a change nor the absence of one.
    static let alreadySentLine =
        "This request was sent earlier; this retry does not confirm its outcome."

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
        guard !busy else { return }
        baselineSeq = model.eventHighWater(for: sessionKey)
        withAnimation(CC.motion.micro) { phase = .sending(alias) }
        Task {
            let attempt = await model.sendModelCommand(alias, to: sessionKey)
            switch attempt {
            case .sent:
                // Keystrokes landed — which proves typing, not execution.
                // Claude Code's own transcript receipt flips this to done, and
                // it may already have landed while `.sending`.
                onLanded()
                if let outcome = ModelChangeWatch.outcome(
                    after: baselineSeq, requested: alias,
                    signal: state?.lastModelCommandSignal)
                {
                    phase = Self.phase(for: outcome)
                } else {
                    phase = .waiting(request: alias, quietly: false)
                    armQuietFlip()
                }
            case .alreadyApplied:
                // The daemon replayed a settled mutation without typing, so no
                // new transcript line is coming. It proves the keys were typed
                // once and nothing else — in particular, the settled outcome it
                // is replaying may have been a cancelled confirmation.
                onLanded()
                phase = .neutral(Self.alreadySentLine)
            case .refused(let reason):
                phase = .failed("Nothing was typed: \(reason)")
            case .failed(let reason):
                phase = .failed("Couldn’t type the command: \(reason)")
            case .indeterminate(let reason):
                phase = .failed(
                    "Not confirmed: \(reason) Retry is safe; the command won’t be typed twice.")
            case .composerRecovered:
                // The command took the composer and the daemon's generic
                // slash-command rescue pressed Esc to give it back. Measured on
                // 2.1.223: when the conversation is already cached for the
                // current model, `/model <alias>` opens a confirmation, and that
                // Esc answers it in the negative — Claude Code then prints
                // `Kept model as …`. So this is never a change.
                //
                // Only what was observed is claimed: the composer went away and
                // one Esc brought it back. The view is not named; the daemon
                // does not identify views.
                onLanded()
                phase = .neutral(
                    "/model \(alias) took the composer on the Mac and CodeConnect "
                        + "pressed Esc to give it back. No model change was confirmed.")
                // On 2.1.223 that recovered path was followed by `Kept model as …`.
                // The UI reports only the observed recovery and that no change
                // was confirmed.
            case .composerLost:
                phase = .failed(
                    "Couldn’t restore the composer. Open Terminal to recover.")
            }
        }
    }

    /// The observation window ends; the observation does not. Past it the
    /// caption stops promising and nothing else changes — the watch stays
    /// armed, so a receipt arriving late still settles this sheet correctly.
    private func armQuietFlip() {
        timeoutTask?.cancel()
        timeoutTask = Task {
            try? await Task.sleep(for: .seconds(5))
            guard !Task.isCancelled else { return }
            if case .waiting(let alias, quietly: false) = phase {
                withAnimation(CC.motion.small) {
                    phase = .waiting(request: alias, quietly: true)
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
        /// Keystrokes landed; watching for Claude Code's own receipt. See the
        /// note on `ModelSheet.Phase.waiting` — `quietly` changes the caption
        /// and nothing else, and this never becomes a failure.
        case waiting(request: String, quietly: Bool)
        case confirmed(String)
        /// See `ModelSheet.Phase.neutral`.
        case neutral(String)
        case failed(String)
    }

    @State private var phase: Phase = .idle
    @State private var timeoutTask: Task<Void, Never>?
    /// How far the event stream had got when a row was tapped; only a receipt
    /// above this line answers this send.
    @State private var baselineSeq: UInt64 = 0

    private var state: SessionState? { model.states[sessionKey] }

    /// See `ModelSheet.busy`.
    private var busy: Bool {
        switch phase {
        case .sending, .waiting(_, quietly: false): return true
        case .waiting(_, quietly: true), .idle, .confirmed, .neutral, .failed: return false
        }
    }

    /// Terminal phase for one effort receipt — the sentence lives on
    /// `EffortConfirmation`, where a test can assert it.
    private static func phase(for receipt: EffortConfirmation) -> Phase {
        receipt.isChange ? .confirmed(receipt.sheetStatus) : .neutral(receipt.sheetStatus)
    }

    var body: some View {
        CCSheetChrome("Effort", onClose: { dismiss() }) {
            VStack(spacing: 0) {
                ScrollView {
                    VStack(alignment: .leading, spacing: CC.space.md) {
                        CCSectionHeader("Choose effort")
                        // The same rule as the Model sheet: state a consequence
                        // before the tap where the app will act on it for you.
                        // Claude Code asks about the re-read itself when the
                        // conversation is warm, and the daemon completes that
                        // confirmation — so this is where the human consents.
                        //
                        // The *scope* is deliberately not claimed here: it is
                        // measured to differ per value (`max` is this-session
                        // only, the rest save as the default), so the receipt
                        // states it afterwards in Claude Code's own words.
                        Text(
                            "Switching mid-conversation makes Claude Code re-read the whole "
                                + "conversation on your next message."
                        )
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        chooserCard
                    }
                    .padding(CC.space.md)
                }
                .scrollBounceBehavior(.basedOnSize)
                pinnedStatus
            }
        }
        // The *sequence*, not the fact: a fact can repeat verbatim, and a
        // backfill can publish a historical one into an empty slot.
        .onChange(of: state?.lastConfirmedEffortSeq) { _, seq in
            guard case .waiting = phase, let seq, seq > baselineSeq,
                let fact = state?.lastConfirmedEffort
            else { return }
            timeoutTask?.cancel()
            withAnimation(CC.motion.small) { phase = Self.phase(for: fact) }
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
            if case .waiting(let active, quietly: false) = phase { return active == value }
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

    /// Pinned for the same reason as the Model sheet's — see `pinnedStatus`
    /// there. Five effort rows already fill a medium-detent sheet, so the
    /// outcome was reliably off-screen.
    @ViewBuilder
    private var pinnedStatus: some View {
        if phase != .idle {
            CCActionBar { statusSlot }
        }
    }

    @ViewBuilder
    private var statusSlot: some View {
        switch phase {
        case .idle:
            EmptyView()
        case .sending(let value):
            effortStatusLine("Typing /effort \(value) on the Mac…", tone: .neutral)
        case .waiting(_, quietly: false):
            effortStatusLine("Waiting for Claude Code to confirm…", tone: .neutral)
        case .waiting(let value, quietly: true):
            effortStatusLine(
                "Sent /effort \(value) to the Mac. Still waiting for Claude Code to confirm.",
                tone: .neutral)
        case .confirmed(let line):
            effortStatusLine(line, tone: .success)
        case .neutral(let line):
            effortStatusLine(line, tone: .neutral)
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
        guard !busy else { return }
        baselineSeq = model.eventHighWater(for: sessionKey)
        withAnimation(CC.motion.micro) { phase = .sending(value) }
        Task {
            let attempt = await model.sendEffortCommand(value, to: sessionKey)
            switch attempt {
            case .sent:
                onLanded()
                // The receipt may already have landed while `.sending`.
                if let fact = state?.lastConfirmedEffort,
                    let seq = state?.lastConfirmedEffortSeq, seq > baselineSeq
                {
                    phase = Self.phase(for: fact)
                } else {
                    phase = .waiting(request: value, quietly: false)
                    armQuietFlip(value)
                }
            case .alreadyApplied:
                onLanded()
                phase = .neutral(ModelSheet.alreadySentLine)
            case .refused(let reason):
                phase = .failed("Nothing was typed: \(reason)")
            case .failed(let reason):
                phase = .failed("Couldn’t type the command: \(reason)")
            case .indeterminate(let reason):
                phase = .failed(
                    "Not confirmed: \(reason) Retry is safe; the command won’t be typed twice.")
            case .composerRecovered:
                // As on the Model sheet: on 2.1.223 `/effort <new value>` opens
                // a confirmation whenever the conversation is already cached for
                // the current level, and the generic rescue answers it with Esc.
                // Claude Code then prints `Kept effort level as …`.
                onLanded()
                phase = .neutral(
                    "/effort \(value) took the composer on the Mac and CodeConnect "
                        + "pressed Esc to give it back. No effort change was confirmed.")
            case .composerLost:
                phase = .failed("Couldn’t restore the composer. Open Terminal to recover.")
            }
        }
    }

    /// See `ModelSheet.armQuietFlip`: the caption stops promising, the watch
    /// continues.
    private func armQuietFlip(_ value: String) {
        timeoutTask?.cancel()
        timeoutTask = Task {
            try? await Task.sleep(for: .seconds(5))
            guard !Task.isCancelled else { return }
            if case .waiting(let request, quietly: false) = phase {
                withAnimation(CC.motion.small) {
                    phase = .waiting(request: request, quietly: true)
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
        /// See `ModelSheet.Phase.neutral`.
        case neutral(String)
        case failed(String)
    }

    @State private var phase: Phase = .idle
    @State private var instructions: String = ""
    @State private var timeoutTask: Task<Void, Never>?
    /// How far the event stream had got when Compact was tapped; only a
    /// completion signal above this line answers this request.
    @State private var baselineSeq: UInt64 = 0

    private var state: SessionState? { model.states[sessionKey] }

    /// See `ModelSheet.busy`.
    private var busy: Bool {
        switch phase {
        case .sending, .waiting(quietly: false): return true
        case .waiting(quietly: true), .idle, .confirmed, .neutral, .failed: return false
        }
    }

    var body: some View {
        CCSheetChrome("Compact context", onClose: { dismiss() }) {
            VStack(spacing: 0) {
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
                    }
                    .padding(CC.space.md)
                }
                .scrollBounceBehavior(.basedOnSize)
                pinnedStatus
            }
        }
        .onAppear {
            if instructions.isEmpty, !prefill.isEmpty { instructions = prefill }
        }
        // The sequence, not the fact — see the Effort sheet.
        .onChange(of: state?.lastCompactSignalSeq) { _, seq in
            guard case .waiting = phase, let seq, seq > baselineSeq,
                let signal = state?.lastCompactSignal
            else { return }
            timeoutTask?.cancel()
            withAnimation(CC.motion.small) {
                switch signal {
                case .compacted:
                    phase = .confirmed("Claude Code confirmed the compaction.")
                case .notEnoughMessages:
                    phase = .failed("Not enough messages to compact.")
                }
            }
        }
        .onDisappear { timeoutTask?.cancel() }
    }

    /// Pinned, as on the Model and Effort sheets — one vocabulary for where a
    /// sheet reports its outcome.
    @ViewBuilder
    private var pinnedStatus: some View {
        if phase != .idle {
            CCActionBar { statusSlot }
        }
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
        case .neutral(let line):
            compactStatusLine(line, tone: .neutral)
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
        baselineSeq = model.eventHighWater(for: sessionKey)
        withAnimation(CC.motion.micro) { phase = .sending }
        Task {
            let attempt = await model.sendCompactCommand(
                instructions: instructions, to: sessionKey)
            switch attempt {
            case .sent:
                onLanded()
                if let signal = state?.lastCompactSignal,
                    let seq = state?.lastCompactSignalSeq, seq > baselineSeq
                {
                    switch signal {
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
                phase = .neutral(ModelSheet.alreadySentLine)
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
