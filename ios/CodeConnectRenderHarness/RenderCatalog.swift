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

    /// **Waits for the subject, then brings it ON SCREEN — and fails if it
    /// cannot.**
    ///
    /// This was `waitForExistence` alone, and that cannot fail for an element
    /// scrolled out of the viewport: XCUITest reports a subject that is nowhere
    /// near the screen as existing. So a scenario "passed" while photographing
    /// none of what it exists to show.
    ///
    /// Measured at AX5 on a 402×874pt iPhone 17 Pro, by an independent
    /// reviewer's probe:
    ///
    /// ```
    /// stop-offered    the Stop control    exists=YES hittable=false y=965..1026
    /// stop-link-down  'try again shortly' exists=YES hittable=false y=1275..2008
    /// stop-aborted    the 'Stopped'banner exists=YES hittable=false y=1170..1441
    /// ```
    ///
    /// All three are below an 874pt screen. `codex-stop-link-down--ax5.png` was
    /// sold as *"the daemon's refusal VERBATIM, and a Stop greyed for ten
    /// seconds"* and contained neither. The whole S-series AX5 evidence
    /// certified nothing — which is the one failure an instrument must not have.
    ///
    /// The same three are fully on screen at `L`, so this is AX5-specific and
    /// reproducible, and it is a **harness** defect: the control is one drag
    /// away, and an accessibility user can reach it.
    /// Exists — the question a **navigation** step asks.
    ///
    /// Reaching a screen and photographing a subject are two different claims,
    /// and only the second one needs geometry. Conflating them made a route
    /// step demand that a card's option list be on screen before the scenario
    /// whose subject is the card's *header* could take its picture.
    @discardableResult
    func requireExists(_ element: XCUIElement, _ what: String) throws -> XCUIElement {
        guard element.waitForExistence(timeout: timeout) else {
            throw RenderFailure.unreachable(what)
        }
        return element
    }

    /// - Parameter scrollingWithin: a sibling that is **already on screen and
    ///   inside the same scroll view as the subject**. When given, the drags
    ///   happen inside that element rather than on the app, which is the
    ///   difference between scrolling the list and scrolling nothing at all.
    ///
    ///   Needed because `dragUp` presses at fixed fractions of the *screen*,
    ///   and at AX5 on the smaller phone the software keyboard owns everything
    ///   below ~0.45 — so both gestures landed on the keyboard and the command
    ///   palette's own list never moved. Measured on iPhone 17 Pro, where
    ///   `session-palette`, `session-palette-filtered` and
    ///   `session-snapshot-cost-is-usage` all failed while passing on the Pro
    ///   Max, whose extra height happened to put 0.45 inside the list.
    @discardableResult
    func require(
        _ element: XCUIElement, _ what: String, scrollingWithin anchor: XCUIElement? = nil
    ) throws -> XCUIElement {
        guard element.waitForExistence(timeout: timeout) else {
            throw RenderFailure.unreachable(what)
        }
        // When the subject lives in a list that clips its own content, the
        // window is the wrong frame to ask about. See `isPhotographed(_:in:)`.
        func photographed() -> Bool {
            if let anchor { return Self.isPhotographed(element, in: anchor) }
            return Self.isPhotographed(element)
        }
        guard !photographed() else { return element }
        let app = XCUIApplication()
        // A drag never travels further than the viewport, so nothing can be
        // scrolled past unseen; the list therefore needs more of them than a
        // screen-sized fling does to reach the end of eight AX5 rows.
        for _ in 0..<(anchor == nil ? 12 : 16) {
            let before = element.frame.minY
            if let anchor { dragUp(within: anchor) } else { dragUp(app) }
            if photographed() { return element }
            // **Nothing moved, so the press missed what scrolls.** A sheet at
            // its `.medium` detent owns the bottom half of the screen, and
            // `dragUp` starts at 0.45 — on the dimmed backdrop above it. Four
            // scenarios failed exactly this way at AX5 (`session-model-sheet`,
            // `session-effort-sheet`, `session-compact-sheet`,
            // `session-model-kept`): subject 20 to 600 points below the screen,
            // twelve drags, and not one point of movement.
            if element.frame.minY == before, anchor == nil {
                dragUpInsideSheet(app)
                if photographed() { return element }
            }
        }
        throw RenderFailure.offScreen(what, frame: element.frame)
    }

    /// **Is the subject really in the photograph?**
    ///
    /// Two questions, because a subject can fail to be photographed in two
    /// different ways, and one test cannot catch both:
    ///
    ///   * a **control** must be `isHittable` — K1's own word. Geometry alone
    ///     passed `codex-stop-offered--ax5`, where the Stop sits *inside* the
    ///     window but *underneath* the fleet's "1 decision needs you" bar. A
    ///     control the shutter cannot see and a thumb cannot reach is exactly
    ///     the certificate this gate exists to refuse.
    ///   * anything **else** — a `Text` in a banner, a section header — is never
    ///     hittable however plainly it is drawn, so for those the question is
    ///     the geometry below. Demanding hittability of text is what regressed
    ///     thirteen good Claude sheet scenarios on the first attempt.
    private static func isPhotographed(_ element: XCUIElement) -> Bool {
        // **A control: hittable, and the point you would touch is on screen.**
        //
        // `isHittable` alone passed `codex-stop-offered--ax5`, where the PNG
        // showed no Stop pill at all. Demanding the whole control fit was the
        // other extreme and wrong for a different reason: at AX5 a fleet *row*
        // is a button taller than the screen, so "wholly" can never hold and
        // three A-series scenarios failed on their way in.
        //
        // What a photograph of a control means is that the thing you would
        // press is in it. So: hittable, and its hit point inside the window.
        if element.elementType == .button {
            guard element.isHittable else { return false }
            let window = XCUIApplication().frame
            let frame = element.frame
            guard !window.isEmpty, !frame.isEmpty else { return false }
            return window.contains(CGPoint(x: frame.midX, y: frame.midY))
        }
        return isInFrame(element)
    }

    /// **Is the subject in the photograph, when the photograph is a list?**
    ///
    /// The window is the wrong frame to ask about whenever the subject sits in
    /// a scroll view that clips its own content. Measured on iPhone 17 Pro at
    /// AX5: the command palette's list is about 120pt tall and the compose bar
    /// and keyboard own everything below it, so the discovery caption at
    /// `y=350` is inside the *window* and behind the *composer*. Both devices
    /// certified `session-palette--ax5` that way and neither PNG has the
    /// caption in it — a green render of a screen nobody photographed.
    ///
    /// So: inside the list's own frame, and enough of it to be worth the
    /// certificate — the whole subject when it fits, and a full viewport of it
    /// when the subject is taller than the list (the AX5 caption is).
    private static func isPhotographed(_ element: XCUIElement, in viewport: XCUIElement) -> Bool {
        let frame = element.frame
        let box = viewport.frame
        guard !frame.isEmpty, !box.isEmpty else { return false }
        let shown = frame.intersection(box)
        guard !shown.isNull, !shown.isEmpty else { return false }
        return shown.height >= min(frame.height, box.height) - 1
            && shown.width >= min(frame.width, box.width) - 1
    }

    /// **Is this element in the photograph?**
    ///
    /// Not `isHittable`, which was the first attempt and is the wrong question
    /// twice over: a `Text` inside a banner is never hittable however plainly it
    /// is on screen, and that regressed thirteen perfectly good Claude sheet
    /// scenarios at AX5. What a render certifies is what the shutter caught, so
    /// the test is the geometry: the subject overlaps the window, and its top
    /// edge is inside it.
    ///
    /// The second clause is what catches the S-series. A subject at
    /// `y=965..1026` on an 874pt screen overlaps nothing and fails; a long
    /// banner running from `y=400` off the bottom starts on screen and passes,
    /// because it is in the picture even though its tail is not.
    private static func isInFrame(_ element: XCUIElement) -> Bool {
        let window = XCUIApplication().frame
        let frame = element.frame
        guard !window.isEmpty, !frame.isEmpty else { return false }
        return window.intersects(frame) && frame.minY >= window.minY - 1
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

    /// **The scroll view that holds this text**, so a drag can be aimed at the
    /// list rather than at whatever happens to be under a screen fraction.
    ///
    /// Anchoring on a *row* was the first attempt and is subtly wrong: the row
    /// scrolls away, and every drag after the first one presses where it used
    /// to be. Measured — the palette's caption climbed 326pt over twelve drags
    /// and then stopped, because eleven of them landed on nothing. A scroll
    /// view stays where it is while its content moves under it.
    func list(holding fragment: String, in app: XCUIApplication) -> XCUIElement {
        app.scrollViews.containing(
            NSPredicate(format: "label CONTAINS[c] %@", fragment)
        ).firstMatch
    }

    /// **A drag that lands inside a named element**, and therefore inside the
    /// scroll view that holds it.
    ///
    /// One row's worth of travel per call rather than a screen's: a command
    /// palette shows about four rows, and a fling that overshoots the subject
    /// is the same failure as never reaching it.
    func dragUp(within anchor: XCUIElement) {
        let start = anchor.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        // Never more than the viewport, or a row can pass the shutter between
        // two drags and the loop will keep dragging past it forever.
        let travel = max(anchor.frame.height * 0.8, 40)
        start.press(
            forDuration: 0.05,
            thenDragTo: start.withOffset(CGVector(dx: 0, dy: -travel)))
    }

    /// The same gesture, begun **low enough to be inside a half-height sheet**.
    ///
    /// Kept separate from `dragUp` rather than replacing it: every route in this
    /// catalogue is calibrated against that gesture, and this one is only
    /// reached when a drag demonstrably moved nothing.
    func dragUpInsideSheet(_ app: XCUIApplication) {
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.80))
            .press(
                forDuration: 0.05,
                thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.30)))
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
        // **Scrolled within the palette's own list.** At AX5 on the smaller
        // phone the keyboard leaves room for about one row, so the wanted row
        // is usually below it — and a drag aimed at the screen lands on the
        // keyboard. The palette's first row is on screen by construction (the
        // fragment filtered to it), and it shares the list's scroll view.
        try require(
            target, "the \(row) palette row", scrollingWithin: list(holding: row, in: app))
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
    /// The subject exists and could not be brought into the viewport. Its own
    /// case, and it carries the frame: "could not reach X" and "X is at
    /// y=1275..2008 on an 874pt screen" send the next reader to different
    /// places, and only the second is true when the render is a lie.
    case offScreen(String, frame: CGRect)
    /// The process resolved to a different content-size category than the one
    /// the pass claims to be rendering. See `CCRenderProbe` in `RootView.swift`.
    case wrongTypeSize(expected: String, actual: String)

    var description: String {
        switch self {
        case .unreachable(let what):
            return "could not reach \(what)"
        case .offScreen(let what, let frame):
            return
                "\(what) exists but never came on screen — last seen at "
                + "y=\(Int(frame.minY))..\(Int(frame.maxY)); the render would have certified nothing"
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
    static let all: [RenderScenario] = product + codex + gallery

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
                // Scrolled by its own list, which stays put while its rows move.
                try driver.require(
                    driver.text(containing: "Run other commands", in: app),
                    "the discovery caption",
                    scrollingWithin: driver.list(holding: "Run other commands", in: app))
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
                    driver.element(containing: "/cost", in: app), "the /cost row",
                    scrollingWithin: driver.list(holding: "/cost", in: app))
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

        // **The relay push states, folded into the one test button.** A relay
        // enrollment cannot happen in a render pass — no App Attest hardware, no
        // live relay — so `-CC_PUSH_STATE` stages each one. The reset action is
        // the anchor present in every relay state; the state's own sentence is
        // the disabled-button reason, which the kit draws as visible warning
        // text. AX5 is where a two-clause reason wraps, so both sizes matter.
        RenderScenario(
            name: "push-relay-enrolling",
            purpose: "the test button while the relay credential is being minted",
            arguments: ["-CC_FIXTURE", "deck", "-CC_PUSH_STATE", "enrolling"],
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(app.buttons["Link health"].firstMatch, "the freshness pill").tap()
                // `isHittable`, not `exists`: the reason is below the fold at AX5
                // and `exists` is true off-screen, which would photograph the top
                // of the sheet instead of the state this scenario is here for.
                try driver.scrollUntil(app, "the enrolling reason on screen") {
                    driver.element(containing: "Enrolling this iPhone", in: app).isHittable
                }
            }),
        RenderScenario(
            name: "push-relay-failed",
            purpose: "enrollment failed, said plainly, with the reset action offered",
            arguments: ["-CC_FIXTURE", "deck", "-CC_PUSH_STATE", "failed"],
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(app.buttons["Link health"].firstMatch, "the freshness pill").tap()
                try driver.scrollUntil(app, "the failure reason on screen") {
                    driver.element(containing: "could not be reached", in: app).isHittable
                }
                try driver.require(
                    app.buttons["Reset notification registration"].firstMatch,
                    "the reset action")
            }),
        RenderScenario(
            name: "push-relay-unsupported",
            purpose: "an iPhone with no App Attest: the honest no-relay message, everything else kept",
            arguments: ["-CC_FIXTURE", "deck", "-CC_PUSH_STATE", "unsupported"],
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(app.buttons["Link health"].firstMatch, "the freshness pill").tap()
                try driver.scrollUntil(app, "the unsupported message on screen") {
                    driver.element(containing: "App Attest", in: app).isHittable
                }
            }),
        RenderScenario(
            name: "push-relay-bootstrap",
            purpose: "a relay daemon over a bootstrap connection: pairing required, no dead reset",
            arguments: ["-CC_FIXTURE", "deck", "-CC_PUSH_STATE", "bootstrap"],
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(app.buttons["Link health"].firstMatch, "the freshness pill").tap()
                try driver.scrollUntil(app, "the pairing-required message on screen") {
                    driver.element(containing: "bootstrap connection", in: app).isHittable
                }
            }),
    ]

    // =========================================================================
    //  Codex — one scenario per frame of the step-1 mock.
    //
    //  Every one is seeded from `fixtures/codex/*` through `-CC_CODEX <state>`,
    //  which stages the session's history, its daemon's age and what the daemon
    //  answers a mutation with. The mutation states are *pressed*, not painted:
    //  the fixture drives the real send path, so what these photograph is what
    //  a tap produces.
    //
    //  **The worst-case data is not optional here.** Each card carries the
    //  measured 75-character command, the daemon's own 120-character amendment
    //  label (four lines at reading size, and the widest thing any Codex card
    //  draws), and the real 169-character `request_id` — whose whole job in a
    //  fixture is to prove that nothing renders it. AX5 is where these break.
    // =========================================================================

    /// **The session key every Codex fixture uses**, and the state names.
    ///
    /// Spelled as strings rather than shared with `CodexFixtures.State`, because
    /// this target drives the app from *outside* — it is an XCUITest runner with
    /// no `@testable import`, which is the whole point of it: what it
    /// photographs is what a user could reach, not what a test could construct.
    ///
    /// The cost is two lists that must agree, so `CodexFixtureCatalogTests`
    /// (in the unit target, which *can* see both) pins them against each other.
    /// A state added on one side and not the other fails there rather than
    /// producing a render nobody notices is missing.
    static let codexSessionKey = "cx-1"

    /// One scenario, with the boilerplate every Codex render shares.
    private static func codexScenario(
        _ state: String,
        purpose: String,
        route: String = "fleet",
        reach: @escaping (XCUIApplication, RenderDriver) throws -> Void
    ) -> RenderScenario {
        var arguments = ["-CC_CODEX", state]
        if route != "fleet" {
            arguments += ["-CC_DEEPLINK", "codeconnect://session/\(codexSessionKey)"]
        }
        return RenderScenario(
            name: "codex-\(state)", purpose: purpose, arguments: arguments, reach: reach)
    }

    /// Brings a control the tail-follow pill may be sitting on top of into
    /// reach, and taps it.
    ///
    /// **The occlusion is intermittent, which is why this is a loop and not a
    /// one-shot.** At AX5 the timeline's `Latest` pill is drawn over the
    /// trailing edge of a decision card's footer — exactly where `Review` and
    /// `View` sit — and whether it is up at the moment the harness looks depends
    /// on where the scroll happened to settle. Across four passes a *different*
    /// single scenario failed each time, which is the signature. Dismissing it
    /// once before scrolling fixed most runs and not all; dismissing it whenever
    /// it reappears fixes the route.
    ///
    /// The occlusion itself is a **product defect in shared tail-follow
    /// chrome** — a decision card's primary control covered by an overlay at
    /// accessibility sizes. It is not Codex's, this phase does not fix it, and
    /// it is reported.
    private static func tapPastTheTailPill(
        _ app: XCUIApplication, _ driver: RenderDriver, _ control: XCUIElement, _ what: String
    ) throws {
        for _ in 0..<12 {
            let latest = app.buttons.matching(
                NSPredicate(format: "label CONTAINS[c] 'Latest'")).firstMatch
            if latest.exists, latest.isHittable { latest.tap() }
            if control.isHittable { control.tap(); return }
            driver.dragUp(app)
        }
        guard control.isHittable else { throw RenderFailure.unreachable(what) }
        control.tap()
    }

    /// Opens a **resolved** card, through the row's own `View` button.
    ///
    /// The ending's banner lives on the card, not on the timeline: the row
    /// carries one word (`RESOLVED` / `RETIRED`) and the card carries the
    /// sentence. Reaching it through the button is also the assertion that
    /// matters — a resolved card is still *readable*, it is only no longer
    /// answerable.
    private static func openResolvedCard(
        _ app: XCUIApplication, _ driver: RenderDriver
    ) throws {
        // **Deep-linked to the run by name, never `openFirstSession`.**
        //
        // `openFirstSession` taps whichever row sorts first, and the Codex run
        // does not always sort first: once its card is resolved AND its turn has
        // ended it is an ordinary Idle row beside the Claude neighbour, and the
        // fleet's ordering can put either ahead. Measured — three scenarios
        // (`cleared-turn-aborted`, `cleared-turn-completed`,
        // `retired-item-completed`) opened the *Claude* session, found no card,
        // and then scrolled twelve times looking for a `View` button that was
        // never going to be there. The twelfth drag opened the diff sheet, which
        // is what the FAILED screenshot showed.
        //
        // These scenarios carry `-CC_DEEPLINK codeconnect://session/cx-1`, so
        // the session is already open; there is nothing to navigate.
        let view = app.buttons["View"].firstMatch
        try driver.require(view, "the resolved card's View button")
        try tapPastTheTailPill(app, driver, view, "the View button on screen")
        try driver.require(driver.text(containing: "Decision", in: app), "the decision sheet")
    }

    /// Opens the staged card as a sheet, the way it really ships.
    ///
    /// **Through the card's own Review button, not the `?request=` deep link.**
    /// That route is spent on a cold launch — consumed once, before the approval
    /// has arrived, and never retried — so it opens the card at `L` and not at
    /// AX5. That is the `deeplink-request-race` defect, it is filed and NOT
    /// fixed here, and a scenario built on it would be a scenario that renders
    /// at one size only.
    /// Opens the staged card, **wherever it legitimately lands**.
    ///
    /// The card has two homes — the per-session sheet and the Deck — and it is
    /// the same `DecisionCardView` in both. Asserting on the *sheet's* chrome
    /// therefore fails on a perfectly good render: measured on
    /// `codex-card-two-options--ax5--FAILED.png`, which photographed the card
    /// open, correct, and answerable in the Deck while the scenario went on
    /// hunting for a `Review` button that had already done its job.
    ///
    /// So the marker is the **card's own** — the section header over Codex's
    /// option list, which is the thing every A-series scenario exists to show.
    private static func openCard(_ app: XCUIApplication, _ driver: RenderDriver) throws {
        let cardIsUp = { driver.element(containing: "Choose one", in: app).exists }
        if !cardIsUp() {
            try driver.openFirstSession(app)
        }
        if !cardIsUp() {
            let review = app.buttons["Review"].firstMatch
            if review.waitForExistence(timeout: 10) {
                try tapPastTheTailPill(app, driver, review, "the Review button on screen")
            }
        }
        // **Existence, not in-frame.** This is the navigation gate: its job is
        // to prove the card is *open*. Which part of it the shutter must catch
        // is the scenario's own business, and every A-series scenario says so
        // on the next line with `require`. Demanding the option list be in
        // frame here failed `card-ceiling`, whose subject is the fold at the
        // top of a card whose options are five screens below it.
        try driver.requireExists(
            driver.element(containing: "Choose one", in: app),
            "the card and its option list, in whichever home it opened")
    }

    static let codex: [RenderScenario] = [
        // ---- A-series: the card ----------------------------------------
        codexScenario(
            "card-command-worst",
            purpose:
                "the widest card the corpus can produce: 75-char command, 120-char amendment label",
            reach: { app, driver in
                try openCard(app, driver)
                try driver.require(
                    driver.text(containing: "Choose one", in: app), "Codex's own option list")
            }),
        codexScenario(
            "card-two-options",
            purpose:
                "two options — the shape that was UNANSWERABLE before this phase (Claude's >2 rule)",
            reach: { app, driver in
                try openCard(app, driver)
                try driver.require(
                    driver.element(containing: "Yes, proceed", in: app), "the first option row")
                // The card must offer NO Allow and NO Deny: both are refused by
                // name at the Mac, so a bar carrying them would be two controls
                // whose only behaviour is a refusal.
                XCTAssertFalse(
                    app.buttons["Allow"].exists, "a Codex card must not offer Allow")
            }),
        codexScenario(
            "card-filechange-wide",
            purpose: "the inline patch: three files, and 33 more the Mac could not send",
            reach: { app, driver in
                try openCard(app, driver)
                try driver.require(
                    driver.text(containing: "are not shown here", in: app),
                    "the changes_omitted line (CONSTRUCTED shape — no capture exercises it)")
            }),
        // **The ceiling: 32 files, ~128 KiB.** The largest card the Mac's own
        // bounds allow. Rendered because "a valid card can make the decision
        // surface unresponsive" is not a claim any three-hunk fixture can test.
        codexScenario(
            "card-ceiling",
            purpose: "32 files and ~128 KiB of diff — the largest card the daemon may send",
            reach: { app, driver in
                try openCard(app, driver)
                try driver.require(
                    driver.text(containing: "32 files", in: app), "the whole-card file count")
                try driver.require(
                    app.buttons["show-all-changes"], "the fold, which keeps the card bounded")
            }),
        codexScenario(
            "card-minimal",
            purpose: "every optional field absent — the measured file-change shape",
            reach: { app, driver in
                try openCard(app, driver)
                try driver.require(
                    driver.text(containing: "1 file", in: app), "the single-file header")
            }),

        // ---- R-series: how a card ends ---------------------------------
        codexScenario(
            "resolved-accepted", purpose: "answered from this phone, on the Codex link",
            route: "session",
            reach: { app, driver in
                try openResolvedCard(app, driver)
                try driver.require(
                    driver.element(containing: "Answered from this phone", in: app),
                    "the ending's own sentence, on the card")
            }),
        codexScenario(
            "resolved-declined", purpose: "declined from this phone — Codex was told no",
            route: "session",
            reach: { app, driver in
                try openResolvedCard(app, driver)
                try driver.require(
                    driver.element(containing: "Answered from this phone", in: app),
                    "the ending's own sentence, on the card")
            }),
        codexScenario(
            "resolved-at-mac",
            purpose:
                "answered at the keyboard, with NO decision — the wire carries no provenance for one",
            route: "session",
            reach: { app, driver in
                try openResolvedCard(app, driver)
                try driver.require(
                    driver.element(containing: "Answered at the Mac", in: app),
                    "the ending's own sentence, on the card")
            }),
        codexScenario(
            "cleared-turn-aborted", purpose: "the turn was stopped and the question went with it",
            route: "session",
            reach: { app, driver in
                try openResolvedCard(app, driver)
                try driver.require(
                    driver.element(containing: "The turn was stopped", in: app),
                    "the ending's own sentence, on the card")
            }),
        codexScenario(
            "cleared-turn-completed", purpose: "Codex finished before this was answered",
            route: "session",
            reach: { app, driver in
                try openResolvedCard(app, driver)
                try driver.require(
                    driver.element(containing: "The turn ended", in: app),
                    "the ending's own sentence, on the card")
            }),
        codexScenario(
            "retired-item-completed",
            purpose: "item_completed — the turn is STILL RUNNING, said in its own words",
            route: "session",
            reach: { app, driver in
                try openResolvedCard(app, driver)
                try driver.require(
                    driver.element(containing: "This step finished", in: app),
                    "the ending's own sentence, on the card")
            }),
        codexScenario(
            "timeout-unknown", purpose: "Codex stopped waiting; nothing was approved or denied",
            route: "session",
            reach: { app, driver in
                try openResolvedCard(app, driver)
                try driver.require(
                    driver.element(containing: "No answer arrived in time", in: app),
                    "the ending's own sentence, on the card")
            }),
        codexScenario(
            "write-unknown", purpose: "written, outcome unknown — never retried, check the Mac",
            route: "session",
            reach: { app, driver in
                try openResolvedCard(app, driver)
                try driver.require(
                    driver.element(containing: "Sent, outcome unknown", in: app),
                    "the ending's own sentence, on the card")
            }),

        // ---- S-series: Stop --------------------------------------------
        codexScenario(
            "stop-offered",
            purpose: "the Stop pill on a running Codex row, in line with its Claude neighbour",
            reach: { app, driver in
                try driver.fleet(app)
                // **Two subjects, because the button alone is not the picture.**
                //
                // The control has to be reachable — `require` on the button is
                // hittability plus its hit point on screen. But at AX5 the row
                // is two screens tall and XCUI answered "hittable, hit point in
                // window" for a pill that was nowhere in the PNG: whatever frame
                // it reports for that button is not where the pill is drawn.
                // The pill's own LABEL is a `Text`, so it goes through the
                // geometry rule, which is measured against what the shutter
                // catches. Requiring both is what makes this render certify the
                // thing it is named after.
                try driver.require(
                    app.buttons["stop-\(codexSessionKey)"], "the Stop control")
                // `staticTexts["Stop"]` is the pill's own title — `CCButton`
                // draws it as a `Text`, so this element's frame is where the
                // words are, not where some ancestor claims to be. Matching by
                // *containment* found a container again (the row carries the
                // accessibility label "Stop the turn … is running"), which is
                // how the last two attempts passed on a PNG with no pill in it.
                try driver.require(app.staticTexts["Stop"], "the Stop pill's own label")
            }),
        codexScenario(
            "stop-aborted", purpose: "the turn reached its aborted boundary",
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(
                    driver.element(containing: "Stopped", in: app), "the stopped banner")
            }),
        // **The gate, photographed.** `codex_link` is `offline`, so F1's send
        // path refuses before anything leaves: what is on screen is the app's
        // own sentence over "Nothing was sent", and the Mac has said nothing
        // because it was never asked.
        codexScenario(
            "stop-link-down",
            purpose: "the phone's own refusal: an offline link, so no frame ever left",
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(
                    driver.element(containing: "Nothing was sent", in: app),
                    "the app's own words, not the daemon's")
                try driver.require(
                    driver.element(containing: "lost its link", in: app),
                    "why the phone would not send")
            }),
        // **The refusal after the fact**, which is a different screen and a
        // different author. The summary said `subscribed`, the frame left, and
        // the Mac answered that its own link had gone down in between.
        codexScenario(
            "stop-refused-late",
            purpose:
                "the daemon's refusal VERBATIM, and a Stop greyed for ten seconds — not for ever",
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(
                    driver.element(containing: "try again shortly", in: app),
                    "the daemon's own sentence, unedited")
            }),
        // **The session route**, so the header's own Stop control is
        // photographed too. The other three S-series scenarios drive the fleet
        // row; this one drives the same mutation from the screen a reader is on
        // when they are watching the turn they want to end, and the two draw the
        // control differently — a pill beside the activity line there, a
        // full-width button with its outcome beneath it here.
        codexScenario(
            "stop-indeterminate",
            purpose: "issued, outcome unknown — NO retry offered, on the session's own Stop",
            route: "session",
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "Sent, outcome unknown", in: app),
                    "the indeterminate banner under the session's Stop")
            }),

        // ---- C-series: Compose -----------------------------------------
        codexScenario(
            "compose-started", purpose: "Codex was idle: your words began the turn it is running",
            route: "session",
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "started a new turn", in: app),
                    "the started sentence")
            }),
        codexScenario(
            "compose-steered", purpose: "Codex was working: your words joined the running turn",
            route: "session",
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "joined the running turn", in: app),
                    "the steered sentence — never the same as started")
            }),
        codexScenario(
            "compose-duplicate",
            purpose: "a replay reading with the verb the ORIGINAL earned (started:false ⇒ steered)",
            route: "session",
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "second time", in: app),
                    "the nothing-was-said-twice sentence")
            }),
        codexScenario(
            "compose-rejected", purpose: "nothing was sent, in the daemon's own words",
            route: "session",
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "not yet watching its thread", in: app),
                    "the refusal, verbatim")
            }),
        codexScenario(
            "compose-indeterminate",
            purpose: "written, outcome unknown — the draft is KEPT, and nothing is resent",
            route: "session",
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "Sent, outcome unknown", in: app),
                    "the indeterminate sentence")
            }),

        // ---- M-series: an older Mac ------------------------------------
        //
        // **First-open states**, and the memory warns exactly about these: a
        // disabled primary with a visible reason reads as scolding if nobody
        // looks at it on first open. Both are looked at here, at both sizes.
        codexScenario(
            "daemon-minor17",
            purpose: "a Mac that can stop a Codex turn but cannot carry a message to one",
            route: "session",
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "too old to carry a message", in: app),
                    "the composer's reason, on first open")
            }),
        codexScenario(
            "daemon-minor16",
            purpose: "a Mac that can do neither — a dead composer WITH a sentence",
            route: "session",
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "too old", in: app),
                    "the composer's reason, on first open")
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
