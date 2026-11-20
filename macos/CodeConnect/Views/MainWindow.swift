import CodeConnectKit
import SwiftUI

/// The main window: the sidebar's agents, computers and projects, and the fleet's runs beside it;
/// before any computer is paired, what to do first in the list's place. Agents and computers are
/// added the same way: the + on their sidebar section, one sheet, and File > New Agent (⌘N) and
/// Add Computer (⇧⌘N); an iPhone from File > Add iPhone.
struct MainWindow: View {
    @Environment(Machines.self) private var machines
    /// The one "…" menu a row of the list or the sidebar opens, drawn over the whole window.
    @State private var rowMenu = RowMenu()
    @State private var adding: AddKind?
    /// The address Add a computer starts with, when a refused computer asked to pair again.
    @State private var repairing: String?
    /// The list, what it is narrowed to, and the run open beside it: the app's, so a closed window opens on them again.
    @Binding var scope: FleetScope
    /// The grouping last chosen on this Mac, kept in its preferences, else by computer.
    @State private var grouping = FleetGroup.Kind.kept(in: .standard)
    /// What the sidebar narrows the list to, if anything.
    @Binding var filter: FleetFilter?
    /// The run open beside the list, by its key; none, and the list fills the window.
    @Binding var opened: String?
    /// What the File menu asked to add while the window was closed: the window opens its sheet on it.
    @Binding var pendingAdd: AddKind?
    /// The tab chosen other than the conversation, with the run it was chosen on. Any other run shows its
    /// conversation, and each run opens on it.
    @State private var chosen: (run: String, tab: SessionTab)?
    /// Whether the open run's composer is using Esc: its commands list is open or its field has focus.
    @FocusedValue(\.composerHoldsEscape) private var composerHoldsEscape
    /// Raised by Return in the run list: the open run's reply box takes the cursor.
    @State private var replyFocus = 0
    @Environment(ConfirmationQueue.self) private var asks
    /// The window's width, from which the list's share beside an open run is worked out.
    @State private var width: CGFloat = 0

    init(scope: Binding<FleetScope>, filter: Binding<FleetFilter?>, opened: Binding<String?>, pendingAdd: Binding<AddKind?>) {
        _scope = scope
        _filter = filter
        _opened = opened
        _pendingAdd = pendingAdd
        #if DEBUG
            _adding = State(
                initialValue: RenderWindow.opensOnIPhone
                    ? .iPhone : RenderWindow.opensOnAgent ? .agent : RenderWindow.opensAddComputer ? .computer : nil)
            if let given = RenderWindow.fleetGrouping { _grouping = State(initialValue: given) }
        #endif
    }

