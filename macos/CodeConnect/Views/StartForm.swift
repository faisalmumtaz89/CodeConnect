import CodeConnectKit
import SwiftUI

/// Start an agent on one of the computers that can start one: a recent folder there, or any folder (on this Mac from
/// the standard folder panel, elsewhere from `FolderBrowser`), where in its repository (`BranchRow`), the first
/// message, and in its box the model, whose menu holds Claude Code's and Codex's: choosing a model chooses the agent.
/// What each choice offers and says is Kit's (`StartContext`), as on the iPhone; the Mac shows it as one form, in the
/// window's main area while no run is open (`StartPane`). The computer defaults to the one used last, the model to the
/// Default of the agent last used in the folder, else of the first one there. Once sent, the form gives way to the
/// start's progress, and the run opens when it is in the list. The sent start is followed by its request on its own
/// computer, so it stays while that computer is away; until it is answered or its retries are stopped, the form opens
/// on it again.
struct StartForm: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup
    /// The run is in the list, by its id: the window opens it.
    let started: (String) -> Void
    /// Add a computer, from the computer menu.
    let addComputer: () -> Void
    /// Why no computer can start one now, said in the form while its computer's link is down.
    let whyNone: String

    /// The window's start: what is chosen on each computer, the first message, and the start sent. The form keeps
    /// none of it, so it is the same start whenever and however the form is drawn again.
    @Bindable var draft: StartDraft
    /// The computer button's panel is open.
    @State private var computerMenuOpen = false
    @State private var menuOpen = false
    @State private var branchMenuOpen = false
    /// The Folder box browses the computer's folders in place of its recent ones.
    @State private var browsing = false
    @Environment(\.sheetRoom) private var room
    /// The folder button's panel is open.
    @State private var folderOpen = false
    /// Where the box stands in `StartForm.space`, which its panels stay inside.
    @State private var box: CGRect = .zero
    /// The folder chip in `StartForm.space`: its panel starts at its leading edge when it fits, and under it.
    @State private var folderChip: CGRect = .zero
    /// How tall the folder panel stands, and its list in it, as drawn.
    @State private var folderPanelHeight: CGFloat = 0
    @State private var listShown: CGFloat = 0
    @Environment(\.startHeight) private var startHeight
    /// The first message has the keyboard: as the form appears, and when New Agent asks for it.
    @FocusState private var messageFocused: Bool
    /// The first message's caret, which the form sets when New Agent gives it the keyboard (`giveKeyboard`).
    @State private var messageSelection: TextSelection?
    /// An ask is up (New Folder's name): Start's keys are its, not Start's.
    @Environment(ConfirmationQueue.self) private var asks
    /// This Mac's daemon's id (`LocalDaemon.machineID`): the computer whose daemon said that id when it connected is
    /// this Mac. Nil without one, as in a test launch with no rig of its own.
    @State private var thisMac: String?
    /// The search field has the keyboard, and with it the folders under it.
    @FocusState private var searchFocused: Bool
    /// The folder list's rows, measured: the list is as tall as they are, up to its most (`listMost`).
    @State private var listHeight: CGFloat = 0

    private var contexts: [StartContext] { machines.startContexts }

    /// The chosen computer, else the one used last.
    private var context: StartContext? {
        contexts.first { $0.id == draft.computerID }
            ?? contexts.max { ($0.lastUsed ?? .distantPast) < ($1.lastUsed ?? .distantPast) }
    }

    /// The chosen computer's choices, from the draft.
    private var choices: StartDraft.Choices { context.map { draft.choices(on: $0.id) } ?? StartDraft.Choices() }
    private var query: String { choices.query }
    private var folderPath: String? { choices.folderPath }
    /// The place chosen, else the folder as it is.
    private var place: StartPlace { choices.place ?? .folder }
    private var newName: String? { choices.newName }

    /// One of the chosen computer's choices, as a binding the form's controls write to the draft through.
    private func choice<Value>(_ path: WritableKeyPath<StartDraft.Choices, Value>) -> Binding<Value> {
        Binding(
            get: { choices[keyPath: path] },
            set: { value in if let context { draft.edit(on: context.id) { $0[keyPath: path] = value } } })
    }

    /// The sent start, found by its request on whichever computer it was sent to, not through the computers that can
    /// start right now, which its own leaves while its link is down.
    private var sentStart: (start: SessionStart, owner: AppModel)? {
        guard let id = draft.sent?.requestID,
            let owner = machines.models.first(where: { $0.starter.starts[id] != nil }),
            let start = owner.starter.starts[id]
        else { return nil }
        return (start, owner)
    }

    /// What the form asks now, once it can ask anything.
    private var ask: StartDraft.Ask? {
        guard let context, let folder, let pick else { return nil }
        return StartDraft.Ask(
            computerID: context.id, folderPath: folder.path, place: startPlace, agent: pick.agent, model: pick.model,
            message: draft.message, files: draft.files.map(\.id))
    }

    /// The draft's page: the sent start found on its computer, or let go when that computer is gone.
    private var phase: StartDraft.Phase {
        draft.phase(outcome: sentStart?.start.outcome, asking: ask, computerKnown: sentStart != nil)
    }

    /// Start can be pressed: the form is composing, nothing blocks it, no ask is up and none of its panels is open.
    private var startable: Bool {
        phase == .composing && blocked == nil && !asks.isAsking
            && !folderOpen && !computerMenuOpen && !menuOpen && !branchMenuOpen
    }

    /// The selected folder: a recent one when it is one, else the one chosen or browsed to.
    private var folder: StartProject? {
        guard let folderPath, let context else { return nil }
        return context.project(at: folderPath)
    }

    private func isThisMac(_ context: StartContext) -> Bool {
        thisMac != nil && machines.model(for: context.id)?.connection.helloAck?.machineID == thisMac
    }

    /// The chosen model while the computer still offers it, else Kit's Default for the folder.
    private var pick: StartModelChoice? {
        guard let context else { return nil }
        if let chosen = choices.model, context.modelChoices.contains(chosen) { return chosen }
        return context.defaultChoice(for: folder)
    }

    /// Why Start can't be pressed yet, or nil when it can.
    private var blocked: String? {
        guard let context else { return "Choose a computer." }
        if context.startAgents.contains(where: { context.models($0) == .checking }) { return context.checkingModels }
        guard pick != nil else { return context.noAgent }
        guard folder != nil else { return "Choose a folder." }
        // Files alone are a first message.
        if draft.message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, draft.files.isEmpty {
            return StartWords.writeTheFirstMessage
        }
        if let reason = draft.filesBlockedReason { return reason }
        if !draft.files.isEmpty, !context.takesUploads { return "\(context.machineName) can’t take files." }
        if let name = newName?.trimmingCharacters(in: .whitespaces) {
            if name.isEmpty { return StartWords.nameTheWorktree }
            // The rule is drawn in the branch menu, where the name is typed; this says it in Start's tooltip.
            if !StartPlace.isValidName(name) { return StartWords.worktreeNameBroken }
        }
        return nil
    }

    /// The window has less room than the tallest form needs (under a notice): after the folder list, the heading goes,
    /// the first message starts at one line and the rows stand closer.
    private var short: Bool { room < 600 }

    /// The place Start sends: a new worktree by the name typed, else the one chosen.
    private var startPlace: StartPlace {
        newName.map { .new($0.trimmingCharacters(in: .whitespaces)) } ?? place
    }

    var body: some View {
        Group {
            // The draft's page: the form while composing, else the sent start's page in the form's column, as tall as
            // its content.
            if let sentStart, phase != .composing {
                sent(sentStart.start, on: sentStart.owner)
                    .frame(maxWidth: Self.column)
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                form
            }
        }
        .task(id: contexts.map(\.id)) { contexts.forEach { $0.load() } }
        // Another folder, or the same path on another computer, has its repository read, as the iPhone's folder step
        // does. Where in it the start goes is the draft's: only the person's own pick of a folder resets it.
        .task(id: [context?.id ?? "", folderPath ?? ""]) {
            branchMenuOpen = false
            guard let context, let folder, context.asksGit else { return }
            context.askGit(folder.path)
            #if DEBUG
                var place = place
                var newName = newName
                RenderWindow.branch(in: context.git(folder.path) ?? nil, place: &place, newName: &newName)
                draft.edit(on: context.id) {
                    $0.place = place
                    $0.newName = newName
                }
                // Opened once the render has sized its window: a resize closes an open panel.
                guard RenderWindow.opensBranchMenu else { return }
                try? await Task.sleep(for: .seconds(1))
                branchMenuOpen = true
            #endif
        }
        .task { thisMac = setup.installer.flatMap { LocalDaemon.machineID(root: $0.root) } }
        #if DEBUG
            .task {
                // Opened once the render has sized its window: a resize closes an open panel.
                guard RenderWindow.opensComputerPicker else { return }
                try? await Task.sleep(for: .seconds(1))
                computerMenuOpen = true
            }
            .task {
                // Opened once the render has sized its window: a resize closes an open panel.
                guard RenderWindow.opensModelMenu else { return }
                try? await Task.sleep(for: .seconds(1))
                menuOpen = true
            }
            .task {
                browsing = RenderWindow.browsed != nil
                if let words = RenderWindow.startMessage { draft.message = words }
                if browsing || RenderWindow.opensFolderPanel {
                    // Opened once the render has sized its window: a resize closes an open panel.
                    try? await Task.sleep(for: .seconds(2))
                    folderOpen = true
                }
                if let path = RenderWindow.chosenFolder {
                    for _ in 0..<150 {
                        if let context, context.projects.contains(where: { $0.path == path }) {
                            if folderPath != path { draft.chooseFolder(path, on: context.id) }
                            break
                        }
                        try? await Task.sleep(for: .milliseconds(200))
                    }
                }
                if !RenderWindow.attachedFiles.isEmpty {
                    for _ in 0..<100 where context?.takesUploads != true { try? await Task.sleep(for: .milliseconds(100)) }
                    FileIntake.add(RenderWindow.attachedFiles, draft.attach)
                }
                guard RenderWindow.startSends else { return }
                for _ in 0..<150 where blocked != nil { try? await Task.sleep(for: .milliseconds(200)) }
                start()
            }
        #endif
        // Another computer browses from its own home.
        .onChange(of: context?.id) { browsing = false }
        // The computer picker is drawn only while there is more than one computer: when one goes away, it is closed,
        // not left open unseen with Start waiting on it.
        .onChange(of: contexts.count) { _, count in if count < 2 { computerMenuOpen = false } }
        // New Agent: the first message takes the keyboard, a turn after the form draws, and no panel stays open. Taken
        // once, whether the form was there before the request or comes with it.
        .onAppear { takeKeyboard(formShown: false) }
        .onChange(of: draft.wantsKeyboard) { takeKeyboard(formShown: true) }
    }

    /// A sent start: the folder question when its computer asked it, else its progress.
    @ViewBuilder private func sent(_ start: SessionStart, on owner: AppModel) -> some View {
        if case .needsTrust(let path) = start.outcome {
            FolderStep(
                start: start, path: path, computer: start.machineName,
                answer: { answer(start, $0, in: StartContext(model: owner)) }, back: { backToForm(from: start) })
        } else {
            StartProgress(
                start: start,
                shown: { owner.shown(for: $0) != nil },
                again: { backToForm(from: start) },
                stopRetrying: {
                    owner.starter.stopRetrying(start.requestID)
                    draft.withdraw()
                },
                close: { backToForm(from: start) },
                started: { started($0) })
        }
    }

    /// The box: the first message, and in its bar the computer, the folder and where in the repository, the model and
    /// Start. What is still missing shows where it is chosen (a folder not chosen is the bar's lit chip, the message
    /// its placeholder), and why Start can't be pressed is Start's tooltip.
    private var form: some View {
        VStack(alignment: .leading, spacing: 10) {
            if let context {
                AgentNotice(context: context, elsewhere: !isThisMac(context))
                composer(context)
                    // Its menus open over what is above and below it: the last modifier, so they draw over the rest.
                    .zIndex(folderOpen || computerMenuOpen || branchMenuOpen || menuOpen ? 1 : 0)
            } else {
                // Its computer's link is down: why, where the box stands. What was typed and chosen stays in the
                // form, and the box comes back with the computer.
                Text(whyNone)
                    .font(Theme.ui(13.5))
                    .foregroundStyle(Theme.text2)
                    .lineSpacing(3)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .frame(maxWidth: Self.column, alignment: .leading)
    }

    /// New Agent's request for the keyboard, taken once: the panels close and the first message has it a turn later.
    /// Not as the form merely appears: a run closed from the list keeps the list's keys.
    /// `formShown`: the form was on screen before the request; else this press draws it, and its first appearance asks
    /// the computers (`.task(id: contexts…)`).
    private func takeKeyboard(formShown: Bool) {
        guard draft.takeKeyboardRequest() else { return }
        // A form already open asks each computer again what it offers (`StartDraft.asksAgain`): a Claude Code signed
        // out or removed while it stayed open is no longer offered.
        if draft.asksAgain(formShown: formShown) { contexts.forEach { $0.load() } }
        folderOpen = false
        computerMenuOpen = false
        menuOpen = false
        branchMenuOpen = false
        DispatchQueue.main.async { giveKeyboard($messageFocused, caret: $messageSelection, endOf: draft.message) }
    }

    /// The form's own coordinates, top-left origin, in which a menu inside it finds the room it has.
    nonisolated static let space = "start.form"

    /// The column the form stands in, as wide as a run's reply box reads.
    static let column: CGFloat = 720

    /// Start: the withdrawn start again under its own id when the form asks exactly what it asked, else a new start
    /// (`StartDraft.send`), so one ask never runs twice.
    private func start() {
        guard startable, let context, let folder, let pick, let ask else { return }
        switch draft.send(for: ask, outcome: sentStart?.start.outcome, computerKnown: sentStart != nil) {
        case .resume(let requestID):
            context.resume(requestID)
            draft.didResume()
        case .new:
            let requestID = context.start(
                folder, place: startPlace, agent: pick.agent, model: pick.model, prompt: ask.message, files: draft.files)
            draft.didSend(requestID, asking: ask)
        }
    }

    /// Back to the form from a start's page, with its first message put back when the form has none.
    private func backToForm(from start: SessionStart) { draft.compose(restoring: start.prompt) }

    /// The folder question's answer: the start that asked, sent again with it. Only the question on screen is
    /// answered, and only once.
    private func answer(_ asked: SessionStart, _ trust: FolderTrust, in context: StartContext) {
        guard let ask = draft.sent?.ask, draft.sent?.requestID == asked.requestID else { return }
        let requestID = context.start(
            asked.project, place: asked.place, agent: asked.agent, model: asked.model, prompt: asked.prompt ?? "",
            trust: trust, files: asked.files)
        draft.didSend(requestID, asking: ask)
    }

    // MARK: Computer

    /// The computer it starts on, green while it answers, as every computer the form offers does: a picker, which also
    /// adds a computer.
    private func computerChip(_ context: StartContext) -> some View {
        ComputerButton(contexts: contexts, chosen: context, open: $computerMenuOpen, add: addComputer) {
            draft.computerID = $0
        }
        .help("\(context.machineName) · \(Self.summary(context, now: .now))")
    }

    /// `12 recent folders · last used 4m ago`, as the iPhone says it.
    private static func summary(_ context: StartContext, now: Date) -> String {
        StartWords.recentFolders(context.projects.count, lastUsed: context.lastUsed, now: now)
    }

    // MARK: Folder

    /// The folder it starts in, a picker: its panel holds the recent folders and their search, or the computer's
    /// folders while browsing them.
    private func folderButton(_ context: StartContext) -> some View {
        Button {
            folderOpen.toggle()
        } label: {
            HStack(spacing: 5) {
                Image(systemName: "folder").font(.system(size: 11)).accessibilityHidden(true)
                Text(folder?.name ?? "Choose folder").font(InputBoxChip.font).lineLimit(1).truncationMode(.tail)
                Image(systemName: "chevron.down").font(.system(size: 8.5, weight: .semibold))
            }
            // While none is chosen, the folder is the piece still missing: the bar's one lit, bordered chip.
            .foregroundStyle(Theme.text)
            .inputBoxChip(open: folderOpen, bordered: folder == nil)
        }
        .buttonStyle(.hover(radius: 8, id: "start.folder"))
        .help(folder?.path ?? "Choose folder")
        .accessibilityIdentifier("start.folder")
        .accessibilityValue(folderOpen ? "open" : "closed")
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(Self.space)) } action: { folderChip = $0 }
        .overlay(alignment: .topLeading) {
            if folderOpen {
                let placed = PanelPlace.place(widest: 520, leading: folderChip.minX, box: box)
                folderPanel(context, width: placed.width).openPanel()
                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { folderPanelHeight = $0 }
                    .offset(x: placed.x - folderChip.minX, y: InputBoxChip.height + Theme.panelGap)
            }
        }
        .closesPanel($folderOpen)
        .onChange(of: folderOpen) { _, open in if !open { browsing = false } }
    }

    /// The most the folder list stands: 260 pt, or less where the pane ends sooner, so the whole panel, its foot rows
    /// included, stays inside the pane; at least one folder's row. Before the panel is measured, 260.
    private var listMost: CGFloat {
        guard folderPanelHeight > 0 else { return 260 }
        let others = folderPanelHeight - listShown
        let room = startHeight - PanelPlace.margin - folderChip.maxY - Theme.panelGap - others
        return min(260, max(room, 44))
    }

    /// Up to 520 pt wide, inside the box (`PanelPlace.place`).
    private func folderPanel(_ context: StartContext, width: CGFloat) -> some View {
        folderRows(context)
            .frame(width: width - 2 * MenuMetrics.inset)
            .menuCard()
    }

    @ViewBuilder private func folderRows(_ context: StartContext) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            if browsing {
                FolderBrowser(
                    context: context,
                    use: {
                        choose($0)
                        browsing = false
                    },
                    recent: { browsing = false },
                    listHeight: min(170, listMost), listShown: $listShown
                )
                .id(context.id)
            } else if context.starter.projects == nil {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Reading \(context.machineName)’s recent folders…").font(Theme.ui(12)).foregroundStyle(Theme.text2)
                }
            } else if context.projects.isEmpty {
                Text("No recent folders on \(context.machineName) yet.").font(Theme.ui(12)).foregroundStyle(Theme.text2)
                if let picked = picked(context) { pickedRow(picked) }
            } else {
                TextField("", text: choice(\.query), prompt: Text("Search recent folders").foregroundStyle(Theme.text3))
                    .accessibilityLabel("Search recent folders")
                    .accessibilityIdentifier("start.search")
                    // The field and the folders under it are one stop for the keyboard: ↑ and ↓ move the chosen
                    // folder through the folders as shown while the cursor stays here, so typing goes on; Return
                    // keeps it and closes the panel.
                    .focused($searchFocused)
                    // The panel opens with the keyboard here, so the first keys typed search and the arrows choose.
                    .onAppear { DispatchQueue.main.async { searchFocused = true } }
                    .onKeyPress(.downArrow) { moveFolder(context, down: true) }
                    .onKeyPress(.upArrow) { moveFolder(context, down: false) }
                    .onSubmit { folderOpen = false }
                    .textFieldStyle(.plain)
                    .font(Theme.ui(12.5))
                    .menuField()
                MenuHeader(text: "Recent")
                ScrollViewReader { list in
                    ScrollView {
                        // Not lazy: every folder is in the accessibility tree, scrolled to or not.
                        VStack(spacing: 0) {
                            if let picked = picked(context) { pickedRow(picked) }
                            ForEach(startFilter(context.projects, query)) { project in folderRow(project) }
                        }
                        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { listHeight = $0 }
                    }
                    // The chosen folder is always in view: scrolled to, the least that shows it, when the panel opens
                    // or the choice or the list changes, so a click on a row in view moves nothing.
                    .task(id: [folderPath ?? "", "\(context.projects.count)"]) { list.scrollTo(folderPath) }
                }
                // As tall as its rows, up to 260 pt or the room the pane has under the panel, then it scrolls. An exact
                // height: a panel is drawn over its button, which offers it only the button's own height, so a range
                // would collapse the list to its least.
                .frame(height: min(max(listHeight, 1), listMost))
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { listShown = $0 }
            }
            // A folder from elsewhere, whatever the list holds: this Mac's from the standard folder panel, another
            // computer's by browsing it.
            if !browsing && (isThisMac(context) || context.browses) {
                MenuDivider().padding(.vertical, 5)
                if isThisMac(context) { chooseButton } else { browseButton }
            }
        }
    }

    /// One arrow-key step through the folders as shown, the picked one first.
    private func moveFolder(_ context: StartContext, down: Bool) -> KeyPress.Result {
        let shown = (picked(context).map { [$0.path] } ?? []) + startFilter(context.projects, query).map(\.path)
        draft.chooseFolder(ListStep.step(from: folderPath, in: shown, down: down), on: context.id)
        return .handled
    }

    private func folderRow(_ project: StartProject) -> some View {
        Button {
            choose(project.path)
        } label: {
            MenuRow(chosen: folderPath == project.path) {
                Text(project.name).font(Theme.ui(12.5, .medium)).foregroundStyle(Theme.text).lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 8)
                AgeText(since: project.lastUsed) { startProjectMeta(project, now: $0) }
                    .font(Theme.ui(11)).foregroundStyle(Theme.text3).lineLimit(1)
            } second: {
                Text(project.shownPath).font(Theme.mono(11.5)).truncationMode(.head)
            }
        }
        .buttonStyle(.menuRow(id: "start.folder.\(project.path)"))
        .focusable(false)
        .accessibilityIdentifier("start.folder.\(project.path)")
        .accessibilityAddTraits(folderPath == project.path ? .isSelected : [])
        .id(project.path)
    }

    /// Any folder on this Mac, from the standard folder panel.
    private var chooseButton: some View {
        Button(action: chooseFolder) {
            MenuRow(chosen: false) { Text("Choose a folder\u{2026}").font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1) }
        }
        .buttonStyle(.menuRow(id: "start.choose"))
        .accessibilityIdentifier("start.choose")
    }

    /// Any folder on a computer that is not this Mac, browsed in the Folder box (a computer below minor 30 lists only
    /// its recent ones).
    private var browseButton: some View {
        Button {
            branchMenuOpen = false
            browsing = true
        } label: {
            MenuRow(chosen: false) { Text("Browse\u{2026}").font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1) }
        }
        .buttonStyle(.menuRow(id: "start.browse"))
        .accessibilityIdentifier("start.browse")
    }

    private func chooseFolder() {
        #if DEBUG
            if let path = RenderWindow.pickedFolder {
                choose(path)
                return
            }
        #endif
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Choose"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        choose(url.path)
    }

    /// A chosen folder is the selected one, as a recent row is; the search goes, so a recent one chosen is in view.
    private func choose(_ path: String) {
        if let context, folderPath != path { draft.chooseFolder(path, on: context.id) }
        folderOpen = false
    }

    /// The selected folder when no row below shows it: one chosen that is not a recent one, or a recent one the search
    /// hides. So the folder Start starts in is always one of the list's rows.
    private func picked(_ context: StartContext) -> StartProject? {
        guard let folder, !startFilter(context.projects, query).contains(where: { $0.path == folder.path }) else {
            return nil
        }
        return folder
    }

    /// The selected folder above the rows when none of them shows it: its name and its path, selected.
    private func pickedRow(_ project: StartProject) -> some View {
        MenuRow(chosen: true) {
            Text(project.name).font(Theme.ui(12.5, .medium)).foregroundStyle(Theme.text).lineLimit(1)
                .truncationMode(.middle)
        } second: {
            Text(project.path).font(Theme.mono(11.5)).truncationMode(.head)
        }
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("start.picked")
        .accessibilityAddTraits(.isSelected)
    }

    // MARK: First message and model

    /// The first message, and in the box's bar the computer, the folder and where in the repository, then the model and
    /// Start (↩). The folder's and the computer's menus open below the bar, the model's and the branch's above it.
    private func composer(_ context: StartContext) -> some View {
        InputBox(barHeight: 50) {
            // The files going with the first message stand above its words.
            VStack(alignment: .leading, spacing: 12) {
                if !draft.files.isEmpty {
                    FileTray(files: draft.files, reason: draft.filesBlockedReason, remove: draft.removeFile, retry: { _ in })
                }
                TextField(
                    "", text: $draft.message, selection: $messageSelection,
                    prompt: Text("What do you want to work on?").foregroundStyle(Theme.text.opacity(0.5)), axis: .vertical
                )
                    .accessibilityLabel("First message")
                    .accessibilityIdentifier("start.message")
                    .textFieldStyle(.plain)
                    .font(Theme.ui(15))
                    .lineSpacing(InputBoxLayout.lineSpacing)
                    .lineLimit(short ? 1...6 : 3...8)
                    .focused($messageFocused)
                    // A chat box, as the reply box is: Return starts (`start` keeps the one rule, `startable`), ⇧ or ⌥ Return
                    // adds a line. Without it Return fell to the field's own end of editing, which selected all its text.
                    .sendsOnReturn(start)
                    // The message area stands at least 96 pt, its insets included.
                    .frame(minHeight: 96 - 28, alignment: .topLeading)
            }
        } leading: {
            if context.takesUploads { AttachButton(id: "start.attach", take: draft.attach) }
            // Short of room, the words cut first where they say least for their length: the computer's name, then
            // the branch, then the folder; the model's last.
            computerChip(context).yields(0)
            folderButton(context).yields(2)
            if let folder, context.branchAhead(folder) {
                BranchRow(
                    context: context, folder: folder, agent: pick?.agent,
                    place: Binding(get: { place }, set: { choice(\.place).wrappedValue = $0 }),
                    newName: choice(\.newName), open: $branchMenuOpen, top: 0
                ) {
                    // A turn after the menu closes, as `takeKeyboard`'s, so AppKit's settling doesn't take it back.
                    DispatchQueue.main.async {
                        giveKeyboard($messageFocused, caret: $messageSelection, endOf: draft.message)
                    }
                }
                .yields(1)
            }
        } trailing: {
            ModelButton(context: context, pick: pick, open: $menuOpen) { choice(\.model).wrappedValue = $0 }
                .yields(3)
            startButton
        }
        .takesFiles(context.takesUploads, pasting: messageFocused, draft.attach)
        #if DEBUG
            .task { AppModel.seedFixtureFiles(draft) }
        #endif
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(Self.space)) } action: { box = $0 }
        .environment(\.startBox, box)
    }

    /// Start: filled once it can be pressed; Return in the first message starts too, and ⌘↩ (the owner's word, 2026-11-22,
    /// over decision 289 for this form). One rule for all three, `startable`:
    /// while an ask or a panel of the form is up, its keys are that ask's or panel's.
    private var startButton: some View {
        InputBoxSubmit(
            enabled: startable, id: "start.start", label: "Start", help: blocked ?? "Start (↩)", action: start)
            .keyboardShortcut(startable ? KeyboardShortcut(.return, modifiers: .command) : nil)
    }
}

