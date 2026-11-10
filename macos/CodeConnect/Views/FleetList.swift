import CodeConnectKit
import SwiftUI

/// Which runs the list shows: the ones still going, or the ones that have ended.
enum FleetScope: String {
    case active = "Active"
    case finished = "Finished"

    func rows(_ fleet: [FleetRow]) -> [FleetRow] {
        fleet.filter { ($0.status == .ended) == (self == .finished) }
    }
}

extension FleetGroup.Kind {
    var title: String { self == .computer ? "Computer" : "Project" }
}

/// What the sidebar narrows the list to: one computer's runs, or one project's, a project being one folder on one
/// computer. Each names a group by its `FleetGroup` id.
enum FleetFilter: Hashable {
    case computer(String)
    case project(String)

    /// The rows of the group this names, as the list groups them; none once that group has no rows.
    func narrow(_ rows: [FleetRow]) -> [FleetRow] {
        let (kind, id): (FleetGroup.Kind, String) =
            switch self {
            case .computer(let id): (.computer, id)
            case .project(let id): (.project, id)
            }
        return FleetGroup.groups(rows, by: kind).first { $0.id == id }?.rows ?? []
    }

    /// What the list's chip calls it: a computer as the sidebar names it, whether or not anything runs there.
    @MainActor func name(in machines: Machines) -> String {
        switch self {
        case .computer(let id):
            machines.models.first { ($0.machineID ?? "") == id }?.machineName ?? "Unknown computer"
        case .project(let key):
            FleetGroup.groups(machines.fleet, by: .project).first { $0.id == key }?.name ?? "Project"
        }
    }
}

/// Every run on every paired computer, one line each, grouped by computer or by project in the
/// fleet's own order, or only one computer's or project's when the sidebar narrows it. A run waiting on you is a
/// state of its row, and its group says how many decisions wait there.
struct FleetList: View {
    @Environment(Machines.self) private var machines
    @Binding var scope: FleetScope
    @Binding var grouping: FleetGroup.Kind
    @Binding var filter: FleetFilter?
    /// Opens Start an agent.
    let newAgent: () -> Void
    @State private var folded: Set<String> = []

    var body: some View {
        let all = scope.rows(machines.fleet)
        let rows = filter?.narrow(all) ?? all
        let namingComputer = machines.models.filter(\.showsFleet).count > 1
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Text(scope.rawValue).font(Theme.ui(13, .semibold)).foregroundStyle(Theme.text)
                Text(verbatim: "\(rows.count)").font(Theme.ui(12)).foregroundStyle(Theme.text3)
                if let filter {
                    FilterChip(name: filter.name(in: machines)) { self.filter = nil }
                }
                Spacer()
                GroupingSwitch(selection: $grouping)
            }
            .padding(.horizontal, 16)
            .frame(height: 52)
            Rectangle().fill(Theme.hairline).frame(height: 1)

            if rows.isEmpty {
                Group {
                    if scope == .active {
                        NoAgentsYet(newAgent: newAgent)
                    } else {
                        Text("No finished agents.").font(Theme.ui(12.5)).foregroundStyle(Theme.text3)
                    }
                }
                .padding(.horizontal, 24)
                .frame(maxWidth: 440)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(FleetGroup.groups(rows, by: grouping)) { group in
                            let isFolded = folded.contains(group.id)
                            Button {
                                if isFolded { folded.remove(group.id) } else { folded.insert(group.id) }
                            } label: {
                                GroupHeader(
                                    group: group, grouping: grouping, namingComputer: namingComputer,
                                    folded: isFolded)
                            }
                            .buttonStyle(.hover(radius: 0, id: "fleet.group.\(group.axID)"))
                            .accessibilityIdentifier("fleet.group.\(group.axID)")
                            .accessibilityValue(isFolded ? "folded" : "open")
                            if !isFolded {
                                ForEach(group.rows) { row in
                                    FleetLine(row: row)
                                }
                            }
                        }
                        let finished = FleetScope.finished.rows(machines.fleet).count
                        if scope == .active, finished > 0 {
                            Button { scope = .finished } label: {
                                HStack(spacing: 8) {
                                    Image(systemName: "chevron.right").font(.system(size: 8.5, weight: .bold))
                                    Text("Finished").font(Theme.ui(12, .medium))
                                    Text(verbatim: "\(finished)").font(Theme.ui(12)).foregroundStyle(Theme.text3)
                                    Spacer()
                                }
                                .foregroundStyle(Theme.text2)
                                .padding(.horizontal, 16)
                                .frame(height: 34)
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.hover(radius: 0, id: "fleet.showFinished"))
                            .accessibilityIdentifier("fleet.showFinished")
                        }
                    }
                    .padding(.bottom, 8)
                }
            }

            Rectangle().fill(Theme.hairline).frame(height: 1)
            HStack(spacing: 14) {
                KeyHint(.agent)
                KeyHint(.computer)
                Spacer()
            }
            .padding(.horizontal, 16)
            .frame(height: 34)
        }
        .background(Theme.window)
    }
}

