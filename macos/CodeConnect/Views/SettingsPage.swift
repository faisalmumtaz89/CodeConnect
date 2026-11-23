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

/// This Mac's agents and the accounts they start on. A service that keeps accounts per agent (minor 48) is asked for
/// them each time the section opens: per agent its Default, then the accounts added, oldest first, a check on the one
/// new runs use, Use on the others, and Add Account…. Only the account in use is known to be signed in or not, by the
/// agents' own answer; the others carry no such word. An older service shows which agents are signed in, and nothing
/// it cannot change.
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
            if let model, model.keepsAccounts, model.linkHealth.notice == nil {
                Text("Each agent uses the account marked here. Click another to move its sessions to it.")
                    .font(Theme.ui(13)).foregroundStyle(Theme.text2)
                ForEach(StartAgent.allCases) { agent in AgentAccounts(model: model, agent: agent) }
                if let problem = model.accountsProblem {
                    Text(problem).font(Theme.ui(12)).foregroundStyle(Theme.red)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("settings.accounts.problem")
                }
            } else {
                Text("The accounts Claude Code and Codex use on this Mac.")
                    .font(Theme.ui(13)).foregroundStyle(Theme.text2)
                SettingsGroup(title: "On this Mac") {
                    if let model, let notice = model.linkHealth.notice {
                        // Its link is down or not yet up: what the computers' rows say about it, in the place of an
                        // answer its service can't stand behind now. The rows come back when it answers again.
                        SettingsRow(title: notice.title, line: notice.sentence) { EmptyView() }
                    } else if let model, !model.starter.agents.isEmpty {
                        ForEach(model.starter.agents, id: \.agent) { status in
                            let installed = model.starter.availableAgents.contains(status.agent)
                            // A missing agent's own sentence says why; a signed-in state needs none.
                            SettingsRow(title: status.agent.displayName, line: installed ? nil : status.detail) {
                                SettingsState(
                                    words: AccountWords.state(status, installed: installed),
                                    good: installed && status.signedIn == true)
                            }
                        }
                    } else if model == nil, setup.settled {
                        SettingsRow(title: "This Mac isn’t set up yet", line: "Set Up… in General sets it up.") { EmptyView() }
                    } else {
                        SettingsRow(title: "Asking this Mac…", line: nil) { EmptyView() }
                    }
                }
            }
        }
        // Asked again when the link comes back, so a refusal from before is gone once it asks.
        .task(id: "\(model?.machineID ?? "")\(model?.connection.phase.isConnected == true)") {
            model?.listProjects()
            await model?.loadAccounts()
        }
    }
}

/// One agent's accounts on this Mac, as design A draws them (decision 307): its Default, then the accounts added,
/// each by the email its agent signs in as and its plan beneath; the share used of each window the agent names, in
/// columns captioned on the group's title line; the row in use filled, with a check at its leading edge; a click on
/// another uses it. "…" on hover (and a right-click) holds Sign In Again… on a signed-out row and Remove… on an added
/// one. "Add account…" is the group's quiet last row: it adds an account and starts its sign-in in the browser at once,
/// no name asked (the row reads its email once signed in). Under the group, a switch in progress in words.
private struct AgentAccounts: View {
    let model: AppModel
    let agent: StartAgent
    @State private var removing: AgentAccount?

    /// The windows the agent names across its accounts, in the order it names them: the group's columns.
    private func columns(_ book: AccountBook) -> [Int] {
        let all = ([book.defaultFacts(for: agent)] + book.accounts(for: agent).map(\.facts)).compactMap { $0 }
        var seen: [Int] = []
        for window in all.flatMap(\.windows) where !seen.contains(window.windowMinutes) { seen.append(window.windowMinutes) }
        return seen
    }

