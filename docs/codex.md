# Codex sessions

How to run OpenAI's Codex CLI under CodeConnect, what your phone can and cannot do with
it, and what to check when something is refused.

Codex sessions work differently from Claude Code sessions. Claude Code tells CodeConnect
what it is waiting for through hooks. Codex has no hooks — it speaks a JSON-RPC protocol
called the *app server*. So CodeConnect launches Codex with a small proxy in front of that
protocol (the **broker**), and the broker is what makes a phone answer safe.

## Launch one

```sh
codeconnect codex                 # in any project directory
```

That is the whole command. Everything after `codex` is passed through to the real `codex`
binary, the same way `codeconnect claude` passes arguments to `claude`:

```sh
codeconnect codex -m <model> "start on the parser"
```

The session runs in CodeConnect's private tmux server and is attached to your terminal.
Codex gets the environment of the shell you launched it from, not the tmux server's;
only `TERM` is tmux's. Codex does not see `TMUX` or `TMUX_PANE`: its output reaches
your terminal unchanged, so it behaves as it does run there directly.
Close the tab and it keeps running; `codeconnect attach <name>` brings it back.
Ctrl+Z stops Codex as it does run directly: the Codex UI and the server behind it both
stop, a turn that is running pauses with them, and your shell prints its own job line;
`fg` brings Codex back where it was. While it is stopped, a message or Stop from the
phone is refused at once, saying Codex is paused at the Mac and continues after `fg`
there, and nothing reaches Codex. The refusal does not use the message or the Stop up:
sent again after `fg`, it goes through once. A link the daemon opens meanwhile is
answered the same way, its `initialize` included, and connects once Codex runs again. An approval answered on the phone
meanwhile is not a request: it is passed to Codex, which acts on it after `fg`.
Closing the tab, `exit` or `kill %1` while it is stopped ends the session, as they end
a stopped `codex`. The phone's Terminal tab never keeps a stopped Codex held: a Ctrl+Z
typed there with no terminal at the Mac attached is let go at once.
A Ctrl+C pressed while the session is still starting is held until your terminal is
attached, then reaches Codex, so the launch quits as `codex` does rather than leaving a
session running unseen.
`codeconnect ls` lists what is running.
Your terminal shows the session through CodeConnect's built-in tmux control-mode
client, so Codex draws as it does run directly, and every key — `Ctrl-B`
included — reaches Codex byte for byte rather than tmux. This also applies to Claude
sessions and reattachments.

Interactive startup attaches the terminal before starting the Codex UI, and the
attach reports the terminal's default colors to tmux for Codex's pane, so Codex's
color query is answered with your terminal's colors. The UI waits up to five seconds
for the attach. A terminal that does not answer the color query leaves tmux without
them, and tmux 3.7 then answers black rather than nothing, so Codex draws its input
band for a black background where run directly it draws none.

When Codex exits, the session ends the way `codex` does run directly: its token usage,
then `To continue this session, run: codex resume <id>` once the conversation is saved,
or `Session ID: <id>` before it is. The hosted Codex UI is a client of the session's own
server and says goodbye as one, with a reconnect command for a socket that closes with
the session, so CodeConnect replaces that goodbye with the direct one. A Codex that
runs on its background server (the default from 0.157) says `Disconnected from this
task.` and `Reconnect: codex resume <id>` instead; a hosted session is not on that
server, so it keeps the direct goodbye, as Codex does when it runs without one. A Ctrl+C
pressed while Codex is already quitting can stop it before it says goodbye; the screen
then keeps what Codex drew.

### What is passed through, and what is refused

Your flags reach Codex as you wrote them: the sandbox, the approval policy, a profile,
`-c` overrides and feature flags included, wherever they sit on the command line — so
`codeconnect codex "fix it" --search` searches, as `codex` does. A flag CodeConnect does
not know (a new Codex flag, say) is passed on unchanged; if it takes a value, write it as
`--flag=value`, because written with a space the next word is kept as your prompt and
Codex reports the missing value. CodeConnect sets no sandbox or approval policy
of its own, so the session runs exactly as `codex` would in the same directory — Codex
picks both from your config and the project's trust.

