import CodeConnectKit
import CoreImage.CIFilterBuiltins
import SwiftUI

/// What is added, and how: an agent from the start form in the window's main area (the run list's +, ⌘N); a computer
/// or an iPhone from the Add sheet, which shows both (the list's computer menu, ⇧⌘N, the iPhone's menu item).
enum AddKind: String, Identifiable, CaseIterable {
    case agent, computer, iPhone

    var id: String { rawValue }

    var title: String {
        switch self {
        case .agent: "New Run"
        case .computer: "Add Computer…"
        case .iPhone: "Add iPhone"
        }
    }

    /// The key that opens it; the iPhone has none.
    var shortcut: String? {
        switch self {
        case .agent: "⌘N"
        case .computer: "⇧⌘N"
        case .iPhone: nil
        }
    }
}

/// The way a computer or an iPhone is added: a sheet with the two side by side, each with its own bar.
struct AddSheet: View {
    /// The address Add a computer starts with: a refused computer pairing again. A binding, so the sheet reads the
    /// address as it is when it opens, not as the window last rendered it.
    @Binding var address: String?
    /// This Mac's Tailscale as the computer side's note says it: the iPhone side's words follow it.
    @State private var tailscale: TailscaleNote.Status?

    var body: some View {
        HStack(alignment: .top, spacing: 0) {
            ComputerForm(address: address, reports: { tailscale = $0 })
            Rectangle().fill(Theme.hairline).frame(width: 1)
            IPhoneSide(tailscale: tailscale).frame(width: 330)
        }
        // As tall as the taller side's content; never taller than the window has room for, where a side gives way.
        .frame(width: 900)
        .frame(maxHeight: room)
        .background(Theme.sheet)
        // A question asked in the sheet, such as New Folder's, is drawn over the sheet, not in the window behind it.
        .confirmationHost()
    }

    @Environment(\.sheetRoom) private var room
}

extension EnvironmentValues {
    /// How tall a sheet over the window may be.
    @Entry var sheetRoom: CGFloat = .infinity
    /// Where the start form's box stands, in `StartForm.space`: no panel of the form reaches past its sides.
    @Entry var startBox: CGRect = .zero
}

/// The bottom bar every side shares: a note on the left, the answer on the right.
struct SheetBar<Leading: View>: View {
    let action: String?
    /// What a driver finds the action by.
    let actionID: String
    let blocked: Bool
    let perform: () -> Void
    /// Nil when there is nothing to cancel: the sheet's work is done, and its action closes it.
    let cancel: (() -> Void)?
    let cancelID: String
    /// "Cancel", or "Close" where closing leaves the work going.
    var cancelTitle = "Cancel"
    /// The action answers ⌘↩; not where another bar beside it already does.
    var keyed = true
    @ViewBuilder let leading: Leading
    /// The sheet's questions, where it has them: while one is up, Esc and ⌘↩ are its, not the bar's.
    @Environment(ConfirmationQueue.self) private var asks: ConfirmationQueue?

    var body: some View {
        let asking = asks?.isAsking == true
        VStack(spacing: 0) {
            Rectangle().fill(Theme.hairline).frame(height: 1)
            HStack(spacing: 8) {
                leading
                Spacer()
                // The secondary answer in the app's one footer style, Set Up's: a bordered
                // button beside the filled one, the same height and corners.
                if let cancel {
                    ActionButton(title: cancelTitle, key: nil, prominent: false, id: cancelID, action: cancel)
                        .keyboardShortcut(asking ? nil : .cancelAction)
                }
                if let action {
                    ActionButton(title: action, key: keyed ? "⌘↩" : nil, prominent: true, id: actionID, action: perform)
                        .keyboardShortcut(asking || !keyed ? nil : KeyboardShortcut(.return, modifiers: .command))
                        .disabled(blocked)
                }
            }
            .padding(.horizontal, 18)
            .frame(height: 50)
        }
    }
}

/// Add a computer: CodeConnect on it, signed in to the same Tailscale account, and it joins the list by itself. A
/// computer that can't be reached that way pairs with the code it shows, behind a quiet option. A refusal stays
/// under the fields with what was typed, until the address or code changes; a code that goes through closes it.
private struct ComputerForm: View {
    @Environment(Machines.self) private var machines
    @Environment(\.dismiss) private var dismiss
    @State private var address = ""
    @State private var code = ""
    @State private var refusal: String?
    /// The address and code are shown: only when asked for, since a computer on the same Tailscale needs neither.
    @State private var usesCode = false

