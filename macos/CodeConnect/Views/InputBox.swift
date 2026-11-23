import SwiftUI

/// The one box a message is written in, the start form's and a run's reply box alike: the message on top, and under it
/// one bar, its controls on the leading side and Submit (or what stands in its place) on the trailing side. It draws
/// only the box: each place hands in its own field and controls, with their actions, keys and identifiers.
struct InputBox<Field: View, Leading: View, Trailing: View>: View {
    /// 50 before a run, 46 in one.
    let barHeight: CGFloat
    @ViewBuilder let field: Field
    @ViewBuilder let leading: Leading
    @ViewBuilder let trailing: Trailing

    static var radius: CGFloat { 16 }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            field
                .padding(.top, 18)
                .padding(.horizontal, 20)
                .padding(.bottom, 10)
            HStack(spacing: 6) {
                leading
                Spacer(minLength: 8)
                trailing
            }
            .padding(.horizontal, 12)
            .frame(height: barHeight)
        }
        // Drawn behind the contents: a menu opened from the bar may sit over the box's edge.
        .background { surface }
        .overlay { rim }
    }

    /// A dark fill, darker toward the foot; a fine border; a faint line of light along the inside of its top; two
    /// shadows under it. A shadow's spread is the inset of the shape that casts it.
    private var surface: some View {
        let shape = RoundedRectangle(cornerRadius: Self.radius)
        return ZStack(alignment: .top) {
            shape.inset(by: 16).fill(Color.black).shadow(color: .black.opacity(0.78), radius: 30, y: 24)
            shape.inset(by: 4).fill(Color.black).shadow(color: .black.opacity(0.5), radius: 8, y: 4)
            shape.fill(LinearGradient(
                colors: [Color(themeHex: 0x131316), Color(themeHex: 0x101013)], startPoint: .top, endPoint: .bottom))
            shape.strokeBorder(Color.white.opacity(0.085))
            Rectangle().fill(Color.white.opacity(0.06)).frame(height: 1)
                .padding(.horizontal, Self.radius)
                .padding(.top, 1)
        }
    }

    /// The lit rim: a fine line 4 pt outside the box all round, brightest at its top. It takes no clicks.
    private var rim: some View {
        RoundedRectangle(cornerRadius: Self.radius + 4)
            .strokeBorder(
                LinearGradient(
                    stops: [
                        .init(color: .white.opacity(0.30), location: 0),
                        .init(color: .white.opacity(0.05), location: 0.6),
                        .init(color: .white.opacity(0.12), location: 1),
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

    private static let lit = Color(themeHex: 0xF4F4F5)

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
