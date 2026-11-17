import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **The one place the model / effort / mode controls are computed** (B8, minor 39), against the
/// daemon's own `minor-39-wire.json`: which menus to draw, what is selected, the display names both
/// apps read, and the sentence after each answer. The selection follows the summary's facts only.
@MainActor
final class RunSettingsTests: XCTestCase {

    private let uid = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"

    private func wire(_ name: String = "minor-39-wire") throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: name, withExtension: "json"))
        return try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }

    private func summary(_ key: String, in name: String = "minor-39-wire") throws -> SessionSummary {
        try JSONDecoder().decode(
            SessionSummary.self, from: Data(try XCTUnwrap(try wire(name)[key]).canonicalJSONString.utf8))
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
        XCTAssertEqual(s.modes.map(\.value), ["default", "acceptEdits", "plan", "bypassPermissions"])
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

    /// **X6: a live engine Codex run draws its settings through the same path as a hosted Claude run.**
    /// The widened guard admits a Codex run with no tmux session; the menus are Codex's own (9 models,
    /// four modes with Full Access confirm-marked), the selection follows the daemon's `selected_model`
    /// and the `permission_mode` fact, and the effort menu is the selected model's efforts — labelled,
    /// with "ultra" now "Ultra". The effort fact is absent (Codex leaves it to the model at a start), so
    /// nothing is selected while the menu still lists its efforts. From the daemon's `codex-settings-wire.json`.
    func testALiveCodexRunDrawsItsSettingsThroughRunSettings() throws {
        guard case .hostedLive(let s) = RunSettings.of(
            summary: try summary("session_codex_live", in: "codex-settings-wire"))
        else { return XCTFail("a live engine Codex run with menus is hostedLive") }
        XCTAssertEqual(s.models.count, 9)
        XCTAssertEqual(s.modes.map(\.value), ["auto", "read-only", "plan", "full-access"])
        XCTAssertEqual(s.modes.last?.confirm, true, "Full Access is confirm-marked")
        XCTAssertEqual(s.selectedModel?.value, "gpt-6.1-sol")
        XCTAssertEqual(s.selectedMode?.value, "auto")
        XCTAssertNil(s.selectedEffort, "the effort fact is absent, so nothing is selected")
        XCTAssertEqual(s.efforts.map(\.value), ["low", "medium", "high", "xhigh", "max", "ultra"])
        XCTAssertEqual(s.efforts.last?.label, "Ultra", "ultra is labelled, the only new Codex effort")
    }

    /// Full Access, once Codex reports it: the `permission_mode` fact selects the confirm-marked row.
    func testACodexRunInFullAccessSelectsTheConfirmMarkedMode() throws {
        guard case .hostedLive(let s) = RunSettings.of(
            summary: try summary("session_codex_full_access", in: "codex-settings-wire"))
        else { return XCTFail("a live engine Codex run with menus is hostedLive") }
        XCTAssertEqual(s.selectedMode?.value, "full-access")
        XCTAssertEqual(s.selectedMode?.confirm, true)
        XCTAssertEqual(s.selectedModel?.value, "gpt-6-astra")
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

    /// Each answer class words itself as the control should show it: the applied cases name the setting
    /// and the value's label, now or from the next message; the refusals and the indeterminate carry
    /// the agent's own sentence verbatim.
    func testEachAnswerWordsItsOutcome() throws {
        XCTAssertEqual(
            SettingOutcome.of(setting: .model, label: "Sonnet", result: try result("applied_now")),
            SettingOutcome(text: "Model set to Sonnet.", tone: .success))
        XCTAssertEqual(
            SettingOutcome.of(setting: .mode, label: "Plan", result: try result("applied_mode_during_a_turn")),
            SettingOutcome(text: "Mode set to Plan.", tone: .success))
        XCTAssertEqual(
            SettingOutcome.of(setting: .effort, label: "High", result: try result("applied_effort")),
            SettingOutcome(text: "Effort changes to High from your next message.", tone: .neutral))
        // Refusals and indeterminate: the agent's sentence, verbatim, as a warning.
        XCTAssertEqual(
            SettingOutcome.of(setting: .mode, label: "Plan", result: try result("refused_bypass")),
            SettingOutcome(
                text: "Turning on Bypass permissions needs your confirmation in the app; nothing was changed.",
                tone: .warning))
        XCTAssertEqual(
            SettingOutcome.of(setting: .mode, label: "Plan", result: try result("indeterminate")),
            SettingOutcome(
                text: "Claude didn't answer, so the change may or may not have been made.",
                tone: .warning))
    }
}
