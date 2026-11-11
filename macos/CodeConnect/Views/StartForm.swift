import CodeConnectKit
import SwiftUI

/// Start an agent on one of the computers that can start one: a recent folder there (on this Mac, any folder, from
/// the standard folder panel), an optional first message, and, under it, the model, whose menu holds Claude Code's
/// and Codex's: choosing a model chooses the agent. What each choice offers and says is Kit's (`StartContext`), as on
/// the iPhone; the Mac shows it as one form. The computer defaults to the one used last, the model to the Default of
/// the agent last used in the folder, else of the first one there. Once sent, the form gives way to the start's
/// progress, and the sheet closes when the run is in the list.
struct StartForm: View {
    @Environment(Machines.self) private var machines
    @Environment(Setup.self) private var setup
    @Environment(\.dismiss) private var dismiss
    /// The run is in the list: the list shows Active.
    let started: () -> Void

    @State private var computerID: String?
    @State private var query = ""
    @State private var folderPath: String?
    @State private var chosen: StartModelChoice?
    @State private var menuOpen = false
    @State private var message = ""
    @State private var requestID: String?
    /// This Mac's daemon's id (`LocalDaemon.machineID`): the computer whose daemon said that id when it connected is
    /// this Mac. Nil without one, as in a test launch with no rig of its own.
    @State private var thisMac: String?

    private var contexts: [StartContext] { machines.startContexts }

    /// The chosen computer, else the one used last.
    private var context: StartContext? {
        contexts.first { $0.id == computerID }
            ?? contexts.max { ($0.lastUsed ?? .distantPast) < ($1.lastUsed ?? .distantPast) }
    }

    /// The selected folder: on this Mac any folder (a recent one when it is one), elsewhere a recent one.
    private var folder: StartProject? {
        guard let folderPath, let context else { return nil }
        return isThisMac(context) ? context.project(at: folderPath) : context.projects.first { $0.path == folderPath }
    }

    private func isThisMac(_ context: StartContext) -> Bool {
        thisMac != nil && machines.model(for: context.id)?.connection.helloAck?.machineID == thisMac
    }

    /// The chosen model while the computer still offers it, else Kit's Default for the folder.
    private var pick: StartModelChoice? {
        guard let context else { return nil }
        if let chosen, context.modelChoices.contains(chosen) { return chosen }
        return context.defaultChoice(for: folder)
    }

    /// Why Start can't be pressed yet, or nil when it can.
    private var blocked: String? {
        guard let context else { return "Choose a computer." }
        if StartAgent.allCases.contains(where: { context.models($0) == .checking }) { return context.checkingModels }
        guard pick != nil else { return context.noAgent }
        guard folder != nil else { return "Choose a folder." }
        return nil
    }

    var body: some View {
        Group {
            if let context, let requestID, let start = context.starter.starts[requestID] {
                StartProgress(
                    start: start,
                    shown: { uid in machines.models.first { $0.machineID == context.id }?.shown(for: uid) != nil },
                    again: { self.requestID = nil },
                    started: {
                        started()
                        dismiss()
                    })
            } else {
                form
            }
        }
        .task(id: contexts.map(\.id)) { contexts.forEach { $0.load() } }
        .task { thisMac = setup.installer.flatMap { LocalDaemon.machineID(root: $0.root) } }
        #if DEBUG
            .task {
                menuOpen = RenderWindow.opensModelMenu
                guard let path = RenderWindow.startFolder ?? RenderWindow.chosenFolder else { return }
                for _ in 0..<150 {
                    if context?.projects.contains(where: { $0.path == path }) == true {
                        folderPath = path
                        guard RenderWindow.startFolder != nil else { return }
                        chosen = context?.modelChoices.first { $0.agent == RenderWindow.startAgent && $0.isDefault }
                        start()
                        return
                    }
                    try? await Task.sleep(for: .milliseconds(200))
                }
            }
        #endif
        // A selection is the computer's it was made on: it goes when the form's computer changes, whether chosen or,
        // with none chosen, the one used last changing under it.
        .onChange(of: context?.id) {
            query = ""
            folderPath = nil
            chosen = nil
        }
    }

