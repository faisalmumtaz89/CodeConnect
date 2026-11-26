import Foundation

/// What the composer should do with one piece of typed text, decided before
/// anything reaches the Mac.
public enum CommandAction: Equatable, Sendable {
    /// `/model` — the run's settings sheet; nothing is typed.
    case nativeModel
    /// `/diff` — the app renders this itself, better than the Mac's own
    /// view; open that instead of typing anything.
    case nativeDiff
    /// Bare `/effort` — the run's settings sheet. `/effort <arg>` does NOT
    /// come here, and must not grow an app-side validity list: the valid set
    /// has already drifted once (`ultracode`, `auto` appeared without notice),
    /// so a list here would refuse things the Mac accepts.
    case nativeEffort
    /// `/compact [instructions]` — the sheet, with any typed instructions
    /// already in its field.
    case nativeCompact(prefillInstructions: String)
    /// Bare `/clear` — the destructive confirmation, no intermediate sheet.
    case nativeClear
    /// A command this app will not send as typed, with the sentence why.
    case blocked(command: String, reason: String)
    /// Ordinary prose — or a custom skill, which expands into a normal
    /// prompt turn and needs nothing from us.
    case passThrough
}

/// The app's slash-command policy: native adapters for what this app answers
/// itself, and untouched passage for everything else — Claude Code answers
/// every other command on a run CodeConnect started.
public enum ClaudeCommandPolicy {

    /// Commands this app answers itself rather than typing: the run's settings
    /// sheet for `/model` and `/effort`, a sheet for `/compact`, a confirmation
    /// for `/clear`, and the native diff for `/diff`.
    static let nativeCommands: Set<String> = [
        "model", "diff", "effort", "compact", "clear",
    ]

    /// `"/model sonnet"` → `"sonnet"` — everything after the command word.
    private static func arguments(of text: String, command: String) -> String {
        text.trimmingCharacters(in: .whitespacesAndNewlines)
            .dropFirst("/\(command)".count)
            .trimmingCharacters(in: .whitespaces)
    }

    /// Classification: the five commands this app answers itself, and everything
    /// else typed as written.
    public static func action(for text: String) -> CommandAction {
        guard let command = firstToken(text) else { return .passThrough }
        let args = arguments(of: text, command: command)
        if command == "model" { return .nativeModel }
        if command == "diff" { return .nativeDiff }
        if command == "effort" {
            // With an argument, pass it through without a local validity list;
            // Claude Code remains the authority on accepted values. A different
            // value may open a confirmation on a cache-warm conversation.
            return args.isEmpty ? .nativeEffort : .passThrough
        }
        if command == "compact" {
            return .nativeCompact(prefillInstructions: args)
        }
        if command == "clear" {
            guard args.isEmpty else {
                return .blocked(
                    command: command,
                    reason:
                        "This app supports /clear without arguments. Remove the extra text.")
            }
            return .nativeClear
        }
        // Everything else — a built-in, a custom skill, a project command, a
        // typo — is typed, and Claude Code's answer is the row.
        return .passThrough
    }

    /// `"/model sonnet"` → `"model"`. Nil when the text is not slash-shaped:
    /// no leading slash, nothing after it, or whitespace before it.
    static func firstToken(_ text: String) -> String? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.hasPrefix("/") else { return nil }
        let name = trimmed.dropFirst().prefix { !$0.isWhitespace }
        guard !name.isEmpty else { return nil }
        // A path like /tmp/x is not a command; commands are word-shaped.
        guard name.allSatisfy({ $0.isLetter || $0.isNumber || $0 == "-" || $0 == "_" }) else {
            return nil
        }
        return String(name).lowercased()
    }
}

/// A model fact — see `SessionState.lastConfirmedModel`.
public struct ConfirmedModel: Sendable, Hashable {
    public var name: String
}

/// What Claude Code's own model receipt says happened, as recorded in the
/// transcript and rendered as a timeline notice.
///
/// **Both spellings are measured, and they mean opposite things.** The applied
/// form is `Set model to X and saved as your default for new sessions`.
/// `Kept model as X` is what Claude Code prints when its `Switch model?`
/// confirmation was cancelled, or the same model was re-chosen — the model did
/// **not** change.
///
/// Both are parsed, on purpose: `Kept model as X` is a true statement of the
/// model in force, and it is the only signal by which this app learns the model
/// after somebody cancels a confirmation at the Mac. Collapsing the two into one
/// bare name is what lets a cancellation report success, so the verb lives in
/// the type, where it cannot be dropped.
public enum ModelConfirmation: Sendable, Hashable {
    case set(String)
    case kept(String)

    /// The model named by the receipt, whichever verb it used.
    var name: String {
        switch self {
        case .set(let name), .kept(let name): return name
        }
    }

