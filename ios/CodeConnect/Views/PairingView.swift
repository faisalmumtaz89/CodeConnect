import SwiftUI
import UIKit

/// Pairing and settings — two screens in one file, split by purpose.
///
/// **Onboarding** is the first thing anyone sees: a full-screen flow with one
/// job. **Settings** is configuration: pairing, transport, SSH, unpair. What it
/// deliberately no longer is, is a second Link Health screen — the numbers live
/// in one place and this one links to it, rather than restating connection,
/// transport and capability rows in a second style.
///
/// Both ways in are kept: scanning the QR `codeconnect pair` prints is the fast path and
/// the one that can hand back a device token; typing an address and a `codeconnect token`
/// still works, because a camera that will not focus at 2am must never be the
/// only way to reach your own Mac.
struct PairingView: View {
    var isOnboarding = false

    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    @State private var address = ""
    @State private var token = ""
    @State private var validationError: String?
    @State private var showScanner = false
    @State private var scanError: String?
    @State private var showManualEntry = false
    @State private var showTerminalSettings = false
    @State private var showLinkHealth = false
    @State private var confirmingUnpair = false
    @State private var pairingStartedAt: Date?

    var body: some View {
        Group {
            if isOnboarding {
                onboarding
            } else {
                settings
            }
        }
        .onAppear(perform: prefill)
        .sheet(isPresented: $showScanner) {
            PairingScanSheet(
                onScan: { payload in
                    showScanner = false
                    handle(scanned: payload)
                },
                onManualEntry: {
                    showScanner = false
                    withAnimation(CC.motion.small) { showManualEntry = true }
                })
        }
    }

    // MARK: - Onboarding

