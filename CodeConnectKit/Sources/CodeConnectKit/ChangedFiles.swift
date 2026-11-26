import Foundation

/// One of a run's changed files as its Changes panel lists it: the path in its folder and name, its lines, and its
/// part of the captured diff when the capture holds one (an untracked file, a binary one, or one past the daemon's
/// cut has none).
public struct ChangedFile: Identifiable, Hashable, Sendable {
    /// Its path from the repository's top folder (a renamed file's new one).
    public let path: String
    /// The folder part of `path`, with its trailing slash; empty at the top.
    public let folder: String
    /// The file's own name.
    public let name: String
    /// Lines added and removed; nil when the computer could not count them (a binary file, one past its budget).
    public let added: UInt64?
    public let removed: UInt64?
    public let diff: UnifiedDiff.FileDiff?

    public var id: String { path }

    init(path: String, added: UInt64?, removed: UInt64?, diff: UnifiedDiff.FileDiff?) {
        self.path = path
        let slash = path.lastIndex(of: "/")
        folder = slash.map { String(path[...$0]) } ?? ""
        name = slash.map { String(path[path.index(after: $0)...]) } ?? path
        self.added = added
        self.removed = removed
        self.diff = diff
    }

    /// A run's changed files, by folder and then name: the computer's own list (`SessionDiff.files`, minor 53), each
    /// with its part of `parsed` when the capture holds one; from an older computer, or when git could not list them,
    /// the files `parsed` holds.
    public static func rows(_ raw: SessionDiff, _ parsed: UnifiedDiff) -> [ChangedFile] {
        let rows: [ChangedFile]
        if let files = raw.files {
            rows = files.map { file in
                let diff = parsed.files.first { parsedFile in
                    parsedFile.newPath == file.path || (file.status == .deleted && parsedFile.oldPath == file.path)
                }
                return ChangedFile(path: file.path, added: file.added, removed: file.removed, diff: diff)
            }
        } else {
            rows = parsed.files.map { file in
                ChangedFile(
                    path: file.newPath ?? file.oldPath ?? file.id, added: UInt64(file.additions),
                    removed: UInt64(file.deletions), diff: file)
            }
        }
        return rows.sorted { ($0.folder, $0.name) < ($1.folder, $1.name) }
    }
}
