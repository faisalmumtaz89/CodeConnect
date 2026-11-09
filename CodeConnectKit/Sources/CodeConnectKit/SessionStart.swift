import Foundation
import Observation

/// The agents a session can be started with from the phone.
public enum StartAgent: String, CaseIterable, Identifiable, Sendable {
    case claude, codex

    public var id: String { rawValue }

    /// The product's own name, as the empty fleet prints it.
    public var displayName: String { self == .claude ? "Claude Code" : "Codex" }
}

/// One checkout of a repository, as `project_git` lists it (minor 29): the
/// main checkout first, then its linked worktrees.
public struct StartWorktree: Hashable, Sendable {
    public let path: String
    /// Nil on a detached HEAD.
    public let branch: String?
    public let dirty: Bool
    /// Listed by git, its folder gone: shown, never started in.
    public let missing: Bool
}

extension StartWorktree: Decodable {}

/// `project_git.git`: a repository's checkouts, and the local branches none of
/// them has, newest commit first. The computer lists at most a hundred.
public struct StartGit: Hashable, Sendable {
    let worktrees: [StartWorktree]
    public let branches: [String]
    public let branchesTruncated: Bool

    /// The checkout a folder is in: the one at its path, else the one at its
    /// nearest enclosing folder, whole path segments compared. A project can
    /// be a repository's subfolder, where a session once started.
    public func checkout(at path: String) -> StartWorktree? {
        worktrees
            .filter { $0.path == path || path.hasPrefix($0.path.hasSuffix("/") ? $0.path : $0.path + "/") }
            .max { $0.path.count < $1.path.count }
    }

    public func others(than path: String) -> [StartWorktree] {
        let here = checkout(at: path)
        return worktrees.filter { $0 != here }
    }
}

extension StartGit: Decodable {
    private enum CodingKeys: String, CodingKey {
        case worktrees, branches
        case branchesTruncated = "branches_truncated"
    }
}

/// Where in its repository a start runs (`start_session.worktree`). The
/// project's folder is never switched.
public enum StartPlace: Hashable, Sendable {
    /// The folder as it is.
    case folder
    /// A worktree the repository has.
    case existing(StartWorktree)
    /// A new worktree for a branch no checkout has.
    case branch(String)
    /// A new worktree, made by name.
    case new(String)

    /// The branch a new worktree of this name gets: Claude Code's own
    /// `worktree-<name>`, Codex's the name as typed.
    public static func newBranch(_ name: String, for agent: StartAgent) -> String {
        agent == .claude ? "worktree-\(name)" : name
    }

    /// Claude Code's rule for a worktree's name, which both agents' follow:
    /// `/`-separated parts of letters, digits, `.`, `_` and `-`, none of them
    /// empty, `.` or `..`, at most 64 bytes in all.
    public static func isValidName(_ name: String) -> Bool {
        guard !name.isEmpty, name.utf8.count <= 64 else { return false }
        let allowed = Set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-")
        return name.split(separator: "/", omittingEmptySubsequences: false).allSatisfy { part in
            !part.isEmpty && part != "." && part != ".." && part.allSatisfy(allowed.contains)
        }
    }
}

/// A `list_folder` (minor 30): a folder, or none for the home folder, with or
/// without its dot folders.
public struct StartFolderRequest: Hashable, Sendable {
    public let path: String?
    public let showHidden: Bool

    public init(path: String?, showHidden: Bool) {
        self.path = path
        self.showHidden = showHidden
    }
}

/// `folder_listing`: the folder listed, as an absolute path, and its
/// subfolders, or the computer's own words for why it could not be read.
public struct StartFolderListing: Hashable, Sendable {
    public let path: String
    public let folders: [String]
    public let truncated: Bool
    public let error: String?
}

/// `create_folder_result.result` as it arrives.
struct StartCreateFolderResult: Decodable {
    let status: String
    let detail: String?
}

/// One entry of `ServerMessage::Projects`: a folder an agent has run in on
/// one computer, from that computer's session history.
public struct StartProject: Identifiable, Hashable, Sendable {
    public let path: String
    public let name: String
    public let lastUsed: Date
    public let lastAgent: StartAgent?
    public let gitBranch: String?
    public let exists: Bool

