import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// Claude's approvals, against the bytes a minor-22 daemon sends.
///
/// `minor-22-wire.json` is frozen, as an earlier daemon emitted it; never
/// regenerated (`fixtures/README.md`). It is checked in at `fixtures/claude/`;
/// the copy in this bundle must stay byte-identical to it.
/// A card is answered only through the hook Claude holds for its own call, so
/// the phone offers an answer only while the daemon says it holds one.
@MainActor
final class HookOnlyApprovalTests: XCTestCase {

    private func wire() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-22-wire", withExtension: "json"),
            "minor-22-wire.json is not in the test bundle (checked in at "
                + "ios/CodeConnectTests/Resources/)")
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func event(_ name: String) throws -> Event {
        try XCTUnwrap(try wire()[name]?.decoded(Event.self), "\(name) decodes as an Event")
    }

    private func card(_ name: String) throws -> ApprovalCard {
        try XCTUnwrap(try event(name).approvalCard)
    }

    /// A held approval is answerable from a daemon that says it answers through
    /// the card's hook.
    func testAHeldApprovalIsAnswerable() throws {
        let held = try card("held_request")
        XCTAssertEqual(held.questionHold, .held)
        XCTAssertTrue(held.verification.hashMatchesDisplayText)
        XCTAssertEqual(
            DecisionCard.answerSurface(
                card: held, agent: .claude, hold: held.questionHold, hookOnlyApprovals: true),
            .allowDeny(allowForSession: false))
    }

    /// The phone grants no standing permission: "always" is chosen at the Mac,
    /// where Claude's own dialog says exactly what it saves. The phone has no
    /// such decision to send, and reads one only as unrecognised.
    func testThePhoneHasNoAllowAlways() throws {
        let decoded = try JSONDecoder().decode(
            AnswerDecision.self, from: Data(#"{"type":"allow_always"}"#.utf8))
        XCTAssertEqual(decoded, .unrecognised("allow_always"))
    }

    /// From a daemon that does not say so, the same card is read-only: such a
    /// daemon types an answer into whatever prompt is on screen. The send path
    /// refuses too, whatever the view drew.
    func testAnOlderDaemonsClaudeApprovalsAreReadOnly() throws {
        let held = try card("held_request")
        XCTAssertEqual(
            DecisionCard.answerSurface(
                card: held, agent: .claude, hold: .held, hookOnlyApprovals: false),
            .noneAnswerable)
        XCTAssertTrue(
            DecisionCard.claudeUnanswerableReason(
                isQuestion: false, hookOnlyApprovals: false, linkReason: nil
            ).contains("too old"))
        for decision in [AnswerDecision.allow, .deny, .option(index: 1), .text("no")] {
            XCTAssertNotNil(
                AppModel.decisionMismatch(
                    decision: decision, agent: .claude, answersApprovalsByHook: false),
                "\(decision) must not be sent to a daemon that types")
            XCTAssertNil(
                AppModel.decisionMismatch(
                    decision: decision, agent: .claude, answersApprovalsByHook: true),
                "\(decision) is a Claude approval's answer")
        }
        XCTAssertNotNil(
            AppModel.decisionMismatch(decision: .allow, agent: .codex),
            "a Codex card is answered by its own options")
    }

    /// A Mac whose link is down has said nothing about itself: its card gives
    /// the link's reason, never that the Mac is too old.
    func testAnApprovalFromAMacThatIsNotConnectedSaysTheLinkNotTheVersion() {
        let model = AppModel(pairing: PairingStore(ephemeral: true), cache: EventCache())
        model.connection.setPhaseForTesting(.waiting(until: Date(), reason: "down"))
        let link = model.actionsBlockedReason

        let reason = DecisionCard.claudeUnanswerableReason(
            isQuestion: false, hookOnlyApprovals: model.daemonProfile.capabilities?.hookOnlyApprovals,
            linkReason: link)

        XCTAssertNotNil(link)
        XCTAssertEqual(reason, link)
        XCTAssertFalse(reason.contains("too old"))
    }

    /// An earlier daemon's hold shapes this build no longer names still decode, and say only what is true: the hold
    /// change is an event kind this build does not name, and the hold `at_mac` is unrecognised.
    func testAnEarlierDaemonsHoldShapesStillDecode() throws {
        XCTAssertEqual(try event("hold_ended").kind, .other("question_hold"))
        XCTAssertEqual(try card("at_mac_request").questionHold, .unrecognised("at_mac"))
    }

    /// An earlier daemon recorded a phone's answer to an approval as sent to Claude, which Claude may not have
    /// used. Today it reads as what it is: an answer that was never confirmed.
    func testAnEarlierDaemonsSentAnswerReadsUnconfirmed() throws {
        let outcome = try XCTUnwrap(try event("answered_allow").approvalOutcome)
        XCTAssertTrue(outcome.indeterminate, "the premise: the record says it was never confirmed")
        XCTAssertEqual(DecisionCard.resultBanner(for: AnswerAttempt.classify(applied: outcome)).title, "Unconfirmed")
        XCTAssertEqual(ApprovalRow.resolutionText(for: outcome), "Answer not confirmed")
    }

    /// What the phone reads back: an allow from the phone, and a denial with a
    /// reason, which is `text`.
    func testAnAllowAndADenialWithAReasonReadBack() throws {
        let allowed = try XCTUnwrap(try event("answered_allow").approvalOutcome)
        XCTAssertEqual(allowed.decision, .allow)
        XCTAssertEqual(allowed.appliedVia, .hookReturn)

        let reasoned = try XCTUnwrap(try event("denied_with_reason").approvalOutcome)
        XCTAssertEqual(reasoned.decision, .text("use the other file"))
    }
}
