import CodeConnectKit
import SwiftUI

/// The message pill's surface, wherever a message is written (a session's reply box, a start's first message): the
/// top of the surface ladder, lit from inside, a hairline rim and a soft glow under it, brightest where the edge
/// faces up or down and gone by the sides' middle. One surface, so the two boxes cannot drift apart.
struct CCPillSurface: View {
    /// The one-line height: the 36pt disc and 8pt of pill round it.
    static let height = CC.size.controlSm + 2 * CC.space.xs

    /// Half the one-line height: a capsule at one line, and a corner that stays concentric with the circle in it
    /// at every size.
    private var shape: RoundedRectangle {
        RoundedRectangle(cornerRadius: Self.height / 2, style: .continuous)
    }

    var body: some View {
        let light = LinearGradient(
            stops: [
                .init(color: CC.color.borderFocus, location: 0),
                .init(color: CC.color.borderFocus.opacity(0), location: 0.3),
                .init(color: CC.color.borderFocus.opacity(0), location: 0.7),
                .init(color: CC.color.borderFocus, location: 1),
            ],
            startPoint: .top, endPoint: .bottom)
        ZStack {
            shape.fill(CC.color.surfaceOverlay)
            shape.strokeBorder(light, lineWidth: 3)
                .blur(radius: 2)
                .opacity(0.15)
                .clipShape(shape)
            shape.strokeBorder(light, lineWidth: CC.stroke.hairline)
                .opacity(0.24)
        }
    }
}