`-C`/`--cd <dir>` works as it does in Codex: `<dir>` becomes the session's folder, where
Codex runs and where the session is listed. A relative path is read against the directory
you ran `codeconnect codex` from, and a path that is not a directory stops the launch before
anything is created.

`codeconnect codex resume …` and `codeconnect codex fork …` are hosted exactly like a new
session — same pane, same broker, and the phone follows the thread you resumed or forked.
Their own options (`--last`, `--all`, a session id, a prompt) work as in Codex; `--cd` is
the session's folder here too. `resume` or `fork` must be the first word that is not a
flag; every word after it is a session id or your prompt. Leaving the picker with Ctrl+C
(or quitting any Codex session while it is still starting) ends the launch quietly, as
it does in Codex.

Two things differ, because the hosted Codex UI is a client of the session's own server:
it does not ask whether to trust the folder (an untrusted folder still runs under Codex's
untrusted-folder restrictions), and its `resume`/`fork` picker and `--last` consider
sessions from every folder, not this one first. Codex applies both only to a `--cd` handed
to the client, and handing it one would move a session resumed from another folder into
this one.

Two kinds of argument are refused, before anything is created, with a message naming
what was refused and why:

| Refused | Examples | Why |
|---|---|---|
| The transport | `--remote`, `--remote-auth-token-env` | The terminal UI must talk to Codex through the broker, or there is no session for the phone to reach. |
| Any other subcommand | `exec`, `review`, `agents`, `queue`, … | Only the interactive Codex TUI is hosted: a new session, `resume` or `fork`. |

`--help`, `-h`, `--version` and `-V` (also inside a short cluster such as `-hC dir`), and
`help` as the first word (`codeconnect codex help resume`), are Codex's own: they run
`codex` with your arguments in your terminal, before anything else is checked (so
`--help resume` shows help, as in Codex), and start no session. The `codex` run is the
one a launch would use — the first native Codex binary CodeConnect finds — or, when
there is only a script wrapper such as the npm shim, that wrapper. Use
`codeconnect --help` for the launcher's own commands.

## What your phone can do

### Answer an approval

When Codex asks to run a command or write a file, the phone gets a card, built from the
request Codex actually sent.

| Card | Buttons | Where they come from |
|---|---|---|
| Command | *Yes, proceed* · *Yes, and don't ask again for commands that start with `…`* · *No, and tell Codex what to do differently* | Codex's own list, on the wire, for that request |
| File change | *Yes, proceed* · *Yes, and don't ask again for these files* · *No, and tell Codex what to do differently* | A fixed set of three — Codex offers no list for this family |

Whichever button you press, the phone names one of the options the card carried; it cannot
compose an answer of its own. If the card offers no option this build has words for, it is
not shown at all rather than shown with a guess.

A command card shows the command, the working directory and Codex's own reason (for
example, "the sandbox is read-only"). A file-change card shows the paths and the diff.
Every card also carries a risk class — `low`, `medium` or `high` — which CodeConnect
computes on the Mac from the request's own text.

The middle button on a command card is narrower than it looks: the "don't ask again" rule
is the exact token list Codex offered, read from the request, never re-derived from the
command text.

### Stop a running turn

Stop is offered when three things are true at once: the daemon really supports it, the
session is a Codex session, and the Mac's control link to that session says `subscribed`.
A refusal can still come back after you tap — the link can drop between the moment the
phone drew the screen and the moment you press it.

| You see | It means |
|---|---|
| Stopped | The turn reached its aborted end. |
| Already asked | This exact ask had already run. The turn was not stopped twice. |
| Refused, with a sentence | Nothing was sent, and the sentence says why. Tapping Stop again is a new try — unless the sentence reports how an earlier tap for that turn already ended (it was refused, or the turn ended on its own), which the daemon answers from its record without sending anything. |
| Not known | The stop was issued, and the Mac either stopped watching before it saw what happened or got an answer from Codex that does not show whether it was taken. **It is never retried for you.** Look at the Mac. |

### Say something

You can type into a Codex session from the phone, up to 8,192 bytes per message. What
happens depends on whether the session is busy:

* **Idle** — your words start a new turn.
* **Busy** — your words join the turn that is already running. There is no second turn:
  one turn started it, one turn ends it, and the reply names that same turn.

