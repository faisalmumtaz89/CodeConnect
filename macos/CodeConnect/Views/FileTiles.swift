import AppKit
import CodeConnectKit
import SwiftUI
import UniformTypeIdentifiers

/// The box's paperclip, first in its bar, on a run whose computer takes files: it opens the Mac's file panel, and each
/// file chosen goes to the computer at once (Kit's `attach`).
struct AttachButton: View {
    let model: AppModel
    let key: String

    var body: some View {
        Button { FileIntake.choose(model: model, key: key) } label: {
            Image(systemName: "paperclip")
                .font(.system(size: 13, weight: .medium))
                .foregroundStyle(Theme.text2)
                .inputBoxChip(open: false)
        }
        .buttonStyle(.hover(radius: 8, id: "session.composer.attach"))
        .help("Attach files")
        .accessibilityLabel("Attach files")
        .accessibilityIdentifier("session.composer.attach")
    }
}

/// How files reach a run's tray: the file panel, a drop on the box, and a picture pasted into it.
@MainActor
enum FileIntake {
    static func choose(model: AppModel, key: String) {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        panel.canChooseFiles = true
        guard panel.runModal() == .OK else { return }
        add(panel.urls, model: model, key: key)
    }

    /// Files by their URLs, each checked by its size before it is read.
    static func add(_ urls: [URL], model: AppModel, key: String) {
        for url in urls {
            let size = (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0
            model.attach(name: url.lastPathComponent, size: size, to: key) { try Data(contentsOf: url) }
        }
    }

    /// What a drop on the box carries: files by their URLs, or a picture with none.
    static func drop(_ providers: [NSItemProvider], model: AppModel, key: String) -> Bool {
        var taken = false
        for provider in providers {
            if provider.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) {
                taken = true
                _ = provider.loadObject(ofClass: URL.self) { url, _ in
                    guard let url else { return }
                    Task { @MainActor in add([url], model: model, key: key) }
                }
            } else if provider.hasItemConformingToTypeIdentifier(UTType.image.identifier) {
                taken = true
                provider.loadDataRepresentation(forTypeIdentifier: UTType.png.identifier) { data, _ in
                    guard let data else { return }
                    Task { @MainActor in model.attach(data: data, name: "image.png", to: key) }
                }
            }
        }
        return taken
    }

    /// ⌘V with a picture, or files, on the pasteboard and no words: they go to the tray, and the paste is taken.
    /// Words paste into the field as ever.
    static func paste(model: AppModel, key: String) -> Bool {
        let board = NSPasteboard.general
        if board.string(forType: .string) != nil { return false }
        if let urls = board.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL],
            !urls.isEmpty
        {
            add(urls, model: model, key: key)
            return true
        }
        guard let image = NSImage(pasteboard: board), let tiff = image.tiffRepresentation,
            let png = NSBitmapImageRep(data: tiff)?.representation(using: .png, properties: [:])
        else { return false }
        model.attach(data: png, name: "image.png", to: key)
        return true
    }
}

/// The files going with the next message, above its words: 64 pt tiles in a row, a picture as itself and any other
/// file by its name and size, each with its ×. One on its way shows how far it has got; one that failed offers to go
/// again; one over a limit says why on its face.
struct FileTray: View {
    let model: AppModel
    let key: String

