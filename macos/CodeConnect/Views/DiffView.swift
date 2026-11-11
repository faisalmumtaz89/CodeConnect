import CodeConnectKit
import SwiftUI

/// One file's diff as Kit parses it (`UnifiedDiff.FileDiff`): its path and what happened to it, its lines added and
/// removed, then each hunk with added lines on green and removed ones on red. Unchanged stretches Kit folds are one
/// line saying how many. What it draws and speaks from the diff is `displaySafe`: a file's name, lines and hunk headers
/// come from the repository, and the characters that reorder a line would draw different code from what the bytes say.
struct FileDiffView: View {
    let file: UnifiedDiff.FileDiff
    /// The file as the wire named it, when that differs from the parsed header (a Codex card's change).
    var path: String?
    /// What the change is, when the wire said so (a Codex card's kind) rather than the parsed header.
    var status: UnifiedDiff.FileDiff.Status?
    /// The most diff lines to draw, counted as Kit's card budget counts them (every line of every hunk, folded or
    /// not, and never a hunk's header); the card counts the rest.
    var lineLimit: Int?

    /// One drawn row: a hunk's header, a line, a folded stretch, or a note.
    private enum Row: Identifiable {
        case header(String, Int), line(UnifiedDiff.Line, Int), folded(Int, Int), note(String, Int)

        var id: Int {
            switch self {
            case .header(_, let id), .line(_, let id), .folded(_, let id), .note(_, let id): id
            }
        }
    }

    private var rows: [Row] {
        var rows: [Row] = []
        var left = lineLimit ?? Int.max
        for hunk in file.hunks where left > 0 {
            rows.append(.header(hunk.header, rows.count))
            for segment in hunk.segments where left > 0 {
                if segment.isFolded {
                    rows.append(.folded(segment.lines.count, rows.count))
                    left -= segment.lines.count
                } else {
                    for line in segment.lines.prefix(left) { rows.append(.line(line, rows.count)) }
                    left -= min(left, segment.lines.count)
                }
            }
        }
        if left > 0 { for note in file.notes { rows.append(.note(note, rows.count)) } }
        return rows
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 6) {
                Image(systemName: "doc.text").font(.system(size: 10)).accessibilityHidden(true)
                Text((path ?? file.displayPath).displaySafe).font(Theme.mono(11.5)).lineLimit(1).truncationMode(.head)
                Text((status ?? file.status).label).font(Theme.ui(11)).foregroundStyle(Theme.text3)
                Spacer(minLength: 8)
                LineDelta(added: UInt64(file.additions), removed: UInt64(file.deletions))
            }
            .foregroundStyle(Theme.text2)
            .padding(.horizontal, 12)
            .frame(height: 30)
            Rectangle().fill(Theme.hairline).frame(height: 1)
            VStack(alignment: .leading, spacing: 0) {
                ForEach(rows) { row in
                    switch row {
                    case .header(let text, _): quiet(text, mono: true)
                    case .line(let line, _): DiffLine(line: line)
                    case .folded(let count, _): quiet("\(count) unchanged lines", mono: false)
                    case .note(let text, _): quiet(text, mono: false)
                    }
                }
            }
            .padding(.vertical, 6)
            .textSelection(.enabled)
        }
        .background(Theme.code, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.hairline))
    }

    private func quiet(_ text: String, mono: Bool) -> some View {
        Text(text.displaySafe).font(mono ? Theme.mono(11) : Theme.ui(11)).foregroundStyle(Theme.text3)
            .padding(.horizontal, 12)
            .padding(.vertical, 3)
    }
}

private struct DiffLine: View {
    let line: UnifiedDiff.Line

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(mark).foregroundStyle(color).frame(width: 8, alignment: .leading).accessibilityHidden(true)
            Text(shown.isEmpty ? " " : shown).foregroundStyle(color)
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(Theme.mono(12))
        .padding(.horizontal, 12)
        .padding(.vertical, 1.5)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(background)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(spoken)
    }

    private var shown: String { line.text.displaySafe }

    /// The line as a screen reader says it: whether it was added or removed, then its text.
    private var spoken: String {
        switch line.kind {
        case .addition: "Added: \(shown)"
        case .deletion: "Removed: \(shown)"
        case .context, .note: shown
        }
    }

    private var mark: String {
        switch line.kind {
        case .addition: "+"
        case .deletion: "−"
        case .context, .note: " "
        }
    }

    private var color: Color {
        switch line.kind {
        case .addition: Theme.green
        case .deletion: Theme.red
        case .context: Theme.text2
        case .note: Theme.text3
        }
    }

    private var background: Color {
        switch line.kind {
        case .addition: Theme.green.opacity(0.09)
        case .deletion: Theme.red.opacity(0.09)
        case .context, .note: .clear
        }
    }
}
