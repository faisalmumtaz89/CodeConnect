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
        /// `ended` is the only fixture with runs that have **exited**, and it
        /// exists because removal cannot be reached without one: the swipe is
        /// offered on `lifecycle == .exited` alone, so under `deck` and `stacked`
        /// — every session `live` — the gesture is not merely untested, it is not
        /// installed. It carries enough of them to overflow the screen, because
        /// the thing most worth proving is that the list still *scrolls* over
        /// rows that own a horizontal drag.
        enum Variant: String, CaseIterable {
            case deck
            case stacked
            case ended
            /// The owner's real machine once held 45 ended soak runs, and the
            /// Ended band materializes every row the moment it expands — the
            /// band's own VStack is deliberately not lazy. This is that fleet,
            /// so the cost of the worst real case stays measurable.
            ///
            /// Measured (simulator, identical instrument, 2026-08): expanding 8
            /// rows took 1563ms wall-clock and 45 rows 1645ms — the instrument
            /// itself (quiescence, queries, the 180ms animation) is the 1.5s;
            /// the 37 extra wrapped rows cost ~82ms, ~2ms each. That is why the
            /// band stays eager: a lazy rewrite would be machinery spent on two
            /// milliseconds a row.
            case ended45

            init?(_ raw: String?) {
                guard let raw, let value = Variant(rawValue: raw) else { return nil }
                self = value
            }

            var cards: [Card] {
                switch self {
                case .deck, .ended, .ended45: return deckCards
                case .stacked: return deckCards + [stackedCard]
                }
            }

            /// Sessions after the cards' own, reported `exited`.
            var endedCount: Int {
                switch self {
                case .ended: return 8
                case .ended45: return 45
                case .deck, .stacked: return 0
                }
            }

            /// How many agents the daemon reports. `stacked` adds a fifth that
            /// is **running a tool** — the `deck` fixture has no Running band at
            /// all, so the one place in the product where a shell command was
            /// set in proportional type could not be reached from a fixture
            /// either.
            var sessionCount: Int {
                switch self {
                case .deck, .ended, .ended45: return 4
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
            for json in confirmedFactsJSON(now: now) {
                if let event = decode(json) { messages.append(event) }
            }
            if variant.hasRunningTool {
                // In order, so `fx-5` reads as Running: the turn's last item has
                // to be the tool call.
                for json in turnJSON(now: now) {
                    if let event = decode(json) { messages.append(event) }
                }
            }
            return messages
        }

        /// **The facts the command sheets report, in their worst shapes.**
        ///
        /// Numbered to *follow* the frames above rather than sitting at some
        /// high round number: the client reads a jump in `seq` as events it
        /// never received and says so, correctly — so a fixture that skips
        /// ahead prints a gap banner over every screen it appears on.
        ///
        /// On `fx-4`, the one session holding no approval card: a
        /// `session_start` on a session that *is* blocked reads as that run
        /// having restarted, and its card correctly stops being current —
        /// which is right behaviour and the wrong fixture.
        ///
        /// Without these the Model sheet's populated state — a raw hook id in
        /// monospace, a variant line, and a provenance line under it — is
        /// unreachable by any render, and that exact state is the one that
        /// shipped looking broken. `claude-opus-5[1m]` is verbatim what the
        /// SessionStart hook hands the app; the effort line is verbatim what
        /// Claude Code's `/effort` writes to its transcript, description and
        /// all, so the Effort sheet's confirmed state has a real string to
        /// wrap rather than a tidy one.
        private static func confirmedFactsJSON(now: Date) -> [String] {
            let ts = rfc3339(now)
            let effort =
                "Set effort level to xhigh (saved as your default for new sessions): "
                + "Deeper reasoning than high, just below maximum "
                + "(Fable 5, Opus 4.7+, Sonnet 5)"
            return [
                """
                {"type":"event","event":{"seq":12,"session_id":"fx-4",\
                "ts":"\(ts)","kind":"session_start","source":"hook",\
                "payload":{"hook_event_name":"SessionStart",\
                "model":"claude-opus-5[1m]"}}}
                """,
                """
                {"type":"event","event":{"seq":13,"session_id":"fx-4",\
                "ts":"\(ts)","kind":"user_message","source":"transcript",\
                "payload":{"type":"user","message":{"role":"user",\
                "content":"<local-command-stdout>\(effort)</local-command-stdout>"}}}}
                """,
            ]
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
            {"type":"hello_ack","protocol_version":1,"protocol_minor":9,\
            "server_time":"2026-07-31T09:14:00.000Z",\
            "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",\
            "answer_path":"send_keys","hold_secs":0,"send_text":true,"capture":true,\
            "push":false,"tls":false,"tls_active":false,"diff":true,"risk_class":true,\
            "delete_session":true,"command_catalog":true,"slash_composer_recovery":true},\
            "device_name":"iPhone","ssh_key_installed":true}
            """

        /// The `/status` view exactly as a narrow (80-column) tmux pane
        /// rendered it in the measurement rig — wrapped cwd, the works —
        /// with same-shape stand-ins for the account strings. Worst-case
        /// real data for the snapshot sheet: the render must survive this,
        /// not a tidied version of it.
        static let statusPane = """
            ❯ /usage
              ⎿  Settings dialog dismissed
            ❯ /status
            ────────────────────────────────────────────────────────────────────────────────
              Settings  Status   Config   Usage   Stats

              Version:          2.1.222
              Session name:     Reply with exactly alpha
              Session ID:       022f6af6-328e-4245-8f10-d2d20ae4886f
              Session kind:     interactive
              cwd:              /private/tmp/claude-501/-Users-example-Documents-GitHub
                                -CodeConnect/6e395e66-643a-4147-a28b-c8f7b18222c2/scratchpad
              Login method:     Claude Max account
              Organization:     fixture-owner@example.com's Organization
              Email:            fixture-owner@example.com

              Model:            fable (claude-fable-5)
              Memory:           project + user
              Setting sources:  Login managed settings, Command line arguments,

              Esc to close
            """

        /// `-cc.debug.sendText <mode>` — what the stubbed daemon says to any
        /// send. `recovered` carries the measured pane above, as the real
        /// daemon does for the three snapshot commands.
        static func sendTextResult(mode: String) -> SendTextResult {
            switch mode {
            case "recovered":
                return .composerRecovered(
                    matched: "foragents",
                    paneSnapshot: statusPane,
                    capturedAt: "2026-08-05T14:32:08.000Z")
            case "recovered-empty":
                return .composerRecovered(
                    matched: "foragents", paneSnapshot: "  \n  ",
                    capturedAt: "2026-08-05T14:32:08.000Z")
            case "lost":
                return .composerLost(matched: "foragents")
            default:
                return .sent(matched: "foragents")
            }
        }

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
            // Exited runs carry a `session_uid`, because that is what a daemon new
            // enough to have ended one this way sends, and it is the only id a
            // removal is allowed to name.
            let endedRuns = (0..<variant.endedCount).map { offset in
                let index = variant.sessionCount + offset + 1
                return """
                    {"session_uid":"01K1B3XQ8ZC0DE5FGH7JKMNP\(String(format: "%02d", offset))",\
                    "session_id":"fx-\(index)","tmux_session":"fx-\(index)",\
                    "cwd":"/Users/dev/app-\(index)","lifecycle":"exited","link":"detached",\
                    "last_seq":10,"created_at":"\(stamp)","updated_at":"\(stamp)",\
                    "blocked_on":[]}
                    """
            }
            let all = (summaries + endedRuns).joined(separator: ",")
            return "{\"type\":\"sessions\",\"sessions\":[\(all)]}"
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
                // **Long on purpose.** Collapsed, the 4-line clamp makes this
                // row the same height at any length — nothing below it moves.
                // Expanded, it is taller than a screen, which is the only
                // geometry in which "Show less" can strand the viewport in
                // blank space and scrolling can leave the tail at all — the
                // two behaviours `SessionFollowUITests` exists to prove.
                """
                {"type":"event","event":{"seq":13,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"agent_message","source":"transcript",\
                "payload":{"message":{"content":[{"type":"text",\
                "text":"## Soak status\\nStarting now — **auto-answer lands after ~60 seconds**, and the config in `~/.codeconnect/config.json` holds (~$0 cost). First failure with its seed:\\n```swift\\nlet seed = 0x5eed\\n```\\nWatching.\\n\\nEvery pass so far, oldest first:\\nPass 1 — daemon killed mid-turn; the log replayed gap-free on restart.\\nPass 2 — duplicate PreToolUse hooks; the second was refused as a replay.\\nPass 3 — approval answered on the phone while the Mac prompt was open.\\nPass 4 — connection flapped during a diff; the capture was re-served.\\nPass 5 — tmux server restarted; the session re-adopted its identity.\\nPass 6 — two approvals stormed concurrently; both resolved exactly once.\\nPass 7 — the transcript rotated mid-read; the tailer followed the inode.\\nPass 8 — a stale watermark was rejected and the client replayed cleanly.\\nNo holes, no duplicates, no silent answers. Still watching."}]}}}}
                """,
                """
                {"type":"event","event":{"seq":14,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"tool_call","source":"hook",\
                "payload":{"tool_use_id":"toolu_fixture_running","tool_name":"Bash",\
                "tool_input":{"command":"echo soak"}}}}
                """,
                // **Three tools, three different label widths.** One tool row
                // cannot show a ragged column, so with a single `Bash` the one
                // place the commands have to share a left edge was unreachable
                // from a fixture — and a state nobody can reach is a state
                // nobody has looked at. `Read` and `MultiEdit` bracket `Bash`
                // on either side, which is what makes the column visible in a
                // render and assertable in a test.
                """
                {"type":"event","event":{"seq":15,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"tool_call","source":"hook",\
                "payload":{"tool_use_id":"toolu_fixture_read","tool_name":"Read",\
                "tool_input":{"file_path":"/Users/dev/app/README.md"}}}}
                """,
                """
                {"type":"event","event":{"seq":16,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"tool_call","source":"hook",\
                "payload":{"tool_use_id":"toolu_fixture_multi","tool_name":"MultiEdit",\
                "tool_input":{"file_path":"/Users/dev/app/Sources/Router.swift"}}}}
                """,
                // **Depth below the long message, so a collapse has somewhere
                // wrong to land.** With the timeline ending at the tools
                // above, the post-collapse content fits one screen and every
                // scroll position looks correct — the follow tests could not
                // tell the re-anchor from the scroll view's own clamp until
                // these rows made the difference visible.
                """
                {"type":"event","event":{"seq":17,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"tool_call","source":"hook",\
                "payload":{"tool_use_id":"toolu_fixture_soak1","tool_name":"Bash",\
                "tool_input":{"command":"./soak/run.sh --pass 1"}}}}
                """,
                """
                {"type":"event","event":{"seq":18,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"tool_call","source":"hook",\
                "payload":{"tool_use_id":"toolu_fixture_soak2","tool_name":"Bash",\
                "tool_input":{"command":"./soak/run.sh --pass 2"}}}}
                """,
                """
                {"type":"event","event":{"seq":19,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"tool_call","source":"hook",\
                "payload":{"tool_use_id":"toolu_fixture_soak3","tool_name":"Bash",\
                "tool_input":{"command":"./soak/run.sh --pass 3"}}}}
                """,
                """
                {"type":"event","event":{"seq":20,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"tool_call","source":"hook",\
                "payload":{"tool_use_id":"toolu_fixture_soak4","tool_name":"Bash",\
                "tool_input":{"command":"./soak/run.sh --pass 4"}}}}
                """,
                """
                {"type":"event","event":{"seq":21,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"tool_call","source":"hook",\
                "payload":{"tool_use_id":"toolu_fixture_soak5","tool_name":"Bash",\
                "tool_input":{"command":"./soak/run.sh --pass 5"}}}}
                """,
                """
                {"type":"event","event":{"seq":22,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"tool_call","source":"hook",\
                "payload":{"tool_use_id":"toolu_fixture_soak6","tool_name":"Bash",\
                "tool_input":{"command":"./soak/run.sh --pass 6"}}}}
                """,
                """
                {"type":"event","event":{"seq":23,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"tool_call","source":"hook",\
                "payload":{"tool_use_id":"toolu_fixture_soak7","tool_name":"Bash",\
                "tool_input":{"command":"./soak/run.sh --pass 7"}}}}
                """,
                """
                {"type":"event","event":{"seq":24,"session_id":"fx-5","ts":"\(stamp)",\
                "kind":"tool_call","source":"hook",\
                "payload":{"tool_use_id":"toolu_fixture_soak8","tool_name":"Bash",\
                "tool_input":{"command":"./soak/run.sh --pass 8"}}}}
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