    var body: some View {
        HStack(spacing: 0) {
            Sidebar(adding: $adding, scope: $scope, filter: $filter, opened: $opened, pairAgain: pairAgain)
                .frame(width: 216)
            Rectangle().fill(Theme.hairline).frame(width: 1)
            if machines.showsFleet {
                let open = opened.flatMap { key in machines.fleet.first { $0.id == key } }
                let model = open.flatMap { machines.model(forSession: $0.id) }
                FleetList(scope: $scope, grouping: $grouping, filter: $filter, opened: $opened) { adding = .agent }
                    // Kept when the person changes it, never at launch.
                    .onChange(of: grouping) { _, kind in kind.keep(in: .standard) }
                    // Beside an open run the list gives way first: the pane keeps room for a card's answers in one
                    // row (450), and the list takes what is left, between what its rows still read at (290) and 580.
                    .frame(width: model == nil ? nil : min(580, max(290, width - 218 - 450)))
                    // The arrows open a run; Return then puts the cursor in its reply box.
                    .onKeyPress(.return) {
                        guard opened != nil else { return .ignored }
                        replyFocus += 1
                        return .handled
                    }
                if let open, let model {
                    Rectangle().fill(Theme.hairline).frame(width: 1)
                    SessionPane(row: open, model: model, tab: tab(of: open.id))
                        .id(open.id)
                        .environment(\.replyFocusRequest, replyFocus)
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
        .rowMenuHost(rowMenu)
        .focusedSceneValue(\.adding, $adding)
        .onChange(of: pendingAdd != nil, initial: true) {
            guard let pendingAdd else { return }
            adding = pendingAdd
            self.pendingAdd = nil
        }
        // Until every computer has caught up, or been waited out, the list says it is connecting.
        .task { await machines.waitOutLaunch() }
        .onChange(of: opened) { chosen = nil }
        // A computer removed, here or from Settings: the list lets go of its narrowing to it and of its run, which
        // has left the list with it.
        .onChange(of: machines.models.compactMap(\.machineID)) { before, after in
            let removed = Set(before).subtracting(after)
            guard !removed.isEmpty else { return }
            if case .computer(let id) = filter, removed.contains(id) { filter = nil }
            if let opened, !machines.fleet.contains(where: { $0.id == opened }) { self.opened = nil }
        }
        // Esc closes the open run; a sheet's own Esc is its Cancel, and an ask's its safe answer. Not while the composer
        // is using it: Esc closes its list, or leaves its field, first.
        .background {
            if opened != nil, adding == nil, composerHoldsEscape != true, !asks.isAsking {
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
            AddSheet(kind: kind, address: $repairing) { run in
                // The run just started is open, in the list of every active agent, unfiltered.
                scope = .active
                filter = nil
                opened = run
            }
                #if DEBUG
                    .renderLook()
                #endif
        }
    }

    /// A run's tab. It is another than the conversation only while that run is the one it was chosen on, so a run
    /// opened from another run's changes shows its conversation from its first frame.
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
/// and these open its sheet, opening the window first when it is closed or behind another. Unavailable while its
/// sheet is open.
struct AddCommands: Commands {
    @FocusedValue(\.adding) private var adding
    @Environment(\.openWindow) private var openWindow
    @Binding var pending: AddKind?

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Group {
                Button("New Agent…") { add(.agent) }
                    .keyboardShortcut("n", modifiers: .command)
                Button("Add Computer…") { add(.computer) }
                    .keyboardShortcut("n", modifiers: [.command, .shift])
                Button("Add iPhone…") { add(.iPhone) }
            }
            .disabled(adding?.wrappedValue != nil)
        }
    }

    private func add(_ kind: AddKind) {
        if let adding {
            adding.wrappedValue = kind
        } else {
            pending = kind
            openWindow(id: "main")
        }
    }
}

// MARK: - Sidebar

/// What the sidebar's arrow keys walk: a scope, or a computer or project the list narrows to.
private enum SidebarChoice: Equatable {
    case scope(FleetScope)
    case filter(FleetFilter)
}

private struct Sidebar: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup
    @Binding var adding: AddKind?
    @Binding var scope: FleetScope
    @Binding var filter: FleetFilter?
    @Binding var opened: String?
    let pairAgain: (String) -> Void
    /// The computer row under the pointer, by the computer's id.
    @State private var hovered: String?
    /// The window's one row menu, which a computer's "…" or a right-click on its row opens.
    @Environment(RowMenu.self) private var menu
    /// The computer Remove… is asking about.
    @State private var removing: AppModel?
    /// Where this Mac's own daemon answers. Its computer, which Setup pairs with again at every launch, is never offered
    /// for removal.
    @State private var address: LocalDaemon.Address?
    /// The row the arrow keys are on, while the sidebar has the keyboard; Return chooses it.
    @State private var highlighted: SidebarChoice?
    @FocusState private var sidebarFocused: Bool

    var body: some View {
        let active = FleetScope.active.rows(machines.fleet)
        let computers = machines.models.filter(\.showsFleet)
        let thisMac = setup.thisMac(at: address)
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
                        Text(FleetWords.noComputersYet)
                            .font(Theme.ui(12.5))
                            .foregroundStyle(Theme.text3)
                            .padding(.horizontal, 18)
                            .frame(height: 28)
                    } else {
                        VStack(spacing: 2) {
                            ForEach(computers, id: \.machineID) { computer in
                                let id = computer.machineID ?? ""
                                let narrowed = FleetFilter.computer(id)
                                let more = id != thisMac
                                    && (hovered == id || menu.shown?.owner == "sidebar.computer.\(id).more"
                                        || HoverButtonStyle.forced("sidebar.computer.\(id)"))
                                ComputerRow(
                                    computer: computer,
                                    count: active.filter { $0.machineID == computer.machineID }.count,
                                    selected: filter == narrowed,
                                    roomForMore: more,
                                    select: { choose(.filter(narrowed)) },
                                    pairAgain: pairAgain)
                                .background(lit(.filter(narrowed)), in: RoundedRectangle(cornerRadius: 6))
                                // A run's "…", as the fleet's rows have it: on hover, its panel holding Remove…; for
                                // VoiceOver, which does not hover, that is the row's own action. Not on this Mac's own.
                                .accessibilityActions {
                                    if id != thisMac {
                                        Button("Remove…") { removing = computer }
                                            .accessibilityIdentifier("sidebar.computer.\(id).removeAction")
                                    }
                                }
                                // On the name line, in the count's place.
                                .overlay(alignment: Alignment(horizontal: .trailing, vertical: .computerNameLine)) {
                                    if more {
                                        RunMore(id: "sidebar.computer.\(id).more", action: .remove) { removing = computer }
                                            .padding(.trailing, 8)
                                    }
                                }
                                // The same menu, at the pointer; this Mac's own has none.
                                .onRightClick { point in
                                    guard id != thisMac else { return }
                                    RunMore.show(
                                        menu, at: CGRect(origin: point, size: .zero), id: "sidebar.computer.\(id).more",
                                        action: .remove
                                    ) { removing = computer }
                                }
                                .onHover { hovered = $0 ? id : (hovered == id ? nil : hovered) }
                            }
                            // The owner's other computers Tailscale lists that have not joined, each with why.
                            ForEach(machines.unjoinedComputers) { unjoined in
                                UnjoinedComputerRow(unjoined: unjoined) { machines.tryAgain(unjoined) }
                            }
                        }
                        .padding(.horizontal, 8)
                        .task(id: computers.map(\.machineID)) {
                            #if DEBUG
                                // A render's ask about the last computer, reached by a fixture too, which has no
                                // install to read this Mac's address from; this Mac's own is told apart as its row does.
                                if RenderWindow.confirm == "computer", let last = computers.last,
                                    (last.machineID ?? "") != setup.thisMac(at: address)
                                {
                                    removing = last
                                }
                            #endif
                            guard let root = setup.installer?.root else { return }
                            address = await Task.detached { LocalDaemon.address(root: root) }.value ?? address
                        }
                    }

                    // The projects of the runs the list shows, Active or Finished, with their counts.
                    let projects = FleetGroup.groups(scope.rows(machines.fleet), by: .project)
                    if !projects.isEmpty {
                        SidebarHeading("Projects")
                        VStack(spacing: 1) {
                            ForEach(projects) { project in
                                let narrowed = FleetFilter.project(project.id)
                                let selected = filter == narrowed
                                Button { choose(.filter(narrowed)) } label: {
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
                                    .background(selected ? Theme.selected : lit(.filter(narrowed)), in: RoundedRectangle(cornerRadius: 6))
                                }
                                .buttonStyle(.hover(id: "sidebar.project.\(project.axID)"))
                                .focusable(false)
                                .accessibilityIdentifier("sidebar.project.\(project.axID)")
                                .accessibilityAddTraits(selected ? .isSelected : [])
                            }
                        }
                        .padding(.horizontal, 8)
                    }
                }
                .padding(.bottom, 12)
            }
            // One stop for the keyboard: ↑ and ↓ move a highlight through the sidebar's rows as drawn (the scopes, the
            // computers, the projects) and change nothing; Return chooses the highlighted row.
            .focusable()
            .focused($sidebarFocused)
            .focusEffectDisabled()
            .onMoveCommand { direction in
                guard direction == .up || direction == .down else { return }
                let current = highlighted ?? filter.map { .filter($0) } ?? .scope(scope)
                highlighted = ListStep.step(from: current, in: choices, down: direction == .down)
            }
            .onKeyPress(.return) {
                guard let highlighted else { return .ignored }
                choose(highlighted)
                return .handled
            }
        }
        .background(Theme.sidebar)
        .removesComputer($removing)
    }


    /// The sidebar's rows as drawn, the ones the arrow keys walk.
    private var choices: [SidebarChoice] {
        [.scope(.active), .scope(.finished)]
            + machines.models.filter(\.showsFleet).map { .filter(.computer($0.machineID ?? "")) }
            + FleetGroup.groups(scope.rows(machines.fleet), by: .project).map { .filter(.project($0.id)) }
    }

    /// A row chosen, by a click or by Return on its highlight: the sidebar takes the keyboard from it.
    private func choose(_ choice: SidebarChoice) {
        switch choice {
        case .scope(let chosen):
            scope = chosen
            filter = nil
        case .filter(let chosen): filter = filter == chosen ? nil : chosen
        }
        highlighted = choice
        sidebarFocused = true
    }

    /// The arrow keys' highlight on a row, drawn as the pointer's: only while the sidebar has the keyboard.
    private func lit(_ choice: SidebarChoice) -> Color {
        sidebarFocused && highlighted == choice ? Theme.hover : .clear
    }

    private func nav(_ target: FleetScope, symbol: String) -> some View {
        Button { choose(.scope(target)) } label: {
            NavRow(symbol: symbol, title: target.title, count: target.rows(machines.fleet).count,
                selected: scope == target)
                .background(lit(.scope(target)), in: RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.hover(id: "sidebar.scope.\(target.rawValue.lowercased())"))
        .focusable(false)
        .accessibilityIdentifier("sidebar.scope.\(target.rawValue.lowercased())")
        .accessibilityAddTraits(scope == target ? .isSelected : [])
    }
}

