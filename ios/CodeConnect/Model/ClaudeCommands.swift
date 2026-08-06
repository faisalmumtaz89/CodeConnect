import Foundation

/// The three Mac views the phone may capture: one Settings dialog on the
/// Mac, three tabs, three commands. Each opens the reusable snapshot sheet.
enum SnapshotCommand: String, CaseIterable, Identifiable {
    case status, usage, cost

    var id: String { rawValue }

    /// The sheet's title — naming the phone's promise (a Mac capture), not
    /// Claude's internal tab names.
    var title: String {
        switch self {
        case .status: return "Mac status"
        case .usage: return "Usage"
        case .cost: return "Cost"
        }
    }
}

/// What the composer should do with one piece of typed text, decided before
/// anything reaches the Mac.
enum CommandAction: Equatable {
    /// `/model` — anything after the name prefills the sheet's custom
    /// field; nothing is typed until the sheet says so.
    case nativeModel(prefillArgs: String)
    /// `/diff` — the app renders this itself, better than the Mac's own
    /// view; open that instead of typing anything.
    case nativeDiff
    /// Bare `/effort` — the sheet chooses. `/effort <arg>` deliberately
    /// does NOT come here: every measured argument (valid and invalid) is
    /// inline, Claude Code prints its own confirmation or error into the
    /// transcript, and the valid set has already drifted once (`ultracode`,
    /// `auto` appeared without notice) — so an app-side validity list
    /// would refuse things the Mac accepts, the false-positive class this
    /// feature exists to end.
    case nativeEffort
    /// `/compact [instructions]` — the sheet, with any typed instructions
    /// already in its field.
    case nativeCompact(prefillInstructions: String)
    /// Bare `/clear` — the destructive confirmation, no intermediate sheet.
    case nativeClear
    /// `/status`, `/usage`, `/cost` — the snapshot sheet, which captures
    /// the Mac view these commands open and closes it again.
    case nativeSnapshot(SnapshotCommand)
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
/// of it is the binary's own inventory: see `action(for:recoversComposer:)`.
enum ClaudeCommandPolicy {

    /// Commands **measured** to take the Mac's composer away — every one of
    /// these was injected exactly as the daemon injects, sampling the
    /// composer-presence needle on a 50/100/200/400/800/1200ms ladder, and
    /// every one showed the composer gone at every sample. While it is gone
    /// the daemon refuses every send, so the phone is locked out of its own
    /// session.
    ///
    /// **This list is the fast path, not the safety guarantee.** The
    /// guarantee is the supervisor's post-injection recovery check; this
    /// list means the common cases are refused client-side and never typed
    /// at all. Two of them are the reason the list must exist regardless:
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

    /// Measured to open a view that **one Escape does not close** — the
    /// supervisor's recovery cannot save these, so they are refused with
    /// their own sentence rather than the generic one.
    static let unrecoverableCommands: Set<String> = ["config", "keybindings"]

