import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Decision 210 (Kit half): a run the next message relaunches opens its composer.** A run whose agent's
/// process died without anyone stopping it reads `exited`, but the daemon says the next message brings it
/// back — so its composer opens and the "Observe only" verdict falls away, while a run ended by End run stays
/// ended. Driven off the daemon-emitted `compose-stop-wire.json`. Codex's source for now is
/// `codexLink == .boundNotStarted`; machine's optional `relaunchable` field (protocol minor 41) joins the same
/// one read for Claude without a second code path.
@MainActor
final class RelaunchableRunTests: XCTestCase {

    private func wire() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "compose-stop-wire", withExtension: "json"),
            "compose-stop-wire.json is not in the test bundle (ios/CodeConnectTests/Resources/)")
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func summary(_ name: String) throws -> SessionSummary {
        try XCTUnwrap(try wire()[name]?.decoded(SessionSummary.self), name)
    }

    private func caps() throws -> Capabilities {
        try XCTUnwrap(try wire()["hello_ack_capabilities"]?.decoded(Capabilities.self))
    }

    // The agent-neutral fact: true only for an exited run the daemon will relaunch.
    func testTheSummaryStatesWhichRunsRelaunch() throws {
        XCTAssertTrue(try summary("session_summary_relaunchable").relaunchesOnMessage)
        XCTAssertFalse(
            try summary("session_summary_ended_by_end_run").relaunchesOnMessage, "End run stays ended")
        XCTAssertFalse(try summary("session_summary").relaunchesOnMessage, "a live run is not relaunchable")
    }

    func testARelaunchableRunOpensItsComposer() throws {
        let badge = FleetStatusRule.capability(
            summary: try summary("session_summary_relaunchable"), capabilities: try caps())
        XCTAssertTrue(badge.canAct, "the composer opens")
        XCTAssertNil(badge.reason, "no block reason is offered")
        XCTAssertNil(badge.bandNote, "the Observe only band note falls away")
    }

    func testARunEndedByEndRunStaysEnded() throws {
        let badge = FleetStatusRule.capability(
            summary: try summary("session_summary_ended_by_end_run"), capabilities: try caps())
        XCTAssertFalse(badge.canAct)
        XCTAssertEqual(badge.reason, "This session has ended.")
    }
}
