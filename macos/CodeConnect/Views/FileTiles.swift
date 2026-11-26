import AppKit
import CodeConnectKit
import SwiftUI

/// The box's paperclip, first in its bar, where the computer takes files: it opens the Mac's file panel, and each file
/// chosen goes to `take`.
struct AttachButton: View {
    let id: String
    let take: FileIntake.Take

    var body: some View {
        Button { FileIntake.choose(take) } label: {
            // As the bar's other icon controls draw (the run's "…"): 12 pt of glyph in the chip's insets.
            Image(systemName: "paperclip")
                .font(InputBoxChip.font)
                .foregroundStyle(Theme.text2)
                .frame(width: 12)
                .inputBoxChip(open: false)
        }
        .buttonStyle(.hover(radius: 8, id: id))
        .help("Attach files")
        .accessibilityLabel("Attach files")
        .accessibilityIdentifier(id)
    }
}

/// How files reach a tray: the file panel, a drop on the conversation, and a picture pasted into the box. Each file
/// goes to `take` by its name and size, and is read only if `take` keeps it (Kit's limits); a folder is refused unread.
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
            let values = try? url.resourceValues(forKeys: [.fileSizeKey, .isDirectoryKey])
            if values?.isDirectory == true {
                take(url.lastPathComponent, 0) { throw Attachment.Folder() }
            } else {
                take(url.lastPathComponent, values?.fileSize ?? 0) { try Data(contentsOf: url) }
            }
        }
    }

    /// The files a pasteboard carries: its file URLs, or else a picture with none (dragged out of a browser, or
    /// copied), as PNG bytes.
    static func files(on board: NSPasteboard) -> (urls: [URL], picture: Data?) {
        if let urls = board.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL],
            !urls.isEmpty
        {
            return (urls, nil)
        }
        guard let image = NSImage(pasteboard: board), let tiff = image.tiffRepresentation,
            let png = NSBitmapImageRep(data: tiff)?.representation(using: .png, properties: [:])
        else { return ([], nil) }
        return ([], png)
    }

    /// The names a drag's files go by, for the drop plate, read without decoding a picture: none when it carries no
    /// file.
    static func names(on board: NSPasteboard) -> [String] {
        if let urls = board.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL],
            !urls.isEmpty
        {
            return urls.map(\.lastPathComponent)
        }
        return board.availableType(from: [.png, .tiff]) == nil ? [] : ["image.png"]
    }

    /// Whatever files a pasteboard carries go to `take`; false when it carries none.
    static func take(from board: NSPasteboard, _ take: Take) -> Bool {
        let (urls, picture) = files(on: board)
        if let picture {
            take("image.png", picture.count) { picture }
            return true
        }
        add(urls, take)
        return !urls.isEmpty
    }

    /// ⌘V with files on the pasteboard (Finder's copy, which carries their names as words too), or a picture and no
    /// words: they go to the tray, and the paste is taken. Words alone paste into the field as ever.
    static func paste(_ take: Take) -> Bool {
        let board = NSPasteboard.general
        if board.string(forType: .string) != nil,
            (board.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) ?? []).isEmpty
        {
            return false
        }
        return self.take(from: board, take)
    }
}

extension EnvironmentValues {
    /// Files are being dragged over the view that takes them.
    @Entry var filesDragged = false
    /// Files are being dragged over where this box's files land: its rim lights. Set by the box that takes them only,
    /// so no other box under the drag lights.
    @Entry var filesOver = false
}

extension View {
    /// The box takes files or a picture pasted into it while `on` and its field has the keyboard.
    func pastesFiles(_ on: Bool, _ take: @escaping FileIntake.Take) -> some View {
        modifier(PastesFiles(active: on, take: take))
    }

    /// The whole of this view takes files dropped on it while `on`: while they are over it, a dashed plate covers it,
    /// `plateBottom` short of its foot (the box the files land in), counting them against the `held` already going.
    func takesDroppedFiles(_ on: Bool, held: Int, plateBottom: CGFloat, _ take: @escaping FileIntake.Take)
        -> some View
    {
        modifier(DroppedFiles(on: on, held: held, plateBottom: plateBottom, take: take))
    }
}

