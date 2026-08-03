import XCTest

@testable import CodeConnect

/// The two rules that decide what a tap costs, and the order a queue is emptied
/// in. Both are behaviour a user can be hurt by, and neither is visible from a
/// screenshot.
final class RiskAndDeckTests: XCTestCase {

    private func card(tool: String, input: JSONValue, risk: WireRisk? = nil) -> ApprovalCard {
        let display = "\(tool)\n\(input.canonicalJSONString)"
        return ApprovalCard(
            requestID: "r-\(tool)", payloadHash: "hash", toolName: tool, toolInput: input,
            displayText: display, permissionSuggestions: nil, promptID: nil, permissionMode: nil,
            risk: risk)
    }

    private let newerDaemon = DaemonProfile(protocolVersion: 1, protocolMinor: 1, capabilities: nil)
    private let olderDaemon = DaemonProfile(protocolVersion: 1, protocolMinor: 0, capabilities: nil)

    private func assess(
        tool: String, input: JSONValue, risk: WireRisk?, profile: DaemonProfile
    ) -> RiskAssessment {
        RiskAssessment.resolve(wire: risk, tool: tool, input: input, profile: profile)
    }

    // MARK: The reconciliation rule

    func testDaemonClassIsUsedWhenItAgrees() {
        let assessment = assess(
            tool: "Read", input: .object(["file_path": .string("/tmp/a")]),
            risk: WireRisk(cls: "low", matchedPattern: nil), profile: newerDaemon)
        XCTAssertEqual(assessment.effective, .low)
        XCTAssertEqual(assessment.source, .daemon)
        XCTAssertFalse(assessment.escalatedLocally)
    }

    func testDaemonCanTightenTheGate() {
        // A plain `ls` reads as low here; the Mac knows it is outside the
        // worktree and says medium. The stricter one wins.
        let assessment = assess(
            tool: "Bash", input: .object(["command": .string("ls /etc")]),
            risk: WireRisk(cls: "medium", matchedPattern: "outside worktree"), profile: newerDaemon)
        XCTAssertEqual(assessment.effective, .medium)
        XCTAssertEqual(assessment.matchedPattern, "outside worktree")
    }

    /// The load-bearing one: the daemon must never be able to *loosen* a gate
    /// below what this build's own reading of the command justifies.
    func testDaemonCannotLoosenTheGate() {
        let assessment = assess(
            tool: "Bash", input: .object(["command": .string("sudo rm -rf /Users/me/work")]),
            risk: WireRisk(cls: "low", matchedPattern: nil), profile: newerDaemon)
        XCTAssertEqual(assessment.effective, .high, "a destructive command cannot become one tap")
        XCTAssertEqual(assessment.declared, .low)
        XCTAssertTrue(assessment.escalatedLocally)
        XCTAssertTrue(assessment.provenance.contains("stricter"))
    }

    /// Contract: on a classifying daemon, an absent class means medium.
    func testAbsentClassOnAClassifyingDaemonMeansMedium() {
        let assessment = assess(
            tool: "Read", input: .object(["file_path": .string("/tmp/a")]), risk: nil,
            profile: newerDaemon)
        XCTAssertEqual(assessment.effective, .medium)
        XCTAssertEqual(assessment.source, .daemonSilent)
    }

    /// …but the floor is a floor, not a ceiling.
    func testAbsentClassStillEscalatesADestructiveCommand() {
        let assessment = assess(
            tool: "Bash", input: .object(["command": .string("git push --force")]), risk: nil,
            profile: newerDaemon)
        XCTAssertEqual(assessment.effective, .high)
    }

    /// An older daemon never offered a class, so nothing is "absent" and the
    /// original behaviour is preserved — a `Read` stays a single tap.
    func testOldDaemonKeepsTheLocalReading() {
        let assessment = assess(
            tool: "Read", input: .object(["file_path": .string("/tmp/a")]), risk: nil,
            profile: olderDaemon)
        XCTAssertEqual(assessment.effective, .low)
        XCTAssertEqual(assessment.source, .local)
    }

    func testUnparseableClassFallsBackRatherThanTrusting() {
        let assessment = assess(
            tool: "Bash", input: .object(["command": .string("echo hi")]),
            risk: WireRisk(cls: "catastrophic", matchedPattern: nil), profile: newerDaemon)
        XCTAssertNil(assessment.declared)
        XCTAssertEqual(assessment.effective, .medium, "unreadable means the medium floor applies")
    }

