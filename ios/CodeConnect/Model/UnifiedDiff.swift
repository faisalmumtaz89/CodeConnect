import Foundation

/// A parsed `git diff` — files, hunks, lines, and the runs of unchanged context
/// that are worth hiding until asked for.
///
/// Written against what `git -C <cwd> diff HEAD` actually emits rather than
/// against the unified-diff grammar in the abstract, because the daemon runs
/// exactly that command. Anything unrecognised is preserved verbatim in
/// `preamble`/`trailing` instead of being dropped: the contract also appends
/// untracked file names, and silently swallowing them would make the diff claim
/// a completeness it does not have.
struct UnifiedDiff: Sendable, Hashable {
    var files: [FileDiff]
    /// Lines before the first `diff --git`.
    var preamble: [String]
    /// Lines after the last hunk that belong to no file — where the untracked
    /// names land.
    var trailing: [String]

    /// Blank lines do not count as content: splitting a trailing newline always
    /// produces one empty element, and "the tree is clean" must not depend on
    /// whether git ended its output with a newline.
    var isEmpty: Bool {
        files.isEmpty && !(preamble + trailing).contains { !$0.trimmingCharacters(in: .whitespaces).isEmpty }
    }
    var additions: Int { files.reduce(0) { $0 + $1.additions } }
    var deletions: Int { files.reduce(0) { $0 + $1.deletions } }

    /// How many lines of context to keep visible on each side of a change.
    static let contextLines = 3

    // MARK: - Model

    struct FileDiff: Sendable, Hashable, Identifiable {
        enum Status: Sendable, Hashable {
            case added, deleted, modified, renamed, binary

            var symbol: String {
                switch self {
                case .added: return "plus.circle"
                case .deleted: return "minus.circle"
                case .modified: return "pencil.circle"
                case .renamed: return "arrow.triangle.turn.up.right.circle"
                case .binary: return "doc.badge.gearshape"
                }
            }

            var label: String {
                switch self {
                case .added: return "added"
                case .deleted: return "deleted"
                case .modified: return "modified"
                case .renamed: return "renamed"
                case .binary: return "binary"
                }
            }
        }

        var id: String
        var oldPath: String?
        var newPath: String?
        var status: Status
        var hunks: [Hunk]
        /// `Binary files … differ`, mode changes, and anything else git said
        /// about the file that is not a hunk.
        var notes: [String]

        var displayPath: String {
            if status == .renamed, let oldPath, let newPath { return "\(oldPath) → \(newPath)" }
            return newPath ?? oldPath ?? id
        }

        /// Just the file name, for a chip that has to fit on a phone.
        var shortName: String {
            let path = status == .deleted ? (oldPath ?? id) : (newPath ?? oldPath ?? id)
            return (path as NSString).lastPathComponent
        }

        var additions: Int {
            hunks.reduce(0) { $0 + $1.lines.reduce(0) { $0 + ($1.kind == .addition ? 1 : 0) } }
        }
        var deletions: Int {
            hunks.reduce(0) { $0 + $1.lines.reduce(0) { $0 + ($1.kind == .deletion ? 1 : 0) } }
        }
    }

    struct Hunk: Sendable, Hashable, Identifiable {
        var id: String
        /// The `@@ … @@` line verbatim, including any section heading git added.
        var header: String
        var oldStart: Int
        var newStart: Int
        var lines: [Line]
        /// The hunk split into runs that are shown and runs that are folded.
        var segments: [Segment]
    }

    struct Line: Sendable, Hashable, Identifiable {
        enum Kind: Sendable, Hashable {
            case context, addition, deletion
            /// `\ No newline at end of file` — a fact about the file, not a line
            /// of it.
            case note
        }

        var id: Int
        var kind: Kind
        var text: String
        var oldNumber: Int?
        var newNumber: Int?
    }

    /// A run of hunk lines, either shown or folded away.
    enum Segment: Sendable, Hashable, Identifiable {
        case shown(id: Int, lines: [Line])
        case folded(id: Int, lines: [Line])

        var id: Int {
            switch self {
            case .shown(let id, _), .folded(let id, _): return id
            }
        }

        var lines: [Line] {
            switch self {
            case .shown(_, let lines), .folded(_, let lines): return lines
            }
        }

        var isFolded: Bool {
            if case .folded = self { return true }
            return false
        }
    }

    // MARK: - Parsing

