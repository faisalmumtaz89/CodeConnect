#if DEBUG
    import AppKit
    import CodeConnectKit
    import ScreenCaptureKit
    import SwiftUI

    /// The render harness, inside the app: launched with `-CC_RENDER_WINDOW <png-path>`, the app
    /// photographs its own window, title bar included, into that PNG once the screen is `ready`,
    /// then quits. `-CC_RENDER_WINDOW_SIZE <width>x<height>` sets the window's content size first.
    /// `-CC_RENDER_PAIR "<address> <code>"` submits that pairing at launch, as typed into the form,
    /// and the screen is ready once the computer has answered. An attached sheet is in the photograph
    /// where it sits, composed by the window server with the window.
    ///
    /// Launch it with `open -g` and never activate it: the window is placed at the back of the
    /// window stack before it becomes visible, so it takes no focus and never covers anything. It
    /// is photographed by the window server (ScreenCaptureKit, the app's own windows, which needs
    /// no permission), so materials such as the sidebar's glass are drawn as they really are.
    /// Controls draw as in the key window (`renderLook()`); the window's own buttons and title,
    /// which follow whether the app is really active, draw as in an inactive one.
    struct RenderWindow: NSViewRepresentable {
        var ready = true
        /// Whether the photograph has a sheet in it: Add, or the setup checklist.
        var showsSheet = false

        /// True when this launch is taking a render.
        static var isRendering: Bool { UserDefaults.standard.string(forKey: "CC_RENDER_WINDOW") != nil }

        /// Whether the app was ever the active one during this render. A render must never take
        /// focus: the app cannot be activated (`keepInactive`), and the render says if it was.
        @MainActor private(set) static var everActive = false

        /// Called before any window exists: a render's app can never become the active one, and
        /// any moment it was is recorded.
        @MainActor static func keepInactive() {
            guard isRendering else { return }
            NSApp.setActivationPolicy(.prohibited)
            NotificationCenter.default.addObserver(
                forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main
            ) { _ in
                MainActor.assumeIsolated { everActive = true }
            }
        }

        /// `-CC_ADD_COMPUTER YES`: the window opens its Add-a-computer sheet at launch. A render waits
        /// for that sheet, and for the one `-CC_RENDER_PAIR` opens.
        static var opensAddComputer: Bool {
            UserDefaults.standard.bool(forKey: "CC_ADD_COMPUTER") || opensOnIPhone || opensOnAgent || pairing != nil
        }

        /// `-CC_ADD_IPHONE YES`: the same sheet, on its iPhone side.
        static var opensOnIPhone: Bool { UserDefaults.standard.bool(forKey: "CC_ADD_IPHONE") }

        /// `-CC_ADD_AGENT YES`: the same sheet, on its Agent side.
        static var opensOnAgent: Bool { UserDefaults.standard.bool(forKey: "CC_ADD_AGENT") }

        /// `-CC_RENDER_FOLDER <folder>`: Start an agent chooses that recent folder once listed, and starts nothing.
        static var chosenFolder: String? { UserDefaults.standard.string(forKey: "CC_RENDER_FOLDER") }
        /// `-CC_PICK_FOLDER <path>`: Start an agent's Choose a folder… takes that path in place of the folder panel, a
        /// window a test launch never opens.
        static var pickedFolder: String? { UserDefaults.standard.string(forKey: "CC_PICK_FOLDER") }
        /// `-CC_MODEL_MENU YES`: Start an agent opens with its model menu open.
        static var opensModelMenu: Bool { UserDefaults.standard.bool(forKey: "CC_MODEL_MENU") }
        /// `-CC_BRANCH_MENU YES`: the start form's Branch menu open.
        static var opensBranchMenu: Bool { UserDefaults.standard.bool(forKey: "CC_BRANCH_MENU") }
        /// `-CC_BRANCH_PLACE <branch>`: the start form's place, the repository's worktree on that branch.
        /// `-CC_BRANCH_NAME <name>`: a new worktree being named, as typed.
        static func branch(in git: StartGit?, place: inout StartPlace, newName: inout String?) {
            let defaults = UserDefaults.standard
            if let branch = defaults.string(forKey: "CC_BRANCH_PLACE"),
                let checkout = git?.others(than: "").first(where: { $0.branch == branch })
            {
                place = .existing(checkout)
            }
            if let name = defaults.string(forKey: "CC_BRANCH_NAME") { newName = name }
        }
        /// `-CC_TOOL_INPUT YES`: a Claude card opens its "Full tool input".
        static var opensToolInput: Bool { UserDefaults.standard.bool(forKey: "CC_TOOL_INPUT") }
        /// `-CC_FIELD_UNFOCUSED YES`: an opened run's reply field is not given the keyboard, as when the run is opened
        /// by a click on its row; a request then holds the reply box's place.
        static var leavesFieldUnfocused: Bool { UserDefaults.standard.bool(forKey: "CC_FIELD_UNFOCUSED") }
        /// `-CC_RUN_MENU <session uid>`: that run's "…", once shown (`-CC_HOVER fleet.row.<uid>`), opens its menu a
        /// moment later, as a click does; the open menu's frame, its control's and the window's are printed once
        /// (`panel: …; control: …; window: …`).
        static var openRunMenu: String? { UserDefaults.standard.string(forKey: "CC_RUN_MENU") }
        /// `-CC_LIST_END YES`: the run list opens scrolled to its end.
        static var listAtEnd: Bool { UserDefaults.standard.bool(forKey: "CC_LIST_END") }
        /// `-CC_CONFIRM endRun|endNote|remove|removeNote|clear|mode|computer|device|batch|batchRun`: that ask or notice is up at launch,
        /// about the first run listed, the open run (`-CC_OPEN`), the last computer, or the first device Settings lists.
        static var confirm: String? { UserDefaults.standard.string(forKey: "CC_CONFIRM") }
        /// `-CC_SELECT all|<n>`: the Ended runs open choosing, with all or the first `n` removable runs chosen; with
        /// `-CC_CONFIRM batch` the one ask for them is up, with `-CC_CONFIRM batchRun` they are removed at once.
        static var select: Int?? {
            guard let value = UserDefaults.standard.string(forKey: "CC_SELECT") else { return nil }
            return .some(Int(value))
        }
        /// `-CC_COMPUTER_MENU YES`: a computer's "…", once shown (`-CC_HOVER sidebar.computer.<id>`), opens its menu a
        /// moment later, as a click does.
        static var opensComputerMenu: Bool { UserDefaults.standard.bool(forKey: "CC_COMPUTER_MENU") }

        /// `-CC_SETTINGS YES`: the Settings window opens at launch, and it is the window photographed, once its pane
        /// has its list: Devices, or the pane `-CC_SETTINGS_TAB devices|folders` names.
        static var opensSettings: Bool {
            UserDefaults.standard.bool(forKey: "CC_SETTINGS") || devices != nil || folders != nil
                || UserDefaults.standard.string(forKey: "CC_SETTINGS_TAB") != nil
        }
        static var settingsTab: SettingsTab {
            UserDefaults.standard.string(forKey: "CC_SETTINGS_TAB").flatMap(SettingsTab.init(rawValue:))
                ?? (folders != nil ? .folders : .devices)
        }

        /// `-CC_FOLDERS <folders>`: the `folder_trust` frame's `folders` every computer is taken to answer, read from
        /// the arguments as given; Settings opens on Folders.
        static var folders: [FolderAnswer]? {
            let arguments = ProcessInfo.processInfo.arguments
            guard let flag = arguments.firstIndex(of: "-CC_FOLDERS"), flag + 1 < arguments.count else { return nil }
            return try? JSONDecoder().decode([FolderAnswer].self, from: Data(arguments[flag + 1].utf8))
        }

        /// `-CC_SETUP_ROWS "service=missing:<sentence>;claude=ready:Ready;codex=checking;path=missing:<sentence>"`: the
        /// checklist opens with these answers, settled, and runs nothing.
        static var setupRows: [String: Setup.Row]? {
            guard let text = UserDefaults.standard.string(forKey: "CC_SETUP_ROWS") else { return nil }
            var rows: [String: Setup.Row] = [:]
            for entry in text.split(separator: ";") {
                let parts = entry.split(separator: "=", maxSplits: 1).map(String.init)
                guard parts.count == 2 else { continue }
                let state = parts[1].split(separator: ":", maxSplits: 1).map(String.init)
                let sentence = state.count == 2 ? state[1] : ""
                rows[parts[0]] =
                    switch state[0] {
                    case "ready": .ready(sentence)
                    case "missing": .missing(sentence)
                    default: .checking
                    }
            }
            return rows
        }

        /// `-CC_DEVICES <frame>`: the `devices` frame this Mac's daemon is taken to answer (a recorded one, from
        /// fixtures/wire/devices.json), read from the arguments as given; Settings opens on it.
        /// `-CC_DEVICES_ASK block|allow`: with the first allowed or blocked entry's question open.
        static var devices: [PairedDevice]? {
            let arguments = ProcessInfo.processInfo.arguments
            guard let flag = arguments.firstIndex(of: "-CC_DEVICES"), flag + 1 < arguments.count,
                case .devices(let devices)? = try? JSONDecoder().decode(DeviceReply.self, from: Data(arguments[flag + 1].utf8))
            else { return nil }
            return devices
        }
        static var devicesAsk: String? { UserDefaults.standard.string(forKey: "CC_DEVICES_ASK") }

        /// `-CC_PEERS <frame>`: the `peers` frame this Mac's daemon is taken to have sent last (a recorded one,
        /// from fixtures/wire/peers.json), in place of the one it sends. Read from the arguments as given.
        static var peers: PeerDirectory? {
            let arguments = ProcessInfo.processInfo.arguments
            guard let flag = arguments.firstIndex(of: "-CC_PEERS"), flag + 1 < arguments.count else { return nil }
            return try? JSONDecoder().decode(PeerDirectory.self, from: Data(arguments[flag + 1].utf8))
        }

        /// `-CC_LOOPBACK YES`: this Mac's daemon is taken to answer devices only at loopback, as before Tailscale:
        /// the Tailscale note reads it so, and Add an iPhone has no code an iPhone could reach.
        static var onLoopback: Bool { UserDefaults.standard.bool(forKey: "CC_LOOPBACK") }

        /// `-CC_MACHINE_STATS <frame>`: the `machine_stats` frame every computer is taken to have answered last (one
        /// serialised by ccd), in place of its own; a row still shows it only while its link is live. Read from the
        /// arguments as given.
        static var machineStats: MachineStats? {
            let arguments = ProcessInfo.processInfo.arguments
            guard let flag = arguments.firstIndex(of: "-CC_MACHINE_STATS"), flag + 1 < arguments.count else { return nil }
            return try? JSONDecoder().decode(MachineStats.self, from: Data(arguments[flag + 1].utf8))
        }

        /// `-CC_FLEET_SCOPE finished`: the list opens on the runs that have ended.
        static var fleetScope: FleetScope {
            FleetScope(rawValue: UserDefaults.standard.string(forKey: "CC_FLEET_SCOPE") ?? "") ?? .active
        }

        /// `-CC_FLEET_GROUPING project` or `computer`: the list opens grouped so, whatever this Mac keeps, and keeps
        /// nothing of it; nil when not given.
        static var fleetGrouping: FleetGroup.Kind? {
            switch UserDefaults.standard.string(forKey: "CC_FLEET_GROUPING") {
            case "project": .project
            case "computer": .computer
            default: nil
            }
        }

        /// `-CC_FILTER computer` or `project`: the list opens narrowed to the sidebar's first computer or project.
        static var filter: String? { UserDefaults.standard.string(forKey: "CC_FILTER") }

        /// `-CC_FOLDS_OPEN YES`: every diff opens with its folded stretches of unchanged lines opened, as clicks open them.
        static var foldsOpen: Bool { UserDefaults.standard.bool(forKey: "CC_FOLDS_OPEN") }
        /// `-CC_CONVERSATION_UP <points>`: a moment after the open run's conversation opens, the reader scrolls up that
        /// far from its end, as a reader's scroll does.
        static var conversationUp: CGFloat? {
            UserDefaults.standard.object(forKey: "CC_CONVERSATION_UP") == nil
                ? nil : CGFloat(UserDefaults.standard.double(forKey: "CC_CONVERSATION_UP"))
        }
        /// `-CC_HUNK menu|comment|sent`: the diff's first hunk header in the Changes tab, a moment after it is placed,
        /// opens its menu, or its comment field, or says its comment was sent.
        static var hunk: String? { UserDefaults.standard.string(forKey: "CC_HUNK") }
        @MainActor static var hunkActed = false
        /// `-CC_HUNK_DRAFT <text>`: the comment field opens holding these words.
        static var hunkDraft: String? { UserDefaults.standard.string(forKey: "CC_HUNK_DRAFT") }
        /// `-CC_HUNK_REFUSED <reason>`: the comment field opens as after a send refused for this reason.
        static var hunkRefusal: String? { UserDefaults.standard.string(forKey: "CC_HUNK_REFUSED") }

        /// `-CC_OPEN <key>`: the run with this key opens beside the list.
        static var opened: String? { UserDefaults.standard.string(forKey: "CC_OPEN") }
        /// `-CC_SESSION_TAB changes`: the opened run shows its Changes.
        static var sessionTab: SessionTab? {
            UserDefaults.standard.string(forKey: "CC_SESSION_TAB") == "changes" ? .changes : nil
        }
        /// `-CC_SLASH_LIT <row>`: with a `/` reply, the slash palette lights this row, as ↓ would.
        static var slashLit: Int { UserDefaults.standard.integer(forKey: "CC_SLASH_LIT") }
        /// `-CC_REPLY <text>`: the opened run's reply box holds these words, as typed (`\n` for a new line).
        static var reply: String? {
            UserDefaults.standard.string(forKey: "CC_REPLY")?.replacingOccurrences(of: "\\n", with: "\n")
        }
        /// The cold launch's splash for a render (`LaunchSurface`): `-CC_LAUNCH_T <seconds>` draws it at that moment;
        /// `-CC_LAUNCH_SETTLE <seconds>` is when the fleet is taken to have caught up (0: at once), drawn as a live
        /// launch draws it: the hold until then, the exit from then; `-CC_LAUNCH_RECORD <folder>` plays it a frame at a
        /// time, 30 a second, photographing the window into launch-<000…>.png there from
        /// `-CC_LAUNCH_RECORD_FROM <seconds>`, then quits.
        static var launchT: Double? {
            UserDefaults.standard.object(forKey: "CC_LAUNCH_T") == nil ? nil : UserDefaults.standard.double(forKey: "CC_LAUNCH_T")
        }
        static var launchSettle: Double? {
            UserDefaults.standard.object(forKey: "CC_LAUNCH_SETTLE") == nil
                ? nil : UserDefaults.standard.double(forKey: "CC_LAUNCH_SETTLE")
        }
        static var launchRecord: String? { UserDefaults.standard.string(forKey: "CC_LAUNCH_RECORD") }
        static var launchRecordFrom: Double { UserDefaults.standard.double(forKey: "CC_LAUNCH_RECORD_FROM") }
        /// A render shows the splash only when it asks for it; every other render opens on the app.
        static var showsLaunch: Bool { launchT != nil || launchRecord != nil }

        /// `-CC_PANEL model|mode|more`: the opened run's settings panel of that name is open.
        static var panel: SettingsPanel? {
            switch UserDefaults.standard.string(forKey: "CC_PANEL") {
            case "model": .model
            case "mode": .mode
            case "more": .more
            default: nil
            }
        }

        /// `-CC_UPDATE_FEED <url>`: the updater starts, with this feed in place of the app's; it checks when asked
        /// (Check for Updates…).
        static var updateFeed: String? { UserDefaults.standard.string(forKey: "CC_UPDATE_FEED") }
        /// `-CC_UPDATE_STEP <name>`: the update window shows the step of this name (`Updates.Step.name`) with fixed values,
        /// over the main window once a render has placed it, and the render photographs the two. No updater starts.
        /// `-CC_UPDATE_NOTES <text>`: the release notes the `found` step shows.
        static var updateStep: String? { UserDefaults.standard.string(forKey: "CC_UPDATE_STEP") }
        static var updateNotes: String { UserDefaults.standard.string(forKey: "CC_UPDATE_NOTES") ?? "" }

        /// The main window, once a render has placed it.
        @MainActor private(set) static var placedMain: NSWindow?

        /// `-CC_HOVER <id>,<id>`: the buttons with these identifiers draw as if the pointer were on them, since a test
        /// launch has no pointer.
        static var hovered: Set<String> {
            Set((UserDefaults.standard.string(forKey: "CC_HOVER") ?? "").split(separator: ",").map(String.init))
        }

        /// `-CC_FORCE <id>=pressed,<id>=disabled`: the buttons with these identifiers draw pressed, or disabled, since a
        /// test launch can neither hold a click nor reach every state that disables a control.
        static var forced: [String: String] {
            Dictionary(
                (UserDefaults.standard.string(forKey: "CC_FORCE") ?? "").split(separator: ",").compactMap { pair in
                    let parts = pair.split(separator: "=", maxSplits: 1).map(String.init)
                    return parts.count == 2 ? (parts[0], parts[1]) : nil
                }, uniquingKeysWith: { $1 })
        }

        /// The address and code `-CC_RENDER_PAIR` asks the pairing form to submit.
        static var pairing: (address: String, code: String)? {
            guard let words = UserDefaults.standard.string(forKey: "CC_RENDER_PAIR")?.split(separator: " "),
                words.count == 2
            else { return nil }
            return (String(words[0]), String(words[1]))
        }

        /// True when the render shows a real computer, one it pairs with (`-CC_RENDER_PAIR`): it is ready
        /// once that computer has answered, connected or refused.
        static var waitsForComputer: Bool { pairing != nil }

        /// Where a test launch keeps the app's data (its cache) instead of the owner's Library; nil for
        /// an ordinary launch.
        private(set) static var appData: URL?
        /// True when `appData` is this launch's own, removed when its render ends.
        private static var throwaway = false

        /// The keychain and the data folder a test launch uses, chosen before anything reads or writes
        /// either (`CodeConnectApp.init`), from the launch's arguments, never from saved defaults. Any
        /// `-CC_` flag means no keychain at all and a throwaway data folder, removed when the render
        /// ends, and an installation id of its own, never written to the app's defaults. An ordinary
        /// launch keeps the app's own keychain, data and id.
        static func isolateTestLaunch() {
            guard ProcessInfo.processInfo.arguments.contains(where: { $0.hasPrefix("-CC_") }) else { return }
            Keychain.service = nil
            let id = UUID().uuidString
            appData = FileManager.default.temporaryDirectory
                .appendingPathComponent("codeconnect-test-\(id)", isDirectory: true)
            throwaway = true
            UserDefaults.standard.register(defaults: [DeviceIdentity.key: id])
            FileHandle.standardError.write(Data("test launch: keychain none, data \(appData?.path ?? "")\n".utf8))
        }

        /// Removes a throwaway data folder: a render's, as it quits.
        static func removeThrowawayData() {
            guard throwaway, let appData else { return }
            try? FileManager.default.removeItem(at: appData)
        }

        func makeNSView(context: Context) -> Probe { Probe() }
        func updateNSView(_ probe: Probe, context: Context) {
            probe.showsSheet = showsSheet
            probe.ready = ready
        }

        final class Probe: NSView {
            var ready = false {
                didSet { renderIfReady() }
            }
            var showsSheet = false
            private var placed = false
            private var started = false
            private var backdrop: NSWindow?

            override func viewDidMoveToWindow() {
                super.viewDidMoveToWindow()
                guard let window, RenderWindow.isRendering, !placed else { return }
                // Invisible until it is behind everything else on the screen.
                window.alphaValue = 0
                DispatchQueue.main.async {
                    // After SwiftUI has named the window for saving its frame: a render leaves no
                    // frame behind for the next launch to restore.
                    window.setFrameAutosaveName("")
                    if let size = UserDefaults.standard.string(forKey: "CC_RENDER_WINDOW_SIZE")?.split(separator: "x")
                        .compactMap({ Double($0) }), size.count == 2
                    {
                        window.setContentSize(NSSize(width: size[0], height: size[1]))
                    }
                    // Wholly on the screen: the part of a window past the screen's edge is not
                    // drawn, and the photograph would show nothing there.
                    if let visible = window.screen?.visibleFrame {
                        window.setFrameTopLeftPoint(NSPoint(x: visible.minX, y: visible.maxY))
                    }
                    window.orderBack(nil)
                    // Glass and other materials blur what is behind a window. Behind this one is a
                    // plain window of one colour, the app's own, so a render shows the same thing
                    // every time and never what else is on the screen.
                    let backdrop = NSWindow(
                        contentRect: window.frame, styleMask: [.borderless], backing: .buffered, defer: false)
                    backdrop.isReleasedWhenClosed = false
                    backdrop.hasShadow = false
                    backdrop.ignoresMouseEvents = true
                    backdrop.backgroundColor = NSColor(srgbRed: 0.11, green: 0.11, blue: 0.12, alpha: 1)
                    backdrop.order(.below, relativeTo: window.windowNumber)
                    self.backdrop = backdrop
                    window.alphaValue = 1
                    RenderWindow.placedMain = window
                    self.placed = true
                    self.renderIfReady()
                }
            }

            private func renderIfReady() {
                guard ready, placed, !started, let window, UserDefaults.standard.string(forKey: "CC_RENDER_WINDOW") != nil
                else { return }
                // The sheet an option opens at launch is attached a moment after the window shows,
                // and the one a pairing closes takes a moment to go.
                if (window.attachedSheet != nil) != showsSheet {
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) { self.renderIfReady() }
                    return
                }
                started = true
                // A start still reading its folder's repository (`-CC_GIT_PENDING`) shows a turning ring, which never
                // stops changing.
                RenderWindow.renderAndQuit(window, still: !UserDefaults.standard.bool(forKey: "CC_GIT_PENDING"))
            }
        }

        /// Photographs `window`, with `over` in front of it, into the render's PNG, then quits: 0 once written, 1 if it
        /// could not be. `still`: the photograph waits for a frame that stops changing; a step with a moving bar never
        /// stops, so it is taken once the window has had a moment to draw. A window `over` has its frame printed beside
        /// the main window's.
        /// What this launch drew wrongly in a way no picture shows: said at once, and a render with any fails.
        @MainActor private(set) static var faults: [String] = []

        @MainActor static func fault(_ what: String) {
            faults.append(what)
            FileHandle.standardError.write(Data("fault: \(what)\n".utf8))
        }

        @MainActor static func renderAndQuit(_ window: NSWindow, over: NSWindow? = nil, still: Bool = true) {
            guard let path = UserDefaults.standard.string(forKey: "CC_RENDER_WINDOW") else { return }
            Task { @MainActor in
                // `-CC_RENDER_DELAY <seconds>`: what a screen shows a while after it is ready.
                try? await Task.sleep(for: .seconds(UserDefaults.standard.double(forKey: "CC_RENDER_DELAY")))
                do {
                    guard faults.isEmpty else { throw RenderFailure(faults.joined(separator: "; ")) }
                    let rep = try await RenderWindow.photograph(window, over: over, still: still)
                    guard let png = rep.representation(using: .png, properties: [:]) else {
                        throw RenderFailure("could not encode the PNG")
                    }
                    try png.write(to: URL(fileURLWithPath: path))
                    let active = RenderWindow.everActive || NSApp.isActive
                    if let over { print("frames: main \(window.frame) over \(over.frame)") }
                    print("render: \(rep.pixelsWide)x\(rep.pixelsHigh) -> \(path) active=\(active)")
                    RenderWindow.removeThrowawayData()
                    exit(0)
                } catch {
                    FileHandle.standardError.write(Data("render: \(error)\n".utf8))
                    RenderWindow.removeThrowawayData()
                    exit(1)
                }
            }
        }

        private struct RenderFailure: Error, CustomStringConvertible {
            let description: String
            init(_ description: String) { self.description = description }
        }

        /// The window as the screen shows it, with its attached sheet where it sits: the window
        /// server composes this app's windows in the window's frame, and nothing else on the screen.
        @MainActor static func photograph(_ window: NSWindow, over: NSWindow? = nil, still: Bool = true) async throws
            -> NSBitmapImageRep
        {
            if let sheet = window.attachedSheet { try await settle { sheet.frame } }
            if let over { try await settle { over.frame } }
            let windows = [window] + (window.attachedSheet.map { [$0] } ?? []) + (over.map { [$0] } ?? [])
            var last: Data?
            for attempt in 0..<40 {
                let content = try await SCShareableContent.currentProcess
                let ids = Set(windows.map { CGWindowID($0.windowNumber) })
                let included = content.windows.filter { ids.contains($0.windowID) }
                guard included.count == windows.count else { throw RenderFailure("the window is not on screen") }
                guard let screen = window.screen,
                    let display = content.displays.first(where: {
                        $0.displayID == screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? CGDirectDisplayID
                    })
                else { throw RenderFailure("the window's display is not available") }
                let filter = SCContentFilter(display: display, including: included)
                let config = SCStreamConfiguration()
                // The window's frame in the display's points, measured from its top left.
                config.sourceRect = CGRect(
                    x: window.frame.minX - screen.frame.minX, y: screen.frame.maxY - window.frame.maxY,
                    width: window.frame.width, height: window.frame.height)
                config.width = Int(window.frame.width * window.backingScaleFactor)
                config.height = Int(window.frame.height * window.backingScaleFactor)
                config.showsCursor = false
                config.backgroundColor = .clear
                let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
                let bytes = image.dataProvider?.data as Data?
                if let bytes, bytes == last || (!still && attempt == 5) { return NSBitmapImageRep(cgImage: image) }
                last = bytes
                try await Task.sleep(for: .milliseconds(100))
            }
            throw RenderFailure("the window never stopped changing")
        }

        /// Waits until a value stops changing: a sheet's frame once it has slid into place.
        @MainActor private static func settle(_ value: () -> NSRect) async throws {
            var last = value()
            for _ in 0..<40 {
                try await Task.sleep(for: .milliseconds(50))
                let now = value()
                if now == last { return }
                last = now
            }
            throw RenderFailure("the sheet never came to rest")
        }
    }

    extension View {
        /// In a render, controls draw as they do in the key window. Apply it to a window's root and
        /// to each sheet's content: a sheet is a window of its own.
        func renderLook() -> some View { modifier(RenderLook()) }
    }

    private struct RenderLook: ViewModifier {
        func body(content: Content) -> some View {
            if RenderWindow.isRendering {
                content.environment(\.controlActiveState, .key)
            } else {
                content
            }
        }
    }
#endif