    func testAssessmentReachesTheApprovalItem() {
        let item = ApprovalItem(
            card: card(
                tool: "Bash", input: .object(["command": .string("echo hi")]),
                risk: WireRisk(cls: "high", matchedPattern: "pipe to shell")),
            requestedAt: Date(), sessionKey: "cc-1", sessionName: "cc-1", outcome: nil,
            paneSnapshot: nil, risk: WireRisk(cls: "high", matchedPattern: "pipe to shell"))
        let assessment = item.assessment(profile: newerDaemon)
        XCTAssertEqual(assessment.effective, .high)
        XCTAssertEqual(assessment.matchedPattern, "pipe to shell")
    }

    // MARK: Deck ordering

    /// `session` is a *key* — a uid on a modern daemon, a tmux name on an old
    /// one. The name is carried alongside it for display only.
    ///
    /// `input` decides the class: with no `risk` block on the wire and a
    /// classifying daemon, `RiskAssessment` takes the stricter of MEDIUM and
    /// what this build reads off the command, which is how a queue built from
    /// fixtures gets three genuinely different tiers.
    private func item(
        session: String, request: String, ageSeconds: TimeInterval, name: String = "cc-1",
        tool: String = "Bash", input: JSONValue = .object([:])
    ) -> ApprovalItem {
        ApprovalItem(
            card: ApprovalCard(
                requestID: request, payloadHash: "h", toolName: tool,
                toolInput: input, displayText: "d", permissionSuggestions: nil,
                promptID: nil, permissionMode: nil, risk: nil),
            requestedAt: Date(timeIntervalSince1970: 1_000_000 - ageSeconds), sessionKey: session,
            sessionName: name, outcome: nil, paneSnapshot: nil, risk: nil)
    }

    private func low(_ request: String, ageSeconds: TimeInterval) -> ApprovalItem {
        item(
            session: "cc-\(request)", request: request, ageSeconds: ageSeconds, tool: "Read",
            input: .object(["file_path": .string("/tmp/a")]))
    }

    private func medium(_ request: String, ageSeconds: TimeInterval) -> ApprovalItem {
        item(
            session: "cc-\(request)", request: request, ageSeconds: ageSeconds, tool: "Write",
            input: .object(["file_path": .string("/tmp/a")]))
    }

    private func high(_ request: String, ageSeconds: TimeInterval) -> ApprovalItem {
        item(
            session: "cc-\(request)", request: request, ageSeconds: ageSeconds, tool: "Bash",
            input: .object(["command": .string("git push --force origin main")]))
    }

    /// **The settled rule: urgency first.** A `git push --force` must not sit
    /// behind a `Read` because the `Read` has been waiting longer. Under the
    /// old age-first order the accessory bar advertised the LOW card while the
    /// force-push waited above it in the list.
    func testDeckRanksByRiskTierBeforeAge() {
        let cards = [
            low("a", ageSeconds: 600),
            high("c", ageSeconds: 30),
            medium("b", ageSeconds: 300),
        ]
        XCTAssertEqual(
            DeckOrdering.sort(cards, profile: olderDaemon).map(\.card.requestID), ["c", "b", "a"],
            "HIGH before MEDIUM before LOW, whatever the ages say")
    }

    /// …and age is still the rule *inside* a tier, so nothing starves.
    func testDeckIsOldestFirstWithinATier() {
        let cards = [
            high("young", ageSeconds: 30),
            high("old", ageSeconds: 900),
            high("middle", ageSeconds: 300),
        ]
        XCTAssertEqual(
            DeckOrdering.sort(cards, profile: olderDaemon).map(\.card.requestID),
            ["old", "middle", "young"],
            "inside one tier the agent that has waited longest is the one you owe")
    }

    /// The tier has to be the same class the *card* gates on, or a card would
    /// be ranked in a tier its own badge disagrees with. On a classifying
    /// daemon an absent class means MEDIUM; on an older one a `Read` is LOW.
    func testDeckRanksOnTheSameClassTheCardGatesOn() {
        let cards = [low("read", ageSeconds: 600), medium("write", ageSeconds: 30)]
        XCTAssertEqual(
            DeckOrdering.sort(cards, profile: olderDaemon).map(\.card.requestID), ["write", "read"],
            "on an older daemon the Read really is LOW and sorts below the Write")
        XCTAssertEqual(
            DeckOrdering.sort(cards, profile: newerDaemon).map(\.card.requestID), ["read", "write"],
            "on a classifying daemon both float to the medium floor, so age decides")
    }

