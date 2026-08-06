import SwiftUI

// =============================================================================
//  A snapshot of the Mac's screen — `/status`, `/usage`, `/cost`.
//
//  These three commands open one Settings dialog on the Mac that replaces
//  Claude's composer. The daemon types the command, waits out the measured
//  settling window, saves the pane while the view is up, presses Esc, and
//  proves the composer returned — and this sheet renders exactly what was
//  saved. Nothing is parsed, extracted, or interpreted: the only judgment
//  applied to the capture is empty versus non-empty. Snapshots are sheet
//  state and nothing else — never cached, never in the timeline, never
//  refreshed behind the reader's back.
// =============================================================================

struct SnapshotSheet: View {
    let sessionKey: String
    let command: SnapshotCommand
    /// The keystrokes landed on the Mac — the composer draft that opened
    /// this sheet has been consumed and may be cleared.
    var onLanded: () -> Void = {}
    /// "Open Terminal" — the recovery path when Esc did not bring the
    /// composer back. Dismisses this sheet and lands on the Terminal tab.
    var onOpenTerminal: () -> Void = {}

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    private enum Phase: Equatable {
        case capturing
        /// The daemon returned the saved pane and proved the composer came
        /// back. The strings are verbatim wire facts.
        case captured(pane: String, capturedAt: String)
        /// Recovered, but the saved pane trimmed to nothing.
        case emptySnapshot
        /// Typed, and the composer never left — there was no view to save.
        case noView
        /// Typed, Escape sent, composer not proved back. The one state
        /// with no retry: sending more keys into an unknown Mac screen is
        /// how a rescue becomes damage.
        case lost
        /// The daemon recognised a replay of an earlier capture whose
        /// response never reached this phone. The snapshot was not
        /// persisted anywhere, so only a fresh capture can show one.
        case duplicate
        case failed(String)
    }

    @State private var phase: Phase = .capturing

    var body: some View {
        CCSheetChrome(command.title, subtitle: "/\(command.rawValue)", onClose: { dismiss() }) {
            ScrollView {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    content
                }
                .padding(CC.space.md)
            }
            .scrollBounceBehavior(.basedOnSize)
        }
        .task { await capture() }
    }

    @ViewBuilder
    private var content: some View {
        switch phase {
        case .capturing:
            HStack(spacing: CC.space.sm) {
                CCProgressRing(.sm)
                Text("Typing /\(command.rawValue) on the Mac…")
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.vertical, CC.space.lg)

        case .captured(let pane, let capturedAt):
            CCSectionHeader("The Mac’s screen when /\(command.rawValue) opened")
            Text(Self.takenLine(capturedAt: capturedAt))
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
            // The Mac's pane is 80 to 200 columns; a phone is not. Saying so
            // is the difference between a reader trusting the layout and a
            // reader trusting the text: a `↳` that arrives unexplained looks
            // like the capture went wrong, when it is the one thing keeping
            // the tail of a long line visible at all.
            Text("Wrapped to fit this screen — ↳ continues the line above.")
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)
            // `isSmall` because this is a *screen*, not a command: the Mac's
            // pane is 80 to 200 columns of grid, and every column the phone
            // fits before wrapping is a row of that grid kept intact. The
            // wrapping itself stays — `CCMonoBlock` marks a break with `↳`
            // rather than hiding the tail, which is the rule that matters
            // more here than tidiness.
            CCMonoBlock(pane, lineLimit: 16, isSmall: true)
            Text("CodeConnect pressed Esc and confirmed the composer returned.")
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)
            CCButton("Capture again", variant: .secondary, size: .sm) {
                Task { await capture() }
            }

        case .emptySnapshot:
            notice(
                title: "No readable snapshot",
                message:
                    "The Mac view was closed and the composer is ready, "
                    + "but the captured pane contained no readable text.")
            HStack(spacing: CC.space.xs) {
                CCButton("Try again", variant: .secondary, size: .sm) {
                    Task { await capture() }
                }
                CCButton("Open Terminal", variant: .ghost, size: .sm) { onOpenTerminal() }
            }

        case .noView:
            notice(
                title: "No Mac view appeared",
                message:
                    "/\(command.rawValue) was typed, but CodeConnect did not observe "
                    + "a view to capture. The composer is still ready.")
            CCButton("Try again", variant: .secondary, size: .sm) {
                Task { await capture() }
            }

        case .lost:
            notice(
                title: "Couldn’t restore the composer",
                message:
                    "The command was typed and Escape was sent, but the composer "
                    + "did not return. Open Terminal before sending anything else.")
            CCButton("Open Terminal", variant: .secondary, size: .sm) { onOpenTerminal() }

        case .duplicate:
            notice(
                title: "Captured earlier",
                message:
                    "That capture ran earlier, but its snapshot did not reach "
                    + "this phone. Capture again.")
            CCButton("Capture again", variant: .secondary, size: .sm) {
                Task { await capture() }
            }

        case .failed(let reason):
            notice(title: "Nothing was captured", message: reason)
            CCButton("Try again", variant: .secondary, size: .sm) {
                Task { await capture() }
            }
        }
    }

    private func notice(title: String, message: String) -> some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            Text(title)
                .ccType(CC.type.headline)
                .foregroundStyle(CC.text.primary)
            CCProse(message, style: CC.type.footnote, color: CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.top, CC.space.sm)
    }

    /// "Not live. A snapshot taken at 14:32:08." — the caption that keeps a
    /// static capture from impersonating a live view. When the daemon's
    /// timestamp cannot be parsed the claim shrinks with the knowledge.
    static func takenLine(capturedAt: String) -> String {
        guard let date = ISO8601.parse(capturedAt) else {
            return "Not live. A snapshot from a moment ago."
        }
        let formatter = DateFormatter()
        formatter.dateFormat = "HH:mm:ss"
        return "Not live. A snapshot taken at \(formatter.string(from: date))."
    }

    private func capture() async {
        withAnimation(CC.motion.micro) { phase = .capturing }
        let attempt = await model.sendSnapshotCommand(command, to: sessionKey)
        let next: Phase
        switch attempt {
        case .composerRecovered(_, let paneSnapshot, let capturedAt):
            let trimmed = paneSnapshot?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            if trimmed.isEmpty {
                next = .emptySnapshot
            } else {
                // The untrimmed pane is what renders: the trim is a
                // readability *test*, not a rewrite of the capture.
                next = .captured(pane: paneSnapshot ?? "", capturedAt: capturedAt)
            }
            onLanded()
        case .sent:
            next = .noView
            onLanded()
        case .alreadyApplied:
            next = .duplicate
            onLanded()
        case .composerLost:
            next = .lost
        case .refused(let reason), .failed(let reason), .indeterminate(let reason):
            next = .failed(reason)
        }
        withAnimation(CC.motion.small) { phase = next }
    }
}
