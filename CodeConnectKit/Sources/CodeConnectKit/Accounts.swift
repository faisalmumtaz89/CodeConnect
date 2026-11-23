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
    /// Whether plan limits apply to this sign-in, as the agent says: false for one with none ("No plan limits"); nil
    /// while not reported.
    public let limitsAvailable: Bool?
    /// When the computer read them (RFC 3339), for an "as of".
    public let readAt: String

    private enum CodingKeys: String, CodingKey {
        case email, plan, windows
        case limitsAvailable = "limits_available"
        case readAt = "read_at"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        email = try c.decodeIfPresent(String.self, forKey: .email)
        plan = try c.decodeIfPresent(String.self, forKey: .plan)
        windows = try c.decodeIfPresent([UsageWindow].self, forKey: .windows) ?? []
        limitsAvailable = try c.decodeIfPresent(Bool.self, forKey: .limitsAvailable)
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

/// The Accounts page's words for a switch in progress and for a row's quiet second line (design A). A switch is lazy:
/// each session moves with its next message, and one still in a turn finishes it first.
public enum AccountSwitchWords {
    /// Under the group while sessions are moving to `target`; nil with none live.
    public static func sentence(to target: String, waiting: Int, moved: Int) -> String? {
        let total = waiting + moved
        guard total > 0 else { return nil }
        let move = total == 1
            ? "1 session will use \(target) from its next message."
            : "\(total) sessions will use \(target) from their next message."
        switch waiting {
        case 0: return move
        case 1 where total == 1: return "\(move) It finishes its turn first."
        case 1: return "\(move) 1 finishes its turn first."
        default: return "\(move) \(waiting) finish their turn first."
        }
    }

    /// In amber under the group: the sessions that couldn't start on `target`, named when one; nil with none. The
    /// service's own sentence is in each conversation, where its warning row stands.
    public static func failure(to target: String, sessions: [String]) -> String? {
        switch sessions.count {
        case 0: return nil
        case 1: return "\(sessions[0]) couldn't start on \(target). Its conversation says why."
        default: return "\(sessions.count) sessions couldn't start on \(target). Their conversations say why."
        }
    }

    /// A usage window's caption, from its own length as the service names it.
    public static func caption(minutes: Int) -> String {
        switch minutes {
        case 300: "5-hour"
        case 10080: "Week"
        case let m where m % 1440 == 0: "\(m / 1440)-day"
        case let m where m % 60 == 0: "\(m / 60)-hour"
        default: "\(minutes)-minute"
        }
    }

    /// In a row's figures' place: its agent says the plan has no limits.
    public static let noPlanLimits = "No plan limits"
    /// In a row's figures' place: its agent hasn't said yet.
    public static let notReported = "Not reported yet"
    /// A row's second line while its sign-in waits on the browser; the computer ends it after ten minutes.
    public static let signingIn = "Finish signing in in your browser. This stops by itself after 10 minutes."

    /// A row's second line when another account is signed in as the same address.
    public static func sameSignIn(as email: String) -> String { "The same sign-in as \(email)" }

    /// A row's second line when Codex keeps its sign-in in the computer's own store.
    public static let notSeparate = "Uses this computer's own Codex sign-in, so it isn't a separate account"
}

/// A switch in progress for one agent's sessions on a computer: the account they move to (its email where its agent
/// says it, else its name), how many still finish a turn first, how many move with their next message, and the ones
/// whose start there was refused, by their folder's name.
public struct AccountSwitch: Equatable, Sendable {
    public let target: String
    public let waiting: Int
    public let moved: Int
    public let failed: [String]

    public init(target: String, waiting: Int, moved: Int, failed: [String]) {
        self.target = target
        self.waiting = waiting
        self.moved = moved
        self.failed = failed
    }

    public var sentence: String? { AccountSwitchWords.sentence(to: target, waiting: waiting, moved: moved) }
    public var failure: String? { AccountSwitchWords.failure(to: target, sessions: failed) }
}
