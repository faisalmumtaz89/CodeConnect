# Changelog

User-facing changes, newest first. Mac releases are cut per
[`RELEASING.md`](RELEASING.md); the iPhone app ships on its own App Store track.

## A phone answer goes only to the call it was for

An approval's Allow, Deny or "always allow" from the phone could reach a different Claude
prompt than the card's. Claude queues background agents' prompts at the Mac, and CodeConnect
typed the answer into whichever one was on screen, so a tap could run, deny or answer
another agent's command. Now every answer goes back only through the hook Claude holds for
that card's own call, and nothing is ever typed into a permission prompt.

- A main-conversation approval waits for the phone and the Mac together; whichever answers
  first wins. A background agent's waits for the phone only while you are away from the Mac,
  as its questions do, and appears at the Mac once you are back.
- Deny stops the main conversation's turn, as Escape does; a background agent's call is denied
  and the agent carries on. Deny with a reason sends your reason to the agent that asked,
  instead of typing it into the main conversation.
- "Allow always" is chosen at the Mac, where Claude's own dialog says exactly what it saves.
  The phone allows once; it no longer offers a standing permission, because the one it could
  send can differ from what the Mac's row would save.
- A card the phone cannot answer that way is shown read-only and says to answer at the Mac:
  one from a session started before this update, one Claude is showing at the Mac, or one whose
  wait has ended. An approval answered at the Mac closes on the phone once Claude has run or
  rejected it, and every open card closes when its session ends.
- A phone answer is recorded as sent to Claude, not as confirmed: Claude takes the first
  answer, the Mac's or the phone's, and may not use the phone's. Every phone shows "Sent to
  Claude from a phone", and the card closes at once.
- The phone (1.1.5) answers Claude approvals only from a Mac with this update (CodeConnect
  0.12.0); with an older Mac they are read-only. A phone that has not been updated can still
  allow and deny; anything else it sends is refused rather than typed.

## A cleaner session timeline

- A reply no longer sits under two empty lines, and the empty fragments Claude writes
  between a tool call and its reply no longer take up space on the timeline.
- "Claude is waiting for your input" is a quiet note rather than an amber warning, and it
  disappears once you send your next message; a finished session no longer turns back to
  running when it appears. The rows saying the session started and the
  link attached are gone from the top of the timeline; a session that starts again later,
  and a link that drops and comes back, are still shown. A later start says what it was:
  "Session started", "Session resumed" or "Conversation compacted" (a cleared conversation
  already says "Conversation cleared.", also after `/clear` with a name, `/new` or `/reset`).
- The "waiting 4s" clock on an approval or question is the same small size as every other
  age, with only the number in fixed-width type.
- At the largest text sizes, rows that stack their parts no longer carry an empty gap where
  a horizontal spacer used to be.
- A question Claude asks is one entry on the timeline: the tool row that repeated it above its
  card, with a meaningless "0ms", is gone.
- That entry reads as a question: "Question" and its words in ordinary type, no MEDIUM badge,
  **Answer** while you can answer it from the phone and **View** when only the Mac can. Once
  answered it says what was chosen ("Answered: Blue") and how long ago, in the same words the
  question itself uses ("Answered at the Mac", "Closed").
- The session's two clocks say what they count: "last event 53s ago" beside the title ("53s
  ago" at the largest text sizes), and "link 0s" in the toolbar pill.
- The session's folder is written in readable grey instead of dimmed. It is still the Mac's
  full path, cut at its start so the end that names the project shows.
- The message box on a Claude session says "Ask Claude to do anything", as a Codex session's
  names Codex.
- The diff button reads "± 0" when the session's folder has no changes at all, instead of
  the same bare "±" it shows before it has looked. New files that are not yet tracked are
  changes, so they never read as 0.
- The bar holding Submit, Allow or Deny now fills the sheet to its bottom edge: there is no
  darker band under it, and at the largest text sizes the question no longer shows through
  beneath it.
- A project's name on a decision or question is set in ordinary type, as it is everywhere else;
  only its folder path stays in fixed-width type.
- A question's sheet has one heading, "Question" (or "4 questions"), instead of "Decision"
  above "Claude has a question"; it names the run as the session does, says up front when an
  answer will take Face ID, and once answered states it once rather than three times. No empty
  band sits above a question that has no topic chip.
- A question you can only read (asked at the Mac, closed) shows its options without empty
  radio buttons, and no rule is drawn under the last option.
