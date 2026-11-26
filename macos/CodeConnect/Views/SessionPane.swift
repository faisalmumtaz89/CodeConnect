import CodeConnectKit
import SwiftUI

/// One run, opened beside the list: where it runs, what it is called, how it stands, its conversation, and its changes
/// beside it or, in a narrow pane, in its place. Opening and closing it mark the run reviewed, as the iPhone does.
struct SessionPane: View {
    let row: FleetRow
    let model: AppModel
    /// The run's changes panel is open: the window's, which keeps it for each run.
    @Binding var changesOpen: Bool
    @State private var size: CGSize = .zero
    /// A request holds the reply box's place.
    @State private var boxTaken = false
    /// How tall the reply box stands (none while a request holds its place): a request shares the half pane with it.
    @State private var replyHeight: CGFloat = 0
    /// The reply field (or its command list) holds the keyboard.
    @FocusedValue(\.composerHoldsEscape) private var composerHolds
    @FocusedValue(\.changesFieldFocused) private var changesFieldFocused
    /// The window's Add sheet, and its setup checklist: either covers the window, and then a request's answers take no
    /// keys, as the window's own Esc stands down for the Add sheet.
    @FocusedValue(\.adding) private var adding
    @Environment(Setup.self) private var setup
    @Environment(\.agentsShown) private var agentsShown

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Band(row: row, model: model, changesOpen: $changesOpen)
            // The conversation stays mounted under its changes, so neither loses its place when the pane crosses the
            // width that holds them side by side: there the panel stands beside it, narrower it covers it. Covered, its
            // foot goes: no request is read or answered, and no reply typed, where it can't be seen (the reply's words
            // are the run's, kept).
            VStack(spacing: 0) {
                // Hidden only while covered, and on the conversation alone: an explicit `accessibilityHidden(false)` on
                // the stack would stand over the reply box's own hiding while a request has its place.
                Conversation(model: model, key: row.id)
                    .opacity(inPlace ? 0 : 1)
                    .allowsHitTesting(!inPlace)
                    .disabled(inPlace)
                    .accessibilityHidden(inPlace)
                if !inPlace { foot }
            }
            .onPreferenceChange(HoldsReplyBox.self) { boxTaken = $0 }
            .padding(.trailing, changesOpen && !inPlace ? 1 + Self.panelWidth : 0)
            .overlay(alignment: .trailing) { if changesOpen { changes } }
        }
        #if DEBUG
            .task { if RenderWindow.opensChanges { changesOpen = true } }
        #endif
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .onGeometryChange(for: CGSize.self) { $0.size } action: { size = $0 }
        .preference(key: ChangesInPlace.self, value: inPlace)
        .background(Theme.window)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("session.pane")
        // Seen only while on screen: under Settings nothing is marked, and coming back from it is seeing the run.
        .onAppear { if agentsShown { model.markReviewed(row.id) } }
        .onDisappear { if agentsShown { model.markReviewed(row.id) } }
        .onChange(of: agentsShown) { _, shown in if shown { model.markReviewed(row.id) } }
    }

    /// What waits on you and the reply box, under the conversation.
    @ViewBuilder private var foot: some View {
        // What waits on you sits at the foot, outside the conversation's scroll, in at most half the pane with
        // the reply box when that shows, so the window holds them both at its smallest; its answers take the
        // reply box's place. Not while the reply field holds the keyboard: the box stays the writer's until it
        // lets go (a hunk's comment field, which holds Esc too, is not the reply's). Once a request has the box,
        // the field can't take the keyboard back, so what holds it then is the request's own field.
        PendingRequests(
            model: model, key: row.id, maxHeight: max(0, size.height / 2 - replyHeight),
            held: composerHolds == true && changesFieldFocused != true && !boxTaken,
            keysElsewhere: adding.map { $0.wrappedValue != nil } ?? false || setup.shown || changesFieldFocused == true)
        // Standing aside, the reply box stays mounted: its words, its last outcome and a send on its way are
        // kept. It takes no room, no click, no keyboard (Tab can't reach it) and no reading. Not clipped: its
        // command list opens above it.
        ComposerBar(key: row.id) { changesOpen = true }
            .frame(height: boxTaken ? 0 : nil, alignment: .top)
            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { replyHeight = $0 }
            .opacity(boxTaken ? 0 : 1)
            .allowsHitTesting(!boxTaken)
            .disabled(boxTaken)
            .accessibilityHidden(boxTaken)
    }

    /// The run's changes: beside the conversation, behind a hairline, or in its place, under the row that gives it back.
    private var changes: some View {
        HStack(spacing: 0) {
            if !inPlace { Rectangle().fill(Theme.hairline).frame(width: 1) }
            VStack(spacing: 0) {
                if inPlace {
                    HStack {
                        Spacer()
                        ActionButton(title: "‹ Conversation", key: "Esc", prominent: false, small: true,
                            id: "session.changes.back") { changesOpen = false }
                    }
                    .padding(.horizontal, 12)
                    .frame(height: 38)
                    .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
                }
                ChangesTab(model: model, key: row.id)
                // The run's shell at the panel's foot, sized by itself: its bar closed, its screen open.
                TerminalFoot(row: row, model: model)
            }
            .frame(width: inPlace ? nil : Self.panelWidth)
        }
        .background(Theme.window)
    }

    /// The changes panel's width, beside the conversation.
    static let panelWidth: CGFloat = 480

    /// The changes stand in the conversation's place: the pane can't hold the conversation's column and the panel side
    /// by side. Before the pane has a width, they stand beside it: its first frame on a wide window draws both.
    private var inPlace: Bool {
        changesOpen && size.width > 0 && size.width < Theme.conversationColumn + 1 + Self.panelWidth
    }
}

