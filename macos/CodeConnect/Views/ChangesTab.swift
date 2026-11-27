import CodeConnectKit
import SwiftUI

/// A run's Changes panel: the branch it is on, how its changes stand, the files it changed and each file's diff, as
/// its computer read them (`git diff HEAD` and the untracked files). Kit asks for them when the panel opens, again when
/// the run's line count changes, and again when the link comes back after it failed them: a diff is read when you are
/// about to read it, and says how old it is rather than passing for live. Who said what decides the state: the
/// computer's own words are quoted as they came, the app's account of its link is never passed off as them, and an
/// empty diff is called clean only when the computer added no note saying why it is empty.
struct ChangesPanel: View {
    let row: FleetRow
    let model: AppModel
    /// Gives the conversation back, while the panel stands in its place: its tabs row then always shows.
    let back: (() -> Void)?
    private var key: String { row.id }
    @State private var hunks: HunkActions
    /// The diff last read, kept on screen while it is read again, so a reread loses no place, fold or comment.
    @State private var shown: (raw: SessionDiff, parsed: UnifiedDiff, fetchedAt: Date)?
    /// Changes or Checks, while the run's open pull request has checks to show.
    @State private var tab = PanelTab.changes

    init(row: FleetRow, model: AppModel, back: (() -> Void)?) {
        self.row = row
        self.model = model
        self.back = back
        let key = row.id
        _hunks = State(initialValue: HunkActions(model: model, key: key))
        // A diff already on hand shows at once when the panel opens, while it is read again.
        if case .loaded(let raw, let parsed, let fetchedAt) = model.diffState(for: key) {
            _shown = State(initialValue: (raw, parsed, fetchedAt))
        }
    }

    /// The run's computer is offline: the panel keeps what it last read, says so and how old it is, and asks nothing
    /// until the computer is back.
    private var offline: Bool { model.linkHealth.level == .offline }
    private var computer: String { model.machineName ?? "your computer" }

