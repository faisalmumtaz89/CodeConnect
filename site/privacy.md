# CodeConnect Privacy Policy

**Effective date: August 14, 2026**

CodeConnect is an iOS app that connects your iPhone to the CodeConnect daemon (`ccd`) running on **your own computer**. This policy covers both halves — the app on your phone and the software on your Mac — because they are one system, and a policy that described only half of it would leave the interesting questions unanswered.

The policy is short on collection because the design is short on collection: there is no CodeConnect server, no account, no login, and no analytics. Your data moves between your own devices, with one deliberate exception named below: a push notification passes through Apple.

## What we collect

**Nothing.** CodeConnect (the developer) operates no servers and receives no data from the app or the daemon. The app contains no analytics, advertising, crash-reporting, or tracking SDKs of any kind. There is no account to create, so there is no identity to store, and nothing in this system can associate you with anything.

## Where your data lives

Everything CodeConnect works with stays on hardware you own:

- **On your Mac** — the daemon keeps its state under `~/.codeconnect`: the event log of your sessions (a local SQLite database), pairing records for your devices, TLS material, and configuration. None of it is transmitted anywhere except directly to your paired iPhone.
- **On your iPhone** — session data is cached locally solely so the app works offline and resumes quickly. The per-device pairing token is stored in the iOS Keychain.

Deleting it is equally local: revoke a device with `codeconnect revoke`, remove the daemon with `codeconnect daemon uninstall` and delete `~/.codeconnect`, and delete the app from your phone. There is no copy anywhere else to ask us about, because we never had one.

## What the app does with data, on your devices

- **Session data** (agent transcripts, permission prompts, diffs, terminal output) travels directly from the daemon on your computer to your iPhone over a connection you configure — typically your own private network. It is cached on your iPhone solely so the app works offline and resumes quickly. It never passes through servers we operate.
- **Pairing** uses your iPhone camera to read a QR code shown in your terminal. The code is processed on-device; nothing is recorded or uploaded.
- **Dictation** in the compose bar uses Apple's speech recognition, on-device wherever your language supports it. Audio is transcribed while you dictate and is not recorded or kept.
- **Face ID / passcode** is used to confirm high-risk approvals. Biometric data never leaves your device and is never visible to the app; the app only receives Apple's yes/no result.
- **Credentials** (per-device pairing tokens) are stored in the iOS Keychain on your iPhone. You can revoke a device's token at any time from your computer, and revocation takes effect immediately, including on open connections.
- **Push notifications** (when enabled) are sent by *your own daemon* directly to Apple's push service, so their content passes through Apple. That content is deliberately minimal and fixed in shape: the **project** a run is working in — the last component of its working directory, for example `Aion` — one of four canned sentences ("Waiting on an approval", "Waiting for your input", "Finished a turn", "Waiting for you") or a count of how many runs need you, and one word saying which of those four rang so a tap knows where to go. Nothing an agent wrote, ran, or changed is ever included: no command text, no file paths, no diffs, no session or request identifiers. We never see any of it — CodeConnect operates no servers.

## Every network connection, enumerated

A privacy policy that says "we respect your privacy" is weaker than one that lists the connections. Here is the complete list.

The **iPhone app** connects to:

1. **Your own Mac** — directly, over the connection you configure.
2. **Apple** — to receive push notifications, and for speech recognition where your language is not handled on-device. Both are Apple's services under Apple's policy.

The **Mac daemon and tools** connect to:

1. **Your paired iPhone** — directly.
2. **GitHub** — after `codeconnect claude` starts a session, a background request to GitHub's public Releases API checks for a newer release when the 24-hour cache is stale. GitHub receives your IP address and ordinary HTTP request metadata; CodeConnect sends no session, prompt, file, or project data. Set `"update_check": false` in `~/.codeconnect/config.json` to disable it. `codeconnect update`, when you run it, asks the same API and downloads that release's files from GitHub — it is only ever a thing you type.

There are no other endpoints. If you choose to reach your Mac through a VPN or tunnel you operate (for example Tailscale), that traffic is governed by that provider's policy — CodeConnect does not require any specific provider.

## How the connection is protected

The link between your phone and your Mac is TLS (`wss://`), your machine's loopback, or your own private tailnet address. A cleartext local-network mode exists but only behind an explicit configuration flag you set yourself — nothing falls back to it silently. Each device holds its own revocable token, high-risk approvals additionally require Face ID or your passcode, and the Mac binaries are Developer ID–signed, with the signature verified by the installer and by `codeconnect update` before anything is installed.

## Your rights

Privacy laws give you rights to access, correct, export, and delete personal data an organization holds about you. CodeConnect holds none, so there is nothing for us to produce, correct, or erase — every copy of your data is on your own devices, under your direct control, and the section above describes how to delete it. If you believe we have this wrong, contact us and we will answer plainly.

## Children

CodeConnect is a developer tool and is not directed at children.

## Changes

If this policy changes, the new version will be posted at this URL with a new effective date. Because the app collects nothing, changes are expected to be rare and editorial.

## Contact

Faisal Mumtaz — [github.com/faisalmumtaz89](https://github.com/faisalmumtaz89). For questions about this policy, open an issue on the [CodeConnect repository](https://github.com/faisalmumtaz89/CodeConnect/issues).
