import CodeConnectKit
import SwiftUI

/// The reply box at the foot of a run's conversation: what became of the last thing sent, or why sending cannot work,
/// over the words and what answers them. Where the words go is Kit's to say (`AppModel.submission`): a Codex run's own
/// message, Claude's typed text, or a slash command this box answers with its own control.
struct ComposerBar: View {
    let key: String
    /// The run's words, kept by its pane: they outlive a trip to the Terminal tab and back.
    let kept: ComposerDraft
    @Environment(Machines.self) private var machines

    var body: some View {
        if let model = machines.model(forSession: key) {
            Composer(model: model, key: key, kept: kept)
        }
    }
}

/// What a run's composer keeps while its tab is not shown, owned by the run's pane: the words being written, and the
/// send whose answer never came. The rest of the box (the last outcome, a command's watch) starts again.
@Observable
final class ComposerDraft {
    var text = ""
    var unanswered: UnansweredSend?
}

private struct Composer: View {
    let model: AppModel
    let key: String
    @Bindable var kept: ComposerDraft

    /// What became of the last Claude send or refused command; Kit words it.
    @State private var result: ComposeAttempt?
    @State private var retire: Task<Void, Never>?
    @State private var sending = false
    @State private var askingAgain = false
    /// The field has the keyboard: Esc leaves it before it closes the run.
    @FocusState private var fieldFocused: Bool
    /// The Model, Effort or Compact command typed last, and where it stands.
    @State private var running: Running?
    @State private var quiet: Task<Void, Never>?
    @State private var commandsOpen = false
    @State private var compacting = false
    @State private var instructions = ""
    @State private var clearing = false
    /// The draft that opened the list, the dialog or the confirmation: a command that lands consumes it, if it is
    /// still the draft.
    @State private var opener: String?

    private struct Running: Equatable {
        let command: NativeCommand
        var phase: CommandPhase
        /// How far the run's events had got when it was sent: only a receipt above this answers it.
        let baseline: UInt64
    }

    private var summary: SessionSummary? { model.shownSummary(for: key) }
    private var isCodex: Bool { summary?.isCodex == true }
    private var blocked: String? { model.sendBlockedReason(for: key, draft: kept.text) }
    private var computer: String { HostDevice.current.computer("the computer", named: model.machineName) }
    private var busy: Bool { sending || running?.phase.busy == true }

