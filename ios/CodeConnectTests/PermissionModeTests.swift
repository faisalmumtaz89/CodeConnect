import XCTest

@testable import CodeConnect

/// A run that will never ask you anything has to say so.
///
/// The defect this covers is silence being indistinguishable from breakage. A
/// session started with `--dangerously-skip-permissions`, or on a Mac whose
/// settings allow everything, streams tools and turns and simply never raises a
/// card — which is correct, and which looks exactly like a dead link.
///
/// Frames are hand-written JSON through the app's real decoder rather than built
/// with the app's own encoder, for the reason `SessionIdentityTests` gives: a
/// round trip passes just as happily when both ends are wrong together. The
/// payload below is transcribed from a real `transcript_permission-mode` event
/// captured from a live bypassed session.
@MainActor
final class PermissionModeTests: XCTestCase {

    private func modeEvent(_ mode: String, seq: UInt64 = 1) throws -> Event {
        try JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":\(seq),"session_uid":"01KYZPH1RBF19BDYFBQ4AF3MPH","session_id":"cc-1",
                 "ts":"2026-08-02T01:18:46.000Z","kind":"transcript_permission-mode",
                 "payload":{"type":"permission-mode","permissionMode":"\(mode)",
                 "sessionId":"5377d7cb-b51f-41dd-a110-2617c0335206"},"source":"transcript"}
                """.utf8))
    }

    func testTheModeIsReadFromTheTranscriptLine() throws {
        XCTAssertEqual(try modeEvent("bypassPermissions").permissionModeChange, "bypassPermissions")
        XCTAssertEqual(try modeEvent("default").permissionModeChange, "default")
    }

    func testAnEventOfAnotherKindCarriesNoMode() throws {
        let tool = try JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":2,"session_uid":"01KYZPH1RBF19BDYFBQ4AF3MPH","session_id":"cc-1",
                 "ts":"2026-08-02T01:18:47.000Z","kind":"tool_call",
                 "payload":{"tool_name":"Bash"},"source":"hook"}
                """.utf8))
        XCTAssertNil(tool.permissionModeChange)
    }

    // MARK: The notice

    func testOnlyABypassedRunGetsTheNotice() throws {
        let state = SessionState(sessionKey: "01KYZPH1RBF19BDYFBQ4AF3MPH")
        state.ingest(try modeEvent("bypassPermissions"))
        XCTAssertNotNil(
            state.silentBecauseOfPermissions,
            "a run that will never ask must say so")
    }

    /// The half that matters more. A notice on a session that *can* still ask
    /// would be a lie told on every screen, and would train the reader to ignore
    /// the one place it is true.
    func testEveryModeThatCanStillAskStaysQuiet() throws {
        for mode in ["default", "acceptEdits", "plan"] {
            let state = SessionState(sessionKey: "01KYZPH1RBF19BDYFBQ4AF3MPH")
            state.ingest(try modeEvent(mode))
            XCTAssertNil(
                state.silentBecauseOfPermissions,
                "\(mode) can still raise a card, so it must not be announced as silent")
        }
    }

    /// "We have not been told yet" is not "nothing will be sent". A daemon or a
    /// transcript that never mentions the mode must produce no claim at all.
    func testAnUnknownModeMakesNoClaim() {
        let state = SessionState(sessionKey: "01KYZPH1RBF19BDYFBQ4AF3MPH")
        XCTAssertNil(state.permissionMode)
        XCTAssertNil(state.silentBecauseOfPermissions)
    }

    /// The same replay hazard `aiTitle` documents: "load all" re-delivers the
    /// whole log after the tail, so an older line must never displace a newer one.
    /// Without the guard, a session switched *out* of bypass would keep claiming
    /// it is silent the moment history was reloaded.
    func testAReplayedOlderLineCannotResurrectAStaleMode() throws {
        let state = SessionState(sessionKey: "01KYZPH1RBF19BDYFBQ4AF3MPH")
        state.ingest(try modeEvent("bypassPermissions", seq: 1))
        state.ingest(try modeEvent("default", seq: 2))
        XCTAssertNil(state.silentBecauseOfPermissions)

        state.ingest(try modeEvent("bypassPermissions", seq: 1))
        XCTAssertNil(
            state.silentBecauseOfPermissions,
            "a replayed seq 1 must not overwrite the mode established at seq 2")
    }
}
