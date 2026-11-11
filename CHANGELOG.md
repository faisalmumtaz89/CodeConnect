# Changelog

User-facing changes, newest first. Mac releases are cut per
[`RELEASING.md`](RELEASING.md); the iPhone app ships on its own App Store track.

## Choose any folder for a new agent on this Mac

- Start an agent listed only folders an agent had already run in, so on a new Mac there was nothing
  to pick and the first agent had to be started from Terminal. On this Mac, "Choose a folder…" beside
  Folder now opens the standard folder window. Other computers keep their recent folders.

## A Codex run that ends closes its waiting request

- When a Codex run ended while one of its requests waited for an answer, the request stayed open:
  the run kept showing as blocked and was counted among those waiting, even after restarts. Ending
  a run now closes its waiting requests, as it already did for Claude Code, and a request left
  open this way is closed the next time CodeConnect starts.

## No developer-tools dialog on a Mac without them

- On a Mac without Apple's command line developer tools, starting a session or opening New agent
  showed macOS's dialog offering to install them, and it came back after Cancel. It no longer
  appears: without git, a run shows no branch or changes, as in a folder that isn't a repository.

## A request waiting at the computer stays open

- A permission request that only the computer could answer closed after 15 minutes as "Closed by
  timeout - nobody answered", and the run showed as running while the agent still waited. This
  happened after the daemon restarted, as an update does, or when the request was made while you
  were at the Mac. It now stays open for as long as the request is on the computer's screen.

## Sending says nothing when it works

- A message you send no longer puts "Typed into the session." above the message box, on the iPhone
  or the Mac, and a message to Codex no longer says it "started a new turn" or "joined the running
  turn". The message appearing in the conversation is what says it went. The iPhone's Send button
  no longer flashes a check mark either. Anything that went wrong is still said there: a message
  that was refused, one that isn't confirmed, or why sending can't work right now.

## A Mac app

- CodeConnect now has a Mac app, `CodeConnect-0.18.0.dmg` on the release page, for macOS 26. Opened
  the first time, it installs CodeConnect itself (`ccd`, `codeconnect`, `cc-hook` and CodeConnect's
  own tmux, into `~/.codeconnect/bin`), starts the background service, which then runs at every
  login, and pairs with it without a code. It keeps them up to date from then on: `codeconnect
  update` leaves an install the app made to the app, and `codeconnect claude` and `codeconnect codex`
  no longer ask GitHub for a newer release there.
- Its window lists the agents running on your computers, grouped by computer or by project, each
  with its state, what it is doing, its agent and how long ago, and every "… ago" keeps counting. A
  group says how many decisions wait in it, and folds away.
- Clicking a computer or a project in the sidebar narrows the list to its agents, with its name in a
  chip beside the count; clicking it again, or the chip, shows them all. Rows and buttons light up
  under the pointer.
- Clicking an agent opens it beside the list: its computer, folder and branch, its state, agent and
  model, the lines it has changed, and its conversation, newest at the bottom. Scrolling to the top
  loads earlier history. Clicking the agent again, or Esc, closes it.
- A reply box under the conversation sends the agent a message, which then shows in the
  conversation; the line above the box says something only when a message didn't simply go through,
  or can't be sent right now. For Claude Code, the button beside Send changes the model or the
  effort, or compacts the context, with instructions if you like; /clear asks first. What you are
  writing stays in the box while you look at another tab, and Esc leaves the box first.
- A request waiting on you is answered where you write to the agent. What it asks sits at the foot
  of the conversation: the command it asks to run, how risky that is and why, a Claude request's
  full tool input a click away, and the edit a Codex request carries. Its answers take the reply
  box's place: Deny and Allow, with a field whose words a Deny carries as its reason; Codex's own
  options; or, for Claude's questions, Decline and Submit. Allow waits until the command has been on
  screen. The Mac asks for no Touch ID or password; the iPhone keeps asking for Face ID or the
  passcode at high risk.
- The oldest request takes the box, saying how many more wait, and each keeps a "Waiting on you
  below" line in the conversation; once answered, it is back there, saying what became of the
  answer. ⌘↩ allows and ⌘D denies; with a reason typed, Return denies with it. Codex's options
  answer to ⌘1 onwards, and each key shows on its button. A request that arrives while you are
  typing a reply waits above the box, with buttons but no keys, until you leave the field.
- A Codex run can be stopped from the top of its pane while its conversation shows (Stop this
  turn), and the box under the conversation then says what became of the stop.
- A Changes tab, between Conversation and Terminal, shows what the run has changed since its last
  commit, as `git diff HEAD` reads it on its computer: each file with what happened to it and its
  lines added and removed, and a total at the top. It reads them when you first open it and again
  on Check again, says how long ago it checked, and then counts the files on the tab. When there is
  nothing to show, or the changes can't be read, it says why.
- A Terminal tab, the last of the three, opens a Claude Code run's live terminal when you click
  Open terminal. While it is shown, what you type goes to the agent, Esc included. When it can't open,
  the tab says why, with the fix when there is one.