    var body: some View {
        let state = model.diffState(for: key)
        let reply = model.runGit(for: key)
        let git = reply?.git
        VStack(alignment: .leading, spacing: 0) {
            // The branch and the state as the run's repository was read (minor 56); before a read, and from an older
            // computer, the branch the run's summary names and the diff's own count. A read that found no repository
            // names no branch.
            if let git {
                // A pull request just opened stands for the one the repository has not read yet.
                let opened: (number: UInt64, url: String)? =
                    if case .opened(let number, let url) = model.gitActionState(for: key) { (number, url) } else { nil }
                BranchLine(
                    branch: git.branch?.displaySafe, base: git.base?.name.displaySafe,
                    pullRequest: (git.pullRequest?.url ?? opened?.url).flatMap { URL(string: $0) },
                    host: git.ghHost ?? "github.com")
                // The panel's one action: a commit while there are changes; once they are committed, a pull request
                // into the repository the computer names, while the branch has none. The line under the state says
                // whether it can be done now; Commit's row follows it, and Create PR stands at the state line's end.
                let committable = !offline && model.commitsChanges && git.changed.files > 0
                let target = git.ghRepo.map { ChangesWords.pullRequestTarget(repo: $0, base: git.base?.name) }
                let proposable =
                    !offline && !committable && model.opensPullRequests && git.pullRequest == nil && opened == nil
                    && (git.ahead ?? 0) > 0 && git.branch != nil && target != nil
                // The run's commit compared with its base (artboard 17): its conflicts lead the line and name their
                // files under it.
                let compared = model.conflicts(for: key)
                let line = StateLine(
                    text: git.pullRequest == nil && opened != nil
                        ? "PR #\(opened?.number ?? 0) · Open" : ChangesWords.state(git, conflicts: compared),
                    delta: nil, read: ISO8601.parse(git.readAt), unreachable: offline ? computer : nil,
                    note: committable
                        ? commitWait(git) ?? ChangesWords.commitReady
                        : proposable
                            ? pullRequestWait(git) ?? target
                            : conflicted.flatMap(ChangesWords.conflictsNote)
                                ?? ChangesWords.checksNote(git, conflicts: compared))
                VStack(alignment: .leading, spacing: 0) {
                    if proposable {
                        PullRequestRow(row: row, model: model, ready: pullRequestWait(git) == nil, line: line)
                    } else if !offline, model.mergesPullRequests, let pr = git.pullRequest, pr.state == .open {
                        MergeRow(
                            row: row, model: model, pr: pr, conflicts: compared, quiet: commitWait(git) == nil,
                            line: line)
                    } else if !offline, model.archivesRuns, !row.summary.archived,
                        ChangesWords.archivable(git, inWorktree: row.summary.inWorktree),
                        let branch = git.branch, let head = git.headOID
                    {
                        ArchiveRow(
                            row: row, model: model, branch: branch, headOID: head, quiet: commitWait(git) == nil,
                            line: line)
                    } else {
                        line
                        if committable { CommitRow(row: row, model: model, ready: commitWait(git) == nil) }
                    }
                }
                .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
            } else if reply == nil, let branch = row.summary.gitBranch?.displaySafe {
                BranchLine(branch: branch, base: nil, pullRequest: nil, host: "")
            }
            // An open pull request's checks take a tab beside its changes (artboards 00, 07); the tabs stand where the
            // changes' band stood.
            let checks = git.flatMap(Self.checks)
            let diff = onHand(state) ?? (state.isLoading ? shown : nil)
            if let checks {
                PanelTabs(
                    tab: $tab, checks: checks, cut: diff.map { $0.raw.truncated || $0.raw.filesTruncated } ?? false,
                    back: back)
            }
            if let checks, tab == .checks {
                ChecksList(checks: checks, complete: git?.pullRequest?.checksComplete != false)
            } else if let diff {
                // One branch draws a read diff, the one on hand or, while it is read again, the last one, so a reread
                // keeps the same views and their folds, scroll and unsent comment.
                loaded(diff.raw, diff.parsed, diff.fetchedAt, counted: git == nil, banded: checks == nil)
            } else {
                unread(state)
            }
            // The run's shell, under both tabs, at the panel's foot; it sets its own height.
            TerminalFoot(row: row, model: model)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(Theme.window, ignoresSafeAreaEdges: [])
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("changes.pane")
        // Read when it opens, again when a turn ends (an agent's commit or push changes no line count), and when the
        // run's branch or line count changes, so the panel follows the run.
        .task(id: key) { reread() }
        .onChange(of: row.status == .running) { _, running in if !running { reread() } }
        .onChange(of: row.summary.gitBranch) { _, _ in reread() }
        .onChange(of: row.summary.diffStat) { _, _ in reread() }
        .onChange(of: loadedAt) { _, _ in
            if let diff = onHand(model.diffState(for: key)) { shown = diff }
        }
        // Read again the moment the link is back, as the iPhone does, but never over a refusal the computer made: its
        // words stay until the person asks again.
        .onChange(of: model.daemonProfile.isConnected) { _, connected in
            guard connected else { return }
            if case .failed(.daemon) = model.diffState(for: key) {
                model.loadRunGit(key: key)
            } else {
                reread()
            }
        }
    }

    /// Why the run's changes cannot be committed yet: its agent works, a card waits for the person, a merge is
    /// unresolved or no branch is checked out; nil when they can.
    private func commitWait(_ git: RunGit) -> String? {
        switch row.status {
        case .running, .stopping: return ChangesWords.commitWaitsWorking
        case .blocked: return ChangesWords.commitWaitsCard
        default: break
        }
        if git.unmerged { return ChangesWords.commitWaitsMerge }
        if git.branch == nil { return ChangesWords.commitWaitsBranch }
        return nil
    }

    /// The run's commit's conflicts with its base, when the computer compared the commit checked out and found some.
    private var conflicted: RunConflicts? {
        guard let compared = model.conflicts(for: key), !compared.clean,
            compared.headOID == model.runGit(for: key)?.git?.headOID
        else { return nil }
        return compared
    }

    /// The run's open pull request's checks, when the computer read some; nil for none, or a pull request not open.
    private static func checks(_ git: RunGit) -> [RunGit.PullRequest.Check]? {
        guard let pr = git.pullRequest, pr.state == .open, let checks = pr.checks, !checks.isEmpty else { return nil }
        return checks
    }

    /// Why Create PR cannot be pressed yet: what holds a commit back, then what the computer would refuse it for (the
    /// branch is the base itself; gh is missing or signed out); nil when it can.
    private func pullRequestWait(_ git: RunGit) -> String? {
        if let wait = commitWait(git) { return wait }
        if git.base?.name == git.branch { return ChangesWords.pullRequestWaitsBase }
        switch git.gh {
        case .signedOut: return ChangesWords.pullRequestWaitsSignIn
        case .missing: return ChangesWords.pullRequestWaitsGh
        default: return nil
        }
    }

    /// The diff a read gave, when the state holds one.
    private func onHand(_ state: DiffState) -> (raw: SessionDiff, parsed: UnifiedDiff, fetchedAt: Date)? {
        if case .loaded(let raw, let parsed, let fetchedAt) = state { return (raw, parsed, fetchedAt) }
        return nil
    }

    /// A state with no read diff to draw: reading, the app's link failing, or the computer's refusal in its words.
    @ViewBuilder
    private func unread(_ state: DiffState) -> some View {
        backBand
        switch state {
        case .failed(.app(let reason)) where offline:
            Message(
                id: "changes.offline", title: ChangesWords.cantReach(computer),
                sentence: model.linkHealth.disabledReason ?? reason)
        case .failed(.app(let reason)):
            Message(id: "changes.offline", title: "The changes could not be read", sentence: reason) {
                model.loadDiff(key: key, force: true)
            }
        case .failed(.daemon(let reason)):
            Message(
                id: "changes.failed", title: ChangesWords.couldNotRead,
                sentence: ChangesWords.itsReason, quote: (reason, "changes.failed.reason"))
        case .idle, .loading, .loaded:
            Message(id: "changes.loading", title: ChangesWords.reading)
        }
    }

    /// The tabs row with no count, where there is no diff to count, only while it gives the conversation back and the
    /// Checks tabs (which give it back themselves) are not shown.
    @ViewBuilder private var backBand: some View {
        if let back, model.runGit(for: key)?.git.flatMap(Self.checks) == nil {
            Band(count: nil, cut: false, back: back)
        }
    }

    /// When the diff on hand was read, while one is.
    private var loadedAt: Date? {
        if case .loaded(_, _, let fetchedAt) = model.diffState(for: key) { return fetchedAt }
        return nil
    }

    /// Asks for the diff again, unless a read is already on its way (the computer takes one at a time).
    private func reread() {
        if !model.diffState(for: key).isLoading { model.loadDiff(key: key, force: true) }
        model.loadRunGit(key: key)
    }

    /// A diff the computer read: clean when it changed nothing and said nothing, its note when it said why it is
    /// empty, else the band and the document, under the diff's own count where the repository gave none (`counted`).
    @ViewBuilder
    private func loaded(
        _ raw: SessionDiff, _ parsed: UnifiedDiff, _ fetchedAt: Date, counted: Bool, banded: Bool
    ) -> some View {
        if parsed.isEmpty && raw.files?.isEmpty != false && raw.note == nil, let conflicted {
            // Nothing uncommitted, and the commit conflicts with its base: its conflicts stand where the diff would.
            backBand
            ConflictsList(key: key, model: model, compared: conflicted)
        } else if parsed.isEmpty && raw.files?.isEmpty != false && raw.note == nil {
            backBand
            Message(id: "changes.clean", title: ChangesWords.noUncommitted, sentence: ChangesWords.clean)
        } else if parsed.isEmpty && raw.files?.isEmpty != false {
            backBand
            Message(
                id: "changes.empty", title: ChangesWords.noDiff,
                sentence: ChangesWords.computersReason, quote: (raw.note ?? "", "changes.note"))
        } else {
            let files = ChangedFile.rows(raw, parsed)
            if counted {
                StateLine(
                    text: "\(ChangesWords.files(files.count)) changed",
                    delta: (files.reduce(0) { $0 + ($1.added ?? 0) }, files.reduce(0) { $0 + ($1.removed ?? 0) }),
                    read: raw.capturedDate ?? fetchedAt, unreachable: offline ? computer : nil, note: nil)
                .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
            }
            if banded { Band(count: files.count, cut: raw.truncated || raw.filesTruncated, back: back) }
            Document(raw: raw, diff: parsed, files: files).environment(hunks)
        }
    }
}

/// The branch the run's folder has checked out, and the branch it is measured against when the computer knows one.
private struct BranchLine: View {
    /// Nil on a detached head.
    let branch: String?
    let base: String?
    /// The branch's pull request on its host, once it has one.
    let pullRequest: URL?
    let host: String

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "arrow.triangle.branch").font(.system(size: 11)).foregroundStyle(Theme.text3)
                .accessibilityHidden(true)
            // The branch and its base read as one; the pull request's link stands on its own.
            HStack(spacing: 8) {
                Text(verbatim: branch ?? "Detached HEAD").font(Theme.mono(12)).foregroundStyle(Theme.text)
                    .lineLimit(1).truncationMode(.middle)
                if let base, base != branch {
                    Image(systemName: "arrow.right").font(.system(size: 9)).foregroundStyle(Theme.text3)
                        .accessibilityLabel("onto")
                    Text(verbatim: base).font(Theme.mono(12)).foregroundStyle(Theme.text3).lineLimit(1)
                }
            }
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("changes.branch")
            Spacer(minLength: 0)
            if let pullRequest {
                Link(destination: pullRequest) { Text(verbatim: "\(host) ↗") }
                    .font(Theme.ui(12))
                    .foregroundStyle(Theme.text3)
                    .accessibilityIdentifier("changes.pullRequest.link")
            }
        }
        .padding(.horizontal, 20)
        .padding(.top, 14)
    }
}

