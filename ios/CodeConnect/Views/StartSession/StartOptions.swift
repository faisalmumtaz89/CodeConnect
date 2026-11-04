import SwiftUI

// The five ways of starting a session, side by side until one is chosen. Each
// hands back the request id of the start it sent; the fleet then opens the
// new session's screen. With several computers paired, each says where the
// computer is chosen; the one last used is chosen first.

// MARK: - 1 · Recents sheet

/// One sheet: the computer, the recent projects with the last-used one already
/// chosen, and the first message, the agent and Start pinned under them. While
/// the search has the keyboard the bar steps aside for the results; while the
/// first message has it, the chosen project scrolls into view above the bar.
///
/// At the accessibility sizes only Start stays pinned: the message and the
/// agent open under the chosen project's row, on its fill, and scroll with
/// it. Pinned, the three filled an SE's sheet and left the list no height.
struct StartRecentsSheet: View {
    let contexts: [StartContext]
    @Binding var machineID: String
    let onStarted: (String) -> Void
    let onClose: () -> Void

    @State private var query = ""
    /// The chosen project by its path: the computer's list is asked for
    /// again on every open, and the record it sends for the project last
    /// started has moved on (its last use, perhaps its branch).
    @State private var selectedID: String?
    @State private var agent: StartAgent?
    @State private var message = ""
    @FocusState private var searching: Bool
    @FocusState private var composing: Bool
    @Environment(\.dynamicTypeSize) private var typeSize

    private var context: StartContext { startContext(contexts, machineID) }
    private var selected: StartProject? { context.projects.first { $0.id == selectedID } }

    var body: some View {
        // With several computers the filled chip says which; the subtitle
        // would say it twice.
        CCSheetChrome(
            "New session", subtitle: contexts.count > 1 ? nil : context.machineName, onClose: onClose
        ) {
            VStack(spacing: 0) {
                if contexts.count > 1 {
                    // Its full height: squeezed by the list, a wrapped name
                    // ran out of its chip.
                    StartComputerPicker(contexts: contexts, selection: $machineID)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.horizontal, CC.space.md)
                        .padding(.top, CC.space.sm)
                }
                StartSearchField(text: $query, focused: $searching)
                    .onSubmit { searching = false }
                    .padding(.horizontal, CC.space.md)
                    .padding(.vertical, CC.space.sm)
                ScrollViewReader { list in
                    ScrollView {
                        LazyVStack(spacing: 0) {
                            ForEach(startFilter(context.projects, query)) { project in
                                Button {
                                    choose(project)
                                    searching = false
                                } label: {
                                    StartProjectRow(project: project, isSelected: project.id == selectedID) {
                                        StartCheck(on: project.id == selectedID)
                                    }
                                }
                                .buttonStyle(StartRowStyle())
                                .id(project.id)
                                .accessibilityIdentifier("start-project-\(project.name)")
                                if typeSize.isAccessibilitySize, project.id == selectedID {
                                    VStack(alignment: .leading, spacing: CC.space.sm) {
                                        messagePill
                                        StartAgentMenu(selection: $agent, project: project, context: context)
                                    }
                                    .padding([.horizontal, .bottom], CC.space.md)
                                    .background(CC.color.surfaceOverlay)
                                }
                                CCHairline().padding(.leading, CC.space.md)
                            }
                        }
                    }
                    .scrollDismissesKeyboard(.interactively)
                    .onChange(of: composing) { _, now in
                        guard now, !typeSize.isAccessibilitySize, let id = selectedID else { return }
                        withAnimation(CC.motion.medium) { list.scrollTo(id, anchor: .bottom) }
                    }
                }
                .accessibilityIdentifier("start-list")
            }
            // At the accessibility sizes a bar over the keyboard left no line
            // for the message being typed; the message's own send starts it.
            // A search that hides the chosen project hides its Start too.
            .safeAreaInset(edge: .bottom, spacing: 0) {
                if !searching, !(composing && typeSize.isAccessibilitySize),
                    let selected, startFilter(context.projects, query).contains(where: { $0.id == selectedID })
                {
                    startBar(selected)
                }
            }
        }
        .onAppear {
            context.load()
            if selectedID == nil, let first = context.projects.first { choose(first) }
        }
        .onChange(of: machineID) { _, _ in
            context.load()
            selectedID = nil
            if let first = context.projects.first { choose(first) }
        }
    }

    private func choose(_ project: StartProject) {
        selectedID = project.id
        agent = context.preferredAgent(project)
    }

    private func startBar(_ selected: StartProject) -> some View {
        CCActionBar {
            if typeSize.isAccessibilitySize {
                startButton(selected)
            } else {
                messagePill
                HStack(spacing: CC.space.sm) {
                    StartAgentMenu(selection: $agent, project: selected, context: context)
                    startButton(selected)
                }
            }
        }
    }

    private var messagePill: some View {
        StartMessagePill(
            text: $message, placeholder: "First message", focused: $composing,
            onSend: typeSize.isAccessibilitySize ? { start(selected) } : nil)
    }

    private func start(_ project: StartProject?) {
        guard let project, let agent else { return }
        onStarted(context.start(project, agent: agent, prompt: message))
    }

    private func startButton(_ project: StartProject) -> some View {
        CCButton(
            "Start", size: .lg, fullWidth: true,
            disabledReason: agent == nil ? CCDisabledReason("Choose an agent.") : nil
        ) {
            start(project)
        }
        .accessibilityIdentifier("start-go")
    }
}

