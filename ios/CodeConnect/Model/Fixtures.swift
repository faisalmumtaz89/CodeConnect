#if DEBUG
    import CryptoKit
    import Foundation

    /// Contract-shaped daemon frames for driving the UI without a daemon.
    ///
    /// Debug builds only. Two rules keep this from becoming the mock that
    /// `CodeConnectUITests` exists to avoid:
    ///
    ///   * every frame here is **JSON text**, decoded by the same
    ///     `ServerMessage` decoder the socket uses. A fixture that stopped
    ///     matching the wire would fail to decode rather than quietly diverge —
    ///     so these double as a decoder test.
    ///   * `payload_hash` is computed the way `protocol/src/hash.rs` computes
    ///     it, so the cards verify for real. A fixture card that failed
    ///     verification would exercise the *blocked* path, not the Deck.
    ///
    /// This is how the cross-fleet Deck gets an XCUITest: it needs several
    /// sessions blocked at once, at three different risk classes, which is not
    /// a state you can reliably arrange on a live Mac on demand.
    enum Fixtures {
        struct Card {
            var sessionID: String
            var requestID: String
            var tool: String
            var input: JSONValue
            var riskClass: String?
            var matchedPattern: String?
            var ageSeconds: TimeInterval
        }

        /// Which shape of fleet to replay.
        ///
        /// `deck` is the original and the default: three agents, one card each,
        /// one per risk class. It is a good Deck fixture and it was a blind spot
        /// on the fleet — with one card per agent, "count the sessions" and
        /// "count the cards" return the same number, so the screen's two
        /// loudest strings agreed by coincidence and a real disagreement could
        /// go unseen indefinitely. `stacked` is the same fleet with one
        /// agent holding two decisions, which is the ordinary case for a
        /// `Write`-heavy turn and the smallest state that tells the two counts
        /// apart.
        enum Variant: String, CaseIterable {
            case deck
            case stacked

            init?(_ raw: String?) {
                guard let raw, let value = Variant(rawValue: raw) else { return nil }
                self = value
            }

            var cards: [Card] {
                switch self {
                case .deck: return deckCards
                case .stacked: return deckCards + [stackedCard]
                }
            }

            /// How many agents the daemon reports. `stacked` adds a fifth that
            /// is **running a tool** — the `deck` fixture has no Running band at
            /// all, so the one place in the product where a shell command was
            /// set in proportional type could not be reached from a fixture
            /// either.
            var sessionCount: Int {
                switch self {
                case .deck: return 4
                case .stacked: return 5
                }
            }

            /// Whether the last agent is mid-tool-call rather than finished.
            var hasRunningTool: Bool { self == .stacked }
        }

        /// Three blocked agents, one per risk class, oldest first by design so
        /// the Deck's ordering rule is observable. Session ids are `fx-N`, not
        /// `cc-N`: a fixture must not be able to collide with a real session's
        /// cached history.
        static let deckCards: [Card] = [
            Card(
                sessionID: "fx-3", requestID: "toolu_fixture_low", tool: "Read",
                input: .object(["file_path": .string("/Users/dev/app/README.md")]),
                riskClass: "low", matchedPattern: nil, ageSeconds: 300),
            Card(
                sessionID: "fx-2", requestID: "toolu_fixture_medium", tool: "Write",
                input: .object([
                    "file_path": .string("/Users/dev/app/Sources/Feature.swift"),
                    "content": .string("import Foundation\n"),
                ]),
                riskClass: "medium", matchedPattern: "write outside worktree",
                ageSeconds: 180),
            Card(
                sessionID: "fx-1", requestID: "toolu_fixture_high", tool: "Bash",
                input: .object([
                    "command": .string("git push --force origin main"),
                    "description": .string("Force-push the rebased branch"),
                ]),
                riskClass: "high", matchedPattern: "git push --force", ageSeconds: 60),
        ]

        /// **A second decision on an agent that already has one.**
        ///
        /// `fx-2` rather than a fifth session, deliberately: the state worth
        /// covering is *one agent, two cards*, and adding a fourth blocked agent
        /// would have kept sessions and cards in step and covered nothing. Aged
        /// between the MEDIUM and the LOW so the Deck's within-tier "oldest
        /// first" tiebreak is observable too — it sorts behind `fx-2`'s own
        /// 180-second `Write` and ahead of nothing else in its tier.
        static let stackedCard = Card(
            sessionID: "fx-2", requestID: "toolu_fixture_medium_second", tool: "Edit",
            input: .object([
                "file_path": .string("/Users/dev/app/Sources/Router.swift"),
                "old_string": .string("case .home"),
                "new_string": .string("case .home, .settings"),
            ]),
            riskClass: "medium", matchedPattern: "edit outside worktree", ageSeconds: 240)

        // MARK: Frames

        static func frames(now: Date = Date(), variant: Variant = .deck) -> [ServerMessage] {
            let cards = variant.cards
            var messages: [ServerMessage] = []
            if let ack = decode(helloAckJSON) { messages.append(ack) }
            if let sessions = decode(sessionsJSON(now: now, variant: variant, cards: cards)) {
                messages.append(sessions)
            }
            for (index, card) in cards.enumerated() {
                if let event = decode(approvalEventJSON(card, seq: UInt64(index + 2), now: now)) {
                    messages.append(event)
                }
            }
            if let turn = decode(turnCompleteJSON(now: now)) { messages.append(turn) }
            if variant.hasRunningTool {
                // In order, so `fx-5` reads as Running: the turn's last item has
                // to be the tool call.
                for json in turnJSON(now: now) {
                    if let event = decode(json) { messages.append(event) }
                }
            }
            return messages
        }

        /// A diff that exercises every branch of the parser: a modified file
        /// with two hunks and a long context run to fold, a new file, and a
        /// trailing untracked name.
        static let diffJSON = """
            {"type":"diff","session_id":"fx-1","truncated":false,\
            "captured_at":"2026-07-31T09:14:02.104Z","unified":\
            "diff --git a/Sources/Feature.swift b/Sources/Feature.swift\\nindex 1a2b3c4..5d6e7f8 100644\\n\
            --- a/Sources/Feature.swift\\n+++ b/Sources/Feature.swift\\n\
            @@ -1,14 +1,15 @@\\n import Foundation\\n \\n struct Feature {\\n     let id: String\\n\
            -    let title: String\\n+    let title: String?\\n+    let subtitle: String?\\n     let createdAt: Date\\n \\n\
                 init(id: String) {\\n         self.id = id\\n         self.title = nil\\n\
            +        self.subtitle = nil\\n         self.createdAt = .now\\n     }\\n }\\n\
            diff --git a/Sources/New.swift b/Sources/New.swift\\nnew file mode 100644\\nindex 0000000..abcdefg\\n\
            --- /dev/null\\n+++ b/Sources/New.swift\\n@@ -0,0 +1,3 @@\\n+enum New {\\n+    static let value = 1\\n+}\\n\
            ?? Sources/Untracked.swift\\n"}
            """

        static func diff() -> SessionDiff? {
            guard case .diff(let diff)? = decode(diffJSON) else { return nil }
            return diff
        }

        // MARK: JSON builders

        private static let helloAckJSON = """
            {"type":"hello_ack","protocol_version":1,"protocol_minor":1,\
            "server_time":"2026-07-31T09:14:00.000Z",\
            "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",\
            "answer_path":"send_keys","hold_secs":0,"send_text":true,"capture":true,\
            "push":false,"tls":false,"tls_active":false,"diff":true,"risk_class":true},\
            "device_name":"iPhone","ssh_key_installed":true}
            """

        /// `blocked_on` is derived from the cards rather than stubbed with one
        /// placeholder id, so `SessionSummary.blockedOn.count` is the number of
        /// decisions that agent is really holding — which is what the fleet
        /// counts before a card's contents have reached the stream.
        private static func sessionsJSON(now: Date, variant: Variant, cards: [Card]) -> String {
            let stamp = rfc3339(now)
            let summaries = (1...variant.sessionCount).map { index in
                let id = "fx-\(index)"
                let blocked = cards.filter { $0.sessionID == id }.map { quoted($0.requestID) }
                return """
                    {"session_id":"\(id)","tmux_session":"\(id)",\
                    "cwd":"/Users/dev/app-\(index)","lifecycle":"live","link":"attached",\
                    "last_seq":10,"created_at":"\(stamp)","updated_at":"\(stamp)",\
                    "blocked_on":[\(blocked.joined(separator: ","))]}
                    """
            }
            return "{\"type\":\"sessions\",\"sessions\":[\(summaries.joined(separator: ","))]}"
        }

        private static func approvalEventJSON(_ card: Card, seq: UInt64, now: Date) -> String {
            let inputJSON = card.input.canonicalJSONString
            // `protocol/src/hash.rs`: SHA-256 of "{tool_name}\n{tool_input}".
            let displayText = "\(card.tool)\n\(inputJSON)"
            let hash = SHA256.hash(data: Data(displayText.utf8))
                .map { String(format: "%02x", $0) }.joined()
            // `protocol/src/ws.rs`: the class rides in a nested `risk` block.
            let pattern = card.matchedPattern.map { ",\"matched_pattern\":\(quoted($0))" } ?? ""
            let risk = card.riskClass.map { ",\"risk\":{\"class\":\"\($0)\"\(pattern)}" } ?? ""
            let stamp = rfc3339(now.addingTimeInterval(-card.ageSeconds))
            return """
                {"type":"event","event":{"seq":\(seq),"session_id":"\(card.sessionID)",\
                "ts":"\(stamp)","kind":"approval_request","source":"hook",\
                "payload":{"card":{"request_id":"\(card.requestID)","payload_hash":"\(hash)",\
                "tool_name":"\(card.tool)","tool_input":\(inputJSON),\
                "display_text":\(quoted(displayText))\(risk)}}}}
                """
        }

        /// **One whole turn on `fx-5`: what you said, what it said, what it is
        /// doing.**
        ///
        /// Three frames rather than one, because the timeline's three text rows
        /// each have their own column and `deck` reached only one of them. The
        /// tool call comes last so the session reads as **Running** and the
        /// fleet grows a Running band.
        ///
        /// `echo soak` deliberately: it is the exact string that was measured
        /// set in proportional type on an Idle row while `git push --force
        /// origin main` two rows above it was monospace — nine characters in
        /// 64.00pt of ink where the mono advance predicts 53.
        private static func turnJSON(now: Date) -> [String] {
            let stamp = rfc3339(now)
            return [
                """
                {"type":"event","event":{"seq":12,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"user_message","source":"transcript",\
                "payload":{"message":{"content":"Run the soak test and tell me what breaks."}}}}
                """,
                """
                {"type":"event","event":{"seq":13,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"agent_message","source":"transcript",\
                "payload":{"message":{"content":[{"type":"text",\
                "text":"Starting the soak now. I will report the first failure with its seed."}]}}}}
                """,
                """
                {"type":"event","event":{"seq":14,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"tool_call","source":"hook",\
                "payload":{"tool_use_id":"toolu_fixture_running","tool_name":"Bash",\
                "tool_input":{"command":"echo soak"}}}}
                """,
            ]
        }

        private static func turnCompleteJSON(now: Date) -> String {
            """
            {"type":"event","event":{"seq":11,"session_id":"fx-4","ts":"\(rfc3339(now))",\
            "kind":"turn_complete","source":"hook",\
            "payload":{"hook_event_name":"Stop","last_assistant_message":"Refactored the parser."}}}
            """
        }

        // MARK: Helpers

        private static func decode(_ json: String) -> ServerMessage? {
            try? JSONDecoder().decode(ServerMessage.self, from: Data(json.utf8))
        }

        private static func quoted(_ value: String) -> String {
            JSONValue.string(value).canonicalJSONString
        }

        private static func rfc3339(_ date: Date) -> String {
            date.formatted(Date.ISO8601FormatStyle(includingFractionalSeconds: true))
        }
    }
#endif
