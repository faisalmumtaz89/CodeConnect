import XCTest

@testable import CodeConnectKit

/// `TimelineItem.places`: the place of each row in the Mac's Conversation, which keeps the reader's row while a page of
/// earlier history lands. A tool call's row is built from whichever of its events the window holds, so its id changes
/// when the page brings the call; its place must not. Places are unique in every column.
final class TimelinePlacesTests: XCTestCase {
    private let at = Date(timeIntervalSince1970: 1_700_000_000)

    private func row(_ id: String, _ content: TimelineItem.Content) -> TimelineItem {
        TimelineItem(id: id, seq: 1, date: at, content: content)
    }

    private func tool(_ id: String, use: String?) -> TimelineItem {
        row(
            id,
            .tool(
                ToolItem(
                    toolUseID: use, name: "Bash", argument: nil, status: .succeeded,
                    durationMS: nil, input: nil, output: nil, isTruncated: false)))
    }

    private func assertUnique(_ items: [TimelineItem], file: StaticString = #filePath, line: UInt = #line) {
        let places = TimelineItem.places(items)
        XCTAssertEqual(places.count, items.count, file: file, line: line)
        XCTAssertEqual(Set(places).count, places.count, "places are unique: \(places)", file: file, line: line)
    }

    /// A result whose call was not loaded is drawn on its own; once the page brings the call, it is the call's row.
    func testAResultKeepsItsPlaceWhenItsCallLands() {
        let before = [tool("s#2502#t#tool_result", use: "U"), row("s#2503#t#turn_complete", .agentMessage("done"))]
        let after = [row("s#2500#t#turn_complete", .agentMessage("earlier")), tool("s#2501#t#tool_call", use: "U")]
            + before.dropFirst()
        XCTAssertEqual(TimelineItem.places(before)[0], TimelineItem.places(after)[1])
        assertUnique(before)
        assertUnique(after)
    }

    /// The transcript's `tool_use` stands for a call whose hook event was not loaded; the page brings the hook's call.
    func testTheTranscriptsToolUseKeepsItsPlaceWhenTheHooksCallLands() {
        let before = [tool("s#10#t#agent_message#U", use: "U")]
        let after = [tool("s#9#t#tool_call", use: "U")]
        XCTAssertEqual(TimelineItem.places(before), TimelineItem.places(after))
    }

    /// Two results of one call drawn while the call is not loaded: the first takes the call's place, the second keeps
    /// its own, so no two rows share a place.
    func testTwoResultsOfOneCallStayUnique() {
        let before = [tool("s#20#t#tool_result", use: "U"), tool("s#21#t#tool_result", use: "U")]
        let after = [tool("s#18#t#tool_call", use: "U")]
        assertUnique(before)
        XCTAssertEqual(TimelineItem.places(before), ["tool.U", "s#21#t#tool_result"])
        XCTAssertEqual(TimelineItem.places(before)[0], TimelineItem.places(after)[0])
    }

    func testTwoCallsHaveTwoPlaces() {
        let items = [tool("s#1#t#tool_call", use: "U"), tool("s#4#t#tool_call", use: "V")]
        assertUnique(items)
        XCTAssertEqual(TimelineItem.places(items), ["tool.U", "tool.V"])
    }

    /// Anything that is not a tool, and a tool row without a call id, is placed by its own id.
    func testOtherRowsArePlacedByTheirID() {
        let items = [
            row("s#1#t#user_message", .userMessage("hi")),
            tool("s#2#t#tool_call", use: nil),
            row("s#3#t#agent_message", .agentMessage("ok")),
        ]
        assertUnique(items)
        XCTAssertEqual(TimelineItem.places(items), items.map(\.id))
    }
}
