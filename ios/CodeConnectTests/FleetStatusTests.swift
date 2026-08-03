import XCTest

@testable import CodeConnect

/// The first tests `FleetStatusRule.status` has ever had.
///
/// That absence is the story. The rule decides every row's band, its sort rank and
/// the screen's headline, and nothing in either target executed it, so a predicate
/// that called healthy sessions "Failed" shipped and no test could have caught it.
///
/// **The rule they enforce**, from `docs/ARCHITECTURE.md`: the observation plane,
/// transcripts and pane snapshots, is "never used to decide anything". A tool's
/// `is_error` is transcript. The session's own exit is control plane. Status may be
/// derived from the second and never from the first.
@MainActor
final class FleetStatusTests: XCTestCase {

    // MARK: Building a session out of real wire frames

    private func summary(
        lifecycle: String, blockedOn: [String] = [], link: String = "attached"
    ) -> SessionSummary {
        let ids = blockedOn.map { "\"\($0)\"" }.joined(separator: ",")
        // A failure here is a decoder change, not a test bug, so it must be loud.
        // swiftlint:disable:next force_try
        return try! JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"cc-1",
                 "tmux_session":"cc-1","cwd":"/tmp/x","lifecycle":"\(lifecycle)",
                 "link":"\(link)","last_seq":9,"created_at":"2026-08-02T10:00:00Z",
                 "updated_at":"2026-08-02T10:00:00Z","blocked_on":[\(ids)]}
                """.utf8))
    }

    private func event(_ seq: UInt64, _ kind: String, _ payload: String, source: String = "hook")
        -> Event
    {
        // swiftlint:disable:next force_try
        try! JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":\(seq),"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNPQR","session_id":"cc-1",
                 "ts":"2026-08-02T10:00:0\(seq)Z","kind":"\(kind)","payload":\(payload),
                 "source":"\(source)"}
                """.utf8))
    }

    /// A tool that ran and reported an error, in the exact shape the transcript
    /// sends one: `kind: tool_result`, `source: transcript`, and the result inside
    /// `message.content` as a block. `PayloadViews.transcriptToolResult` reads
    /// nothing else, so a hand-rolled shape would prove nothing.
    private func failedTool(_ seq: UInt64) -> [Event] {
        [
            event(seq, "tool_call", #"{"tool_name":"Bash","tool_use_id":"t1"}"#),
            event(
                seq + 1, "tool_result",
                #"{"message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"exit 1"}]}}"#,
                source: "transcript"),
        ]
    }

    /// **Awaits the rebuild.** `ingest` schedules the timeline pass rather than
    /// running it, so a 500-event replay collapses into one pass instead of 500
    /// (`SessionState.scheduleRebuild`). Read synchronously the timeline is empty
    /// and every assertion here would pass for the wrong reason, which is exactly
    /// what happened on the first run of this file. The emptiness check is kept so
    /// a future change to that scheduling fails loudly rather than silently
    /// hollowing out the suite.
    private func status(
        lifecycle: String = "live", blockedOn: [String] = [], events: [Event],
        reviewedSeq: UInt64 = 0
    ) async -> FleetStatus {
        let state = SessionState(sessionKey: "01K1B3XQ8ZC0DE5FGH7JKMNPQR")
        for event in events { state.ingest(event) }
        try? await Task.sleep(for: SessionState.coalesceWindow * 6)
        XCTAssertFalse(state.timeline.isEmpty, "the rebuild did not run; the test proves nothing")
        return FleetStatusRule.status(
            summary: summary(lifecycle: lifecycle, blockedOn: blockedOn),
            state: state, reviewedSeq: reviewedSeq)
    }

    // MARK: The defect

    /// The exact report: a Bash call exited 1, Claude read the output and carried
    /// on. The session was working and the phone said Failed.
    func testAFailedToolDoesNotMakeAWorkingSessionFailed() async {
        let result = await status(events: failedTool(1))
        XCTAssertNotEqual(result, .failed, "a tool exiting non-zero is agent input, not a verdict")
        XCTAssertEqual(result, .running, "the agent is still working, so the row says so")
    }

    /// The half that was worse: on an *ended* session there is no next user
    /// message, so the false verdict never expired.
    func testAFailedToolDoesNotPermanentlyBrandAnEndedSession() async {
        let result = await status(lifecycle: "exited", events: failedTool(1))
        XCTAssertEqual(result, .ended, "the run ended; nothing observed says it ended badly")
    }

    /// Pressing Esc is a choice, not a failure.
    func testAnInterruptedToolIsNotAFailure() async {
        let events = [
            event(1, "tool_call", #"{"tool_name":"Bash","tool_use_id":"t1"}"#),
            event(2, "tool_result", #"{"tool_use_id":"t1","interrupted":true}"#),
        ]
        let result = await status(events: events)
        XCTAssertNotEqual(result, .failed)
    }

    /// The third instance of the same mistake: the daemon saying it could not read
    /// a transcript line is a fact about our observation, not about the agent.
    func testATranscriptReadErrorIsNotTheAgentFailing() async {
        let events =
            failedTool(1) + [
                event(3, "turn_complete", "{}"),
                event(4, "error", #"{"message":"transcript_line_too_long"}"#),
            ]
        let result = await status(events: events, reviewedSeq: 99)
        XCTAssertNotEqual(result, .failed, "our own read failure is not the agent's failure")
        XCTAssertNotEqual(
            result, .running,
            "and it must not count as activity, or a finished turn looks busy for ever")
        XCTAssertEqual(result, .idle)
    }

    // MARK: What Failed does mean

    /// Control plane: the session's own exit, non-zero. The only route in.
    func testANonZeroSessionExitIsFailed() async {
        let result = await status(
            lifecycle: "exited", events: [event(1, "session_end", #"{"exit_code":7}"#)])
        XCTAssertEqual(result, .failed)
    }

    func testAZeroExitIsEndedNotFailed() async {
        let result = await status(
            lifecycle: "exited", events: [event(1, "session_end", #"{"exit_code":0}"#)])
        XCTAssertEqual(result, .ended)
    }

    /// The case that is true on every real machine today: nobody watched the run
    /// end, so there is no status. Unknown must not be promoted to failure. The
    /// daemon's own comment calls inventing a zero a lie, and inventing a failure
    /// is the same lie pointed the other way.
    func testAnUnknownExitIsEndedNotFailed() async {
        let result = await status(
            lifecycle: "exited", events: [event(1, "session_end", #"{"exit_code":null}"#)])
        XCTAssertEqual(result, .ended)
    }

    // MARK: The states that must keep working

    func testAPendingApprovalStillWinsEverything() async {
        let result = await status(blockedOn: ["req-1"], events: failedTool(1))
        XCTAssertEqual(
            result, .blocked, "an open human decision outranks every other fact about the run")
    }

    func testAFinishedUnreviewedTurnIsDone() async {
        let events = failedTool(1) + [event(3, "turn_complete", "{}")]
        let result = await status(events: events, reviewedSeq: 0)
        XCTAssertEqual(result, .doneUnreviewed)
    }

    func testAReviewedTurnIsIdle() async {
        let events = failedTool(1) + [event(3, "turn_complete", "{}")]
        let result = await status(events: events, reviewedSeq: 99)
        XCTAssertEqual(result, .idle)
    }

    /// A reviewed turn must stay reviewed. `reviewedSeq` is set to the stream tail
    /// at the moment you look, and events that draw nothing — `usage`, link state —
    /// keep pushing that tail up. Compared against the tail rather than the turn,
    /// the row silently un-reviewed itself and climbed the fleet again.
    func testAReviewedTurnStaysReviewedWhenSilentEventsArriveAfterIt() async {
        let events =
            failedTool(1) + [
                event(3, "turn_complete", "{}"),
                event(4, "usage", #"{"input_tokens":10}"#),
                event(5, "link_state", #"{"state":"attached"}"#),
            ]
        // Reviewed at the turn boundary, seq 3. Seq 4 and 5 draw nothing.
        let result = await status(events: events, reviewedSeq: 3)
        XCTAssertEqual(result, .idle, "silent events after a reviewed turn are not new work")
    }

    /// Link chatter is not the agent working. Guarding an existing rule that also
    /// had no test.
    func testLinkChatterAfterATurnDoesNotLookLikeWork() async {
        let events =
            failedTool(1) + [
                event(3, "turn_complete", "{}"),
                event(4, "link_state", #"{"state":"attached"}"#),
            ]
        let result = await status(events: events, reviewedSeq: 99)
        XCTAssertEqual(result, .idle)
    }


    // MARK: Turn complete is a boundary, not a bearer

    /// The duplication defect, pinned: the hook's copy of the message rendered
    /// in the notice, clamped, directly above the transcript's own message row
    /// — the same words twice. The boundary now carries no content.
    func testTurnCompleteCarriesNoDetail() async {
        let events = [
            event(
                1, "turn_complete",
                #"{"last_assistant_message":"Done - all tests pass."}"#)
        ]
        let state = SessionState(sessionKey: "01K1B3XQ8ZC0DE5FGH7JKMNPQR")
        for e in events { state.ingest(e) }
        try? await Task.sleep(for: SessionState.coalesceWindow * 6)
        let notices = state.timeline.compactMap { item -> NoticeItem? in
            if case .notice(let n) = item.content { return n }
            return nil
        }
        guard let turn = notices.first(where: { $0.kind == .turnComplete }) else {
            return XCTFail("no turn boundary in \(state.timeline)")
        }
        XCTAssertEqual(turn.title, "Turn complete")
        XCTAssertNil(turn.detail, "the transcript's message row is the record; this is a boundary")
    }

    /// And with the transcript row present, the content exists exactly once.
    func testTheMessageRendersExactlyOnceAlongsideItsBoundary() async {
        let events = [
            event(
                1, "agent_message",
                ###"{"message":{"content":[{"type":"text","text":"## Done"}]}}"###,
                source: "transcript"),
            event(2, "turn_complete", ###"{"last_assistant_message":"## Done"}"###),
        ]
        let state = SessionState(sessionKey: "01K1B3XQ8ZC0DE5FGH7JKMNPQR")
        for e in events { state.ingest(e) }
        try? await Task.sleep(for: SessionState.coalesceWindow * 6)
        let contentRows = state.timeline.filter {
            if case .agentMessage = $0.content { return true }
            return false
        }
        XCTAssertEqual(contentRows.count, 1, "one message, one row")
    }

    // MARK: Capability: unknown is not observe

    /// The launch flash, pinned: before the handshake answers, the rule must
    /// say "unknown" — which the fleet renders as nothing — never "observe",
    /// which it renders as a claim over every band.
    func testNilCapabilitiesIsUnknownNotObserve() {
        let badge = FleetStatusRule.capability(
            summary: summary(lifecycle: "live"), capabilities: nil)
        guard case .unknown(let reason) = badge else {
            return XCTFail("an unanswered handshake is not a verdict: \(badge)")
        }
        XCTAssertFalse(badge.canAct, "nothing may act before the ack")
        XCTAssertFalse(badge.isSettled, "and the fleet must say nothing")
        XCTAssertEqual(reason, "Not connected. The daemon has not told us what it can do.")
    }

    /// The band-header rule, whole: only settled observe earns the words.
    func testOnlySettledObserveEarnsTheBandNote() {
        XCTAssertNil(CapabilityBadge.control.bandNote, "control is the promise, not news")
        XCTAssertNil(
            CapabilityBadge.unknown(reason: "Not connected.").bandNote,
            "an in-flight handshake must print nothing — the launch flash, pinned")
        XCTAssertEqual(CapabilityBadge.observe(reason: "detached").bandNote, "Observe only")
    }

    /// The settled truth table stays settled.
    func testSettledCapabilityTruthTable() throws {
        let caps = try JSONDecoder().decode(
            Capabilities.self,
            from: Data(
                #"{"can_approve_reliably":true,"fail_mode":"fail_open","answer_path":"hook_return","hold_secs":25,"send_text":true,"capture":true,"push":true,"tls":false}"#
                    .utf8))
        for (link, expectControl) in [("attached", true), ("degraded", false),
                                      ("detached", false), ("stale", false)]
        {
            let badge = FleetStatusRule.capability(
                summary: summary(lifecycle: "live", link: link), capabilities: caps)
            XCTAssertEqual(badge.canAct, expectControl, link)
            XCTAssertTrue(badge.isSettled, "\(link) is a verdict and must render")
        }
    }
}
