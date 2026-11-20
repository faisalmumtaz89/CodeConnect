import CodeConnectKit
import SwiftUI

// MARK: - 2 · Steps

/// Reopening the start sheet to ask the folder question (decision 168, placement
/// 1B): the sheet lands on the trust step with the folder's path, over the Ready
/// form rebuilt from the earlier answers — so the typed first message and the
/// computer, folder, agent and place are all still there, and the question's
/// Back is a plain sheet pop to that form. Built from the `SessionStart` the
/// `needsTrust` outcome stands on.
struct StartSeed {
    let context: StartContext
    let project: StartProject
    let place: StartPlace
    let agent: StartAgent
    /// The model the start named, nil for the agent's Default.
    let model: String?
    let message: String
    /// The folder the question is about.
    let trustPath: String
}

/// New session, one question a screen: which computer, which folder (a recent
/// one, or browsed to as in a terminal), which agent, and in a repository
/// which branch; then Start, with an optional first message. A tap on an
/// answer moves on; back returns one screen, and in the browser one folder.
struct StartStepsSheet: View {
    /// The paired computers that can start a session now: the first step's
    /// list, live. With none, the step says why.
    let contexts: [StartContext]
    let onStarted: (String) -> Void
    let onClose: () -> Void

    /// One screen of the flow. The browser is a screen per folder visited, so
    /// back retraces the way in; nil is the home folder, which the computer
    /// names.
    enum Screen: Hashable {
        case computer, folder, browse(String?), agent, branch, newWorktree, start, model, trust
    }

    @State private var trail: [Screen] = [.computer]
    @State private var query = ""
    @State private var project: StartProject?
    @State private var agent: StartAgent?
    /// The model chosen on the Model step, as the start names it; nil until
    /// one is chosen, which is the agent's Default (`pick`).
    @State private var modelValue: String?
    @State private var place = StartPlace.folder
    @State private var message = ""
    @State private var worktreeName = ""
    @State private var showsHidden = false
    @State private var namingFolder = false
    @State private var folderName = ""
    /// Why the computer did not make a folder, in its own words.
    @State private var folderRefusal: String?
    /// The `create_folder` awaiting its answer, and where it was asked.
    @State private var creating: String?
    @State private var creatingIn = ""
    @FocusState private var composing: Bool
    @Environment(\.dynamicTypeSize) private var typeSize
    @Environment(Machines.self) private var machines

    /// The computer tapped on the first step, held from then on: a link that
    /// drops later neither takes it away nor hands its choices to another.
    @State private var chosen: StartContext?
    /// Set when the sheet opens on the folder question: the folder it is about.
    @State private var trustPath: String?

    init(
        contexts: [StartContext], seed: StartSeed? = nil,
        onStarted: @escaping (String) -> Void, onClose: @escaping () -> Void
    ) {
        self.contexts = contexts
        self.onStarted = onStarted
        self.onClose = onClose
        guard let seed else { return }
        // Reopened to ask the folder question: rebuild the Ready form from the
        // earlier answers (computer, folder, agent, place, typed message) with a
        // trail that still lets every "Change" step back to its own screen, then
        // land on the trust step over it, so the question's Back is a sheet pop
        // to that form. The branch screen is in the trail only for a repository,
        // the one place Ready offers a Branch row to revisit.
        _chosen = State(initialValue: seed.context)
        _project = State(initialValue: seed.project)
        _agent = State(initialValue: seed.agent)
        _modelValue = State(initialValue: seed.model)
        _place = State(initialValue: seed.place)
        _message = State(initialValue: seed.message)
        _trustPath = State(initialValue: seed.trustPath)
        let isRepo = seed.context.git(seed.project.path).map { $0 != nil } ?? seed.context.branchAhead(seed.project)
        let steps: [Screen] = [.computer, .folder, .agent] + (isRepo ? [.branch] : []) + [.start, .trust]
        _trail = State(initialValue: steps)
    }

    /// The chosen computer. Every step after the first is drawn only once one
    /// is chosen (`content`), so it is always there when read.
    private var context: StartContext {
        guard let chosen else { preconditionFailure("a step after the computer, before one was chosen") }
        return chosen
    }
    private var screen: Screen { trail.last ?? .computer }

    /// The chosen folder's repository; nil outside one, or until the computer
    /// has said.
    private var git: StartGit? {
        guard let project, let answer = context.git(project.path) else { return nil }
        return answer
    }

