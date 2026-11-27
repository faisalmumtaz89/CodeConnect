import CodeConnectKit
import SwiftUI

/// How the person has laid the list out: only what needs them, the projects folded, how many times each project's
/// ended sessions were revealed, and where the open session stood when it was opened, which it keeps while it is
/// open. The app's, so a hidden list or a closed window comes back to it.
struct ListLayout: Equatable {
    var onlyNeedsYou = false
    var folded: Set<String> = []
    var revealed: [String: Int] = [:]
    var held: ProjectList.Held?
}

/// The window's session list, 256 pt down its leading side: in its head the computer it is narrowed to ("All ▾") and
/// New Session (+); under it, while anything needs the person on any computer, "N need you" with Only these; then every
/// session on every paired computer under its project (`ProjectList`), each project under a header that folds it and
/// sticks while its sessions scroll under it, its ended sessions revealed in place ("N earlier ›"); in its foot this
/// Mac's Tailscale and Settings.
struct FleetList: View {
    @Environment(Machines.self) private var machines
    @Binding var layout: ListLayout
    /// The computer the list is narrowed to, if any; the Mac narrows by computer alone.
    @Binding var filter: FleetFilter?
    /// The session open beside the list, if any.
    @Binding var opened: String?
    let newRun: () -> Void
    let addComputer: () -> Void
    let openSettings: () -> Void
    /// The row under the pointer, and the row whose "…" panel is open.
    @State private var hovered: String?
    /// The window's one row menu, which a session's "…" or a right-click on its row opens.
    @Environment(RowMenu.self) private var menu
    @Environment(\.headerClearance) private var headerClearance
    /// The list has the keyboard: one stop, taken by a click on a row and kept across an arrow's move.
    @FocusState private var listFocused: Bool
    /// The session End Session… is asking about, and what its computer said when it did not end.
    @State private var ending: FleetRow?
    @State private var endNote: String?
    /// The session Remove… is asking about, and what its computer said when it was not removed.
    @State private var removing: FleetRow?
    @State private var removeNote: String?
    /// Choosing ended sessions to remove together (⌘-click on one): what is chosen, the batch asked about, whether it
    /// is on its way, and what the last batch did not remove, by session, with the computer's reason.
    @State private var selecting = false
    @State private var chosen: Set<String> = []
    @State private var removingMany: [String]?
    @State private var batchRunning = false
    @State private var batchRefused: [String: String] = [:]

    /// The machine id the list is narrowed to.
    private var computer: String? {
        if case .computer(let id) = filter { return id }
        return nil
    }

    /// The list as it stands, the open session held at `held`.
    private func projects(held: ProjectList.Held?) -> ProjectList {
        ProjectList(
            machines.fleet, computer: computer, onlyNeedsYou: layout.onlyNeedsYou, revealed: layout.revealed,
            deck: machines.deck.map(\.sessionKey), held: held)
    }

    var body: some View {
        let list = projects(held: layout.held)
        // The sessions as the list shows them, for the arrows; a folded project's are not shown.
        let rows = list.projects.filter { !layout.folded.contains($0.id) }.flatMap(\.rows)
        let selectable = rows.filter(removable).map(\.id)
        let picked = selectable.filter(chosen.contains)
        VStack(spacing: 0) {
            // The head's controls stand at the end of the list's 256 pt, whatever the list's width, so the computer menu
            // opens from the same place, over whatever is beside the list.
            HStack(spacing: 4) {
                Spacer(minLength: 0)
                ComputerFilter(filter: $filter, addComputer: addComputer)
                Button(action: newRun) {
                    Image(systemName: "plus").font(.system(size: 13)).foregroundStyle(Theme.text2)
                        .frame(width: 20, height: 24)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.hover(radius: 6, id: "fleet.newRun"))
                .focusable(false)
                .help("New Session")
                .accessibilityLabel("New Session")
                .accessibilityIdentifier("fleet.newRun")
            }
            // Clear of the list's toggle beside the window's buttons when the list comes first.
            .padding(.leading, max(16, headerClearance.leading - 16))
            .padding(.trailing, 12)
            .frame(width: 256, height: 52)
            .frame(maxWidth: .infinity, alignment: .leading)
            // Its dropdown opens over the rows below it.
            .zIndex(1)
            // What the last batch did not remove, and why, under the head.
            if let note = batchNote(rows) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(note.title).font(Theme.ui(12, .medium)).foregroundStyle(Theme.amber)
                    Text(note.reasons).font(Theme.ui(12)).foregroundStyle(Theme.text2).lineLimit(2)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 18)
                .padding(.vertical, 8)
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("fleet.batchNote")
            }

