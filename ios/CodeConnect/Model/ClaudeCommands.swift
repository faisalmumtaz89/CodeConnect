import Foundation

/// The Mac views the phone may capture: one Settings dialog on the Mac, three
/// commands that open it. Each opens the reusable snapshot sheet.
enum SnapshotCommand: String, CaseIterable, Identifiable {
    case status, usage, cost

    var id: String { rawValue }

    /// The sheet's title — naming the phone's promise (a Mac capture), not
    /// Claude's internal tab names.
    ///
    /// **`/cost` is titled "Usage", because that is the view it opens.**
    /// Measured on 2.1.223: injecting `/cost` makes Claude Code record
    /// `<command-name>/usage</command-name>` in the transcript and render a
    /// pane identical to `/usage`'s. Titling it "Cost" promised a third view
    /// that does not exist. The sheet prints the command actually typed
    /// beneath this title, so it reads "Usage · /cost" — the view you got, and
    /// how you got there.
    var title: String {
        switch self {
        case .status: return "Mac status"
        case .usage, .cost: return "Usage"
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
    /// Bare `/effort` — the sheet chooses. `/effort <arg>` deliberately does
    /// NOT come here, and must not grow an app-side validity list: the valid
    /// set has already drifted once (`ultracode`, `auto` appeared without
    /// notice), so a list here would refuse things the Mac accepts — the
    /// false-positive class this feature exists to end.
    ///
    /// **It is not because every argument runs inline.** Measured on 2.1.223:
    /// `/effort` with a value *different from the current one* opens a
    /// `Change effort level?` confirmation whenever the conversation is already
    /// cached for the current level, exactly as `/model` does. Only a same-value
    /// `/effort` is reliably inline. Pass-through stands because the honest fix
    /// is a typed native operation, not a validity list.
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

/// Where a model fact came from. Typed rather than the display string it is
/// rendered as, so nothing decides anything by comparing prose.
enum ModelProvenance: Sendable, Hashable {
    /// The SessionStart hook's `model` field.
    case sessionStart
    /// Claude Code's own `/model` output, whichever verb it used. Which verb
    /// is a property of the *receipt*, and lives on `ModelConfirmation`; here
    /// it would only be a second copy to keep in step.
    case command
}

/// A model fact and its provenance — see `SessionState.lastConfirmedModel`.
struct ConfirmedModel: Sendable, Hashable {
    var name: String
    /// Rendered next to the age, because a fact without its origin reads as
    /// more certain than it is.
    var provenance: ModelProvenance
    var at: Date
}

/// Everything Claude Code's `/model` command is measured to say back.
///
/// The error is here beside the receipts because the Model sheet's free-text
/// field is the only thing that can produce it, and it is that field's
/// guaranteed outcome for a value Claude Code does not know. Left unread it
/// would leave the sheet waiting for a confirmation already on screen in the
/// timeline behind it.
enum ModelCommandOutcome: Sendable, Hashable {
    case receipt(ModelConfirmation)
    /// `Model 'bananas' not found`, verbatim. Terminal, and never a fact about
    /// the session's model.
    case notFound(String)

    static func parse(_ line: String) -> ModelCommandOutcome? {
        if let receipt = ModelConfirmation.parse(line) { return .receipt(receipt) }
        // Matched by its measured shape and nothing looser: a line merely
        // containing "not found" is somebody else's error.
        if line.hasPrefix("Model '") && line.hasSuffix("' not found") { return .notFound(line) }
        return nil
    }
}

/// A `/model` outcome and **the event that carried it** — see
/// `SessionState.lastModelCommandSignal`. The sequence is the point: see
/// `ModelChangeWatch`.
struct ModelCommandSignal: Sendable, Hashable {
    var outcome: ModelCommandOutcome
    var seq: UInt64
}

/// The Model sheet's correlation rule: **a fence in the event stream, and a
/// match on the value that was asked for.**
///
/// The fence alone proves recency, not identity. `SessionState` fills an empty
/// model slot with the first receipt a backfill finds at any sequence, so
/// without it a "load earlier" replay landing inside the wait reports an
/// hour-old change as the answer to this tap. But recency is not enough on its
/// own either: a receipt from somebody at the Mac, or from a second phone, also
/// lands above the fence, and reporting `Model set to Opus 5` over a tap that
/// asked for Sonnet is the same phantom state wearing a fresher timestamp.
///
/// So both. Names are compared through `ModelDisplay`, because the request
/// carries an alias (`sonnet`) and the receipt carries a display name
/// (`Sonnet 5`); the error carries the argument Claude Code quoted back, which
/// is compared to the argument that was sent.
///
/// What survives is still only ever *Claude Code's own report* — the sheet
/// never claims to know which actor caused it, only that what it is showing
/// answers the value this sheet asked for.
enum ModelChangeWatch {
    static func outcome(
        after baselineSeq: UInt64, requested: String, signal: ModelCommandSignal?
    ) -> ModelCommandOutcome? {
        guard let signal, signal.seq > baselineSeq else { return nil }
        let asked = ModelDisplay.from(requested).name
        switch signal.outcome {
        // `Set model to X` names the model that was *applied*, so it answers
        // this request only when X is what this request asked for. Somebody
        // else's switch lands above the fence too, and reporting it here would
        // be the same phantom state wearing a fresher timestamp.
        case .receipt(.set(let name)):
            return ModelDisplay.from(name).name == asked ? signal.outcome : nil
        // **`Kept model as X` names the model still in force, not the one
        // asked for** — measured: `/model sonnet`, cancelled, prints
        // `Kept model as Opus 5`. Matching it against the request would mean a
        // cancelled confirmation never reports at all, which is the silence
        // this whole change exists to end. The fence is what it has, and the
        // sentence it produces is true whoever caused it: Claude Code kept X,
        // and nothing here claims the request succeeded.
        case .receipt(.kept):
            return signal.outcome
        // `Model 'bananas' not found` quotes the value Claude Code could not
        // find, so only this sheet's own value makes it this sheet's failure.
        case .notFound(let line):
            return Self.quotedValue(in: line).map { ModelDisplay.from($0).name } == asked
                ? signal.outcome : nil
        }
    }

    /// The value between the first pair of single quotes, if any.
    static func quotedValue(in line: String) -> String? {
        guard let open = line.firstIndex(of: "'") else { return nil }
        let after = line.index(after: open)
        guard let close = line[after...].firstIndex(of: "'") else { return nil }
        let inner = String(line[after..<close])
        return inner.isEmpty ? nil : inner
    }
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
enum ModelConfirmation: Sendable, Hashable {
    case set(String)
    case kept(String)

    /// The model named by the receipt, whichever verb it used.
    var name: String {
        switch self {
        case .set(let name), .kept(let name): return name
        }
    }

    /// True only for `Set model to …`.
    var isChange: Bool {
        if case .set = self { return true }
        return false
    }

    /// The sentence the Model sheet shows for this receipt.
    ///
    /// Here rather than in the view for two reasons: the live path and the
    /// already-landed path must not word it differently, and an honesty
    /// contract that cannot be asserted in a test is a comment. The
    /// `kept` wording states what Claude Code reported and offers no theory
    /// about who caused it — the same line is printed when a confirmation is
    /// declined at the Mac and when the model asked for is already in force.
    var sheetStatus: String {
        let display = ModelDisplay.from(name).name
        switch self {
        case .set: return "Model set to \(display)."
        case .kept: return "Claude Code kept \(display). The model was not changed."
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
            return name.isEmpty ? nil : .set(name)
        }
        if line.hasPrefix("Kept model as ") {
            let name = line.dropFirst("Kept model as ".count)
                .trimmingCharacters(in: .whitespaces)
            return name.isEmpty ? nil : .kept(name)
        }
        return nil
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
/// The `kept` form must be parsed for the same reason: unread, it leaves the
/// sheet waiting and then saying no confirmation arrived — while the receipt is
/// on screen in the timeline behind it.
///
/// **The scope is read, never assumed.** It is measured to differ per value:
/// `low`, `medium`, `high` and `xhigh` all say "saved as your default for new
/// sessions"; `max` says "this session only". A build that hardcoded either
/// would be wrong for the other, so the bracketed text is carried verbatim and
/// a shape this build has not seen yields no scope at all rather than a guess.
enum EffortConfirmation: Sendable, Hashable {
    case set(value: String, scope: String?)
    case kept(value: String)

    var value: String {
        switch self {
        case .set(let value, _), .kept(let value): return value
        }
    }

    var isChange: Bool {
        if case .set = self { return true }
        return false
    }

    /// The sentence the Effort sheet shows. Here rather than in the view for
    /// the same reasons as `ModelConfirmation.sheetStatus`.
    var sheetStatus: String {
        let label = EffortConfirmation.label(for: value)
        guard case .set(_, let scope) = self else {
            return "Claude Code kept \(label) effort. The level was not changed."
        }
        guard let scope else { return "Claude Code confirmed \(label) effort." }
        return "Claude Code confirmed \(label) effort (\(scope))."
    }

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
