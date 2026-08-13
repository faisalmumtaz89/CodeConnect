import SwiftUI
import UIKit

// =============================================================================
//  CodeConnect Design Kit — token layer
// =============================================================================
//
//  The design language in executable form: the semantic tokens are the API,
//  and **raw hex appears exactly once**, in `Hex` below. Nothing outside this
//  file may name a colour by value.
//
//  Dark-only, on purpose. Every colour is an absolute sRGB value, never a
//  `UIColor` dynamic provider and never `Color(.systemBackground)` — so the
//  palette cannot follow the system appearance even by accident. Force-dark is
//  belt *and* braces: `UIUserInterfaceStyle = Dark` in Support/Info.plist pins
//  UIKit-backed surfaces (keyboard, camera preview, alerts, text selection),
//  and `.ccAppearance()` pins the SwiftUI side.
//
//  Naming note: the nested namespaces are lowercase (`CC.color`, `CC.space`)
//  so call sites read as prose — `CC.color.surfaceRaised`, `CC.space.md`. That
//  is a deliberate deviation from Swift's UpperCamelCase type convention,
//  confined to this one namespace, and it is what makes the token layer
//  disappear into the code that uses it.
//
// =============================================================================

// MARK: - The one place raw hex is allowed

/// Every literal colour value in the product. If you are adding a hex here,
/// you are changing the design language, not just adding a colour.
private enum Hex {
    // Surface — a four-step luminance ladder. The steps are deliberately tiny
    // (L = 0, 0.0030, 0.0065, 0.0103) so separation reads as *depth* rather
    // than as grey blocks. Hairlines do the actual separating.
    static let bg: UInt32 = 0x000000
    static let surface: UInt32 = 0x0A0A0A
    static let surfaceRaised: UInt32 = 0x131313
    static let surfaceOverlay: UInt32 = 0x1A1A1A

    // Border
    static let border: UInt32 = 0x262626
    static let borderStrong: UInt32 = 0x383838
    static let borderFocus: UInt32 = 0xEDEDED

    // Text
    static let text: UInt32 = 0xEDEDED
    static let textSecondary: UInt32 = 0xA1A1A1
    /// Corrected up from an original #737373, which was never measured.
    ///
    /// #737373 fails AA on every surface (4.43 / 4.18 / 3.92 / 3.67). #7D7D7D
    /// clears the first three but sits at 4.23 on `surfaceOverlay`, so shipping
    /// it would mean shipping a "don't use it there" rule alongside it. #828282
    /// clears **AA on all four surfaces** (≈5.42 / 5.11 / 4.79 / 4.50), so no
    /// such rule is needed: a token that is safe everywhere cannot be misused
    /// anywhere.
    static let textTertiary: UInt32 = 0x828282
    /// 2.69:1 on `bg`. Intentionally below AA and explicitly exempt: WCAG
    /// 1.4.3 excludes "text that is part of an inactive user interface
    /// component". A disabled control must *look* disabled; the reason it is
    /// disabled is carried by the accessibility hint, not by the contrast.
    static let textDisabled: UInt32 = 0x525252

    // Accent — the Vercel signature: the primary action is white-on-black.
    static let accent: UInt32 = 0xEDEDED
    static let accentPressed: UInt32 = 0xD4D4D4
    static let onAccent: UInt32 = 0x000000

    // Semantic. Colour is information, never decoration.
    static let success: UInt32 = 0x3ECF8E
    static let info: UInt32 = 0x3B82F6
    static let warning: UInt32 = 0xF5A623
    static let danger: UInt32 = 0xFF4D4F

    // Diff tints, composited once and pinned here as literals so a row tint
    // cannot drift with an opacity change somewhere.
    // Row tint = semantic @ 6% over `bg`; word tint = semantic @ 22%.
    // `text` on all four measures 12.70:1 or better.
    static let successMuted: UInt32 = 0x030C08
    static let dangerMuted: UInt32 = 0x0F0404
    static let successWord: UInt32 = 0x0D2D1F
    static let dangerWord: UInt32 = 0x381011

    // Syntax highlighting. **Three roles, and only three** — "more
    // colours than three turns a diff into a Christmas tree at phone scale".
    // Everything that is not a keyword, a string or a comment stays `text`.
    static let syntaxKeyword: UInt32 = 0xC792EA
    static let syntaxString: UInt32 = 0xC3E88D
    /// Comments were originally set in `textDisabled`. Corrected, for the same
    /// reason `textTertiary` was: `textDisabled` is reserved for genuinely
    /// inactive text — gutter line numbers, the cwd breadcrumb, the `·` between
    /// a project and its start time, disabled-button labels — and a code
    /// comment is none of those. A comment is something you have to *read*, so it takes a colour
    /// that clears AA rather than sitting at 2.69:1.
    ///
    /// #909090 is the dimmest grey that clears AA on the *darkest* background a
    /// syntax token can land on, which is not `bg` but the 22% word-diff tint
    /// `successWord` #0D2D1F: 4.66 there, 5.26 on `dangerWord`, 6.58 on `bg`.
    /// Still unmistakably quieter than code at `text` (17.94).
    static let syntaxComment: UInt32 = 0x909090

