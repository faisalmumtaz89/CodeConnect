import XCTest

@testable import CodeConnectKit

/// The website's conversation picture comes from the timeline fixture's `fx-7`: `-CC_FIXTURE_NOW` stamps the
/// fixture against a fixed clock, so its message times read before the picture's 9:41 status bar, and
/// `-CC_FIXTURE_PLAIN_CONVERSATION` ends `fx-7` on its last finished turn, without the follow-up a Stop cancelled.
final class FixtureShowcaseTests: XCTestCase {

    private let nine41 = "2026-11-24T09:41:00Z"

    override func tearDown() {
        UserDefaults.standard.removeObject(forKey: "CC_FIXTURE_NOW")
        UserDefaults.standard.removeObject(forKey: "CC_FIXTURE_PLAIN_CONVERSATION")
        super.tearDown()
    }

    private func conversation() -> [Event] {
        Fixtures.frames(variant: .timeline).compactMap { frame in
            guard case .event(let event) = frame, event.sessionID == "fx-7" else { return nil }
            return event
        }
    }

    func testAFixedClockStampsTheConversationBeforeIt() throws {
        UserDefaults.standard.set(nine41, forKey: "CC_FIXTURE_NOW")
        let clock = try XCTUnwrap(ISO8601DateFormatter().date(from: nine41))
        let events = conversation()
        XCTAssertFalse(events.isEmpty)
        XCTAssertEqual(events.first?.date, clock.addingTimeInterval(-300), "the first message, five minutes before")
        XCTAssertTrue(events.allSatisfy { $0.date < clock }, "every stamp before the clock")
    }

    func testThePlainConversationEndsOnItsLastTurn() {
        UserDefaults.standard.set(true, forKey: "CC_FIXTURE_PLAIN_CONVERSATION")
        XCTAssertEqual(conversation().map { Int($0.seq) }, Array(1...15))
    }

    func testWithoutTheFlagsTheConversationKeepsItsStoppedFollowUp() {
        XCTAssertEqual(conversation().map { Int($0.seq) }, Array(1...17))
    }
}
