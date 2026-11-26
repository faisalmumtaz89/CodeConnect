import SwiftUI

/// The start pane's ground, as a model: a field of figures that gathers into slow clouds toward the ground's sides and
/// stays calm around what stands on it, so the box always sits on clean black. The pointer lights a soft lens; a click
/// on the bare ground sends one ring outward. Still under Reduce Motion, paused while the window isn't active.
///
/// What each frame draws is a pure function of the ground's size, the calm rect, the clock and the pointer's traces
/// (`marks`); the app's view places a glyph for each mark.
public enum NumericField {
    /// A click's ring: where, and when it left.
    public struct Ripple: Equatable, Sendable {
        public let point: CGPoint
        public let birth: TimeInterval
    }

    /// What one frame draws from: the clock, and the pointer's lens and rings.
    public struct Moment: Equatable, Sendable {
        public let time: TimeInterval
        public let hover: CGPoint?
        public let ripples: [Ripple]
    }

    /// One figure drawn: where, which of `glyphs`, which of the `stops` neutrals, how opaque.
    public struct Mark: Equatable, Sendable {
        public let point: CGPoint
        public let glyph: Int
        public let stop: Int
        public let opacity: Double
    }

    /// The cell each figure stands in, Menlo 9.8's.
    static let cellWidth: CGFloat = 8.4, cellHeight: CGFloat = 11.6
    public static let glyphs = [".", ":", "1", "7", "2", "3", "5", "4", "6", "9", "8", "0"]
    /// The neutrals a figure is drawn in, faint to bright: #3A3A3F, #6E6E75, #9C9CA3, #D8D8DC.
    public static let stops: [UInt32] = [0x3A3A3F, 0x6E6E75, 0x9C9CA3, 0xD8D8DC]
    /// None within 28pt of what stands on the ground; the full field from 150pt; clouds gather over the next 200pt.
    static let clearIn: CGFloat = 28, clearOut: CGFloat = 150, gather: CGFloat = 200
    /// The field fades out over its last 28pt at the ground's own edges, so it never ends in a hard line.
    static let edge: CGFloat = 28
    static let lensRadius: CGFloat = 150, lensGain = 0.5
    static let rippleSpeed = 340.0, rippleLife = 2.2, rippleWidth = 40.0, maxRipples = 4
    static let presence = 0.95, maxAlpha = 0.62
    /// The drift redraws at most 30 times a second.
    static let frameInterval: TimeInterval = 1.0 / 30
    /// The still frame Reduce Motion shows: a fixed moment of the drift, so the ground looks the same every launch.
    static let stillTime: TimeInterval = 12

    /// The drift's clock: at most 30 frames a second, none under Reduce Motion or while the ground isn't `active` (its
    /// window isn't active, or something covers it).
    public static func schedule(still: Bool, active: Bool) -> AnimationTimelineSchedule {
        .animation(minimumInterval: frameInterval, paused: still || !active)
    }

    /// Under Reduce Motion the ground is one fixed frame with no lens and no rings.
    public static func moment(at date: Date, still: Bool, hover: CGPoint?, ripples: [Ripple]) -> Moment {
        still
            ? Moment(time: stillTime, hover: nil, ripples: [])
            : Moment(time: date.timeIntervalSinceReferenceDate, hover: hover, ripples: ripples)
    }

    /// A click at `point`: one more ring, unless it is within 28pt of what stands on the ground or under Reduce
    /// Motion. Rings that have faded are dropped, and only the newest four stay.
    public static func ring(
        at point: CGPoint, time: TimeInterval, adding ripples: [Ripple], clear: CGRect, still: Bool
    ) -> [Ripple] {
        guard !still, signedDistance(point.x, point.y, to: clear) >= clearIn else { return ripples }
        let live = ripples.filter { time - $0.birth <= rippleLife } + [Ripple(point: point, birth: time)]
        return Array(live.suffix(maxRipples))
    }