            if machines.fleet.allSatisfy(\.summary.archived) {
                NoAgentsYet(newAgent: newRun)
                    .padding(18)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            } else {
                if list.needsYou > 0 { needsYouRow(list) }
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0, pinnedViews: [.sectionHeaders]) {
                        ForEach(list.projects) { project in
                            let folded = layout.folded.contains(project.id)
                            Section {
                                if !folded {
                                    ForEach(project.rows) { row in
                                        // Narrowed, a session on another computer (one that needs you, or the open
                                        // one) names that computer by its mark.
                                        runRow(
                                            row, elsewhere: computer != nil && row.machineID != computer,
                                            choosable: selectable.contains(row.id))
                                    }
                                    earlier(project)
                                }
                                // 8 pt above the next header.
                                Color.clear.frame(height: 8)
                            } header: {
                                ProjectHeader(project: project, folded: folded) {
                                    if folded { layout.folded.remove(project.id) } else { layout.folded.insert(project.id) }
                                }
                            }
                        }
                        if list.onlyNeedsYou, !list.quiet.isEmpty {
                            Text(RunListWords.quiet(list.quiet)).font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                                .padding(.horizontal, 18)
                                .padding(.top, 10)
                                .accessibilityIdentifier("fleet.quiet")
                        }
                        if list.projects.isEmpty, let narrowedName {
                            Text(RunListWords.noneOn(narrowedName)).font(Theme.ui(12.5)).foregroundStyle(Theme.text3)
                                .padding(.horizontal, 18)
                                .frame(height: 30)
                                .accessibilityIdentifier("fleet.none")
                        }
                    }
                    // 8 pt above the first header; room below the last row.
                    .padding(.top, 8)
                    .padding(.bottom, 24)
                }
                #if DEBUG
                    .defaultScrollAnchor(RenderWindow.listAtEnd ? .bottom : nil)
                #endif
                // One stop for the keyboard: ↑ and ↓ move the open session through the sessions as the list shows
                // them, and the list keeps the keyboard as the session opens beside it.
                .focusable()
                .focused($listFocused)
                .focusEffectDisabled()
                .onMoveCommand { direction in
                    guard direction == .up || direction == .down else { return }
                    opened = ListStep.step(from: opened, in: rows.map(\.id), down: direction == .down)
                    keepListFocus()
                }
            }

            // While choosing: how many are chosen and Done; every removable session at once, and what is chosen
            // removed together.
            if selecting {
                HStack(spacing: 8) {
                    Text("\(picked.count) selected").font(Theme.ui(12, .medium)).foregroundStyle(Theme.text3)
                        .accessibilityIdentifier("fleet.selected")
                    Spacer(minLength: 0)
                    // No Done while a batch runs: the batch holds the choice until it has answered. While the card
                    // asks, Esc is the card's.
                    if !batchRunning {
                        quietButton("Done", id: "fleet.selectDone") { endSelection() }
                            .keyboardShortcut(removingMany == nil ? .cancelAction : nil)
                    }
                }
                .padding(.leading, 18)
                .padding(.trailing, 14)
                .frame(height: 30)
                HStack(spacing: 8) {
                    let all = !selectable.isEmpty && chosen.isSuperset(of: selectable)
                    ActionButton(title: all ? "Deselect All" : "Select All", key: nil, prominent: false, id: "fleet.selectAll") {
                        chosen = all ? [] : Set(selectable)
                    }
                    .keyboardShortcut("a", modifiers: .command)
                    .disabled(batchRunning)
                    Spacer(minLength: 0)
                    ActionButton(
                        title: batchRunning ? "Removing…" : "Remove \(picked.count)…", key: nil, prominent: false,
                        destructive: true, id: "fleet.removeChosen"
                    ) { removingMany = picked }
                    .disabled(picked.isEmpty || batchRunning)
                }
                .padding(.horizontal, 12)
                .frame(height: 40)
            }
            ListFoot(openSettings: openSettings)
        }
        .background(Theme.sidebar)
        // The open session holds its place while it is open, its project unfolded, from when the list has it: a
        // session just started is open before its row arrives.
        .onChange(of: opened.flatMap { key in machines.fleet.contains { $0.id == key } ? key : nil }, initial: true) {
            _, key in hold(key)
        }
        // No removable session shown, no choosing.
        .onChange(of: selectable.isEmpty) { _, none in if none { endSelection() } }
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
                    startSelection()
                    chosen = Set(selectable.prefix(count ?? selectable.count))
                    switch RenderWindow.confirm {
                    case "batch": removingMany = selectable.filter(chosen.contains)
                    case "batchRun": removeChosen(selectable.filter(chosen.contains))
                    default: break
                    }
                }
            }
            // `-CC_FOLD` and `-CC_REVEAL`, once the projects they name are listed.
            .task(id: list.projects.map(\.name)) {
                for project in list.projects {
                    if RenderWindow.folds.contains(project.name) { layout.folded.insert(project.id) }
                    if let reveals = RenderWindow.reveals[project.name] { layout.revealed[project.id] = reveals }
                }
            }
        #endif
        // Ending a session cannot be undone: it asks once (the approved words), then says only what went wrong.
        .confirmation($ending) { row in
            Confirmation(
                FleetWords.endAsk(row.title, agent: row.summary.agent.displayName), cancelID: "endRun.cancel",
                confirmID: "endRun.confirm"
            ) { endRun(row) }
        }
        .confirmation($endNote) { note in
            Confirmation(
                title: FleetWords.endFailedTitle, message: note, answers: [.init(title: "OK", kind: .ok, id: "endRun.ok")])
        }
        // Removing forgets CodeConnect's own record of a session, never the agent's transcript: it asks once, in the
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
        // One ask for the whole choice: for one session, its own words; for several, the iPhone's.
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

    /// "N need you", over the projects while anything needs the person on any computer: a click keeps only those
    /// (Only these), the row lit amber and saying Show All; narrowed to a computer, it says it counts all of them.
    private func needsYouRow(_ list: ProjectList) -> some View {
        Button { layout.onlyNeedsYou.toggle() } label: {
            HStack(spacing: 9) {
                StateIcon(status: .blocked)
                Text(RunListWords.needYou(list.needsYou)).font(Theme.ui(12.5, .medium)).foregroundStyle(Theme.text)
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Text(
                    list.onlyNeedsYou
                        ? RunListWords.showAll : computer != nil ? RunListWords.allComputers : RunListWords.onlyThese
                )
                .font(Theme.ui(11.5)).foregroundStyle(Theme.text3).fixedSize()
            }
            .padding(.horizontal, 10)
            .frame(height: 30)
            .background(list.onlyNeedsYou ? Theme.amber.opacity(0.14) : .clear, in: RoundedRectangle(cornerRadius: 6))
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 6, id: "fleet.needsYou"))
        .focusable(false)
        .padding(.horizontal, 8)
        .padding(.bottom, 2)
        .accessibilityIdentifier("fleet.needsYou")
        .accessibilityAddTraits(list.onlyNeedsYou ? .isSelected : [])
    }

    /// Under a project's sessions: its ended ones not yet shown ("N earlier ›", 25 more a click), and once any were
    /// revealed, Show fewer, which folds them back.
    @ViewBuilder private func earlier(_ project: ProjectList.Project) -> some View {
        let revealed = layout.revealed[project.id] ?? 0
        // Show fewer while it has something to fold: an ended session revealed in the list.
        let fewer = revealed > 0 && project.rows.contains { $0.status == .ended && !$0.summary.atLimit }
        if project.earlier > 0 || fewer {
            HStack(spacing: 12) {
                if project.earlier > 0 {
                    quietButton(RunListWords.earlier(project.earlier), id: "fleet.project.\(project.name).earlier") {
                        layout.revealed[project.id] = revealed + 1
                    }
                }
                if fewer {
                    quietButton(RunListWords.showFewer, id: "fleet.project.\(project.name).fewer") {
                        layout.revealed[project.id] = nil
                    }
                }
            }
            .padding(.leading, 33)
            .padding(.horizontal, 8)
            .frame(height: 26)
            .padding(.bottom, 2)
        }
    }

    /// A word that acts, quiet as the list's notes.
    private func quietButton(_ title: String, id: String, action: @escaping () -> Void) -> some View {
        Button(action: action) { Text(title).font(Theme.ui(11.5)).foregroundStyle(Theme.text3) }
            .buttonStyle(.hover(radius: 5, id: id))
            .focusable(false)
            .accessibilityIdentifier(id)
    }

    /// The chosen computer's name, while the list is narrowed to one.
    private var narrowedName: String? {
        computer.flatMap { id in machines.models.first { $0.machineID == id }?.machineName }
    }

    /// Holds the session just opened where the list draws it now, and unfolds its project; none open, none held. The
    /// session already held stays where it is held.
    private func hold(_ key: String?) {
        guard layout.held?.session != key else { return }
        let drawn = projects(held: layout.held)
        layout.held = nil
        guard let key else { return }
        for (index, project) in drawn.projects.enumerated() {
            guard let at = project.rows.firstIndex(where: { $0.id == key }) else { continue }
            layout.held = .init(session: key, projectIndex: index, rowIndex: at, ended: project.rows[at].status == .ended)
            layout.folded.remove(project.id)
            return
        }
    }

    /// One run's row: a click opens it (or, while choosing, chooses it), its "…" on hover or a right-click holds what
    /// can be done to it.
    private func runRow(_ row: FleetRow, elsewhere: Bool, choosable: Bool) -> some View {
        let selected = opened == row.id
        let model = machines.model(for: row.machineID)
        let endable = model?.canEndRun(row.summary) == true
        // The iPhone's rule for its Remove: ended (or adopted), on a computer whose ccd removes runs. Never with End
        // run: a run CodeConnect started is removable only once it has ended.
        let removable = row.summary.isRemovable && model?.daemonProfile.removesSessions == true
        // Its computer not reached: its mark dims to its stopping form, its section and age as they were.
        let offline = model?.linkHealth.level == .offline
        let more = hovered == row.id || menu.shown?.owner == "fleet.row.\(row.id).more"
            || HoverButtonStyle.forced("fleet.row.\(row.id)")
        let activate = {
            if selecting || (choosable && NSEvent.modifierFlags.contains(.command)) {
                // Choosing: a click chooses or lets go; ⌘-click starts choosing with it.
                guard choosable, !batchRunning else { return }
                if !selecting { startSelection() }
                if chosen.contains(row.id) { chosen.remove(row.id) } else { chosen.insert(row.id) }
                return
            }
            opened = selected ? nil : row.id
            keepListFocus()
        }
        return Button(action: activate) {
            FleetLine(
                row: row, selected: selected, roomForMore: more && !selecting,
                elsewhere: elsewhere ? model?.machineOS ?? "" : nil, offline: offline,
                chosen: selecting && choosable ? chosen.contains(row.id) : nil,
                refusal: batchRefused[row.id], resets: model?.limitFoot(for: row.id, now: .now)?.resetsAt)
        }
        .buttonStyle(.hover(radius: 6, id: "fleet.row.\(row.id)"))
        .focusable(false)
        .padding(.horizontal, 8)
        .padding(.bottom, 2)
        .accessibilityElement(children: .contain)
        .accessibilityAction { activate() }
        .accessibilityIdentifier("fleet.row.\(row.id)")
        .accessibilityAddTraits(selected ? .isSelected : [])
        // The run's "…" (decisions 185 and 217): on hover, its panel holding Copy Working Directory, Copy Branch Name
        // while the run is on a branch, then End Session… while the run is live, or Remove… once it has ended (an adopted
        // run's at any time); for VoiceOver, which does not hover, those are the row's own actions.
        .accessibilityActions {
            Button("Copy Working Directory") { copyToPasteboard(row.summary.cwd) }
                .accessibilityIdentifier("fleet.row.\(row.id).copyWorkingDirectoryAction")
            if let branch = row.summary.gitBranch {
                Button("Copy Branch Name") { copyToPasteboard(branch) }
                    .accessibilityIdentifier("fleet.row.\(row.id).copyBranchNameAction")
            }
            if endable {
                Button("End Session…") { ending = row }
                    .accessibilityIdentifier("fleet.row.\(row.id).endRunAction")
            }
            if removable {
                Button("Remove…") { removing = row }
                    .accessibilityIdentifier("fleet.row.\(row.id).removeAction")
            }
        }
        .overlay(alignment: .trailing) {
            if more && !selecting {
                RunMore(
                    id: "fleet.row.\(row.id).more", action: endable ? .endRun : removable ? .remove : nil,
                    copies: row.summary.cwd, branch: row.summary.gitBranch
                ) { if endable { ending = row } else { removing = row } }
                .padding(.trailing, 14)
            }
        }
        // The same menu, at the pointer; none while choosing.
        .onRightClick { point in
            guard !selecting else { return }
            RunMore.show(
                menu, at: CGRect(origin: point, size: .zero), id: "fleet.row.\(row.id).more",
                action: endable ? .endRun : removable ? .remove : nil, copies: row.summary.cwd,
                branch: row.summary.gitBranch
            ) { if endable { ending = row } else { removing = row } }
        }
        .onHover { hovered = $0 ? row.id : (hovered == row.id ? nil : hovered) }
    }

    /// Gives the list the keyboard, and gives it again once the window has laid out the run that opened: the new pane
    /// beside the list otherwise hands the keyboard to the first control left in the window.
    private func keepListFocus() {
        listFocused = true
        DispatchQueue.main.async { listFocused = true }
    }

    private func endRun(_ row: FleetRow) {
        guard let model = machines.model(for: row.machineID) else { return }
        Task { endNote = await model.endRun(row.summary) }
    }

    private func remove(_ row: FleetRow) {
        guard let model = machines.model(for: row.machineID) else { return }
        Task { removeNote = FleetWords.removalRefused(await model.removeSession(row.summary)) }
    }

    /// Ended, on a computer that removes runs.
    private func removable(_ row: FleetRow) -> Bool {
        row.status == .ended && !row.summary.atLimit && row.summary.isRemovable
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
            if let note = batchNote(machines.fleet) {
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
}

/// A project's header, 30 pt: its disclosure, which folds and opens the project, its name,
/// and at its end how many of its sessions need the person, in amber, or else how many sessions it has. It sticks to
/// the list's top while its sessions scroll under it, opaque, with a hairline under it.
private struct ProjectHeader: View {
    let project: ProjectList.Project
    let folded: Bool
    let toggle: () -> Void

    var body: some View {
        Button(action: toggle) {
            HStack(spacing: 6) {
                Image(systemName: folded ? "chevron.right" : "chevron.down")
                    .font(.system(size: 8, weight: .semibold)).foregroundStyle(Theme.text3)
                    .frame(width: 9)
                Text(project.name).font(Theme.ui(12, .semibold)).foregroundStyle(Theme.text).lineLimit(1)
                    .truncationMode(.middle)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Text(project.needsYou > 0 ? RunListWords.needYou(project.needsYou) : "\(project.sessions)")
                    .font(Theme.ui(11, project.needsYou > 0 ? .medium : .regular))
                    .foregroundStyle(project.needsYou > 0 ? Theme.amber : Theme.text3)
                    .monospacedDigit()
                    .fixedSize()
            }
            .padding(.leading, 8)
            .padding(.trailing, 6)
            .frame(height: 30)
            // Its fill and hairline once it is stuck at the list's top, read from where it is drawn, frame by frame.
            .background {
                ZStack(alignment: .bottom) {
                    RoundedRectangle(cornerRadius: 6).fill(Theme.sheet)
                    Rectangle().fill(Theme.hairline).frame(height: 1)
                }
                .visualEffect { content, proxy in content.opacity(proxy.frame(in: .scrollView).minY <= 0 ? 1 : 0) }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 6, id: "fleet.project.\(project.name)"))
        .focusable(false)
        .help(project.folder)
        .accessibilityLabel(project.name)
        .accessibilityValue(
            "\(folded ? "folded" : "open"), "
                + (project.needsYou > 0 ? RunListWords.needYou(project.needsYou) : RunListWords.runs(project.sessions)))
        .accessibilityIdentifier("fleet.project.\(project.name)")
        .padding(.horizontal, 8)
        .padding(.bottom, 2)
        // Opaque, so the sessions it is stuck over do not show through it.
        .background(Theme.sidebar)
    }
}

/// The list head's "All ▾": the computer the list is narrowed to, by its mark and name, or All; it opens the computer
/// menu below itself, its leading edge on the button's, 6 pt down. The menu's rows: All computers, with every
/// computer's runs; each computer, its mark, its name over its runs (or, not reached, when it was last seen), and its
/// CPU, RAM and disk, read afresh while the menu is open; then Add Computer….
private struct ComputerFilter: View {
    @Environment(Machines.self) private var machines
    @Binding var filter: FleetFilter?
    let addComputer: () -> Void
    @State private var open = false
    @State private var height: CGFloat = 0

    private var chosen: AppModel? {
        guard case .computer(let id) = filter else { return nil }
        return machines.models.first { $0.machineID == id }
    }

    var body: some View {
        Button { open.toggle() } label: {
            HStack(spacing: 6) {
                ComputerMark(os: chosen?.machineOS, size: 11)
                Text(chosen?.machineName ?? "All").font(Theme.ui(11.5, .medium)).lineLimit(1).truncationMode(.middle)
                Image(systemName: "chevron.down").font(.system(size: 8.5, weight: .semibold))
            }
            .foregroundStyle(Theme.text2)
            // The small button, as the run header's Changes is drawn.
            .padding(.horizontal, 8)
            .frame(height: 22)
            .background(Color.white.opacity(open ? 0.13 : 0.08), in: RoundedRectangle(cornerRadius: 6))
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color.white.opacity(0.06)))
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 6, id: "fleet.computers"))
        .focusable(false)
        .help(chosen?.machineName ?? RunListWords.allTitle)
        .accessibilityIdentifier("fleet.computers")
        .accessibilityValue(open ? "open" : "closed")
        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height = $0 }
        .overlay(alignment: .topLeading) {
            if open { menu.openPanel().offset(y: height + Theme.panelGap) }
        }
        .closesPanel($open)
        #if DEBUG
            .task {
                guard RenderWindow.opensComputerMenu else { return }
                // After the render has sized and placed its window, as a click comes on a window at rest.
                try? await Task.sleep(for: .seconds(1))
                open = true
            }
        #endif
    }

    private var menu: some View {
        let computers = machines.models.filter(\.showsFleet)
        let fleet = machines.fleet
        let all = RunList.activeCount(fleet, on: nil)
        return VStack(alignment: .leading, spacing: 0) {
            choice(0, id: "fleet.computers.all", chosen: filter == nil, to: nil) {
                MenuRow(chosen: filter == nil) {
                    ComputerMark(os: nil).foregroundStyle(Theme.text2)
                    Text(RunListWords.allTitle).font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1)
                } trailing: {
                    Text(RunListWords.runs(all)).font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                }
            }
            MenuDivider().padding(.vertical, 4)
            ForEach(Array(computers.enumerated()), id: \.element.machineID) { index, computer in
                let id = computer.machineID ?? ""
                choice(index + 1, id: "fleet.computers.\(id)", chosen: filter == .computer(id), to: .computer(id)) {
                    ComputerChoice(
                        computer: computer, chosen: filter == .computer(id),
                        count: RunList.activeCount(fleet, on: id))
                }
            }
            MenuDivider().padding(.vertical, 4)
            Button(action: chooseAdd) {
                MenuRow(chosen: false) {
                    Image(systemName: "plus").font(.system(size: 11)).foregroundStyle(Theme.text2)
                    Text("Add Computer…").font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1)
                }
            }
            .buttonStyle(.panelRow)
            .panelRow(computers.count + 1, id: "fleet.computers.addComputer", choose: chooseAdd)
            .accessibilityIdentifier("fleet.computers.addComputer")
        }
        .frame(minWidth: 280)
        .fixedSize()
        .menuCard()
    }

    private func chooseAdd() {
        open = false
        addComputer()
    }

    /// The menu's row `index`: a click narrows the list to `to`, or to every run.
    private func choice(
        _ index: Int, id: String, chosen: Bool, to narrowed: FleetFilter?, @ViewBuilder label: () -> some View
    ) -> some View {
        let choose = {
            filter = narrowed
            open = false
        }
        return Button(action: choose, label: label)
            .buttonStyle(.panelRow)
            .panelRow(index, id: id, choose: choose)
            .accessibilityIdentifier(id)
            .accessibilityAddTraits(chosen ? .isSelected : [])
    }
}

