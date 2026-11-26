import XCTest

@testable import CodeConnectKit

/// **A new worktree named from the task** (screen 34's default, "Branch named from the task"): the first message's
/// words make a name Claude Code's rule accepts, and never one the repository already has.
final class StartWorktreeNameTests: XCTestCase {
    private let empty = StartGit(worktrees: [], branches: [], branchesTruncated: false)

    private func name(_ task: String, _ agent: StartAgent = .claude, in git: StartGit? = nil) -> String {
        StartPlace.named(fromTask: task, for: agent, in: git ?? empty)
    }

    func testTheWordsOfTheTaskInLowerCaseJoinedByDashes() {
        XCTAssertEqual(name("Add a --strict flag that rejects unknown operators"), "add-a-strict-flag-that-rejects-unknown")
        XCTAssertEqual(name("Fix the parser."), "fix-the-parser")
        XCTAssertEqual(name("  Retry the router on 5xx\nand log it "), "retry-the-router-on-5xx-and-log-it")
        XCTAssertEqual(name("v1.2_beta release"), "v1-2-beta-release")
    }

    /// At most 40 bytes, cut between words, never inside one; a first word longer than that is cut to it.
    func testANameIsAtMost40BytesCutBetweenWords() {
        let long = name("Show the reset time next to each usage ring in the reply bar")
        XCTAssertEqual(long, "show-the-reset-time-next-to-each-usage")
        XCTAssertLessThanOrEqual(long.utf8.count, 40)
        XCTAssertEqual(name(String(repeating: "a", count: 50) + " tail"), String(repeating: "a", count: 40))
        XCTAssertEqual(name("abcdefghij abcdefghij abcdefghij abcdefgh"), "abcdefghij-abcdefghij-abcdefghij")
        XCTAssertEqual(name("abcdefghij abcdefghij abcdefghij abcdefg"), "abcdefghij-abcdefghij-abcdefghij-abcdefg")
    }

    /// Accents are dropped from letters that have them; words with no Latin letter or digit, emoji among them, are
    /// left out; a task with none left is named "run".
    func testUnicodeEmojiAndEmptyTasks() {
        XCTAssertEqual(name("Café résumé naïve"), "cafe-resume-naive")
        XCTAssertEqual(name("🚀 Ship it 🎉"), "ship-it")
        XCTAssertEqual(name("🚀🎉"), "run")
        XCTAssertEqual(name("修复登录"), "run")
        XCTAssertEqual(name(""), "run")
        XCTAssertEqual(name("   \n "), "run")
        XCTAssertEqual(name("--- ..."), "run")
    }

    /// Every name made passes the rule the computer checks.
    func testEveryNameMadePassesTheRule() {
        for task in ["Add a --strict flag", "🚀", "", "Café", String(repeating: "x", count: 300), "a/b\\c:d*e?f", "..", "-.-"] {
            XCTAssertTrue(StartPlace.isValidName(name(task)), task)
        }
    }

    /// A name typed under "New Branch…" that the repository has is shown as the computer will make it: the next free
    /// one, by the same rule (its folder writes each "/" as "+", as Claude Code's branch does).
    func testATypedNameThatIsTakenIsTheNextFreeOne() {
        let git = StartGit(
            worktrees: [
                StartWorktree(path: "/w/app", branch: "main", dirty: false, missing: false),
                StartWorktree(path: "/w/app/.claude/worktrees/fix+ui", branch: "other", dirty: false, missing: false),
            ],
            branches: ["worktree-fix+login", "fix/login"], branchesTruncated: false)
        XCTAssertEqual(StartPlace.free("fix/login", for: .claude, in: git), "fix/login-2")
        XCTAssertEqual(StartPlace.free("fix/login", for: .codex, in: git), "fix/login-2")
        XCTAssertEqual(StartPlace.free("fix/ui", for: .codex, in: git), "fix/ui-2")
        XCTAssertEqual(StartPlace.free("fix/new", for: .claude, in: git), "fix/new")
    }

    /// A name the repository has (a branch the new one would be, or a worktree's folder) gets "-2", then "-3".
    func testATakenNameGetsTheNextNumber() {
        let claude = StartGit(
            worktrees: [
                StartWorktree(path: "/w/app", branch: "main", dirty: false, missing: false),
                StartWorktree(path: "/w/app/.claude/worktrees/fix-the-parser-2", branch: "elsewhere", dirty: false, missing: false),
            ],
            branches: ["worktree-fix-the-parser"], branchesTruncated: false)
        // Claude Code's branch is worktree-<name>: the first is taken as a branch, the second as a folder.
        XCTAssertEqual(name("Fix the parser", .claude, in: claude), "fix-the-parser-3")
        // Codex's branch is the name itself, which no branch is here; the -2 folder doesn't stand in the way of -1.
        XCTAssertEqual(name("Fix the parser", .codex, in: claude), "fix-the-parser")
        let codex = StartGit(
            worktrees: [StartWorktree(path: "/w/app", branch: "fix-the-parser", dirty: false, missing: false)],
            branches: ["fix-the-parser-2"], branchesTruncated: false)
        XCTAssertEqual(name("Fix the parser", .codex, in: codex), "fix-the-parser-3")
        XCTAssertEqual(name("Fix the parser", .antigravity, in: codex), "fix-the-parser-3")
        XCTAssertEqual(name("Fix the parser", .claude, in: codex), "fix-the-parser")
        XCTAssertEqual(
            name("🚀", .codex, in: StartGit(worktrees: [], branches: ["run"], branchesTruncated: false)), "run-2")
    }
}
