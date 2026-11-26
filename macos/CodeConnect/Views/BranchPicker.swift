import CodeConnectKit
import SwiftUI

/// Where in its repository a start runs, in the box's bar: by default a new worktree whose branch is named from the task
/// (nothing chosen), else the folder as it is, a worktree the repository has, a new worktree for a branch no checkout
/// has, or a new one by name. The row is there only while the computer can say what
/// the folder's repository holds and has not said it is outside one (`StartContext.branchAhead`), as the iPhone's
/// Branch step is; until it has said, the row says it is reading. What it offers and every word are Kit's.
struct BranchRow: View {
    let context: StartContext
    let folder: StartProject
    let agent: StartAgent?
    /// The place chosen; nil is the default, a new worktree named from the task.
    @Binding var place: StartPlace?
    /// A new worktree's name while one is being named, else nil.
    @Binding var newName: String?
    @Binding var open: Bool
    /// The highest the menu may reach in the form's coordinates (`StartForm.space`).
    let top: CGFloat
    /// A new worktree's name was kept with Return: the menu has closed, and the keyboard goes back to the form.
    let named: () -> Void

    @Environment(\.startBox) private var startBox
    /// The row, label to last line, in the form's coordinates.
    @State private var frame: CGRect = .zero
    /// How tall the menu's naming of a new worktree stands, while one is being named.
    @State private var namingHeight: CGFloat = 0
    /// The menu lists the branches to start a new worktree from, in place of its own rows.
    @State private var listing = false

    var body: some View {
        Group {
            if let answer = context.git(folder.path), let git = answer {
                // A new worktree being named is named in the menu (`BranchNaming`); here, its kind.
                let summary = place.map { startPlaceSummary($0, agent: agent ?? .claude, project: folder, git: git) }
                if newName == nil, let summary {
                    button(summary, branch: true)
                } else {
                    button(StartWords.newWorktree, branch: false)
                }
            } else {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text(StartWords.readingBranches(folder.name)).font(Theme.ui(12)).foregroundStyle(Theme.text2)
                        .lineLimit(1).truncationMode(.tail)
                }
                .accessibilityIdentifier("start.branch.reading")
            }
        }
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(StartForm.space)) } action: { frame = $0 }
        .overlay(alignment: .topLeading) {
            if open, let answer = context.git(folder.path), let git = answer { menu(git) }
        }
        // Around the menu as well as the button: the menu says where it is through what this sets, so a click on one
        // of its rows is a click inside, not one that closes it before the row is chosen.
        .closesPanel($open)
        .onChange(of: open) {
            listing = false
            #if DEBUG
                if open && RenderWindow.opensBranchList { listing = true }
            #endif
        }
    }

    /// The least a menu keeps from the form's edges.
    private static let margin: CGFloat = 8

    /// The menu, as tall as its rows up to `BranchMenu.mostRows`, above its chip as every menu of the box's bar opens
    /// (screen 34), the panels' one gap from it: as many whole rows as fit between the chip and `top`, the rest scrolled
    /// to.
    private func menu(_ git: StartGit) -> some View {
        let rows = BranchMenu.rows(git: git, folder: folder, listing: listing)
        let gap = Theme.panelGap
        let highest = max(top, Self.margin)
        // The naming shows under the menu's own rows, not under the branches' list.
        let naming = newName == nil || listing ? 0 : namingHeight
        let above = frame.minY - gap - highest - naming
        let shown = BranchMenu.rows(fitting: above, of: rows)
        let placed = PanelPlace.place(widest: BranchMenu.width, leading: frame.minX, box: startBox)
        return BranchMenu(
            git: git, folder: folder, place: place, listing: $listing,
            // Inside the box by the start form's one rule (`PanelPlace`), not as narrow as its chip, whose names would
            // be cut that need not be.
            shownRows: shown, width: placed.width,
            naming: newName == nil
                ? nil : BranchNaming(folder: folder, agent: agent, git: git, newName: $newName) {
                    open = false
                    named()
                },
            namingHeight: $namingHeight
        ) { chosen in
            switch chosen {
            case .task, .place:
                if case .place(let chosen) = chosen { place = chosen } else { place = nil }
                newName = nil
                open = false
            case .name:
                // The menu stays open on its name, under "New Branch…".
                newName = ""
            }
        }
        .openPanel()
        .offset(
            x: placed.x - frame.minX,
            y: -(BranchMenu.height(rows: shown) + naming + gap))
    }

    /// The chosen place, a button that opens the menu.
    /// `branch`: the title is a branch, in a terminal's type and cut in the middle, where its prefix and its end both
    /// show; else words ("New worktree"), in the bar's type and cut at their end, as the other chips are.
    private func button(_ title: String, branch: Bool) -> some View {
        Button {
            open.toggle()
        } label: {
            HStack(spacing: 5) {
                Image(systemName: "arrow.triangle.branch").font(.system(size: 11)).accessibilityHidden(true)
                // A branch keeps meaning at both ends ("feature/oauth-device-flow"): cut in its middle.
                Text(verbatim: title).font(branch ? Theme.mono(12) : InputBoxChip.font).lineLimit(1)
                    .truncationMode(branch ? .middle : .tail)
                Image(systemName: "chevron.down").font(.system(size: 8.5, weight: .semibold))
            }
            .foregroundStyle(Theme.text)
            .inputBoxChip(open: open)
        }
        .buttonStyle(.hover(radius: 8, id: "start.branch"))
        .help(title)
        .accessibilityIdentifier("start.branch")
        .accessibilityValue(open ? "open" : "closed")
    }
}

