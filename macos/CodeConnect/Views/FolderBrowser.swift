import CodeConnectKit
import SwiftUI

/// Any folder on a computer that is not this Mac, found as in a terminal, in the start form's folder menu: the
/// folder's path, ".." and the folders in it. A click chooses a row, a double-click or Return goes into it, ← or ⌫ goes
/// up; Use This Folder starts in the folder shown, Recent Folders goes back to the recent ones. New Folder… names one
/// in the app's own card and goes into it. Show hidden folders sits beside the path it changes, so the bar's three
/// buttons keep their whole words in the menu's width. What it says is the Kit's, as on the iPhone's browse step.
struct FolderBrowser: View {
    let context: StartContext
    let use: (String) -> Void
    /// Back to the recent folders, with nothing chosen.
    let recent: () -> Void
    /// The list's one height (`StartForm`'s, as its recent list's), and the height it was drawn at, for the form's room.
    let listHeight: CGFloat
    @Binding var listShown: CGFloat

    /// The folder shown; nil is the computer's home, until it names it.
    @State private var path: String?
    @State private var showsHidden = false
    /// The row the keys act on, by the folder it opens.
    @State private var selected: String?
    @State private var naming = false
    @State private var folderName = ""
    @State private var creating: String?
    @State private var creatingIn = ""
    @State private var refusal: String?
    @FocusState private var focused: Bool

    init(
        context: StartContext, use: @escaping (String) -> Void, recent: @escaping () -> Void, listHeight: CGFloat,
        listShown: Binding<CGFloat>
    ) {
        self.context = context
        self.use = use
        self.recent = recent
        self.listHeight = listHeight
        _listShown = listShown
        #if DEBUG
            _path = State(initialValue: RenderWindow.browsed.flatMap { $0 == "~" ? nil : $0 })
            _showsHidden = State(initialValue: RenderWindow.browsesHidden)
        #endif
    }

    private var request: StartFolderRequest { StartFolderRequest(path: path, showHidden: showsHidden) }
    private var listing: StartFolderListing? { context.listing(request) }