    static func parse(_ text: String) -> UnifiedDiff {
        var files: [FileDiff] = []
        var preamble: [String] = []
        var trailing: [String] = []

        var current: FileDiff?
        var hunk: Hunk?
        var lineID = 0
        var oldNumber = 0
        var newNumber = 0
        var oldRemaining = 0
        var newRemaining = 0

        func closeHunk() {
            guard var open = hunk else { return }
            open.segments = fold(open.lines)
            current?.hunks.append(open)
            hunk = nil
        }

        func closeFile() {
            closeHunk()
            if let file = current { files.append(file) }
            current = nil
        }

        for raw in text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init) {
            // Inside a hunk, the declared counts — not a guess about prefixes —
            // decide where the hunk ends. Trailing content that happens to start
            // with a space or a plus would otherwise be eaten as diff body.
            if hunk != nil, oldRemaining > 0 || newRemaining > 0 {
                if let line = hunkLine(
                    raw, id: lineID, oldNumber: &oldNumber, newNumber: &newNumber,
                    oldRemaining: &oldRemaining, newRemaining: &newRemaining)
                {
                    lineID += 1
                    hunk?.lines.append(line)
                    continue
                }
                // Malformed body: stop trusting the counts rather than
                // mis-numbering everything after it.
                closeHunk()
            } else if hunk != nil {
                closeHunk()
            }

            if raw.hasPrefix("diff --git ") {
                closeFile()
                let paths = gitHeaderPaths(raw)
                current = FileDiff(
                    id: paths.new ?? paths.old ?? raw, oldPath: paths.old, newPath: paths.new,
                    status: .modified, hunks: [], notes: [])
                continue
            }

            if current == nil {
                // `git diff` output that is not introduced by `diff --git`
                // (untracked names, a daemon note) is kept verbatim.
                if files.isEmpty {
                    preamble.append(raw)
                } else {
                    trailing.append(raw)
                }
                continue
            }

            if raw.hasPrefix("@@") {
                closeHunk()
                guard let parsed = parseHunkHeader(raw) else {
                    current?.notes.append(raw)
                    continue
                }
                oldNumber = parsed.oldStart
                newNumber = parsed.newStart
                oldRemaining = parsed.oldCount
                newRemaining = parsed.newCount
                hunk = Hunk(
                    id: "\(current?.id ?? "")#\(parsed.oldStart)-\(parsed.newStart)-\(files.count)",
                    header: raw, oldStart: parsed.oldStart, newStart: parsed.newStart, lines: [],
                    segments: [])
                // A zero-line hunk is legal (a pure deletion of everything);
                // close it immediately so it is not left waiting for body.
                if oldRemaining == 0 && newRemaining == 0 { closeHunk() }
                continue
            }

            // Git emits a file's metadata *before* its hunks, never after. So a
            // line that is neither a new file header nor a hunk header, arriving
            // once this file already has hunks, does not belong to the file at
            // all — it is what the daemon appended after the diff, which is
            // where the untracked names live. Filing it as a file note would
            // hide them.
            if current?.hunks.isEmpty == false {
                closeFile()
                if files.isEmpty {
                    preamble.append(raw)
                } else {
                    trailing.append(raw)
                }
                continue
            }

            applyFileHeader(raw, to: &current)
        }