/// A new worktree being named, in the branch menu under "New branch…": the name, the rule or what breaks it, and what
/// the name makes. While it has the keyboard the menu's keys are its own (`panelField`): Return keeps the name and
/// closes the menu; Escape empties a typed name (the field's own), then stops naming, then closes the menu.
struct BranchNaming: View {
    let folder: StartProject
    let agent: StartAgent?
    let git: StartGit
    @Binding var newName: String?
    let done: () -> Void
    @FocusState private var focused: Bool

    var body: some View {
        let trimmed = (newName ?? "").trimmingCharacters(in: .whitespaces)
        let valid = StartPlace.isValidName(trimmed)
        let here = git.checkout(at: folder.path)
        return VStack(alignment: .leading, spacing: 5) {
            TextField(
                "", text: Binding(get: { newName ?? "" }, set: { newName = $0 }),
                prompt: Text(StartWords.worktreeNameExample).foregroundStyle(Theme.text3)
            )
            .accessibilityLabel(StartWords.worktreeName)
            .accessibilityIdentifier("start.branch.name")
            .textFieldStyle(.plain)
            .font(Theme.mono(12))
            .menuField()
            .overlay { if !(trimmed.isEmpty || valid) { RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.red) } }
            .onSubmit(done)
            .onExitCommand { newName = nil }
            // It appears because "New branch…" was just chosen: what is typed next is its name.
            .takesKeyboardWhenShown($focused)
            // Return, Esc and the arrows are the name's, not the menu's.
            .panelField($focused)
            Text(trimmed.isEmpty || valid ? StartWords.worktreeNameRule : StartWords.worktreeNameBroken)
                .font(Theme.ui(11))
                .foregroundStyle(trimmed.isEmpty || valid ? Theme.text3 : Theme.red)
                .accessibilityIdentifier("start.branch.rule")
            if let agent {
                // Each on a line of its own, as Changes is: the menu is narrower than the box.
                VStack(alignment: .leading, spacing: 5) {
                    fact(StartWords.branch) {
                        // As the computer will make it: the next free name where the one typed is taken.
                        Text(
                            verbatim: valid
                                ? StartPlace.newBranch(StartPlace.free(trimmed, for: agent, in: git), for: agent)
                                : StartWords.notNamedYet)
                            .font(valid ? Theme.mono(11) : Theme.ui(11))
                    }
                    fact(StartWords.from) { Text(StartPlace.newWorktreeBase(for: agent, from: here)) }
                }
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("start.branch.facts")
                if here?.dirty == true {
                    fact(StartWords.changes) { Text(StartWords.changesStay) }
                        .accessibilityElement(children: .combine)
                        .accessibilityIdentifier("start.branch.changes")
                }
            }
        }
    }
}

/// One fact about the new worktree, on one line: its label whole, then its value, cut in the middle where too long.
private func fact(_ label: String, @ViewBuilder value: () -> some View) -> some View {
    HStack(alignment: .firstTextBaseline, spacing: 5) {
        Text(label).foregroundStyle(Theme.text3).fixedSize()
        value().foregroundStyle(Theme.text2).lineLimit(1).truncationMode(.middle)
    }
    .font(Theme.ui(11))
}


