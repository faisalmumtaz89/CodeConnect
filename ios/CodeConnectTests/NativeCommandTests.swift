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
                modelSignal: ModelWatchSignal(
                    command: model.map { ModelCommandSignal(outcome: $0, seq: seq) }, turnEndSeq: 0),
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

    /// A `/model` typed while Claude Code works gets no receipt, ever: the end of
    /// the turn settles the wait, neutrally, claiming nothing. A receipt above the
    /// fence still wins, however late the turn's end behind it.
    func testATurnEndingWithNoReceiptSettlesAModelWait() {
        func receipts(_ model: ModelCommandOutcome? = nil, seq: UInt64 = 0, turnEnd: UInt64) -> SessionState.Receipts {
            SessionState.Receipts(
                modelSignal: ModelWatchSignal(
                    command: model.map { ModelCommandSignal(outcome: $0, seq: seq) }, turnEndSeq: turnEnd),
                effort: nil, effortSeq: 0, compact: nil, compactSeq: 0)
        }
        let sonnet = NativeCommand.model("sonnet")
        let unconfirmed = CommandPhase.neutral(
            "CodeConnect can't confirm /model sonnet: Claude Code writes no result to the conversation for a change "
                + "sent while it's working. The model shown may be out of date.")
        XCTAssertEqual(sonnet.receiptPhase(from: receipts(turnEnd: 5), after: 4), unconfirmed)
        XCTAssertNil(sonnet.receiptPhase(from: receipts(turnEnd: 4), after: 4), "a turn that ended before the send")
        let set = ModelConfirmation.set("Sonnet 5.5")
        XCTAssertEqual(
            sonnet.receiptPhase(from: receipts(.receipt(set), seq: 5, turnEnd: 9), after: 4),
            .confirmed(set.sheetStatus), "the receipt, not the turn's end behind it")
        XCTAssertEqual(
            sonnet.receiptPhase(from: receipts(.receipt(.set("Opus 5")), seq: 5, turnEnd: 9), after: 4), unconfirmed,
            "somebody else's change is not this one's answer")
        let high = NativeCommand.effort("high")
        XCTAssertNil(high.receiptPhase(from: receipts(turnEnd: 5), after: 4), "only /model is settled by a turn")
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

    /// Every word the native controls show, as every app reads them from Kit: the status in each phase on the
    /// iPhone, unchanged, and on a Mac, which names the computer (or says "the agent's computer" when the model does
    /// not know it); the consent lines, the effort levels, and the Clear and Compact controls.
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
        func said(_ command: NativeCommand, _ phase: CommandPhase) -> [String?] {
            [
                unnamed.commandStatus(command, phase, on: .iPhone)?.text,
                named.commandStatus(command, phase, on: .iPhone)?.text,
                unnamed.commandStatus(command, phase, on: .mac)?.text,
                named.commandStatus(command, phase, on: .mac)?.text,
            ]
        }
        let model = NativeCommand.model("sonnet"), effort = NativeCommand.effort("xhigh")
        let compact = NativeCommand.compact(instructions: "keep the plan")
        let host = "studio.tail1234.ts.net"
        XCTAssertEqual(
            said(model, .sending("sonnet")),
            ["Typing /model sonnet on the computer…", "Typing /model sonnet on the computer…",
             "Typing /model sonnet on the agent's computer…", "Typing /model sonnet on \(host)…"])
        XCTAssertEqual(
            said(effort, .sending("xhigh")),
            ["Typing /effort xhigh on the computer…", "Typing /effort xhigh on the computer…",
             "Typing /effort xhigh on the agent's computer…", "Typing /effort xhigh on \(host)…"])
        XCTAssertEqual(
            said(compact, .sending("keep the plan")),
            ["Typing /compact on the computer…", "Typing /compact on the computer…",
             "Typing /compact on the agent's computer…", "Typing /compact on \(host)…"])
        for command in [model, effort, compact] {
            XCTAssertEqual(
                said(command, .waiting(request: command.request, quietly: false)),
                Array(repeating: "Waiting for Claude Code to confirm…", count: 4), "\(command)")
            XCTAssertEqual(said(command, .idle), [nil, nil, nil, nil], "\(command)")
        }
        XCTAssertEqual(
            said(model, .waiting(request: "sonnet", quietly: true)),
            ["Sent /model sonnet to the computer. Still waiting for Claude Code to confirm.",
             "Sent /model sonnet to the computer. Still waiting for Claude Code to confirm.",
             "Sent /model sonnet to the agent's computer. Still waiting for Claude Code to confirm.",
             "Sent /model sonnet to \(host). Still waiting for Claude Code to confirm."])
        XCTAssertEqual(
            said(effort, .waiting(request: "xhigh", quietly: true))[0],
            "Sent /effort xhigh to the computer. Still waiting for Claude Code to confirm.")
        XCTAssertEqual(
            said(compact, .waiting(request: "keep the plan", quietly: true)),
            ["Compaction requested on the computer. CodeConnect has not received a completion signal.",
             "Compaction requested on the computer. CodeConnect has not received a completion signal.",
             "Compaction requested on the agent's computer. CodeConnect has not received a completion signal.",
             "Compaction requested on \(host). CodeConnect has not received a completion signal."])
        let settled: [(CommandPhase, ComposeProse.Tone)] = [
            (.confirmed("Model set to Sonnet 5.5."), .success),
            (.neutral("Claude Code kept Sonnet 5.5."), .neutral),
            (.failed("Not enough messages to compact."), .warning),
        ]
        for (phase, tone) in settled {
            for command in [model, effort, compact] {
                let status = named.commandStatus(command, phase, on: .mac)
                XCTAssertEqual(status?.tone, tone, "\(phase)")
                XCTAssertEqual(said(command, phase), Array(repeating: status?.text, count: 4), "\(phase)")
            }
        }
        XCTAssertEqual(named.commandStatus(model, .sending("sonnet"), on: .mac)?.tone, .neutral)
        XCTAssertEqual(named.commandStatus(model, .waiting(request: "sonnet", quietly: true), on: .mac)?.tone, .neutral)

        XCTAssertEqual(
            NativeCommand.modelConsent,
            "A confirmed change also becomes your default for new sessions. Switching mid-conversation makes "
                + "Claude Code re-read the whole conversation on your next message.")
        XCTAssertEqual(
            NativeCommand.effortConsent,
            "Switching mid-conversation makes Claude Code re-read the whole conversation on your next message.")
        XCTAssertEqual(NativeCommand.effortLevels.map(\.value), ["low", "medium", "high", "xhigh", "max"])
        XCTAssertEqual(NativeCommand.effortLevels.map(\.label), ["Low", "Medium", "High", "Extra high", "Maximum"])
        XCTAssertEqual(
            [ClearConfirmation.title, ClearConfirmation.clear, ClearConfirmation.keep, ClearConfirmation.message],
            ["Clear Claude’s context?", "Clear context", "Keep context",
             "Claude Code will forget this conversation’s context. CodeConnect’s timeline will stay here."])
        XCTAssertEqual(
            [CompactPrompt.title, CompactPrompt.explanation, CompactPrompt.fieldLabel, CompactPrompt.placeholder,
             CompactPrompt.action],
            ["Compact context", "Ask Claude Code to compact its current context. Instructions are optional.",
             "Instructions (optional)", "What should Claude preserve?", "Compact now"])
    }
}
