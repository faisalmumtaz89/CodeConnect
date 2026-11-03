import SwiftUI

// The five ways of starting a session, side by side until one is chosen. Each
// hands back the request id of the start it sent; the fleet then opens the
// new session's screen.

// MARK: - 1 · Recents sheet

/// One sheet: the recent projects, the last-used one already chosen, and the
/// agent, the first message and Start pinned under them.
struct StartRecentsSheet: View {
    let context: StartContext
    let onStarted: (String) -> Void
    let onClose: () -> Void

    @State private var query = ""
    @State private var selected: StartProject?
    @State private var agent: StartAgent?
    @State private var message = ""
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        CCSheetChrome("New session", subtitle: context.machineName, onClose: onClose) {
            VStack(spacing: 0) {
                StartSearchField(text: $query)
                    .padding(.horizontal, CC.space.md)
                    .padding(.vertical, CC.space.sm)
                ScrollView {
                    LazyVStack(spacing: 0) {
                        // At the accessibility sizes a pinned panel would leave
                        // no row in view, so it scrolls at the top of the list.
                        if typeSize.isAccessibilitySize, selected != nil {
                            startPanel
                        }
                        ForEach(startFilter(context.projects, query)) { project in
                            Button {
                                selected = project
                                agent = context.preferredAgent(project)
                            } label: {
                                StartProjectRow(project: project, isSelected: project == selected) {
                                    StartCheck(on: project == selected)
                                }
                            }
                            .buttonStyle(StartRowStyle())
                            .accessibilityIdentifier("start-project-\(project.name)")
                            CCHairline().padding(.leading, CC.space.md)
                        }
                    }
                }
            }
            .safeAreaInset(edge: .bottom, spacing: 0) {
                if !typeSize.isAccessibilitySize { startPanel }
            }
        }
        .onAppear {
            context.load()
            if selected == nil, let first = context.projects.first {
                selected = first
                agent = context.preferredAgent(first)
            }
        }
    }
}

extension StartRecentsSheet {
    @ViewBuilder
    fileprivate var startPanel: some View {
        if let selected {
            CCActionBar {
                StartAgentPicker(selection: $agent, project: selected, context: context)
                StartMessagePill(text: $message, placeholder: "First message (optional)")
                CCButton(
                    "Start \(agent?.displayName ?? "")", fullWidth: true,
                    disabledReason: agent == nil ? CCDisabledReason("Choose an agent.") : nil
                ) {
                    guard let agent else { return }
                    onStarted(context.start(selected, agent: agent, prompt: message))
                }
                .accessibilityIdentifier("start-go")
            }
        }
    }
}

// MARK: - 2 · Three steps

/// Where, then which agent, then what to say: one question a page.
struct StartStepsSheet: View {
    let context: StartContext
    let onStarted: (String) -> Void
    let onClose: () -> Void

    @State private var step = 1
    @State private var query = ""
    @State private var project: StartProject?
    @State private var agent: StartAgent?
    @State private var message = ""

    private var title: String {
        switch step {
        case 1: return "Where?"
        case 2: return "Which agent?"
        default: return "First message"
        }
    }

    var body: some View {
        CCSheetChrome(title, subtitle: "Step \(step) of 3", onClose: onClose) {
            if step > 1 {
                Button {
                    step -= 1
                } label: {
                    Text("Back")
                        .ccType(CC.type.body)
                        .foregroundStyle(CC.text.primary)
                        .ccHitTarget()
                }
                .buttonStyle(.plain)
            }
        } content: {
            switch step {
            case 1: whereStep
            case 2: agentStep
            default: messageStep
            }
        }
        .onAppear { context.load() }
    }

