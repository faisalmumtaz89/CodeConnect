import XCTest

@testable import CodeConnectKit

/// Lists in an agent's reply: Kit parses them, both apps draw them as lists.
final class AgentListTests: XCTestCase {

    private func lists(_ markdown: String) -> [AgentList] {
        AgentProse.segments(markdown).compactMap { segment in
            if case .list(let list) = segment { return list }
            return nil
        }
    }

    private func shape(_ list: AgentList) -> [String] {
        list.items.map { item in
            let marker: String
            switch item.marker {
            case .bullet: marker = "•"
            case .ordered(let number): marker = number
            case .task(let done): marker = done ? "[x]" : "[ ]"
            }
            return "\(item.depth) \(marker) \(item.text)"
        }
    }

    func testNestedThreeDeepKeepsEachLevel() {
        let found = lists("What changed:\n\n- a\n  - b\n    - c\n  - d\n- e")
        XCTAssertEqual(found.count, 1)
        XCTAssertEqual(shape(found[0]), ["0 • a", "1 • b", "2 • c", "1 • d", "0 • e"])
    }

    func testTabsAndFourSpaceIndentsNestTheSameWay() {
        XCTAssertEqual(shape(lists("- a\n\t- b\n\t\t- c")[0]), ["0 • a", "1 • b", "2 • c"])
        XCTAssertEqual(shape(lists("- a\n    - b")[0]), ["0 • a", "1 • b"])
    }

    func testOrderedTasksAndOtherBullets() {
        XCTAssertEqual(
            shape(lists("1. one\n2) two\n* star\n+ plus\n- [x] done\n- [ ] open")[0]),
            ["0 1. one", "0 2) two", "0 • star", "0 • plus", "0 [x] done", "0 [ ] open"])
    }

    func testAnIndentedLineContinuesTheItemAbove() {
        XCTAssertEqual(shape(lists("- first line\n  second line\n- next")[0]), ["0 • first line second line", "0 • next"])
    }

    func testABlankLineBetweenItemsKeepsOneList() {
        XCTAssertEqual(shape(lists("- a\n\n- b")[0]), ["0 • a", "0 • b"])
    }

    func testTheListEndsWhereProseOrAFenceStarts() {
        let segments = AgentProse.segments("- a\n- b\nAfter the list.\n```\n- not an item\n```")
        XCTAssertEqual(segments.count, 3)
        guard case .list(let list) = segments[0], case .prose(let prose) = segments[1],
            case .code(let code) = segments[2]
        else { return XCTFail("segments: \(segments)") }
        XCTAssertEqual(shape(list), ["0 • a", "0 • b"])
        XCTAssertEqual(prose, "After the list.")
        XCTAssertEqual(code, "- not an item")
    }

    func testRulesAndMarkersWithoutWordsStayText() {
        for text in ["---", "- - -", "* * *", "-", "1.", "-no space", "**bold** start", "2026 was the year"] {
            XCTAssertTrue(lists(text).isEmpty, text)
        }
    }

    func testTheCollapsedPreviewIsTheTextAsWritten() {
        let text = "Intro\n- a\n  - b\n\n- c"
        XCTAssertEqual(AgentProse.previewSource(text), "Intro\n- a\n  - b\n- c")
    }

    /// A pipe table straight under a list, with no blank line, is text as it was before lists were parsed; after a
    /// blank line it is a table.
    func testATableNeedsABlankLineAfterAList() {
        let tight = "- a\n| x | y |\n|---|---|\n| 1 | 2 |"
        XCTAssertEqual(AgentProse.previewSource(tight), tight)
        let apart = "- a\n\n| x | y |\n|---|---|\n| 1 | 2 |"
        XCTAssertTrue(AgentProse.segments(apart).contains { if case .table = $0 { return true } else { return false } })
    }
}