    public init(
        path: String, name: String, lastUsed: Date, lastAgent: StartAgent?, gitBranch: String?, exists: Bool
    ) {
        self.path = path
        self.name = name
        self.lastUsed = lastUsed
        self.lastAgent = lastAgent
        self.gitBranch = gitBranch
        self.exists = exists
    }

    public var id: String { path }

    /// The folder as a shell prints it: `~/code/infra`.
    public var shownPath: String {
        let parts = path.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count >= 3, parts[0].isEmpty, parts[1] == "Users" || parts[1] == "home"
        else { return path }
        return (["~"] + parts.dropFirst(3)).joined(separator: "/")
    }
}

/// One entry of `ServerMessage::Projects.agents`: whether an agent is
/// installed on the computer, and the computer's own words when it is not;
/// from minor 33, the models it offers a start and whether it is signed in.
public struct StartAgentStatus: Hashable, Sendable {
    public let agent: StartAgent
    let available: Bool
    public let detail: String?
    /// In the agent's own order and words; empty from a daemon below minor 33,
    /// and when the agent could not be asked.
    public let models: [StartModel]
    /// Nil when not known, and from a daemon below minor 33.
    public let signedIn: Bool?

    /// As it arrives: the agent's name may be one this build does not know.
    struct Wire: Decodable {
        let agent: String
        let available: Bool
        let detail: String?
        let models: [StartModel]?
        let signedIn: Bool?

        enum CodingKeys: String, CodingKey {
            case agent, available, detail, models
            case signedIn = "signed_in"
        }

        var status: StartAgentStatus? {
            StartAgent(rawValue: agent).map {
                StartAgentStatus(
                    agent: $0, available: available, detail: detail, models: models ?? [], signedIn: signedIn)
            }
        }
    }
}

/// One model an agent offers a start (minor 33): `value` is what a start
/// names, `label` and `detail` the agent's own words, shown as sent.
public struct StartModel: Hashable, Sendable, Decodable {
    public let value: String
    public let label: String
    public let detail: String?
}

extension StartProject: Decodable {
    private enum CodingKeys: String, CodingKey {
        case path, name, exists
        case lastUsed = "last_used"
        case lastAgent = "last_agent"
        case gitBranch = "git_branch"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        path = try c.decode(String.self, forKey: .path)
        name = try c.decode(String.self, forKey: .name)
        lastUsed = ISO8601.parse(try c.decode(String.self, forKey: .lastUsed)) ?? .distantPast
        // An agent this build does not know was still last used here; it is
        // simply not one the phone can preselect.
        lastAgent = (try c.decodeIfPresent(String.self, forKey: .lastAgent)).flatMap(StartAgent.init)
        gitBranch = try c.decodeIfPresent(String.self, forKey: .gitBranch)
        exists = try c.decode(Bool.self, forKey: .exists)
    }
}

/// `StartSessionResult.outcome.Failed.reason`.
public enum StartFailure: String, Sendable {
    case agentMissing = "agent_missing"
    case folderMissing = "folder_missing"
    case launchFailed = "launch_failed"
}

/// `StartSessionResult.result`.
public enum StartOutcome: Hashable, Sendable {
    /// `cwd` is the folder the agent started in (minor 29), absent when the
    /// agent makes it: a new Claude Code worktree, whose folder the fleet
    /// shows once the session's first hook names it.
    case started(sessionUID: String, cwd: String? = nil)
    case failed(StartFailure, detail: String)
}

extension StartOutcome: Decodable {
    private enum CodingKeys: String, CodingKey {
        case status, reason, detail
        case sessionUID = "session_uid"
        case cwd
    }

    /// A reason or a status this build has never heard of is still a start that
    /// did not happen, told in the computer's own words — never a Starting… that
    /// waits for an answer that has already come.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let status = try c.decode(String.self, forKey: .status)
        if status == "started" {
            self = .started(
                sessionUID: try c.decode(String.self, forKey: .sessionUID),
                cwd: try c.decodeIfPresent(String.self, forKey: .cwd))
            return
        }
        let detail = try c.decodeIfPresent(String.self, forKey: .detail) ?? status
        let reason = (try c.decodeIfPresent(String.self, forKey: .reason)).flatMap(StartFailure.init)
        self = .failed(status == "failed" ? reason ?? .launchFailed : .launchFailed, detail: detail)
    }
}