| You see | It means |
|---|---|
| Started | The session was idle and your words began a new turn. |
| Joined | A turn was running and your words went into it. |
| Already said | This exact message had already been sent. Nothing was said twice. |
| Refused, with a sentence | Nothing was said, and the sentence says why. Sending the same words again is a new try — unless the sentence reports an earlier attempt under the same request, which the daemon answers from its record without sending anything. |
| Not known | It was written, and the outcome was never seen or Codex's answer does not show whether it was taken. **Never retried for you** — saying the same thing twice cannot be undone. |

## What your phone cannot do

Four limits are measured, not guessed. None of them has a workaround from the phone.

**1. Stop can go quiet after a reconnect, on turns that are doing work.** If the Mac's link
to a Codex session drops while a turn is running a command or editing a file, the
reconnect cannot safely work out which turn is running, so Stop for that turn is refused
until it ends on its own or a new turn starts. Until then, stop it at the Mac. This is
deliberate: the alternative is aiming a stop at a turn that may already be over.

**2. An answer given at the Mac keyboard does not say what was chosen.** Codex's "this was
resolved" message carries only *which* request was answered — never the decision, and it
looks the same however it was settled. So the phone can tell you an approval was answered
at the Mac, but not which button was pressed. Answers from the phone keep their decision,
because the Mac composes that record itself as it forwards the answer.

**3. Stop ends the turn, not the shell it started.** Measured on 0.153.4: an interrupted
turn ends, and a command the agent had already launched runs to completion in its own
shell — in the recorded case, about forty-five seconds later. Stop is not a kill switch
for work already in flight.

**4. Two sessions started in the same millisecond can sort the wrong way round.** Session
ids are ordered within one process, so a session that replaces another always sorts after
it. But two `codeconnect` launches are two processes, and two ids minted in the same
millisecond by different processes separate only on random bits. The odds are about one in
2^80 per millisecond. Closing it would need a shared minting service, which is not built.

Beyond those: the phone cannot create or switch a Codex thread, cannot choose the model,
the effort, the sandbox or the working directory, and cannot ask Codex for anything
besides answering an approval, stopping a turn and saying something.

## The security boundary

CodeConnect puts a broker between Codex's terminal UI and Codex's own app server, on two
separate sockets — one for the keyboard, one for the daemon that speaks for the phone —
and which socket a message arrived on is what decides what it may do.

**The keyboard's socket is a passthrough.** The person at the Mac is as trusted as in
native Codex: the terminal UI is launched with the keyboard's own flags and no sandbox or
approval policy of CodeConnect's, everything it sends reaches Codex byte for byte, and every
request Codex sends — approvals, tool calls, questions for the user — reaches the terminal
UI. The broker only watches that traffic, to know which thread the keyboard is on and
whether a turn is running. The one keyboard message it holds back is an answer to an
approval the phone has already answered.

**The phone's socket refuses by default**: any method, and any *parameter*, that is not
explicitly admitted is refused with a numeric code and never reaches Codex. Four
code-execution methods are refused, always. The phone may not create, fork or unsubscribe a
thread, may not change a thread's settings, and may not do Codex's account and model reads.
Its turn message is exactly fourteen fields — it cannot name a model, an effort, a service
tier or a workspace. Every field but the thread and the words is sent empty, so a turn the
phone starts runs under whatever approval policy, sandbox and permissions the thread has at
that moment, which only the keyboard can change; a turn message from the phone that names
any of them is refused. That also means **the phone has exactly the keyboard's
authority**: a session whose keyboard runs with approvals and the sandbox turned off runs a
phone's turn the same way.

A phone's turn, steer, stop and approval answer must all name **the thread the keyboard is
on**. When the keyboard moves — `/new`, `/resume`, a fork — the phone may act on nothing
until the move lands, and then only on the new thread; a thread the keyboard has left stays
readable from the phone and nothing more. The phone follows the keyboard to that thread
whichever way it moved: Codex announces a thread it creates (`/new`, a fork) but not one the
keyboard resumes, so when the move lands on a thread nobody announced, the broker tells the
daemon itself, once. A turn is refused while one is running, whoever
started it. The phone is handed only the requests it can answer — a command or file-change
approval on that thread. Every other request Codex asks is left to the keyboard: Codex asks
every connected client and takes the first answer, so the phone's leg answering anything,
even with a refusal, would answer the keyboard's question for it.

