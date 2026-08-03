# CodeConnect Privacy Policy

**Effective date: August 2, 2026**

CodeConnect is an iOS app that connects your iPhone to the CodeConnect daemon (`ccd`) running on **your own computer**. This policy is short because the design is short: there is no CodeConnect server, no account, and no analytics. Your data moves between your own devices and nowhere else.

## What we collect

**Nothing.** CodeConnect (the developer) operates no servers and receives no data from the app. The app contains no analytics, advertising, or tracking SDKs of any kind.

## What the app does with data, on your devices

- **Session data** (agent transcripts, permission prompts, diffs, terminal output) travels directly from the daemon on your computer to your iPhone over a connection you configure — typically your own private network. It is cached on your iPhone solely so the app works offline and resumes quickly. It never passes through servers we operate.
- **Pairing** uses your iPhone camera to read a QR code shown in your terminal. The code is processed on-device; nothing is recorded or uploaded.
- **Dictation** in the compose bar uses Apple's speech recognition, on-device wherever your language supports it. Audio is transcribed while you dictate and is not recorded or kept.
- **Face ID / passcode** is used to confirm high-risk approvals. Biometric data never leaves your device and is never visible to the app; the app only receives Apple's yes/no result.
- **Credentials** (per-device pairing tokens, SSH host keys) are stored in the iOS Keychain on your iPhone. You can revoke a device's token at any time from your computer.
- **Push notifications** (when enabled) are sent by *your own daemon* directly to Apple's push service. Notification content originates from your own sessions; we never see it.

## Third parties

The app talks to two kinds of endpoints: **your own computer**, and **Apple** (push delivery, on-device speech where applicable). There are no other endpoints. If you choose to reach your computer through a VPN or tunnel you operate (for example Tailscale), that traffic is governed by that provider's policy — CodeConnect does not require any specific provider.

## Children

CodeConnect is a developer tool and is not directed at children.

## Changes

If this policy changes, the new version will be posted at this URL with a new effective date. Because the app collects nothing, changes are expected to be rare and editorial.

## Contact

Faisal Mumtaz — faisalmumtazhussain@gmail.com
