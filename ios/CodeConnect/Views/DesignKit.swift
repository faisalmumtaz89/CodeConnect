import CodeConnectKit
import SwiftUI
import UIKit

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
        max(0, target - style.uiFont(ofSize: size).lineHeight)
    }
}

extension CCTextStyle {
    /// The UIKit font this style draws at `size`, for its real metrics.
    func uiFont(ofSize size: CGFloat) -> UIFont {
        design == .monospaced
            ? .monospacedSystemFont(ofSize: size, weight: weight.uiKit)
            : .systemFont(ofSize: size, weight: weight.uiKit)
    }
}

extension Font.Weight {
    /// The UIKit twin, for the places the kit has to ask UIKit for a font's
    /// real metrics — `.ccType`'s leading, `CCProse`'s, which measures the
    /// taller of a mixed prose/mono line, and `.ccMark`'s cap height.
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

// MARK: - A mark beside text

private struct CCMarkBeside: ViewModifier {
    let style: CCTextStyle
    @ScaledMetric private var scaledSize: CGFloat

    init(style: CCTextStyle) {
        self.style = style
        _scaledSize = ScaledMetric(wrappedValue: style.size, relativeTo: style.relativeTo)
    }

    func body(content: Content) -> some View {
        let lift = style.uiFont(ofSize: min(scaledSize, style.maxSize ?? .greatestFiniteMagnitude))
            .capHeight / 2
        return content
            .alignmentGuide(.firstTextBaseline) { $0[VerticalAlignment.center] + lift }
            .alignmentGuide(.lastTextBaseline) { $0[VerticalAlignment.center] + lift }
    }
}

extension View {
    /// **A mark beside text centres on the middle of that text's first line.**
    ///
    /// For an icon, a dot or a badge in a row aligned on `.firstTextBaseline`:
    /// the mark answers the baseline with its centre plus half a capital's
    /// height in `style` — the style of the words beside it — so its centre
    /// lands on the middle of their first line, however many lines they wrap
    /// to. The mark cannot know that size itself; aligned by its top, its
    /// middle or its own size instead, marks sat up to 6pt off the words.
    func ccMark(beside style: CCTextStyle) -> some View {
        modifier(CCMarkBeside(style: style))
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