When Codex or the broker refuses something, the phone is told a **numeric code** and a
fixed sentence, never the upstream message: those messages have been measured naming a
turn id the phone never sent, and a future one could say anything. The full text stays on
the Mac, in the log.

## When Codex updates

CodeConnect runs whichever Codex you installed. It pins no version and checks no schema:
the launcher reads `codex --version` and refuses only a binary that cannot say what it is.

What protects the phone does not depend on the Codex version. The broker admits only a
fixed set of phone messages, each in a fixed shape, and refuses everything else — so a
Codex release that changed one of those shapes would have the phone's requests refused
rather than let through. The keyboard's traffic is not inspected, so a new Codex feature
works at the keyboard the day it ships.

Two things are worth re-checking live after a Codex update, because no unit test can see
them: that a phone turn's empty settings still mean "keep the thread's own", and that the
flags and subcommands Codex accepts still match the launcher's lists. A flag the launcher
does not know is passed on unchanged but takes no spaced value (write `--flag=value`), and
a new subcommand is refused only once it is added to the launcher's list — until then the
fence keeps it from being dispatched, and it reaches Codex as prompt text.

### The identity check on the `codex` binary

The launcher hashes the `codex` binary when it finds it, and checks the hash again just
before each time it runs it — for `--version`, for the app server and for the terminal
UI. If `codex` is updated while a launch is starting, the check sees different bytes and
the launch stops and says so; run `codeconnect codex` again once the update has finished.
An update that lands after the last check and before that program starts is not caught:
macOS cannot run a program from the file that was checked.
It is a guard against a benign update landing mid-launch, not against a hostile process,
and it never changes the file.

CodeConnect 0.7.0 and 0.8.0 locked the binary (`uchg`) while they launched, and a launch
killed at the wrong moment could leave it locked: `codex` still runs, but `npm` or an
installer fails with `Operation not permitted`. To unlock it:

```sh
chflags nouchg <the path named in the error>
```

The standalone installer keeps earlier releases, and the locked one may be one of them;
this unlocks every release it keeps:

```sh
find ~/.codex/packages/standalone -flags +uchg -exec chflags nouchg {} +
```

## Quota

A Codex session runs as **you**. It uses your own `CODEX_HOME` — `~/.codex` unless you set
that variable — which is where your Codex sign-in lives.

So: **turns your phone starts are your turns.** Saying something from the phone spends the
same account allowance as typing it at the Mac. There is no CodeConnect account, no proxy
key and no separate meter.

CodeConnect itself never starts a model turn. Everything it does around a session — the
version probe, the hash, the recovery sweeps — is local work that opens no
session and contacts no account.

## Troubleshooting

### Start here

```sh
codeconnect daemon status
```

Real output, with the home path, host, pid and build id replaced:

```text
plist    ~/Library/LaunchAgents/com.codeconnect.ccd.plist (1988 bytes)
launchd  loaded, running as pid NNNNN
daemon   pid NNNNN · version 0.6.0 · build <build id>
         protocol 1.20 · up since 2026-09-08T16:02:47.515Z
         ws://your-mac.your-tailnet.ts.net:8787 · 0 session(s) attached
managed  yes (com.codeconnect.ccd)
logs     ~/.codeconnect/logs
```

The number after `protocol` is the one that decides what the phone can do — see the table
below. `codeconnect ls` shows what is running in tmux even when the daemon is down.

### The refusal sentence on the phone

When a Stop or a message is refused, the phone shows the Mac's own sentence. There are 71
of them and they fall into four kinds. Which kind it is tells you whether trying again is
worth anything:

| Kind | How many | What it means | Try again? |
|---|---|---|---|
| Link state | 21 | A fact about the Mac's control link right now — reconnecting, not yet watching the thread, no link at all — or Codex paused at the Mac with Ctrl+Z. | Yes, shortly (after `fg` for a pause). |
| This Mac's own store | 5 | A local lookup or record failed **before** anything was sent. Nothing reached Codex. | Yes. Then check the Mac. |
| Settled | 43 | The ask was wrong, the id is spent, or the outcome is already recorded. | No. |
| Wire code | 2 | Codex, or CodeConnect's own broker rules, refused the write in a way that proves nothing reached Codex, and the sentence carries the numeric code — never the refuser's message. | Yes, from the phone. A refusal by a rule refuses again. |

