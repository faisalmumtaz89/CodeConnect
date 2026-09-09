import XCTest

@testable import CodeConnect

/// **The fixtures themselves, checked.**
///
/// A render catalogue is only as honest as the states it can reach, and every
/// Codex state is reached through `-CC_CODEX <name>`. Two lists therefore have
/// to agree — `CodexFixtures.State` here, and the string literals in
/// `RenderCatalog.codex`, which is an XCUITest target with no `@testable
/// import` and so cannot share the enum. This is where they are held together:
/// a state added on one side and not the other fails here, rather than
/// producing a render nobody notices is missing.
@MainActor
final class CodexFixtureTests: XCTestCase {

    /// **The render catalogue's own list**, read from the file that defines it
    /// rather than transcribed a third time.
    ///
    /// It was a hand-copied array — a third copy of the same names, which can
    /// stay green while the catalogue diverges, which is exactly the failure the
    /// test exists to prevent. `RenderCatalog` lives in an XCUITest target this
    /// one cannot import, so the names are read out of its source: the list and
    /// the thing it describes are then the same bytes.
    private static func catalogNames() throws -> Set<String> {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("CodeConnectRenderHarness/RenderCatalog.swift")
        guard let source = try? String(contentsOf: url, encoding: .utf8) else {
            XCTFail("RenderCatalog.swift is unreadable at \(url.path)")
            return []
        }
        // Every Codex scenario is `codexScenario(\n    "<name>",`.
        let pattern = try NSRegularExpression(
            pattern: #"codexScenario\(\s*\n\s*"([a-z0-9-]+)""#)
        let range = NSRange(source.startIndex..., in: source)
        return Set(
            pattern.matches(in: source, range: range).compactMap { match in
                Range(match.range(at: 1), in: source).map { String(source[$0]) }
            })
    }

    func testEveryStagedStateHasARenderScenarioAndViceVersa() throws {
        let catalog = try Self.catalogNames()
        XCTAssertEqual(
            Set(CodexFixtures.State.allCases.map(\.rawValue)), catalog,
            "a state that only one side knows about is a state nobody has looked at")
    }

    /// **Every frame decodes through the real `ServerMessage` decoder.**
    ///
    /// The rule the Claude fixtures already live by: a fixture that stopped
    /// matching the wire must fail to decode rather than quietly diverge. Here
    /// it also means a hand-written JSON literal with a stray comma fails in a
    /// unit test in a second, rather than as a render that could not be reached
    /// after a four-minute simulator run.
    func testEveryStateProducesDecodableFrames() {
        for state in CodexFixtures.State.allCases {
            let frames = CodexFixtures.frames(state: state)
            XCTAssertGreaterThanOrEqual(
                frames.count, 2, "\(state.rawValue): at least an ack and a fleet")
            guard case .helloAck(let ack) = frames.first else {
                return XCTFail("\(state.rawValue): the first frame is the ack")
            }
            let expected = state.capabilities
            XCTAssertEqual(ack.capabilities.codexInterrupt, expected.interrupt, state.rawValue)
            XCTAssertEqual(ack.capabilities.codexCompose, expected.compose, state.rawValue)
            XCTAssertTrue(ack.capabilities.hostsCodex, state.rawValue)

            guard case .sessions(let sessions) = frames[1] else {
                return XCTFail("\(state.rawValue): the second frame is the fleet")
            }
            // The captured stream keeps the identity the daemon recorded; every
            // hand-made state is `cx-1`.
            let codex = sessions.first { $0.sessionKey == CodexFixtures.fleetKey(for: state) }
            XCTAssertEqual(codex?.agent, .codex, state.rawValue)
            // Below minor 19 the field is absent and must read as `none`.
            XCTAssertEqual(
                codex?.codexLink.rawValue, state.link ?? "none", state.rawValue)
            XCTAssertEqual(ack.protocolMinor, state.protocolMinor, state.rawValue)
            // A Claude neighbour, so the badge and the Stop pill can be compared
            // row for row against a row that has neither.
            XCTAssertTrue(
                sessions.contains { $0.agent == .claude },
                "\(state.rawValue): every Codex render needs a Claude neighbour to align against")

            // And no frame is an `unknown` — which is what a typo'd `type` or a
            // renamed field would produce.
            for frame in frames {
                if case .unknown(let type) = frame {
                    XCTFail("\(state.rawValue): produced an unreadable \(type) frame")
                }
            }
        }
    }

    /// **The cards verify for real.** A fixture card whose
    /// `SHA-256(display_text) != payload_hash` renders the *blocked* path — a
    /// banner and two dead controls — rather than the card the render is for,
    /// and every earlier photograph would have been of the wrong screen.
    func testEveryFixtureCardPassesTheHashGate() {
        for state in CodexFixtures.State.allCases {
            for frame in CodexFixtures.frames(state: state) {
                guard case .event(let event) = frame, let card = event.approvalCard else { continue }
                XCTAssertTrue(
                    card.verification.hashMatchesDisplayText,
                    "\(state.rawValue): the card must verify, or the render is of a banner")
                XCTAssertTrue(
                    card.verification.renderMatchesDisplayText,
                    "\(state.rawValue): the structured render must reproduce display_text")
                XCTAssertFalse(
                    CodexCard.options(in: card.toolInput).isEmpty,
                    "\(state.rawValue): a Codex card is answered by its options")
            }
        }
    }

    /// **Worst-case real data, not idealised.** The three measurements the
    /// verification bar's first rule is about, pinned so a future tidy-up cannot
    /// quietly shorten them.
    func testTheFixturesCarryTheMeasuredWorstCase() {
        XCTAssertEqual(
            CodexFixtures.longCommand.count, 69,
            "the longest command in the corpus, from approval-amendment-labels-0.153.txt")
        XCTAssertEqual(
            CodexFixtures.longAmendmentLabel.count, 110,
            "the longest amendment label the daemon actually offers")
        XCTAssertEqual(
            CodexFixtures.compositeRequestID.count, 159,
            "the real composite id, verbatim from approval-card-0.153.json")
    }

    /// **The composite id is the card fixture's own id, not a look-alike.**
    ///
    /// `compositeRequestID` is a literal in the app target — `approval-card
    /// -0.153.json` ships only in the test bundle, and bundling a test fixture
    /// into the shipped app to read one string from it would be the worse
    /// trade. So the literal is pinned here instead, and this is the assertion
    /// the length check could not make: when the card fixture was rebuilt from
    /// the real `approval-0.153.jsonl` frames, the new id was **also** 159
    /// characters, so `count == 159` stayed green over an id belonging to a
    /// card that no longer existed.
    func testTheCompositeIdIsTheCardFixturesOwn() throws {
        let object = try XCTUnwrap(
            try JSONDecoder()
                .decode(JSONValue.self, from: Self.bundledCardFixture())
                .objectValue,
            "approval-card-0.153.json did not decode as an object")
        let ids = object.keys.sorted().compactMap { object[$0]?["request_id"]?.stringValue }
        XCTAssertEqual(
            ids.count, object.keys.count,
            "every card in approval-card-0.153.json must carry a request_id: \(object.keys.sorted())")
        XCTAssertTrue(
            ids.contains(CodexFixtures.compositeRequestID),
            """
            CodexFixtures.compositeRequestID is not any card's id in \
            approval-card-0.153.json. The fixture was regenerated and the \
            literal was not. Its ids are: \(ids)
            """)
    }

    private static func bundledCardFixture() throws -> Data {
        let bundle = Bundle(for: CodexFixtureTests.self)
        guard let url = bundle.url(forResource: "approval-card-0.153", withExtension: "json"),
            let data = try? Data(contentsOf: url)
        else {
            XCTFail("approval-card-0.153.json is not in the test bundle")
            throw CocoaError(.fileNoSuchFile)
        }
        return data
    }

    /// **Every refusal sentence the corpus shows a reader is one the daemon
    /// really sends.**
    ///
    /// The fixtures stage `InterruptResult.rejected` and `ComposeResult
    /// .indeterminate` with sentences typed here, and those sentences are what
    /// the render pass photographs. A typed approximation is the same defect as
    /// a stale bundled fixture wearing different clothes: `compose-indeterminate`
    /// carried "that message was written and what became of it is not known",
    /// which is no row the daemon has ever had. It classified the same way by
    /// luck — the row it approximates is `permanent` too, and an unmatched
    /// sentence also declines to grey — so nothing in the suite noticed.
    ///
    /// Matched against `fixtures/codex/refusal-sentences.json`, the source, for
    /// the same reason `CodexRefusalClassifierTests` reads the source: a copy
    /// compared with a copy proves nothing.
    func testEveryStagedRefusalIsASentenceTheDaemonReallySends() throws {
        struct Wire: Decodable {
            struct Row: Decodable {
                let id: String
                let text: String
            }
            let sentences: [Row]
        }
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let url = repoRoot.appendingPathComponent("fixtures/codex/refusal-sentences.json")
        guard let data = try? Data(contentsOf: url) else {
            throw XCTSkip("fixtures/codex/refusal-sentences.json is not reachable from this bundle")
        }
        let rows = try JSONDecoder().decode(Wire.self, from: data).sentences
        // Only rows with no token can be compared verbatim; a template is not a
        // sentence, and the fixtures stage sentences.
        let verbatim = Dictionary(
            rows.filter { !$0.text.contains("{") }.map { ($0.text, $0.id) },
            uniquingKeysWith: { first, _ in first })

        var checked = 0
        for state in CodexFixtures.State.allCases {
            var staged: [(what: String, sentence: String)] = []
            if case .rejected(let reason)? = CodexFixtures.interruptResult(state) {
                staged.append(("interrupt rejected", reason))
            }
            if case .indeterminate(let reason)? = CodexFixtures.interruptResult(state) {
                staged.append(("interrupt indeterminate", reason))
            }
            if case .rejected(let reason)? = CodexFixtures.composeResult(state) {
                staged.append(("compose rejected", reason))
            }
            if case .indeterminate(let reason)? = CodexFixtures.composeResult(state) {
                staged.append(("compose indeterminate", reason))
            }
            for one in staged {
                checked += 1
                XCTAssertNotNil(
                    verbatim[one.sentence],
                    """
                    \(state.rawValue)'s \(one.what) sentence is not one the \
                    daemon sends: "\(one.sentence)". Every refusal a render \
                    photographs must be a row of \
                    fixtures/codex/refusal-sentences.json, verbatim.
                    """)
            }
        }
        // A positive control: this must have had something to check.
        XCTAssertGreaterThanOrEqual(
            checked, 4, "only \(checked) staged refusals found; this test is not looking at them")
    }

    /// A staged card is in `blocked_on` only while it is still unanswered — a
    /// resolved card counted as blocking would put the run in the wrong band and
    /// make the fleet's headline count wrong.
    func testAResolvedFixtureIsNotStillBlocking() {
        for state in CodexFixtures.State.allCases {
            guard case .sessions(let sessions) = CodexFixtures.frames(state: state)[1],
                let codex = sessions.first(where: {
                    $0.sessionKey == CodexFixtures.fleetKey(for: state)
                })
            else { return XCTFail("\(state.rawValue): no Codex session") }
            XCTAssertEqual(
                codex.blockedOn.isEmpty, !state.stagesACard,
                "\(state.rawValue): blocked_on must match whether a card is open")
        }
    }

    /// A state that stages a mutation outcome stages exactly one, so a render
    /// cannot photograph two banners fighting for one slot.
    func testEachStateStagesAtMostOneMutationOutcome() {
        for state in CodexFixtures.State.allCases {
            let staged =
                (CodexFixtures.interruptResult(state) != nil ? 1 : 0)
                + (CodexFixtures.composeResult(state) != nil ? 1 : 0)
            XCTAssertLessThanOrEqual(staged, 1, state.rawValue)
        }
    }

    /// **A resolved state leaves NO pending card**, driven through the real
    /// `TimelineBuilder`.
    ///
    /// Added after a render caught it: `codex-resolved-at-mac` photographed a
    /// row still reading `NEEDS YOU … waiting 1m25s` with a live `Review`
    /// button, on a card the fixture had already resolved. The unit tests for
    /// the builder were green, so the defect was in what the fixture produced —
    /// which is exactly the class of thing only a render finds, and exactly the
    /// class of thing that should then get a test that runs in a second.
    func testAResolvedStateLeavesNoPendingCard() {
        for state in CodexFixtures.State.allCases {
            let events: [Event] = CodexFixtures.frames(state: state).compactMap { frame in
                if case .event(let event) = frame { return event }
                return nil
            }
            let pending = TimelineBuilder.build(events).compactMap(\.pendingApproval)
            XCTAssertEqual(
                pending.isEmpty, !state.stagesACard,
                "\(state.rawValue): a resolved card must not still be waiting on a human")
        }
    }

    /// **A fixture adds the fact it is for, and not a fault as well.**
    ///
    /// `SessionState` reads a jump in `seq` as events it never received and says
    /// so — correctly. A fixture that numbers 10, 20, 30 therefore prints
    /// `RESYNCED, 18 EVENTS NOT SHOWN` across every screen it appears on, which
    /// is a second, false story competing with the one the render is about.
    /// Caught by looking at `codex-resolved-at-mac--L.png`.
    func testTheFixtureSequenceIsContiguous() {
        for state in CodexFixtures.State.allCases {
            let seqs: [UInt64] = CodexFixtures.frames(state: state).compactMap { frame in
                if case .event(let event) = frame { return event.seq }
                return nil
            }
            // A state may legitimately stage no events at all (the two
            // old-daemon fixtures do), and `1...0` is not a range.
            XCTAssertEqual(
                seqs, seqs.isEmpty ? [] : Array(1...UInt64(seqs.count)),
                "\(state.rawValue): a hole in seq is a gap banner over an unrelated render")
        }
    }

    /// **The ceiling fixture really is the ceiling**, and parsing it is cheap
    /// enough to happen during `body`.
    ///
    /// A valid card carries `MAX_CHANGES` (32) files sharing
    /// `MAX_TOTAL_DIFF_BYTES` (128 KiB). Every row used to be built eagerly and
    /// every diff reparsed on each `body` evaluation, and no fixture in the
    /// suite was bigger than three tiny hunks — so the cost of the worst legal
    /// card was never measured. This measures it.
    func testTheCeilingCardIsBoundedAndParsesQuickly() throws {
        let card = try XCTUnwrap(
            CodexFixtures.frames(state: .cardCeiling).compactMap { frame -> ApprovalCard? in
                if case .event(let event) = frame { return event.approvalCard }
                return nil
            }.first)

        let changes = CodexCard.changes(in: card.toolInput)
        XCTAssertEqual(changes.count, 32, "MAX_CHANGES")
        let bytes = changes.reduce(0) { $0 + $1.diff.utf8.count }
        XCTAssertGreaterThan(bytes, 120_000, "within a few KiB of MAX_TOTAL_DIFF_BYTES")
        XCTAssertLessThanOrEqual(bytes, 131_072, "and never over it")

        // The parse the card runs per file, timed over all 32.
        let started = Date()
        var lines = 0
        for change in changes {
            lines += CodexCard.parsedDiff(for: change)?.hunks
                .reduce(0) { $0 + $1.lines.count } ?? 0
        }
        let elapsed = Date().timeIntervalSince(started)
        XCTAssertGreaterThan(lines, 1_000, "there really are thousands of rows behind the fold")
        XCTAssertLessThan(
            elapsed, 2.0,
            "parsing the whole ceiling took \(String(format: "%.2f", elapsed))s — the card folds "
                + "past four files, so a body evaluation pays a fraction of this")
    }

    /// **The fold bounds the file list; it did not bound the card.**
    ///
    /// Found by looking at `codex-card-ceiling--L`, which is what a render pass
    /// is for. Four files is what the fold shows, and at the ceiling four files
    /// are ~4 KiB of diff each: the photograph was a wall of `let value88 =
    /// compute(88)` with the option list five screens below it, on the one
    /// surface whose entire job is to be read before a decision. A card that
    /// cannot be answered without a minute of scrolling is not bounded, whatever
    /// the file count says.
    ///
    /// So the card carries a **row budget**, the way `DiffView` already does for
    /// the diff sheet, and it is a property of the card rather than of the fold:
    /// however many files are shown, the rows built for them are capped and the
    /// remainder is stated.
    func testTheCardsDiffRowsAreBoundedWhateverTheFoldShows() throws {
        let card = try XCTUnwrap(
            CodexFixtures.frames(state: .cardCeiling).compactMap { frame -> ApprovalCard? in
                if case .event(let event) = frame { return event.approvalCard }
                return nil
            }.first)
        let changes = CodexCard.changes(in: card.toolInput)

        // Folded (four files) and fully expanded (all 32) are the same bound.
        for shown in [Array(changes.prefix(4)), changes] {
            let plan = CodexCard.diffPlan(for: shown)
            XCTAssertLessThanOrEqual(
                plan.allowances.reduce(0, +), CodexCard.diffRowBudget,
                "\(shown.count) files drew more rows than the card's budget")
            XCTAssertGreaterThan(
                plan.heldBack, 0, "the ceiling has more rows than any budget worth having")
            XCTAssertEqual(
                plan.allowances.count, shown.count, "every shown file gets an allowance")
        }

        // A small card is untouched: nothing is held back, and nothing is capped.
        let minimal = CodexCard.changes(
            in: try XCTUnwrap(
                CodexFixtures.frames(state: .cardMinimal).compactMap { frame -> ApprovalCard? in
                    if case .event(let event) = frame { return event.approvalCard }
                    return nil
                }.first
            ).toolInput)
        let small = CodexCard.diffPlan(for: minimal)
        XCTAssertEqual(small.heldBack, 0, "a one-file card is nowhere near the budget")
    }

    /// **Stop is offered only where a turn is running** — the one honest hide.
    func testOnlyARunningStateHasATurnToStop() {
        for state in CodexFixtures.State.allCases {
            let events: [Event] = CodexFixtures.frames(state: state).compactMap { frame in
                if case .event(let event) = frame { return event }
                return nil
            }
            XCTAssertEqual(
                CodexTurnTracker.runningTurn(in: events) != nil, state.turnRunningAtRender,
                "\(state.rawValue): the staged turn and the derived one must agree")
        }
    }

    /// **The CONTRACT, not the Boolean against itself.**
    ///
    /// The old self-consistency test compared `hasRunningTurn` to the events
    /// produced from that same Boolean, so it could not catch a fixture that
    /// described a state the daemon cannot produce — and three did. These are
    /// the rules from `ws.rs`, checked against what each state claims.
    func testTheFixturesObeyTheContractTheyClaimToStage() {
        for state in CodexFixtures.State.allCases {
            switch state {
            case .retiredItemCompleted:
                // `item_completed` is deliberately NOT `turn_completed`: the
                // item finished and the turn is still running. It was staged
                // idle, with a `turn_complete` — the opposite of the cause.
                XCTAssertTrue(
                    state.turnRunningAtRender,
                    "item_completed means the turn is still running")
            case .clearedTurnAborted, .clearedTurnCompleted:
                XCTAssertFalse(state.turnRunningAtRender, "\(state.rawValue) ends the turn")
                XCTAssertTrue(
                    state.cardHadATurn,
                    "a card cleared by its turn ending was raised under that turn")
            case .cardMinimal, .cardCommandWorst, .cardTwoOptions, .cardFileChangeWide:
                // A pending card is a question a running turn is waiting on.
                XCTAssertTrue(state.stagesACard)
                XCTAssertTrue(
                    state.turnRunningAtRender, "a pending card implies a running turn")
            case .composeStarted:
                // `started` means Codex was **idle**; preloading a running turn
                // before the stub answers contradicts the arm being staged.
                XCTAssertFalse(
                    state.turnRunningAtRender, "started means Codex was idle beforehand")
            case .daemonMinor16, .daemonMinor17:
                XCTAssertLessThan(state.protocolMinor, 19, "\(state.rawValue) is an OLD daemon")
                XCTAssertNil(state.link, "a daemon below 19 has no codex_link to send")
            default:
                break
            }
        }
    }

    /// **Press and answer are two facts, and the gate sits between them.**
    ///
    /// The render harness presses Stop and Compose for real: `applyCodexFixture`
    /// drives `stopCodexTurn`/`composeToCodex`, which since F1 refuse on the
    /// send path unless the summary's `codex_link` is `subscribed` and the
    /// capability is advertised. So a fixture is only honest if the two agree,
    /// in both directions:
    ///
    ///   * a state that **stages a daemon answer** must be one the gate lets
    ///     through, or the render photographs the phone's own refusal while
    ///     claiming to show the Mac's — which is how `codex-stop-link-down`
    ///     came to certify nothing;
    ///   * a state that **presses with no answer staged** must be one the gate
    ///     stops, or the press is a frame nobody will ever answer, and the
    ///     render hangs on it for the whole request timeout.
    ///
    /// The second clause is also the truthful shape of D7: whether the Mac's
    /// control link to Codex is up is a fact only the Mac holds at the moment of
    /// the send, so "the link went down between the summary and the frame" is
    /// the only way its refusal sentence can ever reach a phone.
    func testPressAndStagedAnswerAgreeWithTheSendPathGate() throws {
        for state in CodexFixtures.State.allCases {
            let caps = state.capabilities
            let link = CodexLinkState(wire: state.link ?? "none")
            let stopBlocked = CodexProse.stopUnavailable(
                agent: .codex, daemonHonoursStop: caps.interrupt,
                link: link, runningTurn: CodexFixtures.turnID)
            let composeBlocked = CodexProse.composeUnavailable(
                agent: .codex, daemonUnderstandsCompose: caps.compose, link: link)

            if CodexFixtures.interruptResult(state) != nil {
                XCTAssertTrue(
                    CodexFixtures.pressesStop(state),
                    "\(state.rawValue) stages a stop answer nothing ever asks for")
                XCTAssertNil(
                    stopBlocked,
                    "\(state.rawValue) stages a stop answer the send path would refuse")
            } else if CodexFixtures.pressesStop(state) {
                XCTAssertNotNil(
                    stopBlocked,
                    "\(state.rawValue) presses Stop with no answer staged, so the gate must stop it")
            }

            if CodexFixtures.composeResult(state) != nil {
                XCTAssertTrue(
                    CodexFixtures.pressesCompose(state),
                    "\(state.rawValue) stages a compose answer nothing ever asks for")
                XCTAssertNil(
                    composeBlocked,
                    "\(state.rawValue) stages a compose answer the send path would refuse")
            } else if CodexFixtures.pressesCompose(state) {
                XCTAssertNotNil(
                    composeBlocked,
                    "\(state.rawValue) presses send with no answer staged, so the gate must stop it")
            }
        }
    }
}


/// **Every fixture the iOS side duplicates, tied to the one it was copied from.**
///
/// Five files under `fixtures/codex/` exist in two or three places — the daemon
/// emits or captures them, the app bundles some of them because a phone has no
/// repo, and the test bundle carries the rest. Nothing compared any copy to its
/// source: not Rust, not Swift, not the `.pbxproj` (the targets use
/// file-system-synchronized groups, so a resource is included by *sitting in the
/// directory* — there is no copy phase to hang a check on), and not CI.
///
/// That is not a hypothetical. It is how `refusal-sentences.json` sat two rows
/// behind the daemon in both iOS copies while `CodexRefusalClassifierTests`
/// compared one copy to the other and stayed green — leaving the composer live
/// against a link `compose_start_in_flight` was refusing. And it is why
/// `compose-0.153.4.jsonl` was found carrying a *different capture's* item id
/// from the source's, which nobody had noticed either.
///
/// So this walks the directories rather than naming files: a sixth duplicated
/// fixture is covered the day it is added, without anyone remembering to add it
/// here. Skipped, not failed, where the repo is unreachable — the same rule
/// `CodexRefusalClassifierTests` and `AgentSeamRenderingTests` already follow.
final class CodexFixtureProvenanceTests: XCTestCase {

