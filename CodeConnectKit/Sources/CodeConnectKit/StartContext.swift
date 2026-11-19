import Foundation

// The rules a New session follows: what a start knows about its computer,
// which computers can start one, a recent folder's line and its search, the
// branch step, a checkout's name and notes, a new worktree's base, the chosen
// place in a word or two, and what a failed start says.

/// What a start needs to know about the computer it runs on.
@MainActor
public struct StartContext {
    public init(model: AppModel) { self.model = model }

    let model: AppModel

    public var starter: SessionStarter { model.starter }
    public var machineName: String { model.machineName ?? "this computer" }
    public var projects: [StartProject] { (starter.projects ?? []).filter(\.exists) }

    public var id: String { model.machineID ?? "" }

    /// Ask again each time: the list is the computer's, and it changes.
    public func load() { model.listProjects() }

    public func start(
        _ project: StartProject, place: StartPlace = .folder, agent: StartAgent, model choice: String? = nil,
        prompt: String, trust: FolderTrust? = nil
    ) -> String {
        model.startSession(project, place: place, agent: agent, model: choice, prompt: prompt, trust: trust)
    }

    /// The folder's repository, once the computer has said: `.some(nil)` for
    /// a folder that is not one.
    public func git(_ path: String) -> StartGit?? { starter.git[path] }
    public func askGit(_ path: String) { model.projectGit(path) }
    /// Whether this computer can say what a folder's repository holds (minor
    /// 29). A redial, which has no capabilities yet, is not a no: the read is
    /// asked again after the handshake, as `AppModel.projectGit` does.
    public var asksGit: Bool { model.connection.capabilities?.startsInWorktrees != false }

    /// Whether the branch step comes next for a folder: the computer can say,
    /// and has not yet said the folder is outside a repository.
    public func branchAhead(_ project: StartProject) -> Bool {
        guard asksGit else { return false }
        return git(project.path).map { $0 != nil } ?? true
    }

    /// A folder's listing, once the computer has answered.
    public func listing(_ request: StartFolderRequest) -> StartFolderListing? { starter.listings[request] }
    public func listFolder(_ request: StartFolderRequest) { model.listFolder(request) }
    public func createFolder(named name: String, in parent: String) -> String {
        model.createFolder(named: name, in: parent)
    }

    /// What came of a `create_folder`: nil until answered, `.some(nil)` once
    /// made, else the computer's words.
    public func created(_ requestID: String?) -> String?? { requestID.flatMap { starter.creates[$0] } }

    /// Whether this computer can browse and make folders (minor 30).
    public var browses: Bool { model.connection.capabilities?.browsesFolders == true }

    /// A browsed folder as a project: a recent one when it is one.
    public func project(at path: String) -> StartProject {
        projects.first { $0.path == path }
            ?? StartProject(
                path: path, name: path == "/" ? "/" : String(path.split(separator: "/").last ?? ""),
                lastUsed: Date(), lastAgent: nil, gitBranch: nil, exists: true)
    }

    /// The folders a live session of this computer's is running in.
    public var runningFolders: Set<String> { model.liveFolders }

    /// When this computer last ran anything the phone can start again.
    public var lastUsed: Date? { projects.first?.lastUsed }

    /// Why an agent cannot be started on this computer, or nil when it can: the computer's own sentence
    /// (ccd's `detail`, for either agent), else plain words naming the computer.
    public func missingReason(_ agent: StartAgent) -> String? {
        guard !starter.availableAgents.contains(agent) else { return nil }
        return said(agent) ?? "\(agent.displayName) can’t start on \(machineName)."
    }

    /// The computer's own sentence for why an agent would be refused or is signed out (`detail`): the
    /// owner's words, naming the computer, safe to show as sent. ccd composes it for either agent now —
    /// Codex's is a plain sentence (a floor, an unread version, a sign-in), no longer its raw check output.
    private func said(_ agent: StartAgent) -> String? {
        starter.agents.first { $0.agent == agent }?.detail
    }

