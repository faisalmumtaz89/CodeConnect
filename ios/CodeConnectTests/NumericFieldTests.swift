import SwiftUI
import XCTest

@testable import CodeConnectKit

/// The start pane's ground: what each frame draws is a pure function of the size, the calm rect, the clock and the
/// pointer's traces. These are the rules the design sets for it (screens 06 and 06b).
final class NumericFieldTests: XCTestCase {
    /// A panel as the Mac draws it, with the box at its middle.
    private let size = CGSize(width: 1164, height: 846)
    private let box = CGRect(x: 222, y: 350, width: 720, height: 146)
    private let date = Date(timeIntervalSinceReferenceDate: 800_000_000)

    private func marks(_ moment: NumericField.Moment, clear: CGRect? = nil) -> [NumericField.Mark] {
        NumericField.marks(size: size, clear: clear ?? box, cache: NumericField.Cache(), moment: moment)
    }

    private func moment(_ t: TimeInterval, hover: CGPoint? = nil, ripples: [NumericField.Ripple] = []) -> NumericField.Moment {
        NumericField.moment(at: Date(timeIntervalSinceReferenceDate: t), still: false, hover: hover, ripples: ripples)
    }

    /// From a point to the box's rounded rect (16pt corners).
    private func distance(_ p: CGPoint, _ r: CGRect) -> CGFloat {
        let qx = abs(p.x - r.midX) - (r.width / 2 - 16), qy = abs(p.y - r.midY) - (r.height / 2 - 16)
        return hypot(max(qx, 0), max(qy, 0)) + min(max(qx, qy), 0) - 16
    }

    func testTheDriftRedrawsAtMostThirtyTimesASecond() {
        let entries = Array(NumericField.schedule(still: false, active: true).entries(from: date, mode: .normal).prefix(60))
        XCTAssertEqual(entries.count, 60, "the drift runs while the window is active")
        for (a, b) in zip(entries, entries.dropFirst()) {
            // Dates this far from the reference date round to about 3e-8 s.
            XCTAssertGreaterThanOrEqual(b.timeIntervalSince(a), 1.0 / 30 - 1e-6)
        }
    }

    func testTheDriftPausesWhileTheWindowIsNotActive() {
        let entries = NumericField.schedule(still: false, active: false).entries(from: date, mode: .normal)
        XCTAssertEqual(Array(entries.prefix(1)), [])
    }

    /// Reduce Motion: no clock, one fixed frame at any time, and no lens or rings.
    func testReduceMotionIsOneStillFrameWithNoLensAndNoRings() {
        XCTAssertEqual(Array(NumericField.schedule(still: true, active: true).entries(from: date, mode: .normal).prefix(1)), [])
        let ring = NumericField.Ripple(point: CGPoint(x: 60, y: 60), birth: date.timeIntervalSinceReferenceDate)
        let first = NumericField.moment(at: date, still: true, hover: CGPoint(x: 80, y: 80), ripples: [ring])
        let later = NumericField.moment(at: date.addingTimeInterval(7.3), still: true, hover: nil, ripples: [])
        XCTAssertEqual(first, later)
        XCTAssertNil(first.hover)
        XCTAssertEqual(first.ripples, [])
        XCTAssertEqual(
            NumericField.ring(at: CGPoint(x: 40, y: 40), time: 1, adding: [], clear: box, still: true), [],
            "a click sends no ring")
    }

    func testAClickSendsOneRingAndAtMostFourStay() {
        var rings: [NumericField.Ripple] = []
        for i in 0..<6 {
            rings = NumericField.ring(
                at: CGPoint(x: 40, y: 40 + Double(i)), time: 100 + 0.1 * Double(i), adding: rings, clear: box, still: false)
        }
        XCTAssertEqual(rings.count, 4)
        XCTAssertEqual(rings.map(\.birth), (2..<6).map { 100 + 0.1 * Double($0) }, "the newest four")
        // A ring that has faded is gone at the next click.
        let after = NumericField.ring(at: CGPoint(x: 40, y: 40), time: 103, adding: rings, clear: box, still: false)
        XCTAssertEqual(after.map(\.birth), [103])
    }

    /// Clicks within 28pt of the box, inside it included, send nothing; from 28pt out they do.
    func testNoRingStartsWithin28ptOfTheBox() {
        for (point, sends) in [
            (CGPoint(x: box.midX, y: box.midY), false),
            (CGPoint(x: box.midX, y: box.minY - 27), false),
            (CGPoint(x: box.maxX + 27.9, y: box.midY), false),
            (CGPoint(x: box.midX, y: box.maxY + 28), true),
            (CGPoint(x: box.minX - 28, y: box.midY), true),
        ] {
            let rings = NumericField.ring(at: point, time: 1, adding: [], clear: box, still: false)
            XCTAssertEqual(rings.count, sends ? 1 : 0, "\(point)")
        }
    }

