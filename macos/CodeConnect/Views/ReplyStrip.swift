import SwiftUI

/// What a run's strip at the reply box's top says: how full its context is, each usage window its account's agent names,
/// and the account it runs on. Each part is absent until the computer has said it.
struct ReplyStripFacts: Equatable {
    struct Context: Equatable {
        /// Tokens the context holds, and its size.
        let used: Int
        let size: Int
        var share: Double { size > 0 ? Double(used) / Double(size) : 0 }
    }

    /// One usage window, as the agent names it: its length, the share used.
    struct Window: Equatable {
        let minutes: Int
        let share: Double
    }

    /// nil: not reported yet.
    var context: Context?
    /// How many times the conversation was compacted.
    var compactions = 0
    /// nil: the agent reports no windows yet; empty never stands for that.
    var windows: [Window]?
    /// The email of the account the run's process runs on; nil while unknown.
    var email: String?
    /// While a switch is pending: the email of the account the run moves to with its next message.
    var movingTo: String?
}

/// The run's strip: the reply box's own top band, a shade darker, in three groups divided by full-height hairlines:
/// Context, Limits, Account, each a small label over its value, the meters fine two-row dot matrices. Amber only for a
/// figure at its limit (90% and over). Where the run pane is narrow it gives way in a fixed order (`layouts`) and never
/// grows past the pane.
struct ReplyStrip: View {
    let facts: ReplyStripFacts

    /// From the widest to the narrowest; the first whose natural width fits the pane is drawn. The last always fits.
    private enum Layout: CaseIterable {
        /// Long meters, the context's tokens, each window's meter.
        case full
        /// Shorter meters.
        case short
        /// No token figure; the windows by their figures alone.
        case figures
        /// No meters: words and figures.
        case words
        /// As `words`, the account on a second row, its email cut in the middle where it must.
        case twoRows
    }

    var body: some View {
        VStack(spacing: 0) {
            ViewThatFits(in: .horizontal) {
                ForEach(Layout.allCases, id: \.self) { layout in band(layout) }
            }
            Rectangle().fill(Self.line).frame(height: 1)
        }
        .background(Color.black.opacity(0.22))
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("session.composer.strip")
    }

    private static let line = Color.white.opacity(0.07)

    // MARK: Layouts

    @ViewBuilder private func band(_ layout: Layout) -> some View {
        if layout == .twoRows {
            VStack(spacing: 0) {
                row([contextGroup(layout), limitsGroup(layout)].compactMap { $0 })
                if let account = accountGroup(layout, cut: true) {
                    Rectangle().fill(Self.line).frame(height: 1)
                    row([account])
                }
            }
        } else {
            row([contextGroup(layout), limitsGroup(layout), accountGroup(layout, cut: false)].compactMap { $0 })
        }
    }

