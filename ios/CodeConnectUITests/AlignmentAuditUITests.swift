import XCTest

/// Proves the app has one text edge per surface, on the running app.
///
/// This exists because the defect it guards is a *claim about pixels*, and the
/// source cannot settle it: the edge a label lands on is the sum of a container's
/// padding and a component's own step. Reading that off the source is exactly how
/// a second text edge survived review — every part of it looked deliberate.
///
/// **Two type sizes, always.** The column the app aligns to is scaled, so a rule
/// that holds at M can break at AX5 and nowhere else; that has happened here
/// before, at a 5.00pt offset visible only at accessibility sizes.
final class AlignmentAuditUITests: XCTestCase {

    /// Half a point. The edges compared here are produced by the same layout pass
    /// and should be identical; the tolerance exists so a float that lands on
    /// 16.000001 is not a failure, and is far below anything an eye could see.
    private let tolerance: CGFloat = 0.5

    override func setUpWithError() throws {
        continueAfterFailure = true
    }

    private enum TypeSize: String {
        case medium = "UICTContentSizeCategoryM"
        case ax5 = "UICTContentSizeCategoryAccessibilityXXXL"
    }

    private func launch(
        fixture: String, size: TypeSize, deeplink: String? = nil
    ) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", fixture,
            "-CC_BIOMETRICS", "allow",
            "-UIPreferredContentSizeCategoryName", size.rawValue,
        ]
        if let deeplink {
            app.launchArguments += ["-CC_DEEPLINK", deeplink]
        }
        app.launch()
        XCTAssertTrue(app.wait(for: .runningForeground, timeout: 30))
        return app
    }

    /// The leading edge of every visible text run **in the content area**.
    ///
    /// `minY > 110` drops the navigation bar, which is not part of the layout
    /// being audited and whose centred title would otherwise be matched by name:
    /// the fleet's nav title and its eyebrow are both the word `Fleet`, and an
    /// early version of this test compared the band header against the title at
    /// x=181 and reported a defect that did not exist. `minX > 0` drops the
    /// full-width accessibility containers, which report an edge of 0 and are not
    /// text anybody sees.
    private func edges(_ app: XCUIApplication) -> [(x: CGFloat, label: String)] {
        var rows: [(CGFloat, String)] = []
        for element in app.staticTexts.allElementsBoundByIndex {
            guard element.exists else { continue }
            let f = element.frame
            guard f.width > 1, f.height > 1, f.minY > 110, f.minX > 0 else { continue }
            let label = element.label.replacingOccurrences(of: "\n", with: " ")
            guard !label.isEmpty else { continue }
            rows.append((f.minX, String(label.prefix(60))))
        }
        return rows
    }

    /// A section label must sit on the same edge as the prose it introduces.
    ///
    /// Stated as "the label and its neighbours", not "every text on screen",
    /// because a card's own content column is legitimately different from the
    /// screen's — what is never legitimate is a *label* floating away from the
    /// text it labels inside one container.
    private func assertLabelSharesBodyEdge(
        _ app: XCUIApplication, label: String, body: String,
        size: TypeSize, file: StaticString = #filePath, line: UInt = #line
    ) {
        let all = edges(app)
        guard let labelEdge = all.first(where: { $0.label.caseInsensitiveCompare(label) == .orderedSame })?.x
        else {
            XCTFail("[\(size.rawValue)] no element labelled \(label)", file: file, line: line)
            return
        }
        guard let bodyEdge = all.first(where: { $0.label.hasPrefix(body) })?.x else {
            XCTFail("[\(size.rawValue)] no body text starting \(body)", file: file, line: line)
            return
        }
        XCTAssertEqual(
            labelEdge, bodyEdge, accuracy: tolerance,
            """
            [\(size.rawValue)] "\(label)" sits at \(labelEdge) while the text it \
            labels sits at \(bodyEdge). A section label must share the edge of the \
            content it introduces — see D1 in internal/DESIGN-TRACKER.md.
            """,
            file: file, line: line)
    }

    // MARK: The decision card — where the defect was reported

    func testDecisionCardLabelsShareTheBodyEdgeAtMedium() {
        let app = launch(
            fixture: "deck", size: .medium, deeplink: "codeconnect://deck/toolu_fixture_medium")
        sleep(3)
        assertLabelSharesBodyEdge(
            app, label: "Exact command", body: "Classified at the Mac", size: .medium)
        assertLabelSharesBodyEdge(
            app, label: "If you deny", body: "Classified at the Mac", size: .medium)
    }

    func testDecisionCardLabelsShareTheBodyEdgeAtAX5() {
        let app = launch(
            fixture: "deck", size: .ax5, deeplink: "codeconnect://deck/toolu_fixture_medium")
        sleep(3)
        assertLabelSharesBodyEdge(
            app, label: "Exact command", body: "Classified at the Mac", size: .ax5)
    }

    // MARK: Fleet — the band header against the screen's own text

    func testFleetBandHeaderSharesTheScreenEdge() {
        let app = launch(fixture: "deck", size: .medium)
        sleep(3)
        let all = edges(app)
        guard let band = all.first(where: { $0.label.caseInsensitiveCompare("Blocked") == .orderedSame })?.x,
            let eyebrow = all.first(where: { $0.label == "Fleet" })?.x
        else {
            XCTFail("expected a Blocked band and a Fleet eyebrow on the fleet screen")
            return
        }
        XCTAssertEqual(
            band, eyebrow, accuracy: tolerance,
            """
            The BLOCKED band header sits at \(band) while the screen's own text \
            sits at \(eyebrow). Both are screen-level text and must share one edge.
            """)
    }

    /// The regression itself, at both type sizes.
    ///
    /// Compared against the card's own body text rather than against the smallest
    /// edge on screen. The fleet list behind the sheet legitimately has a second,
    /// deeper column for its dotted rows, so "every label equals the minimum" is
    /// not the rule and asserting it produced a failure against correct layout.
    /// The rule is the one that was broken: a label sits where its prose sits.
    func testNoSectionLabelIsStrandedOffItsProse() {
        for size in [TypeSize.medium, TypeSize.ax5] {
            let app = launch(
                fixture: "deck", size: size, deeplink: "codeconnect://deck/toolu_fixture_medium")
            sleep(3)
            for label in ["Exact command", "If you deny"] {
                assertLabelSharesBodyEdge(
                    app, label: label, body: "Classified at the Mac", size: size)
            }
            app.terminate()
        }
    }
}
