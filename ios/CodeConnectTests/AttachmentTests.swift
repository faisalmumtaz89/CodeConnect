import XCTest

@testable import CodeConnect
@testable import CodeConnectKit

/// **Files going with a message** (minor 50), counted at the socket: the chunks that leave and where each starts, the
/// tile's progress between them, the gate that keeps every frame home when the computer does not take files, the
/// limits that refuse a tile before anything is read, and the send that waits for every file and carries their ids.
@MainActor
final class AttachmentTests: XCTestCase {

    // MARK: Harness

    private final class Wire {
        var frames: [ClientMessage] = []
        /// Each `put_file` is answered with the bytes held after it, unless this refuses it.
        var refuse: String?
        /// The tile's state as each chunk left, for the progress it showed between chunks.
        var statesAtChunk: [Attachment.State] = []
        var chunks: [(offset: Int, bytes: Int)] {
            frames.compactMap {
                guard case .putFile(_, _, _, _, let offset, let data) = $0 else { return nil }
                return (offset, Data(base64Encoded: data)?.count ?? -1)
            }
        }
    }

    private func list(codex: Bool) throws -> ServerMessage {
        let agent = codex ? #""agent":"codex","codex_link":"subscribed""# : #""agent":"claude""#
        return .sessions([
            try JSONDecoder().decode(
                SessionSummary.self,
                from: Data(
                    """
                    {"session_uid":"u-1","session_id":"cc-1",
                     "cwd":"/work","project_label":"work","lifecycle":"live","link":"attached",
                     "last_seq":0,"created_at":"2026-08-18T09:00:00.000Z",
                     "updated_at":"2026-08-18T09:00:00.000Z","blocked_on":[],\(agent)}
                    """.utf8))
        ])
    }

