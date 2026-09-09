import XCTest

@testable import CodeConnect

/// **K4 — the phone's greying rule, decided by the daemon's own categories.**
///
/// The wire carries no refusal code, so the phone reads the sentence. It used to
/// read it by matching three English phrases, and the shared fixture proved that
/// wrong on the day it landed: the daemon writes 20 link-state refusals today
/// and those three clauses matched 8. Twelve sentences saying the link was
/// coming back would have left the control live against a link that was
/// reconnecting — with every test in the suite still green.
///
/// `fixtures/codex/refusal-sentences.json` is emitted by `ccd` from the one
/// place each sentence is written, and `ccd`'s own gate test keeps it
/// byte-identical to the build. **That file — not either iOS copy of it — is
/// what these tests read**, resolved from `#filePath` the way
/// `CodexFixtureTests` and `AgentSeamRenderingTests` already reach the repo.
/// The two iOS copies exist so the app and the test bundle work on a phone with
/// no repo; `testBothIOSCopiesAreByteIdenticalToTheDaemonsSource` is what keeps
/// them from drifting, and it is the check that was missing when both copies
/// went stale at 68 rows against a 70-row source.
///
/// Counts are asserted against the file's **own `counts` block**, never against
/// a literal in this file: a regenerated fixture then moves both sides at once,
/// and a fixture whose header disagrees with its own rows fails here.
@MainActor
final class CodexRefusalClassifierTests: XCTestCase {

    private struct Wire: Decodable {
        struct Row: Decodable {
            let id: String
            let category: String
            let text: String
        }
        let sentences: [Row]
        let tokens: [String: String]
        /// The daemon's own tally, per category plus `total`. Asserted against
        /// the rows below rather than against a number typed here.
        let counts: [String: Int]
    }

    /// **Every sentence, in the category the daemon gave it.**
    ///
    /// `link_state` and `transient_local` grey — both mean *this did not happen
    /// and the same ask may work shortly*. `permanent` and `wire_code` do not;
    /// several of them say "it will not be sent again", and a bounded grey
    /// followed by a live control would invite exactly that.
    func testEverySentenceClassifiesAsItsDaemonCategorySays() throws {
        let wire = try Self.repoFixture()
        var seen: Set<String> = []

        for row in wire.sentences {
            let rendered = Self.fill(row.text, tokens: wire.tokens)
            let greys = CodexControls.isLinkStateRefusal(rendered)
            let category = CodexRefusals.Category(wire: row.category)
            seen.insert(row.category)

            XCTAssertEqual(
                greys, category.greys,
                """
                \(row.id) is “\(row.category)”, so the control must \
                \(category.greys ? "grey and come back" : "stay live"): \(rendered)
                """)
        }

        // Not a count — the file is being regenerated — but the two sides of the
        // rule must both be exercised, or this proves only one of them.
        XCTAssertTrue(
            seen.contains { CodexRefusals.Category(wire: $0).greys },
            "no greying category in the fixture: \(seen.sorted())")
        XCTAssertTrue(
            seen.contains { !CodexRefusals.Category(wire: $0).greys },
            "no non-greying category in the fixture: \(seen.sorted())")
    }

    /// **Every sentence is recognised at all.** A row the matcher cannot find is
    /// a row whose category never applies — silently, and in the safe direction,
    /// which is exactly how the old needles hid ten misses.
    func testEverySentenceIsMatchedToItsOwnRow() throws {
        let wire = try Self.repoFixture()
        for row in wire.sentences {
            let rendered = Self.fill(row.text, tokens: wire.tokens)
            guard let match = CodexRefusals.match(rendered) else {
                XCTFail("no row matched \(row.id): \(rendered)")
                continue
            }
            XCTAssertEqual(
                match.category, CodexRefusals.Category(wire: row.category),
                "\(row.id) matched a row of a different category (\(match.id))")
        }
    }

    /// **The header's tally is the rows' tally.**
    ///
    /// The count lives in the fixture, not here. A regenerated file moves both
    /// sides of this assertion at once — which is the point: no literal in this
    /// test can go stale, and a fixture whose `counts` block disagrees with its
    /// own `sentences` array (a hand-edited copy, a truncated write) fails
    /// before any classification is asserted. Today that is
    /// `link_state: 20, permanent: 43, transient_local: 5, wire_code: 2,
    /// total: 70`; tomorrow it is whatever the daemon emits.
    func testTheFilesOwnCountsMatchItsOwnRows() throws {
        let wire = try Self.repoFixture()
        var tally: [String: Int] = ["total": wire.sentences.count]
        for row in wire.sentences { tally[row.category, default: 0] += 1 }
        XCTAssertEqual(
            tally, wire.counts,
            "refusal-sentences.json's counts header disagrees with its own rows")
    }

