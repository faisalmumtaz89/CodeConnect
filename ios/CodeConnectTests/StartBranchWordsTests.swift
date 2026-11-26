import XCTest

@testable import CodeConnectKit

/// **Where a run starts in its repository says the same on both apps.** The iPhone's branch and new-worktree steps
/// wrote these words themselves; they are the Kit's now, for the Mac's start form too. Each is pinned as the
/// iPhone showed it.
final class StartBranchWordsTests: XCTestCase {

    func testTheBranchStepsWords() {
        XCTAssertEqual(StartWords.whichBranch, "Which branch?")
        XCTAssertEqual(StartWords.branch, "Branch")
        XCTAssertEqual(StartWords.readingBranches("query-parser"), "Reading query-parser’s branches…")
        XCTAssertEqual(StartWords.thisFolder, "This folder")
        XCTAssertEqual(StartWords.worktrees, "Worktrees")
        XCTAssertEqual(StartWords.newWorktree, "New worktree")
        XCTAssertEqual(StartWords.olderBranches, "Older branches aren’t listed.")
        XCTAssertEqual(StartWords.newBranch, "New branch…")
        XCTAssertEqual(StartWords.newBranchDetail, "A worktree of its own, by name")
    }

    /// The Mac's branch menu (screen 34), and its words for the branches a new worktree can start from.
    func testTheMacBranchMenusWords() {
        XCTAssertEqual(StartWords.ownCheckout, "The folder’s own checkout")
        XCTAssertEqual(StartWords.namedFromTask, "Branch named from the task")
        XCTAssertEqual(StartWords.fromABranch, "From a branch…")
        XCTAssertEqual(StartWords.newBranch.titleStyle, "New Branch…")
        XCTAssertEqual(StartWords.branchList([]), "")
        XCTAssertEqual(StartWords.branchList(["a"]), "a")
        XCTAssertEqual(StartWords.branchList(["a", "b"]), "a, b")
        XCTAssertEqual(
            StartWords.branchList(["accounts-reset-countdown", "fleet-tap-and-remove", "c", "d", "e"]),
            "accounts-reset-countdown, fleet-tap-and-remove, 3 more")
    }

    func testTheNewWorktreeStepsWords() {
        XCTAssertEqual(StartWords.worktreeName, "Name")
        XCTAssertEqual(StartWords.worktreeNameExample, "fix-login")
        XCTAssertEqual(StartWords.worktreeNameRule, "Letters, digits, . _ and -, with / between parts. Up to 64.")
        XCTAssertEqual(StartWords.worktreeNameBroken, "Letters, digits, . _ and - only, with / between parts.")
        XCTAssertEqual(StartWords.notNamedYet, "Not named yet")
        XCTAssertEqual(StartWords.from, "From")
        XCTAssertEqual(StartWords.changes, "Changes")
        XCTAssertEqual(StartWords.changesStay, "Uncommitted changes stay where they are")
        XCTAssertEqual(StartWords.nameTheWorktree, "Name the worktree.")
        XCTAssertEqual(StartWords.nameBreaksTheRule, "The name breaks the rule above.")
    }
}
