import CodeConnectKit
import SwiftUI

/// The slash palette above a Claude run's reply box (Kit's `SlashPalette`): the commands this app answers itself, the
/// lit one lit as every panel's row is (`MenuRow`), the caption under a bare `/`. A click runs a row; the
/// pointer entering a row lights it (`light`), and the keys are the reply box's (`SlashPalette.outcome`).
struct SlashPanel: View {
    let content: SlashPalette.Content
    let lit: Int
    let light: (Int) -> Void
    let run: (SlashRow) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(content.rows.enumerated()), id: \.element.id) { index, row in
                Button { run(row) } label: {
                    MenuRow {
                        Image(systemName: Self.symbol(for: row.command)).font(.system(size: 11))
                            .foregroundStyle(Theme.text3)
                            .frame(width: 14)
                            .accessibilityHidden(true)
                        Text(verbatim: row.title).font(Theme.mono(12)).foregroundStyle(Theme.text).fixedSize()
                        ModelDetail(text: row.subtitle)
                    }
                    .environment(\.menuRowLit, index == lit)
                }
                .buttonStyle(.panelRow)
                .onHover { if $0 { light(index) } }
                .accessibilityLabel(row.title)
                .accessibilityIdentifier("session.slash.\(row.command)")
                .accessibilityAddTraits(index == lit ? .isSelected : [])
            }
            if let caption = content.caption {
                Text(caption).font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                    .padding(.horizontal, 12)
                    .padding(.top, 5)
                    .padding(.bottom, 2)
            }
        }
        .menuCard()
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
