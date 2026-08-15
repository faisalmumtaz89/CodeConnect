# CodeConnect Support

CodeConnect lets you monitor and control the coding agents running on your own computer — approve permission prompts, watch live sessions, review diffs, and take over in a full terminal, from your iPhone. There is no server in between: the app talks directly to the daemon on your Mac, and your sessions never leave your own devices.

## Requirements

- iPhone running iOS 17 or later.
- A Mac running macOS 13 or later — Apple silicon or Intel; the released binaries are universal.
- `tmux` on the Mac. Every session is hosted in a private tmux server, which is what lets it survive a closed laptop lid or a killed terminal. `brew install tmux` if you don't have it — the installer will remind you.
- A network path from your iPhone to your Mac: same Wi-Fi/LAN, or any private network of your choice (many people use Tailscale; it is optional).

## Installing

```
curl -fsSL https://codeconnect.sh | sh
```

The installer downloads the latest release, verifies its Developer ID signature before installing anything, and installs to `~/.codeconnect/bin`. It never uses sudo, never edits your shell configuration — it prints the `PATH` line and lets you decide — and never enables a service. Then:

1. `codeconnect daemon install` — runs the daemon under launchd, restarting on crash.
2. `codeconnect claude` — starts your coding agent through the wrapper.
3. `codeconnect pair` — shows a QR code; scan it with the CodeConnect app.

Your phone now shows every session the daemon supervises. When an agent asks for permission, the prompt appears on your phone; answering it behaves exactly as if you typed at the Mac.

## Updating

`codeconnect update` downloads the latest release, verifies its signature the same way the installer does, and replaces all three binaries in one step — a machine that loses power mid-update comes back with the complete old set or the complete new one, never a mixture. A running daemon is restarted automatically, and sessions survive the restart. `codeconnect claude` mentions at launch when a newer release exists, and `codeconnect --version` always tells you exactly what is installed, down to the commit it was built from.

## Uninstalling

1. `codeconnect daemon uninstall` — stops the daemon and removes its LaunchAgent.
2. Delete `~/.codeconnect` — it holds the binaries, your session history, pairing tokens, and TLS material, and nothing outside it was ever written.
3. Delete the app from your iPhone.

## Troubleshooting

- **`codeconnect: command not found`** — `~/.codeconnect/bin` is not on your `PATH`. For the current terminal: `export PATH="$HOME/.codeconnect/bin:$PATH"`. Add that line to your shell profile to make it permanent — the installer deliberately doesn't edit it for you.
- **Phone shows "link stale" or no sessions** — confirm the Mac and phone can reach each other (same network or VPN up), and that the daemon is running: `codeconnect daemon status`.
- **Approval prompt not appearing on the phone** — approvals only surface for sessions started through the wrapper (e.g. `codeconnect claude`), not for agents launched directly.
- **`codeconnect claude` says tmux is not found** — install it (`brew install tmux`) and rerun; sessions cannot be hosted without it.
- **Pairing QR won't scan** — the pairing code is single-use; run `codeconnect pair` again for a fresh code.
- **Notifications not arriving** — they are sent by your own daemon, so it must be running, and the app must be allowed notifications in iOS Settings.
- **The live terminal says it was superseded** — opening the terminal on a session that already has one open takes it over; the one that was open is told so. Expected behavior, not a fault.
- **`codeconnect update` says another update is already running** — two updates can't run at once; if you are certain none is, rerun after a moment.
- **Revoke a lost device** — on the Mac: `codeconnect devices` to list, `codeconnect revoke <device>` to cut it off immediately, including open connections.

## Frequently asked

**Does my code ever leave my devices?**
No. Sessions travel directly from your Mac to your phone and never touch a CodeConnect server. The one exception is the push notification: it passes through Apple, and — unless your daemon holds its own Apple push key — through CodeConnect's push relay first, which receives only a fixed handful of content-free fields. Both shapes are described exactly in the [privacy policy](https://codeconnect.sh/privacy).

**Is Tailscale required?**
No. Any network path from phone to Mac works — same Wi-Fi, or any private network you operate. Tailscale is simply a common choice.

**Which coding agents are supported?**
Claude Code is fully supported; observation of other CLIs (Codex, Grok, Kimi) is next.

**What exactly is installed on my Mac?**
Three binaries in `~/.codeconnect/bin` — `codeconnect`, `ccd`, `cc-hook` — plus state under `~/.codeconnect`. Every binary is Developer ID–signed and reports its exact version and build commit via `codeconnect --version`.

## Contact

Questions, bugs, feature requests: open an issue at [github.com/faisalmumtaz89/CodeConnect/issues](https://github.com/faisalmumtaz89/CodeConnect/issues).

Please include your app version (Settings → About in the app) and, for connection issues, the output of `codeconnect daemon status`.

For security problems, please don't open a public issue — use [GitHub's private vulnerability reporting](https://github.com/faisalmumtaz89/CodeConnect/security) on the repository instead.