/// One `StartSession` the phone sent, and what came back.
public struct SessionStart: Identifiable, Hashable {
    public let requestID: String
    public let project: StartProject
    public let place: StartPlace
    public let agent: StartAgent
    /// The model the start names (a listed `value`); nil names none.
    public let model: String?
    public let prompt: String?
    public let machineName: String
    public let sentAt: Date
    public var outcome: StartOutcome?

    public var id: String { requestID }

    /// The folder the session runs in: the computer's word once it has
    /// started, else what is known before: a new worktree's is made by the
    /// computer or by the agent.
    public var folder: String? {
        if case .started(_, let cwd?) = outcome { return cwd }
        switch place {
        case .folder: return project.path
        case .existing(let worktree): return worktree.path
        case .branch, .new: return nil
        }
    }
}

/// The phone's side of starting a session: the computer's projects, and every
/// start sent from this phone with its outcome.
///
/// It holds what the computer said and nothing else: `AppModel` sends the
/// requests and hands the replies here.
@MainActor
@Observable
public final class SessionStarter {
    /// Most recently used first; nil until the computer has answered.
    public private(set) var projects: [StartProject]?
    /// Which agents this computer has; empty until it has answered.
    public private(set) var agents: [StartAgentStatus] = []

    public var availableAgents: Set<StartAgent> { Set(agents.filter(\.available).map(\.agent)) }
    public private(set) var starts: [String: SessionStart] = [:]
    /// `project_git` by project path; nil inside once answered for a folder
    /// that is not a repository.
    public private(set) var git: [String: StartGit?] = [:]
    /// `folder_listing` replies (minor 30), by what was asked: the folder (nil
    /// for the home folder) and whether dot folders were shown.
    public private(set) var listings: [StartFolderRequest: StartFolderListing] = [:]

    /// The latest `list_folder` sent for each ask, by request id.
    private var asked: [String: StartFolderRequest] = [:]
    /// `create_folder_result` by request id: nil inside once made, else the
    /// computer's words.
    public private(set) var creates: [String: String?] = [:]

    /// Folders whose repository was asked about and not yet answered.
    public private(set) var gitAsked: Set<String> = []

    public init() {}

    /// A fresh read: what the computer said before is no longer the answer.
    public func askedGit(_ path: String) {
        gitAsked.insert(path)
        git[path] = nil
    }
    public func receive(git: StartGit?, for path: String) {
        gitAsked.remove(path)
        self.git[path] = .some(git)
    }
    public func receive(_ listing: StartFolderListing, for request: StartFolderRequest) { listings[request] = listing }

    /// A `list_folder` sent; any earlier ask for the same folder is now stale.
    public func ask(_ request: StartFolderRequest, as requestID: String) {
        asked = asked.filter { $0.value != request }
        asked[requestID] = request
    }

    /// The answer to the latest ask is shown; an earlier one's is not.
    public func receive(_ listing: StartFolderListing, answering requestID: String) {
        guard let request = asked.removeValue(forKey: requestID) else { return }
        listings[request] = listing
    }

    /// Asks sent and not yet answered, to ask again after a reconnect.
    public var unansweredListings: [StartFolderRequest] { Array(Set(asked.values)) }

    public func resolveCreate(_ requestID: String, refusal: String?) {
        guard creates[requestID] == nil else { return }
        creates[requestID] = .some(refusal)
    }

    /// The fixture answer to `list_projects`: a Mac's forty folders, or a
    /// Linux server's few. `-CC_START_AGENTS claude` leaves Codex uninstalled;
    /// `-CC_START_AGENTS_WIRE <agents>` answers with a recorded frame's `agents`
    /// as given (fixtures/claude/minor-33-wire.json), models and sign-in included.
    public func listFixtureProjects(linux: Bool) {
        projects = linux ? Self.fixtureLinuxProjects(now: Date()) : Self.fixtureProjects(now: Date())
        var codex = true
        #if DEBUG
            codex = UserDefaults.standard.string(forKey: "CC_START_AGENTS") != "claude"
            let arguments = ProcessInfo.processInfo.arguments
            if let flag = arguments.firstIndex(of: "-CC_START_AGENTS_WIRE"), flag + 1 < arguments.count,
                let wire = try? JSONDecoder().decode([StartAgentStatus.Wire].self, from: Data(arguments[flag + 1].utf8))
            {
                agents = wire.compactMap(\.status)
                return
            }
        #endif
        agents = [
            StartAgentStatus(agent: .claude, available: true, detail: nil, models: [], signedIn: nil),
            StartAgentStatus(
                agent: .codex, available: codex, detail: codex ? nil : "codex is not installed", models: [],
                signedIn: nil),
        ]
    }

