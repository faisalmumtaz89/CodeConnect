import CodeConnectKit
import SwiftUI

extension AttributedString {
    /// Markdown as the Mac draws it. A `code` run names a command, and a command reads as one
    /// unit, so its spaces do not break: the whole command wraps to the next line together. A link
    /// is underlined in the accent colour, so it reads as one whatever colour its text is set in; a
    /// click opens it.
    init(macMarkdown source: String) {
        let unbroken = source.replacing(/`[^`]*`/) { $0.output.replacingOccurrences(of: " ", with: "\u{00A0}") }
        self = (try? AttributedString(markdown: unbroken)) ?? AttributedString(source)
        for range in runs.filter({ $0.link != nil }).map(\.range) {
            self[range].foregroundColor = CC.color.accent
            self[range].underlineStyle = .single
        }
    }
}

extension DaemonEndpoint {
    /// The address as a person types it into the code form: `host:port`, an IPv6 host in brackets.
    var typedAddress: String { host.contains(":") ? "[\(host)]:\(port)" : "\(host):\(port)" }
}

/// A line that says how long ago `since` was, kept true while it is on screen: Kit's `AgeTick` wakes it when its
/// printed age changes, as a fleet row's age is kept. With no `since` there is no age, and it never wakes.
struct AgeText: View {
    let since: Date?
    let text: (Date) -> String
    @State private var lastTick = Date()

    var body: some View {
        Text(text(AgeTick.renderTime(lastTick: lastTick)))
            .task(id: since.map { AgeClock(since: $0, scale: .age) }) {
                guard let since else { return }
                await AgeTick.follow(AgeClock(since: since, scale: .age)) { lastTick = $0 }
            }
    }
}

extension FleetGroup {
    /// The group in an accessibility identifier: a project's id holds a line break between its computer and its folder,
    /// which reads as `computer:folder` here.
    var axID: String { id.replacingOccurrences(of: "\n", with: ":") }
}

/// Every button that draws its own look: its label as given, with a faint fill while the pointer is on it, so anything
/// clickable reads as clickable before it is clicked. The rule: a label with padding of its own is its fill's shape; a
/// bare label standing alone (a link, a disclosure, a tool's row: `bare`) has its fill stand `bareBleed` off it, 8 pt
/// sideways and 4 pt above and below, past its layout, so no neighbour moves; and a bare label in a tab bar or right
/// beside another button, or one with an opaque face of its own (`over`), lightens instead and takes no fill, so neither
/// a fill nor a click can reach its neighbour. What lights is what a click takes. The fill goes behind the label, so a
/// label that marks itself selected keeps its own fill on top. `id` is the button's accessibility identifier, which a
/// test launch names to draw it hovered.
struct HoverButtonStyle: ButtonStyle {
    var radius: CGFloat = 6
    /// Lightens instead of taking a fill: a label with an opaque face of its own (a raised chip, a filled button), a tab,
    /// which its underline marks, or a word right beside another button in a row (a sheet's Cancel, a folder's Forget).
    var over = false
    /// A label with no padding of its own, such as a word alone: the fill stands `bareBleed` off it.
    var bare = false
    var id: String?

    /// How far a bare label's fill stands off it: sideways, and above and below.
    static let bareBleed = CGSize(width: 8, height: 4)

    func makeBody(configuration: Configuration) -> some View {
        Hovered(configuration: configuration, radius: radius, over: over,
                bleed: bare ? Self.bareBleed : .zero, id: id)
    }

    private struct Hovered: View {
        let configuration: Configuration
        let radius: CGFloat
        let over: Bool
        let bleed: CGSize
        let id: String?
        @State private var hovering = false
        @Environment(\.isEnabled) private var enabled

        var body: some View {
            let lit = (hovering || forced) && enabled
            configuration.label
                .background(lit && !over ? Theme.hover : .clear, in: plate)
                .brightness(lit && over ? Theme.hoverLift : 0)
                .opacity(configuration.isPressed ? 0.7 : 1)
                // What lights is what a click takes, the label's clear gaps included.
                .contentShape(plate)
                .onHover { hovering = $0 }
        }

        private var plate: Plate { Plate(radius: radius, bleed: bleed) }

        private var forced: Bool {
            #if DEBUG
                id.map(RenderWindow.hovered.contains) ?? false
            #else
                false
            #endif
        }
    }
}

/// A hover fill: the label's frame grown by `bleed`, its corners rounded by `radius`.
private struct Plate: Shape {
    let radius: CGFloat
    let bleed: CGSize

    func path(in rect: CGRect) -> Path {
        RoundedRectangle(cornerRadius: radius).path(in: rect.insetBy(dx: -bleed.width, dy: -bleed.height))
    }
}

extension ButtonStyle where Self == HoverButtonStyle {
    /// `HoverButtonStyle`.
    static func hover(radius: CGFloat = 6, over: Bool = false, bare: Bool = false, id: String? = nil) -> HoverButtonStyle {
        HoverButtonStyle(radius: radius, over: over, bare: bare, id: id)
    }
}

extension FocusedValues {
    /// The composer is using Esc itself: its commands list is open or its field has focus.
    @Entry var composerHoldsEscape: Bool?
}
