import XCTest

/// The five ways of starting a session, each from the fleet through starting
/// to the live session, plus one failure, Codex not installed, and two paired
/// computers. On the `deck` fleet with forty projects and both agents
/// installed unless a scenario says otherwise. `startN-…` is option N of
/// `StartOption`.
extension RenderCatalog {
    static let startOptions: [RenderScenario] =
        recentsSheet + steps + cardAction + messageFirst + searchFirst

    /// Each option's densest screen, its project list, at AX5.
    static let startOptionsAX5: [RenderScenario] = [
        RenderScenario(
            name: "start1-b-sheet", purpose: "option 1: the list under the panel",
            arguments: args(1)
        ) { app, driver in
            try tap(app, "start-entry", driver)
            let list = try driver.requireExists(
                app.descendants(matching: .any)["start-list"].firstMatch, "the list")
            driver.dragUp(within: list)
            try driver.requireExists(app.buttons["start-project-storefront-api"], "the list's rows")
        },
        steps[1], cardAction[3], messageFirst[3], searchFirst[1],
    ]

    /// Option 1 alone, for its width and type-size passes: every screen at L,
    /// and every screen plus the list scrolled under its panel at AX5.
    static let optionOne: [RenderScenario] = recentsSheet
    static let optionOneAX5: [RenderScenario] =
        recentsSheet + [
            RenderScenario(
                name: "start1-b2-list", purpose: startOptionsAX5[0].purpose,
                arguments: startOptionsAX5[0].arguments, reach: startOptionsAX5[0].reach)
        ]

    private static let message = "Add a --strict flag that rejects trailing commas, with tests"
    private static let server = "build-server-eu-west-1"
    /// The longest path in the list, for the folder-gone failure.
    private static let longProject = "payments-reconciliation-worker"

