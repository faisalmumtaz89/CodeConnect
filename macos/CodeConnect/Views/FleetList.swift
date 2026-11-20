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
    /// The run open beside the list, if any.
    @Binding var opened: String?
    /// Opens Start an agent.
    let newAgent: () -> Void
    @State private var folded: Set<String> = []
    /// The row under the pointer, and the row whose "…" panel is open.
    @State private var hovered: String?
    @State private var moreOpen: String?
    /// The run End Run… is asking about, and what its computer said when it did not end.
    @State private var ending: FleetRow?
    @State private var endNote: String?
    /// The run Remove… is asking about, and what its computer said when it was not removed.
    @State private var removing: FleetRow?
    @State private var removeNote: String?

    var body: some View {
        let rows = narrowed(scope.rows(machines.fleet))
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

            // A computer refused for a different version shows why in place of its list: rows it listed before are
            // the last it said, and none can be acted on until it is reached again.
            if rows.isEmpty || shownComputerRefusal != nil {
                Group {
                    if let refused = shownComputerRefusal {
                        DifferentVersion(sentence: refused.linkHealth.detail) { refused.connection.retryNow() }
                    } else if scope == .active {
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
                                    let selected = opened == row.id
                                    let model = machines.model(for: row.machineID)
                                    let endable = model?.canEndRun(row.summary) == true
                                    // The iPhone's rule for its Remove: ended (or adopted), on a computer whose ccd
                                    // removes runs. Never with End run: a run CodeConnect started is removable only
                                    // once it has ended.
                                    let removable =
                                        row.summary.isRemovable && model?.daemonProfile.removesSessions == true
                                    let more = (endable || removable)
                                        && (hovered == row.id || moreOpen == row.id || HoverButtonStyle.forced("fleet.row.\(row.id)"))
                                    Button {
                                        moreOpen = nil
                                        opened = selected ? nil : row.id
                                    } label: {
                                        FleetLine(row: row, selected: selected, roomForMore: more)
                                    }
                                    .buttonStyle(.hover(radius: 0, id: "fleet.row.\(row.id)"))
                                    .accessibilityElement(children: .contain)
                                    .accessibilityAction { opened = selected ? nil : row.id }
                                    .accessibilityIdentifier("fleet.row.\(row.id)")
                                    .accessibilityAddTraits(selected ? .isSelected : [])
                                    // The run's "…" (decisions 185 and 217): on hover, its panel holding End Run…
                                    // while the run is live, or Remove… once it has ended (an adopted run's at any
                                    // time); for VoiceOver, which does not hover, that is the row's own action.
                                    .accessibilityActions {
                                        if endable {
                                            Button("End Run…") { ending = row }
                                                .accessibilityIdentifier("fleet.row.\(row.id).endRunAction")
                                        }
                                        if removable {
                                            Button("Remove…") { removing = row }
                                                .accessibilityIdentifier("fleet.row.\(row.id).removeAction")
                                        }
                                    }
                                    .overlay(alignment: .trailing) { moreButton(row, endable: endable, shown: more) }
                                    .onHover { hovered = $0 ? row.id : (hovered == row.id ? nil : hovered) }
                                    // Its panel opens below the row, over the rows after it.
                                    .zIndex(moreOpen == row.id ? 1 : 0)
                                    .id(group.id + "/" + row.id)
                                }
                            }
                        }
                        let finished = narrowed(FleetScope.finished.rows(machines.fleet)).count
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
                    // Room below the last row for its "…" panel, which opens downward and a scroll view clips.
                    .padding(.bottom, 40)
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
        #if DEBUG
            .onAppear { moreOpen = RenderWindow.openRunMenu }
        #endif
        // Ending a run cannot be undone: it asks once (the approved words), then says only what went wrong.
        .alert(
            "End this run?", isPresented: Binding(get: { ending != nil }, set: { if !$0 { ending = nil } }),
            presenting: ending
        ) { row in
            Button("End Run", role: .destructive) { endRun(row) }.accessibilityIdentifier("endRun.confirm")
            Button("Cancel", role: .cancel) {}.accessibilityIdentifier("endRun.cancel")
        } message: { row in
            Text("\(row.summary.agent.displayName) stops, and the run moves to Finished.")
        }
        .alert(
            "End Run", isPresented: Binding(get: { endNote != nil }, set: { if !$0 { endNote = nil } }),
            presenting: endNote
        ) { _ in
            Button("OK", role: .cancel) {}.accessibilityIdentifier("endRun.ok")
        } message: { note in
            Text(note)
        }
        // Removing forgets CodeConnect's own record of a run, never the agent's transcript: it asks once, in the
        // iPhone's words, then says only what went wrong.
        .alert(
            removing.map { "Remove \($0.label.project)?" } ?? "",
            isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }),
            presenting: removing
        ) { row in
            Button("Remove", role: .destructive) { remove(row) }.accessibilityIdentifier("remove.confirm")
            Button("Cancel", role: .cancel) {}.accessibilityIdentifier("remove.cancel")
        } message: { row in
            Text("\(row.summary.agent.displayName)'s own transcript stays on the computer.")
        }
        .alert(
            "Not removed", isPresented: Binding(get: { removeNote != nil }, set: { if !$0 { removeNote = nil } }),
            presenting: removeNote
        ) { _ in
            Button("OK", role: .cancel) {}.accessibilityIdentifier("remove.ok")
        } message: { note in
            Text(note)
        }
    }

    /// The row's "…" while it is shown: on hover, or while its panel is open.
    @ViewBuilder private func moreButton(_ row: FleetRow, endable: Bool, shown: Bool) -> some View {
        if shown {
            RunMore(
                id: "fleet.row.\(row.id).more",
                open: Binding(get: { moreOpen == row.id }, set: { moreOpen = $0 ? row.id : nil }),
                action: endable ? .endRun : .remove
            ) { if endable { ending = row } else { removing = row } }
            .padding(.trailing, 16)
        }
    }

    private func endRun(_ row: FleetRow) {
        guard let model = machines.model(for: row.machineID) else { return }
        Task { endNote = await model.endRun(row.summary) }
    }

    private func remove(_ row: FleetRow) {
        guard let model = machines.model(for: row.machineID) else { return }
        Task { removeNote = await model.removeSession(row.summary).refusal }
    }

    /// The computer the list is narrowed to, when it was refused for running a different version of CodeConnect,
    /// which the sidebar's "Different version" stands for.
    private var shownComputerRefusal: AppModel? {
        guard case .computer(let id) = filter,
            let model = machines.models.first(where: { ($0.machineID ?? "") == id }),
            model.linkHealth.cause == .refused(.differentVersion)
        else { return nil }
        return model
    }

    /// These rows as the sidebar's filter leaves them.
    private func narrowed(_ rows: [FleetRow]) -> [FleetRow] { filter?.narrow(rows) ?? rows }
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
        SegmentedTrack(
            options: FleetGroup.Kind.allCases, chosen: selection, fill: Theme.raised.opacity(0.6),
            id: { "fleet.grouping.\($0.title.lowercased())" }, choose: { selection = $0 }
        ) { option, chosen in
            Text(option.title)
                .font(Theme.ui(11.5, chosen ? .medium : .regular))
                .foregroundStyle(chosen ? Theme.text : Theme.text3)
                .padding(.horizontal, 9)
                .frame(height: 22)
        }
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
        .background(Theme.pane.opacity(0.55), in: Rectangle())
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
    /// Open beside the list: the selected fill, and a bar on its leading edge.
    let selected: Bool
    /// The run's "…" stands in its age's place, which hides while it shows: nothing else in the row moves.
    let roomForMore: Bool
    @State private var lastTick = Date()

    private var since: Date { row.lastActive }

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
            // daemon's own sentence; asked to end, that it is stopping, until it has.
            if row.status == .stopping {
                Text("Stopping…").font(Theme.ui(12)).foregroundStyle(Theme.text3).lineLimit(1)
            } else if let activity = row.activity {
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
                .frame(width: RunMore.width, alignment: .trailing)
                .opacity(roomForMore ? 0 : 1)
        }
        .padding(.horizontal, 16)
        .frame(height: 36)
        .background(selected ? Theme.selected : .clear, in: Rectangle())
        .overlay(alignment: .leading) {
            if selected { Rectangle().fill(Theme.text2).frame(width: 2) }
        }
        .task(id: AgeClock(since: since, scale: .age)) {
            await AgeTick.follow(AgeClock(since: since, scale: .age)) { lastTick = $0 }
        }
    }
}