    /// Commands this app answers itself rather than typing: sheets for
    /// `/model`, `/effort` and `/compact`, a confirmation for `/clear`, the
    /// native diff for `/diff`, and — when the daemon can recover the
    /// composer — the snapshot sheet for `/status`, `/usage` and `/cost`.
    static let nativeCommands: Set<String> = [
        "model", "diff", "effort", "compact", "clear",
        "status", "usage", "cost",
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
    /// `recoversComposer` is the daemon's `slash_composer_recovery`
    /// capability — the snapshot trio open a Mac view on purpose and are
    /// only offered when the daemon is proven able to close it again.
    ///
    /// **The discovered catalog is deliberately not a parameter.** It was,
    /// and it was wrong: the probe lists 43 commands and omits real built-ins
    /// including `/status`, `/cost`, `/help` and `/export` — all four
    /// measured to take the composer away — so "absent from the catalog" was
    /// read as "harmless custom skill" and typed, which is the lockout this
    /// feature exists to prevent. Safety is the measured lists below plus the
    /// supervisor's post-injection recovery. Taking the argument away is
    /// stronger than a test forbidding its use: the mistake cannot be
    /// written here again.
    static func action(for text: String, recoversComposer: Bool) -> CommandAction {
        guard let command = firstToken(text) else { return .passThrough }
        let args = arguments(of: text, command: command)
        if command == "model" {
            return .nativeModel(prefillArgs: args)
        }
        if command == "diff" { return .nativeDiff }
        if command == "effort" {
            // With an argument this is measured-inline and Claude Code
            // prints its own confirmation or error — pass it through.
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
                        "This app supports /clear without arguments. "
                        + "Remove the extra text or use Terminal.")
            }
            return .nativeClear
        }
        if let snapshot = SnapshotCommand(rawValue: command) {
            guard args.isEmpty else {
                return .blocked(
                    command: command,
                    reason:
                        "/\(command) takes no arguments here. Remove the "
                        + "extra text to capture the Mac's view.")
            }
            guard recoversComposer else {
                return .blocked(
                    command: command,
                    reason:
                        "This session cannot recover slash-command views "
                        + "safely yet. Restart it after updating "
                        + "CodeConnect, or use Terminal.")
            }
            return .nativeSnapshot(snapshot)
        }
        if unrecoverableCommands.contains(command) {
            return .blocked(
                command: command,
                reason:
                    "/\(command) opens a view on the Mac that Esc does not close, so "
                    + "CodeConnect cannot recover the composer for you. "
                    + "Use the Terminal tab to run it.")
        }
        if dialogCommands.contains(command) {
            return .blocked(
                command: command,
                reason:
                    "/\(command) opens a view on the Mac and would take over this "
                    + "composer. Use the Terminal tab to run it.")
        }
        if unobservableCommands.contains(command) {
            return .blocked(
                command: command,
                reason:
                    "/\(command) changes something CodeConnect cannot yet observe. "
                    + "Use the Terminal tab until the Mac reports it back to the app.")
        }
        // Everything else — a measured-inline built-in, a custom skill, a
        // project command, a typo — is typed. The supervisor's recovery
        // check is what makes that safe: if the composer disappears anyway,
        // it presses Esc and says so, rather than leaving the phone locked.
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
/// "Sonnet 5", a custom id) passes through verbatim, and `isRawFallback`
/// tells the view which strings stayed machine-shaped and must wear
/// `monoSmall`, byte-for-byte.
struct ModelDisplay: Equatable {
    var name: String
    /// A variant worth its own quiet line — today only "1M context".
    var meta: String?
    /// True when the string stayed machine-shaped (an id this mapper does
    /// not recognise) and the view should render it in `monoSmall`,
    /// byte-for-byte. Claude's own display words ("Sonnet 5") are human
    /// already and are NOT a raw fallback.
    var isRawFallback: Bool

    /// The four shipped families and their current display versions — also
    /// the alias table, because `/model sonnet` and `claude-sonnet-5` are
    /// the same choice wearing two spellings.
    private static let families: [String: String] = [
        "fable": "Fable 5",
        "opus": "Opus 5",
        "sonnet": "Sonnet 5",
        "haiku": "Haiku 4.5",
    ]

