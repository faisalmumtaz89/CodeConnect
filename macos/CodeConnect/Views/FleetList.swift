import CodeConnectKit
import SwiftUI

/// Which runs the list shows: the ones still going, or the ones that have ended.
enum FleetScope: String {
    case active, finished

    /// The scope by name: the ended runs carry their status's own word.
    var title: String { self == .active ? "Active" : FleetStatus.ended.label }

    func rows(_ fleet: [FleetRow]) -> [FleetRow] {
        fleet.filter { ($0.status == .ended) == (self == .finished) }
    }
}

extension FleetGroup.Kind {
    var title: String { self == .computer ? "Computer" : "Project" }
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
    /// A computer whose pairing has to change: Add Computer, its address already in the code form.
    let pairAgain: (String) -> Void
    /// Opens Start an agent.
    let newAgent: () -> Void
    @State private var folded: Set<String> = []
    /// The row under the pointer, and the row whose "…" panel is open.
    @State private var hovered: String?
    /// The window's one row menu, which a run's "…" or a right-click on its row opens.
    @Environment(RowMenu.self) private var menu
    @Environment(\.headerClearance) private var headerClearance
    /// The list has the keyboard: one stop, taken by a click on a row and kept across an arrow's move.
    @FocusState private var listFocused: Bool
    /// The run End Run… is asking about, and what its computer said when it did not end.
    @State private var ending: FleetRow?
    @State private var endNote: String?
    /// The run Remove… is asking about, and what its computer said when it was not removed.
    @State private var removing: FleetRow?
    @State private var removeNote: String?
    /// Choosing ended runs to remove together (the iPhone's Select): what is chosen, the batch asked about, whether it
    /// is on its way, and what the last batch did not remove, by run, with the computer's reason.
    @State private var selecting = false
    @State private var chosen: Set<String> = []
    @State private var removingMany: [String]?
    @State private var batchRunning = false
    @State private var batchRefused: [String: String] = [:]

