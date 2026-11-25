import CodeConnectKit
import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

/// **`+` at the reply pill's leading edge**: the system's menu of where a file comes from — Photos, the camera,
/// Files. What is picked goes to Kit's `attach`, which applies the limits and starts the upload at once; the tile
/// over the pill shows the rest.
struct AttachMenu: View {
    let key: String
    /// How many more files this message can take.
    let room: Int

    @Environment(AppModel.self) private var model
    @State private var showPhotos = false
    @State private var picked: [PhotosPickerItem] = []
    @State private var showCamera = false
    @State private var showFiles = false

    var body: some View {
        Menu {
            Button { showPhotos = true } label: { Label("Photos", systemImage: "photo.on.rectangle") }
            if UIImagePickerController.isSourceTypeAvailable(.camera) {
                Button { showCamera = true } label: { Label("Camera", systemImage: "camera") }
            }
            Button { showFiles = true } label: { Label("Choose File", systemImage: "folder") }
        } label: {
            // As large as the pill's own circle lets it grow: the field's words start past it.
            CCIcon("plus", size: 18, weight: .semibold, maxSize: 22)
                .foregroundStyle(CC.text.secondary)
                .frame(width: CC.size.controlSm, height: CC.size.controlSm)
                .ccHitTarget()
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .accessibilityLabel("Attach")
        .accessibilityIdentifier("attach.menu")
        .photosPicker(
            isPresented: $showPhotos, selection: $picked, maxSelectionCount: max(room, 1),
            selectionBehavior: .ordered, matching: .images)
        .onChange(of: picked) { _, items in
            guard !items.isEmpty else { return }
            picked = []
            Task { for item in items { await addPhoto(item) } }
        }
        .fullScreenCover(isPresented: $showCamera) {
            CameraCapture { image in
                if let jpeg = image.jpegData(compressionQuality: 0.85) {
                    model.attach(data: jpeg, name: "photo.jpg", to: key)
                }
            }
            .ignoresSafeArea()
        }
        .fileImporter(isPresented: $showFiles, allowedContentTypes: [.item], allowsMultipleSelection: true) { result in
            guard case .success(let urls) = result else { return }
            for url in urls { addFile(url) }
        }
    }

    private func addPhoto(_ item: PhotosPickerItem) async {
        guard let data = try? await item.loadTransferable(type: Data.self),
              let photo = PhotoExport.prepare(data, types: item.supportedContentTypes)
        else { return }
        model.attach(data: photo.data, name: photo.name, to: key)
    }

    /// A file from Files, read only when Kit finds it within the limits: a larger one is a refused tile, its bytes
    /// never read.
    private func addFile(_ url: URL) {
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        let size = (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0
        model.attach(name: url.lastPathComponent, size: size, to: key) { try Data(contentsOf: url) }
    }
}

/// **A photo as it goes to the agent**: PNG, GIF and JPEG as the library holds them; anything else (HEIC, which an
/// agent's tools may not read) as JPEG. Nil when the bytes are no image.
enum PhotoExport {
    static func prepare(_ data: Data, types: [UTType]) -> (data: Data, name: String)? {
        if types.contains(where: { $0.conforms(to: .png) }) { return (data, "image.png") }
        if types.contains(where: { $0.conforms(to: .gif) }) { return (data, "image.gif") }
        if types.contains(where: { $0.conforms(to: .jpeg) }) { return (data, "photo.jpg") }
        guard let jpeg = UIImage(data: data)?.jpegData(compressionQuality: 0.85) else { return nil }
        return (jpeg, "photo.jpg")
    }
}

/// The system camera, for one photo.
private struct CameraCapture: UIViewControllerRepresentable {
    let onPhoto: (UIImage) -> Void
    @Environment(\.dismiss) private var dismiss

    func makeUIViewController(context: Context) -> UIImagePickerController {
        let picker = UIImagePickerController()
        picker.sourceType = .camera
        picker.delegate = context.coordinator
        return picker
    }

    func updateUIViewController(_ picker: UIImagePickerController, context: Context) {}

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    final class Coordinator: NSObject, UIImagePickerControllerDelegate, UINavigationControllerDelegate {
        let parent: CameraCapture
        init(_ parent: CameraCapture) { self.parent = parent }

        func imagePickerController(
            _ picker: UIImagePickerController, didFinishPickingMediaWithInfo info: [UIImagePickerController.InfoKey: Any]
        ) {
            if let image = info[.originalImage] as? UIImage { parent.onPhoto(image) }
            parent.dismiss()
        }

        func imagePickerControllerDidCancel(_ picker: UIImagePickerController) { parent.dismiss() }
    }
}

/// **What goes with the next reply**, 56-pt tiles over the pill, in order. Each says where it is — on its way (a
/// ring), arrived (plain), refused before sending, or failed on the way (send it again) — and each has its ×.
/// Why they hold the message back is said once, in Kit's words, under the tiles.
struct AttachmentTray: View {
    let files: [Attachment]
    /// Kit's `filesBlockedReason`.
    let reason: String?
    let onRemove: (String) -> Void
    let onRetry: (String) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: CC.space.sm) {
                    ForEach(files) { file in
                        AttachmentTile(file: file, onRetry: { onRetry(file.id) })
                            .overlay(alignment: .topTrailing) { removeButton(file) }
                    }
                }
                .padding(.top, 6)
                .padding(.trailing, 6)
            }
            if let reason {
                Text(reason)
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.color.danger)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("attach.problem")
            }
        }
    }

    private func removeButton(_ file: Attachment) -> some View {
        Button { onRemove(file.id) } label: {
            CCIcon("x", size: 9, weight: .bold, maxSize: 11)
                .foregroundStyle(CC.text.primary)
                .frame(width: 20, height: 20)
                .background(CC.color.surfaceOverlay, in: Circle())
                .overlay { Circle().strokeBorder(CC.color.bg, lineWidth: 2) }
                .ccHitTarget(minWidth: 32, minHeight: 32)
        }
        .buttonStyle(.plain)
        .offset(x: 8, y: -8)
        .accessibilityLabel("Remove \(file.name)")
        .accessibilityIdentifier("attach.remove")
    }
}

