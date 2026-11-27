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
    /// The window the box's figures draw a ring for: at a usage limit (`SessionSummary.atLimit`), the window of that
    /// limit's length; else the shortest the account reports (its 5-hour, else the next). Nil with no windows.
    public let ring: Limit?
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
            return ReplyBar(context: context, limits: [], limitsNote: nil, ring: nil, account: nil)
        }
        let facts = book?.facts(for: agent, accountID: account.id)
        let limits = limits(facts, now: now)
        let full = summary.atLimit ? summary.limit?.which.windowMinutes.map(AccountSwitchWords.caption(minutes:)) : nil
        return ReplyBar(
            context: context, limits: limits,
            limitsNote: facts?.limitsAvailable == false ? AccountSwitchWords.noPlanLimits : nil,
            ring: limits.first { $0.caption == full } ?? limits.first,
            account: facts?.email ?? account.label)
    }

    /// An account's windows as drawn, shortest first, wherever its usage is shown (this bar, the Accounts page).
    public static func limits(_ facts: AccountFacts?, now: Date) -> [Limit] {
        guard let facts, facts.limitsAvailable != false else { return [] }
        return facts.windows.sorted { $0.windowMinutes < $1.windowMinutes }.map {
            Limit(
                usedPercent: $0.usedPercent, caption: AccountSwitchWords.caption(minutes: $0.windowMinutes),
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

/// A run stopped at a usage limit (`SessionSummary.limit`), as the band at its conversation's foot says it: which limit,
/// when it resets (read from the run's account's window of that length, which the app holds: the agent's own reset has
/// no declared unit), the account's windows with the full one marked, and the account with its plan. The reply box stays
/// live under it. Nil for a run at no limit, and for one that takes no reply.
extension SessionSummary {
    /// **The run is at a usage limit**: its last turn stopped at one (`limit`), and it still takes a reply, which goes
    /// through once the limit resets. The band over its box shows, and the run list has it in Needs you.
    public var atLimit: Bool { limit != nil && !takesNoReply }
}

public struct LimitFoot: Equatable, Sendable {
    /// "5-hour limit reached".
    public let title: String
    /// "Resets 9:06 PM · in 2h 10m" on the same day, or "Resets Thu 3 Dec, 9:06 PM" on another; nil when the limit has
    /// no window of its own, the window is not held, or its reset has passed.
    public let resets: String?
    /// When that is, for a list's time slot; nil when `resets` is.
    public let resetsAt: Date?
    /// The account's windows, as the reply bar draws them.
    public let limits: [ReplyBar.Limit]
    /// The caption of the window this limit is, drawn red; nil when it is none of them.
    public let full: String?
    /// "<email> · <Plan>", or the account's name alone; nil when the run names no account.
    public let account: String?

    /// The reply box's prompt: `replyPrompt`, or nil when the next message moves the run to another account, since
    /// that message goes through.
    public let prompt: String?

    /// The reply box's prompt while the run is at its limit.
    public static let replyPrompt = "Send after the reset to carry on"

    public static func of(summary: SessionSummary, book: AccountBook?, now: Date, timeZone: TimeZone = .current)
        -> LimitFoot?
    {
        guard summary.atLimit, let limit = summary.limit else { return nil }
        let facts = summary.account.flatMap { account in
            StartAgent(rawValue: summary.agent.rawValue).flatMap { book?.facts(for: $0, accountID: account.id) }
        }
        // The window of that length the run stopped in: one the account began after the limit is a later window.
        let at = ISO8601.parse(limit.at)?.timeIntervalSince1970 ?? now.timeIntervalSince1970
        let window = facts?.windows.first { window in
            window.windowMinutes == limit.which.windowMinutes
                && TimeInterval(window.resetsAt - window.windowMinutes * 60) <= at
        }
        let reset = window.flatMap { window in
            AccountSwitchWords.countdown(window, now: now).map {
                (date: Date(timeIntervalSince1970: TimeInterval(window.resetsAt)), countdown: $0)
            }
        }
        let resets = reset.map { reset in
            sameDay(reset.date, now, timeZone)
                ? "Resets \(clock(reset.date, sameDay: true, timeZone)) · \(reset.countdown)"
                : "Resets \(clock(reset.date, sameDay: false, timeZone))"
        }
        let plan = facts?.plan.map { $0.prefix(1).uppercased() + $0.dropFirst() }
        let name = facts?.email ?? summary.account?.label
        return LimitFoot(
            title: "\(words(limit.which)) limit reached", resets: resets, resetsAt: reset?.date,
            limits: ReplyBar.limits(facts, now: now),
            full: window.map { AccountSwitchWords.caption(minutes: $0.windowMinutes) },
            account: name.map { name in plan.map { "\(name) · \($0)" } ?? name },
            prompt: summary.switching == nil ? replyPrompt : nil)
    }

    private static func words(_ which: LimitKind) -> String {
        switch which {
        case .fiveHour: "5-hour"
        case .weekly: "Weekly"
        case .weeklyOpus: "Weekly Opus"
        case .weeklySonnet: "Weekly Sonnet"
        case .extraUsage: "Extra usage"
        case .credits: "Credit"
        case .workspace: "Workspace"
        case .unknown: "Usage"
        }
    }

    private static func sameDay(_ date: Date, _ now: Date, _ timeZone: TimeZone) -> Bool {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = timeZone
        return calendar.isDate(date, inSameDayAs: now)
    }

    /// A reset as the band names it: its time on the same day, else its day, date and time.
    private static func clock(_ date: Date, sameDay: Bool, _ timeZone: TimeZone) -> String {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = timeZone
        formatter.dateFormat = sameDay ? "h:mm a" : "EEE d MMM, h:mm a"
        return formatter.string(from: date)
    }
}