    private func model(uploads: Bool = true, codex: Bool = false) async throws -> (AppModel, Wire) {
        let model = AppModel(cache: EventCache())
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(
                canApproveReliably: true, sendText: true,
                extra: ["uploads": .bool(uploads), "codex_compose": .bool(codex)]),
            minor: 50)
        let wire = Wire()
        model.connection.sendStub = { [weak model] message in
            wire.frames.append(message)
            if case .compose(let session, let requestID, _, _, _) = message {
                model?.connection.injectForTesting(
                    .composeResult(sessionID: session, requestID: requestID, result: .started(turnID: "t-1")))
            }
            guard case .putFile(_, let id, _, let size, let offset, let data) = message else { return }
            wire.statesAtChunk.append(model?.files(for: "u-1").first?.state ?? .arrived)
            let result: PutFileResult =
                wire.refuse.map { .refused($0) } ?? .received(min(size, offset + (Data(base64Encoded: data)?.count ?? 0)))
            model?.connection.injectForTesting(.putFileResult(uploadID: id, result: result))
        }
        model.connection.injectForTesting(try list(codex: codex))
        try await XCTUnwrap(model.statesForTesting["u-1"]).settleForTesting()
        XCTAssertNotNil(model.drafts(for: "u-1"))
        return (model, wire)
    }

    /// Waits until the run's files stop travelling.
    private func settled(_ model: AppModel) async {
        for _ in 0..<500 {
            if !model.files(for: "u-1").contains(where: { if case .uploading = $0.state { true } else { false } }) {
                return
            }
            try? await Task.sleep(for: .milliseconds(10))
        }
    }

    // MARK: Chunks and progress

    /// A file of two chunks and a bit leaves in three, each starting where the last answer said, none over 512 KiB, and
    /// the tile shows the bytes held between them before it reads arrived.
    func testAFileLeavesInChunksAtTheOffsetsTheComputerHolds() async throws {
        let (model, wire) = try await model()
        let size = 2 * Attachment.chunkBytes + 1_000
        model.attach(data: Data(repeating: 7, count: size), name: "trace.log", to: "u-1")
        await settled(model)

        // The daemon's bound, `ws.rs` MAX_UPLOAD_CHUNK_BYTES, written out: a chunk one byte over it is refused there.
        let bound = 512 * 1024
        XCTAssertEqual(wire.chunks.map(\.offset), [0, bound, 2 * bound])
        XCTAssertEqual(wire.chunks.map(\.bytes), [bound, bound, size - 2 * bound])
        XCTAssertEqual(
            wire.statesAtChunk,
            [.uploading(sent: 0), .uploading(sent: Attachment.chunkBytes), .uploading(sent: 2 * Attachment.chunkBytes)])
        XCTAssertEqual(model.files(for: "u-1").map(\.state), [.arrived])
    }

    /// A refused chunk fails the tile with the computer's words, and a retry is the same bytes under a new id from 0.
    func testARefusedFileFailsAndARetryGoesAgainUnderANewID() async throws {
        let (model, wire) = try await model()
        wire.refuse = "No room left."
        model.attach(data: Data(repeating: 1, count: 10), name: "a.txt", to: "u-1")
        await settled(model)
        let failed = try XCTUnwrap(model.files(for: "u-1").first)
        XCTAssertEqual(failed.state, .failed("No room left."))

        wire.refuse = nil
        model.retryFile(failed.id, in: "u-1")
        await settled(model)
        let again = try XCTUnwrap(model.files(for: "u-1").first)
        XCTAssertNotEqual(again.id, failed.id)
        XCTAssertEqual(again.state, .arrived)
        XCTAssertEqual(wire.chunks.last?.offset, 0)
    }

    // MARK: The gate

    /// A computer that does not advertise `uploads` gets no file control and no frame: an older one answers `put_file`
    /// with `bad_request`.
    func testNoFrameLeavesForAComputerThatTakesNoFiles() async throws {
        let (model, wire) = try await model(uploads: false)
        XCTAssertFalse(model.canUpload("u-1"))
        model.attach(data: Data(repeating: 1, count: 10), name: "a.txt", to: "u-1")
        await settled(model)
        XCTAssertTrue(wire.chunks.isEmpty, "no put_file may leave: \(wire.frames)")
        guard case .failed = model.files(for: "u-1").first?.state else {
            return XCTFail("the tile says it did not go: \(model.files(for: "u-1"))")
        }
    }

    /// A run that has ended takes no reply, so it takes no file either: no paperclip, no drop.
    func testAnEndedRunTakesNoFiles() async throws {
        let (model, _) = try await model()
        XCTAssertTrue(model.canUpload("u-1"))
        model.connection.injectForTesting(
            .sessions([
                try JSONDecoder().decode(
                    SessionSummary.self,
                    from: Data(
                        """
                        {"session_uid":"u-1","session_id":"cc-1",
                         "cwd":"/work","project_label":"work","lifecycle":"exited","link":"attached",
                         "last_seq":0,"created_at":"2026-08-18T09:00:00.000Z",
                         "updated_at":"2026-08-18T09:00:00.000Z","blocked_on":[],"agent":"claude"}
                        """.utf8))
            ]))
        try await XCTUnwrap(model.statesForTesting["u-1"]).settleForTesting()
        XCTAssertEqual(model.shownSummary(for: "u-1")?.takesNoReply, true)
        XCTAssertFalse(model.canUpload("u-1"))
    }

    // MARK: Limits

    /// Over 10 MiB, the tile is refused on its size alone: its bytes are never read, and nothing leaves. An eleventh
    /// file is refused the same way.
    func testTheLimitsRefuseBeforeAnythingIsRead() async throws {
        let (model, wire) = try await model()
        var read = false
        model.attach(name: "big.mov", size: 2 * Attachment.maxBytes, to: "u-1") {
            read = true
            return Data()
        }
        XCTAssertFalse(read)
        guard case .refused(let reason) = model.files(for: "u-1").first?.state else {
            return XCTFail("refused: \(model.files(for: "u-1"))")
        }
        XCTAssertTrue(reason.contains("10 MB"), reason)

        for n in 1..<Attachment.maxCount { model.attach(data: Data([UInt8(n)]), name: "f\(n)", to: "u-1") }
        model.attach(data: Data([1]), name: "eleventh", to: "u-1")
        await settled(model)
        XCTAssertEqual(model.files(for: "u-1").count, Attachment.maxCount + 1)
        XCTAssertEqual(
            model.files(for: "u-1").last?.state, .refused("eleventh can’t be sent: a message carries up to 10 files."))
        XCTAssertEqual(
            model.filesBlockedReason(for: "u-1"), "big.mov is 20 MB; files over 10 MB can’t be sent.",
            "a file that can never go is named first, in its own words")
        XCTAssertEqual(wire.chunks.count, Attachment.maxCount - 1, "only the files inside the limits leave")
    }

    /// What holds the message back is counted in words, never "file(s)": one file or the 2 files that didn't arrive,
    /// 1 file or 2 files on their way.
    func testTheHeldBackLineCountsTheFiles() async throws {
        let (model, wire) = try await model()
        wire.refuse = "no room"
        model.attach(data: Data([1]), name: "a.txt", to: "u-1")
        await settled(model)
        XCTAssertEqual(model.filesBlockedReason(for: "u-1"), "Retry or remove the file that didn’t arrive.")
        model.attach(data: Data([2]), name: "b.txt", to: "u-1")
        await settled(model)
        XCTAssertEqual(model.filesBlockedReason(for: "u-1"), "Retry or remove the 2 files that didn’t arrive.")
        model.files(for: "u-1").forEach { model.removeFile($0.id, from: "u-1") }
        wire.refuse = nil
        model.connection.sendStub = { _ in }
        model.attach(data: Data([3]), name: "c.txt", to: "u-1")
        XCTAssertEqual(model.filesBlockedReason(for: "u-1"), "Waiting for 1 file to arrive.")
        model.attach(data: Data([4]), name: "d.txt", to: "u-1")
        XCTAssertEqual(model.filesBlockedReason(for: "u-1"), "Waiting for 2 files to arrive.")
    }

    /// A folder is refused on its own words before anything is read, and nothing of it leaves.
    func testAFolderIsRefusedBeforeItIsRead() async throws {
        let (model, wire) = try await model()
        model.attach(name: "src", size: 0, to: "u-1") { throw Attachment.Folder() }
        XCTAssertEqual(model.files(for: "u-1").first?.state, .refused("src is a folder; only files can be sent."))
        XCTAssertTrue(wire.chunks.isEmpty)
    }

    /// A file's type as its mark's plate writes it: a known extension in capitals, whatever its case; none for a name
    /// with no extension or one the set does not know.
    func testATypeMarkNamesOnlyKnownTypes() {
        XCTAssertEqual(Attachment.typeLabel("install.log"), "LOG")
        XCTAssertEqual(Attachment.typeLabel("TRACE.LOG"), "LOG")
        XCTAssertEqual(Attachment.typeLabel("audit.csv"), "CSV")
        XCTAssertEqual(Attachment.typeLabel("notes.md"), "MD")
        XCTAssertEqual(Attachment.typeLabel("clip.mov"), "MOV")
        XCTAssertEqual(Attachment.typeLabel("archive.tar.gz"), "GZ")
        XCTAssertNil(Attachment.typeLabel("Brewfile"))
        XCTAssertNil(Attachment.typeLabel("dump.xyz"))
        XCTAssertNil(Attachment.typeLabel("trailing."))
    }

    /// The drop plate's line, counted from the drag against what the tray already holds.
    func testTheDropLineCountsWhatFits() {
        XCTAssertEqual(Attachment.dropLine(dragged: 3, held: 0), "3 files go with your next message · 7 more fit")
        XCTAssertEqual(Attachment.dropLine(dragged: 1, held: 0), "1 file goes with your next message · 9 more fit")
        XCTAssertEqual(Attachment.dropLine(dragged: 1, held: 8), "1 file goes with your next message · 1 more fits")
        XCTAssertEqual(Attachment.dropLine(dragged: 2, held: 8), "2 files go with your next message")
        XCTAssertEqual(Attachment.dropLine(dragged: 12, held: 0), "12 files · 10 fit; 2 won’t be sent")
        XCTAssertEqual(Attachment.dropLine(dragged: 3, held: 9), "3 files · 1 fits; 2 won’t be sent")
        XCTAssertEqual(Attachment.dropLine(dragged: 2, held: 10), "2 files · none fit; 2 won’t be sent")
        XCTAssertEqual(Attachment.dropLimits, "Up to 10 files, 10 MB each")
    }

    /// A name becomes one path component, as the daemon takes it.
    func testAFileNameIsOnePathComponent() {
        XCTAssertEqual(Attachment.fileName("../etc/passwd"), "_etc_passwd")
        XCTAssertEqual(Attachment.fileName(".env"), "env")
        XCTAssertEqual(Attachment.fileName("a\nb.txt"), "a_b.txt")
        XCTAssertEqual(Attachment.fileName("..."), "file")
        XCTAssertEqual(Attachment.fileName(String(repeating: "é", count: 200)).utf8.count, 254)
    }

    // MARK: The send

    /// The send waits while a file travels, then carries every id in order, hashed after the words; the tray empties
    /// once it is sent.
    func testTheSendWaitsForEveryFileThenCarriesTheirIDs() async throws {
        let (model, wire) = try await model()
        model.attach(data: Data(repeating: 2, count: 3), name: "a.png", to: "u-1")
        XCTAssertEqual(model.sendBlockedReason(for: "u-1", draft: "look"), "Waiting for 1 file to arrive.")
        model.attach(data: Data(repeating: 3, count: 4), name: "b.txt", to: "u-1")
        await settled(model)
        XCTAssertNil(model.sendBlockedReason(for: "u-1", draft: "look"))
        let ids = model.files(for: "u-1").map(\.id)

        model.connection.sendTextStub = { _, _ in .sent(matched: "work") }
        let attempt = await model.send(text: "look", to: "u-1", files: true)
        XCTAssertTrue(attempt.landed, "\(attempt)")
        XCTAssertEqual(model.connection.sentAttachments.last, ids)
        XCTAssertEqual(
            model.connection.sendTextIdentities.last?.payloadHash,
            SendTextIdentity.payloadHash(session: "u-1", text: "look", submit: true, attachments: ids))
        XCTAssertTrue(model.files(for: "u-1").isEmpty, "the tray empties once the message carried it")
        XCTAssertFalse(wire.chunks.isEmpty)
    }

    /// A diff comment or any other caller never carries the tray: only the reply box's send does.
    func testOnlyTheReplyBoxSendCarriesFiles() async throws {
        let (model, _) = try await model()
        model.attach(data: Data(repeating: 2, count: 3), name: "a.png", to: "u-1")
        await settled(model)
        model.connection.sendTextStub = { _, _ in .sent(matched: "work") }
        _ = await model.send(text: "a comment", to: "u-1")
        XCTAssertEqual(model.connection.sentAttachments.last, [])
        XCTAssertEqual(model.files(for: "u-1").count, 1)
    }

    /// Files alone are a message.
    func testFilesAloneAreAMessage() async throws {
        let (model, _) = try await model()
        model.attach(data: Data(repeating: 2, count: 3), name: "a.png", to: "u-1")
        await settled(model)
        model.connection.sendTextStub = { _, _ in .sent(matched: "work") }
        let attempt = await model.send(text: "", to: "u-1", files: true)
        XCTAssertTrue(attempt.landed, "\(attempt)")
        XCTAssertEqual(model.connection.sentAttachments.last?.count, 1)
    }

    /// The link dropping loses every upload no message carried: each tile fails, for a retry.
    func testADroppedLinkFailsEveryFileNotYetSent() async throws {
        let (model, _) = try await model()
        model.attach(data: Data(repeating: 2, count: 3), name: "a.png", to: "u-1")
        await settled(model)
        model.connection.simulateDisconnectForTesting()
        XCTAssertEqual(model.files(for: "u-1").first?.state, .failed("The link dropped before it was sent."))
    }

    // MARK: Only the reply box's send is the tray's

    /// A diff comment to a Codex run carries none of the reply box's files and leaves its tray as it was; and the
    /// comment's own box is not held back by those files.
    func testACodexCommentCarriesNoFilesAndLeavesTheTray() async throws {
        let (model, _) = try await model(codex: true)
        model.attach(data: Data(repeating: 2, count: 3), name: "a.png", to: "u-1")
        await settled(model)
        model.attach(name: "big.mov", size: 2 * Attachment.maxBytes, to: "u-1") { Data() }
        XCTAssertNotNil(model.sendBlockedReason(for: "u-1", draft: "look"), "the reply box waits on its files")
        XCTAssertNil(
            model.sendBlockedReason(for: "u-1", draft: "look", files: false), "a comment's box is not held by them")

        _ = await model.composeToCodex(sessionKey: "u-1", text: "a comment")
        XCTAssertEqual(model.connection.sentAttachments.last, [], "a comment carries no files")
        XCTAssertEqual(model.files(for: "u-1").count, 2, "and leaves the tray as it was")
    }

    /// A file added while a send waits for its answer is the next message's: the send takes out only what it carried.
    func testAFileAddedWhileTheSendWaitsStaysForTheNextMessage() async throws {
        let (model, _) = try await model()
        model.attach(data: Data(repeating: 2, count: 3), name: "a.png", to: "u-1")
        await settled(model)
        let sent = model.files(for: "u-1").map(\.id)
        model.connection.sendTextStub = { [weak model] _, _ in
            model?.attach(data: Data(repeating: 3, count: 4), name: "later.txt", to: "u-1")
            return .sent(matched: "work")
        }
        _ = await model.send(text: "look", to: "u-1", files: true)
        await settled(model)
        XCTAssertEqual(model.files(for: "u-1").map(\.name), ["later.txt"])
        XCTAssertFalse(model.files(for: "u-1").contains { sent.contains($0.id) })
    }

    // MARK: Wire

    /// Frames as `ws.rs` reads them: attachments only when there are any, and a chunk's fields.
    func testTheFramesCarryAttachmentsOnlyWhenThereAreAny() throws {
        func json(_ message: ClientMessage) throws -> String {
            String(decoding: try JSONEncoder().encode(message), as: UTF8.self)
        }
        let plain = try json(.sendText(session: "u-1", text: "hi", submit: true, requestID: nil, payloadHash: nil))
        XCTAssertFalse(plain.contains("attachments"), plain)
        let with = try json(
            .compose(session: "u-1", requestID: "c-1", text: "", payloadHash: "h", attachments: ["a", "b"]))
        XCTAssertTrue(with.contains(#""attachments":["a","b"]"#), with)
        let chunk = try json(.putFile(session: nil, uploadID: "up", name: "a.png", size: 5, offset: 0, data: "YWJj"))
        XCTAssertFalse(chunk.contains("session_id"), "a pre-start chunk names no session: \(chunk)")
        XCTAssertTrue(chunk.contains(#""type":"put_file""#) && chunk.contains(#""upload_id":"up""#), chunk)
    }

    func testAPutFileResultReadsAsSent() throws {
        func read(_ text: String) throws -> ServerMessage { try JSONDecoder().decode(ServerMessage.self, from: Data(text.utf8)) }
        guard case .putFileResult("up", .received(3)) = try read(
            #"{"type":"put_file_result","upload_id":"up","result":{"status":"received","received":3}}"#)
        else { return XCTFail() }
        guard case .putFileResult("up", .refused("too big")) = try read(
            #"{"type":"put_file_result","upload_id":"up","result":{"status":"refused","reason":"too big"}}"#)
        else { return XCTFail() }
    }

    /// The hashes match `hash.rs`: none attached is exactly the minor-49 hash (its pinned values), and the ids bind in
    /// order.
    func testTheHashesMatchTheDaemons() {
        XCTAssertEqual(
            SendTextIdentity.payloadHash(session: "cc-1", text: "look", submit: true),
            "164da7901a086ecdb87c002eae6b63408738d330b84ccba7f345cafd0b386c57")
        XCTAssertEqual(
            SendTextIdentity.payloadHash(session: "cc-1", text: "look", submit: true, attachments: ["u1", "u2"]),
            "a24dee8644962428f985f77a3597ccf61a2f78da893248385f9f49f068812399")
        XCTAssertEqual(
            CodexHash.compose(sessionRef: "cc-1", text: "look"),
            "688d7743d5b103572930859255089830f3ac02e1a340987c4e8d8cb4b7592996")
        XCTAssertEqual(
            CodexHash.compose(sessionRef: "cc-1", text: "look", attachments: ["u1"]),
            "b8d12f3b8e9ba8b7e74aaa1e659a5d3f22932372df995d66b31e8ece194457f0")
    }

    // MARK: The start box

    private func startModel() -> (AppModel, Wire, StartProject) {
        let model = AppModel(cache: EventCache())
        model.connection.simulateConnectedForTesting()
        model.connection.simulateCapabilitiesForTesting(
            Capabilities(
                canApproveReliably: true, sendText: true,
                extra: ["uploads": .bool(true), "start_session": .bool(true)]),
            minor: 50)
        let wire = Wire()
        model.connection.sendStub = { [weak model] message in
            wire.frames.append(message)
            switch message {
            case .putFile(_, let id, _, let size, let offset, let data):
                let result: PutFileResult =
                    wire.refuse.map { .refused($0) } ?? .received(min(size, offset + (Data(base64Encoded: data)?.count ?? 0)))
                model?.connection.injectForTesting(.putFileResult(uploadID: id, result: result))
            case .startSession(let requestID, _, _, _, _, _, _, let ids):
                // The first start is refused, as ccd refuses one naming uploads it cannot take; the next starts.
                let first = wire.frames.filter { if case .startSession = $0 { true } else { false } }.count == 1
                model?.connection.injectForTesting(
                    .startSessionResult(
                        requestID: requestID,
                        outcome: first
                            ? .failed(.launchFailed, detail: "\(ids.first ?? "") is not a finished upload this start may attach")
                            : .started(sessionUID: "u-new")))
            default:
                break
            }
        }
        let project = StartProject(
            path: "/work", name: "work", lastUsed: Date(), lastAgent: .claude, gitBranch: "main", exists: true)
        return (model, wire, project)
    }

    private func outcome(_ model: AppModel, _ id: String) async -> StartOutcome? {
        for _ in 0..<500 {
            if let outcome = model.starter.starts[id]?.outcome { return outcome }
            try? await Task.sleep(for: .milliseconds(10))
        }
        return nil
    }

    private func startedIDs(_ wire: Wire) -> [[String]] {
        wire.frames.compactMap { if case .startSession(_, _, _, _, _, _, _, let ids) = $0 { ids } else { nil } }
    }

    /// The start box's files go with the start, for no session; a start the computer refused has spent them, so Start
    /// again sends the same bytes under new ids, and that start goes.
    func testARefusedStartSendsItsFilesAgainUnderNewIDs() async throws {
        let (model, wire, project) = startModel()
        let draft = StartDraft()
        draft.attach(name: "notes.md", size: 5) { Data("hello".utf8) }

        let first = model.startSession(project, agent: .claude, prompt: "read this", files: draft.files)
        guard case .failed = await outcome(model, first) else { return XCTFail("the first start is refused") }
        let second = model.startSession(project, agent: .claude, prompt: "read this", files: draft.files)
        guard case .started = await outcome(model, second) else { return XCTFail("Start again starts") }

        let ids = startedIDs(wire)
        XCTAssertEqual(ids.count, 2)
        XCTAssertEqual(ids.map(\.count), [1, 1])
        XCTAssertTrue(Set(ids[0]).isDisjoint(with: ids[1]), "the second start's files are new uploads: \(ids)")
        XCTAssertFalse(ids.flatMap { $0 }.contains(draft.files[0].id), "a draft's tile id is never an upload's")
        for frame in wire.frames {
            if case .putFile(let session, _, _, _, _, _) = frame { XCTAssertNil(session, "a start's file names no run") }
        }
        XCTAssertEqual(draft.files.count, 1, "the start box keeps its tiles")
    }

    /// A start sent again while its first send is still sending its files (a handshake resends every unanswered start)
    /// leaves it to that send: its files go once, and one start leaves.
    func testAStartSentAgainWhileSendingSendsItsFilesOnce() async throws {
        let (model, wire, project) = startModel()
        let draft = StartDraft()
        draft.attach(name: "notes.md", size: 5) { Data("hello".utf8) }
        let id = model.startSession(project, agent: .claude, prompt: "read this", files: draft.files)
        model.resumeStart(id)
        _ = await outcome(model, id)
        let chunks = wire.frames.filter { if case .putFile = $0 { true } else { false } }
        XCTAssertEqual(chunks.count, 1, "one upload of the one file: \(wire.frames)")
        XCTAssertEqual(startedIDs(wire).count, 1)
    }

    /// Opening the started run spends the start box's files with its message: the next New Agent box is empty.
    func testOpeningTheStartedRunEmptiesTheStartBox() {
        let draft = StartDraft()
        draft.message = "read this"
        draft.attach(name: "notes.md", size: 5) { Data("hello".utf8) }
        draft.didOpen()
        XCTAssertEqual(draft.message, "")
        XCTAssertTrue(draft.files.isEmpty)
    }

    /// An answered start lets its files' bytes go: starts are kept for the app's life.
    func testAnAnsweredStartLetsItsFilesBytesGo() async throws {
        let (model, _, project) = startModel()
        let draft = StartDraft()
        draft.attach(name: "notes.md", size: 5) { Data("hello".utf8) }
        let id = model.startSession(project, agent: .claude, prompt: "", files: draft.files)
        _ = await outcome(model, id)
        let kept = try XCTUnwrap(model.starter.starts[id]?.files)
        XCTAssertEqual(kept.count, 1)
        XCTAssertTrue(kept.allSatisfy { $0.data == nil }, "no start keeps its files' bytes once answered")
    }

    /// A file that does not arrive fails the start with why, and no start leaves.
    func testAStartWhoseFileDidNotArriveIsNotSent() async throws {
        let (model, wire, project) = startModel()
        wire.refuse = "No room left."
        let draft = StartDraft()
        draft.attach(name: "notes.md", size: 5) { Data("hello".utf8) }
        let id = model.startSession(project, agent: .claude, prompt: "", files: draft.files)
        guard case .failed(.launchFailed, let detail) = await outcome(model, id) else {
            return XCTFail("\(String(describing: model.starter.starts[id]?.outcome))")
        }
        XCTAssertEqual(detail, "notes.md didn’t reach the computer: No room left.")
        XCTAssertTrue(startedIDs(wire).isEmpty, "no start_session may leave")
    }

    // MARK: A sent message's files

    func testASentMessagesFilesAreReadBackFromItsText() {
        let path = "/Users/person/.codeconnect/uploads/r/u1/a.png"
        XCTAssertEqual(AttachedFiles.split("hi\n\nAttached:\n\(path)").words, "hi")
        XCTAssertEqual(AttachedFiles.split("hi\n\nAttached:\n\(path)").paths, [path])
        XCTAssertEqual(AttachedFiles.split("Attached:\n\(path)\n\(path)").paths, [path, path])
        XCTAssertEqual(AttachedFiles.split("Attached:\n\(path)").words, "")
        // Wherever the computer keeps its CodeConnect folder (CODECONNECT_HOME).
        let elsewhere = "/private/tmp/rig/cc/uploads/r/u1/b.bin"
        XCTAssertEqual(AttachedFiles.split("x\n\nAttached:\n\(elsewhere)").paths, [elsewhere])
        // Only the trailing block the daemon appends: a path the person wrote stays words, and so does a block with
        // anything after it that is not such a path.
        let mention = "see \(path) and fix it"
        XCTAssertEqual(AttachedFiles.split(mention).words, mention)
        XCTAssertEqual(AttachedFiles.split(mention).paths, [])
        let block = "Attached:\n\(path)\nthen this"
        XCTAssertEqual(AttachedFiles.split(block).paths, [])
        XCTAssertEqual(AttachedFiles.split("a\n\nAttached:\n\(path)\n\nmore words").paths, [])
        XCTAssertEqual(AttachedFiles.split("Attached:\n/a/uploads/r/u").paths, [], "one component short")
        // Words that merely mention it, or paths elsewhere, are words.
        XCTAssertEqual(AttachedFiles.split("see\n\nAttached:\n/tmp/x").paths, [])
        XCTAssertEqual(AttachedFiles.split("Attached: nothing").words, "Attached: nothing")
    }
}
