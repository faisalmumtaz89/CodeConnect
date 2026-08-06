import SwiftUI
import XCTest

@testable import CodeConnect

/// Geometry claims about the table renderer, measured through a real window —
/// "wide tables scroll" and "one cell cannot drag the grid a thousand points
/// wide" are layout facts, and layout facts are measured, not reasoned about.
@MainActor
final class AgentTableLayoutTests: XCTestCase {

    private func parsedTable(
        _ markdown: String, file: StaticString = #filePath, line: UInt = #line
    ) -> MarkdownTable {
        for segment in AgentProse.segments(markdown) {
            if case .table(let table) = segment { return table }
        }
        XCTFail("no table in fixture", file: file, line: line)
        return MarkdownTable(headers: [], alignments: [], rows: [], raw: "")
    }

    /// Mounts the renderer at a phone-ish width and returns the bridged
    /// UIScrollView, whose `contentSize` is the measured truth about how wide
    /// the grid actually laid out.
    private func mountedScrollView(
        _ table: MarkdownTable, width: CGFloat = 350
    ) -> (scroll: UIScrollView?, window: UIWindow) {
        let host = UIHostingController(rootView: AgentTableView(table: table))
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: width, height: 800))
        window.rootViewController = host
        window.isHidden = false
        host.view.layoutIfNeeded()
        return (firstScrollView(in: host.view), window)
    }

    private func firstScrollView(in view: UIView) -> UIScrollView? {
        if let scroll = view as? UIScrollView { return scroll }
        for subview in view.subviews {
            if let found = firstScrollView(in: subview) { return found }
        }
        return nil
    }

    func testAWideTableLaysOutWiderThanTheViewportSoTheScrollIsReal() {
        let table = parsedTable(
            """
            | Check | Where | Result | Duration | Notes |
            |---|---|---:|---:|---|
            | workspace build | continuous integration | passing | 412s | cached |
            | unit tests | continuous integration | passing | 98s | all suites |
            """)
        let (scroll, _) = mountedScrollView(table)
        guard let scroll else { return XCTFail("no scroll view mounted") }
        XCTAssertGreaterThan(
            scroll.contentSize.width, scroll.bounds.width + 1,
            "five real columns must overflow a 350pt viewport and pan")
    }

    func testTheCellCapKeepsOneLongCellFromDraggingTheGridWide() {
        let url = "https://example.com/" + String(repeating: "segment/", count: 60)
        let table = parsedTable("| Name | Link |\n|---|---|\n| artifact | \(url) |")
        let (scroll, _) = mountedScrollView(table)
        guard let scroll else { return XCTFail("no scroll view mounted") }
        // Uncapped, a ~500-character URL measures thousands of points; capped,
        // the whole grid is two bounded columns plus padding.
        XCTAssertLessThan(
            scroll.contentSize.width, 600,
            "the 240pt cell cap must hold: \(scroll.contentSize)")
        XCTAssertGreaterThan(
            scroll.contentSize.height, 100,
            "the capped cell wraps vertically instead of truncating")
    }

    /// GFM's alignment colons are physical left/right — GitHub renders
    /// `:---` on the left in any page language. SwiftUI's semantic
    /// leading/trailing flip under right-to-left, so the view's mapping must
    /// compensate; center never moves.
    func testAlignmentColonsStayPhysicalUnderRightToLeft() {
        XCTAssertEqual(
            AgentTableView.gridAlignment(for: .leading, rightToLeft: false), .leading)
        XCTAssertEqual(
            AgentTableView.gridAlignment(for: .trailing, rightToLeft: false), .trailing)
        XCTAssertEqual(
            AgentTableView.gridAlignment(for: .leading, rightToLeft: true), .trailing)
        XCTAssertEqual(
            AgentTableView.gridAlignment(for: .trailing, rightToLeft: true), .leading)
        XCTAssertEqual(
            AgentTableView.gridAlignment(for: .center, rightToLeft: true), .center)
        XCTAssertEqual(
            AgentTableView.textAlignment(for: .leading, rightToLeft: true), .trailing)
        XCTAssertEqual(
            AgentTableView.textAlignment(for: .trailing, rightToLeft: true), .leading)
        XCTAssertEqual(
            AgentTableView.textAlignment(for: .center, rightToLeft: false), .center)
    }

    /// The refactor's invariant: deriving the hang from the declared column
    /// must land exactly where the old private constant did — the border
    /// steps `CC.space.sm` into the gutter, no more, no less.
    func testTheDerivedHangEqualsTheOldConstantUnderTheTimelineColumn() {
        XCTAssertEqual(
            CCColumn.hang(from: TimelineSpine.content), -CC.space.sm,
            "a declared column hangs back exactly its own padding")
        XCTAssertEqual(
            CCColumn.hang(from: 0), CCColumn.content - CC.space.sm,
            "free-standing surfaces still find the content column themselves")
    }

    func testAStressTableLaysOut() {
        let header = "| " + (0..<10).map { "col\($0)" }.joined(separator: " | ") + " |"
        let delimiter = "|" + Array(repeating: "---", count: 10).joined(separator: "|") + "|"
        let rows = (0..<50).map { row in
            "| " + (0..<10).map { "r\(row)c\($0)" }.joined(separator: " | ") + " |"
        }
        let table = parsedTable(([header, delimiter] + rows).joined(separator: "\n"))
        XCTAssertEqual(table.rows.count, 50)
        let (scroll, _) = mountedScrollView(table)
        guard let scroll else { return XCTFail("no scroll view mounted") }
        XCTAssertGreaterThan(
            scroll.contentSize.height, 500,
            "fifty rows render whole — no virtualization, no clamp")
    }
}
