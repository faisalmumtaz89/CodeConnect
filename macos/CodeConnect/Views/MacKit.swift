import AppKit
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
    /// which its underline marks, a word right beside another button in a row (a sheet's Cancel, a folder's Forget), or
    /// a segment of a track whose chosen segment carries the plate, so a track never shows two plates at once. A row of
    /// a list (the sidebar's) takes the ordinary fill, weaker than its selected row's plate.
    var over = false
    /// How much an `over` label lightens.
    var lift = Theme.hoverLift
    /// A label with no padding of its own, such as a word alone: the fill stands `bareBleed` off it.
    var bare = false
    /// The sides of a bare label that sit on its column's edge: there its fill ends on the label, bleeding only
    /// inward, so no fill crosses the edge the column's fields and text keep.
    var flush: Edge.Set = []
    var id: String?

    /// A test launch's `-CC_HOVER` names `id`: what has it draws as if the pointer were on it, a row's "…" included.
    static func forced(_ id: String) -> Bool {
        #if DEBUG
            RenderWindow.hovered.contains(id)
        #else
            false
        #endif
    }

    /// How far a bare label's fill stands off it: sideways, and above and below.
    static let bareBleed = CGSize(width: 8, height: 4)

    func makeBody(configuration: Configuration) -> some View {
        Hovered(configuration: configuration, radius: radius, over: over, lift: lift,
                bleed: bare ? Self.bareBleed : .zero, flush: flush, id: id)
    }

    private struct Hovered: View {
        let configuration: Configuration
        let radius: CGFloat
        let over: Bool
        let lift: Double
        let bleed: CGSize
        let flush: Edge.Set
        let id: String?
        @State private var hovering = false
        @Environment(\.isEnabled) private var enabled

        var body: some View {
            let off = !enabled || forcedState("disabled")
            let lit = (hovering || forced) && !off
            configuration.label
                .disabled(forcedState("disabled"))
                .background(lit && !over ? Theme.hover : .clear, in: plate)
                .brightness(lit && over ? lift : 0)
                .opacity(off ? Theme.disabledOpacity : configuration.isPressed || forcedState("pressed") ? 0.7 : 1)
                // What lights is what a click takes, the label's clear gaps included.
                .contentShape(plate)
                // The keyboard's focus ring takes the hover's shape too: square on a row that fills its list edge to
                // edge, never a rounded pill against the list's straight sides.
                .contentShape(.focusEffect, plate)
                .onHover { hovering = $0 }
        }

        private var plate: Plate { Plate(radius: radius, bleed: bleed, flush: flush) }

        private var forced: Bool { id.map(HoverButtonStyle.forced) ?? false }

        /// A test launch's `-CC_FORCE` names this button in `state`.
        private func forcedState(_ state: String) -> Bool {
            #if DEBUG
                id.flatMap { RenderWindow.forced[$0] } == state
            #else
                false
            #endif
        }
    }
}

/// A hover fill: the label's frame grown by `bleed`, but not past a `flush` side, its corners rounded by `radius`.
private struct Plate: Shape {
    let radius: CGFloat
    let bleed: CGSize
    let flush: Edge.Set

    func path(in rect: CGRect) -> Path {
        let leading = flush.contains(.leading) ? 0 : bleed.width
        let trailing = flush.contains(.trailing) ? 0 : bleed.width
        return RoundedRectangle(cornerRadius: radius).path(
            in: CGRect(
                x: rect.minX - leading, y: rect.minY - bleed.height,
                width: rect.width + leading + trailing, height: rect.height + 2 * bleed.height))
    }
}

extension ButtonStyle where Self == HoverButtonStyle {
    /// `HoverButtonStyle`.
    static func hover(
        radius: CGFloat = 6, over: Bool = false, lift: Double = Theme.hoverLift, bare: Bool = false,
        flush: Edge.Set = [], id: String? = nil
    ) -> HoverButtonStyle {
        HoverButtonStyle(radius: radius, over: over, lift: lift, bare: bare, flush: flush, id: id)
    }
}

/// One exclusive choice among a few options in one rounded track: the chosen segment carries the only plate, and
/// hovering another lightens its label. The segments' corners are the track's less its inset, so the two are concentric.
/// `segment` draws an option's label, given whether it is the chosen one.
struct SegmentedTrack<Option: Equatable, Segment: View>: View {
    let options: [Option]
    let chosen: Option?
    /// The track's own fill, under its segments.
    let fill: Color
    let id: (Option) -> String
    let choose: (Option) -> Void
    @ViewBuilder let segment: (Option, Bool) -> Segment

