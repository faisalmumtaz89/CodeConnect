# Architecture

How the daemon, the launcher and the phone fit together, and why each piece is shaped the way it is.

## The central constraint

An agent's terminal output tells you what it *has done*. It does not reliably tell you what it is *waiting for*. Claude Code's session transcript contains no approval events at all — a pending permission prompt exists only in the running program's memory and is never written down.

That single fact determines the design. Anything that detects approvals by reading terminal text is inferring a signal from its absence, and it breaks the first time the rendering changes. So the system is split in two:

- **The control plane** — where an agent asks a question and *blocks*. Claude Code's hooks; for Codex, which has none, its own JSON-RPC app server, read through a broker. It is the only place an approval is created or answered.
- **The observation plane** — where an agent narrates what already happened. Session transcripts and pane snapshots. Useful for showing a timeline and a diff. Never used to decide anything: the daemon acts on hooks alone. For Claude, the end of a turn shown on the phone is placed from the transcript, so it lands after the turn's last reply, and the phone's running or done status follows it; that is display, not a decision.

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

**`codeconnect`** launches an agent inside a private tmux server. For Claude Code it generates the settings that wire the hooks; for Codex it starts the broker described below. The terminal is unchanged either way; the session survives a closed tab, and a per-session supervisor connects *outward* to the daemon. The agent runs as a job of the pane's own process, so Ctrl+Z stops it for real and the terminal's shell takes over as it would for the agent run directly; the attachment tells that process when `fg` resumes it.

**`cc-hook`** is what the hooks actually invoke. It is tiny, and it fails safe: if the daemon cannot be reached, the decision returns to the keyboard rather than being answered or silently allowed.

**The phone** is a client of the event log, never a source of truth. It renders what the log says, and shows the age of every fact.

Because the daemon is not in any agent's process-parent chain, killing it does not touch a running session. Restarting it re-attaches and backfills.

## Approving something, safely

Every answer to a Claude card goes back through the `PermissionRequest` hook Claude holds for that card's own call, and nothing is ever typed into a permission prompt. Claude queues background agents' prompts at the Mac, so the prompt on screen says nothing about which call a card is for: an answer typed into it could run, deny or answer another agent's call.

