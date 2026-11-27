import XCTest

@testable import CodeConnectKit

/// **The website's sample** (`-CC_FIXTURE_SITE`), as the design team's Mac pictures hold it: the computer is "Mac
/// Studio", the runs are in the site's five folders, `fx-7` is query-parser's "Parse ISO dates with offsets", and the
/// stacked fleet holds five decisions. Without the flag the fixtures are as they were.
final class FixtureSiteTests: XCTestCase {

    override func tearDown() {
        UserDefaults.standard.removeObject(forKey: "CC_FIXTURE_SITE")
        super.tearDown()
    }

    private func machineName(_ frames: [ServerMessage]) -> String? {
        frames.lazy.compactMap { frame -> String? in
            guard case .helloAck(let ack) = frame else { return nil }
            return ack.machineName
        }.first
    }

    private func runs(_ frames: [ServerMessage]) -> [SessionSummary] {
        frames.lazy.compactMap { frame -> [SessionSummary]? in
            guard case .sessions(let runs) = frame else { return nil }
            return runs
        }.first ?? []
    }

    func testTheSiteSampleNamesTheMacTheFoldersAndFiveDecisions() {
        UserDefaults.standard.set(true, forKey: "CC_FIXTURE_SITE")
        let stacked = Fixtures.frames(variant: .stacked)
        XCTAssertEqual(machineName(stacked), "Mac Studio")
        XCTAssertEqual(
            runs(stacked).map(\.projectLabel),
            ["storefront-api", "data-pipeline", "docs-site", "web-dashboard", "query-parser"])
        XCTAssertEqual(runs(stacked).reduce(0) { $0 + $1.blockedOn.count }, 5, "five decisions waiting")
        let parser = runs(Fixtures.frames(variant: .timeline)).first { $0.sessionID == "fx-7" }
        XCTAssertEqual(parser?.projectLabel, "query-parser")
        XCTAssertEqual(parser?.title, "Parse ISO dates with offsets")
    }

    func testWithoutTheFlagTheFixturesAreUnchanged() {
        let stacked = Fixtures.frames(variant: .stacked)
        XCTAssertEqual(machineName(stacked), "Jordan’s Mac Studio")
        XCTAssertEqual(runs(stacked).map(\.projectLabel), ["app-1", "app-2", "app-3", "app-4", "app-5"])
        XCTAssertEqual(runs(stacked).reduce(0) { $0 + $1.blockedOn.count }, 4)
        let parser = runs(Fixtures.frames(variant: .timeline)).first { $0.sessionID == "fx-7" }
        XCTAssertEqual(parser?.projectLabel, "app-7")
        XCTAssertNil(parser?.title)
    }
}