/// A row's "…" (a run's, or a computer's in the sidebar), and the panel it opens below it, in the app's own panel style
/// (the model panel's), holding End Run… or Remove…, each of which asks before anything is sent.
struct RunMore: View {
    enum Action {
        case endRun, remove

        var title: String { self == .endRun ? "End Run…" : "Remove…" }
        var id: String { self == .endRun ? "endRun" : "remove" }
    }

    static let width: CGFloat = 26
    let id: String
    @Binding var open: Bool
    let action: Action
    let perform: () -> Void

    var body: some View {
        Button { open.toggle() } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Theme.text2)
                .frame(width: Self.width, height: 22)
                .background(open ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
                .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: id))
        .help("More")
        .accessibilityLabel("More")
        .accessibilityIdentifier(id)
        .accessibilityValue(open ? "open" : "closed")
        .overlay(alignment: .topTrailing) {
            if open { panel.offset(y: 28) }
        }
    }

    private var panel: some View {
        Button {
            open = false
            perform()
        } label: {
            Text(action.title).font(Theme.ui(12.5)).foregroundStyle(Theme.red).lineLimit(1)
                .padding(.horizontal, 12)
                .frame(maxWidth: .infinity, minHeight: 24, alignment: .leading)
                .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 4, id: "\(id).\(action.id)"))
        .padding(4)
        .frame(width: 180)
        .background(Theme.raised, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color.white.opacity(0.1)))
        .shadow(color: .black.opacity(0.5), radius: 18, y: 8)
        .accessibilityIdentifier("\(id).\(action.id)")
    }
}

/// A run's state as the fleet orders it (`FleetStatus`), drawn small: amber when it waits on you.
struct StateIcon: View {
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
            case .stopping:
                Circle().strokeBorder(Theme.text3.opacity(0.5), lineWidth: 1.5)
            case .failed:
                Circle().fill(Theme.red)
                Image(systemName: "xmark").font(.system(size: 6.5, weight: .heavy)).foregroundStyle(Theme.window)
            case .doneUnreviewed:
                Circle().fill(Theme.text2)
                Image(systemName: "checkmark").font(.system(size: 6.5, weight: .heavy)).foregroundStyle(Theme.window)
            case .done:
                Circle().strokeBorder(Theme.text3, lineWidth: 1.5)
                Image(systemName: "checkmark").font(.system(size: 6.5, weight: .heavy)).foregroundStyle(Theme.text3)
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
struct AgentMark: View {
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