/// A sent start: its steps while the computer starts it, and Kit's words when it couldn't (the computer's own
/// words too, for a launch that failed). Started, it waits for the run to be in the list.
private struct StartProgress: View {
    let start: SessionStart
    let shown: (String) -> Bool
    let again: () -> Void
    /// No more sends of the start from this Mac, and the form comes back.
    let stopRetrying: () -> Void
    /// Back to the form from the failure page, the failed start let go.
    let close: () -> Void
    let started: (String) -> Void
    /// The computer's own words for a launch that failed are shown.
    @State private var details = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if case .failed(let reason, let detail) = start.outcome {
                failed(reason, detail)
            } else {
                starting
            }
        }
        .onChange(of: liveUID) { _, uid in if let uid { started(uid) } }
        .task {
            if let liveUID { started(liveUID) }
        }
    }

    /// The new run's id once the list has it.
    private var liveUID: String? {
        guard case .started(let uid, _) = start.outcome, shown(uid) else { return nil }
        return uid
    }

    private var starting: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 14) {
                Text(StartWords.starting(start.agent.displayName)).font(Theme.ui(19, .semibold)).foregroundStyle(Theme.text)
                TimelineView(.periodic(from: start.sentAt, by: 1)) { clock in
                    let answered = start.outcome != nil
                    VStack(alignment: .leading, spacing: 8) {
                        step(
                            answered ? .done : .running, answered ? StartWords.started(start.machineName) : StartWords.asking(start.machineName),
                            id: "start.step.asking")
                        step(
                            answered ? .running : .waiting,
                            (start.prompt == nil ? "Opening the run · " : StartWords.waitingForFirstWords(start.agent.displayName))
                                + Format.age(clock.date.timeIntervalSince(start.sentAt)),
                            id: "start.step.waiting")
                    }
                }
                if let prompt = start.prompt {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Your first message").font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.text3)
                        Text(prompt).font(Theme.ui(12.5)).foregroundStyle(Theme.text).fixedSize(horizontal: false, vertical: true)
                    }
                }
                // While the computer hasn't answered, this Mac can stop sending the start, so one whose computer went
                // away is not left resending with no way out, nor the one the form keeps opening on. It says only
                // what it did: the computer may already have started it.
                if start.outcome == nil, !start.retryStopped {
                    VStack(alignment: .leading, spacing: 4) {
                        Button(StartWords.stopRetrying, action: stopRetrying)
                            .buttonStyle(.hover(bare: true, flush: .leading, id: "start.stopRetrying"))
                            .font(Theme.ui(12.5))
                            .foregroundStyle(Theme.text2)
                            .accessibilityIdentifier("start.stopRetrying")
                        Text(StartWords.noMoreSends(start.machineName))
                            .font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
            // No Close while it is being sent, and no bar: the start stays on screen until it is answered or withdrawn
            // (Stop retrying), so it is never forgotten while it may still run. Equal room above and below what is
            // drawn, so the page stands at the area's middle.
            .padding(.horizontal, 18)
            .padding(.vertical, 18)
        }
    }

    private enum Step { case done, running, waiting }

    /// A step's mark, as the iPhone's three: done, under way, not begun. Still marks: the clock beside the last
    /// step is what moves.
    private func step(_ state: Step, _ text: String, id: String) -> some View {
        HStack(spacing: 8) {
            Image(systemName: state == .done ? "checkmark.circle.fill" : state == .running ? "circle.dotted" : "circle.dashed")
                .font(.system(size: 12))
                .foregroundStyle(state == .waiting ? Theme.text3 : Theme.text2)
                .accessibilityLabel(state == .done ? "Done" : state == .running ? "Under way" : "Not begun")
                // A checkmark symbol reads as selected; the label says what it means.
                .accessibilityRemoveTraits(.isSelected)
                .accessibilityIdentifier("\(id).state")
            Text(text).font(Theme.ui(12.5)).foregroundStyle(state == .done ? Theme.text2 : Theme.text)
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(id)
    }

    private func said(_ detail: String) -> some View {
        Text(detail)
            .font(Theme.mono(11.5))
            .foregroundStyle(Theme.text)
            .textSelection(.enabled)
            .accessibilityIdentifier("start.failed.text")
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.leading, 10)
            .padding(.trailing, CopyButton.slot)
            .padding(.vertical, 10)
    }

    /// Kit's words: its title, and its sentence where it has one. A launch that failed has none;
    /// the computer's own words go under Details.
    private func failed(_ reason: StartFailure, _ detail: String) -> some View {
        let words = reason.words(for: start)
        return VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 10) {
                Text(words.title)
                    .font(Theme.ui(19, .semibold))
                    .foregroundStyle(Theme.text)
                    .fixedSize(horizontal: false, vertical: true)
                if let message = words.message {
                    Text(message).font(Theme.ui(13.5)).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
                }
                if reason == .launchFailed, !detail.isEmpty {
                    Button {
                        details.toggle()
                    } label: {
                        HStack(spacing: 6) {
                            Image(systemName: details ? "chevron.down" : "chevron.right").font(.system(size: 9, weight: .semibold))
                            Text("Details").font(Theme.ui(12.5))
                        }
                        .foregroundStyle(Theme.text2)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.hover(bare: true, flush: .leading, id: "start.failed.details"))
                    .accessibilityIdentifier("start.failed.details")
                    .accessibilityValue(details ? "open" : "closed")
                    if details {
                        // As tall as the words, up to a box that scrolls.
                        ViewThatFits(in: .vertical) {
                            said(detail)
                            ScrollView { said(detail) }
                        }
                        .frame(maxHeight: 180, alignment: .top)
                        .fixedSize(horizontal: false, vertical: true)
                        .background(Theme.code, in: RoundedRectangle(cornerRadius: 7))
                        .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
                        .overlay(alignment: .trailing) { CopyButton(text: detail, id: "start.failed.copy") }
                    }
                }
            }
            .padding(.horizontal, 18)
            .padding(.top, 9)  // as much room above as the bar keeps under its buttons
            Color.clear.frame(height: 18)
            SheetBar(
                action: words.action, actionID: "start.failed.again", blocked: false, perform: again, cancel: close,
                cancelID: "start.close", cancelTitle: "Close"
            ) {
                EmptyView()
            }
        }
    }
}

