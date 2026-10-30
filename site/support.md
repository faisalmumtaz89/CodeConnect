# CodeConnect Support

CodeConnect lets you monitor and control the coding agents running on your own computer — approve permission prompts, watch live sessions, review diffs, and take over in a full terminal, from your iPhone. Your sessions travel directly between the app and the daemon on your computer — no server in between — and never leave your own devices; the one CodeConnect-operated service is a content-free push relay, described in the privacy policy.

## Requirements

- iPhone running iOS 17 or later.
- A Mac running macOS 13 or later — Apple silicon or Intel; the released binaries are universal — or a Linux computer, x86_64 or arm64, with OpenSSL 3 (see "On Linux" below), with [Claude Code](https://code.claude.com/docs/en/setup) or Codex already working in a terminal. However you installed them — their own installers, Homebrew, npm, bun, pnpm or mise — CodeConnect runs the `claude` and `codex` your shell runs.
- A way for your iPhone to reach your computer. By default that is [Tailscale](https://tailscale.com/download): install it on both and sign in to the same tailnet. Your local network works too, with a setting on the computer, but without Tailscale's encryption or the live terminal — see "Is Tailscale required?" below.

## Installing

```
curl -fsSL https://codeconnect.sh | sh
```

The installer downloads the latest release, verifies its signature before installing anything — the Developer ID signature on a Mac, CodeConnect's release key on Linux — and installs to `~/.codeconnect/bin`. It installs CodeConnect's own tmux with it, so tmux needs no separate install. It asks once before adding `~/.codeconnect/bin` to your `PATH`, as a marked block in your shell's startup file (two files for bash), which you can delete to undo. Then it asks "Set up CodeConnect now?" — Return means yes — and does, in order, whatever is still missing:

1. Connecting Tailscale. If Tailscale is not installed, it says how to install it and waits. On a Mac you then log in from Tailscale's icon in the menu bar; on Linux it runs `sudo tailscale up --qr`, whose QR code you scan with your phone, signed in to Tailscale. It waits until Tailscale is connected.
2. Starting the daemon now and at login (at boot on Linux), restarting it on crash (`codeconnect daemon install`).
3. Pairing your iPhone: it shows a QR code to scan with the CodeConnect Remote app, then waits for Return (`codeconnect pair` makes a new one).
4. Signing in Claude Code and Codex: each one that is installed but signed out runs its own sign-in. Ctrl-C skips one.

On a Mac it never uses sudo; on Linux it uses sudo only for `tailscale up --qr` and, where systemd refuses it to you, `loginctl enable-linger`. Tailscale is the default, not a requirement: to use your local network instead, answer `n` to that question and see "Is Tailscale required?" below. Stopped partway, the same command picks up where it left off. Then, in a new terminal in your project's folder, `codeconnect claude` starts Claude Code there through CodeConnect. For Codex: `codeconnect codex`.

Your phone now shows every session started this way. When an agent asks for permission, the prompt appears on your phone; answering it there has the same effect as answering at the computer.

## On Linux

Run the same command as your own user, not as root. On a machine where you have only root, as many cloud machines start you, first install and connect Tailscale as root (`curl -fsSL https://tailscale.com/install.sh | sh`, then `tailscale up --qr`; in a container, start `tailscaled` first, as "Tailscale in userspace mode" below shows), then create a user and run it as that user: `useradd -m -s /bin/bash dev && su - dev`. A shell reached with `su` has no systemd user manager, so there the daemon is started by hand, as under "Without systemd".

- **Tailscale** is the default way your phone reaches the computer, as on a Mac. The installer runs `sudo tailscale up --qr`, so a computer with no screen signs in from your phone; where sudo cannot run it, the installer says what to run in a root shell and waits. In a container, where Tailscale usually runs in userspace mode, see [Tailscale in userspace mode](https://github.com/faisalmumtaz89/CodeConnect/blob/main/mac/README.md#tailscale-in-userspace-mode-containers-cloud-sandboxes).
- **With systemd**, the daemon is a systemd user unit, `~/.config/systemd/user/com.codeconnect.ccd.service`, that starts at boot and restarts after a crash; `codeconnect daemon status`, `restart` and `uninstall` work as on a Mac. It keeps running after you log out only while systemd's *linger* is on for your user: the installer turns it on, with `sudo loginctl enable-linger` where systemd refuses it to you. `codeconnect daemon uninstall` leaves linger on and says how to turn it off.
- **Without systemd** — most containers and cloud sandboxes, Alpine and other OpenRC systems, and a shell reached with `su` or `sudo -u` — the installer starts the daemon itself and prints the line that starts it again. Nothing restarts it if it stops or the computer restarts, and neither `codeconnect daemon restart` nor `codeconnect update` restarts it: stop it (`codeconnect daemon status` shows its pid) and run that line again. `codeconnect daemon restart` prints the same line. `codeconnect daemon uninstall` needs systemd too: to uninstall, stop the daemon the same way instead.

## Updating

`codeconnect update` downloads the latest release, verifies its signature the same way the installer does, and replaces all three binaries and CodeConnect's tmux in one step — a machine that loses power mid-update comes back with the complete old set or the complete new one, never a mixture. A running daemon is restarted automatically, and sessions survive the restart. `codeconnect claude` and `codeconnect codex` mention at launch when a newer release exists, and `codeconnect --version` always tells you exactly what is installed, down to the commit it was built from.

The iPhone app updates from the App Store. When the app and a computer cannot understand each other, the app says which one to update: "Update CodeConnect on the computer" (run `codeconnect update` there) or "Update CodeConnect on this iPhone". This page describes CodeConnect 0.15.0, installer 1.7.0 and the iPhone app 1.1.11.

## Uninstalling

1. End your sessions: quit each agent as you normally would, until `codeconnect ls` lists none. To end them all at once: `~/.codeconnect/bin/cc-tmux -L codeconnect kill-server` (it says "no server running" when none are left).
2. `codeconnect daemon uninstall` — stops the daemon and removes its LaunchAgent.
3. Delete `~/.codeconnect` — it holds the binaries, CodeConnect's tmux, your session history, pairing tokens, and TLS material.
4. If you let the installer add CodeConnect to your `PATH`, delete each block between `# >>> codeconnect >>>` and `# <<< codeconnect <<<` in your shell's startup files (`~/.zshrc` for zsh; `~/.bashrc` and your login file, such as `~/.profile`, for bash). If nothing else is left in a file, you can delete it.
5. Delete the app from your iPhone.

## Troubleshooting

- **`codeconnect: command not found`** — `~/.codeconnect/bin` is not on your `PATH`. Open a new terminal if you let the installer add it; otherwise, for the current terminal: `export PATH="$HOME/.codeconnect/bin:$PATH"`. To add it for good, rerun the installer as `curl -fsSL https://codeconnect.sh | sh -s -- --path-yes`, or add that line to your shell's startup file yourself.
- **Phone shows "Offline", "link stale" or no sessions** — confirm the Mac and phone can reach each other (Tailscale, or your own private network, connected on both), and that the daemon is running: `codeconnect daemon status`.
- **Approval prompt not appearing on the phone** — approvals only surface for sessions started through the wrapper (e.g. `codeconnect claude`), not for agents launched directly.
- **`codeconnect claude` says tmux is not found** — CodeConnect's own tmux is missing, which happens only on a Mac first set up by an older release, and the session could not fetch it (usually no network). Run `codeconnect update` to install it, then rerun.
- **`codeconnect pair` says a paired phone could never reach the daemon** — the message says why. Most often the daemon found no Tailscale address and no `ws_bind` is set: Tailscale is not set up or not connected on this Mac. Set up or connect Tailscale, or set `ws_bind` as below, then run `codeconnect daemon restart` (on Linux without systemd, restart it as "On Linux" says) and `codeconnect pair` again.
- **`codeconnect pair` says no phone can pair with this daemon** — the daemon refuses every connection on the address it is bound to. The message says why and what to change; then run `codeconnect daemon restart` (on Linux without systemd, restart it as "On Linux" says).
- **Pairing QR won't scan** — the pairing code is single-use and expires five minutes after it is printed; run `codeconnect pair` again for a fresh one. To type it instead, tap "Enter the address and code instead" under "Scan pairing code" ("Pair by hand" on a device with no camera) and type the address and code it prints.
- **Notifications not arriving** — they start at your own daemon (sent to Apple directly, or through CodeConnect's push relay), so it must be running, and the app must be allowed notifications in iOS Settings.
- **The live terminal says it was superseded** — opening the terminal on a session that already has one open takes it over; the one that was open is told so. Expected behavior, not a fault.
- **`codeconnect update` says another update or installation is already running** — an update, the installer, or a first session fetching CodeConnect's tmux holds the same lock, and two can't run at once; rerun after a moment.
- **Revoke a lost device** — on the Mac: `codeconnect devices` to list, `codeconnect revoke <device>` to cut it off immediately, including open connections.

## Frequently asked

**Does my code ever leave my devices?**
No. Sessions travel directly from your Mac to your phone and never touch a CodeConnect server. The one exception is the push notification: it passes through Apple, and — unless your daemon holds its own Apple push key — through CodeConnect's push relay first, which receives only a fixed handful of content-free fields. Both shapes are described exactly in the [privacy policy](https://codeconnect.sh/privacy).

**Is Tailscale required?**
No, but it is the default: out of the box the daemon listens on your Mac's Tailscale address, so with both devices on the same tailnet there is nothing to configure. On your local network:

1. Find the computer's address on that network — on a Mac, for Wi-Fi, `ipconfig getifaddr en0` (`en1` on a Mac with an Ethernet port); on Linux, `ip -4 addr`.
2. Put it in `~/.codeconnect/config.json` as `ws_bind`, with `ws_allow_plaintext`: CodeConnect then encrypts nothing on that connection, and the live terminal stays off.

   ```
   { "ws_bind": "192.168.1.20", "ws_allow_plaintext": true }
   ```

3. Run `codeconnect daemon restart` — or `codeconnect daemon install` if the daemon is not running yet; on Linux without systemd, start or restart it as "On Linux" says — then `codeconnect pair`.

For encryption and the live terminal, use Tailscale.

Every key is described in the [configuration reference](https://github.com/faisalmumtaz89/CodeConnect/blob/main/mac/README.md#configuration--codeconnectconfigjson).

**Which coding agents are supported?**
Claude Code and Codex. For Claude Code: the fleet, approvals, questions, diffs and the live terminal. For Codex: the fleet, approvals, stopping a turn and saying something.

**What exactly is installed on my Mac?**
Three binaries in `~/.codeconnect/bin` — `codeconnect`, `ccd`, `cc-hook` — and CodeConnect's own tmux (`cc-tmux`) beside them, with their licence notices, plus state under `~/.codeconnect`. Every binary is Developer ID–signed, and `codeconnect --version` reports its exact version and build commit.

## Contact

Questions, bugs, feature requests: open an issue at [github.com/faisalmumtaz89/CodeConnect/issues](https://github.com/faisalmumtaz89/CodeConnect/issues).

Please include the app's version (in the iPhone's Settings app: General → iPhone Storage → CodeConnect), the output of `codeconnect --version` and, for connection issues, the output of `codeconnect daemon status`.

For security problems, please don't open a public issue — use [GitHub's private vulnerability reporting](https://github.com/faisalmumtaz89/CodeConnect/security) on the repository instead.
