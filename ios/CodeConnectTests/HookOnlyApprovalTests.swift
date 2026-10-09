import XCTest

@testable import CodeConnect

/// Claude's approvals, against the bytes a minor-22 daemon sends.
///
/// `minor-22-wire.json` is emitted by ccd's own test
/// (`the_minor_22_wire_fixture_is_what_this_build_emits`) and checked in at
/// `fixtures/claude/`; the copy in this bundle must stay byte-identical to it.
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
            DecisionCardView.answerSurface(
                card: held, agent: .claude, hold: held.questionHold, hookOnlyApprovals: true),
            .allowDeny)
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
            DecisionCardView.answerSurface(
                card: held, agent: .claude, hold: .held, hookOnlyApprovals: false),
            .noneAnswerable)
        XCTAssertTrue(
            DecisionCardView.claudeUnanswerableReason(
                isQuestion: false, hookOnlyApprovals: false, hold: .held
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

    /// A background agent's approval asked while someone is at the Mac is shown
    /// there, and so is one whose hook ended: neither is answerable here.
    func testAnApprovalAtTheMacOrWhoseHoldEndedIsReadOnly() throws {
        let atMac = try card("at_mac_request")
        XCTAssertEqual(atMac.questionHold, .atMac)
        XCTAssertEqual(
            DecisionCardView.answerSurface(
                card: atMac, agent: .claude, hold: .atMac, hookOnlyApprovals: true),
            .noneAnswerable)
        XCTAssertEqual(
            DecisionCardView.claudeUnanswerableReason(
                isQuestion: false, hookOnlyApprovals: true, hold: .atMac),
            "Claude is asking this at the Mac, so answer it there.")

        let ended = try XCTUnwrap(try event("hold_ended").questionHoldChange)
        XCTAssertEqual(ended.hold, .ended)
        XCTAssertEqual(
            DecisionCardView.answerSurface(
                card: atMac, agent: .claude, hold: ended.hold, hookOnlyApprovals: true),
            .noneAnswerable)
    }

    /// A phone answer to an approval is recorded as sent to Claude, which is all
    /// that is known: Claude takes the first answer and may not use this one.
    /// Every place the phone shows it says so, and none says confirmed or
    /// unconfirmed.
    func testAPhoneAnswerReadsAsSentToClaude() throws {
        let outcome = try XCTUnwrap(try event("answered_allow").approvalOutcome)
        let attempt = AnswerAttempt.classify(applied: outcome)
        XCTAssertEqual(ResolutionBanner.classificationLabel(for: attempt), "Sent")
        XCTAssertEqual(
            ResolutionBanner.headline(for: attempt), "Allowed · Sent to Claude from a phone")
        XCTAssertEqual(DecisionCardView.alreadyResolvedTitle(for: outcome), "Sent")
        XCTAssertEqual(ApprovalRow.resolutionText(for: outcome), "Sent to Claude from a phone")
        let shown = [
            ResolutionBanner.classificationLabel(for: attempt),
            ResolutionBanner.headline(for: attempt),
            ResolutionBanner.provenance(for: attempt) ?? "",
            DecisionCardView.alreadyResolvedTitle(for: outcome),
            DecisionCardView.alreadyResolvedMessage(for: outcome, now: Date()),
            ApprovalRow.resolvedAccessibilityLabel(for: outcome, toolName: "Bash"),
        ].joined(separator: " | ").lowercased()
        XCTAssertFalse(shown.contains("confirm"), shown)
    }

    /// A tap on a card another phone already answered gets that phone's
    /// outcome back as a duplicate. Nothing from this tap was sent, so it reads
    /// as already answered, never as sent from here.
    func testATapOnACardAlreadySentToClaudeIsADuplicate() throws {
        let earlier = try XCTUnwrap(try event("answered_allow").approvalOutcome)
        let attempt = AnswerAttempt.classify(duplicate: earlier, staleHash: false)
        XCTAssertEqual(ResolutionBanner.classificationLabel(for: attempt), "Duplicate")
        XCTAssertEqual(ResolutionBanner.headline(for: attempt), "Already answered")
        XCTAssertEqual(
            ResolutionBanner.provenance(for: attempt),
            "Original outcome: Allowed from a phone, \(earlier.resolvedAt).")
    }

    /// When the Mac could not write the record, it says so after the same
    /// sentence, and the phone still shows the answer as sent.
    func testASentAnswerWhoseRecordFailedStillReadsAsSent() throws {
        let sent = try XCTUnwrap(try event("answered_allow").approvalOutcome)
        let unrecorded = AnswerOutcome(
            requestID: sent.requestID, sessionID: sent.sessionID, decision: sent.decision,
            resolvedBy: sent.resolvedBy, appliedVia: sent.appliedVia, resolvedAt: sent.resolvedAt,
            detail: (sent.detail ?? "")
                + " (sent, but the durable record could not be written; a retry will be refused "
                + "rather than sent again)",
            inferred: sent.inferred, indeterminate: sent.indeterminate)
        let attempt = AnswerAttempt.classify(applied: unrecorded)
        XCTAssertEqual(ResolutionBanner.classificationLabel(for: attempt), "Sent")
        XCTAssertEqual(DecisionCardView.alreadyResolvedTitle(for: unrecorded), "Sent")
        XCTAssertEqual(ApprovalRow.resolutionText(for: unrecorded), "Sent to Claude from a phone")
    }

    /// A card whose session ended before anyone answered it says so, and
    /// neither that it timed out nor that anyone answered it.
    func testACardClosedWhenItsSessionEndedSaysSo() throws {
        let closed = try XCTUnwrap(try event("closed_when_run_ended").approvalOutcome)
        XCTAssertEqual(closed.resolvedBy, .timeout)
        XCTAssertEqual(ApprovalRow.resolutionText(for: closed), "Closed when the session ended")
        XCTAssertTrue(
            DecisionCardView.alreadyResolvedMessage(for: closed, now: closed.resolvedDate)
                .hasPrefix("Closed when the session ended · "))
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