/// ⌘V is Edit › Paste's key, which the menu takes before the field sees a key press, and the field's own paste would
/// write a copied file's name: while `active`, a monitor sees ⌘V first and gives the pasteboard's files to `take`,
/// leaving any other paste to the field.
private struct PastesFiles: ViewModifier {
    let active: Bool
    let take: FileIntake.Take
    @State private var monitor: Any?

    func body(content: Content) -> some View {
        content
            .onChange(of: active, initial: true) { _, on in
                stop()
                guard on else { return }
                // AppKit calls a local monitor on the main thread.
                nonisolated(unsafe) let take = take
                monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
                    let paste = event.modifierFlags.intersection(.deviceIndependentFlagsMask) == .command
                        && event.charactersIgnoringModifiers == "v"
                    guard paste, MainActor.assumeIsolated({ FileIntake.paste(take) }) else { return event }
                    return nil
                }
            }
            .onDisappear(perform: stop)
    }

    private func stop() {
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
    }
}

/// Files over the view, read from the drag's own pasteboard as they enter, so the plate can name and count them
/// before they land; a drag that carries no file or picture (words, a link) is not this view's, shows nothing, and
/// goes on to whatever under it takes it (a field types the words in). One AppKit view over the whole of it takes the
/// drag: AppKit finds a drag's destination by the types each view takes, front to back, so it stands in front of a
/// focused field's editor for files while letting clicks, the cursor and words through (`hitTest` is nil).
private struct DroppedFiles: ViewModifier {
    let on: Bool
    let held: Int
    let plateBottom: CGFloat
    let take: FileIntake.Take
    @State private var over: [String]?

    func body(content: Content) -> some View {
        content
            .environment(\.filesDragged, over != nil)
            .overlay {
                Receiver(on: on, over: $over, take: take)
                    .allowsHitTesting(false)
                    .accessibilityHidden(true)
            }
            .overlay {
                if let over {
                    DropPlate(names: over, held: held)
                        .padding(12)
                        .padding(.bottom, plateBottom)
                        .allowsHitTesting(false)
                }
            }
            #if DEBUG
                .task(id: on) { if on, !RenderWindow.droppedNames.isEmpty { over = RenderWindow.droppedNames } }
            #endif
    }

    private struct Receiver: NSViewRepresentable {
        let on: Bool
        @Binding var over: [String]?
        let take: FileIntake.Take

        func makeNSView(context: Context) -> Target { Target() }

        func updateNSView(_ view: Target, context: Context) {
            view.take = take
            view.show = { over = $0 }
            view.taking(on)
        }

        static func dismantleNSView(_ view: Target, coordinator: ()) { view.taking(false) }
    }

    private final class Target: NSView {
        var take: FileIntake.Take = { _, _, _ in }
        var show: ([String]?) -> Void = { _ in }
        private var on = false
        private var holding = false

        /// Registered for files only while the view takes them.
        func taking(_ next: Bool) {
            guard next != on else { return }
            on = next
            if on { registerForDraggedTypes([.fileURL, .png, .tiff]) } else { unregisterDraggedTypes(); clear() }
        }

        override func hitTest(_ point: NSPoint) -> NSView? { nil }

        override func draggingEntered(_ sender: any NSDraggingInfo) -> NSDragOperation {
            let names = on ? FileIntake.names(on: sender.draggingPasteboard) : []
            holding = !names.isEmpty
            show(holding ? names : nil)
            return holding ? .copy : []
        }

        override func draggingUpdated(_ sender: any NSDraggingInfo) -> NSDragOperation { holding ? .copy : [] }

        override func draggingExited(_ sender: (any NSDraggingInfo)?) { clear() }

        override func draggingEnded(_ sender: any NSDraggingInfo) { clear() }

        override func performDragOperation(_ sender: any NSDraggingInfo) -> Bool {
            let taken = on && holding
            clear()
            return taken && FileIntake.take(from: sender.draggingPasteboard, take)
        }

