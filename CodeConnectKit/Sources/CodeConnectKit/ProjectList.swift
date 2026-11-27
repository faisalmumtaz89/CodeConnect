import Foundation

/// The Mac's session list by project (Release 2): every session under its project, the projects in the order the
/// person should look at them, and the one count of what needs them. The iPhone keeps `FleetGroup`.
///
/// A project is one folder on one computer: the session's folder, or for a session in a worktree of its own
/// (`<folder>/.claude/worktrees/<name>`) the folder the worktree belongs to. Its name is that folder's.
public struct ProjectList: Sendable, Equatable {
    /// Where the open session stood when it was opened: it stays there, and its project too, while it is open.
    public struct Held: Sendable, Equatable {
        public let session: String
        public let project: String
        public let projectIndex: Int
        public let rowIndex: Int

        public init(session: String, project: String, projectIndex: Int, rowIndex: Int) {
            self.session = session
            self.project = project
            self.projectIndex = projectIndex
            self.rowIndex = rowIndex
        }
    }

    public struct Project: Sendable, Equatable, Identifiable {
        /// Its key: what Pin, Hide and a reveal are kept by.
        public let id: String
        /// Its folder's name, with its computer, or its parent folder on one computer, when another project has it.
        public let name: String
        /// How many of its sessions need the person.
        public let needsYou: Int
        /// How many sessions it has in the list (archived ones are not).
        public let sessions: Int
        /// The sessions it shows: what needs you, what works, what is done, then the ended ones revealed.
        public let rows: [FleetRow]
        /// Its ended sessions not yet shown: "N earlier ›".
        public let earlier: Int
        public let pinned: Bool
        /// Hidden, and shown only because something in it needs the person.
        public let hidden: Bool
        /// The computer and the folder it is on: where a new session in it starts.
        public let machineID: String?
        public let folder: String
    }

    /// What needs the person, on every computer, whatever else the list does: the list's first answer.
    public let needsYou: Int
    /// "Only these" in effect: asked for, and something needs the person.
    public let onlyNeedsYou: Bool
    public let projects: [Project]
    /// The projects "Only these" leaves out, by name.
    public let quiet: [String]

    /// How many ended sessions a reveal adds.
    public static let page = 25

    /// One rule for what needs the person, the same as the list's sections (`RunList`): a card or a question, a
    /// failure, an idle session with no first message, a session at its usage limit.
    public static func needsYou(_ row: FleetRow) -> Bool {
        [.blocked, .failed, .idle].contains(row.status) || row.summary.atLimit
    }

    /// `rows` narrowed to `computer` (a machine id) where one is chosen, except what needs the person; with
    /// `onlyNeedsYou`, only that. `pinned` (keys, first first) lead; a `hidden` key stays out unless it needs the
    /// person; `revealed` is each project's ended sessions shown; `waitingSince` each waiting session's oldest card;
    /// `held` the open session's place.
    public init(
        _ rows: [FleetRow], computer: String? = nil, onlyNeedsYou: Bool = false, pinned: [String] = [],
        hidden: Set<String> = [], revealed: [String: Int] = [:], waitingSince: [String: Date] = [:],
        held: Held? = nil
    ) {
        let listed = rows.filter { !$0.summary.archived }
        needsYou = listed.count(where: Self.needsYou)
        self.onlyNeedsYou = onlyNeedsYou && needsYou > 0
        let only = self.onlyNeedsYou
        let shown = listed.filter { Self.needsYou($0) || computer == nil || $0.machineID == computer }

        var order: [String] = []
        var groups: [String: [FleetRow]] = [:]
        for row in shown {
            let key = Self.key(row)
            if groups[key] == nil { order.append(key) }
            groups[key, default: []].append(row)
        }
        let names = Self.names(order.map { key in (key, groups[key]!.first!) })

        var projects: [Project] = []
        var quiet: [String] = []
        for key in order {
            let all = groups[key]!
            let need = all.count(where: Self.needsYou)
            let isHidden = hidden.contains(key)
            let holds = all.contains { $0.id == held?.session }
            if isHidden && need == 0 && !holds { continue }
            let kept = only ? all.filter { Self.needsYou($0) || $0.id == held?.session } : all
            if kept.isEmpty {
                quiet.append(names[key]!)
                continue
            }
            let sorted = kept.sorted(by: Self.before)
            let live = sorted.filter { Self.rank($0) < Self.endedRank }
            let ended = sorted.filter { Self.rank($0) == Self.endedRank }
            let reveal = min(ended.count, live.isEmpty ? max(revealed[key] ?? 0, Self.page) : revealed[key] ?? 0)
            var shownRows = live + ended.prefix(reveal)
            if let held, held.project == key, let at = shownRows.firstIndex(where: { $0.id == held.session }) {
                let row = shownRows.remove(at: at)
                shownRows.insert(row, at: min(held.rowIndex, shownRows.count))
            }
            let first = all.first!
            projects.append(Project(
                id: key, name: names[key]!, needsYou: need, sessions: all.count, rows: shownRows,
                earlier: ended.count - reveal, pinned: pinned.contains(key), hidden: isHidden,
                machineID: first.machineID, folder: Self.folder(first.summary.cwd)))
        }

        func wait(_ project: Project) -> Date {
            project.rows.filter(Self.needsYou).map { waitingSince[$0.id] ?? $0.lastActive }.min() ?? .distantFuture
        }
        func urgency(_ project: Project) -> Int { project.rows.filter(Self.needsYou).map(Self.rank).min() ?? Int.max }
        func latest(_ project: Project) -> Date { project.rows.map(\.lastActive).max() ?? .distantPast }
        projects.sort { a, b in
            let pa = pinned.firstIndex(of: a.id), pb = pinned.firstIndex(of: b.id)
            if pa != pb { return (pa ?? Int.max) < (pb ?? Int.max) }
            if (a.needsYou > 0) != (b.needsYou > 0) { return a.needsYou > 0 }
            if a.needsYou > 0 {
                if urgency(a) != urgency(b) { return urgency(a) < urgency(b) }
                if wait(a) != wait(b) { return wait(a) < wait(b) }
            }
            if latest(a) != latest(b) { return latest(a) > latest(b) }
            return a.id < b.id
        }
        if let held, let at = projects.firstIndex(where: { $0.id == held.project }) {
            let project = projects.remove(at: at)
            projects.insert(project, at: min(held.projectIndex, projects.count))
        }
        self.projects = projects
        self.quiet = quiet
    }

