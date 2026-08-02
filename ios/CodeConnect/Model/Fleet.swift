import Foundation

/// The fixed fleet ordering. Not user-sortable on purpose: at 2am the list must
/// be in the same order it was last time.
enum FleetStatus: Int, Sendable, Comparable, CaseIterable {
    case blocked = 0
    case failed = 1
    case doneUnreviewed = 2
    case running = 3
    case idle = 4
    case ended = 5

    static func < (lhs: FleetStatus, rhs: FleetStatus) -> Bool { lhs.rawValue < rhs.rawValue }

    var label: String {
        switch self {
        case .blocked: return "Blocked"
        case .failed: return "Failed"
        case .doneUnreviewed: return "Done"
        case .running: return "Running"
        case .idle: return "Idle"
        case .ended: return "Ended"
        }
    }

    var symbol: String {
        switch self {
        case .blocked: return "hand.raised.fill"
        case .failed: return "exclamationmark.octagon.fill"
        case .doneUnreviewed: return "checkmark.seal.fill"
        case .running: return "circle.dotted"
        case .idle: return "moon.zzz"
        case .ended: return "power"
        }
    }
}

/// Whether an answer typed here can actually reach the agent — reported, never
/// assumed. The daemon is Claude-only today, so the distinction that matters is
/// whether a supervisor is attached to type into.
enum CapabilityBadge: Sendable, Hashable {
    case control
    case observe(reason: String)

    var label: String {
        switch self {
        case .control: return "control"
        case .observe: return "observe"
        }
    }

    var symbol: String {
        switch self {
        case .control: return "circle.fill"
        case .observe: return "circle.lefthalf.filled"
        }
    }

    var reason: String? {
        if case .observe(let reason) = self { return reason }
        return nil
    }

    var canAct: Bool {
        if case .control = self { return true }
        return false
    }
}

/// **What an agent is doing, split at the seam between prose and code.**
///
/// `Bash` is a name and `echo soak` is a command, and the rule that
/// identifiers, commands, diffs and durations are monospace **always** makes
/// that a type distinction rather than a stylistic one.
/// Blocked rows have carried the split since the fleet was rebuilt. Running and
/// Idle rows carried a single flat `String`, so on one screen a `git push
/// --force origin main` two rows up was monospace and `echo soak` was not.
///
/// It is measurable and it was measured, by ink metrics rather than by eye: on a
/// blocked row the tool and its argument have different ink-tops and different
/// ink-heights (304.00 / 9.00 against 305.00 / 9.67) because they are two faces;
/// on a running row they measured identically (301.67 / 11.00 for both), which
/// is one. The advance says the same thing — the blocked rows' arguments run at
/// a constant 7.42pt per character and `echo soak`'s nine characters took
/// 64.00pt where mono predicts 53.
///
/// Nil where the row's last item is not a tool call: a user message, an agent
/// sentence and a notice are prose all the way through, and forcing a split on
/// them would be the same error pointing the other way.
struct FleetActivity: Sendable, Hashable {
    /// The tool's name. Prose.
    let tool: String
    /// The argument that identifies the call. Code, and set as code.
    let argument: String?
}

struct FleetRow: Sendable, Identifiable, Hashable {
    var summary: SessionSummary
    var status: FleetStatus
    var title: String
    /// The daemon's own sentence, for a row whose last item is not a tool call —
    /// and what a screen reader hears either way.
    var subtitle: String
    /// The tool call this row is on, when it is on one.
    var activity: FleetActivity?
    /// The tmux name, plus enough of the uid to tell it from a namesake when the
    /// fleet holds more than one run under it.
    var identity: String
    var capability: CapabilityBadge
    var blockedCount: Int
    var lastEventAt: Date?
    /// Non-nil when nothing live has arrived for this session this launch.
    var cachedAt: Date?

    /// The run, not the name: two rows can share `cc-1` and a `ForEach` that
    /// cannot tell them apart renders one of them.
    var id: String { summary.sessionKey }
}

// MARK: - The one number, and the one sentence that carries it

