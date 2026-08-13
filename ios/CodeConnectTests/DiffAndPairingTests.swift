import XCTest

@testable import CodeConnect

/// The diff parser and the QR payload — two places where being almost right is
/// indistinguishable from being right until it matters.
final class DiffAndPairingTests: XCTestCase {

    // MARK: Unified diff

    private let sample = """
        diff --git a/Sources/Feature.swift b/Sources/Feature.swift
        index 1a2b3c4..5d6e7f8 100644
        --- a/Sources/Feature.swift
        +++ b/Sources/Feature.swift
        @@ -1,7 +1,8 @@
         import Foundation
        \u{20}
         struct Feature {
        -    let title: String
        +    let title: String?
        +    let subtitle: String?
             let createdAt: Date
         }
        diff --git a/Sources/New.swift b/Sources/New.swift
        new file mode 100644
        index 0000000..abcdefg
        --- /dev/null
        +++ b/Sources/New.swift
        @@ -0,0 +1,2 @@
        +enum New {}
        +// end
        ?? Sources/Untracked.swift
        """

    func testParsesFilesHunksAndCounts() {
        let diff = UnifiedDiff.parse(sample)
        XCTAssertEqual(diff.files.count, 2)

        let modified = diff.files[0]
        XCTAssertEqual(modified.newPath, "Sources/Feature.swift")
        XCTAssertEqual(modified.status, .modified)
        XCTAssertEqual(modified.additions, 2)
        XCTAssertEqual(modified.deletions, 1)
        XCTAssertEqual(modified.hunks.count, 1)

        let added = diff.files[1]
        XCTAssertEqual(added.status, .added)
        XCTAssertEqual(added.newPath, "Sources/New.swift")
        XCTAssertEqual(added.additions, 2)
        XCTAssertEqual(added.deletions, 0)

        XCTAssertEqual(diff.additions, 4)
        XCTAssertEqual(diff.deletions, 1)
    }

    /// Untracked names arrive after the last hunk and belong to no file. Losing
    /// them would make the diff claim a completeness it does not have.
    func testTrailingUntrackedNamesAreKept() {
        let diff = UnifiedDiff.parse(sample)
        XCTAssertTrue(diff.trailing.contains("?? Sources/Untracked.swift"))
    }

    /// Line numbers have to be right on both sides or the diff is worse than
    /// useless: it would point at the wrong line of a file you are about to
    /// approve a change to.
    func testLineNumbersTrackBothSides() throws {
        let diff = UnifiedDiff.parse(sample)
        let lines = diff.files[0].hunks[0].lines
        XCTAssertEqual(lines.first?.oldNumber, 1)
        XCTAssertEqual(lines.first?.newNumber, 1)

        let deletion = try XCTUnwrap(lines.first { $0.kind == .deletion })
        XCTAssertEqual(deletion.oldNumber, 4)
        XCTAssertNil(deletion.newNumber, "a removed line has no line number in the new file")

        let additions = lines.filter { $0.kind == .addition }
        XCTAssertEqual(additions.map(\.newNumber), [4, 5])
        XCTAssertTrue(additions.allSatisfy { $0.oldNumber == nil })
    }

    /// The hunk's declared counts decide where the body ends, so trailing text
    /// that happens to start with a space is not eaten as diff content.
    func testHunkBodyEndsAtItsDeclaredCounts() {
        let diff = UnifiedDiff.parse(
            """
            diff --git a/a b/a
            --- a/a
            +++ b/a
            @@ -1,1 +1,1 @@
            -one
            +two
             this line is outside the hunk
            """)
        XCTAssertEqual(diff.files.count, 1)
        XCTAssertEqual(diff.files[0].hunks[0].lines.count, 2)
        XCTAssertTrue(diff.trailing.contains(" this line is outside the hunk"))
    }

    func testEmptyDiffIsEmpty() {
        let diff = UnifiedDiff.parse("")
        XCTAssertTrue(diff.files.isEmpty)
        XCTAssertTrue(diff.isEmpty)
    }

    func testBinaryAndRenameAreRecognised() {
        let diff = UnifiedDiff.parse(
            """
            diff --git a/logo.png b/logo.png
            index aaa..bbb 100644
            Binary files a/logo.png and b/logo.png differ
            diff --git a/old.swift b/new.swift
            similarity index 98%
            rename from old.swift
            rename to new.swift
            """)
        XCTAssertEqual(diff.files.count, 2)
        XCTAssertEqual(diff.files[0].status, .binary)
        XCTAssertEqual(diff.files[1].status, .renamed)
        XCTAssertTrue(diff.files[1].displayPath.contains("→"))
    }

    // MARK: Folding

