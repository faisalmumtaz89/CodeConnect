import XCTest

/// The fleet's offline banner, proven calm on the running app.
///
/// The defect this guards was filmed on a device with Tailscale off: the
/// paired MagicDNS name stops resolving, every retry blanked the banner and a
/// countdown re-rendered each second — a flicker at exactly the moment the
/// user needs one steady instruction.
///
/// The pairing seam points the app at a guaranteed-unresolvable tailnet name,
/// so this exercises the real dial path, the real DNS failure and the real
/// banner. It is the *integration* proof: XCUITest cannot sample fast enough
/// to certify no sub-100ms frame ever blanked, so the frame-level rules —
/// redial stickiness, no countdown in the detail — are pinned deterministically
/// in `LinkOfflineTests`, and this file proves the assembled behavior a human
/// would see.
final class FleetOfflineUITests: XCTestCase {

    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    private func launchUnresolvable() -> XCUIApplication {
        // The paired boot path requests notification permission; the system
        // alert must be answered or it sits over every query.
        addUIInterruptionMonitor(withDescription: "notification permission") { alert in
            let allow = alert.buttons["Allow"]
            guard allow.exists else { return false }
            allow.tap()
            return true
        }
        let app = XCUIApplication()
        app.launchArguments = [
            "-CC_HOST", "bogus-mac.tail9999.ts.net",
            "-CC_TOKEN", "junktoken",
            "-CC_RESET_CACHE", "YES",
            "-CC_BIOMETRICS", "allow",
        ]
        app.launch()
        XCTAssertTrue(app.wait(for: .runningForeground, timeout: 30))
        // Interruption monitors only fire on interaction; give them one.
        app.tap()
        return app
    }

    func testTheOfflineBannerHoldsSteadyInsteadOfFlickering() {
        let app = launchUnresolvable()

        // The banner must arrive once the first dial has failed.
        let banner = app.staticTexts["CONNECT TAILSCALE"]
        XCTAssertTrue(
            banner.waitForExistence(timeout: 15),
            "an unresolvable tailnet host must produce the Tailscale banner")

        // And then it must *stay*. Sampled at 4Hz across a window that spans
        // the fast attempts and the first calm-cadence redial: the pre-fix
        // behavior blanked the banner on every re-dial and ticked its text
        // every second, which this catches at the granularity a human eye
        // does. (The per-frame rule is `LinkOfflineTests`' to enforce.)
        var absences = 0
        for _ in 0..<56 {
            RunLoop.current.run(until: Date().addingTimeInterval(0.25))
            if !banner.exists { absences += 1 }
        }
        XCTAssertEqual(
            absences, 0,
            "the banner blanked \(absences)/56 samples — the retry loop is "
                + "still flashing the screen")
    }

    func testTheBannerTellsTheUserWhatToDoWithoutACountdown() {
        let app = launchUnresolvable()
        XCTAssertTrue(
            app.staticTexts["CONNECT TAILSCALE"].waitForExistence(timeout: 15))

        // The positive claim: the banner carries the static instruction and
        // the way home. The negatives — no countdown, no scheme chatter —
        // are swept here for the assembled screen, and pinned per-string in
        // `LinkOfflineTests`, because the tailnet banner never renders the
        // `detail` those regressions would land in.
        let instruction = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS 'reconnects on its own'")
        ).firstMatch
        XCTAssertTrue(
            instruction.exists,
            "the banner must tell the user the app recovers by itself")
        XCTAssertFalse(
            app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'retrying in'"))
                .firstMatch.exists,
            "a ticking countdown is churn, not information")
        XCTAssertFalse(
            app.staticTexts.matching(NSPredicate(format: "label CONTAINS 'trying ws'"))
                .firstMatch.exists,
            "scheme alternation cannot fix DNS and must not narrate")
    }
}
