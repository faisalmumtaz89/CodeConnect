import Foundation

/// **What an app's model / effort / mode controls draw for one run** (B8, minor 39).
///
/// Computed in ONE place from the run's summary, so the iPhone and the Mac draw the same menus, the
/// same selection and the same strings. A view switches on this with **no `else`**: a hosted run with
/// no menus suppresses the picker rather than falling back to a static family list, which is what keeps
/// a detached, ended or older-daemon run off a list it cannot act on.
public enum RunSettings: Sendable, Hashable {
    /// Not a run these controls own: an agent they do not know. This entry draws
    /// nothing.
    case deferToOtherPath
    /// A hosted Claude run with no menus — detached, ended, still starting, or a daemon below minor 39.
    /// No picker: the model shows read-only, by the label the daemon names it by (minor 46) or else exactly as it
    /// arrived, and no effort or mode line at all.
    case hostedReadOnly(model: RunModelName?)
    /// A live hosted Claude run offering its menus.
    case hostedLive(HostedSettings)

    /// The effort a run is on, as the reply row names it beside the model: the effort in force on the chosen model
    /// (`effortInForce`), by its label; "Default" while the chosen model offers efforts and neither names one; nil when
    /// the chosen model offers none. Without menus, or with no model chosen in them, only what the agent named.
    public static func effortName(summary: SessionSummary) -> String? {
        let named = summary.effort.map { EffortConfirmation.label(for: $0.value) }
        guard let menus = summary.menus, let model = menus.models.first(where: { $0.value == menus.selectedModel })
        else { return named }
        guard !model.efforts.isEmpty else { return nil }
        return effortInForce(on: model, fact: summary.effort?.value).map(EffortConfirmation.label(for:)) ?? "Default"
    }

    /// **The one rule an effort is shown and marked by** (decision 313): the effort the agent last named while the
    /// model offers it; else, with none chosen (or a fact left from the model before a switch), the model's own
    /// default effort as its entry names it; else none, which reads "Default" with nothing marked.
    static func effortInForce(on model: MenuModel, fact: String?) -> String? {
        [fact, model.defaultEffort].compactMap { $0 }.first { model.efforts.contains($0) }
    }

    /// The entry for a run, from its summary and the things only the client knows: the model the run
    /// has confirmed it is on (`AppModel.confirmedModel`), whether one of its setting calls is in flight
    /// (the menus are then disabled), and the last answer still worth showing. The selection follows the
    /// summary's facts only — never an optimistic guess between a tap and its answer.
    public static func of(
        summary: SessionSummary, confirmed: String? = nil, inFlight: Bool = false,
        lastOutcome: SettingOutcome? = nil
    ) -> RunSettings {
        guard summary.agent == .claude || summary.agent == .codex else { return .deferToOtherPath }
        guard let menus = summary.menus else {
            return .hostedReadOnly(
                model: summary.model.map {
                    summary.modelLabel.map { RunModelName(text: $0, reported: false) }
                        ?? RunModelName(text: $0.value, reported: true)
                })
        }
        let models = menus.models.map(\.shown)

        // The model the daemon states as chosen — never re-derived from the fact's text here, because
        // the fact at a turn's init is claude's full id and only the daemon knows the alias it resolves
        // to. Absent (an older daemon, or an unlisted value) selects nothing.
        let selectedModel = menus.selectedModel
            .flatMap { value in models.first { $0.value == value } }
        // Effort and mode facts carry the menu's own words, so a plain equality selects them.
        let selectedMode = summary.permissionMode
            .flatMap { fact in menus.modes.first { $0.value == fact.value } }
        // The effort menu is the selected model's efforts; with nothing selected there is no effort
        // menu to draw. Each effort carries its display name from the one place those names live.
        let efforts = (selectedModel?.efforts ?? []).map {
            EffortChoice(value: $0, label: EffortConfirmation.label(for: $0))
        }
        let selectedEffort = selectedModel
            .flatMap { effortInForce(on: $0, fact: summary.effort?.value) }
            .flatMap { value in efforts.first { $0.value == value } }
        let effortIsDefault =
            selectedEffort != nil && !(summary.effort.map { fact in efforts.contains { $0.value == fact.value } } ?? false)
        // The one string drawn for the run's model — the button, the header: the model the run is on once
        // it has said so, by `modelName`; before that the selected entry's name, and the fact as sent only when
        // the daemon named no selection. "Default" is a row of the menu, never the run's model.
        let modelDisplay = confirmed.map { modelName(confirmed: $0, menus: menus) }
            ?? selectedModel.map { RunModelName(text: modelName(label: $0.label, detail: $0.detail), reported: false) }
            ?? RunModelName(text: summary.model?.value ?? "", reported: summary.model != nil)

        return .hostedLive(
            HostedSettings(
                models: models, modes: menus.modes,
                selectedModel: selectedModel, selectedMode: selectedMode,
                efforts: efforts, selectedEffort: selectedEffort, effortIsDefault: effortIsDefault,
                modelDisplay: modelDisplay.text, modelDisplayReported: modelDisplay.reported, inFlight: inFlight,
                lastOutcome: lastOutcome))
    }