    static func parse(_ line: String) -> ModelConfirmation? {
        if line.hasPrefix("Set model to ") {
            let rest = line.dropFirst("Set model to ".count)
            let name: String
            if let end = rest.range(of: " and saved") {
                name = String(rest[..<end.lowerBound]).trimmingCharacters(in: .whitespaces)
            } else {
                name = rest.trimmingCharacters(in: .whitespaces)
            }
            return name.isEmpty ? nil : .set(unwrapped(name))
        }
        if line.hasPrefix("Kept model as ") {
            let name = line.dropFirst("Kept model as ".count)
                .trimmingCharacters(in: .whitespaces)
            return name.isEmpty ? nil : .kept(unwrapped(name))
        }
        return nil
    }

    /// The name out of its code span: Claude Code 2.1.319 writes
    /// ``Set model to `Opus 5.5` and saved…`` to its transcript, and draws the
    /// name plain. One rule for a receipt's name wherever it is read — this
    /// parse, and the daemon's model fact, which a daemon of 0.17.2 sends as it
    /// was written — and the daemon's own parse keeps the same one.
    static func unwrapped(_ name: String) -> String {
        name.count > 2 && name.hasPrefix("`") && name.hasSuffix("`") ? String(name.dropFirst().dropLast()) : name
    }
}

/// Claude Code's receipt for an effort request. Two spellings, measured on
/// 2.1.223 and meaning opposite things — exactly as for the model:
///
/// * `Set effort level to xhigh (saved as your default for new sessions):
///   Deeper reasoning…` — the value token, a scope note in brackets, then a
///   description.
/// * `Kept effort level as high` — printed when the `Change effort level?`
///   confirmation was cancelled, or the same level was re-chosen. The level
///   did **not** change.
///
/// **The scope is read, never assumed.** It is measured to differ per value:
/// `low`, `medium`, `high` and `xhigh` all say "saved as your default for new
/// sessions"; `max` says "this session only". A build that hardcoded either
/// would be wrong for the other, so the bracketed text is carried verbatim and
/// a shape this build has not seen yields no scope at all rather than a guess.
public enum EffortConfirmation: Sendable, Hashable {
    case set(value: String, scope: String?)
    case kept(value: String)

    static func parse(_ line: String) -> EffortConfirmation? {
        if line.hasPrefix("Set effort level to ") {
            let rest = line.dropFirst("Set effort level to ".count)
            let value = rest.prefix { $0.isLetter }
            guard !value.isEmpty else { return nil }
            return .set(value: String(value), scope: scope(in: rest))
        }
        if line.hasPrefix("Kept effort level as ") {
            let value = line.dropFirst("Kept effort level as ".count)
                .prefix { $0.isLetter }
            return value.isEmpty ? nil : .kept(value: String(value))
        }
        return nil
    }

    /// The scope note, which is the parenthetical **immediately** after the
    /// value — not the first one in the line. `xhigh`'s description ends
    /// `(Fable 5, Opus 4.7+, Sonnet 5)`, so a search anywhere would read a
    /// description as a scope the moment either is reworded. Position is what
    /// makes this safe, so the text is shown as Claude Code wrote it.
    private static func scope(in rest: Substring) -> String? {
        let afterValue = rest.drop { $0.isLetter }.drop { $0 == " " }
        guard afterValue.first == "(", let close = afterValue.firstIndex(of: ")") else {
            return nil
        }
        let inner = String(afterValue[afterValue.index(after: afterValue.startIndex)..<close])
        return inner.isEmpty ? nil : inner
    }

    /// `xhigh` → `Extra high` — the human label for a level. A value
    /// this build has never heard of stays as sent; it will read as the
    /// machine word it is, which is honest.
    static func label(for value: String) -> String {
        switch value {
        case "low": return "Low"
        case "medium": return "Medium"
        case "high": return "High"
        case "xhigh": return "Extra high"
        case "max": return "Maximum"
        case "ultra": return "Ultra"
        default: return value
        }
    }
}

/// What Claude Code says when a compaction finishes or refuses. Both lines
/// are measured verbatim: success is `Compacted (ctrl+o to see full
/// summary)` — prefix-matched, the parenthetical is a Mac keyboard hint —
/// and the refusal on a near-empty context is exact.
public enum CompactConfirmation: Hashable, Sendable {
    case compacted
    case notEnoughMessages

    static func parse(_ line: String) -> CompactConfirmation? {
        if line.hasPrefix("Compacted") { return .compacted }
        if line == "Not enough messages to compact." { return .notEnoughMessages }
        return nil
    }
}

/// One row of the slash palette: the command and what choosing it opens, through the same `CommandAction` a typed send
/// resolves to, so a choice and a typed command can never drift apart.
public struct SlashRow: Equatable, Identifiable, Sendable {
    public let command: String
    public let subtitle: String
    public let action: CommandAction

    public var id: String { command }
    public var title: String { "/\(command)" }
}

