import AppKit
import CodeConnectKit
import SwiftUI

/// Settings' sections, in the list's order, under their group headings.
enum SettingsSection: String, CaseIterable, Identifiable {
    case general, computers, devices, folders, accounts

    var id: String { "settings.section.\(rawValue)" }

    var title: String {
        switch self {
        case .general: "General"
        case .computers: "Computers"
        case .devices: DeviceGroup.tabTitle
        case .folders: FolderWords.paneTitle
        case .accounts: "Accounts"
        }
    }

    var symbol: String {
        switch self {
        case .general: "gearshape"
        case .computers: "desktopcomputer"
        case .devices: "iphone"
        case .folders: "folder"
        case .accounts: "person.crop.circle"
        }
    }

    /// The heading a section starts, when it is the first under it.
    var group: String? {
        switch self {
        case .general: nil
        case .computers, .devices, .folders: "Your computers"
        case .accounts: "Agents"
        }
    }
}

/// Settings in the main window, in the sidebar's place: a back row to the agents, then the sections under their
/// headings. One stop for the keyboard: ↑ and ↓ choose the section above or below.
struct SettingsList: View {
    @Binding var section: SettingsSection
    let close: () -> Void
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            // Clear of the traffic lights.
            Color.clear.frame(height: 52)
            Button(action: close) {
                HStack(spacing: 7) {
                    Image(systemName: "chevron.left").font(.system(size: 10.5, weight: .semibold))
                    Text("Settings").font(Theme.ui(13, .semibold))
                }
                .foregroundStyle(Theme.text)
                .padding(.horizontal, 8)
                .frame(height: 28)
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(.hover(radius: 6, id: "settings.back"))
            .help("Back to your agents")
            .accessibilityLabel("Back to your agents")
            .accessibilityIdentifier("settings.back")
            .padding(.horizontal, 8)
            .padding(.bottom, 6)
            VStack(alignment: .leading, spacing: 1) {
                ForEach(SettingsSection.allCases) { each in
                    if let group = each.group, group != previous(each)?.group {
                        Text(group).font(Theme.ui(11, .semibold)).foregroundStyle(Theme.text3)
                            .padding(.horizontal, 10)
                            .padding(.top, 14)
                            .padding(.bottom, 4)
                            .accessibilityAddTraits(.isHeader)
                    }
                    row(each)
                }
            }
            .padding(.horizontal, 8)
            .focusable()
            .focused($focused)
            .focusEffectDisabled()
            .onMoveCommand { direction in
                let all = SettingsSection.allCases
                guard let at = all.firstIndex(of: section) else { return }
                switch direction {
                case .up: section = all[max(at - 1, 0)]
                case .down: section = all[min(at + 1, all.count - 1)]
                default: break
                }
            }
            Spacer()
        }
        .background(Theme.sidebar)
        // The keyboard comes here from whatever had it under Settings.
        .onAppear { focused = true }
    }

    private func previous(_ each: SettingsSection) -> SettingsSection? {
        let all = SettingsSection.allCases
        return all.firstIndex(of: each).flatMap { $0 > 0 ? all[$0 - 1] : nil }
    }

    private func row(_ each: SettingsSection) -> some View {
        let selected = section == each
        return Button {
            section = each
            focused = true
        } label: {
            HStack(spacing: 9) {
                Image(systemName: each.symbol).font(.system(size: 11.5)).foregroundStyle(Theme.text2)
                    .frame(width: 16)
                    .accessibilityHidden(true)
                Text(each.title).font(Theme.ui(12.5)).foregroundStyle(Theme.text)
                Spacer()
            }
            .padding(.horizontal, 10)
            .frame(height: 30)
            .background(selected ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 6))
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 6, id: each.id))
        .focusable(false)
        .accessibilityIdentifier(each.id)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

/// The section chosen, as one centred column. General, Computers and Accounts are grouped cards of rows, a title and
/// one line on the left and one control on the right; Devices and Folders are the panes they were in the Settings
/// window.
struct SettingsPage: View {
    let section: SettingsSection
    let add: () -> Void

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                switch section {
                case .general: titled(section.title) { GeneralSettings() }
                case .computers: titled(section.title) { ComputerSettings(add: add) }
                case .devices: DevicesPane()
                case .folders: FoldersPane()
                case .accounts: titled(section.title) { AccountSettings() }
                }
            }
            .frame(maxWidth: 760, alignment: .leading)
            .padding(.horizontal, 32)
            .padding(.top, 52)
            .padding(.bottom, 32)
            .frame(maxWidth: .infinity)
        }
        .background(Theme.window)
    }

    private func titled<Content: View>(_ title: String, @ViewBuilder _ content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 18) {
            Text(title).font(Theme.ui(19, .semibold)).foregroundStyle(Theme.text)
                .accessibilityAddTraits(.isHeader)
            content()
        }
    }
}

// MARK: - Rows

