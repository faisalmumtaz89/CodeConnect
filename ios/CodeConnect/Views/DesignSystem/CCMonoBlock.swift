import SwiftUI
import UIKit

// =============================================================================
//  CCMonoBlock — verbatim text with a copy affordance.
//
//  **This component may not hide a character.** It is the surface the product's
//  whole premise rests on — "you see the exact text before you answer" — and it
//  shipped a version that faded `…Feature.swi[ft]` into an alpha gradient, ran
//  `ssh-keygen -lf …key.pub` underneath the copy button, and did both under a
//  label reading EXACT COMMAND. A fade mid-token is precisely where a hostile
//  suffix hides, and it was the only alpha gradient in an app whose every other
//  edge is a 1pt hairline: the one place the language softened was the one
//  place it had to be hardest.
//
//  So the horizontal scroll and its fade are gone. What replaces them is the
//  diff grid's own treatment, which the same review called the best-executed
//  surface in the app: wrap at the measured column and mark the break with a
//  `↳`. The wrap is arithmetic, not a guess — a monospaced font has one advance,
//  so `columns = width / advance` lands on the glyph every time — and it is the
//  same `DiffLineWrap` the grid uses, so a command and a diff line break the
//  same way.
// =============================================================================

/// The one thing `CCMonoBlock` is allowed to shorten, and the two ways it may.
///
/// **There is deliberately no `.tail`.** Tail truncation cuts a command's
/// arguments — the half that says what it will do — and that is the exact byte
/// a hostile suffix hides behind. Removing the case removes the bug.
///
/// Reach for this *only* for a genuinely unbounded identifier: a public key, a
/// base64 blob, a hash — a string with no natural end, whose ends are what
/// identify it. A command is never truncated. A path is never truncated; a path
/// wraps, because `…/Sources/Feature.swift` and `/Users/dev/app/Sources/…` are
/// different files and the reader cannot tell which one they were shown.
enum CCMonoTruncation: Hashable {
    /// `…/Net/Sender.swift`. The tail identifies.
    case head
    /// `ssh-ed25519 AAAA…codeconnect-iphone`. Both ends identify: an SSH public
    /// key's trailing comment is the only thing that says which line in
    /// `authorized_keys` belongs to *this* phone.
    case middle
}

/// Commands, paths, diffs, snapshots, tool output.
///
/// Never re-wrapped and never prettified: what is shown has to be exactly what
/// will run. Long content wraps at the measured column with a `↳` continuation
/// glyph; it never fades, never ellipsises and never runs under the copy button.
struct CCMonoBlock: View {
    let text: String
    /// Collapses the block to this many *rendered* lines, with a disclosure
    /// underneath that says how many there are and opens them.
    ///
    /// Not a truncation: the count is stated and every character is one tap
    /// away. A block that quietly stopped at line 8 would be the same defect
    /// this component exists to eliminate, one axis over.
    var lineLimit: Int?
    /// Tints the text. `.danger` for failed output; `.neutral` leaves it in
    /// `text.primary`, which is the right answer almost always.
    var tone: CCTone = .neutral
    var showsCopy: Bool = true
    /// **Soft-wraps at word boundaries instead of at the column, and drops the
    /// `↳`.** For prose-shaped output only — a daemon message, a stack trace,
    /// an npm error — where the line breaks carry nothing and a continuation
    /// glyph on every second line would be noise.
    ///
    /// The default is the grid wrap, because the default content is a command,
    /// and in a command every character position is load-bearing.
    var wrapsAsProse: Bool = false
    /// `nil` — the default, and the only correct answer for a command or a
    /// path — wraps. See `CCMonoTruncation` before reaching for anything else.
    var truncation: CCMonoTruncation?
    var isSmall: Bool = false
    /// The container-less form — see `init(inline:)`.
    private var isInline: Bool = false
    /// How many lines the inline form may take at accessibility sizes.
    private var inlineLines: Int = 3

