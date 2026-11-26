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
        XCTAssertEqual(draft.phase(outcome: nil, asking: ask(), computerKnown: true), .sending)
        XCTAssertEqual(draft.sent?.requestID, "r1")
    }

    /// Withdrawn, then Start on the same ask: the same start again under its own id, never a second one.
    func testStartAfterWithdrawingTheSameAskResumesItsId() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        draft.withdraw()
        XCTAssertEqual(draft.phase(outcome: nil, asking: ask(), computerKnown: true), .composing)
        XCTAssertEqual(draft.send(for: ask(message: "  Fix the parser.\n"), outcome: nil, computerKnown: true), .resume(requestID: "r1"))
        draft.didResume()
        XCTAssertEqual(draft.phase(outcome: nil, asking: ask(), computerKnown: true), .sending)
    }

    /// Withdrawn, then the ask edited: a new start; the withdrawn one is let go.
    func testStartAfterAnEditIsANewStartAndLetsTheOldOneGo() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        draft.withdraw()
        XCTAssertEqual(draft.send(for: ask(message: "Fix the lexer."), outcome: nil, computerKnown: true), .new)
        XCTAssertEqual(draft.send(for: ask("b"), outcome: nil, computerKnown: true), .new)
        draft.didSend("r2", asking: ask(message: "Fix the lexer."))
        XCTAssertEqual(draft.sent?.requestID, "r2")
        XCTAssertEqual(draft.phase(outcome: nil, asking: ask(), computerKnown: true), .sending)
    }

    /// Withdrawn, and it started after all while the draft still asks it: the run opens.
    func testAWithdrawnStartThatStartsAfterAllOpens() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        draft.withdraw()
        XCTAssertEqual(draft.phase(outcome: .started(sessionUID: "s1"), asking: ask(), computerKnown: true), .started(sessionUID: "s1"))
        // Answered: Start makes a new start, never the answered one again.
        XCTAssertEqual(draft.send(for: ask(), outcome: .started(sessionUID: "s1"), computerKnown: true), .new)
    }

    /// Withdrawn, and answered with a start that never ran: the form, quietly.
    func testAWithdrawnStartAnsweredWithoutARunEndsQuietly() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        draft.withdraw()
        XCTAssertEqual(draft.phase(outcome: .needsTrust(path: "/w/query-parser"), asking: ask(), computerKnown: true), .composing)
        XCTAssertEqual(draft.phase(outcome: .failed(.launchFailed, detail: "x"), asking: ask(), computerKnown: true), .composing)
    }

    /// The folder question and a failure stay until the person answers them: leaving and coming back draws them again.
    func testTheTrustQuestionAndAFailureStayUntilAnswered() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        XCTAssertEqual(draft.phase(outcome: .needsTrust(path: "/w/q"), asking: ask(), computerKnown: true), .trust(path: "/w/q"))
        XCTAssertEqual(draft.phase(outcome: .failed(.folderMissing, detail: ""), asking: ask(), computerKnown: true), .failed(.folderMissing, detail: ""))
        draft.message = ""
        draft.compose(restoring: "Fix the parser.")
        XCTAssertEqual(draft.phase(outcome: nil, asking: ask(), computerKnown: true), .composing)
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
        // Only the person's own new folder resets the place: to none chosen, which each app's form reads as its own
        // default.
        draft.chooseFolder("/w/web", on: "a")
        XCTAssertNil(draft.choices(on: "a").place)
        XCTAssertNil(StartDraft.Choices().place)
    }

    /// New Agent's keyboard request is kept until the form draws, and taken once.
    func testNewAgentsKeyboardRequestIsTakenOnce() {
        let draft = StartDraft()
        draft.requestKeyboard()
        XCTAssertTrue(draft.takeKeyboardRequest())
        XCTAssertFalse(draft.takeKeyboardRequest())
    }

    /// A New Agent on a form already on screen asks its computers again, at most once every 2 s: presses inside 2 s ask
    /// once, a press after them asks again.
    func testNewAgentOnAnOpenFormAsksAtMostEveryTwoSeconds() {
        let draft = StartDraft()
        let first = Date(timeIntervalSinceReferenceDate: 1000)
        XCTAssertTrue(draft.asksAgain(formShown: true, now: first))
        XCTAssertFalse(draft.asksAgain(formShown: true, now: first.addingTimeInterval(0.1)))
        XCTAssertFalse(draft.asksAgain(formShown: true, now: first.addingTimeInterval(1.9)))
        XCTAssertTrue(draft.asksAgain(formShown: true, now: first.addingTimeInterval(2)))
        XCTAssertFalse(draft.asksAgain(formShown: true, now: first.addingTimeInterval(3)))
    }

    /// A New Agent that draws the form does not ask again: the form's first appearance asked. A held ⌘N's repeats on
    /// the form it drew ask no more inside 2 s; a press after them asks.
    func testNewAgentThatDrawsTheFormAsksOnlyThroughItsAppearance() {
        let draft = StartDraft()
        let first = Date(timeIntervalSinceReferenceDate: 1000)
        XCTAssertFalse(draft.asksAgain(formShown: false, now: first))
        XCTAssertFalse(draft.asksAgain(formShown: true, now: first.addingTimeInterval(0.5)))
        XCTAssertFalse(draft.asksAgain(formShown: true, now: first.addingTimeInterval(1.9)))
        XCTAssertTrue(draft.asksAgain(formShown: true, now: first.addingTimeInterval(2)))
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
        XCTAssertNil(draft.choices(on: "a").place)
        XCTAssertEqual(draft.choices(on: "a").folderPath, "/w/query-parser")
    }

    /// Withdrawn, the message changed, and the old start reports it started: the form keeps what is typed; the run
    /// joins the list like any other.
    func testAWithdrawnStartIsLetGoOnceTheFormAsksSomethingElse() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        draft.withdraw()
        XCTAssertEqual(
            draft.phase(outcome: .started(sessionUID: "s1"), asking: ask(message: "Fix the lexer."), computerKnown: true),
            .composing)
        XCTAssertEqual(draft.phase(outcome: nil, asking: ask(message: "Fix the lexer."), computerKnown: true), .composing)
    }

    /// The sending computer removed: its start is let go, and Start makes a new one.
    func testAStartWhoseComputerIsRemovedIsLetGo() {
        let draft = StartDraft()
        draft.didSend("r1", asking: ask())
        XCTAssertEqual(draft.phase(outcome: nil, asking: ask(), computerKnown: false), .composing)
        draft.withdraw()
        XCTAssertEqual(draft.send(for: ask(), outcome: nil, computerKnown: false), .new)
    }

    /// The same folder picked again (an arrow at the list's end) keeps the place chosen in it.
    func testPickingTheSameFolderKeepsItsPlace() {
        let draft = StartDraft()
        draft.chooseFolder("/w/query-parser", on: "a")
        draft.edit(on: "a") { $0.place = .branch("feature/strict-mode") }
        draft.chooseFolder("/w/query-parser", on: "a")
        XCTAssertEqual(draft.choices(on: "a").place, .branch("feature/strict-mode"))
    }
}
