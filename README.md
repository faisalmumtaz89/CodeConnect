<div align="center">
  <img src=".github/assets/wordmark.svg" width="520" alt="CODECONNECT">

**Control your agents from anywhere.**

[codeconnect.sh](https://codeconnect.sh) · [App Store](https://apps.apple.com/app/codeconnect-remote/id6797021894)

[![CI](https://github.com/faisalmumtaz89/CodeConnect/actions/workflows/ci.yml/badge.svg)](https://github.com/faisalmumtaz89/CodeConnect/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

</div>

CodeConnect brings the Claude Code and Codex sessions on your Mac or Linux computer to your iPhone. Approve a command, answer a question or review a diff from your phone while the agent keeps working.

## Features

- **Every session in one place.** One iPhone pairs with several computers and lists every session started through CodeConnect, and what it is waiting for.
- **Approvals with the whole command.** Each request is marked by risk, and a high-risk one needs Face ID or your passcode.
- **Claude's questions.** When Claude asks you to choose, the question appears on your phone, and your answer reaches Claude as if you had chosen at the computer.
- **Diffs you can read on a phone.** Word-level highlighting, and a comment on any hunk goes to the agent.
- **Talk to the agent.** Type or dictate a message; dictation uses Apple's speech recognition, and nothing is sent until you send it.
- **A live terminal.** Open the session's terminal on the phone and take over.
- **Claude Code and Codex.** Both run as they do on their own. Everything above works with Claude Code; on a Codex session the phone answers approvals with Codex's own options, stops a turn, and sends a message ([details](docs/codex.md)).
- **Honest about what it knows.** A session shows the age of its last event, and a stale connection disables actions and says why.

## Requirements

- An iPhone with iOS 17 or later.
- A Mac with macOS 13 or later (Apple silicon or Intel), or a Linux computer (x86_64 or arm64) with OpenSSL 3.
- [Claude Code](https://code.claude.com/docs/en/setup) or [Codex](https://github.com/openai/codex) already working in your terminal. CodeConnect runs the `claude` and `codex` your shell runs.
- [Tailscale](https://tailscale.com/download) on both, signed in to the same tailnet. This is the default; your local network also works, without encryption or the live terminal ([how](https://codeconnect.sh/support)).

## Install

1. On your iPhone, install [CodeConnect Remote](https://apps.apple.com/app/codeconnect-remote/id6797021894) from the App Store, and Tailscale on the same tailnet as your computer.
2. On your computer, as your own user:

   ```sh
   curl -fsSL https://codeconnect.sh | sh
   ```

The installer checks the release's signature before installing anything, installs to `~/.codeconnect/bin` with its own tmux, and asks before adding that folder to your `PATH`. Then it asks "Set up CodeConnect now?" and does whatever is still missing, in order:

1. Connects Tailscale, saying how to install it if it is missing.
2. Starts the CodeConnect daemon.
3. Shows a QR code: scan it with the app.
4. Signs in Claude Code and Codex, if they are installed and signed out.

Run again, it picks up where it stopped. It uses sudo only on Linux, for `tailscale up --qr` and, where systemd needs it, `loginctl enable-linger`. To read it first: `curl -fsSL https://codeconnect.sh -o codeconnect-install.sh`, `less codeconnect-install.sh`, then `sh codeconnect-install.sh`. On a Linux machine where you have only root, or in a container, read [On Linux](site/support.md#on-linux) first.

To add another computer, run the same command there, then in the app go to Settings → Machines → Add a machine.

## Use

In a new terminal, from your project's folder, start the agent through CodeConnect:

```sh
codeconnect claude     # Claude Code; every argument is passed on to claude
codeconnect codex      # Codex
```

The agent works as before, and the session appears on your phone. Other commands:

```sh
codeconnect ls                # your sessions
codeconnect attach cc-1       # reopen a session in a new tab
codeconnect pair              # a new pairing code (single use, 5 minutes)
codeconnect devices           # paired phones
codeconnect revoke <device>   # cut a phone off, open connections included
codeconnect daemon status     # is the daemon running?
```

## Update and uninstall

`codeconnect update` installs the latest signed release; your sessions keep running. The app updates from the App Store. Coming from an older release, read [Upgrading](mac/README.md#upgrading).

To uninstall, follow [Uninstalling](site/support.md#uninstalling): end your sessions, remove the daemon, delete `~/.codeconnect`, and remove what the installer added to your shell's startup files.

## How it works

```
iPhone ──── WebSocket over your tailnet ────▶ ccd, the daemon on your computer
                                                  ▲
                    codeconnect claude / codex ───┘   (the session runs in a private tmux)
```

`codeconnect` runs the agent in a private tmux server, so the session outlives the tab, and shows it in your terminal unchanged. The daemon, `ccd`, logs every session's events. Claude Code reports to it through its hooks, and Codex through a broker in front of Codex's own app server; approval requests come only through those channels, never from the screen. Your answer goes back the same way rather than being typed into the prompt, and if the daemon cannot be reached the prompt stays at your keyboard. More in [ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Security and privacy

- Sessions go directly between your computer and your phone, over Tailscale by default; no CodeConnect server sees them.
- A notification passes through Apple and, unless your daemon has its own Apple push key, CodeConnect's push relay, which receives no session content.
- When `codeconnect claude` or `codeconnect codex` starts a session, CodeConnect asks GitHub for a newer release at most once a day; `"update_check": false` in `~/.codeconnect/config.json` turns that off.
- A paired phone can type into your sessions. [SECURITY.md](SECURITY.md) covers the trust boundaries and reporting a vulnerability privately; the [privacy policy](https://codeconnect.sh/privacy) lists every connection.

## Documentation

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): how the pieces fit together
- [docs/codex.md](docs/codex.md): Codex sessions in detail
- [mac/README.md](mac/README.md): the daemon, pairing, TLS and configuration
- [ios/README.md](ios/README.md): the app, its honesty rules and design system
- [docs/push-gateway.md](docs/push-gateway.md): the push relay

## Build from source

The Rust workspace is in `mac/` and the app's Xcode project in `ios/`; [CONTRIBUTING.md](CONTRIBUTING.md) has the build and test commands.

## License

[MIT](LICENSE), except the iPhone app in `ios/`, which is source-available: see [ios/LICENSE](ios/LICENSE).