/// What the slash palette shows above a Claude run's reply box while the box holds a bare `/` or a prefix of a command
/// this app answers itself. Anything else (arguments, unknown fragments) shows no palette; typing simply continues.
public enum SlashPalette {
    /// The rows for one text, and whether the discovery caption goes under them.
    public struct Content: Equatable, Sendable {
        public var rows: [SlashRow]
        public var showsCaption: Bool
    }

    /// Under the rows of a bare `/`: the commands this app doesn't answer itself still run.
    public static let caption = "Run other commands on the computer."

    /// The five native commands, in the order they are worth attention: the two levers, the two context acts, then
    /// the diff.
    public static let allRows: [SlashRow] = [
        SlashRow(command: "model", subtitle: "Choose model", action: .nativeModel),
        SlashRow(command: "effort", subtitle: "Choose reasoning effort", action: .nativeEffort),
        SlashRow(
            command: "compact", subtitle: CompactPrompt.title, action: .nativeCompact(prefillInstructions: "")),
        SlashRow(command: "clear", subtitle: "Clear context", action: .nativeClear),
        SlashRow(command: "diff", subtitle: "Review working-tree changes", action: .nativeDiff),
    ]

    /// The rows `agent` takes: all five for Claude Code; Antigravity has no `/clear`, so not that one.
    public static func rows(for agent: AgentKind) -> [SlashRow] {
        agent == .antigravity ? allRows.filter { $0.command != "clear" } : allRows
    }

    /// The palette for one text on `agent`'s run, or nil for none. Bare `/` is the discovery moment: every row and the
    /// caption. A fragment filters case-insensitively. Whitespace after the fragment means arguments are coming, and
    /// arguments are typing, not browsing.
    public static func content(for typed: String, agent: AgentKind = .claude) -> Content? {
        let afterLeading = typed.drop(while: \.isWhitespace)
        guard afterLeading.hasPrefix("/") else { return nil }
        let fragment = afterLeading.dropFirst()
        let offered = rows(for: agent)
        if fragment.isEmpty {
            return Content(rows: offered, showsCaption: true)
        }
        guard !fragment.contains(where: \.isWhitespace) else { return nil }
        let needle = fragment.lowercased()
        let matches = offered.filter { $0.command.hasPrefix(needle) }
        return matches.isEmpty ? nil : Content(rows: matches, showsCaption: false)
    }

    /// Whether the palette is closed on the draft as it stands. Esc, a click outside or a row run close it; any edit of
    /// the draft opens it again, so the same words typed again show it again, and words left after Esc are sent as
    /// typed by the next Return.
    public struct Latch: Equatable, Sendable {
        public private(set) var closed = false

        public init() {}

        public mutating func close() { closed = true }

        /// The draft changed.
        public mutating func edited() { closed = false }

        /// The palette for `draft` on a Claude or Antigravity run (`agent`) whose reply box has the keyboard, unless it
        /// is closed; a Codex run has no slash vocabulary.
        public func content(for draft: String, agent: AgentKind?, focused: Bool) -> Content? {
            guard let agent, agent == .claude || agent == .antigravity, focused, !closed else { return nil }
            return SlashPalette.content(for: draft, agent: agent)
        }
    }

    /// A key pressed in the reply box while it may show the palette.
    public enum Key: Sendable {
        case up, down, escape
        /// Return, and whether ⇧ or ⌥ was held with it.
        case `return`(addsLine: Bool)
    }

    /// What a key does.
    public enum KeyOutcome: Equatable, Sendable {
        /// Highlight this row.
        case highlight(Int)
        /// Run this row's action.
        case run(Int)
        /// Close the palette and keep the text: the next Return sends it as typed.
        case close
        /// Add a line where the caret is.
        case addLine
        /// Not the palette's: the box does what it does with the key.
        case toBox
    }

    /// What `key` does with `rows` rows showing (0: no palette) and row `highlighted` lit. The first row is lit when the
    /// palette shows; ↑ and ↓ move the light and stop at the ends; Return runs the lit row; ⇧Return and ⌥Return add a
    /// line, palette or not; Esc closes the palette. With no palette every other key is the box's. The light is a menu
    /// panel's (`MenuLight`), every row one that can be run.
    public static func outcome(of key: Key, rows: Int, highlighted: Int) -> KeyOutcome {
        if case .return(addsLine: true) = key { return .addLine }
        guard rows > 0 else { return .toBox }
        let lit = min(max(highlighted, 0), rows - 1)
        var light = MenuLight(lit: lit)
        let enabled = Array(repeating: true, count: rows)
        switch key {
        case .up: _ = light.press(.up, enabled: enabled)
        case .down: _ = light.press(.down, enabled: enabled)
        case .escape: return .close
        case .return: return .run(lit)
        }
        return .highlight(light.lit ?? lit)
    }
}