/// **How many decisions the fleet is holding, and how the product says it.**
///
/// The fleet's display line and the accessory bar 631pt below it were computing
/// this independently and counting different things: the headline counted
/// *sessions* in the Blocked band, the bar counted *cards* in the Deck. Two
/// agents holding three approvals rendered `2 need you` in the largest type on
/// the screen and `3 need you` in the second largest, word for word identical,
/// on one viewport. Invisible in a fixture where every agent holds exactly one
/// card, which is why it shipped.
///
/// Both numbers were true. Neither string said which question it was answering,
/// so the reader had no way to reconcile them and no reason to trust either.
/// The noun is now stated — a **decision** is the thing you have to answer, and
/// it is what the Deck behind the bar is a queue of — and the arithmetic lives
/// here, once, so the two call sites cannot drift apart again.
enum FleetCount {
    /// Every decision the fleet is waiting on a human for.
    ///
    /// Counted from the rows rather than from the Deck on purpose. `blocked_on`
    /// is the daemon's authoritative statement that a request is open, and it
    /// arrives before the card that carries the request's *contents* reaches the
    /// event stream — so a Deck-derived count reads zero for the beat between a
    /// row turning amber and its card landing, and the headline would say
    /// "Nothing needs you" over a visibly blocked band.
    static func decisions(in rows: [FleetRow]) -> Int {
        rows.filter { $0.status == .blocked }
            // A blocked row is holding at least one, by the definition of
            // blocked; `max` says so rather than trusting the two sources to
            // agree on a row whose card has not arrived.
            .reduce(0) { $0 + max(1, $1.blockedCount) }
    }

    /// The sentence, in the canonical words. One string, every surface: the
    /// fleet's display line, the accessory bar's aggregate, and what a screen
    /// reader hears from either.
    static func needsYou(_ count: Int) -> String {
        count == 1 ? "1 decision needs you" : "\(count) decisions need you"
    }
}

// MARK: - How old the fleet on screen is

/// **A fleet read off the disk has to say so, whatever else is wrong.**
///
/// The one-banner ladder is `rejected > offline > stale > cached`, and it is
/// right: those are ranked by how much of the product is unavailable. But a
/// cold launch with no daemon is *two* facts, not one — the link is down **and**
/// every number on the screen came out of a file — and the ladder dropped the
/// second one. Measured, a cached fleet rendered `OFFLINE / Could not connect to
/// the server. — trying ws:// next. — retrying in 7s.` with **no cache age
/// anywhere on the screen**, not in the banner, not in the pill, not on a row,
/// while two wait clocks read `2m10s` and `5m40s` in `warning` and kept
/// incrementing. A reader at 2am sees an amber `5m40s` and cannot tell it from a
/// live one.
///
/// The two facts are merged into one candidate rather than stacked as two
/// banners: "one banner, ever" is the rule the slot exists to enforce and a
/// compound state is still one state. Kept here, out of the view, so the
/// property that matters — *if the fleet is cached, its age is in the banner, no
/// matter which level won* — is a function that can be asserted.
enum FleetFreshness {
    /// How old the fleet on screen is, or `nil` when it came off the wire.
    static func stamp(cachedAt: Date?, hasLiveFleet: Bool, now: Date) -> String? {
        guard let cachedAt, !hasLiveFleet else { return nil }
        return "Showing the last known state, \(Format.age(since: cachedAt, now: now)) old."
    }

    /// The compound message: the age first, because it is what decides how much
    /// of the screen to believe, then whatever the link had to say for itself.
    ///
    /// The clocks are deliberately left alone. The age of the *card* is still
    /// true; it was the age of the *observation* that was missing.
    static func message(stamp: String?, linkDetail: String?) -> String? {
        let parts = [stamp, linkDetail].compactMap { $0 }.filter { !$0.isEmpty }
        return parts.isEmpty ? nil : parts.joined(separator: " ")
    }
}

enum FleetOrdering {
    /// Within a status band, the most recently active session comes first: the
    /// thing that just changed is the thing you are looking for.
    static func sort(_ rows: [FleetRow]) -> [FleetRow] {
        rows.sorted { lhs, rhs in
            if lhs.status != rhs.status { return lhs.status < rhs.status }
            let left = lhs.lastEventAt ?? lhs.summary.updatedDate
            let right = rhs.lastEventAt ?? rhs.summary.updatedDate
            if left != right { return left > right }
            // The key, not the name: two runs called `cc-1` need a total order
            // or they can swap places between renders.
            return lhs.summary.sessionKey < rhs.summary.sessionKey
        }
    }
}

