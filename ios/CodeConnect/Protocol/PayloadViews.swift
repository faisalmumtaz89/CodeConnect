import Foundation

/// Typed reads over the untyped `Event.payload`.
///
/// The daemon stores hook stdin and transcript lines verbatim
/// (`ccd/src/state.rs` `hook_event`, `ccd/src/tailer.rs` `transcript_event`),
/// so every accessor here is written against a shape recorded from a real
/// session in `fixtures/`. Nothing throws: an absent or reshaped field means
/// "unknown", which the UI renders as unknown rather than guessing.
extension JSONValue {
    /// Round-trips through JSON so `Codable` types defined against the wire can
    /// be read straight out of a payload.
    func decoded<T: Decodable>(_ type: T.Type) -> T? {
        guard let data = try? JSONEncoder().encode(self) else { return nil }
        return try? JSONDecoder().decode(type, from: data)
    }
}

extension Event {
    // MARK: Truncation

    /// The daemon replaces oversized payloads rather than dropping the fact.
    var isTruncated: Bool { payload["_codeconnect_truncated"]?.boolValue == true }

    // MARK: Hook facts

    var hookEventName: String? { payload["hook_event_name"]?.stringValue }
    /// The hook says `tool_name`; the Codex adapter says `tool`
    /// (`tool_call_payload`: `command_execution` / `file_change`). Read through
    /// the hook's spelling alone, every Codex tool row was labelled "tool".
    ///
    /// The two Codex families are spoken in the card's words rather than the
    /// adapter's, because the same daemon already names them that way where a
    /// human reads them: `codex_approval.rs` `Family::tool_name` is `command`
    /// and `file change`. `ToolSummary` exists so "the timeline row, the
    /// decision card and the fleet subtitle can never disagree about what a
    /// call *is*" — a row saying `command_execution` above a card saying
    /// `command` is that disagreement. Only these two, because only these two
    /// exist: an item type this build does not model never reaches `tool_call`
    /// at all (`on_item_started` returns nothing for it).
    var toolName: String? {
        if let name = payload["tool_name"]?.stringValue { return name }
        switch payload["tool"]?.stringValue {
        case "command_execution": return "command"
        case "file_change": return "file change"
        case let other: return other
        }
    }
    /// The hook nests its arguments under `tool_input`; the Codex adapter puts
    /// `command`, `cwd` and `changes` at the top level, so for those the
    /// payload **is** the input.
    var toolInput: JSONValue? {
        if let input = payload["tool_input"] { return input }
        return payload["tool"] == nil ? nil : payload
    }
    /// Hook events carry it at the top level and also in `item_id`; a transcript
    /// `tool_result` hides it inside the message block. `item_id` is *not* a
    /// valid fallback for transcript events, where it holds the entry uuid.
    var toolUseID: String? {
        if let direct = payload["tool_use_id"]?.stringValue { return direct }
        if source == .transcript { return transcriptToolResult?.toolUseID }
        return itemID
    }
    var toolResponse: JSONValue? { payload["tool_response"] }

    /// **What a Codex `tool_result` says became of the call.**
    ///
    /// Measured from `codex_adapter.rs` `tool_result_payload`, which is the
    /// only shape that reaches this arm: `status` (`completed` when the item
    /// ended normally, otherwise the item's own word, and `interrupted` when
    /// the terminal was synthesized by an abort), `exit_code` for a command,
    /// and `aggregated_output` for what it printed. Nothing here is inferred:
    /// a status this build does not know leaves the verdict to the caller
    /// rather than guessing at success.
    var codexToolOutcome: (status: ToolStatus, output: String?)? {
        guard kind == .toolResult, payload["tool"] != nil else { return nil }
        let output = payload["aggregated_output"]?.stringValue
        if payload["interrupted"]?.boolValue == true { return (.interrupted, output) }
        // A command that ran and returned non-zero **failed**, whatever the
        // item's own status says: the status describes the item's lifecycle,
        // the exit code describes the work.
        if let exit = payload["exit_code"]?.intValue, exit != 0 { return (.failed, output) }
        switch payload["status"]?.stringValue {
        case "completed": return (.succeeded, output)
        case "failed": return (.failed, output)
        case "interrupted": return (.interrupted, output)
        default: return nil
        }
    }
    var durationMS: Int? { payload["duration_ms"]?.intValue }
    var permissionMode: String? { payload["permission_mode"]?.stringValue }
    var modelName: String? { payload["model"]?.stringValue }
    var cwd: String? { payload["cwd"]?.stringValue }

    // MARK: Approvals

    var approvalCard: ApprovalCard? { payload["card"]?.decoded(ApprovalCard.self) }
    var approvalOutcome: AnswerOutcome? { payload.decoded(AnswerOutcome.self) }