    /// Whether the branch step comes next: the computer can say, and has not
    /// yet said the folder is outside a repository.
    private var branchAhead: Bool { project.map(context.branchAhead) ?? false }

    var body: some View {
        Group {
            if trail.count > 1 {
                CCSheetChrome(title, subtitle: subtitle, onClose: onClose) {
                    Button {
                        withAnimation(CC.motion.medium) { _ = trail.popLast() }
                    } label: {
                        CCIcon("arrow-left", size: 16, weight: .semibold)
                            .foregroundStyle(CC.text.primary)
                            .ccSheetButtonSizing()
                            .ccSheetButton()
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Back")
                    .accessibilityIdentifier("start-back")
                } content: {
                    content.id(screen)
                }
            } else {
                CCSheetChrome(title, subtitle: subtitle, onClose: onClose) { content.id(screen) }
            }
        }
        .onAppear { for context in contexts { context.load() } }
    }

    private var title: String {
        switch screen {
        case .computer: return contexts.isEmpty ? FleetWords.startAnAgent : "Which computer?"
        case .folder: return "Which folder?"
        case .browse(let path):
            let shown = chosen?.listing(StartFolderRequest(path: path, showHidden: showsHidden))?.path ?? path
            guard let shown else { return "Home" }
            return shown == "/" ? "/" : String(shown.split(separator: "/").last ?? "")
        case .agent: return "Which agent?"
        case .branch: return "Which branch?"
        case .newWorktree: return "New worktree"
        case .start: return "Ready to start"
        case .model: return "Which model?"
        case .trust: return FleetWords.startAnAgent
        }
    }

    /// What is chosen so far, where it helps to see it: the computer while
    /// choosing a folder, the folder while choosing what runs in it.
    private var subtitle: String? {
        switch screen {
        case .computer, .start: return nil
        case .folder, .browse, .trust: return chosen?.machineName
        case .agent, .branch, .newWorktree, .model: return project?.name
        }
    }

    @ViewBuilder
    private var content: some View {
        switch screen {
        case .computer: computerStep
        case _ where chosen == nil: EmptyView()
        case .folder: folderStep
        case .browse(let path): browseStep(path)
        case .agent: agentStep
        case .branch: branchStep
        case .newWorktree: newWorktreeStep
        case .start: startStep
        case .model: modelStep
        case .trust: trustStep
        }
    }

    private func go(_ next: Screen) {
        withAnimation(CC.motion.medium) { trail.append(next) }
    }

    /// Back to an earlier screen, as if back were tapped until it showed.
    private func revisit(_ earlier: Screen) {
        guard let index = trail.lastIndex(of: earlier) else { return }
        withAnimation(CC.motion.medium) { trail.removeSubrange((index + 1)...) }
    }

    // MARK: Computer

    @ViewBuilder
    private var computerStep: some View {
        if contexts.isEmpty {
            unavailableStep
        } else {
            computerList
        }
    }

    /// Why no computer can start an agent now, in the Kit's words, and for a
    /// computer on a different version, Try again.
    private var unavailableStep: some View {
        ScrollView {
            Text(machines.noStartReason)
                .ccType(CC.type.body)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(CC.space.md)
                .accessibilityIdentifier("start-unavailable")
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            if let refused = machines.startRefusedComputer {
                CCActionBar {
                    CCButton("Try again", variant: .secondary, fullWidth: true) {
                        refused.connection.retryNow()
                    }
                    .accessibilityIdentifier("start-try-again")
                }
            }
        }
    }

    private var computerList: some View {
        ScrollView {
            LazyVStack(spacing: 0) {
                ForEach(contexts, id: \.id) { candidate in
                    Button {
                        chosen = candidate
                        candidate.load()
                        go(.folder)
                    } label: {
                        StartStepRow(title: candidate.machineName, detail: computerSummary(candidate))
                    }
                    .buttonStyle(StartRowStyle())
                    .accessibilityIdentifier("start-computer-\(candidate.machineName)")
                    CCHairline().padding(.leading, CC.space.md)
                }
            }
        }
    }

    private func computerSummary(_ candidate: StartContext) -> String {
        StartWords.recentFolders(candidate.projects.count, lastUsed: candidate.lastUsed, now: Date())
    }

    // MARK: Folder

    private var folderStep: some View {
        VStack(spacing: 0) {
            StartSearchField(text: $query, placeholder: "Search recent folders")
                .padding(.horizontal, CC.space.md)
                .padding(.vertical, CC.space.sm)
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(startFilter(context.projects, query)) { candidate in
                        Button {
                            choose(candidate)
                        } label: {
                            StartProjectRow(project: candidate)
                        }
                        .buttonStyle(StartRowStyle())
                        .accessibilityIdentifier("start-project-\(candidate.name)")
                        CCHairline().padding(.leading, CC.space.md)
                    }
                }
            }
            .scrollDismissesKeyboard(.interactively)
            .accessibilityIdentifier("start-list")
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            // A computer below minor 30 starts only in the folders it lists.
            if context.browses {
                CCActionBar {
                    CCButton("Browse folders", variant: .secondary, fullWidth: true) {
                        go(.browse(nil))
                    }
                    .accessibilityIdentifier("start-browse")
                }
            }
        }
    }

    private func choose(_ folder: StartProject) {
        project = folder
        place = .folder
        context.askGit(folder.path)
        go(.agent)
    }

    // MARK: Browse

    private func browseStep(_ asked: String?) -> some View {
        let request = StartFolderRequest(path: asked, showHidden: showsHidden)
        let listing = context.listing(request)
        return VStack(alignment: .leading, spacing: 0) {
            Text(verbatim: listing?.path ?? " ")
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.secondary)
                .lineLimit(1)
                .truncationMode(.head)
                .padding(.horizontal, CC.space.md)
                .padding(.top, CC.space.sm)
            Toggle(isOn: $showsHidden) {
                Text("Show hidden folders")
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
                .tint(CC.color.accent)
                .padding(.horizontal, CC.space.md)
                .frame(minHeight: CC.size.hitTarget)
                .accessibilityIdentifier("start-hidden")
            CCHairline()
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    if let listing {
                        if listing.error == nil {
                            Button {
                                folderName = ""
                                namingFolder = true
                            } label: {
                                StartStepRow(title: "New folder…")
                            }
                            .buttonStyle(StartRowStyle())
                            .accessibilityIdentifier("start-new-folder")
                            CCHairline().padding(.leading, CC.space.md)
                        }
                        if listing.path != "/" {
                            let parent = parentFolder(listing.path)
                            Button {
                                go(.browse(parent))
                            } label: {
                                StartStepRow(title: "..", mono: true)
                            }
                            .buttonStyle(StartRowStyle())
                            .accessibilityIdentifier("start-up")
                            CCHairline().padding(.leading, CC.space.md)
                        }
                        ForEach(listing.folders.sorted { $0.localizedStandardCompare($1) == .orderedAscending }, id: \.self) {
                            name in
                            Button {
                                go(.browse(childFolder(listing.path, name)))
                            } label: {
                                StartStepRow(title: name, mono: true)
                            }
                            .buttonStyle(StartRowStyle())
                            .accessibilityIdentifier("start-folder-\(name)")
                            CCHairline().padding(.leading, CC.space.md)
                        }
                        if let note = listingNote(listing) {
                            Text(note)
                                .ccType(CC.type.footnote)
                                .foregroundStyle(CC.text.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                                .padding(CC.space.md)
                        }
                    }
                }
            }
            .accessibilityIdentifier("start-list")
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            if let listing, listing.error == nil {
                CCActionBar {
                    CCButton("Use this folder", fullWidth: true) { useFolder(listing.path) }
                        .accessibilityIdentifier("start-use-folder")
                }
            }
        }
        .onAppear { context.listFolder(request) }
        // Made, it opens, as `mkdir` then `cd` would; refused, it says why.
        .onChange(of: context.created(creating)) { _, outcome in
            guard let outcome else { return }
            creating = nil
            if let refusal = outcome {
                folderRefusal = refusal
            } else {
                go(.browse(childFolder(creatingIn, folderName)))
            }
        }
        .onChange(of: showsHidden) { _, _ in context.listFolder(StartFolderRequest(path: asked, showHidden: showsHidden)) }
        .ccConfirmation(isPresented: $namingFolder) {
            CCConfirmation(
                title: "New folder", message: "In \(listing?.path ?? "")",
                answers: [
                    .init(title: "Cancel", kind: .cancel, id: "folder.new.cancel"),
                    .init(title: "Create", kind: .plain, id: "folder.new.create") {
                        guard let parent = listing?.path else { return }
                        creatingIn = parent
                        creating = context.createFolder(named: folderName, in: parent)
                    },
                ],
                field: .init(placeholder: "Name", text: $folderName))
        }
        .ccConfirmation($folderRefusal) { refusal in
            CCConfirmation(
                title: "Couldn’t make “\(folderName)”", message: refusal,
                answers: [.init(title: "OK", kind: .ok, id: "folder.new.ok")])
        }
    }

    /// What the list cannot show by itself: the computer's refusal, an empty
    /// folder, or more folders than it sends.
    private func listingNote(_ listing: StartFolderListing) -> String? {
        if let error = listing.error { return error }
        if listing.truncated { return "The first 2,000 folders, in name order." }
        if listing.folders.isEmpty { return showsHidden ? "No folders in here." : "No folders in here, hidden ones aside." }
        return nil
    }

    private func parentFolder(_ path: String) -> String {
        let parts = path.split(separator: "/").dropLast()
        return "/" + parts.joined(separator: "/")
    }

    private func childFolder(_ path: String, _ name: String) -> String {
        (path == "/" ? "" : path) + "/" + name
    }

    /// The browsed folder as a project: a recent one when it is one.
    private func useFolder(_ path: String) { choose(context.project(at: path)) }

    // MARK: Agent

    @ViewBuilder
    private var agentStep: some View {
        if let project {
            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(StartAgent.allCases) { candidate in
                        let missing = context.missingReason(candidate) ?? signedOut(candidate)
                        Button {
                            if agent != candidate { modelValue = nil }
                            agent = candidate
                            go(branchAhead ? .branch : .start)
                        } label: {
                            StartStepRow(
                                title: candidate.displayName,
                                detail: missing ?? (project.lastAgent == candidate ? "Last used here" : nil),
                                disabled: missing != nil)
                        }
                        .buttonStyle(StartRowStyle())
                        .disabled(missing != nil)
                        .accessibilityIdentifier("start-agent-\(candidate.rawValue)")
                        // How to sign in, under the row it is about.
                        if candidate == .claude, signedOut(.claude) != nil { signInHelp }
                        CCHairline().padding(.leading, CC.space.md)
                    }
                }
            }
            .accessibilityIdentifier("start-list")
        }
    }

    /// The computer's own sentence for an agent that is installed but signed
    /// out, which cannot start until it is signed in.
    private func signedOut(_ candidate: StartAgent) -> String? {
        if case .signedOut(let sentence) = context.models(candidate) { return sentence }
        return nil
    }

    /// How to sign Claude Code in on that computer, as the Mac's start form
    /// says it: where to run it, the command to copy, and the computer asked
    /// again while this shows (`StartContext.watchSignIn`).
    private var signInHelp: some View {
        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            Text(context.signInThere)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
            CCMonoBlock(StartContext.claudeSignIn).onContainerEdge()
        }
        .padding(CC.space.md)
        .frame(maxWidth: .infinity, alignment: .leading)
        .task(id: context.id) { await context.watchSignIn() }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("start-sign-in")
    }

    // MARK: Branch

    /// Until the computer has read the folder's repository, a ring says so;
    /// if it says the folder is outside one, the step gives way to Ready.
    private var branchStep: some View {
        let waiting = project.map { context.git($0.path) == nil } ?? false
        return Group { branchContent }
            .onChange(of: waiting) { _, still in
                guard !still, git == nil else { return }
                trail.removeLast()
                trail.append(.start)
            }
    }

    @ViewBuilder
    private var branchContent: some View {
        if let project, context.git(project.path) == nil {
            HStack(spacing: CC.space.sm) {
                CCProgressRing()
                Text("Reading \(project.name)’s branches…")
                    .ccType(CC.type.body)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(CC.space.md)
            .frame(maxWidth: .infinity, alignment: .leading)
        } else if let project, let git {
            let running = context.runningFolders
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    if let here = git.checkout(at: project.path) {
                        section("This folder")
                        placeRow(.folder, title: here.title, detail: here.notes(running: running))
                    }
                    let others = git.others(than: project.path)
                    if !others.isEmpty {
                        section("Worktrees")
                        ForEach(others, id: \.path) { checkout in
                            placeRow(
                                .existing(checkout), title: checkout.title, detail: checkout.notes(running: running),
                                disabled: checkout.missing)
                        }
                    }
                    section("New worktree")
                    ForEach(git.branches, id: \.self) { branch in
                        placeRow(.branch(branch), title: branch, detail: nil)
                    }
                    if git.branchesTruncated {
                        Text("Older branches aren’t listed.")
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.text.tertiary)
                            .padding(.horizontal, CC.space.md)
                            .padding(.vertical, CC.space.sm)
                    }
                    Button {
                        worktreeName = ""
                        go(.newWorktree)
                    } label: {
                        StartStepRow(title: "New branch…", detail: "A worktree of its own, by name")
                    }
                    .buttonStyle(StartRowStyle())
                    .accessibilityIdentifier("start-new-worktree")
                }
            }
            .accessibilityIdentifier("start-list")
        }
    }

    private func placeRow(_ choice: StartPlace, title: String, detail: String?, disabled: Bool = false)
        -> some View
    {
        VStack(spacing: 0) {
            Button {
                place = choice
                go(.start)
            } label: {
                StartStepRow(title: title, mono: true, detail: detail, disabled: disabled)
            }
            .buttonStyle(StartRowStyle())
            .disabled(disabled)
            .accessibilityIdentifier("start-place-\(title)")
            CCHairline().padding(.leading, CC.space.md)
        }
    }

    private func section(_ title: String) -> some View {
        CCSectionHeader(title)
            .padding(.horizontal, CC.space.md)
            .padding(.top, CC.space.md)
    }

    // MARK: New worktree

    @ViewBuilder
    private var newWorktreeStep: some View {
        if let project, let agent, let git {
            let name = worktreeName.trimmingCharacters(in: .whitespaces)
            let valid = StartPlace.isValidName(name)
            let here = git.checkout(at: project.path)
            ScrollView {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    CCField(
                        label: "Name", text: $worktreeName, placeholder: "fix-login",
                        hint: "Letters, digits, . _ and -, with / between parts. Up to 64.",
                        error: name.isEmpty || valid ? nil : "Letters, digits, . _ and - only, with / between parts.",
                        submitLabel: .continue, autocapitalization: .never, disableAutocorrection: true,
                        isMono: true, labelOnContentColumn: false,
                        onSubmit: { if valid { useNewWorktree(name) } })
                    VStack(alignment: .leading, spacing: CC.space.xs) {
                        fact("Branch", valid ? StartPlace.newBranch(name, for: agent) : "Not named yet", mono: valid)
                        fact("From", StartPlace.newWorktreeBase(for: agent, from: here))
                        if here?.dirty == true {
                            fact("Changes", "Uncommitted changes stay where they are")
                        }
                    }
                    // At the accessibility sizes a pinned bar left the field no
                    // room above the keyboard: Continue follows the facts, and
                    // the keyboard's own key continues too.
                    if typeSize.isAccessibilitySize { continueButton(name, valid: valid) }
                }
                .padding(CC.space.md)
            }
            .safeAreaInset(edge: .bottom, spacing: 0) {
                if !typeSize.isAccessibilitySize {
                    CCActionBar { continueButton(name, valid: valid) }
                }
            }
        }
    }

    private func continueButton(_ name: String, valid: Bool) -> some View {
        CCButton(
            "Continue", fullWidth: true,
            disabledReason: valid
                ? nil
                : CCDisabledReason(name.isEmpty ? "Name the worktree." : "The name breaks the rule above.")
        ) {
            useNewWorktree(name)
        }
        .accessibilityIdentifier("start-continue")
    }

    private func useNewWorktree(_ name: String) {
        place = .new(name)
        go(.start)
    }

    private func fact(_ label: String, _ value: String, mono: Bool = false) -> some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            Text(label)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
            Text(verbatim: value)
                .ccType(mono ? CC.type.monoSmall : CC.type.body)
                .foregroundStyle(CC.text.primary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    // MARK: Start

    @ViewBuilder
    private var startStep: some View {
        if let project, let agent {
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    summary("Computer", context.machineName, .computer)
                    summary("Folder", project.shownPath, .folder, mono: true, oneLine: true)
                    summary("Agent", agent.displayName, .agent)
                    if let pick = pick(agent) {
                        summary("Model", pick.isDefault ? "Default" : pick.label, .model)
                    }
                    if git != nil {
                        summary(
                            "Branch", startPlaceSummary(place, agent: agent, project: project, git: git), .branch,
                            mono: true)
                    }
                    if typeSize.isAccessibilitySize { messagePill(project, agent).padding(CC.space.md) }
                }
            }
            .scrollDismissesKeyboard(.interactively)
            .accessibilityIdentifier("start-list")
            // The message rides over the keyboard with Start. At the
            // accessibility sizes the two left no line to type on: the message
            // stays in the list, and while it has the keyboard its own send
            // starts the session.
            .safeAreaInset(edge: .bottom, spacing: 0) {
                if !(composing && typeSize.isAccessibilitySize) {
                    CCActionBar {
                        if !typeSize.isAccessibilitySize { messagePill(project, agent) }
                        CCButton("Start", size: .lg, fullWidth: true) { start(project, agent) }
                            .accessibilityIdentifier("start-go")
                    }
                }
            }
        }
    }

    private func messagePill(_ project: StartProject, _ agent: StartAgent) -> some View {
        StartMessagePill(
            text: $message, placeholder: "First message", focused: $composing,
            onSend: typeSize.isAccessibilitySize ? { start(project, agent) } : nil)
    }

    private func start(_ project: StartProject, _ agent: StartAgent) {
        onStarted(context.start(project, place: place, agent: agent, model: pick(agent)?.model, prompt: message))
    }

    // MARK: Model

    /// The models this agent offers here, in its order and words.
    private func models(_ agent: StartAgent) -> [StartModelChoice] {
        if case .choices(let choices) = context.models(agent) { return choices }
        return []
    }

    /// The model a start names: the one chosen while the agent still offers
    /// it, else the agent's Default, as the Mac's start form picks
    /// (`StartContext.defaultChoice`).
    private func pick(_ agent: StartAgent) -> StartModelChoice? {
        let offered = models(agent)
        if let modelValue, let chosen = offered.first(where: { $0.model == modelValue }) { return chosen }
        return offered.first(where: \.isDefault)
    }

    /// Reached only from Ready's Model row: each model by its name alone, the
    /// one a start would name checked. A tap chooses it and goes back.
    @ViewBuilder
    private var modelStep: some View {
        if let agent {
            let current = pick(agent)
            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(models(agent), id: \.self) { choice in
                        Button {
                            modelValue = choice.model
                            withAnimation(CC.motion.medium) { _ = trail.popLast() }
                        } label: {
                            HStack(spacing: CC.space.sm) {
                                Text(choice.isDefault ? "Default" : choice.label)
                                    .ccType(CC.type.body)
                                    .foregroundStyle(CC.text.primary)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                if choice == current {
                                    CCIcon("check-circle-fill", size: 18, relativeTo: .body)
                                        .foregroundStyle(CC.text.primary)
                                }
                            }
                            .padding(.horizontal, CC.space.md)
                            .padding(.vertical, CC.space.sm)
                            .frame(minHeight: CC.size.rowMin)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(StartRowStyle())
                        .accessibilityAddTraits(choice == current ? .isSelected : [])
                        .accessibilityIdentifier("start-model-\(choice.isDefault ? "default" : choice.label)")
                        CCHairline().padding(.leading, CC.space.md)
                    }
                }
            }
            .accessibilityIdentifier("start-list")
        }
    }

    // MARK: Trust

    /// The folder question as a step of the sheet (decision 168): the approved
    /// card under the sheet's own chrome — "New session" over the computer's
    /// name, a round Back to the Ready form behind it. An answer re-sends the
    /// start carrying it, and the sheet gives way to the session as any Start
    /// does.
    @ViewBuilder
    private var trustStep: some View {
        if let project, let agent, let trustPath {
            ScrollView {
                TrustFolderCard(path: trustPath, computerName: context.machineName) { trust in
                    onStarted(
                        context.start(
                            project, place: place, agent: agent, model: pick(agent)?.model, prompt: message,
                            trust: trust))
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(CC.space.md)
            }
            .accessibilityIdentifier("start-list")
        }
    }

    /// One choice made, named, and a tap back to the screen that made it.
    /// `oneLine` for a path, cut from the front; every other value wraps.
    private func summary(
        _ label: String, _ value: String, _ step: Screen, mono: Bool = false, oneLine: Bool = false
    ) -> some View {
        VStack(spacing: 0) {
            Button {
                // Back to the screen that made the choice; the Model step is
                // reached only from here, so it is a step forward.
                if trail.contains(step) { revisit(step) } else { go(step) }
            } label: {
                HStack(spacing: CC.space.sm) {
                    VStack(alignment: .leading, spacing: CC.space.xxs) {
                        Text(label)
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.text.tertiary)
                        Text(verbatim: value)
                            .ccType(mono ? CC.type.monoSmall : CC.type.body)
                            .foregroundStyle(CC.text.primary)
                            .lineLimit(oneLine && !typeSize.isAccessibilitySize ? 1 : nil)
                            .truncationMode(.head)
                            .fixedSize(horizontal: false, vertical: !oneLine)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    Text("Change")
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.color.accent)
                }
                .padding(.horizontal, CC.space.md)
                .padding(.vertical, CC.space.sm)
                .frame(minHeight: CC.size.rowMin)
                .contentShape(Rectangle())
            }
            .buttonStyle(StartRowStyle())
            .accessibilityIdentifier("start-summary-\(label)")
            CCHairline().padding(.leading, CC.space.md)
        }
    }
}

