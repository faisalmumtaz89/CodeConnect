import CodeConnectKit
import SwiftUI

/// A run's changes as its computer read them (`git diff HEAD` and the untracked names), opened at the top. Kit asks
/// for them when the tab is shown and nothing has been read yet, and again only on "Check again": a diff is read when
/// you are about to read it, and says how old it is rather than passing for live. Who said what decides the state:
/// the computer's own words are quoted as they came, the app's account of its link is never passed off as them, and
/// an empty diff is called clean only when the computer added no note saying why it is empty.
struct ChangesTab: View {
    let model: AppModel
    let key: String

    var body: some View {
        let state = model.diffState(for: key)
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                if case .loaded(let raw, let parsed, let fetchedAt) = state {
                    if !parsed.isEmpty { Summary(diff: parsed) }
                    let read = raw.capturedDate ?? fetchedAt
                    AgeText(since: read) { "Checked \(Format.ago(since: read, now: $0))" }
                        .font(Theme.ui(12))
                        .foregroundStyle(Theme.text3)
                        .monospacedDigit()
                        .accessibilityIdentifier("changes.age")
                }
                Spacer(minLength: 0)
                ActionButton(title: "Check again", key: nil, prominent: false, id: "changes.refresh") {
                    model.loadDiff(key: key, force: true)
                }
                .disabled(state.isLoading)
            }
            .padding(.horizontal, 28)
            .frame(height: 44)
            .overlay(alignment: .bottom) { Rectangle().fill(Theme.hairline).frame(height: 1) }
            switch state {
            case .idle, .loading:
                Message(id: "changes.loading", title: "Reading this run's changes on its computer…")
            case .failed(.app(let reason)):
                Message(id: "changes.offline", title: "The changes could not be read", sentence: reason)
            case .failed(.daemon(let reason)):
                Message(
                    id: "changes.failed", title: "Your computer could not read the changes",
                    sentence: "Its reason, word for word:", quote: (reason, "changes.failed.reason"))
            case .loaded(let raw, let parsed, _) where parsed.isEmpty && raw.note == nil:
                Message(
                    id: "changes.clean", title: "No uncommitted changes",
                    sentence: "This run's folder was clean when it was read.")
            case .loaded(let raw, let parsed, _) where parsed.isEmpty:
                Message(
                    id: "changes.empty", title: "No diff to show",
                    sentence: "Your computer's reason, word for word:", quote: (raw.note ?? "", "changes.note"))
            case .loaded(let raw, let parsed, _):
                Document(raw: raw, diff: parsed)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(Theme.window)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("changes.pane")
        .task(id: key) {
            if case .idle = model.diffState(for: key) { model.loadDiff(key: key) }
        }
    }

    /// How many files the run's last read diff changes, for the tab: only from a diff already read, never asked for
    /// to fill the count, and none when it changes no file.
    static func fileCount(model: AppModel, key: String) -> Int? {
        guard case .loaded(_, let parsed, _) = model.diffState(for: key), !parsed.files.isEmpty else { return nil }
        return parsed.files.count
    }
}

/// Lines added and removed across the diff, and how many files: one reading, as drawn.
private struct Summary: View {
    let diff: UnifiedDiff

    var body: some View {
        let files = diff.files.count == 1 ? "1 file" : "\(diff.files.count) files"
        let added = Text(verbatim: "+\(diff.additions)").foregroundStyle(Theme.green)
        let removed = Text(verbatim: "−\(diff.deletions)").foregroundStyle(Theme.red)
        Text("\(added) \(removed)  ·  \(files)")
        .font(Theme.mono(11.5))
        .foregroundStyle(Theme.text2)
        .accessibilityIdentifier("changes.summary")
    }
}

/// A state with nothing to draw: what it is, the app's sentence about it, and the computer's own words when it gave
/// some, set as machine text and never paraphrased.
private struct Message: View {
    let id: String
    let title: String
    var sentence: String?
    var quote: (text: String, id: String)?

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
        }
        .frame(maxWidth: 460)
        .padding(28)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// The diff itself: the computer's note when it qualified a diff it did send, a banner when it cut the capture, each
/// file, and the lines that belong to no file (the untracked names). Lazy in its files.
private struct Document: View {
    let raw: SessionDiff
    let diff: UnifiedDiff

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 12) {
                if let note = raw.note {
                    Text(verbatim: note)
                        .font(Theme.mono(11.5))
                        .foregroundStyle(Theme.text2)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("changes.note")
                }
                if raw.truncated {
                    HStack(spacing: 6) {
                        Image(systemName: "scissors").accessibilityHidden(true)
                        Text("Cut short by your computer. This is not the whole diff.")
                    }
                    .font(Theme.ui(12))
                    .foregroundStyle(Theme.amber)
                    .accessibilityElement(children: .combine)
                    .accessibilityIdentifier("changes.truncated")
                }
                ForEach(diff.files) { file in
                    FileBlock(file: file)
                }
                let extra = (diff.preamble + diff.trailing).filter { !$0.allSatisfy(\.isWhitespace) }
                if !extra.isEmpty {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Also reported").font(Theme.ui(12, .medium)).foregroundStyle(Theme.text2)
                        Text(verbatim: extra.joined(separator: "\n"))
                            .font(Theme.mono(11.5))
                            .foregroundStyle(Theme.text3)
                            .textSelection(.enabled)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .accessibilityElement(children: .combine)
                    .accessibilityIdentifier("changes.extra")
                }
            }
            .padding(.horizontal, 28)
            .padding(.vertical, 18)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .defaultScrollAnchor(.top)
    }
}

/// One file, drawn up to a budget of lines and the rest a click away: a single file of thousands of lines would
/// otherwise be built whole before the tab could draw at all.
private struct FileBlock: View {
    let file: UnifiedDiff.FileDiff
    @State private var limit = Self.step

    /// Lines a file draws before it stops and says how many it holds back; each click draws this many more.
    static let step = 400

    var body: some View {
        let total = file.hunks.reduce(0) { $0 + $1.lines.count }
        VStack(alignment: .leading, spacing: 6) {
            FileDiffView(file: file, lineLimit: limit)
            if total > limit {
                Button("Show \(min(total - limit, Self.step)) more lines") { limit += Self.step }
                    .buttonStyle(.hover(outset: 4, id: "changes.file.\(file.id).more"))
                    .font(Theme.ui(12))
                    .foregroundStyle(Theme.text2)
                    .accessibilityIdentifier("changes.file.\(file.id).more")
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(
            Text(verbatim: "\(file.displayPath), \(file.status.label), +\(file.additions) −\(file.deletions)"))
        .accessibilityIdentifier("changes.file.\(file.id)")
    }
}
