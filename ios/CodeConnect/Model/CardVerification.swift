import CryptoKit
import Foundation

extension ApprovalCard {
    /// Whether the text on screen is provably the text the daemon hashed.
    ///
    /// Two independent checks, because they fail for different reasons:
    ///   * `hashMatchesDisplayText` — SHA-256 of `display_text` against
    ///     `payload_hash`. Exact, with no canonicalisation involved, so this is
    ///     the check that gates the UI.
    ///   * `renderMatchesDisplayText` — our own `"{tool}\n{input}"` rendering
    ///     against `display_text`. Proves the *structured* fields being shown
    ///     are the hashed ones; if it fails we fall back to showing
    ///     `display_text` verbatim rather than a prettier version we cannot
    ///     vouch for.
    struct Verification: Sendable, Hashable {
        var hashMatchesDisplayText: Bool
        var renderMatchesDisplayText: Bool

        var summary: String {
            if !hashMatchesDisplayText {
                return "This card's text does not match its hash."
            }
            if !renderMatchesDisplayText {
                return "Verified against the hash. Showing the daemon's exact text."
            }
            return "Verified — this is the exact text the daemon hashed."
        }
    }

    var verification: Verification {
        let digest = SHA256.hash(data: Data(displayText.utf8))
        let hex = digest.map { String(format: "%02x", $0) }.joined()
        return Verification(
            hashMatchesDisplayText: hex == payloadHash,
            renderMatchesDisplayText: "\(toolName)\n\(toolInput.canonicalJSONString)" == displayText)
    }

    /// The single line that identifies what will run. Falls back to the exact
    /// hashed text whenever the structured parse cannot be vouched for.
    func primaryText(verification: Verification) -> String {
        guard verification.renderMatchesDisplayText,
            let argument = ToolSummary.principalArgument(tool: toolName, input: toolInput)
        else { return displayText }
        return argument
    }
}

/// Claude's option list, read off a `capture-pane` snapshot.
///
/// Screen text is never used for *state* — that is the rule the architecture is
/// built around. It is used here for exactly one thing: knowing
/// which number to type, so `Option{index}` can be offered instead of guessing
/// that "2" means the same thing in every prompt.
enum PaneOptions {
    struct Option: Sendable, Hashable, Identifiable {
        let index: UInt32
        let label: String
        var id: UInt32 { index }
    }

    /// How far from the bottom of the snapshot a list may sit and still be
    /// believed to be the live prompt.
    private static let promptWindowLines = 25

    static func parse(_ pane: String) -> [Option] {
        let lines = pane.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)

        var run: [Option] = []
        var runEnd = 0
        var lastRun: [Option] = []
        var lastRunEnd = 0

        func closeRun(at index: Int) {
            // The *last* list wins, never the longest. Claude's prompt is always
            // the bottom-most menu; a longer numbered list further up (release
            // notes, tips, ordinary command output) would otherwise be offered
            // as if it were the choices — the user would tap "2" believing it
            // means what "2" means on screen right now.
            if run.count >= 2 {
                lastRun = run
                lastRunEnd = index
            }
            run = []
        }

        for (index, line) in lines.enumerated() {
            if let option = option(in: line) {
                // Options must run consecutively from 1, or this is ordinary
                // numbered output that happens to look like a menu.
                if option.index == UInt32(run.count) + 1 {
                    run.append(option)
                    runEnd = index
                } else {
                    closeRun(at: runEnd)
                    run = option.index == 1 ? [option] : []
                    runEnd = index
                }
            } else if !run.isEmpty {
                // A blank line inside the box does not end the list.
                if line.trimmingCharacters(in: .whitespaces).isEmpty { continue }
                closeRun(at: runEnd)
            }
        }
        closeRun(at: runEnd)

        // A list far above the bottom is not the prompt we are answering. Better
        // to offer no numbered options — Allow/Deny still work — than to offer
        // numbers that mean something else.
        let lastContentLine =
            lines.lastIndex { !$0.trimmingCharacters(in: .whitespaces).isEmpty } ?? lines.count - 1
        guard !lastRun.isEmpty, lastContentLine - lastRunEnd <= promptWindowLines else { return [] }
        return lastRun
    }

    private static func option(in line: String) -> Option? {
        var scalars = Substring(line)
        // Strip box-drawing, the selection caret and leading space.
        scalars = scalars.drop {
            $0.isWhitespace || $0 == "│" || $0 == "|" || $0 == "❯" || $0 == ">" || $0 == "*"
        }
        let digits = scalars.prefix { $0.isNumber }
        guard !digits.isEmpty, digits.count <= 2, let index = UInt32(digits), index >= 1 else {
            return nil
        }
        var rest = scalars.dropFirst(digits.count)
        guard let separator = rest.first, separator == "." || separator == ")" else { return nil }
        rest = rest.dropFirst()
        guard rest.first?.isWhitespace == true else { return nil }
        let label = rest.trimmingCharacters(in: .whitespaces)
            .trimmingCharacters(in: CharacterSet(charactersIn: "│|"))
            .trimmingCharacters(in: .whitespaces)
        guard !label.isEmpty else { return nil }
        return Option(index: index, label: label)
    }
}
