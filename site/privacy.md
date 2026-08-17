# CodeConnect Privacy Policy

**Effective date: August 14, 2026**

CodeConnect is an iOS app that connects your iPhone to the CodeConnect daemon (`ccd`) running on **your own computer**. This policy covers both halves — the app on your phone and the software on your Mac — because they are one system, and a policy that described only half of it would leave the interesting questions unanswered.

The policy is short on collection because the design is short on collection: no account, no login, no analytics, and exactly one CodeConnect-operated service — a push relay that exists so notifications can reach a closed app, and that is designed to be incapable of receiving your content. Your data moves between your own devices, with the deliberate exceptions named below: a push notification passes through Apple, and — unless your daemon holds its own Apple push key — through that relay first.

## What we collect

**Nothing about your work.** CodeConnect (the developer) operates exactly one service — the push relay described below. To deliver a notification it receives only the fields a push needs: the APNs token and its environment, an opaque token-bound credential, one of four fixed event kinds, a blocked-run count, a test marker when you ask for a test, and ordinary network metadata. Enrolling for and maintaining that credential is separate traffic — an App Attest proof, and read-only credential-status or lifecycle requests — detailed under *Push notifications* below. The relay cannot receive project names, commands, file paths, diffs, or conversation content, because its interface has no field for them. The app contains no analytics, advertising, crash-reporting, or tracking SDKs of any kind. There is no account to create, so there is no identity to store.

## Where your data lives

Everything CodeConnect works with stays on hardware you own, with one narrow exception: when relay-backed notifications are enabled, the push relay holds the enrollment records described under push notifications below — key hashes, an attestation receipt, counters and timestamps, never content — for the fixed retention periods. Everything else:

- **On your Mac** — the daemon keeps its state under `~/.codeconnect`: the event log of your sessions (a local SQLite database), pairing records for your devices, TLS material, and configuration. None of it is transmitted anywhere except directly to your paired iPhone.
- **On your iPhone** — session data is cached locally solely so the app works offline and resumes quickly. The per-device pairing token is stored in the iOS Keychain; when relay-backed notifications are enabled, the relay credential and the App Attest key ID are stored there too, as a `ThisDeviceOnly` item that never syncs to iCloud and never migrates to another phone.

Deleting it is equally local: revoke a device with `codeconnect revoke`, remove the daemon with `codeconnect daemon uninstall` and delete `~/.codeconnect`, and delete the app from your phone. Your sessions, code, and conversations have no copy anywhere else to ask us about, because we never had one; the only thing we ever hold is the relay's content-free enrollment record. Turning relay-backed notifications off on a Mac stops that Mac using the credential; it does not by itself delete the relay's record. That record becomes terminal — and is then deleted within 30 days — when the credential is replaced or rotated (resetting notification registration in the app does this, by enrolling afresh) or when Apple reports the push token unregistered.

## What the app does with data, on your devices

- **Session data** (agent transcripts, permission prompts, diffs, terminal output) travels directly from the daemon on your computer to your iPhone over a connection you configure — typically your own private network. It is cached on your iPhone solely so the app works offline and resumes quickly. It never passes through servers we operate.
- **Pairing** uses your iPhone camera to read a QR code shown in your terminal. The code is processed on-device; nothing is recorded or uploaded.
- **Dictation** in the compose bar uses Apple's speech recognition, on-device wherever your language supports it. Audio is transcribed while you dictate and is not recorded or kept.
- **Face ID / passcode** is used to confirm high-risk approvals. Biometric data never leaves your device and is never visible to the app; the app only receives Apple's yes/no result.
- **Credentials** — the per-device pairing token, and, when relay-backed notifications are enabled, the relay credential and App Attest key ID — are stored in the iOS Keychain on your iPhone, the relay pair as a `ThisDeviceOnly` item. You can revoke a device's pairing token at any time from your computer, and revocation takes effect immediately, including on open connections.
- **Push notifications** (when enabled) reach your phone one of two ways, and the shape is deliberately minimal in both:
  - **Direct**, when your daemon holds its own Apple push key: your Mac talks straight to Apple's push service. The content is the **project** a run is working in — the last component of its working directory, for example `Aion` — one of four canned sentences ("Waiting on an approval", "Waiting for your input", "Finished a turn", "Waiting for you") or a count of how many runs need you, and one word saying which of those four rang so a tap knows where to go. We never see any of it.
  - **Relay-backed**, the default: your daemon sends CodeConnect's push relay only the APNs token and environment, an opaque token-bound credential, one of the four fixed event kinds, a blocked-run count, a test marker when applicable, and ordinary network metadata. The relay never receives project names, session identifiers, commands, file paths, diffs, or conversation content — its interface has no field for them. It builds a generic notification (titled "CodeConnect") and forwards it to Apple. Enrolling for relay-backed notifications sends Apple's App Attest proof that a genuine copy of the app is asking; the relay retains the verified public key and receipt, an assertion counter, hashes of the token and credential (never their raw values), status, and timestamps. Retention is fixed per field: enrollment challenges live at most ten minutes, redacted diagnostic logs seven days, and revoked or otherwise terminal records — and encrypted backups — 30 days. The active enrollment record persists until it becomes terminal: it is superseded by a fresh enrollment, rotated, or marked unregistered when Apple reports the token gone. Turning relay-backed notifications off on a Mac stops that Mac using the credential but does not by itself delete the record; resetting notification registration in the app re-enrolls, which supersedes the old record and starts its 30-day deletion clock.

  In both paths, nothing an agent wrote, ran, or changed is ever included: no command text, no file paths, no diffs, no conversation content.