    private var form: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 10) {
                Text("Start an agent").font(Theme.ui(19, .semibold)).foregroundStyle(Theme.text)
                if let context {
                    computerRow(context)
                    folderRows(context)
                    composer(context)
                }
            }
            .padding(.horizontal, 18)
            .padding(.top, 18)
            Spacer(minLength: 0)
            SheetBar(
                action: "Start", actionID: "start.start", blocked: blocked != nil, perform: start, cancel: { dismiss() },
                cancelID: "start.cancel"
            ) {
                if let note = blocked ?? pick.map(StartContext.starts) {
                    Text(note).font(Theme.ui(11.5)).foregroundStyle(Theme.text3).lineLimit(1)
                        .accessibilityIdentifier("start.note")
                }
            }
        }
    }

    private func start() {
        guard blocked == nil, let context, let folder, let pick else { return }
        requestID = context.start(
            folder, agent: pick.agent, model: pick.model, prompt: message.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    // MARK: Computer

    private func computerRow(_ context: StartContext) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            label("Computer")
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                if contexts.count > 1 {
                    Picker("Computer", selection: Binding(get: { context.id }, set: { computerID = $0 })) {
                        ForEach(contexts, id: \.id) { Text($0.machineName).tag($0.id) }
                    }
                    .labelsHidden()
                    .pickerStyle(.menu)
                    .fixedSize()
                    .accessibilityIdentifier("start.computer")
                    // Its choices are otherwise only in a menu that opens on the screen.
                    .accessibilityChildren {
                        ForEach(contexts, id: \.id) { choice in
                            Button(choice.machineName) { computerID = choice.id }
                                .accessibilityIdentifier("start.computer.\(choice.id)")
                                .accessibilityAddTraits(choice.id == context.id ? .isSelected : [])
                        }
                    }
                } else {
                    Text(context.machineName).font(Theme.ui(13, .medium)).foregroundStyle(Theme.text)
                        .accessibilityIdentifier("start.computer")
                }
                AgeText(since: context.lastUsed) { Self.summary(context, now: $0) }
                    .font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
                    .accessibilityIdentifier("start.summary")
            }
        }
    }

    /// `12 recent folders · last used 4m ago`, as the iPhone says it.
    private static func summary(_ context: StartContext, now: Date) -> String {
        let count = context.projects.count
        let used = context.lastUsed.map { " · last used \(Format.age(since: $0, now: now)) ago" } ?? ""
        return "\(count) recent folder\(count == 1 ? "" : "s")\(used)"
    }

    // MARK: Folder

    @ViewBuilder private func folderRows(_ context: StartContext) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                label("Folder")
                Spacer(minLength: 8)
                if isThisMac(context) { chooseButton }
            }
            if context.starter.projects == nil {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Reading \(context.machineName)’s recent folders…").font(Theme.ui(12)).foregroundStyle(Theme.text2)
                }
            } else if context.projects.isEmpty {
                Text("No recent folders on \(context.machineName) yet.").font(Theme.ui(12)).foregroundStyle(Theme.text2)
                if let picked = picked(context) { pickedRow(picked) }
            } else {
                TextField("", text: $query, prompt: Text("Search recent folders").foregroundStyle(Theme.text3))
                    .accessibilityLabel("Search recent folders")
                    .accessibilityIdentifier("start.search")
                    .textFieldStyle(.plain)
                    .font(Theme.ui(12.5))
                    .padding(.horizontal, 10)
                    .frame(height: 28)
                    .background(Theme.code, in: RoundedRectangle(cornerRadius: 7))
                    .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
                ScrollView {
                    // Not lazy: every folder is in the accessibility tree, scrolled to or not.
                    VStack(spacing: 0) {
                        if let picked = picked(context) { pickedRow(picked) }
                        ForEach(startFilter(context.projects, query)) { project in folderRow(project) }
                    }
                }
                .frame(height: 170)
                .background(Theme.code.opacity(0.5), in: RoundedRectangle(cornerRadius: 7))
                .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
            }
        }
    }

    private func folderRow(_ project: StartProject) -> some View {
        Button {
            folderPath = project.path
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(project.name).font(Theme.ui(12.5, .medium)).foregroundStyle(Theme.text).lineLimit(1)
                    Spacer(minLength: 8)
                    AgeText(since: project.lastUsed) { startProjectMeta(project, now: $0) }
                        .font(Theme.ui(11)).foregroundStyle(Theme.text3).lineLimit(1)
                }
                Text(project.shownPath)
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.text3)
                    .lineLimit(1)
                    .truncationMode(.head)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 5)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(folderPath == project.path ? Theme.selected : .clear)
            .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 0, id: "start.folder.\(project.path)"))
        .accessibilityIdentifier("start.folder.\(project.path)")
        .accessibilityAddTraits(folderPath == project.path ? .isSelected : [])
    }

    /// Any folder on this Mac, from the standard folder panel.
    private var chooseButton: some View {
        Button(action: chooseFolder) {
            Text("Choose a folder\u{2026}").font(Theme.ui(11.5)).foregroundStyle(Theme.text2).contentShape(Rectangle())
        }
        .buttonStyle(.hover(outset: 4, id: "start.choose"))
        .accessibilityIdentifier("start.choose")
    }

    private func chooseFolder() {
        #if DEBUG
            if let path = RenderWindow.pickedFolder {
                choose(path)
                return
            }
        #endif
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Choose"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        choose(url.path)
    }

    /// A chosen folder is the selected one, as a recent row is; the search goes, so a recent one chosen is in view.
    private func choose(_ path: String) {
        query = ""
        folderPath = path
    }

    /// The selected folder when it was chosen and is not a recent one.
    private func picked(_ context: StartContext) -> StartProject? {
        guard let folder, !context.projects.contains(where: { $0.path == folder.path }) else { return nil }
        return folder
    }

    /// A chosen folder that is not a recent one: its name and its path as chosen, selected. It has no history to show.
    private func pickedRow(_ project: StartProject) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(project.name).font(Theme.ui(12.5, .medium)).foregroundStyle(Theme.text).lineLimit(1)
            Text(project.path).font(Theme.mono(11)).foregroundStyle(Theme.text3).lineLimit(1).truncationMode(.head)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 5)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.selected)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("start.picked")
        .accessibilityAddTraits(.isSelected)
    }

    // MARK: First message and model

    /// The first message, with the model under it: its menu opens upward, over the folders.
    private func composer(_ context: StartContext) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            TextField("", text: $message, prompt: Text("First message (optional)").foregroundStyle(Theme.text3), axis: .vertical)
                .accessibilityLabel("First message")
                .accessibilityIdentifier("start.message")
                .textFieldStyle(.plain)
                .font(Theme.ui(12.5))
                .lineLimit(2...5)
            ModelButton(context: context, pick: pick, open: $menuOpen) { chosen = $0 }
                .padding(.leading, -7)
        }
        .padding(.horizontal, 10)
        .padding(.top, 8)
        .padding(.bottom, 5)
        // Drawn behind: the menu sits over the composer's edge.
        .background {
            RoundedRectangle(cornerRadius: 7).fill(Theme.code)
                .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
        }
    }

    private func label(_ text: String) -> some View {
        Text(text).font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.text3)
    }
}

