import CodeConnectKit
import SwiftUI

/// A list in an agent's reply: one line per item, its words hanging clear of the marker, each level of nesting
/// indented one step.
struct AgentListView: View {
    let list: AgentList

    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            ForEach(Array(list.items.enumerated()), id: \.offset) { _, item in
                HStack(alignment: .firstTextBaseline, spacing: CC.space.xs) {
                    Group {
                        if let glyph = item.glyph {
                            Text(glyph).ccType(CC.type.reading)
                        } else if case .task(let done) = item.marker {
                            Image(systemName: done ? "checkmark.square" : "square")
                                .accessibilityLabel(done ? "Done" : "Not done")
                        }
                    }
                    .foregroundStyle(CC.text.tertiary)
                    .frame(minWidth: 14, alignment: .leading)
                    Text(AgentProse.inline(item.text))
                        .ccType(CC.type.reading)
                        .foregroundStyle(CC.text.primary)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .padding(.leading, CGFloat(item.depth) * 18)
            }
        }
        .textSelection(.enabled)
    }
}