- Before you choose, the question says "Choose an answer to submit." quietly under its buttons,
  not as an amber warning between Submit and Decline, and the bar no longer jumps when you
  choose. A reason that stops both buttons is said once.
- While an answer is being sent, its choices can no longer be changed, so what the question shows
  is what was sent.
- Every dot and glyph in the left margin (the session's status dot, a card's dot, tool and notice
  icons, the bar beside your messages) is centred on one line, the one the fleet's dots use.
- A tool's command starts the same distance after its name on a timeline row as on an approval
  card or a fleet row.
- Your own messages line up: the words start under "YOU", with the bubble around them.
- On a question, the label over the Other and Notes fields starts at the same edge as the question
  and the field, not indented.
- The buttons pinned at the bottom of a decision, question or command sheet line up with the content
  above them.
- An approval's ending says only what is known: "Allowed from a phone" (any of your paired phones,
  not necessarily this one), "Allowed at the Mac", and "Closed at the Mac" when the prompt left the
  Mac without anyone being seen to answer it. It used to say "from this app", "at the keyboard",
  and once "at the keyboard at the keyboard". A Codex question answered from the phone says
  "Answered from a phone" for the same reason.

## Answer Claude's questions from the phone

When Claude asks you to choose (its `AskUserQuestion`), the phone now shows the question
itself instead of an approval with Allow and Deny: every question, its options and their
descriptions, multiple choice where Claude allows it, "Other" in your own words wherever
the Mac offers it, and the preview with a notes field where Claude offers one. Submit sends your answer to Claude as
if you had chosen at the Mac; Decline does what Escape does (stops the turn, or for a background agent's question
denies it and lets the agent carry on). The question stays on the Mac at the same time,
and whichever answers first wins. Nothing is typed into the Mac's dialog. A decline is
marked unconfirmed, as Claude does not say when it takes one.

Before, Allow on such a question typed keys that chose options you never picked: on a
question with several parts it picked the first answer of the first question, ticked a box
in the second, and left Claude waiting while the phone said "Approved". A phone that has
not been updated still shows the old card: its Deny declines a question waiting on the
phone, and nothing else it sends is typed into a question.

A question asked by a background agent is drawn by Claude only after CodeConnect lets it
go, so it waits for the phone only while you are away from the Mac: no keyboard or mouse
use anywhere on it in the last 10 seconds. Touch the keyboard or mouse and the question
appears at the Mac a moment later (0.08 to 0.15 s from the input, measured). Nothing
holds back what you type: keys typed as it appears are Claude's to handle, as when a
question pops up while you type, so a key arriving as the dialog first draws is dropped
and one a moment later answers it. If the daemon stops while such a question waits,
Claude asks it at the Mac.

A question whose text the Mac's dialog would show differently from what Claude wrote —
invisible characters, more than eight accents or joiners stacked on one letter, emoji with
a style selector — is answered at the Mac only; the phone says so. A preview the Mac does not
show, being longer than 2,000 UTF-16 units, is not checked, and the answer leaves it out,
as the Mac's does. Questions in scripts written
with combining marks, such as Hindi, Thai or Hebrew with points, and joined emoji are
answered from the phone. The phone's notification for a question says "Asking you a
question"; a push relay that does not know questions yet rings it as an approval.

A question now waits on the phone for as long as Claude waits for it. Sessions started
before this update keep the old two-minute window, after which the card says to answer at
the Mac.

## A second daemon no longer cuts the running one off

Starting `ccd` by hand while the daemon was already running used to delete the running
daemon's socket as the second one gave up. The daemon kept running but nothing could
reach it: sessions lost their link and the phone saw no new sessions until the daemon was
restarted. A second `ccd` now stops before it opens the database or the socket, saying
`another ccd is already running`, and the running daemon carries on as before.
Restarting after a crash works as it did.

## Ctrl+Z stops Claude and Codex as it does run directly

Ctrl+Z in `codeconnect claude`, `codeconnect codex` or `codeconnect attach` now stops
the agent: your shell prints its own `Stopped` line, and `fg` brings the agent back where
it was, with what you had typed. Claude used to print its "suspended" message and then
wait for ever, and Codex ignored the key. Codex stops whole, as it does run directly, so
a running turn pauses too, and a message or Stop from the phone meanwhile is refused at
once. Closing the tab, `exit` or `kill %1` while the agent is stopped ends the session,
as it ends a stopped job run directly. The phone's Terminal tab never keeps a stopped
agent held, since nobody can `fg` from it.

