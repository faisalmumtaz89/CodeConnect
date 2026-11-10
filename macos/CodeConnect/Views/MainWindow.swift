import CodeConnectKit
import SwiftUI

/// The main window: the sidebar's agents, computers and projects, and the fleet's runs beside it;
/// before any computer is paired, what to do first in the list's place. Agents and computers are
/// added the same way: the + on their sidebar section, one sheet, and File > New Agent (⌘N) and
/// Add Computer (⇧⌘N); an iPhone from File > Add iPhone.
struct MainWindow: View {
    @Environment(Machines.self) private var machines
    @State private var adding: AddKind?
    /// The address Add a computer starts with, when a refused computer asked to pair again.
    @State private var repairing: String?
    @State private var scope = FleetScope.active
    @State private var grouping = FleetGroup.Kind.computer
    /// What the sidebar narrows the list to, if anything.
    @State private var filter: FleetFilter?
    /// The run open beside the list, by its key; none, and the list fills the window.
    @State private var opened: String?
    /// The tab chosen other than the conversation, with the run it was chosen on. Any other run shows its
    /// conversation, and each run opens on it.
    @State private var chosen: (run: String, tab: SessionTab)?
    /// Whether the open run's composer is using Esc: its commands list is open or its field has focus.
    @FocusedValue(\.composerHoldsEscape) private var composerHoldsEscape
    /// The window's width, from which the list's share beside an open run is worked out.
    @State private var width: CGFloat = 0

    init() {
        #if DEBUG
            _adding = State(
                initialValue: RenderWindow.opensOnIPhone
                    ? .iPhone : RenderWindow.opensOnAgent ? .agent : RenderWindow.opensAddComputer ? .computer : nil)
            _scope = State(initialValue: RenderWindow.fleetScope)
            _grouping = State(initialValue: RenderWindow.fleetGrouping)
            _opened = State(initialValue: RenderWindow.opened)
        #endif
    }

    var body: some View {
        HStack(spacing: 0) {
            Sidebar(adding: $adding, scope: $scope, filter: $filter, pairAgain: pairAgain)
                .frame(width: 216)
            Rectangle().fill(Theme.hairline).frame(width: 1)
            if machines.showsFleet {
                let open = opened.flatMap { key in machines.fleet.first { $0.id == key } }
                let model = open.flatMap { machines.model(forSession: $0.id) }
                FleetList(scope: $scope, grouping: $grouping, filter: $filter, opened: $opened) { adding = .agent }
                    // Beside an open run the list gives way first: the pane keeps room for a card's answers in one
                    // row (450), and the list takes what is left, between what its rows still read at (290) and 580.
                    .frame(width: model == nil ? nil : min(580, max(290, width - 218 - 450)))
                if let open, let model {
                    Rectangle().fill(Theme.hairline).frame(width: 1)
                    SessionPane(row: open, model: model, tab: tab(of: open.id)) {
                        if let address = model.endpoint?.typedAddress { pairAgain(address) }
                    }
                    .id(open.id)
                }
            } else {
                EmptyFleet(adding: $adding)
            }
        }
        .background(Theme.window)
        .ignoresSafeArea()
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { width = $0 }
        .toolbar(removing: .title)
        .toolbarBackgroundVisibility(.hidden, for: .windowToolbar)
        .background(WindowChrome())
        .focusedSceneValue(\.adding, $adding)
        .onChange(of: opened) { chosen = nil }
        // Esc closes the open run; a sheet's own Esc is its Cancel. Not while its terminal is shown: there Esc is the
        // agent's (it interrupts a turn), and a window shortcut would take it before the terminal saw it. Nor while the
        // composer is using it: Esc closes its list, or leaves its field, first.
        .background {
            if opened != nil, adding == nil, !(chosen?.run == opened && chosen?.tab == .terminal),
                composerHoldsEscape != true
            {
                Button("Close") { opened = nil }
                    .keyboardShortcut(.cancelAction)
                    .accessibilityIdentifier("session.close")
                    .opacity(0)
                    .allowsHitTesting(false)
            }
        }
        #if DEBUG
            .task {
                // A test launch's fleet is in place a moment after the window is.
                try? await Task.sleep(for: .seconds(1))
                let active = FleetScope.active.rows(machines.fleet)
                switch RenderWindow.filter {
                case "computer": filter = FleetGroup.groups(active, by: .computer).first.map { .computer($0.id) }
                case "project": filter = FleetGroup.groups(active, by: .project).first.map { .project($0.id) }
                default: break
                }
            }
        #endif
        .sheet(item: $adding, onDismiss: { repairing = nil }) { kind in
            AddSheet(kind: kind, address: repairing) {
                // The run just started is on screen: every active agent, unfiltered.
                scope = .active
                filter = nil
            }
                #if DEBUG
                    .renderLook()
                #endif
        }
    }

