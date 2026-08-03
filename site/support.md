# CodeConnect Support

CodeConnect lets you monitor and control the coding agents running on your own computer — approve permission prompts, watch live sessions, review diffs, and take over in a full terminal, from your iPhone.

## Requirements

- iPhone running iOS 17 or later.
- A Mac running the CodeConnect daemon (`ccd`).
- A network path from your iPhone to your Mac: same Wi-Fi/LAN, or any private network of your choice (many people use Tailscale; it is optional).

## Getting started

1. Install the CodeConnect daemon on your Mac and run `cc daemon install`.
2. Start your coding agent through the wrapper, e.g. `cc claude`.
3. Run `cc pair` on the Mac and scan the QR code with the CodeConnect app.

Your phone now shows every session the daemon supervises. When an agent asks for permission, the prompt appears on your phone; answering it behaves exactly as if you typed at the Mac.

## Troubleshooting

- **Phone shows "link stale" or no sessions** — confirm the Mac and phone can reach each other (same network or VPN up), and that the daemon is running: `cc daemon status`.
- **Approval prompt not appearing on the phone** — approvals only surface for sessions started through the `cc` wrapper (e.g. `cc claude`), not for agents launched directly.
- **Pairing QR won't scan** — the pairing code is single-use; run `cc pair` again for a fresh code.
- **Revoke a lost device** — on the Mac: `cc devices` to list, `cc revoke <device>` to cut it off immediately, including open connections.

## Contact

Questions, bugs, feature requests: **faisalmumtazhussain@gmail.com**

Please include your app version (Settings → About in the app) and, for connection issues, the output of `cc daemon status`.
