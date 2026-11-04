import XCTest

/// New session, step by step: from the fleet's + New through each question,
/// starting and the live session, with the failures, Codex not installed, two
/// paired computers, browsing and the git choices. On the `deck` fleet with
/// forty projects and both agents installed unless a scenario says otherwise.
extension RenderCatalog {
    /// Every screen, at L and AX5.
    static let startSession: [RenderScenario] = steps

    private static let message = "Add a --strict flag that rejects trailing commas, with tests"
    private static let server = "build-server-eu-west-1"
    /// The longest path in the list, for the folder-gone failure.
    private static let longProject = "payments-reconciliation-worker"

    private static func args(
        fixture: String = "deck", outcome: String? = nil, noCodex: Bool = false, twoComputers: Bool = false
    ) -> [String] {
        ["-CC_FIXTURE", fixture, "-CC_FIXTURE_REAL_NAMES", "YES"]
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

    // MARK: The steps

    /// Scroll a step's list until the element sits whole above the pinned
    /// bar, in short drags: at AX5 the list's window is a row or two, and a
    /// long drag carries a row past it between two looks.
    @discardableResult
    private static func reveal(_ app: XCUIApplication, _ id: String, _ driver: RenderDriver) throws
        -> XCUIElement
    {
        let element = app.descendants(matching: .any)[id].firstMatch
        let list = try driver.requireExists(app.descendants(matching: .any)["start-list"].firstMatch, "the list")
        func clear() -> Bool {
            guard element.exists, element.isHittable else { return false }
            let bar = ["start-go", "start-browse", "start-use-folder", "start-continue"]
                .map { app.buttons[$0] }.filter(\.exists).map(\.frame.minY).min()
            return element.frame.maxY <= (bar ?? list.frame.maxY)
        }
        for _ in 0..<40 where !clear() {
            // Back down when the row has passed above the window.
            let above = element.exists && element.frame.minY < list.frame.minY
            let start = list.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.3))
            start.press(forDuration: 0.05, thenDragTo: start.withOffset(CGVector(dx: 0, dy: above ? 100 : -100)))
        }
        // A render must show what it is named for, not the nearest screen.
        guard clear() else { throw RenderFailure.unreachable("\(id), whole above the bar") }
        return element
    }

    private static func step(_ app: XCUIApplication, _ id: String, _ driver: RenderDriver) throws {
        try reveal(app, id, driver).tap()
    }

    /// New session, then the first computer.
    private static func toFolders(_ app: XCUIApplication, _ driver: RenderDriver) throws {
        try tap(app, "start-entry", driver)
        let computer = app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'start-computer-'"))
            .firstMatch
        try driver.requireExists(computer, "a computer").tap()
        try driver.requireExists(app.buttons["start-browse"], "the folder step")
    }

    private static func toBrowse(_ app: XCUIApplication, _ driver: RenderDriver) throws {
        try toFolders(app, driver)
        try tap(app, "start-browse", driver)
        try driver.requireExists(app.buttons["start-use-folder"], "the browser")
    }

    /// Through to the agent step in a recent folder.
    private static func toAgents(_ app: XCUIApplication, _ driver: RenderDriver, _ folder: String = "query-parser")
        throws
    {
        try toFolders(app, driver)
        if folder != "query-parser" { try type(app, "start-search", "\(folder.prefix(4))\n", driver) }
        try step(app, "start-project-\(folder)", driver)
        try driver.requireExists(app.buttons["start-agent-claude"], "the agent step")
    }

    private static func toBranches(_ app: XCUIApplication, _ driver: RenderDriver, _ folder: String = "query-parser")
        throws
    {
        try toAgents(app, driver, folder)
        try tap(app, "start-agent-claude", driver)
        try driver.requireExists(driver.element(containing: "This folder", in: app), "the branch step")
    }

    private static func toReady(_ app: XCUIApplication, _ driver: RenderDriver) throws {
        try toBranches(app, driver)
        try step(app, "start-place-main", driver)
        try driver.requireExists(app.buttons["start-go"], "Start")
    }

    /// Type into a step's field once it is scrolled to and focused: at AX5
    /// the first message sits at the end of the list, under the bar.
    private static func typeIn(_ app: XCUIApplication, _ id: String, _ text: String, _ driver: RenderDriver) throws {
        try step(app, id, driver)
        try driver.requireExists(app.keyboards.firstMatch, "the keyboard")
        app.typeText(text)
    }

    private static let newWorktree = "fix-login"

    private static let steps: [RenderScenario] = [
        RenderScenario(name: "start2-0-fleet", purpose: "steps: + New in the fleet's bar", arguments: args()) {
            app, driver in
            try driver.fleet(app)
            try driver.requireExists(app.buttons["start-entry"], "+ New")
        },
        RenderScenario(name: "start2-a-computer", purpose: "steps: the computer, one paired", arguments: args()) {
            app, driver in
            try tap(app, "start-entry", driver)
            try driver.requireExists(
                app.buttons.matching(NSPredicate(format: "identifier BEGINSWITH 'start-computer-'")).firstMatch,
                "the computer list")
        },
        RenderScenario(
            name: "start2-a2-two-computers", purpose: "steps: the computer, two paired",
            arguments: args(twoComputers: true)
        ) { app, driver in
            try tap(app, "start-entry", driver)
            try driver.requireExists(app.buttons["start-computer-\(server)"], "the second computer")
        },
        RenderScenario(name: "start2-b-folder", purpose: "steps: recent folders", arguments: args()) {
            app, driver in
            try toFolders(app, driver)
        },
        RenderScenario(name: "start2-b2-browse", purpose: "steps: browsing, the home folder", arguments: args()) {
            app, driver in
            try toBrowse(app, driver)
            try driver.requireExists(app.buttons["start-folder-code"], "the home folder's folders")
        },
        RenderScenario(name: "start2-b3-hidden", purpose: "steps: browsing, hidden folders shown", arguments: args()) {
            app, driver in
            try toBrowse(app, driver)
            // The switch itself: a toggle's label does not flip it.
            try driver.requireExists(app.switches.firstMatch, "the hidden toggle")
                .coordinate(withNormalizedOffset: CGVector(dx: 0.93, dy: 0.5)).tap()
            try reveal(app, "start-folder-.config", driver)
        },
        RenderScenario(name: "start2-b4-root", purpose: "steps: browsing, up to /", arguments: args()) {
            app, driver in
            try toBrowse(app, driver)
            try step(app, "start-up", driver)
            try step(app, "start-up", driver)
            try driver.requireExists(app.buttons["start-folder-Applications"], "/")
        },
        RenderScenario(name: "start2-b5-code", purpose: "steps: browsing, a folder of projects", arguments: args()) {
            app, driver in
            try toBrowse(app, driver)
            try step(app, "start-folder-code", driver)
            try reveal(app, "start-folder-query-parser", driver)
        },
        RenderScenario(name: "start2-b6-new-folder", purpose: "steps: naming a new folder", arguments: args()) {
            app, driver in
            try toBrowse(app, driver)
            try step(app, "start-new-folder", driver)
            try driver.requireExists(app.alerts.textFields.firstMatch, "the folder name").typeText("scratch")
            try driver.requireExists(app.keyboards.firstMatch, "the keyboard")
        },
        RenderScenario(name: "start2-b7-denied", purpose: "steps: browsing, a folder that can't be read", arguments: args()) {
            app, driver in
            try toBrowse(app, driver)
            try step(app, "start-up", driver)
            try step(app, "start-up", driver)
            try step(app, "start-folder-private", driver)
            try driver.requireExists(driver.text(containing: "Permission denied", in: app), "the computer's words")
        },
        RenderScenario(name: "start2-d6-subfolder", purpose: "steps: a repository's subfolder, Codex's new worktree", arguments: args()) {
            app, driver in
            try toBrowse(app, driver)
            try step(app, "start-folder-code", driver)
            try step(app, "start-folder-query-parser", driver)
            try step(app, "start-folder-src", driver)
            try tap(app, "start-use-folder", driver)
            try tap(app, "start-agent-codex", driver)
            try reveal(app, "start-new-worktree", driver).tap()
            try driver.requireExists(app.textFields.firstMatch, "the name").tap()
            app.textFields.firstMatch.typeText(newWorktree)
            try driver.requireExists(driver.text(containing: "current commit", in: app), "Codex's base")
        },
        RenderScenario(name: "start2-c-agent", purpose: "steps: the agent", arguments: args()) {
            app, driver in
            try toAgents(app, driver)
        },
        RenderScenario(name: "start2-c2-no-codex", purpose: "steps: the agent, Codex not installed", arguments: args(noCodex: true)) {
            app, driver in
            try toAgents(app, driver)
            try driver.requireExists(driver.text(containing: "isn’t installed", in: app), "the reason")
        },
        RenderScenario(name: "start2-d-branch", purpose: "steps: the branch", arguments: args()) {
            app, driver in
            try toBranches(app, driver)
        },
        RenderScenario(name: "start2-d2-branch-end", purpose: "steps: the branch, its list's end", arguments: args()) {
            app, driver in
            try toBranches(app, driver)
            try reveal(app, "start-new-worktree", driver)
        },
        RenderScenario(name: "start2-d3-long-branch", purpose: "steps: the branch, a long one", arguments: args()) {
            app, driver in
            try toBranches(app, driver, longProject)
            try reveal(app, "start-place-fix/idempotent-ledger-replay-after-partial-failure", driver)
        },
        RenderScenario(name: "start2-d4-new-worktree", purpose: "steps: naming a new worktree", arguments: args()) {
            app, driver in
            try toBranches(app, driver)
            try step(app, "start-new-worktree", driver)
            try driver.requireExists(app.textFields.firstMatch, "the name").tap()
            app.textFields.firstMatch.typeText(newWorktree)
            try driver.requireExists(driver.text(containing: "worktree-\(newWorktree)", in: app), "the branch it gets")
        },
        RenderScenario(name: "start2-d5-bad-name", purpose: "steps: a name the rule refuses", arguments: args()) {
            app, driver in
            try toBranches(app, driver)
            try step(app, "start-new-worktree", driver)
            try driver.requireExists(app.textFields.firstMatch, "the name").tap()
            app.textFields.firstMatch.typeText("fix login!")
        },
        RenderScenario(name: "start2-e-ready", purpose: "steps: ready, on the folder's branch", arguments: args()) {
            app, driver in
            try toReady(app, driver)
        },
        RenderScenario(name: "start2-e2-ready-worktree", purpose: "steps: ready, in a worktree", arguments: args()) {
            app, driver in
            try toBranches(app, driver)
            try step(app, "start-place-fix/unicode-escapes", driver)
            try driver.requireExists(app.buttons["start-go"], "Start")
        },
        RenderScenario(name: "start2-e3-message", purpose: "steps: ready, a first message typed", arguments: args()) {
            app, driver in
            try toReady(app, driver)
            try typeIn(app, "start-message", message, driver)
        },
        RenderScenario(name: "start2-e4-not-repo", purpose: "steps: ready, a folder outside git", arguments: args()) {
            app, driver in
            try toAgents(app, driver, "dotfiles")
            try tap(app, "start-agent-claude", driver)
            try driver.requireExists(app.buttons["start-go"], "Start")
        },
        RenderScenario(name: "start2-f-starting", purpose: "steps: starting, in a worktree", arguments: args(outcome: "pending")) {
            app, driver in
            try toBranches(app, driver)
            try step(app, "start-place-fix/unicode-escapes", driver)
            try tap(app, "start-go", driver)
            try starting(app, driver)
        },
        RenderScenario(name: "start2-h-empty", purpose: "steps: live, nothing recorded yet", arguments: args()) {
            app, driver in
            try toReady(app, driver)
            try tap(app, "start-go", driver)
            try driver.requireExists(driver.element(containing: "Open the terminal", in: app), "the terminal offer")
        },
        RenderScenario(
            name: "start2-i-folder-gone", purpose: "steps: the folder is gone", arguments: args(outcome: "folder_missing")
        ) { app, driver in
            try toAgents(app, driver, longProject)
            try tap(app, "start-agent-claude", driver)
            try step(app, "start-place-fix/idempotent-ledger-replay-after-partial-failure", driver)
            try tap(app, "start-go", driver)
            try failed(app, driver, "isn’t there")
        },
        RenderScenario(
            name: "start2-i2-agent-missing", purpose: "steps: the agent is not installed",
            arguments: args(outcome: "agent_missing")
        ) { app, driver in
            try toAgents(app, driver, "dotfiles")
            try tap(app, "start-agent-claude", driver)
            try tap(app, "start-go", driver)
            try failed(app, driver, "isn’t installed")
        },
        RenderScenario(name: "start2-g-live", purpose: "steps: live, first message sent", arguments: args()) {
            app, driver in
            try toReady(app, driver)
            try typeIn(app, "start-message", message, driver)
            try startTyped(app, driver)
            try live(app, driver, withMessage: true)
        },
    ]
}