        private func clear() {
            guard holding else { return }
            holding = false
            show(nil)
        }
    }
}

/// While files are over the conversation: their type marks, what dropping does, counted, and the limits, on a dashed
/// plate.
private struct DropPlate: View {
    let names: [String]
    let held: Int

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 14) {
                // Fanned, as a stack of papers: at most three.
                let angles: [Double] = names.count == 1 ? [0] : names.count == 2 ? [-8, 8] : [-8, 0, 8]
                ForEach(Array(names.prefix(3).enumerated()), id: \.offset) { index, name in
                    FileTypeMark(name: name, height: 32).rotationEffect(.degrees(angles[index]))
                }
            }
            .padding(.bottom, 16)
            Text("Drop to attach").font(Theme.ui(14, .semibold)).foregroundStyle(Theme.text)
                .padding(.bottom, 8)
            Text(Attachment.dropLine(dragged: names.count, held: held)).font(Theme.ui(13)).foregroundStyle(Theme.text2)
                .padding(.bottom, 8)
            Text(Attachment.dropLimits).font(Theme.ui(12)).foregroundStyle(Theme.text3)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.window.opacity(0.86), in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Color.white.opacity(0.22), style: StrokeStyle(lineWidth: 1, dash: [4, 4])))
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("session.drop.plate")
    }
}

/// A file's type mark: a page with a folded corner and, for a type the app knows, its extension in small capitals on a
/// plate at its foot; any other file is the plain page. Neutral, as colour is kept for state.
struct FileTypeMark: View {
    let name: String
    var height: CGFloat = 24

    var body: some View {
        Image(systemName: "doc")
            .resizable()
            .aspectRatio(contentMode: .fit)
            .fontWeight(.light)
            .foregroundStyle(Theme.text2)
            .frame(height: height)
            .overlay(alignment: .bottom) {
                if let label = Attachment.typeLabel(name) {
                    Text(label)
                        .font(.system(size: 7 * height / 24, weight: .semibold))
                        .foregroundStyle(Theme.text)
                        .lineLimit(1)
                        .fixedSize()
                        .padding(.horizontal, 2)
                        .background(Theme.raised, in: RoundedRectangle(cornerRadius: 2))
                        .overlay(RoundedRectangle(cornerRadius: 2).strokeBorder(Color.white.opacity(0.18), lineWidth: 0.5))
                        .padding(.bottom, height / 8)
                }
            }
            .accessibilityHidden(true)
    }
}