/// A start the computer answered with the folder question: nothing ran. The question is the form's own page; an
/// answer sends the start again with it, and Back returns to the form.
private struct FolderStep: View {
    let start: SessionStart
    /// The folder's real path, as the computer gave it.
    let path: String
    let computer: String
    let answer: (FolderTrust) -> Void
    let back: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 14) {
                Text(FolderWords.title).font(Theme.ui(17, .semibold)).foregroundStyle(Theme.text)
                    .fixedSize(horizontal: false, vertical: true)
                Text(path).font(Theme.mono(11.5)).foregroundStyle(Theme.text2).lineLimit(1).truncationMode(.head)
                    .padding(.leading, 10)
                    .padding(.trailing, CopyButton.slot)
                    .padding(.vertical, 6)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(Theme.code, in: RoundedRectangle(cornerRadius: 6))
                    .accessibilityIdentifier("folder.path")
                    .overlay(alignment: .trailing) { CopyButton(text: path, id: "folder.path.copy") }
                Text(FolderWords.body(computer)).font(Theme.ui(12)).foregroundStyle(Theme.text2)
                    .fixedSize(horizontal: false, vertical: true)
                VStack(spacing: 6) {
                    choice(.trusted)
                    choice(.notTrusted)
                }
                Text(FolderWords.footnote).font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.horizontal, 18)
            .padding(.top, 9)  // as much room above as the bar keeps under its buttons
            .accessibilityElement(children: .contain)
            .accessibilityIdentifier("folder.question")
            Color.clear.frame(height: 18)
            SheetBar(
                action: nil, actionID: "", blocked: true, perform: {}, cancel: back, cancelID: "start.folder.back",
                cancelTitle: "Back"
            ) {
                Text(FolderWords.then(start.agent.displayName, start.project.name)).font(Theme.ui(11.5))
                    .foregroundStyle(Theme.text3)
            }
        }
    }

    private func choice(_ trust: FolderTrust) -> some View {
        let trusts = trust == .trusted
        let id = "folder.\(trusts ? "trust" : "distrust")"
        return Button {
            answer(trust)
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                Text(trusts ? FolderWords.trust : FolderWords.distrust).font(Theme.ui(12.5, .medium)).foregroundStyle(Theme.text)
                Text(trusts ? FolderWords.trustDetail : FolderWords.distrustDetail).font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 9)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.code.opacity(0.5), in: RoundedRectangle(cornerRadius: 7))
            .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 7, id: id))
        .accessibilityIdentifier(id)
    }
}

