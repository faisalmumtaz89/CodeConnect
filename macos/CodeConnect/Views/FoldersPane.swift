import CodeConnectKit
import SwiftUI

/// Settings › Folders: the answers to the folder question that each computer able to start an agent keeps, by path,
/// so a row stays where it is when its answer changes, with Trusted | Not trusted and Forget. Each computer is read now
/// and every five seconds while the pane is open, so an answer given in the start sheet meanwhile shows. A computer is
/// sent one request at a time: every reply is its whole list, and the replies to two requests sent together can arrive
/// in either order. A change that fails says so, then the list is read again; what it said stays until the pane opens
/// again or a change succeeds. A paired computer whose link is down, or not yet connected, has not said whether it can
/// start an agent: it stays listed, and while it can't be read it shows the sentence that says so.
struct FoldersPane: View {
    @Environment(Machines.self) private var machines
    /// Each computer's answers, by its id, once read.
    @State private var lists: [String: [FolderAnswer]] = [:]
    @State private var unreadable: Set<String> = []
    @State private var busy: Set<String> = []
    @State private var refusal: String?

    var body: some View {
        let contexts = machines.models
            .filter { $0.machineID != nil && ($0.connection.capabilities?.startsSessions ?? true) }
            .map(StartContext.init(model:))
        Group {
            VStack(alignment: .leading, spacing: 18) {
                // The title as every section's, 24 pt above the first card.
                VStack(alignment: .leading, spacing: 6) {
                    Text(FolderWords.paneTitle).font(Theme.ui(15, .semibold)).foregroundStyle(Theme.text)
                    Text(FolderWords.paneIntro).font(Theme.ui(13)).foregroundStyle(Theme.text2)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(.bottom, 6)
                ForEach(contexts, id: \.id) { computer($0) }
                Text(FolderWords.forgetHelp).font(Theme.ui(12)).foregroundStyle(Theme.text3)
                    .fixedSize(horizontal: false, vertical: true)
                if let refusal {
                    Text(refusal).font(Theme.ui(12)).foregroundStyle(Theme.red).fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("folders.refusal")
                }
            }
        }
        #if DEBUG
            .background(RenderWindow(ready: contexts.allSatisfy { lists[$0.id] != nil || unreadable.contains($0.id) }))
        #endif
        .onAppear { refusal = nil }
        .task(id: contexts.map(\.id)) {
            #if DEBUG
                if let shown = RenderWindow.folders {
                    for context in contexts { lists[context.id] = shown }
                    return
                }
            #endif
            while !Task.isCancelled {
                for context in contexts { await send(context) }
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    private func computer(_ context: StartContext) -> some View {
        SettingsGroup(title: context.machineName) {
            if let list = lists[context.id] {
                if list.isEmpty {
                    note(FolderWords.empty)
                } else {
                    ForEach(list.sorted { $0.path < $1.path }, id: \.path) { row($0, context) }
                }
            } else {
                note(unreadable.contains(context.id) ? FolderWords.unreachable(context.machineName) : FolderWords.reading(context.machineName))
            }
        }
        .disabled(busy.contains(context.id))
    }

    /// The folder, its answer as one of two segments, and Forget. Each control names its folder, so a list of the
    /// window's buttons reads "Trusted demo", never "Trusted, Trusted".
    private func row(_ answer: FolderAnswer, _ context: StartContext) -> some View {
        let id = "folders.\(answer.path)"
        let name = URL(fileURLWithPath: answer.path).lastPathComponent
        return HStack(spacing: 10) {
            Image(systemName: "folder").font(.system(size: 12)).foregroundStyle(Theme.text2)
                .accessibilityHidden(true)
            Text(context.shown(answer.path)).font(Theme.mono(12.5)).foregroundStyle(Theme.text).lineLimit(1).truncationMode(.head)
            Spacer(minLength: 8)
            SegmentedTrack(
                options: [FolderTrust.trusted, .notTrusted], chosen: answer.trust, fill: Theme.window,
                id: { "\(id).\($0 == .trusted ? "trusted" : "notTrusted")" },
                choose: { trust in
                    Task { await send(context) { try await $0.setFolderTrust(path: answer.path, trust: trust) } }
                }
            ) { trust, chosen in
                Text(trust == .trusted ? FolderWords.trusted : FolderWords.notTrusted)
                    .font(Theme.ui(12, chosen ? .medium : .regular))
                    .foregroundStyle(chosen ? Theme.text : Theme.text3)
                    .padding(.horizontal, 10)
                    .frame(height: 24)
                    .accessibilityLabel("\(trust == .trusted ? FolderWords.trusted : FolderWords.notTrusted) \(name)")
            }
            .fixedSize()
            ActionButton(title: FolderWords.forget, key: nil, prominent: false, id: "\(id).forget") {
                Task { await send(context) { try await $0.forgetFolderTrust(path: answer.path) } }
            }
            .accessibilityLabel("\(FolderWords.forget) \(name)")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(id)
    }

    private func note(_ text: String) -> some View {
        Text(text).font(Theme.ui(12)).foregroundStyle(Theme.text3).fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 14)
            .padding(.vertical, 12)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// One request to the computer, a change or else a read; a change that fails is followed by a read.
    private func send(_ context: StartContext, _ change: ((DaemonConnection) async throws -> [FolderAnswer])? = nil) async {
        guard let connection = machines.model(for: context.id)?.connection, !busy.contains(context.id) else { return }
        busy.insert(context.id)
        defer { busy.remove(context.id) }
        if let change {
            do {
                lists[context.id] = try await change(connection)
                refusal = nil
                return
            } catch {
                refusal = error.localizedDescription
            }
        }
        do {
            lists[context.id] = try await connection.listFolderTrust()
            unreadable.remove(context.id)
        } catch {
            lists[context.id] = nil
            unreadable.insert(context.id)
        }
    }
}
