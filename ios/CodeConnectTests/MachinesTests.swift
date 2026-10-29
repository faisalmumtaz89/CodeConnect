import XCTest

@testable import CodeConnect

/// One model per paired machine, one fleet made of them: each session stays on
/// the machine it came from, and one machine leaving takes nothing of another's.
@MainActor
final class MachinesTests: XCTestCase {

    private let studio = DaemonEndpoint(
        host: "studio.tail1234.ts.net", port: 8787, credential: .token("studio-token"),
        useTLS: false)
    private let laptop = DaemonEndpoint(
        host: "laptop.tail1234.ts.net", port: 8787, credential: .token("laptop-token"),
        useTLS: false)

    /// Both machines paired, in memory: an ephemeral store saves without the
    /// Keychain.
    private func twoMachines() -> Machines {
        let pairing = PairingStore(ephemeral: true)
        pairing.save(studio)
        pairing.save(laptop)
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("machines-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let machines = Machines(
            pairing: pairing, cache: EventCache(root: root),
            settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!))
        for model in machines.models {
            model.connection.simulateConnectedForTesting()
            model.connection.simulateCapabilitiesForTesting(
                Capabilities(extra: ["delete_session": .bool(true), "session_uid": .bool(true)]),
                minor: 7)
        }
        return machines
    }

    private func summary(_ uid: String, lifecycle: String = "live") -> SessionSummary {
        // swiftlint:disable:next force_try
        try! JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"\(uid)","session_id":"cc-1","tmux_session":"cc-1",
                 "cwd":"/tmp/x","lifecycle":"\(lifecycle)","link":"attached","last_seq":0,
                 "created_at":"2026-10-28T10:00:00Z","updated_at":"2026-10-28T10:00:00Z",
                 "blocked_on":[]}
                """.utf8))
    }

    private let onStudio = "01K8N0AAAAAAAAAAAAAAAAAAAA"
    private let onLaptop = "01K8N0BBBBBBBBBBBBBBBBBBBB"

    func testEveryPairedMachineHasAModelOfItsOwn() {
        let machines = twoMachines()
        XCTAssertEqual(machines.models.map(\.endpoint), [studio, laptop])
        XCTAssertFalse(machines.models[0].connection === machines.models[1].connection)
    }

    /// Two machines' `cc-1` are two rows, each naming its own machine, and each
    /// resolves to the model it came from.
    func testTheFleetHoldsEveryMachinesSessionsEachOnItsOwnMachine() throws {
        let machines = twoMachines()
        let (studioModel, laptopModel) = (machines.models[0], machines.models[1])
        studioModel.adoptSummariesForTesting([summary(onStudio)])
        laptopModel.adoptSummariesForTesting([summary(onLaptop)])

        let rows = machines.fleet
        XCTAssertEqual(Set(rows.map(\.id)), [onStudio, onLaptop])
        let studioRow = try XCTUnwrap(rows.first { $0.id == onStudio })
        XCTAssertEqual(studioRow.machineID, studioModel.machineID)
        XCTAssertEqual(studioRow.machineName, studio.host)
        XCTAssertTrue(machines.model(forSession: onStudio) === studioModel)
        XCTAssertTrue(machines.model(forSession: onLaptop) === laptopModel)
    }

    /// A machine whose link drops is that machine's trouble alone: its rows
    /// lead to its link, the other's stay actionable, and the app-wide surfaces
    /// speak for the one still live.
    func testOneMachineOfflineIsOneMachinesTrouble() throws {
        let machines = twoMachines()
        machines.models[0].adoptSummariesForTesting([summary(onStudio)])
        machines.models[1].adoptSummariesForTesting([summary(onLaptop)])
        machines.models[1].connection.setPhaseForTesting(.waiting(until: Date(), reason: "down"))

        let rows = machines.fleet
        let laptopRow = try XCTUnwrap(rows.first { $0.id == onLaptop })
        let studioRow = try XCTUnwrap(rows.first { $0.id == onStudio })
        XCTAssertEqual(machines.model(for: laptopRow.machineID)?.linkHealth.level, .offline)
        XCTAssertNil(machines.model(for: studioRow.machineID)?.actionsBlockedReason)
        XCTAssertTrue(machines.linkModel === machines.models[0])
    }

    /// Removal goes to each session's own machine, and a machine that stops
    /// answering stops only its own part of the batch.
    func testABatchRemovesEachSessionOnItsOwnMachine() async {
        let machines = twoMachines()
        let (studioModel, laptopModel) = (machines.models[0], machines.models[1])
        studioModel.adoptSummariesForTesting([summary(onStudio, lifecycle: "exited")])
        laptopModel.adoptSummariesForTesting([summary(onLaptop, lifecycle: "exited")])
        laptopModel.connection.deleteStub = { _ in throw ConnectionError.sentButUnanswered }
        studioModel.connection.deleteStub = { _ in .deleted(events: 1) }

        let refused = await machines.removeSessions(keys: [onLaptop, onStudio])

        XCTAssertEqual(studioModel.connection.deleteRequests, [onStudio])
        XCTAssertEqual(laptopModel.connection.deleteRequests, [onLaptop])
        XCTAssertEqual(refused, [onLaptop: "No answer"])
        XCTAssertTrue(studioModel.summaries.isEmpty)
    }

    /// Stop on a row reaches the machine the row is on, through the model the
    /// fleet hands that row (`model(for: row.machineID)`), and no other machine
    /// is sent anything. Both machines run a Codex turn under the same name.
    func testAStopGoesToTheMachineTheRowIsOn() async throws {
        let machines = twoMachines()
        var interrupts: [String: [String]] = [:]
        for (model, uid) in zip(machines.models, [onStudio, onLaptop]) {
            let machineID = try XCTUnwrap(model.machineID)
            model.connection.simulateCapabilitiesForTesting(
                Capabilities(
                    canApproveReliably: true,
                    extra: ["codex_interrupt": .bool(true), "session_uid": .bool(true)]),
                minor: 19)
            model.connection.injectForTesting(.sessions([codexSummary(uid)]))
            model.connection.injectForTesting(.event(codexTurn(uid)))
            model.connection.sendStub = { [weak connection = model.connection] message in
                guard case .interrupt(let session, let requestID, _, _) = message else { return }
                interrupts[machineID, default: []].append(session)
                connection?.injectForTesting(
                    .interruptResult(
                        sessionID: session, requestID: requestID, result: .aborted(turnID: "turn-9")))
            }
        }
        let row = try XCTUnwrap(machines.fleet.first { $0.id == onLaptop })

        _ = await machines.model(for: row.machineID)?.stopCodexTurn(sessionKey: row.id)

        XCTAssertEqual(interrupts[machines.models[1].machineID!], [onLaptop])
        XCTAssertNil(interrupts[machines.models[0].machineID!], "the other machine is sent nothing")
    }

    /// What left each machine's socket, by machine id: the frame and the run it
    /// names.
    private final class Sent {
        var frames: [String: [String]] = [:]
    }

    /// Both machines live, each with a Claude `cc-1` holding a card with the
    /// same request id, and every frame recorded where it leaves.
    private func routed() throws -> (Machines, Sent) {
        let machines = twoMachines()
        let sent = Sent()
        for (model, uid) in zip(machines.models, [onStudio, onLaptop]) {
            let machineID = try XCTUnwrap(model.machineID)
            model.connection.simulateCapabilitiesForTesting(
                Capabilities(
                    sendText: true, capture: true,
                    extra: ["hook_only_approvals": .bool(true), "session_uid": .bool(true)]),
                minor: 22)
            model.connection.injectForTesting(.sessions([claudeSummary(uid)]))
            model.connection.injectForTesting(.event(approvalRequest(uid)))
            model.connection.sendStub = { [weak connection = model.connection] message in
                switch message {
                case .answer(_, _, _, let session):
                    sent.frames[machineID, default: []].append("answer \(session ?? "")")
                    throw ConnectionError.notConnected
                case .capture(let session, _):
                    sent.frames[machineID, default: []].append("capture \(session)")
                    connection?.injectForTesting(.captureResult(sessionID: session, text: "pane"))
                default:
                    break
                }
            }
            model.connection.sendTextStub = { @MainActor session, _ in
                sent.frames[machineID, default: []].append("send_text \(session)")
                return .sent(matched: "foragents")
            }
        }
        return (machines, sent)
    }

    /// A card in the Deck is answered through the model of the machine its run
    /// is on, as the Deck hands each card (`model(forSession:)`).
    func testAnAnswerGoesToTheMachineTheCardIsOn() async throws {
        let (machines, sent) = try routed()
        let laptop = try XCTUnwrap(machines.models[1].statesForTesting[onLaptop])
        await laptop.settleForTesting()
        let card = try XCTUnwrap(laptop.pendingApprovals.first)

        _ = await machines.model(forSession: card.sessionKey)?.answer(item: card, decision: .allow)

        XCTAssertEqual(sent.frames[machines.models[1].machineID!], ["answer \(onLaptop)"])
        XCTAssertNil(sent.frames[machines.models[0].machineID!], "the other machine is sent nothing")
    }

    /// Text composed on a session's screen goes to that session's machine.
    func testComposedTextGoesToTheMachineTheSessionIsOn() async throws {
        let (machines, sent) = try routed()

        _ = await machines.model(forSession: onLaptop)?.send(text: "hello", to: onLaptop)

        XCTAssertEqual(sent.frames[machines.models[1].machineID!], ["send_text \(onLaptop)"])
        XCTAssertNil(sent.frames[machines.models[0].machineID!], "the other machine is sent nothing")
    }

    /// A snapshot of a session's screen is captured on that session's machine.
    func testASnapshotIsCapturedOnTheMachineTheSessionIsOn() async throws {
        let (machines, sent) = try routed()

        _ = await machines.model(forSession: onLaptop)?.capturePane(key: onLaptop)

        XCTAssertEqual(sent.frames[machines.models[1].machineID!], ["capture \(onLaptop)"])
        XCTAssertNil(sent.frames[machines.models[0].machineID!], "the other machine is sent nothing")
    }

    private func claudeSummary(_ uid: String) -> SessionSummary {
        // swiftlint:disable:next force_try
        try! JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"\(uid)","session_id":"cc-1","tmux_session":"cc-1",
                 "cwd":"/work","project_label":"work","lifecycle":"live","link":"attached",
                 "last_seq":1,"created_at":"2026-10-28T10:00:00Z",
                 "updated_at":"2026-10-28T10:00:00Z","blocked_on":[],"agent":"claude"}
                """.utf8))
    }

    private func approvalRequest(_ uid: String) -> Event {
        // swiftlint:disable:next force_try
        try! JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":1,"session_uid":"\(uid)","session_id":"cc-1","ts":"2026-10-28T10:00:00Z",
                 "kind":"approval_request","source":"hook","payload":{"card":{
                   "request_id":"toolu_1","payload_hash":"abc","tool_name":"Bash",
                   "tool_input":{"command":"ls"},"display_text":"Bash"}}}
                """.utf8))
    }

    private func codexSummary(_ uid: String) -> SessionSummary {
        // swiftlint:disable:next force_try
        try! JSONDecoder().decode(
            SessionSummary.self,
            from: Data(
                """
                {"session_uid":"\(uid)","session_id":"cc-1","tmux_session":"cc-1",
                 "cwd":"/work","project_label":"work","lifecycle":"live","link":"attached",
                 "last_seq":1,"created_at":"2026-10-28T10:00:00Z",
                 "updated_at":"2026-10-28T10:00:00Z","blocked_on":[],
                 "agent":"codex","codex_link":"subscribed"}
                """.utf8))
    }

    private func codexTurn(_ uid: String) -> Event {
        // swiftlint:disable:next force_try
        try! JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":1,"session_uid":"\(uid)","session_id":"cc-1",
                 "ts":"2026-10-28T10:00:00Z","kind":"tool_call","source":"codex",
                 "turn_id":"turn-9","payload":{}}
                """.utf8))
    }

    /// Removing a machine closes its link and leaves the other's as it was.
    func testRemovingAMachineClosesOnlyItsLink() {
        let machines = twoMachines()
        let (studioModel, laptopModel) = (machines.models[0], machines.models[1])
        let laptopID = laptopModel.machineID!

        machines.removeMachine(id: laptopID)

        XCTAssertEqual(machines.models.count, 1)
        XCTAssertTrue(machines.models[0] === studioModel)
        XCTAssertEqual(laptopModel.connection.phase, .idle, "its link is closed")
        XCTAssertTrue(studioModel.connection.phase.isConnected)
        XCTAssertEqual(machines.pairing.machines.map(\.endpoint), [studio])
    }

    /// Unpair removes the machine the screen asking for it shows, not the
    /// newest: a session's own Pair sheet unpairs that session's machine.
    func testUnpairRemovesTheMachineItIsAskedFrom() {
        let machines = twoMachines()
        let (studioModel, laptopModel) = (machines.models[0], machines.models[1])

        machines.unpair(studioModel)

        XCTAssertEqual(machines.pairing.machines.map(\.endpoint), [laptop])
        XCTAssertEqual(machines.models.count, 1)
        XCTAssertTrue(machines.models[0] === laptopModel)
        XCTAssertTrue(laptopModel.connection.phase.isConnected, "the other machine keeps its link")
    }

    func testRemovingTheLastMachineLeavesTheUnpairedApp() {
        let machines = twoMachines()
        for id in machines.models.compactMap(\.machineID) { machines.removeMachine(id: id) }

        XCTAssertEqual(machines.models.count, 1)
        XCTAssertNil(machines.models[0].machineID)
        XCTAssertFalse(machines.showsFleet)
    }

    /// A re-scan runs on that machine's own model, so it stays one link.
    func testARescanPairsOnThatMachinesOwnModel() {
        let machines = twoMachines()
        let studioModel = machines.models[0]

        machines.pair(withQR: PairingQRPayload(host: studio.host, port: studio.port, code: "ABCD2345"))
        defer {
            machines.cancelPairing()
            studioModel.connection.stop()
        }

        XCTAssertNil(machines.candidate)
        XCTAssertTrue(studioModel.isPairing)
        XCTAssertEqual(machines.models.count, 2)
    }

    /// A new address pairs on a link of its own: the machines already paired
    /// keep theirs, and a pairing given up leaves nothing behind.
    func testANewMachinePairsWithoutTouchingTheOthers() throws {
        let machines = twoMachines()
        machines.pair(
            withQR: PairingQRPayload(host: "desk.tail1234.ts.net", port: 8787, code: "ABCD2345"))

        let candidate = try XCTUnwrap(machines.candidate)
        XCTAssertTrue(machines.isPairing)
        XCTAssertFalse(machines.models.contains { $0 === candidate })
        XCTAssertTrue(machines.models.allSatisfy { $0.connection.phase.isConnected })

        machines.cancelPairing()
        XCTAssertNil(machines.candidate)
        XCTAssertEqual(candidate.connection.phase, .idle)
        XCTAssertEqual(machines.pairing.machines.count, 2)
    }

    // MARK: - One Mac under two addresses

    /// The studio again, by its address rather than its name: the same daemon.
    private let studioByIP = DaemonEndpoint(
        host: "100.64.0.7", port: 8787, credential: .token("studio-ip-token"), useTLS: false)
    private let studioDaemon = "5f1c0de5a17e4b8c9d2e6f7a8b9c0d1e"
    private let laptopDaemon = "0a1b2c3d4e5f60718293a4b5c6d7e8f9"

    /// A daemon's handshake, saying which daemon it is.
    private func ack(_ daemonID: String?, deviceToken: String? = nil) -> ServerMessage {
        .helloAck(
            HelloAck(
                protocolVersion: Wire.protocolVersion, protocolMinor: 26, serverTime: "",
                capabilities: Capabilities(), deviceToken: deviceToken, deviceID: nil,
                deviceName: nil, machineID: daemonID))
    }

    /// The studio paired by address, then the laptop, then the studio by name,
    /// as an issue-3 build could store them, over a cache in its own folder.
    private func studioTwice() -> (Machines, EventCache) {
        let pairing = PairingStore(ephemeral: true)
        pairing.save(studioByIP)
        pairing.save(laptop)
        pairing.save(studio)
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("machines-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let cache = EventCache(root: root)
        let machines = Machines(
            pairing: pairing, cache: cache,
            settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!))
        addTeardownBlock { for model in machines.models { model.connection.stop() } }
        return (machines, cache)
    }

    /// Both of the studio's records answer, in either order: one machine is left —
    /// the record paired first, with its cache, at the later pairing's address,
    /// credential and place — and the laptop is untouched.
    func testOneDaemonUnderTwoAddressesIsOneMachineWhicheverAnswersFirst() async throws {
        for nameFirst in [false, true] {
            let (machines, cache) = studioTwice()
            let (byIP, laptopModel, byName) = (
                machines.models[0], machines.models[1], machines.models[2]
            )
            let (ipID, nameID) = (try XCTUnwrap(byIP.machineID), try XCTUnwrap(byName.machineID))
            await cache.machine(ipID).saveEvents([], key: "01K8N0AAAAAAAAAAAAAAAAAAAA")
            await cache.machine(nameID).saveEvents([], key: "01K8N0BBBBBBBBBBBBBBBBBBBB")
            laptopModel.connection.injectForTesting(ack(laptopDaemon))
            let laptopBefore = try XCTUnwrap(machines.pairing.machine(id: laptopModel.machineID!))

            for model in nameFirst ? [byName, byIP] : [byIP, byName] {
                model.connection.injectForTesting(ack(studioDaemon))
            }

            let records = machines.pairing.machines
            XCTAssertEqual(records.map(\.id), [laptopBefore.id, ipID], "nameFirst \(nameFirst)")
            XCTAssertEqual(records[0], laptopBefore, "the other machine is untouched")
            XCTAssertEqual(records[1].endpoint, studio, "the later pairing's address and credential")
            XCTAssertEqual(records[1].daemonID, studioDaemon)
            XCTAssertTrue(machines.current === byIP, "Settings speaks for the machine just paired")
            XCTAssertEqual(machines.models.map(\.machineID), [ipID, laptopModel.machineID])
            XCTAssertEqual(byName.connection.phase, .idle, "the later record's link is closed")
            let kept = await cache.machine(ipID).loadEvents(key: "01K8N0AAAAAAAAAAAAAAAAAAAA")
            let gone = await cache.machine(nameID).loadEvents(key: "01K8N0BBBBBBBBBBBBBBBBBBBB")
            XCTAssertNotNil(kept, "the kept machine's cache is kept")
            XCTAssertNil(gone, "the later record's folder goes")

            // The next handshake, and the next launch, find nothing more to merge.
            byIP.connection.injectForTesting(ack(studioDaemon))
            let relaunched = Machines(
                pairing: machines.pairing, cache: cache,
                settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!))
            for model in relaunched.models {
                model.connection.injectForTesting(
                    ack(model.machineID == ipID ? studioDaemon : laptopDaemon))
            }
            XCTAssertEqual(machines.pairing.machines, records)
            XCTAssertEqual(relaunched.models.count, 2)
            relaunched.models.forEach { $0.connection.stop() }
        }
    }

    /// The same, from a real daemon: `minor-26-wire.json` holds the `hello_ack` a
    /// minor-26 `ccd` sent from a sandbox home, before and after a restart (its
    /// computer name replaced by a stand-in). The two say the same `machine_id`,
    /// so its two records become one.
    func testAMinor26DaemonIsOneMachineAcrossARestartAndTwoAddresses() throws {
        let url = try XCTUnwrap(
            Bundle(for: type(of: self)).url(forResource: "minor-26-wire", withExtension: "json"))
        let wire = try XCTUnwrap(
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: url)).objectValue)
        let (before, after) = (
            try XCTUnwrap(wire["ccd_hello_ack"]?.decoded(ServerMessage.self)),
            try XCTUnwrap(wire["ccd_hello_ack_after_restart"]?.decoded(ServerMessage.self))
        )
        let (machines, _) = studioTwice()
        let (byIP, byName) = (machines.models[0], machines.models[2])

        byIP.connection.injectForTesting(before)
        byName.connection.injectForTesting(after)

        XCTAssertEqual(machines.pairing.machines.count, 2)
        XCTAssertEqual(machines.pairing.current?.id, byIP.machineID)
        XCTAssertEqual(machines.pairing.current?.endpoint, studio)
        XCTAssertEqual(machines.pairing.current?.daemonID, "7c8b710f09e5def52f223969e788401e")
    }

    /// Two daemons are two machines, whatever they are called.
    func testTwoDaemonsAreNeverOneMachine() {
        let machines = twoMachines()
        let before = machines.pairing.machines
        machines.models[0].connection.injectForTesting(ack(studioDaemon))
        machines.models[1].connection.injectForTesting(ack(laptopDaemon))
        XCTAssertEqual(machines.pairing.machines.map(\.id), before.map(\.id))
        XCTAssertEqual(machines.models.count, 2)
    }

    /// A daemon that does not say which it is is told apart by address alone:
    /// the same Mac under two addresses stays two machines.
    func testADaemonThatDoesNotSayIsNeverMerged() {
        let (machines, _) = studioTwice()
        for model in machines.models { model.connection.injectForTesting(ack(nil)) }
        XCTAssertEqual(machines.pairing.machines.count, 3)
        XCTAssertEqual(machines.models.count, 3)
    }

    /// Scanning the studio's code at a new address, and typing its token at one:
    /// the new pairing turns out to be the studio, so the studio keeps its record
    /// and takes the new address and credential.
    func testANewPairingThatIsAMachineAlreadyPairedJoinsIt() throws {
        for typed in [false, true] {
            let pairing = PairingStore(ephemeral: true)
            pairing.save(studioByIP)
            pairing.save(laptop)
            let root = FileManager.default.temporaryDirectory
                .appendingPathComponent("machines-\(UUID().uuidString)")
            addTeardownBlock { try? FileManager.default.removeItem(at: root) }
            let machines = Machines(
                pairing: pairing, cache: EventCache(root: root),
                settings: AppSettings(defaults: UserDefaults(suiteName: UUID().uuidString)!))
            let studioModel = machines.models[0]
            studioModel.connection.injectForTesting(ack(studioDaemon))

            if typed {
                machines.pair(with: studio)
            } else {
                machines.pair(
                    withQR: PairingQRPayload(host: studio.host, port: studio.port, code: "ABCD2345"))
            }
            let candidate = try XCTUnwrap(machines.candidate)
            candidate.connection.injectForTesting(
                ack(studioDaemon, deviceToken: typed ? nil : "fresh-device-token"))

            XCTAssertNil(machines.candidate)
            let records = pairing.machines
            XCTAssertEqual(records.count, 2, "typed \(typed)")
            XCTAssertEqual(records.last?.id, studioModel.machineID)
            XCTAssertEqual(records.last?.endpoint.host, studio.host)
            XCTAssertEqual(
                records.last?.endpoint.token, typed ? "studio-token" : "fresh-device-token")
            XCTAssertFalse(machines.models.contains { $0 === candidate })
            XCTAssertEqual(candidate.connection.phase, .idle)
            for model in machines.models { model.connection.stop() }
        }
    }
}
