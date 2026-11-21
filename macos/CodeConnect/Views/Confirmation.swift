import SwiftUI

/// An ask before a change, or a notice after one, as the Mac draws every one of them: a card centred on the window
/// under the app's own dimming, never the system's alert. Esc gives the safe answer; a destructive answer never takes
/// Return; a notice's OK takes both.
struct Confirmation {
    let title: String
    let message: String
    /// The answers left to right: the safe one first, the one that acts last.
    let answers: [Answer]
    /// A name to type, for an ask that makes something: it has the keyboard, Return gives the plain answer, and
    /// while it is blank only the safe answer can be given.
    var field: Field?

    struct Answer {
        enum Kind {
            /// The safe answer: Esc gives it.
            case cancel
            /// An answer that acts and cannot be taken back: the destructive face, no key.
            case destructive
            /// An answer that acts and can be taken back: the plain face, no key.
            case plain
            /// A notice's one answer: Return and Esc give it.
            case ok
        }

        let title: String
        let kind: Kind
        /// What a driver finds it by.
        let id: String
        var perform: () -> Void = {}
    }

    struct Field {
        let placeholder: String
        let text: Binding<String>
    }
}

extension View {
    /// Asks while `item` holds something, in the card `ask` makes of it; any answer lets `item` go. The card holds the
    /// keyboard while it is up; whatever answers it, `returnsKeyboard` gives the keyboard back to what had it, if no
    /// other ask is up by then.
    func confirmation<Item>(
        _ item: Binding<Item?>, returnsKeyboard: (() -> Void)? = nil, _ ask: @escaping (Item) -> Confirmation
    ) -> some View {
        modifier(Confirms(asking: item.wrappedValue.map(ask), returnsKeyboard: returnsKeyboard) {
            item.wrappedValue = nil
        })
    }

    /// The window that draws the asks of every view in it, centred on itself.
    func confirmationHost() -> some View { modifier(ConfirmationHost()) }
}

/// The asks waiting to be drawn in one window, the newest on top.
@Observable final class ConfirmationQueue {
    fileprivate var shown: [Shown] = []

    /// An ask is up: the window's own keys wait until it is answered.
    var isAsking: Bool { !shown.isEmpty }

    fileprivate struct Shown {
        let owner: UUID
        let confirmation: Confirmation
        let returnsKeyboard: (() -> Void)?
        let close: () -> Void
    }
}

private struct Confirms: ViewModifier {
    @Environment(ConfirmationQueue.self) private var queue
    @State private var owner = UUID()
    let asking: Confirmation?
    let returnsKeyboard: (() -> Void)?
    let close: () -> Void

    func body(content: Content) -> some View {
        content
            .onChange(of: asking == nil, initial: true) { _, idle in
                queue.shown.removeAll { $0.owner == owner }
                if let asking, !idle {
                    queue.shown.append(
                        .init(owner: owner, confirmation: asking, returnsKeyboard: returnsKeyboard, close: close))
                }
            }
            .onDisappear { queue.shown.removeAll { $0.owner == owner } }
    }
}

private struct ConfirmationHost: ViewModifier {
    @State private var queue = ConfirmationQueue()

    func body(content: Content) -> some View {
        content
            .environment(queue)
            .overlay {
                if let shown = queue.shown.last {
                    ZStack {
                        // The window under the ask takes no clicks while it is up, and is dimmed until no control in it
                        // reads as live: at 0.6 a row's red-edged button falls below the card's own hairline in contrast.
                        Color.black.opacity(0.6).contentShape(Rectangle()).onTapGesture {}
                        ConfirmationCard(
                            confirmation: shown.confirmation, typed: shown.confirmation.field?.text ?? .constant("")
                        ) { answer in
                            queue.shown.removeAll { $0.owner == shown.owner }
                            shown.close()
                            answer.perform()
                            // A turn later, once AppKit has settled the keyboard after the card is gone; an ask the
                            // answer raised holds it instead, and returns it when it is answered in turn.
                            if let returnsKeyboard = shown.returnsKeyboard {
                                DispatchQueue.main.async { if !queue.isAsking { returnsKeyboard() } }
                            }
                        }
                    }
                    .ignoresSafeArea()
                }
            }
    }
}

