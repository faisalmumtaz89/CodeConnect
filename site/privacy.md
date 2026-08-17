# CodeConnect Privacy Policy

**Effective date: August 14, 2026**

CodeConnect is an iOS app that connects your iPhone to the CodeConnect daemon (`ccd`) running on **your own computer**. This policy covers both halves — the app on your phone and the software on your Mac — because they are one system, and a policy that described only half of it would leave the interesting questions unanswered.

The policy is short on collection because the design is short on collection: no account, no login, no analytics, and exactly one CodeConnect-operated service — a push relay that exists so notifications can reach a closed app, and that is designed to be incapable of receiving your content. Your data moves between your own devices, with the deliberate exceptions named below: a push notification passes through Apple, and — unless your daemon holds its own Apple push key — through that relay first.

## What we collect

**Nothing about your work.** CodeConnect (the developer) operates exactly one service — the push relay described below. To deliver a notification it receives only the fields a push needs: the APNs token and its environment, an opaque token-bound credential, one of four fixed event kinds, a blocked-run count, a test marker when you ask for a test, and ordinary network metadata. Enrolling for and maintaining that credential is separate traffic — enrollment and re-enrollment send an App Attest attestation carrying the key ID, challenge, token, and environment; rebind and rotation send an App Attest assertion over the binding; a status check is a read-only request carrying only the bearer — detailed under *Push notifications* below. The relay cannot receive project names, commands, file paths, diffs, or conversation content, because its interface has no field for them. The app contains no analytics, advertising, crash-reporting, or tracking SDKs of any kind. There is no account to create, so there is no identity to store.

## Where your data lives

Everything CodeConnect works with stays on hardware you own, with one narrow exception: once a phone has enrolled for relay-backed notifications, the push relay retains the content-free records described under push notifications below — key hashes, an attestation receipt, counters and timestamps, never content — until they become terminal and their retention period expires. Everything else:

- **On your Mac** — the daemon keeps its state under `~/.codeconnect`: the event log of your sessions (a local SQLite database), pairing records for your devices, TLS material, and configuration. None of it is transmitted anywhere except directly to your paired iPhone — the one exception is relay-backed notifications, where the daemon sends the push token, its environment, and the credential to the relay, exactly as described under push notifications below.
- **On your iPhone** — session data is cached locally solely so the app works offline and resumes quickly. The per-device pairing token is stored in the iOS Keychain; when relay-backed notifications are enabled, the relay credential and the App Attest key ID are stored there too, as a `ThisDeviceOnly` item that never syncs to iCloud and never migrates to another phone.

Deleting the copies on your own devices is local: revoke a device with `codeconnect revoke`, remove the daemon with `codeconnect daemon uninstall` and delete `~/.codeconnect`, and delete the app from your phone. Deleting the app removes its container and cache, but iOS keeps the relay credential and App Attest key ID as `ThisDeviceOnly` Keychain items that can outlive an uninstall; no shipped action currently leaves every local credential erased — resetting notification registration replaces them and immediately re-enrolls. Your sessions, code, and conversations have no copy anywhere else to ask us about, because we never had one; the only thing we ever hold is the relay's content-free relay state and short-lived operational records. Turning relay-backed notifications off on a Mac stops that Mac using the credential, but leaves the relay's active record in place; it is never age-pruned while active. That record becomes terminal — when the credential is replaced or rotated (resetting notification registration in the app does this, by enrolling afresh), rebound to a changed token, or marked unregistered when Apple reports the push token gone. The terminal row is removed from the live relay database within 30 days; the last encrypted backup containing it may remain for up to 30 additional days.

## What the app does with data, on your devices

