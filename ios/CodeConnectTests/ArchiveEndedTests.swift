import XCTest

@testable import CodeConnectKit

/// Archive N Ended Sessions… (artboard 48): from each ended session's repository as its computer read it, which go and
/// which stay, by Archive Session's own rule, and the card that asks.
final class ArchiveEndedTests: XCTestCase {
    private func row(_ id: String, title: String) throws -> FleetRow {
        let json = """
            {"session_uid":"\(id)","session_id":"\(id)","cwd":"/src/web/.claude/worktrees/\(id)","lifecycle":"exited",\
            "link":"attached","last_seq":3,"created_at":"2026-07-31T09:14:00.000Z","updated_at":"2026-07-31T09:14:00.000Z",\
            "title":"\(title)","in_worktree":true}
            """
        let summary = try JSONDecoder().decode(SessionSummary.self, from: Data(json.utf8))
        return FleetRow(
            summary: summary, status: .ended, subtitle: "", activity: nil, label: RunLabel(project: "web", qualifier: nil),
            capability: .control, blockedCount: 0, lastActivityAt: nil, cachedAt: nil, machineID: "m1", machineName: "Studio")
    }

    private func git(files: Int = 0, ahead: Int = 0) throws -> RunGit {
        let json = #"""
            {"branch":"fix/x","head_oid":"1234567","base":{"name":"main","remote":"origin","ref":"refs/remotes/origin/main"},
             "changed":{"files":\#(files),"added":0,"removed":0,"complete":true},"ahead":\#(ahead),
             "pushed":false,"unmerged":false,"read_at":"2026-11-26T12:00:00Z"}
            """#
        return try JSONDecoder().decode(RunGit.self, from: Data(json.utf8))
    }

    /// A clean worktree whose commits are on its base goes; uncommitted changes, commits not on the base, or a
    /// repository that couldn't be read keep a session, named with why.
    func testOnlyWhatArchiveSessionWouldArchiveGoes() throws {
        let rows = try [
            row("a", title: "Merged fix"), row("b", title: "Half done"), row("c", title: "Unmerged work"),
            row("d", title: "Far away"),
        ]
        let plan = ArchiveEnded(rows, gits: ["a": try git(), "b": try git(files: 2), "c": try git(ahead: 3)])
        XCTAssertEqual(plan.going.map(\.key), ["a"])
        XCTAssertEqual(plan.going.first?.branch, "fix/x")
        XCTAssertEqual(plan.going.first?.headOID, "1234567")
        XCTAssertEqual(plan.staying, [
            "“Half done” has uncommitted changes, so it stays.",
            "“Unmerged work” has commits not on its base, so it stays.",
            "“Far away”’s repository couldn’t be read, so it stays.",
        ])
    }

    /// The card in the owner's words: what goes, from where, that the branches go, the red button with its count.
    func testTheCardSaysWhatGoesAndWhatStays() {
        let ask = FleetWords.archiveEndedAsk(
            project: "CodeConnect", count: 6, computers: ["Studio"],
            staying: ["“Section header buttons on the card edge” has uncommitted changes, so it stays."])
        XCTAssertEqual(ask.title, "Archive 6 ended sessions in CodeConnect?")
        XCTAssertEqual(ask.items.map(\.words), [
            "They leave the list; Search still finds them", "6 worktrees are removed from Studio",
            "Their branches are deleted; their commits are already on the base",
        ])
        XCTAssertEqual(ask.note, "“Section header buttons on the card edge” has uncommitted changes, so it stays.")
        XCTAssertEqual([ask.cancel, ask.confirm], ["Cancel", "Archive 6"])
        XCTAssertEqual(FleetWords.archiveEndedAsk(project: "web", count: 1, computers: ["A", "B"], staying: []).items[1].words,
                       "1 worktree is removed from their computers")
        XCTAssertEqual(FleetWords.archiveEnded(6), "Archive 6 Ended Sessions…")
    }
}
