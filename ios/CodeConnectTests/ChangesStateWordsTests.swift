import XCTest

@testable import CodeConnectKit

/// A Changes panel's state line from the run's repository as the computer read it (`ChangesWords.state`): the words the
/// design's screens show (08, 09), and every fact the line can carry, from the computer's own `run_git` (minor 56).
final class ChangesStateWordsTests: XCTestCase {
    private func git(
        branch: String? = "fix/login", files: Int = 0, complete: Bool = true, ahead: Int? = 0, pushed: Bool = false,
        unmerged: Bool = false, pr: String? = nil
    ) throws -> RunGit {
        let branchJSON = branch.map { #""branch":"\#($0)","# } ?? ""
        let aheadJSON = (ahead.map { #","ahead":\#($0)"# } ?? "") + (pr.map { #","pr":\#($0)"# } ?? "")
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
            ChangesWords.actionRefused(code: "nothing_to_commit", detail: nil, step: .commit), "Nothing has changed since the last commit.")
        XCTAssertEqual(
            ChangesWords.actionRefused(code: "git_failed", detail: "pre-commit hook: fmtcheck failed", step: .commit),
            "Git did not commit. pre-commit hook: fmtcheck failed", "git's words, as it gave them")
        XCTAssertEqual(
            ChangesWords.actionRefused(code: "busy", detail: nil, step: .commit),
            "A run in this folder is working or waiting for you, so nothing was committed.", "any run in the folder")
        XCTAssertEqual(ChangesWords.actionRefused(code: "indeterminate", detail: nil, step: .commit), ChangesWords.actionUnknown)
        XCTAssertEqual(ChangesWords.actionRefused(code: "a_newer_code", detail: "x", step: .commit), ChangesWords.actionUnknown)
    }

    /// A branch with a pull request (minor 60): the line names it first, by its number and what became of it, then any
    /// changes not yet committed; the commits and the push are the pull request's now.
    func testABranchsPullRequestLeadsTheLine() throws {
        func pr(_ state: String) -> String {
            #"{"number":12,"url":"https://github.com/acme/app/pull/12","state":"\#(state)","base":"main","head_oid":"1234567"}"#
        }
        XCTAssertEqual(ChangesWords.state(try git(files: 0, ahead: 2, pushed: true, pr: pr("open"))), "PR #12 · Open")
        XCTAssertEqual(
            ChangesWords.state(try git(files: 1, ahead: 2, pushed: true, pr: pr("open"))), "PR #12 · Open · 1 file changed")
        XCTAssertEqual(ChangesWords.state(try git(files: 0, ahead: 0, pr: pr("merged"))), "PR #12 · Merged into main")
        XCTAssertEqual(ChangesWords.state(try git(files: 0, ahead: 2, pushed: true, pr: pr("closed"))), "PR #12 · Closed")
        XCTAssertEqual(
            ChangesWords.state(try git(files: 0, ahead: 3, pushed: false, pr: pr("open"))), "PR #12 · Open · not pushed",
            "commits made since the PR was opened")
    }

    /// Before Create PR is pressed, the line names where the pull request goes: the base branch on the repository the
    /// computer will open it in (a fork's parent when it has one), so a pull request into another repository is never a
    /// surprise.
    func testCreatePRNamesItsRepositoryBeforeThePress() {
        XCTAssertEqual(
            ChangesWords.pullRequestTarget(repo: "acme/app", base: "main"),
            "Pushes the branch, then opens a PR into main on acme/app")
        XCTAssertEqual(
            ChangesWords.pullRequestTarget(repo: "acme/app", base: nil), "Pushes the branch, then opens a PR into acme/app")
    }

    /// Why the computer did not open a pull request, by its code, with gh's own words after where it gave them.
    func testAPullRequestsRefusalIsWordedByItsCode() {
        XCTAssertEqual(
            ChangesWords.actionRefused(code: "base_unknown", detail: nil, step: .pullRequest),
            "No base branch is known for this repository, so nothing was pushed.")
        XCTAssertEqual(
            ChangesWords.actionRefused(code: "gh_signed_out", detail: nil, step: .pullRequest),
            "This computer’s gh isn’t signed in to GitHub, so no PR was opened.")
        XCTAssertEqual(
            ChangesWords.actionRefused(code: "create_failed", detail: "pushed; a pull request already exists", step: .pullRequest),
            "GitHub did not open the PR. pushed; a pull request already exists")
        XCTAssertEqual(
            ChangesWords.actionRefused(code: "git_failed", detail: "! [rejected] fix/login (fetch first)", step: .pullRequest),
            "Git could not push the branch, so no PR was opened. ! [rejected] fix/login (fetch first)",
            "a refused push is not a refused commit")
    }
}
