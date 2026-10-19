import Foundation

/// A background task's report, as Claude Code records it.
///
/// When a background agent or command stops, Claude Code writes a `type:"user"`
/// line stamped `origin.kind: "task-notification"` whose content is markup for
/// the model — `<task-notification>` holding `<task-id>`, `<tool-use-id>`,
/// `<output-file>`, `<status>`, `<summary>` and, for an agent, `<note>`,
/// `<result>` and `<usage>`. The Mac never shows that markup: its TUI draws the
/// line as one row, `⏺ Agent "Audit LLM decider and step loop" finished · 4m 3s`
/// — the summary, and the agent's running time when it has one. Rendered as a
/// person's words it was a YOU bubble of raw XML, often thousands of
/// characters of a subagent's report.
///
/// Recognition and the row follow Claude Code 2.1.304: the content starts with
/// the tag and nothing follows its close, the summary is decoded the way the
/// TUI decodes it, and the time is formatted the way it formats it. The fields
/// are read in the one form Claude Code writes them — a bare `<summary>`, an
/// integer `<duration_ms>`, string content — which is every one of 14,163
/// reports in the owner's transcripts; the TUI's reader is more forgiving of
/// forms it never writes.
struct TaskNotification: Equatable {
    /// `<summary>`, decoded. Nil when the report carries none.
    let summary: String?
    /// `<status>`: `completed`, `failed`, `killed`, `stopped`, or absent.
    let status: String?
    /// `<usage><duration_ms>`, which only an agent's report carries.
    let durationMS: Int?

    /// Recognises one transcript content string. Like the TUI, the content
    /// must *be* a notification — start with the tag, after leading
    /// whitespace, and end at its close — so a prompt that quotes one is never
    /// rewritten (measured: 486 `/loop` prompts carry the literal tag mid-text).
    static func parse(_ content: String) -> TaskNotification? {
        let trimmed = content.drop(while: \.isWhitespace)
        guard trimmed.hasPrefix("<task-notification"),
            let close = trimmed.range(of: "</task-notification>"),
            trimmed[close.upperBound...].allSatisfy(\.isWhitespace)
        else { return nil }
        let body = trimmed[..<close.lowerBound]
        return TaskNotification(
            summary: innerText(of: "summary", in: body).map(decodeEntities),
            status: innerText(of: "status", in: body),
            durationMS: innerText(of: "duration_ms", in: body).flatMap { Int($0) })
    }

    /// The Mac's row: the summary, then ` · ` and the running time when it is
    /// above zero.
    var line: String? {
        guard let summary, !summary.isEmpty else { return nil }
        guard let durationMS, durationMS > 0 else { return summary }
        return "\(summary) · \(Self.elapsed(ms: durationMS))"
    }

    /// The TUI colours the row's dot by status and nothing else.
    var severity: NoticeSeverity {
        switch status {
        case "completed": return .success
        case "failed": return .failure
        case "killed": return .warning
        default: return .info
        }
    }

    /// Claude Code's duration format: whole seconds under a minute, then
    /// `4m 3s`, `1h 2m 3s`, `1d 2h 3m`, the seconds rounded and carried.
    static func elapsed(ms: Int) -> String {
        if ms < 60_000 { return "\(ms / 1000)s" }
        var days = ms / 86_400_000
        var hours = ms % 86_400_000 / 3_600_000
        var minutes = ms % 3_600_000 / 60_000
        var seconds = (ms % 60_000 + 500) / 1000
        if seconds == 60 { seconds = 0; minutes += 1 }
        if minutes == 60 { minutes = 0; hours += 1 }
        if hours == 24 { hours = 0; days += 1 }
        if days > 0 { return "\(days)d \(hours)h \(minutes)m" }
        if hours > 0 { return "\(hours)h \(minutes)m \(seconds)s" }
        return "\(minutes)m \(seconds)s"
    }

    /// The three entities the TUI decodes, in one pass, as it does: a summary
    /// quoting a shell command reads `2>/dev/null && …`, not `2&gt;/dev/null`.
    private static func decodeEntities(_ text: String) -> String {
        var out = ""
        var rest = text[...]
        while let amp = rest.firstIndex(of: "&") {
            out += rest[..<amp]
            let tail = rest[amp...]
            if let (entity, character) = [("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">")]
                .first(where: { tail.hasPrefix($0.0) })
            {
                out += character
                rest = tail.dropFirst(entity.count)
            } else {
                out += "&"
                rest = tail.dropFirst()
            }
        }
        return out + rest
    }

    private static func innerText(of name: String, in text: Substring) -> String? {
        guard let open = text.range(of: "<\(name)>"),
            let close = text[open.upperBound...].range(of: "</\(name)>")
        else { return nil }
        return String(text[open.upperBound..<close.lowerBound])
    }
}
