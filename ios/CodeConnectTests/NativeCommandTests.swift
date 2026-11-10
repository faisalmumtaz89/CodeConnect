import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// The Model, Effort and Compact sheets' phases, as every app reads them from
/// Kit: what a send settles at once, which receipt settles the wait, and when
/// the wait goes quiet.
final class NativeCommandTests: XCTestCase {

    private let commands: [NativeCommand] = [.model("sonnet"), .effort("high"), .compact(instructions: "keep the plan")]

    /// A landed send waits for Claude Code; every other outcome settles at
    /// once, in the sheets' own words.
    func testEverySendOutcomeSettlesInTheSheetsWords() {
        for command in commands {
            XCTAssertNil(command.phase(after: .sent(matched: "cc-1")), "\(command)")
            XCTAssertEqual(
                command.phase(after: .alreadyApplied(appliedAt: "2026-11-10T07:00:00Z")),
                .neutral("This request was sent earlier; this retry does not confirm its outcome."))
            XCTAssertEqual(
                command.phase(after: .refused("the composer is not on screen.")),
                .failed("Nothing was typed: the composer is not on screen."))
            XCTAssertEqual(
                command.phase(after: .failed("the link dropped.")),
                .failed("Couldn’t type the command: the link dropped."))
            XCTAssertEqual(
                command.phase(after: .indeterminate("no answer came back.")),
                .failed("Not confirmed: no answer came back. Retry is safe; the command won’t be typed twice."))
            XCTAssertEqual(
                command.phase(after: .composerLost(command: "/x")),
                .failed("Couldn’t restore the composer. Open Terminal to recover."))
        }
        let recovered = ComposeAttempt.composerRecovered(
            command: "/x", paneSnapshot: nil, capturedAt: "2026-11-10T07:00:00Z")
        XCTAssertEqual(
            NativeCommand.model("sonnet").phase(after: recovered),
            .neutral(
                "/model sonnet took the composer on the computer and CodeConnect pressed Esc to give it back. "
                    + "No model change was confirmed."))
        XCTAssertEqual(
            NativeCommand.effort("high").phase(after: recovered),
            .neutral(
                "/effort high took the composer on the computer and CodeConnect pressed Esc to give it back. "
                    + "No effort change was confirmed."))
        XCTAssertEqual(
            NativeCommand.compact(instructions: "keep the plan").phase(after: recovered),
            .waiting(request: "keep the plan", quietly: false), "the compaction may still complete")
    }

    /// Only Claude Code's own receipt above the fence settles a wait: a change
    /// is confirmed, a kept value is neither success nor failure, and a refusal
    /// is Claude Code's words.
    func testOnlyAReceiptAboveTheFenceSettlesAWait() {
        func receipts(
            model: ModelCommandOutcome? = nil, effort: EffortConfirmation? = nil,
            compact: CompactConfirmation? = nil, seq: UInt64 = 5
        ) -> SessionState.Receipts {
            SessionState.Receipts(
                modelSignal: model.map { ModelCommandSignal(outcome: $0, seq: seq) },
                effort: effort, effortSeq: effort == nil ? 0 : seq,
                compact: compact, compactSeq: compact == nil ? 0 : seq)
        }
        let sonnet = NativeCommand.model("sonnet")
        let set = ModelConfirmation.set("Sonnet 5.5")
        XCTAssertEqual(sonnet.receiptPhase(from: receipts(model: .receipt(set)), after: 4), .confirmed(set.sheetStatus))
        XCTAssertNil(sonnet.receiptPhase(from: receipts(model: .receipt(set)), after: 5), "at the fence is not news")
        let kept = ModelConfirmation.kept("Opus 5")
        XCTAssertEqual(sonnet.receiptPhase(from: receipts(model: .receipt(kept)), after: 4), .neutral(kept.sheetStatus))
        let notFound = "Model 'bananas' not found"
        XCTAssertEqual(
            NativeCommand.model("bananas").receiptPhase(from: receipts(model: .notFound(notFound)), after: 4),
            .failed(notFound))

        let high = NativeCommand.effort("high")
        let confirmed = EffortConfirmation.set(value: "high", scope: nil)
        XCTAssertEqual(high.receiptPhase(from: receipts(effort: confirmed), after: 4), .confirmed(confirmed.sheetStatus))
        XCTAssertNil(high.receiptPhase(from: receipts(effort: confirmed), after: 5))
        let keptLevel = EffortConfirmation.kept(value: "high")
        XCTAssertEqual(high.receiptPhase(from: receipts(effort: keptLevel), after: 4), .neutral(keptLevel.sheetStatus))

        let compact = NativeCommand.compact(instructions: "")
        XCTAssertEqual(
            compact.receiptPhase(from: receipts(compact: .compacted), after: 4),
            .confirmed("Claude Code confirmed the compaction."))
        XCTAssertEqual(
            compact.receiptPhase(from: receipts(compact: .notEnoughMessages), after: 4),
            .failed("Not enough messages to compact."))
        XCTAssertNil(compact.receiptPhase(from: receipts(compact: .compacted), after: 5))

        for command in commands {
            XCTAssertNil(command.receiptPhase(from: nil, after: 0), "\(command)")
        }
    }

    /// A quiet wait frees the controls while the watch stays armed, and a
    /// compaction is given longer before its caption goes quiet.
    func testAQuietWaitFreesTheControlsAndACompactionWaitsLonger() {
        XCTAssertTrue(CommandPhase.sending("high").busy)
        XCTAssertTrue(CommandPhase.waiting(request: "high", quietly: false).busy)
        let free: [CommandPhase] = [
            .waiting(request: "high", quietly: true), .idle, .confirmed("c"), .neutral("n"), .failed("f"),
        ]
        for phase in free {
            XCTAssertFalse(phase.busy, "\(phase)")
        }
        XCTAssertEqual(NativeCommand.model("sonnet").quietAfter, .seconds(5))
        XCTAssertEqual(NativeCommand.effort("high").quietAfter, .seconds(5))
        XCTAssertEqual(NativeCommand.compact(instructions: "").quietAfter, .seconds(10))
    }
}
