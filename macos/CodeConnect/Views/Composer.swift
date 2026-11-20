import CodeConnectKit
import SwiftUI

/// The reply box at the foot of a run's conversation: what became of the last thing sent, or why sending cannot work,
/// over the words and what answers them. Where the words go is Kit's to say (`AppModel.submission`): a Codex run's own
/// message, Claude's typed text, or a slash command this box answers with its own control.
struct ComposerBar: View {
    let key: String
    @Environment(Machines.self) private var machines

    var body: some View {
        if let model = machines.model(forSession: key), let kept = model.drafts(for: key) {
            Composer(model: model, key: key, kept: kept)
        }
    }
}

private struct Composer: View {
    let model: AppModel
    let key: String
    /// The run's words and the send whose answer never came, kept by the run (`RunDrafts`): they outlive this box.
    /// The rest of the box (the last outcome, a command's watch) starts again.
    @Bindable var kept: RunDrafts

    /// What became of the last Claude send or refused command; Kit words it.
    @State private var result: ComposeAttempt?
    @State private var retire: Task<Void, Never>?
    @State private var sending = false
    @State private var askingAgain = false
    /// The field has the keyboard: Esc leaves it before it closes the run.
    @FocusState private var fieldFocused: Bool
    /// The Compact command typed last, and where it stands.
    @State private var running: Running?
    @State private var quiet: Task<Void, Never>?
    /// The settings panel open above the reply row, if any.
    @State private var panel: SettingsPanel?
    @State private var compacting = false
    @State private var instructions = ""
    @State private var clearing = false
    /// A mode the run's menu marks `confirm`, chosen and waiting on its confirmation.
    @State private var confirmingMode: MenuMode?
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
    private var blocked: String? { model.sendBlockedReason(for: key, draft: kept.reply) }
    private var computer: String { HostDevice.current.computer("the computer", named: model.machineName) }
    private var busy: Bool { sending || running?.phase.busy == true }

