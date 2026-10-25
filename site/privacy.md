# CodeConnect Privacy Policy

**Effective date: October 25, 2026**

CodeConnect is an iOS app that connects your iPhone to the CodeConnect daemon (`ccd`) running on **your own computer**. This policy covers both halves — the app on your phone and the software on your Mac — because they are one system.

There is no account, no login and no analytics. Your data moves between your own devices. The deliberate exceptions: a push notification passes through Apple, and — unless your daemon holds its own Apple push key — through CodeConnect's push relay first. The relay is the one service CodeConnect operates; it exists so notifications can reach a closed app, and it is designed to be incapable of receiving your content.

## What we collect

**Nothing about your work.** The push relay receives only what is listed under *The push relay* below; its interface has no field for project names, session identifiers, commands, file paths, diffs, or conversation content. The app contains no analytics, advertising, crash-reporting, or tracking SDKs of any kind. There is no account to create, so there is no identity to store.

## Where your data lives

- **On your Mac** — the daemon keeps its state under `~/.codeconnect`: the event log of your sessions (a local SQLite database), pairing records for your devices, TLS material, and configuration. None of it is transmitted anywhere except directly to your paired iPhone. Notifications are the exceptions, exactly as described under *Push notifications*: in direct mode the Mac sends Apple the notification itself, whose title can be a project's folder name; relay-backed, it sends the relay only the fields listed under *The push relay*.
- **On your iPhone** — session data is cached locally solely so the app works offline and resumes quickly. The pairing — your Mac's address and the per-device token — is stored in the iOS Keychain. When relay-backed notifications are enabled, the relay credential and its generation number, the App Attest key ID, the push token and its environment, and an install identifier are stored there too. Both are `ThisDeviceOnly` items that never sync to iCloud and never migrate to another phone.
- **On the push relay** — once a phone has enrolled for relay-backed notifications, the content-free records listed under *The push relay*, for the periods given there.

## What the app does with data, on your devices

- **Session data** (agent transcripts, permission prompts, diffs, terminal output) travels directly from the daemon on your computer to your iPhone over a connection you configure — typically your own private network. It never passes through servers we operate.
- **Pairing** uses your iPhone camera to read a QR code shown in your terminal. The code is processed on-device; nothing is recorded or uploaded.
- **Dictation** in the compose bar uses Apple's speech recognition, on-device wherever your language supports it. Audio is transcribed while you dictate and is not recorded or kept.
- **Face ID / passcode** is used to confirm high-risk approvals. Biometric data never leaves your device and is never visible to the app; the app only receives Apple's yes/no result.
- **Revoking a device** — you can revoke a device's pairing token at any time from your computer, and revocation takes effect immediately, including on open connections.

## Push notifications

When enabled, notifications reach your phone one of two ways. In both, nothing an agent wrote, ran, or changed is ever included: no command text, no file paths, no diffs, no conversation content.

- **Direct**, when your daemon holds its own Apple push key: your Mac talks straight to Apple's push service. For a single run, the alert's title is the **project** a run is working in — the last component of its working directory, for example `Aion`, or `CodeConnect` when there is no unambiguous label — and the body is one of five canned sentences ("Waiting on an approval", "Asking you a question", "Waiting for your input", "Finished a turn", "Waiting for you"). For multiple blocked runs the project label is dropped: the title is `CodeConnect` and the body is `{n} agents need you`. One word says where a tap should go: a question rings as an approval, so that word is one of four. A test you explicitly request uses the fixed CodeConnect test title and body and a `codeconnect_test` marker. We never see any of it.
- **Relay-backed**, the default: your daemon sends the push relay the notification fields below, and the relay builds a generic notification (titled "CodeConnect") and forwards it to Apple.

## The push relay

**What it receives.** From your Mac, to deliver a notification: the APNs token and its environment, an opaque token-bound credential, one of five fixed event kinds, a blocked-run count, a test marker when you ask for a test, and ordinary network metadata. From the app, to manage that credential: asking for an enrollment challenge sends nothing but a format version; enrolling and re-enrolling send Apple's App Attest attestation — carrying the key ID, challenge, token, and environment — that a genuine copy of the app is asking; rebinding and rotating send an App Attest assertion over the binding; a status check is a read-only GET carrying only the bearer credential. Each of these requests also carries ordinary network metadata.

**What it keeps.** Content-free relay state and short-lived operational records: the internal row relationships, a hash of the App Attest key ID, the attestation environment, the verified public key and receipt, an assertion counter and its trust state, the attested bundle version, the validation category, the APNs environment, the credential generation, hashes of the APNs token and bearer credential (never their raw values), the binding status and any terminal reason, timestamps, expiring challenge hashes, and opaque rate-limit buckets. There is no account, but the record is linked at the device level: the push token is a device identifier, and the enrollment record is joined to it rather than to a name.

