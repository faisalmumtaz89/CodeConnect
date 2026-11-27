import AppKit
import CodeConnectKit
import SwiftUI

/// What a run's Changes tab lets a hunk do, as the iPhone's hunk menu does: comment to the run's agent about it, copy
/// it, copy its file's path. Given to the tab's diffs through the environment; a diff with none (an approval card's)
/// draws its hunk headers as plain lines. One comment field is open at a time, under the header it was asked from.
@Observable @MainActor final class HunkActions {
    let model: AppModel
    let key: String
    /// The hunk whose comment field is open (`HunkActions.id`).
    var commenting: String?
    /// The hunk whose comment just reached the agent, said under its header for a few seconds.
    var sent: String?

    init(model: AppModel, key: String) {
        self.model = model
        self.key = key
    }

    static func id(file: UnifiedDiff.FileDiff, hunk: UnifiedDiff.Hunk) -> String { "\(file.id)#\(hunk.id)" }

    /// A run that takes no reply can't be commented to; its hunks can still be copied.
    var comments: Bool { model.shownSummary(for: key)?.takesNoReply != true }

    /// The run's agent by name, as its reply box names it.
    var agent: String { model.shownSummary(for: key)?.agent.displayName ?? "the agent" }

    func items(file: UnifiedDiff.FileDiff, hunk: UnifiedDiff.Hunk, id: String) -> [RowMenu.Item] {
        let hunkID = Self.id(file: file, hunk: hunk)
        var items: [RowMenu.Item] = []
        if comments {
            items.append(RowMenu.Item(
                id: "\(id).comment", title: "Comment to \(agent)…", symbol: PanelSymbol.comment, destructive: false
            ) { [self] in
                sent = nil
                commenting = hunkID
            })
        }
        items.append(RowMenu.Item(
            id: "\(id).copyHunk", title: "Copy Hunk", symbol: PanelSymbol.copy, destructive: false
        ) {
            copyToPasteboard(hunk.text)
        })
        items.append(RowMenu.Item(
            id: "\(id).copyPath", title: "Copy File Path", symbol: PanelSymbol.copy, destructive: false
        ) {
            copyToPasteboard(file.displayPath)
        })
        return items
    }
}

/// A hunk's `@@ … @@` line. With actions, a "…" in its trailing slot while the pointer is on it (the slot is kept
/// when it is empty, so nothing moves), and a right-click on the line, open the window's row menu with the hunk's
/// acts.
struct HunkHeader: View {
    let file: UnifiedDiff.FileDiff
    let hunk: UnifiedDiff.Hunk
    let actions: HunkActions

    @Environment(RowMenu.self) private var menu
    @State private var hovered = false
    @State private var frame: CGRect = .zero

    private var id: String { "changes.hunk.\(HunkActions.id(file: file, hunk: hunk))" }
    private var open: Bool { menu.shown?.owner == id }

    var body: some View {
        Text(hunk.header.displaySafe).font(Theme.mono(11)).foregroundStyle(Theme.text3)
            .padding(.leading, 12)
            .padding(.trailing, 12 + RunMore.width)
            .padding(.vertical, 3)
            .frame(maxWidth: .infinity, alignment: .leading)
            .overlay(alignment: .trailing) {
                // The dots' ink ends on the file header's +N −M above (12 pt in); their plate stands 6 pt in.
                if hovered || open || HoverButtonStyle.forced(id) { more.padding(.trailing, 6) }
            }
            .contentShape(Rectangle())
            .onHover { hovered = $0 }
            .onRightClick { point in show(at: CGRect(origin: point, size: .zero)) }
            #if DEBUG
                .task {
                    // A render's `-CC_HUNK` acts on the diff's first hunk, a moment after it is placed.
                    guard let act = RenderWindow.hunk, !RenderWindow.hunkActed, hunk.id.hasSuffix("-0"),
                        file.hunks.first?.id == hunk.id
                    else { return }
                    RenderWindow.hunkActed = true
                    try? await Task.sleep(for: .seconds(1))
                    let hunkID = HunkActions.id(file: file, hunk: hunk)
                    switch act {
                    case "menu":
                        // As a click on the dots: drawn as the pointer draws them, measured, then opened from them.
                        hovered = true
                        try? await Task.sleep(for: .milliseconds(300))
                        show(at: frame)
                    case "sent": actions.sent = hunkID
                    default: actions.commenting = hunkID
                    }
                }
            #endif
    }

    private var more: some View {
        Button { show(at: frame) } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Theme.text2)
                .frame(width: RunMore.width, height: 18)
                .background(open ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: 5))
                .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: "\(id).more"))
        .help("More")
        .accessibilityLabel("Hunk actions")
        .accessibilityIdentifier("\(id).more")
        .accessibilityValue(open ? "open" : "closed")
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { frame = $0 }
    }

    private func show(at rect: CGRect) {
        menu.toggle(rect, owner: id, items: actions.items(file: file, hunk: hunk, id: id))
    }
}