    private var canSend: Bool {
        blocked == nil && !sending && !kept.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let line { note(line) }
            box
        }
        .padding(.horizontal, 20)
        .padding(.top, 4)
        .padding(.bottom, 16)
        .onChange(of: kept.text) { _, new in
            kept.unanswered = nil
            // Editing is the reader's next act, so a standing failure or kept value has been read. The draft a landed
            // command consumes is not an edit.
            if result?.clearsOnEdit == true { result = nil }
            if !new.isEmpty, case .failed = running?.phase { running = nil }
            if !new.isEmpty, case .neutral = running?.phase { running = nil }
        }
        // A new handshake is the link back, and so is coming back from the Terminal tab: the unanswered send is asked
        // again, if Kit's rule still allows it.
        .onChange(of: model.connection.generation) { _, _ in askUnansweredAgain() }
        .onAppear { askUnansweredAgain() }
        // Esc closes the list, then leaves the field, before the window's Esc closes the run. Published to the whole
        // window, not only along the focus: a list opened by a click leaves nothing in the box focused.
        .focusedSceneValue(\.composerHoldsEscape, commandsOpen || fieldFocused)
        .onChange(of: model.receipts(for: key)?.modelSignal) { _, _ in settle() }
        .onChange(of: model.receipts(for: key)?.effortSeq) { _, _ in settle() }
        .onChange(of: model.receipts(for: key)?.compactSeq) { _, _ in settle() }
        .onDisappear {
            retire?.cancel()
            quiet?.cancel()
        }
        .sheet(isPresented: $compacting) {
            CompactDialog(instructions: $instructions) {
                compacting = false
                run(.compact(instructions: instructions))
            } cancel: {
                compacting = false
            }
        }
        .alert(ClearConfirmation.title, isPresented: $clearing) {
            Button(ClearConfirmation.clear, role: .destructive) { performClear() }
                .accessibilityIdentifier("session.composer.clear.confirm")
            Button(ClearConfirmation.keep, role: .cancel) {}
                .accessibilityIdentifier("session.composer.clear.cancel")
        } message: {
            Text(ClearConfirmation.message)
        }
    }

    // MARK: The box

    private var box: some View {
        let agent = summary?.agent.displayName ?? "the agent"
        return VStack(alignment: .leading, spacing: 10) {
            TextField(
                "", text: $kept.text, prompt: Text("Reply to \(agent)…").foregroundStyle(Theme.text3), axis: .vertical
            )
                .textFieldStyle(.plain)
                .font(Theme.ui(13))
                .foregroundStyle(Theme.text)
                .lineLimit(1...5)
                .onSubmit(submit)
                .focused($fieldFocused)
                .onExitCommand { if commandsOpen { commandsOpen = false } else { fieldFocused = false } }
                .accessibilityLabel("Reply to \(agent)…")
                .accessibilityIdentifier("session.composer.field")
            HStack(spacing: 10) {
                if isCodex, let counter = ComposeDraft(text: kept.text).counterText {
                    Text(counter)
                        .font(Theme.mono(11))
                        .foregroundStyle(ComposeDraft(text: kept.text).isOverCeiling ? Theme.red : Theme.text3)
                        .accessibilityIdentifier("session.composer.count")
                }
                Spacer()
                if !isCodex {
                    CommandsButton(
                        model: model, key: key, help: blocked ?? "Model, effort and compact", open: $commandsOpen,
                        choose: run
                    ) { compact(prefill: "") }
                    .disabled(blocked != nil || busy)
                }
                sendButton
            }
        }
        .padding(12)
        .background(Theme.raised, in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color.white.opacity(0.09)))
    }

    private var sendButton: some View {
        Button(action: submit) {
            Image(systemName: "arrow.up")
                .font(.system(size: 11, weight: .bold))
                .foregroundStyle(canSend ? Color.black : Theme.text3)
                .frame(width: 24, height: 24)
                .background(Circle().fill(canSend ? Theme.text : Theme.window))
        }
        .buttonStyle(.hover(radius: 12, over: true, id: "session.composer.send"))
        .disabled(!canSend)
        .help(blocked ?? (sending ? "Sending…" : "Send"))
        .accessibilityLabel("Send")
        .accessibilityIdentifier("session.composer.send")
    }

    // MARK: The line above

    private struct Line {
        let text: String
        let tone: ComposeProse.Tone
    }

    /// The command typed last, while it stands and sending can work; else what became of the last send (a Codex run's
    /// own outcome first, as Kit ranks it); else why sending cannot work, or that answers are given at the computer.
    /// A command's line never hides why Send is disabled: the watch for its receipt stays armed underneath.
    private var line: Line? {
        if blocked == nil, let running, let status = model.commandStatus(running.command, running.phase) {
            return Line(text: status.text, tone: status.tone)
        }
        if let kit = model.outcomeNote(for: key, lastSend: result) ?? model.standingNote(for: key, draft: kept.text) {
            return Line(text: kit.text, tone: kit.tone)
        }
        return nil
    }

    private func note(_ line: Line) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            if let symbol = line.tone.symbol {
                Image(systemName: symbol).foregroundStyle(line.tone.color).accessibilityHidden(true)
            }
            Text(line.text).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
        }
        .font(Theme.ui(12))
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("session.composer.note")
    }

    // MARK: Sending

    private func submit() {
        guard canSend else { return }
        let draft = kept.text
        let again = askingAgain
        askingAgain = false
        switch model.submission(of: draft, to: key) {
        case .codex: sendToCodex(draft)
        case .command(let action): route(action, draft: draft)
        case .claude: sendToClaude(draft, again: again)
        }
    }

    /// Typed into Claude's prompt. The draft clears only on a landed send; a refusal stands until the text changes,
    /// and good news retires by itself.
    private func sendToClaude(_ draft: String, again: Bool) {
        sending = true
        running = nil
        result = nil
        retire?.cancel()
        kept.unanswered = nil
        let generation = model.connection.generation
        let sentAt = Date()
        Task {
            let attempt = await model.send(text: draft, to: key)
            sending = false
            if case .indeterminate = attempt, !again {
                kept.unanswered = UnansweredSend(text: draft, sentAt: sentAt)
                // The link may have come back while this send waited.
                if model.connection.generation != generation { askUnansweredAgain() }
            }
            // Words typed while the send was out are the next message, not this one.
            if attempt.landed, kept.text == draft { kept.text = "" }
            noteResult(attempt)
        }
    }

    /// A Codex run's own message. Its outcome is Kit's (`outcomeNote`); the draft is kept on a refusal and on an
    /// outcome nobody can account for, so the reader still holds their words.
    private func sendToCodex(_ draft: String) {
        sending = true
        running = nil
        result = nil
        retire?.cancel()
        Task {
            let outcome = await model.composeToCodex(sessionKey: key, text: draft)
            sending = false
            if outcome?.landed == true, kept.text == draft { kept.text = "" }
        }
    }

    private func askUnansweredAgain() {
        guard let unanswered = kept.unanswered,
            model.asksAgain(unanswered, to: key, draft: kept.text, sending: sending)
        else { return }
        kept.unanswered = nil
        askingAgain = true
        submit()
    }

    private func noteResult(_ attempt: ComposeAttempt) {
        running = nil
        result = attempt
        retire?.cancel()
        guard attempt.landed else { return }
        retire = Task {
            try? await Task.sleep(for: .seconds(4))
            guard !Task.isCancelled else { return }
            result = nil
        }
    }

    // MARK: Commands

    /// A slash command answered with this box's own control, before anything reaches the computer. Model and Effort
    /// open the list, whose lines state what a change does: the daemon completes Claude Code's confirmation for them.
    private func route(_ action: CommandAction, draft: String) {
        switch action {
        case .nativeModel, .nativeEffort:
            opener = draft
            commandsOpen = true
        case .nativeCompact(let prefill):
            compact(prefill: prefill)
            opener = draft
        case .nativeClear:
            opener = draft
            clearing = true
        case .nativeDiff:
            // Kit's policy refuses it, before the wire, in its own words: they point to the Changes tab.
            Task { noteResult(await model.send(text: draft, to: key)) }
        case .nativeSnapshot(let command):
            noteResult(.refused(takesComposer("/\(command.rawValue)")))
        case .blocked(_, let reason):
            noteResult(.refused(reason))
        case .passThrough:
            break
        }
    }

    private func takesComposer(_ command: String) -> String {
        "\(command) opens a view on \(computer) and would take over this composer. Use the Terminal tab to run it."
    }

    private func compact(prefill: String) {
        opener = nil
        instructions = prefill
        compacting = true
    }

    /// Types the command, then watches for Claude Code's own receipt: landing proves typing, not execution.
    private func run(_ command: NativeCommand) {
        guard !busy else { return }
        let draft = opener
        opener = nil
        result = nil
        retire?.cancel()
        quiet?.cancel()
        let baseline = model.eventHighWater(for: key)
        running = Running(command: command, phase: .sending(command.request), baseline: baseline)
        sending = true
        Task {
            let attempt: ComposeAttempt
            switch command {
            case .model(let alias): attempt = await model.sendModelCommand(alias, to: key)
            case .effort(let level): attempt = await model.sendEffortCommand(level, to: key)
            case .compact(let instructions):
                attempt = await model.sendCompactCommand(instructions: instructions, to: key)
            }
            sending = false
            if attempt.landed, let draft, kept.text == draft { kept.text = "" }
            // Claude Code's receipt may already have landed while typing.
            let phase =
                command.phase(after: attempt)
                ?? model.receiptPhase(for: command, in: key, after: baseline)
                ?? .waiting(request: command.request, quietly: false)
            show(Running(command: command, phase: phase, baseline: baseline))
        }
    }

    /// A receipt above the baseline settles a command still waiting for one, however late it comes.
    private func settle() {
        guard let running, case .waiting = running.phase,
            let settled = model.receiptPhase(for: running.command, in: key, after: running.baseline)
        else { return }
        show(Running(command: running.command, phase: settled, baseline: running.baseline))
    }

    /// Past the usual window the caption stops promising and the watch stays armed. A change retires by itself; a kept
    /// value or a failure stands until the next edit or action, as long as a sheet would show it.
    private func show(_ next: Running) {
        running = next
        quiet?.cancel()
        switch next.phase {
        case .waiting(let request, quietly: false):
            quiet = Task {
                try? await Task.sleep(for: next.command.quietAfter)
                guard !Task.isCancelled, running == next else { return }
                running = Running(
                    command: next.command, phase: .waiting(request: request, quietly: true), baseline: next.baseline)
            }
        case .confirmed:
            quiet = Task {
                try? await Task.sleep(for: .seconds(4))
                guard !Task.isCancelled, running == next else { return }
                running = nil
            }
        case .idle, .sending, .waiting, .neutral, .failed:
            break
        }
    }

    /// `/clear`, past its confirmation. The note claims typing, nothing more.
    private func performClear() {
        let draft = opener
        opener = nil
        sending = true
        running = nil
        result = nil
        retire?.cancel()
        Task {
            let attempt = await model.sendClearCommand(to: key)
            sending = false
            if attempt.landed, let draft, kept.text == draft { kept.text = "" }
            noteResult(attempt)
        }
    }
}

