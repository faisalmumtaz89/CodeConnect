#if DEBUG
    import CryptoKit
    import Foundation

    /// **Claude's question card, staged for the render harness and the UI tests.**
    ///
    /// `-CC_QUESTION <state>` replays one Claude session holding one
    /// `AskUserQuestion` card from the daemon's own events —
    /// `fixtures/claude/minor-21-wire.json`, which ccd's test emits, copied into
    /// the app bundle. Every frame goes through the real `ServerMessage` decoder
    /// and the real ingest path.
    ///
    /// Only the question's words are changed: the live 2.1.286 session's four —
    /// single choice, several choices, "Other", previews — **lengthened to the
    /// worst case**, with labels that wrap at reading size and a question
    /// carrying `café ☕`. The card's `display_text` and `payload_hash` are
    /// recomputed from them as the daemon computes them, so the card verifies
    /// for real. Option counts and order are the capture's, so the same taps send
    /// the same option indices.
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

        static func frames(state: State, now: Date = Date()) -> [ServerMessage] {
            let events = events(state: state, now: now)
            guard let first = events.first else { return [] }
            let frames =
                [helloAck(state), sessions(state, from: first, last: events.last!, now: now)]
                + events.map { JSONValue.object(["type": .string("event"), "event": $0]) }
            return frames.compactMap {
                try? JSONDecoder().decode(ServerMessage.self, from: Data($0.canonicalJSONString.utf8))
            }
        }

        /// The daemon's events for `state`, its question's words lengthened.
        static func events(state: State, now: Date = Date()) -> [JSONValue] {
            guard let wire else { return [] }
            let held = wire["held_request"].map { lengthened($0, hold: "held", now: now) }
            let names: [String]
            switch state {
            case .held: return held.map { [$0] } ?? []
            case .oldMac:
                return wire["held_request"].map { [lengthened($0, hold: nil, now: now)] } ?? []
            case .atMac:
                return wire["at_mac_request"].map { [lengthened($0, hold: "at_mac", now: now)] } ?? []
            case .ended: names = ["hold_ended"]
            case .answered: names = ["answered_from_phone"]
            }
            return ([held] + names.map { wire[$0].map { retimed($0, now: now) } }).compactMap { $0 }
        }

        // MARK: From the daemon's events

        private static let wire: [String: JSONValue]? = {
            guard
                let url = Bundle.main.url(forResource: "minor-21-wire", withExtension: "json"),
                let data = try? Data(contentsOf: url)
            else { return nil }
            return (try? JSONDecoder().decode(JSONValue.self, from: data))?.objectValue
        }()

        /// An `approval_request` with this file's words, and the hold given
        /// (none, as a minor-20 daemon sends it).
        private static func lengthened(_ event: JSONValue, hold: String?, now: Date) -> JSONValue {
            guard
                let input = try? JSONDecoder().decode(JSONValue.self, from: Data(toolInput.utf8)),
                var fields = event.objectValue, var payload = fields["payload"]?.objectValue,
                var card = payload["card"]?.objectValue, var hook = payload["hook"]?.objectValue
            else { return event }
            let display = "\(QuestionCard.toolName)\n\(input.canonicalJSONString)"
            card["tool_input"] = input
            card["display_text"] = .string(display)
            card["payload_hash"] = .string(
                SHA256.hash(data: Data(display.utf8)).map { String(format: "%02x", $0) }.joined())
            card["question_hold"] = hold.map { .string($0) }
            hook["tool_input"] = input
            payload["card"] = .object(card)
            payload["hook"] = .object(hook)
            fields["payload"] = .object(payload)
            fields["ts"] = .string(rfc3339(now.addingTimeInterval(-90)))
            return .object(fields)
        }

        /// A later event, at `now`.
        private static func retimed(_ event: JSONValue, now: Date) -> JSONValue {
            guard var fields = event.objectValue else { return event }
            fields["ts"] = .string(rfc3339(now))
            if var payload = fields["payload"]?.objectValue, payload["resolved_at"] != nil {
                payload["resolved_at"] = .string(rfc3339(now))
                fields["payload"] = .object(payload)
            }
            return .object(fields)
        }

        // MARK: Around them

        private static func helloAck(_ state: State) -> JSONValue {
            let text = """
                {"type":"hello_ack","protocol_version":1,"protocol_minor":\(state == .oldMac ? 20 : 21),\
                "server_time":"2026-10-03T00:00:00.000Z",\
                "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",\
                "answer_path":"hook_return","hold_secs":0,"send_text":true,"capture":true,\
                "delete_session":true,"test_push":false,"push":false,"tls":false,\
                "tls_active":false,"diff":true,"risk_class":true,"session_uid":true,\
                "send_text_idempotent":true,"slash_composer_recovery":true,\
                "prompt_identity":true,"command_catalog":true,"terminal_pty":true\
                \(state == .oldMac ? "" : #","question_card":true"#)},"device_name":"iPhone"}
                """
            return (try? JSONDecoder().decode(JSONValue.self, from: Data(text.utf8))) ?? .null
        }

        /// The one session the events belong to, blocked on their card until it
        /// is answered.
        private static func sessions(_ state: State, from first: JSONValue, last: JSONValue, now: Date)
            -> JSONValue
        {
            let stamp = JSONValue.string(rfc3339(now))
            let blocked = state == .answered ? [] : [first["payload"]?["card"]?["request_id"] ?? .null]
            let session: [String: JSONValue] = [
                "session_uid": first["session_uid"] ?? .null,
                "session_id": first["session_id"] ?? .null,
                "tmux_session": first["session_id"] ?? .null,
                "cwd": .string("/Users/dev/release-planning"),
                "project_label": .string("release-planning"),
                "lifecycle": .string("live"), "link": .string("attached"),
                "last_seq": last["seq"] ?? .null,
                "created_at": stamp, "updated_at": stamp, "blocked_on": .array(blocked),
            ]
            return .object(["type": .string("sessions"), "sessions": .array([.object(session)])])
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