/// A paired computer: its name, how many runs are active on it, and how its link is. A live one shows its CPU, memory
/// and disk as three quiet meters in place of the link's word, while the row is shown. One whose daemon refused its
/// token is a button: a click pairs it again, with its address already in the code form.
/// A computer the owner's tailnet lists that has not joined this Mac's list: its Tailscale name, why in the line where
/// a computer's load sits, and the one thing to do in the place of its count.
private struct UnjoinedComputerRow: View {
    let unjoined: Machines.UnjoinedComputer
    let act: () -> Void

    private var id: String { "sidebar.unjoined.\(unjoined.computer.host)" }

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 8) {
                Image(systemName: "desktopcomputer").font(.system(size: 11)).foregroundStyle(Theme.text3)
                    .frame(width: 16)
                    .accessibilityHidden(true)
                Text(unjoined.computer.name).font(Theme.ui(12.5)).foregroundStyle(Theme.text2)
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Button(PeerWords.tryAgain.titleStyle, action: act)
                    .buttonStyle(.hover(bare: true, id: "\(id).act"))
                    .font(Theme.ui(11.5))
                    .foregroundStyle(Theme.text2)
                    .accessibilityIdentifier("\(id).act")
            }
            Text(unjoined.reason)
                .font(Theme.ui(10.5))
                .foregroundStyle(Theme.text3)
                .lineLimit(2)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.leading, 24)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(id)
    }
}