    // MARK: Order within a project

    /// Waiting, then failed or at its limit, idle, working, done (unseen first), ended.
    static func rank(_ row: FleetRow) -> Int {
        if row.summary.atLimit { return 1 }
        switch row.status {
        case .blocked: return 0
        case .failed: return 1
        case .idle: return 2
        case .running, .stopping: return 3
        case .doneUnreviewed: return 4
        case .done: return 5
        case .ended: return endedRank
        }
    }

    static let endedRank = 6

    /// By rank, newest first within one.
    static func before(_ a: FleetRow, _ b: FleetRow) -> Bool {
        if rank(a) != rank(b) { return rank(a) < rank(b) }
        if a.lastActive != b.lastActive { return a.lastActive > b.lastActive }
        return a.id < b.id
    }

    // MARK: Projects

    /// A session's project: its computer and the folder it belongs to.
    static func key(_ row: FleetRow) -> String { "\(row.machineID ?? "")\u{1F}\(folder(row.summary.cwd))" }

    /// The folder a session belongs to: its own, or for a worktree of its own the folder that holds the worktree.
    static func folder(_ cwd: String) -> String {
        var path = cwd
        while path.count > 1, path.hasSuffix("/") { path.removeLast() }
        if let range = path.range(of: "/.claude/worktrees/") { path = String(path[..<range.lowerBound]) }
        return path
    }

    /// A folder's name as the list shows it: its last component, nothing that breaks or reorders a line.
    static func name(_ folder: String) -> String {
        let last = folder.split(separator: "/").last.map(String.init) ?? ""
        let shown = last.displaySafe.filter { !$0.isNewline && !($0.unicodeScalars.first?.properties.generalCategory == .control) }
        let trimmed = shown.trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty || trimmed == "." || trimmed == ".." ? RunLabel.unknown.project : trimmed
    }

    /// Each project's name; two with one name say what tells them apart: their computers when those differ, else
    /// the folder each is in.
    static func names(_ projects: [(key: String, row: FleetRow)]) -> [String: String] {
        let named = projects.map { (key: $0.key, row: $0.row, name: name(folder($0.row.summary.cwd))) }
        var out: [String: String] = [:]
        for entry in named {
            let alike = named.filter { $0.name == entry.name }
            guard alike.count > 1 else {
                out[entry.key] = entry.name
                continue
            }
            let computers = Set(alike.map { $0.row.machineID ?? "" })
            if computers.count == alike.count, let computer = entry.row.machineName {
                out[entry.key] = "\(entry.name) · \(computer)"
            } else {
                let parent = (folder(entry.row.summary.cwd) as NSString).deletingLastPathComponent
                out[entry.key] = "\(entry.name) · \(FleetGroup.shellPath(parent).displaySafe)"
            }
        }
        return out
    }
}
