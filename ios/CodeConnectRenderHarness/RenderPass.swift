import XCTest

// =============================================================================
//  CodeConnectRenderHarness — an inspection instrument, not an assertion test.
//
//  It exists because reading a screen's code does not tell you what that screen
//  looks like at AX5. Photographing every catalogued state has caught safety,
//  honesty, scrolling, wrapping and accessibility failures that reading the
//  source did not.
//
//  It lives here rather than in `CodeConnectUITests` so that the product scheme's
//  own test counts are exactly what they were. Nothing in this target runs under
//  `-scheme CodeConnect`; it runs under `-scheme "CodeConnect Renders"`, driven
//  by `ios/scripts/render-screens.sh`.
//
//  **It makes no pixel assertions.** The only thing it fails on is not being
//  able to reach or photograph a catalogued state — because a state nobody can
//  reach is a state nobody has looked at, and seven Deck states went a long time
//  being exactly that.
// =============================================================================

final class RenderPass: XCTestCase {

    /// **Both sizes, and never `medium`.**
    ///
    /// `L` is iOS's default — `UICTContentSizeCategoryL`. `medium` is one step
    /// below it and renders about 7% small, and the error does not stop at
    /// fonts: `@ScaledMetric` sizes badges, dots, chips, skeleton bars and glyph
    /// containers, so five independent tokens read 1–4pt short there and are
    /// exact at `L`. **Eight earlier measurements had to be thrown out because of
    /// it**, and one real 41.66pt defect hid behind it, because `medium` is not
    /// where the two ramps cross.
    enum Size: String {
        case large = "UICTContentSizeCategoryL"
        case xxLarge = "UICTContentSizeCategoryXXL"
        case xxxLarge = "UICTContentSizeCategoryXXXL"
        case ax1 = "UICTContentSizeCategoryAccessibilityM"
        case ax5 = "UICTContentSizeCategoryAccessibilityXXXL"

        /// The file suffix, and what `render-screens.sh` names the directory it
        /// sorts these into.
        var suffix: String {
            switch self {
            case .large: return "L"
            case .xxLarge: return "xxl"
            case .xxxLarge: return "xxxl"
            case .ax1: return "ax1"
            case .ax5: return "ax5"
            }
        }
    }

    override func setUp() {
        // Every scenario is independent and relaunches the app, so one that
        // cannot be reached must not hide the twenty after it. The run still
        // exits nonzero — see `render(at:)`.
        continueAfterFailure = true
    }

    func testRendersEveryScenarioAtLarge() throws {
        try render(at: .large)
    }

    func testRendersEveryScenarioAtAX5() throws {
        try render(at: .ax5)
    }

    /// The single-line label sizes (xxLarge, xxxLarge) and the two-line boundary just above (AX1), for
    /// the narrow-device sweep that pins "the mode's whole text is on screen" around the single-line edge.
    func testRendersEveryScenarioAtXXLarge() throws { try render(at: .xxLarge) }

    func testRendersEveryScenarioAtXXXLarge() throws { try render(at: .xxxLarge) }

    func testRendersEveryScenarioAtAX1() throws { try render(at: .ax1) }

    func testRendersStartSessionAtLarge() throws {
        try render(at: .large, RenderCatalog.startSession)
    }

    func testRendersStartSessionAtAX5() throws {
        try render(at: .ax5, RenderCatalog.startSession)
    }

    /// The "Not sent: stopped" row, on its own, for a quick look at a Stop-cancelled send.
    func testRendersNotSentStoppedAtLarge() throws {
        try render(at: .large, RenderCatalog.all.filter { $0.name == "session-not-sent-stopped" })
    }

    /// The hosted run's unified model / effort / mode sheet (B8, minor 39), with minor-39's menus in
    /// their worst shapes — the one screen worth a look at the smallest phone and the largest type.
    func testRendersHostedSettingsSheetAtLarge() throws {
        try render(at: .large, RenderCatalog.all.filter { $0.name.hasPrefix("session-hosted-settings") })
    }

    func testRendersHostedSettingsSheetAtAX5() throws {
        try render(at: .ax5, RenderCatalog.all.filter { $0.name.hasPrefix("session-hosted-settings") })
    }

