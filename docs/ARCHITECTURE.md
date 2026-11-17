# Architecture

How the daemon and the phone fit together, and why each piece is shaped the way it is.

## The central constraint

An agent's terminal output tells you what it *has done*. It does not reliably tell you what it is *waiting for*. Claude Code's session transcript contains no approval events at all — a pending permission prompt exists only in the running program's memory and is never written down.

That single fact determines the design. Anything that detects approvals by reading terminal text is inferring a signal from its absence, and it breaks the first time the rendering changes. So the system is split in two:

- **The control plane** — where an agent asks a question and *blocks*. For Claude Code, the agent host it runs under, which holds each request as the Agent SDK's `canUseTool` request; for Codex, its own JSON-RPC app server. It is the only place an approval is created or answered.
- **The observation plane** — where an agent narrates what already happened. The messages its host or app server relays. Useful for showing a timeline and a diff. Never used to decide anything: the daemon acts on the control plane alone.

## The pieces

```
 iPhone (SwiftUI)                          Mac
┌──────────────────────┐        ┌──────────────────────────────────┐
│ Fleet                │  WSS   │  ccd  (Rust, launchd/systemd)    │
│ Session timeline     │◄──────►│  ┌────────────────────────────┐  │
│ Approval card        │ tailnet│  │ ingest → assign seq → dedup│  │
│ Diffs                │        │  │ SQLite WAL   PK(uid, seq)  │◄─┼── source of truth
└──────────────────────┘        │  └──────────▲─────────────────┘  │
                                │             │ unix socket        │
                                │      ┌──────┴───────┐            │
                                │      │cc-agent-host │            │
                                │      └──────▲───────┘            │
                                │             │ Agent SDK          │
                                │   ┌─────────┴──────────┐         │
                                │   │       claude       │         │
                                │   └────────────────────┘         │
                                └──────────────────────────────────┘
```

**`ccd`** is the daemon. It ingests what each run's agent host or app server reports, assigns each event a monotonic sequence number per session, and writes them to SQLite before anything is published. The log is the source of truth: reconnection, backfill, deduplication and crash recovery are all one mechanism — ask for everything after sequence *N*.

**`codeconnect`** is the command line: it installs and runs the daemon, pairs phones, serves the mail tools an agent calls (`mcp`) and installs updates. Agents are started from the apps.

**`cc-agent-host`** runs Claude Code for each run started from an app, through the Agent SDK: it relays what Claude says to the daemon, and the daemon's answers back.

**The phone** is a client of the event log, never a source of truth. It renders what the log says, and shows the age of every fact.

Each run's agent host or app server starts in a session of its own, so killing the daemon does not touch a running session. Restarting it re-attaches and backfills.

## Approving something, safely

Every answer to a Claude card goes back through the agent host Claude runs under, and nothing is ever typed into a permission prompt. Every daemon the apps can connect to advertises `hook_only_approvals`; one of an earlier protocol is refused before any card reaches the phone.

Every answer carries the request ID and the hash of the card text the phone checked before drawing it; a mismatch is refused. The intent to answer is recorded durably before the host is handed anything, and resolved after: an answer whose outcome has not come is never sent again.

The agent host holds each request as the SDK's `canUseTool` request, and there is no dialog at the Mac. The card is answered through the host, joined to its call by the SDK's own `tool_use_id`, with the same request ID and hash check. It offers Allow, Deny and Deny with a reason, and "Allow for this session" when Claude's request carries Claude's own allow rule: that rule goes back to Claude with the session as its destination, never to a settings file, and no mode or folder Claude suggested with it is sent. The host says when the answer is written into Claude's input, so the answer is recorded as taken, not as only sent. The card stays answerable for as long as Claude waits, across a daemon restart, and closes with nobody having answered when Claude stops waiting.

## Claude's own questions

Claude Code asks every `AskUserQuestion` as a permission request, but it is a question to you, not a permission. The daemon recognises it by tool name and answers it only through the agent host, with the question's own answers.

The agent host holds each question as the SDK's `canUseTool` request, and there is no terminal dialog. Nothing about how a terminal would draw the question's text keeps it from the phone: there is no keyboard answer to match. The answer goes back through the host as the call's input, and a decline is a deny with "Declined from iPhone" or "Declined from Mac", with an interrupt for the main conversation's question and none for a background agent's. The host says when the answer is written into Claude's input, so an answer or a decline is recorded as taken, not unconfirmed. The card stays answerable for as long as Claude waits, across a daemon restart, and closes with nobody having answered when Claude stops waiting.