/// What the list is narrowed to, beside its count; a click shows every run again.
private struct FilterChip: View {
    let name: String
    let clear: () -> Void

    var body: some View {
        Button(action: clear) {
            HStack(spacing: 6) {
                Text(name).font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.text).lineLimit(1)
                Image(systemName: "xmark").font(.system(size: 8, weight: .bold)).foregroundStyle(Theme.text2)
            }
            .padding(.horizontal, 9)
            .frame(height: 22)
            .background(Theme.raised, in: Capsule())
            .overlay(Capsule().strokeBorder(Theme.hairline))
        }
        .buttonStyle(.hover(radius: 11, over: true, id: "fleet.filter.clear"))
        .help("Show every agent")
        .accessibilityLabel("\(name), show every agent")
        .accessibilityIdentifier("fleet.filter.clear")
    }
}

private struct GroupingSwitch: View {
    @Binding var selection: FleetGroup.Kind

    var body: some View {
        HStack(spacing: 0) {
            ForEach(FleetGroup.Kind.allCases, id: \.self) { option in
                Button { selection = option } label: {
                    Text(option.title)
                        .font(Theme.ui(11.5, selection == option ? .medium : .regular))
                        .foregroundStyle(selection == option ? Theme.text : Theme.text3)
                        .padding(.horizontal, 9)
                        .frame(height: 22)
                        .background(selection == option ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
                }
                .buttonStyle(.hover(radius: 5, id: "fleet.grouping.\(option.title.lowercased())"))
                .accessibilityIdentifier("fleet.grouping.\(option.title.lowercased())")
                .accessibilityAddTraits(selection == option ? .isSelected : [])
            }
        }
        .padding(2)
        .background(Theme.raised.opacity(0.6), in: RoundedRectangle(cornerRadius: 7))
        .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
    }
}

/// A group's name, how many runs it holds, how many decisions wait in it, and where a project is
/// (`ProjectPlace`); a click folds it.
private struct GroupHeader: View {
    let group: FleetGroup
    let grouping: FleetGroup.Kind
    let namingComputer: Bool
    let folded: Bool

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: folded ? "chevron.right" : "chevron.down")
                .font(.system(size: 8.5, weight: .bold))
                .foregroundStyle(Theme.text3)
                .frame(width: 9)
            Image(systemName: grouping == .computer ? "desktopcomputer" : "folder")
                .font(.system(size: 11))
                .foregroundStyle(grouping == .computer ? Theme.text2 : Theme.text3)
            Text(group.name).font(Theme.ui(12, .medium)).foregroundStyle(Theme.text).lineLimit(1)
            Text(verbatim: "\(group.rows.count)").font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
            if group.waiting > 0 {
                Text(verbatim: "\(group.waiting) waiting").font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.amber)
            }
            Spacer(minLength: 16)
            if grouping == .project {
                ProjectPlace(project: group, namingComputer: namingComputer, size: 11)
            }
        }
        .padding(.horizontal, 16)
        .frame(height: 32)
        .contentShape(Rectangle())
        .background(Theme.pane.opacity(0.55))
        .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
    }
}

/// Where a project is: its computer, when more than one is paired, and its folder. Short in a narrow
/// place, the two share the room, each cut where it keeps its meaning: the folder from the front, where
/// its name is, and the computer in the middle, since one owner's Macs share the front of their names.
struct ProjectPlace: View {
    let project: FleetGroup
    let namingComputer: Bool
    let size: CGFloat

    var body: some View {
        HStack(spacing: 4) {
            if namingComputer {
                Text(project.computer).font(Theme.ui(size)).lineLimit(1).truncationMode(.middle)
                Text(verbatim: "·").font(Theme.ui(size))
            }
            Text(project.folder)
                .font(Theme.mono(size))
                .lineLimit(1)
                .truncationMode(.head)
        }
        .foregroundStyle(Theme.text3)
    }
}

