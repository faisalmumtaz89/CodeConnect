import XCTest

@testable import CodeConnect

/// **Gap G9 — a message this build cannot read is never swallowed.**
///
/// Before this phase both inbound paths dropped it silently:
/// `DaemonConnection.handle` had `case .pong, .sessions, .event, .unknown: break`,
/// and an *undecodable* frame collapsed to one generic string with no type name
/// at all — `"Ignored an unreadable frame from the daemon"`.
///
/// During a phase that adds four message types that is the difference between a
/// five-minute fix and a day, and it is worse than a debugging problem: a reader
/// hunting a feature their Mac says it has gets no signal that this phone cannot
/// hear it. The connection is healthy and the daemon is working; the app is the
/// thing that is behind, and the only honest thing to say is so.
@MainActor
final class CodexLegibilityTests: XCTestCase {

    private func connected() -> DaemonConnection {
        let connection = DaemonConnection()
        connection.simulateConnectedForTesting()
        return connection
    }

    /// A frame type this build has never heard of names itself.
    func testAnUnknownFrameTypeIsNamed() {
        let connection = connected()
        XCTAssertNil(connection.unreadableFrame, "nothing to report yet")

        connection.injectForTesting(.unknown(type: "thread_forked"))

        let reported = try? XCTUnwrap(connection.unreadableFrame)
        XCTAssertTrue(
            reported?.contains("thread_forked") == true,
            "the type is the whole diagnosis: \(reported ?? "nil")")
        XCTAssertNotNil(connection.unreadableFrameAt)
    }

    /// And it is kept **apart from** the daemon's own complaints. An `error`
    /// frame is something the Mac said and may be quoted; this is the app's own
    /// account of its own limitation, and conflating the two is how a screen
    /// came to print the app's words under "the daemon's own reason, verbatim".
    func testItIsNotFiledAsSomethingTheDaemonSaid() {
        let connection = connected()
        connection.injectForTesting(.unknown(type: "thread_forked"))
        XCTAssertNil(
            connection.lastDaemonErrorMessage,
            "the daemon said nothing; this is the app's own account")
    }

    /// **Every frame this phase adds is readable**, which is the other half of
    /// the same guarantee: the row above must fire for a genuinely unknown type
    /// and never for one this build handles.
    func testTheFramesThisPhaseAddsAreNotUnreadable() {
        let connection = connected()
        connection.injectForTesting(
            .interruptResult(sessionID: "u-1", requestID: "s-1", result: .aborted(turnID: "t-1")))
        connection.injectForTesting(
            .composeResult(sessionID: "u-1", requestID: "c-1", result: .started(turnID: "t-1")))
        XCTAssertNil(connection.unreadableFrame)
    }

    /// The ordinary frames stay silent too — a report that fires on a `pong` is
    /// a report nobody reads twice.
    func testOrdinaryFramesReportNothing() {
        let connection = connected()
        connection.injectForTesting(.pong)
        connection.injectForTesting(.sessions([]))
        XCTAssertNil(connection.unreadableFrame)
    }
}

/// **The slash vocabulary belongs to Claude Code**, and applying it to a Codex
/// session is wrong in both directions: the app would open a Claude sheet for a
/// command that session has never heard of, and the injection behind it is a
/// `send_text` the Mac refuses by name.
///
/// Tested on the policy rather than the palette, because the policy is what
/// stands between every caller and the wire — the composer, a denial reason, a
/// diff comment — and the session's agent is what decides whether it applies at
/// all.
@MainActor
final class CodexSlashPolicyTests: XCTestCase {

    /// The policy itself is unchanged and still classifies Claude's built-ins.
    /// What this phase adds is the branch in front of it.
    func testTheClaudePolicyStillClassifiesItsOwnCommands() {
        guard case .nativeModel = ClaudeCommandPolicy.action(
            for: "/model", recoversComposer: true)
        else { return XCTFail("/model is still Claude's own control") }
        guard case .passThrough = ClaudeCommandPolicy.action(
            for: "hello", recoversComposer: true)
        else { return XCTFail("prose still passes through") }
    }

    /// A Codex session never reaches that policy: `AppModel.composeToCodex` is
    /// the whole send path, and it carries no slash routing at all. Proven on
    /// the wire, where it matters — a `/model` typed at a Codex session leaves
    /// as a `compose` carrying those exact characters, not as a `send_text` and
    /// not as a refusal.
    func testASlashOnACodexSessionIsJustAMessage() async throws {
        let model = AppModel(
            cache: EventCache(),
            // Its own defaults suite: the spent-material ledger is durable by
            // design, so a shared one carries one run's uncertainty into the
            // next one's first tap. See `CodexSpentLedger`.
            codexSpentLedger: CodexSpentLedger(
                defaults: UserDefaults(suiteName: "cc.tests.\(UUID().uuidString)")!))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(
                canApproveReliably: true, sendText: true,
                extra: ["codex_interrupt": .bool(true), "codex_compose": .bool(true)]),
            minor: 18)
        model.connection.injectForTesting(
            .sessions(CodexFixtures.frames(state: .composeStarted).compactMap { frame in
                if case .sessions(let list) = frame { return list }
                return nil
            }.first ?? []))

        var sent: [ClientMessage] = []
        let left = expectation(description: "one frame left")
        model.connection.sendStub = { message in
            if case .compose = message {
                sent.append(message)
                left.fulfill()
            }
        }
        Task {
            await model.composeToCodex(
                sessionKey: CodexFixtures.sessionKey, text: "/model opus")
        }
        await fulfillment(of: [left], timeout: 2)

        guard case .compose(_, _, let text, _) = sent.first else {
            return XCTFail("expected exactly one compose, got \(sent)")
        }
        XCTAssertEqual(
            text, "/model opus",
            "a leading slash is not a command here; it is the first character of a message")
    }
}
