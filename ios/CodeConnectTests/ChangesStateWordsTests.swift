import XCTest

@testable import CodeConnectKit

/// A Changes panel's state line from the run's repository as the computer read it (`ChangesWords.state`): the words the
/// design's screens show (08, 09), and every fact the line can carry, from the computer's own `run_git` (minor 56).
final class ChangesStateWordsTests: XCTestCase {
    private func git(
        branch: String? = "fix/login", files: Int = 0, complete: Bool = true, ahead: Int? = 0, pushed: Bool = false,
        unmerged: Bool = false
    ) throws -> RunGit {
        let branchJSON = branch.map { #""branch":"\#($0)","# } ?? ""
        let aheadJSON = ahead.map { #","ahead":\#($0)"# } ?? ""
        let json = #"""
            {\#(branchJSON)"head_oid":"1234567","base":{"name":"main","remote":"origin","ref":"refs/remotes/origin/main"},
             "changed":{"files":\#(files),"added":3,"removed":1,"complete":\#(complete)}\#(aheadJSON),
             "pushed":\#(pushed),"unmerged":\#(unmerged),"read_at":"2026-11-26T12:00:00Z"}
            """#
        return try JSONDecoder().decode(RunGit.self, from: Data(json.utf8))
    }

    func testTheDesignsTwoStatesBeforeAPullRequest() throws {
        XCTAssertEqual(ChangesWords.state(try git(files: 1, ahead: 0)), "1 file changed · nothing committed", "08")
        XCTAssertEqual(ChangesWords.state(try git(files: 0, ahead: 1)), "1 commit · not pushed", "09")
    }

    func testEveryFactTheLineCarries() throws {
        XCTAssertEqual(
            ChangesWords.state(try git(files: 2, ahead: 3, pushed: true)), "2 files changed · 3 commits · pushed")
        XCTAssertEqual(
            ChangesWords.state(try git(files: 4, complete: false, ahead: 0)), "At least 4 files changed · nothing committed")
        XCTAssertEqual(ChangesWords.state(try git(files: 0, complete: false, ahead: 0)), "Changes not all counted")
        XCTAssertEqual(ChangesWords.state(try git(files: 1, ahead: nil)), "1 file changed", "no base: no commit count")
        XCTAssertEqual(
            ChangesWords.state(try git(files: 1, ahead: 0, unmerged: true)), "Merge conflicts · 1 file changed · nothing committed")
        XCTAssertEqual(ChangesWords.state(try git(branch: nil, files: 0, ahead: 2)), "2 commits", "a detached head pushes nothing")
        XCTAssertEqual(ChangesWords.state(try git(files: 0, ahead: 0)), "No changes")
        XCTAssertEqual(
            ChangesWords.state(try git(files: 0, ahead: nil)), "No uncommitted changes", "no base: no claim about commits")
    }

    func testAFolderWithNoCommitYetHasNothingCommitted() throws {
        let unborn = #"""
            {"branch":"main","changed":{"files":1,"added":2,"removed":0,"complete":true},
             "pushed":false,"unmerged":false,"read_at":"2026-11-26T12:00:00Z"}
            """#
        let git = try JSONDecoder().decode(RunGit.self, from: Data(unborn.utf8))
        XCTAssertEqual(ChangesWords.state(git), "1 file changed · nothing committed")
    }

    /// Why the computer did not commit, by its code: in the app's words, git's own words after where it gave them, and
    /// an outcome not known (or a code this build does not know) never claimed either way.
    func testACommitsRefusalIsWordedByItsCode() {
        XCTAssertEqual(
            ChangesWords.commitRefused(code: "nothing_to_commit", detail: nil), "Nothing has changed since the last commit.")
        XCTAssertEqual(
            ChangesWords.commitRefused(code: "git_failed", detail: "pre-commit hook: fmtcheck failed"),
            "Git did not commit. pre-commit hook: fmtcheck failed", "git's words, as it gave them")
        XCTAssertEqual(
            ChangesWords.commitRefused(code: "busy", detail: nil),
            "A run in this folder is working or waiting for you, so nothing was committed.", "any run in the folder")
        XCTAssertEqual(ChangesWords.commitRefused(code: "indeterminate", detail: nil), ChangesWords.commitUnknown)
        XCTAssertEqual(ChangesWords.commitRefused(code: "a_newer_code", detail: "x"), ChangesWords.commitUnknown)
    }
}
