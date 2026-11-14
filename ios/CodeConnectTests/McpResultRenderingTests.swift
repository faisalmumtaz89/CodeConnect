import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Neither app showed what an MCP tool call returned** (R2T1-F1): the result is
/// an array of content blocks — `[{"type":"text","text":…}]` — but the timeline read
/// only plain-string content, so the row's output stayed empty. The fix flattens the
/// blocks (`Event.mcpBlocksText`) for the model's own message stream on both engines:
/// the transcript (tmux) and the SDK (host) carry the identical `message.content[]`.
///
/// Fixtures are daemon-emitted, payloads verbatim:
///   * `mcp-result-transcript-wire.json` — seq 104/108/113 of run
///     01M7C157WHYVPHQZX9J50WH7S0 (app-qa f1-mailread, events.db ccqa.pp47cN). The
///     `read_messages` result. Owner-path metadata (`cwd`, `transcript_path`,
///     `scratchpad_dir`) was dropped; it is not read by the timeline and the result
///     content is untouched.
///   * `mcp-result-sdk-wire.json` — the `Agent` tool_use (item 644bb4f8) and its
///     `tool_result` (item 23273de3, `tool_use_id` toolu_0181j4…) from
///     `fixtures/claude/sdk-session-wire.json`; `seq`/`session_id`/`ts` added to the
///     SDK items (the source wire omits the envelope), payloads verbatim.
@MainActor
final class McpResultRenderingTests: XCTestCase {

    private func events(_ name: String) throws -> [Event] {
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: name, withExtension: "json"))
        return try JSONDecoder().decode([Event].self, from: Data(contentsOf: url))
    }

    private func json(_ s: String) throws -> JSONValue {
        try JSONDecoder().decode(JSONValue.self, from: Data(s.utf8))
    }

    private func toolOutput(_ events: [Event], toolUseID: String) -> String?? {
        for item in TimelineBuilder.build(events) {
            if case .tool(let tool) = item.content, tool.toolUseID == toolUseID { return tool.output }
        }
        return .none
    }

    // MARK: The helper

    /// The leaf rule, with its mutant: a mutant that makes `mcpBlocksText` return nil
    /// (the shipped behaviour) fails the block-array case; an unknown shape abstains so
    /// the caller's `.stringValue` and an unchanged row are preserved.
    func testMcpBlocksTextJoinsTextBlocksAndAbstainsOnEveryOtherShape() throws {
        XCTAssertEqual(
            Event.mcpBlocksText(try json(#"[{"type":"text","text":"one"},{"type":"text","text":"two"}]"#)),
            "one\ntwo", "an array of text blocks is joined")
        XCTAssertNil(
            Event.mcpBlocksText(try json(#"[{"type":"tool_reference","tool_name":"x"}]"#)),
            "a block array with no text block abstains")
        XCTAssertNil(Event.mcpBlocksText(try json(#""just a string""#)), "a plain string is not an array")
        XCTAssertNil(Event.mcpBlocksText(nil), "absent content abstains")
    }

    // MARK: The rendered row, on each engine

    /// The tmux engine's path (transcript). Must-fail before the fix: the row's output
    /// is nil because the block-array result was read as a string.
    func testTheTranscriptMcpRowShowsTheResultText() throws {
        let output = toolOutput(try events("mcp-result-transcript-wire"),
            toolUseID: "toolu_016pGoJ1Wm7Y998V1BDX5bSS")
        XCTAssertNotNil(output, "the read_messages row exists")
        XCTAssertEqual(
            (output ?? nil)?.contains("papaya"), true,
            "the transcript row shows what read_messages returned: \(String(describing: output))")
    }

    /// The host engine's path (SDK) — the one that survives the transition. Same block
    /// shape, a different `source`; the row is built from the agent message's tool_use.
    func testTheSdkToolRowShowsTheResultText() throws {
        let output = toolOutput(try events("mcp-result-sdk-wire"),
            toolUseID: "toolu_0181j4eLd2wa7v9KF2vQszZP")
        XCTAssertNotNil(output, "the SDK tool row exists")
        XCTAssertEqual(
            (output ?? nil)?.contains("Subagent hand-back"), true,
            "the SDK row shows the tool result's text: \(String(describing: output))")
    }

    /// T1-2: a Codex MCP call (read_messages/send_message) is filed by ccd as
    /// `EventKind.other("codex_mcpToolCall")` — call+result in one event. Without a reader it drew NO
    /// row (the person never saw what mail Codex read or sent). It now draws a tool row: name
    /// `mcp__<server>__<tool>`, the arguments as input, the result via `mcpBlocksText`. Fixture is
    /// daemon-emitted, verbatim — codex-mcp-wire.json (run 01M7B2BW…, seq 9 read_messages + seq 11
    /// send_message, events.db ccqa.pp47cN). Mutant: the reader's guard never matches → no row.
    func testACodexMcpCallDrawsAToolRowWithItsResult() throws {
        let rows = TimelineBuilder.build(try events("codex-mcp-wire")).compactMap { item -> ToolItem? in
            if case .tool(let tool) = item.content { return tool }
            return nil
        }
        let read = try XCTUnwrap(
            rows.first { $0.name == "mcp__codeconnect__read_messages" },
            "the Codex read_messages call draws a tool row (it drew none before)")
        XCTAssertEqual(
            read.output?.contains("Message 01M7B2CV") ?? false, true,
            "the row shows what read_messages returned: \(String(describing: read.output))")
        let send = try XCTUnwrap(
            rows.first { $0.name == "mcp__codeconnect__send_message" },
            "the Codex send_message call draws a tool row")
        XCTAssertNotNil(send.output, "the send_message row shows its result")
    }
}