/// The run's changes panel stands in the conversation's place: Esc gives the conversation back before it closes the run.
struct ChangesInPlace: PreferenceKey {
    static let defaultValue = false
    static func reduce(value: inout Bool, nextValue: () -> Bool) { value = value || nextValue() }
}

extension View {
    /// On the conversation's column (`Theme.conversationColumn`), centred in a wider pane.
    func onConversationColumn() -> some View {
        frame(maxWidth: Theme.conversationColumn, alignment: .leading).frame(maxWidth: .infinity)
    }
}

/// The run's band: what it is called, and under it where it runs, its project, the branch checked out there when git
/// names one, and its computer with that computer's mark, greyed while offline and its link's word beside it while the
/// link is not working, as the run list says it; at its end the changes panel's toggle, with the lines the run has
/// changed.
private struct Band: View {
    let row: FleetRow
    let model: AppModel
    @Binding var changesOpen: Bool
    @Environment(\.headerClearance) private var headerClearance

    var body: some View {
        HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 3) {
                Text(row.label.inline)
                    .font(Theme.ui(13, .semibold))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .accessibilityAddTraits(.isHeader)
                    .accessibilityIdentifier("session.title")
                place
            }
            Spacer(minLength: 0)
            PanelToggle(diff: row.summary.diffStat, open: $changesOpen)
        }
        .padding(.leading, max(20, headerClearance.leading))
        .padding(.trailing, max(14, headerClearance.trailing))
        .frame(height: 52)
        .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
    }

    /// Where the run is. The computer and its link's word give way last, so a long project or branch is what shortens;
    /// the whole line, with the folder as a shell prints it, is the help.
    private var place: some View {
        let link = model.linkHealth
        let live = link.level != .offline
        let branch = row.summary.gitBranch?.displaySafe
        let computer = row.machineName ?? "Unknown computer"
        let word = link.machineWord.map { live ? "· \($0)" : RunListWords.offline }
        return HStack(spacing: 5) {
            Text(row.label.project)
            if let branch {
                Image(systemName: "chevron.right").font(.system(size: 7, weight: .semibold)).accessibilityHidden(true)
                Text(branch).font(Theme.mono(11)).accessibilityIdentifier("session.branch")
            }
            Text(verbatim: "·").accessibilityHidden(true)
            HStack(spacing: 4) {
                ComputerMark(os: model.machineOS)
                Text(computer)
                if let word { Text(word).foregroundStyle(Theme.text3) }
            }
            .foregroundStyle(live ? Theme.text2 : Theme.text3)
            .layoutPriority(1)
        }
        .font(Theme.ui(11.5))
        .foregroundStyle(Theme.text3)
        .lineLimit(1)
        .help(
            [row.label.project, FleetGroup.shellPath(row.summary.shownCwd), branch, computer].compactMap { $0 }
                .joined(separator: " · "))
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("session.place")
    }
}

/// The changes panel's toggle: its mark and the lines the run has changed, lighter while the panel is open. It always
/// opens: the count comes at a turn's end, and an older computer sends none, while the panel reads the changes as they
/// are.
private struct PanelToggle: View {
    let diff: DiffStat?
    @Binding var open: Bool

    var body: some View {
        let changed = diff.flatMap { $0.added + $0.removed > 0 ? $0 : nil }
        Button { open.toggle() } label: {
            HStack(spacing: 6) {
                // Not the run's own toggle's sidebar beside it: what this one shows is lines changed.
                Image(systemName: "plus.forwardslash.minus").font(.system(size: 10))
                if let changed { LineDelta(added: changed.added, removed: changed.removed) }
            }
            .foregroundStyle(Theme.text)
            .padding(.horizontal, 8)
            .frame(height: 22)
            .background(Color.white.opacity(open ? 0.13 : 0.08), in: RoundedRectangle(cornerRadius: 6))
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color.white.opacity(0.06)))
        }
        .buttonStyle(.hover(id: "session.changes.toggle"))
        .help(open ? "Hide Changes" : "Show Changes")
        .accessibilityLabel("Changes")
        .accessibilityValue(changed.map { "+\($0.added) −\($0.removed)" } ?? "")
        .accessibilityAddTraits(open ? .isSelected : [])
        .accessibilityIdentifier("session.changes.toggle")
    }
}

/// Lines added and removed, green and red: the one place those two colours mean something.
struct LineDelta: View {
    let added: UInt64
    let removed: UInt64

    var body: some View {
        HStack(spacing: 5) {
            Text(verbatim: "+\(added)").foregroundStyle(Theme.green)
            Text(verbatim: "−\(removed)").foregroundStyle(Theme.red)
        }
        .font(Theme.mono(11))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("+\(added) −\(removed)")
    }
}

extension CodexProse.CodexTone {
    /// A Kit outcome's tone on the Mac: its colour only where it means something, and one symbol per tone.
    var color: Color {
        switch self {
        case .info, .success: Theme.text3
        case .warning: Theme.amber
        case .danger: Theme.red
        }
    }

    var symbol: String {
        switch self {
        case .info: "info.circle"
        case .success: "checkmark.circle"
        case .warning: "exclamationmark.circle"
        case .danger: "xmark.circle"
        }
    }
}
