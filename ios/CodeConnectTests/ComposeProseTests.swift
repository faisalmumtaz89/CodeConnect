import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// What the line above a run's composer says, and which outcomes clear it: the
/// words and rules every app reads from Kit.
final class ComposeProseTests: XCTestCase {

    private let outcomes: [ComposeAttempt] = [
        .sent(matched: "foragents"),
        .refused("the composer is not on screen."),
        .failed("the link dropped."),
        .alreadyApplied(appliedAt: "2026-11-10T03:00:00Z"),
        .indeterminate("no answer came back."),
        .composerRecovered(command: "/effort high", paneSnapshot: nil, capturedAt: "2026-11-10T03:00:00Z"),
        .composerLost(command: "/effort high"),
    ]

    /// Each outcome of a Claude send, in its sentence, tone and mark; a landed send has none.
    func testEverySendOutcomeHasItsSentenceToneAndMark() {
        let expected: [(String, ComposeProse.Tone, String)?] = [
            nil,
            ("Not typed: the composer is not on screen.", .warning, "warning-fill"),
            ("Couldn’t type the message: the link dropped.", .danger, "x-circle-fill"),
            nil,
            (
                "Not confirmed: no answer came back. Retry is safe; the message won’t be typed twice.", .warning,
                "question-fill"
            ),
            (
                "After /effort high was typed, the computer's composer disappeared. CodeConnect pressed Esc and "
                    + "confirmed it returned. The command's outcome was not confirmed.", .warning, "question-fill"
            ),
            (
                "/effort high was typed, but the computer's composer did not come back. Open Terminal to recover.",
                .danger, "warning-fill"
            ),
        ]
        for (attempt, expected) in zip(outcomes, expected) {
            let note = ComposeProse.note(attempt)
            guard let (text, tone, icon) = expected else {
                XCTAssertNil(note, "\(attempt)")
                continue
            }
            XCTAssertEqual(note?.text, text)
            XCTAssertEqual(note?.tone, tone, "\(attempt)")
            XCTAssertEqual(note?.icon, icon, "\(attempt)")
            XCTAssertEqual(note?.offersWhy, false, "\(attempt)")
        }
    }

    /// What landed clears the draft, and a recovered composer's note retires by itself; a refusal, a failure
    /// and an unknown outcome stand until the next edit; a lost composer stands
    /// until another attempt.
    func testWhichOutcomesLandAndWhichAnEditRetires() {
        XCTAssertEqual(outcomes.filter(\.landed), [outcomes[0], outcomes[3], outcomes[5]])
        XCTAssertEqual(outcomes.filter(\.clearsOnEdit), [outcomes[1], outcomes[2], outcomes[4]])
        let codex: [ComposeResult] = [
            .started(turnID: "t"), .steered(turnID: "t"), .duplicate(turnID: "t", started: false),
            .rejected(reason: "r"), .indeterminate(reason: "r"), .unknown(status: "s"),
        ]
        XCTAssertEqual(codex.filter(\.landed), Array(codex.prefix(3)))
    }

    /// Every mark a composer note can carry, so an app that draws its own
    /// symbols knows the whole set: a Claude send's, a settled Codex stop's or
    /// message's, and the not-sent and unanswered marks.
    func testEveryNoteMarkIsOneOfTheKnownSet() {
        let codexMessages: [ComposeResult] = [
            .started(turnID: "t"), .steered(turnID: "t"), .duplicate(turnID: "t", started: true),
            .duplicate(turnID: "t", started: false), .rejected(reason: "r"), .indeterminate(reason: "r"),
            .unknown(status: "s"),
        ]
        let codexStops: [InterruptResult] = [
            .aborted(turnID: "t"), .duplicate(turnID: "t"), .rejected(reason: "r"), .indeterminate(reason: "r"),
            .unknown(status: "s"),
        ]
        let marks =
            outcomes.compactMap { ComposeProse.note($0)?.icon }
            + codexMessages.compactMap { CodexProse.compose($0).map(ComposeProse.note)?.icon }
            + codexStops.compactMap { ComposeProse.note(CodexProse.interrupt($0)).icon }
            + ["warning-circle-fill", "question-fill"]
        XCTAssertEqual(
            Set(marks),
            ["copy", "question", "question-fill", "stop-circle-fill", "warning-circle-fill", "warning-fill", "x-circle-fill"])
    }
}
