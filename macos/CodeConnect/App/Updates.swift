import AppKit
import Sparkle
import SwiftUI

/// Sparkle's updater, shown as one button and nothing else: no window, no notes, no progress. Sparkle checks the feed,
/// verifies, downloads and installs as it always does; this user driver keeps only what the button needs. An update
/// found shows the button (Update); pressed, it stands disabled (Downloading…) while Sparkle downloads and prepares the
/// update, then reads Restart to Update; pressed again, Sparkle installs it and opens the new copy. A check that finds nothing, or
/// fails, says nothing; a failed download lights Update again. Started with a Release app: its daily check, and Check
/// for Updates… in the app menu, which drives the same button.
@MainActor @Observable
final class Updates: NSObject, SPUUserDriver, SPUUpdaterDelegate {
    /// What the button shows; nil while there is no update.
    enum State: String {
        case available
        case downloading
        case ready
        case installing

        var title: String {
            switch self {
            case .available: "Update"
            case .downloading: "Downloading…"
            case .ready, .installing: "Restart to Update"
            }
        }

        /// Pressed, and waiting on Sparkle.
        var working: Bool { self == .downloading || self == .installing }
    }

    private(set) var state: State?
    /// Sparkle's answer for the update it showed; spent once used, or when its session ends.
    @ObservationIgnored private var reply: ((SPUUserUpdateChoice) -> Void)?
    /// Pressed with no answer in hand: the check that press starts installs what it finds.
    @ObservationIgnored private var installsFound = false
    @ObservationIgnored private var updater: SPUUpdater?

    /// The updater for this launch: a Release app's, and a Debug app's only when `-CC_UPDATE_FEED` names its feed, so no
    /// test launch, render or rig otherwise asks codeconnect.sh. A Debug launch with `-CC_UPDATE_STATE` shows the button
    /// in that state, and no updater starts.
    static func forThisLaunch() -> Updates? {
        #if DEBUG
            if let shown = RenderWindow.updateState {
                let updates = Updates()
                updates.state = shown
                return updates
            }
            guard RenderWindow.updateFeed != nil else { return nil }
        #endif
        let updates = Updates()
        let updater = SPUUpdater(hostBundle: .main, applicationBundle: .main, userDriver: updates, delegate: updates)
        updates.updater = updater
        // An updater that cannot start leaves the button unshown for this launch, as a check that fails does.
        try? updater.start()
        return updates
    }

    func checkForUpdates() { updater?.checkForUpdates() }

    /// The button: Update downloads and prepares the update, Restart to Update installs it and opens the new copy.
    func press() {
        guard let state, !state.working else { return }
        self.state = state == .available ? .downloading : .installing
        if let reply {
            self.reply = nil
            reply(.install)
        } else {
            installsFound = true
            checkForUpdates()
        }
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

    func showUserInitiatedUpdateCheck(cancellation: @escaping () -> Void) {}

    func showUpdateFound(with appcastItem: SUAppcastItem, state: SPUUserUpdateState,
                         reply: @escaping (SPUUserUpdateChoice) -> Void) {
        if installsFound {
            installsFound = false
            reply(.install)
            return
        }
        self.reply = reply
        self.state = state.stage == .notDownloaded ? .available : .ready
    }

    func showUpdateReleaseNotes(with downloadData: SPUDownloadData) {}
    func showUpdateReleaseNotesFailedToDownloadWithError(_ error: any Error) {}

    /// Nothing newer: the button goes, even if a press was waiting on this check.
    func showUpdateNotFoundWithError(_ error: any Error, acknowledgement: @escaping () -> Void) {
        state = nil
        acknowledgement()
    }

    func showUpdaterError(_ error: any Error, acknowledgement: @escaping () -> Void) { acknowledgement() }

    func showDownloadInitiated(cancellation: @escaping () -> Void) {}
    func showDownloadDidReceiveExpectedContentLength(_ expectedContentLength: UInt64) {}
    func showDownloadDidReceiveData(ofLength length: UInt64) {}
    func showDownloadDidStartExtractingUpdate() {}
    func showExtractionReceivedProgress(_ progress: Double) {}

    func showReady(toInstallAndRelaunch reply: @escaping (SPUUserUpdateChoice) -> Void) {
        self.reply = reply
        state = .ready
    }

    func showInstallingUpdate(withApplicationTerminated applicationTerminated: Bool,
                              retryTerminatingApplication: @escaping () -> Void) {
        state = .installing
    }

    func showUpdateInstalledAndRelaunched(_ relaunched: Bool, acknowledgement: @escaping () -> Void) {
        state = nil
        acknowledgement()
    }

    /// Sparkle's session ended (a failed download or install among the reasons): its answer is spent, and a press that
    /// was waiting on it is undone, so the button reads Update again and its next press checks afresh.
    func dismissUpdateInstallation() {
        reply = nil
        installsFound = false
        if state?.working == true { state = .available }
    }

    /// Asked again while an update is under way: the app comes forward, where its button is.
    func showUpdateInFocus() { NSApp.activate() }
}

/// The update's one control, at the title bar's trailing end, shown only while there is an update: a quiet capsule, the
/// raised surface under a hairline with its word in the primary text, dimmed and disabled while Sparkle works on what it
/// was pressed for.
struct UpdateButton: View {
    let state: Updates.State
    let press: () -> Void

    static let height: CGFloat = 24

    var body: some View {
        Button(action: press) {
            Text(state.title)
                .font(Theme.ui(12, .medium))
                .foregroundStyle(Theme.text)
                .padding(.horizontal, 10)
                .frame(height: Self.height)
                .background(Theme.raised, in: Capsule())
                .overlay(Capsule().strokeBorder(Theme.hairline))
        }
        .buttonStyle(.hover(radius: Self.height / 2, over: true, id: "update.button"))
        .disabled(state.working)
        .help(state == .ready ? "Install the update and open CodeConnect again" : state.title)
        .accessibilityLabel(state.title)
        .accessibilityIdentifier("update.button")
    }
}