/// One file: its picture or its name, and its state over it.
private struct AttachmentTile: View {
    let file: Attachment
    let onRetry: () -> Void
    @State private var thumbnail: UIImage?

    private static let side: CGFloat = 56
    private var shape: RoundedRectangle { RoundedRectangle(cornerRadius: CC.radius.lg, style: .continuous) }

    var body: some View {
        face
            .frame(width: Self.side, height: Self.side)
            .clipShape(shape)
            .overlay { state }
            .overlay { shape.strokeBorder(edge, lineWidth: troubled ? 1.5 : CC.stroke.hairline) }
            .task(id: file.id) {
                guard file.isImage, thumbnail == nil, let data = file.data else { return }
                thumbnail = await UIImage(data: data)?
                    .byPreparingThumbnail(ofSize: CGSize(width: Self.side * 3, height: Self.side * 3))
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel("\(file.name), \(spoken)")
            .accessibilityIdentifier("attach.tile")
    }

    @ViewBuilder
    private var face: some View {
        if let thumbnail {
            Image(uiImage: thumbnail).resizable().scaledToFill()
        } else {
            VStack(spacing: 4) {
                // While it travels, or once it failed, the ring or Retry takes the glyph's place.
                if case .uploading = file.state {
                    Color.clear.frame(width: 22, height: 22)
                } else if case .failed = file.state {
                    Color.clear.frame(width: 22, height: 22)
                } else {
                Group {
                    if file.isImage {
                        Image(systemName: "photo").font(.system(size: 18))
                    } else {
                        CCIcon("file-text", size: 20, weight: .regular)
                    }
                }
                .foregroundStyle(CC.text.secondary)
                }
                Text(verbatim: file.name)
                    .font(.system(size: 10))
                    .foregroundStyle(CC.text.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .padding(.horizontal, 4)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(CC.color.surfaceRaised)
        }
    }

    @ViewBuilder
    private var state: some View {
        switch file.state {
        case .uploading(let sent):
            ZStack {
                Color.black.opacity(0.45)
                    .opacity(thumbnail == nil ? 0 : 1)
                Circle().stroke(.white.opacity(0.25), lineWidth: 2.5).frame(width: 22, height: 22)
                Circle()
                    .trim(from: 0, to: file.size > 0 ? CGFloat(sent) / CGFloat(file.size) : 0)
                    .stroke(.white, style: StrokeStyle(lineWidth: 2.5, lineCap: .round))
                    .rotationEffect(.degrees(-90))
                    .frame(width: 22, height: 22)
            }
            .padding(.bottom, thumbnail == nil ? 16 : 0)
            .clipShape(shape)
        case .failed:
            Button(action: onRetry) {
                Image(systemName: "arrow.clockwise").font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(CC.text.primary)
                    .frame(width: 30, height: 30)
                    .background(CC.color.surfaceOverlay, in: Circle())
                    .padding(.bottom, thumbnail == nil ? 16 : 0)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Send \(file.name) again")
            .accessibilityIdentifier("attach.retry")
        case .refused, .arrived:
            EmptyView()
        }
    }

    private var troubled: Bool {
        switch file.state {
        case .refused, .failed: true
        default: false
        }
    }

    private var edge: Color { troubled ? CC.color.danger : CC.color.border }

    private var spoken: String {
        switch file.state {
        case .uploading(let sent):
            "sending, \(file.size > 0 ? sent * 100 / file.size : 0) percent"
        case .arrived: "ready"
        case .refused(let reason): "not sent: \(reason)"
        case .failed(let reason): "failed: \(reason)"
        }
    }
}

/// **Files a sent message carried**, under its words: a tile each, by name. The phone never had the bytes of what
/// another device sent, and keeps none once sent, so no picture.
struct SentAttachments: View {
    let paths: [String]

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: CC.space.xs) {
            ForEach(Array(paths.enumerated()), id: \.offset) { _, path in
                let name = (path as NSString).lastPathComponent
                HStack(spacing: 6) {
                    Group {
                        if Self.looksLikeImage(name) {
                            Image(systemName: "photo").font(.system(size: 12))
                        } else {
                            CCIcon("file-text", size: 14, weight: .regular)
                        }
                    }
                    .foregroundStyle(CC.text.secondary)
                    Text(verbatim: name)
                        .ccType(CC.type.monoSmall)
                        .foregroundStyle(CC.text.primary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                .padding(.horizontal, CC.space.sm)
                .frame(height: 32)
                .background(CC.color.surface, in: RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous))
                .overlay {
                    RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous)
                        .strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline)
                }
                .accessibilityLabel("Attached \(name)")
            }
            }
        }
        .accessibilityIdentifier("attach.sent")
    }

    private static func looksLikeImage(_ name: String) -> Bool {
        UTType(filenameExtension: (name as NSString).pathExtension)?.conforms(to: .image) == true
    }
}