    static func from(_ raw: String) -> ModelDisplay {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        let lowered = trimmed.lowercased()
        if let known = families[lowered] {
            return ModelDisplay(name: known, meta: nil, isRawFallback: false)
        }
        guard lowered.hasPrefix("claude-") else {
            // Not an API id shape. Claude's own words ("Sonnet 5"), or some
            // human-entered string — display verbatim, human typography.
            return ModelDisplay(name: trimmed, meta: nil, isRawFallback: false)
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
        guard let family = tokens.first, families.keys.contains(family),
            tokens.dropFirst().allSatisfy({ $0.allSatisfy(\.isNumber) })
        else {
            return ModelDisplay(name: trimmed, meta: nil, isRawFallback: true)
        }
        let versions = tokens.dropFirst()
        let name: String
        if versions.isEmpty {
            name = families[family] ?? family
        } else {
            name = family.prefix(1).uppercased() + family.dropFirst() + " "
                + versions.joined(separator: ".")
        }
        return ModelDisplay(name: name, meta: meta, isRawFallback: false)
    }
}

/// A model fact and its provenance — see `SessionState.lastConfirmedModel`.
struct ConfirmedModel: Sendable, Hashable {
    var name: String
    /// "Session start" or "Command confirmation" — rendered next to the age,
    /// because a fact without its origin reads as more certain than it is.
    var source: String
    var at: Date
}

/// An effort fact — see `SessionState.lastConfirmedEffort`. No provenance
/// field: unlike the model, effort has exactly one measured source, Claude
/// Code's own command stdout.
struct ConfirmedEffort: Sendable, Hashable {
    /// The machine value as confirmed — `xhigh`, not "Extra high".
    var value: String
    var at: Date
}

/// A compaction outcome — see `SessionState.lastCompactSignal`.
struct CompactSignal: Sendable, Hashable {
    var outcome: CompactConfirmation
    var at: Date
}

/// The Model sheet's correlation rule: a confirmation counts only when it is
/// a *new* fact (different from the baseline captured at send time) whose
/// provenance is Claude Code's own command output — never the session-start
/// seed, and never the unchanged baseline replayed. The sheet reports what
/// Claude confirmed, which stays honest even if another device raced a
/// change into the same window: the name shown is the Mac's truth either way.
enum ModelChangeWatch {
    static func confirmed(baseline: ConfirmedModel?, current: ConfirmedModel?) -> String? {
        guard let current, current.source == "Command confirmation", current != baseline
        else { return nil }
        return current.name
    }
}

/// Claude Code's own confirmation line for a model change, as recorded in
/// the transcript and rendered as a timeline notice. Both spellings are
/// measured: the argument form says "Set model to X and saved as your
/// default…", a cancelled or same-model picker says "Kept model as X".
enum ModelConfirmation {
    static func parse(_ line: String) -> String? {
        if line.hasPrefix("Set model to ") {
            let rest = line.dropFirst("Set model to ".count)
            guard let end = rest.range(of: " and saved") else {
                let name = rest.trimmingCharacters(in: .whitespaces)
                return name.isEmpty ? nil : name
            }
            let name = String(rest[..<end.lowerBound]).trimmingCharacters(in: .whitespaces)
            return name.isEmpty ? nil : name
        }
        if line.hasPrefix("Kept model as ") {
            let name = line.dropFirst("Kept model as ".count)
                .trimmingCharacters(in: .whitespaces)
            return name.isEmpty ? nil : name
        }
        return nil
    }
}

/// Claude Code's confirmation line for an effort change. Measured for all
/// five values: `Set effort level to xhigh (this session only): Deeper
/// reasoning…` — the value token, then a scope note, then a description.
/// Only the value is parsed; everything after it is display prose.
enum EffortConfirmation {
    static func parse(_ line: String) -> String? {
        guard line.hasPrefix("Set effort level to ") else { return nil }
        let value = line.dropFirst("Set effort level to ".count)
            .prefix { $0.isLetter }
        return value.isEmpty ? nil : String(value)
    }

    /// `xhigh` → `Extra high` — the human labels the sheet uses. A value
    /// this build has never heard of stays as sent; it will read as the
    /// machine word it is, which is honest.
    static func label(for value: String) -> String {
        switch value {
        case "low": return "Low"
        case "medium": return "Medium"
        case "high": return "High"
        case "xhigh": return "Extra high"
        case "max": return "Maximum"
        default: return value
        }
    }
}

/// What Claude Code says when a compaction finishes or refuses. Both lines
/// are measured verbatim: success is `Compacted (ctrl+o to see full
/// summary)` — prefix-matched, the parenthetical is a Mac keyboard hint —
/// and the refusal on a near-empty context is exact.
enum CompactConfirmation: Hashable, Sendable {
    case compacted
    case notEnoughMessages

    static func parse(_ line: String) -> CompactConfirmation? {
        if line.hasPrefix("Compacted") { return .compacted }
        if line == "Not enough messages to compact." { return .notEnoughMessages }
        return nil
    }
}
