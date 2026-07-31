import SwiftUI

// =============================================================================
//  CCScannerFrame — the viewfinder, drawn by us.
// =============================================================================

/// The scrim, the cutout, the corner brackets and the one instruction, over a
/// live camera preview.
///
/// It exists because `DataScannerViewController`'s own overlay — the yellow
/// highlight and the floating guidance label — belongs to a different design
/// language, and a system chip labelled "QR Code" hovering over this palette is
/// the single most obviously borrowed pixel in the product. The scanner is
/// configured with `isGuidanceEnabled = false` and `isHighlightingEnabled =
/// false`, and this draws what replaces them.
///
/// The scrim is an **even-odd fill**, not a stack of four rectangles: the cutout
/// has to be genuinely clear, and four rects around a rounded hole leave four
/// bright corners exactly where the brackets go.
struct CCScannerFrame: View {
    /// The clear square. 260pt at the reference width — big enough to frame a
    /// terminal's QR at arm's length without the phone having to be close
    /// enough to lose focus.
    var cutout: CGFloat = 260
    /// A code has been read. The brackets tighten and turn `success`, and
    /// `CODE READ` appears — held for 400ms before the caller dismisses,
    /// because an instant dismiss leaves the user unsure whether it worked and
    /// a pairing code is single-use.
    var isDetected: Bool = false
    /// The one instruction. `nil` while the camera is warming up, so the text
    /// does not flash a different sentence at the user mid-aim.
    var caption: String?

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var breathing = false

    var body: some View {
        GeometryReader { proxy in
            let side = min(cutout, min(proxy.size.width, proxy.size.height) - CC.space.xxl * 2)
            // Slightly above centre: the caption below it needs room, and a
            // viewfinder in the exact middle of a tall screen sits low once the
            // instruction is under it.
            let origin = CGPoint(
                x: (proxy.size.width - side) / 2,
                y: (proxy.size.height - side) / 2 - CC.space.xxl)
            let window = CGRect(origin: origin, size: CGSize(width: side, height: side))

            ZStack(alignment: .topLeading) {
                scrim(in: proxy.size, window: window)

                CCScannerBrackets(arm: CC.space.xxl + CC.space.xxs, radius: CC.space.xxs)
                    .stroke(
                        isDetected ? CC.color.success : CC.text.primary,
                        style: StrokeStyle(lineWidth: CC.stroke.focus, lineCap: .round))
                    .frame(width: window.width, height: window.height)
                    .opacity(breathing && !isDetected ? 0.75 : 1)
                    .ccScaleEffect(isDetected ? 0.94 : 1)
                    .offset(x: window.minX, y: window.minY)
                    .ccAnimation(CC.motion.small, value: isDetected)
                    .animation(breatheAnimation, value: breathing)

                caption(window: window, width: proxy.size.width)
            }
            .onAppear { breathing = !reduceMotion }
        }
        .accessibilityElement(children: .contain)
    }

    private func scrim(in size: CGSize, window: CGRect) -> some View {
        Path { path in
            path.addRect(CGRect(origin: .zero, size: size))
            path.addRoundedRect(in: window, cornerSize: CGSize(width: CC.radius.lg, height: CC.radius.lg))
        }
        // 72% `bg`. Not a material: a blur over a camera feed costs a frame
        // budget the preview needs, and its colour is the system's.
        .fill(CC.color.bg.opacity(0.72), style: FillStyle(eoFill: true))
        .accessibilityHidden(true)
    }

    @ViewBuilder
    private func caption(window: CGRect, width: CGFloat) -> some View {
        VStack(spacing: CC.space.sm) {
            if isDetected {
                Text("Code read")
                    .ccType(CC.type.badgeLabel)
                    .foregroundStyle(CC.color.success)
                    .accessibilityLabel("Code read")
                    .transition(.opacity)
            }
            if let caption {
                Text(caption)
                    .ccType(CC.type.callout)
                    .foregroundStyle(CC.text.primary)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        // 24pt below the cutout. No `.thinMaterial` plate — the scrim already
        // provides the contrast, and a second surface here reads as a banner.
        .frame(maxWidth: 280)
        .frame(width: width - CC.space.xl * 2)
        .offset(x: CC.space.xl, y: window.maxY + CC.space.xl)
        .ccAnimation(CC.motion.small, value: isDetected)
    }

    private var breatheAnimation: Animation? {
        guard !reduceMotion else { return nil }
        return .easeInOut(duration: 2.4).repeatForever(autoreverses: true)
    }
}

// MARK: - Brackets

/// Four corner brackets: 2pt stroke, 28pt arms, 4pt corner radius, inset 0 from
/// the cutout.
///
/// Corners rather than a full rectangle because a closed rectangle over a camera
/// reads as a *frame around a photograph*, and the thing being asked for is an
/// aim, not a composition.
struct CCScannerBrackets: Shape {
    var arm: CGFloat = 28
    var radius: CGFloat = 4

    func path(in rect: CGRect) -> Path {
        var path = Path()
        let arm = min(self.arm, min(rect.width, rect.height) / 2 - radius)

        // Top-leading
        path.move(to: CGPoint(x: rect.minX, y: rect.minY + radius + arm))
        path.addLine(to: CGPoint(x: rect.minX, y: rect.minY + radius))
        path.addQuadCurve(
            to: CGPoint(x: rect.minX + radius, y: rect.minY),
            control: CGPoint(x: rect.minX, y: rect.minY))
        path.addLine(to: CGPoint(x: rect.minX + radius + arm, y: rect.minY))

        // Top-trailing
        path.move(to: CGPoint(x: rect.maxX - radius - arm, y: rect.minY))
        path.addLine(to: CGPoint(x: rect.maxX - radius, y: rect.minY))
        path.addQuadCurve(
            to: CGPoint(x: rect.maxX, y: rect.minY + radius),
            control: CGPoint(x: rect.maxX, y: rect.minY))
        path.addLine(to: CGPoint(x: rect.maxX, y: rect.minY + radius + arm))

        // Bottom-trailing
        path.move(to: CGPoint(x: rect.maxX, y: rect.maxY - radius - arm))
        path.addLine(to: CGPoint(x: rect.maxX, y: rect.maxY - radius))
        path.addQuadCurve(
            to: CGPoint(x: rect.maxX - radius, y: rect.maxY),
            control: CGPoint(x: rect.maxX, y: rect.maxY))
        path.addLine(to: CGPoint(x: rect.maxX - radius - arm, y: rect.maxY))

        // Bottom-leading
        path.move(to: CGPoint(x: rect.minX + radius + arm, y: rect.maxY))
        path.addLine(to: CGPoint(x: rect.minX + radius, y: rect.maxY))
        path.addQuadCurve(
            to: CGPoint(x: rect.minX, y: rect.maxY - radius),
            control: CGPoint(x: rect.minX, y: rect.maxY))
        path.addLine(to: CGPoint(x: rect.minX, y: rect.maxY - radius - arm))

        return path
    }
}

// MARK: - Preview

#Preview("CCScannerFrame") {
    ZStack {
        // Stands in for the camera preview.
        LinearGradient(
            colors: [CC.color.surfaceOverlay, CC.color.surface],
            startPoint: .topLeading, endPoint: .bottomTrailing)
        CCScannerFrame(caption: "Point the camera at the QR code in your Mac's terminal.")
    }
    .ignoresSafeArea()
    .ccAppearance()
}
