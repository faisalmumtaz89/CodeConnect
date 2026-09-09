<div align="center">
  <img src=".github/assets/wordmark.svg" width="520" alt="CODECONNECT">
</div>

<div align="center">

**Control your agents from anywhere.**

[![CI](https://github.com/faisalmumtaz89/CodeConnect/actions/workflows/ci.yml/badge.svg)](https://github.com/faisalmumtaz89/CodeConnect/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

</div>

Your agents keep working while you walk away. CodeConnect puts every session on your Mac in your hand: steer an agent mid-run, answer the prompt it's blocked on, review its diff, or take over its terminal outright — without ever pretending to know something it doesn't.

```
$ codeconnect claude                 # your normal Claude Code session, now observable
$ codeconnect codex                  # the same, for OpenAI's Codex CLI
```

That's the whole setup. The agent itself behaves exactly as it did before, and the session also appears on your phone.

## What it does

- **A fleet view.** Every agent, what it's asking for, and how long it's been waiting.
- **Approvals with the actual command.** Risk-tiered — a read is one tap, a `git push --force` needs a deliberate hold and Face ID. The command is never truncated or faded, because a hidden suffix is where a dangerous argument hides.
- **Two agents, one fleet.** Claude Code and OpenAI's Codex CLI both run under `codeconnect`, and a Codex session is drivable from the phone: answer an approval with Codex's own options, stop a running turn, or say something — starting a turn when the session is idle, joining the one already running when it is busy. See [`docs/codex.md`](docs/codex.md).
- **Nothing decides without you.** Claude Code auto-answers an unanswered question after ~60 seconds; CodeConnect holds it open instead. If the daemon can't be reached, the prompt falls back to your keyboard rather than being silently answered.
- **Diff review that works on a phone.** Unified, monospace, word-level highlighting, comment-to-agent on any hunk.
- **Talk to it — out loud if you like.** The compose bar stages text into the agent's prompt; template chips insert, never send. Dictation is Apple's own speech recognition — on-device wherever your language supports it; where it doesn't, Apple's speech service does the transcription. A transcript is always staged for review, never auto-sent.
- **A real terminal.** Attach to the live tmux session over the connection the app is already paired on, and take over completely.
- **It never lies about state.** Every fact shows its age. A stale link disables actions and says why. "Answered at the keyboard" is a state the phone renders, not a guess.

## Why you might not want this (yet)

- **The terminal is tmux's while a session runs.** The session is hosted in a private tmux server, which is what lets it outlive the tab and what the phone types into. An attached tmux client uses the alternate screen, so your existing scrollback is set aside and restored on exit, and tmux prints `[exited]` when the session ends. That is tmux, not CodeConnect, and no tmux setting removes it. If your terminal's own scrollback matters more to you than session survival, run `claude` directly and pair a different session.
  - *Scrolling*: the private server runs `mouse on`, so the wheel scrolls the session's own history — up to `tmux_history_limit` lines (50,000 by default) of conversation. The trade tmux imposes: dragging now selects through tmux's copy mode; hold **Shift** to select through your terminal natively instead.
  - *Padding*: some terminals draw full-screen apps edge to edge by design. Warp pads them with **0px by default** — Settings → Appearance → Full-screen Apps lets you set custom padding or match the blocks UI, which restores the exact framing plain `claude` gets. That is the terminal's presentation of tmux, and the terminal's setting is the right place to change it.
- **The app is not out yet.** It launches on the App Store soon. The iOS source is public to be read and audited — see the license. The Mac side updates itself from signed universal release binaries, so once installed it needs no toolchain; the first install today is `./install.sh`, which builds from source and needs Rust.
- **It assumes one Mac, one tailnet, one person.** That is the shape it is used in daily; anything else is unexplored.

## How it works

```
iPhone (SwiftUI)  ──wss:// tailnet (ws:// fallback)──▶  ccd (Rust daemon, launchd)
      ▲                                                       │
      └── push (a doorbell: direct APNs, or the content-free relay) ──┘
                                                               │ unix socket
                                                 codeconnect claude ────┘  (tmux-hosted session)
```

- **`ccd`** — event-sourced daemon. SQLite WAL log with a per-session monotonic sequence, so a reconnect replays gap-free or says it couldn't. Never the parent of an agent: `kill -9 ccd` loses nothing.
- **`codeconnect`** — launches an agent inside a private tmux server. For Claude Code it wires the hooks; for Codex, which has none, it puts a broker in front of Codex's own JSON-RPC app server. The agent behaves unchanged; the terminal is tmux's while the session is attached.
- **`cc-hook`** — tiny binary the hooks call. Fails safe: if the daemon is unreachable, the decision goes back to the local keyboard.
- **The phone** — a client of the event log, not a source of truth.

Approval **requests** ride structured channels only — Claude Code's hooks, and Codex's app server through the broker; ACP later. Terminal bytes are never parsed to *derive* a fact — three independent projects tried and abandoned it, and Claude Code's transcript contains no approval events at all. The event log is the only source of truth.

Your **answer** travels one of two ways, and the daemon tells the app which one is live:

- **`hook_return`** — the decision goes back through the hook that asked, bound to Claude's own tool-call id. Nothing is typed anywhere. Enable it by setting `hold_ms` in `~/.codeconnect/config.json`; the trade is that the Mac's own prompt is held while your phone is asked.
- **`send_keys`** (default) — the answer is typed into the live prompt, because with `hold_ms: 0` the hook has already returned and the local prompt is what is waiting. Here the visible pane *is* read, to prove the prompt on screen is still the one the card was made for, and a keystroke is refused outright if it cannot be. See [ARCHITECTURE.md](docs/ARCHITECTURE.md) for the four identity checks.

## Getting started

```sh
curl -fsSL https://codeconnect.sh | sh
export PATH="$HOME/.codeconnect/bin:$PATH"

codeconnect daemon install     # run ccd under launchd (restarts on crash)
codeconnect pair               # QR code to pair the phone
codeconnect claude             # start a session in the current directory
codeconnect codex              # the same, for Codex
```

The installer downloads the latest release, verifies its Developer ID
signature before installing anything, and never uses sudo, edits your shell
configuration, or enables a service. Prefer to read it first? It is plain
text at [codeconnect.sh](https://codeconnect.sh). Building from source
instead is `cd mac && ./install.sh` from a clone — the same script
contributors use.

The iPhone app is an Xcode project in `ios/`. Both sides need to be on the same [Tailscale](https://tailscale.com) tailnet.

Without a Mac there is nothing to pair with, so the app also opens a **sample fleet**
from its pairing screen — recorded frames replayed through the real decoders, labelled
wherever they are on screen, and connected to nothing. It takes an explicit tap, it
creates no pairing and opens no socket, and nothing in it reports a link: a release
build still has no way to be *paired* except by a person with a Mac.

**Updating the Mac:** `codeconnect update` — it downloads the latest published release, checks that it is signed by CodeConnect, and replaces all three binaries in one step, restarting a running daemon itself; sessions survive the restart. It needs no toolchain: nothing is compiled, and the machine does not need a checkout. Contributors build their own with `cd mac && ./install.sh`, which is the same script it has always been. The app tolerates an older daemon indefinitely: newer features hide or say what to update, per surface. Every binary knows the exact commit it was built from — `codeconnect --version` shows it — and `codeconnect claude` says so at launch when a newer release exists.

**Update-check requests that leave the tailnet:** by default, after `codeconnect claude` starts a session, CodeConnect makes a background request to GitHub's public Releases API when its 24-hour update cache is stale. `codeconnect update`, when you run it, asks the same API and then downloads that release's files from GitHub. GitHub receives your IP address and ordinary HTTP request metadata; CodeConnect sends no session, prompt, file, or project data. Set `"update_check": false` in `~/.codeconnect/config.json` to disable the background check; `codeconnect update` is only ever a thing you type. For the separate Apple and push-relay connections used by notifications and App Attest, see the privacy policy's complete network inventory.

**Requirements to build from source** — not needed to install a release: Rust stable (1.85+) and **Xcode 26 or newer**. The app deploys to iOS 17, but one navigation-chrome call is guarded with `if #available(iOS 26, *)`, and `#available` is a runtime check, so the symbol still has to exist in the SDK to compile.

The live terminal needs nothing switched on at the Mac: it rides the paired connection the app already holds. It does need that connection to be private — `wss://`, loopback, or this Mac's own tailnet address — so it is the one thing not offered over the cleartext LAN that `ws_allow_plaintext` opens. Opening it on a session that already has one takes it over: the terminal that was open is told it was superseded, and the new one streams.

## Upgrading

Two things are worth knowing before the daemon comes back up.

- **A `ws_bind` onto a LAN, with no certificate, refuses every connection.** Plaintext is served only where the bytes are private already — loopback, or this Mac's own tailnet address, where WireGuard encrypts the path before it touches a network. An address you named yourself on a LAN is neither, so without a certificate that listener turns away every phone before the WebSocket handshake and says so in `ccd.err.log` as it starts. Both ways through are in that line: obtain a certificate for a name that resolves to the address you bound, put its `<name>.crt` and `<name>.key` in `~/.codeconnect/tls/`, and set `tls_hostname` to that name — not with `tailscale cert`, which issues only for this node's MagicDNS name, and that name points at the tailnet interface rather than at your LAN address, which is why the daemon will not carry it beside a LAN listener — or set `"ws_allow_plaintext": true` in `~/.codeconnect/config.json` if you accept what crosses that network in the clear — which buys back everything except the Terminal tab, refused on a connection whose bytes anything on that LAN could read. Leave `ws_bind` unset and none of this applies — the daemon picks its own tailnet address.
- **CodeConnect does not use SSH.** The terminal rides the paired connection, and at startup — and again on every `codeconnect revoke <device>`, whether or not that device was already revoked — the daemon removes what an earlier release wrote into `~/.ssh/authorized_keys`: a `# codeconnect:<id>` marker comment with a bare `ssh-ed25519` line carrying the same id directly beneath it. Four things leave such a grant in place, each a warning in `ccd.err.log` rather than a daemon that refuses to start or a revocation that fails — no absolute `$HOME` to resolve the path against, a file it cannot read or rewrite, a file some other program rewrote while the sweep was working (it retries, then declines rather than overwrite what that program wrote), and tagged lines that are not that whole pair. A phone running an older version of the app shows the key as "not installed" and tells you to run `codeconnect pair --ssh` or `codeconnect ssh-revoke`; both commands explain that they are retired rather than failing as unrecognised, and print the same search. Update the app, then settle the file yourself: `grep -n codeconnect: ~/.ssh/authorized_keys`. Nothing printed means nothing there carries the tag, and `grep` reporting no such file means the same — neither says that nothing is left, because a key line whose comment field does not carry the tag prints nothing and stays, and nothing in the file records who wrote a line. An `ssh-ed25519` line in that output with no `# codeconnect:` marker directly above it is a live grant no sweep will touch, however many times you revoke — that pairing rule is deliberate, since removing a key the file cannot identify is not the daemon's call — so delete it by hand. A whole pair is one a sweep has not reached: `codeconnect revoke <device>` again is the retry, and deleting both lines together is the certainty.

[`mac/README.md`](mac/README.md#upgrading) has the detail: which addresses `ws_allow_plaintext` covers, which it refuses, and every configuration key.

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — how the pieces fit together, and why each is shaped the way it is
- [`docs/codex.md`](docs/codex.md) — Codex sessions: launching one, what the phone can and cannot do with it, the security boundary, and what happens when Codex updates
- [`mac/README.md`](mac/README.md) — the daemon in depth: session and prompt identity, durability, pairing, TLS, configuration, and the chaos soak
- [`ios/README.md`](ios/README.md) — the app: honesty rules, test seams, the render harness, and the design system
- [`SECURITY.md`](SECURITY.md) — trust boundaries, including exactly what a stolen phone token can do
- [`docs/push-gateway.md`](docs/push-gateway.md) — the push relay: architecture, the privacy contract, and the operational runbook (§7)
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — house rules, each one paid for by a bug

## Repo layout

| Path | What |
|---|---|
| `mac/` | Rust workspace: `ccd`, `codeconnect`, `cc-hook`, shared `protocol`, and the chaos soak harness |
| `ios/` | SwiftUI app and its dark-only design system |
| `docs/` | How the system works |
| `fixtures/` | Recorded hook payloads and transcripts, replayed by tests |

## Design

The interface is dark mode only, in a Vercel/Geist register. Two principles do most of the work: **colour is information, never decoration**, and **make the wrong thing unconstructible** — there is no truncation case that can cut a command's arguments, hairlines have no inset parameter to get wrong, and a section header cannot be placed on the wrong column. The design system lives in `ios/CodeConnect/Views/DesignSystem/`, and each component documents the defect it prevents.

## Status

Personal tool, working daily. Claude Code and OpenAI's Codex CLI are both fully supported — a Codex session is hosted through a broker in front of Codex's app server, and the phone can answer its approvals, stop a turn and say something. Other CLIs (Grok, Kimi) are not supported. Mac updates arrive as signed release binaries. **The iPhone app launches on the App Store soon.**

## License

[MIT](LICENSE), except the iOS app (`ios/`), which is source-available — see [`ios/LICENSE`](ios/LICENSE).

## Tests

```sh
cd mac && cargo test --workspace     # daemon, protocol, shim
cd mac && ./soak/run.sh              # chaos gauntlet against a real session
```

The soak harness kills the daemon mid-session, replays duplicate hooks, storms the same approval concurrently and flaps the connection, then asserts the event log has no holes and no duplicates.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). The house rules there are short and each one was paid for by a bug.

## Security

CodeConnect can write keystrokes into your terminal and open a live shell on a paired device. [`SECURITY.md`](SECURITY.md) sets out the trust boundaries, what it deliberately does not protect against, and how to report a vulnerability privately.

## Licence

MIT — see [`LICENSE`](LICENSE).
