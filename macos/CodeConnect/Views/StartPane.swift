import CodeConnectKit
import SwiftUI

/// The window's main area while no run is open: where an agent is started. The start form once a computer can start
/// one, or while a start this Mac sent has no answer yet, which it opens on; before that, why none can (none paired,
/// none connected, or a computer's CodeConnect too old to start one from here). New Agent comes here.
struct StartPane: View {
    @Environment(Machines.self) private var machines
    /// The window's start, which the form draws.
    let draft: StartDraft
    let addComputer: () -> Void
    /// The run just started is in the list, by its id.
    let started: (String) -> Void
    /// The form has been shown. It stays when its computer's link drops, which empties the computers that can start,
    /// so what was typed and chosen is there when the link is back.
    @State private var formShown = false
    @State private var height: CGFloat = 0

    var body: some View {
        VStack(spacing: 0) {
            // The form, and each page of a sent start, stands at the middle of the area: equal room above and below.
            Spacer(minLength: 16)
            Group {
                if formShown || !machines.startContexts.isEmpty || machines.outstandingStart != nil {
                    StartForm(started: started, addComputer: addComputer, whyNone: why, draft: draft)
                        .onAppear { formShown = true }
                } else {
                    noComputer
                }
            }
            .padding(.horizontal, 32)
            Spacer(minLength: 16)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        // One rounded panel inset from the window's edges, clear of the window's controls along its top.
        .background(DottedFade().clipShape(RoundedRectangle(cornerRadius: 12)))
        .background(Theme.window, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Theme.hairline))
        .coordinateSpace(.named(StartForm.space))
        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { height = $0 }
        .padding(.horizontal, 10)
        .padding(.top, 44)
        .padding(.bottom, 10)
        .background(Theme.pane)
        .environment(\.startHeight, height)
        // A start this Mac sent with no answer yet, from before the app last quit: the form opens on it.
        .onAppear {
            guard let start = machines.outstandingStart,
                let owner = machines.models.first(where: { $0.starter.starts[start.requestID] != nil })
            else { return }
            draft.adopt(
                start.requestID,
                asking: StartDraft.Ask(
                    computerID: StartContext(model: owner).id, folderPath: start.project.path, place: start.place,
                    agent: start.agent, model: start.model, message: start.prompt ?? ""))
        }
        .environment(\.sheetRoom, height)
    }

    private var noComputer: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(FleetWords.startAnAgent).font(Theme.ui(19, .semibold)).foregroundStyle(Theme.text)
            Text(why)
                .font(Theme.ui(13.5))
                .foregroundStyle(Theme.text2)
                .lineSpacing(3)
                .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: 8) {
                if let refused {
                    ActionButton(title: "Try Again", key: nil, prominent: true, id: "agent.tryAgain") {
                        refused.connection.retryNow()
                    }
                }
                ActionButton(title: AddKind.computer.title, key: nil, prominent: false, id: "agent.addComputer", action: addComputer)
            }
            .padding(.top, 6)
        }
        .frame(maxWidth: StartForm.column, alignment: .leading)
    }

    /// A computer refused for running a different version of CodeConnect, which only updating cures.
    private var refused: AppModel? { machines.startRefusedComputer }

    /// Why no computer can start an agent, in the words the iPhone says it with.
    private var why: String { machines.noStartReason }
}

/// A faint grid of dots at the top of the main area, gone a third of the way down.
private struct DottedFade: View {
    var body: some View {
        Canvas { context, size in
            let step: CGFloat = 14
            var y = step / 2
            while y < size.height * 0.4 {
                var x = step / 2
                while x < size.width {
                    context.fill(
                        Path(ellipseIn: CGRect(x: x - 0.6, y: y - 0.6, width: 1.2, height: 1.2)),
                        with: .color(.white.opacity(0.11)))
                    x += step
                }
                y += step
            }
        }
        .mask(LinearGradient(colors: [.white, .clear], startPoint: .top, endPoint: UnitPoint(x: 0.5, y: 0.4)))
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }
}
