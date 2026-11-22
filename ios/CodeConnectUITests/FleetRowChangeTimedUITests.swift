import XCTest

/// `FleetRowChangeUITests`'s tests that rest on time: a fixture clock set at launch, an elapsed-time budget or a long press.
/// They run on this Mac before every release and are left off the CI runner, which is slower than they allow.
final class FleetRowChangeTimedUITests: FleetRowChangeUITestsBase {
    /// The cards, `fx-4`'s finished turn and `fx-5`'s running one arrive after
    /// the rows have drawn: the blocked rows gain their card and their clock
    /// moves to it, and two rows change band. Before that, a blocked row's own
    /// clock ticks.
    func testWhatArrivesLaterReachesItsRows() {
        let signals = FixtureSignals()
        let app = launch(["-CC_FIXTURE", "stacked", "-CC_FIXTURE_LATE_APPROVALS", "YES"], signals: signals)
        let high = row(app, "fx-1")
        XCTAssertTrue(high.waitForExistence(timeout: 25), "the fleet")
        XCTAssertFalse(high.label.contains("git push"), "no card yet: \(high.label)")
        XCTAssertFalse(row(app, "fx-4").label.contains("Done"), row(app, "fx-4").label)
        XCTAssertFalse(row(app, "fx-5").label.contains("Running"), row(app, "fx-5").label)

        let ticked = high.label
        XCTAssertTrue(
            waitUntil(timeout: 5) { high.label != ticked && !high.label.contains("git push") },
            "the wait clock ticks on its own row: \(ticked)")

        signals.send("late-approvals")
        XCTAssertTrue(
            waitUntil(timeout: 25) { high.label.contains("git push --force origin main") },
            "the card reaches its row: \(high.label)")
        XCTAssertTrue(high.label.contains("risk HIGH"), high.label)
        XCTAssertTrue(
            high.label.contains("waiting 1 minute"), "the clock counts from the card: \(high.label)")
        XCTAssertTrue(
            waitUntil(timeout: 5) { self.row(app, "fx-4").label.contains("Done") },
            "fx-4's finished turn moves it to Done: \(row(app, "fx-4").label)")
        XCTAssertTrue(
            waitUntil(timeout: 5) {
                let label = self.row(app, "fx-5").label
                return label.contains("Running") && label.contains("--pass 8")
            },
            "fx-5's turn moves it to Running, on its last command: \(row(app, "fx-5").label)")
    }

    /// The link drops and returns: the run's capability is unknown while it is
    /// away and known again after, on the row.
    func testCapabilityFollowsTheLinkOnItsRow() {
        let signals = FixtureSignals()
        let app = launch(["-CC_FIXTURE", "ended", "-CC_FIXTURE_LINK_DROP", "YES"], signals: signals)
        let live = row(app, "fx-1")
        XCTAssertTrue(live.waitForExistence(timeout: 25))
        XCTAssertTrue(
            waitUntil(timeout: 5) { live.label.hasSuffix("control") },
            "a known capability is spoken: \(live.label)")
        signals.send("link-drop")
        XCTAssertTrue(
            waitUntil(timeout: 15) { !live.label.hasSuffix("control") },
            "while the link is down nothing is claimed: \(live.label)")
        signals.send("link-return")
        XCTAssertTrue(
            waitUntil(timeout: 15) { live.label.hasSuffix("control") },
            "and it is claimed again when the link returns: \(live.label)")
    }
}
