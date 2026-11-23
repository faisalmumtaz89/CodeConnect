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
                    // Above the figures, its trailing edge on theirs, clear of them by the panels' one gap.
                    .overlay(alignment: .topTrailing) {
                        if hovered {
                            card(bar)
                                .fixedSize()
                                .alignmentGuide(.top) { $0[.bottom] + Theme.panelGap }
                        }
                    }
                    .accessibilityElement(children: .ignore)
                    .accessibilityLabel(Self.spoken(bar))
                    .accessibilityIdentifier("session.composer.usage")
            }
        }
    }

    private func figures(_ bar: ReplyBar) -> some View {
        HStack(spacing: 10) {
            if let context = bar.context {
                (Text(context.used).foregroundStyle(Theme.text2)
                    + Text(" / \(context.window)").foregroundStyle(Theme.text3))
                    .font(Theme.mono(11))
            }
            if bar.context != nil && !bar.limits.isEmpty {
                Rectangle().fill(Color.white.opacity(0.1)).frame(width: 1, height: 14)
            }
            ForEach(bar.limits, id: \.caption) { limit in
                HStack(spacing: 5) {
                    UsageRing(percent: limit.usedPercent)
                    Text("\(limit.usedPercent)%").font(Theme.mono(11)).foregroundStyle(Theme.text2)
                }
            }
        }
        .frame(height: 30)
    }

    /// Context, each window with its share and reset, then the account: the rows the Kit holds, in that order.
    private func card(_ bar: ReplyBar) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            if let context = bar.context { row("Context", "\(context.used) of \(context.window)") }
            ForEach(bar.limits, id: \.caption) { limit in
                row(limit.caption, "\(limit.usedPercent)%" + (limit.resets.map { " · \($0)" } ?? ""))
            }
            if let note = bar.limitsNote { row(note, "") }
            if let account = bar.account { row("Account", account) }
        }
        .menuCard()
    }

    private func row(_ caption: String, _ value: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 16) {
            Text(caption).font(Theme.ui(12.5)).foregroundStyle(Theme.text2)
            Spacer(minLength: 0)
            Text(value).font(Theme.ui(12.5)).monospacedDigit().foregroundStyle(Theme.text)
        }
        .padding(.horizontal, 10)
        .frame(minHeight: 28)
    }

    private static func spoken(_ bar: ReplyBar) -> String {
        var parts = bar.context.map { ["Context \($0.used) of \($0.window)"] } ?? []
        parts += bar.limits.map { "\($0.caption) \($0.usedPercent)%" + ($0.resets.map { ", \($0)" } ?? "") }
        return parts.joined(separator: "; ")
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
