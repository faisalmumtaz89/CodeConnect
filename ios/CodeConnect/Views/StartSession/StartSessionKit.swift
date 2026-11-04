import SwiftUI

// What the New session steps are built from: what a start knows about its
// computer, a recent folder's row, the search field, the first-message pill
// and the title of the screen a start lands on.

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

    func start(
        _ project: StartProject, place: StartPlace = .folder, agent: StartAgent, prompt: String
    ) -> String {
        model.startSession(project, place: place, agent: agent, prompt: prompt)
    }

    /// The folder's repository, once the computer has said: `.some(nil)` for
    /// a folder that is not one.
    func git(_ path: String) -> StartGit?? { starter.git[path] }
    func askGit(_ path: String) { model.projectGit(path) }
    /// Whether this computer can say what a folder's repository holds (minor 29).
    var asksGit: Bool { model.connection.capabilities?.startsInWorktrees == true }

    /// A folder's listing, once the computer has answered.
    func listing(_ request: StartFolderRequest) -> StartFolderListing? { starter.listings[request] }
    func listFolder(_ request: StartFolderRequest) { model.listFolder(request) }
    func createFolder(named name: String, in parent: String) -> String {
        model.createFolder(named: name, in: parent)
    }

    /// What came of a `create_folder`: nil until answered, `.some(nil)` once
    /// made, else the computer's words.
    func created(_ requestID: String?) -> String?? { requestID.flatMap { starter.creates[$0] } }

    /// Whether this computer can browse and make folders (minor 30).
    var browses: Bool { model.connection.capabilities?.browsesFolders == true }

    /// The folders a live session of this computer's is running in.
    var runningFolders: Set<String> { model.liveFolders }

    /// When this computer last ran anything the phone can start again.
    var lastUsed: Date? { projects.first?.lastUsed }

    /// Why an agent cannot be started on this computer, or nil when it can.
    /// The computer's own words when it has them: its check ran the agent,
    /// and an agent can be there and still not run.
    func missingReason(_ agent: StartAgent, in project: StartProject) -> String? {
        guard !starter.availableAgents.contains(agent) else { return nil }
        guard let detail = starter.agents.first(where: { $0.agent == agent })?.detail else {
            return "\(agent.displayName) isn’t installed on \(machineName)."
        }
        return "\(agent.displayName) can’t start on \(machineName): \(detail)"
    }

}

/// The context a choice names, or the first when it names none.
@MainActor
func startContext(_ contexts: [StartContext], _ id: String) -> StartContext {
    contexts.first { $0.id == id } ?? contexts[0]
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
/// and last agent, with a trailing mark.
struct StartProjectRow<Trailing: View>: View {
    let project: StartProject
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
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            trailing()
        }
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, CC.space.sm)
        .frame(minHeight: CC.size.rowMin)
        .contentShape(Rectangle())
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

    private var field: some View {
        TextField("", text: $text, prompt: Text(placeholder).foregroundStyle(CC.text.placeholder))
            .accessibilityIdentifier("start-search")
    }
}

func startFilter(_ projects: [StartProject], _ query: String) -> [StartProject] {
    let needle = query.trimmingCharacters(in: .whitespaces).lowercased()
    guard !needle.isEmpty else { return projects }
    return projects.filter {
        $0.name.lowercased().contains(needle) || $0.path.lowercased().contains(needle)
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
    let start: SessionStart

    private var project: StartProject { start.project }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(verbatim: project.name)
                .ccType(CC.type.headline)
                .foregroundStyle(CC.text.primary)
                .lineLimit(1)
                .truncationMode(.tail)
            Text(verbatim: start.folder ?? "\(project.path) · new worktree")
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
                .lineLimit(1)
                .truncationMode(.head)
        }
        .dynamicTypeSize(...DynamicTypeSize.xxxLarge)
    }
}
