import Foundation

/// Turning an agent's markdown into what a phone timeline can honestly render.
///
/// The ceiling is deliberate: **inline styling plus fenced code**, nothing
/// more. Bold, italics, inline code and links are what agents actually use in
/// running prose; fences carry the one thing that must never be reflowed. A
/// full block renderer — headings, tables, nested lists — is layout machinery
/// a timeline row does not want, and anything it would not render stays
/// readable as the literal text the agent wrote.
enum AgentProse {

    enum Segment: Equatable {
        case prose(String)
        /// A fenced block's body, exactly as written; the fence lines and the
        /// optional language tag are the wrapper, not the content.
        case code(String)
    }

    /// Split on fence *delimiter lines* only: a line consisting of ``` with an
    /// optional language tag. A fence that never closes is not treated as a
    /// fence at all — the remainder renders as the literal text it is, because
    /// guessing at what an unterminated block "meant" is how content vanishes.
    static func segments(_ text: String) -> [Segment] {
        var prose: [String] = []
        var out: [Segment] = []
        /// The open fence's body — and its *original delimiter line*, kept so
        /// an unclosed fence can fall back to exactly the text the agent
        /// wrote, language tag and all, in one uninterrupted prose run.
        var fence: (opener: String, body: [String])?

        func flushProse() {
            let joined = prose.joined(separator: "\n")
            if !joined.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                out.append(.prose(joined))
            }
            prose = []
        }

        for line in text.split(separator: "\n", omittingEmptySubsequences: false) {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            let isDelimiter =
                trimmed.hasPrefix("```")
                && !trimmed.dropFirst(3).contains("`")
            if isDelimiter {
                if let open = fence {
                    // Only a *close* commits anything: the prose before the
                    // fence flushes here, so an unclosed fence leaves it
                    // joined with the fence text as the single verbatim run
                    // it originally was.
                    flushProse()
                    out.append(.code(open.body.joined(separator: "\n")))
                    fence = nil
                } else {
                    fence = (opener: String(line), body: [])
                }
            } else if fence != nil {
                fence!.body.append(String(line))
            } else {
                prose.append(String(line))
            }
        }
        if let open = fence {
            prose.append(open.opener)
            prose.append(contentsOf: open.body)
        }
        flushProse()
        return out
    }

    /// Inline markdown, whitespace preserved — `**bold**` becomes bold instead
    /// of asterisks, line breaks and list markers survive as written. Malformed
    /// markdown falls back to the literal text; rendering something is always
    /// better than throwing about it.
    static func inline(_ text: String) -> AttributedString {
        let options = AttributedString.MarkdownParsingOptions(
            interpretedSyntax: .inlineOnlyPreservingWhitespace)
        return (try? AttributedString(markdown: text, options: options))
            ?? AttributedString(text)
    }
}
