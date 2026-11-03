import XCTest

/// The five ways of starting a session, each from the fleet to the new
/// session starting and to one failure, on the `deck` fleet with forty
/// projects and Codex not installed. `startN-…` is option N of `StartOption`.
extension RenderCatalog {
    static let startOptions: [RenderScenario] =
        recentsSheet + steps + cardAction + messageFirst + searchFirst

    /// Each option's densest screen, its project list, at AX5.
    static let startOptionsAX5: [RenderScenario] = [
        recentsSheet[1], steps[1], cardAction[3], messageFirst[2], searchFirst[1],
    ]

    private static let message = "Add a --strict flag that rejects trailing commas, with tests"

    private static func args(_ option: Int, failing outcome: String? = nil) -> [String] {
        ["-CC_FIXTURE", "deck", "-CC_FIXTURE_REAL_NAMES", "YES", "-CC_START_OPTION", "\(option)"]
            + (outcome.map { ["-CC_START_OUTCOME", $0] } ?? [])
    }

    private static func tap(_ app: XCUIApplication, _ id: String, _ driver: RenderDriver) throws {
        let element = app.descendants(matching: .any)[id].firstMatch
        try driver.requireExists(element, id).tap()
    }

    private static func type(_ app: XCUIApplication, _ id: String, _ text: String, _ driver: RenderDriver)
        throws
    {
        let field = app.descendants(matching: .any)[id].firstMatch
        try driver.requireExists(field, id).tap()
        field.typeText(text)
    }

    private static func starting(_ app: XCUIApplication, _ driver: RenderDriver) throws {
        try driver.requireExists(driver.text(containing: "Starting", in: app), "the starting screen")
    }

    // MARK: 1 · Recents sheet

    private static let recentsSheet: [RenderScenario] = [
        RenderScenario(name: "start1-a-fleet", purpose: "option 1: the fleet's New session", arguments: args(1)) {
            app, driver in
            try driver.fleet(app)
            try driver.requireExists(app.buttons["start-entry"], "New session")
        },
        RenderScenario(name: "start1-b-sheet", purpose: "option 1: the sheet, last project chosen", arguments: args(1)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try driver.requireExists(app.buttons["start-go"], "Start")
        },
        RenderScenario(name: "start1-c-message", purpose: "option 1: a first message typed", arguments: args(1)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-message", message, driver)
        },
        RenderScenario(name: "start1-d-starting", purpose: "option 1: starting", arguments: args(1)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-message", message, driver)
            try tap(app, "start-go", driver)
            try starting(app, driver)
        },
        RenderScenario(
            name: "start1-e-failed", purpose: "option 1: the folder is gone",
            arguments: args(1, failing: "folder_missing")
        ) { app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-search", "billing", driver)
            try tap(app, "start-project-old-billing-service", driver)
            try tap(app, "start-go", driver)
            try driver.requireExists(driver.text(containing: "isn’t there", in: app), "the failure")
        },
    ]

    // MARK: 2 · Three steps

    private static let steps: [RenderScenario] = [
        RenderScenario(name: "start2-a-fleet", purpose: "option 2: the fleet's +", arguments: args(2)) {
            app, driver in
            try driver.fleet(app)
            try driver.requireExists(app.buttons["start-entry"], "the + button")
        },
        RenderScenario(name: "start2-b-where", purpose: "option 2: step 1, where", arguments: args(2)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try driver.requireExists(app.buttons["start-project-query-parser"], "the project list")
        },
        RenderScenario(name: "start2-c-agent", purpose: "option 2: step 2, Codex last used and missing", arguments: args(2)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-project-ios-app", driver)
            try driver.requireExists(app.buttons["start-continue"], "Continue")
        },
        RenderScenario(name: "start2-d-message", purpose: "option 2: step 3, the first message", arguments: args(2)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-project-query-parser", driver)
            try tap(app, "start-continue", driver)
            try type(app, "start-message", message, driver)
        },
        RenderScenario(name: "start2-e-starting", purpose: "option 2: starting", arguments: args(2)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-project-query-parser", driver)
            try tap(app, "start-continue", driver)
            try tap(app, "start-go", driver)
            try starting(app, driver)
        },
        RenderScenario(
            name: "start2-f-failed", purpose: "option 2: the login shell failed",
            arguments: args(2, failing: "launch_failed")
        ) { app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-project-query-parser", driver)
            try tap(app, "start-continue", driver)
            try tap(app, "start-go", driver)
            try driver.requireExists(driver.text(containing: "didn’t start", in: app), "the failure")
        },
    ]

    // MARK: 3 · From the card

    private static func firstCardAction(_ app: XCUIApplication, _ driver: RenderDriver) throws {
        let action = app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'start-card-'"))
            .firstMatch
        try driver.requireExists(action, "a card's New session").tap()
    }