/// The files going with the next message, above its words, in one row that scrolls sideways once it outgrows the
/// box: no bar, the cut-off edge fades, and the newest scrolls into view as it is added. Under them, in one line, why
/// they hold the message back.
struct FileTray: View {
    let files: [Attachment]
    let reason: String?
    let remove: (String) -> Void
    let retry: (String) -> Void
    @State private var cut = (leading: false, trailing: false)

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ScrollViewReader { reader in
                ScrollView(.horizontal) {
                    HStack(spacing: 10) {
                        ForEach(files) { file in
                            FileTile(file: file) { remove(file.id) } retry: { retry(file.id) }.id(file.id)
                        }
                    }
                    // Room for each tile's ×, which stands half outside it.
                    .padding(.top, 6)
                    .padding(.trailing, 6)
                }
                .scrollIndicators(.never)
                .onScrollGeometryChange(for: [Bool].self) { geometry in
                    [geometry.contentOffset.x > 0.5,
                     geometry.contentOffset.x + geometry.containerSize.width < geometry.contentSize.width - 0.5]
                } action: { _, edges in
                    cut = (edges[0], edges[1])
                }
                .mask {
                    HStack(spacing: 0) {
                        LinearGradient(colors: [.clear, .black], startPoint: .leading, endPoint: .trailing)
                            .frame(width: cut.leading ? 28 : 0)
                        Rectangle()
                        LinearGradient(colors: [.black, .clear], startPoint: .leading, endPoint: .trailing)
                            .frame(width: cut.trailing ? 28 : 0)
                    }
                }
                .onChange(of: files.last?.id, initial: true) { _, newest in
                    guard let newest else { return }
                    withAnimation(.easeOut(duration: 0.2)) { reader.scrollTo(newest, anchor: .trailing) }
                }
            }
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
            .overlay(alignment: .bottom) {
                if case .uploading(let sent) = file.state { Progress(fraction: Double(sent) / Double(max(file.size, 1))) }
            }
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
                mark
                VStack(alignment: .leading, spacing: 2) {
                    FileName(name: file.name).frame(height: 15)
                    Text(sizeLine).font(Theme.ui(11)).foregroundStyle(Theme.text3).lineLimit(1).frame(height: 14)
                }
            }
            .padding(.leading, 14)
            .padding(.trailing, 16)
        }
    }

    /// The file's mark in every state: dimmed with a red ! once refused, and with Retry over its folded corner once it
    /// failed.
    private var mark: some View {
        FileTypeMark(name: file.name)
            .opacity(isRefused ? 0.4 : 1)
            .overlay(alignment: .topTrailing) {
                switch file.state {
                case .failed:
                    Button(action: retry) {
                        Image(systemName: "arrow.clockwise").font(.system(size: 7.5, weight: .bold)).foregroundStyle(Theme.text)
                            .frame(width: 14, height: 14)
                            .background(Theme.sheet, in: Circle())
                            .overlay(Circle().strokeBorder(Color.white.opacity(0.18)))
                            .contentShape(Circle().inset(by: -4))
                    }
                    .buttonStyle(.hover(radius: 7, over: true, id: "session.composer.file.\(file.id).retry"))
                    .offset(x: 5, y: -4)
                    .help("Send it again")
                    .accessibilityLabel("Retry \(file.name)")
                    .accessibilityIdentifier("session.composer.file.\(file.id).retry")
                case .refused:
                    Image(systemName: "exclamationmark.circle.fill").font(.system(size: 12))
                        .foregroundStyle(Theme.red, Theme.sheet)
                        .offset(x: 5, y: -4)
                        .accessibilityHidden(true)
                default:
                    EmptyView()
                }
            }
    }

    /// Its size, or how far it has got on its way: "62% of 1.4 MB".
    private var sizeLine: String {
        guard case .uploading(let sent) = file.state else { return Attachment.sizeText(file.size) }
        return "\(Int(Double(sent) / Double(max(file.size, 1)) * 100))% of \(Attachment.sizeText(file.size))"
    }

    private var isRefused: Bool {
        if case .refused = file.state { return true }
        return false
    }

    private var trouble: Bool {
        switch file.state {
        case .refused, .failed: true
        default: false
        }
    }

    private var spoken: String {
        switch file.state {
        case .uploading: "\(file.name), sending, \(sizeLine)"
        case .arrived: "\(file.name), \(Attachment.sizeText(file.size))"
        case .refused(let reason), .failed(let reason): "\(file.name), \(reason)"
        }
    }
}

/// How far a file on its way has got: a 3 pt bar along the tile's foot, on a faint track.
private struct Progress: View {
    let fraction: Double

    var body: some View {
        GeometryReader { space in
            ZStack(alignment: .leading) {
                Rectangle().fill(Color.white.opacity(0.08))
                Rectangle().fill(Theme.text).frame(width: space.size.width * min(1, max(0, fraction)))
            }
        }
        .frame(height: 3)
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

/// A sent message's files, read back from its text (Kit's `AttachedFiles`): each a 40 pt tile of its type mark and
/// whole name; the bytes stay on the computer.
struct SentFiles: View {
    let paths: [String]

    var body: some View {
        // One row, sideways past the conversation's width, so every name stays whole.
        ScrollView(.horizontal) {
            row
        }
        .scrollIndicators(.never)
    }

    private var row: some View {
        HStack(spacing: 8) {
            ForEach(paths, id: \.self) { path in
                let name = (path as NSString).lastPathComponent
                HStack(spacing: 10) {
                    FileTypeMark(name: name, height: 20)
                    Text(name).font(Theme.ui(12, .medium)).foregroundStyle(Theme.text).lineLimit(1)
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