    /// **Both iOS copies are the daemon's source file, byte for byte.**
    ///
    /// This is the seam that was missing. The classifier reads a bundled
    /// resource because it has to work on a phone with no repo, and the old
    /// drift check compared the app's copy to the *test bundle's* copy — two
    /// derivatives of the same stale `cp`, so it could not fail. Both sat at 68
    /// rows against a 70-row source for as long as nobody looked, and the two
    /// rows missing were `compose_start_in_flight` and `compose_no_turn_to_steer`
    /// — link-state refusals the daemon really sends, which the phone therefore
    /// did not grey.
    ///
    /// So the comparison is against `fixtures/codex/refusal-sentences.json`
    /// itself. Skipped, not failed, when the repo is unreachable (a device run,
    /// a sandboxed bundle); on any checkout it is a hard gate, and a stale `cp`
    /// fails the unit target here.
    func testBothIOSCopiesAreByteIdenticalToTheDaemonsSource() throws {
        let source = try Self.sourceFixtureOrSkip()
        XCTAssertEqual(
            try Self.appCopy(), source,
            """
            ios/CodeConnect/Resources/refusal-sentences.json has drifted from \
            fixtures/codex/refusal-sentences.json. The app classifies live \
            refusals with this copy, so a missing row is a control that stays \
            live against a link the daemon is refusing. Run: cp \
            fixtures/codex/refusal-sentences.json \
            ios/CodeConnect/Resources/refusal-sentences.json
            """)
        XCTAssertEqual(
            try Self.testBundleCopy(), source,
            """
            ios/CodeConnectTests/Resources/refusal-sentences.json has drifted \
            from fixtures/codex/refusal-sentences.json. Run: cp \
            fixtures/codex/refusal-sentences.json \
            ios/CodeConnectTests/Resources/refusal-sentences.json
            """)
    }

    /// **The two rows the stale copy was missing, named.**
    ///
    /// `compose_start_in_flight` and `compose_no_turn_to_steer` are returned by
    /// `compose_route` in `mac/ccd/src/codex_link.rs` — plain arms, no `cfg`,
    /// on the ordinary first-turn path. Both are `link_state`: the turn is
    /// coming, and the same message a moment later will be written. With the
    /// stale 68-row bundle the classifier had never heard of either, the
    /// unmatched arm declined to grey, and the composer stayed live against a
    /// link mid-first-turn.
    ///
    /// Named explicitly rather than left to the loop above, because these two
    /// are the regression: if a future regeneration drops them the loop still
    /// passes over 68 other rows and this does not.
    func testTheFirstTurnLinkStateRefusalsGrey() throws {
        let wire = try Self.repoFixture()
        for id in ["compose_start_in_flight", "compose_no_turn_to_steer"] {
            let row = try XCTUnwrap(
                wire.sentences.first { $0.id == id },
                "\(id) is not in refusal-sentences.json — the daemon still sends it")
            XCTAssertEqual(row.category, "link_state", "\(id) must be a link-state refusal")
            let rendered = Self.fill(row.text, tokens: wire.tokens)
            XCTAssertTrue(
                CodexControls.isLinkStateRefusal(rendered),
                """
                \(id) did not grey the composer: \(rendered) — the app's \
                bundled refusal-sentences.json does not contain this row.
                """)
        }
    }

    /// A sentence from no row does **not** grey. Failing open here would hold a
    /// control dead for ten seconds on a refusal the reader could act on now.
    func testAnUnrecognisedRefusalDoesNotGrey() {
        XCTAssertFalse(CodexControls.isLinkStateRefusal("some future wording nobody has seen"))
        XCTAssertFalse(CodexControls.isLinkStateRefusal(""))
        // And a row's words buried inside a longer, different sentence do not
        // claim it: the template's first literal segment anchors the start.
        XCTAssertFalse(
            CodexControls.isLinkStateRefusal(
                "the operator said: there is no live link to this Codex session, so nothing "
                    + "was sent; stop the turn at the Mac — but that was yesterday"))
    }

