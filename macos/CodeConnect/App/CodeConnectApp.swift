import AppKit
import CodeConnectKit
import SwiftUI

@main
struct CodeConnectApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    /// No push registrar: notifications on the Mac need a wire and relay change no step has built.
    @State private var machines: Machines
    @State private var setup: Setup
    /// Sparkle's updater, with the app's own update sheet: started with the app.
    @State private var updates: Updates?
    @State private var settingsTab = SettingsTab.devices

    /// `machines` is made here, after a test launch has chosen its keychain and its data folder,
    /// because making it reads the one and writes the other; and after the pairing with this Mac's own
    /// daemon has followed it to where it answers now, so the app never dials an address it left.
    init() {
        #if DEBUG
            RenderWindow.isolateTestLaunch()
            _settingsTab = State(initialValue: RenderWindow.settingsTab)
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
                if let updates {
                    Button("Check for Updates\u{2026}") { updates.checkForUpdates() }
                }
                Button("Set Up CodeConnect\u{2026}") { setup.reopen(machines: machines) }
            }
        }
        // CodeConnect > Settings… (⌘,): this computer's devices, and the folders' answers to the folder question.
        Settings {
            TabView(selection: $settingsTab) {
                DevicesPane().tabItem { Label("Devices", systemImage: "iphone") }.tag(SettingsTab.devices)
                FoldersPane().tabItem { Label(FolderWords.paneTitle, systemImage: "folder") }.tag(SettingsTab.folders)
            }
            .confirmationHost()
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

/// The Settings window's panes.
enum SettingsTab: String {
    case devices, folders
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
            .confirmationHost()
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
                            && (!Setup.runsThisLaunch || setup.settled)
                            // A render of an update step photographs the update's own window.
                            && RenderWindow.updateStep == nil,
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