- **Session data** (agent transcripts, permission prompts, diffs, terminal output) travels directly from the daemon on your computer to your iPhone over a connection you configure — typically your own private network. It is cached on your iPhone solely so the app works offline and resumes quickly. It never passes through servers we operate.
- **Pairing** uses your iPhone camera to read a QR code shown in your terminal. The code is processed on-device; nothing is recorded or uploaded.
- **Dictation** in the compose bar uses Apple's speech recognition, on-device wherever your language supports it. Audio is transcribed while you dictate and is not recorded or kept.
- **Face ID / passcode** is used to confirm high-risk approvals. Biometric data never leaves your device and is never visible to the app; the app only receives Apple's yes/no result.
- **Credentials** — the per-device pairing token, and, when relay-backed notifications are enabled, the relay credential and App Attest key ID — are stored in the iOS Keychain on your iPhone, the relay pair as a `ThisDeviceOnly` item. You can revoke a device's pairing token at any time from your computer, and revocation takes effect immediately, including on open connections.
- **Push notifications** (when enabled) reach your phone one of two ways, and the shape is deliberately minimal in both:
  - **Direct**, when your daemon holds its own Apple push key: your Mac talks straight to Apple's push service. For a single run, the alert's title is the **project** a run is working in — the last component of its working directory, for example `Aion`, or `CodeConnect` when there is no unambiguous label — and the body is one of four canned sentences ("Waiting on an approval", "Waiting for your input", "Finished a turn", "Waiting for you"). For multiple blocked runs the project label is dropped: the title is `CodeConnect` and the body is `{n} agents need you`. One word says which of those four kinds rang so a tap knows where to go. A test you explicitly request uses the fixed CodeConnect test title and body and a `codeconnect_test` marker. We never see any of it.
  - **Relay-backed**, the default: your daemon sends CodeConnect's push relay only the APNs token and environment, an opaque token-bound credential, one of the four fixed event kinds, a blocked-run count, a test marker when applicable, and ordinary network metadata. The relay never receives project names, session identifiers, commands, file paths, diffs, or conversation content — its interface has no field for them. It builds a generic notification (titled "CodeConnect") and forwards it to Apple. Enrolling for relay-backed notifications sends Apple's App Attest attestation — carrying the key ID, challenge, token, and environment — that a genuine copy of the app is asking; the relay retains content-free relay state and short-lived operational records: the internal row relationships, a hash of the App Attest key ID, the attestation environment, the verified public key and receipt, an assertion counter and its trust state, the attested bundle version, the validation category, the APNs environment, the credential generation, hashes of the APNs token and bearer credential (never their raw values), the binding status and any terminal reason, timestamps, expiring challenge hashes, and opaque rate-limit buckets. Retention is fixed per field: enrollment challenges live at most ten minutes, redacted diagnostic logs seven days. Revoked or otherwise terminal records are deleted from the live database within 30 days, and the last encrypted backup that still contains one may persist for up to 30 additional days. The active enrollment record persists until it becomes terminal: it is superseded by a fresh enrollment, rotated, rebound to a changed token, or marked unregistered when Apple reports the token gone. Turning relay-backed notifications off on a Mac stops that Mac using the credential but does not by itself delete the record; resetting notification registration in the app re-enrolls, which supersedes the old record and starts its 30-day deletion clock.

  In both paths, nothing an agent wrote, ran, or changed is ever included: no command text, no file paths, no diffs, no conversation content.

## Every network connection, enumerated

A privacy policy that says "we respect your privacy" is weaker than one that lists the connections. Here is the complete list.

The **iPhone app** connects to:

1. **Your own Mac** — directly, over the connection you configure.
2. **Apple** — to register for and receive push notifications, to perform App Attest during enrollment and re-enrollment, and for speech recognition where your language is not handled on-device. These are Apple's services under Apple's policy.
3. **CodeConnect's push relay** — only when enrolling for relay-backed notifications, when checking its credential status (daily-debounced, on returning to the foreground or after an eligible relay-mode handshake), and for credential-lifecycle changes afterwards (rotation or rebinding): enrollment and re-enrollment send Apple's App Attest attestation with the key ID, challenge, token, and environment; rebind and rotation send an App Attest assertion over the binding; a status check is a read-only GET carrying only the bearer, and nothing else, ever.

