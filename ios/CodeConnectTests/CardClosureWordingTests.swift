import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// A hosted Claude run's card that nobody answered, closed because the run ended or an app's Stop ended its turn,
/// against the frames the daemon emits for it (`card-closed-unanswered-wire.json`, byte-identical to
/// `fixtures/claude/`). The daemon files it as a deny resolved by `timeout`; nobody denied it and nothing timed out,
/// so it reads "Closed" with its reason, and its tool row is not stamped denied.
@MainActor
final class CardClosureWordingTests: XCTestCase {

    private func closure(_ name: String) throws -> AnswerOutcome {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "card-closed-unanswered-wire", withExtension: "json"),
            "card-closed-unanswered-wire.json is not in the test bundle (checked in at ios/CodeConnectTests/Resources/)")
        let wire = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
        let frames = try XCTUnwrap(wire[name]?.arrayValue, "\(name) is a list of frames")
        let resolved = try XCTUnwrap(frames.last?.objectValue?["event"]?.decoded(Event.self), "\(name) ends in an event")
        return try XCTUnwrap(resolved.approvalOutcome, "\(name) ends in the card's resolution")
    }

    private func assertClosed(_ name: String, reading words: String) throws {
        let outcome = try closure(name)
        XCTAssertEqual(outcome.resolvedBy, .timeout, "the premise: the daemon files it as resolved by timeout")
        XCTAssertEqual(ApprovalRow.resolutionText(for: outcome), words)
        XCTAssertTrue(
            DecisionCard.alreadyResolvedBanner(for: outcome, now: outcome.resolvedDate).message.hasPrefix("\(words) · "))
        XCTAssertFalse(
            ApprovalRow.resolvedAccessibilityLabel(for: outcome, call: "Bash").contains("Denied"),
            "VoiceOver does not speak a denial nobody made")
        XCTAssertNil(
            TimelineBuilder.statusFromApproval(toolUseID: outcome.requestID, approvals: [outcome.requestID: outcome]),
            "its tool row is not stamped denied")
    }

    func testACardClosedWhenItsRunEndedSaysSo() throws {
        try assertClosed("closed_by_run_end", reading: "Closed when the session ended")
    }

    func testACardClosedWhenItsTurnWasStoppedSaysSo() throws {
        try assertClosed("closed_by_stop", reading: "Closed when the turn was stopped")
    }
}
