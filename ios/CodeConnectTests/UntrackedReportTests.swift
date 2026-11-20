import XCTest

@testable import CodeConnectKit

/// The untracked files ccd appends after a diff, read in its own words
/// (`mac/ccd/src/git.rs`): `# <n> untracked file(s), not shown as a diff:`, then
/// `#   <path>` per file.
final class UntrackedReportTests: XCTestCase {

    private func shown(_ lines: [String]) -> [String] {
        UnifiedDiff.reported(lines).map { row in row.label.map { "\(row.text) [\($0)]" } ?? row.text }
    }

    func testOneUntrackedFileIsItsPathAndTheWord() {
        XCTAssertEqual(
            shown(["# 1 untracked file(s), not shown as a diff:", "#   notes.txt"]),
            ["notes.txt [untracked]"])
    }

    func testSeveralUntrackedFilesEachReadAsAFile() {
        XCTAssertEqual(
            shown([
                "# 3 untracked file(s), not shown as a diff:", "#   notes.txt", "#   docs/plan.md",
                "#   a b.txt",
            ]),
            ["notes.txt [untracked]", "docs/plan.md [untracked]", "a b.txt [untracked]"])
    }

    /// A diff cut short says so after the names, and that line stays as it is.
    func testTheLineAfterTheNamesIsNotAName() {
        XCTAssertEqual(
            shown([
                "# 1 untracked file(s), not shown as a diff:", "#   notes.txt", "… diff truncated by CodeConnect",
            ]),
            ["notes.txt [untracked]", "… diff truncated by CodeConnect"])
    }

    /// With no untracked files ccd writes no header, and every line stays as it came.
    func testWithoutTheHeaderNothingIsAName() {
        XCTAssertEqual(shown(["#   notes.txt", "?? notes.txt"]), ["#   notes.txt", "?? notes.txt"])
        XCTAssertEqual(shown([]), [])
    }

    /// The fixture diff ends as ccd ends one, and its untracked file reads as one.
    func testTheFixtureDiffEndsInCcdsWords() throws {
        let diff = try XCTUnwrap(Fixtures.diff())
        let parsed = UnifiedDiff.parse(diff.unified)
        let rows = UnifiedDiff.reported(parsed.trailing.filter { !$0.trimmingCharacters(in: .whitespaces).isEmpty })
        XCTAssertEqual(rows.map(\.text), ["Sources/Untracked.swift"])
        XCTAssertEqual(rows.map(\.label), ["untracked"])
    }
}
