import AppKit
import CodeConnectKit
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
    /// The run's terminal: its ground, darker than `code`, and the words a program prints in no colour of its own.
    static let terminal = Color(themeHex: 0x0B0B0D)
    static let terminalText = Color(themeHex: 0xB4B4BA)
    /// Inline `code`'s ground inside prose: enough to mark the span on the pane, not a box.
    static let codeGround = Color.white.opacity(0.075)
    static let hairline = Color.white.opacity(0.07)
    /// A list box's corners: a box of rows, as a text field's.
    static let boxRadius: CGFloat = 7
    /// Every list box's fill (Settings' lists, Set Up's checklist): one kind of box, one fill.
    static let boxFill = code.opacity(0.5)
    /// A list box's rows' leading and trailing inset, the one a text field gives its text.
    static let rowInset: CGFloat = 10
    static let selected = Color.white.opacity(0.075)
    /// Under a clickable thing while the pointer is on it: fainter than `selected`, so the two never read alike.
    static let hover = Color.white.opacity(0.04)
    /// The same step for a surface with its own fill (a raised chip, a filled button): a lift in brightness.
    static let hoverLift = 0.04
    /// A control that can't be used now: its label at this opacity.
    static let disabledOpacity = 0.45
    /// How much an unchosen segment lightens under the pointer: its label is dim, so it takes more than `hoverLift`.
    static let segmentLift = 0.15
    /// How far a panel opened by a button stands off it, above or below: a clear gap, never touching.
    static let panelGap: CGFloat = 6
    /// How far a menu opened by a list row's "…" stands off it: close enough that it covers the row's second line whole
    /// rather than cutting through its text.
    static let rowMenuGap: CGFloat = 1

    // The meaning colours and the text greys are the Kit's, shared with the iPhone; the grounds are the Mac's own.
    static let text = CC.text.primary
    static let text2 = CC.text.secondary
    static let text3 = CC.text.tertiary

    static let green = CC.color.success
    static let red = CC.color.danger
    static let amber = CC.color.warning

    /// The words' face: Inter, bundled (Resources/Fonts, copied to the app's Resources and registered there by
    /// Info.plist's ATSApplicationFontsPath): an upright and an italic variable file whose weights, italics and
    /// optical sizes CoreText picks as it does the system face's.
    static let wordsFace = "InterVariable"
    /// The code face: Menlo, the system's, as code editors on a Mac draw code.
    static let codeFace = "Menlo-Regular"

    /// Regular is the face as it is: an explicit regular weight would make a bold italic resolve the italic file's
    /// SemiBold, not its Bold.
    static func ui(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font {
        weight == .regular ? .custom(wordsFace, fixedSize: size) : .custom(wordsFace, fixedSize: size).weight(weight)
    }

    static func mono(_ size: CGFloat) -> Font {
        .custom(codeFace, fixedSize: size)
    }

    /// The same faces for AppKit's measurements, so a width measured is the width drawn. A face that did not register
    /// gives the system's of that size, so a broken install draws the wrong type rather than not opening; a render's
    /// launch check (`RenderWindow.checkFaces`) still fails on it.
    static func uiFont(_ size: CGFloat, _ weight: NSFont.Weight = .regular) -> NSFont {
        let face = NSFont(name: wordsFace, size: size)?.familyName.map {
            NSFontDescriptor(fontAttributes: [.family: $0, .traits: [NSFontDescriptor.TraitKey.weight: weight]])
        }
        return face.flatMap { NSFont(descriptor: $0, size: size) } ?? .systemFont(ofSize: size, weight: weight)
    }

    static func monoFont(_ size: CGFloat) -> NSFont {
        NSFont(name: codeFace, size: size) ?? .monospacedSystemFont(ofSize: size, weight: .regular)
    }
}

extension Color {
    init(themeHex hex: UInt32) {
        self.init(
            .sRGB, red: Double((hex >> 16) & 0xFF) / 255, green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255)
    }
}

/// The root of every SwiftUI hierarchy the app hosts (its window and the AppKit-hosted rows and
/// tables): its text is Inter at 13 pt, macOS's body size, what a Text with no font of its own drew before, in the
/// primary text token (`Theme.text`) where it names no colour of its own, never the system's label colour.
struct ThemeRoot<Content: View>: View {
    let content: Content

    var body: some View { content.font(Theme.ui(13)).foregroundStyle(Theme.text) }
}

/// A key as a hint prints it: a small raised cap, or on a light face (a prominent button's) a faint dark one.
struct KeyCap: View {
    let key: String
    let onLight: Bool

    init(_ key: String, onLight: Bool = false) {
        self.key = key
        self.onLight = onLight
    }

    var body: some View {
        Text(key)
            .font(Theme.ui(10.5, .medium))
            .foregroundStyle(onLight ? Color.black.opacity(0.6) : Theme.text2)
            .padding(.horizontal, 4)
            .frame(minWidth: 16, minHeight: 16)
            .background(onLight ? Color.black.opacity(0.08) : Theme.raised, in: RoundedRectangle(cornerRadius: 3.5))
            .overlay(RoundedRectangle(cornerRadius: 3.5).strokeBorder(onLight ? Color.black.opacity(0.08) : Theme.hairline))
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
