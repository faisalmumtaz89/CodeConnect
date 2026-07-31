# Architecture

How the daemon, the launcher and the phone fit together, and why each piece is shaped the way it is.

## The central constraint

An agent's terminal output tells you what it *has done*. It does not reliably tell you what it is *waiting for*. Claude Code's session transcript contains no approval events at all — a pending permission prompt exists only in the running program's memory and is never written down.

That single fact determines the design. Anything that detects approvals by reading terminal text is inferring a signal from its absence, and it breaks the first time the rendering changes. So the system is split in two:

- **The control plane** — where an agent asks a question and *blocks*. Hooks, today. It is the only place an approval is created or answered.
- **The observation plane** — where an agent narrates what already happened. Session transcripts and pane snapshots. Useful for showing a timeline and a diff. Never used to decide anything.

Terminal bytes are read for exactly one purpose: confirming that the prompt we are about to answer is the prompt currently on screen.

## The pieces

```
 iPhone (SwiftUI)                          Mac
┌──────────────────────┐        ┌──────────────────────────────────┐
│ Fleet                │  WSS   │  ccd  (Rust, launchd)            │
│ Session timeline     │◄──────►│  ┌────────────────────────────┐  │
│ Approval card        │ tailnet│  │ ingest → assign seq → dedup│  │
│ Live terminal (SSH)  │        │  │ SQLite WAL   PK(uid, seq)  │◄─┼── source of truth
└──────────────────────┘        │  └──────────▲─────────────────┘  │
                                │             │ unix socket        │
                                │      ┌──────┴───────┐            │
                                │      │   cc-hook    │            │
                                │      └──────▲───────┘            │
                                │             │ Claude Code hooks  │
                                │   ┌─────────┴──────────┐         │
                                │   │  claude, in tmux   │◄── cc claude
                                │   └────────────────────┘         │
                                └──────────────────────────────────┘
```

**`ccd`** is the daemon. It ingests hook events and transcript lines, assigns each a monotonic sequence number per session, and writes them to SQLite before anything is published. The log is the source of truth: reconnection, backfill, deduplication and crash recovery are all one mechanism — ask for everything after sequence *N*.

**`cc`** launches an agent inside a private tmux server and generates the settings that wire Claude Code's hooks. The terminal is unchanged; the session survives a closed tab, and a per-session supervisor connects *outward* to the daemon.

**`cc-hook`** is what the hooks actually invoke. It is tiny, and it fails safe: if the daemon cannot be reached, the decision returns to the keyboard rather than being answered or silently allowed.

**The phone** is a client of the event log, never a source of truth. It renders what the log says, and shows the age of every fact.

Because the daemon is not in any agent's process-parent chain, killing it does not touch a running session. Restarting it re-attaches and backfills.

## Approving something, safely

An approval is only as trustworthy as the answer to "is this still the thing on screen?" Four mechanisms answer it:

1. **A request identity.** Every answer carries the request ID and a hash of the exact text the phone displayed. A mismatch is refused, not applied to whatever happens to be there now.
2. **A prompt generation.** Each session counts its permission requests. An answer bound to generation *N* is refused once *N+1* exists — which catches what a hash cannot: the same command asked twice, where the screen is byte-identical.
3. **A prompt fingerprint**, taken from the visible pane when the card was created and re-checked immediately before a keystroke is sent. The visible pane only — a prompt sitting in scrollback must never authorise anything.
4. **A durable claim.** The intent to type is recorded before the keystroke and resolved after it. If the daemon dies mid-injection, recovery records an indeterminate outcome and never types again.

If identity cannot be established, the answer is refused. Failing toward the human is always correct here; guessing never is.

Answers are typed into the same TTY a person would type into, so the phone and the keyboard are equal participants and whichever answers first wins.

## Never claiming what it does not know

The interface separates three things that are easy to conflate: what the *process* is doing, what the *agent* is doing, and what *was last observed*. A stale connection is a fact about our knowledge, not about the agent, and is rendered as such — actions disable themselves and say why.

An unmeasured value renders as an em dash, never as zero. Zero is a measurement.

Where the daemon reports an error it is quoted and attributed; where the app speaks for itself, it says so. The two are distinct types in the code precisely so they cannot be confused at a call site.

## Storage and transport

SQLite in WAL mode — one writer, a small reader pool, and all database work off the async executor. Sequence allocation and publication are serialised per session, so a subscriber can never receive sequence *N+1* before *N*; a gap is reported explicitly and replayed from the last durable point rather than skipped.

Transport is a WebSocket bound to the Tailscale interface: `wss://` when the tailnet can issue a certificate, `ws://` on the tailnet alone otherwise. Every connection presents a per-device token issued at pairing, and revoking a device closes its open sockets rather than waiting for a reconnect.

Push notifications are designed but not implemented. The design carries no command text, paths or diffs — a push tells the phone to reconnect and ask the log what is true.

## Adding another agent

Sessions are acquired through an adapter, and the phone talks to a normalised event model rather than anything Claude-specific. A second agent needs its own control channel — Codex exposes a JSON-RPC app server, and several CLIs speak the Agent Client Protocol — plus a tail of whatever session file it writes.

The rule that does not bend: a new adapter may add an *observation* path freely, but never an *approval* path built on parsing terminal output.