/// The card: its title, its sentence, its answers at the bottom right.
private struct ConfirmationCard: View {
    let confirmation: Confirmation
    /// The field's text, held so a blank one keeps the acting answers off as it is typed.
    @Binding var typed: String
    let answer: (Confirmation.Answer) -> Void
    /// The card holds the keyboard while it is up, so nothing typed reaches the window under it; a card with a field
    /// gives it to the field.
    @FocusState private var holdsKeyboard: Bool
    @FocusState private var fieldFocused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(confirmation.title).font(Theme.ui(15, .semibold)).foregroundStyle(Theme.text)
            Text(unwidowed(confirmation.message)).font(Theme.ui(12.5)).foregroundStyle(Theme.text2)
                .fixedSize(horizontal: false, vertical: true)
            if let field = confirmation.field {
                TextField("", text: field.text, prompt: Text(field.placeholder).foregroundStyle(Theme.text3))
                    .accessibilityLabel(field.placeholder)
                    .accessibilityIdentifier("confirmation.field")
                    .focused($fieldFocused)
                    // Esc while typing reaches the field first; unanswered here it climbs to the sheet the card sits in,
                    // which it would close. The field gives the safe answer instead, as Esc does on every card.
                    .onExitCommand {
                        if let safe = confirmation.answers.first(where: { $0.kind == .cancel }) { answer(safe) }
                    }
                    .textFieldStyle(.plain)
                    .font(Theme.ui(12.5))
                    .padding(.horizontal, 10)
                    .frame(height: 28)
                    .background(Theme.code, in: RoundedRectangle(cornerRadius: 7))
                    .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.hairline))
                    .padding(.top, 4)
            }
            HStack(spacing: 8) {
                Spacer()
                ForEach(Array(confirmation.answers.enumerated()), id: \.offset) { _, each in
                    button(each)
                }
            }
            .padding(.top, 10)
        }
        .padding(18)
        .frame(width: 420, alignment: .leading)
        .background(Theme.sheet, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Theme.hairline))
        .shadow(color: .black.opacity(0.5), radius: 24, y: 10)
        .focusable(confirmation.field == nil)
        .focusEffectDisabled()
        .focused($holdsKeyboard)
        .onAppear {
            if confirmation.field == nil { holdsKeyboard = true } else { fieldFocused = true }
        }
        // A notice's OK is its safe answer too.
        .onKeyPress(.escape) {
            guard let ok = confirmation.answers.first(where: { $0.kind == .ok }) else { return .ignored }
            answer(ok)
            return .handled
        }
        .accessibilityElement(children: .contain)
        .accessibilityAddTraits(.isModal)
    }

    /// The sentence with its last two words held together, so its last line never holds one word alone.
    private func unwidowed(_ sentence: String) -> String {
        guard let last = sentence.lastIndex(of: " ") else { return sentence }
        return sentence.replacingCharacters(in: last...last, with: "\u{00A0}")
    }

    @ViewBuilder private func button(_ each: Confirmation.Answer) -> some View {
        // With a field, the plain answer is the one Return gives, and is drawn so.
        let takesReturn = each.kind == .ok || (each.kind == .plain && confirmation.field != nil)
        let blank = confirmation.field != nil && typed.trimmingCharacters(in: .whitespaces).isEmpty
        let face = ActionButton(
            title: each.title, key: nil, prominent: takesReturn, destructive: each.kind == .destructive,
            id: each.id
        ) { answer(each) }
        .disabled(blank && each.kind != .cancel)
        if each.kind == .cancel {
            face.keyboardShortcut(.cancelAction)
        } else if takesReturn {
            face.keyboardShortcut(.defaultAction)
        } else {
            face
        }
    }
}
