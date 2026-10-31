#if DEBUG
    import Foundation

    /// **Other agents' messages to a Claude session, staged for the render harness.**
    ///
    /// `-CC_MAIL on` replays the session in `fixtures/claude/transcript-agent-messages-wire.json`,
    /// which ccd's test emits from a recorded Claude Code 2.1.316 run, copied into the app
    /// bundle: a turn that ran a tool takes two messages at its end through the Stop hook,
    /// then one is typed into the idle box. Every frame goes through the real
    /// `ServerMessage` decoder and the real ingest path.
    ///
    /// Only words are changed, **to the worst case**: the sender is a long address on a
    /// long computer name, and the first message is the length and shape agents send —
    /// paragraphs, a list, code, `café ☕`.
    enum AgentMailFixtures {
        static func frames(now: Date = Date()) -> [ServerMessage] {
            guard let events = wire, let first = events.first, let last = events.last else {
                return []
            }
            let frames =
                [helloAck, sessions(from: first, last: last, now: now)]
                + events.map { JSONValue.object(["type": .string("event"), "event": retimed($0, now: now)]) }
            return frames.compactMap {
                try? JSONDecoder().decode(ServerMessage.self, from: Data($0.canonicalJSONString.utf8))
            }
        }

        private static let sender = "cc-12@build-server-linux-01"

        private static let longMessage = """
            Can you review the migration in db/migrations/0042_add_email_index.sql before I \
            run it on staging? It adds a unique index on users(email), and the café ☕ \
            accounts created before March have mixed-case duplicates.

            1. Check for duplicates first: `SELECT lower(email), count(*) FROM users GROUP BY 1 HAVING count(*) > 1;`
            2. If there are any, tell me which rows to keep.
            3. Reply with send_message when you are done — I am blocked on this.
            """

        private static let wire: [JSONValue]? = {
            guard
                let url = Bundle.main.url(
                    forResource: "transcript-agent-messages-wire", withExtension: "json"),
                let text = try? String(contentsOf: url, encoding: .utf8)
            else { return nil }
            let worst =
                text
                .replacingOccurrences(of: "cc-2@studio", with: sender)
                .replacingOccurrences(
                    of: "Please reply with the word ALPHA, use no tools.",
                    with: longMessage.replacingOccurrences(of: "\n", with: "\\n")
                        .replacingOccurrences(of: "\"", with: "\\\""))
            return (try? JSONDecoder().decode(JSONValue.self, from: Data(worst.utf8)))?.arrayValue
        }()

        /// The recording's times moved so its last event is a minute ago.
        private static func retimed(_ event: JSONValue, now: Date) -> JSONValue {
            guard var fields = event.objectValue, let seq = fields["seq"]?.intValue else {
                return event
            }
            fields["ts"] = .string(rfc3339(now.addingTimeInterval(TimeInterval(-90 + seq * 2))))
            return .object(fields)
        }

        private static let helloAck: JSONValue = {
            let text = """
                {"type":"hello_ack","protocol_version":1,"protocol_minor":27,\
                "server_time":"2026-10-31T00:00:00.000Z",\
                "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",\
                "answer_path":"hook_return","hold_secs":0,"send_text":true,"capture":true,\
                "delete_session":true,"test_push":false,"push":false,"tls":false,\
                "tls_active":false,"diff":true,"risk_class":true,"session_uid":true,\
                "send_text_idempotent":true,"slash_composer_recovery":true,\
                "prompt_identity":true,"command_catalog":true,"terminal_pty":true,\
                "question_card":true},"device_name":"iPhone"}
                """
            return (try? JSONDecoder().decode(JSONValue.self, from: Data(text.utf8))) ?? .null
        }()

        private static func sessions(from first: JSONValue, last: JSONValue, now: Date) -> JSONValue {
            let stamp = JSONValue.string(rfc3339(now))
            let session: [String: JSONValue] = [
                "session_uid": first["session_uid"] ?? .null,
                "session_id": first["session_id"] ?? .null,
                "tmux_session": first["session_id"] ?? .null,
                "cwd": .string("/Users/dev/api"),
                "project_label": .string("api"),
                "lifecycle": .string("live"), "link": .string("attached"),
                "last_seq": last["seq"] ?? .null,
                "created_at": stamp, "updated_at": stamp, "blocked_on": .array([]),
            ]
            return .object(["type": .string("sessions"), "sessions": .array([.object(session)])])
        }

        private static func rfc3339(_ date: Date) -> String {
            date.formatted(Date.ISO8601FormatStyle(includingFractionalSeconds: true))
        }
    }
#endif
