# CodeConnect Support

CodeConnect lets you monitor and control the coding agents running on your own computer from your iPhone: approve permission prompts, watch live sessions and review diffs. Your sessions travel between the app and the daemon on your computer without passing through any server CodeConnect runs, and CodeConnect never sends them anywhere else, though the app’s cache of them is part of your iPhone’s backups; the one CodeConnect-operated service besides its website is a content-free push relay, described in the privacy policy. Your coding agents themselves send your conversation to the AI providers they use, normally Anthropic or OpenAI, as the privacy policy explains.

## Requirements

- For the iPhone app: iOS 17 or later. With the Mac app, an iPhone is optional.
- A Mac running macOS 26 or later for the Mac app, or macOS 13 or later to install from the terminal (Apple silicon or Intel; the released binaries are universal), or a Linux computer, x86_64 or arm64, with OpenSSL 3 (see “On Linux” below), with [Claude Code](https://code.claude.com/docs/en/setup) or Codex already working in a terminal. However you installed them — their own installers, Homebrew, npm, bun, pnpm or mise — CodeConnect runs the `claude` and `codex` your shell runs.
- A way for your iPhone to reach your computer. [Tailscale](https://tailscale.com/download) is one way, privately and from anywhere, and the one CodeConnect uses by default: install it on your iPhone and your computers, and sign in with the same account. Your local network works too, with a setting on the computer, but without Tailscale's encryption (see “Is Tailscale required?” below).

## Installing

On a Mac with macOS 26 or later, download the Mac app from [codeconnect.sh](https://codeconnect.sh) and open it. It installs CodeConnect and starts its background service. Its window lists the agents on your computers: click one to read its conversation, answer what it asks, reply to it and see what it has changed. To use your iPhone too, choose File › Add iPhone… in the app and scan the code it shows with the CodeConnect app on your iPhone.

### On Linux, and on a Mac older than macOS 26

Run this in a terminal:

```
curl -fsSL https://codeconnect.sh | sh
```

The installer downloads the latest release, verifies its signature before installing anything — the Developer ID signature on a Mac, CodeConnect's release key on Linux — and installs to `~/.codeconnect/bin`. It asks once before adding `~/.codeconnect/bin` to your `PATH`, as a marked block in your shell's startup file (two files for bash), which you can delete to undo. Then it asks “Set up CodeConnect now?” — Return means yes — and does, in order, whatever is still missing:

1. Connecting Tailscale. If Tailscale is not installed, it says how to install it and waits. On a Mac you then log in from Tailscale's icon in the menu bar; on Linux it runs `sudo tailscale up --qr`, whose QR code you scan with your phone, signed in to Tailscale. It waits until Tailscale is connected.
2. Starting CodeConnect's background service now and at login (at boot on Linux), restarting it on crash (`codeconnect daemon install`).
3. Pairing your iPhone: it shows a QR code to scan with the CodeConnect Remote app, then waits for Return (`codeconnect pair` makes a new one). On Linux, when your iPhone may already find this computer by itself (see below), it says so under the code.
4. Signing in Claude Code and Codex: each one that is installed but signed out runs its own sign-in. Ctrl-C skips one.

On a Mac it never uses sudo; on Linux it uses sudo only for `tailscale up --qr` and, where systemd refuses it to you, `loginctl enable-linger`. Tailscale is the default, not a requirement: to use your local network instead, answer `n` to that question and see “Is Tailscale required?” below. Stopped partway, the same command picks up where it left off. Then start an agent from the CodeConnect app: choose this computer, the project and the agent.

When an agent asks for permission, the prompt appears on your phone; answering it there has the same effect as answering at the computer.

### More computers

Once your iPhone uses one computer, every other computer signed in to the same Tailscale account appears on it by itself, with no code, as long as neither is a tagged device and the computer does not run Tailscale in userspace mode, as most containers do. A computer your iPhone reaches another way, such as one on another Tailscale account shared with yours or one on your local network, needs its own code: on a Mac with the app, choose File › Add iPhone… there; otherwise run `codeconnect pair` on it. Then scan the code with the app.

## On Linux

Run the same command as your own user, not as root. On a computer where you have only root, as is common in the cloud, first install and connect Tailscale as root (`curl -fsSL https://tailscale.com/install.sh | sh`, then `tailscale up --qr`; in a container, start `tailscaled` first, as “Tailscale in userspace mode” below shows), then create a user and run it as that user: `useradd -m -s /bin/bash dev && su - dev`. A shell reached with `su` has no systemd user manager, so there the daemon is started by hand, as under “Without systemd”.

- **Tailscale** is the default way your phone reaches the computer, as on a Mac. The installer runs `sudo tailscale up --qr`, so a computer with no screen signs in from your phone; where sudo cannot run it, the installer says what to run in a root shell and waits. In a container, where Tailscale usually runs in userspace mode, see [Tailscale in userspace mode](https://github.com/faisalmumtaz89/CodeConnect/blob/main/mac/README.md#tailscale-in-userspace-mode-containers-cloud-sandboxes).
- **With systemd**, the daemon is a systemd user unit, `~/.config/systemd/user/com.codeconnect.ccd.service`, that starts at boot and restarts after a crash; `codeconnect daemon status`, `restart` and `uninstall` work as on a Mac. It keeps running after you log out only while systemd's *linger* is on for your user: the installer turns it on, with `sudo loginctl enable-linger` where systemd refuses it to you. `codeconnect daemon uninstall` leaves linger on and says how to turn it off.
- **Without systemd** — most containers and cloud sandboxes, Alpine and other OpenRC systems, and a shell reached with `su` or `sudo -u` — the installer starts the daemon itself and prints the line that starts it again. Nothing restarts it if it stops or the computer restarts, and neither `codeconnect daemon restart` nor `codeconnect update` restarts it: stop it (`codeconnect daemon status` shows its pid) and run that line again. `codeconnect daemon restart` prints the same line. `codeconnect daemon uninstall` needs systemd too: to uninstall, stop the daemon the same way instead.

## Updating

With the Mac app, the app updates itself: about once a day it checks for a new version and offers to install it, and CodeConnect › Check for Updates… checks now. The new app updates CodeConnect when it opens. `codeconnect update` leaves it to the app.

Installed from the terminal, `codeconnect update` downloads the latest release, verifies its signature the same way the installer does, and replaces all of CodeConnect’s binaries in one step, so a computer that loses power mid-update comes back with the complete old set or the complete new one, never a mixture. A running daemon is restarted automatically, and sessions survive the restart. `codeconnect --version` always tells you exactly what is installed, down to the commit it was built from.

The iPhone app updates from the App Store. When the app and a computer cannot understand each other, the app names the computer and says which to update: it “has an older CodeConnect. Update CodeConnect on it”, it “has a newer CodeConnect. Update CodeConnect on this iPhone”, or, when the app cannot tell which is older, it “runs a different version of CodeConnect. Update CodeConnect on it and on this iPhone”. To update a computer with the Mac app, choose CodeConnect › Check for Updates… there; otherwise run `codeconnect update` there. This page describes CodeConnect 0.19.33 and its Mac app, installer 1.7.15 and the iPhone app 1.1.36.

## Uninstalling

1. On a Mac with the Mac app, quit the app and move it to the Trash, then delete `~/Library/Application Support/CodeConnect`, where it keeps a copy of what your agents did.
2. `~/.codeconnect/bin/codeconnect daemon uninstall` — stops the daemon and removes its LaunchAgent.
3. Delete `~/.codeconnect`: it holds the binaries, your session history, pairing tokens, TLS material and the sign-ins of any accounts you added.
4. If you let the installer add CodeConnect to your `PATH`, delete each block between `# >>> codeconnect >>>` and `# <<< codeconnect <<<` in your shell's startup files (`~/.zshrc` for zsh; `~/.bashrc` and your login file, such as `~/.profile`, for bash). If nothing else is left in a file, you can delete it. With the Mac app, if you clicked Set up, delete the line under `# Added by CodeConnect: its command line tools.` in `~/.zprofile`, and that comment.
5. Delete the app from your iPhone.

## Troubleshooting

- **`codeconnect: command not found`** — `~/.codeconnect/bin` is not on your `PATH`. Open a new terminal if you let the installer add it; otherwise, for the current terminal: `export PATH="$HOME/.codeconnect/bin:$PATH"`. To add it for good, rerun the installer as `curl -fsSL https://codeconnect.sh | sh -s -- --path-yes`, or add that line to your shell's startup file yourself. With the Mac app, choose CodeConnect › Set Up CodeConnect… and click Set up beside “Use CodeConnect from Terminal”.
- **Phone shows “Offline”, “link stale” or no sessions** — confirm the Mac and phone can reach each other (Tailscale, or your own private network, connected on both), and that the daemon is running: with the Mac app, choose CodeConnect › Set Up CodeConnect…, which starts it again if it isn't running; its Background service row then says Running, or what went wrong. Otherwise run `codeconnect daemon status`.
- **The Mac app says “CodeConnect couldn't set up its background service.”** Choose Try Again, beside Done. If it says the same again, open an issue, as under Contact below.
- **Set Up CodeConnect shows a warning beside Claude Code, Codex and Antigravity**, with the reason under each: none of them can start, and you need one. While another agent can start, an agent that isn't installed is shown in grey, since you need only the agents you use. To use one, install it so that it starts in a new Terminal window, where the app looks for it, then choose Try Again.
- **`codeconnect pair` says a paired phone could never reach the daemon** — the message says why. Most often the daemon found no Tailscale address and no `ws_bind` is set: Tailscale is not set up or not connected on this computer. Set up or connect Tailscale, or set `ws_bind` as below, then run `codeconnect daemon restart` (on Linux without systemd, restart it as “On Linux” says) and `codeconnect pair` again.
- **`codeconnect pair` says no phone can pair with this daemon** — the daemon refuses every connection on the address it is bound to. The message says why and what to change; then run `codeconnect daemon restart` (on Linux without systemd, restart it as “On Linux” says).
- **A computer doesn't appear on my iPhone**: it appears by itself only when it is signed in to the same Tailscale account as a computer your iPhone already uses, neither is a tagged device, and it does not run Tailscale in userspace mode. Otherwise, show its code and scan it with the app: on a Mac with the app, choose File › Add iPhone… there; otherwise run `codeconnect pair` on it.
- **Pairing QR won't scan** — each code works once and expires after five minutes. On a Mac with the app, choose File › Add iPhone… to show a new code; otherwise run `codeconnect pair` again. To type it instead, before any computer is paired, tap “Enter the address and code instead” under “Scan pairing code” (“Pair by hand” on a device with no camera); later, Add a computer offers “Pair by hand” only when the camera can't be used. Then type the address and code shown with it.
- **Notifications not arriving** — they start at your own daemon (sent to Apple directly, or through CodeConnect's push relay), so it must be running, and the app must be allowed notifications in iOS Settings.
- **`codeconnect update` says another update or installation is already running**: an update and the installer hold the same lock, and two can’t run at once; rerun after a moment.
- **Revoke a lost device** — on each computer it uses: on a Mac with the app, choose CodeConnect › Settings…, open Devices and click Block beside it; otherwise `codeconnect devices` lists them and `codeconnect revoke <device>` cuts one off. Either way that computer cuts it off immediately, including open connections, along with every other device it recognized through Tailscale as the same phone or computer. A revoked device stays out, even on your own Tailscale account, until you let it back in, with no new code: click Allow Again in the app, or run `codeconnect unblock <device>`. Pairing it again with a code also lets it in.
- **Use another Claude Code or Codex account**: on a Mac with the app, choose CodeConnect › Settings… and open Accounts. To add one, click Add Account… under Claude Code or Codex and sign in in your browser; then click the account to use. New sessions use it at once. An open session moves to it when you next send it a message, going on with the same conversation and looking just as it did. A session that is still busy sends that message on its current account and moves at a later one. A command a session started in the background outside its agent, such as with `&`, stops when it moves. Sessions open since before CodeConnect 0.19.28 keep their account until they end.

## Frequently asked

**Does my code ever leave my devices?**  
Not to any server of ours. Sessions travel directly from your Mac to your phone and never touch a CodeConnect server, though the app's cache of recent session activity is part of your iPhone's backups, iCloud Backup included when it is on. Push notifications pass through Apple and, unless your daemon holds its own Apple push key, through CodeConnect's push relay first, which receives only a fixed handful of content-free fields. Dictation goes to Apple's speech recognition when your language can't be handled on the iPhone itself. Both are described exactly in the [privacy policy](https://codeconnect.sh/privacy). Your coding agent is different: to do its work, it sends your conversation, including the code in it, to the AI provider it uses, normally Anthropic for Claude Code and OpenAI for Codex, under that provider's own terms.

**Is Tailscale required?**  
No, but it is the default: out of the box the daemon listens on your computer's Tailscale address, so with your iPhone and your computer signed in to the same Tailscale account there is nothing to configure. On your local network:

1. Find the computer's address on that network — on a Mac, for Wi-Fi, `ipconfig getifaddr en0` (`en1` on a Mac with an Ethernet port); on Linux, `ip -4 addr`.
2. Put it in `~/.codeconnect/config.json` as `ws_bind`, with `ws_allow_plaintext`: CodeConnect then encrypts nothing on that connection.

   ```
   { "ws_bind": "192.168.1.20", "ws_allow_plaintext": true }
   ```

3. Run `codeconnect daemon restart` — or `codeconnect daemon install` if the daemon is not running yet; on Linux without systemd, start or restart it as “On Linux” says. Then show a new code: File › Add iPhone… in the Mac app; on Linux, or a Mac without the app, `codeconnect pair`.

For encryption, use Tailscale.

Every key is described in the [configuration reference](https://github.com/faisalmumtaz89/CodeConnect/blob/main/mac/README.md#configuration--codeconnectconfigjson).

**Which coding agents are supported?**  
Claude Code and Codex, in the Mac app as on the iPhone. For Claude Code: the fleet, approvals, questions and diffs. For Codex: the fleet, approvals, stopping a turn and saying something.

**What exactly is installed on my Mac?**  
CodeConnect’s binaries in `~/.codeconnect/bin`, `codeconnect` and `ccd` among them, with their licence notices, plus state under `~/.codeconnect`. With the Mac app, there is also the app itself, which carries these files and copies them into `~/.codeconnect/bin`, and its copy of what your agents did, in `~/Library/Application Support/CodeConnect`. Every binary is Developer ID–signed, and `codeconnect --version` reports its exact version and build commit.

## Contact

Questions, bugs, feature requests: open an issue at [github.com/faisalmumtaz89/CodeConnect/issues](https://github.com/faisalmumtaz89/CodeConnect/issues).

Please include the app's version (in the iPhone's Settings app: General → iPhone Storage → CodeConnect; in the Mac app: CodeConnect › About CodeConnect), the output of `codeconnect --version` and, for connection issues, the output of `codeconnect daemon status`.

For security problems, please don't open a public issue — use [GitHub's private vulnerability reporting](https://github.com/faisalmumtaz89/CodeConnect/security) on the repository instead.