    func testDeckTiesBreakStablyRatherThanReshuffling() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        let cards = [
            ApprovalItem(
                card: ApprovalCard(
                    requestID: "z", payloadHash: "h", toolName: "T", toolInput: .object([:]),
                    displayText: "d", permissionSuggestions: nil, promptID: nil,
                    permissionMode: nil, risk: nil), requestedAt: now, sessionKey: "cc-1",
                sessionName: "cc-1", outcome: nil, paneSnapshot: nil, risk: nil),
            ApprovalItem(
                card: ApprovalCard(
                    requestID: "a", payloadHash: "h", toolName: "T", toolInput: .object([:]),
                    displayText: "d", permissionSuggestions: nil, promptID: nil,
                    permissionMode: nil, risk: nil), requestedAt: now, sessionKey: "cc-1",
                sessionName: "cc-1", outcome: nil, paneSnapshot: nil, risk: nil),
        ]
        XCTAssertEqual(
            DeckOrdering.sort(cards, profile: newerDaemon).map(\.card.requestID), ["a", "z"])
        XCTAssertEqual(
            DeckOrdering.sort(cards.reversed(), profile: newerDaemon).map(\.card.requestID),
            ["a", "z"],
            "a stack that reshuffles between renders is a stack you cannot tap")
    }

    /// A settled card must leave the stack immediately, not when the resolution
    /// event eventually arrives — otherwise it sits on top inviting a second
    /// answer to a decision already made.
    func testSettledCardsLeaveTheStackAtOnce() {
        var pass = DeckPass()
        let cards = [
            item(session: "cc-1", request: "a", ageSeconds: 600),
            item(session: "cc-2", request: "b", ageSeconds: 60),
        ]
        pass.settle(cards[0].id)
        XCTAssertEqual(pass.arrange(cards, profile: newerDaemon).map(\.card.requestID), ["b"])
        XCTAssertEqual(pass.settledCount, 1)
    }

    /// Postponing is not deciding: the card stays in the queue, at the back.
    func testPostponedCardsGoToTheBackAndStayPending() {
        var pass = DeckPass()
        let cards = [
            item(session: "cc-1", request: "a", ageSeconds: 600),
            item(session: "cc-2", request: "b", ageSeconds: 300),
            item(session: "cc-3", request: "c", ageSeconds: 60),
        ]
        pass.postpone(cards[0].id)
        XCTAssertEqual(
            pass.arrange(cards, profile: newerDaemon).map(\.card.requestID), ["b", "c", "a"])
        pass.postpone(cards[1].id)
        XCTAssertEqual(
            pass.arrange(cards, profile: newerDaemon).map(\.card.requestID), ["c", "a", "b"])
        XCTAssertEqual(pass.settledCount, 0, "postponing decides nothing")
    }

    // MARK: The read-before-you-decide gate

    /// Nothing in either test target referenced `hasSeenCommand`, `armGate` or
    /// the reason string. The gate that separates MEDIUM from LOW — and, at
    /// AX5, the only thing between one tap and an unread write outside the
    /// worktree — had no test at any level. These are the assertions that
    /// would have caught it.

    /// **A gate that has seen nothing is shut at every tier, and states why.**
    ///
    /// This assertion used to read `XCTAssertTrue(gate.isOpen(at: .low))`, and
    /// it was wrong in the same way the code was: risk decides *friction* — a
    /// tap, a hold, a face — and never whether the command has to be visible. A
    /// LOW `Read` at AX5 measured `Allow` enabled, as the filled white primary
    /// in the thumb zone, over **13 of the 24 characters** of
    /// `/Users/dev/app/README.md`; the eleven that were behind the action bar
    /// are the ones that tell a README from `~/.ssh/id_ed25519`.
    ///
    /// The old exemption's argument — "a `Read` that costs a scroll is a gate
    /// people learn to defeat" — is answered by
    /// `testLowGateCostsNothingWhenTheCommandFits` below: at reading sizes the
    /// gate is already open on the first measurement, so it costs no scroll at
    /// all. It only bites where the product would otherwise be approving
    /// something it had not shown.
    func testUnreadCommandBlocksEveryTier() {
        let gate = ReadGate()
        for risk in [RiskClass.low, .medium, .high] {
            XCTAssertFalse(
                gate.isOpen(at: risk),
                "\(risk) must not be answerable before its command has been on screen")
            XCTAssertEqual(
                gate.blockedReason(at: risk), "Scroll the command into view before deciding.",
                "and a shut gate says why, at every tier")
        }
    }

    /// **The LOW exemption, re-stated as the thing it was actually protecting.**
    ///
    /// Nobody wanted a scroll in front of a `Read`. What the gate costs at LOW
    /// is exactly one geometry report — the command block's bottom edge is above
    /// the action bar on the first frame at any reading size, so `hasSeenCommand`
    /// is already true when the card is drawn and `Allow` is live to the first
    /// tap. LOW and MEDIUM open on the same single fact; only HIGH asks for the
    /// second.
    func testLowGateCostsNothingWhenTheCommandFits() {
        var gate = ReadGate()
        gate = advance(gate, commandBottom: 400, provenanceBottom: 900, visibleBottom: 600)
        XCTAssertTrue(gate.isOpen(at: .low), "a command that fits opens the gate on sight")
        XCTAssertNil(gate.blockedReason(at: .low))
        XCTAssertTrue(gate.isOpen(at: .medium), "LOW and MEDIUM open on the same single fact")
        XCTAssertFalse(gate.isOpen(at: .high), "and HIGH still wants the class it was given")
    }

    /// The measured frame, as a value: the LOW card at AX5, where the block
    /// carrying `/Users/dev/app/README.md` runs from above the fold to y=830
    /// against an action bar whose top edge is y=636.
    func testLowCardWithTheCommandBehindTheBarIsShut() {
        var gate = ReadGate()
        gate = advance(gate, commandBottom: 830, provenanceBottom: 1_100, visibleBottom: 636)
        XCTAssertFalse(
            gate.isOpen(at: .low),
            "13 of 24 characters rendered is not a command anybody has read")
        XCTAssertEqual(
            gate.blockedReason(at: .low), "Scroll the command into view before deciding.")

        // …and the same card, scrolled.
        gate = advance(gate, commandBottom: 600, provenanceBottom: 870, visibleBottom: 636)
        XCTAssertTrue(gate.isOpen(at: .low))
    }

    /// HIGH is gated on the command **and** the class that explains it, stated
    /// separately rather than left to fall out of the layout: a face is about
    /// to authorise this, and that the one is drawn above the other is not
    /// evidence that both were seen.
    func testHighAlsoRequiresTheProvenance() {
        var gate = ReadGate()
        gate.hasSeenCommand = true
        XCTAssertTrue(gate.isOpen(at: .medium), "MEDIUM asks for the command and nothing else")
        XCTAssertFalse(gate.isOpen(at: .high))
        XCTAssertEqual(
            gate.blockedReason(at: .high),
            "Scroll to the end of the command block before deciding.")

        gate.hasSeenProvenance = true
        XCTAssertTrue(gate.isOpen(at: .high))
        XCTAssertNil(gate.blockedReason(at: .high))
    }

    /// The visibility test itself. A block counts as read when its **bottom
    /// edge** has been inside the visible region — where the visible region
    /// ends at the top of the pinned action bar, not at the bottom of the
    /// scroll view, because `safeAreaInset` draws the bar *over* the scroll
    /// view rather than shortening it. That is the exact geometry of
    /// A MEDIUM card at AX5: the command block's bottom was below the bar
    /// and the gate was armed anyway.
    func testVisibilityIsAViewportTest() {
        // Bottom edge above the fold: read.
        XCTAssertTrue(ReadGate.isVisible(bottom: 400, visibleBottom: 600))
        // Exactly on the fold: read.
        XCTAssertTrue(ReadGate.isVisible(bottom: 600, visibleBottom: 600))
        // One point under the action bar: not read.
        XCTAssertFalse(
            ReadGate.isVisible(bottom: 601, visibleBottom: 600),
            "a command the action bar is covering has not been seen")
        // The AX5 case: the block runs far past the bar.
        XCTAssertFalse(ReadGate.isVisible(bottom: 1_400, visibleBottom: 600))
        // Scrolled off the top: not a fresh arming, and the flag that matters
        // is sticky anyway.
        XCTAssertFalse(ReadGate.isVisible(bottom: -20, visibleBottom: 600))
    }

    /// **The first render, before any geometry has arrived.**
    ///
    /// Every way of not knowing has to answer the same way — unarmed — and
    /// none of them may be expressed as a sentinel the rest of the program can
    /// trip over. An `.infinity` in this state trapped a `Text` interpolation
    /// on the card and crash-looped the app; it would also have made
    /// "unmeasured" indistinguishable from "very far down the document".
    /// **The bug the real device found and every simulator run missed.**
    ///
    /// The four geometry facts arrive as four separate `onPreferenceChange`
    /// callbacks, in whatever order SwiftUI delivers them. `actionBarHeight`
    /// defaulted to `0` — a legitimate value meaning "covers nothing" — so a
    /// frame in which the viewport had been reported and the bar had not
    /// computed `visibleBottom` as the *whole* scroll view, which extends
    /// underneath the pinned bar. A command hidden behind that bar measured as
    /// visible, and because the flags are deliberately sticky, that single
    /// frame opened the gate permanently.
    ///
    /// Measured on an iPhone Air at AX5: a MEDIUM `Write` with 13 characters of
    /// its path on screen and `Allow` drawn as the enabled white primary. At
    /// MEDIUM there is no hold and no Face ID, so this gate was the only thing
    /// in the way. HIGH survived only because it also needs the provenance.
    ///
    /// "Not measured yet" and "measures zero" must therefore be different
    /// values, exactly as they already are for every edge on this probe.
    @MainActor
    func testTheBarsHeightArrivingLateCannotOpenTheGate() {
        let probe = ReadGateProbe()
        // The scroll view is 800pt tall; the bar covers the bottom 140pt; the
        // command block ends at 760pt — behind the bar, unreadable.
        probe.viewportBottom = 800

        XCTAssertNil(
            probe.visibleBottom,
            "the bar has not reported yet, so how much of the viewport is readable is unknown")

        var gate = ReadGate()
        if let visible = probe.visibleBottom,
            ReadGate.isVisible(bottom: 760, visibleBottom: visible) {
            gate.hasSeenCommand = true
        }
        XCTAssertFalse(
            gate.hasSeenCommand,
            "a command behind a bar that has not been measured has not been read")

        // The bar reports, and the same command is still unreadable.
        probe.actionBarHeight = 140
        XCTAssertEqual(probe.visibleBottom, 660)
        XCTAssertFalse(
            ReadGate.isVisible(bottom: 760, visibleBottom: 660),
            "760 is behind the bar; nobody has read it")
        XCTAssertFalse(gate.isOpen(at: .low))
        XCTAssertFalse(gate.isOpen(at: .medium))

        // **The values the device actually reported**, captured at the instant
        // the flag latched: the bar said it was 1pt tall on an early layout
        // pass, so the readable region came out 167pt too generous and a
        // command 4pt inside it counted as read.
        let transient = ReadGateProbe()
        transient.viewportBottom = 853
        transient.actionBarHeight = 1
        XCTAssertNil(
            transient.visibleBottom,
            "a bar shorter than one tappable control has not been laid out")

        let settled = ReadGateProbe()
        settled.viewportBottom = 853
        settled.actionBarHeight = 168
        XCTAssertEqual(settled.visibleBottom, 685)
        XCTAssertFalse(
            ReadGate.isVisible(bottom: 848, visibleBottom: 685),
            "the command the device showed ends behind the bar")
    }

    func testUnmeasuredGeometryLeavesTheGateShut() {
        XCTAssertFalse(
            ReadGate.isVisible(bottom: nil, visibleBottom: 600),
            "nobody measured the block, so nobody has read it")
        XCTAssertFalse(
            ReadGate.isVisible(bottom: 400, visibleBottom: 0),
            "a layout that has not happened cannot have shown anything")
        XCTAssertFalse(ReadGate.isVisible(bottom: .infinity, visibleBottom: 600))
        XCTAssertFalse(ReadGate.isVisible(bottom: 400, visibleBottom: .infinity))
        XCTAssertFalse(ReadGate.isVisible(bottom: .nan, visibleBottom: 600))
        XCTAssertFalse(ReadGate.isVisible(bottom: 400, visibleBottom: .nan))

        // …and a card whose geometry never arrives is a card that cannot be
        // approved **at all**, which is the safe direction. LOW used to be the
        // exception here, which meant an unmeasured card was approvable at the
        // one tier where a single tap is the whole interaction.
        let gate = ReadGate()
        XCTAssertFalse(gate.isOpen(at: .low))
        XCTAssertFalse(gate.isOpen(at: .medium))
        XCTAssertFalse(gate.isOpen(at: .high))
    }

    /// The transition the AX5 screenshots never reached: scroll, and the gate
    /// opens. Modelled as the view does it — sticky flags fed by successive
    /// geometry reports.
    func testGateArmsAfterScrollingToTheCommand() {
        var gate = ReadGate()
        let visibleBottom: CGFloat = 600

        // First layout at a large type size: the command block starts below the
        // fold and its provenance is far below that.
        var commandBottom: CGFloat = 900
        var provenanceBottom: CGFloat = 1_500
        gate = advance(gate, commandBottom, provenanceBottom, visibleBottom)
        XCTAssertFalse(gate.isOpen(at: .medium))
        XCTAssertFalse(gate.isOpen(at: .high))

        // Scrolled 400pt: the command is now on screen, the provenance is not.
        commandBottom -= 400
        provenanceBottom -= 400
        gate = advance(gate, commandBottom, provenanceBottom, visibleBottom)
        XCTAssertTrue(gate.isOpen(at: .medium), "MEDIUM arms as soon as the command is on screen")
        XCTAssertFalse(gate.isOpen(at: .high), "HIGH still wants the class it was given")

        // Scrolled to the end of the block.
        commandBottom -= 600
        provenanceBottom -= 600
        gate = advance(gate, commandBottom, provenanceBottom, visibleBottom)
        XCTAssertTrue(gate.isOpen(at: .high))

        // …and it stays armed once the block has scrolled off the top: reading
        // something twice is not a requirement.
        gate = advance(gate, -200, -50, visibleBottom)
        XCTAssertTrue(gate.isOpen(at: .high), "a flag, once set, stays set")
    }

    /// The view's `updateGate`, as a function.
    private func advance(
        _ gate: ReadGate, commandBottom: CGFloat?, provenanceBottom: CGFloat?,
        visibleBottom: CGFloat
    ) -> ReadGate {
        advance(gate, commandBottom, provenanceBottom, visibleBottom)
    }

    private func advance(
        _ gate: ReadGate, _ commandBottom: CGFloat?, _ provenanceBottom: CGFloat?,
        _ visibleBottom: CGFloat
    ) -> ReadGate {
        var next = gate
        if ReadGate.isVisible(bottom: commandBottom, visibleBottom: visibleBottom) {
            next.hasSeenCommand = true
        }
        if ReadGate.isVisible(bottom: provenanceBottom, visibleBottom: visibleBottom) {
            next.hasSeenProvenance = true
        }
        return next
    }
}

