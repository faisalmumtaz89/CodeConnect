import XCTest

/// The commands beside tool names share one left edge.
///
/// Measured before this was enforced, on a timeline holding `Bash`, `Read` and
/// `MultiEdit`: the commands began at **98.00, 99.00 and 124.33** — a 26pt
/// ragged edge down a vertical list. On the fleet, with `Edit`/`Bash`/`Read`, it
/// was 83.67/90.00/90.67.
///
/// Asserted rather than left to a render, because it is invisible in review: the
/// code reads perfectly well as `Text(tool) ; Text(command)` and the defect only
/// exists once two rows with different-width tool names sit above one another.
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
        let list = app.scrollViews.firstMatch
        guard list.exists else { return [] }
        let bounds = list.frame
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

    func testTimelineCommandsShareOneLeftEdge() {
        let app = launch("UICTContentSizeCategoryL")
        let row = app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'app-5'")).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 25), "the running session row")
        row.tap()
        XCTAssertTrue(app.buttons["open-diff"].waitForExistence(timeout: 20), "session detail")

        // `Bash`, `Read` and `MultiEdit` — deliberately different widths, which
        // is the whole reason the fixture carries all three.
        let origins = commandOrigins(
            app, containing: ["echo soak", "README.md", "Router.swift"])
        XCTAssertGreaterThanOrEqual(
            origins.count, 3, "the fixture must reach three tool rows at once")
        let spread = (origins.max() ?? 0) - (origins.min() ?? 0)
        XCTAssertEqual(
            spread, 0, accuracy: 0.5,
            "commands must start on one edge; measured origins \(origins)")
    }

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
                XCTAssertTrue(app.buttons["open-diff"].waitForExistence(timeout: 20))
            case "link-health":
                let pill = app.buttons["Link health"].firstMatch
                if pill.waitForExistence(timeout: 20) { pill.tap() }
            case "settings":
                let gear = app.buttons["Settings and pairing"].firstMatch
                if gear.waitForExistence(timeout: 20) { gear.tap() }
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