    func testLongContextRunsFoldWithThreeLinesEachSide() {
        var lines: [UnifiedDiff.Line] = []
        lines.append(
            UnifiedDiff.Line(id: 0, kind: .addition, text: "+", oldNumber: nil, newNumber: 1))
        for index in 1...20 {
            lines.append(
                UnifiedDiff.Line(
                    id: index, kind: .context, text: "ctx \(index)", oldNumber: index,
                    newNumber: index))
        }
        lines.append(
            UnifiedDiff.Line(id: 21, kind: .deletion, text: "-", oldNumber: 21, newNumber: nil))

        let segments = UnifiedDiff.fold(lines)
        XCTAssertEqual(segments.count, 5)
        XCTAssertFalse(segments[0].isFolded)
        XCTAssertEqual(segments[1].lines.count, 3, "three lines of context stay after the change")
        XCTAssertTrue(segments[2].isFolded)
        XCTAssertEqual(segments[2].lines.count, 14)
        XCTAssertEqual(segments[3].lines.count, 3, "three lines stay before the next change")
        XCTAssertFalse(segments[4].isFolded)
    }

    func testShortContextRunsAreNotFolded() {
        let lines = (0..<4).map {
            UnifiedDiff.Line(id: $0, kind: .context, text: "c", oldNumber: $0, newNumber: $0)
        }
        let segments = UnifiedDiff.fold(lines)
        XCTAssertEqual(segments.count, 1)
        XCTAssertTrue(segments[0].isFolded, "a whole-context hunk has nothing worth keeping open")
    }

    // MARK: Soft wrap

    func testWrapMarksContinuationsAtTheRightWidth() {
        let text = String(repeating: "x", count: 25)
        let segments = DiffLineWrap.wrap(text, columns: 10)
        // First segment gets the full width; continuations lose a column to `↳`.
        XCTAssertEqual(segments.map(\.count), [10, 9, 6])
        XCTAssertEqual(segments.joined(), text, "wrapping must not lose or add a character")
    }

    func testShortLinesAreNotWrapped() {
        XCTAssertEqual(DiffLineWrap.wrap("short", columns: 40), ["short"])
        XCTAssertEqual(DiffLineWrap.wrap("", columns: 40), [" "])
    }

    // MARK: QR pairing