    /// How to sign in to Claude Code on a computer other than this Mac (the owner's 2A): these words, then
    /// `claudeSignIn` to copy. On this Mac the form shows the sentence alone.
    public var signInThere: String {
        "To sign in, run this in a terminal on \(machineName). CodeConnect sees it when you’re done."
    }
    public static let claudeSignIn = "claude auth login"

    // MARK: Models

    /// The model button while the computer hasn't answered, and when no agent can start.
    public static let checkingTitle = "Checking…"
    public static let noAgentTitle = "No agent can start here"

    /// While the computer hasn't said what its agents offer.
    public var checkingModels: String { "Checking Claude Code and Codex on \(machineName)…" }
    /// When neither agent can start here.
    public var noAgent: String { "Neither Claude Code nor Codex can start on \(machineName)." }

    /// What the model menu holds for an agent here. The agent's own models, in
    /// its order and words, with Default first: Claude Code's own `default`
    /// entry, or, for an agent that lists none (Codex), a Default that names no
    /// model. A signed-out agent offers nothing but why. With no models listed,
    /// because the agent couldn't be asked, Default alone.
    public func models(_ agent: StartAgent) -> StartModels {
        guard starter.projects != nil else { return .checking }
        if let reason = missingReason(agent) { return .missing(reason) }
        let status = starter.agents.first { $0.agent == agent }
        if status?.signedIn == false {
            return .signedOut(said(agent) ?? "\(agent.displayName) on \(machineName) isn’t signed in.")
        }
        let offered = (status?.models ?? []).map {
            StartModelChoice(agent: agent, model: $0.value, label: $0.label)
        }
        if offered.contains(where: \.isDefault) { return .choices(offered) }
        return .choices([StartModelChoice(agent: agent, model: nil, label: "Default")] + offered)
    }

    /// Every entry that can be chosen here, agent by agent.
    public var modelChoices: [StartModelChoice] {
        StartAgent.allCases.flatMap { agent -> [StartModelChoice] in
            switch models(agent) {
            case .choices(let choices): return choices
            case .checking, .missing, .signedOut: return []
            }
        }
    }

    /// The entry a start opens on: the Default of the agent last used in the
    /// folder, else of the first agent that can start.
    public func defaultChoice(for folder: StartProject?) -> StartModelChoice? {
        let defaults = modelChoices.filter(\.isDefault)
        return defaults.first { $0.agent == folder?.lastAgent } ?? defaults.first
    }

    /// The model button's words: `Claude Code · Default`, `Codex · GPT-6-Astra`.
    public func title(_ choice: StartModelChoice) -> String {
        return "\(choice.agent.displayName) · \(choice.isDefault ? "Default" : choice.label)"
    }

    /// What Start does: `Starts Claude Code with its default model.`, or with
    /// the chosen model's label.
    public static func starts(_ choice: StartModelChoice) -> String {
        "Starts \(choice.agent.displayName) with \(choice.isDefault ? "its default model" : choice.label)."
    }

    /// Why a start cannot go yet, or nil when it can: a run always starts with a first message, so an empty
    /// one is refused — ccd refuses it too (`start_session.rs`), and this keeps the Start control honest
    /// before the person taps rather than letting the start fail. The one rule both apps' Start surfaces read.
    public static func firstMessageReason(_ message: String) -> String? {
        message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? "Write the first message." : nil
    }
}

/// What the model menu holds for one agent on a computer.
public enum StartModels: Hashable, Sendable {
    /// The computer hasn't answered yet.
    case checking
    /// It can't start there: `missingReason`'s words.
    case missing(String)
    /// It isn't signed in: the sentence that says so, and nothing to choose.
    case signedOut(String)
    case choices([StartModelChoice])
}

/// One entry of the model menu: an agent and the model a start names.
public struct StartModelChoice: Hashable, Sendable {
    public let agent: StartAgent
    /// The listed `value` sent as `start_session.model`; nil names none.
    public let model: String?
    /// The agent's own name for the model, shown as sent and alone.
    public let label: String

