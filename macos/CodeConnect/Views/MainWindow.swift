import CodeConnectKit
import SwiftUI

/// The main window: the run list (`FleetList`) down its leading side, and the run open beside it, or with none open the
/// start form (`StartPane`); or Settings in their place; before any computer is paired, what to do first in the list's
/// place. A run is started from the start form: the list head's + and File > New Agent (⌘N) leave Settings, close the
/// open run and give the form the keyboard. A computer is added from a sheet: the list head's computer menu and File >
/// Add Computer (⇧⌘N); an iPhone from File > Add iPhone.
struct MainWindow: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup
    /// The one "…" menu a row of the list opens, drawn over the whole window.
    @State private var rowMenu = RowMenu()
    @State private var adding: AddKind?
    /// The window's start of an agent, the app's, so a closed window opens on the same start.
    let startDraft: StartDraft
    /// The address Add a computer starts with, when a refused computer asked to pair again.
    @State private var repairing: String?
    /// The list, what it is narrowed to, and the run open beside it: the app's, so a closed window opens on them again.
    @Binding var scope: FleetScope
    /// The computer the list is narrowed to, if any.
    @Binding var filter: FleetFilter?
    /// The run open beside the list, by its key; none, and the list fills the window.
    @Binding var opened: String?
    /// What the File menu asked to add while the window was closed: the window opens its sheet on it.
    @Binding var pendingAdd: AddKind?
    /// Settings, in the panes' place, and its section; nil, and the window shows the agents.
    @Binding var settings: SettingsSection?
    /// The runs whose changes panel is open: each run keeps its own, opened or closed, as it was left.
    @State private var changesOpen: Set<String> = []
    /// The open run's changes panel stands in its conversation's place (`ChangesInPlace`): Esc gives the conversation
    /// back first.
    @State private var changesInPlace = false
    /// Whether the open run's composer is using Esc: its commands list is open or its field has focus.
    @FocusedValue(\.composerHoldsEscape) private var composerHoldsEscape
    /// Raised by Return in the run list: the open run's reply box takes the cursor.
    @State private var replyFocus = 0
    @Environment(ConfirmationQueue.self) private var asks
    /// The window's height, from which the room for a sheet is worked out.
    @State private var height: CGFloat = 0
    /// The app's updater, whose button stands at the title bar's trailing end while there is an update.
    @Environment(\.updates) private var updates
    /// How wide the update button stands, while it shows.
    @State private var updateWidth: CGFloat = 0
    @AppStorage(Panes.listHidden) private var listHidden = false
    @AppStorage(Panes.runHidden) private var runHidden = false

    init(
        scope: Binding<FleetScope>, filter: Binding<FleetFilter?>, opened: Binding<String?>, pendingAdd: Binding<AddKind?>,
        settings: Binding<SettingsSection?>, startDraft: StartDraft
    ) {
        _settings = settings
        self.startDraft = startDraft
        _scope = scope
        _filter = filter
        _opened = opened
        _pendingAdd = pendingAdd
        #if DEBUG
            _adding = State(
                initialValue: RenderWindow.opensOnIPhone
                    ? .iPhone : RenderWindow.opensOnAgent ? .agent : RenderWindow.opensAddComputer ? .computer : nil)
        #endif
    }

    var body: some View {
        HStack(spacing: 0) {
            if machines.showsFleet {
                let open = opened.flatMap { key in machines.fleet.first { $0.id == key } }
                let model = open.flatMap { machines.model(forSession: $0.id) }
                let showsRun = model != nil && !runHidden
                // The list hides only beside a run that shows: the window is never left empty.
                let showsList = !listHidden || !showsRun
                // With no run open, the start form is beside the list.
                let showsStart = open == nil
                if showsList {
                    FleetList(
                        scope: $scope, filter: $filter, opened: $opened, newRun: { adding = .agent },
                        addComputer: { adding = .computer }, openSettings: { settings = .general })
                        // Beside an open run, or the start form, the list is the grid's 256; alone, it fills the window.
                        .frame(width: showsRun || showsStart ? 256 : nil)
                        .environment(\.headerClearance, clearance(first: true, last: !showsRun && !showsStart))
                        // Its computer menu opens over the pane beside it.
                        .zIndex(1)
                        // The arrows open a run; Return then puts the cursor in its reply box, which is there only while
                        // the run's changes don't stand in its place.
                        .onKeyPress(.return) {
                            guard opened != nil, !changesInPlace else { return .ignored }
                            replyFocus += 1
                            return .handled
                        }
                }
                if let open, let model, showsRun {
                    if showsList { Rectangle().fill(Theme.hairline).frame(width: 1) }
                    SessionPane(row: open, model: model, changesOpen: changesOpen(of: open.id))
                        .id(open.id)
                        .environment(\.replyFocusRequest, replyFocus)
                        .environment(\.headerClearance, clearance(first: !showsList, last: true))
                } else if showsStart {
                    if showsList { Rectangle().fill(Theme.hairline).frame(width: 1) }
                    StartPane(draft: startDraft, addComputer: { adding = .computer }, started: started)
                }
            } else {
                EmptyFleet(adding: $adding)
            }
        }
        .onPreferenceChange(ChangesInPlace.self) { changesInPlace = $0 }
        // Settings is drawn over the agents, which stay as they were left (a draft, where a conversation was read, what
        // was open), hidden and out of reach, for the back row and Esc to return to.
        .opacity(settings == nil ? 1 : 0)
        .disabled(settings != nil)
        .accessibilityHidden(settings != nil)
        .overlay {
            if let section = settings {
                HStack(spacing: 0) {
                    SettingsList(section: Binding(get: { section }, set: { settings = $0 })) { settings = nil }
                        .frame(width: 216)
                    Rectangle().fill(Theme.hairline).frame(width: 1)
                    SettingsPage(section: section, add: { adding = .computer }, pairAgain: pairAgain)
                }
                .background(Theme.window)
            }
        }
        // The panes' toggles at the window's top edge: the list's beside the window's buttons, the run's at the right.
        // The list and the run hide only while a run is open.
        .overlay(alignment: .topLeading) {
            if settings == nil {
                PaneToggle(symbol: "sidebar.left", shown: !listHidden || !runIsOpen,
                    title: listHidden ? "Show List" : "Hide List", id: "pane.toggle.list") {
                    listHidden.toggle()
                    if listHidden { runHidden = false }
                }
                .disabled(!runIsOpen)
                .padding(.leading, 84)
                .padding(.top, 13)
            }
        }
        // The update button is the window's, over Settings too; the run's toggle only over the panes.
        .overlay(alignment: .topTrailing) {
            HStack(spacing: Self.updateGap) {
                if let updates, let state = updates.state {
                    UpdateButton(state: state) { updates.press() }
                        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { updateWidth = $0 }
                }
                if settings == nil {
                    PaneToggle(symbol: "sidebar.right", shown: runIsOpen && !runHidden,
                        title: runHidden ? "Show Run" : "Hide Run", id: "pane.toggle.run") {
                        runHidden.toggle()
                        if runHidden { listHidden = false }
                    }
                    .disabled(!runIsOpen)
                }
            }
            // The toggle's height: the button stands centred on it.
            .frame(height: 24)
            .padding(.trailing, 12)
            .padding(.top, 13)
        }
        .focusedSceneValue(\.runIsOpen, runIsOpen)
        .focusedSceneValue(\.settingsShown, settings != nil)
        // While an ask or a sheet covers the window, Settings… waits for it.
        .focusedSceneValue(\.windowCovered, asks.isAsking || adding != nil || setup.shown)
        .background(Theme.window)
        .ignoresSafeArea()
        .onGeometryChange(for: CGFloat.self) {
            $0.size.height + $0.safeAreaInsets.top + $0.safeAreaInsets.bottom
        } action: {
            height = $0
        }
        .toolbar(removing: .title)
        .toolbarBackgroundVisibility(.hidden, for: .windowToolbar)
        .background(WindowChrome())
        .rowMenuHost(rowMenu)
        // Whether the agents are on screen, for what they do only while seen (`agentsShown`): outside the row menu's
        // host, which closes with every other panel.
        .environment(\.agentsShown, settings == nil)
        .focusedSceneValue(\.adding, $adding)
        // New Agent is the start form in the main area: Settings and the open run close, and the form takes the
        // keyboard. Before any computer is added there is no form: New Agent adds a computer first.
        .onChange(of: adding, initial: true) { _, kind in
            guard kind == .agent else { return }
            guard machines.showsFleet else {
                adding = .computer
                return
            }
            adding = nil
            settings = nil
            opened = nil
            startDraft.requestKeyboard()
        }
        .onChange(of: pendingAdd != nil, initial: true) {
            guard let pendingAdd else { return }
            adding = pendingAdd
            self.pendingAdd = nil
        }
        // Until every computer has caught up, or been waited out, the list says it is connecting.
        .task { await machines.waitOutLaunch() }
        .onChange(of: opened) {
            if opened != nil { settings = nil }
            // A run opened shows, even with its pane hidden; with none open, the list shows again.
            if opened != nil { runHidden = false } else { listHidden = false }
            // The start's own run, opened from the list as it launches: the start is done, so New Agent shows a fresh
            // form rather than the start's progress opening it again.
            if let opened, let id = startDraft.sent?.requestID,
                case .started(let uid, _)? = machines.models.lazy.compactMap({ $0.starter.starts[id] }).first?.outcome,
                uid == opened
            {
                startDraft.didOpen()
            }
        }
        // A computer removed, here or from Settings: the list lets go of its narrowing to it and of its run, which
        // has left the list with it.
        .onChange(of: machines.models.compactMap(\.machineID)) { before, after in
            let removed = Set(before).subtracting(after)
            guard !removed.isEmpty else { return }
            if case .computer(let id) = filter, removed.contains(id) { filter = nil }
            if let opened, !machines.fleet.contains(where: { $0.id == opened }) { self.opened = nil }
        }
        // Esc closes the open run, or first gives its conversation back where its changes stand in its place; a sheet's own
        // Esc is its Cancel, and an ask's its safe answer. Not while the composer or a field of the changes is using it:
        // Esc closes its list, or leaves its field, first. Nor while the run's shell has the keyboard: there Esc is the
        // program's (an editor's own), and a window shortcut would take it before the shell saw it.
        .background {
            // Esc in Settings goes back to the agents, unless an ask or a sheet holds it.
            if settings != nil, adding == nil, !asks.isAsking {
                Button("Back") { settings = nil }
                    .keyboardShortcut(.cancelAction)
                    .accessibilityIdentifier("settings.close")
                    .opacity(0)
                    .allowsHitTesting(false)
            } else if let opened, changesInPlace, adding == nil, composerHoldsEscape != true, !Shells.shared.holdsKeys,
                !asks.isAsking
            {
                Button("Conversation") { changesOpen.remove(opened) }
                    .keyboardShortcut(.cancelAction)
                    .accessibilityIdentifier("session.changes.close")
                    .opacity(0)
                    .allowsHitTesting(false)
            } else if opened != nil, adding == nil, composerHoldsEscape != true, !Shells.shared.holdsKeys,
                !asks.isAsking
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
                guard let after = RenderWindow.settingsAfter else { return }
                try? await Task.sleep(for: .seconds(after))
                settings = .general
            }
            .task {
                guard RenderWindow.resizeAfter > 0 else { return }
                try? await Task.sleep(for: .seconds(RenderWindow.resizeAfter))
                guard let window = NSApp.windows.first(where: \.canBecomeMain) else { return }
                var frame = window.frame
                frame.size.width += 40
                window.setFrame(frame, display: true)
            }
            .task {
                // A test launch's fleet is in place a moment after the window is.
                try? await Task.sleep(for: .seconds(1))
                if RenderWindow.filter == "computer" {
                    filter = FleetScope.active.rows(machines.fleet).first?.machineID.map { .computer($0) }
                }
            }
        #endif
        .sheet(item: $adding, onDismiss: { repairing = nil }) { kind in
            AddSheet(kind: kind, address: $repairing)
            // A sheet stands in the middle of the window: clear of the 52-point header above it, so as far from the
            // bottom edge.
            .environment(\.sheetRoom, height - 2 * 52)
                #if DEBUG
                    .renderLook()
                #endif
        }
    }

    /// The run just started is open, in the list of every active agent, unfiltered.
    private func started(_ run: String) {
        startDraft.didOpen()
        scope = .active
        filter = nil
        opened = run
    }

    /// Whether a run's changes panel is open.
    private func changesOpen(of run: String) -> Binding<Bool> {
        Binding(
            get: { changesOpen.contains(run) },
            set: { if $0 { changesOpen.insert(run) } else { changesOpen.remove(run) } })
    }

    /// A refused computer paired again: the Add Computer sheet, its address already in the code form.
    private func pairAgain(_ address: String) {
        repairing = address
        adding = .computer
    }
}