/// **The one number on the triage screen, and the one sentence that carries it.**
///
/// The fleet's display line and the accessory bar 631pt below it computed this
/// independently and counted different things — sessions against cards — in
/// word-for-word identical wording. Nothing in either test target referenced
/// either count.
final class FleetCountTests: XCTestCase {

    /// Built through the **real decoder**, from the daemon's own JSON.
    ///
    /// `SessionSummary` declares `init(from decoder:)` in its own body, which
    /// suppresses Swift's synthesised memberwise initialiser — so there is no
    /// twelve-label form to call, and inventing one in the type to satisfy a
    /// test would put a second way to construct a wire object into the program.
    /// Going through `JSONDecoder` is also the house rule for fixtures: a
    /// summary that stopped matching the wire fails here rather than diverging
    /// quietly.
    private func summary(_ id: String, blockedOn: [String]) -> SessionSummary {
        let ids = blockedOn.map { "\"\($0)\"" }.joined(separator: ",")
        let json = """
            {"session_uid":"uid-\(id)","session_id":"\(id)","tmux_session":"\(id)",\
            "cwd":"/Users/dev/\(id)","lifecycle":"live","link":"attached","last_seq":10,\
            "created_at":"2026-07-31T09:14:00.000Z","updated_at":"2026-07-31T09:14:00.000Z",\
            "blocked_on":[\(ids)]}
            """
        // A failure here is a decoder change, not a test bug, and it must be
        // loud: `try!` rather than an optional that quietly counts zero rows.
        // swiftlint:disable:next force_try
        return try! JSONDecoder().decode(SessionSummary.self, from: Data(json.utf8))
    }