## Every network connection, enumerated

A privacy policy that says "we respect your privacy" is weaker than one that lists the connections. Here is the complete list.

The **iPhone app** connects to:

1. **Your own Mac** — directly, over the connection you configure.
2. **Apple** — to receive push notifications, and for speech recognition where your language is not handled on-device. Both are Apple's services under Apple's policy.
3. **CodeConnect's push relay** — only when enrolling for relay-backed notifications, when checking its credential status on returning to the foreground, and for credential-lifecycle changes afterwards (rotation or rebinding): the app sends Apple's App Attest proof, or a read-only bearer for a status check, along with the token binding described above, and nothing else, ever.

The **Mac daemon and tools** connect to:

1. **Your paired iPhone** — directly.
2. **GitHub** — after `codeconnect claude` starts a session, a background request to GitHub's public Releases API checks for a newer release when the 24-hour cache is stale. GitHub receives your IP address and ordinary HTTP request metadata; CodeConnect sends no session, prompt, file, or project data. Set `"update_check": false` in `~/.codeconnect/config.json` to disable it. `codeconnect update`, when you run it, asks the same API and downloads that release's files from GitHub — it is only ever a thing you type.

3. **CodeConnect's push relay** — only when relay-backed notifications are enabled, and only with the fixed fields described above. Sessions, terminals, approvals, and everything else in this system never touch it.

There are no other endpoints. If you choose to reach your Mac through a VPN or tunnel you operate (for example Tailscale), that traffic is governed by that provider's policy — CodeConnect does not require any specific provider.

## How the connection is protected

The link between your phone and your Mac is TLS (`wss://`), your machine's loopback, or your own private tailnet address. A cleartext local-network mode exists but only behind an explicit configuration flag you set yourself — nothing falls back to it silently. Each device holds its own revocable token, high-risk approvals additionally require Face ID or your passcode, and the Mac binaries are Developer ID–signed, with the signature verified by the installer and by `codeconnect update` before anything is installed.

## Your rights

Privacy laws give you rights to access, correct, export, and delete personal data an organization holds about you. CodeConnect holds exactly one thing: the push relay's content-free enrollment record — the hashed token and credential, the App Attest public key and receipt, an assertion counter, status, and timestamps, bound to a push token rather than a name. There is no account, but the record is linked at the device level: the push token is a device identifier, and the enrollment record is joined to it. Turning relay-backed notifications off on a Mac stops that Mac using the credential; the record itself becomes terminal — and is deleted within 30 days — when the credential is replaced or rotated, or when Apple reports the token unregistered. Resetting notification registration in the app enrolls afresh, which supersedes the previous record and starts that 30-day clock; there is no separate on-request deletion path, because the record holds nothing that identifies you beyond the push token it is keyed to. Everything else — your sessions, code, and conversations — exists only on your own devices, under your direct control, and the section above describes how to delete it. If you believe we have this wrong, contact us and we will answer plainly.

## Children

CodeConnect is a developer tool and is not directed at children.

## Changes

If this policy changes, the new version will be posted at this URL with a new effective date. Because the system holds nothing beyond the relay's content-free enrollment record, changes are expected to be rare and editorial.

## Contact

Faisal Mumtaz — [github.com/faisalmumtaz89](https://github.com/faisalmumtaz89). For questions about this policy, open an issue on the [CodeConnect repository](https://github.com/faisalmumtaz89/CodeConnect/issues).