    /// The settings line over the reply box: under a long reply, read-only on an ended run, and beside a live
    /// Codex turn's Stop.
    private static let settingsLine = RenderCatalog.all.filter {
        ["session-settings-over-a-long-reply", "session-ended-relaunchable", "codex-stop-offered-session"]
            .contains($0.name)
    }

    func testRendersTheSettingsLineAtLarge() throws { try render(at: .large, Self.settingsLine) }

    func testRendersTheSettingsLineAtAX5() throws { try render(at: .ax5, Self.settingsLine) }

    /// A run moved to another account, from the daemon's recordings.
    private static let switchedRun = RenderCatalog.all.filter { $0.name.hasPrefix("switch-") }

    func testRendersTheSwitchedRunAtLarge() throws { try render(at: .large, Self.switchedRun) }

    private static let usage = RenderCatalog.all.filter { $0.name.hasPrefix("session-usage-") }

    func testRendersTheUsageAtLarge() throws { try render(at: .large, Self.usage) }

    func testRendersTheUsageAtAX5() throws { try render(at: .ax5, Self.usage) }

    // MARK: - The pass

    private func render(at size: Size, _ scenarios: [RenderScenario] = RenderCatalog.all) throws {
        var failures: [String] = []

        for scenario in scenarios {
            let app = XCUIApplication()
            app.launchArguments =
                scenario.arguments + [
                    // The calibration mark. See `CCRenderProbe`.
                    "-CC_RENDER_PROBE", "YES"
                ]
            app.launchEnvironment = scenario.environment
            scenario.signals?.arm(app)

            // **`activate()`, never `launch()`.** `xcodebuild test` plus
            // `XCUIApplication.launch()` resets the simulator's content-size
            // category to the default, and `TEST_RUNNER_*` never reaches the
            // runner — which is how an "AX5 pass" ends up being an `L` pass that
            // nobody noticed. The category is set once per invocation by
            // `render-screens.sh` with `xcrun simctl ui <device> content_size`,
            // and `activate()` attaches without disturbing it.
            app.terminate()
            app.activate()

            let driver = RenderDriver(test: self, timeout: scenario.timeout)
            do {
                try verifyTypeSize(app, expected: size)
                try scenario.reach(app, driver)
                capture(app, named: "\(scenario.name)--\(size.suffix)")
            } catch {
                // Photograph the failure too. What the app was showing when it
                // could not be driven any further is usually the whole answer.
                capture(app, named: "\(scenario.name)--\(size.suffix)--FAILED")
                let message = "\(scenario.name) [\(size.suffix)]: \(error)"
                failures.append(message)
                XCTFail(message)
            }
            app.terminate()
        }

        // Restated as one line at the end, because a run with three failures
        // scattered through forty scenarios is a log nobody reads to the bottom.
        if !failures.isEmpty {
            XCTFail(
                "\(failures.count) of \(scenarios.count) scenarios could not be "
                    + "rendered at \(size.suffix):\n  " + failures.joined(separator: "\n  "))
        }
    }

    /// **Proves the pass is at the size it claims**, before it photographs
    /// anything.
    ///
    /// This is the instrument's calibration mark, and it exists because the
    /// alternative has already cost this project real work: a harness whose
    /// content-size category was silently reset produced "AX5 measurements" that
    /// were `L` measurements, and they were believed. A render that cannot state
    /// its own conditions is not evidence.
    private func verifyTypeSize(_ app: XCUIApplication, expected: Size) throws {
        let probe = app.descendants(matching: .any)["cc-render-probe"]
        guard probe.waitForExistence(timeout: 30) else {
            throw RenderFailure.unreachable(
                "the render probe (is `-CC_RENDER_PROBE` reaching the app?)")
        }
        guard probe.label == expected.rawValue else {
            throw RenderFailure.wrongTypeSize(expected: expected.rawValue, actual: probe.label)
        }
    }

    /// The screenshot, named. `render-screens.sh` exports these out of the
    /// result bundle and writes them as `<name>.png`.
    private func capture(_ app: XCUIApplication, named name: String) {
        // **Let the frame settle first.** A scenario's last assertion returns
        // the instant its text exists, which is the *start* of any transition
        // that put it there — so a caption replacing another caption in the
        // same slot photographs as both at once, and the render is evidence of
        // nothing. One wait, just past `CC.duration.draw` (0.30s), the longest
        // class the product animates.
        Thread.sleep(forTimeInterval: 0.35)
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