- File › New Agent… (⌘N) starts Claude Code or Codex on one of your computers: pick the computer, a
  folder, and a model from those the agents offer there (choosing the model chooses the agent), and
  add a first message if you like.
- File › Add Computer… (⇧⌘N) pairs another computer, and File › Add iPhone… shows the QR code that
  pairs an iPhone, with its address and code written beneath it.
- CodeConnect › Settings… lists the phones and computers paired with this Mac. Block keeps one out,
  and Allow again lets a blocked one back in; each asks first.
- CodeConnect › Set Up CodeConnect… shows what CodeConnect needs on this Mac: the background service,
  tmux, Claude Code, Codex, and `codeconnect` in a new Terminal window, which it can add to
  `~/.zprofile`. It opens by itself only when the background service or tmux isn't working, says
  what failed in one sentence, and offers Try Again.
- The app updates itself: it checks once a day, and CodeConnect › Check for Updates… checks now.
- Every control in the app has an accessibility name and identifier, so automation and assistive
  tools can find, press and read it.

## Your own Tailscale devices join without a code

- A phone or computer signed in to the same Tailscale account as this computer, with neither of them
  tagged, is paired without a code when it connects: the rule that already decided which computers
  may send each other messages. Once one of your computers is paired, the Mac app adds your others
  this way. A device shared into your tailnet from another account, a tagged device, or a phone off
  Tailscale still needs a code.
- Blocking a device (Block in the Mac app, or `codeconnect revoke`) keeps out every device signed in
  from the same phone or computer, at once and after a restart, and this computer stops exchanging
  agents' messages with it in either direction.
- Allow again (or `codeconnect unblock <device>`) lets it back in with the pairing it already holds,
  as the same device, with no new code.

## `fg` continues an agent that was stopped twice

- On a Mac, an agent stopped with Ctrl+Z that was stopped once more while it waited, as a program
  reading the terminal can be, could stay stopped after `fg`. It now continues.

## The end of a long session is no longer lost

- A session that ended while the daemon was stopped, or before the daemon had read its transcript
  through, was read only to its first 4 MiB, and the rest of the conversation never reached the
  phone. It is now read to the end.

## Reinstalling the daemon over a running one leaves it running

- `codeconnect daemon install` over a daemon that was already running could leave none: the old
  one stopped, and launchd turned the new one away because it was still removing the old, so
  nothing ran again until the next login. The Mac app's first run and its updates install this
  way. Installing now waits until launchd has let go of the old daemon before it starts the new
  one.

## A notification always has something to answer

- Your phone now rings only while an approval or a question is waiting that you can answer from
  it, Claude's or Codex's, and the notification is about that decision. Claude going idle or
  finishing a turn, Codex finishing a turn, and a permission prompt with nothing to approve on the
  phone no longer ring on their own, so tapping a notification opens a decision to answer. While
  one is waiting, those moments still remind you of it.

## A model set with /model reaches the phone by its name

- Claude Code 2.1.319 writes the model `/model` set in backticks (`` `Opus 5.5` ``). The computer
  now reads the name out of them, as the phone does, so the model it sends the phone for a session
  is the plain name.

## A Codex start that fails says why

- When a Codex session failed while it was starting, `codeconnect codex` could say "tmux reading the
  codex pane failed … no server running" instead of why it failed. It now says why it failed.

## A start picks the model

