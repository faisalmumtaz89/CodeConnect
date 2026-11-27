import Foundation

/// The Mac's list's counts: a computer's sessions in the head's menu. The list itself is `ProjectList`.
public enum RunList {
    /// How many sessions `computer` has, or every computer when nil, that have not ended (one stopped at its usage
    /// limit has not): its count in the head's menu.
    public static func activeCount(_ rows: [FleetRow], on computer: String?) -> Int {
        rows.count { (computer == nil || $0.machineID == computer) && ($0.status != .ended || $0.summary.atLimit) }
    }
}

/// The run list's own words.
public enum RunListWords {
    /// A count of runs, the word following the count.
    public static func runs(_ count: Int) -> String { count == 1 ? "1 session" : "\(count) sessions" }

    /// What needs the person, on every computer: the list's first row, and a project header's count.
    public static func needYou(_ count: Int) -> String { count == 1 ? "1 needs you" : "\(count) need you" }

    /// The first row's way to only what needs the person, and back.
    public static let onlyThese = "Only these"
    public static let showAll = "Show All"

    /// Under Only these, the projects it leaves out: one by its name, several by their count.
    public static func quiet(_ names: [String]) -> String {
        (names.count == 1 ? "\(names[0]) is hidden" : "\(names.count) projects are hidden") + ": nothing there needs you."
    }

    /// A project's ended sessions not yet shown, and the way to fold the shown ones back.
    public static func earlier(_ count: Int) -> String { "\(count) earlier ›" }
    public static let showFewer = "Show fewer"

    /// A project's header menu, and the head menu's way back to a hidden project.
    public static let newSessionHere = "New Session Here"
    public static func pin(_ pinned: Bool) -> String { pinned ? "Unpin" : "Pin to Top" }
    public static let collapseOthers = "Collapse Other Projects"
    public static func hide(_ hidden: Bool) -> String { hidden ? "Unhide Project" : "Hide Project" }
    public static let hiddenProjects = "Hidden Projects"

    /// Search's field and its count.
    public static let search = "Search"
    public static func found(_ count: Int) -> String { "\(count) found" }

    /// The list with nothing in it while it is narrowed to one computer.
    public static func noneOn(_ computer: String) -> String { "None on \(computer)" }

    /// The first row's note while the list is narrowed to one computer: it still counts every computer's sessions.
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