// MARK: - Status derivation

@MainActor
enum FleetStatusRule {
    /// `blockedOn` from the daemon is authoritative for "needs you"; the local
    /// timeline is consulted too so a card that arrived on the stream counts
    /// even before the next `sessions` refresh.
    static func status(
        summary: SessionSummary, state: SessionState?, reviewedSeq: UInt64
    ) -> FleetStatus {
        let pending = state?.pendingApprovals.count ?? 0
        if !summary.blockedOn.isEmpty || pending > 0 { return .blocked }
        if summary.lifecycle == .exited {
            return lastTurnFailed(state) ? .failed : .ended
        }
        if lastTurnFailed(state) { return .failed }
        guard let state, let last = lastSubstantiveItem(state) else {
            return summary.lifecycle == .live ? .idle : .ended
        }
        if isTurnBoundary(last) {
            return state.lastSeq > reviewedSeq ? .doneUnreviewed : .idle
        }
        return .running
    }

    /// Link and lifecycle chatter says nothing about whether the agent is
    /// working, so a `link_state` arriving after a finished turn must not make
    /// the session look busy again.
    private static func lastSubstantiveItem(_ state: SessionState) -> TimelineItem? {
        state.timeline.last { item in
            guard case .notice(let notice) = item.content else { return true }
            switch notice.kind {
            case .link, .sessionStart, .other: return false
            case .turnComplete, .sessionEnded, .agentWaiting, .agentFinished, .failure: return true
            }
        }
    }

    private static func isTurnBoundary(_ item: TimelineItem) -> Bool {
        guard case .notice(let notice) = item.content else { return false }
        switch notice.kind {
        case .turnComplete, .sessionEnded, .agentFinished: return true
        default: return false
        }
    }

    /// "Failed" means the newest turn ended badly — scanning only that turn
    /// keeps a failure from an hour ago out of the top of the fleet forever.
    private static func lastTurnFailed(_ state: SessionState?) -> Bool {
        guard let state else { return false }
        var sawFailure = false
        for item in state.timeline.reversed() {
            switch item.content {
            case .userMessage:
                return sawFailure
            case .tool(let tool):
                if tool.status == .failed || tool.status == .interrupted { sawFailure = true }
            case .notice(let notice):
                if notice.severity == .failure { sawFailure = true }
            default:
                break
            }
        }
        return sawFailure
    }

    static func capability(summary: SessionSummary, capabilities: Capabilities?) -> CapabilityBadge {
        guard let capabilities else {
            return .observe(reason: "Not connected. The daemon has not told us what it can do.")
        }
        guard capabilities.canApproveReliably else {
            return .observe(reason: "The daemon does not guarantee an answer will reach the agent.")
        }
        switch summary.link {
        case .attached:
            return .control
        case .degraded:
            return .observe(reason: "The link to this session is degraded; answers may not land.")
        case .detached:
            return .observe(
                reason: "No supervisor is attached to this session. It can only be answered at the Mac."
            )
        case .stale:
            return .observe(reason: "The daemon has not heard from this session recently.")
        }
    }
}

// MARK: - Freshness

/// Link health, expressed so that no number is ever shown without its age and
/// no button is ever enabled on a link that cannot carry it.
struct LinkHealth: Sendable, Equatable {
    enum Level: Sendable, Equatable {
        case live
        case lagging
        case stale
        case connecting
        case offline
        case rejected
    }

    var level: Level
    var age: TimeInterval?
    var detail: String

    /// Beyond this, a "connected" socket is not evidence of anything.
    private static let lagAfter: TimeInterval = 15
    private static let staleAfter: TimeInterval = 45

    var actionsEnabled: Bool { level == .live || level == .lagging }

    var disabledReason: String? {
        guard !actionsEnabled else { return nil }
        switch level {
        case .stale: return "Link stale - \(ageText) since the daemon last spoke. \(detail)"
        case .connecting: return "Connecting to the daemon…"
        case .offline: return detail
        case .rejected: return detail
        case .live, .lagging: return nil
        }
    }

