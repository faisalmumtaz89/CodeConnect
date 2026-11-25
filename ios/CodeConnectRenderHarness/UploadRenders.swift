import XCTest

/// Files with a reply: the "+" at the pill's start and its menu, and the tray holding one file in each state
/// (Kit's `-CC_FIXTURE_FILES`). The fx-7 conversation.
extension RenderCatalog {
    private static func upload(_ extra: [String]) -> [String] {
        ["-CC_FIXTURE", "timeline", "-CC_DEEPLINK", "codeconnect://session/fx-7",
         "-CC_FIXTURE_PLAIN_CONVERSATION", "YES", "-CC_FIXTURE_NOW", nineFortyOneToday] + extra
    }

    static let uploads: [RenderScenario] = [
        RenderScenario(
            name: "upload-plus", purpose: "a computer that takes files: + at the reply pill's start",
            arguments: upload(["-CC_FIXTURE_UPLOADS", "YES"])
        ) { app, driver in
            try driver.require(driver.text(containing: "Turn complete", in: app), "the conversation")
            try driver.require(app.buttons["attach.menu"], "the + button")
        },
        RenderScenario(
            name: "upload-menu", purpose: "+ opens Photos and Choose File (no camera in the simulator)",
            arguments: upload(["-CC_FIXTURE_UPLOADS", "YES"])
        ) { app, driver in
            try driver.require(driver.text(containing: "Turn complete", in: app), "the conversation")
            try driver.require(app.buttons["attach.menu"], "the + button").tap()
            try driver.require(
                app.buttons.matching(NSPredicate(format: "label == 'Choose File'")).firstMatch, "the menu")
        },
        RenderScenario(
            name: "upload-tray", purpose: "the tray: arrived, on its way at 40%, failed, refused, and why",
            arguments: upload(["-CC_FIXTURE_FILES", "YES"])
        ) { app, driver in
            try driver.require(driver.text(containing: "Turn complete", in: app), "the conversation")
            try driver.requireExists(app.descendants(matching: .any)["attach.tile"], "the tray's tiles")
            try driver.requireExists(app.descendants(matching: .any)["attach.problem"], "the first problem, in words")
        },
        RenderScenario(
            name: "upload-none", purpose: "a computer that takes no files: no +",
            arguments: upload([])
        ) { app, driver in
            try driver.require(driver.text(containing: "Turn complete", in: app), "the conversation")
            if app.buttons["attach.menu"].exists { throw RenderFailure.unreachable("no + on a computer that takes no files") }
        },
    ]
}
