import XCTest

@testable import CodeConnectKit

/// The Mac's list (W1a): what needs the person, a computer's count in the head's menu, and the list's words.
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

    /// A session still starting, nothing drawn yet, is working; one whose start failed before its agent existed is
    /// Failed, and needs the person: from the daemon's lifecycle, never with the ended ones.
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
        XCTAssertEqual(ids(rows.filter(ProjectList.needsYou)), ["failed"])
    }

    /// W1-2: a session at its usage limit waits on the person, whatever its last turn's status, and is counted among its
    /// computer's sessions; one that has ended for good does not.
    func testARunAtItsLimitNeedsYou() {
        let capped = row("capped", .doneUnreviewed, on: "m2", limited: true)
        let over = row("capped-over", .ended, on: "m2", limited: true, lifecycle: "exited")
        let stopped = row("capped-stopped", .ended, on: "m2", limited: true, lifecycle: "exited", relaunchable: true)
        XCTAssertEqual(
            ids((fleet + [capped, over, stopped]).filter(ProjectList.needsYou)),
            ["approval", "failed", "idle", "question", "capped", "capped-stopped"])
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
        XCTAssertEqual(RunListWords.runs(1), "1 session")
        XCTAssertEqual(RunListWords.runs(2), "2 sessions")
        XCTAssertEqual(RunListWords.runs(0), "0 sessions")
        XCTAssertEqual(RunListWords.needYou(1), "1 needs you")
        XCTAssertEqual(RunListWords.needYou(4), "4 need you")
        XCTAssertEqual(RunListWords.earlier(7), "7 earlier ›")
        XCTAssertEqual(RunListWords.quiet(["local-ai"]), "local-ai is hidden: nothing there needs you.")
        XCTAssertEqual(RunListWords.quiet(["a", "b"]), "2 projects are hidden: nothing there needs you.")
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
        XCTAssertEqual(RunListWords.offlineLine(lastSeen: nil, waiting: 2, now: now, calendar: calendar), "2 sessions")
        XCTAssertNil(RunListWords.offlineLine(lastSeen: nil, waiting: 0, now: now, calendar: calendar))
    }
}
