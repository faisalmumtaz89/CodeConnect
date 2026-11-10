import CodeConnectKit
import SwiftUI

/// One file's diff as Kit parses it (`UnifiedDiff.FileDiff`): its path and what happened to it, its lines added and
/// removed, then each hunk with added lines on green and removed ones on red. Unchanged stretches Kit folds are one
/// line saying how many.
struct FileDiffView: View {
    let file: UnifiedDiff.FileDiff
    /// The file as the wire named it, when that differs from the parsed header (a Codex card's change).
    var path: String?
    /// The most rows to draw, where a card's budget limits them; the rest are counted by the card.
    var rowLimit: Int?

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
        for hunk in file.hunks {
            rows.append(.header(hunk.header, rows.count))
            for segment in hunk.segments {
                if segment.isFolded {
                    rows.append(.folded(segment.lines.count, rows.count))
                } else {
                    for line in segment.lines { rows.append(.line(line, rows.count)) }
                }
            }
        }
        for note in file.notes { rows.append(.note(note, rows.count)) }
        return rowLimit.map { Array(rows.prefix($0)) } ?? rows
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 6) {
                Image(systemName: "doc.text").font(.system(size: 10)).accessibilityHidden(true)
                Text(path ?? file.displayPath).font(Theme.mono(11.5)).lineLimit(1).truncationMode(.head)
                Text(file.status.label).font(Theme.ui(11)).foregroundStyle(Theme.text3)
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
        Text(text).font(mono ? Theme.mono(11) : Theme.ui(11)).foregroundStyle(Theme.text3)
            .padding(.horizontal, 12)
            .padding(.vertical, 3)
    }
}

private struct DiffLine: View {
    let line: UnifiedDiff.Line

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(mark).foregroundStyle(color).frame(width: 8, alignment: .leading).accessibilityHidden(true)
            Text(line.text.isEmpty ? " " : line.text).foregroundStyle(color)
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(Theme.mono(12))
        .padding(.horizontal, 12)
        .padding(.vertical, 1.5)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(background)
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