    /// A run's tab. It is another than the conversation only while that run is the one it was chosen on, so a run
    /// opened from another run's terminal shows its conversation from its first frame.
    private func tab(of run: String) -> Binding<SessionTab> {
        Binding(
            get: { chosen.flatMap { $0.run == run ? $0.tab : nil } ?? .conversation },
            set: { chosen = $0 == .conversation ? nil : (run, $0) })
    }

    /// A refused computer paired again: the Add Computer sheet, its address already in the code form.
    private func pairAgain(_ address: String) {
        repairing = address
        adding = .computer
    }
}

extension FocusedValues {
    /// What the main window is adding: the File menu's New Agent, Add Computer and Add iPhone open its sheet.
    @Entry var adding: Binding<AddKind?>?
}

/// File > New Agent (⌘N), Add Computer (⇧⌘N) and Add iPhone, in place of New Window: the app has one window,
/// and these open its sheet. Unavailable with no main window, or with its sheet already open.
struct AddCommands: Commands {
    @FocusedValue(\.adding) private var adding

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Group {
                Button("New Agent…") { adding?.wrappedValue = .agent }
                    .keyboardShortcut("n", modifiers: .command)
                Button("Add Computer…") { adding?.wrappedValue = .computer }
                    .keyboardShortcut("n", modifiers: [.command, .shift])
                Button("Add iPhone…") { adding?.wrappedValue = .iPhone }
            }
            .disabled(adding.map { $0.wrappedValue != nil } ?? true)
        }
    }
}

// MARK: - Sidebar

private struct Sidebar: View {
    @Environment(Machines.self) private var machines
    @Binding var adding: AddKind?
    @Binding var scope: FleetScope
    @Binding var filter: FleetFilter?
    let pairAgain: (String) -> Void

