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

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(StartWords.branch).font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.text3)
            if let answer = context.git(folder.path), let git = answer {
                if let newName {
                    naming(newName, git: git)
                } else {
                    button(startPlaceSummary(place, agent: agent ?? .claude, project: folder, git: git), git: git)
                }
            } else {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text(StartWords.readingBranches(folder.name)).font(Theme.ui(12)).foregroundStyle(Theme.text2)
                }
                .accessibilityIdentifier("start.branch.reading")
            }
        }
    }

    /// The chosen place, a button whose menu opens above it, as the model's does.
    private func button(_ title: String, git: StartGit) -> some View {
        Button {
            open.toggle()
        } label: {
            HStack(spacing: 5) {
                Text(verbatim: title).font(Theme.mono(12)).lineLimit(1)
                Image(systemName: "chevron.down").font(.system(size: 8.5, weight: .semibold))
            }
            .foregroundStyle(Theme.text)
            .padding(.horizontal, 7)
            .frame(height: 24)
            .background(open ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: "start.branch"))
        .padding(.leading, -7)
        .accessibilityIdentifier("start.branch")
        .accessibilityValue(open ? "open" : "closed")
        // Above the button, over the folders, clear of it by the panels' one gap: the row stands low in the sheet, as the
        // model's button does, and there is room above it.
        .overlay(alignment: .bottomLeading) {
            if open {
                BranchMenu(
                    git: git, folder: folder, running: context.runningFolders, place: newName == nil ? place : nil
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
                .offset(x: -7, y: -(24 + Theme.panelGap))
            }
        }
        .closesPanel($open)
    }

    /// A new worktree being named: the menu's button, the name, the rule or what breaks it, and what the name makes.
    private func naming(_ name: String, git: StartGit) -> some View {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        let valid = StartPlace.isValidName(trimmed)
        let here = git.checkout(at: folder.path)
        return VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 8) {
                button(StartWords.newWorktree, git: git)
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
                .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(trimmed.isEmpty || valid ? Theme.hairline : Theme.red))
                .onExitCommand { newName = nil }
            }
            Text(trimmed.isEmpty || valid ? StartWords.worktreeNameRule : StartWords.worktreeNameBroken)
                .font(Theme.ui(11))
                .foregroundStyle(trimmed.isEmpty || valid ? Theme.text3 : Theme.red)
                .accessibilityIdentifier("start.branch.rule")
            if let agent {
                HStack(alignment: .firstTextBaseline, spacing: 5) {
                    Text(StartWords.branch).foregroundStyle(Theme.text3)
                    Text(verbatim: valid ? StartPlace.newBranch(trimmed, for: agent) : StartWords.notNamedYet)
                        .font(valid ? Theme.mono(11) : Theme.ui(11))
                        .foregroundStyle(Theme.text2)
                    Text(verbatim: "·").foregroundStyle(Theme.text3)
                    Text(StartWords.from).foregroundStyle(Theme.text3)
                    Text(StartPlace.newWorktreeBase(for: agent, from: here)).foregroundStyle(Theme.text2)
                }
                .font(Theme.ui(11))
                .lineLimit(1)
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("start.branch.facts")
                if here?.dirty == true {
                    Text("\(StartWords.changes): \(StartWords.changesStay)").font(Theme.ui(11)).foregroundStyle(Theme.text3)
                }
            }
        }
    }
}

/// The places a start can run in a repository, in the iPhone's sections and words: the folder's own checkout, the
/// repository's other worktrees (one whose folder is gone is shown, never chosen), a new worktree for each branch no
/// checkout has, then a new one by name. Drawn as the model menu is. `choose` gets nil for a new one by name.
struct BranchMenu: View {
    let git: StartGit
    let folder: StartProject
    let running: Set<String>
    /// The chosen place, checked; nil while a new one is being named.
    let place: StartPlace?
    let choose: (StartPlace?) -> Void

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                if let here = git.checkout(at: folder.path) {
                    header(StartWords.thisFolder)
                    row(.folder, title: here.title, detail: here.notes(running: running))
                }
                let others = git.others(than: folder.path)
                if !others.isEmpty {
                    divider
                    header(StartWords.worktrees)
                    ForEach(others, id: \.path) { checkout in
                        row(
                            .existing(checkout), title: checkout.title, detail: checkout.notes(running: running),
                            disabled: checkout.missing)
                    }
                }
                divider
                header(StartWords.newWorktree)
                ForEach(git.branches, id: \.self) { branch in row(.branch(branch), title: branch, detail: nil) }
                if git.branchesTruncated {
                    Text(StartWords.olderBranches).font(Theme.ui(12)).foregroundStyle(Theme.text3)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 4)
                }
                item(id: "start.branch.new", name: StartWords.newBranch, checked: place == nil, choose: { choose(nil) }) {
                    Text(StartWords.newBranch).font(Theme.ui(12.5)).foregroundStyle(Theme.text).fixedSize()
                    ModelDetail(text: StartWords.newBranchDetail)
                }
            }
            .padding(.vertical, 5)
        }
        .frame(width: 440)
        .frame(maxHeight: 300)
        .fixedSize(horizontal: false, vertical: true)
        .background(Theme.raised, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color.white.opacity(0.1)))
        .shadow(color: .black.opacity(0.5), radius: 18, y: 8)
    }

    private func header(_ text: String) -> some View {
        Text(text).font(Theme.ui(11, .semibold)).foregroundStyle(Theme.text3)
            .padding(.horizontal, 12)
            .padding(.top, 5)
            .padding(.bottom, 3)
    }

    private var divider: some View {
        Rectangle().fill(Theme.hairline).frame(height: 1).padding(.vertical, 5)
    }

    /// A checkout or branch by its name, in a terminal's type, with what a start there would share or meet beside it.
    private func row(_ choice: StartPlace, title: String, detail: String?, disabled: Bool = false) -> some View {
        item(id: "start.branch.\(title)", name: title, checked: place == choice, choose: { choose(choice) }) {
            Text(verbatim: title).font(Theme.mono(12)).foregroundStyle(Theme.text).lineLimit(1).fixedSize()
            if let detail { ModelDetail(text: detail) }
        }
        .disabled(disabled)
        .opacity(disabled ? 0.45 : 1)
    }

    /// One entry, by `name` to accessibility, its words as `content` draws them.
    private func item(
        id: String, name: String, checked: Bool, choose: @escaping () -> Void, @ViewBuilder content: () -> some View
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
            .frame(height: 24)
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 4, id: id))
        .accessibilityLabel(name)
        .padding(.horizontal, 4)
        .accessibilityIdentifier(id)
        .accessibilityAddTraits(checked ? .isSelected : [])
    }
}