    /// **What became of a Codex approval.**
    ///
    /// A Codex `approval_resolved` payload is a **bare `CodexResolution`** —
    /// not an `AnswerOutcome`, not wrapped in one — so `approvalOutcome` reads
    /// it as nil and a second accessor is the only way to see it at all. This is
    /// the single most dangerous divergence in the phase: without it a Codex
    /// card stays live and tappable after it has already been decided.
    var codexResolution: CodexResolution? {
        // `kind` is checked here rather than at the call site because a
        // `CodexResolution` decoder is total by design — every unrecognised
        // status has a case — so it would happily "decode" an unrelated
        // payload's object into `.unrecognisedStatus`, and a tool result would
        // start retiring cards.
        guard kind == .approvalResolved else { return nil }
        return payload.decoded(CodexResolution.self)
    }

    /// Which card a Codex resolution belongs to (decision D1).
    ///
    /// The daemon puts `request_id` in the payload additively, in the same
    /// spelling and position as Claude's. **`source_event_id` is deliberately
    /// not parsed**: it carries `"resolved:<request_id>"`, and prefix-parsing an
    /// id-bearing string is precisely the kind of correlation that fails
    /// silently — a rename, a second prefix, or an id that happens to contain a
    /// colon all produce a wrong answer rather than no answer. A resolution with
    /// no `request_id` correlates to nothing, and the card stays live, which is
    /// the safe direction.
    var codexResolvedRequestID: String? {
        guard kind == .approvalResolved else { return nil }
        return payload["request_id"]?.stringValue
    }

    /// The daemon's risk block, wherever it put it.
    ///
    /// `protocol/src/ws.rs` puts it inside `card`, which `ApprovalCard` already
    /// decodes. This is the fallback for an event that carries it beside the
    /// card instead — two lines that mean a placement change downgrades to the
    /// local heuristic instead of going unnoticed. Interpreting the class is
    /// `RiskAssessment`'s job, not this accessor's.
    var declaredRisk: WireRisk? {
        if let beside = payload["risk"]?.decoded(WireRisk.self) { return beside }
        return nil
    }

    // MARK: Notifications

    var notificationType: String? { payload["notification_type"]?.stringValue }
    var notificationMessage: String? { payload["message"]?.stringValue }

    /// `capture-pane` text the daemon attaches to a `permission_prompt`
    /// notification — the only place the phone can learn Claude's *exact*
    /// option list, which is what makes `Option{index}` safe to offer.
    var paneSnapshot: String? { payload["_codeconnect_pane"]?.stringValue }

    // MARK: Lifecycle

    /// From feature level 1 the turn boundary has its own kind. Before that, the
    /// `Stop` hook was filed as `EventKind::SessionEnd` even though the session
    /// was still very much alive, and the raw hook name was the only way to tell
    /// the two apart.
    ///
    /// The legacy clause is kept for older daemons and is inert against a newer
    /// one by construction — a daemon that emits `turn_complete` for `Stop`
    /// never emits a `session_end` carrying `hook_event_name: "Stop"`. Which
    /// build we are talking to is still checked explicitly, in
    /// `DaemonProfile.trustsTurnCompleteKind`, so the fallback is a fallback and
    /// not a second source of truth.
    var isTurnComplete: Bool {
        if kind == .turnComplete { return true }
        return kind == .sessionEnd && hookEventName == "Stop"
    }

    var isSessionExit: Bool {
        kind == .sessionEnd && !isTurnComplete
    }

    var exitCode: Int? { payload["exit_code"]?.intValue }
    var lastAssistantMessage: String? { payload["last_assistant_message"]?.stringValue }

    var linkStateFact: (link: String, reason: String?)? {
        guard kind == .linkState, let link = payload["link"]?.stringValue else { return nil }
        return (link, payload["reason"]?.stringValue)
    }

    var resyncSkipped: Int? {
        guard kind == .resync else { return nil }
        return payload["skipped"]?.intValue
    }

    // MARK: Transcript facts

    /// The session's permission mode, from a `type: "permission-mode"` transcript
    /// line — `bypassPermissions`, `acceptEdits`, `default`, `plan`.
    ///
    /// Read here rather than from a card because the mode that matters most is the
    /// one under which **no card is ever raised**. A run started with
    /// `--dangerously-skip-permissions`, or on a Mac whose settings allow
    /// everything, will never ask a human anything — so the phone shows a session
    /// that streams tools, results and turns and simply never needs you. That is
    /// correct behaviour and it is indistinguishable from a broken link unless the
    /// app says which one it is looking at.
    var permissionModeChange: String? {
        guard case .other(let raw) = kind, raw == "transcript_permission-mode" else { return nil }
        return payload["permissionMode"]?.stringValue
    }