    var body: some View {
        let active = FleetScope.active.rows(machines.fleet)
        let computers = machines.models.filter(\.showsFleet)
        VStack(alignment: .leading, spacing: 0) {
            // Clear of the traffic lights.
            Color.clear.frame(height: 52)

            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    SidebarHeading("Agents", add: .agent, adding: $adding)
                    VStack(spacing: 1) {
                        nav(.active, symbol: "circle.dotted")
                        nav(.finished, symbol: "checkmark.circle")
                    }
                    .padding(.horizontal, 8)

                    SidebarHeading("Computers", add: .computer, adding: $adding)
                    if computers.isEmpty {
                        Text("None yet")
                            .font(Theme.ui(12.5))
                            .foregroundStyle(Theme.text3)
                            .padding(.horizontal, 18)
                            .frame(height: 28)
                    } else {
                        VStack(spacing: 2) {
                            ForEach(computers, id: \.machineID) { computer in
                                let narrowed = FleetFilter.computer(computer.machineID ?? "")
                                ComputerRow(
                                    computer: computer,
                                    count: active.filter { $0.machineID == computer.machineID }.count,
                                    selected: filter == narrowed,
                                    select: { filter = filter == narrowed ? nil : narrowed },
                                    pairAgain: pairAgain)
                            }
                        }
                        .padding(.horizontal, 8)
                    }

                    let projects = FleetGroup.groups(active, by: .project)
                    if !projects.isEmpty {
                        SidebarHeading("Projects")
                        VStack(spacing: 1) {
                            ForEach(projects) { project in
                                let narrowed = FleetFilter.project(project.id)
                                let selected = filter == narrowed
                                Button { filter = selected ? nil : narrowed } label: {
                                    VStack(alignment: .leading, spacing: 3) {
                                        HStack(spacing: 8) {
                                            Image(systemName: "folder").font(.system(size: 11)).foregroundStyle(Theme.text3)
                                                .frame(width: 16)
                                                .accessibilityHidden(true)
                                            Text(project.name).font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1)
                                            Spacer()
                                            Text(verbatim: "\(project.rows.count)").font(Theme.ui(11.5))
                                                .foregroundStyle(Theme.text3).monospacedDigit()
                                        }
                                        ProjectPlace(project: project, namingComputer: computers.count > 1, size: 10)
                                            .padding(.leading, 24)
                                    }
                                    .padding(.horizontal, 8)
                                    .padding(.vertical, 5)
                                    .background(selected ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 6))
                                }
                                .buttonStyle(.hover(id: "sidebar.project.\(project.axID)"))
                                .accessibilityIdentifier("sidebar.project.\(project.axID)")
                                .accessibilityAddTraits(selected ? .isSelected : [])
                            }
                        }
                        .padding(.horizontal, 8)
                    }
                }
                .padding(.bottom, 12)
            }
        }
        .background(Theme.sidebar)
    }

    private func nav(_ target: FleetScope, symbol: String) -> some View {
        Button { scope = target } label: {
            NavRow(symbol: symbol, title: target.rawValue, count: target.rows(machines.fleet).count,
                selected: scope == target)
        }
        .buttonStyle(.hover(id: "sidebar.scope.\(target.rawValue.lowercased())"))
        .accessibilityIdentifier("sidebar.scope.\(target.rawValue.lowercased())")
        .accessibilityAddTraits(scope == target ? .isSelected : [])
    }
}

/// A paired computer: its name, how many runs are active on it, and how its link is. One whose daemon refused
/// its token is a button: a click pairs it again, with its address already in the code form.
private struct ComputerRow: View {
    let computer: AppModel
    let count: Int
    let selected: Bool
    /// Narrows the list to this computer, or back to every run when it already is.
    let select: () -> Void
    let pairAgain: (String) -> Void

    private var address: String? { computer.endpoint?.typedAddress }
    private var id: String { "sidebar.computer.\(computer.machineID ?? "")" }

    var body: some View {
        Group {
            if computer.linkHealth.level == .rejected, let address {
                Button { pairAgain(address) } label: { row.contentShape(Rectangle()) }
                    .buttonStyle(.hover(id: "\(id).pairAgain"))
                    .accessibilityIdentifier("\(id).pairAgain")
                    .accessibilityElement(children: .contain)
                    .accessibilityIdentifier(id)
            } else {
                Button(action: select) {
                    row.background(selected ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 6))
                }
                .buttonStyle(.hover(id: id))
                .accessibilityIdentifier(id)
                .accessibilityAddTraits(selected ? .isSelected : [])
            }
        }
        .help([computer.machineSystem, computer.linkHealth.word].compactMap { $0 }.joined(separator: " · "))
    }

    private var row: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 8) {
                Image(systemName: "desktopcomputer").font(.system(size: 11)).foregroundStyle(Theme.text2)
                    .frame(width: 16)
                    .accessibilityHidden(true)
                Text(computer.machineName ?? "Unknown computer").font(Theme.ui(12.5)).foregroundStyle(Theme.text)
                    .lineLimit(1)
                Spacer()
                Text(verbatim: "\(count)").font(Theme.ui(11.5)).foregroundStyle(Theme.text3).monospacedDigit()
            }
            Text(computer.linkHealth.word)
                .font(Theme.ui(10.5))
                .foregroundStyle(Theme.text3)
                .padding(.leading, 24)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
    }
}

/// A sidebar section's name, with the + that adds to it: the one way agents and computers are
/// both added.
private struct SidebarHeading: View {
    let title: String
    var add: AddKind?
    var adding: Binding<AddKind?>?

