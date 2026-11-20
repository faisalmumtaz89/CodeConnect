import CodeConnectKit
import XCTest

/// A command cut to a preview's width keeps its start and its end, cut only
/// between tokens, and never hides the end.
final class CommandPreviewTests: XCTestCase {

    /// The command app-qa saw cut inside a word, `…sh --force origin main`.
    func testACutCommandKeepsWholeTokensAtEveryWidth() {
        let command = "git push --force origin main"
        XCTAssertEqual(commandPreview(command, columns: 12), "git … main")
        XCTAssertEqual(commandPreview(command, columns: 16), "git push … main")
        XCTAssertEqual(commandPreview(command, columns: 24), "git push … origin main")
        let tokens = Set(command.split(separator: " ").map(String.init) + ["…"])
        for columns in 8..<command.count {
            let preview = commandPreview(command, columns: columns)
            XCTAssertLessThanOrEqual(preview.count, columns, "at \(columns)")
            for token in preview.split(separator: " ") {
                XCTAssertTrue(tokens.contains(String(token)), "a part of a token at \(columns): \(preview)")
            }
        }
    }

    /// The danger is in the suffix, so the suffix is always on screen.
    func testTheEndOfACommandIsAlwaysShown() {
        let command = "git push origin feature && rm -rf ~/work"
        for columns in 8..<command.count {
            XCTAssertTrue(commandPreview(command, columns: columns).hasSuffix("~/work"), "at \(columns)")
        }
        XCTAssertEqual(commandPreview(command, columns: 30), "git push … && rm -rf ~/work")
    }

    /// With room for the end alone, the start goes.
    func testTheEndWinsWhenTheStartCannotFitBesideIt() {
        XCTAssertEqual(commandPreview("git push --force origin main", columns: 8), "… main")
    }

    /// One token too long to fit shows its own end.
    func testAOneTokenCommandShowsItsEnd() {
        XCTAssertEqual(commandPreview("./run-the-very-long-script.sh", columns: 12), "… -script.sh")
    }

    func testACommandThatFitsIsUnchanged() {
        XCTAssertEqual(commandPreview("ls -la", columns: 6), "ls -la")
        XCTAssertEqual(commandPreview("git  push", columns: 40), "git  push")
    }
}