/// The button beside Send naming the run's model, and its list opening upward over the conversation: the four
/// families, the five effort levels, and Compact. The current model is marked; the effort is not, as Claude Code does
/// not report it.
private struct CommandsButton: View {
    let model: AppModel
    let key: String
    /// The button's own tooltip: why it is disabled, or what it holds. Not the list's, whose rows say what they are.
    let help: String
    @Binding var open: Bool
    let choose: (NativeCommand) -> Void
    let compact: () -> Void

    private var current: ModelDisplay? { model.confirmedModel(for: key).map { ModelDisplay.from($0.name) } }

    var body: some View {
        Button {
            open.toggle()
        } label: {
            HStack(spacing: 5) {
                AgentMark(agent: .claude)
                Text(current?.name ?? "Model").foregroundStyle(Theme.text)
                Image(systemName: "chevron.down").font(.system(size: 8, weight: .semibold))
            }
            .font(Theme.ui(12))
            .foregroundStyle(Theme.text2)
            .padding(.horizontal, 6)
            .frame(height: 24)
            .background(open ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: "session.composer.commands"))
        .help(help)
        .accessibilityLabel(current?.name ?? "Model")
        .accessibilityIdentifier("session.composer.commands")
        .accessibilityValue(open ? "open" : "closed")
        .overlay(alignment: .bottomTrailing) {
            if open {
                list.offset(y: -30)
                    // Esc closes the list first: the window's own Esc stands down while the list is open.
                    .background {
                        Button("Close list") { open = false }
                            .keyboardShortcut(.cancelAction)
                            .accessibilityIdentifier("session.composer.commands.close")
                            .opacity(0)
                            .allowsHitTesting(false)
                    }
            }
        }
    }

