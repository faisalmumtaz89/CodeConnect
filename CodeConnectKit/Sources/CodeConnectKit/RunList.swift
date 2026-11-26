import Foundation

/// The Mac's run list (W1a): every run in one of three sections by what it needs, each in the fleet's own order. Needs
/// you holds each run that waits on the person (a card or a question, a failure, an idle run with no first message) on
/// every computer, so a computer chosen in the list's head can never hide one. Working and Done hold the runs going and
/// the runs finished, on the chosen computer, or on all of them. The ended runs are kept apart, on all computers,
/// behind Done's "N ended ›".
public struct RunList: Sendable, Equatable {
    public let needsYou: [FleetRow]
    public let working: [FleetRow]
    public let done: [FleetRow]
    public let ended: [FleetRow]

    /// `rows`, in the fleet's order, narrowed to `computer` (a machine id) where one is chosen.
    public init(_ rows: [FleetRow], computer: String?) {
        let shown = { (row: FleetRow) in computer == nil || row.machineID == computer }
        needsYou = rows.filter { [.blocked, .failed, .idle].contains($0.status) }
        working = rows.filter { shown($0) && [.running, .stopping].contains($0.status) }
        done = rows.filter { shown($0) && [.doneUnreviewed, .done].contains($0.status) }
        ended = rows.filter { $0.status == .ended }
    }

    /// How many runs `computer` has that have not ended: the ones the list shows of it, its count in the head's menu.
    public static func activeCount(_ rows: [FleetRow], on computer: String) -> Int {
        rows.count { $0.machineID == computer && $0.status != .ended }
    }
}

/// The run list's own words.
public enum RunListWords {
    /// A count of runs, the word following the count.
    public static func runs(_ count: Int) -> String { count == 1 ? "1 run" : "\(count) runs" }

    /// Done's way to the ended runs.
    public static func ended(_ count: Int) -> String { "\(count) ended ›" }

    /// A section with nothing in it while the list is narrowed to one computer.
    public static func noneOn(_ computer: String) -> String { "None on \(computer)" }

    /// Needs you's note while the list is narrowed to one computer: it still holds every computer's runs.
    public static let allComputers = "all computers"
}
