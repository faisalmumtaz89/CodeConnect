import Foundation

/// The Mac's session list by project (Release 2): every session under its project, the projects in the order the
/// person should look at them, and the one count of what needs them. The iPhone keeps `FleetGroup`.
///
/// A project is one folder on one computer: the session's folder, or for a session in a worktree of its own
/// (`<folder>/.claude/worktrees/<name>`, as the daemon's `in_worktree` reads it) the folder the worktree belongs to.
/// Its name is that folder's.
public struct ProjectList: Sendable, Equatable {
    /// Where the open session stood when it was opened: it stays there, and its project too, while it is open,
    /// whatever its state becomes and whatever the list narrows to.
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
        /// Its folder's name; with its computer, or its parent folder, when another project in the list has it.
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
        /// Hidden, and shown only because something in it needs the person, or because its session is open.
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
    /// The projects "Only these" leaves out, by name, in name order.
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
    /// person; `revealed` is how many reveals each project has had; `deck` the waiting sessions' keys in the Deck's
    /// order (`Deck.sort`: most urgent card, then oldest); `held` the open session's place.
    public init(
        _ rows: [FleetRow], computer: String? = nil, onlyNeedsYou: Bool = false, pinned: [String] = [],
        hidden: Set<String> = [], revealed: [String: Int] = [:], deck: [String] = [], held: Held? = nil
    ) {
        let listed = rows.filter { !$0.summary.archived }
        needsYou = listed.count(where: Self.needsYou)
        self.onlyNeedsYou = onlyNeedsYou && needsYou > 0
        let only = self.onlyNeedsYou
        let open = held?.session
        let shown = listed.filter { Self.needsYou($0) || computer == nil || $0.machineID == computer || $0.id == open }
        var place: [String: Int] = [:]
        for (index, key) in deck.enumerated() where place[key] == nil { place[key] = index }

        var order: [String] = []
        var groups: [String: [FleetRow]] = [:]
        for row in shown {
            let key = Self.key(row)
            if groups[key] == nil { order.append(key) }
            groups[key, default: []].append(row)
        }

        // Each project as it will be drawn, with what orders it, before names: a project Hide keeps out is not in
        // the list, so it does not lend another its suffix.
        struct Built {
            var key: String
            var first: FleetRow
            var rows: [FleetRow]
            var need: Int
            var sessions: Int
            var earlier: Int
            var hidden: Bool
            var quiet: Bool
            /// The most urgent rank among what needs you, and within it the Deck's place or the oldest activity.
            var urgency: (rank: Int, wait: Double)
            var latest: Date
        }
        var built: [Built] = []
        for key in order {
            let all = groups[key]!
            let need = all.count(where: Self.needsYou)
            let isHidden = hidden.contains(key)
            let holds = all.contains { $0.id == open }
            if isHidden && need == 0 && !holds { continue }
            // A hidden project shown only for its open session shows that session alone.
            let kept = all.filter {
                (only || isHidden && need == 0) ? Self.needsYou($0) || $0.id == open : true
            }
            let sorted = kept.sorted(by: Self.before)
            let live = sorted.filter { Self.rank($0) < Self.endedRank }
            let ended = sorted.filter { Self.rank($0) == Self.endedRank }
            let reveals = revealed[key] ?? 0
            let reveal = min(ended.count, (live.isEmpty ? 1 + reveals : reveals) * Self.page)
            var rows = live + ended.prefix(reveal)
            var earlier = ended.count - reveal
            if let open, let row = sorted.first(where: { $0.id == open }), !rows.contains(where: { $0.id == open }) {
                rows.append(row)
                earlier -= 1
            }
            if let held, held.project == key, let at = rows.firstIndex(where: { $0.id == held.session }) {
                let row = rows.remove(at: at)
                rows.insert(row, at: min(held.rowIndex, rows.count))
            }
            let waiting = all.filter(Self.needsYou)
            let best = waiting.map(Self.rank).min() ?? Int.max
            let wait = waiting.filter { Self.rank($0) == best }.map { row -> Double in
                best == 0 ? Double(place[row.id] ?? Int.max) : row.lastActive.timeIntervalSinceReferenceDate
            }.min() ?? .infinity
            built.append(Built(
                key: key, first: all.first!, rows: rows, need: need, sessions: all.count, earlier: earlier,
                hidden: isHidden, quiet: rows.isEmpty, urgency: (best, wait),
                latest: all.map(\.lastActive).max() ?? .distantPast))
        }

        let names = Self.names(built.map { ($0.key, $0.first) })
        let pins = Dictionary(pinned.enumerated().map { ($1, $0) }, uniquingKeysWith: { first, _ in first })
        built.sort { a, b in
            let pa = pins[a.key] ?? Int.max, pb = pins[b.key] ?? Int.max
            if pa != pb { return pa < pb }
            if (a.need > 0) != (b.need > 0) { return a.need > 0 }
            if a.need > 0, a.urgency != b.urgency { return a.urgency < b.urgency }
            if a.latest != b.latest { return a.latest > b.latest }
            return a.key < b.key
        }
        var projects = built.filter { !$0.quiet }.map { entry in
            Project(
                id: entry.key, name: names[entry.key]!, needsYou: entry.need, sessions: entry.sessions,
                rows: entry.rows, earlier: entry.earlier, pinned: pins[entry.key] != nil, hidden: entry.hidden,
                machineID: entry.first.machineID, folder: Self.folder(entry.first.summary.cwd))
        }
        if let held, let at = projects.firstIndex(where: { $0.id == held.project }) {
            let project = projects.remove(at: at)
            projects.insert(project, at: min(held.projectIndex, projects.count))
        }
        self.projects = projects
        quiet = built.filter(\.quiet).map { names[$0.key]! }.sorted()
    }

