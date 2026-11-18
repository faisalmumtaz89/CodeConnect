import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// What an approval's ending says about who answered and where — only as far
/// as the record proves it. `resolved_by: phone` names a paired phone, not
/// this one; an answerer this build does not name, such as an earlier daemon's
/// `local`, is said to be one; a `timeout` is a card nobody answered, with no
/// answer seen.
@MainActor
final class ProvenanceWordingTests: XCTestCase {

    private func outcome(_ resolvedBy: String) throws -> AnswerOutcome {
        try JSONDecoder().decode(
            AnswerOutcome.self,
            from: Data(
                """
                {"request_id":"toolu_1","session_id":"cc-1","decision":{"type":"allow"},
                 "resolved_by":"\(resolvedBy)","applied_via":"send_keys",
                 "resolved_at":"2026-10-05T10:00:00.000Z"}
                """.utf8))
    }

    func testAPhoneAnswerDoesNotClaimThisPhone() throws {
        XCTAssertEqual(ApprovalRow.resolutionText(for: try outcome("phone")), "Allowed from a phone")
    }

    /// A card nobody answered, closed for a reason this build does not name: closed, never answered, in the
    /// timeout words.
    func testATimeoutClaimsNoAnswer() throws {
        let closed = try outcome("timeout")
        XCTAssertEqual(ApprovalRow.resolutionText(for: closed), "Closed by timeout - nobody answered")
        XCTAssertEqual(closed.decisionLabel, "Closed")
        XCTAssertEqual(
            DecisionCard.alreadyResolvedBanner(
                for: closed, now: closed.resolvedDate.addingTimeInterval(65)).message,
            "Closed by timeout - nobody answered · 1m ago", "the reason said once")
    }

    /// No daemon names an answer superseded any more: an earlier one's record names an answerer this app does not
    /// recognise.
    func testAnEarlierDaemonsSupersededAnswerNamesNoAnswerer() throws {
        let superseded = try outcome("superseded")
        XCTAssertEqual(superseded.resolvedBy, .unknown)
        XCTAssertEqual(
            ApprovalRow.resolutionText(for: superseded), "Allowed by an answerer this app does not recognise")
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
