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
            name: "terminal-setup",
            purpose: "the app never enables a system service; it says what to run",
            arguments: ["-CC_FIXTURE", "deck", "-cc.debug.terminalState", "needsSetup"],
            reach: { app, driver in
                try driver.openTerminal(app)
                try driver.require(
                    driver.text(containing: "Tailscale SSH", in: app), "the SSH setup card")
            }),

        RenderScenario(
            name: "terminal-hostkey-changed",
            purpose:
                "the app's most serious screen: two fingerprints, diffed, and a hard stop",
            arguments: ["-CC_FIXTURE", "deck", "-cc.debug.terminalState", "hostKeyChanged"],
            reach: { app, driver in
                try driver.openTerminal(app)
                try driver.require(
                    driver.text(containing: "SSH key changed", in: app),
                    "the changed-host-key alarm")
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
            name: "settings-terminal-ssh",
            purpose:
                "this iPhone's own key, in the `CCMonoBlock` whose text lands on the content column",
            arguments: ["-CC_FIXTURE", "deck"],
            reach: { app, driver in
                try driver.openSettings(app)
                let link = app.buttons.matching(
                    NSPredicate(format: "label CONTAINS 'Terminal and SSH'")
                ).firstMatch
                try driver.require(link, "the Terminal and SSH row")
                // `isHittable`, not `exists`: the row sits below the connection
                // and transport sections and is in the tree long before it is
                // under a thumb.
                try driver.scrollUntil(app, "the Terminal and SSH row is reachable") {
                    link.isHittable
                }
                link.tap()
                try driver.require(
                    app.navigationBars["Terminal and SSH"], "the SSH identity screen")

                // **Mint the key.** Without this the screen renders its
                // *not created yet* branch, and the `CCMonoBlock` this scenario
                // exists to photograph — the public key, `.middle`-truncated,
                // whose text lands on the content column and whose border
                // hangs 12pt left of it — is never drawn. A render of the empty
                // branch would have looked like a pass.
                let create = app.buttons.matching(
                    NSPredicate(format: "label CONTAINS 'Create this iPhone'")
                ).firstMatch
                if create.waitForExistence(timeout: 5) { create.tap() }
                try driver.require(
                    app.buttons.matching(
                        NSPredicate(format: "label CONTAINS 'Copy public key'")
                    ).firstMatch,
                    "this iPhone's public key")
            }),

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
    /// **This is the only reachable render several components have.** `CCKeyCap`
    /// had none at all — no gallery page, and a screen that needs a live SSH
    /// server — so "a control is 44pt" could be written about it and never
    /// checked. A claim nobody can photograph is a claim nobody has tested.
    /// Each page, and one section heading that only that page draws.
    ///
    /// Waiting on the picker badge would prove only that the *picker* rendered —
    /// and the picker is on every page. The marker is a section header from the
    /// page's own content, so a page that failed to build its body fails the run
    /// instead of photographing an empty column.
    private static let galleryPages: [(page: String, marker: String)] = [
        ("foundations", "Foundations"),
        ("identity", "CCIdentity"),
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
