import AppKit
import CodeConnectKit
import Sparkle
import SwiftUI

/// Sparkle's updater with the app's own window: Sparkle checks the feed, verifies, downloads and installs as it always
/// does, and asks this user driver to show each step; each is drawn in the app's own look (`UpdateSheet`) in a window
/// of its own, never in Sparkle's standard windows. A window of its own, as Sparkle's are, so a step shows whatever the
/// main window is doing: closed, on Settings, or holding a sheet of its own. Started with a Release
/// app: its daily check, and Check for Updates… in the app menu.
@MainActor @Observable
final class Updates: NSObject, SPUUserDriver, SPUUpdaterDelegate {
    /// What the sheet shows; nil while nothing is asked.
    enum Step {
        case checking(cancel: () -> Void)
        /// `installing`: the update was already prepared, so Install relaunches into it at once.
        case found(version: String, notes: String, installing: Bool, reply: (SPUUserUpdateChoice) -> Void)
        case downloading(received: UInt64, expected: UInt64, cancel: () -> Void)
        case extracting(progress: Double)
        case ready(reply: (SPUUserUpdateChoice) -> Void)
        case installing
        case upToDate(text: String, done: () -> Void)
        case failed(text: String, done: () -> Void)

        /// The step's name, as a test launch asks for it and a driver finds the sheet by.
        var name: String {
            switch self {
            case .checking: "checking"
            case .found: "found"
            case .downloading: "downloading"
            case .extracting: "extracting"
            case .ready: "ready"
            case .installing: "installing"
            case .upToDate: "upToDate"
            case .failed: "failed"
            }
        }

        /// Its bar moves for as long as it is shown.
        var moves: Bool {
            switch self {
            case .checking, .installing: true
            default: false
            }
        }
    }

    private(set) var step: Step?
    /// The version found, for the steps after it.
    private(set) var version = ""
    @ObservationIgnored private var updater: SPUUpdater?
    @ObservationIgnored private var window: NSWindow?
    /// Where the window's centre stands while it is shown: every step is centred on it, whatever its size.
    @ObservationIgnored private var centre: NSPoint?

    /// The updater for this launch: a Release app's, and a Debug app's only when `-CC_UPDATE_FEED` names its feed, so no
    /// test launch, render or rig otherwise asks codeconnect.sh.
    static func forThisLaunch() -> Updates? {
        #if DEBUG
            if let name = RenderWindow.updateStep {
                let updates = Updates()
                Task { await updates.render(name) }
                return updates
            }
            guard RenderWindow.updateFeed != nil else { return nil }
        #endif
        let updates = Updates()
        let updater = SPUUpdater(hostBundle: .main, applicationBundle: .main, userDriver: updates, delegate: updates)
        updates.updater = updater
        do {
            try updater.start()
        } catch {
            updates.present(.failed(text: error.localizedDescription) { [weak updates] in updates?.present(nil) })
        }
        return updates
    }

    func checkForUpdates() { updater?.checkForUpdates() }

    private func present(_ next: Step?) {
        step = next
        guard let next else {
            window?.orderOut(nil)
            return
        }
        let window = window ?? makeWindow()
        self.window = window
        if !window.isVisible { centre = Self.centre(besides: window) }
        #if DEBUG
            if let main = RenderWindow.placedMain {
                window.order(.above, relativeTo: main.windowNumber)
                keepCentred()
                RenderWindow.renderAndQuit(main, over: window, still: !next.moves)
                return
            }
        #endif
        window.makeKeyAndOrderFront(nil)
        keepCentred()
    }

    #if DEBUG
        /// A render's step, with fixed values in place of Sparkle's, shown once the render has placed the main window.
        private func render(_ name: String) async {
            while RenderWindow.placedMain == nil { try? await Task.sleep(for: .milliseconds(50)) }
            version = "0.19.2"
            let none: () -> Void = {}
            let choice: (SPUUserUpdateChoice) -> Void = { _ in }
            let shown: Step? =
                switch name {
                case "checking": .checking(cancel: none)
                case "found": .found(version: version, notes: RenderWindow.updateNotes, installing: false, reply: choice)
                case "downloading": .downloading(received: 12_400_000, expected: 31_000_000, cancel: none)
                case "extracting": .extracting(progress: 0.6)
                case "ready": .ready(reply: choice)
                case "installing": .installing
                case "upToDate": .upToDate(text: "CodeConnect 0.19.1 is currently the newest version available.", done: none)
                case "failed":
                    .failed(text: "An error occurred while downloading the update. Please try again later.", done: none)
                default: nil
                }
            guard let shown else {
                FileHandle.standardError.write(Data("render: no update step named \(name)\n".utf8))
                exit(2)
            }
            present(shown)
        }
    #endif

