import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// What an approval's ending says about who answered and where — only as far
/// as the record proves it. `resolved_by: phone` names a paired phone, not
/// this one; an answerer this build does not name, such as an earlier daemon's
/// `local`, is said to be one; an inferred ending is the prompt leaving, with no
/// answer seen.
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

    /// The daemon saw the prompt leave, not an answer: closed, never answered.
    func testAnInferredEndingClaimsNoAnswer() throws {
        let inferred = try outcome("local", inferred: true)
        XCTAssertEqual(
            ApprovalRow.resolutionText(for: inferred), "Closed by an answerer this app does not recognise")
        XCTAssertEqual(inferred.decisionLabel, "Closed")
        XCTAssertEqual(
            DecisionCard.alreadyResolvedBanner(
                for: inferred, now: inferred.resolvedDate.addingTimeInterval(65)).message,
            "Closed by an answerer this app does not recognise · 1m ago", "the answerer said once")
    }

    /// Codex's `resolved_by: phone` names a paired phone too.
    func testACodexPhoneAnswerDoesNotClaimThisPhone() {
        XCTAssertEqual(
            CodexProse.resolution(.answered(by: .phone, decision: nil, answeredOn: nil)).title, "Answered from a phone")
    }

    func testTheCardsBannerUsesTheSameWords() throws {
        let phone = try outcome("phone")
        XCTAssertEqual(
            DecisionCard.alreadyResolvedBanner(
                for: phone, now: phone.resolvedDate.addingTimeInterval(5)).message,
            "Allowed from a phone · 5s ago")
        XCTAssertEqual(
            DecisionCard.resultBanner(for: .answeredAtKeyboard("x")).message, "Answered at the computer")
    }
}
