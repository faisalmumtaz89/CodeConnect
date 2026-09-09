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
Close the tab and it keeps running; `codeconnect attach <name>` brings it back.
`codeconnect ls` lists what is running.

### What is refused, and why

CodeConnect owns some of the launch. If you pass one of those settings, the launch stops
before anything is created and says which setting and who owns it. There are six kinds of
refusal:

| Refused | Examples | Why |
|---|---|---|
| A flag CodeConnect sets | `-C`/`--cd`, `-s`/`--sandbox`, `--add-dir`, `--remote` | The working directory, the sandbox and the transport are the session's identity. Everything else is checked against them. |
| A profile | `-p`/`--profile` | A named codex profile can carry approval and hook settings, so the profile choice is CodeConnect's. |
| An approval control | `-a`/`--ask-for-approval`, `--full-auto`, `--yolo`, the `--dangerously-bypass-*` flags | These move who answers an approval. Your phone answers approvals, so they cannot be handed away. |
| A config key CodeConnect owns | `-c approval_policy=…`, `--enable`/`--disable` of a pinned feature | Same reason, reached by another spelling. Nesting and dotted paths are both caught. |
| A subcommand | `resume`, `fork`, `exec`, `agents`, `queue`, … | Only the interactive Codex TUI is hosted. |
| Anything it cannot classify | an unexpandable short-flag cluster, a `-c` value it cannot decode | It fails closed rather than forward something it does not understand. |

One refusal is worth calling out because it is not a permission problem: Codex's
"request permissions tool" feature is pinned **off**, because it asks for a permission
profile that only the terminal can grant, and CodeConnect answers approvals from the phone.

`codeconnect codex` has no `--help` of its own — `--help` is passed to `codex`. Use
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
| Refused, with a sentence | Nothing was sent at all, and the sentence says why. |
| Not known | The stop was issued and the Mac stopped watching before it saw what happened. **It is never retried for you.** Look at the Mac. |

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
| Refused, with a sentence | Nothing was said, and the sentence says why. |
| Not known | It was written and the outcome was never seen. **Never retried for you** — saying the same thing twice cannot be undone. |

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
separate sockets — one for the keyboard, one for the daemon — and which socket a message
arrived on is what decides what it may do. The broker **refuses by default**: any method,
and any *parameter*, that is not explicitly admitted for that socket is refused with a
numeric code and never reaches Codex. Four code-execution methods are refused on every
socket, always. **The phone's socket is narrower than the keyboard's**: it may not create
or fork a thread, may not do Codex's account and model reads, and its turn message is
exactly fourteen fields against the keyboard's twenty-four — it cannot name a model, an
effort, a service tier or a workspace, and the policy fields it may name are held to exact
equality with the session's own launch. When Codex or the broker refuses something, the
phone is told a **numeric code** and a fixed sentence, never the upstream message: those
messages have been measured naming a turn id the phone never sent, and a future one could
say anything. The full text stays on the Mac, in the log.

## When Codex updates

CodeConnect does not pin a Codex version number. It checks the part of Codex it actually
guards — the app-server methods the broker filters, and Codex's root subcommand and flag
list — against what it was grounded on. This is called the **guarded-surface gate**, and it
runs at every launch, before anything is created.

* If that surface is unchanged, the build is admitted whatever it calls itself. Weekly
  Codex releases that do not move the wire are hosted with no change here.
* If it moved, the launch is refused, and the refusal **names each thing that moved**.
  The message says what is true: the checks that keep a session inside its sandbox have not
  been proven for this build, CodeConnect has to be re-grounded first, and downgrading
  Codex is not being asked for.

Two Codex surfaces are grounded in the tree today: 0.147, which the live gates were proven
on, and 0.153. The version is recorded in the launch evidence, not used as the gate — the
build measured at this commit is `codex-cli 0.153.4`, and the launcher's own test suite
confirms its guarded surface is admitted (`cargo test -p codeconnect --bin codeconnect`,
486 tests, all passing).

### The freeze on the `codex` binary

macOS cannot run a program by file descriptor, so between hashing the `codex` binary and
running it there is a window where an installer could swap the file. The launcher closes
that window by setting the immutable flag (`uchg`) on the binary for the length of the
launch, and clearing it once the session is past `exec`. It is a guard against a benign
update landing mid-launch, not against a hostile process.