- **The answer is built by the daemon.** The phone sends, per question, which options were picked (in the order picked), any "Other" text, and notes on a preview. The daemon builds Claude's answer from the question it stored — the same strings, joins and preview notes Claude's own dialog produces from the same choices at the keyboard — and refuses an incomplete answer or one that names an option the question does not have. The phone draws every string of a question with NUL, U+061C and the bidi embeddings, overrides and isolates as �. Past the daemon's payload limit (`max_payload_bytes`, 512 KiB) the phone gets no card to answer. A preview longer than 2,000 UTF-16 units is not annotated: Claude's dialog does not show it, and the answer leaves it out of the preview notes, as the keyboard's does. A single choice with previews has no "Other" at the Mac, so the phone offers none and the daemon refuses one.
- **A decision meant for an approval is refused.** `allow`, `option` and `text` on a question are refused before anything is claimed; `deny` declines it.

## Never claiming what it does not know

The interface separates three things that are easy to conflate: what the *process* is doing, what the *agent* is doing, and what *was last observed*. A stale connection is a fact about our knowledge, not about the agent, and is rendered as such — actions disable themselves and say why.

An unmeasured value renders as an em dash, never as zero. Zero is a measurement.

The same rule decides what a Codex session may claim. Codex's "this request was resolved" message carries only *which* request was answered — never the decision, and it reads the same however it was settled. So an approval that Codex reports resolved without an answer from an app is recorded as cleared, not answered, with the cause the daemon saw (the request resolved, its item or turn ended, the turn stopped): nothing on the wire says how it was settled, so no decision is claimed. An answer sent *from* an app keeps its decision, because the daemon composes that record itself as it forwards the answer.

Where the daemon reports an error it is quoted and attributed; where the app speaks for itself, it says so. The two are distinct types in the code precisely so they cannot be confused at a call site.

## Storage and transport

SQLite in WAL mode — one writer, a small reader pool, and all database work off the async executor. Sequence allocation and publication are serialised per session, so a subscriber can never receive sequence *N+1* before *N*; a gap is reported explicitly and replayed from the last durable point rather than skipped.

Transport is a WebSocket bound to the Tailscale interface: `wss://` when the tailnet can issue a certificate, `ws://` on the tailnet alone otherwise. Every connection presents a per-device token issued at pairing, and revoking a device closes its open sockets rather than waiting for a reconnect.

Push notifications ring only for a decision the phone can answer — an approval or a question card, Claude's or Codex's — and only once you are away from the Mac. A Codex run, or a Claude run CodeConnect started, finishing a turn is an occasion to ring, and rings only if, when the notification would go, such a decision is open somewhere; it is then about that decision. Every paired phone registered for push is rung, for Claude and Codex alike: every iPhone build since 1.1.0 shows a Codex run, so a notification is not narrowed by agent. Every notification waits while the Mac has had keyboard or mouse input in the last two minutes, read from the HID system's idle time, which needs no permission. It reads that idle time every 100 ms, asks once a second whether it still matters, and rings once the Mac has been left for two minutes — if it still matters then: a decision still unanswered, a run that has not moved on since (no new turn or thread) and is still running, a run that still exists. A run's newer finished notification retires an older one not yet rung, so a run rings what it is doing now. Every approval rings once, each card on its own. When the idle time cannot be read, notifications ring as they always did. A daemon restart drops a notification that was waiting. Sent directly (a daemon holding its own APNs key), the alert names the project the decision is in and whether it is an approval or a question, unless several decisions are waiting — then it says how many, and names a project only when they are all one run's; sent through the relay, the title is the fixed word `CodeConnect` and the project name never leaves the Mac. Either way it carries no command text, paths, diffs, tool name, risk class or identifier — a push tells the phone to reconnect and ask the log what is true. Tapping one opens the decision list — the list, never a particular card, because the payload names none.

## How Codex is run

A Codex session is started from the app: the daemon runs Codex's own app server for it, through your login shell, and talks to it over that server's socket. [`codex.md`](codex.md) is the operator page.

## Adding another agent

Sessions are acquired through an adapter, and the phone talks to a normalised event model rather than anything Claude-specific. That is what Codex was added through, and what a third agent would use: its own control channel, as Claude Code's agent host and Codex's app server are; several CLIs speak the Agent Client Protocol.

The rule that does not bend: a new adapter may add an *observation* path freely, but never an *approval* path built on parsing terminal output.