private struct ComputerRow: View {
    let computer: AppModel
    let count: Int
    let selected: Bool
    /// Its "…" stands in its count's place, which hides while it shows, as a run's row does with its age.
    let roomForMore: Bool
    /// Narrows the list to this computer, or back to every run when it already is.
    let select: () -> Void
    let pairAgain: (String) -> Void

    private var address: String? { computer.endpoint?.typedAddress }
    private var id: String { "sidebar.computer.\(computer.machineID ?? "")" }

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

    var body: some View {
        let load = load
        Group {
            if computer.linkHealth.pairingCures, let address {
                Button { pairAgain(address) } label: { row(load).contentShape(Rectangle()) }
                    .buttonStyle(.hover(id: "\(id).pairAgain"))
                    .focusable(false)
                    .accessibilityIdentifier("\(id).pairAgain")
                    .accessibilityElement(children: .contain)
                    .accessibilityIdentifier(id)
            } else {
                Button(action: select) {
                    row(load).background(selected ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 6))
                }
                .buttonStyle(.hover(id: id))
                .focusable(false)
                .accessibilityIdentifier(id)
                .accessibilityAddTraits(selected ? .isSelected : [])
            }
        }
        .help([computer.machineSystem, computer.linkHealth.word, load?.line].compactMap { $0 }.joined(separator: " · "))
        .task { await computer.keepMachineStatsFresh() }
    }

    private func row(_ load: MachineLoad?) -> some View {
        VStack(alignment: .leading, spacing: load == nil ? 3 : 5) {
            HStack(spacing: 8) {
                Image(systemName: "desktopcomputer").font(.system(size: 11)).foregroundStyle(Theme.text2)
                    .frame(width: 16)
                    .accessibilityHidden(true)
                // The name takes all the line up to the count's column, less one gap.
                Text(computer.machineName ?? "Unknown computer").font(Theme.ui(12.5)).foregroundStyle(Theme.text)
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Text(verbatim: "\(count)").font(Theme.ui(11.5)).foregroundStyle(Theme.text3).monospacedDigit()
                    .frame(minWidth: RunMore.width, alignment: .trailing)
                    .opacity(roomForMore ? 0 : 1)
            }
            .alignmentGuide(.computerNameLine) { $0[VerticalAlignment.center] }
            if let load {
                HStack(spacing: 10) {
                    if let cpu = load.cpu { LoadMeter(label: "CPU", used: cpu) }
                    if let memory = load.memory { LoadMeter(label: "RAM", used: memory) }
                    if let disk = load.disk { LoadMeter(label: "Disk", used: disk) }
                }
                .padding(.leading, 24)
            } else {
                Text(computer.linkHealth.word)
                    .font(Theme.ui(10.5))
                    .foregroundStyle(Theme.text3)
                    .padding(.leading, 24)
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, load == nil ? 5 : 6)
    }
}

