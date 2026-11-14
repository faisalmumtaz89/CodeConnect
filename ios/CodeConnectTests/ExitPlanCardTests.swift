import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// R2T1-F2: a plan-approval card shows ExitPlanMode's plan, not the escaped JSON the
/// daemon's display_text carries — and the card's "Full tool input" disclosure still
/// carries every field (planFilePath), so a later field beside the plan can never be
/// dropped silently into nowhere.
///
/// `exit-plan-card-wire.json` is daemon-emitted: ccd builds the card's `display_text`
/// and `payload_hash` from one ExitPlanMode `tool_input`, so it verifies against the
/// real render, not against itself. Regenerate with
/// `cargo test -p ccd --bin ccd -- --ignored regenerate_the_exit_plan_card_fixture`;
/// the committed card is gated by `the_exit_plan_card_fixture_is_what_this_build_emits`.
/// The {plan, planFilePath} field list is that input's (an app-qa t1 e6 capture).
@MainActor
final class ExitPlanCardTests: XCTestCase {

    private func card() throws -> ApprovalCard {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "exit-plan-card-wire", withExtension: "json"))
        let wire = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
        return try XCTUnwrap(wire["exit_plan"]?["payload"]?["card"]?.decoded(ApprovalCard.self))
    }

    /// The card's primary text is the plan itself, not "ExitPlanMode\n{escaped JSON}".
    func testThePlanCardShowsThePlanNotEscapedJSON() throws {
        let card = try card()
        let plan = try XCTUnwrap(card.toolInput.firstString("plan"))
        let primary = card.primaryText(verification: card.verification, agent: .claude)
        XCTAssertEqual(primary, plan, "the plan-approval card's primary text is the plan itself")
        XCTAssertFalse(primary.hasPrefix("ExitPlanMode\n"), "never the escaped-JSON display text")
        XCTAssertFalse(primary.contains("planFilePath"), "the primary text is the plan, not the input JSON")
    }

    /// The full-input disclosure still carries every field, so nothing being approved
    /// is dropped silently — if a later build adds a consequential field, it is here.
    func testTheFullInputDisclosureStillCarriesEveryField() throws {
        let card = try card()
        XCTAssertTrue(
            card.shownToolInput.contains("planFilePath"),
            "the Full tool input disclosure keeps planFilePath (and any field beside the plan)")
    }
}