/// A computer in the list head's menu: its mark and name over how many runs it has, with its load at the row's end
/// while it is reached; not reached, it is greyed and says so, and its line says when it was last seen, or how many
/// runs wait for it. Its load is read afresh while the row is shown, as long as the menu is open.
private struct ComputerChoice: View {
    let computer: AppModel
    let chosen: Bool
    let count: Int

    /// Not offline: a link connecting, quiet or refused is still this computer's, and says so in Settings.
    private var live: Bool { computer.linkHealth.level != .offline }

    /// What the row shows of the computer's latest sample: only while its link is live, so no figure outlasts the
    /// link that brought it.
    private var load: MachineLoad? {
        guard computer.linkHealth.level == .live else { return nil }
        var stats = computer.machineStats
        #if DEBUG
            if let shown = RenderWindow.machineStats { stats = shown }
        #endif
        guard let stats else { return nil }
        return MachineLoad(
            cpu: stats.cpu, memoryUsed: stats.memoryUsed, memoryTotal: stats.memoryTotal,
            diskUsed: stats.diskUsed, diskTotal: stats.diskTotal)
    }

    private var second: String? {
        live
            ? RunListWords.runs(count)
            : RunListWords.offlineLine(lastSeen: computer.connection.lastContactAt, waiting: count, now: .now)
    }

