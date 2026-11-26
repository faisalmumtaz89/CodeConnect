import Foundation

/// **One file going with a message** (minor 50): sent to the computer as soon as it is added, in chunks, so the
/// message waits only for what has not arrived yet. Its id is the upload's: a retry is another upload, under a new id.
public struct Attachment: Identifiable, Hashable, Sendable {
    /// The daemon's bounds (`ws.rs`): a file, the files one message carries, and one chunk, decoded.
    public static let maxBytes = 10 * 1024 * 1024
    public static let maxCount = 10
    public static let chunkBytes = 512 * 1024

    public enum State: Hashable, Sendable {
        /// On its way: `sent` of `size` bytes are on the computer.
        case uploading(sent: Int)
        /// All of it is on the computer, ready to go with the message.
        case arrived
        /// Over a limit, so never sent: it can only be removed.
        case refused(String)
        /// The computer refused it, or the link dropped: it can be sent again.
        case failed(String)
    }

    public let id: String
    /// As the agent will see it: one path component.
    public let name: String
    public let size: Int
    public let isImage: Bool
    /// The bytes, kept for a retry; none for a file refused before it was read.
    public let data: Data?
    public internal(set) var state: State

    public init(id: String, name: String, size: Int, isImage: Bool, data: Data?, state: State) {
        self.id = id
        self.name = name
        self.size = size
        self.isImage = isImage
        self.data = data
        self.state = state
    }

    /// The file's name as one path component, as the daemon takes it: `/` and control characters become `_`, leading
    /// dots go, at most 255 bytes, and "file" when nothing is left.
    public static func fileName(_ raw: String) -> String {
        var name = String(raw.unicodeScalars.map { $0 == "/" || $0.properties.generalCategory == .control ? "_" : Character($0) })
        while name.hasPrefix(".") { name.removeFirst() }
        while name.utf8.count > 255 { name.removeFirst() }
        return name.isEmpty ? "file" : name
    }

    /// PNG, JPEG, GIF or WebP, by their first bytes: the extension each is written with ("png", "jpg", "gif", "webp"), or
    /// nil for anything else.
    public static func imageExtension(_ data: Data) -> String? {
        let head = [UInt8](data.prefix(12))
        func starts(_ bytes: [UInt8], at offset: Int = 0) -> Bool {
            head.count >= offset + bytes.count && Array(head[offset..<offset + bytes.count]) == bytes
        }
        if starts([0x89, 0x50, 0x4E, 0x47]) { return "png" }
        if starts([0xFF, 0xD8, 0xFF]) { return "jpg" }
        if starts(Array("GIF8".utf8)) { return "gif" }
        if starts(Array("RIFF".utf8)) && starts(Array("WEBP".utf8), at: 8) { return "webp" }
        return nil
    }

    static func isImage(_ data: Data) -> Bool { imageExtension(data) != nil }

    /// What a `read` throws for a folder: it is refused in its own words before anything is read.
    public struct Folder: Error {
        public init() {}
    }

    /// The extensions a type mark names on its plate. Any other file, and one with no extension, has the plain page.
    static let knownTypes: Set<String> = [
        "swift", "rs", "py", "js", "ts", "tsx", "go", "c", "h", "cpp", "java", "kt", "rb", "sh", "html", "css", "diff",
        "patch", "txt", "md", "rtf", "pdf", "doc", "docx", "xls", "xlsx", "log", "json", "jsonl", "yaml", "yml", "toml",
        "xml", "csv", "tsv", "sql", "plist", "png", "jpg", "jpeg", "gif", "webp", "svg", "heic", "zip", "tar", "gz",
        "mp3", "wav", "m4a", "mov", "mp4",
    ]

    /// The type a file's mark names on its plate ("LOG", "CSV"), or nil for the plain page.
    public static func typeLabel(_ name: String) -> String? {
        let ext = (name as NSString).pathExtension.lowercased()
        return knownTypes.contains(ext) ? ext.uppercased() : nil
    }

    /// The drop plate's limits, as one line.
    public static let dropLimits = "Up to \(maxCount) files, \(sizeText(maxBytes)) each"

