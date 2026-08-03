import SwiftUI
import UIKit

/// Everything the live terminal needs, and the honest account of what each
/// setting does at the Mac.
///
/// The public key lives here with a copy button because there are two ways it
/// gets authorised — `codeconnect pair --ssh`, which does it for you, and pasting it into
/// `~/.ssh/authorized_keys` by hand — and the second one has to be possible or
/// the terminal is hostage to a daemon feature. That is why `Copy public key` is
/// a 52pt primary-weight control rather than a row: it is the action this screen
/// exists for.
struct TerminalSettingsView: View {
    @Environment(AppModel.self) private var model

    @State private var showResetConfirmation = false
    @State private var showForgetHostsConfirmation = false
    @State private var identity: SSHIdentity?
    @State private var pinned: [(host: String, pin: KnownHostKeys.Pin)] = []
    @State private var portText = ""
    @State private var portError: String?

    var body: some View {
        @Bindable var settings = model.settings
        return ScrollView {
            VStack(alignment: .leading, spacing: CC.rhythm.sections) {
                keySection
                whereToConnect(
                    username: $settings.sshUsername, hostname: $settings.sshHostOverride)
                pinnedHostKeys
                forgetKey
            }
            .padding(.horizontal, CC.space.md)
            .padding(.vertical, CC.space.lg)
        }
        .background(CC.color.bg)
        .ccNavigationChrome()
        .navigationTitle("Terminal and SSH")
        .navigationBarTitleDisplayMode(.inline)
        .task {
            identity = SSHIdentityStore.existingIdentity()
            pinned = KnownHostKeys.all()
            // **Seeded only when the port is not the default.** An inferred
            // value belongs in the placeholder — which is where `ACCOUNT NAME`
            // and `HOSTNAME` put theirs, at `textTertiary` — and pre-filling
            // `22` rendered a default the user never typed as typed input:
            // #EDEDED at 16.91:1 beside two siblings at #828282 / 5.15:1, three
            // fields in one card with two meanings for one visual state. It
            // then contradicted itself, because clearing the field produced
            // "A port is required; 22 is the default." over a field that had
            // just shown 22 as if it were an answer.
            portText =
                model.settings.sshPort == AppSettings.defaultSSHPort
                ? "" : "\(model.settings.sshPort)"
        }
        .confirmationDialog(
            "Forget this iPhone's SSH key?", isPresented: $showResetConfirmation,
            titleVisibility: .visible
        ) {
            Button("Forget it", role: .destructive) {
                SSHIdentityStore.reset()
                identity = nil
            }
            Button("Keep it", role: .cancel) {}
        } message: {
            Text(
                "The terminal will stop working until the new key is authorised at the Mac with `codeconnect pair --ssh`."
            )
        }
        .confirmationDialog(
            "Forget all pinned host keys?", isPresented: $showForgetHostsConfirmation,
            titleVisibility: .visible
        ) {
            Button("Forget them", role: .destructive) {
                KnownHostKeys.forgetAll()
                pinned = KnownHostKeys.all()
            }
            Button("Keep them", role: .cancel) {}
        } message: {
            Text(
                "The next connection to each Mac will pin whatever key it is offered, without anything to compare it against."
            )
        }
    }

    // MARK: - Public key