    var body: some View {
        let book = model.accountBook
        let active = book?.activeID(for: agent)
        let columns = book.map(columns) ?? []
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 0) {
                Text(agent.displayName).font(Theme.ui(12, .semibold)).foregroundStyle(Theme.text3)
                Spacer()
                ForEach(columns, id: \.self) { minutes in
                    Text("\(AccountSwitchWords.caption(minutes: minutes)) used").font(Theme.ui(10.5)).foregroundStyle(Theme.text3)
                        .frame(minWidth: AccountRow.column, alignment: .trailing)
                }
                Color.clear.frame(width: AccountRow.trailing, height: 1)
            }
            .padding(.leading, 4)
            VStack(spacing: 0) {
                if let book {
                    AccountRow(
                        model: model, agent: agent, id: nil, facts: book.defaultFacts(for: agent), label: "Default",
                        signedOut: false, signingIn: false, note: nil, columns: columns, inUse: active == nil,
                        choose: { await model.activateAccount(agent: agent, accountID: nil) }, acts: [], remove: {})
                    ForEach(book.accounts(for: agent)) { account in
                        AccountRow(
                            model: model, agent: agent, id: account.id, facts: account.facts, label: account.label,
                            signedOut: account.signedOut, signingIn: account.signingIn,
                            note: note(account, in: book), columns: columns, inUse: active == account.id,
                            choose: { await model.choose(account) }, acts: AccountRowAct.acts(for: account),
                            remove: { removing = account })
                    }
                    Button { Task { await model.addAndSignIn(agent: agent) } } label: {
                        HStack(spacing: 10) {
                            Image(systemName: "plus").font(.system(size: 11, weight: .medium)).frame(width: AccountRow.lead)
                            Text("Add account…").font(Theme.ui(13))
                            Spacer()
                        }
                        .foregroundStyle(Theme.text3)
                        .padding(.horizontal, 10).padding(.vertical, 9)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .disabled(!model.canAddAccount(agent: agent))
                    .accessibilityIdentifier("settings.accounts.\(agent.rawValue).add")
                } else {
                    // Not yet answered: no account is shown as in use before the computer says which.
                    Text("Asking this Mac…").font(Theme.ui(13)).foregroundStyle(Theme.text3)
                        .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 10).padding(.vertical, 9)
                }
            }
            .padding(6)
            .background(Theme.boxFill, in: RoundedRectangle(cornerRadius: 10))
            .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Theme.hairline))
            if let move = switchInProgress {
                if let sentence = move.sentence { line(sentence, Theme.text3, id: "switching") }
                if let failure = move.failure { line(failure, Theme.amber, id: "switchFailed") }
            }
        }
        .confirmation(Binding(get: { removing }, set: { removing = $0 })) { account in
            Confirmation(
                title: "Remove \(account.facts?.email ?? account.label)?",
                message: "\(agent.displayName) signs it out of this Mac. New runs use another account.",
                answers: [
                    .init(title: "Cancel", kind: .cancel, id: "settings.accounts.remove.cancel"),
                    .init(title: "Remove", kind: .destructive, id: "settings.accounts.remove.confirm") {
                        Task { await model.removeAccount(accountID: account.id) }
                    },
                ])
        }
    }

    /// The switch in progress, as the sessions' own summaries say it.
    private var switchInProgress: AccountSwitch? {
        #if DEBUG
            if let fixture = AccountSwitch.fixture { return fixture }
        #endif
        return model.accountSwitch(for: agent)
    }

    /// A row's quiet second line when the account isn't a sign-in of its own.
    private func note(_ account: AgentAccount, in book: AccountBook) -> String? {
        if account.notSeparate { return AccountSwitchWords.notSeparate }
        return account.sameAsAnother ? AccountSwitchWords.sameSignIn : nil
    }

    private func line(_ text: String, _ color: Color, id: String) -> some View {
        Text(text).font(Theme.ui(11.5)).foregroundStyle(color).fixedSize(horizontal: false, vertical: true)
            .padding(.leading, 4)
            .accessibilityIdentifier("settings.accounts.\(agent.rawValue).\(id)")
    }
}

/// One account's row, the whole of it the choice: the email (else its name) and its plan, "Signed out" in amber, or
/// its quiet note beneath; its figures under the group's columns, right-aligned, an empty cell where it names no such
/// window. In use: the Settings selection fill and a leading check; a sign-in waiting on the browser: a spinner there.
/// Under the pointer: the lighter hover fill, the pointing hand, and, for a row with acts, a "…" at the trailing edge
/// opening the window's row menu (`RowMenu`), as a run's does (`RunMore`); a right-click on the row opens the same menu
/// at the pointer. The "…" stands over the row's button, never inside it, so a click anywhere on it never chooses the row.
private struct AccountRow: View {
    let model: AppModel
    let agent: StartAgent
    let id: String?
    let facts: AccountFacts?
    let label: String
    let signedOut: Bool
    let signingIn: Bool
    let note: String?
    let columns: [Int]
    let inUse: Bool
    /// What a click does: use it, or sign a signed-out one in again (`AppModel.choose`).
    let choose: () async -> Void
    /// What its "…" offers (`AccountRowAct.acts`); none, no "…".
    let acts: [AccountRowAct]
    /// Remove…: asks first, by the page's card.
    let remove: () -> Void
    @Environment(RowMenu.self) private var menu
    @State private var hovered = false
    @State private var moreFrame: CGRect = .zero