extension MainWindow {
    /// A run is open beside the list, whether or not its pane shows.
    fileprivate var runIsOpen: Bool {
        opened.flatMap { key in machines.fleet.first { $0.id == key } }.flatMap { machines.model(forSession: $0.id) } != nil
    }

    /// Where a pane's header keeps clear of the window's own controls, one gap on both sides: its text and controls
    /// start 16 pt after the list's toggle beside the window's buttons (which ends 112 pt in) when the pane comes first, and
    /// end 16 pt before the run's toggle (which starts 40 pt in), and before the update button beside it while that
    /// shows, when it comes last. Only the header band moves; the pane's content keeps its own inset, under the hairline
    /// that closes the band.
    fileprivate func clearance(first: Bool, last: Bool) -> EdgeInsets {
        let update = updates?.state == nil ? 0 : updateWidth + Self.updateGap
        return EdgeInsets(top: 0, leading: first ? 128 : 0, bottom: 0, trailing: last ? 56 + update : 0)
    }

    /// The update button's gap before the run's toggle.
    fileprivate static let updateGap: CGFloat = 10
}

extension FocusedValues {
    /// What the main window is adding: the File menu's New Agent opens its start form; Add Computer and Add iPhone its
    /// sheet.
    @Entry var adding: Binding<AddKind?>?
}

