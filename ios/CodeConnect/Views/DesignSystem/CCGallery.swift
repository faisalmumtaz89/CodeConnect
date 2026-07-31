import SwiftUI

#if DEBUG

    // =========================================================================
    //  CCGallery — every component, every state.
    //
    //  This is the design system's test surface, and the only place every
    //  component can be seen at once. If a component is not in here, it is not
    //  finished; if a state is not in here, nobody has looked at it.
    //
    //  Run the previews at the bottom of this file. `Gallery · AX3` and
    //  `Gallery · AX5` are the ones that matter — a component that survives
    //  those without clipping is a component that ships.
    //
    //  Pressed states are LIVE, not mocked. Press and hold any control in the
    //  canvas or the simulator: press is a 1.5% contraction plus a one-step
    //  luminance change, and the hold button traces its ring in real time.
    // =========================================================================

    struct CCGallery: View {
        /// Pages, so a component can be reached without scrolling past six
        /// others. The picker is built from tappable `CCBadge`s, which means
        /// the gallery's own chrome is a live test of the kit.
        enum Page: String, CaseIterable, Identifiable {
            case foundations
            case identity
            case buttons
            case rows
            case indicators
            case controls
            case feedback
            /// The terminal's own furniture, plus the three shapes a wait takes.
            case terminal
            /// The diff grid's primitives, drawn as a grid.
            case diff

            var id: String { rawValue }
        }

        @State private var page: Page
        @State private var segment: Surface = .timeline
        @State private var threeWay: Density = .comfortable
        @State private var fieldText = ""
        @State private var monoField = "100.84.21.7:8765"
        @State private var errorField = "not-a-token"
        @State private var multiline = ""
        @State private var unlabelled = ""
        @State private var loadingButton = false
        @State private var lastAction = "—"

        enum Surface: String, Hashable { case timeline, terminal }
        enum Density: String, Hashable { case compact, comfortable, loose }

        init(page: Page = .foundations) {
            // Lets the screenshot harness open straight onto a page:
            //   xcrun simctl launch <dev> <bundle> --console
            //   with CC_GALLERY_PAGE=buttons in the environment.
            let requested = ProcessInfo.processInfo.environment["CC_GALLERY_PAGE"]
            _page = State(initialValue: requested.flatMap(Page.init(rawValue:)) ?? page)
        }

        var body: some View {
            VStack(spacing: 0) {
                pagePicker
                CCHairline()
                ScrollViewReader { proxy in
                    ScrollView {
                        VStack(alignment: .leading, spacing: CC.space.xxl) {
                            pageContent
                            Color.clear.frame(height: CC.space.xxl)
                        }
                        .padding(.horizontal, CC.space.md)
                        .padding(.top, CC.space.md)
                    }
                    // A page is long enough that "open the gallery and scroll"
                    // is not a review instruction anyone follows to the bottom.
                    // `CC_GALLERY_SECTION=CCFactRow` opens on the component,
                    // which is what makes a screenshot harness possible at all.
                    .onAppear {
                        guard
                            let anchor = ProcessInfo.processInfo
                                .environment["CC_GALLERY_SECTION"], !anchor.isEmpty
                        else { return }
                        DispatchQueue.main.asyncAfter(deadline: .now() + 0.35) {
                            withAnimation(.none) { proxy.scrollTo(anchor, anchor: .top) }
                        }
                    }
                }
            }
            .background(CC.color.bg)
            .ccAppearance()
        }

        @ViewBuilder
        private var pageContent: some View {
            switch page {
            case .foundations:
                header
                foundationsSection
                proseSection
            case .identity:
                identitySection
                gapSection
            case .buttons:
                buttonSection
                holdSection
                actionPairSection
            case .rows:
                rowSection
                composedRowSection
                factRowSection
                sectionHeaderSection
                cardSection
                statStripSection
                stepRowSection
                emptyStateSection
            case .indicators:
                badgeSection
                dotSection
                freshnessSection
            case .controls:
                fieldSection
                segmentedSection
                monoSection
                hunkHeaderSection
            case .feedback:
                bannerSection
                screenMarkSection
                waitSection
            case .terminal:
                keyCapSection
                disclosureSection
                progressSection
                scannerSection
            case .diff:
                diffChipSection
                diffGridSection
            }
        }

        private var pagePicker: some View {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: CC.space.xs) {
                    ForEach(Page.allCases) { candidate in
                        CCBadge(
                            candidate.rawValue,
                            isSelected: candidate == page,
                            action: { page = candidate })
                    }
                }
                .padding(.horizontal, CC.space.md)
            }
            .padding(.vertical, CC.space.xxs)
            .background(CC.color.bg)
            .accessibilityLabel("Gallery page")
        }

        // MARK: Header

        /// **The header states the rule the gallery is held to, and it is now
        /// true.**
        ///
        /// It used to say "Design system gallery — dark only", which is a
        /// description, not a claim anybody could fail. The file's own comment
        /// made the real claim — *if a component is not in here, it is not
        /// finished* — while nine renderable components had no page:
        /// `CCKeyCap`, `CCKeyCapDivider`, `CCSkeleton`, `CCSkeletonRow`,
        /// `CCWaitingNotice`, `CCProgressRing`, `CCScannerFrame`,
        /// `CCDisclosure` and the diff strip. `CCKeyCap` was the worst of them:
        /// no gallery page *and* a screen that needs a live SSH server, so
        /// "a control is 44pt" could be written about it and never checked.
        ///
        /// The three lines are the kit's own components — `CCProse` resolves the
        /// backticks, `CCFactRow` renders the last action through `CCMeasured`,
        /// so the em dash that means *nothing yet* draws at the same 2.53:1 the
        /// rest of the product draws it at instead of at full contrast.
        private var header: some View {
            VStack(alignment: .leading, spacing: CC.space.xs) {
                Text("CodeConnect")
                    .ccType(CC.type.display)
                    .foregroundStyle(CC.text.primary)
                CCProse(
                    "Every component under `Views/DesignSystem/` has a page here, dark only. "
                        + "A component with no reachable render cannot be measured, so nothing "
                        + "claimed about it can be checked — which is how `CCKeyCap` spent a "
                        + "long time being called a 44pt control.",
                    style: CC.type.callout, color: CC.text.secondary
                )
                .fixedSize(horizontal: false, vertical: true)

                CCCard(padding: 0) {
                    CCFactRow("Last action", value: lastAction, labelStyle: .key, separator: false)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }

        // MARK: Foundations

        @ViewBuilder
        private var foundationsSection: some View {
            section("Foundations") { surfaceLadder }
            section("Syntax") { syntaxSpecimen }
            section("ANSI") { ansiSpecimen }
            section("Type") { typeSpecimen }
        }

        /// The three syntax roles, on the three backgrounds a diff line
        /// can actually have: `bg`, the 6% row tint, and the 22% word tint. The
        /// word tint is the dark one and it is the one that decides the values.
        private var syntaxSpecimen: some View {
            VStack(alignment: .leading, spacing: CC.space.xs) {
                Text("SYNTAX — THREE ROLES, MEASURED ON THE WORST BACKGROUND")
                    .ccType(CC.type.micro)
                    .foregroundStyle(CC.text.tertiary)

                VStack(alignment: .leading, spacing: CC.space.xxs) {
                    contrastRow("keyword", CC.color.syntaxKeyword, "8.73 · 6.18 on word tint")
                    contrastRow("string", CC.color.syntaxString, "15.25 · 10.80")
                    contrastRow("comment", CC.color.syntaxComment, "6.58 · 4.66")
                    contrastRow("everything else = text", CC.text.primary, "17.94 · 12.70")
                }

                ForEach(
                    [
                        ("on bg", CC.color.bg),
                        ("on successMuted (6%)", CC.color.successMuted),
                        ("on successWord (22%)", CC.color.successWord),
                        ("on dangerWord (22%)", CC.color.dangerWord),
                    ], id: \.0
                ) { label, background in
                    VStack(alignment: .leading, spacing: 2) {
                        Text(label)
                            .ccType(CC.type.monoSmall)
                            .foregroundStyle(CC.text.tertiary)
                        codeSpecimen
                            .padding(.horizontal, CC.space.xs)
                            .padding(.vertical, CC.space.xxs)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .background(background)
                    }
                }
            }
        }

        /// A line of Swift in the three roles plus `text`, so the four can be
        /// judged against each other rather than one at a time.
        private var codeSpecimen: some View {
            (Text("func ").foregroundStyle(CC.color.syntaxKeyword)
                + Text("send(_ text: ").foregroundStyle(CC.text.primary)
                + Text("String").foregroundStyle(CC.color.syntaxKeyword)
                + Text(") { ").foregroundStyle(CC.text.primary)
                + Text("// why, not what").foregroundStyle(CC.color.syntaxComment)
                + Text(" \"cc-1\"").foregroundStyle(CC.color.syntaxString))
                .ccType(CC.type.monoSmall)
                .lineLimit(1)
        }

        /// The terminal's 16-colour table. Ten of the sixteen are palette tokens
        /// under another name; what has to hold is that no bright is darker than
        /// its normal and that nothing here belongs to a different family.
        private var ansiSpecimen: some View {
            VStack(alignment: .leading, spacing: CC.space.xs) {
                Text("ANSI — 16 COLOURS, NORMAL OVER BRIGHT")
                    .ccType(CC.type.micro)
                    .foregroundStyle(CC.text.tertiary)

                ForEach(0..<2, id: \.self) { half in
                    HStack(spacing: CC.space.xxs) {
                        ForEach(0..<8, id: \.self) { index in
                            VStack(spacing: 2) {
                                RoundedRectangle(cornerRadius: CC.radius.sm, style: .continuous)
                                    .fill(CC.ansi.table[half * 8 + index].color)
                                    .frame(height: 28)
                                    .overlay {
                                        RoundedRectangle(
                                            cornerRadius: CC.radius.sm, style: .continuous
                                        )
                                        .strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline)
                                    }
                                Text("\(half * 8 + index)")
                                    .ccType(CC.type.monoSmall)
                                    .foregroundStyle(CC.text.tertiary)
                            }
                        }
                    }
                }

                Text(
                    "0 black · 1 red · 2 green · 3 yellow · 4 blue · 5 magenta · 6 cyan · 7 white, then the brights. Black and bright black are the two below AA and the two that must be — ANSI black is a background, bright black is the dim slot."
                )
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)

                // Foreground on background, as the terminal actually draws it.
                VStack(alignment: .leading, spacing: 2) {
                    ForEach(Array(Self.ansiNames.enumerated()), id: \.offset) { index, name in
                        HStack(spacing: CC.space.xs) {
                            Text("\(name) text")
                                .ccType(CC.type.monoSmall)
                                .foregroundStyle(CC.ansi.table[index].color)
                            Text("bright")
                                .ccType(CC.type.monoSmall)
                                .foregroundStyle(CC.ansi.table[index + 8].color)
                        }
                    }
                }
                .padding(CC.space.xs)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(CC.ansi.background)
                .overlay {
                    RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous)
                        .strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline)
                }
            }
        }

        private static let ansiNames = [
            "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        ]

        private var surfaceLadder: some View {
            VStack(alignment: .leading, spacing: CC.space.xs) {
                Text("SURFACE LADDER")
                    .ccType(CC.type.micro)
                    .foregroundStyle(CC.text.tertiary)
                HStack(spacing: CC.space.xs) {
                    swatch("bg", CC.color.bg, "L 0")
                    swatch("surface", CC.color.surface, "L .0030")
                    swatch("raised", CC.color.surfaceRaised, "L .0065")
                    swatch("overlay", CC.color.surfaceOverlay, "L .0103")
                }

                Text("TEXT ON BG — MEASURED")
                    .ccType(CC.type.micro)
                    .foregroundStyle(CC.text.tertiary)
                    .padding(.top, CC.space.xs)
                VStack(alignment: .leading, spacing: CC.space.xxs) {
                    contrastRow("text", CC.text.primary, "17.94:1")
                    contrastRow("textSecondary", CC.text.secondary, "8.13:1")
                    contrastRow("textTertiary", CC.text.tertiary, "5.42:1")
                    contrastRow("textDisabled", CC.text.disabled, "2.69:1 · exempt")
                }

                Text("SEMANTIC")
                    .ccType(CC.type.micro)
                    .foregroundStyle(CC.text.tertiary)
                    .padding(.top, CC.space.xs)
                VStack(alignment: .leading, spacing: CC.space.xxs) {
                    contrastRow("success", CC.color.success, "10.52:1")
                    contrastRow("info", CC.color.info, "5.71:1")
                    contrastRow("warning", CC.color.warning, "10.36:1")
                    contrastRow("danger", CC.color.danger, "6.43:1")
                }
            }
        }

        private func swatch(_ name: String, _ color: Color, _ detail: String) -> some View {
            VStack(alignment: .leading, spacing: CC.space.xxs) {
                RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous)
                    .fill(color)
                    .frame(height: 44)
                    .overlay {
                        RoundedRectangle(cornerRadius: CC.radius.md, style: .continuous)
                            .strokeBorder(CC.color.border, lineWidth: CC.stroke.hairline)
                    }
                // The swatch's name labels the value under it — `fieldLabel`,
                // not `micro`. `micro` is `textTertiary` and nothing else.
                Text(name)
                    .ccType(CC.type.fieldLabel, color: nil)
                Text(detail)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }

        private func contrastRow(_ name: String, _ color: Color, _ ratio: String) -> some View {
            HStack(spacing: CC.space.xs) {
                Text(name)
                    .ccType(CC.type.callout)
                    .foregroundStyle(color)
                Spacer(minLength: CC.space.xs)
                Text(ratio)
                    .ccType(CC.type.monoSmall)
                    .foregroundStyle(CC.text.tertiary)
            }
        }

        private var typeSpecimen: some View {
            VStack(alignment: .leading, spacing: CC.space.xs) {
                Text("TYPE SCALE")
                    .ccType(CC.type.micro)
                    .foregroundStyle(CC.text.tertiary)
                specimen("display 32/38", CC.type.display, CC.text.primary)
                specimen("title 22/28", CC.type.title, CC.text.primary)
                specimen("headline 17/22", CC.type.headline, CC.text.primary)
                specimen("body 16/22", CC.type.body, CC.text.primary)
                specimen("callout 15/20", CC.type.callout, CC.text.primary)
                specimen("footnote 13/18", CC.type.footnote, CC.text.secondary)
                // The three 11pt roles, stacked so the separation is visible.
                // They shipped as one token in four colours, which is why
                // `BLOCKED`, `MEDIUM` and `HELD FOR YOU` measured identically
                // 200pt apart on one screen.
                specimen("MICRO 11/14 +0.06EM — SECTION LABELS ONLY", CC.type.micro, CC.text.tertiary)
                specimen("BADGELABEL 11/14 SEMIBOLD +0.04EM", CC.type.badgeLabel, CC.text.primary)
                specimen("FIELDLABEL 11/14 +0.02EM", CC.type.fieldLabel, CC.text.secondary)
                specimen("mono 14  cc-1 · K76F46", CC.type.mono, CC.text.primary)
                specimen("monoSmall 12  1.4s", CC.type.monoSmall, CC.text.secondary)
            }
        }

        private func specimen(_ text: String, _ style: CCTextStyle, _ color: Color) -> some View {
            Text(text)
                .ccType(style)
                .foregroundStyle(color)
                .frame(maxWidth: .infinity, alignment: .leading)
                .fixedSize(horizontal: false, vertical: true)
        }

        // MARK: Identity

        private var identitySection: some View {
            section("CCIdentity") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    Text(
                        "The bright text is always the part that identifies. Two runs sharing `cc-1` differ only in the tail — so the tail is lit and the shared prefix is dimmed."
                    )
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)

                    CCCard {
                        VStack(alignment: .leading, spacing: CC.space.xs) {
                            CCIdentity(name: "cc-1", tail: "K76F46")
                            CCIdentity(name: "cc-1", tail: "WXNSK1")
                            CCIdentity(name: "cc-2")
                            CCIdentity(name: "cc-3", tail: "4410FE", style: CC.type.mono)
                        }
                    }

                    Text("FINGERPRINT DIFF — SSH HOST KEY CHANGED")
                        .ccType(CC.type.micro)
                        .foregroundStyle(CC.text.tertiary)
                    CCCard {
                        VStack(alignment: .leading, spacing: CC.space.xs) {
                            // `fieldLabel`, not `micro`: these name the value
                            // directly beneath them, which is the whole
                            // definition of the token. `micro` is section
                            // labels, and nothing else.
                            Text("PINNED")
                                .ccType(CC.type.fieldLabel)
                                .foregroundStyle(CC.text.secondary)
                            CCFingerprint(
                                value: "SHA256:9Xk2LmQpR7vN3wZ", reference: nil,
                                name: "The key you pinned")
                            Text("OFFERED NOW")
                                .ccType(CC.type.fieldLabel)
                                .foregroundStyle(CC.text.secondary)
                            CCFingerprint(
                                value: "SHA256:9Xk2LmQzR7vB3wZ",
                                reference: "SHA256:9Xk2LmQpR7vN3wZ",
                                name: "The key offered now")
                        }
                    }
                }
            }
        }

        // MARK: Gap marker

        private var gapSection: some View {
            section("CCGapMarker") {
                VStack(alignment: .leading, spacing: CC.space.xs) {
                    Text("A gap has a position in time. It is never a top banner.")
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                    CCCard(padding: CC.space.sm) {
                        VStack(alignment: .leading, spacing: CC.space.xs) {
                            Text("Read src/router.ts")
                                .ccType(CC.type.callout)
                                .foregroundStyle(CC.text.primary)
                            CCGapMarker(
                                label: "12 events missing · 4m ago",
                                actionLabel: "Load the missing events",
                                action: { lastAction = "gap" })
                            Text("Edit src/router.ts")
                                .ccType(CC.type.callout)
                                .foregroundStyle(CC.text.primary)
                        }
                    }
                }
            }
        }

        // MARK: Buttons

        private var buttonSection: some View {
            section("CCButton") {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    // Four primary heights shipped — 52, 52, 44 and a **36** on
                    // the most consequential card in Session Detail, below the
                    // stated 44pt minimum for a visible control. The ladder is
                    // three rungs; a primary may not stand on the bottom one, so
                    // `.primary` + `.sm` is promoted to `.md` in the initialiser
                    // rather than being written down and measured at 36 later.
                    // Read the primary row: its first button is 44, not 36.
                    note("sm 36 · md 44 · lg 52 — a primary at sm is promoted to md")

                    ForEach(CCButtonVariant.allCases, id: \.self) { variant in
                        VStack(alignment: .leading, spacing: CC.space.xs) {
                            Text(variant.rawValue.uppercased())
                                .ccType(CC.type.micro)
                                .foregroundStyle(CC.text.tertiary)
                            CCAdaptiveStack(horizontalSpacing: CC.space.xs) {
                                ForEach(CCButtonSize.allCases, id: \.self) { size in
                                    CCButton(
                                        size.rawValue, variant: variant, size: size,
                                        action: { lastAction = "\(variant.rawValue).\(size.rawValue)" })
                                }
                            }
                        }
                    }

                    VStack(alignment: .leading, spacing: CC.space.xs) {
                        Text("STATES")
                            .ccType(CC.type.micro)
                            .foregroundStyle(CC.text.tertiary)
                        CCButton(
                            "With icon", icon: "checkmark.seal.fill", variant: .primary,
                            size: .lg, fullWidth: true, action: { lastAction = "icon" })
                        CCButton(
                            "Loading", variant: .primary, size: .lg, fullWidth: true,
                            isLoading: true, action: {})
                        CCButton(
                            "Loading, secondary", variant: .secondary, size: .md,
                            fullWidth: true, isLoading: true, action: {})
                        CCButton(
                            "Disabled — link is stale", variant: .primary, size: .lg,
                            fullWidth: true,
                            disabledReason: CCDisabledReason(
                                "The daemon has not spoken for 48 seconds."),
                            action: {})
                        CCButton(
                            "Disabled secondary", variant: .secondary, size: .md,
                            fullWidth: true,
                            disabledReason: CCDisabledReason("Not paired."), action: {})
                        CCButton(
                            "Disabled ghost", variant: .ghost, size: .md, fullWidth: true,
                            disabledReason: CCDisabledReason("Nothing to undo."), action: {})
                        CCButton(
                            "Deny", icon: "xmark", variant: .destructive, size: .lg,
                            fullWidth: true, action: { lastAction = "deny" })
                        HStack(spacing: CC.space.xs) {
                            CCButton(
                                "Deny", variant: .destructive, size: .lg, fullWidth: true,
                                action: { lastAction = "deny" })
                            CCButton(
                                "Allow", variant: .primary, size: .lg, fullWidth: true,
                                isLoading: loadingButton,
                                action: {
                                    lastAction = "allow"
                                    loadingButton.toggle()
                                })
                        }
                    }
                }
            }
        }

        // MARK: Hold

        private var holdSection: some View {
            section("CCHoldButton") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    Text(
                        "Press and hold — the ring traces the perimeter in 1.2s. Tap without holding: the ring rewinds and the button says why, rather than looking like nothing happened."
                    )
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    CCHoldButton("Hold to allow", action: { lastAction = "held allow" })
                    CCHoldButton(
                        "Hold to force push", icon: "arrow.up.forward.square.fill",
                        tone: .warning, action: { lastAction = "held push" })
                    CCHoldButton("Submitting", isLoading: true, action: {})

                    Text("DISABLED — THE REASON IS DRAWN, NOT ONLY HINTED")
                        .ccType(CC.type.micro)
                        .foregroundStyle(CC.text.tertiary)
                        .padding(.top, CC.space.xs)
                    CCHoldButton(
                        "Hold to allow",
                        disabledReason: CCDisabledReason(
                            "The text on this card does not match its hash."),
                        action: {})
                    // The HIGH-risk stale-link case that makes this mandatory: at HIGH
                    // the hold button *is* Allow, so a silent one leaves the
                    // card with no sentence anywhere on it.
                    CCHoldButton(
                        "Hold to allow",
                        disabledReason: CCDisabledReason(
                            "2m since the daemon last spoke. Actions are disabled until it answers."
                        ),
                        action: {})
                }
            }
        }

        // MARK: Rows

        private var rowSection: some View {
            section("CCRow") {
                CCCard(padding: 0) {
                    VStack(spacing: 0) {
                        CCRow(
                            "codeconnect",
                            // No state prefix. `Waiting on:` / `needs you:` were
                            // deleted from rows — the band header, the aggregate
                            // and the display line each already say it, and a
                            // fourth copy on the row is exactly the restatement
                            // that was cut. The subtitle is the daemon's
                            // activity sentence and nothing else.
                            subtitle: "Bash rm -rf node_modules",
                            meta: "cc-1 · K76F46 · 12s",
                            action: { lastAction = "row 1" }
                        ) {
                            CCStatusDot(status: .blocked)
                        } trailing: {
                            CCBadge(status: .blocked, count: 3)
                        }

                        CCRow(
                            "api-gateway",
                            subtitle: "Edit src/router.ts",
                            meta: "cc-2 · 9A21BC · 3s",
                            action: { lastAction = "row 2" }
                        ) {
                            CCStatusDot(status: .running)
                        } trailing: {
                            CCBadge(status: .running)
                        }

                        CCRow(
                            "infra-terraform",
                            // `Done`, not `Finished`: one word per state, and
                            // the word is the one `FleetStatus.label` prints.
                            subtitle: "Done — you have not looked at the diff",
                            meta: "cc-3 · 4410FE · 4m",
                            action: { lastAction = "row 3" }
                        ) {
                            CCStatusDot(status: .doneUnreviewed)
                        } trailing: {
                            CCBadge(status: .doneUnreviewed)
                        }

                        CCRow(
                            "Display row — no action",
                            subtitle: "No press state, no chevron, no button trait",
                            meta: "cc-4 · 0000AA · 2h",
                            showsChevron: false
                        ) {
                            CCStatusDot(tone: .neutral)
                        }

                        CCRow(
                            "Disabled row",
                            subtitle: "Observe only — no supervisor attached",
                            meta: "cc-5 · BEEF01 · 9m",
                            separator: false,
                            disabledReason: CCDisabledReason(
                                "No supervisor is attached to this session."),
                            action: { lastAction = "never" }
                        ) {
                            CCStatusDot(tone: .neutral)
                        } trailing: {
                            CCBadge(capability: .observe(reason: "detached"))
                        }
                    }
                }
            }
        }

        /// The fleet's own row, built from the same component: `.comfortable`
        /// density (the 76pt and 96pt row floors), middle truncation on the
        /// place name, and line three as a *composition* rather than a string.
        ///
        /// **This specimen is the `meta`-width regression test.** The badge and
        /// the clock are the widest accessory the product ships, and the command
        /// underneath them is longer than the space they used to leave: if line
        /// three ever reads `…origin ma…` here again, the slot has gone back to
        /// sharing the title's column. The copy is the shipped copy too — the
        /// gallery kept a `needs you:` prefix and a `HELD FOR YOU` chip long
        /// after both were deleted from the product, which is how a component
        /// gallery starts describing an app that no longer exists.
        private var composedRowSection: some View {
            section("CCRow · meta slot, comfortable density") {
                CCCard(padding: 0) {
                    VStack(spacing: 0) {
                        CCRow(
                            "codeconnect",
                            titleTruncation: .middle,
                            subtitleLineLimit: 1,
                            showsChevron: false,
                            density: .comfortable,
                            // The shipped label, in the shipped order
                            // (`FleetView.accessibilityLabel`): place, status,
                            // class, the class's *rationale*, the uid spelled
                            // one character at a time, the activity, the wait,
                            // and the capability — which is announced on every
                            // row whether or not the badge is drawn.
                            accessibilityLabelText:
                                "codeconnect, Blocked, risk HIGH, Destructive, credentialed, or publishes something., cc-1, run K, 7, 6, F, 4, 6, Bash, git push --force origin main, waiting 4 minutes ago, control",
                            action: { lastAction = "fleet row" }
                        ) {
                            // **No dot.** A blocked row draws none: the band
                            // name, the band border and the clock already carry
                            // the state, and the disc was a fourth copy of it.
                            // The column is still 8pt wide, so the title does
                            // not move — `FleetView.gutter` draws the same disc
                            // in nothing.
                            CCStatusDot(color: .clear, pulses: false)
                        } trailing: {
                            CCAdaptiveStack(
                                horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xxs
                            ) {
                                CCBadge(risk: .high)
                                // No tone: `CCWaitClock` measures its own. The
                                // specimen's wait is 4m12s, which is past the
                                // two-minute step and therefore amber — one
                                // stop of a three-stop scale, shown because it
                                // was derived and not because it was asked for.
                                CCWaitClock(
                                    since: Self.waitingSince, now: Date(), prefix: nil)
                            }
                        } meta: {
                            VStack(alignment: .leading, spacing: CC.space.xxs) {
                                CCAdaptiveStack(
                                    horizontalSpacing: CC.space.xs, verticalSpacing: 2,
                                    verticalAlignment: .firstTextBaseline
                                ) {
                                    Text("Bash")
                                        .ccType(CC.type.footnote)
                                        .foregroundStyle(CC.text.primary)
                                        .lineLimit(1)
                                        .layoutPriority(1)
                                    CCMonoBlock(inline: "git push --force origin main")
                                    Spacer(minLength: 0)
                                }
                                CCAdaptiveStack(
                                    horizontalSpacing: CC.space.sm, verticalSpacing: CC.space.xs
                                ) {
                                    CCIdentity(name: "cc-1", tail: "K76F46")
                                    Spacer(minLength: CC.space.xs)
                                }
                            }
                        }

                        // Middle truncation is the point of this row: the tail
                        // is the only part that differs between soak sessions.
                        CCRow(
                            "ccsoak-tail-54568-1785466205",
                            subtitle: "Bash echo soak",
                            titleTruncation: .middle,
                            subtitleLineLimit: 1,
                            showsChevron: false,
                            density: .comfortable,
                            action: { lastAction = "soak row" }
                        ) {
                            CCStatusDot(status: .running)
                        } trailing: {
                            Text("2h")
                                .ccType(CC.type.monoSmall)
                                .foregroundStyle(CC.text.tertiary)
                        } meta: {
                            EmptyView()
                        }

                        // An ended row keeps full contrast on everything that
                        // identifies it and drops only its title a step.
                        // Never `.opacity`: dimming a whole row is forbidden.
                        CCRow(
                            "infra-terraform",
                            subtitle: "Session ended",
                            titleTruncation: .middle,
                            subtitleLineLimit: 1,
                            showsChevron: false,
                            separator: false,
                            density: .comfortable,
                            isDimmed: true,
                            action: { lastAction = "ended row" }
                        ) {
                            CCStatusDot(status: .ended)
                        } trailing: {
                            Text("4d")
                                .ccType(CC.type.monoSmall)
                                .foregroundStyle(CC.text.tertiary)
                        } meta: {
                            EmptyView()
                        }
                    }
                }
            }
        }

        private static let waitingSince = Date().addingTimeInterval(-252)

        // MARK: Fact rows

        private var factRowSection: some View {
            section("CCFactRow") {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    CCCard(padding: 0) {
                        VStack(spacing: 0) {
                            CCFactRow("State", value: "Live", tone: .success)
                            CCFactRow("Daemon last spoke", value: "0.4s ago")
                            // `—`, never `0`: nobody measured this.
                            CCFactRow("Round trip", value: "—", isUnmeasured: true)
                            CCFactRow("Address", value: "ws://100.x.y.z:8787")
                            CCFactRow(
                                "Transport", value: "ws (plain)", tone: .warning,
                                separator: false)
                        }
                    }

                    CCCard(padding: 0) {
                        VStack(spacing: 0) {
                            CCFactRow(
                                "Answer approvals", accessibilityValueText: "yes",
                                value: {
                                    CCBadge("Yes", tone: .success)
                                })
                            // `.key` — a name the daemon chose the spelling of.
                            CCFactRow("answer_path", value: "hook", labelStyle: .key)
                            CCFactRow(
                                "hold_secs", value: "1.2", labelStyle: .key, separator: false)
                        }
                    }

                    CCCard(padding: 0) {
                        VStack(spacing: 0) {
                            // `.identifier` — the row *is* the host, so the host
                            // carries the emphasis and the age stays quiet.
                            CCFactRow(
                                "studio.tail1234.ts.net", labelStyle: .identifier,
                                age: "pinned 3d ago",
                                detail: {
                                    CCIdentity.fingerprint(
                                        "SHA256:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU",
                                        comparedTo: nil,
                                        name: "Pinned key for studio.tail1234.ts.net")
                                })
                            CCFactRow(
                                "Last error", age: "2m ago", separator: false,
                                detail: {
                                    CCMonoBlock(
                                        "connection refused (os error 61)", tone: .danger,
                                        isSmall: true)
                                })
                        }
                    }
                }
            }
        }

        // MARK: Section headers

        private var sectionHeaderSection: some View {
            section("CCSectionHeader") {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    // **The band dot is gone**, and so is its pulse. It was a
                    // fourth encoding of a state the band name, the band border
                    // and the row's own clock all carried, and it was one of the
                    // five amber discs that were measured pulsing in unison. No
                    // screen passes `dotColor:` any more; a specimen that still
                    // draws one is a gallery describing a product that no
                    // longer exists.
                    //
                    // **Placed flush.** Every one of these carried
                    // `.padding(.leading, CCColumn.content)` while the component
                    // placed nothing, and the moment the column moved into the
                    // component the two stacked: 36 + 36 put every label on
                    // x=88. A screen gets two left edges — 32 for marks, 52 for
                    // language — and never a third; that is the collision
                    // eleven screen call sites have to avoid, and the gallery
                    // walked into it first, which is the argument for the
                    // component owning the number, not against it.
                    CCSectionHeader("Blocked", count: 3)
                    CCSectionHeader("Running", count: 8)
                    CCSectionHeader(
                        "Idle", count: 4, note: "Observe only",
                        noteAction: { lastAction = "observe note" })
                    CCSectionHeader("Ended", count: 4, note: "Not tappable")
                    CCSectionHeader(
                        "Since you looked", actionTitle: "Show all",
                        action: { lastAction = "show all" })
                    // The one header in the product that takes a mark. The dot
                    // hangs back in the 32 gutter; the label stays on 52.
                    CCSectionHeader("Blocked", count: 3, dotColor: CC.color.warning)
                }
            }
        }

        // MARK: Cards

        private var cardSection: some View {
            section("CCCard") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    CCCard {
                        Text("A plain card. Surface, hairline, radius lg, 16pt padding.")
                            .ccType(CC.type.callout)
                            .foregroundStyle(CC.text.secondary)
                    }

                    CCCard {
                        CCSectionHeader("Exact command")
                    } content: {
                        VStack(alignment: .leading, spacing: CC.space.xs) {
                            CCMonoBlock("rm -rf node_modules && npm install")
                            Text("Writes files, installs, or reaches the network.")
                                .ccType(CC.type.footnote)
                                .foregroundStyle(CC.color.warning)
                        }
                    } footer: {
                        HStack {
                            Text("payload verified")
                                .ccType(CC.type.monoSmall)
                                .foregroundStyle(CC.text.tertiary)
                            Spacer()
                            CCBadge(risk: .medium)
                        }
                    }

                    // There is no raised card. `surfaceRaised` is the rung for a
                    // block nested *inside* a card, and a `CCMonoBlock` on a
                    // `CCCard` is what that looks like.
                    CCCard {
                        VStack(alignment: .leading, spacing: CC.space.sm) {
                            Text("One fill for a card. `surfaceRaised` is for what sits on it.")
                                .ccType(CC.type.callout)
                                .foregroundStyle(CC.text.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                            CCMonoBlock("cc claude --resume", isSmall: true)
                        }
                    }
                }
            }
        }

        // MARK: Badges

        private var badgeSection: some View {
            section("CCBadge") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    labelled("one construction — tint@12 fill, tint@40 border, full-strength label") {
                        ForEach(CCTone.allCases, id: \.self) { tone in
                            CCBadge(tone.rawValue, tone: tone)
                        }
                    }
                    labelled("status") {
                        ForEach(FleetStatus.allCases, id: \.self) { status in
                            CCBadge(status: status)
                        }
                    }
                    // The finding this page exists to prove is fixed: HIGH was
                    // a 100% `danger` fill with a black label — the primary
                    // button's recipe — so a label nobody can press out-shouted
                    // the button underneath it. Read this row left to right: the
                    // three stops share a construction, and HIGH still leads on
                    // its glyph, its 1.5pt edge and its word.
                    labelled("risk — one scale, three stops; HIGH leads by glyph + edge") {
                        CCBadge(risk: .low)
                        CCBadge(risk: .medium)
                        CCBadge(risk: .high)
                    }
                    labelled("…beside the primary it must not out-shout") {
                        CCBadge(risk: .high)
                        CCButton("Review", size: .sm) { lastAction = "review" }
                    }
                    labelled("capability") {
                        CCBadge(capability: .control)
                        CCBadge(capability: .observe(reason: "No supervisor attached."))
                    }
                    labelled("count · icon · CCCountChip") {
                        CCBadge(status: .blocked, count: 3)
                        CCBadge("live", icon: "bolt.fill", tone: .success)
                        CCCountChip(3)
                        CCCountChip(128)
                    }
                    labelled("tappable — 44pt hit, press state, selected") {
                        CCBadge(
                            "src/router.ts",
                            action: { lastAction = "chip" })
                        CCBadge(
                            "since you looked", isSelected: true,
                            action: { lastAction = "toggle" })
                    }
                }
            }
        }

        // MARK: Dots

        private var dotSection: some View {
            section("CCStatusDot") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    labelled("fleet status — running is WHITE, not blue") {
                        ForEach(FleetStatus.allCases, id: \.self) { status in
                            HStack(spacing: CC.space.xxs) {
                                CCStatusDot(status: status)
                                Text(status.label.lowercased())
                                    .ccType(CC.type.monoSmall)
                                    .foregroundStyle(CC.text.tertiary)
                            }
                        }
                    }
                    labelled("hollow = from cache") {
                        ForEach(FleetStatus.allCases, id: \.self) { status in
                            CCStatusDot(status: status, isCached: true)
                        }
                    }
                    // The token permits it for two states; the *product* spends
                    // it on one object — the Deck's aggregate — because five
                    // dots pulsing in unison on the fleet reads as an alarm.
                    labelled("pulsing — the aggregate only; a fleet row never pulses") {
                        CCStatusDot(tone: .warning, pulses: true, accessibilityText: "Blocked")
                        CCStatusDot(tone: .info, pulses: true, accessibilityText: "Connecting")
                    }
                    labelled("sizes 8 / 10 / 12") {
                        CCStatusDot(tone: .success, size: CCStatusDot.Size.row.rawValue)
                        CCStatusDot(tone: .success, size: CCStatusDot.Size.cardHeader.rawValue)
                        CCStatusDot(tone: .success, size: CCStatusDot.Size.hero.rawValue)
                    }
                    // The dot scales now. Read this row at AX5: the dot has to
                    // still look like a state signal beside the word, not like
                    // dust that landed next to it.
                    labelled("scales with type — 8 → 14 at AX5, beside its word") {
                        HStack(spacing: CC.space.xs) {
                            CCStatusDot(status: .blocked)
                            Text("blocked")
                                .ccType(CC.type.mono)
                                .foregroundStyle(CC.color.warning)
                        }
                        HStack(spacing: CC.space.xs) {
                            CCStatusDot(status: .running)
                            Text("running")
                                .ccType(CC.type.mono)
                                .foregroundStyle(CC.text.primary)
                        }
                        HStack(spacing: CC.space.xs) {
                            CCStatusDot(status: .running, isCached: true)
                            Text("cached")
                                .ccType(CC.type.mono)
                                .foregroundStyle(CC.text.tertiary)
                        }
                    }
                }
            }
        }

        // MARK: Freshness

        private var freshnessSection: some View {
            section("CCFreshnessPill") {
                CCAdaptiveStack(horizontalSpacing: CC.space.xs, verticalSpacing: CC.space.xs) {
                    CCFreshnessPill(
                        health: LinkHealth(level: .live, age: 1.2, detail: "Live."),
                        action: { lastAction = "link" })
                    CCFreshnessPill(
                        health: LinkHealth(level: .lagging, age: 18, detail: "Link lagging."))
                    CCFreshnessPill(
                        health: LinkHealth(level: .connecting, age: nil, detail: "Opening."))
                    CCFreshnessPill(
                        health: LinkHealth(level: .stale, age: 61, detail: "Gone quiet."))
                    CCFreshnessPill(
                        health: LinkHealth(level: .offline, age: nil, detail: "Not paired."))
                    CCFreshnessPill(
                        health: LinkHealth(level: .rejected, age: nil, detail: "Token rejected."))
                }
            }
        }

        // MARK: Fields

        private var fieldSection: some View {
            section("CCField") {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    CCField(
                        label: "Reason", text: $fieldText,
                        placeholder: "Tell Claude what to do instead",
                        hint: "Denies with Escape, then types this into the session.")
                    CCField(
                        label: "Daemon address", text: $monoField,
                        placeholder: "100.x.y.z:8765",
                        keyboardType: .URL, autocapitalization: .never,
                        disableAutocorrection: true, isMono: true)
                    CCField(
                        label: "Pairing token", text: $errorField,
                        placeholder: "paste the code",
                        error: "That is not a valid pairing token.")
                    CCField(
                        label: "Message", text: $multiline,
                        placeholder: "Say something to this agent",
                        axis: .vertical, lineLimit: 1...5)

                    Text("NO LABEL — THE COMPOSE BAR'S FORM")
                        .ccType(CC.type.micro)
                        .foregroundStyle(CC.text.tertiary)
                    Text(
                        "An empty label string still reserves its line: ~22pt of dead space above the field. `nil` draws no row at all, and VoiceOver falls back to the placeholder."
                    )
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    CCField(
                        text: $unlabelled,
                        placeholder: "Say something to this agent",
                        axis: .vertical, lineLimit: 1...5)
                }
            }
        }

        // MARK: Segmented

        private var segmentedSection: some View {
            section("CCSegmented") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    CCSegmented(
                        selection: $segment,
                        options: [
                            CCSegmentedOption(.timeline, title: "Timeline", icon: "list.bullet"),
                            CCSegmentedOption(.terminal, title: "Terminal", icon: "terminal"),
                        ],
                        accessibilityLabel: "Session surface")
                    CCSegmented(
                        selection: $threeWay,
                        options: [
                            CCSegmentedOption(.compact, title: "Compact"),
                            CCSegmentedOption(.comfortable, title: "Comfortable"),
                            CCSegmentedOption(.loose, title: "Loose"),
                        ],
                        accessibilityLabel: "Density")
                }
            }
        }

        // MARK: Mono

        private var monoSection: some View {
            VStack(alignment: .leading, spacing: CC.space.xxl) {
                section("CCMonoBlock") {
                    VStack(alignment: .leading, spacing: CC.space.sm) {
                        // The three strings that were measured breaking. Read
                        // them at AX5, which is where the old block hid a
                        // command completely.
                        note("wraps with ↳ — never fades, never ellipsises")
                        CCMonoBlock(
                            "git push --force-with-lease origin main && gh pr merge --squash --delete-branch"
                        )
                        note("a path — never hyphen-broken, never proportional")
                        CCMonoBlock("/private/tmp/ccsoak-work.giePJZ/Sources/Feature.swift")
                        note("clears the copy button at every type size")
                        CCMonoBlock(
                            "ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub", isSmall: true)
                        note("prose output — soft wrap; ↳ only where a token was cut")
                        CCMonoBlock(
                            "npm ERR! code ELIFECYCLE\nnpm ERR! Failed at the build script; log at /Users/you/.npm/_logs/2026-07-31T09_14_02_113Z-debug.log",
                            tone: .danger, wraps: true)
                        note("lineLimit collapses and says so — it never truncates")
                        CCMonoBlock(
                            "{\n  \"tool\": \"Bash\",\n  \"command\": \"rm -rf node_modules && npm install\",\n  \"cwd\": \"/Users/dev/app\"\n}",
                            lineLimit: 2)
                        CCMonoBlock("cc-1 · K76F46", showsCopy: false, isSmall: true)
                    }
                }

                section("CCMonoBlock · identifiers") {
                    VStack(alignment: .leading, spacing: CC.space.sm) {
                        // The only thing that may be shortened, and there is no
                        // `.tail`: tail truncation cuts a command's arguments,
                        // which is the byte a hostile suffix hides behind.
                        note("a key, wrapped — the default; nothing hidden")
                        CCMonoBlock(Self.publicKey, lineLimit: 3, isSmall: true)
                        note("middle — keeps the algorithm and the device comment")
                        CCMonoBlock(
                            Self.publicKey, lineLimit: 2, wraps: true, truncation: .middle,
                            isSmall: true)
                        note("head — a hash is identified by its tail")
                        CCMonoBlock(
                            "sha256:9f2b1c7d4e6a8035bd5c1e2f7a9048c3b6d1e5f082a4c7d9e3b0f6a1c8d2e4b7",
                            lineLimit: 1, truncation: .head, isSmall: true)
                    }
                }

                section("ccScrollCap") {
                    VStack(alignment: .leading, spacing: CC.space.sm) {
                        note("capped at 45% of the viewport, scrolling the rest")
                        VStack(alignment: .leading, spacing: 0) {
                            ForEach(0..<12, id: \.self) { index in
                                CCFactRow("Bar \(index + 1)", value: "\((index + 1) * 7)ms")
                            }
                        }
                        .ccScrollCap()
                        .ccSurface(.surface, radius: CC.radius.md)
                    }
                }
            }
        }

        /// A one-line caption over a specimen. Deliberately short: at AX5 a
        /// three-sentence caption is a screenful, and a gallery page that opens
        /// on an essay is a page nobody scrolls to the component.
        private func note(_ text: String) -> some View {
            // `CCProse`, so the gallery's own captions demonstrate the rule they
            // describe: every `\`token\`` in a caption sets in mono, and the
            // backticks that used to render as grave accents are gone.
            CCProse(text, style: CC.type.footnote, color: CC.text.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }

        private static let publicKey =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGb3kZ9pQ2xM7vN4tR8sW1yU6cH0aJ5dF2gK9lP3oQxZ codeconnect-iphone17pro-K76F46"

        // MARK: Banners

        private var bannerSection: some View {
            section("CCBanner + CCBannerSlot") {
                VStack(alignment: .leading, spacing: CC.space.xs) {
                    Text(
                        "One banner. Ever. The slot is handed every candidate and renders the highest-priority one — rejected > offline > stale > cached > gap > truncated."
                    )
                    .ccType(CC.type.footnote)
                    .foregroundStyle(CC.text.secondary)
                    .fixedSize(horizontal: false, vertical: true)

                    // Four candidates in, exactly one banner out.
                    CCBannerSlot([
                        CCBannerItem(
                            .truncated, title: "Showing recent history only",
                            message: "The start of this session was not loaded.", tone: .info),
                        CCBannerItem(
                            .cached, title: "Last known state, 6m old",
                            message: "Nothing live has arrived yet this launch."),
                        CCBannerItem(
                            .rejected, title: "Token rejected",
                            message: "The daemon refused this phone's pairing token.",
                            tone: .danger, icon: "lock.slash",
                            actionTitle: "Settings", action: { lastAction = "settings" }),
                        CCBannerItem(
                            .stale, title: "Link stale",
                            message: "2m since the daemon last spoke.", tone: .danger),
                    ])
                    Text("↑ four candidates in, only `rejected` renders")
                        .ccType(CC.type.monoSmall)
                        .foregroundStyle(CC.text.tertiary)
                        .padding(.bottom, CC.space.xs)

                    CCBanner(
                        "Last known state, 6m old",
                        message: "Nothing live has arrived yet this launch.",
                        tone: .warning, icon: "clock.arrow.circlepath",
                        actionTitle: "Retry", action: { lastAction = "retry" })
                    CCBanner(
                        "Showing recent history only",
                        message: "The start of this session was not loaded.",
                        tone: .info, actionTitle: "Load all", action: { lastAction = "load" })
                    CCBanner(
                        "Token rejected",
                        message: "Actions are disabled until the link is back.",
                        tone: .danger, actionTitle: "Settings", action: { lastAction = "settings" })
                    CCBanner(
                        "Answer confirmed by the daemon",
                        message: "Returned to the hook · 0.4s",
                        tone: .success)
                    CCBanner("A bare title, no message, no action.", tone: .neutral)
                }
            }
        }

        // MARK: Stat strip

        private var statStripSection: some View {
            VStack(alignment: .leading, spacing: CC.space.xxl) {
                section("CCStatStrip") {
                    VStack(alignment: .leading, spacing: CC.space.sm) {
                        // Every label reserves two lines, so the one-line labels
                        // cannot centre themselves against the two-line one.
                        // Measured on the shipped strip: label ink tops at 251,
                        // 258 and 258 — no two of the three shared a first
                        // baseline, on the screen whose whole job is to be
                        // trusted.
                        note("two `fieldLabel` lines reserved — the labels share a baseline")
                        CCStatStrip([
                            CCStat("Missed decisions", value: "—", spokenValue: "not measured"),
                            CCStat("Seq gaps", value: "0"),
                            CCStat("Reconnects", value: "2"),
                        ])
                        note("a value that is itself news takes the tone")
                        CCStatStrip([
                            CCStat("Missed decisions", value: "3", tone: .warning),
                            CCStat("Seq gaps", value: "1", tone: .danger),
                        ])

                        // The strip above and the fact row below print the same
                        // state; they must print it in the same colour.
                        // They did not: the strip drew `—` at #EDEDED / 16.91:1,
                        // identical in weight to the two real numbers beside it,
                        // while the fact row drew it at #525252 / 2.53:1.
                        note(
                            "not measured is one colour — `CCStat` and `CCFactValue` both ask `CCMeasured`, so the strip's `—` and the row's `—` cannot drift apart again"
                        )
                        CCStatStrip([
                            CCStat.unmeasured("Missed decisions"),
                            CCStat("Seq gaps", value: "0"),
                            CCStat("Reconnects", value: "2"),
                        ])
                        CCCard(padding: 0) {
                            VStack(spacing: 0) {
                                CCFactRow("Missed decisions", value: "—")
                                CCFactRow("Round trip", value: "—", separator: false)
                            }
                        }
                        note(
                            "a tone cannot outrank it — a value nobody took is not also news")
                        CCStatStrip([
                            CCStat(
                                "Missed decisions", value: "—", tone: .warning,
                                isUnmeasured: true),
                            CCStat("Reconnects", value: "2"),
                        ])
                    }
                }
            }
        }

        // MARK: Step rows

        private var stepRowSection: some View {
            section("CCStepRow") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    note("`fieldLabel` titles, dimming to tertiary once a step is done")
                    CCCard(padding: 0) {
                        VStack(spacing: 0) {
                            CCStepRow(
                                index: 1,
                                title: "Option 1 — Tailscale SSH",
                                message:
                                    "Authentication rides your tailnet ACLs, and no port is exposed anywhere.",
                                command: "tailscale up --ssh"
                            ) {
                                CCBadge("Recommended", tone: .success)
                            }
                            CCHairline()
                            CCStepRow(
                                index: 2,
                                title: "Option 2 — macOS Remote Login",
                                message:
                                    "System Settings → General → Sharing → Remote Login.",
                                isComplete: true)
                            CCHairline()
                            CCStepRow(
                                index: 3,
                                title: "Then authorise this iPhone",
                                message: "Run this at the Mac and scan the QR it prints.",
                                command: "cc pair --ssh")
                        }
                    }
                }
            }
        }

        // MARK: Empty state

        /// The empty-state mark: a 32pt glyph inside a 64pt bordered circle, and
        /// the ring scales with the glyph. All three tones, because the border
        /// takes the tone at 45% and a failure state has to read as one at a
        /// glance.
        private var emptyStateSection: some View {
            section("CCEmptyState") {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    CCCard(padding: 0) {
                        CCEmptyState(
                            glyph: "terminal",
                            title: "No agents running",
                            message: "Start one on the Mac:",
                            actionTitle: "Pair a Mac",
                            action: { lastAction = "pair" },
                            detail: {
                                CCMonoBlock("cc claude")
                                    .frame(maxWidth: 280)
                            })
                    }

                    CCCard(padding: 0) {
                        CCEmptyState(
                            glyph: "exclamationmark.triangle",
                            title: "Not connected",
                            message: "The daemon cannot be asked for a diff.",
                            tone: .warning,
                            actionTitle: "Link health",
                            action: { lastAction = "link health" })
                    }

                    CCCard(padding: 0) {
                        CCEmptyState(
                            glyph: "xmark.octagon",
                            title: "The Mac could not read the diff",
                            tone: .danger,
                            actionTitle: "Try again",
                            action: { lastAction = "retry diff" },
                            detail: {
                                CCMonoBlock(
                                    "fatal: not a git repository (or any of the parent directories): .git",
                                    tone: .danger, wraps: true, isSmall: true)
                            })
                    }
                }
            }
        }

        // MARK: Prose

        /// Backticks are markup; the span between them is an identifier.
        private var proseSection: some View {
            section("CCProse") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    note("identifiers and commands are monospace — always, and in prose too")
                    CCProse(
                        "In `ios/CodeConnect/Net/Sender.swift` lines 12–28",
                        style: CC.type.footnote, color: CC.text.secondary)
                    CCProse(
                        "The terminal will stop working until the new key is authorised at the Mac with `cc pair --ssh`.",
                        style: CC.type.body, color: CC.text.primary)
                    CCProse(
                        "Send to CodeConnect · `cc-tests`", style: CC.type.headline,
                        color: CC.text.primary)
                    note(
                        "an **odd** number of backticks is not markup — it is a string with a backtick in it, and it renders literally rather than being silently eaten"
                    )
                    CCProse(
                        "a lone ` backtick stays put", style: CC.type.footnote,
                        color: CC.text.secondary)
                }
            }
        }

        // MARK: The decision pair

        /// The specimen carries its own ruler: a `CCActionPair` over a second
        /// one holding two plain rectangles, so the split can be measured off a
        /// screenshot without having to find the edge of a button's rounded
        /// corner.
        private var actionPairSection: some View {
            section("CCActionPair") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    note(
                        "40 : 60, 12pt gap — on a 370pt bar that is 143.20 / 214.80, on the first frame"
                    )
                    CCActionBar {
                        CCActionPair {
                            CCButton("Deny", variant: .secondary, size: .lg, fullWidth: true) {
                                lastAction = "deny"
                            }
                        } allow: {
                            CCButton("Allow", variant: .primary, size: .lg, fullWidth: true) {
                                lastAction = "allow"
                            }
                        }
                    }
                    note("HIGH inverts the fill; the ratio does not move")
                    CCActionBar {
                        CCActionPair {
                            CCButton("Deny", variant: .primary, size: .lg, fullWidth: true) {
                                lastAction = "deny"
                            }
                        } allow: {
                            CCButton(
                                "Hold to allow", variant: .destructive, size: .lg,
                                fullWidth: true
                            ) { lastAction = "allow" }
                        }
                    }
                    note("the ruler — flat fills, so the split is measurable to the pixel")
                    CCActionBar {
                        CCActionPair {
                            Rectangle().fill(CC.color.danger).frame(height: CC.size.controlLg)
                        } allow: {
                            Rectangle().fill(CC.color.success).frame(height: CC.size.controlLg)
                        }
                    }
                }
            }
        }

        // MARK: Hunk header

        /// The band is 28pt and the target is 44 — two numbers that used to be
        /// one, in the wrong direction.
        private var hunkHeaderSection: some View {
            section("CCHunkHeader") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    note(
                        "a band that carries actions is a control, so it is 44pt — tap or press and hold. The gesture lives here because the hunk's own rows carry `textSelection`, which always wins the long press"
                    )
                    VStack(spacing: 0) {
                        CCHunkHeader(
                            header: "@@ -12,7 +12,9 @@ func send(_ text: String)",
                            fontSize: 12,
                            actions: CCHunkActions(
                                comment: { lastAction = "comment" },
                                copyHunk: { lastAction = "copy hunk" },
                                copyPath: { lastAction = "copy path" }))
                        ForEach(Self.hunkLines) { line in
                            CCDiffRow(
                                line: line,
                                metrics: CCDiffMetrics(fontSize: 12, availableWidth: 340))
                        }
                    }
                    .ccSurface(.surface, radius: CC.radius.md)

                    note("no actions — the band is a caption again, 28pt, and draws no glyph")
                    VStack(spacing: 0) {
                        CCHunkHeader(
                            header: "@@ -40,3 +42,3 @@ private var barWidth: CGFloat",
                            fontSize: 12)
                    }
                    .ccSurface(.surface, radius: CC.radius.md)
                }
            }
        }

        private static let hunkLines: [UnifiedDiff.Line] = [
            .init(
                id: 0, kind: .context, text: "    let payload = encode(text)", oldNumber: 12,
                newNumber: 12),
            .init(
                id: 1, kind: .deletion, text: "    try await transport.write(payload)",
                oldNumber: 13, newNumber: nil),
            .init(
                id: 2, kind: .addition, text: "    try await transport.writeAll(payload)",
                oldNumber: nil, newNumber: 13),
        ]

        // MARK: The screen's mark

        /// Two marks that mean "here is the thing this screen is about" must be
        /// one diameter at **every** size — the defect hid at `L`, which is the
        /// one step where the two ramps crossed.
        private var screenMarkSection: some View {
            section("CCScreenMark") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    note(
                        "one construction, one ramp — a shared token was not enough, because both sides already used `CC.size.emptyGlyphCircle` and still measured 150.33 against 108.67 at AX5"
                    )
                    HStack(spacing: CC.space.xl) {
                        CCScreenMark(glyph: "terminal")
                        CCScreenMark(glyph: "exclamationmark.triangle", tone: .warning)
                        CCScreenMark(glyph: "bolt.horizontal.circle", tone: .danger)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
        }

        // MARK: Terminal furniture

        /// **The component this page exists for.**
        ///
        /// `CCKeyCap` was the only thing in the kit with no reachable render:
        /// not on a gallery page, and on a screen that needs a live SSH server
        /// to reach. So "a control is 44pt" could be asserted about it and never
        /// *checked* — which is exactly the class of claim the honest-target rule
        /// exists to stop anybody making. Latched is here too, because the
        /// modifier you cannot tell is on is the one that types the wrong
        /// character.
        private var keyCapSection: some View {
            section("CCKeyCap") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    note(
                        "40pt of cap inside 44pt of finger — the 4pt is real layout, not a `contentShape` reaching outside the button. Press one: the cap contracts 6% and lifts a step"
                    )
                    keyRow
                    note(
                        "latched `ctrl` is the Vercel signature — `#EDEDED` fill, `#000` label — the same treatment a primary button and a selected chip take, because there is only one in the product"
                    )
                    HStack(spacing: CC.space.xs) {
                        CCKeyCap("ctrl", spokenLabel: "Control, on", isLatched: true) {
                            lastAction = "ctrl (latched)"
                        }
                        CCKeyCap("ctrl", spokenLabel: "Control, off") { lastAction = "ctrl" }
                    }
                }
            }
        }

        /// The accessory row exactly as `TerminalTabView` builds it, so what is
        /// reviewed here is what ships.
        private var keyRow: some View {
            HStack(spacing: CC.space.xs) {
                CCKeyCap("esc", spokenLabel: "Escape") { lastAction = "esc" }
                CCKeyCap("tab", spokenLabel: "Tab") { lastAction = "tab" }
                CCKeyCap("^C", spokenLabel: "Control C, interrupt") { lastAction = "^C" }
                CCKeyCapDivider()
                CCKeyCap(symbol: "arrow.up", spokenLabel: "Up arrow") { lastAction = "up" }
                CCKeyCap(symbol: "arrow.down", spokenLabel: "Down arrow") { lastAction = "down" }
            }
            .padding(.horizontal, CC.space.sm)
            .padding(.vertical, CC.space.xs)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(CC.color.surfaceRaised)
            .overlay(alignment: .top) { CCHairline() }
        }

        private var disclosureSection: some View {
            section("CCDisclosure") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    note(
                        "`DisclosureGroup` is banned partly because it indents its content, and what is behind one of these is a command to be read character for character against the Mac. Inset 0, full measure"
                    )
                    CCCard(padding: 0) {
                        VStack(spacing: 0) {
                            CCDisclosure("Full tool input", note: "412 B", showsTopRule: false) {
                                CCMonoBlock(
                                    "{\n  \"command\": \"git push --force origin main\",\n  \"cwd\": \"/Users/dev/app\"\n}",
                                    isSmall: true)
                            }
                            CCDisclosure(
                                "The Mac's screen when Claude asked", initiallyExpanded: true
                            ) {
                                CCMonoBlock(
                                    "❯ git status --short\n M Sources/Feature.swift", isSmall: true)
                            }
                        }
                        .padding(.vertical, CC.space.xs)
                    }
                }
            }
        }

        private var progressSection: some View {
            section("CCProgressRing") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    note(
                        "the only spinner in the app. `ProgressView` is the system's and cannot be sized honestly against a 13pt label. Under Reduce Motion it stops turning and drops to 55% — a static mark that still reads as *not finished*"
                    )
                    HStack(alignment: .center, spacing: CC.space.xl) {
                        VStack(spacing: CC.space.xs) {
                            CCProgressRing(.sm)
                            Text("sm 16").ccType(CC.type.monoSmall, color: nil)
                        }
                        VStack(spacing: CC.space.xs) {
                            CCProgressRing(.md)
                            Text("md 24").ccType(CC.type.monoSmall, color: nil)
                        }
                        VStack(spacing: CC.space.xs) {
                            CCProgressRing(.lg)
                            Text("lg 32").ccType(CC.type.monoSmall, color: nil)
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
        }

        /// Drawn over a stand-in for the camera, because the whole point of the
        /// component is that the system's own overlay does not ship — and a
        /// viewfinder reviewed on black tells you nothing about the scrim.
        private var scannerSection: some View {
            section("CCScannerFrame") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    note(
                        "the scrim is one even-odd fill, not four rectangles: four rects around a rounded hole leave four bright corners exactly where the brackets go"
                    )
                    ForEach([false, true], id: \.self) { detected in
                        ZStack {
                            // A stand-in for the camera preview: the scrim has to
                            // be reviewed over something it is dimming.
                            LinearGradient(
                                colors: [CC.color.surfaceOverlay, CC.color.surfaceRaised],
                                startPoint: .topLeading, endPoint: .bottomTrailing)
                            CCScannerFrame(
                                cutout: 180,
                                isDetected: detected,
                                caption: detected ? nil : "Point at the QR the Mac printed")
                        }
                        .frame(height: 300)
                        .clipShape(
                            RoundedRectangle(cornerRadius: CC.radius.lg, style: .continuous))
                    }
                }
            }
        }

        // MARK: The shape of a wait

        /// *Every wait has a shape, a sentence, and a ticking elapsed counter.*
        /// All three, side by side, at the two elapsed times where the sentence
        /// changes.
        private var waitSection: some View {
            section("CCSkeleton") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    note(
                        "**no shimmer, ever.** A travelling highlight is an animation that implies progress, and this is the state where there may be none — on a stalled link it is a lie told sixty times a second"
                    )
                    CCCard(padding: 0) {
                        VStack(spacing: 0) {
                            CCSkeletonRow(shape: .fleetRow)
                            CCHairline()
                            CCSkeletonRow(shape: .fleetRow)
                        }
                    }
                    CCCard {
                        VStack(alignment: .leading, spacing: CC.space.sm) {
                            CCSkeletonRow(shape: .toolRow)
                            CCSkeletonRow(shape: .proseRow)
                        }
                    }

                    note("the sentence, before and after it stops being normal")
                    CCCard {
                        VStack(alignment: .leading, spacing: CC.space.sm) {
                            CCWaitingNotice(elapsed: 3)
                            CCWaitingNotice(elapsed: 21) { lastAction = "check link" }
                        }
                    }
                }
            }
        }

        // MARK: The diff grid

        private var diffChipSection: some View {
            section("CCDiffFileChip") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    note(
                        "32pt of chip, 44pt of finger, and **no selected state** — the strip is a jump list, and the document does not know which file is under the fold. Marking the last one tapped would be wrong the moment you scrolled"
                    )
                    ScrollView(.horizontal, showsIndicators: false) {
                        HStack(spacing: CC.space.xs) {
                            ForEach(Self.files) { file in
                                CCDiffFileChip(file: file) { lastAction = file.shortName }
                            }
                        }
                    }
                    .scrollBounceBehavior(.basedOnSize, axes: .horizontal)

                    note(
                        "the sticky band. At accessibility sizes it draws the **basename** rather than head-truncating a path down to `….swift` — a shorter true string beats a longer truncated one"
                    )
                    VStack(spacing: 0) {
                        ForEach(Self.files) { file in
                            CCDiffFileHeader(file: file)
                        }
                    }
                    .ccSurface(.surface, radius: CC.radius.md)
                }
            }
        }

        private var diffGridSection: some View {
            section("CCDiffRow") {
                VStack(alignment: .leading, spacing: CC.space.sm) {
                    note(
                        "three parts to the change signal — a 2pt bar, a marker glyph and a **6%** row tint. Not 12%: over a full-bleed mono block a 12% fill reads as a highlighter and after two screens you stop seeing it"
                    )
                    let metrics = CCDiffMetrics(fontSize: 12, availableWidth: 340)
                    let map = CCDiffWordHighlight.map(for: Self.gridLines)
                    VStack(spacing: 0) {
                        CCHunkHeader(
                            header: "@@ -12,7 +12,9 @@ func send(_ text: String)", fontSize: 12)
                        ForEach(Self.gridLines) { line in
                            CCDiffRow(
                                line: line, metrics: metrics, wordRanges: map[line.id] ?? [])
                        }
                        CCFoldRow(count: 6, metrics: metrics) { lastAction = "expand fold" }
                    }
                    .ccSurface(.surface, radius: CC.radius.md)

                    note(
                        "the fold is a control, so it is 44pt — and its `⋯` is `textTertiary`, not the 2.54:1 `textDisabled` reserved for gutter numbers and inactive chrome"
                    )
                    CCGapMarker(label: "Truncated at 512KB")
                }
            }
        }

        private static let files: [UnifiedDiff.FileDiff] = [
            .init(
                id: "ios/CodeConnect/Net/Sender.swift",
                oldPath: "ios/CodeConnect/Net/Sender.swift",
                newPath: "ios/CodeConnect/Net/Sender.swift",
                status: .modified, hunks: [], notes: []),
            .init(
                id: "ios/CodeConnect/Views/DesignSystem/CCKeyCap.swift", oldPath: nil,
                newPath: "ios/CodeConnect/Views/DesignSystem/CCKeyCap.swift",
                status: .added, hunks: [], notes: []),
            .init(
                id: "mac/daemon/src/legacy.rs",
                oldPath: "mac/daemon/src/legacy.rs", newPath: nil,
                status: .deleted, hunks: [], notes: []),
            .init(
                id: "docs/PLAN.md", oldPath: "docs/PLAN.md", newPath: "docs/ROADMAP.md",
                status: .renamed, hunks: [], notes: []),
        ]

        /// Long enough to wrap at 340pt, so the `↳` is on screen without a drag —
        /// it is the mark that says a break fell inside a token, and it is the
        /// one thing on the grid that may not be dimmer than the code.
        private static let gridLines: [UnifiedDiff.Line] = [
            .init(
                id: 0, kind: .context, text: "    let payload = encode(text)", oldNumber: 12,
                newNumber: 12),
            .init(
                id: 1, kind: .deletion,
                text: "    try await transport.write(payload, deadline: .now() + .seconds(30))",
                oldNumber: 13, newNumber: nil),
            .init(
                id: 2, kind: .addition,
                text: "    try await transport.writeAll(payload, deadline: .now() + .seconds(45))",
                oldNumber: nil, newNumber: 13),
            .init(id: 3, kind: .context, text: "    return", oldNumber: 14, newNumber: 14),
        ]

        // MARK: Scaffolding

        private func section<Content: View>(
            _ title: String, @ViewBuilder content: () -> Content
        ) -> some View {
            VStack(alignment: .leading, spacing: CC.space.sm) {
                CCSectionHeader(title)
                content()
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            // The anchor `CC_GALLERY_SECTION` scrolls to.
            .id(title)
        }

        private func labelled<Content: View>(
            _ title: String, @ViewBuilder content: () -> Content
        ) -> some View {
            VStack(alignment: .leading, spacing: CC.space.xxs) {
                Text(title.uppercased())
                    .ccType(CC.type.micro)
                    .foregroundStyle(CC.text.tertiary)
                // Wraps, so eight badges at AX5 do not run off the edge.
                CCFlowLayout(spacing: CC.space.xs, lineSpacing: CC.space.xs) {
                    content()
                }
            }
        }
    }

    // MARK: - Flow layout

    /// Wrapping row of chips. Only used by the gallery — screens lay their
    /// badges out deliberately — but without it a row of eight badges at AX5 is
    /// a row of three badges and five that ran off the screen, which would make
    /// the accessibility previews lie.
    struct CCFlowLayout: Layout {
        var spacing: CGFloat = CC.space.xs
        var lineSpacing: CGFloat = CC.space.xs

        func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout Void)
            -> CGSize
        {
            let maxWidth = proposal.width ?? .infinity
            var x: CGFloat = 0
            var y: CGFloat = 0
            var lineHeight: CGFloat = 0
            for subview in subviews {
                let size = subview.sizeThatFits(.unspecified)
                if x > 0, x + size.width > maxWidth {
                    x = 0
                    y += lineHeight + lineSpacing
                    lineHeight = 0
                }
                x += size.width + spacing
                lineHeight = max(lineHeight, size.height)
            }
            return CGSize(width: maxWidth == .infinity ? x : maxWidth, height: y + lineHeight)
        }

        func placeSubviews(
            in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout Void
        ) {
            var x = bounds.minX
            var y = bounds.minY
            var lineHeight: CGFloat = 0
            for subview in subviews {
                let size = subview.sizeThatFits(.unspecified)
                if x > bounds.minX, x + size.width > bounds.maxX {
                    x = bounds.minX
                    y += lineHeight + lineSpacing
                    lineHeight = 0
                }
                subview.place(
                    at: CGPoint(x: x, y: y), anchor: .topLeading,
                    proposal: ProposedViewSize(size))
                x += size.width + spacing
                lineHeight = max(lineHeight, size.height)
            }
        }
    }

    // MARK: - Sheet chrome, previewed in situ

    private struct CCSheetChromeDemo: View {
        @State private var showing = true

        var body: some View {
            ZStack {
                CC.color.bg.ignoresSafeArea()
                CCButton("Open sheet", size: .lg) { showing = true }
            }
            .sheet(isPresented: $showing) {
                // Worth opening at AX5: the close cross and its ring scale
                // together now, where the ring used to stay at 28pt and the
                // glyph climbed straight out of it.
                CCSheetChrome(
                    "Decision",
                    subtitle: "codeconnect · Bash",
                    onClose: { showing = false }
                ) {
                    CCBadge(risk: .high)
                } content: {
                    ScrollView {
                        VStack(alignment: .leading, spacing: CC.space.md) {
                            CCMonoBlock("rm -rf / --no-preserve-root")
                            Text(
                                "Nothing decides this but you. There is no timer on this card."
                            )
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.text.secondary)
                        }
                        .padding(CC.space.md)
                    }
                    .safeAreaInset(edge: .bottom) {
                        CCActionBar {
                            HStack(spacing: CC.space.xs) {
                                CCButton(
                                    "Deny", variant: .destructive, size: .lg, fullWidth: true,
                                    action: {})
                            }
                            CCHoldButton("Hold to allow", action: {})
                        }
                    }
                }
                .presentationDetents([.large])
            }
        }
    }

    // MARK: - Previews

    #Preview("Gallery · default") {
        CCGallery()
    }

    #Preview("Gallery · xxxLarge") {
        CCGallery()
            .dynamicTypeSize(.xxxLarge)
    }

    #Preview("Gallery · AX3") {
        CCGallery()
            .dynamicTypeSize(.accessibility3)
    }

    #Preview("Gallery · AX5 (worst case)") {
        CCGallery()
            .dynamicTypeSize(.accessibility5)
    }

    // Per-page previews, so a component can be worked on without the other six
    // recompiling in the canvas.
    #Preview("Buttons") { CCGallery(page: .buttons) }
    #Preview("Buttons · AX5") { CCGallery(page: .buttons).dynamicTypeSize(.accessibility5) }
    #Preview("Rows") { CCGallery(page: .rows) }
    #Preview("Rows · AX5") { CCGallery(page: .rows).dynamicTypeSize(.accessibility5) }
    #Preview("Indicators") { CCGallery(page: .indicators) }
    #Preview("Indicators · AX5") {
        CCGallery(page: .indicators).dynamicTypeSize(.accessibility5)
    }
    #Preview("Controls") { CCGallery(page: .controls) }
    #Preview("Controls · AX5") { CCGallery(page: .controls).dynamicTypeSize(.accessibility5) }
    #Preview("Identity") { CCGallery(page: .identity) }
    #Preview("Feedback") { CCGallery(page: .feedback) }
    #Preview("Terminal") { CCGallery(page: .terminal) }
    #Preview("Terminal · AX5") { CCGallery(page: .terminal).dynamicTypeSize(.accessibility5) }
    #Preview("Diff") { CCGallery(page: .diff) }
    #Preview("Diff · AX5") { CCGallery(page: .diff).dynamicTypeSize(.accessibility5) }

    // Reduce Motion has no writable environment key, so it cannot be forced
    // from a preview. Verify it in the Simulator:
    //   Settings → Accessibility → Motion → Reduce Motion.
    // Expected: no press-scale, no pulsing dots, no spinner rotation; every
    // transition becomes a 120ms cross-fade; every haptic still fires.

    #Preview("Sheet chrome") {
        CCSheetChromeDemo()
            .ccAppearance()
    }

#endif