A message or Stop sent from the phone while Codex is stopped is refused with a sentence
saying Codex is paused at the Mac and continues after `fg` there, and the same message or
Stop sent again after `fg` goes through once.

A message or Stop from the phone that was refused before it reached Codex can now be
sent again from the phone: for example while the session was busy or switching thread,
or when the turn ended as you tapped. It used to be refused again for good, with "an
earlier attempt to say this was refused" for a message and "an earlier request to stop
that turn was refused" for a Stop. An answer from Codex that does not show whether it
took the message or Stop, such as an internal error inside Codex, is now reported as not
known rather than as a refusal, and is still never sent again.

## A long session no longer freezes while scrolling — iPhone app

Scrolling back down past a long expanded message or a long failed command could freeze
the session screen for good. The timeline is now a list that measures only the rows on
screen, and it no longer freezes; a 3000-event session opens as fast as before. The
newest message now also stays in view when the keyboard opens, the phone rotates or the
last row grows, while a reader who scrolled up, opened a message or output, or tapped
the status bar is left where they chose to be. Long-pressing a message offers Copy only.

## A Claude session's end reaches the phone within seconds

When the last session on CodeConnect's tmux server ended, or the server was stopped, a
Claude session's supervisor could not tell the session was gone: it kept running and the
phone showed the session live until the daemon's own sweep noticed. It now pins the
server its session runs on and reports the end within a few seconds.

## Ctrl+C while a session starts

A Ctrl+C pressed while `codeconnect codex` or `codeconnect claude` is still starting is
held until the terminal is attached and then reaches the agent as if typed there,
instead of ending `codeconnect` and leaving the session running unseen, or printing an
error. While it starts, a stuck launch can no longer be interrupted; it gives up on its
own after at most about a minute. Quitting Codex just after it has started a
conversation, before the launch finished, ends within about a second instead of waiting
a minute for `launch deadline expired`, and a launch whose Codex host exits before it is
ready (Ctrl+\ at start-up, a crash) fails in about a second, saying so.

## Codex starts faster

On Apple silicon, `codeconnect codex` checks the codex binary's identity with the CPU's
SHA-256 instructions, so each of the four checks in a launch takes about 0.1 s instead of
about 1 s.

`codeconnect codex` no longer locks the codex binary while it launches. Changing the
lock made macOS check codex over again before it next ran, which slowed launches and
sometimes your next plain `codex`, and a launch killed at the wrong moment left codex
locked so it could not update. The identity checks stay. Together, a hosted session
reaches its composer in about a second, down from about 7.5 s (about 3.5 s of that from
the faster checks). If an earlier version left
codex locked (`npm` or an installer fails with `Operation not permitted`), unlock it with:

```sh
chflags nouchg <the path named in the error>
```

The standalone installer keeps earlier releases, and the locked one may be one of them;
this unlocks every release it keeps:

```sh
find ~/.codex/packages/standalone -flags +uchg -exec chflags nouchg {} +
```

## Hosted sessions look like the agent run directly

`codeconnect claude`, `codeconnect codex` and `codeconnect attach` now show the session
through a tmux control-mode client built into `codeconnect`, instead of an ordinary tmux
client on the alternate screen.

- **Drawn as run directly.** Claude Code and Codex draw exactly as they do run
  directly — inline, not forced full screen — so terminals such as Warp frame them
  the same way. Your terminal's own scrollback, selection and mouse wheel work as
  usual.
- **Re-attaching repaints the session.** `codeconnect attach` paints the history and
  screen with their colours, the cursor and the title, then streams the session live.
- **Your environment.** Claude and Codex get the environment of the shell you launched
  them from — API keys, a virtualenv or direnv `PATH` — not the tmux server's. They do
  not see `TMUX`/`TMUX_PANE`, so Claude keeps true colour.
- **Codex exits the way it does run directly**, with its token usage and `codex resume
  <id>` or `Session ID: <id>`, not a reconnect command for a socket that is gone.
- Known limits: kitty keyboard mode is not detected, and a re-attached tab does not get
  back keyboard modes tmux does not track. A terminal that does not report its colours
  is answered black by tmux, so Codex draws its input band for a black background.

## Codex runs as it does natively

`codeconnect codex` now hosts whichever Codex you installed, launched the way you would
launch it yourself. See [`docs/codex.md`](docs/codex.md).

