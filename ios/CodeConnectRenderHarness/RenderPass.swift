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
        case ax5 = "UICTContentSizeCategoryAccessibilityXXXL"

        /// The file suffix, and what `render-screens.sh` names the directory it
        /// sorts these into.
        var suffix: String { self == .large ? "L" : "ax5" }
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

    // MARK: - The pass

    private func render(at size: Size) throws {
        var failures: [String] = []

        for scenario in RenderCatalog.all {
            let app = XCUIApplication()
            app.launchArguments =
                scenario.arguments + [
                    // The calibration mark. See `CCRenderProbe`.
                    "-CC_RENDER_PROBE", "YES"
                ]
            app.launchEnvironment = scenario.environment

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
                "\(failures.count) of \(RenderCatalog.all.count) scenarios could not be "
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
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