    /// The drop plate's count, from the files dragged against the `held` already going with the message: how many go
    /// and how many more fit, or, when they don't all fit, how many won't be sent.
    public static func dropLine(dragged: Int, held: Int) -> String {
        let room = max(0, maxCount - held)
        func files(_ n: Int) -> String { n == 1 ? "1 file" : "\(n) files" }
        if dragged > room {
            let fit = room == 0 ? "none fit" : room == 1 ? "1 fits" : "\(room) fit"
            return "\(files(dragged)) · \(fit); \(dragged - room) won’t be sent"
        }
        let goes = "\(files(dragged)) \(dragged == 1 ? "goes" : "go") with your next message"
        let left = room - dragged
        return left == 0 ? goes : "\(goes) · \(left) more \(left == 1 ? "fits" : "fit")"
    }

    /// A file added to a tray that holds `count` already: refused on its name and size alone when over a limit (`read` is
    /// then never called), or when it cannot be read; else ready to go, under a new id.
    static func taking(name raw: String, size: Int, count: Int, read: () throws -> Data) -> Attachment {
        let name = fileName(raw)
        func refused(_ reason: String) -> Attachment {
            Attachment(id: newID(), name: name, size: size, isImage: false, data: nil, state: .refused(reason))
        }
        if count >= maxCount { return refused("\(name) can’t be sent: a message carries up to \(maxCount) files.") }
        // Over the size limit on its size alone; an empty file or a folder is known once read, which costs nothing.
        if size > maxBytes, let reason = sizeRefusal(name, size) { return refused(reason) }
        let data: Data
        do { data = try read() } catch is Folder {
            return refused("\(name) is a folder; only files can be sent.")
        } catch { return refused("\(name) can’t be read: \(error.localizedDescription)") }
        if let reason = sizeRefusal(name, data.count) { return refused(reason) }
        return Attachment(
            id: newID(), name: name, size: data.count, isImage: isImage(data), data: data, state: .uploading(sent: 0))
    }

    /// The same file with its bytes let go: what a start keeps once it is answered.
    func withoutBytes() -> Attachment {
        Attachment(id: id, name: name, size: size, isImage: isImage, data: nil, state: state)
    }

    /// The same bytes as a new upload, from the start.
    func again() -> Attachment {
        Attachment(id: Self.newID(), name: name, size: size, isImage: isImage, data: data, state: .uploading(sent: 0))
    }

    static func sizeRefusal(_ name: String, _ size: Int) -> String? {
        if size == 0 { return "\(name) is empty." }
        if size > maxBytes {
            return "\(name) is \(sizeText(size)); files over \(sizeText(maxBytes)) can’t be sent."
        }
        return nil
    }

    static func newID() -> String { UUID().uuidString.lowercased() }

    /// A size as the tiles write it: "4 KB", "2.4 MB".
    public static func sizeText(_ bytes: Int) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .memory)
    }
}

extension AppModel {
    /// This run's computer takes files, and the run takes a reply they could go with: no file control is drawn, and
    /// no drop taken, otherwise.
    public func canUpload(_ key: String) -> Bool {
        !sampleFleetActive && connection.capabilities?.takesUploads == true && drafts(for: key) != nil
            && shownSummary(for: key)?.takesNoReply != true
    }

    /// The files going with this run's next message, in order.
    public func files(for key: String) -> [Attachment] { drafts(for: key)?.files ?? [] }

    /// Adds a file to this run's next message and sends it to the computer at once. Its limits are checked first, with
    /// only its name and size: `read` is called only for a file inside them, and its failure refuses the tile.
    public func attach(name: String, size: Int, to key: String, read: () throws -> Data) {
        guard let drafts = drafts(for: key) else { return }
        let file = Attachment.taking(name: name, size: size, count: drafts.files.count, read: read)
        drafts.files.append(file)
        if case .uploading = file.state { Task { await upload(file.id, of: key) } }
    }

    public func attach(data: Data, name: String, to key: String) {
        attach(name: name, size: data.count, to: key) { data }
    }

    public func removeFile(_ id: String, from key: String) {
        drafts(for: key)?.files.removeAll { $0.id == id }
    }

    /// The same bytes again, under a new id, from the start.
    public func retryFile(_ id: String, in key: String) {
        guard let drafts = drafts(for: key), let index = drafts.files.firstIndex(where: { $0.id == id }),
            case .failed = drafts.files[index].state, drafts.files[index].data != nil
        else { return }
        let again = drafts.files[index].again()
        drafts.files[index] = again
        Task { await upload(again.id, of: key) }
    }