    init(_ title: String, add: AddKind? = nil, adding: Binding<AddKind?>? = nil) {
        self.title = title
        self.add = add
        self.adding = adding
    }

    var body: some View {
        HStack {
            Text(title).font(Theme.ui(11, .medium)).foregroundStyle(Theme.text3)
            Spacer()
            if let add, let adding {
                let id = add == .agent ? "sidebar.newAgent" : "sidebar.addComputer"
                Button { adding.wrappedValue = add } label: {
                    Image(systemName: "plus").font(.system(size: 11, weight: .medium))
                        .frame(width: 20, height: 18)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.hover(radius: 4, id: id))
                .foregroundStyle(Theme.text2)
                .help(add.shortcut.map { "\(add.title) (\($0))" } ?? add.title)
                // Its symbol alone reads "Add", the same in both sections.
                .accessibilityLabel(add.title)
                .accessibilityIdentifier(id)
            }
        }
        .padding(.leading, 18)
        .padding(.trailing, 12)
        .padding(.top, 20)
        .padding(.bottom, 6)
    }
}

private struct NavRow: View {
    let symbol: String
    let title: String
    let count: Int
    let selected: Bool

    var body: some View {
        HStack(spacing: 8) {
            // Hidden: a checkmark symbol in a button's label marks the button selected.
            Image(systemName: symbol).font(.system(size: 11.5)).frame(width: 16)
                .accessibilityHidden(true)
            Text(title).font(Theme.ui(12.5, selected ? .medium : .regular))
            Spacer()
            Text("\(count)").font(Theme.ui(11.5)).foregroundStyle(Theme.text3).monospacedDigit()
        }
        .foregroundStyle(selected ? Theme.text : Theme.text2)
        .padding(.horizontal, 8)
        .frame(height: 28)
        .background(selected ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 6))
    }
}

// MARK: - The empty fleet

/// What the list shows before there is anything in it: what CodeConnect needs, and the way to
/// give it.
private struct EmptyFleet: View {
    @Binding var adding: AddKind?

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Text("Active").font(Theme.ui(13, .semibold)).foregroundStyle(Theme.text)
                Text("0").font(Theme.ui(12)).foregroundStyle(Theme.text3)
                Spacer()
            }
            .padding(.horizontal, 16)
            .frame(height: 52)
            Rectangle().fill(Theme.hairline).frame(height: 1)

            VStack(spacing: 12) {
                Image(systemName: "desktopcomputer")
                    .font(.system(size: 30, weight: .light))
                    .foregroundStyle(Theme.text3)
                    .padding(.bottom, 4)
                    .accessibilityHidden(true)
                Text("No computers yet").font(Theme.ui(15, .semibold)).foregroundStyle(Theme.text)
                Text(
                    "Add the Mac or Linux computer your agents run on. Its sessions appear here, with what needs you marked."
                )
                .font(Theme.ui(12.5))
                .foregroundStyle(Theme.text2)
                .multilineTextAlignment(.center)
                .lineSpacing(3)
                .frame(maxWidth: 380)
                ActionButton(title: "Add a computer", key: AddKind.computer.shortcut, prominent: true, id: "empty.addComputer") {
                    adding = .computer
                }
                .padding(.top, 6)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

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

/// A button with its key, where one reaches it: white and solid for the one thing to do, raised for
/// the rest.
struct ActionButton: View {
    let title: String
    let key: String?
    let prominent: Bool
    /// What a driver finds it by.
    let id: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                Text(title).font(Theme.ui(12, .medium))
                if let key { Text(key).font(Theme.ui(11)).opacity(0.6) }
            }
            .foregroundStyle(prominent ? Color.black : Theme.text)
            .padding(.horizontal, 10)
            .frame(height: 26)
            .background(prominent ? Theme.text : Theme.raised, in: RoundedRectangle(cornerRadius: 6))
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(prominent ? Color.clear : Theme.hairline))
        }
        .buttonStyle(.hover(over: true, id: id))
        .accessibilityIdentifier(id)
    }
}