    var body: some View {
        let load = load
        Group {
            if let second {
                MenuRow(chosen: chosen) { name } second: { Text(second) } trailing: { trailing(load) }
            } else {
                MenuRow(chosen: chosen) { name } trailing: { trailing(load) }
            }
        }
        .help([computer.machineSystem, load?.line].compactMap { $0 }.joined(separator: " · "))
        .task { await computer.keepMachineStatsFresh() }
    }

    /// Its mark and name, greyed while it is offline; a link reached but not working says its own word after the name
    /// ("· Pair again", "· Blocked").
    @ViewBuilder private var name: some View {
        ComputerMark(os: computer.machineOS).foregroundStyle(live ? Theme.text2 : Theme.text3)
        Text(computer.machineName ?? "Unknown computer").font(Theme.ui(12.5))
            .foregroundStyle(live ? Theme.text : Theme.text3).lineLimit(1)
        if live, let word = computer.linkHealth.machineWord {
            Text("· \(word)").font(Theme.ui(12.5)).foregroundStyle(Theme.text3).fixedSize()
        }
    }

    /// At the row's end: its load while it is reached, "Offline" while it is not (artboard 01).
    @ViewBuilder private func trailing(_ load: MachineLoad?) -> some View {
        if !live {
            Text(computer.linkHealth.word).font(Theme.ui(11.5)).foregroundStyle(Theme.text3).fixedSize()
        } else if let load {
            HStack(spacing: 10) {
                if let cpu = load.cpu { LoadMeter(label: "CPU", used: cpu) }
                if let memory = load.memory { LoadMeter(label: "RAM", used: memory) }
                if let disk = load.disk { LoadMeter(label: "Disk", used: disk) }
            }
            .padding(.leading, 16)
        }
    }
}

