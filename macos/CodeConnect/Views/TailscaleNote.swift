import AppKit
import CodeConnectKit
import SwiftUI

/// This Mac's Tailscale on the Computer and iPhone sides of the Add sheet, as this Mac's own daemon says it in
/// its `peers` frame (the app never runs `tailscale`): one line in plain words, the one thing to do about it,
/// and, once signed in, what signing in trusts. Nothing until that daemon has said.
struct TailscaleNote: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup
    /// Where this Mac's daemon answers devices now (`daemon_info`), asked every five seconds.
    @State private var address: LocalDaemon.Address?

    /// What the frame puts this Mac in, with its words.
    enum Status: Equatable {
        case notInstalled, notConnected, moving, signedIn(String?), tagged

        /// A `tailnet` word this app doesn't know reads as not installed, never as signed in. Running, with the
        /// daemon still answering at loopback, is the daemon moving onto Tailscale: it started before Tailscale
        /// was up, and its own watch restarts it there.
        init(_ directory: PeerDirectory, onLoopback: Bool) {
            switch directory.tailnet {
            case "running": self = onLoopback ? .moving : directory.tagged ? .tagged : .signedIn(directory.login)
            case "not_running": self = .notConnected
            default: self = .notInstalled
            }
        }

        var line: String {
            switch self {
            case .notInstalled:
                "Tailscale is one way your iPhone and other computers can reach this Mac, privately, from anywhere."
            case .notConnected: "Tailscale is on this Mac but isn't connected. Sign in to Tailscale to connect."
            case .moving: "Tailscale is connected. CodeConnect is moving onto it, which can take up to a minute."
            case .signedIn(let login): login.map { "Signed in to Tailscale as \($0)." } ?? "Signed in to Tailscale."
            case .tagged: "This Mac is on Tailscale but not as you, so your iPhone and other computers need a code to reach it."
            }
        }
    }

    /// What signing in to Tailscale trusts.
    static let trust =
        "A computer signed in to your Tailscale is trusted as a whole: every user account on it can reach your sessions. For a shared computer, use another Tailscale account."
    /// Tailscale's own download page: its steps are Tailscale's to keep current.
    private static let download = URL(string: "https://tailscale.com/download")!
    /// The Tailscale app, where the daemon looks for it.
    private static let app = URL(fileURLWithPath: "/Applications/Tailscale.app")

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let directory {
                card(Status(directory, onLoopback: address.map { PairingQRPayload.unreachableHost($0.host) != nil } ?? false))
                    .padding(.horizontal, 18)
                    .padding(.top, 14)
            }
        }
        .task {
            guard let root = setup.installer?.root else { return }
            while !Task.isCancelled {
                address = await Task.detached { LocalDaemon.address(root: root) }.value
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    /// What this Mac's daemon last said about its Tailscale.
    private var directory: PeerDirectory? {
        #if DEBUG
            if let peers = RenderWindow.peers { return peers }
        #endif
        guard let id = setup.thisMac(at: address) else { return nil }
        return machines.model(for: id)?.tailnet
    }

    private func card(_ status: Status) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            mark(status)
            VStack(alignment: .leading, spacing: 4) {
                Text(status.line).font(Theme.ui(12.5)).foregroundStyle(Theme.text)
                    .accessibilityIdentifier("tailscale.status")
                if case .signedIn = status {
                    Text(Self.trust).font(Theme.ui(11.5)).foregroundStyle(Theme.text3).lineSpacing(2)
                }
            }
            .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 8)
            if let action = action(status) {
                ActionButton(title: action.title, key: nil, prominent: false, id: "tailscale.action") {
                    NSWorkspace.shared.open(action.url)
                }
            }
        }
        .padding(12)
        .background(Theme.window, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.hairline))
    }

    /// The one thing to do: get Tailscale, or open it when its app is where the daemon found it.
    private func action(_ status: Status) -> (title: String, url: URL)? {
        switch status {
        case .notInstalled: ("Get Tailscale", Self.download)
        case .notConnected where FileManager.default.fileExists(atPath: Self.app.path): ("Open Tailscale", Self.app)
        default: nil
        }
    }

    /// Grey only: a check once it's done, dotted while it happens, an i for what's on offer. Never amber: Tailscale
    /// is one way to reach this Mac, not something it lacks.
    private func mark(_ status: Status) -> some View {
        Group {
            switch status {
            case .notInstalled, .notConnected:
                Image(systemName: "info.circle").foregroundStyle(Theme.text3)
            case .moving: Image(systemName: "circle.dotted").foregroundStyle(Theme.text3)
            case .signedIn, .tagged: Image(systemName: "checkmark.circle.fill").foregroundStyle(Theme.text2)
            }
        }
        .font(.system(size: 12))
        .frame(width: 14)
        // The sentence beside it says the same in words.
        .accessibilityHidden(true)
    }
}