    /// The rows as shown, by the folder each opens: ".." first, then the folders in name order.
    private func targets(_ listing: StartFolderListing) -> [(name: String, path: String)] {
        let up = listing.path == "/" ? [] : [(name: "..", path: StartContext.parentFolder(listing.path))]
        let folders = listing.folders.sorted { $0.localizedStandardCompare($1) == .orderedAscending }
        return up + folders.map { (name: $0, path: StartContext.childFolder(listing.path, $0)) }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                // As a shell prints it, "~/models" (screen 33).
                Text(verbatim: (listing?.path ?? path).map(StartProject.shown) ?? " ")
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.text2)
                    .lineLimit(1)
                    .truncationMode(.head)
                    .accessibilityIdentifier("start.browse.path")
                Spacer(minLength: 8)
                Toggle(StartWords.showHiddenFolders, isOn: $showsHidden)
                    .toggleStyle(.checkbox)
                    .font(Theme.ui(11.5))
                    .foregroundStyle(Theme.text2)
                    .fixedSize()
                    .accessibilityIdentifier("start.browse.hidden")
            }
            ScrollViewReader { list in
                ScrollView {
                    VStack(spacing: 0) { rows }
                }
                .task(id: selected) { list.scrollTo(selected) }
            }
            // One height whatever the folder holds, so the panel stays still while you browse: 170 pt, giving way first
            // in a short window. A short folder's rows sit at its top. An exact height: the panel is offered only its
            // chip's height, and a range would collapse the list to its least.
            .frame(height: listHeight)
            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { listShown = $0 }
            .focusable()
            .focusEffectDisabled()
            .focused($focused)
            .onAppear { focused = true }
            .onKeyPress(.downArrow) { step(down: true) }
            .onKeyPress(.upArrow) { step(down: false) }
            .onKeyPress(.return) { open(selected) }
            .onKeyPress(.leftArrow) { up() }
            // ⌫ sends DEL (U+007F) on a Mac, not SwiftUI's `.delete` (U+0008).
            .onKeyPress(KeyEquivalent("\u{7F}")) { up() }
            bar
        }
        .task(id: request) { context.listFolder(request) }
        #if DEBUG
            .task {
                guard let name = RenderWindow.newFolderName else { return }
                for _ in 0..<150 where listing == nil { try? await Task.sleep(for: .milliseconds(200)) }
                folderName = name
                naming = true
            }
        #endif
        // Made, it opens, as `mkdir` then `cd` would; refused, it says why.
        .onChange(of: context.created(creating)) { _, outcome in
            guard let outcome else { return }
            creating = nil
            if let outcome { refusal = outcome } else { go(StartContext.childFolder(creatingIn, folderName)) }
        }
        .confirmation(
            Binding(get: { naming ? true : nil }, set: { naming = $0 ?? false }), returnsKeyboard: { focused = true }
        ) { _ in
            Confirmation(
                title: StartWords.newFolder, message: StartWords.newFolderIn(listing?.path ?? ""),
                answers: [
                    .init(title: "Cancel", kind: .cancel, id: "folder.new.cancel"),
                    .init(title: "Create", kind: .plain, id: "folder.new.create") {
                        guard let parent = listing?.path else { return }
                        creatingIn = parent
                        creating = context.createFolder(named: folderName, in: parent)
                    },
                ],
                field: .init(placeholder: StartWords.folderName, text: $folderName))
        }
        .confirmation($refusal, returnsKeyboard: { focused = true }) { refusal in
            Confirmation(
                title: StartWords.couldNotMake(folderName), message: refusal,
                answers: [.init(title: "OK", kind: .ok, id: "folder.new.ok")])
        }
    }

    @ViewBuilder private var rows: some View {
        if let listing {
            ForEach(targets(listing), id: \.path) { row(name: $0.name, opens: $0.path) }
            if let note = note(listing) {
                Text(note).font(Theme.ui(12)).foregroundStyle(Theme.text2)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 6)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("start.browse.note")
            }
        } else {
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text(StartWords.readingFolder(path, on: context.machineName)).font(Theme.ui(12))
                    .foregroundStyle(Theme.text2)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
            .accessibilityIdentifier("start.browse.reading")
        }
    }

    /// A folder by its name, its mark before it (".." has none), lit while the keys are on it: a click or the pointer
    /// puts them there, and the list has that one fill (screen 33). It chooses nothing: Use This Folder takes the folder
    /// shown.
    private func row(name: String, opens target: String) -> some View {
        Button {
            selected = target
            focused = true
        } label: {
            MenuRow {
                if name != ".." {
                    Image(systemName: "folder").font(.system(size: 11)).foregroundStyle(Theme.text2)
                        .accessibilityHidden(true)
                }
                Text(verbatim: name).font(Theme.ui(12.5)).foregroundStyle(Theme.text).lineLimit(1)
                    .truncationMode(.middle)
            }
            .environment(\.menuRowLit, selected == target)
        }
        .buttonStyle(.plain)
        .onHover { if $0 { selected = target } }
        .simultaneousGesture(TapGesture(count: 2).onEnded { go(target) })
        .focusable(false)
        .accessibilityIdentifier("start.browse.folder.\(name)")
        .accessibilityAddTraits(selected == target ? .isSelected : [])
        .id(target)
    }

    /// As the Mac's own folder panel lays its bar out: New Folder… at the leading edge, and the two ways out at the
    /// trailing edge, back to the recent folders or into the folder shown. Show hidden folders sits by the path above.
    private var bar: some View {
        let readable = listing.map { $0.error == nil } ?? false
        return HStack(spacing: 8) {
            ActionButton(title: StartWords.newFolderAction, key: nil, prominent: false, id: "start.browse.new") {
                folderName = ""
                naming = true
            }
            .disabled(!readable)
            Spacer(minLength: 8)
            ActionButton(title: "Recent Folders", key: nil, prominent: false, id: "start.browse.recent", action: recent)
            ActionButton(title: StartWords.useThisFolder, key: nil, prominent: true, id: "start.browse.use") {
                if let listing { use(listing.path) }
            }
            .disabled(!readable)
        }
    }

    /// What the list cannot show by itself: the computer's refusal, an empty folder, or more folders than it sends.
    private func note(_ listing: StartFolderListing) -> String? {
        if let error = listing.error { return error }
        if listing.truncated { return StartWords.firstFolders }
        if listing.folders.isEmpty { return StartWords.noFolders(showingHidden: showsHidden) }
        return nil
    }

    private func go(_ target: String) {
        path = target
        selected = nil
    }

    private func open(_ target: String?) -> KeyPress.Result {
        guard let target else { return .ignored }
        go(target)
        return .handled
    }

    private func up() -> KeyPress.Result {
        guard let listing, listing.path != "/" else { return .ignored }
        let from = listing.path
        go(StartContext.parentFolder(from))
        selected = from
        return .handled
    }

    private func step(down: Bool) -> KeyPress.Result {
        guard let listing else { return .ignored }
        selected = ListStep.step(from: selected, in: targets(listing).map(\.path), down: down)
        return .handled
    }
}