- **No version or schema gate.** The launch no longer compares Codex against recorded
  0.147/0.153 schemas, so a new Codex release is hosted the day it ships. The phone
  stays protected by the broker's fixed phone shapes, which refuse anything a release
  changes.
- **Your flags reach Codex.** The sandbox, the approval policy, profiles, `-c`
  overrides and feature flags are passed through instead of refused. CodeConnect no
  longer starts the terminal UI read-only or turns Codex's permissions tool off: Codex
  picks both from your config and the project's trust, exactly as when you run `codex`.
  A phone's turn runs under whatever the keyboard has set.
- **`--cd <dir>` works.** The directory becomes the session's folder — where Codex runs
  and where the session is listed. A relative path is read against the directory you
  ran the command from; a path that is not a directory stops the launch.
- **Flags work wherever you put them, including new ones.** A flag after the prompt
  still reaches Codex as a flag, and a flag CodeConnect does not know is passed on
  unchanged (write a value as `--flag=value`).
- **`--help` and `--version` are Codex's.** They run `codex` in your terminal and start
  no session.
- **`resume` and `fork` work.** `codeconnect codex resume --last`, `codeconnect codex
  resume <id>` and `codeconnect codex fork …` are hosted like a new session, and the
  phone follows the thread you picked. Quitting the picker with Ctrl+C ends quietly, as
  in Codex, instead of being reported as a failed launch.
- Still refused: `--remote`/`--remote-auth-token-env` (the terminal UI must talk
  through the broker) and every other subcommand.

## Deep links land on the decision — iPhone app

A tapped notification resolves to `codeconnect://session/<id>?request=<rid>`, whose
whole promise is that the reader lands on the decision, not on the timeline above
it. It landed on the timeline: the request id was spent the moment the session
opened, and the card is filed a beat later, when the history has been read — on
every launch, not only a slow one. The app now keeps the requested decision until
its card exists and opens it then, at reading size and at the largest accessibility
size alike. Proven in the render harness with the approval arriving both before and
after the link, and the decision-sheet scenario now reaches its sheet through the
link alone.

## Codex sessions — protocol minor 20

OpenAI's Codex CLI runs under CodeConnect and is drivable from the phone. Codex has
no hooks, so it is hosted differently from Claude Code — and the difference is the
whole of this entry. See [`docs/codex.md`](docs/codex.md).

- **`codeconnect codex` hosts a Codex session.** It runs the real `codex` binary in
  the same private tmux server, passing your arguments through, and puts a **broker**
  in front of Codex's own JSON-RPC app server. Arguments CodeConnect owns for the
  session — the working directory, the sandbox, the transport, a named profile, the
  approval controls, and every `codex` subcommand but the interactive TUI — are
  refused before anything is created, and the refusal names the setting and who owns
  it.
- **Approvals, with Codex's own options.** When Codex asks to run a command or write
  a file, the phone gets a card built from the request Codex actually sent: the
  command, the working directory and Codex's own reason, or the paths and the diff.
  The buttons are the option list on the wire for that request, never a guess — a
  card offering an option this build has no words for is not shown at all. The
  "don't ask again" button carries the exact token list Codex offered, read from the
  request rather than re-derived from the command text.
- **Stop a running turn.** Offered only when the daemon supports it, the session is
  a Codex session, and the Mac's control link says so. The outcome is named rather
  than assumed: stopped, already asked, refused with a sentence, or *not known* —
  and a stop is never retried for you.
- **Say something.** Up to 8,192 bytes from the phone. Idle, the words **start** a
  turn; busy, they **join** the turn already running — one turn started it, one turn
  ends it, and the reply names that same turn. `started` and `steered` are different
  news and the app renders them differently.
- **Four limits, measured rather than guessed.** Stop goes quiet after a reconnect
  on a turn that is running a command or editing a file — the reconnect cannot
  safely name the running turn, so Stop is refused until it ends or a new one
  starts. An approval answered at the Mac keyboard does not say *which* button was
  pressed, because Codex's resolution message carries only which request was
  answered. Stop ends the turn, not a shell the agent had already launched (measured
  on 0.153.4: the command finished about forty-five seconds later). And two sessions
  minted in the same millisecond by two processes can sort the wrong way round —
  about one in 2^80 per millisecond, closable only by a shared minting service that
  is not built.
