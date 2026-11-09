import Foundation

/// **The fleet in groups**, for a list that shows it by computer or by project.
///
/// The groups come in the order of their first rows, and each keeps the fleet's own order inside, so a
/// grouped list reads in the order the flat one does (`FleetOrdering`). A computer is its paired
/// machine (`FleetRow.machineID`). A project is one working directory on one computer, so one folder
/// on two computers is two projects, and so are two folders the daemon names alike; its name is the
/// daemon's (`RunLabel.project`).
public struct FleetGroup: Sendable, Hashable, Identifiable {
    public enum Kind: Sendable, Hashable, CaseIterable {
        case computer
        case project
    }

    public let id: String
    public let name: String
    public let rows: [FleetRow]

    /// The decisions this group's runs are waiting on a person for, counted as `FleetCount` counts
    /// the whole fleet's.
    public var waiting: Int { FleetCount.decisions(in: rows) }

    /// A project's folder, as a shell prints it (`shellPath`).
    public var folder: String { Self.shellPath(rows[0].summary.shownCwd) }

    /// The computer a project is on: with its folder, what tells two projects named alike apart.
    public var computer: String { rows[0].machineName ?? "Unknown computer" }

    /// The home folder as `~`, the way a shell prints it: `/Users/<name>/…` on a
    /// Mac and `/home/<name>/…` on Linux.
    public static func shellPath(_ cwd: String) -> String {
        let parts = cwd.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count >= 3, parts[0].isEmpty, parts[1] == "Users" || parts[1] == "home"
        else { return cwd }
        return (["~"] + parts.dropFirst(3)).joined(separator: "/")
    }

    public static func groups(_ rows: [FleetRow], by kind: Kind) -> [FleetGroup] {
        var keys: [String] = []
        var grouped: [String: (name: String, rows: [FleetRow])] = [:]
        for row in rows {
            let key: String
            let name: String
            switch kind {
            case .computer:
                key = row.machineID ?? ""
                name = row.machineName ?? "Unknown computer"
            case .project:
                key = (row.machineID ?? "") + "\n" + row.summary.shownCwd
                name = row.label.project
            }
            if grouped[key] == nil {
                keys.append(key)
                grouped[key] = (name, [])
            }
            grouped[key]?.rows.append(row)
        }
        return keys.compactMap { key in
            grouped[key].map { FleetGroup(id: key, name: $0.name, rows: $0.rows) }
        }
    }
}