    // ANSI, the 16-colour terminal table.
    //
    // Only the six values below are new. The other ten are the palette's own
    // tokens re-used by name, which is the point: `git diff` in the terminal is
    // the same green as a `CCDiffRow`'s `+`, and a failing test is the same red
    // as a `danger` badge. A terminal that agrees with the app it is inside is
    // one fewer thing to learn at 2am.
    //
    //   0 black         `surfaceRaised`    8  bright black    `textDisabled`
    //   1 red           `danger`           9  bright red      ↓
    //   2 green         `success`         10  bright green    ↓
    //   3 yellow        `warning`         11  bright yellow   ↓
    //   4 blue          `info`            12  bright blue     ↓
    //   5 magenta       ↓                 13  bright magenta  `syntaxKeyword`
    //   6 cyan          ↓                 14  bright cyan     ↓
    //   7 white         `textSecondary`   15  bright white    `text`
    //
    // Magenta and cyan are the two hues the app itself has no use for, so they
    // are built rather than borrowed: magenta takes `syntaxKeyword`'s hue at
    // full chroma (and `syntaxKeyword` *is* its bright form), cyan sits at 190°,
    // between `success` and `info`, at the same saturation.
    static let ansiMagenta: UInt32 = 0xAD57E6
    static let ansiCyan: UInt32 = 0x4DBFD6
    /// Brights are their normals with the saturation pulled back and the value
    /// lifted — a lighter tint of the same hue, never a different colour.
    static let ansiBrightRed: UInt32 = 0xFF9495
    static let ansiBrightGreen: UInt32 = 0x79EEBA
    static let ansiBrightYellow: UInt32 = 0xFFC96F
    static let ansiBrightBlue: UInt32 = 0x87B4FF
    static let ansiBrightCyan: UInt32 = 0x94E6F6
}

private extension Color {
    /// Absolute sRGB. Deliberately not `Color(UIColor)` — a dynamic provider
    /// here is how a light-mode pixel gets into a dark-only product.
    init(cc hex: UInt32, opacity: Double = 1) {
        self.init(
            .sRGB,
            red: Double((hex >> 16) & 0xFF) / 255,
            green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255,
            opacity: opacity)
    }
}

// MARK: - Namespace

/// The design system's entire public surface.
enum CC {}

// MARK: - Colour

extension CC {
    // swiftlint:disable:next type_name
    enum color {
        // Surface
        static let bg = Color(cc: Hex.bg)
        static let surface = Color(cc: Hex.surface)
        static let surfaceRaised = Color(cc: Hex.surfaceRaised)
        static let surfaceOverlay = Color(cc: Hex.surfaceOverlay)

        // Border. `border` and `borderStrong` measure 1.39:1 and 1.79:1 on
        // `bg` — they are *separation*, never the sole identifier of a
        // control. Every CC control is identified by its label (≥ 4.5:1) and,
        // when focused, by a `borderFocus` ring at 17.94:1 (WCAG 1.4.11 and
        // 2.4.11 are met through the label and the focus ring, not the resting
        // hairline). See README "Contrast verification".
        static let border = Color(cc: Hex.border)
        static let borderStrong = Color(cc: Hex.borderStrong)
        static let borderFocus = Color(cc: Hex.borderFocus)

        // Action
        static let accent = Color(cc: Hex.accent)
        static let accentPressed = Color(cc: Hex.accentPressed)
        static let onAccent = Color(cc: Hex.onAccent)

        // Semantic
        static let success = Color(cc: Hex.success)
        static let info = Color(cc: Hex.info)
        static let warning = Color(cc: Hex.warning)
        static let danger = Color(cc: Hex.danger)

        /// The muted wash behind a semantic label. 12%, never a full-saturation
        /// fill — measured at 4.84:1 (info, the tightest) up to 8.31:1.
        static func muted(_ tone: CCTone) -> Color { tone.color.opacity(CC.opacity.muted) }

        // Diff row and word tints.
        static let successMuted = Color(cc: Hex.successMuted)
        static let dangerMuted = Color(cc: Hex.dangerMuted)
        static let successWord = Color(cc: Hex.successWord)
        static let dangerWord = Color(cc: Hex.dangerWord)

        // Syntax highlighting. Three roles; everything else is `text`.
        // Measured against every background a diff line can have — `bg`, the 6%
        // row tints and the 22% word tints — worst case in brackets:
        //   keyword 8.73 on `bg` (6.18 on `successWord`)
        //   string 15.25 on `bg` (10.80 on `successWord`)
        //   comment 6.58 on `bg` (4.66 on `successWord`)
        // All three clear AA everywhere the diff grid can put them.
        static let syntaxKeyword = Color(cc: Hex.syntaxKeyword)
        static let syntaxString = Color(cc: Hex.syntaxString)
        static let syntaxComment = Color(cc: Hex.syntaxComment)
    }

    // swiftlint:disable:next type_name
    enum text {
        static let primary = Color(cc: Hex.text)
        static let secondary = Color(cc: Hex.textSecondary)
        static let tertiary = Color(cc: Hex.textTertiary)
        static let disabled = Color(cc: Hex.textDisabled)
        /// The label that sits on top of `color.accent`.
        static let onAccent = Color(cc: Hex.onAccent)
    }

    // swiftlint:disable:next type_name
    enum opacity {
        /// Semantic wash behind a badge or banner.
        static let muted: Double = 0.12
        /// The wash that marks a *pressed* translucent control.
        static let press: Double = 0.08
        /// A control that is disabled but must still be readable as itself.
        static let disabled: Double = 0.45
        /// The resting glow around a live status dot.
        static let pulse: Double = 0.45
    }
}

// MARK: - ANSI

/// One entry in the terminal's 16-colour table.
///
/// Carries 16-bit components as well as a `Color` because the emulator's
/// palette API takes `0...65535` per channel, and going through `Color` to get
/// there would mean reading components back out of a `CGColor` — a round trip
/// through a colour space that is not necessarily sRGB.
struct CCANSIColor: Equatable, Sendable {
    let red: UInt16
    let green: UInt16
    let blue: UInt16

