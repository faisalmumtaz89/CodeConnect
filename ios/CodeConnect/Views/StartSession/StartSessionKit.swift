import SwiftUI

// The pieces every way of starting a session shares: a project's row, the
// agent choice, the first-message pill, the search field and the screen a
// start lands on.

/// What a start needs to know about the computer it runs on.
@MainActor
struct StartContext {
    init(model: AppModel) { self.model = model }

    let model: AppModel

    var starter: SessionStarter { model.starter }
    var machineName: String { model.machineName ?? "this computer" }
    var projects: [StartProject] { (starter.projects ?? []).filter(\.exists) }

    var id: String { model.machineID ?? "" }

    /// Ask again each time: the list is the computer's, and it changes.
    func load() { model.listProjects() }

    func start(_ project: StartProject, agent: StartAgent, prompt: String) -> String {
        model.startSession(project, agent: agent, prompt: prompt)
    }

    /// When this computer last ran anything the phone can start again.
    var lastUsed: Date? { projects.first?.lastUsed }

    /// Why an agent cannot be started on this computer, or nil when it can.
    func missingReason(_ agent: StartAgent, in project: StartProject) -> String? {
        starter.availableAgents.contains(agent)
            ? nil : "\(agent.displayName) isn’t installed on \(machineName)."
    }

    func preferredAgent(_ project: StartProject) -> StartAgent? {
        project.preferredAgent(among: starter.availableAgents)
    }
}

/// The context a choice names, or the first when it names none.
@MainActor
func startContext(_ contexts: [StartContext], _ id: String) -> StartContext {
    contexts.first { $0.id == id } ?? contexts[0]
}

/// The paired computers that can start a session, as chips: the chosen one
/// filled, each with its link's dot. In a row while every name fits whole,
/// else one under another, where a long name wraps rather than losing the
/// part that tells two computers apart.
struct StartComputerPicker: View {
    let contexts: [StartContext]
    @Binding var selection: String

    var body: some View {
        ViewThatFits(in: .horizontal) {
            HStack(spacing: CC.space.xs) { chips(wrapping: false) }
            VStack(alignment: .leading, spacing: CC.space.xs) { chips(wrapping: true) }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func chips(wrapping: Bool) -> some View {
        ForEach(contexts, id: \.id) { context in
            let on = context.id == selection
            Button {
                selection = context.id
            } label: {
                HStack(alignment: .firstTextBaseline, spacing: CC.space.xs) {
                    Circle().fill(CC.color.success).frame(width: 6, height: 6)
                        .ccMark(beside: CC.type.footnote)
                    Text(context.machineName)
                        .ccType(CC.type.footnote)
                        .foregroundStyle(on ? CC.text.onAccent : CC.text.primary)
                        .fixedSize(horizontal: !wrapping, vertical: true)
                }
                .padding(.horizontal, CC.space.sm)
                .padding(.vertical, CC.space.xxs)
                .frame(minHeight: CC.size.controlSm)
                .background(
                    on ? CC.color.accent : CC.color.surfaceRaised,
                    in: RoundedRectangle(cornerRadius: CC.size.controlSm / 2))
                .overlay {
                    RoundedRectangle(cornerRadius: CC.size.controlSm / 2)
                        .strokeBorder(on ? Color.clear : CC.color.border, lineWidth: CC.stroke.hairline)
                }
                .ccHitTarget()
            }
            .buttonStyle(.plain)
            .accessibilityAddTraits(on ? .isSelected : [])
            .accessibilityIdentifier("start-computer-\(context.machineName)")
        }
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
    /// The computer it is on, where rows from several are mixed.
    var computer: String?
    @ViewBuilder var trailing: () -> Trailing

    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        HStack(alignment: .center, spacing: CC.space.sm) {
            VStack(alignment: .leading, spacing: CC.space.xxs) {
                Text(project.name)
                    .ccType(CC.type.rowTitle)
                    .foregroundStyle(CC.text.primary)
                    .fixedSize(horizontal: false, vertical: true)
                // One line at every size: the name above already says the
                // tail, and wrapped at AX5 the path made a row half a screen.
                Text(verbatim: project.shownPath)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
                    .lineLimit(1)
                    .truncationMode(.head)
                Text(startProjectMeta(project))
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .lineLimit(typeSize.isAccessibilitySize ? nil : 1)
                    .truncationMode(.middle)
                if let computer {
                    Text("On \(computer)")
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.tertiary)
                        .lineLimit(1)
                }
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
        self.init(project: project, isSelected: isSelected, computer: nil) { EmptyView() }
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

/// The agent a start runs, named once, on the control that changes it. Its
/// menu lists both agents: the chosen one checked, one that is not installed
/// disabled and saying so.
struct StartAgentMenu: View {
    @Binding var selection: StartAgent?
    let project: StartProject
    let context: StartContext

    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        Menu {
            ForEach(StartAgent.allCases) { agent in
                if context.missingReason(agent, in: project) != nil {
                    Button("\(agent.displayName) — not installed") {}
                        .disabled(true)
                } else if agent == selection {
                    Button { selection = agent } label: { Label(agent.displayName, image: "check") }
                } else {
                    Button(agent.displayName) { selection = agent }
                }
            }
        } label: {
            HStack(alignment: .firstTextBaseline, spacing: CC.space.xs) {
                // Whole beside Start, whose label gives way first; on its own
                // line at the accessibility sizes, free to wrap.
                Text(selection?.displayName ?? "No agent")
                    .ccType(CC.type.headline)
                    .fixedSize(horizontal: !typeSize.isAccessibilitySize, vertical: true)
                CCIcon("caret-down", size: 14, weight: .semibold)
                    .foregroundStyle(CC.text.tertiary)
                    .ccMark(beside: CC.type.headline)
            }
            .foregroundStyle(CC.text.primary)
            .padding(.horizontal, CC.space.md)
            .padding(.vertical, CC.space.xs)
            // The whole line at the accessibility sizes, where it stands
            // alone: offered only its ideal width, the name broke in two.
            .frame(
                maxWidth: typeSize.isAccessibilitySize ? .infinity : nil, minHeight: CC.size.controlLg,
                alignment: .leading
            )
            .ccSurface(fill: .clear, radius: CC.radius.lg, border: CC.color.border)
            .contentShape(Rectangle())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .accessibilityLabel("Agent, \(selection?.displayName ?? "none")")
        .accessibilityIdentifier("start-agent")
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
        // One line at the accessibility sizes: a second one grew under the
        // keyboard, taking the line being typed and the send with it.
        .lineLimit(typeSize.isAccessibilitySize ? 1...1 : 1...5)
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
/// the project over its folder, set as the session's own bar sets them, so
/// nothing moves when the session takes over.
struct StartBarTitle: View {
    let project: StartProject

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(verbatim: project.name)
                .ccType(CC.type.headline)
                .foregroundStyle(CC.text.primary)
                .lineLimit(1)
                .truncationMode(.tail)
            Text(verbatim: project.path)
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
                .lineLimit(1)
                .truncationMode(.head)
        }
        .dynamicTypeSize(...DynamicTypeSize.xxxLarge)
    }
}
