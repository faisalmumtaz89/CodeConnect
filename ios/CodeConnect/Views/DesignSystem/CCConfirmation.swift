import CodeConnectKit
import SwiftUI

/// An ask before a change, or a notice after one, as both apps draw every one
/// of them (the Mac's is `Confirmation`): a card centred on the screen under
/// the app's own dimming, never the system's alert. The safe answer comes
/// first and the one that acts last; a destructive answer is red-edged and
/// never filled. A tap on the dimming gives the safe answer.
struct CCConfirmation {
    let title: String
    let message: String
    /// The answers in order: the safe one first, the one that acts last.
    let answers: [Answer]
    /// A name to type, for an ask that makes something.
    var field: Field?

    struct Answer {
        enum Kind {
            /// The safe answer: a tap on the dimming gives it.
            case cancel
            /// An answer that acts and cannot be taken back.
            case destructive
            /// An answer that acts and can be taken back.
            case plain
            /// A notice's one answer.
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
    /// Asks while `item` holds something, in the card `ask` makes of it; any
    /// answer lets `item` go.
    func ccConfirmation<Item>(_ item: Binding<Item?>, _ ask: @escaping (Item) -> CCConfirmation) -> some View {
        modifier(CCConfirms(asking: item.wrappedValue.map(ask)) { item.wrappedValue = nil })
    }

    /// Asks while `isPresented` is true, in the card `ask` makes.
    func ccConfirmation(isPresented: Binding<Bool>, _ ask: @escaping () -> CCConfirmation) -> some View {
        modifier(CCConfirms(asking: isPresented.wrappedValue ? ask() : nil) { isPresented.wrappedValue = false })
    }

    /// The screen that draws the asks of every view in it, over all of
    /// itself: the root, and the root of each sheet, since a sheet covers the
    /// screen under it.
    func ccConfirmationHost() -> some View { modifier(CCConfirmationHost()) }
}

extension EnvironmentValues {
    /// The asks of the screen this view is on. A view built alone, as a test
    /// builds one, has no host, and its asks are not drawn.
    @Entry fileprivate var ccConfirmationQueue: CCConfirmationQueue?
}

/// The asks waiting to be drawn on one screen, the newest on top.
@Observable final class CCConfirmationQueue {
    fileprivate var shown: [Shown] = []

    fileprivate struct Shown {
        let owner: UUID
        let confirmation: CCConfirmation
        let close: () -> Void
    }
}

private struct CCConfirms: ViewModifier {
    @Environment(\.ccConfirmationQueue) private var queue
    @State private var owner = UUID()
    let asking: CCConfirmation?
    let close: () -> Void

    func body(content: Content) -> some View {
        content
            .onChange(of: asking == nil, initial: true) { _, idle in
                queue?.shown.removeAll { $0.owner == owner }
                if let asking, !idle { queue?.shown.append(.init(owner: owner, confirmation: asking, close: close)) }
            }
            .onDisappear { queue?.shown.removeAll { $0.owner == owner } }
    }
}

private struct CCConfirmationHost: ViewModifier {
    @State private var queue = CCConfirmationQueue()

    func body(content: Content) -> some View {
        content
            .environment(\.ccConfirmationQueue, queue)
            .overlay {
                if let shown = queue.shown.last {
                    CCConfirmationScreen(confirmation: shown.confirmation) { answer in
                        queue.shown.removeAll { $0.owner == shown.owner }
                        shown.close()
                        answer.perform()
                    }
                }
            }
    }
}

private struct CCConfirmationScreen: View {
    let confirmation: CCConfirmation
    let answer: (CCConfirmation.Answer) -> Void
    @Environment(\.dynamicTypeSize) private var typeSize

    var body: some View {
        ZStack {
            Color.black.opacity(0.6)
                .ignoresSafeArea()
                .contentShape(Rectangle())
                .onTapGesture {
                    if let safe = confirmation.answers.first(where: { $0.kind == .cancel || $0.kind == .ok }) {
                        answer(safe)
                    }
                }
                .accessibilityHidden(true)
            card
                .frame(maxWidth: 420)
                .padding(.horizontal, CC.space.md)
        }
    }

    private var card: some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            Text(confirmation.title)
                .ccType(CC.type.headline)
                .foregroundStyle(CC.text.primary)
                .fixedSize(horizontal: false, vertical: true)
            Text(unwidowed(confirmation.message))
                .ccType(CC.type.callout)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if let field = confirmation.field {
                CCField(
                    label: nil, text: field.text, placeholder: field.placeholder,
                    autocapitalization: .never, disableAutocorrection: true, focusOnAppear: true
                )
                .padding(.top, CC.space.xxs)
            }
            answers.padding(.top, CC.space.sm)
        }
        .padding(CC.space.lg)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(CC.color.surfaceOverlay, in: RoundedRectangle(cornerRadius: CC.radius.lg))
        .overlay(RoundedRectangle(cornerRadius: CC.radius.lg).strokeBorder(CC.color.border))
        .shadow(color: .black.opacity(0.5), radius: 24, y: 10)
        .accessibilityElement(children: .contain)
        .accessibilityAddTraits(.isModal)
    }

    /// In a row at the right edge; at the accessibility sizes one under the
    /// other, the whole width, the acting answer last.
    @ViewBuilder
    private var answers: some View {
        if typeSize.isAccessibilitySize {
            VStack(spacing: CC.space.xs) {
                ForEach(Array(confirmation.answers.enumerated()), id: \.offset) { _, each in
                    button(each, fullWidth: true)
                }
            }
        } else {
            HStack(spacing: CC.space.xs) {
                Spacer(minLength: 0)
                ForEach(Array(confirmation.answers.enumerated()), id: \.offset) { _, each in
                    button(each, fullWidth: false)
                }
            }
        }
    }

    private func button(_ each: CCConfirmation.Answer, fullWidth: Bool) -> some View {
        let variant: CCButtonVariant
        switch each.kind {
        case .cancel, .plain: variant = .secondary
        case .destructive: variant = .destructive
        case .ok: variant = .primary
        }
        return CCButton(each.title, variant: variant, size: .md, fullWidth: fullWidth, haptic: nil) { answer(each) }
            .accessibilityIdentifier(each.id)
    }

    /// The sentence with its last two words held together, so its last line
    /// never holds one word alone.
    private func unwidowed(_ sentence: String) -> String {
        guard let last = sentence.lastIndex(of: " ") else { return sentence }
        return sentence.replacingCharacters(in: last...last, with: "\u{00A0}")
    }
}
