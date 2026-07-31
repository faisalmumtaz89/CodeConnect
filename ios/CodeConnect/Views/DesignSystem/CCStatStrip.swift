import SwiftUI

// =============================================================================
//  CCStatStrip / CCWaitClock — where a number is the headline.
// =============================================================================

/// One column of a `CCStatStrip`.
struct CCStat: Identifiable, Equatable {
    let label: String
    let value: String
    /// `neutral` leaves the value in `text`. A tone is for a value that is
    /// *itself* news — a non-zero missed-decision count, a degraded encryption
    /// row — never for decoration.
    var tone: CCTone = .neutral
    /// **Nobody measured this.** Renders `CCMeasured.mark` in `textDisabled`,
    /// the same way `CCFactRow` has always rendered it — one rule, asked of
    /// `CCMeasured`, so the strip and the fact row cannot disagree about what
    /// "not measured" looks like again.
    ///
    /// Setting it is belt and braces rather than a requirement: a value that is
    /// *already* the em dash is recognised without it (see
    /// `CCMeasured.isUnmeasured`). Say it anyway where you know — it is what
    /// VoiceOver reads, and it survives the value later becoming a number.
    var isUnmeasured: Bool = false
    /// What VoiceOver says instead of the raw value, when the value is a
    /// compacted string a screen reader cannot pronounce (`47s`, `1.2k`).
    var spokenValue: String?

    var id: String { label }

    init(
        _ label: String,
        value: String,
        tone: CCTone = .neutral,
        isUnmeasured: Bool = false,
        spokenValue: String? = nil
    ) {
        self.label = label
        self.value = value
        self.tone = tone
        self.isUnmeasured = isUnmeasured
        self.spokenValue = spokenValue
    }

    /// A column nobody could measure. The mark and the state are set together,
    /// so the pair cannot come apart.
    static func unmeasured(_ label: String) -> CCStat {
        CCStat(label, value: CCMeasured.mark, isUnmeasured: true)
    }
}

/// Two to four equal columns of `micro` label over a `title` `mono` value,
/// divided by 1pt `border` verticals, inside one `CCCard`.
///
/// The strip exists because a number that matters — decisions missed, decisions
/// cleared, reconnects — deserves to be read at a glance and compared against
/// its neighbours, which a list of label/value rows does not allow. Equal
/// columns are the whole point: unequal ones would rank the numbers, and the
/// ranking would be an accident of string length.
struct CCStatStrip: View {
    let stats: [CCStat]

    @Environment(\.dynamicTypeSize) private var typeSize
    /// Two `fieldLabel` lines, 14 each. Scaled, so the reservation and the
    /// label it reserves for grow together.
    @ScaledMetric(relativeTo: .caption) private var labelBlock: CGFloat = 28

    init(_ stats: [CCStat]) {
        self.stats = stats
    }

    var body: some View {
        CCCard(padding: 0) {
            // At accessibility sizes four columns of `title` type cannot share a
            // 361pt line without one of them becoming a single character per
            // row, so the strip becomes a stack and the dividers become
            // horizontal. Same information, same order, same component.
            if typeSize.isAccessibilitySize {
                VStack(spacing: 0) {
                    ForEach(Array(stats.enumerated()), id: \.element.id) { index, stat in
                        if index > 0 { CCHairline() }
                        column(stat)
                    }
                }
            } else {
                HStack(spacing: 0) {
                    ForEach(Array(stats.enumerated()), id: \.element.id) { index, stat in
                        if index > 0 {
                            Rectangle()
                                .fill(CC.color.border)
                                .frame(width: CC.stroke.hairline)
                                .accessibilityHidden(true)
                        }
                        column(stat)
                            .frame(maxWidth: .infinity)
                    }
                }
                .fixedSize(horizontal: false, vertical: true)
            }
        }
        .accessibilityElement(children: .contain)
    }