extension VerticalAlignment {
    private enum ComputerNameLine: AlignmentID {
        static func defaultValue(in context: ViewDimensions) -> CGFloat { context[VerticalAlignment.center] }
    }

    /// A computer row's name line, where its "…" sits.
    fileprivate static let computerNameLine = VerticalAlignment(ComputerNameLine.self)
}

/// One figure of a computer's load: its name, its share under it, and a thin bar, amber at 90% or more, when the
/// computer needs you, and quiet grey below that. The name sits above the share, not beside it, so "100%" keeps one
/// line in a meter as narrow as a sidebar with a scroll bar leaves it (about 41 pt, for 28 pt).
private struct LoadMeter: View {
    let label: String
    let used: Double

    var body: some View {
        let high = used >= 0.9
        VStack(alignment: .leading, spacing: 3) {
            VStack(alignment: .leading, spacing: 0) {
                Text(label).foregroundStyle(Theme.text3)
                Text(MachineLoad.percent(used)).foregroundStyle(high ? Theme.amber : Theme.text2)
            }
            .font(Theme.ui(9.5, .medium))
            .monospacedDigit()
            GeometryReader { geometry in
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.white.opacity(0.08))
                    Capsule().fill(high ? Theme.amber : Theme.text3).frame(width: geometry.size.width * used)
                }
            }
            .frame(height: 2.5)
        }
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
                // The sidebar is one stop for the keyboard; ⌘N and ⇧⌘N reach what this adds.
                .focusable(false)
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
                Text(FleetWords.noComputersYet).font(Theme.ui(15, .semibold)).foregroundStyle(Theme.text)
                Text(
                    "Add the Mac or Linux computer your agents run on. Its runs appear here, with what needs you marked."
                )
                .font(Theme.ui(12.5))
                .foregroundStyle(Theme.text2)
                .multilineTextAlignment(.center)
                .lineSpacing(3)
                .frame(maxWidth: 380)
                ActionButton(title: AddKind.computer.title, key: AddKind.computer.shortcut, prominent: true, id: "empty.addComputer") {
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
    /// Acts and cannot be taken back: the plain face in red, as every destructive button on the Mac is drawn. Never
    /// prominent, never Return.
    var destructive = false
    /// What a driver finds it by.
    let id: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                Text(title.titleStyle).font(Theme.ui(12, .medium))
                if let key { Text(key).font(Theme.ui(11)).opacity(0.6) }
            }
            .foregroundStyle(prominent ? Color.black : destructive ? Theme.red : Theme.text)
            .padding(.horizontal, 10)
            .frame(height: 26)
            .background(prominent ? Theme.text : Theme.raised, in: RoundedRectangle(cornerRadius: 6))
            .overlay(
                RoundedRectangle(cornerRadius: 6)
                    .strokeBorder(prominent ? Color.clear : destructive ? Theme.red.opacity(0.45) : Theme.hairline))
        }
        .buttonStyle(.hover(over: true, id: id))
        .accessibilityIdentifier(id)
    }
}
