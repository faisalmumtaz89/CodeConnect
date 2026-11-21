import SwiftUI

/// CodeConnect's mark at any moment of a cold launch, the same on the Mac and the iPhone: the slice.
///
/// The mark (ios/Branding/AppIcon.svg) is a disc of radius 284 centred in a 1024 box, cut by horizontal chords at
/// y = 415 and y = 609 into three bands, the top band slid −74 along the cut, the bottom +74, the whole turned −38°.
/// Fills: #ADB2BB, #767B85, #4E525A.
///
/// The launch starts from the ground alone. Intro: a whole disc grows in, its cuts appear, and the outer bands slide to
/// their offsets: the mark making itself. Hold, looped while the fleet catches up: the outer bands breathe a few points
/// along the cut. Exit, the moment the intro is done and the fleet has caught up: the outer bands slide on along the
/// cut from wherever their breath had them, off both edges, the middle band last, and the app is what is left.
/// Nothing changes opacity; the last frame draws nothing.
///
/// A frame is a pure function of `t`, seconds since the launch surface appeared, and `exitAt`, when the exit began
/// (nil while the fleet is still catching up), so a still at any exact moment and the live surface are one drawing.
public struct LaunchMark: View {
    public let t: Double
    public let exitAt: Double?
    /// The assembled mark, still.
    public let reduceMotion: Bool
    /// The 1024 box's side, in points.
    public let size: CGFloat

    public init(t: Double, exitAt: Double?, reduceMotion: Bool, size: CGFloat = 160) {
        self.t = t
        self.exitAt = exitAt
        self.reduceMotion = reduceMotion
        self.size = size
    }

    public var body: some View {
        Canvas { context, canvas in
            let frame = reduceMotion ? Self.rest : Self.frame(t: t, exitAt: exitAt)
            guard frame.drawn else { return }
            let unit = size / 1024
            if frame.whole {
                let side = 568 * unit * frame.scale
                let disc = CGRect(x: (canvas.width - side) / 2, y: (canvas.height - side) / 2, width: side, height: side)
                context.fill(Path(ellipseIn: disc), with: .color(Self.fills[1]))
                return
            }
            for index in 0..<3 {
                let transform = CGAffineTransform(translationX: canvas.width / 2, y: canvas.height / 2)
                    .scaledBy(x: unit * frame.scale, y: unit * frame.scale)
                    .rotated(by: -38 * .pi / 180)
                    .translatedBy(x: Self.offsets[index] + frame.along[index], y: 0)
                    .translatedBy(x: -512, y: -512)
                context.fill(Self.parts[index].applying(transform), with: .color(Self.fills[index]))
            }
        }
    }

    // MARK: Timing

    /// When the intro has assembled the mark.
    public static let introEnd = 0.75
    /// One loop of the hold, which starts and ends at rest.
    public static let holdPeriod = 2.4
    /// How long the exit runs, ending with nothing drawn.
    public static let exitLength = 0.4
    /// When the line under the mark appears, by a cut, if the launch is still waiting.
    public static let lineAfter = 1.0

    /// When the exit begins for a launch whose fleet caught up at `settledAt`: once the intro has assembled the mark
    /// and the fleet has caught up, never later; the splash never holds back an app that is ready.
    public static func exitStart(settledAt: Double) -> Double { max(introEnd, settledAt) }

    /// Whether the launch surface is gone at `t`: the exit has run its length.
    public static func finished(t: Double, exitAt: Double?) -> Bool {
        guard let exitAt else { return false }
        return t >= exitAt + exitLength
    }

    // MARK: The drawing at a moment

    struct Frame: Equatable {
        var drawn = true
        /// One uncut disc in the middle band's fill: the mark before its cuts appear.
        var whole = false
        /// Each band's slide along the cut, on top of its own offset, in the 1024 box.
        var along: [CGFloat] = [0, 0, 0]
        var scale: CGFloat = 1
    }