    var body: some View {
        HStack(spacing: trackInset) {
            ForEach(Array(options.enumerated()), id: \.offset) { _, option in
                let isChosen = option == chosen
                Button {
                    if !isChosen { choose(option) }
                } label: {
                    // As wide as its chosen look whatever is chosen, so choosing moves no segment's label.
                    ZStack {
                        segment(option, true).hidden()
                        segment(option, isChosen)
                    }
                        .background(isChosen ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: trackRadius - trackInset))
                        .contentShape(Rectangle())
                }
                // A dim unchosen label needs more lift than a bright one to show it is under the pointer.
                .buttonStyle(.hover(radius: trackRadius - trackInset, over: true, lift: Theme.segmentLift, id: id(option)))
                .accessibilityIdentifier(id(option))
                .accessibilityAddTraits(isChosen ? .isSelected : [])
            }
        }
        .padding(trackInset)
        .background(fill, in: RoundedRectangle(cornerRadius: trackRadius))
        .overlay(RoundedRectangle(cornerRadius: trackRadius).strokeBorder(Theme.hairline))
    }
}

private let trackRadius: CGFloat = 7
private let trackInset: CGFloat = 2

extension FocusedValues {
    /// The composer is using Esc itself: its commands list is open or its field has focus.
    @Entry var composerHoldsEscape: Bool?
}

extension View {
    /// A control that opens a panel, and toggles it itself. While the panel is open, a click anywhere in the window
    /// outside the control and the panel closes it, and the click still does what it does there (another control's
    /// opens that control's panel); Esc closes it and goes no further, so a second Esc does what Esc did before.
    func closesPanel(_ open: Binding<Bool>) -> some View { modifier(ClosesPanel(open: open)) }

    /// The panel a `closesPanel` control opens: a click on it keeps it open.
    func openPanel() -> some View { modifier(OpenPanel()) }
}

/// Where a control and its open panel are, in the window's coordinates (top-left origin).
final class PanelFrames {
    var control: CGRect = .zero
    var panel: CGRect = .zero
}

extension EnvironmentValues {
    @Entry var panelFrames: PanelFrames?
}

private struct ClosesPanel: ViewModifier {
    @Binding var open: Bool
    @State private var frames = PanelFrames()
    @State private var monitor: Any?

    func body(content: Content) -> some View {
        content
            .environment(\.panelFrames, frames)
            .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { frames.control = $0 }
            .onChange(of: open, initial: true) { _, isOpen in
                stop()
                if isOpen { start() }
            }
            .onDisappear(perform: stop)
    }

    private func start() {
        let frames = frames
        let close = $open
        monitor = NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown, .keyDown]) { event in
            if event.type == .keyDown {
                guard event.keyCode == 53 else { return event }  // Esc
                close.wrappedValue = false
                return nil
            }
            if let view = event.window?.contentView {
                let point = view.convert(event.locationInWindow, from: nil)
                let top = CGPoint(x: point.x, y: view.isFlipped ? point.y : view.bounds.height - point.y)
                if !frames.control.contains(top), !frames.panel.contains(top) { close.wrappedValue = false }
            }
            return event
        }
    }

    private func stop() {
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
    }
}

private struct OpenPanel: ViewModifier {
    @Environment(\.panelFrames) private var frames

    func body(content: Content) -> some View {
        content.onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { frames?.panel = $0 }
    }
}

/// Each "…" panel act's one symbol, chosen once for both apps: the iPhone's menus draw the same act with the Phosphor
/// icon named beside it.
enum PanelSymbol {
    /// iPhone: `power`.
    static let endRun = "power"
    /// iPhone: `trash`.
    static let remove = "trash"
    /// iPhone: `arrows-in`.
    static let compact = "arrow.down.right.and.arrow.up.left"
    /// iPhone: `eraser`.
    static let clear = "eraser"
}

/// One act in a "…" panel, on the Model and Mode panels' row: a quiet icon, the 12.5 pt label 8 pt in, 4 pt above and
/// below, a 4-pt-radius hover plate 4 pt in from the panel's edge. A destructive act's icon and label are red.
struct PanelItem: View {
    let title: String
    let symbol: String
    var destructive = false
    let id: String
    let perform: () -> Void

    var body: some View {
        Button(action: perform) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: symbol).font(.system(size: 11))
                    .foregroundStyle(destructive ? Theme.red : Theme.text3)
                    .frame(width: 14)
                    .accessibilityHidden(true)
                Text(title).font(Theme.ui(12.5)).foregroundStyle(destructive ? Theme.red : Theme.text).lineLimit(1)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 4, id: id))
        .padding(.horizontal, 4)
        .accessibilityIdentifier(id)
    }
}