    let reports: (TailscaleNote.Status?) -> Void

    /// With an address, the form opens on the code for that computer.
    init(address: String?, reports: @escaping (TailscaleNote.Status?) -> Void) {
        _address = State(initialValue: address ?? "")
        _usesCode = State(initialValue: address != nil)
        self.reports = reports
    }

    private var exchanging: Bool { machines.isPairing && machines.pairingError == nil }

    /// The computers his Tailscale account lists that he removed from this Mac: each added again by one click, no code.
    private var onYourTailscale: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(PeerWords.onYourTailscale).font(Theme.ui(12, .medium)).foregroundStyle(Theme.text2)
            VStack(spacing: 0) {
                ForEach(machines.removedComputers, id: \.host) { computer in
                    HStack(spacing: 8) {
                        Image(systemName: "desktopcomputer").font(.system(size: 11)).foregroundStyle(Theme.text2)
                            .frame(width: 16)
                            .accessibilityHidden(true)
                        Text(computer.name).font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1)
                            .frame(maxWidth: .infinity, alignment: .leading)
                        ActionButton(
                            title: PeerWords.add, key: nil, prominent: false, id: "computer.tailnet.\(computer.host).add"
                        ) {
                            machines.addAgain(computer)
                            dismiss()
                        }
                    }
                    .padding(.horizontal, 12)
                    .frame(minHeight: 40)
                }
            }
            .background(Theme.boxFill, in: RoundedRectangle(cornerRadius: Theme.boxRadius))
            .overlay(RoundedRectangle(cornerRadius: Theme.boxRadius).strokeBorder(Theme.hairline))
        }
        .padding(.horizontal, 18)
        .padding(.top, 14)
    }
    private var problem: String? { refusal ?? machines.pairingError }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 10) {
                Text("Add a computer").font(Theme.ui(19, .semibold)).foregroundStyle(Theme.text)
                Text(PeerWords.addIntro(removedListed: !machines.removedComputers.isEmpty))
                    .font(Theme.ui(13.5))
                    .foregroundStyle(Theme.text2)
                    .lineSpacing(3)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.horizontal, 18)
            .padding(.top, 18)
            if !machines.removedComputers.isEmpty { onYourTailscale }
            TailscaleNote(reports: reports)

            VStack(alignment: .leading, spacing: 14) {
                Text(AttributedString(macMarkdown: "Get CodeConnect for the other computer at [codeconnect.sh](https://codeconnect.sh/#install)."))
                    .font(Theme.ui(12.5))
                    .foregroundStyle(Theme.text)
                Button {
                    usesCode.toggle()
                } label: {
                    HStack(spacing: 6) {
                        Image(systemName: usesCode ? "chevron.down" : "chevron.right").font(.system(size: 9, weight: .semibold))
                        Text("Use the pairing code from the other computer").font(Theme.ui(12.5))
                    }
                    .foregroundStyle(Theme.text2)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.hover(bare: true, flush: .leading, id: "computer.useCode"))
                .accessibilityIdentifier("computer.useCode")
                .accessibilityValue(usesCode ? "open" : "closed")
                .padding(.top, 4)
                if usesCode {
                    HStack(alignment: .top, spacing: 12) {
                        field(
                            PairingWords.address, id: "computer.address", text: $address, prompt: "The other computer’s address",
                            hint: "Shown with the code on the other computer.")
                        field(PairingWords.code, id: "computer.code", text: $code, prompt: PairingWords.pairingCode)
                    }
                    .padding(.leading, 15)
                    .disabled(exchanging)
                    if let problem {
                        Text(AttributedString(macMarkdown: problem))
                            .font(Theme.ui(12))
                            .foregroundStyle(Theme.red)
                            .lineSpacing(2)
                            .fixedSize(horizontal: false, vertical: true)
                            .padding(.leading, 15)
                            .accessibilityIdentifier("computer.problem")
                    }
                }
            }
            .padding(.horizontal, 18)
            .padding(.top, 18)

            Spacer(minLength: 18)

            SheetBar(
                action: usesCode ? (exchanging ? PairingWords.connecting : PairingWords.connect) : nil,
                actionID: "computer.connect",
                blocked: !usesCode || exchanging || PairingCredentialInput.blockedReason(address: address, code: code) != nil,
                perform: connect,
                cancel: {
                    machines.cancelPairing()
                    dismiss()
                },
                cancelID: "computer.cancel"
            ) {
                if usesCode {
                    Text(PairingWords.codeWorksOnce)
                        .font(Theme.ui(11.5))
                        .foregroundStyle(Theme.text3)
                }
            }
        }
        .onChange(of: address) { clear() }
        .onChange(of: code) { clear() }
        .onChange(of: machines.showsFleet) { _, shows in if shows { dismiss() } }
        // Pairing again a computer already in the list changes no list: the exchange ends with nothing left pairing.
        .onChange(of: exchanging) { was, now in if was, !now, !machines.isPairing { dismiss() } }
        #if DEBUG
            .task {
                guard let pairing = RenderWindow.pairing else { return }
                usesCode = true
                address = pairing.address
                code = pairing.code
                connect()
            }
        #endif
    }

    private func connect() {
        switch PairingCredentialInput.submission(address: address, code: code) {
        case .refused(let reason):
            refusal = reason
        case .token(let endpoint):
            refusal = nil
            machines.pair(with: endpoint)
        case .pairingCode(let payload):
            refusal = nil
            machines.pair(withQR: payload)
        }
    }

    /// A refusal answers what was sent; once the address or code changes it no longer does.
    private func clear() {
        refusal = nil
        if machines.pairingError != nil { machines.cancelPairing() }
    }

    private func field(_ label: String, id: String, text: Binding<String>, prompt: String, hint: String? = nil) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label).font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.text3)
            // A field's title names nothing; its label does.
            TextField("", text: text, prompt: Text(prompt).foregroundStyle(Theme.text3))
                .accessibilityLabel(label)
                .accessibilityIdentifier(id)
                .textFieldStyle(.plain)
                .font(Theme.mono(12.5))
                .foregroundStyle(Theme.text)
                .autocorrectionDisabled()
                .padding(.horizontal, 10)
                .frame(height: 30)
                .background(Theme.code, in: RoundedRectangle(cornerRadius: 7))
                .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
            if let hint { Text(hint).font(Theme.ui(11.5)).foregroundStyle(Theme.text3) }
        }
    }
}