    /// The agent's default: no model named, or Claude Code's own `default`,
    /// which the computer passes on as no model either.
    public var isDefault: Bool { model == nil || model == "default" }
}

extension Machines {
    /// Every paired computer that can start a session.
    public var startContexts: [StartContext] {
        models.filter { $0.connection.capabilities?.startsSessions == true }
            .map { StartContext(model: $0) }
    }

    /// The most recent start this phone sent that has no answer and has not been
    /// retry-stopped, across every paired computer. New reopens it instead of a
    /// fresh flow, so a start whose computer went quiet is not silently started
    /// again elsewhere. Found by its request, not by whether its computer can
    /// start right now — so it survives the computer leaving `startContexts`.
    public var outstandingStart: SessionStart? {
        models.flatMap { $0.starter.unanswered }.filter { !$0.retryStopped }
            .max { $0.sentAt < $1.sentAt }
    }
}

/// `main · 4m ago · Claude Code`: the branch, when it was last used and by
/// which agent. A folder that is not a repository has no branch to show.
public func startProjectMeta(_ project: StartProject, now: Date = Date()) -> String {
    var parts: [String] = []
    if let branch = project.gitBranch { parts.append(branch) }
    parts.append(Format.ago(since: project.lastUsed, now: now))
    if let agent = project.lastAgent { parts.append(agent.displayName) }
    return parts.joined(separator: " · ")
}

/// The projects whose name or folder holds the query, in any case; all of
/// them for an empty one.
public func startFilter(_ projects: [StartProject], _ query: String) -> [StartProject] {
    let needle = query.trimmingCharacters(in: .whitespaces).lowercased()
    guard !needle.isEmpty else { return projects }
    return projects.filter {
        $0.name.lowercased().contains(needle) || $0.path.lowercased().contains(needle)
    }
}

extension StartWorktree {
    /// A checkout by its branch.
    public var title: String { branch ?? "Detached HEAD" }

    /// What a start in this checkout would share or meet.
    public func notes(running: Set<String>) -> String? {
        let notes =
            (missing ? ["Its folder is gone"] : [])
            + (running.contains(path) ? ["A session is running here"] : [])
            + (dirty ? ["Uncommitted changes"] : [])
        return notes.isEmpty ? nil : notes.joined(separator: " · ")
    }
}

extension StartPlace {
    /// Where a new worktree's branch starts: Claude Code's own default base,
    /// or for Codex the commit the folder's checkout is on.
    public static func newWorktreeBase(for agent: StartAgent, from here: StartWorktree?) -> String {
        agent == .claude
            ? "Claude Code’s own default base"
            : "the current commit of \(here?.branch ?? "this folder")"
    }
}

/// The chosen place in a word or two: the branch, and where it runs.
public func startPlaceSummary(_ place: StartPlace, agent: StartAgent, project: StartProject?, git: StartGit?)
    -> String
{
    switch place {
    case .folder: return git?.checkout(at: project?.path ?? "").map(\.title) ?? "This folder"
    case .existing(let checkout): return "\(checkout.title) · worktree"
    case .branch(let branch): return "\(branch) · new worktree"
    case .new(let name): return "\(StartPlace.newBranch(name, for: agent)) · new worktree"
    }
}

extension StartFailure {
    /// What a failed start says: its title, the sentence under it where there
    /// is one, and the action back to choosing.
    public func words(for start: SessionStart) -> (title: String, message: String?, action: String) {
        let agent = start.agent.displayName
        switch self {
        case .folderMissing:
            return (
                "That folder isn’t there any more",
                "It was moved, renamed or deleted on \(start.machineName). Nothing was started.",
                "Start again")
        case .agentMissing:
            return (
                "\(agent) isn’t installed",
                "\(start.machineName) has no \(agent). Install it there, then start again from here.",
                "Start again")
        case .launchFailed:
            // One plain headline and no sentence; the computer's own words go
            // under Details.
            return ("\(agent) didn’t start on \(start.machineName).", nil, "Start again")
        }
    }
}
