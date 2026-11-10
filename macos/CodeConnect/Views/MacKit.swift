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
/// clickable reads as clickable before it is clicked. The fill goes behind the label, so a label that marks itself
/// selected keeps its own fill on top; a label with an opaque face of its own (`over`) takes it on top instead. `id` is
/// the button's accessibility identifier, which a test launch names to draw it hovered.
struct HoverButtonStyle: ButtonStyle {
    var radius: CGFloat = 6
    /// For a label with an opaque fill of its own (a raised chip, a filled button): it lightens by the same step
    /// instead, as a fill under it would not show.
    var over = false
    /// How far the fill reaches past a label with no padding of its own, such as a word alone.
    var outset: CGFloat = 0
    var id: String?

    func makeBody(configuration: Configuration) -> some View {
        Hovered(configuration: configuration, radius: radius, over: over, outset: outset, id: id)
    }

    private struct Hovered: View {
        let configuration: Configuration
        let radius: CGFloat
        let over: Bool
        let outset: CGFloat
        let id: String?
        @State private var hovering = false
        @Environment(\.isEnabled) private var enabled

        var body: some View {
            let lit = (hovering || forced) && enabled
            configuration.label
                .background(lit && !over ? Theme.hover : .clear, in: RoundedRectangle(cornerRadius: radius).inset(by: -outset))
                .brightness(lit && over ? Theme.hoverLift : 0)
                .opacity(configuration.isPressed ? 0.7 : 1)
                // What lights is what a click takes, the label's clear gaps included.
                .contentShape(RoundedRectangle(cornerRadius: radius).inset(by: -outset))
                .onHover { hovering = $0 }
        }

        private var forced: Bool {
            #if DEBUG
                id.map(RenderWindow.hovered.contains) ?? false
            #else
                false
            #endif
        }
    }
}

extension ButtonStyle where Self == HoverButtonStyle {
    /// `HoverButtonStyle`.
    static func hover(radius: CGFloat = 6, over: Bool = false, outset: CGFloat = 0, id: String? = nil) -> HoverButtonStyle {
        HoverButtonStyle(radius: radius, over: over, outset: outset, id: id)
    }
}
