import SwiftUI
import XCTest

@testable import CodeConnect

/// How `CCToolPairLayout` divides a row between a tool label and its argument:
/// the widest label on the screen that fits in half the row, a wider label half
/// the row and no more, and the argument the rest.
@MainActor
final class ToolPairLayoutTests: XCTestCase {

    private let gap = CCToolColumn.argumentGap

    // MARK: The split

    func testAShortLabelTakesTheWidestLabelThatFits() {
        let split = CCToolPairLayout.split(width: 300, labels: [24, 30, 40], label: 24)
        XCTAssertEqual(split.label, 40)
        XCTAssertEqual(split.argument, 300 - 40 - gap)
    }

    func testAnOutlierTakesHalfItsRowAndNoMore() {
        let split = CCToolPairLayout.split(width: 300, labels: [30, 250], label: 250)
        XCTAssertEqual(split.label, (300 - gap) / 2)
        XCTAssertEqual(split.argument, (300 - gap) / 2)
    }

    func testAnOutlierWidensNobodyElsesColumn() {
        let split = CCToolPairLayout.split(width: 300, labels: [24, 30, 250], label: 24)
        XCTAssertEqual(split.label, 30)
        XCTAssertEqual(split.argument, 300 - 30 - gap)
    }

    func testANarrowOrZeroRowSplitsWhatThereIsAndNeverGoesNegative() {
        let narrow = CCToolPairLayout.split(width: 20, labels: [30], label: 30)
        XCTAssertEqual(narrow.label, (20 - gap) / 2)
        XCTAssertEqual(narrow.argument, (20 - gap) / 2)

        let zero = CCToolPairLayout.split(width: 0, labels: [30], label: 30)
        XCTAssertEqual(zero.label, 0)
        XCTAssertEqual(zero.argument, 0)

        // Narrower than the gap: the gap is what there is, so the argument
        // starts inside the pair, not past it.
        let sliver = CCToolPairLayout.split(width: 4, labels: [30], label: 30)
        XCTAssertEqual(sliver.label + sliver.gap + (sliver.argument ?? -1), 4)
        XCTAssertEqual(zero.label + zero.gap, 0)
    }

    func testANarrowerRowThanTheGapPlacesTheArgumentInsideIt() {
        let frames = layOut(width: 4, rows: [(30, 50)])
        XCTAssertLessThanOrEqual(frames["argument0"]?.maxX ?? .infinity, 4 + 0.01)
    }

    func testBeforeAnythingIsMeasuredTheLabelTakesItsOwnWidth() {
        let short = CCToolPairLayout.split(width: 300, labels: [], label: 30)
        XCTAssertEqual(short.label, 30)
        XCTAssertEqual(short.argument, 300 - 30 - gap)

        let long = CCToolPairLayout.split(width: 300, labels: [], label: 250)
        XCTAssertEqual(long.label, (300 - gap) / 2)

        // Measured, but not yet this label: it still gets its whole width.
        let unseen = CCToolPairLayout.split(width: 300, labels: [24], label: 31)
        XCTAssertEqual(unseen.label, 31)
    }

    func testOfferedNoWidthNothingIsAnOutlier() {
        let ideal = CCToolPairLayout.split(width: nil, labels: [30, 250], label: 30)
        XCTAssertEqual(ideal.label, 250)
        XCTAssertNil(ideal.argument)

        let unbounded = CCToolPairLayout.split(width: .infinity, labels: [30, 250], label: 30)
        XCTAssertEqual(unbounded.label, 250)
        XCTAssertEqual(unbounded.argument, .infinity)
    }

    // MARK: Laid out

    private final class Frames {
        var byName: [String: CGRect] = [:]
    }

    /// A stand-in that wants `width` and takes less when offered less, as a
    /// one-line label or command does.
    private static func part(_ name: String, width: CGFloat, into frames: Frames) -> some View {
        Color.clear
            .frame(idealWidth: width, maxWidth: width)
            .frame(height: 10)
            .onGeometryChange(for: CGRect.self) { $0.frame(in: .named("row")) } action: {
                frames.byName[name] = $0
            }
    }

    /// Lays out pairs of (label width, argument width or nil), each offered
    /// `width` and handed every label's width, and returns every part's frame
    /// within its own row.
    private func layOut(
        width: CGFloat, rows: [(label: CGFloat, argument: CGFloat?)]
    ) -> [String: CGRect] {
        let frames = Frames()
        let labels = Set(rows.map(\.label))
        let view = VStack(spacing: 0) {
            ForEach(rows.indices, id: \.self) { index in
                CCToolPairLayout(labels: labels) {
                    Self.part("label\(index)", width: rows[index].label, into: frames)
                    if let argument = rows[index].argument {
                        Self.part("argument\(index)", width: argument, into: frames)
                    }
                }
                .frame(width: width)
                .coordinateSpace(.named("row"))
            }
        }
        let host = UIHostingController(rootView: view)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 400))
        window.rootViewController = host
        window.isHidden = false
        host.view.layoutIfNeeded()
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
        window.isHidden = true
        return frames.byName
    }

    func testRowsOfferedOneWidthStartTheirArgumentsOnOneEdge() {
        let frames = layOut(width: 300, rows: [(24, 200), (40, 200), (31, 50)])
        let edges = (0..<3).compactMap { frames["argument\($0)"]?.minX }
        XCTAssertEqual(edges.count, 3, "every argument laid out: \(frames)")
        for edge in edges { XCTAssertEqual(edge, 40 + gap, accuracy: 0.01) }
        XCTAssertEqual(frames["label0"]?.width ?? 0, 24, accuracy: 0.01)
    }

    func testAShortLabelBesideAnOutlierKeepsItsOwnColumn() {
        let frames = layOut(width: 300, rows: [(250, 200), (30, 200), (24, 200)])
        let half = (300 - gap) / 2
        // The outlier: half its row, its argument at the half-row edge.
        XCTAssertEqual(frames["label0"]?.width ?? 0, half, accuracy: 0.01)
        XCTAssertEqual(frames["argument0"]?.minX ?? 0, half + gap, accuracy: 0.01)
        XCTAssertEqual(frames["argument0"]?.width ?? 0, half, accuracy: 0.01)
        // The short rows: one edge just past the wider of the two, and room for
        // the whole argument.
        for row in [1, 2] {
            XCTAssertEqual(frames["argument\(row)"]?.minX ?? 0, 30 + gap, accuracy: 0.01)
            XCTAssertEqual(frames["argument\(row)"]?.width ?? 0, 200, accuracy: 0.01)
        }
    }

    /// Nothing to leave room for: a call that names no command is not cut
    /// beside empty space.
    func testWithNoArgumentTheLabelTakesTheRow() {
        let frames = layOut(width: 300, rows: [(250, nil)])
        XCTAssertEqual(frames["label0"]?.minX ?? -1, 0, accuracy: 0.01)
        XCTAssertEqual(frames["label0"]?.width ?? 0, 250, accuracy: 0.01)
        XCTAssertNil(frames["argument0"])

        let wider = layOut(width: 300, rows: [(400, nil)])
        XCTAssertEqual(wider["label0"]?.width ?? 0, 300, accuracy: 0.01)
    }
}
