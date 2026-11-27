import CodeConnectKit
import SwiftUI

/// The reply box at the foot of a run's conversation: what became of the last thing sent, or why sending cannot work,
/// over the words and what answers them. Where the words go is Kit's to say (`AppModel.submission`): a Codex run's own
/// message, Claude's typed text, or a slash command this box answers with its own control. A run that takes no reply
/// (Kit's `takesNoReply`) has no box: one quiet line in its place says it has ended, and where the run can be picked up
/// at the computer, the command that does it stands above that line.
struct ComposerBar: View {
    let key: String
    /// Opens the run's changes panel, as the header's Changes does: the box's /diff.
    let showsChanges: () -> Void
    @Environment(Machines.self) private var machines

    var body: some View {
        if let model = machines.model(forSession: key) {
            if let summary = model.shownSummary(for: key), summary.takesNoReply {
                if let command = summary.resumeCommand { ResumeCommand(command: command).onConversationColumn() }
                EndedLine(model: model, key: key, summary: summary)
            } else if let kept = model.drafts(for: key) {
                Composer(model: model, key: key, kept: kept, showsChanges: showsChanges).onConversationColumn()
            }
        }
    }
}

/// In an ended run's reply box's place: the pane's foot band, under its hairline; the mark and words on the
/// conversation's notice columns, and under its words the agent the run was, with its model and the effort it ended
/// with, as far as the run said them.
private struct EndedLine: View {
    let model: AppModel
    let key: String
    let summary: SessionSummary

    var body: some View {
        VStack(spacing: 0) {
            Rectangle().fill(Theme.hairline).frame(height: 1)
            HStack(alignment: .firstTextBaseline, spacing: 7) {
                Image(systemName: "checkmark.circle").font(.system(size: 11)).foregroundStyle(Theme.text3)
                    .frame(width: 14)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 2) {
                    Text(SessionWords.runEnded)
                    agentLine
                }
                .font(Theme.ui(12))
                .foregroundStyle(Theme.text3)
                .padding(.vertical, 8)
            }
            .padding(.horizontal, 28)
            .frame(minHeight: 34)
            .onConversationColumn()
        }
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("session.endedLine")
    }

    /// The agent, and the model it said it ran when it said: by the label of the run's menu entry for that model when
    /// there is one, so the line and the settings list spell it alike, else as the agent reported it, in code type
    /// (Kit's `modelName`); then the effort it ended with (Kit's `headerEffort`), with or without a model said.
    private var agentLine: Text {
        var line = Text(summary.agent.productName)
        if let confirmed = model.confirmedModel(for: key) {
            let name = RunSettings.modelName(confirmed: confirmed.name, menus: summary.menus, label: summary.modelLabel)
            line = Text("\(line) · \(name.reported ? Text(name.text).font(Theme.mono(11.5)) : Text(name.text))")
        }
        if let effort = RunSettings.headerEffort(summary: summary) { line = Text("\(line) · \(effort)") }
        return line
    }
}

/// Over an ended run's foot band, on the conversation's column: the command that picks the conversation up at the
/// computer (Kit's `resumeCommand`), drawn whole, selectable and copied whole by its button, as the iPhone offers it.
private struct ResumeCommand: View {
    let command: String

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(SessionWords.pickUpAtComputer).font(Theme.ui(12)).foregroundStyle(Theme.text2)
            // Whole, wrapping where it must: a cut would hide what runs.
            Text(command).font(Theme.mono(12)).foregroundStyle(Theme.text)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
                .padding(.leading, 10)
                .padding(.trailing, CopyButton.slot)
                .padding(.vertical, 6)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Theme.code, in: RoundedRectangle(cornerRadius: 6))
                .accessibilityIdentifier("session.resume.command")
                .overlay(alignment: .trailing) { CopyButton(text: command, id: "session.resume.copy") }
        }
        .padding(.horizontal, 28)
        .padding(.bottom, 12)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("session.resume")
    }
}

private struct Composer: View {
    let model: AppModel
    let key: String
    /// The run's words, the send whose answer never came, a send on its way, the last outcome and a command's watch,
    /// kept by the run (`RunDrafts`): they outlive this box. The rest of the box (its panels, its palette) starts again.
    @Bindable var kept: RunDrafts
    let showsChanges: () -> Void

