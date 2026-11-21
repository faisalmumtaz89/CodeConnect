import XCTest

@testable import CodeConnectKit

/// A list narrowed to one computer or one project keeps that group's runs, in the fleet's order, and nothing else: a
/// project is one folder on one computer, and a group that has gone leaves nothing.
final class FleetFilterTests: XCTestCase {
    private func row(_ id: String, machine: String, folder: String) -> FleetRow {
        let json = """
            {"session_uid":"\(id)","session_id":"\(id)","cwd":"\(folder)","lifecycle":"running","link":"attached",\
            "last_seq":3,"created_at":"2026-07-31T09:14:00.000Z","updated_at":"2026-07-31T09:14:00.000Z"}
            """
        // swiftlint:disable:next force_try
        let summary = try! JSONDecoder().decode(SessionSummary.self, from: Data(json.utf8))
        return FleetRow(
            summary: summary, status: .running, subtitle: "", activity: nil,
            label: RunLabel(project: folder, qualifier: nil), capability: .control, blockedCount: 0,
            lastActivityAt: nil, cachedAt: nil, machineID: machine, machineName: "\(machine) name")
    }

    /// a and c on m1 in /web, b on m2 in /web, d on m1 in /api.
    private lazy var rows = [
        row("a", machine: "m1", folder: "/web"), row("b", machine: "m2", folder: "/web"),
        row("c", machine: "m1", folder: "/web"), row("d", machine: "m1", folder: "/api"),
    ]

    private func ids(_ filter: FleetFilter) -> [String] { filter.narrow(rows).map(\.summary.sessionKey) }

    func testAComputerKeepsItsRunsInOrder() {
        XCTAssertEqual(ids(.computer("m1")), ["a", "c", "d"])
        XCTAssertEqual(ids(.computer("m2")), ["b"])
    }

    func testAProjectIsOneFolderOnOneComputer() {
        XCTAssertEqual(ids(.project("m1\n/web")), ["a", "c"])
        XCTAssertEqual(ids(.project("m2\n/web")), ["b"])
    }

    func testAGroupThatHasGoneLeavesNothing() {
        XCTAssertEqual(ids(.computer("m3")), [])
        XCTAssertEqual(ids(.project("m1\n/old")), [])
    }
}
