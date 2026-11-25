import XCTest

/// An Antigravity run's own commands: its "/" palette and its "…" menu offer Compact and never Clear (agy has no
/// clear), and Compact's sheet names Antigravity. The recorded live run (`-CC_FIXTURE antigravity`).
extension RenderCatalog {
    private static let agyRun = ["-CC_FIXTURE", "antigravity", "-CC_DEEPLINK", "codeconnect://session/01M88F6WB3V8YECRANAJVF4VXC"]

    private static func atAgyRun(_ app: XCUIApplication, _ driver: RenderDriver) throws {
        try driver.require(driver.text(containing: "Hello.", in: app), "the Antigravity run")
    }

    static let antigravityCommands: [RenderScenario] = [
        RenderScenario(name: "agy-palette", purpose: "an Antigravity run's / palette: model, effort, compact, diff; no clear", arguments: agyRun) {
            app, driver in
            try atAgyRun(app, driver)
            try driver.typeIntoComposer(app, "/")
            try driver.require(driver.element(containing: "/compact", in: app), "the /compact row")
            if driver.element(containing: "/clear", in: app).exists {
                throw RenderFailure.unreachable("no /clear row on an Antigravity run")
            }
        },
        RenderScenario(name: "agy-more", purpose: "an Antigravity run's …: Compact… and no Clear…", arguments: agyRun) {
            app, driver in
            try atAgyRun(app, driver)
            try driver.openSessionMenu(app)
            try driver.require(app.buttons.matching(NSPredicate(format: "label == 'Compact…'")).firstMatch, "Compact… in the menu")
            if app.buttons.matching(NSPredicate(format: "label == 'Clear…'")).firstMatch.exists {
                throw RenderFailure.unreachable("no Clear… on an Antigravity run")
            }
        },
        RenderScenario(name: "agy-compact", purpose: "Compact's sheet on an Antigravity run, in Antigravity's words", arguments: agyRun) {
            app, driver in
            try atAgyRun(app, driver)
            try driver.openSessionMenu(app)
            try driver.require(app.buttons.matching(NSPredicate(format: "label == 'Compact…'")).firstMatch, "Compact…").tap()
            try driver.require(driver.text(containing: "Ask Antigravity to compact", in: app), "the sheet in Antigravity's words")
        },
    ]
}
