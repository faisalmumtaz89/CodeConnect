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
            return "Verified - this is the exact text the daemon hashed."
        }
    }

    /// The gate's own hash of `display_text`, exposed so nothing has to
    /// re-implement it. A second copy of this expression in a test helper is a
    /// copy that agrees with itself by construction.
    var sha256OfDisplayText: String {
        SHA256.hash(data: Data(displayText.utf8)).map { String(format: "%02x", $0) }.joined()
    }

    var verification: Verification {
        let hex = sha256OfDisplayText
        return Verification(
            hashMatchesDisplayText: hex == payloadHash,
            renderMatchesDisplayText: "\(toolName)\n\(toolInput.canonicalJSONString)" == displayText)
    }

    /// **The pending card's promise, and the one card that must not make it.**
    ///
    /// "Nothing decides this but you. There is no timer on this card." is the
    /// card's standing promise and it survives every redesign — on a card that
    /// can be answered. On one that cannot, it is false in the most misleading
    /// direction available: something other than the reader has already decided
    /// that the reader does not decide, and a reader who believes the sentence
    /// waits at a control that is never coming. It was drawn directly above
    /// "it cannot be answered from here", so the card said both at once.
    ///
    /// A function rather than a `let`, so the condition is stated once, next to
    /// the words, and can be asserted without standing up a `View`.
    static func pendingPromise(isAnswerable: Bool) -> String? {
        guard isAnswerable else { return nil }
        return "Nothing decides this but you. There is no timer on this card."
    }

    /// What a Codex card says when the phone cannot vouch for its rendering.
    /// One sentence, and never the daemon's `tool\n{JSON}`.
    static let unverifiableCodexLine =
        "This card could not be verified on the phone. Answer it at the Mac."

    /// The single line that identifies what will run.
    ///
    /// Falls back to the exact hashed text whenever the structured parse cannot
    /// be vouched for — **for Claude**, whose `display_text` is the tool's own
    /// arguments and is what its reader is checking.
    ///
    /// For Codex, and for a card whose agent nobody knows, that fallback was a
    /// raw-JSON hole: `display_text` there is literally `"{tool}\n{input}"`, so
    /// an unverifiable card printed the wire onto the one product screen that
    /// must never show it. Those get one sentence instead, and the card is
    /// not answerable (see `DecisionCardView.answerSurface`) — a rendering
    /// nobody can vouch for is not a question anybody should answer from here.
    func primaryText(verification: Verification, agent: AgentKind?) -> String {
        guard verification.renderMatchesDisplayText,
            let argument = ToolSummary.principalArgument(tool: toolName, input: toolInput)
        else { return agent == .claude ? displayText : Self.unverifiableCodexLine }
        return argument
    }
}
