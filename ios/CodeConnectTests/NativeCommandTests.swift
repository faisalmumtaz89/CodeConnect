import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// The Compact sheet's phases, as every app reads them from Kit: what a send settles at once, which receipt
/// settles the wait, and when the wait goes quiet.
final class NativeCommandTests: XCTestCase {

    private let compact = NativeCommand.compact(instructions: "keep the plan")

    /// A landed send waits for Claude Code; every other outcome settles at
    /// once, in the sheet's own words.
    func testEverySendOutcomeSettlesInTheSheetsWords() {
        XCTAssertNil(compact.phase(after: .sent(matched: "cc-1")))
        XCTAssertEqual(
            compact.phase(after: .alreadyApplied(appliedAt: "2026-11-10T07:00:00Z")),
            .neutral("This request was sent earlier; this retry does not confirm its outcome."))
        XCTAssertEqual(
            compact.phase(after: .refused("the composer is not on screen.")),
            .failed("Nothing was typed: the composer is not on screen."))
        XCTAssertEqual(
            compact.phase(after: .failed("the link dropped.")),
            .failed("Couldn’t type the command: the link dropped."))
        XCTAssertEqual(
            compact.phase(after: .indeterminate("no answer came back.")),
            .failed("Not confirmed: no answer came back. Retry is safe; the command won’t be typed twice."))
    }

    /// Only Claude Code's own receipt above the fence settles a wait: a
    /// compaction is confirmed, and a refusal is Claude Code's words.
    func testOnlyAReceiptAboveTheFenceSettlesAWait() {
        func receipts(_ signal: CompactConfirmation, seq: UInt64 = 5) -> SessionState.Receipts {
            SessionState.Receipts(compact: signal, compactSeq: seq)
        }
        XCTAssertEqual(
            compact.receiptPhase(from: receipts(.compacted), after: 4),
            .confirmed("Claude Code confirmed the compaction."))
        XCTAssertEqual(
            compact.receiptPhase(from: receipts(.notEnoughMessages), after: 4),
            .failed("Not enough messages to compact."))
        XCTAssertNil(compact.receiptPhase(from: receipts(.compacted), after: 5), "at the fence is not news")
        XCTAssertNil(compact.receiptPhase(from: nil, after: 0))
    }

    /// A quiet wait frees the controls while the watch stays armed.
    func testAQuietWaitFreesTheControls() {
        XCTAssertTrue(CommandPhase.sending("keep the plan").busy)
        XCTAssertTrue(CommandPhase.waiting(request: "keep the plan", quietly: false).busy)
        let free: [CommandPhase] = [
            .waiting(request: "keep the plan", quietly: true), .idle, .confirmed("c"), .neutral("n"), .failed("f"),
        ]
        for phase in free {
            XCTAssertFalse(phase.busy, "\(phase)")
        }
        XCTAssertEqual(compact.quietAfter, .seconds(10))
    }

    /// Every word the native controls show, as every app reads them from Kit: the status in each phase on the
    /// iPhone, unchanged, and on a Mac, which names the computer (or says "the agent's computer" when the model does
    /// not know it); and the Clear and Compact controls.
    @MainActor
    func testTheCommandWordsInEachDevicesWords() {
        let unnamed = AppModel(machineID: "unpaired")
        let named = AppModel(
            machineID: "automation", pairing: PairingStore(ephemeral: true),
            cache: EventCache(
                root: URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent(UUID().uuidString)),
            settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!))
        named.pairing.saveEphemeral(
            DaemonEndpoint(host: "studio.tail1234.ts.net", port: 8787, credential: .token("t"), useTLS: false))
        func said(_ phase: CommandPhase) -> [String?] {
            [
                unnamed.commandStatus(compact, phase, on: .iPhone)?.text,
                named.commandStatus(compact, phase, on: .iPhone)?.text,
                unnamed.commandStatus(compact, phase, on: .mac)?.text,
                named.commandStatus(compact, phase, on: .mac)?.text,
            ]
        }
        let host = "studio.tail1234.ts.net"
        XCTAssertEqual(
            said(.sending("keep the plan")),
            ["Typing /compact on the computer…", "Typing /compact on the computer…",
             "Typing /compact on the agent’s computer…", "Typing /compact on \(host)…"])
        XCTAssertEqual(
            said(.waiting(request: compact.request, quietly: false)),
            Array(repeating: "Waiting for Claude Code to confirm…", count: 4))
        XCTAssertEqual(said(.idle), [nil, nil, nil, nil])
        XCTAssertEqual(
            said(.waiting(request: "keep the plan", quietly: true)),
            ["Compaction requested on the computer. CodeConnect has not received a completion signal.",
             "Compaction requested on the computer. CodeConnect has not received a completion signal.",
             "Compaction requested on the agent’s computer. CodeConnect has not received a completion signal.",
             "Compaction requested on \(host). CodeConnect has not received a completion signal."])
        let settled: [(CommandPhase, ComposeProse.Tone)] = [
            (.confirmed("Claude Code confirmed the compaction."), .success),
            (.neutral(NativeCommand.alreadySentLine), .neutral),
            (.failed("Not enough messages to compact."), .warning),
        ]
        for (phase, tone) in settled {
            let status = named.commandStatus(compact, phase, on: .mac)
            XCTAssertEqual(status?.tone, tone, "\(phase)")
            XCTAssertEqual(said(phase), Array(repeating: status?.text, count: 4), "\(phase)")
        }
        XCTAssertEqual(named.commandStatus(compact, .sending("keep the plan"), on: .mac)?.tone, .neutral)
        XCTAssertEqual(
            named.commandStatus(compact, .waiting(request: "keep the plan", quietly: true), on: .mac)?.tone, .neutral)

        let clear = ClearConfirmation.ask
        XCTAssertEqual(
            [clear.title, clear.confirm, clear.cancel, clear.message],
            ["Clear Claude’s context?", "Clear Context", "Keep Context",
             "Claude Code forgets this conversation’s context. CodeConnect’s timeline stays here."])
        XCTAssertEqual(
            [CompactPrompt.title, CompactPrompt.explanation(), CompactPrompt.fieldLabel, CompactPrompt.placeholder(),
             CompactPrompt.action],
            ["Compact context", "Ask Claude Code to compact its current context. Instructions are optional.",
             "Instructions (optional)", "What should Claude preserve?", "Compact"])
    }
}
