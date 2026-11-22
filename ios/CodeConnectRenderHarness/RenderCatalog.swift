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
    /// For a scenario whose fixture waits on the test (`FixtureSignals`): armed at each launch, sent from `reach`.
    var signals: FixtureSignals? = nil
    /// How long to allow for the state to arrive. Generous only where the state
    /// itself takes time (`stale` crosses `LinkHealth.staleAfter` at 30s).
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
            // `dragUp` starts at 0.45 — on the dimmed backdrop above it. Sheet
            // scenarios failed exactly this way at AX5 (`session-compact-sheet`
            // among them): subject 20 to 600 points below the screen, twelve
            // drags, and not one point of movement.
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
    ///   * a **control** must be `isHittable`. Geometry alone passed
    ///     `codex-stop-offered--ax5`, where the Stop sits *inside* the
    ///     window but *underneath* the fleet's "1 waiting for you" bar. A
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
    ///
    /// Either kind of scroller: the palette is a scroll view, and the session
    /// timeline is a `List`, which the accessibility tree calls a collection
    /// view.
    func list(holding fragment: String, in app: XCUIApplication) -> XCUIElement {
        app.descendants(matching: .any).matching(
            NSPredicate(
                format: "elementType == %lu OR elementType == %lu",
                XCUIElement.ElementType.scrollView.rawValue,
                XCUIElement.ElementType.collectionView.rawValue)
        ).containing(
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

    /// **Back up the list**, for a subject the screen has already scrolled past.
    ///
    /// Every other route here drags *forward*, because every other subject is
    /// at or below the tail a timeline follows. A conversation's opening lines
    /// are not: they are above the viewport, and in a lazy list they are not
    /// merely off screen — they are **not in the accessibility tree at all**,
    /// so `require` throws `unreachable` before it ever drags. Hence the
    /// existence check inside the loop rather than a `waitForExistence` in
    /// front of it.
    /// - Parameter inside: the list's own frame, measured by the caller
    ///   **before any scrolling**. Screen
    ///   fractions were tried first and are wrong here for the same reason they
    ///   were wrong for the command palette: at AX5 a fixed 0.35 of the screen
    ///   lands on a decision card's own controls and 0.70 on the composer, and
    ///   sixteen drags moved the timeline by nothing
    ///   (`codex-phone-turn-stream--ax5`, run 20260908-204539).
    ///
    ///   A `CGRect` rather than the element, because the query that finds the
    ///   list names a row inside it — and by the second call that row has
    ///   scrolled out of a lazy list, so the query resolves to nothing and the
    ///   drags go back to guessing (run 20260908-211915, where the first
    ///   subject was reached and the second was not).
    @discardableResult
    func requireEarlier(
        _ element: XCUIElement, _ what: String, inside box: CGRect
    ) throws -> XCUIElement {
        for _ in 0..<16 {
            if element.exists, Self.isPhotographed(element) { return element }
            dragDown(inside: box)
        }
        guard element.exists else { throw RenderFailure.unreachable(what) }
        throw RenderFailure.offScreen(what, frame: element.frame)
    }

    /// The mirror of `dragUp`: begun a quarter of the way into the list and
    /// travelling **down**, which moves the content down and brings earlier
    /// rows into view. Both ends stay inside `box`, so the gesture belongs to
    /// the list from touch-down to lift.
    func dragDown(inside box: CGRect) {
        let origin = XCUIApplication().coordinate(withNormalizedOffset: .zero)
        let start = origin.withOffset(
            CGVector(dx: box.midX, dy: box.minY + box.height * 0.2))
        let travel = max(box.height * 0.6, 40)
        start.press(
            forDuration: 0.05, thenDragTo: start.withOffset(CGVector(dx: 0, dy: travel)))
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
        try require(app.navigationBars["Projects"], "the Fleet root")
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
        try require(app.navigationBars["Waiting for you"], "the Deck")
    }

    func openSettings(_ app: XCUIApplication) throws {
        try fleet(app)
        let settings = app.buttons["Settings"]
        try require(settings, "the Settings toolbar button")
        settings.tap()
        try require(app.navigationBars["Settings"], "the Settings screen")
    }

    /// Link health, under the computer in the session bar's menu. A fixture's
    /// computer has no screen of its own in Settings to open it from.
    func openLinkHealth(_ app: XCUIApplication) throws {
        try openFirstSession(app)
        try openSessionMenu(app)
        let link = app.buttons["Link health"].firstMatch
        try require(link, "Link health in the session bar's menu")
        link.tap()
        // The sheet is `CCSheetChrome`, so there is no navigation bar to wait
        // on — the measured sentence is the signal.
        try require(element(containing: "your computer last answered", in: app), "the link-health sheet")
    }

    /// The session bar's "…" menu.
    func openSessionMenu(_ app: XCUIApplication) throws {
        let more = app.buttons["session-more"].firstMatch
        try require(more, "the session bar's menu")
        more.tap()
    }

    /// The session's changes, from the bar's menu.
    func openChanges(_ app: XCUIApplication) throws {
        try openSessionMenu(app)
        let changes = app.buttons.matching(NSPredicate(format: "label == 'Changes'")).firstMatch
        try require(changes, "the changes in the session bar's menu")
        changes.tap()
    }

    /// Focuses the composer and types. A vertical-axis TextField is backed
    /// by a text view, not a text field — the same fallback the product
    /// suites use. Navigates only if the Fleet is still on screen, so a
    /// scenario that arrived by deep link is not sent back to the root.
    func typeIntoComposer(_ app: XCUIApplication, _ text: String) throws {
        if app.navigationBars["Projects"].exists { try openFirstSession(app) }
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
    /// session, the diff, the trust screen, then the kit's own
    /// gallery — which is the only reachable render several components have.
    static let all: [RenderScenario] = product + codex + questions + gallery

    /// A pairing link as `codeconnect pair` prints it (`fixtures/wire/qr-link.txt`),
    /// naming a computer with a long name on a port other than the default.
    static func pairingLink(version: Int) -> String {
        "https://codeconnect.sh/p#v=\(version)&h=jordans-macbook-pro-16-inch-m4-max.example.ts.net&p=18794&c=ABCD2345"
    }

    static let product: [RenderScenario] = [
        // The first screen, launched unpaired with no arguments, exactly as the
        // App Store build opens. Taller than a phone at any type size, so it is
        // photographed three times: the top, the steps, and the way to pair.
        RenderScenario(
            name: "onboarding",
            purpose: "the first screen a stranger sees: what this is, and the way in with no Mac",
            arguments: [],
            reach: { app, driver in
                try driver.require(app.buttons["pairing-sample-mode"], "the first screen")
            }),

        RenderScenario(
            name: "onboarding-steps",
            purpose: "the setup steps, from installing on the Mac onwards",
            arguments: [],
            reach: { app, driver in
                try driver.require(app.buttons["pairing-sample-mode"], "the first screen")
                try driver.scrollUntil(app, "the network step on screen") {
                    driver.element(containing: "reach your computer", in: app).isHittable
                }
            }),

        RenderScenario(
            name: "onboarding-pair",
            purpose: "the end of the steps and the button that pairs",
            arguments: [],
            reach: { app, driver in
                try driver.require(app.buttons["pairing-sample-mode"], "the first screen")
                try driver.scrollUntil(app, "the pairing button on screen") {
                    app.buttons["Pair by hand"].isHittable
                        || app.buttons["Scan pairing code"].isHittable
                }
            }),

        // A pairing link opened from outside the app, with the longest computer
        // name a tailnet gives and a port other than the default, so the address
        // wraps where it must. The host resolves nowhere, which keeps a tapped
        // Pair pending: a refused code needs a daemon, so this catalogue cannot
        // reach the failed state.
        RenderScenario(
            name: "pairing-link",
            purpose: "a pairing link opened from outside: the computer it names, and one tap to pair",
            arguments: ["-CC_OPEN_LINK", RenderCatalog.pairingLink(version: 1)],
            reach: { app, driver in
                try driver.require(app.buttons["Pair"], "the pairing link's Pair button")
            }),

        RenderScenario(
            name: "pairing-link-pairing",
            purpose: "Pair tapped: the code being exchanged",
            arguments: ["-CC_OPEN_LINK", RenderCatalog.pairingLink(version: 1)],
            reach: { app, driver in
                try driver.require(app.buttons["Pair"], "the pairing link's Pair button").tap()
                try driver.require(
                    app.buttons.matching(NSPredicate(format: "label == 'Pair' AND value == 'Busy'"))
                        .firstMatch,
                    "Pair, busy while it pairs")
            }),

        RenderScenario(
            name: "pairing-link-unreadable",
            purpose: "a link from a newer computer: what to update, and nothing to tap",
            arguments: ["-CC_OPEN_LINK", RenderCatalog.pairingLink(version: 2)],
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "Could not read that code", in: app),
                    "the unreadable link's message")
            }),

        RenderScenario(
            name: "fleet",
            purpose: "the 4-second surface, with one agent holding two decisions",
            arguments: ["-CC_FIXTURE", "stacked"],
            reach: { app, driver in try driver.fleet(app) }),

        RenderScenario(
            name: "fleet-single",
            purpose: "one request waiting: the bar's ordinary case",
            arguments: ["-CC_FIXTURE", "single"],
            reach: { app, driver in try driver.require(app.buttons["deck-bar"], "the Deck bar") }),

        RenderScenario(
            name: "fleet-empty",
            purpose: "paired with no session: how to start an agent, and where",
            arguments: ["-CC_FIXTURE", "empty"],
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(
                    driver.element(containing: "Start an agent", in: app),
                    "the empty fleet's instruction")
            }),

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
            // `LinkHealth.staleAfter` is 30s and the fixture withholds its
            // keep-alive `pong`, so the state arrives on its own. There is no
            // shorter way in, and without it the state is unaudited — which is
            // how the accessory bar shipped with its blocked reason cut
            // mid-word at `…but the daem…` and dimmed to 3.16:1.
            timeout: 90,
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(
                    driver.element(containing: "since your computer last answered", in: app),
                    "the stale-link banner")
            }),

        RenderScenario(
            name: "fleet-refused-version",
            purpose: "a computer that refused the link for a different version: the card's mark and the banner say so",
            arguments: ["-CC_FIXTURE", "stacked", "-CC_FIXTURE_LINK", "refused-version"],
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "Different version", in: app), "the different-version mark")
            }),

        RenderScenario(
            name: "fleet-refused-pairing",
            purpose: "a computer that refused this iPhone's pairing: the card's mark and the banner say Pair again",
            arguments: ["-CC_FIXTURE", "stacked", "-CC_FIXTURE_LINK", "refused-pairing"],
            reach: { app, driver in
                try driver.require(driver.element(containing: "Pair again", in: app), "the pair-again mark")
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
                try driver.require(app.navigationBars["Waiting for you"], "the Deck")
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
                try driver.require(app.navigationBars["Waiting for you"], "the named HIGH card")
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
                try driver.require(app.navigationBars["Waiting for you"], "the named HIGH card")
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
                try driver.require(app.buttons["session-more"], "the session detail")
            }),

        // **A conversation, as a new user's first one reads.** Your words and
        // Claude's on one edge, and no "Turn complete" under a reply.
        RenderScenario(
            name: "session-conversation",
            purpose: "\"hi\" and Claude's answer: one edge, no line under a reply",
            arguments: [
                "-CC_FIXTURE", "timeline", "-CC_DEEPLINK", "codeconnect://session/fx-6",
            ],
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "What can I help you with", in: app), "Claude's reply")
            }),
        RenderScenario(
            name: "session-conversation-tools",
            purpose: "a longer exchange whose last turn ends on a tool: its line stays",
            arguments: [
                "-CC_FIXTURE", "timeline", "-CC_DEEPLINK", "codeconnect://session/fx-7",
            ],
            reach: { app, driver in
                try driver.require(driver.text(containing: "Turn complete", in: app), "the line")
            }),
        // **"Not sent: stopped."** A follow-up a Stop cancelled marks the person's own row.
        RenderScenario(
            name: "session-not-sent-stopped",
            purpose: "a follow-up a Stop cancelled: the person's row reads 'Not sent: stopped'",
            arguments: [
                "-CC_FIXTURE", "timeline", "-CC_DEEPLINK", "codeconnect://session/fx-7",
            ],
            reach: { app, driver in
                try driver.require(
                    driver.text(containing: "Not sent: stopped", in: app),
                    "the cancelled send's marker on the person's own row")
            }),
        // **The run's branch and its +N −M**, under the bar, in ccd's `git_branch`
        // and `diff_stat`: a long branch, a short one, a Codex run, and none.
        RenderScenario(
            name: "session-facts-long",
            purpose: "a branch longer than the line gives way in its middle; +N −M keeps its edge",
            arguments: ["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1"],
            reach: { app, driver in
                let facts = app.descendants(matching: .any).matching(identifier: "session-facts").firstMatch
                try driver.requireExists(facts, "the run's branch line")
                guard facts.label.contains("feature/") else { throw RenderFailure.unreachable("feature/ on the run's branch line") }
            }),
        RenderScenario(
            name: "session-facts-short",
            purpose: "a short branch and its +N −M",
            arguments: ["-CC_FIXTURE", "deck", "-CC_DEEPLINK", "codeconnect://session/fx-1"],
            reach: { app, driver in
                let facts = app.descendants(matching: .any).matching(identifier: "session-facts").firstMatch
                try driver.requireExists(facts, "the run's branch line")
                guard facts.label.contains("main") else { throw RenderFailure.unreachable("main on the run's branch line") }
            }),
        RenderScenario(
            name: "session-facts-codex",
            purpose: "a Codex run's branch and +N −M",
            arguments: ["-CC_FIXTURE", "stacked", "-CC_DEEPLINK", "codeconnect://session/fx-4"],
            reach: { app, driver in
                let facts = app.descendants(matching: .any).matching(identifier: "session-facts").firstMatch
                try driver.requireExists(facts, "the run's branch line")
                guard facts.label.contains("main") else { throw RenderFailure.unreachable("main on the run's branch line") }
            }),
        RenderScenario(
            name: "session-facts-none",
            purpose: "a folder ccd reports no branch for: no line under the bar",
            arguments: ["-CC_FIXTURE", "deck", "-CC_DEEPLINK", "codeconnect://session/fx-3"],
            reach: { app, driver in
                try driver.require(app.buttons["session-more"], "the session detail")
                if app.descendants(matching: .any).matching(identifier: "session-facts").firstMatch.exists {
                    throw RenderFailure.unreachable("no branch line for a run without one")
                }
            }),
        // **The run's ⋯ on a Claude run started from the app**: Changes, then
        // Claude Code's Compact and Clear, as the Mac's reply box offers them.
        RenderScenario(
            name: "session-menu-hosted",
            purpose: "the run's ⋯ on a hosted Claude run: every item with its icon, Compact and Clear",
            arguments: ["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1"],
            reach: { app, driver in
                try driver.openSessionMenu(app)
                try driver.requireExists(app.buttons["Compact…"], "Compact in the run's menu")
                try driver.requireExists(app.buttons["Clear…"], "Clear in the run's menu")
            }),
        // **Every ask is the app's own card**, centred under its dimming: a
        // run's removal, Clear, a mode that works without asking, and a
        // computer's removal. A new folder's name is `start2-b6-new-folder`.
        RenderScenario(
            name: "confirm-remove-run",
            purpose: "an ended run's Remove asks in the app's card",
            arguments: ["-CC_FIXTURE", "ended"],
            reach: { app, driver in
                let show = app.buttons["Show"]
                try driver.requireExists(show, "the Ended band's Show")
                func drag(_ id: String, _ dx: CGFloat, _ dy: CGFloat) {
                    let start = app.descendants(matching: .any).matching(identifier: id).firstMatch
                        .coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
                    start.press(forDuration: 0.05, thenDragTo: start.withOffset(CGVector(dx: dx, dy: dy)))
                }
                drag("session-fx-1", 0, -260)
                show.tap()
                drag("session-fx-1", 0, -260)
                let ended = app.descendants(matching: .any).matching(identifier: "session-01K1B3XQ8ZC0DE5FGH7JKMNP00")
                    .firstMatch
                try driver.requireExists(ended, "an ended run's row")
                // The revealed Remove sits at the row's trailing edge, fixed on
                // the screen while the row's content slides away from it.
                let block = CGVector(dx: ended.frame.maxX - 40, dy: ended.frame.midY)
                drag("session-01K1B3XQ8ZC0DE5FGH7JKMNP00", -112, 0)
                app.coordinate(withNormalizedOffset: .zero).withOffset(block).tap()
                try driver.requireExists(app.buttons["remove.confirm"], "the card's Remove")
            }),
        RenderScenario(
            name: "confirm-clear",
            purpose: "Clear asks in the app's card",
            arguments: ["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1"],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/cl", row: "/clear")
                try driver.requireExists(app.buttons["session.clear.confirm"], "the card's Clear")
            }),
        RenderScenario(
            name: "confirm-mode",
            purpose: "a mode that works without asking asks first, in the app's card over the settings sheet",
            arguments: ["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1"],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/m", row: "/model")
                let bypass = app.buttons.matching(NSPredicate(format: "label BEGINSWITH 'Bypass permissions'")).firstMatch
                try driver.require(bypass, "the Bypass permissions row").tap()
                try driver.requireExists(app.buttons["settings.mode.confirm"], "the card's confirm")
            }),
        RenderScenario(
            name: "confirm-remove-computer",
            purpose: "Remove this computer asks in the app's card over Settings",
            arguments: secondMachineArguments,
            reach: { app, driver in
                try driver.openSettings(app)
                try driver.require(secondMachineRow(app), "the offline machine's row").tap()
                try driver.require(app.buttons["Remove this computer"], "Remove this computer").tap()
                try driver.requireExists(app.buttons["computer.remove.confirm"], "the card's Remove")
            }),
        // **An ended run takes no reply**: no reply box, one quiet line in its
        // place, as on the Mac. A run the next message relaunches keeps its box.
        RenderScenario(
            name: "session-ended-line",
            purpose: "an ended run: the quiet line where the reply box was",
            arguments: ["-CC_FIXTURE", "ended", "-CC_DEEPLINK", "codeconnect://session/01K1B3XQ8ZC0DE5FGH7JKMNP00"],
            reach: { app, driver in
                try driver.requireExists(
                    app.descendants(matching: .any).matching(identifier: "session-ended-line").firstMatch,
                    "the ended line")
            }),
        RenderScenario(
            name: "session-ended-relaunchable",
            purpose: "an ended run the next message relaunches keeps its reply box",
            arguments: ["-CC_FIXTURE", "ended", "-CC_DEEPLINK", "codeconnect://session/01K1B3XQ8ZC0DE5FGH7JKMNP07"],
            reach: { app, driver in
                try driver.require(app.buttons["session-more"], "the session detail")
                if app.descendants(matching: .any).matching(identifier: "session-ended-line").firstMatch.exists {
                    throw RenderFailure.unreachable("the reply box, not the ended line")
                }
            }),
        // The header's one line, when the computer's name is longer than it.
        RenderScenario(
            name: "session-header-long-name",
            purpose: "a computer name longer than its line in the session bar's menu",
            arguments: [
                "-CC_FIXTURE", "timelineLongName", "-CC_DEEPLINK", "codeconnect://session/fx-6",
            ],
            reach: { app, driver in
                try driver.openSessionMenu(app)
                try driver.requireExists(
                    app.staticTexts["On Jordan’s MacBook Pro (16-inch, Nov 2024)"].firstMatch,
                    "the computer's name, heading the menu's link item")
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
            // The deep link opens the card: the same route a tapped
            // notification takes, proven at both sizes by `DeepLinkTests`.
            // Nothing here taps Review, so a link that stopped opening the
            // sheet fails this scenario rather than being papered over.
            arguments: [
                "-CC_FIXTURE", "deck",
                "-CC_DEEPLINK", "codeconnect://session/fx-1?request=toolu_fixture_high",
            ],
            reach: { app, driver in
                try driver.require(driver.text(containing: "Decision", in: app), "the sheet title")
            }),

        // **A run of tool rows, which nothing else reaches.** The `deck`
        // fixture's first session has a single approval and no tool calls, so
        // the one place tool rows stack — flush, each a line of words, and
        // where their 44pt targets are paid for — was never photographed.
        // `stacked` puts `Bash`, `Read` and `MultiEdit` on one run because
        // their names are three different widths.
        RenderScenario(
            name: "session-tool-rows",
            purpose: "a run of tool calls, with the names at three widths",
            // Deep-linked rather than tapped. At AX5 the Running band is far
            // below the fold, so a tap-driven route reached this at reading
            // size and failed the pass at AX5 — and a scenario that only
            // renders at one size is exactly the coverage gap this catalogue
            // exists to close.
            arguments: [
                "-CC_FIXTURE", "stacked", "-CC_DEEPLINK", "codeconnect://session/fx-5",
            ],
            reach: { app, driver in
                try driver.require(app.buttons["session-more"], "the session detail")
            }),

        // **The slash palette and its five native commands.** Every state
        // here shipped broken once for want of a photograph: the first
        // palette read as "Mac only" scolding.
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
                    driver.element(containing: "/clear", in: app), "the /clear row",
                    scrollingWithin: driver.list(holding: "/clear", in: app))
            }),
        RenderScenario(
            name: "session-hosted-settings-sheet",
            purpose:
                "the hosted run's unified model/effort/mode sheet, minor-39 menus in their worst shapes",
            arguments: [
                "-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/m", row: "/model")
                try driver.require(
                    driver.text(containing: "Permission mode", in: app), "the mode section")
                try driver.require(
                    driver.element(containing: "Extra high", in: app),
                    "the labelled efforts the hosted sheet draws")
            }),
        RenderScenario(
            name: "session-hosted-settings-model",
            purpose:
                "the hosted settings sheet at its top — the Model section, each model by its name with the agent's own "
                + "description beside it, on one line",
            arguments: [
                "-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/m", row: "/model")
                // One row holds the name and, beside it, the description as the agent sends it, price included.
                let sonnet = driver.element(
                    containing: "Sonnet, Sonnet 5.5 · Efficient for routine tasks · $2/$10 per Mtok", in: app)
                try driver.require(sonnet, "the Sonnet row with its description beside its name")
                // On one line: the row is no taller than a row with no description, as an effort's.
                let effort = driver.element(containing: "Extra high", in: app)
                try driver.require(effort, "an effort row to measure the Sonnet row against")
                if sonnet.frame.height > effort.frame.height + 1 {
                    throw RenderFailure.unreachable("the Sonnet row on one line")
                }
            }),
        RenderScenario(
            name: "session-hosted-settings-modes",
            purpose: "the hosted settings sheet scrolled to Permission mode — Plan is the selected row",
            arguments: [
                "-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1",
            ],
            reach: { app, driver in
                try driver.openPaletteRow(app, fragment: "/m", row: "/model")
                try driver.require(driver.text(containing: "Plan", in: app), "the Plan mode row")
            }),
        RenderScenario(
            name: "session-settings-claude-plan",
            purpose: "the composer settings control — a hosted Claude run in Plan, the mode named beside the model",
            arguments: ["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1"],
            reach: { app, driver in
                try driver.require(
                    app.buttons["session.composer.settings"], "the composer settings control")
            }),
        RenderScenario(
            name: "session-settings-claude-bypass",
            purpose: "the composer settings control — Claude in Bypass permissions, a confirm mode named in red",
            arguments: ["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-2"],
            reach: { app, driver in
                try driver.require(
                    app.buttons["session.composer.settings"], "the composer settings control")
            }),
        RenderScenario(
            name: "session-settings-over-a-long-reply",
            purpose:
                "Claude in Bypass permissions under a reply scrolled back a little, so its text runs where the "
                + "settings line stands: the line and the reply box are one block on the page's ground",
            arguments: [
                "-CC_FIXTURE", "hostedSettings", "-CC_FIXTURE_LONG_REPLY", "YES",
                "-CC_DEEPLINK", "codeconnect://session/fx-2",
            ],
            reach: { app, driver in
                try driver.require(
                    app.buttons["session.composer.settings"], "the composer settings control")
                // The reply folds behind "Show more"; unfolded, it runs past the screen.
                try driver.require(app.buttons["Show more"], "the long reply's Show more").tap()
                try driver.require(driver.text(containing: "Section 8", in: app), "the long reply's last section")
                app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.40))
                    .press(
                        forDuration: 0.05,
                        thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.50)))
            }),
        RenderScenario(
            name: "session-settings-codex-default",
            purpose: "the composer settings control — a live Codex run at its default, the model named alone",
            arguments: ["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/cx-1"],
            reach: { app, driver in
                try driver.require(
                    app.buttons["session.composer.settings"], "the composer settings control")
            }),
        RenderScenario(
            name: "session-settings-codex-full",
            purpose: "the composer settings control — Codex in Full Access, a confirm mode named in red",
            arguments: ["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/cx-2"],
            reach: { app, driver in
                try driver.require(
                    app.buttons["session.composer.settings"], "the composer settings control")
            }),
        RenderScenario(
            name: "session-settings-opens-sheet-claude",
            purpose: "tapping the composer settings control opens the unified settings sheet for a Claude run",
            arguments: ["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/fx-1"],
            reach: { app, driver in
                try driver.require(
                    app.buttons["session.composer.settings"], "the composer settings control").tap()
                try driver.require(
                    driver.text(containing: "Permission mode", in: app),
                    "the settings sheet, opened by the control")
            }),
        RenderScenario(
            name: "session-settings-opens-sheet-codex",
            purpose: "tapping the composer settings control opens the unified settings sheet for a Codex run",
            arguments: ["-CC_FIXTURE", "hostedSettings", "-CC_DEEPLINK", "codeconnect://session/cx-1"],
            reach: { app, driver in
                try driver.require(
                    app.buttons["session.composer.settings"], "the composer settings control").tap()
                try driver.require(
                    driver.text(containing: "Permission mode", in: app),
                    "the settings sheet, opened by the control")
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
                try driver.openChanges(app)
                try driver.require(
                    driver.text(containing: "Captured on the computer", in: app),
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
                try driver.openChanges(app)
                try driver.require(
                    driver.text(containing: "Truncated", in: app),
                    "the truncation banner")
            }),

        RenderScenario(
            name: "settings",
            purpose: "the machines this iPhone keeps, and the way to add another",
            arguments: ["-CC_FIXTURE", "deck"],
            reach: { app, driver in try driver.openSettings(app) }),

        RenderScenario(
            name: "settings-unjoined",
            purpose: "a computer of the owner's whose CodeConnect answered and refused this iPhone, with why",
            arguments: ["-CC_FIXTURE", "deck", "-CC_UNJOINED", "refused"],
            reach: { app, driver in
                try driver.openSettings(app)
                try driver.require(
                    driver.element(containing: "without a pairing code", in: app), "the refused row")
            }),

        RenderScenario(
            name: "settings-removed-on-tailnet",
            purpose: "a computer the owner removed that his Tailscale account still lists, offered to add again",
            arguments: ["-CC_FIXTURE", "deck", "-CC_UNJOINED", "removed"],
            reach: { app, driver in
                try driver.openSettings(app)
                try driver.require(driver.element(containing: "On your Tailscale account", in: app), "the section")
            }),

        RenderScenario(
            name: "settings-machines",
            purpose: "two machines, one offline with the longest name, and the way to add another",
            arguments: secondMachineArguments,
            reach: { app, driver in
                try driver.openSettings(app)
                try driver.require(secondMachineRow(app), "the offline machine's row")
            }),

        RenderScenario(
            name: "machine",
            purpose: "one machine's own screen: its link, its encryption in plain words, Remove",
            arguments: secondMachineArguments,
            reach: { app, driver in
                try driver.openSettings(app)
                try driver.require(secondMachineRow(app), "the offline machine's row").tap()
                try driver.require(
                    app.navigationBars[secondMachine], "that machine's own screen, by its name")
                try driver.require(app.buttons["Remove this computer"], "Remove this computer")
            }),

        RenderScenario(
            name: "link-health",
            purpose: "freshness measured, not assumed",
            arguments: ["-CC_FIXTURE", "deck"],
            reach: { app, driver in try driver.openLinkHealth(app) }),

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
                try driver.openLinkHealth(app)
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
                try driver.openLinkHealth(app)
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
                try driver.openLinkHealth(app)
                try driver.scrollUntil(app, "the unsupported message on screen") {
                    driver.element(containing: "App Attest", in: app).isHittable
                }
            }),
        RenderScenario(
            name: "push-relay-bootstrap",
            purpose: "a relay daemon over a bootstrap connection: pairing required, no dead reset",
            arguments: ["-CC_FIXTURE", "deck", "-CC_PUSH_STATE", "bootstrap"],
            reach: { app, driver in
                try driver.openLinkHealth(app)
                try driver.scrollUntil(app, "the pairing-required message on screen") {
                    driver.element(containing: "connected without pairing", in: app).isHittable
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
    /// A second machine beside the fixture's, offline, named as long as a
    /// machine's name gets.
    static let secondMachine = "build-server-eu-west-1"
    static let secondMachineArguments = [
        "-CC_FIXTURE", "deck", "-CC_FIXTURE_SECOND_MACHINE", secondMachine,
    ]

    /// Its row in Settings, by the name its label starts with.
    static func secondMachineRow(_ app: XCUIApplication) -> XCUIElement {
        app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", secondMachine)).firstMatch
    }

    static let codexSessionKey = "cx-1"

    /// One scenario, with the boilerplate every Codex render shares.
    private static func codexScenario(
        _ state: String,
        purpose: String,
        route: String = "fleet",
        /// The session the deeplink opens, when it is not the shared `cx-1` —
        /// the captured stream carries the identity the daemon recorded.
        sessionKey: String? = nil,
        reach: @escaping (XCUIApplication, RenderDriver) throws -> Void
    ) -> RenderScenario {
        var arguments = ["-CC_CODEX", state]
        if route != "fleet" {
            arguments += [
                "-CC_DEEPLINK", "codeconnect://session/\(sessionKey ?? codexSessionKey)",
            ]
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
    /// accessibility sizes. It is not Codex's, it is not fixed here, and
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
                "two options — the shape Claude's >2 rule once made UNANSWERABLE",
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
            "resolved-accepted", purpose: "answered from a phone, on the Codex link",
            route: "session",
            reach: { app, driver in
                try openResolvedCard(app, driver)
                try driver.require(
                    driver.element(containing: "Answered from a phone", in: app),
                    "the ending's own sentence, on the card")
            }),
        codexScenario(
            "resolved-declined", purpose: "declined from a phone — Codex was told no",
            route: "session",
            reach: { app, driver in
                try openResolvedCard(app, driver)
                try driver.require(
                    driver.element(containing: "Answered from a phone", in: app),
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
        // **The gate, photographed.** `codex_link` is `none`, so the send
        // path refuses before anything leaves: what is on screen is the app's
        // own sentence over "Nothing was sent", and the Mac has said nothing
        // because it was never asked.
        codexScenario(
            "stop-link-down",
            purpose: "the phone's own refusal: no link, so no frame ever left",
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(
                    driver.element(containing: "Nothing was sent", in: app),
                    "the app's own words, not the daemon's")
                try driver.require(
                    driver.element(containing: "no live link", in: app),
                    "why the phone would not send")
            }),
        // **The refusal after the fact**, which is a different screen and a
        // different author. The summary said `subscribed`, the frame left, and
        // the Mac answered that it had no connection to the run.
        codexScenario(
            "stop-refused-late",
            purpose:
                "the daemon's refusal VERBATIM, and a Stop greyed for ten seconds — not for ever",
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(
                    driver.element(containing: "no connection to this run", in: app),
                    "the daemon's own sentence, unedited")
            }),
        // **What a refusal said, opened from Stop's place on the row**, with
        // Stop offered beside it.
        RenderScenario(
            name: "codex-stop-refused-late-sheet",
            purpose: "the stop's outcome opened from its word on the row: the daemon's sentence, whole",
            arguments: ["-CC_CODEX", "stop-refused-late"],
            reach: { app, driver in
                try driver.fleet(app)
                try driver.require(app.buttons["stop-outcome-\(codexSessionKey)"], "the outcome's word").tap()
                try driver.require(
                    driver.text(containing: "no connection to this run", in: app),
                    "the daemon's own sentence, unedited")
                // Stop itself, on screen with nothing over it: while a word
                // stands in its place on the row, this is the way to stop the
                // turn.
                try driver.require(app.buttons["stop-again-\(codexSessionKey)"], "Stop beside the sentence")
            }),
        // **The session route**, so the session screen's own Stop control is
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

        // **Stop in the composer's circle** (N7): while a turn runs, the empty field's circle is the run's Stop; a
        // typed message turns it back to Send; a stopped turn leaves the circle to dictation.
        RenderScenario(
            name: "codex-stop-offered-session",
            purpose: "a running turn: the empty field's circle is Stop",
            arguments: ["-CC_CODEX", "stop-offered", "-CC_DEEPLINK", "codeconnect://session/\(codexSessionKey)"],
            reach: { app, driver in
                try driver.require(app.buttons["stop-\(codexSessionKey)"], "the circle's Stop face")
            }),
        RenderScenario(
            name: "codex-stop-offered-typed",
            purpose: "a running turn with a message typed while a card waits: the draft cannot go yet, so the circle stays Stop",
            arguments: ["-CC_CODEX", "stop-offered", "-CC_DEEPLINK", "codeconnect://session/\(codexSessionKey)"],
            reach: { app, driver in
                try driver.typeIntoComposer(app, "Also run the linter.")
                try driver.require(app.buttons["stop-\(codexSessionKey)"], "the circle's Stop face")
            }),
        RenderScenario(
            name: "codex-stop-aborted-session",
            purpose: "a stopped turn on the session screen: no Stop, the circle back to dictation",
            arguments: ["-CC_CODEX", "stop-aborted", "-CC_DEEPLINK", "codeconnect://session/\(codexSessionKey)"],
            reach: { app, driver in
                try driver.require(app.buttons["Start dictation"], "the circle's dictation face")
            }),

        // ---- C-series: Compose -----------------------------------------
        codexScenario(
            "compose-started", purpose: "Codex was idle: your words began a turn, and nothing says so but the conversation",
            route: "session",
            reach: { app, driver in
                // A message that landed says nothing: the photograph shows the line above the field empty, and the
                // timeline laid out to the composer.
                try driver.require(app.collectionViews["session-timeline"], "the session timeline")
            }),
        // **The one Codex render whose input is the daemon's own output.**
        //
        // Every other scenario on this list is staged from JSON written in this
        // repo, and that is how a serious bug shipped: the hand-made
        // `user_message` wore Claude's `{"message":{"content":…}}` while the
        // adapter sends `{"text":…}`, so the fixtures and the app agreed with
        // each other, both renders were green, and a real phone drew a whole
        // Codex turn as a lone "Turn complete". This one replays
        // `fixtures/codex/phone-turn-stream-0.153.4.json` — the file ccd
        // byte-compares against — frame for frame, and requires the words and
        // the tool rows to be in the photograph.
        codexScenario(
            "phone-turn-stream",
            purpose: "the daemon's own capture of a phone-started turn, replayed frame for frame",
            route: "session",
            sessionKey: "01K1B3XQ8ZC0DE5FGH7JKMNPQR",
            reach: { app, driver in
                // The screen opens on the tail, and the subject is the head of
                // the conversation — so these scroll *back*, which nothing in
                // this catalogue had ever needed to do. The command row first,
                // then the words just above it, so the photograph ends on the
                // thing the operator could not see.
                // The timeline list itself, measured once: its rectangle stays
                // where it is while the rows scroll back under it.
                let timeline = try driver.require(
                    app.collectionViews["session-timeline"], "the session timeline"
                ).frame
                try driver.requireEarlier(
                    driver.element(containing: "/bin/zsh", in: app),
                    "the command tool row, whose argument the payload holds at its top level",
                    inside: timeline)
                try driver.requireEarlier(
                    // "just run it" rather than "Do not explain": both of the
                    // capture's user messages end with that phrase, and a
                    // fragment two rows answer to is a fragment that certifies
                    // whichever one the query happened to reach first.
                    driver.element(containing: "just run it", in: app),
                    "what the phone said, from the daemon's own bytes",
                    inside: timeline)
            }),
        codexScenario(
            "compose-steered", purpose: "Codex was working: your words joined the turn, and nothing says so but the conversation",
            route: "session",
            reach: { app, driver in
                // A message that landed says nothing: the photograph shows the line above the field empty, and the
                // timeline laid out to the composer.
                try driver.require(app.collectionViews["session-timeline"], "the session timeline")
            }),
        codexScenario(
            "compose-duplicate", purpose: "a replay of words that landed: nothing is said a second time, and nothing says so",
            route: "session",
            reach: { app, driver in
                // A message that landed says nothing: the photograph shows the line above the field empty, and the
                // timeline laid out to the composer.
                try driver.require(app.collectionViews["session-timeline"], "the session timeline")
            }),
        codexScenario(
            "compose-rejected", purpose: "nothing was sent, in the daemon's own words",
            route: "session",
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "no connection to this run", in: app),
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

    // =========================================================================
    //  Claude's question card — `AskUserQuestion` answered from the phone.
    //
    //  Seeded through `-CC_QUESTION <state>`: one session holding the live
    //  2.1.286 four-question card in a minor-21 daemon's shape, its words
    //  lengthened to the worst case (labels that wrap, `café ☕`, the real
    //  previews). The steps are *tapped* to, through the real card, so each
    //  render is what a reader reaches.
    // =========================================================================

    /// One question-card scenario: the session opened by deep link, the card
    /// opened from its own timeline row.
    private static func questionScenario(
        _ name: String, state: String, purpose: String, opensWith button: String = "Answer",
        reach: @escaping (XCUIApplication, RenderDriver) throws -> Void
    ) -> RenderScenario {
        RenderScenario(
            name: "question-\(name)", purpose: purpose,
            arguments: ["-CC_QUESTION", state, "-CC_DEEPLINK", "codeconnect://session/01K1B3XQ8ZC0DE5FGH7JKMNPQR"],
            reach: { app, driver in
                let open = app.buttons[button].firstMatch
                try driver.requireExists(open, "the question's \(button) button")
                try tapPastTheTailPill(app, driver, open, "the \(button) button on screen")
                try driver.requireExists(
                    app.staticTexts["4 questions"].firstMatch, "the question card's sheet")
                try reach(app, driver)
            })
    }

    /// Taps a control on the card at its top edge, once that edge is in view
    /// between the sheet's chrome and the pinned bar.
    ///
    /// Never through `RenderDriver.require`'s drag: it starts on the option
    /// rows and a press there toggles one. Never a bare `tap()` either —
    /// XCUITest taps an element's centre, which at AX5 is a row taller than the
    /// screen or a field behind the bar, where the tap lands on `Next`. The card
    /// is scrolled by a drag in its left margin, outside every row.
    private static func tapQuestion(
        _ app: XCUIApplication, _ driver: RenderDriver, _ identifier: String
    ) throws {
        let control = app.buttons[identifier].firstMatch
        try driver.requireExists(control, identifier)
        if identifier == "question-next" {
            control.tap()
        } else {
            try tapTop(app, control, identifier)
        }
    }

    private static func tapTop(
        _ app: XCUIApplication, _ element: XCUIElement, _ what: String
    ) throws {
        let bar = app.buttons.matching(
            NSPredicate(
                format: "identifier IN %@", ["question-next", "question-submit", "question-back"])
        ).allElementsBoundByIndex.map(\.frame.minY).min() ?? app.frame.maxY
        // The sheet's own chrome ends where its title does.
        let top = app.staticTexts["4 questions"].firstMatch.frame.maxY
        let reach: CGFloat = 44
        let margin = app.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: 12, dy: app.frame.height * 0.45))
        func inView() -> Bool {
            element.frame.minY >= top && element.frame.minY + reach <= bar
        }
        for _ in 0..<10 where !inView() {
            let travel: CGFloat = element.frame.minY + reach > bar ? -200 : 200
            margin.press(forDuration: 0.05, thenDragTo: margin.withOffset(CGVector(dx: 0, dy: travel)))
        }
        guard inView() else { throw RenderFailure.offScreen(what, frame: element.frame) }
        element.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0))
            .withOffset(CGVector(dx: 0, dy: reach / 2)).tap()
    }

    /// Types into the "Other" field just opened.
    private static func typeOther(
        _ app: XCUIApplication, _ driver: RenderDriver, _ text: String
    ) throws {
        let field = try driver.requireExists(app.textFields["Other"], "the Other field")
        try tapTop(app, field, "the Other field")
        app.typeText(text + "\n")
    }

    /// The Other field, by what was typed into it.
    private static func typedOther(_ fragment: String, in app: XCUIApplication) -> XCUIElement {
        app.descendants(matching: .any)
            .matching(NSPredicate(format: "value CONTAINS %@", fragment)).firstMatch
    }

    /// Answers questions `0..<count` with the daemon fixture's own choices.
    private static func answerQuestions(
        _ app: XCUIApplication, _ driver: RenderDriver, through count: Int
    ) throws {
        let steps: [() throws -> Void] = [
            { try tapQuestion(app, driver, "question-0-option-0") },
            {
                try tapQuestion(app, driver, "question-1-option-0")
                try tapQuestion(app, driver, "question-1-option-2")
            },
            {
                try tapQuestion(app, driver, "question-2-other")
                try typeOther(app, driver, "Archive it under docs/plans, café ☕")
            },
            { try tapQuestion(app, driver, "question-3-option-1") },
        ]
        for step in steps.prefix(count) {
            try step()
            try tapQuestion(app, driver, "question-next")
        }
    }

    static let questions: [RenderScenario] = [
        questionScenario(
            "step-single", state: "held",
            purpose: "question 1 of 4: a single choice with two long labels, nothing chosen yet",
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "Question 1 of 4", in: app), "the step's own count")
                guard !app.buttons["Allow"].exists else {
                    throw RenderFailure.unreachable("a question card must never offer Allow")
                }
            }),
        questionScenario(
            "step-multi-other", state: "held",
            purpose: "question 2: several choices kept in tap order, plus Other with café ☕",
            reach: { app, driver in
                try answerQuestions(app, driver, through: 1)
                try tapQuestion(app, driver, "question-1-option-2")
                try tapQuestion(app, driver, "question-1-option-0")
                try tapQuestion(app, driver, "question-1-other")
                try typeOther(app, driver, "Fuzz for an hour, café ☕")
                try driver.require(typedOther("Fuzz for an hour", in: app), "the typed Other answer")
            }),
        questionScenario(
            "step-other", state: "held",
            purpose: "question 3: Other chosen and typed, the single choice cleared",
            reach: { app, driver in
                try answerQuestions(app, driver, through: 2)
                try tapQuestion(app, driver, "question-2-option-0")
                try tapQuestion(app, driver, "question-2-other")
                try typeOther(app, driver, "Archive it under docs/plans, café ☕")
                try driver.require(typedOther("Archive it under", in: app), "the typed Other answer")
            }),
        questionScenario(
            "step-preview", state: "held",
            purpose: "question 4: the chosen option's preview under it, and the Notes field",
            reach: { app, driver in
                try answerQuestions(app, driver, through: 3)
                try tapQuestion(app, driver, "question-3-option-1")
                try driver.require(
                    app.descendants(matching: .any)["question-3-preview"].firstMatch,
                    "the preview under the chosen option")
            }),
        questionScenario(
            "review", state: "held",
            purpose: "Review: every answer before it is sent, Submit live",
            reach: { app, driver in
                try answerQuestions(app, driver, through: 4)
                try driver.require(app.buttons["question-submit"].firstMatch, "Submit")
                try driver.require(
                    driver.element(containing: "Review your answers", in: app), "the review list")
            }),
        questionScenario(
            "old-mac", state: "old-mac",
            purpose: "read-only on a minor-20 Mac: no Allow, and why",
            opensWith: "View",
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "too old to answer", in: app), "the card's status")
                guard !app.buttons["Allow"].exists else {
                    throw RenderFailure.unreachable("an old Mac's question must not offer Allow")
                }
            }),
        questionScenario(
            "answered", state: "answered",
            purpose: "answered on this iPhone, confirmed by the Mac, with what was sent",
            opensWith: "View",
            reach: { app, driver in
                try driver.require(
                    driver.element(containing: "Claude has the answers sent from the phone", in: app),
                    "the card's status")
            }),
    ]

    /// The kit's own review surface, one render per page.
    ///
    /// **This is the only reachable render several components have**, and a
    /// claim nobody can photograph is a claim nobody has tested.
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
