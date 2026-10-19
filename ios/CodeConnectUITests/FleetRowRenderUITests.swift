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
/// last frame — `fx-5`'s next command — arrives late, while the fleet is on
/// screen.
final class FleetRowRenderUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    /// Half a second off the whole seconds the fixture's clocks tick on, so the
    /// event is not expected in the same pass as a row's own tick. A pass that
    /// did hold both would count as the clock's and could hide a redraw; it
    /// cannot invent one.
    private let lateEventMS = 10_500

    func testAnEventRedrawsOnlyItsOwnRow() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "stacked", "-CC_BIOMETRICS", "allow",
            "-CC_ROW_RENDER_PROBE", "YES",
            "-CC_FIXTURE_LATE_EVENT_MS", "\(lateEventMS)",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
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

        XCTAssertTrue(
            waitUntil(timeout: Double(lateEventMS) / 1000 + 15) {
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
    /// and 8 — and delivers them five seconds apart while `fx-1`'s chat is
    /// open. The probe's `late` count proves each has landed before the
    /// reading after it, and the next has not; `fx-5` drawing pass 8 once back
    /// proves they reached the fleet.
    func testEventsBehindAChatRunNoFleetBody() {
        let app = launchBehindAChat([])
        let changed = app.descendants(matching: .any)["session-fx-5"]
        XCTAssertTrue(changed.waitForExistence(timeout: 25), "the running session's row")
        XCTAssertTrue(
            waitUntil(timeout: 10) { changed.label.contains("--pass 5") },
            "before the held-back events, fx-5 is on pass 5: \(changed.label)")

        app.descendants(matching: .any)["session-fx-1"].tap()
        assertNoFleetWorkBehindTheChat(app)
    }

    /// The same when the chat is opened by a link at launch — a tapped
    /// notification — rather than by a tap on its row. The link is followed as
    /// the fleet first appears, so the chat is pushed before the fleet was ever
    /// on screen to disappear from.
    func testEventsBehindAChatOpenedAtLaunchRunNoFleetBody() {
        let app = launchBehindAChat(["-CC_DEEPLINK", "codeconnect://session/fx-1"])
        assertNoFleetWorkBehindTheChat(app)
    }

    /// Behind the Deck, too, other sessions' events cost the fleet nothing.
    ///
    /// The Deck covers the whole screen, and the probe with it, so the fleet's
    /// body passes are read only between covers. The Deck is opened twice: for
    /// 8 s with nothing arriving, then for 52 s over the fixture's three
    /// held-back frames. Opening and closing the Deck cost the fleet the same
    /// passes both times; three events behind the second cover may add the one
    /// pass the opening leaves armed, and nothing per event.
    func testEventsBehindTheDeckRunNoFleetBody() {
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_FIXTURE", "stacked", "-CC_BIOMETRICS", "allow",
            "-CC_ROW_RENDER_PROBE", "YES",
            "-CC_FIXTURE_LATE_EVENT_MS", "40500,45500,50500",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
        ]
        app.launch()
        XCTAssertTrue(
            app.descendants(matching: .any)["session-fx-5"].waitForExistence(timeout: 25),
            "the fleet")
        RunLoop.current.run(until: Date().addingTimeInterval(3))

        let start = fleet(app)
        XCTAssertEqual(start.late, 0, "no event has landed yet")
        let quiet = behindTheDeck(app, for: 8)
        let afterQuiet = fleet(app)
        XCTAssertEqual(afterQuiet.late, 0, "nothing arrived behind the first cover")

        let busy = behindTheDeck(app, for: 52)
        let afterBusy = fleet(app)
        XCTAssertEqual(afterBusy.late, 3, "all three events arrived behind the second cover")
        XCTAssertGreaterThanOrEqual(busy, quiet, "the second cover was held as long as the first")

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

    /// Two launches into the Deck by `entry`, alike but for when the fixture's
    /// three held-back frames arrive: behind the Deck, or after it has closed.
    /// Behind it, they may cost the fleet's body the one pass the opening
    /// leaves armed, and nothing more.
    private func assertNoFleetWorkUnderTheDeckAtLaunch(_ entry: [String]) {
        let behind = launchUnderTheDeck(entry, lateEventMS: "20500,25500,30500")
        let after = launchUnderTheDeck(entry, lateEventMS: "60500,65500,70500")
        XCTAssertEqual(behind.late, 3, "all three events arrived behind the Deck")
        XCTAssertEqual(after.late, 0, "none arrived before the reading")
        XCTAssertLessThanOrEqual(
            behind.bodies, after.bodies + 1,
            "three events behind the Deck cost the fleet's body no pass of their own: "
                + "behind \(behind), after \(after)")
    }

    /// Launch into the Deck by `entry`, hold it past the fixture's first
    /// held-back events, close it, and read the probe.
    private func launchUnderTheDeck(
        _ entry: [String], lateEventMS: String
    ) -> (bodies: Int, builds: Int, late: Int) {
        let app = XCUIApplication()
        app.launchArguments =
            [
                "-CC_FIXTURE", "stacked", "-CC_BIOMETRICS", "allow",
                "-CC_ROW_RENDER_PROBE", "YES",
                "-CC_FIXTURE_LATE_EVENT_MS", lateEventMS,
                "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
            ] + entry
        app.launch()
        XCTAssertTrue(app.navigationBars["Needs you"].waitForExistence(timeout: 30), "the Deck opens")
        // Past 30.5 s after the fixture loaded, which was before the Deck
        // opened. The later launch must be read before its first event at
        // 60.5 s; its `late` count says if a slow run was not.
        RunLoop.current.run(until: Date().addingTimeInterval(34))
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

    /// Open the Deck, hold it `seconds`, close it; how long it was held.
    private func behindTheDeck(_ app: XCUIApplication, for seconds: TimeInterval) -> TimeInterval {
        let opened = Date()
        XCUIDevice.shared.system.open(URL(string: "codeconnect://deck")!)
        XCTAssertTrue(app.navigationBars["Needs you"].waitForExistence(timeout: 15), "the Deck opens")
        RunLoop.current.run(until: opened.addingTimeInterval(seconds))
        app.buttons["Done"].firstMatch.tap()
        XCTAssertTrue(
            app.descendants(matching: .any)["cc-row-render-probe"].waitForExistence(timeout: 10),
            "the fleet is back")
        RunLoop.current.run(until: Date().addingTimeInterval(2))
        return Date().timeIntervalSince(opened)
    }

    private func launchBehindAChat(_ extra: [String]) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments =
            [
                "-CC_FIXTURE", "stacked", "-CC_BIOMETRICS", "allow",
                "-CC_ROW_RENDER_PROBE", "YES",
                "-CC_FIXTURE_LATE_EVENT_MS", "20500,25500,30500",
                "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryL",
            ] + extra
        app.launch()
        return app
    }

    /// With `fx-1`'s chat opening, read the probe before and between the three
    /// held-back events, go back, and hold the fleet to the rule above.
    private func assertNoFleetWorkBehindTheChat(_ app: XCUIApplication) {
        XCTAssertTrue(
            app.descendants(matching: .any)["session-timeline"].waitForExistence(timeout: 20),
            "fx-1's chat opens")
        // Past the push, which the fleet draws for.
        RunLoop.current.run(until: Date().addingTimeInterval(3))

        var readings = [fleet(app)]
        XCTAssertEqual(readings[0].late, 0, "the first reading comes before any event")
        for landed in 1...3 {
            // The first event is due 20.5 s after the fixture loads, however
            // soon the chat was open; the later ones five seconds apart.
            XCTAssertTrue(
                waitUntil(timeout: landed == 1 ? 30 : 15) { fleet(app).late >= landed },
                "event \(landed) lands")
            // Long enough for a pass it caused; short of the next event.
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