    /// What `projects` said.
    public func receive(projects: [StartProject], agents: [StartAgentStatus]) {
        self.projects = projects
        self.agents = agents
    }

    /// What `start_session_result` said about one start. The first answer is
    /// the answer: a repeat after a resend says the same thing again.
    public func resolve(_ requestID: String, _ outcome: StartOutcome) {
        guard starts[requestID]?.outcome == nil else { return }
        starts[requestID]?.outcome = outcome
    }

    /// Starts sent and not yet answered, to send again, unchanged, after a
    /// reconnect.
    public var unanswered: [SessionStart] {
        starts.values.filter { $0.outcome == nil }.sorted { $0.sentAt < $1.sentAt }
    }

    /// Send one start. The request id is what makes a double tap or a resend
    /// after a reconnect one session rather than two.
    @discardableResult
    public func start(
        project: StartProject, place: StartPlace = .folder, agent: StartAgent, model: String? = nil, prompt: String,
        machineName: String
    ) -> String {
        let requestID = UUID().uuidString.lowercased()
        let trimmed = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        starts[requestID] = SessionStart(
            requestID: requestID, project: project, place: place, agent: agent, model: model,
            prompt: trimmed.isEmpty ? nil : trimmed, machineName: machineName, sentAt: Date())
        return requestID
    }

    // MARK: Fixtures

    /// Forty folders on one Mac: the long paths, long branch names and the
    /// folder that is not a repository are the shapes a real history holds.
    static func fixtureProjects(now: Date) -> [StartProject] {
        let home = "/Users/jordan"
        let named: [(String, String?, TimeInterval, StartAgent?)] = [
            ("code/query-parser", "main", 4 * 60, .claude),
            ("code/storefront-api", "feature/checkout-v2", 52 * 60, .claude),
            ("code/ios-app", "release/1.4", 3 * 3600, .codex),
            ("code/clients/northwind-logistics/platform/services/payments-reconciliation-worker",
             "fix/idempotent-ledger-replay-after-partial-failure", 5 * 3600, .claude),
            ("code/docs-site", "main", 26 * 3600, .claude),
            ("code/web-dashboard", "feature/oauth-device-flow-and-token-refresh", 30 * 3600, .codex),
            ("Documents/experiments/2026-q4/llm-eval-harness-with-a-very-long-folder-name", "main",
             2 * 86400, .claude),
            ("code/data-pipeline", "main", 2 * 86400 + 3600, .claude),
            ("code/old-billing-service", "main", 3 * 86400, .claude),
            ("dotfiles", nil, 3 * 86400 + 7200, .claude),
        ]
        let others = [
            "infra-terraform", "mobile-design-system", "analytics-etl", "auth-service",
            "search-indexer", "notification-relay", "marketing-site", "cli-tools",
            "terraform-modules", "k8s-manifests", "feature-flags", "pdf-renderer",
            "image-resizer", "rate-limiter", "graphql-gateway", "admin-console", "billing-ui",
            "email-templates", "load-tests", "chaos-experiments", "status-page",
            "sdk-typescript", "sdk-python", "sdk-swift", "android-app", "desktop-electron",
            "browser-extension", "embeddings-service", "vector-store-bench", "release-tools",
        ]
        let rows =
            named
            + others.enumerated().map { index, name in
                ("code/\(name)", index % 4 == 3 ? "develop" : "main",
                 TimeInterval(4 + index) * 86400, index % 5 == 2 ? StartAgent.codex : .claude)
            }
        return rows.map { relative, branch, age, agent in
            let path = "\(home)/\(relative)"
            return StartProject(
                path: path, name: String(relative.split(separator: "/").last ?? ""),
                lastUsed: now.addingTimeInterval(-age), lastAgent: agent, gitBranch: branch,
                exists: true)
        }
    }
}

