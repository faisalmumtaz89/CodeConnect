import Foundation

/// A pipe table, parsed — headers, per-column alignment, and rows that are
/// all exactly `headers.count` wide, because the parser refuses ragged rows
/// rather than padding or silently discarding a cell the agent wrote.
struct MarkdownTable: Equatable {
    /// From the delimiter row's colons. `.leading` is also the no-colon
    /// default; on a phone there is no distinct "default" rendering to keep.
    enum Alignment: Equatable {
        case leading
        case center
        case trailing
    }

    /// Raw inline markdown — a renderer parses these like any prose.
    let headers: [String]
    /// One per column; count always equals `headers.count`.
    let alignments: [Alignment]
    /// Body rows, each exactly `headers.count` cells. Empty for a table that
    /// is only a header and its delimiter.
    let rows: [[String]]
    /// The source lines, verbatim, for anything that needs what was written
    /// rather than what was parsed.
    let raw: String
}

/// Turning an agent's markdown into what a phone timeline can honestly render.
///
/// The ceiling is deliberate: **inline styling, fenced code, headings and pipe
/// tables**, nothing more. Bold, italics, inline code and links are what
/// agents actually use in running prose; fences carry the one thing that must
/// never be reflowed; a table is the one structure whose meaning *is* its
/// geometry — `| pass | 3 |` reflowed into prose stops saying which number
/// belongs to which word. Anything else stays readable as the literal text
/// the agent wrote.
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
        /// A GFM-shaped pipe table. Parsed here — cells, alignments, widths —
        /// so rendering and VoiceOver read structure, not delimiter art.
        case table(MarkdownTable)
    }

    /// Split on fence *delimiter lines* only: a line consisting of ``` with an
    /// optional language tag. A fence that never closes is not treated as a
    /// fence at all — the remainder renders as the literal text it is, because
    /// guessing at what an unterminated block "meant" is how content vanishes.
    /// Precedence outside a fence: heading, then table candidate, then prose.
    static func segments(_ text: String) -> [Segment] {
        // CRLF is one grapheme to Swift: split on "\n" alone never divides a
        // CRLF message, and the whole text used to sail through as one prose
        // lump — measured, not theorized. Both terminators divide lines.
        let lines = text.split(
            omittingEmptySubsequences: false,
            whereSeparator: { $0 == "\n" || $0 == "\r\n" }
        ).map(String.init)
        var prose: [String] = []
        var out: [Segment] = []
        /// The open fence's body — and its *original delimiter line*, kept so
        /// an unclosed fence can fall back to exactly the text the agent
        /// wrote, language tag and all, in one uninterrupted prose run.
        var fence: (opener: String, body: [String])?
        /// Whether the next line sits on a block boundary: the start of the
        /// message, or just after a blank line, a heading, or a closed fence.
        /// A table header is recognized only there. The false positives this
        /// exists to exclude are pipe-bearing lines inside hard-wrapped prose;
        /// a heading or fence above a candidate is a block edge, not prose,
        /// and `### Results` directly over a table is how agents write them.
        var atBoundary = true

        func flushProse() {
            let joined = prose.joined(separator: "\n")
            if !joined.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                out.append(.prose(joined))
            }
            prose = []
        }

        var index = 0
        while index < lines.count {
            let line = lines[index]
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if fenceDelimiter(trimmed) {
                if let open = fence {
                    // Only a *close* commits anything: the prose before the
                    // fence flushes here, so an unclosed fence leaves it
                    // joined with the fence text as the single verbatim run
                    // it originally was.
                    flushProse()
                    out.append(.code(open.body.joined(separator: "\n")))
                    fence = nil
                    atBoundary = true
                } else {
                    fence = (opener: line, body: [])
                    atBoundary = false
                }
            } else if fence != nil {
                fence!.body.append(line)
            } else if let heading = headingText(trimmed) {
                // Its own segment, so the markers never reach a renderer. Only
                // outside fences: a `# comment` inside code is code.
                flushProse()
                out.append(.heading(heading))
                atBoundary = true
            } else if atBoundary, let (table, consumed) = parseTable(in: lines, from: index) {
                flushProse()
                out.append(.table(table))
                index += consumed
                atBoundary = false
                continue
            } else {
                prose.append(line)
                atBoundary = line.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            }
            index += 1
        }
        if let open = fence {
            prose.append(open.opener)
            prose.append(contentsOf: open.body)
        }
        flushProse()
        return out
    }

    // MARK: - Table grammar

    /// A conservative subset of GitHub's pipe tables:
    ///
    ///     table         := header newline delimiter (newline bodyRow)*
    ///     delimiterCell := ":"? "---"… ":"?      (three hyphens minimum)
    ///
    /// The header needs at least two columns, one of them non-empty, and a
    /// delimiter row of exactly the same width on the next line — `a | b` on
    /// its own is prose. Body rows must match the header's width exactly; the
    /// first blank, non-row, or wrong-width line ends the table *unconsumed*,
    /// so it and everything after it segment normally. Stricter than GFM,
    /// which pads short rows and drops excess cells — this product never
    /// silently discards a cell the agent wrote.
    private static func parseTable(
        in lines: [String], from start: Int
    ) -> (MarkdownTable, Int)? {
        guard start + 1 < lines.count,
            let headers = tableRowCells(lines[start]),
            headers.count >= 2,
            headers.contains(where: { !$0.isEmpty }),
            let alignments = delimiterAlignments(lines[start + 1]),
            alignments.count == headers.count
        else { return nil }

        var rows: [[String]] = []
        var next = start + 2
        while next < lines.count,
            !beginsAnotherBlock(lines[next]),
            let cells = tableRowCells(lines[next]),
            cells.count == headers.count
        {
            rows.append(cells)
            next += 1
        }
        let table = MarkdownTable(
            headers: headers,
            alignments: alignments,
            rows: rows,
            raw: lines[start..<next].joined(separator: "\n"))
        return (table, next - start)
    }

    /// A fence *delimiter line*: ``` plus an optional info string, no further
    /// backticks. One definition, used by the segment loop and the table
    /// body scan alike — precedence claims mean nothing if the two disagree.
    private static func fenceDelimiter(_ trimmed: String) -> Bool {
        trimmed.hasPrefix("```") && !trimmed.dropFirst(3).contains("`")
    }

    /// Whether a line opens a different block and therefore ends a table's
    /// body scan, unconsumed. A fence opener with a pipe in its info string
    /// (` ```swift | metadata `) or a heading with a pipe in its text
    /// (`# Result | Detail`) can be exactly wide enough to impersonate a
    /// body row — and block starts outrank table rows here exactly as they
    /// do in the segment loop, or the fence after them silently loses its
    /// code rendering.
    private static func beginsAnotherBlock(_ line: String) -> Bool {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        return fenceDelimiter(trimmed) || headingText(trimmed) != nil
    }

    /// One row line into trimmed cells; nil when the line cannot be a row at
    /// all — blank, tab-indented, indented four or more spaces (that is
    /// indented code), or bearing no unescaped pipe.
    ///
    /// Splitting follows GFM: at most one unescaped outer pipe comes off each
    /// end — leading and trailing independently optional — then unescaped
    /// pipes divide cells. A pipe is escaped when an odd-length run of
    /// backslashes precedes it; that holds inside code spans too, where GFM
    /// likewise requires `\|`. Cells are trimmed of spaces and tabs and
    /// otherwise preserved exactly, escapes included — the inline renderer is
    /// what turns `\|` back into `|`.
    private static func tableRowCells(_ rawLine: String) -> [String]? {
        var line = rawLine
        if line.hasSuffix("\r") { line.removeLast() }
        var indent = 0
        for character in line {
            if character == " " { indent += 1 } else if character == "\t" {
                return nil
            } else {
                break
            }
        }
        guard indent <= 3 else { return nil }
        let content = line.trimmingCharacters(in: .whitespaces)
        guard !content.isEmpty, content.contains("|") else { return nil }

        let characters = Array(content)
        var boundaries: [Int] = []
        var backslashes = 0
        for (position, character) in characters.enumerated() {
            if character == "\\" {
                backslashes += 1
                continue
            }
            if character == "|", backslashes.isMultiple(of: 2) {
                boundaries.append(position)
            }
            backslashes = 0
        }
        guard !boundaries.isEmpty else { return nil }

        var lower = 0
        var upper = characters.count
        if boundaries.first == 0 {
            lower = 1
            boundaries.removeFirst()
        }
        if let last = boundaries.last, last == characters.count - 1, last >= lower {
            upper = last
            boundaries.removeLast()
        }
        var cells: [String] = []
        var cursor = lower
        for boundary in boundaries {
            cells.append(String(characters[cursor..<boundary]))
            cursor = boundary + 1
        }
        cells.append(String(characters[cursor..<upper]))
        return cells.map { $0.trimmingCharacters(in: .whitespaces) }
    }

    /// The delimiter row's cells as alignments; nil when any cell is not
    /// `:---:`-shaped. Three hyphens minimum, per GitHub's own authoring
    /// documentation — one or two stay prose.
    private static func delimiterAlignments(_ line: String) -> [MarkdownTable.Alignment]? {
        guard let cells = tableRowCells(line) else { return nil }
        var alignments: [MarkdownTable.Alignment] = []
        for cell in cells {
            var dashes = Substring(cell)
            let leadingColon = dashes.hasPrefix(":")
            if leadingColon { dashes.removeFirst() }
            let trailingColon = dashes.hasSuffix(":")
            if trailingColon { dashes.removeLast() }
            guard dashes.count >= 3, dashes.allSatisfy({ $0 == "-" }) else { return nil }
            switch (leadingColon, trailingColon) {
            case (true, true): alignments.append(.center)
            case (false, true): alignments.append(.trailing)
            case (true, false), (false, false): alignments.append(.leading)
            }
        }
        return alignments
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
    /// useful" and not "## How it's useful" — the artifact, smaller. A table
    /// becomes its one semantic line for the same reason: four lines of pipe
    /// art say nothing, "Table: Command, Status — 4 rows" says what arrived.
    static func previewSource(_ text: String) -> String {
        segments(text)
            .map { segment in
                switch segment {
                case .prose(let prose): return prose
                case .heading(let heading): return heading
                case .code(let code): return code
                case .table(let table): return tablePreviewLine(table)
                }
            }
            .joined(separator: "\n")
    }

    // MARK: - Table summaries

    /// `Table: Command, Description, Status — 4 rows` — the collapsed
    /// preview's stand-in for the table itself.
    static func tablePreviewLine(_ table: MarkdownTable) -> String {
        "Table: " + table.headers.map(plainCell).joined(separator: ", ")
            + " — " + rowCountPhrase(table.rows.count)
    }

    /// `Table, 3 columns, 4 rows.` — the VoiceOver container label.
    static func tableSummary(_ table: MarkdownTable) -> String {
        "Table, \(table.headers.count) columns, \(rowCountPhrase(table.rows.count))."
    }

    /// `Columns: Command, Description, Status.` — the VoiceOver header stop.
    static func tableColumnsLabel(_ table: MarkdownTable) -> String {
        "Columns: " + table.headers.map(plainCell).joined(separator: ", ") + "."
    }

    /// `Row 1. Command: git status. Description: Lists changed files.` — one
    /// VoiceOver stop per row, each cell paired with its column so a wide row
    /// stays comprehensible without sight of the grid.
    static func tableRowLabel(_ table: MarkdownTable, row: Int) -> String {
        let pairs = zip(table.headers, table.rows[row]).map { header, cell in
            "\(plainCell(header)): \(plainCell(cell))."
        }
        return (["Row \(row + 1)."] + pairs).joined(separator: " ")
    }

    private static func rowCountPhrase(_ count: Int) -> String {
        count == 1 ? "1 row" : "\(count) rows"
    }

    /// A cell as speech and previews should say it: inline markdown resolved
    /// to its characters — `**Ready**` says Ready, `\|` says a pipe — and an
    /// empty cell says so instead of vanishing into silence.
    private static func plainCell(_ cell: String) -> String {
        let plain = String(inline(cell).characters)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return plain.isEmpty ? "empty" : plain
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
