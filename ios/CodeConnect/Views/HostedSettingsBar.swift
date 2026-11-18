import CodeConnectKit
import SwiftUI

/// The run's model-and-mode control, above the composer, for a hosted Claude run or a live Codex run: a tap opens the
/// `HostedSettingsSheet`. The run's mode is named beside the model whenever it is not the agent's asking default — the
/// menu's first mode — so a run left in another mode says so at rest (decision 228); a mode the menu marks `confirm`
/// (one that lets the agent work without asking) is in the alert's red. The disabled check follows the run's facts:
/// the sheet's calls are refused while a card waits on the run or it is still starting, so the control is dead then
/// (the Mac's rule, through the same `sendBlockedReason`).
///
/// **At accessibility sizes the label is two lines**: the model — which may truncate — over the mode — which may not,
/// because the mode is the one thing this label exists to show. A run with no menus shows its model as a read-only
/// fact; a run these controls do not own (`deferToOtherPath`) shows nothing.
struct HostedSettingsBar: View {
    let model: AppModel
    let key: String
    /// Open the `HostedSettingsSheet` for this run.
    let onOpen: () -> Void

    @Environment(\.dynamicTypeSize) private var typeSize

    /// The sheet's calls are refused while a card waits or the run is starting; the empty draft keeps a long Codex
    /// draft's own ceiling — a send concern, not a settings one — out of this check.
    private var blocked: Bool { model.sendBlockedReason(for: key, draft: "") != nil }

    var body: some View {
        switch model.runSettings(for: key) {
        case .deferToOtherPath:
            EmptyView()
        case .hostedReadOnly(let fact):
            // A run with no menus shows its model as a read-only fact — but only when there is one. A
            // still-starting run has no model fact yet; it shows nothing rather than a "Model" placeholder.
            if let fact, !fact.isEmpty {
                row {
                    label(modelName: ModelDisplay.from(fact).line, mode: nil)
                        .foregroundStyle(CC.text.secondary)
                        .accessibilityIdentifier("session.composer.model")
                }
            } else {
                EmptyView()
            }
        case .hostedLive(let settings):
            row {
                Button(action: onOpen) {
                    label(modelName: settings.modelDisplay.isEmpty ? "Model" : settings.modelDisplay,
                          mode: settings.standingMode)
                        .frame(minHeight: 44)
                        .contentShape(Rectangle())
                }
                .foregroundStyle(CC.text.secondary)
                .disabled(blocked)
                .accessibilityIdentifier("session.composer.settings")
                .accessibilityLabel(
                    settings.standingMode.map { "\(settings.modelDisplay), \($0.label)" }
                        ?? (settings.modelDisplay.isEmpty ? "Model" : settings.modelDisplay))
            }
        }
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
    private func label(modelName: String, mode: MenuMode?) -> some View {
        let icon = Image(systemName: "slider.horizontal.3").accessibilityHidden(true)
        Group {
            if typeSize.isAccessibilitySize, let mode {
                VStack(alignment: .trailing, spacing: 1) {
                    HStack(spacing: 4) { icon; Text(modelName).lineLimit(1).truncationMode(.tail) }
                    Text(mode.label).lineLimit(1).foregroundStyle(modeStyle(mode))
                }
            } else {
                HStack(spacing: 4) {
                    icon
                    Text(modelName).lineLimit(1).truncationMode(.tail)
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