    fileprivate init(_ hex: UInt32) {
        // 0xFF → 0xFFFF exactly. `<< 8` alone would top out at 0xFF00 and tint
        // the whole table a shade dark.
        red = UInt16((hex >> 16) & 0xFF) * 257
        green = UInt16((hex >> 8) & 0xFF) * 257
        blue = UInt16(hex & 0xFF) * 257
    }

    var color: Color {
        Color(
            .sRGB, red: Double(red) / 65535, green: Double(green) / 65535,
            blue: Double(blue) / 65535)
    }
}

extension CC {
    /// The terminal's 16-colour table.
    ///
    /// Supplied explicitly because SwiftTerm's defaults are tuned for a light
    /// background: its normal blue is #0000EE, which measures **1.19:1** on
    /// `bg` and is unreadable in this product. Every entry here is either a
    /// palette token or built from one — see `Hex` for the derivation and the
    /// index map.
    ///
    /// Contrast on `bg`, in index order:
    /// `—` / 6.43 / 10.52 / 10.36 / 5.71 / 5.28 / 9.74 / 8.13 ·
    /// `—` / 9.92 / 14.75 / 13.84 / 10.00 / 8.73 / 14.91 / 17.94.
    ///
    /// The two exemptions are the two that must be: **black** (#131313, 1.13)
    /// and **bright black** (#525252, 2.69). ANSI black is a background colour
    /// and is invisible on a black terminal by construction — every emulator
    /// behaves this way, and lifting it would break `\e[30;47m` (black on
    /// white). Bright black is the conventional "dim" slot: when a remote
    /// program asks for dim, printing it bright would misreport what the
    /// program said. Neither is a colour this app *chooses* for its own text.
    // swiftlint:disable:next type_name
    enum ansi {
        /// Indices 0–7 normal, 8–15 bright, in the standard ANSI order:
        /// black, red, green, yellow, blue, magenta, cyan, white.
        static let table: [CCANSIColor] = [
            CCANSIColor(Hex.surfaceRaised),
            CCANSIColor(Hex.danger),
            CCANSIColor(Hex.success),
            CCANSIColor(Hex.warning),
            CCANSIColor(Hex.info),
            CCANSIColor(Hex.ansiMagenta),
            CCANSIColor(Hex.ansiCyan),
            CCANSIColor(Hex.textSecondary),

            CCANSIColor(Hex.textDisabled),
            CCANSIColor(Hex.ansiBrightRed),
            CCANSIColor(Hex.ansiBrightGreen),
            CCANSIColor(Hex.ansiBrightYellow),
            CCANSIColor(Hex.ansiBrightBlue),
            CCANSIColor(Hex.syntaxKeyword),
            CCANSIColor(Hex.ansiBrightCyan),
            CCANSIColor(Hex.text),
        ]

        /// The terminal's own background and foreground. Named separately
        /// because they are not part of the 16 and an emulator sets them by a
        /// different door.
        static let background = CC.color.bg
        static let foreground = CC.text.primary
    }
}

// MARK: - Space

extension CC {
    /// The 4pt grid, complete. Nothing between these values is a legal margin.
    // swiftlint:disable:next type_name
    enum space {
        static let xxs: CGFloat = 4
        static let xs: CGFloat = 8
        static let sm: CGFloat = 12
        static let md: CGFloat = 16
        static let lg: CGFloat = 20
        static let xl: CGFloat = 24
        static let xxl: CGFloat = 32
        static let xxxl: CGFloat = 48
    }

    /// **Vertical rhythm, by what the gap means.**
    ///
    /// `CC.space` is a grid: eight legal values and no opinion about which one a
    /// given gap should be. That is enough to keep every margin on the 4pt grid
    /// and not nearly enough to keep the app consistent, because the same
    /// relationship then drifts across screens — measured before this existed,
    /// text→text was written as 4, 8 *and* 16; surface→surface as 8, 12 *and* 16;
    /// section→section as 20 *and* 24. Every one of those is a legal margin. No
    /// two of them are the same decision.
    ///
    /// So the rhythm is named by the relationship rather than the size, and a
    /// screen picks the *meaning* — the number follows. Four values, because a
    /// fifth is how the drift starts again:
    ///
    ///   * `text` — consecutive text runs in one thought.
    ///   * `textSurface` — text meeting a card, block or field, either direction.
    ///   * `surfaces` — two surfaces side by side in a stack.
    ///   * `sections` — one section of a screen to the next.
    ///
    /// `CC.space.xxs` (4) survives for *component-internal optical* pairs — a
    /// title and its own subtitle, a glyph and its baseline — and is deliberately
    /// absent here: it is not a relationship between two things on a screen.
    // swiftlint:disable:next type_name
    enum rhythm {
        /// Text → text. 8.
        static let text: CGFloat = CC.space.xs
        /// Text ↔ card / block / field. 12.
        static let textSurface: CGFloat = CC.space.sm
        /// Surface → surface. 16.
        static let surfaces: CGFloat = CC.space.md
        /// Section → section. 24.
        static let sections: CGFloat = CC.space.xl
        /// Stacked controls inside one cluster. 12.
        ///
        /// A fifth name, added deliberately and with an argument, because the two
        /// places that stack controls disagreed — 8 on the fleet's action group,
        /// 12 under the decision card's action bar — and neither `surfaces` nor
        /// `textSurface` describes what that gap is. Two buttons offered as
        /// alternatives are not two independent surfaces sitting near each other;
        /// they are one control presenting its options, and spacing them like
        /// separate cards reads as two unrelated decisions.
        static let controls: CGFloat = CC.space.sm
    }

