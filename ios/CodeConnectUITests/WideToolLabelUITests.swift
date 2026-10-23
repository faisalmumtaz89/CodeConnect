import Vision
import XCTest

/// A tool name wider than its row leaves the commands beside it on screen.
///
/// An MCP tool is named `mcp__<server>__<tool>`, and one of them set the shared
/// tool column wider than a phone row: on the fleet, `Bash`, `Edit` and `Read`
/// drew no command at all and the MCP row's card ran off the screen's right
/// edge; in a chat, every tool row and the approval row lost theirs.
///
/// **Read off the pixels.** Every command here is spoken in full whatever is
/// drawn, so its accessibility label proves nothing about what is visible. Each
/// row's own screenshot is read back with text recognition, and the command has
/// to be there whole or cut the way its preview cuts it — `.head` keeps the
/// tail, `.middle` keeps both ends — with enough of it left to recognise: an
/// ellipsis alone is not a command.
final class WideToolLabelUITests: XCTestCase {

    private enum Cut { case head, middle }

    /// Characters of a command that must survive its cut, counted after
    /// `comparable` drops what recognition reads unreliably.
    private let recognisable = 10

    override func setUpWithError() throws {
        continueAfterFailure = true
    }

    private func launch(_ fixture: String, deeplink: String? = nil) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", fixture, "-CC_BIOMETRICS", "allow",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryM",
        ]
        if let deeplink { app.launchArguments += ["-CC_DEEPLINK", deeplink] }
        app.launch()
        return app
    }

    /// The lines of text drawn inside `element`, as recognised in its screenshot.
    /// Runs recognised on one line are joined left to right: monospace spacing
    /// splits `--pass 8` into two.
    private func drawnLines(_ element: XCUIElement) -> [String] {
        guard let image = element.screenshot().image.cgImage else { return [] }
        let request = VNRecognizeTextRequest()
        request.recognitionLevel = .accurate
        request.usesLanguageCorrection = false
        try? VNImageRequestHandler(cgImage: image).perform([request])
        var lines: [(midY: CGFloat, runs: [(x: CGFloat, text: String)])] = []
        for observation in request.results ?? [] {
            guard let text = observation.topCandidates(1).first?.string else { continue }
            let box = observation.boundingBox
            if let index = lines.firstIndex(where: { abs($0.midY - box.midY) < box.height / 2 }) {
                lines[index].runs.append((box.minX, text))
            } else {
                lines.append((box.midY, [(box.minX, text)]))
            }
        }
        return lines.map { $0.runs.sorted { $0.x < $1.x }.map(\.text).joined(separator: " ") }
    }

    /// Letters, digits and slashes: the dots, dashes, quotes and spaces of a
    /// monospace run are where recognition is least reliable (`./` has been
    /// read as `•/`, `…` as `.`).
    private func comparable(_ text: Substring) -> String {
        String(text.filter { $0.isLetter || $0.isNumber || $0 == "/" })
    }

    /// Whether `line` draws `command`, whole or cut by `cut`, with at least
    /// `recognisable` of its characters. Whatever precedes the command on the
    /// line — the tool's own name, itself possibly cut — is ignored, which is
    /// why the command's ellipsis is the line's last.
    private func draws(_ line: String, _ command: String, _ cut: Cut) -> Bool {
        let whole = comparable(command[...])
        if comparable(line[...]).hasSuffix(whole) { return true }
        // Recognition reads an ellipsis as `…`, as two or three dots, or as one
        // dot opening a word — never a path's own `./`.
        let line = line.replacingOccurrences(
            of: #"\.{2,3}|(?<=\s)\.(?=[^./\s])"#, with: "…", options: .regularExpression)
        guard let ellipsis = line.lastIndex(of: "…") else { return false }
        let tail = comparable(line[line.index(after: ellipsis)...])
        // An empty tail is a suffix of everything: `./soak/run.sh …` would pass
        // for `--pass 8` and `--pass 9` alike. The command's end must show.
        guard !tail.isEmpty, whole.hasSuffix(tail) else { return false }
        switch cut {
        case .head:
            return tail.count >= recognisable
        case .middle:
            let before = comparable(line[..<ellipsis])
            let head = (0...min(before.count, whole.count)).last {
                before.hasSuffix(whole.prefix($0))
            } ?? 0
            return head > 0 && head + tail.count >= recognisable
        }
    }

    private func assertDraws(
        _ element: XCUIElement, _ command: String, _ cut: Cut, _ what: String,
        file: StaticString = #filePath, line: UInt = #line
    ) {
        let lines = drawnLines(element)
        XCTAssertTrue(
            lines.contains { draws($0, command, cut) },
            "\(what): \"\(command)\" is not drawn, whole or cut at the \(cut); recognised \(lines)",
            file: file, line: line)
    }

    /// Whether `line` draws `name` cut in the middle: at least three of its
    /// first characters run up to the ellipsis and at least three of its last
    /// run on from it. Measured that way rather than word by word, because
    /// recognition splits `mcp__fil` into `mcp_ fil`.
    private func showsBothEnds(_ line: String, of name: String) -> Bool {
        // The ellipsis as `draws` reads it: `…`, two or three dots, or one dot
        // opening a word.
        let line = line.replacingOccurrences(
            of: #"\.{2,3}|(?<=\s)\.(?=[^./\s])"#, with: "…", options: .regularExpression)
        guard let ellipsis = line.firstIndex(of: "…") else { return false }
        let whole = comparable(name[...])
        let before = comparable(line[..<ellipsis])
        let after = comparable(line[line.index(after: ellipsis)...])
        let head = (0...min(before.count, whole.count)).last { before.hasSuffix(whole.prefix($0)) } ?? 0
        let tail = (0...min(after.count, whole.count)).last { whole.hasSuffix(after.prefix($0)) } ?? 0
        return head >= 3 && tail >= 3
    }

    private func assertInside(
        _ element: XCUIElement, _ screen: CGRect, _ what: String,
        file: StaticString = #filePath, line: UInt = #line
    ) {
        let frame = element.frame
        XCTAssertTrue(
            frame.minX >= screen.minX - 0.5 && frame.maxX <= screen.maxX + 0.5,
            "\(what) runs off the screen: \(frame) in \(screen)", file: file, line: line)
    }

    /// **A tool label is one element, not two.** The column measures each label
    /// from a copy drawn behind it; hidden, the copy is no element of its own,
    /// so VoiceOver reads each tool name once. On the `wideTool` fleet each
    /// short tool's label is found exactly once in its row's tree.
    func testAToolLabelsMeasuringCopyIsNoElement() {
        let app = launch("wideTool")
        XCTAssertTrue(app.descendants(matching: .any)["session-fx-5"].waitForExistence(timeout: 25))
        sleep(2)
        for (key, tool) in [("fx-1", "Bash"), ("fx-2", "Edit"), ("fx-3", "Read")] {
            let row = app.descendants(matching: .any)["session-\(key)"]
            XCTAssertTrue(row.waitForExistence(timeout: 10), key)
            let named = row.descendants(matching: .any)
                .matching(NSPredicate(format: "label == %@", tool)).count
            XCTAssertEqual(named, 1, "\(key): \(tool) is \(named) elements")
        }
    }

    func testFleetRowsKeepTheirCommandsBesideAWideTool() {
        let app = launch("wideTool")
        let mcp = app.descendants(matching: .any)["session-fx-5"]
        XCTAssertTrue(mcp.waitForExistence(timeout: 25), "the MCP tool's row")
        sleep(2)
        let screen = app.windows.firstMatch.frame

        // Each blocked row's front card, a `.head` preview.
        for (key, command) in [
            ("fx-1", "git push --force origin main"),
            ("fx-2", "/Users/dev/app/Sources/Router.swift"),
            ("fx-3", "/Users/dev/app/README.md"),
        ] {
            let row = app.descendants(matching: .any)["session-\(key)"]
            XCTAssertTrue(row.exists, "the \(key) row")
            assertInside(row, screen, "the \(key) row")
            assertDraws(row, command, .head, "the \(key) row")
        }

        // The MCP name is the outlier and widens nobody else's column: the
        // `Bash` row has the room to draw its whole command, as the Deck's
        // accessory bar does.
        let bash = app.descendants(matching: .any)["session-fx-1"]
        let whole = drawnLines(bash)
        XCTAssertTrue(
            whole.contains { comparable($0[...]).hasSuffix(comparable("git push --force origin main")) },
            "the fx-1 row cuts its command; recognised \(whole)")

        // The short rows' commands share one edge, just past the widest short
        // label and the 8pt `CCToolColumn.argumentGap` — not half a row along.
        var labels: [CGRect] = []
        var commands: [CGRect] = []
        for (key, tool, command) in [
            ("fx-1", "Bash", "git push --force origin main"),
            ("fx-2", "Edit", "/Users/dev/app/Sources/Router.swift"),
            ("fx-3", "Read", "/Users/dev/app/README.md"),
        ] {
            let row = app.descendants(matching: .any)["session-\(key)"]
            let label = row.staticTexts[tool]
            let argument = row.staticTexts[command]
            XCTAssertTrue(label.exists && argument.exists, "the \(key) row's label and command")
            labels.append(label.frame)
            commands.append(argument.frame)
        }
        let edges = commands.map(\.minX)
        XCTAssertEqual(
            (edges.max() ?? 0) - (edges.min() ?? 0), 0, accuracy: 0.5,
            "the short rows' commands start on one edge; measured \(edges)")
        if let edge = edges.min(), let start = labels.map(\.minX).min(),
            let widest = labels.map(\.width).max()
        {
            XCTAssertLessThanOrEqual(
                edge - start, widest + 8 + 0.5,
                "commands start \(edge - start)pt after their labels; the widest short label is \(widest)")
        }

        assertInside(mcp, screen, "the MCP row's card")
        XCTAssertTrue(
            app.staticTexts["mcp__filesystem__list_directory_with_sizes"].exists,
            "the MCP tool's name is still spoken in full")
        // Drawn, it is cut in the middle: how it starts names the server, how
        // it ends names the tool, and a cut at either end loses one of them.
        let name = "mcp__filesystem__list_directory_with_sizes"
        let lines = drawnLines(mcp)
        XCTAssertTrue(
            lines.contains { showsBothEnds($0, of: name) },
            "the MCP row's name is not cut in the middle; recognised \(lines)")
        assertDraws(mcp, "/Users/dev/app", .head, "the MCP row")
    }

    func testAChatKeepsItsCommandsBesideAWideApproval() {
        let app = launch("mcpApproval", deeplink: "codeconnect://session/fx-5")
        let list = app.collectionViews["session-timeline"]
        XCTAssertTrue(list.waitForExistence(timeout: 25), "the session's timeline")
        sleep(2)
        let screen = app.windows.firstMatch.frame

        // The tail the timeline opens on: the approval the turn is held on, a
        // `.head` preview, under the last soak pass, a `.middle` one.
        let approval = app.descendants(matching: .any).matching(
            NSPredicate(format: "label BEGINSWITH 'Pending approval' AND label CONTAINS 'mcp__'")
        ).firstMatch
        XCTAssertTrue(approval.waitForExistence(timeout: 10), "the MCP approval row")
        assertInside(approval, screen, "the MCP approval row")
        assertDraws(approval, "/Users/dev/app", .head, "the MCP approval row")

        // Up the turn to the tool rows that share the approval's column: the
        // first three are the three label widths the fixture carries.
        for (tool, argument) in [
            ("Bash", "./soak/run.sh --pass 8"),
            ("MultiEdit", "/Users/dev/app/Sources/Router.swift"),
            ("Read", "/Users/dev/app/README.md"),
            ("Bash", "echo soak"),
        ] {
            let row = app.descendants(matching: .any).matching(
                NSPredicate(format: "label BEGINSWITH %@", "\(tool) tool call, \(argument)")
            ).firstMatch
            var swipes = 0
            while !(row.exists && list.frame.contains(row.frame)), swipes < 8 {
                list.swipeDown(velocity: .slow)
                swipes += 1
            }
            XCTAssertTrue(row.exists, "the \(tool) row")
            assertInside(row, screen, "the \(tool) row")
            assertDraws(row, argument, .middle, "the \(tool) row")
        }
    }
}
