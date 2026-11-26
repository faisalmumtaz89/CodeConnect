import Foundation

/// **What a session's reply box shows beside its send button**: how much of its model's window the conversation fills,
/// each usage window of the account its process runs on, and that account's name. Only what the app already holds: the
/// run's own `usage` events and the computer's account book. A part with nothing held is absent, never a placeholder.
public struct ReplyBar: Equatable, Sendable {
    /// The conversation's size against its model's window, as drawn: "390k" of "1M".
    public struct Context: Equatable, Sendable {
        public let used: String
        public let window: String
    }

    /// One usage window of an account: its share used, its caption ("5-hour", "Week") and how long until it resets
    /// (`AccountSwitchWords.resets`; nil once the time has come).
    public struct Limit: Equatable, Sendable {
        public let usedPercent: Int
        public let caption: String
        public let resets: String?
    }

    /// From the run's newest `usage` event that names both its context and its model's window; nil before one has (a
    /// command's turn, /compact or /clear names neither).
    public let context: Context?
    /// The account's windows, shortest first; empty when the account or its facts are not known, when its agent names
    /// none, or when no plan limits apply.
    public let limits: [Limit]
    /// In the limits' place, the Accounts page's words when the account's agent says no plan limits apply.
    public let limitsNote: String?
    /// The account by the Kit's name rule: its email as the account book knows it, else its label; nil when the run
    /// names no account.
    public let account: String?

    /// The bar's whole reading as one sentence, as both apps give it to VoiceOver: the context, each window with its
    /// reset, the no-limits words, then the account; an absent part left out.
    public var spoken: String {
        var parts = context.map { ["Context \($0.used) of \($0.window)"] } ?? []
        parts += limits.map { "\($0.caption) \($0.usedPercent)%" + ($0.resets.map { ", \($0)" } ?? "") }
        parts += [limitsNote, account.map { "Account \($0)" }].compactMap { $0 }
        return parts.joined(separator: "; ")
    }

    public static func of(events: [Event], summary: SessionSummary, book: AccountBook?, now: Date) -> ReplyBar {
        let context = events.reversed().lazy.compactMap { event -> Context? in
            guard event.kind == .usage, let used = event.payload["last"]?["totalTokens"]?.intValue,
                let window = event.payload["modelContextWindow"]?.intValue
            else { return nil }
            return Context(used: tokens(used), window: tokens(window))
        }.first
        guard let account = summary.account, let agent = StartAgent(rawValue: summary.agent.rawValue) else {
            return ReplyBar(context: context, limits: [], limitsNote: nil, account: nil)
        }
        let facts = book?.facts(for: agent, accountID: account.id)
        return ReplyBar(
            context: context, limits: barLimits(facts, now: now),
            limitsNote: facts?.limitsAvailable == false ? AccountSwitchWords.noPlanLimits : nil,
            account: facts?.email ?? account.label)
    }

    /// The windows this bar shows: all of them; by model group (Antigravity's), the one nearest its limit, standing for
    /// them all, as the bar has room for few.
    public static func barLimits(_ facts: AccountFacts?, now: Date) -> [Limit] {
        let all = limits(facts, now: now)
        guard facts?.windows.contains(where: { $0.group != nil }) == true else { return all }
        return all.max { $0.usedPercent < $1.usedPercent }.map { [$0] } ?? []
    }

    /// An account's windows as drawn, shortest first (by model group, in the agent's order, where it names groups),
    /// wherever its usage is shown (this bar, the Accounts page).
    public static func limits(_ facts: AccountFacts?, now: Date) -> [Limit] {
        guard let facts, facts.limitsAvailable != false else { return [] }
        let groups = facts.windows.compactMap(\.group).reduce(into: [String]()) { if !$0.contains($1) { $0.append($1) } }
        let rank = { (window: UsageWindow) in groups.firstIndex(of: window.group ?? "") ?? 0 }
        return facts.windows.sorted { (rank($0), $0.windowMinutes) < (rank($1), $1.windowMinutes) }.map {
            Limit(
                usedPercent: $0.usedPercent, caption: AccountSwitchWords.caption($0),
                resets: AccountSwitchWords.resets($0, now: now))
        }
    }

    /// A count of tokens as the bar writes it: the number under a thousand; whole thousands to the nearest ("390k");
    /// from what rounds to a thousand thousands, millions to the nearest tenth, ".0" dropped ("1M", "1.2M").
    static func tokens(_ count: Int) -> String {
        if count < 1000 { return "\(count)" }
        let thousands = (count + 500) / 1000
        if thousands < 1000 { return "\(thousands)k" }
        let tenths = (count + 50_000) / 100_000
        return tenths % 10 == 0 ? "\(tenths / 10)M" : "\(tenths / 10).\(tenths % 10)M"
    }
}
