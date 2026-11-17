import Foundation

/// What the composer should do with one piece of typed text, decided before
/// anything reaches the Mac.
public enum CommandAction: Equatable {
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
    /// A built-in this app will not inject, with the honest sentence why.
    /// Blocking beats "send and warn": a warning followed by a measured
    /// lockout is not a safeguard.
    case blocked(command: String, reason: String)
    /// Ordinary prose — or a custom skill, which expands into a normal
    /// prompt turn and needs nothing from us.
    case passThrough
}

/// The app's slash-command policy: native adapters for what this app answers
/// itself, a measured guard for the built-ins that take the Mac's composer
/// away, and untouched passage for everything else.
///
/// Every list here is *measured* — each command injected exactly as the
/// daemon injects it, with the composer-presence needle sampled on a timing
/// ladder afterwards. None of it is a guess about what a command is, and none
/// of it is the binary's own inventory: see `action(for:)`.
public enum ClaudeCommandPolicy {

    /// Commands **measured** to take the Mac's composer away — every one of
    /// these was injected exactly as the daemon injects, sampling the
    /// composer-presence needle on a 50/100/200/400/800/1200ms ladder, and
    /// every one showed the composer gone at every sample. While it is gone
    /// the daemon refuses every send, so the phone is locked out of its own
    /// session.
    ///
    /// They are refused client-side and never typed at all. Two of them are
    /// the reason the list must exist regardless:
    /// `config` and `keybindings` survive Escape — `keybindings` spawns
    /// **vim** on a config file, where Escape is a mode key — so no
    /// automatic recovery can rescue them.
    ///
    /// Measured-inline and therefore NOT here, though a hand-written list
    /// once claimed they were dialogs: `context` (a large render that can
    /// hide the footer transiently and returns by itself), `agents`,
    /// `focus`.
    static let dialogCommands: Set<String> = [
        "model", "status", "usage", "cost", "help", "export", "diff",
        "permissions", "memory", "config", "hooks", "ide", "keybindings",
        "mcp",
    ]

    /// Measured to open a view that **one Escape does not close**, so they are
    /// refused with their own sentence rather than the generic one.
    static let unrecoverableCommands: Set<String> = ["config", "keybindings"]

    /// Commands this app answers itself rather than typing: the run's settings
    /// sheet for `/model` and `/effort`, a sheet for `/compact`, a confirmation
    /// for `/clear`, and the native diff for `/diff`.
    static let nativeCommands: Set<String> = [
        "model", "diff", "effort", "compact", "clear",
    ]

    /// Refused because CodeConnect cannot observe the result: the Mac's
    /// title changes and nothing reports it back, so the app would be
    /// claiming a change it cannot see.
    static let unobservableCommands: Set<String> = ["rename"]

    /// `"/model sonnet"` → `"sonnet"` — everything after the command word.
    private static func arguments(of text: String, command: String) -> String {
        text.trimmingCharacters(in: .whitespacesAndNewlines)
            .dropFirst("/\(command)".count)
            .trimmingCharacters(in: .whitespaces)
    }

    /// Classification, in the order the measurements justify.
    ///
    /// **The discovered catalog is deliberately not a parameter.** It was,
    /// and it was wrong: the probe lists 43 commands and omits real built-ins
    /// including `/status`, `/cost`, `/help` and `/export` — all four
    /// measured to take the composer away — so "absent from the catalog" was
    /// read as "harmless custom skill" and typed, which is the lockout this
    /// feature exists to prevent. Safety is the measured lists below. Taking
    /// the argument away is stronger than a test forbidding its use: the mistake cannot be
    /// written here again.
    public static func action(for text: String, runSettings: RunSettings = .deferToOtherPath) -> CommandAction {
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
        // Decision 227: the refusals below guard a terminal composer, which these commands take over.
        // They apply only where `runSettings` defers — a terminal-attached Claude run, a non-Claude run,
        // or an unknown one (fail-safe). A run CodeConnect started sends every command; claude's answer,
        // or its own "isn't available in this environment" line, is the row. (C5 rewrites `runSettings`
        // so no Claude run defers; C6 then removes the dialog list and this branch with it.)
        guard case .deferToOtherPath = runSettings else { return .passThrough }
        if unrecoverableCommands.contains(command) {
            return .blocked(
                command: command,
                reason:
                    "/\(command) opens a view on \(HostDevice.current.computer("the computer")) that Esc does not close, so "
                    + "CodeConnect cannot recover the composer for you.")
        }
        if dialogCommands.contains(command) {
            return .blocked(
                command: command,
                reason:
                    "/\(command) opens a view on \(HostDevice.current.computer("the computer")) and would take over this "
                    + "composer.")
        }
        if unobservableCommands.contains(command) {
            return .blocked(
                command: command,
                reason:
                    "/\(command) changes something CodeConnect cannot yet observe.")
        }
        // Everything else — a measured-inline built-in, a custom skill, a
        // project command, a typo — is typed.
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

/// A model identifier as a human should see it. The SessionStart hook hands
/// the app raw API ids — `claude-opus-5[1m]`, `claude-haiku-4-5-20251001` —
/// and rendering one of those to a person was a shipped, screenshotted
/// mistake. Parsing is deliberately narrow: only the `claude-` family shape
/// is interpreted; anything else (Claude's own display words like
/// "Sonnet 5", a custom id) passes through verbatim.
public struct ModelDisplay: Equatable {
    public var name: String
    /// A variant worth its own quiet line — today only "1M context".
    public var meta: String?

    /// Claude Code's model families, which `/model` takes as aliases for the
    /// newest of each, named as its own model list names them. Names only:
    /// which version an alias stands for is Claude Code's to say, in its
    /// receipt, and the versions this table used to carry went stale with a
    /// release.
    public static let families = ["fable", "opus", "sonnet", "haiku"]

    public static func from(_ raw: String) -> ModelDisplay {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        let lowered = trimmed.lowercased()
        if families.contains(lowered) {
            return ModelDisplay(name: word(lowered), meta: nil)
        }
        guard lowered.hasPrefix("claude-") else {
            // Not an API id shape. Claude's own words ("Sonnet 5"), or some
            // human-entered string — display verbatim, human typography.
            return ModelDisplay(name: trimmed, meta: nil)
        }
        var body = String(lowered.dropFirst("claude-".count))
        var meta: String?
        if body.hasSuffix("[1m]") {
            body = String(body.dropLast("[1m]".count))
            meta = "1M context"
        }
        var tokens = body.split(separator: "-").map(String.init)
        // A trailing 8-digit date stamps the snapshot, not the identity.
        if let last = tokens.last, last.count == 8, last.allSatisfy(\.isNumber) {
            tokens.removeLast()
        }
        // Only the known families earn an invented display name; a family
        // this build has never heard of stays visibly machine-shaped rather
        // than being dressed in a name nobody vouched for.
        guard let family = tokens.first, families.contains(family),
            tokens.dropFirst().allSatisfy({ $0.allSatisfy(\.isNumber) })
        else {
            return ModelDisplay(name: trimmed, meta: nil)
        }
        let versions = tokens.dropFirst()
        let name = versions.isEmpty ? word(family) : word(family) + " " + versions.joined(separator: ".")
        return ModelDisplay(name: name, meta: meta)
    }

    /// `opus` → `Opus`.
    private static func word(_ family: String) -> String {
        family.prefix(1).uppercased() + family.dropFirst()
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