    private var canSend: Bool {
        blocked == nil && !sending && !kept.reply.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let line { note(line) }
            box
        }
        // On the pane's one content column: the conversation's rows and the heading stand 28 pt in.
        .padding(.horizontal, 28)
        .padding(.top, 4)
        .padding(.bottom, 16)
        .onChange(of: kept.reply) { old, new in
            // If the draft has moved off the words it held — the same normalisation the send's mutation key uses, so a
            // whitespace-only edit is not a move — the send kept under those words is no longer this draft's retry: drop
            // its identity, so the same words typed later are a new send.
            model.abandonHeldSend(whenDraftChangesFrom: old, to: new, in: key)
            kept.unanswered = nil
            // Editing is the reader's next act, so a standing failure or kept value has been read. The draft a landed
            // command consumes is not an edit.
            if result?.clearsOnEdit == true { result = nil }
            if !new.isEmpty, case .failed = running?.phase { running = nil }
            if !new.isEmpty, case .neutral = running?.phase { running = nil }
        }
        // A send a live Stop cancelled hands its words back here, but only into an empty reply, so a draft being typed
        // is never overwritten. Taken or not, Kit's offer is consumed, so the words are offered once.
        .onChange(of: model.composerRestore[key]) { _, restore in
            guard let restore else { return }
            if kept.reply.isEmpty { kept.reply = restore }
            model.consumeComposerRestore(for: key)
        }
        // A new handshake is the link back, and so is the box coming back, from another tab or another run: the
        // unanswered send is asked again, if Kit's rule still allows it.
        .onChange(of: model.connection.generation) { _, _ in askUnansweredAgain() }
        .onAppear { askUnansweredAgain() }
        // A run nothing has been said to yet, as one started with no first message, has the cursor in its box once its
        // conversation is in. Not "an empty conversation": a Codex run files its session start before any message.
        .task(id: awaitsFirstMessage) {
            #if DEBUG
                if RenderWindow.leavesFieldUnfocused { return }
            #endif
            if awaitsFirstMessage { fieldFocused = true }
        }
        // Esc closes the open settings panel, then leaves the field, before the window's Esc closes the run. Published
        // to the whole window, not only along the focus: a panel opened by a click leaves nothing in the box focused.
        .focusedSceneValue(\.composerHoldsEscape, panel != nil || fieldFocused)
        // A request that arrives is answered above the box: no settings panel may cover its answers.
        .onChange(of: model.shown(for: key)?.pendingApprovals.first?.id) { _, now in if now != nil { panel = nil } }
        .onChange(of: model.receipts(for: key)?.compactSeq) { _, _ in settle() }
        #if DEBUG
            .task {
                if let typed = RenderWindow.reply, kept.reply.isEmpty { kept.reply = typed }
                if let shown = RenderWindow.panel { panel = shown }
            }
        #endif
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
            Button(ClearConfirmation.clear.titleStyle, role: .destructive) { performClear() }
                .accessibilityIdentifier("session.composer.clear.confirm")
            Button(ClearConfirmation.keep.titleStyle, role: .cancel) {}
                .accessibilityIdentifier("session.composer.clear.cancel")
        } message: {
            Text(ClearConfirmation.message)
        }
        // Kit's words for the run's agent; Keep Asking and Esc send nothing, and the mode stays as it was.
        .alert(
            modeWords.title,
            isPresented: Binding(get: { confirmingMode != nil }, set: { if !$0 { confirmingMode = nil } }),
            presenting: confirmingMode
        ) { mode in
            Button(modeWords.confirm, role: .destructive) {
                takeOpener()
                Task { _ = await model.setMode(for: key, modeValue: mode.value, confirmed: true) }
            }
            .accessibilityIdentifier("session.composer.mode.confirm")
            Button(modeWords.cancel, role: .cancel) {}
                .accessibilityIdentifier("session.composer.mode.cancel")
        } message: { _ in
            Text(modeWords.message)
        }
    }

    private var modeWords: ModeConfirmation.Words { ModeConfirmation.words(for: summary?.agent ?? .claude) }

    // MARK: The box

    private var box: some View {
        let agent = summary?.agent.displayName ?? "the agent"
        return VStack(alignment: .leading, spacing: 10) {
            TextField(
                "", text: $kept.reply, prompt: Text(SessionWords.replyPrompt(agent)).foregroundStyle(Theme.text3), axis: .vertical
            )
                .textFieldStyle(.plain)
                .font(Theme.ui(13))
                .foregroundStyle(Theme.text)
                .lineLimit(1...5)
                .onSubmit(submit)
                .focused($fieldFocused)
                .onExitCommand { if panel != nil { panel = nil } else { fieldFocused = false } }
                .accessibilityLabel(SessionWords.replyPrompt(agent))
                .accessibilityIdentifier("session.composer.field")
            HStack(spacing: 10) {
                if isCodex, let counter = ComposeDraft(text: kept.reply).counterText {
                    Text(counter)
                        .font(Theme.mono(11))
                        .foregroundStyle(ComposeDraft(text: kept.reply).isOverCeiling ? Theme.red : Theme.text3)
                        .accessibilityIdentifier("session.composer.count")
                }
                Spacer()
                // Which model control this run has is Kit's to say: a run started from the app gets its own menus, or
                // its model as a fact when it has none; a run these controls do not own draws none.
                switch model.runSettings(for: key) {
                case .deferToOtherPath:
                    EmptyView()
                case .hostedReadOnly(let fact):
                    ModelFactLabel(agent: summary?.agent ?? .claude, fact: fact)
                case .hostedLive(let settings):
                    HostedSettingsButton(
                        model: model, key: key, agent: summary?.agent ?? .claude, settings: settings,
                        blocked: blocked, open: $panel, chose: takeOpener, compact: { compact(prefill: "") },
                        clear: {
                            opener = nil
                            clearing = true
                        },
                        confirmMode: { confirmingMode = $0 })
                    .disabled(blocked != nil || busy)
                }
                sendButton
            }
        }
        .padding(12)
        // The outline is drawn under the box's contents, not over them: a settings panel opens out of the box and
        // nothing of the box may cross it.
        .background {
            RoundedRectangle(cornerRadius: 10).fill(Theme.raised)
            RoundedRectangle(cornerRadius: 10).strokeBorder(Color.white.opacity(0.09))
        }
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

    /// The command typed last, while it stands and sending can work; else, on a run started from the app, the answer
    /// to its last model, effort or mode change; else what became of the last send (a Codex run's own outcome first, as
    /// Kit ranks it); else why sending cannot work, or that answers are given at the computer.
    /// A command's line never hides why Send is disabled: the watch for its receipt stays armed underneath.
    private var line: Line? {
        if blocked == nil, let running, let status = model.commandStatus(running.command, running.phase) {
            return Line(text: status.text, tone: status.tone)
        }
        // A model, effort or mode call on a run started from the app: its answer, for as long as Kit keeps it
        // (`HostedSettings.lastOutcome`).
        if case .hostedLive(let settings) = model.runSettings(for: key), let outcome = settings.lastOutcome {
            return Line(text: outcome.text, tone: outcome.tone.composeTone)
        }
        if let kit = model.outcomeNote(for: key, lastSend: result) ?? model.standingNote(for: key, draft: kept.reply) {
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
        let draft = kept.reply
        let again = askingAgain
        askingAgain = false
        switch model.submission(of: draft, to: key) {
        case .codex: sendToCodex(draft)
        case .command(let action): route(action, draft: draft)
        case .claude: sendToClaude(draft, again: again)
        }
    }

    /// Typed into Claude's prompt. The draft clears only on a landed send, which shows nothing; a refusal stands
    /// until the text changes, and a recovered composer's note retires by itself.
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
            if attempt.landed, kept.reply == draft { kept.reply = "" }
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
            if outcome?.landed == true, kept.reply == draft { kept.reply = "" }
        }
    }

    /// The run's conversation is in and holds no message of the person's.
    private var awaitsFirstMessage: Bool {
        guard let timeline = model.shown(for: key)?.timeline else { return false }
        return !timeline.contains { if case .userMessage = $0.content { true } else { false } }
    }

    private func askUnansweredAgain() {
        guard let unanswered = kept.unanswered,
            model.asksAgain(unanswered, to: key, draft: kept.reply, sending: sending)
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
    /// open the run's model panel, where the choice is made from the run's own menus.
    private func route(_ action: CommandAction, draft: String) {
        switch action {
        case .nativeModel, .nativeEffort:
            opener = draft
            panel = .model
        case .nativeCompact(let prefill):
            compact(prefill: prefill)
            opener = draft
        case .nativeClear:
            opener = draft
            clearing = true
        case .nativeDiff:
            // Kit's policy refuses it, before the wire, in its own words: they point to the Changes tab.
            Task { noteResult(await model.send(text: draft, to: key)) }
        case .blocked(_, let reason):
            noteResult(.refused(reason))
        case .passThrough:
            break
        }
    }

    /// A choice in a hosted run's list answers the `/model` or `/effort` that opened it: those words leave the box, as a
    /// landed command's do.
    private func takeOpener() {
        if let opener, kept.reply == opener { kept.reply = "" }
        opener = nil
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
            case .compact(let instructions):
                attempt = await model.sendCompactCommand(instructions: instructions, to: key)
            }
            sending = false
            if attempt.landed, let draft, kept.reply == draft { kept.reply = "" }
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

    /// `/clear`, past its confirmation. A landed send says nothing; the conversation shows the clear.
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
            if attempt.landed, let draft, kept.reply == draft { kept.reply = "" }
            noteResult(attempt)
        }
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