- **One card, one call.** A request is joined to its call's `tool_use_id` by the `PreToolUse` that fires just before it, keyed by run, prompt, tool, input and background agent. If two open calls share that key, the request joins neither, and its card is read-only.
- **Held for the phone.** `cc-hook` waits for the daemon as long as Claude waits on it. A main-conversation card is always held, and Claude draws its dialog at the same time, so the Mac and the phone can both answer and Claude takes the first answer. A background agent's card is held only while nobody is at the Mac, by the same rule as its questions (below); when someone comes back, its hook returns with no decision and Claude draws the dialog there.
- **What the phone can send.** Allow (and option 1, an older phone's yes) runs exactly this call; `ExitPlanMode` is allowed with the input it runs with, as Claude requires. Deny stops the main conversation's turn, as Escape does, and denies a background agent's call in Claude's own words for an Escape at that agent's dialog, so the agent carries on. A denial with a reason sends the reason to the agent that asked, in Claude's own words for "No, and tell Claude what to do differently". A standing permission ("always") is chosen at the Mac: the hook's suggested rule can differ from what Claude's own dialog row saves (measured on 2.1.290: `docker compose up -d` suggests `docker compose *`, the row saves `docker compose up *`), so the phone offers none and refuses an older phone's option 2. Any other decision is refused before anything is claimed.
- **How a card ends.** A card ends when its call's own result appears: the call's `PostToolUse`, or its `tool_result` in the transcript, which says whether Claude ran the call or rejected it (`toolDenialKind`). That closes a card answered at the Mac, lets its hook go, and leaves nothing to hand over to a phone answer claimed in the meantime, and it holds even if Claude does not end the hook after an Escape. Escape at the Mac normally ends the hook itself (74 to 1,242 ms later, median 83, over 32 runs on 2.1.290); the card then stops being answerable and waits for the call's result to say how it ended. A result filed before the call's request is read is kept by the call's `tool_use_id`: the request is then not held, and its card closes from that result. When the run ends, its open cards close as "the session ended before anyone answered" and a held hook is let go, whatever process it runs under; an answer already in flight finishes first. A daemon restart ends every hold.
- **Read-only cards.** A card from an older `cc-hook`, one joined to no single call, one for a tool whose own dialog alone answers it, and one whose hold has ended are shown on the phone and answered at the Mac. An MCP tool can declare that only its own dialog answers it, and nothing in its request says so; Claude ignores an allow from the phone for such a tool and its dialog stays at the Mac. The phone answers Claude approvals only from a daemon that advertises `hook_only_approvals`: an older one types the answer into whatever prompt is on screen.

Every answer carries the request ID and a hash of the exact text the phone displayed; a mismatch is refused. The intent to answer is recorded durably before the hook is handed anything, and resolved after: if the daemon dies in between, recovery records an indeterminate outcome and never sends it again.

What is recorded for a phone answer to an approval is what the daemon knows: it was sent to Claude ("Sent to Claude from a phone", with `indeterminate` set), and the card closes at once. Claude takes the first answer, the Mac's or the phone's, and may not use the phone's: the Mac may have answered first, an Escape a few milliseconds earlier wins, and Claude ignores an allow for an MCP tool only its own dialog answers. Claude records the same result whichever answer it took (measured on 2.1.290), so nothing later says which it was; the call's own result, in the timeline, says what happened to it.

## Claude's own questions

Claude Code asks every `AskUserQuestion` through a `PermissionRequest`, but it is a question to you, not a permission, and nothing is ever typed into its dialog: a keystroke picks an option. The daemon recognises it by tool name and answers it only through the hook Claude is holding, with the question's own answers.

- **The answer is built by the daemon.** The phone sends, per question, which options were picked (in the order picked), any "Other" text, and notes on a preview. The daemon builds Claude's answer from the question it stored — the same strings, joins and preview notes Claude's own dialog produces from the same choices at the keyboard — and refuses an incomplete answer or one that names an option the question does not have. Claude's dialog shows a question only after rewriting what it will not draw as written — invisible and format characters (the Hangul fillers among them), controls, bidi marks, variation selectors, tabs, more than eight zero-width characters joined to one character, overlong labels — and answers with what it showed. A question is not offered to the phone at all if its text, labels or previews hold anything but letters, numbers, punctuation, symbols, plain spaces, combining marks and the zero-width joiner and non-joiner (and line breaks in a preview), if more than eight of those zero-width characters, Hangul vowel and final jamo counted with them, are joined to one character, or if two options look alike, or if its card would exceed the daemon's payload limit (`max_payload_bytes`, 512 KiB), past which the phone gets no card to answer. A preview longer than 2,000 UTF-16 units is neither checked nor annotated: Claude's dialog does not show it, so nothing in it can be shown differently, and the answer leaves it out of the preview notes, as the keyboard's does. That keeps questions in scripts such as Hindi, Thai or Hebrew with points answerable from the phone; how much the dialog shows as written was measured on Claude Code 2.1.286, and a later version may draw differently. A single choice with previews has no "Other" at the Mac, so the phone offers none and the daemon refuses one.
- **First complete answer wins.** For the main conversation Claude draws its dialog while the hook is held, so the Mac and the phone can both answer and Claude takes whichever answer comes first. The daemon records the phone's answer as applied only when the `PostToolUse` that follows shows Claude ran with it; if the Mac got there first with a different answer, the phone is told so and nothing changes. The `PostToolUse` carries the answer, not who gave it, so the same answer chosen at the Mac first is recorded as the phone's: what Claude ran with is exact, who chose it is not. A decline has no `PostToolUse` and Claude reports none, so one is recorded as unconfirmed unless the Mac's answer shows up first. A Mac answer releases the held hook at once.
- **The hold lasts as long as the question.** The `PermissionRequest` hook's timeout is 2,147,483 s (about 24.8 days), the largest whose milliseconds fit the 32-bit timer Claude arms it with. Escape or the session ending makes Claude end the hook, and the daemon reads its connection closing as the question being gone. "Chat about this" at the Mac leaves the hook running; the end of the main conversation's turn tells the daemon the question is gone, and it lets the hook go. Nothing earlier is proof: a tool asked for in the same message as the question fires its `PreToolUse` and `PermissionRequest` at the same moment as the question's, its dialog waiting behind the question's. A daemon restart ends every hold: the question stays at the Mac and the card says so. One case is known and left as is: after "Chat about this", if that turn is interrupted, no hook says the question is gone, so the card stays answerable until the next turn ends; an answer from the phone in that window reaches a hook Claude no longer reads, changes nothing, and the card says Claude never confirmed it.
- **A background agent's question is held only while nobody is at the Mac.** Claude draws such a question only after its hook returns, so holding it hides it. "At the Mac" is keyboard or mouse input anywhere on the Mac in the last 10 seconds, read from the HID system's idle time, which needs no permission; when it cannot be read, someone counts as there. A question asked while someone is at the Mac goes there at once. A held one is checked every 100 ms, and once input resumes its hook returns with no decision and Claude draws the question at the Mac: 83 to 152 ms from the input to the dialog on the pane, over eight runs, the input's moment read from the HID idle time. Nothing in your terminal is read or changed for this, so keys typed as you come back are Claude's to handle, as when a question pops up while you type, and can answer the question once it is drawn: on Claude Code 2.1.286 a key arriving as the dialog first draws is dropped, the same with CodeConnect 0.10.1 where nothing is held, and one a moment later answers it. If the daemon stops while a question is held, the hook ends with it and Claude draws the question, as plain Claude would.
- **Decline is Escape.** For the main conversation it stops the turn; for a background agent it denies the question and the agent carries on.
- **Older phones** still see the question as an approval. `allow`, `option` and `text` on it are refused before anything is claimed; `deny` declines a question held for the phone through its hook, and is refused on any other, because the prompt on screen need not be the question's.

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

**The launcher.** `codeconnect codex` runs the real `codex` binary in the same private tmux server `codeconnect claude` uses, passing your arguments through as native `codex` would read them — the sandbox, the approval policy, profiles and config overrides included; CodeConnect sets none of its own. `--cd <dir>` becomes the session's folder, a flag it does not know is passed on unchanged, and `--help`/`--version` run `codex` itself with no session. Only the transport flags (the terminal UI must be the broker's client) and every `codex` subcommand but the interactive TUI are refused, *before* anything is created, with a message naming what was refused and why.