/// The places a start can run in a repository (screen 34): the folder's own checkout; a new worktree, by default named
/// from the task or from a branch no checkout has; the repository's other worktrees (one whose folder is gone is shown,
/// never chosen); then a new one by name. "From a branch…" lists the branches in the menu's place, as the folder
/// browser lists folders: a line naming the list, ".." back, and ← or ⌫ back too.
///
/// Every line, a section's heading included, is one `rowHeight` tall, so a menu cut to `shownRows` ends on a whole
/// line and scrolls to the rest. Its foot ("New Branch…", or the note on older branches under the branches) stays in
/// sight under the lines that scroll: naming a new worktree is never scrolled away.
struct BranchMenu: View {
    enum Choice {
        /// A new worktree named from the task: nothing chosen.
        case task
        case place(StartPlace)
        /// A new worktree by name, named under "New Branch…".
        case name
    }

    let git: StartGit
    let folder: StartProject
    /// The chosen place, checked; nil is the new worktree named from the task, checked unless one is being named.
    let place: StartPlace?
    @Binding var listing: Bool
    /// How many lines show before it scrolls.
    let shownRows: Int
    let width: CGFloat
    /// A new worktree's naming, under "New Branch…" while one is being named, and how tall it stands.
    let naming: BranchNaming?
    @Binding var namingHeight: CGFloat
    let choose: (Choice) -> Void

    @FocusState private var backKeys: Bool

    static let width: CGFloat = 440
    /// A line: a menu row and its gap (`MenuRow`).
    static let rowHeight: CGFloat = MenuMetrics.rowHeight + MenuMetrics.rowGap
    static let mostRows = 12
    /// The card's padding (`menuCard`), above and below the rows.
    private static let inset = MenuMetrics.inset

    /// The menu's lines for this folder: each section's heading and entries, or the branches' list.
    static func rows(git: StartGit, folder: StartProject, listing: Bool) -> Int {
        if listing { return 2 + git.branches.count + (git.branchesTruncated ? 1 : 0) }
        let others = git.others(than: folder.path).count
        return (git.checkout(at: folder.path) == nil ? 0 : 2) + 2 + (git.branches.isEmpty ? 0 : 1)
            + (others == 0 ? 0 : 1 + others) + 1
    }

    /// The menu's height showing `rows` lines.
    static func height(rows: Int) -> CGFloat { CGFloat(rows) * rowHeight + 2 * inset }

    /// The most whole lines of `rows`, up to `mostRows`, that fit in `room`; at least the foot and one line over it.
    static func rows(fitting room: CGFloat, of rows: Int) -> Int {
        max(3, min(rows, mostRows, Int(((room - 2 * inset) / rowHeight).rounded(.down))))
    }