/// One run on one line: its state, what it is called, what it is doing, which agent, and how long
/// since it last did anything. Its computer is its group's to say.
private struct FleetLine: View {
    let row: FleetRow
    @State private var lastTick = Date()

    private var since: Date { row.lastEventAt ?? row.summary.updatedDate }

    var body: some View {
        HStack(spacing: 10) {
            StateIcon(status: row.status)
                .accessibilityIdentifier("fleet.row.\(row.id).status")
            Text(row.label.inline)
                .font(Theme.ui(12.5, row.status == .blocked ? .medium : .regular))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
                .layoutPriority(1)
            // What it is on: the tool call in prose and its argument as code, or else the
            // daemon's own sentence.
            if let activity = row.activity {
                Text(activity.tool).font(Theme.ui(12)).foregroundStyle(Theme.text2).lineLimit(1)
                if let argument = activity.argument, !argument.isEmpty {
                    Text(argument).font(Theme.mono(11.5)).foregroundStyle(Theme.text3).lineLimit(1)
                }
            } else if !row.subtitle.isEmpty {
                Text(row.subtitle).font(Theme.ui(12)).foregroundStyle(Theme.text3).lineLimit(1)
            }
            Spacer(minLength: 6)
            AgentMark(agent: row.summary.agent)
            Text(Format.age(since: since, now: AgeTick.renderTime(lastTick: lastTick)))
                .font(Theme.ui(11.5))
                .foregroundStyle(Theme.text3)
                .monospacedDigit()
                .frame(width: 26, alignment: .trailing)
        }
        .padding(.horizontal, 16)
        .frame(height: 36)
        .contentShape(Rectangle())
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("fleet.row.\(row.id)")
        .task(id: AgeClock(since: since, scale: .age)) {
            await AgeTick.follow(AgeClock(since: since, scale: .age)) { lastTick = $0 }
        }
    }
}

/// A run's state as the fleet orders it (`FleetStatus`), drawn small: amber when it waits on you.
private struct StateIcon: View {
    let status: FleetStatus

    var body: some View {
        ZStack {
            switch status {
            case .blocked:
                Circle().strokeBorder(Theme.amber, lineWidth: 1.5)
                Circle().fill(Theme.amber).frame(width: 5, height: 5)
            case .running:
                Circle().strokeBorder(Theme.text3.opacity(0.5), lineWidth: 1.5)
                Circle().trim(from: 0, to: 0.62).stroke(Theme.text, lineWidth: 1.5).rotationEffect(.degrees(-90))
                    .padding(0.75)
            case .failed:
                Circle().fill(Theme.red)
                Image(systemName: "xmark").font(.system(size: 6.5, weight: .heavy)).foregroundStyle(Theme.window)
            case .doneUnreviewed:
                Circle().fill(Theme.text2)
                Image(systemName: "checkmark").font(.system(size: 6.5, weight: .heavy)).foregroundStyle(Theme.window)
            case .idle:
                Circle().strokeBorder(Theme.text3, style: StrokeStyle(lineWidth: 1.5, dash: [2, 2]))
            case .ended:
                Circle().fill(Theme.text3)
                Image(systemName: "checkmark").font(.system(size: 6.5, weight: .heavy)).foregroundStyle(Theme.window)
            }
        }
        .frame(width: 13, height: 13)
        .help(status.label)
        // One element, read as the word it stands for: its marks alone read as symbol names ("Selected").
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(status.label)
    }
}

/// Which agent a run is: Claude's asterisk, Codex's brackets, or the daemon's own word for one this
/// build does not know.
private struct AgentMark: View {
    let agent: AgentKind

    var body: some View {
        Group {
            switch agent {
            case .claude: Image(systemName: "asterisk").font(.system(size: 9, weight: .bold))
            case .codex: Image(systemName: "chevron.left.forwardslash.chevron.right").font(.system(size: 7.5, weight: .bold))
            case .unsupported: Image(systemName: "questionmark").font(.system(size: 8, weight: .bold))
            }
        }
        .foregroundStyle(Theme.text2)
        .frame(width: 18, height: 18)
        .background(Circle().fill(Color.white.opacity(0.08)))
        .help(agent.displayName)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(agent.displayName)
    }
}
