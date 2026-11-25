import Foundation

/// **One file going with a message** (minor 50): sent to the computer as soon as it is added, in chunks, so the
/// message waits only for what has not arrived yet. Its id is the upload's: a retry is another upload, under a new id.
public struct Attachment: Identifiable, Equatable, Sendable {
    /// The daemon's bounds (`ws.rs`): a file, the files one message carries, and one chunk, decoded.
    public static let maxBytes = 10 * 1024 * 1024
    public static let maxCount = 10
    public static let chunkBytes = 512 * 1024

    public enum State: Equatable, Sendable {
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

    /// PNG, JPEG, GIF or WebP, by their first bytes.
    static func isImage(_ data: Data) -> Bool {
        let head = [UInt8](data.prefix(12))
        func starts(_ bytes: [UInt8], at offset: Int = 0) -> Bool {
            head.count >= offset + bytes.count && Array(head[offset..<offset + bytes.count]) == bytes
        }
        return starts([0x89, 0x50, 0x4E, 0x47]) || starts([0xFF, 0xD8, 0xFF]) || starts(Array("GIF8".utf8))
            || (starts(Array("RIFF".utf8)) && starts(Array("WEBP".utf8), at: 8))
    }

    /// A size as the tiles write it: "4 KB", "2.4 MB".
    public static func sizeText(_ bytes: Int) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .memory)
    }
}

extension AppModel {
    /// This run's computer takes files: no file control is drawn otherwise.
    public func canUpload(_ key: String) -> Bool {
        !sampleFleetActive && connection.capabilities?.takesUploads == true && drafts(for: key) != nil
    }

    /// The files going with this run's next message, in order.
    public func files(for key: String) -> [Attachment] { drafts(for: key)?.files ?? [] }

    /// Adds a file to this run's next message and sends it to the computer at once. Its limits are checked first, with
    /// only its name and size: `read` is called only for a file inside them, and its failure refuses the tile.
    public func attach(name: String, size: Int, to key: String, read: () throws -> Data) {
        guard let drafts = drafts(for: key) else { return }
        let name = Attachment.fileName(name)
        func refuse(_ reason: String) {
            drafts.files.append(
                Attachment(id: Self.uploadID(), name: name, size: size, isImage: false, data: nil, state: .refused(reason)))
        }
        if drafts.files.count >= Attachment.maxCount {
            return refuse("A message carries up to \(Attachment.maxCount) files.")
        }
        if let reason = Self.sizeRefusal(size) { return refuse(reason) }
        let data: Data
        do { data = try read() } catch { return refuse(error.localizedDescription) }
        if let reason = Self.sizeRefusal(data.count) { return refuse(reason) }
        let file = Attachment(
            id: Self.uploadID(), name: name, size: data.count, isImage: Attachment.isImage(data), data: data,
            state: .uploading(sent: 0))
        drafts.files.append(file)
        Task { await upload(file.id, of: key) }
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
            case .failed = drafts.files[index].state, let data = drafts.files[index].data
        else { return }
        let old = drafts.files[index]
        let again = Attachment(
            id: Self.uploadID(), name: old.name, size: data.count, isImage: old.isImage, data: data,
            state: .uploading(sent: 0))
        drafts.files[index] = again
        Task { await upload(again.id, of: key) }
    }

    /// Why the files hold this run's message back, or nil when every one has arrived (or there are none).
    public func filesBlockedReason(for key: String) -> String? {
        let files = files(for: key)
        let travelling = files.filter { if case .uploading = $0.state { true } else { false } }.count
        let stuck = files.filter {
            switch $0.state {
            case .refused, .failed: true
            default: false
            }
        }.count
        if stuck > 0 {
            return stuck == 1 ? "Remove or retry the file that didn’t arrive." : "Remove or retry the files that didn’t arrive."
        }
        if travelling > 0 {
            return travelling == 1 ? "Waiting for the file to arrive." : "Waiting for \(travelling) files to arrive."
        }
        return nil
    }

    /// The upload ids the next message carries: every file, once all have arrived.
    func arrivedFileIDs(_ key: String) -> [String] {
        filesBlockedReason(for: key) == nil ? files(for: key).map(\.id) : []
    }

    /// The message carried them: the tray starts again empty.
    func filesSent(_ key: String) {
        if drafts(for: key)?.files.isEmpty == false { drafts(for: key)?.files = [] }
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
        guard let data = files(for: key).first(where: { $0.id == id })?.data,
            let name = files(for: key).first(where: { $0.id == id })?.name
        else { return }
        var offset = 0
        while true {
            let end = min(offset + Attachment.chunkBytes, data.count)
            let result: PutFileResult
            do {
                result = try await connection.putFile(
                    session: key, uploadID: id, name: name, size: data.count, offset: offset,
                    chunk: data.subdata(in: offset..<end))
            } catch {
                setFile(id, of: key, to: .failed(error.localizedDescription))
                return
            }
            switch result {
            case .refused(let reason):
                setFile(id, of: key, to: .failed(reason))
                return
            case .received(let held) where held >= data.count:
                setFile(id, of: key, to: .arrived)
                return
            case .received(let held) where held > offset:
                offset = held
                guard setFile(id, of: key, to: .uploading(sent: held)) else { return }
            case .received:
                setFile(id, of: key, to: .failed("The computer stopped taking the file."))
                return
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

    static func sizeRefusal(_ size: Int) -> String? {
        if size == 0 { return "This file is empty." }
        if size > Attachment.maxBytes {
            return "This file is \(Attachment.sizeText(size)). Files go up to \(Attachment.sizeText(Attachment.maxBytes))."
        }
        return nil
    }

    private static func uploadID() -> String { UUID().uuidString.lowercased() }
}

/// **A sent message's files**, from its stored text: the daemon gives the agent each file's path on a line after
/// "Attached:", after the typed words, and the conversation shows them as files again.
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
        guard !paths.isEmpty, paths.allSatisfy({ $0.hasPrefix("/") && $0.contains("/.codeconnect/uploads/") }) else {
            return (text, [])
        }
        return (String(text[..<range.lowerBound]), paths)
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
                    state: .refused(Self.sizeRefusal(48_300_000) ?? "")),
            ]
        }

        /// A small picture for the fixture's image tile: a 96×64 PNG drawn as a page's screenshot.
        private static let fixturePicture = Data(
            base64Encoded:
                "iVBORw0KGgoAAAANSUhEUgAAAGAAAABACAIAAABqVuVZAAAAzElEQVR42u3YMQ6CMBQGYCGegsHVE0BYOZGzxtHI7IlYCVzKwcFFnUrNa75/JaH0y2v7aNUcjjv5nhoBIECAAAECBAiQfMr+9+Pr5Zzza273UQVZYoAAyTuV6w4VBAgQoLCddNe1EWe1LKsKssT0QSoIECCJfcwnPM5VECDHvAoCBAgQIAEEKOOvxivDaU413vToS6ughDrJ3/Z/oC3mE8vIHgQIECBAgIoF2qKvi9Ur1pnnE66TduVqDwIECBAgQIAACSBAgAABAlRYnuXgGl8vRte/AAAAAElFTkSuQmCC")
            ?? Data()
    }
#endif