    var body: some View {
        let rows = narrowed(scope.rows(machines.fleet))
        let namingComputer = machines.models.filter(\.showsFleet).count > 1
        VStack(spacing: 0) {
            let selectable = selectableKeys(rows)
            let picked = FleetGroup.picked(rows, by: grouping, folded: folded, chosen: chosen, removable: removable)
            HStack(spacing: 10) {
                if selecting {
                    Text(verbatim: "\(picked.count) selected").font(Theme.ui(13, .semibold)).foregroundStyle(Theme.text)
                    Spacer()
                    let all = !selectable.isEmpty && chosen.isSuperset(of: selectable)
                    ActionButton(title: all ? "Deselect All" : "Select All", key: nil, prominent: false, id: "fleet.selectAll") {
                        chosen = all ? [] : Set(selectable)
                    }
                    .keyboardShortcut("a", modifiers: .command)
                    .disabled(batchRunning)
                    // No Done while a batch runs: the batch holds the choice until it has answered.
                    if !batchRunning {
                        // While the card asks, Esc is the card's: Done keeps its face (the card stops clicks behind
                        // it) but not the key.
                        ActionButton(title: "Done", key: nil, prominent: false, id: "fleet.selectDone") { endSelection() }
                            .keyboardShortcut(removingMany == nil ? .cancelAction : nil)
                    }
                } else {
                    Text(scope.title).font(Theme.ui(13, .semibold)).foregroundStyle(Theme.text)
                    Text(verbatim: "\(rows.count)").font(Theme.ui(12)).foregroundStyle(Theme.text3)
                    if let filter {
                        FilterChip(name: filter.name(in: machines)) { self.filter = nil }
                    }
                    Spacer()
                    if !selectable.isEmpty {
                        ActionButton(title: "Select", key: nil, prominent: false, id: "fleet.select") { startSelection() }
                    }
                    GroupingSwitch(selection: $grouping)
                }
            }
            .padding(.horizontal, 16)
            .padding(.leading, max(0, headerClearance.leading - 16))
            .padding(.trailing, max(0, headerClearance.trailing - 16))
            .frame(height: 52)
            Rectangle().fill(Theme.hairline).frame(height: 1)
            // What the last batch did not remove, and why, under the header.
            if let note = batchNote(rows) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(note.title).font(Theme.ui(12, .medium)).foregroundStyle(Theme.amber)
                    Text(note.reasons).font(Theme.ui(12)).foregroundStyle(Theme.text2).lineLimit(2)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("fleet.batchNote")
                Rectangle().fill(Theme.hairline).frame(height: 1)
            }

            // A computer whose link is not working shows why in place of its list, and the one thing to do: rows it
            // listed before are the last it said, and none can be acted on until it is reached again.
            if rows.isEmpty || shownComputerNotice != nil {
                Group {
                    if let (computer, notice) = shownComputerNotice {
                        ComputerNotice(notice: notice, act: noticeAction(computer, notice))
                    } else if scope == .active {
                        NoAgentsYet(newAgent: newAgent)
                    } else {
                        Text(FleetWords.noEndedRuns).font(Theme.ui(12.5)).foregroundStyle(Theme.text3)
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
                            // The list is one stop for the keyboard; its rows are not stops of their own.
                            .focusable(false)
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
                                    let more = hovered == row.id || menu.shown?.owner == "fleet.row.\(row.id).more"
                                        || HoverButtonStyle.forced("fleet.row.\(row.id)")
                                    let choosable = selectable.contains(row.id)
                                    Button {
                                        if selecting || (choosable && NSEvent.modifierFlags.contains(.command)) {
                                            // Choosing: a click chooses or lets go; ⌘-click starts choosing with it.
                                            guard choosable, !batchRunning else { return }
                                            if !selecting { startSelection() }
                                            if chosen.contains(row.id) { chosen.remove(row.id) } else { chosen.insert(row.id) }
                                            return
                                        }
                                        opened = selected ? nil : row.id
                                        keepListFocus()
                                    } label: {
                                        FleetLine(
                                            row: row, selected: selected, roomForMore: more && !selecting,
                                            chosen: selecting && choosable ? chosen.contains(row.id) : nil,
                                            refusal: batchRefused[row.id])
                                    }
                                    .buttonStyle(.hover(radius: 0, id: "fleet.row.\(row.id)"))
                                    .focusable(false)
                                    .accessibilityElement(children: .contain)
                                    .accessibilityAction { opened = selected ? nil : row.id }
                                    .accessibilityIdentifier("fleet.row.\(row.id)")
                                    .accessibilityAddTraits(selected ? .isSelected : [])
                                    // The run's "…" (decisions 185 and 217): on hover, its panel holding Copy
                                    // Working Directory, then End Run… while the run is live, or Remove… once it has
                                    // ended (an adopted run's at any time); for VoiceOver, which does not hover, those
                                    // are the row's own actions.
                                    .accessibilityActions {
                                        Button("Copy Working Directory") { copyToPasteboard(row.summary.cwd) }
                                            .accessibilityIdentifier("fleet.row.\(row.id).copyWorkingDirectoryAction")
                                        if endable {
                                            Button("End Run…") { ending = row }
                                                .accessibilityIdentifier("fleet.row.\(row.id).endRunAction")
                                        }
                                        if removable {
                                            Button("Remove…") { removing = row }
                                                .accessibilityIdentifier("fleet.row.\(row.id).removeAction")
                                        }
                                    }
                                    .overlay(alignment: .trailing) {
                                        moreButton(row, endable: endable, removable: removable, shown: more && !selecting)
                                    }
                                    // The same menu, at the pointer; none while choosing.
                                    .onRightClick { point in
                                        guard !selecting else { return }
                                        RunMore.show(
                                            menu, at: CGRect(origin: point, size: .zero), id: "fleet.row.\(row.id).more",
                                            action: endable ? .endRun : removable ? .remove : nil, copies: row.summary.cwd
                                        ) { if endable { ending = row } else { removing = row } }
                                    }
                                    .onHover { hovered = $0 ? row.id : (hovered == row.id ? nil : hovered) }
                                    .id(group.id + "/" + row.id)
                                }
                            }
                        }
                        let finished = narrowed(FleetScope.finished.rows(machines.fleet)).count
                        if scope == .active, finished > 0 {
                            Button { scope = .finished } label: {
                                HStack(spacing: 8) {
                                    Image(systemName: "chevron.right").font(.system(size: 8.5, weight: .bold))
                                    Text(FleetScope.finished.title).font(Theme.ui(12, .medium))
                                    Text(verbatim: "\(finished)").font(Theme.ui(12)).foregroundStyle(Theme.text3)
                                    Spacer()
                                }
                                .foregroundStyle(Theme.text2)
                                .padding(.horizontal, 16)
                                .frame(height: 34)
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.hover(radius: 0, id: "fleet.showFinished"))
                            .focusable(false)
                            .accessibilityIdentifier("fleet.showFinished")
                        }
                    }
                    // Room below the last row.
                    .padding(.bottom, 40)
                }
                #if DEBUG
                    .defaultScrollAnchor(RenderWindow.listAtEnd ? .bottom : nil)
                #endif
                // One stop for the keyboard: ↑ and ↓ move the open run through the runs as the list shows them, a
                // folded group's skipped, and the list keeps the keyboard as the run opens beside it.
                .focusable()
                .focused($listFocused)
                .focusEffectDisabled()
                .onMoveCommand { direction in
                    guard direction == .up || direction == .down else { return }
                    let shown = FleetGroup.groups(rows, by: grouping).filter { !folded.contains($0.id) }
                        .flatMap { $0.rows.map(\.id) }
                    opened = ListStep.step(from: opened, in: shown, down: direction == .down)
                    keepListFocus()
                }
            }

            // The list's foot, only while choosing runs: what is chosen is removed from it.
            if selecting {
                Rectangle().fill(Theme.hairline).frame(height: 1)
                HStack(spacing: 14) {
                    Spacer()
                    ActionButton(
                        title: batchRunning ? "Removing…" : "Remove \(picked.count)…", key: nil, prominent: false,
                        destructive: true, id: "fleet.removeChosen"
                    ) { removingMany = picked }
                    .disabled(picked.isEmpty || batchRunning)
                }
                .padding(.horizontal, 16)
                .frame(height: 34)
            }
        }
        .background(Theme.window)
        // Leaving the Ended runs leaves the choosing, and so does no removable run being left at all. A run only
        // folded away or filtered out is still there: it stays chosen and is picked again once shown.
        .onChange(of: scope) { endSelection() }
        .onChange(of: scope.rows(machines.fleet).contains(where: removable)) { _, any in if !any { endSelection() } }
        #if DEBUG
            .task(id: rows.first?.id) {
                guard let row = rows.first else { return }
                switch RenderWindow.confirm {
                case "endRun": ending = row
                case "endNote": endNote = "It is still starting; end it once it has started."
                case "remove": removing = row
                case "removeNote": removeNote = FleetWords.removalRefused(.stillRunning)
                default: break
                }
                if let count = RenderWindow.select {
                    let selectable = selectableKeys(rows)
                    startSelection()
                    chosen = Set(selectable.prefix(count ?? selectable.count))
                    switch RenderWindow.confirm {
                    case "batch": removingMany = selectable.filter(chosen.contains)
                    case "batchRun": removeChosen(selectable.filter(chosen.contains))
                    default: break
                    }
                }
            }
        #endif
        // Ending a run cannot be undone: it asks once (the approved words), then says only what went wrong.
        .confirmation($ending) { row in
            Confirmation(
                FleetWords.endAsk(row.label.project, agent: row.summary.agent.displayName), cancelID: "endRun.cancel",
                confirmID: "endRun.confirm"
            ) { endRun(row) }
        }
        .confirmation($endNote) { note in
            Confirmation(
                title: FleetWords.endFailedTitle, message: note, answers: [.init(title: "OK", kind: .ok, id: "endRun.ok")])
        }
        // Removing forgets CodeConnect's own record of a run, never the agent's transcript: it asks once, in the
        // iPhone's words, then says only what went wrong.
        .confirmation($removing) { row in
            Confirmation(
                title: FleetWords.removeTitle(row.label.project),
                message: FleetWords.removeKeeps(row.summary.agent.displayName),
                answers: [
                    .init(title: "Cancel", kind: .cancel, id: "remove.cancel"),
                    .init(title: "Remove", kind: .destructive, id: "remove.confirm") { remove(row) },
                ])
        }
        // One ask for the whole choice: for one run, its own words; for several, the iPhone's.
        .confirmation($removingMany) { keys in
            let one = keys.count == 1 ? rows.first { $0.id == keys[0] } : nil
            return Confirmation(
                title: one.map { FleetWords.removeTitle($0.label.project) } ?? FleetWords.removeManyTitle(keys.count),
                message: one.map { FleetWords.removeKeeps($0.summary.agent.displayName) } ?? FleetWords.removeManyKeeps,
                answers: [
                    .init(title: "Cancel", kind: .cancel, id: "removeMany.cancel"),
                    .init(title: "Remove", kind: .destructive, id: "removeMany.confirm") { removeChosen(keys) },
                ])
        }
        .confirmation($removeNote) { note in
            Confirmation(
                title: FleetWords.removeFailedTitle, message: note, answers: [.init(title: "OK", kind: .ok, id: "remove.ok")])
        }
    }

    /// Gives the list the keyboard, and gives it again once the window has laid out the run that opened: the new pane
    /// beside the list otherwise hands the keyboard to the first control left in the window.
    private func keepListFocus() {
        listFocused = true
        DispatchQueue.main.async { listFocused = true }
    }

    /// The row's "…" while it is shown: on hover, or while its panel is open.
    @ViewBuilder private func moreButton(_ row: FleetRow, endable: Bool, removable: Bool, shown: Bool) -> some View {
        if shown {
            RunMore(
                id: "fleet.row.\(row.id).more", action: endable ? .endRun : removable ? .remove : nil,
                copies: row.summary.cwd
            ) {
                if endable { ending = row } else { removing = row }
            }
            .padding(.trailing, 16)
        }
    }

    private func endRun(_ row: FleetRow) {
        guard let model = machines.model(for: row.machineID) else { return }
        Task { endNote = await model.endRun(row.summary) }
    }

    private func remove(_ row: FleetRow) {
        guard let model = machines.model(for: row.machineID) else { return }
        Task { removeNote = FleetWords.removalRefused(await model.removeSession(row.summary)) }
    }

    /// The runs a choice can hold: the Ended runs the list shows that can be removed (`FleetGroup.choosable`).
    private func selectableKeys(_ rows: [FleetRow]) -> [String] {
        guard scope == .finished else { return [] }
        return FleetGroup.choosable(rows, by: grouping, folded: folded, removable: removable)
    }

    /// Ended, on a computer that removes runs.
    private func removable(_ row: FleetRow) -> Bool {
        row.status == .ended && row.summary.isRemovable
            && machines.model(for: row.machineID)?.daemonProfile.removesSessions == true
    }

    private func startSelection() {
        menu.close()
        batchRefused = [:]
        selecting = true
    }

    private func endSelection() {
        selecting = false
        chosen = []
        batchRefused = [:]
    }

    /// Removes each chosen run in turn; whatever was not removed stays chosen, with its reason on its row.
    private func removeChosen(_ keys: [String]) {
        batchRunning = true
        batchRefused = [:]
        Task {
            let refused = await machines.removeSessions(keys: keys)
            batchRunning = false
            batchRefused = refused
            if selecting { chosen = Set(refused.keys) }
            if let note = batchNote(scope.rows(machines.fleet)) {
                NSAccessibility.post(
                    element: NSApp.mainWindow as Any, notification: .announcementRequested,
                    userInfo: [.announcement: "\(note.title). \(note.reasons)", .priority: NSAccessibilityPriorityLevel.high])
            }
        }
    }

    /// What the last batch did not remove, among these rows, as the iPhone says it.
    private func batchNote(_ rows: [FleetRow]) -> (title: String, reasons: String)? {
        let kept = rows.compactMap { batchRefused[$0.id] }
        guard !kept.isEmpty else { return nil }
        return ("\(kept.count) not removed", Set(kept).sorted().joined(separator: " · "))
    }

    /// The computer the list is narrowed to, when its link is not working, and what it says: the sentence behind the
    /// sidebar's word.
    private var shownComputerNotice: (AppModel, LinkNotice)? {
        guard case .computer(let id) = filter,
            let model = machines.models.first(where: { ($0.machineID ?? "") == id }),
            let notice = model.linkHealth.notice
        else { return nil }
        return (model, notice)
    }

    /// The notice's one action on the Mac, as the computer's sidebar row offers it (`LinkNotice.macAction`): Try
    /// Again where the link can be asked again, Open Tailscale where Tailscale is off and its app is here, Pair Again
    /// where the pairing or its address has to change.
    private func noticeAction(_ computer: AppModel, _ notice: LinkNotice) -> (title: String, run: () -> Void)? {
        let app = TailscaleApp.url
        switch LinkNotice.macAction(notice.action, tailscaleFound: app != nil) {
        case .tryAgain: return (PeerWords.tryAgain, { computer.connection.retryNow() })
        case .openTailscale:
            guard let app else { return nil }
            return (LinkNotice.openTailscale, { NSWorkspace.shared.open(app) })
        case .pairAgain:
            guard let address = computer.endpoint?.typedAddress else { return nil }
            return (LinkHealth.pairAgain, { pairAgain(address) })
        case nil: return nil
        }
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
    @Environment(Machines.self) private var machines
    let group: FleetGroup
    let grouping: FleetGroup.Kind
    let namingComputer: Bool
    let folded: Bool

    /// A computer's link when it is not working, in the word the sidebar and the iPhone's cards say beside its name.
    private var linkWord: String? {
        guard grouping == .computer else { return nil }
        return machines.models.first { ($0.machineID ?? "") == group.id }?.linkHealth.machineWord
    }

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
                .truncationMode(.middle)
            // The counts stay whole on one line; the name before them is what gives way, in its middle.
            Text(verbatim: "\(group.rows.count)").font(Theme.ui(11.5)).foregroundStyle(Theme.text3).fixedSize()
            if let linkWord {
                Text(linkWord).font(Theme.ui(11.5)).foregroundStyle(Theme.text3).lineLimit(1).fixedSize()
            }
            if group.waiting > 0 {
                Text(FleetCount.waitingForYou(group.waiting)).font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.amber)
                    .lineLimit(1)
                    .fixedSize()
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
/// place, each is cut once, where it keeps its meaning: the computer at its end, and the folder to its
/// last part after "…/", whole, so a cut never lands inside a folder's name.
struct ProjectPlace: View {
    let project: FleetGroup
    let namingComputer: Bool
    let size: CGFloat

    var body: some View {
        HStack(spacing: 4) {
            if namingComputer {
                // First in line for room: the folder takes what is left, down to "…/" and its last part cut.
                Text(project.computer).font(Theme.ui(size)).lineLimit(1).truncationMode(.tail).layoutPriority(1)
                Text(verbatim: "·").font(Theme.ui(size))
            }
            ViewThatFits(in: .horizontal) {
                Text(project.folder)
                Text(verbatim: "…/" + (project.folder.split(separator: "/").last.map(String.init) ?? project.folder))
                    .truncationMode(.middle)
            }
            .font(Theme.mono(size))
            .lineLimit(1)
        }
        .foregroundStyle(Theme.text3)
    }
}

/// A run's command line cut to the room the row leaves it, by the Kit's rule (`commandPreview`): its start and its
/// end, never a tail cut that hides what it runs on. It takes the row's room up to the agent's mark, so the width it
/// measures is that room and not its own text's; until measured, the whole command, held to one line. Spoken in full.
private struct CommandPreview: View {
    let command: String
    @State private var width: CGFloat = 0

    /// One character's width in the line's font: a monospaced font has one.
    private static let advance = max(("0" as NSString).size(withAttributes: [.font: Theme.monoFont(11.5)]).width, 1)

    /// Half a point of slack, so a rounding difference never pushes the last character past the edge.
    private var columns: Int { width > 0 ? Int(((width - 0.5) / Self.advance).rounded(.down)) : 0 }

    var body: some View {
        Text(columns > 0 ? commandPreview(command, columns: columns) : command)
            .font(Theme.mono(11.5))
            .foregroundStyle(Theme.text3)
            .lineLimit(1)
            .frame(maxWidth: .infinity, alignment: .leading)
            .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { width = $0 }
            .accessibilityLabel(command)
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
    /// While choosing runs to remove: whether this one is chosen; its box takes the status mark's place.
    var chosen: Bool? = nil
    /// Why the last batch did not remove it, in place of what it last did.
    var refusal: String? = nil
    @State private var lastTick = Date()

    private var since: Date { row.lastActive }

    var body: some View {
        HStack(spacing: 10) {
            if let chosen {
                ChooseBox(on: chosen).accessibilityIdentifier("fleet.row.\(row.id).chosen")
            } else {
                StateIcon(status: row.status)
                    .accessibilityIdentifier("fleet.row.\(row.id).status")
            }
            Text(row.label.inline)
                .font(Theme.ui(12.5, row.status == .blocked ? .medium : .regular))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
                .truncationMode(.middle)
                .layoutPriority(1)
            // What it is on: the tool call in prose and its argument as code, or else the
            // daemon's own sentence; asked to end, that it is stopping, until it has.
            if let refusal {
                Text(refusal).font(Theme.ui(12)).foregroundStyle(Theme.amber).lineLimit(1).modifier(DropsWhenNarrow())
            } else if row.status == .stopping {
                Text("Stopping…").font(Theme.ui(12)).foregroundStyle(Theme.text3).lineLimit(1).modifier(DropsWhenNarrow())
            } else if let activity = row.activity {
                Text(activity.tool).font(Theme.ui(12)).foregroundStyle(Theme.text2).lineLimit(1).modifier(DropsWhenNarrow())
                    // Beside a command, which fills the row, the tool's name keeps its own width.
                    .layoutPriority(isCommandTool(activity.tool) ? 0.5 : 0)
                if let argument = activity.argument, !argument.isEmpty {
                    if isCommandTool(activity.tool) {
                        CommandPreview(command: argument).modifier(DropsWhenNarrow()).layoutPriority(0.5)
                    } else {
                        Text(argument).font(Theme.mono(11.5)).foregroundStyle(Theme.text3).lineLimit(1).modifier(DropsWhenNarrow())
                    }
                }
            } else if !row.subtitle.isEmpty {
                Text(row.subtitle).font(Theme.ui(12)).foregroundStyle(Theme.text3).lineLimit(1).modifier(DropsWhenNarrow())
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

/// A row's "…" (a run's, or a computer's in the sidebar): it opens the window's row menu (`RowMenu`) below itself,
/// holding End Run… or Remove… (`PanelItem`, destructive), each of which asks before anything is sent; a run's menu
/// first offers Copy Working Directory, its folder as the computer names it, as the iPhone's run offers it, and is
/// there for every run, so a run that can be neither ended nor removed from here has that item alone. A right-click
/// on the row opens the same menu at the pointer (`RunMore.show`).
struct RunMore: View {
    enum Action {
        case endRun, remove

        var title: String { self == .endRun ? "End Run…" : "Remove…" }
        var id: String { self == .endRun ? "endRun" : "remove" }
        var symbol: String { self == .endRun ? PanelSymbol.endRun : PanelSymbol.remove }
    }

    static let width: CGFloat = 26
    let id: String
    /// What the menu does to the row's subject after any copy: none for a run that can be neither ended nor removed
    /// from here (its computer's link is down, or its ccd cannot stop it), whose menu only copies its folder.
    let action: Action?
    /// A run's folder, which the menu's first item copies; none for a computer.
    var copies: String? = nil
    let perform: () -> Void
    #if DEBUG
        /// A render's `-CC_RUN_MENU <uid>` names this run's "…", or `-CC_COMPUTER_MENU` a computer's: once shown, it
        /// opens a moment later, as a click opens it.
        private var opensAtLaunch: Bool {
            RenderWindow.openRunMenu.map { id == "fleet.row.\($0).more" } == true
                || (RenderWindow.opensComputerMenu && id.hasPrefix("sidebar.computer."))
        }
    #endif
    @Environment(RowMenu.self) private var menu
    @State private var frame: CGRect = .zero

    private var open: Bool { menu.shown?.owner == id }

    var body: some View {
        Button { Self.show(menu, at: frame, id: id, action: action, copies: copies, perform: perform) } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Theme.text2)
                .frame(width: Self.width, height: 22)
                .background(open ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
                .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: id))
        // Inside a list that is one stop for the keyboard: reached by the pointer and by VoiceOver's action.
        .focusable(false)
        .help("More")
        .accessibilityLabel("More")
        .accessibilityIdentifier(id)
        .accessibilityValue(open ? "open" : "closed")
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { frame = $0 }
        #if DEBUG
            .task {
                guard opensAtLaunch else { return }
                // After the render has sized and placed its window, as a click comes on a window at rest.
                try? await Task.sleep(for: .seconds(1))
                Self.show(menu, at: frame, id: id, action: action, copies: copies, perform: perform)
            }
        #endif
    }

    /// The row's menu by `rect` (its "…", or a right-click's point as a zero-size rect): Copy Working Directory when
    /// it `copies` a folder, then the act on the row's own subject through `perform`.
    @MainActor static func show(
        _ menu: RowMenu, at rect: CGRect, id: String, action: Action?, copies: String? = nil,
        perform: @escaping () -> Void
    ) {
        let copy = copies.map { folder in
            RowMenu.Item(id: "\(id).copyWorkingDirectory", title: "Copy Working Directory", symbol: PanelSymbol.copy,
                         destructive: false) { copyToPasteboard(folder) }
        }
        let act = action.map { action in
            RowMenu.Item(id: "\(id).\(action.id)", title: action.title, symbol: action.symbol, destructive: true,
                         perform: perform)
        }
        menu.toggle(rect, owner: id, items: [copy, act].compactMap { $0 })
    }

}

/// A row's words given less room than a short word shows nothing, never a sliver of a letter beside the name. Only
/// words cut to that room: a short word shown whole ("Edit") is shown.
private struct DropsWhenNarrow: ViewModifier {
    @State private var width: CGFloat = .infinity
    /// The words' own width, uncut.
    @State private var whole: CGFloat = 0

    func body(content: Content) -> some View {
        content
            .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { width = $0 }
            .background {
                content.fixedSize().hidden()
                    .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { whole = $0 }
            }
            .opacity(width < 28 && width < whole - 0.5 ? 0 : 1)
    }
}

/// A run's state as the fleet orders it (`FleetStatus`), drawn small: amber when it waits on you.
/// A run's box while choosing runs to remove, in the status mark's 13-point place.
private struct ChooseBox: View {
    let on: Bool

    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: 3.5).fill(on ? Theme.text : .clear)
            RoundedRectangle(cornerRadius: 3.5).strokeBorder(on ? Theme.text : Theme.text3, lineWidth: 1.5)
            if on {
                Image(systemName: "checkmark").font(.system(size: 7.5, weight: .heavy)).foregroundStyle(Theme.window)
            }
        }
        .frame(width: 13, height: 13)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(on ? "Chosen" : "Not chosen")
    }
}

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

/// Which agent a run is: Claude's asterisk, Codex's brackets, Antigravity's sparkle (the owner's pick), or the mark of an
/// agent this build does not know, each a grey system symbol in the same circle, named by its product on hover.
struct AgentMark: View {
    let agent: AgentKind

    var body: some View {
        Group {
            switch agent {
            case .claude: Image(systemName: "asterisk").font(.system(size: 9, weight: .bold))
            case .codex: Image(systemName: "chevron.left.forwardslash.chevron.right").font(.system(size: 7.5, weight: .bold))
            case .antigravity: Image(systemName: "sparkle").font(.system(size: 8.5, weight: .bold))
            case .unsupported: Image(systemName: "questionmark").font(.system(size: 8, weight: .bold))
            }
        }
        .foregroundStyle(Theme.text2)
        .frame(width: 18, height: 18)
        .background(Circle().fill(Color.white.opacity(0.08)))
        .help(agent.productName)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(agent.productName)
    }

}