    private func column(_ stat: CCStat) -> some View {
        VStack(alignment: .leading, spacing: CC.space.xs) {
            // Two lines are *reserved*, not merely allowed. Measured on the
            // shipped strip: `MISSED / DECISIONS` wrapped to two lines and its
            // one-line neighbours `SEQ GAPS` and `RECONNECTS` centred
            // themselves against it, so no two of the three labels shared a
            // first baseline — three numbers on a wavy line, on the screen
            // whose whole job is to be trusted.
            Text(stat.label.uppercased())
                .ccType(CC.type.fieldLabel, color: nil)
                .lineLimit(2)
                .fixedSize(horizontal: false, vertical: true)
                // Only while the columns share a line. Once the strip stacks at
                // accessibility sizes there is no neighbour to align to, and
                // the reservation would be 14pt of dead space per row.
                .frame(
                    minHeight: typeSize.isAccessibilitySize ? 0 : labelBlock,
                    alignment: .topLeading)
            Text(stat.value)
                // `title` at the mono design: these are measurements, and
                // a column of measurements that do not share a digit width is a
                // column you cannot compare down.
                //
                // **On the token, not on the view.** It shipped as
                // `.ccType(CC.type.title).monospaced()`, which is a no-op —
                // `.ccType` sets the font inside its own body and the outer
                // transform never reaches the `Text`. `THIS PASS`'s `26s`
                // measured 17.67 then 19.33 between glyphs *inside one number*,
                // and the rule is categorical: durations are monospace, always.
                // See `CCTextStyle.monospaced()`.
                .ccType(CC.type.title.monospaced())
                // **The one rule, asked rather than restated.** This line used
                // to read `stat.tone == .neutral ? CC.text.primary : …`, which
                // has no notion of "not measured" at all — so `MISSED
                // DECISIONS`'s `—` drew at #EDEDED / 16.91:1, indistinguishable
                // in weight from the two real numbers beside it, while the
                // identical state eleven rows below rendered #525252 / 2.53:1.
                // Note that unmeasured outranks tone: a value nobody took
                // cannot also be news.
                .foregroundStyle(
                    CCMeasured.color(stat.value, tone: stat.tone, flagged: stat.isUnmeasured)
                )
                .lineLimit(1)
                .minimumScaleFactor(0.7)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, CC.space.md)
        .padding(.vertical, CC.space.md)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(stat.label)
        .accessibilityValue(
            stat.spokenValue ?? CCMeasured.spoken(stat.value, flagged: stat.isUnmeasured))
    }
}

// MARK: - Wait clock

/// How long an agent has been held, in the one format the product uses for it.
///
/// Lives beside `CCStatStrip` because it is the same kind of object: a number
/// that is the headline rather than a caption. It is the only value on the Fleet
/// set in `mono` (14) rather than `monoSmall` — a deliberate one-step
/// promotion, because *how long somebody has been waiting on you* is the most
/// important number on that screen.
///
/// `warning`, monospaced digits, and it never rounds up: `4m12s` is a fact, not
/// an estimate, and a clock that jumps a whole minute at a time reads as a
/// stalled render rather than as elapsed time.
struct CCWaitClock: View {
    let since: Date
    let now: Date
    /// `"waiting"` on the card and the timeline row; nothing on a Fleet row,
    /// where the band header has already said what the number means.
    var prefix: String?
    /// **The colour is a measurement, not a mood**, so `nil` — the default —
    /// derives it from the wait itself and a caller does not get to have an
    /// opinion without a reason.
    ///
    /// It shipped defaulting to `warning`, which meant `1m03s`, `3m03s` and
    /// `5m03s` all printed the same `#F5A623`: a hue reporting nothing anybody
    /// could act on, and three restatements of the band the row sat in.
    /// The fleet row then fixed it *at the call site*, so the same 63-second
    /// wait rendered neutral on the row and amber in the accessory bar twelve
    /// hundred points below it — one fact, two colours, one screen. The rule
    /// belongs to the component.
    var tone: CCTone?
    var style: CCTextStyle = CC.type.mono

    var body: some View {
        Text(text)
            .ccType(style)
            .foregroundStyle(measuredTone.color)
            .lineLimit(1)
            .accessibilityLabel(
                "\(prefix ?? "waiting") \(Format.spokenAge(now.timeIntervalSince(since)))")
    }

    private var text: String {
        let clock = Self.clock(since: since, now: now)
        guard let prefix else { return clock }
        return "\(prefix) \(clock)"
    }

    private var measuredTone: CCTone {
        tone ?? Self.tone(elapsed: now.timeIntervalSince(since))
    }

    /// The two points at which what you should do changes: two minutes, and
    /// ten. Below the first, a wait is a wait.
    static func tone(elapsed: TimeInterval) -> CCTone {
        if elapsed >= 600 { return .danger }
        if elapsed >= 120 { return .warning }
        return .neutral
    }

    /// `12s` · `4m12s` · `2h04m` · `3d02h`. Two units, never three: the third is
    /// always noise at the scale where the second one matters.
    static func clock(since: Date, now: Date) -> String {
        let total = Int(max(0, now.timeIntervalSince(since)))
        switch total {
        case ..<60:
            return "\(total)s"
        case ..<3600:
            return "\(total / 60)m\(String(format: "%02d", total % 60))s"
        case ..<86400:
            return "\(total / 3600)h\(String(format: "%02d", (total % 3600) / 60))m"
        default:
            return "\(total / 86400)d\(String(format: "%02d", (total % 86400) / 3600))h"
        }
    }
}