    /// No figure within 28pt of the box; under 150pt the field is calmer than it is beyond; it fades over its last
    /// 28pt at the ground's edges. With the lens and rings at their brightest, at many moments.
    func testTheFieldIsEmptyNearTheBoxCalmToItAndFadesAtTheEdges() {
        let cache = NumericField.Cache()
        for cell in cache.cells(size: size, clear: box) {
            let d = Double(distance(cell.point, box))
            let edge = Double(min(cell.point.x, cell.point.y, size.width - cell.point.x, size.height - cell.point.y))
            XCTAssertGreaterThan(d, 28)
            XCTAssertLessThanOrEqual(cell.weight, NumericField.smooth(28, 150, d) + 1e-12)
            XCTAssertLessThanOrEqual(cell.weight, NumericField.smooth(0, 28, edge) + 1e-12)
            if d >= 150 && edge >= 28 { XCTAssertEqual(cell.weight, 1, accuracy: 1e-12) }
        }
        for step in 0..<40 {
            let t = date.timeIntervalSinceReferenceDate + Double(step) * 0.37
            let lit = moment(
                t, hover: CGPoint(x: box.minX - 30, y: box.midY),
                ripples: [.init(point: CGPoint(x: box.midX, y: box.maxY + 40), birth: t - 0.1)])
            for mark in marks(lit) {
                XCTAssertGreaterThan(distance(mark.point, box), 28)
            }
        }
    }

    /// Four neutrals, twelve glyphs, never above 62% opacity, at any moment, under the lens and in a ring.
    func testEveryFigureIsAKnownGlyphInANeutralAtMost62PercentOpaque() {
        var seen = Set<Int>()
        for step in 0..<40 {
            let t = date.timeIntervalSinceReferenceDate + Double(step) * 0.53
            let lit = moment(
                t, hover: CGPoint(x: 120, y: 140),
                ripples: (0..<4).map { .init(point: CGPoint(x: 100 + 40 * Double($0), y: 120), birth: t - 0.2 * Double($0)) })
            for mark in marks(lit) {
                XCTAssertLessThanOrEqual(mark.opacity, 0.62)
                XCTAssertTrue(NumericField.glyphs.indices.contains(mark.glyph))
                XCTAssertTrue(NumericField.stops.indices.contains(mark.stop))
                seen.insert(mark.stop)
            }
        }
        XCTAssertEqual(seen, [0, 1, 2, 3], "all four neutrals are used")
        XCTAssertEqual(NumericField.stops, [0x3A3A3F, 0x6E6E75, 0x9C9CA3, 0xD8D8DC])
    }

    /// A ring travels out at 340pt a second and is gone after 2.2 seconds.
    func testARingTravelsAt340ptASecondAndFadesOver2Point2Seconds() {
        let origin = CGPoint(x: 120, y: 120)
        let t = date.timeIntervalSinceReferenceDate
        let plain = marks(moment(t))
        for age in [0.5, 1.0] {
            let ringed = marks(moment(t, ripples: [.init(point: origin, birth: t - age)]))
            let changed = Set(ringed.map { "\($0.point)|\($0.glyph)|\($0.opacity)" })
                .symmetricDifference(plain.map { "\($0.point)|\($0.glyph)|\($0.opacity)" })
            let points = (ringed + plain).filter { changed.contains("\($0.point)|\($0.glyph)|\($0.opacity)") }.map(\.point)
            XCTAssertFalse(points.isEmpty, "age \(age)")
            for point in points {
                // The ring's band: within twice its 40pt width of 340 × age from where it was clicked.
                XCTAssertEqual(Double(hypot(point.x - origin.x, point.y - origin.y)), 340 * age, accuracy: 80, "age \(age)")
            }
        }
        XCTAssertEqual(marks(moment(t, ripples: [.init(point: origin, birth: t - 2.2)])), plain)
    }

    /// The lens lifts only the figures near the pointer, and the field without a pointer is the plain drift.
    func testTheLensLightsOnlyNearThePointer() {
        let t = date.timeIntervalSinceReferenceDate
        let pointer = CGPoint(x: 140, y: 160)
        let plain = marks(moment(t))
        let lit = marks(moment(t, hover: pointer))
        XCTAssertNotEqual(lit, plain)
        let far = { (m: NumericField.Mark) in hypot(m.point.x - pointer.x, m.point.y - pointer.y) >= 240 }
        XCTAssertEqual(lit.filter(far), plain.filter(far))
        XCTAssertGreaterThan(lit.filter { !far($0) }.count, plain.filter { !far($0) }.count)
    }
}
