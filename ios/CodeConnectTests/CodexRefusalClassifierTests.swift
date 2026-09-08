import XCTest

@testable import CodeConnect

/// **K4 — the phone's greying rule, decided by the daemon's own categories.**
///
/// The wire carries no refusal code, so the phone reads the sentence. It used to
/// read it by matching three English phrases, and the shared fixture proved that
/// wrong on the day it landed: the daemon writes 18 link-state refusals and
/// those three clauses matched 8. Ten sentences saying the link was coming back
/// would have left the control live against a link that was reconnecting — with
/// every test in the suite still green.
///
/// `fixtures/codex/refusal-sentences.json` is emitted by `ccd` from the one
/// place each sentence is written, and `ccd`'s own gate test keeps it
/// byte-identical to the build. This asserts the phone against it **by
/// category, never by count**, so the file can be regenerated — the daemon is
/// adding a fourth category and removing two rows — and this still holds.
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

    /// **The app's copy is the repo's copy.**
    ///
    /// The classifier reads a bundled resource, because it has to work on a
    /// phone with no repo. That copy is only safe while it is identical to the
    /// file the daemon emits — otherwise the app's rule and the daemon's
    /// sentences drift apart with nothing to say so.
    func testTheBundledFixtureIsIdenticalToTheDaemonsOwn() throws {
        XCTAssertEqual(
            try Self.appCopy(), try Self.daemonCopy(),
            """
            The app's bundled refusal-sentences.json has drifted from the \
            daemon's. Both are copies of fixtures/codex/refusal-sentences.json: \
            ios/CodeConnect/Resources/ (the app classifies with it, on a phone \
            with no repo) and ios/CodeConnectTests/Resources/ (the tests read \
            it). Copy the daemon's file over both.
            """)
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

    /// **The daemon's file, from the test bundle; the app's, from the app
    /// bundle.**
    ///
    /// Both used to be read through `#filePath`, which bakes this machine's
    /// checkout path into the binary — on CI or a fresh clone the classifier's
    /// whole evidence base was unreachable. The daemon's copy is checked in at
    /// `ios/CodeConnectTests/Resources/refusal-sentences.json` and ships inside
    /// the test bundle; the app's copy ships inside the app, which is where the
    /// classifier actually reads it. Comparing those two is the drift check
    /// that matters, and it needs no repo at all.
    private static func repoFixture() throws -> Wire {
        try JSONDecoder().decode(Wire.self, from: try daemonCopy())
    }

    private static func daemonCopy() throws -> Data {
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
            case "{err}": value = "database is locked"
            case "{status}": value = "interrupted"
            case "{word}": value = "started"
            default: value = "x"
            }
            out = out.replacingOccurrences(of: token, with: value)
        }
        return out
    }
}
