import XCTest

@testable import CodeConnectKit

/// What a removal of several runs takes is what the list shows as chosen: a run chosen and then hidden, by a filter or a
/// folded group, is neither counted nor removed, and is picked again once shown.
final class RunsToRemoveTests: XCTestCase {
    private func row(_ id: String, folder: String) -> FleetRow {
        let json = """
            {"session_uid":"\(id)","session_id":"\(id)","cwd":"\(folder)","lifecycle":"ended","link":"detached",\
            "last_seq":3,"created_at":"2026-07-31T09:14:00.000Z","updated_at":"2026-07-31T09:14:00.000Z"}
            """
        // swiftlint:disable:next force_try
        let summary = try! JSONDecoder().decode(SessionSummary.self, from: Data(json.utf8))
        return FleetRow(
            summary: summary, status: .ended, subtitle: "", activity: nil,
            label: RunLabel(project: folder, qualifier: nil), capability: .control, blockedCount: 0,
            lastActivityAt: nil, cachedAt: nil, machineID: "mac", machineName: "mac name")
    }

    /// Two projects on one computer: a and b in /a, c in /b.
    private lazy var rows = [row("a", folder: "/a"), row("b", folder: "/a"), row("c", folder: "/b")]
    private let projectA = "mac\n/a"

    private func picked(_ rows: [FleetRow], folded: Set<String> = [], chosen: Set<String>) -> [String] {
        FleetGroup.picked(rows, by: .project, folded: folded, chosen: chosen) { _ in true }
    }

    func testAChosenRunInAFoldedGroupIsNotPicked() {
        XCTAssertEqual(picked(rows, folded: [projectA], chosen: ["a", "c"]), ["c"])
    }

    func testAChosenRunAFilterLeavesOutIsNotPicked() {
        let onlyB = FleetGroup.groups(rows, by: .project).first { $0.id == "mac\n/b" }?.rows ?? []
        XCTAssertEqual(picked(onlyB, chosen: ["a", "b", "c"]), ["c"])
    }

    func testUnfoldingPicksItAgain() {
        let chosen: Set<String> = ["a", "c"]
        XCTAssertEqual(picked(rows, folded: [projectA], chosen: chosen), ["c"])
        XCTAssertEqual(picked(rows, chosen: chosen), ["a", "c"])
    }

    func testOnlyRemovableRunsCanBeChosen() {
        let choosable = FleetGroup.choosable(rows, by: .project, folded: []) { $0.id != "b" }
        XCTAssertEqual(choosable, ["a", "c"])
    }
}
