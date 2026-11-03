import XCTest

/// Where a command sits beside its tool's name.
///
/// On the fleet, the commands beside tool names share one left edge. Measured
/// before that was enforced, with `Edit`/`Bash`/`Read`, they began at
/// 83.67/90.00/90.67. In the conversation a tool call reads as one line of words
/// instead: the command follows its own name, on its line, at every size.
///
/// Asserted rather than left to a render, because both are invisible in review:
/// the code reads perfectly well either way, and the difference only shows once
/// rows with different-width tool names sit above one another.
final class ToolColumnUITests: XCTestCase {

    private func launch(_ size: String) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "stacked",
            "-CC_BIOMETRICS", "allow",
            "-UIPreferredContentSizeCategoryName", size,
        ]
        app.launch()
        return app
    }

    /// Every command **inside the scrolling list**, by its left edge.
    ///
    /// Scoped to the list on purpose. The Deck's pinned accessory bar also
    /// renders a command, but on its own line at the content column rather than
    /// beside a tool name — a different construction with a different correct
    /// answer, and including it made this test fail on layout that is right.
    private func commandOrigins(_ app: XCUIApplication, containing needles: [String]) -> [CGFloat] {
        let list = app.collectionViews["session-timeline"]
        guard list.waitForExistence(timeout: 10) else { return [] }
        let bounds = list.frame
        // The rows sit above the tail the screen opens on, and a `List` holds
        // only the rows near its viewport: nudge the first one into the list's
        // frame, a short drag at a time so the others stay in it too.
        if let first = needles.first {
            let row = app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", first)).firstMatch
            var attempts = 0
            while !(row.exists && bounds.contains(CGPoint(x: row.frame.midX, y: row.frame.midY))),
                attempts < 8
            {
                list.coordinate(withNormalizedOffset: CGVector(dx: 0.97, dy: 0.3)).press(
                    forDuration: 0.05,
                    thenDragTo: list.coordinate(withNormalizedOffset: CGVector(dx: 0.97, dy: 0.45)),
                    withVelocity: .default, thenHoldForDuration: 0.25)
                attempts += 1
            }
        }
        var origins: [CGFloat] = []
        for index in 0..<app.staticTexts.count {
            let element = app.staticTexts.element(boundBy: index)
            guard element.exists, element.frame.width > 0 else { continue }
            guard bounds.contains(CGPoint(x: element.frame.midX, y: element.frame.midY)) else {
                continue
            }
            if needles.contains(where: { element.label.contains($0) }) {
                origins.append(element.frame.minX)
            }
        }
        return origins
    }

    /// In the conversation each command follows its own tool's name on the
    /// name's line, one argument gap after it, at L and at AX5, where the pair
    /// used to stack.
    func testTimelineToolCallsReadAsOneLine() {
        for size in ["UICTContentSizeCategoryL", "UICTContentSizeCategoryAccessibilityXXXL"] {
            let app = launch(size)
            let row = app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'app-5'")).firstMatch
            XCTAssertTrue(row.waitForExistence(timeout: 25), "the running session row, \(size)")
            row.tap()
            XCTAssertTrue(app.buttons["session-more"].waitForExistence(timeout: 20), "session detail, \(size)")

            // `Bash`, `Read` and `MultiEdit`: different widths, so a shared
            // column would show as commands starting on one edge.
            let commands = ["echo soak", "README.md", "Router.swift"]
            _ = commandOrigins(app, containing: commands)
            let list = app.collectionViews["session-timeline"].frame
            let texts = (0..<app.staticTexts.count).map { app.staticTexts.element(boundBy: $0) }
                .filter { $0.exists && $0.frame.width > 0 && list.contains(CGPoint(x: $0.frame.midX, y: $0.frame.midY)) }
            let names = texts.filter { ["Bash", "Read", "MultiEdit"].contains($0.label) }.map(\.frame)
            var checked = 0
            for command in texts where commands.contains(where: { command.label.contains($0) }) {
                let line = command.frame
                let name = names.filter { $0.maxX <= line.minX + 0.5 && $0.minY < line.maxY && line.minY < $0.maxY }
                    .max { $0.maxX < $1.maxX }
                XCTAssertNotNil(name, "\(command.label) sits on a tool name's line, \(size)")
                if let name {
                    XCTAssertLessThanOrEqual(
                        line.minX - name.maxX, argumentGap + 0.5,
                        "\(command.label) follows its name, \(size): name ends \(name.maxX), command starts \(line.minX)")
                }
                checked += 1
            }
            XCTAssertGreaterThanOrEqual(checked, 2, "the fixture must reach tool rows, \(size)")
            app.terminate()
        }
    }

    /// `CCToolColumn.argumentGap` (8): the UI test target does not link the app.
    private let argumentGap: CGFloat = 8

    func testFleetCommandsShareOneLeftEdge() {
        let app = launch("UICTContentSizeCategoryL")
        XCTAssertTrue(app.staticTexts.firstMatch.waitForExistence(timeout: 25))

        // Scoped to the session rows. The pinned accessory bar overlays the
        // scroll view and also renders a command — on its own line at the
        // content column, which is a different construction and correctly a
        // different edge — so a screen-wide query fails on layout that is right.
        let rows = app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'session-'"))
        var origins: [CGFloat] = []
        for index in 0..<rows.count {
            let row = rows.element(boundBy: index)
            guard row.exists else { continue }
            for textIndex in 0..<row.staticTexts.count {
                let text = row.staticTexts.element(boundBy: textIndex)
                guard text.exists, text.frame.width > 0 else { continue }
                if ["git push --force", "README.md", "Router.swift", "echo soak"]
                    .contains(where: { text.label.contains($0) })
                {
                    origins.append(text.frame.minX)
                }
            }
        }
        XCTAssertGreaterThanOrEqual(origins.count, 3, "the fleet must show three tool rows")
        let spread = (origins.max() ?? 0) - (origins.min() ?? 0)
        XCTAssertEqual(
            spread, 0, accuracy: 0.5,
            "commands must start on one edge; measured origins \(origins)")
    }

    /// **Nothing enforced the 44pt floor anywhere**, which is how a row shipped
    /// at 36 against a spec that named both numbers ("36pt tall, 44pt hit
    /// area"). This asserts the rule itself rather than that one row.
    ///
    /// `isHittable` is the filter that matters: a row with nothing to expand is
    /// not a control and correctly stays at its 36pt reading height.
    func testEveryHittableControlMeetsTheTouchFloor() {
        // Every surface that carries controls. The per-screen sweep measured
        // each of these once; this is what keeps them measured.
        for screen in ["fleet", "detail", "link-health", "settings"] {
            let app = XCUIApplication()
            app.launchArguments = [
                "-CC_FIXTURE", "stacked", "-CC_BIOMETRICS", "allow",
                "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
            ]
            app.launch()
            XCTAssertTrue(app.staticTexts.firstMatch.waitForExistence(timeout: 25))
            switch screen {
            case "detail":
                let row = app.buttons.matching(
                    NSPredicate(format: "identifier BEGINSWITH 'session-'")
                ).firstMatch
                XCTAssertTrue(row.waitForExistence(timeout: 25))
                row.tap()
                XCTAssertTrue(app.buttons["session-more"].waitForExistence(timeout: 20))
            case "link-health":
                let row = app.buttons.matching(
                    NSPredicate(format: "identifier BEGINSWITH 'session-'")
                ).firstMatch
                if row.waitForExistence(timeout: 25) { row.tap() }
                let more = app.buttons["session-more"].firstMatch
                if more.waitForExistence(timeout: 20) { more.tap() }
                let link = app.buttons["Link health"].firstMatch
                if link.waitForExistence(timeout: 10) { link.tap() }
            case "settings":
                let settings = app.buttons["Settings"].firstMatch
                if settings.waitForExistence(timeout: 20) { settings.tap() }
            default:
                break
            }
            var undersized: [String] = []
            for index in 0..<app.buttons.count {
                let element = app.buttons.element(boundBy: index)
                guard element.exists, element.isHittable else { continue }
                let frame = element.frame
                guard frame.width > 1, frame.height > 1 else { continue }
                // Half a point of tolerance. Without it this flagged three
                // controls reporting exactly 44.00 — the comparison, not the
                // layout, was wrong, and a guard that cries wolf is a guard
                // people delete.
                if frame.height < 44 - 0.5 {
                    undersized.append(
                        String(format: "%@ h=%.2f", element.label.prefix(34) as CVarArg,
                               frame.height))
                }
            }
            XCTAssertTrue(
                undersized.isEmpty,
                "\(screen): tappable controls under the 44pt floor — \(undersized)")
            app.terminate()
        }
    }

    /// At accessibility sizes these rows stack, so the tool is on its own line
    /// and a fixed width would be dead space. The column has to switch itself
    /// off, and nothing else about the row may move.
    func testTheColumnIsOffWhenTheRowsStack() {
        let app = launch("UICTContentSizeCategoryAccessibilityXXXL")
        XCTAssertTrue(app.staticTexts.firstMatch.waitForExistence(timeout: 25))
        var toolEdge: CGFloat?
        var commandEdge: CGFloat?
        for index in 0..<app.staticTexts.count {
            let element = app.staticTexts.element(boundBy: index)
            guard element.exists, element.frame.width > 0 else { continue }
            if element.label == "Bash", toolEdge == nil { toolEdge = element.frame.minX }
            if element.label.contains("git push"), commandEdge == nil {
                commandEdge = element.frame.minX
            }
        }
        guard let toolEdge, let commandEdge else {
            return XCTFail("expected a Bash row and its command at AX5")
        }
        XCTAssertEqual(
            toolEdge, commandEdge, accuracy: 0.5,
            "stacked, the tool and its command share the content column")
    }
}