    // swiftlint:disable:next type_name
    enum radius {
        static let sm: CGFloat = 6
        static let md: CGFloat = 8
        static let lg: CGFloat = 12
        static let xl: CGFloat = 16
        /// Capsule. Expressed as a number so it can feed `RoundedRectangle`
        /// where a `Capsule` will not fit; prefer `Capsule()` when you can.
        static let pill: CGFloat = 999
    }

    // swiftlint:disable:next type_name
    enum stroke {
        /// 1pt, not 1px. A true hairline (1/3pt at @3x) at these luminances
        /// disappears; 1pt is the value that actually reads as an edge.
        static let hairline: CGFloat = 1
        /// One step up from a hairline, for the one place the badge scale has
        /// to escalate: a `danger` badge's border.
        ///
        /// `HIGH` used to be a 100%-saturation fill with a black label — the
        /// primary-button recipe in another hue — so the loudest object on the
        /// root screen was a non-interactive label out-shouting the `Review`
        /// button below it. Severity now escalates by **weight**: same fill
        /// rule, same border colour rule, half a point more edge.
        static let emphasis: CGFloat = 1.5
        /// The focus ring. 2pt at `borderFocus` — 17.94:1, unmissable.
        static let focus: CGFloat = 2
        /// The hold-to-approve progress ring.
        static let ring: CGFloat = 3
    }

    /// Fixed dimensions. Control *heights* are deliberately not scaled by
    /// Dynamic Type — they grow only when their content demands it, which the
    /// `minHeight` frames below allow.
    // swiftlint:disable:next type_name
    enum size {
        /// Apple's floor, and this product's: nothing tappable is smaller.
        static let hitTarget: CGFloat = 44
        static let controlSm: CGFloat = 36
        static let controlMd: CGFloat = 44
        /// Primary actions, at 52pt.
        static let controlLg: CGFloat = 52
        /// `CCRow`. Big, honest targets.
        static let rowMin: CGFloat = 64
        /// **The one badge height**, and the one chip height with it.
        ///
        /// 20, not the 21.33 that shipped: a 3-line blocked row budgets
        /// 16+22+4+18+4+20+16 = 100, which is on the 4pt grid, and 21.33 is not
        /// achievable on it. Scaled by Dynamic Type at the call site — see
        /// `CCBadge`, which is the only place this number is applied.
        static let badge: CGFloat = 20
        /// **A tappable chip**, which is a different object from a badge.
        ///
        /// A `CCBadge` with no action is a 20pt *label on something*. The same
        /// construction with an action is a control — the compose bar's file
        /// chips, the compose templates, the `SINCE YOU LOOKED` toggle — and a
        /// control of that kind is drawn at 32. Both shipped at the badge's 20
        /// (measured 19.67), so a row of things to press read as a strip of
        /// small captions; the 44pt finger around them was real but invisible.
        static let chip: CGFloat = 32
        /// How far a chip's **floor** may outgrow its nominal height.
        ///
        /// A chip is a `minHeight` around one line of `monoSmall`, so its
        /// content always wins where it genuinely needs the room; everything the
        /// ramp bought past that was empty box. Measured at AX5: a 32pt chip
        /// reserved ~115pt for a ~40pt line, and the diff's file strip alone took
        /// 100pt of the ~570pt of chrome standing above the first line of code
        /// on an 874pt screen. Same doctrine as `dotMaxScale` — round things
        /// scale, but not without limit.
        static let chipMaxScale: CGFloat = 1.75
        static let dot: CGFloat = 8
        static let dotSm: CGFloat = 6
        /// How far a status dot is allowed to outgrow its nominal size before
        /// it stops scaling.
        ///
        /// A dot that does not scale at all is an 8pt speck beside 40pt type —
        /// and the dot is a load-bearing state signal, not punctuation. A dot
        /// that scales *linearly* is a 25pt disc that blows the 32–40pt gutter
        /// column the whole spine is built on. 1.75× is the value that reaches
        /// the 14pt a dot needs at AX5 and still fits the gutter.
        static let dotMaxScale: CGFloat = 1.75
        static let iconSm: CGFloat = 13
        static let icon: CGFloat = 16
        static let iconLg: CGFloat = 20
        /// A glyph container: the circle around a sheet's close cross.
        static let glyph: CGFloat = 28
        /// The empty-state mark — a 32pt glyph inside a 64pt bordered circle.
        static let emptyGlyph: CGFloat = 32
        static let emptyGlyphCircle: CGFloat = 64
        /// A fact row: label left, value right, 48pt.
        static let factRow: CGFloat = 48
        /// A two-line list row: 16 + 22 + 4 + 18 + 16.
        static let rowRoomy: CGFloat = 76
    }
}

// MARK: - Motion

extension CC {
    /// The complete motion table. No duration in the product is off this list.
    // swiftlint:disable:next type_name
    enum duration {
        static let micro: Double = 0.12
        static let small: Double = 0.18
        static let medium: Double = 0.22
        static let exit: Double = 0.24
        static let draw: Double = 0.30
        /// Hold-to-approve. Long enough that it cannot happen in a pocket.
        static let hold: Double = 1.2
        /// How long "Copied" stays on screen.
        static let toast: Double = 1.4
        /// Reduce Motion collapses every class above to this cross-fade.
        static let reduced: Double = 0.12
    }

