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
    private var key: String { row.id }
    @State private var hunks: HunkActions
    /// The diff last read, kept on screen while it is read again, so a reread loses no place, fold or comment.
    @State private var shown: (raw: SessionDiff, parsed: UnifiedDiff, fetchedAt: Date)?

    init(row: FleetRow, model: AppModel) {
        self.row = row
        self.model = model
        let key = row.id
        _hunks = State(initialValue: HunkActions(model: model, key: key))
        // A diff already on hand shows at once when the panel opens, while it is read again.
        if case .loaded(let raw, let parsed, let fetchedAt) = model.diffState(for: key) {
            _shown = State(initialValue: (raw, parsed, fetchedAt))
        }
    }

    var body: some View {
        let state = model.diffState(for: key)
        let reply = model.runGit(for: key)
        let git = reply?.git
        VStack(alignment: .leading, spacing: 0) {
            // The branch and the state as the run's repository was read (minor 56); before a read, and from an older
            // computer, the branch the run's summary names and the diff's own count. A read that found no repository
            // names no branch.
            if let git {
                BranchLine(branch: git.branch?.displaySafe, base: git.base?.name.displaySafe)
                StateLine(text: ChangesWords.state(git), delta: nil, read: ISO8601.parse(git.readAt))
            } else if reply == nil, let branch = row.summary.gitBranch?.displaySafe {
                BranchLine(branch: branch, base: nil)
            }
            // One branch draws a read diff, the one on hand or, while it is read again, the last one, so a reread keeps
            // the same views and their folds, scroll and unsent comment.
            if let diff = onHand(state) ?? (state.isLoading ? shown : nil) {
                loaded(diff.raw, diff.parsed, diff.fetchedAt, counted: reply == nil)
            } else {
                unread(state)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(Theme.window)
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
        // Asked again the moment the link is back, as the iPhone does: only after the app's own link failure, never
        // after a refusal the computer made.
        .onChange(of: model.daemonProfile.isConnected) { _, connected in
            if connected { reread() }
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
        switch state {
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
    private func loaded(_ raw: SessionDiff, _ parsed: UnifiedDiff, _ fetchedAt: Date, counted: Bool) -> some View {
        if parsed.isEmpty && raw.files?.isEmpty != false && raw.note == nil {
            Message(id: "changes.clean", title: ChangesWords.noUncommitted, sentence: ChangesWords.clean)
        } else if parsed.isEmpty && raw.files?.isEmpty != false {
            Message(
                id: "changes.empty", title: ChangesWords.noDiff,
                sentence: ChangesWords.computersReason, quote: (raw.note ?? "", "changes.note"))
        } else {
            let files = ChangedFile.rows(raw, parsed)
            if counted {
                StateLine(
                    text: "\(ChangesWords.files(files.count)) changed",
                    delta: (files.reduce(0) { $0 + ($1.added ?? 0) }, files.reduce(0) { $0 + ($1.removed ?? 0) }),
                    read: raw.capturedDate ?? fetchedAt)
            }
            Band(count: files.count, cut: raw.truncated || raw.filesTruncated)
            Document(raw: raw, diff: parsed, files: files).environment(hunks)
        }
    }

    /// How many files the run's last read diff changes, for the tab: only from a diff already read, never asked for
    /// to fill the count, and none when it changes no file.
    static func fileCount(model: AppModel, key: String) -> Int? {
        guard case .loaded(let raw, let parsed, _) = model.diffState(for: key) else { return nil }
        let count = ChangedFile.rows(raw, parsed).count
        return count == 0 ? nil : count
    }
}

/// The branch the run's folder has checked out, and the branch it is measured against when the computer knows one.
private struct BranchLine: View {
    /// Nil on a detached head.
    let branch: String?
    let base: String?

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "arrow.triangle.branch").font(.system(size: 11)).foregroundStyle(Theme.text3)
                .accessibilityHidden(true)
            Text(verbatim: branch ?? "Detached HEAD").font(Theme.mono(12)).foregroundStyle(Theme.text)
                .lineLimit(1).truncationMode(.middle)
            if let base, base != branch {
                Image(systemName: "arrow.right").font(.system(size: 9)).foregroundStyle(Theme.text3)
                    .accessibilityLabel("onto")
                Text(verbatim: base).font(Theme.mono(12)).foregroundStyle(Theme.text3).lineLimit(1)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 20)
        .padding(.top, 14)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("changes.branch")
    }
}

/// How the run's changes stand, then when that was read.
private struct StateLine: View {
    let text: String
    /// Lines added and removed, where the line counts them itself.
    let delta: (added: UInt64, removed: UInt64)?
    let read: Date?

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(verbatim: text).font(Theme.ui(13, .medium)).foregroundStyle(Theme.text)
                if let delta { LineDelta(added: delta.added, removed: delta.removed) }
            }
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("changes.summary")
            AgeText(since: read) { ChangesWords.checked(since: read, now: $0) }
                .font(Theme.ui(12))
                .foregroundStyle(Theme.text3)
                .monospacedDigit()
                .accessibilityIdentifier("changes.age")
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
    }
}

/// The panel's one tab before the run has a pull request: its changes, and how many files.
private struct Band: View {
    let count: Int
    /// The computer cut the diff short.
    let cut: Bool

    var body: some View {
        HStack(spacing: 8) {
            Text("Changes").font(Theme.ui(13, .medium)).foregroundStyle(Theme.text)
            Text(ChangesWords.files(count)).font(Theme.ui(12)).foregroundStyle(Theme.text3)
            if cut {
                Text("Cut short by your computer").font(Theme.ui(12)).foregroundStyle(Theme.amber)
                    .accessibilityIdentifier("changes.truncated")
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 20)
        .frame(height: 38)
        .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("changes.band")
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
                (Text(verbatim: file.folder.displaySafe).foregroundStyle(Theme.text3)
                    + Text(verbatim: file.name.displaySafe).foregroundStyle(Theme.text))
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
