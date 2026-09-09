# Architecture

How the daemon, the launcher and the phone fit together, and why each piece is shaped the way it is.

## The central constraint

An agent's terminal output tells you what it *has done*. It does not reliably tell you what it is *waiting for*. Claude Code's session transcript contains no approval events at all — a pending permission prompt exists only in the running program's memory and is never written down.

That single fact determines the design. Anything that detects approvals by reading terminal text is inferring a signal from its absence, and it breaks the first time the rendering changes. So the system is split in two:

- **The control plane** — where an agent asks a question and *blocks*. Claude Code's hooks; for Codex, which has none, its own JSON-RPC app server, read through a broker. It is the only place an approval is created or answered.
- **The observation plane** — where an agent narrates what already happened. Session transcripts and pane snapshots. Useful for showing a timeline and a diff. Never used to decide anything.

Terminal bytes are read for exactly one purpose: confirming that the prompt we are about to answer is the prompt currently on screen.

## The pieces

```
 iPhone (SwiftUI)                          Mac
┌──────────────────────┐        ┌──────────────────────────────────┐
│ Fleet                │  WSS   │  ccd  (Rust, launchd)            │
│ Session timeline     │◄──────►│  ┌────────────────────────────┐  │
│ Approval card        │ tailnet│  │ ingest → assign seq → dedup│  │
│ Live terminal (tmux) │        │  │ SQLite WAL   PK(uid, seq)  │◄─┼── source of truth
└──────────────────────┘        │  └──────────▲─────────────────┘  │
                                │             │ unix socket        │
                                │      ┌──────┴───────┐            │
                                │      │   cc-hook    │            │
                                │      └──────▲───────┘            │
                                │             │ Claude Code hooks  │
                                │   ┌─────────┴──────────┐         │
                                │   │  claude, in tmux   │◄── codeconnect claude
                                │   └────────────────────┘         │
                                └──────────────────────────────────┘
```

**`ccd`** is the daemon. It ingests hook events and transcript lines, assigns each a monotonic sequence number per session, and writes them to SQLite before anything is published. The log is the source of truth: reconnection, backfill, deduplication and crash recovery are all one mechanism — ask for everything after sequence *N*.

**`codeconnect`** launches an agent inside a private tmux server. For Claude Code it generates the settings that wire the hooks; for Codex it starts the broker described below. The terminal is unchanged either way; the session survives a closed tab, and a per-session supervisor connects *outward* to the daemon.

**`cc-hook`** is what the hooks actually invoke. It is tiny, and it fails safe: if the daemon cannot be reached, the decision returns to the keyboard rather than being answered or silently allowed.

**The phone** is a client of the event log, never a source of truth. It renders what the log says, and shows the age of every fact.

Because the daemon is not in any agent's process-parent chain, killing it does not touch a running session. Restarting it re-attaches and backfills.

## Approving something, safely

There are two ways an answer can reach the agent, and the daemon advertises which one is live as `answer_path`:

- **`hook_return`** — the decision is returned through the hook that asked for it, carrying Claude's own `tool_use_id`. Nothing is typed, and no pane is read: the question and the answer share one connection, so there is nothing to re-identify. Set `hold_ms` in `~/.codeconnect/config.json` to enable it. The cost is that the Mac's own prompt is held open while the phone is asked, and if nobody answers within the hold the local prompt appears as usual.
- **`send_keys`** — the default (`hold_ms: 0`). The hook has already returned by the time you answer, so the thing waiting is the Mac's own prompt, and the answer is typed into it.

Everything below is about the second path, where the answer arrives after the fact and the prompt has to be re-identified before anything is typed.

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

The same rule decides what a Codex session may claim. Codex's "this request was resolved" message carries only *which* request was answered — never the decision, and it reads the same however it was settled. So an approval answered at the Mac's keyboard is rendered as answered and nothing more: the phone is not told which button was pressed, because nothing on the wire says. An answer sent *from* the phone keeps its decision, because the Mac composes that record itself as it forwards the answer.