/// Add an iPhone: a code from this Mac's daemon, as the QR an iPhone's camera opens (the app when it is
/// installed, else the page that offers it). A code no iPhone could reach, the daemon still on loopback before
/// Tailscale, is no QR, by Kit's own rule, and the side asks again every few seconds. A fresh code replaces each
/// one as it expires. Under the QR, its address and code as text, for what can't scan. A device the daemon
/// didn't have when the code was shown is named, with Done.
private struct IPhoneSide: View {
    @Environment(Setup.self) private var setup
    @Environment(\.dismiss) private var dismiss
    @State private var code: Code?
    /// What the computer side's Tailscale note says, so the two never disagree.
    let tailscale: TailscaleNote.Status?

    private enum Code {
        case shown(PairingQRPayload)
        case unreachable
        case notAnswering
        /// The device's name: paired with a code, or joined through Tailscale by the same account.
        case added(String)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 10) {
                Text("Add an iPhone").font(Theme.ui(19, .semibold)).foregroundStyle(Theme.text)
                Text(intro)
                    .font(Theme.ui(13.5))
                    .foregroundStyle(Theme.text2)
                    .lineSpacing(3)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.horizontal, 18)
            .padding(.top, 18)
            if case .shown(let payload) = code, let image = Self.qr(payload.link) {
                Image(nsImage: image)
                    .interpolation(.none)
                    .resizable()
                    .accessibilityLabel("Pairing QR code")
                    .accessibilityIdentifier("iphone.qr")
                    .frame(width: 138, height: 138)
                    .padding(12)
                    .background(.white, in: RoundedRectangle(cornerRadius: 10))
                    .frame(maxWidth: .infinity)
                    .padding(.top, 28)
                // For what can't scan: an iPhone pairing by hand, or a Mac on another Tailscale account.
                if case .pairingCode(let pairingCode) = payload.endpoint.credential {
                    VStack(spacing: 4) {
                        Text("Or enter this address and code:").font(Theme.ui(12)).foregroundStyle(Theme.text2)
                        Text("\(payload.endpoint.typedAddress) · \(pairingCode)").font(Theme.mono(11.5)).foregroundStyle(Theme.text2)
                            .textSelection(.enabled)
                    }
                    .multilineTextAlignment(.center)
                    .accessibilityElement(children: .combine)
                    .accessibilityIdentifier("iphone.code")
                    .frame(maxWidth: .infinity)
                    .padding(.horizontal, 18)
                    .padding(.top, 14)
                }
            }
            Spacer(minLength: 18)
            // Beside the computer's bar, whose Cancel and Esc close the sheet and whose Connect answers ⌘↩: Done alone,
            // lit once an iPhone has paired, answering a click.
            SheetBar(
                action: "Done", actionID: "iphone.done", blocked: !added, perform: { dismiss() },
                cancel: nil, cancelID: "iphone.cancel", keyed: false
            ) {
                if !added {
                    Text("Done lights once the iPhone has paired.")
                        .font(Theme.ui(11.5))
                        .foregroundStyle(Theme.text3)
                }
            }
        }
        .task {
            #if DEBUG
                if RenderWindow.onLoopback {
                    code = .unreachable
                    return
                }
            #endif
            let root = setup.installer?.root
            while !Task.isCancelled {
                code = await Self.mint(root: root)
                guard case .shown = code, let root else {
                    // No code to show: asked every 5 s where the daemon answers, and a new code only once an
                    // iPhone could reach it.
                    while !Task.isCancelled {
                        try? await Task.sleep(for: .seconds(5))
                        guard let reachable = await Self.reach(root: root) else { code = .notAnswering; continue }
                        if reachable { break }
                        code = .unreachable
                    }
                    continue
                }
                // Until this code expires: a device that wasn't there when it was shown. Without the list to
                // compare with, the code is shown to its end unwatched.
                let before = await Self.devices(root: root)
                for _ in 0..<150 {
                    try? await Task.sleep(for: .seconds(2))
                    if Task.isCancelled { return }
                    if let before, let name = await Self.devices(root: root)?.first(where: { before[$0.key] == nil })?.value {
                        code = .added(name)
                        return
                    }
                }
            }
        }
    }

    private var added: Bool {
        if case .added = code { true } else { false }
    }

    private var intro: String {
        switch code {
        // In the note's own state. Signed in, on loopback, is the daemon still moving onto Tailscale.
        case .unreachable:
            switch tailscale {
            case .notConnected: PairingWords.iPhoneReachesOnceConnected
            case .moving, .signedIn, .tagged: PairingWords.iPhoneReachesOnceMoved
            case .notInstalled, nil: PairingWords.iPhoneCantReach
            }
        case .notAnswering: "The background service isn’t answering. Choose Set Up CodeConnect… to try again."
        case .added(let name): "\(name) was added."
        case .shown, nil: "Scan this with your iPhone’s camera to pair it with this Mac."
        }
    }

    /// Whether an iPhone could reach the daemon where it answers now (`daemon_info`, Kit's rule, no code
    /// minted); nil when it doesn't answer.
    private static func reach(root: URL?) async -> Bool? {
        guard let root, let address = await Task.detached(operation: { LocalDaemon.address(root: root) }).value
        else { return nil }
        return PairingQRPayload.unreachableHost(address.host) == nil
    }

    /// The devices the daemon has paired, by id: their names.
    private static func devices(root: URL) async -> [String: String]? {
        await Task.detached(operation: { LocalDaemon.devices(root: root) }).value
    }

    /// A fresh code from this Mac's daemon, and whether an iPhone could reach it.
    private static func mint(root: URL?) async -> Code {
        guard let root, let payload = await Task.detached(operation: { LocalDaemon.pairing(root: root) }).value
        else { return .notAnswering }
        if case .failure(.unreachableHost) = PairingQRPayload.decode(payload.link) { return .unreachable }
        return .shown(payload)
    }

    /// The link as a QR, one pixel a module, drawn larger without smoothing so every module stays sharp.
    private static func qr(_ link: String) -> NSImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(link.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage, let image = CIContext().createCGImage(output, from: output.extent)
        else { return nil }
        return NSImage(cgImage: image, size: output.extent.size)
    }
}
