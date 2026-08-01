# CodeConnect

[![CI](https://github.com/faisalmumtaz89/CodeConnect/actions/workflows/ci.yml/badge.svg)](https://github.com/faisalmumtaz89/CodeConnect/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Monitor and approve your terminal coding agents from your iPhone.

Your agents keep working while you walk away. CodeConnect shows you every session on your Mac, tells you the moment one is blocked, and lets you read the command and approve or deny it from your phone — without ever pretending to know something it doesn't.

```
$ codeconnect claude                 # your normal Claude Code session, now observable
```

That's the whole setup. The session looks and behaves exactly as it did before; it just also appears on your phone.

## What it does

- **A fleet view.** Every agent, what it's asking for, and how long it's been waiting.
- **Approvals with the actual command.** Risk-tiered — a read is one tap, a `git push --force` needs a deliberate hold and Face ID. The command is never truncated or faded, because a hidden suffix is where a dangerous argument hides.
- **Nothing decides without you.** Claude Code auto-answers an unanswered question after ~60 seconds; CodeConnect holds it open instead. If the daemon can't be reached, the prompt falls back to your keyboard rather than being silently answered.
- **Diff review that works on a phone.** Unified, monospace, word-level highlighting, comment-to-agent on any hunk.
- **A real terminal.** SSH into the live tmux session and take over completely.
- **It never lies about state.** Every fact shows its age. A stale link disables actions and says why. "Answered at the keyboard" is a state the phone renders, not a guess.

## Architecture

```
iPhone (SwiftUI)  ──WSS over Tailscale──▶  ccd (Rust daemon, launchd)
      ▲                                          │
      └──────── APNs (a doorbell, not data) ──────┘
                                                  │ unix socket
                                    codeconnect claude ────┘  (tmux-hosted session)
```

- **`ccd`** — event-sourced daemon. SQLite WAL log with a per-session monotonic sequence, so a reconnect replays gap-free or says it couldn't. Never the parent of an agent: `kill -9 ccd` loses nothing.
- **`codeconnect`** — launches an agent inside a private tmux server and wires Claude Code's hooks. Your terminal experience is unchanged.
- **`cc-hook`** — tiny binary the hooks call. Fails safe: if the daemon is unreachable, the decision goes back to the local keyboard.
- **The phone** — a client of the event log, not a source of truth.

Approval **requests** ride structured channels only (hooks, and later ACP / Codex's app-server). Terminal bytes are never parsed to *derive* a fact — three independent projects tried and abandoned it, and Claude Code's transcript contains no approval events at all. The event log is the only source of truth.

Your **answer** travels one of two ways, and the daemon tells the app which one is live:

- **`hook_return`** — the decision goes back through the hook that asked, bound to Claude's own tool-call id. Nothing is typed anywhere. Enable it by setting `hold_ms` in `~/.codeconnect/config.json`; the trade is that the Mac's own prompt is held while your phone is asked.
- **`send_keys`** (default) — the answer is typed into the live prompt, because with `hold_ms: 0` the hook has already returned and the local prompt is what is waiting. Here the visible pane *is* read, to prove the prompt on screen is still the one the card was made for, and a keystroke is refused outright if it cannot be. See [ARCHITECTURE.md](docs/ARCHITECTURE.md) for the four identity checks.

## Getting started

```sh
cd mac && ./install.sh
export PATH="$HOME/.codeconnect/bin:$PATH"

codeconnect daemon install     # run ccd under launchd (restarts on crash)
codeconnect pair               # QR code to pair the phone (--ssh also authorises the app's SSH key)
codeconnect claude             # start a session in the current directory
```

The iPhone app is an Xcode project in `ios/`. Both sides need to be on the same [Tailscale](https://tailscale.com) tailnet.

**Requirements:** Rust stable (1.80+) and **Xcode 26 or newer**. The app deploys to iOS 17, but one navigation-chrome call is guarded with `if #available(iOS 26, *)` — and `#available` is a runtime check, so the symbol still has to exist in the SDK to compile.

For the live terminal, enable Remote Login (System Settings → General → Sharing) or Tailscale SSH. **CodeConnect never enables a system service for you** — it shows you the command and lets you decide.

## Repo layout

| Path | What |
|---|---|
| `mac/` | Rust workspace: `ccd`, `codeconnect`, `cc-hook`, shared `protocol`, and the chaos soak harness |
| `ios/` | SwiftUI app and its dark-only design system |
| `docs/` | How the system works |
| `fixtures/` | Recorded hook payloads and transcripts, replayed by tests |

## Design

How the system fits together is in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

The interface is dark mode only, in a Vercel/Geist register. Two principles do most of the work: **colour is information, never decoration**, and **make the wrong thing unconstructible** — there is no truncation case that can cut a command's arguments, hairlines have no inset parameter to get wrong, and a section header cannot be placed on the wrong column. The design system lives in `ios/CodeConnect/Views/DesignSystem/`, and each component documents the defect it prevents.

## Status

Personal tool, working daily. Claude Code is fully supported; observation of other CLIs (Codex, Grok, Kimi) is next. Not yet packaged for anyone else.

## Tests

```sh
cd mac && cargo test --workspace     # daemon, protocol, shim
cd mac && ./soak/run.sh              # chaos gauntlet against a real session
```

The soak harness kills the daemon mid-session, replays duplicate hooks, storms the same approval concurrently and flaps the connection, then asserts the event log has no holes and no duplicates.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). The house rules there are short and each one was paid for by a bug.

## Security

CodeConnect can write keystrokes into your terminal and, with an explicit flag, authorise an SSH key. [`SECURITY.md`](SECURITY.md) sets out the trust boundaries, what it deliberately does not protect against, and how to report a vulnerability privately.

## Licence

MIT — see [`LICENSE`](LICENSE).
