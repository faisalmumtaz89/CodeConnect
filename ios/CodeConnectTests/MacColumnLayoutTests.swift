import XCTest

@testable import CodeConnectKit

/// The Mac column's layout arithmetic (`MacColumnLayout`), one fixture: the Earlier row 30 pt, r1-r10 50 pt each, the tail
/// 1 pt (the column 531 pt), a pane of 200 pt (the highest origin 331). Each case states the origin the reader must be
/// left at, the column's height, and what `apply` hands the follow.
final class MacColumnLayoutTests: XCTestCase {
    private func row(_ place: String, _ height: CGFloat, timeline: Bool = true) -> MacColumnLayout.Row {
        MacColumnLayout.Row(place: place, height: height, isTimeline: timeline)
    }

    private var earlier: MacColumnLayout.Row { row("earlier", 30, timeline: false) }
    private var tail: MacColumnLayout.Row { row("tail", 1, timeline: false) }
    private func rs(_ range: ClosedRange<Int>) -> [MacColumnLayout.Row] { range.map { row("r\($0)", 50) } }
    private func named(_ prefix: String, _ count: Int, _ height: CGFloat) -> [MacColumnLayout.Row] {
        (1...count).map { row("\(prefix)\($0)", height) }
    }

    private func fixture(at origin: CGFloat, pane: CGFloat = 200) -> MacColumnLayout {
        MacColumnLayout(rows: [earlier] + rs(1...10) + [tail], origin: origin, pane: pane)
    }

    private func check(
        _ layout: MacColumnLayout, _ moved: MacColumnLayout.Moved, origin: CGFloat, height: CGFloat, dy: CGFloat,
        grown: CGFloat, file: StaticString = #filePath, line: UInt = #line
    ) {
        XCTAssertEqual(layout.origin, origin, "origin", file: file, line: line)
        XCTAssertEqual(layout.height, height, "column height", file: file, line: line)
        XCTAssertEqual(moved, MacColumnLayout.Moved(dy: dy, grown: grown), "moved", file: file, line: line)
    }

    private var page: [MacColumnLayout.Row] { named("p", 3, 40) }

    // MARK: A page lands

    func test1PageAboveTheReaderKeepsTheirRow() {
        var c = fixture(at: 100)
        let m = c.apply(.init(rows: [earlier] + page + rs(1...10) + [tail]))
        check(c, m, origin: 220, height: 651, dy: 120, grown: 120)
    }

    func test1bEarlierOnScreenIsNeverTheAnchor() {
        var c = fixture(at: 0)
        let m = c.apply(.init(rows: [earlier] + page + rs(1...10) + [tail]))
        check(c, m, origin: 120, height: 651, dy: 120, grown: 120)
    }

    func test1cPageUnderAReaderAtTheEnd() {
        var c = fixture(at: 331)
        let m = c.apply(.init(rows: [earlier] + page + rs(1...10) + [tail]))
        check(c, m, origin: 451, height: 651, dy: 120, grown: 120)
    }

    func test1dPageWithTheBoundaryRowChanging() {
        var c = fixture(at: 100)
        let m = c.apply(.init(rows: [earlier] + page + [row("r1", 70)] + rs(2...10) + [tail]))
        check(c, m, origin: 240, height: 671, dy: 140, grown: 140)
    }

    // MARK: A row measured anew

    func test2RowAboveTheReaderRemeasured() {
        var c = fixture(at: 150)
        let m = c.apply(.init(heights: ["r1": 90]))
        check(c, m, origin: 190, height: 571, dy: 40, grown: 40)
    }

    func test2bRowBelowTheReaderRemeasured() {
        var c = fixture(at: 150)
        let m = c.apply(.init(heights: ["r8": 90]))
        check(c, m, origin: 150, height: 571, dy: 0, grown: 40)
    }

    func test2cTheAnchorRowItselfRemeasured() {
        var c = fixture(at: 150)
        let m = c.apply(.init(heights: ["r3": 80]))
        check(c, m, origin: 150, height: 561, dy: 0, grown: 30)
    }

    func test2dRemeasuredUnderAReaderAtTheEnd() {
        var c = fixture(at: 331)
        let m = c.apply(.init(heights: ["r1": 90]))
        check(c, m, origin: 371, height: 571, dy: 40, grown: 40)
    }

    // MARK: A new width

    private var wider: [String: CGFloat] { Dictionary(uniqueKeysWithValues: (1...10).map { ("r\($0)", CGFloat(60)) }) }

    func test3NewWidthKeepsTheReadersRow() {
        var c = fixture(at: 150)
        let m = c.apply(.init(heights: wider))
        check(c, m, origin: 170, height: 631, dy: 20, grown: 100)
    }

    func test3bNewWidthAtTheEnd() {
        var c = fixture(at: 331)
        let m = c.apply(.init(heights: wider))
        check(c, m, origin: 431, height: 631, dy: 100, grown: 100)
    }

    // MARK: The pane

    func test4PaneShrinksUnderAReaderAtTheEnd() {
        var c = fixture(at: 331)
        let m = c.apply(.init(pane: 150))
        check(c, m, origin: 381, height: 531, dy: 50, grown: 0)
        XCTAssertTrue(c.isAtEnd)
    }

