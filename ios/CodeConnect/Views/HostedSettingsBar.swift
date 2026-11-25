import CodeConnectKit
import SwiftUI

/// The run's model-and-mode control, above the composer, for a hosted Claude run or a live Codex run: a tap opens the
/// `HostedSettingsSheet`. The run's mode is named beside the model whenever it is not the agent's asking default — the
/// menu's first mode — so a run left in another mode says so at rest (decision 228); a mode the menu marks `confirm`
/// (one that lets the agent work without asking) is in the alert's red. The disabled check follows the run's facts:
/// the sheet's calls are refused while a card waits on the run or it is still starting, so the control is dead then
/// (the Mac's rule, through the same `sendBlockedReason`). The effort in use is named after the model, by Kit's one rule
/// (`RunSettings.effortName`), as the Mac names it.
///
/// **At accessibility sizes the label is two lines**: the model — which may truncate — over the mode — which may not,
/// because the mode is the one thing this label exists to show. A run with no menus shows its model as a read-only
/// fact; a run these controls do not own (`deferToOtherPath`) shows nothing.
///
/// **The run's usage** (`ReplyBar`) stands at the row's leading edge, part of the same button, in the first form that
/// fits beside the whole line: its context and every window, the fullest window's ring alone, or none; at
/// accessibility sizes none. Its sentence is always the button's value, so VoiceOver reads it whichever form is drawn.
struct HostedSettingsBar: View {
    let model: AppModel
    let key: String
    /// Open the `HostedSettingsSheet` for this run.
    let onOpen: () -> Void

    @Environment(\.dynamicTypeSize) private var typeSize

    /// The sheet's calls are refused while a card waits or the run is starting; the empty draft keeps a long Codex
    /// draft's own ceiling — a send concern, not a settings one — out of this check.
    private var blocked: Bool { Self.isHeld(model, key) }

    /// Files on their way, or one that failed, hold the message, not the run: its model and effort stay open, as on
    /// the Mac.
    static func isHeld(_ model: AppModel, _ key: String) -> Bool {
        model.sendBlockedReason(for: key, draft: "", files: false) != nil
    }
    /// The effort in use, by Kit's one rule.
    private var effort: String? { model.shownSummary(for: key).flatMap(RunSettings.effortName) }

    var body: some View {
        switch model.runSettings(for: key) {
        case .deferToOtherPath:
            EmptyView()
        case .hostedReadOnly(let fact):
            // A run with no menus shows its model as a read-only fact — but only when there is one. A
            // still-starting run has no model fact yet; it shows nothing rather than a "Model" placeholder.
            if let fact, !fact.text.isEmpty {
                row {
                    label(modelName: fact.text, mono: fact.reported, effort: effort, mode: nil)
                        .foregroundStyle(CC.text.secondary)
                        .accessibilityIdentifier("session.composer.model")
                }
            } else {
                EmptyView()
            }
        case .hostedLive(let settings):
            if let bar = usage, !typeSize.isAccessibilitySize {
                // The first that fits whole: the figures, the fullest window's ring alone, or the line as it is
                // without them. The line is measured whole, so its model's name is never cut to make room.
                ViewThatFits(in: .horizontal) {
                    figuresRow(settings, bar: bar, fullestOnly: false)
                    if !bar.limits.isEmpty {
                        figuresRow(settings, bar: bar, fullestOnly: true)
                    }
                    row { settingsButton(settings, usage: bar) { line(settings) } }
                }
            } else {
                // No usage held, or an accessibility size, where the line alone fills the row; any usage held is
                // still the button's value.
                row { settingsButton(settings, usage: usage) { line(settings) } }
            }
        }
    }

    /// The run's usage (`ReplyBar`), when the app holds its context or a window of its account.
    private var usage: ReplyBar? {
        guard let bar = model.replyBar(for: key, now: Date()), bar.context != nil || !bar.limits.isEmpty
        else { return nil }
        return bar
    }

