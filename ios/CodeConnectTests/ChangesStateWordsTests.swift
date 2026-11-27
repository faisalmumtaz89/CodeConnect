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

    /// An open pull request's checks lead its line as artboard 07 draws them: a failure first, then what still runs;
    /// the line under it counts what passed and names what failed; and the Checks tab counts them. A closed or merged
    /// one's checks, and checks not read, change nothing.
    func testAnOpenPullRequestsChecksLeadItsLine() throws {
        func check(_ name: String, _ state: String) -> String { #"{"name":"\#(name)","state":"\#(state)"}"# }
        func pr(_ state: String = "open", checks: [String]?) -> String {
            let list = checks.map { #","checks":[\#($0.joined(separator: ","))]"# } ?? ""
            return #"{"number":412,"url":"https://github.com/acme/app/pull/412","state":"\#(state)","base":"main","#
                + #""head_oid":"1234567"\#(list)}"#
        }
        let passed = ["build-mac", "build-ios", "kit", "ccd", "lint", "control-names"].map { check($0, "pass") }
        let failing = try git(ahead: 1, pushed: true, pr: pr(checks: passed + [check("ui-tests", "fail")]))
        XCTAssertEqual(ChangesWords.state(failing), "PR #412 · 1 check failed", "07")
        XCTAssertEqual(ChangesWords.checksNote(failing), "6 of 7 checks passed · ui-tests failed", "07")
        XCTAssertEqual(ChangesWords.checksTab(failing.pullRequest!.checks!), "Checks 6/7", "07")
        let five = Array(passed.dropLast())
        let two = try git(ahead: 1, pushed: true, pr: pr(checks: five + [check("lint2", "fail"), check("ui", "fail")]))
        XCTAssertEqual(ChangesWords.state(two), "PR #412 · 2 checks failed")
        XCTAssertEqual(ChangesWords.checksNote(two), "5 of 7 checks passed · lint2, ui failed")
        let running = try git(
            ahead: 1, pushed: true, pr: pr(checks: five + [check("a", "pending"), check("b", "pending")]))
        XCTAssertEqual(ChangesWords.state(running), "PR #412 · 2 checks running")
        XCTAssertEqual(ChangesWords.checksNote(running), "5 of 7 checks passed · 2 running")
        let green = try git(ahead: 1, pushed: true, pr: pr(checks: passed + [check("skip", "skipped")]))
        XCTAssertEqual(ChangesWords.state(green), "PR #412 · Open", "all passed: nothing more to lead with")
        XCTAssertEqual(ChangesWords.checksNote(green), "7 of 7 checks passed", "a skipped check counts as passed")
        let unread = try git(ahead: 1, pushed: true, pr: pr(checks: nil))
        XCTAssertEqual(ChangesWords.state(unread), "PR #412 · Open")
        XCTAssertNil(ChangesWords.checksNote(unread), "no checks read: no line about them")
        let merged = try git(ahead: 0, pr: pr("merged", checks: passed + [check("ui-tests", "fail")]))
        XCTAssertEqual(ChangesWords.state(merged), "PR #412 · Merged into main")
        XCTAssertNil(ChangesWords.checksNote(merged), "a merged one's checks are not its state")
    }

    /// An open pull request's merge state leads its line once its checks have nothing to say (artboard 10): ready,
    /// queued (which leads before anything), behind or in conflict with its base, a draft, or held by its base's rules;
    /// and the merge button is offered only where GitHub would merge it.
    func testAnOpenPullRequestsMergeStateLeadsItsLine() throws {
        let passing = #"[{"name":"kit","state":"pass"}]"#
        func pr(_ mergeState: String, checks: String, queued: Bool) -> String {
            #"{"number":412,"url":"https://github.com/acme/app/pull/412","state":"open","base":"main","#
                + #""head_oid":"1234567","#
                + #""checks":\#(checks),"merge_state":"\#(mergeState)","merge_methods":["merge","squash"],"#
                + #""merge_queued":\#(queued)}"#
        }
        func line(_ state: String, checks: String? = nil, queued: Bool = false) throws
            -> (String, Bool)
        {
            let git = try git(ahead: 1, pushed: true, pr: pr(state, checks: checks ?? passing, queued: queued))
            return (ChangesWords.state(git), ChangesWords.mergeable(git.pullRequest!))
        }
        XCTAssertEqual(try line("clean").0, "PR #412 · Ready to merge", "10")
        XCTAssertTrue(try line("clean").1)
        XCTAssertEqual(try line("has_hooks").0, "PR #412 · Ready to merge")
        XCTAssertTrue(try line("has_hooks").1)
        XCTAssertEqual(try line("behind").0, "PR #412 · Behind main")
        XCTAssertFalse(try line("behind").1)
        XCTAssertEqual(try line("dirty").0, "PR #412 · Conflicts with main")
        XCTAssertFalse(try line("dirty").1)
        XCTAssertEqual(try line("draft").0, "PR #412 · Draft")
        XCTAssertFalse(try line("draft").1)
        XCTAssertEqual(try line("blocked").0, "PR #412 · Blocked by main’s rules")
        XCTAssertFalse(try line("blocked").1)
        XCTAssertEqual(try line("unknown").0, "PR #412 · Open", "GitHub still working it out says nothing more")
        XCTAssertFalse(try line("unknown").1)
        let failed = #"[{"name":"ui-tests","state":"fail"}]"#
        XCTAssertEqual(
            try line("blocked", checks: failed).0, "PR #412 · 1 check failed", "a failure leads, as 07 draws it")
        XCTAssertEqual(try line("unstable", checks: failed).0, "PR #412 · 1 check failed")
        XCTAssertTrue(try line("unstable", checks: failed).1, "a check GitHub doesn't require holds nothing back")
        XCTAssertEqual(try line("clean", queued: true).0, "PR #412 · Queued to merge")
        XCTAssertFalse(try line("clean", queued: true).1, "a queued one is not merged again")
    }

    /// The merge button names the method it uses, and the menu names each; one the repository turns off says so (10).
    func testTheMergeMethodsAreNamedAsTheMenuNamesThem() {
        XCTAssertEqual(ChangesWords.mergeMethod(.squash), "Squash and merge")
        XCTAssertEqual(ChangesWords.mergeMethod(.merge), "Create a merge commit")
        XCTAssertEqual(ChangesWords.mergeMethod(.rebase), "Rebase and merge")
        XCTAssertEqual(ChangesWords.mergeMethodOff(.rebase), "Rebase and merge (off in this repo)")
        XCTAssertEqual(ChangesWords.defaultMergeMethod([.merge, .squash]), .squash, "squash where it is allowed")
        XCTAssertEqual(ChangesWords.defaultMergeMethod([.merge, .rebase]), .merge, "else the first allowed")
        XCTAssertNil(ChangesWords.defaultMergeMethod([]))
    }

    /// A merge the computer did not make is worded as a merge's, never as a commit's or a push's.
    func testAMergesRefusalIsWordedByItsCode() {
        func said(_ code: String, _ detail: String? = nil) -> String {
            ChangesWords.actionRefused(code: code, detail: detail, step: .merge)
        }
        XCTAssertEqual(
            said("head_moved"),
            "The pull request or its commit changed since the panel showed it, so nothing was merged.")
        XCTAssertEqual(
            said("method_not_allowed"), "This repository doesn’t allow that merge method, so nothing was merged.")
        XCTAssertEqual(said("no_pull_request"), "This branch has no open pull request, so nothing was merged.")
        XCTAssertEqual(
            said("merge_failed", "Required status check \"ci\" is expected."),
            "GitHub did not merge it. Required status check \"ci\" is expected.")
        XCTAssertEqual(said("busy"), "A run in this folder is working or waiting for you, so nothing was merged.")
        XCTAssertEqual(said("timeout"), "GitHub took too long; the pull request may or may not have been merged.")
        XCTAssertEqual(said("gh_missing"), "This computer has no gh, so nothing was merged.")
        XCTAssertEqual(said("gh_signed_out"), "This computer’s gh isn’t signed in to GitHub, so nothing was merged.")
        XCTAssertEqual(said("github_unavailable"), "GitHub could not be reached, so nothing was merged.")
    }

    /// A check's duration as artboard 07 writes it: seconds alone under a minute, minutes and two-digit seconds under
    /// an hour, then hours and two-digit minutes.
    func testACheckDurationIsWrittenAsTheDesignWritesIt() {
        XCTAssertEqual(ChangesWords.checkDuration(22), "22s")
        XCTAssertEqual(ChangesWords.checkDuration(252), "4m 12s")
        XCTAssertEqual(ChangesWords.checkDuration(303), "5m 03s")
        XCTAssertEqual(ChangesWords.checkDuration(3720), "1h 02m")
        XCTAssertEqual(ChangesWords.checkDuration(0), "0s")
        XCTAssertEqual(ChangesWords.checkDuration(-5), "0s", "a clock behind its start says nothing negative")
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