    private func row(_ id: String, _ status: FleetStatus, blockedCount: Int) -> FleetRow {
        FleetRow(
            summary: summary(id, blockedOn: (0..<blockedCount).map { "r\($0)" }),
            status: status, title: id, subtitle: "", activity: nil, identity: id,
            capability: .control, blockedCount: blockedCount, lastEventAt: nil, cachedAt: nil)
    }

    /// The state that was invisible in the shipping fixture: three blocked
    /// agents, one of them holding two decisions. Counting rows gives 3;
    /// counting decisions gives 4, and 4 is the number of things a human has to
    /// answer.
    func testStackedDecisionsAreCountedNotRows() {
        let rows = [
            row("fx-1", .blocked, blockedCount: 1),
            row("fx-2", .blocked, blockedCount: 2),
            row("fx-3", .blocked, blockedCount: 1),
            row("fx-4", .running, blockedCount: 0),
        ]
        XCTAssertEqual(FleetCount.decisions(in: rows), 4)
        XCTAssertEqual(FleetCount.needsYou(4), "4 decisions need you")
    }

    /// **`blocked_on` before the card.** The daemon says a request is open one
    /// frame before the card carrying its contents reaches the stream, so a
    /// count taken from the Deck reads zero over a visibly blocked band. The
    /// headline would say "Nothing needs you" above an amber card.
    func testARowWhoseCardHasNotArrivedStillCounts() {
        let rows = [row("fx-1", .blocked, blockedCount: 0)]
        XCTAssertEqual(
            FleetCount.decisions(in: rows), 1,
            "a blocked row is holding at least one decision by definition")
    }

