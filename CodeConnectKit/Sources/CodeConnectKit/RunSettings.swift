import Foundation

/// **What an app's model / effort / mode controls draw for one run** (B8, minor 39).
///
/// Computed in ONE place from the run's summary, so the iPhone and the Mac draw the same menus, the
/// same selection and the same strings. A view switches on this with **no `else`**: a hosted run with
/// no menus suppresses the picker rather than falling back to a static family list, which is what keeps
/// a detached, ended or older-daemon run off a list it cannot act on.
public enum RunSettings: Sendable, Hashable {
    /// Not a hosted run these controls own: a terminal (tmux) run, Claude's or Codex's. This entry draws
    /// nothing.
    case deferToOtherPath
    /// A hosted Claude run with no menus — detached, ended, still starting, or a daemon below minor 39.
    /// No picker: the model fact shows read-only exactly as it arrived (no label lookup without a menu
    /// to match against), and no effort or mode line at all.
    case hostedReadOnly(model: String?)
    /// A live hosted Claude run offering its menus.
    case hostedLive(HostedSettings)

    /// The entry for a run, from its summary and the two things only the client knows: whether one of
    /// its setting calls is in flight (the menus are then disabled), and the last answer still worth
    /// showing. The selection follows the summary's facts only — never an optimistic guess between a
    /// tap and its answer.
    public static func of(
        summary: SessionSummary, inFlight: Bool = false, lastOutcome: SettingOutcome? = nil
    ) -> RunSettings {
        guard summary.agent == .claude || summary.agent == .codex, summary.tmuxSession.isEmpty
        else { return .deferToOtherPath }
        guard let menus = summary.menus else { return .hostedReadOnly(model: summary.model?.value) }

        // The model the daemon states as chosen — never re-derived from the fact's text here, because
        // the fact at a turn's init is claude's full id and only the daemon knows the alias it resolves
        // to. Absent (an older daemon, or an unlisted value) selects nothing.
        let selectedModel = menus.selectedModel
            .flatMap { value in menus.models.first { $0.value == value } }
        // Effort and mode facts carry the menu's own words, so a plain equality selects them.
        let selectedMode = summary.permissionMode
            .flatMap { fact in menus.modes.first { $0.value == fact.value } }
        // The effort menu is the selected model's efforts; with nothing selected there is no effort
        // menu to draw. Each effort carries its display name from the one place those names live.
        let efforts = (selectedModel?.efforts ?? []).map {
            EffortChoice(value: $0, label: EffortConfirmation.label(for: $0))
        }
        let selectedEffort = summary.effort
            .flatMap { fact in efforts.first { $0.value == fact.value } }
        // The one string drawn for the model — the button, the header: the selected entry's label, and
        // the raw fact only when the daemon named no selection.
        let modelDisplay = selectedModel?.label ?? summary.model?.value ?? ""

        return .hostedLive(
            HostedSettings(
                models: menus.models, modes: menus.modes,
                selectedModel: selectedModel, selectedMode: selectedMode,
                efforts: efforts, selectedEffort: selectedEffort,
                modelDisplay: modelDisplay, inFlight: inFlight, lastOutcome: lastOutcome))
    }
}

/// A live hosted run's settings, ready to draw. The menus are the daemon's own; the selection and the
/// display strings are computed once here so both apps agree.
public struct HostedSettings: Sendable, Hashable {
    /// The model rows, in the daemon's order (each with its `value`, `label`, `detail` and `efforts`).
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
    /// The model string for a single-line surface (the button, the header): the selected row's label,
    /// or the raw fact when nothing matched.
    public let modelDisplay: String
    /// A set call is in flight: the menus are disabled, and the selection stays on the current fact.
    public let inFlight: Bool
    /// The last answer still worth showing — retained until that setting's fact changes or the next
    /// call starts. `nil` when there is nothing to report.
    public let lastOutcome: SettingOutcome?
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
