import SwiftUI
import UIKit
import XCTest

@testable import CodeConnect

/// What a catch-up's one change looks like on screen: the clock is true on the
/// first frame back, and an open chat lands on its caught-up rows in one step.
@MainActor
final class CatchUpScreenTests: XCTestCase {

    private let a = "01K1B3XQ8ZC0DE5FGH7JKMNPS1"

    private final class Sent {
        var sent: [ClientMessage] = []
    }

    private static let capabilities = Capabilities(
        canApproveReliably: true, sendText: true,
        extra: ["history_pages": .bool(true), "session_uid": .bool(true)])

    private func model() -> (AppModel, Sent) {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("screen-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(cache: EventCache(root: root))
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(Self.capabilities, minor: 23)
        let sent = Sent()
        model.connection.sendStub = { message in
            MainActor.assumeIsolated { sent.sent.append(message) }
        }
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        // The hosted chat marks this run reviewed in the real store.
        let key = a
        addTeardownBlock { ReviewMarks.forget(sessionKey: key) }
        return (model, sent)
    }

    private func frame(_ json: String) -> ServerMessage {
        // swiftlint:disable:next force_unwrapping
        Fixtures.decode(json)!
    }

    private func sessions(_ lastSeq: Int) -> ServerMessage {
        frame(
            """
            {"type":"sessions","sessions":[{"session_uid":"\(a)","session_id":"cc-1",\
            "tmux_session":"cc-1","cwd":"/tmp/a","lifecycle":"live","link":"attached",\
            "last_seq":\(lastSeq),"created_at":"2026-10-16T10:00:00Z",\
            "updated_at":"2026-10-16T10:00:00Z","blocked_on":[]}]}
            """)
    }

    private func inject(_ model: AppModel, _ seqs: ClosedRange<Int>) {
        for seq in seqs {
            model.connection.injectForTesting(
                frame(
                    """
                    {"type":"event","event":{"seq":\(seq),"session_uid":"\(a)","session_id":"cc-1",\
                    "ts":"2026-10-16T10:00:00Z","kind":"user_message","source":"transcript",\
                    "payload":{"message":{"content":"step \(seq)"}}}}
                    """))
        }
    }

    private func settle(_ model: AppModel) async {
        for _ in 0..<5 {
            for state in model.statesForTesting.values { await state.settleForTesting() }
            for _ in 0..<20 { await Task.yield() }
        }
    }

    /// A model holding `lastSeq - 499 … lastSeq` of one run, caught up.
    private func windowed(_ lastSeq: Int) async -> (AppModel, Sent) {
        let (model, sent) = model()
        model.refreshFleet()
        model.connection.injectForTesting(sessions(lastSeq))
        for _ in 0..<300 where !sent.sent.contains(where: {
            if case .subscribe = $0 { return true } else { return false }
        }) {
            try? await Task.sleep(for: .milliseconds(10))
        }
        inject(model, max(1, lastSeq - 499)...lastSeq)
        await settle(model)
        return (model, sent)
    }

    /// A window in a test process is never on a display, so nothing drives
    /// SwiftUI's update pass but a flush.
    private func render(_ model: AppModel) async {
        for _ in 0..<4 {
            await settle(model)
            CATransaction.flush()
            try? await Task.sleep(for: .milliseconds(100))
        }
    }

    private func collectionView(in view: UIView) -> UICollectionView? {
        if let list = view as? UICollectionView { return list }
        return view.subviews.lazy.compactMap { self.collectionView(in: $0) }.first
    }

    private func host(_ model: AppModel) throws -> Host {
        let scene = try XCTUnwrap(
            UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first)
        return Host(
            SessionDetailView(route: SessionRoute(key: a), surfaceForTesting: .constant(.timeline))
                .environment(model),
            scene: scene, size: CGSize(width: 402, height: 874))
    }

    // MARK: G11. The clock

    /// The tick stops in the background and its next beat is a second away, so
    /// every age on screen read the moment of leaving for that second. The
    /// clock is set on the change itself.
    func testTheClockIsCurrentTheMomentTheAppIsBack() async {
        let (model, _) = model()
        model.scenePhaseChanged(to: .background)
        try? await Task.sleep(for: .milliseconds(50))
        let pulledDown = Date()
        model.scenePhaseChanged(to: .inactive)
        XCTAssertGreaterThanOrEqual(model.now, pulledDown, "true on the first frame of inactive")
        try? await Task.sleep(for: .milliseconds(50))
        let back = Date()
        model.scenePhaseChanged(to: .active)
        XCTAssertGreaterThanOrEqual(model.now, back, "and of active")
    }

    /// Control Center or a pulled-down notification never left the foreground:
    /// nothing is caught up and the screen is not held.
    func testBeingInactiveOpensNoCatchUp() async {
        let (model, _) = await windowed(10)
        model.scenePhaseChanged(to: .inactive)
        XCTAssertFalse(model.isCatchingUp)
        inject(model, 11...15)
        await settle(model)
        XCTAssertEqual(model.shown(for: a)?.drawnThrough, 15, "drawn as it arrives")
        model.scenePhaseChanged(to: .active)
        XCTAssertFalse(model.isCatchingUp)
    }

    /// A release that leaves the last row as it was moves no tail, so the next
    /// live change still follows with its animation.
    func testAReleaseThatKeepsTheLastRowLeavesTheFollowAnimated() {
        let anchor = PageAnchor()
        let at = Date(timeIntervalSince1970: 0)
        let rows = (1...3).map {
            TimelineItem(id: "m\($0)", seq: UInt64($0), date: at, content: .agentMessage("m\($0)"))
        }
        var middleChanged = rows
        middleChanged[1] = TimelineItem(id: "m2", seq: 2, date: at, content: .agentMessage("edited"))
        anchor.catchUpWillLand(shown: rows, next: middleChanged, endMoves: false)
        XCTAssertFalse(anchor.takeQuietFollow(), "the tail did not move")

        let appended = rows + [TimelineItem(id: "m4", seq: 4, date: at, content: .agentMessage("m4"))]
        anchor.catchUpWillLand(shown: rows, next: appended, endMoves: true)
        XCTAssertTrue(anchor.takeQuietFollow(), "the release's own follow lands without animating")
    }

    // MARK: P6. The chat's one change

    /// A reader at the latest when a 700-event return lands is at the newest
    /// row in the same step: nothing scrolls through what arrived.
    func testAReaderAtTheLatestLandsAtTheLatestWithoutScrolling() async throws {
        let (model, _) = await windowed(600)
        let host = try host(model)
        defer { host.dismantle() }
        await render(model)
        let list = try XCTUnwrap(collectionView(in: host.view), "no timeline on screen")
        XCTAssertTrue(PageAnchor.isAtLatest(list))

        model.scenePhaseChanged(to: .background)
        model.scenePhaseChanged(to: .active)
        model.connection.injectForTesting(sessions(1_300))
        inject(model, 601...1_300)
        await settle(model)
        CATransaction.flush()
        try? await Task.sleep(for: .milliseconds(40))
        XCTAssertTrue(PageAnchor.isAtLatest(list), "at the newest row on the step that brings it")
        await render(model)
        XCTAssertTrue(PageAnchor.isAtLatest(list))
        XCTAssertEqual(model.shown(for: a)?.windowStart, 801)
    }

    /// A redial with the app open keeps the window, and what arrived is
    /// appended in one step that a reader at the latest lands on directly.
    func testARedialsAppendedRowsLandInOneStep() async throws {
        let (model, sent) = await windowed(600)
        let host = try host(model)
        defer { host.dismantle() }
        await render(model)
        let list = try XCTUnwrap(collectionView(in: host.view), "no timeline on screen")

        model.connection.injectForTesting(
            .helloAck(
                HelloAck(
                    protocolVersion: Wire.protocolVersion, protocolMinor: 23, serverTime: "",
                    capabilities: Self.capabilities, deviceToken: nil, deviceID: nil,
                    deviceName: nil)))
        model.connection.injectForTesting(sessions(640))
        for _ in 0..<300 where sent.sent.filter({
            if case .subscribe = $0 { return true } else { return false }
        }).count < 2 {
            try? await Task.sleep(for: .milliseconds(10))
        }
        let published = model.statesForTesting[a]?.publications.count ?? 0
        inject(model, 601...640)
        await settle(model)
        CATransaction.flush()
        try? await Task.sleep(for: .milliseconds(40))
        XCTAssertTrue(PageAnchor.isAtLatest(list), "appended and landed in one step")
        XCTAssertEqual(model.statesForTesting[a]?.publications.count, published + 1)
        XCTAssertEqual(model.shown(for: a)?.windowStart, 101, "no reset: the window is kept")
    }
}