    /// What became of the last Claude send or refused command; Kit words it. The run's (`RunDrafts.lastSend`).
    private var result: ComposeAttempt? {
        get { kept.lastSend }
        nonmutating set { kept.lastSend = newValue }
    }
    @State private var retire: Task<Void, Never>?
    /// A send or command is on its way. The run's (`RunDrafts.sending`).
    private var sending: Bool {
        get { kept.sending }
        nonmutating set { kept.sending = newValue }
    }
    @State private var askingAgain = false
    /// The field has the keyboard: Esc leaves it before it closes the run.
    @FocusState private var fieldFocused: Bool
    /// The field's caret, which the box sets when the app gives it the keyboard back (`takeKeyboardBack`).
    @State private var selection: TextSelection?
    @Environment(\.replyFocusRequest) private var replyFocusRequest
    @Environment(\.agentsShown) private var agentsShown
    @Environment(\.filesDragged) private var filesDragged
    /// The Compact command typed last, and where it stands. The run's (`RunDrafts.command`).
    private var running: Running? {
        get { kept.command }
        nonmutating set { kept.command = newValue }
    }
    @State private var quiet: Task<Void, Never>?
    /// The settings panel open above the box's bar, if any.
    @State private var panel: SettingsPanel?
    /// The box's content width, between its bar's insets: no panel is wider.
    @State private var rowWidth: CGFloat = 0
    /// Where the run's settings controls stand in the box, which places their panels inside its content.
    @State private var controls: CGRect = .zero
    private nonisolated static let space = "composer.box"
    @State private var compacting = false
    @State private var instructions = ""
    @State private var clearing = false
    /// A mode the run's menu marks `confirm`, chosen and waiting on its confirmation.
    @State private var confirmingMode: MenuMode?
    /// The draft that opened the list, the dialog or the confirmation: a command that lands consumes it, if it is
    /// still the draft.
    @State private var opener: String?
    /// The slash palette's lit row, the first whenever it shows.
    @State private var slashLit = 0
    /// Whether the slash palette is closed on the draft as it stands (Kit's `SlashPalette.Latch`).
    @State private var slashLatch = SlashPalette.Latch()
    @State private var slashHeight: CGFloat = 0
    /// The pointer is on the usage figures, and their card shows over the bar.
    @State private var usageHovered = false

    private typealias Running = CommandWatch

    private var summary: SessionSummary? { model.shownSummary(for: key) }
    private var isCodex: Bool { summary?.isCodex == true }
    private var blocked: String? { model.sendBlockedReason(for: key, draft: kept.reply) }
    /// Why the run cannot be talked to now: what blocks a send, except the files, which hold back only the message.
    private var runBlocked: String? { blocked == model.filesBlockedReason(for: key) ? nil : blocked }
    private var computer: String { HostDevice.current.computer("the computer", named: model.machineName) }
    private var busy: Bool { sending || running?.phase.busy == true }

    /// The slash palette, on a Claude or Antigravity run while the box has the keyboard and no settings panel is open (Kit's
    /// `SlashPalette`: the commands this app answers itself, as the iPhone offers them). A Codex run has no slash
    /// vocabulary.
    private var slash: SlashPalette.Content? {
        slashLatch.content(for: kept.reply, agent: summary?.agent, focused: fieldFocused && panel == nil)
    }

    /// A palette row, as the typed command would go: the same `route`, then the palette stays closed.
    private func runSlash(_ row: SlashRow) {
        slashLatch.close()
        route(row.action, draft: kept.reply)
    }