    private static func args(
        _ option: Int, fixture: String = "deck", outcome: String? = nil, noCodex: Bool = false,
        twoComputers: Bool = false
    ) -> [String] {
        ["-CC_FIXTURE", fixture, "-CC_FIXTURE_REAL_NAMES", "YES", "-CC_START_OPTION", "\(option)"]
            + (outcome.map { ["-CC_START_OUTCOME", $0] } ?? [])
            + (noCodex ? ["-CC_START_AGENTS", "claude"] : [])
            + (twoComputers
                ? ["-CC_FIXTURE_SECOND_MACHINE", server, "-CC_FIXTURE_SECOND_LIVE", "YES"] : [])
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

    /// Start with a first message typed: Start, or at the accessibility sizes,
    /// where the bar steps aside for the keyboard, the message's own send.
    private static func startTyped(_ app: XCUIApplication, _ driver: RenderDriver) throws {
        let go = app.buttons["start-go"]
        try tap(app, go.exists ? "start-go" : "start-message-button", driver)
    }

    private static func starting(_ app: XCUIApplication, _ driver: RenderDriver) throws {
        try driver.requireExists(driver.text(containing: "Starting", in: app), "the starting screen")
    }

    /// The live session: its first message in the timeline, or, with none,
    /// the keyboard up in its composer.
    private static func live(_ app: XCUIApplication, _ driver: RenderDriver, withMessage: Bool) throws {
        if withMessage {
            try driver.requireExists(driver.text(containing: "--strict flag", in: app), "the first message")
        } else {
            try driver.requireExists(app.keyboards.firstMatch, "the keyboard in the composer")
        }
        Thread.sleep(forTimeInterval: 0.6)
    }

    private static func failed(_ app: XCUIApplication, _ driver: RenderDriver, _ fragment: String) throws {
        try driver.requireExists(driver.text(containing: fragment, in: app), "the failure")
    }

    // MARK: 1 · Recents sheet

    private static let recentsSheet: [RenderScenario] = [
        RenderScenario(name: "start1-a-fleet", purpose: "option 1: + New in the bar", arguments: args(1)) {
            app, driver in
            try driver.fleet(app)
            try driver.requireExists(app.buttons["start-entry"], "+ New")
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
        RenderScenario(name: "start1-c2-search", purpose: "option 1: searching, keyboard up", arguments: args(1)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-search", "e", driver)
        },
        RenderScenario(
            name: "start1-c3-search-hides", purpose: "option 1: a search that hides the chosen project",
            arguments: args(1)
        ) { app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-search", "ios\n", driver)
            try driver.requireExists(app.buttons["start-project-ios-app"], "the result")
            guard !app.buttons["start-go"].exists else { throw RenderFailure.unreachable("no Start") }
        },
        RenderScenario(name: "start1-d-starting", purpose: "option 1: starting", arguments: args(1, outcome: "pending")) {
            app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-message", message, driver)
            try startTyped(app, driver)
            try starting(app, driver)
        },
        RenderScenario(name: "start1-e-live", purpose: "option 1: live, first message sent", arguments: args(1)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-message", message, driver)
            try startTyped(app, driver)
            try live(app, driver, withMessage: true)
        },
        RenderScenario(name: "start1-e2-live-typing", purpose: "option 1: live, no first message", arguments: args(1)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-go", driver)
            try live(app, driver, withMessage: false)
        },
        RenderScenario(
            name: "start1-f2-agent-missing", purpose: "option 1: the agent is not installed",
            arguments: args(1, outcome: "agent_missing")
        ) { app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-go", driver)
            try failed(app, driver, "isn’t installed")
        },
        RenderScenario(
            name: "start1-f-failed", purpose: "option 1: the folder is gone",
            arguments: args(1, outcome: "folder_missing")
        ) { app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-search", "pay", driver)
            try tap(app, "start-project-\(longProject)", driver)
            try tap(app, "start-go", driver)
            try failed(app, driver, "isn’t there")
        },
        RenderScenario(name: "start1-g-no-codex", purpose: "option 1: Codex not installed", arguments: args(1, noCodex: true)) {
            app, driver in
            try tap(app, "start-entry", driver)
            // Third in the list: below the fold at AX5, scrolled to as a thumb would.
            let row = app.buttons["start-project-ios-app"]
            let list = try driver.requireExists(
                app.descendants(matching: .any)["start-list"].firstMatch, "the list")
            for _ in 0..<10 where !(row.exists && row.isHittable) { driver.dragUp(within: list) }
            try driver.requireExists(row, "ios-app").tap()
            try driver.requireExists(app.buttons["start-go"], "Start")
        },
        RenderScenario(name: "start1-g2-agent-menu", purpose: "option 1: the agent menu, Codex not installed", arguments: args(1, noCodex: true)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-agent", driver)
            try driver.requireExists(driver.element(containing: "not installed", in: app), "the agent menu")
        },
        RenderScenario(name: "start1-h-two-computers", purpose: "option 1: two computers", arguments: args(1, twoComputers: true)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try driver.requireExists(app.buttons["start-computer-\(server)"], "the computer picker")
        },
    ]

    // MARK: 2 · Steps

    private static let steps: [RenderScenario] = [
        RenderScenario(name: "start2-a-fleet", purpose: "option 2: the fleet's +", arguments: args(2)) {
            app, driver in
            try driver.fleet(app)
            try driver.requireExists(app.buttons["start-entry"], "the + button")
        },
        RenderScenario(name: "start2-b-where", purpose: "option 2: where", arguments: args(2)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try driver.requireExists(app.buttons["start-project-query-parser"], "the project list")
        },
        RenderScenario(name: "start2-c-agent", purpose: "option 2: which agent, both installed", arguments: args(2)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-project-ios-app", driver)
            try driver.requireExists(app.buttons["start-continue"], "Continue")
        },
        RenderScenario(name: "start2-d-message", purpose: "option 2: the first message", arguments: args(2)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-project-query-parser", driver)
            try tap(app, "start-continue", driver)
            try type(app, "start-message", message, driver)
        },
        RenderScenario(name: "start2-e-starting", purpose: "option 2: starting", arguments: args(2, outcome: "pending")) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-project-query-parser", driver)
            try tap(app, "start-continue", driver)
            try tap(app, "start-go", driver)
            try starting(app, driver)
        },
        RenderScenario(name: "start2-f-live", purpose: "option 2: live, keyboard up", arguments: args(2)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-project-query-parser", driver)
            try tap(app, "start-continue", driver)
            try tap(app, "start-go", driver)
            try live(app, driver, withMessage: false)
        },
        RenderScenario(
            name: "start2-g-failed", purpose: "option 2: the agent is not installed",
            arguments: args(2, outcome: "agent_missing")
        ) { app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-project-query-parser", driver)
            try tap(app, "start-continue", driver)
            try tap(app, "start-go", driver)
            try failed(app, driver, "isn’t installed")
        },
        RenderScenario(name: "start2-h-no-codex", purpose: "option 2: Codex not installed", arguments: args(2, noCodex: true)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-project-ios-app", driver)
            try driver.requireExists(app.buttons["start-continue"], "Continue")
        },
        RenderScenario(name: "start2-i-two-computers", purpose: "option 2: which computer", arguments: args(2, twoComputers: true)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try driver.requireExists(app.buttons["start-computer-\(server)"], "the computer step")
        },
    ]

    // MARK: 3 · From the card

    private static func firstCardAction(_ app: XCUIApplication, _ driver: RenderDriver) throws {
        let action = app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'start-card-'"))
            .firstMatch
        try driver.requireExists(action, "a card's New session").tap()
    }

    private static func openAll(_ app: XCUIApplication, _ driver: RenderDriver) throws {
        try driver.fleet(app)
        let all = app.buttons["start-all"]
        try driver.scrollUntil(app, "All projects", attempts: 40) { all.exists && all.isHittable }
        all.tap()
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
            try openAll(app, driver)
            try driver.requireExists(app.buttons["start-project-query-parser"], "the project list")
        },
        RenderScenario(name: "start3-e-starting", purpose: "option 3: starting", arguments: args(3, outcome: "pending")) {
            app, driver in
            try firstCardAction(app, driver)
            try type(app, "start-message", message, driver)
            try tap(app, "start-go", driver)
            try starting(app, driver)
        },
        RenderScenario(name: "start3-f-live", purpose: "option 3: live, first message sent", arguments: args(3)) {
            app, driver in
            try firstCardAction(app, driver)
            try type(app, "start-message", message, driver)
            try tap(app, "start-go", driver)
            try live(app, driver, withMessage: true)
        },
        RenderScenario(
            name: "start3-g-failed", purpose: "option 3: the folder is gone",
            arguments: args(3, outcome: "folder_missing")
        ) { app, driver in
            try openAll(app, driver)
            try type(app, "start-search", "pay", driver)
            try tap(app, "start-project-\(longProject)", driver)
            try tap(app, "start-go", driver)
            try failed(app, driver, "isn’t there")
        },
        RenderScenario(name: "start3-h-no-codex", purpose: "option 3: Codex not installed", arguments: args(3, noCodex: true)) {
            app, driver in
            try firstCardAction(app, driver)
            try driver.requireExists(app.buttons["start-go"], "Start")
        },
        RenderScenario(name: "start3-i-two-computers", purpose: "option 3: recent projects from two computers", arguments: args(3, twoComputers: true)) {
            app, driver in
            try driver.fleet(app)
            try driver.require(app.buttons["start-all"], "All projects")
        },
    ]