    /// Centres the window on `centre` at its size now: when it opens, and each time a step gives it another size.
    private func keepCentred() {
        guard let window, let centre else { return }
        window.setFrameOrigin(NSPoint(x: centre.x - window.frame.width / 2, y: centre.y - window.frame.height / 2))
    }

    /// The main window's centre, or the screen's when it is closed.
    private static func centre(besides window: NSWindow) -> NSPoint {
        let over = NSApp.windows.first { $0 !== window && $0.isVisible && $0.title == "CodeConnect" }?.frame
            ?? NSScreen.main?.visibleFrame ?? .zero
        return NSPoint(x: over.midX, y: over.midY)
    }

    /// The steps' window: the sheet's look, with no title bar of its own, sized by what it shows. It closes by a choice.
    private func makeWindow() -> NSWindow {
        let content = NSHostingController(
            rootView: ThemeRoot(content: UpdateSheet(updates: self)).preferredColorScheme(.dark).tint(CC.color.accent))
        // The window takes each step's own size: the notes' step is tall, the others are not. Under the transparent
        // title bar, as a sheet has none: no safe area, so the content starts at its own top padding and the window is
        // exactly its height.
        content.sizingOptions = [.preferredContentSize]
        content.safeAreaRegions = []
        let window = NSWindow(contentViewController: content)
        window.styleMask = [.titled, .fullSizeContentView]
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.isMovableByWindowBackground = true
        window.backgroundColor = NSColor(Theme.sheet)
        window.title = "Software Update"
        window.isReleasedWhenClosed = false
        NotificationCenter.default.addObserver(forName: NSWindow.didResizeNotification, object: window, queue: .main) {
            [weak self] _ in MainActor.assumeIsolated { self?.keepCentred() }
        }
        return window
    }

    #if DEBUG
        nonisolated func feedURLString(for updater: SPUUpdater) -> String? {
            MainActor.assumeIsolated { RenderWindow.updateFeed }
        }
    #endif

    // MARK: SPUUserDriver

    /// Info.plist turns the daily check on, so Sparkle never asks; if it does, the answer is the same.
    func show(_ request: SPUUpdatePermissionRequest, reply: @escaping (SUUpdatePermissionResponse) -> Void) {
        reply(SUUpdatePermissionResponse(automaticUpdateChecks: true, sendSystemProfile: false))
    }

    func showUserInitiatedUpdateCheck(cancellation: @escaping () -> Void) {
        present(.checking(cancel: cancellation))
    }

    func showUpdateFound(with appcastItem: SUAppcastItem, state: SPUUserUpdateState,
                         reply: @escaping (SPUUserUpdateChoice) -> Void) {
        version = appcastItem.displayVersionString
        present(.found(version: version, notes: Self.notes(appcastItem.itemDescription),
                       installing: state.stage == .installing, reply: answered(reply)))
    }

    /// The feed carries its notes in the item (appcast.sh), so there are none to fetch.
    func showUpdateReleaseNotes(with downloadData: SPUDownloadData) {}
    func showUpdateReleaseNotesFailedToDownloadWithError(_ error: any Error) {}

    func showUpdateNotFoundWithError(_ error: any Error, acknowledgement: @escaping () -> Void) {
        let error = error as NSError
        present(.upToDate(text: error.localizedRecoverySuggestion ?? error.localizedDescription, done: done(acknowledgement)))
    }

    func showUpdaterError(_ error: any Error, acknowledgement: @escaping () -> Void) {
        present(.failed(text: error.localizedDescription, done: done(acknowledgement)))
    }

    func showDownloadInitiated(cancellation: @escaping () -> Void) {
        present(.downloading(received: 0, expected: 0, cancel: cancellation))
    }

    func showDownloadDidReceiveExpectedContentLength(_ expectedContentLength: UInt64) {
        guard case .downloading(_, _, let cancel) = step else { return }
        present(.downloading(received: 0, expected: expectedContentLength, cancel: cancel))
    }

