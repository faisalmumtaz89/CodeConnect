import SwiftUI

/// The CodeConnect mark — "Graphite / light trails".
///
/// Drawn rather than shipped as an image, from the geometry the master artwork
/// states in its own comment: a disc of r=284 centred at (512,512) in a 1024
/// square, cut by horizontal chords at y=415 and y=609, the three bands displaced
/// −74 / 0 / +74 on x, the whole mark rotated −38°.
///
/// **Why not the app icon asset.** `AppIcon` is full-bleed and opaque with its own
/// background gradient, because iOS applies the corner mask and will not accept
/// transparency. On a screen it would render as a grey square, not a mark. This is
/// the same artwork with the plate removed, so it sits on whatever is behind it.
///
/// One number lives here that is not in the SVG: the bands are drawn from the
/// *same* three greys, which carry the whole identity. They are deliberately not
/// design-system tokens — a palette change should never silently restyle the
/// product's mark.
struct CCMark: View {
    var size: CGFloat = 40

    private static let center = CGPoint(x: 512, y: 512)
    private static let radius: CGFloat = 284
    /// Where the chords meet the circle, as angles about the centre. Derived from
    /// the SVG's own endpoints (245.08 and 778.92 at y=415 and y=609) rather than
    /// recomputed, so the two files cannot drift.
    private static let upper = 19.98  // degrees above/below the horizontal axis
    private static let lower = 19.98

    var body: some View {
        Canvas { context, canvasSize in
            let scale = min(canvasSize.width, canvasSize.height) / 1024
            context.translateBy(x: canvasSize.width / 2, y: canvasSize.height / 2)
            context.rotate(by: .degrees(-38))
            context.scaleBy(x: scale, y: scale)
            context.translateBy(x: -512, y: -512)

            context.fill(Self.topBand(), with: .color(Color(.sRGB, red: 0xAD / 255, green: 0xB2 / 255, blue: 0xBB / 255)))
            context.fill(Self.middleBand(), with: .color(Color(.sRGB, red: 0x76 / 255, green: 0x7B / 255, blue: 0x85 / 255)))
            context.fill(Self.bottomBand(), with: .color(Color(.sRGB, red: 0x4E / 255, green: 0x52 / 255, blue: 0x5A / 255)))
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }

    // The three slices, each the disc clipped to one horizontal band. Built with
    // arcs rather than by masking a circle, so the shape is exact at any size and
    // there is no seam where two masks meet.

    private static func topBand() -> Path {
        var path = Path()
        path.addArc(
            center: center, radius: radius,
            startAngle: .degrees(180 + upper), endAngle: .degrees(-upper),
            clockwise: false)
        path.closeSubpath()
        return path.offsetBy(dx: -74, dy: 0)
    }

    private static func middleBand() -> Path {
        var path = Path()
        // Left side, from the upper chord down to the lower one.
        path.addArc(
            center: center, radius: radius,
            startAngle: .degrees(180 + upper), endAngle: .degrees(180 - lower),
            clockwise: true)
        // Right side, back up.
        path.addArc(
            center: center, radius: radius,
            startAngle: .degrees(lower), endAngle: .degrees(-upper),
            clockwise: true)
        path.closeSubpath()
        return path
    }

    private static func bottomBand() -> Path {
        var path = Path()
        path.addArc(
            center: center, radius: radius,
            startAngle: .degrees(180 - lower), endAngle: .degrees(lower),
            clockwise: true)
        path.closeSubpath()
        return path.offsetBy(dx: 74, dy: 0)
    }
}