    // MARK: Order within a project

    /// Waiting, then failed or at its limit, idle, working, done (unseen first), ended.
    static func rank(_ row: FleetRow) -> Int {
        if row.status == .blocked { return 0 }
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
        let ra = rank(a), rb = rank(b)
        if ra != rb { return ra < rb }
        if a.lastActive != b.lastActive { return a.lastActive > b.lastActive }
        return a.id < b.id
    }

    // MARK: Projects

    /// A session's project: its computer and the folder it belongs to.
    static func key(_ row: FleetRow) -> String { "\(row.machineID ?? "")\u{1F}\(folder(row.summary.cwd))" }

    /// The folder a session belongs to: its own, or for a worktree of its own (`<folder>/.claude/worktrees/<name>`,
    /// that last) the folder that holds the worktree.
    static func folder(_ cwd: String) -> String {
        var path = cwd
        while path.count > 1, path.hasSuffix("/") { path.removeLast() }
        let parts = path.split(separator: "/", omittingEmptySubsequences: false)
        if parts.count >= 4, parts[parts.count - 3] == ".claude", parts[parts.count - 2] == "worktrees",
            !parts[parts.count - 1].isEmpty, parts[parts.count - 1] != ".", parts[parts.count - 1] != ".."
        {
            let kept = parts.dropLast(3).joined(separator: "/")
            return kept.isEmpty ? "/" : kept
        }
        return path
    }

    /// Words drawn on one line: nothing that breaks or reorders it.
    static func oneLine(_ text: String) -> String {
        text.displaySafe
            .filter { !$0.isNewline && !$0.unicodeScalars.contains { $0.properties.generalCategory == .control } }
            .trimmingCharacters(in: .whitespaces)
    }

    /// A folder's name as the list shows it: its last component on one line.
    static func name(_ folder: String) -> String {
        let shown = oneLine(folder.split(separator: "/").last.map(String.init) ?? "")
        return shown.isEmpty || shown == "." || shown == ".." ? RunLabel.unknown.project : shown
    }

    /// Each project's name. Two with one name say what tells each apart: its computer when no other of that name is
    /// on it, else the folder it is in.
    static func names(_ projects: [(key: String, row: FleetRow)]) -> [String: String] {
        let named = projects.map { (key: $0.key, row: $0.row, name: name(folder($0.row.summary.cwd))) }
        var out: [String: String] = [:]
        for alike in Dictionary(grouping: named, by: \.name).values {
            guard alike.count > 1 else {
                out[alike[0].key] = alike[0].name
                continue
            }
            let perComputer = Dictionary(grouping: alike, by: { $0.row.machineID ?? "" }).mapValues(\.count)
            for entry in alike {
                let computer = oneLine(entry.row.machineName ?? "")
                if perComputer[entry.row.machineID ?? ""] == 1, !computer.isEmpty {
                    out[entry.key] = "\(entry.name) · \(computer)"
                } else {
                    let parent = (folder(entry.row.summary.cwd) as NSString).deletingLastPathComponent
                    out[entry.key] = "\(entry.name) · \(oneLine(FleetGroup.shellPath(parent)))"
                }
            }
        }
        return out
    }
}