/// How the run's changes stand, then when that was read, or what the person can do about them.
private struct StateLine: View {
    let text: String
    /// Lines added and removed, where the line counts them itself.
    let delta: (added: UInt64, removed: UInt64)?
    let read: Date?
    /// The computer the panel cannot reach, while it is offline: said with how old the read is, in place of the note.
    let unreachable: String?
    /// Said in place of when it was read.
    let note: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(verbatim: text).font(Theme.ui(13, .medium)).foregroundStyle(Theme.text)
                if let delta { LineDelta(added: delta.added, removed: delta.removed) }
            }
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("changes.summary")
            if let unreachable {
                VStack(alignment: .leading, spacing: 2) {
                    Text(ChangesWords.cantReach(unreachable)).font(Theme.ui(12, .medium)).foregroundStyle(Theme.text2)
                        .lineLimit(1).truncationMode(.tail)
                    AgeText(since: read) { ChangesWords.lastSaid(since: read, now: $0) }
                        .font(Theme.ui(12))
                        .foregroundStyle(Theme.text3)
                        .monospacedDigit()
                }
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("changes.lastSaid")
            } else if let note {
                Text(note).font(Theme.ui(11.5)).foregroundStyle(Theme.text3).lineLimit(1).truncationMode(.tail)
                    .accessibilityIdentifier("changes.note.action")
            } else {
                AgeText(since: read) { ChangesWords.checked(since: read, now: $0) }
                    .font(Theme.ui(12))
                    .foregroundStyle(Theme.text3)
                    .monospacedDigit()
                    .accessibilityIdentifier("changes.age")
            }
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// The commit row: the message, the run's title to begin with, and Commit, which Return in the field presses too. Only
/// while the run's agent is quiet; what the computer said when it did not commit stays under it.
private struct CommitRow: View {
    let row: FleetRow
    let model: AppModel
    /// The agent is quiet, so the changes can be committed.
    let ready: Bool
    @State private var message: String

