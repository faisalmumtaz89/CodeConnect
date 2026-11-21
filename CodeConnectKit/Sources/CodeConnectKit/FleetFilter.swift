import Foundation

/// What the sidebar narrows the list to: one computer's runs, or one project's, a project being one folder on one
/// computer. Each names a group by its `FleetGroup` id.
public enum FleetFilter: Hashable {
    case computer(String)
    case project(String)

    /// The rows of the group this names, as the list groups them; none once that group has no rows.
    public func narrow(_ rows: [FleetRow]) -> [FleetRow] {
        let (kind, id): (FleetGroup.Kind, String) =
            switch self {
            case .computer(let id): (.computer, id)
            case .project(let id): (.project, id)
            }
        return FleetGroup.groups(rows, by: kind).first { $0.id == id }?.rows ?? []
    }

    /// What the list's chip calls it: a computer as the sidebar names it, whether or not anything runs there.
    @MainActor public func name(in machines: Machines) -> String {
        switch self {
        case .computer(let id):
            machines.models.first { ($0.machineID ?? "") == id }?.machineName ?? "Unknown computer"
        case .project(let key):
            FleetGroup.groups(machines.fleet, by: .project).first { $0.id == key }?.name ?? "Project"
        }
    }
}