    private static let cardAction: [RenderScenario] = [
        RenderScenario(name: "start3-a-fleet", purpose: "option 3: New session on each card", arguments: args(3)) {
            app, driver in
            try driver.fleet(app)
            try driver.requireExists(
                app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'start-card-'")).firstMatch,
                "a card's New session")
        },
        RenderScenario(name: "start3-b-recent", purpose: "option 3: recent projects under the cards", arguments: args(3)) {
            app, driver in
            try driver.fleet(app)
            try driver.require(app.buttons["start-all"], "All projects")
        },
        RenderScenario(name: "start3-c-sheet", purpose: "option 3: the project's start sheet", arguments: args(3)) {
            app, driver in
            try firstCardAction(app, driver)
            try driver.requireExists(app.buttons["start-go"], "Start")
        },
        RenderScenario(name: "start3-d-all", purpose: "option 3: all projects", arguments: args(3)) {
            app, driver in
            try driver.fleet(app)
            let all = app.buttons["start-all"]
            try driver.scrollUntil(app, "All projects", attempts: 40) { all.exists && all.isHittable }
            all.tap()
            try driver.requireExists(app.buttons["start-project-query-parser"], "the project list")
        },
        RenderScenario(name: "start3-e-starting", purpose: "option 3: starting", arguments: args(3)) {
            app, driver in
            try firstCardAction(app, driver)
            try type(app, "start-message", message, driver)
            try tap(app, "start-go", driver)
            try starting(app, driver)
        },
        RenderScenario(
            name: "start3-f-failed", purpose: "option 3: the folder is gone",
            arguments: args(3, failing: "folder_missing")
        ) { app, driver in
            try driver.fleet(app)
            try driver.require(app.buttons["start-all"], "All projects").tap()
            try type(app, "start-search", "billing", driver)
            try tap(app, "start-project-old-billing-service", driver)
            try tap(app, "start-go", driver)
            try driver.requireExists(driver.text(containing: "isn’t there", in: app), "the failure")
        },
    ]

    // MARK: 4 · Message first

    private static let messageFirst: [RenderScenario] = [
        RenderScenario(name: "start4-a-fleet", purpose: "option 4: the composer on the fleet", arguments: args(4)) {
            app, driver in
            try driver.fleet(app)
            try driver.requireExists(app.descendants(matching: .any)["start-message"].firstMatch, "the composer")
        },
        RenderScenario(name: "start4-b-typed", purpose: "option 4: a message typed", arguments: args(4)) {
            app, driver in
            try type(app, "start-message", message, driver)
        },
        RenderScenario(name: "start4-c-pick", purpose: "option 4: choosing the project", arguments: args(4)) {
            app, driver in
            try tap(app, "start-chip-project", driver)
            try driver.requireExists(app.buttons["start-project-query-parser"], "the project list")
        },
        RenderScenario(name: "start4-d-starting", purpose: "option 4: starting", arguments: args(4)) {
            app, driver in
            try type(app, "start-message", message, driver)
            try tap(app, "start-message-button", driver)
            try starting(app, driver)
        },
        RenderScenario(
            name: "start4-e-failed", purpose: "option 4: the login shell failed",
            arguments: args(4, failing: "launch_failed")
        ) { app, driver in
            try type(app, "start-message", message, driver)
            try tap(app, "start-message-button", driver)
            try driver.requireExists(driver.text(containing: "didn’t start", in: app), "the failure")
        },
    ]

    // MARK: 5 · Search first

    private static let searchFirst: [RenderScenario] = [
        RenderScenario(name: "start5-a-fleet", purpose: "option 5: the search door", arguments: args(5)) {
            app, driver in
            try driver.fleet(app)
            try driver.requireExists(app.buttons["start-entry"], "the search door")
        },
        RenderScenario(name: "start5-b-search", purpose: "option 5: search, nothing typed", arguments: args(5)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try driver.requireExists(app.buttons["start-claude-query-parser"], "the results")
        },
        RenderScenario(name: "start5-c-typed", purpose: "option 5: search narrowed", arguments: args(5)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-search", "pay", driver)
            try driver.requireExists(
                app.buttons["start-claude-payments-reconciliation-worker"], "the narrowed results")
        },
        RenderScenario(name: "start5-d-starting", purpose: "option 5: starting", arguments: args(5)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-claude-query-parser", driver)
            try starting(app, driver)
        },
        RenderScenario(
            name: "start5-e-failed", purpose: "option 5: the folder is gone",
            arguments: args(5, failing: "folder_missing")
        ) { app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-search", "billing", driver)
            try tap(app, "start-claude-old-billing-service", driver)
            try driver.requireExists(driver.text(containing: "isn’t there", in: app), "the failure")
        },
    ]
}
