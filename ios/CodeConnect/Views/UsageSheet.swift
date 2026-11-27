import CodeConnectKit
import SwiftUI

/// **A session's usage, in full** (`ReplyBar`, the Mac's card), opened by the reply box's figures: the conversation's
/// size against its model's window; each window of its account with its share and when it resets, or the words for an
/// account with no plan limits; and the account, by its whole email and plan. Each section only where the app holds
/// it. It asks the computer nothing: drawn from what the app holds, again once a minute while open, so a reset's
/// "resets in" stays current.
struct UsageSheet: View {
    let sessionKey: String

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        CCSheetChrome("Usage", onClose: { dismiss() }) {
            ScrollView {
                TimelineView(.everyMinute) { context in
                    if let bar = model.replyBar(for: sessionKey, now: context.date) {
                        sections(bar)
                            .padding(CC.space.md)
                    }
                }
            }
            .scrollBounceBehavior(.basedOnSize)
        }
    }

    @ViewBuilder
    private func sections(_ bar: ReplyBar) -> some View {
        VStack(alignment: .leading, spacing: CC.space.md) {
            if let context = bar.context {
                CCSectionHeader("Context")
                card { CCFactRow("\(context.used) of \(context.window)", separator: false, value: { EmptyView() }, detail: { EmptyView() }) }
            }
            if !bar.limits.isEmpty || bar.limitsNote != nil {
                CCSectionHeader("Limits")
                card {
                    ForEach(Array(bar.limits.enumerated()), id: \.offset) { index, limit in
                        CCFactRow(
                            limit.caption, value: "\(limit.usedPercent)%", age: limit.resets,
                            separator: index < bar.limits.count - 1 || bar.limitsNote != nil)
                    }
                    if let note = bar.limitsNote {
                        CCFactRow(note, separator: false, value: { EmptyView() }, detail: { EmptyView() })
                    }
                }
            }
            if let account = bar.account {
                CCSectionHeader("Account")
                card {
                    Text(account)
                        .ccType(CC.type.callout)
                        .foregroundStyle(CC.text.primary)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, minHeight: CC.size.hitTarget, alignment: .leading)
                        .padding(.horizontal, CC.space.md)
                        .accessibilityIdentifier("usage.account")
                }
            }
        }
    }

    private func card<Content: View>(@ViewBuilder _ content: @escaping () -> Content) -> some View {
        CCCard(padding: 0) { VStack(spacing: 0) { content() } }
    }
}