    /// "Motion is confirmation, not decoration." Springs only where a physical
    /// gesture warrants one, and never bouncy — the default SwiftUI spring
    /// overshoot is banned on sight.
    // swiftlint:disable:next type_name
    enum motion {
        /// 120ms. Press states, badge colour changes, opacity.
        static let micro = Animation.easeOut(duration: CC.duration.micro)
        /// 180ms. Disclosure expand/collapse, banner insert.
        static let small = Animation.easeOut(duration: CC.duration.small)
        /// 220ms. Sheet content entrance, gate arming, button morphs.
        static let medium = Animation.easeOut(duration: CC.duration.medium)
        /// 240ms ease-*in*, so departures accelerate away.
        static let exit = Animation.easeIn(duration: CC.duration.exit)
        /// Physical gestures only: Deck card promotion, hold-ring release.
        static let physical = Animation.spring(response: 0.32, dampingFraction: 0.82)
        /// 300ms. Stroke-drawn checkmarks and progress rings.
        static let draw = Animation.easeOut(duration: CC.duration.draw)
        /// What every class above becomes under Reduce Motion: a cross-fade,
        /// no scale, no translation, no spring, no stroke-draw.
        static let reduced = Animation.easeInOut(duration: CC.duration.reduced)
        /// A progress sweep must be linear or it lies about elapsed time.
        static func linear(_ seconds: Double) -> Animation { .linear(duration: seconds) }

        /// The default for state changes. Named `standard` because most call
        /// sites do not need to know which row of the table they are on.
        static let standard = small
    }
}

// MARK: - Type

/// A complete typographic specification: size, line height, weight, tracking
/// and the text style it scales against. Applied with `.ccType(_:)`.
///
/// Tracking and line height are stored at *nominal* size and scaled by the same
/// ratio Dynamic Type applies to the size, so the proportions of the scale
/// survive from `xSmall` all the way to `AX5`.
struct CCTextStyle: Equatable {
    var size: CGFloat
    var lineHeight: CGFloat
    var weight: Font.Weight
    /// Points at nominal size. The scale is authored in em; the conversion
    /// is done once, here, at declaration.
    var tracking: CGFloat
    var design: Font.Design
    /// The Dynamic Type ramp this style rides.
    var relativeTo: Font.TextStyle
    /// The colour this style carries by default, where it has one.
    /// Components apply it explicitly — `.ccType` never sets colour, so a call
    /// site can always override without fighting modifier precedence.
    var documentedColor: Color?
    /// Numbers that tick — ages, durations, sequence numbers — must not
    /// re-flow the row on every tick. True for every mono token.
    var monospacedDigits: Bool = false
    /// A ceiling on Dynamic Type growth, in points.
    ///
    /// `display` and `title` stop growing at `.accessibility1`: they are
    /// already large, and past that they only steal room from the content the
    /// user actually enlarged the type to read. Everything that carries
    /// information is uncapped.
    var maxSize: CGFloat?

    func weight(_ weight: Font.Weight) -> CCTextStyle {
        var copy = self
        copy.weight = weight
        return copy
    }

    func tracking(_ tracking: CGFloat) -> CCTextStyle {
        var copy = self
        copy.tracking = tracking
        return copy
    }

    /// The same style at the **monospaced design**, with tabular figures and no
    /// tracking.
    ///
    /// For the one shape the scale does not have a token for: a number that is
    /// the *headline* rather than metadata — a `CCStatStrip` value at `title`,
    /// where `mono` (14) and `monoSmall` (12) would both render the figure
    /// smaller than or equal to the 11pt label above it.
    ///
    /// **It has to be a property of the token, not a modifier on the view.**
    /// `.ccType(CC.type.title).monospaced()` is what shipped, and it does
    /// nothing: `.ccType` sets `.font(…)` *inside* its own body, closer to the
    /// `Text` than the outer transform, so the environment change is discarded
    /// before it is read. `THIS PASS`'s `26s` measured two advances inside one
    /// number — 17.67 then 19.33 — on the screen people screenshot, while
    /// `waiting 1m03s` two screens earlier was set in SF Mono.
    /// `.ccType` also derives its leading from `UIFont.systemFont` unless the
    /// style says monospaced, so the dead modifier left the line height wrong as
    /// well as the advance.
    ///
    /// Tracking goes to zero for the same reason `CCProse` drops it on a code
    /// run: mono is drawn on its own advance and a proportional token's letter-
    /// spacing pushes it off the grid that makes a column of figures comparable.
    func monospaced() -> CCTextStyle {
        var copy = self
        copy.design = .monospaced
        copy.monospacedDigits = true
        copy.tracking = 0
        return copy
    }

    /// The unscaled font, for the rare place that needs a `Font` and not a
    /// modifier (`.textSelection` contexts, `UIViewRepresentable` bridges).
    var font: Font { .system(size: size, weight: weight, design: design) }
}

extension CC {
    /// SF Pro throughout; `mono`/`monoSmall` are SF Mono.
    ///
    /// Identifiers, commands, paths, diffs, sequence numbers and durations are
    /// monospace — always. Prose is monospace — never.
    // swiftlint:disable:next type_name
    enum type {
        /// 32/38 semibold, −0.02em. The screen title, drawn in scroll content.
        /// Stops growing at 44pt, the size it reaches at `.accessibility1`.
        static let display = CCTextStyle(
            size: 32, lineHeight: 38, weight: .semibold, tracking: -0.64,
            design: .default, relativeTo: .largeTitle, documentedColor: CC.text.primary,
            maxSize: 44)

        /// 22/28 semibold, −0.01em. Sheet titles, stat values.
        /// Stops growing at 30pt, the size it reaches at `.accessibility1`.
        static let title = CCTextStyle(
            size: 22, lineHeight: 28, weight: .semibold, tracking: -0.22,
            design: .default, relativeTo: .title2, documentedColor: CC.text.primary,
            maxSize: 30)

        /// 17/22 semibold. Row titles, card titles, button labels at `lg`.
        static let headline = CCTextStyle(
            size: 17, lineHeight: 22, weight: .semibold, tracking: 0,
            design: .default, relativeTo: .headline, documentedColor: CC.text.primary)