    @State private var justCopied = false
    @State private var resetTask: Task<Void, Never>?
    @State private var isExpanded = false
    /// The text column's width, measured. Starts at 0, which yields the minimum
    /// column count — narrow, never wide — so the first layout pass can never
    /// overflow the container it is still measuring.
    @State private var measuredWidth: CGFloat = 0

    @Environment(\.dynamicTypeSize) private var typeSize
    /// The container's declared text column. See `columnHang`.
    @Environment(\.ccColumnInset) private var columnInset
    /// A centred container has no gutter to hang into. See `columnHang`.
    @Environment(\.ccCentredContent) private var isCentred

    /// The copy affordance's chrome, on the same Dynamic Type ramp as the glyph
    /// inside it — and, crucially, as the trailing space reserved for it below.
    @ScaledMetric(relativeTo: .footnote) private var copyChrome: CGFloat = CC.size.glyphSm
    /// The rendered point size of the mono run, on the token's own ramp.
    ///
    /// Built the same way `CCTypeModifier` builds it — same nominal size, same
    /// `relativeTo` — so the advance measured here is the advance the text is
    /// actually laid out with. If these two ever diverge, the wrap column count
    /// is wrong and a glyph goes over the edge.
    @ScaledMetric private var fontSize: CGFloat

    init(
        _ text: String,
        lineLimit: Int? = nil,
        tone: CCTone = .neutral,
        showsCopy: Bool = true,
        wraps wrapsAsProse: Bool = false,
        truncation: CCMonoTruncation? = nil,
        isSmall: Bool = false
    ) {
        self.text = text
        self.lineLimit = lineLimit
        self.tone = tone
        self.showsCopy = showsCopy
        self.wrapsAsProse = wrapsAsProse
        self.truncation = truncation
        self.isSmall = isSmall
        let style = isSmall ? CC.type.monoSmall : CC.type.mono
        _fontSize = ScaledMetric(wrappedValue: style.size, relativeTo: style.relativeTo)
    }

    /// **A mono run with no container** — the preview form.
    ///
    /// Three places show a command *inside something else*: the fleet row's
    /// activity line, the session's approval preview, and the Deck's accessory
    /// bar. All three had written their own `Text(…).ccType(CC.type.monoSmall)`,
    /// because the full block is a raised surface, a copy button and ~22pt of
    /// chrome — a tenth element inside a 76pt row — and one of the three had
    /// quietly set its command in *proportional* type as a result. This is the
    /// one construction for all three: same font, same colour rule, same
    /// declared truncation, no container.
    ///
    /// **It is a preview, and the block is the proof.** Every call site sits one
    /// tap from a real `CCMonoBlock` that wraps at the measured column and hides
    /// nothing, which is what makes a bounded preview honest here and dishonest
    /// there. Below the accessibility sizes it is one line; at and above them it
    /// wraps up to `lines`, because a row that has grown to fit 40pt type has
    /// room to spend and a half-command is worth nothing.
    ///
    /// - Parameters:
    ///   - truncation: which end identifies, for the case where even the wrap
    ///     runs out. `.head` — keep the tail — is the default and the right
    ///     answer for a command: the prose label beside it already names the
    ///     tool, so what this run carries is the *object*, and `origin main` /
    ///     `origin master` is the distinction the product exists to draw.
    ///   - lines: the ceiling at accessibility sizes. Generous rather than
    ///     tight — the commands this product shows are shell one-liners and six
    ///     wrapped lines of AX5 monospace holds all of them — because the cap is
    ///     a guard against a pathological string, not a layout budget. When it
    ///     does bite it says how many lines it withheld.
    init(
        inline text: String,
        truncation: CCMonoTruncation = .head,
        lines: Int = 6,
        tone: CCTone = .neutral
    ) {
        self.text = text
        self.lineLimit = nil
        self.tone = tone
        self.showsCopy = false
        self.wrapsAsProse = false
        self.truncation = truncation
        self.isSmall = true
        self.isInline = true
        self.inlineLines = lines
        let style = CC.type.monoSmall
        _fontSize = ScaledMetric(wrappedValue: style.size, relativeTo: style.relativeTo)
    }