        closeFile()
        return UnifiedDiff(files: files, preamble: preamble, trailing: trailing)
    }

    // MARK: Header handling

    private static func applyFileHeader(_ raw: String, to file: inout FileDiff?) {
        guard var current = file else { return }
        defer { file = current }

        if raw.hasPrefix("--- ") {
            current.oldPath = strippedPath(String(raw.dropFirst(4)))
            if current.oldPath == nil { current.status = .added }
            return
        }
        if raw.hasPrefix("+++ ") {
            current.newPath = strippedPath(String(raw.dropFirst(4)))
            if current.newPath == nil { current.status = .deleted }
            return
        }
        if raw.hasPrefix("new file mode") {
            current.status = .added
            return
        }
        if raw.hasPrefix("deleted file mode") {
            current.status = .deleted
            return
        }
        if raw.hasPrefix("rename from ") || raw.hasPrefix("rename to ") {
            current.status = .renamed
            current.notes.append(raw)
            return
        }
        if raw.hasPrefix("Binary files ") || raw.hasPrefix("GIT binary patch") {
            current.status = .binary
            current.notes.append(raw)
            return
        }
        if raw.hasPrefix("index ") || raw.isEmpty { return }
        if raw.hasPrefix("old mode") || raw.hasPrefix("new mode")
            || raw.hasPrefix("similarity index") || raw.hasPrefix("dissimilarity index")
        {
            current.notes.append(raw)
            return
        }
        current.notes.append(raw)
    }

    /// `--- a/path` / `+++ b/path`; `/dev/null` means the file did not exist on
    /// that side.
    private static func strippedPath(_ value: String) -> String? {
        var path = value
        if let tab = path.firstIndex(of: "\t") { path = String(path[..<tab]) }
        path = path.trimmingCharacters(in: .whitespaces)
        if path == "/dev/null" { return nil }
        if path.hasPrefix("a/") || path.hasPrefix("b/") { path = String(path.dropFirst(2)) }
        return path.isEmpty ? nil : path
    }

    /// Best-effort split of `diff --git a/x b/y`. Only used until the `---`/`+++`
    /// lines arrive with the authoritative answer, so a path containing " b/" is
    /// a cosmetic risk for one line rather than a correctness one.
    private static func gitHeaderPaths(_ raw: String) -> (old: String?, new: String?) {
        let body = String(raw.dropFirst("diff --git ".count))
        guard let separator = body.range(of: " b/") else {
            return (strippedPath(body), strippedPath(body))
        }
        let old = String(body[body.startIndex..<separator.lowerBound])
        let new = String(body[separator.lowerBound...].dropFirst())
        return (strippedPath(old), strippedPath(new))
    }

    private static func parseHunkHeader(_ raw: String) -> (
        oldStart: Int, oldCount: Int, newStart: Int, newCount: Int
    )? {
        // @@ -oldStart,oldCount +newStart,newCount @@ optional heading
        guard raw.hasPrefix("@@ "), let close = raw.range(of: " @@") else { return nil }
        let ranges = raw[raw.index(raw.startIndex, offsetBy: 3)..<close.lowerBound]
        let parts = ranges.split(separator: " ")
        guard parts.count >= 2, parts[0].hasPrefix("-"), parts[1].hasPrefix("+") else { return nil }
        guard let old = parseRange(parts[0].dropFirst()), let new = parseRange(parts[1].dropFirst())
        else { return nil }
        return (old.start, old.count, new.start, new.count)
    }

    private static func parseRange(_ value: Substring) -> (start: Int, count: Int)? {
        let pieces = value.split(separator: ",")
        guard let start = Int(pieces[0]) else { return nil }
        // A missing count means exactly one line, per the unified-diff format.
        let count = pieces.count > 1 ? (Int(pieces[1]) ?? 1) : 1
        return (start, count)
    }

    private static func hunkLine(
        _ raw: String, id: Int, oldNumber: inout Int, newNumber: inout Int,
        oldRemaining: inout Int, newRemaining: inout Int
    ) -> Line? {
        if raw.hasPrefix("\\") {
            return Line(id: id, kind: .note, text: raw, oldNumber: nil, newNumber: nil)
        }
        // An empty line inside a hunk is a context line whose single space was
        // stripped somewhere in transit. Accepting it keeps a diff readable that
        // would otherwise stop dead at the first blank line.
        let marker = raw.first ?? " "
        let body = raw.isEmpty ? "" : String(raw.dropFirst())
        switch marker {
        case " ":
            guard oldRemaining > 0, newRemaining > 0 else { return nil }
            let line = Line(
                id: id, kind: .context, text: body, oldNumber: oldNumber, newNumber: newNumber)
            oldNumber += 1
            newNumber += 1
            oldRemaining -= 1
            newRemaining -= 1
            return line
        case "+":
            guard newRemaining > 0 else { return nil }
            let line = Line(id: id, kind: .addition, text: body, oldNumber: nil, newNumber: newNumber)
            newNumber += 1
            newRemaining -= 1
            return line
        case "-":
            guard oldRemaining > 0 else { return nil }
            let line = Line(id: id, kind: .deletion, text: body, oldNumber: oldNumber, newNumber: nil)
            oldNumber += 1
            oldRemaining -= 1
            return line
        default:
            return nil
        }
    }

    // MARK: Folding

    /// Split a hunk into shown and folded runs.
    ///
    /// A run of unchanged lines longer than the context that surrounds it is
    /// folded to a single tappable row. Runs at the edges of a hunk keep context
    /// only on their inward side — the three lines nearest the change are the
    /// ones worth reading.
    static func fold(_ lines: [Line], context: Int = contextLines) -> [Segment] {
        guard !lines.isEmpty else { return [] }
        var segments: [Segment] = []
        var nextID = 0
        var index = 0

        func append(_ run: [Line], folded: Bool) {
            guard !run.isEmpty else { return }
            segments.append(folded ? .folded(id: nextID, lines: run) : .shown(id: nextID, lines: run))
            nextID += 1
        }

        while index < lines.count {
            if lines[index].kind == .context {
                var end = index
                while end < lines.count, lines[end].kind == .context { end += 1 }
                let run = Array(lines[index..<end])
                let head = index == 0 ? 0 : context
                let tail = end == lines.count ? 0 : context
                if run.count > head + tail + 1 {
                    append(Array(run.prefix(head)), folded: false)
                    append(Array(run.dropFirst(head).dropLast(tail)), folded: true)
                    append(Array(run.suffix(tail)), folded: false)
                } else {
                    append(run, folded: false)
                }
                index = end
            } else {
                var end = index
                while end < lines.count, lines[end].kind != .context { end += 1 }
                append(Array(lines[index..<end]), folded: false)
                index = end
            }
        }
        return segments
    }
}