    /// No navigation bar, no `Form`, no sections: one screen, one job, and
    /// the command you have to run at the Mac rendered as something you can
    /// actually copy — it used to be inline backticks inside a footnote, on the
    /// one screen where the user is sitting at a keyboard.
    private var onboarding: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                if model.isPairing {
                    pairingProgress
                        .transition(.opacity)
                } else {
                    pairingInvitation
                        .transition(.opacity)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, CC.space.md)
            .padding(.vertical, CC.space.xxl)
            .ccAnimation(CC.motion.medium, value: model.isPairing)
        }
        .background(CC.color.bg)
        .scrollDismissesKeyboard(.interactively)
    }

    @ViewBuilder
    private var pairingInvitation: some View {
        Text("Pair with your Mac")
            .ccType(CC.type.display)
            .foregroundStyle(CC.text.primary)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.bottom, CC.space.xxl)

        Text("Run this on the Mac running ccd, then scan the code it prints.")
            .ccType(CC.type.body)
            .foregroundStyle(CC.text.secondary)
            .fixedSize(horizontal: false, vertical: true)
            // 50–75 characters is the readable measure.
            .frame(maxWidth: 320, alignment: .leading)
            .padding(.bottom, CC.space.xl)

        CCCard(padding: 0) {
            VStack(spacing: 0) {
                CCStepRow(index: 1, title: "At the Mac", command: "codeconnect pair")
                CCHairline()
                CCStepRow(
                    index: 2,
                    title: "Then scan the code it prints",
                    message:
                        "Add --ssh if you also want the live terminal. It authorises this iPhone's key, and tells you so first.")
            }
        }
        .padding(.bottom, CC.space.xxl)

        // A scan error is not a dead end and the primary stays enabled —
        // retrying is the expected action.
        if let scanError {
            CCBanner("Could not read that code", message: scanError, tone: .warning)
                .padding(.bottom, CC.space.md)
        }
        if let error = model.pairingError {
            CCBanner("Pairing failed", message: error, tone: .danger)
                .padding(.bottom, CC.space.md)
        }

        if QRScannerView.isSupported {
            CCButton(
                "Scan pairing code", icon: "qrcode.viewfinder", variant: .primary, size: .lg,
                fullWidth: true
            ) {
                scanError = nil
                showScanner = true
            }

            if !showManualEntry {
                CCButton(
                    "Enter an address and token instead", variant: .ghost, size: .md,
                    fullWidth: true
                ) {
                    withAnimation(CC.motion.small) { showManualEntry = true }
                }
                .padding(.top, CC.space.md)
            }
        } else {
            // Above the button, not below it: this sentence is the *reason* the
            // primary action changed, and a reason that arrives after the
            // control it explains has already been read is not an explanation.
            // Found by rendering — the Simulator has no scanner, which is
            // exactly the case this copy exists for.
            Text("No scanner on this device — pair by hand below.")
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.bottom, CC.space.sm)

            CCButton("Pair by hand", variant: .primary, size: .lg, fullWidth: true) {
                withAnimation(CC.motion.small) { showManualEntry = true }
            }
        }

        if showManualEntry {
            manualEntry
                .padding(.top, CC.space.xl)
                .transition(.opacity)
        }

        Text("The code is single-use and expires five minutes after codeconnect pair prints it.")
            .ccType(CC.type.footnote)
            .foregroundStyle(CC.text.tertiary)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: 320, alignment: .leading)
            .padding(.top, CC.space.xl)
    }

    /// The card is replaced in place rather than growing a spinner underneath
    /// it: what is happening is the only thing happening.
    private var pairingProgress: some View {
        VStack(alignment: .leading, spacing: CC.space.md) {
            CCProgressRing(.lg)

            Text("Exchanging the code for a device token…")
                .ccType(CC.type.headline)
                .foregroundStyle(CC.text.primary)
                .fixedSize(horizontal: false, vertical: true)

            Text("The code is spent on this one connection and never stored.")
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)

            Text(Format.age(pairingElapsed))
                .ccType(CC.type.monoSmall)
                .foregroundStyle(CC.text.tertiary)

            if let error = model.pairingError {
                CCBanner("Pairing failed", message: error, tone: .danger)
            }

            // `ghost`, which draws its own 1pt edge at rest now, so a recovery
            // control no longer has to be promoted to a louder variant just to
            // be visible. This is the only way out of a pairing exchange that
            // has stalled, and it stands alone in whitespace: borderless it was
            // a bare centred label, indistinguishable from the body copy above
            // it.
            CCButton("Cancel pairing", variant: .ghost, size: .md) {
                model.cancelPairing()
                pairingStartedAt = nil
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .onAppear { pairingStartedAt = pairingStartedAt ?? Date() }
        .onDisappear { pairingStartedAt = nil }
    }

    private var pairingElapsed: TimeInterval {
        guard let pairingStartedAt else { return 0 }
        return max(0, model.now.timeIntervalSince(pairingStartedAt))
    }

    // MARK: - Manual entry

    /// The keyboard path. It never goes away, on any screen, for any reason.
    @ViewBuilder
    private var manualEntry: some View {
        VStack(alignment: .leading, spacing: CC.space.md) {
            // **Flush, and in a card, and the two go together.**
            //
            // The header owns the content column now, so it lands on 52 from
            // either of this block's two homes — inline under the onboarding
            // screen's 16pt page inset, and inside the repair sheet's. It was
            // the one header in the app with no inset at all (x=16.00), because
            // a call site cannot be told what column it is on.
            //
            // Moving it alone would have been half a fix: the rule the eleven
            // headers broke is *a header agrees with the rows it names*, and
            // these two fields sat on the page column at 16, so a header on 52
            // would have been adrift of its own content by the same 36pt — in
            // the other direction. The fields take the content column instead,
            // in the card that is what puts text there, which is exactly how
            // `TerminalSettingsView.whereToConnect` draws the same object: a
            // section header over a card of `CCField`s. The fields themselves
            // are untouched — 52pt tall, `micro` labels, both placeholders
            // verbatim — and the block still expands in place.
            //
            // `Save and connect` stays outside it: the card holds what you type,
            // and the button that spends it belongs to the page, at page width,
            // like every other primary in the app.
            CCSectionHeader("Or pair by hand")

            CCCard {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    CCField(
                        label: "Address",
                        text: $address,
                        placeholder: "100.x.y.z or ws://host:8787",
                        hint:
                            "Port \(DaemonEndpoint.defaultPort) is assumed when you leave it out.",
                        keyboardType: .URL,
                        autocapitalization: .never,
                        disableAutocorrection: true,
                        isMono: true)

                    CCField(
                        label: "Code or token",
                        text: $token,
                        placeholder: "Pairing code or token",
                        hint: nil,
                        error: validationError,
                        autocapitalization: .characters,
                        disableAutocorrection: true,
                        isMono: true)

                    // A *confirmation*, not a warning: the app is telling you it
                    // read what you typed the way you meant it. It qualifies the
                    // field above it, so it lives with it.
                    if let credential = PairingCredentialInput.classify(token) {
                        HStack(alignment: .firstTextBaseline, spacing: CC.space.xxs + 1) {
                            CCIcon(
                                "checkmark.circle.fill", size: 11, weight: .semibold,
                                relativeTo: .caption
                            )
                            .foregroundStyle(CC.color.success)
                            Text(PairingCredentialInput.describe(credential))
                                .ccType(CC.type.footnote)
                                .foregroundStyle(CC.color.success)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                }
            }

            CCButton(
                "Save and connect", variant: .primary, size: .lg, fullWidth: true,
                disabledReason: manualEntryBlockedReason
            ) {
                save()
            }
        }
    }

    private var manualEntryBlockedReason: CCDisabledReason? {
        if address.trimmingCharacters(in: .whitespaces).isEmpty {
            return CCDisabledReason("An address is needed — the one `codeconnect pair` printed.")
        }
        if token.trimmingCharacters(in: .whitespaces).isEmpty {
            return CCDisabledReason("A pairing code or a `codeconnect token` is needed.")
        }
        return nil
    }

    // MARK: - Settings

    /// Configuration only. The link's *numbers* live on Link Health and are not
    /// restated here — two screens showing the same facts with different styling
    /// was an IA bug, not just a visual one.
    private var settings: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: CC.space.xl) {
                    macSection
                    transportSection
                    capabilitiesSection
                    unpairSection
                }
                .padding(.horizontal, CC.space.md)
                .padding(.vertical, CC.space.lg)
            }
            .background(CC.color.bg)
            // On scroll, `bg` fades in over the first 24pt of travel, expressed
            // in the platform's own terms — see `ccNavigationChrome`.
            .ccNavigationChrome()
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { doneItem }
            .navigationDestination(isPresented: $showTerminalSettings) {
                TerminalSettingsView().environment(model)
            }
            .sheet(isPresented: $showLinkHealth) {
                LinkHealthSheet().environment(model)
            }
            .sheet(isPresented: $showManualEntry) {
                repairSheet
            }
        }
    }

    /// The sheet's dismissal. Drawn by us, and — on the platform that offers it
    /// — stripped of the Liquid Glass capsule the toolbar wraps every item in,
    /// which is a second, differently-shaped surface on a screen whose whole
    /// argument is that surfaces differ by a few percent of luminance.
    ///
    /// The availability dance is `ccPlainToolbarItem`'s now, so this screen and
    /// every other toolbar in the app opt out the same way.
    private var doneItem: some ToolbarContent {
        ToolbarItem(placement: .cancellationAction) { doneButton }
            .ccPlainToolbarItem()
    }

    private var doneButton: some View {
        Button {
            dismiss()
        } label: {
            Text("Done")
                .ccType(CC.type.callout.weight(.semibold))
                .foregroundStyle(CC.text.primary)
        }
        .buttonStyle(.plain)
        .ccHitTarget(minWidth: CC.size.hitTarget)
    }

    private var macSection: some View {
        VStack(alignment: .leading, spacing: CC.space.sm) {
            // Flush: the header owns the content column itself now — see
            // `TerminalSettingsView.keySection`. Anything added here is added
            // twice.
            CCSectionHeader("Mac")

            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    // The one row in the app outside a settings list that earns
                    // a chevron, and it earns it because the row below it does
                    // not have one — a chevron only means anything when it
                    // distinguishes.
                    CCRow(
                        model.pairing.endpoint?.host ?? "Not paired",
                        meta: model.pairing.endpoint?.displayAddress,
                        action: { showLinkHealth = true }
                    ) {
                        CCStatusDot(
                            color: model.linkHealth.level.ccTone.color,
                            isHollow: model.linkHealth.level == .offline,
                            pulses: model.linkHealth.level == .connecting,
                            accessibilityText: "Link health")
                    } trailing: {
                        Text(model.linkHealth.shortText)
                            .ccType(CC.type.monoSmall)
                            .foregroundStyle(CC.text.tertiary)
                            .fixedSize()
                    }

                    CCRow("Terminal and SSH", action: { showTerminalSettings = true }) {
                        CCIcon("terminal", size: CC.size.icon, weight: .medium)
                            .foregroundStyle(CC.text.tertiary)
                    }

                    CCRow(
                        "Scan a new pairing code",
                        subtitle: "Replaces the token this iPhone holds. Nothing is unpaired first.",
                        showsChevron: false,
                        separator: false,
                        action: {
                            scanError = nil
                            showScanner = true
                        }
                    ) {
                        CCIcon("qrcode.viewfinder", size: CC.size.icon, weight: .medium)
                            .foregroundStyle(CC.text.tertiary)
                    }
                }
            }
        }
    }

    /// Prefer the daemon's own report over what this end asked for.
    private var encrypted: Bool {
        model.daemonProfile.connectionEncrypted || model.connection.usingTLS
    }

    /// Amber means **something is degraded**, and the only degraded transport is
    /// a daemon that holds a certificate this socket is not using.
    ///
    /// The row shipped as `tone: encrypted ? .neutral : .warning`, which painted
    /// plain `ws` amber on a daemon reporting `Holds a certificate: no` — the
    /// designed configuration, inside the tailnet, with the footnote directly
    /// beneath the row explaining that WireGuard carries the encryption.
    /// "If a colour isn't telling you something actionable, it's a bug", and a
    /// warning nobody can act on is also the one that gets ignored on the day it
    /// is real. Link Health computes this same fact this same way; two screens
    /// disagreeing about whether one socket is degraded is worse than either
    /// answer.
    private var transportTone: CCTone {
        guard !encrypted else { return .neutral }
        return model.connection.capabilities?.tls == true ? .warning : .neutral
    }

    /// Everything that qualifies the transport row, in prose, under it. Both can
    /// be true at once — a daemon that holds a certificate it is not serving on
    /// *this* socket, and a stated reason why — and neither is the other's
    /// paraphrase.
    private var transportNotes: [String] {
        var notes: [String] = []
        if let capabilities = model.connection.capabilities, capabilities.tls, !encrypted {
            notes.append(
                "The daemon holds a certificate but this connection is not using it.")
        }
        if let reason = model.pairing.tlsUnavailableReason {
            notes.append(reason)
        }
        return notes
    }

    private var transportSection: some View {
        VStack(alignment: .leading, spacing: CC.space.sm) {
            CCSectionHeader("Transport")

            CCCard(padding: 0) {
                VStack(alignment: .leading, spacing: 0) {
                    // The daemon reports whether *this* connection is encrypted,
                    // separately from whether it holds a certificate at all.
                    // Both are shown, because "the server can do TLS" and "you
                    // are using it" are different facts and only one of them is
                    // about you.
                    //
                    // `wss` / `ws` and nothing else in the value: the shipped
                    // row read `ws (plain)`, where `plain` is an English word
                    // wearing monospace. What qualifies the transport is a
                    // sentence, and there is a slot for it below.
                    CCFactRow(
                        "Encryption",
                        value: encrypted ? "wss" : "ws",
                        tone: transportTone,
                        separator: false,
                        accessibilityValueText: encrypted
                            ? "wss, TLS" : "ws, not encrypted by TLS"
                    )

                    ForEach(transportNotes, id: \.self) { note in
                        CCHairline()
                        Text(note)
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.color.warning)
                            .fixedSize(horizontal: false, vertical: true)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.leading, CCColumn.content)
                            .padding(.trailing, CC.space.md)
                            .padding(.vertical, CC.space.sm)
                    }
                }
            }

            Text(
                encrypted
                    ? "The daemon serves its own certificate from `tailscale cert`."
                    : "Plain ws:// inside the tailnet — WireGuard carries the encryption. The app moves to wss:// as soon as the daemon offers a certificate for its MagicDNS name."
            )
            .ccType(CC.type.footnote)
            .foregroundStyle(CC.text.tertiary)
            .fixedSize(horizontal: false, vertical: true)
        }
    }

    @ViewBuilder
    private var capabilitiesSection: some View {
        VStack(alignment: .leading, spacing: CC.space.sm) {
            // The note slot rather than a second sentence: when there is no
            // daemon there is nothing to list, and the header says so before the
            // reader gets to an empty card.
            CCSectionHeader(
                "What this daemon advertises",
                note: model.connection.capabilities == nil ? "not connected" : nil
            )

            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    if let capabilities = model.connection.capabilities {
                        let rows = capabilities.advertisedRows
                        ForEach(Array(rows.enumerated()), id: \.element.name) { index, row in
                            // `CCFactRow`, not fifteen hand-written lines: the
                            // 48pt floor, the `key` label style and the stacked
                            // form at accessibility sizes all come from one place
                            // now.
                            //
                            // The separator moves outside the row so it stays
                            // full card width while the row's *text* takes the
                            // content column — the shipped list drew its hairline
                            // at a 16pt leading inset and flush right, an
                            // asymmetry nothing else in the app has.
                            if index > 0 { CCHairline() }
                            CCFactRow(
                                row.name, value: row.value, labelStyle: .key, separator: false
                            )
                        }
                    } else {
                        // Never a guess and never a blank: the app says it does
                        // not know, in the daemon's absence.
                        Text("Unknown — not connected.")
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.text.tertiary)
                            .fixedSize(horizontal: false, vertical: true)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.leading, CCColumn.content)
                            .padding(.trailing, CC.space.md)
                            .padding(.vertical, CC.space.md)
                    }
                }
            }

            Text(
                "Listed exactly as the daemon reported it, including anything this app version does not have a name for."
            )
            .ccType(CC.type.footnote)
            .foregroundStyle(CC.text.tertiary)
            .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var unpairSection: some View {
        VStack(alignment: .leading, spacing: CC.space.sm) {
            CCButton("Unpair and erase cache", variant: .destructive, size: .lg, fullWidth: true) {
                confirmingUnpair = true
            }

            Text(
                "Removes the token from the Keychain and deletes cached events. The SSH key stays until you remove it under Terminal and SSH; revoke it at the Mac with `codeconnect ssh-revoke`."
            )
            .ccType(CC.type.footnote)
            .foregroundStyle(CC.text.tertiary)
            .fixedSize(horizontal: false, vertical: true)
        }
        .confirmationDialog(
            "Unpair and erase cache?", isPresented: $confirmingUnpair, titleVisibility: .visible
        ) {
            Button("Unpair", role: .destructive) {
                model.unpair()
                dismiss()
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This iPhone will need a new pairing code before it can see the fleet again.")
        }
    }

    /// Manual entry, presented as a sheet from Settings rather than inline —
    /// on this screen it is a repair job, not the main event.
    private var repairSheet: some View {
        CCSheetChrome("Pair by hand", onClose: { showManualEntry = false }) {
            ScrollView {
                manualEntry
                    .padding(CC.space.md)
            }
        }
        .presentationDetents([.medium, .large])
    }

    // MARK: - Actions

    private func prefill() {
        guard let endpoint = model.pairing.endpoint else { return }
        address = endpoint.displayAddress
        token = endpoint.token ?? ""
    }

    private func handle(scanned payload: String) {
        switch PairingQRPayload.decode(payload) {
        case .success(let parsed):
            scanError = nil
            pairingStartedAt = Date()
            model.pair(withQR: parsed)
        case .failure(let failure):
            scanError = failure.localizedDescription
        }
    }

    private func save() {
        guard let credential = PairingCredentialInput.classify(token) else {
            validationError =
                "That is neither an eight-character pairing code nor a `codeconnect token`."
            return
        }
        guard let endpoint = DaemonEndpoint.parse(address: address, credential: credential) else {
            validationError = "That address could not be parsed. Try 100.x.y.z or ws://host:8787."
            return
        }
        validationError = nil
        switch credential {
        case .token:
            model.pair(with: endpoint)
            if !isOnboarding { showManualEntry = false }
        case .pairingCode(let code):
            // A typed code is the same one-shot exchange the QR performs; it
            // must not be stored, so it goes through the pairing path.
            pairingStartedAt = Date()
            model.pair(
                withQR: PairingQRPayload(host: endpoint.host, port: endpoint.port, code: code))
            if !isOnboarding { showManualEntry = false }
        }
    }
}