    var body: some View {
        ScrollView(.horizontal) {
            HStack(spacing: 10) {
                ForEach(model.files(for: key)) { file in
                    FileTile(file: file) { model.removeFile(file.id, from: key) } retry: {
                        model.retryFile(file.id, in: key)
                    }
                }
            }
            // Room for each tile's ×, which stands half outside it.
            .padding(.top, 6)
            .padding(.trailing, 6)
        }
        .scrollIndicators(.never)
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
            .frame(width: Self.side, height: Self.side)
            .background(Theme.raised)
            .clipShape(RoundedRectangle(cornerRadius: 10))
            .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(trouble != nil ? Theme.red.opacity(0.5) : Card.border))
            .overlay { if case .uploading(let sent) = file.state { Progress(fraction: Double(sent) / Double(max(file.size, 1))) } }
            .overlay(alignment: .topTrailing) { corner(remove, symbol: "xmark", id: "remove", label: "Remove \(file.name)") }
            .help(trouble ?? "\(file.name), \(Attachment.sizeText(file.size))")
            .accessibilityElement(children: .contain)
            .accessibilityLabel(spoken)
            .accessibilityIdentifier("session.composer.file.\(file.id)")
    }

    @ViewBuilder private var face: some View {
        if case .failed = file.state {
            Button(action: retry) {
                VStack(spacing: 4) {
                    Image(systemName: "arrow.clockwise").font(.system(size: 14, weight: .medium))
                    Text("Retry").font(Theme.ui(10.5, .medium))
                }
                .foregroundStyle(Theme.text)
                .frame(width: Self.side, height: Self.side)
                .contentShape(Rectangle())
            }
            .buttonStyle(.hover(radius: 10, id: "session.composer.file.\(file.id).retry"))
            .accessibilityLabel("Retry \(file.name)")
            .accessibilityIdentifier("session.composer.file.\(file.id).retry")
        } else if file.isImage, let data = file.data, let image = NSImage(data: data) {
            Image(nsImage: image).resizable().aspectRatio(contentMode: .fill)
                .frame(width: Self.side, height: Self.side)
                .opacity(file.state == .arrived ? 1 : 0.55)
        } else {
            VStack(alignment: .leading, spacing: 2) {
                Image(systemName: trouble == nil ? "doc" : "exclamationmark.circle")
                    .font(.system(size: 14))
                    .foregroundStyle(trouble == nil ? Theme.text2 : Theme.red)
                Spacer(minLength: 0)
                Text(file.name).font(Theme.ui(10.5, .medium)).foregroundStyle(Theme.text)
                    .lineLimit(1).truncationMode(.middle)
                Text(Attachment.sizeText(file.size)).font(Theme.ui(10)).foregroundStyle(Theme.text3).lineLimit(1)
            }
            .padding(8)
            .frame(width: Self.side, height: Self.side, alignment: .topLeading)
        }
    }

    private func corner(_ action: @escaping () -> Void, symbol: String, id: String, label: String) -> some View {
        Button(action: action) {
            Image(systemName: symbol).font(.system(size: 7.5, weight: .bold)).foregroundStyle(Theme.text)
                .frame(width: 16, height: 16)
                .background(Theme.sheet, in: Circle())
                .overlay(Circle().strokeBorder(Color.white.opacity(0.12)))
                .contentShape(Circle())
        }
        .buttonStyle(.hover(radius: 8, over: true, id: "session.composer.file.\(file.id).\(id)"))
        .offset(x: 5, y: -5)
        .help(label)
        .accessibilityLabel(label)
        .accessibilityIdentifier("session.composer.file.\(file.id).\(id)")
    }

    /// Why it cannot go as it is, or nil.
    private var trouble: String? {
        switch file.state {
        case .refused(let reason), .failed(let reason): reason
        default: nil
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

/// How far a file on its way has got: a ring on a dimmed tile.
private struct Progress: View {
    let fraction: Double

    var body: some View {
        ZStack {
            Color.black.opacity(0.35)
            Circle().stroke(Color.white.opacity(0.18), lineWidth: 2.5).frame(width: 22, height: 22)
            Circle().trim(from: 0, to: max(0.04, fraction))
                .stroke(Theme.text, style: StrokeStyle(lineWidth: 2.5, lineCap: .round))
                .rotationEffect(.degrees(-90))
                .frame(width: 22, height: 22)
        }
        .clipShape(RoundedRectangle(cornerRadius: 10))
        .allowsHitTesting(false)
    }
}

/// A sent message's files, read back from its text (Kit's `AttachedFiles`): tiles of the same size, each by its name;
/// the bytes stay on the computer.
struct SentFiles: View {
    let paths: [String]

    var body: some View {
        HStack(spacing: 8) {
            ForEach(paths, id: \.self) { path in
                let name = (path as NSString).lastPathComponent
                VStack(alignment: .leading, spacing: 2) {
                    Image(systemName: "doc").font(.system(size: 14)).foregroundStyle(Theme.text2)
                    Spacer(minLength: 0)
                    Text(name).font(Theme.ui(10.5, .medium)).foregroundStyle(Theme.text).lineLimit(1).truncationMode(.middle)
                }
                .padding(8)
                .frame(width: 64, height: 64, alignment: .topLeading)
                .background(Theme.raised, in: RoundedRectangle(cornerRadius: 10))
                .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Card.border))
                .help(path)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(name)
            }
        }
    }
}
