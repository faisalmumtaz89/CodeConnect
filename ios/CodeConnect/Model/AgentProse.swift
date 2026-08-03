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
        /// A heading line's text, markers stripped. One case for all six
        /// levels: a phone timeline gets one heading style, and a size ladder
        /// per `#` count would be typography for a document this is not.
        case heading(String)
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
            } else if let heading = headingText(trimmed) {
                // Its own segment, so the markers never reach a renderer. Only
                // outside fences: a `# comment` inside code is code.
                flushProse()
                out.append(.heading(heading))
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

    /// `## Title` -> `Title`; nil when the line is not a heading. CommonMark's
    /// shape: one to six `#`, then whitespace — `#nospace` and seven hashes are
    /// literal text and stay that way.
    private static func headingText(_ trimmed: String) -> String? {
        let hashes = trimmed.prefix(while: { $0 == "#" })
        guard (1...6).contains(hashes.count) else { return nil }
        let rest = trimmed.dropFirst(hashes.count)
        guard let first = rest.first, first == " " || first == "\t" else { return nil }
        let text = rest.trimmingCharacters(in: .whitespaces)
        return text.isEmpty ? nil : text
    }

    /// The collapsed preview's source: the same text with heading markers
    /// stripped (outside fences), so the four-line preview shows "How it's
    /// useful" and not "## How it's useful" — the artifact, smaller.
    static func previewSource(_ text: String) -> String {
        segments(text)
            .map { segment in
                switch segment {
                case .prose(let prose): return prose
                case .heading(let heading): return heading
                case .code(let code): return code
                }
            }
            .joined(separator: "\n")
    }

    /// Inline markdown, whitespace preserved — `**bold**` becomes bold instead
    /// of asterisks, line breaks and list markers survive as written. Malformed
    /// markdown falls back to the literal text; rendering something is always
    /// better than throwing about it.
    static func inline(_ text: String) -> AttributedString {
        let options = AttributedString.MarkdownParsingOptions(
            interpretedSyntax: .inlineOnlyPreservingWhitespace)
        let safe = neutralizeLoneTildes(text)
        return (try? AttributedString(markdown: safe, options: options))
            ?? AttributedString(text)
    }

    /// Escape the tildes that mean "approximately" so they cannot mean
    /// "delete this".
    ///
    /// Measured on a real message: `~60 seconds` at one end and `~$25–40` two
    /// paragraphs later — Apple's parser paired the two lone tildes as
    /// `~strikethrough~` and struck 1,790 characters between them. The rules,
    /// each load-bearing:
    ///   * a `~` with another `~` beside it is left alone — `~~text~~` is the
    ///     legitimate strikethrough an agent occasionally means;
    ///   * anything inside a backtick span is untouched. Inline code is where
    ///     `~/.codeconnect` lives, and an escape there renders as a literal
    ///     backslash in the path. Spans are matched by *run length*, per
    ///     CommonMark: `` `a` `` opens with one backtick, ``` ``x`` ``` with
    ///     two, and only an equal run closes;
    ///   * a `~` already escaped stays singly escaped.
    static func neutralizeLoneTildes(_ text: String) -> String {
        var out = String()
        out.reserveCapacity(text.count + 8)
        let chars = Array(text)
        var i = 0
        var codeFenceRun = 0
        while i < chars.count {
            let c = chars[i]
            if c == "`" {
                var run = 0
                while i + run < chars.count, chars[i + run] == "`" { run += 1 }
                if codeFenceRun == 0 {
                    codeFenceRun = run
                } else if run == codeFenceRun {
                    codeFenceRun = 0
                }
                out.append(String(repeating: "`", count: run))
                i += run
                continue
            }
            if c == "~", codeFenceRun == 0 {
                let prev = i > 0 ? chars[i - 1] : " "
                let next = i + 1 < chars.count ? chars[i + 1] : " "
                if prev != "~", next != "~", prev != "\\" {
                    out.append("\\~")
                    i += 1
                    continue
                }
            }
            out.append(c)
            i += 1
        }
        return out
    }
}
