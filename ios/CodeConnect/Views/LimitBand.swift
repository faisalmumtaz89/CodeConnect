import CodeConnectKit
import SwiftUI

/// **A session stopped at a usage limit**, as the Mac's band says it over the reply box (`LimitFoot`): a red ring, which
/// limit, and when it resets; under it, the account's windows, the full one in red. The account is in Usage. Redrawn
/// each minute while shown, so the countdown stays current; the box stays live under it.
struct LimitBand: View {
    let model: AppModel
    let key: String

    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        TimelineView(.everyMinute) { context in
            if let foot = model.limitFoot(for: key, now: context.date) {
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 8) {
                        UsageRing(percent: 100, red: true)
                        Text(foot.title).ccType(CC.type.footnote).fontWeight(.semibold)
                            .foregroundStyle(CC.text.primary)
                            .lineLimit(1).fixedSize()
                        if let resets = foot.resets, !typeSize.isAccessibilitySize {
                            Text(resets).ccType(CC.type.footnote).foregroundStyle(CC.text.secondary)
                                .lineLimit(1).truncationMode(.tail)
                        }
                    }
                    // At accessibility sizes the title takes the line, so when it resets gets its own.
                    if let resets = foot.resets, typeSize.isAccessibilitySize {
                        Text(resets).ccType(CC.type.footnote).foregroundStyle(CC.text.secondary)
                            .padding(.leading, 22)
                    }
                    if !foot.limits.isEmpty {
                        windows(foot)
                            .ccType(CC.type.footnote)
                            .lineLimit(1).truncationMode(.tail)
                            .padding(.leading, 22)
                    }
                }
                .padding(.horizontal, 4)
                .padding(.bottom, 14)
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("session.limit")
            }
        }
    }

    /// "5-hour 100% · Weekly 64%", the full window in red.
    private func windows(_ foot: LimitFoot) -> Text {
        foot.limits.enumerated().reduce(Text(verbatim: "")) { line, each in
            let part = Text(verbatim: "\(each.element.caption) \(each.element.usedPercent)%")
                .foregroundStyle(each.element.caption == foot.full ? CC.color.danger : CC.text.tertiary)
            return each.offset == 0
                ? part : Text("\(line)\(Text(verbatim: " · ").foregroundStyle(CC.text.tertiary))\(part)")
        }
    }
}
