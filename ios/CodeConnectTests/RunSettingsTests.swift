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
        XCTAssertEqual(s.modelDisplay, "Sonnet 5.5")
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
    /// the model's own default effort is the one selected. From the daemon's `codex-settings-wire.json`.
    func testALiveCodexRunDrawsItsSettingsThroughRunSettings() throws {
        guard case .hostedLive(let s) = RunSettings.of(
            summary: try summary("session_codex_live", in: "codex-settings-wire"))
        else { return XCTFail("a live engine Codex run with menus is hostedLive") }
        XCTAssertEqual(s.models.count, 9)
        XCTAssertEqual(s.modes.map(\.value), ["auto", "read-only", "plan", "full-access"])
        XCTAssertEqual(s.modes.last?.confirm, true, "Full Access is confirm-marked")
        XCTAssertEqual(s.selectedModel?.value, "gpt-6.1-sol")
        XCTAssertEqual(s.selectedMode?.value, "auto")
        XCTAssertEqual(s.selectedEffort?.value, "low", "the effort fact is absent, so the model's default is selected")
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
        XCTAssertEqual(s.modelDisplay, "Sonnet 5.5")
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
        // The daemon names the fact by the chosen entry's id, so a run with no menus reads it bare too.
        XCTAssertEqual(model, RunModelName(text: "claude-sonnet-5-5", reported: true))
    }

    /// **An ended run names its model as its menu did (minor 46)**, from the daemon's own `minor-46-wire.json`:
    /// a run on the default model reads "Opus 5.5" while live, by its menu, and once it has ended, by the daemon's
    /// `model_label`, both in the header and in the reply box's read-only label. Must-fail: a build that draws
    /// an ended run's fact as reported reads `claude-opus-5-5`.
    func testAnEndedRunReadsByTheLabelTheDaemonNamesItsModelBy() throws {
        let live = try summary("session_hosted_live", in: "minor-46-wire")
        let ended = try summary("session_hosted_ended", in: "minor-46-wire")
        let opus = RunModelName(text: "Opus 5.5", reported: false)
        XCTAssertEqual(RunSettings.modelName(confirmed: "claude-opus-5-5", menus: live.menus), opus)
        XCTAssertNil(ended.menus)
        XCTAssertEqual(ended.model?.value, "claude-opus-5-5")
        XCTAssertEqual(ended.modelLabel, "Opus 5.5")
        XCTAssertEqual(
            RunSettings.modelName(confirmed: "claude-opus-5-5", menus: ended.menus, label: ended.modelLabel), opus)
        XCTAssertEqual(RunSettings.of(summary: ended), .hostedReadOnly(model: opus))
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
        XCTAssertEqual(plan.modelDisplay, "Sonnet 5.5")

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

    /// A Claude run's model fact is claude's full id, which no entry's value is: it reads by the label of the entry
    /// that runs it (its `resolved_model`), as the menu names it, so the header and the button say one model.
    func testAClaudeRunsModelReadsByTheEntryThatRunsIt() throws {
        let run = try summary("session_hosted_live_init")
        XCTAssertNotNil(run.menus)
        XCTAssertEqual(
            RunSettings.modelName(confirmed: "claude-sonnet-5-5", menus: run.menus),
            RunModelName(text: "Sonnet 5.5", reported: false))
    }

    /// Just after a change on a live run, the Mac's model fact and claude's receipt of it are one event: either
    /// reading names the chosen entry by its label.
    func testALiveRunsChangedModelReadsByItsEntryFromEitherReading() throws {
        let run = try summary("session_hosted_live")
        XCTAssertEqual(run.menus?.selectedModel, "sonnet")
        for confirmed in ["claude-sonnet-5-5", "sonnet (claude-sonnet-5-5)"] {
            XCTAssertEqual(
                RunSettings.modelName(confirmed: confirmed, menus: run.menus),
                RunModelName(text: "Sonnet 5.5", reported: false), confirmed)
        }
    }

    /// A model this build has never heard of, on a run whose menus name no entry for it, reads as the agent
    /// reported it: nothing here names models.
    func testAnUnlistedModelReadsAsReported() throws {
        let run = try summary("session_hosted_live_minor_38")
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

    /// After an app's model change a Claude run's fact is claude's receipt, which no entry carries: the run's
    /// model reads by the entry the daemon says is chosen, by the model it runs (minor 45's `resolved_model`, from
    /// Claude Code 2.1.319's own list), never as the receipt's text.
    func testARunsModelAfterAChangeReadsByTheChosenEntry() throws {
        let run = try summary("session_hosted_live")
        XCTAssertEqual(run.model?.value, "claude-sonnet-5-5")
        XCTAssertEqual(run.menus?.selectedModel, "sonnet")
        XCTAssertEqual(
            RunSettings.modelName(confirmed: "sonnet (claude-sonnet-5-5)", menus: run.menus),
            RunModelName(text: "Sonnet 5.5", reported: false))
        guard case .hostedLive(let s) = RunSettings.of(
            summary: run, confirmed: "sonnet (claude-sonnet-5-5)") else { return XCTFail("hostedLive") }
        XCTAssertEqual(s.modelDisplay, "Sonnet 5.5")
        XCTAssertFalse(s.modelDisplayReported)
    }

    /// The Default entry names no model, so a run on the default model reads by the entry of the model it runs
    /// ("Opus"), whether the fact is the id or claude's receipt; with no other entry for that model it reads as the
    /// id, as reported.
    func testARunOnTheDefaultModelReadsByTheEntryOfTheModelItRuns() throws {
        var frame = try XCTUnwrap(try wire()["session_hosted_live"]?.objectValue)
        var menus = try XCTUnwrap(frame["menus"]?.objectValue)
        menus["selected_model"] = .string("default")
        frame["menus"] = .object(menus)
        func decoded() throws -> SessionSummary {
            try JSONDecoder().decode(SessionSummary.self, from: Data(JSONValue.object(frame).canonicalJSONString.utf8))
        }
        XCTAssertEqual(try decoded().menus?.models.first?.resolvedModel, "claude-opus-5-5")
        for confirmed in ["claude-opus-5-5", "default (claude-opus-5-5)"] {
            XCTAssertEqual(
                RunSettings.modelName(confirmed: confirmed, menus: try decoded().menus),
                RunModelName(text: "claude-opus-5-5", reported: true), confirmed)
        }
        menus["models"] = .array(
            try XCTUnwrap(menus["models"]?.arrayValue) + [
                .object([
                    "value": .string("opus"), "label": .string("Opus"), "efforts": .array([]),
                    "resolved_model": .string("claude-opus-5-5"),
                ])
            ])
        frame["menus"] = .object(menus)
        for confirmed in ["claude-opus-5-5", "default (claude-opus-5-5)"] {
            XCTAssertEqual(
                RunSettings.modelName(confirmed: confirmed, menus: try decoded().menus),
                RunModelName(text: "Opus", reported: false), confirmed)
        }
    }

    /// A chosen entry the agent gives no model id for (a Claude Code that lists no `resolvedModel`) reads by its
    /// label, never by its value: "default" is an alias, not a model.
    func testAChosenEntryWithNoModelIDReadsByItsLabel() throws {
        var frame = try XCTUnwrap(try wire()["session_hosted_live"]?.objectValue)
        var menus = try XCTUnwrap(frame["menus"]?.objectValue)
        menus["models"] = .array(
            try XCTUnwrap(menus["models"]?.arrayValue).map { model in
                var entry = try XCTUnwrap(model.objectValue)
                entry["resolved_model"] = nil
                return .object(entry)
            })
        menus["selected_model"] = .string("default")
        frame["menus"] = .object(menus)
        let run = try JSONDecoder().decode(
            SessionSummary.self, from: Data(JSONValue.object(frame).canonicalJSONString.utf8))
        XCTAssertNil(run.menus?.models.first?.resolvedModel)
        XCTAssertEqual(
            RunSettings.modelName(confirmed: "claude-opus-5-5", menus: run.menus),
            RunModelName(text: "Default", reported: false))
    }

    /// Each Claude entry carries the model id claude's own list gives it, as the daemon sent it (minor 45); a
    /// Codex entry carries none, its value being the model.
    func testAMenuRowCarriesTheModelIDItRuns() throws {
        let claude = try summary("session_hosted_live")
        XCTAssertEqual(
            claude.menus?.models.map(\.resolvedModel), ["claude-opus-5-5", "claude-sonnet-5-5"])
        let codex = try summary("session_codex_live", in: "codex-settings-wire")
        XCTAssertTrue(codex.menus?.models.allSatisfy { $0.resolvedModel == nil } ?? false)
    }

    /// A row the agent labels with its id alone reads by that row's label.
    func testARunsModelOnAnIDOnlyRowReadsByItsLabel() throws {
        let run = try summary("session_codex_unlisted_default", in: "codex-settings-wire")
        XCTAssertEqual(
            RunSettings.modelName(confirmed: "stub-model", menus: run.menus),
            RunModelName(text: "stub-model", reported: false))
    }

    /// **A model is named in full, never by its family alone**: by the first part of its entry's description, up to
    /// " · ", when that part begins with the entry's label; otherwise by the label. The entries are Claude Code's own,
    /// a local model's, a Command Code model's and a Codex model's; Default names no model.
    func testAModelIsNamedByItsDescriptionsFirstPartWhenItBeginsWithItsLabel() {
        let cases: [(label: String, detail: String?, named: String)] = [
            ("Sonnet", "Sonnet 5.5 · Efficient for routine tasks", "Sonnet 5.5"),
            ("Opus", "Opus 5.5 · Best for everyday, complex tasks", "Opus 5.5"),
            ("Fable", "Fable 5.2 · Most capable for your hardest work", "Fable 5.2"),
            ("Haiku", "Haiku 5.5 · Fastest for quick answers", "Haiku 5.5"),
            ("Haiku", "Haiku 5.5", "Haiku"),
            ("Sonnet", "Sonnet is efficient for routine tasks", "Sonnet"),
            ("GPT-6.1 Terra", "GPT-6.1 Terra is the frontier model", "GPT-6.1 Terra"),
            ("Qwen3.8-27B", "Local model on the box", "Qwen3.8-27B"),
            ("GLM-5.3 · Command Code", "Command Code API, 1M context", "GLM-5.3 · Command Code"),
            ("GPT-6.1-Sol", "Latest workhorse model for coding and everyday work.", "GPT-6.1-Sol"),
            ("Sonnet", nil, "Sonnet"),
            ("Sonnet", "", "Sonnet"),
        ]
        for (label, detail, named) in cases {
            let menus = RunMenus(
                models: [MenuModel(value: "m", label: label, efforts: [], detail: detail)], modes: [], selectedModel: "m")
            XCTAssertEqual(
                RunSettings.modelName(confirmed: "m", menus: menus), RunModelName(text: named, reported: false),
                "\(label) / \(detail ?? "nil")")
        }
        let defaultEntry = MenuModel(
            value: "default", label: "Default (recommended)", efforts: [],
            detail: "Opus 5.5 · Best for everyday, complex tasks")
        XCTAssertEqual(defaultEntry.shown.label, "Default")
        XCTAssertEqual(RunSettings.modelName(label: defaultEntry.shown.label, detail: defaultEntry.detail), "Default")
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

    /// **The effort in use, named beside the model** (decisions 303 and 313): the effort the agent named, by its label;
    /// while none is named, or the one named is not the chosen model's (left from the model before a switch), the
    /// chosen model's own default (Sonnet's Medium in `minor-39-wire.json`); nothing for a run whose menus are gone and
    /// whose agent named none.
    func testTheEffortInUseIsNamedAndUnnamedReadsTheModelsDefault() throws {
        let live = try summary("session_hosted_live")
        XCTAssertEqual(RunSettings.effortName(summary: live), "High")
        var unnamed = try XCTUnwrap(try wire()["session_hosted_live"]?.objectValue)
        unnamed["effort"] = nil
        let none = try JSONDecoder().decode(
            SessionSummary.self, from: Data(JSONValue.object(unnamed).canonicalJSONString.utf8))
        XCTAssertEqual(RunSettings.effortName(summary: none), "Medium")
        // A fact the chosen model does not offer (left from the model before a switch) gives way to its default.
        unnamed["effort"] = .object(["seq": .int(7), "value": .string("ultra")])
        let stale = try JSONDecoder().decode(
            SessionSummary.self, from: Data(JSONValue.object(unnamed).canonicalJSONString.utf8))
        XCTAssertEqual(RunSettings.effortName(summary: stale), "Medium")
        XCTAssertNil(RunSettings.effortName(summary: try summary("session_hosted_starting")))
    }

    /// A recorded summary with its effort fact and selected model set as a case needs them.
    private func summary(
        _ key: String, in name: String, effort: String?, selected: String? = nil
    ) throws -> SessionSummary {
        var run = try XCTUnwrap(try wire(name)[key]?.objectValue)
        run["effort"] = effort.map { .object(["seq": .int(9), "value": .string($0)]) }
        if let selected, var menus = run["menus"]?.objectValue {
            menus["selected_model"] = .string(selected)
            run["menus"] = .object(menus)
        }
        return try JSONDecoder().decode(SessionSummary.self, from: Data(JSONValue.object(run).canonicalJSONString.utf8))
    }

    /// **With no effort chosen, a run names and marks its model's own default effort** (decisions 312 and 313), as the
    /// daemon names it on the menu's entry (`default_effort`, minor 47): the level by its name, never "Default". Codex's
    /// gpt-6.1-sol is Low (its own `defaultReasoningEffort`); Claude Code's Default entry is Medium (its documented
    /// default). A chosen effort still wins, and a fact the chosen model does not offer gives way to the default.
    func testWithNoEffortChosenARunNamesAndMarksItsModelsDefault() throws {
        for (key, name, value, label) in [
            ("session_codex_live", "codex-settings-wire", "low", "Low"),
            ("session_hosted_live", "minor-46-wire", "medium", "Medium"),
        ] {
            let run = try summary(key, in: name, effort: nil)
            XCTAssertEqual(RunSettings.effortName(summary: run), label, name)
            guard case .hostedLive(let s) = RunSettings.of(summary: run) else { return XCTFail("\(name): a live run") }
            XCTAssertEqual(s.selectedEffort?.value, value, "\(name): the default is the effort marked in force")

            let chosen = try summary(key, in: name, effort: "high")
            XCTAssertEqual(RunSettings.effortName(summary: chosen), "High", name)
            guard case .hostedLive(let c) = RunSettings.of(summary: chosen) else { return XCTFail("\(name): a live run") }
            XCTAssertEqual(c.selectedEffort?.value, "high", name)

            let stale = try summary(key, in: name, effort: "from-another-model")
            XCTAssertEqual(RunSettings.effortName(summary: stale), label, "\(name): a stale fact gives way")
        }
    }

    /// A run whose effort was never chosen, moved to another model: the mark follows the new entry's default
    /// (Codex's gpt-6-sol is Medium).
    func testTheDefaultFollowsTheModelWhenNoEffortWasChosen() throws {
        let moved = try summary("session_codex_live", in: "codex-settings-wire", effort: nil, selected: "gpt-6-sol")
        XCTAssertEqual(RunSettings.effortName(summary: moved), "Medium")
        guard case .hostedLive(let s) = RunSettings.of(summary: moved) else { return XCTFail("a live Codex run") }
        XCTAssertEqual(s.selectedModel?.value, "gpt-6-sol")
        XCTAssertEqual(s.selectedEffort?.value, "medium")
    }

    /// An entry that names no default keeps today's reading: "Default", nothing marked (decision 313: a model the
    /// documentation does not name stays unmarked). Claude Code's Fable entry names none in the daemon's menu.
    func testAnEntryWithNoDefaultReadsDefaultWithNothingMarked() throws {
        let run = try summary("session_hosted_live", in: "minor-46-wire", effort: nil, selected: "fable")
        XCTAssertNil(run.menus?.models.first { $0.value == "fable" }?.defaultEffort)
        XCTAssertEqual(RunSettings.effortName(summary: run), "Default")
        guard case .hostedLive(let s) = RunSettings.of(summary: run) else { return XCTFail("a live Claude run") }
        XCTAssertNil(s.selectedEffort)
    }

    /// **A default marked is named as the default** (decision 318): while the mark is the entry's own default, the
    /// panel says so; once an effort is chosen, it does not.
    func testTheMarkSaysWhenItIsTheModelsDefault() throws {
        for (key, name) in [("session_codex_live", "codex-settings-wire"), ("session_hosted_live", "minor-46-wire")] {
            guard case .hostedLive(let unset) = RunSettings.of(summary: try summary(key, in: name, effort: nil))
            else { return XCTFail("\(name): a live run") }
            XCTAssertNotNil(unset.selectedEffort, name)
            XCTAssertTrue(unset.effortIsDefault, "\(name): no effort chosen, the mark is the default")
            guard case .hostedLive(let stale) = RunSettings.of(
                summary: try summary(key, in: name, effort: "from-another-model"))
            else { return XCTFail("\(name): a live run") }
            XCTAssertTrue(stale.effortIsDefault, "\(name): a fact this model does not offer, the mark is the default")
            guard case .hostedLive(let chosen) = RunSettings.of(summary: try summary(key, in: name, effort: "high"))
            else { return XCTFail("\(name): a live run") }
            XCTAssertFalse(chosen.effortIsDefault, "\(name): a chosen effort is no default")
        }
        // Nothing marked (an entry that names no default) is no default marked either.
        guard case .hostedLive(let fable) = RunSettings.of(
            summary: try summary("session_hosted_live", in: "minor-46-wire", effort: nil, selected: "fable"))
        else { return XCTFail("a live Claude run") }
        XCTAssertFalse(fable.effortIsDefault)
    }
}