Every sentence is written in one place in the daemon and emitted as
[`fixtures/codex/refusal-sentences.json`](../fixtures/codex/refusal-sentences.json), which
a build gate compares byte for byte. If you want the exact wording of all 71, read that
file.

Two words in those sentences are worth knowing: **rejected** means nothing was sent, and
**indeterminate** means it was sent and nobody saw the result. 54 of the 71 are the first,
17 are the second.

### Lines in the daemon log

The daemon's log lives under `~/.codeconnect/logs`. Four lines start with `codex recovery:`
and all four are about the daemon's repair pass over Codex launches whose launcher and
cleanup helper both died:

| Line | What it means |
|---|---|
| `codex recovery: <the pass's own account>` | One line copied from the repair pass, e.g. that it recorded such a launch as failed and armed a new cleanup helper. Informational. |
| `codex recovery: … the next pass asks again` | This repair pass did not finish. A launch may still be waiting for its cleanup; the daemon retries in five minutes. |
| `codex recovery: the pass is completing again` | An earlier complaint has cleared. Healthy. |
| `codex recovery: CODECONNECT_LAUNCHER_BIN names …, which is not a file` | You pointed that variable at a path that does not exist. The daemon ignored it. |

A Codex stopped with Ctrl+Z before its conversation is saved (no turn yet) makes the
daemon's link log one `STOP-AND-AMEND — thread/resume … answered with something this
build cannot read`: the broker refused the link's own resume while Codex was stopped.
The link reconnects by itself once `fg` brings Codex back.

Lines from a launch itself are prefixed `codeconnect:`, and the per-session repair helper
writes to `~/.codeconnect/logs/codex-custodian-<uid>.log`.

### What the phone shows, by daemon version

`codeconnect daemon status` prints `protocol 1.<minor>`. The phone hides what the daemon
has not advertised rather than failing when you tap.

| Daemon | Approval cards | Stop | Say something | Link state on the fleet list |
|---|---|---|---|---|
| 1.16 | Yes | Hidden — the daemon refuses every stop | No | No |
| 1.17 | Yes | Yes | No | No |
| 1.18 | Yes | Yes | Yes | No |
| 1.19 | Yes, and a card can offer Stop for its own turn | Yes | Yes | Yes — `subscribed`, `bound`, `offline` or `none` |
| 1.20 | Yes | Yes | Yes, including the first turn of a new session | Yes — adds `bound_not_started`: the thread has no history yet, so the phone may start it |

`subscribed` is the only state in which a Stop reaches Codex, and the ordinary one for a
message. `bound_not_started` (daemon 1.20) is the one exception: the thread is bound but has
no history yet, so the phone may say something to start its first turn; Stop is refused there
because nothing is running. `bound` means the Mac knows the thread but is not receiving its frames. `offline` means the link is
dialling, backing off or mid-handshake. `none` means the run has no Codex control link at
all — every Claude session reads `none`, and so does any Codex run whose supervisor has
disconnected. It is a fact about the **link**, not about whether the session is alive.

### Common causes

| Symptom | Likely cause |
|---|---|
| The launch refuses and names a flag or subcommand | Only the interactive TUI is hosted, through CodeConnect's own transport. See [what is refused](#what-is-passed-through-and-what-is-refused). |
| `npm`/installer cannot update `codex` — `Operation not permitted` | CodeConnect 0.7.0 or 0.8.0 left `codex` locked. See [the identity check](#the-identity-check-on-the-codex-binary) for the one command that unlocks it. |
| No Stop button on a Codex session | Daemon below 1.17, or the link is not `subscribed`. |
| No composer on a Codex session | Daemon below 1.18. |
| Stop refused right after the Mac reconnected | Limit 1 above — the turn is doing tool work. Stop it at the Mac. |

## See also

* [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — how the daemon, launcher and phone fit together
* [`mac/README.md`](../mac/README.md) — the Mac side in detail: pairing, TLS, the LaunchAgent
* [`ios/README.md`](../ios/README.md#codex-sessions) — the phone side: the card, the two controls and the one rule that gates them
* [`fixtures/README.md`](../fixtures/README.md) — every recorded Codex measurement and what it proves
