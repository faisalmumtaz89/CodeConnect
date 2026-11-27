import CodeConnectKit
import SwiftUI

// =============================================================================
//  Slash commands, from a phone.
//
//  Typing `/` in the composer summons a palette of what this app can do with
//  a command natively — the run's settings sheet for `/model` and `/effort`, a
//  sheet for `/compact`, a confirmation for `/clear`, and the app's own diff
//  for `/diff`. The send-time policy underneath: typed native commands open
//  their controls, and anything else passes through as the command, custom
//  skill or plain text it is.
// =============================================================================

/// The palette above the keyboard while the composer holds a bare `/` or a
/// prefix of a native command (Kit's `SlashPalette`: its rows, filter and
/// caption). Anything else — arguments, unknown fragments — shows no palette
/// at all; typing simply continues.
struct CommandPalette: View {
    let content: SlashPalette.Content
    let onAction: (CommandAction) -> Void

    /// Each native command's mark in this app's icon set.
    static func icon(for command: String) -> String {
        switch command {
        case "model": "cpu"
        case "effort": "gauge"
        case "compact": "arrows-in"
        case "clear": "eraser"
        default: "plus-minus"
        }
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
                            subtitle: row.unavailable ?? row.subtitle,
                            separator: row != content.rows.last || content.caption != nil,
                            isDimmed: row.unavailable != nil,
                            disabledReason: row.unavailable.map(CCDisabledReason.init),
                            action: { onAction(row.action) }
                        ) {
                            CCIcon(Self.icon(for: row.command), size: 16, weight: .regular, relativeTo: .body)
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
                    if let caption = content.caption {
                        Text(caption)
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
    /// The run's agent: its words in the sheet, and what answers a compact (Claude's own signal; Antigravity's
    /// turn's end).
    private var agent: AgentKind { model.shownSummary(for: sessionKey)?.agent ?? .claude }

    /// See `CommandPhase.busy`.
    private var busy: Bool { phase.busy }

    var body: some View {
        CCSheetChrome(CompactPrompt.title, onClose: { dismiss() }) {
            VStack(spacing: 0) {
                ScrollView {
                    VStack(alignment: .leading, spacing: CC.space.md) {
                        Text(CompactPrompt.explanation(for: agent))
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
                        // A field and its label, nothing in a gutter: the
                        // card's own padding on both sides.
                        CCCard(contentColumn: false) {
                            CCField(
                                label: CompactPrompt.fieldLabel, text: $instructions,
                                placeholder: CompactPrompt.placeholder(for: agent),
                                submitLabel: .done, labelOnContentColumn: false,
                                onSubmit: { apply() })
                        }
                        CCButton(
                            CompactPrompt.action,
                            variant: .primary, size: .lg, fullWidth: true,
                            isLoading: busy
                        ) { apply() }
                    }
                    .padding(CC.space.md)
                    .ccSheetHeightPart()
                }
                .scrollBounceBehavior(.basedOnSize)
                pinnedStatus.ccSheetHeightPart()
            }
        }
        .onAppear {
            if instructions.isEmpty, !prefill.isEmpty { instructions = prefill }
        }
        // The sequence, not the fact: a fact alone cannot say whether it is news. A compact is answered by the
        // agent's own signal, or, for Antigravity, by the turn's end.
        .onChange(of: [receipts?.compactSeq ?? 0, receipts?.turnEndSeq ?? 0]) { _, _ in
            guard case .waiting(let request, _) = phase,
                let settled = model.receiptPhase(
                    for: .compact(instructions: request), in: sessionKey, after: baselineSeq, agent: agent)
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
        if let sent, let status = model.commandStatus(sent, phase, agent: agent) {
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
                ?? model.receiptPhase(for: command, in: sessionKey, after: baselineSeq, agent: agent)
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
