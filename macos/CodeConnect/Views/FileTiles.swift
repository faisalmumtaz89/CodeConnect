import AppKit
import CodeConnectKit
import SwiftUI
import UniformTypeIdentifiers

/// The box's paperclip, first in its bar, where the computer takes files: it opens the Mac's file panel, and each file
/// chosen goes to `take`.
struct AttachButton: View {
    let id: String
    let take: FileIntake.Take

    var body: some View {
        Button { FileIntake.choose(take) } label: {
            Image(systemName: "paperclip")
                .font(.system(size: 13, weight: .medium))
                .foregroundStyle(Theme.text2)
                .inputBoxChip(open: false)
        }
        .buttonStyle(.hover(radius: 8, id: id))
        .help("Attach files")
        .accessibilityLabel("Attach files")
        .accessibilityIdentifier(id)
    }
}

/// How files reach a tray: the file panel, a drop on the box, and a picture pasted into it. Each file goes to `take` by
/// its name and size, and is read only if `take` keeps it (Kit's limits).
@MainActor
enum FileIntake {
    typealias Take = (_ name: String, _ size: Int, _ read: () throws -> Data) -> Void

    static func choose(_ take: Take) {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        panel.canChooseFiles = true
        guard panel.runModal() == .OK else { return }
        add(panel.urls, take)
    }

    static func add(_ urls: [URL], _ take: Take) {
        for url in urls {
            let size = (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0
            take(url.lastPathComponent, size) { try Data(contentsOf: url) }
        }
    }

    /// What a drop on the box carries: files by their URLs, or a picture with none.
    static func drop(_ providers: [NSItemProvider], _ take: @escaping Take) -> Bool {
        var taken = false
        for provider in providers {
            if provider.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) {
                taken = true
                _ = provider.loadObject(ofClass: URL.self) { url, _ in
                    guard let url else { return }
                    Task { @MainActor in add([url], take) }
                }
            } else if provider.hasItemConformingToTypeIdentifier(UTType.image.identifier) {
                taken = true
                provider.loadDataRepresentation(forTypeIdentifier: UTType.png.identifier) { data, _ in
                    guard let data else { return }
                    Task { @MainActor in take("image.png", data.count) { data } }
                }
            }
        }
        return taken
    }

    /// ⌘V with a picture, or files, on the pasteboard and no words: they go to the tray, and the paste is taken.
    /// Words paste into the field as ever.
    static func paste(_ take: Take) -> Bool {
        let board = NSPasteboard.general
        if board.string(forType: .string) != nil { return false }
        if let urls = board.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL],
            !urls.isEmpty
        {
            add(urls, take)
            return true
        }
        guard let image = NSImage(pasteboard: board), let tiff = image.tiffRepresentation,
            let png = NSBitmapImageRep(data: tiff)?.representation(using: .png, properties: [:])
        else { return false }
        take("image.png", png.count) { png }
        return true
    }
}

extension View {
    /// The box takes files dropped on it, and a picture or files pasted into it, while `on`.
    func takesFiles(_ on: Bool, _ take: @escaping FileIntake.Take) -> some View {
        onDrop(of: [.fileURL, .image], isTargeted: nil) { providers in on && FileIntake.drop(providers, take) }
            .onKeyPress(keys: ["v"], phases: .down) { press in
                guard press.modifiers == .command, on, FileIntake.paste(take) else { return .ignored }
                return .handled
            }
    }
}

/// The files going with the next message, above its words, in a row: a picture as a 64 pt square of itself, any other
/// file as a tile of its icon, whole name and size, each with its ×. One on its way shows how far it has got in its
/// icon's place; one that failed offers Retry there. Under them, in one line, why they hold the message back.
struct FileTray: View {
    let files: [Attachment]
    let reason: String?
    let remove: (String) -> Void
    let retry: (String) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ScrollView(.horizontal) {
                HStack(spacing: 10) {
                    ForEach(files) { file in
                        FileTile(file: file) { remove(file.id) } retry: { retry(file.id) }
                    }
                }
                // Room for each tile's ×, which stands half outside it.
                .padding(.top, 6)
                .padding(.trailing, 6)
            }
            .scrollIndicators(.never)
            if let reason {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Image(systemName: "exclamationmark.circle").foregroundStyle(Theme.amber).accessibilityHidden(true)
                    Text(reason).foregroundStyle(Theme.text2).lineLimit(1).truncationMode(.middle)
                }
                .font(Theme.ui(12))
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier("session.composer.files.reason")
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("session.composer.files")
    }
}

private struct FileTile: View {
    let file: Attachment
    let remove: () -> Void
    let retry: () -> Void

    static let side: CGFloat = 64