/// One figure of a computer's load: its name and share on one line over a 44 pt bar, amber at 90% or more, when the
/// computer needs you, and quiet grey below that.
private struct LoadMeter: View {
    let label: String
    let used: Double

    var body: some View {
        let high = used >= 0.9
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 3) {
                Text(label).foregroundStyle(Theme.text3)
                Text(MachineLoad.percent(used)).foregroundStyle(high ? Theme.amber : Theme.text2)
            }
            .font(Theme.mono(10.5))
            .fixedSize()
            ZStack(alignment: .leading) {
                Capsule().fill(Color.white.opacity(0.08))
                Capsule().fill(high ? Theme.amber : Theme.text3).frame(width: 44 * min(max(used, 0), 1))
            }
            .frame(width: 44, height: 2.5)
        }
    }
}

/// The list's foot, and Settings' under its own list: this Mac's Tailscale as its own daemon says it (`peers`), read as
/// the Add sheet reads it, the whole sign-in, or in its place what is wrong in the words a computer's link uses; then
/// the gear, into Settings or, in Settings, back out. Nothing until the daemon has said, but the gear. Where this Mac's
/// daemon answers is read every five seconds, as the Add sheet reads it, so its move from loopback onto Tailscale shows.
struct ListFoot: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup
    let openSettings: () -> Void
    @State private var address: LocalDaemon.Address?

    /// What this Mac's own daemon last said about its Tailscale.
    private var directory: PeerDirectory? {
        #if DEBUG
            if let peers = RenderWindow.peers { return peers }
        #endif
        return setup.thisMac(at: address).flatMap { machines.model(for: $0)?.tailnet }
    }

    var body: some View {
        let status = directory.map { TailscaleNote.Status($0, onLoopback: TailscaleNote.onLoopback(address)) }
        VStack(spacing: 0) {
            Rectangle().fill(Theme.hairline).frame(height: 1)
            HStack(spacing: 7) {
                if let status {
                    Text(status.footWords).font(Theme.ui(11.5)).foregroundStyle(status.isSignedIn ? Theme.text2 : Theme.text3)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .help(status.footWords)
                        .accessibilityLabel(status.spokenFoot)
                        .accessibilityIdentifier("fleet.tailscale")
                }
                Spacer(minLength: 4)
                Button { openSettings() } label: {
                    Image(systemName: "gearshape.fill").font(.system(size: 13)).foregroundStyle(Theme.text2)
                        .frame(width: 26, height: 26)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.hover(radius: 6, id: "fleet.settings"))
                .help("Settings")
                .accessibilityLabel("Settings")
                .accessibilityIdentifier("fleet.settings")
            }
            .padding(.leading, 18)
            .padding(.trailing, 10)
            .frame(height: 34)
        }
        .accessibilityElement(children: .contain)
        .task {
            guard let root = setup.installer?.root else { return }
            while !Task.isCancelled {
                // A read that gets no answer (the service restarting onto Tailscale) keeps what it last said: the foot
                // changes only on an answer.
                if let read = await Task.detached(operation: { LocalDaemon.address(root: root) }).value { address = read }
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }
}

extension TailscaleNote.Status {
    fileprivate var isSignedIn: Bool {
        if case .signedIn = self { return true }
        return false
    }

    /// What VoiceOver reads for the foot's words.
    fileprivate var spokenFoot: String {
        switch self {
        case .signedIn(let login): login.map { "Tailscale connected, signed in as \($0)" } ?? "Tailscale connected"
        default: "Tailscale: \(footWords)"
        }
    }

    /// The foot's words: the whole sign-in once signed in; otherwise what is wrong.
    fileprivate var footWords: String {
        switch self {
        case .signedIn(let login): login ?? "Signed in to Tailscale"
        case .notConnected, .notInstalled: LinkNotice.connectTailscale
        case .moving: "Connecting"
        case .tagged: "Not signed in as you"
        }
    }
}

/// One run on one 30 pt line: its state, what it is called, and how long since it last did anything; in Needs you,
/// narrowed to another computer, that run's computer by its mark before its age.
private struct FleetLine: View {
    let row: FleetRow
    /// Open beside the list: the selected fill.
    let selected: Bool
    /// The run's "…" stands in its age's place, which hides while it shows: nothing else in the row moves.
    let roomForMore: Bool
    /// The run's computer's system, when the row names its computer.
    var elsewhere: String? = nil
    /// Its computer is not reached: the mark dims (artboard 15).
    var offline = false
    /// While choosing runs to remove: whether this one is chosen; its box takes the status mark's place.
    var chosen: Bool? = nil
    /// Why the last batch did not remove it, in place of its age.
    var refusal: String? = nil
    /// When the usage limit the run is at resets, in place of its age; nil when that is not known.
    var resets: Date? = nil
    @State private var lastTick = Date()

    private var since: Date { row.lastActive }

    var body: some View {
        HStack(spacing: 10) {
            if let chosen {
                ChooseBox(on: chosen).accessibilityIdentifier("fleet.row.\(row.id).chosen")
            } else {
                StateIcon(status: row.status, offline: offline, limited: row.summary.atLimit)
                    .accessibilityIdentifier("fleet.row.\(row.id).status")
            }
            // The name fills the row up to its time: no spacer of its own, which would cost the row's gap twice.
            Text(row.title)
                .font(Theme.ui(13, row.status == .blocked ? .medium : .regular))
                .foregroundStyle(row.status == .ended ? Theme.text2 : Theme.text)
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(maxWidth: .infinity, alignment: .leading)
            if let refusal {
                Text(refusal).font(Theme.ui(11.5)).foregroundStyle(Theme.amber).lineLimit(1)
            }
            if let elsewhere {
                ComputerMark(os: elsewhere, size: 10).foregroundStyle(Theme.text3)
            }
            Text(time(now: AgeTick.renderTime(lastTick: lastTick)))
                .font(Theme.ui(11.5))
                .foregroundStyle(Theme.text3)
                .monospacedDigit()
                .frame(minWidth: RunMore.width, alignment: .trailing)
                .opacity(roomForMore ? 0 : 1)
        }
        .padding(.horizontal, 10)
        .frame(height: 30)
        .background(selected ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 6))
        // Its computer on hover, which tells apart two runs named alike on two computers.
        .help(row.machineName ?? "")
        .task(id: AgeClock(since: since, scale: .age)) {
            await AgeTick.follow(AgeClock(since: since, scale: .age)) { lastTick = $0 }
        }
    }

    /// At a usage limit, when it resets ("6:00 PM", "Mon"); else how long since the run last did anything.
    private func time(now: Date) -> String {
        guard row.summary.atLimit, let resets else { return Format.age(since: since, now: now) }
        return Format.clock(resets, now: now)
    }
}