    static let lead: CGFloat = 16
    /// A figure's column: its longest countdown line ("resets in 47h 59m", 99 pt at 11.5 pt) and a 16 pt gap before
    /// the next, so two long lines side by side never read as one.
    static let column: CGFloat = 116
    /// The "…"'s slot and its gap, kept on every row so nothing moves.
    static let trailing: CGFloat = 6 + 10 + 30
    /// The "…"'s slot in the row, which its button stands centred over: as wide as the one it replaced, so the figures
    /// keep their places under the captions.
    private static let slot: CGFloat = 22

    private var rowID: String { "settings.accounts.\(agent.rawValue).\(id ?? "default")" }
    private var moreID: String { "\(rowID).more" }
    private var menuOpen: Bool { menu.shown?.owner == moreID }
    /// Under the pointer, or drawn as if (a test launch's `-CC_HOVER <row id>`).
    private var lit: Bool { hovered || HoverButtonStyle.forced(rowID) }

    var body: some View {
        Button {
            guard !inUse else { return }
            Task { await choose() }
        } label: {
            HStack(spacing: 8) {
                HStack(spacing: 10) {
                    ZStack {
                        Color.clear
                        if signingIn {
                            ProgressView().controlSize(.mini)
                        } else if inUse {
                            Image(systemName: "checkmark").font(.system(size: 12, weight: .semibold)).foregroundStyle(Theme.text)
                                .accessibilityLabel("In use")
                                .accessibilityIdentifier("\(rowID).inUse")
                        }
                    }
                    .frame(width: Self.lead, height: 16)
                    // Every row stands two lines tall; one with no second line centres its name on the row, as
                    // its figures are.
                    ZStack(alignment: .leading) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(" ").font(Theme.ui(13))
                            Text(" ").font(Theme.ui(11.5))
                        }
                        .hidden()
                        VStack(alignment: .leading, spacing: 2) {
                            Text(facts?.email ?? label).font(Theme.ui(13)).foregroundStyle(Theme.text)
                                .lineLimit(1).truncationMode(.middle)
                                .accessibilityIdentifier("\(rowID).email")
                            if !secondLine.isEmpty {
                                Text(secondLine).font(Theme.ui(11.5)).foregroundStyle(signedOut ? Theme.amber : Theme.text3)
                                    .lineLimit(1)
                                    .accessibilityIdentifier("\(rowID).line")
                            }
                        }
                    }
                    Spacer(minLength: 12)
                    figures
                }
                Color.clear.frame(width: Self.slot, height: 1)
            }
            .padding(.horizontal, 10).padding(.vertical, 9)
            .background(
                inUse ? Theme.selected : lit ? Color.white.opacity(Theme.hoverLift) : .clear,
                in: RoundedRectangle(cornerRadius: 6))
            .contentShape(Rectangle())
        }
        .accessibilityIdentifier(rowID)
        .buttonStyle(.plain)
        .disabled(model.accountsBusy)
        // For VoiceOver, which does not hover: the "…"'s acts as the row's own.
        .accessibilityActions {
            ForEach(acts, id: \.self) { act in
                Button(act.title) { perform(act) }.accessibilityIdentifier("\(rowID).\(Self.name(act))Action")
            }
        }
        .overlay(alignment: .trailing) {
            if !acts.isEmpty, lit || menuOpen || HoverButtonStyle.forced(moreID) {
                more.padding(.trailing, 10 + (Self.slot - RunMore.width) / 2)
            }
        }
        .onRightClick { point in
            guard !acts.isEmpty, !model.accountsBusy else { return }
            show(at: CGRect(origin: point, size: .zero))
        }
        .onHover { inside in
            hovered = inside
            if inside { NSCursor.pointingHand.push() } else { NSCursor.pop() }
        }
    }

    /// Its share used of each column's window, right-aligned, on the name's line, and beneath it, on the plan's, how long
    /// until that window resets (`AccountSwitchWords.resets`, kept current each minute while the page is shown), the
    /// exact time as its tooltip; an empty cell where it names no such window. Or, in their place, "No plan limits"
    /// where its agent says none apply (`limitsAvailable` false), else "Not reported yet".
    @ViewBuilder private var figures: some View {
        if let facts, !facts.windows.isEmpty {
            // Redrawn each minute while shown; each countdown is from the time of that drawing, since the schedule's own
            // date is the minute's start, up to 59 s earlier.
            TimelineView(.everyMinute) { _ in
                let now = Date.now
                HStack(spacing: 0) {
                    ForEach(columns, id: \.self) { minutes in
                        let window = facts.windows.first { $0.windowMinutes == minutes }
                        VStack(alignment: .trailing, spacing: 2) {
                            Text(window.map { "\($0.usedPercent)%" } ?? " ").font(Theme.ui(13)).monospacedDigit()
                                .foregroundStyle((window?.usedPercent ?? 0) >= 90 ? Theme.amber : Theme.text2)
                            Text(window.flatMap { AccountSwitchWords.resets($0, now: now) } ?? " ")
                                .font(Theme.ui(11.5)).monospacedDigit().foregroundStyle(Theme.text3).lineLimit(1)
                        }
                        .frame(width: Self.column, alignment: .trailing)
                        .help(window.map { Self.resetTime($0) } ?? "")
                    }
                }
            }
            .accessibilityIdentifier("\(rowID).usage")
        } else {
            Text(facts?.limitsAvailable == false ? AccountSwitchWords.noPlanLimits : AccountSwitchWords.notReported)
                .font(Theme.ui(12)).foregroundStyle(Theme.text3)
                .accessibilityIdentifier("\(rowID).usage")
        }
    }

    /// "Resets Thursday at 9:00 AM", in the person's own calendar and clock.
    private static func resetTime(_ window: UsageWindow) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(window.resetsAt))
        return "Resets \(date.formatted(.dateTime.weekday(.wide))) at \(date.formatted(date: .omitted, time: .shortened))"
    }

    /// The plan in the agent's own word, capitalized; "Signed out"; or the row's note; "As set up on this Mac" for a
    /// Default whose agent hasn't said.
    private var secondLine: String {
        if signedOut { return "Signed out" }
        if let note { return note }
        if signingIn { return AccountSwitchWords.signingIn }
        if let plan = facts?.plan { return plan.prefix(1).uppercased() + plan.dropFirst() }
        return id == nil ? "As set up on this Mac" : ""
    }

    /// The row's "…", the app's row-menu button (`RunMore`'s size, hover and open state): it opens the menu below itself.
    private var more: some View {
        Button { show(at: moreFrame) } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Theme.text2)
                .frame(width: RunMore.width, height: 22)
                .background(menuOpen ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
                .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: moreID))
        .focusable(false)
        .disabled(model.accountsBusy)
        .help("More")
        .accessibilityLabel("More")
        .accessibilityIdentifier(moreID)
        .accessibilityValue(menuOpen ? "open" : "closed")
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { moreFrame = $0 }
        #if DEBUG
            .task {
                guard let id, RenderWindow.openAccountMenu == id else { return }
                // After the render has sized and placed its window, as a click comes on a window at rest.
                try? await Task.sleep(for: .seconds(1))
                show(at: moreFrame)
            }
        #endif
    }

    /// The row's menu by `rect` (its "…", or a right-click's point as a zero-size rect).
    private func show(at rect: CGRect) {
        let items = acts.map { act in
            RowMenu.Item(
                id: "\(rowID).\(Self.name(act))", title: act.title,
                symbol: act == .remove ? PanelSymbol.remove : PanelSymbol.signIn, destructive: act == .remove
            ) { perform(act) }
        }
        menu.toggle(rect, owner: moreID, items: items)
    }

    private func perform(_ act: AccountRowAct) {
        switch act {
        case .signInAgain:
            guard let id else { return }
            Task { await model.signInAccount(accountID: id) }
        case .remove:
            remove()
        }
    }

    private static func name(_ act: AccountRowAct) -> String { act == .remove ? "remove" : "signInAgain" }
}

#if DEBUG
    extension AccountSwitch {
        /// `-CC_ACCOUNTS_SWITCH <target>,<waiting>,<moved>[,<failed session>…]`: a render's page as a switch toward
        /// `target` stands.
        static var fixture: AccountSwitch? {
            guard let raw = UserDefaults.standard.string(forKey: "CC_ACCOUNTS_SWITCH") else { return nil }
            let parts = raw.split(separator: ",").map(String.init)
            guard parts.count >= 3, let waiting = Int(parts[1]), let moved = Int(parts[2]) else { return nil }
            return AccountSwitch(target: parts[0], waiting: waiting, moved: moved, failed: Array(parts.dropFirst(3)))
        }
    }
#endif

/// An agent's sign-in, from the fields that mean it: installed or not, then signed in, signed out, or not said (an
/// older service, or an agent it couldn't ask).
private enum AccountWords {
    static func state(_ status: StartAgentStatus, installed: Bool) -> String {
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