    /// **G7 — a template that ends in literal text must end the sentence.**
    ///
    /// Matching was "these literal segments appear in this order", start-
    /// anchored when the template opens with a literal. Nothing held the tail:
    /// a sentence that quoted a link-state row and then went on saying
    /// something else still matched it, and the control greyed for ten seconds
    /// on a refusal that was not a link-state refusal at all. The daemon's own
    /// rows are exact sentences, so the phone should require an exact sentence:
    /// anchored at both ends unless a token sits at that end.
    func testATemplateEndingInLiteralTextMustEndTheSentence() throws {
        let wire = try Self.repoFixture()
        // A real link-state row, rendered, with prose appended.
        let linkState = try XCTUnwrap(
            wire.sentences.first { $0.category == "link_state" && !$0.text.hasSuffix("}") })
        let rendered = Self.fill(linkState.text, tokens: wire.tokens)

        XCTAssertTrue(
            CodexRefusals.greys(rendered), "the sentence itself still greys")
        XCTAssertFalse(
            CodexRefusals.greys(rendered + " — but that was an hour ago and it is gone for good"),
            "a link-state row's words inside a longer, different sentence are not that refusal")
        XCTAssertNil(
            CodexRefusals.match(rendered + ", and the session has since been deleted"),
            "no row may claim a sentence that continues past its own last word")

        // And a template that *ends* in a token still matches whatever fills
        // it — the anchor is on literal tails only.
        let tokenTailed = wire.sentences.first { $0.text.hasSuffix("}") }
        if let tokenTailed {
            XCTAssertNotNil(
                CodexRefusals.match(Self.fill(tokenTailed.text, tokens: wire.tokens)),
                "\(tokenTailed.id) ends in a token, so its tail cannot be anchored")
        }
    }

    // MARK: Fixture access

    /// **The daemon's own file when the repo is there; the test bundle's copy
    /// when it is not.**
    ///
    /// The expectations these tests assert must come from a file the phone does
    /// not also ship, or the test proves only that a copy agrees with itself —
    /// which is exactly how 68 rows passed against a 70-row daemon. So the
    /// source of truth is `fixtures/codex/refusal-sentences.json`, reached from
    /// `#filePath` the way `CodexFixtureTests.catalogNames()` and
    /// `AgentSeamRenderingTests.testTheFixtureFileMatchesThePinnedStrings()`
    /// already reach the repo tree. The subject under test is always the **app
    /// bundle's** copy, because `CodexRefusals.all` loads from `Bundle.main` —
    /// so a stale app copy fails these assertions, which is the whole point.
    ///
    /// `#filePath` is unreachable on a device run or a relocated bundle. There
    /// the test bundle's checked-in copy stands in, so the classification rules
    /// still get exercised; only the byte-identity gate skips, and it is the one
    /// assertion that has nothing to say without a repo.
    private static func repoFixture() throws -> Wire {
        let data = try sourceFixtureData() ?? testBundleCopy()
        return try JSONDecoder().decode(Wire.self, from: data)
    }

    /// `#filePath` is `<repo>/ios/CodeConnectTests/<thisfile>.swift` → up 3 to
    /// the repo root. `nil` when the checkout is not on this filesystem.
    private static func sourceFixtureData() throws -> Data? {
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let url = repoRoot.appendingPathComponent("fixtures/codex/refusal-sentences.json")
        return try? Data(contentsOf: url)
    }

    private static func sourceFixtureOrSkip() throws -> Data {
        guard let data = try sourceFixtureData() else {
            throw XCTSkip(
                "fixtures/codex/refusal-sentences.json is not reachable from this bundle; "
                    + "the byte-identity gate needs a checkout")
        }
        return data
    }

    private static func testBundleCopy() throws -> Data {
        try data(inBundle: Bundle(for: CodexRefusalClassifierTests.self), what: "the test bundle")
    }

    private static func appCopy() throws -> Data {
        try data(inBundle: .main, what: "the app bundle")
    }

    private static func data(inBundle bundle: Bundle, what: String) throws -> Data {
        guard let url = bundle.url(forResource: "refusal-sentences", withExtension: "json"),
            let data = try? Data(contentsOf: url)
        else {
            XCTFail(
                "refusal-sentences.json is not in \(what). This is the file that binds the "
                    + "phone's greying rule to the daemon's own sentences; it must not be skipped.")
            throw CocoaError(.fileNoSuchFile)
        }
        return data
    }

    /// Fills a template with values of the shape the daemon documents, so the
    /// classifier is shown a sentence rather than a form. Deliberately bland:
    /// nothing substituted here may be what makes a sentence classify.
    ///
    /// Driven by the file's **own** `tokens` map, so a token the daemon retires
    /// simply stops being substituted — `{err}` left when the daemon stopped
    /// interpolating a local store error into a sentence it sends a phone — and
    /// a token it adds gets the bland `default` rather than silently going
    /// unfilled.
    private static func fill(_ text: String, tokens: [String: String]) -> String {
        var out = text
        for token in tokens.keys {
            let value: String
            switch token {
            case "{uid}": value = "01K1B3XQ8ZC0DE5FGH7JKMNPQR"
            case "{ref}": value = "cc-1"
            case "{agent}": value = "gemini"
            case "{n}": value = "8192"
            case "{max}": value = "8192"
            case "{status}": value = "interrupted"
            case "{word}": value = "started"
            default: value = "x"
            }
            out = out.replacingOccurrences(of: token, with: value)
        }
        return out
    }
}