// MARK: - 3 · From the card

/// A project already chosen — from its card, or from the recent ones under
/// the cards: only the agent, the message and Start are left. The computer is
/// the card's own, or the recent row's.
struct StartProjectSheet: View {
    let context: StartContext
    let project: StartProject
    var showsComputer = false
    let onStarted: (String) -> Void
    let onClose: () -> Void

    @State private var agent: StartAgent?
    @State private var message = ""

    var body: some View {
        CCSheetChrome(project.name, subtitle: project.shownPath, onClose: onClose) {
            VStack(alignment: .leading, spacing: CC.space.md) {
                Text(
                    (showsComputer ? "On \(context.machineName) · " : "") + startProjectMeta(project)
                )
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
                StartAgentMenu(selection: $agent, project: project, context: context)
                StartMessagePill(text: $message, placeholder: "First message (optional)")
                CCButton(
                    "Start", fullWidth: true,
                    disabledReason: agent == nil ? CCDisabledReason("Choose an agent.") : nil
                ) {
                    guard let agent else { return }
                    onStarted(context.start(project, agent: agent, prompt: message))
                }
                .accessibilityIdentifier("start-go")
            }
            .padding(CC.space.md)
        }
        .onAppear { if agent == nil { agent = context.preferredAgent(project) } }
    }
}

/// Every project on a computer, searchable, for the one not among the recent
/// few; with several computers, the computer is chosen above the list.
struct StartAllProjectsSheet: View {
    let contexts: [StartContext]
    @Binding var machineID: String
    let onPick: (StartProject, StartContext) -> Void
    let onClose: () -> Void

    @State private var query = ""

    private var context: StartContext { startContext(contexts, machineID) }

    var body: some View {
        CCSheetChrome("All projects", subtitle: context.machineName, onClose: onClose) {
            VStack(spacing: 0) {
                if contexts.count > 1 {
                    StartComputerPicker(contexts: contexts, selection: $machineID)
                        .padding(.horizontal, CC.space.md)
                        .padding(.top, CC.space.sm)
                }
                StartSearchField(text: $query)
                    .padding(.horizontal, CC.space.md)
                    .padding(.vertical, CC.space.sm)
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(startFilter(context.projects, query)) { project in
                            Button { onPick(project, context) } label: { StartProjectRow(project: project) }
                                .buttonStyle(StartRowStyle())
                                .accessibilityIdentifier("start-project-\(project.name)")
                            CCHairline().padding(.leading, CC.space.md)
                        }
                    }
                }
            }
        }
        .onAppear { for context in contexts { context.load() } }
    }
}

