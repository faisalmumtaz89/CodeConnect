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
    var toolName: String? { payload["tool_name"]?.stringValue }
    var toolInput: JSONValue? { payload["tool_input"] }
    /// Hook events carry it at the top level and also in `item_id`; a transcript
    /// `tool_result` hides it inside the message block. `item_id` is *not* a
    /// valid fallback for transcript events, where it holds the entry uuid.
    var toolUseID: String? {
        if let direct = payload["tool_use_id"]?.stringValue { return direct }
        if source == .transcript { return transcriptToolResult?.toolUseID }
        return itemID
    }
    var toolResponse: JSONValue? { payload["tool_response"] }
    var durationMS: Int? { payload["duration_ms"]?.intValue }
    var permissionMode: String? { payload["permission_mode"]?.stringValue }
    var modelName: String? { payload["model"]?.stringValue }
    var cwd: String? { payload["cwd"]?.stringValue }

    // MARK: Approvals

    var approvalCard: ApprovalCard? { payload["card"]?.decoded(ApprovalCard.self) }
    var approvalOutcome: AnswerOutcome? { payload.decoded(AnswerOutcome.self) }

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

    /// A user turn's prose. `message.content` is a bare string for typed
    /// prompts and a block array when the CLI attaches context.
    var userText: String? {
        guard kind == .userMessage, let content = payload["message"]?["content"] else { return nil }
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
        guard kind == .agentMessage, let blocks = payload["message"]?["content"]?.arrayValue
        else { return nil }
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
