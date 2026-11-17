import CodeConnectKit
import SwiftUI

// =============================================================================
//  Slash commands, from a phone.
//
//  Typing `/` in the composer summons a palette of what this app can do with
//  a command natively — the run's settings sheet for `/model` and `/effort`, a
//  sheet for `/compact`, a confirmation for `/clear`, and the app's own diff
//  for `/diff`. The send-time policy underneath: typed native commands open
//  their controls, typed dialog built-ins get the composer's refusal note, and
//  anything unknown passes through as the custom skill or plain text it is.
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

    /// The five native commands, in the order they are worth a thumb's
    /// attention: the two levers, the two context acts, then the diff.
    static let allRows: [PaletteRow] = [
        PaletteRow(
            command: "model", subtitle: "Choose model", icon: "cpu",
            action: .nativeModel),
        PaletteRow(
            command: "effort", subtitle: "Choose reasoning effort", icon: "gauge",
            action: .nativeEffort),
        PaletteRow(
            command: "compact", subtitle: "Compact context",
            icon: "arrows-in",
            action: .nativeCompact(prefillInstructions: "")),
        PaletteRow(
            command: "clear", subtitle: "Clear Claude’s context", icon: "eraser",
            action: .nativeClear),
        PaletteRow(
            command: "diff", subtitle: "Review working-tree changes",
            icon: "plus-minus",
            action: .nativeDiff),
    ]

    /// The palette for one composer text, or `nil` for none. Bare `/` is the
    /// discovery moment: every available row plus the caption. A fragment
    /// filters case-insensitively. Whitespace after the fragment means
    /// arguments are coming, and arguments are typing, not browsing.
    static func content(for typed: String) -> Content? {
        let afterLeading = typed.drop(while: \.isWhitespace)
        guard afterLeading.hasPrefix("/") else { return nil }
        let fragment = afterLeading.dropFirst()
        if fragment.isEmpty {
            return Content(rows: allRows, showsCaption: true)
        }
        guard !fragment.contains(where: \.isWhitespace) else { return nil }
        let needle = fragment.lowercased()
        let matches = allRows.filter { $0.command.hasPrefix(needle) }
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
                        Text("Run other commands on the computer.")
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

    @State private var phase: CommandPhase = .idle
    /// The command the last tap typed: what the status line is about.
    @State private var sent: NativeCommand?
    @State private var instructions: String = ""
    @State private var timeoutTask: Task<Void, Never>?
    /// How far the event stream had got when Compact was tapped; only a
    /// completion signal above this line answers this request.
    @State private var baselineSeq: UInt64 = 0

    private var receipts: SessionState.Receipts? { model.receipts(for: sessionKey) }

    /// See `CommandPhase.busy`.
    private var busy: Bool { phase.busy }

    var body: some View {
        CCSheetChrome(CompactPrompt.title, onClose: { dismiss() }) {
            VStack(spacing: 0) {
                ScrollView {
                    VStack(alignment: .leading, spacing: CC.space.md) {
                        Text(CompactPrompt.explanation)
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.text.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                        // In a card, like every other field in the app. A field
                        // label sits on the *content* column while a bare `Text`
                        // and a field's own border sit on the container's edge —
                        // so a field standing free on a sheet puts its label 36pt
                        // right of the box it names. The card is what gives all
                        // three the same edge. Measured against a shipped screen
                        // where label, border and hint share one column inside
                        // exactly this container.
                        CCCard {
                            CCField(
                                label: CompactPrompt.fieldLabel, text: $instructions,
                                placeholder: CompactPrompt.placeholder,
                                submitLabel: .done,
                                onSubmit: { apply() })
                        }
                        CCButton(
                            CompactPrompt.action,
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
        // The sequence, not the fact: a fact alone cannot say whether it is news.
        .onChange(of: receipts?.compactSeq) { _, _ in
            guard case .waiting(let request, _) = phase,
                let settled = model.receiptPhase(
                    for: .compact(instructions: request), in: sessionKey, after: baselineSeq)
            else { return }
            timeoutTask?.cancel()
            withAnimation(CC.motion.small) { phase = settled }
        }
        .onDisappear { timeoutTask?.cancel() }
    }

    /// Pinned at the bottom, where a sheet reports its outcome.
    @ViewBuilder
    private var pinnedStatus: some View {
        if phase != .idle {
            CCActionBar { statusSlot }
        }
    }

    @ViewBuilder
    private var statusSlot: some View {
        if let sent, let status = model.commandStatus(sent, phase) {
            compactStatusLine(status.text, tone: status.tone.ccTone)
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
        let command = NativeCommand.compact(instructions: instructions)
        sent = command
        baselineSeq = model.eventHighWater(for: sessionKey)
        withAnimation(CC.motion.micro) { phase = .sending(command.request) }
        Task {
            let attempt = await model.sendCompactCommand(
                instructions: command.request, to: sessionKey)
            if attempt.landed { onLanded() }
            phase = command.phase(after: attempt)
                ?? model.receiptPhase(for: command, in: sessionKey, after: baselineSeq)
                ?? .waiting(request: command.request, quietly: false)
            if case .waiting(_, quietly: false) = phase { armQuietFlip(command) }
        }
    }

    /// The window's end is not a failure: the watch continues and only the
    /// caption stops promising (see `NativeCommand.quietAfter`).
    private func armQuietFlip(_ command: NativeCommand) {
        timeoutTask?.cancel()
        timeoutTask = Task {
            try? await Task.sleep(for: command.quietAfter)
            guard !Task.isCancelled else { return }
            if case .waiting(let request, quietly: false) = phase {
                withAnimation(CC.motion.small) { phase = .waiting(request: request, quietly: true) }
            }
        }
    }
}
