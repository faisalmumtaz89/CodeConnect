import CodeConnectKit
import SwiftUI

/// Where in its repository a start runs, under the folder: the folder as it is, a worktree the repository has, a new
/// worktree for a branch no checkout has, or a new one by name. The row is there only while the computer can say what
/// the folder's repository holds and has not said it is outside one (`StartContext.branchAhead`), as the iPhone's
/// Branch step is; until it has said, the row says it is reading. What it offers and every word are Kit's.
struct BranchRow: View {
    let context: StartContext
    let folder: StartProject
    let agent: StartAgent?
    @Binding var place: StartPlace
    /// A new worktree's name while one is being named, else nil.
    @Binding var newName: String?
    @Binding var open: Bool
    /// The highest the menu may reach in the form's coordinates (`StartForm.space`).
    let top: CGFloat

    @Environment(\.startHeight) private var startHeight
    @Environment(\.startBox) private var startBox
    /// The row, label to last line, in the form's coordinates.
    @State private var frame: CGRect = .zero
    /// The button's bottom edge, in the row's coordinates.
    @State private var buttonBottom: CGFloat = 0

    var body: some View {
        Group {
            if let answer = context.git(folder.path), let git = answer {
                // A new worktree being named is named under the box (`BranchNaming`); here, its kind.
                let summary = startPlaceSummary(place, agent: agent ?? .claude, project: folder, git: git)
                button(newName == nil ? summary : StartWords.newWorktree)
            } else {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text(StartWords.readingBranches(folder.name)).font(Theme.ui(12)).foregroundStyle(Theme.text2)
                }
                .accessibilityIdentifier("start.branch.reading")
            }
        }
        .coordinateSpace(.named(Self.space))
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(StartForm.space)) } action: { frame = $0 }
        .overlay(alignment: .topLeading) {
            if open, let answer = context.git(folder.path), let git = answer { menu(git) }
        }
        // Around the menu as well as the button: the menu says where it is through what this sets, so a click on one
        // of its rows is a click inside, not one that closes it before the row is chosen.
        .closesPanel($open)
    }

    private nonisolated static let space = "start.branch.row"
    /// The least a menu keeps from the form's edges.
    private static let margin: CGFloat = 8

    /// The menu, as tall as its rows up to `BranchMenu.mostRows`, ending inside the pane. It opens above the row,
    /// clear of the label, when all of it fits between the label and `top`; else on the side of the button
    /// with more room, as many whole rows as fit there, the rest scrolled to. Cut short above, it hangs from `top`, so
    /// its edge never lands inside a line there.
    private func menu(_ git: StartGit) -> some View {
        let rows = BranchMenu.rows(git: git, folder: folder)
        let gap = Theme.panelGap
        let highest = max(top, Self.margin)
        let above = frame.minY - gap - highest
        let below = startHeight - frame.minY - buttonBottom - gap - Self.margin
        let opensAbove = BranchMenu.height(rows: rows) <= above || above > below
        let shown = BranchMenu.rows(fitting: opensAbove ? above : below, of: rows)
        let cut = shown < min(rows, BranchMenu.mostRows)
        let placed = StartPanel.place(widest: BranchMenu.width, leading: frame.minX, box: startBox)
        return BranchMenu(
            git: git, folder: folder, running: context.runningFolders, place: newName == nil ? place : nil,
            // Inside the pane by the start form's one rule (`StartPanel`), not as narrow as its row: beside the model
            // button the row is narrow, and its names would be cut that need not be.
            shownRows: shown, width: placed.width
        ) { chosen in
            if let chosen {
                place = chosen
                newName = nil
            } else {
                newName = ""
            }
            open = false
        }
        .openPanel()
        .offset(
            x: placed.x - frame.minX,
            y: !opensAbove ? buttonBottom + gap : cut ? highest - frame.minY : -(BranchMenu.height(rows: shown) + gap))
    }

    /// The chosen place, a button that opens the menu.
    private func button(_ title: String) -> some View {
        Button {
            open.toggle()
        } label: {
            HStack(spacing: 5) {
                Image(systemName: "arrow.triangle.branch").font(.system(size: 11)).accessibilityHidden(true)
                Text(verbatim: title).font(Theme.mono(12)).lineLimit(1).truncationMode(.middle)
                Image(systemName: "chevron.down").font(.system(size: 8.5, weight: .semibold))
            }
            .foregroundStyle(Theme.text)
            .inputBoxChip(open: open)
        }
        .buttonStyle(.hover(radius: 8, id: "start.branch"))
        .help(title)
        .accessibilityIdentifier("start.branch")
        .accessibilityValue(open ? "open" : "closed")
        .onGeometryChange(for: CGFloat.self) { $0.frame(in: .named(Self.space)).maxY } action: { buttonBottom = $0 }
    }
}