// MARK: - Scan sheet

/// Full-screen camera with the one instruction that matters.
///
/// Apple's `DataScanner` guidance label and yellow highlight are switched off
/// and ours is drawn instead: a system chip reading "QR Code" hovering over this
/// palette is the most obviously borrowed pixel in the product.
struct PairingScanSheet: View {
    let onScan: (String) -> Void
    /// Every camera failure offers a keyboard path. No dead ends.
    var onManualEntry: () -> Void = {}

    @Environment(\.dismiss) private var dismiss
    @State private var error: String?
    @State private var detected = false
    @State private var torchOn = false

    var body: some View {
        ZStack {
            CC.color.bg.ignoresSafeArea()

            if let error {
                unavailable(
                    glyph: "exclamationmark.triangle",
                    title: "The scanner stopped",
                    message: nil,
                    detail: error,
                    primaryTitle: "Try again",
                    primary: { self.error = nil })
            } else if !QRScannerView.isSupported {
                unavailable(
                    glyph: "camera.metering.unknown",
                    title: "No scanner on this device",
                    message:
                        "This device cannot run the code scanner. Close this and pair with an address and a `codeconnect token` instead.",
                    detail: nil,
                    primaryTitle: "Pair by hand",
                    primary: onManualEntry,
                    secondaryTitle: nil)
            } else if !QRScannerView.isAvailable {
                unavailable(
                    glyph: "camera.metering.unknown",
                    title: "Camera access is off",
                    message:
                        "CodeConnect does not have camera access, so it cannot read the code. Grant it in Settings, or pair by hand.",
                    detail: nil,
                    primaryTitle: "Open Settings",
                    primary: openSystemSettings)
            } else {
                camera
            }
        }
        .overlay(alignment: .topLeading) { cancelButton }
        .background(CC.color.bg)
        .presentationBackground(CC.color.bg)
    }

