import AppKit
import CodeConnectKit
import Sparkle
import SwiftUI

@main
struct CodeConnectApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    /// No push registrar: notifications on the Mac need a wire and relay change no step has built.
    @State private var machines: Machines
    @State private var setup: Setup
    #if !DEBUG
        /// Sparkle's standard updater, started with the app: its daily check, and its own window when a newer
        /// CodeConnect is out. Release only, so no test launch, render or rig ever asks codeconnect.sh.
        private let updater = SPUStandardUpdaterController(
            startingUpdater: true, updaterDelegate: nil, userDriverDelegate: nil)
    #endif

    /// `machines` is made here, after a test launch has chosen its keychain and its data folder,
    /// because making it reads the one and writes the other; and after the pairing with this Mac's own
    /// daemon has followed it to where it answers now, so the app never dials an address it left.
    init() {
        #if DEBUG
            RenderWindow.isolateTestLaunch()
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
    }

    var body: some Scene {
        // The app's one window: no New Window, and no window tabs.
        Window("CodeConnect", id: "main") {
            RootView()
                .environment(machines)
                .environment(setup)
                .preferredColorScheme(.dark)
                .tint(CC.color.accent)
                .task {
                    machines.bootstrap()
                    if Setup.runsThisLaunch { await setup.run(machines: machines) }
                }
        }
        .defaultSize(width: 1280, height: 820)
        .windowResizability(.contentMinSize)
        .commands {
            AddCommands()
            CommandGroup(after: .appInfo) {
                #if !DEBUG
                    Button("Check for Updates\u{2026}") { updater.checkForUpdates(nil) }
                #endif
                Button("Set Up CodeConnect\u{2026}") { setup.reopen(machines: machines) }
            }
        }
        // CodeConnect > Settings… (⌘,): this computer's devices.
        Settings {
            DevicesPane()
                .environment(machines)
                .environment(setup)
                .preferredColorScheme(.dark)
                .tint(CC.color.accent)
                #if DEBUG
                    .renderLook()
                #endif
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

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

/// The main window, paired or not.
struct RootView: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup
    #if DEBUG
        @Environment(\.openSettings) private var openSettings
    #endif

    var body: some View {
        MainWindow()
            .frame(minWidth: 960, minHeight: 520)
            .environment(machines.linkModel)
            .sheet(isPresented: Bindable(setup).shown) { SetupView().environment(setup).environment(machines) }
            #if DEBUG
                .renderLook()
                // A render of Settings photographs that window, never this one.
                .background(
                    RenderWindow(
                        ready: !RenderWindow.opensSettings
                            && (!RenderWindow.waitsForComputer || machines.pairingError != nil || connected)
                            && (!Setup.runsThisLaunch || setup.settled),
                        showsSheet: (RenderWindow.pairing == nil ? RenderWindow.opensAddComputer : !connected)
                            || setup.shown))
                .task { if RenderWindow.opensSettings { openSettings() } }
            #endif
    }

    #if DEBUG
        /// A render waiting for a computer is photographed once it has connected, without the sheet
        /// a pairing that went through closes.
        private var connected: Bool { machines.linkModel.connection.phase.isConnected }
    #endif
}
