import CodeConnectKit
import SwiftUI
import UIKit

/// Pairing and settings — two screens in one file, split by purpose.
///
/// **Onboarding** is the first thing anyone sees: a full-screen flow with one
/// job. **Settings** is configuration: pairing, transport, unpair. What it
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

    /// The machine paired most recently (`Machines.current`), or the one model
    /// with nothing paired. A pairing goes through `Machines`, which adds a
    /// machine rather than repointing this one.
    @Environment(AppModel.self) private var model
    @Environment(Machines.self) private var machines
    @Environment(\.dismiss) private var dismiss
    @Environment(\.openURL) private var openURL

    @State private var address = ""
    @State private var token = ""
    @State private var validationError: String?
    @State private var showScanner = false
    @State private var scanError: String?
    @State private var showManualEntry = false
    @State private var openMachineID: String?
    @State private var pairingStartedAt: Date?

    var body: some View {
        Group {
            if isOnboarding {
                onboarding
            } else {
                settings
            }
        }
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
                if machines.isPairing {
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
            .ccAnimation(CC.motion.medium, value: machines.isPairing)
        }
        .background(CC.color.bg)
        // No navigation bar draws behind the status bar here, so scrolled text
        // would run under the clock: the status bar keeps the screen's own
        // background instead.
        .overlay(alignment: .top) {
            GeometryReader { proxy in
                CC.color.bg
                    .frame(height: proxy.safeAreaInsets.top)
                    .ignoresSafeArea(edges: .top)
            }
            .allowsHitTesting(false)
        }
        .scrollDismissesKeyboard(.interactively)
    }

    /// The app introducing itself, because this is the first thing a stranger sees.
    ///
    /// It used to open with "Pair with your Mac" and the line "Run this on the Mac
    /// running `ccd`". Neither the product's name nor its purpose appeared
    /// anywhere, and `ccd` is a word the reader has never met: the screen assumed a
    /// Mac already set up, without ever saying so or how.
    @ViewBuilder
    private var identity: some View {
        CCMark(size: 52)
            .padding(.bottom, CC.rhythm.textSurface)

        Text("CodeConnect")
            .ccType(CC.type.display)
            .foregroundStyle(CC.text.primary)
            .fixedSize(horizontal: false, vertical: true)

        Text("See what your coding agents are doing, and answer them.")
            .ccType(CC.type.body)
            .foregroundStyle(CC.text.secondary)
            .fixedSize(horizontal: false, vertical: true)
            // 50-75 characters is the readable measure.
            .frame(maxWidth: 320, alignment: .leading)
            .padding(.top, CC.rhythm.text)
            // Text spacing, not a section break: what follows the tagline is
            // the sample-fleet line, and it belongs to the sentence above it.
            .padding(.bottom, CC.rhythm.text)
    }

    /// The way in for somebody who has no Mac yet.
    ///
    /// **Above the fold, under the tagline, and before the steps**, because it
    /// is the answer to the question the four steps raise — *do I have to do all
    /// that before I can see anything?* — and an answer placed after them has
    /// already been read past. It was last on the screen once, below a footnote
    /// about pairing codes, where it read as a third way to pair rather than as
    /// what you do when you cannot.
    ///
    /// `ghost`, so it never competes with the primary action of anybody who does
    /// have a Mac, and it says "sample data" rather than "demo" or "try it": the
    /// one conclusion a reader must not draw — that these are their agents — is
    /// the one the label never invites. It enters on the tap and never on its
    /// own.
    @ViewBuilder
    private var sampleFleetEntry: some View {
        CCButton("New here? Look around with sample data", variant: .ghost, size: .md) {
            model.startSampleFleet()
        }
        .accessibilityIdentifier("pairing-sample-mode")
        .padding(.bottom, CC.rhythm.sections)
    }

    @ViewBuilder
    private var pairingInvitation: some View {
        identity

        sampleFleetEntry

        CCSectionHeader("To get started")

        // **Four steps, in the order they can work**, and the screen once named
        // only two of them. Each step left out was a reader told to run a
        // command that could not work yet: the installer's pairing code with no
        // network between the two devices, or a pairing that showed nothing
        // because no session had been started through CodeConnect. The
        // installer asks to start the daemon and shows the code itself, so the
        // network comes first.
        //
        // They are *instructions*, not verified state: no tick marks, and nothing
        // here claims to know anything about the reader's Mac. The app cannot see
        // it, and a checkmark it could not earn is the thing this product refuses
        // to draw.
        CCCard(padding: 0) {
            VStack(spacing: 0) {
                // Tailscale is the default, never a requirement: another
                // private network works with `ws_bind` on the Mac, and an app
                // may not demand another app (App Review 4.2.3).
                CCStepRow(
                    index: 1,
                    title: "Make sure your iPhone can reach your computer",
                    message:
                        "Tailscale is one way: sign in to the same account on both. Other private networks work too ([codeconnect.sh/support](https://codeconnect.sh/support)). The connection never goes through a server of ours.")
                CCHairline()
                // A link, never a command: the page shows the install for the
                // reader's computer, so this screen names no command at all.
                CCStepRow(
                    index: 2,
                    title: "Install CodeConnect on your computer",
                    message:
                        "Get CodeConnect for your computer at [codeconnect.sh](https://codeconnect.sh). It installs, starts, and shows a code to scan here. Your computer also needs Claude Code or Codex.")
                CCHairline()
                CCStepRow(
                    index: 3,
                    title: "Pair this iPhone",
                    message: "Scan the code your computer shows.")
                CCHairline()
                CCStepRow(
                    index: 4,
                    title: "Start an agent",
                    message: "Agents you start with CodeConnect appear here.")
            }
        }
        .padding(.top, CC.rhythm.textSurface)
        .padding(.bottom, CC.rhythm.sections)

        // A scan error is not a dead end and the primary stays enabled —
        // retrying is the expected action.
        if let scanError {
            CCBanner("Could not read that code", message: scanError, tone: .warning)
                .padding(.bottom, CC.space.md)
        }
        if let error = machines.pairingError {
            CCBanner("Pairing failed", message: error, tone: .danger)
                .padding(.bottom, CC.space.md)
        }

        if QRScannerView.isSupported {
            CCButton(
                "Scan pairing code", icon: "qr-code", variant: .primary, size: .lg,
                fullWidth: true
            ) {
                scanError = nil
                showScanner = true
            }

            if !showManualEntry {
                CCButton(
                    "Enter the address and code instead", variant: .ghost, size: .md,
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
            Text("No scanner on this device. Pair by hand below.")
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

        CCProse(
            "Each code works once and expires after five minutes.",
            style: CC.type.footnote, color: CC.text.tertiary
        )
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: 320, alignment: .leading)
            .padding(.top, CC.space.xl)

    }

    /// The card is replaced in place rather than growing a spinner underneath
    /// it: what is happening is the only thing happening.
    private var pairingProgress: some View {
        VStack(alignment: .leading, spacing: CC.space.md) {
            // A failed exchange is over: a spinner and a running clock beside
            // its reason would say it is still going.
            if let error = machines.pairingError {
                CCBanner("Pairing failed", message: error, tone: .danger)
            } else {
                CCProgressRing(.lg)

                Text("Pairing…")
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
            }

            // `ghost`, which draws its own 1pt edge at rest now, so a recovery
            // control no longer has to be promoted to a louder variant just to
            // be visible. This is the only way out of a pairing exchange that
            // has stalled, and it stands alone in whitespace: borderless it was
            // a bare centred label, indistinguishable from the body copy above
            // it.
            CCButton(
                machines.pairingError == nil ? "Cancel pairing" : "Back", variant: .ghost, size: .md
            ) {
                machines.cancelPairing()
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
        // 12, matching `machinesSection`. These are the same relationship — a header
        // meeting the card it introduces — written 16 here and 12 there, on one screen.
        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            // **Flush, and in a card, and the two go together.**
            //
            // The header owns the content column, so it lands on 52 from either
            // of this block's two homes — inline under the onboarding screen's
            // 16pt page inset, and inside the repair sheet's. A call site
            // cannot be told what column it is on, so it cannot be asked to
            // inset this itself.
            //
            // The card is the other half of that. The rule is *a header agrees
            // with the rows it names*, and fields sitting on the page column at
            // 16 under a header on 52 are adrift by 36pt just as surely as the
            // reverse. Putting the fields in a card is what puts them on the
            // content column: a section header over a card of `CCField`s, 52pt
            // tall with `micro` labels, expanding in place.
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
                        placeholder: "Your computer's address",
                        hint:
                            "Shown with the code on your computer.",
                        keyboardType: .URL,
                        autocapitalization: .never,
                        disableAutocorrection: true,
                        isMono: true)

                    CCField(
                        label: "Code",
                        text: $token,
                        placeholder: "Pairing code",
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
                                "check-circle-fill", size: 11, weight: .semibold,
                                relativeTo: .caption
                            )
                            .foregroundStyle(CC.color.success)
                            .ccMark(beside: CC.type.footnote)
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
                isLoading: machines.isPairing,
                disabledReason: manualEntryBlockedReason
            ) {
                save()
            }
        }
    }

    private var manualEntryBlockedReason: CCDisabledReason? {
        PairingCredentialInput.blockedReason(address: address, code: token).map { CCDisabledReason($0) }
    }

    // MARK: - Settings

    /// Configuration only. The link's *numbers* live on Link Health and are not
    /// restated here — two screens showing the same facts with different styling
    /// was an IA bug, not just a visual one.
    private var settings: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: CC.rhythm.sections) {
                    machinesSection
                    aboutSection
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
            // The machine is fixed by its id when its row is tapped, so its
            // screen shows — and removes — that machine, whatever the others do.
            .navigationDestination(item: $openMachineID) { id in
                if let machine = machines.model(for: id) {
                    MachineScreen().environment(machine)
                }
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

    /// Every machine this iPhone keeps, oldest first, and the way to add one. A
    /// machine's link and encryption are on its own screen, behind its row.
    private var machinesSection: some View {
        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            // Flush: the header owns the content column itself. Anything added
            // here is added twice.
            CCSectionHeader("Computers")

            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    // Nothing paired — the sample fleet — has no machine to open,
                    // and the row says so.
                    if listedMachines.isEmpty {
                        CCRow("Not paired") {
                            CCStatusDot(
                                color: CC.text.tertiary, isHollow: true,
                                accessibilityText: "Not paired")
                        }
                    }
                    ForEach(listedMachines, id: \.machineID) { machine in
                        machineRow(machine)
                    }

                    CCRow(
                        "Add a computer",
                        subtitle: "Scan the code that computer shows.",
                        showsChevron: false,
                        separator: false,
                        action: {
                            scanError = nil
                            showScanner = true
                        }
                    ) {
                        CCIcon("qr-code", size: CC.size.icon, weight: .medium)
                            .foregroundStyle(CC.text.tertiary)
                    }
                }
            }
        }
    }

    /// The machines Settings lists: every paired one and, in a fixture run, the
    /// fixture standing in for one.
    private var listedMachines: [AppModel] {
        machines.models.filter { $0.machineID != nil || $0.fixturesActive }
    }

    /// One machine: its link in the app's own words for that level, dimmed while
    /// it is offline.
    private func machineRow(_ machine: AppModel) -> some View {
        let health = machine.linkHealth
        let offline = health.level == .offline
        let name = machine.machineName ?? "Not paired"
        return CCRow(
            name,
            subtitle: machine.machineSystem,
            isDimmed: offline,
            accessibilityLabelText: [name, machine.machineSystem, health.word]
                .compactMap { $0 }.joined(separator: ", "),
            action: machine.machineID.map { id in { openMachineID = id } }
        ) {
            CCStatusDot(
                color: health.level.ccTone.color,
                isHollow: offline,
                pulses: health.level == .connecting,
                accessibilityText: health.word)
        } trailing: {
            Text(health.word)
                .ccType(CC.type.footnote)
                .foregroundStyle(CC.text.tertiary)
                .fixedSize()
        }
    }

    /// What protects a connection, in plain words, said only as far as the
    /// daemon has vouched for it. A paired phone is offered the terminal exactly
    /// when its connection is private — TLS, the daemon's tailnet address or
    /// loopback — so that one answer separates "Tailscale encrypts it" from "not
    /// encrypted"; with no paired answer it claims neither.
    static func transport(
        encrypted: Bool, paired: Bool, capabilities: Capabilities?
    ) -> (value: String, note: String) {
        if encrypted {
            return ("Encrypted", "Your computer encrypts this connection itself.")
        }
        if paired, let capabilities {
            if capabilities.servesTerminal {
                return (
                    "Encrypted by Tailscale",
                    "This iPhone reaches your computer inside Tailscale, which encrypts the connection.")
            }
            if capabilities.withholdsTerminal {
                return (
                    "Not encrypted",
                    "Your computer allows unencrypted connections on this network. The live terminal isn't offered over it.")
            }
        }
        return ("Unknown", "Private only on a network that encrypts it, such as Tailscale.")
    }

    /// The privacy policy has to be readable from inside the app, not only from
    /// the store page — and these are the same two URLs the listing carries, so
    /// there is exactly one copy of each claim to keep true.
    private var aboutSection: some View {
        VStack(alignment: .leading, spacing: CC.space.sm) {
            CCSectionHeader("About")

            CCButton("Privacy policy", variant: .ghost, size: .md, fullWidth: true) {
                openURL(URL(string: "https://codeconnect.sh/privacy")!)
            }
            CCButton("Support", variant: .ghost, size: .md, fullWidth: true) {
                openURL(URL(string: "https://codeconnect.sh/support")!)
            }
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
        .ccResizableSheet()
    }

    // MARK: - Actions

    private func handle(scanned payload: String) {
        switch PairingQRPayload.decode(payload) {
        case .success(let parsed):
            scanError = nil
            pairingStartedAt = Date()
            machines.pair(withQR: parsed)
        case .failure(let failure):
            scanError = failure.localizedDescription
        }
    }

    private func save() {
        switch PairingCredentialInput.submission(address: address, code: token) {
        case .refused(let reason):
            validationError = reason
        case .token(let endpoint):
            validationError = nil
            machines.pair(with: endpoint)
            if !isOnboarding { showManualEntry = false }
        case .pairingCode(let payload):
            validationError = nil
            pairingStartedAt = Date()
            machines.pair(withQR: payload)
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
                    glyph: "warning",
                    title: "The scanner stopped",
                    message: nil,
                    detail: error,
                    primaryTitle: "Try again",
                    primary: { self.error = nil })
            } else if !QRScannerView.isSupported {
                unavailable(
                    glyph: "camera-slash",
                    title: "No scanner on this device",
                    message:
                        "This device can't scan codes. Close this and enter the address and code instead.",
                    detail: nil,
                    primaryTitle: "Pair by hand",
                    primary: onManualEntry,
                    secondaryTitle: nil)
            } else if !QRScannerView.isAvailable {
                unavailable(
                    glyph: "camera-slash",
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
                caption: "Point the camera at the QR code in your computer's terminal.")
            .ignoresSafeArea()
        }
        .overlay(alignment: .bottom) { torchButton }
        .accessibilityLabel("Point the camera at the code your computer shows.")
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
                torchOn ? "Torch on" : "Torch", icon: torchOn ? "lightning-fill" : "lightning",
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

// MARK: - One machine

/// One paired machine: its link, whether its connection is encrypted, and the
/// way to remove it, in plain words; what its daemon advertises is in Link
/// health, behind its row. Pushed from its row in Settings with that machine's
/// model, so it speaks for — and removes — that machine alone.
struct MachineScreen: View {
    @Environment(AppModel.self) private var model
    @Environment(Machines.self) private var machines
    @Environment(\.dismiss) private var dismiss

    @State private var showLinkHealth = false
    @State private var confirmingRemove = false

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: CC.rhythm.sections) {
                computerSection
                transportSection
                removeSection
            }
            .padding(.horizontal, CC.space.md)
            .padding(.vertical, CC.space.lg)
        }
        .background(CC.color.bg)
        .ccNavigationChrome()
        .navigationTitle(model.machineName ?? "Computer")
        .navigationBarTitleDisplayMode(.inline)
        .sheet(isPresented: $showLinkHealth) {
            LinkHealthSheet().environment(model)
        }
    }

    private var computerSection: some View {
        VStack(alignment: .leading, spacing: CC.rhythm.textSurface) {
            // Flush: the header owns the content column itself. Anything added
            // here is added twice.
            CCSectionHeader("Computer")

            CCCard(padding: 0) {
                CCRow(
                    model.machineName ?? "Not paired",
                    subtitle: model.machineSystem,
                    meta: model.endpoint?.host,
                    separator: false,
                    action: { showLinkHealth = true }
                ) {
                    CCStatusDot(
                        color: model.linkHealth.level.ccTone.color,
                        isHollow: model.linkHealth.level == .offline,
                        pulses: model.linkHealth.level == .connecting,
                        accessibilityText: "Link health")
                } trailing: {
                    Text(model.linkHealth.word)
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.tertiary)
                        .fixedSize()
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
    /// beneath the row explaining that Tailscale encrypts it.
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
                "Your computer can encrypt this connection itself, but this iPhone isn't using that.")
        }
        if let reason = model.tlsUnavailableReason {
            notes.append(reason)
        }
        return notes
    }

    private var transport: (value: String, note: String) {
        PairingView.transport(
            encrypted: encrypted,
            paired: model.connection.helloAck?.deviceID != nil,
            capabilities: model.connection.capabilities)
    }

    private var transportSection: some View {
        VStack(alignment: .leading, spacing: CC.space.sm) {
            CCSectionHeader("Connection")

            CCCard(padding: 0) {
                VStack(alignment: .leading, spacing: 0) {
                    // The daemon reports whether *this* connection is encrypted,
                    // separately from whether it holds a certificate at all.
                    // Both are shown, because "the server can do TLS" and "you
                    // are using it" are different facts and only one of them is
                    // about you. The value is a few plain words; what qualifies
                    // it is a sentence, in the slot below.
                    CCFactRow(
                        "Encryption",
                        value: transport.value,
                        tone: transportTone,
                        separator: false
                    )

                    ForEach(transportNotes, id: \.self) { note in
                        CCHairline()
                        Text(note)
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.color.warning)
                            .fixedSize(horizontal: false, vertical: true)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.leading, CC.space.md)
                            .padding(.trailing, CC.space.md)
                            .padding(.vertical, CC.space.sm)
                    }
                }
            }

            CCProse(transport.note, style: CC.type.footnote, color: CC.text.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// Removing a machine forgets it and nothing else: every other machine stays
    /// paired, connected and cached.
    private var removeSection: some View {
        VStack(alignment: .leading, spacing: CC.space.sm) {
            CCButton("Remove this computer", variant: .destructive, size: .lg, fullWidth: true) {
                confirmingRemove = true
            }

            CCProse(
                "Removes this computer and its saved activity from this iPhone. Other computers stay paired.",
                style: CC.type.footnote, color: CC.text.tertiary
            )
            .fixedSize(horizontal: false, vertical: true)
        }
        .confirmationDialog(
            "Remove this computer?", isPresented: $confirmingRemove, titleVisibility: .visible
        ) {
            Button("Remove", role: .destructive) {
                machines.unpair(model)
                dismiss()
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(
                "This iPhone forgets \(model.machineName ?? "this computer"). To use it again, show a new code on it and scan it (codeconnect.sh/support shows how)."
            )
        }
    }
}