/// One answer on a step's list: its name, what is worth knowing about it, and
/// a caret, since a tap moves on. Disabled, it says why and has no caret.
struct StartStepRow: View {
    let title: String
    /// Folder and branch names, as a terminal prints them.
    var mono = false
    var detail: String?
    var disabled = false

    var body: some View {
        HStack(alignment: .center, spacing: CC.space.sm) {
            VStack(alignment: .leading, spacing: CC.space.xxs) {
                Text(verbatim: title)
                    .ccType(mono ? CC.type.mono : CC.type.rowTitle)
                    .foregroundStyle(disabled ? CC.text.secondary : CC.text.primary)
                    .fixedSize(horizontal: false, vertical: true)
                if let detail {
                    // Disabled, the name and its reason dim together, the
                    // reason still a step under the name.
                    Text(detail)
                        .ccType(CC.type.footnote)
                        .foregroundStyle(disabled ? CC.text.tertiary : CC.text.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            if !disabled {
                CCIcon("caret-right", size: 16).foregroundStyle(CC.text.tertiary)
            }
        }
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, CC.space.sm)
        .frame(minHeight: CC.size.rowMin)
        .contentShape(Rectangle())
    }
}

/// The folder-trust question (decision 168): shown when a start comes back
/// `needsTrust` — nothing ran, and the folder has no kept answer. The question,
/// the folder's path in code style, what each answer does, the two answers, and
/// where to change it later. An answer re-sends the start carrying it.
struct TrustFolderCard: View {
    let path: String
    let computerName: String
    let onAnswer: (FolderTrust) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.lg) {
            VStack(alignment: .leading, spacing: CC.space.sm) {
                Text(FolderWords.title)
                    .ccType(CC.type.title)
                    .foregroundStyle(CC.text.primary)
                    .fixedSize(horizontal: false, vertical: true)
                // Drawn flush, so the box's border sits on the one left edge the
                // title, the body and the answers card below it all share; free-
                // standing it would hang its border out into a gutter this card
                // does not have.
                CCMonoBlock(path, lineLimit: 2, truncation: .head, isSmall: true)
                    .ccCentredContent()
            }

            CCProse(
                FolderWords.body(computerName),
                style: CC.type.body, color: CC.text.secondary
            )
            .fixedSize(horizontal: false, vertical: true)

            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    answer(
                        FolderWords.trust, FolderWords.trustDetail,
                        .trusted, id: "trust-yes")
                    CCHairline().padding(.leading, CC.space.md)
                    answer(
                        FolderWords.distrust, FolderWords.distrustDetail,
                        .notTrusted, id: "trust-no")
                }
            }

            Text(FolderWords.footnote)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// One answer: its label and, under it, what it does. A tap re-sends the
    /// start with the answer, as the flow's rows move on.
    private func answer(_ title: String, _ sublabel: String, _ trust: FolderTrust, id: String) -> some View {
        Button { onAnswer(trust) } label: {
            StartStepRow(title: title, detail: sublabel)
        }
        .buttonStyle(StartRowStyle())
        .accessibilityIdentifier(id)
    }
}
