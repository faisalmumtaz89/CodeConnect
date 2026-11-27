import XCTest

@testable import CodeConnectKit

/// The Mac's session list by project (Release 2, S1): projects from the sessions, their names, their order and their
/// rows; the one count of what needs the person; Only these, Pin, Hide, the computer filter, the reveal of ended
/// sessions and the open session held in place.
final class ProjectListTests: XCTestCase {
    private let t0 = Date(timeIntervalSince1970: 1_800_000_000)

    /// A session in `status`, in `cwd` on `machine`, last active `minutes` after t0.
    private func row(
        _ id: String, _ status: FleetStatus, cwd: String = "/work/web", on machine: String = "m1", minutes: Double = 0,
        lifecycle: String = "running", limited: Bool = false, archived: Bool = false
    ) -> FleetRow {
        let extra = (limited ? #","limit":{"which":"five_hour","at":"2026-07-31T09:14:00.000Z"}"# : "")
            + (archived ? #","archived":true"# : "")
        let json = """
            {"session_uid":"\(id)","session_id":"\(id)","cwd":"\(cwd)","lifecycle":"\(lifecycle)","link":"attached",\
            "last_seq":3,"created_at":"2026-07-31T09:14:00.000Z","updated_at":"2026-07-31T09:14:00.000Z"\(extra)}
            """
        // swiftlint:disable:next force_try
        let summary = try! JSONDecoder().decode(SessionSummary.self, from: Data(json.utf8))
        return FleetRow(
            summary: summary, status: status, subtitle: "", activity: nil,
            label: RunLabel(project: "web", qualifier: nil), capability: .control, blockedCount: 0,
            lastActivityAt: t0.addingTimeInterval(minutes * 60), cachedAt: nil, machineID: machine,
            machineName: "\(machine) name")
    }

    private func ids(_ rows: [FleetRow]) -> [String] { rows.map(\.id) }
    private func names(_ list: ProjectList) -> [String] { list.projects.map(\.name) }

    // MARK: Projects and names

    /// A session in a worktree of its own belongs to the folder that holds the worktree; a project is one folder on
    /// one computer, named by it.
    func testAWorktreeSessionJoinsItsFolder() {
        let list = ProjectList([
            row("main", .done, cwd: "/work/web"),
            row("wt", .running, cwd: "/work/web/.claude/worktrees/fix-login"),
            row("other", .done, cwd: "/work/api"),
        ])
        XCTAssertEqual(names(list).sorted(), ["api", "web"])
        XCTAssertEqual(ids(list.projects.first { $0.name == "web" }!.rows).sorted(), ["main", "wt"])
        XCTAssertEqual(list.projects.first { $0.name == "web" }!.folder, "/work/web")
    }

    /// Two projects with one name say their computers when those differ, else the folder each is in.
    func testTwoProjectsWithOneNameSayWhatTellsThemApart() {
        let computers = ProjectList([row("a", .done, on: "m1"), row("b", .done, on: "m2", minutes: 1)])
        XCTAssertEqual(names(computers), ["web · m2 name", "web · m1 name"])
        let folders = ProjectList([row("a", .done, cwd: "/a/web"), row("b", .done, cwd: "/b/web", minutes: 1)])
        XCTAssertEqual(names(folders), ["web · /b", "web · /a"])
    }

    // MARK: What needs you

    /// One count, every computer, whatever the computer filter, Only these, Pin or Hide do; an ended session's old
    /// limit does not count, and an archived session is not in the list at all.
    func testTheCountOfWhatNeedsYouIsOneAnswer() {
        let rows = [
            row("card", .blocked, on: "m1"), row("failed", .failed, cwd: "/work/api", on: "m2"),
            row("idle", .idle, cwd: "/work/app"), row("limit", .running, limited: true),
            row("old-limit", .ended, lifecycle: "exited", limited: true), row("work", .running),
            row("gone", .blocked, archived: true),
        ]
        let plain = ProjectList(rows)
        XCTAssertEqual(plain.needsYou, 4)
        for list in [
            ProjectList(rows, computer: "m3"), ProjectList(rows, onlyNeedsYou: true),
            ProjectList(rows, pinned: [ProjectList.key(rows[1])]), ProjectList(rows, hidden: [ProjectList.key(rows[0])]),
        ] { XCTAssertEqual(list.needsYou, 4) }
        XCTAssertFalse(plain.projects.flatMap(\.rows).contains { $0.id == "gone" })
    }

    /// Narrowed to one computer, a project shows that computer's sessions, and still what needs you on another.
    func testTheComputerFilterNeverHidesWhatNeedsYou() {
        let list = ProjectList(
            [row("here", .done, on: "m1"), row("there", .done, on: "m2"), row("card", .blocked, on: "m2")],
            computer: "m1")
        XCTAssertEqual(Set(list.projects.flatMap(\.rows).map(\.id)), ["here", "card"])
    }

    /// Only these keeps what needs you under its projects, names the projects it leaves out, and turns itself off
    /// once nothing needs you.
    func testOnlyTheseKeepsWhatNeedsYouAndTurnsItselfOff() {
        let rows = [row("card", .blocked), row("work", .running), row("api", .done, cwd: "/work/api")]
        let only = ProjectList(rows, onlyNeedsYou: true)
        XCTAssertTrue(only.onlyNeedsYou)
        XCTAssertEqual(ids(only.projects.flatMap(\.rows)), ["card"])
        XCTAssertEqual(only.quiet, ["api"])
        let none = ProjectList([row("work", .running)], onlyNeedsYou: true)
        XCTAssertFalse(none.onlyNeedsYou)
        XCTAssertEqual(ids(none.projects.flatMap(\.rows)), ["work"])
    }

    // MARK: Order

    /// Within a project: waiting, failed or at its limit, idle, working, done (unseen first), then ended; newest
    /// first within each.
    func testAProjectsSessionsAreInTheOrderTheyNeedYou() {
        let list = ProjectList([
            row("done", .done, minutes: 9), row("idle", .idle), row("work-old", .running, minutes: 1),
            row("work-new", .running, minutes: 5), row("card", .blocked), row("unseen", .doneUnreviewed),
            row("limit", .running, limited: true), row("failed", .failed, minutes: 2), row("stop", .stopping, minutes: 3),
        ])
        XCTAssertEqual(
            ids(list.projects[0].rows),
            ["card", "failed", "limit", "idle", "work-new", "stop", "work-old", "unseen", "done"])
    }

    /// Projects: what needs you first, the most urgent, then the oldest wait; then the latest activity; a pinned
    /// project before all of them.
    func testProjectsAreInTheOrderTheyNeedYou() {
        let rows = [
            row("quiet", .done, cwd: "/p/quiet", minutes: 50), row("busy", .running, cwd: "/p/busy", minutes: 40),
            row("idle", .idle, cwd: "/p/idle"), row("old-card", .blocked, cwd: "/p/old"),
            row("new-card", .blocked, cwd: "/p/new"),
        ]
        let waiting = ["old-card": t0, "new-card": t0.addingTimeInterval(60)]
        XCTAssertEqual(names(ProjectList(rows, waitingSince: waiting)), ["old", "new", "idle", "quiet", "busy"])
        let pinned = ProjectList(rows, pinned: [ProjectList.key(rows[1])], waitingSince: waiting)
        XCTAssertEqual(names(pinned), ["busy", "old", "new", "idle", "quiet"])
        XCTAssertTrue(pinned.projects[0].pinned)
    }

    /// The open session stays where it was while it is open, and its project too, after its card is answered.
    func testAnAnsweredOpenSessionStaysWhereItWas() {
        let before = ProjectList([
            row("card", .blocked, cwd: "/p/a"), row("work", .running, cwd: "/p/a", minutes: 5),
            row("other", .blocked, cwd: "/p/b"),
        ], waitingSince: ["card": t0, "other": t0.addingTimeInterval(60)])
        XCTAssertEqual(names(before), ["a", "b"])
        XCTAssertEqual(ids(before.projects[0].rows), ["card", "work"])
        let held = ProjectList.Held(session: "card", project: before.projects[0].id, projectIndex: 0, rowIndex: 0)
        let after = ProjectList([
            row("card", .running, cwd: "/p/a", minutes: 1), row("work", .running, cwd: "/p/a", minutes: 5),
            row("other", .blocked, cwd: "/p/b"),
        ], held: held)
        XCTAssertEqual(names(after), ["a", "b"])
        XCTAssertEqual(ids(after.projects[0].rows), ["card", "work"])
        let free = ProjectList([
            row("card", .running, cwd: "/p/a", minutes: 1), row("work", .running, cwd: "/p/a", minutes: 5),
            row("other", .blocked, cwd: "/p/b"),
        ])
        XCTAssertEqual(names(free), ["b", "a"], "not held, both move")
        XCTAssertEqual(ids(free.projects[1].rows), ["work", "card"])
    }

    // MARK: Ended sessions, Hide

    /// Ended sessions wait behind "N earlier", shown 25 at a time; a project with nothing live opens straight to
    /// them.
    func testEndedSessionsAreRevealedTwentyFiveAtATime() {
        let ended = (0..<30).map { row("e\($0)", .ended, minutes: Double($0), lifecycle: "exited") }
        let live = ProjectList(ended + [row("work", .running)])
        XCTAssertEqual(ids(live.projects[0].rows), ["work"])
        XCTAssertEqual(live.projects[0].earlier, 30)
        let more = ProjectList(ended + [row("work", .running)], revealed: [live.projects[0].id: ProjectList.page])
        XCTAssertEqual(more.projects[0].rows.count, 26)
        XCTAssertEqual(more.projects[0].rows[1].id, "e29")
        XCTAssertEqual(more.projects[0].earlier, 5)
        let allEnded = ProjectList(ended)
        XCTAssertEqual(allEnded.projects[0].rows.count, 25)
        XCTAssertEqual(allEnded.projects[0].earlier, 5)
    }

    /// A hidden project is out of the list, and back while something in it needs you.
    func testAHiddenProjectComesBackWhileItNeedsYou() {
        let quiet = [row("done", .done, cwd: "/p/a"), row("other", .done, cwd: "/p/b")]
        let key = ProjectList.key(quiet[0])
        XCTAssertEqual(names(ProjectList(quiet, hidden: [key])), ["b"])
        let needs = ProjectList(quiet + [row("card", .blocked, cwd: "/p/a")], hidden: [key])
        XCTAssertEqual(names(needs), ["a", "b"])
        XCTAssertTrue(needs.projects[0].hidden)
    }

    /// RunList and the projects say "needs you" by one rule.
    func testRunListAndProjectsShareOneRule() {
        let rows = [
            row("card", .blocked), row("idle", .idle), row("work", .running), row("limit", .running, limited: true),
        ]
        XCTAssertEqual(ids(RunList(rows, computer: nil).needsYou), ids(rows.filter(ProjectList.needsYou)))
    }
}
