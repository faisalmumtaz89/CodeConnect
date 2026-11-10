import AppKit
import SwiftUI

/// The Mac window's look: near-black surfaces a step apart, hairlines instead of boxes, and grey
/// type. Colour only where it means something: amber for what needs you, red for what failed or
/// was refused, green and red for lines added and removed.
enum Theme {
    static let window = Color(themeHex: 0x111113)
    static let sidebar = Color(themeHex: 0x0C0C0E)
    static let pane = Color(themeHex: 0x151517)
    static let raised = Color(themeHex: 0x1C1C1F)
    static let sheet = Color(themeHex: 0x19191C)
    static let code = Color(themeHex: 0x0E0E10)
    static let hairline = Color.white.opacity(0.07)
    static let selected = Color.white.opacity(0.075)
    /// Under a clickable thing while the pointer is on it: fainter than `selected`, so the two never read alike.
    static let hover = Color.white.opacity(0.04)
    /// The same step for a surface with its own fill (a raised chip, a filled button): a lift in brightness.
    static let hoverLift = 0.04

    static let text = Color(themeHex: 0xE4E4E7)
    static let text2 = Color(themeHex: 0x9C9CA3)
    static let text3 = Color(themeHex: 0x6B6B72)

    static let red = Color(themeHex: 0xE5636A)
    static let amber = Color(themeHex: 0xE9A23B)

    static func ui(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font {
        .system(size: size, weight: weight)
    }

    static func mono(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font {
        .system(size: size, weight: weight, design: .monospaced)
    }
}

extension Color {
    init(themeHex hex: UInt32) {
        self.init(
            .sRGB, red: Double((hex >> 16) & 0xFF) / 255, green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255)
    }
}

/// A key as a hint prints it: a small raised cap.
struct KeyCap: View {
    let key: String

    init(_ key: String) { self.key = key }

    var body: some View {
        Text(key)
            .font(Theme.ui(10.5, .medium))
            .foregroundStyle(Theme.text2)
            .padding(.horizontal, 4)
            .frame(minWidth: 16, minHeight: 16)
            .background(Theme.raised, in: RoundedRectangle(cornerRadius: 3.5))
            .overlay(RoundedRectangle(cornerRadius: 3.5).strokeBorder(Theme.hairline))
    }
}

/// A key and what it does, as the window's footer lists them.
struct KeyHint: View {
    let keys: [String]
    let label: String

    var body: some View {
        HStack(spacing: 5) {
            ForEach(keys, id: \.self) { KeyCap($0) }
            Text(label).font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
        }
    }
}

/// Takes the window's title bar out of the way, so the sidebar runs to the top edge under the
/// traffic lights.
struct WindowChrome: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async {
            guard let window = view.window else { return }
            window.titlebarAppearsTransparent = true
            window.titleVisibility = .hidden
            window.styleMask.insert(.fullSizeContentView)
            window.titlebarSeparatorStyle = .none
            window.backgroundColor = NSColor(Theme.window)
        }
        return view
    }

    func updateNSView(_ view: NSView, context: Context) {}
}
