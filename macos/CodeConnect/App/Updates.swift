import AppKit
import CodeConnectKit
import Sparkle
import SwiftUI

/// Sparkle's updater with the app's own window: Sparkle checks the feed, verifies, downloads and installs as it always
/// does, and asks this user driver to show each step; each is drawn in the app's own look (`UpdateSheet`) in a window
/// of its own, never in Sparkle's standard windows. A window of its own, as Sparkle's are, so a step shows whatever the
/// main window is doing: closed behind an open Settings window, or holding a sheet of its own. Started with a Release
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
    }

    private(set) var step: Step?
    /// The version found, for the steps after it.
    private(set) var version = ""
    @ObservationIgnored private var updater: SPUUpdater?
    @ObservationIgnored private var window: NSWindow?
    /// Where the window's top left stands while it is shown: kept for every step, whatever its height.
    @ObservationIgnored private var topLeft: NSPoint?

    /// The updater for this launch: a Release app's, and a Debug app's only when `-CC_UPDATE_FEED` names its feed, so no
    /// test launch, render or rig otherwise asks codeconnect.sh.
    static func forThisLaunch() -> Updates? {
        #if DEBUG
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
        #if DEBUG
            if RenderWindow.updateStep != nil { updater.checkForUpdates() }
        #endif
        return updates
    }

    func checkForUpdates() { updater?.checkForUpdates() }

    private func present(_ next: Step?) {
        #if DEBUG
            // A render keeps the step it photographs, whatever Sparkle does next.
            if let held = step?.name, held == RenderWindow.updateStep { return }
        #endif
        step = next
        guard let next else {
            window?.orderOut(nil)
            return
        }
        let window = window ?? makeWindow()
        self.window = window
        #if DEBUG
            if RenderWindow.isRendering {
                if next.name == RenderWindow.updateStep { RenderWindow.place(window) { RenderWindow.renderAndQuit(window) } }
                return
            }
        #endif
        if !window.isVisible { topLeft = Self.topLeft(width: window.frame.width, besides: window) }
        window.makeKeyAndOrderFront(nil)
        keepTopLeft()
    }

    private func keepTopLeft() {
        if let topLeft { window?.setFrameTopLeftPoint(topLeft) }
    }

    /// Centred over the main window, or the screen when it is closed, with its top where the tallest step's would be
    /// when centred: every step opens at the same place.
    private static func topLeft(width: CGFloat, besides window: NSWindow) -> NSPoint {
        let over = NSApp.windows.first { $0 !== window && $0.isVisible && $0.title == "CodeConnect" }?.frame
            ?? NSScreen.main?.visibleFrame ?? .zero
        return NSPoint(x: over.midX - width / 2, y: over.midY + UpdateSheet.tallest / 2)
    }

    /// The steps' window: the sheet's look, with no title bar of its own, sized by what it shows. It closes by a choice.
    private func makeWindow() -> NSWindow {
        // Under the transparent title bar, as a sheet has none: the content starts at its own top padding.
        let content = NSHostingController(
            rootView: UpdateSheet(updates: self).ignoresSafeArea().preferredColorScheme(.dark).tint(CC.color.accent))
        // The window takes each step's own size: the notes' step is tall, the others are not.
        content.sizingOptions = [.preferredContentSize]
        let window = NSWindow(contentViewController: content)
        window.styleMask = [.titled, .fullSizeContentView]
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.isMovableByWindowBackground = true
        window.backgroundColor = NSColor(Theme.sheet)
        window.title = "Software Update"
        window.isReleasedWhenClosed = false
        // A step of another height keeps the window's top where it was.
        NotificationCenter.default.addObserver(forName: NSWindow.didResizeNotification, object: window, queue: .main) {
            [weak self] _ in MainActor.assumeIsolated { self?.keepTopLeft() }
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
        #if DEBUG
            if RenderWindow.updateStep == "downloading" { reply(.install) }
        #endif
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
        #if DEBUG
            // A render of the download stops it there: the installer Sparkle starts next runs outside the test launch.
            if RenderWindow.updateStep == "downloading" { Task { cancellation() } }
        #endif
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

/// An update step, drawn as the setup checklist is: a title and a line under it, the step's body, and its buttons on a
/// footer under a hairline.
struct UpdateSheet: View {
    let updates: Updates

    /// The tallest step's height: the one with the release's notes.
    static let tallest: CGFloat = 400

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
                .padding(.bottom, 16)
                content(step)
                Color.clear.frame(height: 16)
                VStack(spacing: 0) {
                    Rectangle().fill(Theme.hairline).frame(height: 1)
                    HStack(spacing: 8) { buttons(step) }
                        .padding(.horizontal, 14)
                        .frame(height: 52)
                }
            }
            .frame(width: 480, height: hasNotes(step) ? Self.tallest : nil)
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
            ScrollView {
                Text(notes).font(Theme.ui(12)).foregroundStyle(Theme.text2)
                    .lineSpacing(3)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .textSelection(.enabled)
                    .padding(14)
            }
            .background(Theme.raised.opacity(0.6), in: RoundedRectangle(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.hairline))
            .padding(.horizontal, 24)
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
            Spacer()
        }
    }

    private func hasNotes(_ step: Updates.Step) -> Bool {
        if case .found(_, let notes, _, _) = step { !notes.isEmpty } else { false }
    }

    private func bytes(_ count: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: count), countStyle: .file)
    }
}