- **The security boundary.** The broker listens on two separate sockets — one for
  the keyboard, one for the daemon — and which socket a message arrived on decides
  what it may do. It refuses by default: any method, and any *parameter*, not
  explicitly admitted for that socket is refused with a numeric code and never
  reaches Codex; four code-execution methods are refused on every socket, always.
  The phone's socket is the narrower one — it may not create or fork a thread, may
  not read Codex's account and model settings, and its turn message is exactly
  fourteen fields against the keyboard's twenty-four, so it cannot name a model, an
  effort, a service tier or a workspace. A refusal reaches the phone as a numeric
  code and a fixed sentence, never the upstream message.
- **No Codex version is pinned; the guarded surface is.** At every launch, before
  anything is created, CodeConnect checks the part of Codex it actually guards — the
  app-server methods the broker filters, and Codex's root subcommand and flag list —
  against what it was grounded on. Unchanged, the build is admitted whatever it
  calls itself. Moved, the launch is refused and the refusal names each thing that
  moved.
- **The `codex` binary is frozen for the length of a launch.** macOS cannot run a
  program by file descriptor, so the launcher sets the immutable flag on `codex`
  between hashing it and running it, and clears it once the session is past `exec`.
  A `SIGKILL` inside that window can leave the flag standing — which stops `npm` or
  an installer updating `codex` with `Operation not permitted` — so it is cleared
  twice over: the next `codeconnect codex` clears leftovers as its first act, and the
  daemon sweeps at startup and every five minutes, logging what it did.
- **Protocol.** `PROTOCOL_MINOR` is now 20 (major stays 1), across six additive
  steps. 15 opens the agent seam: `AgentKind`, `Capabilities.supported_agents`,
  `SessionSummary.agent`, `SessionSummary.codex_thread_id`, a separate
  `CodexResolution` envelope so Claude's `AnswerOutcome` stays byte-identical, an
  opaque `AnswerDecision::OptionId`, the composite wire-id codec, and the
  `Interrupt` operation — defined and refused. 16 lets the phone answer a Codex
  approval by writing the app server's own response. 17 honours `Interrupt`, makes
  every `InterruptResult` status reachable, and adds the `codex_interrupt`
  capability. 18 adds the `Compose` message, `ComposeResult`, and `codex_compose` —
  a stronger flag than its sibling, because a daemon below 18 decodes nothing and
  answers nothing. 19 adds `request_id` inside the `approval_resolved` payload,
  `turn_id` on the approval card's event envelope, and `SessionSummary.codex_link`.
  20 adds that field's fifth word, `bound_not_started`. All additive — an older app
  or daemon keeps working, and a client that does not know a `codex_link` word
  treats it as not actuatable, which loses the affordance and never correctness.
- **App.** Codex support went to TestFlight as 1.1.0 (72). That build is a live
  consumer of minor 19 and knows exactly four `codex_link` words, which is why the
  fifth cost a minor of its own rather than being folded into 19.

## Push gateway — protocol minor 14

Relay-backed push notifications, so an iPhone can be rung by a Mac that holds no
Apple push key of its own — the case for every App Store customer.

- **Relay-backed notifications are the default.** A daemon with no Apple push key
  now sends through CodeConnect's push relay, which builds a generic notification
  titled `CodeConnect` and forwards it to Apple. The relay receives only the APNs
  token and environment, an opaque token-bound credential, one of four fixed
  event kinds, a blocked-run count, and a test marker when applicable — never
  project names, session identifiers, commands, file paths, diffs, or
  conversation content. See the [privacy policy](site/privacy.md).
- **App Attest enrollment.** The iPhone proves it is a genuine copy of the app
  with Apple's App Attest to obtain the relay credential, which lives — with the
  App Attest key ID — in `ThisDeviceOnly` Keychain storage. App Attest runs for
  enrollment or re-enrollment after pairing — normally once per install, and
  again after a reset or credential recovery — never per push.
- **Direct-key override, unchanged.** A Mac configured with its own APNs key
  (`apns_key_path`, `apns_key_id`, `apns_team_id`, `apns_topic`) talks straight
  to Apple and keeps the project-labelled payload. This override always takes
  precedence over the relay.
- **Off.** `push_enabled: false` disables push entirely.
- **Protocol.** `PROTOCOL_MINOR` is now 14 (major stays 1): the `push_relay`
  capability, `register_push.relay_credential`, the `credential_invalid` test
  result, and `hello_ack.push_environment`. All additive — an older app or daemon
  keeps working, and a relay daemon advertises the legacy `push` capability as
  false on purpose so an older app never registers a credential-less token.
