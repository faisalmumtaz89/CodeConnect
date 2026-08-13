import XCTest

// =============================================================================
//  The render catalog — the screen inventory, executable.
//
//  **This file is part of the screen inventory.** Adding or removing a
//  user-visible screen, or a safety- or honesty-relevant state of one, requires
//  updating the catalog in the same change. A state that is not in here is a
//  state nobody has looked at: seven Deck states went unrendered for exactly
//  that reason, and the two worst bugs on the diff sheet — a blank black
//  rectangle, and the app's own words attributed to the daemon — were both
//  states no render could reach.
//
//  It asserts nothing about pixels. It is an instrument: it reaches a state,
//  proves it is at the size it claims, and photographs it. What it *does* fail
//  on is not being able to get there, because a state nobody can reach is a
//  state nobody has looked at.
// =============================================================================

/// One reachable state of the product, and how to get to it.
struct RenderScenario {
    /// The file stem. `<name>--<size>.png` lands in the run directory.
    let name: String
    /// One line, in the report, saying what this render is for.
    let purpose: String
    /// Launch arguments, appended to the harness's own. All `#if DEBUG` seams —
    /// see the "Test seams" section of `ios/README.md`.
    var arguments: [String] = []
    var environment: [String: String] = [:]
    /// How long to allow for the state to arrive. Generous only where the state
    /// itself takes time (`stale` crosses `LinkHealth.staleAfter` at 45s).
    var timeout: TimeInterval = 30
    /// Drives the app from launch to the state. Throws or fails if it cannot
    /// get there — which is the whole contract.
    let reach: (XCUIApplication, RenderDriver) throws -> Void
}

/// The navigation vocabulary, in one place, so twenty scenarios cannot invent
/// twenty ways to open a session.
///
/// Every idiom here was found the hard way by the product suites and is
/// deliberately identical to them — notably the press-and-drag, because
/// `app.swipeUp()` scrolls nothing in this app: measured on the Deck at AX5,
/// five consecutive calls moved the card's scroll view exactly zero points.
struct RenderDriver {
    let test: XCTestCase
    let timeout: TimeInterval

    // MARK: Waiting

    @discardableResult
    func require(_ element: XCUIElement, _ what: String) throws -> XCUIElement {
        guard element.waitForExistence(timeout: timeout) else {
            throw RenderFailure.unreachable(what)
        }
        return element
    }

    func text(containing fragment: String, in app: XCUIApplication) -> XCUIElement {
        app.staticTexts.matching(NSPredicate(format: "label CONTAINS[c] %@", fragment)).firstMatch
    }

    /// Any element whose label contains the fragment — for strings the kit draws
    /// inside a combined row rather than as their own `staticText`.
    func element(containing fragment: String, in app: XCUIApplication) -> XCUIElement {
        app.descendants(matching: .any)
            .matching(NSPredicate(format: "label CONTAINS[c] %@", fragment)).firstMatch
    }

    // MARK: Scrolling