**The broker, on two sockets.** Between Codex's terminal UI and Codex's own app server sits a broker, listening on two separate sockets — one for the keyboard, one for the daemon — and which socket a message arrived on is what decides what it may do. **The keyboard's socket is a passthrough**: the person at the Mac is as trusted as in native Codex, so every frame the terminal UI sends reaches Codex unchanged and every request Codex sends reaches the terminal UI. The broker watches that traffic to follow which thread the keyboard is on and whether a turn is running, and holds back exactly one kind of keyboard frame: an answer to a command or file-change approval whose one answer is already taken — by the phone, or by the keyboard's own earlier answer to it. **The daemon's socket refuses by default**: any method, and any *parameter*, not explicitly admitted is refused with a numeric code and never reaches Codex. Four code-execution methods are refused there, always.

**The daemon's control link, and the phone's narrower frame.** The daemon subscribes to the session over its own socket, which is where approval cards, turn status and the timeline come from. That socket is deliberately the weaker of the two: it may not create, fork or unsubscribe a thread and may not read Codex's account or model settings, and the turn message it may write carries exactly fourteen fields — so it cannot name a model, an effort, a service tier or a workspace. Every turn, steer, stop and approval answer it sends must name the thread the keyboard is on; while the keyboard is moving to another thread it may act on none, and a turn is refused while one is running, whoever started it. It is handed only the requests it can answer — a command or file-change approval on that thread — and every other request is left to the keyboard, because Codex asks every connected client and takes the first answer. Every field but the thread and the words is sent empty, so a turn the phone starts runs under whatever approval policy, sandbox and permissions the thread has at that moment, which only the keyboard can change; a turn message from the phone that names any of them is refused. Its resume names the thread and nothing else. When Codex or the broker refuses a write, the phone is told a numeric code and a fixed sentence, never the upstream message; the full text stays in the Mac's log. While Codex is stopped with Ctrl+Z (its UI and its app server both), every request on this socket is refused at once rather than left waiting for `fg`.

**The identity check on the `codex` binary.** The launcher pins `codex` by the SHA-256 of its bytes when it resolves it, and re-checks that digest before each exec — `--version`, the app server, the terminal UI — and each check confirms, before it closes the file it hashed, that the name still points at that same file. A mismatch refuses the launch. macOS cannot execute a program by file descriptor, so the interval between the last check and the exec stays open: the check is a guard against a benign update landing mid-launch, not against a hostile process.

**No version gate.** CodeConnect runs whichever Codex is installed. The phone is protected by the broker's fixed phone shapes, which refuse anything a newer Codex changes, and the keyboard is not inspected — so nothing at launch compares Codex against a recorded schema.

## Adding another agent

Sessions are acquired through an adapter, and the phone talks to a normalised event model rather than anything Claude-specific. That is what Codex was added through, and what a third agent would use: its own control channel — several CLIs speak the Agent Client Protocol — plus a tail of whatever session file it writes.

The rule that does not bend: a new adapter may add an *observation* path freely, but never an *approval* path built on parsing terminal output.