    func showDownloadDidReceiveData(ofLength length: UInt64) {
        guard case .downloading(let received, let expected, let cancel) = step else { return }
        present(.downloading(received: received + length, expected: expected, cancel: cancel))
    }

    func showDownloadDidStartExtractingUpdate() { present(.extracting(progress: 0)) }

    func showExtractionReceivedProgress(_ progress: Double) { present(.extracting(progress: progress)) }

    func showReady(toInstallAndRelaunch reply: @escaping (SPUUserUpdateChoice) -> Void) {
        present(.ready(reply: answered(reply)))
    }

    func showInstallingUpdate(withApplicationTerminated applicationTerminated: Bool,
                              retryTerminatingApplication: @escaping () -> Void) {
        present(.installing)
    }

    func showUpdateInstalledAndRelaunched(_ relaunched: Bool, acknowledgement: @escaping () -> Void) {
        present(nil)
        acknowledgement()
    }

    func dismissUpdateInstallation() { present(nil) }

    func showUpdateInFocus() { NSApp.activate() }

    /// A choice closes the sheet, and Sparkle shows what comes next.
    private func answered(_ reply: @escaping (SPUUserUpdateChoice) -> Void) -> (SPUUserUpdateChoice) -> Void {
        { [weak self] choice in
            self?.present(nil)
            reply(choice)
        }
    }

    private func done(_ acknowledgement: @escaping () -> Void) -> () -> Void {
        { [weak self] in
            self?.present(nil)
            acknowledgement()
        }
    }