    private func row(_ groups: [AnyView]) -> some View {
        HStack(spacing: 0) {
            ForEach(Array(groups.enumerated()), id: \.offset) { index, group in
                if index > 0 { Rectangle().fill(Self.line).frame(width: 1) }
                group
                    .padding(.horizontal, 14)
                    .padding(.vertical, 8)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .fixedSize(horizontal: false, vertical: true)
    }

    // MARK: Groups

    private func contextGroup(_ layout: Layout) -> AnyView? {
        AnyView(group("Context") {
            if let context = facts.context {
                if layout == .full || layout == .short { Matrix(share: context.share, columns: layout == .full ? 44 : 24) }
                if layout == .full || layout == .short {
                    figure("\(Self.tokens(context.used)) / \(Self.tokens(context.size))", Theme.text2)
                }
                figure(Self.percent(context.share), Self.tone(context.share, else: Theme.text))
                if facts.compactions > 0 { figure("\(facts.compactions)×", Theme.text3) }
            } else {
                notReported(layout)
            }
        })
    }

    private func limitsGroup(_ layout: Layout) -> AnyView? {
        AnyView(group("Limits") {
            if let windows = facts.windows, !windows.isEmpty {
                ForEach(Array(windows.enumerated()), id: \.offset) { _, window in
                    HStack(spacing: 6) {
                        figure(Self.caption(minutes: window.minutes), Theme.text3)
                        if layout == .full || layout == .short { Matrix(share: window.share, columns: layout == .full ? 14 : 8) }
                        figure(Self.percent(window.share), Self.tone(window.share, else: Theme.text))
                    }
                }
            } else {
                notReported(layout)
            }
        })
    }

    private func accountGroup(_ layout: Layout, cut: Bool) -> AnyView? {
        guard let address = facts.email else { return nil }
        return AnyView(group("Account") {
            Image(systemName: "person.crop.circle").font(.system(size: 11)).foregroundStyle(Theme.text3)
                .accessibilityHidden(true)
            email(address, cut: cut)
            if let to = facts.movingTo {
                figure("→", Theme.text3)
                email(to, cut: cut)
            }
        })
    }

    // MARK: Pieces

    private func group(_ name: String, @ViewBuilder _ content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(name).font(Theme.ui(10.5)).foregroundStyle(Theme.text3).fixedSize()
            HStack(spacing: 8) { content() }.frame(height: 15)
        }
    }

    private func figure(_ text: String, _ color: Color) -> some View {
        Text(text).font(Theme.ui(12.5)).monospacedDigit().foregroundStyle(color).fixedSize()
    }

    /// The email whole on one row; on a row of its own, cut in the middle where the pane is narrower than it.
    @ViewBuilder private func email(_ address: String, cut: Bool) -> some View {
        let text = Text(address).font(Theme.ui(12.5)).foregroundStyle(Theme.text2).lineLimit(1)
        if cut { text.truncationMode(.middle) } else { text.fixedSize() }
    }

    /// A figure the computer hasn't reported: a faint empty meter, as the owner's reference draws one, and the words.
    @ViewBuilder private func notReported(_ layout: Layout) -> some View {
        if layout == .full || layout == .short { Matrix(share: nil, columns: layout == .full ? 14 : 8) }
        figure("Not reported yet", Theme.text3)
    }

    // MARK: Words

    /// A window's caption from its own length, as the agent names it: "5-hour", "Week", else hours or days.
    static func caption(minutes: Int) -> String {
        switch minutes {
        case 300: "5-hour"
        case 10080: "Week"
        case let m where m % 1440 == 0: "\(m / 1440)-day"
        case let m where m % 60 == 0: "\(m / 60)-hour"
        default: "\(minutes)-minute"
        }
    }

    static func tokens(_ count: Int) -> String {
        switch count {
        case 1_000_000...: String(format: "%.1fM", Double(count) / 1_000_000)
        case 1_000...: String(format: "%.1fk", Double(count) / 1_000)
        default: "\(count)"
        }
    }

    static func percent(_ share: Double) -> String { "\(Int((share * 100).rounded()))%" }

    private static func tone(_ share: Double, else color: Color) -> Color { share >= 0.9 ? Theme.amber : color }
}

/// A fine dot matrix, two rows deep, lit from the left to the share; unknown, a faint field with nothing lit.
private struct Matrix: View {
    let share: Double?
    let columns: Int

    var body: some View {
        let lit = Int(((share ?? 0) * Double(columns)).rounded(.up))
        let on = (share ?? 0) >= 0.9 ? Theme.amber : Theme.text2
        Canvas { context, _ in
            for column in 0..<columns {
                for row in 0..<2 {
                    let rect = CGRect(x: CGFloat(column) * 2.6, y: 3 + CGFloat(row) * 2.6, width: 1.6, height: 1.6)
                    let color = share != nil && column < lit ? on : Color.white.opacity(share == nil ? 0.05 : 0.09)
                    context.fill(Path(rect), with: .color(color))
                }
            }
        }
        .frame(width: CGFloat(columns) * 2.6, height: 11)
        .accessibilityHidden(true)
    }
}

#if DEBUG
    extension ReplyStripFacts {
        /// `-CC_STRIP_FIXTURE <name>`: a render's run shows the strip with these made-up facts. `two` (two windows),
        /// `one` (Codex's one window), `none` (Claude today: no context, no windows, no email), `hot` (a window near
        /// full), `moving` (a switch pending).
        static var fixture: ReplyStripFacts? {
            switch UserDefaults.standard.string(forKey: "CC_STRIP_FIXTURE") {
            case "two":
                ReplyStripFacts(
                    context: .init(used: 50_900, size: 1_000_000),
                    windows: [.init(minutes: 300, share: 0.34), .init(minutes: 10080, share: 0.20)], email: "ada@example.com")
            case "one":
                ReplyStripFacts(
                    context: .init(used: 73_400, size: 272_000), windows: [.init(minutes: 300, share: 0.42)],
                    email: "cy@example.com")
            case "none":
                ReplyStripFacts()
            case "hot":
                ReplyStripFacts(
                    context: .init(used: 584_000, size: 1_000_000), compactions: 1,
                    windows: [.init(minutes: 300, share: 0.96), .init(minutes: 10080, share: 0.61)], email: "ada@example.com")
            case "moving":
                ReplyStripFacts(
                    context: .init(used: 50_900, size: 1_000_000),
                    windows: [.init(minutes: 300, share: 0.96), .init(minutes: 10080, share: 0.61)], email: "ada@example.com",
                    movingTo: "ben@example.com")
            default:
                nil
            }
        }
    }
#endif
