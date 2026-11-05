import SwiftUI

/// Where a start lands the moment it is sent: the new session's own screen,
/// saying what it is waiting for until the fleet lists the session, or why it
/// did not start and what to do about it.
struct StartingSessionView: View {
    let requestID: String
    /// The session is live: show its real screen, ready to type into when
    /// no first message was sent.
    let onLive: (_ key: String, _ focusComposer: Bool) -> Void
    /// Back to choosing, from a start that failed.
    let onChooseAgain: () -> Void

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    private var start: SessionStart? { model.starter.starts[requestID] }

    var body: some View {
        Group {
            if let start {
                if case .failed(let reason, let detail) = start.outcome {
                    failure(start, reason: reason, detail: detail)
                } else {
                    starting(start)
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(CC.color.bg)
        .ccNavigationChrome()
        .navigationTitle("")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            if let start {
                ToolbarItem(placement: .principal) { StartBarTitle(start: start) }
            }
        }
        .onChange(of: liveKey) { _, key in if let key { onLive(key, start?.prompt == nil) } }
    }

    /// The new session's key once the fleet lists it.
    private var liveKey: String? {
        guard case .started(let uid, _) = start?.outcome ?? .failed(.launchFailed, detail: ""),
            model.shown(for: uid) != nil
        else { return nil }
        return uid
    }

    // MARK: Starting

    /// The screen's title grows with the steps under it, as `CCEmptyState`'s
    /// does: capped, the heading measured smaller than its own steps at AX5.
    private static var titleStyle: CCTextStyle {
        var style = CC.type.title
        style.maxSize = nil
        return style
    }

    /// The session screen's own two columns (`TimelineSpine`), since this is
    /// that screen before its first event: every line of text on 52, the
    /// steps' marks centred on the one mark axis, 36.
    private func starting(_ start: SessionStart) -> some View {
        let answered = start.outcome != nil
        return ScrollView {
            VStack(alignment: .leading, spacing: CC.space.lg) {
                Text("Starting \(start.agent.displayName)…")
                    .ccType(Self.titleStyle)
                    .foregroundStyle(CC.text.primary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, TimelineSpine.content)
                TimelineView(.periodic(from: start.sentAt, by: 1)) { context in
                    VStack(alignment: .leading, spacing: CC.space.sm) {
                        step(
                            answered ? .done : .running,
                            answered
                                ? "\(start.machineName) started it"
                                : "Asking \(start.machineName) to start it")
                        step(
                            answered ? .running : .waiting,
                            "Waiting for \(start.agent.displayName)’s first words · "
                                + Format.age(context.date.timeIntervalSince(start.sentAt)))
                    }
                }
                if let prompt = start.prompt {
                    VStack(alignment: .leading, spacing: CC.space.xs) {
                        Text("YOUR FIRST MESSAGE")
                            .ccType(CC.type.fieldLabel, color: nil)
                        Text(prompt)
                            .ccType(CC.type.reading)
                            .foregroundStyle(CC.text.primary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(.leading, TimelineSpine.content)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, CC.space.md)
            .padding(.top, CC.space.xl)
        }
    }

    private enum StepState { case done, running, waiting }

    /// The ring's own size, which does not grow: grown, the icons beside it
    /// measured 31pt at AX5 and left the mark axis for the gutter's edge.
    private static let markSize = CCProgressRing.Size.sm.rawValue

    /// A step: its mark in the gutter, centred on the mark axis beside the
    /// first line, and its words on the text column.
    private func step(_ state: StepState, _ text: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: TimelineSpine.gap) {
            Group {
                switch state {
                case .done:
                    CCIcon("check-circle-fill", size: Self.markSize, maxSize: Self.markSize)
                        .foregroundStyle(CC.color.success)
                case .running:
                    CCProgressRing()
                case .waiting:
                    CCIcon("circle-dashed", size: Self.markSize, maxSize: Self.markSize)
                        .foregroundStyle(CC.text.tertiary)
                }
            }
            .ccMark(beside: CC.type.body)
            .ccGutterMark()
            Text(text)
                .ccType(CC.type.body)
                .foregroundStyle(state == .done ? CC.text.secondary : CC.text.primary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    // MARK: Failed

    private func failure(_ start: SessionStart, reason: StartFailure, detail: String) -> some View {
        let copy = Self.copy(for: reason, start: start)
        return ScrollView {
            CCEmptyState(
                glyph: copy.glyph, title: copy.title, message: copy.message, tone: .danger,
                actionTitle: copy.action, action: onChooseAgain,
                secondaryActionTitle: "Back to Projects", secondaryAction: { dismiss() }
            ) {
                VStack(alignment: .leading, spacing: CC.space.xxs) {
                    Text("\(start.machineName.uppercased()) SAID")
                        .ccType(CC.type.fieldLabel, color: nil)
                    CCMonoBlock(detail)
                }
                .frame(maxWidth: 320)
                .padding(.top, CC.space.xs)
            }
            .padding(.top, CC.space.xxl)
        }
    }

    private static func copy(for reason: StartFailure, start: SessionStart)
        -> (glyph: String, title: String, message: String, action: String)
    {
        let agent = start.agent.displayName
        switch reason {
        case .folderMissing:
            return (
                "folder-dashed", "That folder isn’t there any more",
                "It was moved, renamed or deleted on \(start.machineName). Nothing was started.",
                "Start again")
        case .agentMissing:
            return (
                "terminal-window", "\(agent) isn’t installed",
                "\(start.machineName) has no \(agent). Install it there, then start again from here.",
                "Start again")
        case .launchFailed:
            return (
                "warning", "\(agent) didn’t start",
                "\(start.machineName) could not start it. Its own words are below.",
                "Start again")
        }
    }
}
