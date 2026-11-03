import Foundation
import Observation

/// The agents a session can be started with from the phone.
enum StartAgent: String, CaseIterable, Identifiable, Sendable {
    case claude, codex

    var id: String { rawValue }

    /// The product's own name, as the empty fleet prints it.
    var displayName: String { self == .claude ? "Claude Code" : "Codex" }
}

/// One entry of `ServerMessage::Projects`: a folder an agent has run in on
/// one computer, from that computer's session history.
struct StartProject: Identifiable, Hashable, Sendable {
    let path: String
    let name: String
    let lastUsed: Date
    let lastAgent: StartAgent?
    let gitBranch: String?
    let exists: Bool

    var id: String { path }

    /// The folder as a shell prints it: `~/code/infra`.
    var shownPath: String {
        let parts = path.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count >= 3, parts[0].isEmpty, parts[1] == "Users" || parts[1] == "home"
        else { return path }
        return (["~"] + parts.dropFirst(3)).joined(separator: "/")
    }

    /// The agent to offer first: the one last used here, when it is still
    /// installed, else the first one that is.
    func preferredAgent(among available: Set<StartAgent>) -> StartAgent? {
        if let lastAgent, available.contains(lastAgent) { return lastAgent }
        return StartAgent.allCases.first { available.contains($0) }
    }
}

/// One entry of `ServerMessage::Projects.agents`: whether an agent is
/// installed on the computer, and the computer's own words when it is not.
struct StartAgentStatus: Hashable, Sendable {
    let agent: StartAgent
    let available: Bool
    let detail: String?
}

/// `StartSessionResult.outcome.Failed.reason`.
enum StartFailure: String, Sendable {
    case agentMissing = "agent_missing"
    case folderMissing = "folder_missing"
    case launchFailed = "launch_failed"
}

/// `StartSessionResult.outcome`.
enum StartOutcome: Hashable, Sendable {
    case started(sessionUID: String)
    case failed(StartFailure, detail: String)
}

/// One `StartSession` the phone sent, and what came back.
struct SessionStart: Identifiable, Hashable {
    let requestID: String
    let project: StartProject
    let agent: StartAgent
    let prompt: String?
    let machineName: String
    let sentAt: Date
    var outcome: StartOutcome?

    var id: String { requestID }
}

/// The phone's side of starting a session: the computer's projects, and every
/// start sent from this phone with its outcome.
///
/// Answered from fixtures until the daemon speaks `list_projects` and
/// `start_session`; the shapes are the protocol's, so the wire swaps in here.
@MainActor
@Observable
final class SessionStarter {
    /// Most recently used first; nil until the computer has answered.
    private(set) var projects: [StartProject]?
    /// Which agents this computer has; empty until it has answered.
    private(set) var agents: [StartAgentStatus] = []

    var availableAgents: Set<StartAgent> { Set(agents.filter(\.available).map(\.agent)) }
    private(set) var starts: [String: SessionStart] = [:]

    func listProjects(fixtures: Bool) {
        guard fixtures else { return }
        projects = Self.fixtureProjects(now: Date())
        agents = [
            StartAgentStatus(agent: .claude, available: true, detail: nil),
            StartAgentStatus(agent: .codex, available: false, detail: "codex is not installed"),
        ]
    }

    /// Send one start. The request id is what makes a double tap or a resend
    /// after a reconnect one session rather than two.
    @discardableResult
    func start(
        project: StartProject, agent: StartAgent, prompt: String, machineName: String,
        fixtures: Bool
    ) -> String {
        let requestID = UUID().uuidString.lowercased()
        let trimmed = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        starts[requestID] = SessionStart(
            requestID: requestID, project: project, agent: agent,
            prompt: trimmed.isEmpty ? nil : trimmed, machineName: machineName, sentAt: Date())
        if fixtures { answerFromFixture(requestID) }
        return requestID
    }

    // MARK: Fixtures

    /// A fixture start stays starting — no session ever arrives — unless
    /// `CC_START_OUTCOME` names the failure it should end in.
    private func answerFromFixture(_ requestID: String) {
        #if DEBUG
            guard let wanted = UserDefaults.standard.string(forKey: "CC_START_OUTCOME"),
                let reason = StartFailure(rawValue: wanted),
                let start = starts[requestID]
            else { return }
            let detail: String
            switch reason {
            case .folderMissing: detail = "cd: no such file or directory: \(start.project.path)"
            case .agentMissing: detail = "zsh: command not found: \(start.agent.rawValue)"
            case .launchFailed:
                detail = "/Users/jordan/.zshrc:42: command not found: nvm\nlogin shell exited with status 127"
            }
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(600))
                starts[requestID]?.outcome = .failed(reason, detail: detail)
            }
        #endif
    }

    /// Forty folders on one computer that has Claude Code and no Codex: the
    /// long paths, long branch names and the folder that is not a repository
    /// are the shapes a real history holds.
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

/// The five ways of starting a session the prototype offers, one of which the
/// owner keeps. Chosen with `-CC_START_OPTION 1…5` in a debug build.
enum StartOption: Int, CaseIterable {
    /// A "New session" word in the bar; one sheet with recents, agent, message.
    case recentsSheet = 1
    /// A round + over the fleet; project, agent and message as three steps.
    case steps
    /// "New session" on every project card, and recent projects under them.
    case cardAction
    /// A composer at the foot of the fleet: the first message starts it.
    case messageFirst
    /// A search field under the title; each result starts an agent directly.
    case searchFirst

    static var current: StartOption {
        #if DEBUG
            if let option = StartOption(rawValue: UserDefaults.standard.integer(forKey: "CC_START_OPTION")) {
                return option
            }
        #endif
        return .recentsSheet
    }
}
