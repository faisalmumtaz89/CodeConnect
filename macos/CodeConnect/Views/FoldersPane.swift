import CodeConnectKit
import SwiftUI

/// The words of the folder question and of Settings › Folders.
enum FolderWords {
    static let title = "Do you trust the authors of the files in this folder?"
    static func body(_ computer: String) -> String {
        "If you do, the agent loads the folder’s own scripts, tools and instructions, and its scripts can run on "
            + "\(computer) without asking you. If you don’t, the agent starts with nothing from the folder."
    }
    static let trust = "Yes, I trust the authors"
    static let trustDetail = "Load the folder’s scripts, tools and instructions"
    static let distrust = "No, I don’t trust the authors"
    static let distrustDetail = "Start with nothing from the folder"
    static let footnote = "Asked once for this folder. You can change it in Settings › Folders."
    static func then(_ agent: String, _ folder: String) -> String { "Then \(agent) starts in \(folder)." }

    static let paneTitle = "Folders"
    static let paneIntro =
        "Agents load the scripts, tools and instructions of folders you trust. In other folders they start with nothing from the folder."
    static let trusted = "Trusted"
    static let notTrusted = "Not trusted"
    static let forgetHelp = "Ask again the next time an agent starts here."
    static let empty = "No folders yet. CodeConnect asks the first time an agent starts in a folder."
    static func reading(_ computer: String) -> String { "Reading \(computer)’s folders…" }
    static func unreachable(_ computer: String) -> String {
        "\(computer)’s CodeConnect isn’t answering, so its folders can’t be shown."
    }
}

/// Settings › Folders: the answers to the folder question that each computer able to start an agent keeps, by path,
/// so a row stays where it is when its answer changes, with Trust / Don't Trust and Forget. Each computer is read now
/// and every five seconds while the pane is open, so an answer given in the start sheet meanwhile shows. A computer is
/// sent one request at a time: every reply is its whole list, and the replies to two requests sent together can arrive
/// in either order. A change that fails says so, then the list is read again; what it said stays until the pane opens
/// again or a change succeeds.
struct FoldersPane: View {
    @Environment(Machines.self) private var machines
    /// Each computer's answers, by its id, once read.
    @State private var lists: [String: [FolderAnswer]] = [:]
    @State private var unreadable: Set<String> = []
    @State private var busy: Set<String> = []
    @State private var refusal: String?

    var body: some View {
        let contexts = machines.startContexts
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                VStack(alignment: .leading, spacing: 6) {
                    Text(FolderWords.paneTitle).font(Theme.ui(19, .semibold)).foregroundStyle(Theme.text)
                    Text(FolderWords.paneIntro).font(Theme.ui(13)).foregroundStyle(Theme.text2)
                        .fixedSize(horizontal: false, vertical: true)
                }
                ForEach(contexts, id: \.id) { computer($0) }
                if let refusal {
                    Text(refusal).font(Theme.ui(12)).foregroundStyle(Theme.red).fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("folders.refusal")
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(24)
        }
        .frame(width: 560, height: 460)
        .background(Theme.sheet)
        .onAppear { refusal = nil }
        .task(id: contexts.map(\.id)) {
            while !Task.isCancelled {
                for context in contexts { await send(context) }
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    private func computer(_ context: StartContext) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(context.machineName).font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.text3)
            if let list = lists[context.id] {
                if list.isEmpty {
                    note(FolderWords.empty)
                } else {
                    VStack(spacing: 0) {
                        ForEach(Array(list.sorted { $0.path < $1.path }.enumerated()), id: \.element.path) { index, answer in
                            if index > 0 { Rectangle().fill(Theme.hairline).frame(height: 1) }
                            row(answer, context)
                        }
                    }
                    .background(Theme.code.opacity(0.5), in: RoundedRectangle(cornerRadius: 7))
                    .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
                    .disabled(busy.contains(context.id))
                }
            } else {
                note(unreadable.contains(context.id) ? FolderWords.unreachable(context.machineName) : FolderWords.reading(context.machineName))
            }
        }
    }

    private func row(_ answer: FolderAnswer, _ context: StartContext) -> some View {
        let trusted = answer.trust == .trusted
        let id = "folders.\(answer.path)"
        let name = URL(fileURLWithPath: answer.path).lastPathComponent
        return HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 2) {
                Text(name).font(Theme.ui(12.5, .medium))
                    .foregroundStyle(Theme.text).lineLimit(1)
                Text(answer.path).font(Theme.mono(11)).foregroundStyle(Theme.text3).lineLimit(1).truncationMode(.head)
            }
            Spacer(minLength: 8)
            Text(trusted ? FolderWords.trusted : FolderWords.notTrusted)
                .font(Theme.ui(11.5))
                .foregroundStyle(trusted ? Theme.text2 : Theme.amber)
                .fixedSize()
                .accessibilityIdentifier("\(id).state")
            // Each control names its folder, so a list of the window's buttons reads "Trust demo", never "Trust, Trust".
            ActionButton(title: trusted ? "Don’t Trust" : "Trust", key: nil, prominent: false, id: "\(id).change") {
                Task {
                    await send(context) { try await $0.setFolderTrust(path: answer.path, trust: trusted ? .notTrusted : .trusted) }
                }
            }
            .accessibilityLabel("\(trusted ? "Don’t Trust" : "Trust") \(name)")
            Button("Forget") {
                Task { await send(context) { try await $0.forgetFolderTrust(path: answer.path) } }
            }
            .buttonStyle(.hover(outset: 4, id: "\(id).forget"))
            .font(Theme.ui(12))
            .foregroundStyle(Theme.text2)
            .help(FolderWords.forgetHelp)
            .accessibilityLabel("Forget \(name)")
            .accessibilityIdentifier("\(id).forget")
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(id)
    }

    private func note(_ text: String) -> some View {
        Text(text).font(Theme.ui(12)).foregroundStyle(Theme.text3).fixedSize(horizontal: false, vertical: true)
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