    /// Why the files hold this run's message back, or nil when every one has arrived (or there are none): a file over a
    /// limit first, in its own words, as it can only be removed; then one that failed; then those still on their way.
    public func filesBlockedReason(for key: String) -> String? {
        let files = files(for: key)
        for file in files { if case .refused(let reason) = file.state { return reason } }
        let travelling = files.filter { if case .uploading = $0.state { true } else { false } }.count
        let failed = files.filter { if case .failed = $0.state { true } else { false } }.count
        if failed > 0 {
            return failed == 1
                ? "Retry or remove the file that didn’t arrive." : "Retry or remove the \(failed) files that didn’t arrive."
        }
        if travelling > 0 {
            return travelling == 1 ? "Waiting for 1 file to arrive." : "Waiting for \(travelling) files to arrive."
        }
        return nil
    }

    /// The upload ids the next message carries: every file, once all have arrived.
    func arrivedFileIDs(_ key: String) -> [String] {
        filesBlockedReason(for: key) == nil ? files(for: key).map(\.id) : []
    }

    /// The message carried these: they leave the tray, and a file added while it waited for its answer stays.
    func filesSent(_ ids: [String], from key: String) {
        guard !ids.isEmpty, let drafts = drafts(for: key) else { return }
        drafts.files.removeAll { ids.contains($0.id) }
    }

    /// The link dropped, and the computer removed every upload no message had carried: each must go again.
    func filesLostWithLink() {
        for drafts in shownDrafts where !drafts.files.isEmpty {
            drafts.files = drafts.files.map { file in
                guard case .refused = file.state else {
                    var lost = file
                    lost.state = .failed("The link dropped before it was sent.")
                    return lost
                }
                return file
            }
        }
    }

    private func upload(_ id: String, of key: String) async {
        guard let file = files(for: key).first(where: { $0.id == id }) else { return }
        let end = await sendFile(file, session: key) { setFile(id, of: key, to: $0) }
        setFile(id, of: key, to: end)
    }

    /// Sends one file to the computer in chunks, for `session`'s run or, with none, for a start on this connection:
    /// `progress` hears each step on the way and stops the upload by answering false. What became of it: arrived, or
    /// failed with why.
    func sendFile(_ file: Attachment, session: String?, progress: (Attachment.State) -> Bool) async -> Attachment.State {
        guard let data = file.data else { return .failed("\(file.name) can’t be read.") }
        let id = file.id
        let name = file.name
        var offset = 0
        while true {
            let end = min(offset + Attachment.chunkBytes, data.count)
            let result: PutFileResult
            do {
                result = try await connection.putFile(
                    session: session, uploadID: id, name: name, size: data.count, offset: offset,
                    chunk: data.subdata(in: offset..<end))
            } catch {
                return .failed(error.localizedDescription)
            }
            switch result {
            case .refused(let reason):
                return .failed(reason)
            case .received(let held) where held >= data.count:
                return .arrived
            case .received(let held) where held > offset:
                offset = held
                guard progress(.uploading(sent: held)) else { return .failed("Stopped.") }
            case .received:
                return .failed("The computer stopped taking the file.")
            }
        }
    }

    /// Sets one file's state while it is still in the tray: false once it was removed or sent, so its upload stops.
    @discardableResult
    private func setFile(_ id: String, of key: String, to state: Attachment.State) -> Bool {
        guard let drafts = drafts(for: key), let index = drafts.files.firstIndex(where: { $0.id == id }) else {
            return false
        }
        drafts.files[index].state = state
        return true
    }

}

/// **A sent message's files**, from its stored text: the daemon gives the agent each file's path on a line after
/// "Attached:", after the typed words, and the conversation shows them as files again. A path is the daemon's
/// `<home>/uploads/<run>/<upload>/<name>`, where home is the computer's CodeConnect folder, wherever that is.
public enum AttachedFiles {
    /// The typed words and the files' paths, or the text as it is with no paths when it carries none.
    public static func split(_ text: String) -> (words: String, paths: [String]) {
        let marker = "Attached:\n"
        let range: Range<String.Index>
        if text.hasPrefix(marker) {
            range = text.startIndex..<text.index(text.startIndex, offsetBy: marker.count)
        } else if let found = text.range(of: "\n\n" + marker, options: .backwards) {
            range = found
        } else {
            return (text, [])
        }
        let paths = text[range.upperBound...].split(separator: "\n").map(String.init)
        guard !paths.isEmpty, paths.allSatisfy(isUpload) else {
            return (text, [])
        }
        return (String(text[..<range.lowerBound]), paths)
    }