    init(row: FleetRow, model: AppModel, ready: Bool) {
        self.row = row
        self.model = model
        self.ready = ready
        _message = State(initialValue: row.title)
    }

    var body: some View {
        let state = model.gitActionState(for: row.id)
        let words = message.trimmingCharacters(in: .whitespacesAndNewlines)
        let enabled = ready && state != .working && !words.isEmpty
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                TextField("Commit message", text: $message)
                    .textFieldStyle(.plain)
                    .font(Theme.ui(13))
                    .foregroundStyle(Theme.text)
                    .padding(.horizontal, 10)
                    .frame(height: 30)
                    .background(Theme.code, in: RoundedRectangle(cornerRadius: 6))
                    .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Theme.hairline))
                    .disabled(!ready || state == .working)
                    .onSubmit { if enabled { model.commit(key: row.id, message: words) } }
                    .accessibilityLabel("Commit message")
                    .accessibilityIdentifier("changes.commit.message")
                ActionButton(title: "Commit", key: "↩", prominent: true, id: "changes.commit") {
                    model.commit(key: row.id, message: words)
                }
                .disabled(!enabled)
            }
            if case .refused(let why, .commit) = state {
                Text(why)
                    .font(Theme.ui(12))
                    .foregroundStyle(Theme.amber)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("changes.commit.refused")
            }
        }
        .padding(.horizontal, 20)
        .padding(.bottom, 12)
    }
}

/// The pull request row: the state line, and at its end Create PR (⇧⌘P), which pushes the branch and opens its pull
/// request where that line says. Only while the run's agent is quiet; what the computer said when it did not open one
/// stays under it.
private struct PullRequestRow: View {
    let row: FleetRow
    let model: AppModel
    /// The agent is quiet, so the pull request can be opened.
    let ready: Bool
    /// The state line, with Create PR at its row's end.
    let line: StateLine
    /// A confirmation or a sheet covers the window: ⇧⌘P must not act under it.
    @FocusedValue(\.windowCovered) private var covered

    var body: some View {
        let state = model.gitActionState(for: row.id)
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                line
                ActionButton(title: "Create PR", key: "⇧⌘P", prominent: true, id: "changes.createPR") {
                    model.createPullRequest(key: row.id)
                }
                .keyboardShortcut("p", modifiers: [.command, .shift])
                .disabled(!ready || state == .working || covered == true)
                .padding(.trailing, 20)
            }
            if case .refused(let why, .pullRequest) = state {
                Text(why)
                    .font(Theme.ui(12))
                    .foregroundStyle(Theme.amber)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 20)
                    .padding(.bottom, 12)
                    .accessibilityIdentifier("changes.createPR.refused")
            }
        }
    }
}

/// A commit's conflicts with its base (artboard 17), each file's under its name: the lines on this branch, then the
/// base's, each marked at its edge. A file's hunks are read when its block comes into view.
private struct ConflictsList: View {
    let key: String
    let model: AppModel
    let compared: RunConflicts

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 12) {
                if compared.files.isEmpty {
                    Text(ChangesWords.conflictsUnnamed).font(Theme.ui(12)).foregroundStyle(Theme.text3)
                }
                ForEach(compared.files, id: \.self) { path in
                    ConflictBlock(key: key, model: model, path: path, base: compared.base.name.displaySafe)
                }
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .accessibilityIdentifier("changes.conflicts")
    }
}

private struct ConflictBlock: View {
    let key: String
    let model: AppModel
    let path: String
    let base: String

    var body: some View {
        let reply = model.conflict(for: key, path: path)
        VStack(alignment: .leading, spacing: 8) {
            Text(ChangesWords.conflictTitle(path)).font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                .padding(.horizontal, 2)
            VStack(alignment: .leading, spacing: 0) {
                if let hunks = reply?.hunks {
                    if hunks.isEmpty { quiet(ChangesWords.conflictNoLines) }
                    ForEach(Array(hunks.enumerated()), id: \.offset) { _, hunk in
                        side(ChangesWords.conflictThisBranch, hunk.ours)
                        side(base, hunk.theirs)
                    }
                    if reply?.hunksComplete == false { quiet(ChangesWords.conflictMore) }
                } else if let note = reply?.note {
                    quiet(note.displaySafe)
                } else {
                    quiet(ChangesWords.reading)
                }
            }
            .padding(.vertical, 8)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.code, in: RoundedRectangle(cornerRadius: 7))
            .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
        }
        .task(id: path) { model.loadConflict(key: key, path: path) }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("changes.conflict.\(path)")
    }

    /// One side's lines under its name, each marked at its leading edge.
    private func side(_ name: String, _ text: String) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(verbatim: name).font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                .padding(.leading, 13)
                .padding(.top, 3)
                .padding(.bottom, 1)
            // A side's text ends its last line with a newline, which is not a line of its own.
            let lines = (text.hasSuffix("\n") ? String(text.dropLast()) : text)
                .split(separator: "\n", omittingEmptySubsequences: false)
            ForEach(Array(lines.enumerated()), id: \.offset) {
                _, line in
                HStack(spacing: 0) {
                    Rectangle().fill(Theme.amber).frame(width: 3)
                    Text(verbatim: String(line).displaySafe).font(Theme.mono(11.5)).foregroundStyle(Theme.text)
                        .lineLimit(1)
                        .padding(.leading, 10)
                }
                .frame(height: 19)
            }
        }
    }

    private func quiet(_ text: String) -> some View {
        Text(text).font(Theme.ui(12)).foregroundStyle(Theme.text3).padding(.horizontal, 13).padding(.vertical, 4)
    }
}

