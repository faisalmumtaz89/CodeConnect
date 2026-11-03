import SwiftUI

// The pieces every way of starting a session shares: a project's row, the
// agent choice, the first-message pill, the search field and the screen a
// start lands on.

/// What a start needs to know about the computer it runs on.
@MainActor
struct StartContext {
    let model: AppModel

    var starter: SessionStarter { model.starter }
    var machineName: String { model.machineName ?? "this computer" }
    var projects: [StartProject] { (starter.projects ?? []).filter(\.exists) }

    func load() {
        if starter.projects == nil { starter.listProjects(fixtures: model.fixturesActive) }
    }

    func start(_ project: StartProject, agent: StartAgent, prompt: String) -> String {
        starter.start(
            project: project, agent: agent, prompt: prompt, machineName: machineName,
            fixtures: model.fixturesActive)
    }

    /// Why an agent cannot be started on this computer, or nil when it can.
    func missingReason(_ agent: StartAgent, in project: StartProject) -> String? {
        starter.availableAgents.contains(agent)
            ? nil : "\(agent.displayName) isn’t installed on \(machineName)."
    }

    func preferredAgent(_ project: StartProject) -> StartAgent? {
        project.preferredAgent(among: starter.availableAgents)
    }
}

// MARK: - Project row

/// `main · 4m ago · Claude Code`: the branch, when it was last used and by
/// which agent. A folder that is not a repository has no branch to show.
func startProjectMeta(_ project: StartProject, now: Date = Date()) -> String {
    var parts: [String] = []
    if let branch = project.gitBranch { parts.append(branch) }
    parts.append("\(Format.age(since: project.lastUsed, now: now)) ago")
    if let agent = project.lastAgent { parts.append(agent.displayName) }
    return parts.joined(separator: " · ")
}

/// A project: its name, its folder cut from the front, and its branch, age
/// and last agent. The trailing slot is the option's own: a check, a chevron,
/// or the agents themselves.
struct StartProjectRow<Trailing: View>: View {
    let project: StartProject
    var isSelected = false
    @ViewBuilder var trailing: () -> Trailing

    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        HStack(alignment: .center, spacing: CC.space.sm) {
            VStack(alignment: .leading, spacing: CC.space.xxs) {
                Text(project.name)
                    .ccType(CC.type.rowTitle)
                    .foregroundStyle(CC.text.primary)
                    .fixedSize(horizontal: false, vertical: true)
                Text(verbatim: project.shownPath)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
                    .lineLimit(typeSize.isAccessibilitySize ? nil : 1)
                    .truncationMode(.head)
                Text(startProjectMeta(project))
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .lineLimit(typeSize.isAccessibilitySize ? nil : 1)
                    .truncationMode(.middle)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            trailing()
        }
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, CC.space.sm)
        .frame(minHeight: CC.size.rowMin)
        .background(isSelected ? CC.color.surfaceOverlay : Color.clear)
        .contentShape(Rectangle())
    }
}

extension StartProjectRow where Trailing == EmptyView {
    init(project: StartProject, isSelected: Bool = false) {
        self.init(project: project, isSelected: isSelected) { EmptyView() }
    }
}

/// A selected row's mark: the same check the model chooser uses.
struct StartCheck: View {
    let on: Bool
    var body: some View {
        CCIcon(on ? "check-circle-fill" : "circle", size: 22, weight: on ? .regular : .light)
            .foregroundStyle(on ? CC.color.success : CC.text.disabled)
            .accessibilityHidden(true)
    }
}

/// A whole row as one target, lightening one step under a finger.
struct StartRowStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(configuration.isPressed ? CC.color.surfaceRaised : Color.clear)
    }
}

// MARK: - Search

/// A field that narrows a project list by name or folder.
struct StartSearchField: View {
    @Binding var text: String
    var placeholder = "Search projects"
    var focused: FocusState<Bool>.Binding?

    var body: some View {
        HStack(spacing: CC.space.xs) {
            CCIcon("magnifying-glass", size: 18)
                .foregroundStyle(CC.text.tertiary)
            field
                .ccType(CC.type.body)
                .foregroundStyle(CC.text.primary)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .submitLabel(.search)
        }
        .padding(.horizontal, CC.space.sm)
        .frame(minHeight: CC.size.controlMd)
        .background(CC.color.surfaceRaised, in: RoundedRectangle(cornerRadius: CC.radius.lg))
        .overlay {
            RoundedRectangle(cornerRadius: CC.radius.lg)
                .strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline)
        }
    }

    @ViewBuilder
    private var field: some View {
        let base = TextField(
            "", text: $text, prompt: Text(placeholder).foregroundStyle(CC.text.placeholder)
        )
        .accessibilityIdentifier("start-search")
        if let focused { base.focused(focused) } else { base }
    }
}

func startFilter(_ projects: [StartProject], _ query: String) -> [StartProject] {
    let needle = query.trimmingCharacters(in: .whitespaces).lowercased()
    guard !needle.isEmpty else { return projects }
    return projects.filter {
        $0.name.lowercased().contains(needle) || $0.path.lowercased().contains(needle)
    }
}