/// A group of Settings rows: its heading, then a rounded card of rows with hairlines between them.
struct SettingsGroup<Rows: View>: View {
    let title: String
    @ViewBuilder let rows: Rows

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(Theme.ui(12, .semibold)).foregroundStyle(Theme.text3)
                .padding(.leading, 4)
                .accessibilityAddTraits(.isHeader)
            VStack(spacing: 0) {
                Group(subviews: rows) { subviews in
                    ForEach(Array(subviews.enumerated()), id: \.offset) { index, subview in
                        if index > 0 { Rectangle().fill(Theme.hairline).frame(height: 1).padding(.leading, 14) }
                        subview
                    }
                }
            }
            .background(Theme.boxFill, in: RoundedRectangle(cornerRadius: 10))
            .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Theme.hairline))
        }
    }
}

/// One Settings row: a title and one line on the left, one control (or none) on the right.
struct SettingsRow<Control: View>: View {
    let title: String
    let line: String?
    @ViewBuilder let control: Control

    var body: some View {
        HStack(alignment: .center, spacing: 16) {
            VStack(alignment: .leading, spacing: 3) {
                Text(title).font(Theme.ui(13)).foregroundStyle(Theme.text).lineLimit(1).truncationMode(.middle)
                if let line {
                    Text(line).font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 12)
            control
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 12)
    }
}

/// A state in words beside a dot: green for what works, an open grey ring for anything else.
struct SettingsState: View {
    let words: String
    let good: Bool

    var body: some View {
        HStack(spacing: 6) {
            Group {
                if good {
                    Circle().fill(Theme.green)
                } else {
                    Circle().strokeBorder(Theme.text3, lineWidth: 1.2)
                }
            }
            .frame(width: 7, height: 7)
            .accessibilityHidden(true)
            Text(words).font(Theme.ui(12)).foregroundStyle(Theme.text2)
        }
    }
}

// MARK: - General

/// This Mac's Tailscale in a sentence, CodeConnect's version and its updates, its service, and where to find help.
private struct GeneralSettings: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup
    @Environment(\.updates) private var updates
    @State private var address: LocalDaemon.Address?
    private static let support = URL(string: "https://codeconnect.sh/support")!
    private static let privacy = URL(string: "https://codeconnect.sh/privacy")!

    private var version: String {
        (Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String).map { "CodeConnect \($0)" }
            ?? "CodeConnect"
    }

    /// What this Mac's own daemon last said about its Tailscale.
    private var status: TailscaleNote.Status? {
        var directory = setup.thisMac(at: address).flatMap { machines.model(for: $0)?.tailnet }
        #if DEBUG
            if let peers = RenderWindow.peers { directory = peers }
        #endif
        // Read as the Add sheet reads it.
        return directory.map { TailscaleNote.Status($0, onLoopback: TailscaleNote.onLoopback(address)) }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 24) {
            SettingsGroup(title: "Tailscale") {
                if let status {
                    SettingsRow(title: tailscaleTitle(status), line: tailscaleLine(status)) { tailscaleControl(status) }
                } else {
                    SettingsRow(title: "Tailscale", line: "CodeConnect hasn’t heard from this Mac’s service yet.") {
                        EmptyView()
                    }
                }
            }
            SettingsGroup(title: "Updates") {
                SettingsRow(title: version, line: "CodeConnect looks for a new version on its own and asks before installing it.") {
                    if let updates {
                        ActionButton(title: "Check for Updates…", key: nil, prominent: false, id: "settings.updates") {
                            updates.checkForUpdates()
                        }
                    }
                }
            }
            SettingsGroup(title: "This Mac") {
                SettingsRow(
                    title: "CodeConnect’s service",
                    line: "Runs your agents on this Mac and lets your iPhone and other computers reach them."
                ) {
                    ActionButton(title: "Set Up…", key: nil, prominent: false, id: "settings.setup") {
                        setup.reopen(machines: machines)
                    }
                }
            }
            SettingsGroup(title: "About") {
                SettingsRow(title: "Support", line: "Help with CodeConnect, and a way to reach us.") {
                    ActionButton(title: "Open", key: nil, prominent: false, id: "settings.support") {
                        NSWorkspace.shared.open(Self.support)
                    }
                }
                SettingsRow(title: "Privacy policy", line: "What CodeConnect keeps, and where.") {
                    ActionButton(title: "Open", key: nil, prominent: false, id: "settings.privacy") {
                        NSWorkspace.shared.open(Self.privacy)
                    }
                }
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

    /// Once signed in, what that trusts; otherwise what Tailscale's state is, in the Add sheet's words.
    private func tailscaleLine(_ status: TailscaleNote.Status) -> String {
        if case .signedIn = status { return TailscaleNote.trust }
        return status.line
    }

    private func tailscaleTitle(_ status: TailscaleNote.Status) -> String {
        if case .signedIn(let login?) = status { return login }
        return "Tailscale"
    }

    @ViewBuilder private func tailscaleControl(_ status: TailscaleNote.Status) -> some View {
        switch status {
        case .signedIn: SettingsState(words: "Connected", good: true)
        case .moving: SettingsState(words: "Connecting", good: false)
        default:
            if let action = TailscaleNote.action(status) {
                ActionButton(title: action.title, key: nil, prominent: false, id: "settings.tailscale") {
                    NSWorkspace.shared.open(action.url)
                }
            }
        }
    }
}

// MARK: - Computers

/// The paired computers, each with how its link is, and Remove… for every one but this Mac; then Add Computer….
private struct ComputerSettings: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup
    let add: () -> Void
    @State private var removing: AppModel?
    @State private var address: LocalDaemon.Address?

    var body: some View {
        let computers = machines.models.filter(\.showsFleet)
        let thisMac = setup.thisMac(at: address)
        VStack(alignment: .leading, spacing: 24) {
            SettingsGroup(title: "Paired with this Mac") {
                if computers.isEmpty {
                    SettingsRow(title: FleetWords.noComputersYet, line: nil) { EmptyView() }
                }
                ForEach(computers, id: \.machineID) { computer in
                    let id = computer.machineID ?? ""
                    let link = computer.linkHealth
                    SettingsRow(title: computer.machineName ?? id, line: id == thisMac ? "This Mac" : nil) {
                        HStack(spacing: 12) {
                            SettingsState(words: link.word, good: link.machineWord == nil)
                            if id != thisMac {
                                ActionButton(
                                    title: "Remove…", key: nil, prominent: false, destructive: true,
                                    id: "settings.computer.\(id).remove"
                                ) { removing = computer }
                            }
                        }
                    }
                }
            }
            ActionButton(title: "Add Computer…", key: "⇧⌘N", prominent: false, id: "settings.computers.add", action: add)
        }
        .removesComputer($removing)
        .task(id: computers.map(\.machineID)) {
            guard let root = setup.installer?.root else { return }
            address = await Task.detached { LocalDaemon.address(root: root) }.value ?? address
        }
    }
}

// MARK: - Accounts

/// Which agents are signed in on this Mac, as its service says when asked what it can start, asked again each time the
/// section opens: what the app can read today, and nothing it cannot change.
private struct AccountSettings: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup

    /// This Mac's own computer, by the id its service has, whether or not its link is up now; in a render, the
    /// fixtures' first, which stands for it.
    private var model: AppModel? {
        #if DEBUG
            if RenderWindow.isRendering { return machines.models.first }
        #endif
        return setup.localMachineID.flatMap { machines.model(daemonID: $0) }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 24) {
            Text("The accounts Claude Code and Codex use on this Mac.")
                .font(Theme.ui(13)).foregroundStyle(Theme.text2)
            SettingsGroup(title: "On this Mac") {
                if let model, let notice = model.linkHealth.notice {
                    // Its link is down or not yet up: what the computers' rows say about it, in the place of an answer
                    // its service can't stand behind now. The rows come back when it answers again.
                    SettingsRow(title: notice.title, line: notice.sentence) { EmptyView() }
                } else if let model, !model.starter.agents.isEmpty {
                    ForEach(model.starter.agents, id: \.agent) { status in
                        let installed = model.starter.availableAgents.contains(status.agent)
                        // A missing agent's own sentence says why; a signed-in state needs none.
                        SettingsRow(title: status.agent.displayName, line: installed ? nil : status.detail) {
                            SettingsState(words: words(status, installed: installed), good: installed && status.signedIn == true)
                        }
                    }
                } else if model == nil, setup.settled {
                    SettingsRow(title: "This Mac isn’t set up yet", line: "Set Up… in General sets it up.") { EmptyView() }
                } else {
                    SettingsRow(title: "Asking this Mac…", line: nil) { EmptyView() }
                }
            }
        }
        .task(id: model?.machineID) { model?.listProjects() }
    }

    /// From the fields that mean it: installed or not, then signed in, signed out, or not said (an older service, or
    /// an agent it couldn't ask).
    private func words(_ status: StartAgentStatus, installed: Bool) -> String {
        guard installed else { return "Not installed" }
        switch status.signedIn {
        case true?: return "Signed in"
        case false?: return "Not signed in"
        case nil: return "Not known"
        }
    }
}

/// CodeConnect › Settings… (⌘,): the main window on Settings, opening the window first when it is closed.
struct SettingsCommands: Commands {
    @Environment(\.openWindow) private var openWindow
    @FocusedValue(\.windowCovered) private var windowCovered
    @Binding var settings: SettingsSection?

    var body: some Commands {
        CommandGroup(replacing: .appSettings) {
            Button("Settings\u{2026}") {
                if settings == nil { settings = .general }
                openWindow(id: "main")
            }
            .keyboardShortcut(",")
            .disabled(windowCovered == true)
        }
    }
}

extension EnvironmentValues {
    /// The app's updater, for Settings' Updates row (CodeConnect › Check for Updates…); nil when this build has none.
    @Entry var updates: Updates?
}