    /// The transcript's local-command shape, when this user line is one —
    /// see `LocalCommandLine`. Checked before `userText` by the timeline
    /// builder, so command markup never renders as a person's words.
    var localCommand: LocalCommandLine? {
        guard kind == .userMessage,
            let content = payload["message"]?["content"]?.stringValue
        else { return nil }
        return LocalCommandLine.parse(content)
    }

    /// **A message that is a flat `text`, which is how Codex sends one.**
    ///
    /// `ccd/src/codex_adapter.rs` `message_payload` builds exactly
    /// `{"text": …, "interrupted": …}` for both `userMessage` and
    /// `agentMessage` items — no `message`, no `content`, no blocks. Read
    /// through Claude's shape it is nil, and a nil message draws no row: the
    /// operator's phone showed a Codex turn as a lone "Turn complete" while
    /// the daemon's log held both halves of the conversation.
    ///
    /// Empty is nil, not "": the adapter defaults `text` to the empty string
    /// when an item carries none, and a blank row is a worse lie than no row.
    private var flatText: String? {
        guard let text = payload["text"]?.stringValue, !text.isEmpty else { return nil }
        return text
    }

    /// **Interrupted, as the wire states it.** The adapter synthesises a
    /// terminal item with `interrupted: true` when a turn is aborted mid-item,
    /// so a reply that was cut short says so on the row rather than being drawn
    /// as a finished thought.
    var isInterruptedItem: Bool { payload["interrupted"]?.boolValue == true }

    /// A user turn's prose. `message.content` is a bare string for typed
    /// prompts and a block array when the CLI attaches context; Codex sends a
    /// flat `text` instead. Exactly those two measured shapes — an unknown
    /// third stays unknown.
    var userText: String? {
        guard kind == .userMessage else { return nil }
        guard let content = payload["message"]?["content"] else { return flatText }
        if let text = content.stringValue { return text }
        guard let blocks = content.arrayValue else { return nil }
        let joined =
            blocks
            .filter { $0["type"]?.stringValue == "text" }
            .compactMap { $0["text"]?.stringValue }
            .joined(separator: "\n")
        return joined.isEmpty ? nil : joined
    }

    /// Prose from an assistant turn. `tool_use` blocks are deliberately skipped:
    /// the PreToolUse hook already reported those as `tool_call` events with a
    /// daemon-assigned seq, and rendering both would double every tool call.
    var agentText: String? {
        guard kind == .agentMessage else { return nil }
        guard let blocks = payload["message"]?["content"]?.arrayValue else { return flatText }
        let joined =
            blocks
            .filter { $0["type"]?.stringValue == "text" }
            .compactMap { $0["text"]?.stringValue }
            .joined(separator: "\n")
        return joined.isEmpty ? nil : joined
    }

    /// Tool-use blocks announced by the assistant turn, used only as a fallback
    /// when no hook `tool_call` carried the same id.
    var agentToolUses: [(id: String, name: String, input: JSONValue)] {
        guard kind == .agentMessage, let blocks = payload["message"]?["content"]?.arrayValue
        else { return [] }
        return blocks.compactMap { block in
            guard block["type"]?.stringValue == "tool_use",
                let id = block["id"]?.stringValue,
                let name = block["name"]?.stringValue
            else { return nil }
            return (id, name, block["input"] ?? .null)
        }
    }

    /// A transcript `tool_result` block, which — unlike the PostToolUse hook —
    /// carries Claude's own `is_error` verdict.
    var transcriptToolResult: (toolUseID: String, isError: Bool, text: String?)? {
        guard kind == .toolResult, source == .transcript,
            let blocks = payload["message"]?["content"]?.arrayValue
        else { return nil }
        for block in blocks where block["type"]?.stringValue == "tool_result" {
            guard let id = block["tool_use_id"]?.stringValue else { continue }
            let isError = block["is_error"]?.boolValue ?? false
            let text = block["content"]?.stringValue
            return (id, isError, text)
        }
        return nil
    }
}

// MARK: - Tool rendering

