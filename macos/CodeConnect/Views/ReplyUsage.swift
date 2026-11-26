import CodeConnectKit
import SwiftUI

/// What a run's reply bar shows before Send (`ReplyBar`): the conversation's size against its model's window in
/// figures, a hairline, and a small ring per usage window of the account with its share; hovered, a card with the
/// details. Each part is drawn only where the Kit holds it. It asks the computer nothing: it draws what the app holds,
/// again each minute while shown, so a reset's countdown stays current.
struct ReplyUsage: View {
    let model: AppModel
    let key: String
    /// The pointer is on the figures, and the card shows.
    @Binding var hovered: Bool

    var body: some View {
        TimelineView(.everyMinute) { _ in
            if let bar = model.replyBar(for: key, now: .now), bar.context != nil || !bar.limits.isEmpty {
                figures(bar)
                    .contentShape(Rectangle())
                    .onHover { hovered = $0 }
                    // Above the figures, its trailing edge on theirs, clear of them by the panels' one gap: its foot on
                    // theirs, then raised by their height and the gap.
                    .overlay(alignment: .bottomTrailing) {
                        if hovered {
                            card(bar)
                                .fixedSize()
                                .offset(y: -(Self.figuresHeight + Theme.panelGap))
                        }
                    }
                    .accessibilityElement(children: .ignore)
                    .accessibilityLabel(bar.spoken)
                    .accessibilityIdentifier("session.composer.usage")
                    // As far from Send as the bar's chips stand from each other, label to label: 8 + 2 + 8 pt, of
                    // which the bar's own spacing gives 6.
                    .padding(.trailing, 12)
            }
        }
    }

    private func figures(_ bar: ReplyBar) -> some View {
        HStack(spacing: 10) {
            if let context = bar.context {
                let window = Text(" / \(context.window)").foregroundStyle(Theme.text3)
                Text("\(Text(context.used).foregroundStyle(Theme.text2))\(window)").font(Theme.mono(11))
            }
            if bar.context != nil && !bar.figures.isEmpty {
                Rectangle().fill(Color.white.opacity(0.1)).frame(width: 1, height: 14)
            }
            ForEach(bar.figures, id: \.caption) { limit in
                HStack(spacing: 5) {
                    UsageRing(percent: limit.usedPercent)
                    Text("\(limit.usedPercent)%").font(Theme.mono(11)).foregroundStyle(Theme.text2)
                }
            }
        }
        .frame(height: Self.figuresHeight)
    }

    private static let figuresHeight: CGFloat = 30

    /// Context, each window with its share and reset, then the account: the rows the Kit holds, in that order.
    private func card(_ bar: ReplyBar) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            if let context = bar.context { row("Context", value: context.used, rest: "of \(context.window)") }
            ForEach(bar.limits, id: \.caption) { limit in
                row(limit.caption, value: "\(limit.usedPercent)%", rest: limit.resets)
            }
            if let note = bar.limitsNote { row(note) }
            if let account = bar.account { row("Account", rest: account) }
        }
        // A tip, not a menu: 250 wide, its own fill, border, corners and room, under the box's shadows.
        .frame(width: 250 - 26)
        .padding(.vertical, 9)
        .padding(.horizontal, 13)
        .background {
            BoxShadow(radius: 12)
            RoundedRectangle(cornerRadius: 12).fill(Card.fill)
        }
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Card.border))
    }

    /// A card row, 12 pt throughout: its label muted at the leading side; at least 24 pt on, its value bright in Menlo,
    /// then the rest muted.
    private func row(_ label: String, value: String? = nil, rest: String? = nil) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Text(label).font(Theme.ui(12)).foregroundStyle(Card.muted)
            Spacer(minLength: 24)
            if let value { Text(value).font(Theme.mono(12)).foregroundStyle(Theme.text) }
            if let rest { Text(rest).font(Theme.ui(12)).foregroundStyle(Card.muted) }
        }
        .padding(.vertical, 4)
    }
}

/// A usage window's share as a small ring: a dark track, filled clockwise from the top.
private struct UsageRing: View {
    let percent: Int

    var body: some View {
        ZStack {
            Circle().stroke(Color(themeHex: 0x2C2C33), lineWidth: 2.4)
            Circle().trim(from: 0, to: CGFloat(percent) / 100)
                .stroke(Color(themeHex: 0xD4D4D8), style: StrokeStyle(lineWidth: 2.4, lineCap: .round))
                .rotationEffect(.degrees(-90))
        }
        .frame(width: 16, height: 16)
    }
}