/// A new worktree being named, under the start form's box: the name, the rule or what breaks it, and what the name
/// makes. Escape stops naming it.
struct BranchNaming: View {
    let folder: StartProject
    let agent: StartAgent?
    let git: StartGit
    @Binding var newName: String?

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
            .padding(.horizontal, 10)
            .frame(height: 26)
            .background(Theme.code, in: RoundedRectangle(cornerRadius: 7))
            .overlay(
                RoundedRectangle(cornerRadius: 7).strokeBorder(trimmed.isEmpty || valid ? Theme.hairline : Theme.red))
            .onExitCommand { newName = nil }
            // It appears because "New branch…" was just chosen: what is typed next is its name.
            .takesKeyboardWhenShown()
            Text(trimmed.isEmpty || valid ? StartWords.worktreeNameRule : StartWords.worktreeNameBroken)
                .font(Theme.ui(11))
                .foregroundStyle(trimmed.isEmpty || valid ? Theme.text3 : Theme.red)
                .accessibilityIdentifier("start.branch.rule")
            if let agent {
                HStack(alignment: .firstTextBaseline, spacing: 5) {
                    fact(StartWords.branch) {
                        Text(verbatim: valid ? StartPlace.newBranch(trimmed, for: agent) : StartWords.notNamedYet)
                            .font(valid ? Theme.mono(11) : Theme.ui(11))
                    }
                    Text(verbatim: "·").foregroundStyle(Theme.text3)
                    fact(StartWords.from) { Text(StartPlace.newWorktreeBase(for: agent, from: here)) }
                }
                .lineLimit(1)
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

/// One fact about the new worktree: its label, then its value.
private func fact(_ label: String, @ViewBuilder value: () -> some View) -> some View {
    HStack(alignment: .firstTextBaseline, spacing: 5) {
        Text(label).foregroundStyle(Theme.text3)
        value().foregroundStyle(Theme.text2)
    }
    .font(Theme.ui(11))
}

/// The places a start can run in a repository, in the iPhone's sections and words: the folder's own checkout, the
/// repository's other worktrees (one whose folder is gone is shown, never chosen), a new worktree for each branch no
/// checkout has, then a new one by name. Drawn as the model menu is. `choose` gets nil for a new one by name.
///
/// Every line, a section's heading included, is one `rowHeight` tall, so a menu cut to `shownRows` ends on a whole
/// line and scrolls to the rest. Its foot, "New branch…" and the note on older branches above it, stays in sight under
/// the lines that scroll: naming a new worktree is never scrolled away.
struct BranchMenu: View {
    let git: StartGit
    let folder: StartProject
    let running: Set<String>
    /// The chosen place, checked; nil while a new one is being named.
    let place: StartPlace?
    /// How many lines show before it scrolls.
    let shownRows: Int
    let width: CGFloat
    let choose: (StartPlace?) -> Void

    static let width: CGFloat = 440
    static let rowHeight: CGFloat = 24
    static let mostRows = 12
    /// The card's padding (`menuCard`), above and below the rows.
    private static let inset: CGFloat = 6

    /// The menu's lines for this folder: each section's heading and entries.
    static func rows(git: StartGit, folder: StartProject) -> Int {
        let others = git.others(than: folder.path).count
        return (git.checkout(at: folder.path) == nil ? 0 : 2) + (others == 0 ? 0 : 1 + others) + 1 + git.branches.count
            + (git.branchesTruncated ? 1 : 0) + 1
    }

    /// The menu's height showing `rows` lines.
    static func height(rows: Int) -> CGFloat { CGFloat(rows) * rowHeight + 2 * inset }

    /// The most whole lines of `rows`, up to `mostRows`, that fit in `room`; at least the foot and one line over it.
    static func rows(fitting room: CGFloat, of rows: Int) -> Int {
        max(3, min(rows, mostRows, Int(((room - 2 * inset) / rowHeight).rounded(.down))))
    }

    /// The lines that never scroll.
    private var foot: Int { git.branchesTruncated ? 2 : 1 }

    var body: some View {
        // To the keys the entries are rows in reading order (`panelRow`), "New branch…" the last.
        let here = git.checkout(at: folder.path)
        let others = git.others(than: folder.path)
        let first = here == nil ? 0 : 1
        let branchesFrom = first + others.count
        return VStack(alignment: .leading, spacing: 0) {
            PanelScroll {
                VStack(alignment: .leading, spacing: 0) {
                    if let here {
                        header(StartWords.thisFolder, divided: false)
                        row(.folder, index: 0, title: here.title, detail: here.notes(running: running))
                    }
                    if !others.isEmpty {
                        header(StartWords.worktrees, divided: true)
                        ForEach(Array(others.enumerated()), id: \.element.path) { offset, checkout in
                            row(
                                .existing(checkout), index: first + offset, title: checkout.title,
                                detail: checkout.notes(running: running), disabled: checkout.missing)
                        }
                    }
                    header(StartWords.newWorktree, divided: here != nil || !others.isEmpty)
                    ForEach(Array(git.branches.enumerated()), id: \.element) { offset, branch in
                        row(.branch(branch), index: branchesFrom + offset, title: branch, detail: nil)
                    }
                }
            }
            .frame(height: CGFloat(shownRows - foot) * Self.rowHeight)
            // The sections' hairline over the foot: the lines above it are a list that goes on, the foot stays.
            VStack(alignment: .leading, spacing: 0) {
                if git.branchesTruncated {
                    Text(StartWords.olderBranches).font(Theme.ui(12)).foregroundStyle(Theme.text3)
                        .padding(.horizontal, 12)
                        .frame(height: Self.rowHeight)
                }
                item(
                    index: branchesFrom + git.branches.count, id: "start.branch.new", name: StartWords.newBranch,
                    checked: place == nil, choose: { choose(nil) }
                ) {
                    Text(StartWords.newBranch).font(Theme.ui(12.5)).foregroundStyle(Theme.text).fixedSize()
                    ModelDetail(text: StartWords.newBranchDetail)
                }
            }
            .overlay(alignment: .top) { Rectangle().fill(Theme.hairline).frame(height: 1) }
        }
        .frame(width: width - 12, alignment: .leading)
        .menuCard()
    }

    /// A section's heading, a hairline over it after the first.
    private func header(_ text: String, divided: Bool) -> some View {
        Text(text).font(Theme.ui(11, .semibold)).foregroundStyle(Theme.text3)
            .padding(.horizontal, 12)
            .padding(.bottom, 3)
            .frame(maxWidth: .infinity, minHeight: Self.rowHeight, maxHeight: Self.rowHeight, alignment: .bottomLeading)
            .overlay(alignment: .top) {
                if divided { Rectangle().fill(Theme.hairline).frame(height: 1).padding(.top, 3) }
            }
    }

    /// A checkout or branch by its name, in a terminal's type, with what a start there would share or meet beside it.
    private func row(_ choice: StartPlace, index: Int, title: String, detail: String?, disabled: Bool = false)
        -> some View
    {
        item(index: index, id: "start.branch.\(title)", name: title, checked: place == choice, choose: { choose(choice) }) {
            // Cut in the middle when the menu is narrower than the name, where a branch's prefix and its end both show.
            Text(verbatim: title).font(Theme.mono(12)).foregroundStyle(Theme.text).lineLimit(1)
                .truncationMode(.middle).layoutPriority(1)
            if let detail { ModelDetail(text: detail) }
        }
        .disabled(disabled)
        .opacity(disabled ? 0.45 : 1)
    }

    /// Entry `index`, by `name` to accessibility, its words as `content` draws them.
    private func item(
        index: Int, id: String, name: String, checked: Bool, choose: @escaping () -> Void,
        @ViewBuilder content: () -> some View
    ) -> some View {
        Button(action: choose) {
            HStack(spacing: 8) {
                Image(systemName: "checkmark")
                    .font(.system(size: 9.5, weight: .bold))
                    .opacity(checked ? 1 : 0)
                    .frame(width: 12)
                    .accessibilityHidden(true)
                content()
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 8)
            .frame(height: Self.rowHeight)
            .contentShape(Rectangle())
        }
        .buttonStyle(.panelRow)
        .panelRow(index, id: id, choose: choose)
        .accessibilityLabel(name)
        .padding(.horizontal, 4)
        .accessibilityIdentifier(id)
        .accessibilityAddTraits(checked ? .isSelected : [])
    }
}
