import XCTest

@testable import CodeConnectKit

/// An ended run's line (run state 13): "You ended this run" when the person ended it, with "its worktree and branch
/// are kept" when it worked in a worktree of its own, from the daemon's own facts (the end's `ended_by`, the
/// summary's `in_worktree`); "This run has ended" for any other end, and for every run from an older daemon.
final class RunEndedTests: XCTestCase {
    private func end(_ payload: String) throws -> Event {
        try JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":9,"session_uid":"u-1","session_id":"cc-1","ts":"2026-11-27T09:00:00.000Z",
                 "kind":"session_end","source":"daemon","payload":\(payload)}
                """.utf8))
    }

    private func summary(_ extra: String) throws -> SessionSummary {
        try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"u-1","session_id":"cc-1","cwd":"/web/.claude/worktrees/fix+login","lifecycle":"exited",\
                "link":"detached","last_seq":9,"created_at":"2026-11-27T09:00:00.000Z",\
                "updated_at":"2026-11-27T09:00:00.000Z"\(extra)}
                """.utf8))
    }

    func testTheLineSaysWhoEndedTheRunAndWhatIsKept() {
        XCTAssertEqual(
            SessionWords.ended(byPerson: true, inWorktree: true), "You ended this run · its worktree and branch are kept")
        XCTAssertEqual(SessionWords.ended(byPerson: true, inWorktree: false), "You ended this run")
        XCTAssertEqual(SessionWords.ended(byPerson: false, inWorktree: true), "This run has ended")
        XCTAssertEqual(SessionWords.ended(byPerson: false, inWorktree: false), "This run has ended")
    }

    func testAnEndTheDaemonMarksIsThePersons() throws {
        XCTAssertTrue(try end(#"{"exit_code":null,"reason":"Stopped","ended_by":"person"}"#).endedByPerson)
        XCTAssertFalse(try end(#"{"exit_code":null,"reason":"Stopped"}"#).endedByPerson, "an older daemon's")
        XCTAssertFalse(
            try end(#"{"exit_code":null,"reason":"Switched","switched_to":{"account_id":null,"label":"Default"}}"#)
                .endedByPerson, "a switch's end")
    }

    /// The conversation's own end says who ended the run and what is kept (screen check 2); any other end stays
    /// "Run ended".
    func testTheConversationsEndSaysWhoEndedTheRun() throws {
        func title(_ payload: String, keepsWorktree: Bool) throws -> String? {
            TimelineBuilder.build([try end(payload)], keepsWorktree: keepsWorktree).compactMap {
                if case .notice(let notice) = $0.content { notice.title } else { nil }
            }.first
        }
        let person = #"{"exit_code":null,"reason":"Stopped","ended_by":"person"}"#
        XCTAssertEqual(try title(person, keepsWorktree: true), "You ended this run · its worktree and branch are kept")
        XCTAssertEqual(try title(person, keepsWorktree: false), "You ended this run")
        XCTAssertEqual(try title(#"{"exit_code":null,"reason":"Stopped"}"#, keepsWorktree: true), "Run ended")
    }

    func testASummarySaysWhetherItsRunIsInAWorktreeOfItsOwn() throws {
        XCTAssertTrue(try summary(#","in_worktree":true"#).inWorktree)
        XCTAssertFalse(try summary("").inWorktree, "absent: false, as an older daemon sends it")
    }
}
