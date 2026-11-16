import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **The one place the model / effort / mode controls are computed** (B8, minor 39), against the
/// daemon's own `minor-39-wire.json`: which menus to draw, what is selected, the display names both
/// apps read, and the sentence after each answer. The selection follows the summary's facts only.
@MainActor
final class RunSettingsTests: XCTestCase {

    private let uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"

    private func wire() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-39-wire", withExtension: "json"))
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func summary(_ key: String) throws -> SessionSummary {
        try JSONDecoder().decode(
            SessionSummary.self, from: Data(try XCTUnwrap(try wire()[key]).canonicalJSONString.utf8))
    }

    private func result(_ key: String) throws -> SettingResult {
        let frame = try JSONDecoder().decode(
            ServerMessage.self, from: Data(try XCTUnwrap(try wire()[key]).canonicalJSONString.utf8))
        guard case .settingResult(_, _, let result) = frame else {
            throw XCTSkip("\(key) is not a setting_result")
        }
        return result
    }

    /// A live hosted run draws its menus, the selection its facts pick, and the display names — the
    /// matched model label, the labelled efforts of that model, the mode by its own label.
    func testALiveHostedRunDrawsItsMenusSelectionAndNames() throws {
        guard case .hostedLive(let s) = RunSettings.of(summary: try summary("session_hosted_live")) else {
            return XCTFail("a live hosted run with menus is hostedLive")
        }
        XCTAssertEqual(s.models.map(\.value), ["default", "sonnet"])
        XCTAssertEqual(s.modes.map(\.value), ["default", "acceptEdits", "plan"])
        // Selection: the daemon's `selected_model` ("sonnet") for the model; the mode and effort facts
        // ("plan", "high") match their menu values directly.
        XCTAssertEqual(s.selectedModel?.value, "sonnet")
        XCTAssertEqual(s.selectedMode?.value, "plan")
        XCTAssertEqual(s.selectedEffort?.value, "high")
        // Display names, from the one place each already lives.
        XCTAssertEqual(s.modelDisplay, "Sonnet")
        XCTAssertEqual(s.selectedEffort?.label, "High")
        // The selected model's efforts are labelled: raw "xhigh"/"max" read as "Extra high"/"Maximum".
        XCTAssertEqual(
            s.efforts.map(\.label), ["Low", "Medium", "High", "Extra high", "Maximum"])
        XCTAssertFalse(s.inFlight)
        XCTAssertNil(s.lastOutcome)
    }

    /// **The fix's reason, against the daemon's own init-shape summary.** After claude's next init the
    /// model fact is its full id ("claude-sonnet-5-5"), which is not a menu value; the selection follows
    /// the daemon's `selected_model` ("sonnet"), so the right row — and its efforts — still draw.
    /// Reading the fact's text instead (the rule this replaced) would select nothing here: the must-fail.
    func testTheInitShapeSelectsViaTheDaemonField() throws {
        let fact = try summary("session_hosted_live_init").model?.value
        XCTAssertEqual(fact, "claude-sonnet-5-5", "the init fact is claude's full id, not a menu value")
        guard case .hostedLive(let s) = RunSettings.of(summary: try summary("session_hosted_live_init"))
        else {
            return XCTFail("a live hosted run with menus is hostedLive")
        }
        XCTAssertEqual(s.selectedModel?.value, "sonnet", "selection follows selected_model, not the fact")
        XCTAssertEqual(s.modelDisplay, "Sonnet")
        XCTAssertEqual(s.selectedEffort?.value, "high")
        XCTAssertEqual(s.selectedMode?.value, "plan")
    }

    /// The same daemon's older summary — no `menus` — draws no picker: the model fact shows read-only,
    /// exactly as it arrived, with no label lookup and no effort or mode line (ruling 7).
    func testAHostedRunWithoutMenusIsReadOnlyRawFact() throws {
        guard case .hostedReadOnly(let model) = RunSettings.of(
            summary: try summary("session_hosted_live_minor_38"))
        else {
            return XCTFail("a hosted run with no menus is hostedReadOnly")
        }
        XCTAssertEqual(model, "sonnet (claude-sonnet-5-5)")
    }

    /// A terminal (tmux) Claude run keeps its own picker: this entry draws nothing.
    func testATmuxClaudeRunDefersToTheOtherPath() throws {
        XCTAssertEqual(RunSettings.of(summary: try summary("session_tmux_live")), .deferToOtherPath)
    }

    /// Each answer class words itself as the control should show it: the applied cases name the setting
    /// and the value's label, now or from the next message; the refusals and the indeterminate carry
    /// the agent's own sentence verbatim.
    func testEachAnswerWordsItsOutcome() throws {
        XCTAssertEqual(
            SettingOutcome.of(setting: .model, label: "Sonnet", result: try result("applied_now")),
            SettingOutcome(text: "Model set to Sonnet.", tone: .success))
        XCTAssertEqual(
            SettingOutcome.of(setting: .mode, label: "Plan", result: try result("applied_next_turn")),
            SettingOutcome(text: "Mode changes to Plan from your next message.", tone: .neutral))
        XCTAssertEqual(
            SettingOutcome.of(setting: .effort, label: "High", result: try result("applied_effort")),
            SettingOutcome(text: "Effort changes to High from your next message.", tone: .neutral))
        // Refusals and indeterminate: the agent's sentence, verbatim, as a warning.
        XCTAssertEqual(
            SettingOutcome.of(setting: .mode, label: "Plan", result: try result("refused_bypass")),
            SettingOutcome(
                text: "That permission mode isn't offered in this session; nothing was changed.",
                tone: .warning))
        XCTAssertEqual(
            SettingOutcome.of(setting: .mode, label: "Plan", result: try result("indeterminate")),
            SettingOutcome(
                text: "Claude didn't answer, so the change may or may not have been made.",
                tone: .warning))
    }
}
