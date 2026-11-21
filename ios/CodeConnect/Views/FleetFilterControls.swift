import CodeConnectKit
import SwiftUI

/// What the fleet can be narrowed to: every computer the list names, then every project, each a
/// `FleetFilter`. Offered only when there are two or more of either, the Mac's rule.
struct FleetFilterChoices {
    struct Choice: Identifiable {
        let filter: FleetFilter
        let name: String
        /// A project's computer, which tells two projects named alike apart; nil for a computer.
        let computer: String?
        var id: FleetFilter { filter }
    }

    let computers: [Choice]
    let projects: [Choice]

    @MainActor init(machines: Machines) {
        computers = machines.models.filter(\.showsFleet).compactMap { model in
            model.machineName.map { Choice(filter: .computer(model.machineID ?? ""), name: $0, computer: nil) }
        }
        projects = FleetGroup.groups(machines.fleet.filter { $0.status != .ended }, by: .project).map {
            Choice(filter: .project($0.id), name: $0.name, computer: $0.computer)
        }
    }

    var offered: Bool { computers.count > 1 || projects.count > 1 }
}

/// The title as a menu: All, every computer, then every project, each with a project's computer as its subtitle; the
/// system's own checkmark marks the one shown, and the system scrolls a menu taller than the screen.
struct FleetFilterMenu: View {
    let choices: FleetFilterChoices
    @Binding var chosen: FleetFilter?

    var body: some View {
        Menu {
            item("All", nil, chosen: nil)
            Section("Computers") {
                ForEach(choices.computers) { item($0.name, nil, chosen: $0.filter) }
            }
            Section("Projects") {
                ForEach(choices.projects) { item($0.name, $0.computer, chosen: $0.filter) }
            }
        } label: {
            HStack(alignment: .firstTextBaseline, spacing: CC.space.xs) {
                Text(FleetWords.projects)
                    .ccType(CC.type.display)
                    .foregroundStyle(CC.text.primary)
                CCIcon("caret-down", size: 16, weight: .bold, relativeTo: .title)
                    .foregroundStyle(CC.text.secondary)
            }
            // Whole on one line beside the held-change pill, which gives way to its count instead.
            .fixedSize()
        }
        .accessibilityLabel("\(FleetWords.projects), show")
        .accessibilityIdentifier("fleet-filter-menu")
    }

    /// One line of the menu, checked while it is the one shown: its name, and a project's computer as the line's
    /// subtitle.
    private func item(_ name: String, _ computer: String?, chosen filter: FleetFilter?) -> some View {
        Toggle(isOn: Binding(get: { chosen == filter }, set: { if $0 { chosen = filter } })) {
            Text(name)
            if let computer { Text(computer) }
        }
    }
}

/// The narrowed list's chip: what it is narrowed to, and its ×, which shows every run again.
struct FleetFilterChip: View {
    let name: String
    let clear: () -> Void

    var body: some View {
        Button(action: clear) {
            HStack(spacing: CC.space.xxs) {
                Text(name)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.primary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                CCIcon("x", size: 10, weight: .bold, relativeTo: .footnote)
                    .foregroundStyle(CC.text.secondary)
            }
            .padding(.horizontal, CC.space.sm)
            .frame(minHeight: CC.size.controlSm)
            .background(CC.color.surfaceRaised, in: Capsule())
            .overlay { Capsule().strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline) }
            .ccHitTarget(minWidth: 0)
        }
        .buttonStyle(.plain)
        .accessibilityLabel("\(name), show every agent")
        .accessibilityIdentifier("fleet-filter-clear")
    }
}