The flag can be left standing in one case: the launcher is `SIGKILL`ed inside that window.
Codex still **runs** with the flag on — but it cannot be **updated**, and `npm` or an
installer will fail with `Operation not permitted`.

Two things clear it without you:

1. The next `codeconnect codex` clears leftover freezes as its very first act, before it
   takes one of its own. It prints a line per repair, prefixed `codeconnect:`.
2. The daemon sweeps once at startup and then every five minutes, and logs what it did
   under `codex recovery:`.

If you ever need to clear one by hand:

```sh
chflags nouchg /path/to/codex
```

## Quota

A Codex session runs as **you**. It uses your own `CODEX_HOME` — `~/.codex` unless you set
that variable — which is where your Codex sign-in lives.

So: **turns your phone starts are your turns.** Saying something from the phone spends the
same account allowance as typing it at the Mac. There is no CodeConnect account, no proxy
key and no separate meter.

CodeConnect itself never starts a model turn. Everything it does around a session — the
version probe, the schema and argv reads, the guarded-surface gate, the hash and freeze,
the recovery sweeps — is local work that opens no session and contacts no account.

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

When a Stop or a message is refused, the phone shows the Mac's own sentence. There are 70
of them and they fall into four kinds. Which kind it is tells you whether trying again is
worth anything:

| Kind | How many | What it means | Try again? |
|---|---|---|---|
| Link state | 20 | A fact about the Mac's control link right now — reconnecting, not yet watching the thread, no link at all. | Yes, shortly. |
| This Mac's own store | 5 | A local lookup or record failed **before** anything was sent. Nothing reached Codex. | Yes. Then check the Mac. |
| Settled | 43 | The ask was wrong, the id is spent, or the outcome is already recorded. | No. |
| Wire code | 2 | Codex or the broker refused the write, and the sentence carries their numeric code — never their message. | No. Do it at the Mac. |

Every sentence is written in one place in the daemon and emitted as
[`fixtures/codex/refusal-sentences.json`](../fixtures/codex/refusal-sentences.json), which
a build gate compares byte for byte. If you want the exact wording of all 70, read that
file.

Two words in those sentences are worth knowing: **rejected** means nothing was sent, and
**indeterminate** means it was sent and nobody saw the result. 53 of the 70 are the first,
17 are the second.

### Lines in the daemon log

The daemon's log lives under `~/.codeconnect/logs`. Four lines start with `codex recovery:`
and all four are about leftover freezes on the `codex` binary:

| Line | What it means |
|---|---|
| `codex recovery: <the pass's own account>` | One line copied from the repair pass, e.g. that it cleared a freeze a dead launch left behind. Informational. |
| `codex recovery: … the next pass asks again` | This repair pass did not finish. A freeze may still be sitting on the `codex` binary; the daemon retries in five minutes. |
| `codex recovery: the pass is completing again` | An earlier complaint has cleared. Healthy. |
| `codex recovery: CODECONNECT_LAUNCHER_BIN names …, which is not a file` | You pointed that variable at a path that does not exist. The daemon ignored it. |

If a pass keeps failing and `codex` will not update, clear the flag by hand — see
[the freeze section](#the-freeze-on-the-codex-binary).

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
| The launch refuses and names a flag | CodeConnect owns that setting. See [what is refused](#what-is-refused-and-why). |
| The launch refuses naming things that "moved" | Codex updated past what CodeConnect was grounded on. Update CodeConnect. |
| `npm`/installer cannot update `codex` — `Operation not permitted` | A freeze was left standing. Run `codeconnect codex` once, or `chflags nouchg /path/to/codex`. |
| No Stop button on a Codex session | Daemon below 1.17, or the link is not `subscribed`. |
| No composer on a Codex session | Daemon below 1.18. |
| Stop refused right after the Mac reconnected | Limit 1 above — the turn is doing tool work. Stop it at the Mac. |

## See also

* [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — how the daemon, launcher and phone fit together
* [`mac/README.md`](../mac/README.md) — the Mac side in detail: pairing, TLS, the LaunchAgent
* [`ios/README.md`](../ios/README.md#codex-sessions) — the phone side: the card, the two controls and the one rule that gates them
* [`fixtures/README.md`](../fixtures/README.md) — every recorded Codex measurement and what it proves