/// The recent projects with nothing running, under the fleet's cards, from
/// every computer, each one tap from its start sheet, and the door to all.
struct StartRecentSection: View {
    let projects: [(project: StartProject, context: StartContext)]
    let total: Int
    let showsComputer: Bool
    let onPick: (StartProject, StartContext) -> Void
    let onAll: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            CCSectionHeader("Recent projects")
                .padding(.horizontal, CC.space.md)
                .frame(minHeight: CC.space.xxl)
                .padding(.bottom, CC.space.xs)
            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    ForEach(projects, id: \.project.id) { item in
                        Button { onPick(item.project, item.context) } label: {
                            StartProjectRow(
                                project: item.project,
                                computer: showsComputer ? item.context.machineName : nil
                            ) {
                                CCIcon("plus", size: 18)
                                    .foregroundStyle(CC.text.primary)
                                    .ccSheetButton()
                            }
                        }
                        .buttonStyle(StartRowStyle())
                        .accessibilityIdentifier("start-recent-\(item.project.name)")
                        CCHairline()
                    }
                    Button(action: onAll) {
                        HStack {
                            Text("All \(total) projects")
                                .ccType(CC.type.body)
                                .foregroundStyle(CC.text.primary)
                            Spacer()
                            CCIcon("caret-right", size: 16).foregroundStyle(CC.text.tertiary)
                        }
                        .padding(.horizontal, CC.space.md)
                        .frame(minHeight: CC.size.controlLg)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(StartRowStyle())
                    .accessibilityIdentifier("start-all")
                }
            }
            .padding(.horizontal, CC.space.md)
        }
    }
}

// MARK: - 4 · Message first

/// A composer at the foot of the fleet, sharing one surface with the Deck
/// bar: say what to do, and sending it starts the agent in the project on its
/// chip, on the computer on its chip.
struct StartComposerBar: View {
    let contexts: [StartContext]
    @Binding var machineID: String
    @Binding var project: StartProject?
    let onPickProject: () -> Void
    let onStarted: (String) -> Void

    @State private var agent: StartAgent?
    @State private var message = ""

    private var context: StartContext { startContext(contexts, machineID) }

    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            if let project {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: CC.space.xs) {
                        if contexts.count > 1 {
                            Menu {
                                ForEach(contexts, id: \.id) { candidate in
                                    Button(candidate.machineName) { machineID = candidate.id }
                                }
                            } label: {
                                chipFace(context.machineName, icon: "desktop")
                            }
                            .accessibilityIdentifier("start-chip-computer")
                        }
                        Button(action: onPickProject) { chipFace(project.name, icon: nil) }
                            .buttonStyle(.plain)
                            .accessibilityIdentifier("start-chip-project")
                        Menu {
                            ForEach(StartAgent.allCases) { candidate in
                                if let missing = context.missingReason(candidate, in: project) {
                                    Button("\(candidate.displayName) — \(missing)") {}
                                        .disabled(true)
                                } else {
                                    Button(candidate.displayName) { agent = candidate }
                                }
                            }
                        } label: {
                            chipFace(agent?.displayName ?? "Agent", icon: nil)
                        }
                        .accessibilityIdentifier("start-chip-agent")
                    }
                }
                StartMessagePill(
                    text: $message,
                    placeholder: "Start \(agent?.displayName ?? "an agent") in \(project.name)…",
                    onSend: {
                        guard let agent else { return }
                        onStarted(context.start(project, agent: agent, prompt: message))
                        message = ""
                    }
                )
            }
        }
        .padding(.horizontal, CC.space.md)
        .padding(.top, CC.space.sm)
        .padding(.bottom, CC.space.xs)
        .onChange(of: project, initial: true) { _, new in agent = new.flatMap(context.preferredAgent) }
        .onChange(of: machineID) { _, _ in
            context.load()
            project = context.projects.first
        }
    }

    private func chipFace(_ title: String, icon: String?) -> some View {
        HStack(spacing: CC.space.xxs) {
            if let icon { CCIcon(icon, size: 14).foregroundStyle(CC.text.secondary) }
            Text(title)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.primary)
                .lineLimit(1)
            CCIcon("caret-down", size: 12).foregroundStyle(CC.text.tertiary)
        }
        .padding(.horizontal, CC.space.sm)
        .frame(minHeight: CC.size.controlSm)
        .background(CC.color.surfaceOverlay, in: Capsule())
        .overlay { Capsule().strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline) }
        .ccHitTarget()
    }
}

