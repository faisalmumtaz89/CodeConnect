import SwiftUI

/// A session's reply box surface, the Mac's lit box sized for a thumb: corners of 16, a dark ground lit a shade from
/// above with a hairline edge and a deep shadow under it, and the lit rim 4 pt outside the box, brightest at its top,
/// faint past its middle and a little brighter again at its foot.
struct CCBoxSurface: View {
    static let radius: CGFloat = 16

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: Self.radius, style: .continuous)
        let rim = LinearGradient(
            stops: [
                .init(color: .white.opacity(0.30), location: 0),
                .init(color: .white.opacity(0.05), location: 0.6),
                .init(color: .white.opacity(0.12), location: 1),
            ],
            startPoint: .top, endPoint: .bottom)
        shape
            .fill(
                LinearGradient(
                    colors: [Color(red: 0x13 / 255, green: 0x13 / 255, blue: 0x16 / 255),
                             Color(red: 0x10 / 255, green: 0x10 / 255, blue: 0x13 / 255)],
                    startPoint: .top, endPoint: .bottom)
            )
            .overlay(shape.strokeBorder(Color.white.opacity(0.085), lineWidth: 1))
            .shadow(color: .black.opacity(0.78), radius: 15, y: 14)
            .shadow(color: .black.opacity(0.5), radius: 4, y: 4)
            .background(
                RoundedRectangle(cornerRadius: Self.radius + 4, style: .continuous)
                    .strokeBorder(rim, lineWidth: 1)
                    .padding(-4)
            )
    }
}