    /// The figures at the row's leading edge and the line, whole, at its trailing edge, all one button.
    private func figuresRow(_ settings: HostedSettings, bar: ReplyBar, fullestOnly: Bool) -> some View {
        settingsButton(settings, usage: bar) {
            HStack(spacing: 0) {
                UsageFigures(bar: bar, fullestOnly: fullestOnly)
                Spacer(minLength: CC.space.sm)
                line(settings).fixedSize()
            }
        }
        .padding(.horizontal, CC.space.md)
        .padding(.top, CC.space.xs)
    }

    private func line(_ settings: HostedSettings) -> some View {
        label(modelName: settings.modelDisplay.isEmpty ? "Model" : settings.modelDisplay,
              mono: settings.modelDisplayReported && !settings.modelDisplay.isEmpty,
              effort: effort, mode: settings.standingMode)
    }

    /// The one control: opens the sheet; its label is the line's words, its value the usage's.
    private func settingsButton<Content: View>(
        _ settings: HostedSettings, usage: ReplyBar?, @ViewBuilder _ content: () -> Content
    ) -> some View {
        Button(action: onOpen) {
            content()
                .frame(minHeight: 44)
                .contentShape(Rectangle())
        }
        .foregroundStyle(CC.text.secondary)
        .disabled(blocked)
        .accessibilityIdentifier("session.composer.settings")
        .accessibilityLabel(
            [settings.modelDisplay.isEmpty ? "Model" : settings.modelDisplay,
             effort.map { "\($0) effort" }, settings.standingMode?.label]
                .compactMap { $0 }.joined(separator: ", "))
        .accessibilityValue(usage?.spoken ?? "")
    }

    /// The control sits at the trailing edge, just above the composer.
    private func row<Content: View>(@ViewBuilder _ content: () -> Content) -> some View {
        HStack(spacing: 0) {
            Spacer()
            content()
        }
        .padding(.horizontal, CC.space.md)
        .padding(.top, CC.space.xs)
    }

    @ViewBuilder
    /// `mono`: the name is the agent's own report of the model (an id), drawn in code type.
    private func label(modelName: String, mono: Bool, effort: String?, mode: MenuMode?) -> some View {
        let icon = Image(systemName: "slider.horizontal.3").accessibilityHidden(true)
        let name = Text(modelName).ccType(mono ? CC.type.footnote.monospaced() : CC.type.footnote)
        Group {
            if typeSize.isAccessibilitySize, let mode {
                VStack(alignment: .trailing, spacing: 1) {
                    HStack(spacing: 4) {
                        icon
                        name.lineLimit(1).truncationMode(.tail)
                        if let effort { Text("· \(effort)").lineLimit(1).layoutPriority(1) }
                    }
                    Text(mode.label).lineLimit(1).foregroundStyle(modeStyle(mode))
                }
            } else {
                HStack(spacing: 4) {
                    icon
                    name.lineLimit(1).truncationMode(.tail)
                    if let effort { Text("· \(effort)").lineLimit(1).layoutPriority(1) }
                    if let mode {
                        Text("·")
                        // The model yields first (no priority, truncates) so the mode — the one thing this
                        // label exists to show — keeps its whole text even at the largest non-AX sizes. This
                        // is not asserted by a test (XCUITest reads the full accessibility label whatever is
                        // drawn); it was shown red and green by render on the 375pt device at xxxLarge with
                        // the longest real labels, so a regression would surface only in a render review.
                        Text(mode.label).lineLimit(1).layoutPriority(1).foregroundStyle(modeStyle(mode))
                    }
                }
            }
        }
        .ccType(CC.type.footnote)
    }

    /// A `confirm` mode is in the alert's red (it lets the agent work without asking); any other mode reads as the
    /// rest of the label.
    private func modeStyle(_ mode: MenuMode) -> AnyShapeStyle {
        mode.confirm ? AnyShapeStyle(CC.color.danger) : AnyShapeStyle(CC.text.secondary)
    }
}
