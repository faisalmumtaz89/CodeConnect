import Foundation

/// What the composer should do with one piece of typed text, decided before
/// anything reaches the Mac.
enum CommandAction: Equatable {
    /// `/model` — the one command with native app UX. Anything after the
    /// name prefills the sheet's custom field; nothing is typed until the
    /// sheet says so.
    case nativeModel(prefillArgs: String)
    /// A built-in this app will not inject, with the honest sentence why.
    /// Blocking beats "send and warn": a warning followed by a measured
    /// lockout is not a safeguard.
    case blocked(command: String, reason: String)
    /// Ordinary prose — or a custom skill, which expands into a normal
    /// prompt turn and needs nothing from us.
    case passThrough
}

/// The app's slash-command policy: one native adapter, a static guard for
/// the dialog-opening built-ins, and fail-closed for every built-in the
/// installed binary reports that this app has not deliberately supported.
///
/// The *list* of commands is discovered from the binary (`get_command_catalog`),
/// never maintained by hand. What is maintained is this small policy — which
/// is the part that genuinely needs product judgment per command.
enum ClaudeCommandPolicy {

    /// Built-ins documented to open an interactive view in the terminal.
    /// Measured consequence of injecting one from the phone: the dialog
    /// replaces the composer, the daemon's presence check starts refusing
    /// every send, and the phone is locked out of its own session until
    /// someone presses Esc at the Mac. Static rather than discovered, so the
    /// guard holds even when the catalog is unavailable.
    static let dialogCommands: Set<String> = [
        "model", "permissions", "agents", "config", "context", "diff",
        "memory", "hooks", "ide", "keybindings", "focus", "mcp",
    ]

    static func action(for text: String, catalog: [String]?) -> CommandAction {
        guard let command = firstToken(text) else { return .passThrough }
        if command == "model" {
            let args = text.trimmingCharacters(in: .whitespacesAndNewlines)
                .dropFirst("/model".count)
                .trimmingCharacters(in: .whitespaces)
            return .nativeModel(prefillArgs: args)
        }
        if dialogCommands.contains(command) {
            return .blocked(
                command: command,
                reason:
                    "/\(command) opens an interactive view on the Mac's screen, which "
                    + "locks this composer until someone presses Esc there. "
                    + "Use the Terminal tab to drive it.")
        }
        if let catalog, catalog.contains(command) {
            // A real built-in with no native support yet. Fail closed: a
            // command this app has not classified might be a dialog under a
            // new name, and the lockout is not a risk worth a guess.
            return .blocked(
                command: command,
                reason:
                    "/\(command) is a Claude Code command this app doesn't drive yet. "
                    + "Use the Terminal tab to run it.")
        }
        // Not a known built-in: a custom skill or literal text. Skills expand
        // into ordinary prompt turns, so they need no special handling — and
        // refusing them would break the user's own commands.
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

/// A model fact and its provenance — see `SessionState.lastConfirmedModel`.
struct ConfirmedModel: Sendable, Hashable {
    var name: String
    /// "Session start" or "Command confirmation" — rendered next to the age,
    /// because a fact without its origin reads as more certain than it is.
    var source: String
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