    // MARK: 4 · Message first

    private static let messageFirst: [RenderScenario] = [
        RenderScenario(name: "start4-a-fleet", purpose: "option 4: composer under the Deck bar, 3 waiting", arguments: args(4)) {
            app, driver in
            try driver.fleet(app)
            try driver.requireExists(app.descendants(matching: .any)["start-message"].firstMatch, "the composer")
        },
        RenderScenario(name: "start4-b-quiet", purpose: "option 4: composer alone, nothing waiting", arguments: args(4, fixture: "timeline")) {
            app, driver in
            try driver.fleet(app)
            try driver.requireExists(app.descendants(matching: .any)["start-message"].firstMatch, "the composer")
        },
        RenderScenario(name: "start4-c-typed", purpose: "option 4: a message typed", arguments: args(4)) {
            app, driver in
            try type(app, "start-message", message, driver)
        },
        RenderScenario(name: "start4-d-pick", purpose: "option 4: choosing the project", arguments: args(4)) {
            app, driver in
            try tap(app, "start-chip-project", driver)
            try driver.requireExists(app.buttons["start-project-query-parser"], "the project list")
        },
        RenderScenario(name: "start4-e-starting", purpose: "option 4: starting", arguments: args(4, outcome: "pending")) {
            app, driver in
            try type(app, "start-message", message, driver)
            try tap(app, "start-message-button", driver)
            try starting(app, driver)
        },
        RenderScenario(name: "start4-f-live", purpose: "option 4: live, first message sent", arguments: args(4)) {
            app, driver in
            try type(app, "start-message", message, driver)
            try tap(app, "start-message-button", driver)
            try live(app, driver, withMessage: true)
        },
        RenderScenario(
            name: "start4-g-failed", purpose: "option 4: the agent is not installed",
            arguments: args(4, outcome: "agent_missing")
        ) { app, driver in
            try type(app, "start-message", message, driver)
            try tap(app, "start-message-button", driver)
            try failed(app, driver, "isn’t installed")
        },
        RenderScenario(name: "start4-h-no-codex", purpose: "option 4: Codex not installed, agent menu", arguments: args(4, noCodex: true)) {
            app, driver in
            try tap(app, "start-chip-agent", driver)
            try driver.requireExists(driver.element(containing: "isn’t installed", in: app), "the menu")
        },
        RenderScenario(name: "start4-i-two-computers", purpose: "option 4: the computer chip", arguments: args(4, twoComputers: true)) {
            app, driver in
            try tap(app, "start-chip-computer", driver)
            try driver.requireExists(driver.element(containing: server, in: app), "the computer menu")
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
            // A fresh simulator's keyboard shows its one-time typing tip first.
            let tip = app.buttons["Continue"]
            if tip.waitForExistence(timeout: 2) { tip.tap() }
            try driver.requireExists(app.buttons["start-claude-query-parser"], "the results")
        },
        RenderScenario(name: "start5-c-typed", purpose: "option 5: search narrowed", arguments: args(5)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-search", "pay", driver)
            try driver.requireExists(app.buttons["start-claude-\(longProject)"], "the narrowed results")
        },
        RenderScenario(name: "start5-d-starting", purpose: "option 5: starting", arguments: args(5, outcome: "pending")) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-claude-query-parser", driver)
            try starting(app, driver)
        },
        RenderScenario(name: "start5-e-live", purpose: "option 5: live, keyboard up", arguments: args(5)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try tap(app, "start-claude-query-parser", driver)
            try live(app, driver, withMessage: false)
        },
        RenderScenario(
            name: "start5-f-failed", purpose: "option 5: the folder is gone",
            arguments: args(5, outcome: "folder_missing")
        ) { app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-search", "pay", driver)
            try tap(app, "start-claude-\(longProject)", driver)
            try failed(app, driver, "isn’t there")
        },
        RenderScenario(name: "start5-g-no-codex", purpose: "option 5: Codex not installed", arguments: args(5, noCodex: true)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try driver.requireExists(app.buttons["start-claude-query-parser"], "the results")
        },
        RenderScenario(name: "start5-h-two-computers", purpose: "option 5: two computers' results", arguments: args(5, twoComputers: true)) {
            app, driver in
            try tap(app, "start-entry", driver)
            try type(app, "start-search", "data", driver)
            try driver.requireExists(app.buttons["start-codex-data-pipeline"], "the second computer's results")
        },
    ]
}