        /// 16/22 regular. The reading size.
        static let body = CCTextStyle(
            size: 16, lineHeight: 22, weight: .regular, tracking: 0,
            design: .default, relativeTo: .body, documentedColor: CC.text.primary)

        /// 15/20 regular. Subtitles and secondary prose.
        static let callout = CCTextStyle(
            size: 15, lineHeight: 20, weight: .regular, tracking: 0,
            design: .default, relativeTo: .callout, documentedColor: CC.text.primary)

        /// 13/18 regular, `textSecondary`.
        static let footnote = CCTextStyle(
            size: 13, lineHeight: 18, weight: .regular, tracking: 0,
            design: .default, relativeTo: .footnote, documentedColor: CC.text.secondary)

        /// 11/14 medium, uppercase, +0.06em, `textTertiary`.
        ///
        /// **Section labels. One job, one colour, no exceptions.** `BLOCKED`,
        /// `DONE`, `RUNNING`, `LINK`, `EVENT STREAM` — the word that names a
        /// group of rows and nothing else.
        ///
        /// It used to do seven jobs in four colours, which is why `BLOCKED`, a
        /// `MEDIUM` badge and a row's status word measured as the same 11pt
        /// uppercase `#828282` on one screen: three classes of information
        /// rendered identically, so the reader's parser had nothing to grip.
        /// The other six jobs now have `badgeLabel` and `fieldLabel`, and the
        /// three roles separate optically by weight, tracking and colour.
        ///
        /// Uppercasing is the *caller's* job — `CCSectionHeader` does it —
        /// because a token must not mangle a string.
        static let micro = CCTextStyle(
            size: 11, lineHeight: 14, weight: .medium, tracking: 0.66,
            design: .default, relativeTo: .caption, documentedColor: CC.text.tertiary)

        /// 11/14 **semibold**, uppercase, +0.04em. **Badge and chip labels.**
        ///
        /// The text inside a `CCBadge`, a count chip, a `CCBanner`'s
        /// classification, a gap marker's pill. Semibold and tighter-tracked
        /// than `micro`, so a badge reads as a *label on an object* rather than
        /// as another section heading.
        ///
        /// Carries no `documentedColor`: a badge label is always its variant's
        /// tint at full strength, which only the component knows.
        static let badgeLabel = CCTextStyle(
            size: 11, lineHeight: 14, weight: .semibold, tracking: 0.44,
            design: .default, relativeTo: .caption, documentedColor: nil)

        /// 11/14 medium, uppercase, +0.02em, `textSecondary`. **The label that
        /// names a value** — a form field, a stat column, a numbered step.
        ///
        /// Brighter and tighter than `micro` because it is *attached* to the
        /// thing beside it rather than presiding over a group. Its one
        /// documented variation: it drops to `textTertiary` once the thing it
        /// labels is finished, which is a state, not a second colour rule.
        static let fieldLabel = CCTextStyle(
            size: 11, lineHeight: 14, weight: .medium, tracking: 0.22,
            design: .default, relativeTo: .caption, documentedColor: CC.text.secondary)

        /// 14/20. Commands, paths, diffs, identifiers.
        static let mono = CCTextStyle(
            size: 14, lineHeight: 20, weight: .regular, tracking: 0,
            design: .monospaced, relativeTo: .footnote, documentedColor: CC.text.primary,
            monospacedDigits: true)

        /// 12/16, `textTertiary`. Ages, durations, clock times, counts.
        static let monoSmall = CCTextStyle(
            size: 12, lineHeight: 16, weight: .regular, tracking: 0,
            design: .monospaced, relativeTo: .caption, documentedColor: CC.text.tertiary,
            monospacedDigits: true)
    }
}

// MARK: - Type application

private struct CCTypeModifier: ViewModifier {
    private let style: CCTextStyle
    @ScaledMetric private var scaledSize: CGFloat

    init(style: CCTextStyle) {
        self.style = style
        _scaledSize = ScaledMetric(wrappedValue: style.size, relativeTo: style.relativeTo)
    }

    func body(content: Content) -> some View {
        let size = min(scaledSize, style.maxSize ?? .greatestFiniteMagnitude)
        // One ratio drives size, tracking and leading together, so the style
        // keeps its proportions at every Dynamic Type step instead of turning
        // into a different typeface at AX5.
        let ratio = size / style.size
        return content
            .font(font(at: size))
            .tracking(style.tracking * ratio)
            .lineSpacing(leading(at: size, target: style.lineHeight * ratio))
    }

    private func font(at size: CGFloat) -> Font {
        let base = Font.system(size: size, weight: style.weight, design: style.design)
        // Ages tick once a second; proportional digits make the whole row
        // jitter as 9 becomes 10.
        return style.monospacedDigits ? base.monospacedDigit() : base
    }

    /// SwiftUI's `lineSpacing` is the gap *between* lines, not the line box, so
    /// the font's own line height has to come out of the target first. Asking
    /// UIKit for it is exact; a 1.2× guess is not.
    private func leading(at size: CGFloat, target: CGFloat) -> CGFloat {
        let font: UIFont =
            style.design == .monospaced
            ? .monospacedSystemFont(ofSize: size, weight: style.weight.uiKit)
            : .systemFont(ofSize: size, weight: style.weight.uiKit)
        return max(0, target - font.lineHeight)
    }
}

extension Font.Weight {
    /// The UIKit twin, for the two places the kit has to ask UIKit for a font's
    /// real metrics — `.ccType`'s leading, and `CCProse`'s, which measures the
    /// taller of a mixed prose/mono line.
    var uiKit: UIFont.Weight {
        switch self {
        case .ultraLight: return .ultraLight
        case .thin: return .thin
        case .light: return .light
        case .medium: return .medium
        case .semibold: return .semibold
        case .bold: return .bold
        case .heavy: return .heavy
        case .black: return .black
        default: return .regular
        }
    }
}

