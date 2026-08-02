import SwiftUI

// =============================================================================
//  CCStepRow — a numbered instruction the user performs somewhere else.
// =============================================================================

/// One step of a procedure the *app cannot do for you*: switching on Remote
/// Login, running `codeconnect pair` at the Mac, authorising a key.
///
/// That framing is the reason this is a component rather than a stack of
/// `Text`s. Every screen that uses it is a screen where CodeConnect is asking
/// the person to go and do something at a keyboard it does not own, so the
/// index, the state glyph and the copyable command have to look identical
/// everywhere or the user cannot tell how far through they are.
///
/// **Layout — the two-edge spine.** Inside a `CCCard(padding: 0)` the row's 16pt
/// inset puts its content edge on 32, and the index badge takes an 8pt layout
/// slot (the gutter column, 32→40) with a 12pt gap, so every string in the row —
/// title, body, command block — begins at **52**. The badge itself is 20pt and
/// is allowed to overflow its slot symmetrically: it is a graphic centred on the
/// gutter column, not text, and bleeding 6pt each way into empty padding is what
/// keeps the text on the spine. A 24pt badge — the size first drawn — would
/// leave 4pt between a bordered circle and a label, which reads as a collision.
struct CCStepRow<Accessory: View, Detail: View>: View {
    /// 1-based. Rendered verbatim; steps are referred to out loud by number.
    let index: Int
    let title: String
    var message: String?
    /// A command the user copies and runs. Nil for "click here in System
    /// Settings", which cannot be copied and must not pretend to be.
    var command: String?
    /// Provably done — SSH answered, the key is installed. Swaps the number for
    /// a `success` checkmark and drops the body to `textTertiary`.
    var isComplete: Bool = false

    private let accessory: () -> Accessory
    private let detail: () -> Detail

    /// Declared in the struct body rather than an extension on purpose: an
    /// initialiser in an extension does not suppress the synthesised memberwise
    /// one, and the two collide over the private closure properties.
    init(
        index: Int,
        title: String,
        message: String? = nil,
        command: String? = nil,
        isComplete: Bool = false,
        @ViewBuilder accessory: @escaping () -> Accessory,
        @ViewBuilder detail: @escaping () -> Detail
    ) {
        self.index = index
        self.title = title
        self.message = message
        self.command = command
        self.isComplete = isComplete
        self.accessory = accessory
        self.detail = detail
    }

    @Environment(\.dynamicTypeSize) private var typeSize
    /// How far the row's own container has already stepped out. Added to the
    /// row's inset so `stepBody` can declare where its text actually starts.
    @Environment(\.ccColumnInset) private var columnInset

    /// **Where this row's text column is**, declared for what it contains.
    ///
    /// A `CCMonoBlock` in the body is a nested surface, and the two-edge rule's
    /// corollary hangs its border 12pt left of the text column while its text
    /// stays on it.
    /// The block cannot know where that column is — the row put it there, 16 of
    /// inset plus the 8pt gutter slot plus the 12pt gap — so the row says so
    /// rather than the block guessing. At accessibility sizes the gutter is
    /// abandoned (see `body`) and the column is the row's own 16.
    private var bodyColumn: CGFloat {
        columnInset + (typeSize.isAccessibilitySize ? CC.space.md : CCColumn.content)
    }