    var body: some View {
        face
            .frame(height: Self.side)
            .background(Theme.raised)
            .clipShape(RoundedRectangle(cornerRadius: 10))
            .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(trouble ? Theme.red.opacity(0.5) : Card.border))
            .overlay(alignment: .topTrailing) {
                CornerButton(symbol: "xmark", id: "session.composer.file.\(file.id).remove", label: "Remove \(file.name)",
                    action: remove)
            }
            .help("\(file.name), \(Attachment.sizeText(file.size))")
            .accessibilityElement(children: .contain)
            .accessibilityLabel(spoken)
            .accessibilityIdentifier("session.composer.file.\(file.id)")
    }

    @ViewBuilder private var face: some View {
        if file.isImage, let data = file.data, let image = NSImage(data: data), file.state == .arrived {
            Image(nsImage: image).resizable().aspectRatio(contentMode: .fill)
                .frame(width: Self.side, height: Self.side)
        } else {
            HStack(spacing: 10) {
                mark.frame(width: 28, height: 28)
                VStack(alignment: .leading, spacing: 3) {
                    FileName(name: file.name)
                    Text(Attachment.sizeText(file.size)).font(Theme.ui(11)).foregroundStyle(Theme.text3).lineLimit(1)
                }
            }
            .padding(.leading, 12)
            .padding(.trailing, 16)
        }
    }

    /// The icon's place: the file's icon, how far it has got while on its way, Retry once it failed, or the mark of a
    /// file over a limit.
    @ViewBuilder private var mark: some View {
        switch file.state {
        case .uploading(let sent):
            Ring(fraction: Double(sent) / Double(max(file.size, 1)))
        case .failed:
            Button(action: retry) {
                Image(systemName: "arrow.clockwise").font(.system(size: 13, weight: .semibold)).foregroundStyle(Theme.text)
                    .frame(width: 28, height: 28)
                    .background(Theme.selected, in: Circle())
                    .contentShape(Circle())
            }
            .buttonStyle(.hover(radius: 14, over: true, id: "session.composer.file.\(file.id).retry"))
            .help("Send it again")
            .accessibilityLabel("Retry \(file.name)")
            .accessibilityIdentifier("session.composer.file.\(file.id).retry")
        case .refused:
            Image(systemName: "exclamationmark.circle").font(.system(size: 17)).foregroundStyle(Theme.red)
        case .arrived:
            Image(systemName: file.isImage ? "photo" : "doc").font(.system(size: 17)).foregroundStyle(Theme.text2)
        }
    }

    private var trouble: Bool {
        switch file.state {
        case .refused, .failed: true
        default: false
        }
    }

    private var spoken: String {
        switch file.state {
        case .uploading(let sent): "\(file.name), sending, \(Int(Double(sent) / Double(max(file.size, 1)) * 100)) percent"
        case .arrived: "\(file.name), \(Attachment.sizeText(file.size))"
        case .refused(let reason), .failed(let reason): "\(file.name), \(reason)"
        }
    }
}

/// A file's name as wide as it needs, up to `widest`; a longer one is cut in its middle, so its extension stays.
private struct FileName: View {
    let name: String
    static let widest: CGFloat = 160

    var body: some View {
        let needs = (name as NSString).size(withAttributes: [.font: Theme.uiFont(12, .medium)]).width.rounded(.up) + 1
        Text(name).font(Theme.ui(12, .medium)).foregroundStyle(Theme.text)
            .lineLimit(1).truncationMode(.middle)
            .frame(width: min(needs, Self.widest), alignment: .leading)
    }
}

/// A tile's ×, half outside its corner.
private struct CornerButton: View {
    let symbol: String
    let id: String
    let label: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Image(systemName: symbol).font(.system(size: 7.5, weight: .bold)).foregroundStyle(Theme.text)
                .frame(width: 16, height: 16)
                .background(Theme.sheet, in: Circle())
                .overlay(Circle().strokeBorder(Color.white.opacity(0.12)))
                .contentShape(Circle())
        }
        .buttonStyle(.hover(radius: 8, over: true, id: id))
        .offset(x: 5, y: -5)
        .help(label)
        .accessibilityLabel(label)
        .accessibilityIdentifier(id)
    }
}

/// How far a file on its way has got, in its icon's place.
private struct Ring: View {
    let fraction: Double

    var body: some View {
        ZStack {
            Circle().stroke(Color.white.opacity(0.15), lineWidth: 2.5)
            Circle().trim(from: 0, to: max(0.04, fraction))
                .stroke(Theme.text, style: StrokeStyle(lineWidth: 2.5, lineCap: .round))
                .rotationEffect(.degrees(-90))
        }
        .padding(3)
    }
}

/// A sent message's files, read back from its text (Kit's `AttachedFiles`): each a tile of its icon and whole name, as
/// in the box; the bytes stay on the computer.
struct SentFiles: View {
    let paths: [String]

    var body: some View {
        HStack(spacing: 8) {
            ForEach(paths, id: \.self) { path in
                let name = (path as NSString).lastPathComponent
                HStack(spacing: 10) {
                    Image(systemName: "doc").font(.system(size: 15)).foregroundStyle(Theme.text2).frame(width: 22)
                    FileName(name: name)
                }
                .padding(.horizontal, 12)
                .frame(height: 40)
                .background(Theme.raised, in: RoundedRectangle(cornerRadius: 10))
                .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Card.border))
                .help(path)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(name)
            }
        }
    }
}