    var ageText: String { age.map(Format.age) ?? "-" }

    var symbol: String {
        switch level {
        case .live: return "circle.fill"
        case .lagging: return "circle.lefthalf.filled"
        case .stale: return "exclamationmark.triangle.fill"
        case .connecting: return "arrow.triangle.2.circlepath"
        case .offline: return "bolt.horizontal.circle"
        case .rejected: return "lock.slash"
        }
    }

    var shortText: String {
        switch level {
        case .live, .lagging, .stale: return ageText
        case .connecting: return "connecting"
        case .offline: return "offline"
        case .rejected: return "token"
        }
    }

    static func evaluate(
        phase: DaemonConnection.Phase, lastContactAt: Date?, now: Date = Date()
    ) -> LinkHealth {
        switch phase {
        case .idle:
            return LinkHealth(level: .offline, age: nil, detail: "Not paired with a daemon.")
        case .connecting:
            return LinkHealth(level: .connecting, age: nil, detail: "Opening the connection.")
        case .failed(let reason):
            return LinkHealth(level: .rejected, age: nil, detail: reason)
        case .waiting(let until, let reason):
            let seconds = max(0, until.timeIntervalSince(now))
            // Early backoffs are sub-second; "retrying in 0s" reads like a stuck
            // counter rather than the truth, which is "any moment now".
            let when = seconds < 1 ? "retrying now" : "retrying in \(Int(seconds.rounded()))s"
            return LinkHealth(
                level: .offline,
                age: lastContactAt.map { now.timeIntervalSince($0) },
                detail: "\(reason), \(when).")
        case .connected:
            guard let lastContactAt else {
                return LinkHealth(
                    level: .connecting, age: nil, detail: "Connected; waiting for the first frame.")
            }
            let age = now.timeIntervalSince(lastContactAt)
            if age >= staleAfter {
                return LinkHealth(
                    level: .stale, age: age,
                    detail: "The socket is open but the daemon has gone quiet.")
            }
            if age >= lagAfter {
                return LinkHealth(level: .lagging, age: age, detail: "Link lagging.")
            }
            return LinkHealth(level: .live, age: age, detail: "Live.")
        }
    }
}

// MARK: - Formatting

enum Format {
    /// Compact ages, because every fact on screen carries one.
    ///
    /// A sub-second age is `0s`, never `0.0s`. The decimal was false precision
    /// on the one surface whose whole job is honest freshness — nothing in this
    /// app measures tenths, and a reader who sees `0.4s` is entitled to believe
    /// something did. `CCWaitClock` has always truncated to whole seconds; this
    /// is the same rule, applied at the source so every consumer inherits it.
    static func age(_ interval: TimeInterval) -> String {
        let seconds = max(0, interval)
        if seconds < 1 { return "0s" }
        if seconds < 60 { return "\(Int(seconds))s" }
        if seconds < 3600 { return "\(Int(seconds / 60))m" }
        if seconds < 86400 { return "\(Int(seconds / 3600))h" }
        return "\(Int(seconds / 86400))d"
    }

    static func age(since date: Date?, now: Date = Date()) -> String {
        guard let date else { return "never" }
        return age(now.timeIntervalSince(date))
    }

    static func duration(ms: Int) -> String {
        if ms < 1000 { return "\(ms)ms" }
        if ms < 60000 { return String(format: "%.1fs", Double(ms) / 1000) }
        return "\(ms / 60000)m \((ms % 60000) / 1000)s"
    }

    /// VoiceOver reads "14s" as "fourteen ess"; spell it out for the label.
    static func spokenAge(_ interval: TimeInterval) -> String {
        let seconds = Int(max(0, interval))
        if seconds < 60 { return "\(seconds) second\(seconds == 1 ? "" : "s") ago" }
        if seconds < 3600 {
            let minutes = seconds / 60
            return "\(minutes) minute\(minutes == 1 ? "" : "s") ago"
        }
        let hours = seconds / 3600
        return "\(hours) hour\(hours == 1 ? "" : "s") ago"
    }
}