    func testNothingBlockedCountsNothing() {
        XCTAssertEqual(FleetCount.decisions(in: []), 0)
        XCTAssertEqual(
            FleetCount.decisions(in: [row("fx-1", .running, blockedCount: 0)]), 0)
    }

    /// Singular is a different sentence, not a pluralised one.
    func testTheSentenceAgreesWithItself() {
        XCTAssertEqual(FleetCount.needsYou(1), "1 decision needs you")
        XCTAssertEqual(FleetCount.needsYou(2), "2 decisions need you")
    }
}

/// **A fleet read off the disk states its age, whatever else is wrong.**
final class FleetFreshnessTests: XCTestCase {

    private let now = Date(timeIntervalSince1970: 1_785_466_205)

    // MARK: The launch grace

    /// The defect these pin: the cached banner rendered in the half-second
    /// between the cache painting the screen and the first live frame — an
    /// ominous amber flash on every healthy launch. The banner has to be
    /// *earned* by a launch that had a fair chance and delivered nothing.
    func testTheCachedBannerIsNotEarnedTheInstantTheCacheRestores() {
        XCTAssertFalse(
            FleetFreshness.cachedBannerEarned(restoredAt: now, connectingSince: nil, now: now),
            "restore and render happen in the same beat; the banner must not")
        XCTAssertFalse(
            FleetFreshness.cachedBannerEarned(
                restoredAt: now.addingTimeInterval(-FleetFreshness.launchGrace + 0.1),
                connectingSince: nil, now: now))
    }