    /// Words, or files alone: either is a message.
    private var canSend: Bool {
        blocked == nil && !sending
            && (!kept.reply.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !model.files(for: key).isEmpty)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if compacting {
                CompactCard(instructions: $instructions, agent: summary?.agent ?? .claude) {
                    compacting = false
                    run(.compact(instructions: instructions))
                } cancel: {
                    compacting = false
                }
            } else {
                if let line { note(line) }
                box
            }
        }
        // Docked under the conversation, on its column's 28 pt sides and 20 pt above the pane's foot; 16 pt under the
        // conversation's last line at its end (its row's 7 pt inset and the column's 1 pt end, then these 8).
        .padding(.horizontal, 28)
        .padding(.top, 8)
        .padding(.bottom, 20)
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
        // is never overwritten; offered while no box showed the run, they are taken as one appears. Taken or not, Kit's
        // offer is consumed, so the words are offered once.
        .onChange(of: model.composerRestore[key], initial: true) { _, restore in
            guard let restore else { return }
            if kept.reply.isEmpty { kept.reply = restore }
            model.consumeComposerRestore(for: key)
        }
        // A new handshake is the link back, and so is the box coming back, from another tab or another run: the
        // unanswered send is asked again, if Kit's rule still allows it.
        .onChange(of: model.connection.generation) { _, _ in askUnansweredAgain() }
        .onAppear {
            askUnansweredAgain()
            // A command still watched takes a receipt that came while no box showed the run. Its caption's timers, and
            // a landed note's, run on in the box that started them, each acting only on its own value.
            settle()
        }
        // A run nothing has been said to yet, as one started with no first message, has the cursor in its box once its
        // conversation is in. Not "an empty conversation": a Codex run files its session start before any message.
        .task(id: awaitsFirstMessage) {
            #if DEBUG
                if RenderWindow.leavesFieldUnfocused { return }
            #endif
            if awaitsFirstMessage { fieldFocused = true }
        }
        // Return in the run list asks for the reply box.
        .onChange(of: replyFocusRequest) { takeKeyboardBack() }
        // Back from Settings, the box has the keyboard again.
        .onChange(of: agentsShown) { _, shown in if shown { takeKeyboardBack() } }
        // Esc closes the open settings panel, then leaves the field, before the window's Esc closes the run. Published
        // to the whole window, not only along the focus: a panel opened by a click leaves nothing in the box focused.
        .focusedSceneValue(\.composerHoldsEscape, panel != nil || fieldFocused)
        // A request that arrives is answered above the box: no settings panel may cover its answers.
        .onChange(of: model.shown(for: key)?.pendingApprovals.first?.id) { _, now in if now != nil { panel = nil } }
        // Claude Code's receipt settles a compact; on Antigravity, the turn agy runs `/compact` in ending does.
        .onChange(of: model.receipts(for: key).map { [$0.compactSeq, $0.turnEndSeq] }) { _, _ in settle() }
        #if DEBUG
            .task {
                if let typed = RenderWindow.reply, kept.reply.isEmpty { kept.reply = typed }
                model.seedFixtureFiles(key)
                if !RenderWindow.attachedFiles.isEmpty, RenderWindow.opened == key {
                    for _ in 0..<100 where !model.canUpload(key) { try? await Task.sleep(for: .milliseconds(100)) }
                    FileIntake.add(RenderWindow.attachedFiles, takeFile)
                }
                // A render of the slash palette: the box holds the keyboard, as a person typing "/" there has it, once
                // the render has sized its window (a resize closes an open panel), and `-CC_SLASH_LIT <row>` lights a
                // row, as ↓ would.
                if RenderWindow.reply?.hasPrefix("/") == true {
                    try? await Task.sleep(for: .seconds(1))
                    slashLatch.edited()
                    fieldFocused = true
                    try? await Task.sleep(for: .milliseconds(300))
                    slashLit = RenderWindow.slashLit
                }
                // Opened once the render has sized its window: a resize closes an open panel.
                if let shown = RenderWindow.panel {
                    try? await Task.sleep(for: .seconds(1))
                    panel = shown
                }
            }
        #endif
        .confirmation(
            Binding(get: { clearing ? true : nil }, set: { clearing = $0 ?? false }), returnsKeyboard: takeKeyboardBack
        ) { _ in
            Confirmation(
                ClearConfirmation.ask, cancelID: "session.composer.clear.cancel",
                confirmID: "session.composer.clear.confirm", perform: performClear)
        }
        // Kit's words for the run's agent; Keep Asking and Esc send nothing, and the mode stays as it was.
        .confirmation($confirmingMode, returnsKeyboard: takeKeyboardBack) { mode in
            Confirmation(
                ModeConfirmation.words(for: summary?.agent ?? .claude), cancelID: "session.composer.mode.cancel",
                confirmID: "session.composer.mode.confirm"
            ) {
                takeOpener()
                Task { _ = await model.setMode(for: key, modeValue: mode.value, confirmed: true) }
            }
        }
    }

    /// A file for the next message, to the run's tray (Kit's `attach`).
    private func takeFile(_ name: String, _ size: Int, _ read: () throws -> Data) {
        model.attach(name: name, size: size, to: key, read: read)
    }

    // MARK: The box

    /// The reply field's prompt: at a usage limit, when a message goes through again; while the run's computer is
    /// offline, that it is and when a reply can go.
    private func prompt(_ agent: String) -> String {
        model.limitFoot(for: key, now: .now)?.prompt ?? model.offlineReply(for: key, draft: kept.reply)
            ?? SessionWords.replyPrompt(agent)
    }

    private var box: some View {
        let agent = summary?.agent.displayName ?? "the agent"
        return InputBox(barHeight: 46) {
            // The files going with the message stand above its words.
            VStack(alignment: .leading, spacing: 12) {
                if !model.files(for: key).isEmpty {
                    FileTray(
                        files: model.files(for: key), reason: model.filesBlockedReason(for: key),
                        remove: { model.removeFile($0, from: key) }, retry: { model.retryFile($0, in: key) })
                }
                TextField(
                    "", text: $kept.reply, selection: $selection,
                    prompt: Text(prompt(agent)).foregroundStyle(Theme.text3), axis: .vertical
                )
                    .textFieldStyle(.plain)
                    .font(Theme.ui(15))
                    .lineSpacing(InputBoxLayout.lineSpacing)
                    .foregroundStyle(Theme.text)
                    .lineLimit(1...5)
                    .accessibilityLabel(prompt(agent))
                    .accessibilityIdentifier("session.composer.field")
                    // Inside `sendsOnReturn`: over the palette, ↑ ↓ move its light and Return runs the lit row; anything
                    // the palette leaves (⇧/⌥Return, no palette) goes on to the box. ⌘ and ⌃ Return are never the palette's.
                    .onKeyPress(keys: [.upArrow, .downArrow, .return], phases: .down) { press in
                        guard let slash, !press.modifiers.contains(.command), !press.modifiers.contains(.control) else {
                            return .ignored
                        }
                        let key: SlashPalette.Key =
                            switch press.key {
                            case .upArrow: .up
                            case .downArrow: .down
                            default: .return(addsLine: press.modifiers.contains(.shift) || press.modifiers.contains(.option))
                            }
                        switch SlashPalette.outcome(of: key, rows: slash.rows.count, highlighted: slashLit) {
                        case .highlight(let row): slashLit = row
                        case .run(let row): runSlash(slash.rows[row])
                        case .close: slashLatch.close()
                        case .addLine, .toBox: return .ignored
                        }
                        return .handled
                    }
                    .sendsOnReturn(submit)
                    .focused($fieldFocused)
                    .onExitCommand { if panel != nil { panel = nil } else { fieldFocused = false } }
            }
        } leading: {
            if model.canUpload(key) { AttachButton(id: "session.composer.attach", take: takeFile) }
            // Which model control this run has is Kit's to say: a run started from the app gets its own menus, or
            // its model as a fact when it has none; a run these controls do not own draws none.
            switch model.runSettings(for: key) {
            case .deferToOtherPath:
                EmptyView()
            case .hostedReadOnly(let fact):
                ModelFactLabel(
                    agent: summary?.agent ?? .claude, fact: fact, effort: summary.flatMap(RunSettings.effortName))
                .yields(0)
            case .hostedLive(let settings):
                HostedSettingsButton(
                    model: model, key: key, agent: summary?.agent ?? .claude, settings: settings,
                    blocked: runBlocked, open: $panel, chose: takeOpener, compact: { compact(prefill: "") },
                    clear: {
                        opener = nil
                        clearing = true
                    },
                    confirmMode: { confirmingMode = $0 },
                    box: CGRect(x: InputBoxLayout.barInset - controls.minX, y: 0, width: rowWidth, height: 0))
                .disabled(runBlocked != nil || busy)
                .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(Self.space)) } action: { controls = $0 }
                #if DEBUG
                    .task {
                        switch RenderWindow.confirm {
                        case "clear": clearing = true
                        case "mode": confirmingMode = settings.modes.first(where: \.confirm)
                        default: break
                        }
                    }
                #endif
                // Outermost: a layout value set inside `onGeometryChange` does not reach the bar's layout.
                .yields(0)
            }
            if isCodex, let counter = ComposeDraft(text: kept.reply).counterText {
                Text(counter)
                    .font(Theme.mono(11))
                    .foregroundStyle(ComposeDraft(text: kept.reply).isOverCeiling ? Theme.red : Theme.text3)
                    .fixedSize()
                    .accessibilityIdentifier("session.composer.count")
            }
        } trailing: {
            ReplyUsage(model: model, key: key, hovered: $usageHovered)
            sendButton
        }
        .coordinateSpace(.named(Self.space))
        // Files or a picture pasted into it go with the message, where the computer takes them (a drop lands on the
        // whole conversation: `SessionPane`).
        .pastesFiles(model.canUpload(key) && fieldFocused, takeFile)
        .environment(\.filesOver, filesDragged && model.canUpload(key))
        .onGeometryChange(for: CGFloat.self) { $0.size.width - 2 * InputBoxLayout.barInset } action: { rowWidth = $0 }
        // The slash palette above the box at its leading edge, clear of it by the panels' one gap, 420 pt wide.
        .overlay(alignment: .topLeading) {
            if let slash {
                SlashPanel(
                    content: slash, lit: min(slashLit, slash.rows.count - 1), light: { slashLit = $0 }, run: runSlash)
                    .frame(width: 420)
                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { slashHeight = $0 }
                    .openPanel()
                    .offset(y: -(slashHeight + Theme.panelGap))
            }
        }
        // Around the box and its palette: a click outside or Esc closes the palette and keeps the words.
        .closesPanel(Binding(get: { slash != nil }, set: { if !$0 { slashLatch.close() } }))
        .onChange(of: kept.reply) {
            slashLit = 0
            slashLatch.edited()
        }
    }

    /// Send, or while a turn runs Stop (`StopTurnButton`), as chat apps do. Return still sends what is typed: a Codex
    /// run takes it into the running turn, a Claude run takes it once the turn ends.
    @ViewBuilder private var sendButton: some View {
        if model.stopUnavailable(for: key) == nil {
            StopTurnButton(model: model, key: key)
        } else {
            plainSendButton
        }
    }

    private var plainSendButton: some View {
        InputBoxSubmit(
            enabled: canSend, id: "session.composer.send", label: "Send",
            help: blocked ?? (sending ? "Sending…" : "Send"), action: submit)
    }

    // MARK: The line above

    private struct Line {
        let text: String
        let tone: ComposeProse.Tone
    }

    /// The command typed last, while it stands and sending can work; else, on a run started from the app, the answer
    /// to its last model, effort or mode change; else what became of the last send (a Codex run's own outcome first, as
    /// Kit ranks it); else why sending cannot work, or that the run can only be observed, and why.
    /// A command's line never hides why Send is disabled: the watch for its receipt stays armed underneath.
    private var line: Line? {
        if blocked == nil, let running,
            let status = model.commandStatus(running.command, running.phase, agent: summary?.agent ?? .claude)
        {
            return Line(text: status.text, tone: status.tone)
        }
        // A model, effort or mode call on a run started from the app: its answer, for as long as Kit keeps it
        // (`HostedSettings.lastOutcome`).
        if case .hostedLive(let settings) = model.runSettings(for: key), let outcome = settings.lastOutcome {
            return Line(text: outcome.text, tone: outcome.tone.composeTone)
        }
        // Why the files hold the message back stands under them, not here as well; an offline computer is said in the
        // field's prompt.
        let standing =
            blocked != nil && runBlocked == nil || model.offlineReply(for: key, draft: kept.reply) != nil
            ? nil : model.standingNote(for: key, draft: kept.reply)
        if let kit = model.outcomeNote(for: key, lastSend: result) ?? standing {
            return Line(text: kit.text, tone: kit.tone)
        }
        return nil
    }

    private func note(_ line: Line) -> some View {
        KitNote(text: line.text, tone: line.tone, id: "session.composer.note")
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
            let attempt = await model.send(text: draft, to: key, files: true)
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
            let outcome = await model.composeToCodex(sessionKey: key, text: draft, files: true)
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
            // Only its own: a box no longer shown still runs this, and a later outcome is another box's to retire.
            guard !Task.isCancelled, result == attempt else { return }
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
            // The run's own changes panel, as the iPhone opens its Changes: nothing reaches the computer, and the words
            // that asked for it leave the box, typed whole or a part with the row chosen.
            kept.reply = ""
            showsChanges()
        case .blocked(_, let reason):
            noteResult(.refused(reason))
        case .passThrough:
            break
        }
    }

    /// The keyboard back to the box from where the app sent it (an ask, the run list's Return), with the caret at the end
    /// of the words: never the select-all AppKit gives a field handed the keyboard by code, which the next key would
    /// replace. Not where the caret was: this field's selection binding does not report the caret, so where it was
    /// cannot be known. The caret goes in a turn after the keyboard, once that select-all is done.
    private func takeKeyboardBack() { giveKeyboard($fieldFocused, caret: $selection, endOf: kept.reply) }

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
                ?? model.receiptPhase(for: command, in: key, after: baseline, agent: summary?.agent ?? .claude)
                ?? .waiting(request: command.request, quietly: false)
            show(Running(command: command, phase: phase, baseline: baseline))
        }
    }

    /// A receipt above the baseline settles a command still waiting for one, however late it comes.
    private func settle() {
        guard let running, case .waiting = running.phase,
            let settled = model.receiptPhase(
                for: running.command, in: key, after: running.baseline, agent: summary?.agent ?? .claude)
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

/// Compact in the reply box's place: what it does, then the box, its field the optional instructions and its bar Cancel
/// (Esc) and Compact (⌘↩). Nothing is typed until Compact.
private struct CompactCard: View {
    @Binding var instructions: String
    /// The run's agent, whom the compaction is asked of.
    let agent: AgentKind
    let run: () -> Void
    let cancel: () -> Void
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 8) {
                    Image(systemName: PanelSymbol.compact).font(.system(size: 12)).foregroundStyle(Theme.text2)
                        .accessibilityHidden(true)
                    Text(CompactPrompt.title).font(Theme.ui(12.5, .medium)).foregroundStyle(Theme.text)
                }
                Text(CompactPrompt.explanation(for: agent))
                    .font(Theme.ui(12.5))
                    .foregroundStyle(Theme.text2)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.horizontal, 4)
            InputBox(barHeight: 46) {
                TextField(
                    "", text: $instructions,
                    prompt: Text(CompactPrompt.placeholder(for: agent)).foregroundStyle(Theme.text3), axis: .vertical
                )
                .textFieldStyle(.plain)
                .font(Theme.ui(15))
                .lineSpacing(InputBoxLayout.lineSpacing)
                .foregroundStyle(Theme.text)
                .lineLimit(1...5)
                .takesKeyboardWhenShown($focused)
                // Esc cancels here, before it would close the run.
                .onExitCommand(perform: cancel)
                .focusedValue(\.composerHoldsEscape, focused ? true : nil)
                .accessibilityLabel(CompactPrompt.fieldLabel)
                .accessibilityIdentifier("session.composer.compact.instructions")
            } leading: {
                EmptyView()
            } trailing: {
                ActionButton(title: "Cancel", key: "Esc", prominent: false, id: "session.composer.compact.cancel", action: cancel)
                ActionButton(title: CompactPrompt.action, key: "⌘↩", prominent: true, id: "session.composer.compact.run", action: run)
                    .keyboardShortcut(.return, modifiers: .command)
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("session.composer.compact")
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