extension SessionStarter {
    /// `folder_listing` for a fixture folder, as minor 30 answers it: the
    /// home folder for none, names in byte order, dot folders only when asked,
    /// and `/private` unreadable.
    public static func fixtureListing(_ request: StartFolderRequest, projects: [StartProject], made: [String: [String]])
        -> StartFolderListing
    {
        let home = projects.first.map { "/" + $0.path.split(separator: "/").prefix(2).joined(separator: "/") } ?? "/"
        let path = request.path ?? home
        if path == "/private" {
            return StartFolderListing(path: path, folders: [], truncated: false, error: "Permission denied")
        }
        let names = (fixtureFolders(projects: projects)[path] ?? []) + (made[path] ?? [])
        return StartFolderListing(
            path: path, folders: names.filter { request.showHidden || !$0.hasPrefix(".") }.sorted(),
            truncated: false, error: nil)
    }

    /// The fixture computer's folders, each with its subfolders: every
    /// project's folder up to `/`, the home folder's usual others, and dot
    /// folders.
    public static func fixtureFolders(projects: [StartProject]) -> [String: [String]] {
        var tree: [String: Set<String>] = [:]
        func add(_ path: String) {
            var parts = path.split(separator: "/").map(String.init)
            while let child = parts.popLast() {
                tree["/" + parts.joined(separator: "/"), default: []].insert(child)
            }
        }
        projects.forEach { add($0.path) }
        let home = projects.first.map { "/" + $0.path.split(separator: "/").prefix(2).joined(separator: "/") }
        ["/Applications", "/Library", "/System", "/opt/homebrew", "/private", "/usr/local", "/Users/Shared", "/.vol"]
            .forEach(add)
        if let home {
            ["Desktop", "Documents/notes", "Downloads", "Library", "Movies", "Music", "Pictures", ".config/git",
             ".ssh", ".cache", "code/.archive", "code/query-parser/src", "code/query-parser/docs"]
                .forEach { add("\(home)/\($0)") }
        }
        return tree.mapValues { $0.sorted { $0.localizedStandardCompare($1) == .orderedAscending } }
    }

    /// `project_git` for a fixture folder, or any folder inside one, as git
    /// answers from anywhere in a repository: query-parser with uncommitted
    /// changes, two worktrees (one with changes), one whose folder is gone and
    /// more branches than the computer lists; the payments worker on its long
    /// branch; the rest on their one branch. Nil outside a repository.
    public static func fixtureGit(_ folder: String, projects: [StartProject]) -> StartGit? {
        guard
            let project = projects.first(where: { folder == $0.path || folder.hasPrefix($0.path + "/") }),
            let branch = project.gitBranch
        else { return nil }
        let path = project.path
        let trees = "\(path)/.claude/worktrees"
        func checkout(_ path: String, _ branch: String?, dirty: Bool = false, missing: Bool = false) -> StartWorktree {
            StartWorktree(path: path, branch: branch, dirty: dirty, missing: missing)
        }
        switch project.name {
        case "query-parser":
            return StartGit(
                worktrees: [
                    checkout(path, branch, dirty: true),
                    checkout("\(trees)/strict-mode", "feature/strict-mode"),
                    checkout("\(trees)/fix+unicode-escapes", "fix/unicode-escapes", dirty: true),
                    checkout("\(path)-old", "wt/gone", missing: true),
                ],
                branches: ["perf/arena-allocator", "docs/grammar-reference", "spike/wasm-target"],
                branchesTruncated: true)
        case "payments-reconciliation-worker":
            return StartGit(
                worktrees: [checkout(path, branch)], branches: ["release/2026.11", "main"],
                branchesTruncated: false)
        default:
            return StartGit(
                worktrees: [checkout(path, branch)], branches: branch == "main" ? ["develop"] : ["main"],
                branchesTruncated: false)
        }
    }

    /// A Linux build server's few projects, in the home folder.
    static func fixtureLinuxProjects(now: Date) -> [StartProject] {
        let rows: [(String, String?, TimeInterval, StartAgent?)] = [
            ("data-pipeline", "main", 20 * 60, .codex),
            ("ml-eval", "eval/q4-regression-suite", 2 * 3600, .claude),
            ("gpu-benchmarks", "main", 26 * 3600, .claude),
            ("log-shipper", "main", 4 * 86400, .codex),
            ("nightly-builds", nil, 9 * 86400, .claude),
        ]
        return rows.map { name, branch, age, agent in
            StartProject(
                path: "/home/jordan/\(name)", name: name, lastUsed: now.addingTimeInterval(-age),
                lastAgent: agent, gitBranch: branch, exists: true)
        }
    }
}