/// A finished run's row (artboard 11): the state line, and at its end Archive Run, which asks first on the one card
/// (`FleetWords.archiveAsk`), then ends the run, removes its worktree and deletes its branch, only while the branch is
/// at the commit the panel shows. What the computer said when it did not archive stays under it.
private struct ArchiveRow: View {
    let row: FleetRow
    let model: AppModel
    let branch: String
    let headOID: String
    /// The run's agent is quiet.
    let quiet: Bool
    let line: StateLine
    /// The run whose archiving is being asked about.
    @State private var asking: FleetRow?
    /// A confirmation or a sheet covers the window.
    @FocusedValue(\.windowCovered) private var covered

    var body: some View {
        let state = model.gitActionState(for: row.id)
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                line
                ActionButton(title: "Archive Run", key: nil, prominent: false, id: "changes.archive") { asking = row }
                    .disabled(!quiet || state == .working || covered == true)
                    .padding(.trailing, 20)
            }
            if case .refused(let why, .archive) = state {
                Text(why)
                    .font(Theme.ui(12))
                    .foregroundStyle(Theme.amber)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 20)
                    .padding(.bottom, 12)
                    .accessibilityIdentifier("changes.archive.refused")
            }
        }
        .confirmation($asking) { asked in
            Confirmation(
                FleetWords.archiveAsk(asked.title), cancelID: "changes.archive.cancel",
                confirmID: "changes.archive.confirm"
            ) { model.archiveRun(key: asked.id, branch: branch, headOID: headOID) }
        }
    }
}

/// An open pull request's row: the state line, and at its end the merge button (artboard 10), which names the method it
/// merges by, ⇧⌘M, with the repository's methods a click away on its chevron. It merges only what GitHub would merge as
/// it stands, while the run's agent is quiet, and only the pull request and commit the panel shows; what the computer
/// said when it did not merge stays under it.
private struct MergeRow: View {
    let row: FleetRow
    let model: AppModel
    let pr: RunGit.PullRequest
    /// The run's agent is quiet.
    /// The run's commit compared with its base: a conflict withholds the merge.
    let conflicts: RunConflicts?
    let quiet: Bool
    let line: StateLine
    /// The method chosen from the menu; until then the repository's default (`ChangesWords.defaultMergeMethod`).
    @State private var chosen: MergeMethod?
    @State private var menuOpen = false
    /// A confirmation or a sheet covers the window: ⇧⌘M must not act under it.
    @FocusedValue(\.windowCovered) private var covered