    /// A controlled press-and-drag between two points that are **both** inside
    /// the document. See the type's note.
    func dragUp(_ app: XCUIApplication) {
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.45))
            .press(
                forDuration: 0.05,
                thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.15)))
    }

    /// Scrolls until `condition` holds, or says what it gave up waiting for.
    func scrollUntil(
        _ app: XCUIApplication, _ what: String, attempts: Int = 12, condition: () -> Bool
    ) throws {
        for _ in 0..<attempts {
            if condition() { return }
            dragUp(app)
        }
        guard condition() else { throw RenderFailure.unreachable(what) }
    }

    // MARK: Common routes

    func fleet(_ app: XCUIApplication) throws {
        try require(app.navigationBars["Fleet"], "the Fleet root")
    }

    func openFirstSession(_ app: XCUIApplication) throws {
        try fleet(app)
        let row = app.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH 'session-'")
        ).firstMatch
        try require(row, "a session row on the fleet")
        row.tap()
    }

    func openDeck(_ app: XCUIApplication) throws {
        let bar = app.buttons["deck-bar"]
        try require(bar, "the Deck accessory bar")
        bar.tap()
        try require(app.navigationBars["Needs you"], "the Deck")
    }

    func openSettings(_ app: XCUIApplication) throws {
        try fleet(app)
        let settings = app.buttons["Settings and pairing"]
        try require(settings, "the Settings toolbar button")
        settings.tap()
        try require(app.navigationBars["Settings"], "the Settings screen")
    }

    /// The session's Terminal tab. `firstMatch` because `safeAreaInset` renders
    /// the segmented control into the hierarchy more than once.
    func openTerminal(_ app: XCUIApplication) throws {
        try openFirstSession(app)
        let terminal = app.buttons["Terminal"].firstMatch
        try require(terminal, "the Terminal tab")
        terminal.tap()
    }

    /// Focuses the composer and types. A vertical-axis TextField is backed
    /// by a text view, not a text field — the same fallback the product
    /// suites use. Navigates only if the Fleet is still on screen, so a
    /// scenario that arrived by deep link is not sent back to the root.
    func typeIntoComposer(_ app: XCUIApplication, _ text: String) throws {
        if app.navigationBars["Fleet"].exists { try openFirstSession(app) }
        let field =
            app.textViews.firstMatch.exists
            ? app.textViews.firstMatch : app.textFields.firstMatch
        try require(field, "the compose bar")
        field.tap()
        field.typeText(text)
    }

    /// Types a fragment and taps the palette row it filters to — the same
    /// door a thumb uses, so the route itself proves the row responds.
    func openPaletteRow(_ app: XCUIApplication, fragment: String, row: String) throws {
        try typeIntoComposer(app, fragment)
        let target = app.buttons.matching(
            NSPredicate(format: "label BEGINSWITH[c] %@", row)
        ).firstMatch
        try require(target, "the \(row) palette row")
        target.tap()
    }

    /// Taps a labelled row inside an open sheet, having first established that
    /// it is there. Never `app.buttons[label].tap()` directly: tapping an
    /// element that does not exist fails as an opaque snapshot timeout, which
    /// reads exactly like an app hang and sends the next reader hunting for
    /// one. `require` turns the same case into "could not reach <row>".
    func tapRow(_ app: XCUIApplication, _ label: String) throws {
        try require(app.buttons[label].firstMatch, "the \(label) row").tap()
    }
}

/// The one failure this harness raises. It is never about a pixel.
enum RenderFailure: Error, CustomStringConvertible {
    case unreachable(String)
    /// The process resolved to a different content-size category than the one
    /// the pass claims to be rendering. See `CCRenderProbe` in `RootView.swift`.
    case wrongTypeSize(expected: String, actual: String)

    var description: String {
        switch self {
        case .unreachable(let what):
            return "could not reach \(what)"
        case .wrongTypeSize(let expected, let actual):
            return "rendered at \(actual), not \(expected) — the pass would have been a lie"
        }
    }
}

// MARK: - The catalog

enum RenderCatalog {
    /// Every state this harness knows how to reach.
    ///
    /// Ordered the way a reader meets them: the fleet, the decision, the
    /// session, the diff, the terminal, the trust screen, then the kit's own
    /// gallery — which is the only reachable render several components have.
    static let all: [RenderScenario] = product + gallery

    static let product: [RenderScenario] = [
        RenderScenario(
            name: "fleet",
            purpose: "the 4-second surface, with one agent holding two decisions",
            arguments: ["-CC_FIXTURE", "stacked"],
            reach: { app, driver in try driver.fleet(app) }),

        RenderScenario(
            name: "fleet-cached",
            purpose:
                "a fleet read off disk, stating its age while the wait clocks keep ticking",
            arguments: ["-CC_FIXTURE", "stacked", "-CC_FIXTURE_CACHED", "372"],
            reach: { app, driver in
                try driver.fleet(app)
                // The banner has to *say* the age. A cached fleet that renders
                // without one is the defect this scenario exists to catch.
                try driver.require(
                    driver.element(containing: "last known state", in: app),
                    "the cached-fleet banner and its age")
            }),

        RenderScenario(
            name: "fleet-stale",
            purpose:
                "the link has gone quiet: every action disables itself **with its reason**",
            arguments: ["-CC_FIXTURE", "stacked", "-CC_FIXTURE_LINK", "stale"],
            // `LinkHealth.staleAfter` is 45s and the fixture withholds its
            // keep-alive `pong`, so the state arrives on its own. There is no
            // shorter way in, and without it the state is unaudited — which is
            // how the accessory bar shipped with its blocked reason cut
            // mid-word at `…but the daem…` and dimmed to 3.16:1.
            timeout: 90,
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(
                    driver.element(containing: "since the daemon last spoke", in: app),
                    "the stale-link banner")
            }),