/// One-line renderings of a tool call. Kept in one place so the timeline row,
/// the decision card and the fleet subtitle can never disagree about what a
/// call "is".
enum ToolSummary {
    /// The argument that actually identifies the call — the command for Bash,
    /// the path for a file tool. Verbatim: never elided, never re-wrapped.
    static func principalArgument(tool: String, input: JSONValue?) -> String? {
        guard let input else { return nil }
        switch tool {
        case "Bash", "BashOutput":
            return input.firstString("command")
        case "Read", "Write", "Edit", "NotebookEdit":
            return input.firstString("file_path", "notebook_path")
        case "Glob", "Grep":
            let pattern = input.firstString("pattern") ?? ""
            if let path = input.firstString("path") { return "\(pattern)  in \(path)" }
            return pattern.isEmpty ? nil : pattern
        case "WebFetch", "WebSearch":
            return input.firstString("url", "query")
        case "Task", "Agent":
            return input.firstString("description", "prompt")
        // **Codex's two families**, whose arguments are not where a Claude
        // tool's are. A command carries `command` at the top level (the
        // `default` arm below finds it); a file change carries no path at all
        // except inside `changes[]`, so read through the default arm it drew a
        // row naming a file it never named.
        case "file change":
            guard let changes = input["changes"]?.arrayValue, !changes.isEmpty else { return nil }
            if changes.count == 1 { return changes[0]["path"]?.stringValue }
            return "\(changes.count) files"
        case "TodoWrite":
            guard let todos = input["todos"]?.arrayValue else { return nil }
            return "\(todos.count) item\(todos.count == 1 ? "" : "s")"
        default:
            return input.firstString(
                "command", "file_path", "path", "pattern", "url", "query", "description")
        }
    }

    /// A fuller, still-single-purpose description for the decision card header.
    static func intent(tool: String, input: JSONValue?) -> String? {
        input?.firstString("description")
    }

    static func symbol(tool: String) -> String {
        switch tool {
        case "Bash", "BashOutput", "KillShell": return "terminal"
        case "Read", "NotebookRead": return "doc.text"
        case "Write": return "square.and.pencil"
        case "Edit", "NotebookEdit": return "pencil.line"
        case "Glob": return "folder.badge.questionmark"
        case "Grep": return "text.magnifyingglass"
        case "WebFetch", "WebSearch": return "globe"
        case "Task", "Agent": return "person.2"
        // **Codex's two families.** Same glyphs as Claude's equivalents: a
        // command is a terminal and a file change is an edit, whichever agent
        // ran it, or the same act reads as two different kinds of thing.
        case "command": return "terminal"
        case "file change": return "pencil.line"
        case "TodoWrite": return "checklist"
        default: return "wrench.and.screwdriver"
        }
    }

    /// The phone's own reading of a tool call, used when the daemon offers no
    /// classification. Deliberately coarse and deliberately pessimistic: an
    /// unrecognised tool is never LOW.
    static func risk(tool: String, input: JSONValue?) -> RiskClass {
        switch tool {
        case "Read", "Glob", "Grep", "NotebookRead", "TodoWrite", "WebSearch":
            return .low
        case "Bash", "BashOutput":
            guard let command = input?.firstString("command") else { return .high }
            return commandRisk(command)
        case "Write", "Edit", "NotebookEdit":
            return .medium
        default:
            return .medium
        }
    }

    private static let destructiveNeedles = [
        "rm -rf", "rm -fr", "rm -r", "sudo", "mkfs", "dd if=", "shutdown", "reboot",
        "chmod 777", "curl | sh", "curl|sh", "wget | sh", "| sh", "> /dev/",
        "git push --force", "git push -f", "git reset --hard", "git clean -fd",
        "drop table", "drop database", "truncate table", "killall", "launchctl unload",
        "defaults delete", "security delete", "npm publish", "cargo publish",
    ]

    private static func commandRisk(_ command: String) -> RiskClass {
        let lowered = command.lowercased()
        if destructiveNeedles.contains(where: { lowered.contains($0) }) { return .high }
        // Network or install side effects leave the worktree behind.
        if lowered.contains("curl ") || lowered.contains("wget ")
            || lowered.contains("npm install") || lowered.contains("pip install")
            || lowered.contains("brew install") || lowered.contains("git push")
        {
            return .medium
        }
        return .low
    }
}

/// Ordered low < medium < high so that reconciling two classifiers is a `max`
/// rather than a nest of `if`s (`RiskAssessment`).
enum RiskClass: Int, Sendable, Hashable, Comparable {
    case low = 0
    case medium = 1
    case high = 2

    static func < (lhs: RiskClass, rhs: RiskClass) -> Bool { lhs.rawValue < rhs.rawValue }

    var label: String {
        switch self {
        case .low: return "LOW"
        case .medium: return "MEDIUM"
        case .high: return "HIGH"
        }
    }

    /// What the class *means*, spelled out — a badge nobody can decode is a
    /// badge that gets ignored.
    var rationale: String {
        switch self {
        case .low: return "Reads, or a shell command with no obvious side effect."
        case .medium: return "Writes files, installs, or reaches the network."
        case .high: return "Destructive, credentialed, or publishes something."
        }
    }
}
