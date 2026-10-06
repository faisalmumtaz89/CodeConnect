import XCTest

@testable import CodeConnect

/// What an approval's ending says about who answered and where — only as far
/// as the record proves it. `resolved_by: phone` names a paired phone, not
/// this one; `local` is the Mac; an inferred ending is the prompt leaving the
/// Mac, with no answer seen.
@MainActor
final class ProvenanceWordingTests: XCTestCase {

    private func outcome(_ resolvedBy: String, inferred: Bool = false) throws -> AnswerOutcome {
        try JSONDecoder().decode(
            AnswerOutcome.self,
            from: Data(
                """
                {"request_id":"toolu_1","session_id":"cc-1","decision":{"type":"allow"},
                 "resolved_by":"\(resolvedBy)","applied_via":"send_keys",
                 "resolved_at":"2026-10-05T10:00:00.000Z","inferred":\(inferred)}
                """.utf8))
    }

    func testAPhoneAnswerDoesNotClaimThisPhone() throws {
        XCTAssertEqual(ApprovalRow.resolutionText(for: try outcome("phone")), "Allowed from a phone")
    }

    func testAnAnswerAtTheMacSaysTheMac() throws {
        XCTAssertEqual(ApprovalRow.resolutionText(for: try outcome("local")), "Allowed at the Mac")
    }

    /// The daemon saw the prompt leave, not an answer: closed, never answered.
    func testAnInferredEndingClaimsNoAnswer() throws {
        let inferred = try outcome("local", inferred: true)
        XCTAssertEqual(ApprovalRow.resolutionText(for: inferred), "Closed at the Mac")
        XCTAssertEqual(inferred.decisionLabel, "Closed")
        XCTAssertEqual(
            DecisionCardView.alreadyResolvedMessage(
                for: inferred, now: inferred.resolvedDate.addingTimeInterval(65)),
            "Closed at the Mac · 1m ago", "the place said once")
    }

    /// Codex's `resolved_by: phone` names a paired phone too.
    func testACodexPhoneAnswerDoesNotClaimThisPhone() {
        XCTAssertEqual(
            CodexProse.resolution(.answered(by: .phone, decision: nil)).title, "Answered from a phone")
    }

    func testTheCardsBannerUsesTheSameWords() throws {
        let phone = try outcome("phone")
        XCTAssertEqual(
            DecisionCardView.alreadyResolvedMessage(
                for: phone, now: phone.resolvedDate.addingTimeInterval(5)),
            "Allowed from a phone · 5s ago")
        XCTAssertEqual(
            ResolutionBanner.headline(for: .answeredAtKeyboard("x")), "Answered at the Mac")
    }
}