        RenderScenario(
            name: "deck-high",
            purpose: "the HIGH card as it arrives, before the command has been read",
            // The deep link rather than a tap on the accessory bar: at
            // accessibility sizes the bar's decorative `Review` button is gone
            // and its label-predicate tap resolves to a container whose centre
            // misses the control. The bar itself is rendered by `fleet`.
            arguments: [
                "-CC_FIXTURE", "deck", "-CC_BIOMETRICS", "allow",
                "-CC_DEEPLINK", "codeconnect://deck/toolu_fixture_high",
            ],
            reach: { app, driver in
                try driver.require(app.navigationBars["Needs you"], "the Deck")
                try driver.require(
                    app.staticTexts.matching(
                        NSPredicate(format: "label BEGINSWITH 'Risk HIGH'")
                    ).firstMatch,
                    "the HIGH risk badge")
            }),

        RenderScenario(
            name: "deck-high-read",
            purpose: "the same card with the read gate satisfied — the hold target armed",
            arguments: [
                "-CC_FIXTURE", "deck", "-CC_BIOMETRICS", "allow",
                "-CC_DEEPLINK", "codeconnect://deck/toolu_fixture_high",
            ],
            reach: { app, driver in
                try driver.require(app.navigationBars["Needs you"], "the named HIGH card")
                let hold = app.buttons.matching(identifier: "Hold to allow").firstMatch
                try driver.require(hold, "the hold-to-allow control")
                try driver.scrollUntil(app, "the read gate arms") { hold.isEnabled }
            }),

        RenderScenario(
            name: "deck-biometrics-refused",
            purpose:
                "a refused Face ID must explain itself **above** the action bar, not under it",
            arguments: [
                "-CC_FIXTURE", "deck", "-CC_BIOMETRICS", "deny",
                "-CC_DEEPLINK", "codeconnect://deck/toolu_fixture_high",
            ],
            reach: { app, driver in
                try driver.require(app.navigationBars["Needs you"], "the named HIGH card")
                let hold = app.buttons.matching(identifier: "Hold to allow").firstMatch
                try driver.require(hold, "the hold-to-allow control")
                try driver.scrollUntil(app, "the read gate arms") { hold.isEnabled }
                hold.press(forDuration: 1.6)
                try driver.require(
                    driver.element(containing: "Not approved", in: app),
                    "the refused-biometric notice")
            }),

        RenderScenario(
            name: "session-timeline",
            purpose: "one run's timeline",
            arguments: ["-CC_FIXTURE", "deck"],
            reach: { app, driver in
                try driver.openFirstSession(app)
                try driver.require(app.buttons["open-diff"], "the session detail")
            }),

        // **The decision, as a sheet.** The Deck renders this card full
        // screen; from a session it arrives as a sheet, and until now nothing
        // photographed that. It is the tallest thing the design system
        // presents on a sheet — a command block, two prose blocks and a
        // three-button bar — so it is where a sheet height policy is felt
        // first.
        RenderScenario(
            name: "session-decision-sheet",
            purpose: "the approval card presented as a sheet, where its height is tightest",
            // The deep link is here to *scroll* the timeline to the card, not
            // to open it: at AX5 the Review button is otherwise far below the
            // fold and a tap route photographs this at reading size only —
            // the gap `session-tool-rows` records.
            //
            // It is not relied on to open the sheet. `?request=` opens the
            // card at `L` and not at AX5, because the route is consumed once
            // and never retried: a slower launch resolves it before the
            // approval has arrived and the request is spent on nothing. That
            // is a defect in the deep link, not in this scenario, so the
            // button is what this drives.
            arguments: [
                "-CC_FIXTURE", "deck",
                "-CC_DEEPLINK", "codeconnect://session/fx-1?request=toolu_fixture_high",
            ],
            reach: { app, driver in
                if !app.staticTexts["Decision"].waitForExistence(timeout: 5) {
                    try driver.tapRow(app, "Review")
                }
                try driver.require(driver.text(containing: "Decision", in: app), "the sheet title")
            }),