    func testTheCachedBannerIsEarnedOnceTheGracePasses() {
        XCTAssertTrue(
            FleetFreshness.cachedBannerEarned(
                restoredAt: now.addingTimeInterval(-FleetFreshness.launchGrace),
                connectingSince: nil, now: now))
    }

    /// The reconnect edge: the restore grace is long spent, but a *fresh dial*
    /// is running. Without the second clock the banner would fill every
    /// reconnect's grace window with the same amber flash this rule exists to
    /// kill — the launch defect reappearing at every drop.
    func testAFreshDialHoldsTheBannerEvenWhenTheRestoreGraceIsSpent() {
        let longAgo = now.addingTimeInterval(-3600)
        XCTAssertFalse(
            FleetFreshness.cachedBannerEarned(
                restoredAt: longAgo, connectingSince: now.addingTimeInterval(-0.5), now: now))
        XCTAssertTrue(
            FleetFreshness.cachedBannerEarned(
                restoredAt: longAgo,
                connectingSince: now.addingTimeInterval(-FleetFreshness.launchGrace), now: now),
            "a dial that has used up its own grace no longer holds the banner")
        XCTAssertTrue(
            FleetFreshness.cachedBannerEarned(restoredAt: longAgo, connectingSince: nil, now: now),
            "no dial running holds nothing")
    }

