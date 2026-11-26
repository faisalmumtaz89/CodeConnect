import SwiftUI

/// The one box a message is written in, the start form's and a run's reply box alike: the message on top, and under it
/// one bar, its controls on the leading side and Submit (or what stands in its place) on the trailing side, always one
/// row (`BarLayout`). It draws only the box: each place hands in its own field and controls, with their actions, keys
/// and identifiers.
struct InputBox<Field: View, Leading: View, Trailing: View>: View {
    /// 50 before a run, 46 in one.
    let barHeight: CGFloat
    @ViewBuilder let field: Field
    @ViewBuilder let leading: Leading
    @ViewBuilder let trailing: Trailing
    @Environment(\.filesOver) private var filesOver

    static var radius: CGFloat { 16 }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            field
                .padding(.top, 18)
                .padding(.horizontal, 20)
                .padding(.bottom, 10)
            BarLayout {
                leading
                Group { trailing }.layoutValue(key: BarLayout.Trailing.self, value: true)
            }
            .frame(height: barHeight)
            .padding(.horizontal, InputBoxLayout.barInset)
        }
        // Drawn behind the contents, rim and all: a menu opened from the bar may sit over the box's edge and covers it.
        .background {
            surface
            rim
        }
    }

    /// A dark fill, darker toward the foot; a fine border; a faint line of light along the inside of its top; the
    /// box's shadows under it.
    private var surface: some View {
        let shape = RoundedRectangle(cornerRadius: Self.radius)
        return ZStack(alignment: .top) {
            BoxShadow(radius: Self.radius)
            shape.fill(LinearGradient(
                colors: [Color(themeHex: 0x131316), Color(themeHex: 0x101013)], startPoint: .top, endPoint: .bottom))
            shape.strokeBorder(Color.white.opacity(0.085))
            Rectangle().fill(Color.white.opacity(0.06)).frame(height: 1)
                .padding(.horizontal, Self.radius)
                .padding(.top, 1)
        }
    }

    /// The lit rim: a fine line 4 pt outside the box all round, brightest at its top, and brighter while files are
    /// dragged over where they would land. It takes no clicks.
    private var rim: some View {
        RoundedRectangle(cornerRadius: Self.radius + 4)
            .strokeBorder(
                LinearGradient(
                    stops: [
                        .init(color: .white.opacity(filesOver ? 0.7 : 0.30), location: 0),
                        .init(color: .white.opacity(filesOver ? 0.35 : 0.05), location: 0.6),
                        .init(color: .white.opacity(filesOver ? 0.5 : 0.12), location: 1),
                    ],
                    startPoint: .top, endPoint: .bottom),
                lineWidth: 1)
            .padding(-4)
            .allowsHitTesting(false)
    }
}

/// The box's submit: a light square with a dark arrow while it can be pressed, a faint one with a grey arrow while it
/// cannot. What it does, when, and what it is called are its place's own.
struct InputBoxSubmit: View {
    let enabled: Bool
    let id: String
    let label: String
    let help: String
    let action: () -> Void

    /// The lit fill, the app's white: the update button wears it too.
    static let lit = Color(themeHex: 0xF4F4F5)

    var body: some View {
        Button(action: action) {
            Image(systemName: "arrow.up")
                .font(.system(size: 13, weight: .bold))
                .foregroundStyle(enabled ? Theme.window : Theme.text3)
                .frame(width: 30, height: 30)
                .background(enabled ? Self.lit : Color.white.opacity(0.06), in: RoundedRectangle(cornerRadius: 9))
        }
        // The hover style takes a click on its whole frame, so the square's corners press.
        .buttonStyle(.hover(radius: 9, over: true, id: id))
        .disabled(!enabled)
        .help(help)
        .accessibilityLabel(label)
        .accessibilityIdentifier(id)
    }
}

/// A bar of controls in one row, `spacing` apart: the leading ones from its leading edge, those marked `Trailing` at its
/// trailing edge, at least 8 pt from them. A control marked `yields(_:)` has words that cut ("…"), and gives way when
/// the bar is short of room; every other control keeps its own width. Room is shared in two rounds: first every
/// yielding control gets its words up to `InputBoxChip.floor` (all of them, if shorter), and where even that does not
/// fit, the lowest `yields` gives its floor up first; then what is left goes to them whole, highest `yields` first.
/// The bar is wider than it is given only when its controls at their least do not fit; the floors never widen it.
struct BarLayout: Layout {
    /// The control stands at the bar's trailing edge.
    struct Trailing: LayoutValueKey { static let defaultValue = false }
    /// How late the control's words give way: the lowest first. Nil for a control that keeps its width.
    struct Yields: LayoutValueKey { static let defaultValue: Int? = nil }

