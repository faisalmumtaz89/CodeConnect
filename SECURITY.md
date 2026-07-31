# Security

CodeConnect sits between an AI agent and your machine, so it is worth being precise about what it can do and what it protects.

## Reporting a vulnerability

Please **do not** open a public issue for a security problem. Use GitHub's [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability) on this repository.

Include what you did, what happened, and what you expected. A proof of concept helps. This is a personal project, so expect a first response in days rather than hours.

## What the software can do

- **`ccd`** runs as your user under launchd. It reads Claude Code's hook payloads and session transcripts, and it can write keystrokes into a tmux pane you started with `cc`.
- **`cc pair --ssh`** appends a public key to `~/.ssh/authorized_keys`, tagged `# codeconnect:<device>`. **Only** with the explicit `--ssh` flag, only for the key sent during that pairing, and removable with `cc ssh-revoke <device>`.
- CodeConnect never enables Remote Login, Tailscale SSH, or any other system service. It tells you the command and lets you run it.

## Trust boundaries

- **Everything is local by default.** The daemon binds to your Tailscale interface and loopback. There is no cloud relay, no account, and no telemetry.
- **The phone is a client, not an authority.** It holds a per-device token issued at pairing, stored hashed. Tokens are listed with `cc devices` and revoked with `cc revoke <device>` — revocation closes open connections rather than waiting for the next reconnect.
- **Pairing codes are single-use and expire in 5 minutes.**
- **Push notifications are not implemented yet.** Nothing is sent to Apple today; the sender is a logging stub. The *design* is that a push is a doorbell carrying no command text, no diffs and no paths — the app reconnects and asks the event log what is true. Until that ships, be aware the internal hint currently carries the agent's own notification text and tool name, which routinely name files and commands. It is inert, but it is one provider key away from being live and wrong, so treat "push is safe to enable" as false until this line changes.
- **Approvals are gated on evidence.** An answer from the phone carries the request ID and a hash of the exact text that was displayed. It is also bound to a prompt *generation* and a fingerprint of the prompt block as it appeared on screen, both re-checked against the **visible pane** immediately before a keystroke is sent. If the prompt has changed, or if identity cannot be established at all, the answer is refused rather than applied to something else.

## What a stolen phone token can do

State this plainly, because it is wider than "it can approve things".

A valid device token can:

- **Read everything.** Every session, every event, every transcript line the daemon has ingested, plus pane snapshots and Git diffs on demand. Terminal output routinely contains secrets an agent printed, so treat this as read access to anything your agents have seen.
- **Answer live approvals**, once it has the displayed text to hash.
- **Submit arbitrary text to a session.** Against an agent running in a permissive mode, that is functionally close to command execution on your Mac.

It cannot use the local IPC administration socket, start or kill sessions, or install an SSH key — that last one requires a fresh pairing code created with `cc pair --ssh` at your keyboard.

**The static bootstrap token is weaker than a device token.** It is a permanent global credential, stored in plaintext because it has to be readable before pairing exists, it cannot be revoked individually, and connections already open with it deliberately survive deletion of the file. Pair a device, then delete it. It exists to bootstrap the first pairing, not to be a long-lived credential.

## What it does not protect against

- **An agent you have already told to skip permissions.** CodeConnect mirrors Claude Code's own prompts; it cannot gate what Claude Code never asks about.
- **Anyone with access to your unlocked Mac.** The daemon's socket and database are protected by file permissions, not by a second factor.
- **Terminal content is sensitive.** Transcripts and pane snapshots can contain secrets an agent printed to stdout. They are stored unencrypted in `~/.codeconnect` under your user account, which is the same protection your shell history has.

## Transport

`wss://` with a certificate from `tailscale cert` when your tailnet has HTTPS certificates enabled, `ws://` bound to the tailnet interface otherwise. Both require a valid device token. The QR payload carries the MagicDNS hostname deliberately: a certificate cannot validate against a bare tailnet IP.