/// The one computer a start can go to, by name, green while it answers.
/// The computer a start goes to: a button and its panel, the model button's kind, so one form has one kind of menu.
/// The chosen computer carries the check; the lit row, by the pointer or the keys, is filled (`panelRow`).
private struct ComputerButton: View {
    let contexts: [StartContext]
    let chosen: StartContext
    @Binding var open: Bool
    /// Add a computer, the panel's last row.
    let add: () -> Void
    let choose: (String) -> Void

    @Environment(\.startBox) private var startBox
    /// The chip's leading edge in `StartForm.space`, where its panel starts when it fits.
    @State private var minX: CGFloat = 0
    private var placed: (x: CGFloat, width: CGFloat) { PanelPlace.place(widest: 260, leading: minX, box: startBox) }

    var body: some View {
        Button {
            open.toggle()
        } label: {
            HStack(spacing: 5) {
                Image(systemName: "desktopcomputer").font(.system(size: 11)).accessibilityHidden(true)
                Text(chosen.machineName).font(InputBoxChip.font).lineLimit(1).truncationMode(.tail)
                Circle().fill(Theme.green).frame(width: 6, height: 6).accessibilityHidden(true)
                Image(systemName: "chevron.down").font(.system(size: 8.5, weight: .semibold))
            }
            .foregroundStyle(Theme.text2)
            .inputBoxChip(open: open)
        }
        .buttonStyle(.hover(radius: 8, id: "start.computer"))
        .help(chosen.machineName)
        .accessibilityIdentifier("start.computer")
        .accessibilityValue(open ? "open" : "closed")
        .onGeometryChange(for: CGFloat.self) { $0.frame(in: .named(StartForm.space)).minX } action: { minX = $0 }
        .overlay(alignment: .topLeading) {
            if open { panel.openPanel().offset(x: placed.x - minX, y: InputBoxChip.height + Theme.panelGap) }
        }
        .closesPanel($open)
    }

    private var panel: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(contexts.enumerated()), id: \.element.id) { index, context in
                let pick = {
                    choose(context.id)
                    open = false
                }
                Button(action: pick) {
                    MenuRow(chosen: context.id == chosen.id) {
                        Text(context.machineName).font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1)
                    }
                }
                .buttonStyle(.panelRow)
                .panelRow(index, id: "start.computer.\(context.id)", choose: pick)
                .accessibilityIdentifier("start.computer.\(context.id)")
                .accessibilityAddTraits(context.id == chosen.id ? .isSelected : [])
            }
            let adding = {
                open = false
                add()
            }
            MenuDivider().padding(.vertical, 5)
            Button(action: adding) {
                MenuRow {
                    Image(systemName: "plus").font(.system(size: 9.5, weight: .bold)).frame(width: 12)
                        .accessibilityHidden(true)
                    Text("Add a computer\u{2026}").font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1)
                }
            }
            .buttonStyle(.panelRow)
            .panelRow(contexts.count, id: "start.addComputer", choose: adding)
            .accessibilityIdentifier("start.addComputer")
        }
        .frame(width: placed.width - 2 * MenuMetrics.inset)
        .menuCard()
    }
}
