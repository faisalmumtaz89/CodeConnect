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

    /// **A Claude run its own stop left relaunchable opens its composer too** (minor 42). The daemon's
    /// `relaunchable` field answers for both agents, so a hosted Claude run whose claude exited on its
    /// own — with no `codexLink` to lean on — reads relaunchable just as a Codex run does. From the
    /// daemon's own `minor-37-wire.json`. Must-fail: today's Codex-only read says a Claude run never
    /// relaunches.
    func testAClaudeRunTheDaemonMarksRelaunchableOpensItsComposer() throws {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-37-wire", withExtension: "json"),
            "minor-37-wire.json is not in the test bundle (ios/CodeConnectTests/Resources/)")
        let claudeWire = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
        let summary = try XCTUnwrap(try claudeWire["session_hosted_ended"]?.decoded(SessionSummary.self))
        let caps = try XCTUnwrap(try claudeWire["hello_ack_capabilities"]?.decoded(Capabilities.self))
        XCTAssertFalse(summary.isCodex, "it is a Claude run")
        XCTAssertEqual(summary.codexLink, .none, "with no Codex link to lean on")
        XCTAssertTrue(
            summary.relaunchesOnMessage,
            "the daemon's relaunchable field opens the composer for Claude too")
        let badge = FleetStatusRule.capability(summary: summary, capabilities: caps)
        XCTAssertTrue(badge.canAct, "the composer opens")
        XCTAssertNil(badge.bandNote, "the Observe only band note falls away")
        // A self-stopped run carries no menus, so its sheet draws no effort or mode line — its effort
        // reads unset, never the value it held before the stop.
        XCTAssertEqual(
            RunSettings.of(summary: summary), .hostedReadOnly(model: nil),
            "the self-stopped run's effort reads unset, not the old value")
    }

    /// **A run the next message relaunches offers no manual resume command.** A relaunchable ended
    /// run is picked up by sending it a message, so `cd … && claude --resume` beside its open composer
    /// would be a second, worse path. The command stays for an ended run the daemon does NOT relaunch.
    /// Repro from the daemon's own `minor-37-wire.json` (`session_hosted_ended`: exited, relaunchable, with
    /// a recorded id). Must-fail: without the relaunchable guard the relaunchable run still offers its id.
    func testARelaunchableRunOffersNoManualResumeCommand() throws {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-37-wire", withExtension: "json"),
            "minor-37-wire.json is not in the test bundle (ios/CodeConnectTests/Resources/)")
        let claudeWire = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
        let relaunchable = try XCTUnwrap(
            try claudeWire["session_hosted_ended"]?.decoded(SessionSummary.self))
        XCTAssertTrue(relaunchable.relaunchesOnMessage, "precondition: the fixture is relaunchable")
        XCTAssertFalse(
            (relaunchable.claudeSessionID ?? "").isEmpty, "precondition: it has a recorded session id")
        XCTAssertNil(
            relaunchable.resumeSessionID,
            "a relaunchable run offers no manual resume command — it is relaunched by a message")

        // The contrast: an ended Claude run the daemon does NOT relaunch still offers the command.
        let ended = try JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"u-ended","session_id":"u-ended","tmux_session":"",
                 "cwd":"/work","project_label":"work","lifecycle":"exited","link":"attached",
                 "last_seq":1,"created_at":"2026-09-04T21:31:59.000Z",
                 "updated_at":"2026-09-04T21:31:59.000Z","blocked_on":[],"agent":"claude",
                 "codex_link":"none","claude_session_id":"abc123"}
                """.utf8))
        XCTAssertFalse(ended.relaunchesOnMessage, "precondition: not relaunchable")
        XCTAssertEqual(
            ended.resumeSessionID, "abc123",
            "a non-relaunchable ended run still offers its resume command")
    }
}