    /// The model a run is on, as a line names it, from structured fields only, and as the menu names it, so every
    /// line and the settings button spell one model alike: the name (`modelName(label:detail:)`) of the entry whose
    /// `value` is that model (a Codex run's), else of the entry that runs it (its `resolvedModel`, the id a Claude run
    /// reports). The Default
    /// entry is never the one: its label names no model, so a run on the default model reads by the entry of the
    /// model it runs. Claude's receipt of a change is no id: it reads by the entry the daemon says is chosen, by the
    /// model that entry runs, and by its own name when the agent gives it no id. A run sent no menus (not live)
    /// reads by the label the daemon names its model by (`label`, minor 46). A model no entry names reads as the
    /// agent reported it.
    public static func modelName(confirmed: String, menus: RunMenus?, label: String? = nil) -> RunModelName {
        let models = menus?.models ?? []
        func entry(running id: String) -> MenuModel? {
            models.first { $0.value == id } ?? models.first { $0.value != "default" && $0.resolvedModel == id }
        }
        func named(_ row: MenuModel) -> RunModelName {
            RunModelName(text: modelName(label: row.shown.label, detail: row.detail), reported: false)
        }
        if let row = entry(running: confirmed) { return named(row) }
        if let chosen = models.first(where: { $0.value == menus?.selectedModel }) {
            guard let id = chosen.resolvedModel else { return named(chosen) }
            return entry(running: id).map(named) ?? RunModelName(text: id, reported: true)
        }
        if menus == nil, let label { return RunModelName(text: label, reported: false) }
        return RunModelName(text: confirmed, reported: true)
    }

    /// **The one rule a model is named by**, wherever an app names one (ccd names an ended run's model by the same
    /// rule): when its entry's description has a " · ", the part before the first one, if that part begins with the
    /// entry's label; otherwise the entry's label. Claude Code's "Sonnet" reads by its description's "Sonnet 5.5";
    /// a description with no " · " is a sentence, never a name, so its model keeps its label, as does a label its
    /// description does not repeat ("Qwen3.8-27B", a Codex model's) and Default, whose description names the model it
    /// runs. The menu's own rows show the label and the description as the agent wrote them.
    public static func modelName(label: String, detail: String?) -> String {
        guard let parts = detail?.components(separatedBy: " · "), parts.count > 1, parts[0].hasPrefix(label)
        else { return label }
        return parts[0]
    }
}

/// The model a run is on, as a single line names it.
public struct RunModelName: Sendable, Hashable {
    public let text: String
    /// `text` is the agent's own report of the model, not a menu row's label: drawn in code type.
    public let reported: Bool
}