    private var camera: some View {
        QRScannerView(
            onScan: { payload in
                guard !detected else { return }
                detected = true
                CCHaptic.success.fire()
                // Held for 400ms before dismissing: an instant dismiss leaves
                // the user unsure whether it worked, and a pairing code is
                // single-use — the reassurance is worth 400ms.
                Task {
                    try? await Task.sleep(for: .milliseconds(400))
                    onScan(payload)
                }
            },
            onError: { error = $0 })
        .ignoresSafeArea()
        .overlay {
            CCScannerFrame(
                isDetected: detected,
                caption: "Point the camera at the QR code in your Mac's terminal.")
            .ignoresSafeArea()
        }
        .overlay(alignment: .bottom) { torchButton }
        .accessibilityLabel("Point the camera at the QR code printed by codeconnect pair on your Mac.")
    }

    private func unavailable(
        glyph: String, title: String, message: String?, detail: String?,
        primaryTitle: String?, primary: (() -> Void)?,
        secondaryTitle: String? = "Pair by hand"
    ) -> some View {
        ScrollView {
            CCEmptyState(
                glyph: glyph,
                title: title,
                message: message,
                tone: .warning,
                actionTitle: primaryTitle,
                action: primary,
                secondaryActionTitle: secondaryTitle,
                secondaryAction: secondaryTitle == nil ? nil : onManualEntry
            ) {
                if let detail {
                    // The raw error, verbatim, never paraphrased.
                    CCMonoBlock(detail, tone: .neutral, wraps: true, isSmall: true)
                        .padding(.horizontal, CC.space.md)
                }
            }
            .padding(.top, CC.space.xxxl)
        }
    }

    /// `secondary`, not `ghost`, and this one stays: it sits over a **live
    /// camera feed**, where `textSecondary` on whatever the lens happens to be
    /// pointing at has no measurable contrast. `secondary`'s full-strength label
    /// is the difference between a control and a smudge; the 1pt edge both
    /// variants now draw is not enough on its own here.
    private var cancelButton: some View {
        CCButton("Cancel", variant: .secondary, size: .md) { dismiss() }
            .padding(CC.space.md)
    }

    /// Small, real, and the thing people reach for in a dim office.
    @ViewBuilder
    private var torchButton: some View {
        if QRScannerView.hasTorch {
            CCButton(
                torchOn ? "Torch on" : "Torch", icon: torchOn ? "bolt.fill" : "bolt",
                variant: .secondary, size: .md
            ) {
                torchOn = QRScannerView.setTorch(!torchOn)
            }
            .padding(.bottom, CC.space.xxl)
        }
    }

    private func openSystemSettings() {
        guard let url = URL(string: UIApplication.openSettingsURLString) else { return }
        UIApplication.shared.open(url)
    }
}