    /// The lines that never scroll.
    private var foot: Int { listing ? (git.branchesTruncated ? 1 : 0) : 1 }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            PanelScroll {
                VStack(alignment: .leading, spacing: 0) {
                    if listing { branches } else { places }
                }
            }
            .frame(height: CGFloat(shownRows - foot) * Self.rowHeight)
            if listing {
                if git.branchesTruncated {
                    Text(StartWords.olderBranches).font(Theme.ui(12)).foregroundStyle(Theme.text3)
                        .padding(.horizontal, 12)
                        .frame(height: Self.rowHeight)
                }
            } else {
                // The sections' hairline over the foot: the lines above it are a list that goes on, the foot stays.
                VStack(alignment: .leading, spacing: 0) {
                    let index = Self.rows(git: git, folder: folder, listing: false) - 1
                    item(
                        index: index, id: "start.branch.new", name: StartWords.newBranch.titleStyle,
                        checked: naming != nil, choose: { choose(.name) }
                    ) {
                        Text(StartWords.newBranch.titleStyle).font(Theme.ui(12.5)).foregroundStyle(Theme.text).fixedSize()
                    }
                    if let naming {
                        naming
                            .padding(.horizontal, 10)
                            .padding(.bottom, 6)
                            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { namingHeight = $0 }
                    }
                }
                .overlay(alignment: .top) { MenuDivider() }
            }
        }
        .frame(width: width - 2 * MenuMetrics.inset, alignment: .leading)
        .menuCard()
    }

    /// The menu's own rows, in screen 34's sections. Entries are numbered in reading order for the keys (`panelRow`).
    @ViewBuilder private var places: some View {
        let here = git.checkout(at: folder.path)
        let others = git.others(than: folder.path)
        let first = here == nil ? 0 : 1
        let fromBranch = git.branches.isEmpty ? 0 : 1
        if let here {
            header(StartWords.thisFolder, divided: false)
            row(.folder, index: 0, title: here.title, detail: StartWords.ownCheckout)
        }
        header(StartWords.newWorktree, divided: here != nil)
        item(
            index: first, id: "start.branch.task", name: StartWords.newWorktree,
            checked: place == nil && naming == nil, choose: { choose(.task) }
        ) {
            Text(StartWords.newWorktree).font(Theme.ui(12.5)).foregroundStyle(Theme.text).fixedSize()
            ModelDetail(text: StartWords.namedFromTask)
        }
        if fromBranch == 1 {
            item(
                index: first + 1, id: "start.branch.from", name: StartWords.fromABranch, checked: false,
                choose: { listing = true }
            ) {
                Text(StartWords.fromABranch).font(Theme.ui(12.5)).foregroundStyle(Theme.text).fixedSize()
                ModelDetail(text: StartWords.branchList(git.branches))
            }
        }
        if !others.isEmpty {
            header(StartWords.worktrees, divided: true)
            ForEach(Array(others.enumerated()), id: \.element.path) { offset, checkout in
                row(
                    .existing(checkout), index: first + 1 + fromBranch + offset, title: checkout.title,
                    detail: Self.shortPath(checkout.path), disabled: checkout.missing)
            }
        }
    }

    /// The branches a new worktree can start from, under a line naming the list, ".." back first.
    @ViewBuilder private var branches: some View {
        Text(verbatim: StartWords.fromABranch.trimmingCharacters(in: CharacterSet(charactersIn: "…")))
            .font(Theme.mono(11))
            .foregroundStyle(Theme.text2)
            .padding(.horizontal, 12)
            .frame(maxWidth: .infinity, minHeight: Self.rowHeight, maxHeight: Self.rowHeight, alignment: .leading)
            .accessibilityIdentifier("start.branch.list")
        VStack(alignment: .leading, spacing: 0) {
            item(index: 0, id: "start.branch.back", name: "..", checked: nil, choose: { listing = false }) {
                Text(verbatim: "..").font(Theme.mono(12)).foregroundStyle(Theme.text)
            }
            ForEach(Array(git.branches.enumerated()), id: \.element) { offset, branch in
                row(.branch(branch), index: 1 + offset, title: branch, detail: nil)
            }
        }
        // ← and ⌫ go back, as they go up in the folder browser; ↓ ↑ and Return stay the menu's.
        .focusable()
        .focusEffectDisabled()
        .focused($backKeys)
        .onAppear { backKeys = true }
        .onKeyPress(.leftArrow) {
            listing = false
            return .handled
        }
        // ⌫ sends DEL (U+007F) on a Mac, not SwiftUI's `.delete` (U+0008).
        .onKeyPress(KeyEquivalent("\u{7F}")) {
            listing = false
            return .handled
        }
    }

    /// A worktree's folder by its last two parts, as `worktrees/<name>`.
    static func shortPath(_ path: String) -> String {
        path.split(separator: "/").suffix(2).joined(separator: "/")
    }

    /// A section's heading, a hairline over it after the first.
    private func header(_ text: String, divided: Bool) -> some View {
        MenuHeader(text: text)
            .frame(maxWidth: .infinity, minHeight: Self.rowHeight, maxHeight: Self.rowHeight, alignment: .bottomLeading)
            .overlay(alignment: .top) {
                if divided { MenuDivider().padding(.top, 3) }
            }
    }

    /// A checkout or branch by its name, in a terminal's type, with what it is beside it.
    private func row(_ choice: StartPlace, index: Int, title: String, detail: String?, disabled: Bool = false)
        -> some View
    {
        item(
            index: index, id: "start.branch.\(title)", name: title, checked: naming == nil && place == choice,
            choose: { choose(.place(choice)) }
        ) {
            // Cut in the middle when the menu is narrower than the name, where a branch's prefix and its end both show.
            Text(verbatim: title).font(Theme.mono(12)).foregroundStyle(Theme.text).lineLimit(1)
                .truncationMode(.middle).layoutPriority(1)
            if let detail { ModelDetail(text: detail) }
        }
        .disabled(disabled)
        .opacity(disabled ? Theme.disabledOpacity : 1)
    }

    /// Entry `index`, by `name` to accessibility, its words as `content` draws them; `checked` nil has no check slot.
    private func item(
        index: Int, id: String, name: String, checked: Bool?, choose: @escaping () -> Void,
        @ViewBuilder content: () -> some View
    ) -> some View {
        Button(action: choose) {
            MenuRow(chosen: checked) { content() }
        }
        .buttonStyle(.panelRow)
        .panelRow(index, id: id, choose: choose)
        .accessibilityLabel(name)
        .accessibilityIdentifier(id)
        .accessibilityAddTraits(checked == true ? .isSelected : [])
    }
}
