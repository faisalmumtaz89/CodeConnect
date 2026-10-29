import XCTest

@testable import CodeConnect

/// One phone, several machines: what the Keychain and the cache hold for each.
///
/// Every store here runs against the real Keychain under accounts of its own, so
/// loading and migration are the production code paths and the pairing the
/// simulator holds is never touched. Skipped rather than failed only where the
/// build has no Keychain entitlement (unsigned, `-34018`): each assertion needs an
/// item to have really been stored. Any other Keychain error fails.
@MainActor
final class PairedMachinesTests: XCTestCase {

    private func prefix() throws -> String {
        let prefix = "test-\(UUID().uuidString)-"
        addTeardownBlock {
            Keychain.delete(account: prefix + "daemon")
            Keychain.delete(account: prefix + "machines")
        }
        do {
            try Keychain.save(Data(), account: prefix + "machines")
        } catch Keychain.Failure.status(errSecMissingEntitlement) {
            throw XCTSkip("this build has no Keychain entitlement (unsigned)")
        }
        Keychain.delete(account: prefix + "machines")
        return prefix
    }

    private func endpoint(_ host: String, token: String, port: Int = 8787) -> DaemonEndpoint {
        DaemonEndpoint(host: host, port: port, credential: .token(token), useTLS: true)
    }

    private let studio = "studio.tail1234.ts.net"
    private let laptop = "laptop.tail1234.ts.net"

    // MARK: Migration

    func testTheSinglePairingAnEarlierBuildHeldBecomesTheFirstMachine() throws {
        let prefix = try prefix()
        let held = endpoint(studio, token: "aa".padding(toLength: 64, withPad: "b", startingAt: 0))
        // What an earlier build wrote: one endpoint, encoded the way it encoded it.
        try Keychain.save(JSONEncoder().encode(held), account: prefix + "daemon")

        let store = PairingStore(keychainPrefix: prefix)

        XCTAssertEqual(store.machines.map(\.endpoint), [held])
        XCTAssertNil(Keychain.load(account: prefix + "daemon"), "the old item is gone once migrated")
        let reloaded = PairingStore(keychainPrefix: prefix)
        XCTAssertEqual(reloaded.machines, store.machines, "the next launch reads the same machine")
    }

    func testAMigrationStoppedBeforeItsCleanupIsFinishedOnTheNextLaunch() throws {
        let prefix = try prefix()
        let migrated = PairedMachine(id: "m1", endpoint: endpoint(studio, token: "migrated-token-0001"))
        try Keychain.save(JSONEncoder().encode([migrated]), account: prefix + "machines")
        try Keychain.save(
            JSONEncoder().encode(endpoint(studio, token: "migrated-token-0001")),
            account: prefix + "daemon")

        let store = PairingStore(keychainPrefix: prefix)

        XCTAssertEqual(store.machines, [migrated])
        XCTAssertNil(Keychain.load(account: prefix + "daemon"))
    }

    // MARK: Add, re-scan, remove

    func testAddingAMachineNeverReplacesOne() throws {
        let prefix = try prefix()
        let store = PairingStore(keychainPrefix: prefix)
        store.save(endpoint(studio, token: "studio-token-00001"))
        store.save(endpoint(laptop, token: "laptop-token-00001"))

        let reloaded = PairingStore(keychainPrefix: prefix)
        XCTAssertEqual(
            reloaded.machines.map(\.endpoint),
            [endpoint(studio, token: "studio-token-00001"), endpoint(laptop, token: "laptop-token-00001")])
        XCTAssertEqual(reloaded.current?.endpoint.host, laptop, "the newest is the last")
    }

    func testRescanningAPairedMachineReplacesOnlyItsCredential() throws {
        let prefix = try prefix()
        let store = PairingStore(keychainPrefix: prefix)
        store.save(endpoint(studio, token: "studio-token-00001"))
        let studioID = try XCTUnwrap(store.current?.id)
        store.noteMachine(name: "Jordan’s Mac Studio", os: "macos", daemonID: nil, id: studioID)
        store.save(endpoint(laptop, token: "laptop-token-00001"))
        store.noteMachine(
            name: "Jordan’s MacBook Air", os: "macos", daemonID: nil,
            id: try XCTUnwrap(store.current?.id))
        let laptopBefore = try XCTUnwrap(store.current)
        XCTAssertEqual(store.machines.first?.name, "Jordan’s Mac Studio", "a name goes to its own machine")

        store.save(endpoint(studio.uppercased(), token: "studio-token-00002"))

        let reloaded = PairingStore(keychainPrefix: prefix)
        XCTAssertEqual(reloaded.machines.count, 2)
        XCTAssertEqual(reloaded.machines.first, laptopBefore, "the other machine is untouched")
        let studioAfter = try XCTUnwrap(reloaded.current)
        XCTAssertEqual(studioAfter.id, studioID, "the same machine, so the same cache")
        XCTAssertEqual(studioAfter.endpoint.token, "studio-token-00002")
        XCTAssertEqual(studioAfter.name, "Jordan’s Mac Studio")
    }

