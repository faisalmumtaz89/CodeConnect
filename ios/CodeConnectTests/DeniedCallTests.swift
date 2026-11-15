import XCTest

@testable import CodeConnectKit

/// **A call the person refused reads "denied", not as if it ran.** Both inputs are what a daemon stored or files:
/// - `denied-call-wire.json`: a real claude run on the Claude engine started from an app. An allowed call (seq 3-8),
///   then a call denied from the phone (seq 32-37), after which claude files its refusal as an error `tool_result`
///   and ends the turn. The rows as ccd's store holds them.
/// - `declined-command-wire.json`: what ccd files for a live Codex capture whose command approval was refused, codex
///   completing the item `declined` (re-derived by `ccd/src/codex_link.rs`).
@MainActor
final class DeniedCallTests: XCTestCase {
    private let allowed = "toolu_01SpA61bTRh6dFwPj24FNHmz"
    private let denied = "toolu_019Ah3gUoKTckG3SLtu6jcEy"

    func testACallDeniedFromAnAppReadsDeniedThoughClaudeFiledAnErrorResult() throws {
        let tools = toolStatuses(try WireRecording.decode(try rows()))
        XCTAssertEqual(tools[allowed], .succeeded, "the allowed call")
        XCTAssertEqual(tools[denied], .denied, "the call denied from the phone")
    }

    /// Only a deny given from an app is the person's refusal. One the computer recorded is no proof the call never
    /// ran (a Yes at the Mac followed by Esc records one), and one a timeout settled was no refusal. The result keeps
    /// its reading.
    func testADenyTheComputerRecordedLeavesTheResultsReading() throws {
        for resolvedBy in ["local", "timeout"] {
            var rows = try rows()
            // The deny (seq 34); the allow (seq 5) comes first.
            let at = try XCTUnwrap(rows.lastIndex { $0["kind"] as? String == "approval_resolved" })
            var payload = try XCTUnwrap(rows[at]["payload"] as? [String: Any])
            payload["resolved_by"] = resolvedBy
            rows[at]["payload"] = payload
            XCTAssertEqual(toolStatuses(try WireRecording.decode(rows))[denied], .failed, "resolved by \(resolvedBy)")
        }
    }

    func testACommandCodexCompletedDeclinedReadsDenied() throws {
        let file = try XCTUnwrap(WireRecording.json("declined-command-wire") as? [String: Any])
        let frames = try XCTUnwrap(file["frames"] as? [[String: Any]])
        let tools = toolStatuses(try WireRecording.decode(frames.compactMap { $0["event"] as? [String: Any] }))
        XCTAssertEqual(Array(tools.values), [.denied], "the declined command's one row")
    }

    private func rows() throws -> [[String: Any]] {
        try XCTUnwrap(WireRecording.json("denied-call-wire") as? [[String: Any]])
    }

    /// Each tool row's status, by its call's id, as both apps draw it.
    private func toolStatuses(_ events: [Event]) -> [String: ToolStatus] {
        var statuses: [String: ToolStatus] = [:]
        for item in TimelineBuilder.build(events) {
            if case .tool(let tool) = item.content, let id = tool.toolUseID { statuses[id] = tool.status }
        }
        return statuses
    }
}