    /// The release's notes as text: appcast.sh writes them as one paragraph with `&`, `<` and `>` escaped.
    static func notes(_ html: String?) -> String {
        (html ?? "").replacing(/<[^>]*>/, with: "")
            .replacingOccurrences(of: "&lt;", with: "<").replacingOccurrences(of: "&gt;", with: ">")
            .replacingOccurrences(of: "&amp;", with: "&")
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

/// An update step, drawn as the setup checklist is, at its width: a title and a line under it, the step's body, and its
/// buttons on a footer under a hairline. A step with no buttons has no footer; one with no body ends at the line.
struct UpdateSheet: View {
    let updates: Updates

    var body: some View {
        if let step = updates.step {
            VStack(alignment: .leading, spacing: 0) {
                VStack(alignment: .leading, spacing: 6) {
                    Text(title(step)).font(Theme.ui(17, .semibold)).foregroundStyle(Theme.text)
                    Text(line(step)).font(Theme.ui(12.5)).foregroundStyle(Theme.text2)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(.horizontal, 24)
                .padding(.top, 22)
                .padding(.bottom, hasBody(step) || hasFooter(step) ? 16 : 22)
                if hasBody(step) {
                    content(step)
                    Color.clear.frame(height: hasFooter(step) ? 16 : 22)
                }
                if hasFooter(step) {
                    VStack(spacing: 0) {
                        Rectangle().fill(Theme.hairline).frame(height: 1)
                        // At the content's edges, as every sheet's footer is.
                        HStack(spacing: 8) { buttons(step) }
                            .padding(.horizontal, 24)
                            .frame(height: 52)
                    }
                }
            }
            .frame(width: 560)
            .background(Theme.sheet)
            .accessibilityElement(children: .contain)
            .accessibilityIdentifier("update.\(step.name)")
            #if DEBUG
                .renderLook()
            #endif
        }
    }

    private var current: String { Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "" }

    private func title(_ step: Updates.Step) -> String {
        switch step {
        case .checking: "Checking for updates"
        case .found(let version, _, _, _): "CodeConnect \(version) is available"
        case .downloading: "Downloading CodeConnect \(updates.version)"
        case .extracting: "Preparing the update"
        case .ready: "Ready to install"
        case .installing: "Installing the update"
        case .upToDate: "CodeConnect is up to date"
        case .failed: "The update didn’t work"
        }
    }

    private func line(_ step: Updates.Step) -> String {
        switch step {
        case .checking: "Looking for a newer version of CodeConnect."
        case .found: "You have \(current). Install replaces this copy and opens the new one."
        case .downloading(let received, let expected, _):
            expected > 0 ? "\(bytes(received)) of \(bytes(expected))" : "Starting the download…"
        case .extracting: "Checking and unpacking CodeConnect \(updates.version)."
        case .ready: "CodeConnect quits, installs \(updates.version) and opens again."
        case .installing: "CodeConnect opens again when it is done."
        case .upToDate(let text, _), .failed(let text, _): text
        }
    }

    @ViewBuilder private func content(_ step: Updates.Step) -> some View {
        switch step {
        case .found(_, let notes, _, _) where !notes.isEmpty:
            NotesBox(notes: notes).padding(.horizontal, 24)
        case .checking, .installing:
            ProgressView().progressViewStyle(.linear).padding(.horizontal, 24)
        case .downloading(let received, let expected, _):
            ProgressView(value: Double(min(received, expected)), total: Double(max(expected, 1))).padding(.horizontal, 24)
        case .extracting(let progress):
            ProgressView(value: progress).padding(.horizontal, 24)
        default:
            EmptyView()
        }
    }

    @ViewBuilder private func buttons(_ step: Updates.Step) -> some View {
        switch step {
        case .checking(let cancel), .downloading(_, _, let cancel):
            Spacer()
            ActionButton(title: "Cancel", key: nil, prominent: false, id: "update.cancel", action: cancel)
                .keyboardShortcut(.cancelAction)
        case .found(_, _, let installing, let reply):
            ActionButton(title: "Skip This Version", key: nil, prominent: false, id: "update.skip") { reply(.skip) }
            Spacer()
            ActionButton(title: "Later", key: nil, prominent: false, id: "update.later") { reply(.dismiss) }
                .keyboardShortcut(.cancelAction)
            ActionButton(title: installing ? "Install and Relaunch" : "Install Update", key: nil, prominent: true,
                         id: "update.install") { reply(.install) }
                .keyboardShortcut(.defaultAction)
        case .ready(let reply):
            Spacer()
            ActionButton(title: "Later", key: nil, prominent: false, id: "update.later") { reply(.dismiss) }
                .keyboardShortcut(.cancelAction)
            ActionButton(title: "Install and Relaunch", key: nil, prominent: true, id: "update.install") { reply(.install) }
                .keyboardShortcut(.defaultAction)
        case .upToDate(_, let done), .failed(_, let done):
            Spacer()
            ActionButton(title: "OK", key: nil, prominent: true, id: "update.ok", action: done)
                .keyboardShortcut(.defaultAction)
        case .extracting, .installing:
            EmptyView()
        }
    }

    private func hasBody(_ step: Updates.Step) -> Bool {
        switch step {
        case .checking, .downloading, .extracting, .installing: true
        case .found: hasNotes(step)
        case .ready, .upToDate, .failed: false
        }
    }

    private func hasFooter(_ step: Updates.Step) -> Bool {
        switch step {
        case .extracting, .installing: false
        default: true
        }
    }

    private func hasNotes(_ step: Updates.Step) -> Bool {
        if case .found(_, let notes, _, _) = step { !notes.isEmpty } else { false }
    }

    private func bytes(_ count: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: count), countStyle: .file)
    }
}

/// The release's notes in a raised box as tall as they are, up to `lines` lines; longer notes scroll, and the box then
/// shows whole lines at rest, its padding the same above and below.
private struct NotesBox: View {
    let notes: String
    private static let lines = 12
    @State private var textHeight: CGFloat = 0
    @State private var oneLine: CGFloat = 0
    @State private var twoLines: CGFloat = 0

    private func text(_ string: String) -> some View {
        Text(string).font(Theme.ui(12)).foregroundStyle(Theme.text2).lineSpacing(3)
    }

    /// The text's height in the box: all of it, or the most whole lines that fit.
    private var shown: CGFloat {
        let most = oneLine + CGFloat(Self.lines - 1) * (twoLines - oneLine)
        return oneLine > 0 ? min(textHeight, most) : textHeight
    }

    var body: some View {
        ScrollView {
            text(notes)
                .frame(maxWidth: .infinity, alignment: .leading)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { textHeight = $0 }
        }
        .scrollBounceBehavior(.basedOnSize)
        // The padding is outside the scroll, so the lines shown end where the box's padding starts.
        .frame(height: shown)
        .padding(14)
        .background(alignment: .topLeading) {
            // One line and two, measured in the notes' own type: the step from one line to the next.
            ZStack {
                text("A").fixedSize().onGeometryChange(for: CGFloat.self) { $0.size.height } action: { oneLine = $0 }
                text("A\nA").fixedSize().onGeometryChange(for: CGFloat.self) { $0.size.height } action: { twoLines = $0 }
            }
            .hidden()
        }
        .background(Theme.raised.opacity(0.6), in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.hairline))
    }
}