        // **A column of tool rows, which nothing else reaches.** The `deck`
        // fixture's first session has a single approval and no tool calls, so
        // the one place tool rows stack — where their commands have to share a
        // left edge, and where their 44pt targets are paid for — was never
        // photographed. `stacked` puts `Bash`, `Read` and `MultiEdit` on one
        // run precisely because their labels are three different widths.
        RenderScenario(
            name: "session-tool-rows",
            purpose: "a column of tool calls, with the labels at three widths",
            // Deep-linked rather than tapped. At AX5 the Running band is far
            // below the fold, so a tap-driven route reached this at reading
            // size and failed the pass at AX5 — and a scenario that only
            // renders at one size is exactly the coverage gap this catalogue
            // exists to close.
            arguments: [
                "-CC_FIXTURE", "stacked", "-CC_DEEPLINK", "codeconnect://session/fx-5",
            ],
            reach: { app, driver in
                try driver.require(app.buttons["open-diff"], "the session detail")
            }),

        // **The slash palette and its eight native commands.** Every state
        // here shipped broken once for want of a photograph: the first
        // palette read as "Mac only" scolding, and the first /model sheet
        // wore raw ids and a disabled-at-rest button. The fixture daemon
        // advertises `slash_composer_recovery`, so the full eight-row
        // discovery card is the state rendered; the snapshot rows' capability
        // omission is covered by unit tests, not a photograph — a shorter
        // list is not a distinct visual risk.
        RenderScenario(
            name: "session-palette",
            purpose: "bare / — every native row, the caption, the ~4-row scroll cap",
            arguments: [
                "-CC_FIXTURE", "deck", "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.typeIntoComposer(app, "/")
                try driver.require(
                    driver.element(containing: "/model", in: app), "the /model row")
                try driver.require(
                    driver.text(containing: "Run other commands", in: app),
                    "the discovery caption")
            }),
        RenderScenario(
            name: "session-palette-filtered",
            purpose: "a typed fragment filters the rows and drops the caption",
            arguments: [
                "-CC_FIXTURE", "deck", "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.typeIntoComposer(app, "/c")
                try driver.require(
                    driver.element(containing: "/compact", in: app), "the /compact row")
                try driver.require(
                    driver.element(containing: "/cost", in: app), "the /cost row")
            }),
        // **Opened on the session that carries the facts.** `fx-4`'s log
        // holds a SessionStart with the raw hook id `claude-opus-5[1m]` and a
        // real `/effort` confirmation, so these two sheets render their
        // *populated* states — the raw id in monospace, its variant line, the
        // provenance line beneath. That is the exact state that shipped
        // looking broken, and until the fixture carried a model fact no
        // render could reach it: every earlier photograph showed "Not
        // confirmed" and proved nothing about the screen that was rejected.
        RenderScenario(
            name: "session-model-sheet",
            purpose: "the /model sheet with a raw hook id confirmed — the state that shipped broken",
            arguments: [
                "-CC_FIXTURE", "deck", "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/m", row: "/model")
                try driver.require(
                    driver.text(containing: "Choose model", in: app), "the chooser section")
                try driver.require(
                    driver.element(containing: "1M context", in: app),
                    "the variant line the raw id resolves to")
            }),
        RenderScenario(
            name: "session-effort-sheet",
            purpose: "the /effort sheet — five rows, no current-state claim",
            arguments: [
                "-CC_FIXTURE", "deck", "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/e", row: "/effort")
                try driver.require(
                    driver.text(containing: "Choose effort", in: app), "the chooser section")
                try driver.require(
                    driver.element(containing: "Extra high", in: app), "the xhigh row's label")
            }),
        RenderScenario(
            name: "session-compact-sheet",
            purpose: "the /compact sheet — optional field, always-active button",
            arguments: [
                "-CC_FIXTURE", "deck", "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/com", row: "/compact")
                try driver.require(app.buttons["Compact now"], "the always-active button")
            }),
        RenderScenario(
            name: "session-clear-confirm",
            purpose: "the /clear destructive confirmation, consequence stated",
            arguments: [
                "-CC_FIXTURE", "deck", "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/cl", row: "/clear")
                try driver.require(app.buttons["Clear context"], "the destructive action")
            }),
        RenderScenario(
            name: "session-snapshot-captured",
            purpose: "a /status capture — the measured 80-column pane, verbatim",
            arguments: [
                "-CC_FIXTURE", "deck", "-cc.debug.sendText", "recovered",
                "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/st", row: "/status")
                try driver.require(
                    driver.text(containing: "pressed Esc", in: app), "the recovery footer")
            }),
        RenderScenario(
            name: "session-snapshot-lost",
            purpose: "the capture whose Escape failed — no retry, Terminal only",
            arguments: [
                "-CC_FIXTURE", "deck", "-cc.debug.sendText", "lost",
                "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/st", row: "/status")
                try driver.require(
                    driver.text(containing: "Couldn’t restore the composer", in: app),
                    "the honest failure")
            }),

        // ---------------------------------------------------------------
        //  The "no change" states.
        //
        //  Measured on Claude Code 2.1.223: `/model <alias>` and
        //  `/effort <value>` open a confirmation whenever the conversation is
        //  already cached for the current value — which is every session
        //  somebody would actually use these sheets on. Cancel it and Claude
        //  Code prints `Kept model as X` / `Kept effort level as X`.
        //
        //  These are the states that say so, at reading size and at AX5, where
        //  a two-clause sentence is where a sheet breaks.
        // ---------------------------------------------------------------
        RenderScenario(
            name: "session-model-kept",
            purpose: "the model was NOT changed, said plainly",
            arguments: [
                "-CC_FIXTURE", "deck", "-cc.debug.sendText", "kept-model",
                "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/m", row: "/model")
                try driver.tapRow(app, "Sonnet 5")
                try driver.require(
                    driver.text(containing: "was not changed", in: app),
                    "the honest no-change line")
            }),
        RenderScenario(
            name: "session-model-still-waiting",
            purpose: "no receipt arrived: the caption stops promising and the rows come back",
            arguments: [
                "-CC_FIXTURE", "deck", "-cc.debug.sendText", "sent",
                "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/m", row: "/model")
                try driver.tapRow(app, "Sonnet 5")
                // The watch stays armed — this is the moment it stops claiming
                // a receipt is coming, and gives the controls back rather than
                // leaving the sheet inert with a spinner nobody can stop.
                try driver.require(
                    driver.text(containing: "Still waiting", in: app),
                    "the quiet caption")
            }),
        RenderScenario(
            name: "session-model-composer-taken",
            purpose: "the command took the composer and CodeConnect gave it back — no change claimed",
            arguments: [
                "-CC_FIXTURE", "deck", "-cc.debug.sendText", "recovered",
                "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/m", row: "/model")
                try driver.tapRow(app, "Sonnet 5")
                try driver.require(
                    driver.text(containing: "pressed Esc", in: app),
                    "the observed-facts-only line")
            }),
        RenderScenario(
            name: "session-effort-kept",
            purpose: "the level was NOT changed, said plainly",
            arguments: [
                "-CC_FIXTURE", "deck", "-cc.debug.sendText", "kept-effort",
                "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/e", row: "/effort")
                try driver.tapRow(app, "Maximum")
                try driver.require(
                    driver.text(containing: "was not changed", in: app),
                    "the honest no-change line")
            }),
        RenderScenario(
            name: "session-effort-scoped",
            purpose: "a confirmed level carrying Claude Code's own scope words — the longest one",
            arguments: [
                "-CC_FIXTURE", "deck", "-cc.debug.sendText", "set-effort-session",
                "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/e", row: "/effort")
                try driver.tapRow(app, "Maximum")
                try driver.require(
                    driver.text(containing: "this session only", in: app),
                    "the scope clause, verbatim from the receipt")
            }),
        RenderScenario(
            name: "session-snapshot-cost-is-usage",
            purpose: "/cost is titled Usage, because that is the view it opens",
            arguments: [
                "-CC_FIXTURE", "deck", "-cc.debug.sendText", "recovered",
                "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/co", row: "/cost")
                // **Exact label, not `contains`.** `text(containing:)` is
                // `CONTAINS[c]` over the whole app, so "Usage" also matches the
                // palette's own "/usage" row and its "Alias of /usage" subtitle
                // sitting behind the sheet — the assertion would pass with the
                // sheet still titled "Cost", which is the one thing this
                // scenario exists to hold.
                try driver.require(
                    app.staticTexts["Usage"].firstMatch, "the sheet titled Usage")
            }),
        RenderScenario(
            name: "session-compact-already-sent",
            purpose: "a replayed mutation: typed once, outcome unknown, said neutrally",
            arguments: [
                "-CC_FIXTURE", "deck", "-cc.debug.sendText", "duplicate",
                "-CC_DEEPLINK", "codeconnect://session/fx-4",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/com", row: "/compact")
                try driver.require(app.buttons["Compact now"], "the always-active button").tap()
                try driver.require(
                    driver.text(containing: "does not confirm its outcome", in: app),
                    "the neutral duplicate line")
            }),

        RenderScenario(
            name: "diff-sample",
            purpose: "the diff grid, every part of it at once",
            arguments: ["-CC_FIXTURE", "deck", "-cc.debug.diff", "sample"],
            reach: { app, driver in
                try driver.openFirstSession(app)
                let diff = app.buttons["open-diff"]
                try driver.require(diff, "the diff button")
                diff.tap()
                try driver.require(
                    driver.text(containing: "Captured on the Mac", in: app),
                    "a rendered diff carrying its capture time")
            }),

        RenderScenario(
            name: "diff-truncated",
            purpose:
                "a capture the daemon cut at 512KB draws itself **and** says it is not all of it",
            arguments: [
                "-CC_FIXTURE", "deck", "-cc.debug.diff", "truncated",
                "-cc.debug.diffRows", "12",
            ],
            reach: { app, driver in
                try driver.openFirstSession(app)
                let diff = app.buttons["open-diff"]
                try driver.require(diff, "the diff button")
                diff.tap()
                try driver.require(
                    driver.text(containing: "Truncated", in: app),
                    "the truncation banner")
            }),

        RenderScenario(
            name: "terminal-ended",
            purpose: "a session that has ended, and the one control it offers",
            arguments: ["-CC_FIXTURE", "deck", "-cc.debug.terminalState", "ended"],
            reach: { app, driver in try driver.openTerminal(app) }),

        RenderScenario(
            name: "settings",
            purpose: "the trust screen: what this daemon advertises, verbatim",
            arguments: ["-CC_FIXTURE", "deck"],
            reach: { app, driver in try driver.openSettings(app) }),

        RenderScenario(
            name: "link-health",
            purpose: "freshness measured, not assumed",
            arguments: ["-CC_FIXTURE", "deck"],
            reach: { app, driver in
                try driver.fleet(app)
                // `firstMatch`: the pill is in the fleet toolbar and in the
                // session detail's, and both stacks can be live.
                let pill = app.buttons["Link health"].firstMatch
                try driver.require(pill, "the freshness pill")
                pill.tap()
                // The sheet is `CCSheetChrome`, so there is no navigation bar to
                // wait on — the measured sentence is the signal.
                try driver.require(
                    driver.element(containing: "daemon last spoke", in: app),
                    "the link-health sheet")
            }),
    ]

    /// The kit's own review surface, one render per page.
    ///
    /// **This is the only reachable render several components have.**
    /// `CCKeyCap`'s other one is a screen that needs a live terminal, so
    /// without these pages "a control is 44pt" is written about it and never
    /// checked. A claim nobody can photograph is a claim nobody has tested.
    /// Each page, and one section heading that only that page draws.
    ///
    /// Waiting on the picker badge would prove only that the *picker* rendered —
    /// and the picker is on every page. The marker is a section header from the
    /// page's own content, so a page that failed to build its body fails the run
    /// instead of photographing an empty column.
    private static let galleryPages: [(page: String, marker: String)] = [
        ("foundations", "Foundations"),
        ("buttons", "CCButton"),
        ("rows", "CCRow"),
        ("indicators", "CCBadge"),
        ("controls", "CCField"),
        ("feedback", "CCBanner"),
        ("terminal", "CCKeyCap"),
        ("diff", "CCDiffFileChip"),
    ]

    static let gallery: [RenderScenario] = galleryPages.map { page, marker in
        RenderScenario(
            name: "gallery-\(page)",
            purpose: "CCGallery · \(page)",
            environment: ["CC_GALLERY_PAGE": page],
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: marker, in: app),
                    "the \(page) gallery page (`\(marker)`)")
            })
    }
}