/// File > New Agent (⌘N), Add Computer (⇧⌘N) and Add iPhone, in place of New Window: the app has one window,
/// and these open its start form or its sheet, opening the window first when it is closed or behind another.
/// Unavailable while its sheet is open.
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

/// What the list shows before there is anything in it: what CodeConnect needs, and the way to
/// give it.
private struct EmptyFleet: View {
    @Binding var adding: AddKind?

    var body: some View {
        VStack(spacing: 0) {
            // The header band, empty: the window's buttons and the list's toggle stand in it.
            Color.clear.frame(height: 52)
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
            // A container: flat under the window, the hidden image above was still read by VoiceOver.
            .accessibilityElement(children: .contain)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .background(Theme.window)
    }
}

/// A button with its key on a cap, where one reaches it: white and solid for the one thing to do, a faint white
/// for the rest.
struct ActionButton: View {
    let title: String
    let key: String?
    let prominent: Bool
    /// Acts and cannot be taken back: the plain face with its words in red, as every destructive button on the Mac is
    /// drawn. Never prominent, never Return.
    var destructive = false
    /// A title the button is as wide as, too: a button whose word changes (Trust, Don't Trust) keeps one width.
    var sizedFor: String? = nil
    /// The small face, for a band's own row (a panel's tabs): 22 pt tall, 11.5 pt words.
    var small = false
    /// What a driver finds it by.
    let id: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 7) {
                ZStack {
                    if let sizedFor { Text(sizedFor.titleStyle).font(Theme.ui(small ? 11.5 : 12.5, .medium)).hidden() }
                    Text(title.titleStyle).font(Theme.ui(small ? 11.5 : 12.5, .medium))
                }
                if let key { KeyCap(key, onLight: prominent) }
            }
            .foregroundStyle(prominent ? Color.black : destructive ? Theme.red : Theme.text)
            .padding(.horizontal, small ? 8 : 10)
            .frame(height: small ? 22 : 26)
            .background(prominent ? Theme.text : Color.white.opacity(0.08), in: RoundedRectangle(cornerRadius: 6))
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(prominent ? Color.clear : Color.white.opacity(0.06)))
        }
        // The faint face shows the hover's fill through it; the white one, opaque, lightens instead.
        .buttonStyle(.hover(over: prominent, id: id))
        .accessibilityIdentifier(id)
    }
}
