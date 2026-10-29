import CryptoKit
import SwiftUI
import Vision
import XCTest

@testable import CodeConnect

/// Every agent-controlled string on a decision surface is drawn and spoken
/// `displaySafe`, through the model that carries it to the view.
///
/// The cards are the daemon's own bytes, re-derived by the daemon's tests from
/// the hostile inputs under `fixtures/`:
///
///   * `codex-cards-rlo-wire.json` — three Codex cards from `Approval::read`
///     and `Approval::card` (`ccd/src/codex_approval.rs`, from
///     `codex/approval-rlo-inputs.json`): a command whose
///     "don't ask again" grant prints a wrapper script, one whose grant
///     prints tokens, and a file change with a rename and a diff. U+202E is in
///     the command, the reason, the cwd, both grants, a path, its rename and a
///     diff line.
///   * `question-cards-rlo-wire.json` — two `AskUserQuestion` cards through
///     `handle_hook` / `handle_hook_held` (`ccd/src/state.rs`, from
///     `claude/askuq-rlo-inputs.json`), and the held one answered from the
///     phone (`unchecked_answered`). `everywhere`
///     has U+202E in its question, header, label, description and a
///     2,101-unit preview, and the daemon still sends it, `at_mac`;
///     `unchecked` has it only where the daemon does not look — header,
///     description and long preview — and the daemon holds it for the phone.
///
/// These tests prove the models hand views mapped text, and that the screens
/// rendered here — named below — speak no mapped scalar. `DecisionTextSourceTests`
/// says which raw reads the views may make, and where; neither proves that a
/// view not rendered here, reading a name neither lists, draws safe text.
@MainActor
final class DecisionSurfaceTextTests: XCTestCase {

    /// The scalars `displaySafe` maps, every one.
    private static let mapped: [Unicode.Scalar] =
        ([0x00, 0x061C] + Array(0x202A...0x202E) + Array(0x2066...0x2069))
        .compactMap(Unicode.Scalar.init)

    private func assertShowsNoControl(
        _ text: String?, _ what: String, file: StaticString = #filePath, line: UInt = #line
    ) {
        guard let text else { return XCTFail("\(what) is missing", file: file, line: line) }
        let found = text.unicodeScalars.filter { Self.mapped.contains($0) }
        XCTAssertTrue(
            found.isEmpty, "\(what) carries \(found.map { $0.escaped(asASCII: true) }): \(text.debugDescription)",
            file: file, line: line)
        XCTAssertTrue(text.contains("\u{FFFD}"), "\(what) lost the character: \(text.debugDescription)", file: file, line: line)
    }

