import Foundation

/// One account an agent can start on, kept by the computer (`accounts`, minor 47): its id, its agent, the name the
/// person gave it, and whether its agent says it is signed in.
public struct AgentAccount: Decodable, Hashable, Sendable, Identifiable {
    /// The computer's own id for it.
    public let id: String
    /// The agent as the computer names it; one this build does not know is kept and shown nowhere.
    public let agent: String
    public let label: String
    /// Whether its agent says it is signed in; nil when the computer does not know.
    public let signedIn: Bool?
    /// A sign-in for it is going on the computer, waiting on the browser.
    public let signingIn: Bool

    private enum CodingKeys: String, CodingKey {
        case id, agent, label
        case signedIn = "signed_in"
        case signingIn = "signing_in"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        agent = try c.decode(String.self, forKey: .agent)
        label = try c.decode(String.self, forKey: .label)
        signedIn = try c.decodeIfPresent(Bool.self, forKey: .signedIn)
        // Omitted when false.
        signingIn = try c.decodeIfPresent(Bool.self, forKey: .signingIn) ?? false
    }
}

/// The account an agent's new starts use.
public struct ActiveAccount: Decodable, Hashable, Sendable {
    public let agent: String
    public let accountID: String

    private enum CodingKeys: String, CodingKey {
        case agent
        case accountID = "account_id"
    }
}

/// Each agent's accounts on one computer, oldest first, and the one its new starts use (`accounts` reply). An agent
/// missing from `active` starts on its Default: the agent as it is on that computer.
public struct AccountBook: Hashable, Sendable {
    public let accounts: [AgentAccount]
    public let active: [ActiveAccount]

    public init(accounts: [AgentAccount], active: [ActiveAccount]) {
        self.accounts = accounts
        self.active = active
    }

    /// `agent`'s accounts, oldest first, Default not among them.
    public func accounts(for agent: StartAgent) -> [AgentAccount] {
        accounts.filter { $0.agent == agent.rawValue }
    }

    /// The account `agent`'s next starts use; nil is its Default.
    public func activeID(for agent: StartAgent) -> String? {
        active.first { $0.agent == agent.rawValue }?.accountID
    }
}

/// An accounts request the computer refused, in its own words (`accounts_refused`, naming the request).
public struct AccountsRefusal: LocalizedError, Sendable, Equatable {
    public let message: String
    public var errorDescription: String? { message }
}
