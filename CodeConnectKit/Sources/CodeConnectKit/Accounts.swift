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

    /// Signed out, as a row says it: its agent says it isn't signed in, and no sign-in for it is going on (the computer
    /// says `signed_in: false` while the person is still in the browser). Not while unread, nor when not separate.
    public var signedOut: Bool { signedIn == false && !signingIn && !notSeparate }

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
/// the plan in the agent's own word, and each usage window it names (a window it doesn't name is absent; of
/// Antigravity's, only the Gemini models' week).
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
    /// Unix seconds; 0 for a window the agent has not opened yet (Antigravity's, unused).
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

/// The sign-in of an agent that keeps one and no accounts of CodeConnect's (Antigravity's, minor 51), as the computer
/// last read it: its plan is its Default's facts'.
public struct AgentSignIn: Decodable, Hashable, Sendable {
    public let agent: String
    /// Whether it is signed in; nil while the computer does not know.
    public let signedIn: Bool?
    /// A sign-in for it is going on the computer, waiting on the browser.
    public let signingIn: Bool

    private enum CodingKeys: String, CodingKey {
        case agent
        case signedIn = "signed_in"
        case signingIn = "signing_in"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        agent = try c.decode(String.self, forKey: .agent)
        signedIn = try c.decodeIfPresent(Bool.self, forKey: .signedIn)
        signingIn = try c.decodeIfPresent(Bool.self, forKey: .signingIn) ?? false
    }

    /// The words its one row says signed in: its plan as the agent names it (`plan`, passed through as sent), as Claude
    /// Code's row says "Max"; "Signed in" when it names none; nil otherwise, where the row says it is signed out or
    /// signing in.
    public func line(plan: String?) -> String? {
        guard signedIn == true, !signingIn else { return nil }
        return plan ?? "Signed in"
    }

    /// Whether its Default can be chosen: not while the agent says it is signed out, a sign-in going on or not, so no
    /// session moves onto a Default that can't start ([`AppModel.chooseDefault`]).
    public var choosable: Bool { signedIn != false }

    /// Signed out, as its row says it: the agent says so and no sign-in is going on.
    public var signedOut: Bool { signedIn == false && !signingIn }

    /// Its row's acts: Sign In… signed out, Sign Out… signed in, none while not known or signing in.
    public var acts: [AccountRowAct] {
        if signingIn { return [] }
        switch signedIn {
        case true?: return [.signOut]
        case false?: return [.signIn]
        case nil: return []
        }
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
    /// What each agent's Default says of itself, where the computer has read it.
    public let defaults: [DefaultAccount]
    /// The sign-in of each agent that keeps one and no accounts (minor 51: Antigravity).
    public let agentSignIns: [AgentSignIn]

    public init(
        accounts: [AgentAccount], active: [ActiveAccount], defaults: [DefaultAccount] = [],
        agentSignIns: [AgentSignIn] = []
    ) {
        self.accounts = accounts
        self.active = active
        self.defaults = defaults
        self.agentSignIns = agentSignIns
    }

    /// `agent`'s own sign-in, where the computer has said it.
    public func agentSignIn(for agent: StartAgent) -> AgentSignIn? {
        agentSignIns.first { $0.agent == agent.rawValue }
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
/// each open session moves with its next message, once nothing is running in it.
public enum AccountSwitchWords {
    /// Under the group while sessions are due to move to `target`, each with its next message; nil with none.
    public static func sentence(to target: String, waiting: Int, moved: Int) -> String? {
        guard waiting + moved > 0 else { return nil }
        return "Open sessions move to \(target) with their next message."
    }

    /// In amber under the group: the sessions that couldn't switch, named when one; nil with none. No account is
    /// named: the one a session failed on may since be removed or replaced, and its conversation says which and what
    /// to do (the service keeps no raw reason there).
    public static func failure(sessions: [String]) -> String? {
        switch sessions.count {
        case 0: return nil
        case 1: return "\(sessions[0]) couldn't switch accounts. Its conversation says what to do."
        default: return "\(sessions.count) sessions couldn't switch accounts. Their conversations say what to do."
        }
    }

    /// A usage window's caption, from its own length as the service names it.
    public static func caption(minutes: Int) -> String {
        switch minutes {
        case 300: "5-hour"
        case 10080: "Weekly"
        case let m where m % 1440 == 0: "\(m / 1440)-day"
        case let m where m % 60 == 0: "\(m / 60)-hour"
        default: "\(minutes)-minute"
        }
    }

    /// Beneath a window's figure, how long until it resets: "resets in Nd Hh" from two days on (whole hours, rounded
    /// down; "resets in 3d" on the day), "resets in Nh Mm" from an hour ("resets in 5h" on the hour), "resets in N min" under an hour (at least 1);
    /// nil once the time has come, until the computer reads the window again.
    public static func resets(_ window: UsageWindow, now: Date) -> String? {
        if window.resetsAt == 0 { return "not started" }
        return countdown(window, now: now).map { "resets \($0)" }
    }

    /// How long until a window resets, as `resets` says it after "resets ": "in 2h 10m"; nil once it has.
    public static func countdown(_ window: UsageWindow, now: Date) -> String? {
        let left = window.resetsAt - Int(now.timeIntervalSince1970.rounded(.down))
        guard left > 0 else { return nil }
        if left >= 2 * 86400 {
            let hours = left % 86400 / 3600
            return "in \(left / 86400)d" + (hours > 0 ? " \(hours)h" : "")
        }
        if left >= 3600 {
            let minutes = left % 3600 / 60
            return "in \(left / 3600)h" + (minutes > 0 ? " \(minutes)m" : "")
        }
        return "in \(max(1, left / 60)) min"
    }

    /// In a row's figures' place: its agent says the plan has no limits.
    public static let noPlanLimits = "No plan limits"
    /// In a row's figures' place: its agent hasn't said yet.
    public static let notReported = "Not reported yet"
    /// A row's second line while its sign-in waits on the browser; the computer ends it after ten minutes.
    public static let signingIn = "Finish signing in in your browser. This stops by itself after 10 minutes."

    /// A row's second line when another account is signed in as the same address: the row's title already is that
    /// address, so the line names none.
    public static let sameSignIn = "The same sign-in as another account here"

    /// A row's second line when Codex keeps its sign-in in the computer's own store.
    public static let notSeparate = "Uses this computer's own Codex sign-in, so it isn't a separate account"
}

/// What an account's row on Settings › Accounts offers in its "…": Sign In Again… where the account is signed out, and
/// Remove… for every account added here. The Default is the agent as set up on the computer and offers neither.
public enum AccountRowAct: Sendable, Hashable {
    case signInAgain, remove
    /// An agent's own sign-in (Antigravity's), signed out or in.
    case signIn, signOut

    public var title: String {
        switch self {
        case .signInAgain: "Sign In Again…"
        case .remove: "Remove…"
        case .signIn: "Sign In…"
        case .signOut: "Sign Out…"
        }
    }

    /// `account`'s acts, in the menu's order; none for the Default (nil).
    public static func acts(for account: AgentAccount?) -> [AccountRowAct] {
        guard let account else { return [] }
        return account.signedOut ? [.signInAgain, .remove] : [.remove]
    }
}

/// A switch in progress for one agent's sessions on a computer: the account the due and moving ones go to (its email
/// where its agent says it, else its name), how many are due to move with their next message, how many are moving,
/// and the ones that couldn't switch, by their folder's name, whatever account they tried.
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
    public var failure: String? { AccountSwitchWords.failure(sessions: failed) }
}