**How long.** Retention is fixed per field: enrollment challenges live at most ten minutes, rate-limit buckets until the end of the day they are counted, and redacted diagnostic logs seven days. The active enrollment record is never age-pruned while it is active. It becomes terminal when the credential is replaced or rotated (resetting notification registration in the app does this, by enrolling afresh), rebound to a changed token, or marked unregistered when Apple reports the push token gone. A terminal record is deleted from the live relay database within 30 days, and the last encrypted backup containing it may remain for up to 31 additional days. Turning relay-backed notifications off on a Mac stops that Mac using the credential but does not by itself delete the record. The relay's daily backups are encrypted full copies of its database, each removed at the first daily backup after it is 30 days old, so any record above may also remain in a backup for up to 31 days after it leaves the live database.

## Every network connection, enumerated

A privacy policy that says "we respect your privacy" is weaker than one that lists the connections. Here is the complete list.

The **iPhone app** connects to:

1. **Your own Mac** — directly, over the connection you configure.
2. **Apple** — to register for and receive push notifications, to perform App Attest during enrollment and re-enrollment, and for speech recognition where your language is not handled on-device. These are Apple's services under Apple's policy.
3. **CodeConnect's push relay** — only to enroll for relay-backed notifications, to check its credential status (daily-debounced, on returning to the foreground or after an eligible relay-mode handshake), and for credential changes afterwards (rotation or rebinding), each carrying only what *The push relay* lists for that request from the app, and nothing else, ever.

The **Mac daemon and tools** connect to:

1. **Your paired iPhone** — directly.
2. **GitHub** — after `codeconnect claude` starts a session, a background request to GitHub's public Releases API checks for a newer release when the 24-hour cache is stale. GitHub receives your IP address and ordinary HTTP request metadata; CodeConnect sends no session, prompt, file, or project data. Set `"update_check": false` in `~/.codeconnect/config.json` to disable it. `codeconnect update`, when you run it, asks the same API and downloads that release's files from GitHub — it is only ever a thing you type. One more request is made without being typed, and that setting does not stop it, because sessions run on CodeConnect's own tmux even on a Mac that has another: when a release's binaries are installed without CodeConnect's tmux beside them, the next `codeconnect claude` or `codeconnect codex` asks the same API for that release and downloads its tmux from GitHub, until it is installed.
3. **Apple** — only when your daemon holds its own Apple push key (direct mode), with the content described under *Push notifications* and nothing else.
4. **CodeConnect's push relay** — only when relay-backed notifications are enabled, and only with the fields listed under *The push relay*. Sessions, terminals, approvals, and everything else in this system never touch it.

The **CodeConnect push relay** itself connects to:

1. **Apple's push service (APNs)** — to deliver the generic `CodeConnect` notification it built from those fields.
2. **Operator-controlled backup storage** — an S3-compatible object store that receives the relay's encrypted daily database backups, each removed at the first daily backup after it is 30 days old. The backup is encrypted before it leaves the relay and contains only the content-free records listed above.

There are no other endpoints. If you choose to reach your Mac through a VPN or tunnel you operate (for example Tailscale), that traffic is governed by that provider's policy — CodeConnect does not require any specific provider.

## How the connection is protected

The link between your phone and your Mac is TLS (`wss://`), your machine's loopback, or your own private tailnet address. A cleartext local-network mode exists but only behind an explicit configuration flag you set yourself — nothing falls back to it silently. Each device holds its own revocable token, high-risk approvals additionally require Face ID or your passcode, and the Mac binaries are Developer ID–signed, with the signature verified by the installer and by `codeconnect update` before anything is installed.

## Deleting your data, and your rights

Privacy laws give you rights to access, correct, export, and delete personal data an organization holds about you.

- **On your devices** — revoke a device with `codeconnect revoke`, remove the daemon with `codeconnect daemon uninstall` and delete `~/.codeconnect`, and in the app use *Unpair and erase cache*, which deletes the pairing, before deleting the app from your phone. Deleting the app removes its container and cache, but iOS keeps the app's `ThisDeviceOnly` Keychain items — the pairing, and the relay credential with its key ID, push token and install identifier — and they can outlive an uninstall; no shipped action currently leaves every local credential erased — resetting notification registration replaces the relay items and immediately re-enrolls.
- **On the push relay** — CodeConnect holds only the records listed under *The push relay*. Resetting notification registration in the app enrolls afresh, which supersedes the previous record and starts its deletion clock: 30 days in the live database, and up to 31 more in the last backup. There is no separate on-request deletion path, because the record holds nothing that identifies you beyond the push token it is keyed to.

Your sessions, code, and conversations exist only on your own devices, under your direct control; there is no copy anywhere else to ask us about, because we never had one. If you believe we have this wrong, contact us and we will answer plainly.

## Children

CodeConnect is a developer tool and is not directed at children.

## Changes

If this policy changes, the new version will be posted at this URL with a new effective date. Because the system holds nothing beyond the relay's content-free records, changes are expected to be rare and editorial.

## Contact

Faisal Mumtaz — [github.com/faisalmumtaz89](https://github.com/faisalmumtaz89). For questions about this policy, open an issue on the [CodeConnect repository](https://github.com/faisalmumtaz89/CodeConnect/issues).
