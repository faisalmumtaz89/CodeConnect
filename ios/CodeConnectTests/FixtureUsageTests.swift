import XCTest

@testable import CodeConnectKit

/// `-CC_FIXTURE_USAGE YES|limits` on a fixture's frames (`Fixtures.withUsage`): every run names its agent's Default
/// account, and with YES reports its context once; the limits and the email are `-CC_ACCOUNTS_WIRE`'s. Nothing else in
/// the frames changes, so a fixture launched without the flag is the fixture it was.
final class FixtureUsageTests: XCTestCase {

    private let now = Date(timeIntervalSince1970: 1_800_000_000)

    /// An accounts reply as a scenario passes it: Claude's Default at 23% of five hours and 61% of the week.
    private func book() throws -> AccountBook {
        let frame = """
            {"type":"accounts","accounts":[],"active":[],"defaults":[{"agent":"claude","facts":{"email":"you@example.com",
            "plan":"max","read_at":"2027-01-15T08:00:00Z","windows":[
            {"used_percent":23,"window_minutes":300,"resets_at":1800007800},
            {"used_percent":61,"window_minutes":10080,"resets_at":1800259200}]}}]}
            """
        guard case .accounts(_, let book) = try JSONDecoder().decode(ServerMessage.self, from: Data(frame.utf8)) else {
            throw XCTSkip("not an accounts frame")
        }
        return book
    }

    private func events(_ frames: [ServerMessage], of key: String) -> [Event] {
        frames.compactMap { if case .event(let event) = $0, event.sessionKey == key { event } else { nil } }
    }

    private func runs(_ frames: [ServerMessage]) -> [SessionSummary] {
        frames.flatMap { frame -> [SessionSummary] in
            guard case .sessions(let runs) = frame else { return [] }
            return runs
        }
    }

    private func bar(_ frames: [ServerMessage], _ key: String, book: AccountBook?) throws -> ReplyBar {
        let run = try XCTUnwrap(runs(frames).last { $0.sessionKey == key }, key)
        return ReplyBar.of(events: events(frames, of: key), summary: run, book: book, now: now)
    }

    func testYesGivesEveryRunItsContextAndItsDefaultAccountsLimits() throws {
        let frames = Fixtures.withUsage(Fixtures.frames(now: now, variant: .timeline), context: true, now: now)
        let bar = try bar(frames, "fx-7", book: try book())
        XCTAssertEqual(bar.context, .init(used: "390k", window: "1M"))
        XCTAssertEqual(bar.limits, [
            .init(usedPercent: 23, caption: "5-hour", resets: "resets in 2h 10m"),
            .init(usedPercent: 61, caption: "Weekly", resets: "resets in 3d"),
        ])
        XCTAssertEqual(bar.account, "you@example.com · Max")
        for run in runs(frames) {
            XCTAssertNotNil(run.account, run.sessionKey)
            let usage = try XCTUnwrap(events(frames, of: run.sessionKey).last, run.sessionKey)
            XCTAssertEqual(usage.kind, .usage, run.sessionKey)
            XCTAssertEqual(usage.seq, run.lastSeq, "\(run.sessionKey): the listing names the usage as its last")
        }
        XCTAssertNil(try self.bar(frames, "fx-7", book: nil).limits.first, "no book: no limits")
    }

    func testLimitsGivesTheAccountWithoutContext() throws {
        let frames = Fixtures.withUsage(Fixtures.frames(now: now, variant: .timeline), context: false, now: now)
        let bar = try bar(frames, "fx-7", book: try book())
        XCTAssertNil(bar.context)
        XCTAssertEqual(bar.limits.map(\.usedPercent), [23, 61])
        XCTAssertEqual(bar.account, "you@example.com · Max")
    }

    /// The post-process adds only what it says: the original frames' events stay as they were, in order, and each
    /// listing's runs differ only in `account` and, with YES, `last_seq`. So without the flag, nothing differs.
    func testNothingElseInTheFramesChanges() throws {
        for variant in [Fixtures.Variant.timeline, .deck, .hostedSettings] {
            let plain = Fixtures.frames(now: now, variant: variant)
            // The flag rewrites a run through JSON: a run must come back from that whole, every field kept.
            for run in runs(plain) {
                let back = try JSONDecoder().decode(SessionSummary.self, from: JSONEncoder().encode(run))
                XCTAssertEqual(back, run, "\(variant) \(run.sessionKey) survives the round trip")
            }
            for context in [true, false] {
                let used = Fixtures.withUsage(plain, context: context, now: now)
                XCTAssertEqual(used.count, plain.count + (context ? Set(runs(plain).map(\.sessionKey)).count : 0))
                for (before, after) in zip(plain, used) {
                    switch (before, after) {
                    case (.sessions(let a), .sessions(let b)):
                        XCTAssertEqual(try a.map(stripped), try b.map(stripped), "\(variant) listing")
                    case (.event(let a), .event(let b)):
                        XCTAssertEqual(a.seq, b.seq)
                        XCTAssertEqual(a.sessionKey, b.sessionKey)
                        XCTAssertEqual(a.kind, b.kind)
                    default:
                        XCTAssertEqual(String(describing: before), String(describing: after), "\(variant)")
                    }
                }
            }
        }
    }

    /// A run as JSON without the two fields the flag sets.
    private func stripped(_ run: SessionSummary) throws -> String {
        var object = try XCTUnwrap(try JSONDecoder().decode(JSONValue.self, from: JSONEncoder().encode(run)).objectValue)
        object["account"] = nil
        object["last_seq"] = nil
        return JSONValue.object(object).canonicalJSONString
    }
}
