import AppKit
import CodeConnectKit
import SwiftUI

@main
struct CodeConnectApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    /// No push registrar: notifications on the Mac need a wire and relay change no step has built.
    @State private var machines: Machines
    @State private var setup: Setup
    /// Sparkle's updater, shown as the update button alone: started with the app.
    @State private var updates: Updates?
    /// Settings, in the main window in the sidebar's place, and its section: kept while the window is closed.
    @State private var settings: SettingsSection?
    /// Help › CodeConnect Support.
    private static let support = URL(string: "https://codeconnect.sh/support")!
    /// Where the main window was, kept while it is closed: it opens again on the same list and run.
    @State private var scope = FleetScope.active
    @State private var filter: FleetFilter?
    @State private var opened: String?
    @State private var pendingAdd: AddKind?
    /// The window's start of an agent: what is chosen and typed, and the start sent.
    @State private var startDraft = StartDraft()
    @State private var started = false

    /// `machines` is made here, after a test launch has chosen its keychain and its data folder,
    /// because making it reads the one and writes the other; and after the pairing with this Mac's own
    /// daemon has followed it to where it answers now, so the app never dials an address it left.
    init() {
        #if DEBUG
            RenderWindow.isolateTestLaunch()
            _settings = State(initialValue: RenderWindow.settingsSection)
            _scope = State(initialValue: RenderWindow.fleetScope)
            _opened = State(initialValue: RenderWindow.opened)
        #endif
        let installer = Installer.forThisLaunch()
        let pairing = PairingStore()
        if let installer { Setup.followThisMac(pairing, root: installer.root) }
        #if DEBUG
            _machines = State(initialValue: Machines(pairing: pairing, cache: EventCache(root: RenderWindow.appData)))
        #else
            _machines = State(initialValue: Machines(pairing: pairing))
        #endif
        _setup = State(initialValue: Setup(installer: installer, pairing: pairing))
        _updates = State(initialValue: Updates.forThisLaunch())
    }

    var body: some Scene {
        // The app's one window: no New Window, and no window tabs.
        Window("CodeConnect", id: "main") {
            ThemeRoot(content: RootView(
                scope: $scope, filter: $filter, opened: $opened, pendingAdd: $pendingAdd, settings: $settings,
                startDraft: startDraft))
                .environment(machines)
                .environment(setup)
                .preferredColorScheme(.dark)
                .tint(CC.color.accent)
                .environment(\.updates, updates)
                // Once a launch, not once a window: the window opened again keeps the computers as they are.
                .task {
                    guard !started else { return }
                    started = true
                    machines.bootstrap()
                    if Setup.runsThisLaunch { await setup.run(machines: machines) }
                }
        }
        .defaultSize(width: 1280, height: 820)
        .windowResizability(.contentMinSize)
        .commands {
            AddCommands(pending: $pendingAdd)
            SettingsCommands(settings: $settings)
            PaneCommands()
            // View › Refresh: every computer asked for its runs again, as the iPhone's pull to refresh asks.
            CommandGroup(after: .sidebar) {
                Button("Refresh") { machines.refreshFleet() }
                    .keyboardShortcut("r")
            }
            CommandGroup(after: .appInfo) {
                if let updates {
                    Button("Check for Updates\u{2026}") { updates.checkForUpdates() }
                }
                Button("Set Up CodeConnect\u{2026}") { setup.reopen(machines: machines) }
            }
            // The app ships no Help book, so Help › CodeConnect Help would only say help isn't available: Help holds
            // the support page instead.
            CommandGroup(replacing: .help) {
                Button("CodeConnect Support") { NSWorkspace.shared.open(Self.support) }
            }
        }
    }
}

/// Dark only, as the iPhone app is: pinned for the whole app, so menus, alerts and sheets the
/// system draws are dark too, whatever the Mac is set to.
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApp.appearance = NSAppearance(named: .darkAqua)
        #if DEBUG
            RenderWindow.keepInactive()
        #endif
    }

    /// Closing the main window leaves the app running in the Dock; ⌘Q quits. (An app whose one window scene is a
    /// `Window` quits with it otherwise.)
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
}

/// The main window, paired or not.
struct RootView: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup
    @Binding var scope: FleetScope
    @Binding var filter: FleetFilter?
    @Binding var opened: String?
    @Binding var pendingAdd: AddKind?
    @Binding var settings: SettingsSection?
    let startDraft: StartDraft

    var body: some View {
        MainWindow(
            scope: $scope, filter: $filter, opened: $opened, pendingAdd: $pendingAdd, settings: $settings,
            startDraft: startDraft)
            .confirmationHost()
            .frame(minWidth: 960, minHeight: 520)
            // A cold launch's splash, over the window from its first frame: until it is over, no control under it
            // draws a focus ring.
            .focusEffectDisabled(!LaunchSurface.state.over)
            .overlay { LaunchSurface() }
            .environment(machines.linkModel)
            // Set Up waits for the splash: its intro, then a cut to the sheet.
            .sheet(isPresented: Binding(get: { setup.shown && LaunchSurface.state.over }, set: { setup.shown = $0 })) {
                SetupView().environment(setup).environment(machines)
            }
            #if DEBUG
                .renderLook()
                // A render of Devices or Folders is photographed by that pane once its list is in.
                .background(
                    RenderWindow(
                        // A recording of the splash photographs its own frames and quits.
                        ready: RenderWindow.launchRecord == nil
                            && settings != .devices && settings != .folders
                            && (!RenderWindow.waitsForComputer || machines.pairingError != nil || connected)
                            && (!Setup.runsThisLaunch || setup.settled),
                        showsSheet: (RenderWindow.pairing == nil ? RenderWindow.opensAddComputer : !connected)
                            || setup.shown))
            #endif
    }

    #if DEBUG
        /// A render waiting for a computer is photographed once it has connected, without the sheet
        /// a pairing that went through closes.
        private var connected: Bool { machines.linkModel.connection.phase.isConnected }
    #endif
}