// MARK: - 5 · Search first

/// The fleet's door to search: a field that is a button, under the title.
struct StartSearchDoor: View {
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: CC.space.xs) {
                CCIcon("magnifying-glass", size: 18).foregroundStyle(CC.text.tertiary)
                Text("Start a session in…")
                    .ccType(CC.type.body)
                    .foregroundStyle(CC.text.placeholder)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, CC.space.sm)
            .frame(minHeight: CC.size.controlMd)
            .background(CC.color.surfaceRaised, in: RoundedRectangle(cornerRadius: CC.radius.lg))
            .overlay {
                RoundedRectangle(cornerRadius: CC.radius.lg)
                    .strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline)
            }
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("start-entry")
    }
}

/// Type a few letters of the project; under each result, one button per
/// installed agent names exactly what it starts. Every computer's projects are
/// searched at once, under their computer's name. The first message is typed
/// in the session.
struct StartSearchScreen: View {
    let contexts: [StartContext]
    let onStarted: (String) -> Void
    let onClose: () -> Void

    @State private var query = ""
    @FocusState private var searching: Bool

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: CC.space.sm) {
                StartSearchField(text: $query, placeholder: "Project or folder", focused: $searching)
                Button(action: onClose) {
                    CCIcon("x", size: 16, weight: .semibold)
                        .foregroundStyle(CC.text.primary)
                        .ccSheetButtonSizing()
                        .ccSheetButton()
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Close")
            }
            .padding(.horizontal, CC.space.md)
            .padding(.vertical, CC.space.sm)
            CCHairline()
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(contexts, id: \.id) { context in
                        let results = startFilter(context.projects, query)
                        if contexts.count > 1, !results.isEmpty {
                            CCSectionHeader(context.machineName)
                                .padding(.horizontal, CC.space.md)
                                .frame(minHeight: CC.space.xxl)
                        }
                        if let missing = StartAgent.allCases.first(where: {
                            !context.starter.availableAgents.contains($0)
                        }), !results.isEmpty {
                            Text("\(missing.displayName) isn’t installed on \(context.machineName).")
                                .ccType(CC.type.footnote)
                                .foregroundStyle(CC.text.tertiary)
                                .padding(.horizontal, CC.space.md)
                                .padding(.vertical, CC.space.xs)
                        }
                        ForEach(results) { project in
                            result(project, context)
                            CCHairline().padding(.leading, CC.space.md)
                        }
                    }
                }
            }
            .scrollDismissesKeyboard(.interactively)
        }
        .background(CC.color.bg)
        .onAppear {
            for context in contexts { context.load() }
            searching = true
        }
    }

    private func result(_ project: StartProject, _ context: StartContext) -> some View {
        let installed = StartAgent.allCases.filter { context.starter.availableAgents.contains($0) }
        let preferred = context.preferredAgent(project)
        return VStack(alignment: .leading, spacing: 0) {
            StartProjectRow(project: project)
            CCAdaptiveStack(horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xs) {
                ForEach(installed) { agent in
                    Button {
                        onStarted(context.start(project, agent: agent, prompt: ""))
                    } label: {
                        Text("Start \(agent.displayName)")
                            .ccType(CC.type.footnote)
                            .foregroundStyle(agent == preferred ? CC.text.onAccent : CC.text.primary)
                            .padding(.horizontal, CC.space.sm)
                            .frame(minHeight: CC.size.controlSm)
                            .background(
                                agent == preferred ? CC.color.accent : CC.color.surfaceRaised,
                                in: Capsule())
                            .overlay {
                                Capsule().strokeBorder(
                                    agent == preferred ? Color.clear : CC.color.border,
                                    lineWidth: CC.stroke.hairline)
                            }
                            .ccHitTarget()
                    }
                    .buttonStyle(.plain)
                    .accessibilityIdentifier("start-\(agent.rawValue)-\(project.name)")
                }
            }
            .padding(.horizontal, CC.space.md)
            .padding(.bottom, CC.space.sm)
        }
    }
}
