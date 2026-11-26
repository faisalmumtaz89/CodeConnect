import CodeConnectKit
import SwiftUI

/// Over the reply box while the run is at a usage limit (`LimitFoot`): which limit, when it resets, the account's
/// windows with the full one red, and the account. It holds no state and asks the computer nothing: it draws what the
/// app holds, again each minute, so the countdown stays current.
struct LimitBand: View {
    let model: AppModel
    let key: String

    var body: some View {
        TimelineView(.everyMinute) { context in
            if let foot = model.limitFoot(for: key, now: context.date) {
                VStack(alignment: .leading, spacing: 9) {
                    HStack(spacing: 8) {
                        UsageRing(percent: 100, red: true)
                        Text(foot.title).font(Theme.ui(13, .medium)).foregroundStyle(Theme.text)
                        Spacer(minLength: 12)
                        if let resets = foot.resets {
                            Text(resets).font(Theme.ui(12)).foregroundStyle(Theme.text3).lineLimit(1)
                        }
                    }
                    if !foot.limits.isEmpty || foot.account != nil {
                        HStack(spacing: 12) {
                            ForEach(foot.limits, id: \.caption) { limit in
                                let full = limit.caption == foot.full
                                HStack(spacing: 5) {
                                    UsageRing(percent: limit.usedPercent, red: full)
                                    Text(limit.caption).font(Theme.ui(12)).foregroundStyle(Theme.text2)
                                    Text("\(limit.usedPercent)%").font(Theme.mono(11))
                                        .foregroundStyle(full ? Theme.red : Theme.text3)
                                }
                            }
                            if let account = foot.account {
                                Text("· \(account)").font(Theme.ui(12)).foregroundStyle(Theme.text3).lineLimit(1)
                                    .truncationMode(.middle)
                            }
                        }
                    }
                }
                .padding(.horizontal, 24)
                .padding(.vertical, 14)
                .overlay(alignment: .top) { Rectangle().fill(Theme.hairline).frame(height: 1) }
                .padding(.horizontal, 4)
                .onConversationColumn()
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("session.limit")
            }
        }
    }
}
