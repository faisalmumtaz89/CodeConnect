import XCTest

@testable import CodeConnectKit

/// Archive Session (minor 67): its request, its capability, an archived run's summary, and the card that asks first.
final class ArchiveTests: XCTestCase {
    private func summary(archived: Bool) throws -> SessionSummary {
        let url = try XCTUnwrap(Bundle(for: type(of: self)).url(forResource: "minor-46-wire", withExtension: "json"))
        let wire = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
        var frame = try XCTUnwrap(wire["session_hosted_live"]?.objectValue)
        frame["lifecycle"] = .string("exited")
        frame["claude_session_id"] = .string("4f1c2a7e-0000-4000-8000-000000000001")
        if archived { frame["archived"] = .bool(true) }
        return try JSONDecoder().decode(SessionSummary.self, from: Data(JSONValue.object(frame).canonicalJSONString.utf8))
    }

    /// An ended run offers its resume command; an archived one has no folder left, so none.
    func testAnArchivedRunHasNoResumeCommand() throws {
        let ended = try summary(archived: false)
        XCTAssertFalse(ended.archived)
        XCTAssertNotNil(ended.resumeCommand)
        let archived = try summary(archived: true)
        XCTAssertTrue(archived.archived)
        XCTAssertNil(archived.resumeCommand)
    }

    /// The action names the branch and the commit the panel showed, and is offered only where `git_actions` names it.
    func testArchiveCarriesTheBranchAndCommitShown() throws {
        let data = try JSONEncoder().encode(
            ClientMessage.archiveRun(requestID: "a-1", sessionUID: "u-1", branch: "worktree-x", headOID: "abc"))
        let fields = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: data).objectValue)
        XCTAssertEqual(fields["type"]?.stringValue, "git_action")
        XCTAssertEqual(fields["request_id"]?.stringValue, "a-1")
        XCTAssertEqual(fields["session_uid"]?.stringValue, "u-1")
        XCTAssertEqual(fields["action"]?.objectValue?["kind"]?.stringValue, "archive")
        XCTAssertEqual(fields["action"]?.objectValue?["branch"]?.stringValue, "worktree-x")
        XCTAssertEqual(fields["action"]?.objectValue?["head_oid"]?.stringValue, "abc")

        let offered = try JSONDecoder().decode(
            Capabilities.self, from: Data(#"{"git_actions":["commit","create_pr","merge","archive"]}"#.utf8))
        XCTAssertTrue(offered.archivesRuns)
        let older = try JSONDecoder().decode(
            Capabilities.self, from: Data(#"{"git_actions":["commit","create_pr","merge"]}"#.utf8))
        XCTAssertFalse(older.archivesRuns)
    }

    /// Archive's done carries neither a commit nor a pull request, and is read as done.
    func testAnArchiveDoneIsReadAsDone() throws {
        let done = try JSONDecoder().decode(GitActionResult.self, from: Data(#"{"status":"done"}"#.utf8))
        XCTAssertEqual(done, .done(sha: nil))
    }

    /// A refusal names Archive, not a commit or a pull request, and a partial one leads with how far it got.
    func testArchiveRefusalsSayArchive() {
        XCTAssertEqual(
            ChangesWords.actionRefused(code: "on_base_branch", detail: nil, step: .archive),
            "The branch checked out is the base branch itself, so nothing was archived.")
        XCTAssertEqual(
            ChangesWords.actionRefused(
                code: "git_failed", detail: "ended, worktree removed; feat kept: feat moved", step: .archive),
            "Git did not finish archiving: ended, worktree removed; feat kept: feat moved")
        XCTAssertEqual(
            ChangesWords.actionRefused(code: "locked", detail: "claude agent", step: .archive),
            "Git has this worktree locked, so nothing was archived. claude agent")
    }

    /// The card says what goes, and that files git ignores go too, before Archive Session is pressed.
    func testTheArchiveCardSaysIgnoredFilesGoToo() {
        let ask = FleetWords.archiveAsk("fix the tests", agent: "Codex")
        XCTAssertEqual(ask.title, "Archive “fix the tests”?")
        XCTAssertEqual(
            ask.items.map(\.words), ["Removes the worktree", "Deletes the local branch", "Ends the Codex session"])
        XCTAssertEqual(ask.items.map(\.mark), ["folder", "arrow.triangle.branch", "terminal"])
        XCTAssertEqual(ask.note, "Files git ignores in it, like .env, are deleted too.")
        XCTAssertEqual(ask.confirm, "Archive Session")
    }
}