    @ViewBuilder
    private var keySection: some View {
        VStack(alignment: .leading, spacing: CC.space.sm) {
            // **Flush.** The header owns its own column now: x=32 is a non-text
            // gutter for marks and glyphs, and every text run — section labels
            // explicitly included — starts on 52. This call site used to add
            // `CCColumn.gutter` by hand, which put the label on 32 — 20pt
            // adrift of the very rows it names, all of which measured 52.00 —
            // and adding `CCColumn.content` here instead would land it on 88.
            // Nothing to add: the kit steps out, and its trailing inset is what
            // puts `NOT CREATED YET` on 370 rather than on the screen margin.
            CCSectionHeader(
                "This iPhone's public key",
                note: identity == nil ? "not created yet" : nil
            )

            // Plain `CCCard`: the kit's own default now lands its content on the
            // app's 52pt content column, so this screen no longer carries a
            // private constant to do it.
            CCCard {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    if let identity {
                        // `.middle` — the one case `CCMonoTruncation` exists
                        // for. A public key is an unbounded identifier whose
                        // *ends* identify it: the algorithm at the front and the
                        // `codeconnect-<device>-<id>` comment at the back, which
                        // is the only thing that says which line in
                        // authorized_keys belongs to this phone. Tail truncation
                        // cut exactly that and left 300 interchangeable base64
                        // characters.
                        //
                        // `showsCopy: false`: the 52pt `Copy public key` control
                        // below copies the same string, and two copy affordances
                        // for one value is one too many — it also put a third
                        // bordered rounded rect inside the second, and stole
                        // 36pt of width from the key.
                        CCMonoBlock(
                            identity.openSSHPublicKey, lineLimit: 4, showsCopy: false,
                            truncation: .middle, isSmall: true
                        )
                        .accessibilityLabel("This iPhone's SSH public key")

                        // `CCFingerprint`, not a bare `Text`. This file drew two
                        // fingerprints two ways — the pinned-host rows use the
                        // component that groups a fingerprint into readable runs
                        // and spells it out for VoiceOver, and this one did not.
                        // One object, one construction.
                        CCIdentity.fingerprint(
                            identity.fingerprint, comparedTo: nil,
                            name: "This iPhone's key fingerprint")

                        CCButton(
                            "Copy public key", icon: "doc.on.doc", variant: .secondary, size: .lg,
                            fullWidth: true
                        ) {
                            UIPasteboard.general.string = identity.openSSHPublicKey
                            CCHaptic.success.fire()
                        }
                        .accessibilityHint(
                            "Copies the public key so you can paste it into authorized_keys")

                        keyNote
                    } else {
                        Text(
                            "No key yet. One is created the first time you open a terminal, or now if you want to authorise it at the Mac first."
                        )
                        .ccType(CC.type.footnote)
                        .foregroundStyle(CC.text.secondary)
                        .fixedSize(horizontal: false, vertical: true)

                        CCButton(
                            "Create this iPhone's SSH key", icon: "key", variant: .secondary,
                            size: .lg, fullWidth: true
                        ) {
                            identity = SSHIdentityStore.identity()
                        }
                    }
                }
            }

            Text(
                "The private half never leaves this device. `codeconnect pair --ssh` at the Mac is the only thing that files the public half in authorized_keys, and it says so at the terminal before it does."
            )
            .ccType(CC.type.footnote)
            .foregroundStyle(CC.text.tertiary)
            .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// The daemon reports whether it actually installed the key, which separates
    /// "the operator did not consent" from "no key was ever offered" — two
    /// situations that need very different things said about them.
    @ViewBuilder
    private var keyNote: some View {
        if let note = model.daemonProfile.sshKeyNote {
            CCBanner(
                model.daemonProfile.sshKeyInstalled == true ? "Authorised" : "Not installed",
                message: note,
                tone: model.daemonProfile.sshKeyInstalled == true ? .success : .warning,
                icon: model.daemonProfile.sshKeyInstalled == true
                    ? "checkmark.seal" : "exclamationmark.triangle")
        } else if model.daemonProfile.isConnected, !model.daemonProfile.speaksMinor1OrLater {
            CCBanner(
                "Older daemon",
                message:
                    "This daemon predates `codeconnect pair --ssh`. Pasting the key into ~/.ssh/authorized_keys works either way.",
                tone: .info)
        }
    }

    // MARK: - Where to connect

    private func whereToConnect(username: Binding<String>, hostname: Binding<String>) -> some View {
        VStack(alignment: .leading, spacing: CC.space.sm) {
            CCSectionHeader("Where to connect")

            CCCard {
                VStack(alignment: .leading, spacing: CC.space.md) {
                    CCField(
                        label: "Account name",
                        text: username,
                        placeholder: inferredUsername ?? "Mac account name",
                        hint: usernameFooter,
                        autocapitalization: .never,
                        disableAutocorrection: true,
                        isMono: true)

                    CCField(
                        label: "Hostname",
                        text: hostname,
                        placeholder: model.pairing.endpoint?.host ?? "Mac hostname",
                        hint: "Leave blank to use the address this app is paired with.",
                        keyboardType: .URL,
                        autocapitalization: .never,
                        disableAutocorrection: true,
                        isMono: true)

                    CCField(
                        label: "Port",
                        text: $portText,
                        placeholder: "\(AppSettings.defaultSSHPort)",
                        hint: portError == nil ? "22 unless you moved sshd." : nil,
                        error: portError,
                        keyboardType: .numberPad,
                        autocapitalization: .never,
                        disableAutocorrection: true,
                        isMono: true)
                    // A `Stepper` on a five-digit value is 65,000 taps. A field
                    // plus validation is one.
                    .onChange(of: portText) { _, value in commitPort(value) }
                }
            }
        }
    }

    /// Blank commits the default, which is what the placeholder underneath the
    /// cursor has been promising all along. An empty field is not an error
    /// here — it is the field saying "22", and it now means it.
    private func commitPort(_ value: String) {
        let trimmed = value.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty else {
            portError = nil
            model.settings.sshPort = AppSettings.defaultSSHPort
            return
        }
        guard let port = Int(trimmed), (1...65535).contains(port) else {
            portError = "A port is a number between 1 and 65535."
            return
        }
        portError = nil
        model.settings.sshPort = port
    }

    // MARK: - Pinned host keys

    private var pinnedHostKeys: some View {
        VStack(alignment: .leading, spacing: CC.space.sm) {
            CCSectionHeader("Pinned host keys", count: pinned.isEmpty ? nil : pinned.count)

            CCCard(padding: 0) {
                VStack(spacing: 0) {
                    if pinned.isEmpty {
                        // No `CCEmptyState` for a sub-section: a glyph and a
                        // title for "nothing here yet" out-shouts the sections
                        // that do have content.
                        Text("Nothing pinned yet.")
                            .ccType(CC.type.footnote)
                            .foregroundStyle(CC.text.tertiary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.leading, CC.space.md)
                            .padding(.trailing, CC.space.md)
                            .padding(.vertical, CC.space.md)
                    } else {
                        ForEach(Array(pinned.enumerated()), id: \.element.host) { index, entry in
                            if index > 0 { CCHairline() }
                            pinnedRow(entry)
                        }
                    }
                }
            }

            Text(
                "The Mac's SSH key is pinned the first time you connect. If it ever changes, the terminal stops rather than warning, a changed key means either the Mac was rebuilt or something else is answering."
            )
            .ccType(CC.type.footnote)
            .foregroundStyle(CC.text.tertiary)
            .fixedSize(horizontal: false, vertical: true)

            if !pinned.isEmpty {
                // `lg`, like every other full-width section action in the app.
                // This screen shipped two 44pt destructives beside Settings'
                // 52pt `Unpair and erase cache` — three page-level primaries at
                // two heights, where one role gets one construction.
                CCButton(
                    "Forget all pinned host keys", variant: .destructive, size: .lg,
                    fullWidth: true
                ) {
                    showForgetHostsConfirmation = true
                }
            }
        }
    }

    /// Host, fingerprint, age — all three monospace.
    ///
    /// A `CCFactRow` in the `.identifier` label style: the row *is* the host, so
    /// the hostname carries the emphasis and the age sits quietly at the
    /// trailing edge. The fingerprint goes in the `detail` slot rather than the
    /// value column, because a *truncated* fingerprint is worse than no
    /// fingerprint — the whole purpose of the string is to be compared
    /// character by character with what the Mac prints, and `CCFingerprint` is
    /// the component that knows that.
    ///
    /// The age moved from `textDisabled` to the row's `monoSmall` `textTertiary`
    /// on the way: `textDisabled` is reserved for genuinely inactive text, and
    /// an age is a fact you are meant to read, so it takes a colour that clears
    /// AA.
    private func pinnedRow(_ entry: (host: String, pin: KnownHostKeys.Pin)) -> some View {
        PinnedHostRow(host: entry.host, pin: entry.pin)
    }

    // MARK: - Forget this key

    private var forgetKey: some View {
        VStack(alignment: .leading, spacing: CC.space.sm) {
            CCButton(
                "Forget this iPhone's SSH key", variant: .destructive, size: .lg, fullWidth: true
            ) {
                showResetConfirmation = true
            }

            Text(
                "Generates a new key next time. The old one stays in the Mac's authorized_keys until you run `codeconnect ssh-revoke` there, this app cannot remove it for you."
            )
            .ccType(CC.type.footnote)
            .foregroundStyle(CC.text.tertiary)
            .fixedSize(horizontal: false, vertical: true)
        }
    }

    // MARK: - Copy

    private var inferredUsername: String? {
        AppSettings.inferredUsername(fromSessionPaths: model.sessionPaths)
    }

    private var usernameFooter: String {
        if model.settings.sshUsername.trimmingCharacters(in: .whitespaces).isEmpty {
            if let inferred = inferredUsername {
                return
                    "Left blank, CodeConnect uses \"\(inferred)\", read from the working directory the daemon reports for your sessions. Type a name to override it."
            }
            return
                "No account name could be read from your sessions' working directories, so this one has to be filled in."
        }
        return "Leave blank to use the account name read from your sessions' working directories."
    }
}

// MARK: - Pinned host key

/// **One pinned host key, clocked by itself.**
///
/// Host, fingerprint, age — all three monospace.
///
/// A `CCFactRow` in the `.identifier` label style: the row *is* the host, so the
/// hostname carries the emphasis and the age sits quietly at the trailing edge.
/// The fingerprint goes in the `detail` slot rather than the value column,
/// because a *truncated* fingerprint is worse than no fingerprint — the whole
/// purpose of the string is to be compared character by character with what the
/// Mac prints, and `CCFingerprint` is the component that knows that.
///
/// The age moved from `textDisabled` to the row's `monoSmall` `textTertiary` on
/// the way: `textDisabled` is reserved for genuinely inactive text, and an age
/// is a fact you are meant to read, so it takes a colour that clears AA.
///
/// It reads `AppModel.now` no longer. `pinned 3d ago` changes once an hour at
/// most, and reading the clock for it from `TerminalSettingsView.body` rebuilt
/// the whole screen — every card, every field, every fingerprint — once a second
/// for a string that had not moved since the key was pinned.
private struct PinnedHostRow: View {
    let host: String
    let pin: KnownHostKeys.Pin

    /// Read, not merely written: a `@State` the body never looks at does not
    /// invalidate the view. See `AgeTick.renderTime`.
    @State private var lastTick = Date()

    private var clock: AgeClock { AgeClock(since: pin.addedAt, scale: .age) }

    var body: some View {
        CCFactRow(
            host,
            labelStyle: .identifier,
            age: "pinned \(Format.age(since: pin.addedAt, now: AgeTick.renderTime(lastTick: lastTick))) ago",
            // The separator is the caller's, so it stays full card width while
            // the row's text takes the content column. A rule inset on one side
            // and flush on the other is an asymmetry nothing else in the app
            // has, and the text stays on the two left edges every screen holds
            // rather than inventing a third.
            separator: false,
            // Named rather than trailing: `value` and `detail` are both single
            // closures, so a bare trailing closure cannot say which slot it is.
            detail: {
                CCIdentity.fingerprint(
                    pin.fingerprint, comparedTo: nil, name: "Pinned key for \(host)")
            }
        )
        .task(id: clock) { await AgeTick.follow(clock) { lastTick = $0 } }
    }
}