    static let fills = [Color(red: 0xAD / 255, green: 0xB2 / 255, blue: 0xBB / 255),
                        Color(red: 0x76 / 255, green: 0x7B / 255, blue: 0x85 / 255),
                        Color(red: 0x4E / 255, green: 0x52 / 255, blue: 0x5A / 255)]
    static let offsets: [CGFloat] = [-74, 0, 74]
    static let rest = Frame()
    /// Far enough along the cut to leave any window, in the 1024 box.
    static let away: CGFloat = 7000
    /// When the disc's cuts appear.
    static let cutAt = 0.25

    static func frame(t: Double, exitAt: Double?) -> Frame {
        guard t > 0 else { return Frame(drawn: false) }
        if let exitAt, t >= exitAt {
            guard !finished(t: t, exitAt: exitAt) else { return Frame(drawn: false) }
            let e = (t - exitAt) / exitLength
            // The outer bands slide on along the cut, from where their breath had them, off both edges; the middle
            // band follows last.
            let slide = breath(at: exitAt) + away * CGFloat(easeIn(e))
            return Frame(along: [-slide, away * CGFloat(easeIn(max(0, e - 0.3) / 0.7)), slide])
        }
        if t < introEnd {
            // A whole disc grows in; its cuts appear, and the outer bands slide out to their offsets.
            let open = 1 - spring(t - cutAt, response: 0.4, damping: 0.72)
            return Frame(
                whole: t < cutAt, along: offsets.map { -$0 * open }, scale: spring(t, response: 0.3, damping: 0.8))
        }
        let breath = breath(at: t)
        return Frame(along: [-breath, 0, breath])
    }

    /// How far the outer bands have breathed along the cut at `t` in the hold: out and back, once a loop, 0 on a loop
    /// boundary.
    static func breath(at t: Double) -> CGFloat {
        guard t > introEnd else { return 0 }
        let loop = (t - introEnd) / holdPeriod
        return 18 * CGFloat(sin(2 * .pi * (loop - loop.rounded(.down))))
    }

    // MARK: Curves

    /// A damped spring from 0 to 1, as SwiftUI's `spring(response:dampingFraction:)` moves.
    static func spring(_ t: Double, response: Double, damping: Double) -> CGFloat {
        guard t > 0 else { return 0 }
        let omega = 2 * .pi / response
        let decay = exp(-damping * omega * t)
        let damped = omega * (1 - damping * damping).squareRoot()
        return CGFloat(1 - decay * (cos(damped * t) + damping * omega / damped * sin(damped * t)))
    }

    static func easeIn(_ x: Double) -> Double {
        let x = min(1, max(0, x))
        return x * x * x
    }

    // MARK: The parts

    /// Each band in the 1024 box, unrotated and unslid: the top segment above y = 415, the middle band between the
    /// chords, the bottom segment below y = 609; a point along each arc every degree.
    static let parts: [Path] = {
        let radius: Double = 284
        // Where the chords meet the circle: 97 from the centre.
        let halfRadians: Double = asin(97 / radius)
        let half: Double = halfRadians * 180 / Double.pi
        func arc(_ from: Double, _ to: Double) -> [CGPoint] {
            let steps: Int = max(2, Int(abs(to - from)))
            return (0...steps).map { (step: Int) -> CGPoint in
                let swept: Double = (to - from) * Double(step) / Double(steps)
                let degrees: Double = from + swept
                let angle: Double = degrees * Double.pi / 180
                let x: CGFloat = CGFloat(512 + radius * cos(angle))
                let y: CGFloat = CGFloat(512 + radius * sin(angle))
                return CGPoint(x: x, y: y)
            }
        }
        func shape(_ points: [CGPoint]) -> Path {
            var path = Path()
            path.addLines(points)
            path.closeSubpath()
            return path
        }
        return [
            shape(arc(180 + half, 360 - half)),
            shape(arc(-half, half) + arc(180 - half, 180 + half)),
            shape(arc(half, 180 - half)),
        ]
    }()
}