/// The comment field under a hunk's header, in the reply box's style: the file and lines it is about over the field,
/// Return sends it to the run's agent (⇧ or ⌥ Return adds a line), Esc closes it. Whether it can be sent, and what
/// became of it, are the Kit's words for the run, as under the reply box. Once it reaches the agent the field closes;
/// a refusal keeps the words.
struct HunkCommentField: View {
    let file: UnifiedDiff.FileDiff
    let hunk: UnifiedDiff.Hunk
    let actions: HunkActions

    @State private var draft = ""
    @State private var sending = false
    /// What became of this field's last Claude send.
    @State private var result: ComposeAttempt?
    /// This field has sent, so the run's last outcome is its own.
    @State private var hasSent = false
    @FocusState private var focused: Bool

    private var model: AppModel { actions.model }
    private var key: String { actions.key }
    /// The reply box's files are not this comment's.
    private var blocked: String? { model.sendBlockedReason(for: key, draft: draft, files: false) }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(ChangesWords.commentPlace(file: file, hunk: hunk).displaySafe)
                .font(Theme.mono(11)).foregroundStyle(Theme.text3)
                .lineLimit(1).truncationMode(.middle)
            TextField(
                "", text: $draft, prompt: Text(ChangesWords.commentPrompt(actions.agent)).foregroundStyle(Theme.text3),
                axis: .vertical
            )
                .textFieldStyle(.plain)
                .font(Theme.ui(13))
                .foregroundStyle(Theme.text)
                .lineLimit(1...5)
                .sendsOnReturn(send)
                // It appears because Comment to <agent>… was just chosen: what is typed next is the comment.
                .takesKeyboardWhenShown($focused)
                .onExitCommand { actions.commenting = nil }
                .accessibilityLabel(ChangesWords.commentPrompt(actions.agent))
                .accessibilityIdentifier("changes.comment.field")
                .padding(10)
                .background {
                    RoundedRectangle(cornerRadius: 8).fill(Theme.raised)
                    RoundedRectangle(cornerRadius: 8).strokeBorder(Color.white.opacity(0.09))
                }
            if let note { KitNote(text: note.text, tone: note.tone, id: "changes.comment.note") }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .focusedValue(\.composerHoldsEscape, focused ? true : nil)
        .focusedValue(\.changesFieldFocused, focused ? true : nil)
        #if DEBUG
            .task {
                // A render's words and refusal, typed after the field has the keyboard, as a person's are.
                guard RenderWindow.hunkDraft != nil || RenderWindow.hunkRefusal != nil else { return }
                try? await Task.sleep(for: .milliseconds(300))
                draft = RenderWindow.hunkDraft ?? draft
                try? await Task.sleep(for: .milliseconds(100))
                result = RenderWindow.hunkRefusal.map(ComposeAttempt.refused)
                hasSent = result != nil
            }
        #endif
        .onChange(of: draft) { if result?.clearsOnEdit == true { result = nil } }
    }

    /// The last send's outcome, once this field has sent; else why sending cannot work.
    private var note: ComposeProse.Note? {
        if sending { return nil }
        if hasSent, let outcome = model.outcomeNote(for: key, lastSend: result) { return outcome }
        return model.standingNote(for: key, draft: draft)
    }

    private func send() {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard blocked == nil, !sending, !text.isEmpty else { return }
        let message = ChangesWords.comment(text, file: file, hunk: hunk)
        let hunkID = HunkActions.id(file: file, hunk: hunk)
        sending = true
        result = nil
        hasSent = true
        Task {
            let landed: Bool
            if model.shownSummary(for: key)?.isCodex == true {
                landed = await model.composeToCodex(sessionKey: key, text: message)?.landed == true
            } else {
                let attempt = await model.send(text: message, to: key)
                result = attempt
                landed = attempt.landed
            }
            sending = false
            guard landed else { return }
            actions.commenting = nil
            actions.sent = hunkID
            try? await Task.sleep(for: .seconds(4))
            if actions.sent == hunkID { actions.sent = nil }
        }
    }
}

/// One Kit note under a box that sends to an agent (the reply box, a hunk's comment field): its tone's mark, then its
/// words.
struct KitNote: View {
    let text: String
    let tone: ComposeProse.Tone
    let id: String

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            if let symbol = tone.symbol {
                Image(systemName: symbol).foregroundStyle(tone.color).accessibilityHidden(true)
            }
            Text(text).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
        }
        .font(Theme.ui(12))
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier(id)
    }
}
