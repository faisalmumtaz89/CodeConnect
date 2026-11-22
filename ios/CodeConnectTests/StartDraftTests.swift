import XCTest

@testable import CodeConnectKit

/// **One window's start, held above the form** (decision 303, N1): what is chosen and sent survives the form being
/// drawn again, a computer switched and switched back, and a withdrawn start is never sent twice for one ask.
@MainActor
final class StartDraftTests: XCTestCase {

    private func ask(_ computer: String = "a", message: String = "Fix the parser.", place: StartPlace = .folder)
        -> StartDraft.Ask
    {
        StartDraft.Ask(
            computerID: computer, folderPath: "/w/query-parser", place: place, agent: .claude, model: nil,
            message: message)
    }

    /// Sent and unanswered: the sending page; the start is not forgotten, whatever draws the form.
    func testASentStartShowsItsProgressUntilAnswered() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        XCTAssertEqual(draft.phase(outcome: nil), .sending)
        XCTAssertEqual(draft.sent?.requestID, "r1")
    }

    /// Withdrawn, then Start on the same ask: the same start again under its own id, never a second one.
    func testStartAfterWithdrawingTheSameAskResumesItsId() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        draft.withdraw()
        XCTAssertEqual(draft.phase(outcome: nil), .composing)
        XCTAssertEqual(draft.send(for: ask(message: "  Fix the parser.\n"), outcome: nil), .resume(requestID: "r1"))
        draft.didResume()
        XCTAssertEqual(draft.phase(outcome: nil), .sending)
    }

    /// Withdrawn, then the ask edited: a new start; the withdrawn one is let go.
    func testStartAfterAnEditIsANewStartAndLetsTheOldOneGo() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        draft.withdraw()
        XCTAssertEqual(draft.send(for: ask(message: "Fix the lexer."), outcome: nil), .new)
        XCTAssertEqual(draft.send(for: ask("b"), outcome: nil), .new)
        draft.didSend("r2", asking: ask(message: "Fix the lexer."))
        XCTAssertEqual(draft.sent?.requestID, "r2")
        XCTAssertEqual(draft.phase(outcome: nil), .sending)
    }

    /// Withdrawn, and it started after all while the draft still asks it: the run opens.
    func testAWithdrawnStartThatStartsAfterAllOpens() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        draft.withdraw()
        XCTAssertEqual(draft.phase(outcome: .started(sessionUID: "s1")), .started(sessionUID: "s1"))
        // Answered: Start makes a new start, never the answered one again.
        XCTAssertEqual(draft.send(for: ask(), outcome: .started(sessionUID: "s1")), .new)
    }

    /// Withdrawn, and answered with a start that never ran: the form, quietly.
    func testAWithdrawnStartAnsweredWithoutARunEndsQuietly() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        draft.withdraw()
        XCTAssertEqual(draft.phase(outcome: .needsTrust(path: "/w/query-parser")), .composing)
        XCTAssertEqual(draft.phase(outcome: .failed(.launchFailed, detail: "x")), .composing)
    }

    /// The folder question and a failure stay until the person answers them: leaving and coming back draws them again.
    func testTheTrustQuestionAndAFailureStayUntilAnswered() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        XCTAssertEqual(draft.phase(outcome: .needsTrust(path: "/w/q")), .trust(path: "/w/q"))
        XCTAssertEqual(draft.phase(outcome: .failed(.folderMissing, detail: "")), .failed(.folderMissing, detail: ""))
        draft.message = ""
        draft.compose(restoring: "Fix the parser.")
        XCTAssertEqual(draft.phase(outcome: nil), .composing)
        XCTAssertEqual(draft.message, "Fix the parser.")
    }

    /// Each computer keeps its own choices, the place in the repository included, across a switch and back.
    func testEachComputerKeepsItsChoicesAndPlaceAcrossASwitch() {
        let draft = StartDraft()
        draft.chooseFolder("/w/query-parser", on: "a")
        draft.edit(on: "a") { $0.place = .branch("feature/strict-mode") }
        draft.computerID = "b"
        draft.chooseFolder("/w/other", on: "b")
        draft.computerID = "a"
        XCTAssertEqual(draft.choices(on: "a").folderPath, "/w/query-parser")
        XCTAssertEqual(draft.choices(on: "a").place, .branch("feature/strict-mode"))
        XCTAssertEqual(draft.choices(on: "b").folderPath, "/w/other")
        // Only the person's own new folder resets the place.
        draft.chooseFolder("/w/web", on: "a")
        XCTAssertEqual(draft.choices(on: "a").place, .folder)
    }

    /// New Agent's keyboard request is kept until the form draws, and taken once.
    func testNewAgentsKeyboardRequestIsTakenOnce() {
        let draft = StartDraft()
        draft.requestKeyboard()
        XCTAssertTrue(draft.takeKeyboardRequest())
        XCTAssertFalse(draft.takeKeyboardRequest())
    }

    /// The started run opened: its message and place are spent, its folder stays chosen.
    func testOpeningTheStartedRunSpendsTheMessageAndPlace() {
        let draft = StartDraft()
        draft.chooseFolder("/w/query-parser", on: "a")
        draft.edit(on: "a") { $0.place = .new("fix-login") }
        draft.message = "Fix the parser."
        draft.didSend("r1", asking: ask(place: .new("fix-login")))
        draft.didOpen()
        XCTAssertEqual(draft.message, "")
        XCTAssertNil(draft.sent)
        XCTAssertEqual(draft.choices(on: "a").place, .folder)
        XCTAssertEqual(draft.choices(on: "a").folderPath, "/w/query-parser")
    }
}