    /// An absolute path whose last four components are `uploads`, a run, an upload and a name.
    private static func isUpload(_ path: String) -> Bool {
        let parts = path.split(separator: "/", omittingEmptySubsequences: false)
        return path.hasPrefix("/") && parts.count >= 5 && parts[parts.count - 4] == "uploads"
            && !parts.suffix(3).contains(where: \.isEmpty)
    }
}

#if DEBUG
    extension AppModel {
        /// `-CC_FIXTURE_FILES YES`: a render's run tray holds one file in each state, a picture on its way at 40%, one
        /// arrived, one the computer refused and one over the limit. Nothing is sent.
        public func seedFixtureFiles(_ key: String) {
            guard UserDefaults.standard.bool(forKey: "CC_FIXTURE_FILES"), let drafts = drafts(for: key),
                drafts.files.isEmpty
            else { return }
            let picture = Self.fixturePicture
            drafts.files = [
                Attachment(
                    id: "fx-up-1", name: "screenshot.png", size: picture.count, isImage: true, data: picture,
                    state: .arrived),
                Attachment(
                    id: "fx-up-2", name: "schema.sql", size: 4_210, isImage: false, data: Data(count: 4_210),
                    state: .uploading(sent: 1_684)),
                Attachment(
                    id: "fx-up-3", name: "trace.log", size: 812_000, isImage: false, data: Data(count: 812_000),
                    state: .failed("The link dropped before it was sent.")),
                Attachment(
                    id: "fx-up-4", name: "recording.mov", size: 48_300_000, isImage: false, data: nil,
                    state: .refused(Attachment.sizeRefusal("recording.mov", 48_300_000) ?? "")),
            ]
        }

        /// `-CC_FIXTURE_FILES YES`: the start box holds a picture, a file and one over the limit. Nothing is sent.
        public static func seedFixtureFiles(_ draft: StartDraft) {
            guard UserDefaults.standard.bool(forKey: "CC_FIXTURE_FILES"), draft.files.isEmpty else { return }
            draft.attach(name: "screenshot.png", size: fixturePicture.count) { fixturePicture }
            draft.attach(name: "schema.sql", size: 4_210) { Data(count: 4_210) }
            draft.attach(name: "recording.mov", size: 48_300_000) { Data() }
        }

        /// A small picture for the fixture's image tile: a 96×64 PNG drawn as a page's screenshot.
        static let fixturePicture = Data(
            base64Encoded:
                "iVBORw0KGgoAAAANSUhEUgAAAGAAAABACAIAAABqVuVZAAAAzElEQVR42u3YMQ6CMBQGYCGegsHVE0BYOZGzxtHI7IlYCVzKwcFFnUrNa75/JaH0y2v7aNUcjjv5nhoBIECAAAECBAiQfMr+9+Pr5Zzza273UQVZYoAAyTuV6w4VBAgQoLCddNe1EWe1LKsKssT0QSoIECCJfcwnPM5VECDHvAoCBAgQIAEEKOOvxivDaU413vToS6ughDrJ3/Z/oC3mE8vIHgQIECBAgIoF2qKvi9Ur1pnnE66TduVqDwIECBAgQIAACSBAgAABAlRYnuXgGl8vRte/AAAAAElFTkSuQmCC")
            ?? Data()
    }
#endif

extension AppModel {
    /// A start's files, sent for this send of it, each under a new id: the computer keeps a start's files only for the
    /// connection that sent them, and a start that was sent has spent them. Their ids in order, or nil when one did not
    /// arrive: the start fails as one that did not start, its details naming the file and why, and Start again sends them
    /// again.
    func sendStartFiles(_ requestID: String) async -> [String]? {
        guard let start = starter.starts[requestID], !start.files.isEmpty else { return [] }
        var files = start.files.map { $0.again() }
        starter.setFiles(files, of: requestID)
        for index in files.indices {
            let end = await sendFile(files[index], session: nil) { state in
                files[index].state = state
                starter.setFiles(files, of: requestID)
                return starter.starts[requestID]?.retryStopped == false
            }
            files[index].state = end
            starter.setFiles(files, of: requestID)
            if case .failed(let reason) = end {
                starter.resolve(
                    requestID, .failed(.launchFailed, detail: "\(files[index].name) didn’t reach the computer: \(reason)"))
                return nil
            }
        }
        return files.map(\.id)
    }
}
