import Foundation

/// One account an agent can start on, kept by the computer (`accounts`, minor 48): its id, its agent, the name the
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
    /// Its agent says it is signed in, but with the computer's own credential, so it is not a separate account.
    public let notSeparate: Bool
    /// Another of its agent's accounts on the computer, the Default included, is signed in as the same address.
    public let sameAsAnother: Bool
    /// What its agent says of it; nil until the computer has read it.
    public let facts: AccountFacts?

    private enum CodingKeys: String, CodingKey {
        case id, agent, label, facts
        case signedIn = "signed_in"
        case signingIn = "signing_in"
        case notSeparate = "not_separate"
        case sameAsAnother = "same_as_another"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        agent = try c.decode(String.self, forKey: .agent)
        label = try c.decode(String.self, forKey: .label)
        signedIn = try c.decodeIfPresent(Bool.self, forKey: .signedIn)
        // Each omitted when false.
        signingIn = try c.decodeIfPresent(Bool.self, forKey: .signingIn) ?? false
        notSeparate = try c.decodeIfPresent(Bool.self, forKey: .notSeparate) ?? false
        sameAsAnother = try c.decodeIfPresent(Bool.self, forKey: .sameAsAnother) ?? false
        facts = try c.decodeIfPresent(AccountFacts.self, forKey: .facts)
    }
}

/// What an agent says of one of its accounts, read by the computer on demand and held in its memory only: the address,
/// the plan in the agent's own word, and each usage window it names (a window it doesn't name is absent).
public struct AccountFacts: Decodable, Hashable, Sendable {
    public let email: String?
    public let plan: String?
    public let windows: [UsageWindow]
    /// When the computer read them (RFC 3339), for an "as of".
    public let readAt: String

    private enum CodingKeys: String, CodingKey {
        case email, plan, windows
        case readAt = "read_at"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        email = try c.decodeIfPresent(String.self, forKey: .email)
        plan = try c.decodeIfPresent(String.self, forKey: .plan)
        windows = try c.decodeIfPresent([UsageWindow].self, forKey: .windows) ?? []
        readAt = try c.decode(String.self, forKey: .readAt)
    }
}

/// One usage window as the agent names it: the share used, its length, when it resets.
public struct UsageWindow: Decodable, Hashable, Sendable {
    public let usedPercent: Int
    public let windowMinutes: Int
    /// Unix seconds.
    public let resetsAt: Int

    public init(usedPercent: Int, windowMinutes: Int, resetsAt: Int) {
        self.usedPercent = usedPercent
        self.windowMinutes = windowMinutes
        self.resetsAt = resetsAt
    }

    private enum CodingKeys: String, CodingKey {
        case usedPercent = "used_percent"
        case windowMinutes = "window_minutes"
        case resetsAt = "resets_at"
    }
}

/// An agent's Default (the agent as it is on the computer), with what it says of it.
public struct DefaultAccount: Decodable, Hashable, Sendable {
    public let agent: String
    public let facts: AccountFacts
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
    /// What each agent's Default says of itself, where the computer has read it.
    public let defaults: [DefaultAccount]

    public init(accounts: [AgentAccount], active: [ActiveAccount], defaults: [DefaultAccount] = []) {
        self.accounts = accounts
        self.active = active
        self.defaults = defaults
    }

    /// What `agent`'s Default says of itself; nil until read.
    public func defaultFacts(for agent: StartAgent) -> AccountFacts? {
        defaults.first { $0.agent == agent.rawValue }?.facts
    }

    /// The facts of the account `accountID` names (nil: the Default); nil for one unknown, or since removed.
    public func facts(for agent: StartAgent, accountID: String?) -> AccountFacts? {
        guard let accountID else { return defaultFacts(for: agent) }
        return accounts.first { $0.id == accountID && $0.agent == agent.rawValue }?.facts
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
