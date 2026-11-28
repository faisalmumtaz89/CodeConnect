import Foundation

/// A project's ended sessions archived together (artboard 48): which go and which stay, from each one's repository as
/// its computer read it just now, by Archive Session's own rule (`ChangesWords.archivable`), so the computer refuses
/// none for what the app could see.
public struct ArchiveEnded: Sendable, Equatable {
    /// One that goes: archived as Archive Session archives it, only while `branch` is at `headOID`.
    public struct Going: Sendable, Equatable {
        public let key: String
        public let machineID: String?
        public let branch: String
        public let headOID: String
    }

    public let going: [Going]
    /// Each that stays, in the card's words: its title and why.
    public let staying: [String]

    /// `rows`, the project's ended sessions in worktrees of their own; `gits`, each one's repository as read, absent
    /// where it could not be read.
    public init(_ rows: [FleetRow], gits: [String: RunGit]) {
        var going: [Going] = []
        var staying: [String] = []
        for row in rows {
            guard let git = gits[row.id] else {
                staying.append(FleetWords.staysUnread(row.title))
                continue
            }
            if git.changed.files > 0 || !git.changed.complete {
                staying.append(FleetWords.staysUncommitted(row.title))
            } else if git.base == nil {
                staying.append(FleetWords.staysNoBase(row.title))
            } else if ChangesWords.archivable(git, inWorktree: row.summary.inWorktree), let branch = git.branch,
                let head = git.headOID
            {
                going.append(Going(key: row.id, machineID: row.machineID, branch: branch, headOID: head))
            } else {
                staying.append(FleetWords.staysOffBase(row.title))
            }
        }
        self.going = going
        self.staying = staying
    }
}
