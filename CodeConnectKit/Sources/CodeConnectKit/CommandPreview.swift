import Foundation

/// A command cut to a preview's width, `git push … origin main`: as much of
/// its start as says what runs, and as much of its end as says what it runs
/// on, cut only between tokens. The end is never hidden, because that is where
/// a dangerous suffix sits (`… && rm -rf ~/work`); when the two cannot both
/// fit, the end wins and the start goes (`… origin main`). A command that fits
/// comes back unchanged.
///
/// `columns` is the preview's width in characters (the preview is monospaced).
/// Tokens are split on single spaces and joined back with them, so the
/// characters shown are the command's own.
public func commandPreview(_ command: String, columns: Int) -> String {
    guard command.count > columns else { return command }
    let tokens = command.split(separator: " ", omittingEmptySubsequences: false).map(String.init)
    let gap = " … "
    let lead = "… "
    func joined(_ part: ArraySlice<String>) -> String { part.joined(separator: " ") }
    func width(_ start: Int, _ end: Int) -> Int {
        let tail = joined(tokens[(tokens.count - end)...]).count
        return start == 0 ? lead.count + tail : joined(tokens[..<start]).count + gap.count + tail
    }
    // The last token alone does not fit: the end of it is what is left.
    guard tokens.count > 1, width(0, 1) <= columns else {
        return lead + String(command.suffix(max(0, columns - lead.count)))
    }
    var start = 0
    var end = 1
    // The end, then the verb (up to its first two tokens), then the rest of
    // the end, then the rest of the start, each only while it fits.
    func grow(start more: Int, end grow: Int) -> Bool {
        guard start + more + end + grow < tokens.count, width(start + more, end + grow) <= columns else {
            return false
        }
        start += more
        end += grow
        return true
    }
    _ = grow(start: 1, end: 0) && grow(start: 1, end: 0)
    while grow(start: 0, end: 1) {}
    while grow(start: 1, end: 0) {}
    return start == 0
        ? lead + joined(tokens[(tokens.count - end)...])
        : joined(tokens[..<start]) + gap + joined(tokens[(tokens.count - end)...])
}

/// Whether a tool's principal argument is a command line, which a preview cuts
/// with `commandPreview`, rather than a path or a query, which keeps its end
/// by a head cut. Claude Code's `Bash` and Codex's `command`.
public func isCommandTool(_ tool: String) -> Bool {
    ["Bash", "BashOutput", "command"].contains(tool)
}
