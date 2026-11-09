import CodeConnectKit
import SwiftUI

// What the New session steps are built from: a recent folder's row, the
// search field, the first-message pill and the title of the screen a start
// lands on. What a start knows about its computer, and the rules the steps
// follow, are Kit's (`StartContext`).

// MARK: - Project row

/// A project: its name, its folder cut from the front, and its branch, age
/// and last agent, with a caret, since a tap moves on.
struct StartProjectRow: View {
    let project: StartProject

    var body: some View {
        HStack(alignment: .center, spacing: CC.space.sm) {
            VStack(alignment: .leading, spacing: CC.space.xxs) {
                Text(project.name)
                    .ccType(CC.type.rowTitle)
                    .foregroundStyle(CC.text.primary)
                    .fixedSize(horizontal: false, vertical: true)
                // One line at every size: the name above already says the
                // tail, and wrapped at AX5 the path made a row half a screen.
                Text(verbatim: project.shownPath)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
                    .lineLimit(1)
                    .truncationMode(.head)
                Text(startProjectMeta(project))
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            CCIcon("caret-right", size: 16).foregroundStyle(CC.text.tertiary)
        }
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, CC.space.sm)
        .frame(minHeight: CC.size.rowMin)
        .contentShape(Rectangle())
    }
}

/// A whole row as one target, lightening one step under a finger.
struct StartRowStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(configuration.isPressed ? CC.color.surfaceRaised : Color.clear)
    }
}

// MARK: - Search

/// A field that narrows a project list by name or folder.
struct StartSearchField: View {
    @Binding var text: String
    let placeholder: String

    var body: some View {
        HStack(spacing: CC.space.xs) {
            CCIcon("magnifying-glass", size: 18)
                .foregroundStyle(CC.text.tertiary)
            field
                .ccType(CC.type.body)
                .foregroundStyle(CC.text.primary)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .submitLabel(.search)
        }
        .padding(.horizontal, CC.space.sm)
        .frame(minHeight: CC.size.controlMd)
        .background(CC.color.surfaceRaised, in: RoundedRectangle(cornerRadius: CC.radius.lg))
        .overlay {
            RoundedRectangle(cornerRadius: CC.radius.lg)
                .strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline)
        }
    }

    private var field: some View {
        TextField("", text: $text, prompt: Text(placeholder).foregroundStyle(CC.text.placeholder))
            .accessibilityIdentifier("start-search")
    }
}

// MARK: - First message

/// The composer's own pill, for the first message: the field, and one circle
/// at its end that is the mic while the field is empty.
struct StartMessagePill: View {
    @Binding var text: String
    let placeholder: String
    var focused: FocusState<Bool>.Binding
    /// Drawn in the circle once text exists; nil keeps the mic.
    var onSend: (() -> Void)?

    @State private var dictation = DictationController()
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        Group {
            if dictation.isRecording {
                Text(dictation.transcript.isEmpty ? "Listening…" : dictation.transcript)
                    .ccType(CC.type.body)
                    .foregroundStyle(CC.text.tertiary)
                    .frame(maxWidth: .infinity, alignment: .topLeading)
            } else {
                field
            }
        }
        .padding(.leading, CC.space.sm)
        .padding(.trailing, CC.size.hitTarget + CC.space.xxs)
        .padding(.vertical, CC.space.xs)
        .frame(minHeight: CC.size.controlSm + 2 * CC.space.xs)
        .frame(maxWidth: .infinity, alignment: .leading)
        .overlay(alignment: .bottomTrailing) {
            CCVoiceButton(phase: phase) { act() }
                .accessibilityIdentifier("start-message-button")
                .padding(CC.space.xxs)
        }
        .background {
            RoundedRectangle(cornerRadius: (CC.size.controlSm + 2 * CC.space.xs) / 2, style: .continuous)
                .fill(CC.color.surfaceOverlay)
                .overlay {
                    RoundedRectangle(
                        cornerRadius: (CC.size.controlSm + 2 * CC.space.xs) / 2, style: .continuous
                    )
                    .strokeBorder(CC.color.borderStrong, lineWidth: CC.stroke.hairline)
                }
        }
        .onDisappear { dictation.cancel() }
    }

    private var field: some View {
        TextField(
            "", text: $text, prompt: Text(placeholder).foregroundStyle(CC.text.placeholder),
            axis: .vertical
        )
        // One line at the accessibility sizes: a second one grew under the
        // keyboard, taking the line being typed and the send with it.
        .lineLimit(typeSize.isAccessibilitySize ? 1...1 : 1...5)
        .ccType(CC.type.body)
        .foregroundStyle(CC.text.primary)
        .accessibilityIdentifier("start-message")
        .focused(focused)
    }

    private var phase: CCVoiceButtonPhase {
        if dictation.isRecording || dictation.isStarting { return .stop }
        return (text.isEmpty || onSend == nil) ? .dictate : .send
    }

    private func act() {
        if dictation.isRecording || dictation.isStarting {
            let heard = dictation.stop()
            if !heard.isEmpty { text = text.isEmpty ? heard : text + " " + heard }
        } else if !text.isEmpty, let onSend {
            onSend()
        } else {
            Task { await dictation.start() }
        }
    }
}

// MARK: - Bar title

/// The session screen's bar title, for a session that does not exist yet:
/// the project over its folder, set as the session's own bar sets them, so
/// nothing moves when the session takes over.
struct StartBarTitle: View {
    let start: SessionStart

    private var project: StartProject { start.project }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(verbatim: project.name)
                .ccType(CC.type.headline)
                .foregroundStyle(CC.text.primary)
                .lineLimit(1)
                .truncationMode(.tail)
            Text(verbatim: start.folder ?? "\(project.path) · new worktree")
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)
                .lineLimit(1)
                .truncationMode(.head)
        }
        .dynamicTypeSize(...DynamicTypeSize.xxxLarge)
    }
}
