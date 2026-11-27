import XCTest

@testable import CodeConnectKit

/// The Mac's session list by project (Release 2, S1): projects from the sessions, their names, their order and their
/// rows; the one count of what needs the person; Only these, Pin, Hide, the computer filter, the reveal of ended
/// sessions and the open session held in place.
final class ProjectListTests: XCTestCase {
    private let t0 = Date(timeIntervalSince1970: 1_800_000_000)

    /// A session in `status`, in `cwd` on `machine` (named `name`), last active `minutes` after t0.
    private func row(
        _ id: String, _ status: FleetStatus, cwd: String = "/work/web", on machine: String = "m1", minutes: Double = 0,
        lifecycle: String = "running", limited: Bool = false, archived: Bool = false, name: String? = nil,
        repo: String? = nil
    ) -> FleetRow {
        let extra = (limited ? #","limit":{"which":"five_hour","at":"2026-07-31T09:14:00.000Z"}"# : "")
            + (archived ? #","archived":true"# : "") + (repo.map { #","repo":"# + $0 } ?? "")
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
            machineName: name ?? "\(machine) name")
    }

    private func ended(_ id: String, cwd: String = "/work/web", minutes: Double = 0) -> FleetRow {
        row(id, .ended, cwd: cwd, minutes: minutes, lifecycle: "exited")
    }

    private func ids(_ rows: [FleetRow]) -> [String] { rows.map(\.id) }
    private func names(_ list: ProjectList) -> [String] { list.projects.map(\.name) }

    // MARK: Projects and names

    /// A session in a worktree of its own (the worktree its folder's last component) belongs to the folder that
    /// holds the worktree; a folder under the worktree is a project of its own.
    func testAWorktreeSessionJoinsItsFolder() {
        let list = ProjectList([
            row("wt", .running, cwd: "/work/web/.claude/worktrees/fix-login/", minutes: 1),
            row("main", .done, cwd: "/work/web"),
            row("deep", .done, cwd: "/work/web/.claude/worktrees/fix-login/src"),
        ])
        let web = list.projects.first { $0.folder == "/work/web" }!
        XCTAssertEqual(web.name, "web")
        XCTAssertEqual(ids(web.rows), ["wt", "main"])
        XCTAssertEqual(list.projects.first { $0.name == "src" }.map { ids($0.rows) }, ["deep"])
    }

    /// With the daemon's repository fact a project is its repository: one GitHub remote is one project on every
    /// computer, whatever its folders; without a remote, its main checkout on its computer, an outside worktree too; a
    /// session without the fact joins the project whose main checkout is its folder on its computer.
    func testAProjectIsItsRepository() {
        let web = #"{"name":"web","remote":"github.com/o/web","main_path":"/src/web"}"#
        let copy = #"{"name":"web-copy","remote":"github.com/o/web","main_path":"/home/f/web-copy"}"#
        let list = ProjectList([
            row("m1", .running, cwd: "/src/web/.claude/worktrees/a", on: "m1", minutes: 3, repo: web),
            row("m2", .done, cwd: "/home/f/web-copy", on: "m2", minutes: 2, repo: copy),
            row("old", .done, cwd: "/src/web", on: "m1", minutes: 1),
            row("other", .done, cwd: "/src/web", on: "m3"),
            row("ext", .done, cwd: "/wt/feature", on: "m1", repo: #"{"name":"api","main_path":"/src/api"}"#),
            row("api", .done, cwd: "/src/api", on: "m1", minutes: -1),
        ])
        XCTAssertEqual(list.projects.map { ids($0.rows) }, [["m1", "m2", "old"], ["ext", "api"], ["other"]])
        XCTAssertEqual(list.projects.map(\.sessions), [3, 2, 1])
        XCTAssertEqual(list.projects[1].name, "api")
        XCTAssertEqual(list.projects[1].folder, "/src/api")
    }

    /// Two projects with one name say their computers when no other of that name is on it, else the folder each is
    /// in; a hidden project lends no one a suffix; suffixes are drawn on one line.
    func testTwoProjectsWithOneNameSayWhatTellsThemApart() {
        let computers = ProjectList([row("a", .done, on: "m1"), row("b", .done, on: "m2", minutes: 1)])
        XCTAssertEqual(names(computers), ["web · m2 name", "web · m1 name"])
        let folders = ProjectList([row("a", .done, cwd: "/a/web"), row("b", .done, cwd: "/b/web", minutes: 1)])
        XCTAssertEqual(names(folders), ["web · /b", "web · /a"])
        let three = ProjectList([
            row("m1src", .done, cwd: "/src/web", on: "m1", minutes: 3),
            row("m2src", .done, cwd: "/src/web", on: "m2", minutes: 2),
            row("m1tmp", .done, cwd: "/tmp/web", on: "m1", minutes: 1),
        ])
        XCTAssertEqual(names(three), ["web · /src", "web · m2 name", "web · /tmp"])
        let hiddenOld = [row("new", .done, cwd: "/work/web"), row("old", .done, cwd: "/old/web")]
        XCTAssertEqual(names(ProjectList(hiddenOld, hidden: [ProjectList.key(hiddenOld[1])])), ["web"])
        let odd = ProjectList([
            row("a", .done, on: "m1", name: "Mac\u{2028}\u{202E}Studio"), row("b", .done, on: "m2", minutes: 1),
        ])
        XCTAssertFalse(names(odd).contains { $0.contains("\u{2028}") || $0.contains("\u{202E}") })
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
        XCTAssertEqual(
            RunList(rows.filter { !$0.summary.archived }, computer: nil).needsYou.count, 4,
            "the list's sections count the same")
        XCTAssertFalse(plain.projects.flatMap(\.rows).contains { $0.id == "gone" })
    }

    /// Narrowed to one computer, a project shows that computer's sessions, and still what needs you on another.
    func testTheComputerFilterNeverHidesWhatNeedsYou() {
        let list = ProjectList(
            [row("here", .done, on: "m1"), row("there", .done, on: "m2"), row("card", .blocked, on: "m2")],
            computer: "m1")
        XCTAssertEqual(Set(list.projects.flatMap(\.rows).map(\.id)), ["here", "card"])
    }

    /// Only these keeps what needs you under its projects, names the projects it leaves out in name order, and turns
    /// itself off once nothing needs you.
    func testOnlyTheseKeepsWhatNeedsYouAndTurnsItselfOff() {
        let rows = [
            row("card", .blocked), row("work", .running), row("zed", .done, cwd: "/work/zed", minutes: 1),
            row("api", .done, cwd: "/work/api"),
        ]
        let only = ProjectList(rows, onlyNeedsYou: true)
        XCTAssertTrue(only.onlyNeedsYou)
        XCTAssertEqual(ids(only.projects.flatMap(\.rows)), ["card"])
        XCTAssertEqual(only.quiet, ["api", "zed"])
        let none = ProjectList([row("work", .running)], onlyNeedsYou: true)
        XCTAssertFalse(none.onlyNeedsYou)
        XCTAssertEqual(ids(none.projects.flatMap(\.rows)), ["work"])
    }

    // MARK: Order

    /// Within a project: waiting (at its limit too), failed or at its limit, idle, working, done (unseen first), then
    /// ended; newest first within each.
    func testAProjectsSessionsAreInTheOrderTheyNeedYou() {
        let list = ProjectList([
            row("done", .done, minutes: 9), row("idle", .idle), row("work-old", .running, minutes: 1),
            row("work-new", .running, minutes: 5), row("card", .blocked), row("unseen", .doneUnreviewed),
            row("limit", .running, limited: true), row("failed", .failed, minutes: 2), row("stop", .stopping, minutes: 3),
            row("card-at-limit", .blocked, minutes: 1, limited: true),
        ])
        XCTAssertEqual(
            ids(list.projects[0].rows),
            ["card-at-limit", "card", "failed", "limit", "idle", "work-new", "stop", "work-old", "unseen", "done"])
    }

    /// Projects: what needs you first, the most urgent, then within that rank the Deck's order for cards (or the
    /// oldest); then the latest activity of any of its sessions, shown or not; a pinned project before all of them.
    func testProjectsAreInTheOrderTheyNeedYou() {
        let rows = [
            row("quiet", .done, cwd: "/p/quiet", minutes: 50), row("busy", .running, cwd: "/p/busy", minutes: 40),
            row("idle", .idle, cwd: "/p/idle"), row("old-card", .blocked, cwd: "/p/old"),
            row("new-card", .blocked, cwd: "/p/new"), row("old-idle", .idle, cwd: "/p/new", minutes: -600),
        ]
        let deck = ["old-card", "new-card"]
        XCTAssertEqual(names(ProjectList(rows, deck: deck)), ["old", "new", "idle", "quiet", "busy"])
        XCTAssertEqual(names(ProjectList(rows, deck: deck.reversed())).prefix(2), ["new", "old"], "the Deck decides")
        let pinned = ProjectList(rows, pinned: [ProjectList.key(rows[1])], deck: deck)
        XCTAssertEqual(names(pinned), ["busy", "old", "new", "idle", "quiet"])
        XCTAssertTrue(pinned.projects[0].pinned)
        // Cards not yet in the Deck, and failures: the oldest first, counted within the most urgent rank alone.
        let unranked = [
            row("late-card", .blocked, cwd: "/q/late", minutes: 10), row("early-card", .blocked, cwd: "/q/early"),
        ]
        XCTAssertEqual(names(ProjectList(unranked)), ["early", "late"])
        XCTAssertEqual(names(ProjectList(unranked, deck: ["late-card"])), ["late", "early"], "a card in the Deck first")
        let failures = [
            row("new-fail", .failed, cwd: "/f/new", minutes: 5), row("old-idle", .idle, cwd: "/f/new", minutes: -600),
            row("old-fail", .failed, cwd: "/f/old"),
        ]
        XCTAssertEqual(names(ProjectList(failures)), ["old", "new"])
    }

    /// Narrowing to a computer changes what a project shows, not how big it is or where it stands.
    func testTheComputerFilterMovesNoProject() {
        let rows = [
            row("a-idle", .idle, cwd: "/p/a", on: "m2"), row("a-work", .running, cwd: "/p/a", on: "m2", minutes: 120),
            row("b-idle", .idle, cwd: "/p/b", on: "m1"), row("b-work", .running, cwd: "/p/b", on: "m1", minutes: 60),
        ]
        XCTAssertEqual(names(ProjectList(rows)), ["a", "b"])
        let narrowed = ProjectList(rows, computer: "m1")
        XCTAssertEqual(names(narrowed), ["a", "b"])
        XCTAssertEqual(narrowed.projects[0].sessions, 2)
        XCTAssertEqual(ids(narrowed.projects[0].rows), ["a-idle"])
    }

    /// Revealing a project's earlier sessions moves no project: activity counts every session, shown or not.
    func testRevealingEarlierSessionsMovesNoProject() {
        let rows = [
            row("a-live", .running, cwd: "/p/a", minutes: 0), ended("a-ended", cwd: "/p/a", minutes: 20),
            row("b-live", .running, cwd: "/p/b", minutes: 10),
        ]
        let closed = ProjectList(rows)
        XCTAssertEqual(names(closed), ["a", "b"])
        XCTAssertEqual(names(ProjectList(rows, revealed: [closed.projects[0].id: 1])), ["a", "b"])
    }

    /// The open session stays where it was while it is open, and its project too: after its card is answered, when
    /// the list is narrowed to another computer, and when it ends.
    func testAnOpenSessionStaysWhereItWas() {
        let before = ProjectList([
            row("card", .blocked, cwd: "/p/a", on: "m2"), row("work", .running, cwd: "/p/a", on: "m2", minutes: 5),
            row("other", .blocked, cwd: "/p/b"),
        ], computer: "m1", deck: ["card", "other"])
        XCTAssertEqual(names(before), ["a", "b"])
        let held = ProjectList.Held(session: "card", projectIndex: 0, rowIndex: 0)
        let answered = [
            row("card", .running, cwd: "/p/a", on: "m2", minutes: 1), row("work", .running, cwd: "/p/a", on: "m2", minutes: 5),
            row("other", .blocked, cwd: "/p/b"),
        ]
        let after = ProjectList(answered, computer: "m1", held: held)
        XCTAssertEqual(names(after), ["a", "b"])
        XCTAssertEqual(ids(after.projects[0].rows), ["card"])
        XCTAssertEqual(names(ProjectList(answered, computer: "m1")), ["b"], "not held, it goes")
        let ends = [
            row("card", .ended, cwd: "/p/a", on: "m2", minutes: 1, lifecycle: "exited"),
            row("work", .running, cwd: "/p/a", on: "m2", minutes: 5), row("other", .blocked, cwd: "/p/b"),
        ]
        let endedHeld = ProjectList(ends, held: held)
        XCTAssertEqual(ids(endedHeld.projects[0].rows), ["card", "work"])
        XCTAssertEqual(endedHeld.projects[0].earlier, 0)
        XCTAssertEqual(ProjectList(ends).projects.first { $0.name == "a" }!.earlier, 1, "not held, it is earlier")
        // Held and ended beside 25 older ones: a reveal shows all 25, the held one taking no place in it.
        let older = (0..<25).map { row("old\($0)", .ended, cwd: "/p/a", on: "m2", minutes: -Double($0), lifecycle: "exited") }
        let page = ProjectList(ends + older, revealed: [ProjectList.key(ends[1]): 1], held: held)
        XCTAssertEqual(page.projects[0].rows.count, 27)
        XCTAssertEqual(page.projects[0].earlier, 0)
        // Held at the second row of the third project, where its own state would put it first in the second: both
        // places kept, each on its own.
        let three = [
            row("first-card", .blocked, cwd: "/h/one"), row("lead", .running, cwd: "/h/two", minutes: 9),
            row("opened", .blocked, cwd: "/h/two", minutes: 1), row("third", .blocked, cwd: "/h/three"),
        ]
        let deck = ["third", "opened", "first-card"]
        let free = ProjectList(three, deck: deck)
        XCTAssertEqual(names(free), ["three", "two", "one"])
        XCTAssertEqual(ids(free.projects[1].rows), ["opened", "lead"])
        let kept = ProjectList(three, deck: deck, held: ProjectList.Held(session: "opened", projectIndex: 2, rowIndex: 1))
        XCTAssertEqual(names(kept), ["three", "one", "two"])
        XCTAssertEqual(ids(kept.projects[2].rows), ["lead", "opened"])
        // An ended session already shown, opened: nothing moves and nothing more is revealed.
        let over = (0..<30).map { row("x\($0)", .ended, cwd: "/h/old", minutes: Double($0), lifecycle: "exited") }
        let shownEnded = ProjectList(over, held: ProjectList.Held(session: "x29", projectIndex: 0, rowIndex: 0, ended: true))
        XCTAssertEqual(shownEnded.projects[0].rows.count, 25)
        XCTAssertEqual(shownEnded.projects[0].earlier, 5)
        // Its session gone, a project is held no longer.
        let gone = ProjectList([row("work", .running, cwd: "/p/a", on: "m2"), row("other", .blocked, cwd: "/p/b")], held: held)
        XCTAssertEqual(names(gone), ["b", "a"])
    }

    // MARK: Ended sessions, Hide

    /// Ended sessions wait behind "N earlier", shown 25 a reveal; a project with nothing live opens to its first 25,
    /// and its first reveal shows the rest.
    func testEndedSessionsAreRevealedTwentyFiveAtATime() {
        let over = (0..<30).map { ended("e\($0)", minutes: Double($0)) }
        let live = ProjectList(over + [row("work", .running)])
        XCTAssertEqual(ids(live.projects[0].rows), ["work"])
        XCTAssertEqual(live.projects[0].earlier, 30)
        let more = ProjectList(over + [row("work", .running)], revealed: [live.projects[0].id: 1])
        XCTAssertEqual(more.projects[0].rows.count, 26)
        XCTAssertEqual(more.projects[0].rows[1].id, "e29")
        XCTAssertEqual(more.projects[0].earlier, 5)
        let allEnded = ProjectList(over)
        XCTAssertEqual(allEnded.projects[0].rows.count, 25)
        XCTAssertEqual(allEnded.projects[0].earlier, 5)
        let clicked = ProjectList(over, revealed: [allEnded.projects[0].id: 1])
        XCTAssertEqual(clicked.projects[0].rows.count, 30)
        XCTAssertEqual(clicked.projects[0].earlier, 0)
    }

    /// A hidden project is out of the list; back while something in it needs you; and while its session is open,
    /// that session alone.
    func testAHiddenProjectComesBackWhileItNeedsYou() {
        let quiet = [row("done", .done, cwd: "/p/a"), row("more", .done, cwd: "/p/a"), row("other", .done, cwd: "/p/b")]
        let key = ProjectList.key(quiet[0])
        XCTAssertEqual(names(ProjectList(quiet, hidden: [key])), ["b"])
        let needs = ProjectList(quiet + [row("card", .blocked, cwd: "/p/a")], hidden: [key])
        XCTAssertEqual(names(needs), ["a", "b"])
        XCTAssertTrue(needs.projects[0].hidden)
        let held = ProjectList.Held(session: "done", projectIndex: 0, rowIndex: 0)
        let open = ProjectList(quiet, hidden: [key], held: held)
        XCTAssertEqual(ids(open.projects[0].rows), ["done"])
    }
}