extension View {
    /// Applies a `CCTextStyle` — font, tracking and leading, scaled together.
    ///
    /// Deliberately does **not** set colour: the style's `documentedColor` is
    /// applied by the component, so a call site's own `.foregroundStyle` is
    /// never in a precedence fight with the type token.
    func ccType(_ style: CCTextStyle) -> some View {
        modifier(CCTypeModifier(style: style))
    }

    /// `.ccType` plus the style's own default colour.
    @ViewBuilder
    func ccType(_ style: CCTextStyle, color: Color?) -> some View {
        if let color = color ?? style.documentedColor {
            ccType(style).foregroundStyle(color)
        } else {
            ccType(style)
        }
    }
}

// MARK: - Tone

/// The semantic axis. Everything that carries state — badges, dots, banners,
/// freshness — is described by a tone, so a status and a risk and a link
/// health cannot drift into three different greens.
enum CCTone: String, CaseIterable, Hashable, Sendable {
    /// No colour. The default, and the correct answer far more often than the
    /// others: "if a colour isn't telling you something actionable, it's a bug".
    case neutral
    case success
    case info
    case warning
    case danger

    var color: Color {
        switch self {
        case .neutral: return CC.text.secondary
        case .success: return CC.color.success
        case .info: return CC.color.info
        case .warning: return CC.color.warning
        case .danger: return CC.color.danger
        }
    }

    /// The 12% wash behind a label of this tone.
    var muted: Color { color.opacity(CC.opacity.muted) }

    /// The hairline around a `muted` fill of this tone.
    ///
    /// **32%, which composites to the intended 40%.** The border is drawn
    /// *over* the 12% fill, not over the surface, so the number that reaches the
    /// screen is `0.32 + 0.68 × 0.12 = 0.40`. Measured: `warning` lands on
    /// `#684914` and `danger` on `#6C2526`, both exact. Writing 0.40 here would
    /// ship a 47% border, and every badge would sit a step louder than the rest
    /// of the scale.
    ///
    /// Neutral borrows the system hairline rather than a tinted one, so a
    /// stateless badge adds no colour at all.
    var border: Color {
        self == .neutral ? CC.color.border : color.opacity(0.32)
    }
}

// MARK: - Motion, with the accessibility setting honoured

private struct CCReduceMotionAnimation<V: Equatable>: ViewModifier {
    let animation: Animation?
    let value: V
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func body(content: Content) -> some View {
        // Not `nil`: under Reduce Motion every motion class *collapses to a
        // 120ms cross-fade* rather than snapping. Snapping loses
        // the confirmation the motion was carrying; the fade keeps it without
        // any scale, translation or spring.
        content.animation(reduceMotion ? CC.motion.reduced : animation, value: value)
    }
}

/// Suppresses a geometric effect — scale, offset, rotation — under Reduce
/// Motion while leaving colour and opacity changes alone.
private struct CCReduceMotionScale: ViewModifier {
    let scale: CGFloat
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    func body(content: Content) -> some View {
        content.scaleEffect(reduceMotion ? 1 : scale)
    }
}

extension View {
    /// `.animation(_:value:)` that honours Reduce Motion.
    ///
    /// Every animated state change in the kit goes through this. SwiftUI does
    /// not honour the setting for you.
    func ccAnimation<V: Equatable>(_ animation: Animation?, value: V) -> some View {
        modifier(CCReduceMotionAnimation(animation: animation, value: value))
    }

    /// A scale that becomes 1.0 under Reduce Motion.
    func ccScaleEffect(_ scale: CGFloat) -> some View {
        modifier(CCReduceMotionScale(scale: scale))
    }
}

// MARK: - Appearance

extension View {
    /// Pins the SwiftUI side of the app to dark and paints the window
    /// background, so an over-scroll or a sheet's backdrop never flashes the
    /// system's own colour.
    ///
    /// Apply once, at the app's root. `UIUserInterfaceStyle = Dark` in
    /// Support/Info.plist covers the UIKit side.
    func ccAppearance() -> some View {
        self
            .preferredColorScheme(.dark)
            .tint(CC.color.accent)
            .background(CC.color.bg.ignoresSafeArea())
    }

    /// The navigation bar, expressed the way iOS 26 wants it.
    ///
    /// **Two things, and it has to be both.** iOS 26 draws a *scroll edge
    /// effect* — a progressive blur of whatever is passing under the bar —
    /// independently of the toolbar's background:
    ///
    ///  * `.toolbarBackground(bg)` **alone** leaves the blur in place, which
    ///    over this palette reads as a lighter slab at the top of a black
    ///    screen and lets scrolled rows show through the title.
    ///  * `.scrollEdgeEffectStyle(.hard)` **alone** replaces the blur with a
    ///    flat edge but the bar keeps the system's own translucency, so the
    ///    same content is still faintly legible through it.
    ///
    /// Both, and only both, give the intended behaviour: on scroll, `bg` fades
    /// in from 0→1 over the first 24pt of travel. Measured twice on iOS 26,
    /// which is why they are welded into one modifier: every screen that took
    /// one and not the other shipped a different wrong bar.
    ///
    /// Apply where the screen's own scroll view is, above `.navigationTitle`.
    func ccNavigationChrome() -> some View {
        modifier(CCNavigationChrome())
    }
}