    /// What a frame draws: per live cell, the cloud's density there plus the lens and rings, as a glyph, a neutral and
    /// an opacity; faint cells thin out, and the faintest draw nothing.
    public static func marks(size: CGSize, clear: CGRect, cache: Cache, moment: Moment) -> [Mark] {
        let t = moment.time
        var marks: [Mark] = []
        for cell in cache.cells(size: size, clear: clear) {
            let (u, v) = (cell.u, cell.v)
            let cloud = 0.50 + 0.24 * sin(u * 5.2 - t * 0.30 + v * 2.4) + 0.16 * cos(u * 11.0 + v * 7.0 + t * 0.20)
                + 0.10 * sin((u - v) * 17.0 - t * 0.45)
            var d = max(0, cloud * 1.3 - 0.30) * (0.30 + 0.90 * cell.reach) * cell.weight * presence
            var roll = 0.0
            if let hover = moment.hover {
                let dist = Double(hypot(cell.point.x - hover.x, cell.point.y - hover.y))
                if dist < Double(lensRadius) * 1.6 {
                    let falloff = exp(-pow(dist / Double(lensRadius), 2))
                    d += falloff * lensGain * cell.weight
                    roll = falloff * 12
                }
            }
            for ripple in moment.ripples {
                let age = t - ripple.birth
                guard age >= 0, age < rippleLife else { continue }
                let delta = Double(hypot(cell.point.x - ripple.point.x, cell.point.y - ripple.point.y)) - age * rippleSpeed
                guard abs(delta) < rippleWidth * 2 else { continue }
                let ring = exp(-pow(delta / rippleWidth, 2)) * pow(1 - age / rippleLife, 2)
                d += ring * 0.9 * cell.weight
                roll += ring * 9
            }
            d = min(1, d)
            guard d >= 0.06 else { continue }
            if d < 0.28 && cell.hash > (d / 0.28) * 0.88 + 0.06 { continue }
            let glyph =
                d < 0.16
                ? (cell.hash > 0.45 ? 0 : 1)
                : 2 + ((Int(cell.hash * 37 + t * (0.6 + cell.hash * 0.8) + roll) % 10) + 10) % 10
            marks.append(
                Mark(
                    point: cell.point, glyph: glyph, stop: min(stops.count - 1, Int(d * 3.99)),
                    opacity: min(maxAlpha, 0.10 + pow(d, 1.1) * 0.75)))
        }
        return marks
    }

    /// What doesn't move between frames, kept per size and calm rect: each live cell's place, calm and edge fade, how
    /// far the clouds reach there, and its hash. Cells inside the calm zone or off the edge never exist.
    public final class Cache {
        struct Cell {
            let point: CGPoint
            let u: Double
            let v: Double
            /// The calm zone's and the edge's fades, together.
            let weight: Double
            let reach: Double
            let hash: Double
        }
        private var key: (CGSize, CGRect)?
        private var cells: [Cell] = []

        public init() {}

        func cells(size: CGSize, clear: CGRect) -> [Cell] {
            if let key, key.0 == size, key.1 == clear { return cells }
            var built: [Cell] = []
            let cols = Int((size.width / cellWidth).rounded(.up)), rows = Int((size.height / cellHeight).rounded(.up))
            built.reserveCapacity(cols * rows / 2)
            for gy in 0..<rows {
                let y = (CGFloat(gy) + 0.5) * cellHeight
                for gx in 0..<cols {
                    let x = (CGFloat(gx) + 0.5) * cellWidth
                    let sd = Double(signedDistance(x, y, to: clear))
                    let calm = smooth(Double(clearIn), Double(clearOut), sd)
                    let fade = smooth(0, Double(edge), Double(min(x, y, size.width - x, size.height - y)))
                    guard calm > 0, fade > 0 else { continue }
                    built.append(
                        Cell(
                            point: CGPoint(x: x, y: y), u: Double(x / size.width), v: Double(y / size.height),
                            weight: calm * fade, reach: smooth(Double(clearOut), Double(clearOut + gather), sd),
                            hash: Double(cellHash(gx, gy)) / 65535))
                }
            }
            key = (size, clear)
            cells = built
            return built
        }
    }

    /// From a point to a rect with the box's 16pt corners: negative inside.
    static func signedDistance(_ x: CGFloat, _ y: CGFloat, to r: CGRect, radius: CGFloat = 16) -> CGFloat {
        let qx = abs(x - r.midX) - (r.width / 2 - radius), qy = abs(y - r.midY) - (r.height / 2 - radius)
        return hypot(max(qx, 0), max(qy, 0)) + min(max(qx, qy), 0) - radius
    }

    static func smooth(_ a: Double, _ b: Double, _ x: Double) -> Double {
        let t = min(1, max(0, (x - a) / (b - a)))
        return t * t * (3 - 2 * t)
    }

    /// A per-cell value in 0…65535 with no visible period. (A plain xor of two products tiles into repeating runs.)
    static func cellHash(_ gx: Int, _ gy: Int) -> Int {
        var h = UInt32(truncatingIfNeeded: gx) &* 374_761_393 &+ UInt32(truncatingIfNeeded: gy) &* 668_265_263
        h = (h ^ (h >> 13)) &* 1_274_126_177
        return Int((h ^ (h >> 16)) & 0xFFFF)
    }
}
