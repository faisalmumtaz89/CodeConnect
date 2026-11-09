import CodeConnectKit
import SwiftUI

/// Settings: this computer's devices, as its own CodeConnect lists them over its local socket. One entry a phone or
/// computer (Kit's `DeviceGroup`), with Block; the blocked ones with Allow again. Each asks first, naming the device.
/// The app's own entry has no Block. Every word is Kit's.
struct DevicesPane: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup
    /// Nil until this computer has answered.
    @State private var devices: [PairedDevice]?
    @State private var answers = true
    @State private var address: LocalDaemon.Address?
    @State private var asking: Ask?
    @State private var refusal: String?

    private enum Ask {
        case block(DeviceGroup)
        case allow(DeviceGroup)

        var group: DeviceGroup {
            switch self {
            case .block(let group), .allow(let group): group
            }
        }
        var question: String {
            switch self {
            case .block(let group): group.blockQuestion
            case .allow(let group): group.allowQuestion
            }
        }
        var sentence: String {
            switch self {
            case .block(let group): group.blockSentence
            case .allow(let group): group.allowSentence
            }
        }
    }

    /// The app's own pairing with this computer, by the device id its daemon gave it.
    private var thisDevice: String? {
        machines.model(for: setup.thisMac(at: address))?.connection.helloAck?.deviceID
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                VStack(alignment: .leading, spacing: 6) {
                    Text(DeviceGroup.title).font(Theme.ui(19, .semibold)).foregroundStyle(Theme.text)
                    Text(DeviceGroup.intro).font(Theme.ui(13)).foregroundStyle(Theme.text2)
                }
                if let devices {
                    let (allowed, blocked) = DeviceGroup.groups(devices)
                    if allowed.isEmpty, blocked.isEmpty { note(DeviceGroup.empty) }
                    if !allowed.isEmpty { list(allowed) }
                    if !blocked.isEmpty {
                        VStack(alignment: .leading, spacing: 8) {
                            Text(DeviceGroup.blockedTitle).font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.text3)
                            list(blocked)
                        }
                    }
                } else {
                    note(answers ? DeviceGroup.reading : DeviceGroup.unreachable)
                }
                if let refusal {
                    Text(refusal).font(Theme.ui(12)).foregroundStyle(Theme.red).fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(24)
        }
        .frame(width: 560, height: 460)
        .background(Theme.sheet)
        #if DEBUG
            .background(RenderWindow(ready: devices != nil || !answers, showsSheet: asking != nil))
        #endif
        .alert(
            asking?.question ?? "", isPresented: Binding(get: { asking != nil }, set: { if !$0 { asking = nil } }),
            presenting: asking
        ) { ask in
            switch ask {
            case .block:
                Button(DeviceGroup.blockAction, role: .destructive) { perform(ask) }.accessibilityIdentifier("settings.ask.block")
            case .allow: Button(DeviceGroup.allowAction) { perform(ask) }.accessibilityIdentifier("settings.ask.allow")
            }
            Button("Cancel", role: .cancel) {}.accessibilityIdentifier("settings.ask.cancel")
        } message: { ask in
            Text(ask.sentence)
        }
        .task { await watch() }
    }

    private func note(_ text: String) -> some View {
        Text(text).font(Theme.ui(12.5)).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
    }

    private func list(_ groups: [DeviceGroup]) -> some View {
        VStack(spacing: 0) {
            ForEach(Array(groups.enumerated()), id: \.element.id) { index, group in
                if index > 0 { Rectangle().fill(Theme.hairline).frame(height: 1) }
                row(group)
            }
        }
        .background(Theme.window, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.hairline))
    }

    private func row(_ group: DeviceGroup) -> some View {
        let isThisMac = group.devices.contains { $0.deviceID == thisDevice }
        // By the device Block and Allow again act on.
        let id = "settings.device.\(group.devices[0].deviceID)"
        return HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                Text(group.name).font(Theme.ui(13, .medium)).foregroundStyle(Theme.text)
                AgeText(since: group.isBlocked ? group.blockedAt : isThisMac ? nil : group.lastSeen) { now in
                    if group.isBlocked {
                        group.blockedLine(now: now)
                    } else if isThisMac {
                        DeviceGroup.thisDevice
                    } else {
                        group.seenLine(now: now)
                    }
                }
                .font(Theme.ui(11.5))
                .foregroundStyle(Theme.text3)
                .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 12)
            if group.isBlocked {
                ActionButton(title: DeviceGroup.allowAction, key: nil, prominent: false, id: "\(id).allow") {
                    asking = .allow(group)
                }
            } else if !isThisMac {
                ActionButton(title: DeviceGroup.blockAction, key: nil, prominent: false, id: "\(id).block") {
                    asking = .block(group)
                }
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(id)
    }

    /// Blocks or allows again by the newest pairing; the daemon does the same to every pairing of that Tailscale
    /// device. Then the list as it is now.
    private func perform(_ ask: Ask) {
        guard let root = setup.installer?.root else { return }
        let id = ask.group.devices[0].deviceID
        let request: DeviceRequest =
            switch ask {
            case .block: .block(deviceID: id)
            case .allow: .allow(deviceID: id)
            }
        Task {
            let reply = await Task.detached { LocalDaemon.ask(request, root: root) }.value
            if case .refused(let message)? = reply { refusal = message } else { refusal = nil }
            await read(root)
        }
    }

    /// The list, and where this computer answers, now and every five seconds while the pane is open: a computer
    /// that joins through Tailscale appears by itself.
    private func watch() async {
        #if DEBUG
            if let shown = RenderWindow.devices {
                devices = shown
                asking = RenderWindow.devicesAsk.flatMap { ask in
                    let (allowed, blocked) = DeviceGroup.groups(shown)
                    return ask == "allow" ? blocked.first.map(Ask.allow) : allowed.first.map(Ask.block)
                }
                return
            }
        #endif
        guard let root = setup.installer?.root else {
            answers = false
            return
        }
        while !Task.isCancelled {
            await read(root)
            try? await Task.sleep(for: .seconds(5))
        }
    }

    private func read(_ root: URL) async {
        address = await Task.detached { LocalDaemon.address(root: root) }.value
        let reply = await Task.detached { LocalDaemon.ask(.list, root: root) }.value
        if case .devices(let listed)? = reply {
            devices = listed
            answers = true
        } else {
            answers = false
        }
    }
}