    private func resource(_ name: String) throws -> JSONValue {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: name, withExtension: "json"),
            "\(name).json is not in the test bundle")
        return try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url))
    }

    private func codexCard(_ name: String) throws -> ApprovalCard {
        try XCTUnwrap(try resource("codex-cards-rlo-wire")[name]?.decoded(ApprovalCard.self))
    }

    private func questionCard(_ name: String) throws -> ApprovalCard {
        try XCTUnwrap(
            try resource("question-cards-rlo-wire")[name]?["payload"]?["card"]?.decoded(ApprovalCard.self))
    }

    // MARK: The models

    /// Codex's reason, cwd, change list, rename, diff and option labels — the
    /// "don't ask again" grant among them — are drawn `displaySafe`. The
    /// option ids, which are the answer, stay the daemon's.
    func testACodexCardsTextIsDrawnSafe() throws {
        for name in ["command_wrapped", "command_tokens"] {
            let card = try codexCard(name)
            if name == "command_wrapped" {
                assertShowsNoControl(CodexCard.reason(in: card.toolInput), "\(name) reason")
                assertShowsNoControl(CodexCard.cwd(in: card.toolInput), "\(name) cwd")
            }
            let options = CodexCard.options(in: card.toolInput)
            XCTAssertEqual(options.map(\.id), ["accept", "acceptWithExecpolicyAmendment", "cancel"])
            assertShowsNoControl(options[1].label, "\(name) grant")
        }
        let card = try codexCard("file_change")
        assertShowsNoControl(CodexCard.reason(in: card.toolInput), "file change reason")
        let changes = CodexCard.changes(in: card.toolInput)
        assertShowsNoControl(changes[0].path, "changed path")
        assertShowsNoControl(changes[0].movePath, "rename")
        assertShowsNoControl(changes[0].diff, "diff")
        XCTAssertEqual(changes[1].path, "/work/b.txt", "a plain path is drawn as it arrives")

        // Two paths drawn alike are still two rows: a change is known by its
        // place in the list, never by what it draws.
        let alike = CodexCard.changes(
            in: .object([
                "changes": .array([
                    .object(["path": .string("/work/\u{202E}a")]),
                    .object(["path": .string("/work/\u{202D}a")]),
                ])
            ]))
        XCTAssertEqual(alike.map(\.path), ["/work/\u{FFFD}a", "/work/\u{FFFD}a"])
        XCTAssertEqual(Set(alike.map(\.id)).count, 2)
    }

    /// Every string of a question is drawn `displaySafe`, on the card the
    /// daemon holds for the phone and on the one it sends to be answered at
    /// the Mac. Which options take notes is decided on the preview as sent.
    func testAQuestionCardsTextIsDrawnSafe() throws {
        for name in ["everywhere", "unchecked"] {
            let questions = try XCTUnwrap(QuestionCard(card: try questionCard(name)), name)
            let question = questions.questions[0]
            assertShowsNoControl(question.header, "\(name) header")
            assertShowsNoControl(question.options[0].description, "\(name) description")
            assertShowsNoControl(question.options[0].preview, "\(name) preview")
            XCTAssertTrue(question.takesNotes, "\(name): a single choice with a preview takes notes")
            if name == "everywhere" {
                assertShowsNoControl(question.text, "question")
                assertShowsNoControl(question.options[0].label, "label")
            }
        }
    }

    /// A notice's title and detail are drawn `displaySafe`, and so is inline
    /// prose after its Markdown is read: `&#x202e;` is a right-to-left
    /// override once parsed, though the text holds none.
    func testANoticeAndItsProseAreDrawnSafe() {
        let notice = NoticeItem(
            kind: .agentWaiting, symbol: "bell", title: "Push to \u{202E}niam",
            detail: "Run \u{2066}this", severity: .info)
        assertShowsNoControl(notice.title, "notice title")
        assertShowsNoControl(notice.detail, "notice detail")
        for encoded in ["git push origin &#x202e;niam ecrof--", "a &#8294;b", "c &#x061c;d"] {
            assertShowsNoControl(String(AgentProse.inline(encoded).characters), encoded)
        }
        XCTAssertEqual(
            String(AgentProse.inline("plain **bold** `code` ~60 s").characters),
            "plain bold code ~60 s", "plain prose reads as before")
    }

    /// The project label is drawn `displaySafe`; two labels that differ only
    /// by a mapped character are told apart as two runs of one project.
    func testAProjectLabelIsDrawnSafe() throws {
        func summary(_ uid: String, project: String, created: String) throws -> SessionSummary {
            try JSONDecoder().decode(
                SessionSummary.self,
                from: Data(
                    """
                    {"session_uid":"\(uid)","session_id":"cc-1","tmux_session":"cc-1",
                     "cwd":"/work/\u{202E}ppa","project_label":"\(project)","lifecycle":"live",
                     "link":"attached","last_seq":1,"created_at":"\(created)",
                     "updated_at":"2026-10-22T00:00:00Z","blocked_on":[]}
                    """.utf8))
        }
        let a = try summary("01K1B3XQ8ZC0DE5FGH7JKMNPQA", project: "app\u{202E}", created: "2026-10-22T09:00:00Z")
        let b = try summary("01K1B3XQ8ZC0DE5FGH7JKMNPQB", project: "app\u{202D}", created: "2026-10-22T10:00:00Z")
        let labels = RunLabel.labels(for: [a, b])
        assertShowsNoControl(labels[a.sessionKey]?.project, "project")
        XCTAssertNotNil(labels[a.sessionKey]?.qualifier, "drawn alike, so told apart")
        XCTAssertNotEqual(labels[a.sessionKey]?.inline, labels[b.sessionKey]?.inline)
        assertShowsNoControl(a.shownCwd, "cwd")
        XCTAssertEqual(a.cwd, "/work/\u{202E}ppa", "the summary keeps the daemon's bytes")
    }

    /// Pretty JSON is drawn `displaySafe`; the canonical JSON the render check
    /// compares against `display_text` keeps the bytes.
    func testPrettyJSONIsDrawnSafeAndCanonicalJSONIsNot() {
        let input = JSONValue.object(["command": .string("git push origin \u{202E}niam ecrof--")])
        assertShowsNoControl(input.prettyJSONString, "pretty JSON")
        XCTAssertEqual(input.canonicalJSONString, "{\"command\":\"git push origin \u{202E}niam ecrof--\"}")
    }

    /// The Mac's screen attached to a permission prompt is drawn `displaySafe`.
    func testThePaneSnapshotIsDrawnSafe() throws {
        let event = try JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":1,"session_id":"cc-1","ts":"2026-10-22T00:00:00.000Z","kind":"notification",
                 "source":"hook","payload":{"notification_type":"permission_prompt",
                 "message":"Claude needs your permission","prompt_id":"p1",
                 "_codeconnect_pane":"git push origin \u{202E}niam ecrof--"}}
                """.utf8))
        assertShowsNoControl(event.paneSnapshot, "pane snapshot")
    }

    /// The fleet row's sentence — here the agent's own last words — is drawn
    /// `displaySafe`.
    func testTheFleetRowsSentenceIsDrawnSafe() async throws {
        let model = try screenModel(agentSays: "Pushed \u{202E}niam ecrof--")
        for _ in 0..<100 where model.fleet.first?.subtitle.isEmpty ?? true {
            try await Task.sleep(for: .milliseconds(20))
        }
        assertShowsNoControl(model.fleet.first?.subtitle, "fleet subtitle")
    }

    // MARK: On the screen

    private typealias SetAccessibilityEnabled = @convention(c) (Int32) -> Void

    private func setAccessibility(_ enabled: Bool) throws {
        let library = try XCTUnwrap(dlopen("/usr/lib/libAccessibility.dylib", RTLD_NOW))
        let symbol = try XCTUnwrap(dlsym(library, "_AXSApplicationAccessibilitySetEnabled"))
        unsafeBitCast(symbol, to: SetAccessibilityEnabled.self)(enabled ? 1 : 0)
    }

    private func stops(in object: NSObject) -> [NSObject] {
        if object.accessibilityElementsHidden { return [] }
        if object.isAccessibilityElement { return [object] }
        let count = object.accessibilityElementCount()
        let children: [NSObject]
        if let elements = object.accessibilityElements as? [NSObject], !elements.isEmpty {
            children = elements
        } else if count > 0, count != NSNotFound {
            children = (0..<count).compactMap { object.accessibilityElement(at: $0) as? NSObject }
        } else if let view = object as? UIView {
            children = view.subviews.filter { !$0.isHidden }
        } else {
            children = []
        }
        return children.flatMap { stops(in: $0) }
    }

    private func settle(_ model: AppModel) async {
        for _ in 0..<5 {
            for state in model.statesForTesting.values { await state.settleForTesting() }
            for _ in 0..<20 { await Task.yield() }
        }
    }

    /// What `root` draws and speaks on an iPhone 17 Pro's screen, after
    /// activating each stop whose label begins with one of `opening` — the
    /// disclosures and rows a reader opens.
    private struct Screen {
        /// Every label and value VoiceOver can stop on.
        var spoken: [String]
        /// The text recognised in the pixels, letters and digits only.
        var drawn: String
    }

    private func screen(
        _ name: String, _ model: AppModel, _ root: some View, height: CGFloat = 2600,
        opening: [String] = []
    ) async throws -> Screen {
        try setAccessibility(true)
        defer { try? setAccessibility(false) }
        let scene = try XCTUnwrap(
            UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first)
        let controller = UIHostingController(
            rootView: root.environment(model).environment(Machines(holding: model)))
        let previous = scene.windows.first { $0.isKeyWindow }
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 402, height: height)
        window.rootViewController = controller
        window.makeKeyAndVisible()
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        func pump(_ rounds: Int) async {
            for _ in 0..<rounds {
                await settle(model)
                controller.view.layoutIfNeeded()
                CATransaction.flush()
                RunLoop.main.run(until: Date().addingTimeInterval(0.15))
            }
        }
        await pump(8)
        for label in opening {
            let target = try XCTUnwrap(
                stops(in: window).first { ($0.accessibilityLabel ?? "").hasPrefix(label) },
                "\(name): nothing to open labelled \(label)")
            _ = target.accessibilityActivate()
            await pump(6)
        }
        let image = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
            window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
        }
        let shot = XCTAttachment(image: image)
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
        let request = VNRecognizeTextRequest()
        request.recognitionLevel = .accurate
        request.usesLanguageCorrection = false
        if let pixels = image.cgImage { try? VNImageRequestHandler(cgImage: pixels).perform([request]) }
        let drawn = (request.results ?? []).compactMap { $0.topCandidates(1).first?.string }
            .joined(separator: " ").lowercased().filter { $0.isLetter || $0.isNumber }
        return Screen(
            spoken: stops(in: window).flatMap { [$0.accessibilityLabel ?? "", $0.accessibilityValue ?? ""] },
            drawn: drawn)
    }

    /// No stop on the screen speaks a mapped scalar, and `expected` — what the
    /// poisoned fields read once mapped — is spoken somewhere.
    ///
    /// U+2068 and U+2069 are left out: SwiftUI wraps every argument it
    /// interpolates into a localized string in that isolate pair —
    /// `Working directory, ⁨/work/�ppa⁩` — balanced, and its own. The models'
    /// tests hold all eleven.
    private func assertScreenIsSafe(
        _ spoken: [String], _ name: String, expected: [String],
        file: StaticString = #filePath, line: UInt = #line
    ) {
        let poison = Self.mapped.filter { $0.value != 0x2068 && $0.value != 0x2069 }
        for text in spoken where text.unicodeScalars.contains(where: { poison.contains($0) }) {
            XCTFail("\(name) speaks \(text.debugDescription)", file: file, line: line)
        }
        for wanted in expected where !spoken.contains(where: { $0.contains(wanted) }) {
            XCTFail("\(name) never speaks \(wanted.debugDescription): \(spoken)", file: file, line: line)
        }
    }

    private func frame(_ value: JSONValue) throws -> ServerMessage {
        try JSONDecoder().decode(ServerMessage.self, from: JSONEncoder().encode(value))
    }

    private func newModel() -> AppModel {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("decision-text-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let model = AppModel(cache: EventCache(root: root))
        model.connection.simulateConnectedForTesting()
        addTeardownBlock { @MainActor in model.scenePhaseChanged(to: .background) }
        return model
    }

    private func summary(
        _ uid: String, agent: String? = nil, blocked: [String], lastSeq: Int
    ) -> JSONValue {
        var fields: [String: JSONValue] = [
            "session_uid": .string(uid), "session_id": .string("cc-1"), "tmux_session": .string("cc-1"),
            "cwd": .string("/work/\u{202E}ppa"), "project_label": .string("app\u{202E}"),
            "lifecycle": .string("live"), "link": .string("attached"), "last_seq": .int(Int64(lastSeq)),
            "created_at": .string("2026-10-22T00:00:00Z"), "updated_at": .string("2026-10-22T00:00:00Z"),
            "blocked_on": .array(blocked.map { .string($0) }),
        ]
        if let agent {
            fields["agent"] = .string(agent)
            fields["codex_thread_id"] = .string("01a06db1-1f8e-7db2-8fd5-10f13af55d1b")
            fields["codex_link"] = .string("subscribed")
        }
        return .object(fields)
    }

    private func inject(_ model: AppModel, _ event: JSONValue) throws {
        model.connection.injectForTesting(
            try frame(.object(["type": .string("event"), "event": event])))
    }

    private func sessions(_ model: AppModel, _ rows: [JSONValue]) throws {
        model.connection.injectForTesting(
            try frame(.object(["type": .string("sessions"), "sessions": .array(rows)])))
    }

    /// A Claude run whose last words, project and cwd carry U+202E.
    private func screenModel(agentSays words: String) throws -> AppModel {
        let model = newModel()
        try sessions(model, [summary(WireRecording.uid, blocked: [], lastSeq: 1)])
        try inject(
            model,
            .object([
                "seq": .int(1), "session_uid": .string(WireRecording.uid), "session_id": .string("cc-1"),
                "ts": .string("2026-10-22T00:00:00.000Z"), "kind": .string("agent_message"),
                "source": .string("transcript"),
                "payload": .object([
                    "type": .string("assistant"),
                    "message": .object([
                        "role": .string("assistant"),
                        "content": .array([.object(["type": .string("text"), "text": .string(words)])]),
                    ]),
                ]),
            ]))
        return model
    }

    private func pendingApprovals(_ model: AppModel, count: Int) async throws -> [ApprovalItem] {
        for _ in 0..<60 where model.deck.count < count {
            await settle(model)
            try await Task.sleep(for: .milliseconds(40))
        }
        XCTAssertEqual(model.deck.count, count, "the cards reached the Deck")
        return model.deck
    }

    /// The run's one approval, as its timeline holds it, once it has arrived.
    private func approval(in model: AppModel, _ uid: String) async throws -> ApprovalItem {
        for _ in 0..<60 {
            await settle(model)
            for item in model.statesForTesting[uid]?.timeline ?? [] {
                if case .approval(let approval) = item.content { return approval }
            }
            try await Task.sleep(for: .milliseconds(40))
        }
        return try XCTUnwrap(nil, "no approval reached the timeline")
    }

    // MARK: The screens

    /// The three Codex cards, each with its Details open; and the fleet, whose
    /// project labels and Deck bar carry the same poison.
    func testTheCodexCardsAndTheFleetSpeakNoControl() async throws {
        let model = newModel()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(canApproveReliably: true, sendText: true, extra: [
                "session_uid": .bool(true), "supported_agents": .array([.string("claude"), .string("codex")]),
            ]),
            minor: 22)
        let cards = try resource("codex-cards-rlo-wire")
        let names = ["command_wrapped", "command_tokens", "file_change"]
        let uids = [
            "01K1B3XQ8ZC0DE5FGH7JKMNPC1", "01K1B3XQ8ZC0DE5FGH7JKMNPC2", "01K1B3XQ8ZC0DE5FGH7JKMNPC3",
        ]
        let ids = try names.map { try XCTUnwrap(cards[$0]?["request_id"]?.stringValue) }
        try sessions(
            model, zip(uids, ids).map { summary($0, agent: "codex", blocked: [$1], lastSeq: 1) })
        for (index, name) in names.enumerated() {
            try inject(
                model,
                .object([
                    "seq": .int(1), "session_uid": .string(uids[index]), "session_id": .string("cc-1"),
                    "ts": .string("2026-10-22T00:00:00.000Z"), "kind": .string("approval_request"),
                    "source": .string("daemon"), "turn_id": .string("01a06db1-3933-73c2-a871-bde7f1db46fc"),
                    "source_event_id": .string("perm:\(ids[index])"),
                    "payload": .object(["card": try XCTUnwrap(cards[name])]),
                ]))
        }
        let pending = try await pendingApprovals(model, count: 3)
        assertScreenIsSafe(
            try await screen("fleet", model, FleetView(), height: 1400).spoken, "the fleet",
            expected: ["app\u{FFFD}", "niam ecrof"])
        for (index, name) in names.enumerated() {
            let approval = try XCTUnwrap(pending.first { $0.card.requestID == ids[index] })
            let spoken = try await screen(
                "codex-card-\(name)", model, ScrollView { DecisionCardView(approval: approval) },
                opening: ["Details"]
            ).spoken
            assertScreenIsSafe(
                spoken, name,
                expected: [
                    "command_wrapped": [
                        "start with `git push origin \u{FFFD}niam", "\u{FFFD}etomer eht",
                        "/work/\u{FFFD}ppa-tset", "app\u{FFFD}",
                    ],
                    "command_tokens": ["start with `git push '\u{FFFD}niam"],
                    "file_change": ["\u{FFFD}txt.olleh", "\u{FFFD}hs.nimda", "\u{FFFD}elif eht"],
                ][name] ?? [])
        }
    }

    /// The question cards in every state the card draws: the one sent to be
    /// answered at the Mac, read-only; the one held for the phone at its first
    /// question, and at its review step after choosing an option on each; and
    /// the same card once its answer from the phone is recorded.
    func testTheQuestionCardsSpeakNoControl() async throws {
        let wire = try resource("question-cards-rlo-wire")
        let scenes: [(name: String, events: [String], blocked: Bool, opening: [String], expected: [String])] = [
            ("at-mac", ["everywhere"], true, [], ["Head\u{FFFD}er", "Desc \u{FFFD}cba", "Push to \u{FFFD}niam?"]),
            ("held", ["unchecked"], true, [], ["Head\u{FFFD}er", "Desc \u{FFFD}cba"]),
            ("review", ["unchecked"], true, ["Left", "Next", "Up", "Review"], ["Head\u{FFFD}er", "Sec\u{FFFD}ond"]),
            ("recorded", ["unchecked", "unchecked_answered"], false, [], ["Head\u{FFFD}er", "Sec\u{FFFD}ond"]),
        ]
        for scene in scenes {
            let model = newModel()
            model.connection.simulateCapabilitiesForTesting(
                Capabilities(canApproveReliably: true, sendText: true, extra: [
                    "session_uid": .bool(true), "hook_only_approvals": .bool(true),
                    "question_card": .bool(true),
                ]),
                minor: 22)
            let events = try scene.events.map { try XCTUnwrap(wire[$0], $0) }
            let id = try XCTUnwrap(events[0]["payload"]?["card"]?["request_id"]?.stringValue)
            try sessions(
                model,
                [summary(WireRecording.uid, blocked: scene.blocked ? [id] : [], lastSeq: 2 + events.count)])
            for event in events { try inject(model, event) }
            var approval = try await approval(in: model, WireRecording.uid)
            if !scene.blocked {
                for _ in 0..<60 where approval.outcome == nil {
                    try await Task.sleep(for: .milliseconds(40))
                    approval = try await self.approval(in: model, WireRecording.uid)
                }
                XCTAssertNotNil(approval.outcome, "the answer is recorded on the card")
            }
            let spoken = try await screen(
                "question-\(scene.name)", model, ScrollView { DecisionCardView(approval: approval) },
                opening: scene.opening
            ).spoken
            assertScreenIsSafe(spoken, scene.name, expected: scene.expected)
        }
    }

    /// The Claude card with its full input and the Mac's screen open, and the
    /// chat with the held call's own tool row open above its approval.
    func testTheClaudeCardAndTheHeldCallsToolRowSpeakNoControl() async throws {
        let model = newModel()
        let wire = try resource("minor-22-wire")
        var request = try XCTUnwrap(wire["held_request"]?.objectValue)
        var payload = try XCTUnwrap(request["payload"]?.objectValue)
        var card = try XCTUnwrap(payload["card"]?.objectValue)
        let input = JSONValue.object(["command": .string("git push origin \u{202E}niam ecrof--")])
        let text = "Bash\n\(input.canonicalJSONString)"
        card["tool_input"] = input
        card["display_text"] = .string(text)
        card["payload_hash"] = .string(
            SHA256.hash(data: Data(text.utf8)).map { String(format: "%02x", $0) }.joined())
        card["prompt_id"] = .string("p1")
        payload["card"] = .object(card)
        request["payload"] = .object(payload)
        request["seq"] = .int(3)
        let uid = WireRecording.uid
        try sessions(model, [summary(uid, blocked: ["toolu_allowed"], lastSeq: 3)])
        try inject(
            model,
            .object([
                "seq": .int(1), "session_uid": .string(uid), "session_id": .string("cc-1"),
                "ts": .string("2026-10-22T00:00:00.000Z"), "kind": .string("tool_call"),
                "source": .string("hook"),
                "payload": .object([
                    "tool_use_id": .string("toolu_allowed"), "tool_name": .string("Bash"),
                    "tool_input": input,
                ]),
            ]))
        try inject(
            model,
            .object([
                "seq": .int(2), "session_uid": .string(uid), "session_id": .string("cc-1"),
                "ts": .string("2026-10-22T00:00:00.000Z"), "kind": .string("notification"),
                "source": .string("hook"),
                "payload": .object([
                    "notification_type": .string("permission_prompt"),
                    "message": .string("Claude needs your permission to use Bash"),
                    "prompt_id": .string("p1"),
                    "_codeconnect_pane": .string("Bash command\n git push origin \u{202E}niam ecrof--"),
                ]),
            ]))
        try inject(model, .object(request))
        let approval = try await approval(in: model, uid)
        assertScreenIsSafe(
            try await screen(
                "claude-card", model, ScrollView { DecisionCardView(approval: approval) },
                opening: ["Full tool input", "The computer's screen"]
            ).spoken,
            "the Claude card", expected: ["\"command\": \"git push origin \u{FFFD}niam", " git push origin \u{FFFD}niam", "app\u{FFFD}"])
        // The tool row speaks one combined label, so its opened input is
        // read off the pixels: drawn as it arrives, the override made it
        // `"command": "git push origin --force main"`.
        let chat = try await screen(
            "claude-chat", model,
            NavigationStack {
                SessionDetailView(route: SessionRoute(key: uid), surfaceForTesting: .constant(.timeline))
            }, height: 1400, opening: ["Bash tool call"])
        assertScreenIsSafe(chat.spoken, "the chat", expected: ["Bash tool call, git push origin \u{FFFD}niam"])
        XCTAssertTrue(chat.drawn.contains("command"), "the held call's row is open: \(chat.drawn)")
        XCTAssertTrue(chat.drawn.contains("niamecrof"), chat.drawn)
        XCTAssertFalse(chat.drawn.contains("forcemain"), chat.drawn)
    }
}