The **Mac daemon and tools** connect to:

1. **Your paired iPhone** — directly.
2. **GitHub** — after `codeconnect claude` starts a session, a background request to GitHub's public Releases API checks for a newer release when the 24-hour cache is stale. GitHub receives your IP address and ordinary HTTP request metadata; CodeConnect sends no session, prompt, file, or project data. Set `"update_check": false` in `~/.codeconnect/config.json` to disable it. `codeconnect update`, when you run it, asks the same API and downloads that release's files from GitHub — it is only ever a thing you type.

3. **Apple** — only when your daemon holds its own Apple push key (direct mode): the Mac sends notifications straight to Apple's push service, with the minimal content described above and nothing else.

4. **CodeConnect's push relay** — only when relay-backed notifications are enabled, and only with the fixed fields described above. Sessions, terminals, approvals, and everything else in this system never touch it.

The **CodeConnect push relay** itself connects to:

1. **Apple's push service (APNs)** — to deliver the generic `CodeConnect` notification it built from the content-free fields above.
2. **Operator-controlled backup storage** — an S3-compatible object store that receives the relay's encrypted daily database backups (30-day retention). The backup is encrypted before it leaves the relay and contains only the content-free records described above; it holds no session, project, or conversation data, because the relay never receives any.

There are no other endpoints. If you choose to reach your Mac through a VPN or tunnel you operate (for example Tailscale), that traffic is governed by that provider's policy — CodeConnect does not require any specific provider.

## How the connection is protected

The link between your phone and your Mac is TLS (`wss://`), your machine's loopback, or your own private tailnet address. A cleartext local-network mode exists but only behind an explicit configuration flag you set yourself — nothing falls back to it silently. Each device holds its own revocable token, high-risk approvals additionally require Face ID or your passcode, and the Mac binaries are Developer ID–signed, with the signature verified by the installer and by `codeconnect update` before anything is installed.

## Your rights

Privacy laws give you rights to access, correct, export, and delete personal data an organization holds about you. CodeConnect holds content-free relay state and short-lived operational records — a hash of the App Attest key ID, the attestation environment, the verified public key and receipt, an assertion counter and its trust state, the attested bundle version, the validation category, the APNs environment, the credential generation, hashes of the APNs token and bearer credential (never their raw values), the binding status and any terminal reason, timestamps, expiring challenge hashes, and opaque rate-limit buckets — bound to a push token rather than a name. There is no account, but the record is linked at the device level: the push token is a device identifier, and the enrollment record is joined to it. Turning relay-backed notifications off on a Mac stops that Mac using the credential but leaves the active record in place; the record itself becomes terminal — when the credential is replaced or rotated, rebound to a changed token, or marked unregistered when Apple reports the token gone. The terminal row is then removed from the live relay database within 30 days; the last encrypted backup containing it may remain for up to 30 additional days. Resetting notification registration in the app enrolls afresh, which supersedes the previous record and starts that 30-day clock; there is no separate on-request deletion path, because the record holds nothing that identifies you beyond the push token it is keyed to. Everything else — your sessions, code, and conversations — exists only on your own devices, under your direct control, and the section above describes how to delete it, including the caveat that the relay credential and App Attest key ID are `ThisDeviceOnly` Keychain items that can survive deleting the app. If you believe we have this wrong, contact us and we will answer plainly.

## Children

CodeConnect is a developer tool and is not directed at children.

## Changes

If this policy changes, the new version will be posted at this URL with a new effective date. Because the system holds nothing beyond the relay's content-free relay state and short-lived operational records, changes are expected to be rare and editorial.

## Contact

Faisal Mumtaz — [github.com/faisalmumtaz89](https://github.com/faisalmumtaz89). For questions about this policy, open an issue on the [CodeConnect repository](https://github.com/faisalmumtaz89/CodeConnect/issues).
