import CodeConnectKit
import SwiftUI

/// A run's usage as its settings line draws it, the Mac's figures (`ReplyBar`): the conversation's size against its
/// model's window, a hairline, and a ring with its share for each usage window of the account, in Menlo; or, where
/// those do not fit, the fullest window's ring alone. Each part only where the app holds it; it asks the computer
/// nothing and shows no countdown, so it needs no timer. VoiceOver reads the button it sits in, whose value is the
/// Kit's sentence for the same figures.
struct UsageFigures: View {
    let bar: ReplyBar
    /// The fullest window's ring alone.
    var fullestOnly = false

    var body: some View {
        HStack(spacing: CC.space.sm) {
            if fullestOnly {
                if let fullest = bar.limits.max(by: { $0.usedPercent < $1.usedPercent }) { share(fullest) }
            } else {
                if let context = bar.context {
                    let window = Text(verbatim: " / \(context.window)").foregroundStyle(CC.text.tertiary)
                    Text("\(Text(verbatim: context.used).foregroundStyle(CC.text.secondary))\(window)")
                }
                if bar.context != nil && !bar.figures.isEmpty {
                    Rectangle().fill(CC.color.border).frame(width: CC.stroke.hairline, height: 14)
                }
                ForEach(bar.figures, id: \.caption) { share($0) }
            }
        }
        .ccType(CC.type.monoSmall)
        .lineLimit(1)
        .accessibilityHidden(true)
    }

    private func share(_ limit: ReplyBar.Limit) -> some View {
        HStack(spacing: CC.space.xxs) {
            UsageRing(percent: limit.usedPercent)
            // As wide as "100%", so which form fits never changes with the value; the value starts at the ring.
            ZStack(alignment: .leading) {
                Text(verbatim: "100%").hidden()
                Text(verbatim: "\(limit.usedPercent)%").foregroundStyle(CC.text.secondary)
            }
        }
    }
}

/// A usage window's share as a small ring: a dark track, filled clockwise from the top, as the Mac draws it.
private struct UsageRing: View {
    let percent: Int

    var body: some View {
        ZStack {
            Circle().stroke(CC.color.border, lineWidth: 2)
            Circle().trim(from: 0, to: CGFloat(percent) / 100)
                .stroke(CC.text.secondary, style: StrokeStyle(lineWidth: 2, lineCap: .round))
                .rotationEffect(.degrees(-90))
        }
        .frame(width: 13, height: 13)
    }
}
