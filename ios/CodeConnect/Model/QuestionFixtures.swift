#if DEBUG
    import CryptoKit
    import Foundation

    /// **Claude's question card, staged for the render harness and the UI tests.**
    ///
    /// `-CC_QUESTION <state>` replays one Claude session holding one
    /// `AskUserQuestion` card in the shape a minor-21 daemon sends it —
    /// `fixtures/claude/minor-21-wire.json`, which ccd's own test emits and the
    /// unit tests decode whole. Every frame goes through the real `ServerMessage`
    /// decoder and the real ingest path, and `payload_hash` is computed as
    /// `protocol/src/hash.rs` computes it, so the card verifies for real.
    ///
    /// The questions are the live 2.1.286 session's four — single choice, several
    /// choices, "Other", previews — with their **shape unchanged and their words
    /// lengthened to the worst case**: labels that wrap at reading size, a
    /// question carrying `café ☕`, and the real previews. Same option counts and
    /// order, so the same taps produce the same `answers` bytes as the daemon's
    /// fixture.
    enum QuestionFixtures {
        enum State: String, CaseIterable {
            /// Held: the phone can answer.
            case held
            /// The question is on the Mac and only the Mac can answer it.
            case atMac = "at-mac"
            /// Held when raised, then a `question_hold: ended` event.
            case ended
            /// Answered from this phone and confirmed by the Mac.
            case answered
            /// A minor-20 Mac: no `question_card`, so read-only.
            case oldMac = "old-mac"

            init?(_ raw: String?) {
                guard let raw, let value = State(rawValue: raw) else { return nil }
                self = value
            }
        }

        static let sessionKey = "qx-1"
        static let requestID = "toolu_fixture_question"
        private static let promptID = "5f0c2a1e-7b3d-4c8e-9a61-2d4f8b0e6c13"
        private static let claudeSessionID = "0b8e4f2a-91c7-4d35-a6e0-7c2f19d8b453"

        /// The answers `answered` carries, and the ones the UI test taps: the
        /// daemon fixture's own `answered_from_phone` choices.
        static let answeredJSON = """
            [{"selected":[0]},{"selected":[0,2]},\
            {"other":"Archive it under docs/plans, café ☕"},\
            {"selected":[1],"notes":"wider screens only"}]
            """

        static func frames(state: State, now: Date = Date()) -> [ServerMessage] {
            ([helloAck(state), sessions(state, now: now)] + events(state: state, now: now))
                .compactMap { try? JSONDecoder().decode(ServerMessage.self, from: Data($0.utf8)) }
        }

        /// The `event` frames, as JSON text. Their shape — every key at every
        /// level, and each value's JSON type — is pinned to the daemon's
        /// `minor-21-wire.json` by `QuestionCardTests`; only the values differ.
        static func events(state: State, now: Date = Date()) -> [String] {
            var json = [request(state, now: now)]
            switch state {
            case .ended:
                json.append(
                    event(
                        seq: 2, kind: "question_hold", now: now, source: "daemon",
                        sourceEventID: "hold:\(requestID):ended",
                        payload: #"{"question_hold":"ended","request_id":"\#(requestID)"}"#))
            case .answered:
                json.append(
                    event(
                        seq: 2, kind: "approval_resolved", now: now, source: "daemon",
                        sourceEventID: "resolved:\(requestID)",
                        payload: """
                            {"applied_via":"hook_return","decision":{"answers":\(answeredJSON),\
                            "type":"answers"},"detail":"returned through Claude's question hook",\
                            "request_id":"\(requestID)","resolved_at":"\(rfc3339(now))",\
                            "resolved_by":"phone","session_id":"cc-1"}
                            """))
            case .held, .atMac, .oldMac:
                break
            }
            return json
        }

        // MARK: Frames

        private static func helloAck(_ state: State) -> String {
            let minor = state == .oldMac ? 20 : 21
            let card = state == .oldMac ? "" : #","question_card":true"#
            return """
                {"type":"hello_ack","protocol_version":1,"protocol_minor":\(minor),\
                "server_time":"2026-10-03T00:00:00.000Z",\
                "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",\
                "answer_path":"hook_return","hold_secs":0,"send_text":true,"capture":true,\
                "delete_session":true,"test_push":false,"push":false,"tls":false,\
                "tls_active":false,"diff":true,"risk_class":true,"session_uid":true,\
                "send_text_idempotent":true,"slash_composer_recovery":true,\
                "prompt_identity":true,"command_catalog":true,"terminal_pty":true\(card)},\
                "device_name":"iPhone"}
                """
        }

        private static func sessions(_ state: State, now: Date) -> String {
            let stamp = rfc3339(now)
            let blocked = state == .answered ? "" : "\"\(requestID)\""
            let lastSeq = state == .ended || state == .answered ? 2 : 1
            return """
                {"type":"sessions","sessions":[\
                {"session_uid":"\(sessionKey)","session_id":"cc-1","tmux_session":"cc-1",\
                "cwd":"/Users/dev/release-planning","project_label":"release-planning",\
                "lifecycle":"live","link":"attached","last_seq":\(lastSeq),\
                "created_at":"\(stamp)","updated_at":"\(stamp)","blocked_on":[\(blocked)]}]}
                """
        }

        private static func request(_ state: State, now: Date) -> String {
            let input = (try? JSONDecoder().decode(JSONValue.self, from: Data(toolInput.utf8)))?
                .canonicalJSONString ?? toolInput
            let display = "\(QuestionCard.toolName)\n\(input)"
            let hash = SHA256.hash(data: Data(display.utf8))
                .map { String(format: "%02x", $0) }.joined()
            let hold: String
            switch state {
            case .held, .ended, .answered: hold = #","question_hold":"held""#
            case .atMac: hold = #","question_hold":"at_mac""#
            case .oldMac: hold = ""
            }
            return event(
                seq: 1, kind: "approval_request", now: now.addingTimeInterval(-90), source: "hook",
                sourceEventID: "perm:\(requestID)",
                payload: """
                    {"card":{"display_text":\(JSONValue.string(display).canonicalJSONString),\
                    "generation":1,"identity_bound":false,"payload_hash":"\(hash)",\
                    "permission_mode":"acceptEdits","prompt_id":"\(promptID)"\(hold),\
                    "request_id":"\(requestID)","risk":{"class":"medium"},\
                    "tool_input":\(input),"tool_name":"\(QuestionCard.toolName)"},\
                    "hook":{"cwd":"/Users/dev/release-planning","effort":{"level":"medium"},\
                    "hook_event_name":"PermissionRequest","permission_mode":"acceptEdits",\
                    "prompt_id":"\(promptID)","session_id":"\(claudeSessionID)",\
                    "tool_input":\(input),"tool_name":"\(QuestionCard.toolName)",\
                    "transcript_path":"/Users/dev/.claude/projects/-Users-dev-release-planning/\(claudeSessionID).jsonl"}}
                    """)
        }

        private static func event(
            seq: Int, kind: String, now: Date, source: String, sourceEventID: String,
            payload: String
        ) -> String {
            """
            {"type":"event","event":{"seq":\(seq),"session_uid":"\(sessionKey)",\
            "session_id":"cc-1","ts":"\(rfc3339(now))","kind":"\(kind)","source":"\(source)",\
            "source_event_id":"\(sourceEventID)","payload":\(payload)}}
            """
        }

        /// The 2.1.286 card's four questions, lengthened to the worst case.
        private static let toolInput = """
            {"questions":[\
            {"header":"Snapshots","multiSelect":false,\
            "question":"How should the 1,284 snapshot files under fixtures/ be stored once the old harness is retired?",\
            "options":[\
            {"label":"Dedupe with hardlinks across every worktree that shares them (Recommended)",\
            "description":"Saves about 2.1 GB and keeps every snapshot readable by the old tests"},\
            {"label":"Delete all snapshots and regenerate them on the next full run",\
            "description":"Frees everything; the next full run takes about forty minutes longer"}]},\
            {"header":"Checks","multiSelect":true,\
            "question":"Which checks should run before the release is tagged?",\
            "options":[\
            {"label":"Unit tests","description":"fast, about ninety seconds"},\
            {"label":"Lint","description":"style and formatting"},\
            {"label":"Soak test against a real Mac for eight hours","description":"slow"}]},\
            {"header":"Archive","multiSelect":false,\
            "question":"Where should the plan archive go — the café ☕ notes included?",\
            "options":[\
            {"label":"Keep on disk, untracked (Recommended)","description":"untracked"},\
            {"label":"Commit it under docs/plans","description":"tracked"}]},\
            {"header":"Layout","multiSelect":false,\
            "question":"Which layout do you prefer for the question card on wide screens?",\
            "options":[\
            {"label":"Stacked","description":"one column",\
            "preview":"+------+\\n| A    |\\n+------+\\n| B    |\\n+------+"},\
            {"label":"Side by side","description":"two columns",\
            "preview":"+---+---+\\n| A | B |\\n+---+---+"}]}]}
            """

        private static func rfc3339(_ date: Date) -> String {
            date.formatted(Date.ISO8601FormatStyle(includingFractionalSeconds: true))
        }
    }
#endif
