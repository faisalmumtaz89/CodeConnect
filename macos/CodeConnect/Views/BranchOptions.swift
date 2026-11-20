#if DEBUG
    import CodeConnectKit
    import SwiftUI

    /// `-CC_BRANCH_OPTION B|C|D|E`: the other places the start form's Branch choice could stand, drawn for the owner
    /// to compare with the Branch row (A). B: the places listed under the label in a box, as the folders are. C: the choice is the
    /// chosen folder row's second line. D: a chip beside the model in the message box. E: a three-way control with
    /// its list under it.
    enum BranchOption {
        static var current: String? { UserDefaults.standard.string(forKey: "CC_BRANCH_OPTION") }
    }

    /// The chosen place as a small button, for C's folder row and D's message box.
    struct BranchChip: View {
        let title: String
        @Binding var open: Bool

        var body: some View {
            Button {
                open.toggle()
            } label: {
                HStack(spacing: 5) {
                    Image(systemName: "arrow.triangle.branch").font(.system(size: 10, weight: .medium))
                    Text(verbatim: title).font(Theme.mono(11.5)).lineLimit(1)
                    Image(systemName: "chevron.down").font(.system(size: 8.5, weight: .semibold))
                }
                .foregroundStyle(Theme.text2)
                .padding(.horizontal, 7)
                .frame(height: 22)
                .background(open ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
                .contentShape(Rectangle())
            }
            .buttonStyle(.hover(radius: 5, id: "start.branch.chip"))
            .accessibilityLabel(StartWords.branch)
            .accessibilityIdentifier("start.branch.chip")
        }
    }

    /// B: every place in a box under the label, the chosen one filled, as the folder list is; no menu.
    struct BranchList: View {
        let git: StartGit
        let folder: StartProject
        let running: Set<String>
        @Binding var place: StartPlace
        @Binding var newName: String?

        var body: some View {
            VStack(alignment: .leading, spacing: 6) {
                Text(StartWords.branch).font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.text3)
                ScrollView {
                    VStack(spacing: 0) {
                        if let here = git.checkout(at: folder.path) {
                            row(.folder, here.title, here.notes(running: running))
                        }
                        ForEach(git.others(than: folder.path), id: \.path) { checkout in
                            row(.existing(checkout), checkout.title, checkout.notes(running: running))
                                .opacity(checkout.missing ? 0.45 : 1)
                        }
                        ForEach(git.branches, id: \.self) { row(.branch($0), $0, StartWords.newWorktree) }
                    }
                }
                .frame(height: 120)
                .background(Theme.code.opacity(0.5), in: RoundedRectangle(cornerRadius: Theme.boxRadius))
                .overlay(RoundedRectangle(cornerRadius: Theme.boxRadius).strokeBorder(Theme.hairline))
            }
        }

        private func row(_ choice: StartPlace, _ title: String, _ detail: String?) -> some View {
            HStack(spacing: 8) {
                Text(verbatim: title).font(Theme.mono(12)).foregroundStyle(Theme.text).lineLimit(1)
                Spacer(minLength: 8)
                if let detail { Text(detail).font(Theme.ui(11)).foregroundStyle(Theme.text3).lineLimit(1) }
            }
            .padding(.horizontal, 10)
            .frame(height: 26)
            .background(place == choice && newName == nil ? Theme.selected : .clear, in: Rectangle())
        }
    }

    /// E: This folder · Worktree · New branch, and under it what the choice holds.
    struct BranchSegments: View {
        let git: StartGit
        let folder: StartProject
        let running: Set<String>
        @Binding var place: StartPlace
        @Binding var newName: String?

        private enum Kind: Hashable { case here, worktree, new }

        private var kind: Kind { newName != nil ? .new : place == .folder ? .here : .worktree }

        var body: some View {
            VStack(alignment: .leading, spacing: 6) {
                Text(StartWords.branch).font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.text3)
                Picker(
                    StartWords.branch,
                    selection: Binding(
                        get: { kind },
                        set: {
                            switch $0 {
                            case .here:
                                place = .folder
                                newName = nil
                            case .worktree:
                                newName = nil
                                if place == .folder, let first = git.others(than: folder.path).first {
                                    place = .existing(first)
                                }
                            case .new: newName = ""
                            }
                        })
                ) {
                    Text(StartWords.thisFolder).tag(Kind.here)
                    Text(StartWords.worktrees).tag(Kind.worktree)
                    Text(StartWords.newBranch).tag(Kind.new)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .accessibilityIdentifier("start.branch.kind")
                .frame(width: 360)
                if kind == .worktree {
                    BranchMenu(git: git, folder: folder, running: running, place: place) { chosen in
                        if let chosen { place = chosen } else { newName = "" }
                    }
                }
            }
        }
    }
#endif
