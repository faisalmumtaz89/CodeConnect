import XCTest

@testable import CodeConnectKit

/// The Mac's run list (W1a, W1-1 and W1-6): Needs you, Working and Done, each in the fleet's order; Needs you whole on
/// every computer whatever is chosen; the ended runs apart, on every computer; and the list's words.
final class RunListTests: XCTestCase {
    private func row(_ id: String, _ status: FleetStatus, on machine: String) -> FleetRow {
        let json = """
            {"session_uid":"\(id)","session_id":"\(id)","cwd":"/web","lifecycle":"running","link":"attached",\
            "last_seq":3,"created_at":"2026-07-31T09:14:00.000Z","updated_at":"2026-07-31T09:14:00.000Z"}
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
        XCTAssertEqual(Format.clock(now.addingTimeInterval(16 * 3600), now: now, calendar: calendar), "6:26 PM")
        XCTAssertEqual(Format.clock(now.addingTimeInterval(4 * 86400), now: now, calendar: calendar), "Mon")
        XCTAssertEqual(Format.clock(now.addingTimeInterval(-3 * 86400), now: now, calendar: calendar), "Mon")
        XCTAssertEqual(
            RunListWords.offlineLine(lastSeen: now.addingTimeInterval(-60), waiting: 3, now: now, calendar: calendar),
            "Last seen 2:25 AM")
        XCTAssertEqual(RunListWords.offlineLine(lastSeen: nil, waiting: 2, now: now, calendar: calendar), "2 runs")
        XCTAssertNil(RunListWords.offlineLine(lastSeen: nil, waiting: 0, now: now, calendar: calendar))
    }
}