    var body: some View {
        let state = model.gitActionState(for: row.id)
        let allowed = pr.mergeMethods ?? []
        let method = chosen.flatMap { allowed.contains($0) ? $0 : nil } ?? ChangesWords.defaultMergeMethod(allowed)
        let ready =
            quiet && ChangesWords.mergeable(pr, conflicts: conflicts) && method != nil && state != .working
            && covered != true
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                line
                HStack(spacing: 1) {
                    Button {
                        if let method {
                            model.mergePullRequest(key: row.id, number: pr.number, method: method, headOID: pr.headOID)
                        }
                    } label: {
                        HStack(spacing: 7) {
                            Text(method.map(ChangesWords.mergeMethod) ?? ChangesWords.mergeMethod(.squash))
                                .font(Theme.ui(12.5, .medium))
                            KeyCap("⇧⌘M", onLight: true)
                        }
                        .foregroundStyle(Color.black)
                        .padding(.horizontal, 10)
                        .frame(height: 26)
                        .background(Theme.text, in: UnevenRoundedRectangle(topLeadingRadius: 6, bottomLeadingRadius: 6))
                    }
                    // Only the pair's own 0.5 dims it, so both halves read as one button.
                    .buttonStyle(.hover(over: true, fades: false, id: "changes.merge"))
                    .keyboardShortcut("m", modifiers: [.command, .shift])
                    .disabled(!ready)
                    .accessibilityIdentifier("changes.merge")
                    Button { menuOpen.toggle() } label: {
                        Image(systemName: "chevron.down").font(.system(size: 8, weight: .semibold))
                            .foregroundStyle(Color.black)
                            .padding(.horizontal, 7)
                            .frame(height: 26)
                            .background(
                                Theme.text, in: UnevenRoundedRectangle(bottomTrailingRadius: 6, topTrailingRadius: 6))
                    }
                    .buttonStyle(.hover(over: true, id: "changes.merge.methods"))
                    .disabled(allowed.isEmpty || state == .working)
                    .accessibilityLabel(ChangesWords.mergeMethods)
                    .accessibilityIdentifier("changes.merge.methods")
                }
                .opacity(ready ? 1 : 0.5)
                .overlay(alignment: .topTrailing) {
                    if menuOpen {
                        menu(chosen: method)
                            .openPanel()
                            .offset(y: 26 + Theme.panelGap)
                    }
                }
                .closesPanel($menuOpen)
                .padding(.trailing, 20)
            }
            if case .refused(let why, .merge) = state {
                Text(why)
                    .font(Theme.ui(12))
                    .foregroundStyle(Theme.amber)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 20)
                    .padding(.bottom, 12)
                    .accessibilityIdentifier("changes.merge.refused")
            }
        }
        .zIndex(1)
    }

    /// The methods the repository allows, the one the button uses checked, then a rule and the ones it turns off,
    /// dimmed, each saying so.
    private func menu(chosen method: MergeMethod?) -> some View {
        let allowed = MergeMethod.ordered.filter { (pr.mergeMethods ?? []).contains($0) }
        let off = MergeMethod.ordered.filter { !allowed.contains($0) }
        return VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(allowed.enumerated()), id: \.element) { index, option in
                let choose = {
                    chosen = option
                    menuOpen = false
                }
                Button(action: choose) {
                    MenuRow(chosen: option == method) {
                        Text(ChangesWords.mergeMethod(option)).font(Theme.ui(12.5)).foregroundStyle(Theme.text)
                            .lineLimit(1)
                    }
                }
                .buttonStyle(.panelRow)
                .panelRow(index, id: "changes.merge.method.\(option.rawValue)", choose: choose)
                .accessibilityIdentifier("changes.merge.method.\(option.rawValue)")
                .accessibilityAddTraits(option == method ? .isSelected : [])
            }
            if !off.isEmpty && !allowed.isEmpty { MenuDivider().padding(.vertical, 5) }
            ForEach(off, id: \.self) { option in
                MenuRow(chosen: false) {
                    Text(ChangesWords.mergeMethodOff(option)).font(Theme.ui(12.5)).foregroundStyle(Theme.text)
                        .lineLimit(1)
                }
                .opacity(0.45)
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("changes.merge.method.\(option.rawValue)")
            }
        }
        .frame(width: 270 - 2 * MenuMetrics.inset)
        .fixedSize(horizontal: false, vertical: true)
        .menuCard()
        // Every click inside the card is its own: one on a dimmed row or a gap does nothing below it.
        .contentShape(RoundedRectangle(cornerRadius: MenuMetrics.cardRadius))
        .onTapGesture {}
    }
}

extension MergeMethod {
    /// The menu's order (artboard 10): squash, a merge commit, then rebase.
    static let ordered: [MergeMethod] = [.squash, .merge, .rebase]
}

/// The panel's two tabs once its open pull request has checks: Changes, and Checks with how many passed. Where the
/// band said the diff was cut short, so does this row while Changes is chosen.
enum PanelTab: Hashable { case changes, checks }

private struct PanelTabs: View {
    @Binding var tab: PanelTab
    let checks: [RunGit.PullRequest.Check]
    let cut: Bool
    /// Gives the conversation back, while the panel stands in its place (Esc does the same).
    let back: (() -> Void)?

    var body: some View {
        HStack(spacing: 8) {
            SegmentedTrack(
                options: [PanelTab.changes, .checks], chosen: tab, fill: Theme.window,
                id: { $0 == .changes ? "changes.tab.changes" : "changes.tab.checks" }, choose: { tab = $0 }
            ) { option, chosen in
                Text(option == .changes ? "Changes" : ChangesWords.checksTab(checks))
                    .font(Theme.ui(12, chosen ? .medium : .regular))
                    .foregroundStyle(chosen ? Theme.text : Theme.text3)
                    .padding(.horizontal, 10)
                    .frame(height: 24)
            }
            .fixedSize()
            if cut && tab == .changes {
                Text("Cut short by your computer").font(Theme.ui(12)).foregroundStyle(Theme.amber)
                    .accessibilityIdentifier("changes.truncated")
            }
            Spacer(minLength: 0)
            if let back {
                ActionButton(
                    title: "‹ Conversation", key: "Esc", prominent: false, small: true, id: "session.changes.back",
                    action: back)
            }
        }
        .padding(.horizontal, 12)
        .frame(height: 38)
        .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
    }
}

/// An open pull request's checks, in the computer's order (a failure first), as artboard 07 lists them: each one's
/// mark, name and duration, and a failure open in place with its first failure line and its log.
private struct ChecksList: View {
    let checks: [RunGit.PullRequest.Check]
    /// False when there were more than the computer reads, or its head moved while they were read.
    let complete: Bool

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(Array(checks.enumerated()), id: \.offset) { _, check in
                    CheckRow(check: check)
                }
                if !complete {
                    Text(ChangesWords.checksNotAll).font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                        .padding(.horizontal, 14)
                        .padding(.vertical, 8)
                        .accessibilityIdentifier("changes.checks.notAll")
                }
            }
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .accessibilityIdentifier("changes.checks")
    }
}

