import CodeConnectKit
import SwiftUI

/// The pairing link a `codeconnect pair` QR code holds,
/// `https://codeconnect.sh/p#v=1&h=<host>&p=<port>&c=<code>`, opened from
/// outside the app and read the way a scanned code is.
struct PairingLink: Identifiable {
    let id = UUID()
    let decoded: Result<PairingQRPayload, PairingQRPayload.Failure>

    /// Nil for any other link.
    init?(url: URL) {
        guard url.scheme?.lowercased() == "https", url.host?.lowercased() == "codeconnect.sh",
            url.path == "/p"
        else { return nil }
        decoded = PairingQRPayload.decode(url.absoluteString)
    }
}

/// Pairs with the computer a link names, and only on a tap: a link can come from
/// anywhere, a message included, so opening one never pairs by itself.
struct PairingLinkSheet: View {
    let link: PairingLink

    @Environment(Machines.self) private var machines
    @Environment(\.dismiss) private var dismiss
    /// Set by the tap on Pair, so only a pairing this sheet started closes it.
    @State private var started = false

    var body: some View {
        CCSheetChrome("Pair this iPhone", onClose: close) {
            ScrollView {
                VStack(alignment: .leading, spacing: CC.rhythm.sections) {
                    switch link.decoded {
                    case .success(let payload):
                        confirm(payload)
                    case .failure(let failure):
                        CCBanner(
                            "Could not read that code", message: failure.localizedDescription,
                            tone: .warning)
                    }
                }
                .padding(CC.space.md)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .background(CC.color.bg)
        }
        // A pairing that went through is no longer pending; one that failed
        // stays pending with its reason until it is tried again or closed.
        .onChange(of: machines.isPairing) { _, pairing in
            if started, !pairing { dismiss() }
        }
    }

    @ViewBuilder
    private func confirm(_ payload: PairingQRPayload) -> some View {
        CCProse(
            "This pairs your iPhone with the computer below. Pair only if this code is showing on your computer right now.",
            color: CC.text.secondary
        )
        .fixedSize(horizontal: false, vertical: true)

        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            CCSectionHeader("Computer")
            CCCard {
                Text(address(payload.endpoint))
                    .ccType(CC.type.mono)
                    .foregroundStyle(CC.text.primary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }

        if started, let error = machines.pairingError {
            CCBanner("Pairing failed", message: error, tone: .danger)
        }

        CCButton(
            "Pair", variant: .primary, size: .lg, fullWidth: true,
            isLoading: started && machines.isPairing && machines.pairingError == nil
        ) {
            if machines.pairingError != nil { machines.cancelPairing() }
            started = true
            machines.pair(withQR: payload)
        }
    }

    /// The address as the manual field takes it: the port only when it is not
    /// the default, and an IPv6 address in brackets when it has one.
    private func address(_ endpoint: DaemonEndpoint) -> String {
        guard endpoint.port != DaemonEndpoint.defaultPort else { return endpoint.host }
        let host = endpoint.host.contains(":") ? "[\(endpoint.host)]" : endpoint.host
        return "\(host):\(endpoint.port)"
    }

    private func close() {
        if started, machines.isPairing { machines.cancelPairing() }
        dismiss()
    }
}
