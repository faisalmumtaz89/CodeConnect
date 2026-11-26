import AppKit
import CodeConnectKit
import SwiftUI

extension AttributedString {
    /// Markdown as the Mac draws it. A `code` run names a command, and a command reads as one
    /// unit, so its spaces do not break: the whole command wraps to the next line together. A link
    /// is underlined in the accent colour, so it reads as one whatever colour its text is set in; a
    /// click opens it.
    init(macMarkdown source: String) {
        let unbroken = source.replacing(/`[^`]*`/) { $0.output.replacingOccurrences(of: " ", with: "\u{00A0}") }
        self = (try? AttributedString(markdown: unbroken)) ?? AttributedString(source)
        for range in runs.filter({ $0[AttributeScopes.FoundationAttributes.LinkAttribute.self] != nil }).map(\.range) {
            self[range][AttributeScopes.SwiftUIAttributes.ForegroundColorAttribute.self] = CC.color.accent
            self[range][AttributeScopes.SwiftUIAttributes.UnderlineStyleAttribute.self] = .single
        }
    }
}

extension DaemonEndpoint {
    /// The address as a person types it into the code form: `host:port`, an IPv6 host in brackets.
    var typedAddress: String { host.contains(":") ? "[\(host)]:\(port)" : "\(host):\(port)" }
}

/// A line that says how long ago `since` was, kept true while it is on screen: Kit's `AgeTick` wakes it when its
/// printed age changes, as a fleet row's age is kept. With no `since` there is no age, and it never wakes.
struct AgeText: View {
    let since: Date?
    let text: (Date) -> String
    @State private var lastTick = Date()

    var body: some View {
        Text(text(AgeTick.renderTime(lastTick: lastTick)))
            .task(id: since.map { AgeClock(since: $0, scale: .age) }) {
                guard let since else { return }
                await AgeTick.follow(AgeClock(since: since, scale: .age)) { lastTick = $0 }
            }
    }
}

/// Every button that draws its own look: its label as given, with a faint fill while the pointer is on it, so anything
/// clickable reads as clickable before it is clicked. The rule: a label with padding of its own is its fill's shape; a
/// bare label standing alone (a link, a disclosure, a tool's row: `bare`) has its fill stand `bareBleed` off it, 8 pt
/// sideways and 4 pt above and below, past its layout, so no neighbour moves; and a bare label in a tab bar or right
/// beside another button, or one with an opaque face of its own (`over`), lightens instead and takes no fill, so neither
/// a fill nor a click can reach its neighbour. What lights is what a click takes. The fill goes behind the label, so a
/// label that marks itself selected keeps its own fill on top. `id` is the button's accessibility identifier, which a
/// test launch names to draw it hovered.
struct HoverButtonStyle: ButtonStyle {
    var radius: CGFloat = 6
    /// Lightens instead of taking a fill: a label with an opaque face of its own (a raised chip, a filled button), a tab,
    /// which its underline marks, a word right beside another button in a row (a sheet's Cancel, a folder's Forget), or
    /// a segment of a track whose chosen segment carries the plate, so a track never shows two plates at once. A row of
    /// a list (the sidebar's) takes the ordinary fill, weaker than its selected row's plate.
    var over = false
    /// How much an `over` label lightens.
    var lift = Theme.hoverLift
    /// A label with no padding of its own, such as a word alone: the fill stands `bareBleed` off it.
    var bare = false
    /// The sides of a bare label that sit on its column's edge: there its fill ends on the label, bleeding only
    /// inward, so no fill crosses the edge the column's fields and text keep.
    var flush: Edge.Set = []
    var id: String?

    /// A test launch's `-CC_HOVER` names `id`: what has it draws as if the pointer were on it, a row's "…" included.
    static func forced(_ id: String) -> Bool {
        #if DEBUG
            RenderWindow.hovered.contains(id)
        #else
            false
        #endif
    }

    /// How far a bare label's fill stands off it: sideways, and above and below.
    static let bareBleed = CGSize(width: 8, height: 4)

    func makeBody(configuration: Configuration) -> some View {
        Hovered(configuration: configuration, radius: radius, over: over, lift: lift,
                bleed: bare ? Self.bareBleed : .zero, flush: flush, id: id)
    }

    private struct Hovered: View {
        let configuration: Configuration
        let radius: CGFloat
        let over: Bool
        let lift: Double
        let bleed: CGSize
        let flush: Edge.Set
        let id: String?
        @State private var hovering = false
        @Environment(\.isEnabled) private var enabled

        var body: some View {
            let off = !enabled || forcedState("disabled")
            let lit = (hovering || forced) && !off
            configuration.label
                .disabled(forcedState("disabled"))
                .background(lit && !over ? Theme.hover : .clear, in: plate)
                .brightness(lit && over ? lift : 0)
                .opacity(off ? Theme.disabledOpacity : configuration.isPressed || forcedState("pressed") ? 0.7 : 1)
                // What lights is what a click takes: the button's whole frame, its rounded-off corners included, so a
                // corner that lit never leaves a click to whatever lies under it. A bare label's fill reaches past its
                // frame, which takes no click, so it does not light there either.
                .contentShape(Rectangle())
                // The keyboard's focus ring takes the hover's shape too: square on a row that fills its list edge to
                // edge, never a rounded pill against the list's straight sides.
                .contentShape(.focusEffect, plate)
                .onHover { hovering = $0 }
        }

        private var plate: Plate { Plate(radius: radius, bleed: bleed, flush: flush) }

        private var forced: Bool { id.map(HoverButtonStyle.forced) ?? false }

        /// A test launch's `-CC_FORCE` names this button in `state`.
        private func forcedState(_ state: String) -> Bool {
            #if DEBUG
                id.flatMap { RenderWindow.forced[$0] } == state
            #else
                false
            #endif
        }
    }
}

/// A hover fill: the label's frame grown by `bleed`, but not past a `flush` side, its corners rounded by `radius`.
private struct Plate: Shape {
    let radius: CGFloat
    let bleed: CGSize
    let flush: Edge.Set

    func path(in rect: CGRect) -> Path {
        let leading = flush.contains(.leading) ? 0 : bleed.width
        let trailing = flush.contains(.trailing) ? 0 : bleed.width
        return RoundedRectangle(cornerRadius: radius).path(
            in: CGRect(
                x: rect.minX - leading, y: rect.minY - bleed.height,
                width: rect.width + leading + trailing, height: rect.height + 2 * bleed.height))
    }
}

extension ButtonStyle where Self == HoverButtonStyle {
    /// `HoverButtonStyle`.
    static func hover(
        radius: CGFloat = 6, over: Bool = false, lift: Double = Theme.hoverLift, bare: Bool = false,
        flush: Edge.Set = [], id: String? = nil
    ) -> HoverButtonStyle {
        HoverButtonStyle(radius: radius, over: over, lift: lift, bare: bare, flush: flush, id: id)
    }
}

/// One exclusive choice among a few options in one rounded track: the chosen segment carries the only plate, and
/// hovering another lightens its label. The segments' corners are the track's less its inset, so the two are concentric.
/// `segment` draws an option's label, given whether it is the chosen one.
struct SegmentedTrack<Option: Equatable, Segment: View>: View {
    let options: [Option]
    let chosen: Option?
    /// The track's own fill, under its segments.
    let fill: Color
    /// In a panel, the first segment's row there (`panelRow`): the segments are its rows from it, left to right, the
    /// chosen one never lit, and the panel's light stands for the pointer's lightening.
    var firstPanelRow: Int? = nil
    let id: (Option) -> String
    /// Whether clicking an option chooses it: any but the chosen one, unless the caller offers that too (an effort
    /// marked as the model's default, which choosing makes the person's own).
    var offers: ((Option) -> Bool)? = nil
    let choose: (Option) -> Void
    @ViewBuilder let segment: (Option, Bool) -> Segment

    var body: some View {
        HStack(spacing: trackInset) {
            ForEach(Array(options.enumerated()), id: \.offset) { offset, option in
                let isChosen = option == chosen
                let offered = offers?(option) ?? !isChosen
                Button {
                    if offered { choose(option) }
                } label: {
                    // As wide as its chosen look whatever is chosen, so choosing moves no segment's label.
                    ZStack {
                        segment(option, true).hidden()
                        segment(option, isChosen)
                    }
                        .background(isChosen ? Theme.selected : .clear, in: RoundedRectangle(cornerRadius: trackRadius - trackInset))
                        .contentShape(Rectangle())
                }
                .accessibilityIdentifier(id(option))
                .accessibilityAddTraits(isChosen ? .isSelected : [])
                .modifier(
                    SegmentLight(
                        panelRow: firstPanelRow.map { $0 + offset }, id: id(option), chosen: isChosen, offered: offered,
                        choose: { choose(option) }))
            }
        }
        .padding(trackInset)
        .background(fill, in: RoundedRectangle(cornerRadius: trackRadius))
        .overlay(RoundedRectangle(cornerRadius: trackRadius).strokeBorder(Theme.hairline))
    }
}

/// A track segment's light: in a panel, the panel's (`panelRow`, at `panelRow`), the chosen segment never lit; else
/// the pointer's own lightening, as a dim unchosen label needs more lift than a bright one to show it is under it.
private struct SegmentLight: ViewModifier {
    let panelRow: Int?
    let id: String
    let chosen: Bool
    let offered: Bool
    let choose: () -> Void

    func body(content: Content) -> some View {
        if let panelRow {
            content.buttonStyle(.panelRow).panelRow(panelRow, id: id, choosable: offered, lifts: true, choose: choose)
        } else {
            content.buttonStyle(.hover(radius: trackRadius - trackInset, over: true, lift: Theme.segmentLift, id: id))
        }
    }
}

private let trackRadius: CGFloat = 7
private let trackInset: CGFloat = 2

extension EnvironmentValues {
    /// Raised each time Return in the run list asks for the open run's reply box.
    @Entry var replyFocusRequest = 0
    /// How far from the window's edges a pane's header starts and ends: clear of the window's own controls.
    @Entry var headerClearance = EdgeInsets()
}

extension FocusedValues {
    /// The composer is using Esc itself: its commands list is open or its field has focus.
    @Entry var composerHoldsEscape: Bool?
}

/// Where a panel stands across its bounds, every panel's one rule: as wide as `widest` where the bounds have the room,
/// never past either of their sides, and its leading edge on its control's (`leading`), moved only as far as it must to
/// stay inside.
enum PanelPlace {
    /// The least a panel keeps from the pane's top and bottom.
    static let margin: CGFloat = 8

    /// A panel `widest` wide that would start at `leading`: as wide as the bounds allow, at `leading` where it fits,
    /// else moved toward their leading side just enough to end at their trailing side.
    static func place(widest: CGFloat, leading: CGFloat, box: CGRect) -> (x: CGFloat, width: CGFloat) {
        // Before the bounds are measured, the panel as it is drawn.
        guard box.width > 0 else { return (leading, widest) }
        let width = min(widest, box.width)
        return (min(max(leading, box.minX), box.maxX - width), width)
    }
}

extension View {
    /// A control that opens a panel, and toggles it itself. While the panel is open, a click anywhere in the window
    /// outside the control and the panel closes it, and the click still does what it does there (another control's
    /// opens that control's panel); Esc closes it and goes no further, so a second Esc does what Esc did before; a
    /// scroll or a window's resize closes it too. While a field of the panel's own (`panelField`) has the keyboard,
    /// every key is the field's.
    func closesPanel(_ open: Binding<Bool>) -> some View { modifier(ClosesPanel(open: open, measuresControl: true)) }

    /// The panel a `closesPanel` control opens: a click on it keeps it open.
    func openPanel() -> some View { modifier(OpenPanel()) }

    /// A field of an open panel that takes the panel's keys while `focused`: Esc, Return and the arrows are its own,
    /// not the panel's light's or its closing's. Every other panel's keys are as `closesPanel` says.
    func panelField(_ focused: FocusState<Bool>.Binding) -> some View { modifier(PanelField(focused: focused)) }

    /// A field that appears because of a choice just made takes the keyboard as it appears, so what is typed next
    /// goes into it and not into the field that had the keyboard before.
    func takesKeyboardWhenShown() -> some View { modifier(TakesKeyboardWhenShown()) }

    /// The same, for a field that reads its own focus: it takes the keyboard through `focused`, a turn after it
    /// appears. The choice that showed it closed a panel in the same update, and AppKit settles the keyboard after that
    /// close (on the field that had it, or the window's first control), over a focus set before.
    func takesKeyboardWhenShown(_ focused: FocusState<Bool>.Binding) -> some View {
        self.focused(focused).onAppear { DispatchQueue.main.async { focused.wrappedValue = true } }
    }
}

/// Where a control and its open panel are, in the window's coordinates (top-left origin).
final class PanelFrames {
    var control: CGRect = .zero
    var panel: CGRect = .zero
}

extension EnvironmentValues {
    @Entry var panelFrames: PanelFrames?
    /// The agents (the sidebar, the list, a run) are on screen: false while Settings covers them, kept as they were.
    @Entry var agentsShown = true
}

private struct ClosesPanel: ViewModifier {
    @Binding var open: Bool
    /// A panel closes when Settings covers the agents it opened over.
    @Environment(\.agentsShown) private var agentsShown
    /// The view it modifies is the control; else the panel says where its control is (the window's row menu).
    let measuresControl: Bool
    @State private var frames = PanelFrames()
    @State private var light = PanelLight()
    @State private var monitor: Any?
    @State private var resizing: NSObjectProtocol?
    /// The window's asks: while one is up (a name asked for from inside a panel), the clicks and keys are the ask's,
    /// and the panel waits behind it.
    @Environment(ConfirmationQueue.self) private var asks: ConfirmationQueue?

    func body(content: Content) -> some View {
        content
            .environment(\.panelFrames, frames)
            .environment(\.panelLight, light)
            .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: {
                if measuresControl { frames.control = $0 }
            }
            .onChange(of: open, initial: true) { _, isOpen in
                stop()
                if isOpen { start() }
            }
            .onChange(of: agentsShown) { _, shown in if !shown { open = false } }
            .onDisappear(perform: stop)
    }

    private func start() {
        let frames = frames
        let close = $open
        let light = light
        let asks = asks
        light.opened()
        // The window the panel opened in, which had the keyboard: keys typed in another window are that window's.
        let window = NSApp.keyWindow
        monitor = NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown, .keyDown, .scrollWheel]) {
            event in
            if MainActor.assumeIsolated({ asks?.isAsking == true }) { return event }
            // A scroll elsewhere closes it: an open panel never moves with, or hides behind, what scrolls under it. A
            // scroll over the panel scrolls the panel.
            if event.type == .scrollWheel {
                if let view = event.window?.contentView {
                    let point = view.convert(event.locationInWindow, from: nil)
                    let top = CGPoint(x: point.x, y: view.isFlipped ? point.y : view.bounds.height - point.y)
                    if frames.panel.contains(top) { return event }
                }
                close.wrappedValue = false
                return event
            }
            if event.type == .keyDown {
                guard event.window === window else { return event }
                // While a field of the panel's own (`panelField`) has the keyboard, every key is the field's.
                if MainActor.assumeIsolated({ light.fieldHasKeyboard }) { return event }
                if event.keyCode == 53 {  // Esc
                    close.wrappedValue = false
                    return nil
                }
                // ↓ ↑ Return move and choose the panel's light (`PanelLight`); held with ⌘, ⌃, ⇧ or ⌥ (Kit's
                // `MenuLight.key`), or over a panel whose rows take no keys here, they go on as before.
                let held = event.modifierFlags
                guard let key = MenuLight.key(
                        code: event.keyCode, command: held.contains(.command), control: held.contains(.control),
                        shift: held.contains(.shift), option: held.contains(.option)),
                    let outcome = MainActor.assumeIsolated({ light.press(key) })
                else { return event }
                if outcome == .close { close.wrappedValue = false }
                return nil
            }
            if let view = event.window?.contentView {
                let point = view.convert(event.locationInWindow, from: nil)
                let top = CGPoint(x: point.x, y: view.isFlipped ? point.y : view.bounds.height - point.y)
                if !frames.control.contains(top), !frames.panel.contains(top) { close.wrappedValue = false }
            }
            return event
        }
        // A resize of a window of the app's own closes it too: a panel never outlives the geometry it was placed from.
        // Not the system's helper windows, which a text field that takes the keyboard shows and sizes (its input
        // and suggestion bubbles): those can't become main.
        resizing = NotificationCenter.default.addObserver(
            forName: NSWindow.didResizeNotification, object: nil, queue: .main
        ) { note in
            let resized = (note.object as? NSWindow).map(ObjectIdentifier.init)
            MainActor.assumeIsolated {
                guard NSApp.windows.first(where: { ObjectIdentifier($0) == resized })?.canBecomeMain == true else { return }
                close.wrappedValue = false
            }
        }
    }

    private func stop() {
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
        resizing.map(NotificationCenter.default.removeObserver)
        resizing = nil
    }
}

private struct PanelField: ViewModifier {
    let focused: FocusState<Bool>.Binding
    @Environment(\.panelLight) private var light

    func body(content: Content) -> some View {
        content
            .onChange(of: focused.wrappedValue, initial: true) { _, has in light?.fieldHasKeyboard = has }
            .onDisappear { light?.fieldHasKeyboard = false }
    }
}

private struct TakesKeyboardWhenShown: ViewModifier {
    @FocusState private var focused: Bool

    func body(content: Content) -> some View { content.takesKeyboardWhenShown($focused) }
}

private struct OpenPanel: ViewModifier {
    @Environment(\.panelFrames) private var frames

    func body(content: Content) -> some View {
        content.onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { frames?.panel = $0 }
            #if DEBUG
                // A panel outside the view its `closesPanel` wraps can't say where it is, so every click on it reads
                // as outside and closes it before it lands. Said, and a render of it fails.
                .onAppear {
                    if frames == nil {
                        RenderWindow.fault("a panel is drawn outside the closesPanel that closes it: clicks on it close it")
                    }
                }
            #endif
    }
}

extension EnvironmentValues {
    /// The open panel's light, which its `closesPanel` keeps.
    @Entry var panelLight: PanelLight?
}

/// An open panel's one lit row (Kit's `MenuLight`): the pointer entering a row lights it, ↓ ↑ move the light, Return
/// chooses the lit row. Each row of the panel says where it stands and whether it can be chosen (`panelRow`).
@Observable @MainActor final class PanelLight {
    /// A lit row's plate, in every panel.
    static let fill = Theme.selected

    private(set) var light = MenuLight()
    /// The row the keys last lit, which a panel that scrolls brings into sight (`PanelScroll`).
    private(set) var keyed: Int?
    @ObservationIgnored private var rows: [Int: Row] = [:]
    /// A field of the panel's own (`panelField`) has the keyboard.
    @ObservationIgnored var fieldHasKeyboard = false

    private struct Row {
        let id: String
        let enabled: Bool
        let choose: () -> Void
    }

    /// The panel opened: nothing is lit, and no field of it has the keyboard.
    func opened() {
        light = MenuLight()
        keyed = nil
        fieldHasKeyboard = false
    }

    func register(_ index: Int, id: String, enabled: Bool, choose: @escaping () -> Void) {
        rows[index] = Row(id: id, enabled: enabled, choose: choose)
    }

    /// Row `index` went away, and its light with it: the panel changed (a run's Model panel for its Mode panel).
    func remove(_ index: Int, id: String) {
        if rows[index]?.id == id { rows[index] = nil }
        if light.lit == index { opened() }
    }

    func enter(_ index: Int) { light.enter(index, enabled: enabled) }

    /// `key`, pressed while the panel is open; nil when no row of it takes keys here (the slash panel's are the reply
    /// box's), so the key goes on.
    func press(_ key: MenuLight.Key) -> MenuLight.Outcome? {
        guard !rows.isEmpty else { return nil }
        let outcome = light.press(key, enabled: enabled)
        switch outcome {
        case .moved: keyed = light.lit
        case .choose(let row): rows[row]?.choose()
        case .close: break
        }
        return outcome
    }

    private var enabled: [Bool] {
        (0..<((rows.keys.max() ?? -1) + 1)).map { rows[$0]?.enabled ?? false }
    }
}

extension View {
    /// Row `index` of an open panel, in reading order, which `choose` chooses as a click does: lit when it is the panel's
    /// light, its `MenuRow` filled or, on a track's segment, by lightening (`lifts`). A disabled row, or one `choosable` says
    /// isn't, never lights. A test launch's `-CC_HOVER id` lights it as the pointer does.
    func panelRow(_ index: Int, id: String, choosable: Bool = true, lifts: Bool = false, choose: @escaping () -> Void)
        -> some View
    {
        modifier(PanelRow(index: index, id: id, choosable: choosable, lifts: lifts, choose: choose))
    }
}

private struct PanelRow: ViewModifier {
    let index: Int
    let id: String
    let choosable: Bool
    let lifts: Bool
    let choose: () -> Void
    @Environment(\.panelLight) private var light
    @Environment(\.isEnabled) private var isEnabled

    func body(content: Content) -> some View {
        let lit = light?.light.lit == index
        content
            // A menu's row draws its own light inside its gap (`MenuRow`); a track's segment lightens.
            .environment(\.menuRowLit, lit && !lifts)
            .brightness(lit && lifts ? Theme.segmentLift : 0)
            .onHover { if $0 { light?.enter(index) } }
            .onAppear {
                register()
                if HoverButtonStyle.forced(id) { light?.enter(index) }
            }
            .onChange(of: choosable && isEnabled) { register() }
            .onDisappear { light?.remove(index, id: id) }
            .id(index)
    }

    private func register() { light?.register(index, id: id, enabled: choosable && isEnabled, choose: choose) }
}

/// A computer's mark, wherever the app names a computer (a run's header, the run list's filter, Settings): a Mac's
/// screen, or a Linux server's stacked racks, as the daemon says its system is (`os`, "macos" or "linux"); a computer
/// that hasn't said draws as a Mac. The words beside it name it; the mark itself is not read out.
struct ComputerMark: View {
    let os: String?
    var size: CGFloat = 11

    var body: some View {
        Image(systemName: os == "linux" ? "server.rack" : "desktopcomputer")
            .font(.system(size: size))
            .accessibilityHidden(true)
    }
}

/// The one menu's measures: its card's inset and corners, and its rows' heights, gap and lit corners.
enum MenuMetrics {
    /// The card's padding round its rows.
    static let inset: CGFloat = 5
    static let cardRadius: CGFloat = 9
    /// A row of one line; one with a second line under it.
    static let rowHeight: CGFloat = 28
    static let twoLineHeight: CGFloat = 40
    /// Between a row and the one above it, so two lit rows never touch.
    static let rowGap: CGFloat = 2
    static let rowRadius: CGFloat = 5
}

extension View {
    /// Every menu's card, whatever it holds: the sheet's fill, a fine border, corners of 9, its rows 5 pt in, and one
    /// deep shadow.
    func menuCard() -> some View {
        padding(MenuMetrics.inset)
            .background {
                RoundedRectangle(cornerRadius: MenuMetrics.cardRadius).fill(Theme.sheet)
                    .shadow(color: .black.opacity(0.6), radius: 25, y: 18)
            }
            .overlay(RoundedRectangle(cornerRadius: MenuMetrics.cardRadius).strokeBorder(Color.white.opacity(0.1)))
    }
}

/// A menu card's section heading: 11 pt medium in the third text colour, 8 pt above it, 10 in, 4 below.
struct MenuHeader: View {
    let text: String

    var body: some View {
        Text(text).font(Theme.ui(11, .medium)).foregroundStyle(Theme.text3)
            .padding(.top, 8)
            .padding(.horizontal, 10)
            .padding(.bottom, 4)
    }
}

/// A menu card's divider: a fine white line, 4 pt in from the card's sides.
struct MenuDivider: View {
    var body: some View { Rectangle().fill(Theme.hairline).frame(height: 1).padding(.horizontal, 4) }
}

extension View {
    /// A field in a menu card, to filter or name: 32 pt tall, 10 pt in, a faint fill and border, corners of 8.
    func menuField() -> some View {
        padding(.horizontal, 10)
            .frame(height: 32)
            .background(Color.white.opacity(0.04), in: RoundedRectangle(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.hairline))
    }
}

/// The one menu's row: what it says on one line, and under it, where the row has one, a second line in 11.5 pt, muted;
/// 28 pt tall, 40 with the second line, 10 pt in at its sides, 2 pt below the row above. In a menu that chooses
/// (`chosen` given) a check slot leads every row, checked on the chosen one; what is chosen is never filled. Where the
/// row has one, a trailing view stands at its trailing edge, centred on its lines. The row the pointer or keys light
/// (`panelRow`) is filled, at corners of 5.
struct MenuRow<Content: View, Second: View, Trailing: View>: View {
    var chosen: Bool?
    let content: Content
    let second: Second
    let trailing: Trailing
    private let twoLines: Bool
    @Environment(\.menuRowLit) private var lit

    init(
        chosen: Bool? = nil, @ViewBuilder content: () -> Content, @ViewBuilder second: () -> Second,
        @ViewBuilder trailing: () -> Trailing
    ) {
        self.init(chosen: chosen, twoLines: true, content: content(), second: second(), trailing: trailing())
    }

    private init(chosen: Bool?, twoLines: Bool, content: Content, second: Second, trailing: Trailing) {
        self.chosen = chosen
        self.twoLines = twoLines
        self.content = content
        self.second = second
        self.trailing = trailing
    }

    var body: some View {
        HStack(spacing: 8) {
            if let chosen {
                Image(systemName: "checkmark").font(.system(size: 9.5, weight: .bold)).foregroundStyle(Theme.text)
                    .frame(width: 12)
                    .opacity(chosen ? 1 : 0)
                    .accessibilityHidden(true)
            }
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 8) { content }
                if twoLines { second.font(Theme.ui(11.5)).foregroundStyle(Theme.text3).lineLimit(1) }
            }
            Spacer(minLength: 0)
            trailing
        }
        .padding(.horizontal, 10)
        .frame(minHeight: twoLines ? MenuMetrics.twoLineHeight : MenuMetrics.rowHeight)
        .background(lit ? PanelLight.fill : .clear, in: RoundedRectangle(cornerRadius: MenuMetrics.rowRadius))
        .contentShape(Rectangle())
        .padding(.top, MenuMetrics.rowGap)
    }
}

extension MenuRow where Trailing == EmptyView {
    init(chosen: Bool? = nil, @ViewBuilder content: () -> Content, @ViewBuilder second: () -> Second) {
        self.init(chosen: chosen, twoLines: true, content: content(), second: second(), trailing: EmptyView())
    }
}

extension MenuRow where Second == EmptyView {
    init(chosen: Bool? = nil, @ViewBuilder content: () -> Content, @ViewBuilder trailing: () -> Trailing) {
        self.init(chosen: chosen, twoLines: false, content: content(), second: EmptyView(), trailing: trailing())
    }
}

extension MenuRow where Second == EmptyView, Trailing == EmptyView {
    init(chosen: Bool? = nil, @ViewBuilder content: () -> Content) {
        self.init(chosen: chosen, twoLines: false, content: content(), second: EmptyView(), trailing: EmptyView())
    }
}

extension EnvironmentValues {
    /// The `MenuRow` under it is its panel's lit row (`panelRow`).
    @Entry var menuRowLit = false
}

/// A panel row's button: the panel's light is its only plate (`panelRow`); disabled it dims, pressed it fades.
struct PanelRowStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View { Label(configuration: configuration) }

    private struct Label: View {
        let configuration: Configuration
        @Environment(\.isEnabled) private var enabled

        var body: some View {
            configuration.label
                .opacity(!enabled ? Theme.disabledOpacity : configuration.isPressed ? 0.7 : 1)
                .contentShape(Rectangle())
        }
    }
}

extension ButtonStyle where Self == PanelRowStyle {
    /// `PanelRowStyle`.
    static var panelRow: PanelRowStyle { PanelRowStyle() }
}

/// A menu row's button in a list whose keys move its choice, not a light (Start's folders, the Folder box): the
/// pointer lights its `MenuRow` as a panel's light does, and a test launch's `-CC_HOVER id` does too; disabled it dims,
/// pressed it fades.
struct MenuRowButtonStyle: ButtonStyle {
    let id: String

    func makeBody(configuration: Configuration) -> some View { Label(configuration: configuration, id: id) }

    private struct Label: View {
        let configuration: Configuration
        let id: String
        @State private var hovering = false
        @Environment(\.isEnabled) private var enabled

        var body: some View {
            configuration.label
                .environment(\.menuRowLit, (hovering || HoverButtonStyle.forced(id)) && enabled)
                .opacity(!enabled ? Theme.disabledOpacity : configuration.isPressed ? 0.7 : 1)
                .contentShape(Rectangle())
                .onHover { hovering = $0 }
        }
    }
}

extension ButtonStyle where Self == MenuRowButtonStyle {
    /// `MenuRowButtonStyle`.
    static func menuRow(id: String) -> MenuRowButtonStyle { MenuRowButtonStyle(id: id) }
}

/// A panel's rows that scroll: the row the keys light comes into sight.
struct PanelScroll<Content: View>: View {
    @ViewBuilder let content: Content
    @Environment(\.panelLight) private var light

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView { content }
                .onChange(of: light?.keyed) { _, row in if let row { proxy.scrollTo(row) } }
        }
    }
}

/// Each "…" panel act's one symbol, chosen once for both apps: the iPhone's menus draw the same act with the Phosphor
/// icon named beside it.
enum PanelSymbol {
    /// iPhone: `power`.
    static let endRun = "power"
    /// iPhone: `trash`.
    static let remove = "trash"
    /// iPhone: `arrows-in`.
    static let compact = "arrow.down.right.and.arrow.up.left"
    /// iPhone: `eraser`.
    static let clear = "eraser"
    /// iPhone: `chat-text`.
    static let comment = "text.bubble"
    /// iPhone: `copy`.
    static let copy = "doc.on.doc"
    /// iPhone: `sign-in`.
    static let signIn = "person.badge.key"
    static let signOut = "rectangle.portrait.and.arrow.right"
}

/// One act in a "…" panel, on the Model and Mode panels' row: an 11 pt mark in a 14 pt slot and the 12.5 pt label in a
/// `MenuRow`, lit as every panel's row is (`panelRow`, row `index`). A destructive act's mark and label are red.
struct PanelItem: View {
    let index: Int
    let title: String
    let symbol: String
    var destructive = false
    let id: String
    let perform: () -> Void

    var body: some View {
        Button(action: perform) {
            MenuRow {
                Image(systemName: symbol).font(.system(size: 11))
                    .foregroundStyle(destructive ? Theme.red : Theme.text3)
                    .frame(width: 14)
                    .accessibilityHidden(true)
                Text(title).font(Theme.ui(12.5)).foregroundStyle(destructive ? Theme.red : Theme.text).lineLimit(1)
            }
        }
        .buttonStyle(.panelRow)
        .panelRow(index, id: id, choose: perform)
        .accessibilityIdentifier(id)
    }
}

extension View {
    /// A box that sends to an agent (the reply box, a card's reason, a hunk's comment): Return sends; ⇧Return and
    /// ⌥Return add a line where the caret is (`ComposeProse.returnAddsLine`).
    func sendsOnReturn(_ send: @escaping () -> Void) -> some View {
        onKeyPress(.return, phases: .down) { press in
            let held = press.modifiers
            guard ComposeProse.returnAddsLine(
                shift: held.contains(.shift), option: held.contains(.option), command: held.contains(.command),
                control: held.contains(.control))
            else { return .ignored }
            NSApp.sendAction(#selector(NSResponder.insertNewlineIgnoringFieldEditor(_:)), to: nil, from: nil)
            return .handled
        }
        .onSubmit(send)
    }

    /// A right-click on this view, at the pointer's place in the window's coordinates (top-left origin), the coordinates
    /// `.global` frames use. Only the right button stops here: every other click, the hover and the keyboard reach the
    /// view as before, and nothing else draws a menu for it.
    func onRightClick(_ perform: @escaping (CGPoint) -> Void) -> some View {
        overlay(RightClick(perform: perform))
    }
}

private struct RightClick: NSViewRepresentable {
    let perform: (CGPoint) -> Void

    func makeNSView(context: Context) -> Catcher { Catcher(perform: perform) }
    func updateNSView(_ view: Catcher, context: Context) { view.perform = perform }

    final class Catcher: NSView {
        var perform: (CGPoint) -> Void

        init(perform: @escaping (CGPoint) -> Void) {
            self.perform = perform
            super.init(frame: .zero)
        }

        required init?(coder: NSCoder) { nil }

        override func hitTest(_ point: NSPoint) -> NSView? {
            NSApp.currentEvent?.type == .rightMouseDown ? super.hitTest(point) : nil
        }

        override func rightMouseDown(with event: NSEvent) {
            guard let content = window?.contentView else { return }
            let point = content.convert(event.locationInWindow, from: nil)
            perform(CGPoint(x: point.x, y: content.isFlipped ? point.y : content.bounds.height - point.y))
        }
    }
}

/// The one "…" panel the main window shows for a row: where it was asked for, what it holds, and whose it is. A row
/// opens it at the click with what it knows (its "…"'s frame, or a point where a right-click landed) and the items it
/// builds over its own subject, so the panel never looks its row up.
@Observable @MainActor final class RowMenu {
    struct Item: Identifiable {
        let id: String
        let title: String
        let symbol: String
        let destructive: Bool
        let perform: () -> Void
    }

    struct Shown {
        let owner: String
        let rect: CGRect
        let items: [Item]
    }

    private(set) var shown: Shown?

    /// Opens `items` by `rect`, in the window's coordinates; the same control asking again closes it.
    func toggle(_ rect: CGRect, owner: String, items: [Item]) {
        shown = shown?.owner == owner && shown?.rect == rect ? nil : Shown(owner: owner, rect: rect, items: items)
    }

    func close() { shown = nil }
}

extension View {
    /// The main window's row menu (`RowMenu`), drawn over everything in the window: its leading edge on the rect it was
    /// opened by, below it where it fits, above it where it would cross the window's bottom, never past the window's
    /// sides (`PanelPlace`). It closes as
    /// every panel does (`closesPanel`), the rect being its control.
    func rowMenuHost(_ menu: RowMenu) -> some View { modifier(RowMenuHost(menu: menu)) }
}

private struct RowMenuHost: ViewModifier {
    let menu: RowMenu
    /// At least this wide, and as wide as its longest item, so no item's words are cut.
    static let minWidth: CGFloat = 180
    @State private var size: CGSize = CGSize(width: minWidth, height: 0)

    func body(content: Content) -> some View {
        content
            .environment(menu)
            .overlay(alignment: .topLeading) {
                GeometryReader { window in
                    if let shown = menu.shown {
                        let bounds = window.frame(in: .global)
                        let corner = place(shown.rect, in: bounds)
                        VStack(alignment: .leading, spacing: 0) {
                            ForEach(Array(shown.items.enumerated()), id: \.element.id) { index, item in
                                PanelItem(
                                    index: index, title: item.title, symbol: item.symbol, destructive: item.destructive,
                                    id: item.id
                                ) {
                                    menu.close()
                                    item.perform()
                                }
                            }
                        }
                        .frame(minWidth: Self.minWidth - 2 * MenuMetrics.inset)
                        .fixedSize()
                        .menuCard()
                        .onGeometryChange(for: CGSize.self) { $0.size } action: { size = $0 }
                        .modifier(ControlRect(rect: shown.rect, window: bounds))
                        .openPanel()
                        .offset(x: corner.x - bounds.minX, y: corner.y - bounds.minY)
                    }
                }
            }
            .modifier(ClosesPanel(
                open: Binding(get: { menu.shown != nil }, set: { if !$0 { menu.close() } }), measuresControl: false))
    }

    /// Its top leading corner: its leading edge on the rect's, below it where it fits, above it where it would cross
    /// the window's bottom, inside the window's sides. A right-click's point: its top leading corner there.
    private func place(_ rect: CGRect, in bounds: CGRect) -> CGPoint {
        let point = rect.size == .zero
        let gap = point ? 0 : Theme.panelGap
        let below = rect.maxY + gap
        let above = rect.minY - gap - size.height
        let y = below + size.height <= bounds.maxY || above < bounds.minY ? below : above
        return CGPoint(x: PanelPlace.place(widest: size.width, leading: rect.minX, box: bounds).x, y: y)
    }
}

/// Tells the open panel's closing where its control is: the rect it was opened by.
private struct ControlRect: ViewModifier {
    let rect: CGRect
    let window: CGRect
    @Environment(\.panelFrames) private var frames

    func body(content: Content) -> some View {
        content
            .onAppear { frames?.control = rect }
            .onChange(of: rect) { _, rect in frames?.control = rect }
            #if DEBUG
                // For a render's check: once, where the panel stands beside its control and inside its window.
                .task(id: rect) {
                    guard RenderWindow.isRendering else { return }
                    try? await Task.sleep(for: .milliseconds(300))
                    guard let panel = frames?.panel else { return }
                    func text(_ r: CGRect) -> String { "\(Int(r.minX)) \(Int(r.minY)) \(Int(r.width)) \(Int(r.height))" }
                    print("panel: \(text(panel)); control: \(text(rect)); window: \(text(window))")
                }
            #endif
    }
}

/// One tab: its word, and a count beside it when it has one, underlined while it is the one shown: a run's
/// Conversation and Changes, and Settings' panes. Every tab is a button; only the shown one reads selected, and each
/// keeps the header trait the single Conversation tab had.
struct UnderlineTab: View {
    let title: String
    /// What a driver finds the tab by.
    let id: String
    let selected: Bool
    let count: Int?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            VStack(spacing: 7) {
                HStack(spacing: 5) {
                    Text(title).foregroundStyle(selected ? Theme.text : Theme.text3)
                    if let count { Text(verbatim: "\(count)").foregroundStyle(Theme.text3) }
                }
                .font(Theme.ui(12.5, .medium))
                Rectangle().fill(selected ? Theme.text : Color.clear).frame(height: 1.5)
            }
            .fixedSize()
        }
        .buttonStyle(.hover(over: true, id: id))
        .accessibilityAddTraits(selected ? [.isHeader, .isSelected] : [.isHeader])
        .accessibilityIdentifier(id)
    }
}

/// `text` on the general pasteboard as plain text, in place of whatever it held: every Copy in the app.
@MainActor func copyToPasteboard(_ text: String) {
    NSPasteboard.general.clearContents()
    NSPasteboard.general.setString(text, forType: .string)
}

/// A block's Copy, as the iPhone's code block draws it: the glyph alone at the block's trailing edge, centred on the
/// whole block it copies, a check in green for a moment once it has. The block keeps `slot` clear at its trailing edge
/// for its whole height, so the end of a line, a command's above all, is never under it.
struct CopyButton: View {
    let text: String
    let id: String
    @State private var copied = false
    @State private var reset: Task<Void, Never>?

    /// The text's trailing room: the button, its inset from the block's edge, and a gap before the text.
    static let slot: CGFloat = size + inset + 8
    private static let size: CGFloat = 24
    private static let inset: CGFloat = 4

    var body: some View {
        Button(action: copy) {
            Image(systemName: copied ? "checkmark" : PanelSymbol.copy)
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(copied ? Theme.green : Theme.text3)
                .frame(width: Self.size, height: Self.size)
                .contentShape(Rectangle())
        }
        .buttonStyle(.hover(radius: 5, id: id))
        .padding(.trailing, Self.inset)
        .help("Copy")
        .accessibilityLabel("Copy")
        .accessibilityValue(copied ? "Copied" : "")
        .accessibilityIdentifier(id)
        #if DEBUG
            .onAppear { if RenderWindow.copied.contains(id) { copied = true } }
        #endif
    }

    private func copy() {
        copyToPasteboard(text)
        reset?.cancel()
        copied = true
        reset = Task {
            try? await Task.sleep(for: .seconds(CC.duration.toast))
            guard !Task.isCancelled else { return }
            copied = false
        }
    }
}

/// The keyboard given by code to a field that holds words: the caret goes at the end of them, a turn later, once AppKit's
/// select-all for a field made first responder by code is done, so the next key adds to the words and never replaces
/// them. Not where the caret was: a vertical field's selection binding does not report the caret, so where it was
/// cannot be known.
@MainActor
func giveKeyboard(_ focused: FocusState<Bool>.Binding, caret selection: Binding<TextSelection?>, endOf text: String) {
    focused.wrappedValue = true
    let end = text.endIndex
    DispatchQueue.main.async { selection.wrappedValue = TextSelection(insertionPoint: end) }
}