/// One check: its state's mark, its name, its duration ("running" while it runs), and, failed, its first failure line
/// and a link to its log, open in place.
private struct CheckRow: View {
    let check: RunGit.PullRequest.Check

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 9) {
                Image(systemName: mark.symbol).font(.system(size: 12)).foregroundStyle(mark.tint)
                    .accessibilityLabel(mark.spoken)
                Text(verbatim: check.name.displaySafe).font(Theme.ui(12)).foregroundStyle(Theme.text)
                    .lineLimit(1).truncationMode(.tail)
                Spacer(minLength: 8)
                let duration: String? =
                    check.state == .pending ? ChangesWords.checkRunning : ChangesWords.checkDuration(check, now: .now)
                if let duration {
                    Text(verbatim: duration).font(Theme.mono(10.5)).foregroundStyle(Theme.text3)
                }
            }
            .padding(.horizontal, 14)
            .frame(height: 30)
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("changes.check.\(check.name)")
            if check.state == .fail, check.failureLine != nil || check.url != nil {
                VStack(alignment: .leading, spacing: 8) {
                    if let line = check.failureLine {
                        Text(verbatim: line.displaySafe).font(Theme.mono(11.5)).foregroundStyle(Theme.text2)
                            .lineSpacing(4)
                            .textSelection(.enabled)
                            .fixedSize(horizontal: false, vertical: true)
                            .accessibilityIdentifier("changes.check.failure")
                    }
                    if let url = check.url.flatMap(URL.init(string:)) {
                        Link(destination: url) { Text(verbatim: "Log ↗") }
                            .font(Theme.ui(11.5))
                            .foregroundStyle(Theme.text3)
                            .accessibilityIdentifier("changes.check.log")
                    }
                }
                .padding(.horizontal, 10)
                .padding(.vertical, 9)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Theme.red.opacity(0.06), in: RoundedRectangle(cornerRadius: 7))
                .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.red.opacity(0.18)))
                .padding(.leading, 35)
                .padding(.trailing, 14)
                .padding(.top, 2)
                .padding(.bottom, 10)
            }
        }
    }

    /// The mark for the check's state, its color, and what a screen reader says for it.
    private var mark: (symbol: String, tint: Color, spoken: String) {
        switch check.state {
        case .pass: ("checkmark.circle", Theme.green, "Passed")
        case .fail: ("xmark.circle", Theme.red, "Failed")
        case .pending: ("clock", Theme.amber, "Running")
        case .cancelled: ("minus.circle", Theme.text3, "Cancelled")
        case .skipped: ("arrow.right.circle", Theme.text3, "Skipped")
        case .unknown: ("questionmark.circle", Theme.text3, "Unknown")
        }
    }
}

/// The panel's tabs row before the run has a pull request: its changes, how many files when there are some, and while
/// the panel stands in the conversation's place, at its trailing end, the way back to it (Esc does the same).
private struct Band: View {
    let count: Int?
    /// The computer cut the diff short.
    let cut: Bool
    let back: (() -> Void)?

    var body: some View {
        HStack(spacing: 8) {
            HStack(spacing: 8) {
                Text("Changes").font(Theme.ui(13, .medium)).foregroundStyle(Theme.text)
                if let count { Text(ChangesWords.files(count)).font(Theme.ui(12)).foregroundStyle(Theme.text3) }
                if cut {
                    Text("Cut short by your computer").font(Theme.ui(12)).foregroundStyle(Theme.amber)
                        .accessibilityIdentifier("changes.truncated")
                }
            }
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("changes.band")
            Spacer(minLength: 0)
            if let back {
                ActionButton(title: "‹ Conversation", key: "Esc", prominent: false, small: true,
                    id: "session.changes.back", action: back)
            }
        }
        .padding(.leading, 20)
        .padding(.trailing, back == nil ? 20 : 12)
        .frame(height: 38)
        .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
    }
}

/// A state with nothing to draw: what it is, the app's sentence about it, and the computer's own words when it gave
/// some, set as machine text and never paraphrased.
private struct Message: View {
    let id: String
    let title: String
    var sentence: String?
    var quote: (text: String, id: String)?
    /// Asks again, where only the app's link failed.
    var retry: (() -> Void)?

