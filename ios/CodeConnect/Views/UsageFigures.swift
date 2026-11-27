import CodeConnectKit
import SwiftUI

/// A run's usage as the reply box's bar draws it, the Mac's figures (`ReplyBar`): the conversation's size against its
/// model's window, a hairline, and one ring with its share, for the window the Kit names (`ReplyBar.ring`: the window
/// at its limit, else the shortest), 8 pt apart, in Menlo 12. Narrower forms drop the size and its hairline, then the
/// share. Each part only where the app holds it; it asks the computer nothing and shows no countdown, so it needs no
/// timer. VoiceOver reads the button it sits in, whose value is the Kit's sentence for the same figures.
struct UsageFigures: View {
    enum Form {
        /// The size, its hairline, the ring and its share.
        case full
        /// The ring and its share.
        case ringAndShare
        /// The ring alone.
        case ring
    }

    let bar: ReplyBar
    var form: Form = .full
    /// The session is at a usage limit: its ring is in the alert's red.
    var atLimit = false

    var body: some View {
        HStack(spacing: 8) {
            if form == .full, let context = bar.context {
                Text(verbatim: "\(context.used) / \(context.window)")
                if bar.ring != nil {
                    Rectangle().fill(Color.white.opacity(0.14)).frame(width: 1, height: 12)
                }
            }
            if let ring = bar.ring {
                UsageRing(percent: ring.usedPercent, red: atLimit)
                if form != .ring {
                    // As wide as "100%", so which form fits never changes with the value; the value starts at the ring.
                    ZStack(alignment: .leading) {
                        Text(verbatim: "100%").hidden()
                        Text(verbatim: "\(ring.usedPercent)%")
                    }
                }
            }
        }
        .ccType(CC.type.monoSmall)
        .foregroundStyle(CC.text.secondary)
        .lineLimit(1)
        .accessibilityHidden(true)
    }
}

/// A usage window's share as a small ring: a dark track, filled clockwise from the top, as the Mac draws it.
struct UsageRing: View {
    let percent: Int
    /// At a usage limit: in the alert's red.
    var red = false

    var body: some View {
        ZStack {
            Circle().stroke(Color.white.opacity(0.1), lineWidth: 2.4)
            Circle().trim(from: 0, to: CGFloat(min(percent, 100)) / 100)
                .stroke(red ? CC.color.danger : CC.text.secondary, style: StrokeStyle(lineWidth: 2.4))
                .rotationEffect(.degrees(-90))
        }
        .padding(1.2)
        .frame(width: 14, height: 14)
    }
}
