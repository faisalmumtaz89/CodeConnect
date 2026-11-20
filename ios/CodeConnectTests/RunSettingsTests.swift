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
        // The default entry is named "Default" whatever the agent calls it.
        XCTAssertEqual(s.models.first?.label, "Default")
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

    /// **A Codex run started on a model its start's check did not list has that model as its checked row.** The
    /// daemon offers the model the run started on first (its id its label, no efforts: Codex names none for it) and
    /// names it `selected_model`, so the button shows a chosen row the person can return to. With no efforts, no
    /// effort menu is drawn while it is chosen: the written miss. From the daemon's `codex-settings-wire.json`.
    func testACodexRunOnAnUnlistedModelHasItAsTheCheckedRow() throws {
        guard case .hostedLive(let s) = RunSettings.of(
            summary: try summary("session_codex_unlisted_default", in: "codex-settings-wire"))
        else { return XCTFail("a live engine Codex run with menus is hostedLive") }
        XCTAssertEqual(s.models.count, 10)
        XCTAssertEqual(s.models.first?.value, "stub-model")
        XCTAssertEqual(s.selectedModel?.value, "stub-model", "the model the run started on is the checked row")
        XCTAssertEqual(s.modelDisplay, "stub-model")
        XCTAssertEqual(s.efforts, [], "Codex names no efforts for it, so no effort menu")
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
                text: "Claude didn’t answer, so the change may or may not have been made.",
                tone: .warning))
    }

    /// **The standing mode the control names beside the model (decision 228).** `nil` when the run sits in
    /// the agent's asking default (the menu's first mode); the mode otherwise, carrying the `confirm` flag
    /// the view draws in red. The one place both apps read this, so iPhone and Mac name the same mode.
    func testStandingModeNamesTheModeOnlyWhenNotTheAskingDefault() throws {
        // The asking default (the menu's first mode): nothing to name beside the model.
        guard case .hostedLive(let auto) = RunSettings.of(
            summary: try summary("session_codex_live", in: "codex-settings-wire"))
        else { return XCTFail("a live Codex run is hostedLive") }
        XCTAssertEqual(auto.selectedMode?.value, auto.modes.first?.value, "auto is the menu's first mode")
        XCTAssertNil(auto.standingMode, "the asking default names no mode")
        XCTAssertEqual(auto.modelDisplay, "GPT-6.1-Sol")

        // A non-default, non-confirm mode: named, not red.
        guard case .hostedLive(let plan) = RunSettings.of(summary: try summary("session_hosted_live"))
        else { return XCTFail("a live hosted Claude run is hostedLive") }
        XCTAssertEqual(plan.standingMode?.value, "plan")
        XCTAssertEqual(plan.standingMode?.confirm, false, "Plan is not a confirm mode")
        XCTAssertEqual(plan.modelDisplay, "Sonnet")

        // A confirm mode: named, confirm-marked — the view draws it red.
        guard case .hostedLive(let full) = RunSettings.of(
            summary: try summary("session_codex_full_access", in: "codex-settings-wire"))
        else { return XCTFail("a live Codex run is hostedLive") }
        XCTAssertEqual(full.standingMode?.value, "full-access")
        XCTAssertEqual(full.standingMode?.confirm, true, "Full Access is confirm-marked")
        XCTAssertEqual(full.modelDisplay, "GPT-6-Astra")
    }

    /// **The model a run is on, named as its menu names it.** A Codex run's model fact is a menu value,
    /// so it reads by that row's label, the button's own words.
    func testARunsModelMatchingAMenuEntryReadsByItsLabel() throws {
        let run = try summary("session_codex_live", in: "codex-settings-wire")
        XCTAssertEqual(run.model?.value, "gpt-6.1-sol")
        XCTAssertEqual(
            RunSettings.modelName(confirmed: "gpt-6.1-sol", menus: run.menus),
            RunModelName(text: "GPT-6.1-Sol", reported: false))
    }

    /// A Claude run's model fact is claude's full id, which no menu entry carries: it reads as the id,
    /// exactly as reported, while the button names the setting.
    func testARunsModelMatchingNoEntryReadsAsReported() throws {
        let run = try summary("session_hosted_live_init")
        XCTAssertNotNil(run.menus)
        XCTAssertEqual(
            RunSettings.modelName(confirmed: "claude-sonnet-5-5", menus: run.menus),
            RunModelName(text: "claude-sonnet-5-5", reported: true))
        // A model this build has never heard of reads the same way: nothing here names models.
        XCTAssertEqual(
            RunSettings.modelName(confirmed: "claude-opus-6", menus: run.menus),
            RunModelName(text: "claude-opus-6", reported: true))
    }

    /// A run with no menus has nothing to match: its model reads as reported.
    func testARunsModelWithNoMenusReadsAsReported() throws {
        let run = try summary("session_hosted_live_minor_38")
        XCTAssertNil(run.menus)
        XCTAssertEqual(
            RunSettings.modelName(confirmed: "claude-sonnet-5-5", menus: run.menus),
            RunModelName(text: "claude-sonnet-5-5", reported: true))
    }

    /// After an app's model change a Claude run's fact is claude's receipt, which no entry carries: it
    /// reads as sent, until the next turn's init reports the id.
    func testARunsModelAfterAChangeReadsAsItsReceipt() throws {
        let run = try summary("session_hosted_live")
        XCTAssertEqual(run.model?.value, "sonnet (claude-sonnet-5-5)")
        XCTAssertEqual(
            RunSettings.modelName(confirmed: "sonnet (claude-sonnet-5-5)", menus: run.menus),
            RunModelName(text: "sonnet (claude-sonnet-5-5)", reported: true))
    }

    /// A row the agent labels with its id alone reads by that row's label.
    func testARunsModelOnAnIDOnlyRowReadsByItsLabel() throws {
        let run = try summary("session_codex_unlisted_default", in: "codex-settings-wire")
        XCTAssertEqual(
            RunSettings.modelName(confirmed: "stub-model", menus: run.menus),
            RunModelName(text: "stub-model", reported: false))
    }

    /// Each menu row carries the agent's own description, exactly as the daemon sent it from the agent:
    /// Claude Code 2.1.319's (its version inside), and Codex 0.167.0's.
    func testAMenuRowKeepsTheAgentsDescriptionAsSent() throws {
        let claude = try summary("session_hosted_live")
        let sonnet = try XCTUnwrap(claude.menus?.models.first { $0.value == "sonnet" })
        XCTAssertEqual(sonnet.detail, "Sonnet 5.5 · Efficient for routine tasks · $2/$10 per Mtok")
        let codex = try summary("session_codex_live", in: "codex-settings-wire")
        let sol = try XCTUnwrap(codex.menus?.models.first { $0.value == "gpt-6.1-sol" })
        XCTAssertEqual(sol.detail, "Latest workhorse model for coding and everyday work.")
        guard case .hostedLive(let s) = RunSettings.of(summary: claude) else { return XCTFail("hostedLive") }
        XCTAssertEqual(s.models.first { $0.value == "sonnet" }?.detail, sonnet.detail)
    }
}