/// A run's "…": it opens the window's row menu (`RowMenu`) below itself,
/// holding End Session… or Remove… (`PanelItem`, destructive), each of which asks before anything is sent; a run's menu
/// first offers Copy Working Directory, its folder as the computer names it, as the iPhone's run offers it, and is
/// there for every run, so a run that can be neither ended nor removed from here has that item alone. A right-click
/// on the row opens the same menu at the pointer (`RunMore.show`).
struct RunMore: View {
    enum Action {
        case endRun, remove

        var title: String { self == .endRun ? "End Session…" : "Remove…" }
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
    /// The branch the run is on, which the menu's next item copies; none on a detached head, or before git was read.
    var branch: String? = nil
    let perform: () -> Void
    #if DEBUG
        /// A render's `-CC_RUN_MENU <uid>` names this run's "…": once shown, it opens a moment later, as a click
        /// opens it.
        private var opensAtLaunch: Bool { RenderWindow.openRunMenu.map { id == "fleet.row.\($0).more" } == true }
    #endif
    @Environment(RowMenu.self) private var menu
    @State private var frame: CGRect = .zero

    private var open: Bool { menu.shown?.owner == id }

    var body: some View {
        Button { Self.show(menu, at: frame, id: id, action: action, copies: copies, branch: branch, perform: perform)
        } label: {
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
                Self.show(menu, at: frame, id: id, action: action, copies: copies, branch: branch, perform: perform)
            }
        #endif
    }