    var body: some View {
        if isInline { inlineRun } else { block }
    }

    /// No surface, no border, no padding, no copy affordance: exactly the height
    /// of its own lines, so it can be the third line of a 76pt row without
    /// making it a 98pt one.
    ///
    /// **Two layouts, and the split is the point.** Below the accessibility
    /// sizes it is one line and there is no wrap to get wrong, so it is a plain
    /// `Text` — no geometry probe on any of the two dozen rows a fleet can hold.
    /// At and above them it wraps, and a wrap here has to be *ours*: handed the
    /// string, the layout engine broke `git push --force origin main` after the
    /// `--`, which reads as a bare `--` separator followed by a file called
    /// `force`. That is the unmarked mid-token break `DiffLineWrap` and the `↳`
    /// exist to make impossible, and a command is the last place to allow it.
    @ViewBuilder
    private var inlineRun: some View {
        if typeSize.isAccessibilitySize {
            VStack(alignment: .leading, spacing: runSpacing) {
                ForEach(inlineVisibleRuns) { run in
                    HStack(alignment: .firstTextBaseline, spacing: 0) {
                        if run.isContinuation {
                            Text("↳")
                                .ccType(CC.type.monoSmall)
                                // `textTertiary`, not `textDisabled`.
                                // `textDisabled` is reserved for genuinely
                                // inactive text — gutter line numbers, the cwd
                                // breadcrumb, the `·` in `CCIdentity`,
                                // disabled-button labels — and this mark is none
                                // of them: it is the only thing on screen saying
                                // that a break fell **inside a token**, on the
                                // one component that may not hide a character. It
                                // shipped at 2.54:1, dimmer than the `−` in the
                                // command it was marking.
                                .foregroundStyle(CC.text.tertiary)
                                .fixedSize()
                                // Exactly one advance, so a continuation's
                                // column lines up with the column above it.
                                .frame(width: advance, alignment: .leading)
                                .accessibilityHidden(true)
                        }
                        Text(run.text)
                            .ccType(CC.type.monoSmall)
                            .foregroundStyle(foreground)
                            .allowsTightening(false)
                    }
                }
                // Never a silent cut. A preview that stopped at line six with no
                // mark would be the defect this component exists to eliminate,
                // one axis over; the count is stated, and every character is on
                // the decision card one tap away.
                if inlineOverflow > 0 {
                    // The count is a fact the reader has to act on — it is the
                    // sentence that says the preview is not the whole command —
                    // so it takes `textTertiary` (4.79:1) rather than the 2.54:1
                    // `textDisabled` reserved for inactive chrome.
                    Text("… \(inlineOverflow) more line\(inlineOverflow == 1 ? "" : "s")")
                        .ccType(CC.type.monoSmall)
                        .foregroundStyle(CC.text.tertiary)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .background { widthProbe }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(text)
        } else {
            Text(text)
                .ccType(CC.type.monoSmall)
                .foregroundStyle(foreground)
                // Never squeezed to fit: a tightened mono run is not the text
                // that will run.
                .allowsTightening(false)
                .lineLimit(1)
                .truncationMode(truncation == .head ? .head : .middle)
                .fixedSize(horizontal: false, vertical: true)
                // Spoken in full even where the line is not. A preview that
                // reads back its own ellipsis tells a VoiceOver user less than
                // the row shows a sighted one.
                .accessibilityLabel(text)
        }
    }

    /// The runs an inline block draws, bounded so a pathological command cannot
    /// grow one fleet row past the screen.
    private var inlineVisibleRuns: [Run] {
        let all = runs
        return all.count > inlineLines ? Array(all.prefix(inlineLines)) : all
    }

    private var inlineOverflow: Int { max(0, runs.count - inlineLines) }

    private var block: some View {
        // 4, not 12: the rows below the text are 44pt controls that already
        // carry their own air. Stacked on a 12pt gutter they read as slack
        // under the command rather than as a footer to it.
        VStack(alignment: .leading, spacing: CC.space.xxs) {
            content
            // The rule is not decoration. Without it, at AX5, `SHOW ALL 9
            // LINES` and `Copy` sit 4pt under the last wrapped line inside the
            // same fill and read for a beat as the command's next lines —
            // which is the one reading this component may never allow.
            if hasFooter {
                CCHairline()
                    .padding(.top, CC.space.xxs)
                disclosure
                if stacksCopy { copyRow }
            }
        }
        // Always the full width it was proposed, so the width measured below is
        // the container's and not the content's. A content-derived width would
        // feed the wrap that produced it — the block would settle at whatever
        // its first frame happened to be.
        .frame(maxWidth: .infinity, alignment: .leading)
        .background { widthProbe }
        .padding(.vertical, CC.space.sm)
        .padding(.leading, CC.space.sm)
        // Room for the copy button, so a long first line does not run
        // underneath it.
        //
        // Measured failure this replaces: the reserved width was a constant
        // 40pt while the button inside it grew with Dynamic Type, so at AX5 the
        // copy chrome sat *on top of* the scrolling command — the one string on
        // the card that must never be obscured. The reservation is now the
        // button's own scaled width plus its 4pt inset plus a 12pt gap, and it
        // is taken off the text column for the block's whole height, not just
        // its first line: the button is 24pt tall at `medium` and over 60 at
        // AX5, where it spans three lines of the command underneath it.
        .padding(.trailing, trailingReservation)
        .frame(maxWidth: .infinity, alignment: .leading)
        .ccSurface(.raised, radius: CC.radius.md)
        .overlay(alignment: .topTrailing) { if !stacksCopy { copyButton } }
        // **The two-edge rule's corollary, and the only place it is
        // implemented** (see `CCColumn.hang(from:)`). Applied outside the
        // surface, so it moves the border and leaves the text where the
        // container put it.
        .padding(.leading, columnHang)
        .onDisappear { resetTask?.cancel() }
    }

    // MARK: - Layout arithmetic

    /// What the block adds to its own leading edge so that its **text** lands on
    /// the container's text column and its **border** hangs 12pt left of it —
    /// text 52, border 40.
    ///
    /// Three cases, and the component is told which one it is in rather than the
    /// call site being asked:
    ///
    ///  * **Inside a container that declares its column** (`CCCard`, `CCStepRow`,
    ///    `CCFactRow`'s detail slot, the host-key alarm) — the block is already
    ///    standing on the column, so it hangs back the 12 its own padding will
    ///    add. Measured before this on the host-key card at AX5: every string at
    ///    32.00 and the mono block's at **44.00**, the last extra text edge on
    ///    the app's most serious screen.
    ///  * **Free-standing on a page** (`columnInset == 0` — the decision card,
    ///    the comment sheet) — nothing has stepped out yet, so the block finds
    ///    the content column itself, exactly as `CCSectionHeader` does, and hangs
    ///    back from there. The `EXACT COMMAND` header and the command underneath
    ///    it end up on one edge.
    ///  * **Inside a centred container** (`CCEmptyState`, a centred card) — no
    ///    gutter exists, so nothing moves: a 12pt step on one side of a centred
    ///    280pt block is a 6pt error in the middle of it.
    private var columnHang: CGFloat {
        isCentred ? 0 : CCColumn.hang(from: columnInset)
    }

    private var style: CCTextStyle { isSmall ? CC.type.monoSmall : CC.type.mono }

    private var uiFont: UIFont {
        .monospacedSystemFont(ofSize: fontSize, weight: .regular)
    }

    /// One character's width. A monospaced font has exactly one.
    private var advance: CGFloat {
        max(("0" as NSString).size(withAttributes: [.font: uiFont]).width, 1)
    }

    /// The gap between wrapped runs, so a stack of one-line `Text`s keeps the
    /// same rhythm a single wrapped `Text` would have had. `.ccType` applies
    /// this as `lineSpacing`, which only acts *inside* a `Text`; here the runs
    /// are separate views, so the leading has to become the stack's spacing.
    private var runSpacing: CGFloat {
        let ratio = fontSize / style.size
        return max(0, style.lineHeight * ratio - uiFont.lineHeight)
    }

    /// How many characters fit. Half a point of slack absorbs the difference
    /// between UIKit's measurement and CoreText's layout, so a rounding error
    /// can never push the last glyph past the edge — where SwiftUI would break
    /// the line itself, unmarked.
    ///
    /// **The width can be infinite, and `Int(.infinity)` is a trap, not a
    /// number.** A block placed in an `overlay` or a horizontal scroller is
    /// proposed an unbounded width, `.frame(maxWidth: .infinity)` returns it
    /// unchanged, and the geometry probe reports `inf` — which took the whole
    /// app down the first time the decision card rendered one. Anything that is
    /// not a finite, positive count means "not measured yet": fall back to a
    /// single unwrapped run, which is always safe because it is what the block
    /// showed before it knew its width.
    private var columns: Int {
        guard measuredWidth.isFinite, measuredWidth > 0, advance > 0 else { return 0 }
        let fitting = ((measuredWidth - 0.5) / advance).rounded(.down)
        guard fitting.isFinite, fitting >= 4 else { return 0 }
        // 4096 is not a layout limit, it is an arithmetic one: no phone is this
        // wide, so a number past it is a measurement that went wrong.
        return min(4096, Int(fitting))
    }

    /// At accessibility sizes the copy button leaves the text's line and
    /// becomes a row of its own underneath it.
    ///
    /// Two things at once. The reservation costs ~76pt of a ~340pt block at AX5
    /// — a quarter of the width, off the one string that must be readable —
    /// and a 60pt icon-only square is a worse target than a labelled 44pt row.
    /// Below the block, the command gets the full width and the button gets a
    /// word.
    private var stacksCopy: Bool { showsCopy && typeSize.isAccessibilitySize }

    private var trailingReservation: CGFloat {
        showsCopy && !stacksCopy
            ? copyChrome + CC.space.xxs + CC.space.sm
            : CC.space.sm
    }

    private var widthProbe: some View {
        GeometryReader { proxy in
            Color.clear
                .onChange(of: proxy.size.width, initial: true) { _, new in
                    // Only ever store a real measurement. An unbounded
                    // proposal reports `inf`, and a stored `inf` is a number
                    // every arithmetic below has to defend against instead of
                    // one that never arrives.
                    measuredWidth = new.isFinite && new > 0 ? new : 0
                }
        }
    }

    // MARK: - Content

    /// Two modes, and no third.
    ///
    ///  * `truncation == nil` — the grid. Wraps, marks the break, shows
    ///    everything. Every command and every path lands here.
    ///  * `truncation != nil` — an unbounded identifier. One run, clipped at
    ///    the end that does not identify it, with the copy button carrying the
    ///    whole string.
    @ViewBuilder
    private var content: some View {
        if let truncation {
            identifierRun(truncation)
        } else {
            wrappedRuns
        }
    }

    private var wrappedRuns: some View {
        VStack(alignment: .leading, spacing: runSpacing) {
            ForEach(visibleRuns) { run in
                HStack(alignment: .firstTextBaseline, spacing: 0) {
                    if run.isContinuation {
                        Text("↳")
                            .ccType(style)
                            // See the inline run's note: the mark that says a
                            // break fell inside a token is read, not chrome, so
                            // it clears AA rather than sitting at `textDisabled`.
                            .foregroundStyle(CC.text.tertiary)
                            .fixedSize()
                            // Exactly one advance, so a continuation's code
                            // column lines up with the column above it.
                            .frame(width: advance, alignment: .leading)
                            .accessibilityHidden(true)
                    }
                    Text(run.text)
                        .ccType(style)
                        .foregroundStyle(foreground)
                        // Never squeezed to fit: a tightened mono run is not
                        // the text that will run.
                        .allowsTightening(false)
                        .textSelection(.enabled)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        // One element, not one per wrapped line. VoiceOver reads the command
        // the daemon sent, without the continuation glyphs and without making
        // the user swipe once per line to hear it.
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(text)
    }

    private func identifierRun(_ mode: CCMonoTruncation) -> some View {
        Text(text)
            .ccType(style)
            .foregroundStyle(foreground)
            .allowsTightening(false)
            .textSelection(.enabled)
            .lineLimit(lineLimit)
            .truncationMode(mode == .head ? .head : .middle)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            .accessibilityLabel(text)
    }

    /// The block **is** the content, so it takes `text` at full contrast. The
    /// inline run *qualifies* a prose label sitting beside it — `Bash` then the
    /// command — and a preview that out-weighs the thing it belongs to has
    /// inverted the hierarchy, so it takes one step down. A tone always wins:
    /// failed output is `danger` in either form.
    private var foreground: Color {
        guard tone == .neutral else { return tone.color }
        return isInline ? CC.text.secondary : CC.text.primary
    }

    /// One rendered line.
    private struct Run: Identifiable {
        let id: Int
        let text: String
        /// A continuation of the line above it, so it wears the `↳`.
        let isContinuation: Bool
    }

    private var runs: [Run] {
        guard columns > 0 else {
            return [Run(id: 0, text: text, isContinuation: false)]
        }
        var out: [Run] = []
        for line in text.components(separatedBy: "\n") {
            if wrapsAsProse {
                // Prose wears the `↳` only where the break fell *inside* a
                // token. A break at a space needs no mark — that is what a
                // space is — but a break through the middle of
                // `…2026-07-31T09_14_02_113Z-debug.log` reads as a space that
                // is not there, which is the same ambiguity the hyphen-broken
                // path had.
                for piece in CCMonoProseWrap.wrap(line, columns: columns) {
                    out.append(
                        Run(id: out.count, text: piece.text, isContinuation: piece.splitsAToken))
                }
            } else {
                for (index, piece) in DiffLineWrap.wrap(line, columns: columns).enumerated() {
                    out.append(Run(id: out.count, text: piece, isContinuation: index > 0))
                }
            }
        }
        return out
    }

    private var visibleRuns: [Run] {
        guard let lineLimit, lineLimit > 0, !isExpanded, runs.count > lineLimit else {
            return runs
        }
        return Array(runs.prefix(lineLimit))
    }

    private var isCollapsible: Bool {
        guard truncation == nil, let lineLimit, lineLimit > 0 else { return false }
        return runs.count > lineLimit
    }

    private var hasFooter: Bool { isCollapsible || stacksCopy }

    /// The honest end of a collapsed block: how many lines there are, and a way
    /// to see them. Never an ellipsis, never a fade.
    @ViewBuilder
    private var disclosure: some View {
        if isCollapsible, let lineLimit {
            Button {
                CCHaptic.light.fire()
                withAnimation(CC.motion.small) { isExpanded.toggle() }
            } label: {
                Text(isExpanded ? "Show fewer lines" : "Show all \(runs.count) lines")
                    .ccType(CC.type.badgeLabel)
                    .foregroundStyle(CC.text.secondary)
                    .textCase(.uppercase)
                    .ccHitTarget(minWidth: 0)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(CCCopyButtonStyle())
            .accessibilityHint(
                isExpanded
                    ? "Collapses the block"
                    : "Shows the remaining \(runs.count - lineLimit) lines")
        }
    }

    // MARK: - Copy

    private func copy() {
        CCPasteboard.copy(text)
        resetTask?.cancel()
        withAnimation(CC.motion.micro) { justCopied = true }
        resetTask = Task {
            try? await Task.sleep(for: .seconds(CC.duration.toast))
            guard !Task.isCancelled else { return }
            withAnimation(CC.motion.standard) { justCopied = false }
        }
    }

    private var copyGlyph: some View {
        CCIcon(
            justCopied ? "checkmark" : "doc.on.doc",
            size: CC.size.iconSm, weight: .semibold, relativeTo: .footnote
        )
        .foregroundStyle(justCopied ? CC.color.success : CC.text.tertiary)
    }

    @ViewBuilder
    private var copyButton: some View {
        if showsCopy && !stacksCopy {
            Button(action: copy) {
                copyGlyph
                    // Scales on the same ramp as the glyph inside and as the
                    // trailing space reserved for it in `body`.
                    .ccGlyphContainer(
                        CC.size.glyphSm, radius: CC.radius.sm, level: .overlay,
                        relativeTo: .footnote)
                    // 24pt of visible chrome, 44pt of finger.
                    .ccHitTarget()
            }
            .buttonStyle(CCCopyButtonStyle())
            .padding(.trailing, CC.space.xxs)
            .accessibilityLabel("Copy")
            .accessibilityValue(justCopied ? "Copied" : "")
        }
    }

    private var copyRow: some View {
        Button(action: copy) {
            HStack(spacing: CC.space.xs) {
                copyGlyph
                Text(justCopied ? "Copied" : "Copy")
                    .ccType(CC.type.footnote.weight(.semibold))
                    .foregroundStyle(justCopied ? CC.color.success : CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .frame(maxWidth: .infinity, minHeight: CC.size.hitTarget, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(CCCopyButtonStyle())
        .accessibilityLabel("Copy")
        .accessibilityValue(justCopied ? "Copied" : "")
    }
}

private struct CCCopyButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .ccPressScale(configuration.isPressed, scale: 0.92)
    }
}

// MARK: - Prose wrapping

/// Word wrapping for prose-shaped monospace output.
///
/// Done here rather than by the layout engine for the same reason the diff grid
/// does its own: the engine will break a path at the hyphen inside it
/// (`/private/tmp/ccsoak-` / `work.giePJZ`) and a reader cannot then tell
/// whether the `-` belongs to the path. This breaks at whitespace, or at the
/// column when a token is longer than the line — never inside a word that fits.
enum CCMonoProseWrap {
    /// One wrapped line, and whether getting here cut through a word.
    struct Line {
        let text: String
        /// The break above this line landed inside a token, so the line wears
        /// the `↳`. A break at a space does not: a space is its own mark.
        let splitsAToken: Bool
    }

    /// Breaks by *index*, never by re-joining tokens: splitting on spaces and
    /// gluing the words back with one space each would silently normalise the
    /// indentation of a stack trace, and this component's contract is that what
    /// you see is what was sent. The only character it ever consumes is the one
    /// space it broke at, which is what a word wrap is.
    static func wrap(_ text: String, columns: Int) -> [Line] {
        guard columns > 1 else { return [Line(text: text, splitsAToken: false)] }
        let characters = Array(text)
        guard characters.count > columns else {
            return [Line(text: text.isEmpty ? " " : text, splitsAToken: false)]
        }

        var lines: [Line] = []
        var start = 0
        var carriedHardBreak = false
        while start < characters.count {
            // A line that wears the `↳` loses one column to it, exactly as the
            // diff grid's continuations do. Get this wrong and the marked line
            // is one advance too wide — which is how the layout engine gets
            // handed a break it will make silently.
            let width = max(1, columns - (carriedHardBreak ? 1 : 0))
            guard characters.count - start > width else {
                lines.append(
                    Line(text: String(characters[start...]), splitsAToken: carriedHardBreak))
                break
            }
            // The window runs one past the line so a break landing exactly on
            // the column boundary fills the line rather than orphaning a word.
            let limit = start + width
            var breakAt = -1
            var index = limit
            while index > start {
                if characters[index] == " " {
                    breakAt = index
                    break
                }
                index -= 1
            }
            if breakAt > start {
                lines.append(
                    Line(text: String(characters[start..<breakAt]), splitsAToken: carriedHardBreak))
                carriedHardBreak = false
                start = breakAt + 1
            } else {
                // One token, longer than the line. There is nowhere to break it
                // but the column, and dropping it is not an option.
                lines.append(
                    Line(text: String(characters[start..<limit]), splitsAToken: carriedHardBreak))
                carriedHardBreak = true
                start = limit
            }
        }
        return lines.isEmpty ? [Line(text: " ", splitsAToken: false)] : lines
    }
}