// MARK: - Agent choice

/// Claude Code and Codex side by side, the one that is not installed dimmed
/// with the reason under the pair.
struct StartAgentPicker: View {
    @Binding var selection: StartAgent?
    let project: StartProject
    let context: StartContext

    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            CCAdaptiveStack(horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xs) {
                ForEach(StartAgent.allCases) { agent in segment(agent) }
            }
            ForEach(StartAgent.allCases.filter { context.missingReason($0, in: project) != nil }) {
                agent in
                Text(context.missingReason(agent, in: project) ?? "")
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.tertiary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func segment(_ agent: StartAgent) -> some View {
        let available = context.missingReason(agent, in: project) == nil
        let on = selection == agent
        return Button {
            selection = agent
        } label: {
            Text(agent.displayName)
                .ccType(CC.type.body)
                .foregroundStyle(
                    on ? CC.text.onAccent : (available ? CC.text.primary : CC.text.disabled))
                .frame(maxWidth: .infinity, minHeight: CC.size.controlMd)
                .background(
                    on ? CC.color.accent : CC.color.surfaceRaised,
                    in: RoundedRectangle(cornerRadius: CC.radius.lg))
                .overlay {
                    RoundedRectangle(cornerRadius: CC.radius.lg)
                        .strokeBorder(on ? Color.clear : CC.color.border, lineWidth: CC.stroke.hairline)
                }
        }
        .buttonStyle(.plain)
        .disabled(!available)
        .accessibilityAddTraits(on ? .isSelected : [])
    }
}

// MARK: - First message

/// The composer's own pill, for the first message: the field, and one circle
/// at its end that is the mic while the field is empty.
struct StartMessagePill: View {
    @Binding var text: String
    let placeholder: String
    var focused: FocusState<Bool>.Binding?
    /// Drawn in the circle once text exists; nil keeps the mic.
    var onSend: (() -> Void)?

    @State private var dictation = DictationController()
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        Group {
            if dictation.isRecording {
                Text(dictation.transcript.isEmpty ? "Listening…" : dictation.transcript)
                    .ccType(CC.type.body)
                    .foregroundStyle(CC.text.tertiary)
                    .frame(maxWidth: .infinity, alignment: .topLeading)
            } else {
                field
            }
        }
        .padding(.leading, CC.space.sm)
        .padding(.trailing, CC.size.hitTarget + CC.space.xxs)
        .padding(.vertical, CC.space.xs)
        .frame(minHeight: CC.size.controlSm + 2 * CC.space.xs)
        .frame(maxWidth: .infinity, alignment: .leading)
        .overlay(alignment: .bottomTrailing) {
            CCVoiceButton(phase: phase) { act() }
                .accessibilityIdentifier("start-message-button")
                .padding(CC.space.xxs)
        }
        .background {
            RoundedRectangle(cornerRadius: (CC.size.controlSm + 2 * CC.space.xs) / 2, style: .continuous)
                .fill(CC.color.surfaceOverlay)
                .overlay {
                    RoundedRectangle(
                        cornerRadius: (CC.size.controlSm + 2 * CC.space.xs) / 2, style: .continuous
                    )
                    .strokeBorder(CC.color.borderStrong, lineWidth: CC.stroke.hairline)
                }
        }
        .onDisappear { dictation.cancel() }
    }

    @ViewBuilder
    private var field: some View {
        let base = TextField(
            "", text: $text, prompt: Text(placeholder).foregroundStyle(CC.text.placeholder),
            axis: .vertical
        )
        .lineLimit(typeSize.isAccessibilitySize ? 1...3 : 1...5)
        .ccType(CC.type.body)
        .foregroundStyle(CC.text.primary)
        .accessibilityIdentifier("start-message")
        if let focused { base.focused(focused) } else { base }
    }

    private var phase: CCVoiceButtonPhase {
        if dictation.isRecording || dictation.isStarting { return .stop }
        return (text.isEmpty || onSend == nil) ? .dictate : .send
    }

    private func act() {
        if dictation.isRecording || dictation.isStarting {
            let heard = dictation.stop()
            if !heard.isEmpty { text = text.isEmpty ? heard : text + " " + heard }
        } else if !text.isEmpty, let onSend {
            onSend()
        } else {
            Task { await dictation.start() }
        }
    }
}

// MARK: - Bar title

/// The session screen's bar title, for a session that does not exist yet:
/// the project over its folder.
struct StartBarTitle: View {
    let project: StartProject

    var body: some View {
        VStack(spacing: 0) {
            Text(project.name)
                .ccType(CC.type.headline)
                .foregroundStyle(CC.text.primary)
                .lineLimit(1)
            Text(verbatim: project.shownPath)
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
                .lineLimit(1)
                .truncationMode(.head)
        }
        .frame(maxWidth: 240)
    }
}