    /// The row's menu by `rect` (its "…", or a right-click's point as a zero-size rect): Copy Working Directory when
    /// it `copies` a folder, Copy Branch Name when it knows a `branch`, then the act on the row's own subject through
    /// `perform`.
    @MainActor static func show(
        _ menu: RowMenu, at rect: CGRect, id: String, action: Action?, copies: String? = nil, branch: String? = nil,
        perform: @escaping () -> Void
    ) {
        let copy = copies.map { folder in
            RowMenu.Item(id: "\(id).copyWorkingDirectory", title: "Copy Working Directory", symbol: PanelSymbol.copy,
                         destructive: false) { copyToPasteboard(folder) }
        }
        let copyBranch = branch.map { branch in
            RowMenu.Item(id: "\(id).copyBranchName", title: "Copy Branch Name", symbol: PanelSymbol.copy,
                         destructive: false) { copyToPasteboard(branch) }
        }
        let act = action.map { action in
            RowMenu.Item(id: "\(id).\(action.id)", title: action.title, symbol: action.symbol, destructive: true,
                         perform: perform)
        }
        menu.toggle(rect, owner: id, items: [copy, copyBranch, act].compactMap { $0 })
    }

}

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

/// A run's state as the fleet orders it (`FleetStatus`), drawn small: amber when it waits on you; a red ring when the
/// run is at its usage limit, whatever its last turn's state.
struct StateIcon: View {
    let status: FleetStatus
    /// Its computer is not reached: drawn in its stopping form, dim, as nothing of it can move; read as its status.
    var offline = false
    var limited = false

    var body: some View {
        ZStack {
            if limited {
                Circle().strokeBorder(Theme.red, lineWidth: 1.5)
            } else {
                mark
            }
        }
        .frame(width: 13, height: 13)
        .help(label)
        // One element, read as the word it stands for: its marks alone read as symbol names ("Selected").
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(label)
    }

    private var label: String { limited ? "Usage limit reached" : status.label }

    @ViewBuilder private var mark: some View {
        ZStack {
            switch offline && status != .ended ? .stopping : status {
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
