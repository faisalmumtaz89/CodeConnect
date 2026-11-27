import Foundation

/// The Mac's run list (W1a): every run in one of three sections by what it needs, each in the fleet's own order. Needs
/// you holds each run that waits on the person (a card or a question, a failure, an idle run with no first message, a
/// run at its usage limit) on every computer, so a computer chosen in the list's head can never hide one. Working and
/// Done hold the runs going and the runs finished, on the chosen computer, or on all of them. The ended runs are kept
/// apart behind Done's "N ended ›", narrowed as Working and Done are.
public struct RunList: Sendable, Equatable {
    public let needsYou: [FleetRow]
    public let working: [FleetRow]
    public let done: [FleetRow]
    public let ended: [FleetRow]

    /// `rows`, in the fleet's order, narrowed to `computer` (a machine id) where one is chosen.
    public init(_ rows: [FleetRow], computer: String?) {
        let shown = { (row: FleetRow) in computer == nil || row.machineID == computer }
        needsYou = rows.filter { [.blocked, .failed, .idle].contains($0.status) || $0.summary.atLimit }
        working = rows.filter { shown($0) && !$0.summary.atLimit && [.running, .stopping].contains($0.status) }
        done = rows.filter { shown($0) && !$0.summary.atLimit && [.doneUnreviewed, .done].contains($0.status) }
        ended = rows.filter { shown($0) && $0.status == .ended && !$0.summary.atLimit }
    }

    /// How many runs `computer` has, or every computer when nil, that have not ended: the ones the list shows of it,
    /// its count in the head's menu.
    public static func activeCount(_ rows: [FleetRow], on computer: String?) -> Int {
        rows.count { (computer == nil || $0.machineID == computer) && ($0.status != .ended || $0.summary.atLimit) }
    }
}

/// The run list's own words.
public enum RunListWords {
    /// A count of runs, the word following the count.
    public static func runs(_ count: Int) -> String { count == 1 ? "1 session" : "\(count) sessions" }

    /// Done's way to the ended runs.
    public static func ended(_ count: Int) -> String { "\(count) ended ›" }

    /// A section with nothing in it while the list is narrowed to one computer.
    public static func noneOn(_ computer: String) -> String { "None on \(computer)" }

    /// Needs you's note while the list is narrowed to one computer: it still holds every computer's runs.
    public static let allComputers = "all computers"

    /// The head menu's first row, every computer's runs.
    public static let allTitle = "All computers"

    /// A computer not reached, in words (D1).
    public static let offline = "· Offline"

    /// An offline computer's line: when it was last reached (D4), else how many runs wait for it, else nothing.
    public static func offlineLine(lastSeen: Date?, waiting: Int, now: Date, calendar: Calendar = .current) -> String? {
        if let lastSeen { return "Last seen \(Format.clock(lastSeen, now: now, calendar: calendar))" }
        return waiting > 0 ? runs(waiting) : nil
    }
}
