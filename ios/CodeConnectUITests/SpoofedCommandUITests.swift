import Vision
import XCTest

/// A command that reads as a different command is drawn and spoken as the one
/// that runs.
///
/// `spoofedCommand` holds one card whose command is `git push origin
/// <U+202E>niam ecrof--`. Drawn as it arrives, the override reverses the rest
/// of the line and the screen reads `git push origin --force main`. Every place
/// the card is shown — the fleet row, the Deck bar, the chat's approval row and
/// the decision card — draws the override as U+FFFD instead, so the screen
/// reads the bytes in the order they run, and speaks them with U+FFFD where the
/// override was.
///
/// **Drawn is read off the pixels**, as `WideToolLabelUITests` reads it: each
/// element's screenshot through text recognition.
final class SpoofedCommandUITests: XCTestCase {

    private let spoken = "git push origin \u{FFFD}niam ecrof--"

    override func setUpWithError() throws {
        continueAfterFailure = true
    }

    private func launch(deeplink: String? = nil) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "spoofedCommand", "-CC_BIOMETRICS", "allow",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryM",
        ]
        if let deeplink { app.launchArguments += ["-CC_DEEPLINK", deeplink] }
        app.launch()
        return app
    }

    /// The text recognised in `element`'s screenshot, letters and digits only,
    /// so spacing and punctuation recognition reads unreliably cannot decide.
    private func drawn(_ element: XCUIElement) -> String {
        guard let image = element.screenshot().image.cgImage else { return "" }
        let request = VNRecognizeTextRequest()
        request.recognitionLevel = .accurate
        request.usesLanguageCorrection = false
        try? VNImageRequestHandler(cgImage: image).perform([request])
        let text = (request.results ?? []).compactMap { $0.topCandidates(1).first?.string }
            .joined(separator: " ")
        return text.lowercased().filter { $0.isLetter || $0.isNumber }
    }

    /// Spoken with U+FFFD and without the override; drawn in the order the
    /// bytes run, never as `--force main`.
    private func assertShowsTheBytesThatRun(
        _ element: XCUIElement, spoken label: String, _ what: String,
        file: StaticString = #filePath, line: UInt = #line
    ) {
        XCTAssertTrue(label.contains(spoken), "\(what) speaks \(label.debugDescription)", file: file, line: line)
        XCTAssertFalse(label.contains("\u{202E}"), "\(what) speaks the override", file: file, line: line)
        let pixels = drawn(element)
        XCTAssertTrue(pixels.contains("niamecrof"), "\(what) draws \(pixels)", file: file, line: line)
        XCTAssertFalse(pixels.contains("forcemain"), "\(what) draws \(pixels)", file: file, line: line)
    }

    func testTheFleetRowAndTheDeckBarShowTheCommandThatRuns() {
        let app = launch()
        let row = app.buttons["session-fx-1"]
        XCTAssertTrue(row.waitForExistence(timeout: 25), "the blocked run's row")
        assertShowsTheBytesThatRun(row, spoken: row.label, "the fleet row")

        let bar = app.buttons["deck-bar"]
        XCTAssertTrue(bar.waitForExistence(timeout: 25), "the Deck bar")
        assertShowsTheBytesThatRun(bar, spoken: bar.value as? String ?? "", "the Deck bar")
    }

    func testTheChatRowShowsTheCommandThatRuns() {
        let app = launch(deeplink: "codeconnect://session/fx-1")
        let approval = app.descendants(matching: .any).matching(
            NSPredicate(format: "label BEGINSWITH 'Pending approval'")
        ).firstMatch
        XCTAssertTrue(approval.waitForExistence(timeout: 25), "the chat's approval row")
        sleep(1)
        assertShowsTheBytesThatRun(approval, spoken: approval.label, "the chat row")
    }

    /// The card's command block and its "Full tool input" both show the
    /// override as U+FFFD, and the description draws the words after its NUL.
    func testTheDecisionCardShowsTheCommandThatRuns() {
        let app = launch(deeplink: "codeconnect://deck/toolu_fixture_spoofed")
        let command = app.descendants(matching: .any).matching(
            NSPredicate(format: "label CONTAINS 'niam ecrof'")
        ).firstMatch
        XCTAssertTrue(command.waitForExistence(timeout: 25), "the card's command block")
        sleep(1)
        assertShowsTheBytesThatRun(command, spoken: command.label, "the card's command")
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = "decision-card"
        shot.lifetime = .keepAlways
        add(shot)

        // The card vouches for the bytes it hashed, never for the characters
        // drawn, some of which are U+FFFD.
        XCTAssertTrue(
            app.staticTexts["Verified against the daemon's hash."].exists,
            "the verification line claims the hash")

        let description = app.staticTexts.matching(
            NSPredicate(format: "label BEGINSWITH 'Force-push'")
        ).firstMatch
        XCTAssertTrue(description.exists, "the card's description")
        let words = drawn(description)
        XCTAssertTrue(words.contains("rebasedbranch"), "the description draws past its NUL: \(words)")

        let disclosure = app.buttons["Full tool input"]
        var swipes = 0
        while !disclosure.isHittable, swipes < 6 {
            app.swipeUp(velocity: .slow)
            swipes += 1
        }
        disclosure.tap()
        let input = app.descendants(matching: .any).matching(
            NSPredicate(format: "label CONTAINS '\"command\"'")
        ).firstMatch
        XCTAssertTrue(input.waitForExistence(timeout: 10), "the full tool input")
        XCTAssertTrue(input.label.contains(spoken), "the full tool input: \(input.label)")
        XCTAssertFalse(input.label.contains("\u{202E}"), "the full tool input speaks the override")
    }
}