    var body: some View {
        VStack(spacing: 10) {
            VStack(spacing: 6) {
                Text(title).font(Theme.ui(13, .medium)).foregroundStyle(Theme.text)
                if let sentence { Text(sentence).font(Theme.ui(12.5)).foregroundStyle(Theme.text2) }
            }
            .multilineTextAlignment(.center)
            .fixedSize(horizontal: false, vertical: true)
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier(id)
            if let quote {
                Text(verbatim: quote.text)
                    .font(Theme.mono(11.5))
                    .foregroundStyle(Theme.text2)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(10)
                    .background(Theme.code, in: RoundedRectangle(cornerRadius: 6))
                    .accessibilityIdentifier(quote.id)
            }
            if let retry {
                ActionButton(title: ChangesWords.checkAgain, key: nil, prominent: false, id: "changes.refresh", action: retry)
            }
        }
        .frame(maxWidth: 460)
        .padding(28)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// The files the run changed, each a click from its diff, then each file's diff folded to its header or open: the
/// computer's note when it qualified a diff it did send, each file the capture holds, and from an older computer the
/// lines that belong to no file (the untracked names). Lazy in its files.
private struct Document: View {
    let raw: SessionDiff
    let diff: UnifiedDiff
    let files: [ChangedFile]
    /// The files whose diffs are folded to their headers.
    @State private var folded: Set<String> = []

    var body: some View {
        ScrollViewReader { reader in
            ScrollView {
                // One column: the list, its rule, then the diffs. A scroll view's views are not stacked by it.
                VStack(alignment: .leading, spacing: 0) {
                    VStack(alignment: .leading, spacing: 2) {
                        ForEach(files) { file in
                            FileRow(file: file) { block in
                                folded.remove(block)
                                withAnimation(.easeOut(duration: 0.2)) { reader.scrollTo("block:\(block)", anchor: .top) }
                            }
                        }
                    }
                    .padding(.horizontal, 8)
                    .padding(.vertical, 8)
                    Rectangle().fill(Theme.hairline).frame(height: 1)
                    LazyVStack(alignment: .leading, spacing: 12) {
                        if let note = raw.note {
                            Text(verbatim: note)
                                .font(Theme.mono(11.5))
                                .foregroundStyle(Theme.text2)
                                .textSelection(.enabled)
                                .fixedSize(horizontal: false, vertical: true)
                                .accessibilityIdentifier("changes.note")
                        }
                        ForEach(ChangedFile.blocks(diff).compactMap(\.diff)) { fileDiff in
                            FileBlock(
                                file: fileDiff,
                                expanded: Binding(
                                    get: { !folded.contains(fileDiff.id) },
                                    set: { open in if open { folded.remove(fileDiff.id) } else { folded.insert(fileDiff.id) } })
                            )
                            .id("block:\(fileDiff.id)")
                        }
                        let extra = raw.files == nil ? (diff.preamble + diff.trailing).filter { !$0.allSatisfy(\.isWhitespace) } : []
                        if !extra.isEmpty {
                            VStack(alignment: .leading, spacing: 4) {
                                Text("Also reported").font(Theme.ui(12, .medium)).foregroundStyle(Theme.text2)
                                // Not selectable: a selectable text is dropped from the combined element, so the names would
                                // be drawn but never read.
                                // Each as a file header names its file: the path, then its kind in a word.
                                ForEach(Array(UnifiedDiff.reported(extra).enumerated()), id: \.offset) { _, reported in
                                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                                        Text(verbatim: reported.text.displaySafe).font(Theme.mono(11.5)).foregroundStyle(Theme.text2)
                                            .fixedSize(horizontal: false, vertical: true)
                                        if let label = reported.label {
                                            Text(label).font(Theme.ui(11)).foregroundStyle(Theme.text3)
                                        }
                                    }
                                }
                            }
                            .accessibilityElement(children: .combine)
                            .accessibilityIdentifier("changes.extra")
                        }
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 12)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            .defaultScrollAnchor(.top)
        }
    }
}

/// One changed file in the panel's list: its folder quiet, its name plain, and its lines when they were counted. A
/// click opens its diff and brings it up, where the capture holds one; a file with none (an untracked one) is a plain
/// row.
private struct FileRow: View {
    let file: ChangedFile
    /// Opens the diff block with this id and brings it up.
    let open: (String) -> Void

    var body: some View {
        if let block = file.diff?.id {
            Button { open(block) } label: { row }
                .buttonStyle(.hover(id: "changes.row.\(file.id)"))
                .accessibilityIdentifier("changes.row.\(file.id)")
        } else {
            row.accessibilityElement(children: .combine).accessibilityIdentifier("changes.row.\(file.id)")
        }
    }

    private var row: some View {
            HStack(spacing: 8) {
                Image(systemName: "doc").font(.system(size: 10)).foregroundStyle(Theme.text3).accessibilityHidden(true)
                Text(
                    "\(Text(verbatim: file.folder.displaySafe).foregroundStyle(Theme.text3))\(Text(verbatim: file.name.displaySafe).foregroundStyle(Theme.text))"
                )
                .font(Theme.mono(11.5))
                .lineLimit(1)
                .truncationMode(.head)
                Spacer(minLength: 8)
                if let added = file.added, let removed = file.removed {
                    LineDelta(added: added, removed: removed)
                }
            }
            .padding(.horizontal, 12)
            .frame(height: 30)
            .contentShape(Rectangle())
    }
}

/// One file, drawn up to a budget of lines and the rest a click away: a single file of thousands of lines would
/// otherwise be built whole before the tab could draw at all.
private struct FileBlock: View {
    let file: UnifiedDiff.FileDiff
    let expanded: Binding<Bool>
    @State private var limit = Self.step

    /// Lines a file draws before it stops and says how many it holds back; each click draws this many more.
    static let step = 400

    var body: some View {
        let total = file.hunks.reduce(0) { $0 + $1.lines.count }
        VStack(alignment: .leading, spacing: 6) {
            FileDiffView(file: file, lineLimit: limit, expanded: expanded)
            if expanded.wrappedValue, total > limit {
                Button(ChangesWords.showMoreLines(min(total - limit, Self.step))) { limit += Self.step }
                    .buttonStyle(.hover(bare: true, flush: .leading, id: "changes.file.\(file.id).more"))
                    .font(Theme.ui(12))
                    .foregroundStyle(Theme.text2)
                    .accessibilityIdentifier("changes.file.\(file.id).more")
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(
            Text(verbatim: "\(file.displayPath.displaySafe), \(file.status.label), "
                + "+\(file.additions) −\(file.deletions)"))
        .accessibilityIdentifier("changes.file.\(file.id)")
    }
}
