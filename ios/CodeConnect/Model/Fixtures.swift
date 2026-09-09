import CryptoKit
import Foundation

/// Contract-shaped daemon frames for driving the UI without a daemon.
///
/// **Ships in release**, because the sample fleet is replayed from here and a
/// reader who arrives with no Mac has no other way to see the product: every
/// screen but pairing is behind a pairing that needs one. It does not weaken the
/// rule it sits beside — the sample fleet creates no pairing and opens no
/// socket, so a release build still has no way to be *paired* except by a person
/// with a Mac.
///
/// Two rules keep this from becoming the mock that `CodeConnectUITests` exists
/// to avoid:
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

    /// The frames `startSampleFleet` replays: the deck, under the sample ack.
    ///
    /// Same sessions, cards and events as the harness's deck — one fleet,
    /// photographed and shipped alike — but the ack withholds what the sample
    /// cannot serve, so the screens it cannot reach are omitted rather than
    /// offered as dead ends.
    static func sampleFrames(now: Date = Date()) -> [ServerMessage] {
        var messages = frames(now: now, variant: .deck)
        if case .helloAck = messages.first, let ack = decode(sampleHelloAckJSON) {
            messages[0] = ack
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

    /// A current daemon's ack, capability for capability, as a paired device
    /// sees it. The render harness and the debug fixture flows photograph
    /// every screen, and a capability withheld here is a screen they cannot
    /// reach — so this ack advertises the full set and stays level with
    /// `protocol::PROTOCOL_MINOR` and `ws_server::capabilities`. `push` is
    /// false because a fixture daemon holds no APNs key, and `test_push`
    /// follows it for that reason. The release sample fleet does not use
    /// this ack; see `sampleHelloAckJSON`.
    private static let helloAckJSON = """
        {"type":"hello_ack","protocol_version":1,"protocol_minor":14,\
        "server_time":"2026-07-31T09:14:00.000Z",\
        "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",\
        "answer_path":"send_keys","hold_secs":0,"send_text":true,"capture":true,\
        "delete_session":true,"test_push":false,"push":false,"tls":false,\
        "tls_active":false,"diff":true,"risk_class":true,"session_uid":true,\
        "send_text_idempotent":true,"slash_composer_recovery":true,\
        "prompt_identity":true,"command_catalog":true,"terminal_pty":true},\
        "device_name":"iPhone"}
        """

    /// The sample fleet's ack: exactly what the sample serves and nothing
    /// more — the demo daemon's own rule, "an action this server cannot
    /// perform is not offered". `capture`, `delete_session`,
    /// `command_catalog` and `terminal_pty` are false because each one's
    /// request rides the connection, and the sample has none: advertising
    /// them dresses a dead end as a control. `send_text` stays true — typing
    /// is answered in sample vocabulary, not hidden — and `diff` is true
    /// because the sample preloads its own.
    private static let sampleHelloAckJSON = """
        {"type":"hello_ack","protocol_version":1,"protocol_minor":14,\
        "server_time":"2026-07-31T09:14:00.000Z",\
        "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",\
        "answer_path":"send_keys","hold_secs":0,"send_text":true,"capture":false,\
        "delete_session":false,"test_push":false,"push":false,"tls":false,\
        "tls_active":false,"diff":true,"risk_class":true,"session_uid":true,\
        "send_text_idempotent":true,"slash_composer_recovery":true,\
        "prompt_identity":true,"command_catalog":false,"terminal_pty":false},\
        "device_name":"iPhone"}
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

    /// Claude Code's own receipt for a command, delivered the way the real
    /// daemon delivers it: the send returns, and the transcript line
    /// arrives afterwards as an event.
    ///
    /// Without it the `kept` states are unreachable by any render, and a
    /// state nobody can reach is a state nobody has looked at.
    ///
    /// Every line is verbatim from the 2.1.223 measurement rig, escape
    /// codes and all: `Kept model as \u{1b}[1mFable 5\u{1b}[22m` is what
    /// the transcript actually holds, so the render exercises the same
    /// stripping the live path does.
    static func receiptFrame(mode: String, session: String, now: Date = Date()) -> String? {
        let line: String
        switch mode {
        case "kept-model":
            line = "Kept model as \u{1b}[1mFable 5\u{1b}[22m"
        case "kept-effort":
            line = "Kept effort level as high"
        case "set-effort-session":
            line =
                "Set effort level to max (this session only): Maximum capability with "
                + "deepest reasoning. May use excessive tokens resulting in long response "
                + "times or overthinking. Use sparingly for the hardest tasks."
        default:
            return nil
        }
        let escaped = line.replacingOccurrences(of: "\u{1b}", with: "\\u001b")
        // Seq 14, immediately after `confirmedFactsJSON`'s 12 and 13: a
        // higher number leaves a hole, and the session then carries a gap
        // and its banner through a scenario that is about a sheet. A
        // fixture adds the one fact it is for, not a fault as well.
        return """
            {"type":"event","event":{"seq":14,"session_id":"\(session)",\
            "ts":"\(rfc3339(now))","kind":"user_message","source":"transcript",\
            "payload":{"type":"user","message":{"role":"user",\
            "content":"<local-command-stdout>\(escaped)</local-command-stdout>"}}}}
            """
    }

    /// `-cc.debug.sendText <mode>` — what the stubbed daemon says to any
    /// send. `recovered` carries the measured pane above, as the real
    /// daemon does for the three snapshot commands. The `kept-*` and
    /// `set-effort-session` modes answer `sent` and let `receiptFrame`
    /// supply the transcript line, which is the shape of the real thing.
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
        case "duplicate":
            // The daemon replaying a settled mutation. Its `matched` and
            // timestamp are all the wire carries — deliberately not the
            // original outcome, which is why the sheets may not claim one.
            return .duplicate(matched: "foragents", appliedAt: "2026-08-05T14:31:55.000Z")
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
                "cwd":"/Users/dev/app-\(index)","project_label":"app-\(index)",\
                "lifecycle":"live","link":"attached",\
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
                "cwd":"/Users/dev/app-\(index)","project_label":"app-\(index)",\
                "lifecycle":"exited","link":"detached",\
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
            //
            // Not longer: a several-screen version starved XCUITest's own
            // accessibility snapshot over this selectable text.
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

    static func decode(_ json: String) -> ServerMessage? {
        try? JSONDecoder().decode(ServerMessage.self, from: Data(json.utf8))
    }

    private static func quoted(_ value: String) -> String {
        JSONValue.string(value).canonicalJSONString
    }

    private static func rfc3339(_ date: Date) -> String {
        date.formatted(Date.ISO8601FormatStyle(includingFractionalSeconds: true))
    }
}

// MARK: - Codex

/// **The Codex fleet, seeded from `fixtures/codex/*`.**
///
/// Every string below is measured, not invented — the `ui-verification-bar`
/// memory's first rule, applied literally:
///
///   * the 75-character command is the longest in the captured corpus;
///   * the amendment label is the daemon's own 120-character construction, which
///     wraps to four lines at reading size and is the widest thing any Codex card
///     draws;
///   * the `request_id` is the real 169-character `CompositeId` — the true width
///     worst case for any id a card can disclose;
///   * `gpt-5.3-codex-spark` is the model the live probes actually ran on, after
///     the operator's default was quota-refused;
///   * the file-change diffs are the captured ones, `/work/hello.txt` included.
///
/// `changes_omitted` is the one shape with **no capture** (contract §4.6): the
/// daemon emits it only past 32 files and the corpus never crossed that line. It
/// is constructed from the daemon's source and labelled as constructed wherever
/// it is rendered, so nobody mistakes it for a measurement.
enum CodexFixtures {

    /// **The longest command the corpus holds — 69 characters**, measured from
    /// `approval-amendment-labels-0.153.txt` section 4 (`wire command`). Its
    /// argument contains a space, which is why the daemon single-quotes it and
    /// why it is the widest one that still earns a full option table.
    static let longCommand =
        "/bin/zsh -lc \"touch '/tmp/cc-label-d.0000000000000000000 spaced.txt'\""

    /// **The daemon's own amendment label, at its measured 110 characters** —
    /// the longest in the corpus that is actually offered. It wraps to four
    /// lines at reading size and is the widest thing any Codex card draws; at
    /// AX5 it is where this card breaks if it is going to.
    ///
    /// The 113-character `f` variant is longer and is deliberately **not** here:
    /// its argv contains a line break, and the daemon withholds that option
    /// entirely rather than shorten a label it cannot shorten honestly. That
    /// case is the `card-two-options` state, not a longer label.
    static let longAmendmentLabel =
        "Yes, and don't ask again for commands that start with `touch '/tmp/cc-label-d.0000000000000000000 spaced.txt'`"

    /// **The real composite id, 159 characters** — the `command` card's, from
    /// `fixtures/codex/approval-card-0.153.json`. The true width worst case for
    /// any id a card can disclose, and its whole job in a fixture is to prove
    /// that nothing renders it.
    ///
    /// It is a literal because this is the **app** target and that file ships
    /// only in the test bundle: sourcing it here would mean bundling a test
    /// fixture into the shipped app to read one string out of it. So the
    /// literal is pinned instead —
    /// `CodexFixtureTests.testTheCompositeIdIsTheCardFixturesOwn` fails if it
    /// stops equalling the bundled card's `request_id`, and
    /// `CodexFixtureProvenanceTests` fails if that bundled card stops equalling
    /// the daemon's. The two together are the chain a literal cannot rot
    /// through: literal → bundled card → `fixtures/codex/` → the real
    /// `approval-0.153.jsonl` frames the Rust gate re-derives it from.
    ///
    /// It rotted exactly once, which is why the pin exists: the card fixture was
    /// rebuilt from the real frames and this stayed on the hand-authored card's
    /// id, still 159 characters, so the length check stayed green.
    static let compositeRequestID =
        "AQAaMDFLMUIzWFE4WkMwREU1RkdIN0pLTU5QQ1gAJDAxYTA2ZGIxLTFmOGUtN2RiMi04ZmQ1LTEwZjEzYWY1NWQxYgEAKWV4ZWMtZDI3MDBlZDMtYzY5ZC00OTE1LWE2MjAtMzZjMDAxZTdmNTc3AAAAAAAAAAE"

    /// The turn the interrupt capture really aborted.
    static let turnID = "01a073f6-2004-7750-a697-a6c12004ca48"
    static let threadID = "01a073f6-09c8-7c10-8212-4d369b80140b"

    /// Which Codex state to stage. One per HTML frame in the step-1 mock, so a
    /// render can be compared with the thing it was drawn from.
    enum State: String, CaseIterable {
        // Cards (A-series)
        case cardCommandWorst = "card-command-worst"
        case cardTwoOptions = "card-two-options"
        case cardFileChangeWide = "card-filechange-wide"
        case cardMinimal = "card-minimal"
        /// **The ceiling.** 32 files and ~128 KiB of diff — the largest card the
        /// Mac's own bounds allow. Nothing smaller exercises what a valid card
        /// can actually cost the decision surface.
        case cardCeiling = "card-ceiling"
        /// **The card that may be read and must not be answered.**
        ///
        /// Below minor 19 a Codex `approval_resolved` carries no `request_id`,
        /// so an answered card can never be retired — `DaemonProfile
        /// .resolvesCodexCards` is false and `DecisionCard` returns
        /// `.noneAnswerable`. That is a real shipping state on any Mac a user
        /// has not updated, and it was the one card state no render reached:
        /// `daemon-minor16` and `daemon-minor17` stage a dead **composer**, not
        /// a card, so the caveat above an unanswerable option list had never
        /// been photographed at any type size.
        ///
        /// Same card as `card-two-options`, on a minor-17 daemon — so what the
        /// render isolates is the read-only treatment and nothing else.
        case cardReadOnly = "card-read-only"
        // Resolutions (R-series)
        case resolvedAccepted = "resolved-accepted"
        case resolvedDeclined = "resolved-declined"
        case resolvedAtMac = "resolved-at-mac"
        case clearedTurnAborted = "cleared-turn-aborted"
        case clearedTurnCompleted = "cleared-turn-completed"
        case retiredItemCompleted = "retired-item-completed"
        case timeoutUnknown = "timeout-unknown"
        case writeUnknown = "write-unknown"
        // Stop (S-series)
        case stopOffered = "stop-offered"
        case stopAborted = "stop-aborted"
        /// **The phone's own gate.** `codex_link` is `offline`, so the frame
        /// never leaves: what is photographed is the app's "nothing was sent",
        /// in the app's own words. The daemon says nothing because it was never
        /// asked.
        case stopLinkDown = "stop-link-down"
        /// **The refusal after the fact.** The summary said `subscribed`, so the
        /// send path let it go, and the Mac answered that its link to Codex had
        /// gone down in between — which is the only way that sentence can ever
        /// reach a phone, and what D7 greys the control for ten seconds about.
        case stopRefusedLate = "stop-refused-late"
        case stopIndeterminate = "stop-indeterminate"
        // Compose (C-series)
        case composeStarted = "compose-started"
        case composeSteered = "compose-steered"
        case composeDuplicate = "compose-duplicate"
        case composeRejected = "compose-rejected"
        case composeIndeterminate = "compose-indeterminate"
        /// **The daemon's own capture, replayed frame for frame** —
        /// `fixtures/codex/phone-turn-stream-0.153.4.json`, the file ccd's
        /// tests byte-compare against. Nothing about this state is written by
        /// hand: the events are the bytes, and the fleet row is built from the
        /// identity and the high-water mark those bytes carry.
        ///
        /// It exists because every other Codex state on this list was
        /// hand-written, and hand-written in Claude's shape: a `user_message`
        /// as `{"message":{"content":…}}` where the adapter sends
        /// `{"text":…}`. Fixture and reader agreed with each other, neither
        /// agreed with the wire, and a real phone drew a turn as a lone "Turn
        /// complete". A render whose input is the daemon's own output cannot
        /// fail that way twice.
        case phoneTurnStream = "phone-turn-stream"
        // Daemon age (M-series)
        case daemonMinor17 = "daemon-minor17"
        case daemonMinor16 = "daemon-minor16"

        init?(_ raw: String?) {
            guard let raw, let value = State(rawValue: raw) else { return nil }
            self = value
        }

        /// **The protocol minor this state's daemon reports.**
        ///
        /// It was always 19, so `daemon-minor16` and `daemon-minor17` were
        /// capability permutations wearing the wrong names — and they carried
        /// `codex_link`, a minor-19 field, on the wire of a daemon that predates
        /// it. A degradation fixture that is not actually degraded proves
        /// nothing about degradation.
        var protocolMinor: UInt32 {
            switch self {
            case .daemonMinor16: return 16
            case .daemonMinor17, .cardReadOnly: return 17
            default: return 19
            }
        }

        /// Which capability flags the ack advertises for this state.
        var capabilities: (interrupt: Bool, compose: Bool) {
            switch self {
            // Minor 17 honours a stop and cannot decode a compose at all.
            // `card-read-only` is the same Mac, so it advertises the same thing:
            // a fixture that is old enough to refuse the card but modern enough
            // to compose would be a daemon that does not exist.
            case .daemonMinor17, .cardReadOnly: return (true, false)
            // Minor 16 does neither. The composer is dead **with a sentence**,
            // which is the state the verification bar warns reads as scolding
            // if it is not looked at on first open.
            case .daemonMinor16: return (false, false)
            default: return (true, true)
            }
        }

        /// The Codex control link's state for the session this stages.
        ///
        /// `nil` where the daemon is too old to have the field at all — the
        /// summary then omits it, and the phone's `#[serde(default)]`
        /// equivalent reads `none`, which is what such a fleet really is.
        var link: String? {
            guard protocolMinor >= 19 else { return nil }
            switch self {
            case .stopLinkDown: return "offline"
            default: return "subscribed"
            }
        }

        /// **Whether the card, when it was raised, had a turn.**
        ///
        /// D2 puts the approval's own turn on its envelope, and that is a fact
        /// about the moment the question was asked — not about the state the
        /// render ends in, and not about what later retired it. It was
        /// `stagesACard || resolutionRetiresTheTurn`, which left answered,
        /// timed-out, unknown-write and `item_completed` cards with no
        /// `turn_id` at all: minor-19 cards no daemon would send, standing in
        /// for the ones it does.
        ///
        /// Every card a minor-19 daemon raises was raised by a running turn, so
        /// the answer is simply the minor.
        var cardHadATurn: Bool { protocolMinor >= 19 }

        /// **Whether a turn is running in the state that is photographed.**
        ///
        /// The contract, not a Boolean compared against itself:
        ///
        ///   * `item_completed` means the ITEM finished and the turn is **still
        ///     running** — it was marked idle, which is the opposite of what the
        ///     cause means;
        ///   * a card still pending is a question a running turn is waiting on,
        ///     so `cardMinimal` cannot be both pending and idle;
        ///   * `turn_completed` and `turn_aborted` do end the turn.
        var turnRunningAtRender: Bool {
            switch self {
            case .clearedTurnCompleted, .clearedTurnAborted: return false
            // `item_completed`: the step finished, the turn did not.
            case .retiredItemCompleted: return true
            // A pending card is a running turn's own question.
            case .cardCommandWorst, .cardTwoOptions, .cardFileChangeWide, .cardMinimal,
                .cardCeiling, .cardReadOnly,
                .stopOffered, .stopAborted, .stopLinkDown, .stopRefusedLate, .stopIndeterminate:
                return true
            // An answer, a timeout or an unconfirmed write says nothing about
            // whether the turn went on; it did.
            case .resolvedAccepted, .resolvedDeclined, .resolvedAtMac, .timeoutUnknown,
                .writeUnknown:
                return true
            // **Codex was idle**, which is the whole point of `started`: the
            // events that describe the turn arrive AFTER the mutation, so the
            // fixture must not preload one.
            case .composeStarted: return false
            // A steer joins a turn that was already running, so the log has one.
            case .composeSteered, .composeDuplicate: return true
            case .composeRejected, .composeIndeterminate: return false
            case .daemonMinor16, .daemonMinor17: return false
            // The capture stages nothing: its two turns both completed in it,
            // and its events are the daemon's own bytes rather than anything
            // this file decides.
            case .phoneTurnStream: return false
            }
        }

        /// Whether this state's resolution is one that ends the turn with it.
        var resolutionRetiresTheTurn: Bool {
            switch self {
            case .clearedTurnAborted, .clearedTurnCompleted: return true
            default: return false
            }
        }

    }

    /// The session key every Codex fixture uses. `cx-1`, not `fx-N`: a Codex
    /// fixture must not collide with the Claude ones in a cache or a review mark.
    static let sessionKey = "cx-1"

    // MARK: Frames

    static func frames(state: State, now: Date = Date()) -> [ServerMessage] {
        var messages: [ServerMessage] = []
        if let ack = decodeOne(helloAck(state)) { messages.append(ack) }
        if let sessions = decodeOne(sessions(state, now: now)) { messages.append(sessions) }
        if state == .phoneTurnStream {
            // The capture's own frames, decoded through the same
            // `ServerMessage` decoder the socket uses. Not re-serialised from
            // anything of ours: a fixture that has been through this app's
            // encoder proves only that the app agrees with itself.
            return messages + capturedStream().map(\.message)
        }
        for json in events(state, now: now) {
            if let message = decodeOne(json) { messages.append(message) }
        }
        return messages
    }

    /// **The session identity a state's fleet row carries.** Every hand-made
    /// state is `cx-1`; the captured stream is whatever the daemon recorded,
    /// because rewriting the ids in a capture to suit the app is the same
    /// mistake as writing the payloads by hand.
    static func fleetKey(for state: State) -> String {
        guard state == .phoneTurnStream else { return sessionKey }
        return capturedStream().compactMap(\.event).first?.sessionKey ?? sessionKey
    }

    /// The capture, parsed once: each frame as the `ServerMessage` it decodes
    /// to, paired with its `Event` where it is one.
    ///
    /// A frame that fails to decode is **dropped loudly** rather than silently:
    /// `testEveryStateProducesDecodableFrames` counts the frames and
    /// `testTheDaemonsOwnPhoneTurnCaptureDecodesWhole` decodes every one of
    /// them, so a shape this build cannot read fails a test rather than a
    /// render.
    private static let capturedStreamCache: [(message: ServerMessage, event: Event?)] = {
        guard
            let url = Bundle.main.url(
                forResource: "phone-turn-stream-0.153.4", withExtension: "json"),
            let data = try? Data(contentsOf: url),
            let root = try? JSONDecoder().decode(JSONValue.self, from: data),
            let frames = root["frames"]?.arrayValue
        else { return [] }
        return frames.compactMap { frame in
            guard let message = decodeOne(frame.canonicalJSONString) else { return nil }
            return (message, frame["event"]?.decoded(Event.self))
        }
    }()

    private static func capturedStream() -> [(message: ServerMessage, event: Event?)] {
        capturedStreamCache
    }

    private static func helloAck(_ state: State) -> String {
        let caps = state.capabilities
        return """
            {"type":"hello_ack","protocol_version":1,"protocol_minor":\(state.protocolMinor),\
            "server_time":"2026-09-04T21:31:59.000Z",\
            "capabilities":{"can_approve_reliably":true,"fail_mode":"fail_open",\
            "answer_path":"codex_response","hold_secs":0,"send_text":true,"capture":true,\
            "delete_session":true,"test_push":false,"push":false,"tls":false,\
            "tls_active":false,"diff":true,"risk_class":true,"session_uid":true,\
            "send_text_idempotent":true,"slash_composer_recovery":true,\
            "prompt_identity":true,"command_catalog":true,"terminal_pty":true,\
            "codex_interrupt":\(caps.interrupt),"codex_compose":\(caps.compose),\
            "supported_agents":["claude","codex"]},\
            "device_name":"iPhone"}
            """
    }

    /// Two runs: the Codex one this state is about, and a Claude neighbour.
    ///
    /// **The neighbour is not decoration.** The verification bar's fourth rule
    /// is that a new surface is compared against its neighbours row for row, and
    /// the Codex badge and the Stop pill both live on a fleet row that has to
    /// stay in line with a Claude one. Without a Claude row in the same render
    /// there is nothing to compare against.
    private static func sessions(_ state: State, now: Date) -> String {
        let stamp = rfc3339(now)
        let blocked = state.stagesACard ? "\"\(compositeRequestID)\"" : ""
        // Omitted entirely below minor 19 — the field does not exist there, and
        // a degradation fixture that carries it is not degraded.
        let linkClause = state.link.map { #","codex_link":"\#($0)""# } ?? ""
        // The fleet's own high-water mark, matching what the events really
        // carry. Advertised higher, `subscribeIfNeeded` keeps asking for events
        // that do not exist; advertised at all when the session has none is the
        // same lie in miniature.
        let lastSeq =
            state == .phoneTurnStream
            ? Int(capturedStream().compactMap { $0.event?.seq }.max() ?? 0)
            : events(state, now: now).count
        // The captured stream keeps the identity and the working directory the
        // daemon recorded; every hand-made state keeps `cx-1`.
        let key = fleetKey(for: state)
        let tmux = capturedStream().compactMap(\.event).first?.sessionID ?? key
        let cwd = state == .phoneTurnStream ? "/work" : "/work/p-abort-pending"
        let label = state == .phoneTurnStream ? "work" : "p-abort-pending"
        return """
            {"type":"sessions","sessions":[\
            {"session_uid":"\(key)","session_id":"\(tmux)",\
            "tmux_session":"\(tmux)","cwd":"\(cwd)",\
            "project_label":"\(label)","lifecycle":"live","link":"attached",\
            "last_seq":\(lastSeq),"created_at":"\(stamp)","updated_at":"\(stamp)",\
            "blocked_on":[\(blocked)],"agent":"codex",\
            "codex_thread_id":"\(threadID)"\(linkClause)},\
            {"session_uid":"fx-9","session_id":"fx-9","tmux_session":"fx-9",\
            "cwd":"/Users/dev/proj","project_label":"proj","lifecycle":"live",\
            "link":"attached","last_seq":0,"created_at":"\(stamp)",\
            "updated_at":"\(stamp)","blocked_on":[]}]}
            """
    }

    /// **Contiguous `seq`, always.**
    ///
    /// `SessionState` reads a jump in `seq` as events it never received and says
    /// so on screen — correctly. A fixture numbered 10, 20, 30 therefore prints
    /// `RESYNCED, 18 EVENTS NOT SHOWN` across every render it appears in, which
    /// is a second, false story competing with the one the render is about. So
    /// the events are numbered as they are appended, and
    /// `testTheFixtureSequenceIsContiguous` keeps it that way. (Caught by
    /// looking at `codex-resolved-at-mac--L.png`.)
    private static func events(_ state: State, now: Date) -> [String] {
        var out: [String] = []
        var seq = 0
        func next() -> Int {
            seq += 1
            return seq
        }

        // A running turn, carried on the envelope.
        if state.turnRunningAtRender {
            out.append(
                codexEvent(
                    seq: next(), kind: "tool_call", turn: turnID, now: now,
                    // `tool_call_payload`: `tool`, `command`, `cwd` — the hook's
                    // `tool_name`/`tool_input` is Claude's spelling and drew
                    // every Codex tool row as an unnamed "tool".
                    payload: #"{"tool":"command_execution","command":"\#(escaped(longCommand))","cwd":"/work/p-abort-pending"}"#
                ))
        }
        if let card = cardJSON(state) {
            // **D2: the card carries its own turn on the envelope.** Minor 19
            // added it, so Stop on a card needs no inference at all — the
            // derivation from preceding item events stays as the fallback for
            // cards raised by a daemon that predates the field.
            // **The turn the card was raised under**, which is a fact about
            // when the question was asked. Decided from the final state, a card
            // later cleared by turn completion arrived with no turn at all.
            let turnClause = state.cardHadATurn ? #","turn_id":"\#(turnID)"# + "\"" : ""
            out.append(
                """
                {"type":"event","event":{"seq":\(next()),"session_uid":"\(sessionKey)",\
                "session_id":"\(sessionKey)","ts":"\(rfc3339(now.addingTimeInterval(-60)))",\
                "kind":"approval_request","source":"daemon"\(turnClause),\
                "source_event_id":"perm:\(compositeRequestID)",\
                "payload":{"card":\(card)}}}
                """)
        }
        if let resolution = resolutionJSON(state) {
            out.append(
                """
                {"type":"event","event":{"seq":\(next()),"session_uid":"\(sessionKey)",\
                "session_id":"\(sessionKey)","ts":"\(rfc3339(now.addingTimeInterval(-20)))",\
                "kind":"approval_resolved","source":"daemon",\
                "source_event_id":"resolved:\(compositeRequestID)",\
                "payload":{"request_id":"\(compositeRequestID)",\(resolution)}}}
                """)
        }
        // A turn that ended, where the state says the session is idle — and
        // only where a turn existed to end.
        if !state.turnRunningAtRender, state.cardHadATurn {
            out.append(
                codexEvent(
                    seq: next(), kind: "turn_complete", turn: turnID, now: now,
                    // The adapter's own turn_complete payload, whole:
                    // status, both timestamps, duration, and a null error.
                    payload: #"{"completed_at":"\#(rfc3339(now))","duration_ms":6385,"error":null,"started_at":"\#(rfc3339(now.addingTimeInterval(-6.385)))","status":"completed"}"#
                ))
        }
        // What the phone said, for the compose states — so the C-series renders
        // show a real exchange rather than an empty timeline under a banner.
        if state.stagesACompose {
            // **No turn on the envelope.** These are the exchange the compose
            // produced, and the phone learns which turn it landed in from the
            // `ComposeResult` itself — that is the whole point of F6. Putting
            // the turn here too would preload the fact the mutation is supposed
            // to deliver, and for `started` it would contradict the arm being
            // staged: `started` means Codex was idle when the words arrived.
            // **The daemon's real message shape, not Claude's.** These two
            // rows were hand-written as `{"message":{"content":…}}` — the
            // transcript shape a Claude session has — and the adapter has never
            // sent it: `codex_adapter.rs` `message_payload` emits a flat
            // `{"text":…, "interrupted":…}`. Every render and every test passed
            // over the difference because the fixture and the reader agreed
            // with each other and neither agreed with the wire, and a phone-
            // started turn on the operator's build 73 drew a lone "Turn
            // complete" over a conversation the daemon had logged in full.
            out.append(
                codexEvent(
                    seq: next(), kind: "user_message", turn: nil, now: now,
                    payload:
                        #"{"interrupted":false,"text":"Run the shell command touch /work/marker.txt now. Do not explain, just run it."}"#
                ))
            out.append(
                codexEvent(
                    seq: next(), kind: "agent_message", turn: nil, now: now,
                    payload:
                        #"{"interrupted":false,"text":"I'm creating marker.txt in the current working directory now."}"#
                ))
        }
        return out
    }

    private static func codexEvent(
        seq: Int, kind: String, turn: String?, now: Date, payload: String
    ) -> String {
        let turnClause = turn.map { #","turn_id":"\#($0)""# } ?? ""
        return """
            {"type":"event","event":{"seq":\(seq),"session_uid":"\(sessionKey)",\
            "session_id":"\(sessionKey)","ts":"\(rfc3339(now))","kind":"\(kind)",\
            "source":"codex"\(turnClause),"payload":\(payload)}}
            """
    }

    // MARK: Cards

    private static func cardJSON(_ state: State) -> String? {
        switch state {
        case .cardCommandWorst, .resolvedAccepted, .resolvedDeclined, .resolvedAtMac,
            .clearedTurnAborted, .clearedTurnCompleted, .timeoutUnknown, .writeUnknown,
            .stopOffered, .stopAborted, .stopLinkDown, .stopRefusedLate, .stopIndeterminate:
            return card(
                toolName: "command",
                input: """
                    {"command":"\(escaped(longCommand))","cwd":"/work/p-abort-pending",\
                    "options":[{"id":"accept","label":"Yes, proceed"},\
                    {"id":"acceptWithExecpolicyAmendment","label":"\(escaped(longAmendmentLabel))",\
                    "payload":{"execpolicy_amendment":["touch","marker.txt"]}},\
                    {"id":"cancel","label":"No, and tell Codex what to do differently"}],\
                    "reason":"Allow creating the requested directory in /tmp?"}
                    """)

        // **Two options is a real shape**: the daemon withholds the amendment
        // entirely when its argv contains a line break rather than shortening a
        // label it cannot shorten honestly. This is the card that was
        // unanswerable before this phase.
        //
        // `card-read-only` is deliberately the SAME bytes on an older daemon:
        // the only difference between the two renders is the read-only
        // treatment, so the photograph isolates it.
        case .cardTwoOptions, .cardReadOnly:
            return card(
                toolName: "command",
                input: """
                    {"command":"/bin/zsh -lc 'touch marker.txt'","cwd":"/work/p-accept",\
                    "options":[{"id":"accept","label":"Yes, proceed"},\
                    {"id":"cancel","label":"No, and tell Codex what to do differently"}],\
                    "reason":"the sandbox is read-only"}
                    """)

        // MAX_CHANGES files at MAX_TOTAL_DIFF_BYTES, which is what the daemon
        // will really send: a card is bounded, and this is the bound.
        case .cardCeiling:
            return card(toolName: "file change", input: ceilingInput())

        case .cardFileChangeWide, .retiredItemCompleted:
            return card(
                toolName: "file change",
                input: """
                    {"path":"/work/hello.txt","changes":[\
                    {"path":"/work/hello.txt","kind":{"type":"update","move_path":null},\
                    "diff":"@@ -1 +1 @@\\n-hello from the codex approvals probe\\n+goodbye from the codex approvals probe\\n"},\
                    {"path":"/work/marker.txt","kind":{"type":"add","move_path":null},\
                    "diff":"@@ -0,0 +1 @@\\n+goodbye\\n"},\
                    {"path":"/tmp/cc-label-f.0000000000000000/newline.txt",\
                    "kind":{"type":"update","move_path":null},\
                    "diff":"@@ -1 +1 @@\\n-hello\\n+goodbye\\n"}],\
                    "changes_omitted":{"count":33,\
                    "sha256":"bbe20d76f734f1833ffdf048fa5d2f2e15d3c7df06875188366e08ae23b891a3"},\
                    "options":[{"id":"accept","label":"Yes, proceed"},\
                    {"id":"acceptForSession","label":"Yes, and don't ask again for these files"},\
                    {"id":"cancel","label":"No, and tell Codex what to do differently"}]}
                    """)

        // **Every optional field absent**, which is the measured `file change`
        // shape: no reason, no grant_root, no prompt_id, no permission_mode, no
        // permission_suggestions, no matched_pattern. `identity_bound` is always
        // false for Codex — there is no pane prompt to fingerprint.
        case .cardMinimal:
            return card(
                toolName: "file change",
                input: """
                    {"path":"/work/hello.txt","changes":[\
                    {"path":"/work/hello.txt","kind":{"type":"update","move_path":null},\
                    "diff":"@@ -1 +1 @@\\n-hello\\n+goodbye\\n"}],\
                    "options":[{"id":"accept","label":"Yes, proceed"},\
                    {"id":"acceptForSession","label":"Yes, and don't ask again for these files"},\
                    {"id":"cancel","label":"No, and tell Codex what to do differently"}]}
                    """)

        case .composeStarted, .composeSteered, .composeDuplicate, .composeRejected,
            .composeIndeterminate, .daemonMinor17, .daemonMinor16:
            return nil
        // The capture carries its own two cards, in the daemon's own bytes.
        case .phoneTurnStream:
            return nil
        }
    }

    /// **The largest valid card**: `MAX_CHANGES` (32) files sharing
    /// `MAX_TOTAL_DIFF_BYTES` (128 KiB), built from fixture-shaped hunks.
    ///
    /// Each file gets ~4 KiB of diff, which is what 128 KiB over 32 files means.
    /// The point is not that it is pretty — it is that a card the daemon is
    /// allowed to send must not make the decision surface unresponsive, and no
    /// fixture below this size can say whether it does.
    private static func ceilingInput() -> String {
        // 128 KiB / 32 files, and the loop stops **before** crossing it: a
        // fixture that exceeds `MAX_TOTAL_DIFF_BYTES` is not the ceiling, it is
        // a card the daemon would never have sent.
        let perFile = 131_072 / 32
        var changes: [String] = []
        for index in 0..<32 {
            let path = "/work/pkg/module\(index)/Source\(index).swift"
            var diff = "@@ -1,\(perFile / 64) +1,\(perFile / 64) @@\n"
            var line = 0
            while true {
                let next =
                    line % 3 == 0
                    ? "-    let value\(line) = compute(\(line))\n"
                    : (line % 3 == 1
                        ? "+    let value\(line) = compute(\(line), cached: true)\n"
                        : "     // unchanged context line \(line)\n")
                guard diff.utf8.count + next.utf8.count <= perFile else { break }
                diff += next
                line += 1
            }
            changes.append(
                """
                {"path":"\(path)","kind":{"type":"update","move_path":null},\
                "diff":\(quotedString(diff))}
                """)
        }
        return """
            {"path":"/work/pkg/module0/Source0.swift","changes":[\(changes.joined(separator: ","))],\
            "options":[{"id":"accept","label":"Yes, proceed"},\
            {"id":"acceptForSession","label":"Yes, and don't ask again for these files"},\
            {"id":"cancel","label":"No, and tell Codex what to do differently"}]}
            """
    }

    /// A card whose `payload_hash` verifies for real, exactly as the daemon
    /// builds it: `SHA-256("{tool_name}\n{tool_input}")`. A fixture card that
    /// failed verification would render the *blocked* path — a banner and two
    /// dead controls — rather than the card the render is for.
    private static func card(toolName: String, input: String) -> String {
        // **Canonicalised before it is hashed**, and the same canonical text is
        // what goes on the wire. `display_text` is `"{tool_name}\n{tool_input}"`
        // and the phone checks that its own render of `tool_input` reproduces
        // it — a check that compares against `JSONValue.canonicalJSONString`,
        // which sorts keys. A hand-written literal whose keys happen to be in a
        // different order therefore fails the *structured* half of the gate and
        // renders the daemon's raw text instead of the card. Measured: three of
        // these did exactly that.
        let canonical =
            (try? JSONDecoder().decode(JSONValue.self, from: Data(input.utf8)))?
            .canonicalJSONString ?? input
        let display = "\(toolName)\n\(canonical)"
        let hash = SHA256.hash(data: Data(display.utf8))
            .map { String(format: "%02x", $0) }.joined()
        return """
            {"request_id":"\(compositeRequestID)","payload_hash":"\(hash)",\
            "tool_name":"\(toolName)","tool_input":\(canonical),\
            "display_text":\(quotedString(display)),"risk":{"class":"medium"},\
            "generation":1,"identity_bound":false}
            """
    }

    // MARK: Resolutions

    private static func resolutionJSON(_ state: State) -> String? {
        switch state {
        case .resolvedAccepted:
            return #""status":"answered","by":"phone","decision":{"type":"option_id","option_id":"accept"}"#
        case .resolvedDeclined:
            return #""status":"answered","by":"phone","decision":{"type":"option_id","option_id":"cancel"}"#
        // The common case for a keyboard answer: `decision` ABSENT, because the
        // Codex wire carries no provenance for one.
        case .resolvedAtMac:
            return #""status":"answered","by":"local""#
        case .clearedTurnAborted:
            return #""status":"cleared","cause":"turn_aborted""#
        case .clearedTurnCompleted:
            return #""status":"cleared","cause":"turn_completed""#
        case .retiredItemCompleted:
            return #""status":"cleared","cause":"item_completed""#
        case .timeoutUnknown:
            return #""status":"timeout""#
        case .writeUnknown:
            return """
                "status":"unknown","attempted_by":"phone",\
                "attempted_decision":{"type":"option_id","option_id":"accept"},\
                "write_stage":"upstream_write_unconfirmed",\
                "cause":"connection reset before ack"
                """
        default:
            return nil
        }
    }

    // MARK: Staged mutation outcomes

    /// **Whether the render presses Stop**, which is not the same question as
    /// whether an answer is staged.
    ///
    /// It used to be derived from `interruptResult != nil`, and that made one
    /// state unphotographable: `stop-link-down`'s whole subject is the *phone's*
    /// refusal, where the frame never leaves and so no daemon answer exists to
    /// stage. Press and answer are two facts, and the send-path gate sits
    /// between them — which is exactly the thing being photographed.
    static func pressesStop(_ state: State) -> Bool {
        switch state {
        case .stopAborted, .stopLinkDown, .stopRefusedLate, .stopIndeterminate: return true
        default: return false
        }
    }

    /// The same distinction for the composer.
    static func pressesCompose(_ state: State) -> Bool {
        switch state {
        case .composeStarted, .composeSteered, .composeDuplicate, .composeRejected,
            .composeIndeterminate, .daemonMinor16, .daemonMinor17:
            return true
        default: return false
        }
    }

    /// What the stubbed daemon answers a stop with, or nil where the state does
    /// not stage one.
    static func interruptResult(_ state: State) -> InterruptResult? {
        switch state {
        case .stopAborted: return .aborted(turnID: turnID)
        // Verbatim from `state.rs`. The button greys for ten seconds and comes
        // back — the sentence itself says to try again shortly.
        case .stopRefusedLate:
            return .rejected(
                reason:
                    "this Mac has lost its control link to the Codex session and is reconnecting, "
                    + "so nothing was sent; try again shortly, or stop the turn at the Mac")
        case .stopIndeterminate:
            return .indeterminate(
                reason:
                    "this interrupt was already sent and what became of it is not known; "
                    + "it will not be sent again. Check the Mac.")
        default: return nil
        }
    }

    static func composeResult(_ state: State) -> ComposeResult? {
        switch state {
        case .composeStarted: return .started(turnID: turnID)
        case .composeSteered: return .steered(turnID: turnID)
        // `started: false` — a replay of words that STEERED. The route was
        // snapshotted at the claim, so the verb is the one that was true when
        // they landed, not the one that would be true now.
        case .composeDuplicate: return .duplicate(turnID: turnID, started: false)
        case .composeRejected:
            return .rejected(
                reason:
                    "this Mac is connected to the Codex session but is not yet watching its thread, "
                    + "so a message cannot be confirmed; nothing was sent")
        // **`compose_already_sent_unknown`, verbatim.** This was a hand-written
        // approximation — "that message was written and what became of it is
        // not known" — which is no row the daemon has ever had, so the render
        // showed a reader a sentence no Mac sends. Same category and so the
        // same greying, which is why nothing failed;
        // `testEveryStagedRefusalIsASentenceTheDaemonReallySends` is what fails
        // now.
        case .composeIndeterminate:
            return .indeterminate(
                reason:
                    "this message was already sent and what became of it is not known; "
                    + "it will not be sent again. Check the Mac.")
        default: return nil
        }
    }

    // MARK: Helpers

    private static func decodeOne(_ json: String) -> ServerMessage? {
        try? JSONDecoder().decode(ServerMessage.self, from: Data(json.utf8))
    }

    private static func quotedString(_ value: String) -> String {
        JSONValue.string(value).canonicalJSONString
    }

    /// The contents of a JSON string, without its quotes — for interpolating
    /// a measured string into a hand-written JSON literal.
    private static func escaped(_ value: String) -> String {
        String(quotedString(value).dropFirst().dropLast())
    }

    private static func rfc3339(_ date: Date) -> String {
        date.formatted(Date.ISO8601FormatStyle(includingFractionalSeconds: true))
    }
}

extension CodexFixtures.State {
    var stagesACard: Bool {
        switch self {
        case .composeStarted, .composeSteered, .composeDuplicate, .composeRejected,
            .composeIndeterminate, .daemonMinor17, .daemonMinor16:
            return false
        default:
            // A card that has been resolved is no longer blocking the run, so
            // it does not belong in `blocked_on` either.
            return CodexFixtures.isUnresolved(self)
        }
    }

    var stagesACompose: Bool {
        switch self {
        case .composeStarted, .composeSteered, .composeDuplicate, .composeRejected,
            .composeIndeterminate:
            return true
        default: return false
        }
    }
}

extension CodexFixtures {
    fileprivate static func isUnresolved(_ state: State) -> Bool {
        switch state {
        case .cardCommandWorst, .cardTwoOptions, .cardFileChangeWide, .cardMinimal,
            .cardCeiling, .cardReadOnly,
            .stopOffered, .stopAborted, .stopLinkDown, .stopRefusedLate, .stopIndeterminate:
            return true
        default: return false
        }
    }
}
