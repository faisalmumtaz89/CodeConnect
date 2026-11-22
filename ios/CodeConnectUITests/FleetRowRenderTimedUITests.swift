import XCTest

/// One session's event redraws that session's row and no other.
///
/// The fleet's body reads every session, so any session's event runs it, and
/// it used to hand every row a fresh set of closures that SwiftUI cannot
/// compare: each event redrew all of them. Measured on a streaming fleet, about
/// 46 row bodies per event where one row had changed.
///
/// Counted through `-CC_ROW_RENDER_PROBE`, which reports each row's redraws
/// other than its own clock's, and how often it was made anew — a row
/// recreated on every event would otherwise pass as unchanged. The fixture's
/// last frames — `fx-5`'s next commands — are held back and released by the
/// test (`FixtureSignals`) while the fleet is on screen or covered.
///
/// These count body passes, so they run serially: how SwiftUI coalesces updates
/// on a loaded Mac is what the counts measure.
///
/// **Accepted miss.** The test releases an event at a moment of its own, which
/// no longer sits half a second off the rows' one-second clock ticks. An event
/// that lands in the same frame as a row's tick is drawn in the tick's pass and
/// counted as the tick's, so `testAnEventRedrawsOnlyItsOwnRow` can miss that
/// one redraw: about one frame in sixty at 60 Hz, some 1.7% of runs. It can
/// hide a redraw, never invent one, so it cannot fail a correct build.
final class FleetRowRenderTimedUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    func testAnEventRedrawsOnlyItsOwnRow() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "stacked", "-CC_BIOMETRICS", "allow",
            "-CC_ROW_RENDER_PROBE", "YES",
            "-CC_FIXTURE_LATE_EVENTS", "1",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        let signals = FixtureSignals()
        signals.arm(app)
        app.launch()

        let changed = app.descendants(matching: .any)["session-fx-5"]
        XCTAssertTrue(changed.waitForExistence(timeout: 25), "the running session's row")
        XCTAssertTrue(
            waitUntil(timeout: 10) { changed.label.contains("--pass 7") },
            "before the held-back event, fx-5 is on pass 7: \(changed.label)")
        // Past the first passes, where the tool column settles.
        RunLoop.current.run(until: Date().addingTimeInterval(3))

        let before = renders(app)
        XCTAssertFalse(changed.label.contains("--pass 8"), "the reading comes before the event")
        let others = ["fx-1", "fx-2", "fx-3", "fx-4"]
        for key in others + ["fx-5"] {
            XCTAssertNotNil(before[key], "the probe counts \(key): \(before)")
        }

        signals.send("late-1")
        XCTAssertTrue(
            waitUntil(timeout: 15) {
                changed.label.contains("--pass 8")
            },
            "the changed row draws the new command")
        RunLoop.current.run(until: Date().addingTimeInterval(2))

        let after = renders(app)
        for key in others {
            XCTAssertEqual(
                after[key], before[key],
                "\(key) did not change, so it must not redraw or be made anew: "
                    + "before \(before), after \(after)")
        }
    }

    /// Behind a chat, other sessions' events cost the fleet nothing: with a
    /// chat open, the first change after it opened may run the fleet's body
    /// once, every later change runs it zero times, and the fleet builds its
    /// screen from the model zero times throughout.
    ///
    /// Why one pass may follow the opening: the push itself runs the fleet's
    /// body while it is still live — the fleet freezes in response to the push,
    /// after that pass — and SwiftUI keeps watching what that last live pass read
    /// until the first of it changes. That one pass draws the frozen screen
    /// and reads nothing; nothing it draws watches a session, so no later
    /// change runs it again.
    ///
    /// The fixture holds back its last three frames — `fx-5`'s passes 6, 7
    /// and 8 — and the test releases them one by one while `fx-1`'s chat is
    /// open. The probe's `late` count proves each has landed before the
    /// reading after it, and the next has not; `fx-5` drawing pass 8 once back
    /// proves they reached the fleet.
    func testEventsBehindAChatRunNoFleetBody() {
        let (app, signals) = launchBehindAChat([])
        let changed = app.descendants(matching: .any)["session-fx-5"]
        XCTAssertTrue(changed.waitForExistence(timeout: 25), "the running session's row")
        XCTAssertTrue(
            waitUntil(timeout: 10) { changed.label.contains("--pass 5") },
            "before the held-back events, fx-5 is on pass 5: \(changed.label)")

        app.descendants(matching: .any)["session-fx-1"].tap()
        assertNoFleetWorkBehindTheChat(app, signals)
    }

    /// The same when the chat is opened by a link at launch — a tapped
    /// notification — rather than by a tap on its row. The link is followed as
    /// the fleet first appears, so the chat is pushed before the fleet was ever
    /// on screen to disappear from.
    func testEventsBehindAChatOpenedAtLaunchRunNoFleetBody() {
        let (app, signals) = launchBehindAChat(["-CC_DEEPLINK", "codeconnect://session/fx-1"])
        assertNoFleetWorkBehindTheChat(app, signals)
    }

    /// Behind the Deck, too, other sessions' events cost the fleet nothing.
    ///
    /// The Deck covers the whole screen, and the probe with it, so the fleet's
    /// body passes are read only between covers. The Deck is opened twice, for
    /// the same time: with nothing arriving, then over the fixture's three
    /// held-back frames, released one by one. Opening and closing the Deck cost the fleet the same
    /// passes both times; three events behind the second cover may add the one
    /// pass the opening leaves armed, and nothing per event.
    func testEventsBehindTheDeckRunNoFleetBody() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "stacked", "-CC_BIOMETRICS", "allow",
            "-CC_ROW_RENDER_PROBE", "YES",
            "-CC_FIXTURE_LATE_EVENTS", "3",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        let signals = FixtureSignals()
        signals.arm(app)
        app.launch()
        XCTAssertTrue(
            app.descendants(matching: .any)["session-fx-5"].waitForExistence(timeout: 25),
            "the fleet")
        RunLoop.current.run(until: Date().addingTimeInterval(3))

        let start = fleet(app)
        XCTAssertEqual(start.late, 0, "no event has landed yet")
        behindTheDeck(app)
        let afterQuiet = fleet(app)
        XCTAssertEqual(afterQuiet.late, 0, "nothing arrived behind the first cover")

        behindTheDeck(app, releasing: signals)
        let afterBusy = fleet(app)
        XCTAssertEqual(afterBusy.late, 3, "all three events arrived behind the second cover")

        let changed = app.descendants(matching: .any)["session-fx-5"]
        XCTAssertTrue(
            changed.label.contains("--pass 8"),
            "the fleet the Deck uncovers is current: \(changed.label)")
        XCTAssertEqual(
            afterBusy.builds - afterQuiet.builds, afterQuiet.builds - start.builds,
            "the fleet builds its screen from the model no more behind the Deck when events "
                + "arrive than when none do: \([start, afterQuiet, afterBusy])")
        XCTAssertLessThanOrEqual(
            afterBusy.bodies - afterQuiet.bodies, afterQuiet.bodies - start.bodies + 1,
            "three events behind the Deck cost the fleet's body no pass of their own: "
                + "\([start, afterQuiet, afterBusy])")
    }

    /// The same behind a Deck opened by a link at launch, which the fleet is
    /// first drawn under.
    func testEventsBehindADeckOpenedByALinkAtLaunchRunNoFleetBody() {
        assertNoFleetWorkUnderTheDeckAtLaunch(["-CC_DEEPLINK", "codeconnect://deck"])
    }

    /// And behind one opened by a tapped decision notification at launch.
    func testEventsBehindADeckOpenedByATapAtLaunchRunNoFleetBody() {
        assertNoFleetWorkUnderTheDeckAtLaunch(["-CC_TAP", "approval"])
    }

    /// Two launches into the Deck by `entry`, alike but for whether the
    /// fixture's three held-back frames are released behind the Deck or not at
    /// all. Behind it, they may cost the fleet's body the one pass the opening
    /// leaves armed, and nothing more.
    private func assertNoFleetWorkUnderTheDeckAtLaunch(_ entry: [String]) {
        let behind = launchUnderTheDeck(entry, releasingEvents: true)
        let after = launchUnderTheDeck(entry, releasingEvents: false)
        XCTAssertEqual(behind.late, 3, "all three events arrived behind the Deck")
        XCTAssertEqual(after.late, 0, "none arrived before the reading")
        XCTAssertLessThanOrEqual(
            behind.bodies, after.bodies + 1,
            "three events behind the Deck cost the fleet's body no pass of their own: "
                + "behind \(behind), after \(after)")
    }

    /// Launch into the Deck by `entry`, hold it for `deckHold` (releasing the
    /// fixture's held-back events behind it, if asked), close it, and read the
    /// probe.
    private func launchUnderTheDeck(
        _ entry: [String], releasingEvents: Bool
    ) -> (bodies: Int, builds: Int, late: Int) {
        let app = XCUIApplication()
        app.launchArguments =
            [
                "-CC_FIXTURE", "stacked", "-CC_BIOMETRICS", "allow",
                "-CC_ROW_RENDER_PROBE", "YES",
                "-CC_FIXTURE_LATE_EVENTS", "3",
                "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
            ] + entry
        let signals = FixtureSignals()
        signals.arm(app)
        app.launch()
        let opened = Date()
        XCTAssertTrue(app.navigationBars["Waiting for you"].waitForExistence(timeout: 30), "the Deck opens")
        hold(from: opened, releasing: releasingEvents ? signals : nil)
        app.buttons["Done"].firstMatch.tap()
        XCTAssertTrue(
            app.descendants(matching: .any)["cc-row-render-probe"].waitForExistence(timeout: 10),
            "the fleet is under it")
        RunLoop.current.run(until: Date().addingTimeInterval(2))
        let reading = fleet(app)
        if reading.late == 3 {
            let changed = app.descendants(matching: .any)["session-fx-5"]
            XCTAssertTrue(
                changed.label.contains("--pass 8"),
                "\(entry): the fleet the Deck uncovers is current: \(changed.label)")
        }
        app.terminate()
        return reading
    }

    /// How long a cover is held, and when in it each held-back event is
    /// released: apart, so each is its own change and the counts can tell
    /// them apart.
    private let deckHold: TimeInterval = 8
    private let releases: [TimeInterval] = [2, 4, 6]

    /// Hold a cover `deckHold` from `opened`, releasing `late-1`…`late-3` on
    /// `releases` when `signals` is given.
    private func hold(from opened: Date, releasing signals: FixtureSignals?) {
        if let signals {
            for (index, at) in releases.enumerated() {
                RunLoop.current.run(until: opened.addingTimeInterval(at))
                signals.send("late-\(index + 1)")
            }
        }
        RunLoop.current.run(until: opened.addingTimeInterval(deckHold))
    }

    /// Open the Deck, hold it `deckHold`, close it.
    private func behindTheDeck(_ app: XCUIApplication, releasing signals: FixtureSignals? = nil) {
        let opened = Date()
        XCUIDevice.shared.system.open(URL(string: "codeconnect://deck")!)
        XCTAssertTrue(app.navigationBars["Waiting for you"].waitForExistence(timeout: 15), "the Deck opens")
        hold(from: opened, releasing: signals)
        app.buttons["Done"].firstMatch.tap()
        XCTAssertTrue(
            app.descendants(matching: .any)["cc-row-render-probe"].waitForExistence(timeout: 10),
            "the fleet is back")
        RunLoop.current.run(until: Date().addingTimeInterval(2))
    }

    private func launchBehindAChat(_ extra: [String]) -> (XCUIApplication, FixtureSignals) {
        let app = XCUIApplication()
        app.launchArguments =
            [
                "-CC_FIXTURE", "stacked", "-CC_BIOMETRICS", "allow",
                "-CC_ROW_RENDER_PROBE", "YES",
                "-CC_FIXTURE_LATE_EVENTS", "3",
                "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
            ] + extra
        let signals = FixtureSignals()
        signals.arm(app)
        app.launch()
        return (app, signals)
    }

    /// With `fx-1`'s chat opening, read the probe before and between the three
    /// held-back events, go back, and hold the fleet to the rule above.
    private func assertNoFleetWorkBehindTheChat(_ app: XCUIApplication, _ signals: FixtureSignals) {
        XCTAssertTrue(
            app.descendants(matching: .any)["session-timeline"].waitForExistence(timeout: 20),
            "fx-1's chat opens")
        // Past the push, which the fleet draws for.
        RunLoop.current.run(until: Date().addingTimeInterval(3))

        var readings = [fleet(app)]
        XCTAssertEqual(readings[0].late, 0, "the first reading comes before any event")
        for landed in 1...3 {
            signals.send("late-\(landed)")
            XCTAssertTrue(
                waitUntil(timeout: 15) { fleet(app).late >= landed },
                "event \(landed) lands")
            // Long enough for a pass it caused; the next event waits for the test.
            RunLoop.current.run(until: Date().addingTimeInterval(2))
            readings.append(fleet(app))
            XCTAssertEqual(
                readings[landed].late, landed,
                "the reading falls between event \(landed) and the next: \(readings)")
        }

        app.navigationBars.buttons.firstMatch.tap()
        let changed = app.descendants(matching: .any)["session-fx-5"]
        XCTAssertTrue(
            waitUntil(timeout: 10) { changed.label.contains("--pass 8") },
            "the events reached the fleet: \(changed.label)")
        XCTAssertLessThanOrEqual(
            readings[1].bodies - readings[0].bodies, 1,
            "the first change after opening runs the fleet's body at most once: \(readings)")
        XCTAssertEqual(
            readings[3].bodies, readings[1].bodies,
            "every later change runs the fleet's body zero times: \(readings)")
        XCTAssertEqual(
            readings.map(\.builds), Array(repeating: readings[0].builds, count: 4),
            "the fleet builds its screen from the model zero times while the chat is open: "
                + "\(readings)")
    }

    /// The fleet's body passes and screen builds, and the held-back frames
    /// delivered, from one reading of the probe.
    private func fleet(_ app: XCUIApplication) -> (bodies: Int, builds: Int, late: Int) {
        let counts = renders(app)
        let pair = (counts["fleet"] ?? "").split(separator: "+").compactMap { Int($0) }
        XCTAssertEqual(pair.count, 2, "the probe counts the fleet: \(counts)")
        return (pair.first ?? -1, pair.last ?? -1, Int(counts["late"] ?? "") ?? -1)
    }

    /// `fleet=12+12 late=1 fx-1=0+1 …` from the probe, by key: the fleet's body
    /// passes and screen builds, the held-back frames delivered, then each row's
    /// redraws and mounts.
    private func renders(_ app: XCUIApplication) -> [String: String] {
        let probe = app.descendants(matching: .any)["cc-row-render-probe"]
        XCTAssertTrue(probe.waitForExistence(timeout: 5), "the render probe is on screen")
        var counts: [String: String] = [:]
        for pair in probe.label.split(separator: " ") {
            let parts = pair.split(separator: "=")
            if parts.count == 2 { counts[String(parts[0])] = String(parts[1]) }
        }
        return counts
    }

    private func waitUntil(timeout: TimeInterval, _ condition: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if condition() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.25))
        }
        return condition()
    }
}
