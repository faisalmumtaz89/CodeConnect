import XCTest

/// Which raw reads of an agent's text the views make, line by line.
///
/// An agent's words reach a view through a model that has already made them
/// `displaySafe` — `ToolSummary`, `CodexCard`, `QuestionCard`, `NoticeItem`,
/// `FleetRow`, `RunLabel`, `prettyJSONString`, the approval's `paneSnapshot`,
/// and `AgentProse.inline` after it parses Markdown (`DecisionSurfaceTextTests`).
///
/// What this test catches: any line under `ios/CodeConnect/Views/` that names
/// one of the raw fields or accessors below, or calls `AttributedString(markdown:`,
/// and is not one of the exact sites listed with it — so a new read of a raw
/// field, an alias of one (`let input = approval.card.toolInput`), or a second
/// `AttributedString(markdown:)` parse fails here with its file and line. What it
/// does not catch: a raw string reached through a name it does not list, or
/// Markdown parsed another way (`Text` given a `LocalizedStringKey` built from a
/// string), which the models' own tests and the rendered scenarios in
/// `DecisionSurfaceTextTests` are left to.
final class DecisionTextSourceTests: XCTestCase {

    /// A raw read, and every place a view may make it: the exact lines, in
    /// order, that hold it. A site of more than one line pins what the read is
    /// handed to, so the same line elsewhere does not pass.
    private struct Rule {
        let pattern: String
        let sites: [(file: String, lines: [String])]
    }

    private static let rules: [Rule] = [
        Rule(pattern: #"toolInput"#, sites: [
            // `ToolSummary.principalArgument`, wrapped.
            ("FleetView.swift", [
                "argument: ToolSummary.principalArgument(",
                "tool: tool, input: front.card.toolInput)?.firstLine),",
            ]),
            ("FleetView.swift", [
                "ToolSummary.principalArgument(",
                "tool: approval.card.toolName, input: approval.card.toolInput)?.firstLine",
            ]),
            ("TimelineRows.swift", [
                "if let argument = ToolSummary.principalArgument(",
                "tool: approval.card.toolName, input: approval.card.toolInput)",
            ]),
            ("TimelineRows.swift", [
                "let argument = ToolSummary.principalArgument(",
                "tool: tool, input: approval.card.toolInput)?.firstLine,",
            ]),
            // `ToolSummary.intent`, wrapped.
            ("DecisionCard.swift", [
                "let intent = ToolSummary.intent(",
                "tool: approval.card.toolName, input: approval.card.toolInput)",
            ]),
            ("DecisionCard.swift", ["let options = CodexCard.options(in: card.toolInput)"]),
            ("DecisionCard.swift", ["if let reason = CodexCard.reason(in: approval.card.toolInput) {"]),
            ("DecisionCard.swift", [
                "private var changes: [CodexCard.Change] { CodexCard.changes(in: approval.card.toolInput) }",
            ]),
            ("DecisionCard.swift", ["CodexCard.changesOmitted(in: approval.card.toolInput)"]),
            ("DecisionCard.swift", ["let cwd = CodexCard.cwd(in: approval.card.toolInput)"]),
        ]),
        Rule(pattern: #"\.cwd\b"#, sites: [
            // Copied, never drawn: the daemon's bytes go to the pasteboard.
            ("FleetView.swift", ["CCPasteboard.copy(row.summary.cwd)"]),
            ("SessionDetailView.swift", [".onLongPressGesture { CCPasteboard.copy(summary.cwd) }"]),
            ("DecisionCard.swift", ["let cwd = CodexCard.cwd(in: approval.card.toolInput)"]),
        ]),
        // The raw tool input of a timeline row: its presence, and its pretty
        // rendering.
        Rule(pattern: #"\btool\.input\b"#, sites: [
            ("TimelineRows.swift", [
                "(tool.input?.isNull == false) || !(tool.output ?? \"\").isEmpty || tool.isTruncated",
            ]),
            ("TimelineRows.swift", [
                "if let input = tool.input, !input.isNull {",
                "CCMonoBlock(input.prettyJSONString, lineLimit: 40)",
            ]),
        ]),
        Rule(pattern: #"displayText"#, sites: []),
        Rule(pattern: #"projectLabel"#, sites: []),
        Rule(pattern: #"\.payload\b|payload\s*\["#, sites: []),
        Rule(pattern: #"notificationMessage"#, sites: []),
        Rule(pattern: #"agentText"#, sites: []),
        Rule(pattern: #"lastAssistantMessage"#, sites: []),
        Rule(pattern: #"userText"#, sites: []),
        // Markdown is parsed for agent text by `AgentProse.inline` alone, which
        // maps what the parse produces.
        Rule(pattern: #"AttributedString\s*\(\s*markdown"#, sites: [
            // The pairing steps: the daemon's own copy, never an agent's.
            ("CCStepRow.swift", ["(try? AttributedString(markdown: message)) ?? AttributedString(message)"]),
        ]),
    ]

    private func viewSources() throws -> [(name: String, lines: [String])] {
        // #filePath is repo/ios/CodeConnectTests/<this file> → up 2 to repo/ios.
        let views = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("CodeConnect/Views")
        let files = try XCTUnwrap(
            FileManager.default.enumerator(at: views, includingPropertiesForKeys: nil)?
                .compactMap { $0 as? URL }.filter { $0.pathExtension == "swift" })
        XCTAssertGreaterThan(files.count, 20, "the views were found at \(views.path)")
        return try files.map { file in
            let lines = try String(contentsOf: file, encoding: .utf8).components(separatedBy: "\n")
                .map { $0.trimmingCharacters(in: .whitespaces) }
            return (file.lastPathComponent, lines)
        }
    }

    /// Whether the read on `lines[index]` is one of `rule`'s sites.
    private func isAllowed(_ rule: Rule, file: String, lines: [String], at index: Int) -> Bool {
        rule.sites.contains { site in
            guard site.file == file else { return false }
            return site.lines.indices.contains { offset in
                let start = index - offset
                guard start >= 0, start + site.lines.count <= lines.count else { return false }
                return Array(lines[start..<(start + site.lines.count)]) == site.lines
            }
        }
    }

    func testNoViewReadsARawAgentFieldOutsideItsSites() throws {
        var offences: [String] = []
        for (name, lines) in try viewSources() {
            for (index, line) in lines.enumerated() where !line.hasPrefix("//") {
                for rule in Self.rules where line.range(of: rule.pattern, options: .regularExpression) != nil {
                    if !isAllowed(rule, file: name, lines: lines, at: index) {
                        offences.append("\(name):\(index + 1): \(line)")
                    }
                }
            }
        }
        XCTAssertEqual(offences, [], "raw agent text read outside its sites:\n" + offences.joined(separator: "\n"))
    }

    /// Every site is still in the source, so the list cannot keep a hole open
    /// for a line that has gone.
    func testEverySiteIsStillInTheViews() throws {
        let sources = Dictionary(try viewSources().map { ($0.name, $0.lines) }, uniquingKeysWith: { a, _ in a })
        for rule in Self.rules {
            for site in rule.sites {
                let lines = try XCTUnwrap(sources[site.file], site.file)
                let found = lines.indices.contains { start in
                    start + site.lines.count <= lines.count
                        && Array(lines[start..<(start + site.lines.count)]) == site.lines
                }
                XCTAssertTrue(found, "\(site.file): \(site.lines.joined(separator: " / ")) is gone")
            }
        }
    }
}