    var spacing: CGFloat = 6
    static let gap: CGFloat = 8

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let widths = widths(for: proposal.width, subviews)
        let used = widths.reduce(0, +) + gaps(subviews)
        let height = subviews.indices.map { subviews[$0].sizeThatFits(.init(width: widths[$0], height: nil)).height }
            .max() ?? 0
        // As wide as it is given, and never narrower than its controls at their least: what holds it learns what it
        // truly needs, and is never widened by the floors.
        return CGSize(width: max(proposal.width ?? used, used), height: height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        let widths = widths(for: bounds.width, subviews)
        var x = bounds.minX
        var trailingX = bounds.maxX - subviews.indices.filter { subviews[$0][Trailing.self] }
            .reduce(0) { $0 + widths[$1] + spacing } + spacing
        for index in subviews.indices {
            let size = ProposedViewSize(width: widths[index], height: bounds.height)
            if subviews[index][Trailing.self] {
                subviews[index].place(at: CGPoint(x: trailingX, y: bounds.midY), anchor: .leading, proposal: size)
                trailingX += widths[index] + spacing
            } else {
                subviews[index].place(at: CGPoint(x: x, y: bounds.midY), anchor: .leading, proposal: size)
                x += widths[index] + spacing
            }
        }
    }

    private func gaps(_ subviews: Subviews) -> CGFloat {
        let split = subviews.contains { $0[Trailing.self] } && subviews.contains { !$0[Trailing.self] }
        return CGFloat(max(subviews.count - 1, 0)) * spacing + (split ? max(Self.gap - spacing, 0) : 0)
    }

    /// Each control's width in a bar `width` wide (nil: as wide as they want).
    private func widths(for width: CGFloat?, _ subviews: Subviews) -> [CGFloat] {
        let whole = subviews.map { $0.sizeThatFits(.unspecified).width }
        guard let width else { return whole }
        var widths = whole
        let yielding = subviews.indices.filter { subviews[$0][Yields.self] != nil }
            .sorted { subviews[$0][Yields.self]! < subviews[$1][Yields.self]! }
        var least: [Int: CGFloat] = [:]
        for index in yielding {
            least[index] = subviews[index].sizeThatFits(.init(width: 0, height: nil)).width
            widths[index] = min(whole[index], least[index]! + InputBoxChip.floor)
        }
        var room = width - gaps(subviews) - widths.reduce(0, +)
        // Short even of the floors: the lowest gives its floor up first, down to its least.
        for index in yielding where room < 0 {
            let given = min(-room, widths[index] - least[index]!)
            widths[index] -= given
            room += given
        }
        // Room to spare: the highest takes its whole words first.
        for index in yielding.reversed() where room > 0 {
            let taken = min(room, whole[index] - widths[index])
            widths[index] += taken
            room -= taken
        }
        return widths
    }
}

extension View {
    /// A bar control whose words cut ("…") when the bar is short of room, `order` saying how late: the lowest first
    /// (`BarLayout`).
    func yields(_ order: Int) -> some View { layoutValue(key: BarLayout.Yields.self, value: order) }

    /// A chip in the box's bar, drawn around its label: 8 pt in at the sides, `InputBoxChip.height` tall, corners of 8,
    /// filled while its menu is `open`; `bordered`, the model chip's fine border. Its whole frame takes a click.
    func inputBoxChip(open: Bool, bordered: Bool = false) -> some View {
        padding(.horizontal, 8)
            .frame(height: InputBoxChip.height)
            .background(open ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 8))
            .overlay {
                if bordered { RoundedRectangle(cornerRadius: 8).strokeBorder(Color.white.opacity(0.08)) }
            }
            .contentShape(Rectangle())
    }
}

/// The box's measures its places share: the bar's side inset, and the message's line spacing (its text at 15 pt
/// stands 1.55 lines apart).
enum InputBoxLayout {
    static let barInset: CGFloat = 12
    static let lineSpacing: CGFloat = 5
}

/// The bar's chips' one size: their words 12.5 pt medium, 6 pt above and below; words cut keep `floor` while the bar
/// has room for every chip's (`BarLayout`).
enum InputBoxChip {
    static let height: CGFloat = 28
    static let font = Theme.ui(12.5, .medium)
    /// About four letters past "…": a chip cut to it still says which it is.
    static let floor: CGFloat = 40
}

/// The usage tip's card: its fill, its fine border (a file tile's too), and its muted words.
enum Card {
    static let fill = Color(themeHex: 0x141418)
    static let border = Color.white.opacity(0.09)
    static let muted = Color(themeHex: 0x8A8A93)
}

/// The input box's shadows, which the usage tip casts too: a deep soft one and a close one. A shadow's spread is the
/// inset of the shape that casts it.
struct BoxShadow: View {
    let radius: CGFloat

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: radius)
        ZStack {
            shape.inset(by: 16).fill(Color.black).shadow(color: .black.opacity(0.78), radius: 30, y: 24)
            shape.inset(by: 4).fill(Color.black).shadow(color: .black.opacity(0.5), radius: 8, y: 4)
        }
    }
}