    private var list: some View {
        VStack(alignment: .leading, spacing: 0) {
            header("Model")
            footnote(NativeCommand.modelConsent)
            ForEach(ModelDisplay.families, id: \.self) { alias in
                let isCurrent = current.map { ModelChangeWatch.answers(alias, with: $0.name) } ?? false
                row(ModelDisplay.from(alias).name, marked: isCurrent, id: "session.composer.model.\(alias)") {
                    choose(.model(alias))
                }
                .disabled(isCurrent)
            }
            divider
            header("Effort")
            footnote(NativeCommand.effortConsent)
            ForEach(NativeCommand.effortLevels, id: \.value) { level in
                row(level.label, marked: false, id: "session.composer.effort.\(level.value)") {
                    choose(.effort(level.value))
                }
            }
            divider
            row("Compact…", marked: false, id: "session.composer.compact", perform: compact)
        }
        .padding(.vertical, 5)
        .frame(width: 280)
        .fixedSize(horizontal: false, vertical: true)
        .background(Theme.raised, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color.white.opacity(0.1)))
        .shadow(color: .black.opacity(0.5), radius: 18, y: 8)
    }

    private func header(_ text: String) -> some View {
        Text(text).font(Theme.ui(11, .semibold)).foregroundStyle(Theme.text3)
            .padding(.horizontal, 12)
            .padding(.top, 5)
            .padding(.bottom, 3)
    }

    private func footnote(_ text: String) -> some View {
        Text(text).font(Theme.ui(11)).foregroundStyle(Theme.text3)
            .padding(.horizontal, 12)
            .padding(.bottom, 4)
            .fixedSize(horizontal: false, vertical: true)
    }

    private var divider: some View {
        Rectangle().fill(Theme.hairline).frame(height: 1).padding(.vertical, 5)
    }

    private func row(_ title: String, marked: Bool, id: String, perform: @escaping () -> Void) -> some View {
        Button {
            open = false
            perform()
        } label: {
            HStack(spacing: 8) {
                Image(systemName: "checkmark")
                    .font(.system(size: 9.5, weight: .bold))
                    .opacity(marked ? 1 : 0)
                    .frame(width: 12)
                    .accessibilityHidden(true)
                Text(title).font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 8)
            .frame(height: 24)
            .background(marked ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: id))
        .padding(.horizontal, 4)
        .accessibilityIdentifier(id)
        .accessibilityAddTraits(marked ? .isSelected : [])
    }
}

