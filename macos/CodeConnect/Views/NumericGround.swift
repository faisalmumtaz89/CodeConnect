import CodeConnectKit
import SwiftUI

/// The start pane's ground (`NumericField`): figures in Menlo that gather into slow clouds and stay calm around what
/// stands on it. The pointer lights a soft lens; a click on the bare ground sends one ring outward. Decoration only:
/// hidden from accessibility, never takes focus.
struct NumericGround: View {
    /// Where the form stands, in this view's coordinates: the field is calm there.
    let clear: CGRect

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// The window is key in the active app: the drift runs only there. A render draws its inactive app's window with the
    /// key window's look (`RenderLook`), which `appearsActive` follows, so the app's own activity is read from AppKit
    /// too (`appActive`); without it the drift ran on in a render, which never settled.
    @Environment(\.appearsActive) private var appearsActive
    @State private var appActive = NSApp.isActive
    /// Settings covers the pane without removing it: the drift stops while it does.
    @Environment(\.agentsShown) private var agentsShown
    @State private var traces = Traces()
    @State private var cache = NumericField.Cache()

    /// The pointer's lens and rings, held outside the view's state: a change is drawn at the drift's next frame, so
    /// moving the pointer never redraws the ground by itself (as state, each move redrew it, about 80 times a second).
    private final class Traces {
        var hover: CGPoint?
        var ripples: [NumericField.Ripple] = []
    }

    var body: some View {
        TimelineView(NumericField.schedule(still: reduceMotion, active: agentsShown && appearsActive && appActive)) {
            timeline in
            let moment = NumericField.moment(
                at: timeline.date, still: reduceMotion, hover: traces.hover, ripples: traces.ripples)
            Canvas(rendersAsynchronously: true) { context, size in
                // Each glyph in each neutral is drawn once, as a symbol, and placed per mark: a quarter of the cost of
                // laying out a glyph of text per mark (5.0 ms a frame against 17.4 ms for a 1164×846 panel, offscreen).
                for mark in NumericField.marks(size: size, clear: clear, cache: cache, moment: moment) {
                    guard let symbol = context.resolveSymbol(id: Self.symbol(mark.glyph, mark.stop)) else { continue }
                    var placed = context
                    placed.opacity = mark.opacity
                    placed.draw(symbol, at: mark.point, anchor: .center)
                }
            } symbols: {
                // One list, so every symbol's id is its own (a ForEach inside a ForEach repeats the inner ids).
                ForEach(0..<(NumericField.glyphs.count * NumericField.stops.count), id: \.self) { id in
                    // Menlo has no medium face: medium draws its bold.
                    Text(NumericField.glyphs[id / NumericField.stops.count])
                        .font(Theme.mono(9.8).weight(.medium))
                        .foregroundStyle(Color(themeHex: NumericField.stops[id % NumericField.stops.count]))
                        .tag(id)
                }
            }
        }
        .contentShape(Rectangle())
        // What stands on the ground (the box, its menus) takes the pointer, so the lens never lights under it.
        .onContinuousHover { phase in
            switch phase {
            case .active(let point): traces.hover = point
            case .ended: traces.hover = nil
            }
        }
        .simultaneousGesture(
            SpatialTapGesture().onEnded { tap in
                traces.ripples = NumericField.ring(
                    at: tap.location, time: Date.timeIntervalSinceReferenceDate, adding: traces.ripples, clear: clear,
                    still: reduceMotion)
            }
        )
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            appActive = true
        }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didResignActiveNotification)) { _ in
            appActive = false
        }
        .accessibilityHidden(true)
    }

    private static func symbol(_ glyph: Int, _ stop: Int) -> Int { glyph * NumericField.stops.count + stop }
}