    /// The same Mac paired by address and then by name, merged: the next launch
    /// reads one machine — the first record's id at the later address, in the
    /// later place — and the other machine as it was.
    func testOneMacMergedFromTwoAddressesIsOneMachineAfterReload() throws {
        let prefix = try prefix()
        let store = PairingStore(keychainPrefix: prefix)
        store.save(endpoint("100.64.0.7", token: "studio-ip-token-001"))
        let byAddress = try XCTUnwrap(store.current)
        store.save(endpoint(laptop, token: "laptop-token-00001"))
        let laptopMachine = try XCTUnwrap(store.current)
        store.save(endpoint(studio, token: "studio-dns-token-01"))
        let byName = try XCTUnwrap(store.current)
        for id in [byAddress.id, byName.id] {
            store.noteMachine(name: "Jordan’s Mac Studio", os: "macos", daemonID: "studio-daemon", id: id)
        }
        XCTAssertEqual(store.sameDaemon(as: byName.id)?.id, byAddress.id)

        store.merge(byName.id, into: byAddress.id)

        let reloaded = PairingStore(keychainPrefix: prefix)
        XCTAssertEqual(reloaded.machines.map(\.id), [laptopMachine.id, byAddress.id])
        XCTAssertEqual(reloaded.machines.first, laptopMachine, "the other machine is untouched")
        let merged = try XCTUnwrap(reloaded.current)
        XCTAssertEqual(merged.endpoint, endpoint(studio, token: "studio-dns-token-01"))
        XCTAssertEqual(merged.daemonID, "studio-daemon")
        XCTAssertEqual(merged.name, "Jordan’s Mac Studio")
        XCTAssertNil(reloaded.sameDaemon(as: byAddress.id), "nothing left to merge")
    }

    func testRemovingAMachineLeavesEveryOtherExactlyAsItWas() throws {
        let prefix = try prefix()
        let store = PairingStore(keychainPrefix: prefix)
        store.save(endpoint(studio, token: "studio-token-00001"))
        let studioMachine = try XCTUnwrap(store.current)
        store.save(endpoint(laptop, token: "laptop-token-00001"))
        let laptopMachine = try XCTUnwrap(store.current)

        store.remove(id: studioMachine.id)

        XCTAssertEqual(PairingStore(keychainPrefix: prefix).machines, [laptopMachine])
    }

    // MARK: The cache

    private func cacheRoot() -> URL {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("machines-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        return root
    }

    private func event(_ uid: String, _ seq: UInt64) throws -> Event {
        try JSONDecoder().decode(
            Event.self,
            from: Data(
                """
                {"seq":\(seq),"session_id":"cc-1","session_uid":"\(uid)",\
                "ts":"2026-10-27T10:00:00Z","kind":"tool_call","source":"hook","payload":{}}
                """.utf8))
    }

    func testTheCacheAnEarlierBuildFiledMovesUnderTheFirstMachine() async throws {
        let base = EventCache(root: cacheRoot())
        // An earlier build filed one machine's cache at the top of the folder.
        await base.adopt(keying: .uid)
        await base.saveEvents([try event("01RUN", 1), try event("01RUN", 2)], key: "01RUN")

        base.moveUnfiledFiles(toMachine: "m1")

        let first = base.machine("m1")
        let moved = await first.loadEvents(key: "01RUN")
        let dropped = await first.adopt(keying: .uid)
        let loose = await base.loadEvents(key: "01RUN")
        XCTAssertEqual(moved?.events.map(\.seq), [1, 2], "the history is the first machine's")
        XCTAssertTrue(dropped.isEmpty, "and still filed under uids")
        XCTAssertNil(loose, "nothing is left loose")
    }

    func testRemovingAMachineDeletesOnlyItsOwnCache() async throws {
        let base = EventCache(root: cacheRoot())
        let studioCache = base.machine("studio")
        let laptopCache = base.machine("laptop")
        await studioCache.saveEvents([try event("01RUN", 1)], key: "01RUN")
        await laptopCache.saveEvents([try event("01RUN", 7)], key: "01RUN")

        base.removeMachine("studio")

        let studioAfter = await base.machine("studio").loadEvents(key: "01RUN")
        let laptopAfter = await laptopCache.loadEvents(key: "01RUN")
        XCTAssertNil(studioAfter)
        XCTAssertEqual(laptopAfter?.events.map(\.seq), [7])
    }
}