- A computer now tells the Mac app which models Claude Code and Codex offer on it, in the agents'
  own words (Claude Code's default first, with the model it is now), and whether each is signed in,
  and starts a session with the model picked. It asks each agent itself, from the home folder, with
  no hooks, MCP servers or session file; while an agent is signed in it is asked once per version.
- `codeconnect codex` with no terminal to show a session in — a script, a pipe, `ssh` without
  `-t` — runs Codex itself, as `codeconnect claude` runs Claude Code, so `codeconnect codex login
  status` or `codeconnect codex exec …` in a script does what `codex` does. It used to refuse
  everything there but `--help` and `--version`.

## An ended session keeps only its own messages

- Claude Code started with `--resume` writes the conversation on into the file of the session it
  resumes. Each time the daemon started again, after an update or a restart, it read that file once
  more for the ended session and showed the new session's messages under it as well, more than a
  thousand at a time for a long conversation. An ended session's file is now read once, when it
  ends, and then let go; updating lets go of the files of sessions that had already ended.

## A session restarted from a copied command shows once

- A session restarted with a command copied from an earlier one, with that session's `--settings` and
  `--mcp-config` files in it, showed twice on the phone: the ended session came back every minute
  with the new one's activity, and the new one said "Nothing recorded yet". Its messages to other
  agents went out as the ended session too. A session's hooks and its message tool now name only the
  session CodeConnect started, so a copied command, or Claude Code resumed outside CodeConnect with
  one, no longer brings an ended session back.

## Many more sessions on one computer

- One computer was limited to 128 connections to CodeConnect, and an idle Claude session holds two
  (its supervisor and its mail waiter), so about 64 Claude sessions. Past that, a new session never
  appeared in the fleet, and a start from the phone failed after 80 seconds. The limit is now 4,096
  connections, about two thousand sessions. On a Mac the system's 511 terminals come first: about
  500 sessions started from the phone, or about 250 typed in a terminal, which takes two.
- CodeConnect and its daemon raise their open-file limit as they start, from macOS's 256 to 10,240,
  so the daemon and the tmux server holding the sessions both have room. A tmux server already
  running keeps its old limit until it next starts.

## Start a session from the phone

- From the iPhone, start Claude Code or Codex on a paired computer, in a folder a session has run
  in, with an optional first message. The computer starts it as `codeconnect claude` or `codeconnect
  codex` typed in that folder would, through your login shell. The first message is handed over as
  the agent's own first prompt.
- The folder list is the computer's own, most recent first: every folder a session has started in,
  including sessions you have since removed. Each folder shows its git branch, a folder that no
  longer exists is left out, and an agent the computer cannot start is shown as not installed.
- The phone hears "started" once the session is in the fleet, so it opens the session straight away.
  If the start fails, it says why: the agent is not installed, the folder is gone, or the launcher's
  own reason. A start the phone sends again after a reconnect mid-start starts one session.
- A session started from the phone sees the environment a terminal opened in your session would:
  PATH, your shell's startup files, and a UTF-8 encoding when nothing else names one.
- Starting is offered only to a paired iPhone on a private connection, the same as the terminal. It
  needs both CodeConnect 0.17.0 (protocol minor 28) and the matching iPhone app; with an older Mac
  the app does not offer it.

## Start in a branch, a worktree, or any folder

- The computer can now start a session in another branch without switching the folder you work in.
  It can use a worktree git already lists, make a worktree for a local branch, or make a new one.
  New ones go where Claude Code puts its own, `.claude/worktrees/<name>`. A new Claude worktree is
  made by Claude itself; a new Codex one is made in the same place, on a branch named as you typed
  it. CodeConnect never removes a worktree it made.
- The computer can also list any folder's folders for the phone to browse, starting at your home
  folder, and make a new folder. A start can then begin in any folder, not only one a session has
  run in.
- Branches, worktrees, folder browsing and new folders need CodeConnect 0.17.0 (protocol minors 29
  and 30), and the iPhone app offers them as it adds them.
- On macOS, Desktop, Documents and Downloads are asked about while you are at the Mac, so a phone
  never waits on a permission dialog nobody can see: `codeconnect daemon folders` asks, as does your
  first `codeconnect claude` or `codeconnect codex` typed at the Mac itself (not over SSH). A folder
  macOS keeps from CodeConnect says where to change that: System Settings > Privacy & Security >
  Files and Folders > ccd.

## A session starts in its folder, whatever the folder is called

- A folder with `#` in its name, such as `C#` or `a##b`, started Claude Code or Codex in your home
  folder instead. A folder whose name ends in `;` could not be started at all. Both now start in the
  folder as named.
- A Claude session in a worktree, typed as `codeconnect claude --worktree <name>`, was filed under
  the main folder after the daemon restarted (for example after `codeconnect update`) and again once
  it ended, with the main folder's name and changes. It now stays under its worktree.

## Arguments keep a trailing semicolon

- An argument ending in `;` given to `codeconnect claude` or `codeconnect codex`, such as a first
  prompt, now reaches the agent whole; it used to lose the `;`.

## The conversation reads at one size

- On the iPhone, everything in a conversation is now the size of Claude's own reply: a tool call
  such as `Bash`, what you sent, notices like "Claude is waiting for your input", and approvals
  and questions. Tool names used to be larger and your messages smaller.
- A tool call reads as one line of words, its command right after its name, at every text size;
  at the largest sizes the command no longer drops onto a line of its own. Consecutive tool calls
  sit together, and each is still a full-size tap.
- Icons in the conversation are smaller to match, and sit on the middle of their line.

## A cleaner Changes screen on the iPhone

- The screen that shows a session's uncommitted changes is called Changes, and it reads like
  VS Code: whole lines tinted red or green with the changed words stronger, a `+` or `−` beside
  each changed line, quiet separators between hunks, and long lines that wrap under their own
  indent. Folded lines are one quiet "⋯ 3 unchanged lines" row; tap it to show them.
- In tab-indented files, such as Go, the highlighted words and the wrapped lines now line up
  with the code they belong to.
- Press and hold a line to copy it: Copy line copies the whole line exactly as it is in the file,
  tabs included.
- The line above the diff is one short line, such as `+6 −3 · 2 files · 3s ago`.
- A new or deleted file is marked with a plain plus or minus, like the pencil on an edited one.
- At the largest text sizes the folded-lines row stays on one line and a file's chip keeps its
  counts on screen, shortening a long name in the middle.
- The refresh and close buttons sit 8 points apart, in the same pills as the session's header,
  level with the title. Every other sheet's close button uses the same pill.

## A softer message box on the iPhone

- The message box in a session is a little taller on one line, 52 points instead of 44, with a
  lighter fill and a soft light along its top and bottom edges. The send button sits evenly inside
  it. Tapping into the box no longer turns its edge white; the cursor and the keyboard show where
  you are typing. The placeholder, the icons and how the box grows with longer messages are
  unchanged.

## More room for the conversation on the iPhone

- A session's name and folder now sit in the navigation bar, and the block of facts under it is
  gone, so the conversation and the terminal get the space back. In landscape the terminal now
  keeps the bar, a few rows shorter than before, so you are always one tap from the conversation.
- The bar's "…" menu holds the rest. Terminal comes first, and on the terminal the same item
  reads Timeline, so the way back is the same place. Then Changes, which opens the session's
  uncommitted changes, and the link's health, under the name of the computer the session is on.
- Notes that matter only for a while, such as how to pick an ended conversation up at the
  computer, show as a slim strip under the bar only while they apply.
- Long-press the name in the bar to copy the session's full folder path.

## New icons on the iPhone

- The iPhone app draws its icons from Phosphor, a cleaner, rounder set, in place of Apple's SF
  Symbols. Every icon keeps its place and meaning; the status marks keep their filled shapes, so
  a running, finished, failed or waiting session still reads at a glance without colour.
  Phosphor's MIT licence ships with the app.
- Every icon beside words now sits on the middle of their first line, at every text size: a
  timeline notice, a tool call, a banner, a button, a row. Some sat up to 6 points above or below
  it, most visibly beside "Claude is waiting for your input".

## A message sent as the connection drops no longer stays in the box

- When the network between your iPhone and your computer goes quiet, as it does when Tailscale
  disconnects, the app now notices within half a minute and connects again by itself. It used to
  keep waiting on the dead connection for minutes.
- If you had just sent a message when that happened, your computer had usually already typed it,
  but the app never heard back, so the text stayed in the box. Once the app has reconnected, it
  now checks that message with your computer and clears the box. Within a minute of the send, if
  the message never arrived it is typed then, once; nothing is ever typed twice. The note no longer
  says the message couldn't be typed when nobody knows; it says it isn't confirmed.

## The conversation stays still after you send

- After you send a message from the iPhone, the conversation moves once, to your message, and stays
  there. A few seconds after a send it used to jump up to an earlier reply and scroll back down;
  that happened when the "Typed into the session." note under it went away. The note now appears
  and goes in one step.
- When the keyboard opens or closes, or a note appears over the message box, the newest message
  stays just above the box instead of sliding under it for a moment and scrolling back.

## Agents message each other

- A Claude Code or Codex session started through CodeConnect can message any other such session on
  your computers. Each gets a `codeconnect` tool: `list_agents` names the others by their address
  (`<run id>@<computer>`, with their `cc-N@<computer>` label beside it), `send_message` sends one a
  message or answers one, and `read_messages` reads the session's own, with the exact call that
  answers each when your agents act on messages (below).
- A message waits on the receiving computer until that session reads it, for a day at most, and the
  sender reads in its own `read_messages` whether it was read, expired, or never read because the
  session ended. A message holds at most 8 KiB, and one agent may leave another 30 an hour.
- The receiving agent is woken to read its mail with one fixed line, never with anything a message
  says: Claude Code once it is done, after any prompt you queued meanwhile, or at once when idle (for
  up to 12 hours), and after Esc only once your next turn has ended; Codex through its own queue,
  after any running turn, and after Esc likewise. After three woken turns that leave the mail unread,
  wakes stop until your next prompt or new mail.
- The first session you start in a terminal asks once: "Let your agents act on messages from your
  other agents?" With yes, an agent handles each message as a teammate's request, within its own
  permissions; with no, or until you answer, it shows them to you. `codeconnect mail-trust on|off`
  changes it. Nothing is written to Claude Code's or Codex's own instruction files.
- Messages between computers travel only to your own Mac and Linux computers of the same Tailscale
  user, on the daemon's port, and a message to a computer that cannot be reached is offered again
  until it is taken, for a day at most. A session started before this release is not listed and
  takes no messages until it is restarted.

## Names stay whole at the largest text sizes

- At the largest accessibility text sizes on the iPhone, a timeline notice puts its time under its
  words, a tool call puts its command under the tool's name, and an approval card puts its risk tag
  under the tool's name. Each gets the whole width, so a name such as `build-server-linux-01` or
  `mcp__github__get_issue` is no longer broken mid-word. A single word longer than a whole line
  still wraps.

## A message from the phone that ends in `;` arrives whole

- A message from the phone that ends in `;` arrives with it. tmux read a trailing `;` as the end of a
  command, so `end;` arrived as `end`, `x\;` as `x;`, and a message that was only `;` as nothing.
  Everything else is typed exactly as before.

## The phone types into a renamed session, and its terminal shows the computer's screen

- A Claude session renamed with `/rename` takes the phone's messages again, instead of refusing them
  with "the message box is not on screen at the computer". The phone knows the session's box by the
  name Claude recorded for it, so a background agent being viewed at the computer, whose box looks
  the same, is still refused. If that name cannot be read, equals any agent task recorded on this
  computer, or is cut short in a narrow window, the phone is refused as before. A message into a
  renamed session can take a moment longer to arrive, while those records are read.
- While the computer also shows a session, the iPhone's Terminal tab draws the screen at the
  computer's size instead of garbling it: it opens at the prompt in readable text, slides sideways
  for long lines, and pinches out to show the whole screen. The computer's own window is unchanged.

## "Show all" keeps your place on iOS 27

- On iOS 27, "Show all" on a long output keeps your place, as it does on iOS 26: the lines you were
  reading stay where they were, the Latest button offers the way back, and events that arrive are
  counted ("1 new") instead of scrolling you on.
- The iPhone's message box is one pill resting just above the bottom edge, with the send and
  dictation button inside it, so the conversation gets more of the screen: about 70 points more when
  the box is empty. It still grows to five lines (three at the largest text sizes) and then scrolls.

## Codex hears about updates, and the terminal takes the screen

- `codeconnect daemon status | head -3` and the other commands that print and exit end quietly when
  the reader goes away, instead of panicking with "Broken pipe".
- On Linux without systemd, `codeconnect daemon uninstall` says there is no CodeConnect service to
  remove and changes nothing, instead of failing with a raw `systemctl` error; `codeconnect daemon
  status` shows the pid of a daemon you started by hand.
- `codeconnect codex` shows the notes `codeconnect claude` shows before a session — a newer release,
  a phone that cannot reach the computer — with the same 10-second pause to read them, and runs the
  same once-a-day update check.
- On the iPhone the terminal gets the screen: while it has the keyboard the header shrinks to one
  line, and turned to landscape the Terminal tab fills the screen. The key row is one row with Paste
  and Hide keyboard, and a tap on a web link opens a sheet with the whole address, Open in Safari
  and Copy link.

## CodeConnect runs on Linux

- The same command installs it on Linux (x86_64 or arm64): `curl -fsSL https://codeconnect.sh | sh`.
  It checks the release's signature with OpenSSL 3 before anything is unpacked or run, and
  without `openssl` it installs nothing and says which package to add.
- The daemon runs as a systemd user unit under the same `codeconnect daemon` commands: it starts at
  boot, keeps running when you log out, and restarts after a crash. Where the system asks for it,
  `codeconnect daemon install` prints the one `sudo loginctl enable-linger` line to run first.
- `codeconnect update` on Linux installs only releases signed with CodeConnect's Linux release key.
- CodeConnect finds Tailscale where Linux packages install it, and the phone shows the computer by its
  hostname. A computer whose Tailscale is switched off waits on its own and joins the tailnet when
  it comes back.
- Containers and cloud sandboxes where Tailscale runs in userspace mode work too: the daemon listens
  on loopback, where Tailscale delivers tailnet connections, and the pairing code carries the
  computer's tailnet address.
- Without systemd (Alpine, most containers) you start the daemon by hand, with the one line the
  installer and `codeconnect daemon install` print. `--source` installs are macOS-only.
- The terminal's notes and pairing messages, and the daemon's warnings about its address, files
  and service, say "this computer" where they said "this Mac".
- The phone and the computer say "the computer" where they said "the Mac" — on a Mac, on Linux
  and on a cloud machine alike: a card's notes, why an answer, a message or Stop did not go
  through, and the app's own words. An iPhone app older than this release still shows the
  computer's messages, but after a Codex link refusal that names the computer it no longer greys
  Stop for its few seconds; the matching app does.

## One phone, several computers

- The iPhone keeps every computer it pairs with and stays connected to all of them: scanning
  another computer's code adds it, and the fleet shows every computer's sessions together.
- Settings lists your computers, each with its name, its system and how its link is. Each one's own
  screen shows how the phone reaches it and what its daemon offers, and removes that computer alone.
  Offline, a computer is still called by the name it last reported.
- One computer paired under two addresses — an IP, then its MagicDNS name — is one computer: the
  daemon tells the phone which daemon it is (protocol minor 26).

## The phone names your Mac the way you do

- While connected, Settings on the iPhone shows your Mac by its computer name, with "macOS" under
  it, instead of its network address. Renaming the Mac shows on the phone the next time it connects.
  Needs both this Mac release and the matching iPhone app; with an older Mac the phone still shows the
  address.

## Codex says goodbye as it does run directly again

- Quitting `codeconnect codex` ends with what `codex` prints run directly — its token usage and
  `To continue this session, run: codex resume <id>` — again. With Codex 0.164 it showed Codex's own
  "Disconnected from this task" text instead, with a reconnect command for a socket that closes with the
  session: Codex now prints that command on a line of its own, and CodeConnect only looked for the older
  one-line form.

## Codex and Claude Code run however you installed them

- `codeconnect codex` runs Codex installed with npm, bun, pnpm or mise. It used to refuse anything but
  OpenAI's installer or Homebrew's cask, saying the `codex` was "a wrapper, not a native executable",
  while typing `codex` worked.
- `codeconnect claude` and `codeconnect codex` run the `claude` and `codex` your shell runs: the first on
  your `PATH`. With two installs, they used to prefer a few fixed folders, such as `~/.local/bin`, over
  your `PATH`, and could start a different version from the one you use.

## A Codex turn reads in order after a restart

- When the daemon restarts or updates while a Codex turn runs, what it recovers from Codex's history
  afterwards — the prompt, a command that ran while it was down — appears on the phone inside its turn,
  where it happened, instead of below the turn's "Turn complete". The session reads Done when the turn has
  finished, instead of Running with the old prompt as its latest line. Needs both this Mac release and the
  matching iPhone app; either one alone keeps the old order.
- When a Codex turn stops or fails around a daemon restart, update or reconnect, its prompt and what it did
  before the daemon attached reach the phone once the next turn completes. They used to stay missing:
  Codex names a stopped turn's prompt only by a placeholder until a later turn has run.

## Codex sessions stay on the phone after a restart

- A Codex session whose history holds a stopped turn, a failed turn, a web search, an MCP or sub-agent
  call, a context compaction or an image keeps its phone link when the daemon reads that history: after
  a restart or an update, on a new session it attaches to late, and on a thread you resume or fork. The
  daemon used to refuse what Codex told it about such a history and reconnected every 15 seconds for the
  rest of the session, with no approvals, Stop or messages from the phone. What the daemon reads back from
  Codex's history has gaps, listed in the Codex guide under what your phone cannot do.
- A Codex command approval reaches the phone when the daemon attaches to a turn that has already run a
  command: after the daemon restarts (an update, a crash) while the approval waits, or on a new session
  that sat idle before its first request. The daemon used to refuse what Codex told it about such a turn
  and reconnected every 15 seconds until the turn ended at the Mac, so the approval never left the Mac.
- Stop works on a turn that was running a command when the Mac's link to Codex reconnected. The Codex
  guide's limit on it is gone.

## Set up in one command

- The installer asks to start the daemon and to pair your iPhone, after the PATH question (Return means
  yes), and ends with the one step left: `codeconnect claude` in a new terminal in your project. Run again,
  it downloads nothing and asks only for what is still missing, so stopping at any question and running
  the same command picks up there.
- On the phone, the setup steps follow the installer: install and answer its questions, scan the code it
  shows, start your agent.

## The Mac says when the phone cannot see a session

- `codeconnect claude` on a Mac with no daemon installed (never installed, uninstalled, or unloaded) says
  the phone cannot see the session and names `codeconnect daemon install`, instead of opening with no
  word. An installed daemon that is starting or restarting in your login session gets no note.
- A `ws_bind` on your local network without `ws_allow_plaintext` is told the setting first — the step the
  support page gives — and the certificate as the encrypted alternative.
- `codeconnect pair` on a daemon no phone can reach says Tailscale is not set up or not connected, instead of
  blaming a late start at login (a daemon run by launchd moves to a tailnet address by itself once one
  appears); a `ws_bind` that names the unreachable address, or one not applied yet, is told so instead.
  The "Phone unreachable" note before a session opens gives the same answer.
- Plainer words in small places: `codeconnect revoke` with no device shows `codeconnect revoke`, not `cc`;
  `codeconnect token` before the daemon has ever run says to start it; a Mac with no Claude Code is given
  the setup link; `daemon install` is described as starting the daemon now and at login; the installer
  says once that CodeConnect is installed and that its Developer ID signature was verified.
- On the phone, the pairing steps link codeconnect.sh/support.
- `codeconnect codex` whose keyboard or screen is not a terminal (`< /dev/null`, `> file`, `ssh` without
  `-t`) refuses in native `codex`'s words — "stdin is not a terminal" or "stdout is not a terminal" — before
  starting anything. It used to start a session it could not show, then fail or hang writing the screen
  into the file.

## A cleaner first run

- A Mac with no Codex is told both ways to install the native Codex that CodeConnect runs:
  OpenAI's installer, from its GitHub releases, or Homebrew; one with only npm's `codex` is told the
  installer. The command it used to print returns 404.
- The website, README, support page and the app say what a local network gives up:
  Tailscale's encryption and the live terminal. The "Phone unreachable" note no longer waits for a
  tailnet to come "back" on a Mac that never had one.
- What you type during the 10-second note before a session opens reaches the agent: a line typed
  and sent with Return used to be swallowed by the note, which only meant to skip it. A Return on its
  own still skips the note.
- `codeconnect devices` shows a phone as seen from the moment it pairs; one still on the connection it
  paired over used to read "never".
- The installer gives the PATH line once, does not call an existing install an error (it exits 0), and
  after a reinstall no longer tells you to install a daemon you have; `codeconnect update` and the
  installer say once that the daemon restarted. Output that is not a terminal carries no colour codes.
- `codeconnect pair` shows the two things the app's manual form asks for, address and code, under the
  form's own label; the raw JSON line, the scheme and the timestamp a second expiry repeated are gone.
- When the phone's message cannot be typed at the Mac, the reason is said plainly — "the message box is
  not on screen at the Mac", "a menu or view at the Mac has the keyboard" — instead of naming the
  composer's prompt row and its box rule.
- On the phone, VoiceOver no longer reads an empty clause on a fleet row with nothing to show yet
  ("Idle, , …").
- On the phone, an empty fleet says "No agents running" once; the header no longer adds "No agents
  registered" above it.
- `codeconnect --help` says what each command does in plain words and lists `--version`; it no longer
  names tmux servers, the event log, plists or LaunchAgents.
- On a private network that uses Tailscale's address range (NetBird and others), the daemon no longer
  says `tailscale ip` timed out when Tailscale is simply not found or not up, and no longer offers
  `tailscale cert` for an address that is not Tailscale's: it says where a certificate of your own
  goes and how long it must still be valid.

## CodeConnect brings its own tmux

- A new Mac needs neither Homebrew nor tmux: CodeConnect installs its own tmux, built from
  pinned sources and signed like the rest of it.
- The installer asks once, on the terminal, before adding `~/.codeconnect/bin` to your
  `PATH`. It adds one marked block to your shell's startup file — delete the block to undo
  it — and remembers a no. `--path-yes` and `--path-no` answer without asking.
- `codeconnect update` installs CodeConnect's tmux with each release and repairs it if it is
  missing. A Mac updated by an older version gets it the first time a session starts.
- Sessions already running keep the tmux they started with until they end.
- What a first run can trip over now says what to do: with no daemon running, commands name
  `codeconnect daemon install`; a Mac without Claude Code, or with only npm's `codex` wrapper,
  is told how to install the one CodeConnect runs.
- `codeconnect claude` no longer loses what an agent printed when it ends before the
  terminal shows it — `--version`, `--help`, a mistyped flag, a startup error, or one that
  finishes during the "phone cannot reach this Mac" note — whether or not other sessions are
  running. Its output is shown, and the command exits 0 instead of "no session named cc-1".
- `codeconnect claude` with no terminal to show a session in — from a script, through a pipe, or
  over `ssh` without `-t` — runs the Claude Code it would have hosted directly, with Claude Code's
  own output and exit status, instead of starting a session it cannot show and leaving the agent
  running behind an error.
- The commands in `codeconnect --help` no longer act on an argument they do not take. `codeconnect daemon uninstall --help`
  uninstalled the daemon, `update --help` updated, `revoke <device> --dry-run` revoked and
  `token --help` printed the token; now each names what it takes and does nothing.
- On the phone, a session that ends normally reads "Session ended": no "Link detached" rows around
  the end, and no banner saying it can only be answered at the Mac.
- A session that ends just as its supervisor reconnects to the daemon is reported as ended, instead
  of staying live on the phone until the daemon next restarts.
- On the phone, a Claude turn stopped at the Mac — Esc, or No at a tool's prompt — ends there: it
  reads as the Mac's "Interrupted · What should Claude do instead?" row and the session as done,
  instead of Running until the next prompt.
- On the phone, a Claude session working on a prompt that draws no row of its own — a scheduled
  prompt, another session's message — reads Running from the moment it is handed over, not Done
  until Claude's first reply.
- On the phone, a Claude session where `/model` or `/login` was typed after a turn ended reads
  Done, as the Mac does, instead of Running until the next prompt; `/plan` and `/goal` with a
  request, which Claude answers, read Running.
- On the phone, a tool with a long name — an MCP server's `mcp__server__tool` — no longer blanks
  the command beside every other tool on the fleet and in the chat: its own name is shortened in
  the middle, and every row keeps room for its command.
- On the phone, a Claude turn that ends on an API error — a rate limit, an overloaded or failing
  server, an expired login — ends there, where the Mac draws its "done" line under the error, and
  the session reads as done instead of Running until the next prompt.
- On the phone, a prompt typed with pasted images that take it over 512 KiB shows under YOU, and
  the session reads Running while Claude works on it, instead of no row and Done. The phone is
  sent the prompt without its images; that holds while what is left fits in 512 KiB and the
  whole prompt, images included, is under 4 MiB.
- On the phone, VoiceOver reads an approval in the chat as the tool and the first line of its
  command — "Bash, git push --force origin main" — waiting or answered, instead of the tool name
  alone. A command whose lines end Windows-style (`\r\n`), or in another line break, is cut at its
  first line in the chat's approval and tool rows, the Deck bar and the fleet's line for a tool
  call, as one with ordinary line ends always was.
- On the phone, an agent's words on a decision draw a right-to-left override, the other bidi embeddings
  and isolates, the Arabic letter mark and NUL as �, as Claude Code's own screen does: the command,
  description and full input of an approval and of its tool row, a Codex card's reason, files, diff,
  working directory and "don't ask again" options, every string of a question, notices, the Mac's
  screen, and the project and folder a run is in, on the cards, the fleet and the Deck. An override
  made `git push origin <U+202E>niam ecrof--` read
  `git push origin --force main` under "Verified" on the fleet, the Deck, the chat and the decision
  card. What is sent, verified and answered is unchanged; the card says it is verified against the
  daemon's hash, and its command is headed Command.
- On the phone, the fleet's line for a run waiting on an approval shows and speaks the first line of its
  command, as the chat and the Deck bar do; VoiceOver used to read every line of it.
- On the phone, a skill or custom slash command shows under YOU as the command — `/simplify` and its
  arguments — instead of the markup Claude Code records it in, and the session reads Running while
  Claude works on it.
- On the phone, VoiceOver says how long something has waited as "waiting 1 minute", on the fleet, in the
  chat, on the Deck bar and on the wait clock, instead of "waiting 1 minute ago" or "waiting 1m"; an
  answer the Mac has not confirmed is "still waiting on the Mac, 4 seconds".
- On the phone, output whose lines end Windows-style (`\r\n`) no longer draws a blank line under every line.
- Two `codeconnect claude` started in the same instant — tabs a terminal restores together — both
  start, as `cc-1` and `cc-2`; one used to fail with "duplicate session".
- `codeconnect claude` and `codeconnect attach` never show, or type into, a session another
  terminal started in its place: when the session they meant has ended, a new one that tmux
  gave the same ids is left alone.
- A session that ends at once — the only one running — no longer leaves its background
  supervisor behind, logging every two seconds; and one still finishing after an uninstall
  no longer recreates `~/.codeconnect`.
- `codeconnect pair` makes no code a phone could never use: on an address where the daemon
  refuses every connection (a local network with neither a certificate nor
  `ws_allow_plaintext`) it says so and how to fix it, and on a Mac with neither Tailscale nor
  `ws_bind` it names both.
- `codeconnect daemon install`, `daemon restart` and `pair` say when `~/.codeconnect/config.json`
  cannot be read — a stray comma, say — and that the daemon is using its defaults, rather than
  leaving that to the daemon's log. A misspelled setting, or a `ws_bind` written with a port, is
  named the same way instead of being silently ignored. So is an `apns` block, which no setting
  ever read: a file holding one with `null` or an unexpected value in it used to fall back to
  every default, and now the rest of its settings apply.
- The phone rings only when you are away from the Mac. A notification — Claude or Codex, a
  finished turn, a wait, an approval or a question — waits while you are using the Mac, and
  rings once the Mac has had no keyboard or mouse input for two minutes, if it still matters
  then: nothing rings for a decision you answered, a run that moved on — a new prompt or a newer
  finished or waiting state included — or ended, or one you removed. A decision answered at the
  Mac does not ring when its screen, read as it would ring, shows the dialog gone and the input
  box back. Every approval rings on its own: the second and later approvals
  of one turn used to ring nothing. A background agent's question is still kept off the Mac for the
  phone after ten seconds without input; its notification, like every other, waits for two
  minutes.
- Codex approvals and finished Codex turns ring your phone. They used to ring no phone at all:
  the Mac sent a Codex notification only to a phone that had said it could show Codex, and no
  iPhone build ever said so. Every iPhone build since 1.1.0 shows Codex, so the Mac now rings
  every paired phone for Codex exactly as for Claude. A notification counts every decision waiting,
  Claude's and Codex's together, and the Mac sends a phone its notifications in the order they were
  counted, so a finished turn no longer replaces one that says a decision waits while that decision
  still waits. The one exception: a phone that pairs again while a notification is still on its way
  to it can receive that older one last.
- A project folder whose name holds a character that changes text direction (U+202E and its
  kin) appears in a notification's title with that character shown as a replacement mark, so the
  name cannot be drawn reordered.
- A tool call too large to send to the phone in one piece now shows there as too large; the
  stand-in the Mac sent named its marker in a spelling the phone does not read, so it drew as an
  empty tool row. A reply or a tool result that large still shows nothing on the phone.

## A plainer message box, and Claude's notes to itself stay its own

- The message box says "Ask anything", in a quieter grey, and the suggestion chips under it
  (Continue, Fix it, Run the tests, …) are gone.
- When a background agent or command finishes, the chat shows the line the Mac shows —
  `Agent "Audit the parser" finished · 4m 3s`, its dot coloured by how the task ended —
  instead of Claude Code's raw report under YOU.
- Lines Claude Code writes for itself, not for you, no longer appear as yours: another
  session's message, a scheduled prompt, Stop hook feedback, and the summary that continues
  a compacted conversation.
- iPhone only (1.1.6); the Mac needs no update.

## Chat history in pages, and a fleet that keeps up

- A chat opens on its newest 500 events, at launch and on every return to the app, however
  long the app was away. Reaching the top loads the 500 before them in place, without moving
  what you are reading. Open approvals and questions, the model and the mode stay right even
  when they are older than what the chat holds.
- At launch the fleet waits behind "Connecting to your Mac…" until it has caught up, and
  shows the saved list if the Mac cannot be reached. Coming back to the app updates the
  screen once, instead of stepping through everything that happened while it was away.
- Tapping a row opens its chat. Removing a session asks first. Ended sessions can be selected
  and removed together with one confirmation, and the Ended section folds away with Hide.
- The fleet scrolls at the largest text sizes, a row swiped open starts fresh after the link
  drops and returns, and a link that opens another chat opens that chat.
- The fleet does far less work: an idle fleet no longer redraws every row each second, a row
  redraws only when what it shows changes, and the fleet does nothing while a chat or the
  Deck covers it.
- Pages need a Mac with this update (CodeConnect 0.13.0, protocol minor 23); with an older
  Mac the phone (1.1.6) keeps "Load all".

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