    var body: some View {
        Group {
            if typeSize.isAccessibilitySize {
                // At accessibility sizes the gutter is abandoned rather than
                // defended: the badge grows with its digit, and a growing
                // graphic centred on an 8pt column eventually reaches the text
                // beside it. Measured at `accessibility-extra-large`, where the
                // circle overlapped the first letter of every step title.
                // Everything moves to one left edge instead.
                VStack(alignment: .leading, spacing: CC.space.xs) {
                    HStack(alignment: .firstTextBaseline, spacing: CC.space.xs) {
                        badge
                        titleLabel
                    }
                    accessory()
                    stepBody
                }
            } else {
                HStack(alignment: .top, spacing: CC.space.sm) {
                    badge
                        // The gutter column. See the note above: 8pt of layout,
                        // 20pt of graphic, centred.
                        .frame(width: CC.size.dot)
                        // **A fixed line box, not a minimum.** With `minHeight` the
                        // frame grew to the badge's own 20pt while the title's line
                        // is about 14, so the circle's centre sat ~3pt below the
                        // centre of the words it numbers, and every step in a card
                        // repeated the error down the column. A fixed height centres
                        // the badge on the title's line and lets the circle overhang
                        // it symmetrically, which is what a marginal number does.
                        .frame(height: titleLine)

                    VStack(alignment: .leading, spacing: CC.space.xs) {
                        HStack(spacing: CC.space.xs) {
                            titleLabel
                            // The accessory belongs on the row's **trailing
                            // edge**, not tucked against the title. Without the
                            // spacer `RECOMMENDED` measured at x=199 — floating
                            // mid-row, reading as part of the step's name rather
                            // than as a mark on the step.
                            //
                            // Horizontal form only: the accessibility branch
                            // above stacks, and a `Spacer` that survived the
                            // switch would push the accessory to the far side of
                            // a row it is supposed to sit under.
                            Spacer(minLength: CC.space.xs)
                            accessory()
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)

                        stepBody
                    }
                }
            }
        }
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, CC.space.sm)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .contain)
        .accessibilityLabel(
            isComplete ? "Step \(index), done. \(title)" : "Step \(index). \(title)")
    }

    private var titleLabel: some View {
        // `fieldLabel`: it names the block underneath it, it is not a heading
        // over a group of rows. Its documented dim-when-done is the one
        // variation the token allows, and it is a state, not a second colour.
        Text(title.uppercased())
            .ccType(CC.type.fieldLabel)
            .foregroundStyle(isComplete ? CC.text.tertiary : CC.text.secondary)
            .fixedSize(horizontal: false, vertical: true)
            // Uppercasing is a visual style, not content: VoiceOver gets the
            // string the daemon actually wrote, and so does any test matching
            // on it.
            .accessibilityLabel(title)
    }

    @ViewBuilder
    private var stepBody: some View {
        if let message {
            Text(Self.prose(message))
                .ccType(CC.type.footnote)
                .foregroundStyle(isComplete ? CC.text.tertiary : CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }

        if let command {
            // `CCMonoBlock` brings the 44pt copy button, the `Copied` state and
            // the `.impact(.light)` with it — copy is the single most-used
            // control on a setup screen.
            CCMonoBlock(command, isSmall: true)
                .padding(.top, CC.space.xxs)
                .ccColumnInset(bodyColumn)
        }

        detail()
            .ccColumnInset(bodyColumn)
    }

    /// Step prose, with backticked fragments set in monospace.
    ///
    /// The daemon's own copy writes commands as `` `--ssh` `` and this screen
    /// renders it **verbatim** — but verbatim means the *words*, not the
    /// punctuation a markup convention uses to mark them. Rendering the
    /// backticks literally puts a command in prose type and two stray glyphs on
    /// screen; parsing them puts it in monospace, which is the rule everywhere
    /// else — commands are monospace, always. Nothing is added, removed or
    /// reordered. Unparseable input falls back to the raw string.
    private static func prose(_ message: String) -> AttributedString {
        (try? AttributedString(markdown: message)) ?? AttributedString(message)
    }

    /// Tracks the title's own line height so the badge stays beside the *title*
    /// on a step whose body runs to four lines, rather than drifting to the
    /// vertical centre of the whole block.
    @ScaledMetric(relativeTo: .caption) private var titleLine: CGFloat = 14

    /// The badge scales with the digit inside it. A fixed 20pt circle around a
    /// glyph that grows is a glyph that escapes its circle — `ccGlyphContainer`
    /// is the one place that rule is implemented.
    private var badge: some View {
        Group {
            if isComplete {
                CCIcon("checkmark", size: 11, weight: .bold, relativeTo: .caption)
                    .foregroundStyle(CC.color.success)
            } else {
                Text("\(index)")
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.primary)
            }
        }
        .ccGlyphContainer(
            CC.space.lg, level: .overlay,
            border: isComplete ? CC.color.success.opacity(0.45) : CC.color.border,
            relativeTo: .caption)
        .accessibilityHidden(true)
    }
}

// MARK: - Convenience initialisers

extension CCStepRow where Accessory == EmptyView, Detail == EmptyView {
    init(
        index: Int,
        title: String,
        message: String? = nil,
        command: String? = nil,
        isComplete: Bool = false
    ) {
        self.init(
            index: index, title: title, message: message, command: command,
            isComplete: isComplete, accessory: { EmptyView() }, detail: { EmptyView() })
    }
}

extension CCStepRow where Detail == EmptyView {
    init(
        index: Int,
        title: String,
        message: String? = nil,
        command: String? = nil,
        isComplete: Bool = false,
        @ViewBuilder accessory: @escaping () -> Accessory
    ) {
        self.init(
            index: index, title: title, message: message, command: command,
            isComplete: isComplete, accessory: accessory, detail: { EmptyView() })
    }
}

// MARK: - Preview

#Preview("CCStepRow") {
    ScrollView {
        VStack(spacing: CC.space.md) {
            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    CCStepRow(
                        index: 1,
                        title: "Option 1 - Tailscale SSH",
                        message:
                            "Authentication and access control ride your tailnet ACLs, and no port is exposed anywhere.",
                        command: "tailscale up --ssh"
                    ) {
                        CCBadge("Recommended", tone: .success)
                    }
                    CCHairline()
                    CCStepRow(
                        index: 2,
                        title: "Option 2 - macOS Remote Login",
                        message:
                            "System Settings → General → Sharing → Remote Login. Limit access to your own user.",
                        isComplete: true)
                    CCHairline()
                    CCStepRow(
                        index: 3,
                        title: "Then authorise this iPhone",
                        message: "Run this at the Mac and scan the QR it prints.",
                        command: "codeconnect pair --ssh")
                }
            }
        }
        .padding(CC.space.md)
    }
    .background(CC.color.bg)
    .ccAppearance()
}