    /// The iOS directories that hold copies, and what each one is for.
    private static let mirrors: [(path: String, why: String)] = [
        ("CodeConnect/Resources", "the app bundle — the phone reads these with no repo to check"),
        ("CodeConnectTests/Resources", "the test bundle — the unit tests' evidence base"),
    ]

    func testEveryDuplicatedFixtureIsByteIdenticalToItsSource() throws {
        let repoRoot = try Self.repoRootOrSkip()
        let source = repoRoot.appendingPathComponent("fixtures/codex")
        var compared: [String] = []

        for mirror in Self.mirrors {
            let directory = repoRoot.appendingPathComponent("ios/\(mirror.path)")
            let names = try FileManager.default.contentsOfDirectory(atPath: directory.path)
            for name in names.sorted() where !name.hasPrefix(".") {
                let origin = source.appendingPathComponent(name)
                guard let expected = try? Data(contentsOf: origin) else { continue }
                let copy = try Data(contentsOf: directory.appendingPathComponent(name))
                compared.append("\(mirror.path)/\(name)")
                XCTAssertEqual(
                    copy, expected,
                    """
                    ios/\(mirror.path)/\(name) has drifted from \
                    fixtures/codex/\(name), which is where it is generated or \
                    captured. This copy is \(mirror.why), so a stale one is a \
                    phone reasoning about a wire the daemon no longer speaks. \
                    Run: cp fixtures/codex/\(name) ios/\(mirror.path)/\(name)
                    """)
            }
        }

        // **A positive control.** A wrong repo root, a renamed directory or a
        // moved fixtures tree would compare nothing at all and report success,
        // which is the same silent pass this test exists to end.
        XCTAssertTrue(
            compared.contains("CodeConnect/Resources/refusal-sentences.json"),
            """
            this check compared \(compared.count) files and not the app's \
            refusal-sentences.json, so it is not looking where it thinks it is: \
            \(compared)
            """)
        XCTAssertGreaterThanOrEqual(
            compared.count, 5, "only \(compared.count) duplicated fixtures found: \(compared)")
    }

    /// `#filePath` is `<repo>/ios/CodeConnectTests/<thisfile>.swift` → up 3.
    private static func repoRootOrSkip() throws -> URL {
        let root = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        guard FileManager.default.fileExists(atPath: root.appendingPathComponent("fixtures/codex").path)
        else {
            throw XCTSkip("fixtures/codex is not reachable from this bundle; this gate needs a checkout")
        }
        return root
    }
}
