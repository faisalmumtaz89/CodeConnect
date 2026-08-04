import Foundation

/// The transcript's local-command grammar, parsed.
///
/// When a built-in slash command runs — typed at the Mac or injected from
/// this phone — Claude Code records it as up to three `type:"user"` lines
/// whose content is XML-ish markup, not prose: a boilerplate caveat block, a
/// `<command-name>` invocation, and a `<local-command-stdout>` result wrapped
/// in raw ANSI styling. Measured across 481 real invocations in the owner's
/// transcripts; exactly these three shapes occur, nothing else. Rendered
/// literally they read as markup soup under a "YOU" label — so the timeline
/// parses them into what they are: the command you ran, and what Claude Code
/// said back.
enum LocalCommandLine: Equatable {
    /// `/model sonnet` — the command as the user issued it.
    case invocation(command: String, args: String)
    /// Claude Code's own one-liner back, ANSI stripped.
    case output(String)
    /// The caveat boilerplate ("DO NOT respond to these messages…") — meta
    /// addressed to the model, not something a human said.
    case caveat

    /// Recognises one transcript content string, or returns nil for ordinary
    /// prose. Recognition is by leading tag, so a user who *mentions*
    /// `<command-name>` mid-sentence is never rewritten.
    static func parse(_ content: String) -> LocalCommandLine? {
        let trimmed = content.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.hasPrefix("<local-command-caveat>") {
            return .caveat
        }
        if trimmed.hasPrefix("<local-command-stdout>") {
            let inner = innerText(of: "local-command-stdout", in: trimmed)
            let clean = stripANSI(inner).trimmingCharacters(in: .whitespacesAndNewlines)
            return .output(clean)
        }
        if trimmed.hasPrefix("<command-name>") {
            let command = innerText(of: "command-name", in: trimmed)
                .trimmingCharacters(in: .whitespacesAndNewlines)
            let args = innerText(of: "command-args", in: trimmed)
                .trimmingCharacters(in: .whitespacesAndNewlines)
            guard !command.isEmpty else { return nil }
            return .invocation(command: command, args: args)
        }
        return nil
    }

    /// The invocation as one line: `/model sonnet`, or just `/model`.
    var invocationText: String? {
        guard case .invocation(let command, let args) = self else { return nil }
        return args.isEmpty ? command : "\(command) \(args)"
    }

    /// Text between `<name>` and `</name>`; an unclosed tag yields the rest
    /// of the string rather than nothing — rendering something is always
    /// better than losing content to malformed markup.
    private static func innerText(of name: String, in text: String) -> String {
        guard let open = text.range(of: "<\(name)>") else { return "" }
        let after = text[open.upperBound...]
        guard let close = after.range(of: "</\(name)>") else { return String(after) }
        return String(after[..<close.lowerBound])
    }

    /// Removes CSI escape sequences — `ESC [`, parameter and intermediate
    /// bytes, then one final byte in `0x40...0x7E` per ECMA-48. The final
    /// byte is a *range*, not "a letter": `ESC [ 2 ~` ends at `~`, and a
    /// letter-based scan would eat the first real character after it.
    /// A malformed sequence (non-ASCII before any final byte) stops the
    /// strip and leaves the rest visible — losing styling is fine, losing
    /// content is not.
    static func stripANSI(_ text: String) -> String {
        var out = String()
        out.reserveCapacity(text.count)
        var index = text.startIndex
        while index < text.endIndex {
            let character = text[index]
            if character == "\u{1B}" {
                var cursor = text.index(after: index)
                if cursor < text.endIndex, text[cursor] == "[" {
                    cursor = text.index(after: cursor)
                    // Only ECMA-48 body bytes may be skipped: parameters
                    // 0x30-0x3F and intermediates 0x20-0x2F. Anything else —
                    // a newline, a control byte — is a malformed sequence,
                    // and the strip stops in front of it rather than
                    // swallowing real content on the way to a fake final.
                    while cursor < text.endIndex,
                        let ascii = text[cursor].asciiValue,
                        (0x20...0x3F).contains(ascii)
                    {
                        cursor = text.index(after: cursor)
                    }
                    if cursor < text.endIndex, let ascii = text[cursor].asciiValue,
                        (0x40...0x7E).contains(ascii)
                    {
                        cursor = text.index(after: cursor)
                    }
                    index = cursor
                    continue
                }
            }
            out.append(character)
            index = text.index(after: index)
        }
        return out
    }
}
