# Security

CodeConnect sits between an AI agent and your machine, so it is worth being precise about what it can do and what it protects.

## Reporting a vulnerability

Please **do not** open a public issue for a security problem. Use GitHub's [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability) on this repository.

Include what you did, what happened, and what you expected. A proof of concept helps. This is a personal project, so expect a first response in days rather than hours.

## What the software can do

- **`ccd`** runs as your user under launchd. It reads Claude Code's hook payloads and session transcripts, and it can write keystrokes into a tmux pane you started with `codeconnect`.
- **`codeconnect pair --ssh`** appends a public key to `~/.ssh/authorized_keys`, tagged `# codeconnect:<device>`. **Only** with the explicit `--ssh` flag, only for the key sent during that pairing, and removable with `codeconnect ssh-revoke <device>`.
- CodeConnect never enables Remote Login, Tailscale SSH, or any other system service. It tells you the command and lets you run it.

## Trust boundaries

- **Everything is local by default.** The daemon binds to your Tailscale interface and loopback. There is no cloud relay, no account, and no telemetry.
- **The phone is a client, not an authority.** It holds a per-device token issued at pairing, stored hashed. Tokens are listed with `codeconnect devices` and revoked with `codeconnect revoke <device>` — revocation closes open connections rather than waiting for the next reconnect.
- **Pairing codes are single-use and expire in 5 minutes.**
- **Push notifications are a doorbell, and only a doorbell.** With an APNs provider key configured the daemon sends real notifications; without one the sender is an inert logging stub, and `hello_ack.capabilities.push` tells the phone which it is talking to. What crosses Apple's servers is deliberately austere: the **project** a run is working in (the last component of its working directory), one of four canned sentences, and a count of how many runs are holding a decision (under coalescing that count *replaces* the sentence rather than joining it). It carries no tool name and no risk tier, no session uid, no request id and no routing token — nothing that could be reversed into a command or left pointing at a decision. It does carry one word saying which of four kinds rang, which is what lets a tap choose between the decision list and the fleet without naming anything, and a fixed collapse id — the constant string `codeconnect`, identical for every device and every run — so that a later doorbell replaces an earlier one instead of racing it. (A *test* notification, which the user asks for explicitly, is a separate fixed payload carrying a `codeconnect_test` marker so the phone can banner it.) Apple necessarily sees the device token in the request path, and repeated project titles are of course comparable to each other; the claim here is about the JSON, not anonymity. Nothing an agent wrote ever rides a push: a body is either one of four canned sentences chosen by the hook's *type*, or — when several runs are holding decisions — a count, never the agent's message, so no command text, no diffs and no paths leave the tailnet. The app reconnects and asks the event log what is true. A paired phone can also request one **test** notification (rate-limited per device, refused for the bootstrap token) to prove the chain end to end.
- **Approvals are gated on evidence.** An answer from the phone carries the request ID and a hash of the exact text that was displayed. It is also bound to a prompt *generation* and a fingerprint of the prompt block as it appeared on screen, both re-checked against the **visible pane** immediately before a keystroke is sent. If the prompt has changed, or if identity cannot be established at all, the answer is refused rather than applied to something else.

## What a stolen phone token can do

State this plainly, because it is wider than "it can approve things".

A valid device token can:

- **Read everything.** Every session, every event, every transcript line the daemon has ingested, plus pane snapshots and Git diffs on demand. Terminal output routinely contains secrets an agent printed, so treat this as read access to anything your agents have seen.
- **Answer live approvals**, once it has the displayed text to hash.
- **Submit arbitrary text to a session.** Against an agent running in a permissive mode, that is functionally close to command execution on your Mac.
- **Destroy history, one run at a time, and irreversibly.** From protocol minor 7 a client may send `delete_session`, which removes that run's row and every event, answer and cursor filed under it. There is no undo and no archive. For a run CodeConnect **hosts** (spawned by `codeconnect claude`), it is refused unless the lifecycle is `exited`, and refused for any run the daemon still holds a supervisor or an open approval for. For an **adopted** run — hooks arrived but CodeConnect never launched it, so no probe can ever prove it ended — it is accepted at any lifecycle: the honest contract is "the daemon cannot say; you may remove it", and removal also stops observation of that conversation until a new session start re-adopts it. Neither case reaches Claude Code's own transcripts under `~/.claude/projects`, so the conversations themselves survive and remain resumable. What is lost is CodeConnect's record: the timeline, the approvals and what was decided.

It cannot use the local IPC administration socket, start or kill sessions, delete a running *hosted* session's record, or install an SSH key — that last one requires a fresh pairing code created with `codeconnect pair --ssh` at your keyboard.

**The static bootstrap token is weaker than a device token.** It is a permanent global credential, stored in plaintext because it has to be readable before pairing exists, it cannot be revoked individually, and connections already open with it deliberately survive deletion of the file. Pair a device, then delete it. It exists to bootstrap the first pairing, not to be a long-lived credential.

## What it does not protect against

- **An agent you have already told to skip permissions.** CodeConnect mirrors Claude Code's own prompts; it cannot gate what Claude Code never asks about.
- **Anyone with access to your unlocked Mac.** The daemon's socket and database are protected by file permissions, not by a second factor.
- **Terminal content is sensitive.** Transcripts and pane snapshots can contain secrets an agent printed to stdout. They are stored unencrypted in `~/.codeconnect` under your user account, which is the same protection your shell history has.

## Transport

`wss://` with a certificate from `tailscale cert` when your tailnet has HTTPS certificates enabled, `ws://` bound to the tailnet interface otherwise. Both require a credential — a device token, or the static bootstrap token described above, which authenticates just as well and is exactly why it should be deleted once a device is paired. The QR payload carries the MagicDNS hostname deliberately: a certificate cannot validate against a bare tailnet IP.