private struct CCNavigationChrome: ViewModifier {
    func body(content: Content) -> some View {
        // `.toolbarBackground` is unconditional — it is the half of the pair
        // that has existed since iOS 16 and is correct on every version. Only
        // the scroll-edge style is new, and on iOS 25 and earlier there is no
        // edge effect to replace.
        Group {
            if #available(iOS 26.0, *) {
                content.scrollEdgeEffectStyle(.hard, for: .top)
            } else {
                content
            }
        }
        .toolbarBackground(CC.color.bg, for: .navigationBar)
    }
}

// MARK: - Toolbar chrome

extension ToolbarContent {
    /// Opts a toolbar item out of the platform's shared item background.
    ///
    /// **The default toolbar glass must not ship.** On iOS 26 every
    /// toolbar item is wrapped in a shared Liquid Glass capsule, which renders
    /// as a light-grey lozenge over `#000` — a second, system-coloured surface
    /// sitting on top of a palette whose whole premise is that surfaces differ
    /// by a few percent of luminance. Hiding it leaves the item's self-drawn
    /// 36pt circle underneath, which is the chrome this product specifies.
    ///
    /// Every `ToolbarItem` in the app carries this. A bare one is a bug.
    @ToolbarContentBuilder
    func ccPlainToolbarItem() -> some ToolbarContent {
        if #available(iOS 26.0, *) {
            self.sharedBackgroundVisibility(.hidden)
        } else {
            self
        }
    }
}

// MARK: - Surface treatment

/// Where a component sits in the luminance ladder. Surfaces differ by a few
/// percent; the hairline does the separating.
enum CCSurfaceLevel {
    case bg
    case surface
    case raised
    case overlay

    var fill: Color {
        switch self {
        case .bg: return CC.color.bg
        case .surface: return CC.color.surface
        case .raised: return CC.color.surfaceRaised
        case .overlay: return CC.color.surfaceOverlay
        }
    }

    /// One step up. What a pressed row or a hovered card becomes.
    var pressed: Color {
        switch self {
        case .bg: return CC.color.surface
        case .surface: return CC.color.surfaceRaised
        case .raised: return CC.color.surfaceOverlay
        case .overlay: return CC.color.surfaceOverlay
        }
    }
}

extension View {
    /// Fill + inset hairline + radius, in the one order that keeps the border
    /// optically centred on the shape's edge.
    ///
    /// `strokeBorder`, never `stroke`: `stroke` straddles the path and leaves
    /// half a point of border outside the corner radius, which reads as a
    /// soft, slightly wrong edge at exactly the sizes this product uses.
    func ccSurface(
        _ level: CCSurfaceLevel,
        radius: CGFloat = CC.radius.lg,
        border: Color? = CC.color.border,
        lineWidth: CGFloat = CC.stroke.hairline
    ) -> some View {
        ccSurface(fill: level.fill, radius: radius, border: border, lineWidth: lineWidth)
    }

    func ccSurface(
        fill: Color,
        radius: CGFloat = CC.radius.lg,
        border: Color? = CC.color.border,
        lineWidth: CGFloat = CC.stroke.hairline
    ) -> some View {
        let shape = RoundedRectangle(cornerRadius: radius, style: .continuous)
        return
            self
            .background(fill, in: shape)
            .overlay {
                if let border {
                    shape.strokeBorder(border, lineWidth: lineWidth)
                }
            }
            .clipShape(shape)
    }

    /// Guarantees a 44pt tappable region around content that is visually
    /// smaller, and makes the whole region hit-test — including its
    /// transparent parts, which `Rectangle` does and a `Text` does not.
    func ccHitTarget(minWidth: CGFloat = CC.size.hitTarget, minHeight: CGFloat = CC.size.hitTarget)
        -> some View
    {
        frame(minWidth: minWidth, minHeight: minHeight)
            .contentShape(Rectangle())
    }
}

// MARK: - Haptics

/// The complete haptics table. **There are no others.** Every case is named
/// for its trigger, not for the generator it
/// pokes, so a screen cannot invent a new one without inventing a new trigger.
///
/// Two standing rules:
///  * Never two haptics inside 400ms — it reads as a glitch. (The hold ladder
///    at 25/50/75% is the one deliberate exception: it is a single escalating
///    event, not two unrelated ones.)
///  * Never a haptic on entrance from a notification — the notification
///    already buzzed.
///
/// Haptics are **not** suppressed by Reduce Motion. A haptic is not motion, and
/// removing it would remove information.
enum CCHaptic {
    /// Row press-in and release. Deliberately silent — visual only.
    case rowPress
    /// The Deck accessory bar, and a long-press on a diff hunk.
    case openHeavy
    /// The MEDIUM gate arming as the command scrolls into view; a Deck card
    /// landing; pull-to-refresh crossing the threshold; copy to clipboard.
    case light
    /// Allow, Deny and numbered-option taps, on touch-up.
    case decision
    /// Hold-to-approve passing 25 / 50 / 75%.
    case holdTick
    /// Hold-to-approve completing.
    case commit
    /// The daemon confirmed an answer. A QR code was detected. Fleet cleared.
    case success
    /// Face ID failed or was cancelled.
    case warning
    /// The daemon rejected the answer, or the network failed.
    case failure

    @MainActor
    func fire() {
        switch self {
        case .rowPress:
            break
        case .openHeavy, .decision:
            UIImpactFeedbackGenerator(style: .medium).impactOccurred()
        case .light:
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
        case .holdTick:
            UIImpactFeedbackGenerator(style: .soft).impactOccurred(intensity: 0.4)
        case .commit:
            UIImpactFeedbackGenerator(style: .rigid).impactOccurred()
        case .success:
            UINotificationFeedbackGenerator().notificationOccurred(.success)
        case .warning:
            UINotificationFeedbackGenerator().notificationOccurred(.warning)
        case .failure:
            UINotificationFeedbackGenerator().notificationOccurred(.error)
        }
    }
}
