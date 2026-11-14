import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// The Kit half of the per-computer CPU/memory/disk overview (owner decision 199):
/// `AppModel.machineStats`, the `keepMachineStatsFresh()` the Mac sidebar drives,
/// and the clear-on-drop chain. The daemon's own bytes come from `minor-31-wire.json`.
@MainActor
final class MachineStatsTests: XCTestCase {

    private final class Sent: @unchecked Sendable {
        var sent: [ClientMessage] = []
        var getStatsCount: Int {
            sent.filter { if case .getMachineStats = $0 { return true } else { return false } }.count
        }
    }

    private func wire() throws -> [String: JSONValue] {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-31-wire", withExtension: "json"))
        return try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
    }
    /// The daemon's own machine_stats frame (byte for byte what ccd emits).
    private func statsFrame() throws -> ServerMessage {
        let value = try XCTUnwrap(try wire()["machine_stats"])
        return try JSONDecoder().decode(ServerMessage.self, from: Data(value.canonicalJSONString.utf8))
    }
    private func reportingCapabilities() throws -> Capabilities {
        let advertised = try XCTUnwrap(try wire()["hello_ack_capabilities"])
        return try JSONDecoder().decode(Capabilities.self, from: Data(advertised.canonicalJSONString.utf8))
    }

    private func connectedModel(reportingStats: Bool = true) throws -> (AppModel, Sent) {
        let model = AppModel(cache: EventCache())
        model.connection.simulateConnectedForTesting()
        let caps = try reportingStats
            ? reportingCapabilities()
            : Capabilities(canApproveReliably: true, sendText: true)
        model.connection.simulateCapabilitiesForTesting(caps, minor: reportingStats ? 31 : 23)
        XCTAssertEqual(caps.reportsMachineStats, reportingStats, "fixture capability setup")
        let wire = Sent()
        model.connection.sendStub = { message in MainActor.assumeIsolated { wire.sent.append(message) } }
        return (model, wire)
    }

    /// Let a just-started poll run its first iteration (the capability check and, if due, its first send).
    private func settle() async { for _ in 0..<10 { await Task.yield() } }

    /// Yield until `done` holds or a bound is reached — for an async effect like a cancelled poll's `defer`,
    /// whose continuation resumes after an indeterminate number of scheduler turns (not a fixed count).
    private func waitUntil(_ done: () -> Bool) async { for _ in 0..<5000 where !done() { await Task.yield() } }

    /// G1: while a view watches, the daemon's answer becomes `machineStats`.
    func testAnAnswerSetsMachineStatsWhileAViewWatches() async throws {
        let (model, _) = try connectedModel()
        let poll = Task { await model.keepMachineStatsFresh() }
        await settle()
        model.connection.injectForTesting(try statsFrame())
        XCTAssertEqual(model.machineStats?.cpu, 0.38)
        XCTAssertEqual(model.machineStats?.memoryTotal, 206_158_430_208)
        poll.cancel()
    }

    /// G6: an answer with no view watching is ignored — nothing holds a figure nobody shows.
    func testAnAnswerWithNoViewWatchingIsIgnored() throws {
        let (model, _) = try connectedModel()
        model.connection.injectForTesting(try statsFrame())
        XCTAssertNil(model.machineStats, "a figure with no view watching is not held")
    }

    /// G2: a dropped link clears the figure at once (the onDisconnected chain), so it is never shown as current.
    func testADropClearsMachineStats() async throws {
        let (model, _) = try connectedModel()
        let poll = Task { await model.keepMachineStatsFresh() }
        await settle()
        model.connection.injectForTesting(try statsFrame())
        XCTAssertNotNil(model.machineStats)
        model.connection.simulatePhaseForTesting(.connecting)
        model.connection.onDisconnected?()
        XCTAssertNil(model.machineStats, "a dropped link's figure is not shown as current")
        poll.cancel()
    }

    /// G3: a daemon that does not report stats is never asked.
    func testWithoutTheCapabilityItNeverAsks() async throws {
        let (model, wire) = try connectedModel(reportingStats: false)
        let poll = Task { await model.keepMachineStatsFresh() }
        await settle()
        XCTAssertEqual(wire.getStatsCount, 0, "an old daemon that doesn't report stats is never asked")
        poll.cancel()
    }

    /// G4: a second concurrent caller starts no second stream — one poll per model.
    func testASecondCallSendsNoSecondStream() async throws {
        let (model, wire) = try connectedModel()
        let a = Task { await model.keepMachineStatsFresh() }
        await settle()
        XCTAssertEqual(wire.getStatsCount, 1, "the first poll asked once")
        // Launched as a task, not awaited: the real code returns the second caller at once,
        // but a build that lost the guard would loop here — a task lets that surface as an
        // extra send (a failed assertion), never a hung test holding the build lock.
        let b = Task { await model.keepMachineStatsFresh() }
        await settle()
        XCTAssertEqual(wire.getStatsCount, 1, "the second caller started no second stream")
        a.cancel()
        b.cancel()
    }

    /// G5: when the view stops watching, the poll ends and the sample clears.
    func testCancellingTheViewClearsMachineStats() async throws {
        let (model, _) = try connectedModel()
        let poll = Task { await model.keepMachineStatsFresh() }
        await settle()
        model.connection.injectForTesting(try statsFrame())
        XCTAssertNotNil(model.machineStats)
        poll.cancel()
        await waitUntil { model.machineStats == nil }
        XCTAssertNil(model.machineStats, "the sample clears when the view stops watching")
    }
}