/// Compact's own dialog: optional instructions, then Compact now or Cancel. Nothing is typed until Compact now.
private struct CompactDialog: View {
    @Binding var instructions: String
    let run: () -> Void
    let cancel: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 12) {
                Text(CompactPrompt.title).font(Theme.ui(15, .semibold)).foregroundStyle(Theme.text)
                Text(CompactPrompt.explanation)
                    .font(Theme.ui(12.5))
                    .foregroundStyle(Theme.text2)
                    .fixedSize(horizontal: false, vertical: true)
                TextField(
                    "", text: $instructions,
                    prompt: Text(CompactPrompt.placeholder).foregroundStyle(Theme.text3), axis: .vertical
                )
                .textFieldStyle(.plain)
                .font(Theme.ui(12.5))
                .lineLimit(2...5)
                .padding(8)
                .background(Theme.code, in: RoundedRectangle(cornerRadius: 7))
                .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
                .accessibilityLabel(CompactPrompt.fieldLabel)
                .accessibilityIdentifier("session.composer.compact.instructions")
            }
            .padding(18)
            SheetBar(
                action: CompactPrompt.action, actionID: "session.composer.compact.run", blocked: false, perform: run,
                cancel: cancel, cancelID: "session.composer.compact.cancel"
            ) {
                EmptyView()
            }
        }
        .frame(width: 420)
        .background(Theme.sheet)
    }
}

extension ComposeProse.Tone {
    /// A Kit note's tone on the Mac, as `CodexProse.CodexTone` draws it; a neutral note has no mark.
    var color: Color {
        switch self {
        case .neutral, .info, .success: Theme.text3
        case .warning: Theme.amber
        case .danger: Theme.red
        }
    }

    var symbol: String? {
        switch self {
        case .neutral: nil
        case .info: "info.circle"
        case .success: "checkmark.circle"
        case .warning: "exclamationmark.circle"
        case .danger: "xmark.circle"
        }
    }
}