/// A live hosted run's settings, ready to draw. The menus are the daemon's own; the selection and the
/// display strings are computed once here so both apps agree.
public struct HostedSettings: Sendable, Hashable {
    /// The model rows, in the daemon's order (each with its `value`, `label` and `efforts`).
    public let models: [MenuModel]
    /// The permission-mode rows, with the agent's own labels.
    public let modes: [MenuMode]
    /// The model selected now — the row the daemon named in `menus.selected_model` — or `nil` when it
    /// named none (an older daemon, or a value off the menus; `modelDisplay` then shows the raw fact).
    public let selectedModel: MenuModel?
    public let selectedMode: MenuMode?
    /// The selected model's efforts, each with its display name. Empty when no model is selected.
    public let efforts: [EffortChoice]
    public let selectedEffort: EffortChoice?
    /// `selectedEffort` is the chosen model's own default, not one the run was set to (decision 318): the agent named
    /// no effort, or one this model does not offer. The panel then names it as the default; the lines name its level.
    public let effortIsDefault: Bool
    /// The model string for a single-line surface (the button, the header): the model the run has
    /// confirmed, by `RunSettings.modelName`; before it has, the selected row's name, or the fact as sent
    /// when nothing matched.
    public let modelDisplay: String
    /// `modelDisplay` is the agent's own report of the model, not a row's label: drawn in code type.
    public let modelDisplayReported: Bool
    /// A set call is in flight: the menus are disabled, and the selection stays on the current fact.
    public let inFlight: Bool
    /// The last answer still worth showing — retained until that setting's fact moves past it (an answer
    /// that applies from the next turn: until this device sends the run's next message) or the next call
    /// starts. `nil` when there is nothing to report.
    public let lastOutcome: SettingOutcome?

    /// The run's mode when it is not the agent's asking default (the menu's first mode), to name beside the
    /// model at rest so a run left in another mode says so (decision 228); `nil` when it is the asking default.
    /// A mode the menu marks `confirm` is drawn in the alert's red by the view. The one place this rule is
    /// composed, so the iPhone and the Mac name the same mode.
    public var standingMode: MenuMode? {
        selectedMode == modes.first ? nil : selectedMode
    }
}

/// One effort choice: the wire `value` a `set_model` carries, and the display name it shows.
public struct EffortChoice: Sendable, Hashable {
    public let value: String
    public let label: String
}

/// Which setting a call changed — chooses the applied-case wording. Not read off any string.
public enum SettingKind: Sendable, Hashable {
    case model, effort, mode

    var noun: String {
        switch self {
        case .model: return "Model"
        case .effort: return "Effort"
        case .mode: return "Mode"
        }
    }
}

/// What to show after one setting call. The applied cases the app words itself, naming the setting and
/// the value's label; the refusals and the indeterminate carry the agent's own sentence, verbatim.
public struct SettingOutcome: Sendable, Hashable {
    public let text: String
    public let tone: Tone

    public enum Tone: Sendable, Hashable { case success, neutral, warning }

    public init(text: String, tone: Tone) {
        self.text = text
        self.tone = tone
    }

    /// The outcome of one call, or `nil` when there is nothing honest to show (a status this build
    /// cannot read claims neither applied nor not). `label` is the display name of the value that was
    /// set — the menu's label for a model or mode, `EffortConfirmation.label` for an effort.
    public static func of(setting: SettingKind, label: String, result: SettingResult) -> SettingOutcome? {
        switch result {
        case .applied(.now):
            return SettingOutcome(text: "\(setting.noun) set to \(label).", tone: .success)
        case .applied(.nextTurn), .applied(.unknown):
            return SettingOutcome(
                text: "\(setting.noun) changes to \(label) from your next message.", tone: .neutral)
        case .refused(let reason), .indeterminate(let reason):
            return SettingOutcome(text: reason, tone: .warning)
        case .unknown:
            return nil
        }
    }
}
