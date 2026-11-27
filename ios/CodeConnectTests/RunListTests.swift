import XCTest

@testable import CodeConnectKit

/// The Mac's run list (W1a, W1-1 and W1-6): Needs you, Working and Done, each in the fleet's order; Needs you whole on
/// every computer whatever is chosen; the ended runs apart, on every computer; and the list's words.
final class RunListTests: XCTestCase {
    /// A run in `status`; `limited`: the usage limit its last turn stopped at, as the daemon sends it; `relaunchable`:
    /// the next message starts it again.
    private func row(
        _ id: String, _ status: FleetStatus, on machine: String, limited: Bool = false, lifecycle: String = "running",
        relaunchable: Bool = false
    ) -> FleetRow {
        let limit = (limited ? #","limit":{"which":"five_hour","at":"2026-07-31T09:14:00.000Z"}"# : "")
            + (relaunchable ? #","relaunchable":true"# : "")
        let json = """
            {"session_uid":"\(id)","session_id":"\(id)","cwd":"/web","lifecycle":"\(lifecycle)","link":"attached",\
            "last_seq":3,"created_at":"2026-07-31T09:14:00.000Z","updated_at":"2026-07-31T09:14:00.000Z"\(limit)}
            """
        // swiftlint:disable:next force_try
        let summary = try! JSONDecoder().decode(SessionSummary.self, from: Data(json.utf8))
        return FleetRow(
            summary: summary, status: status, subtitle: "", activity: nil,
            label: RunLabel(project: "/web", qualifier: nil), capability: .control, blockedCount: 0,
            lastActivityAt: nil, cachedAt: nil, machineID: machine, machineName: "\(machine) name")
    }

    /// Every status, on two computers, in the fleet's order.
    private lazy var fleet = [
        row("approval", .blocked, on: "m1"), row("working", .running, on: "m1"), row("failed", .failed, on: "m2"),
        row("unseen", .doneUnreviewed, on: "m2"), row("idle", .idle, on: "m1"), row("ending", .stopping, on: "m2"),
        row("seen", .done, on: "m1"), row("over", .ended, on: "m2"), row("question", .blocked, on: "m2"),
        row("over-too", .ended, on: "m1"),
    ]

    private func ids(_ rows: [FleetRow]) -> [String] { rows.map(\.summary.sessionKey) }

    /// W1-1: each run in its section, in the fleet's order; the ended runs apart.
    func testEachRunIsInItsSectionInTheFleetsOrder() {
        let list = RunList(fleet, computer: nil)
        XCTAssertEqual(ids(list.needsYou), ["approval", "failed", "idle", "question"])
        XCTAssertEqual(ids(list.working), ["working", "ending"])
        XCTAssertEqual(ids(list.done), ["unseen", "seen"])
        XCTAssertEqual(ids(list.ended), ["over", "over-too"])
    }

    /// W1-6: narrowed to one computer, Working and Done hold its runs alone; Needs you, and the ended runs, every
    /// computer's.
    func testNeedsYouStaysWholeWhenOneComputerIsChosen() {
        let list = RunList(fleet, computer: "m1")
        XCTAssertEqual(ids(list.needsYou), ["approval", "failed", "idle", "question"])
        XCTAssertEqual(ids(list.working), ["working"])
        XCTAssertEqual(ids(list.done), ["seen"])
        XCTAssertEqual(ids(list.ended), ["over", "over-too"])
        let other = RunList(fleet, computer: "m3")
        XCTAssertEqual(ids(other.needsYou), ["approval", "failed", "idle", "question"])
        XCTAssertEqual(other.working, [])
        XCTAssertEqual(other.done, [])
    }

    /// A run still starting, nothing drawn yet, is Working; one whose start failed before its agent existed is Failed,
    /// in Needs you: from the daemon's lifecycle, never hidden behind "N ended ›".
    @MainActor func testAStartingRunWorksAndOneThatFailedToStartNeedsYou() throws {
        func summary(_ id: String, _ lifecycle: String) throws -> SessionSummary {
            let json = """
                {"session_uid":"\(id)","session_id":"\(id)","cwd":"/web","lifecycle":"\(lifecycle)","link":"detached",\
                "last_seq":1,"created_at":"2026-07-31T09:14:00.000Z","updated_at":"2026-07-31T09:14:00.000Z"}
                """
            return try JSONDecoder().decode(SessionSummary.self, from: Data(json.utf8))
        }
        let starting = try summary("starting", "starting")
        let failed = try summary("failed", "failed")
        XCTAssertEqual(FleetStatusRule.status(summary: starting, shown: nil, reviewedSeq: 0), .running)
        XCTAssertEqual(FleetStatusRule.status(summary: failed, shown: nil, reviewedSeq: 0), .failed)
        let rows = [
            row("starting", FleetStatusRule.status(summary: starting, shown: nil, reviewedSeq: 0), on: "m1"),
            row("failed", FleetStatusRule.status(summary: failed, shown: nil, reviewedSeq: 0), on: "m1"),
        ]
        let list = RunList(rows, computer: nil)
        XCTAssertEqual(ids(list.working), ["starting"])
        XCTAssertEqual(ids(list.needsYou), ["failed"])
        XCTAssertEqual(list.ended, [])
    }

    /// W1-2: a run at its usage limit waits on the person, whatever its last turn's status and whichever computer is
    /// chosen, in that list alone, and counted among its computer's runs; one that has ended for good is with the
    /// ended runs.
    func testARunAtItsLimitNeedsYou() {
        let capped = row("capped", .doneUnreviewed, on: "m2", limited: true)
        let over = row("capped-over", .ended, on: "m2", limited: true, lifecycle: "exited")
        let stopped = row("capped-stopped", .ended, on: "m2", limited: true, lifecycle: "exited", relaunchable: true)
        let list = RunList(fleet + [capped, over, stopped], computer: "m1")
        XCTAssertEqual(ids(list.needsYou), ["approval", "failed", "idle", "question", "capped", "capped-stopped"])
        XCTAssertEqual(ids(list.done), ["seen"])
        XCTAssertEqual(ids(RunList(fleet + [capped], computer: "m2").done), ["unseen"])
        XCTAssertEqual(ids(list.ended), ["over", "over-too", "capped-over"])
        XCTAssertEqual(RunList.activeCount(fleet + [capped, over, stopped], on: "m2"), 6)
        XCTAssertEqual(RunList.activeCount(fleet + [capped, over, stopped], on: nil), 10, "every computer's")
    }

    /// The head's menu counts a computer's runs that have not ended, the ones the list shows of it.
    func testAComputersCountIsItsRunsNotEnded() {
        XCTAssertEqual(RunList.activeCount(fleet, on: "m1"), 4)
        XCTAssertEqual(RunList.activeCount(fleet, on: "m2"), 4)
        XCTAssertEqual(RunList.activeCount(fleet, on: "m3"), 0)
    }

    /// The words follow the count (D3), and the list's fixed words are exact.
    func testTheListsWords() {
        XCTAssertEqual(RunListWords.runs(1), "1 run")
        XCTAssertEqual(RunListWords.runs(2), "2 runs")
        XCTAssertEqual(RunListWords.runs(0), "0 runs")
        XCTAssertEqual(RunListWords.ended(9), "9 ended ›")
        XCTAssertEqual(RunListWords.noneOn("lumen5090"), "None on lumen5090")
        XCTAssertEqual(RunListWords.allComputers, "all computers")
        XCTAssertEqual(RunListWords.allTitle, "All computers")
        XCTAssertEqual(RunListWords.offline, "· Offline")
    }

    /// D2 and D4: a time on the same day reads as its clock time, an earlier or later day as the day; an offline
    /// computer's line names when it was last seen, else how many runs wait for it, else nothing.
    func testTimesReadAsTheClockTodayAndTheDayElse() throws {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = try XCTUnwrap(TimeZone(identifier: "UTC"))
        calendar.locale = Locale(identifier: "en_US")
        let now = Date(timeIntervalSince1970: 1_795_660_000)  // Thu 26 Nov 2026, 02:26 UTC
        // Foundation sets a time's AM/PM apart with a narrow no-break space (U+202F).
        XCTAssertEqual(Format.clock(now.addingTimeInterval(16 * 3600), now: now, calendar: calendar), "6:26\u{202F}PM")
        XCTAssertEqual(Format.clock(now.addingTimeInterval(4 * 86400), now: now, calendar: calendar), "Mon")
        XCTAssertEqual(Format.clock(now.addingTimeInterval(-3 * 86400), now: now, calendar: calendar), "Mon")
        XCTAssertEqual(
            RunListWords.offlineLine(lastSeen: now.addingTimeInterval(-60), waiting: 3, now: now, calendar: calendar),
            "Last seen 2:25\u{202F}AM")
        XCTAssertEqual(RunListWords.offlineLine(lastSeen: nil, waiting: 2, now: now, calendar: calendar), "2 runs")
        XCTAssertNil(RunListWords.offlineLine(lastSeen: nil, waiting: 0, now: now, calendar: calendar))
    }
}
