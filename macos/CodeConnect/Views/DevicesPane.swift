import CodeConnectKit
import SwiftUI

/// Settings: this computer's devices, as its own CodeConnect lists them over its local socket. One entry a phone or
/// computer (Kit's `DeviceGroup`), with Block; the blocked ones with Allow Again. Each asks first, naming the device.
/// The app's own entry has no Block, and no entry has Block while the app can't tell which is its own. While this
/// computer isn't answering, the pane says so rather than show the last list. Every word is Kit's.
struct DevicesPane: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup
    /// Nil until this computer has answered, and again whenever a read fails.
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
        #if DEBUG
            // `-CC_THIS_DEVICE <device id>`: the app's own entry in a render's list.
            if let forced = UserDefaults.standard.string(forKey: "CC_THIS_DEVICE") { return forced }
        #endif
        return machines.model(for: setup.thisMac(at: address))?.connection.helloAck?.deviceID
    }

    /// The pane knows which entry is the app's own: its link has said the device id, or the app has no pairing with
    /// this computer. Not while that link is down or still connecting, when any entry could be the app's own.
    private var knowsThisDevice: Bool { thisDevice != nil || setup.thisMac(at: address) == nil }

    var body: some View {
        Group {
            VStack(alignment: .leading, spacing: 18) {
                VStack(alignment: .leading, spacing: 6) {
                    Text(DeviceGroup.title).font(Theme.ui(19, .semibold)).foregroundStyle(Theme.text)
                    Text(DeviceGroup.intro).font(Theme.ui(13)).foregroundStyle(Theme.text2)
                }
                if let devices {
                    let (allowed, blocked) = DeviceGroup.groups(devices)
                    if allowed.isEmpty, blocked.isEmpty { note(DeviceGroup.empty) }
                    if !allowed.isEmpty { SettingsGroup { ForEach(allowed) { row($0) } } }
                    if !blocked.isEmpty { SettingsGroup(title: DeviceGroup.blockedTitle) { ForEach(blocked) { row($0) } } }
                } else {
                    note(answers ? DeviceGroup.reading : DeviceGroup.unreachable)
                }
                if let refusal {
                    Text(refusal).font(Theme.ui(12)).foregroundStyle(Theme.red).fixedSize(horizontal: false, vertical: true)
                }
            }
        }
        #if DEBUG
            .background(RenderWindow(ready: devices != nil || !answers))
        #endif
        .confirmation($asking) { ask in
            let acts: Confirmation.Answer = switch ask {
            case .block: .init(title: DeviceGroup.blockAction, kind: .destructive, id: "settings.ask.block") { perform(ask) }
            case .allow: .init(title: DeviceGroup.allowAction, kind: .plain, id: "settings.ask.allow") { perform(ask) }
            }
            return Confirmation(title: ask.question, message: ask.sentence, answers: [
                .init(title: "Cancel", kind: .cancel, id: "settings.ask.cancel"), acts,
            ])
        }
        .task { await watch() }
    }

    private func note(_ text: String) -> some View {
        Text(text).font(Theme.ui(12.5)).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
    }

    private func row(_ group: DeviceGroup) -> some View {
        let isThisMac = group.devices.contains { $0.deviceID == thisDevice }
        // By the device Block and Allow Again act on.
        let id = "settings.device.\(group.devices[0].deviceID)"
        return HStack(alignment: .center, spacing: 12) {
            // A Mac or an iPhone; the place kept, empty, until a hello has said which.
            Image(systemName: group.kind == .iphone ? "iphone" : "desktopcomputer")
                .font(.system(size: 12)).foregroundStyle(Theme.text2).frame(width: 16)
                .opacity(group.kind == nil ? 0 : 1)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                // The app's own entry is "This Mac", its name under it.
                Text(isThisMac ? DeviceGroup.thisDevice : group.name).font(Theme.ui(13)).foregroundStyle(Theme.text)
                AgeText(since: group.isBlocked ? group.blockedAt : isThisMac ? nil : group.lastSeen) { now in
                    if group.isBlocked {
                        group.blockedLine(now: now)
                    } else if isThisMac {
                        group.name
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
            } else if !isThisMac, knowsThisDevice {
                ActionButton(title: DeviceGroup.blockAction, key: nil, prominent: false, id: "\(id).block") {
                    asking = .block(group)
                }
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 12)
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
                    // Never the app's own entry, which has no Block.
                    let blockable = allowed.first { group in !group.devices.contains { $0.deviceID == thisDevice } }
                    return ask == "allow" ? blocked.first.map(Ask.allow) : blockable.map(Ask.block)
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
            devices = nil
            answers = false
        }
    }
}