    /// No restore, no banner — whatever else the model believes. A launch with
    /// nothing on disk has nothing to be stale about.
    func testNoRestoreNeverEarnsTheBanner() {
        XCTAssertFalse(
            FleetFreshness.cachedBannerEarned(restoredAt: nil, connectingSince: nil, now: now))
    }

    /// The render-harness seam stages `.distantPast`, because it photographs
    /// the earned state rather than the launch that leads to it.
    func testADistantPastRestoreIsAlwaysEarned() {
        XCTAssertTrue(
            FleetFreshness.cachedBannerEarned(
                restoredAt: .distantPast, connectingSince: nil, now: now))
    }

    /// The composition table the fleet's slot renders. Compound always wins
    /// when both facts exist — the cache's age is *part of* the link's story,
    /// never a second banner ("one banner, ever") — and the standalone cached
    /// notice exists only when the link is silent and the grace has passed.
    func testTheBannerChoiceTable() {
        XCTAssertEqual(
            FleetFreshness.bannerChoice(hasLink: true, hasStamp: true, earned: false), .compound,
            "a hard link failure carries the cache age immediately; it waits for no grace")
        XCTAssertEqual(
            FleetFreshness.bannerChoice(hasLink: true, hasStamp: false, earned: false), .link)
        XCTAssertEqual(
            FleetFreshness.bannerChoice(hasLink: false, hasStamp: true, earned: false), .none,
            "the launch flash: cache on screen, link quietly dialling — nothing is shown")
        XCTAssertEqual(
            FleetFreshness.bannerChoice(hasLink: false, hasStamp: true, earned: true), .cachedOnly)
        XCTAssertEqual(
            FleetFreshness.bannerChoice(hasLink: false, hasStamp: false, earned: false), .none)
    }

    func testLiveFleetIsNotStamped() {
        XCTAssertNil(
            FleetFreshness.stamp(
                cachedAt: now.addingTimeInterval(-7200), hasLiveFleet: true, now: now),
            "a fleet that came off the wire has nothing to admit to")
        XCTAssertNil(FleetFreshness.stamp(cachedAt: nil, hasLiveFleet: false, now: now))
    }

    func testCachedFleetStatesItsAge() {
        let stamp = FleetFreshness.stamp(
            cachedAt: now.addingTimeInterval(-7200), hasLiveFleet: false, now: now)
        XCTAssertEqual(stamp, "Showing the last known state, 2h old.")
    }

    /// The compound state. `offline` outranks `cached` in the one-banner ladder
    /// and is *right* to — but the fact it displaced is the one that says how
    /// much of the screen to believe, so it rides in the same message rather
    /// than being dropped or stacked as a second banner.
    func testTheCacheAgeSurvivesALinkFailure() {
        let stamp = FleetFreshness.stamp(
            cachedAt: now.addingTimeInterval(-7200), hasLiveFleet: false, now: now)
        let message = FleetFreshness.message(
            stamp: stamp,
            linkDetail: "Could not connect to the server. — trying ws:// next. — retrying in 7s.")
        XCTAssertEqual(
            message,
            "Showing the last known state, 2h old. "
                + "Could not connect to the server. — trying ws:// next. — retrying in 7s.")
        XCTAssertTrue(
            message?.hasPrefix("Showing the last known state") == true,
            "the age leads: it is what decides how much of the screen to trust")
    }

    func testALiveLinkFailureIsUnchanged() {
        XCTAssertEqual(
            FleetFreshness.message(stamp: nil, linkDetail: "Opening the connection."),
            "Opening the connection.")
        XCTAssertNil(FleetFreshness.message(stamp: nil, linkDetail: nil))
    }
}