    func testValidQRPayloadDecodes() {
        let result = PairingQRPayload.decode(
            #"{"v":1,"host":"mac.tail1234.ts.net","port":8787,"code":"ABCD2345"}"#)
        guard case .success(let payload) = result else { return XCTFail("should decode") }
        XCTAssertEqual(payload.host, "mac.tail1234.ts.net")
        XCTAssertEqual(payload.port, 8787)
        XCTAssertEqual(payload.code, "ABCD2345")
        XCTAssertTrue(payload.endpoint.isPairingCode)
        XCTAssertFalse(payload.endpoint.useTLS, "TLS is a fact the daemon reports, not a guess")
    }

    func testWrongVersionIsRefusedWithAReason() {
        let result = PairingQRPayload.decode(#"{"v":2,"host":"h","port":1,"code":"c"}"#)
        guard case .failure(let failure) = result else { return XCTFail("should refuse") }
        XCTAssertEqual(failure, .unsupportedVersion(2))
        XCTAssertTrue(failure.localizedDescription.contains("version 2"))
    }

    func testNonCodeConnectQRIsRefused() {
        guard case .failure(let failure) = PairingQRPayload.decode("https://example.com") else {
            return XCTFail("should refuse")
        }
        XCTAssertEqual(failure, .notJSON)
    }

    func testMissingCodeIsRefused() {
        guard case .failure(let failure) = PairingQRPayload.decode(#"{"v":1,"host":"h"}"#) else {
            return XCTFail("should refuse")
        }
        XCTAssertEqual(failure, .missingField("code"))
    }

    // MARK: Endpoint

    /// A pairing written before per-device credentials existed must survive the
    /// app update that introduced them.
    func testLegacyPairingRecordStillDecodes() throws {
        let legacy = Data(#"{"host":"100.1.2.3","port":8787,"token":"abc","useTLS":false}"#.utf8)
        let endpoint = try JSONDecoder().decode(DaemonEndpoint.self, from: legacy)
        XCTAssertEqual(endpoint.token, "abc")
        XCTAssertFalse(endpoint.isPairingCode)
        XCTAssertTrue(endpoint.hostIsIPLiteral)
    }

    /// A `tailscale cert` certificate has a DNS SAN, so TLS against a literal
    /// address can never validate — the app must know which it has.
    func testIPLiteralDetection() {
        XCTAssertTrue(
            // A real address, because this assertion is about *recognising* one.
            // 100.64.0.0/10 is the CGNAT range a tailnet actually issues from,
            // so this is shaped like the thing it stands for without being
            // anybody's machine.
            DaemonEndpoint.parse(address: "100.64.0.1", token: "t")?.hostIsIPLiteral == true)
        XCTAssertTrue(DaemonEndpoint.parse(address: "[fd7a::1]:8787", token: "t")?.hostIsIPLiteral == true)
        XCTAssertEqual(
            DaemonEndpoint.parse(address: "mac.tail1234.ts.net", token: "t")?.hostIsIPLiteral,
            false)
    }

    func testEndpointURLFollowsTheRequestedScheme() throws {
        let endpoint = try XCTUnwrap(DaemonEndpoint.parse(address: "mac.ts.net:9000", token: "t"))
        XCTAssertEqual(endpoint.url(useTLS: true)?.absoluteString, "wss://mac.ts.net:9000")
        XCTAssertEqual(endpoint.url(useTLS: false)?.absoluteString, "ws://mac.ts.net:9000")
    }

    // MARK: Deep links

    func testDeepLinkRoutes() {
        XCTAssertEqual(DeepLink(url: URL(string: "codeconnect://deck")!), .deck(requestID: nil))
        XCTAssertEqual(
            DeepLink(url: URL(string: "codeconnect://deck?request=toolu_1")!),
            .deck(requestID: "toolu_1"))
        XCTAssertEqual(
            DeepLink(url: URL(string: "codeconnect://session/cc-1")!),
            .session(sessionID: "cc-1", requestID: nil))
        XCTAssertEqual(
            DeepLink(url: URL(string: "codeconnect://session/cc-1?request=toolu_2")!),
            .session(sessionID: "cc-1", requestID: "toolu_2"))
        XCTAssertEqual(
            DeepLink(url: URL(string: "codeconnect://session/cc-1/diff")!),
            .diff(sessionID: "cc-1"))
        XCTAssertNil(DeepLink(url: URL(string: "https://example.com/deck")!))
        XCTAssertNil(DeepLink(url: URL(string: "codeconnect://nowhere")!))
    }

    // MARK: Who said it

    /// **The app must never be able to route its own words into the slot
    /// captioned "the daemon's own reason follows, verbatim".**
    ///
    /// Cold-launching `codeconnect://session/<uid>/diff` beats the socket: the
    /// sheet's `.task` fires `start(force:false)` while the connection is still
    /// being made, `loadDiff` refuses, and what the reader saw was
    /// `The Mac could not read the diff` over `No diff was produced. The
    /// daemon's own reason follows, verbatim.` over **`Connecting to the
    /// daemon…`** — a sentence `LinkHealth` wrote about itself, attributed to
    /// the Mac. The `notConnected` branch that should have drawn instead has
    /// existed all along and was simply unreachable, because the state carried
    /// a `String` and a string does not say who wrote it.
    @MainActor
    func testAFailureWithNoDaemonBehindItIsTaggedAsTheAppsOwn() {
        let model = AppModel()
        // Nothing is connected in a unit test, which is exactly the deep-link
        // race this reproduces.
        model.loadDiff(key: "cc-1")

        guard case .failed(let failure) = model.diffState(for: "cc-1") else {
            return XCTFail("an unconnected diff request has to fail, not hang")
        }
        XCTAssertFalse(
            failure.isFromDaemon,
            "the daemon was never asked, so nothing it said can be quoted")
        XCTAssertTrue(
            model.diffState(for: "cc-1").failedOnTheLink,
            "the screen distinguishes a link failure so it can draw its own state for it")
        if case .daemon = failure { XCTFail("a link failure is never a daemon failure") }
    }

    /// The two origins are distinguishable at the point of use, which is what
    /// the `failed(_:)` branch's caption depends on.
    func testOnlyADaemonFailureMayBeQuoted() {
        XCTAssertTrue(DiffFailure.daemon("session directory is not readable").isFromDaemon)
        XCTAssertFalse(DiffFailure.app("Connecting to the daemon…").isFromDaemon)
        XCTAssertEqual(DiffFailure.app("Connecting to the daemon…").reason, "Connecting to the daemon…")
    }

    func testDeepLinksRoundTripThroughTheirURLs() throws {
        for link in [
            DeepLink.deck(requestID: nil),
            .deck(requestID: "toolu_1"),
            .session(sessionID: "cc-1", requestID: nil),
            .session(sessionID: "cc-1", requestID: "toolu_2"),
            .diff(sessionID: "cc-1"),
        ] {
            let url = try XCTUnwrap(link.url)
            XCTAssertEqual(DeepLink(url: url), link)
        }
    }
}

/// The manual pairing field takes either credential, because `codeconnect pair` tells the
/// user they may type the code by hand — and it prints it hyphenated.
final class CredentialInputTests: XCTestCase {
    func testHyphenatedPairingCodeIsRecognised() {
        XCTAssertEqual(PairingCredentialInput.classify("35AG-AE55"), .pairingCode("35AGAE55"))
        XCTAssertEqual(PairingCredentialInput.classify(" 35agae55 "), .pairingCode("35AGAE55"))
    }

    func testStaticTokenIsRecognised() {
        let token = "8ef368f146c2c387c55310a00cab548a52717a20a40e85ca3f6a515fb70eff39"
        XCTAssertEqual(PairingCredentialInput.classify(token), .token(token))
    }

    /// An 8-character hex string is ambiguous by shape; the code reading wins,
    /// because a token is never that short.
    func testShortHexReadsAsACode() {
        XCTAssertEqual(PairingCredentialInput.classify("deadbeef"), .pairingCode("DEADBEEF"))
    }

    func testNonsenseIsRefusedRatherThanGuessed() {
        XCTAssertNil(PairingCredentialInput.classify(""))
        XCTAssertNil(PairingCredentialInput.classify("abc"))
        XCTAssertNil(PairingCredentialInput.classify("hello world"))
    }
}
