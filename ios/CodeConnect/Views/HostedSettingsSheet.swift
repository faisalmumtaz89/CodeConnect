import CodeConnectKit
import SwiftUI

/// **The model, effort and permission-mode controls for a hosted Claude run** (B8, minor 39).
///
/// One sheet, three menus and the run's usage, one outcome line — reached from `/model` and `/effort`, because a run
/// CodeConnect started offers all three as structured menus the daemon validates, not as typed
/// commands. `SessionDetailView` sends every run's `/model` and `/effort` here.
///
/// Everything drawn comes from `AppModel.runSettings(for:)` — the one place both apps read — so the
/// selection, the labels and the in-flight state are the Kit's, never re-derived here. The view
/// switches on its three cases with **no `else`**: a run that is not offering menus (detached, ended,
/// an older daemon) shows its model read-only and no chooser, rather than a static list it cannot act
/// on.
struct HostedSettingsSheet: View {
    let sessionKey: String
    /// The `/model` or `/effort` draft that opened this sheet is consumed when a change is applied.
    var onLanded: () -> Void = {}

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.dynamicTypeSize) private var typeSize

    /// A mode the menu marks `confirm`, tapped and awaiting the person's confirmation before it is sent.
    @State private var confirmingMode: MenuMode?

    private var settings: RunSettings { model.runSettings(for: sessionKey) }

    /// The run's agent — the confirmation words are the agent's. Claude for a hosted run, which is all
    /// this sheet is reached for.
    private var agent: AgentKind { model.shownSummary(for: sessionKey)?.agent ?? .claude }

    var body: some View {
        CCSheetChrome("Settings", onClose: { dismiss() }) {
            VStack(spacing: 0) {
                ScrollView {
                    VStack(alignment: .leading, spacing: CC.space.md) {
                        switch settings {
                        case .hostedLive(let live):
                            liveSections(live)
                        case .hostedReadOnly(let modelFact):
                            readOnly(modelFact)
                        case .deferToOtherPath:
                            // An agent these menus do not speak for: it says so.
                            // Handled, not defaulted, so the picker is never drawn for such a run.
                            unavailable
                        }
                    }
                    .padding(CC.space.md)
                }
                .scrollBounceBehavior(.basedOnSize)
                outcomeBar
            }
            .ccConfirmation($confirmingMode) { mode in
                let words = ModeConfirmation.words(for: agent)
                return CCConfirmation(
                    title: words.title, message: words.message,
                    answers: [
                        .init(title: words.cancel, kind: .cancel, id: "settings.mode.cancel"),
                        .init(title: words.confirm, kind: .destructive, id: "settings.mode.confirm") {
                            run { await model.setMode(for: sessionKey, modeValue: mode.value, confirmed: true) }
                        },
                    ])
            }
        }
    }

    // MARK: Live

    @ViewBuilder
    private func liveSections(_ live: HostedSettings) -> some View {
        if !typeSize.isAccessibilitySize { usage }
        CCSectionHeader("Model")
        card {
            ForEach(Array(live.models.enumerated()), id: \.element.value) { index, entry in
                choiceRow(
                    title: entry.label, detail: entry.detail,
                    isSelected: live.selectedModel?.value == entry.value,
                    isLast: index == live.models.count - 1, disabled: live.inFlight
                ) { await model.setModel(for: sessionKey, modelValue: entry.value) }
            }
        }
        // The effort menu is the selected model's; with nothing selected there is none to draw.
        if !live.efforts.isEmpty {
            CCSectionHeader("Effort")
            card {
                ForEach(Array(live.efforts.enumerated()), id: \.element.value) { index, entry in
                    let isSelected = live.selectedEffort?.value == entry.value
                    choiceRow(
                        title: entry.label,
                        // The model's own default, marked while no effort is chosen (decision 318).
                        detail: isSelected && live.effortIsDefault ? "Default" : nil,
                        isSelected: isSelected, offered: live.offers(entry),
                        isLast: index == live.efforts.count - 1, disabled: live.inFlight
                    ) { await model.setEffort(for: sessionKey, effortValue: entry.value) }
                }
            }
        }
        CCSectionHeader("Permission mode")
        card {
            ForEach(Array(live.modes.enumerated()), id: \.element.value) { index, entry in
                choiceRow(
                    title: entry.confirm ? "\(entry.label)…" : entry.label,
                    isSelected: live.selectedMode?.value == entry.value,
                    isLast: index == live.modes.count - 1, disabled: live.inFlight
                ) {
                    // A mode the daemon marks `confirm` asks first; nothing is sent until the alert's
                    // confirm button, which sends it with `confirmed`.
                    if entry.confirm {
                        confirmingMode = entry
                        return nil
                    }
                    return await model.setMode(for: sessionKey, modeValue: entry.value)
                }
            }
        }
        if typeSize.isAccessibilitySize { usage }
    }

    // MARK: Usage

    /// **What the run's usage is, in full** (`ReplyBar`, the Mac's card): the conversation's size against its
    /// model's window, each window of its account with its share and when it resets, the words for an account with
    /// no plan limits, and the account; each row only where the app holds it, the section only where it holds any.
    /// It asks the computer nothing: drawn from what the app holds, again once a minute while the sheet is open, so
    /// a reset's "resets in" stays current. It stands first where the settings line can draw the figures, so a tap on
    /// them opens on their details, and last at accessibility sizes, where the line never draws them and the sheet
    /// opens on its settings as before.
    @ViewBuilder
    private var usage: some View {
        if let bar = model.replyBar(for: sessionKey, now: Date()), !Self.usageRows(bar).isEmpty {
            TimelineView(.everyMinute) { context in
                let rows = model.replyBar(for: sessionKey, now: context.date).map(Self.usageRows) ?? []
                VStack(alignment: .leading, spacing: CC.space.md) {
                    CCSectionHeader("Usage")
                    card {
                        ForEach(Array(rows.enumerated()), id: \.offset) { index, row in
                            if let value = row.value {
                                CCFactRow(
                                    row.label, value: value, age: row.resets,
                                    separator: index < rows.count - 1)
                            } else {
                                CCFactRow(
                                    row.label, separator: index < rows.count - 1,
                                    value: { EmptyView() }, detail: { EmptyView() })
                            }
                        }
                    }
                }
            }
        }
    }

    /// The card's rows, in the Mac card's order: Context, each window, the no-limits words (a row with no value),
    /// Account.
    private static func usageRows(_ bar: ReplyBar) -> [(label: String, value: String?, resets: String?)] {
        var rows: [(label: String, value: String?, resets: String?)] = []
        if let context = bar.context { rows.append(("Context", "\(context.used) of \(context.window)", nil)) }
        rows += bar.limits.map { ($0.caption, "\($0.usedPercent)%", $0.resets) }
        if let note = bar.limitsNote { rows.append((note, nil, nil)) }
        if let account = bar.account { rows.append(("Account", account, nil)) }
        return rows
    }

    private func card<Content: View>(@ViewBuilder _ content: @escaping () -> Content) -> some View {
        CCCard(padding: 0) { VStack(spacing: 0) { content() } }
    }

    /// One selectable row: its label, a check when it is the current value, and
    /// its tap — disabled while any call is in flight (the menus are one unit) and on the row already in
    /// force (there is nothing to apply), unless the row says it is still on offer (`offered`: an effort marked as
    /// the model's default, which choosing makes the person's own).
    private func choiceRow(
        title: String, detail: String? = nil, isSelected: Bool, offered: Bool? = nil, isLast: Bool, disabled: Bool,
        apply: @escaping () async -> SettingResult?
    ) -> some View {
        CCRow(
            title,
            titleDetail: detail,
            showsChevron: false,
            separator: !isLast,
            action: (disabled || !(offered ?? !isSelected)) ? nil : { run(apply) }
        ) {
            CCIcon(isSelected ? "check-circle-fill" : "circle", size: 18, relativeTo: .body)
                .foregroundStyle(isSelected ? CC.text.primary : CC.text.tertiary)
        } trailing: {
            EmptyView()
        } meta: {
            EmptyView()
        }
        // The checkmark is drawn with an icon, which VoiceOver hides, so the selection is announced
        // as a trait on the row instead.
        .accessibilityAddTraits(isSelected ? [.isSelected] : [])
    }

    private func run(_ apply: @escaping () async -> SettingResult?) {
        Task {
            // The /model or /effort draft that opened the sheet is consumed only when a change is
            // actually applied; a refusal, an indeterminate answer or a lost link leaves it to retry.
            if case .applied? = await apply() { onLanded() }
        }
    }

    // MARK: Read-only

    @ViewBuilder
    private func readOnly(_ modelFact: RunModelName?) -> some View {
        if !typeSize.isAccessibilitySize { usage }
        CCSectionHeader("Model")
        card {
            if let modelFact, !modelFact.text.isEmpty {
                // The model by its menu's name, or as the run reported it: an id, in code type.
                CCRow(modelFact.text, titleIsMono: modelFact.reported, showsChevron: false, separator: false) {
                    CCIcon("check-circle-fill", size: 18, relativeTo: .body)
                        .foregroundStyle(CC.text.primary)
                } trailing: { EmptyView() } meta: { EmptyView() }
            } else {
                CCRow(
                    "Not reported", meta: "This run has not reported a model.",
                    showsChevron: false, separator: false
                ) {
                    CCIcon("circle-dashed", size: 16, weight: .regular, relativeTo: .body)
                        .foregroundStyle(CC.text.tertiary)
                } trailing: { EmptyView() }
            }
        }
        Text("This run isn’t offering model controls right now. Its model is shown as last reported.")
            .ccType(CC.type.footnote)
            .foregroundStyle(CC.text.tertiary)
            .fixedSize(horizontal: false, vertical: true)
        if typeSize.isAccessibilitySize { usage }
    }

    @ViewBuilder
    private var unavailable: some View {
        CCCard {
            Text("This run’s settings aren’t available here.")
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
        }
    }

    // MARK: Outcome

    /// **Pinned, not scrolled** — the one line the reader has to see, below the sections. While a
    /// call is in flight it says so once (the menus above are disabled); then the Kit's own wording for
    /// what the daemon answered, for as long as the Kit keeps it (`HostedSettings.lastOutcome`).
    @ViewBuilder
    private var outcomeBar: some View {
        if case .hostedLive(let live) = settings {
            if live.inFlight {
                CCActionBar { statusLine("Applying…", tone: .neutral) }
            } else if let outcome = live.lastOutcome {
                CCActionBar { statusLine(outcome.text, tone: Self.ccTone(outcome.tone)) }
            }
        }
    }

    private func statusLine(_ text: String, tone: CCTone) -> some View {
        CCProse(
            text, style: CC.type.footnote,
            color: tone == .neutral ? CC.text.secondary : tone.color
        )
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private static func ccTone(_ tone: SettingOutcome.Tone) -> CCTone {
        switch tone {
        case .success: return .success
        case .neutral: return .neutral
        case .warning: return .warning
        }
    }
}