Where the daemon reports an error it is quoted and attributed; where the app speaks for itself, it says so. The two are distinct types in the code precisely so they cannot be confused at a call site.

## Storage and transport

SQLite in WAL mode — one writer, a small reader pool, and all database work off the async executor. Sequence allocation and publication are serialised per session, so a subscriber can never receive sequence *N+1* before *N*; a gap is reported explicitly and replayed from the last durable point rather than skipped.

Transport is a WebSocket bound to the Tailscale interface: `wss://` when the tailnet can issue a certificate, `ws://` on the tailnet alone otherwise. Every connection presents a per-device token issued at pairing, and revoking a device closes its open sockets rather than waiting for a reconnect.

Push notifications ring when a run needs a human, and when one finishes a turn. Sent directly (a daemon holding its own APNs key), the alert names the project and says why it rang unless several runs are holding decisions at once — then it says how many; sent through the relay, the title is the fixed word `CodeConnect` and the project name never leaves the Mac. Either way it carries no command text, paths, diffs, tool name, risk class or identifier — a push tells the phone to reconnect and ask the log what is true. Tapping one opens the decision list when an approval rang, and the fleet otherwise — the list, never a particular card, because the payload names none.

## How Codex is hosted

Codex is the second agent, and it arrived through the seam below rather than around it. Five pieces carry it. [`codex.md`](codex.md) is the operator page and has the detail for each.

**The launcher.** `codeconnect codex` runs the real `codex` binary in the same private tmux server `codeconnect claude` uses, passing your arguments through — except the ones CodeConnect owns for the session. The working directory, the sandbox, the transport, a named profile, the approval controls and every `codex` subcommand but the interactive TUI are refused *before* anything is created, and the refusal names the setting and who owns it. They are refused because everything else is checked against them: an approval CodeConnect answers from a phone cannot also be handed to a flag.

**The broker, on two sockets.** Between Codex's terminal UI and Codex's own app server sits a broker, listening on two separate sockets — one for the keyboard, one for the daemon — and which socket a message arrived on is what decides what it may do. It **refuses by default**: any method, and any *parameter*, not explicitly admitted for that socket is refused with a numeric code and never reaches Codex. Four code-execution methods are refused on every socket, always.

**The daemon's control link, and the phone's narrower frame.** The daemon subscribes to the session over its own socket, which is where approval cards, turn status and the timeline come from. That socket is deliberately the weaker of the two: it may not create or fork a thread and may not read Codex's account or model settings, and the turn message it may write carries exactly fourteen fields against the keyboard's twenty-four — so it cannot name a model, an effort, a service tier or a workspace, and the policy fields it may name are held to exact equality with the session's own launch. When Codex or the broker refuses a write, the phone is told a numeric code and a fixed sentence, never the upstream message; the full text stays in the Mac's log.

**The freeze on the `codex` binary.** macOS cannot execute a program by file descriptor, so between hashing `codex` and running it there is a window in which an installer could swap the file. The launcher closes it by setting the immutable flag on the binary for the length of the launch and clearing it once the session is past `exec`. A `SIGKILL` inside that window can leave the flag standing, so it is cleaned up twice over: the next `codeconnect codex` clears leftovers as its first act, and the daemon sweeps at startup and every five minutes, logging what it did.

**The guarded-surface gate.** No Codex version number is pinned. What is checked, at every launch and before anything is created, is the part of Codex the broker actually guards — the app-server methods it filters, and Codex's root subcommand and flag list — against what CodeConnect was grounded on. An unchanged surface is admitted whatever the build calls itself; a moved one refuses the launch and names each thing that moved, because the checks that keep a session inside its sandbox have not been proven for a surface nobody measured.

## Adding another agent

Sessions are acquired through an adapter, and the phone talks to a normalised event model rather than anything Claude-specific. That is what Codex was added through, and what a third agent would use: its own control channel — several CLIs speak the Agent Client Protocol — plus a tail of whatever session file it writes.

The rule that does not bend: a new adapter may add an *observation* path freely, but never an *approval* path built on parsing terminal output.