    func test4bPaneShrinksUnderAReaderAway() {
        var c = fixture(at: 100)
        let m = c.apply(.init(pane: 150))
        check(c, m, origin: 100, height: 531, dy: 0, grown: 0)
    }

    func test4cPaneGrowsAtTheEnd() {
        var c = fixture(at: 331)
        let m = c.apply(.init(pane: 260))
        check(c, m, origin: 271, height: 531, dy: -60, grown: 0)
    }

    func test4dPaneGrowsTallerThanTheColumn() {
        var c = fixture(at: 331)
        let m = c.apply(.init(pane: 600))
        check(c, m, origin: 0, height: 531, dy: -331, grown: 0)
    }

    // MARK: The window set afresh

    private var refreshed: [MacColumnLayout.Row] { [earlier] + named("q", 5, 40) + rs(3...10) + [tail] }

    func test5ResetKeepingTheAnchor() {
        var c = fixture(at: 150)
        let m = c.apply(.init(rows: refreshed))
        check(c, m, origin: 250, height: 631, dy: 100, grown: 100)
    }

    func test5bResetKeepingNoRowOnScreen() {
        var c = fixture(at: 150)
        let m = c.apply(.init(rows: [earlier] + named("s", 8, 50) + [tail]))
        check(c, m, origin: 231, height: 431, dy: 81, grown: -100)
    }

    func test5cResetWithoutTheFirstRowOnScreen() {
        var c = fixture(at: 150)
        let m = c.apply(.init(rows: [earlier] + rs(4...10) + [tail]))
        check(c, m, origin: 0, height: 381, dy: -150, grown: -150)
    }

    func test5dResetAtTheEnd() {
        var c = fixture(at: 331)
        let m = c.apply(.init(rows: refreshed))
        check(c, m, origin: 431, height: 631, dy: 100, grown: 100)
    }

    // MARK: The clamps

    func test6ClampAtTheTop() {
        var c = fixture(at: 150)
        let m = c.apply(.init(rows: [earlier] + rs(5...10) + [tail]))
        check(c, m, origin: 0, height: 331, dy: -150, grown: -200)
    }

    func test6bClampAtTheBottom() {
        var c = fixture(at: 250)
        let m = c.apply(.init(rows: [earlier] + rs(1...6) + [tail]))
        check(c, m, origin: 131, height: 331, dy: -119, grown: -200)
    }

    // MARK: New rows and a growing row

    func test7NewRowAtTheEnd() {
        var c = fixture(at: 331)
        let m = c.apply(.init(rows: [earlier] + rs(1...11) + [tail]))
        check(c, m, origin: 381, height: 581, dy: 50, grown: 50)
    }

    func test7bNewRowWhileAway() {
        var c = fixture(at: 100)
        let m = c.apply(.init(rows: [earlier] + rs(1...11) + [tail]))
        check(c, m, origin: 100, height: 581, dy: 0, grown: 50)
    }

    func test7cLastRowGrowsAtTheEnd() {
        var c = fixture(at: 331)
        let m = c.apply(.init(heights: ["r10": 120]))
        check(c, m, origin: 401, height: 601, dy: 70, grown: 70)
    }

    // MARK: The reader's own move, then a measurement

    func test8PageUpThenAMeasurementKeepsTheMove() {
        var c = fixture(at: 331)
        c.scrolled(to: 131)
        let m = c.apply(.init(heights: ["r1": 90]))
        check(c, m, origin: 171, height: 571, dy: 40, grown: 40)
    }

    // MARK: The end's slack

    func test9ExactlyThirtyTwoFromTheEndIsTheEnd() {
        var c = fixture(at: 299)
        let m = c.apply(.init(heights: ["r1": 90]))
        check(c, m, origin: 371, height: 571, dy: 72, grown: 40)
    }

    func test9bThirtyThreeFromTheEndIsAway() {
        var c = fixture(at: 298)
        let m = c.apply(.init(heights: ["r1": 90]))
        check(c, m, origin: 338, height: 571, dy: 40, grown: 40)
    }

    // MARK: What the follow takes out

    func test10TheFollowReadsOnlyTheReadersOwnMove() {
        var c = fixture(at: 331)
        c.scrolled(to: 131)
        let readerMove = c.origin - 331
        let m = c.apply(.init(heights: ["r1": 90]))
        let reading = (dy: (c.origin - 331) - m.dy, grown: (c.height - 531) - m.grown)
        XCTAssertEqual(reading.dy, readerMove)
        XCTAssertEqual(reading.grown, 0)
    }

    // MARK: A row the reader opened at the end

    func test11ARowOpenedAtTheEndKeepsItsTop() {
        var c = fixture(at: 331)
        let top = c.minY(of: "r9")! - c.origin
        let m = c.apply(.init(heights: ["r9": 400]), readerAway: true)
        check(c, m, origin: 331, height: 881, dy: 0, grown: 350)
        XCTAssertEqual(c.minY(of: "r9")! - c.origin, top, "the opened row's top on screen")
    }

    func test11bAReaderAtTheEndWhoHasNotLeftFollowsALanding() {
        var c = fixture(at: 331)
        let m = c.apply(.init(rows: [earlier] + rs(1...11) + [tail]), readerAway: false)
        check(c, m, origin: 381, height: 581, dy: 50, grown: 50)
    }
}