/// A sent start: its steps while the computer starts it, and Kit's words when it couldn't (the computer's own
/// words too, for a launch that failed). Started, it waits for the run to be in the list.
private struct StartProgress: View {
    let start: SessionStart
    let shown: (String) -> Bool
    let again: () -> Void
    let started: () -> Void
    @Environment(\.dismiss) private var dismiss
    /// The computer's own words for a launch that failed are shown.
    @State private var details = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if case .failed(let reason, let detail) = start.outcome {
                failed(reason, detail)
            } else {
                starting
            }
        }
        .onChange(of: liveUID) { _, uid in if uid != nil { started() } }
        .task {
            if liveUID != nil { started() }
        }
    }

    /// The new run's id once the list has it.
    private var liveUID: String? {
        guard case .started(let uid, _) = start.outcome, shown(uid) else { return nil }
        return uid
    }

    private var starting: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 14) {
                Text("Starting \(start.agent.displayName)…").font(Theme.ui(19, .semibold)).foregroundStyle(Theme.text)
                TimelineView(.periodic(from: start.sentAt, by: 1)) { clock in
                    let answered = start.outcome != nil
                    VStack(alignment: .leading, spacing: 8) {
                        step(
                            answered ? .done : .running, answered ? "\(start.machineName) started it" : "Asking \(start.machineName) to start it",
                            id: "start.step.asking")
                        step(
                            answered ? .running : .waiting,
                            "Waiting for \(start.agent.displayName)’s first words · "
                                + Format.age(clock.date.timeIntervalSince(start.sentAt)),
                            id: "start.step.waiting")
                    }
                }
                if let prompt = start.prompt {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Your first message").font(Theme.ui(11.5, .medium)).foregroundStyle(Theme.text3)
                        Text(prompt).font(Theme.ui(12.5)).foregroundStyle(Theme.text).fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
            .padding(.horizontal, 18)
            .padding(.top, 18)
            Spacer(minLength: 0)
            SheetBar(
                action: nil, actionID: "", blocked: true, perform: {}, cancel: { dismiss() }, cancelID: "start.close",
                cancelTitle: "Close"
            ) {
                Text("It keeps starting if you close this.").font(Theme.ui(11.5)).foregroundStyle(Theme.text3)
            }
        }
    }

    private enum Step { case done, running, waiting }

    /// A step's mark, as the iPhone's three: done, under way, not begun. Still marks: the clock beside the last
    /// step is what moves.
    private func step(_ state: Step, _ text: String, id: String) -> some View {
        HStack(spacing: 8) {
            Image(systemName: state == .done ? "checkmark.circle.fill" : state == .running ? "circle.dotted" : "circle.dashed")
                .font(.system(size: 12))
                .foregroundStyle(state == .waiting ? Theme.text3 : Theme.text2)
                .accessibilityLabel(state == .done ? "Done" : state == .running ? "Under way" : "Not begun")
                // A checkmark symbol reads as selected; the label says what it means.
                .accessibilityRemoveTraits(.isSelected)
                .accessibilityIdentifier("\(id).state")
            Text(text).font(Theme.ui(12.5)).foregroundStyle(state == .done ? Theme.text2 : Theme.text)
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(id)
    }

    private func said(_ detail: String) -> some View {
        Text(detail)
            .font(Theme.mono(11.5))
            .foregroundStyle(Theme.text)
            .textSelection(.enabled)
            .accessibilityIdentifier("start.failed.text")
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(10)
    }

    /// Kit's words: its title, and its sentence where it has one. A launch that failed has none;
    /// the computer's own words go under Details.
    private func failed(_ reason: StartFailure, _ detail: String) -> some View {
        let words = reason.words(for: start)
        return VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 10) {
                Text(words.title)
                    .font(Theme.ui(19, .semibold))
                    .foregroundStyle(Theme.text)
                    .fixedSize(horizontal: false, vertical: true)
                if let message = words.message {
                    Text(message).font(Theme.ui(13.5)).foregroundStyle(Theme.text2).fixedSize(horizontal: false, vertical: true)
                }
                if reason == .launchFailed, !detail.isEmpty {
                    Button {
                        details.toggle()
                    } label: {
                        HStack(spacing: 6) {
                            Image(systemName: details ? "chevron.down" : "chevron.right").font(.system(size: 9, weight: .semibold))
                            Text("Details").font(Theme.ui(12.5))
                        }
                        .foregroundStyle(Theme.text2)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.hover(outset: 5, id: "start.failed.details"))
                    .accessibilityIdentifier("start.failed.details")
                    .accessibilityValue(details ? "open" : "closed")
                    if details {
                        // As tall as the words, up to a box that scrolls.
                        ViewThatFits(in: .vertical) {
                            said(detail)
                            ScrollView { said(detail) }
                        }
                        .frame(maxHeight: 180, alignment: .top)
                        .fixedSize(horizontal: false, vertical: true)
                        .background(Theme.code, in: RoundedRectangle(cornerRadius: 7))
                        .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
                    }
                }
            }
            .padding(.horizontal, 18)
            .padding(.top, 18)
            Spacer(minLength: 0)
            SheetBar(
                action: words.action, actionID: "start.failed.again", blocked: false, perform: again, cancel: { dismiss() },
                cancelID: "start.close", cancelTitle: "Close"
            ) {
                EmptyView()
            }
        }
    }
}
