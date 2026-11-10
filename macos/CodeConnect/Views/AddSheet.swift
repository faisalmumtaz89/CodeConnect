import CodeConnectKit
import CoreImage.CIFilterBuiltins
import SwiftUI

/// What the Add sheet adds, and the side it opens on. The sidebar's + and ⌘N/⇧⌘N open an agent or a computer;
/// the iPhone has a menu item and is one click from either side.
enum AddKind: String, Identifiable, CaseIterable {
    case agent, computer, iPhone

    var id: String { rawValue }

    var title: String {
        switch self {
        case .agent: "New agent"
        case .computer: "Add computer"
        case .iPhone: "Add iPhone"
        }
    }

    var tab: String {
        switch self {
        case .agent: "Agent"
        case .computer: "Computer"
        case .iPhone: "iPhone"
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

extension KeyHint {
    /// What a key opens: its key and the sheet's title.
    init(_ kind: AddKind) { self.init(keys: kind.shortcut.map { [$0] } ?? [], label: kind.title) }
}

/// The one way anything is added: a sheet with Agent, Computer and iPhone side by side.
struct AddSheet: View {
    @State private var kind: AddKind
    /// The address Add a computer starts with: a refused computer pairing again.
    private let address: String?
    /// An agent was started and is in the list.
    private let started: () -> Void

    init(kind: AddKind, address: String? = nil, started: @escaping () -> Void = {}) {
        _kind = State(initialValue: kind)
        self.address = address
        self.started = started
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 10) {
                HStack(spacing: 0) {
                    ForEach(AddKind.allCases) { option in
                        Button { kind = option } label: {
                            Text(option.tab).font(Theme.ui(12, kind == option ? .medium : .regular))
                                .foregroundStyle(kind == option ? Theme.text : Theme.text3)
                                .padding(.horizontal, 10)
                                .frame(height: 24)
                                .background(kind == option ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
                        }
                        .buttonStyle(.hover(radius: 5, id: "add.tab.\(option.rawValue.lowercased())"))
                        .accessibilityIdentifier("add.tab.\(option.rawValue.lowercased())")
                        .accessibilityAddTraits(kind == option ? .isSelected : [])
                    }
                }
                .padding(2)
                .background(Theme.window, in: RoundedRectangle(cornerRadius: 7))
                .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
                Spacer()
            }
            .padding(.horizontal, 14)
            .frame(height: 50)
            Rectangle().fill(Theme.hairline).frame(height: 1)
            switch kind {
            case .agent: AgentSide(addComputer: { kind = .computer }, started: started)
            case .computer: ComputerForm(address: address)
            case .iPhone: IPhoneSide()
            }
        }
        .frame(width: 640, height: 540)
        .background(Theme.sheet)
    }
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
    @ViewBuilder let leading: Leading

    var body: some View {
        VStack(spacing: 0) {
            Rectangle().fill(Theme.hairline).frame(height: 1)
            HStack(spacing: 8) {
                leading
                Spacer()
                if let cancel {
                    Button(cancelTitle, action: cancel)
                        .buttonStyle(.hover(outset: 5, id: cancelID))
                        .font(Theme.ui(12.5))
                        .foregroundStyle(Theme.text2)
                        .keyboardShortcut(.cancelAction)
                        .accessibilityIdentifier(cancelID)
                }
                if let action {
                    ActionButton(title: action, key: "⌘↩", prominent: true, id: actionID, action: perform)
                        .keyboardShortcut(.return, modifiers: .command)
                        .disabled(blocked)
                        .opacity(blocked ? 0.45 : 1)
                }
            }
            .padding(.horizontal, 14)
            .frame(height: 50)
        }
    }
}

/// The Agent side: the start form once a computer can start an agent; before that, why none can (none paired,
/// none connected, or a computer's CodeConnect too old to start one from here).
private struct AgentSide: View {
    @Environment(Machines.self) private var machines
    @Environment(\.dismiss) private var dismiss
    let addComputer: () -> Void
    let started: () -> Void

    var body: some View {
        if !machines.startContexts.isEmpty {
            StartForm(started: started)
        } else {
            VStack(alignment: .leading, spacing: 0) {
                VStack(alignment: .leading, spacing: 10) {
                    Text("Start an agent").font(Theme.ui(19, .semibold)).foregroundStyle(Theme.text)
                    Text(why)
                        .font(Theme.ui(13.5))
                        .foregroundStyle(Theme.text2)
                        .lineSpacing(3)
                        .fixedSize(horizontal: false, vertical: true)
                    if !machines.showsFleet {
                        ActionButton(title: "Add a computer", key: nil, prominent: false, id: "agent.addComputer", action: addComputer)
                            .padding(.top, 6)
                    }
                }
                .padding(.horizontal, 18)
                .padding(.top, 18)
                Spacer(minLength: 0)
                SheetBar(action: nil, actionID: "", blocked: true, perform: {}, cancel: { dismiss() }, cancelID: "agent.cancel") {
                    EmptyView()
                }
            }
        }
    }

    private var why: String {
        guard machines.showsFleet else {
            return "An agent runs on one of your computers, in a project there. Add a computer first, then start one here."
        }
        guard let old = machines.models.first(where: { $0.connection.phase.isConnected }) else {
            return "None of your computers is connected right now. You can start an agent once one is."
        }
        return "\(old.machineName ?? "That computer")’s CodeConnect is too old to start agents from here. Update it, then try again."
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

    /// With an address, the form opens on the code for that computer.
    init(address: String?) {
        _address = State(initialValue: address ?? "")
        _usesCode = State(initialValue: address != nil)
    }

    private var exchanging: Bool { machines.isPairing && machines.pairingError == nil }
    private var problem: String? { refusal ?? machines.pairingError }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 10) {
                Text("Add a computer").font(Theme.ui(19, .semibold)).foregroundStyle(Theme.text)
                Text("Install CodeConnect on it and sign in to the same Tailscale account. It appears here by itself.")
                    .font(Theme.ui(13.5))
                    .foregroundStyle(Theme.text2)
                    .lineSpacing(3)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.horizontal, 18)
            .padding(.top, 18)
            TailscaleNote()

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
                .buttonStyle(.hover(outset: 5, id: "computer.useCode"))
                .accessibilityIdentifier("computer.useCode")
                .accessibilityValue(usesCode ? "open" : "closed")
                .padding(.top, 4)
                if usesCode {
                    HStack(alignment: .top, spacing: 12) {
                        field(
                            "Address", id: "computer.address", text: $address, prompt: "The other computer's address",
                            hint: "Shown with the code on the other computer.")
                        field("Code", id: "computer.code", text: $code, prompt: "Pairing code")
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
            .padding(.top, 22)

            Spacer(minLength: 0)

            SheetBar(
                action: usesCode ? (exchanging ? "Connecting…" : "Connect") : nil,
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
                    Text("Each code works once and expires after five minutes.")
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
            TailscaleNote()
            if case .shown(let payload) = code, let image = Self.qr(payload.link) {
                Image(nsImage: image)
                    .interpolation(.none)
                    .resizable()
                    .accessibilityLabel("Pairing QR code")
                    .accessibilityIdentifier("iphone.qr")
                    .frame(width: 196, height: 196)
                    .padding(12)
                    .background(.white, in: RoundedRectangle(cornerRadius: 10))
                    .frame(maxWidth: .infinity)
                    .padding(.top, 28)
                // For what can't scan: an iPhone pairing by hand, or a Mac on another Tailscale account.
                if case .pairingCode(let pairingCode) = payload.endpoint.credential {
                    Text("Or enter this address and code: \(payload.endpoint.typedAddress) · \(pairingCode)")
                        .font(Theme.ui(12))
                        .foregroundStyle(Theme.text2)
                        .textSelection(.enabled)
                        .accessibilityIdentifier("iphone.code")
                        .frame(maxWidth: .infinity)
                        .padding(.top, 14)
                }
            }
            Spacer(minLength: 0)
            SheetBar(
                action: added ? "Done" : nil, actionID: "iphone.done", blocked: !added, perform: { dismiss() },
                cancel: added ? nil : { dismiss() }, cancelID: "iphone.cancel"
            ) {
                if case .shown = code {
                    Text("Each code works once and expires after five minutes.")
                        .font(Theme.ui(11.5))
                        .foregroundStyle(Theme.text3)
                }
            }
        }
        .task {
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
        case .unreachable:
            "Your iPhone can't reach this Mac yet. Connect this Mac to a network your iPhone can reach, such as Tailscale, then come back here."
        case .notAnswering: "The background service isn't answering. Choose Set Up CodeConnect… to try again."
        case .added(let name): "\(name) was added."
        case .shown, nil: "Scan this with your iPhone's camera to pair it with this Mac."
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
