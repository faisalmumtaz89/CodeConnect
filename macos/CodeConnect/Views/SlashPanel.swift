import CodeConnectKit
import SwiftUI

/// The slash palette above a Claude run's reply box (Kit's `SlashPalette`): the commands this app answers itself, the
/// lit one plated, the caption under a bare `/`. A click runs a row; the keys are the reply box's (`SlashPalette.outcome`).
struct SlashPanel: View {
    let content: SlashPalette.Content
    let lit: Int
    let run: (SlashRow) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(content.rows.enumerated()), id: \.element.id) { index, row in
                Button { run(row) } label: {
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Image(systemName: Self.symbol(for: row.command)).font(.system(size: 11))
                            .foregroundStyle(Theme.text3)
                            .frame(width: 14)
                            .accessibilityHidden(true)
                        Text(verbatim: row.title).font(Theme.mono(12)).foregroundStyle(Theme.text).fixedSize()
                        ModelDetail(text: row.subtitle)
                        Spacer(minLength: 0)
                    }
                    .padding(.horizontal, 8)
                    .frame(height: 24)
                    .background(index == lit ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 4))
                    .contentShape(Rectangle())
                }
                .buttonStyle(.hover(radius: 4, id: "session.slash.\(row.command)"))
                .padding(.horizontal, 4)
                .accessibilityLabel(row.title)
                .accessibilityIdentifier("session.slash.\(row.command)")
                .accessibilityAddTraits(index == lit ? .isSelected : [])
            }
            if content.showsCaption {
                Text(SlashPalette.caption).font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                    .padding(.horizontal, 12)
                    .padding(.top, 5)
                    .padding(.bottom, 2)
            }
        }
        .padding(.vertical, 5)
        .background(Theme.raised, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color.white.opacity(0.1)))
        .shadow(color: .black.opacity(0.5), radius: 18, y: 8)
    }

    /// Each native command's mark, the iPhone's icon in SF Symbols.
    private static func symbol(for command: String) -> String {
        switch command {
        case "model": "cpu"
        case "effort": "gauge.with.dots.needle.50percent"
        case "compact": PanelSymbol.compact
        case "clear": PanelSymbol.clear
        default: "plusminus"
        }
    }
}