    private var whereStep: some View {
        VStack(spacing: 0) {
            StartSearchField(text: $query)
                .padding(.horizontal, CC.space.md)
                .padding(.vertical, CC.space.sm)
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(startFilter(context.projects, query)) { candidate in
                        Button {
                            project = candidate
                            agent = context.preferredAgent(candidate)
                            step = 2
                        } label: {
                            StartProjectRow(project: candidate) {
                                CCIcon("caret-right", size: 16)
                                    .foregroundStyle(CC.text.tertiary)
                            }
                        }
                        .buttonStyle(StartRowStyle())
                        .accessibilityIdentifier("start-project-\(candidate.name)")
                        CCHairline().padding(.leading, CC.space.md)
                    }
                }
            }
        }
    }

    @ViewBuilder
    private var agentStep: some View {
        if let project {
            ScrollView {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    Text("In \(project.name)\(project.gitBranch.map { " on \($0)" } ?? "")")
                        .ccType(CC.type.callout)
                        .foregroundStyle(CC.text.secondary)
                        .padding(.bottom, CC.space.xs)
                    ForEach(StartAgent.allCases) { candidate in agentCard(candidate, project) }
                }
                .padding(CC.space.md)
            }
            .safeAreaInset(edge: .bottom, spacing: 0) {
                CCActionBar {
                    CCButton("Continue", fullWidth: true) { step = 3 }
                        .accessibilityIdentifier("start-continue")
                }
            }
        }
    }

    private func agentCard(_ candidate: StartAgent, _ project: StartProject) -> some View {
        let missing = context.missingReason(candidate, in: project)
        let on = agent == candidate
        return Button {
            agent = candidate
        } label: {
            HStack(alignment: .top, spacing: CC.space.sm) {
                StartCheck(on: on)
                VStack(alignment: .leading, spacing: CC.space.xxs) {
                    HStack(spacing: CC.space.xs) {
                        Text(candidate.displayName)
                            .ccType(CC.type.headline)
                            .foregroundStyle(missing == nil ? CC.text.primary : CC.text.disabled)
                        if project.lastAgent == candidate, missing == nil {
                            CCTag("LAST USED HERE", accessibilityText: "Last used here")
                        }
                    }
                    Text(verbatim: "codeconnect \(candidate.rawValue)")
                        .ccType(CC.type.monoSmall)
                        .foregroundStyle(missing == nil ? CC.text.tertiary : CC.text.disabled)
                    if let missing {
                        Text(missing)
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.text.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .padding(CC.space.md)
            .background(on ? CC.color.surfaceOverlay : CC.color.surface,
                        in: RoundedRectangle(cornerRadius: CC.radius.lg))
            .overlay {
                RoundedRectangle(cornerRadius: CC.radius.lg)
                    .strokeBorder(on ? CC.color.borderFocus : CC.color.border, lineWidth: CC.stroke.hairline)
            }
        }
        .buttonStyle(.plain)
        .disabled(missing != nil)
    }

    @ViewBuilder
    private var messageStep: some View {
        if let project, let agent {
            VStack(alignment: .leading, spacing: CC.space.sm) {
                Text("\(agent.displayName) in \(project.name)")
                    .ccType(CC.type.callout)
                    .foregroundStyle(CC.text.secondary)
                StartMessagePill(
                    text: $message, placeholder: "What should \(agent.displayName) do first?")
                Text("Optional. Leave it empty to start and type in the session.")
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.tertiary)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
            }
            .padding(CC.space.md)
            .safeAreaInset(edge: .bottom, spacing: 0) {
                CCActionBar {
                    CCButton("Start \(agent.displayName)", fullWidth: true) {
                        onStarted(context.start(project, agent: agent, prompt: message))
                    }
                    .accessibilityIdentifier("start-go")
                }
            }
        }
    }
}

// MARK: - 3 · From the card

/// A project already chosen — from its card, or from the recent ones under
/// the cards: only the agent, the message and Start are left.
struct StartProjectSheet: View {
    let context: StartContext
    let project: StartProject
    let onStarted: (String) -> Void
    let onClose: () -> Void

    @State private var agent: StartAgent?
    @State private var message = ""

    var body: some View {
        CCSheetChrome(project.name, subtitle: project.shownPath, onClose: onClose) {
            VStack(alignment: .leading, spacing: CC.space.md) {
                Text(startProjectMeta(project))
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                StartAgentPicker(selection: $agent, project: project, context: context)
                StartMessagePill(text: $message, placeholder: "First message (optional)")
                CCButton("Start \(agent?.displayName ?? "")", fullWidth: true,
                         disabledReason: agent == nil ? CCDisabledReason("Choose an agent.") : nil) {
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

/// Every project, searchable, for the one not among the recent few.
struct StartAllProjectsSheet: View {
    let context: StartContext
    let onPick: (StartProject) -> Void
    let onClose: () -> Void

    @State private var query = ""

    var body: some View {
        CCSheetChrome("All projects", subtitle: context.machineName, onClose: onClose) {
            VStack(spacing: 0) {
                StartSearchField(text: $query)
                    .padding(.horizontal, CC.space.md)
                    .padding(.vertical, CC.space.sm)
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(startFilter(context.projects, query)) { project in
                            Button { onPick(project) } label: { StartProjectRow(project: project) }
                                .buttonStyle(StartRowStyle())
                                .accessibilityIdentifier("start-project-\(project.name)")
                            CCHairline().padding(.leading, CC.space.md)
                        }
                    }
                }
            }
        }
        .onAppear { context.load() }
    }
}

/// The recent projects with nothing running, under the fleet's cards, each
/// one tap from its start sheet, and the door to all of them.
struct StartRecentSection: View {
    let projects: [StartProject]
    let total: Int
    let onPick: (StartProject) -> Void
    let onAll: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            CCSectionHeader("Recent projects")
                .padding(.horizontal, CC.space.md)
                .frame(minHeight: CC.space.xxl)
                .padding(.bottom, CC.space.xs)
            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    ForEach(projects) { project in
                        Button { onPick(project) } label: {
                            StartProjectRow(project: project) {
                                CCIcon("plus", size: 18)
                                    .foregroundStyle(CC.text.primary)
                                    .ccSheetButton()
                            }
                        }
                        .buttonStyle(StartRowStyle())
                        .accessibilityIdentifier("start-recent-\(project.name)")
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

/// A composer at the foot of the fleet: say what to do, and sending it starts
/// the agent in the project on its chip.
struct StartComposerBar: View {
    let context: StartContext
    @Binding var project: StartProject?
    let onPickProject: () -> Void
    let onStarted: (String) -> Void

    @State private var agent: StartAgent?
    @State private var message = ""

    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            if let project {
                HStack(spacing: CC.space.xs) {
                    chip(project.name, identifier: "start-chip-project", action: onPickProject)
                    Menu {
                        ForEach(StartAgent.allCases) { candidate in
                            Button(candidate.displayName) { agent = candidate }
                                .disabled(context.missingReason(candidate, in: project) != nil)
                        }
                    } label: {
                        chipFace(agent?.displayName ?? "Agent")
                    }
                    .accessibilityIdentifier("start-chip-agent")
                    Spacer(minLength: 0)
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
        .padding(.vertical, CC.space.xs)
        .background(CC.color.bg)
        .onChange(of: project, initial: true) { _, new in agent = new.flatMap(context.preferredAgent) }
    }

    private func chip(_ title: String, identifier: String, action: @escaping () -> Void) -> some View {
        Button(action: action) { chipFace(title) }
            .buttonStyle(.plain)
            .accessibilityIdentifier(identifier)
    }

    private func chipFace(_ title: String) -> some View {
        HStack(spacing: CC.space.xxs) {
            Text(title)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.primary)
                .lineLimit(1)
            CCIcon("caret-down", size: 12).foregroundStyle(CC.text.tertiary)
        }
        .padding(.horizontal, CC.space.sm)
        .frame(minHeight: CC.size.controlSm)
        .background(CC.color.surfaceRaised, in: Capsule())
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

/// Type a few letters of the project; each result starts an installed agent
/// with one tap. The first message is typed in the session.
struct StartSearchScreen: View {
    let context: StartContext
    let onStarted: (String) -> Void
    let onClose: () -> Void

    @State private var query = ""
    @FocusState private var searching: Bool

    private var installed: [StartAgent] {
        StartAgent.allCases.filter { context.starter.availableAgents.contains($0) }
    }

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
            if let missing = StartAgent.allCases.first(where: { !installed.contains($0) }) {
                Text("\(missing.displayName) isn’t installed on \(context.machineName).")
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.tertiary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, CC.space.md)
                    .padding(.bottom, CC.space.xs)
            }
            CCHairline()
            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(startFilter(context.projects, query)) { project in
                        StartProjectRow(project: project) {
                            VStack(spacing: CC.space.xs) {
                                ForEach(installed) { agent in
                                    Button {
                                        onStarted(context.start(project, agent: agent, prompt: ""))
                                    } label: {
                                        Text(agent == .claude ? "Claude" : "Codex")
                                            .ccType(CC.type.footnote)
                                            .foregroundStyle(
                                                agent == context.preferredAgent(project)
                                                    ? CC.text.onAccent : CC.text.primary)
                                            .padding(.horizontal, CC.space.sm)
                                            .frame(minHeight: CC.size.controlSm)
                                            .background(
                                                agent == context.preferredAgent(project)
                                                    ? CC.color.accent : CC.color.surfaceRaised,
                                                in: Capsule())
                                            .ccHitTarget()
                                    }
                                    .buttonStyle(.plain)
                                    .accessibilityIdentifier("start-\(agent.rawValue)-\(project.name)")
                                }
                            }
                        }
                        CCHairline().padding(.leading, CC.space.md)
                    }
                }
            }
            .scrollDismissesKeyboard(.interactively)
        }
        .background(CC.color.bg)
        .onAppear {
            context.load()
            searching = true
        }
    }
}
